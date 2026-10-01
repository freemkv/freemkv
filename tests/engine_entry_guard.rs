//! Anti-drift §4 (user decision 2026-09-30): front-end code (the CLI, the desktop app, the
//! server) may not call libfreemkv or the engine's lower layers directly for engine-owned
//! work — opening a drive, acquiring keys, copying, recovering, muxing, extracting. That work
//! goes through `freemkv_engine::run(Plan)`.
//!
//! The direct calls that predate the measure are listed in `BASELINE` with their counts. The
//! list may only shrink: a new direct call fails here, and so does a baseline entry the code
//! no longer needs (remove it, so the count cannot creep back).

use std::collections::BTreeMap;
use std::path::Path;

/// Calls that are engine-owned work when a front end makes them.
const FORBIDDEN: &[&str] = &[
    "libfreemkv::mux_with_keys(",
    "libfreemkv::mux_url(",
    "libfreemkv::input(",
    "libfreemkv::write_image(",
    ".extract_tree(",
    "KeyRing::acquire",
    "freemkv_engine::copy(",
    "freemkv_engine::sweep(",
    "freemkv_engine::patch(",
    "fe::copy(",
    "fe::sweep(",
    "fe::patch(",
    "DiscSession::open",
    "Drive::open(",
];

/// Front-end files allowed a direct call today, and how many. May only shrink.
const BASELINE: &[(&str, &str, usize)] = &[
    ("src/cli_entry.rs", "libfreemkv::input(", 1),
    ("src/disc_info.rs", "DiscSession::open", 1),
    ("src/engine.rs", "libfreemkv::mux_with_keys(", 1),
    ("src/engine.rs", "libfreemkv::input(", 1),
    ("src/engine.rs", "Drive::open(", 2),
    ("src/info.rs", "Drive::open(", 1),
    ("src/pipe.rs", "libfreemkv::mux_with_keys(", 1),
    ("src/pipe.rs", "libfreemkv::mux_url(", 1),
    ("src/server/ripper/mod.rs", "freemkv_engine::sweep(", 1),
    ("src/server/ripper/mod.rs", "freemkv_engine::patch(", 1),
    ("src/server/ripper/mod.rs", "DiscSession::open", 2),
    ("src/server/ripper/mod.rs", "Drive::open(", 5),
    ("src/server/ripper/mux.rs", "libfreemkv::mux_with_keys(", 1),
    ("src/server/ripper/session.rs", "Drive::open(", 1),
];

// Test-only files: fixtures may build what they test against.
const TEST_ONLY: &[&str] = &[
    "src/ku_fixture.rs",
    "src/ku_fixtures.rs",
    "src/test_support.rs",
];

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

// The production half of a source file: everything before its first test module.
fn production(src: &str) -> &str {
    match src.find("\n#[cfg(test)]\nmod ") {
        Some(i) => &src[..i],
        None => src,
    }
}

#[test]
fn front_ends_reach_engine_owned_work_only_through_the_engine() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    let mut found: BTreeMap<(String, &str), usize> = BTreeMap::new();
    for f in &files {
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if TEST_ONLY.contains(&rel.as_str()) {
            continue;
        }
        let src = std::fs::read_to_string(f).unwrap().replace("\r\n", "\n");
        for pat in FORBIDDEN {
            let n = production(&src).matches(pat).count();
            if n > 0 {
                found.insert((rel.clone(), pat), n);
            }
        }
    }
    let baseline: BTreeMap<(String, &str), usize> = BASELINE
        .iter()
        .map(|&(f, p, n)| ((f.to_string(), p), n))
        .collect();
    let mut problems = Vec::new();
    for (k, &n) in &found {
        match baseline.get(k) {
            None => problems.push(format!(
                "{}: new direct call `{}` x{n} — go through freemkv_engine::run(Plan)",
                k.0, k.1
            )),
            Some(&b) if n > b => problems.push(format!(
                "{}: `{}` x{n}, baseline {b} — go through freemkv_engine::run(Plan)",
                k.0, k.1
            )),
            Some(&b) if n < b => problems.push(format!(
                "{}: `{}` x{n}, baseline {b} — shrink BASELINE",
                k.0, k.1
            )),
            Some(_) => {}
        }
    }
    for k in baseline.keys() {
        if !found.contains_key(k) {
            problems.push(format!(
                "{}: `{}` is gone — remove it from BASELINE",
                k.0, k.1
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
