//! The `AacsState` literal guard (keys-upfront design v3.4 §2.2, EK9/FK8 literal half).
//!
//! Every Rust file under `src/` (`#[cfg(test)]` included) and `tests/` is scanned for
//! `((\w+::)*)AacsState\s*\{`. A match is a return type or a definition, not a
//! literal, when the 80 characters before it end with `->\s*(\w+::)*`, `struct\s+`,
//! `enum\s+` or `impl…\s+`. Every other match is a struct literal, and the only
//! sanctioned construction is `libfreemkv::test_util::aacs_state()`, so the fields
//! can later be removed from `AacsState` without touching this repo.

use std::path::{Path, PathBuf};

/// Characters before a match that decide whether it is a signature (§2.2).
const LOOKBEHIND: usize = 80;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `s` with a trailing `(\w+::)*` removed.
fn strip_path_suffix(mut s: &str) -> &str {
    while let Some(head) = s.strip_suffix("::") {
        let trimmed = head.trim_end_matches(is_word);
        if trimmed.len() == head.len() {
            break;
        }
        s = trimmed;
    }
    s
}

/// `s` with its trailing `\s+` removed, or `None` if it does not end in whitespace.
fn strip_ws_suffix(s: &str) -> Option<&str> {
    let t = s.trim_end();
    (t.len() < s.len()).then_some(t)
}

/// Whether the text before a match ends with an excluded prefix (§2.2).
fn is_signature(before: &str) -> bool {
    if strip_path_suffix(before).trim_end().ends_with("->") {
        return true;
    }
    let Some(t) = strip_ws_suffix(before) else {
        return false;
    };
    let last_line = t.rsplit('\n').next().unwrap_or(t);
    t.ends_with("struct") || t.ends_with("enum") || last_line.contains("impl")
}

/// 1-based line numbers of every `AacsState` struct literal in `src`.
fn literal_lines(src: &str) -> Vec<usize> {
    let mut hits = Vec::new();
    for (at, _) in src.match_indices("AacsState") {
        let after = src[at + "AacsState".len()..].trim_start();
        if !after.starts_with('{') {
            continue;
        }
        // The leftmost match start: extend back over `(\w+::)*`.
        let start = strip_path_suffix(&src[..at]).len();
        let before: String = {
            let chars: Vec<char> = src[..start].chars().collect();
            chars[chars.len().saturating_sub(LOOKBEHIND)..]
                .iter()
                .collect()
        };
        if !is_signature(&before) {
            hits.push(src[..at].matches('\n').count() + 1);
        }
    }
    hits
}

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rs_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Guard: no `AacsState` struct literal anywhere in this crate's `src/` or `tests/`.
/// Build one with `libfreemkv::test_util::aacs_state()` (KU design §2.2, KU-P1).
#[test]
fn no_aacs_state_struct_literals_outside_test_util() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    rs_files(&root.join("tests"), &mut files);
    assert!(
        files
            .iter()
            .any(|f| f.ends_with("aacs_state_literal_guard.rs")),
        "the scan must reach tests/ (found {} files)",
        files.len()
    );
    let mut hits = Vec::new();
    for f in &files {
        let src = std::fs::read_to_string(f).expect("read source");
        for line in literal_lines(&src) {
            hits.push(format!(
                "{}:{line}",
                f.strip_prefix(root).unwrap().display()
            ));
        }
    }
    assert!(
        hits.is_empty(),
        "AacsState struct literals (use libfreemkv::test_util::aacs_state()):\n{}",
        hits.join("\n")
    );
}

/// Self-test: the 7 signature shapes of the §2.2 proof never match; a literal does.
#[test]
fn the_guard_skips_signatures_and_catches_literals() {
    let ty = "AacsState";
    // engine recovery_copy_dispatch.rs:115, resolve.rs:199, preflight.rs:249,
    // recovery/mapfile.rs:2358; freemkv pipe.rs:5785, engine.rs:3257, disc_capture.rs:529.
    let signatures = [
        format!("fn aacs_with(unit_keys: Vec<(u32, [u8; 16])>) -> {ty} {{"),
        format!("    fn aacs(origin: libfreemkv::KeyOrigin) -> libfreemkv::{ty} {{"),
        format!("    fn resolved_aacs() -> libfreemkv::{ty} {{"),
        format!(
            "    fn aacs_with(\n        unit_keys: Vec<(u32, [u8; 16])>,\n        \
             volume_id: [u8; 16],\n    ) -> libfreemkv::disc::{ty} {{"
        ),
        format!("    pub(super) fn aacs(unit_keys: Vec<(u32, [u8; 16])>) -> libfreemkv::{ty} {{"),
        format!("    pub(super) fn aacs(unit_keys: Vec<(u32, [u8; 16])>) -> libfreemkv::{ty} {{"),
        format!("    fn aacs_with_secrets(disc_hash: &str) -> {ty} {{"),
        // Definition and impl headers.
        format!("pub struct {ty} {{"),
        format!("impl Default for {ty} {{"),
    ];
    for s in &signatures {
        assert!(literal_lines(s).is_empty(), "signature matched: {s}");
    }
    let literals = [
        format!("let a = {ty} {{ version: 1, .. }};"),
        format!("aacs: Some(libfreemkv::{ty} {{\n    version: 1,"),
        format!("    libfreemkv::disc::{ty} {{\n        version: 2,"),
    ];
    for s in &literals {
        assert_eq!(literal_lines(s).len(), 1, "literal missed: {s}");
    }
}
