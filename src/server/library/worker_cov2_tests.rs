// The worker loops, planning refusals, frozen-plan checks and the job ending's edges.
use super::*;
use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};
use std::path::PathBuf;

fn output(id: &str, title_index: usize, filename: &str) -> PlannedOutput {
    PlannedOutput {
        id: id.into(),
        title_index,
        episode: None,
        episode_name: String::new(),
        filename: filename.into(),
    }
}

fn plan_of(outputs: Vec<PlannedOutput>) -> RemuxPlan {
    RemuxPlan {
        media: MediaMetadata::default(),
        outputs,
    }
}

fn queued(lib: &Library, iso: PathBuf, target: PathBuf) -> Job {
    lib.queue.add(vec![NewJob {
        title: "B".into(),
        iso,
        target,
        replace: false,
    }]);
    lib.queue.claim_next().unwrap()
}

// A job for `<library>/B/B.mkv` frozen to `plan` (and `targets`), as the queue now holds it.
fn frozen(
    lib: &Library,
    dirs: &Dirs,
    plan: &RemuxPlan,
    targets: Option<Vec<PathBuf>>,
    target: PathBuf,
) -> Job {
    let iso = dirs.isos.as_ref().unwrap().join("B.iso");
    let job = queued(lib, iso, target);
    assert!(freeze_fixture_plan(lib, job.id, plan.clone(), targets).unwrap());
    lib.queue
        .snapshot()
        .jobs
        .into_iter()
        .find(|j| j.id == job.id)
        .unwrap()
}

fn library_cfg(dirs: &Dirs) -> Config {
    Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        tv_auto: false,
        tmdb_api_key: String::new(),
        ..Config::default()
    }
}

fn never(
    _: &freemkv_engine::RemuxJob,
    _: Option<&Path>,
    _: usize,
) -> std::io::Result<Option<String>> {
    panic!("nothing may run")
}

fn failed_with(ending: Ending, words: &str) -> std::io::Error {
    match ending {
        Ending::Failed(e) => {
            assert!(e.to_string().contains(words), "{e}");
            e
        }
        other => panic!("{other:?}"),
    }
}

fn console(lib: &Library) -> Vec<String> {
    lib.console_since(0).into_iter().map(|l| l.text).collect()
}

fn health_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::server::health::tests::LAST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[test]
fn two_outputs_frozen_onto_one_file_fail_before_any_write() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let target = dirs.library.join("B/B.mkv");
    let plan = plan_of(vec![output("e1", 0, "E1.mkv"), output("e2", 1, "E2.mkv")]);
    let targets = vec![target.clone(), target.clone()];
    let job = frozen(&lib, &dirs, &plan, Some(targets), target.clone());
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(&job, &plan, &library_cfg(&dirs), &sink, never);
    failed_with(ending, "duplicate remux output target");
    assert!(!target.exists());
}

#[test]
fn an_output_kept_without_source_proof_is_not_muxed_again() {
    let (t, lib, dirs) = library_with(&["A"]);
    let target = dirs.library.join("B/B.mkv");
    let plan = plan_of(vec![output("main", 0, "B.mkv")]);
    let job = frozen(&lib, &dirs, &plan, None, target.clone());
    let staged = StagedFile {
        path: t.path().join("stage/B.0123456789ab.staged.mkv"),
        bytes: 1,
        attempts: 1,
    };
    lib.queue
        .retain_unproven_output(job.id, "main", staged)
        .unwrap();
    lib.queue.set_output_staged(job.id, "main", None);
    let job = lib.queue.snapshot().jobs[0].clone();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(&job, &plan, &library_cfg(&dirs), &sink, never);
    failed_with(ending, "cannot prove");
    assert!(!target.exists());
}

#[test]
fn a_frozen_output_under_a_missing_tv_folder_fails_on_its_check() {
    let _g = health_lock();
    let (t, lib, dirs) = library_with(&["A"]);
    let cfg = Config {
        output_dir: t.path().to_string_lossy().into_owned(),
        tv_dir: "movies/TV".into(),
        ..library_cfg(&dirs)
    };
    let target = dirs.library.join("TV/Show/E1.mkv");
    let plan = plan_of(vec![output("main", 0, "E1.mkv")]);
    let job = frozen(
        &lib,
        &dirs,
        &plan,
        Some(vec![target.clone()]),
        target.clone(),
    );
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let e = failed_with(remux_fixture_with(&job, &plan, &cfg, &sink, never), "");
    assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}");
    assert!(!target.exists());
}

#[test]
fn the_running_row_names_the_output_being_made() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let target = dirs.library.join("B/B.mkv");
    let plan = plan_of(vec![output("main", 0, "B.mkv")]);
    let job = frozen(&lib, &dirs, &plan, None, target.clone());
    lib.set_running(|r| {
        *r = Some(Running {
            job_id: job.id,
            ..Default::default()
        })
    });
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = 0;
    let _ = remux_fixture_with(&job, &plan, &library_cfg(&dirs), &sink, |_, _, _| {
        calls += 1;
        Ok(None)
    });
    assert_eq!(calls, 1);
    assert_eq!(lib.running().map(|r| r.target), Some(target));
}

#[test]
fn a_stop_on_this_thread_ends_the_job_as_interrupted() {
    let (_t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let stop = crate::server::test_stop::after(0);
    let ending = cancelled_ending(&sink);
    drop(stop);
    assert!(
        matches!(ending, Some(Ending::Stopped(JobNote::Interrupted))),
        "{ending:?}"
    );
    assert!(cancelled_ending(&sink).is_none());
}

#[test]
fn a_plan_that_cannot_be_proven_or_frozen_is_refused() {
    let (t, lib, dirs) = library_with(&[]);
    let iso = t.path().join("isos/B.iso");
    std::fs::create_dir_all(iso.parent().unwrap()).unwrap();
    std::fs::write(&iso, b"synthetic ISO source").unwrap();
    let mut job = queued(&lib, iso.clone(), dirs.library.join("B/B.mkv"));
    let stamp = crate::server::library::links::iso_stamp(&iso).unwrap();
    let plan = plan_of(vec![output("main", 0, "B.mkv")]);
    let e = persist_plan(&lib, &mut job, plan.clone(), None, stamp, &[]).unwrap_err();
    assert!(e.to_string().contains("cannot prove"), "{e}");
    let mut stray = job.clone();
    stray.id = 999;
    let e = persist_plan(&lib, &mut stray, plan, None, stamp, &fixture_titles(1)).unwrap_err();
    assert!(e.to_string().contains("job changed"), "{e}");
    assert!(lib.queue.snapshot().jobs[0].plan.is_none());
}

#[test]
fn a_saved_match_without_a_media_type_plans_nothing() {
    let (t, lib, dirs) = library_with(&[]);
    let mut job = queued(&lib, t.path().join("B.iso"), dirs.library.join("B/B.mkv"));
    let stamp = crate::server::util::Stamp {
        size: 1,
        modified: std::time::SystemTime::UNIX_EPOCH,
    };
    let media = MediaMetadata::default();
    let e = attach_corrected_plan_with_scan(
        &lib,
        &Config::default(),
        &mut job,
        (&media, None),
        None,
        stamp,
        |_| panic!("no scan without a media type"),
    )
    .unwrap_err();
    assert!(e.to_string().contains("no media type"), "{e}");
    let Err(e) = corrected_plan(&Config::default(), &[], &media) else {
        panic!("planned without a media type");
    };
    assert!(e.to_string().contains("no media type"), "{e}");
}

// A real disc image in a fresh library: (temp dir, library, its claimed job).
fn imaged(name: &str) -> (tempfile::TempDir, Library, Job) {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    let iso = t.path().join(format!("{name}.iso"));
    std::fs::write(&iso, &crate::ku_fixture::bd_image().img.image).unwrap();
    let target = t.path().join(format!("movies/{name}/{name}.mkv"));
    let job = queued(&lib, iso, target);
    (t, lib, job)
}

#[test]
fn an_automatic_movie_plan_scans_the_image_and_writes_no_media() {
    let (t, lib, mut job) = imaged("Film");
    let cfg = Config {
        output_dir: t.path().join("output").to_string_lossy().into_owned(),
        ..Config::default()
    };
    match attach_automatic_movie_plan(&lib, &cfg, &mut job) {
        Ok(()) => assert!(job.plan.is_some()),
        Err(e) => assert!(!e.to_string().is_empty()),
    }
    assert!(!job.target.exists(), "planning writes no media");
}

#[test]
fn an_episode_row_plans_only_its_own_output_or_says_why_not() {
    let (t, lib, mut job) = imaged("Show");
    crate::server::library::links::record_frozen_output(
        t.path(),
        &job.target,
        &job.iso,
        None,
        true,
        None,
    )
    .unwrap();
    let cfg = Config {
        output_dir: t.path().join("output").to_string_lossy().into_owned(),
        tv_auto: true,
        tmdb_api_key: String::new(),
        ..Config::default()
    };
    let found = tv_metadata(&MediaMetadata {
        title: "Show".into(),
        year: 2001,
        tmdb_id: 7,
        kind: Some(MediaKind::Tv),
        ..Default::default()
    });
    match attach_from_lookup(&lib, &cfg, &mut job, Ok(Some(found))) {
        Ok(()) => assert_eq!(job.output_targets().len(), 1, "{job:?}"),
        Err(e) => assert!(!e.to_string().is_empty()),
    }
    assert!(!job.target.exists(), "planning writes no media");
}

#[test]
fn a_job_that_only_copies_in_its_kept_file_says_so() {
    let (t, lib, dirs) = library_with(&[]);
    let job = queued(
        &lib,
        t.path().join("isos/A.iso"),
        dirs.library.join("A/A.mkv"),
    );
    let kept = t.path().join("stage/A.0123456789ab.staged.mkv");
    lib.queue.set_staged(
        job.id,
        Some(StagedFile {
            path: kept.clone(),
            bytes: 1,
            attempts: 1,
        }),
    );
    let job = lib.queue.snapshot().jobs[0].clone();
    let arbiter = Arbiter::new();
    run_job(&lib, &library_cfg(&dirs), &arbiter, arbiter.epoch(), job);
    assert!(lib.running().is_none());
    let lines = console(&lib);
    let kept = kept.to_string_lossy();
    assert!(lines.iter().any(|l| l.contains(&*kept)), "{lines:?}");
}

#[test]
fn the_startup_sweep_runs_when_the_queue_loads() {
    let (t, lib, _d) = library_with(&[]);
    let stage = t.path().join("stage");
    std::fs::create_dir_all(&stage).unwrap();
    let now = crate::server::util::epoch_secs();
    assert!(sweep_staging_once(
        &lib,
        &Config::default(),
        Some(&stage),
        now
    ));
    assert!(lib.queue.snapshot().jobs.is_empty());
}

#[test]
fn an_unreadable_iso_folder_is_the_problem_named() {
    let _g = health_lock();
    let (t, lib, _d) = library_with(&[]);
    let out = t.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let mut job = job_for(t.path(), 1);
    job.target = out.join("A.mkv");
    job.iso = t.path().join("nowhere/A.iso");
    let p = folder_problem(&lib, &job).expect("the ISO folder fails its check");
    assert!(p.path.ends_with("nowhere"), "{p}");
}

#[test]
fn a_storage_fault_past_its_retries_gives_up_and_names_the_folder() {
    let _g = health_lock();
    let (t, lib, _d) = library_with(&[]);
    let mut job = queued(
        &lib,
        t.path().join("isos/A.iso"),
        t.path().join("gone/A/A.mkv"),
    );
    job.attempts = 40;
    job.first_failed_at = Some(1);
    let lines = Lines(Mutex::new(Vec::new()));
    let quiet = std::io::Error::new(std::io::ErrorKind::TimedOut, "the share went quiet");
    finish(&lib, &job, Ending::Failed(quiet), Duration::ZERO, &lines);
    let out = lines.0.lock().unwrap().clone();
    assert!(
        out.iter().any(|l| l.starts_with("Giving up after 40")),
        "{out:?}"
    );
    let j = lib.queue.snapshot().jobs[0].clone();
    assert_eq!(j.state, JobState::Failed, "{j:?}");
}

#[test]
fn small_refusals_name_their_cause() {
    let t = tempfile::tempdir().unwrap();
    let blocker = t.path().join("file.txt");
    std::fs::write(&blocker, b"a file, not a folder").unwrap();
    assert!(refuse_unowned(&blocker.join("x.mkv"), &[]).is_err());
    let root = t.path().join("lib");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("file.txt"), b"x").unwrap();
    let why = safe_destination(&root, &root.join("file.txt/x.mkv"), false).unwrap_err();
    assert!(why.contains("cannot resolve"), "{why}");
    clean_stale_staging(&t.path().join("missing"));
    let log = JobLog::create(&blocker.join("logs/A.log"));
    assert!(log.0.lock().unwrap().is_none());
    let raw = "E65000: odd";
    assert_eq!(error_text(&std::io::Error::other(raw)), raw);
}

#[test]
fn a_change_match_with_every_file_in_place_says_to_run_it_again() {
    let t = tempfile::tempdir().unwrap();
    let mut job = job_for(t.path(), 1);
    job.retire = true;
    job.outputs = vec![super::super::super::queue::OutputProgress {
        id: "main".into(),
        target: t.path().join("A/A.mkv"),
        state: OutputState::Done,
        staged: None,
        landing: None,
        source_unproven: false,
    }];
    let done = safe_text(&job);
    job.outputs[0].state = OutputState::Pending;
    let pending = safe_text(&job);
    assert!(!done.is_empty());
    assert_ne!(done, pending);
}

#[test]
fn a_linked_file_that_cannot_be_read_fails_a_job_that_needs_it() {
    let (t, _lib, dirs) = library_with(&["A"]);
    let config = t.path().join("config");
    let mkv = dirs.library.join("A/A.mkv");
    let iso = dirs.isos.as_ref().unwrap().join("A.iso");
    crate::server::library::links::record(&config, &mkv, &iso).unwrap();
    std::fs::remove_dir_all(dirs.library.join("A")).unwrap();
    std::fs::write(dirs.library.join("A"), b"not a folder").unwrap();
    let mut job = job_for(t.path(), 1);
    job.iso = iso;
    job.target = mkv;
    job.replace = false;
    assert!(capture_owned(&job, &library_cfg(&dirs), &config).is_err());
}

#[test]
fn audit_lanes_skip_files_they_cannot_or_need_not_read() {
    let (t, lib, dirs) = library_with(&["A"]);
    let missing = dirs.library.join("missing.mkv");
    quick_one(&lib, &missing, &dirs.library, false);
    let a = dirs.library.join("A/A.mkv");
    let no_ffmpeg = t.path().join("no-ffmpeg");
    deep_one(&lib, &a, &dirs.library, &|| false, None);
    deep_one(&lib, &a, &dirs.library, &|| false, Some(no_ffmpeg.clone()));
    quick_one(&lib, &a, &dirs.library, true);
    lib.audits.start_deep(&a, "A".into());
    lib.audits.stop_all();
    deep_one(&lib, &a, &dirs.library, &|| false, Some(no_ffmpeg));
    assert!(lib.audits.status().running.is_none());
    let sig = super::super::super::probe::FileSig::stat(&a).unwrap();
    assert!(
        lib.audits.report(&a, sig).is_some(),
        "the quick read stands"
    );
}

#[cfg(unix)]
#[test]
fn a_decode_reports_progress_and_a_stopped_one_is_not_requeued() {
    let (t, lib, dirs) = library_with(&["A", "B"]);
    let (a, b) = (dirs.library.join("A/A.mkv"), dirs.library.join("B/B.mkv"));
    let (talk, slow) = (t.path().join("talk"), t.path().join("slow"));
    std::fs::create_dir_all(&talk).unwrap();
    std::fs::create_dir_all(&slow).unwrap();
    quick_one(&lib, &a, &dirs.library, true);
    quick_one(&lib, &b, &dirs.library, true);
    while lib.audits.next_deep().is_some() {}
    let talking = fake_ffmpeg(&talk, "echo out_time_us=2000000\nexit 0");
    deep_one(&lib, &a, &dirs.library, &|| false, Some(talking));
    let sleeping = fake_ffmpeg(&slow, "exec sleep 30");
    let stop = || {
        lib.audits.stop_all();
        true
    };
    deep_one(&lib, &b, &dirs.library, &stop, Some(sleeping));
    assert!(lib.audits.next_deep().is_none(), "a stopped decode waits");
    assert!(lib.audits.status().running.is_none());
}

#[test]
fn the_watchdog_marks_the_running_row_and_warns_once_after_cancelling() {
    let (_t, lib, _d) = library_with(&[]);
    lib.set_running(|r| {
        *r = Some(Running {
            job_id: 7,
            ..Default::default()
        })
    });
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| watchdog(&lib, 7, &sink, &done));
        std::thread::sleep(Duration::from_millis(1700));
        done.store(true, Ordering::SeqCst);
    });
    assert!(lib.running().is_some_and(|r| r.stalled_secs >= 60));
    let lines = console(&lib);
    let warned = lines
        .iter()
        .filter(|l| l.starts_with("No progress for"))
        .count();
    assert!(warned >= 1, "the stall is reported: {lines:?}");
}

#[test]
fn closing_an_opened_transcript_adds_no_opening_line() {
    let (_t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    sink.event(&Event::TitleStart {
        idx: 0,
        dest: "mkv:///m/A.mkv",
    });
    let before = console(&lib).len();
    sink.close_open_line();
    assert_eq!(console(&lib).len(), before);
}

#[test]
fn the_remux_loop_waits_runs_a_job_and_drains_on_stop() {
    let (t, lib, dirs) = library_with(&["A"]);
    let lib = Arc::new(lib);
    let cfg = Arc::new(RwLock::new(library_cfg(&dirs)));
    let arbiter = Arbiter::new();
    {
        let _stop = crate::server::test_stop::after(1);
        run(&lib, &cfg, &arbiter);
    }
    lib.queue.add(vec![NewJob {
        title: "B".into(),
        iso: t.path().join("isos/B.iso"),
        target: dirs.library.join("B/B.mkv"),
        replace: false,
    }]);
    {
        let _stop = crate::server::test_stop::after(1);
        run(&lib, &cfg, &arbiter);
    }
    assert!(lib.running().is_none());
    assert!(!dirs.library.join("B/B.mkv").exists());
}

#[test]
fn the_index_loop_sweeps_partials_indexes_and_drains_on_stop() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let partial = dirs.library.join("A/A.mkv.partial");
    std::fs::write(&partial, b"left by a killed remux").unwrap();
    let lib = Arc::new(lib);
    let cfg = Arc::new(RwLock::new(library_cfg(&dirs)));
    {
        let _stop = crate::server::test_stop::after(2);
        index_loop(&lib, &cfg);
    }
    assert!(
        !partial.exists(),
        "an orphaned partial is removed at startup"
    );
}

#[test]
fn the_audit_loop_turns_deep_audit_on_and_reads_a_file() {
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    let lib = Arc::new(lib);
    let cfg = Arc::new(RwLock::new(Config {
        deep_audit: true,
        ..library_cfg(&dirs)
    }));
    {
        let _stop = crate::server::test_stop::after(2);
        audit_loop(&lib, &cfg, &Arbiter::new());
    }
    assert!(lib.deep_enabled());
    let a = dirs.library.join("A/A.mkv");
    let sig = super::super::super::probe::FileSig::stat(&a).unwrap();
    assert!(lib.audits.report(&a, sig).is_some());
}

#[test]
fn the_deep_lane_idles_while_deep_audit_is_off() {
    let (_t, lib, dirs) = library_with(&[]);
    let lib = Arc::new(lib);
    let cfg = Arc::new(RwLock::new(library_cfg(&dirs)));
    {
        let _stop = crate::server::test_stop::after(1);
        deep_audit_loop(&lib, &cfg, &Arbiter::new());
    }
    assert!(lib.audits.status().running.is_none());
}

#[test]
fn a_library_emptied_during_the_title_scan_is_not_written_into() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let target = dirs.library.join("B/B.mkv");
    let plan = plan_of(vec![output("main", 0, "B.mkv")]);
    let job = frozen(&lib, &dirs, &plan, None, target.clone());
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let library = dirs.library.clone();
    let scan = |_: &Path| -> std::io::Result<Vec<libfreemkv::DiscTitle>> {
        std::fs::remove_dir_all(library.join("A"))?;
        Ok(fixture_titles(1))
    };
    let ending = remux_planned_with_scan(&job, &plan, &library_cfg(&dirs), &sink, scan, never);
    failed_with(ending, "");
    assert!(!target.exists());
}

#[cfg(unix)]
#[test]
fn an_output_folder_that_is_a_dangling_link_is_not_created_through() {
    let (t, lib, dirs) = library_with(&["A"]);
    let target = dirs.library.join("B/B.mkv");
    let plan = plan_of(vec![output("main", 0, "B.mkv")]);
    let job = frozen(&lib, &dirs, &plan, None, target.clone());
    let nowhere = t.path().join("nowhere");
    std::os::unix::fs::symlink(&nowhere, dirs.library.join("B")).unwrap();
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let ending = remux_fixture_with(&job, &plan, &library_cfg(&dirs), &sink, never);
    failed_with(ending, "");
    assert!(!nowhere.exists(), "nothing is made through the link");
    assert!(!target.exists());
}
