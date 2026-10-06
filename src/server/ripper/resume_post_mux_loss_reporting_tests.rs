// Regression: a resume must report sweep loss + demux loss to the operator, not the sweep
// mapfile alone (previously demux-time loss was invisible). Pinned at source level.
#[test]
fn resume_reports_demux_loss_on_accepted_rip() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    // Bound to the accepted-success region: from the combined-loss
    // (sweep + demux) computation up to the auto-eject tail.
    let start = src
        .find("Operator-facing loss for a resume")
        .expect("resume.rs should compute combined resume loss");
    let end = src[start..]
        .find("Honor auto_eject after a successful resume")
        .map(|i| start + i)
        .expect("resume.rs should have the auto_eject tail after success");
    let region = &src[start..end];

    // The combined figures must be derived from BOTH sweep damage and the
    // demux loss signal.
    assert!(
        region.contains("done_sweep_damage.errors.saturating_add(mux_outcome.errors)"),
        "accepted resume must add demux errors to sweep errors"
    );
    assert!(
        region.contains(
            "done_sweep_damage.main_lost_ms / crate::server::util::MILLIS_PER_SEC + demux_lost_secs"
        ),
        "accepted resume must add demux lost seconds to sweep main loss"
    );
    // Both the done card and the webhook must consume the combined figures,
    // not the sweep-only fields.
    assert!(
        region.contains("errors: done_errors"),
        "done card / webhook must report combined errors (done_errors)"
    );
    assert!(
        region.contains("lost_video_secs: done_lost_video_secs"),
        "done card / webhook must report combined loss (done_lost_video_secs)"
    );
    // Guard against regressing to the sweep-only webhook figures.
    assert!(
        !region.contains(
            "lost_video_secs: done_sweep_damage.main_lost_ms / crate::server::util::MILLIS_PER_SEC"
        ),
        "webhook must not report sweep-only loss, hiding demux loss"
    );
}

// Regression: mux-incomplete must route through incomplete_mux_status
// (mirroring rip_disc) so a mid-mux error surfaces instead of a
// silent idle/None verdict indistinguishable from /api/stop.
#[test]
fn resume_incomplete_mux_surfaces_read_error_not_silent_idle() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    // Bound to the mux-incomplete early-return: from the guard up to the
    // "A loss is a loss" mux-time-loss note that immediately follows it. The
    // new anchor brackets just the ~35-line guard block.
    let start = src
        .find("if !mux_outcome.output_opened || !mux_outcome.completed {")
        .expect("resume.rs should have the mux-incomplete guard");
    let end = src[start..]
        .find("A loss is a loss. Mux-time")
        .map(|i| start + i)
        .expect("resume.rs should have the mux-time-loss note after the guard");
    let region = &src[start..end];

    assert!(
        region.contains("super::incomplete_mux_status("),
        "resume mux-incomplete branch must route through incomplete_mux_status, \
             matching rip_disc, so finalize/read-error causes surface"
    );
    assert!(
        region.contains("mux_outcome.read_error.as_deref()"),
        "resume mux-incomplete branch must pass mux_outcome.read_error so a \
             mid-mux drive/ISO read error surfaces as status=error with the cause"
    );
    assert!(
        region.contains("mux_outcome.finalize_error.as_deref()"),
        "resume mux-incomplete branch must pass mux_outcome.finalize_error so a \
             structural mux finalize failure surfaces as status=failed"
    );
    // Guard against regressing to the silent idle/None verdict that
    // discarded the cause and looked like a clean /api/stop.
    assert!(
        !region.contains(
            "reset_status_after_ripping(device, \"idle\", &display_name, \
                 &disc_format, &duration, None)"
        ),
        "resume mux-incomplete branch must not hardcode idle/None, hiding the \
             read-error cause and aliasing a failure to a clean stop"
    );
}

// FIX 1 — behavioural coverage the source-substring test above lacks:
// drives quarantine_incomplete_mux's two real arms (finalize_error ->
// terminal Failed/SkipTerminal; read_error -> stays resumable/Dispatch).
#[test]
fn incomplete_mux_finalize_quarantines_read_error_stays_resumable() {
    use crate::server::muxer::{MuxVerdict, mux_dispatch_verdict};
    use crate::server::ripper::staging::{self, DiscState, StagingState, snapshot_staging_disc};

    let tmp = tempfile::TempDir::new().unwrap();

    // Arm 1: a structural finalize failure quarantines to terminal.
    let finalize_dir = tmp.path().join("Finalize_Fail");
    std::fs::create_dir_all(&finalize_dir).unwrap();
    staging::write_state(&finalize_dir, &DiscState::new(StagingState::Ripped));
    assert_eq!(
        mux_dispatch_verdict(snapshot_staging_disc(&finalize_dir).as_ref()),
        MuxVerdict::Dispatch,
        "a fresh Ripped hand-off must dispatch before the quarantine"
    );
    let quarantined =
        super::quarantine_incomplete_mux(&finalize_dir, Some("mux produced no frames (E6008)"));
    assert!(
        quarantined,
        "a finalize_error must report a terminal quarantine"
    );
    assert_eq!(
        snapshot_staging_disc(&finalize_dir)
            .and_then(|_| staging::read_state(&finalize_dir))
            .map(|s| s.state),
        Some(StagingState::Failed),
        "a finalize_error must transition state → Failed"
    );
    assert_eq!(
        mux_dispatch_verdict(snapshot_staging_disc(&finalize_dir).as_ref()),
        MuxVerdict::SkipTerminal,
        "after the finalize quarantine the dir must never re-dispatch"
    );

    // Arm 2: a mid-mux read error (finalize_error == None) stays resumable.
    let read_dir = tmp.path().join("Read_Error");
    std::fs::create_dir_all(&read_dir).unwrap();
    staging::write_state(&read_dir, &DiscState::new(StagingState::Ripped));
    let quarantined = super::quarantine_incomplete_mux(&read_dir, None);
    assert!(!quarantined, "a read_error must NOT quarantine");
    assert_eq!(
        staging::read_state(&read_dir).map(|s| s.state),
        Some(StagingState::Ripped),
        "a read_error must leave the dir in the resumable Ripped state"
    );
    assert_eq!(
        mux_dispatch_verdict(snapshot_staging_disc(&read_dir).as_ref()),
        MuxVerdict::Dispatch,
        "a read_error dir must stay re-muxable (Dispatch), not be quarantined"
    );
}

// quarantine_incomplete_mux returns whether the terminal write actually LANDED, not
// merely whether the failure was a finalize — an unwritable mount must return false.
#[test]
fn quarantine_incomplete_mux_returns_false_when_write_dropped() {
    use crate::server::ripper::staging::{self, StagingState};

    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("Unwritable");
    std::fs::create_dir_all(&dir).unwrap();
    // Force the terminal state.json write to fail (a dir can't be renamed over).
    std::fs::create_dir_all(dir.join(staging::STATE_FILE)).unwrap();

    let landed = super::quarantine_incomplete_mux(&dir, Some("mux produced no frames (E6008)"));
    assert!(
        !landed,
        "a finalize failure whose terminal write was DROPPED must return false"
    );
    // And the dir did NOT reach the terminal Failed state on disk.
    assert_ne!(
        staging::read_state(&dir).map(|s| s.state),
        Some(StagingState::Failed),
        "a dropped write must not leave a persisted terminal state"
    );
}

// production wiring end to end: RipState.failure_finalize must thread through
// apply_failure_fields to the worker's terminal gate and a persisted Failed state.
#[test]
fn finalize_finalize_threads_ripstate_to_worker_gate_and_persists_failed() {
    use crate::server::muxer::{MuxFailureClass, mux_failure_is_terminal};
    use crate::server::muxer::{MuxVerdict, mux_dispatch_verdict};
    use crate::server::ripper::staging::{self, DiscState, StagingState, snapshot_staging_disc};

    // 1. A terminal finalize failure as `resume_remux` records it on the `_mux`
    //    RipState: a real error string + the structural-finalize bit.
    let rs = crate::server::ripper::RipState {
        last_error: "mux finalize failed: E6008 no muxable frames".to_string(),
        failure_finalize: true,
        failure_deferred: false,
        ..crate::server::ripper::RipState::default()
    };

    // 2. The handoff builder threads it into the outcome the worker consumes.
    let mut outcome = super::MuxHandoffOutcome::default();
    super::apply_failure_fields(&mut outcome, &rs);
    assert!(
        outcome.failure_finalize,
        "the finalize bit must thread RipState → MuxHandoffOutcome (the reverted FIX)"
    );
    assert_eq!(
        outcome.failure_reason.as_deref(),
        Some("mux finalize failed: E6008 no muxable frames")
    );

    // 3. The worker's terminal gate, fed from that outcome, says TERMINAL.
    assert!(
        mux_failure_is_terminal(MuxFailureClass {
            aborted_loss: false,
            has_worker_reason: outcome.failure_reason.is_some(),
            is_finalize: outcome.failure_finalize,
        }),
        "a threaded finalize failure must drive the worker gate to quarantine"
    );

    // 4. End to end: the transition the gate authorises persists Failed and the
    //    next dispatch verdict is SkipTerminal — the dir never re-muxes.
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("Finalize_E2E");
    std::fs::create_dir_all(&dir).unwrap();
    staging::write_state(&dir, &DiscState::new(StagingState::Ripped));
    assert_eq!(
        mux_dispatch_verdict(snapshot_staging_disc(&dir).as_ref()),
        MuxVerdict::Dispatch,
        "a fresh Ripped hand-off dispatches before the quarantine"
    );
    assert!(crate::server::muxer::persist_terminal_mux_quarantine(
        &dir.to_string_lossy(),
        &dir,
        outcome.failure_reason.as_deref().unwrap(),
    ));
    assert_eq!(
        snapshot_staging_disc(&dir)
            .and_then(|_| staging::read_state(&dir))
            .map(|s| s.state),
        Some(StagingState::Failed),
        "the threaded terminal finalize must persist state → Failed"
    );
    assert_eq!(
        mux_dispatch_verdict(snapshot_staging_disc(&dir).as_ref()),
        MuxVerdict::SkipTerminal,
        "after the quarantine the dir must never re-dispatch"
    );
}

// the sweep-loss abort path must quarantine to a resumable.aborted-loss like the
// mux-time loss gate does (else the worker re-dispatches the doomed dir forever).
#[test]
fn sweep_loss_abort_quarantines_to_resumable_aborted_loss() {
    // (a) Source-level: the §3 sweep-loss abort block quarantines via the marker.
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    let start = src
        .find("\"disc loss\" for raw ISO (whole-disc scope)")
        .expect("resume.rs should have the §3 sweep-loss scope note");
    let end = src[start..]
        .find("4. Build MuxInputs + run mux")
        .map(|i| start + i)
        .expect("resume.rs should have the mux-build step after the §3 gate");
    let region = &src[start..end];
    // Assert on the CALL form (`staging::mark_aborted_on_loss(`), not the bare
    // identifier — a prose mention of the symbol in a nearby comment must not
    // satisfy this (the vacuous-substring trap).
    assert!(
        region.contains("staging::mark_aborted_on_loss_reporting_landed("),
        "the §3 sweep-loss abort must quarantine to a resumable .aborted-loss (mirror §4), \
             else the worker re-dispatches the doomed dir forever"
    );

    // (b) Behavioural: the marker the §3 path now writes flips the worker's
    //     dispatch verdict to SkipAbortedLoss, stopping the re-dispatch loop.
    use crate::server::muxer::{MuxVerdict, mux_dispatch_verdict};
    use crate::server::ripper::staging::{self, DiscState, StagingState, snapshot_staging_disc};
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().join("Sweep_Loss");
    std::fs::create_dir_all(&dir).unwrap();
    staging::write_state(&dir, &DiscState::new(StagingState::Ripped));
    assert_eq!(
        mux_dispatch_verdict(snapshot_staging_disc(&dir).as_ref()),
        MuxVerdict::Dispatch,
        "a fresh Ripped hand-off dispatches before the sweep-loss quarantine"
    );
    let _ = staging::mark_aborted_on_loss(
        &dir,
        "aborted: disc loss 12.50s exceeds threshold 0s (sweep)",
    );
    assert_eq!(
        mux_dispatch_verdict(snapshot_staging_disc(&dir).as_ref()),
        MuxVerdict::SkipAbortedLoss,
        "after the sweep-loss quarantine the dir must not re-dispatch (SkipAbortedLoss)"
    );
}

// v1.2.0 invariant ("a loss is a loss"): a COMPLETED mux carrying mux-time loss is gated on
// abort_on_lost_secs just like read-time loss, reported always, never silently dropped.
#[test]
fn completed_mux_with_loss_gated_by_abort_on_lost_secs() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    // The completed-mux success region: from the "A loss is a loss" mux-time-
    // loss note through the auto-eject tail.
    let start = src
        .find("A loss is a loss. Mux-time")
        .expect("resume.rs should have the mux-time-loss note");
    let end = src[start..]
        .find("Honor auto_eject after a successful resume")
        .map(|i| start + i)
        .expect("resume.rs should have the auto_eject tail after success");
    let region = &src[start..end];

    // (a) The loss is still REPORTED, never silently dropped.
    assert!(
        region.contains("demux_lost_secs"),
        "completed-mux success region must report demux-time loss"
    );
    // (b) Within threshold it hands off to Done/Review, routed through the
    // shared `handoff_label` title-confidence policy (staging) so this
    // completion route can't drift from the fresh-rip one.
    assert!(
        region.contains("handoff_label(title_confident)"),
        "completed mux must hand off to .done (confident) or .review (not)"
    );
    assert!(
        region.contains("mark_handoff("),
        "completed mux must write the unified hand-off state so the mover/operator picks it up"
    );
    // (c) A loss is a loss: mux-time loss OVER abort_on_lost_secs quarantines
    //     to a RESUMABLE .aborted-loss, gated on the threshold.
    assert!(
        region.contains("mark_aborted_on_loss"),
        "mux-time loss over threshold must quarantine to a resumable .aborted-loss"
    );
    assert!(
        region.contains("abort_on_lost_secs") || region.contains("effective_abort"),
        "the mux-time loss gate must consult the abort_on_lost_secs threshold"
    );
}
