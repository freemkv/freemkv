#!/usr/bin/env python3
"""Key-service canary (design v4 D2, decision 15): the online key service still answers for a fixture.

  media_canary.py --bucket B --externals externals.json --out canary.json

For each probe in the policy's canary.probes, the fixture's AACS inputs are read straight out of
the pinned S3 object with ranged GETs (VersionId, or If-Match on the planned ETag, exactly as
tests/media_fetch.py pins its fetch): the disc's UDF file system gives /AACS/Unit_Key_RO.inf,
/AACS/MKB_RO.inf and the largest /BDMV/STREAM/*.m2ts, whose encrypted aligned units are sampled.
That is the request freemkv-keysources' OnlineSource sends (inf_b64, mkb_b64, units_b64), POSTed to
FMKV_KEY_URL with FMKV_KEY_AUTH as the bearer token. A probe passes only if the answer (a UK, or a
VUK over the disc's own title keys) decrypts every sampled unit into clean MPEG-TS: the fixture's
known key, proven against the fixture's own ciphertext rather than a stored digest.

Always writes --out and exits 0; plan-media turns a failed or missing result into qa red.
Never logs the request, the answer or a key. Needs boto3 (tests/media-fetch-requirements.txt).
"""

import argparse
import base64
import json
import os
from pathlib import Path
import struct
import sys
import urllib.error
import urllib.request

SECTOR = 2048
UNIT = 6144                     # an AACS aligned unit: 3 sectors
PACKET = 192                    # a BD source packet: 4-byte TP_extra_header + 188-byte TS packet
MIN_SAMPLE_UNITS = 8            # libfreemkv keysource::MIN_SAMPLE_UNITS: the client sends no fewer
SAMPLES = 64                    # OnlineSource asks its context for this many
PROBES_PER_FILE, CHUNK_UNITS = 8, 15   # libfreemkv keysource::read_encrypted_units
KEY_PROOF_PACKETS = 4           # libfreemkv aacs::content: synced packets that prove a key
MAX_MKB = 64 << 20              # OnlineSource MAX_MKB_BYTES
MAX_INF = 1 << 20
MAX_RESPONSE = 1 << 20          # OnlineSource MAX_RESPONSE_BYTES
TIMEOUT = 180                   # OnlineSource TIMEOUT_SECS
AACS_IV = bytes.fromhex('0BA0F8DDFEA61FB3D8DF9F566A050F78')
POLICY = Path(__file__).with_name('media-gate-policy.json')


class CanaryError(Exception):
    """A probe failed; the message is safe to print (no key material, no URL, no token)."""


class Transient(CanaryError):
    """The service did not answer (network, 5xx, 429): worth another try. A wrong or missing key never is."""


ATTEMPTS = 3
BACKOFF = (5, 15)


# ── AES-128 (FIPS-197), enough for the AACS unit decrypt ───────────────────

def _rotl8(x, s):
    return ((x << s) | (x >> (8 - s))) & 0xFF


def _tables():
    sbox, p, q = [0] * 256, 1, 1
    while True:
        p = (p ^ (p << 1) ^ (0x1B if p & 0x80 else 0)) & 0xFF
        q ^= q << 1
        q ^= q << 2
        q ^= q << 4
        q &= 0xFF
        if q & 0x80:
            q ^= 0x09
        sbox[p] = q ^ _rotl8(q, 1) ^ _rotl8(q, 2) ^ _rotl8(q, 3) ^ _rotl8(q, 4) ^ 0x63
        if p == 1:
            break
    sbox[0] = 0x63
    inv = [0] * 256
    for i, v in enumerate(sbox):
        inv[v] = i

    def mul(a, b):
        r = 0
        while b:
            if b & 1:
                r ^= a
            a = ((a << 1) ^ 0x1B) & 0xFF if a & 0x80 else a << 1
            b >>= 1
        return r
    muls = {k: [mul(i, k) for i in range(256)] for k in (2, 3, 9, 11, 13, 14)}
    return sbox, inv, muls


SBOX, INV_SBOX, MUL = _tables()
SHIFT = [0, 5, 10, 15, 4, 9, 14, 3, 8, 13, 2, 7, 12, 1, 6, 11]
INV_SHIFT = [SHIFT.index(i) for i in range(16)]


def expand_key(key):
    if len(key) != 16:
        raise ValueError('AES-128 needs a 16-byte key')
    w = list(key)
    rcon = 1
    while len(w) < 176:
        t = w[-4:]
        if len(w) % 16 == 0:
            t = [SBOX[t[1]] ^ rcon, SBOX[t[2]], SBOX[t[3]], SBOX[t[0]]]
            rcon = ((rcon << 1) ^ 0x1B) & 0xFF if rcon & 0x80 else rcon << 1
        w += [a ^ b for a, b in zip(w[-16:-12], t)]
    return [w[i:i + 16] for i in range(0, 176, 16)]


def encrypt_block(rk, block):
    s = [a ^ b for a, b in zip(block, rk[0])]
    m2, m3 = MUL[2], MUL[3]
    for r in range(1, 11):
        s = [SBOX[s[SHIFT[i]]] for i in range(16)]
        if r < 10:
            out = []
            for c in range(0, 16, 4):
                a0, a1, a2, a3 = s[c:c + 4]
                out += [m2[a0] ^ m3[a1] ^ a2 ^ a3, a0 ^ m2[a1] ^ m3[a2] ^ a3,
                        a0 ^ a1 ^ m2[a2] ^ m3[a3], m3[a0] ^ a1 ^ a2 ^ m2[a3]]
            s = out
        s = [a ^ b for a, b in zip(s, rk[r])]
    return bytes(s)


def decrypt_block(rk, block):
    s = [a ^ b for a, b in zip(block, rk[10])]
    m9, m11, m13, m14 = MUL[9], MUL[11], MUL[13], MUL[14]
    for r in range(9, -1, -1):
        s = [INV_SBOX[s[INV_SHIFT[i]]] for i in range(16)]
        s = [a ^ b for a, b in zip(s, rk[r])]
        if r:
            out = []
            for c in range(0, 16, 4):
                a0, a1, a2, a3 = s[c:c + 4]
                out += [m14[a0] ^ m11[a1] ^ m13[a2] ^ m9[a3], m9[a0] ^ m14[a1] ^ m11[a2] ^ m13[a3],
                        m13[a0] ^ m9[a1] ^ m14[a2] ^ m11[a3], m11[a0] ^ m13[a1] ^ m9[a2] ^ m14[a3]]
            s = out
    return bytes(s)


def _xor(a, b):
    return bytes(x ^ y for x, y in zip(a, b))


# ── AACS aligned units (libfreemkv aacs::content, mirrored) ────────────────

def unit_encrypted(unit):
    """The copy-permission bits of the first TP_extra_header: set on an encrypted unit."""
    return len(unit) >= UNIT and unit[0] & 0xC0 != 0


def is_clean(unit):
    """At least min(content packets, 4) of packets 1.. carry the 0x47 sync (packet 0 is clear)."""
    content = synced = 0
    for off in range(PACKET, min(len(unit), UNIT) - PACKET + 1, PACKET):
        if any(unit[off + 4:off + PACKET]):
            content += 1
            synced += unit[off + 4] == 0x47
    return content == 0 or synced >= min(content, KEY_PROOF_PACKETS)


def _pads(unit):
    return [not any(unit[o:o + PACKET]) for o in range(0, UNIT, PACKET)]


def decrypt_unit(unit, key):
    """The seed (bytes 0..16) stays clear; the rest is AES-128-CBC under E(k, seed) ^ seed."""
    pads = _pads(unit)
    seed = bytes(unit[:16])
    rk = expand_key(_xor(encrypt_block(expand_key(key), seed), seed))
    out, prev = bytearray(seed), AACS_IV
    for i in range(16, UNIT, 16):
        block = bytes(unit[i:i + 16])
        out += _xor(decrypt_block(rk, block), prev)
        prev = block
    for p, pad in enumerate(pads):
        if pad:
            out[p * PACKET:(p + 1) * PACKET] = bytes(PACKET)
    return bytes(out)


def encrypt_unit(unit, key):
    """The exact inverse of decrypt_unit (the tests build fixtures with it)."""
    pads = _pads(unit)
    seed = bytes(unit[:16])
    rk = expand_key(_xor(encrypt_block(expand_key(key), seed), seed))
    out, prev = bytearray(seed), AACS_IV
    for i in range(16, UNIT, 16):
        prev = encrypt_block(rk, _xor(unit[i:i + 16], prev))
        out += prev
    for p, pad in enumerate(pads):
        if pad:
            out[p * PACKET:(p + 1) * PACKET] = bytes(PACKET)
    return bytes(out)


def title_keys(inf):
    """Every encrypted unit key Unit_Key_RO.inf could hold, at both the AACS 1 and 2 strides."""
    if len(inf) < 20:
        raise CanaryError('Unit_Key_RO.inf is truncated')
    pos = struct.unpack_from('>I', inf, 0)[0]
    if pos + 2 > len(inf):
        raise CanaryError('Unit_Key_RO.inf key storage is out of range')
    count = struct.unpack_from('>H', inf, pos)[0]
    keys = []
    for stride in (48, 64):
        for i in range(count):
            at = pos + 48 + i * stride
            if at + 16 <= len(inf) and inf[at:at + 16] not in keys:
                keys.append(inf[at:at + 16])
    return keys


def parse_key(text):
    if not isinstance(text, str):
        return None
    text = text[2:] if text[:2] in ('0x', '0X') else text
    if len(text) != 32 or any(c not in '0123456789abcdefABCDEF' for c in text):
        return None
    return bytes.fromhex(text)


def answer_keys(reply, inf):
    """The unit keys an OnlineSource derives from a /decode answer (UK, or VUK over the title keys)."""
    if not isinstance(reply, dict):
        raise CanaryError('the key service answered with something other than a JSON object')
    uk = reply.get('UK')
    if isinstance(uk, str):
        uk = [uk]
    if isinstance(uk, list) and uk:
        keys = [parse_key(k) for k in uk]
        if None in keys:
            raise CanaryError('the key service returned a malformed UK')
        return keys
    vuk = parse_key(reply.get('VUK'))
    if vuk:
        rk = expand_key(vuk)
        return [decrypt_block(rk, k) for k in title_keys(inf)]
    raise CanaryError('the key service has no key for this fixture')


def prove(samples, keys):
    """Every scrambled sample must decrypt clean under some key; (proven, scrambled count)."""
    scrambled = [s for s in samples if unit_encrypted(s) and not is_clean(s)]
    if not scrambled:
        raise CanaryError('no scrambled sample to prove a key against')
    for s in scrambled:
        if not any(is_clean(decrypt_unit(s, k)) for k in keys):
            return False, len(scrambled)
    return True, len(scrambled)


# ── UDF (ECMA-167 / UDF 2.50 with a metadata partition), read-only ─────────

def _u16(b, o):
    return struct.unpack_from('<H', b, o)[0]


def _u32(b, o):
    return struct.unpack_from('<I', b, o)[0]


def _u64(b, o):
    return struct.unpack_from('<Q', b, o)[0]


def _tag(block, want):
    """The descriptor tag id, after checking its checksum; `want` (an id or ids) is enforced."""
    if len(block) < 16 or (sum(block[0:4]) + sum(block[5:16])) & 0xFF != block[4]:
        raise CanaryError('UDF descriptor tag checksum mismatch')
    tid = _u16(block, 0)
    if want is not None and tid not in ((want,) if isinstance(want, int) else want):
        raise CanaryError(f'UDF descriptor {tid} where {want} was expected')
    return tid


class Udf:
    """Just enough UDF to find a file by path and read byte ranges of it."""

    def __init__(self, read):
        self.read = read
        avdp = self.sector(256)
        _tag(avdp, 2)
        length, loc = _u32(avdp, 16), _u32(avdp, 20)
        starts, lvd = {}, None
        for i in range(max(1, length // SECTOR)):
            d = self.sector(loc + i)
            tid = _tag(d, None)
            if tid == 5:
                starts[_u16(d, 22)] = _u32(d, 188)
            elif tid == 6:
                lvd = d
            elif tid == 8:
                break
        if lvd is None:
            raise CanaryError('no UDF logical volume descriptor')
        if _u32(lvd, 212) != SECTOR:
            raise CanaryError(f'UDF block size {_u32(lvd, 212)} is not {SECTOR}')
        self.maps, off, meta = [], 440, []
        for _ in range(_u32(lvd, 268)):
            kind, size = lvd[off], lvd[off + 1]
            if kind == 1:
                self.maps.append(('phys', starts[_u16(lvd, off + 4)]))
            elif kind == 2 and lvd[off + 5:off + 28].startswith(b'*UDF Metadata Partition'):
                self.maps.append(('meta', starts[_u16(lvd, off + 38)], None))
                meta.append((len(self.maps) - 1, _u32(lvd, off + 40)))
            else:
                raise CanaryError(f'unsupported UDF partition map type {kind}')
            off += size
        for ref, loc in meta:
            phys = next(i for i, m in enumerate(self.maps) if m[0] == 'phys' and m[1] == self.maps[ref][1])
            entry = self.entry(phys, loc)
            self.maps[ref] = ('meta', self.maps[ref][1], [(lbn, n // SECTOR) for _, lbn, n in entry['extents']])
        fsd_ref, fsd_lbn = _u16(lvd, 256), _u32(lvd, 252)
        fsd = self.block(fsd_ref, fsd_lbn)
        _tag(fsd, 256)
        self.root = (_u16(fsd, 408), _u32(fsd, 404))

    def sector(self, n):
        return self.read(n * SECTOR, SECTOR)

    def absolute(self, ref, lbn):
        m = self.maps[ref]
        if m[0] == 'phys':
            return m[1] + lbn
        for start, count in m[2]:
            if lbn < count:
                return m[1] + start + lbn
            lbn -= count
        raise CanaryError('UDF metadata block out of range')

    def block(self, ref, lbn):
        return self.sector(self.absolute(ref, lbn))

    def _ads(self, data, ad_type, ref, out):
        step = 8 if ad_type == 0 else 16
        for o in range(0, len(data) - step + 1, step):
            raw = _u32(data, o)
            kind, length = raw >> 30, raw & 0x3FFFFFFF
            if length == 0:
                break
            lbn = _u32(data, o + 4)
            pref = ref if ad_type == 0 else _u16(data, o + 8)
            if kind == 3:
                aed = self.block(pref, lbn)
                _tag(aed, 258)
                self._ads(aed[24:24 + _u32(aed, 20)], ad_type, ref, out)
                return
            out.append((pref if kind == 0 else None, lbn, length))

    def entry(self, ref, lbn):
        fe = self.block(ref, lbn)
        tid = _tag(fe, (261, 266))
        l_ea, l_ad, base = (_u32(fe, 168), _u32(fe, 172), 176) if tid == 261 else (_u32(fe, 208), _u32(fe, 212), 216)
        ad_type = _u16(fe, 34) & 7
        ads = fe[base + l_ea:base + l_ea + l_ad]
        out = {'size': _u64(fe, 56), 'directory': fe[27] == 4, 'extents': [], 'embedded': None}
        if ad_type == 3:
            out['embedded'] = bytes(ads)
        elif ad_type in (0, 1):
            self._ads(ads, ad_type, ref, out['extents'])
        else:
            raise CanaryError(f'unsupported UDF allocation descriptor type {ad_type}')
        return out

    def read_entry(self, entry, off, n):
        n = max(0, min(n, entry['size'] - off))
        if entry['embedded'] is not None:
            return entry['embedded'][off:off + n]
        out, pos = bytearray(), 0
        for ref, lbn, length in entry['extents']:
            if n <= 0:
                break
            if off < pos + length:
                skip = off - pos
                take = min(length - skip, n)
                if ref is None:
                    out += bytes(take)
                elif self.maps[ref][0] == 'phys':
                    out += self.read(self.absolute(ref, lbn) * SECTOR + skip, take)
                else:
                    first, last = skip // SECTOR, (skip + take - 1) // SECTOR
                    data = b''.join(self.block(ref, lbn + b) for b in range(first, last + 1))
                    out += data[skip - first * SECTOR:skip - first * SECTOR + take]
                off, n = off + take, n - take
            pos += length
        return bytes(out)

    def listdir(self, entry):
        data, out, pos = self.read_entry(entry, 0, entry['size']), {}, 0
        while pos + 38 <= len(data):
            _tag(data[pos:pos + 16], 257)
            chars, l_fi, l_iu = data[pos + 18], data[pos + 19], _u16(data, pos + 36)
            icb = (_u16(data, pos + 28), _u32(data, pos + 24))
            raw = data[pos + 38 + l_iu:pos + 38 + l_iu + l_fi]
            if not chars & 0x08 and raw:
                name = raw[1:].decode('utf-16-be') if raw[0] == 16 else raw[1:].decode('latin-1')
                out[name] = icb
            pos += (38 + l_iu + l_fi + 3) & ~3
        return out

    def lookup(self, path):
        entry = self.entry(*self.root)
        for part in [p for p in path.split('/') if p]:
            names = self.listdir(entry)
            hit = names.get(part) or next((v for k, v in names.items() if k.upper() == part.upper()), None)
            if hit is None:
                raise CanaryError(f'{path}: not on the disc')
            entry = self.entry(*hit)
        return entry


# ── The probe ──────────────────────────────────────────────────────────────

def disc_inputs(udf, count=SAMPLES):
    """(Unit_Key_RO.inf, MKB, encrypted sample units) as OnlineSource would send them."""
    inf_entry, mkb_entry = udf.lookup('/AACS/Unit_Key_RO.inf'), udf.lookup('/AACS/MKB_RO.inf')
    if inf_entry['size'] > MAX_INF or mkb_entry['size'] > MAX_MKB:
        raise CanaryError('Unit_Key_RO.inf or MKB_RO.inf is oversized')
    inf, mkb = udf.read_entry(inf_entry, 0, inf_entry['size']), udf.read_entry(mkb_entry, 0, mkb_entry['size'])
    stream = udf.lookup('/BDMV/STREAM')
    clips = [(name, udf.entry(*icb)) for name, icb in udf.listdir(stream).items() if name.lower().endswith('.m2ts')]
    if not clips:
        raise CanaryError('no .m2ts clip under /BDMV/STREAM')
    name, clip = max(clips, key=lambda c: c[1]['size'])
    total, samples, next_unit = clip['size'] // UNIT, [], 0
    for p in range(1, PROBES_PER_FILE + 1):
        unit = max(total * p // (PROBES_PER_FILE + 1), next_unit)
        if unit >= total:
            continue
        units = min(CHUNK_UNITS, total - unit)
        next_unit = unit + units
        data = udf.read_entry(clip, unit * UNIT, units * UNIT)
        for i in range(0, len(data) - UNIT + 1, UNIT):
            if unit_encrypted(data[i:i + UNIT]):
                samples.append(data[i:i + UNIT])
                if len(samples) >= count:
                    return inf, mkb, samples
    return inf, mkb, samples


def _post_once(url, headers, body, opener):
    req = urllib.request.Request(url, data=body, headers=headers, method='POST')
    try:
        with (opener or urllib.request.urlopen)(req, timeout=TIMEOUT) as resp:
            data = resp.read(MAX_RESPONSE + 1)
    except urllib.error.HTTPError as exc:
        if exc.code >= 500 or exc.code == 429:
            raise Transient(f'the key service answered HTTP {exc.code}') from None
        raise CanaryError(f'the key service answered HTTP {exc.code}') from None
    except (urllib.error.URLError, OSError) as exc:
        raise Transient(f'the key service is unreachable ({type(exc).__name__})') from None
    if len(data) > MAX_RESPONSE:
        raise CanaryError('the key service answer is over the size cap')
    try:
        return json.loads(data)
    except ValueError:
        raise CanaryError('the key service answer is not JSON') from None


def post_decode(url, auth, body, opener=None, sleep=None):
    """POST the /decode request the way OnlineSource does; the parsed JSON answer. A transient
    failure (network, 5xx, 429) is retried ATTEMPTS times with backoff; nothing else is.
    No error message ever carries the URL or the token: anything unexpected is reported by type."""
    if not url.startswith('https://'):
        raise CanaryError('FMKV_KEY_URL is not https:// (key material is never sent in cleartext)')
    if auth and not all(0x20 < ord(c) < 0x7F for c in auth):
        raise CanaryError('FMKV_KEY_AUTH holds characters an HTTP header cannot carry (not shown)')
    headers = {'Content-Type': 'application/json', 'Accept': 'application/json'}
    if auth:
        headers['Authorization'] = f'Bearer {auth}'
    data = json.dumps(body).encode()
    sleep = sleep or __import__('time').sleep
    for attempt in range(ATTEMPTS):
        try:
            return _post_once(url, headers, data, opener)
        except Transient:
            if attempt == ATTEMPTS - 1:
                raise
            sleep(BACKOFF[min(attempt, len(BACKOFF) - 1)])
        except CanaryError:
            raise
        except Exception as exc:  # noqa: BLE001 — may quote the request (the token): type only
            raise CanaryError(f'the request could not be sent ({type(exc).__name__})') from None


def probe(udf, url, auth, post=post_decode):
    """One fixture: build the request, ask the service, prove the answer against the ciphertext."""
    inf, mkb, samples = disc_inputs(udf)
    if len(samples) < MIN_SAMPLE_UNITS:
        raise CanaryError(f'only {len(samples)} encrypted sample units (the client needs {MIN_SAMPLE_UNITS})')
    b64 = lambda b: base64.b64encode(b).decode()
    reply = post(url, auth, {'inf_b64': b64(inf), 'mkb_b64': b64(mkb), 'units_b64': [b64(s) for s in samples]})
    keys = answer_keys(reply, inf)
    proven, scrambled = prove(samples, keys)
    if not proven:
        raise CanaryError(f'the key service answered, but its key does not decrypt the fixture '
                          f'({scrambled} scrambled samples, {len(keys)} keys)')
    return {'samples': len(samples), 'scrambled': scrambled, 'keys': len(keys)}


def run(policy, pins, url, auth, open_image, post=post_decode, log=print):
    """{'ok', 'probes': [...]} for every policy probe. Any failure is recorded, never raised."""
    result = {'ok': True, 'probes': []}
    for spec in policy['canary'].get('probes', []):
        fixture = spec['fixture']
        entry = {'fixture': fixture, 'ok': False}
        try:
            if not url or not auth:
                raise CanaryError('FMKV_KEY_URL / FMKV_KEY_AUTH are not set')
            pin = pins.get(fixture) if isinstance(pins, dict) else None
            if not isinstance(pin, dict) or not pin.get('etag'):
                raise CanaryError('the fixture pin is unavailable (plan-media could not read S3)')
            entry.update(probe(Udf(open_image(fixture, pin)), url, auth, post), ok=True, reason='known key returned')
        except CanaryError as exc:
            entry['reason'] = str(exc)
        except Exception as exc:  # noqa: BLE001 — its text may quote the token or the URL: type only
            entry['reason'] = f'unexpected {type(exc).__name__}'
        result['probes'].append(entry)
        result['ok'] = result['ok'] and entry['ok']
        log(f'canary {fixture}: {"PASS" if entry["ok"] else "FAIL"}: {entry["reason"]}')
    return result


class S3Image:
    """Ranged reads of one pinned S3 object, cached in 256 KiB blocks for the file-system walk."""
    BLOCK = 256 << 10

    def __init__(self, s3, bucket, key, pin):
        self.s3, self.bucket, self.key, self.pin, self.cache = s3, bucket, key, pin, {}

    def _get(self, lo, hi):
        import media_fetch
        args = {'Bucket': self.bucket, 'Key': self.key, 'Range': f'bytes={lo}-{hi}'}
        if self.pin.get('version_id'):
            args['VersionId'] = self.pin['version_id']
        else:
            args['IfMatch'] = self.pin['etag']
        try:
            body = self.s3.get_object(**args)['Body'].read()
        except Exception as exc:  # noqa: BLE001 — classified
            code, status = media_fetch.error_code(exc)
            if code == 'PreconditionFailed' or status == 412:
                raise CanaryError(f'{self.key} changed in S3 since the plan pinned it') from None
            raise CanaryError(f'{self.key}: S3 read failed ({code or status or type(exc).__name__})') from None
        if len(body) != hi - lo + 1:
            raise CanaryError(f'{self.key}: short read at {lo}')
        return body

    def read(self, off, n):
        if n <= 0:
            return b''
        if off + n > self.pin['size']:
            raise CanaryError(f'{self.key}: read past the end of the image')
        if n > self.BLOCK:
            return self._get(off, off + n - 1)
        out = bytearray()
        for b in range(off // self.BLOCK, (off + n - 1) // self.BLOCK + 1):
            if b not in self.cache:
                lo = b * self.BLOCK
                self.cache[b] = self._get(lo, min(lo + self.BLOCK, self.pin['size']) - 1)
            out += self.cache[b]
        start = off - (off // self.BLOCK) * self.BLOCK
        return bytes(out[start:start + n])


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    p.add_argument('--bucket', required=True)
    p.add_argument('--externals', type=Path, required=True)
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--policy', type=Path, default=POLICY)
    p.add_argument('--region', default=os.environ.get('AWS_REGION', 'us-west-2'))
    args = p.parse_args(argv)
    try:
        policy = json.loads(args.policy.read_text())
        pins = json.loads(args.externals.read_text()).get('fixtures', {})
        sys.path.insert(0, str(Path(__file__).parent))
        import media_fetch
        s3 = media_fetch.client(args.region, 4)
        result = run(policy, pins, os.environ.get('FMKV_KEY_URL', ''), os.environ.get('FMKV_KEY_AUTH', ''),
                     lambda key, pin: S3Image(s3, args.bucket, key, pin).read)
    except Exception as exc:  # noqa: BLE001 — still write a (failed) result for the plan; type only
        result = {'ok': False, 'probes': [], 'error': f'unexpected {type(exc).__name__}'}
        print(f'::warning title=Key-service canary::{result["error"]}')
    args.out.write_text(json.dumps(result, indent=1, sort_keys=True) + '\n')
    return 0


if __name__ == '__main__':
    sys.exit(main())
