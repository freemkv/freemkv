import json
from pathlib import Path
import tempfile
import tomllib
import unittest
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
        self.assertNotIn("v1.7.6", json.dumps(manifest))
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

    def test_metadata_without_release_history_fails(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "metadata.xml"
            path.write_text('<component/>')
            with self.assertRaises(ValueError):
                prepare.update_metadata(path, "1.7.7", "2026-09-26")


if __name__ == "__main__":
    unittest.main()
