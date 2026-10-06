use super::super::queue::{Failure, JobState, NewJob, StagedFile};
use super::*;

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
