"""The full-disc gate must preserve the explicit 3D folder limitation."""

import hashlib
from pathlib import Path
import sys
import tempfile
import threading
import types
import unittest
from unittest.mock import patch

import full_disc_qa as qa


class ThreeDFolderTests(unittest.TestCase):
    def test_ssif_folder_requires_the_specific_failure_and_no_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / 'title.mkv'

            def refusal(command, **kwargs):
                kwargs['stdout'].write('Error: E9061 unsupported SSIF\n')
                return types.SimpleNamespace(returncode=1)

            with patch.object(qa.subprocess, 'run', refusal):
                qa.check_3d_folder_limit(root / 'freemkv', root / 'folder', target,
                                         root / 'run.log', 1, root / 'empty-keydb.cfg')

            def wrong_failure(command, **kwargs):
                kwargs['stdout'].write('Error: E7013 bad key\n')
                return types.SimpleNamespace(returncode=1)

            with patch.object(qa.subprocess, 'run', wrong_failure):
                with self.assertRaisesRegex(RuntimeError, 'did not refuse'):
                    qa.check_3d_folder_limit(root / 'freemkv', root / 'folder', target,
                                             root / 'run.log', 1, root / 'empty-keydb.cfg')

            def success_without_output(command, **kwargs):
                kwargs['stdout'].write('Error: E9061 unsupported SSIF\n')
                return types.SimpleNamespace(returncode=0)

            with patch.object(qa.subprocess, 'run', success_without_output):
                with self.assertRaisesRegex(RuntimeError, 'did not refuse'):
                    qa.check_3d_folder_limit(root / 'freemkv', root / 'folder', target,
                                             root / 'run.log', 1, root / 'empty-keydb.cfg')

            def refusal_with_output(command, **kwargs):
                kwargs['stdout'].write('Error: E9061 unsupported SSIF\n')
                target.write_bytes(b'mkv')
                return types.SimpleNamespace(returncode=1)

            with patch.object(qa.subprocess, 'run', refusal_with_output):
                with self.assertRaisesRegex(RuntimeError, 'did not refuse'):
                    qa.check_3d_folder_limit(root / 'freemkv', root / 'folder', target,
                                             root / 'run.log', 1, root / 'empty-keydb.cfg')


class RedactTests(unittest.TestCase):
    def test_key_values_and_url_hosts_are_hidden(self):
        text = ('exited 1:\nfetch https://keys.example.net/v1/disc?t=abc failed\n'
                'auth tok-123 and url KEYURL; http://other.example:8080/x')
        out = qa.redact(text, 'KEYURL', 'tok-123', '')
        for leaked in ('keys.example.net', 'other.example', 'tok-123', 'KEYURL'):
            self.assertNotIn(leaked, out)
        self.assertIn('<redacted>', out)
        self.assertIn('exited 1:', out)

    def test_run_failure_tail_is_redacted_in_the_report(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def failing(command, **kwargs):
                kwargs['stdout'].write('GET https://keys.example.net/x secret-auth\n')
                return types.SimpleNamespace(returncode=2)

            argv = ['qa', '--binary', 'b', '--bd-iso', 'b', '--uhd-iso', 'u', '--keydb', 'k',
                    '--key-url', 'https://keys.example.net/x', '--key-auth', 'secret-auth',
                    '--out', str(root)]
            with patch.object(sys, 'argv', argv), patch.object(qa.subprocess, 'run', failing), \
                    patch.object(qa.Path, 'is_file', lambda self: True), \
                    patch.object(qa, 'check_family', lambda *a, **k: qa.run(
                        Path('b'), 'src', 'dst', root / 'l.log')):
                with self.assertRaises(RuntimeError):
                    qa.main()
            report = (root / 'full-disc-qa.json').read_text()
            self.assertNotIn('keys.example.net', report)
            self.assertNotIn('secret-auth', report)
            self.assertIn('<redacted>', report)


def fake_disc_run(barrier):
    """A stand-in CLI: copies meet at `barrier`, so a serial gate deadlocks."""
    def run(binary, source, destination, log, *options):
        scheme, target = destination.split('://', 1)
        target = Path(target)
        if source.startswith('iso://') and target.name in ('decrypted.iso', 'direct-folder'):
            barrier.wait()
        if scheme == 'iso':
            target.write_bytes(Path(source.split('://', 1)[1]).read_bytes())
        elif scheme == 'dir':
            for name, body in (('SCAA/Unit_Key_RO.inf', b'key'), ('BDMV/index.bdmv', b'INDX'),
                               ('BDMV/PLAYLIST/00000.mpls', b'MPLS0200'),
                               ('BDMV/CLIPINF/00000.clpi', b'HDMV0200')):
                (target / name).parent.mkdir(parents=True, exist_ok=True)
                (target / name).write_bytes(body)
        else:
            target.write_bytes(b'mkv')
    return run


NAVIGATION = ['BDMV/index.bdmv', 'BDMV/PLAYLIST/00000.mpls', 'BDMV/CLIPINF/00000.clpi']


class DecryptedNamesTests(unittest.TestCase):
    def test_the_aacs_folder_is_renamed_scaa_with_its_key_file_kept(self):
        qa.assert_disc_tree({name: {} for name in ['SCAA/Unit_Key_RO.inf', 'SCAA/MKB_RO.inf',
                                                   *NAVIGATION]})

    def test_an_aacs_folder_or_a_renamed_key_file_fails(self):
        for tree in (['AACS/Unit_Key_RO.inf'],                         # not renamed
                     ['AACS/Unit_Key_RO.bak'],                         # 1.8.6's file rename
                     ['SCAA/Unit_Key_RO.inf', 'AACS/Unit_Key_RO.inf'],  # both folders
                     ['SCAA/Unit_Key_RO.bak'],                         # key file not OEM
                     []):
            with self.subTest(tree=tree):
                with self.assertRaisesRegex(RuntimeError, 'SCAA'):
                    qa.assert_disc_tree({name: {} for name in [*tree, *NAVIGATION]})


class FullDiscFlowTests(unittest.TestCase):
    def test_copies_overlap_and_every_hash_is_unchanged(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'bd.iso'
            source.write_bytes(b'decrypted image bytes')
            out = root / 'out'
            out.mkdir()
            barrier = threading.Barrier(2, timeout=10)
            with patch.object(qa, 'run', fake_disc_run(barrier)), \
                    patch.object(qa, 'smallest_title', lambda binary, source: 1), \
                    patch.object(qa, 'content_hash', lambda path: 'same'), \
                    patch.object(qa.shutil, 'disk_usage', lambda path: types.SimpleNamespace(free=1 << 40)):
                report = qa.check_family(root / 'freemkv', 'bd', source, [], out)
            self.assertEqual(report['decrypted_iso_sha256'],
                             hashlib.sha256(b'decrypted image bytes').hexdigest())
            self.assertEqual(report['files']['BDMV/index.bdmv'],
                             {'bytes': 4, 'sha256': hashlib.sha256(b'INDX').hexdigest()})
            self.assertEqual(report['file_count'], 4)
            self.assertEqual(set(report['title_hashes']), {'source', 'decrypted_iso', 'folder'})

    def test_manifest_matches_serial_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tree = root / 'tree'
            for index in range(20):
                (tree / f'd{index % 3}').mkdir(parents=True, exist_ok=True)
                (tree / f'd{index % 3}' / f'{index}.bin').write_bytes(bytes([index]) * (index * 997))
            files = qa.manifest(tree)
            serial = {path.relative_to(tree).as_posix(): {'bytes': path.stat().st_size,
                                                          'sha256': qa.sha256(path)}
                      for path in sorted(tree.rglob('*')) if path.is_file()}
            self.assertEqual(files, serial)
            self.assertEqual(list(files), sorted(files))


if __name__ == '__main__':
    unittest.main()
