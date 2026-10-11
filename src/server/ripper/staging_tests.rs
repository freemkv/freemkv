use super::*;
use std::fs;

#[test]
fn capture_reset_keeps_corrected_identity_but_discards_old_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = DiscState::new(StagingState::Stopped);
    old.disc_identity = "disc-hash".into();
    old.title = "Castaway".into();
    try_write_state(dir.path(), &old).unwrap();
    let correction = UserMetadata {
        episode_start: None,
        title: "Cast Away".into(),
        year: 2000,
        media_type: crate::server::planner::MediaKind::Movie,
        tmdb_id: 8358,
        poster_url: String::new(),
        overview: String::new(),
    };
    let revision = save_user_metadata(dir.path(), correction.clone()).unwrap();
    fs::write(dir.path().join("old.iso"), b"old capture").unwrap();
    fs::create_dir(dir.path().join("old-output")).unwrap();
    fs::write(dir.path().join("old-output/episode.mkv"), b"old output").unwrap();
    let lease = job_lease(dir.path());
    let _owner = lease.lock().unwrap();
    reset_capture_preserving_metadata(dir.path(), "").unwrap();
    let saved = read_state(dir.path()).unwrap();
    assert_eq!(saved.disc_identity, "disc-hash");
    assert_eq!(saved.user_metadata, Some(correction));
    assert_eq!(saved.metadata_revision, revision);
    assert_eq!(saved.title, "Cast Away");
    assert_eq!(saved.state, StagingState::Stopped);
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    let started = begin_capture(dir.path(), 0, "").unwrap();
    assert_eq!(started.title, "Cast Away");
    assert_eq!(started.tmdb_id, 8358);
    assert_eq!(started.metadata_revision, revision);
    assert_eq!(started.state, StagingState::Sweeping);
    assert_eq!(started.max_retries, 0);
}

#[test]
fn capture_reset_refuses_corrupt_metadata_without_removing_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(state_path(dir.path()), b"{ torn").unwrap();
    fs::write(dir.path().join("keep.iso"), b"capture").unwrap();
    let lease = job_lease(dir.path());
    let _owner = lease.lock().unwrap();
    assert!(reset_capture_preserving_metadata(dir.path(), "").is_err());
    assert_eq!(fs::read(dir.path().join("keep.iso")).unwrap(), b"capture");
    assert_eq!(fs::read(state_path(dir.path())).unwrap(), b"{ torn");
}

#[test]
fn interrupted_capture_reset_cannot_resume_old_capture_with_an_empty_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = DiscState::new(StagingState::Stopped);
    old.disc_identity = "same-disc".into();
    old.title = "Show".into();
    old.media_type = "tv".into();
    try_write_state(dir.path(), &old).unwrap();
    fs::write(dir.path().join("old.iso"), b"old capture").unwrap();
    let lease = job_lease(dir.path());
    let _owner = lease.lock().unwrap();
    assert!(
        reset_capture_with(dir.path(), "", || Err(io::Error::other("simulated crash"))).is_err()
    );
    assert!(read_state(dir.path()).unwrap().reset_in_progress);
    assert!(dir.path().join("old.iso").exists());
    assert!(snapshot_staging_disc(dir.path()).is_none());
    assert!(begin_capture(dir.path(), 1, "").is_err());
    assert!(try_write_state(dir.path(), &old).is_err());
    mutate_state_if_present(dir.path(), |st| st.reset_in_progress = false);
    assert!(read_state(dir.path()).unwrap().reset_in_progress);
    reset_capture_preserving_metadata(dir.path(), "").unwrap();
    let ready = read_state(dir.path()).unwrap();
    assert!(!ready.reset_in_progress);
    assert_eq!(ready.disc_identity, "same-disc");
    assert!(!dir.path().join("old.iso").exists());
    assert!(begin_capture(dir.path(), 1, "").is_ok());
}

#[test]
fn interrupted_legacy_capture_reset_is_also_held_until_cleanup_finishes() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("legacy");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("old.iso"), b"old capture").unwrap();
    let lease = job_lease(&dir);
    let _owner = lease.lock().unwrap();
    assert!(reset_capture_with(&dir, "", || Err(io::Error::other("simulated crash"))).is_err());
    assert!(read_state(&dir).unwrap().reset_in_progress);
    assert!(snapshot_staging_disc(&dir).is_none());
    reset_capture_preserving_metadata(&dir, "").unwrap();
    assert!(!dir.exists());
}

#[test]
fn interrupted_legacy_reset_is_rediscovered_after_mapfile_removal() {
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("OldMovie");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("OldMovie.iso"), b"capture").unwrap();
    let map_path = dir.join("OldMovie.iso.mapfile");
    let mut map = freemkv_engine::Mapfile::create(&map_path, 4096, "test").unwrap();
    map.set_disc_hash(hash);
    map.flush().unwrap();
    assert_eq!(
        super::super::jobs::locate(root.path(), hash).unwrap(),
        "OldMovie"
    );
    let lease = job_lease(&dir);
    let _owner = lease.lock().unwrap();
    assert!(
        reset_capture_with(&dir, hash, || {
            fs::remove_file(&map_path)?;
            Err(io::Error::other(
                "crash after removing identity-bearing map",
            ))
        })
        .is_err()
    );
    assert_eq!(
        super::super::jobs::locate(root.path(), hash).unwrap(),
        "OldMovie"
    );
    assert!(read_state(&dir).unwrap().reset_in_progress);
    reset_capture_preserving_metadata(&dir, hash).unwrap();
    assert_eq!(
        super::super::jobs::locate(root.path(), hash).unwrap(),
        "OldMovie"
    );
    assert!(!read_state(&dir).unwrap().reset_in_progress);
    assert!(!dir.join("OldMovie.iso").exists());
}

#[test]
fn capture_reset_refuses_a_job_whose_delivery_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let mut done = DiscState::new(StagingState::Done);
    done.disc_identity = "same-disc".into();
    try_write_state(dir.path(), &done).unwrap();
    fs::write(dir.path().join("Show_S01E03.mkv"), b"unpublished").unwrap();
    fs::write(dir.path().join(DELIVERY_JOURNAL), b"{}").unwrap();
    let lease = job_lease(dir.path());
    let _owner = lease.lock().unwrap();
    let refused = reset_capture_preserving_metadata(dir.path(), "same-disc").unwrap_err();
    assert!(refused.to_string().contains("delivery"), "{refused}");
    assert_eq!(
        fs::read(dir.path().join("Show_S01E03.mkv")).unwrap(),
        b"unpublished"
    );
    assert!(dir.path().join(DELIVERY_JOURNAL).exists());
    let kept = read_state(dir.path()).unwrap();
    assert_eq!(kept.state, StagingState::Done);
    assert!(!kept.reset_in_progress);
}

#[test]
fn capture_reset_does_not_hold_the_state_lock_while_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let mut old = DiscState::new(StagingState::Stopped);
    old.disc_identity = "same-disc".into();
    try_write_state(dir.path(), &old).unwrap();
    fs::write(dir.path().join("old.iso"), b"old capture").unwrap();
    let lease = job_lease(dir.path());
    let _owner = lease.lock().unwrap();
    let path = dir.path().to_path_buf();
    reset_capture_with(dir.path(), "same-disc", || {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let correction = UserMetadata {
                episode_start: None,
                title: "Cast Away".into(),
                year: 2000,
                media_type: crate::server::planner::MediaKind::Movie,
                tmdb_id: 8358,
                poster_url: String::new(),
                overview: String::new(),
            };
            let _ = tx.send(save_user_metadata(&path, correction).is_err());
        });
        let refused = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a metadata save must not wait on the reset's deletion");
        assert!(refused, "the interrupted reset refuses the save");
        Ok(())
    })
    .unwrap();
    assert!(!dir.path().join("old.iso").exists());
    assert!(!read_state(dir.path()).unwrap().reset_in_progress);
}

#[test]
fn capture_reset_refuses_a_conflicting_verified_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut saved = DiscState::new(StagingState::Stopped);
    saved.disc_identity = "first-disc".into();
    try_write_state(dir.path(), &saved).unwrap();
    fs::write(dir.path().join("keep.iso"), b"capture").unwrap();
    let lease = job_lease(dir.path());
    let _owner = lease.lock().unwrap();
    assert!(reset_capture_preserving_metadata(dir.path(), "other-disc").is_err());
    assert_eq!(fs::read(dir.path().join("keep.iso")).unwrap(), b"capture");
    assert_eq!(read_state(dir.path()).unwrap().disc_identity, "first-disc");
    assert!(!read_state(dir.path()).unwrap().reset_in_progress);
}

fn tmpdir() -> PathBuf {
    // Repo-local scratch, never /tmp (wiped on reboot, cross-run collisions).
    // Anchor to the crate's target/ dir so `cargo clean` cleans it up.
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch");
    let p = base.join(format!(
        "autorip-staging-test-{}-{}",
        std::process::id(),
        crate::server::util::epoch_secs()
    ));
    fs::create_dir_all(&p).unwrap();
    // Fresh subdir even when two tests land on the same epoch second: a
    // monotonic counter is non-repeating, unlike a stack-address ({:p}).
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let sub = p.join(format!("t-{}", COUNTER.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(&sub).unwrap();
    sub
}

#[test]
fn read_state_declines_a_foreign_schema() {
    let dir = tmpdir();
    write_state(&dir, &DiscState::new(StagingState::Failed));
    assert!(read_state(&dir).is_some(), "current schema reads back");

    // Tamper the on-disk schema to a foreign value (e.g. the schema-1
    // RippedMarker): read_state must decline it, not resume on defaults.
    let p = state_path(&dir);
    let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    v["schema"] = serde_json::json!(1);
    fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(
        read_state(&dir).is_none(),
        "a foreign-schema state.json must not resume as unified state",
    );
}

// Every read-modify-write writer must REFUSE to replace an unreadable
// state.json (corrupt, foreign, or an I/O error) with a fresh default: that
// would silently turn a held TV plan into an empty, valid movie plan.
#[test]
fn rmw_writers_refuse_to_overwrite_an_unreadable_state_json() {
    const BAD: &[u8] = b"{ torn";
    let seed = || {
        let d = tmpdir();
        fs::write(state_path(&d), BAD).unwrap();
        d
    };
    let untouched = |d: &Path, who: &str| {
        assert_eq!(
            fs::read(state_path(d)).unwrap(),
            BAD,
            "{who} overwrote state.json"
        );
    };

    let d = seed();
    assert!(mutate_state(&d, StagingState::Sweeping, |_| {}).is_err());
    untouched(&d, "mutate_state");
    let d = seed();
    assert!(!write_failed_marker(&d, "boom"), "must report not-landed");
    untouched(&d, "write_failed_marker");
    let d = seed();
    assert!(
        !write_aborted_loss_marker(&d, "loss", 1),
        "must report not-landed"
    );
    untouched(&d, "write_aborted_loss_marker");
    let d = seed();
    assert!(mark_handoff(&d, true, |_| {}).is_err());
    untouched(&d, "mark_handoff");
    let d = seed();
    write_completed_marker(&d);
    untouched(&d, "write_completed_marker");
    let d = seed();
    write_sweeping_marker(&d);
    untouched(&d, "write_sweeping_marker");
    let d = seed();
    let m = DiscState::new(StagingState::Ripped).to_ripped_marker();
    assert!(crate::server::muxer::write_marker(&d, &m).is_err());
    untouched(&d, "muxer::write_marker");
}

// An unreadable state.json blocks a capture start outright: nothing is set
// aside or replaced, so the operator's held card stays the one way forward.
#[test]
fn begin_capture_refuses_an_unreadable_state_and_keeps_it() {
    let d = tmpdir();
    fs::write(state_path(&d), b"{ torn").unwrap();
    assert!(begin_capture(&d, 1, "").is_err());
    assert_eq!(fs::read(state_path(&d)).unwrap(), b"{ torn");
    assert!(!d.join(UNREADABLE_STATE_ASIDE).exists());
}

// G5/G6: startup must hold (not wipe, not quarantine over) a dir whose
// state.json is unreadable, and raise the operator card.
#[test]
fn startup_holds_a_dir_with_an_unreadable_state_json() {
    let root = tmpdir();
    let bare = root.join("Only_State");
    let looping = root.join("Looping");
    for d in [&bare, &looping] {
        fs::create_dir_all(d).unwrap();
        fs::write(state_path(d), b"{ torn").unwrap();
    }
    fs::write(looping.join("Looping.iso"), b"x").unwrap();
    fs::write(looping.join(RESTART_COUNT_FILE), b"99\n").unwrap();

    let hints = resume_or_quarantine_staging(&root.to_string_lossy());
    for d in [&bare, &looping] {
        assert!(d.is_dir(), "{} must not be wiped", d.display());
        assert_eq!(
            fs::read(state_path(d)).unwrap(),
            b"{ torn",
            "{}",
            d.display()
        );
        let h = hints.iter().find(|h| &h.dir == d).expect("a hint per dir");
        assert!(
            matches!(h.action, ResumeAction::HeldUnreadableState { .. }),
            "{}: {:?}",
            d.display(),
            h.action
        );
        let path = d.to_string_lossy().to_string();
        assert!(
            crate::server::muxer::MUX_ERRORS
                .lock()
                .unwrap()
                .contains_key(&path),
            "{} must raise the held card",
            d.display()
        );
        crate::server::muxer::clear_error_with_prefix(&path, STATE_HELD_PREFIX);
    }
}

// N2: the card reason is the de-dupe key, so an I/O hold's reason must
// not carry the raw (flapping EIO/ESTALE) error text.
#[test]
fn io_hold_reason_is_stable_across_errnos() {
    let a = StateUnreadable::io(&io::Error::other("Input/output error"));
    let b = StateUnreadable::io(&io::Error::other("Stale file handle"));
    assert_eq!(a.held_reason(), b.held_reason());
    assert!(a.hint().contains("click Resume"), "{}", a.hint());
}

#[test]
fn read_state_checked_separates_absent_from_unreadable() {
    let dir = tmpdir();
    assert!(matches!(read_state_checked(&dir), StateRead::Absent));
    write_state(&dir, &DiscState::new(StagingState::Ripped));
    assert!(matches!(read_state_checked(&dir), StateRead::Valid(_)));
    let p = state_path(&dir);
    fs::write(&p, br#"{"schema": 1, "state": "ripped"}"#).unwrap();
    assert!(matches!(read_state_checked(&dir), StateRead::Unreadable(_)));
    fs::write(&p, b"{ torn").unwrap();
    assert!(matches!(read_state_checked(&dir), StateRead::Unreadable(_)));
}

#[test]
fn restart_count_missing_returns_zero() {
    let d = tmpdir();
    assert_eq!(restart_count(&d), 0);
}

#[test]
fn accept_loss_marker_round_trips_and_is_one_shot() {
    let d = tmpdir();
    assert!(!accept_loss_requested(&d), "absent by default");
    write_accept_loss_marker(&d);
    assert!(accept_loss_requested(&d), "present after write");
    clear_accept_loss_marker(&d);
    assert!(
        !accept_loss_requested(&d),
        "cleared (one-shot) after consume"
    );
}

#[test]
fn increment_creates_then_advances() {
    let d = tmpdir();
    assert_eq!(increment_restart_count(&d).unwrap(), 1);
    assert_eq!(restart_count(&d), 1);
    assert_eq!(increment_restart_count(&d).unwrap(), 2);
    assert_eq!(restart_count(&d), 2);
}

#[test]
fn clear_is_idempotent() {
    let d = tmpdir();
    clear_restart_count(&d); // missing — must not panic
    increment_restart_count(&d).unwrap();
    clear_restart_count(&d);
    assert_eq!(restart_count(&d), 0);
    clear_restart_count(&d); // already gone — must not error
}

// The durability gate decides whether `.done`/`.completed` may be
// written; getting it backwards files a truncated file as a finished
// title. A mutation run once dropped the `!` in each copy unnoticed.
#[test]
fn durability_gate_blocks_markers_unless_the_output_is_provably_durable() {
    // A failed fsync must withhold the markers.
    assert!(!durability_gate_passes(false, || false));
    // A successful one must not.
    assert!(durability_gate_passes(false, || true));

    // A network sink has no local file, so the gate passes WITHOUT
    // evaluating the fsync at all — an eager `is_network || fsync(path)`
    // would stat a path that does not exist.
    let mut called = false;
    assert!(durability_gate_passes(true, || {
        called = true;
        false
    }));
    assert!(!called, "the fsync must not run for a network sink");
}

// Both halves matter: a "network" format with no target configured
// falls back to LOCAL output and still needs the flush.
#[test]
fn network_output_requires_both_the_format_and_a_target() {
    assert!(is_network_output("network", "nfs://box/media"));
    assert!(
        !is_network_output("network", ""),
        "no target means local output — the durability gate must still run"
    );
    assert!(!is_network_output("mkv", "nfs://box/media"));
    assert!(!is_network_output("iso", ""));
}

// `fsync_output_file` is the mux durability gate. The rc.4.1 Windows
// remux loop was this returning false forever (opened read-only, and
// FlushFileBuffers rejects that); this pins both arms of the contract.
#[test]
fn fsync_output_file_true_for_real_false_for_missing() {
    let d = tmpdir();
    let f = d.join("out.mkv");
    fs::write(&f, b"muxed bytes").unwrap();
    assert!(
        fsync_output_file(&f),
        "an existing output file must fsync successfully (gate passes)"
    );
    assert!(
        !fsync_output_file(&d.join("never-written.mkv")),
        "a missing output file must fail the gate so staging is preserved"
    );
}

// Must round-trip the incremented value and leave no `.restart_count.tmp`
// behind — a dangling `.tmp` would mean a torn write or a rename that
// never happened.
#[test]
fn increment_roundtrips_and_cleans_up_tmp() {
    let d = tmpdir();
    let tmp = d.join(format!("{}.tmp", RESTART_COUNT_FILE));

    let v1 = increment_restart_count(&d).unwrap();
    assert_eq!(v1, 1);
    assert_eq!(restart_count(&d), 1, "incremented value must round-trip");
    assert!(
        !tmp.exists(),
        "{} must be renamed away, not left behind",
        tmp.display()
    );

    let v2 = increment_restart_count(&d).unwrap();
    assert_eq!(v2, 2);
    assert_eq!(restart_count(&d), 2);
    assert!(!tmp.exists(), "tmp file must not persist across increments");
}

#[test]
fn corrupt_restart_count_returns_zero() {
    let d = tmpdir();
    fs::write(d.join(RESTART_COUNT_FILE), b"garbage\n").unwrap();
    assert_eq!(restart_count(&d), 0);
}

#[test]
fn failed_marker_roundtrip() {
    let d = tmpdir();
    write_failed_marker(&d, "test reason");
    assert_eq!(read_failed_reason(&d).as_deref(), Some("test reason"));
}

// A hand-off marker must never be written empty: the mover skips
// directories whose marker won't parse, stranding a finished output
// with no operator-facing signal.
#[test]
fn handoff_marker_is_nonempty_and_parseable() {
    let d = tmpdir();
    let marker = serde_json::json!({
        "title": "Some Movie",
        "format": "Blu-ray",
        "year": 2024,
        "date": "2024-01-01",
    });
    let body = serde_json::to_string_pretty(&marker).expect("json! value is always serialisable");
    let path = d.join(".done");
    write_handoff_marker(&path, body.as_bytes()).unwrap();

    let written = fs::read(&path).unwrap();
    assert!(!written.is_empty(), ".done marker must not be empty bytes");
    let parsed: serde_json::Value = serde_json::from_slice(&written).unwrap();
    assert_eq!(
        parsed.get("title").and_then(|v| v.as_str()),
        Some("Some Movie")
    );
}

// Graceful-shutdown belt-and-suspenders: strips every `.sweeping`/
// `.muxing` marker under the root so a clean SIGTERM isn't misread by
// the next startup's resume classifier as a crash.
#[test]
fn clear_inprogress_markers_strips_sweeping_and_muxing_under_root() {
    let root = tmpdir();
    let disc_a = root.join("DiscA");
    let disc_b = root.join("DiscB");
    fs::create_dir_all(&disc_a).unwrap();
    fs::create_dir_all(&disc_b).unwrap();
    write_sweeping_marker(&disc_a);
    let mut st = DiscState::new(StagingState::Ripped);
    st.muxing = true;
    write_state(&disc_b, &st);
    assert_eq!(read_state(&disc_a).unwrap().state, StagingState::Sweeping);
    assert!(read_state(&disc_b).unwrap().muxing);

    clear_inprogress_markers(&root);

    assert_eq!(
        read_state(&disc_a).unwrap().state,
        StagingState::Stopped,
        ".sweeping must be cleared on graceful shutdown"
    );
    assert!(
        !read_state(&disc_b).unwrap().muxing,
        ".muxing must be cleared on graceful shutdown"
    );
}

// `write_failed_marker` must REPORT whether the terminal state landed,
// not silently swallow a write failure — a dropped write re-dispatches
// to the mux worker forever without this signal.
#[test]
fn write_failed_marker_reports_whether_state_landed() {
    // Success: a normal dir → terminal state lands, returns true.
    let root = tmpdir();
    let disc = root.join("Good");
    fs::create_dir_all(&disc).unwrap();
    assert!(
        write_failed_marker(&disc, "E6008"),
        "a successful terminal write must report landed=true"
    );
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);

    // Failure: point the writer at a path whose parent is a FILE, so the
    // atomic state.json write cannot create its temp file (ENOTDIR). The
    // return must be false — the signal the worker surfaces loudly.
    let not_a_dir = root.join("iam_a_file");
    fs::write(&not_a_dir, b"x").unwrap();
    let doomed = not_a_dir.join("child");
    assert!(
        !write_failed_marker(&doomed, "E6008"),
        "a failed terminal write must report landed=false, never swallow it"
    );
}

#[test]
fn resume_marks_failed_after_limit() {
    // Build a fake staging tree: <root>/<disc>/foo.iso plus
    // .restart_count == RESTART_LIMIT.
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    fs::write(
        disc.join(RESTART_COUNT_FILE),
        format!("{}\n", RESTART_LIMIT).as_bytes(),
    )
    .unwrap();

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(matches!(
        hints[0].action,
        ResumeAction::RestartLoopFailed { .. }
    ));
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);
    // Counter cleared after promotion to .failed.
    assert_eq!(restart_count(&disc), 0);
}

#[test]
fn resume_cleanly_stopped_resumable_not_counted() {
    // A bare resumable dir (ISO, no `.sweeping`/`.muxing`) is a clean
    // stop/redeploy/reboot, not a crash loop — must not bump
    // `.restart_count`, or healthy resumable rips get walked to `.failed`.
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    match &hints[0].action {
        ResumeAction::ResumePreserved { attempt, .. } => assert_eq!(*attempt, 0),
        other => panic!("unexpected action: {:?}", other),
    }
    assert_eq!(
        restart_count(&disc),
        0,
        "a clean stop must not bump restart_count"
    );
    assert!(!disc.join(FAILED_MARKER).exists());
}

#[test]
fn resume_preserves_completed_dirs() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.mkv"), b"x").unwrap();
    write_completed_marker(&disc);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(matches!(hints[0].action, ResumeAction::AlreadyCompleted));
    // Marker must still be there afterwards.
    assert!(snapshot_staging_disc(&disc).unwrap().completed);
    // MKV must still be there afterwards.
    assert!(disc.join("foo.mkv").exists());
}

#[test]
fn done_marker_with_partial_state_is_completed_not_retried() {
    // A crash between writing .done and .completed leaves .done +
    // the ISO/mapfile on disk. The resume scan must treat this as a
    // completed rip awaiting the mover, NOT bump .restart_count.
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    fs::write(disc.join("foo.iso.mapfile"), b"x").unwrap();
    fs::write(disc.join(DONE_MARKER), b"{}").unwrap();
    // No .completed marker (the crash happened before it landed).

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::AlreadyCompleted),
        "got {:?}",
        hints[0].action
    );
    // Counter must NOT have been bumped — this was a finished rip.
    assert_eq!(restart_count(&disc), 0);
    assert!(!disc.join(FAILED_MARKER).exists());
    // Data preserved for the mover.
    assert!(disc.join("foo.iso").exists());
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Done);
}

#[test]
fn review_marker_with_partial_state_is_completed_not_retried() {
    // A crash between writing .review and .completed leaves .review +
    // the ISO/mapfile/MKV on disk; must be treated as finished, held
    // for operator review, not bumped toward .failed.
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    fs::write(disc.join("foo.iso.mapfile"), b"x").unwrap();
    fs::write(disc.join("MyDisc.mkv"), b"x").unwrap();
    fs::write(disc.join(REVIEW_MARKER), b"{}").unwrap();
    // No .completed marker (the crash happened before it landed).

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::AlreadyCompleted),
        "got {:?}",
        hints[0].action
    );
    // Counter must NOT have been bumped — this was a finished rip.
    assert_eq!(restart_count(&disc), 0);
    assert!(!disc.join(FAILED_MARKER).exists());
    // Data preserved for the operator/mover.
    assert!(disc.join("MyDisc.mkv").exists());
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Review);
}

// The legacy-upgrade path must fold every field of a real (non-empty)
// `.done` body into the fresh state.json, and remove the legacy file.
// Earlier tests only wrote `b"{}"` bodies, so field-folding was untested.
#[test]
fn legacy_done_body_migrates_metadata_into_state() {
    let root = tmpdir();
    let disc = root.join("Endeavour_S5_D2");
    fs::create_dir_all(&disc).unwrap();
    let body = serde_json::json!({
        "title": "Endeavour",
        "disc_name": "ENDEAVOUR_S5_D2",
        "format": "bluray",
        "year": 2012,
        "media_type": "tv",
        "tmdb_id": 44264,
        "season": 5,
        "disc": 2,
        "poster_url": "http://x/p.jpg",
        "overview": "A young detective in 1960s Oxford.",
        "date": "2026-08-22",
    });
    fs::write(disc.join(DONE_MARKER), body.to_string()).unwrap();

    let snap = snapshot_staging_disc(&disc);
    assert!(snap.is_some(), "a real .done body must still snapshot");

    let st = read_state(&disc).expect("state.json must exist after migration");
    assert_eq!(st.state, StagingState::Done);
    assert_eq!(st.title, "Endeavour");
    assert_eq!(st.disc_name, "ENDEAVOUR_S5_D2");
    assert_eq!(st.disc_format, "bluray");
    assert_eq!(st.year, 2012);
    assert_eq!(st.media_type, "tv");
    assert_eq!(st.tmdb_id, 44264);
    assert_eq!(st.season, Some(5));
    assert_eq!(st.disc_number, Some(2));
    assert!(
        !st.tmdb_poster.is_empty(),
        "poster_url must fold into tmdb_poster"
    );
    assert_eq!(st.tmdb_poster, "http://x/p.jpg");
    assert_eq!(st.tmdb_overview, "A young detective in 1960s Oxford.");
    assert_eq!(st.date, "2026-08-22");

    assert!(
        !disc.join(DONE_MARKER).exists(),
        "the legacy .done file must be removed once migrated to state.json"
    );
}

/// Same migration, but for the `.review` hand-off (unconfident-title path):
/// the folded state must land in `StagingState::Review`, and the legacy
/// `.review` file must be gone afterwards.
#[test]
fn legacy_review_body_migrates_to_review_state() {
    let root = tmpdir();
    let disc = root.join("SomeShow_S1_D1");
    fs::create_dir_all(&disc).unwrap();
    let body = serde_json::json!({
        "title": "Some Show",
        "disc_name": "SOMESHOW_S1_D1",
        "format": "dvd",
        "year": 2005,
        "media_type": "tv",
        "tmdb_id": 9999,
        "season": 1,
        "disc": 1,
        "poster_url": "http://x/q.jpg",
        "overview": "overview text",
        "date": "2026-08-20",
    });
    fs::write(disc.join(REVIEW_MARKER), body.to_string()).unwrap();

    let snap = snapshot_staging_disc(&disc);
    assert!(snap.is_some());

    let st = read_state(&disc).expect("state.json must exist after migration");
    assert_eq!(st.state, StagingState::Review);
    assert_eq!(st.title, "Some Show");
    assert_eq!(st.disc_name, "SOMESHOW_S1_D1");
    assert_eq!(st.disc_format, "dvd");
    assert_eq!(st.year, 2005);
    assert_eq!(st.media_type, "tv");
    assert_eq!(st.tmdb_id, 9999);
    assert_eq!(st.season, Some(1));
    assert_eq!(st.disc_number, Some(1));
    assert_eq!(st.tmdb_poster, "http://x/q.jpg");
    assert_eq!(st.tmdb_overview, "overview text");

    assert!(
        !disc.join(REVIEW_MARKER).exists(),
        "the legacy .review file must be removed once migrated to state.json"
    );
}

// A legacy `.failed` marker migrates its `reason` into
// `DiscState::failure_reason`, and the legacy file must be removed.
#[test]
fn legacy_failed_migrates_and_removes_file() {
    let root = tmpdir();
    let disc = root.join("BadDisc");
    fs::create_dir_all(&disc).unwrap();
    let body = serde_json::json!({ "reason": "boom" });
    fs::write(disc.join(FAILED_MARKER), body.to_string()).unwrap();

    let snap = snapshot_staging_disc(&disc);
    assert!(snap.is_some());

    let st = read_state(&disc).expect("state.json must exist after migration");
    assert_eq!(st.state, StagingState::Failed);
    assert_eq!(st.failure_reason.as_deref(), Some("boom"));

    assert!(
        !disc.join(FAILED_MARKER).exists(),
        "the legacy .failed file must be removed once migrated to state.json"
    );
}

#[test]
fn snapshot_reports_unknown_on_unreadable_dir() {
    // A path that isn't a directory (read_dir errors) must yield
    // None, not a "looks empty" snapshot that the caller might wipe.
    let root = tmpdir();
    let not_a_dir = root.join("a_file");
    fs::write(&not_a_dir, b"x").unwrap();
    assert!(snapshot_staging_disc(&not_a_dir).is_none());
}

#[test]
fn all_direntry_errors_with_no_artifacts_is_unknown_not_partial() {
    // read_dir opened but every DirEntry I/O errored and nothing
    // trustworthy was observed — must classify UNKNOWN so the caller
    // skips it without bumping `.restart_count` toward `.failed`.
    let obs = ScanObservations {
        saw_read_ok: true,
        had_entry_error: true,
        ..Default::default()
    };
    assert!(obs.observed_nothing());
    assert!(
        obs.contents_unknown(),
        "all-DirEntry-error + no artifacts must be UNKNOWN, not partial state"
    );
}

#[test]
fn all_read_dir_attempts_errored_is_unknown() {
    // The original all-attempts-errored defense: never got a listing.
    let obs = ScanObservations {
        saw_read_ok: false,
        ..Default::default()
    };
    assert!(obs.contents_unknown());
}

#[test]
fn entry_error_alongside_real_artifact_is_not_unknown() {
    // One DirEntry errored but the ISO was still seen — not unknown,
    // so the snapshot is kept and normal resume/restart handling runs.
    let obs = ScanObservations {
        saw_read_ok: true,
        saw_any_entries: true,
        had_entry_error: true,
        has_iso: true,
        ..Default::default()
    };
    assert!(!obs.observed_nothing());
    assert!(!obs.contents_unknown());
}

#[test]
fn clean_empty_dir_is_not_unknown() {
    // read_dir succeeded, dir was genuinely empty, no entry errors.
    // Not UNKNOWN — the caller may legitimately wipe a truly-empty,
    // marker-less staging dir.
    let obs = ScanObservations {
        saw_read_ok: true,
        ..Default::default()
    };
    assert!(!obs.contents_unknown());
}

#[test]
fn unknown_contents_snapshot_does_not_bump_restart_count() {
    // A snapshot returning None for UNKNOWN contents means the dir is
    // skipped entirely, so its restart count stays untouched (can't
    // provoke real per-entry NFS errors here, so we assert the contract).
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    // Pre-seed restart_count near the limit to make a wrongful bump
    // (which would push it to .failed) maximally visible.
    fs::write(
        disc.join(RESTART_COUNT_FILE),
        format!("{}\n", RESTART_LIMIT - 1).as_bytes(),
    )
    .unwrap();
    // The contract: when snapshot_staging_disc returns None (UNKNOWN),
    // the dir is skipped. Verify the predicate that drives that None.
    let unknown = ScanObservations {
        saw_read_ok: true,
        had_entry_error: true,
        ..Default::default()
    };
    assert!(unknown.contents_unknown());
    // And confirm that simply NOT processing the dir leaves the
    // counter where it was — no bump, no promotion to .failed.
    assert_eq!(restart_count(&disc), RESTART_LIMIT - 1);
    assert!(!disc.join(FAILED_MARKER).exists());
}

#[test]
fn resume_preserves_failed_dirs() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    write_failed_marker(&disc, "prior failure");

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    match &hints[0].action {
        ResumeAction::AlreadyFailed { reason } => assert_eq!(reason, "prior failure"),
        other => panic!("unexpected action: {:?}", other),
    }
    assert!(disc.join("foo.iso").exists());
}

// A loss-abort stays RESUMABLE no matter how many times it recurs — never
// promoted to terminal `.failed` by attempt count. The attempt counter
// still advances on disk so the UI can report it.
#[test]
fn mark_aborted_on_loss_always_resumable() {
    let disc = tmpdir();
    fs::write(disc.join("foo.iso"), b"x").unwrap();

    for expected in 1..=5 {
        let terminal = mark_aborted_on_loss(&disc, "loss exceeds threshold");
        assert!(!terminal, "a loss-abort must never become terminal");
        let (_, attempt) = read_aborted_loss(&disc).expect(".aborted-loss must exist");
        assert_eq!(attempt, expected, "attempt count must advance on disk");
        assert!(
            !disc.join(FAILED_MARKER).exists(),
            "must never write terminal .failed for a loss-abort"
        );
    }
}

/// (a) A `.aborted-loss` marker BELOW the attempt limit is RESUMABLE: the
/// scan emits `ResumeAbortedLoss` (not `AlreadyFailed`), leaves the ISO +
/// marker intact, and does NOT write `.failed`.
#[test]
fn aborted_loss_below_limit_is_resumable() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    fs::write(disc.join("foo.iso.mapfile"), b"x").unwrap();
    // First abort → attempt 1 (< MAX_LOSS_RESUME_ATTEMPTS).
    write_aborted_loss_marker(&disc, "12.50s lost exceeds 0s", 1);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    match &hints[0].action {
        ResumeAction::ResumeAbortedLoss {
            attempt,
            has_iso,
            has_mapfile,
            ..
        } => {
            assert_eq!(*attempt, 1);
            assert!(
                *has_iso && *has_mapfile,
                "ISO + mapfile must be reported intact"
            );
        }
        other => panic!("expected ResumeAbortedLoss, got {other:?}"),
    }
    assert!(disc.join("foo.iso").exists());
    assert_eq!(
        read_state(&disc).unwrap().state,
        StagingState::AbortedLoss,
        "marker left intact for retry"
    );
    assert!(
        !disc.join(FAILED_MARKER).exists(),
        "below limit must NOT be terminal"
    );
}

// (b) Stays RESUMABLE regardless of attempt count — no terminal
// promotion. The old attempt-cap promotion clobbered a complete swept ISO.
#[test]
fn aborted_loss_high_attempt_count_stays_resumable() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    write_aborted_loss_marker(&disc, "12.50s lost exceeds 0s", 99);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    match &hints[0].action {
        ResumeAction::ResumeAbortedLoss {
            attempt, has_iso, ..
        } => {
            assert_eq!(*attempt, 99);
            assert!(*has_iso, "ISO must stay recoverable");
        }
        other => panic!("expected ResumeAbortedLoss, got {other:?}"),
    }
    assert!(
        !disc.join(FAILED_MARKER).exists(),
        "a loss-abort must never be promoted to terminal .failed"
    );
    assert_eq!(
        read_state(&disc).unwrap().state,
        StagingState::AbortedLoss,
        ".aborted-loss must remain for the operator"
    );
}

// (c) A real terminal `.failed` stays terminal — unaffected by the
// abort-loss path; pinned alongside the new variants against regression.
#[test]
fn real_failed_stays_terminal_alongside_abort_loss() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    write_failed_marker(&disc, "cancelled by operator");

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::AlreadyFailed { .. }),
        "a real .failed must stay terminal, got {:?}",
        hints[0].action
    );
}

/// (d) A finished rip (`.done`/`.completed`) is unaffected by the new
/// abort-loss branch — still classified `AlreadyCompleted`.
#[test]
fn done_and_completed_unaffected_by_abort_loss_branch() {
    // .completed → AlreadyCompleted.
    let root1 = tmpdir();
    let d1 = root1.join("DiscA");
    fs::create_dir_all(&d1).unwrap();
    write_marker_durable(&d1.join(COMPLETED_MARKER), b"{}").unwrap();
    let h1 = resume_or_quarantine_staging(root1.to_str().unwrap());
    assert_eq!(h1.len(), 1);
    assert!(
        matches!(h1[0].action, ResumeAction::AlreadyCompleted),
        "got {:?}",
        h1[0].action
    );

    // .done (+ leftover ISO) → AlreadyCompleted (finished, awaiting mover).
    let root2 = tmpdir();
    let d2 = root2.join("DiscB");
    fs::create_dir_all(&d2).unwrap();
    fs::write(d2.join("foo.iso"), b"x").unwrap();
    write_marker_durable(&d2.join(DONE_MARKER), b"{}").unwrap();
    let h2 = resume_or_quarantine_staging(root2.to_str().unwrap());
    assert_eq!(h2.len(), 1);
    assert!(
        matches!(h2[0].action, ResumeAction::AlreadyCompleted),
        "got {:?}",
        h2[0].action
    );
}

// A `.sweeping` dir from a NON-watchdog hard crash lands with
// `.restart_count == 0`. The InProgress carve-out must STILL restart-count
// it so a deterministically-crashing sweep walks toward `.failed`.
#[test]
fn sweeping_in_progress_is_restart_counted() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    // `.sweeping` present, count at 0 (raw kill — no watchdog bump).
    write_sweeping_marker(&disc);
    assert_eq!(restart_count(&disc), 0);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::InProgress),
        "below the limit a .sweeping dir is left in progress, got {:?}",
        hints[0].action
    );
    assert_eq!(
        restart_count(&disc),
        1,
        ".sweeping InProgress skip must bump .restart_count (else a crash loop never escapes)"
    );
    assert!(disc.join("foo.iso").exists());
    assert!(!disc.join(FAILED_MARKER).exists());
}

// Once a `.sweeping` dir's restart count reaches RESTART_LIMIT the
// carve-out must promote it to `.failed` and clear the in-progress marker.
#[test]
fn sweeping_in_progress_fails_after_limit() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("foo.iso"), b"x").unwrap();
    write_sweeping_marker(&disc);
    mutate_state_if_present(&disc, |s| s.restart_count = RESTART_LIMIT);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::RestartLoopFailed { .. }),
        "got {:?}",
        hints[0].action
    );
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);
    // The in-progress marker is cleared on promotion.
    assert_ne!(read_state(&disc).unwrap().state, StagingState::Sweeping);
    assert_eq!(restart_count(&disc), 0);
}

// A terminal write must release the `.muxing` exclusion lock — the cold
// operator-resume path writes terminal markers without the worker's
// MuxingGuard, so a stale lock would block the re-insert path forever.
#[test]
fn terminal_writers_clear_muxing_lock() {
    // .completed clears .muxing.
    let d1 = tmpdir();
    let mut st1 = DiscState::new(StagingState::Ripped);
    st1.muxing = true;
    write_state(&d1, &st1);
    assert!(read_state(&d1).unwrap().muxing);
    write_completed_marker(&d1);
    assert!(
        !read_state(&d1).unwrap().muxing,
        ".completed must clear a leftover .muxing lock"
    );

    // .failed clears .muxing.
    let d2 = tmpdir();
    let mut st2 = DiscState::new(StagingState::Ripped);
    st2.muxing = true;
    write_state(&d2, &st2);
    assert!(read_state(&d2).unwrap().muxing);
    write_failed_marker(&d2, "terminal");
    assert!(
        !read_state(&d2).unwrap().muxing,
        ".failed must clear a leftover .muxing lock"
    );
}

// If the durable `.done` write FAILS, the caller must NOT proceed to
// write `.completed` or clear `.restart_count`, else the dir looks
// terminal-complete while the mover has nothing to act on.
#[test]
fn failed_done_write_leaves_no_completed_and_preserves_restart_count() {
    let disc = tmpdir();

    // Seed a restart counter so we can prove it is NOT cleared.
    increment_restart_count(&disc).unwrap();
    assert_eq!(restart_count(&disc), 1);

    // Force the `.done` write to fail by targeting a path whose parent is
    // a non-existent subdirectory (the durable write can't create the tmp
    // file there). This mirrors a real I/O failure at the marker site.
    let bad_done = disc.join("missing-subdir").join(DONE_MARKER);
    let handoff = write_handoff_marker(&bad_done, b"{}");
    assert!(
        handoff.is_err(),
        "precondition: the hand-off marker write must fail for this test"
    );

    // The fix: on that Err, `rip_disc` returns early — so the following
    // two calls are SKIPPED. We assert the post-state that skipping yields.
    // (We deliberately do NOT call write_completed_marker / clear_restart_count.)

    assert!(
        !disc.join(COMPLETED_MARKER).exists(),
        ".completed must not exist when the .done write failed"
    );
    assert!(
        !disc.join(DONE_MARKER).exists(),
        "no durable .done landed in the staging dir"
    );
    assert_eq!(
        restart_count(&disc),
        1,
        ".restart_count must be preserved (not cleared) when .done failed"
    );
}

// A rename failure must not leak the `.tmp` sibling. Forced by making the
// target path a non-empty directory, which fails rename(2) AFTER the
// tmp file was created + fsynced, exercising cleanup-on-error.
#[test]
fn marker_rename_failure_cleans_up_tmp() {
    let d = tmpdir();
    let target = d.join(".done");
    // Make the target a non-empty directory so rename-over it fails.
    fs::create_dir(&target).unwrap();
    fs::write(target.join("occupant"), b"x").unwrap();

    let res = write_marker_durable(&target, b"{}");
    assert!(
        res.is_err(),
        "precondition: rename onto a non-empty dir must fail"
    );

    let tmp = d.join(".done.tmp");
    assert!(
        !tmp.exists(),
        "the .tmp sibling must be cleaned up after a rename failure, found: {}",
        tmp.display()
    );
}

#[test]
fn marker_directory_sync_failure_is_reported_after_atomic_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.json");
    fs::write(&path, b"old").unwrap();
    let error = write_marker_with_directory_sync(&path, b"new", |parent| {
        assert_eq!(parent, dir.path());
        Err(io::Error::from(io::ErrorKind::StorageFull))
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    assert_eq!(
        fs::read(&path).unwrap(),
        b"new",
        "failure does not imply rollback"
    );
    assert!(!dir.path().join("journal.json.tmp").exists());
}

#[test]
fn marker_phased_write_distinguishes_rename_and_directory_sync_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("marker");
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(
        write_marker_phased(&path, b"new", |_| panic!("sync after failed rename")),
        Err(MarkerWriteError::BeforePublish(_))
    ));
    assert!(!dir.path().join("marker.tmp").exists());
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, b"old").unwrap();
    let error = write_marker_phased(&path, b"new", |_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "injected directory sync",
        ))
    })
    .unwrap_err();
    assert!(
        matches!(error, MarkerWriteError::AfterPublish(ref e) if e.kind() == std::io::ErrorKind::StorageFull)
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    assert!(!dir.path().join("marker.tmp").exists());
}

// Exhaustive resume-on-startup classifier matrix (rc4 hardening): drives
// `resume_or_quarantine_staging` over every marker/artifact/restart_count
// combination, asserting the resulting `ResumeAction` or silent wipe/skip.

#[derive(Clone, Copy)]
enum Mk {
    Completed,
    Failed,
    Done,
    Review,
    Ripped,
    Sweeping,
    Muxing,
    Iso,
    Mapfile,
    Mkv,
    RestartAtLimit,
    RestartBelowLimit,
    /// A non-JSON `.failed` body (e.g. review.rs's operator-cancel). Used
    /// to pin that terminal-ness keys on marker PRESENCE, not parse.
    FailedNonJson,
}

/// What `resume_or_quarantine_staging` must decide for one disc dir.
#[derive(Debug, PartialEq)]
enum Verdict {
    Completed,
    Failed,
    RestartLoopFailed,
    ResumePreserved,
    /// Dir carries a resumable `.aborted-loss` below the attempt limit.
    ResumeAbortedLoss,
    /// Dir is owned/in progress (`.sweeping`/`.muxing`) — left alone, not
    /// restart-counted.
    InProgress,
    Wiped,
}

fn resume_verdict(markers: &[Mk]) -> Verdict {
    let root = tmpdir();
    let disc = root.join("Disc");
    fs::create_dir_all(&disc).unwrap();
    for m in markers {
        match m {
            Mk::Completed => write_completed_marker(&disc),
            Mk::Failed => {
                let _ = write_failed_marker(&disc, "prior failure");
            }
            Mk::Done => fs::write(disc.join(DONE_MARKER), b"{}").unwrap(),
            Mk::Review => fs::write(disc.join(REVIEW_MARKER), b"{}").unwrap(),
            Mk::Ripped => fs::write(disc.join(RIPPED_MARKER), b"{}").unwrap(),
            Mk::Sweeping => fs::write(disc.join(SWEEPING_MARKER), b"{}").unwrap(),
            Mk::Muxing => fs::write(disc.join(MUXING_MARKER), b"{}").unwrap(),
            Mk::FailedNonJson => {
                // Mimic the legacy review.rs body: a non-JSON `.failed`.
                fs::write(disc.join(FAILED_MARKER), b"cancelled by operator\n").unwrap()
            }
            Mk::Iso => fs::write(disc.join("Disc.iso"), b"x").unwrap(),
            Mk::Mapfile => fs::write(disc.join("Disc.iso.mapfile"), b"x").unwrap(),
            Mk::Mkv => fs::write(disc.join("Disc.mkv"), b"x").unwrap(),
            Mk::RestartAtLimit => fs::write(
                disc.join(RESTART_COUNT_FILE),
                format!("{}\n", RESTART_LIMIT).as_bytes(),
            )
            .unwrap(),
            Mk::RestartBelowLimit => fs::write(
                disc.join(RESTART_COUNT_FILE),
                format!("{}\n", RESTART_LIMIT - 1).as_bytes(),
            )
            .unwrap(),
        }
    }
    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    if hints.is_empty() {
        // No hint: wiped (empty/junk) or skipped (UNKNOWN); these local-FS
        // rows are never UNKNOWN, so the dir must really be gone.
        assert!(!disc.exists(), "a no-hint dir must have been wiped");
        return Verdict::Wiped;
    }
    assert!(disc.exists(), "a dir with a hint must be kept");
    assert_eq!(hints.len(), 1, "expected exactly one disc dir");
    match &hints[0].action {
        ResumeAction::AlreadyCompleted => Verdict::Completed,
        ResumeAction::AlreadyFailed { .. } => Verdict::Failed,
        ResumeAction::RestartLoopFailed { .. } => Verdict::RestartLoopFailed,
        ResumeAction::ResumePreserved { .. } => Verdict::ResumePreserved,
        ResumeAction::ResumeAbortedLoss { .. } => Verdict::ResumeAbortedLoss,
        ResumeAction::InProgress => Verdict::InProgress,
        ResumeAction::HeldUnreadableState { .. } => panic!("matrix never corrupts state.json"),
    }
}

#[test]
fn resume_classifier_matrix() {
    use Mk::*;
    let table: &[(&[Mk], Verdict, &str)] = &[
        // --- empty / junk: wiped ---
        (
            &[],
            Verdict::Wiped,
            "empty dir, no markers/artifacts → wipe",
        ),
        // --- .completed: terminal, leave for mover ---
        (&[Completed], Verdict::Completed, ".completed alone"),
        (
            &[Completed, Mkv],
            Verdict::Completed,
            ".completed with output",
        ),
        (
            &[Completed, Iso, Mapfile],
            Verdict::Completed,
            ".completed with leftover ISO",
        ),
        // --- .failed: terminal, leave for operator. Checked BEFORE the
        //     .done/.review carve-outs and the partial-state branch. ---
        (&[Failed], Verdict::Failed, ".failed alone"),
        (
            &[Failed, Iso],
            Verdict::Failed,
            ".failed with partial ISO still present",
        ),
        (
            &[Failed, Iso, Mapfile, RestartAtLimit],
            Verdict::Failed,
            ".failed wins even at the restart limit (terminal precedence)",
        ),
        // --- .done carve-out: crash between .done and .completed.
        //     Finished rip awaiting mover; must NOT be restart-counted. ---
        (
            &[Done],
            Verdict::Completed,
            ".done alone → AlreadyCompleted",
        ),
        (
            &[Done, Iso, Mapfile],
            Verdict::Completed,
            "CRASH WINDOW: .done + ISO + mapfile, no .completed → finished, not retried",
        ),
        (
            &[Done, Iso, Mapfile, RestartAtLimit],
            Verdict::Completed,
            ".done short-circuits even with restart_count at limit (must not become .failed)",
        ),
        // --- .review carve-out: same crash-window reasoning ---
        (
            &[Review],
            Verdict::Completed,
            ".review alone → AlreadyCompleted",
        ),
        (
            &[Review, Iso, Mapfile, Mkv],
            Verdict::Completed,
            "CRASH WINDOW: .review + artifacts, no .completed → finished/held, not retried",
        ),
        // --- partial state, no terminal marker, below limit → preserve+bump ---
        (
            &[Iso],
            Verdict::ResumePreserved,
            "ISO only → partial, preserve",
        ),
        (
            &[Iso, Mapfile],
            Verdict::ResumePreserved,
            "ISO+mapfile → partial, preserve",
        ),
        (
            &[Mapfile],
            Verdict::ResumePreserved,
            "mapfile only → partial, preserve",
        ),
        (
            &[Mkv],
            Verdict::ResumePreserved,
            "partial MKV only → partial, preserve",
        ),
        (
            &[Iso, Mapfile, RestartBelowLimit],
            Verdict::ResumePreserved,
            "partial below limit → preserve + bump",
        ),
        // --- partial state AT the restart limit → promote to .failed ---
        (
            &[Iso, RestartAtLimit],
            Verdict::RestartLoopFailed,
            "partial at RESTART_LIMIT → quarantine (.failed)",
        ),
        (
            &[Iso, Mapfile, RestartAtLimit],
            Verdict::RestartLoopFailed,
            "partial at limit with full ISO+mapfile → quarantine",
        ),
        // --- ISO present but no mapfile: still partial (the resume CLASSIFIER
        //     downstream rejects it as not-eligible, but the staging scan still
        //     preserves it as partial state to resume the sweep). ---
        (
            &[Iso, RestartBelowLimit],
            Verdict::ResumePreserved,
            "ISO + no mapfile → partial, preserve (classify_resume later rejects remux)",
        ),
        // --- .ripped-only, no artifacts: not partial state, wiped as junk;
        //     the mux worker (separate tick) is what acts on .ripped. ---
        (
            &[Ripped],
            Verdict::Wiped,
            ".ripped with no ISO/mapfile/MKV is not partial state to the resume scan → wiped",
        ),
        // --- .ripped alongside real artifacts: partial state, preserved ---
        (
            &[Ripped, Iso, Mapfile],
            Verdict::ResumePreserved,
            ".ripped + artifacts → partial state preserved (mux worker handles the .ripped)",
        ),
        // --- H2/M1: .sweeping in-progress marker, verdict InProgress —
        //     `.restart_count` IS bumped each skip so a wedge converges. ---
        (
            &[Sweeping],
            Verdict::InProgress,
            ".sweeping alone → owned/in-progress, leave alone",
        ),
        (
            &[Sweeping, Iso, Mapfile],
            Verdict::InProgress,
            "CRASH MID-SWEEP: .sweeping + artifacts → in-progress, not partial state",
        ),
        // R2 finding 1: below the limit a healthy sweep stays InProgress;
        // a sweep wedged RESTART_LIMIT times must promote to `.failed`,
        // else the carve-out defeats the watchdog's restart-loop guard.
        (
            &[Sweeping, Iso, Mapfile, RestartBelowLimit],
            Verdict::InProgress,
            ".sweeping below limit → healthy long rip, leave alone",
        ),
        (
            &[Sweeping, Iso, Mapfile, RestartAtLimit],
            Verdict::RestartLoopFailed,
            ".sweeping AT restart limit → deterministic wedge, quarantine (honors watchdog guard)",
        ),
        // --- H1: .muxing exclusion lock. Owned by the mux worker; same
        //     in-progress treatment as .sweeping. ---
        (
            &[Muxing, Iso, Mapfile],
            Verdict::InProgress,
            ".muxing + artifacts → mux worker owns it, in-progress",
        ),
        (
            &[Muxing, Iso, Mapfile, RestartBelowLimit],
            Verdict::InProgress,
            ".muxing below limit → mux worker owns it, leave alone",
        ),
        (
            &[Muxing, Iso, Mapfile, RestartAtLimit],
            Verdict::RestartLoopFailed,
            ".muxing AT restart limit → deterministically-wedging mux, quarantine",
        ),
        // --- M2: a non-JSON `.failed` body (review.rs operator-cancel)
        //     must still be TERMINAL — keyed on marker presence, not
        //     parse-success. ---
        (
            &[FailedNonJson],
            Verdict::Failed,
            "non-JSON .failed body is still terminal (presence-keyed)",
        ),
        (
            &[FailedNonJson, Iso, Mapfile, RestartAtLimit],
            Verdict::Failed,
            "non-JSON .failed + artifacts at restart limit → terminal, not restart-counted",
        ),
    ];
    for (markers, expected, why) in table {
        let got = resume_verdict(markers);
        assert_eq!(&got, expected, "resume matrix row failed: {why}");
    }
}

/// Named explicit cells (per the rc4 brief).
#[test]
fn resume_restart_count_at_limit_quarantines() {
    assert_eq!(
        resume_verdict(&[Mk::Iso, Mk::RestartAtLimit]),
        Verdict::RestartLoopFailed
    );
}
#[test]
fn resume_completed_plus_failed_conflict_is_terminal() {
    // Writing both .completed and .failed migrates to a single state; the
    // migration priority makes Failed win. Either way the dir is terminal —
    // the key property is that it is NEVER re-ripped. Pin Failed.
    assert_eq!(
        resume_verdict(&[Mk::Completed, Mk::Failed]),
        Verdict::Failed,
        "a conflicting .completed + .failed pair collapses to a single terminal state (Failed wins); never re-ripped"
    );
}
#[test]
fn resume_done_only_crash_window_treated_finished() {
    assert_eq!(
        resume_verdict(&[Mk::Done, Mk::Iso, Mk::Mapfile, Mk::RestartAtLimit]),
        Verdict::Completed,
        "a .done crash-window dir must be finished, never promoted to .failed by the restart gate"
    );
}
#[test]
fn resume_nothing_present_is_wiped() {
    assert_eq!(resume_verdict(&[]), Verdict::Wiped);
}

// A dir with `.sweeping` + ISO/mapfile (crash mid-sweep) is classified
// InProgress and left in place, but ALSO restart-counted on each skip so
// a deterministically-crashing sweep walks toward `.failed`.
#[test]
fn sweeping_marker_is_in_progress_and_restart_counted() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
    write_sweeping_marker(&disc);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::InProgress),
        "got {:?}",
        hints[0].action
    );
    // R3 finding 2: counter is bumped on the InProgress skip (was 0 before).
    assert_eq!(
        restart_count(&disc),
        1,
        ".sweeping InProgress skip must bump .restart_count"
    );
    assert!(!disc.join(FAILED_MARKER).exists());
    // Artifacts + marker preserved for the resuming rip.
    assert!(disc.join("MyDisc.iso").exists());
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Sweeping);
}

// A STRUCTURAL mux failure in the inline fallback must quarantine the
// dir, not leak `.sweeping`: `.failed` present, `.sweeping` gone, and it
// classifies terminal AlreadyFailed, not stranded InProgress.
#[test]
fn structural_mux_failure_quarantines_instead_of_stranding() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
    // Pre-state: `.sweeping` was written at staging-dir creation and the
    // inline-mux fallback is only reached because the `.ripped` hand-off
    // write failed, so `.sweeping` is still on disk here.
    write_sweeping_marker(&disc);

    // The fix's quarantine sequence (mirrors mod.rs's ISO-open /
    // build_iso_pipeline Err arms).
    write_failed_marker(&disc, "cannot open ISO for mux: ENOENT");
    clear_restart_count(&disc);

    // `.sweeping` superseded by `.failed`.
    assert_ne!(read_state(&disc).unwrap().state, StagingState::Sweeping);
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::AlreadyFailed { .. }),
        "structural mux failure must be terminal AlreadyFailed, got {:?}",
        hints[0].action
    );
}

// A `.muxing` lock dir is owned by the mux worker — the resume scan
// leaves it InProgress but restart-counts it too, so a hard kill mid-mux
// still walks toward `.failed` over RESTART_LIMIT restarts.
#[test]
fn muxing_marker_is_in_progress_and_restart_counted() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
    let mut st = DiscState::new(StagingState::Ripped);
    st.muxing = true;
    write_state(&disc, &st);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(matches!(hints[0].action, ResumeAction::InProgress));
    assert_eq!(restart_count(&disc), 1);
    assert_ne!(read_state(&disc).unwrap().state, StagingState::Failed);
}

// A wedging mux killed by the hard watchdog re-acquires `.muxing` with
// `.restart_count` bumped. The carve-out must promote to `.failed` at
// RESTART_LIMIT, not re-dispatch and re-wedge forever.
#[test]
fn muxing_at_restart_limit_is_promoted_to_failed() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
    // Watchdog has crashed RESTART_LIMIT times; the mux worker owns the dir
    // (muxing lock) and the count is on disk.
    let mut st = DiscState::new(StagingState::Ripped);
    st.muxing = true;
    st.restart_count = RESTART_LIMIT;
    write_state(&disc, &st);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::RestartLoopFailed { .. }),
        "wedging .muxing at the restart limit must be promoted to .failed, got {:?}",
        hints[0].action
    );
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);
    // The lock is cleared so the dir reads terminal, not owned, next pass.
    assert!(!read_state(&disc).unwrap().muxing);
    // Count cleared so a manual re-queue starts fresh.
    assert_eq!(restart_count(&disc), 0);
}

/// Convergence R2 finding 1 companion: the `.sweeping` inline-mux path has
/// the same loop. A `.sweeping` dir whose `.restart_count` already reached
/// RESTART_LIMIT must be quarantined, with `.sweeping` cleared.
#[test]
fn sweeping_at_restart_limit_is_promoted_to_failed() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
    write_sweeping_marker(&disc);
    mutate_state_if_present(&disc, |s| s.restart_count = RESTART_LIMIT);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::RestartLoopFailed { .. }),
        "wedging .sweeping at the restart limit must be promoted to .failed, got {:?}",
        hints[0].action
    );
    assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);
    assert_ne!(read_state(&disc).unwrap().state, StagingState::Sweeping);
    assert_eq!(restart_count(&disc), 0);
}

// A restart of an owned `.muxing` dir whose count is BELOW the limit
// stays InProgress and preserved, but the scan bumps the counter on the
// skip, so it advances to RESTART_LIMIT and fails on the NEXT restart.
#[test]
fn muxing_below_restart_limit_stays_in_progress() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
    let mut st = DiscState::new(StagingState::Ripped);
    st.muxing = true;
    st.restart_count = RESTART_LIMIT - 1;
    write_state(&disc, &st);

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::InProgress),
        "below-limit .muxing must stay InProgress, got {:?}",
        hints[0].action
    );
    assert_ne!(read_state(&disc).unwrap().state, StagingState::Failed);
    assert!(read_state(&disc).unwrap().muxing);
    // R3 finding 2: the scan bumps the counter on the InProgress skip — the
    // dir advances to the limit and fails on the next restart.
    assert_eq!(restart_count(&disc), RESTART_LIMIT);
}

// The `.sweeping` marker is superseded by every terminal/hand-off
// transition, so a finished/quarantined dir isn't mis-read as active.
#[test]
fn sweeping_marker_cleared_by_terminal_writes() {
    let d = tmpdir();
    write_sweeping_marker(&d);
    assert_eq!(read_state(&d).unwrap().state, StagingState::Sweeping);
    write_completed_marker(&d);
    assert_ne!(
        read_state(&d).unwrap().state,
        StagingState::Sweeping,
        ".completed must clear .sweeping"
    );

    let d2 = tmpdir();
    write_sweeping_marker(&d2);
    write_failed_marker(&d2, "boom");
    assert_ne!(
        read_state(&d2).unwrap().state,
        StagingState::Sweeping,
        ".failed must clear .sweeping"
    );

    let d3 = tmpdir();
    write_sweeping_marker(&d3);
    clear_sweeping_marker(&d3);
    // Stopping releases ownership without discarding identity or metadata.
    assert_eq!(read_state(&d3).unwrap().state, StagingState::Stopped);
    // Idempotent: clearing an already-gone marker must not panic/error.
    clear_sweeping_marker(&d3);
}

// A `.failed`-only dir with a non-JSON body is still terminal to the
// resume scan — it keys on `has_failed` presence, not parse-success.
#[test]
fn non_json_failed_is_terminal() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join(FAILED_MARKER), b"cancelled by operator\n").unwrap();
    // The parser can't read a reason out of it...
    assert_eq!(read_failed_reason(&disc), None);
    // ...but the scan still treats it as terminal.
    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert_eq!(hints.len(), 1);
    assert!(
        matches!(hints[0].action, ResumeAction::AlreadyFailed { .. }),
        "got {:?}",
        hints[0].action
    );
    // And the snapshot exposes has_failed even with no parseable reason.
    let snap = snapshot_staging_disc(&disc).unwrap();
    assert!(snap.has_failed);
    assert!(snap.failed_reason.is_none());
}

/// Same guarantee for `increment_restart_count`: a rename failure must not
/// leak `.restart_count.tmp`.
#[test]
fn restart_count_rename_failure_cleans_up_tmp() {
    let d = tmpdir();
    // Make the final target a non-empty directory so rename-over fails.
    let target = d.join(RESTART_COUNT_FILE);
    fs::create_dir(&target).unwrap();
    fs::write(target.join("occupant"), b"x").unwrap();

    let res = increment_restart_count(&d);
    assert!(
        res.is_err(),
        "precondition: rename onto a non-empty dir must fail"
    );

    let tmp = d.join(format!("{}.tmp", RESTART_COUNT_FILE));
    assert!(
        !tmp.exists(),
        "the .tmp sibling must be cleaned up after a rename failure, found: {}",
        tmp.display()
    );
}

/// Make the next `state.json` write in `dir` fail: its `.tmp` sibling is a dir.
fn block_state_write(dir: &Path) {
    fs::create_dir_all(dir.join("state.json.tmp")).unwrap();
}

#[test]
fn increment_restart_count_reports_a_failed_state_write() {
    let disc = tmpdir();
    let mut st = DiscState::new(StagingState::Sweeping);
    st.restart_count = 1;
    write_state(&disc, &st);
    block_state_write(&disc);

    assert!(
        increment_restart_count(&disc).is_err(),
        "a bump that did not persist must not report success"
    );
    assert_eq!(restart_count(&disc), 1);
}

#[test]
fn legacy_upgrade_keeps_markers_when_state_write_fails() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join(FAILED_MARKER), br#"{"reason":"prior failure"}"#).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    block_state_write(&disc);

    let snap = snapshot_staging_disc(&disc).unwrap();
    assert!(snap.has_failed);
    assert!(
        disc.join(FAILED_MARKER).exists(),
        "the only lifecycle record must survive a failed upgrade"
    );
    assert!(read_state(&disc).is_none());

    // Once the mount recovers, the next scan upgrades it.
    fs::remove_dir(disc.join("state.json.tmp")).unwrap();
    snapshot_staging_disc(&disc).unwrap();
    let st = read_state(&disc).unwrap();
    assert_eq!(st.state, StagingState::Failed);
    assert_eq!(st.failure_reason.as_deref(), Some("prior failure"));
    assert!(!disc.join(FAILED_MARKER).exists());
}

#[test]
fn legacy_upgrade_carries_aborted_loss_and_annotations() {
    let disc = tmpdir();
    fs::write(
        disc.join(ABORTED_LOSS_MARKER),
        br#"{"reason":"lost 9s","attempt":2}"#,
    )
    .unwrap();
    fs::write(disc.join(ACCEPT_LOSS_MARKER), b"{}").unwrap();
    fs::write(disc.join(MUXING_MARKER), b"{}").unwrap();
    fs::write(disc.join(RESTART_COUNT_FILE), b"2\n").unwrap();

    snapshot_staging_disc(&disc).unwrap();
    let st = read_state(&disc).expect("legacy dir upgraded");
    assert_eq!(st.state, StagingState::AbortedLoss);
    assert_eq!(st.failure_reason.as_deref(), Some("lost 9s"));
    assert_eq!(st.aborted_loss_attempt, 2);
    assert!(st.accept_loss, "the operator's accept-loss must carry over");
    assert!(st.muxing);
    assert_eq!(st.restart_count, 2);
}

#[test]
fn aborted_loss_attempt_climbs_across_another_pass() {
    let disc = tmpdir();
    write_state(&disc, &DiscState::new(StagingState::Ripped));
    assert!(mark_aborted_on_loss_reporting_landed(&disc, "lossy"));
    assert_eq!(read_aborted_loss(&disc).unwrap().1, 1);

    // Run another pass: the dir goes back to Sweeping, then aborts again.
    write_sweeping_marker(&disc);
    assert!(mark_aborted_on_loss_reporting_landed(&disc, "lossy"));
    assert_eq!(read_aborted_loss(&disc).unwrap().1, 2);

    mutate_state_if_present(&disc, |s| s.state = StagingState::Ripped);
    assert!(!mark_aborted_on_loss(&disc, "lossy"));
    assert_eq!(read_aborted_loss(&disc).unwrap().1, 3);
}

#[test]
fn live_rip_seed_drops_the_prior_attempts_one_shots() {
    let disc = tmpdir();
    let mut st = DiscState::new(StagingState::AbortedLoss);
    st.accept_loss = true;
    st.needs_disc = true;
    st.aborted_loss_attempt = 2;
    st.outputs = vec![Output {
        filename: "ep1.mkv".into(),
        ..Default::default()
    }];
    write_state(&disc, &st);

    start_sweep(&disc, |_| {}).unwrap();
    let st = read_state(&disc).unwrap();
    assert_eq!(st.state, StagingState::Sweeping);
    assert!(
        !st.accept_loss,
        "a stale accept-loss must not cover a new rip"
    );
    assert!(!st.needs_disc, "the live rip has the disc");
    assert_eq!(st.aborted_loss_attempt, 2, "the attempt count is kept");
    assert_eq!(st.outputs.len(), 1, "the plan is kept");
}

#[test]
fn muxing_status_fails_closed_on_corrupt_or_foreign_state() {
    let disc = tmpdir();
    fs::write(disc.join(STATE_FILE), b"{ torn").unwrap();
    assert!(muxing_status(&disc).is_err(), "unparseable state.json");

    let mut v = serde_json::to_value(DiscState::new(StagingState::Ripped)).unwrap();
    v["schema"] = serde_json::json!(1);
    fs::write(disc.join(STATE_FILE), v.to_string()).unwrap();
    assert!(muxing_status(&disc).is_err(), "foreign-schema state.json");
    fs::remove_file(disc.join(STATE_FILE)).unwrap();

    let mut st = DiscState::new(StagingState::Ripped);
    st.muxing = true;
    write_state(&disc, &st);
    assert!(muxing_status(&disc).unwrap());
}

#[test]
fn restart_loop_quarantine_write_failure_keeps_the_count() {
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    fs::write(
        disc.join(RESTART_COUNT_FILE),
        format!("{RESTART_LIMIT}\n").as_bytes(),
    )
    .unwrap();
    block_state_write(&disc);

    resume_or_quarantine_staging(root.to_str().unwrap());
    assert!(read_state(&disc).is_none(), "the quarantine did not land");
    assert_eq!(
        restart_count(&disc),
        RESTART_LIMIT,
        "the count must survive so the next start retries the quarantine"
    );
}

#[test]
fn resume_scan_never_wipes_files_the_ripper_did_not_write() {
    let root = tmpdir();
    let foreign = root.join("Some Movie");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("movie.mp4"), b"x").unwrap();
    let junk = root.join("Abandoned");
    fs::create_dir_all(&junk).unwrap();
    fs::write(junk.join(DISC_LABEL_FILE), b"LABEL").unwrap();

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    assert!(hints.is_empty());
    assert!(
        foreign.join("movie.mp4").exists(),
        "foreign content is kept"
    );
    assert!(!junk.exists(), "a dir of only ripper bookkeeping is wiped");
}

#[cfg(unix)]
#[test]
fn unknown_contents_dir_is_skipped_by_the_resume_scan() {
    use std::os::unix::fs::PermissionsExt;
    let root = tmpdir();
    let disc = root.join("MyDisc");
    fs::create_dir_all(&disc).unwrap();
    fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
    fs::write(disc.join(RESTART_COUNT_FILE), b"1\n").unwrap();
    // Write+search but no read: entries are reachable, the listing is not.
    fs::set_permissions(&disc, fs::Permissions::from_mode(0o300)).unwrap();
    if fs::read_dir(&disc).is_ok() {
        // Running as root: the listing can't be denied, so nothing to test.
        fs::set_permissions(&disc, fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }

    let hints = resume_or_quarantine_staging(root.to_str().unwrap());
    fs::set_permissions(&disc, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(hints.is_empty(), "an unknown dir gets no verdict");
    assert!(disc.join("MyDisc.iso").exists(), "never wiped");
    assert_eq!(restart_count(&disc), 1, "never restart-counted");
}

#[test]
fn staging_free_bytes_reads_a_real_dir_and_none_for_missing() {
    let dir = tmpdir();
    let free = staging_free_bytes(dir.to_str().unwrap());
    assert!(free.is_some_and(|b| b > 0), "got {free:?}");
    assert_eq!(
        staging_free_bytes(dir.join("missing").to_str().unwrap()),
        None
    );
}

#[test]
fn ripped_marker_round_trips_through_disc_state() {
    let m = crate::server::muxer::RippedMarker {
        schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
        iso_path: "/staging/D/D.iso".into(),
        mapfile_path: "/staging/D/D.iso.mapfile".into(),
        display_name: "Title".into(),
        disc_format: "uhd".into(),
        mkv_filename: "Title.mkv".into(),
        tmdb_title: "Title".into(),
        tmdb_year: 1999,
        tmdb_poster: "poster".into(),
        tmdb_overview: "overview".into(),
        tmdb_media_type: crate::server::planner::MediaKind::Tv,
        max_retries: 4,
        abort_on_lost_secs: 7,
        rip_elapsed_secs: 1.5,
        rip_errors: 3,
        rip_lost_video_secs: 2.5,
        rip_last_sector: 42,
        origin_device: "sg3".into(),
        sweep_errors: 5,
        sweep_total_lost_ms: 11.0,
        sweep_main_lost_ms: 13.0,
        sweep_num_bad_ranges: 17,
        sweep_largest_gap_ms: 19.0,
        title_confident: true,
    };
    let mut st = DiscState::new(StagingState::Ripped);
    st.apply_ripped(&m);
    assert_eq!(
        serde_json::to_value(st.to_ripped_marker()).unwrap(),
        serde_json::to_value(&m).unwrap()
    );
}
#[test]
fn singlepass_empty_outputs_refuse_plan_change_from_persisted_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mut running = super::DiscState::new(super::StagingState::Sweeping);
    running.media_type = "movie".into();
    running.max_retries = 0;
    super::try_write_state(dir.path(), &running).unwrap();
    let result = super::save_user_metadata(
        dir.path(),
        super::UserMetadata {
            episode_start: None,
            title: "Show".into(),
            year: 2020,
            media_type: crate::server::planner::MediaKind::Tv,
            tmdb_id: 10,
            poster_url: String::new(),
            overview: String::new(),
        },
    );
    assert!(
        result.is_err(),
        "active single-pass has no ISO even before outputs exist"
    );
    assert_eq!(super::read_state(dir.path()).unwrap(), running);
}

#[test]
fn operator_metadata_survives_stale_worker_snapshot_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut worker = super::DiscState::new(super::StagingState::Sweeping);
    worker.title = "Castaway".into();
    super::try_write_state(dir.path(), &worker).unwrap();
    super::save_user_metadata(
        dir.path(),
        super::UserMetadata {
            episode_start: None,
            title: "Cast Away".into(),
            year: 2000,
            media_type: crate::server::planner::MediaKind::Movie,
            tmdb_id: 8358,
            poster_url: String::new(),
            overview: "Chosen movie".into(),
        },
    )
    .unwrap();
    worker.state = super::StagingState::Ripped;
    super::try_write_state(dir.path(), &worker).unwrap();
    let restored = super::read_state(dir.path()).unwrap();
    assert_eq!(restored.title, "Cast Away");
    assert_eq!(restored.tmdb_id, 8358);
    assert_eq!(restored.state, super::StagingState::Ripped);
    assert!(restored.title_confident);
}

#[test]
fn persisted_mode_guard_allows_idle_selection_and_multipass_replanning() {
    for (state, retries, media_type, current_id, new_id) in [
        (super::StagingState::Stopped, 0, "movie", 0, 10),
        (super::StagingState::Sweeping, 3, "movie", 0, 10),
        (super::StagingState::Sweeping, 0, "tv", 10, 10),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut saved = super::DiscState::new(state);
        saved.max_retries = retries;
        saved.media_type = media_type.into();
        saved.tmdb_id = current_id;
        super::try_write_state(dir.path(), &saved).unwrap();
        super::save_user_metadata(
            dir.path(),
            super::UserMetadata {
                episode_start: None,
                title: "Selected".into(),
                year: 2020,
                media_type: crate::server::planner::MediaKind::Tv,
                tmdb_id: new_id,
                poster_url: String::new(),
                overview: String::new(),
            },
        )
        .unwrap();
        assert_eq!(super::read_state(dir.path()).unwrap().title, "Selected");
    }
}
#[test]
fn a_metadata_edit_while_mux_finishes_holds_the_old_plan() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    let mut initial = super::DiscState::new(super::StagingState::Ripped);
    initial.media_type = "tv".into();
    initial.tmdb_id = 1;
    initial.max_retries = 3;
    initial.iso_path = dir.join("show.iso").to_string_lossy().into_owned();
    fs::write(&initial.iso_path, b"retained capture").unwrap();
    initial.outputs.push(super::Output {
        filename: "old-episode.mkv".into(),
        episode: Some(1),
        ..Default::default()
    });
    super::try_write_state(dir, &initial).unwrap();
    let paused = std::sync::Barrier::new(2);
    let resume = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            super::mark_handoff(dir, true, |stale| {
                paused.wait();
                resume.wait();
                stale.title = "Old show".into();
                stale.tmdb_id = 1;
            })
            .unwrap();
        });
        paused.wait();
        super::save_user_metadata(
            dir,
            super::UserMetadata {
                episode_start: None,
                title: "Correct show".into(),
                year: 2020,
                media_type: crate::server::planner::MediaKind::Tv,
                tmdb_id: 2,
                poster_url: String::new(),
                overview: String::new(),
            },
        )
        .unwrap();
        resume.wait();
        worker.join().unwrap();
    });
    let saved = super::read_state(dir).unwrap();
    assert_eq!(saved.state, super::StagingState::Ripped);
    assert!(saved.replan_required);
    assert_eq!(saved.tmdb_id, 2);
    assert_eq!(saved.title, "Correct show");
    assert_eq!(saved.metadata_revision, 1);
}

#[test]
fn a_correction_committed_during_the_iso_prune_check_keeps_the_iso() {
    let dir = tempfile::tempdir().unwrap();
    try_write_state(dir.path(), &DiscState::new(StagingState::Review)).unwrap();
    let lock = state_lock(dir.path());
    let held = lock.lock().unwrap();
    let path = dir.path().to_path_buf();
    let pruner = std::thread::spawn(move || prune_unless_replan_required(&path, &[]).is_some());
    std::thread::sleep(std::time::Duration::from_millis(200));
    // A correction lands (under the state lock) while the prune is deciding.
    let mut st = read_state(dir.path()).unwrap();
    st.replan_required = true;
    write_state_unlocked(dir.path(), &st).unwrap();
    drop(held);
    assert!(
        !pruner.join().unwrap(),
        "the replan's ISO must not be pruned"
    );
}

// After the intermediate ISO was pruned, a correction that needs a replan cannot be
// honoured: it is refused with the reason (job stays held), never flagged for a
// replan from an image that no longer exists.
#[test]
fn a_correction_needing_a_pruned_iso_is_refused_and_the_job_held() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = DiscState::new(StagingState::Review);
    st.media_type = "movie".into();
    st.title = "Castaway".into();
    st.max_retries = 1;
    st.iso_path = dir
        .path()
        .join("Castaway.iso")
        .to_string_lossy()
        .into_owned();
    st.mapfile_path = dir
        .path()
        .join("Castaway.iso.mapfile")
        .to_string_lossy()
        .into_owned();
    st.outputs = vec![Output {
        filename: "Castaway.mkv".into(),
        ..Default::default()
    }];
    try_write_state(dir.path(), &st).unwrap();
    fs::write(dir.path().join("Castaway.mkv"), b"muxed").unwrap();
    let as_tv = UserMetadata {
        episode_start: None,
        title: "Castaway Show".into(),
        year: 2000,
        media_type: crate::server::planner::MediaKind::Tv,
        tmdb_id: 77,
        poster_url: String::new(),
        overview: String::new(),
    };
    let refused = save_review_metadata(dir.path(), as_tv.clone()).unwrap_err();
    assert!(refused.to_string().contains("disc image"), "{refused}");
    let held = read_state(dir.path()).unwrap();
    assert_eq!(held.state, StagingState::Review);
    assert!(!held.replan_required);
    assert!(held.user_metadata.is_none());
    assert_eq!(held.metadata_revision, 0);
    // A correction that keeps the plan needs no image and is accepted.
    let same_plan = UserMetadata {
        title: "Cast Away".into(),
        media_type: crate::server::planner::MediaKind::Movie,
        ..as_tv.clone()
    };
    save_review_metadata(dir.path(), same_plan).unwrap();
    assert!(!read_state(dir.path()).unwrap().replan_required);
    // With the image retained the replan is accepted as before.
    let kept = tempfile::tempdir().unwrap();
    let mut st = st.clone();
    st.iso_path = kept
        .path()
        .join("Castaway.iso")
        .to_string_lossy()
        .into_owned();
    st.mapfile_path = kept
        .path()
        .join("Castaway.iso.mapfile")
        .to_string_lossy()
        .into_owned();
    try_write_state(kept.path(), &st).unwrap();
    fs::write(&st.iso_path, b"image").unwrap();
    fs::write(&st.mapfile_path, b"map").unwrap();
    save_review_metadata(kept.path(), as_tv).unwrap();
    assert!(read_state(kept.path()).unwrap().replan_required);
}

#[test]
fn state_json_media_type_round_trips_old_values() {
    use crate::server::planner::MediaKind;
    let mut v = serde_json::to_value(DiscState::new(StagingState::Stopped)).unwrap();
    for (raw, want) in [
        (Some("tv"), MediaKind::Tv),
        (Some("movie"), MediaKind::Movie),
        (Some(""), MediaKind::Movie),
        (None, MediaKind::Movie),
    ] {
        match raw {
            Some(r) => v["media_type"] = serde_json::json!(r),
            None => {
                v.as_object_mut().unwrap().remove("media_type");
            }
        }
        let st: DiscState = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(st.media_type, want, "{raw:?}");
    }
    let mut st = DiscState::new(StagingState::Stopped);
    st.media_type = MediaKind::Tv;
    assert_eq!(serde_json::to_value(&st).unwrap()["media_type"], "tv");
}

// A Start over interrupted by a restart is finished at startup (and by Clear staging or a
// drive's access), instead of leaving the job invisible to every guard and the mux queue.
#[test]
fn startup_finishes_an_interrupted_start_over() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("disc-abc");
    fs::create_dir(&dir).unwrap();
    let mut old = DiscState::new(StagingState::Stopped);
    old.disc_identity = "abc".into();
    old.title = "Heat".into();
    try_write_state(&dir, &old).unwrap();
    fs::write(dir.join("Heat.iso"), b"old capture").unwrap();
    fs::write(dir.join("Heat.iso.mapfile"), b"old map").unwrap();
    {
        let lease = job_lease(&dir);
        let _owner = lease.lock().unwrap();
        assert!(reset_capture_with(&dir, "abc", || Err(io::Error::other("restart"))).is_err());
    }
    assert!(snapshot_staging_disc(&dir).is_none());
    resume_or_quarantine_staging(&root.path().to_string_lossy());
    let st = read_state(&dir).unwrap();
    assert!(
        !st.reset_in_progress,
        "the interrupted reset was left pending"
    );
    assert_eq!(st.disc_identity, "abc");
    assert_eq!(st.title, "Heat");
    assert!(!dir.join("Heat.iso").exists());
    assert!(!dir.join("Heat.iso.mapfile").exists());
    assert!(snapshot_staging_disc(&dir).is_some());
}

#[test]
fn a_live_start_over_is_not_finished_by_another_thread() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = DiscState::new(StagingState::Stopped);
    st.reset_in_progress = true;
    st.disc_identity = "abc".into();
    try_write_state(dir.path(), &DiscState::new(StagingState::Stopped)).unwrap();
    mutate_state(dir.path(), StagingState::Stopped, |s| *s = st.clone()).unwrap();
    fs::write(dir.path().join("Heat.iso"), b"capture").unwrap();
    let lease = job_lease(dir.path());
    let _resetting = lease.lock().unwrap();
    assert!(!finish_interrupted_reset(dir.path()).unwrap());
    assert!(dir.path().join("Heat.iso").exists());
}

// An unverified-<epoch> job is never adopted by a later scan, so once Stopped and no
// drive shows it any more, Clear staging reclaims it (with its partial capture).
#[test]
fn clear_staging_reclaims_stopped_unverified_jobs_no_drive_shows() {
    let root = tempfile::tempdir().unwrap();
    let job = |name: &str| {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        let mut st = DiscState::new(StagingState::Stopped);
        st.user_metadata = Some(UserMetadata {
            title: "Scratched".into(),
            year: 2001,
            media_type: crate::server::planner::MediaKind::Movie,
            episode_start: None,
            tmdb_id: 1,
            poster_url: String::new(),
            overview: String::new(),
        });
        try_write_state(&dir, &st).unwrap();
        fs::write(dir.join("Scratched.iso"), b"partial").unwrap();
        fs::write(dir.join("Scratched.iso.mapfile"), b"map").unwrap();
        dir
    };
    let orphan = job("unverified-1760000000");
    let shown = job("unverified-1760000001");
    let verified = job("disc-0123");
    super::super::update_state_with("dev-unverified-shown", |s| {
        s.job_id = "unverified-1760000001".into();
    });
    let (removed, failed) = clear_abandoned_staging(root.path());
    super::super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove("dev-unverified-shown");
    assert_eq!((removed, failed), (1, 0));
    assert!(!orphan.exists(), "an unadoptable capture was kept for good");
    assert!(shown.exists(), "the drive still shows this job");
    assert!(
        verified.exists(),
        "a verified job can be resumed by its disc"
    );
}

// A 1.8.3 title-named folder no disc can match again is listed for the operator, not
// deleted (its owner might still want the partial).
#[test]
fn clear_staging_lists_unmatchable_1_8_3_folders_without_deleting_them() {
    let root = tempfile::tempdir().unwrap();
    let legacy = root.path().join("Heat");
    fs::create_dir(&legacy).unwrap();
    fs::write(legacy.join("Heat.mkv"), b"partial").unwrap();
    fs::write(legacy.join(".disc-label"), b"HEAT").unwrap();
    let job = root.path().join("disc-0123");
    fs::create_dir(&job).unwrap();
    let mut st = DiscState::new(StagingState::Stopped);
    st.disc_identity = "0123".into();
    try_write_state(&job, &st).unwrap();
    fs::write(job.join("Heat.mkv"), b"partial").unwrap();
    for system in [".snapshot", "@eaDir", "#recycle"] {
        fs::create_dir(root.path().join(system)).unwrap();
        fs::write(root.path().join(system).join("Thumbs.db"), b"nas").unwrap();
    }
    assert_eq!(unmatchable_legacy_jobs(root.path()), ["Heat"]);
    clear_abandoned_staging(root.path());
    assert!(legacy.join("Heat.mkv").exists());
}

// An unverified capture a crash left mid-sweep is stopped at startup, so Clear staging
// reclaims it like any other unverified capture no drive can adopt again.
#[test]
fn an_unverified_capture_cut_off_mid_sweep_is_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("unverified-1769999999");
    fs::create_dir(&dir).unwrap();
    try_write_state(&dir, &DiscState::new(StagingState::Sweeping)).unwrap();
    fs::write(dir.join("Disc.iso"), b"partial capture").unwrap();
    fs::write(dir.join("Disc.iso.mapfile"), b"# map").unwrap();
    resume_or_quarantine_staging(&root.path().to_string_lossy());
    assert_eq!(read_state(&dir).unwrap().state, StagingState::Stopped);
    assert_eq!(clear_abandoned_staging(root.path()), (1, 0));
    assert!(!dir.exists());
}

// Only an inert 1.8.3 leftover is offered for deleting by hand: never one the mover still
// delivers or holds, a capture waiting in Review, or a folder a live owner holds.
#[test]
fn live_1_8_3_hand_offs_and_review_captures_are_not_listed_as_unmatchable() {
    let root = tempfile::tempdir().unwrap();
    let legacy = |name: &str, state: StagingState, extra: &[&str]| {
        let dir = root.path().join(name);
        fs::create_dir(&dir).unwrap();
        try_write_state(&dir, &DiscState::new(state)).unwrap();
        fs::write(dir.join(format!("{name}.mkv")), b"muxed").unwrap();
        for file in extra {
            fs::write(dir.join(file), b"{}").unwrap();
        }
        dir
    };
    legacy("Done", StagingState::Done, &[]);
    legacy("Ripped", StagingState::Ripped, &[]);
    legacy("Review", StagingState::Review, &[]);
    legacy("HandedOff", StagingState::Stopped, &[DONE_MARKER]);
    legacy("Journaled", StagingState::Stopped, &[DELIVERY_JOURNAL]);
    let leased = legacy("Leased", StagingState::Stopped, &[]);
    legacy("Inert", StagingState::Stopped, &[]);
    let lease = job_lease(&leased);
    let _held = lease.try_lock().unwrap();
    let listed = std::thread::spawn({
        let root = root.path().to_path_buf();
        move || unmatchable_legacy_jobs(&root)
    })
    .join()
    .unwrap();
    assert_eq!(listed, ["Inert"]);
}

// The multi-GB unlinks of a prune run without the state lock, so a correction request
// is not held behind them; the files are already renamed aside, so it is refused.
#[test]
fn a_prune_unlinks_outside_the_state_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut st = DiscState::new(StagingState::Done);
    let iso = dir.path().join("Heat.iso");
    let map = dir.path().join("Heat.iso.mapfile");
    st.iso_path = iso.to_string_lossy().into_owned();
    try_write_state(dir.path(), &st).unwrap();
    fs::write(&iso, b"image").unwrap();
    fs::write(&map, b"map").unwrap();
    let path = dir.path().to_path_buf();
    let pruned = prune_with(dir.path(), &[&iso, &map], |aside| {
        let lock = state_lock(&path);
        assert!(lock.try_lock().is_ok(), "unlink ran under the state lock");
        assert!(!saved_image_present(&path, &read_state(&path).unwrap()));
        fs::remove_file(aside)
    })
    .unwrap();
    assert!(pruned.iter().all(|(_, r)| r.is_ok()));
    assert!(!iso.exists() && !map.exists());
    assert_eq!(
        fs::read_dir(dir.path()).unwrap().count(),
        1,
        "only state.json is left"
    );
}

// An unreadable state.json is a storage failure, not a conflict with the job's state:
// the error keeps its own kind so the web layer does not answer 409.
#[test]
fn an_unreadable_state_keeps_its_error_kind() {
    let corrupt = tempfile::tempdir().unwrap();
    fs::write(state_path(corrupt.path()), b"{ torn").unwrap();
    let io_fault = tempfile::tempdir().unwrap();
    fs::create_dir(state_path(io_fault.path())).unwrap();
    let selection = UserMetadata {
        title: "Heat".into(),
        year: 1995,
        media_type: crate::server::planner::MediaKind::Movie,
        episode_start: None,
        tmdb_id: 949,
        poster_url: String::new(),
        overview: String::new(),
    };
    for dir in [corrupt.path(), io_fault.path()] {
        let saved = save_user_metadata(dir, selection.clone()).unwrap_err();
        assert_ne!(saved.kind(), io::ErrorKind::Other, "{saved}");
        let written = try_write_state(dir, &DiscState::new(StagingState::Stopped)).unwrap_err();
        assert_ne!(written.kind(), io::ErrorKind::Other, "{written}");
    }
    assert_eq!(
        save_user_metadata(corrupt.path(), selection)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

// A crash between a prune's rename and its unlink leaves `<iso>.pruning`: startup removes
// it from a job nobody holds, and nothing but the prune's own leftovers.
#[test]
fn startup_removes_an_interrupted_prunes_leftovers_from_unheld_jobs() {
    let root = tmpdir();
    let mut leftovers = Vec::new();
    for name in ["Heat", "Held"] {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        try_write_state(&dir, &DiscState::new(StagingState::Stopped)).unwrap();
        let iso = dir.join(format!("{name}.iso"));
        let map = dir.join(format!("{name}.iso.mapfile"));
        fs::write(&iso, b"image").unwrap();
        fs::write(&map, b"map").unwrap();
        fs::write(dir.join("notes.pruning"), b"operator file").unwrap();
        fs::write(dir.join(format!("{name}.mkv")), b"muxed").unwrap();
        let crashed = prune_with(&dir, &[&iso, &map], |_| Err(io::Error::other("crash")));
        assert!(crashed.unwrap().iter().all(|(_, r)| r.is_err()));
        leftovers.push(dir.join(format!("{name}.iso.pruning")));
        leftovers.push(dir.join(format!("{name}.iso.mapfile.pruning")));
    }
    let held = job_lease(&root.join("Held"));
    let _owner = held.try_lock().unwrap();
    resume_or_quarantine_staging(&root.to_string_lossy());
    assert!(!leftovers[0].exists() && !leftovers[1].exists());
    assert!(
        leftovers[2].exists() && leftovers[3].exists(),
        "a held job is left alone"
    );
    assert!(root.join("Heat/notes.pruning").exists());
    assert!(root.join("Heat/Heat.mkv").exists());
}
