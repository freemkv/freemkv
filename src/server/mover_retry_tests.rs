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
    let staging = tmp.join("staging");
    let job = staging.join(title);
    let output = tmp.join("output");
    for d in [&job, &output, &tmp.join("config")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let marker = serde_json::json!({"title": title, "year": 2024, "media_type": "movie"});
    std::fs::write(job.join(".done"), marker.to_string()).unwrap();
    let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
    mkv.extend_from_slice(&[0x5Au8; 2048]);
    let src = job.join(format!("{title}.mkv"));
    std::fs::write(&src, &mkv).unwrap();
    let dest = output.join(format!("{title}.mkv"));
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
    (cfg, job, dest)
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
