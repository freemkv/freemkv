"""The key-service canary (tests/media_canary.py): decision 15, with no operator-supplied probe."""

import base64
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import struct
import sys
import tempfile
import unittest
import unittest.mock
import urllib.error

sys.path.insert(0, str(Path(__file__).parent))
import media_canary as mc  # noqa: E402

UK = bytes.fromhex('00112233445566778899aabbccddeeff')
VUK = bytes.fromhex('0f1e2d3c4b5a69788796a5b4c3d2e1f0')
S = 2048
P = 300                      # physical partition start sector
CLIP_UNITS = 40


# ── A synthetic UDF 2.50 disc image (metadata partition, FE and EFE, short and long ADs) ──

def tag(tid, loc, block):
    struct.pack_into('<HH', block, 0, tid, 3)
    struct.pack_into('<I', block, 12, loc)
    block[4] = (sum(block[0:4]) + sum(block[5:16])) & 0xFF


def file_entry(loc, size, ads, ad_type, directory=False, extended=False):
    b = bytearray(S)
    b[27] = 4 if directory else 5
    struct.pack_into('<H', b, 34, ad_type)
    struct.pack_into('<Q', b, 56, size)
    base, lens = (216, 208) if extended else (176, 168)
    struct.pack_into('<II', b, lens, 0, len(ads))
    b[base:base + len(ads)] = ads
    tag(266 if extended else 261, loc, b)
    return b


def short_ad(lbn, length):
    return struct.pack('<II', length, lbn)


def long_ad(lbn, length, ref):
    return struct.pack('<IIH6x', length, lbn, ref)


def fid(name, lbn, ref=1, directory=False, parent=False):
    raw = b'' if parent else b'\x08' + name.encode()
    b = bytearray(38 + len(raw))
    b[18] = (0x02 if directory else 0) | (0x08 if parent else 0)
    b[19] = len(raw)
    struct.pack_into('<IIH', b, 20, S, lbn, ref)
    b[38:] = raw
    tag(257, 0, b)
    return bytes(b + bytes((-len(b)) % 4))


def clip_units(key, clear=(5,)):
    """CLIP_UNITS aligned units of clean MPEG-TS, encrypted under `key` except the `clear` ones."""
    units = []
    for u in range(CLIP_UNITS):
        unit = bytearray(mc.UNIT)
        for p in range(32):
            o = p * mc.PACKET
            if p == 7:
                continue            # an all-zero padding packet stays zero through the crypto
            unit[o + 4] = 0x47
            for i in range(o + 5, o + mc.PACKET):
                unit[i] = (u * 31 + p * 7 + i) % 251 + 1
        if u in clear:
            units.append(bytes(unit))
        else:
            unit[0] |= 0xC0
            units.append(mc.encrypt_unit(bytes(unit), key))
    return units


class Disc:
    """Builds the image and remembers what it put where."""

    def __init__(self, key=UK, inf=None):
        self.inf = inf or self.unit_key_ro([mc.encrypt_block(mc.expand_key(VUK), UK)])
        self.mkb = bytes(range(256)) * 20            # 5 KiB, three sectors
        self.units = clip_units(key)
        self.clip = b''.join(self.units)
        self.small = bytes(3 * mc.UNIT)
        sectors = {}
        pd = bytearray(S)
        struct.pack_into('<H', pd, 22, 0)
        struct.pack_into('<II', pd, 188, P, 2000)
        tag(5, 32, pd)
        lvd = bytearray(S)
        struct.pack_into('<I', lvd, 212, S)
        struct.pack_into('<IIH', lvd, 248, S, 0, 1)            # FSD at metadata lbn 0
        struct.pack_into('<II', lvd, 264, 70, 2)
        lvd[440:446] = bytes([1, 6]) + struct.pack('<HH', 1, 0)
        meta = bytearray(64)
        meta[0:2] = bytes([2, 64])
        meta[5:28] = b'*UDF Metadata Partition'
        struct.pack_into('<HHIII', meta, 36, 1, 0, 0, 0xFFFFFFFF, 0xFFFFFFFF)
        lvd[446:510] = meta
        tag(6, 33, lvd)
        td = bytearray(S)
        tag(8, 34, td)
        avdp = bytearray(S)
        struct.pack_into('<II', avdp, 16, 16 * S, 32)
        tag(2, 256, avdp)
        sectors.update({256: avdp, 32: pd, 33: lvd, 34: td})
        # Physical lbn 0: the metadata file; metadata blocks 0..19 live at physical lbn 1..20.
        sectors[P] = file_entry(0, 20 * S, short_ad(1, 20 * S), 0, extended=True)
        m = lambda lbn: P + 1 + lbn
        fsd = bytearray(S)
        struct.pack_into('<IIH', fsd, 400, S, 1, 1)
        tag(256, 0, fsd)
        sectors[m(0)] = fsd

        def directory(fe_lbn, data_lbn, entries, extended):
            data = fid('', 0, parent=True) + b''.join(entries)
            sectors[m(fe_lbn)] = file_entry(fe_lbn, len(data), short_ad(data_lbn, len(data)), 0, True, extended)
            sectors[m(data_lbn)] = bytearray(data.ljust(S, b'\0'))
        directory(1, 2, [fid('AACS', 3, directory=True), fid('BDMV', 5, directory=True)], True)
        directory(3, 4, [fid('Unit_Key_RO.inf', 9), fid('MKB_RO.inf', 10)], False)
        directory(5, 6, [fid('STREAM', 7, directory=True)], True)
        directory(7, 8, [fid('00001.m2ts', 11), fid('00002.m2ts', 12)], False)
        self.files = {}

        def place(fe_lbn, data, extents, extended=False):
            """Store `data` over physical extents [(lbn, sectors)], as long_ads into partition 0."""
            ads, pos = b'', 0
            for lbn, count in extents:
                chunk = data[pos:pos + count * S]
                for i in range(count):
                    sectors[P + lbn + i] = bytearray(chunk[i * S:(i + 1) * S].ljust(S, b'\0'))
                ads += long_ad(lbn, len(chunk), 0)
                pos += len(chunk)
            assert pos >= len(data)
            sectors[m(fe_lbn)] = file_entry(fe_lbn, len(data), ads, 1, extended=extended)
        place(9, self.inf, [(40, 1)])
        place(10, self.mkb, [(42, 3)], extended=True)
        # The clip in two extents with a gap; 50 sectors is not a whole number of units,
        # so one unit straddles the extent boundary.
        clip_sectors = len(self.clip) // S
        place(11, self.clip, [(100, 50), (400, clip_sectors - 50)])
        place(12, self.small, [(60, len(self.small) // S)])
        size = (max(sectors) + 1) * S
        self.image = bytearray(size)
        for n, block in sectors.items():
            self.image[n * S:(n + 1) * S] = block
        self.reads = []

    @staticmethod
    def unit_key_ro(keys, stride=48):
        pos = 32
        inf = bytearray(pos + 48 + stride * len(keys))
        struct.pack_into('>I', inf, 0, pos)
        struct.pack_into('>H', inf, pos, len(keys))
        for i, k in enumerate(keys):
            inf[pos + 48 + i * stride:pos + 64 + i * stride] = k
        return bytes(inf)

    def read(self, off, n):
        self.reads.append((off, n))
        if off < 0 or off + n > len(self.image):
            raise IOError('read past the image')
        return bytes(self.image[off:off + n])


class Service:
    """A fake /decode endpoint: records the request, returns a canned answer."""

    def __init__(self, answer):
        self.answer, self.requests = answer, []

    def __call__(self, url, auth, body):
        self.requests.append((url, auth, body))
        if isinstance(self.answer, Exception):
            raise self.answer
        return self.answer


class CryptoTests(unittest.TestCase):
    def test_fips_197_vector(self):
        rk = mc.expand_key(bytes(range(16)))
        pt = bytes.fromhex('00112233445566778899aabbccddeeff')
        ct = bytes.fromhex('69c4e0d86a7b0430d8cdb78070b4c55a')
        self.assertEqual(mc.encrypt_block(rk, pt), ct)
        self.assertEqual(mc.decrypt_block(rk, ct), pt)

    def test_unit_round_trip_and_the_structural_proof(self):
        clear = clip_units(UK, clear=range(CLIP_UNITS))[0]
        self.assertTrue(mc.is_clean(clear))
        enc = clip_units(UK, clear=())[0]
        self.assertTrue(mc.unit_encrypted(enc))
        self.assertFalse(mc.is_clean(enc))
        self.assertEqual(enc[:16], bytes([clear[0] | 0xC0]) + clear[1:16], 'the seed stays clear')
        self.assertEqual(enc[7 * mc.PACKET:8 * mc.PACKET], bytes(mc.PACKET), 'padding stays zero')
        self.assertTrue(mc.is_clean(mc.decrypt_unit(enc, UK)))
        self.assertFalse(mc.is_clean(mc.decrypt_unit(enc, VUK)))


class UdfTests(unittest.TestCase):
    def test_paths_and_bytes(self):
        d = Disc()
        udf = mc.Udf(d.read)
        inf = udf.lookup('/AACS/Unit_Key_RO.inf')
        self.assertEqual(udf.read_entry(inf, 0, inf['size']), d.inf)
        mkb = udf.lookup('/aacs/mkb_ro.inf')
        self.assertEqual(udf.read_entry(mkb, 0, mkb['size']), d.mkb)
        clip = udf.lookup('/BDMV/STREAM/00001.m2ts')
        self.assertEqual(clip['size'], len(d.clip))
        boundary = 50 * S
        self.assertEqual(udf.read_entry(clip, boundary - 100, 300), d.clip[boundary - 100:boundary + 200],
                         'a read across the extent boundary follows the allocation descriptors')
        self.assertEqual(sorted(udf.listdir(udf.lookup('/BDMV/STREAM'))), ['00001.m2ts', '00002.m2ts'])
        with self.assertRaises(mc.CanaryError):
            udf.lookup('/AACS/Content000.cer')

    def test_a_corrupt_descriptor_is_refused(self):
        d = Disc()
        d.image[256 * S + 100] ^= 0xFF       # AVDP reserved bytes: fine; its tag is intact
        mc.Udf(d.read)
        d.image[256 * S + 1] ^= 0xFF         # AVDP tag: checksum no longer matches
        with self.assertRaises(mc.CanaryError):
            mc.Udf(d.read)

    def test_samples_are_the_encrypted_units_of_the_largest_clip(self):
        d = Disc()
        inf, mkb, samples = mc.disc_inputs(mc.Udf(d.read))
        self.assertEqual((inf, mkb), (d.inf, d.mkb))
        self.assertGreaterEqual(len(samples), mc.MIN_SAMPLE_UNITS)
        self.assertEqual(len(set(samples)), len(samples), 'no unit sampled twice')
        for s in samples:
            self.assertIn(s, d.units, 'every sample is a whole, clip-anchored aligned unit')
            self.assertTrue(mc.unit_encrypted(s))
        self.assertNotIn(d.units[5], samples, 'a clear unit is not a sample')
        self.assertIn(d.units[16], samples, 'the unit straddling the extent boundary is read whole')


class ProbeTests(unittest.TestCase):
    def test_uk_answer_that_decrypts_the_fixture_passes(self):
        d = Disc()
        svc = Service({'UK': UK.hex()})
        got = mc.probe(mc.Udf(d.read), 'https://keys.example/decode', 'tok', svc)
        self.assertGreaterEqual(got['scrambled'], mc.MIN_SAMPLE_UNITS)
        url, auth, body = svc.requests[0]
        self.assertEqual((url, auth), ('https://keys.example/decode', 'tok'))
        self.assertEqual(set(body), {'inf_b64', 'mkb_b64', 'units_b64'}, 'what OnlineSource sends from an image')
        self.assertEqual(base64.b64decode(body['inf_b64']), d.inf)
        self.assertEqual(base64.b64decode(body['mkb_b64']), d.mkb)
        self.assertGreaterEqual(len(body['units_b64']), mc.MIN_SAMPLE_UNITS)
        self.assertLessEqual(len(body['units_b64']), mc.SAMPLES)

    def test_uk_list_and_0x_prefix(self):
        d = Disc()
        mc.probe(mc.Udf(d.read), 'https://k/d', 't', Service({'UK': [VUK.hex(), '0x' + UK.hex()]}))

    def test_vuk_answer_derives_the_unit_key_from_the_disc(self):
        d = Disc()
        mc.probe(mc.Udf(d.read), 'https://k/d', 't', Service({'VUK': VUK.hex()}))
        d = Disc(inf=Disc.unit_key_ro([bytes(16), mc.encrypt_block(mc.expand_key(VUK), UK)], stride=64))
        mc.probe(mc.Udf(d.read), 'https://k/d', 't', Service({'VUK': VUK.hex()}))

    def test_every_wrong_answer_fails(self):
        wrong = bytes(16)
        cases = {
            'wrong UK': {'UK': wrong.hex()},
            'wrong VUK': {'VUK': wrong.hex()},
            'no key': {},
            'malformed UK': {'UK': 'zz' * 16},
            'not an object': ['UK'],
            'unreachable': mc.CanaryError('the key service is unreachable (URLError)'),
        }
        for name, answer in cases.items():
            with self.subTest(name=name):
                with self.assertRaises(mc.CanaryError):
                    mc.probe(mc.Udf(Disc().read), 'https://k/d', 't', Service(answer))

    def test_a_key_for_part_of_the_disc_fails(self):
        d = Disc()
        other = bytes.fromhex('ffeeddccbbaa99887766554433221100')
        d2 = Disc(key=other)
        mixed = d.units[:20] + d2.units[20:]
        self.assertFalse(mc.prove([u for u in mixed if mc.unit_encrypted(u)], [UK])[0])
        self.assertTrue(mc.prove([u for u in mixed if mc.unit_encrypted(u)], [UK, other])[0])

    def test_post_decode(self):
        seen = []

        def opener(req, timeout):
            seen.append((req, timeout))
            return Resp(b'{"UK": "00"}')
        self.assertEqual(mc.post_decode('https://k/d', 'tok', {'a': 1}, opener), {'UK': '00'})
        req, timeout = seen[0]
        self.assertEqual((req.get_method(), req.get_header('Authorization'), timeout), ('POST', 'Bearer tok', 180))
        self.assertEqual(json.loads(req.data), {'a': 1})
        with self.assertRaises(mc.CanaryError):
            mc.post_decode('http://k/d', 'tok', {}, opener)

        def denied(req, timeout):
            raise urllib.error.HTTPError(req.full_url, 401, 'no', {}, io.BytesIO(b'secret detail'))
        with self.assertRaisesRegex(mc.CanaryError, 'HTTP 401'):
            mc.post_decode('https://k/d', 'tok', {}, denied)
        with self.assertRaisesRegex(mc.CanaryError, 'size cap'):
            mc.post_decode('https://k/d', 'tok', {}, lambda r, timeout: Resp(b'x' * (mc.MAX_RESPONSE + 1)))


class Resp(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


class SecretTests(unittest.TestCase):
    """Review FB2: nothing that touches FMKV_KEY_AUTH or FMKV_KEY_URL is echoed."""
    TOKEN = 'tok-5ecret-\x07-tail'

    def test_a_token_a_header_cannot_carry_is_never_quoted(self):
        # What http.client says for such a token: it quotes the whole header value.
        def opener(req, timeout):
            raise ValueError(f"Invalid header value b'Bearer {self.TOKEN}'")
        with self.assertRaises(mc.CanaryError) as cm:
            mc.post_decode('https://k/d', self.TOKEN, {}, opener)
        self.assertNotIn('5ecret', str(cm.exception))
        with self.assertRaises(mc.CanaryError) as cm:
            mc.post_decode('https://k/d', 'tok-5ecret-ok', {}, opener)
        self.assertNotIn('5ecret', str(cm.exception), 'an unexpected error is reported by type only')

    def test_run_output_never_carries_the_token_or_url(self):
        url = 'https://k.example/d?key=5ecretq'
        lines = []

        def leaky(u, auth, body):
            raise ValueError(f"Invalid header value b'Bearer {auth}' for {u}")
        result = mc.run(RunTests.POLICY, RunTests.PINS, url, 'tok-5ecret', lambda k, p: Disc().read, leaky,
                        log=lines.append)
        text = json.dumps(result) + '\n'.join(lines)
        self.assertFalse(result['ok'])
        self.assertNotIn('5ecret', text)
        self.assertIn('ValueError', text)

    def test_main_error_is_type_only(self):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, d)
        (d / 'ext.json').write_text('{"fixtures": {}}')
        with unittest.mock.patch.object(mc, 'run', side_effect=RuntimeError('Bearer tok-5ecret')), \
                contextlib.redirect_stdout(io.StringIO()) as out:
            mc.main(['--bucket', 'b', '--externals', str(d / 'ext.json'), '--out', str(d / 'c.json')])
        self.assertNotIn('5ecret', (d / 'c.json').read_text() + out.getvalue())


class RetryTests(unittest.TestCase):
    """Review FB3: a transient failure is retried with backoff; a wrong or missing key is not."""

    def post(self, outcomes):
        calls, sleeps = [], []

        def opener(req, timeout):
            calls.append(req)
            o = outcomes[min(len(calls), len(outcomes)) - 1]
            if isinstance(o, BaseException):
                raise o
            return Resp(o)
        return calls, sleeps, lambda: mc.post_decode('https://k/d', 't', {}, opener, sleep=sleeps.append)

    def http(self, code):
        return urllib.error.HTTPError('https://k/d', code, 'x', {}, io.BytesIO(b''))

    def test_transient_then_answer(self):
        for name, first in (('5xx', self.http(503)), ('429', self.http(429)),
                            ('network', urllib.error.URLError('reset')), ('timeout', TimeoutError())):
            with self.subTest(name=name):
                calls, sleeps, go = self.post([first, b'{"UK": "00"}'])
                self.assertEqual(go(), {'UK': '00'})
                self.assertEqual(len(calls), 2)
                self.assertEqual(sleeps, [mc.BACKOFF[0]])

    def test_gives_up_after_the_attempts(self):
        calls, sleeps, go = self.post([self.http(502)])
        with self.assertRaisesRegex(mc.CanaryError, 'HTTP 502'):
            go()
        self.assertEqual(len(calls), mc.ATTEMPTS)
        self.assertEqual(len(sleeps), mc.ATTEMPTS - 1)

    def test_never_retries_an_answer(self):
        for name, outcome in (('401', self.http(401)), ('404', self.http(404)), ('no key', b'{}'),
                              ('not json', b'<html>')):
            with self.subTest(name=name):
                calls, sleeps, go = self.post([outcome, b'{"UK": "00"}'])
                try:
                    got = go()
                except mc.CanaryError:
                    got = None
                self.assertEqual(len(calls), 1)
                self.assertEqual(sleeps, [])
                if name == 'no key':
                    with self.assertRaises(mc.CanaryError):
                        mc.answer_keys(got, b'')


class RunTests(unittest.TestCase):
    POLICY = {'canary': {'probes': [{'fixture': 'uhd.iso'}]}}
    PINS = {'uhd.iso': {'etag': '"e-9"', 'size': 1, 'version_id': None}}

    def run_canary(self, answer, url='https://k/d', auth='tok', pins=PINS, disc=None):
        d = disc or Disc()
        lines = []
        result = mc.run(self.POLICY, pins, url, auth, lambda key, pin: d.read, Service(answer), log=lines.append)
        return result, lines

    def test_pass(self):
        result, lines = self.run_canary({'UK': UK.hex()})
        self.assertTrue(result['ok'])
        self.assertEqual(result['probes'][0]['fixture'], 'uhd.iso')
        text = json.dumps(result) + '\n'.join(lines)
        for secret in (UK.hex(), 'tok', 'https://k/d'):
            self.assertNotIn(secret, text, 'neither the key, the token nor the URL is ever written')

    def test_failures_are_recorded_not_raised(self):
        cases = {
            'wrong key': dict(answer={'UK': VUK.hex()}),
            'no secrets': dict(answer={'UK': UK.hex()}, url='', auth=''),
            'no pin': dict(answer={'UK': UK.hex()}, pins={'uhd.iso': {'error': 'AccessDenied'}}),
            'service down': dict(answer=mc.CanaryError('the key service answered HTTP 503')),
            'unexpected error': dict(answer=KeyError('boom')),
        }
        for name, kw in cases.items():
            with self.subTest(name=name):
                result, lines = self.run_canary(**kw)
                self.assertFalse(result['ok'])
                self.assertFalse(result['probes'][0]['ok'])
                self.assertTrue(result['probes'][0]['reason'])
                self.assertIn('FAIL', lines[0])

    def test_s3_image_pins_every_read(self):
        calls = []

        class Body(io.BytesIO):
            pass

        class S3:
            def get_object(self, **kw):
                calls.append(kw)
                lo, hi = map(int, kw['Range'][6:].split('-'))
                return {'Body': Body(bytes(hi - lo + 1))}
        img = mc.S3Image(S3(), 'b', 'uhd.iso', {'etag': '"e-9"', 'size': 1 << 20, 'version_id': None})
        self.assertEqual(len(img.read(100, 10)), 10)
        self.assertEqual(len(img.read(200, 10)), 10)
        self.assertEqual(len(calls), 1, 'small reads share a cached block')
        self.assertEqual(calls[0]['IfMatch'], '"e-9"')
        img = mc.S3Image(S3(), 'b', 'uhd.iso', {'etag': '"e-9"', 'size': 1 << 20, 'version_id': 'v1'})
        img.read(0, 600 << 10)
        self.assertEqual(calls[-1]['VersionId'], 'v1')
        with self.assertRaises(mc.CanaryError):
            img.read((1 << 20) - 5, 10)

        class Changed:
            def get_object(self, **kw):
                exc = Exception('precondition')
                exc.response = {'Error': {'Code': 'PreconditionFailed'}, 'ResponseMetadata': {'HTTPStatusCode': 412}}
                raise exc
        with self.assertRaisesRegex(mc.CanaryError, 'changed in S3'):
            mc.S3Image(Changed(), 'b', 'uhd.iso', {'etag': '"e"', 'size': 100}).read(0, 10)

    def test_main_always_writes_a_result(self):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, d)
        out = d / 'canary.json'
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(mc.main(['--bucket', 'b', '--externals', str(d / 'missing.json'), '--out', str(out)]), 0)
        self.assertFalse(json.loads(out.read_text())['ok'])


if __name__ == '__main__':
    unittest.main()
