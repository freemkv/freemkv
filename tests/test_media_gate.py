"""Unit tests for the qa full-disc media gate (design v4 §10.1 groups C, G, K, D, W, A, L)."""

import base64
import copy
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import unittest.mock

sys.path.insert(0, str(Path(__file__).parent))
import media_gate as mg  # noqa: E402

ROOT = Path(__file__).parents[1]
POLICY = mg.load_policy()
RUN_ID = 424242
QA_TIP = 'd' * 40          # refs/heads/qa of freemkv in the fake API


def _run_start():
    import datetime
    # A week ago, relative to now: evidence expires at max_age_days, so fixed dates would rot.
    return (datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=7)).replace(microsecond=0)


RUN_START = _run_start()


def stamp(base, **delta):
    import datetime
    return (base + datetime.timedelta(**delta)).strftime('%Y-%m-%dT%H:%M:%SZ')
SIB_SHA = {r: f'{i + 1:x}' * 40 for i, r in enumerate(mg.FIRST_PARTY)}

LOCK_PACKAGES = [
    ('freemkv', '1.8.0', None, ['freemkv-engine', 'freemkv-i18n', 'freemkv-keysources', 'libfreemkv',
                                'mimalloc', 'ureq', 'libc', 'gtk4', 'zip']),
    ('libfreemkv', '1.8.0', 'git+https://github.com/freemkv/libfreemkv?tag=v1.8.0#aaaa',
     ['freemkv-unlock', 'serde_json', 'tracing', 'libc']),
    ('freemkv-engine', '1.8.0', 'git+https://github.com/freemkv/freemkv-engine?tag=v1.8.0#bbbb',
     ['libfreemkv', 'serde_json']),
    ('freemkv-keysources', '1.8.0', 'git+https://github.com/freemkv/freemkv-keysources?tag=v1.8.0#cccc',
     ['libfreemkv', 'ureq']),
    ('freemkv-i18n', '1.8.0', 'git+https://github.com/freemkv/freemkv-i18n?tag=v1.8.0#dddd', ['serde_json']),
    ('freemkv-unlock', '1.8.0', 'git+https://github.com/freemkv/freemkv-unlock?tag=v1.8.0#eeee', ['num-bigint']),
    ('serde_json', '1.0.151', 'reg', ['serde']),
    ('serde', '1.0.229', 'reg', []),
    ('tracing', '0.1.44', 'reg', []),
    ('libc', '0.2.189', 'reg', []),
    ('ureq', '3.4.2', 'reg', ['rustls']),
    ('rustls', '0.23.45', 'reg', []),
    ('mimalloc', '0.1.52', 'reg', ['libmimalloc-sys']),
    ('libmimalloc-sys', '0.1.49', 'reg', ['cc']),
    ('cc', '1.5.1', 'reg', []),
    ('num-bigint', '0.5.1', 'reg', []),
    ('gtk4', '0.9.7', 'reg', []),
    ('zip', '8.6.0', 'reg', []),
]


def lock_text(packages=LOCK_PACKAGES, edit=None):
    out = ['version = 4', '']
    for name, version, source, deps in packages:
        if edit:
            name, version, source, deps = edit(name, version, source, deps)
        out += ['[[package]]', f'name = "{name}"', f'version = "{version}"']
        if source == 'reg':
            out += ['source = "registry+https://github.com/rust-lang/crates.io-index"',
                    f'checksum = "{mg.sha256((name + version).encode())}"']
        elif source:
            out.append(f'source = "{source}"')
        if deps:
            out.append('dependencies = [' + ', '.join(f'"{d}"' for d in deps) + ']')
        out.append('')
    return '\n'.join(out)


def bump(target, version):
    def edit(name, v, source, deps):
        return name, (version if name == target else v), source, deps
    return edit


LIB_TOML = '''[package]
name = "{name}"
version = "1.8.0"
edition = "2024"
rust-version = "1.88"

[dependencies]
freemkv-unlock = {{ git = "https://github.com/freemkv/freemkv-unlock", tag = "v1.8.0" }}
serde_json = "1"

[dev-dependencies]
tempfile = "3"
'''

# Verbatim from libfreemkv build.rs (dev 4659ee7) and freemkv-engine src/resolve.rs (dev baac812).
LIBFREEMKV_BUILD = '''fn main() {
    println!("cargo:rerun-if-changed=src/scsi/macos_shim.c");
    println!("cargo:rustc-env=GIT_SUFFIX={suffix}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(ref_path) = std::fs::read_to_string(".git/HEAD") {
        println!("cargo:rerun-if-changed=.git/{}", ref_path.trim());
    }
}
'''
ENGINE_RESOLVE = '''pub fn resolve() {}
#[cfg(test)]
mod tests {
    #[test]
    fn documented() {
        let src = include_str!("resolve.rs");
        let guide = include_str!("../USING_THE_ENGINE.md");
        assert!(guide.contains("resolve") && !src.is_empty());
    }
}
'''


def real(path):
    return (ROOT / path).read_text()


def base_files():
    return {
        'libfreemkv': {'Cargo.toml': LIB_TOML.format(name='libfreemkv'), 'build.rs': LIBFREEMKV_BUILD,
                       'src/lib.rs': 'pub mod udf;\n', 'src/udf.rs': 'pub fn udf() {}\n',
                       'src/scsi/macos_shim.c': 'int x;\n', 'README.md': '# libfreemkv\n',
                       'tests/t.rs': '#[test] fn t() {}\n', 'benches/b.rs': 'fn main() {}\n',
                       'Cargo.lock': 'version = 4\n'},
        'freemkv-engine': {'Cargo.toml': LIB_TOML.format(name='freemkv-engine'),
                           'src/lib.rs': 'pub mod resolve;\npub mod run;\n', 'src/resolve.rs': ENGINE_RESOLVE,
                           'src/run.rs': 'pub fn run() {}\n', 'USING_THE_ENGINE.md': '# resolve\n'},
        'freemkv-keysources': {'Cargo.toml': LIB_TOML.format(name='freemkv-keysources'),
                               'src/lib.rs': 'pub fn keys() {}\n'},
        'freemkv-i18n': {'Cargo.toml': LIB_TOML.format(name='freemkv-i18n'), 'src/lib.rs': 'pub fn t() {}\n'},
        'freemkv-unlock': {'Cargo.toml': LIB_TOML.format(name='freemkv-unlock'), 'src/lib.rs': 'pub fn u() {}\n'},
        'freemkv': {path: real(path) for path in (
            'Cargo.toml', 'build.rs', 'res/freemkv.manifest', 'src/main.rs', 'src/lib.rs', 'src/pipe.rs',
            'src/keydb_fetch.rs', 'src/file_identity.rs', 'src/title_identity.rs', 'src/cli_entry.rs',
            'src/disc_copy_verdict.rs', 'src/sources.rs', 'src/rip_keys.rs', 'src/cli_stop.rs', 'src/artifact_lock.rs',
            'src/plan_core.rs',
            '.github/workflows/qa.yml', 'tests/media_gate.py', 'tests/media_checks.py',
            'tests/media-gate-policy.json',
            *(f for f in POLICY['freemkv_required'] if f.startswith('src/') and f.endswith('_tests.rs')))} | {
            'Cargo.lock': lock_text(), 'src/ui.rs': 'pub fn ui() {}\n', 'res/freemkv.ico': 'ICO',
            'README.md': '# freemkv\n', 'CHANGELOG.md': '# changes\n', 'docs/x.md': 'x\n',
            'packaging/flatpak/org.freemkv.FreeMKV.metainfo.xml': '<x/>\n'},
    }


def git(path, *args):
    return subprocess.check_output(['git', '-C', str(path), '-c', 'user.email=t@example.com', '-c', 'user.name=t',
                                    '-c', 'commit.gpgsign=false', '-c', 'core.hooksPath=/nonexistent',
                                    '-c', 'maintenance.auto=false', '-c', 'gc.auto=0', *args],
                                   text=True, stderr=subprocess.DEVNULL).strip()


def rmtree_quiet(path):
    # CI flake (freemkv-library run 36409482771): an entry under .git/objects vanished mid-rmtree,
    # likely a detached git auto-maintenance; git() now disables it, and a vanished entry is not an error.
    def gone_ok(func, p, exc):
        if not isinstance(exc if isinstance(exc, BaseException) else exc[1], FileNotFoundError):
            raise exc if isinstance(exc, BaseException) else exc[1]
    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=gone_ok)
    else:
        shutil.rmtree(path, onerror=gone_ok)


class Workspace:
    def __init__(self, test, files=None):
        self.root = Path(tempfile.mkdtemp())
        test.addCleanup(rmtree_quiet, self.root)
        self.files = files or base_files()
        for repo, tree in self.files.items():
            (self.root / repo).mkdir()
            git(self.root / repo, 'init', '-q')
            for path, text in tree.items():
                self.write(repo, path, text, commit=False)
            git(self.root / repo, 'add', '-A')
            git(self.root / repo, 'commit', '-q', '-m', 'fixture')

    def write(self, repo, path, text, commit=True):
        target = self.root / repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)
        if commit:
            git(self.root / repo, 'add', '-A')
            git(self.root / repo, 'commit', '-q', '-m', f'edit {path}')

    def edit(self, repo, path, old, new):
        text = (self.root / repo / path).read_text()
        assert old in text, (repo, path, old)
        self.write(repo, path, text.replace(old, new, 1))

    def guards(self, policy=POLICY):
        return mg.run_guards(self.root, policy, (self.root / 'freemkv' / 'Cargo.lock').read_text())

    def inputs(self, policy=POLICY, features=None, externals=None):
        lock = (self.root / 'freemkv' / 'Cargo.lock').read_text()
        return mg.required_inputs(self.root, policy, lock, features if features is not None else FEATURES,
                                  externals if externals is not None else EXTERNALS)

    def f(self, **kw):
        return mg.fingerprint(self.inputs(**kw))


FEATURES = {'x86_64-unknown-linux-musl': [['serde_json', '1.0.151', 'default,std'], ['libfreemkv', '', 'rip']]}
EXTERNALS = {
    'fixtures': {k: {'version_id': f'v-{k}', 'etag': f'"{k}"'} for k in POLICY['fixtures']},
    'launch_templates': {k: {'version': 3, 'image_id': 'ami-1', 'instance_type': 'c7i.4xlarge',
                             'user_data_sha256': 'u'} for k in mg.launch_templates(POLICY)},
}


def with_policy(**changes):
    policy = copy.deepcopy(POLICY)
    for key, value in changes.items():
        policy[key] = value
    return policy


# ── C: classification ──────────────────────────────────────────────────────

class ClassificationTests(unittest.TestCase):
    def test_required(self):
        cases = [('freemkv', p) for p in ('src/pipe.rs', 'src/keydb_fetch.rs', 'src/file_identity.rs',
                                          'src/title_identity.rs', 'src/disc_copy_verdict.rs', 'build.rs',
                                          'res/freemkv.manifest',
                                          '.github/workflows/qa.yml', 'tests/media_gate.py',
                                          'tests/media-gate-policy.json', 'tests/media_checks.py',
                                          '.github/runner-templates/user-data-linux.sh',
                                          '.github/scripts/exact-toolchain.sh')]
        cases += [(r, 'assets/x') for r in POLICY['libraries']]
        cases += [('libfreemkv', 'src/udf.rs'), ('libfreemkv', 'build.rs'), ('freemkv-engine', 'src/run.rs'),
                  ('freemkv-engine', 'USING_THE_ENGINE.md'), ('freemkv-keysources', 'src/lib.rs')]
        for repo, path in cases:
            with self.subTest(repo=repo, path=path):
                self.assertEqual(mg.classify(repo, path, POLICY), 'required')

    def test_projected(self):
        self.assertEqual(mg.classify('freemkv', 'src/main.rs', POLICY), 'root')
        self.assertEqual(mg.classify('freemkv', 'src/lib.rs', POLICY), 'root')
        for repo in list(POLICY['libraries']) + ['freemkv']:
            self.assertEqual(mg.classify(repo, 'Cargo.toml', POLICY), 'manifest')

    def test_not_required(self):
        cases = [('freemkv', p) for p in ('src/cli_entry.rs', 'src/ui.rs', 'src/disc_info.rs', 'res/freemkv.ico',
                                          'assets/x', 'Cargo.lock', 'README.md', 'docs/x.md',
                                          'packaging/flatpak/org.freemkv.FreeMKV.metainfo.xml',
                                          '.github/workflows/ci.yml', 'tests/cli_tests.rs')]
        cases += [('freemkv-i18n', 'src/lib.rs'), ('freemkv-unlock', 'src/lib.rs'), ('bdemu', 'src/main.rs'),
                  ('autorip', 'src/main.rs')]
        cases += [('libfreemkv', p) for p in ('tests/t.rs', 'benches/b.rs', 'README.md', 'Cargo.lock',
                                              '.github/workflows/ci.yml', 'LICENSE', 'docs/x.md')]
        for repo, path in cases:
            with self.subTest(repo=repo, path=path):
                self.assertIsNone(mg.classify(repo, path, POLICY))

    def test_manifest_fields(self):
        ws = Workspace(self)
        base = ws.f()
        for old, new in (('edition = "2024"', 'edition = "2021"'),
                         ('[package]\n', '[package]\nresolver = "3"\n'),
                         ('[package]\n', '[package]\nbuild = "build2.rs"\n'),
                         ('[package]\n', '[package]\nlinks = "x"\n'),
                         ('rust-version = "1.98.1"', 'rust-version = "1.98.2"')):
            with self.subTest(new=new):
                ws.edit('freemkv', 'Cargo.toml', old, new)
                self.assertNotEqual(ws.f(), base)
                ws.edit('freemkv', 'Cargo.toml', new, old)
        ws.edit('freemkv', 'Cargo.toml', 'description = ', 'description = "changed" #')
        self.assertEqual(ws.f(), base)


# ── G: guards ──────────────────────────────────────────────────────────────

class GuardTests(unittest.TestCase):
    def assert_fires(self, ws, code, text=None, policy=POLICY):
        errors = ws.guards(policy)
        self.assertTrue(any(e.startswith(code) and (text is None or text in e) for e in errors), errors)

    def test_clean_fixture_passes(self):
        self.assertEqual(Workspace(self).guards(), [])

    def test_g1_path_moved_pipe(self):
        ws = Workspace(self)
        ws.edit('freemkv', 'src/main.rs', '\nmod pipe;', '\n#[path = "rip/pipe2.rs"]\nmod pipe;')
        self.assert_fires(ws, 'G1', 'mod pipe')

    def test_g1_cfg_gated_or_missing_module(self):
        ws = Workspace(self)
        ws.edit('freemkv', 'src/main.rs', '\nmod pipe;', '\n#[cfg(feature = "gui")]\nmod pipe;')
        self.assert_fires(ws, 'G1', 'cfg')
        ws = Workspace(self)
        ws.edit('freemkv', 'src/main.rs', '\nmod pipe;', '\n')
        self.assert_fires(ws, 'G1', '`pipe` is not declared')

    def test_g1_nested_decl_cannot_hide_a_moved_top_level_one(self):
        ws = Workspace(self)
        ws.edit('freemkv', 'src/main.rs', '\nmod pipe;', '\n#[path = "rip/pipe2.rs"]\nmod pipe;\n'
                '#[cfg(test)]\nmod t {\n    mod pipe;\n}')
        self.assert_fires(ws, 'G1', 'mod pipe')

    def test_g1_lib_root_declarations_are_plain_too(self):
        ws = Workspace(self)
        ws.edit('freemkv', 'src/lib.rs', 'pub mod keydb_fetch;', '#[path = "x.rs"]\npub mod keydb_fetch;')
        self.assert_fires(ws, 'G1', 'src/lib.rs')

    def test_g1_include_and_out_of_line_mod_in_required_file(self):
        ws = Workspace(self)
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\ninclude!("../x.rs");\n')
        self.assert_fires(ws, 'G1', 'include!')
        ws = Workspace(self)
        ws.write('freemkv', 'src/title_identity.rs', real('src/title_identity.rs') + '\nmod helper;\n')
        self.assert_fires(ws, 'G1', 'mod helper')

    def test_g1_cfg_test_side_file_must_itself_be_required(self):
        ws = Workspace(self)
        self.assertEqual(ws.guards(), [])
        ws.write('freemkv', 'src/title_identity.rs', real('src/title_identity.rs') +
                 '\n#[cfg(test)]\n#[path = "title_identity_tests.rs"]\nmod tests;\n')
        ws.write('freemkv', 'src/title_identity_tests.rs', 'use super::*;\n')
        self.assert_fires(ws, 'G1', 'mod tests')
        self.assert_fires(ws, 'G1', 'title_identity_tests.rs')
        policy = with_policy(freemkv_required=POLICY['freemkv_required'] + ['src/title_identity_tests.rs'])
        self.assertEqual(ws.guards(policy), [])
        ws.write('freemkv', 'src/title_identity.rs', real('src/title_identity.rs') +
                 '\n#[path = "title_identity_tests.rs"]\nmod tests;\n')
        self.assertTrue(any('G1' in e for e in ws.guards(policy)))

    def test_g2_library_path_and_include_targets(self):
        ws = Workspace(self)
        ws.write('libfreemkv', 'src/lib.rs', 'pub mod udf;\n#[path = "../benches/c.rs"]\nmod c;\n')
        self.assert_fires(ws, 'G2', 'benches/c.rs')
        ws = Workspace(self)
        ws.write('libfreemkv', 'src/udf.rs', 'const G: &str = include_str!("../examples/g.rs");\n')
        self.assert_fires(ws, 'G2', 'examples/g.rs')
        ws = Workspace(self)
        ws.write('libfreemkv', 'src/udf.rs', 'const G: &[u8] = include_bytes!("../../outside");\n')
        self.assert_fires(ws, 'G2', 'outside the repo')

    def test_g2_non_literal_target_needs_a_required_build_script(self):
        ws = Workspace(self)
        ws.write('libfreemkv', 'src/udf.rs', 'include!(concat!(env!("OUT_DIR"), "/gen.rs"));\n')
        self.assertEqual(ws.guards(), [])
        ws.write('libfreemkv', 'src/udf.rs', 'include!(concat!(env!("HOME"), "/gen.rs"));\n')
        self.assert_fires(ws, 'G2', 'non-literal')

    def test_g2_freemkv_required_file(self):
        ws = Workspace(self)
        ws.write('freemkv', 'src/keydb_fetch.rs', real('src/keydb_fetch.rs') + '\nconst R: &str = include_str!("../README.md");\n')
        self.assert_fires(ws, 'G2', 'README.md')

    def test_g3_rerun_target_outside_required(self):
        ws = Workspace(self)
        ws.edit('libfreemkv', 'build.rs', 'src/scsi/macos_shim.c', 'docs/x')
        self.assert_fires(ws, 'G3', 'docs/x')

    def test_g3_code_emitting_build_script_must_be_required(self):
        ws = Workspace(self)
        ws.write('freemkv', 'build.rs', real('build.rs') + '\nfn cfg() { println!("cargo:rustc-cfg=fast"); }\n')
        policy = with_policy(freemkv_required=[p for p in POLICY['freemkv_required'] if p != 'build.rs'])
        self.assert_fires(ws, 'G3', 'rustc-cfg', policy)
        self.assertEqual([e for e in ws.guards() if e.startswith('G3')], [])

    def test_g4_new_crate_callee(self):
        ws = Workspace(self)
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\nfn moved() { crate::rip_util::f(); }\n')
        self.assert_fires(ws, 'G4', 'rip_util')
        ws = Workspace(self)
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\nuse super::rip_util;\n')
        self.assert_fires(ws, 'G4', 'rip_util')
        ws = Workspace(self)
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\nuse crate::{strings, rip_util::f};\n')
        self.assert_fires(ws, 'G4', 'rip_util')

    def test_g4_through_the_lib_crate_or_an_alias(self):
        for extra in ('\nfn a() { freemkv::rip_util::f(); }\n', '\nfn b() { ::freemkv::settings::g(); }\n',
                      '\nuse crate as root;\nfn c() { root::rip_util::f(); }\n',
                      '\nuse freemkv::{strings, rip_util};\n'):
            with self.subTest(extra=extra):
                ws = Workspace(self)
                ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + extra)
                self.assertTrue(any(e.startswith('G4') for e in ws.guards()), extra)

    def test_g4_inline_test_module_super_is_fine(self):
        ws = Workspace(self)
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') +
                 '\n#[cfg(test)]\nmod extra_tests {\n    use super::*;\n    fn t() { super::helper(); }\n}\n')
        self.assertEqual(ws.guards(), [])

    def test_g5_unknown_crate_token(self):
        ws = Workspace(self)
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\nfn f() { foo::bar(); }\n')
        self.assert_fires(ws, 'G5', '`foo::`')

    def test_g5_single_segment_use_is_a_crate(self):
        ws = Workspace(self)
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\nuse foo;\nfn f() { foo::x(); }\n')
        self.assert_fires(ws, 'G5', '`foo::`')
        ws = Workspace(self)
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\nuse foo as bar;\nfn f() { bar::x(); }\n')
        self.assert_fires(ws, 'G5', '`foo::`')

    def test_g6_manifest_declared_build_inputs(self):
        cases = [('libfreemkv', 'Cargo.toml', '[package]\n', '[package]\nbuild = "docs/b.rs"\n', 'package.build'),
                 ('libfreemkv', 'Cargo.toml', '[package]\n', '[lib]\npath = "benches/lib.rs"\n\n[package]\n', 'lib.path'),
                 ('freemkv', 'Cargo.toml', 'path = "src/lib.rs"', 'path = "src/lib2.rs"', 'lib.path'),
                 ('freemkv', 'Cargo.toml', 'path = "src/main.rs"', 'path = "src/cli2.rs"', 'CLI bin'),
                 ('freemkv', 'Cargo.toml', '[package]\n', '[package]\nbuild = "build2.rs"\n', 'package.build')]
        for repo, path, old, new, text in cases:
            with self.subTest(new=new):
                ws = Workspace(self)
                if repo == 'libfreemkv' and 'build =' in new:
                    ws.write('libfreemkv', 'docs/b.rs', 'fn main() {}\n')
                ws.edit(repo, path, old, new)
                self.assert_fires(ws, 'G6', text)
        for tracked in ('.cargo/config.toml', 'rust-toolchain.toml', 'rust-toolchain'):
            with self.subTest(tracked=tracked):
                ws = Workspace(self)
                ws.write('freemkv', tracked, '[build]\nrustflags = []\n')
                self.assert_fires(ws, 'G6', tracked)

    def test_g5_comments_strings_and_aliases(self):
        ws = Workspace(self)
        ws.write('freemkv', 'src/title_identity.rs', real('src/title_identity.rs') +
                 '\n/// See engine::rescan for the GUI side.\n// foo::bar\nconst S: &str = "zzz::y";\n'
                 'use ureq::Agent as A;\nfn g(_: A) {}\n')
        self.assertEqual(ws.guards(), [])

    def test_real_dev_content_fires_until_the_policy_fixes(self):
        """G2 (engine .md), G3 (libfreemkv .git/HEAD) and G5 (libc::) on real dev content."""
        edit = lambda n, v, s, d: (n, v, s, [x for x in d if not (n == 'libfreemkv' and x == 'libc')])
        files = base_files()
        files['freemkv']['Cargo.lock'] = lock_text(edit=edit)
        ws = Workspace(self, files)
        before = with_policy(required_extra={}, closure_roots=['ureq', 'mimalloc'],
                             rerun_exempt={'freemkv': POLICY['rerun_exempt']['freemkv']})
        errors = ws.guards(before)
        self.assertTrue(any(e.startswith('G2 freemkv-engine/src/resolve.rs') and 'USING_THE_ENGINE.md' in e
                            for e in errors), errors)
        self.assertTrue(any(e.startswith('G3 libfreemkv/build.rs') and '.git/HEAD' in e for e in errors), errors)
        self.assertTrue(any(e.startswith('G3 libfreemkv/build.rs') and '.git/{}' in e for e in errors), errors)
        self.assertTrue(any(e.startswith('G5 freemkv/src/cli_stop.rs') and '`libc::`' in e for e in errors), errors)
        self.assertEqual(ws.guards(POLICY), [])


# ── K: K, Φ and F sensitivity ──────────────────────────────────────────────

class FingerprintTests(unittest.TestCase):
    def setUp(self):
        self.ws = Workspace(self)
        self.base = self.ws.f()

    def changed(self, **kw):
        return self.ws.f(**kw) != self.base

    def lock(self, text):
        self.ws.write('freemkv', 'Cargo.lock', text)

    def test_k_is_the_library_and_root_closure_only(self):
        names = [e[0] for e in mg.closure(mg.parse_lock(lock_text()), POLICY)]
        self.assertEqual(sorted(names), ['cc', 'libc', 'libmimalloc-sys', 'mimalloc', 'rustls', 'serde',
                                         'serde_json', 'tracing', 'ureq'])

    def test_changes_f(self):
        cases = {
            'K version bump': lambda: self.lock(lock_text(edit=bump('serde', '1.0.230'))),
            'root closure (mimalloc)': lambda: self.lock(lock_text(edit=bump('mimalloc', '0.1.53'))),
            'libfreemkv src': lambda: self.ws.write('libfreemkv', 'src/udf.rs', 'pub fn udf() { 1; }\n'),
            'libfreemkv new top-level': lambda: self.ws.write('libfreemkv', 'assets/x', 'x'),
            'engine guide': lambda: self.ws.write('freemkv-engine', 'USING_THE_ENGINE.md', '# resolve 2\n'),
            'pipe.rs': lambda: self.ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\n'),
            'title_identity.rs': lambda: self.ws.write('freemkv', 'src/title_identity.rs',
                                                       real('src/title_identity.rs') + '\n'),
            'manifest (res)': lambda: self.ws.write('freemkv', 'res/freemkv.manifest', '<x/>'),
            'build.rs': lambda: self.ws.write('freemkv', 'build.rs', real('build.rs') + '\n'),
            'allocator removed': lambda: self.ws.edit('freemkv', 'src/main.rs', '#[global_allocator]', ''),
            'allocator cfg-gated': lambda: self.ws.edit('freemkv', 'src/main.rs', '#[global_allocator]',
                                                        '#[cfg(target_os = "none")]\n#[global_allocator]'),
            'nested lib doc': lambda: self.ws.write('libfreemkv', 'assets/table.md', '| x |'),
            'rust-version': lambda: self.ws.edit('freemkv', 'Cargo.toml', '"1.98.1"', '"1.98.2"'),
            'resolver': lambda: self.ws.edit('freemkv', 'Cargo.toml', '[package]\n', '[package]\nresolver = "2"\n'),
            'profile': lambda: self.ws.write('freemkv', 'Cargo.toml', real('Cargo.toml') +
                                             '\n[profile.release]\nlto = true\n'),
            'new non-GUI dep': lambda: self.ws.edit('freemkv', 'Cargo.toml', 'libc = "0.2"', 'libc = "0.2"\nrayon = "1"'),
            'lib feature spec': lambda: self.ws.edit('libfreemkv', 'Cargo.toml', 'serde_json = "1"',
                                                     'serde_json = { version = "1", features = ["raw_value"] }'),
            'control plane': lambda: self.ws.write('freemkv', 'tests/media_checks.py', '# changed\n'),
        }
        for name, apply in cases.items():
            with self.subTest(name=name):
                self.ws = Workspace(self)
                apply()
                self.assertTrue(self.changed(), name)

    def test_features_and_externals_and_policy_change_f(self):
        phi = copy.deepcopy(FEATURES)
        phi['x86_64-unknown-linux-musl'][0][2] = 'arbitrary_precision,default,std'
        self.assertTrue(self.changed(features=phi), 'i18n enabling serde_json/arbitrary_precision (Φ)')
        phi = copy.deepcopy(FEATURES)
        phi['x86_64-unknown-linux-musl'].append(['flate2', '1.1.10', 'zlib-rs'])
        self.assertTrue(self.changed(features=phi), 'unlock enabling a flate2 backend (Φ)')
        for path, value in ((('fixtures', 'uhd.iso', 'etag'), '"other"'),
                            (('fixtures', 'bd.iso', 'version_id'), 'v2'),
                            (('launch_templates', 'freemkv-runner-linux', 'version'), 4),
                            (('launch_templates', 'freemkv-runner-windows', 'image_id'), 'ami-2'),
                            (('launch_templates', 'freemkv-runner-windows', 'instance_type'), 'c6i.4xlarge')):
            ext = copy.deepcopy(EXTERNALS)
            ext[path[0]][path[1]][path[2]] = value
            with self.subTest(external=path):
                self.assertTrue(self.changed(externals=ext))
        for change in ({'accept': [{'os': 'linux'}]}, {'protocol': 2}):
            policy = copy.deepcopy(POLICY)
            policy['perf'].update(change)
            with self.subTest(perf=change):
                self.assertTrue(self.changed(policy=policy))
        self.assertTrue(self.changed(policy=with_policy(revision=POLICY['revision'] + 1)))
        harness = dict(POLICY['harness'], sha='0' * 40)
        self.assertTrue(self.changed(policy=with_policy(harness=harness)))

    def test_does_not_change_f(self):
        cases = {
            'gtk4 bump': lambda: self.lock(lock_text(edit=bump('gtk4', '0.9.8'))),
            'non-K CLI crate bump (zip)': lambda: self.lock(lock_text(edit=bump('zip', '8.6.1'))),
            'unlock-only third-party bump': lambda: self.lock(lock_text(edit=bump('num-bigint', '0.5.2'))),
            'first-party lock version/source': lambda: self.lock(lock_text(edit=lambda n, v, s, d: (
                n, '1.8.1' if n in mg.FIRST_PARTY else v,
                s.replace('v1.8.0', 'v1.8.1') if s and n in mg.FIRST_PARTY else s, d))),
            'package.version': lambda: self.ws.edit('freemkv', 'Cargo.toml', 'version = "1.8.0"', 'version = "1.8.1"'),
            'description': lambda: self.ws.edit('freemkv', 'Cargo.toml', 'description = ', 'description = "x" #'),
            'first-party pins': lambda: self.ws.edit('freemkv', 'Cargo.toml', 'tag = "v1.8.0"', 'tag = "v1.8.1"'),
            'lib version + pins': lambda: self.ws.edit('libfreemkv', 'Cargo.toml', 'tag = "v1.8.0"', 'tag = "v9"'),
            'lib dev-dependency': lambda: self.ws.edit('libfreemkv', 'Cargo.toml', 'tempfile = "3"', 'tempfile = "4"'),
            'gui dependency': lambda: self.ws.edit('freemkv', 'Cargo.toml', 'block2 = { version = "=0.6"',
                                                   'block2 = { version = "=0.7"'),
            'docs/readme/changelog': lambda: [self.ws.write('freemkv', p, 'new\n')
                                              for p in ('README.md', 'CHANGELOG.md', 'docs/x.md')],
            'lib README/tests/benches': lambda: [self.ws.write('libfreemkv', p, 'new\n')
                                                 for p in ('README.md', 'tests/t.rs', 'benches/b.rs')],
            'GUI file and CLI text': lambda: [self.ws.write('freemkv', p, '// new\n')
                                              for p in ('src/ui.rs', 'src/cli_entry.rs')],
            'icon': lambda: self.ws.write('freemkv', 'res/freemkv.ico', 'ICO2'),
            'flatpak metainfo': lambda: self.ws.write('freemkv', 'packaging/flatpak/org.freemkv.FreeMKV.metainfo.xml',
                                                      '<y/>'),
            'i18n and unlock source': lambda: [self.ws.write(r, 'src/lib.rs', 'pub fn z() {}\n')
                                               for r in ('freemkv-i18n', 'freemkv-unlock')],
            'main.rs outside the projection': lambda: self.ws.write('freemkv', 'src/main.rs',
                                                                    real('src/main.rs') + '\n// note\nfn unused() {}\n'),
        }
        for name, apply in cases.items():
            with self.subTest(name=name):
                self.ws = Workspace(self)
                apply()
                self.assertFalse(self.changed(), name)

    def test_parse_tree_normalizes_first_party_and_duplicates(self):
        text = ('freemkv v1.8.0 (/w/freemkv) default\n'
                'libfreemkv v1.8.0 (https://github.com/freemkv/libfreemkv?tag=v1.8.0#c8e67f16) default,rip\n'
                'serde_json v1.0.151 default,std\n'
                'serde_json v1.0.151 default,std (*)\n'
                'serde_derive v1.0.229 (proc-macro) \n'
                'gtk4 v0.9.7 v4_10\n')
        self.assertEqual(mg.parse_tree(text, {'libfreemkv', 'freemkv', 'serde_json', 'serde_derive'}),
                         [['freemkv', '', 'default'], ['libfreemkv', '', 'default,rip'],
                          ['serde_derive', '1.0.229', ''], ['serde_json', '1.0.151', 'default,std']])


# ── D: evidence validation and decide() fail-safe ──────────────────────────

class RecordSelfCheckTests(unittest.TestCase):
    def test_the_running_record_job_does_not_shadow_its_success(self):
        running = [{'name': 'cli-matrix (linux)', 'conclusion': 'success'},
                   {'name': 'record-media-evidence', 'conclusion': None, 'status': 'in_progress'}]
        done = mg.with_record_done(running)
        self.assertEqual((mg.job_named(done, 'record-media-evidence') or {}).get('conclusion'), 'success')
        self.assertEqual(sum(j['name'] == 'record-media-evidence' for j in done), 1)
        self.assertEqual(len(mg.with_record_done([])), 1)


def leg_record(leg):
    target = mg.LEG_TARGET[leg]
    return {'runner_name': f'ephemeral-{leg}-i-0123456789abcdef0', 'instance_id': 'i-0123456789abcdef0',
            'launched_by': RUN_ID,
            'rustc_release': '1.98.1', 'target': target, 'instance_type': 'c7i.4xlarge',
            'c_toolchain': 'musl-gcc: gcc 13.2.0', 'env_clean': True}


class Evidence:
    """A fake GitHub API serving one evidence tag and its run."""

    def __init__(self, test, policy=POLICY):
        self.policy = policy
        self.inputs = Workspace(test).inputs(policy=policy)
        self.f = mg.fingerprint(self.inputs)
        self.lock = lock_text().encode()
        revisions = dict(SIB_SHA)
        self.ev = {'schema': mg.SCHEMA, 'fingerprint': self.f, 'inputs': self.inputs, 'revisions': revisions,
                   'lock_sha256': mg.sha256(self.lock), 'run_id': RUN_ID,
                   'legs': {leg: leg_record(leg) for leg in mg.legs(policy)}}
        self.run = {'id': RUN_ID, 'path': '.github/workflows/qa.yml', 'event': 'push',
                    'head_branch': 'qa', 'head_sha': revisions['freemkv'],
                    'repository': {'full_name': 'freemkv/freemkv'},
                    'head_repository': {'full_name': 'freemkv/freemkv'},
                    'status': 'completed', 'conclusion': 'success',
                    'created_at': stamp(RUN_START), 'updated_at': stamp(RUN_START, hours=2)}
        # The tag object as record-media-evidence writes it through GITHUB_TOKEN.
        self.tag = {'tag': f'media-evidence/{self.f}/{RUN_ID}', 'object': {'sha': 'c1', 'type': 'commit'},
                    'tagger': dict(mg.ACTIONS_BOT, date=stamp(RUN_START, hours=1, minutes=50))}
        self.jobs = [{'name': n, 'conclusion': 'success', 'runner_name': None, 'labels': []}
                     for n in mg.required_jobs(policy)]
        for leg in mg.legs(policy):
            job = next(j for j in self.jobs if j['name'] == mg.LEG_JOB[leg])
            job.update(runner_name=self.ev['legs'][leg]['runner_name'],
                       labels=['self-hosted', 'freemkv-media', leg.split('-')[0], f'run-{RUN_ID}'])
        self.error = None
        self.tags = None
        # What run RUN_ID's own plan-media uploaded: the candidate it planned (no legs yet).
        self.plans = [{k: v for k, v in self.ev.items() if k != 'legs'}]
        self.plan_origin = {'id': RUN_ID, 'head_sha': revisions['freemkv'], 'head_branch': 'qa'}
        # compare/{QA_TIP}...{head_sha}: the qa branch contains the run's commit.
        self.qa_compare = {'status': 'behind', 'ahead_by': 0, 'behind_by': 3}

    def job(self, name):
        return next(j for j in self.jobs if j['name'] == name)

    def request(self, endpoint):
        if self.error:
            raise self.error
        prefix = 'repos/freemkv/freemkv/'
        path = endpoint[len(prefix):]
        blob = lambda b: {'content': base64.b64encode(b).decode()}
        routes = {
            f'git/matching-refs/tags/media-evidence/{self.f}/': self.tags if self.tags is not None else [
                {'ref': f'refs/tags/media-evidence/{self.f}/{RUN_ID}', 'object': {'sha': 't1', 'type': 'tag'}}],
            'git/tags/t1': self.tag,
            'git/commits/c1': {'parents': [], 'tree': {'sha': 'tr1'}},
            'git/trees/tr1': {'tree': [{'path': 'evidence.json', 'type': 'blob', 'sha': 'b1', 'size': 10},
                                       {'path': 'Cargo.lock', 'type': 'blob', 'sha': 'b2', 'size': 10}]},
            'git/blobs/b1': blob(json.dumps(self.ev).encode()),
            'git/blobs/b2': blob(self.lock),
            f'actions/runs/{RUN_ID}': self.run,
            f'actions/runs/{RUN_ID}/jobs?filter=latest&per_page=100': {'jobs': self.jobs},
            f'actions/runs/{RUN_ID}/artifacts?name=media-plan&per_page=100': {'artifacts': [
                {'id': 900 + i, 'name': 'media-plan', 'expired': False, 'size_in_bytes': 1000,
                 'workflow_run': dict(self.plan_origin)}
                for i in range(len(self.plans))]},
            'git/ref/heads/qa': {'ref': 'refs/heads/qa', 'object': {'sha': QA_TIP, 'type': 'commit'}},
            f'compare/{QA_TIP}...{self.run.get("head_sha")}': self.qa_compare,
        }
        if path not in routes:
            raise RuntimeError(f'HTTP 404 {endpoint}')
        return routes[path]

    def download(self, endpoint):
        """The zip of media-plan artifact 900+i."""
        import io
        import zipfile
        m = re.fullmatch(r'repos/freemkv/freemkv/actions/artifacts/(\d+)/zip', endpoint)
        if not m or not 0 <= int(m.group(1)) - 900 < len(self.plans):
            raise RuntimeError(f'HTTP 404 {endpoint}')
        out = io.BytesIO()
        with zipfile.ZipFile(out, 'w') as z:
            z.writestr('evidence.json', json.dumps(self.plans[int(m.group(1)) - 900]))
            z.writestr('Cargo.lock', self.lock)
        return out.getvalue()

    def forge(self, change):
        """Evidence for another F, naming this (real, successful) run: what a tag pusher can write."""
        ev = copy.deepcopy(self.ev)
        change(ev['inputs'])
        ev['fingerprint'] = mg.fingerprint(ev['inputs'])
        self.f, self.ev = ev['fingerprint'], ev
        self.tag['tag'] = f'media-evidence/{self.f}/{RUN_ID}'
        self.tags = None

    def found(self, perf_check=None, log=lambda *_: None):
        return mg.find_evidence(self.f, self.policy, self.request, perf_check, log=log, download=self.download)


class DecideTests(unittest.TestCase):
    def test_valid_evidence_is_reused(self):
        e = Evidence(self)
        got = e.found()
        self.assertIsNotNone(got)
        self.assertEqual(got['run_id'], RUN_ID)
        self.assertEqual(mg.decide(True)[:2], ('reuse', False))

    def test_every_invalid_case_runs(self):
        perf_policy = copy.deepcopy(POLICY)
        perf_policy['perf']['enabled'] = True
        cases = {
            'no tag': lambda e: setattr(e, 'tags', []),
            'lightweight tag': lambda e: e.tags.__setitem__(0, {**e.tags[0], 'object': {'sha': 't1', 'type': 'commit'}}),
            'API exception': lambda e: setattr(e, 'error', RuntimeError('HTTP 502')),
            'wrong schema': lambda e: e.ev.update(schema=3),
            'F not recomputable': lambda e: e.ev['inputs'].update(toolchain='1.98.2'),
            'F not recomputable (files)': lambda e: e.ev['inputs']['files'][0].__setitem__(2, '0' * 64),
            'leg record from another runner': lambda e: e.ev['legs']['linux'].update(
                runner_name='ephemeral-linux-i-0fedcba9876543210'),
            'lock digest': lambda e: setattr(e, 'lock', e.lock + b'\n'),
            'K not from the lock': lambda e: e.ev['inputs']['K'].pop(),
            'run not found': lambda e: setattr(e, 'run', {}),
            'pull_request event': lambda e: e.run.update(event='pull_request'),
            'feature branch': lambda e: e.run.update(head_branch='feature-x'),
            'dev branch': lambda e: e.run.update(head_branch='dev'),
            'run still in progress': lambda e: e.run.update(status='in_progress', conclusion=None),
            'run failed': lambda e: e.run.update(conclusion='failure'),
            'run cancelled': lambda e: e.run.update(conclusion='cancelled'),
            'wrong workflow': lambda e: e.run.update(path='.github/workflows/ci.yml'),
            'fork': lambda e: e.run.update(head_repository={'full_name': 'evil/freemkv'}),
            'head sha': lambda e: e.run.update(head_sha='f' * 40),
            'run id mismatch': lambda e: e.ev.update(run_id=RUN_ID + 1),
            'job failed': lambda e: e.job('compare-cli-matrix').update(conclusion='failure'),
            'record failed': lambda e: e.job('record-media-evidence').update(conclusion='cancelled'),
            'windows leg failed': lambda e: e.job('cli-matrix (windows)').update(conclusion='failure'),
            'rogue runner': lambda e: [e.job('cli-matrix (linux)').update(runner_name='freemkv-media-1'),
                                       e.ev['legs']['linux'].update(runner_name='freemkv-media-1')],
            'no run label': lambda e: e.job('cli-matrix (linux)').update(labels=['self-hosted', 'freemkv-media']),
            'launched-by unconfirmed': lambda e: e.ev['legs']['windows'].update(launched_by=None),
            'rustc != TC': lambda e: e.ev['legs']['linux'].update(rustc_release='1.98.2'),
            'gnu target': lambda e: e.ev['legs']['linux'].update(target='x86_64-unknown-linux-gnu'),
            'no instance type': lambda e: e.ev['legs']['linux'].update(instance_type=''),
            'no C toolchain': lambda e: e.ev['legs']['windows'].pop('c_toolchain'),
            'instance is not the runner': lambda e: e.ev['legs']['windows'].update(instance_id='i-0fedcba9876543210'),
            'dirty env': lambda e: e.ev['legs']['windows'].update(env_clean=False),
            'leg missing': lambda e: e.ev['legs'].pop('windows'),
            'oversized': lambda e: e.ev.update(pad='x' * (mg.MAX_EVIDENCE + 1)),
        }
        for name, apply in cases.items():
            with self.subTest(name=name):
                e = Evidence(self)
                if e.tags is None:
                    e.tags = [{'ref': f'refs/tags/media-evidence/{e.f}/{RUN_ID}', 'object': {'sha': 't1', 'type': 'tag'}}]
                apply(e)
                self.assertIsNone(e.found(), name)
        e = Evidence(self, perf_policy)
        e.ev.pop('perf', None)
        self.assertIsNone(e.found(lambda r, p: True), 'evidence without perf')
        e = Evidence(self, perf_policy)
        e.ev['perf'] = {'linux': {'verdict': 'PASS'}, 'windows': {'verdict': 'PASS'}}
        self.assertIsNone(e.found(lambda r, p: False), 'perf verdict does not recompute')
        self.assertIsNotNone(e.found(lambda r, p: True))
        e = Evidence(self, perf_policy)
        e.ev['perf'] = {'linux': {}, 'windows': {}}
        e.ev['legs']['linux-perf']['runner_name'] = 'ephemeral-linux-i-0123456789abcdef0'
        e.job('cli-perf (linux)')['runner_name'] = 'ephemeral-linux-i-0123456789abcdef0'
        self.assertIsNone(e.found(lambda r, p: True), 'perf leg on a functional runner')

    def test_forged_tags_are_rejected(self):
        """A tag anyone but a real, successful qa.yml run wrote, whose commit is the qa tip or an
        ancestor of it, is never evidence."""
        human = {'name': 'Matthew Jackson', 'email': '1085847+MattJackson@users.noreply.github.com'}
        cases = {
            'pushed by a person': lambda e: e.tag.update(tagger=dict(human, date=stamp(RUN_START, hours=1))),
            'bot name, other email': lambda e: e.tag['tagger'].update(email='github-actions@example.com'),
            'no tagger': lambda e: e.tag.pop('tagger'),
            'tag object under another name': lambda e: e.tag.update(tag=f'media-evidence/{e.f}/{RUN_ID + 1}'),
            'dated before the run': lambda e: e.tag['tagger'].update(date=stamp(RUN_START, seconds=-1)),
            'dated after the run': lambda e: e.tag['tagger'].update(date=stamp(RUN_START, days=1)),
            'undated': lambda e: e.tag['tagger'].pop('date'),
            'run id that does not exist': lambda e: e.tags.__setitem__(0, {
                'ref': f'refs/tags/media-evidence/{e.f}/{RUN_ID + 7}', 'object': {'sha': 't1', 'type': 'tag'}}),
            'run of another workflow': lambda e: e.run.update(path='.github/workflows/release.yml'),
            'run at another sha': lambda e: e.run.update(head_sha='e' * 40),
            'run on dev': lambda e: e.run.update(head_branch='dev', event='workflow_dispatch'),
            'run that failed': lambda e: e.run.update(conclusion='failure'),
        }
        for name, apply in cases.items():
            with self.subTest(name=name):
                e = Evidence(self)
                e.tags = [{'ref': f'refs/tags/media-evidence/{e.f}/{RUN_ID}', 'object': {'sha': 't1', 'type': 'tag'}}]
                apply(e)
                self.assertIsNone(e.found(), name)
        # The same fixture untouched is accepted, so each case above fails on its own change.
        self.assertIsNotNone(Evidence(self).found())

    def test_forged_fingerprint_on_a_real_run_is_rejected(self):
        """Review FB1: a real successful qa run R, and evidence for any F the tag pusher likes
        (here: a libfreemkv file hash nobody tested). Every tag field checks out; R's own plan
        says it tested something else."""
        e = Evidence(self)
        real_f = e.f
        e.forge(lambda inputs: inputs['files'][0].__setitem__(2, 'b' * 64))
        self.assertNotEqual(e.f, real_f)
        logs = []
        self.assertIsNone(e.found(log=logs.append))
        self.assertTrue(any("run's own media-plan" in line for line in logs), logs)

    def test_a_run_on_a_tag_named_qa_is_not_a_qa_run(self):
        """Review 2 item 1. Someone who can push creates refs/tags/qa on their own commit, dispatches
        qa.yml on it, and so owns run R: head_branch is "qa" (a run on a tag reports the tag name
        there), R uploads its own media-plan matching the forged evidence, R is successful, and the
        bot tagger and dates are theirs to write. The qa BRANCH does not contain R's commit."""
        for status, ahead in (('diverged', 2), ('ahead', 1)):
            with self.subTest(compare=status):
                e = Evidence(self)
                e.run.update(event='workflow_dispatch', head_branch='qa')
                e.qa_compare.update(status=status, ahead_by=ahead)
                logs = []
                self.assertIsNone(e.found(log=logs.append))
                self.assertTrue(any('is not on the qa branch' in line for line in logs), logs)

    def test_the_qa_branch_must_contain_the_run(self):
        # GitHub REST API compare, status enum ["diverged", "ahead", "behind", "identical"]; BASE is
        # the qa tip, HEAD the run's commit: contained means "identical" or "behind" with ahead_by 0.
        for status, ahead, ok in (('identical', 0, True), ('behind', 0, True), ('ahead', 1, False),
                                  ('diverged', 1, False), ('behind', 1, False), (None, None, False)):
            with self.subTest(status=status, ahead_by=ahead):
                e = Evidence(self)
                e.qa_compare.clear()
                if status:
                    e.qa_compare.update(status=status, ahead_by=ahead)
                self.assertEqual(e.found() is not None, ok)
        e = Evidence(self)
        e.request_error = None
        routes = e.request

        def no_branch(endpoint):
            if endpoint.endswith('git/ref/heads/qa'):
                raise RuntimeError('HTTP 404')
            return routes(endpoint)
        self.assertIsNone(mg.find_evidence(e.f, e.policy, no_branch, log=lambda *_: None, download=e.download))

    def test_the_branch_read_must_be_the_branch(self):
        """GitHub REST API "Get a reference" answers {"ref": "refs/heads/qa", "object": {"sha": ...}};
        anything else (a tag's ref, a malformed sha) is not the qa branch."""
        for tip in ({'ref': 'refs/tags/qa', 'object': {'sha': QA_TIP}},
                    {'ref': 'refs/heads/qa', 'object': {'sha': 'not-a-sha'}}, {}):
            with self.subTest(tip=tip):
                e = Evidence(self)
                routes = e.request

                def request(endpoint, tip=tip):
                    return tip if endpoint.endswith('git/ref/heads/qa') else routes(endpoint)
                self.assertIsNone(mg.find_evidence(e.f, e.policy, request, log=lambda *_: None, download=e.download))

    def test_on_qa_branch_compares_shas_not_names(self):
        calls = []

        def request(endpoint):
            calls.append(endpoint)
            if endpoint.endswith('git/ref/heads/qa'):
                return {'ref': 'refs/heads/qa', 'object': {'sha': QA_TIP}}
            return {'status': 'identical', 'ahead_by': 0}
        mg.on_qa_branch(SIB_SHA['freemkv'], request)
        self.assertEqual(calls, ['repos/freemkv/freemkv/git/ref/heads/qa',
                                 f'repos/freemkv/freemkv/compare/{QA_TIP}...{SIB_SHA["freemkv"]}'])

    def test_the_plan_artifact_must_come_from_the_run(self):
        """The artifact schema's workflow_run {id, head_sha} must name this run at this commit."""
        for field, value in (('id', RUN_ID + 1), ('head_sha', 'e' * 40), ('id', None)):
            with self.subTest(field=field):
                e = Evidence(self)
                e.plan_origin[field] = value
                self.assertIsNone(e.found())
        e = Evidence(self)
        e.plan_origin.clear()
        self.assertIsNone(e.found(), 'an artifact without workflow_run is not bound to the run')

    def test_evidence_binding_needs_the_runs_plan(self):
        cases = {
            'plan artifact expired': lambda e: e.plans.clear(),
            'plan names other revisions': lambda e: e.plans[0].update(revisions=dict(SIB_SHA, libfreemkv='9' * 40)),
            'plan has another lock': lambda e: e.plans[0].update(lock_sha256='0' * 64),
            'plan is of another run': lambda e: e.plans[0].update(run_id=RUN_ID + 1),
        }
        for name, apply in cases.items():
            with self.subTest(name=name):
                e = Evidence(self)
                apply(e)
                self.assertIsNone(e.found(), name)
        e = Evidence(self)
        e.plans.insert(0, dict(e.plans[0], fingerprint='0' * 64))
        self.assertIsNotNone(e.found(), 'a re-run attempt\'s other plan does not hide the matching one')

    def test_forged_tag_with_otherwise_perfect_evidence_is_rejected(self):
        """Copying a real run's evidence byte for byte does not help a tag the bot did not write."""
        e = Evidence(self)
        e.tag['tagger'] = {'name': 'github-actions', 'email': mg.ACTIONS_BOT['email'], 'date': stamp(RUN_START, hours=1)}
        logs = []
        self.assertIsNone(e.found(log=logs.append))
        self.assertTrue(any('not created by github-actions[bot]' in line for line in logs), logs)

    def test_newest_valid_tag_wins_and_a_bad_one_never_blocks(self):
        e = Evidence(self)
        e.tags = [{'ref': f'refs/tags/media-evidence/{e.f}/{RUN_ID - 1}', 'object': {'sha': 'bad', 'type': 'tag'}},
                  {'ref': f'refs/tags/media-evidence/{e.f}/{RUN_ID}', 'object': {'sha': 't1', 'type': 'tag'}}]
        self.assertEqual(e.found()['run_id'], RUN_ID)

    def test_evidence_expires_explicitly_at_max_age(self):
        """Review 2 item 4: at 90 days evidence is expired, and says so; it is not 'still valid'."""
        import datetime

        def at(days):
            e = Evidence(self)
            t = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=days, hours=1)
            e.run['created_at'] = t.strftime('%Y-%m-%dT%H:%M:%SZ')
            e.run['updated_at'] = (t + datetime.timedelta(hours=2)).strftime('%Y-%m-%dT%H:%M:%SZ')
            e.tag['tagger']['date'] = (t + datetime.timedelta(hours=1)).strftime('%Y-%m-%dT%H:%M:%SZ')
            logs = []
            return e.found(log=logs.append), logs
        limit = POLICY['max_age_days']
        self.assertEqual(limit, 90)
        got, _ = at(10)
        self.assertEqual(got['warnings'], [])
        got, _ = at(limit - 5)
        self.assertTrue(any('expires at 90 days (in 5)' in w for w in got['warnings']), got['warnings'])
        for days in (limit, limit + 400):
            got, logs = at(days)
            self.assertIsNone(got)
            self.assertTrue(any('evidence expired' in line and 'counts for 90 days' in line for line in logs), logs)
            self.assertFalse(any('still valid' in line for line in logs))
        e = Evidence(self)
        e.run.pop('created_at')
        self.assertIsNone(e.found())

    def age_run(self, age):
        """Evidence whose run was created `age` (a timedelta) ago."""
        import datetime
        e = Evidence(self)
        t = datetime.datetime.now(datetime.timezone.utc) - age
        e.run['created_at'] = t.strftime('%Y-%m-%dT%H:%M:%SZ')
        e.run['updated_at'] = (t + datetime.timedelta(minutes=30)).strftime('%Y-%m-%dT%H:%M:%SZ')
        e.tag['tagger']['date'] = (t + datetime.timedelta(minutes=20)).strftime('%Y-%m-%dT%H:%M:%SZ')
        logs = []
        return e.found(log=logs.append), logs

    def test_the_last_hour_before_expiry_is_accepted_with_a_warning(self):
        import datetime
        got, logs = self.age_run(datetime.timedelta(days=89, hours=23))
        self.assertIsNotNone(got, logs)
        self.assertTrue(any('89 days old and expires at 90 days (in 1)' in w for w in got['warnings']),
                        got['warnings'])
        got, logs = self.age_run(datetime.timedelta(days=90, minutes=5))
        self.assertIsNone(got)
        self.assertTrue(any('evidence expired: it is 90 days old' in line for line in logs), logs)

    def test_the_warning_window_opens_at_76_days(self):
        """max_age_days 90, EXPIRY_NOTICE_DAYS 14: whole days of age >= 76 warn, below do not."""
        import datetime
        self.assertEqual((POLICY['max_age_days'], mg.EXPIRY_NOTICE_DAYS), (90, 14))
        got, _ = self.age_run(datetime.timedelta(days=75, hours=23))
        self.assertIsNotNone(got)
        self.assertEqual(got['warnings'], [], '75 days 23 h: no warning yet')
        got, _ = self.age_run(datetime.timedelta(days=76, minutes=5))
        self.assertIsNotNone(got)
        self.assertEqual(len(got['warnings']), 1)
        self.assertIn('76 days old and expires at 90 days (in 14)', got['warnings'][0])

    def test_expiry_matches_the_plan_artifact_retention(self):
        import yaml
        qa = yaml.safe_load((ROOT / '.github/workflows/qa.yml').read_text())
        upload = next(s for s in qa['jobs']['plan-media']['steps'] if s.get('with', {}).get('name') == 'media-plan')
        self.assertEqual(upload['with']['retention-days'], POLICY['max_age_days'])

    def test_canary(self):
        policy = with_policy(canary={'probes': [{'fixture': 'uhd.iso'}]})
        ok = {'ok': True, 'probes': [{'fixture': 'uhd.iso', 'ok': True, 'reason': 'known key returned'}]}
        self.assertEqual(mg.canary(policy, ok, 'refs/heads/qa'), (True, '', None))
        failed = {'ok': False, 'probes': [{'fixture': 'uhd.iso', 'ok': False, 'reason': 'HTTP 503'}]}
        self.assertEqual(mg.canary(policy, failed, 'refs/heads/qa')[:2], (False, 'uhd.iso: HTTP 503'))
        for name, result in {'no result': None, 'not a dict': ['ok'], 'probe skipped': {'ok': True, 'probes': []},
                             'probe not ok': {'ok': True, 'probes': [{'fixture': 'uhd.iso', 'ok': 'yes'}]},
                             'crashed': {'ok': False, 'probes': [], 'error': 'KeyError'}}.items():
            with self.subTest(name=name):
                self.assertFalse(mg.canary(policy, result, 'refs/heads/qa')[0])
        ok_dev, _, note = mg.canary(policy, None, 'refs/heads/dev')
        self.assertTrue(ok_dev, 'the canary runs where the gate runs (qa), not on dev')
        self.assertIn('qa branch only', note)
        self.assertIn('qa branch only', mg.canary(policy, None, 'refs/tags/qa')[2], 'a tag named qa is not qa')
        self.assertEqual(mg.canary(with_policy(canary={'probes': []}), None, 'refs/heads/qa'), (True, '', None))
        self.assertEqual(mg.decide(False, canary_ok=False)[:2], ('canary-failed', False))
        self.assertEqual(mg.decide(True, run_media=True, canary_ok=False)[:2], ('canary-failed', False))
        self.assertIn('HTTP 503', mg.decide(False, canary_ok=False, canary_why='uhd.iso: HTTP 503')[2])

    def test_the_canary_is_configured(self):
        """Decision 15 is enforced by the checked-in policy, not waiting on an operator."""
        self.assertIn({'fixture': 'uhd.iso'}, POLICY['canary']['probes'])
        self.assertIn('tests/media_canary.py', POLICY['control_plane'], 'the canary is part of F')

    def test_decide_order(self):
        self.assertEqual(mg.decide(False)[:2], ('run', True))
        self.assertEqual(mg.decide(True, run_media=True)[:2], ('run', True))
        self.assertEqual(mg.decide(True, superseded=True)[:2], ('superseded', False))


# ── W: waiver ──────────────────────────────────────────────────────────────

class WaiverTests(unittest.TestCase):
    def test_skip_while_unproven_is_red(self):
        self.assertEqual(mg.decide(False, skip=True, skip_reason='[skip-media]')[:2], ('waived', False))
        self.assertEqual(mg.decide(False, skip=True, skip_reason='outage')[:2], ('waived', False))

    def test_skip_is_ignored_when_proven(self):
        self.assertEqual(mg.decide(True, skip=True, skip_reason='x')[:2], ('reuse', False))

    def test_dispatch_skip_needs_a_reason(self):
        with self.assertRaises(ValueError):
            mg.decide(False, skip=True, skip_reason='  ')


# ── A: seal ────────────────────────────────────────────────────────────────

class SealTests(unittest.TestCase):
    def plan_dir(self, run_id=RUN_ID, sha=SIB_SHA['freemkv']):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(rmtree_quiet, d)
        lock = lock_text().encode()
        (d / 'Cargo.lock').write_bytes(lock)
        (d / 'evidence.json').write_text(json.dumps({'run_id': run_id, 'revisions': {'freemkv': sha},
                                                     'lock_sha256': mg.sha256(lock)}))
        return d

    def test_any_attempt_of_the_run_may_seal(self):
        mg.seal(self.plan_dir(), str(RUN_ID), SIB_SHA['freemkv'])

    def test_other_run_or_candidate_is_rejected(self):
        with self.assertRaises(ValueError):
            mg.seal(self.plan_dir(run_id=RUN_ID + 1), str(RUN_ID), SIB_SHA['freemkv'])
        with self.assertRaises(ValueError):
            mg.seal(self.plan_dir(sha='f' * 40), str(RUN_ID), SIB_SHA['freemkv'])
        d = self.plan_dir()
        (d / 'Cargo.lock').write_text('tampered')
        with self.assertRaises(ValueError):
            mg.seal(d, str(RUN_ID), SIB_SHA['freemkv'])


# ── L: locks ───────────────────────────────────────────────────────────────

class LockTests(unittest.TestCase):
    def test_third_party_change_fails_and_first_party_bumps_pass(self):
        base = lock_text()
        self.assertEqual(mg.lock_assert(base, lock_text(edit=bump('zip', '8.6.1'))) != [], True)
        released = lock_text(edit=lambda n, v, s, d: (n, '1.8.1' if n in mg.FIRST_PARTY else v,
                                                       s.replace('v1.8.0', 'v1.8.1') if s and n in mg.FIRST_PARTY
                                                       else s, d))
        self.assertEqual(mg.lock_assert(base, released), [])

    def test_k_only_predictive_check(self):
        base = lock_text()
        self.assertEqual(mg.lock_assert(base, lock_text(edit=bump('zip', '8.6.1')), True, POLICY), [])
        self.assertNotEqual(mg.lock_assert(base, lock_text(edit=bump('tracing', '0.1.45')), True, POLICY), [])

    def test_cli_exit_codes(self):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(rmtree_quiet, d)
        (d / 'a.lock').write_text(lock_text())
        (d / 'b.lock').write_text(lock_text(edit=bump('ureq', '3.4.3')))
        (d / 'c.lock').write_text(lock_text(edit=bump('freemkv', '1.8.1')))
        run = lambda *a: mg.main(['release-lock-assert', '--base', str(d / 'a.lock'), *a])
        self.assertEqual(run('--candidate', str(d / 'b.lock')), 1)
        self.assertEqual(run('--candidate', str(d / 'c.lock')), 0)
        self.assertEqual(run('--candidate', str(d / 'b.lock'), '--k-only'), 1)

    def workspace_with_resolve(self):
        ws = Workspace(self)
        resolved = lock_text(edit=bump('serde', '1.0.230'))

        def run(cmd, cwd=None, **kw):
            (Path(cwd) / 'Cargo.lock').write_text(resolved)
            packages = [{'name': r, 'manifest_path': str(ws.root / r / 'Cargo.toml')} for r in mg.FIRST_PARTY]
            return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps({'packages': packages}), stderr='')
        return ws, run, resolved

    def test_plan_on_a_stale_lock_warns_and_fingerprints_l_qa(self):
        ws, run, resolved = self.workspace_with_resolve()
        env = {'REVISIONS': json.dumps(SIB_SHA), 'GITHUB_RUN_ID': str(RUN_ID)}
        outputs, evidence, lock = mg.plan(ws.root, POLICY, env, EXTERNALS, request=lambda e: [], run=run,
                                          tree=lambda root, k, run: FEATURES)
        self.assertEqual(lock, resolved)
        self.assertTrue(any('stale' in w for w in outputs['warnings']))
        self.assertEqual(outputs['status'], 'run')
        self.assertEqual(evidence['lock_sha256'], mg.sha256(resolved.encode()))
        self.assertEqual(evidence['inputs']['K'], mg.closure(mg.parse_lock(resolved), POLICY))

    def test_plan_guard_violation_is_red(self):
        ws, run, _ = self.workspace_with_resolve()
        ws.write('freemkv', 'src/pipe.rs', real('src/pipe.rs') + '\nfn f() { crate::rip_util::g(); }\n')
        env = {'REVISIONS': json.dumps(SIB_SHA), 'GITHUB_RUN_ID': str(RUN_ID)}
        with self.assertRaises(mg.GuardError):
            mg.plan(ws.root, POLICY, env, EXTERNALS, request=lambda e: [], run=run, tree=lambda r, k, run: FEATURES)

    def test_restore_rejects_a_tampered_lock(self):
        ws, run, _ = self.workspace_with_resolve()
        d = Path(tempfile.mkdtemp())
        self.addCleanup(rmtree_quiet, d)
        revisions = {r: mg.git(ws.root / r, 'rev-parse', 'HEAD') for r in mg.FIRST_PARTY}
        lock = lock_text().encode()
        (d / 'Cargo.lock').write_bytes(lock)
        (d / 'evidence.json').write_text(json.dumps({'revisions': revisions, 'lock_sha256': mg.sha256(lock)}))
        mg.restore(ws.root, d, run=run)
        (d / 'Cargo.lock').write_bytes(lock + b'\n# tampered\n')
        with self.assertRaises(ValueError):
            mg.restore(ws.root, d, run=run)

    def test_resolve_rejects_a_crate_from_outside_the_snapshot(self):
        ws = Workspace(self)

        def run(cmd, cwd=None, **kw):
            packages = [{'name': r, 'manifest_path': f'/elsewhere/{r}/Cargo.toml'} for r in mg.FIRST_PARTY]
            return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps({'packages': packages}), stderr='')
        with self.assertRaises(ValueError):
            mg.resolve(ws.root, run=run)


class PlanTests(unittest.TestCase):
    def setUp(self):
        self.ws = Workspace(self)
        lock = (self.ws.root / 'freemkv' / 'Cargo.lock').read_text()
        ws = self.ws

        def run(cmd, cwd=None, **kw):
            (Path(cwd) / 'Cargo.lock').write_text(lock)
            packages = [{'name': r, 'manifest_path': str(ws.root / r / 'Cargo.toml')} for r in mg.FIRST_PARTY]
            return subprocess.CompletedProcess(cmd, 0, stdout=json.dumps({'packages': packages}), stderr='')
        self.run = run

    def plan(self, request=lambda e, **kw: [], download=None, **env):
        base = {'REVISIONS': json.dumps(SIB_SHA), 'GITHUB_RUN_ID': str(RUN_ID)}
        return mg.plan(self.ws.root, POLICY, dict(base, **env), EXTERNALS, request=request, run=self.run,
                       tree=lambda root, k, run: FEATURES, download=download)[0]

    def test_matching_evidence_is_reused(self):
        e = Evidence(self)
        out = self.plan(request=e.request, download=e.download)
        self.assertEqual(out['fingerprint'], e.f, 'plan and evidence must fingerprint the same candidate alike')
        self.assertEqual((out['status'], out['run']), ('reuse', 'false'))
        self.assertIn(str(RUN_ID), out['evidence_url'])

    def test_plan_on_qa_runs_the_canary_verdict(self):
        base = {'REVISIONS': json.dumps(SIB_SHA), 'GITHUB_RUN_ID': str(RUN_ID), 'GITHUB_REF': 'refs/heads/qa'}

        def plan(result):
            return mg.plan(self.ws.root, POLICY, base, EXTERNALS, request=lambda e, **kw: [], run=self.run,
                           canary_result=result, tree=lambda root, k, run: FEATURES)[0]
        ok = plan({'ok': True, 'probes': [{'fixture': 'uhd.iso', 'ok': True, 'reason': 'known key returned'}]})
        self.assertEqual((ok['status'], ok['run']), ('run', 'true'))
        self.assertFalse([w for w in ok['warnings'] if 'canary' in w], 'no canary warning once it is active')
        self.assertEqual(ok['notices'], [])
        bad = plan({'ok': False, 'probes': [{'fixture': 'uhd.iso', 'ok': False, 'reason': 'no key'}]})
        self.assertEqual((bad['status'], bad['run']), ('canary-failed', 'false'), 'red immediately, no EC2')
        self.assertIn('no key', bad['reason'])
        self.assertEqual(plan(None)['status'], 'canary-failed', 'a canary that never reported fails closed')

    def test_plan_off_qa_notes_the_canary_did_not_run(self):
        out = self.plan(GITHUB_REF='refs/heads/dev')
        self.assertEqual(out['status'], 'run')
        self.assertFalse([w for w in out['warnings'] if 'canary' in w])
        self.assertTrue(any('qa branch only' in n for n in out['notices']))

    def test_plan_names_the_legs_to_launch(self):
        self.assertEqual(json.loads(self.plan()['legs']), ['linux', 'windows'])

    def test_env_driven_decisions(self):
        self.assertEqual(self.plan()['status'], 'run')
        self.assertEqual(self.plan(HEAD_MESSAGE='fix typo [skip-media]')['status'], 'waived')
        self.assertEqual(self.plan(SKIP_MEDIA='true', SKIP_REASON='key service outage')['status'], 'waived')
        with self.assertRaises(ValueError):
            self.plan(SKIP_MEDIA='true', SKIP_REASON='')
        e = Evidence(self)
        self.assertEqual(self.plan(request=e.request, download=e.download, RUN_MEDIA='true')['status'], 'run')
        self.assertEqual(self.plan(request=e.request, download=e.download, HEAD_MESSAGE='[skip-media]')['status'], 'reuse')
        self.assertEqual(self.plan(SUPERSEDED='true')['status'], 'superseded')

    def test_outputs_cannot_be_injected(self):
        d = Path(tempfile.mkdtemp())
        self.addCleanup(rmtree_quiet, d)
        out = d / 'out'
        env = dict(os.environ, GITHUB_OUTPUT=str(out))
        with unittest.mock.patch.dict(os.environ, env, clear=True):
            mg._write_outputs({'status': 'waived', 'reason': 'outage\nstatus=reuse\nrun=false'})
        parsed, lines = {}, out.read_text().splitlines()
        i = 0
        while i < len(lines):
            key, delim = lines[i].split('<<', 1)
            j = lines.index(delim, i + 1)
            parsed.setdefault(key, []).append('\n'.join(lines[i + 1:j]))
            i = j + 1
        self.assertEqual(parsed['status'], ['waived'])
        self.assertNotIn('run', parsed)


class TokenizerTests(unittest.TestCase):
    def test_c_strings(self):
        toks = mg.tokenize('let a = c"x::y"; let b = cr#"q"::"#; z::w();')
        self.assertEqual([t[1] for t in toks if t[0] == 'ident'], ['let', 'a', 'let', 'b', 'z', 'w'])

    def test_comments_strings_and_lifetimes(self):
        toks = mg.tokenize('/* a /* nested */ foo:: */ fn f<\'a>(x: &\'a str) -> char { let s = r#"x::y"#; '
                           '\'"\' ; b\'x\'; "q\\"::" ; bar::baz() } // tail::x')
        idents = [t[1] for t in toks if t[0] == 'ident']
        self.assertNotIn('foo', idents)
        self.assertNotIn('tail', idents)
        self.assertIn('bar', idents)
        self.assertIn('x::y', [t[1] for t in toks if t[0] == 'str'])


if __name__ == '__main__':
    unittest.main()
