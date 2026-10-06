// Production source of this file: every `#[cfg(test)] mod` block removed
// (brace-matched), so the pins below see only non-test code.
fn production_src() -> String {
    let src = crate::server::util::source_lf(include_str!("mod.rs"));
    let mut out = String::new();
    let mut rest: &str = &src;
    while let Some(i) = rest.find("#[cfg(test)]\nmod ") {
        out.push_str(&rest[..i]);
        let open = i + rest[i..].find('{').expect("mod body");
        let mut depth = 0usize;
        let mut end = rest.len();
        for (j, c) in rest[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + j + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

// Every rip-side quarantine must go through `quarantine_or_log` so a refused
// or failed `.failed` write is surfaced in the device log, and the live-disc
// sweep seed must be the one that may replace an unreadable state.json.
#[test]
fn rip_side_quarantines_log_when_not_persisted() {
    let prod = production_src();
    let bare = prod.matches("staging::write_failed_marker(").count();
    assert_eq!(
        bare, 1,
        "only quarantine_or_log may call write_failed_marker; found {bare}"
    );
    assert_eq!(
        prod.matches("quarantine_or_log(").count(),
        7,
        "6 call sites + the fn"
    );
    assert!(prod.contains("staging::seed_sweeping_for_live_rip("));
    assert!(!prod.contains("staging::write_sweeping_marker("));
}
