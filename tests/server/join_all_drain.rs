//! Coverage for the shutdown drain, `ripper::join_all_rip_threads`.
//!
//! Runs in its own integration binary because the function is process-global (cancels every
//! registered device's `Halt`, joins every registered thread) and would drain other tests'
//! fixtures out from under them if shared.

use std::time::{Duration, Instant};

use freemkv::server::ripper;

// Phase 1 catches a missing halt.cancel() (workers never exit, joins time out). Phase 2, with
// workers that ignore Halt, catches a per-device timeout regression (N-drive shutdown blocking
// N×timeout). One test: the function is process-global, so the phases cannot run in parallel.
#[test]
fn join_all_cancels_every_halt_and_shares_one_budget() {
    let devs: Vec<String> = (0..3)
        .map(|i| format!("join-all-drain-{}-{}", std::process::id(), i))
        .collect();
    let mut done = Vec::new();
    for d in &devs {
        let _ = ripper::take_rip_thread(d);
        let halt = libfreemkv::Halt::new();
        ripper::register_halt(d, halt.clone());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        done.push(rx);
        ripper::spawn_rip_thread(d, "rip", move || {
            // A phase loop: polls its Halt exactly like sweep / patch / mux.
            while !halt.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            let _ = tx.send(());
        })
        .expect("spawn must register");
    }

    let t0 = Instant::now();
    ripper::join_all_rip_threads(Duration::from_secs(5));
    let elapsed = t0.elapsed();

    for rx in done {
        rx.recv_timeout(Duration::from_secs(1))
            .expect("every worker must have been cancelled and drained");
    }
    assert!(
        elapsed < Duration::from_secs(5),
        "the drain took {elapsed:?}: the halts must be cancelled before the \
         joins begin, and three devices must share ONE budget"
    );
    for d in &devs {
        assert!(
            ripper::take_rip_thread(d).is_none(),
            "a drained thread must have been joined and unregistered"
        );
        ripper::unregister_halt(d);
    }

    // Phase 2: three workers that never look at their Halt, so every join runs out its budget.
    let budget = Duration::from_millis(400);
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    for d in &devs {
        ripper::register_halt(d, libfreemkv::Halt::new());
        let release = release.clone();
        ripper::spawn_rip_thread(d, "rip", move || {
            while !release.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
        .expect("spawn must register");
    }
    let t0 = Instant::now();
    ripper::join_all_rip_threads(budget);
    let elapsed = t0.elapsed();
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    for d in &devs {
        ripper::take_rip_thread(d)
            .expect("a thread that outlived the budget stays registered")
            .join()
            .expect("worker");
        ripper::unregister_halt(d);
    }
    assert!(
        elapsed >= budget,
        "the drain gave up after {elapsed:?}, before its {budget:?} budget"
    );
    assert!(
        elapsed < budget * 2,
        "three stuck devices took {elapsed:?}: they must share ONE {budget:?} budget, \
         not get one each"
    );
}
