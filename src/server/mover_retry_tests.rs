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
