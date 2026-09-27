#!/usr/bin/env python3
"""Fetch the media fixtures from S3: all keys at once, pinned, and verified end to end.

  media_fetch.py resolve --bucket B --out fixtures.json [--keys k ...]
      HEAD every key and record {etag, version_id, size}: the pin a run fetches by.
  media_fetch.py fetch --bucket B --manifest fixtures.json --dest DIR [--report r.json]
      Download every key concurrently in ranged parts. Each GET is pinned by VersionId,
      or by If-Match on the ETag when the bucket is unversioned, so an object replaced
      mid-run fails the fetch instead of mixing versions. The parts follow the upload's
      part boundaries, so the object's S3 ETag is recomputed from the bytes as they
      arrive and compared with the pin — no second read of 121 GiB from disk.

Needs boto3 (tests/media-fetch-requirements.txt, hash-pinned).
"""

import argparse
import concurrent.futures
import hashlib
import json
import math
import os
from pathlib import Path
import sys
import threading
import time

MIB = 1 << 20
DEFAULT_KEYS = ('dvd.iso', 'bd.iso', 'uhd.iso', 'hddvd.iso', 'keydb.cfg')


def client(region, workers):
    import boto3
    from botocore.config import Config
    return boto3.client('s3', region_name=region, config=Config(
        max_pool_connections=workers + 8, retries={'max_attempts': 10, 'mode': 'adaptive'},
        read_timeout=120, connect_timeout=20))


def resolve(s3, bucket, keys):
    versioned = s3.get_bucket_versioning(Bucket=bucket).get('Status') == 'Enabled'
    out = {}
    for key in keys:
        head = s3.head_object(Bucket=bucket, Key=key)
        out[key] = {'etag': head['ETag'], 'size': head['ContentLength'],
                    'version_id': head.get('VersionId') if versioned else None}
    return out


def part_layout(etag, size):
    """(part size, part count) matching the upload, or None when it cannot be derived."""
    tag = etag.strip('"')
    if '-' not in tag:
        return (max(size, 1), 1)
    count = int(tag.split('-', 1)[1])
    part = math.ceil(math.ceil(size / count) / MIB) * MIB
    if part and math.ceil(size / part) == count:
        return (part, count)
    return None


def etag_of(digests):
    if len(digests) == 1 and not digests[0][1]:
        return '"' + digests[0][0].hex() + '"'
    return '"' + hashlib.md5(b''.join(d for d, _ in digests)).hexdigest() + f'-{len(digests)}"'


class Fetch:
    def __init__(self, s3, bucket, key, pin, dest, chunk):
        self.s3, self.bucket, self.key, self.pin = s3, bucket, key, pin
        self.path = Path(dest) / key
        self.size = pin['size']
        layout = part_layout(pin['etag'], self.size)
        self.verify = layout is not None
        self.part = layout[0] if layout else chunk
        self.count = layout[1] if layout else max(1, math.ceil(self.size / chunk))
        self.multipart = '-' in pin['etag']
        self.digests = [None] * self.count
        self.local = threading.local()
        self.start = self.end = None
        self.done = 0
        self.lock = threading.Lock()
        self.handles = []

    def prepare(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with open(self.path, 'wb') as f:
            f.truncate(self.size)

    def handle(self):
        f = getattr(self.local, 'f', None)
        if f is None:
            f = self.local.f = open(self.path, 'r+b')
            with self.lock:
                self.handles.append(f)
        return f

    def get(self, i):
        lo = i * self.part
        hi = min(self.size, lo + self.part) - 1
        args = {'Bucket': self.bucket, 'Key': self.key}
        if self.size:
            args['Range'] = f'bytes={lo}-{hi}'
        if self.pin.get('version_id'):
            args['VersionId'] = self.pin['version_id']
        else:
            args['IfMatch'] = self.pin['etag']
        for attempt in range(4):
            try:
                body = self.s3.get_object(**args)['Body'].read()
                if len(body) != hi - lo + 1 and self.size:
                    raise IOError(f'{self.key} part {i}: got {len(body)} bytes, want {hi - lo + 1}')
                break
            except Exception as exc:  # noqa: BLE001 — retried below, then raised
                if 'PreconditionFailed' in str(exc) or '412' in str(exc) or attempt == 3:
                    raise
                time.sleep(2 ** attempt)
        f = self.handle()
        f.seek(lo)
        f.write(body)
        self.digests[i] = (hashlib.md5(body).digest(), self.multipart)
        with self.lock:
            self.done += 1
            if self.done == self.count:
                self.end = time.monotonic()

    def close(self):
        for f in self.handles:
            f.close()
        self.handles = []

    def check(self):
        if self.path.stat().st_size != self.size:
            raise IOError(f'{self.key}: {self.path.stat().st_size} bytes on disk, want {self.size}')
        if self.verify:
            got = etag_of(self.digests)
            if got != self.pin['etag']:
                raise IOError(f'{self.key}: content ETag {got} != pinned {self.pin["etag"]}')
        return self.verify


def fetch(s3, bucket, manifest, dest, workers=96, chunk=64 * MIB, log=print):
    jobs = [Fetch(s3, bucket, key, pin, dest, chunk) for key, pin in manifest.items()]
    for job in jobs:
        job.prepare()
    # Interleave parts round-robin so every key downloads at once.
    tasks = []
    for i in range(max(j.count for j in jobs)):
        tasks += [(j, i) for j in jobs if i < j.count]
    t0 = time.monotonic()
    for job in jobs:
        job.start = t0
    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            futures = [pool.submit(j.get, i) for j, i in tasks]
            for fut in concurrent.futures.as_completed(futures):
                exc = fut.exception()
                if exc:
                    for other in futures:
                        other.cancel()
                    raise exc
    finally:
        for job in jobs:
            job.close()
    wall = time.monotonic() - t0
    report = {'wall_s': round(wall, 1), 'bytes': sum(j.size for j in jobs), 'workers': workers, 'keys': {}}
    for job in jobs:
        verified = job.check()
        secs = (job.end or time.monotonic()) - job.start
        report['keys'][job.key] = {'bytes': job.size, 'seconds': round(secs, 1),
                                   'mib_s': round(job.size / MIB / max(secs, 1e-3), 1),
                                   'etag': job.pin['etag'], 'version_id': job.pin.get('version_id'),
                                   'content_verified': verified}
        if not verified:
            log(f'::warning::{job.key}: upload part size not derivable from its ETag; pinned by '
                f'{"VersionId" if job.pin.get("version_id") else "If-Match"} only')
        log(f'{job.key}: {job.size / (1 << 30):.1f} GiB in {secs:.0f}s ({report["keys"][job.key]["mib_s"]} MiB/s)'
            f'{" content ETag verified" if verified else ""}')
    report['mib_s'] = round(report['bytes'] / MIB / max(wall, 1e-3), 1)
    log(f'total: {report["bytes"] / (1 << 30):.1f} GiB in {wall:.0f}s ({report["mib_s"]} MiB/s)')
    return report


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    p.add_argument('mode', choices=('resolve', 'fetch'))
    p.add_argument('--bucket', required=True)
    p.add_argument('--region', default=os.environ.get('AWS_REGION', 'us-west-2'))
    p.add_argument('--keys', nargs='+', default=list(DEFAULT_KEYS))
    p.add_argument('--out', type=Path)
    p.add_argument('--manifest', type=Path)
    p.add_argument('--dest', type=Path)
    p.add_argument('--report', type=Path)
    p.add_argument('--workers', type=int, default=96)
    args = p.parse_args(argv)
    s3 = client(args.region, args.workers)
    if args.mode == 'resolve':
        manifest = resolve(s3, args.bucket, args.keys)
        text = json.dumps(manifest, indent=1, sort_keys=True) + '\n'
        if args.out:
            args.out.write_text(text)
        print(text, end='')
        return 0
    manifest = json.loads(args.manifest.read_text())
    try:
        report = fetch(s3, args.bucket, manifest, args.dest, args.workers)
    except Exception as exc:  # noqa: BLE001 — one clear error line for the job log
        print(f'::error title=Fixture fetch failed::{exc}')
        return 1
    if args.report:
        args.report.write_text(json.dumps(report, indent=1, sort_keys=True) + '\n')
    return 0


if __name__ == '__main__':
    sys.exit(main())
