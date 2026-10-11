//! A resume waiting for a job another drive or the mux worker holds honours Stop,
//! gives up after a bound, and notices the job changed while it waited.
use super::*;
use std::time::{Duration, Instant};

#[test]
fn stop_interrupts_a_resume_waiting_for_the_job_lease() {
    let lease = std::sync::Arc::new(std::sync::Mutex::new(()));
    let held = lease.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        let _g = held.lock().unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(Duration::from_secs(3));
    });
    rx.recv().unwrap();
    let device = "lease-wait-stop";
    let halt = libfreemkv::Halt::new();
    super::super::register_halt(device, halt.clone());
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        halt.cancel();
    });
    let t0 = Instant::now();
    let outcome = wait_for_job_lease(device, &lease, Duration::from_secs(60));
    assert!(matches!(outcome, LeaseWait::Stopped));
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "Stop did not interrupt the wait"
    );
    super::super::unregister_halt(device);
    stopper.join().unwrap();
    owner.join().unwrap();
}

#[test]
fn a_resume_gives_up_on_a_lease_held_past_its_bound() {
    let lease = std::sync::Arc::new(std::sync::Mutex::new(()));
    let held = lease.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        let _g = held.lock().unwrap();
        tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(800));
    });
    rx.recv().unwrap();
    let outcome = wait_for_job_lease("lease-wait-busy", &lease, Duration::from_millis(100));
    assert!(matches!(outcome, LeaseWait::Busy));
    owner.join().unwrap();
    let outcome = wait_for_job_lease("lease-wait-busy", &lease, Duration::from_millis(100));
    assert!(matches!(outcome, LeaseWait::Acquired { waited: false, .. }));
}

#[test]
fn a_job_finished_while_waiting_is_not_remuxed_on_a_stale_classification() {
    use staging::StagingState::*;
    assert!(changed_while_waiting(Some(Ripped), Some(Done)));
    assert!(changed_while_waiting(Some(Stopped), None));
    assert!(!changed_while_waiting(Some(Ripped), Some(Ripped)));
}
