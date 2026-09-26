"""Prepare a relocatable, offline Flatpak build from exact repository revisions."""

import argparse
import hashlib
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import xml.etree.ElementTree as ET

REPOS = ("freemkv", "libfreemkv", "freemkv-engine", "freemkv-keysources",
         "freemkv-i18n", "freemkv-unlock")


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd)


def patches():
    lines = ["[patch.crates-io]"]
    lines += [f'{name} = {{ path = "../{name}" }}' for name in REPOS[1:-1]]
    lines += ['[patch."https://github.com/freemkv/freemkv-unlock"]',
              'freemkv-unlock = { path = "../freemkv-unlock" }']
    return "\n".join(lines) + "\n"


def update_metadata(path, version, date):
    tree = ET.parse(path)
    releases = tree.getroot().find("releases")
    if releases is None:
        raise ValueError("AppStream release history is missing")
    latest = releases.find("release")
    if latest is None or latest.get("version") != version:
        releases.insert(0, ET.Element("release", version=version, date=date))
    ET.indent(tree, space="  ")
    tree.write(path, encoding="utf-8", xml_declaration=True)


def manifest(checksum):
    return {
        "app-id": "org.freemkv.FreeMKV", "runtime": "org.gnome.Platform",
        "runtime-version": "51", "sdk": "org.gnome.Sdk",
        "sdk-extensions": ["org.freedesktop.Sdk.Extension.rust-stable"],
        "command": "freemkv",
        "finish-args": ["--socket=wayland", "--socket=fallback-x11", "--share=ipc",
                        "--device=all", "--filesystem=xdg-videos", "--filesystem=xdg-download",
                        "--share=network"],
        "build-options": {"append-path": "/usr/lib/sdk/rust-stable/bin",
                          "env": {"CARGO_HOME": "/run/build/freemkv/cargo"}},
        "modules": [{"name": "freemkv", "buildsystem": "simple", "build-commands": [
            "cd freemkv && cargo build --offline --locked --release --features gui --bin freemkv",
            "install -Dm755 freemkv/target/release/freemkv /app/bin/freemkv",
            "install -Dm644 freemkv/packaging/flatpak/org.freemkv.FreeMKV.desktop /app/share/applications/org.freemkv.FreeMKV.desktop",
            "install -Dm644 freemkv/packaging/flatpak/org.freemkv.FreeMKV.metainfo.xml /app/share/metainfo/org.freemkv.FreeMKV.metainfo.xml",
            "install -Dm644 freemkv/res/freemkv-icon.svg /app/share/icons/hicolor/scalable/apps/org.freemkv.FreeMKV.svg",
        ], "sources": [{"type": "archive", "path": "sources.tar.gz", "sha256": checksum,
                         "strip-components": 0}]}],
    }


def prepare(checkouts, output):
    output.mkdir(parents=True, exist_ok=False)
    with tempfile.TemporaryDirectory() as temp:
        root = Path(temp).resolve()
        revisions = {}
        versions = {}
        for name in REPOS:
            checkout = checkouts / name
            sha = run("git", "rev-parse", "HEAD", cwd=checkout).decode().strip()
            revisions[name] = sha
            dest = root / name
            dest.mkdir()
            archive = run("git", "archive", "--format=tar", sha, cwd=checkout)
            with tarfile.open(fileobj=io.BytesIO(archive)) as contents:
                contents.extractall(dest, filter="data")
            versions[name] = tomllib.loads((dest / "Cargo.toml").read_text())["package"]["version"]
        version = versions["freemkv"]
        if any(v != version for v in versions.values()):
            raise ValueError(f"candidate versions differ: {versions}")
        app = root / "freemkv"
        config = app / ".cargo/config.toml"
        config.parent.mkdir(exist_ok=True)
        config.write_text(patches())
        # Keep the committed lock's dependency versions while resolving candidate paths.
        run("cargo", "metadata", "--format-version=1", cwd=app)
        vendor = run("cargo", "vendor", "--locked", "--versioned-dirs", "--respect-source-config",
                     "../vendor", cwd=app).decode()
        config.write_text(patches() + "\n" + vendor)
        metadata = json.loads(run("cargo", "metadata", "--offline", "--locked", "--format-version=1", cwd=app))
        for name in REPOS:
            packages = [p for p in metadata["packages"] if p["name"] == name]
            if len(packages) != 1 or Path(packages[0]["manifest_path"]) != root / name / "Cargo.toml":
                raise ValueError(f"{name} did not resolve to its pinned source")
        date = run("git", "show", "-s", "--format=%cs", revisions["freemkv"],
                   cwd=checkouts / "freemkv").decode().strip()
        metainfo = app / "packaging/flatpak/org.freemkv.FreeMKV.metainfo.xml"
        update_metadata(metainfo, version, date)
        shutil.copy2(metainfo, output / metainfo.name)
        shutil.copy2(app / "Cargo.lock", output / "Cargo.lock")
        provenance = {"version": version, "revisions": revisions,
                      "lock_sha256": hashlib.sha256((app / "Cargo.lock").read_bytes()).hexdigest()}
        (output / "provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
        with tarfile.open(output / "sources.tar.gz", "w:gz") as archive:
            for child in sorted(root.iterdir()):
                archive.add(child, arcname=child.name)
        checksum = hashlib.sha256((output / "sources.tar.gz").read_bytes()).hexdigest()
        (output / "org.freemkv.FreeMKV.json").write_text(json.dumps(manifest(checksum), indent=2) + "\n")
        shutil.copy2(app / "packaging/flatpak/flathub.json", output / "flathub.json")
        (output / "BUILD.txt").write_text(
            "Sources include all first-party revisions and vendored dependencies with their licenses.\n"
            "See provenance.json for exact source commits and Cargo.lock for dependency versions.\n"
            "Build offline after installing GNOME 51 SDK/runtime and its Rust extension:\n"
            "flatpak-builder --user --force-clean --repo=repo build org.freemkv.FreeMKV.json\n"
            "flatpak build-bundle repo freemkv.flatpak org.freemkv.FreeMKV\n")
    print(f"Prepared freemkv {version} from {revisions['freemkv']}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--checkouts", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    prepare(args.checkouts.resolve(), args.output.resolve())
