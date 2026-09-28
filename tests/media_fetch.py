#!/usr/bin/env python3
"""Fetch the media fixtures from S3: all keys at once, pinned, and verified end to end.

  media_fetch.py resolve --bucket B --out fixtures.json [--keys k ...]
      HEAD every key (and its first part) and record {etag, size, version_id,
      part_size, parts}: the pin a run fetches by.
  media_fetch.py fetch --bucket B --manifest fixtures.json --dest DIR [--report r.json]
      Download every key concurrently in ranged parts. Each GET is pinned by VersionId,
      or by If-Match on the ETag when the bucket is unversioned, so an object replaced
      mid-run fails the fetch instead of mixing versions. A multipart object is read on
      its upload's part boundaries, so its S3 ETag is recomputed from the bytes as they
      arrive; a single-part object's MD5 ETag is checked by one read of the file.

Only s3:GetObject is needed (HeadObject and ranged GETs); s3:GetObjectVersion as well
once the bucket is versioned. Needs boto3 (tests/media-fetch-requirements.txt).
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
from threading import local as thread_local
import time

MIB = 1 << 20
CHUNK = 16 * MIB
DEFAULT_KEYS = ('dvd.iso', 'bd.iso', 'uhd.iso', 'hddvd.iso', 'keydb.cfg')


class Precondition(Exception):
    """The object is no longer the pinned one."""


def client(region, workers):
    import boto3
    from botocore.config import Config
    return boto3.client('s3', region_name=region, config=Config(
        max_pool_connections=workers + 8, retries={'max_attempts': 5, 'mode': 'standard'},
        read_timeout=120, connect_timeout=20))


def error_code(exc):
    """The S3 error code and HTTP status of a botocore ClientError (or ('', 0))."""
    response = getattr(exc, 'response', None) or {}
    return (response.get('Error', {}).get('Code', ''),
            int(response.get('ResponseMetadata', {}).get('HTTPStatusCode', 0) or 0))


def resolve(s3, bucket, keys):
    """The pins. HeadObject needs only s3:GetObject; PartNumber=1 gives the exact upload part size."""
    out = {}
    for key in keys:
        head = s3.head_object(Bucket=bucket, Key=key)
        version = head.get('VersionId')
        pin = {'etag': head['ETag'], 'size': head['ContentLength'],
               'version_id': version if version not in (None, 'null') else None}
        if '-' in head['ETag'].strip('"'):
            first = s3.head_object(Bucket=bucket, Key=key, PartNumber=1, IfMatch=head['ETag'])
            pin['part_size'] = first['ContentLength']
            pin['parts'] = first.get('PartsCount')
        out[key] = pin
    return out


def layout(pin):
    """(part size, part count, multipart) for a pin; raises when the pin is inconsistent."""
    tag = pin['etag'].strip('"')
    size = pin['size']
    if '-' not in tag:
        return (CHUNK, max(1, math.ceil(size / CHUNK)), False)
    count = int(tag.rsplit('-', 1)[1]) if tag.rsplit('-', 1)[1].isdigit() else -1
    part = pin.get('part_size')
    if not part or count < 1 or pin.get('parts') not in (None, count) or math.ceil(size / part) != count:
        raise ValueError(f'inconsistent multipart pin {pin} (re-run resolve)')
    return (part, count, True)


def etag_of(digests):
    return '"' + hashlib.md5(b''.join(digests)).hexdigest() + f'-{len(digests)}"'


class Fetch:
    def __init__(self, s3, bucket, key, pin, dest):
        self.s3, self.bucket, self.key, self.pin = s3, bucket, key, pin
        self.path = Path(dest) / key
        self.size = pin['size']
        self.part, self.count, self.multipart = layout(pin)
        self.digests = [None] * self.count
        self.tls = thread_local()
        self.lock = threading.Lock()
        self.handles = []
        self.done = 0
        self.start = self.end = None

    def prepare(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with open(self.path, 'wb') as f:
            f.truncate(self.size)

    def handle(self):
        f = getattr(self.tls, 'f', None)
        if f is None:
            f = self.tls.f = open(self.path, 'r+b')
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
                if self.size and len(body) != hi - lo + 1:
                    raise IOError(f'{self.key} part {i}: got {len(body)} bytes, want {hi - lo + 1}')
                break
            except Exception as exc:  # noqa: BLE001 — classified below
                code, status = error_code(exc)
                if code == 'PreconditionFailed' or status == 412:
                    raise Precondition(f'{self.key} changed in S3 during the fetch (ETag is no longer '
                                       f'{self.pin["etag"]}); re-run to pin the new object') from exc
                if code in ('AccessDenied', 'NoSuchKey', 'NoSuchVersion') or status in (403, 404):
                    hint = ' (a VersionId GET needs s3:GetObjectVersion)' if 'VersionId' in args else ''
                    raise IOError(f'{self.key}: {code or status}{hint}') from exc
                if attempt == 3:
                    raise
                time.sleep(2 ** attempt)
        f = self.handle()
        f.seek(lo)
        f.write(body)
        if self.multipart:
            self.digests[i] = hashlib.md5(body).digest()
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
        if self.multipart:
            got = etag_of(self.digests)
        else:
            md5 = hashlib.md5()
            with open(self.path, 'rb') as f:
                for block in iter(lambda: f.read(CHUNK), b''):
                    md5.update(block)
            got = '"' + md5.hexdigest() + '"'
        if got != self.pin['etag']:
            raise IOError(f'{self.key}: content ETag {got} != pinned {self.pin["etag"]}')


def fetch(s3, bucket, manifest, dest, workers=96, log=print):
    jobs = [Fetch(s3, bucket, key, pin, dest) for key, pin in manifest.items()]
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
        job.check()
        secs = (job.end or time.monotonic()) - job.start
        report['keys'][job.key] = {'bytes': job.size, 'seconds': round(secs, 1),
                                   'mib_s': round(job.size / MIB / max(secs, 1e-3), 1), 'etag': job.pin['etag'],
                                   'version_id': job.pin.get('version_id'), 'content_verified': True}
        log(f'{job.key}: {job.size / (1 << 30):.1f} GiB in {secs:.0f}s '
            f'({report["keys"][job.key]["mib_s"]} MiB/s), ETag verified')
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
    try:
        if args.mode == 'resolve':
            manifest = resolve(s3, args.bucket, args.keys)
            text = json.dumps(manifest, indent=1, sort_keys=True) + '\n'
            if args.out:
                args.out.write_text(text)
            print(text, end='')
            return 0
        manifest = json.loads(args.manifest.read_text())
        report = fetch(s3, args.bucket, manifest, args.dest, args.workers)
    except Exception as exc:  # noqa: BLE001 — one clear error line for the job log
        print(f'::error title=Fixture {args.mode} failed::{" ".join(str(exc).splitlines())} — the BD/UHD '
              'legs cannot decrypt without keydb.cfg and every leg needs all four ISOs')
        return 1
    if args.report:
        args.report.write_text(json.dumps(report, indent=1, sort_keys=True) + '\n')
    return 0


if __name__ == '__main__':
    sys.exit(main())
