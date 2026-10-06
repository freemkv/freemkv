use super::*;
use crate::server::ripper::staging;

fn tmpdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!(
            "autorip-resume-lock-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
    // See the note in the find-iso `tmpdir`: clear stale contents so a
    // reused scratch path (persistent dir + CI pid reuse) starts empty.
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// H1: a cold operator-resume must write .muxing for the duration of
// the mux so a concurrent Wipe / second cold resume is blocked; the
// guard clears the marker on drop.
#[test]
fn cold_resume_guard_writes_and_clears_muxing() {
    let d = tmpdir();
    // The muxing lock is a field set on the existing (Ripped) state, so seed
    // a Ripped state.json for the guard to attach the lock to.
    staging::write_state(&d, &staging::DiscState::new(staging::StagingState::Ripped));
    assert!(!staging::read_state(&d).map(|s| s.muxing).unwrap_or(false));
    {
        let _g = ResumeMuxingGuard::acquire("sg0", &d);
        assert!(
            staging::read_state(&d).expect("state").muxing,
            "cold-resume guard must set the muxing lock while the mux is in flight"
        );
        // While held, the snapshot the ownership/blocked checks consult
        // reports the dir as owned.
        let snap = staging::snapshot_staging_disc(&d).expect("snapshot");
        assert!(snap.has_muxing);
    }
    assert!(
        !staging::read_state(&d).expect("state").muxing,
        "the muxing lock must be cleared on guard drop (covers early-return / panic)"
    );
}

// H1: the _mux worker already holds the lock via its own MuxingGuard,
// so resume_remux's guard must NOT touch the marker (a clear would
// release the worker's exclusion mid-dispatch).
#[test]
fn worker_mux_device_does_not_double_manage_muxing() {
    let d = tmpdir();
    // Simulate the worker having set the muxing lock before dispatch. The
    // lock is a field on the Ripped state, so seed that state first.
    staging::write_state(&d, &staging::DiscState::new(staging::StagingState::Ripped));
    staging::write_muxing_marker(&d);
    assert!(staging::read_state(&d).expect("state").muxing);
    {
        let _g = ResumeMuxingGuard::acquire("_mux", &d);
        assert!(
            staging::read_state(&d).expect("state").muxing,
            "worker's lock stays put"
        );
    }
    assert!(
        staging::read_state(&d).expect("state").muxing,
        "the `_mux` guard must leave the worker's muxing lock intact on drop"
    );
}

/// M4: below `RESTART_LIMIT`, a fsync failure bumps `.restart_count` and
/// preserves staging (no `.failed`) for the next retry.
#[test]
fn fsync_failure_below_limit_preserves_and_bumps() {
    let d = tmpdir();
    // Seed a `.ripped` so we can assert it survives below the limit.
    std::fs::write(d.join(".ripped"), b"{}").unwrap();
    let quarantined = handle_resume_fsync_failure("_mux", &d, "mux output");
    assert!(!quarantined, "first failure must not quarantine");
    assert_eq!(staging::restart_count(&d), 1);
    assert!(!d.join(".failed").exists(), "no .failed below the limit");
    assert!(d.join(".ripped").exists(), ".ripped preserved for retry");
}

/// M4: once `.restart_count` reaches `RESTART_LIMIT`, the repeated fsync
/// failure promotes the dir to terminal `.failed`, drops `.ripped` so the
/// worker can't re-queue it, and clears the counter.
#[test]
fn fsync_failure_at_limit_quarantines() {
    let d = tmpdir();
    std::fs::write(d.join(".ripped"), b"{}").unwrap();
    // Pre-seed the count to one below the limit so the next bump trips it.
    staging::write_marker_durable(
        &d.join(".restart_count"),
        format!("{}\n", staging::RESTART_LIMIT - 1).as_bytes(),
    )
    .unwrap();
    let quarantined = handle_resume_fsync_failure("_mux", &d, "mux output");
    assert!(quarantined, "reaching RESTART_LIMIT must quarantine");
    let snap = staging::snapshot_staging_disc(&d).expect("snapshot");
    assert!(snap.has_failed, "state Failed written (terminal)");
    assert!(
        !d.join(".ripped").exists(),
        ".ripped dropped so the worker can't re-queue the terminal dir"
    );
    assert!(
        !snap.has_ripped,
        "state is no longer Ripped so the worker can't re-queue the terminal dir"
    );
    assert_eq!(
        staging::restart_count(&d),
        0,
        ".restart_count cleared after quarantine"
    );
}

// FIX (fsync cap preservation): at RESTART_LIMIT, if the terminal.failed write does NOT
// land, handle_resume_fsync_failure must NOT tear down the restart cap or report
// quarantine.
#[test]
fn fsync_failure_at_limit_dropped_write_preserves_cap() {
    let d = tmpdir();
    std::fs::write(d.join(".ripped"), b"{}").unwrap();
    // Make the terminal state.json write fail: a directory can't be renamed over.
    std::fs::create_dir(d.join(staging::STATE_FILE)).unwrap();
    // Legacy counter one below the limit; the next bump trips it (state.json is
    // a dir, so `read_state` is None and the legacy `.restart_count` path runs).
    staging::write_marker_durable(
        &d.join(".restart_count"),
        format!("{}\n", staging::RESTART_LIMIT - 1).as_bytes(),
    )
    .unwrap();
    let quarantined = handle_resume_fsync_failure("_mux", &d, "mux output");
    assert!(
        !quarantined,
        "a dropped terminal write must NOT report a successful quarantine"
    );
    assert_eq!(
        staging::restart_count(&d),
        staging::RESTART_LIMIT,
        "the restart cap must be PRESERVED (not cleared) when the terminal write is dropped"
    );
    assert!(
        d.join(".ripped").exists(),
        ".ripped must stay so a dir that never went terminal isn't stranded"
    );
}

// OPERATOR-CARD PARITY: on the cold operator-resume path a dropped terminal write must
// raise an operator card (record_error) the same way the muxer site does.
#[test]
fn fsync_dropped_write_raises_operator_card() {
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = tmpdir();
    std::fs::write(d.join(".ripped"), b"{}").unwrap();
    // Force the terminal state.json write to fail (a dir can't be renamed
    // over), same trick as `fsync_failure_at_limit_dropped_write_preserves_cap`.
    std::fs::create_dir(d.join(staging::STATE_FILE)).unwrap();
    staging::write_marker_durable(
        &d.join(".restart_count"),
        format!("{}\n", staging::RESTART_LIMIT - 1).as_bytes(),
    )
    .unwrap();
    let path_key = d.to_string_lossy().to_string();
    crate::server::muxer::clear_error(&path_key);

    // A REAL device (cold operator-resume), not the `"_mux"` worker.
    let quarantined = handle_resume_fsync_failure("sg0", &d, "mux output");

    assert!(
        !quarantined,
        "a dropped terminal write must still report NOT quarantined"
    );
    assert!(
        crate::server::muxer::MUX_ERRORS
            .lock()
            .unwrap()
            .contains_key(&path_key),
        "a dropped terminal write on the cold operator-resume path must raise \
             an operator card (MUX_ERRORS) — syslog/device_log alone are not \
             visible on the System page"
    );
    crate::server::muxer::clear_error(&path_key);
}

// Mirrors `fsync_dropped_write_raises_operator_card`: a dropped `.aborted-loss` write must
// also raise an operator card, not just silently retry forever.
#[test]
fn loss_abort_dropped_write_raises_operator_card() {
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = tmpdir();
    // Force the state.json write to fail (a dir can't be renamed over).
    std::fs::create_dir(d.join(staging::STATE_FILE)).unwrap();
    let path_key = d.to_string_lossy().to_string();
    crate::server::muxer::clear_error(&path_key);

    let landed = staging::mark_aborted_on_loss_reporting_landed(&d, "loss exceeds threshold");
    assert!(!landed, "the forced write failure must report landed=false");
    record_loss_abort_write_failure("sg0", &d, "loss exceeds threshold");

    assert!(
        crate::server::muxer::MUX_ERRORS
            .lock()
            .unwrap()
            .contains_key(&path_key),
        "a dropped .aborted-loss write on a real device must raise an \
             operator card (MUX_ERRORS)"
    );
    crate::server::muxer::clear_error(&path_key);
}
