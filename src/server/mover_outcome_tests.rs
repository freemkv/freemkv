// Delivery outcomes of the check-and-move pass, its skips, and the retry bookkeeping edges.
use super::*;
use crate::server::health::{Fault, Problem};
use std::path::PathBuf;
use std::time::Duration;

fn guard() -> std::sync::MutexGuard<'static, ()> {
    super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn row(key: &str) -> Option<MoverError> {
    MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}

fn mkv_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0x1A, 0x45, 0xDF, 0xA3];
    bytes.resize(len, 0x5A);
    bytes
}

// A journaled delivery of `<title>.mkv` holding `bytes` into `output/<folder>`: (config, job,
// staged source, planned destination).
fn journaled(
    tmp: &Path,
    title: &str,
    bytes: &[u8],
    folder: &str,
) -> (Config, PathBuf, PathBuf, PathBuf) {
    let staging = tmp.join("staging");
    let job = staging.join(title);
    let output = tmp.join("output");
    let folder = output.join(folder);
    for d in [&job, &folder, &tmp.join("config")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let marker = serde_json::json!({"title": title, "year": 2024, "media_type": "movie"});
    std::fs::write(job.join(".done"), marker.to_string()).unwrap();
    let src = job.join(format!("{title}.mkv"));
    std::fs::write(&src, bytes).unwrap();
    let dest = folder.join(format!("{title}.mkv"));
    super::manifest::Manifest::create(
        &job,
        marker,
        Vec::new(),
        vec![output.to_string_lossy().into_owned()],
        &[(src.clone(), dest.to_string_lossy().into_owned())],
    )
    .unwrap();
    let cfg = Config {
        staging_dir: staging.to_string_lossy().into_owned(),
        output_dir: output.to_string_lossy().into_owned(),
        autorip_dir: tmp.join("config").to_string_lossy().into_owned(),
        keep_iso: true,
        ..Config::default()
    };
    (cfg, job, src, dest)
}

// A legacy (`.done`) hand-off of `<title>.mkv` into `<tmp>/output/Movies`.
fn handed_off(staging: &Path, title: &str, marker: &str) -> PathBuf {
    let dir = staging.join(title);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".done"), marker).unwrap();
    std::fs::write(dir.join(format!("{title}.mkv")), mkv_bytes(4096)).unwrap();
    dir
}

fn movie_cfg(tmp: &Path) -> Config {
    let movies = tmp.join("output/Movies");
    std::fs::create_dir_all(&movies).unwrap();
    std::fs::create_dir_all(tmp.join("config")).unwrap();
    Config {
        staging_dir: tmp.join("staging").to_string_lossy().into_owned(),
        output_dir: tmp.join("output").to_string_lossy().into_owned(),
        movie_dir: movies.to_string_lossy().into_owned(),
        autorip_dir: tmp.join("config").to_string_lossy().into_owned(),
        ..Config::default()
    }
}

fn movie_marker(title: &str) -> String {
    serde_json::json!({"title": title, "disc_name": title, "year": 2024, "media_type": "movie"})
        .to_string()
}

#[test]
fn every_post_copy_rejection_names_what_failed() {
    let cases = [
        (
            MoveError::SizeDoesNotMatch {
                src_size: 10,
                dst_size: 4,
            },
            "src=10 bytes, dst=4 bytes",
        ),
        (MoveError::MkvBadHead, "EBML header"),
        (MoveError::MkvBadTail, "tail too short"),
        (MoveError::M2tsBadSync, "TS sync"),
        (MoveError::Unreadable("gone".into()), "unreadable: gone"),
    ];
    for (error, words) in cases {
        let text = error.to_string();
        assert!(text.contains(words), "{text}");
        let boxed: Box<dyn std::error::Error> = Box::new(error);
        assert!(!boxed.to_string().is_empty());
    }
}

#[test]
fn a_copy_cancelled_just_before_publication_leaves_no_file() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("rip.mkv");
    std::fs::write(&src, mkv_bytes(1000)).unwrap();
    let dest = dir.path().join("Rip (2024).mkv");
    let calls = std::sync::atomic::AtomicU32::new(0);
    let cancel = || calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 4;
    let written = std::sync::atomic::AtomicU64::new(0);
    let error = copy_counting_cancellable(&src, &dest, &written, &cancel).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::Interrupted, "{error}");
    assert!(!dest.exists());
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left.len(), 1, "only the source remains: {left:?}");
}

#[test]
fn a_source_that_cannot_be_read_fails_the_copy_and_leaves_no_temporary() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("a-folder.mkv");
    std::fs::create_dir_all(&folder).unwrap();
    let dest = dir.path().join("out.mkv");
    let written = std::sync::atomic::AtomicU64::new(0);
    assert!(copy_counting_cancellable(&folder, &dest, &written, &|| false).is_err());
    assert!(!dest.exists());
    let left = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(left, 1, "no temporary is left beside the destination");
}

#[test]
fn temporaries_of_a_bare_root_and_an_unremovable_one_are_tolerated() {
    assert!(sweep_temporaries(Path::new("/")).is_ok());
    let dir = tempfile::tempdir().unwrap();
    let not_a_file = dir.path().join("Title.mkv.part-1");
    std::fs::create_dir_all(&not_a_file).unwrap();
    remove_temporary(&not_a_file);
    assert!(
        not_a_file.is_dir(),
        "a folder is never removed as a temporary"
    );
}

#[test]
fn a_short_mkv_and_a_vanished_source_are_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let short = dir.path().join("short.mkv");
    std::fs::write(&short, [0x1A, 0x45]).unwrap();
    assert!(matches!(
        check_post_copy_mkv(&short),
        Err(MoveError::Unreadable(_))
    ));
    let gone = dir.path().join("gone.iso");
    match check_post_copy_size(&gone, &short) {
        Err(MoveError::Unreadable(why)) => assert!(why.contains("src stat failed"), "{why}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn copy_errors_that_are_not_failures_keep_their_outcome() {
    let exists = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    assert_eq!(copy_failure_outcome(&exists), MoveOutcome::Collision);
    let unsupported = std::io::Error::from(std::io::ErrorKind::Unsupported);
    assert_eq!(copy_failure_outcome(&unsupported), MoveOutcome::Unsupported);
}

#[test]
fn a_poisoned_config_skips_the_pass() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    let held = cfg.clone();
    let poisoned = std::thread::spawn(move || {
        let _w = held.write().unwrap();
        panic!("poison the config lock");
    })
    .join();
    assert!(poisoned.is_err());
    let mut moved = false;
    assert!(!mover_tick(&cfg, |_| moved = true));
    assert!(!moved, "no move runs without a config snapshot");
}

#[test]
fn the_mover_loop_runs_a_pass_and_drains_on_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RwLock::new(Config {
        staging_dir: tmp.path().join("no-staging").to_string_lossy().into_owned(),
        autorip_dir: tmp.path().join("config").to_string_lossy().into_owned(),
        ..Config::default()
    }));
    let _stop = crate::server::test_stop::after(2);
    run(&cfg);
    assert!(
        !tmp.path().join("no-staging").exists(),
        "nothing was created"
    );
}

#[test]
fn a_rip_filed_elsewhere_whose_folder_holds_media_is_shown_not_removed() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("job");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("keep.mkv"), b"media").unwrap();
    let key = dir.to_string_lossy().into_owned();
    finish_filed_elsewhere(&dir, "Keep");
    let left = row(&key);
    clear_error(&key);
    assert!(dir.join("keep.mkv").exists());
    let left = left.expect("the failed cleanup is shown");
    assert!(left.reason.contains("staging cleanup failed"), "{left:?}");
}

#[test]
fn a_moved_away_output_is_logged_once() {
    let tmp = tempfile::tempdir().unwrap();
    let output = || crate::server::ripper::staging::Output {
        filename: "gone.mkv".into(),
        ..Default::default()
    };
    for _ in 0..2 {
        let (kept, gone) = split_moved_away(tmp.path(), vec![output()]);
        assert!(kept.is_empty());
        assert_eq!(gone.len(), 1);
    }
}

#[test]
fn retry_bookkeeping_without_a_row_creates_none() {
    let _g = guard();
    let key = "/test-outcome-no-row/job";
    clear_error(key);
    block_move(usize::MAX);
    back_off_unjournaled(key);
    hold_for_operator(key);
    schedule_move_retry_with(key, |_| {});
    commit_retry_with(key, 1, None, |_| {});
    finish_stall(key);
    assert!(row(key).is_none());
}

#[test]
fn a_returned_worker_whose_stall_was_unsaved_stays_held() {
    let _g = guard();
    let key = "/test-outcome-stall-held/job";
    clear_error(key);
    record_error_with(key, "copy stalled", "wait", |_| {});
    if let Some(r) = MOVE_ERRORS.lock().unwrap().get_mut(key) {
        r.stall_resume = Some(None);
        r.retry_after = Some(std::time::Instant::now());
    }
    finish_stall(key);
    let left = row(key);
    clear_error(key);
    let left = left.expect("the row stays");
    assert!(left.retry_held && left.retry_after.is_none(), "{left:?}");
}

#[test]
fn an_unsaved_stall_commit_holds_the_blocked_worker() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let job = tmp.path().join("job");
    std::fs::create_dir_all(&job).unwrap();
    std::fs::write(job.join(super::manifest::FILE), b"not a journal").unwrap();
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    record_error_with(&key, "copy stalled", "wait", |_| {});
    if let Some(r) = MOVE_ERRORS.lock().unwrap().get_mut(&key) {
        r.worker_active = true;
    }
    let mut logged = String::new();
    commit_retry_with(&key, 1, Some(Duration::from_secs(30)), |line| {
        logged = line.to_string()
    });
    let left = row(&key);
    clear_error(&key);
    let left = left.expect("the row stays");
    assert_eq!(left.stall_resume, Some(None), "{left:?}");
    assert!(left.retry_held && left.retry_after.is_none(), "{left:?}");
    assert!(left.reason.contains("could not be saved"), "{left:?}");
    assert!(logged.starts_with("Move held"), "{logged}");
}

// A frozen plan in `<tmp>/staging/job` of one MKV into `<tmp>/output`.
fn frozen(tmp: &Path) -> (PathBuf, super::manifest::Manifest) {
    let job = tmp.join("staging/job");
    let output = tmp.join("output");
    std::fs::create_dir_all(&job).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    let src = job.join("T.mkv");
    std::fs::write(&src, mkv_bytes(64)).unwrap();
    let saved = super::manifest::Manifest::create(
        &job,
        serde_json::json!({"title": "T"}),
        Vec::new(),
        vec![output.to_string_lossy().into_owned()],
        &[(src, output.join("T.mkv").to_string_lossy().into_owned())],
    )
    .unwrap();
    (job, saved)
}

fn hold_row(key: &str, attempts: u32) {
    record_error_with(key, "copy failed", "retry", |_| {});
    if let Some(r) = MOVE_ERRORS.lock().unwrap().get_mut(key) {
        r.attempts = attempts;
        r.retry_after = Some(std::time::Instant::now() + Duration::from_secs(3600));
        r.retry_held = false;
    }
}

#[test]
fn a_retry_whose_journal_cannot_be_cleared_waits_and_says_why() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (job, saved) = frozen(tmp.path());
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    std::fs::write(job.join(super::manifest::FILE), b"torn").unwrap();
    RETRY_REQUESTS.lock().unwrap().insert(key.clone(), false);
    let attempt = restore_retry(&key, &saved);
    let left = row(&key);
    let pending = take_retry_request(&key);
    clear_error(&key);
    assert!(matches!(attempt, Attempt::Wait));
    assert!(!pending, "the request was taken");
    let left = left.expect("the refusal is shown");
    assert!(left.reason.contains("cannot clear"), "{left:?}");
}

#[test]
fn a_backed_off_journal_waits_with_or_without_saved_retry_state() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (job, saved) = frozen(tmp.path());
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    hold_row(&key, 1);
    let no_state = restore_retry(&key, &saved);

    super::manifest::Manifest::record_retry(
        &job,
        Some(super::manifest::RetryState {
            attempts: 1,
            not_before: Some(std::time::SystemTime::now() + Duration::from_secs(60)),
            reason: "copy failed".into(),
            hint: "retry".into(),
        }),
    )
    .unwrap();
    let saved = super::manifest::Manifest::load(&job).unwrap().unwrap();
    hold_row(&key, 2);
    let with_state = restore_retry(&key, &saved);
    let left = row(&key);
    clear_error(&key);
    assert!(matches!(no_state, Attempt::Wait));
    assert!(matches!(with_state, Attempt::Wait));
    assert_eq!(left.map(|r| r.attempts), Some(2), "the row's count stands");
}

#[test]
fn unanswering_and_other_root_faults_read_as_such() {
    let p = |fault| Problem::new("output", Path::new("/nas"), fault, "detail".into());
    let slow = root_reason("/nas", &p(Fault::Unresponsive));
    assert!(slow.contains("not responding"), "{slow}");
    let stale = root_reason("/nas", &p(Fault::Stale));
    assert!(stale.contains("not usable"), "{stale}");
}

#[test]
fn destination_edges_keep_their_names() {
    assert_eq!(dest_with_variant("/", 3), "/");
    let cfg = Config {
        output_dir: "/out".into(),
        iso_dir: "isos".into(),
        ..Config::default()
    };
    let iso = build_destination(&cfg, &None, "My Disc.iso", None);
    assert!(
        iso.starts_with("/out/isos/") && iso.ends_with(".iso"),
        "{iso}"
    );
}

#[test]
fn folders_outside_the_root_or_climbing_out_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let outside = create_below_root(root, Path::new("/elsewhere/Title"), None).unwrap_err();
    assert!(outside.to_string().contains("not under"), "{outside}");
    let climbing = create_below_root(root, &root.join("a/../b"), None).unwrap_err();
    assert!(climbing.to_string().contains("unexpected"), "{climbing}");
    assert!(!root.join("a").exists(), "what it made is removed");
}

#[test]
fn a_held_test_lock_is_reported_busy() {
    let lock = Mutex::new(());
    let _held = lock.lock().unwrap();
    assert!(!test_lock_is_free(&lock));
}

// Dirs the pass must skip without a row: leased, not handed off, torn, governed, stranded,
// nameless. The stranded one is warned once, then quietly.
#[test]
fn dirs_that_are_not_ready_are_skipped_without_a_row() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = movie_cfg(tmp.path());
    let staging = tmp.path().join("staging");
    let leased = handed_off(&staging, "Leased", &movie_marker("Leased"));
    let ripping = staging.join("Ripping");
    std::fs::create_dir_all(&ripping).unwrap();
    let schema = crate::server::ripper::staging::DISC_STATE_SCHEMA;
    std::fs::write(
        ripping.join(crate::server::ripper::staging::STATE_FILE),
        format!(r#"{{"schema":{schema},"state":"ripped"}}"#),
    )
    .unwrap();
    let torn = handed_off(&staging, "Torn", "{not json");
    let muxing = staging.join("Muxing");
    std::fs::create_dir_all(&muxing).unwrap();
    std::fs::write(muxing.join(".muxing"), b"").unwrap();
    let stranded = staging.join("Stranded");
    std::fs::create_dir_all(&stranded).unwrap();
    std::fs::write(stranded.join("notes.txt"), b"left by hand").unwrap();
    let nameless = handed_off(&staging, "Nameless", r#"{"title":"","disc_name":""}"#);

    let lease = crate::server::ripper::staging::job_lease(&leased);
    {
        let _owned = lease.lock().unwrap();
        check_and_move(&cfg);
        check_and_move(&cfg);
    }
    let dirs = [&leased, &ripping, &torn, &muxing, &stranded, &nameless];
    let rows: Vec<_> = dirs
        .iter()
        .filter_map(|d| row(&d.to_string_lossy()))
        .collect();
    for d in dirs {
        clear_error(&d.to_string_lossy());
    }
    assert!(rows.is_empty(), "{rows:?}");
    assert!(
        leased.join("Leased.mkv").exists(),
        "a leased job is not moved"
    );
    assert!(torn.join("Torn.mkv").exists());
    assert!(nameless.join("Nameless.mkv").exists());
    assert!(stranded.join("notes.txt").exists());
    let journal = super::manifest::FILE;
    assert!(!leased.join(journal).exists() && !nameless.join(journal).exists());
}

#[test]
fn a_rip_with_no_title_files_under_the_output_folder() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = movie_cfg(tmp.path());
    let staging = tmp.path().join("staging");
    let dir = handed_off(&staging, "Untitled", r#"{"disc_name":"Untitled"}"#);
    let key = dir.to_string_lossy().into_owned();
    check_and_move(&cfg);
    let left = row(&key);
    clear_error(&key);
    let filed = tmp.path().join("output/Untitled.mkv");
    assert!(
        filed.exists() || dir.join("Untitled.mkv").exists(),
        "the rip is filed or kept: {left:?}"
    );
    assert!(!tmp.path().join("output/Movies/Untitled").exists());
}

#[test]
fn an_incompletely_listed_new_rip_is_never_frozen() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let cfg = movie_cfg(tmp.path());
    let dir = handed_off(
        &tmp.path().join("staging"),
        "Partial",
        &movie_marker("Partial"),
    );
    let key = dir.to_string_lossy().into_owned();
    clear_error(&key);
    test_hooks::LISTING_FAULTS.lock().unwrap().push(dir.clone());
    check_and_move(&cfg);
    test_hooks::LISTING_FAULTS
        .lock()
        .unwrap()
        .retain(|p| *p != dir);
    let left = row(&key);
    clear_error(&key);
    assert!(!dir.join(super::manifest::FILE).exists(), "nothing frozen");
    assert!(dir.join("Partial.mkv").exists());
    let left = left.expect("the refusal is shown");
    assert!(
        left.reason.contains("incomplete staging listing"),
        "{left:?}"
    );
}

#[test]
fn a_structurally_invalid_copy_is_held_back_and_retried() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let mut junk = mkv_bytes(16 << 20);
    junk[..4].copy_from_slice(b"JUNK");
    let (cfg, job, src, dest) = journaled(tmp.path(), "Garbled", &junk, "");
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    let _hooks = test_hooks::set(tmp.path(), Duration::from_secs(60), Arc::new(|| {}));
    check_and_move(&cfg);
    drop(_hooks);
    let left = row(&key);
    clear_error(&key);
    assert!(!dest.exists(), "an invalid copy is never published");
    assert_eq!(std::fs::metadata(&src).unwrap().len(), junk.len() as u64);
    let left = left.expect("the rejection is shown");
    assert!(left.reason.contains("structurally invalid"), "{left:?}");
    assert_eq!(left.attempts, 1, "{left:?}");
}

#[test]
fn a_copy_whose_source_grew_is_a_size_mismatch() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, job, src, dest) = journaled(tmp.path(), "Grown", &mkv_bytes(8192), "");
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    let grow = src.clone();
    let _hooks = test_hooks::set(
        tmp.path(),
        Duration::from_secs(60),
        Arc::new(move || {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&grow) {
                let _ = f.write_all(b"!");
            }
        }),
    );
    check_and_move(&cfg);
    drop(_hooks);
    let left = row(&key);
    clear_error(&key);
    assert!(!dest.exists(), "a short copy is never published");
    assert!(src.exists());
    let left = left.expect("the mismatch is shown");
    assert!(left.reason.contains("size does not match"), "{left:?}");
}

#[cfg(unix)]
#[test]
fn a_destination_folder_swapped_for_a_link_holds_the_delivery() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, job, src, _dest) = journaled(tmp.path(), "Swapped", &mkv_bytes(4096), "Sub");
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    let folder = tmp.path().join("output/Sub");
    let moved = tmp.path().join("output/Sub-real");
    std::fs::rename(&folder, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, folder).unwrap();
    check_and_move(&cfg);
    let left = row(&key);
    clear_error(&key);
    assert!(src.exists(), "the staged source is kept");
    assert!(
        !moved.join("Swapped.mkv").exists(),
        "nothing lands through the link"
    );
    let left = left.expect("the hold is shown");
    assert!(
        left.reason.contains("ownership lock unavailable"),
        "{left:?}"
    );
}

#[test]
fn a_journaled_delivery_whose_root_is_gone_waits_for_it() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, job, src, _dest) = journaled(tmp.path(), "Unrooted", &mkv_bytes(4096), "");
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    std::fs::remove_dir_all(tmp.path().join("output")).unwrap();
    check_and_move(&cfg);
    let left = row(&key);
    clear_error(&key);
    assert!(src.exists());
    assert!(
        !tmp.path().join("output").exists(),
        "the root is never recreated"
    );
    let left = left.expect("the wait is shown");
    assert!(
        left.reason.contains("waiting for the output folder"),
        "{left:?}"
    );
}

// The first file of a pair is changed while the second is copied: the delivery is not torn
// down on a destination that no longer matches what it recorded.
#[test]
fn a_delivered_file_changed_before_teardown_keeps_the_staging_job() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let job = tmp.path().join("staging/Pair");
    let output = tmp.path().join("output");
    for d in [&job, &output, &tmp.path().join("config")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let marker = serde_json::json!({"title": "Pair", "year": 2024, "media_type": "movie"});
    std::fs::write(job.join(".done"), marker.to_string()).unwrap();
    let mut moves = Vec::new();
    for name in ["A", "B"] {
        let src = job.join(format!("{name}.mkv"));
        std::fs::write(&src, mkv_bytes(4096)).unwrap();
        let dest = output.join(format!("{name}.mkv"));
        moves.push((src, dest.to_string_lossy().into_owned()));
    }
    super::manifest::Manifest::create(
        &job,
        marker,
        Vec::new(),
        vec![output.to_string_lossy().into_owned()],
        &moves,
    )
    .unwrap();
    let cfg = Config {
        staging_dir: tmp.path().join("staging").to_string_lossy().into_owned(),
        output_dir: output.to_string_lossy().into_owned(),
        autorip_dir: tmp.path().join("config").to_string_lossy().into_owned(),
        ..Config::default()
    };
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    let first = output.join("A.mkv");
    let touched = first.clone();
    let _hooks = test_hooks::set(
        tmp.path(),
        Duration::from_secs(60),
        Arc::new(move || {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&touched) {
                let _ = f.write_all(b"!");
            }
        }),
    );
    check_and_move(&cfg);
    drop(_hooks);
    let left = row(&key);
    clear_error(&key);
    assert!(
        job.join("A.mkv").exists() || first.exists(),
        "nothing is lost"
    );
    assert!(job.exists(), "the staging job is kept");
    let left = left.expect("the hold is shown");
    assert!(
        left.reason.contains("cannot verify delivered ownership"),
        "{left:?}"
    );
}
