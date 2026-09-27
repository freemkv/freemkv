"""The fixture fetch: parallel, pinned by VersionId or If-Match, and ETag-verified from the bytes."""

import hashlib
import io
import math
from pathlib import Path
import shutil
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import media_fetch as mf  # noqa: E402

MIB = mf.MIB


def s3_etag(data, part):
    if part is None:
        return '"' + hashlib.md5(data).hexdigest() + '"'
    parts = [data[i:i + part] for i in range(0, max(len(data), 1), part)]
    return '"' + hashlib.md5(b''.join(hashlib.md5(p).digest() for p in parts)).hexdigest() + f'-{len(parts)}"'


class FakeS3:
    """get_object over in-memory objects, honouring Range, VersionId and If-Match."""

    def __init__(self, objects, versioned=False):
        self.objects = objects
        self.versioned = versioned
        self.calls = []
        self.lock = threading.Lock()
        self.in_flight = set()
        self.max_keys_in_flight = 0
        self.replace = {}

    def get_object(self, Bucket, Key, Range=None, VersionId=None, IfMatch=None):
        with self.lock:
            self.calls.append({'Key': Key, 'Range': Range, 'VersionId': VersionId, 'IfMatch': IfMatch})
            self.in_flight.add(Key)
            self.max_keys_in_flight = max(self.max_keys_in_flight, len(self.in_flight))
        try:
            time.sleep(0.01)
            data, etag = self.objects[Key]
            if Key in self.replace and len([c for c in self.calls if c['Key'] == Key]) > 2:
                data, etag = self.replace[Key]
            if IfMatch is not None and IfMatch != etag:
                raise RuntimeError('An error occurred (PreconditionFailed) when calling the GetObject operation')
            if Range:
                lo, hi = map(int, Range[len('bytes='):].split('-'))
                data = data[lo:hi + 1]
            return {'Body': io.BytesIO(data)}
        finally:
            with self.lock:
                self.in_flight.discard(Key)

    def head_object(self, Bucket, Key):
        data, etag = self.objects[Key]
        out = {'ETag': etag, 'ContentLength': len(data)}
        if self.versioned:
            out['VersionId'] = 'v-' + Key
        return out

    def get_bucket_versioning(self, Bucket):
        return {'Status': 'Enabled'} if self.versioned else {}


def objects():
    out = {}
    for name, size, part in (('dvd.iso', 3 * MIB + 5, MIB), ('bd.iso', 5 * MIB, 2 * MIB),
                             ('keydb.cfg', 1000, 8 * MIB), ('small.bin', 10, None)):
        data = bytes((i * 7 + len(name)) % 251 for i in range(size))
        out[name] = (data, s3_etag(data, part))
    return out


class FetchTests(unittest.TestCase):
    def setUp(self):
        self.dest = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.dest)
        self.objects = objects()

    def run_fetch(self, s3, workers=8):
        manifest = mf.resolve(s3, 'b', list(self.objects))
        return manifest, mf.fetch(s3, 'b', manifest, self.dest, workers=workers, log=lambda *_: None)

    def test_fetches_everything_concurrently_and_verifies_content(self):
        s3 = FakeS3(self.objects)
        _, report = self.run_fetch(s3)
        for key, (data, _) in self.objects.items():
            self.assertEqual((self.dest / key).read_bytes(), data, key)
            self.assertTrue(report['keys'][key]['content_verified'], key)
        self.assertGreater(s3.max_keys_in_flight, 1, 'keys were fetched one after another')

    def test_parts_are_interleaved_across_keys(self):
        s3 = FakeS3(self.objects)
        self.run_fetch(s3, workers=1)
        self.assertEqual({c['Key'] for c in s3.calls[:len(self.objects)]}, set(self.objects))

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

    def test_object_replaced_mid_fetch_fails(self):
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', list(self.objects))
        data = b'x' * (3 * MIB + 5)
        s3.replace['dvd.iso'] = (data, s3_etag(data, MIB))
        with self.assertRaises(RuntimeError):
            mf.fetch(s3, 'b', manifest, self.dest, workers=1, log=lambda *_: None)
        self.assertEqual(len([c for c in s3.calls if c['Key'] == 'dvd.iso']), 3, 'a 412 must not be retried')

    def test_wrong_bytes_fail_the_content_check(self):
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', list(self.objects))
        data, etag = self.objects['bd.iso']
        s3.objects['bd.iso'] = (data[:-1] + b'\0', etag)
        with self.assertRaises(IOError):
            mf.fetch(s3, 'b', manifest, self.dest, log=lambda *_: None)

    def test_short_part_fails(self):
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', list(self.objects))
        manifest['dvd.iso']['size'] += 1
        with self.assertRaises(IOError):
            mf.fetch(s3, 'b', manifest, self.dest, log=lambda *_: None)

    def test_part_layout(self):
        self.assertEqual(mf.part_layout('"x-4429"', 37151703040), (8 * MIB, 4429))
        self.assertEqual(mf.part_layout('"x-6782"', 56890556416), (8 * MIB, 6782))
        self.assertEqual(mf.part_layout('"x-1"', 24489617), (24 * MIB, 1))
        self.assertEqual(mf.part_layout('"abc"', 10), (10, 1))
        self.assertIsNone(mf.part_layout('"x-4"', 5 * MIB))

    def test_underivable_layout_still_pins_but_warns(self):
        s3 = FakeS3(self.objects)
        manifest = mf.resolve(s3, 'b', ['bd.iso'])
        manifest['bd.iso']['etag'] = self.objects['bd.iso'][1].replace('-3"', '-4"')
        s3.objects['bd.iso'] = (self.objects['bd.iso'][0], manifest['bd.iso']['etag'])
        logs = []
        report = mf.fetch(s3, 'b', manifest, self.dest, log=logs.append)
        self.assertFalse(report['keys']['bd.iso']['content_verified'])
        self.assertTrue(any('If-Match only' in line for line in logs))


class WorkflowFetchTests(unittest.TestCase):
    def test_qa_fetches_through_media_fetch_only(self):
        qa = (Path(__file__).parents[1] / '.github/workflows/qa.yml').read_text()
        self.assertNotIn('s3api get-object', qa)
        self.assertNotRegex(qa, r'aws s3 cp [^\n]*\.iso')
        self.assertNotRegex(qa, r'aws s3 cp [^\n]*keydb')
        self.assertIn('media_fetch.py fetch', qa)
        self.assertIn('--require-hashes', qa)


class RequirementsTests(unittest.TestCase):
    def test_every_requirement_is_exact_and_hash_pinned(self):
        text = (Path(__file__).parent / 'media-fetch-requirements.txt').read_text()
        reqs = [r for r in text.replace('\\\n', ' ').splitlines() if r.strip() and not r.startswith('#')]
        self.assertIn('boto3', ' '.join(reqs))
        for req in reqs:
            self.assertRegex(req, r'^[A-Za-z0-9_.-]+==\S+ +(--hash=sha256:[0-9a-f]{64} *)+$')


if __name__ == '__main__':
    unittest.main()
