use super::resume_effective_abort;

// Catches the mutation that recomputes the abort threshold from raw config at one loss gate
// while the other honours.accept-loss — the two-gates-one-run disagreement.
#[test]
fn the_accept_loss_override_raises_the_threshold_for_every_resume_gate() {
    assert_eq!(
        resume_effective_abort(true, "mkv", 5),
        u64::MAX,
        "with the override armed no loss gate may abort"
    );
    assert_eq!(
        resume_effective_abort(true, "iso", 0),
        u64::MAX,
        "the override outranks even the ISO byte-complete rule — the \
             operator is looking at the recorded damage when they press it"
    );
    assert_eq!(
        resume_effective_abort(false, "mkv", 5),
        super::super::effective_abort_secs("mkv", 5),
        "without the override the threshold is exactly the configured one"
    );
    assert_eq!(
        resume_effective_abort(false, "iso", 30),
        0,
        "without the override ISO still forces byte-complete"
    );
}

// Catches a re-introduced SECOND, hand-rolled threshold computation:
// every loss gate must route through resume_effective_abort, so
// effective_abort_secs may be named exactly once (comments stripped).
#[test]
fn resume_has_exactly_one_threshold_computation() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    // Production code only: the test modules below name these same
    // functions, and counting them would make this pin count itself.
    let src = &src[..src.find("#[cfg(test)]").expect("this file has tests")];
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| !l.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n");
    let direct = code.matches("super::effective_abort_secs(").count();
    assert_eq!(
        direct, 1,
        "the resume path must compute its abort threshold in ONE place \
             (resume_effective_abort); found {direct} direct calls to \
             effective_abort_secs, which is how the sweep gate and the mux gate \
             came to disagree about `.accept-loss` in the same run"
    );
}

// `.accept-loss` is READ at entry but CLEARED only at a hand-off. Each
// delivery path (raw-ISO branch, MKV mux) consumes it, so each clear must
// follow ITS OWN path's completion-marker write, with nothing risky between.
#[test]
fn the_accept_loss_marker_is_consumed_only_once_the_rip_is_delivered() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    // Production code only: the test modules below name these same
    // functions, and counting them would make this pin count itself.
    let src = &src[..src.find("#[cfg(test)]").expect("this file has tests")];
    let code: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| !l.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n");
    let read_at = code
        .find("staging::accept_loss_requested(")
        .expect("resume_remux must read the marker");
    let cleared: Vec<usize> = code
        .match_indices("staging::clear_accept_loss_marker(")
        .map(|(i, _)| i)
        .collect();
    assert!(
        !cleared.is_empty(),
        "the override must be consumed on delivery, in at least one place"
    );
    let mut delivered_by: Vec<usize> = Vec::new();
    for &c in &cleared {
        let w = code[..c]
            .rfind("staging::write_completed_marker(")
            .expect("every `.accept-loss` clear must follow a completion-marker write");
        assert!(w > read_at, "a clear's delivery must come after the read");
        let between = &code[w..c];
        for risky in ["mux_iso(", "mark_handoff(", "return"] {
            assert!(
                !between.contains(risky),
                "`.accept-loss` must be cleared right after its own path's \
                     write_completed_marker, but `{risky}` sits between them — a \
                     transient failure there would spend the operator's consent"
            );
        }
        assert!(
            !delivered_by.contains(&w),
            "two `.accept-loss` clears share one completion-marker write: one \
                 of them has moved ahead of its own path's delivery"
        );
        delivered_by.push(w);
    }
}
