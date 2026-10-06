use super::*;

fn state_of(device: &str) -> crate::server::ripper::RipState {
    super::super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned()
        .expect("the terminal write must have created a STATE entry")
}

/// Grade a terminal state exactly as `remux_from_ripped_marker`'s
/// non-success branch does, and hand back the outcome the muxer reads.
fn graded(device: &str) -> MuxHandoffOutcome {
    let rs = state_of(device);
    let mut outcome = build_mux_handoff_outcome(false);
    apply_failure_fields(&mut outcome, &rs);
    outcome
}

#[test]
fn a_hard_failure_that_lands_on_idle_is_not_reported_as_retryable() {
    // Unique per test: STATE is process-global.
    let dev = format!("_retryable-hard-{}", std::process::id());
    // Verbatim shape of resume_remux's unreadable-mapfile exit.
    reset_status_after_ripping(
        &dev,
        "idle",
        "Some Disc",
        "bluray",
        "1:52",
        Some("Could not read this disc's saved recovery map".to_string()),
    );
    assert_eq!(
        state_of(&dev).status,
        "idle",
        "the fixture must reproduce the trap"
    );

    let outcome = graded(&dev);

    assert_eq!(
        outcome.failure_reason.as_deref(),
        Some("Could not read this disc's saved recovery map"),
        "the operator must be told the real reason the dir didn't advance"
    );
    assert!(
        !outcome.failure_retryable,
        "a hard failure must not be graded retryable just because its \
             terminal status is \"idle\" — that puts \"will mux automatically \
             once keys are available\" on a corrupt ISO's error card"
    );
}

#[test]
fn a_keyless_deferral_is_reported_as_retryable() {
    let dev = format!("_retryable-deferral-{}", std::process::id());
    defer_status_after_ripping(
        &dev,
        "Some Disc",
        "bluray",
        "1:52",
        "Ripped to ISO — no keys, mux deferred.".to_string(),
    );
    assert_eq!(
        state_of(&dev).status,
        "idle",
        "a deferral still reads as idle"
    );

    let outcome = graded(&dev);

    assert!(
        outcome.failure_retryable,
        "the keyless deferral is the case that IS retryable — the ISO \
             stays staged and muxes itself once keys land"
    );
}

// A non-success with NO recorded error records nothing at all, so
// crate::server::muxer's Some("")-vs-None dispatch falls through to its own
// .aborted-loss / failed-reason fallbacks instead of a blank card.
#[test]
fn a_non_success_without_a_recorded_error_reports_no_reason() {
    let dev = format!("_retryable-silent-{}", std::process::id());
    reset_status_after_ripping(&dev, "idle", "Some Disc", "bluray", "1:52", None);
    assert!(
        state_of(&dev).last_error.is_empty(),
        "the fixture must leave last_error empty"
    );

    let outcome = graded(&dev);

    assert!(
        outcome.failure_reason.is_none(),
        "an empty last_error must leave failure_reason None so the muxer \
             uses its own fallback hints"
    );
    assert!(!outcome.failure_retryable);
}

// Pins remux_from_ripped_marker's source-level wiring to this grading (can't drive it
// directly without a full mux pipeline) — same technique used for resume_remux's webhook
// call sites.
#[test]
fn the_non_success_branch_routes_through_apply_failure_fields() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    let start = src
        .find("pub(crate) fn remux_from_ripped_marker(")
        .expect("remux_from_ripped_marker must exist");
    let region = &src[start..];
    let region = &region[..region.find("\n}\n").expect("the function ends")];
    assert!(
        region.contains("apply_failure_fields(&mut outcome, rs);"),
        "remux_from_ripped_marker's non-success branch must grade through \
             apply_failure_fields, not a hand-rolled status inference"
    );
    assert!(
        !region.contains("rs.status == \"idle\""),
        "remux_from_ripped_marker must never infer retryability from the \
             terminal status: three hard-failure exits write \"idle\" too"
    );
}
