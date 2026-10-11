use super::*;
use crate::server::library::links;
use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};
use std::path::PathBuf;

// A remux job replacing `<library>/A/A.mkv`, linked to its ISO as freemkv wrote it.
fn replacing() -> (tempfile::TempDir, Library, Dirs, Job, PathBuf) {
    let (t, lib, dirs) = library_with(&["A"]);
    let target = dirs.library.join("A/A.mkv");
    let iso = dirs.isos.as_ref().unwrap().join("A.iso");
    links::record(&t.path().join("config"), &target, &iso).unwrap();
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso,
        target: target.clone(),
        replace: true,
    }]);
    let job = lib.queue.claim_next().unwrap();
    let plan = RemuxPlan {
        media: MediaMetadata::default(),
        outputs: vec![PlannedOutput {
            id: "main".into(),
            title_index: 0,
            episode: None,
            episode_name: String::new(),
            filename: "A.mkv".into(),
        }],
    };
    assert!(lib.queue.set_plan(job.id, plan));
    let job = lib.queue.snapshot().jobs[0].clone();
    (t, lib, dirs, job, target)
}

fn cfg(dirs: &Dirs) -> Config {
    Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    }
}

// What the engine does after its verify: the "replace" phase, a last look at the halt, the rename.
fn land_like_the_engine(
    sink: &JobSink<'_>,
    target: &Path,
    bytes: &[u8],
) -> std::io::Result<Option<String>> {
    let partial = super::super::super::queue::partial_path(target);
    std::fs::write(&partial, bytes)?;
    sink.event(&Event::Phase { name: "replace" });
    if sink.should_cancel() {
        std::fs::remove_file(&partial)?;
        return Err(libfreemkv::Error::Halted.into());
    }
    std::fs::rename(&partial, target)?;
    Ok(Some(current_stamp()))
}

#[test]
fn a_library_file_changed_during_the_mux_is_not_replaced_at_landing() {
    let (_t, lib, dirs, job, target) = replacing();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            // Another tool rewrites the library file while the title muxes.
            std::fs::write(&request.target, b"written by another tool during the mux")?;
            land_like_the_engine(&sink, &request.target, &movie())
        },
    );
    let Ending::Failed(e) = ending else {
        panic!("{ending:?}")
    };
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e}");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"written by another tool during the mux"
    );
}

#[test]
fn a_restart_after_the_new_file_landed_but_before_done_finishes_the_output() {
    let (t, lib, dirs, job, target) = replacing();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    // The rename commits; the process dies before the output is recorded Done.
    let first = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            land_like_the_engine(&sink, &request.target, &movie())?;
            Err(std::io::Error::other("killed after the rename"))
        },
    );
    assert!(matches!(first, Ending::Failed(_)), "{first:?}");
    assert_eq!(std::fs::read(&target).unwrap(), movie());
    let job = lib.queue.snapshot().jobs[0].clone();
    let mut calls = 0;
    let again = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |_, _, _| {
            calls += 1;
            Ok(None)
        },
    );
    assert!(matches!(again, Ending::Done { .. }), "{again:?}");
    assert_eq!(calls, 0, "nothing is muxed again");
    assert_eq!(
        lib.queue.snapshot().jobs[0].outputs[0].state,
        OutputState::Done
    );
    let links = links::read(&t.path().join("config")).unwrap();
    assert!(links.contains_key(&target));
}

#[test]
fn may_replace_refuses_an_owned_path_that_is_now_a_symlink_as_changed() {
    let t = tempfile::tempdir().unwrap();
    let (real, target) = (t.path().join("real.mkv"), t.path().join("A.mkv"));
    std::fs::write(&target, b"ours").unwrap();
    let owned = vec![Owned {
        path: target.clone(),
        stamp: crate::server::util::Stamp::read(&target).unwrap(),
    }];
    std::fs::remove_file(&target).unwrap();
    std::fs::write(&real, b"ours").unwrap();
    std::os::unix::fs::symlink(&real, &target).unwrap();
    let e = may_replace(&target, &owned).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e}");
    assert!(e.to_string().contains("changed since"), "{e}");
}
