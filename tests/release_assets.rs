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
    // A listed hash would be expanded again into a `.sha256.sha256` nothing uploads.
    for n in e.all() {
        assert!(!n.ends_with(".sha256"), "{n}: list the base name only");
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

/// Names a workflow uploads: its `files: |` blocks (literal names, or each matrix `asset:`/`legacy:`
/// joined with the suffixes used) and its `gh release upload` arguments, where `name*` expands to
/// `name` plus `name.sha256` when the job hashes `name`.
fn produced() -> BTreeSet<String> {
    let files = [
        "deb.yml",
        "appimage.yml",
        "flatpak.yml",
        "release.yml",
        "snap.yml",
    ];
    let texts: Vec<(&str, String)> = files.iter().map(|f| (*f, workflow(f))).collect();
    produced_from(&texts)
}

/// Splits a workflow at the two-space job keys under `jobs:`.
fn jobs(y: &str) -> Vec<Vec<&str>> {
    let mut jobs: Vec<Vec<&str>> = vec![Vec::new()];
    for line in y.lines() {
        let job_key = line.starts_with("  ") && !line[2..].starts_with([' ', '#']);
        if job_key && line.trim_end().ends_with(':') {
            jobs.push(Vec::new());
        }
        jobs.last_mut().unwrap().push(line);
    }
    jobs
}

/// Names a job hashes as `sha256sum "X" > "X.sha256"` or `for f in X Y; do sha256sum "$f" > "$f.sha256"`.
fn hashed(job: &[&str]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let lines: Vec<&str> = job.iter().map(|l| l.trim()).collect();
    for (i, t) in lines.iter().enumerate() {
        if let Some(rest) = t.strip_prefix("for ")
            && let Some((var, rest)) = rest.split_once(" in ")
            && let Some((list, body)) = rest.split_once("; do")
        {
            let tail = lines[i + 1..].iter().take_while(|l| !l.starts_with("done"));
            let body = std::iter::once(body)
                .chain(tail.copied())
                .collect::<Vec<_>>();
            let hash = format!(r#"sha256sum "${var}" > "${var}.sha256""#);
            if body.iter().any(|l| l.contains(&hash)) {
                out.extend(list.split_whitespace().map(String::from));
            }
        } else if let Some(rest) = t.strip_prefix("sha256sum \"")
            && let Some((name, redirect)) = rest.split_once('"')
            && redirect.trim() == format!(r#"> "{name}.sha256""#)
        {
            out.insert(name.to_string());
        }
    }
    out
}

/// Arguments of every `gh release upload <tag> ...` command, `\` continuations joined.
fn gh_uploads(job: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = job.iter().map(|l| l.trim());
    while let Some(t) = lines.next() {
        let Some((_, rest)) = t.split_once("gh release upload ") else {
            continue;
        };
        let mut cmd = rest.to_string();
        while cmd.ends_with('\\') {
            cmd.pop();
            cmd.push(' ');
            cmd.push_str(lines.next().unwrap_or_default());
        }
        let args = cmd
            .split_whitespace()
            .skip(1)
            .filter(|a| !a.starts_with('-'));
        out.extend(args.map(|a| a.trim_matches('"').to_string()));
    }
    out
}

fn produced_from(workflows: &[(&str, String)]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (_, y) in workflows {
        for job in jobs(y) {
            let hashes = hashed(&job);
            for arg in gh_uploads(&job) {
                match arg.strip_suffix('*') {
                    Some(name) => {
                        out.insert(name.to_string());
                        if hashes.contains(name) {
                            out.insert(format!("{name}.sha256"));
                        }
                    }
                    None => {
                        out.insert(arg);
                    }
                }
            }
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
                    if t.starts_with("freemkv-") {
                        out.insert(t.to_string());
                        continue;
                    }
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
    }
    out
}

fn edit(y: &str, from: &str, to: &str) -> String {
    assert!(y.contains(from), "fixture text {from:?} not found");
    y.replacen(from, to, 1)
}

#[test]
fn only_real_upload_steps_count_as_produced() {
    let deb = edit(
        &workflow("deb.yml"),
        "freemkv-cli-amd64.deb freemkv-cli-amd64.deb.sha256",
        "",
    );
    let made = produced_from(&[("deb.yml", deb)]);
    assert!(made.contains("freemkv-amd64.deb.sha256"), "{made:?}");
    assert!(!made.contains("freemkv-cli-amd64.deb"), "{made:?}");
    assert!(!made.contains("freemkv-cli-amd64.deb.sha256"), "{made:?}");

    let appimage = produced_from(&[("appimage.yml", workflow("appimage.yml"))]);
    let want: BTreeSet<String> = [
        "freemkv-x86_64-linux.AppImage",
        "freemkv-x86_64-linux.AppImage.sha256",
    ]
    .map(String::from)
    .into();
    assert_eq!(appimage, want);

    let flatpak = produced_from(&[("flatpak.yml", workflow("flatpak.yml"))]);
    for n in [
        "freemkv-x86_64-linux.flatpak",
        "freemkv-flatpak-package.tar.gz",
    ] {
        assert!(
            flatpak.contains(n) && flatpak.contains(&format!("{n}.sha256")),
            "{flatpak:?}"
        );
    }
    let unhashed = edit(
        &workflow("flatpak.yml"),
        "linux.flatpak freemkv-flatpak-package.tar.gz; do",
        "linux.flatpak; do",
    );
    let made = produced_from(&[("flatpak.yml", unhashed)]);
    assert!(
        !made.contains("freemkv-flatpak-package.tar.gz.sha256"),
        "{made:?}"
    );
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
        // "`<stem>.<a>` / `.<b>`": a sibling extension on the same line.
        let (stem, ext) = n.rsplit_once('.').unwrap_or((n, ""));
        let shorthand = !ext.is_empty()
            && doc
                .lines()
                .any(|l| l.contains(&format!("{stem}.")) && l.contains(&format!("`.{ext}`")));
        assert!(shorthand, "INSTALL.md never mentions {n}");
    }
}

/// The `local freemkv_optional=(...)` list: verified with a warning, never a failure.
fn optional() -> Vec<String> {
    let yml = workflow("release-orchestrate.yml");
    let start = yml
        .find("freemkv_optional=(")
        .expect("release-orchestrate.yml: no freemkv_optional=( array");
    let body = &yml[start + "freemkv_optional=(".len()..];
    body[..body.find(')').unwrap()]
        .split_whitespace()
        .map(String::from)
        .collect()
}

#[test]
fn optional_assets_are_the_snap_and_stay_off_the_required_list() {
    let opt = optional();
    assert_eq!(opt, ["freemkv-amd64.snap", "freemkv-amd64.snap.sha256"]);
    let e = expected();
    let made = produced();
    let doc = read("INSTALL.md");
    for n in &opt {
        assert!(!e.all().any(|m| m == n), "{n} must not block a release");
        assert!(!has_version(n), "{n}");
        assert!(
            made.contains(n),
            "{n} is verified but no workflow uploads it"
        );
        assert!(doc.contains(n.as_str()), "INSTALL.md never mentions {n}");
    }
}
