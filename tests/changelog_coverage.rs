//! Anti-drift §5 (user decision 2026-09-30): every user-facing line in the unreleased section
//! of `CHANGELOG.md` states which front ends it covers — `(CLI, app, server)` — and a change
//! that reaches only some of them says why: `(CLI only: an exit code)`, `(CLI, app only:
//! packaging)`. A front-end-only change with no stated reason fails here.

const FRONT_ENDS: [&str; 3] = ["CLI", "app", "server"];

// The coverage tag of a bullet: `(A, B, C)` naming all three, or `(A[, B] only: reason)`.
fn coverage(line: &str) -> Option<Result<(), String>> {
    let mut rest = line;
    while let Some(open) = rest.find('(') {
        let inner_start = open + 1;
        let Some(close) = rest[inner_start..].find(')') else {
            break;
        };
        let inner = &rest[inner_start..inner_start + close];
        rest = &rest[inner_start + close + 1..];
        let (names, reason) = match inner.split_once(" only:") {
            Some((n, r)) => (n, Some(r.trim())),
            None => (inner, None),
        };
        let names: Vec<&str> = names.split(',').map(str::trim).collect();
        if names.is_empty() || !names.iter().all(|n| FRONT_ENDS.contains(n)) {
            continue;
        }
        return Some(match reason {
            Some("") => Err("a front-end-only change needs its reason after `only:`".into()),
            Some(_) => Ok(()),
            None if FRONT_ENDS.iter().all(|f| names.contains(f)) => Ok(()),
            None => Err(format!(
                "`({inner})` leaves a front end out: name all three, or say why with `only: …`"
            )),
        });
    }
    None
}

// The bullet lines of a section: `- ` or `* `, at any indent.
fn bullets(section: &str) -> Vec<&str> {
    section
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("- ") || t.starts_with("* ")
        })
        .collect()
}

#[test]
fn every_unreleased_line_states_its_front_end_coverage() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/CHANGELOG.md"))
        .unwrap()
        .replace("\r\n", "\n");
    // `## Unreleased`, or the release-ready `## [X.Y.Z] — Unreleased` the release dates.
    let heading = text
        .lines()
        .find(|l| l.starts_with("## ") && l.to_ascii_lowercase().ends_with("unreleased"))
        .expect("an Unreleased section");
    let start = text.find(heading).unwrap();
    let body = &text[start + heading.len()..];
    let end = body.find("\n## ").unwrap_or(body.len());
    let lines = bullets(&body[..end]);
    assert!(!lines.is_empty(), "the Unreleased section has no bullets");
    let mut problems = Vec::new();
    for line in lines {
        match coverage(line) {
            Some(Ok(())) => {}
            Some(Err(why)) => problems.push(format!("{why}: {line}")),
            None => problems.push(format!(
                "no coverage tag — add `(CLI, app, server)` or `(… only: reason)`: {line}"
            )),
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_coverage_tag_reads_as_documented() {
    assert_eq!(coverage("- x (CLI, app, server)"), Some(Ok(())));
    assert_eq!(coverage("- x (CLI only: an exit code)"), Some(Ok(())));
    assert!(matches!(coverage("- x (CLI, app)"), Some(Err(_))));
    assert!(matches!(coverage("- x (CLI only:)"), Some(Err(_))));
    assert_eq!(coverage("- x (see #52) and nothing else"), None);
}

#[test]
fn star_and_indented_bullets_are_checked_too() {
    let section = "- a\n* b\n  - c\n    * d\nplain\n";
    assert_eq!(bullets(section), ["- a", "* b", "  - c", "    * d"]);
}
