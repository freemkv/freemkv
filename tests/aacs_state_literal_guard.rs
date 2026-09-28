//! FK8, both halves (keys-upfront design §2.2): the `AacsState` literal guard, and the
//! structural guard against the legacy key APIs, which freemkv bans with no allow-path.
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

/// FK8 structural half (KU §2.2): banned in freemkv "as engine, with no allow-path". The
/// §2.2 "Removed" rows and §3.5's legacy gates are banned too (JUDGEMENT, see `the_*`).
const BANNED: &[&str] = &[
    ".get_unit_keys(",
    ".get_fmts_indexes(",
    ".resolve_unit_keys(",
    "decrypt_with(",
    "AacsKeyMap::from_ranges",
    "with_key_map(",
    "set_key_map(",
    "decrypt_unit(",
    "DecryptingSectorSource::new(",
    "DiscStream::new(",
    "KeyFetch",
    "key_fetch(",
    "freemkv-uk",
    "freemkv-vid:",
    "set_vid(",
    "resolve_keys_for(",
    "inject_unit_keys(",
    ".resolve_keys(",
    "open_scan_resolve",
    "resolve_disc_keys(",
    ".decrypt_keys()",
    ".ensure_decryptable(",
    ".ensure_title_decryptable(",
];

/// `src` with comments blanked, and with string and char literal contents blanked too
/// when `strings` is set. Byte offsets and newlines are kept, so the views line up.
fn blank(src: &str, strings: bool) -> Vec<u8> {
    let b = src.as_bytes();
    let mut out = b.to_vec();
    let mut i = 0;
    let wipe = |out: &mut Vec<u8>, from: usize, to: usize| {
        for c in &mut out[from..to] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    while i < b.len() {
        if b[i..].starts_with(b"//") {
            let end = src[i..].find('\n').map_or(b.len(), |n| i + n);
            wipe(&mut out, i, end);
            i = end;
        } else if b[i..].starts_with(b"/*") {
            let end = src[i + 2..].find("*/").map_or(b.len(), |n| i + n + 4);
            wipe(&mut out, i, end);
            i = end;
        } else if b[i] == b'"' || (b[i] == b'r' && matches!(b.get(i + 1), Some(b'"' | b'#'))) {
            let start = i;
            let hashes = if b[i] == b'r' {
                let h = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                i += 1 + h;
                Some(h)
            } else {
                None
            };
            i += 1;
            match hashes {
                Some(h) => {
                    let close = format!("\"{}", "#".repeat(h));
                    i = src[i..]
                        .find(&close)
                        .map_or(b.len(), |n| i + n + close.len());
                }
                None => {
                    while i < b.len() && b[i] != b'"' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                    i += 1;
                }
            }
            if strings {
                wipe(&mut out, start + 1, i.min(b.len()).saturating_sub(1));
            }
        } else if let Some(n) = char_literal_len(&b[i..]) {
            if strings {
                wipe(&mut out, i + 1, i + n - 1);
            }
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

/// The byte length of a char literal (`'x'`, `'\n'`, `'\''`) at the start of `b`; `None`
/// for anything else, such as a lifetime `'a`.
fn char_literal_len(b: &[u8]) -> Option<usize> {
    if b.first() != Some(&b'\'') {
        return None;
    }
    if b.get(1) == Some(&b'\\') {
        return b.get(3..)?.iter().position(|&c| c == b'\'').map(|p| p + 4);
    }
    let text = std::str::from_utf8(&b[1..b.len().min(5)])
        .unwrap_or_else(|e| std::str::from_utf8(&b[1..1 + e.valid_up_to()]).unwrap_or(""));
    let c = text.chars().next()?;
    (b.get(1 + c.len_utf8()) == Some(&b'\'')).then_some(2 + c.len_utf8())
}

/// Byte ranges of `#[cfg(test)] mod name { .. }` blocks in `code` (comments and strings
/// already blanked).
fn test_modules(code: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (at, _) in code.match_indices("#[cfg(test)]") {
        let rest = &code[at..];
        let Some(open) = rest.find('{') else { continue };
        let head = &rest[..open];
        if !head.split_whitespace().any(|w| w == "mod") || head.contains(';') {
            continue;
        }
        let mut depth = 0usize;
        for (k, c) in rest[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        out.push((at, at + open + k + 1));
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// `(line, banned)` for every banned use in `src`. Comments never count; string literals
/// count only outside test code (`is_test`, or a `#[cfg(test)]` module), where a test may
/// name a banned token to assert its absence (EK9's D9 rule, with no production allow-path).
fn banned_uses(src: &str, is_test: bool) -> Vec<(usize, &'static str)> {
    let mut view = blank(src, false);
    let bare = blank(src, true);
    let bare_text = String::from_utf8_lossy(&bare);
    let tests = if is_test {
        vec![(0, src.len())]
    } else {
        test_modules(&bare_text)
    };
    for &(s, e) in &tests {
        view[s..e].copy_from_slice(&bare[s..e]);
    }
    let mut hits = Vec::new();
    let helper = imports_test_util_decrypt_unit(&bare_text);
    for word in BANNED {
        for at in 0..view.len() {
            if !view[at..].starts_with(word.as_bytes()) {
                continue;
            }
            let in_test = tests.iter().any(|&(s, e)| (s..e).contains(&at));
            // KU §2.2: `decrypt_unit` "move[s] to `libfreemkv::test_util::decrypt_unit`",
            // the sanctioned test helper (feature `test-util`, never in a release build).
            if *word == "decrypt_unit(" && in_test && helper {
                continue;
            }
            hits.push((
                view[..at].iter().filter(|&&c| c == b'\n').count() + 1,
                *word,
            ));
        }
    }
    hits.sort();
    hits
}

/// Whether `code` brings in `libfreemkv::test_util::decrypt_unit` (by path or a `use`).
fn imports_test_util_decrypt_unit(code: &str) -> bool {
    code.contains("test_util::decrypt_unit")
        || code.match_indices("test_util::{").any(|(at, _)| {
            let group = &code[at..];
            let group = &group[..group.find('}').unwrap_or(group.len())];
            group
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|w| w == "decrypt_unit")
        })
}

/// FK8 (KU §2.2): no legacy key API anywhere in freemkv's `src/` or `tests/`, with no
/// allow-path: every rip reads through its up-front `ResolvedKeySet`.
#[test]
fn no_legacy_key_api_anywhere_in_freemkv() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    rs_files(&root.join("tests"), &mut files);
    let mut hits = Vec::new();
    for f in &files {
        let rel = f.strip_prefix(root).unwrap();
        let src = std::fs::read_to_string(f).expect("read source");
        for (line, word) in banned_uses(&src, rel.starts_with("tests")) {
            hits.push(format!("{}:{line}: {word}", rel.display()));
        }
    }
    assert!(
        hits.is_empty(),
        "legacy key APIs (use the rip's ResolvedKeySet, KU §2.2):\n{}",
        hits.join("\n")
    );
}

/// Self-test: code, a string outside tests, and a test's own code are caught; comments,
/// doc comments and a test's string literals are not.
#[test]
fn the_structural_guard_skips_comments_and_test_strings() {
    let fetch = ["Key", "Fetch"].concat();
    let caught = [
        format!("fn f(x: Option<libfreemkv::sector::{fetch}>) {{}}"),
        format!(
            "fn f() {{ let s = \"# {}: 00\"; }}",
            ["freemkv", "-uk"].concat()
        ),
        format!("#[cfg(test)]\nmod t {{\n    fn g() {{ let _ = {fetch}::unit_only; }}\n}}"),
        format!("fn f() {{ disc{}; }}", [".decrypt", "_keys()"].concat()),
    ];
    for s in &caught {
        assert_eq!(banned_uses(s, false).len(), 1, "missed: {s}");
    }
    let skipped = [
        format!("// the old {fetch} is gone\nfn f() {{}}"),
        format!("/// [`{fetch}`] was removed\nfn f() {{}}"),
        format!("/* {fetch} */ fn f() {{}}"),
        format!("#[cfg(test)]\nmod t {{\n    const B: &str = \"{fetch}\";\n}}"),
        format!("fn f<'a>(x: &'a str) -> char {{ '\"' }} // {fetch}"),
    ];
    for s in &skipped {
        assert!(banned_uses(s, false).is_empty(), "matched: {s}");
    }
    assert!(banned_uses(&format!("const B: &str = \"{fetch}\";"), true).is_empty());
    let unit = ["decrypt", "_unit"].concat();
    let helper =
        format!("use libfreemkv::test_util::{{BdFile, {unit}}};\nfn t() {{ {unit}(&mut u, &k); }}");
    assert!(
        banned_uses(&helper, true).is_empty(),
        "the test_util helper"
    );
    let door = format!("use libfreemkv::aacs::content::{unit};\nfn t() {{ {unit}(&mut u, &k); }}");
    assert_eq!(
        banned_uses(&door, true).len(),
        1,
        "the library door, even in a test"
    );
    assert_eq!(
        banned_uses(&helper, false).len(),
        1,
        "never in production code"
    );
}
