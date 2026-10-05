import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location('deb2rpm', Path(__file__).with_name('deb2rpm.py'))
deb2rpm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deb2rpm)

APP_DEPENDS = ('libadwaita-1-0 (>= 1.4~beta), libc6 (>= 2.39), libgcc-s1 (>= 4.2), '
               'libglib2.0-0t64 (>= 2.54.0), libgtk-4-1 (>= 4.9.3), ca-certificates')


class Requires(unittest.TestCase):
    def test_app_gets_rich_gtk_and_adwaita_floors(self):
        reqs = deb2rpm.requires(APP_DEPENDS, {'gtk4': '4.10', 'libadwaita': '1.4'})
        self.assertEqual(reqs, ['(libadwaita >= 1.4 or libadwaita-1-0 >= 1.4)',
                                '(gtk4 >= 4.10 or libgtk-4-1 >= 4.10)', 'ca-certificates'])

    def test_a_higher_deb_minimum_beats_the_floor(self):
        self.assertEqual(deb2rpm.requires('libgtk-4-1 (>= 4.14.2)', {'gtk4': '4.10'}),
                         ['(gtk4 >= 4.14.2 or libgtk-4-1 >= 4.14.2)'])

    def test_static_cli_has_none(self):
        self.assertEqual(deb2rpm.requires('', {}), [])

    def test_alternatives_are_refused(self):
        with self.assertRaises(ValueError):
            deb2rpm.requires('a | b', {})


class Versions(unittest.TestCase):
    def test_upstream(self):
        self.assertEqual(deb2rpm.upstream('1.8.0'), '1.8.0')
        self.assertEqual(deb2rpm.upstream('1:1.9.0~rc1-2'), '1.9.0~rc1')

    def test_arch_map(self):
        self.assertEqual([deb2rpm.ARCHES[a] for a in ('amd64', 'arm64', 'armhf')],
                         ['x86_64', 'aarch64', 'armv7hl'])

    def test_debian_only_files_are_dropped(self):
        for path in ('usr/share/lintian/overrides/freemkv-cli', 'usr/share/doc/freemkv/README.Debian',
                     'usr/share/doc/freemkv/changelog.Debian.gz'):
            self.assertTrue(deb2rpm.DEBIAN_ONLY.search(path), path)
        self.assertFalse(deb2rpm.DEBIAN_ONLY.search('usr/share/doc/freemkv/changelog.gz'))


class Control(unittest.TestCase):
    def test_multiline_description(self):
        f = deb2rpm.control_fields('Package: x\nDescription: one\n two\n .\n three\n')
        self.assertEqual(f['Description'], 'one\n two\n .\n three')


if __name__ == '__main__':
    unittest.main()
