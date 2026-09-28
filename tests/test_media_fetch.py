"""The fixture fetch: parallel, pinned by VersionId or If-Match, and ETag-verified from the bytes."""

import builtins
import hashlib
import io
from pathlib import Path
import re
import shutil
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).parent))
import media_fetch as mf  # noqa: E402

MIB = mf.MIB
ROOT = Path(__file__).parents[1]


class ClientError(Exception):
    """Shaped like botocore.exceptions.ClientError."""

    def __init__(self, code, status):
        super().__init__(f'An error occurred ({code}) when calling the GetObject operation')
        self.response = {'Error': {'Code': code}, 'ResponseMetadata': {'HTTPStatusCode': status}}


def s3_object(data, part):
    """(data, etag, part size) as S3 stores a single-part (part=None) or multipart upload."""
    if part is None:
        return data, '"' + hashlib.md5(data).hexdigest() + '"', None
    parts = [data[i:i + part] for i in range(0, max(len(data), 1), part)]
    etag = '"' + hashlib.md5(b''.join(hashlib.md5(p).digest() for p in parts)).hexdigest() + f'-{len(parts)}"'
    return data, etag, part


class FakeS3:
    """HeadObject and ranged GetObject over in-memory objects; only the calls the role allows."""

    def __init__(self, objects, versioned=False):
        self.objects = objects
        self.versioned = versioned
        self.calls = []
        self.lock = threading.Lock()
        self.in_flight = set()
        self.max_keys_in_flight = 0
        self.replace = {}
        self.transient = {}

    def head_object(self, Bucket, Key, PartNumber=None, IfMatch=None):
        data, etag, part = self.objects[Key]
        if IfMatch is not None and IfMatch != etag:
            raise ClientError('PreconditionFailed', 412)
        out = {'ETag': etag, 'ContentLength': len(data)}
        if PartNumber is not None and part:
            out.update(ContentLength=min(part, len(data)), PartsCount=int(etag.strip('"').split('-')[1]))
        if self.versioned:
            out['VersionId'] = 'v-' + Key
        return out

    def get_object(self, Bucket, Key, Range=None, VersionId=None, IfMatch=None):
        with self.lock:
            self.calls.append({'Key': Key, 'Range': Range, 'VersionId': VersionId, 'IfMatch': IfMatch})
            self.in_flight.add(Key)
            self.max_keys_in_flight = max(self.max_keys_in_flight, len(self.in_flight))
            fail = self.transient.get((Key, Range), 0)
            if fail:
                self.transient[(Key, Range)] = fail - 1
        try:
            time.sleep(0.01)
            if fail:
                raise ClientError('SlowDown', 503)
            data, etag, _ = self.objects[Key]
            if Key in self.replace and len([c for c in self.calls if c['Key'] == Key]) > 2:
                data, etag, _ = self.replace[Key]
            if IfMatch is not None and IfMatch != etag:
                raise ClientError('PreconditionFailed', 412)
            if Range:
                lo, hi = map(int, Range[len('bytes='):].split('-'))
                data = data[lo:hi + 1]
            return {'Body': io.BytesIO(data)}
        finally:
            with self.lock:
                self.in_flight.discard(Key)


def objects():
    out = {}
    for name, size, part in (('dvd.iso', 3 * MIB + 5, MIB), ('bd.iso', 5 * MIB, 2 * MIB),
                             ('keydb.cfg', 1000, 8 * MIB), ('small.bin', 10, None),
                             ('single.bin', 2 * mf.CHUNK + 7, None)):
        data = (bytes(range(251)) * ((size + len(name)) // 251 + 2))[len(name):len(name) + size]
        out[name] = s3_object(data, part)
    return out


class FetchTests(unittest.TestCase):
    def setUp(self):
        self.dest = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dest)
        self.enterContext(mock.patch.object(mf, 'CHUNK', MIB))
        self.objects = objects()

    def run_fetch(self, s3, workers=8):
        manifest = mf.resolve(s3, 'b', list(self.objects))
        return manifest, mf.fetch(s3, 'b', manifest, self.dest, workers=workers, log=lambda *_: None)

    def test_fetches_everything_concurrently_and_verifies_content(self):
        s3 = FakeS3(self.objects)
        _, report = self.run_fetch(s3)
        for key, (data, _, _) in self.objects.items():
            self.assertEqual((self.dest / key).read_bytes(), data, key)
            self.assertTrue(report['keys'][key]['content_verified'], key)
        self.assertGreater(s3.max_keys_in_flight, 1, 'keys were fetched one after another')

    def test_parts_are_interleaved_across_keys(self):
        s3 = FakeS3(self.objects)
        self.run_fetch(s3, workers=1)
        self.assertEqual({c['Key'] for c in s3.calls[:len(self.objects)]}, set(self.objects))

    def test_resolve_records_the_exact_upload_part_size(self):
        manifest = mf.resolve(FakeS3(self.objects), 'b', list(self.objects))
        self.assertEqual((manifest['bd.iso']['part_size'], manifest['bd.iso']['parts']), (2 * MIB, 3))
        self.assertEqual((manifest['keydb.cfg']['part_size'], manifest['keydb.cfg']['parts']), (1000, 1))
        self.assertNotIn('part_size', manifest['small.bin'])
        self.assertIsNone(manifest['dvd.iso']['version_id'])

    def test_unversioned_bucket_pins_every_get_by_if_match(self):
        s3 = FakeS3(self.objects)
        manifest, _ = self.run_fetch(s3)
        for call in s3.calls:
            self.assertEqual(call['IfMatch'], manifest[call['Key']]['etag'])
            self.assertIsNone(call['VersionId'])

    def test_versioned_bucket_pins_every_get_by_version(self):
        s3 = FakeS3(self.objects, versioned=True)
        self.run_fetch(s3)
        for call in s3.calls:
            self.assertEqual(call['VersionId'], 'v-' + call['Key'])
            self.assertIsNone(call['IfMatch'])

    def test_parts_follow_the_upload_boundaries(self):
        s3 = FakeS3(self.objects)
        self.run_fetch(s3)
        ranges = sorted(c['Range'] for c in s3.calls if c['Key'] == 'bd.iso')
        self.assertEqual(ranges, ['bytes=0-2097151', 'bytes=2097152-4194303', 'bytes=4194304-5242879'])

    def test_small_multipart_object_uses_its_real_part_size(self):
        # 18 MiB in 3 parts of 8 MiB: a guess from size/parts would say 6 MiB.
        data = (bytes(range(251)) * (18 * MIB // 251 + 1))[:18 * MIB]
        s3 = FakeS3({'keydb.cfg': s3_object(data, 8 * MIB)})
        manifest = mf.resolve(s3, 'b', ['keydb.cfg'])
        report = mf.fetch(s3, 'b', manifest, self.dest, log=lambda *_: None)
        self.assertTrue(report['keys']['keydb.cfg']['content_verified'])

    def test_single_part_objects_are_ranged_and_md5_checked(self):
        s3 = FakeS3(self.objects)
        self.run_fetch(s3)
        ranges = [c['Range'] for c in s3.calls if c['Key'] == 'single.bin']
        self.assertEqual(len(ranges), 3, 'a single-part object must not be one whole-object GET')
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', ['single.bin'])
        data = self.objects['single.bin'][0]
        s3.objects['single.bin'] = (data[:-1] + b'\0', manifest['single.bin']['etag'], None)
        with self.assertRaises(IOError):
            mf.fetch(s3, 'b', manifest, self.dest, log=lambda *_: None)

    def test_object_replaced_mid_fetch_fails_without_retrying(self):
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', list(self.objects))
        s3.replace['dvd.iso'] = s3_object(b'x' * (3 * MIB + 5), MIB)
        with self.assertRaises(mf.Precondition):
            mf.fetch(s3, 'b', manifest, self.dest, workers=1, log=lambda *_: None)
        self.assertEqual(len([c for c in s3.calls if c['Key'] == 'dvd.iso']), 3, 'a 412 must not be retried')

    def test_transient_errors_are_retried(self):
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', list(self.objects))
        s3.transient[('bd.iso', 'bytes=2097152-4194303')] = 2
        with mock.patch.object(mf.time, 'sleep'):
            mf.fetch(s3, 'b', manifest, self.dest, log=lambda *_: None)
        self.assertEqual((self.dest / 'bd.iso').read_bytes(), self.objects['bd.iso'][0])

    def test_access_denied_names_the_missing_permission(self):
        s3 = FakeS3(self.objects, versioned=True)
        manifest = mf.resolve(s3, 'b', ['bd.iso'])
        s3.get_object = mock.Mock(side_effect=ClientError('AccessDenied', 403))
        with self.assertRaisesRegex(IOError, 'GetObjectVersion'):
            mf.fetch(s3, 'b', manifest, self.dest, log=lambda *_: None)

    def test_wrong_bytes_fail_the_content_check(self):
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', list(self.objects))
        data, etag, part = self.objects['bd.iso']
        s3.objects['bd.iso'] = (data[:-1] + b'\0', etag, part)
        with self.assertRaises(IOError):
            mf.fetch(s3, 'b', manifest, self.dest, log=lambda *_: None)

    def test_short_part_fails(self):
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', list(self.objects))
        manifest['dvd.iso']['size'] += 1
        with self.assertRaises((IOError, ValueError)):
            mf.fetch(s3, 'b', manifest, self.dest, log=lambda *_: None)

    def test_inconsistent_pins_are_rejected(self):
        base = {'etag': '"x-4429"', 'size': 37151703040, 'part_size': 8 * MIB, 'parts': 4429}
        self.assertEqual(mf.layout(base), (8 * MIB, 4429, True))
        for bad in ({'part_size': None}, {'parts': 4428}, {'part_size': 16 * MIB}, {'etag': '"x-abc"'}):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    mf.layout(dict(base, **bad))

    def test_every_file_handle_is_closed(self):
        opened = []
        real_open = builtins.open

        def tracking(*a, **kw):
            f = real_open(*a, **kw)
            opened.append(f)
            return f
        with mock.patch.object(builtins, 'open', tracking):
            self.run_fetch(FakeS3(self.objects))
        self.assertTrue(opened)
        self.assertTrue(all(f.closed for f in opened))


class WorkflowFetchTests(unittest.TestCase):
    def test_qa_fetches_through_media_fetch_only(self):
        qa = (ROOT / '.github/workflows/qa.yml').read_text()
        self.assertNotIn('s3api get-object', qa)
        self.assertNotRegex(qa, r'aws s3 (cp|sync)')
        self.assertIn('media_fetch.py fetch', qa)
        self.assertRegex(qa, r'--require-hashes[^\n]*\\\n\s*-r freemkv/tests/media-fetch-requirements.txt')

    def test_fetch_needs_only_get_object(self):
        src = (ROOT / 'tests/media_fetch.py').read_text()
        calls = set(re.findall(r's3\.([a-z_]+)\(', src))
        self.assertEqual(calls, {'head_object', 'get_object'})


class RequirementsTests(unittest.TestCase):
    def test_every_requirement_is_exact_and_hash_pinned(self):
        text = (ROOT / 'tests/media-fetch-requirements.txt').read_text()
        reqs = [r for r in text.replace('\\\n', ' ').splitlines() if r.strip() and not r.startswith('#')]
        self.assertIn('boto3', ' '.join(reqs))
        for req in reqs:
            self.assertRegex(req, r'^[A-Za-z0-9_.-]+==\S+ +(--hash=sha256:[0-9a-f]{64} *)+$')


if __name__ == '__main__':
    unittest.main()
