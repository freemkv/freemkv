use super::*;
use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};

fn plan_of(id: &str, filename: &str) -> RemuxPlan {
    RemuxPlan {
        media: MediaMetadata::default(),
        outputs: vec![PlannedOutput {
            id: id.into(),
            title_index: 0,
            episode: None,
            episode_name: String::new(),
            filename: filename.into(),
        }],
    }
}

fn queued(lib: &Library, dirs: &Dirs) -> Job {
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: dirs.isos.as_ref().unwrap().join("A.iso"),
        target: dirs.library.join("A/A.mkv"),
        replace: false,
    }]);
    lib.queue.claim_next().unwrap()
}

fn texts(lib: &Library) -> Vec<String> {
    lib.console_since(0).into_iter().map(|l| l.text).collect()
}

fn count(lib: &Library, kind: LineKind) -> usize {
    lib.console_since(0)
        .iter()
        .filter(|l| l.kind == kind)
        .count()
}

#[test]
fn a_job_whose_planning_fails_is_logged_and_left_failed() {
    let (_t, lib, dirs) = library_with(&[]);
    let job = queued(&lib, &dirs);
    let cfg = Config {
        tv_auto: false,
        tmdb_api_key: String::new(),
        ..Config::default()
    };
    let arbiter = Arbiter::new();
    run_job(&lib, &cfg, &arbiter, arbiter.epoch(), job.clone());
    assert!(lib.running().is_none());
    let j = lib.queue.snapshot().jobs[0].clone();
    assert_eq!(j.state, JobState::Failed, "{j:?}");
    let lines = texts(&lib);
    assert!(
        lines.iter().any(|l| l.starts_with("$ freemkv iso://")),
        "{lines:?}"
    );
    assert!(lines.contains(&opening(&format!("iso://{}", job.iso.display()))));
    let log = std::fs::read_to_string(lib.log_path(&job.title)).unwrap();
    assert!(log.contains("freemkv iso://"), "{log}");
}

#[test]
fn a_planned_job_a_rip_preempts_goes_back_to_the_head_of_the_queue() {
    let (_t, lib, dirs) = library_with(&[]);
    let job = queued(&lib, &dirs);
    assert!(freeze_fixture_plan(&lib, job.id, plan_of("main", "A.mkv"), None).unwrap());
    let job = lib.queue.snapshot().jobs[0].clone();
    let arbiter = Arbiter::new();
    let slot = arbiter.rip();
    run_job(&lib, &Config::default(), &arbiter, arbiter.epoch(), job);
    drop(slot);
    assert!(lib.running().is_none());
    let j = lib.queue.snapshot().jobs[0].clone();
    assert_eq!(
        (j.state, j.note),
        (JobState::Queued, Some(JobNote::Preempted))
    );
    assert!(
        texts(&lib)
            .iter()
            .any(|l| l.contains("a rip needs the mux slot"))
    );
}

#[test]
fn every_folder_fault_maps_to_its_io_error_kind() {
    use std::io::ErrorKind as K;
    for (fault, kind) in [
        (Fault::Stale, K::StaleNetworkFileHandle),
        (Fault::ReadOnly, K::ReadOnlyFilesystem),
        (Fault::Missing, K::NotFound),
        (Fault::Denied, K::PermissionDenied),
        (Fault::Unresponsive, K::TimedOut),
        (Fault::Unmounted, K::TimedOut),
        (Fault::Io, K::Other),
        (Fault::Other, K::Other),
    ] {
        let p = Problem::new("output", Path::new("/nas/movies"), fault, "detail".into());
        let e = problem_error(&p);
        assert_eq!(e.kind(), kind, "{fault:?}");
        assert!(e.to_string().contains("/nas/movies"), "{e}");
    }
}

#[test]
fn an_unplanned_job_console_names_its_single_target() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = job_for(dir.path(), 1);
    job.retire = false;
    let line = command_line(&job);
    assert!(
        line.ends_with(&format!("mkv://{}", job.target.display())),
        "{line}"
    );
    let kept = dir.path().join("stage/A.0123456789ab.staged.mkv");
    let line = kept_line(&job, &kept);
    assert!(line.ends_with(&*job.target.to_string_lossy()), "{line}");
    job.plan = Some(RemuxPlan {
        media: MediaMetadata::default(),
        outputs: Vec::new(),
    });
    let line = command_line(&job);
    assert!(!line.contains("mkv://"), "{line}");
    let line = kept_line(&job, &kept);
    assert!(line.ends_with(&*kept.to_string_lossy()), "{line}");
}

#[test]
fn a_kept_file_without_its_output_and_several_targets_names_no_destination() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = job_for(dir.path(), 1);
    job.plan = Some(plan_of("e1", "E1.mkv"));
    job.outputs = (1..=2)
        .map(|e| super::super::super::queue::OutputProgress {
            id: format!("e{e}"),
            target: dir.path().join(format!("tv/E{e}.mkv")),
            state: OutputState::Pending,
            staged: None,
            landing: None,
            source_unproven: false,
        })
        .collect();
    let kept = dir.path().join("stage/other.staged.mkv");
    let line = kept_line(&job, &kept);
    assert!(line.ends_with(&*kept.to_string_lossy()), "{line}");
}

#[test]
fn a_timeout_names_its_phase_only_when_the_phase_is_known() {
    for phase in ["open", "mux", "sync", "verify", "copy", "replace", "target"] {
        assert!(phase_words(phase).is_some(), "{phase}");
        let e = std::io::Error::other(format!("E{}: {phase}", libfreemkv::error::E_TIMED_OUT));
        assert_eq!(phase_of(&e).as_deref(), Some(phase));
    }
    assert!(phase_words("elsewhere").is_none());
    let e = std::io::Error::other(format!("E{}: elsewhere", libfreemkv::error::E_TIMED_OUT));
    assert_eq!(phase_of(&e), None);
    assert_eq!(phase_of(&std::io::Error::other("E7013: mux")), None);
}

#[test]
fn engine_log_lines_show_by_level_and_debug_setting() {
    let (_t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    for level in [
        Level::Trace,
        Level::Debug,
        Level::Info,
        Level::Warn,
        Level::Error,
    ] {
        sink.log(level, "quiet");
    }
    assert_eq!(count(&lib, LineKind::Debug), 0);
    assert_eq!(count(&lib, LineKind::Warn), 1);
    assert_eq!(count(&lib, LineKind::Err), 1);
    sink.debug = true;
    sink.log(Level::Debug, "debug");
    sink.log(Level::Trace, "trace");
    sink.log(Level::Info, "info");
    assert_eq!(count(&lib, LineKind::Debug), 3);
    assert!(sink.last_activity.load(Ordering::Relaxed) > 0);
}

#[test]
fn progress_and_title_events_update_the_running_row_and_the_console() {
    let (_t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.debug = true;
    lib.set_running(|r| {
        *r = Some(Running {
            job_id: 7,
            phase: "start".into(),
            ..Default::default()
        })
    });
    sink.event(&Event::Phase { name: "mux" });
    assert_eq!(lib.running().unwrap().phase, "mux");
    sink.event(&Event::TitleStart {
        idx: 0,
        dest: "mkv:///m/A.mkv",
    });
    sink.progress(&Progress {
        bytes_done: 50,
        bytes_total: 100,
        speed_bps: 10,
        eta_secs: Some(5),
        ..Default::default()
    });
    let r = lib.running().unwrap();
    assert_eq!(r.pct, Some(50.0));
    assert_eq!((r.bytes_done, r.bytes_total, r.speed_bps), (50, 100, 10));
    assert!(!r.line.is_empty());
    let progress = r.line.clone();
    for completed in [true, false] {
        let outcome = libfreemkv::MuxOutcome {
            halted: !completed,
            completed,
            output_opened: true,
            bytes_written: 1 << 20,
            errors: 0,
            lost_bytes: 0,
            streams: 1,
            undelivered_streams: Vec::new(),
        };
        sink.event(&Event::TitleDone {
            idx: 0,
            dest: "mkv:///m/A.mkv",
            result: Ok(&outcome),
        });
    }
    assert!(lib.running().unwrap().line.is_empty());
    assert!(texts(&lib).contains(&progress));
    assert_eq!(count(&lib, LineKind::Ok), 1);
    assert_eq!(count(&lib, LineKind::Warn), 1);
    let path = Path::new("/m/A.mkv");
    for ok in [true, false] {
        sink.event(&Event::Verify {
            path,
            ok,
            runtime_secs: Some(60.0),
            expected_secs: 60.0,
        });
    }
    assert_eq!(count(&lib, LineKind::Err), 1);
    sink.event(&Event::Replaced { path });
    assert!(texts(&lib).contains(&"Replaced /m/A.mkv".to_string()));
    let before = lib.console_since(0).len();
    sink.event(&Event::TitleSkipped {
        idx: 1,
        empty: true,
    });
    assert_eq!(lib.console_since(0).len(), before);
}

#[test]
fn a_second_title_start_without_its_title_prints_the_opening_once() {
    let (_t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    for idx in [0, 1] {
        sink.event(&Event::TitleStart {
            idx,
            dest: "mkv:///m/A.mkv",
        });
    }
    let opened = format!("{}{}", opening("iso:///i/A.iso"), ok());
    let n = texts(&lib).iter().filter(|l| **l == opened).count();
    assert_eq!(n, 1);
}

#[test]
fn closing_an_unopened_transcript_leaves_its_opening_and_last_progress() {
    let (_t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    sink.progress(&Progress {
        bytes_done: 1,
        bytes_total: 100,
        ..Default::default()
    });
    sink.close_open_line();
    let lines = texts(&lib);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], opening("iso:///i/A.iso"));
    assert!(!lines[1].is_empty());
    sink.close_open_line();
    assert_eq!(texts(&lib).len(), 3, "the progress line is printed once");
}

#[test]
fn a_landing_over_a_folder_is_refused_and_a_gone_target_needs_its_partial() {
    let (t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let stamp = crate::server::util::Stamp {
        size: 1,
        modified: std::time::SystemTime::UNIX_EPOCH,
    };
    let folder = t.path().join("folder.mkv");
    std::fs::create_dir_all(&folder).unwrap();
    sink.expect_landing(&folder, "main", Some(stamp), None);
    sink.event(&Event::Phase { name: "replace" });
    assert_eq!(sink.landing_refused().as_deref(), Some(folder.as_path()));
    assert!(sink.should_cancel());

    let gone = t.path().join("gone.mkv");
    sink.expect_landing(&gone, "main", Some(stamp), None);
    sink.event(&Event::Phase { name: "replace" });
    assert!(sink.landing_refused().is_none());
    assert!(sink.landing_unsaved().is_some(), "no partial to record");
    assert!(sink.landed_stamp().is_none());
    assert!(sink.should_cancel());
}

#[test]
fn a_kept_file_made_for_another_target_is_not_copied_in() {
    let (t, lib, dirs) = library_with(&[]);
    let target = dirs.library.join("A/A.mkv");
    let kept = write_kept(&t.path().join("stage"), "A.1", &target, &movie(), 1);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let other = dirs.library.join("B/B.mkv");
    let e = resume_kept(&kept, &other, &sink).unwrap_err();
    assert!(e.to_string().contains("destination differs"), "{e}");
    assert!(kept.exists());
    assert!(!other.exists());
}

#[test]
fn a_frozen_plan_that_no_longer_matches_its_job_fails_before_any_scan() {
    let (_t, lib, dirs) = library_with(&[]);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    let cfg = Config::default();
    let no_scan = |_: &freemkv_engine::RemuxJob,
                   _: Option<&Path>,
                   _: usize|
     -> std::io::Result<Option<String>> { panic!("nothing may run") };
    let failed = |ending: Ending, what: &str| match ending {
        Ending::Failed(e) => assert!(e.to_string().contains(what), "{e}"),
        other => panic!("{other:?}"),
    };

    let t = tempfile::tempdir().unwrap();
    let stray = job_for(t.path(), 99);
    let bad = plan_of("main", "a/b.mkv");
    failed(
        remux_fixture_with(&stray, &bad, &cfg, &sink, no_scan),
        "invalid remux output plan",
    );
    let good = plan_of("main", "A.mkv");
    failed(
        remux_fixture_with(&stray, &good, &cfg, &sink, no_scan),
        "no longer exists",
    );

    let job = queued(&lib, &dirs);
    sink.job_id = job.id;
    failed(
        remux_fixture_with(&job, &good, &cfg, &sink, no_scan),
        "progress count does not match",
    );
    assert!(freeze_fixture_plan(&lib, job.id, good, None).unwrap());
    let job = lib.queue.snapshot().jobs[0].clone();
    failed(
        remux_fixture_with(&job, &plan_of("other", "A.mkv"), &cfg, &sink, no_scan),
        "progress does not match its plan",
    );
}

#[test]
fn a_stall_with_an_unhealthy_folder_is_retried_after_the_backoff() {
    let _g = crate::server::health::tests::LAST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let a = lib.queue.claim_next().unwrap();
    std::fs::rename(&dirs.library, t.path().join("unmounted")).unwrap();
    let lines = Lines(Mutex::new(Vec::new()));
    finish(
        &lib,
        &a,
        Ending::Stopped(JobNote::Stalled),
        Duration::ZERO,
        &lines,
    );
    let q = lib.queue.snapshot();
    let j = &q.jobs[0];
    assert_eq!((j.state, j.attempts), (JobState::Queued, 1), "{j:?}");
    assert!(!q.results.contains_key(&*a.target.to_string_lossy()));
    let msg = &j.failure.as_ref().unwrap().message;
    assert!(msg.contains("stalled"), "{msg}");
}

#[test]
fn a_stop_whose_kept_file_is_gone_just_says_stopped() {
    let (t, lib, dirs) = library_with(&[]);
    let job = queued(&lib, &dirs);
    lib.queue.set_staged(
        job.id,
        Some(StagedFile {
            path: t.path().join("stage/A.0123456789ab.staged.mkv"),
            bytes: 1,
            attempts: 1,
        }),
    );
    let job = lib.queue.snapshot().jobs[0].clone();
    assert!(job.staged.is_some());
    let lines = Lines(Mutex::new(Vec::new()));
    finish(
        &lib,
        &job,
        Ending::Stopped(JobNote::Cancelled),
        Duration::ZERO,
        &lines,
    );
    let j = lib.queue.snapshot().jobs[0].clone();
    assert_eq!(j.state, JobState::Failed);
    assert_eq!(j.failure.unwrap().message, "Stopped.");
    assert!(j.staged.is_none());
}
