//! Anti-drift §4 (user decision 2026-09-30): front-end code (the CLI, the desktop app, the
//! server) may not call libfreemkv or the engine's lower layers directly for engine-owned
//! work — opening a drive, acquiring keys, copying, recovering, muxing, extracting. That work
//! goes through `freemkv_engine::run(Plan)`.
//!
//! Calls are matched in code only: a comment or a string (a log line naming `Drive::open`)
//! is not a call. `BASELINE` is empty — every front end reaches this work through the engine
//! — and may only stay so: a new direct call fails here.

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

/// Front-end files allowed a direct call, and how many. Empty, and may only stay so.
const BASELINE: &[(&str, &str, usize)] = &[];

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

// `src` with every comment and string literal blanked, so only code is matched.
fn code_only(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        let rest = &src[i..];
        if rest.starts_with("//") {
            i += rest.find('\n').unwrap_or(rest.len());
        } else if rest.starts_with("/*") {
            i += rest.find("*/").map_or(rest.len(), |e| e + 2);
        } else if let Some(n) = raw_string_len(rest) {
            i += n;
            out.push(' ');
        } else if b[i] == b'"' {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            i = j + 1;
            out.push(' ');
        } else if let Some(n) = char_literal_len(rest) {
            i += n;
            out.push(' ');
        } else {
            let c = rest.chars().next().expect("in bounds");
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

// The byte length of a raw string literal (`r"…"`, `r#"…"#`) starting `s`, if one does.
fn raw_string_len(s: &str) -> Option<usize> {
    let after_r = s.strip_prefix('r')?;
    let hashes = after_r.len() - after_r.trim_start_matches('#').len();
    let body = after_r[hashes..].strip_prefix('"')?;
    let close = format!("\"{}", "#".repeat(hashes));
    Some(1 + hashes + 1 + body.find(&close)? + close.len())
}

// The byte length of a char literal (`'x'`, `'\n'`, `'"'`) starting `s`; a lifetime is not one.
fn char_literal_len(s: &str) -> Option<usize> {
    let body = s.strip_prefix('\'')?;
    let mut chars = body.char_indices();
    let (_, c) = chars.next()?;
    let end = if c == '\\' {
        body[1..].find('\'')? + 1
    } else {
        let (at, next) = chars.next()?;
        if next != '\'' {
            return None;
        }
        at
    };
    Some(1 + end + 1)
}

#[test]
fn code_only_blanks_comments_and_strings() {
    let src = "a(); // Drive::open(\nlet s = \"Drive::open(\"; let c = '\"'; b('x');\n\
               /* DiscSession::open */ let r = r#\"Drive::open(\"#; fn f<'a>() {}";
    let code = code_only(src);
    assert!(!code.contains("Drive::open("), "{code}");
    assert!(!code.contains("DiscSession::open"), "{code}");
    assert!(code.contains("a();") && code.contains("b( );") && code.contains("fn f<'a>()"));
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
            let n = code_only(production(&src)).matches(pat).count();
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
