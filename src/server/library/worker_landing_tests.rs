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

// A links-only failure must leave the queue's pre-rename proof durable until the link is saved.
#[test]
fn failed_link_write_preserves_landing_across_restarts_and_retries() {
    let (t, lib, dirs, job, target) = replacing();
    let file = t.path().join("config").join(links::LINKS_FILE);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let first = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            let app = land_like_the_engine(&sink, &request.target, &movie())?;
            std::fs::remove_file(&file)?;
            std::fs::create_dir(&file)?;
            Ok(app)
        },
    );
    assert!(
        matches!(first, Ending::Failed(ref e) if e.to_string().contains("is in place")),
        "{first:?}"
    );
    let original = crate::server::util::Stamp::read(&target).unwrap();
    assert_eq!(
        lib.queue.snapshot().jobs[0].outputs[0].landing,
        Some(original)
    );
    assert_ne!(
        lib.queue.snapshot().jobs[0].outputs[0].state,
        OutputState::Done
    );
    let current = lib.queue.snapshot().jobs[0].clone();
    let text = failure_message("links could not be saved", None, None, &current, None);
    assert!(!text.contains("No MKV was written"), "{text}");
    assert!(!text.contains("existing MKV is unchanged"), "{text}");
    assert_eq!(std::fs::read(&target).unwrap(), movie());
    drop(sink);
    drop(lib);
    // Fail the recovery branch's link write as well; it must retain the same proof.
    for repaired in [false, true] {
        if repaired {
            std::fs::remove_dir(&file).unwrap();
        }
        let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
        let job = lib.queue.claim_next().unwrap();
        assert_eq!(job.outputs[0].landing, Some(original));
        let mut sink = test_sink(&lib, &arbiter);
        sink.job_id = job.id;
        let again = remux_fixture_with(
            &job,
            job.plan.as_ref().unwrap(),
            &cfg(&dirs),
            &sink,
            |_, _, _| panic!("a landed output must never execute again"),
        );
        assert_eq!(
            lib.landed.load(Ordering::SeqCst),
            0,
            "recovery must not notify another landing"
        );
        if repaired {
            assert!(matches!(again, Ending::Done { .. }), "{again:?}");
            assert_eq!(link_stamp(&t, &target), Some(original));
            assert_eq!(
                lib.queue.snapshot().jobs[0].outputs[0].state,
                OutputState::Done
            );
            assert_eq!(lib.queue.snapshot().jobs[0].outputs[0].landing, None);
        } else {
            assert!(matches!(again, Ending::Failed(_)), "{again:?}");
            assert_eq!(
                lib.queue.snapshot().jobs[0].outputs[0].landing,
                Some(original)
            );
        }
    }
}

#[test]
fn retained_landing_cannot_vouch_for_a_replacement_file_or_directory() {
    for directory in [false, true] {
        let (t, lib, dirs, job, target) = replacing();
        let file = t.path().join("config").join(links::LINKS_FILE);
        let arbiter = Arbiter::new();
        let mut sink = test_sink(&lib, &arbiter);
        sink.job_id = job.id;
        let first = remux_fixture_with(
            &job,
            job.plan.as_ref().unwrap(),
            &cfg(&dirs),
            &sink,
            |request, _, _| {
                let app = land_like_the_engine(&sink, &request.target, &movie())?;
                std::fs::remove_file(&file)?;
                std::fs::create_dir(&file)?;
                Ok(app)
            },
        );
        assert!(matches!(first, Ending::Failed(_)));
        std::fs::remove_dir(&file).unwrap();
        std::fs::remove_file(&target).unwrap();
        if directory {
            std::fs::create_dir(&target).unwrap();
        } else {
            std::fs::write(&target, b"another application's replacement").unwrap();
        }
        let job = lib.queue.snapshot().jobs[0].clone();
        let again = remux_fixture_with(
            &job,
            job.plan.as_ref().unwrap(),
            &cfg(&dirs),
            &sink,
            |_, _, _| panic!("a replacement must not be overwritten"),
        );
        assert!(matches!(again, Ending::Failed(_)), "{again:?}");
        assert!(
            !links::read_entries(&t.path().join("config"))
                .unwrap()
                .contains_key(&target)
        );
        assert_ne!(
            lib.queue.snapshot().jobs[0].outputs[0].state,
            OutputState::Done
        );
    }
}

#[test]
fn legacy_done_without_landing_still_cannot_acquire_ownership() {
    let (t, lib, dirs, job, target) = replacing();
    std::fs::remove_file(t.path().join("config").join(links::LINKS_FILE)).unwrap();
    lib.queue.finish_output(job.id, "main", OutputState::Done);
    let job = lib.queue.snapshot().jobs[0].clone();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |_, _, _| panic!("Done must not execute"),
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(link_stamp(&t, &target), None);
}

#[test]
fn a_durable_link_before_done_completes_without_another_landing() {
    let (t, lib, dirs, job, target) = replacing();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let first = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            land_like_the_engine(&sink, &request.target, &movie())?;
            Err(std::io::Error::other("killed after rename"))
        },
    );
    assert!(matches!(first, Ending::Failed(_)));
    let original = lib.queue.snapshot().jobs[0].outputs[0].landing.unwrap();
    links::record(&t.path().join("config"), &target, &job.iso).unwrap();
    let job = lib.queue.snapshot().jobs[0].clone();
    let again = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |_, _, _| panic!("durably linked output must not execute"),
    );
    assert!(matches!(again, Ending::Done { .. }), "{again:?}");
    assert_eq!(lib.landed.load(Ordering::SeqCst), 0);
    assert_eq!(link_stamp(&t, &target), Some(original));
    assert_eq!(
        lib.queue.snapshot().jobs[0].outputs[0].state,
        OutputState::Done
    );
}

#[test]
fn first_output_link_failure_stops_later_outputs_until_recovery() {
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
    let outputs = ["A.mkv", "B.mkv"]
        .iter()
        .enumerate()
        .map(|(i, name)| PlannedOutput {
            id: format!("title-{i}"),
            title_index: i,
            episode: None,
            episode_name: String::new(),
            filename: (*name).into(),
        })
        .collect();
    assert!(
        freeze_fixture_plan(
            &lib,
            job.id,
            RemuxPlan {
                media: MediaMetadata::default(),
                outputs
            },
            None
        )
        .unwrap()
    );
    let job = lib.queue.snapshot().jobs[0].clone();
    let file = t.path().join("config").join(links::LINKS_FILE);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = Vec::new();
    let first = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, ordinal| {
            calls.push(ordinal);
            let app = land_like_the_engine(&sink, &request.target, &movie())?;
            std::fs::remove_file(&file)?;
            std::fs::create_dir(&file)?;
            Ok(app)
        },
    );
    assert!(matches!(first, Ending::Failed(_)));
    assert_eq!(calls, vec![0]);
    assert!(!target.with_file_name("B.mkv").exists());
    let original = lib.queue.snapshot().jobs[0].outputs[0].landing.unwrap();
    std::fs::remove_dir(&file).unwrap();
    let job = lib.queue.snapshot().jobs[0].clone();
    let again = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, ordinal| {
            calls.push(ordinal);
            land_like_the_engine(&sink, &request.target, &movie())
        },
    );
    assert!(matches!(again, Ending::Done { .. }), "{again:?}");
    assert_eq!(calls, vec![0, 1]);
    assert_eq!(link_stamp(&t, &target), Some(original));
    assert!(
        lib.queue.snapshot().jobs[0]
            .outputs
            .iter()
            .all(|o| o.state == OutputState::Done)
    );
    assert_eq!(lib.landed.load(Ordering::SeqCst), 2);
}

// Model the persisted Failed state left by a links-only failure, independently of
// the worker change that retains landing until the link is durable.
fn failed_row_landing(
    originally_owned: bool,
    renamed: bool,
) -> (tempfile::TempDir, Library, Dirs, Job, PathBuf) {
    let (t, lib, dirs, job, target) = replacing();
    std::fs::remove_file(t.path().join("config").join(links::LINKS_FILE)).unwrap();
    let owned = if originally_owned {
        vec![Owned {
            path: target.clone(),
            stamp: crate::server::util::Stamp::read(&target).unwrap(),
        }]
    } else {
        std::fs::remove_file(&target).unwrap();
        Vec::new()
    };
    lib.queue.set_owned(job.id, owned).unwrap();
    let partial = super::super::super::queue::partial_path(&target);
    std::fs::write(&partial, movie()).unwrap();
    lib.queue
        .set_output_landing(
            job.id,
            "main",
            crate::server::util::Stamp::read(&partial).unwrap(),
        )
        .unwrap();
    if renamed {
        std::fs::rename(&partial, &target).unwrap();
    } else {
        std::fs::remove_file(partial).unwrap();
    }
    lib.queue.finish_output(job.id, "main", OutputState::Failed);
    lib.queue.finish(
        job.id,
        JobResult::Failed {
            code: None,
            message: "landed file's links could not be saved".into(),
            finished_at: 1,
        },
    );
    let failed = lib.queue.snapshot().jobs[0].clone();
    assert_eq!(failed.state, JobState::Failed);
    assert!(failed.outputs[0].landing.is_some());
    (t, lib, dirs, failed, target)
}

fn reenqueue_row(t: &tempfile::TempDir, lib: Library, old: &Job) -> (Library, Job) {
    assert_eq!(
        lib.queue.add(vec![NewJob {
            title: old.title.clone(),
            iso: old.iso.clone(),
            target: old.target.clone(),
            replace: true,
        }]),
        1
    );
    drop(lib);
    let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
    let job = lib.queue.claim_next().unwrap();
    (lib, job)
}

#[test]
fn row_reenqueue_repairs_retained_landing_without_another_mux_or_notification() {
    for originally_owned in [false, true] {
        let (t, lib, dirs, old, target) = failed_row_landing(originally_owned, true);
        let bytes = std::fs::read(&target).unwrap();
        let (lib, job) = reenqueue_row(&t, lib, &old);
        assert_eq!(job.outputs[0].state, OutputState::Pending);
        assert_eq!(job.outputs[0].landing, old.outputs[0].landing);
        assert_eq!(job.owned, old.owned);
        assert_eq!(job.plan, old.plan);
        assert!(!job.retire);
        let arbiter = Arbiter::new();
        let mut sink = test_sink(&lib, &arbiter);
        sink.job_id = job.id;
        let mut calls = 0;
        let ending = remux_fixture_with(
            &job,
            job.plan.as_ref().unwrap(),
            &cfg(&dirs),
            &sink,
            |_, _, _| {
                calls += 1;
                Ok(None)
            },
        );
        assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
        assert_eq!(calls, 0);
        assert_eq!(lib.landed.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(&target).unwrap(), bytes);
        assert_eq!(link_stamp(&t, &target), old.outputs[0].landing);
        assert_eq!(
            lib.queue.snapshot().jobs[0].outputs[0].state,
            OutputState::Done
        );
    }
}

fn assert_row_reenqueue_cannot_recapture_replacement(originally_owned: bool) {
    let (t, lib, dirs, old, target) = failed_row_landing(originally_owned, true);
    std::fs::write(&target, b"someone else's replacement").unwrap();
    let (lib, job) = reenqueue_row(&t, lib, &old);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = 0;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            calls += 1;
            land_like_the_engine(&sink, &request.target, &movie())
        },
    );
    let Ending::Failed(e) = ending else {
        panic!("{ending:?}")
    };
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        calls, 0,
        "replace:true must not capture a fresh ownership baseline"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"someone else's replacement"
    );
    assert!(
        links::read_entries(&t.path().join("config"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn row_reenqueue_cannot_recapture_originally_unowned_replacement() {
    assert_row_reenqueue_cannot_recapture_replacement(false);
}

#[test]
fn row_reenqueue_cannot_recapture_originally_owned_replacement() {
    assert_row_reenqueue_cannot_recapture_replacement(true);
}

#[test]
fn row_reenqueue_reruns_when_original_owned_file_never_got_the_rename() {
    let (t, lib, dirs, old, target) = failed_row_landing(true, false);
    let (lib, job) = reenqueue_row(&t, lib, &old);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = 0;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            calls += 1;
            assert!(request.replace);
            land_like_the_engine(&sink, &request.target, &movie())
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(calls, 1);
    assert_eq!(std::fs::read(target).unwrap(), movie());
}

#[test]
fn row_reenqueue_reruns_when_retained_landing_target_is_missing() {
    let (t, lib, dirs, old, target) = failed_row_landing(false, true);
    std::fs::remove_file(&target).unwrap();
    let (lib, job) = reenqueue_row(&t, lib, &old);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = 0;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            calls += 1;
            assert!(!request.replace);
            land_like_the_engine(&sink, &request.target, &movie())
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(calls, 1);
    assert_eq!(std::fs::read(target).unwrap(), movie());
}

#[test]
fn row_reenqueue_of_done_output_still_performs_requested_remux() {
    let (t, lib, dirs, old, target) = failed_row_landing(false, true);
    lib.queue.finish_output(old.id, "main", OutputState::Done);
    lib.queue.finish(
        old.id,
        JobResult::Done {
            size_bytes: movie().len() as u64,
            secs: 1,
            finished_at: 1,
            writing_app: Some(current_stamp()),
        },
    );
    let (lib, job) = reenqueue_row(&t, lib, &old);
    assert!(job.outputs[0].landing.is_none());
    assert!(job.owned.is_none());
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = 0;
    let ending = remux_fixture_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |request, _, _| {
            calls += 1;
            land_like_the_engine(&sink, &request.target, &movie())
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(calls, 1);
    assert_eq!(std::fs::read(target).unwrap(), movie());
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

#[test]
fn linked_landing_failure_reenqueue_recovers_after_source_change_without_remux() {
    for source_missing in [false, true] {
        for proof in ["known", "legacy", "uncertain"] {
            let (t, lib, dirs, job, target) = replacing();
            let file = t.path().join("config").join(links::LINKS_FILE);
            let arbiter = Arbiter::new();
            let mut sink = test_sink(&lib, &arbiter);
            sink.job_id = job.id;
            let first = remux_fixture_with(
                &job,
                job.plan.as_ref().unwrap(),
                &cfg(&dirs),
                &sink,
                |request, _, _| {
                    let app = land_like_the_engine(&sink, &request.target, &movie())?;
                    std::fs::remove_file(&file)?;
                    std::fs::create_dir(&file)?;
                    Ok(app)
                },
            );
            assert!(
                matches!(first, Ending::Failed(ref e) if e.to_string().contains("is in place")),
                "{first:?}"
            );
            let landed = crate::server::util::Stamp::read(&target).unwrap();
            assert_eq!(
                lib.queue.snapshot().jobs[0].outputs[0].landing,
                Some(landed)
            );
            finish(&lib, &job, first, Duration::ZERO, &sink);
            let failed = lib.queue.snapshot().jobs[0].clone();
            assert_eq!(failed.state, JobState::Failed);
            assert_ne!(failed.outputs[0].state, OutputState::Done);
            assert!(failed.staged.is_none());
            let queue_path = lib.queue.path().to_path_buf();
            drop(sink);
            drop(lib);

            // Legacy/uncertain jobs cannot freshly execute. Seed only those persisted
            // recovery preconditions after the real failure, retaining its landing proof.
            if proof != "known" {
                let mut wire: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&queue_path).unwrap()).unwrap();
                if proof == "legacy" {
                    wire["jobs"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("source_stamp");
                    wire["jobs"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("title_identities");
                } else {
                    wire["jobs"][0]["outputs"][0]["source_unproven"] = true.into();
                }
                std::fs::write(&queue_path, serde_json::to_vec(&wire).unwrap()).unwrap();
            }
            if source_missing {
                std::fs::remove_file(&job.iso).unwrap();
            } else {
                std::fs::write(&job.iso, b"replacement ISO after failed link registration")
                    .unwrap();
            }
            std::fs::remove_dir(&file).unwrap();
            let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
            let donor = lib.queue.snapshot().jobs[0].clone();
            assert_eq!(
                lib.queue.add(vec![NewJob {
                    title: donor.title.clone(),
                    iso: donor.iso.clone(),
                    target: donor.target.clone(),
                    replace: true,
                }]),
                1
            );
            drop(lib);
            let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
            let retry = lib.queue.claim_next().unwrap();
            assert_ne!(retry.id, donor.id);
            assert_eq!(retry.outputs[0].landing, Some(landed));
            assert_eq!(retry.owned, donor.owned);
            assert_eq!(retry.source_stamp, donor.source_stamp);
            assert_eq!(retry.title_identities, donor.title_identities);
            assert_eq!(retry.outputs[0].source_unproven, proof == "uncertain");
            let mut sink = test_sink(&lib, &arbiter);
            sink.job_id = retry.id;
            let ending = remux_fixture_with(
                &retry,
                retry.plan.as_ref().unwrap(),
                &cfg(&dirs),
                &sink,
                |_, _, _| panic!("a durably landed file must recover without another mux"),
            );
            assert!(
                matches!(ending, Ending::Done { .. }),
                "{source_missing}/{proof}: {ending:?}"
            );
            assert_eq!(lib.landed.load(Ordering::SeqCst), 0);
            assert_eq!(crate::server::util::Stamp::read(&target).unwrap(), landed);
            let link = links::read_entries(&t.path().join("config"))
                .unwrap()
                .remove(&target)
                .unwrap();
            assert_eq!(link.iso, job.iso);
            if proof == "known" {
                assert_eq!(link.iso_stamp, job.source_stamp);
                assert_eq!(link.stamp, Some(landed));
            } else {
                assert!(link.iso_stamp.is_none());
                assert!(link.stamp.is_none());
            }
            finish(&lib, &retry, ending, Duration::ZERO, &sink);
            assert_eq!(lib.queue.snapshot().jobs[0].state, JobState::Done);
        }
    }
}
