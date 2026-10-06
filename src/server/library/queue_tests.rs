use super::*;

fn job(dir: &Path, name: &str) -> NewJob {
    NewJob {
        title: name.into(),
        iso: dir.join(format!("{name}.iso")),
        target: dir.join(name).join(format!("{name}.mkv")),
        replace: true,
    }
}

#[test]
fn jobs_run_in_order_one_at_a_time_and_pause() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    assert_eq!(q.add(vec![job(t.path(), "a"), job(t.path(), "b")]), 2);
    assert_eq!(q.add(vec![job(t.path(), "a")]), 0, "already queued");
    let a = q.claim_next().unwrap();
    assert_eq!(a.title, "a");
    assert!(q.claim_next().is_none(), "one at a time");
    q.finish(
        a.id,
        JobResult::Failed {
            code: Some(6000),
            message: "E6000".into(),
            finished_at: 1,
        },
    );
    q.set_paused(true);
    assert!(q.claim_next().is_none());
    q.set_paused(false);
    assert_eq!(q.claim_next().unwrap().title, "b");
    // A finished target can be queued again; it replaces the old entry.
    assert_eq!(q.add(vec![job(t.path(), "a")]), 1);
    assert_eq!(
        q.snapshot().jobs.iter().filter(|j| j.title == "a").count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn a_queue_file_that_cannot_be_read_is_kept() {
    use std::os::unix::fs::PermissionsExt as _;
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join(QUEUE_FILE);
    std::fs::write(&file, br#"{"jobs":[]}"#).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o0)).unwrap();
    if std::fs::read(&file).is_ok() {
        return; // root reads anything
    }
    Queue::open(t.path());
    let kept = t.path().join("library-queue.json.unreadable");
    std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(std::fs::read(&kept).unwrap(), br#"{"jobs":[]}"#);
}

#[test]
fn has_work_is_a_running_job_or_a_waiting_one_unpaused() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    assert!(!q.has_work());
    q.add(vec![job(t.path(), "a")]);
    assert!(q.has_work());
    q.set_paused(true);
    assert!(!q.has_work());
    q.set_paused(false);
    q.claim_next().unwrap();
    q.set_paused(true);
    assert!(q.has_work(), "a running job still holds the disks");
}

#[test]
fn a_restart_requeues_the_running_job_first_and_keeps_results() {
    let t = tempfile::tempdir().unwrap();
    let target_b;
    {
        let q = Queue::open(t.path());
        q.add(vec![
            job(t.path(), "a"),
            job(t.path(), "b"),
            job(t.path(), "c"),
        ]);
        let a = q.claim_next().unwrap();
        q.finish(
            a.id,
            JobResult::Done {
                size_bytes: 42,
                secs: 3,
                finished_at: 9,
                writing_app: Some("freemkv 1.8.0".into()),
            },
        );
        let b = q.claim_next().unwrap();
        target_b = b.target.clone();
        std::fs::create_dir_all(b.target.parent().unwrap()).unwrap();
        std::fs::write(&b.target, b"old").unwrap();
        std::fs::write(partial_path(&b.target), b"half").unwrap();
        q.set_debug_log(true);
        // Dropped here while `b` is running: the process died.
    }
    let q = Queue::open(t.path());
    let f = q.snapshot();
    assert!(f.debug_log);
    let b = f.jobs.iter().find(|j| j.title == "b").unwrap();
    assert_eq!(b.state, JobState::Queued);
    assert_eq!(b.note, Some(JobNote::Restarted));
    assert!(!partial_path(&target_b).exists(), "stale partial cleaned");
    assert_eq!(
        std::fs::read(&target_b).unwrap(),
        b"old",
        "old MKV untouched"
    );
    assert!(matches!(
        f.results.values().next(),
        Some(JobResult::Done { size_bytes: 42, .. })
    ));
    assert_eq!(
        q.claim_next().unwrap().title,
        "b",
        "the cut-off job goes first"
    );
    assert_eq!(q.clear_finished(), 1);
    assert_eq!(
        q.snapshot().results.len(),
        1,
        "results outlive the cleared job"
    );
}

#[test]
fn a_failure_is_kept_on_the_job_and_survives_a_restart() {
    let t = tempfile::tempdir().unwrap();
    {
        let q = Queue::open(t.path());
        q.add(vec![job(t.path(), "a")]);
        let a = q.claim_next().unwrap();
        q.finish(
            a.id,
            JobResult::Failed {
                code: Some(7013),
                message: "E7013 Decryption failed".into(),
                finished_at: 1,
            },
        );
    }
    let q = Queue::open(t.path());
    let f = q.snapshot();
    assert_eq!(
        f.jobs[0].failure,
        Some(Failure {
            code: Some(7013),
            message: "E7013 Decryption failed".into()
        })
    );
    // A retry clears it while it runs.
    q.add(vec![job(t.path(), "a")]);
    assert_eq!(q.claim_next().unwrap().failure, None);
}

#[test]
fn an_idle_queue_is_not_rewritten_by_polling() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let file = t.path().join(QUEUE_FILE);
    let (g, m) = (
        q.generation(),
        std::fs::metadata(&file).unwrap().modified().unwrap(),
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    for _ in 0..5 {
        assert!(q.claim_next().is_none());
    }
    assert_eq!(q.generation(), g, "no change, no generation bump");
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        m,
        "no write"
    );
    q.set_paused(true);
    q.add(vec![job(t.path(), "a")]);
    let g = q.generation();
    assert!(q.claim_next().is_none());
    assert_eq!(q.generation(), g, "a paused queue is not rewritten either");
}

#[test]
fn a_preempted_job_goes_back_to_the_head() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "a"), job(t.path(), "b")]);
    let a = q.claim_next().unwrap();
    q.add(vec![job(t.path(), "c")]);
    q.requeue(a.id, JobNote::Preempted);
    assert_eq!(q.claim_next().unwrap().title, "a");
}

#[test]
fn an_unreadable_queue_file_is_set_aside() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join(QUEUE_FILE), b"{ not json").unwrap();
    let q = Queue::open(t.path());
    assert!(q.snapshot().jobs.is_empty());
    assert!(t.path().join("library-queue.json.unreadable").exists());
    let back: QueueFile =
        serde_json::from_slice(&std::fs::read(t.path().join(QUEUE_FILE)).unwrap()).unwrap();
    assert_eq!(back.schema, SCHEMA);
}

#[test]
fn removing_and_dropping_touch_only_what_they_name() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![
        job(t.path(), "a"),
        job(t.path(), "b"),
        job(t.path(), "c"),
    ]);
    let a = q.claim_next().unwrap();
    assert_eq!(
        q.remove_queued(&a.target),
        0,
        "a running job is not removable"
    );
    assert_eq!(q.remove_queued(&job(t.path(), "b").target), 1);
    q.note_running(a.id, JobNote::Stalled);
    assert_eq!(q.snapshot().running().unwrap().note, Some(JobNote::Stalled));
    q.note_running(a.id + 100, JobNote::Cancelled);
    assert_eq!(q.snapshot().running().unwrap().note, Some(JobNote::Stalled));
    q.drop_job(a.id + 1);
    let titles: Vec<_> = q.snapshot().jobs.iter().map(|j| j.title.clone()).collect();
    assert_eq!(titles, ["a", "c"].map(String::from), "b went, a and c stay");
    q.drop_job(a.id);
    assert_eq!(q.snapshot().jobs.len(), 1);
}

// A queue file written before the retry fields and notes existed.
const OLD_QUEUE: &str = r#"{
  "schema": 1,
  "next_id": 3,
  "paused": false,
  "debug_log": true,
  "jobs": [
    {"id": 2, "title": "B", "iso": "/i/B.iso", "target": "/m/B/B.mkv", "replace": true,
     "state": "failed", "queued_at": 5, "started_at": 6, "finished_at": 7, "note": "stalled",
     "failure": {"code": 9073, "message": "E9073 copy"}},
    {"id": 3, "title": "C", "iso": "/i/C.iso", "target": "/m/C/C.mkv", "replace": false,
     "state": "queued", "queued_at": 8, "started_at": null, "finished_at": null, "note": null}
  ],
  "results": {"/m/B/B.mkv": {"outcome": "failed", "code": 9073, "message": "E9073 copy", "finished_at": 7}}
}"#;

#[test]
fn an_old_queue_file_still_loads() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join(QUEUE_FILE), OLD_QUEUE).unwrap();
    let q = Queue::open(t.path());
    let f = q.snapshot();
    assert!(f.debug_log);
    assert_eq!(f.jobs.len(), 2);
    let c = &f.jobs[1];
    assert_eq!(
        (c.attempts, c.not_before, c.staged.as_ref()),
        (0, None, None)
    );
    assert!(!t.path().join("library-queue.json.unreadable").exists());
    assert_eq!(q.claim_next().unwrap().title, "C");
    // What it writes back carries none of the new fields while they are unset.
    let back = std::fs::read_to_string(t.path().join(QUEUE_FILE)).unwrap();
    assert!(
        !back.contains("not_before") && !back.contains("attempts") && !back.contains("staged"),
        "{back}"
    );
}

#[test]
fn a_storage_fault_backs_off_1_5_15_then_hourly_and_gives_up_after_a_day() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "a")]);
    let mut a = q.claim_next().unwrap();
    let now = 1_000_000;
    assert_eq!(
        retry_at(&a, now, true),
        Some(now),
        "a fresh check passed: at once"
    );
    assert_eq!(retry_at(&a, now, false), Some(now + 60));
    let mut waits = Vec::new();
    for _ in 0..5 {
        waits.push(retry_at(&a, now, false).unwrap() - now);
        a.attempts += 1;
        a.first_failed_at = Some(now);
    }
    assert_eq!(waits, [60, 300, 900, 3600, 3600]);
    assert_eq!(
        retry_at(&a, now, true),
        Some(now + 3600),
        "only the first retry skips the wait"
    );
    assert_eq!(
        retry_at(&a, now + RETRY_GIVE_UP_SECS, false),
        None,
        "a day on: give up"
    );
    a.attempts = RETRY_MAX_ATTEMPTS;
    assert_eq!(retry_at(&a, now, false), None);
}

#[test]
fn a_job_waiting_out_its_backoff_is_held_and_others_go_first() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "a"), job(t.path(), "b")]);
    let a = q.claim_next().unwrap();
    let failure = Failure {
        code: Some(9073),
        message: "E9073 copy".into(),
    };
    let later = crate::server::util::epoch_secs() + 3600;
    q.retry_later(a.id, failure.clone(), later, None);
    let f = q.snapshot();
    let held = &f.jobs[0];
    assert_eq!(
        (held.state, held.note, held.attempts, held.not_before),
        (
            JobState::Queued,
            Some(JobNote::WaitingForFolder),
            1,
            Some(later)
        )
    );
    assert_eq!(
        held.failure.as_ref(),
        Some(&failure),
        "the queue says why it waits"
    );
    assert!(held.first_failed_at.is_some());
    assert_eq!(
        q.claim_next().unwrap().title,
        "b",
        "a backoff never blocks the rest"
    );
    let b = q.snapshot().running().unwrap().id;
    q.drop_job(b);
    assert!(q.claim_next().is_none(), "a waits out its backoff");
    assert!(
        !q.has_work(),
        "a waiting job does not hold the disks from audits"
    );
    assert_eq!(
        q.add(vec![job(t.path(), "a")]),
        0,
        "still queued: not queued twice"
    );
    // A kept file waits under its own note and survives a restart.
    let kept = StagedFile {
        path: "/stage/A.0011.staged.mkv".into(),
        bytes: 42,
        attempts: 1,
    };
    q.retry_later(a.id, failure, 0, Some(kept));
    drop(q);
    let q = Queue::open(t.path());
    let j = q.claim_next().unwrap();
    assert_eq!(j.note, Some(JobNote::StagedWaiting));
    assert_eq!(
        j.staged.as_deref(),
        Some(Path::new("/stage/A.0011.staged.mkv"))
    );
    assert_eq!((j.staged_bytes, j.staged_attempts), (Some(42), Some(1)));
    assert_eq!(j.attempts, 2);
}

#[test]
fn clearing_the_queued_jobs_unpauses_and_spares_the_running_one() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "a"), job(t.path(), "b")]);
    q.claim_next().unwrap();
    q.set_paused(true);
    assert_eq!(q.clear_queued(), 1);
    let f = q.snapshot();
    assert!(!f.paused);
    assert_eq!(f.jobs.len(), 1);
    assert!(f.running().is_some());
}

#[test]
fn stop_all_and_unqueue_keep_a_kept_file_offered() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "a"), job(t.path(), "b")]);
    let a = q.claim_next().unwrap();
    let failure = Failure {
        code: None,
        message: "E5000".into(),
    };
    let kept = StagedFile {
        path: "/stage/a.staged.mkv".into(),
        bytes: 7,
        attempts: 1,
    };
    q.retry_later(a.id, failure, 0, Some(kept));
    assert_eq!(q.clear_queued(), 2);
    let f = q.snapshot();
    assert_eq!(f.count(JobState::Queued), 0, "nothing left queued");
    let parked = f.jobs.iter().find(|j| j.id == a.id).unwrap();
    assert_eq!(parked.state, JobState::Failed);
    assert!(parked.staged.is_some(), "the kept file is still offered");
    assert!(
        parked
            .failure
            .as_ref()
            .unwrap()
            .message
            .contains("still kept")
    );
    assert!(q.retry_staged_now(&a.target));
    assert_eq!(q.remove_queued(&a.target), 1);
    assert_eq!(q.snapshot().jobs.len(), 1, "unqueued, still listed");
    assert_eq!(q.take_staged(&a.target), Ok("/stage/a.staged.mkv".into()));
    assert_eq!(q.take_staged(&a.target), Err(false));
    assert_eq!(q.staged_total(), (0, 0));
}
