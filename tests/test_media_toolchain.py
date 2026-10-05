"""The exact toolchain (TC) and clean build environment for evidence and shipped builds."""

from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).parents[1]
SCRIPT = ROOT / '.github/scripts/exact-toolchain.sh'
WORKFLOWS = ROOT / '.github/workflows'
# Workflows whose builds are media evidence or ship.
PINNED = ['qa.yml', 'hash-matrix.yml', 'release.yml', 'deb.yml', 'appimage.yml']
EXACT = re.compile(r'^\d+\.\d+\.\d+$')


def jobs(workflow):
    """{job: [step text, ...]} from the workflow's two-space job / six-space step layout."""
    out, job, step = {}, None, None
    lines = (WORKFLOWS / workflow).read_text().split('jobs:\n', 1)[1].splitlines()
    for line in lines:
        m = re.match(r'^  ([A-Za-z0-9_-]+):\s*$', line)
        if m:
            job = m.group(1)
            out[job] = []
            continue
        if job is None:
            continue
        if line.startswith('      - '):
            out[job].append(line + '\n')
        elif out[job] and (line.startswith('        ') or not line.strip()):
            out[job][-1] += line + '\n'
    return out


def builds(step):
    return re.search(r'\b(cargo|cross) (build|test)\b', step) is not None


class Sandbox:
    """A temp dir with fake rustup/rustc/cc on PATH ahead of the real ones."""

    def __init__(self, test, rustc_release=None):
        self.dir = Path(tempfile.mkdtemp())
        test.addCleanup(shutil.rmtree, self.dir)
        self.bin = self.dir / 'bin'
        self.bin.mkdir()
        # Hermetic PATH: only the tools the script needs, plus the fakes below.
        for tool in ('bash', 'sed', 'grep', 'sort', 'head', 'tr', 'dirname', 'env', 'cat'):
            (self.bin / tool).symlink_to(shutil.which(tool))
        self.log = self.dir / 'rustup.log'
        self.fake('rustup', f'echo "$@" >> {self.log}\n')
        release = f'"{rustc_release}"' if rustc_release else '"$RUSTUP_TOOLCHAIN"'
        self.fake('rustc', f'echo "rustc x"; echo "release: {release}"\n')
        self.fake('cargo', 'echo "cargo x"\n')
        self.fake('cc', 'echo "fake cc 1.0"\n')
        self.home = self.dir / 'cargo-home'
        self.home.mkdir()
        self.crate = self.dir / 'work' / 'freemkv'
        self.crate.mkdir(parents=True)

    def fake(self, name, body):
        path = self.bin / name
        path.write_text('#!/bin/bash\n' + body)
        path.chmod(0o755)

    def manifest(self, version):
        path = self.crate / 'Cargo.toml'
        path.write_text(f'[package]\nname = "x"\nversion = "1.0.0"\nrust-version = "{version}"\n')
        return path

    def run(self, *args, env=None):
        # HOSTTYPE pinned: bash sets it from its own build, and musl targets depend on it.
        base = dict(PATH=str(self.bin), HOME=str(self.dir), CARGO_HOME=str(self.home), HOSTTYPE='x86_64',
                    GITHUB_ENV=str(self.dir / 'env'), GITHUB_OUTPUT=str(self.dir / 'out'))
        base.update(env or {})
        return subprocess.run([str(self.bin / 'bash'), str(SCRIPT), *args], capture_output=True,
                              text=True, env=base, cwd=self.dir)


class ExactToolchainTests(unittest.TestCase):
    def test_freemkv_pins_an_exact_version(self):
        text = (ROOT / 'Cargo.toml').read_text()
        v = re.search(r'^rust-version\s*=\s*"([^"]*)"', text, re.M).group(1)
        self.assertRegex(v, EXACT)

    def test_inexact_or_missing_versions_are_rejected(self):
        box = Sandbox(self)
        for bad in ('1.98', '1', '1.98.0-beta.1', 'stable', ''):
            with self.subTest(bad=bad):
                r = box.run('install', str(box.manifest(bad)))
                self.assertNotEqual(r.returncode, 0)
                self.assertIn('exact X.Y.Z', r.stderr)
                self.assertFalse(box.log.exists(), 'rustup must not run for an inexact version')

    def test_installs_exports_and_asserts_the_exact_version(self):
        box = Sandbox(self)
        r = box.run('install', str(box.manifest('1.98.0')), 'x86_64-unknown-linux-musl')
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('toolchain install 1.98.0 --profile minimal --no-self-update '
                      '--target x86_64-unknown-linux-musl', box.log.read_text())
        self.assertIn('RUSTUP_TOOLCHAIN=1.98.0', (box.dir / 'env').read_text())
        self.assertIn('version=1.98.0', (box.dir / 'out').read_text())

    def test_a_rustc_reporting_another_release_fails(self):
        box = Sandbox(self, rustc_release='1.98.1')
        r = box.run('install', str(box.manifest('1.98.0')))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("rustc release '1.98.1'", r.stderr)
        self.assertFalse((box.dir / 'env').exists())

    def test_clean_environment_passes_and_records_the_c_toolchain(self):
        box = Sandbox(self)
        r = box.run('assert-env', 'x86_64-unknown-linux-gnu', str(box.crate))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('fake cc 1.0', r.stdout)
        self.assertIn('fake cc 1.0', (box.dir / 'out').read_text())

    def test_each_code_changing_variable_fails(self):
        box = Sandbox(self)
        for name in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS', 'FREEMKV_BUILD_LABEL',
                     'FREEMKV_GH_TOKEN', 'CARGO_PROFILE_RELEASE_LTO', 'CARGO_PROFILE_RELEASE_OPT_LEVEL',
                     'CC', 'CFLAGS', 'CXX', 'CXXFLAGS', 'AR', 'TARGET_CC', 'HOST_CFLAGS',
                     'CC_x86_64_unknown_linux_musl', 'CFLAGS_x86_64-unknown-linux-musl',
                     'CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS', 'RUSTC_WRAPPER',
                     'CL', '_CL_', 'LINK', '_LINK_', 'RANLIB', 'CRATE_CC_NO_DEFAULTS', 'RUSTC_BOOTSTRAP'):
            with self.subTest(name=name):
                r = box.run('assert-env', 'x86_64-unknown-linux-gnu', str(box.crate), env={name: 'x'})
                self.assertNotEqual(r.returncode, 0)
                self.assertIn(name, r.stderr)

    def test_harmless_variables_pass(self):
        box = Sandbox(self)
        env = {'CARGO_INCREMENTAL': '0', 'CARGO_TERM_COLOR': 'always', 'CARGO_PROFILE_DEV_DEBUG': '0',
               'CCACHE_DIR': '/x', 'ARCH': 'x64'}
        r = box.run('assert-env', 'x86_64-unknown-linux-gnu', str(box.crate), env=env)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_cargo_config_that_changes_code_fails(self):
        cases = {
            'crate': ('[build]\nrustflags = ["-C", "target-cpu=native"]\n', True),
            'parent': ('[target.x86_64-unknown-linux-musl]\nlinker = "x"\n', True),
            'home': ('[profile.release]\nlto = false\n', True),
            'env': ('[env]\nFOO = "1"\n', True),
            'dotted-build': ('build.rustflags = ["-Ctarget-cpu=native"]\n', True),
            'dotted-profile': ('profile.release.opt-level = 1\n', True),
            'dotted-target': ('target.x86_64-unknown-linux-musl.linker = "x"\n', True),
            'dotted-env': ('env.CC = "clang"\n', True),
            'inline': ('build = { rustflags = ["x"] }\n', True),
            'include': ('include = "other.toml"\n', True),
            'host': ('[host]\nlinker = "x"\n', True),
            'literal-key': ("[ 'build' ]\nx = 1\n", True),
            'literal-profile': ("['profile'.release]\nlto = false\n", True),
            'net': ('[net]\ngit-fetch-with-cli = true\n', False),
            'patch': ('[patch.crates-io]\nlibfreemkv = { path = "../libfreemkv" }\n', False),
        }
        for where, (text, fails) in cases.items():
            with self.subTest(where=where):
                box = Sandbox(self)
                base = {'parent': box.crate.parent, 'home': None}.get(where, box.crate)
                path = box.home / 'config.toml' if base is None else base / '.cargo' / 'config.toml'
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
                r = box.run('assert-env', 'x86_64-unknown-linux-gnu', str(box.crate))
                if fails:
                    self.assertNotEqual(r.returncode, 0)
                    self.assertIn(str(path), r.stderr)
                else:
                    self.assertEqual(r.returncode, 0, r.stderr)

    def test_musl_needs_musl_gcc_and_records_it(self):
        box = Sandbox(self)
        r = box.run('assert-env', 'x86_64-unknown-linux-musl', str(box.crate))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn('No C toolchain', r.stderr)
        box.fake('musl-gcc', 'echo "gcc (fake) 13.2.0"\n')
        r = box.run('assert-env', 'x86_64-unknown-linux-musl', str(box.crate))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('musl-gcc: gcc (fake) 13.2.0', r.stdout)

    def test_cross_leg_records_its_pinned_image(self):
        box = Sandbox(self)
        image = 'ghcr.io/cross-rs/aarch64-unknown-linux-musl:0.2.5@sha256:abc'
        r = box.run('assert-env', 'aarch64-unknown-linux-musl', str(box.crate),
                    env={'CROSS_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_IMAGE': image})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(image, r.stdout)
        r = box.run('assert-env', 'aarch64-unknown-linux-musl', str(box.crate))
        self.assertNotEqual(r.returncode, 0, 'an unpinned cross image must fail')

    def test_armv7_cross_leg_records_its_pinned_image(self):
        box = Sandbox(self)
        box.fake('musl-gcc', 'echo "gcc (fake) 13.2.0"\n')
        image = 'ghcr.io/cross-rs/armv7-unknown-linux-musleabihf:0.2.5@sha256:abc'
        target = 'armv7-unknown-linux-musleabihf'
        r = box.run('assert-env', target, str(box.crate),
                    env={'CROSS_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_IMAGE': image})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(image, r.stdout)
        r = box.run('assert-env', target, str(box.crate))
        self.assertNotEqual(r.returncode, 0, 'a foreign musl target without a pinned image must fail')
        r = box.run('assert-env', target, str(box.crate),
                    env={'CROSS_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_IMAGE': image.split('@')[0]})
        self.assertNotEqual(r.returncode, 0, 'an unpinned cross image must fail')

    def test_native_musl_on_the_matching_host_uses_musl_gcc(self):
        box = Sandbox(self)
        box.fake('musl-gcc', 'echo "gcc (fake) 13.2.0"\n')
        r = box.run('assert-env', 'aarch64-unknown-linux-musl', str(box.crate), env={'HOSTTYPE': 'aarch64'})
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn('musl-gcc: gcc (fake) 13.2.0', r.stdout)


class WorkflowToolchainTests(unittest.TestCase):
    def test_edited_workflows_are_valid_yaml(self):
        try:
            import yaml
        except ImportError:
            self.skipTest('PyYAML is not installed')
        for wf in PINNED + ['release-orchestrate.yml', 'snap.yml']:
            with self.subTest(wf=wf):
                self.assertIn('jobs', yaml.safe_load((WORKFLOWS / wf).read_text()))

    def test_snap_builds_with_the_exact_toolchain(self):
        snap = (ROOT / 'snap/snapcraft.yaml').read_text()
        cargo = (ROOT / 'Cargo.toml').read_text()
        want = re.search(r'^rust-version\s*=\s*"([^"]*)"', cargo, re.M).group(1)
        self.assertIn(f'- RUST_TOOLCHAIN: "{want}"', snap)

    def test_no_floating_toolchain_in_evidence_or_shipped_builds(self):
        for wf in PINNED:
            text = (WORKFLOWS / wf).read_text()
            with self.subTest(wf=wf):
                self.assertNotIn('toolchain: stable', text)
                self.assertNotRegex(text, r'rust-toolchain@stable|\{ *toolchain: *stable')
                self.assertNotIn('dtolnay/rust-toolchain', text)

    def test_every_build_job_installs_the_exact_toolchain_first(self):
        for wf in PINNED:
            for job, steps in jobs(wf).items():
                idx = [i for i, s in enumerate(steps) if builds(s)]
                if not idx:
                    continue
                with self.subTest(wf=wf, job=job):
                    tc = [i for i, s in enumerate(steps) if 'exact-toolchain.sh install' in s]
                    self.assertTrue(tc, 'no exact-toolchain install step')
                    self.assertLess(tc[0], idx[0])

    def test_shipped_and_evidence_builds_assert_a_clean_environment(self):
        shipped = [('release.yml', 'test'), ('release.yml', 'build'), ('release.yml', 'build-windows'), ('release.yml', 'cli-binaries'),
                   ('deb.yml', 'build'), ('appimage.yml', 'build'), ('qa.yml', 'cli-matrix')]
        for wf, job in shipped:
            steps = jobs(wf)[job]
            with self.subTest(wf=wf, job=job):
                check = [i for i, s in enumerate(steps) if 'exact-toolchain.sh assert-env' in s]
                built = [i for i, s in enumerate(steps) if builds(s)]
                self.assertTrue(check and built)
                self.assertLess(check[-1], built[0])
                for i in range(check[-1] + 1, built[-1] + 1):
                    for sink in ('GITHUB_ENV', 'GITHUB_PATH'):
                        self.assertNotIn(sink, steps[i], f'{sink} changed after the clean-env check')
                for i in built:
                    self.assertNotRegex(steps[i], r'\n        env:', 'a build step sets its own env')

    def test_release_appimage_is_locked(self):
        steps = jobs('appimage.yml')['build']
        cmds = [c for s in steps for c in re.findall(r'cargo build[^\n]*', s)]
        self.assertTrue(cmds)
        for cmd in cmds:
            self.assertIn("inputs.release && '--locked'", cmd)

    def test_release_builds_are_locked(self):
        for job, steps in jobs('release.yml').items():
            for step in steps:
                for cmd in re.findall(r'\b(?:cargo|cross) (?:build|test)\b[^\n]*', step):
                    with self.subTest(job=job, cmd=cmd):
                        self.assertIn('--locked', cmd)

    def test_release_orchestrate_reads_the_exact_version(self):
        text = (WORKFLOWS / 'release-orchestrate.yml').read_text()
        self.assertIn(r'[[ "$V" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]', text)
        self.assertNotIn('TOOLCHAIN="${TOOLCHAIN:-stable}"', text)
        self.assertIn('rustc "+$V" -Vv', text)


if __name__ == '__main__':
    unittest.main()
