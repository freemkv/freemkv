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

/// Files declared as `#[cfg(test)] #[path = "x.rs"] mod name;` side files (relative to the
/// declaring file): test code, blanked like the inline `#[cfg(test)]` modules they replaced.
fn test_side_files(files: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(f).unwrap();
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        for w in lines.windows(3) {
            let path = w[1]
                .strip_prefix("#[path = \"")
                .and_then(|p| p.strip_suffix("\"]"));
            if let (true, Some(p), true) = (w[0] == "#[cfg(test)]", path, w[2].starts_with("mod "))
            {
                out.push(f.parent().unwrap().join(p));
            }
        }
    }
    out
}

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

// `code` with every `#[cfg(test)]` module blanked (any visibility, any further attributes,
// wherever it sits in the file), so only production code is left. `code` is `code_only`
// output, so braces inside strings and comments cannot unbalance the match.
fn without_test_modules(code: &str) -> String {
    let mut out = code.to_string();
    let mut from = 0;
    while let Some(at) = out[from..].find("#[cfg(test)]").map(|i| i + from) {
        let mut rest = out[at + "#[cfg(test)]".len()..].trim_start();
        while rest.starts_with("#[") {
            rest = rest[rest.find(']').map_or(rest.len(), |i| i + 1)..].trim_start();
        }
        let head = rest
            .strip_prefix("pub")
            .map(|r| match r.trim_start().strip_prefix('(') {
                Some(v) => v.split_once(')').map_or("", |(_, t)| t),
                None => r,
            })
            .unwrap_or(rest)
            .trim_start();
        from = at + 1;
        if !head.starts_with("mod ") {
            continue;
        }
        let start = out.len() - rest.len();
        let body = &out[start..];
        let end = match (body.find('{'), body.find(';')) {
            (Some(o), Some(sc)) if sc < o => sc + 1,
            (Some(o), _) => {
                let mut depth = 0usize;
                let mut end = body.len();
                for (i, c) in body[o..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = o + i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                end
            }
            (None, Some(sc)) => sc + 1,
            (None, None) => body.len(),
        };
        out.replace_range(at..start + end, &" ".repeat(start + end - at));
    }
    out
}

// Engine-owned entry points a front end must not import by name (a bare call would dodge the
// qualified `FORBIDDEN` patterns), and the only alias the engine crate may be imported under.
const IMPORTED_FNS: &[&str] = &[
    "mux_with_keys",
    "mux_url",
    "input",
    "write_image",
    "copy",
    "sweep",
    "patch",
];
const ENGINE_ALIAS: &str = "fe";

// Problems in the `use libfreemkv…;` / `use freemkv_engine…;` statements of `code`: an engine-owned
// function imported by name, or either crate renamed to something other than `ENGINE_ALIAS`.
fn bad_imports(code: &str) -> Vec<String> {
    let mut bad = Vec::new();
    for stmt in code.split("use ").skip(1) {
        let Some(stmt) = stmt.split(';').next() else {
            continue;
        };
        let stmt = stmt.trim();
        let Some(path) = ["libfreemkv", "freemkv_engine"]
            .iter()
            .find_map(|c| stmt.strip_prefix(c).map(|r| (*c, r)))
        else {
            continue;
        };
        if let Some(alias) = path.1.trim().strip_prefix("as ")
            && alias.trim() != ENGINE_ALIAS
        {
            bad.push(format!("`{}` renamed to `{}`", path.0, alias.trim()));
        }
        for tok in path
            .1
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|t| IMPORTED_FNS.contains(t))
        {
            bad.push(format!("`{tok}` imported by name from `{}`", path.0));
        }
    }
    bad
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
fn a_test_module_anywhere_hides_only_itself() {
    let src = "fn a() { Drive::open(1); }\n#[cfg(test)]\npub(crate) mod t { fn x() { Drive::open(2); \
               let _ = \"}\"; } }\n#[cfg(test)]\n#[allow(dead_code)]\nmod u { fn y() { Drive::open(3); } }\n\
               fn b() { Drive::open(4); }\n#[cfg(test)]\nmod v;\nfn c() { Drive::open(5); }";
    let code = without_test_modules(&code_only(src));
    assert_eq!(code.matches("Drive::open(").count(), 3, "{code}");
    for kept in ["open(1)", "open(4)", "open(5)"] {
        assert!(code.contains(kept), "{kept} was blanked: {code}");
    }
}

#[test]
fn a_bare_or_renamed_engine_import_is_caught() {
    for bad in [
        "use libfreemkv::mux_url;",
        "use libfreemkv::{input, Disc};",
        "use freemkv_engine::{Plan, copy};",
        "use libfreemkv as lf;",
        "use freemkv_engine as eng;",
    ] {
        assert!(!bad_imports(bad).is_empty(), "{bad} was not flagged");
    }
    for ok in [
        "use freemkv_engine as fe;",
        "use libfreemkv::{Disc, Error, Halt};",
        "use libfreemkv::keys::{KeyRing, KeyScope};",
        "use freemkv_engine::{Event as E, Plan};",
    ] {
        assert!(bad_imports(ok).is_empty(), "{ok} was flagged");
    }
}

#[test]
fn front_ends_reach_engine_owned_work_only_through_the_engine() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rs_files(&root.join("src"), &mut files);
    let side_tests = test_side_files(&files);
    let mut found: BTreeMap<(String, &str), usize> = BTreeMap::new();
    let mut problems = Vec::new();
    for f in &files {
        if side_tests.contains(f) {
            continue;
        }
        let rel = f
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if TEST_ONLY.contains(&rel.as_str()) {
            continue;
        }
        let src = std::fs::read_to_string(f).unwrap().replace("\r\n", "\n");
        let code = without_test_modules(&code_only(&src));
        for bad in bad_imports(&code) {
            problems.push(format!(
                "{rel}: {bad} — go through freemkv_engine::run(Plan)"
            ));
        }
        for pat in FORBIDDEN {
            let n = code.matches(pat).count();
            if n > 0 {
                found.insert((rel.clone(), pat), n);
            }
        }
    }
    let baseline: BTreeMap<(String, &str), usize> = BASELINE
        .iter()
        .map(|&(f, p, n)| ((f.to_string(), p), n))
        .collect();
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
