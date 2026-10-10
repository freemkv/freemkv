use super::*;
use crate::server::library::queue;
use crate::server::planner::{MediaMetadata, PLAN_VERSION, PlannedOutput, RemuxPlan};

#[test]
fn presentation_preference_frozen_movie_and_single_tv_execute_audio_all_after_preference_change() {
    use crate::server::planner::{MediaKind, planner_for};
    for kind in [MediaKind::Movie, MediaKind::Tv] {
        let (_t, lib, dirs) = library_with(&["B"]);
        lib.queue.add(vec![NewJob {
            title: "Feature".into(),
            iso: "/i/feature.iso".into(),
            target: dirs.library.join("Feature/Feature.mkv"),
            replace: false,
        }]);
        let mut job = lib.queue.claim_next().unwrap();
        let mut cfg = Config {
            presentation_language: "de".into(),
            tv_auto: false,
            ..config(&dirs)
        };
        let plan = planner_for(
            &cfg,
            &MediaMetadata {
                kind: Some(kind),
                ..Default::default()
            },
        )
        .plan(
            &job.iso,
            &crate::selection_test_fixtures::launch_titles(),
            &MediaMetadata {
                kind: Some(kind),
                ..Default::default()
            },
            &job.target,
        )
        .unwrap();
        persist_auto_plan(&lib, &cfg, &mut job, plan).unwrap();
        // A changed preference must not reinterpret an already-frozen job.
        cfg.presentation_language = "en".into();
        maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap();
        let arbiter = Arbiter::new();
        let mut sink = test_sink(&lib, &arbiter);
        sink.job_id = job.id;
        let mut calls = 0;
        let ending = remux_planned_with(
            &job,
            job.plan.as_ref().unwrap(),
            &cfg,
            &sink,
            |request, _, _| {
                calls += 1;
                assert_eq!(request.title, Some(1));
                assert!(
                    request.streams.is_all(),
                    "presentation choice must not filter audio"
                );
                std::fs::write(&request.target, movie()).unwrap();
                Ok(Some(current_stamp()))
            },
        );
        assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
        assert_eq!(calls, 1);
    }
}

#[test]
fn planned_output_count_mismatch_is_rejected_before_execution() {
    let (_t, lib, dirs) = library_with(&["B"]);
    let job = planned(&lib, &dirs);
    let mut plan = job.plan.clone().unwrap();
    plan.outputs.pop();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with(&job, &plan, &config(&dirs), &sink, |_, _, _| {
        panic!("mismatched plan must not execute")
    });
    assert!(matches!(ending, Ending::Failed(_)));
}

fn planned(lib: &Library, dirs: &Dirs) -> Job {
    lib.queue.add(vec![NewJob {
        title: "Show".into(),
        iso: "/i/A.iso".into(),
        target: dirs.library.join("Show/Show.mkv"),
        replace: false,
    }]);
    let job = lib.queue.claim_next().unwrap();
    let plan = RemuxPlan {
        version: PLAN_VERSION,
        source_iso: job.iso.clone(),
        media: MediaMetadata::default(),
        outputs: (0..2)
            .map(|n| PlannedOutput {
                id: format!("episode-{n}"),
                title_index: n,
                episode: Some(n as u16 + 1),
                episode_name: String::new(),
                filename: format!("Show_S01E{:02}.mkv", n + 1),
            })
            .collect(),
    };
    assert!(lib.queue.set_plan(job.id, plan));
    lib.queue.snapshot().jobs[0].clone()
}

fn config(dirs: &Dirs) -> Config {
    Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    }
}

#[test]
fn tv_outputs_use_mover_names_and_tv_root_and_survive_rescan() {
    let (t, lib, dirs) = library_with(&["B"]);
    let original = planned(&lib, &dirs);
    let mut plan = original.plan.unwrap();
    plan.source_iso = dirs.isos.as_ref().unwrap().join("Show.iso");
    std::fs::write(&plan.source_iso, b"source image").unwrap();
    lib.queue.drop_job(original.id);
    plan.media = MediaMetadata {
        title: "Show".into(),
        year: 2001,
        season: Some(3),
        kind: Some(crate::server::planner::MediaKind::Tv),
        ..Default::default()
    };
    plan.outputs[0].episode_name = "First".into();
    plan.outputs[1].episode_name = "Second".into();
    let cfg = Config {
        output_dir: t.path().join("output").to_string_lossy().into_owned(),
        tv_dir: "tv".into(),
        ..config(&dirs)
    };
    let (root, targets) = tv_destinations(&cfg, &plan).unwrap();
    assert_eq!(
        targets[0],
        root.join("Show (2001)/Season 03/Show S03E01 - First.mkv")
    );
    assert_eq!(
        targets[1],
        root.join("Show (2001)/Season 03/Show S03E02 - Second.mkv")
    );
    std::fs::create_dir_all(&root).unwrap();
    lib.queue.add(vec![NewJob {
        title: "Show".into(),
        iso: plan.source_iso.clone(),
        target: original.target.clone(),
        replace: false,
    }]);
    let claimed = lib.queue.claim_next().unwrap();
    assert!(
        lib.queue
            .set_plan_targets(claimed.id, plan, Some(targets.clone()))
    );
    let job = lib.queue.snapshot().jobs[0].clone();
    for target in &targets {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(queue::partial_path(target), b"active").unwrap();
    }
    assert_eq!(lib.sweep_partials(&dirs), 0, "active TV outputs are owned");
    for target in &targets {
        assert!(queue::partial_path(target).exists());
    }
    // Reopen after the simulated worker has drained: all frozen destinations,
    // not just the original movie target, must have crash leftovers removed.
    std::fs::create_dir_all(original.target.parent().unwrap()).unwrap();
    std::fs::write(queue::partial_path(&original.target), b"legacy orphan").unwrap();
    let unrelated = root.join("unrelated.mkv.partial");
    std::fs::write(&unrelated, b"unrelated").unwrap();
    let reopened = queue::Queue::open(&t.path().join("config"));
    assert!(!queue::partial_path(&original.target).exists());
    assert!(unrelated.exists(), "reopen only removes persisted targets");
    for target in &targets {
        assert!(!queue::partial_path(target).exists());
    }
    drop(reopened);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with(&job, job.plan.as_ref().unwrap(), &cfg, &sink, |r, _, _| {
        std::fs::write(&r.target, movie()).unwrap();
        Ok(Some(current_stamp()))
    });
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    finish(&lib, &job, ending, Duration::ZERO, &sink);
    assert!(!original.target.exists());
    lib.index_now(&dirs);
    for target in &targets {
        assert!(lib.snapshot().sigs.contains_key(target));
    }
    let fresh = Library::open(&t.path().join("config"), &t.path().join("logs"));
    fresh.index_now(&dirs);
    for target in &targets {
        assert!(fresh.snapshot().sigs.contains_key(target));
        let listing = fresh.listing(&dirs);
        let row = listing
            .rows
            .iter()
            .find(|r| r.mkv.as_ref() == Some(target))
            .unwrap();
        assert!(row.linked);
        assert_eq!(row.iso.as_ref(), Some(&job.iso));
        assert!(fresh.audits.is_queued(target));
    }
    fresh.queue.drop_job(job.id);
    for target in &targets {
        std::fs::write(queue::partial_path(target), b"orphan").unwrap();
    }
    let missing_movies = Dirs {
        library: t.path().join("missing-movies"),
        ..dirs.clone()
    };
    assert_eq!(fresh.sweep_partials(&missing_movies), targets.len());
    for target in &targets {
        assert!(target.exists());
        assert!(!queue::partial_path(target).exists());
    }
    fresh.index_now(&dirs);
    for target in &targets {
        assert!(
            fresh.snapshot().sigs.contains_key(target),
            "links outlive queue history"
        );
        assert!(
            fresh
                .listing(&dirs)
                .rows
                .iter()
                .any(|r| r.mkv.as_ref() == Some(target) && r.linked)
        );
    }
    let listing = lib.listing(&dirs);
    for target in &targets {
        let row = listing
            .rows
            .iter()
            .find(|r| r.mkv.as_ref() == Some(target))
            .unwrap();
        assert!(row.linked);
        assert!(!row.needs_remux);
        assert!(row.result.is_some());
    }
    assert_eq!(
        lib.queue.add(vec![NewJob {
            title: "Show S03E02 - Second".into(),
            iso: job.iso.clone(),
            target: targets[1].clone(),
            replace: true,
        }]),
        1
    );
    let redo = lib.queue.claim_next().unwrap();
    assert_eq!(redo.plan.as_ref().unwrap().outputs.len(), 1);
    assert_eq!(redo.plan.as_ref().unwrap().outputs[0].title_index, 1);
    let mut redo_sink = test_sink(&lib, &arbiter);
    redo_sink.job_id = redo.id;
    let ending = remux_planned_with(
        &redo,
        redo.plan.as_ref().unwrap(),
        &cfg,
        &redo_sink,
        |r, _, _| {
            assert_eq!(r.target, targets[1]);
            assert_eq!(r.title, Some(1));
            std::fs::write(&r.target, movie()).unwrap();
            Ok(Some(current_stamp()))
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    finish(&lib, &redo, ending, Duration::ZERO, &redo_sink);
    let changed = Config {
        tv_dir: "elsewhere".into(),
        ..cfg
    };
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &changed,
        &sink,
        |_, _, _| panic!("must not execute"),
    );
    assert!(matches!(ending, Ending::Failed(_)));
    assert!(!t.path().join("output/elsewhere").exists());
}

#[test]
fn fanout_delivers_indexes_links_and_audits_each_output_and_protects_active_partials() {
    let (t, lib, dirs) = library_with(&["B"]);
    lib.index_now(&dirs);
    let job = planned(&lib, &dirs);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = Vec::new();
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &config(&dirs),
        &sink,
        |request, kept, _| {
            assert!(kept.is_none());
            calls.push(request.title.unwrap());
            let partial = super::super::super::queue::partial_path(&request.target);
            std::fs::write(&partial, b"in progress").unwrap();
            let orphan = request.target.parent().unwrap().join("orphan.mkv.partial");
            std::fs::write(&orphan, b"orphan").unwrap();
            assert_eq!(lib.sweep_partials(&dirs), 1);
            assert!(partial.exists(), "the active episode is owned");
            std::fs::write(&partial, movie()).unwrap();
            std::fs::rename(&partial, &request.target).unwrap();
            Ok(Some(current_stamp()))
        },
    );
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    finish(&lib, &job, ending, Duration::from_secs(2), &sink);
    assert_eq!(calls, vec![0, 1]);
    assert!(!job.target.exists());
    let links = super::super::super::links::load(&t.path().join("config"));
    for target in job.output_targets() {
        assert_eq!(std::fs::read(&target).unwrap(), movie());
        assert!(lib.snapshot().sigs.contains_key(&target));
        assert!(lib.audits.is_queued(&target));
        assert_eq!(links.get(&target), Some(&job.iso));
    }
    let snapshot = lib.queue.snapshot();
    assert!(
        snapshot.jobs[0]
            .outputs
            .iter()
            .all(|o| o.state == OutputState::Done)
    );
    assert!(
        matches!(snapshot.results.values().next().unwrap(), JobResult::Done { size_bytes, .. } if *size_bytes == 2 * movie().len() as u64)
    );
    assert!(!lib.snapshot().sigs.contains_key(&job.target));
}

#[test]
fn fanout_restart_adopts_retained_episode_once_and_real_delivery_needs_no_iso() {
    let (t, lib, dirs) = library_with(&["B"]);
    let job = planned(&lib, &dirs);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let stage = t.path().join("stage");
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &config(&dirs),
        &sink,
        |request, _, n| {
            if n == 0 {
                std::fs::write(&request.target, movie()).unwrap();
                return Ok(Some(current_stamp()));
            }
            let kept = write_kept(
                &stage,
                "Show.2",
                &request.target,
                &movie(),
                crate::server::util::epoch_secs(),
            );
            Err(kept_error(&kept, std::io::Error::other("delivery failed")))
        },
    );
    assert!(matches!(ending, Ending::Failed(_)));
    finish(&lib, &job, ending, Duration::ZERO, &sink);
    assert_eq!(lib.queue.staged_total().0, 1);
    let first = job.output_targets()[0].clone();
    let stamp = std::fs::metadata(&first).unwrap().modified().unwrap();
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
    assert_eq!(
        lib.queue.snapshot().jobs.len(),
        1,
        "no standalone episode job"
    );
    let retry = lib.queue.claim_next().unwrap();
    assert_eq!(retry.outputs[0].state, OutputState::Done);
    assert!(retry.outputs[1].staged.is_some());
    assert!(!retry.iso.exists());
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = retry.id;
    let ending = remux_planned(&retry, retry.plan.as_ref().unwrap(), &config(&dirs), &sink);
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    finish(&lib, &retry, ending, Duration::ZERO, &sink);
    assert_eq!(
        std::fs::metadata(&first).unwrap().modified().unwrap(),
        stamp
    );
    assert_eq!(lib.queue.staged_total(), (0, 0));
    assert!(
        lib.queue.snapshot().jobs[0]
            .outputs
            .iter()
            .all(|o| o.staged.is_none() && o.state == OutputState::Done)
    );
}

#[test]
fn fanout_preemption_requeues_without_repeating_completed_episode() {
    let (_t, lib, dirs) = library_with(&["B"]);
    let job = planned(&lib, &dirs);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &config(&dirs),
        &sink,
        |request, _, n| {
            assert_eq!(n, 0);
            std::fs::write(&request.target, movie()).unwrap();
            drop(arbiter.rip());
            Ok(Some(current_stamp()))
        },
    );
    assert!(
        matches!(ending, Ending::Stopped(JobNote::Preempted)),
        "{ending:?}"
    );
    finish(&lib, &job, ending, Duration::ZERO, &sink);
    let retry = lib.queue.claim_next().unwrap();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = retry.id;
    let mut called = Vec::new();
    let ending = remux_planned_with(
        &retry,
        retry.plan.as_ref().unwrap(),
        &config(&dirs),
        &sink,
        |request, _, n| {
            called.push(n);
            std::fs::write(&request.target, movie()).unwrap();
            Ok(Some(current_stamp()))
        },
    );
    assert_eq!(called, vec![1]);
    assert!(matches!(ending, Ending::Done { .. }));
}

#[test]
fn fanout_stop_preserves_retained_episode_for_retry_or_discard() {
    let (t, lib, dirs) = library_with(&["B"]);
    let job = planned(&lib, &dirs);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &config(&dirs),
        &sink,
        |request, _, _| {
            let kept = write_kept(
                &t.path().join("stage"),
                "Show.1",
                &request.target,
                &movie(),
                1,
            );
            lib.stop_all();
            Err(kept_error(&kept, libfreemkv::Error::Halted.into()))
        },
    );
    assert!(matches!(ending, Ending::Stopped(JobNote::Cancelled)));
    finish(&lib, &job, ending, Duration::ZERO, &sink);
    let saved = lib.queue.snapshot().jobs[0].clone();
    assert!(saved.staged.is_some());
    assert_eq!(saved.state, JobState::Failed);
    assert!(lib.queue.retry_staged_now(&job.target));
    let path = lib.queue.take_staged(&job.target).unwrap();
    assert!(path.exists());
    deliver::discard(&path).unwrap();
    assert_eq!(lib.queue.staged_total(), (0, 0));
    assert!(!path.exists());
}

#[test]
fn retained_legacy_job_never_acquires_a_tv_plan() {
    let (t, lib, dirs) = library_with(&["B"]);
    let mut job = job_for(&dirs.library, 7);
    job.replace = false;
    job.staged = Some(write_kept(
        &t.path().join("stage"),
        "A.1",
        &job.target,
        &movie(),
        1,
    ));
    let cfg = Config {
        tv_auto: true,
        tmdb_api_key: "must-not-be-used".into(),
        ..config(&dirs)
    };
    maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap();
    assert!(job.plan.is_none());
    let arbiter = Arbiter::new();
    assert!(matches!(
        remux(&job, &cfg, &test_sink(&lib, &arbiter)),
        Ending::Done { .. }
    ));
}

#[test]
fn planned_and_legacy_targets_both_obey_current_library_root() {
    let (t, lib, dirs) = library_with(&["B"]);
    let mut job = planned(&lib, &dirs);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let cfg = Config {
        library_dir: t.path().join("new-library").to_string_lossy().into_owned(),
        ..Config::default()
    };
    let ending = remux_planned_with(&job, job.plan.as_ref().unwrap(), &cfg, &sink, |_, _, _| {
        panic!("must not execute")
    });
    assert!(matches!(ending, Ending::Failed(_)));
    assert!(!job.target.parent().unwrap().exists());
    job.plan = None;
    for replace in [false, true] {
        job.replace = replace;
        let Ending::Failed(e) = remux(&job, &cfg, &sink) else {
            panic!("foreign target accepted")
        };
        assert!(e.to_string().contains("library folder"));
    }
}

#[cfg(unix)]
#[test]
fn planned_target_cannot_escape_through_symlink() {
    let (t, lib, dirs) = library_with(&["B"]);
    let job = planned(&lib, &dirs);
    let outside = t.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, job.target.parent().unwrap()).unwrap();
    let arbiter = Arbiter::new();
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &config(&dirs),
        &test_sink(&lib, &arbiter),
        |_, _, _| panic!("must not execute"),
    );
    assert!(matches!(ending, Ending::Failed(_)));
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
}
