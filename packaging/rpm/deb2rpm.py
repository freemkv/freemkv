#!/usr/bin/env python3
"""Repackage a release .deb as an .rpm (Fedora, RHEL, openSUSE).

The .deb is the validated build (deb.yml): same binary, desktop entry, icon,
metainfo, man page and notices. This turns its payload into an RPM with
rpmbuild, so rpm's own dependency generator adds the shared-library Requires
(libgtk-4.so.1, libc.so.6(GLIBC_...), ...) that every RPM distro resolves.
Minimum versions the .deb states for GTK/libadwaita become rich Requires that
match both the Fedora (gtk4, libadwaita) and openSUSE (libgtk-4-1,
libadwaita-1-0) package names. Debian-only files are dropped.

Generic: nothing names a package; any .deb in, any architecture.

    deb2rpm.py --output DIR [--license MIT] [--floor gtk4=4.10 ...] PKG.deb...

Writes DIR/<package>-<rpm arch>.rpm. Needs: rpm-build, zstd.
"""
import argparse
import io
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

# dpkg architecture -> rpm architecture.
ARCHES = {'amd64': 'x86_64', 'arm64': 'aarch64', 'armhf': 'armv7hl', 'i386': 'i686', 'all': 'noarch'}
# Debian dependency -> (Fedora name, openSUSE name) for a versioned rich Requires.
# Anything else on the Depends line is a shared library rpm's generator already
# covers, or listed in NAMED.
VERSIONED = {
    'libgtk-4-1': ('gtk4', 'libgtk-4-1'),
    'libadwaita-1-0': ('libadwaita', 'libadwaita-1-0'),
}
# Debian dependency -> RPM name (same on Fedora and openSUSE), not a library.
NAMED = {'ca-certificates': 'ca-certificates'}
# Payload paths that only mean something to dpkg.
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


def untar(blob, name, dest):
    """Extract a control/data tarball whatever its compression (gz, xz, zst)."""
    dest.mkdir(parents=True, exist_ok=True)
    if name.endswith('.zst'):
        blob = subprocess.run(['zstd', '-dc'], input=blob, capture_output=True, check=True).stdout
    with tarfile.open(fileobj=io.BytesIO(blob)) as t:
        t.extractall(dest, filter='tar')


def control_fields(text):
    fields, key = {}, None
    for line in text.splitlines():
        if line[:1] in (' ', '\t') and key:
            fields[key] += '\n' + line
        elif ':' in line:
            key, _, value = line.partition(':')
            key = key.strip()
            fields[key] = value.strip()
    return fields


def parse_depends(value):
    """[(name, op, version)] of a Depends line; alternatives are not used by freemkv."""
    out = []
    for item in filter(None, (s.strip() for s in value.split(','))):
        if '|' in item:
            raise ValueError(f'alternative dependencies are not supported: {item}')
        m = re.fullmatch(r'([a-z0-9][a-z0-9+.-]+)(?::\S+)?\s*(?:\(\s*([<>=]+)\s*([^)\s]+)\s*\))?', item)
        if not m:
            raise ValueError(f'unparsable dependency: {item}')
        out.append(m.groups())
    return out


def upstream(version):
    """Debian version -> rpm Version: drop epoch and Debian revision; `~` sorts the same in both."""
    v = version.split(':', 1)[-1]
    v = v.rsplit('-', 1)[0] if '-' in v else v
    if not re.fullmatch(r'[0-9A-Za-z.+~^]+', v):
        raise ValueError(f'unsupported version for rpm: {version}')
    return v


def vkey(v):
    return [int(p) if p.isdigit() else 0 for p in re.split(r'[.~+-]', v)]


def requires(depends, floors):
    out = []
    for name, op, ver in parse_depends(depends):
        if name in VERSIONED:
            fedora, suse = VERSIONED[name]
            need = upstream(ver.replace('~beta', '').replace('~', '')) if ver and '>' in op else None
            floor = floors.get(fedora)
            if floor and (not need or vkey(floor) > vkey(need)):
                need = floor
            out.append(f'({fedora} >= {need} or {suse} >= {need})' if need else f'({fedora} or {suse})')
        elif name in NAMED:
            out.append(NAMED[name])
        # Libraries (libc6, libgcc-s1, libglib2.0-0t64, ...): rpm's ELF
        # dependency generator emits the matching soname Requires.
    return out


def spec_text(f, version, arch, files, dirs, reqs, license_):
    package = f['Package']
    summary, _, body = f['Description'].partition('\n')
    description = '\n'.join('' if l.strip() == '.' else l.strip() for l in body.splitlines()) or summary
    conflicts = [d[0] for d in parse_depends(f.get('Conflicts', ''))]
    lines = [
        '%global debug_package %{nil}',
        '%global __strip /bin/true',
        '%global __brp_strip %{nil}',
        '%global _build_id_links none',
        # xz: every rpm in use (RHEL 8, Leap 15) reads it.
        '%define _binary_payload w6.xzdio',
        '%define __os_install_post %{nil}',
        f'Name: {package}',
        f'Version: {version}',
        'Release: 1',
        f'Summary: {summary.strip()}',
        f'License: {license_}',
        f'URL: {f.get("Homepage", "https://freemkv.org")}',
        f'BuildArch: {arch}' if arch == 'noarch' else '',
        *(f'Requires: {r}' for r in reqs),
        *(f'Conflicts: {c}' for c in conflicts),
        '',
        '%description',
        description,
        '',
        '%install',
        'cp -a %{_sourcedir}/root/. %{buildroot}/',
        '',
        '%files',
        '%defattr(-,root,root,-)',
        *(f'%dir "/{d}"' for d in dirs),
        *(f'"/{p}"' for p in files),
        '',
    ]
    return '\n'.join(l for l in lines if l is not None) + '\n'


def convert(deb, output, license_, floors):
    members = ar_members(deb.read_bytes())
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        ctl = next(n for n in members if n.startswith('control.tar'))
        dat = next(n for n in members if n.startswith('data.tar'))
        untar(members[ctl], ctl, tmp / 'control')
        root = tmp / 'SOURCES' / 'root'
        untar(members[dat], dat, root)
        f = control_fields((tmp / 'control' / 'control').read_text())
        package, arch = f['Package'], ARCHES[f['Architecture']]
        for p in sorted(root.rglob('*'), reverse=True):
            rel = p.relative_to(root).as_posix()
            if DEBIAN_ONLY.search(rel):
                shutil.rmtree(p) if p.is_dir() and not p.is_symlink() else p.unlink()
        files = sorted(p.relative_to(root).as_posix() for p in root.rglob('*')
                       if p.is_file() or p.is_symlink())
        # Own only directories that are this package's (doc dir); the shared
        # hierarchy (/usr/bin, hicolor, ...) belongs to filesystem packages.
        dirs = sorted({p.relative_to(root).as_posix() for p in root.rglob('*')
                       if p.is_dir() and re.fullmatch(rf'usr/share/doc/{re.escape(package)}', p.relative_to(root).as_posix())})
        version = upstream(f['Version'])
        spec = tmp / 'SPECS' / f'{package}.spec'
        spec.parent.mkdir()
        spec.write_text(spec_text(f, version, arch, files, dirs, requires(f.get('Depends', ''), floors), license_))
        subprocess.run(['rpmbuild', '-bb', '--target', arch, '--define', f'_topdir {tmp}', str(spec)],
                       check=True, stdout=subprocess.DEVNULL)
        built = list((tmp / 'RPMS').rglob('*.rpm'))
        if len(built) != 1:
            raise RuntimeError(f'{deb.name}: rpmbuild produced {built}')
        output.mkdir(parents=True, exist_ok=True)
        dest = output / f'{package}-{arch}.rpm'
        shutil.copyfile(built[0], dest)
        print(f'{deb.name} -> {dest.name} ({package} {version} {arch})')
        return dest


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--output', type=Path, required=True)
    ap.add_argument('--license', default='MIT')
    ap.add_argument('--floor', action='append', default=[], metavar='NAME=VERSION',
                    help='raise a rich Requires minimum, e.g. gtk4=4.10 (Fedora name)')
    ap.add_argument('debs', type=Path, nargs='+')
    args = ap.parse_args()
    floors = dict(x.split('=', 1) for x in args.floor)
    for deb in args.debs:
        convert(deb, args.output, args.license, floors)


if __name__ == '__main__':
    sys.exit(main())
