//! Tests for the 0.20.8 hang-path fixes that touch autorip-side code.
//!
//! Covers the hard watchdog not touching NFS before exit: the production
//! bounded `.restart_count` bump (`ripper::watchdog_bump_restart_count`,
//! built on `ripper::bounded_call`) returns within its deadline even when
//! the underlying call would never complete, and increments the counter
//! on the happy path.

use std::time::{Duration, Instant};
use tempfile::tempdir;

use freemkv::server::ripper::{bounded_call, staging, watchdog_bump_restart_count};

#[test]
fn watchdog_counter_bump_happy_path_increments() {
    // Sanity: on a healthy staging dir, the production bounded bump returns
    // true within its deadline and the on-disk count increments.
    let tmp = tempdir().expect("tempdir");
    let staging_dir = tmp.path().to_path_buf();
    assert_eq!(staging::restart_count(&staging_dir), 0);

    assert!(
        watchdog_bump_restart_count("test-watchdog", &staging_dir),
        "bounded counter bump should finish within its deadline"
    );
    assert_eq!(
        staging::restart_count(&staging_dir),
        1,
        "happy-path bump must increment the counter"
    );
}

#[test]
fn watchdog_counter_bump_times_out_when_op_hangs() {
    // Simulate a wedged increment_restart_count (sleep far past the
    // deadline); the bounded call must return false near the deadline so
    // the watchdog can exit(1) instead of trapping in a wedged NFS syscall.
    let started = Instant::now();
    let finished = bounded_call("test-bounded-call", Duration::from_millis(200), || {
        std::thread::sleep(Duration::from_secs(30));
    });
    let elapsed = started.elapsed();

    assert!(!finished, "bounded call must time out on wedged op");
    // Returned at the deadline, not at op completion.
    assert!(
        elapsed < Duration::from_secs(10),
        "timeout returned far past deadline: {elapsed:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(150),
        "timeout returned too early: {elapsed:?}"
    );
}

// The settings-save guard-drop test here was REMOVED, not moved: it never
// invoked `handle_settings_post` (private, unreachable) and only proved Rust
// drops a scoped guard. Real coverage now lives in `web::web_tests`.
