import gzip
import hashlib
import os
from pathlib import Path
import tempfile
import unittest
import xml.etree.ElementTree as ET

import build


class DependencyTests(unittest.TestCase):
    def test_missing_or_non_gui_dependencies_fail(self):
        for output in ['', 'shlibs:Depends=', 'shlibs:Depends=libc6 (>= 2.38)']:
            with self.subTest(output=output), self.assertRaises(ValueError):
                build.dependencies(output)

    def test_actual_symbol_versions_are_preserved(self):
        deps = 'libadwaita-1-0 (>= 1.4), libc6 (>= 2.38), libgtk-4-1 (>= 4.10)'
        self.assertEqual(build.dependencies('shlibs:Depends=' + deps), deps + ', ca-certificates')


class StaticBinaryTests(unittest.TestCase):
    HEADERS = 'Program Headers:\n  Type Offset\n  LOAD 0x000000\n'

    def test_interpreter_means_dynamic(self):
        self.assertTrue(build.static_binary(self.HEADERS))
        self.assertFalse(build.static_binary(self.HEADERS + '  INTERP 0x000318\n'))

    def test_unreadable_binary_fails(self):
        with self.assertRaises(ValueError):
            build.static_binary('readelf: Error: Not an ELF file')


class ArchitectureTests(unittest.TestCase):
    def test_elf_machine_is_read_from_the_header(self):
        self.assertEqual(build.elf_machine('ELF Header:\n  Class: ELF32\n  Machine:                           ARM\n'), 'ARM')
        with self.assertRaises(ValueError):
            build.elf_machine('readelf: Error: Not an ELF file')

    def test_every_arch_has_a_distinct_machine(self):
        self.assertEqual(set(build.ARCHES), {'amd64', 'arm64', 'armhf'})
        self.assertEqual(len({m for m, _ in build.ARCHES.values()}), len(build.ARCHES))
        self.assertTrue(set(build.APP_ARCHES) <= set(build.ARCHES))

    def test_foreign_binaries_use_the_cross_strip(self):
        self.assertEqual(build.strip_tool('amd64', 'amd64'), 'strip')
        self.assertEqual(build.strip_tool('arm64', 'arm64'), 'strip')
        self.assertEqual(build.strip_tool('armhf', 'amd64'), 'arm-linux-gnueabihf-strip')


class PackageTests(unittest.TestCase):
    def test_complete_native_layout_version_permissions_and_checksums(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            binary = base / 'binary'
            binary.write_bytes(b'fixture executable')
            root = base / 'package'
            build.stage(binary, root, '1.7.7', 'libc6 (>= 2.38)', 1790380800,
                        'Fixture dependency copyright and license\n')
            control = (root / 'DEBIAN/control').read_text()
            self.assertIn('Version: 1.7.7\n', control)
            self.assertIn('Architecture: amd64\n', control)
            self.assertIn('Depends: libc6 (>= 2.38)\n', control)
            self.assertIn('Conflicts: freemkv-cli\nReplaces: freemkv-cli\n', control)
            self.assertFalse((root / 'usr/bin/freemkv-gui').exists())
            desktop = (root / 'usr/share/applications/org.freemkv.FreeMKV.desktop').read_text()
            self.assertIn('\nName=freemkv\n', desktop)
            self.assertIn('\nExec=freemkv\n', desktop)
            self.assertEqual((root / 'usr/bin/freemkv').stat().st_mode & 0o777, 0o755)
            self.assertEqual((root / 'usr/share/doc/freemkv/copyright').stat().st_mode & 0o777, 0o644)
            self.assertIn('third-party-notices.gz', (root / 'usr/share/doc/freemkv/copyright').read_text())
            self.assertIn(b'Fixture dependency', gzip.decompress(
                (root / 'usr/share/doc/freemkv/third-party-notices.gz').read_bytes()))
            self.assertTrue((root / 'usr/share/icons/hicolor/scalable/apps/org.freemkv.FreeMKV.svg').is_file())
            self.assertIn('freemkv (1.7.7)', gzip.decompress((root / 'usr/share/doc/freemkv/changelog.Debian.gz').read_bytes()).decode())
            xml = ET.parse(root / 'usr/share/metainfo/org.freemkv.FreeMKV.metainfo.xml')
            self.assertEqual(xml.find('releases/release').get('version'), '1.7.7')
            for line in (root / 'DEBIAN/md5sums').read_text().splitlines():
                checksum, name = line.split('  ', 1)
                self.assertEqual(hashlib.md5((root / name).read_bytes()).hexdigest(), checksum)
            self.assertFalse((root / 'etc').exists(), 'package must not install device permission overrides')
            self.assertEqual({p.name for p in (root / 'DEBIAN').iterdir()}, {'control', 'md5sums'})

    def test_arch_reaches_control_readme_and_changelog(self):
        for package, arch in [('freemkv', 'arm64'), ('freemkv-cli', 'armhf')]:
            with self.subTest(package=package, arch=arch), tempfile.TemporaryDirectory() as temp:
                base = Path(temp)
                binary = base / 'binary'
                binary.write_bytes(b'fixture executable')
                root = base / 'package'
                build.stage(binary, root, '1.7.7', '', 1790380800, 'notices\n', package, arch)
                self.assertIn(f'Architecture: {arch}\n', (root / 'DEBIAN/control').read_text())
                doc = root / 'usr/share/doc' / package
                self.assertIn(arch, (doc / 'README.Debian').read_text())
                self.assertNotIn('amd64', (doc / 'README.Debian').read_text())
                self.assertIn(f'Ubuntu 24.04 {arch}', gzip.decompress((doc / 'changelog.Debian.gz').read_bytes()).decode())

    def test_cli_is_static_without_desktop_files_and_replaces_the_app(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            binary = base / 'binary'
            binary.write_bytes(b'fixture executable')
            root = base / 'package'
            build.stage(binary, root, '1.7.8~rc1', '', 1790380800, 'Fixture notices\n', 'freemkv-cli')
            control = (root / 'DEBIAN/control').read_text()
            self.assertIn('Package: freemkv-cli\n', control)
            self.assertIn('Version: 1.7.8~rc1\n', control)
            self.assertNotIn('Depends:', control)
            self.assertIn('Conflicts: freemkv\nReplaces: freemkv\n', control)
            self.assertEqual((root / 'usr/bin/freemkv').stat().st_mode & 0o777, 0o755)
            doc = root / 'usr/share/doc/freemkv-cli'
            self.assertIn('/usr/share/doc/freemkv-cli/third-party-notices.gz', (doc / 'copyright').read_text())
            self.assertIn(b'Fixture notices', gzip.decompress((doc / 'third-party-notices.gz').read_bytes()))
            self.assertTrue(gzip.decompress((doc / 'changelog.Debian.gz').read_bytes())
                            .decode().startswith('freemkv-cli (1.7.8~rc1)'))
            self.assertTrue((doc / 'changelog.gz').is_file())
            self.assertTrue((root / 'usr/share/man/man1/freemkv.1.gz').is_file())
            for path in ['usr/share/doc/freemkv', 'usr/share/applications', 'usr/share/icons',
                         'usr/share/metainfo', 'etc']:
                self.assertFalse((root / path).exists(), path)
            files = {str(p.relative_to(root)) for p in root.rglob('*') if p.is_file() and 'DEBIAN' not in p.parts}
            sums = {line.split('  ', 1)[1] for line in (root / 'DEBIAN/md5sums').read_text().splitlines()}
            self.assertEqual(files, sums)

    def test_modes_do_not_depend_on_the_builder_umask(self):
        for package in build.PACKAGES:
            with tempfile.TemporaryDirectory() as temp:
                base = Path(temp)
                binary = base / 'binary'
                binary.write_bytes(b'fixture executable')
                root = base / 'package'
                old = os.umask(0o002)
                try:
                    build.stage(binary, root, '1.7.7', '', 1790380800, 'notices\n', package)
                finally:
                    os.umask(old)
                for path in [root, *root.rglob('*')]:
                    want = 0o755 if path.is_dir() or path == root / 'usr/bin/freemkv' else 0o644
                    self.assertEqual(path.stat().st_mode & 0o777, want, f'{package}: {path.relative_to(root)}')

    def test_prerelease_is_an_upstream_version_in_metadata(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            binary = base / 'binary'
            binary.touch()
            build.stage(binary, base / 'package', '1.7.8~rc1', 'libc6', 1790380800, '')
            xml = ET.parse(base / 'package/usr/share/metainfo/org.freemkv.FreeMKV.metainfo.xml')
            self.assertEqual(xml.find('releases/release').get('version'), '1.7.8-rc1')


if __name__ == '__main__':
    unittest.main()
