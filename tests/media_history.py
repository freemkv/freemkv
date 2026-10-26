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


def build_baseline(workspace, spec, target):
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
    if not gate.valid_revisions(spec['revisions']):
        raise ValueError('baseline must pin all six release revisions')
    plan = json.loads((args.plan_directory / 'evidence.json').read_text())
    identity = {'schema': 1, 'run_id': os.environ.get('GITHUB_RUN_ID'),
                'hash_method': 'framecrc-copyts-stream-and-chapter-metadata-v2',
                'run_attempt': os.environ.get('GITHUB_RUN_ATTEMPT'), 'target': args.target,
                'fixtures': plan['inputs']['externals']['fixtures'],
                'harness': plan['inputs']['policy']['harness'],
                'rustc': subprocess.check_output(['rustc', '-Vv'], text=True),
                'ffmpeg': subprocess.check_output(['ffmpeg', '-version'], text=True).splitlines()[0]}
    baseline_bin = build_baseline(args.workspace.resolve(), spec, args.target)
    baseline = run_suite(baseline_bin, args.harness, args.out / spec['version'],
                         dict(identity, version=spec['version'], revisions=spec['revisions'],
                              lock_sha256=sha256(args.workspace / 'freemkv/Cargo.lock')))
    (args.out / spec['version'] / 'Cargo.lock').write_bytes(
        (args.workspace / 'freemkv/Cargo.lock').read_bytes())
    version = subprocess.check_output([str(args.binary.resolve()), 'version'], text=True).strip()
    candidate = run_suite(args.binary, args.harness, args.out / 'candidate',
                          dict(identity, version=version, revisions=plan['revisions'],
                               lock_sha256=plan['lock_sha256']))
    (args.out / 'candidate' / 'Cargo.lock').write_bytes((args.plan_directory / 'Cargo.lock').read_bytes())
    if os.environ.get('FMKV_HASHES'):
        Path(os.environ['FMKV_HASHES']).write_bytes((args.out / 'candidate/raw-hashes.txt').read_bytes())
    comparison = compare(baseline['artifacts'], candidate['artifacts'], spec['expected_differences'])
    save(args.out / 'comparison.json', comparison)
    print(f"Release comparison: {len(comparison['differences'])} changed outputs; "
          f"{len(comparison['container_only'])} container-only changes; "
          f"{len(comparison['unexpected'])} unexplained changes")
    for name in comparison['unexpected']:
        print(f'UNEXPECTED: {name}')
    # A failed historical suite is saved and reported too; it cannot become a green baseline silently.
    return int(bool(baseline['suite_exit'] or candidate['suite_exit'] or comparison['unexpected']
                    or comparison['stale_exceptions']))


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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=Path)
    parser.add_argument('--baseline', type=Path, default=Path(__file__).with_name('media-baseline.json'))
    parser.add_argument('--binary', type=Path)
    parser.add_argument('--harness', type=Path)
    parser.add_argument('--plan-directory', type=Path)
    parser.add_argument('--workspace', type=Path, default=Path('historical-build'))
    parser.add_argument('--out', type=Path, default=Path('release-comparison'))
    parser.add_argument('--target')
    args = parser.parse_args()
    if args.archive:
        archive(args.archive, os.environ['GITHUB_RUN_ID'], os.environ['GITHUB_RUN_ATTEMPT'])
        return 0
    return run(args)


if __name__ == '__main__':
    raise SystemExit(main())
