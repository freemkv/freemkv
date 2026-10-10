use super::*;

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
                None,
                None,
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
    std::fs::write(&item.iso, b"source").unwrap();
    let replacement =
        super::super::replacement::Replacement::capture(&item.iso, &Default::default()).unwrap();
    assert!(
        q.try_mutate_durable_with(
            |f| {
                Ok(Queue::append(
                    f,
                    vec![item.clone()],
                    false,
                    Some(corrected_match()),
                    Some(replacement),
                ))
            },
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
    assert!(q.add_corrected(item.clone(), corrected_match()).is_err());
    std::fs::remove_dir(tmp).unwrap();
    assert_eq!(q.add_corrected(item, corrected_match()).unwrap(), 0);
    let claimed = q.claim_next().unwrap();
    assert_eq!(claimed.id, id);
    assert_eq!(claimed.selected_match, Some(corrected_match()));
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

fn corrected_match() -> crate::server::library::matches::SavedMatch {
    crate::server::library::matches::SavedMatch {
        revision: 9,
        media: crate::server::planner::MediaMetadata {
            title: "Cast Away".into(),
            year: 2000,
            tmdb_id: 8358,
            kind: Some(crate::server::planner::MediaKind::Movie),
            ..Default::default()
        },
    }
}

#[test]
fn replacement_phase_checkpoints_survive_postrename_failure_and_legacy_write() {
    for phase in ["prepared", "retired", "complete"] {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        let item = job(t.path(), "old");
        std::fs::write(&item.iso, b"source").unwrap();
        q.add_corrected(item, corrected_match()).unwrap();
        let claimed = q.claim_next().unwrap();
        let mut journal = serde_json::to_value(claimed.replacement.unwrap()).unwrap();
        journal["prepared"] = serde_json::json!([{
            "target": claimed.target,
            "candidate": t.path().join("candidate"),
            "identity": null,
            "writing_app": null
        }]);
        if phase != "prepared" {
            journal["retired"] = serde_json::json!([t.path().join("old-output")]);
        }
        if phase == "complete" {
            journal["complete"] = serde_json::json!(true);
        }
        let expected: super::super::replacement::Replacement =
            serde_json::from_value(journal).unwrap();
        assert!(
            q.try_mutate_durable_with(
                |f| {
                    f.jobs[0].replacement = Some(expected.clone());
                    Ok(())
                },
                fail_directory_sync
            )
            .is_err()
        );
        q.set_debug_log(true);
        let reopened = Queue::open(t.path());
        assert_eq!(
            reopened.snapshot().jobs[0].replacement.as_ref(),
            Some(&expected),
            "phase {phase}"
        );
    }
}

#[test]
fn unfinished_replacement_survives_drop_clear_and_legacy_enqueue() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let item = job(t.path(), "old");
    std::fs::write(&item.iso, b"source").unwrap();
    q.add_corrected(item, corrected_match()).unwrap();
    let claimed = q.claim_next().unwrap();
    // Model the durable checkpoint reserving a candidate before preparation.
    let mut journal = serde_json::to_value(claimed.replacement.as_ref().unwrap()).unwrap();
    journal["prepared"] = serde_json::json!([{
        "target": claimed.target,
        "candidate": t.path().join("reserved-candidate"),
        "identity": null,
        "writing_app": null
    }]);
    let replacement = serde_json::from_value(journal).unwrap();
    q.save_replacement(claimed.id, replacement).unwrap();
    q.drop_job(claimed.id);
    assert_eq!(
        q.clear_finished(),
        0,
        "cleanup journal is not disposable history"
    );
    assert_eq!(
        q.add(vec![job(t.path(), "old")]),
        0,
        "legacy enqueue cannot erase the journal"
    );
    let mut other = job(t.path(), "other-target");
    other.iso = claimed.iso.clone();
    assert_eq!(
        q.add(vec![other]),
        0,
        "same source under another target cannot bypass ownership"
    );
    drop(q);
    let q = Queue::open(t.path());
    let saved = q.snapshot();
    assert_eq!(saved.jobs.len(), 1);
    assert_eq!(saved.jobs[0].id, claimed.id);
    assert!(saved.jobs[0].has_replacement_work());
    assert_eq!(saved.jobs[0].state, JobState::Failed);
}

#[test]
fn corrected_enqueue_freezes_identity_durably_before_claim() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let selected = corrected_match();
    std::fs::write(t.path().join("old.iso"), b"source").unwrap();
    assert_eq!(
        q.add_corrected(job(t.path(), "old"), selected.clone())
            .unwrap(),
        1
    );
    let disk: QueueFile = serde_json::from_slice(&std::fs::read(&q.path).unwrap()).unwrap();
    assert_eq!(disk.jobs[0].selected_match.as_ref(), Some(&selected));
    drop(q);
    let q = Queue::open(t.path());
    let claimed = q.claim_next().unwrap();
    assert_eq!(claimed.selected_match, Some(selected));
    assert!(claimed.plan.is_none());
    let replacement = claimed.replacement.unwrap();
    replacement.verify_source(&claimed.iso).unwrap();
    std::fs::write(&claimed.iso, b"different source").unwrap();
    assert!(replacement.verify_source(&claimed.iso).is_err());
}

#[test]
fn corrected_enqueue_refuses_unreadable_ownership_without_resetting_links() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let item = job(t.path(), "old");
    std::fs::write(&item.iso, b"source").unwrap();
    let links = t.path().join(crate::server::library::links::LINKS_FILE);
    std::fs::write(&links, b"corrupt provenance").unwrap();
    assert!(q.add_corrected(item, corrected_match()).is_err());
    assert!(q.snapshot().jobs.is_empty());
    assert_eq!(std::fs::read(links).unwrap(), b"corrupt provenance");
}

#[test]
fn corrected_enqueue_write_failure_does_not_publish_or_consume_id() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let before = q.snapshot();
    let generation = q.generation();
    std::fs::write(t.path().join("old.iso"), b"source").unwrap();
    std::fs::rename(&q.path, q.path.with_extension("saved")).unwrap();
    std::fs::create_dir(&q.path).unwrap();
    assert!(
        q.add_corrected(job(t.path(), "old"), corrected_match())
            .is_err()
    );
    assert_eq!(q.snapshot(), before);
    assert_eq!(q.generation(), generation);
    assert!(q.claim_next().is_none());
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
    assert_eq!(q.add_corrected(other, corrected_match()).unwrap(), 0);
    assert_eq!(q.snapshot().jobs.len(), 1);
}

#[test]
fn corrected_source_replans_without_reusing_prior_episode_selection() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![job(t.path(), "old")]);
    let first = q.claim_next().unwrap();
    let plan = crate::server::planner::RemuxPlan {
        version: crate::server::planner::PLAN_VERSION,
        source_iso: first.iso.clone(),
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
    assert_eq!(q.add_replanned(vec![job(t.path(), "old")]), 1);
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
    assert_eq!(q.add_replanned(vec![new]), 0);
    let running = q.claim_next().unwrap();
    let mut new = job(t.path(), "new");
    new.iso = running.iso;
    assert_eq!(q.add_replanned(vec![new]), 0);
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
        version: crate::server::planner::PLAN_VERSION,
        source_iso: claimed.iso.clone(),
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
        version: crate::server::planner::PLAN_VERSION,
        source_iso: claimed.iso,
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
