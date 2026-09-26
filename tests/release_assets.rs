//! Release-asset drift: the list `release-orchestrate.yml` verifies on every release must stay
//! stable-named and complete, be what the build workflows upload, and be what INSTALL.md offers.

use std::collections::BTreeSet;
use std::path::Path;

const OSES: [&str; 3] = ["macos", "windows", "linux"];

// Uploaded but not a user download, so INSTALL.md need not name it.
const NOT_A_DOWNLOAD: [&str; 1] = ["freemkv-flatpak-package.tar.gz"];

fn read(rel: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn workflow(name: &str) -> String {
    read(&format!(".github/workflows/{name}"))
}

/// Entries of the `local freemkv_expected=( ... )` bash array, split at the legacy comment.
struct Expected {
    current: Vec<String>,
    legacy: Vec<String>,
}

impl Expected {
    fn all(&self) -> impl Iterator<Item = &String> {
        self.current.iter().chain(&self.legacy)
    }
}

fn expected() -> Expected {
    let yml = workflow("release-orchestrate.yml");
    let start = yml
        .find("freemkv_expected=(")
        .expect("release-orchestrate.yml: no freemkv_expected=( array");
    let body = &yml[start..];
    let body = &body[body.find('(').unwrap() + 1..];
    let body = &body[..body.find(')').expect("unterminated freemkv_expected")];
    let (mut current, mut legacy, mut in_legacy) = (Vec::new(), Vec::new(), false);
    for line in body.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(c) = line.strip_prefix('#') {
            in_legacy = c.to_lowercase().contains("legacy");
            continue;
        }
        for name in line.split_whitespace() {
            if in_legacy { &mut legacy } else { &mut current }.push(name.to_string());
        }
    }
    assert!(current.len() >= 10, "parsed too few assets: {current:?}");
    Expected { current, legacy }
}

// `\d+\.\d+\.\d+`, without a regex dependency.
fn has_version(name: &str) -> bool {
    let b = name.as_bytes();
    (0..b.len()).any(|i| {
        let mut j = i;
        for part in 0..3 {
            let s = j;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j == s {
                return false;
            }
            if part < 2 {
                if j >= b.len() || b[j] != b'.' {
                    return false;
                }
                j += 1;
            }
        }
        true
    })
}

#[test]
fn the_version_detector_works() {
    assert!(has_version("freemkv-1.7.7-amd64.deb"));
    assert!(has_version("freemkv-v10.0.12-x86_64-linux"));
    assert!(!has_version("freemkv-x86_64-linux.AppImage"));
    assert!(!has_version("freemkv-1.7-amd64.deb"));
}

#[test]
fn every_asset_name_is_unique_and_unversioned() {
    let e = expected();
    let mut seen = BTreeSet::new();
    for n in e.all() {
        assert!(seen.insert(n), "{n} listed twice");
        assert!(
            !has_version(n),
            "{n}: versioned names were dropped, use a stable one"
        );
        assert!(n.starts_with("freemkv-"), "{n}");
    }
}

#[test]
fn every_asset_has_a_sha256_partner() {
    let e = expected();
    let yml = workflow("release-orchestrate.yml");
    // The list is written without hashes; one bash line adds `.sha256` to every entry.
    assert!(
        yml.contains(r#"freemkv_expected+=("${freemkv_expected[@]/%/.sha256}")"#),
        "the .sha256 expansion of freemkv_expected is gone"
    );
    for n in e.all().filter(|n| n.ends_with(".sha256")) {
        let base = n.trim_end_matches(".sha256");
        assert!(e.all().any(|m| m == base), "{n} hashes nothing");
    }
}

#[test]
fn every_os_ships_an_app_and_a_cli() {
    let e = expected();
    for os in OSES {
        let on = |n: &&String| n.contains(os);
        assert!(
            e.current
                .iter()
                .filter(on)
                .any(|n| !n.starts_with("freemkv-cli-")),
            "no app asset for {os}"
        );
        assert!(
            e.current
                .iter()
                .filter(on)
                .any(|n| n.starts_with("freemkv-cli-")),
            "no freemkv-cli-* asset for {os}"
        );
    }
    for must in [
        "freemkv-x86_64-windows-setup.exe",
        "freemkv-x86_64-windows.zip",
        "freemkv-amd64.deb",
        "freemkv-cli-amd64.deb",
        "freemkv-x86_64-linux.AppImage",
        "freemkv-x86_64-linux.flatpak",
    ] {
        assert!(
            e.current.iter().any(|n| n == must),
            "{must} is not verified"
        );
    }
}

#[test]
fn legacy_aliases_are_the_cli_names_without_cli() {
    let e = expected();
    assert!(!e.legacy.is_empty(), "legacy section not found");
    for l in &e.legacy {
        let cli = l.replacen("freemkv-", "freemkv-cli-", 1);
        assert!(e.current.contains(&cli), "{l} aliases no current {cli}");
    }
}

/// Names a workflow can upload: literal `freemkv-*` tokens (and their hash), plus each release.yml matrix
/// `asset:`/`legacy:` joined with every suffix its `files:` block uses.
fn produced() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for f in ["deb.yml", "appimage.yml", "flatpak.yml"] {
        let y = workflow(f);
        for tok in y.split(|c: char| !(c.is_ascii_alphanumeric() || "-_.".contains(c))) {
            if tok.starts_with("freemkv-") {
                let tok = tok.trim_end_matches('.');
                // These hash in a shell loop (`> "$f.sha256"`), so the pair isn't literal.
                if y.contains("sha256sum") {
                    out.insert(format!("{tok}.sha256"));
                }
                out.insert(tok.to_string());
            }
        }
    }
    let rel = workflow("release.yml");
    // Split at the two-space job keys under `jobs:`.
    let mut jobs: Vec<Vec<&str>> = vec![Vec::new()];
    for line in rel.lines() {
        let job_key = line.starts_with("  ") && !line[2..].starts_with([' ', '#']);
        if job_key && line.trim_end().ends_with(':') {
            jobs.push(Vec::new());
        }
        jobs.last_mut().unwrap().push(line);
    }
    for job in jobs {
        let mut entries: Vec<(Vec<String>, String)> = Vec::new();
        let mut suffixes = BTreeSet::new();
        let mut in_files = false;
        for line in job {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("- ")
                && rest.contains(':')
                && !rest.starts_with("uses:")
                && !rest.starts_with("name:")
            {
                entries.push((Vec::new(), String::new()));
            }
            let kv = t.trim_start_matches("- ");
            if let Some((k, v)) = kv.split_once(':') {
                let v = v.trim().trim_matches('\'').trim_matches('"').to_string();
                if let Some(e) = entries.last_mut() {
                    match k.trim() {
                        "asset" | "legacy" => e.0.push(v),
                        "ext" => e.1 = v,
                        _ => {}
                    }
                }
            }
            if t == "files: |" {
                in_files = true;
                continue;
            }
            if in_files {
                if !t.starts_with("${{") {
                    in_files = false;
                    continue;
                }
                for var in ["${{ matrix.asset }}", "${{ matrix.legacy }}"] {
                    if let Some(s) = t.strip_prefix(var) {
                        suffixes.insert(s.to_string());
                    }
                }
            }
        }
        for (names, ext) in &entries {
            for n in names {
                for s in &suffixes {
                    out.insert(format!("{n}{}", s.replace("${{ matrix.ext }}", ext)));
                }
            }
        }
    }
    out
}

#[test]
fn every_verified_asset_is_uploaded_by_a_build_workflow() {
    let made = produced();
    let e = expected();
    for n in e.all() {
        assert!(
            made.contains(n),
            "{n} is verified but no workflow uploads it"
        );
        let sha = format!("{n}.sha256");
        assert!(made.contains(&sha), "{sha} is verified but never uploaded");
    }
}

#[test]
fn install_md_offers_every_current_asset() {
    let doc = read("INSTALL.md");
    let e = expected();
    for n in e
        .current
        .iter()
        .filter(|n| !NOT_A_DOWNLOAD.contains(&n.as_str()))
    {
        if doc.contains(n.as_str()) {
            continue;
        }
        // "`freemkv-aarch64-macos.dmg` / `.zip`": a sibling extension on the same line.
        let (stem, ext) = n.rsplit_once('.').unwrap_or((n, ""));
        let shorthand = !ext.is_empty()
            && doc
                .lines()
                .any(|l| l.contains(&format!("{stem}.")) && l.contains(&format!("`.{ext}`")));
        assert!(shorthand, "INSTALL.md never mentions {n}");
    }
}
