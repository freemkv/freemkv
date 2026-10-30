"""QA gate for the real AACS full-disc ISO and folder outputs.

Run on the candidate only. The 1.8.4 baseline intentionally has different
protection-file behavior, so its existing title-output comparison remains
separate from this candidate's full-disc contract.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

from media_history import content_hash


def redact(text, *secrets):
    """Hide the key values and any URL before text reaches the published report."""
    for secret in sorted(filter(None, secrets), key=len, reverse=True):
        text = text.replace(secret, '<redacted>')
    return re.sub(r'https?://[^\s\'"<>)]+', '<redacted>', text)


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


# hashlib releases the GIL, so files hash in parallel; largest first keeps the pool busy.
HASH_WORKERS = min(8, os.cpu_count() or 1)


def manifest(root):
    print(f'HASH {root}', flush=True)
    files = sorted(path for path in root.rglob('*') if path.is_file())
    jobs = sorted(files, key=lambda path: path.stat().st_size, reverse=True)
    with ThreadPoolExecutor(HASH_WORKERS) as pool:
        digests = dict(zip(jobs, pool.map(sha256, jobs)))
    return {path.relative_to(root).as_posix(): {'bytes': path.stat().st_size, 'sha256': digests[path]}
            for path in files}


def assert_disc_tree(files):
    names = set(files)
    # The AACS folder is renamed SCAA, its files kept as the disc has them.
    if 'SCAA/Unit_Key_RO.inf' not in names or any(name.startswith('AACS/') for name in names):
        raise RuntimeError('decrypted output must rename the AACS folder to SCAA, '
                           'keeping SCAA/Unit_Key_RO.inf, and have no AACS folder')
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


def check_3d_folder_limit(binary, folder, destination, log, title, empty_keydb):
    """The synthetic UDF reader must refuse SSIF aliases until it can model them."""
    source = url('dir', folder)
    target = url('mkv', destination)
    print(f'CHECK known 3D folder limit {source}', flush=True)
    with log.open('w') as output:
        result = subprocess.run([str(binary), source, target, '-t', str(title),
                                 '--keydb', str(empty_keydb)], stdout=output,
                                stderr=subprocess.STDOUT)
    answer = log.read_text(errors='replace')
    if result.returncode == 0 or 'E9061' not in answer or destination.exists():
        raise RuntimeError('3D folder did not refuse unsupported SSIF extents cleanly: '
                           + answer[-2000:])


def check_family(binary, family, source, key_args, output, bd_info=None):
    if not source.is_file():
        raise RuntimeError(f'{family} fixture missing: {source}')
    # The suite already fetched and authenticated the source. Fail explicitly
    # Hold only the ISO and one extracted folder at a time. The runner already
    # carries all four source fixtures and has no room for a third full copy.
    required = source.stat().st_size * 2 + 6 * 1024**3
    free = shutil.disk_usage(output).free
    if free < required:
        raise RuntimeError(f'{family} full-disc gate needs {required} free bytes; runner has {free}')
    with tempfile.TemporaryDirectory(prefix=f'freemkv-{family}-disc-', dir=output) as scratch:
        root = Path(scratch)
        decrypted_iso = root / 'decrypted.iso'
        direct_folder = root / 'direct-folder'
        iso_folder = root / 'iso-folder'
        # Both copies coexist anyway, so they run concurrently at the same peak disk use.
        with ThreadPoolExecutor(2) as pool:
            copies = [pool.submit(run, binary, url('iso', source), url('iso', decrypted_iso),
                                  root / 'decrypted-iso.log', *key_args),
                      pool.submit(run, binary, url('iso', source), url('dir', direct_folder),
                                  root / 'direct-folder.log', *key_args)]
            for copy in copies:
                copy.result()
        if decrypted_iso.stat().st_size != source.stat().st_size:
            raise RuntimeError(f'{family} decrypted ISO has a different size from its source')
        empty_keydb = root / 'empty-keydb.cfg'
        empty_keydb.write_text('')
        direct = manifest(direct_folder)
        navigation = check_navigation(direct_folder, direct)
        external = check_libbluray(bd_info, direct_folder, root / 'bd_info.log') if bd_info else None

        title = smallest_title(binary, url('iso', source))
        title_hashes = {}
        ssif = any(name.startswith('BDMV/STREAM/SSIF/') for name in direct)
        for label, input_url, options in (
                ('source', url('iso', source), key_args),
                ('decrypted_iso', url('iso', decrypted_iso), ['--keydb', str(empty_keydb)]),
                ('folder', url('dir', direct_folder), ['--keydb', str(empty_keydb)])):
            mkv = root / f'{label}.mkv'
            if label == 'folder' and ssif:
                check_3d_folder_limit(binary, direct_folder, mkv, root / 'folder.log',
                                      title, empty_keydb)
                continue
            run(binary, input_url, url('mkv', mkv), root / f'{label}.log', '-t', str(title), *options)
            title_hashes[label] = {'bytes': mkv.stat().st_size, 'sha256': sha256(mkv),
                                   'content_timing_sha256': content_hash(mkv)}
            mkv.unlink()
        if len({value['content_timing_sha256'] for value in title_hashes.values()}) != 1:
            raise RuntimeError(f'{family} title {title} differs between source, decrypted ISO and folder')

        shutil.rmtree(direct_folder)
        # The extract reads the ISO anyway; hashing it alongside shares that read.
        with ThreadPoolExecutor(1) as pool:
            iso_digest = pool.submit(sha256, decrypted_iso)
            run(binary, url('iso', decrypted_iso), url('dir', iso_folder), root / 'iso-folder.log',
                '--keydb', str(empty_keydb))
            decrypted_iso_sha256 = iso_digest.result()
        from_iso = manifest(iso_folder)
        if direct != from_iso:
            difference = [
                name for name in sorted(direct.keys() | from_iso.keys()) if direct.get(name) != from_iso.get(name)]
            raise RuntimeError(f'{family} decrypted ISO/folder differ in {len(difference)} files: {difference[:20]}')
        check_navigation(iso_folder, from_iso)
        return {'fixture_bytes': source.stat().st_size, 'decrypted_iso_sha256': decrypted_iso_sha256,
                'files': direct, 'file_count': len(direct), 'navigation_counts': navigation,
                'title': title, 'title_hashes': title_hashes, 'folder_title_status':
                '3d-ssif-unsupported-E9061' if ssif else 'matched', 'libbluray': external}


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
                  f"valid navigation, title {report['families'][name]['title']} "
                  f"folder status {report['families'][name]['folder_title_status']}",
                  flush=True)
    except Exception as error:
        report['error'] = redact(str(error), args.key_url, args.key_auth)
        raise
    finally:
        (args.out / 'full-disc-qa.json').write_text(json.dumps(report, indent=2, sort_keys=True) + '\n')


if __name__ == '__main__':
    main()
