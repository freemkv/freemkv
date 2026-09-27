"""Exercise release guards with missing evidence and local Git remotes."""

import base64
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch


WORKFLOWS = Path(__file__).parents[1] / '.github/workflows'
WORKFLOW = WORKFLOWS / 'release-orchestrate.yml'
REPOS = ['freemkv-unlock', 'libfreemkv', 'freemkv-keysources', 'freemkv-i18n',
         'freemkv-engine', 'bdemu', 'freemkv', 'autorip']


def embedded(start, end):
    source = WORKFLOW.read_text().split(start, 1)[1].split(end, 1)[0]
    return '\n'.join(line[10:] for line in source.splitlines())


def cascade_function(name):
    lines = WORKFLOW.read_text().splitlines()
    start = lines.index(f'          {name}() {{')
    end = lines.index('          }', start)
    return '\n'.join(line[10:] for line in lines[start:end + 1])


def run_block(workflow, step):
    lines = (WORKFLOWS / workflow).read_text().splitlines()
    start = next(n for n, line in enumerate(lines) if line.strip() == f'- name: {step}')
    run = next(n for n in range(start, len(lines)) if lines[n].strip() == 'run: |')
    indent = len(lines[run]) - len(lines[run].lstrip())
    body = []
    for line in lines[run + 1:]:
        if line.strip() and len(line) - len(line.lstrip()) <= indent:
            break
        body.append(line)
    width = min(len(line) - len(line.lstrip()) for line in body if line.strip())
    return '\n'.join(line[width:] for line in body)


def git_in(path, *args):
    return subprocess.check_output(
        ['git', '-C', str(path), '-c', 'core.hooksPath=/nonexistent', '-c', 'commit.gpgsign=false',
         '-c', 'tag.gpgsign=false', '-c', 'user.email=test@example.com', '-c', 'user.name=Release test',
         *args], stderr=subprocess.DEVNULL, text=True).strip()


FAKE_GH = r"""import json, os, subprocess, sys
args = sys.argv[1:]
with open(os.environ['FAKE_GH_LOG'], 'a') as log:
    log.write(' '.join(args) + '\n')
if args[:1] != ['api']:
    sys.exit(0)
routes = json.load(open(os.environ['FAKE_GH_ROUTES']))
path, query, method, rest = None, None, 'GET', args[1:]
while rest:
    arg = rest.pop(0)
    if arg in ('-q', '--jq'):
        query = rest.pop(0)
    elif arg == '-X':
        method = rest.pop(0)
    elif arg in ('-f', '-F'):
        rest.pop(0)
    elif path is None:
        path = arg
route = routes.get(path if method == 'GET' else f'{method} {path}')
if route is None:
    sys.stderr.write('gh: Not Found (HTTP 404)\n')
    sys.exit(1)
if isinstance(route, int):
    sys.stderr.write(f'gh: Server Error (HTTP {route})\n')
    sys.exit(1)
out = json.dumps(route)
if query:
    out = subprocess.run(['jq', '-r', query], input=out, capture_output=True, text=True, check=True).stdout
sys.stdout.write(out)
"""


def green_runs(branch, names=('CI', 'qa')):
    return [{'name': n, 'head_branch': branch, 'status': 'completed', 'conclusion': 'success',
             'created_at': '2026-09-01T00:00:00Z'} for n in names]


@unittest.skipUnless(shutil.which('jq'), 'jq is required to emulate gh --jq')
class GhHarness(unittest.TestCase):
    def gh(self, routes, script, env=None, files=None):
        temp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, temp)
        (temp / 'gh.py').write_text(FAKE_GH)
        (temp / 'routes.json').write_text(json.dumps(routes))
        (temp / 'tmp').mkdir()
        for name, text in (files or {}).items():
            (temp / 'tmp' / name).write_text(text)
        full = dict(os.environ, FAKE_GH=str(temp / 'gh.py'),
                    FAKE_GH_ROUTES=str(temp / 'routes.json'), FAKE_GH_LOG=str(temp / 'gh.log'), **(env or {}))
        script = 'gh() { python3 "$FAKE_GH" "$@"; }\n' + script.replace('/tmp/', f"{temp / 'tmp'}/")
        result = subprocess.run(['bash', '-e', '-c', script], capture_output=True, text=True, env=full)
        log = (temp / 'gh.log').read_text() if (temp / 'gh.log').exists() else ''
        return result, log


class ReleaseEvidenceTests(unittest.TestCase):
    def test_missing_platform_and_stale_dependencies_fail(self):
        code = embedded("          python3 - <<'PYMEDIA'\n", '          PYMEDIA')
        repos = ['freemkv', 'libfreemkv', 'freemkv-engine', 'freemkv-keysources',
                 'freemkv-i18n', 'freemkv-unlock']
        names = ['compare', 'compare-known-answers', 'compare-real']
        names += [f'{job} ({os})' for job in ['suite', 'real-media']
                  for os in ['ubuntu-latest', 'macos-latest', 'windows-latest']]
        for scenario in ['valid', 'missing-platform', 'stale-dependency', 'missing-dependency',
                         'unexpected-dependency', 'failure', 'cancelled', 'skipped']:
            revisions = {r: 'a' * 40 for r in repos}
            jobs = [{'name': n, 'conclusion': 'success'} for n in names]
            if scenario == 'missing-platform':
                jobs.pop()
            if scenario == 'stale-dependency':
                revisions['libfreemkv'] = 'b' * 40
            if scenario == 'missing-dependency':
                del revisions['freemkv-engine']
            if scenario == 'unexpected-dependency':
                revisions['bdemu'] = 'a' * 40
            if scenario in ('failure', 'cancelled', 'skipped'):
                jobs[-1]['conclusion'] = scenario
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
            git('config', 'user.email', 'test@example.com')
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
            git('checkout', 'qa')
            self.assertEqual(git('rev-parse', 'HEAD'), target)
            before = git('ls-remote', '--heads', 'origin')
            result = align()
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('dev advanced outside this release', result.stderr)
            self.assertEqual(git('ls-remote', '--heads', 'origin'), before)
            extra = git('commit-tree', target + '^{tree}', '-p', target, '-m', 'concurrent promotion')
            git('push', 'origin', extra + ':refs/heads/qa')
            before = git('ls-remote', '--heads', 'origin')
            result = align()
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('qa changed', result.stderr)
            self.assertEqual(git('ls-remote', '--heads', 'origin'), before)



class ReleasePreflightTests(GhHarness):
    STEP = 'Preflight — qa green on all eight, main coherent, media evidence'
    NON_GATING = '^(Dependabot Updates|CodeQL|Scorecard|pages-build-deployment|release-orchestrate|Release|ci-runner-launch|ci-runner-sweeper)$'

    def routes(self, qa='a' * 40):
        routes = {}
        for r in REPOS:
            for branch in ('qa', 'main', 'dev'):
                routes[f'repos/freemkv/{r}/git/ref/heads/{branch}'] = {'object': {'sha': qa}}
            routes[f'repos/freemkv/{r}/actions/runs?head_sha={qa}&per_page=100'] = {'workflow_runs': green_runs('qa')}
        return routes

    def preflight(self, routes):
        return self.gh(routes, run_block('release-orchestrate.yml', self.STEP),
                       env={'SKIP_MEDIA': 'true', 'NON_GATING': self.NON_GATING})

    def test_green_and_coherent_set_passes(self):
        result, _ = self.preflight(self.routes())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_dev_ahead_of_qa_is_refused_before_any_tag(self):
        routes = self.routes()
        routes['repos/freemkv/freemkv/git/ref/heads/dev'] = {'object': {'sha': 'd' * 40}}
        routes[f"repos/freemkv/freemkv/compare/{'a' * 40}...{'d' * 40}"] = {'status': 'ahead'}
        result, _ = self.preflight(routes)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('freemkv: dev is', result.stdout)
        routes[f"repos/freemkv/freemkv/compare/{'a' * 40}...{'d' * 40}"] = {'status': 'behind'}
        result, _ = self.preflight(routes)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_unreadable_main_or_dev_fails_closed(self):
        for branch in ('main', 'dev'):
            routes = self.routes()
            routes[f'repos/freemkv/libfreemkv/git/ref/heads/{branch}'] = 502
            with self.subTest(branch=branch):
                result, _ = self.preflight(routes)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f'libfreemkv: cannot read {branch}', result.stdout)
        routes = self.routes()
        del routes['repos/freemkv/libfreemkv/git/ref/heads/main']
        result, _ = self.preflight(routes)
        self.assertEqual(result.returncode, 0, 'a repo without main is coherent')

    def test_only_the_latest_run_of_each_workflow_gates(self):
        routes = self.routes()
        key = f"repos/freemkv/freemkv/actions/runs?head_sha={'a' * 40}&per_page=100"
        old = {'name': 'deb', 'head_branch': 'qa', 'status': 'completed', 'created_at': '2026-09-01T00:00:00Z'}
        new = dict(old, created_at='2026-09-02T00:00:00Z')
        routes[key] = {'workflow_runs': green_runs('qa') + [dict(new, conclusion='success'),
                                                            dict(old, conclusion='failure')]}
        result, _ = self.preflight(routes)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        routes[key] = {'workflow_runs': green_runs('qa') + [dict(new, conclusion='cancelled'),
                                                            dict(old, conclusion='success')]}
        result, _ = self.preflight(routes)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('not green (deb)', result.stdout)


class PromoteTests(GhHarness):
    STEP = "Check every repo's dev is green and qa can fast-forward"

    def routes(self):
        dev, qa = 'd' * 40, 'a' * 40
        cargo = base64.b64encode(b'[package]\nversion = "1.7.8"\n').decode()
        routes = {}
        for r in REPOS:
            routes[f'repos/freemkv/{r}/git/ref/heads/dev'] = {'object': {'sha': dev}}
            routes[f'repos/freemkv/{r}/git/ref/heads/qa'] = {'object': {'sha': qa}}
            routes[f'repos/freemkv/{r}/git/ref/heads/main'] = {'object': {'sha': qa}}
            routes[f'repos/freemkv/{r}/compare/{qa}...{dev}'] = {'status': 'ahead'}
            routes[f'repos/freemkv/{r}/compare/{dev}...{qa}'] = {'status': 'behind'}
            routes[f'repos/freemkv/{r}/contents/Cargo.toml?ref={dev}'] = {'content': cargo}
            routes[f'repos/freemkv/{r}/actions/runs?head_sha={dev}&per_page=100'] = {'workflow_runs': green_runs('dev')}
        return routes

    def check(self, routes):
        env = {'REPOS': ' '.join(REPOS), 'NON_GATING': '^(promote)$'}
        return self.gh(routes, run_block('promote.yml', self.STEP), env=env)

    def test_green_set_is_ready(self):
        result, _ = self.check(self.routes())
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_unreadable_qa_or_main_fails_closed(self):
        for branch in ('qa', 'main'):
            routes = self.routes()
            routes[f'repos/freemkv/bdemu/git/ref/heads/{branch}'] = 503
            with self.subTest(branch=branch):
                result, _ = self.check(routes)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f'bdemu: cannot read {branch}', result.stdout)

    def test_superseded_failed_run_does_not_block(self):
        routes = self.routes()
        key = f"repos/freemkv/autorip/actions/runs?head_sha={'d' * 40}&per_page=100"
        routes[key]['workflow_runs'][0].update(conclusion='failure')
        routes[key]['workflow_runs'].append(dict(routes[key]['workflow_runs'][0], conclusion='success',
                                                 created_at='2026-09-03T00:00:00Z'))
        result, _ = self.check(routes)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_unchanged_dependents_rerun_qa_against_advanced_dependencies(self):
        step = run_block('promote.yml', 'Promote dev → qa, in dependency order')
        shas = ''.join(f"{r} {'d' * 40} {'ready' if r == 'libfreemkv' else 'already'}\n" for r in REPOS)
        routes = {f'repos/freemkv/{r}/git/ref/heads/qa': {'object': {'sha': 'a' * 40}} for r in REPOS}
        routes['PATCH repos/freemkv/libfreemkv/git/refs/heads/qa'] = {'object': {'sha': 'd' * 40}}
        result, log = self.gh(routes, step, files={'shas': shas})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        dispatched = [line for line in log.splitlines() if line.startswith('workflow run')]
        downstream = REPOS[REPOS.index('libfreemkv') + 1:]
        expected = [f'workflow run qa.yml --repo freemkv/{r} --ref qa' for r in downstream]
        expected.append('workflow run hash-matrix.yml --repo freemkv/freemkv --ref qa')
        self.assertEqual(sorted(dispatched), sorted(expected))
        result, log = self.gh(routes, step, files={'shas': shas.replace('ready', 'already')})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn('workflow run', log)


class CascadeHelperTests(unittest.TestCase):
    PRELUDE = ('say() { echo "$*"; }; ok() { echo "$*"; }; warn() { echo "$*" >&2; }\n'
               'err() { echo "$*" >&2; }; die() { err "$*"; exit 1; }\n')

    def bash(self, body, *args, cwd=None):
        return subprocess.run(['bash', '-c', 'set -euo pipefail\n' + self.PRELUDE + body, 'test', *args],
                              capture_output=True, text=True, cwd=cwd)

    def repo(self):
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root)
        remote, checkout = root / 'remote.git', root / 'freemkv'
        subprocess.run(['git', 'init', '-q', '--bare', str(remote)], check=True)
        subprocess.run(['git', 'clone', '-q', str(remote), str(checkout)], check=True, capture_output=True)
        git_in(checkout, 'checkout', '-q', '-b', 'qa')
        return checkout

    def test_missing_changelog_heading_explains_itself(self):
        checkout = self.repo()
        (checkout / 'CHANGELOG.md').write_text('# Changelog\n\n## [1.7.7] — 2026-09-25\n\n- notes\n')
        body = cascade_function('check_changelog_ready') + '\ncheck_changelog_ready "$1" 1.7.8\n'
        result = self.bash(body, str(checkout))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("freemkv: CHANGELOG.md has no '## [1.7.8]' heading", result.stderr)

    def test_missing_cargo_version_explains_itself(self):
        checkout = self.repo()
        (checkout / 'Cargo.toml').write_text('[package]\nname = "freemkv"\n')
        body = 'DRY_RUN=false\n' + cascade_function('bump_cargo_toml_version') + '\nbump_cargo_toml_version "$1" 1.7.8\n'
        result = self.bash(body, str(checkout))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not find a top-level 'version", result.stderr)

    def test_dated_metainfo_reaches_the_tagged_commit(self):
        checkout = self.repo()
        metainfo = checkout / 'packaging/flatpak/org.freemkv.FreeMKV.metainfo.xml'
        metainfo.parent.mkdir(parents=True)
        metainfo.write_text('<releases>\n</releases>\n')
        (checkout / 'Cargo.toml').write_text('[package]\nversion = "1.7.7"\n')
        git_in(checkout, 'add', '-A')
        git_in(checkout, 'commit', '-q', '-m', 'base')
        git_in(checkout, 'push', '-q', 'origin', 'qa')
        (checkout / 'Cargo.toml').write_text('[package]\nversion = "1.7.8"\n')
        metainfo.write_text('<releases>\n    <release version="1.7.8" date="2026-09-27"/>\n</releases>\n')
        body = ('DRY_RUN=false; PRERELEASE=false; WORK_BRANCH=qa; VERSION=1.7.8\n'
                'git() { command git -c core.hooksPath=/nonexistent -c commit.gpgsign=false -c tag.gpgsign=false '
                '-c user.email=test@example.com -c user.name=Release "$@"; }\n'
                + cascade_function('git_commit_push_tag') + '\ngit_commit_push_tag "$1" freemkv "v1.7.8: bump" false\n')
        result = self.bash(body, str(checkout))
        self.assertEqual(result.returncode, 0, result.stderr)
        tagged = git_in(checkout, 'show', 'v1.7.8:packaging/flatpak/org.freemkv.FreeMKV.metainfo.xml')
        self.assertIn('<release version="1.7.8" date="2026-09-27"/>', tagged)
        self.assertEqual(git_in(checkout, 'status', '--porcelain', '-uno'), '')

    def test_prerelease_refuses_a_qa_stamped_candidate_tag(self):
        checkout = self.repo()
        git_in(checkout, 'commit', '-q', '--allow-empty', '-m', 'base')
        git_in(checkout, 'tag', 'v1.7.8-rc1')
        git_in(checkout, 'tag', '-a', 'v1.7.8-rc2', '-m', 'v1.7.8-rc2')
        git_in(checkout, 'push', '-q', 'origin', 'qa', 'v1.7.8-rc1', 'v1.7.8-rc2')
        function = cascade_function('refuse_qa_candidate_tag')
        for version, refused in (('1.7.8-rc1', True), ('1.7.8-rc2', False), ('1.7.8-rc3', False)):
            with self.subTest(version=version):
                result = self.bash(f'VERSION={version}\n{function}\nrefuse_qa_candidate_tag "$1"\n', str(checkout))
                self.assertEqual(result.returncode != 0, refused, result.stderr)
                if refused:
                    self.assertIn('qa.yml candidate stamp', result.stderr)


class SuiteDiagnosticsTests(unittest.TestCase):
    def run_suite(self, workflow, step, os_name, files, env):
        temp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, temp)
        for name, text in files.items():
            path = temp / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
            path.chmod(0o755)
        script = run_block(workflow, step).replace('${{ matrix.os }}', os_name)
        result = subprocess.run(['bash', '--noprofile', '--norc', '-eo', 'pipefail', '-c', script],
                                capture_output=True, text=True, cwd=temp, env=dict(os.environ, LF_SHA='abc1234', **env))
        return result, temp

    def test_qa_acceptance_failure_still_reports_and_records(self):
        files = {'freemkv/target/release/freemkv': '',
                 'ci/scripts/cli-acceptance.sh': 'echo "  FAIL dvd remux"\nexit 3\n'}
        env = {'FMKV_KEY_URL': 'u', 'FMKV_KEY_AUTH': 'a', 'ISO_DIR': '/nonexistent', 'KEYDB_PATH': '/nonexistent'}
        result, temp = self.run_suite('qa.yml', 'Run the full CLI acceptance suite', 'linux', files, env)
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertIn('FAIL dvd remux', result.stdout.split('every failing check', 1)[-1])
        self.assertIn('CLI acceptance failed on linux', result.stdout)
        self.assertIn('libfreemkv=abc1234', (temp / 'cli-linux.txt').read_text())

    def test_hash_matrix_suite_failure_still_records(self):
        files = {'freemkv/tests/cli-integration.sh': 'echo "h  a.mkv" >> "$FMKV_HASHES"\nexit 4\n'}
        result, temp = self.run_suite('hash-matrix.yml', 'Run the CLI suite against the shared media',
                                      'ubuntu-latest', files, {})
        self.assertEqual(result.returncode, 4, result.stderr)
        self.assertIn('hashes recorded on ubuntu-latest', result.stdout)
        self.assertIn('libfreemkv=abc1234', (temp / 'hashes-ubuntu-latest.txt').read_text())


if __name__ == '__main__':
    unittest.main()
