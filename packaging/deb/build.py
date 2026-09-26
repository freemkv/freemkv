"""Build the native amd64 package on Ubuntu 24.04 using its library metadata."""

import argparse
from datetime import datetime, timezone
from email.utils import format_datetime
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[2]


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


def stage(binary, root, package_version, depends, epoch, notices):
    copy(root, binary, 'usr/bin/freemkv', 0o755)
    write(root, 'usr/bin/freemkv-gui', '#!/bin/sh\nexec /usr/bin/freemkv gui "$@"\n', 0o755)
    packaging = ROOT / 'packaging/flatpak'
    copy(root, packaging / 'org.freemkv.FreeMKV.desktop',
         'usr/share/applications/org.freemkv.FreeMKV.desktop')
    copy(root, ROOT / 'res/freemkv-icon.svg',
         'usr/share/icons/hicolor/scalable/apps/org.freemkv.FreeMKV.svg')
    metainfo = copy(root, packaging / 'org.freemkv.FreeMKV.metainfo.xml',
                    'usr/share/metainfo/org.freemkv.FreeMKV.metainfo.xml')
    tree = ET.parse(metainfo)
    releases = tree.find('releases')
    if releases is None:
        raise ValueError('missing AppStream release history')
    upstream = package_version.replace('~', '-', 1)
    if releases.find('release') is None or releases.find('release').get('version') != upstream:
        releases.insert(0, ET.Element('release', version=upstream,
                                     date=datetime.fromtimestamp(epoch, timezone.utc).date().isoformat()))
    ET.indent(tree, space='  ')
    tree.write(metainfo, encoding='utf-8', xml_declaration=True)
    copyright_text = ((ROOT / 'LICENSE').read_text()
                      + '\nUpstream: https://github.com/freemkv/freemkv\n'
                      + 'Dependency licenses and notices: /usr/share/doc/freemkv/third-party-notices.gz\n')
    write(root, 'usr/share/doc/freemkv/copyright', copyright_text)
    (root / 'usr/share/doc/freemkv/third-party-notices.gz').write_bytes(
        gzip.compress(notices.encode(), mtime=0))
    write(root, 'usr/share/doc/freemkv/README.Debian',
          'Built for Ubuntu 24.04 amd64 and compatible derivatives such as Linux Mint 22.\n'
          'Launch freemkv from the application menu, or run: freemkv gui\n'
          'The command-line interface is also available as freemkv.\n\n'
          'Optical drives use the distribution\'s existing device ACLs and group permissions.\n'
          'Use an active local desktop session. If access is denied, inspect the device\n'
          'owner/group and your distribution\'s optical-drive access policy. No device\n'
          'permissions or group memberships are changed by this package.\n')
    changelog = (f'freemkv ({package_version}) unstable; urgency=medium\n\n'
                 '  * Package the upstream release for Ubuntu 24.04 amd64.\n\n'
                 ' -- Matthew Jackson <1085847+MattJackson@users.noreply.github.com>  '
                 + format_datetime(datetime.fromtimestamp(epoch, timezone.utc)) + '\n')
    for filename, content in [('changelog.Debian.gz', changelog),
                               ('changelog.gz', (ROOT / 'CHANGELOG.md').read_text())]:
        dest = root / 'usr/share/doc/freemkv' / filename
        dest.write_bytes(gzip.compress(content.encode(), mtime=0))
    man = ('.TH FREEMKV 1\n.SH NAME\nfreemkv \\- rip DVD, Blu-ray and UHD discs\n'
           '.SH SYNOPSIS\n.B freemkv\n[command] [options]\n'
           '.SH DESCRIPTION\nCopy original media streams without re-encoding.\n'
           '.PP\nRun freemkv gui for the desktop interface, or freemkv --help for CLI usage.\n'
           '.SH SEE ALSO\nhttps://freemkv.org\n')
    dest = root / 'usr/share/man/man1'
    dest.mkdir(parents=True)
    (dest / 'freemkv.1.gz').write_bytes(gzip.compress(man.encode(), mtime=0))
    (dest / 'freemkv-gui.1.gz').write_bytes(gzip.compress(b'.so man1/freemkv.1\n', mtime=0))
    installed = sum(p.stat().st_size for p in root.rglob('*') if p.is_file())
    write(root, 'DEBIAN/control',
          f'Package: freemkv\nVersion: {package_version}\nArchitecture: amd64\n'
          'Section: video\nPriority: optional\n'
          'Maintainer: Matthew Jackson <1085847+MattJackson@users.noreply.github.com>\n'
          f'Installed-Size: {(installed + 1023) // 1024}\nDepends: {depends}\n'
          'Homepage: https://freemkv.org\n'
          'Description: Rip DVD, Blu-ray and UHD discs without re-encoding\n'
          ' Native GTK4 desktop and command-line interfaces for selecting titles,\n'
          ' audio and subtitle tracks and saving them to MKV, MP4 or M2TS.\n')
    checksums = ''.join(hashlib.md5(p.read_bytes()).hexdigest() + '  ' + str(p.relative_to(root)) + '\n'
                        for p in sorted(root.rglob('*')) if p.is_file() and 'DEBIAN' not in p.parts)
    write(root, 'DEBIAN/md5sums', checksums)


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


def build(binary, output):
    if run('dpkg', '--print-architecture') != 'amd64':
        raise ValueError('build the native package on Ubuntu 24.04 amd64')
    package_version = version()
    epoch = int(run('git', 'log', '-1', '--format=%ct'))
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp)
        control = root / 'debian/control'
        control.parent.mkdir()
        control.write_text('Source: freemkv\nSection: video\nPriority: optional\n'
                           'Maintainer: Matthew Jackson <1085847+MattJackson@users.noreply.github.com>\n\n'
                           'Package: freemkv\nArchitecture: amd64\nDescription: Disc ripper\n')
        depends = dependencies(run('dpkg-shlibdeps', '-O', str(binary), cwd=root))
        stripped = root / 'stripped'
        shutil.copyfile(binary, stripped)
        subprocess.run(['strip', '--strip-unneeded', str(stripped)], check=True)
        stage(stripped, root / 'package', package_version, depends, epoch, dependency_notices())
        artifact = output / f'freemkv-{package_version}-amd64.deb'
        subprocess.run(['dpkg-deb', '--root-owner-group', '--build', str(root / 'package'), str(artifact)],
                       check=True, env={**os.environ, 'SOURCE_DATE_EPOCH': str(epoch)})
        shutil.copyfile(artifact, output / 'freemkv-amd64.deb')
        (output / 'package.json').write_text(json.dumps({'version': package_version, 'architecture': 'amd64',
            'baseline': 'Ubuntu 24.04', 'depends': depends, 'source': run('git', 'rev-parse', 'HEAD'),
            'sha256': hashlib.sha256(artifact.read_bytes()).hexdigest()}, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/release/freemkv')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    build(args.binary.resolve(), args.output.resolve())
