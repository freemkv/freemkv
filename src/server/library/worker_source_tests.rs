use super::*;
use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};

fn frozen(lib: &Library, dirs: &Dirs, count: usize) -> Job {
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: dirs.isos.as_ref().unwrap().join("A.iso"),
        target: dirs.library.join("A/new.mkv"),
        replace: false,
    }]);
    let job = lib.queue.claim_next().unwrap();
    let plan = RemuxPlan {
        media: MediaMetadata::default(),
        outputs: (0..count)
            .map(|n| PlannedOutput {
                id: format!("output-{n}"),
                title_index: n,
                episode: None,
                episode_name: String::new(),
                filename: format!("new-{n}.mkv"),
            })
            .collect(),
    };
    assert!(lib.queue.set_plan(job.id, plan));
    lib.queue.snapshot().jobs[0].clone()
}

fn cfg(dirs: &Dirs) -> Config {
    Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

#[test]
fn frozen_source_legacy_pending_plan_cannot_acquire_proof_by_retry() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let job = frozen(&lib, &dirs, 1);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = 0;
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |_, _, _| {
            calls += 1;
            Ok(None)
        },
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    assert_eq!(calls, 0, "unknown historic source must not run new mux");
}

#[test]
fn frozen_source_legacy_staged_delivery_does_not_assert_source_ownership() {
    let (t, lib, dirs) = library_with(&["A"]);
    let job = frozen(&lib, &dirs, 1);
    let staged = t.path().join("kept.mkv");
    std::fs::write(&staged, b"verified old artifact").unwrap();
    lib.queue
        .set_output_staged(job.id, "output-0", Some(staged));
    let job = lib.queue.snapshot().jobs[0].clone();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |r, kept, _| {
            assert!(kept.is_some());
            std::fs::write(&r.target, b"verified old artifact")?;
            // A real delivery records its verified landing before rename.
            lib.queue.set_output_landing(
                job.id,
                "output-0",
                crate::server::util::Stamp::read(&r.target)?,
            )?;
            sink.landing.lock().unwrap().as_mut().unwrap().recorded =
                Some(crate::server::util::Stamp::read(&r.target)?);
            Ok(None)
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    let link = super::super::super::links::entries(&t.path().join("config"))
        .remove(&job.target)
        .unwrap();
    assert!(
        link.stamp.is_none(),
        "unknown provenance cannot prove ownership"
    );
    assert!(
        link.iso_stamp.is_none(),
        "must not sample current ISO for old staged media"
    );
}

fn titles(count: usize) -> Vec<libfreemkv::DiscTitle> {
    (0..count)
        .map(|n| {
            let mut title = libfreemkv::DiscTitle::empty();
            title.playlist = format!("title-{n}");
            title.playlist_id = n as u16;
            title.extents = vec![libfreemkv::disc::Extent {
                start_lba: n as u32 * 100,
                sector_count: 50,
            }];
            title
        })
        .collect()
}

fn proven(lib: &Library, dirs: &Dirs, count: usize, retire: bool) -> Job {
    let item = NewJob {
        title: "A".into(),
        iso: dirs.isos.as_ref().unwrap().join("A.iso"),
        target: dirs.library.join("A/new.mkv"),
        replace: false,
    };
    if retire {
        lib.queue.add_fresh(vec![item], true);
    } else {
        lib.queue.add(vec![item]);
    }
    let mut job = lib.queue.claim_next().unwrap();
    let plan = RemuxPlan {
        media: MediaMetadata::default(),
        outputs: (0..count)
            .map(|n| PlannedOutput {
                id: format!("output-{n}"),
                title_index: n,
                episode: None,
                episode_name: String::new(),
                filename: format!("new-{n}.mkv"),
            })
            .collect(),
    };
    let stamp = planning_source_stamp(&job.iso).unwrap();
    persist_plan(lib, &mut job, plan, None, stamp, &titles(count)).unwrap();
    job
}

fn run(
    lib: &Library,
    dirs: &Dirs,
    job: &Job,
    scan: impl FnOnce(&Path) -> std::io::Result<Vec<libfreemkv::DiscTitle>>,
    execute: impl FnMut(
        &freemkv_engine::RemuxJob,
        Option<&Path>,
        usize,
    ) -> std::io::Result<Option<String>>,
) -> Ending {
    let arbiter = Arbiter::new();
    let mut sink = test_sink(lib, &arbiter);
    sink.job_id = job.id;
    remux_planned_with_scan(
        job,
        job.plan.as_ref().unwrap(),
        &cfg(dirs),
        &sink,
        scan,
        execute,
    )
}

#[test]
fn frozen_source_replaced_after_restart_blocks_saved_owned_mux_and_retirement() {
    let (t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 2, true);
    let old = dirs.library.join("A/A.mkv");
    super::super::super::links::record(&t.path().join("config"), &old, &job.iso).unwrap();
    let owned = capture_owned(&job, &cfg(&dirs), &t.path().join("config")).unwrap();
    assert!(!owned.is_empty());
    lib.queue.set_owned(job.id, owned).unwrap();
    lib.queue
        .finish_output(job.id, "output-0", OutputState::Done);
    let before = std::fs::read(&old).unwrap();
    drop(lib);
    std::fs::write(&job.iso, b"replacement source image").unwrap();
    let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
    let job = lib.queue.claim_next().unwrap();
    let ending = run(
        &lib,
        &dirs,
        &job,
        |_| panic!("mismatched source must not be scanned"),
        |_, _, _| panic!("must not mux replacement"),
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    assert_eq!(std::fs::read(&old).unwrap(), before);
    assert!(!job.output_targets()[1].exists());
    assert_eq!(lib.queue.snapshot().jobs[0].source_stamp, job.source_stamp);
}

#[test]
fn frozen_source_unique_identity_rebinds_indices_without_changing_names() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 2, false);
    let mut rescanned = titles(2);
    rescanned.reverse();
    let mut actual = Vec::new();
    let ending = run(
        &lib,
        &dirs,
        &job,
        |_| Ok(rescanned),
        |r, _, _| {
            actual.push((r.title, r.target.clone()));
            std::fs::write(&r.target, b"new output")?;
            Ok(None)
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(
        actual,
        vec![
            (Some(1), job.output_targets()[0].clone()),
            (Some(0), job.output_targets()[1].clone())
        ]
    );
    assert_eq!(lib.queue.snapshot().jobs[0].plan, job.plan);
}

#[test]
fn frozen_source_missing_or_ambiguous_later_identity_stops_all_fresh_writes() {
    for duplicate in [false, true] {
        let (_t, lib, dirs) = library_with(&["A"]);
        let job = proven(&lib, &dirs, 2, false);
        let mut rescanned = titles(if duplicate { 2 } else { 1 });
        if duplicate {
            rescanned.push(rescanned[1].clone());
        }
        let ending = run(
            &lib,
            &dirs,
            &job,
            |_| Ok(rescanned),
            |_, _, _| panic!("validate all identities before any mux"),
        );
        assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
        assert!(job.output_targets().iter().all(|p| !p.exists()));
    }
}

#[test]
fn frozen_source_replacement_during_rescan_cannot_start_media() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 1, false);
    let ending = run(
        &lib,
        &dirs,
        &job,
        |iso| {
            std::fs::write(iso, b"another image while scan was open")?;
            Ok(titles(1))
        },
        |_, _, _| panic!("post-scan source proof must match"),
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
}

#[test]
fn frozen_source_replacement_between_outputs_stops_the_next_mux() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 2, false);
    let mut calls = 0;
    let ending = run(
        &lib,
        &dirs,
        &job,
        |_| Ok(titles(2)),
        |r, _, _| {
            calls += 1;
            std::fs::write(&r.target, b"first complete output")?;
            std::fs::write(&job.iso, b"replacement after first output")?;
            Ok(None)
        },
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    assert_eq!(calls, 1);
    assert!(!job.output_targets()[1].exists());
}

#[test]
fn frozen_source_replacement_before_retirement_keeps_old_files() {
    let (t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 1, true);
    let old = dirs.library.join("A/A.mkv");
    super::super::super::links::record(&t.path().join("config"), &old, &job.iso).unwrap();
    let before = std::fs::read(&old).unwrap();
    let ending = run(
        &lib,
        &dirs,
        &job,
        |_| Ok(titles(1)),
        |r, _, _| {
            std::fs::write(&r.target, b"complete new output")?;
            std::fs::write(&job.iso, b"replacement just before retirement")?;
            Ok(None)
        },
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    assert_eq!(std::fs::read(&old).unwrap(), before);
}

#[test]
fn frozen_source_retained_artifact_delivers_with_original_provenance_and_pending_mux_held() {
    for absent in [false, true] {
        let (t, lib, dirs) = library_with(&["A"]);
        let job = proven(&lib, &dirs, 2, true);
        let old = dirs.library.join("A/A.mkv");
        super::super::super::links::record(&t.path().join("config"), &old, &job.iso).unwrap();
        let old_bytes = std::fs::read(&old).unwrap();
        let kept = t.path().join("kept.mkv");
        std::fs::write(&kept, b"verified staged media").unwrap();
        lib.queue.set_output_staged(job.id, "output-0", Some(kept));
        if absent {
            std::fs::remove_file(&job.iso).unwrap();
        } else {
            std::fs::write(&job.iso, b"replacement ISO").unwrap();
        }
        let job = lib.queue.snapshot().jobs[0].clone();
        let arbiter = Arbiter::new();
        let mut sink = test_sink(&lib, &arbiter);
        sink.job_id = job.id;
        let mut delivered = 0;
        let ending = remux_planned_with_scan(
            &job,
            job.plan.as_ref().unwrap(),
            &cfg(&dirs),
            &sink,
            |_| panic!("replacement must not be scanned"),
            |r, staged, _| {
                assert!(staged.is_some(), "only retained media may run");
                delivered += 1;
                std::fs::write(&r.target, b"verified staged media")?;
                sink.landing.lock().unwrap().as_mut().unwrap().recorded =
                    Some(crate::server::util::Stamp::read(&r.target)?);
                Ok(None)
            },
        );
        assert!(
            matches!(ending, Ending::Failed(_)),
            "pending mux held: {ending:?}"
        );
        assert_eq!(delivered, 1);
        let link = super::super::super::links::entries(&t.path().join("config"))
            .remove(&job.output_targets()[0])
            .unwrap();
        assert_eq!(link.iso_stamp, job.source_stamp);
        assert!(link.stamp.is_some());
        assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
        let queue = lib.queue.snapshot();
        assert_eq!(queue.jobs[0].outputs[0].state, OutputState::Done);
        assert_eq!(queue.jobs[0].outputs[1].state, OutputState::Pending);
        if !absent {
            assert!(
                capture_owned(&job, &cfg(&dirs), &t.path().join("config"))
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

#[test]
fn frozen_source_job_local_landing_recovers_after_source_disappears() {
    let (t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 1, false);
    std::fs::write(&job.target, b"landed verified media").unwrap();
    lib.queue
        .set_output_landing(
            job.id,
            "output-0",
            crate::server::util::Stamp::read(&job.target).unwrap(),
        )
        .unwrap();
    std::fs::remove_file(&job.iso).unwrap();
    let job = lib.queue.snapshot().jobs[0].clone();
    let ending = run(
        &lib,
        &dirs,
        &job,
        |_| panic!("no media scan for completed landing"),
        |_, _, _| panic!("no second mux"),
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    let link = super::super::super::links::entries(&t.path().join("config"))
        .remove(&job.target)
        .unwrap();
    assert_eq!(link.iso_stamp, job.source_stamp);
    assert!(link.stamp.is_some());
}

#[test]
fn frozen_source_changed_during_mux_refuses_the_before_landing_boundary() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 1, false);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with_scan(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |_| Ok(titles(1)),
        |r, _, _| {
            let partial = super::super::super::queue::partial_path(&r.target);
            std::fs::write(&partial, b"new artifact")?;
            std::fs::write(&job.iso, b"replacement source during mux")?;
            sink.event(&Event::Phase { name: "replace" });
            assert!(sink.should_cancel());
            if sink.should_cancel() {
                return Err(libfreemkv::Error::Halted.into());
            }
            std::fs::rename(partial, &r.target)?;
            Ok(None)
        },
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    assert!(!job.target.exists());
}

#[test]
fn frozen_source_planning_replacement_cannot_persist_old_choices_under_new_proof() {
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: dirs.isos.as_ref().unwrap().join("A.iso"),
        target: dirs.library.join("new.mkv"),
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    let before_scan = planning_source_stamp(&job.iso).unwrap();
    let plan = RemuxPlan {
        media: MediaMetadata::default(),
        outputs: vec![PlannedOutput {
            id: "main".into(),
            title_index: 0,
            episode: None,
            episode_name: String::new(),
            filename: "new.mkv".into(),
        }],
    };
    std::fs::write(&job.iso, b"replacement during planning or metadata lookup").unwrap();
    assert!(persist_plan(&lib, &mut job, plan, None, before_scan, &titles(1)).is_err());
    let saved = lib.queue.snapshot();
    assert!(saved.jobs[0].plan.is_none());
    assert!(saved.jobs[0].source_stamp.is_none());
    assert!(saved.jobs[0].outputs.is_empty());
}

#[test]
fn frozen_source_same_size_new_mtime_is_still_a_different_source() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 1, false);
    let original = job.source_stamp.unwrap();
    std::fs::write(&job.iso, b"new").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&job.iso)
        .unwrap()
        .set_modified(original.modified + Duration::from_secs(2))
        .unwrap();
    assert_eq!(planning_source_stamp(&job.iso).unwrap().size, original.size);
    let ending = run(
        &lib,
        &dirs,
        &job,
        |_| panic!("different source must not be scanned"),
        |_, _, _| panic!("must not mux replacement"),
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
}

#[test]
fn frozen_source_missing_legacy_title_proof_stays_unknown_after_reopen() {
    let (t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 1, false);
    let path = lib.queue.path().to_path_buf();
    drop(lib);
    let mut wire: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    wire["jobs"][0]
        .as_object_mut()
        .unwrap()
        .remove("title_identities");
    std::fs::write(&path, serde_json::to_vec(&wire).unwrap()).unwrap();
    let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
    let reopened = lib.queue.claim_next().unwrap();
    assert_eq!(reopened.source_stamp, job.source_stamp);
    assert!(reopened.title_identities.is_empty());
    let ending = run(
        &lib,
        &dirs,
        &reopened,
        |_| Ok(titles(1)),
        |_, _, _| panic!("rescan cannot invent historic title proof"),
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    assert!(lib.queue.snapshot().jobs[0].title_identities.is_empty());
}

#[test]
fn frozen_source_failed_plan_write_never_exposes_plan_proof_or_starts_media() {
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: dirs.isos.as_ref().unwrap().join("A.iso"),
        target: dirs.library.join("new.mkv"),
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    let stamp = planning_source_stamp(&job.iso).unwrap();
    let plan = RemuxPlan {
        media: MediaMetadata::default(),
        outputs: vec![PlannedOutput {
            id: "main".into(),
            title_index: 0,
            episode: None,
            episode_name: String::new(),
            filename: "new.mkv".into(),
        }],
    };
    let queue_path = lib.queue.path();
    std::fs::remove_file(queue_path).unwrap();
    std::fs::create_dir(queue_path).unwrap();
    let result = persist_plan(&lib, &mut job, plan, None, stamp, &titles(1));
    assert!(result.is_err());
    let saved = lib.queue.snapshot();
    assert!(saved.jobs[0].plan.is_none());
    assert!(saved.jobs[0].source_stamp.is_none());
    assert!(saved.jobs[0].title_identities.is_empty());
    assert!(saved.jobs[0].outputs.is_empty());
    assert!(matches!(
        after_tv_planning(result, || panic!("durability failure must stop before mux")),
        Ending::Failed(_)
    ));
}

#[test]
fn frozen_source_replaced_after_match_validation_is_refused_before_corrected_scan() {
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: dirs.isos.as_ref().unwrap().join("A.iso"),
        target: dirs.library.join("new.mkv"),
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    let matched_source = planning_source_stamp(&job.iso).unwrap();
    std::fs::write(&job.iso, b"replacement after saved-match validation").unwrap();
    let media = MediaMetadata {
        kind: Some(MediaKind::Movie),
        title: "Saved match A".into(),
        ..Default::default()
    };
    let result = attach_corrected_plan_with_scan(
        &lib,
        &cfg(&dirs),
        &mut job,
        &media,
        None,
        matched_source,
        |_| panic!("saved match for A must not scan B"),
    );
    assert!(result.is_err());
    assert!(lib.queue.snapshot().jobs[0].plan.is_none());
    assert!(lib.queue.snapshot().jobs[0].source_stamp.is_none());
}

fn unproven(output: &super::super::super::queue::OutputProgress) -> bool {
    serde_json::to_value(output).unwrap()["source_unproven"]
        .as_bool()
        .unwrap_or(false)
}

fn rewrite_kept_iso(kept: &Path, iso: &Path) {
    let sidecar = kept.with_extension("json");
    let mut wire: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&sidecar).unwrap()).unwrap();
    wire["iso"] = serde_json::to_value(iso).unwrap();
    std::fs::write(sidecar, serde_json::to_vec(&wire).unwrap()).unwrap();
}

#[test]
fn frozen_source_only_newly_uncertain_artifact_loses_ownership_and_retirement_after_restart() {
    let (t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 2, true);
    let old = dirs.library.join("A/A.mkv");
    super::super::super::links::record(&t.path().join("config"), &old, &job.iso).unwrap();
    let old_bytes = std::fs::read(&old).unwrap();
    let stage = t.path().join("stage");
    let known = write_kept(
        &stage,
        "precompleted",
        &job.output_targets()[0],
        &movie(),
        crate::server::util::epoch_secs(),
    );
    rewrite_kept_iso(&known, &job.iso);
    lib.queue
        .set_output_staged(job.id, "output-0", Some(known.clone()));
    lib.queue.set_staged(job.id, still_kept(Some(&known)));
    let job = lib.queue.snapshot().jobs[0].clone();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with_scan(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |_| Ok(titles(2)),
        |r, kept, ordinal| {
            if let Some(kept) = kept {
                return resume_kept(kept, &r.target, &sink);
            }
            execute_staged(r, &sink, &stage, ordinal, |staged, capture| {
                let report = super::landing::engine_into_stage(staged, capture)?;
                std::fs::write(&job.iso, b"replacement while second artifact completes")?;
                Ok(report)
            })
        },
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    let saved = lib.queue.snapshot();
    assert!(!unproven(&saved.jobs[0].outputs[0]));
    assert!(unproven(&saved.jobs[0].outputs[1]));
    assert!(saved.jobs[0].outputs[1].staged.is_some());
    let links = super::super::super::links::entries(&t.path().join("config"));
    assert_eq!(links[&job.output_targets()[0]].iso_stamp, job.source_stamp);
    assert!(links[&job.output_targets()[0]].stamp.is_some());
    drop(sink);
    drop(lib);
    let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
    housekeep_staging(
        &lib,
        &stage,
        &StagingLimits {
            max_age: Duration::from_secs(3600),
            max_bytes: u64::MAX,
        },
        &dirs.library,
    );
    let retry = lib.queue.claim_next().unwrap();
    assert!(!unproven(&retry.outputs[0]));
    assert!(unproven(&retry.outputs[1]));
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = retry.id;
    let ending = remux_planned(&retry, retry.plan.as_ref().unwrap(), &cfg(&dirs), &sink);
    assert!(
        matches!(ending, Ending::Failed(_)),
        "uncertain output cannot retire: {ending:?}"
    );
    let link = super::super::super::links::entries(&t.path().join("config"))
        .remove(&retry.output_targets()[1])
        .unwrap();
    assert!(link.stamp.is_none());
    assert!(link.iso_stamp.is_none());
    assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
    // Restoring A must not upgrade the uncertain artifact or permit retirement.
    std::fs::write(&job.iso, b"iso").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&job.iso)
        .unwrap()
        .set_modified(job.source_stamp.unwrap().modified)
        .unwrap();
    let retry = lib.queue.snapshot().jobs[0].clone();
    assert!(matches!(
        remux_planned(&retry, retry.plan.as_ref().unwrap(), &cfg(&dirs), &sink),
        Ending::Failed(_)
    ));
    assert_eq!(std::fs::read(&old).unwrap(), old_bytes);
}

#[test]
fn frozen_source_failed_uncertainty_registration_preserves_orphan_and_adoption_demotes_it() {
    let (t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 1, false);
    let stage = t.path().join("stage");
    let queue_path = lib.queue.path().to_path_buf();
    let durable_before = std::fs::read(&queue_path).unwrap();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with_scan(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg(&dirs),
        &sink,
        |_| Ok(titles(1)),
        |r, _, ordinal| {
            execute_staged(r, &sink, &stage, ordinal, |staged, capture| {
                let report = super::landing::engine_into_stage(staged, capture)?;
                std::fs::write(&job.iso, b"replacement during mux")?;
                std::fs::remove_file(&queue_path)?;
                std::fs::create_dir(&queue_path)?;
                Ok(report)
            })
        },
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    let kept = deliver::pending(&stage);
    assert_eq!(kept.len(), 1);
    assert!(
        lib.queue.snapshot().jobs[0].outputs[0].staged.is_none(),
        "failed atomic registration must not publish association with old proof"
    );
    std::fs::remove_dir(&queue_path).unwrap();
    std::fs::write(&queue_path, durable_before).unwrap();
    drop(sink);
    drop(lib);
    let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
    housekeep_staging(
        &lib,
        &stage,
        &StagingLimits {
            max_age: Duration::from_secs(3600),
            max_bytes: u64::MAX,
        },
        &dirs.library,
    );
    let retry = lib.queue.claim_next().unwrap();
    assert!(
        unproven(&retry.outputs[0]),
        "unassociated orphan cannot inherit source authority"
    );
    assert_eq!(retry.source_stamp, job.source_stamp);
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = retry.id;
    let ending = remux_planned(&retry, retry.plan.as_ref().unwrap(), &cfg(&dirs), &sink);
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    let link = super::super::super::links::entries(&t.path().join("config"))
        .remove(&job.target)
        .unwrap();
    assert!(link.stamp.is_none());
    assert!(link.iso_stamp.is_none());
}

#[test]
fn frozen_source_delivery_error_before_landing_still_demotes_a_new_uncertain_artifact() {
    let (t, lib, dirs) = library_with(&["A"]);
    let job = proven(&lib, &dirs, 1, false);
    let stage = t.path().join("stage");
    let ending = run(
        &lib,
        &dirs,
        &job,
        |_| Ok(titles(1)),
        |r, _, _| {
            let kept = write_kept(
                &stage,
                "new",
                &r.target,
                &movie(),
                crate::server::util::epoch_secs(),
            );
            rewrite_kept_iso(&kept, &job.iso);
            std::fs::write(&job.iso, b"replacement before a storage error during copy")?;
            Err(kept_error(
                &kept,
                std::io::Error::other("delivery failed before replace phase"),
            ))
        },
    );
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    let saved = lib.queue.snapshot();
    assert!(unproven(&saved.jobs[0].outputs[0]));
    assert!(saved.jobs[0].outputs[0].staged.is_some());
}

#[test]
fn frozen_source_legacy_review_names_the_existing_clear_finished_recovery() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let legacy = frozen(&lib, &dirs, 1);
    let ending = run(
        &lib,
        &dirs,
        &legacy,
        |_| panic!("legacy proof must not be invented"),
        |_, _, _| panic!("legacy plan held"),
    );
    let Ending::Failed(error) = &ending else {
        panic!("{ending:?}");
    };
    assert!(error.to_string().contains("Clear finished"), "{error}");
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    finish(&lib, &legacy, ending, Duration::ZERO, &sink);
    assert_eq!(lib.queue.clear_finished(), 1);
    std::fs::write(&legacy.iso, b"the current replacement image").unwrap();
    assert_eq!(
        lib.queue.add(vec![NewJob {
            title: "Current".into(),
            iso: legacy.iso.clone(),
            target: legacy.target.clone(),
            replace: false
        }]),
        1
    );
    let mut fresh = lib.queue.claim_next().unwrap();
    assert_ne!(fresh.id, legacy.id);
    assert!(fresh.plan.is_none());
    assert!(fresh.source_stamp.is_none());
    let stamp = planning_source_stamp(&fresh.iso).unwrap();
    let plan = RemuxPlan {
        media: MediaMetadata::default(),
        outputs: vec![PlannedOutput {
            id: "fresh-current-title".into(),
            title_index: 1,
            episode: None,
            episode_name: String::new(),
            filename: "new.mkv".into(),
        }],
    };
    persist_plan(&lib, &mut fresh, plan, None, stamp, &titles(2)).unwrap();
    let ending = run(
        &lib,
        &dirs,
        &fresh,
        |_| Ok(titles(2)),
        |r, _, _| {
            assert_eq!(r.title, Some(1));
            std::fs::write(&r.target, b"fresh current-source output")?;
            Ok(None)
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(fresh.source_stamp, Some(stamp));
}
