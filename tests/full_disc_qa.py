"""QA gate for the real AACS full-disc ISO and folder outputs.

Run on the candidate only. The 1.8.4 baseline intentionally has different
protection-file behavior, so its existing title-output comparison remains
separate from this candidate's full-disc contract.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

from media_history import content_hash


def sha256(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def url(scheme, path):
    # Native Windows accepts C:/ paths inside CLI URLs; C:\\ does not.
    return f'{scheme}://{path.as_posix()}'


def run(binary, source, destination, log, *options):
    print(f'CHECK {source} -> {destination}', flush=True)
    command = [str(binary), source, destination, *options]
    with log.open('w') as output:
        result = subprocess.run(command, stdout=output, stderr=subprocess.STDOUT)
    if result.returncode:
        tail = '\n'.join(log.read_text(errors='replace').splitlines()[-25:])
        raise RuntimeError(f'{source} -> {destination} exited {result.returncode}:\n{tail}')


def smallest_title(binary, source):
    result = subprocess.run([str(binary), 'info', source, '--full'],
                            capture_output=True, text=True, check=True)
    rows = []
    for line in result.stdout.splitlines():
        match = re.match(r'^\s+(\d+)\.\s+\S+\s+(\d+)h\s+(\d+)m\s+([0-9.]+)\s+GB', line)
        if match and int(match[2]) * 60 + int(match[3]) >= 1:
            rows.append((float(match[4]), int(match[1])))
    if not rows:
        raise RuntimeError(f'no title of at least one minute in {source}')
    return min(rows)[1]


def manifest(root):
    answer = {}
    for path in sorted(root.rglob('*')):
        if path.is_file():
            answer[path.relative_to(root).as_posix()] = {
                'bytes': path.stat().st_size, 'sha256': sha256(path),
            }
    return answer


def assert_disc_tree(files):
    names = set(files)
    if 'AACS/Unit_Key_RO.bak' not in names or 'AACS/Unit_Key_RO.inf' in names:
        raise RuntimeError('decrypted output must retain AACS/Unit_Key_RO.bak and omit .inf')
    if not any(name.startswith('AACS/') for name in names):
        raise RuntimeError('decrypted output lost the AACS directory')
    if 'BDMV/index.bdmv' not in names:
        raise RuntimeError('decrypted output lost BDMV/index.bdmv')
    navigation = []
    for directory, suffix, signature in (('BDMV/PLAYLIST/', '.mpls', b'MPLS'),
                                         ('BDMV/CLIPINF/', '.clpi', b'HDMV')):
        found = [name for name in names if name.startswith(directory) and name.lower().endswith(suffix)]
        if not found:
            raise RuntimeError(f'decrypted output has no {suffix} navigation files')
        navigation.append((directory, found, signature))
    return navigation


def check_navigation(root, files):
    counts = {}
    for directory, found, signature in assert_disc_tree(files):
        for name in found:
            with (root / name).open('rb') as stream:
                if stream.read(len(signature)) != signature:
                    raise RuntimeError(f'navigation header corrupt: {name}')
        counts[directory] = len(found)
    return counts


def check_libbluray(executable, folder, log):
    with log.open('w') as output:
        result = subprocess.run([str(executable), str(folder)], stdout=output,
                                stderr=subprocess.STDOUT)
    answer = log.read_text(errors='replace')
    if result.returncode or not re.search(r'BluRay detected\s*:\s*yes', answer):
        raise RuntimeError(f'libbluray could not open the decrypted folder: {answer[-2000:]}')
    if not re.search(r'AACS detected\s*:\s*no', answer):
        raise RuntimeError(f'libbluray still sees the decrypted folder as AACS protected: {answer[-2000:]}')
    titles = re.findall(r'(?:HDMV|BD-J) titles\s*:\s*(\d+)', answer)
    if not titles or sum(map(int, titles)) == 0:
        raise RuntimeError('libbluray found no titles in the decrypted folder')
    return {'title_count': sum(map(int, titles)), 'aacs_detected': False}


def check_family(binary, family, source, key_args, output, bd_info=None):
    if not source.is_file():
        raise RuntimeError(f'{family} fixture missing: {source}')
    # The suite already fetched and authenticated the source. Fail explicitly
    # if the runner lacks room for both user-visible outputs and their comparison.
    required = source.stat().st_size * 3 + 2 * 1024**3
    free = shutil.disk_usage(output).free
    if free < required:
        raise RuntimeError(f'{family} full-disc gate needs {required} free bytes; runner has {free}')
    with tempfile.TemporaryDirectory(prefix=f'freemkv-{family}-disc-', dir=output) as scratch:
        root = Path(scratch)
        decrypted_iso = root / 'decrypted.iso'
        direct_folder = root / 'direct-folder'
        iso_folder = root / 'iso-folder'
        run(binary, url('iso', source), url('iso', decrypted_iso), root / 'decrypted-iso.log', *key_args)
        if decrypted_iso.stat().st_size != source.stat().st_size:
            raise RuntimeError(f'{family} decrypted ISO has a different size from its source')
        run(binary, url('iso', source), url('dir', direct_folder), root / 'direct-folder.log', *key_args)
        empty_keydb = root / 'empty-keydb.cfg'
        empty_keydb.write_text('')
        run(binary, url('iso', decrypted_iso), url('dir', iso_folder), root / 'iso-folder.log',
            '--keydb', str(empty_keydb))

        direct = manifest(direct_folder)
        from_iso = manifest(iso_folder)
        if direct != from_iso:
            difference = [
                name for name in sorted(direct.keys() | from_iso.keys()) if direct.get(name) != from_iso.get(name)]
            raise RuntimeError(f'{family} decrypted ISO/folder differ in {len(difference)} files: {difference[:20]}')
        navigation = check_navigation(direct_folder, direct)
        check_navigation(iso_folder, from_iso)
        external = check_libbluray(bd_info, direct_folder, root / 'bd_info.log') if bd_info else None

        title = smallest_title(binary, url('iso', source))
        title_hashes = {}
        for label, input_url, options in (
                ('source', url('iso', source), key_args),
                ('decrypted_iso', url('iso', decrypted_iso), ['--keydb', str(empty_keydb)]),
                ('folder', url('dir', direct_folder), ['--keydb', str(empty_keydb)])):
            mkv = root / f'{label}.mkv'
            run(binary, input_url, url('mkv', mkv), root / f'{label}.log', '-t', str(title), *options)
            title_hashes[label] = {'bytes': mkv.stat().st_size, 'sha256': sha256(mkv),
                                   'content_timing_sha256': content_hash(mkv)}
        if len({value['content_timing_sha256'] for value in title_hashes.values()}) != 1:
            raise RuntimeError(f'{family} title {title} differs between source, decrypted ISO and folder')
        return {'fixture_bytes': source.stat().st_size, 'decrypted_iso_sha256': sha256(decrypted_iso),
                'files': direct, 'file_count': len(direct), 'navigation_counts': navigation,
                'title': title, 'title_hashes': title_hashes, 'libbluray': external}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--bd-iso', type=Path, required=True)
    parser.add_argument('--uhd-iso', type=Path, required=True)
    parser.add_argument('--keydb', type=Path, required=True)
    parser.add_argument('--key-url', required=True)
    parser.add_argument('--key-auth', required=True)
    parser.add_argument('--bd-info', type=Path)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    key_args = ['--keydb', str(args.keydb), '--key-url', args.key_url,
                '--key-auth', args.key_auth]
    report = {'schema': 1, 'families': {}}
    try:
        for name, source in (('bd', args.bd_iso), ('uhd', args.uhd_iso)):
            report['families'][name] = check_family(args.binary.resolve(), name, source.resolve(),
                                                    key_args, args.out.resolve(), args.bd_info)
            print(f"PASS {name}: {report['families'][name]['file_count']} matching files, "
                  f"valid navigation, title {report['families'][name]['title']} matches source keylessly",
                  flush=True)
    except Exception as error:
        report['error'] = str(error)
        raise
    finally:
        (args.out / 'full-disc-qa.json').write_text(json.dumps(report, indent=2, sort_keys=True) + '\n')


if __name__ == '__main__':
    main()
