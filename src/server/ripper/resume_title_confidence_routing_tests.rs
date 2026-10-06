use super::super::handoff_marker_name;
use super::resume_title_confident;

/// No TMDB key configured → always confident, regardless of carried
/// state or match, matching `title_is_confident`'s "operators running
/// keyless expect the disc-label filename" rule.
#[test]
fn no_api_key_is_always_confident() {
    assert!(resume_title_confident(
        "",
        None,
        "BD_ROM_R1",
        "Casablanca",
        1942
    ));
    assert!(resume_title_confident(
        "   ",
        Some(false),
        "BD_ROM_R1",
        "Casablanca",
        1942
    ));
}

// carried_confident = Some(true) must be OR'd in even when the
// current match check alone says no: an operator's deliberate pick
// must not be second-guessed back into .review on resume.
#[test]
fn carried_confident_true_overrides_a_weak_match() {
    assert!(resume_title_confident(
        "tmdb-key",
        Some(true),
        "BD_ROM_R1",
        "Some Guessed Title",
        0,
    ));
}

// carried_confident = None (cold auto-resume) must fall through to
// the plain match check, not be treated as confident by default.
#[test]
fn no_carried_state_falls_through_to_the_match_check() {
    assert!(
        !resume_title_confident("tmdb-key", None, "BD_ROM_R1", "Casablanca", 1942),
        "a disc-label title with no year match must not be confident"
    );
    assert!(resume_title_confident(
        "tmdb-key",
        None,
        "THE_MATRIX",
        "The Matrix",
        1999,
    ));
}

/// The marker-name choice itself: confident → `.done`, not → `.review`.
#[test]
fn marker_name_follows_confidence() {
    assert_eq!(handoff_marker_name(true), ".done");
    assert_eq!(handoff_marker_name(false), ".review");
}

// resume_remux's real call site can't be driven end-to-end (needs a
// real Disc::scan_image), so pin the exact argument order at the
// source level as a stopgap against a silent argument swap.
#[test]
fn resume_remux_calls_resume_title_confident_with_disc_label_then_match_title() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    assert!(
            src.contains(
                "resume_title_confident(\n        &cfg_read.tmdb_api_key,\n        carried_confident,\n        &disc_label,\n        &title_for_match,\n        tmdb_year,\n    )"
            ),
            "resume_remux must call resume_title_confident(tmdb_api_key, carried_confident, \
             disc_label, title_for_match, tmdb_year) in that exact argument order"
        );
}

// The cold-auto-resume ISO completion branch is a separate call site
// from the MKV one, so it needs its own pin: it must route through
// handoff_marker_name, not keep its own .done/.review ternary.
#[test]
fn iso_completion_uses_handoff_marker_name() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    let start = src
        .find("ISO output: deliver the whole-disc image")
        .expect("resume.rs should have the ISO completion branch");
    // Bound at the MKV-path's own gate note (pinned separately by
    // `completed_mux_with_loss_gated_by_abort_on_lost_secs`), NOT at the
    // call instead of the ISO site's.
    let end = src[start..]
        .find("A loss is a loss. Mux-time")
        .map(|i| start + i)
        .expect("resume.rs should have the mux-time-loss note after the ISO branch");
    let region = &src[start..end];
    assert!(
        region.contains("mark_handoff("),
        "the ISO completion branch must hand off via staging::mark_handoff, not a duplicated ternary"
    );
}
