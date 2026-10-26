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
    assert!(freeze_fixture_plan(&lib, job.id, plan, None).unwrap());
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
    let ending = remux_fixture_with(
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
    let first = remux_fixture_with(
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
    let again = remux_fixture_with(
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
    assert!(
        e.to_string().contains("changed while this remux ran"),
        "{e}"
    );
}

#[cfg(unix)]
#[test]
fn a_landing_that_cannot_be_recorded_durably_is_not_landed() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores modes
    }
    let (t, lib, dirs, job, target) = replacing();
    let before = std::fs::read(&target).unwrap();
    let config = t.path().join("config");
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            // The config folder stops taking writes while the title muxes.
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o500))?;
            let landed = land_like_the_engine(&sink, &request.target, &movie());
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755))?;
            landed
        },
    );
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755)).unwrap();
    let Ending::Failed(e) = ending else {
        panic!("{ending:?}")
    };
    assert!(e.to_string().contains("could not be recorded"), "{e}");
    assert_eq!(std::fs::read(&target).unwrap(), before);
}

// The engine muxing into the remux staging folder: its own "replace" phase lands the local file.
pub(super) fn engine_into_stage(
    staged: &freemkv_engine::RemuxJob,
    sink: &dyn Sink,
) -> std::io::Result<freemkv_engine::RemuxReport> {
    let partial = super::super::super::queue::partial_path(&staged.target);
    std::fs::write(&partial, movie())?;
    sink.event(&Event::Phase { name: "replace" });
    if sink.should_cancel() {
        std::fs::remove_file(&partial)?;
        return Err(libfreemkv::Error::Halted.into());
    }
    std::fs::rename(&partial, &staged.target)?;
    let verified = libfreemkv::probe_mkv_with_cues(std::fs::File::open(&staged.target)?)
        .map_err(std::io::Error::other)?;
    Ok(freemkv_engine::RemuxReport {
        outcome: libfreemkv::MuxOutcome {
            halted: false,
            completed: true,
            output_opened: true,
            bytes_written: movie().len() as u64,
            errors: 0,
            lost_bytes: 0,
            streams: 1,
            undelivered_streams: Vec::new(),
        },
        writing_app: verified.writing_app.clone(),
        verified,
        replaced: false,
    })
}

fn link_stamp(t: &tempfile::TempDir, target: &Path) -> Option<crate::server::util::Stamp> {
    links::read_entries(&t.path().join("config")).unwrap()[target].stamp
}

#[test]
fn a_staged_remux_lands_in_the_library() {
    let (t, lib, dirs, job, target) = replacing();
    let stage = t.path().join("remux-stage");
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, ordinal| execute_staged(request, &sink, &stage, ordinal, engine_into_stage),
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(std::fs::read(&target).unwrap(), movie());
    let landed = crate::server::util::Stamp::read(&target).unwrap();
    assert_eq!(link_stamp(&t, &target), Some(landed));
    assert!(deliver::pending(&stage).is_empty());
}

#[test]
fn a_direct_remux_lands_in_the_library() {
    let (t, lib, dirs, job, target) = replacing();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| land_like_the_engine(&sink, &request.target, &movie()),
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(std::fs::read(&target).unwrap(), movie());
    let landed = crate::server::util::Stamp::read(&target).unwrap();
    assert_eq!(link_stamp(&t, &target), Some(landed));
}

#[test]
fn the_link_carries_the_recorded_landing_not_a_later_look() {
    let (t, lib, dirs, job, target) = replacing();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut landed = None;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            let app = land_like_the_engine(&sink, &request.target, &movie())?;
            landed = Some(crate::server::util::Stamp::read(&request.target)?);
            // Another client replaces the file right after the rename.
            std::fs::write(&request.target, b"a media manager's transcode")?;
            Ok(app)
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(link_stamp(&t, &target), landed);
}

#[cfg(unix)]
#[test]
fn a_staged_landing_that_cannot_be_recorded_keeps_the_verified_file() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores modes
    }
    let (t, lib, dirs, job, target) = replacing();
    let before = std::fs::read(&target).unwrap();
    let (config, stage) = (t.path().join("config"), t.path().join("remux-stage"));
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, ordinal| {
            let engine = |staged: &freemkv_engine::RemuxJob, s: &dyn Sink| {
                let report = engine_into_stage(staged, s);
                // The config folder stops taking writes before the library copy lands.
                std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o500))?;
                report
            };
            let landed = execute_staged(request, &sink, &stage, ordinal, engine);
            std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755))?;
            landed
        },
    );
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o755)).unwrap();
    let Ending::Failed(e) = ending else {
        panic!("{ending:?}")
    };
    assert!(e.to_string().contains("could not be recorded"), "{e}");
    assert_eq!(std::fs::read(&target).unwrap(), before);
    let kept = deliver::pending(&stage);
    assert_eq!(kept.len(), 1, "the verified remux is kept");
    let staged = lib.queue.snapshot().jobs[0].outputs[0].staged.clone();
    assert_eq!(staged.as_deref(), Some(kept[0].staged.as_path()));
}

#[test]
fn frozen_source_refusal_keeps_verified_staging_for_delivery_with_original_provenance() {
    let (t, lib, dirs, job, target) = replacing();
    let before = std::fs::read(&target).unwrap();
    let stage = t.path().join("remux-stage");
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, ordinal| {
            execute_staged(request, &sink, &stage, ordinal, |staged, capture| {
                let report = engine_into_stage(staged, capture)?;
                std::fs::write(&job.iso, b"replacement after local verification")?;
                Ok(report)
            })
        },
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    assert_eq!(std::fs::read(&target).unwrap(), before);
    let kept = deliver::pending(&stage);
    assert_eq!(
        kept.len(),
        1,
        "source refusal must retain the verified local artifact"
    );
    let retry = lib.queue.snapshot().jobs[0].clone();
    assert_eq!(
        retry.outputs[0].staged.as_deref(),
        Some(kept[0].staged.as_path())
    );
    assert_eq!(retry.source_stamp, job.source_stamp);
    let mut retry_sink = test_sink(&lib, &arbiter);
    retry_sink.job_id = job.id;
    let delivered = remux_planned(
        &retry,
        retry.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &retry_sink,
    );
    assert!(matches!(delivered, Ending::Done { .. }), "{delivered:?}");
    assert_eq!(std::fs::read(&target).unwrap(), movie());
    let link = links::read_entries(&t.path().join("config"))
        .unwrap()
        .remove(&target)
        .unwrap();
    assert!(link.iso_stamp.is_none());
    assert!(link.stamp.is_none());
    let linked = links::read_entries(&t.path().join("config")).unwrap();
    assert!(
        super::super::super::owned::capture(
            &linked,
            &job.iso,
            &super::super::super::owned::roots(&dirs)
        )
        .unwrap()
        .is_empty()
    );
}
