#!/usr/bin/env python3
"""Check that every published download of freemkv VERSION resolves to the right bytes.

The last step of release-orchestrate, after the site is live:

  - every release asset has a `.sha256` whose hash is the asset's own (GitHub's digest);
  - every download link on every freemkv.org page (sitemap) names an asset of the release
    and resolves (HTTP 200) to the VERSION asset, `releases/latest/download/` included;
  - the Homebrew cask/formula and Scoop manifests name VERSION assets with their hashes.

Polls until everything checks or --wait runs out, then prints one OK/FAIL line per check
and exits 1 on any failure.

    verify-download-links.py 1.8.2 [--wait 1800]
"""
import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

REPO = "freemkv/freemkv"
SITE = "https://freemkv.org"
TAP = "https://raw.githubusercontent.com/freemkv/homebrew-tap/main"
SCOOP = "https://raw.githubusercontent.com/freemkv/scoop-bucket/main/bucket"
TAP_FILES = ("Casks/freemkv.rb", "Formula/freemkv-cli.rb")
SCOOP_FILES = ("freemkv.json", "freemkv-cli.json")
ASSET_LINK = re.compile(
    r"https://github\.com/freemkv/freemkv/releases/(?:latest/download|download/(v[^/\"'\s<>]+))/([^\"'\s<>?#)]+)")


def request(url, method="GET", api=False):
    headers = {"User-Agent": "freemkv-release"}
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if api and token:
        headers["Authorization"] = f"Bearer {token}"
    return urllib.request.urlopen(urllib.request.Request(url, headers=headers, method=method), timeout=60)


def fetch(url, api=False):
    with request(url, api=api) as r:
        return r.read()


def release_assets(version):
    rel = json.loads(fetch(f"https://api.github.com/repos/{REPO}/releases/tags/v{version}", api=True))
    return {a["name"]: (a.get("digest") or "").removeprefix("sha256:") for a in rel["assets"]}


def check_checksums(version, assets):
    for name, digest in sorted(assets.items()):
        if name.endswith((".sha256", ".sig")):
            continue
        if not digest:
            yield f"asset {name}", "FAIL (GitHub publishes no digest)"
            continue
        if name + ".sha256" not in assets:
            yield f"asset {name}", "FAIL (no .sha256 published)"
            continue
        text = fetch(f"https://github.com/{REPO}/releases/download/v{version}/{name}.sha256").decode()
        got = (text.split() or [""])[0].lower()
        yield f"asset {name}.sha256", "OK" if got == digest else f"FAIL ({got[:12]} != {digest[:12]})"


def site_links():
    """Every release-asset link on every page in the sitemap: {url: [pages]}."""
    index = fetch(f"{SITE}/sitemap-index.xml").decode()
    pages = []
    for sitemap in re.findall(r"<loc>([^<]+)</loc>", index):
        pages += re.findall(r"<loc>([^<]+)</loc>", fetch(sitemap).decode())
    links = {}
    for page in pages:
        for m in ASSET_LINK.finditer(fetch(page).decode(errors="replace")):
            links.setdefault(m.group(0), []).append(page.removeprefix(SITE))
    return links


def resolves(url):
    try:
        with request(url, method="HEAD") as r:
            return r.status, r.url
    except urllib.error.HTTPError as e:
        return e.code, url


def check_site(version, assets):
    links = site_links()
    if not links:
        yield "site download links", "FAIL (no release-asset links found on any page)"
    for url, pages in sorted(links.items()):
        m = ASSET_LINK.fullmatch(url)
        tag, name = m.group(1), m.group(2)
        where = f"site {name} ({pages[0]}{' +' + str(len(pages) - 1) if len(pages) > 1 else ''})"
        if tag is None and name not in assets:
            yield where, f"FAIL (v{version} has no asset {name})"
            continue
        status, final = resolves(url)
        if status != 200:
            yield where, f"FAIL (HTTP {status})"
        elif tag is None and f"/v{version}/" not in final and f"%2Fv{version}%2F" not in final \
                and "release-assets" not in final:
            yield where, f"FAIL (resolves to {final[:80]})"
        else:
            yield where, "OK"


def check_url_hash(label, url, sha, version, assets):
    m = ASSET_LINK.fullmatch(url)
    if not m or m.group(1) != f"v{version}":
        return label, f"FAIL (not a v{version} asset: {url})"
    name = m.group(2)
    if name not in assets:
        return label, f"FAIL (v{version} has no asset {name})"
    return label, "OK" if sha.lower() == assets[name] else f"FAIL (hash {sha[:12]} != {assets[name][:12]})"


def tap_pairs(text, version):
    """(url, sha256) per download in a cask (`arch arm:/intel:` + `sha256 arm:/intel:` over one
    `#{arch}` url) or a formula (one url + sha256 per `on_arm`/`on_intel` block)."""
    urls = [u.replace("#{version}", version) for u in re.findall(r'^\s*url\s+"([^"]+)"', text, re.M)]
    arch = re.search(r'^\s*arch\s+arm:\s*"([^"]+)",\s*intel:\s*"([^"]+)"', text, re.M)
    per_arch = re.search(r'sha256\s+arm:\s*"([0-9a-f]{64})",\s*intel:\s*"([0-9a-f]{64})"', text)
    if arch and per_arch and len(urls) == 1:
        return [(urls[0].replace("#{arch}", arch.group(i)), per_arch.group(i)) for i in (1, 2)]
    shas = re.findall(r'^\s*sha256\s+"([0-9a-f]{64})"', text, re.M)
    return list(zip(urls, shas)) if len(urls) == len(shas) else []


def check_tap(version, assets):
    for path in TAP_FILES:
        text = fetch(f"{TAP}/{path}").decode()
        ver = re.search(r'^\s*version\s+"([^"]+)"', text, re.M)
        if ver and ver.group(1) != version:
            yield f"homebrew {path}", f"FAIL (version {ver.group(1)})"
            continue
        pairs = tap_pairs(text, version)
        if not pairs:
            yield f"homebrew {path}", "FAIL (could not pair its urls with sha256 hashes)"
        for url, sha in pairs:
            yield check_url_hash(f"homebrew {path} {url.rsplit('/', 1)[-1]}", url, sha, version, assets)


def check_scoop(version, assets):
    for path in SCOOP_FILES:
        manifest = json.loads(fetch(f"{SCOOP}/{path}"))
        if manifest.get("version") != version:
            yield f"scoop {path}", f"FAIL (version {manifest.get('version')})"
            continue
        entries = list((manifest.get("architecture") or {}).items()) or [("", manifest)]
        for arch, entry in entries:
            urls, hashes = entry.get("url"), entry.get("hash")
            urls = urls if isinstance(urls, list) else [urls]
            hashes = hashes if isinstance(hashes, list) else [hashes]
            for url, sha in zip(urls, hashes):
                url = url.split("#", 1)[0]  # `#/name.exe` renames the download; not part of the URL
                yield check_url_hash(f"scoop {path} {arch}".rstrip(), url, sha or "", version, assets)


def run(version):
    assets = release_assets(version)
    results = []
    for check in (check_checksums, check_site, check_tap, check_scoop):
        try:
            results += list(check(version, assets))
        except Exception as e:  # noqa: BLE001 — a fetch failure is a FAIL line, polled again
            results.append((check.__name__, f"FAIL ({type(e).__name__}: {e})"))
    return results


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("version")
    ap.add_argument("--wait", type=int, default=0, help="seconds to keep polling")
    ap.add_argument("--poll", type=int, default=60)
    args = ap.parse_args()
    deadline = time.time() + args.wait
    while True:
        results = run(args.version)
        failed = [r for r in results if not r[1].startswith("OK")]
        if not failed or time.time() >= deadline:
            break
        print(f"{len(failed)} download check(s) not yet OK; polling again in {args.poll}s", flush=True)
        time.sleep(args.poll)
    for label, status in results:
        print(f"  {label:<60} {status}")
    if failed:
        print(f"{len(failed)} of {len(results)} download checks FAILED for {args.version}")
        sys.exit(1)
    print(f"all {len(results)} download checks OK for {args.version}")


if __name__ == "__main__":
    main()
