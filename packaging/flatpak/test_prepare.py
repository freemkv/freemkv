from contextlib import redirect_stdout
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

import prepare


class PackageTests(unittest.TestCase):
    def test_all_first_party_dependencies_override_old_release_tags(self):
        config = tomllib.loads(prepare.patches())["patch"]
        actual = dict(config["crates-io"])
        actual.update(config["https://github.com/freemkv/freemkv-unlock"])
        self.assertEqual(set(actual), set(prepare.REPOS) - {"freemkv"})
        for name, spec in actual.items():
            self.assertEqual(spec, {"path": f"../{name}"})

    def test_new_version_is_first_without_rewriting_old_release(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "metadata.xml"
            path.write_text('<component><releases><release version="1.7.6" date="2026-09-25"/></releases></component>')
            prepare.update_metadata(path, "1.7.7", "2026-09-26")
            prepare.update_metadata(path, "1.7.7", "2026-09-27")
            releases = ET.parse(path).findall("releases/release")
            self.assertEqual([r.get("version") for r in releases], ["1.7.7", "1.7.6"])
            self.assertEqual(releases[0].get("date"), "2026-09-26")

    def test_manifest_is_relocatable_and_builds_locked_offline(self):
        manifest = prepare.manifest("a" * 64)
        module = manifest["modules"][0]
        self.assertEqual(module["sources"][0], {
            "type": "archive", "path": "sources.tar.gz", "sha256": "a" * 64,
            "strip-components": 0})
        self.assertIn("--offline --locked", module["build-commands"][0])
        for source in module["sources"]:
            self.assertFalse({"url", "tag", "branch", "commit"} & set(source), source)
            self.assertFalse(Path(source["path"]).is_absolute(), source)
        self.assertFalse(any(s.get("type") == "patch" for s in module["sources"]))

    def test_bare_command_launches_the_gui_build(self):
        manifest = prepare.manifest("a" * 64)
        module = manifest["modules"][0]
        self.assertEqual(manifest["command"], "freemkv")
        self.assertIn("--features gui --bin freemkv", module["build-commands"][0])
        self.assertEqual(len(module["sources"]), 1)
        self.assertNotIn("freemkv-gui", json.dumps(manifest))
        desktop = (Path(prepare.__file__).parent / "org.freemkv.FreeMKV.desktop").read_text()
        self.assertIn("\nExec=freemkv\n", desktop)
        # The app takes no file argument, so it must not offer itself in
        # "Open With" for disc images or MKV files.
        self.assertNotIn("MimeType=", desktop)

    def test_metadata_without_release_history_fails(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "metadata.xml"
            path.write_text('<component/>')
            with self.assertRaises(ValueError):
                prepare.update_metadata(path, "1.7.7", "2026-09-26")


def git(*args, cwd):
    env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
           "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com",
           "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
    subprocess.run(["git", *args], cwd=cwd, env=env, check=True, capture_output=True)


class PrepareTests(unittest.TestCase):
    """prepare() over fake checkouts; cargo is stubbed so nothing touches the network."""

    def checkouts(self, base, versions=None):
        here = Path(prepare.__file__).parent
        for name in prepare.REPOS:
            repo = base / name
            repo.mkdir(parents=True)
            version = (versions or {}).get(name, "1.7.8")
            (repo / "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion = "{version}"\n')
            if name == "freemkv":
                (repo / "Cargo.lock").write_text("# lock\n")
                flatpak = repo / "packaging/flatpak"
                flatpak.mkdir(parents=True)
                for f in ("org.freemkv.FreeMKV.metainfo.xml", "flathub.json"):
                    shutil.copy(here / f, flatpak / f)
            git("init", "-q", cwd=repo)
            git("add", "-A", cwd=repo)
            git("commit", "-qm", "fixture", cwd=repo)
        return base

    def run_prepare(self, base, resolve=None):
        real = prepare.run

        def fake(*args, cwd=None):
            if args[0] != "cargo":
                return real(*args, cwd=cwd)
            if args[1] == "vendor":
                return b""
            root = Path(cwd).parent
            packages = [{"name": n, "manifest_path": str(root / n / "Cargo.toml")} for n in prepare.REPOS]
            return json.dumps({"packages": (resolve or (lambda p, r: p))(packages, root)}).encode()

        with patch.object(prepare, "run", fake), redirect_stdout(io.StringIO()):
            prepare.prepare(base / "checkouts", base / "out")

    def test_pinned_snapshot_records_every_revision_and_the_archive_checksum(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            self.checkouts(base / "checkouts")
            self.run_prepare(base)
            out = base / "out"
            provenance = json.loads((out / "provenance.json").read_text())
            self.assertEqual(provenance["version"], "1.7.8")
            self.assertEqual(set(provenance["revisions"]), set(prepare.REPOS))
            manifest = json.loads((out / "org.freemkv.FreeMKV.json").read_text())
            digest = hashlib.sha256((out / "sources.tar.gz").read_bytes()).hexdigest()
            self.assertEqual(manifest["modules"][0]["sources"][0]["sha256"], digest)
            releases = ET.parse(out / "org.freemkv.FreeMKV.metainfo.xml").findall("releases/release")
            self.assertEqual(releases[0].get("version"), "1.7.8")

    def test_mixed_candidate_versions_are_refused(self):
        with tempfile.TemporaryDirectory() as temp:
            base = Path(temp)
            self.checkouts(base / "checkouts", {"libfreemkv": "1.7.7"})
            with self.assertRaisesRegex(ValueError, "candidate versions differ"):
                self.run_prepare(base)

    def test_crate_resolved_from_a_tag_or_twice_is_refused(self):
        tagged = lambda packages, root: [
            {**p, "manifest_path": "/cargo/git/checkouts/libfreemkv/Cargo.toml"}
            if p["name"] == "libfreemkv" else p for p in packages]
        doubled = lambda packages, root: packages + [packages[1]]
        for resolve in (tagged, doubled):
            with tempfile.TemporaryDirectory() as temp:
                base = Path(temp)
                self.checkouts(base / "checkouts")
                with self.assertRaisesRegex(ValueError, "libfreemkv did not resolve to its pinned source"):
                    self.run_prepare(base, resolve)


if __name__ == "__main__":
    unittest.main()
