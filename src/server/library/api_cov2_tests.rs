use super::super::queue::{Failure, JobResult, NewJob, StagedFile};
use super::*;

// A kept file whose name is not a kept pair: taken from its job, then refused by the delete.
fn odd_kept(lib: &Library, stage: &std::path::Path) -> PathBuf {
    let target = PathBuf::from("/m/B/B.mkv");
    let mkv = stage.join("B.mkv");
    std::fs::write(&mkv, b"finished").unwrap();
    let job = NewJob {
        title: "B".into(),
        iso: "/i/B.iso".into(),
        target: target.clone(),
        replace: true,
    };
    let staged = StagedFile {
        path: mkv,
        bytes: 8,
        attempts: 1,
    };
    let failure = Failure {
        code: None,
        message: "x".into(),
    };
    lib.queue.adopt_staged(vec![(job, staged, failure)]);
    target
}

fn open(t: &tempfile::TempDir) -> (Library, PathBuf) {
    let stage = t.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(t.path().join("cfg")).unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    (lib, stage)
}

#[test]
fn a_kept_file_that_cannot_be_deleted_answers_500() {
    let t = tempfile::tempdir().unwrap();
    let (lib, stage) = open(&t);
    let target = odd_kept(&lib, &stage);
    let (code, msg) = discard_kept(&lib, &target, Some(&stage), false).unwrap_err();
    assert_eq!(code, 500, "{msg}");
    assert!(msg.starts_with("could not delete"), "{msg}");
}

#[test]
fn clear_staging_counts_a_kept_remux_it_could_not_delete() {
    let t = tempfile::tempdir().unwrap();
    let (lib, stage) = open(&t);
    odd_kept(&lib, &stage);
    let job = lib.queue.claim_next().unwrap();
    lib.queue.finish(
        job.id,
        JobResult::Failed {
            code: None,
            message: "the copy failed".into(),
            finished_at: 0,
        },
    );
    assert_eq!(clear_kept_remuxes(&lib, &stage), (0, 1));
}
