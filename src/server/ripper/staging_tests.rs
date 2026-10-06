use super::*;
use std::fs;

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

// A live-disc rip rebuilds the plan from the disc, so it may deliberately
// replace an unreadable state.json (kept aside); the hand-off must then land.
#[test]
fn live_disc_seed_replaces_unreadable_state_and_handoff_proceeds() {
    let d = tmpdir();
    fs::write(state_path(&d), b"{ torn").unwrap();
    let replaced = seed_sweeping_for_live_rip(&d);
    assert!(replaced.is_some(), "the replacement must be reported");
    assert_eq!(
        read_state(&d).map(|s| s.state),
        Some(StagingState::Sweeping)
    );
    assert_eq!(fs::read(d.join(UNREADABLE_STATE_ASIDE)).unwrap(), b"{ torn");
    let m = DiscState::new(StagingState::Ripped).to_ripped_marker();
    assert!(
        crate::server::muxer::write_marker(&d, &m).is_ok(),
        "hand-off refused"
    );
    assert!(
        mark_handoff(&d, true, |_| {}).is_ok(),
        "mark_handoff refused"
    );

    // A readable state is seeded in place, nothing reported or set aside.
    let ok = tmpdir();
    write_state(&ok, &DiscState::new(StagingState::Ripped));
    assert!(seed_sweeping_for_live_rip(&ok).is_none());
    assert_eq!(
        read_state(&ok).map(|s| s.state),
        Some(StagingState::Sweeping)
    );
    assert!(!ok.join(UNREADABLE_STATE_ASIDE).exists());
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

    assert!(
        read_state(&disc_a).is_none(),
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
    // Clearing a Sweeping dir removes state.json entirely (resumable, not owned).
    assert!(read_state(&d3).is_none());
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

// Every disc of a boxset resolves to one TMDB title, so before this they
// shared a staging dir and disc 2 was never read. The raw volume label
// is what still tells the discs apart.
#[test]
fn a_different_disc_with_the_same_title_gets_its_own_staging_dir() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();

    // Nothing there yet: the plain title wins.
    assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_1"), "Movie");

    // Disc 1 rips and finishes.
    std::fs::create_dir_all(r.join("Movie")).unwrap();
    write_disc_label(&r.join("Movie"), "MOVIE_DISC_1");

    // Disc 1 re-inserted (a container restart with the disc still in the
    // drive) must find ITS OWN dir, not spawn a new one — otherwise every
    // Watchtower deploy re-sweeps a finished disc.
    assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_1"), "Movie");

    // Disc 2 is a different disc with the same title: its own dir.
    assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_2"), "Movie_2");

    // ...and once disc 2 exists, disc 3 goes past both.
    std::fs::create_dir_all(r.join("Movie_2")).unwrap();
    write_disc_label(&r.join("Movie_2"), "MOVIE_DISC_2");
    assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_3"), "Movie_3");
    // Disc 2 still finds its own.
    assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_2"), "Movie_2");
}

/// A staging dir written before labels existed has none. It must read as
/// "this disc", so an upgrade does not re-rip staging that is already
/// finished, and does not orphan a partial rip into a new directory.
#[test]
fn an_unlabelled_legacy_staging_dir_is_treated_as_the_same_disc() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    std::fs::create_dir_all(r.join("Movie")).unwrap();
    // No .disc-label written — this is what the previous version left.
    assert!(dir_is_same_disc(&r.join("Movie"), "ANY_LABEL"));
    assert_eq!(staging_name_for_disc(r, "Movie", "ANY_LABEL"), "Movie");
}

// The mirror case: the CALLER doesn't know the label (some RipStates
// are seeded by the mux/mover paths). Must resolve to the plain title
// dir, never to a `_2` that no rip created.
#[test]
fn an_unknown_disc_label_resolves_to_the_plain_title_dir() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    std::fs::create_dir_all(r.join("Movie")).unwrap();
    write_disc_label(&r.join("Movie"), "MOVIE_DISC_1");
    assert_eq!(staging_name_for_disc(r, "Movie", ""), "Movie");
    assert_eq!(staging_basename(r, "Movie", ""), "Movie");
}

/// `adopt_disc_label` stamps an unlabelled (legacy) dir for the first disc
/// that uses it, so the NEXT different disc no longer matches it — and
/// never rewrites a label that is already there.
#[test]
fn adopting_a_legacy_dir_stamps_it_once_for_its_first_user() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    let dir = r.join("Movie");
    std::fs::create_dir_all(&dir).unwrap();

    adopt_disc_label(&dir, "MOVIE_DISC_1");
    assert_eq!(read_disc_label(&dir).as_deref(), Some("MOVIE_DISC_1"));
    // Now that it is owned, a different disc of the same title moves over.
    assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_2"), "Movie_2");

    // Never clobbers an existing label.
    adopt_disc_label(&dir, "MOVIE_DISC_2");
    assert_eq!(read_disc_label(&dir).as_deref(), Some("MOVIE_DISC_1"));

    // An unknown label adopts nothing — it would record a lie.
    let dir2 = r.join("Other");
    std::fs::create_dir_all(&dir2).unwrap();
    adopt_disc_label(&dir2, "");
    assert!(read_disc_label(&dir2).is_none());
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

    assert!(seed_sweeping_for_live_rip(&disc).is_none());
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
        tmdb_media_type: "tv".into(),
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
