//! Regression tests for the v0.13.6 -> v0.13.7 stop-drain bug.
//!
//! `spawn_rip_thread_registers_handle` asserts:
//!   1. `spawn_rip_thread(dev, role, f)` returns Ok.
//!   2. Immediately after spawn (well before the worker exits), a
//!      `JoinHandle` is retrievable via `take_rip_thread(dev)`.
//!   3. Joining the handle does not hang — the worker exits cleanly.

use std::time::{Duration, Instant};

use freemkv_autorip::ripper;

#[test]
fn spawn_rip_thread_registers_handle() {
    let device = "test-sg9";

    // Defensive: clear any stale entry from a prior test run in the
    // same process (the static map is process-global).
    let _ = ripper::take_rip_thread(device);

    let spawn_started = Instant::now();
    ripper::spawn_rip_thread(device, "rip", || {
        std::thread::sleep(Duration::from_millis(100));
    })
    .expect("spawn_rip_thread should succeed");

    // The 100 ms worker is still sleeping; the handle must be in
    // RIP_THREADS now. take() returning None here means registration
    // didn't happen — the v0.13.6 bug this guards against.
    let handle = ripper::take_rip_thread(device);
    assert!(
        spawn_started.elapsed() < Duration::from_millis(80),
        "test setup took too long; the worker may have already exited \
         and been reaped, invalidating the registration check"
    );
    assert!(
        handle.is_some(),
        "spawn_rip_thread must register the JoinHandle in RIP_THREADS \
         so handle_stop / join_rip_thread can drain it"
    );

    // Drain the worker so it doesn't outlive the test process.
    handle
        .unwrap()
        .join()
        .expect("worker thread should join cleanly");

    // Post-condition: a second take returns None — we already removed
    // the entry above.
    assert!(
        ripper::take_rip_thread(device).is_none(),
        "RIP_THREADS entry should be consumed by take_rip_thread"
    );
}

// Catches the old spawn-then-check ordering bug where a duplicate spawn could run its whole
// closure before rejection was noticed.
#[test]
fn a_rejected_duplicate_spawn_never_runs_its_closure() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let device = "test-sg-duplicate-spawn";
    let _ = ripper::take_rip_thread(device);

    // The incumbent: parked until we release it, so it is unambiguously
    // *running* when the duplicate spawn is attempted.
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    ripper::spawn_rip_thread(device, "rip", move || {
        let _ = release_rx.recv();
    })
    .expect("the first spawn owns the device");

    let ran = Arc::new(AtomicBool::new(false));
    let ran_in_worker = Arc::clone(&ran);
    let err = ripper::spawn_rip_thread(device, "rip", move || {
        ran_in_worker.store(true, Ordering::SeqCst);
    })
    .expect_err("a second spawn for a device with a live worker must fail");
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::AlreadyExists,
        "the rejection must be reported as AlreadyExists so the caller's \
         spawn-failure path runs"
    );
    assert!(
        !ran.load(Ordering::SeqCst),
        "the rejected duplicate must be reaped BEFORE it runs any of its work \
         closure — running it means a second rip against the incumbent's \
         staging dir and an HTTP handler blocked for the length of it"
    );

    // The incumbent is untouched by the rejection and still drains normally.
    drop(release_tx);
    ripper::take_rip_thread(device)
        .expect("the incumbent keeps the registration slot")
        .join()
        .expect("incumbent joins cleanly");
}
