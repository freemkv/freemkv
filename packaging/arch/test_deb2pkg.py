import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location('deb2pkg', Path(__file__).with_name('deb2pkg.py'))
deb2pkg = importlib.util.module_from_spec(spec)
spec.loader.exec_module(deb2pkg)

APP = {
    'Package': 'freemkv', 'Version': '1.8.0', 'Architecture': 'amd64', 'Homepage': 'https://freemkv.org',
    'Depends': 'libadwaita-1-0 (>= 1.4~beta), libc6 (>= 2.39), libgcc-s1 (>= 4.2), '
               'libglib2.0-0t64 (>= 2.54.0), libgtk-4-1 (>= 4.9.3), ca-certificates',
    'Conflicts': 'freemkv-cli',
    'Description': "Rip DVD, Blu-ray and UHD discs without re-encoding\n more",
}


class Depends(unittest.TestCase):
    def test_app_maps_to_arch_names_with_floors(self):
        self.assertEqual(deb2pkg.depends(APP['Depends'], {'gtk4': '4.10', 'libadwaita': '1.4'}),
                         ['libadwaita>=1.4', 'glibc', 'gcc-libs', 'glib2', 'gtk4>=4.10', 'ca-certificates'])

    def test_arm64_app_maps_its_extra_libraries(self):
        # dpkg-shlibdeps on arm64 lists cairo, gdk-pixbuf and pango directly.
        arm64 = ('libadwaita-1-0 (>= 1.4~beta), libc6 (>= 2.39), libcairo2 (>= 1.2.4), libgcc-s1 (>= 4.2), '
                 'libgdk-pixbuf-2.0-0 (>= 2.22.0), libglib2.0-0t64 (>= 2.54.0), libgtk-4-1 (>= 4.9.3), '
                 'libpango-1.0-0 (>= 1.14.0), ca-certificates')
        self.assertEqual(deb2pkg.depends(arm64, {}),
                         ['libadwaita', 'glibc', 'cairo', 'gcc-libs', 'gdk-pixbuf2', 'glib2', 'gtk4', 'pango',
                          'ca-certificates'])

    def test_static_cli_has_none(self):
        self.assertEqual(deb2pkg.depends('', {}), [])

    def test_unknown_dependency_is_an_error(self):
        with self.assertRaises(ValueError):
            deb2pkg.depends('libsomething9', {})


class Versions(unittest.TestCase):
    def test_pkgver(self):
        self.assertEqual(deb2pkg.pkgver('1.8.0'), '1.8.0')
        self.assertEqual(deb2pkg.pkgver('1:1.9.0~rc1-2'), '1.9.0rc1')

    def test_arch_map(self):
        self.assertEqual([deb2pkg.ARCHES[a] for a in ('amd64', 'arm64', 'armhf')], ['x86_64', 'aarch64', 'armv7h'])


class Pkgbuild(unittest.TestCase):
    def test_fields(self):
        text = deb2pkg.pkgbuild(APP, 'freemkv-amd64.deb', 'ab' * 32, 'MIT', {'gtk4': '4.10'})
        for line in ("pkgname=freemkv", "pkgver=1.8.0", "arch=('x86_64')", "conflicts=('freemkv-cli')",
                     "pkgdesc='Rip DVD, Blu-ray and UHD discs without re-encoding'",
                     "source=('freemkv-amd64.deb')"):
            self.assertIn(line + '\n', text)
        self.assertIn("'gtk4>=4.10'", text)


if __name__ == '__main__':
    unittest.main()
