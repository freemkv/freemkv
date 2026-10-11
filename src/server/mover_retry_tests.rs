// Retry state, operator Retry and the error rows' lifetime (audit round 2).
use super::*;
use std::collections::HashSet;
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

fn requested(key: &str) -> bool {
    RETRY_REQUESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(key)
}

// Retry, Retry all and the mover's prune run on different threads at once; none of them may
// wait forever for a lock another holds.
#[test]
fn retry_clicks_and_the_mover_prune_never_deadlock() {
    let _g = guard();
    let root = "/test-retry-deadlock";
    let key = format!("{root}/job");
    let (tx, rx) = std::sync::mpsc::channel();
    for which in 0..3 {
        let (key, tx) = (key.clone(), tx.clone());
        std::thread::spawn(move || {
            for _ in 0..20_000 {
                match which {
                    0 => clear_all_move_errors(),
                    1 => prune_move_errors(root, &HashSet::from([key.clone()])),
                    _ => {
                        record_error_with(&key, "copy failed", "retry", |_| {});
                        clear_move_error(&key);
                    }
                }
            }
            let _ = tx.send(());
        });
    }
    for _ in 0..3 {
        rx.recv_timeout(Duration::from_secs(60))
            .expect("Retry, Retry all and the prune deadlocked");
    }
    clear_error(&key);
    prune_move_errors(root, &HashSet::new());
}

// A Retry posted for a path with no row (any LAN client can post one) leaves nothing behind,
// and a request no staging dir can consume is pruned.
#[test]
fn a_retry_request_needs_a_row_and_is_pruned_with_it() {
    let _g = guard();
    let unknown = "/lan/never-a-row-4b1d";
    clear_move_error(unknown);
    assert!(!requested(unknown), "a Retry without a row is not kept");

    let dest = "/library/Movies/Kept (2020)/Kept (2020).mkv";
    record_error_with(dest, "publication failed", "fix it", |_| {});
    clear_move_error(dest);
    prune_move_errors("/staging-prune-test", &HashSet::new());
    assert!(
        !requested(dest),
        "a request for a non-staging key is pruned"
    );
}

// A destination-keyed row belongs to the delivery that raised it: once that staging dir is
// gone, the row goes with it.
#[test]
fn a_destination_row_is_pruned_with_its_staging_dir() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let job = staging.join("job");
    let library = tmp.path().join("library");
    for d in [&job, &library] {
        std::fs::create_dir_all(d).unwrap();
    }
    let src = job.join("a.mkv");
    std::fs::write(&src, b"bytes").unwrap();
    let dest = library.join("a.mkv");
    let outcome = move_file(
        &src,
        &dest,
        &|_, _, _, _| {},
        std::sync::Arc::new(|_| Err(std::io::Error::other("checkpoint refused"))),
        &job,
    );
    assert_eq!(outcome, MoveOutcome::Failed);
    let key = dest.to_string_lossy().into_owned();
    assert!(row(&key).is_some(), "fixture: the destination row exists");

    let root = staging.to_string_lossy().into_owned();
    prune_move_errors(&root, &HashSet::new());
    let left = row(&key);
    clear_error(&key);
    assert!(left.is_none(), "the abandoned delivery's row is pruned");
}

// A journaled delivery of one MKV in `tmp`: (config, staging job dir, planned destination).
fn journaled(tmp: &Path, title: &str) -> (Config, std::path::PathBuf, std::path::PathBuf) {
    let (cfg, job, mut dests) = journaled_files(tmp, title, &["mkv"]);
    (cfg, job, dests.remove(0))
}

// A journaled delivery of `title.<ext>` for each of `exts`, straight into the output root.
fn journaled_files(
    tmp: &Path,
    title: &str,
    exts: &[&str],
) -> (Config, std::path::PathBuf, Vec<std::path::PathBuf>) {
    let staging = tmp.join("staging");
    let job = staging.join(title);
    let output = tmp.join("output");
    for d in [&job, &output, &tmp.join("config")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let marker = serde_json::json!({"title": title, "year": 2024, "media_type": "movie"});
    std::fs::write(job.join(".done"), marker.to_string()).unwrap();
    let mut moves = Vec::new();
    let mut dests = Vec::new();
    for ext in exts {
        let mut bytes = vec![0x1A, 0x45, 0xDF, 0xA3];
        bytes.extend_from_slice(&[0x5Au8; 2048]);
        let src = job.join(format!("{title}.{ext}"));
        std::fs::write(&src, &bytes).unwrap();
        let dest = output.join(format!("{title}.{ext}"));
        moves.push((src, dest.to_string_lossy().into_owned()));
        dests.push(dest);
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
        staging_dir: staging.to_string_lossy().into_owned(),
        output_dir: output.to_string_lossy().into_owned(),
        autorip_dir: tmp.join("config").to_string_lossy().into_owned(),
        keep_iso: true,
        ..Config::default()
    };
    (cfg, job, dests)
}

#[cfg(unix)]
fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

// The journal cannot be saved (staging full or read-only): its retry state goes stale, so
// re-deriving the count from it would never reach the cap. The in-memory count stands, and
// a failure whose budget cannot be recorded holds the delivery instead of retrying.
#[cfg(unix)]
#[test]
fn an_unsaved_retry_state_holds_instead_of_copying_forever() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return;
    }
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, job, dest) = journaled(tmp.path(), "Unsaved");
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    super::manifest::Manifest::record_retry(
        &job,
        Some(super::manifest::RetryState {
            attempts: 1,
            not_before: Some(std::time::SystemTime::now() - Duration::from_secs(1)),
            reason: "copy to destination failed".into(),
            hint: "retry".into(),
        }),
    )
    .unwrap();
    std::fs::set_permissions(&job, std::fs::Permissions::from_mode(0o555)).unwrap();
    check_and_move(&cfg);
    let first = row(&key);
    // The backoff ran out; the journal still says one attempt.
    if let Some(r) = MOVE_ERRORS.lock().unwrap().get_mut(&key) {
        r.retry_after = Some(std::time::Instant::now());
        r.retry_held = false;
    }
    check_and_move(&cfg);
    let second = row(&key);
    std::fs::set_permissions(&job, std::fs::Permissions::from_mode(0o755)).unwrap();
    clear_error(&key);
    let first = first.expect("a row");
    assert!(!dest.exists());
    assert!(
        first.retry_held && first.reason.contains("could not be saved"),
        "{first:?}"
    );
    let second = second.expect("a row");
    assert_eq!(
        second.attempts, 3,
        "the journal's stale count never wins: {second:?}"
    );
    assert!(second.retry_held, "{second:?}");
}

// A journaled backoff that has run out is due on the next pass.
#[test]
fn an_expired_journaled_backoff_delivers() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, job, dest) = journaled(tmp.path(), "Expired");
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    super::manifest::Manifest::record_retry(
        &job,
        Some(super::manifest::RetryState {
            attempts: 1,
            not_before: Some(std::time::SystemTime::now() - Duration::from_secs(1)),
            reason: "copy to destination failed".into(),
            hint: "retry".into(),
        }),
    )
    .unwrap();
    check_and_move(&cfg);
    let left = row(&key);
    clear_error(&key);
    assert!(dest.exists(), "delivered: {left:?}");
}

// A journal's not_before is staging data: an absurd value (corruption, a clock that was far
// ahead) neither panics the mover nor postpones the retry past the longest real backoff.
#[test]
fn a_restored_backoff_is_clamped_to_the_longest_real_one() {
    let far = std::time::UNIX_EPOCH + Duration::from_secs(i64::MAX as u64 - 1000);
    let year = std::time::SystemTime::now() + Duration::from_secs(365 * 24 * 3600);
    for not_before in [far, year] {
        let mut r = MoverError {
            path: "clamp".into(),
            reason: String::new(),
            hint: String::new(),
            attempts: 0,
            retry_held: false,
            waiting_for_folder: false,
            worker_active: false,
            retrying: false,
            waiting_root: None,
            copy_stalled: false,
            retry_after: None,
            owner: None,
            missing_folder: false,
        };
        let state = super::manifest::RetryState {
            attempts: 2,
            not_before: Some(not_before),
            reason: String::new(),
            hint: String::new(),
        };
        let now = std::time::Instant::now();
        let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply_retry(&mut r, &state);
        }));
        assert!(applied.is_ok(), "{not_before:?} panicked the mover");
        let after = r.retry_after.expect("a backoff, not a hold");
        assert!(
            after <= now + Duration::from_secs(121),
            "{not_before:?} postponed the retry by {:?}",
            after - now
        );
    }
}

// Disc 1's journal froze `Twin.mkv` but has not published it yet (held, backing off or cut
// off by a restart). Disc 2 must not freeze the same name: it is filed as `_2`.
#[test]
fn a_destination_frozen_by_another_delivery_is_taken() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, first, frozen) = journaled(tmp.path(), "Twin");
    let first_key = first.to_string_lossy().into_owned();
    record_error_with(&first_key, "copy to destination failed", "retry", |_| {});
    hold_for_operator(&first_key);
    let second = tmp.path().join("staging/Twin disc 2");
    std::fs::create_dir_all(&second).unwrap();
    let marker = serde_json::json!({"title": "Twin", "year": 2024, "media_type": "movie"});
    std::fs::write(second.join(".done"), marker.to_string()).unwrap();
    std::fs::write(
        second.join("Twin.mkv"),
        std::fs::read(first.join("Twin.mkv")).unwrap(),
    )
    .unwrap();

    check_and_move(&cfg);
    let second_row = row(&second.to_string_lossy());
    clear_error(&first_key);
    clear_error(&second.to_string_lossy());
    assert!(
        !frozen.exists(),
        "disc 1's frozen name is left to disc 1: {second_row:?}"
    );
    assert!(
        frozen.with_file_name("Twin_2.mkv").exists(),
        "disc 2 filed beside it: {second_row:?}"
    );
}

#[cfg(unix)]
fn inode(path: &Path) -> u64 {
    std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(path).unwrap())
}

// Delivered, but the staging teardown fails (a file appeared after the freeze). The row
// backs off like a failed copy instead of re-running the whole post-copy stage every tick,
// and a re-pass over the delivered files rewrites neither the journal nor the links.
#[cfg(unix)]
#[test]
fn a_failed_teardown_backs_off_and_its_re_pass_writes_nothing() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, job, dests) = journaled_files(tmp.path(), "Tidy", &["mkv", "iso"]);
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    std::fs::write(job.join("late arrival.txt"), b"not planned").unwrap();
    check_and_move(&cfg);
    let first = row(&key);
    let links = tmp
        .path()
        .join("config")
        .join(crate::server::library::links::LINKS_FILE);
    let journal = job.join(super::manifest::FILE);
    let (links_before, journal_before) = (inode(&links), inode(&journal));
    let delivered = super::manifest::Manifest::deliver(&job, 0, &|_, _, _, _| {});
    let journal_after_deliver = inode(&journal);
    if let Some(r) = MOVE_ERRORS.lock().unwrap().get_mut(&key) {
        r.retry_after = Some(std::time::Instant::now());
    }
    check_and_move(&cfg);
    let links_after = inode(&links);
    clear_error(&key);
    assert!(dests.iter().all(|d| d.exists()), "fixture: delivered");
    let first = first.expect("the teardown failure is shown");
    assert!(first.reason.contains("staging cleanup failed"), "{first:?}");
    assert_eq!(first.attempts, 1, "it backs off: {first:?}");
    assert!(
        first
            .retry_after
            .is_some_and(|t| t > std::time::Instant::now()),
        "not due again at once: {first:?}"
    );
    assert!(
        matches!(delivered, Ok(MoveOutcome::Skipped)),
        "{delivered:?}"
    );
    assert_eq!(journal_before, journal_after_deliver, "journal rewritten");
    assert_eq!(links_before, links_after, "links rewritten");
}

// The incomplete-listing teardown skip goes through the same backoff as a failed teardown.
#[test]
fn the_incomplete_listing_skip_backs_off_too() {
    let src = crate::server::util::source_lf(include_str!("mover.rs"));
    let skip = src.find("if !listing_complete {\n            teardown_blocked(");
    assert!(
        skip.is_some(),
        "the listing skip must record a backed-off row"
    );
}

// A journaled delivery into `output/<title> (2024)/`, whose folder the test may remove.
fn journaled_in_folder(
    tmp: &Path,
    title: &str,
) -> (Config, std::path::PathBuf, std::path::PathBuf) {
    let staging = tmp.join("staging");
    let job = staging.join(title);
    let output = tmp.join("output");
    let folder = output.join(format!("{title} (2024)"));
    for d in [&job, &folder, &tmp.join("config")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let marker = serde_json::json!({"title": title, "year": 2024, "media_type": "movie"});
    std::fs::write(job.join(".done"), marker.to_string()).unwrap();
    let src = job.join(format!("{title}.mkv"));
    std::fs::write(&src, [0x1A, 0x45, 0xDF, 0xA3, 1, 2, 3, 4, 5, 6]).unwrap();
    let dest = folder.join(format!("{title}.mkv"));
    super::manifest::Manifest::create(
        &job,
        marker,
        Vec::new(),
        vec![output.to_string_lossy().into_owned()],
        &[(src, dest.to_string_lossy().into_owned())],
    )
    .unwrap();
    let cfg = Config {
        staging_dir: staging.to_string_lossy().into_owned(),
        output_dir: output.to_string_lossy().into_owned(),
        autorip_dir: tmp.join("config").to_string_lossy().into_owned(),
        ..Config::default()
    };
    (cfg, job, folder)
}

// Retry on a row waiting for the output ROOT (not responding, stale, unmounted...) only
// retries: the root may come back as an empty local folder, so it never consents to
// recreating the delivery's missing title folder. Only the missing-folder row does.
#[test]
fn retry_on_a_root_wait_never_recreates_a_missing_title_folder() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg, job, folder) = journaled_in_folder(tmp.path(), "Remounted");
    let key = job.to_string_lossy().into_owned();
    clear_error(&key);
    std::fs::remove_dir(&folder).unwrap();
    let root = Path::new(&cfg.output_dir);
    record_error_at_stage(
        &key,
        "waiting for the output folder: The output folder is not responding",
        "Remount the share",
        Some(root),
        |_| {},
    );
    clear_move_error(&key);
    check_and_move(&cfg);
    let waiting = row(&key);
    assert!(
        !folder.exists(),
        "a root-wait Retry recreated the folder: {waiting:?}"
    );
    let waiting = waiting.expect("the missing folder is shown");
    assert!(waiting.reason.contains("missing"), "{waiting:?}");
    // Retry on the missing-folder row itself is the consent.
    clear_move_error(&key);
    check_and_move(&cfg);
    clear_error(&key);
    assert!(folder.exists() && !job.exists(), "recreated and delivered");
}

// A Retry on a dir with no journal yet is spent on its next pass, never kept for a later
// journal (where it would read as consent to recreate folders).
#[test]
fn a_retry_on_an_unjournaled_dir_is_spent_on_its_next_pass() {
    let _g = guard();
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let job = staging.join("Early");
    std::fs::create_dir_all(&job).unwrap();
    let marker = serde_json::json!({"title": "Early", "year": 2024, "media_type": "movie"});
    std::fs::write(job.join(".done"), marker.to_string()).unwrap();
    std::fs::write(job.join("Early.mkv"), [0x1A, 0x45, 0xDF, 0xA3, 0, 0]).unwrap();
    let cfg = Config {
        staging_dir: staging.to_string_lossy().into_owned(),
        // Never created: the destination root is missing, so nothing is frozen.
        output_dir: tmp.path().join("absent").to_string_lossy().into_owned(),
        ..Config::default()
    };
    let key = job.to_string_lossy().into_owned();
    record_error_at_stage(
        &key,
        "waiting for the output folder",
        "",
        Some(Path::new(&cfg.output_dir)),
        |_| {},
    );
    clear_move_error(&key);
    assert!(requested(&key), "fixture: Retry recorded");
    check_and_move(&cfg);
    let left = requested(&key);
    clear_error(&key);
    assert!(
        !job.join(super::manifest::FILE).exists(),
        "fixture: no journal"
    );
    assert!(!left, "the request outlived the pass");
}
