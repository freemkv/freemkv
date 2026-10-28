use super::*;

// Channels control the writer; timeouts only bound a regression's failure rather
// than advance the scenario. Always release it before asserting or joining.
fn blocked_durable_write_preserves_reads_and_mutations(failure: Option<bool>) {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    let t = tempfile::tempdir().unwrap();
    let q = Arc::new(Queue::open(t.path()));
    let original = q.snapshot();
    let (writing, entered) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let durable = {
        let q = q.clone();
        std::thread::spawn(move || {
            q.try_mutate_durable_with(
                |f| {
                    f.debug_log = true;
                    Ok(())
                },
                |path, bytes| {
                    writing.send(()).unwrap();
                    released.recv().unwrap();
                    match failure {
                        None => write_queue_durable(path, bytes),
                        Some(false) => Err((false, std::io::Error::other("before publish"))),
                        Some(true) => fail_directory_sync(path, bytes),
                    }
                },
            )
        })
    };
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let (starting, started) = mpsc::channel();
    let (committing, committed) = mpsc::channel();
    let ordinary = {
        let q = q.clone();
        std::thread::spawn(move || {
            starting.send(()).unwrap();
            q.mutate(|f| {
                committing.send(()).unwrap();
                f.paused = true;
            });
        })
    };
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    let (reading, read) = mpsc::channel();
    let reader = {
        let q = q.clone();
        std::thread::spawn(move || reading.send(q.snapshot()).unwrap())
    };
    let visible = read.recv_timeout(Duration::from_secs(2));
    // The mutation parks on the durable writer's gate; give a regression time to slip past it.
    let premature_mutation = committed.recv_timeout(Duration::from_millis(300));
    release.send(()).unwrap();
    let result = durable.join().unwrap();
    ordinary.join().unwrap();
    reader.join().unwrap();
    assert_eq!(
        visible.expect("snapshot blocked behind durable writer"),
        original
    );
    assert!(matches!(
        premature_mutation,
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(result.is_err(), failure.is_some());
    assert!(q.snapshot().paused, "ordinary mutation was lost");
    assert_eq!(q.snapshot().debug_log, failure != Some(false));
    assert_eq!(Queue::open(t.path()).snapshot(), q.snapshot());
}

#[test]
fn blocked_durable_success_keeps_readers_available_and_both_mutations() {
    blocked_durable_write_preserves_reads_and_mutations(None);
}

#[test]
fn blocked_durable_before_publish_failure_keeps_readers_available_and_ordinary_mutation() {
    blocked_durable_write_preserves_reads_and_mutations(Some(false));
}

#[test]
fn blocked_durable_after_publish_failure_keeps_readers_available_and_both_mutations() {
    blocked_durable_write_preserves_reads_and_mutations(Some(true));
}

#[test]
fn before_publish_failure_preserves_state_generations_and_pending_durability() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    for pending in [false, true] {
        q.durability_pending.store(pending, Ordering::SeqCst);
        let before = q.snapshot();
        let generation = q.generation();
        let saved = *q.saved.lock().unwrap();
        let result = q.try_mutate_durable_with(
            |f| {
                f.debug_log = true;
                Ok(())
            },
            |_, _| Err((false, std::io::Error::other("before publish"))),
        );
        assert!(result.is_err());
        assert_eq!(q.snapshot(), before);
        assert_eq!(q.generation(), generation);
        assert_eq!(*q.saved.lock().unwrap(), saved);
        assert_eq!(q.durability_pending.load(Ordering::SeqCst), pending);
    }
}

#[test]
fn successful_durable_commit_rejects_an_old_snapshot_before_writing() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let generation = q.generation();
    let bytes = serde_json::to_vec(&q.snapshot()).unwrap();
    q.try_mutate_durable(|f| {
        f.debug_log = true;
        Ok(())
    })
    .unwrap();
    q.persist_snapshot(generation, &bytes, |_, _| {
        panic!("stale snapshot reached writer")
    });
    assert!(Queue::open(t.path()).snapshot().debug_log);
}

#[test]
fn postrename_failure_preserves_landed_queue_before_legacy_persist() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let result = q.try_mutate_durable_with(
        |f| {
            Ok(Queue::append(
                f,
                vec![job(t.path(), "landed")],
                false,
                false,
            ))
        },
        |path, bytes| {
            write_queue(path, bytes, |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "injected directory sync failure after rename",
                ))
            })
        },
    );
    assert!(result.is_err());
    let disk: QueueFile = serde_json::from_slice(&std::fs::read(&q.path).unwrap()).unwrap();
    assert_eq!(
        q.snapshot(),
        disk,
        "landed intent must not be lost from memory"
    );
    q.set_debug_log(true);
    let reopened = Queue::open(t.path());
    assert_eq!(reopened.snapshot().jobs.len(), 1);
}

fn fail_directory_sync(path: &Path, bytes: &[u8]) -> Result<(), (bool, std::io::Error)> {
    write_queue(path, bytes, |_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "directory sync injected failure",
        ))
    })
}

#[test]
fn uncertain_checkpoint_retry_cannot_take_unchanged_state_success_shortcut() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    assert!(
        q.try_mutate_durable_with(
            |f| {
                f.debug_log = true;
                Ok(())
            },
            fail_directory_sync
        )
        .is_err()
    );
    let before = q.generation();
    assert!(
        q.try_mutate_durable_with(
            |f| {
                f.debug_log = true;
                Ok(())
            },
            fail_directory_sync
        )
        .is_err()
    );
    assert!(q.generation() > before);
    assert!(q.durability_pending.load(Ordering::SeqCst));
    q.try_mutate_durable(|f| {
        f.debug_log = true;
        Ok(())
    })
    .unwrap();
    assert!(!q.durability_pending.load(Ordering::SeqCst));
}

#[test]
fn old_inflight_snapshot_cannot_overwrite_uncertain_published_checkpoint() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let old_generation = q.generation();
    let old_bytes = serde_json::to_vec(&q.snapshot()).unwrap();
    assert!(
        q.try_mutate_durable_with(
            |f| {
                f.debug_log = true;
                Ok(())
            },
            fail_directory_sync
        )
        .is_err()
    );
    q.persist_snapshot(old_generation, &old_bytes, |_, _| {
        panic!("stale snapshot reached writer")
    });
    assert!(q.durability_pending.load(Ordering::SeqCst));
    let disk: QueueFile = serde_json::from_slice(&std::fs::read(&q.path).unwrap()).unwrap();
    assert!(disk.debug_log);
}

#[test]
fn uncertain_admission_keeps_frozen_intent_and_blocks_claim_until_storage_recovers() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let item = job(t.path(), "old");
    assert!(
        q.try_mutate_durable_with(
            |f| Ok(Queue::append(f, vec![item.clone()], false, true)),
            fail_directory_sync
        )
        .is_err()
    );
    let id = q.snapshot().jobs[0].id;
    // Prevent the claim gate's durability retry from writing its temporary file.
    let tmp = q.path.with_file_name(format!("{QUEUE_FILE}.tmp"));
    std::fs::create_dir(&tmp).unwrap();
    assert!(q.claim_next().is_none());
    assert_eq!(q.snapshot().jobs[0].state, JobState::Queued);
    std::fs::remove_dir(tmp).unwrap();
    assert_eq!(
        q.add_fresh(vec![item], true),
        0,
        "the source is already queued"
    );
    let claimed = q.claim_next().unwrap();
    assert_eq!(claimed.id, id);
    assert!(claimed.retire);
    assert_eq!(q.snapshot().jobs.len(), 1);
}

fn job(dir: &Path, name: &str) -> NewJob {
    NewJob {
        title: name.into(),
        iso: dir.join(format!("{name}.iso")),
        target: dir.join(name).join(format!("{name}.mkv")),
        replace: true,
    }
}

#[test]
fn a_change_match_is_queued_durably_to_retire_and_planned_at_claim() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    assert_eq!(q.add_fresh(vec![job(t.path(), "old")], true), 1);
    let disk: QueueFile = serde_json::from_slice(&std::fs::read(&q.path).unwrap()).unwrap();
    assert!(disk.jobs[0].retire);
    drop(q);
    let q = Queue::open(t.path());
    let claimed = q.claim_next().unwrap();
    assert!(claimed.retire);
    assert!(claimed.plan.is_none() && claimed.owned.is_none());
}

#[test]
fn corrected_enqueue_excludes_other_jobs_using_same_iso() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let first = job(t.path(), "old");
    std::fs::write(&first.iso, b"source").unwrap();
    let mut other = job(t.path(), "different");
    other.iso = first.iso.clone();
    q.add(vec![first]);
    assert_eq!(q.add_fresh(vec![other], true), 0);
    assert_eq!(q.snapshot().jobs.len(), 1);
}

#[test]
fn corrected_source_replans_without_reusing_prior_episode_selection() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "old")]);
    let first = q.claim_next().unwrap();
    let plan = crate::server::planner::RemuxPlan {
        media: Default::default(),
        outputs: vec![crate::server::planner::PlannedOutput {
            id: "episode".into(),
            title_index: 6,
            episode: Some(1),
            episode_name: "Wrong episode".into(),
            filename: "old.mkv".into(),
        }],
    };
    assert!(q.set_plan(first.id, plan));
    q.finish(
        first.id,
        JobResult::Failed {
            code: None,
            message: "stopped".into(),
            finished_at: 1,
        },
    );
    assert_eq!(q.add_fresh(vec![job(t.path(), "old")], false), 1);
    let fresh = q.claim_next().unwrap();
    assert!(fresh.plan.is_none());
    assert!(fresh.outputs.is_empty());
    drop(q);
    let reopened = Queue::open(t.path());
    assert!(reopened.snapshot().jobs.iter().all(|j| j.plan.is_none()));
}

#[test]
fn correction_cannot_compete_with_an_active_source_at_a_different_target() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "old")]);
    let mut new = job(t.path(), "new");
    new.iso = t.path().join("old.iso");
    assert_eq!(q.add_fresh(vec![new], true), 0);
    let running = q.claim_next().unwrap();
    let mut new = job(t.path(), "new");
    new.iso = running.iso;
    assert_eq!(q.add_fresh(vec![new], true), 0);
    assert_eq!(q.snapshot().jobs.len(), 1);
}

#[test]
fn saving_a_match_refuses_active_and_retained_source_jobs() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "old")]);
    let source = t.path().join("old.iso");
    let called = std::cell::Cell::new(false);
    assert!(
        q.with_idle_source(&source, || {
            called.set(true);
            Ok(())
        })
        .is_err()
    );
    assert!(!called.get());
    let running = q.claim_next().unwrap();
    assert!(q.with_idle_source(&source, || Ok(())).is_err());
    q.finish(
        running.id,
        JobResult::Failed {
            code: None,
            message: "stopped".into(),
            finished_at: 1,
        },
    );
    assert!(q.with_idle_source(&source, || Ok(())).is_ok());
    q.mutate(|f| f.jobs[0].staged = Some(t.path().join("kept.mkv")));
    assert!(q.with_idle_source(&source, || Ok(())).is_err());
    assert!(
        q.with_idle_source(&t.path().join("different.iso"), || Ok(()))
            .is_ok()
    );
}

#[test]
fn cleanup_holds_ownership_until_unlink_before_a_new_claim_creates_its_partial() {
    use std::sync::{TryLockError, mpsc};
    use std::time::Duration;
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let new = job(t.path(), "a");
    let partial = partial_path(&new.target);
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(&partial, b"orphan").unwrap();
    q.add(vec![new]);
    std::thread::scope(|scope| {
        let q = &q;
        let partial = &partial;
        let (checked_tx, checked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let cleanup = scope.spawn(move || {
            q.remove_orphan_partial_with(partial, |p| {
                checked_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                std::fs::remove_file(p)
            })
        });
        checked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let locked = matches!(q.state.try_lock(), Err(TryLockError::WouldBlock));
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let claim = scope.spawn(move || {
            attempt_tx.send(()).unwrap();
            let claimed = q.claim_next().unwrap();
            std::fs::write(partial_path(&claimed.target), b"active").unwrap();
        });
        attempt_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        release_tx.send(()).unwrap();
        assert!(cleanup.join().unwrap());
        claim.join().unwrap();
        assert!(
            locked,
            "ownership must remain locked at the unlink boundary"
        );
    });
    assert_eq!(std::fs::read(&partial).unwrap(), b"active");
    assert!(!q.remove_orphan_partial(&partial));
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

#[test]
fn a_claimed_job_persists_its_frozen_plan_once() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "show")]);
    let claimed = q.claim_next().unwrap();
    let plan = crate::server::planner::RemuxPlan {
        media: crate::server::planner::MediaMetadata {
            title: "Show".into(),
            kind: Some(crate::server::planner::MediaKind::Tv),
            ..Default::default()
        },
        outputs: vec![crate::server::planner::PlannedOutput {
            id: "title-1-episode-1".into(),
            title_index: 1,
            episode: Some(1),
            episode_name: "Pilot".into(),
            filename: "Show_S01E01.mkv".into(),
        }],
    };
    assert!(q.set_plan(claimed.id, plan.clone()));
    assert_eq!(q.snapshot().jobs[0].outputs[0].state, OutputState::Pending);
    let kept = t.path().join("1.staged.mkv");
    q.set_output_staged(claimed.id, "title-1-episode-1", Some(kept.clone()));
    assert_eq!(q.snapshot().jobs[0].outputs[0].staged, Some(kept));
    assert!(q.begin_output(claimed.id, "title-1-episode-1"));
    q.finish_output(claimed.id, "title-1-episode-1", OutputState::Done);
    assert!(!q.begin_output(claimed.id, "title-1-episode-1"));
    assert!(
        !q.set_plan(claimed.id, plan.clone()),
        "a plan is immutable once attached"
    );
    drop(q);
    let q = Queue::open(t.path());
    assert_eq!(q.snapshot().jobs[0].plan, Some(plan));
    assert_eq!(q.snapshot().jobs[0].outputs[0].state, OutputState::Done);
}

#[test]
fn frozen_plan_write_failure_leaves_job_unplanned() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "movie")]);
    let claimed = q.claim_next().unwrap();
    let before = q.snapshot();
    std::fs::rename(&q.path, q.path.with_extension("saved")).unwrap();
    std::fs::create_dir(&q.path).unwrap();
    let plan = crate::server::planner::RemuxPlan {
        media: Default::default(),
        outputs: vec![crate::server::planner::PlannedOutput {
            id: "movie".into(),
            title_index: 0,
            episode: None,
            episode_name: String::new(),
            filename: "movie.mkv".into(),
        }],
    };
    assert!(!q.set_plan(claimed.id, plan));
    assert_eq!(q.snapshot(), before);
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
        q.remove_queued(&a.target, None),
        0,
        "a running job is not removable"
    );
    assert_eq!(q.remove_queued(&job(t.path(), "b").target, None), 1);
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
    assert_eq!(q.remove_queued(&a.target, None), 1);
    assert_eq!(q.snapshot().jobs.len(), 1, "unqueued, still listed");
    assert_eq!(
        q.take_staged(&a.target, false),
        Ok("/stage/a.staged.mkv".into())
    );
    assert_eq!(q.take_staged(&a.target, false), Err(Untaken::Gone));
    assert_eq!(q.staged_total(), (0, 0));
}

#[test]
fn sweeping_a_missing_partial_never_touches_the_share_under_the_queue_lock() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let missing = t.path().join("never-written.mkv.partial");
    assert!(!q.remove_orphan_partial_with(&missing, |_| panic!(
        "unlink attempted for a missing partial"
    )));
}

#[test]
fn forgetting_a_jobs_kept_file_forgets_it_on_its_output_too() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "show")]);
    let claimed = q.claim_next().unwrap();
    let plan = crate::server::planner::RemuxPlan {
        media: crate::server::planner::MediaMetadata::default(),
        outputs: vec![crate::server::planner::PlannedOutput {
            id: "feature".into(),
            title_index: 1,
            episode: None,
            episode_name: String::new(),
            filename: "show.mkv".into(),
        }],
    };
    assert!(q.set_plan(claimed.id, plan));
    let kept = t.path().join("1.staged.mkv");
    q.set_output_staged(claimed.id, "feature", Some(kept.clone()));
    q.set_staged(
        claimed.id,
        Some(StagedFile {
            path: kept,
            bytes: 1,
            attempts: 0,
        }),
    );
    // The kept file turned out unusable: the job forgets it.
    q.set_staged(claimed.id, None);
    assert_eq!(q.snapshot().jobs[0].outputs[0].staged, None);
}

// A schema-2 job from a development build, mid-way through the old replacement transaction.
const DEV_QUEUE: &str = r#"{"schema": 2, "next_id": 4, "jobs": [{
  "id": 4, "title": "Show", "iso": "/i/Show.iso", "target": "/m/Show/Show.mkv",
  "plan": {"version": 1, "source_iso": "/i/Show.iso",
    "media": {"title": "Show", "year": 2001, "tmdb_id": 7, "season": 1, "disc": null, "kind": "tv"},
    "outputs": [{"id": "title-0", "title_index": 0, "episode": null, "episode_name": "", "filename": "Show.mkv"}]},
  "selected_match": {"revision": 2, "media": {"title": "Show", "year": 2001, "tmdb_id": 7, "season": 1, "disc": null, "kind": "tv"}},
  "replacement": {"source": {"canonical": "/i/Show.iso", "size": 3, "modified": {"secs_since_epoch": 1, "nanos_since_epoch": 0}},
    "old_outputs": [{"path": "/m/Old.mkv", "identity": {"canonical": "/m/Old.mkv", "size": 3, "modified": {"secs_since_epoch": 1, "nanos_since_epoch": 0}}}],
    "prepared": [{"target": "/m/Show/Show.mkv", "candidate": "/m/Show/.replacement-4-0.mkv", "identity": null}],
    "retired": [], "complete": false},
  "outputs": [{"id": "title-0", "target": "/m/Show/Show.mkv", "state": "running"}],
  "replace": true, "state": "running", "queued_at": 1}]}"#;

#[test]
fn a_dev_queue_file_with_the_old_replacement_fields_loads_as_a_plain_job() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join(QUEUE_FILE), DEV_QUEUE).unwrap();
    let q = Queue::open(t.path());
    assert!(!t.path().join("library-queue.json.unreadable").exists());
    let job = q.claim_next().unwrap();
    assert_eq!(job.id, 4);
    assert!(!job.retire, "never deletes anything");
    assert_eq!(job.plan.unwrap().outputs[0].filename, "Show.mkv");
    let back = std::fs::read_to_string(t.path().join(QUEUE_FILE)).unwrap();
    for gone in ["replacement", "selected_match", "source_iso", "\"version\""] {
        assert!(!back.contains(gone), "{gone}: {back}");
    }
}

// A job of `iso` planned into two outputs beside `target`.
fn planned_two(q: &Queue, dir: &Path) -> (Job, Vec<PathBuf>) {
    q.add(vec![job(dir, "Show")]);
    let claimed = q.claim_next().unwrap();
    let plan = RemuxPlan {
        media: crate::server::planner::MediaMetadata::default(),
        outputs: (1..=2)
            .map(|n| crate::server::planner::PlannedOutput {
                id: format!("episode-{n}"),
                title_index: n,
                episode: Some(n as u16),
                episode_name: String::new(),
                filename: format!("Show_S01E0{n}.mkv"),
            })
            .collect(),
    };
    assert!(q.set_plan(claimed.id, plan));
    let job = q.snapshot().jobs[0].clone();
    let targets = job.output_targets();
    (job, targets)
}

#[test]
fn remove_finds_a_planned_job_by_any_of_its_output_targets() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let (job, targets) = planned_two(&q, t.path());
    q.requeue(job.id, JobNote::Interrupted);
    assert_eq!(q.remove_queued(&targets[1], None), 1);
    assert!(q.snapshot().jobs.is_empty());
}

#[cfg(unix)]
#[test]
fn a_queue_file_that_cannot_be_read_is_left_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join(QUEUE_FILE);
    std::fs::write(&file, br#"{"schema":2,"next_id":4,"jobs":[]}"#).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&file).is_ok() {
        return; // running as a user that ignores modes
    }
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "A")]);
    assert!(
        q.unreadable()
            .is_some_and(|why| why.contains("cannot be read"))
    );
    assert!(
        q.claim_next().is_none(),
        "nothing runs from a queue that was not loaded"
    );
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        br#"{"schema":2,"next_id":4,"jobs":[]}"#
    );
    assert!(!t.path().join("library-queue.json.unreadable").exists());
}

#[test]
fn every_damaged_queue_file_is_set_aside_under_its_own_name() {
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join(QUEUE_FILE);
    std::fs::write(&file, b"first damage").unwrap();
    drop(Queue::open(t.path()));
    std::fs::write(&file, b"second damage").unwrap();
    drop(Queue::open(t.path()));
    assert_eq!(
        std::fs::read(t.path().join("library-queue.json.unreadable")).unwrap(),
        b"first damage"
    );
    assert_eq!(
        std::fs::read(t.path().join("library-queue.json.unreadable.1")).unwrap(),
        b"second damage"
    );
}

#[cfg(unix)]
#[test]
fn a_queue_file_that_could_not_be_read_is_loaded_once_it_reads_again() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join(QUEUE_FILE);
    let first = Queue::open(t.path());
    first.add(vec![job(t.path(), "A")]);
    drop(first);
    let written = std::fs::read(&file).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&file).is_ok() {
        return; // running as a user that ignores modes
    }
    let q = Queue::open(t.path());
    let opened = crate::server::util::epoch_secs();
    assert!(q.unreadable().is_some());
    // Still unreadable: held, and the file is never written over.
    assert!(!q.retry_unreadable(opened + 60));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), written);
    // At most one try a minute.
    assert!(!q.retry_unreadable(opened + 61));
    assert!(q.unreadable().is_some());
    assert!(q.retry_unreadable(opened + 120));
    assert!(q.unreadable().is_none());
    assert_eq!(q.snapshot().jobs.len(), 1);
    assert!(q.claim_next().is_some(), "the loaded queue runs");
}

#[cfg(unix)]
#[test]
fn nothing_changes_a_queue_whose_file_could_not_be_read() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join(QUEUE_FILE);
    let first = Queue::open(t.path());
    first.add(vec![job(t.path(), "A")]);
    drop(first);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&file).is_ok() {
        return; // running as a user that ignores modes
    }
    let q = Queue::open(t.path());
    let opened = crate::server::util::epoch_secs();
    // An operator's Pause during the hold is refused, not kept in memory and then lost.
    q.set_paused(true);
    q.set_debug_log(true);
    assert!(!q.snapshot().paused);
    assert!(!q.snapshot().debug_log);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(q.retry_unreadable(opened + 60));
    assert_eq!(q.snapshot().jobs.len(), 1);
    q.set_paused(true);
    assert!(q.snapshot().paused);
}

// A partial the sweep cannot remove leaves no `<target>.lock` the sweep made, and keeps
// one that was already there.
#[test]
fn a_partial_the_sweep_keeps_leaves_no_lock_file_it_made() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let target = t.path().join("Movie (2020)/Movie (2020).mkv");
    let partial = partial_path(&target);
    let sidecar = libfreemkv::io::artifact_lock::lock_path(&target);
    std::fs::create_dir_all(partial.parent().unwrap()).unwrap();
    std::fs::write(&partial, b"orphan").unwrap();
    let refused = |_: &Path| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    assert!(!q.remove_orphan_partial_with(&partial, refused));
    assert!(partial.exists());
    assert!(!sidecar.exists(), "the sweep left a lock file it made");
    std::fs::write(&sidecar, b"").unwrap();
    assert!(!q.remove_orphan_partial_with(&partial, refused));
    assert!(sidecar.exists(), "a lock file that was there is kept");
    // A running job's partial is skipped without a lock file left behind.
    std::fs::remove_file(&sidecar).unwrap();
    let mut running = job(t.path(), "Movie (2020)");
    running.target = target.clone();
    q.add(vec![running]);
    q.claim_next().unwrap();
    assert!(!q.remove_orphan_partial(&partial));
    assert!(
        !sidecar.exists(),
        "the sweep left a lock file beside a running job"
    );
}

// A lock that cannot be taken for a reason other than its holder is logged, not skipped
// silently: here another user's sidecar this process cannot open.
#[cfg(unix)]
#[test]
fn a_partial_lock_that_cannot_be_opened_is_logged() {
    use std::os::unix::fs::PermissionsExt;
    use tracing_subscriber::layer::SubscriberExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root opens the unreadable sidecar anyway
    }
    #[derive(Clone, Default)]
    struct Buf(std::sync::Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let target = t.path().join("Movie/Movie.mkv");
    let partial = partial_path(&target);
    let sidecar = libfreemkv::io::artifact_lock::lock_path(&target);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&sidecar, b"").unwrap();
    std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::write(&partial, b"orphan").unwrap();
    let buf = Buf::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(buf.clone())
            .with_ansi(false),
    );
    let removed = tracing::subscriber::with_default(subscriber, || {
        q.remove_orphan_partial_with(&partial, |_| panic!("unlinked without its lock"))
    });
    assert!(!removed);
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(out.contains("its lock could not be taken"), "{out}");
}

fn failed_landing_donor(q: &Queue, dir: &Path) -> (Job, Vec<PathBuf>) {
    let (job, targets) = planned_two(q, dir);
    let mut owned = Vec::new();
    for (output, target) in job.outputs.iter().zip(&targets) {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, output.id.as_bytes()).unwrap();
        let stamp = crate::server::util::Stamp::read(target).unwrap();
        owned.push(super::super::owned::Owned {
            path: target.clone(),
            stamp,
        });
        q.set_output_landing(job.id, &output.id, stamp).unwrap();
        q.finish_output(job.id, &output.id, OutputState::Failed);
    }
    owned.push(super::super::owned::Owned {
        path: dir.join("obsolete.mkv"),
        stamp: owned[0].stamp,
    });
    q.set_owned(job.id, owned).unwrap();
    q.finish(
        job.id,
        JobResult::Failed {
            code: None,
            message: "links failed".into(),
            finished_at: 1,
        },
    );
    q.mutate(|f| f.jobs[0].retire = true);
    (q.snapshot().jobs[0].clone(), targets)
}

#[test]
fn row_reenqueue_transfers_only_selected_failed_output_and_ownership_after_reopen() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let (old, targets) = failed_landing_donor(&q, t.path());
    assert_eq!(
        q.add(vec![NewJob {
            title: "second".into(),
            iso: old.iso.clone(),
            target: targets[1].clone(),
            replace: true
        }]),
        1
    );
    drop(q);
    let q = Queue::open(t.path());
    let new = q.claim_next().unwrap();
    assert_ne!(new.id, old.id);
    assert_eq!(
        new.plan.as_ref().unwrap().outputs,
        vec![old.plan.as_ref().unwrap().outputs[1].clone()]
    );
    assert_eq!(
        new.outputs,
        vec![OutputProgress {
            id: old.outputs[1].id.clone(),
            target: targets[1].clone(),
            state: OutputState::Pending,
            staged: None,
            landing: old.outputs[1].landing,
            source_unproven: old.outputs[1].source_unproven,
        }]
    );
    assert_eq!(
        new.owned,
        Some(vec![old.owned.as_ref().unwrap()[1].clone()])
    );
    assert!(!new.retire);
    assert_eq!(new.attempts, 0);
}

#[test]
fn row_reenqueue_does_not_transfer_ineligible_or_misaligned_landing() {
    for case in [
        "source",
        "target",
        "no-landing",
        "done",
        "output-stage",
        "job-stage",
        "missing-id",
        "duplicate-id",
        "fresh",
    ] {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        let (old, targets) = failed_landing_donor(&q, t.path());
        let mut item = NewJob {
            title: "second".into(),
            iso: old.iso.clone(),
            target: targets[1].clone(),
            replace: true,
        };
        match case {
            "source" => item.iso = t.path().join("other.iso"),
            "target" => item.target = t.path().join("other.mkv"),
            "no-landing" => q.mutate(|f| f.jobs[0].outputs[1].landing = None),
            "done" => q.mutate(|f| f.jobs[0].state = JobState::Done),
            "output-stage" => {
                q.set_output_staged(old.id, &old.outputs[1].id, Some(t.path().join("kept.mkv")))
            }
            "job-stage" => q.mutate(|f| f.jobs[0].staged = Some(t.path().join("kept.mkv"))),
            "missing-id" => {
                q.mutate(|f| f.jobs[0].plan.as_mut().unwrap().outputs[1].id = "unrelated".into())
            }
            "duplicate-id" => q.mutate(|f| {
                let plan = f.jobs[0].plan.as_mut().unwrap();
                plan.outputs.push(plan.outputs[1].clone());
            }),
            "fresh" => {}
            _ => unreachable!(),
        }
        let count = if case == "fresh" {
            q.add_fresh(vec![item], false)
        } else {
            q.add(vec![item])
        };
        if case == "job-stage" {
            assert_eq!(count, 0);
            continue;
        }
        assert_eq!(count, 1, "{case}");
        let new = q.snapshot().jobs.last().unwrap().clone();
        assert!(new.outputs.iter().all(|o| o.landing.is_none()), "{case}");
        assert!(new.owned.is_none(), "{case}");
    }
}

#[test]
fn row_reenqueue_keeps_an_empty_baseline_when_donor_has_no_captured_ownership() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let (old, targets) = failed_landing_donor(&q, t.path());
    q.mutate(|f| f.jobs[0].owned = None);
    assert_eq!(
        q.add(vec![NewJob {
            title: "second".into(),
            iso: old.iso,
            target: targets[1].clone(),
            replace: true
        }]),
        1
    );
    drop(q);
    let q = Queue::open(t.path());
    assert_eq!(q.claim_next().unwrap().owned, Some(Vec::new()));
}
