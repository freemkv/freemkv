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
        self.assertIn('state="unknown none"', watch, 'a failed poll is retried, not fatal')
        cancel = next(s for s in launch['steps'] if s.get('name') == 'Cancel the run (no runner is coming)')
        self.assertEqual(cancel['if'], "failure() && steps.watch.outcome != 'failure'")
        self.assertGreater(names.index('Cancel the run (no runner is coming)'), names.index('Watch the leg'))
        teardown = next(s for s in launch['steps'] if s.get('name') == 'Tear down')
        self.assertTrue(teardown['if'].startswith('always()'))
        self.assertIn('terminate-instances', teardown['run'])
        self.assertIn('{Name: "tag:launched-by", Values: [$r]}', teardown['run'])
        self.assertIn('--filters "$filters"', teardown['run'])
        self.assertIn('{Name: "tag:runner-labels", Values: [$l]}', teardown['run'])
        self.assertNotRegex(teardown['run'], r'Name=tag:runner-labels,Values=[^"]', 'shorthand splits the value on commas')
        launch_run = next(s for s in launch['steps'] if s.get('name') == 'Launch the ephemeral runner')['run']
        self.assertIn('{Key: "runner-labels", Value: $l}', launch_run)
        self.assertIn('launched-by', launch_run)
        self.assertIn('--user-data "file://$userdata"', launch_run)
        self.assertIn('Version=$version', launch_run)

    def test_launch_attests_and_leg_uploads_survive_reruns(self):
        launch_run = next(s for s in self.jobs['launch']['steps'] if s.get('name') == 'Launch the ephemeral runner')['run']
        self.assertIn('launch-$LEG-$ATTEMPT.json', launch_run)
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
        self.assertIn('ephemeral-windows$kind-$iid', windows)
        self.assertIn('ephemeral-linux$KIND-$IID', linux)
        self.assertIn('finally', windows)
        self.assertRegex('ephemeral-windows-i-0123456789abcdef0', POLICY['runner_name_re'])


class CanaryWiringTests(unittest.TestCase):
    """Decision 15 in CI: plan-media runs the canary on qa and the plan reads its result."""

    @classmethod
    def setUpClass(cls):
        try:
            cls.plan = load()['jobs']['plan-media']
        except ImportError:
            raise unittest.SkipTest('PyYAML is not installed')
        cls.names = [s.get('name', s.get('uses', '')) for s in cls.plan['steps']]

    def test_canary_step(self):
        step = next(s for s in self.plan['steps'] if s.get('name') == 'Key-service canary')
        self.assertEqual(step['if'], "github.ref == 'refs/heads/qa'",
                         'where the gate runs: the qa branch, never dev, never a tag named qa')
        self.assertTrue(step['continue-on-error'], 'a crash leaves no result, which the plan fails closed on')
        self.assertEqual(step['env']['FMKV_KEY_URL'], '${{ secrets.FMKV_KEY_URL }}')
        self.assertEqual(step['env']['FMKV_KEY_AUTH'], '${{ secrets.FMKV_KEY_AUTH }}')
        self.assertIn('media_canary.py --bucket "$B" --externals externals.json --out canary.json', step['run'])
        self.assertIn('--require-hashes', step['run'])

    def test_a_canary_failure_fails_plan_media(self):
        gate = next(s for s in self.plan['steps'] if s.get('name') == 'Key-service canary verdict')
        self.assertEqual(gate['if'], "steps.plan.outputs.status == 'canary-failed'")
        self.assertIn('exit 1', gate['run'])
        self.assertGreater(self.names.index('Key-service canary verdict'), self.names.index('Plan'))
        upload = next(s for s in self.plan['steps'] if s.get('with', {}).get('name') == 'media-plan')
        self.assertTrue(upload['with']['overwrite'], '"Re-run failed jobs" re-uploads the plan')
        self.assertLess(self.plan['steps'].index(upload), self.names.index('Key-service canary verdict'))

    def test_canary_reads_with_the_fixtures_role_before_the_plan(self):
        roles = [i for i, s in enumerate(self.plan['steps']) if 'configure-aws-credentials' in s.get('uses', '')]
        canary = self.names.index('Key-service canary')
        self.assertTrue(roles[0] < canary < roles[1], 'after the fixtures role, before the runner role')
        plan = next(s for s in self.plan['steps'] if s.get('name') == 'Plan')
        self.assertIn('--canary canary.json', plan['run'])
        self.assertGreater(self.names.index('Plan'), canary)


class LaunchWiringTests(unittest.TestCase):
    """No manual AWS step: perf legs, the Spot cap and the token parameter all live in the launch job."""

    @classmethod
    def setUpClass(cls):
        try:
            cls.jobs = load()['jobs']
        except ImportError:
            raise unittest.SkipTest('PyYAML is not installed')
        cls.launch = cls.jobs['launch']
        cls.text = '\n'.join(s.get('run', '') for s in cls.launch['steps'])

    def step(self, name):
        return next(s for s in self.launch['steps'] if s.get('name') == name)

    def test_superseded_check_uses_the_branch_ref(self):
        step = self.step('Still the newest candidate')
        self.assertEqual(step['env']['REF'], '${{ github.ref }}')
        self.assertIn('[ "$REF" = refs/heads/qa ]', step['run'])
        self.assertIn('git/ref/heads/qa', step['run'])

    def test_legs_come_from_the_plan(self):
        self.assertIn('needs.plan-media.outputs.legs', self.launch['strategy']['matrix']['leg'])
        self.assertEqual(self.jobs['plan-media']['outputs']['legs'], '${{ steps.plan.outputs.legs }}')

    def test_everything_run_instances_gets_comes_from_the_spec(self):
        self.assertIn('media_gate.py launch-spec --leg "$LEG"', self.step('Launch spec')['run'])
        run = self.step('Launch the ephemeral runner')['run']
        for want in ('--instance-market-options "$options"', '--block-device-mappings "$bdm"',
                     "jq -c '.markets[]'", '--instance-type "$t"'):
            self.assertIn(want, run)
        self.assertNotIn("MarketType=spot", run, 'the market (and its price cap) comes from the policy')
        self.assertNotRegex(run, r'c[67]i\.4xlarge', 'instance types come from the policy')
        self.assertNotRegex(self.text, r'freemkv-runner-[a-z]+-perf', 'no separate perf launch templates')

    def test_token_parameter_comes_from_the_spec_and_is_not_deleted_by_the_launcher(self):
        self.assertNotIn('delete-parameter', '\n'.join(json.dumps(s) for s in self.launch['steps']))
        mint = self.step('Mint a registration token and stash it in SSM (SecureString)')
        self.assertIn('steps.spec.outputs.param', mint['env']['PARAM'])
        linux = (ROOT / '.github/runner-templates/user-data-linux.sh').read_text()
        self.assertIn('aws ssm delete-parameter', linux, 'the instance deletes the token after reading it')

    def test_teardown_matches_the_legs_labels(self):
        teardown = self.step('Tear down')
        self.assertEqual(teardown['env']['LABELS'], '${{ steps.spec.outputs.labels }}')
        self.assertEqual(self.step('Watch the leg')['env']['JOB'], '${{ steps.spec.outputs.job }}')


class LaunchSpecTests(unittest.TestCase):
    TEMPLATES = {'freemkv-runner-linux': {'version': 7, 'image_id': 'ami-l', 'instance_type': 'c7i.4xlarge'},
                 'freemkv-runner-windows': {'version': 5, 'image_id': 'ami-w', 'instance_type': 'm7i.4xlarge'}}

    def perf_policy(self):
        import copy
        policy = copy.deepcopy(POLICY)
        policy['perf']['enabled'] = True
        return policy

    def fake_aws(self, calls):
        def aws(*args):
            calls.append(args)
            return {'LaunchTemplateVersions': [{'LaunchTemplateData': {'BlockDeviceMappings': [
                {'DeviceName': '/dev/sda1', 'Ebs': {'VolumeSize': 600, 'VolumeType': 'gp3', 'Iops': 6000,
                                                   'Throughput': 500, 'DeleteOnTermination': True}}]}}]}
        return aws

    def test_only_the_functional_templates_are_pinned(self):
        self.assertEqual(mg.launch_templates(POLICY), ['freemkv-runner-linux', 'freemkv-runner-windows'])
        self.assertEqual(mg.launch_templates(self.perf_policy()), mg.launch_templates(POLICY))

    def test_linux_spot_carries_the_policy_cap(self):
        spec = mg.launch_spec(POLICY, 'linux', RUN_ID, 'refs/heads/qa', self.TEMPLATES, aws=None)
        self.assertEqual((spec['template'], spec['version']), ('freemkv-runner-linux', '7'))
        self.assertEqual([m['name'] for m in spec['markets']], ['spot'],
                         'the live Linux template carries Spot market options, so no On-Demand fallback')
        spot = spec['markets'][0]['options']
        self.assertEqual(spot['MarketType'], 'spot')
        self.assertEqual(spot['SpotOptions']['MaxPrice'], POLICY['launch']['linux']['spot_max_price'])
        self.assertEqual(spec['types'], POLICY['launch']['linux']['types'])
        self.assertIsNone(spec['block_device_mappings'])
        self.assertEqual(spec['labels'], f'freemkv-media,linux,run-{RUN_ID}')
        self.assertEqual(spec['job'], 'cli-matrix (linux)')

    def on_demand_policy(self):
        import copy
        policy = copy.deepcopy(POLICY)
        policy['launch']['linux']['on_demand_fallback'] = True
        return policy

    def template_aws(self, market):
        def aws(*args):
            data = {'BlockDeviceMappings': []}
            if market:
                data['InstanceMarketOptions'] = {'MarketType': 'spot', 'SpotOptions': {'MaxPrice': '0.72'}}
            return {'LaunchTemplateVersions': [{'LaunchTemplateData': data}]}
        return aws

    def test_on_demand_passes_no_market_options(self):
        """Review FB4: '{}' did not clear the template's Spot options; On-Demand passes none."""
        spec = mg.launch_spec(self.on_demand_policy(), 'linux', RUN_ID, 'refs/heads/qa', self.TEMPLATES,
                              aws=self.template_aws(False))
        self.assertEqual(spec['markets'][1], {'name': 'on-demand', 'options': None})

    def test_on_demand_refuses_a_template_with_market_options(self):
        with self.assertRaisesRegex(ValueError, 'InstanceMarketOptions'):
            mg.launch_spec(self.on_demand_policy(), 'linux', RUN_ID, 'refs/heads/qa', self.TEMPLATES,
                           aws=self.template_aws(True))
        templates = {'freemkv-runner-linux': {'version': 1, 'market_options': True},
                     'freemkv-runner-windows': {'version': 3, 'market_options': False}}
        self.assertEqual(len(mg.check_launch_markets(self.on_demand_policy(), templates)), 1)
        self.assertEqual(mg.check_launch_markets(POLICY, templates), [], 'Spot-only overrides the template')

    def test_externals_record_the_template_market(self):
        def run(cmd, **kw):
            name = cmd[cmd.index('--launch-template-name') + 1]
            data = {'ImageId': 'ami-1', 'InstanceType': 'c7i.4xlarge'}
            if name.endswith('linux'):
                data['InstanceMarketOptions'] = {'MarketType': 'spot'}
            return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps(
                {'LaunchTemplateVersions': [{'VersionNumber': 1, 'LaunchTemplateData': data}]}), stderr='')
        pins = mg.read_externals('launch_templates', POLICY, run=run)
        self.assertEqual({k: v['market_options'] for k, v in pins.items()},
                         {'freemkv-runner-linux': True, 'freemkv-runner-windows': False})

    def test_windows_uses_its_template_market_and_type(self):
        spec = mg.launch_spec(POLICY, 'windows', RUN_ID, 'refs/heads/qa', self.TEMPLATES)
        self.assertEqual(spec['markets'], [{'name': 'template', 'options': None}])
        self.assertEqual(spec['types'], [])
        self.assertTrue(spec['user_data'].endswith('user-data-windows.ps1'))

    def test_perf_legs_override_the_functional_template(self):
        policy = self.perf_policy()
        self.assertEqual(mg.legs(policy), ['linux', 'windows', 'linux-perf', 'windows-perf'])
        for leg, template, version in (('linux-perf', 'freemkv-runner-linux', '7'),
                                       ('windows-perf', 'freemkv-runner-windows', '5')):
            with self.subTest(leg=leg):
                calls = []
                spec = mg.launch_spec(policy, leg, RUN_ID, 'refs/heads/qa', self.TEMPLATES, aws=self.fake_aws(calls))
                self.assertEqual((spec['template'], spec['version']), (template, version))
                self.assertEqual(spec['types'], [policy['perf']['instance_type']], 'one type, no fallback')
                self.assertEqual(spec['labels'], f'freemkv-media-perf,{leg.split("-")[0]},run-{RUN_ID}')
                self.assertEqual(spec['job'], f'cli-perf ({leg.split("-")[0]})')
                root = spec['block_device_mappings'][0]
                self.assertEqual(root['Ebs']['VolumeSize'], policy['perf']['root_volume_gib'])
                self.assertEqual((root['DeviceName'], root['Ebs']['Throughput']), ('/dev/sda1', 500),
                                 'every other template setting of the volume is kept')
                self.assertEqual(calls, [('ec2', 'describe-launch-template-versions', '--launch-template-name',
                                          template, '--versions', version)])

    def test_perf_legs_only_when_enabled(self):
        with self.assertRaises(ValueError):
            mg.launch_spec(POLICY, 'linux-perf', RUN_ID, 'refs/heads/qa', self.TEMPLATES)

    def test_one_parameter_per_run_attempt_and_leg(self):
        """Review FB5: overlapping runs never share a token parameter."""
        a = mg.launch_spec(POLICY, 'linux', RUN_ID, 'refs/heads/qa', self.TEMPLATES)['param']
        self.assertEqual(a, f'/freemkv-ci/runner-reg/{RUN_ID}-1-linux')
        self.assertNotEqual(a, mg.launch_spec(POLICY, 'linux', RUN_ID + 1, 'refs/heads/qa', self.TEMPLATES)['param'])
        self.assertNotEqual(a, mg.launch_spec(POLICY, 'linux', RUN_ID, 'refs/heads/qa', self.TEMPLATES, attempt=2)['param'])
        self.assertNotEqual(a, mg.launch_spec(POLICY, 'windows', RUN_ID, 'refs/heads/qa', self.TEMPLATES)['param'])
        # Both roles grant exactly arn:...:parameter/freemkv-ci/runner-reg/* (checked 2026-09-27).
        self.assertRegex(a, r'^/freemkv-ci/runner-reg/[0-9]+-[0-9]+-[a-z-]+$')

    def test_dev_and_other_branches_launch_nothing(self):
        """Review FB6: only qa runs record evidence, so only qa launches."""
        for ref in ('refs/heads/dev', 'refs/heads/main', 'refs/tags/qa', 'qa', 'refs/heads/qa2'):
            with self.subTest(ref=ref):
                with self.assertRaises(ValueError):
                    mg.launch_spec(POLICY, 'linux', RUN_ID, ref, self.TEMPLATES)

    def test_unpinned_templates_launch_the_default_version(self):
        spec = mg.launch_spec(POLICY, 'linux', RUN_ID, 'refs/heads/qa', {'freemkv-runner-linux': {'error': 'AccessDenied'}})
        self.assertEqual(spec['version'], '$Default')

    def test_cli_prints_one_json_line(self):
        import io
        import contextlib
        import os
        out = io.StringIO()
        env = {'GITHUB_RUN_ID': str(RUN_ID), 'GITHUB_REF': 'refs/heads/qa', 'GITHUB_RUN_ATTEMPT': '2',
               'TEMPLATES': json.dumps(self.TEMPLATES)}
        with unittest.mock.patch.dict(os.environ, env, clear=True), contextlib.redirect_stdout(out):
            self.assertEqual(mg.main(['launch-spec', '--leg', 'linux']), 0)
        self.assertEqual(len(out.getvalue().strip().splitlines()), 1)
        self.assertEqual(json.loads(out.getvalue())['version'], '7')
        self.assertEqual(json.loads(out.getvalue())['param'], f'/freemkv-ci/runner-reg/{RUN_ID}-2-linux')

    def test_record_expects_the_perf_label_on_perf_legs(self):
        self.assertEqual(mg.leg_labels('linux-perf', 5), 'freemkv-media-perf,linux,run-5')
        self.assertEqual(mg.leg_labels('windows', 5), 'freemkv-media,windows,run-5')
        self.assertRegex('ephemeral-linux-perf-i-0123456789abcdef0', POLICY['runner_name_re'])


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
    TIPS = {repo: f'{9 - i:x}' * 40 for i, repo in enumerate(mg.SIBLINGS)}

    def routes(self, branch='dev', qa_head='f' * 40):
        # "Get a commit": ref "Can be a commit SHA, branch name (`heads/BRANCH_NAME`), or tag name
        # (`tags/TAG_NAME`)." Only the heads/ form is served, so a bare name (which a tag could
        # shadow) would 404.
        r = {f'repos/freemkv/{repo}/commits/heads/{branch}': {'sha': SIB_SHA[repo]} for repo in mg.SIBLINGS}
        r['repos/freemkv/freemkv/git/ref/heads/qa'] = {'object': {'sha': qa_head}}
        return r

    def test_sibling_tips_on_dev(self):
        routes = self.routes()
        revisions, superseded = mg.pin('refs/heads/dev', SIB_SHA['freemkv'], '', routes.__getitem__)
        self.assertEqual(revisions, SIB_SHA)
        self.assertFalse(superseded)

    def test_superseded_on_qa(self):
        routes = self.routes('qa', qa_head='e' * 40)
        self.assertTrue(mg.pin('refs/heads/qa', SIB_SHA['freemkv'], '', routes.__getitem__)[1])
        routes['repos/freemkv/freemkv/git/ref/heads/qa'] = {'object': {'sha': SIB_SHA['freemkv']}}
        self.assertFalse(mg.pin('refs/heads/qa', SIB_SHA['freemkv'], '', routes.__getitem__)[1])

    def test_a_tag_named_qa_is_refused(self):
        """Review 2 item 1: a dispatch on refs/tags/qa has GITHUB_REF_NAME "qa" too."""
        routes = self.routes('qa', qa_head=SIB_SHA['freemkv'])
        for ref in ('refs/tags/qa', 'refs/tags/dev', 'qa', 'refs/heads/main', 'refs/heads/qa/x'):
            with self.subTest(ref=ref):
                with self.assertRaises(ValueError):
                    mg.pin(ref, SIB_SHA['freemkv'], '', routes.__getitem__)

    def test_dispatched_revisions_must_be_on_the_branch(self):
        routes = self.routes()
        for repo in mg.SIBLINGS:
            routes[f'repos/freemkv/{repo}/commits/heads/dev'] = {'sha': self.TIPS[repo]}
            # compare between shas only: no branch/tag name for a tag to shadow.
            routes[f'repos/freemkv/{repo}/compare/{SIB_SHA[repo]}...{self.TIPS[repo]}'] = {'status': 'ahead'}
        self.assertEqual(mg.pin('refs/heads/dev', SIB_SHA['freemkv'], json.dumps(SIB_SHA), routes.__getitem__)[0], SIB_SHA)
        routes[f'repos/freemkv/libfreemkv/compare/{SIB_SHA["libfreemkv"]}...{self.TIPS["libfreemkv"]}'] = {'status': 'diverged'}
        with self.assertRaises(ValueError):
            mg.pin('refs/heads/dev', SIB_SHA['freemkv'], json.dumps(SIB_SHA), routes.__getitem__)
        with self.assertRaises(ValueError):
            mg.pin('refs/heads/dev', 'a' * 40, json.dumps(SIB_SHA), routes.__getitem__)
        with self.assertRaises(ValueError):
            mg.pin('refs/heads/feature-x', SIB_SHA['freemkv'], '', routes.__getitem__)


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
                raise subprocess.CalledProcessError(254, a, output='', stderr=tags['gone'])
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
        tags['gone'] = 'An error occurred (InvalidInstanceID.NotFound) when calling DescribeInstances'
        name, _ = mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertTrue(name.startswith('media-evidence/'))
        e, plan, legs, env, aws, post, posts, tags = self.setup_record()
        tags['gone'] = 'An error occurred (UnauthorizedOperation) when calling DescribeInstances'
        with self.assertRaises(subprocess.CalledProcessError):
            mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertEqual(posts, [])

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

    def existing_tag(self, tagger, f='f' * 64, run_id=RUN_ID):
        """A fake API holding refs/tags/media-evidence/<f>/<run_id> written by `tagger`."""
        name = f'media-evidence/{"f" * 64}/{RUN_ID}'
        blob = lambda b: {'content': base64.b64encode(b).decode()}
        routes = {
            f'repos/freemkv/freemkv/git/ref/tags/{name}': {'ref': f'refs/tags/{name}', 'object': {'sha': 't9', 'type': 'tag'}},
            'repos/freemkv/freemkv/git/tags/t9': {'tag': name, 'object': {'sha': 'c9', 'type': 'commit'},
                                                  'tagger': dict(tagger, date='2026-09-20T01:00:00Z')},
            'repos/freemkv/freemkv/git/commits/c9': {'parents': [], 'tree': {'sha': 'tr9'}},
            'repos/freemkv/freemkv/git/trees/tr9': {'tree': [
                {'path': 'evidence.json', 'type': 'blob', 'sha': 'b9', 'size': 1},
                {'path': 'Cargo.lock', 'type': 'blob', 'sha': 'b8', 'size': 1}]},
            'repos/freemkv/freemkv/git/blobs/b9': blob(json.dumps({'fingerprint': f, 'run_id': run_id}).encode()),
            'repos/freemkv/freemkv/git/blobs/b8': blob(b''),
        }
        return routes.__getitem__

    def test_existing_tag_from_an_earlier_attempt_is_success(self):
        calls = []

        def post(endpoint, body):
            calls.append((endpoint, body))
            if endpoint.endswith('/refs'):
                raise subprocess.CalledProcessError(1, 'gh', output='{"message":"Reference already exists"}')
            return {'sha': 'x'}
        name, commit = mg.write_evidence_tag('f' * 64, RUN_ID, b'{}', b'', post=post,
                                             request=self.existing_tag(mg.ACTIONS_BOT))
        self.assertTrue(name.endswith(f'/{RUN_ID}'))
        self.assertEqual(commit, 'c9', 'the standing tag\'s commit is the evidence')

        def post_fail(endpoint, body):
            if endpoint.endswith('/refs'):
                raise subprocess.CalledProcessError(1, 'gh', output='{"message":"Resource not accessible"}')
            return {'sha': 'x'}
        with self.assertRaises(subprocess.CalledProcessError):
            mg.write_evidence_tag('f' * 64, RUN_ID, b'{}', b'', post=post_fail)

    def test_a_forged_tag_squatting_the_name_is_refused(self):
        def post(endpoint, body):
            if endpoint.endswith('/refs'):
                raise subprocess.CalledProcessError(1, 'gh', output='{"message":"Reference already exists"}')
            return {'sha': 'x'}
        person = {'name': 'Someone', 'email': 'someone@users.noreply.github.com'}
        for label, request in (('person', self.existing_tag(person)),
                               ('another F', self.existing_tag(mg.ACTIONS_BOT, f='e' * 64)),
                               ('another run', self.existing_tag(mg.ACTIONS_BOT, run_id=RUN_ID + 1))):
            with self.subTest(label):
                with self.assertRaises(ValueError):
                    mg.write_evidence_tag('f' * 64, RUN_ID, b'{}', b'', post=post, request=request)

    def test_the_writer_names_the_actions_bot_as_tagger(self):
        posts = []

        def post(endpoint, body):
            posts.append((endpoint, body))
            return {'sha': f's{len(posts)}'}
        import datetime
        now = datetime.datetime(2026, 9, 20, 1, 0, tzinfo=datetime.timezone.utc)
        mg.write_evidence_tag('f' * 64, RUN_ID, b'{}', b'', post=post, now=now)
        tag = next(b for e, b in posts if e.endswith('/tags'))
        self.assertEqual(tag['tagger'], dict(mg.ACTIONS_BOT, date='2026-09-20T01:00:00Z'))

    def test_record_refuses_a_run_off_the_qa_branch(self):
        e, plan, legs, env, aws, post, posts, _ = self.setup_record()
        e.run.update(head_branch='dev', event='workflow_dispatch')
        with self.assertRaises(ValueError):
            mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertEqual(posts, [])

    def test_record_refuses_a_run_on_a_tag_named_qa(self):
        """head_branch "qa" from a tag: the qa branch does not contain the commit, so nothing is written."""
        e, plan, legs, env, aws, post, posts, _ = self.setup_record()
        e.run.update(event='workflow_dispatch', head_branch='qa', status='in_progress', conclusion=None)
        e.qa_compare.update(status='diverged', ahead_by=4)
        with self.assertRaisesRegex(ValueError, 'not on the qa branch'):
            mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertEqual(posts, [])

    def test_record_runs_inside_its_own_unfinished_run(self):
        e, plan, legs, env, aws, post, posts, _ = self.setup_record()
        e.run.update(status='in_progress', conclusion=None)
        name, _ = mg.record(plan, legs, POLICY, env, request=e.request, aws=aws, post=post)
        self.assertTrue(name.startswith('media-evidence/'))

    def test_job_named_accepts_matrix_suffixes(self):
        jobs = [{'name': 'cli-matrix (linux, x86_64-unknown-linux-musl)'}, {'name': 'cli-matrix (windows)'}]
        self.assertEqual(mg.job_named(jobs, 'cli-matrix (linux)')['name'], jobs[0]['name'])
        self.assertEqual(mg.job_named(jobs, 'cli-matrix (windows)')['name'], 'cli-matrix (windows)')
        self.assertIsNone(mg.job_named([{'name': 'cli-matrix (linuxx)'}], 'cli-matrix (linux)'))

    def test_record_refuses_a_leg_built_with_another_toolchain(self):
        e, plan, legs, env, aws, post, posts, _ = self.setup_record()
        rec = json.loads((legs / 'leg-linux.json').read_text())
        rec['rustc_release'] = '1.98.2'
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
            ({'PLAN_RESULT': 'failure', 'STATUS': 'canary-failed'}, False),
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
