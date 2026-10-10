use super::*;
use crate::server::mover::{MOVE_ERRORS, MOVE_STATE, MovePhase, MoveState};
use std::sync::Arc;

fn stalled_copy(mode: u8) {
    let _guard = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let job = temp.path().join("job");
    std::fs::create_dir(&job).unwrap();
    let source = job.join("source.iso");
    let destination = temp.path().join("destination.iso");
    std::fs::write(&source, b"retained source").unwrap();
    *MOVE_STATE.lock().unwrap() = vec![MoveState {
        name: "Disc".into(),
        artifact: "iso".into(),
        phase: MovePhase::Copying,
        progress_pct: 0,
        progress_gb: 0.0,
        total_gb: 1.0,
        speed_mbs: 2.0,
        eta: "1:00".into(),
    }];
    let lease = crate::server::ripper::staging::job_lease(&job);
    let owner_lease = lease.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let job_thread = job.clone();
    let src = source.clone();
    let dst = destination.clone();
    let caller = std::thread::spawn(move || {
        let _ownership = owner_lease.lock().unwrap();
        let halt = libfreemkv::Halt::new();
        let _target = libfreemkv::io::ArtifactLock::acquire(&dst, &[], &halt).unwrap();
        let worker_halt = halt.clone();
        let written = Arc::new(AtomicU64::new(0));
        let worker_written = written.clone();
        let activity = Arc::new(Activity::default());
        let worker_activity = activity.clone();
        let worker_destination = dst.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let (monitor_tx, monitor_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            if mode != 1 {
                worker_activity.set(if mode == 2 {
                    "publishing destination"
                } else {
                    "writing destination temporary file"
                });
                entered_tx.send(()).unwrap();
                monitor_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            let result = if mode == 2 {
                // Model an already-entered kernel publication returning late:
                // cancellation cannot undo it, nor may the monitor delete it.
                std::fs::copy(&src, &worker_destination)
            } else {
                crate::server::mover::copy_counting_with_halt(
                    &src,
                    &worker_destination,
                    &worker_written,
                    &|| false,
                    Some(&worker_halt),
                    &|_| {
                        assert_eq!(
                            mode, 1,
                            "a late write must observe cancellation before checkpoint/publication"
                        );
                        entered_tx.send(()).unwrap();
                        monitor_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok(())
                    },
                    Some(&worker_activity),
                )
            };
            tx.send(result).unwrap();
        });
        // Begin the short injected-stall clock after the normal filesystem
        // flush, so a slow but healthy test host cannot preempt the test hook.
        monitor_rx.recv().unwrap();
        let result = Monitor {
            job: &job_thread,
            destination: &dst,
            written: &written,
            activity: &activity,
            halt: &halt,
            progress: &|_, _, _, _| {
                // If the monitor calls this after stall it would erase Blocked.
                MOVE_STATE.lock().unwrap()[0].phase = MovePhase::Copying;
            },
            source_size: 15,
            stall_window: Duration::from_millis(30),
            poll_interval: Duration::from_millis(2),
        }
        .wait(rx, worker);
        finished_tx.send(result).unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let key = job.to_string_lossy().into_owned();
    while !MOVE_ERRORS
        .lock()
        .unwrap()
        .get(&key)
        .is_some_and(|e| e.worker_active)
    {
        assert!(Instant::now() < deadline, "stall feedback did not arrive");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        lease.try_lock().is_err(),
        "no concurrent retry/cleanup while worker remains blocked"
    );
    assert!(
        finished_rx.try_recv().is_err(),
        "monitor must keep awaiting actual worker completion"
    );
    crate::server::mover::clear_move_error(&key);
    crate::server::mover::clear_all_move_errors();
    let error = MOVE_ERRORS.lock().unwrap().get(&key).unwrap().clone();
    assert!(error.worker_active && error.retry_held);
    assert!(error.reason.contains(destination.to_str().unwrap()));
    assert!(error.hint.contains("Cannot retry"));
    std::thread::sleep(Duration::from_millis(15));
    assert!(matches!(
        MOVE_STATE.lock().unwrap()[0].phase,
        MovePhase::Blocked
    ));
    assert!(source.exists());
    assert!(!destination.exists());
    release_tx.send(()).unwrap();
    let failure = finished_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap_err();
    assert_eq!(failure.kind(), io::ErrorKind::TimedOut);
    caller.join().unwrap();
    assert!(lease.try_lock().is_ok());
    assert!(
        source.exists(),
        "late completion cannot clean up the source"
    );
    assert_eq!(destination.exists(), mode == 2);
    assert!(!MOVE_ERRORS.lock().unwrap().get(&key).unwrap().worker_active);
    crate::server::mover::clear_move_error(&key);
    // Retry can race after drain but before outer batch outcome handling.
    // Clearing mutable UI state must not turn this attempt into generic Failed.
    assert!(matches!(
        crate::server::mover::copy_failure_outcome(&failure),
        crate::server::mover::MoveOutcome::Stalled
    ));
    MOVE_STATE.lock().unwrap().clear();
}

#[test]
fn stalled_write_retains_ownership_and_late_return_cannot_publish() {
    stalled_copy(0);
}

#[test]
fn stalled_checkpoint_cancels_publication_after_callback_returns() {
    stalled_copy(1);
}

#[test]
fn already_entered_publication_returning_late_is_retained_for_reconciliation() {
    stalled_copy(2);
}

#[test]
fn progressing_copy_does_not_hit_a_total_duration_deadline() {
    let _guard = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let written = Arc::new(AtomicU64::new(0));
    let worker_written = written.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for count in 1..=10 {
            worker_written.store(count, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(10));
        }
        tx.send(Ok(10)).unwrap();
    });
    let halt = libfreemkv::Halt::new();
    let result = Monitor {
        job: temp.path(),
        destination: temp.path(),
        written: &written,
        activity: &Activity::default(),
        halt: &halt,
        progress: &|_, _, _, _| {},
        source_size: 10,
        stall_window: Duration::from_millis(50),
        poll_interval: Duration::from_millis(2),
    }
    .wait(rx, worker);
    assert_eq!(result.unwrap(), 10);
    assert!(!halt.is_cancelled());
}
