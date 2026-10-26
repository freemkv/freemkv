"""The full-disc gate must preserve the explicit 3D folder limitation."""

from pathlib import Path
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


if __name__ == '__main__':
    unittest.main()
