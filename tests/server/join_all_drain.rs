//! Coverage for the shutdown drain, `ripper::join_all_rip_threads`.
//!
//! Runs in its own integration binary because the function is process-global (cancels every
//! registered device's `Halt`, joins every registered thread) and would drain other tests'
//! fixtures out from under them if shared.

use std::time::{Duration, Instant};

use freemkv_autorip::ripper;

// Catches missing halt.cancel() (workers never exit, joins time out) and a per-device timeout
// regression (N-drive shutdown blocking N×timeout).
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
}
