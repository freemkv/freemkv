"""Build the amd64 `freemkv` (app) or `freemkv-cli` (static CLI) package on Ubuntu 24.04."""

import argparse
from datetime import datetime, timezone
from email.utils import format_datetime
import gzip
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[2]
MAINTAINER = 'Matthew Jackson <1085847+MattJackson@users.noreply.github.com>'
DEVICE_NOTE = ('Optical drives use the distribution\'s existing device ACLs and group permissions.\n'
               'Use an active local desktop session. If access is denied, inspect the device\n'
               'owner/group and your distribution\'s optical-drive access policy. No device\n'
               'permissions or group memberships are changed by this package.\n')
PACKAGES = {
    'freemkv': {
        'other': 'freemkv-cli',
        'summary': 'Rip DVD, Blu-ray and UHD discs without re-encoding',
        'description': ' Native GTK4 desktop app and command-line interface for selecting titles,\n'
                       ' audio and subtitle tracks and saving them to MKV, MP4 or M2TS.\n'
                       ' Running freemkv without arguments opens the desktop app.\n',
        'readme': 'Built for Ubuntu 24.04 amd64 and compatible derivatives such as Linux Mint 22.\n'
                  'Launch freemkv from the application menu, or run freemkv with no arguments.\n'
                  'Any arguments run the command-line interface.\n\n' + DEVICE_NOTE,
        'usage': 'Run freemkv with no arguments for the desktop app, or freemkv --help for CLI usage.',
    },
    'freemkv-cli': {
        'other': 'freemkv',
        'summary': 'Rip DVD, Blu-ray and UHD discs without re-encoding (command line)',
        'description': ' Static command-line build of freemkv for selecting titles, audio and\n'
                       ' subtitle tracks and saving them to MKV, MP4 or M2TS. It has no desktop\n'
                       ' interface; install the freemkv package for the desktop app.\n',
        'readme': 'Static amd64 command-line build with no library dependencies.\n'
                  'Run freemkv --help for usage. Install the freemkv package for the desktop app.\n\n'
                  + DEVICE_NOTE,
        'usage': 'Run freemkv --help for usage.',
    },
}


def run(*args, cwd=ROOT):
    return subprocess.check_output(args, cwd=cwd).decode().strip()


def version():
    value = tomllib.loads((ROOT / 'Cargo.toml').read_text())['package']['version']
    if not re.fullmatch(r'\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?', value):
        raise ValueError(f'unsupported package version: {value}')
    return value.replace('-', '~', 1)


def dependencies(output):
    values = [line.split('=', 1)[1] for line in output.splitlines()
              if line.startswith('shlibs:Depends=')]
    if len(values) != 1 or not values[0].strip():
        raise ValueError('dpkg-shlibdeps returned no dependency set')
    if not all(name in values[0] for name in ('libgtk-4-1', 'libadwaita-1-0', 'libc6')):
        raise ValueError('binary does not link the expected Linux GUI libraries')
    return values[0] + ', ca-certificates'


def static_binary(program_headers):
    """True when `readelf -lW` output has no interpreter, i.e. nothing to load at runtime."""
    if 'Program Headers:' not in program_headers:
        raise ValueError('readelf returned no program headers')
    return not re.search(r'^\s*INTERP\b', program_headers, re.MULTILINE)


def write(root, path, text, mode=0o644):
    dest = root / path
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(text)
    dest.chmod(mode)
    return dest


def copy(root, source, path, mode=0o644):
    dest = root / path
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, dest)
    dest.chmod(mode)
    return dest


def stage(binary, root, package_version, depends, epoch, notices, package='freemkv'):
    meta = PACKAGES[package]
    doc = f'usr/share/doc/{package}'
    copy(root, binary, 'usr/bin/freemkv', 0o755)
    if package == 'freemkv':
        stage_desktop(root, package_version, epoch)
    else:
        write(root, 'usr/share/lintian/overrides/freemkv-cli',
              'freemkv-cli: statically-linked-binary [usr/bin/freemkv]\n')
    copyright_text = ((ROOT / 'LICENSE').read_text()
                      + '\nUpstream: https://github.com/freemkv/freemkv\n'
                      + f'Dependency licenses and notices: /{doc}/third-party-notices.gz\n')
    write(root, f'{doc}/copyright', copyright_text)
    (root / doc / 'third-party-notices.gz').write_bytes(gzip.compress(notices.encode(), mtime=0))
    write(root, f'{doc}/README.Debian', meta['readme'])
    changelog = (f'{package} ({package_version}) unstable; urgency=medium\n\n'
                 '  * Package the upstream release for Ubuntu 24.04 amd64.\n\n'
                 f' -- {MAINTAINER}  '
                 + format_datetime(datetime.fromtimestamp(epoch, timezone.utc)) + '\n')
    for filename, content in [('changelog.Debian.gz', changelog),
                               ('changelog.gz', (ROOT / 'CHANGELOG.md').read_text())]:
        (root / doc / filename).write_bytes(gzip.compress(content.encode(), mtime=0))
    man = ('.TH FREEMKV 1\n.SH NAME\nfreemkv \\- rip DVD, Blu-ray and UHD discs\n'
           '.SH SYNOPSIS\n.B freemkv\n[command] [options]\n'
           '.SH DESCRIPTION\nCopy original media streams without re-encoding.\n'
           f'.PP\n{meta["usage"]}\n'
           '.SH SEE ALSO\nhttps://freemkv.org\n')
    dest = root / 'usr/share/man/man1'
    dest.mkdir(parents=True)
    (dest / 'freemkv.1.gz').write_bytes(gzip.compress(man.encode(), mtime=0))
    installed = sum(p.stat().st_size for p in root.rglob('*') if p.is_file())
    (root / 'DEBIAN').mkdir(exist_ok=True)
    normalize_modes(root)
    write(root, 'DEBIAN/control',
          f'Package: {package}\nVersion: {package_version}\nArchitecture: amd64\n'
          'Section: video\nPriority: optional\n'
          f'Maintainer: {MAINTAINER}\n'
          f'Installed-Size: {(installed + 1023) // 1024}\n'
          + (f'Depends: {depends}\n' if depends else '')
          + f'Conflicts: {meta["other"]}\nReplaces: {meta["other"]}\n'
          'Homepage: https://freemkv.org\n'
          f'Description: {meta["summary"]}\n' + meta['description'])
    checksums = ''.join(hashlib.md5(p.read_bytes()).hexdigest() + '  ' + str(p.relative_to(root)) + '\n'
                        for p in sorted(root.rglob('*')) if p.is_file() and 'DEBIAN' not in p.parts)
    write(root, 'DEBIAN/md5sums', checksums)


def normalize_modes(root):
    """Debian policy 10.9 modes regardless of the builder's umask: dirs 0755, files 0644."""
    for path in [root, *root.rglob('*')]:
        executable = path.is_dir() or path == root / 'usr/bin/freemkv'
        path.chmod(0o755 if executable else 0o644)


def stage_desktop(root, package_version, epoch):
    packaging = ROOT / 'packaging/flatpak'
    copy(root, packaging / 'org.freemkv.FreeMKV.desktop',
         'usr/share/applications/org.freemkv.FreeMKV.desktop')
    copy(root, ROOT / 'res/freemkv-icon.svg',
         'usr/share/icons/hicolor/scalable/apps/org.freemkv.FreeMKV.svg')
    metainfo = copy(root, packaging / 'org.freemkv.FreeMKV.metainfo.xml',
                    'usr/share/metainfo/org.freemkv.FreeMKV.metainfo.xml')
    flatpak_metadata().update_metadata(metainfo, package_version.replace('~', '-', 1),
                                       datetime.fromtimestamp(epoch, timezone.utc).date().isoformat())


def flatpak_metadata():
    """The Flatpak preparer, which owns the shared AppStream release-history update."""
    spec = importlib.util.spec_from_file_location('flatpak_prepare', ROOT / 'packaging/flatpak/prepare.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def dependency_notices():
    metadata = json.loads(run('cargo', 'metadata', '--locked', '--format-version=1'))
    notices = []
    for package in sorted(metadata['packages'], key=lambda p: (p['name'], p['version'])):
        base = Path(package['manifest_path']).parent
        files = {p for p in base.rglob('*') if p.is_file()
                 and p.name.upper().startswith(('LICENSE', 'LICENCE', 'COPYING', 'NOTICE'))}
        if package.get('license_file'):
            files.add(base / package['license_file'])
        notices.append(f"{package['name']} {package['version']} — {package.get('license') or 'see license files'}\n")
        for path in sorted(files):
            notices.append(path.read_text(errors='replace') + '\n')
    return '\n'.join(notices)


def build(binary, output, package='freemkv'):
    if run('dpkg', '--print-architecture') != 'amd64':
        raise ValueError('build the native package on Ubuntu 24.04 amd64')
    package_version = version()
    epoch = int(run('git', 'log', '-1', '--format=%ct'))
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        if package == 'freemkv':
            control = root / 'debian/control'
            control.parent.mkdir()
            control.write_text(f'Source: freemkv\nSection: video\nPriority: optional\n'
                               f'Maintainer: {MAINTAINER}\n\n'
                               'Package: freemkv\nArchitecture: amd64\nDescription: Disc ripper\n')
            depends = dependencies(run('dpkg-shlibdeps', '-O', str(binary), cwd=root))
        elif static_binary(run('readelf', '-lW', str(binary))):
            depends = ''
        else:
            raise ValueError('freemkv-cli must be a static binary')
        stripped = root / 'stripped'
        shutil.copyfile(binary, stripped)
        subprocess.run(['strip', '--strip-unneeded', str(stripped)], check=True)
        stage(stripped, root / 'package', package_version, depends, epoch, dependency_notices(), package)
        artifact = output / f'{package}-amd64.deb'
        subprocess.run(['dpkg-deb', '--root-owner-group', '--build', str(root / 'package'), str(artifact)],
                       check=True, env={**os.environ, 'SOURCE_DATE_EPOCH': str(epoch)})
        (output / f'{package}.json').write_text(json.dumps({'package': package, 'version': package_version,
            'architecture': 'amd64', 'baseline': 'Ubuntu 24.04', 'depends': depends,
            'source': run('git', 'rev-parse', 'HEAD'),
            'sha256': hashlib.sha256(artifact.read_bytes()).hexdigest()}, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', choices=sorted(PACKAGES), default='freemkv')
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/freemkv')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    build(args.binary.resolve(), args.output.resolve(), args.package)
