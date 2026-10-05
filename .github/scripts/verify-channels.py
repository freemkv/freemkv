#!/usr/bin/env python3
"""Check that every install channel serves freemkv VERSION.

Run by release-orchestrate after the release assets are verified; polls until
every channel is current or --wait runs out, then prints one OK/MISSING line
per check and exits 1 if anything is missing.

    verify-channels.py 1.8.1 [--wait 3600] [--skip snap,rpm]
"""
import argparse
import gzip
import io
import json
import re
import sys
import tarfile
import time
import urllib.request
import xml.etree.ElementTree as ET

IMAGES = ("freemkv-library", "autorip")
IMAGE_PLATFORMS = {"linux/amd64", "linux/arm64", "linux/arm/v7"}
SITE = "https://freemkv.org"
TAP = "https://raw.githubusercontent.com/freemkv/homebrew-tap/main"
# Package -> architectures it ships for, per repository's architecture names.
APT = {"amd64": ("freemkv", "freemkv-cli"), "arm64": ("freemkv", "freemkv-cli"), "armhf": ("freemkv-cli",)}
RPM = {"x86_64": ("freemkv", "freemkv-cli"), "aarch64": ("freemkv", "freemkv-cli"), "armv7hl": ("freemkv-cli",)}
PACMAN = {"x86_64": ("freemkv", "freemkv-cli"), "aarch64": ("freemkv", "freemkv-cli"), "armv7h": ("freemkv-cli",)}
SNAP_ARCHES = ("amd64",)


def fetch(url, headers=None):
    req = urllib.request.Request(url, headers={"User-Agent": "freemkv-release", **(headers or {})})
    with urllib.request.urlopen(req, timeout=60) as r:
        return r.read()


def check_images(version):
    for image in IMAGES:
        token = json.loads(fetch(f"https://ghcr.io/token?scope=repository:freemkv/{image}:pull"))["token"]
        accept = ", ".join((
            "application/vnd.oci.image.index.v1+json",
            "application/vnd.docker.distribution.manifest.list.v2+json",
        ))
        digests = {}
        for tag in (f"v{version}", "latest"):
            name = f"ghcr.io/freemkv/{image}:{tag}"
            try:
                req = urllib.request.Request(
                    f"https://ghcr.io/v2/freemkv/{image}/manifests/{tag}",
                    headers={"Authorization": f"Bearer {token}", "Accept": accept})
                with urllib.request.urlopen(req, timeout=60) as r:
                    digests[tag] = r.headers.get("Docker-Content-Digest")
                    index = json.loads(r.read())
            except Exception as e:
                yield name, False, f"not found ({e})"
                continue
            if "manifests" not in index:
                yield name, False, "single-platform image, not a multi-arch index"
                continue
            have = set()
            for m in index["manifests"]:
                p = m.get("platform", {})
                if p.get("os") == "unknown":
                    continue  # build attestations
                have.add("/".join(x for x in (p.get("os"), p.get("architecture"), p.get("variant")) if x))
            missing = IMAGE_PLATFORMS - have
            yield name, not missing, f"missing {', '.join(sorted(missing))}" if missing else ", ".join(sorted(have))
        if len(digests) == 2:
            same = digests[f"v{version}"] == digests["latest"]
            yield f"ghcr.io/freemkv/{image}:latest = v{version}", same, "" if same else "latest points elsewhere"


def check_tap(version):
    cask = fetch(f"{TAP}/Casks/freemkv.rb").decode()
    ok = f'version "{version}"' in cask
    yield "homebrew cask freemkv", ok, "" if ok else "cask version differs"
    formula = fetch(f"{TAP}/Formula/freemkv-cli.rb").decode()
    ok = f"/v{version}/" in formula
    yield "homebrew formula freemkv-cli", ok, "" if ok else "formula URL is another version"


def check_snap(version):
    info = json.loads(fetch("https://api.snapcraft.io/v2/snaps/info/freemkv", {"Snap-Device-Series": "16"}))
    stable = {c["channel"]["architecture"]: c["version"]
              for c in info["channel-map"] if c["channel"]["name"] == "stable"}
    for arch in SNAP_ARCHES:
        got = stable.get(arch)
        yield f"snap stable {arch}", got == version, f"stable is {got}"


def check_apt(version):
    for arch, names in APT.items():
        try:
            text = fetch(f"{SITE}/apt/dists/stable/main/binary-{arch}/Packages").decode()
        except Exception:
            text = ""
        have = {}
        for stanza in text.split("\n\n"):
            fields = dict(re.findall(r"^(Package|Version): (.+)$", stanza, re.M))
            if "Package" in fields:
                have.setdefault(fields["Package"], set()).add(fields.get("Version"))
        for name in names:
            got = have.get(name, set())
            yield f"apt {name} {arch}", version in got, f"has {sorted(got) or 'nothing'}"


def check_rpm(version):
    have = set()
    try:
        repomd = ET.fromstring(fetch(f"{SITE}/rpm/repodata/repomd.xml"))
        ns = {"r": "http://linux.duke.edu/metadata/repo"}
        href = next(d.find("r:location", ns).get("href")
                    for d in repomd.findall("r:data", ns) if d.get("type") == "primary")
        primary = ET.fromstring(gzip.decompress(fetch(f"{SITE}/rpm/{href}")))
        c = {"c": "http://linux.duke.edu/metadata/common"}
        for p in primary.findall("c:package", c):
            have.add((p.find("c:name", c).text, p.find("c:arch", c).text, p.find("c:version", c).get("ver")))
    except Exception:
        pass
    for arch, names in RPM.items():
        for name in names:
            ok = (name, arch, version) in have
            yield f"rpm {name} {arch}", ok, "" if ok else "not in repodata"


def check_pacman(version):
    for arch, names in PACMAN.items():
        have = set()
        try:
            with tarfile.open(fileobj=io.BytesIO(fetch(f"{SITE}/arch/{arch}/freemkv.db"))) as db:
                for m in db.getmembers():
                    if m.name.endswith("/desc"):
                        desc = db.extractfile(m).read().decode()
                        fields = dict(re.findall(r"%(NAME|VERSION)%\n(.+)", desc))
                        have.add((fields.get("NAME"), fields.get("VERSION", "").rsplit("-", 1)[0]))
        except Exception:
            pass
        for name in names:
            ok = (name, version) in have
            yield f"pacman {name} {arch}", ok, "" if ok else "not in freemkv.db"


CHECKS = {"images": check_images, "tap": check_tap, "snap": check_snap,
          "apt": check_apt, "rpm": check_rpm, "pacman": check_pacman}


def run(version, skip):
    results = []
    for key, check in CHECKS.items():
        if key in skip:
            continue
        try:
            results += list(check(version))
        except Exception as e:
            results.append((key, False, f"check failed: {e}"))
    return results


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("version")
    ap.add_argument("--wait", type=int, default=0, help="seconds to keep polling")
    ap.add_argument("--poll", type=int, default=60)
    ap.add_argument("--skip", default="", help="comma-separated: " + ",".join(CHECKS))
    a = ap.parse_args()
    skip = set(filter(None, a.skip.split(",")))
    deadline = time.time() + a.wait
    while True:
        results = run(a.version, skip)
        pending = [name for name, ok, _ in results if not ok]
        if not pending or time.time() >= deadline:
            break
        print(f"  waiting on {len(pending)}: {', '.join(pending)}", flush=True)
        time.sleep(a.poll)
    for name, ok, detail in results:
        print(f"  {name:<44} {'OK' if ok else 'MISSING'}{'  ' + detail if detail and not ok else ''}")
    if pending:
        print(f"{len(pending)} channel check(s) are not serving {a.version}", file=sys.stderr)
        return 1
    print(f"every channel serves {a.version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
