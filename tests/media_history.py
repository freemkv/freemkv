"""Retain release comparisons independently of expiring Actions artifacts."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
from concurrent.futures import ThreadPoolExecutor

import media_gate as gate


MEDIA = {'.mkv', '.m2ts', '.mp4', '.ts', '.mpg', '.vob', '.evo'}
METADATA = {'.mpls', '.clpi', '.ifo', '.bup', '.inf', '.bak', '.bdmv', '.bdjo',
            '.jar', '.png', '.jpg', '.jpeg', '.cer', '.tbl', '.aacs'}


def sha256(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def content_hash(path):
    # Stream packet content AND timestamps; build stamps in container headers are excluded.
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
        result = subprocess.run(['ffmpeg', '-nostdin', '-v', 'error', '-copyts', '-i', str(path),
                                 '-map', '0', '-c', 'copy', '-f', 'framecrc', '-'],
                                stdout=output, stderr=errors)
        output.seek(0)
        if result.returncode or not any(line.strip() and not line.startswith(b'#') for line in output):
            raise ValueError(f'cannot hash media content: {path.name}')
        output.seek(0)
        digest = hashlib.file_digest(output, 'sha256')
        metadata = subprocess.run(['ffprobe', '-v', 'error', '-show_entries',
                                   'stream=index,codec_name,codec_type,width,height,pix_fmt,sample_rate,'
                                   'channels,channel_layout:stream_tags=language,title:stream_disposition:'
                                   'chapter=time_base,start,end:chapter_tags=title,language',
                                   '-of', 'json', str(path)], capture_output=True)
        if metadata.returncode:
            raise ValueError(f'cannot hash stream metadata: {path.name}')
        digest.update(b'\0streams-and-chapters\0')
        digest.update(json.dumps(json.loads(metadata.stdout), sort_keys=True).encode())
        return digest.hexdigest()


def snapshot(root):
    paths = [path for path in sorted(root.rglob('*'))
             if path.is_file() and path.suffix.lower() in MEDIA | METADATA]

    def inspect(path):
        answer = {'bytes': path.stat().st_size}
        if path.suffix.lower() not in MEDIA:
            answer['sha256'] = sha256(path)
            return path.relative_to(root).as_posix(), answer
        # Both hashes read the file side by side, so the second reader is served from the page
        # cache: one pass over the disk instead of two. The values are unchanged.
        with ThreadPoolExecutor(max_workers=1) as side:
            content = side.submit(content_hash, path)
            answer['sha256'] = sha256(path)
            try:
                answer['content_timing_sha256'] = content.result()
            except ValueError as error:
                answer['error'] = str(error)
        return path.relative_to(root).as_posix(), answer

    # The hash method stays identical; independent files can share the runner's
    # CPU and disk bandwidth instead of serializing every full-feature scan.
    with ThreadPoolExecutor(max_workers=max(4, min(16, os.cpu_count() or 4))) as pool:
        answers = dict(pool.map(inspect, paths))
    if not answers:
        raise ValueError('the media suite produced no artifacts to compare')
    return answers


def compare(baseline, candidate, expected):
    differences, unexpected, container_only = {}, [], {}
    for name in sorted(baseline.keys() | candidate.keys()):
        old, new = baseline.get(name), candidate.get(name)
        field = 'content_timing_sha256' if Path(name).suffix.lower() in MEDIA else 'sha256'
        if old and new and field in old and field in new and old[field] == new[field]:
            if old['sha256'] != new['sha256']:
                container_only[name] = {'baseline': old['sha256'], 'candidate': new['sha256']}
            continue
        if old and new and old.get('error') and new.get('error') and old['sha256'] == new['sha256']:
            continue
        differences[name] = {'baseline': old, 'candidate': new}
        approval = expected.get(name, {})
        # An exception binds BOTH exact answers and a reason; a filename wildcard cannot bless drift.
        old_hash = old.get(field) if old else None
        new_hash = new.get(field) if new else None
        if not (approval.get('reason') and (old is None or old_hash) and (new is None or new_hash)
                and 'baseline' in approval and 'candidate' in approval
                and approval['baseline'] == old_hash and approval['candidate'] == new_hash):
            unexpected.append(name)
    stale = sorted(set(expected) - differences.keys())
    return {'differences': differences, 'container_only': container_only,
            'unexpected': unexpected, 'stale_exceptions': stale}


def save(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + '\n')


def download_manifest(bucket, key, destination):
    # The fixture-fetch venv has pinned boto3 on both EC2 operating systems.
    import boto3
    boto3.client('s3', region_name='us-west-2').download_file(bucket, key, str(destination))


def build_baseline(workspace, spec, target):
    """Build the exact released source set, for explicit history-epoch recording only."""
    workspace.mkdir(parents=True, exist_ok=True)
    app = workspace / 'freemkv'
    subprocess.run(['git', 'init', '-q', str(app)], check=True)
    subprocess.run(['git', '-C', str(app), 'fetch', '-q', '--depth=1',
                    'https://github.com/freemkv/freemkv', spec['revisions']['freemkv']], check=True)
    subprocess.run(['git', '-C', str(app), 'checkout', '-q', '--detach', 'FETCH_HEAD'], check=True)
    gate.checkout(workspace, spec['revisions'])
    gate.resolve(workspace)
    env = dict(os.environ, CARGO_TARGET_DIR=str(workspace / 'target'))
    subprocess.run(['cargo', 'build', '--locked', '--release', '--target', target, '--bin', 'freemkv'],
                   cwd=app, env=env, check=True)
    extension = '.exe' if 'windows' in target else ''
    return workspace / 'target' / target / 'release' / f'freemkv{extension}'


def record_baseline(args, spec, plan, identity):
    """Generate a reference only; this mode deliberately makes no candidate assertion."""
    history = spec.get('history', [])
    if len(history) != 1 or history[0].get('version') != '1.8.4':
        raise ValueError('baseline epoch mode is restricted to the single pinned 1.8.4 history entry')
    entry = history[0]
    if not gate.valid_revisions(entry['revisions']):
        raise ValueError('baseline epoch must pin all six released revisions')
    fixtures = plan['inputs']['externals']['fixtures']
    if fixtures.get('error'):
        raise ValueError('baseline epoch requires a successfully fingerprinted fixture set')
    binary = build_baseline(args.workspace.resolve(), entry, args.target)
    version = subprocess.check_output([str(binary.resolve()), 'version'], text=True).strip()
    if version != entry['version']:
        raise ValueError('built historical binary version differs from the pinned release')
    report = run_suite(binary, args.harness, args.out / entry['version'],
                       dict(identity, version=entry['version'], revisions=entry['revisions'],
                            lock_sha256=sha256(args.workspace / 'freemkv/Cargo.lock')))
    (args.out / entry['version']).mkdir(parents=True, exist_ok=True)
    (args.out / entry['version'] / 'Cargo.lock').write_bytes(
        (args.workspace / 'freemkv/Cargo.lock').read_bytes())
    if os.environ.get('FMKV_HASHES'):
        Path(os.environ['FMKV_HASHES']).write_bytes(
            (args.out / entry['version'] / 'raw-hashes.txt').read_bytes())
    return int(bool(report['suite_exit']))


def historical_manifest(bucket, spec, target, identity, destination):
    pin = spec['manifests'][target]
    digest = pin['sha256']
    if len(digest) != 64 or any(c not in '0123456789abcdef' for c in digest):
        raise ValueError(f"invalid historical SHA-256 for {spec['version']} {target}")
    key = f"media-history/v1/{spec['version']}/{target}/{digest}.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    download_manifest(bucket, key, destination)
    if sha256(destination) != digest:
        raise ValueError(f'historical SHA-256 mismatch: {key}')
    report = json.loads(destination.read_text())
    for field in ('schema', 'target', 'fixtures', 'harness', 'hash_method', 'rustc', 'ffmpeg'):
        if report.get(field) != identity[field]:
            raise ValueError(f'historical {field} differs for {spec["version"]} {target}; '
                             'create a new pinned media history epoch before comparing')
    if (report.get('version') != spec['version'] or
            report.get('revisions') != spec['revisions'] or
            report.get('suite_exit') != 0 or not report.get('artifacts')):
        raise ValueError(f'invalid historical result: {key}')
    return report


def run_suite(binary, harness, destination, identity):
    # The existing harness retains its scratch files until hashes have been saved, then we clean them.
    destination = destination.resolve()
    with tempfile.TemporaryDirectory(prefix='media-history-') as scratch:
        env = dict(os.environ, FMKV_BIN=binary.resolve().as_posix(), FMKV_KEEP_SCRATCH='1',
                   TMPDIR=scratch.replace('\\', '/'), FMKV_HASHES=(destination / 'raw-hashes.txt').as_posix())
        destination.mkdir(parents=True, exist_ok=True)
        with (destination / 'suite.log').open('w') as log:
            with subprocess.Popen(['bash', harness.resolve().as_posix()], env=env,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) as result:
                for line in result.stdout:
                    log.write(line)
                    print(line, end='', flush=True)
        roots = list(Path(scratch).glob('freemkv-cli-acceptance.*'))
        if len(roots) != 1:
            raise ValueError(f'expected one suite workspace, found {len(roots)}')
        report = dict(identity, binary_sha256=sha256(binary), suite_exit=result.returncode,
                      artifacts=snapshot(roots[0]))
        save(destination / 'manifest.json', report)
    return report


def run(args):
    spec = json.loads(args.baseline.read_text())
    history = spec['history']
    if not history or len({entry['version'] for entry in history}) != len(history):
        raise ValueError('media history must contain unique released versions')
    if any(not gate.valid_revisions(entry['revisions']) for entry in history):
        raise ValueError('media history must pin all six release revisions')
    plan = json.loads((args.plan_directory / 'evidence.json').read_text())
    identity = {'schema': 1, 'run_id': os.environ.get('GITHUB_RUN_ID'),
                'hash_method': 'framecrc-copyts-stream-and-chapter-metadata-v2',
                'run_attempt': os.environ.get('GITHUB_RUN_ATTEMPT'), 'target': args.target,
                'fixtures': plan['inputs']['externals']['fixtures'],
                'harness': plan['inputs']['policy']['harness'],
                'rustc': subprocess.check_output(['rustc', '-Vv'], text=True),
                'ffmpeg': subprocess.check_output(['ffmpeg', '-version'], text=True).splitlines()[0]}
    if args.record_baseline:
        return record_baseline(args, spec, plan, identity)
    baselines = {entry['version']: historical_manifest(args.bucket, entry, args.target,
                 identity, args.out / entry['version'] / 'manifest.json') for entry in history}
    version = subprocess.check_output([str(args.binary.resolve()), 'version'], text=True).strip()
    def version_parts(value):
        return tuple(int(part) for part in value.split('.'))
    if any(version_parts(entry['version']) >= version_parts(version) for entry in history):
        raise ValueError('media history must contain only earlier released versions')
    candidate = run_suite(args.binary, args.harness, args.out / 'candidate',
                          dict(identity, version=version, revisions=plan['revisions'],
                               lock_sha256=plan['lock_sha256']))
    (args.out / 'candidate' / 'Cargo.lock').write_bytes((args.plan_directory / 'Cargo.lock').read_bytes())
    if os.environ.get('FMKV_HASHES'):
        Path(os.environ['FMKV_HASHES']).write_bytes((args.out / 'candidate/raw-hashes.txt').read_bytes())
    comparisons = {}
    for entry in history:
        old_version = entry['version']
        comparison = compare(baselines[old_version]['artifacts'], candidate['artifacts'],
                             entry['expected_differences'])
        comparisons[old_version] = comparison
        save(args.out / f'comparison-{old_version}.json', comparison)
        print(f"{old_version} → {version}: {len(comparison['differences'])} changed outputs; "
              f"{len(comparison['container_only'])} container-only changes; "
              f"{len(comparison['unexpected'])} unexplained changes")
        for name in comparison['unexpected']:
            print(f'UNEXPECTED against {old_version}: {name}')
    save(args.out / 'comparison.json', {'comparisons': comparisons})
    return int(bool(candidate['suite_exit'] or any(c['unexpected'] or c['stale_exceptions']
                                                  for c in comparisons.values())))


def archive(directory, run_id, attempt, post=gate.gh_post):
    files = sorted(p for p in directory.rglob('*') if p.is_file() and p.suffix in {'.json', '.lock'})
    if not files:
        raise ValueError('no release comparison reports to archive')
    tree = post('repos/freemkv/freemkv/git/trees', {'tree': [
        {'path': p.relative_to(directory).as_posix(), 'mode': '100644', 'type': 'blob',
         'content': p.read_text()} for p in files]})['sha']
    commit = post('repos/freemkv/freemkv/git/commits', {
        'message': f'Release media comparison, run {run_id} attempt {attempt}', 'tree': tree,
        'parents': []})['sha']
    # Create only: a rerun has its own name; history is never force-updated.
    name = f'media-history/{run_id}/{attempt}'
    post('repos/freemkv/freemkv/git/refs', {'ref': f'refs/tags/{name}', 'sha': commit})
    print(f'https://github.com/freemkv/freemkv/tree/{name}')


def publish(directory, bucket):
    """Store the successful Linux QA answer for the next release, without replacing prior runs."""
    approved = []
    for platform in ('linux',):
        path = directory / f'release-comparison-{platform}' / 'candidate' / 'manifest.json'
        report = json.loads(path.read_text())
        if report.get('suite_exit') != 0 or not report.get('artifacts'):
            raise ValueError(f'{platform} has no successful candidate media answer')
        approved.append((path, report))
    for path, report in approved:
        digest = sha256(path)
        key = (f"media-history/v1/candidates/{report['version']}/{report['run_id']}/"
               f"{report['target']}/{digest}.json")
        command = ['aws', 's3api', 'put-object', '--bucket', bucket, '--key', key,
                   '--body', str(path), '--content-type', 'application/json',
                   '--if-none-match', '*', '--region', 'us-west-2']
        result = subprocess.run(command, capture_output=True, text=True)
        if result.returncode:
            # An Actions retry can encounter its own immutable object. Verify it byte for byte.
            with tempfile.TemporaryDirectory() as scratch:
                existing = Path(scratch) / 'manifest.json'
                subprocess.run(['aws', 's3api', 'get-object', '--bucket', bucket, '--key', key,
                                str(existing), '--region', 'us-west-2'], check=True,
                               stdout=subprocess.DEVNULL)
                if sha256(existing) != digest:
                    raise ValueError(f'published answer differs: {key}')
        print(f's3://{bucket}/{key}')


def publish_baseline(directory, bucket, spec_path=Path(__file__).with_name('media-baseline.json'),
                     run_id=None, run_attempt=None, plan_directory=None):
    """Publish only a successful pinned 1.8.4 Linux reference manifest."""
    history = json.loads(spec_path.read_text()).get('history', [])
    if len(history) != 1 or history[0].get('version') != '1.8.4':
        raise ValueError('baseline publication is restricted to the pinned 1.8.4 epoch')
    entry = history[0]
    if not gate.valid_revisions(entry['revisions']):
        raise ValueError('baseline publication requires all six pinned 1.8.4 revisions')
    expected_run_id = str(run_id or os.environ.get('GITHUB_RUN_ID') or '')
    expected_attempt = str(run_attempt or os.environ.get('GITHUB_RUN_ATTEMPT') or '')
    if not expected_run_id:
        raise ValueError('baseline publication requires the current Actions run ID')
    if not expected_attempt:
        raise ValueError('baseline publication requires the current Actions run attempt')
    if plan_directory is None:
        raise ValueError('baseline publication requires the run media plan')
    plan = json.loads((plan_directory / 'evidence.json').read_text())
    expected_fixtures = plan['inputs']['externals']['fixtures']
    expected_harness = plan['inputs']['policy']['harness']
    if plan.get('run_id') != int(expected_run_id) or expected_fixtures.get('error') or 'uhd.iso' not in expected_fixtures:
        raise ValueError('baseline publication requires this run\'s successfully fingerprinted UHD fixture')
    approved = []
    identity_fields = ('schema', 'fixtures', 'harness', 'hash_method', 'rustc', 'ffmpeg')
    for platform, target in (('linux', 'x86_64-unknown-linux-musl'),):
        path = directory / f'release-comparison-{platform}' / '1.8.4' / 'manifest.json'
        if not path.is_file():
            raise ValueError(f'{target} has no reference manifest')
        report = json.loads(path.read_text())
        if (report.get('schema') != 1 or report.get('version') != entry['version'] or
                report.get('target') != target or
                report.get('revisions') != entry['revisions'] or report.get('suite_exit') != 0 or
                not report.get('artifacts') or str(report.get('run_id')) != expected_run_id or
                str(report.get('run_attempt')) != expected_attempt or
                not re.fullmatch(r'[0-9a-f]{64}', report.get('lock_sha256', '')) or
                not re.fullmatch(r'[0-9a-f]{64}', report.get('binary_sha256', '')) or
                report.get('hash_method') != 'framecrc-copyts-stream-and-chapter-metadata-v2'):
            raise ValueError(f'{target} reference manifest is not a successful pinned 1.8.4 answer')
        if report.get('fixtures') != expected_fixtures or report.get('harness') != expected_harness:
            raise ValueError(f'{target} reference identity differs from this run media plan')
        if any(not report.get(field) for field in identity_fields):
            raise ValueError(f'{target} reference manifest has incomplete identity')
        approved.append((path, report))
    for path, report in approved:
        digest = sha256(path)
        key = f"media-history/v1/{report['version']}/{report['target']}/{digest}.json"
        result = subprocess.run(['aws', 's3api', 'put-object', '--bucket', bucket, '--key', key,
                                 '--body', str(path), '--content-type', 'application/json',
                                 '--if-none-match', '*', '--region', 'us-west-2'],
                                capture_output=True, text=True)
        if result.returncode:
            with tempfile.TemporaryDirectory() as scratch:
                existing = Path(scratch) / 'manifest.json'
                subprocess.run(['aws', 's3api', 'get-object', '--bucket', bucket, '--key', key,
                                str(existing), '--region', 'us-west-2'], check=True,
                               stdout=subprocess.DEVNULL)
                if sha256(existing) != digest:
                    raise ValueError(f'published baseline differs: {key}')
        print(f's3://{bucket}/{key}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--publish', type=Path)
    parser.add_argument('--publish-baseline', type=Path)
    parser.add_argument('--record-baseline', action='store_true')
    parser.add_argument('--baseline', type=Path, default=Path(__file__).with_name('media-baseline.json'))
    parser.add_argument('--bucket')
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--harness', type=Path)
    parser.add_argument('--plan-directory', type=Path)
    parser.add_argument('--out', type=Path, default=Path('release-comparison'))
    parser.add_argument('--workspace', type=Path, default=Path('historical-build'))
    parser.add_argument('--target')
    args = parser.parse_args()
    if args.archive:
        archive(args.archive, os.environ['GITHUB_RUN_ID'], os.environ['GITHUB_RUN_ATTEMPT'])
        return 0
    if args.publish:
        publish(args.publish, args.bucket)
        return 0
    if args.publish_baseline:
        publish_baseline(args.publish_baseline, args.bucket, args.baseline,
                         os.environ.get('GITHUB_RUN_ID'), os.environ.get('GITHUB_RUN_ATTEMPT'),
                         args.plan_directory)
        return 0
    return run(args)


if __name__ == '__main__':
    raise SystemExit(main())
