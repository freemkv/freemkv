use super::super::queue::{Failure, JobState, NewJob, StagedFile};
use super::*;

#[test]
fn match_request_requires_revision_and_typed_media_kind() {
    let good = serde_json::json!({"source":"/iso/source.iso", "expected_revision":0,
        "media":{"title":"Cast Away", "year":2000, "tmdb_id":8358, "season":null, "disc":null, "kind":"movie"}});
    assert!(serde_json::from_value::<MatchRequest>(good.clone()).is_ok());
    let mut missing = good.clone();
    missing.as_object_mut().unwrap().remove("expected_revision");
    assert!(serde_json::from_value::<MatchRequest>(missing).is_err());
    let mut invalid = good.clone();
    invalid["media"]["kind"] = "series-ish".into();
    assert!(serde_json::from_value::<MatchRequest>(invalid).is_err());
    let mut extra = good;
    extra["delete_directory"] = "/movies".into();
    assert!(serde_json::from_value::<MatchRequest>(extra).is_err());
}

#[test]
fn clear_staging_removes_only_abandoned_failures_and_empty_leftovers() {
    use crate::server::ripper::staging::{self, DiscState, StagingState, UserMetadata};
    let root = tempfile::tempdir().unwrap();
    let job = |name: &str, state: StagingState, files: &[&str]| {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        staging::try_write_state(&dir, &DiscState::new(state)).unwrap();
        for file in files {
            std::fs::write(dir.join(file), b"data").unwrap();
        }
        dir
    };
    // Undelivered or resumable output, live owners and operator choices are kept.
    job("review", StagingState::Review, &["Title.iso", "Title.mkv"]);
    job(
        "aborted",
        StagingState::AbortedLoss,
        &["Title.iso", "Title.iso.mapfile"],
    );
    job(
        "stopped",
        StagingState::Stopped,
        &["Title.iso", "Title.iso.mapfile"],
    );
    job("completed", StagingState::Completed, &["Title.mkv"]);
    job("done", StagingState::Done, &["Title.mkv"]);
    job("sweep", StagingState::Sweeping, &["Title.iso"]);
    job("mux", StagingState::Ripped, &["Title.iso"]);
    job(
        "journal",
        StagingState::Failed,
        &["Title.mkv", ".delivery.json"],
    );
    job("leased", StagingState::Failed, &["Title.iso"]);
    let corrected = root.path().join("corrected");
    std::fs::create_dir(&corrected).unwrap();
    let mut st = DiscState::new(StagingState::Stopped);
    st.user_metadata = Some(UserMetadata {
        title: "Cast Away".into(),
        year: 2000,
        media_type: crate::server::planner::MediaKind::Movie,
        episode_start: None,
        tmdb_id: 8358,
        poster_url: String::new(),
        overview: String::new(),
    });
    staging::try_write_state(&corrected, &st).unwrap();
    for name in ["unknown", "corrupt"] {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("important"), b"keep").unwrap();
    }
    std::fs::write(root.path().join("corrupt/state.json"), b"{bad").unwrap();
    // Abandoned terminal failures and leftovers with nothing to deliver go.
    job(
        "failed",
        StagingState::Failed,
        &["Title.iso", "Title.iso.mapfile"],
    );
    job("leftover", StagingState::Stopped, &[".disc-label"]);
    std::fs::create_dir(root.path().join("empty")).unwrap();
    let lease = staging::job_lease(&root.path().join("leased"));
    let _owner = lease.lock().unwrap();
    assert_eq!(clear_idle_staging(root.path()), (3, 0));
    for name in ["failed", "leftover", "empty"] {
        assert!(!root.path().join(name).exists(), "{name} should be cleared");
    }
    for name in [
        "review",
        "aborted",
        "stopped",
        "completed",
        "done",
        "sweep",
        "mux",
        "journal",
        "leased",
        "corrected",
        "unknown",
        "corrupt",
    ] {
        assert!(root.path().join(name).exists(), "{name} must be kept");
    }
    assert!(root.path().join("review/Title.iso").exists());
    assert!(root.path().join("journal/.delivery.json").exists());
}

#[cfg(unix)]
#[test]
fn clear_staging_does_not_follow_directory_symlinks() {
    use crate::server::ripper::staging::{self, DiscState, StagingState};
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    staging::try_write_state(outside.path(), &DiscState::new(StagingState::Stopped)).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
    assert_eq!(clear_idle_staging(root.path()), (0, 0));
    assert!(outside.path().join(staging::STATE_FILE).exists());
    assert!(
        root.path()
            .join("link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

fn kept_job(lib: &Library, stage: &std::path::Path) -> (PathBuf, PathBuf) {
    let target = PathBuf::from("/m/A/A.mkv");
    let mkv = stage.join("A.0123456789ab.staged.mkv");
    std::fs::write(&mkv, b"finished").unwrap();
    std::fs::write(mkv.with_extension("json"), b"{}").unwrap();
    let job = NewJob {
        title: "A".into(),
        iso: "/i/A.iso".into(),
        target: target.clone(),
        replace: true,
    };
    let staged = StagedFile {
        path: mkv.clone(),
        bytes: 8,
        attempts: 1,
    };
    let failure = Failure {
        code: None,
        message: "x".into(),
    };
    lib.queue.adopt_staged(vec![(job, staged, failure)]);
    (target, mkv)
}

#[test]
fn discard_deletes_only_a_kept_pair_in_staging_and_never_while_it_is_copied_in() {
    let t = tempfile::tempdir().unwrap();
    let stage = t.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(t.path().join("cfg")).unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    let (target, mkv) = kept_job(&lib, &stage);
    let elsewhere = t.path().join("other");
    assert_eq!(
        discard_kept(&lib, &target, Some(&elsewhere), false)
            .unwrap_err()
            .0,
        409
    );
    assert_eq!(discard_kept(&lib, &target, None, false).unwrap_err().0, 409);
    assert!(mkv.exists(), "nothing deleted outside the staging folder");
    let running = lib.queue.claim_next().unwrap();
    assert_eq!(
        discard_kept(&lib, &target, Some(&stage), false)
            .unwrap_err()
            .0,
        409
    );
    assert!(mkv.exists(), "never while it is being copied in");
    lib.queue
        .requeue(running.id, super::super::queue::JobNote::Interrupted);
    discard_kept(&lib, &target, Some(&stage), false).unwrap();
    assert!(!mkv.exists() && !mkv.with_extension("json").exists());
    assert!(
        lib.queue.snapshot().jobs.iter().all(|j| j.target != target),
        "the waiting job went with it"
    );
    assert_eq!(
        discard_kept(&lib, &target, Some(&stage), false)
            .unwrap_err()
            .0,
        404
    );
    assert_eq!(lib.queue.snapshot().count(JobState::Queued), 0);
}

#[test]
fn discard_finds_a_planned_jobs_kept_file_by_its_output_target() {
    use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};
    let t = tempfile::tempdir().unwrap();
    let stage = t.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(t.path().join("cfg")).unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    lib.queue.add(vec![NewJob {
        title: "Show".into(),
        iso: "/i/Show.iso".into(),
        target: PathBuf::from("/m/Show/Show.mkv"),
        replace: false,
    }]);
    let job = lib.queue.claim_next().unwrap();
    let plan = RemuxPlan {
        media: MediaMetadata::default(),
        outputs: (1..=2)
            .map(|n| PlannedOutput {
                id: format!("episode-{n}"),
                title_index: n,
                episode: Some(n as u16),
                episode_name: String::new(),
                filename: format!("Show_S01E0{n}.mkv"),
            })
            .collect(),
    };
    assert!(lib.queue.set_plan(job.id, plan));
    let episode = lib.queue.snapshot().jobs[0].output_targets()[1].clone();
    let mkv = stage.join("Show_S01E02.0123456789ab.staged.mkv");
    std::fs::write(&mkv, b"finished").unwrap();
    std::fs::write(mkv.with_extension("json"), b"{}").unwrap();
    lib.queue
        .set_output_staged(job.id, "episode-2", Some(mkv.clone()));
    lib.queue.set_staged(
        job.id,
        Some(StagedFile {
            path: mkv.clone(),
            bytes: 8,
            attempts: 1,
        }),
    );
    lib.queue.finish(
        job.id,
        super::super::queue::JobResult::Failed {
            code: None,
            message: "x".into(),
            finished_at: 1,
        },
    );
    discard_kept(&lib, &episode, Some(&stage), false).unwrap();
    assert!(!mkv.exists());
}

#[test]
fn clear_staging_keeps_a_kept_remux_still_waiting_to_be_copied_in() {
    let t = tempfile::tempdir().unwrap();
    let stage = t.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(t.path().join("cfg")).unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    let (_, mkv) = kept_job(&lib, &stage);
    // Queued to be copied in once the output folder answers: not abandoned.
    assert_eq!(clear_kept_remuxes(&lib, &stage), (0, 0));
    assert!(mkv.exists());
    let job = lib.queue.claim_next().unwrap();
    lib.queue.finish(
        job.id,
        super::super::queue::JobResult::Failed {
            code: None,
            message: "the copy failed".into(),
            finished_at: 0,
        },
    );
    assert_eq!(clear_kept_remuxes(&lib, &stage), (1, 0));
    assert!(!mkv.exists());
}

// Clear staging decides on a snapshot; a Copy in now that lands before it takes the file
// keeps the job and its kept remux.
#[test]
fn clear_staging_never_takes_a_kept_remux_requeued_since_its_snapshot() {
    let t = tempfile::tempdir().unwrap();
    let stage = t.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(t.path().join("cfg")).unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    let (target, mkv) = kept_job(&lib, &stage);
    let job = lib.queue.claim_next().unwrap();
    lib.queue.finish(
        job.id,
        super::super::queue::JobResult::Failed {
            code: None,
            message: "the copy failed".into(),
            finished_at: 0,
        },
    );
    // Clear staging has picked it; the operator presses Copy in now. The file is kept on
    // purpose, which is no failure.
    let cleared = clear_kept_remuxes_with(&lib, &stage, |picked| {
        assert_eq!(picked, target.as_path());
        assert!(lib.queue.retry_staged_now(&target));
    });
    assert_eq!(cleared, (0, 0));
    assert!(mkv.exists());
    assert_eq!(lib.queue.snapshot().count(JobState::Queued), 1);
}
