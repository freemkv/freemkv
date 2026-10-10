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
fn clear_staging_preserves_owned_unknown_corrupt_and_delivery_entries() {
    use crate::server::ripper::staging::{self, DiscState, StagingState};
    let root = tempfile::tempdir().unwrap();
    for (name, state) in [
        ("idle", StagingState::Stopped),
        ("sweep", StagingState::Sweeping),
        ("mux", StagingState::Ripped),
        ("move", StagingState::Done),
        ("leased", StagingState::Stopped),
    ] {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        staging::try_write_state(&dir, &DiscState::new(state)).unwrap();
        std::fs::write(dir.join("source.iso"), b"capture").unwrap();
    }
    for name in ["unknown", "corrupt"] {
        let dir = root.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("important"), b"keep").unwrap();
    }
    std::fs::write(root.path().join("corrupt/state.json"), b"{bad").unwrap();
    let lease = staging::job_lease(&root.path().join("leased"));
    let _owner = lease.lock().unwrap();
    assert_eq!(clear_idle_staging(root.path()), (1, 0));
    assert!(!root.path().join("idle").exists());
    for name in ["sweep", "mux", "move", "leased", "unknown", "corrupt"] {
        assert!(root.path().join(name).exists(), "{name}");
    }
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
        discard_kept(&lib, &target, Some(&elsewhere)).unwrap_err().0,
        409
    );
    assert_eq!(discard_kept(&lib, &target, None).unwrap_err().0, 409);
    assert!(mkv.exists(), "nothing deleted outside the staging folder");
    let running = lib.queue.claim_next().unwrap();
    assert_eq!(
        discard_kept(&lib, &target, Some(&stage)).unwrap_err().0,
        409
    );
    assert!(mkv.exists(), "never while it is being copied in");
    lib.queue
        .requeue(running.id, super::super::queue::JobNote::Interrupted);
    discard_kept(&lib, &target, Some(&stage)).unwrap();
    assert!(!mkv.exists() && !mkv.with_extension("json").exists());
    assert!(
        lib.queue.snapshot().jobs.iter().all(|j| j.target != target),
        "the waiting job went with it"
    );
    assert_eq!(
        discard_kept(&lib, &target, Some(&stage)).unwrap_err().0,
        404
    );
    assert_eq!(lib.queue.snapshot().count(JobState::Queued), 0);
}
