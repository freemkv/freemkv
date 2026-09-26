import gzip
import hashlib
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
            self.assertEqual((root / 'usr/bin/freemkv').stat().st_mode & 0o777, 0o755)
            self.assertEqual((root / 'usr/share/doc/freemkv/copyright').stat().st_mode & 0o777, 0o644)
            self.assertIn('Fixture dependency', (root / 'usr/share/doc/freemkv/copyright').read_text())
            self.assertTrue((root / 'usr/share/applications/org.freemkv.FreeMKV.desktop').is_file())
            self.assertTrue((root / 'usr/share/icons/hicolor/scalable/apps/org.freemkv.FreeMKV.svg').is_file())
            self.assertIn('freemkv (1.7.7)', gzip.decompress((root / 'usr/share/doc/freemkv/changelog.Debian.gz').read_bytes()).decode())
            xml = ET.parse(root / 'usr/share/metainfo/org.freemkv.FreeMKV.metainfo.xml')
            self.assertEqual(xml.find('releases/release').get('version'), '1.7.7')
            for line in (root / 'DEBIAN/md5sums').read_text().splitlines():
                checksum, name = line.split('  ', 1)
                self.assertEqual(hashlib.md5((root / name).read_bytes()).hexdigest(), checksum)
            self.assertFalse((root / 'etc').exists(), 'package must not install device permission overrides')
            self.assertEqual({p.name for p in (root / 'DEBIAN').iterdir()}, {'control', 'md5sums'})

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
