use super::{AutoResumeAction, auto_insert_rip_mode, auto_resume_action, insert_tick};

#[test]
fn auto_resume_uses_partial_state_and_falls_back_to_fresh() {
    assert_eq!(
        auto_resume_action(Some(super::Resumable::Sweep)),
        AutoResumeAction::Sweep
    );
    assert_eq!(
        auto_resume_action(Some(super::Resumable::Remux)),
        AutoResumeAction::Remux
    );
    assert_eq!(auto_resume_action(None), AutoResumeAction::Fresh);
}

// on_insert=rip must never map to the operator's unguarded Wipe (1.7.6 regression).
#[test]
fn insert_rip_modes_distinguish_fresh_from_prefer_resume() {
    assert_eq!(
        auto_insert_rip_mode("rip"),
        Some(crate::server::web::ResumeMode::Fresh)
    );
    assert_eq!(
        auto_insert_rip_mode("resume"),
        Some(crate::server::web::ResumeMode::Prefer)
    );
    assert_eq!(auto_insert_rip_mode("scan"), None);
}

// A disc seen during the 5s post-Stop cooldown must still be ripped once it expires —
// latching it early retires the only auto-rip trigger the loop has.
#[test]
fn a_disc_seen_during_the_stop_cooldown_is_still_ripped_once_it_expires() {
    // Tick 1 — new disc, device still cooling down after a Stop.
    let t1 = insert_tick(true, true);
    assert!(!t1.dispatch, "the cooldown must suppress the trigger");
    assert!(
        !t1.latch,
        "a disc this tick did NOT act on must not be recorded as seen — \
             latching it retires the only auto-rip trigger the loop has"
    );

    // Tick 2 — cooldown expired. Tick 1 did not latch, so the device is
    // still absent from had_disc and this is still a new insert.
    let t2 = insert_tick(true, false);
    assert!(
        t2.dispatch,
        "once the cooldown expires the disc must be ripped"
    );
    assert!(t2.latch, "a dispatched disc is now genuinely handled");
}

/// The regression this fix could plausibly cause: a disc that simply sits
/// in the drive must not re-trigger a rip on every tick.
#[test]
fn a_resident_disc_does_not_retrigger() {
    let t = insert_tick(false, false);
    assert!(!t.dispatch, "an already-handled disc must not re-trigger");
    assert!(
        t.latch,
        "and it stays latched so it keeps not re-triggering"
    );
}
