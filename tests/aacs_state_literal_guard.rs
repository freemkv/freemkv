//! The `AacsState` literal guard (keys-upfront design v3.4 §2.2, EK9/FK8 literal half).
//!
//! Every Rust file under `src/` (`#[cfg(test)]` included) and `tests/` is scanned for
//! `\b((\w+::)*)AacsState\s*\{`. A match is a return type or a definition, not a
//! literal, when the 80 characters before it end with `->\s*(\w+::)*`, `\bstruct\s+`
//! or `\benum\s+`, or its line is an impl header (`^\s*(pub(\(..\))?\s+)?impl\b`).
//! Every other match fails: a struct literal (build one with
//! `libfreemkv::test_util::aacs_state()`) and, deliberately, a destructuring pattern
//! (a `let` or `Some(..)` that names the type with braces). Reading the key state field
//! by field outside libfreemkv is the coupling KU-X2 removes with those fields.

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

/// Whether `t` ends with the whole word `word`.
fn ends_with_word(t: &str, word: &str) -> bool {
    t.strip_suffix(word)
        .is_some_and(|head| !head.ends_with(is_word))
}

/// Whether `line` is an impl header: `^\s*(pub(\(..\))?\s+)?impl\b`.
fn is_impl_header(line: &str) -> bool {
    let mut l = line.trim_start();
    if let Some(rest) = l.strip_prefix("pub") {
        let rest = match rest.strip_prefix('(') {
            Some(r) => r.split_once(')').map_or("", |(_, r)| r),
            None => rest,
        };
        match strip_ws_prefix(rest) {
            Some(r) => l = r,
            None => return false,
        }
    }
    l.strip_prefix("impl")
        .is_some_and(|r| !r.starts_with(is_word))
}

/// `s` with its leading `\s+` removed, or `None` if it does not start with whitespace.
fn strip_ws_prefix(s: &str) -> Option<&str> {
    let t = s.trim_start();
    (t.len() < s.len()).then_some(t)
}

/// Whether a match is a signature (§2.2): `before` is the 80 characters before it,
/// `prefix` everything before it (for the impl header's line start).
fn is_signature(before: &str, prefix: &str) -> bool {
    if strip_path_suffix(before).trim_end().ends_with("->") {
        return true;
    }
    if strip_ws_suffix(before).is_none() {
        return false;
    }
    let t = prefix.trim_end();
    let line = t.rsplit('\n').next().unwrap_or(t);
    ends_with_word(t, "struct") || ends_with_word(t, "enum") || is_impl_header(line)
}

/// 1-based line numbers of every `AacsState` struct literal in `src`.
fn literal_lines(src: &str) -> Vec<usize> {
    let mut hits = Vec::new();
    for (at, _) in src.match_indices("AacsState") {
        let after = src[at + "AacsState".len()..].trim_start();
        // `\b`: `MyAacsState` is another type.
        if !after.starts_with('{') || src[..at].ends_with(is_word) {
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
        if !is_signature(&before, &src[..start]) {
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
        // Definition and impl headers; another type whose name ends in the type's.
        format!("pub struct {ty} {{"),
        format!("impl Default for {ty} {{"),
        format!("    impl<T> From<T> for libfreemkv::{ty} {{"),
        format!("impl Default for\n    {ty} {{"),
        format!("struct My{ty} {{ }}\nlet m = My{ty} {{ }};"),
    ];
    for s in &signatures {
        assert!(literal_lines(s).is_empty(), "signature matched: {s}");
    }
    // Literals, including ones near the words impl/struct, and patterns (banned).
    let literals = [
        format!("let a = {ty} {{ version: 1, .. }};"),
        format!("let simple = {ty} {{ version: 1, .. }};"),
        format!("let implied = libfreemkv::{ty} {{ version: 1, .. }};"),
        format!("fn build_impl() {{ let a = {ty} {{ version: 1, .. }}; }}"),
        format!("let destruct = {ty} {{ version: 1, .. }};"),
        format!("let {ty} {{ volume_id, .. }} = state;"),
        format!("if let Some({ty} {{ unit_keys, .. }}) = disc.aacs {{}}"),
        format!("aacs: Some(libfreemkv::{ty} {{\n    version: 1,"),
        format!("    libfreemkv::disc::{ty} {{\n        version: 2,"),
    ];
    for s in &literals {
        assert_eq!(literal_lines(s).len(), 1, "literal missed: {s}");
    }
}
