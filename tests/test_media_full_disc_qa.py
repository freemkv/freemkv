"""The full-disc gate must preserve the explicit 3D folder limitation."""

from pathlib import Path
import sys
import tempfile
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


if __name__ == '__main__':
    unittest.main()
