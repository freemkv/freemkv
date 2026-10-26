"""Retain release comparisons independently of expiring Actions artifacts."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

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
    answers = {}
    for path in sorted(root.rglob('*')):
        if not path.is_file() or path.suffix.lower() not in MEDIA | METADATA:
            continue
        answer = {'bytes': path.stat().st_size, 'sha256': sha256(path)}
        if path.suffix.lower() in MEDIA:
            try:
                answer['content_timing_sha256'] = content_hash(path)
            except ValueError as error:
                answer['error'] = str(error)
        answers[path.relative_to(root).as_posix()] = answer
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


def historical_manifest(bucket, spec, target, identity, destination):
    pin = spec['manifests'][target]
    digest = pin['sha256']
    if len(digest) != 64 or any(c not in '0123456789abcdef' for c in digest):
        raise ValueError(f"invalid historical SHA-256 for {spec['version']} {target}")
    key = f"media-history/v1/{spec['version']}/{target}/{digest}.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(['aws', 's3api', 'get-object', '--bucket', bucket, '--key', key,
                    str(destination), '--region', 'us-west-2', '--query', 'ContentLength',
                    '--output', 'text'], check=True)
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
    """Store successful QA answers for the next release, without replacing prior runs."""
    approved = []
    for platform in ('linux', 'windows'):
        root = directory / f'release-comparison-{platform}' / 'candidate'
        path = root / 'manifest.json'
        report = json.loads(path.read_text())
        if report.get('suite_exit') != 0 or not report.get('artifacts'):
            raise ValueError(f'{platform} has no successful candidate media answer')
        full_disc = root / 'full-disc-qa.json'
        if not full_disc.is_file():
            raise ValueError(f'{platform} has no successful full-disc report')
        full_report = json.loads(full_disc.read_text())
        if 'error' in full_report or set(full_report.get('families', {})) != {'bd', 'uhd'}:
            raise ValueError(f'{platform} has no successful full-disc report')
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--publish', type=Path)
    parser.add_argument('--baseline', type=Path, default=Path(__file__).with_name('media-baseline.json'))
    parser.add_argument('--bucket')
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--harness', type=Path)
    parser.add_argument('--plan-directory', type=Path)
    parser.add_argument('--out', type=Path, default=Path('release-comparison'))
    parser.add_argument('--target')
    args = parser.parse_args()
    if args.archive:
        archive(args.archive, os.environ['GITHUB_RUN_ID'], os.environ['GITHUB_RUN_ATTEMPT'])
        return 0
    if args.publish:
        publish(args.publish, args.bucket)
        return 0
    return run(args)


if __name__ == '__main__':
    raise SystemExit(main())
