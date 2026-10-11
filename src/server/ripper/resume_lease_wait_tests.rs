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

// A Ripped job held by `hold` (another drive or the worker) for `held_for`; `meanwhile`
// runs under that hold. Returns the device's (status, last_error) and whether the
// resume took the mux slot.
fn resume_against_a_held_job(
    device: &str,
    lease_wait: Duration,
    held_for: Duration,
    meanwhile: impl FnOnce(&std::path::Path) + Send + 'static,
) -> (String, String, bool) {
    let _env = crate::server::log::env_guard();
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    // SAFETY: env access in tests, serialized by env_guard.
    unsafe {
        std::env::set_var("AUTORIP_DIR", root.path());
    }
    let dir = root.path().join("Heat");
    std::fs::create_dir_all(&dir).unwrap();
    staging::write_state(
        &dir,
        &staging::DiscState::new(staging::StagingState::Ripped),
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let held = dir.clone();
    let owner = std::thread::spawn(move || {
        let lease = staging::job_lease(&held);
        let _owner = lease.lock().unwrap();
        tx.send(()).unwrap();
        // After the resume read the lifecycle it classified the job by.
        std::thread::sleep(Duration::from_millis(150));
        meanwhile(&held);
        std::thread::sleep(held_for);
    });
    rx.recv().unwrap();
    let class = ResumeClass::Remux {
        iso_path: dir.join("Heat.iso"),
        mapfile_path: dir.join("Heat.iso.mapfile"),
        display_name: "Heat".into(),
        title_confident: None,
    };
    let cfg = Arc::new(RwLock::new(Config::default()));
    let mut claimed = false;
    resume_remux_within(&cfg, device, class, lease_wait, || claimed = true);
    owner.join().unwrap();
    let rs = super::super::STATE.lock().unwrap().remove(device).unwrap();
    (rs.status, rs.last_error, claimed)
}

// A wait that ends Busy refuses with the busy reason and never takes the mux slot,
// so a Library remux is not stopped for a resume that does nothing.
#[test]
fn a_resume_that_gives_up_waiting_takes_no_mux_slot() {
    let (status, error, claimed) = resume_against_a_held_job(
        "lease-wait-remux-busy",
        Duration::from_millis(100),
        Duration::from_millis(600),
        |_| {},
    );
    assert_eq!(status, "error");
    assert_eq!(error, super::super::JOB_BUSY_REFUSED);
    assert!(!claimed, "a Busy wait took the mux slot");
    // The mux worker's own wording: no other drive's "nothing was reset".
    let (_, error, _) = resume_against_a_held_job(
        "_mux",
        Duration::from_millis(100),
        Duration::from_millis(600),
        |_| {},
    );
    assert!(
        error.contains("not muxed") && !error.contains("reset"),
        "{error}"
    );
}

// The other owner finished the job while this resume waited: the stale classification is
// not acted on (no re-mux, no mux slot), and the device says why.
#[test]
fn a_job_finished_while_waiting_is_not_remuxed_on_a_stale_classification() {
    let (status, error, claimed) = resume_against_a_held_job(
        "lease-wait-remux-changed",
        Duration::from_secs(10),
        Duration::from_millis(200),
        |dir| {
            staging::write_state(dir, &staging::DiscState::new(staging::StagingState::Done));
        },
    );
    assert_eq!(status, "idle");
    assert!(error.contains("changed while"), "{error}");
    assert!(!claimed);
}
