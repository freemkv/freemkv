"""qa.yml wiring of the media gate (design v4 §3.2, §10.1 group S) and its media_gate commands."""

import base64
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
import unittest.mock

sys.path.insert(0, str(Path(__file__).parent))
import media_gate as mg  # noqa: E402
from test_media_gate import Evidence, RUN_ID, SIB_SHA, lock_text  # noqa: E402

ROOT = Path(__file__).parents[1]
QA = ROOT / '.github/workflows/qa.yml'
POLICY = mg.load_policy()


def load(path=QA):
    import yaml
    return yaml.safe_load(path.read_text())


def steps_text(job):
    return '\n'.join(json.dumps(s) for s in job.get('steps', []))


@unittest.skipUnless(shutil.which('python3'), 'python3')
class StructureTests(unittest.TestCase):
    """Group S."""

    @classmethod
    def setUpClass(cls):
        try:
            cls.qa = load()
        except ImportError:
            raise unittest.SkipTest('PyYAML is not installed')
        cls.jobs = cls.qa['jobs']
        cls.text = QA.read_text()

    def test_rc_tag_only_on_qa_pushes(self):
        self.assertEqual(self.jobs['rc-tag']['if'], "github.event_name == 'push' && github.ref_name == 'qa'")

    def test_ui_dependency_guard_in_the_ec2_legs(self):
        self.assertIn('The CLI build has no UI dependencies', steps_text(self.jobs['cli-matrix']))

    def test_every_ec2_runs_on_includes_the_run_label(self):
        for name, job in self.jobs.items():
            runs_on = job.get('runs-on')
            if isinstance(runs_on, list) and 'self-hosted' in runs_on:
                with self.subTest(job=name):
                    self.assertIn('run-${{ github.run_id }}', runs_on)

    def test_no_leg_needs_its_launch_job(self):
        needs = self.jobs['cli-matrix']['needs']
        self.assertNotIn('launch', needs)
        self.assertEqual(self.jobs['launch']['needs'], needs, 'launch and its leg start together')

    def test_launch_has_deadline_watch_cancel_and_teardown(self):
        launch = self.jobs['launch']
        names = [s.get('name', '') for s in launch['steps']]
        for want in ('Watch the leg', 'Cancel the run (no runner is coming)', 'Tear down', 'Still the newest candidate'):
            self.assertIn(want, names)
        watch = next(s for s in launch['steps'] if s.get('name') == 'Watch the leg')['run']
        self.assertIn('deadline', watch)
        self.assertIn('actions/runs/$RUN_ID/cancel', watch)
        self.assertIn('picked=true', watch, 'the deadline must stop applying once the leg is picked up')
        self.assertIn('[ "$picked" = false ]', watch)
        self.assertIn('sleep 30; continue', watch, 'a failed poll is retried, not fatal')
        cancel = next(s for s in launch['steps'] if s.get('name') == 'Cancel the run (no runner is coming)')
        self.assertEqual(cancel['if'], "failure() && steps.watch.outcome != 'failure'")
        self.assertGreater(names.index('Cancel the run (no runner is coming)'), names.index('Watch the leg'))
        teardown = next(s for s in launch['steps'] if s.get('name') == 'Tear down')
        self.assertTrue(teardown['if'].startswith('always()'))
        self.assertIn('terminate-instances', teardown['run'])
        self.assertIn('Name=tag:launched-by,Values=$RUN_ID', teardown['run'])
        self.assertIn('Name=tag:runner-labels,Values=freemkv-media,$OS,run-$RUN_ID', teardown['run'])
        launch_run = next(s for s in launch['steps'] if s.get('name') == 'Launch the ephemeral runner')['run']
        self.assertIn('run-" + $id', launch_run)
        self.assertIn('launched-by', launch_run)
        self.assertIn('--user-data "file://$userdata"', launch_run)
        self.assertIn('Version=$version', launch_run)

    def test_launch_attests_and_leg_uploads_survive_reruns(self):
        launch_run = next(s for s in self.jobs['launch']['steps'] if s.get('name') == 'Launch the ephemeral runner')['run']
        self.assertIn('launch-$OS-$ATTEMPT.json', launch_run)
        for job in ('launch', 'cli-matrix'):
            for step in self.jobs[job]['steps']:
                if 'upload-artifact' in step.get('uses', ''):
                    with self.subTest(job=job, name=step['with']['name']):
                        self.assertTrue(step['with'].get('overwrite'))
        record = self.jobs['record-media-evidence']
        self.assertIn('launch', record['needs'])
        self.assertIn("needs.launch.result == 'success'", record['if'])
        self.assertIn('launch-*', [st.get('with', {}).get('pattern') for st in record['steps']])

    def test_concurrency_is_per_candidate(self):
        self.assertIn('${{ github.sha }}', self.qa['concurrency']['group'])
        self.assertTrue(self.qa['concurrency']['cancel-in-progress'])

    def test_no_floating_toolchain_or_unlocked_build(self):
        self.assertNotRegex(self.text, r'toolchain: stable|@stable')
        for cmd in re.findall(r'\bcargo (?:build|test|tree)\b[^\n]*', self.text):
            with self.subTest(cmd=cmd):
                self.assertIn('--locked', cmd)

    def test_no_branch_resolved_siblings(self):
        self.assertNotIn('checkout-siblings', self.text)
        for name in ('release-tests', 'windows-build', 'cli-integration', 'cli-matrix'):
            with self.subTest(job=name):
                text = steps_text(self.jobs[name])
                self.assertIn('media_gate.py checkout', text)
                self.assertIn('media_gate.py restore', text)
                self.assertIn('plan-media', self.jobs[name]['needs'])

    def test_media_jobs_gate_on_the_plan(self):
        for name in ('launch', 'cli-matrix', 'compare-cli-matrix', 'record-media-evidence'):
            with self.subTest(job=name):
                self.assertIn("needs.plan-media.outputs.run == 'true'", self.jobs[name]['if'])
                self.assertNotIn('inputs.run_media', self.jobs[name]['if'])
        self.assertEqual(self.jobs['media-verdict']['if'], 'always()')

    def test_only_rc_tag_and_record_write_contents(self):
        writers = {n for n, j in self.jobs.items() if (j.get('permissions') or {}).get('contents') == 'write'}
        self.assertEqual(writers, {'rc-tag', 'record-media-evidence'})
        self.assertEqual(self.qa['permissions'], {'contents': 'read'})

    def test_no_single_stream_iso_fetch(self):
        self.assertNotIn('s3api get-object', self.text)
        self.assertNotRegex(self.text, r'aws s3 (cp|sync)')

    def test_harness_comes_from_the_plan(self):
        self.assertNotRegex(self.text, r'ref: [0-9a-f]{40}')
        self.assertIn('${{ needs.plan-media.outputs.harness }}', self.text)

    def test_the_legs_fetch_the_planned_fixture_pins(self):
        self.assertIn("json.load(open('media-plan/evidence.json'))['inputs']['externals']", self.text)

    def test_third_party_actions_are_pinned(self):
        for use in re.findall(r'uses: ([^\s]+)', self.text):
            if use.startswith(('actions/', './')):
                continue
            with self.subTest(use=use):
                self.assertRegex(use, r'@[0-9a-f]{40}$')

    def test_user_data_registers_with_the_run_labels(self):
        linux = (ROOT / '.github/runner-templates/user-data-linux.sh').read_text()
        windows = (ROOT / '.github/runner-templates/user-data-windows.ps1').read_text()
        self.assertIn('tags/instance/runner-labels', linux)
        self.assertIn('--labels $LABELS', linux)
        self.assertIn("trap 'shutdown -h now' EXIT", linux)
        self.assertIn('tags/instance/runner-labels', windows)
        self.assertIn('--labels $labels', windows)
        self.assertIn('ephemeral-windows-$iid', windows)
        self.assertIn('finally', windows)
        self.assertRegex('ephemeral-windows-i-0123456789abcdef0', POLICY['runner_name_re'])


class LeakGuardTests(unittest.TestCase):
    """The media gate files must pass the org leak-guard's generic net."""

    # Built from pieces so this file does not itself trip the net it mirrors.
    _TLDS = ['inter' + 'nal', 'lo' + 'cal', 'l' + 'an', 'co' + 'rp', 'inva' + 'lid']
    NET = re.compile('|'.join([r'\b1' + r'0\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}',
                               r'\b17' + r'2\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3}',
                               r'\b19' + r'2\.168\.[0-9]{1,3}\.[0-9]{1,3}',
                               '/Us' + 'ers/[A-Za-z0-9._-]+/', '/ho' + 'me/[A-Za-z0-9._-]+/']
                              + [r'\.' + t + r'\b' for t in _TLDS]))

    def test_no_leak_guard_pattern(self):
        files = [p for p in (ROOT / 'tests').glob('*media*') if p.is_file()]
        files += [ROOT / '.github/workflows/qa.yml', ROOT / '.github/scripts/exact-toolchain.sh']
        files += list((ROOT / '.github/runner-templates').iterdir())
        for path in files:
            with self.subTest(path=path.name):
                self.assertIsNone(self.NET.search(path.read_text(errors='replace')))


class PinTests(unittest.TestCase):
    def routes(self, qa_head='f' * 40):
        r = {f'repos/freemkv/{repo}/commits/dev': {'sha': SIB_SHA[repo]} for repo in mg.SIBLINGS}
        r['repos/freemkv/freemkv/git/ref/heads/qa'] = {'object': {'sha': qa_head}}
        return r

    def test_sibling_tips_on_dev(self):
        routes = self.routes()
        revisions, superseded = mg.pin('dev', SIB_SHA['freemkv'], '', routes.__getitem__)
        self.assertEqual(revisions, SIB_SHA)
        self.assertFalse(superseded)

    def test_superseded_on_qa(self):
        routes = {f'repos/freemkv/{repo}/commits/qa': {'sha': SIB_SHA[repo]} for repo in mg.SIBLINGS}
        routes['repos/freemkv/freemkv/git/ref/heads/qa'] = {'object': {'sha': 'e' * 40}}
        self.assertTrue(mg.pin('qa', SIB_SHA['freemkv'], '', routes.__getitem__)[1])
        routes['repos/freemkv/freemkv/git/ref/heads/qa'] = {'object': {'sha': SIB_SHA['freemkv']}}
        self.assertFalse(mg.pin('qa', SIB_SHA['freemkv'], '', routes.__getitem__)[1])

    def test_dispatched_revisions_must_be_on_the_branch(self):
        routes = self.routes()
        for repo in mg.SIBLINGS:
            routes[f'repos/freemkv/{repo}/compare/{SIB_SHA[repo]}...dev'] = {'status': 'ahead'}
        self.assertEqual(mg.pin('dev', SIB_SHA['freemkv'], json.dumps(SIB_SHA), routes.__getitem__)[0], SIB_SHA)
        routes['repos/freemkv/libfreemkv/compare/' + SIB_SHA['libfreemkv'] + '...dev'] = {'status': 'diverged'}
        with self.assertRaises(ValueError):
            mg.pin('dev', SIB_SHA['freemkv'], json.dumps(SIB_SHA), routes.__getitem__)
        with self.assertRaises(ValueError):
            mg.pin('dev', 'a' * 40, json.dumps(SIB_SHA), routes.__getitem__)
        with self.assertRaises(ValueError):
            mg.pin('feature-x', SIB_SHA['freemkv'], '', routes.__getitem__)


class ExternalsTests(unittest.TestCase):
    def fake_aws(self, calls):
        def run(cmd, **kw):
            calls.append(cmd)
            args = cmd[1:]
            if args[:2] == ['s3api', 'head-object']:
                key = args[args.index('--key') + 1]
                if '--part-number' in args:
                    out = {'ContentLength': 8388608, 'PartsCount': 3}
                elif key == 'keydb.cfg':
                    out = {'ETag': '"abc"', 'ContentLength': 10}
                else:
                    out = {'ETag': '"def-3"', 'ContentLength': 20000000, 'VersionId': 'null'}
            else:
                name = args[args.index('--launch-template-name') + 1]
                out = {'LaunchTemplateVersions': [{'VersionNumber': 3, 'LaunchTemplateData': {
                    'ImageId': 'ami-' + name[-5:], 'InstanceType': 'c7i.4xlarge', 'UserData': 'eA=='}}]}
            return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps(out), stderr='')
        return run

    def test_fixture_pins_carry_the_part_layout(self):
        calls = []
        pins = mg.read_externals('fixtures', POLICY, 'bucket', run=self.fake_aws(calls))
        self.assertEqual(set(pins), set(POLICY['fixtures']))
        self.assertEqual(pins['bd.iso'], {'etag': '"def-3"', 'size': 20000000, 'version_id': None,
                                          'part_size': 8388608, 'parts': 3})
        self.assertNotIn('part_size', pins['keydb.cfg'])
        self.assertTrue(any('--if-match' in c for c in calls))

    def test_template_pins_are_the_default_version_and_perf_only_when_enabled(self):
        calls = []
        pins = mg.read_externals('launch_templates', POLICY, run=self.fake_aws(calls))
        self.assertEqual(set(pins), {'freemkv-runner-linux', 'freemkv-runner-windows'})
        self.assertEqual(pins['freemkv-runner-linux']['version'], 3)
        self.assertTrue(all('$Default' in c for c in calls))

    def test_unreadable_externals_are_recorded_not_fatal(self):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, d)
        out = d / 'ext.json'
        with unittest.mock.patch.object(mg, 'read_externals', side_effect=RuntimeError('AccessDenied')):
            self.assertEqual(mg.main(['externals', '--part', 'fixtures', '--bucket', 'b', '--out', str(out)]), 0)
        self.assertIn('error', json.loads(out.read_text())['fixtures'])


class LegAndRecordTests(unittest.TestCase):
    def test_leg_identity(self):
        run = lambda cmd, **kw: subprocess.CompletedProcess(cmd, 0, stdout='rustc 1.98.0\nrelease: 1.98.0\n', stderr='')
        meta = {'instance-id': 'i-0123456789abcdef0', 'instance-type': 'c7i.4xlarge'}.__getitem__
        rec = mg.leg_identity('linux', 'x86_64-unknown-linux-musl', 'musl-gcc: 13\n',
                              {'RUNNER_NAME': 'ephemeral-linux-i-0123456789abcdef0'}, run=run, meta=meta)
        self.assertEqual((rec['rustc_release'], rec['instance_id'], rec['instance_type'], rec['c_toolchain']),
                         ('1.98.0', 'i-0123456789abcdef0', 'c7i.4xlarge', 'musl-gcc: 13'))

    def setup_record(self):
        e = Evidence(self)
        d = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, d)
        plan = d / 'plan'
        legs = d / 'legs'
        plan.mkdir()
        legs.mkdir()
        ev = dict(e.ev)
        legs_rec = ev.pop('legs')
        (plan / 'evidence.json').write_text(json.dumps(ev))
        (plan / 'Cargo.lock').write_bytes(e.lock)
        for leg, rec in legs_rec.items():
            rec = dict(rec)
            rec.pop('launched_by')
            rec['runner_name'] = f'ephemeral-{leg}-i-0{"1" if leg == "linux" else "2"}23456789abcdef0'
            rec['instance_id'] = rec['runner_name'].rsplit('-', 2)[-2] + '-' + rec['runner_name'].rsplit('-', 1)[-1]
            (legs / f'leg-{leg}.json').write_text(json.dumps(rec))
            next(j for j in e.jobs if j['name'] == mg.LEG_JOB[leg])['runner_name'] = rec['runner_name']
            e.ev['legs'][leg].update(runner_name=rec['runner_name'], instance_id=rec['instance_id'])
            att = {'instance_id': rec['instance_id'], 'launched_by': RUN_ID, 'run_attempt': 1, 'os': leg,
                   'runner_labels': f'freemkv-media,{leg},run-{RUN_ID}', 'launch_template': f'freemkv-runner-{leg}',
                   'launch_template_version': '3'}
            (legs / f'launch-{leg}-1.json').write_text(json.dumps(att))
        e.jobs = [j for j in e.jobs if j['name'] != 'record-media-evidence']
        tags = {'launched-by': str(RUN_ID)}

        def aws(*a):
            if tags.get('gone'):
                raise subprocess.CalledProcessError(254, a, 'InvalidInstanceID.NotFound')
            os_name = 'linux' if a[-1].startswith('i-01') else 'windows'
            t = dict(tags, **{'runner-labels': tags.get('labels', f'freemkv-media,{os_name},run-{RUN_ID}')})
            return {'Reservations': [{'Instances': [{'Tags': [{'Key': k, 'Value': v} for k, v in t.items()]}]}]}
        posts = []

        def post(endpoint, body):
            posts.append((endpoint, body))
            return {'sha': f's{len(posts)}'}
        env = {'GITHUB_RUN_ID': str(RUN_ID), 'GITHUB_SHA': SIB_SHA['freemkv']}
        return e, plan, legs, env, aws, post, posts, tags

    def test_record_writes_an_orphan_commit_and_annotated_tag(self):
        e, plan, legs, env, aws, post, posts, _ = self.setup_record()
        name, commit = mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertEqual(name, f'media-evidence/{e.f}/{RUN_ID}')
        kinds = [p[0].rsplit('/', 1)[1] for p in posts]
        self.assertEqual(kinds, ['blobs', 'blobs', 'trees', 'commits', 'tags', 'refs'])
        self.assertEqual(posts[3][1]['parents'], [])
        self.assertEqual(posts[5][1]['ref'], f'refs/tags/{name}')
        written = json.loads(base64.b64decode(posts[0][1]['content']))
        self.assertEqual(written['legs']['linux']['launched_by'], RUN_ID)
        self.assertEqual(base64.b64decode(posts[1][1]['content']), e.lock)

    def test_record_refuses_a_leg_not_launched_by_this_run(self):
        e, plan, legs, env, aws, post, posts, tags = self.setup_record()
        tags['launched-by'] = str(RUN_ID + 1)
        with self.assertRaises(ValueError):
            mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertEqual(posts, [])

    def test_record_uses_the_launch_record_once_ec2_forgets_the_instance(self):
        e, plan, legs, env, aws, post, posts, tags = self.setup_record()
        tags['gone'] = True
        name, _ = mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertTrue(name.startswith('media-evidence/'))

    def test_record_refuses_a_leg_without_a_matching_launch_record(self):
        for change in ('delete', 'os', 'run'):
            with self.subTest(change=change):
                e, plan, legs, env, aws, post, posts, tags = self.setup_record()
                path = legs / 'launch-windows-1.json'
                att = json.loads(path.read_text())
                if change == 'delete':
                    path.unlink()
                elif change == 'os':
                    att.update(os='linux', runner_labels=f'freemkv-media,linux,run-{RUN_ID}')
                    path.write_text(json.dumps(att))
                else:
                    att['launched_by'] = RUN_ID + 1
                    path.write_text(json.dumps(att))
                with self.assertRaises(ValueError):
                    mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
                self.assertEqual(posts, [])

    def test_record_refuses_ec2_labels_for_another_os(self):
        e, plan, legs, env, aws, post, posts, tags = self.setup_record()
        tags['labels'] = f'freemkv-media,linux,run-{RUN_ID}'
        with self.assertRaises(ValueError):
            mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)

    def test_existing_tag_from_an_earlier_attempt_is_success(self):
        calls = []

        def post(endpoint, body):
            calls.append(endpoint)
            if endpoint.endswith('/refs'):
                raise subprocess.CalledProcessError(1, 'gh', output='{"message":"Reference already exists"}')
            return {'sha': 'x'}
        name, _ = mg.write_evidence_tag('f' * 64, RUN_ID, b'{}', b'', post=post)
        self.assertTrue(name.endswith(f'/{RUN_ID}'))

        def post_fail(endpoint, body):
            if endpoint.endswith('/refs'):
                raise subprocess.CalledProcessError(1, 'gh', output='{"message":"Resource not accessible"}')
            return {'sha': 'x'}
        with self.assertRaises(subprocess.CalledProcessError):
            mg.write_evidence_tag('f' * 64, RUN_ID, b'{}', b'', post=post_fail)

    def test_job_named_accepts_matrix_suffixes(self):
        jobs = [{'name': 'cli-matrix (linux, x86_64-unknown-linux-musl)'}, {'name': 'cli-matrix (windows)'}]
        self.assertEqual(mg.job_named(jobs, 'cli-matrix (linux)')['name'], jobs[0]['name'])
        self.assertEqual(mg.job_named(jobs, 'cli-matrix (windows)')['name'], 'cli-matrix (windows)')
        self.assertIsNone(mg.job_named([{'name': 'cli-matrix (linuxx)'}], 'cli-matrix (linux)'))

    def test_record_refuses_a_leg_built_with_another_toolchain(self):
        e, plan, legs, env, aws, post, posts, _ = self.setup_record()
        rec = json.loads((legs / 'leg-linux.json').read_text())
        rec['rustc_release'] = '1.98.1'
        (legs / 'leg-linux.json').write_text(json.dumps(rec))
        with self.assertRaises(ValueError):
            mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertEqual(posts, [])

    def test_record_refuses_a_runner_mismatch_or_another_candidate(self):
        e, plan, legs, env, aws, post, posts, _ = self.setup_record()
        rec = json.loads((legs / 'leg-windows.json').read_text())
        rec['runner_name'] = 'ephemeral-windows-i-0fedcba9876543210'
        (legs / 'leg-windows.json').write_text(json.dumps(rec))
        with self.assertRaises(ValueError):
            mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        e, plan, legs, env, aws, post, posts, _ = self.setup_record()
        with self.assertRaises(ValueError):
            mg.record(plan, legs, POLICY, dict(env, GITHUB_SHA='f' * 40), request=e.request, aws=aws, post=post)
        self.assertEqual(posts, [])


class VerdictTests(unittest.TestCase):
    def test_i1(self):
        ok = {'PLAN_RESULT': 'success'}
        cases = [
            (dict(ok, STATUS='reuse', EVIDENCE_URL='u'), True),
            (dict(ok, STATUS='run', MATRIX='success', COMPARE='success', RECORD='success'), True),
            (dict(ok, STATUS='run', MATRIX='failure', COMPARE='success', RECORD='skipped'), False),
            (dict(ok, STATUS='run', MATRIX='success', COMPARE='success', RECORD='failure'), False),
            (dict(ok, STATUS='run', MATRIX='cancelled', COMPARE='cancelled', RECORD='skipped'), False),
            (dict(ok, STATUS='waived'), False),
            (dict(ok, STATUS='superseded'), False),
            (dict(ok, STATUS='canary-failed'), False),
            ({'PLAN_RESULT': 'failure', 'STATUS': 'reuse'}, False),
            ({'PLAN_RESULT': 'success', 'STATUS': ''}, False),
        ]
        for env, green in cases:
            with self.subTest(env=env):
                self.assertEqual(mg.verdict(env)[0], green)

    def test_reason_cannot_issue_workflow_commands(self):
        import io
        import contextlib
        import os
        out = io.StringIO()
        env = {'PLAN_RESULT': 'success', 'STATUS': 'waived', 'REASON': '::add-mask::x ::stop-commands::t'}
        with unittest.mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(out):
            self.assertEqual(mg.main(['verdict']), 1)
        body = out.getvalue().split('::error', 1)[0]
        self.assertNotIn('::add-mask::', body)
        self.assertNotIn('::stop-commands::', body)

    def test_reuse_says_why(self):
        green, lines = mg.verdict({'PLAN_RESULT': 'success', 'STATUS': 'reuse', 'EVIDENCE_URL': 'https://x/1',
                                   'REASON': 'full-disc not needed: …'})
        self.assertTrue(green)
        self.assertTrue(any('https://x/1' in line for line in lines))


if __name__ == '__main__':
    import unittest.mock  # noqa: F401
    unittest.main()
