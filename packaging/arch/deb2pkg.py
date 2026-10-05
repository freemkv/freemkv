#!/usr/bin/env python3
"""Repackage a release .deb as an Arch Linux package (.pkg.tar.zst).

The .deb is the validated build (deb.yml): same binary, desktop entry, icon,
metainfo, man page and notices. A generated PKGBUILD unpacks its payload and
makepkg builds the package, so the result is a normal pacman package. Debian
Depends map to Arch package names (DEPS); the GTK/libadwaita minimums can be
raised with --floor. Debian-only files are dropped; the copyright file is
installed as the package license.

Generic: nothing names a package; any .deb in, any architecture.

    deb2pkg.py --output DIR [--license MIT] [--floor gtk4=4.10 ...] PKG.deb...

Run as an unprivileged user on Arch Linux (makepkg refuses root). Writes
DIR/<package>-<arch>.pkg.tar.zst.
"""
import argparse
import hashlib
import io
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

# dpkg architecture -> pacman architecture (Arch Linux / Arch Linux ARM).
ARCHES = {'amd64': 'x86_64', 'arm64': 'aarch64', 'armhf': 'armv7h', 'all': 'any'}
# Debian dependency -> Arch Linux package.
DEPS = {
    'libc6': 'glibc', 'libgcc-s1': 'gcc-libs', 'libglib2.0-0t64': 'glib2', 'libglib2.0-0': 'glib2',
    'libgtk-4-1': 'gtk4', 'libadwaita-1-0': 'libadwaita', 'ca-certificates': 'ca-certificates',
    'libgl1': 'libglvnd', 'libegl1': 'libglvnd', 'libx11-6': 'libx11', 'libxkbcommon0': 'libxkbcommon',
    'libxkbcommon-x11-0': 'libxkbcommon-x11', 'libwayland-client0': 'wayland', 'libfontconfig1': 'fontconfig',
    'libfreetype6': 'freetype2', 'libdbus-1-3': 'dbus', 'libudev1': 'systemd-libs',
    'libssl3t64': 'openssl', 'libssl3': 'openssl', 'libxcursor1': 'libxcursor', 'libxrandr2': 'libxrandr',
    'libxi6': 'libxi', 'libvulkan1': 'vulkan-icd-loader',
    'libcairo2': 'cairo', 'libgdk-pixbuf-2.0-0': 'gdk-pixbuf2', 'libpango-1.0-0': 'pango',
    'libpangocairo-1.0-0': 'pango', 'libharfbuzz0b': 'harfbuzz', 'libgraphene-1.0-0': 'graphene',
}
DEBIAN_ONLY = re.compile(r'^usr/share/lintian(/|$)|^usr/share/doc/[^/]+/(README\.Debian|changelog\.Debian\.gz)$')


def ar_members(data):
    if not data.startswith(b'!<arch>\n'):
        raise ValueError('not a .deb (ar) archive')
    pos, out = 8, {}
    while pos < len(data):
        header = data[pos:pos + 60]
        name = header[:16].decode().strip().rstrip('/')
        size = int(header[48:58].decode().strip())
        out[name] = data[pos + 60:pos + 60 + size]
        pos += 60 + size + (size & 1)
    return out


def control_of(deb):
    members = ar_members(deb.read_bytes())
    name = next(n for n in members if n.startswith('control.tar'))
    blob = members[name]
    if name.endswith('.zst'):
        blob = subprocess.run(['zstd', '-dc'], input=blob, capture_output=True, check=True).stdout
    with tarfile.open(fileobj=io.BytesIO(blob)) as t:
        text = t.extractfile('./control').read().decode()
    fields, key = {}, None
    for line in text.splitlines():
        if line[:1] in (' ', '\t') and key:
            fields[key] += '\n' + line
        elif ':' in line:
            key, _, value = line.partition(':')
            key = key.strip()
            fields[key] = value.strip()
    return fields


def names(value):
    out = []
    for item in filter(None, (s.strip() for s in value.split(','))):
        if '|' in item:
            raise ValueError(f'alternative dependencies are not supported: {item}')
        out.append(re.match(r'[a-z0-9][a-z0-9+.-]+', item).group(0))
    return out


def depends(value, floors):
    out = []
    for dep in names(value):
        if dep not in DEPS:
            raise ValueError(f'no Arch Linux mapping for Debian dependency {dep!r}; add it to DEPS')
        arch = DEPS[dep]
        entry = f'{arch}>={floors[arch]}' if arch in floors else arch
        if entry not in out:
            out.append(entry)
    return out


def pkgver(version):
    """Debian version -> pacman pkgver: no epoch or revision; `~rc1` becomes `rc1` (sorts before the release)."""
    v = version.split(':', 1)[-1]
    v = v.rsplit('-', 1)[0] if '-' in v else v
    v = v.replace('~', '')
    if not re.fullmatch(r'[0-9A-Za-z.+_]+', v):
        raise ValueError(f'unsupported version for pacman: {version}')
    return v


def q(values):
    return ' '.join("'" + v.replace("'", "'\\''") + "'" for v in values)


def pkgbuild(f, deb_name, digest, license_, floors):
    package = f['Package']
    summary = f['Description'].split('\n', 1)[0].strip()
    lines = [
        f'pkgname={package}',
        f'pkgver={pkgver(f["Version"])}',
        'pkgrel=1',
        f'pkgdesc={q([summary])}',
        f'arch=({q([ARCHES[f["Architecture"]]])})',
        f'url={q([f.get("Homepage", "https://freemkv.org")])}',
        f'license=({q([license_])})',
        f'depends=({q(depends(f.get("Depends", ""), floors))})',
        f'conflicts=({q(names(f.get("Conflicts", "")))})',
        "options=('!strip' '!debug')",
        f'source=({q([deb_name])})',
        f'sha256sums=({q([digest])})',
        '',
        'package() {',
        '  bsdtar -xf data.tar.* -C "$pkgdir"',
        '  rm -rf "$pkgdir"/usr/share/lintian',
        '  rm -f "$pkgdir"/usr/share/doc/*/README.Debian "$pkgdir"/usr/share/doc/*/changelog.Debian.gz',
        '  chmod 755 "$pkgdir"',
        '  install -Dm644 "$pkgdir/usr/share/doc/$pkgname/copyright" "$pkgdir/usr/share/licenses/$pkgname/LICENSE"',
        '}',
        '',
    ]
    return '\n'.join(lines)


def convert(deb, output, license_, floors):
    f = control_of(deb)
    arch = ARCHES[f['Architecture']]
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        shutil.copyfile(deb, tmp / deb.name)
        digest = hashlib.sha256(deb.read_bytes()).hexdigest()
        (tmp / 'PKGBUILD').write_text(pkgbuild(f, deb.name, digest, license_, floors))
        conf = tmp / 'makepkg.conf'
        conf.write_text(Path('/etc/makepkg.conf').read_text()
                        + f'\nCARCH="{arch}"\nPKGEXT=".pkg.tar.zst"\nPKGDEST="{tmp}/out"\n')
        (tmp / 'out').mkdir()
        subprocess.run(['makepkg', '--config', str(conf), '--nodeps', '--noconfirm', '--cleanbuild'],
                       cwd=tmp, check=True, env={**os.environ, 'CARCH': arch})
        built = list((tmp / 'out').glob('*.pkg.tar.zst'))
        if len(built) != 1:
            raise RuntimeError(f'{deb.name}: makepkg produced {built}')
        output.mkdir(parents=True, exist_ok=True)
        dest = output / f'{f["Package"]}-{arch}.pkg.tar.zst'
        shutil.copyfile(built[0], dest)
        print(f'{deb.name} -> {dest.name} ({built[0].name})')
        return dest


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--output', type=Path, required=True)
    ap.add_argument('--license', default='MIT')
    ap.add_argument('--floor', action='append', default=[], metavar='NAME=VERSION',
                    help='minimum version for an Arch dependency, e.g. gtk4=4.10')
    ap.add_argument('debs', type=Path, nargs='+')
    args = ap.parse_args()
    floors = dict(x.split('=', 1) for x in args.floor)
    for deb in args.debs:
        convert(deb.resolve(), args.output.resolve(), args.license, floors)


if __name__ == '__main__':
    sys.exit(main())
