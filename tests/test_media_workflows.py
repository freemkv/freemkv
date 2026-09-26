"""Exercise release guards with missing evidence and local Git remotes."""

import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


WORKFLOW = Path(__file__).parents[1] / '.github/workflows/release-orchestrate.yml'


def embedded(start, end):
    source = WORKFLOW.read_text().split(start, 1)[1].split(end, 1)[0]
    return '\n'.join(line[10:] for line in source.splitlines())


class ReleaseEvidenceTests(unittest.TestCase):
    def test_missing_platform_and_stale_dependencies_fail(self):
        code = embedded("          python3 - <<'PYMEDIA'\n", '          PYMEDIA')
        repos = ['freemkv', 'libfreemkv', 'freemkv-engine', 'freemkv-keysources',
                 'freemkv-i18n', 'freemkv-unlock']
        names = ['compare', 'compare-known-answers', 'compare-real']
        names += [f'{job} ({os})' for job in ['suite', 'real-media']
                  for os in ['ubuntu-latest', 'macos-latest', 'windows-latest']]
        for scenario in ['valid', 'missing-platform', 'stale-dependency', 'missing-dependency']:
            revisions = {r: 'a' * 40 for r in repos}
            jobs = [{'name': n, 'conclusion': 'success'} for n in names]
            if scenario == 'missing-platform':
                jobs.pop()
            if scenario == 'stale-dependency':
                revisions['libfreemkv'] = 'b' * 40
            if scenario == 'missing-dependency':
                del revisions['freemkv-engine']
            documents = {'/tmp/qa-shas': '\n'.join(r + ' ' + 'a' * 40 for r in repos),
                         '/tmp/media-jobs.json': json.dumps({'jobs': jobs}),
                         '/tmp/media-evidence/revisions.json': json.dumps(revisions)}
            with self.subTest(scenario=scenario), patch('builtins.open', side_effect=lambda name: io.StringIO(documents[name])):
                if scenario == 'valid':
                    exec(code, {})
                else:
                    with self.assertRaises(SystemExit):
                        exec(code, {})


class ReleaseBranchTests(unittest.TestCase):
    def test_alignment_preserves_restore_commit_and_rejects_divergence(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            remote, checkout = root / 'remote.git', root / 'checkout'

            def git(*args):
                return subprocess.check_output(['git', '-C', str(checkout), *args], stderr=subprocess.DEVNULL, text=True).strip()

            subprocess.run(['git', 'init', '--bare', str(remote)], check=True, capture_output=True)
            subprocess.run(['git', 'clone', str(remote), str(checkout)], check=True, capture_output=True)
            git('config', 'commit.gpgsign', 'false')
            git('config', 'core.hooksPath', str(root / 'no-hooks'))
            git('config', 'user.email', 'test@example.invalid')
            git('config', 'user.name', 'Release test')
            git('checkout', '-b', 'main')
            git('commit', '--allow-empty', '-m', 'base')
            git('branch', 'dev')
            git('checkout', '-b', 'qa')
            git('push', 'origin', 'main', 'qa', 'dev')
            git('commit', '--allow-empty', '-m', 'release')
            git('tag', 'v1.7.7')
            git('commit', '--allow-empty', '-m', 'restore development dependency')
            git('push', 'origin', 'qa')
            target = git('rev-parse', 'HEAD')
            function = embedded('          advance_main_to_release() {\n', '\n          preflight\n')
            script = ('die() { echo "$*" >&2; exit 1; }; ok() { :; }; DRY_RUN=false; VERSION=1.7.7\n'
                      + 'advance_main_to_release() {\n' + function + '\nadvance_main_to_release "$1"\n')

            def align():
                return subprocess.run(['bash', '-c', script, 'test', str(checkout)], capture_output=True, text=True)

            result = align()
            self.assertEqual(result.returncode, 0, result.stderr)
            for branch in ('main', 'qa', 'dev'):
                self.assertEqual(git('ls-remote', 'origin', f'refs/heads/{branch}').split()[0], target)
            self.assertEqual(align().returncode, 0, 'alignment must be resumable')
            git('checkout', 'dev')
            git('reset', '--hard', target)
            git('commit', '--allow-empty', '-m', 'concurrent development')
            git('push', 'origin', 'dev')
            before = git('ls-remote', '--heads', 'origin')
            self.assertNotEqual(align().returncode, 0)
            self.assertEqual(git('ls-remote', '--heads', 'origin'), before)


if __name__ == '__main__':
    unittest.main()
