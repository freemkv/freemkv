use super::*;

// The stall clock counts monotonic time since the last activity: a wall-clock step in
// the frame stamp is activity (never a stall), and only a frozen stamp + byte count stalls.
#[test]
fn the_stall_clock_is_monotonic_and_reset_by_activity() {
    let t0 = std::time::Instant::now();
    let at = |s: u64| t0 + std::time::Duration::from_secs(s);
    let mut c = StallClock::new(1_000, 0, t0);
    assert_eq!(c.observe(1_000, 0, at(15)), 15);
    // The host clock stepped forward 30 minutes between two frames.
    assert_eq!(c.observe(1_000 + 1_800, 4096, at(30)), 0);
    assert_eq!(c.observe(1_000 + 1_800, 4096, at(45)), 15);
    // Bytes alone are activity too.
    assert_eq!(c.observe(1_000 + 1_800, 8192, at(60)), 0);
    assert_eq!(
        c.observe(1_000 + 1_800, 8192, at(60 + HARD_WATCHDOG_STALL_SECS)),
        HARD_WATCHDOG_STALL_SECS
    );
}

// The soft stall shows at 30 s, and never over a terminal tile.
#[test]
fn the_soft_stall_threshold_and_terminal_statuses() {
    assert_eq!(SOFT_WATCHDOG_STALL_SECS, 30);
    for status in ["idle", "done", "complete", "failed", "error"] {
        assert!(!watchdog_may_mark_stalled(status), "{status}");
    }
    assert!(watchdog_may_mark_stalled("ripping"));
}

// The hard escalation must bump through the bounded production helper
// (tests/watchdog.rs covers its timeout) before `exit(1)`, never inline.
#[test]
fn hard_watchdog_bumps_via_the_bounded_helper_before_exit() {
    let src = crate::server::util::source_lf(include_str!("mux.rs"));
    let start = src
        .find("fn spawn_mux_watchdog(")
        .expect("watchdog spawner");
    let body = &src[start..];
    let esc = body
        .find("if stall_secs >= HARD_WATCHDOG_STALL_SECS {")
        .expect("hard escalation branch");
    let exit = esc + body[esc..].find("std::process::exit(1)").expect("exit(1)");
    let region = &body[esc..exit];
    assert!(
        region.contains("watchdog_bump_restart_count(&wd_device, &wd_staging_disc_dir)"),
        "the hard watchdog must call watchdog_bump_restart_count before exit(1)"
    );
    assert!(
        !region.contains("increment_restart_count("),
        "the counter bump must not be inlined (unbounded) in the escalation"
    );
}

const DISC: u64 = 60_000_000_000; // 60 GB stand-in for a UHD

// Regression: a hard producer read error must surface the SPECIFIC coded cause, not a
// generic truncation string, so an operator sees the real fault (decrypt / DiscRead / AACS)
// in `last_error`.
#[test]
fn producer_read_error_cause_preserves_coded_root_cause() {
    // A decrypt failure manifesting mid-stream.
    let decrypt_io: std::io::Error = libfreemkv::Error::DecryptFailed.into();
    let decrypt_code = libfreemkv::Error::DecryptFailed.code();
    let cause = producer_read_error_cause(&decrypt_io);
    // The annotated parenthetical form must actually be emitted — not just
    // an incidental `E####` in the message tail (guards the dead `else`
    // branch the code-extraction round-trip used to leave unreachable).
    assert!(
        cause.contains(&format!("(E{decrypt_code})")),
        "decrypt cause must name the coded fault in the annotation, got: {cause}"
    );
    assert!(cause.contains("read error mid-stream"), "got: {cause}");

    // A coded disc read error (the genuine bad-sector / drive fault).
    let disc_err = libfreemkv::Error::DiscRead {
        sector: 12345,
        status: None,
        sense: None,
    };
    let disc_code = disc_err.code();
    let disc_io: std::io::Error = disc_err.into();
    let cause = producer_read_error_cause(&disc_io);
    assert!(
        cause.contains(&format!("(E{disc_code})")),
        "disc-read cause must name the coded fault in the annotation, got: {cause}"
    );
}

// Regression (rc4): the cause must carry an English description, not a bare duplicated
// `E####` (was `read error mid-stream (E7013): E7013`).
#[test]
fn producer_read_error_cause_carries_english_label() {
    let decrypt_io: std::io::Error = libfreemkv::Error::DecryptFailed.into();
    let decrypt_code = libfreemkv::Error::DecryptFailed.code();
    let cause = producer_read_error_cause(&decrypt_io);
    assert!(
        cause.contains("decryption failed"),
        "decrypt cause must read in English, got: {cause}"
    );
    // The bare code must not appear as the trailing description (the
    // original leaked `(E7013): E7013` defect).
    assert!(
        !cause.ends_with(&format!("E{decrypt_code}")),
        "cause must not end with a bare duplicated code, got: {cause}"
    );
    assert!(
        !cause.contains(&format!("): E{decrypt_code}")),
        "cause must not render the code as its own description, got: {cause}"
    );

    // A coded disc-read fault gets its English label too.
    let disc_io: std::io::Error = libfreemkv::Error::DiscRead {
        sector: 42,
        status: None,
        sense: None,
    }
    .into();
    let cause = producer_read_error_cause(&disc_io);
    assert!(
        cause.contains("disc read error"),
        "disc-read cause must read in English, got: {cause}"
    );
}

/// A plain (non-coded) io error must NOT get a spurious `E####`
/// numeric prefix — its message round-trips to the generic IoError
/// code, so only the `{e}` tail describes it.
#[test]
fn producer_read_error_cause_handles_plain_io_error() {
    let plain = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read");
    let cause = producer_read_error_cause(&plain);
    assert!(cause.contains("short read"), "got: {cause}");
    assert!(
        !cause.contains(&format!("(E{})", libfreemkv::error::E_IO_ERROR)),
        "plain io error must not carry a synthetic code prefix, got: {cause}"
    );
    // No parenthetical annotation at all for a non-coded message.
    assert!(
        !cause.contains("(E"),
        "plain io error must not carry any code annotation, got: {cause}"
    );

    // A coded error that maps to the generic IoError code must also not
    // gain a spurious `(E5000)` annotation — only its tail names it.
    let io_coded: std::io::Error = libfreemkv::Error::from(std::io::Error::other("boom")).into();
    let cause = producer_read_error_cause(&io_coded);
    assert!(
        !cause.contains(&format!("(E{})", libfreemkv::error::E_IO_ERROR)),
        "IoError-coded fault must not carry the generic annotation, got: {cause}"
    );
}

// Clean disc: retry term vanishes, total_work reduces to 2x capacity, so
// mux opens at exactly 50% and climbs linearly to 100%.
#[test]
fn clean_disc_mux_opens_at_50_percent() {
    // max_retries planned 5, but bytes_unreadable=0 → retries
    // contribute nothing whether 0 or 5 actually ran.
    assert_eq!(total_pct_byte_weight(DISC, 5, 0, 0), 50);
    assert_eq!(total_pct_byte_weight(DISC, 5, 0, 50), 75);
    assert_eq!(total_pct_byte_weight(DISC, 5, 0, 100), 100);
    // Same disc, max_retries planned 0 (couldn't have happened
    // here since multipass implies max_retries > 0, but the
    // helper falls through to direct-mode behaviour anyway).
    assert_eq!(total_pct_byte_weight(DISC, 0, 0, 50), 50);
}

/// Damaged disc with residual `bytes_unreadable`: retry term
/// inflates the denominator, mux opens lower than 50% because
/// the rip "did more total work than just sweep+mux."
#[test]
fn damaged_disc_mux_opens_below_50_percent() {
    // 1 GB unreadable, max_retries=5 → retry term = 5 GB.
    // total_work = 60 + 5 + 60 = 125 GB.
    // mux start: total_done = 60 + 5 + 0 = 65. 65/125 = 52%.
    assert_eq!(total_pct_byte_weight(DISC, 5, 1_000_000_000, 0), 52);
    // mux halfway: total_done = 60 + 5 + 30 = 95. 95/125 = 76%.
    assert_eq!(total_pct_byte_weight(DISC, 5, 1_000_000_000, 50), 76);
    // mux done: 100.
    assert_eq!(total_pct_byte_weight(DISC, 5, 1_000_000_000, 100), 100);
}

/// Direct-mux / single-pass mode (`max_retries == 0`): there are
/// no separate phases — total tracks current 1:1.
#[test]
fn direct_mode_passthrough() {
    assert_eq!(total_pct_byte_weight(DISC, 0, 0, 0), 0);
    assert_eq!(total_pct_byte_weight(DISC, 0, 0, 42), 42);
    assert_eq!(total_pct_byte_weight(DISC, 0, 0, 100), 100);
}

/// Bound + edge cases: zero inputs, overshoot.
#[test]
fn edge_cases() {
    // Zero capacity (drive read failed) → fall through to mux pct.
    assert_eq!(total_pct_byte_weight(0, 5, 0, 73), 73);
    // pct overshoot doesn't push total past 100.
    assert_eq!(total_pct_byte_weight(DISC, 5, 0, 200), 100);
    assert_eq!(total_pct_byte_weight(DISC, 5, 1_000_000_000, 200), 100);
}

// ── resume progress starts at >0 (telemetry audit Fix 2) ── When
// max_retries > 0, a resumed rip (mux_pct=0) opens above 0% since the
// helper credits the already-completed sweep.
#[test]
fn resume_progress_starts_above_zero_when_max_retries_nonzero() {
    // Clean disc (bytes_unreadable=0, retry term vanishes): total_work =
    // 2×cap. At mux start (mux_pct=0), total_done = cap, so total_pct =
    // cap / (2*cap) * 100 = 50%.
    let pct = total_pct_byte_weight(DISC, 3, 0, 0);
    assert_eq!(
        pct, 50,
        "resume with max_retries=3 and clean disc should open at 50%, not 0%"
    );
}

// max_retries=0 falls through to mux_pct directly (correct for
// single-pass/direct mode) — guard against accidentally changing it.
#[test]
fn direct_mode_progress_matches_mux_pct() {
    // max_retries=0 → direct-mode passthrough: total_pct == mux_pct.
    assert_eq!(total_pct_byte_weight(DISC, 0, 0, 0), 0);
    assert_eq!(total_pct_byte_weight(DISC, 0, 0, 50), 50);
    assert_eq!(total_pct_byte_weight(DISC, 0, 0, 100), 100);
}

// ── AutoripMuxEvents bridge feeds the watchdog byte atomics ──────────────

fn test_shared_atomics() -> (
    SharedAtomics,
    Arc<AtomicU64>,
    Arc<AtomicU64>,
    Arc<AtomicU64>,
) {
    let wd_bytes = Arc::new(AtomicU64::new(0));
    let wd_last_frame = Arc::new(AtomicU64::new(0));
    let latest_bytes_read = Arc::new(AtomicU64::new(0));
    let atomics = SharedAtomics {
        latest_bytes_read: latest_bytes_read.clone(),
        rip_last_lba: Arc::new(AtomicU64::new(0)),
        rip_current_batch: Arc::new(AtomicU16::new(0)),
        wd_last_frame: wd_last_frame.clone(),
        wd_bytes: wd_bytes.clone(),
        input_errors: Arc::new(AtomicU32::new(0)),
    };
    (atomics, wd_bytes, wd_last_frame, latest_bytes_read)
}

fn test_ui_state() -> UiState {
    UiState {
        device: "sr-test".to_string(),
        display_name: String::new(),
        disc_format: String::new(),
        tmdb_title: String::new(),
        tmdb_year: 0,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        duration: String::new(),
        codecs: String::new(),
        filename: String::new(),
        batch: 0,
        total_bytes: 1_000_000,
        total_passes: 0,
        bytes_total_disc: 0,
        max_retries: 0,
        bytes_unreadable_at_mux: 0,
        sweep_damage: SweepDamageSnapshot::default(),
    }
}

// `push_mux_state` is the only writer of live per-frame `RipState` during mux; a mutant
// that reverts status/disc_present to defaults would make a busy device look idle.
#[test]
fn push_mux_state_reports_ripping_and_disc_present() {
    let device = "push_mux_state_test_device";
    let mut ui = test_ui_state();
    ui.device = device.to_string();
    let (atomics, ..) = test_shared_atomics();

    push_mux_state(&ui, &atomics, 10, 5.0, "1:00".into(), 1000, 0.0, 0);

    let rs = super::super::STATE
        .lock()
        .unwrap()
        .get(device)
        .cloned()
        .expect("push_mux_state must write a STATE entry for the device");
    assert_eq!(rs.status, "ripping");
    assert!(
        rs.disc_present,
        "a live mux tick must report disc_present=true"
    );
    super::super::STATE.lock().unwrap().remove(device);
}

// `push_mux_state` carries the sweep's damage through the mux (ISO reads add none), and a
// clean sweep passes the live counters through.
#[test]
fn push_mux_state_carries_sweep_damage_else_live_counters() {
    let device = "push_mux_state_damage_test_device";
    let (atomics, ..) = test_shared_atomics();
    let mut ui = test_ui_state();
    ui.device = device.to_string();
    ui.bytes_total_disc = 1_000;
    ui.max_retries = 3;
    ui.sweep_damage = SweepDamageSnapshot {
        errors: 42,
        total_lost_ms: 3_700.0,
        main_lost_ms: 2_000.0,
        num_bad_ranges: 2,
        largest_gap_ms: 900.0,
        ..Default::default()
    };
    push_mux_state(&ui, &atomics, 0, 5.0, String::new(), 0, 0.25, 5);
    let rs = super::super::STATE.lock().unwrap().remove(device).unwrap();
    assert_eq!(rs.errors, 42);
    assert!((rs.lost_video_secs - 3.7).abs() < 1e-9);
    assert_eq!((rs.total_lost_ms, rs.main_lost_ms), (3_700.0, 2_000.0));
    assert_eq!((rs.num_bad_ranges, rs.largest_gap_ms), (2, 900.0));
    assert_eq!(
        rs.total_progress_pct,
        total_pct_byte_weight(1_000, 3, 0, 0),
        "the mux opens at the sweep's credited share"
    );

    ui.sweep_damage = SweepDamageSnapshot::default();
    push_mux_state(&ui, &atomics, 0, 5.0, String::new(), 0, 0.25, 5);
    let rs = super::super::STATE.lock().unwrap().remove(device).unwrap();
    assert_eq!(rs.errors, 5);
    assert!((rs.lost_video_secs - 0.25).abs() < 1e-9);
}

// Past the 1 s throttle, a write tick pushes progress from the ISO read-ahead position
// when one is known (the write position lags it).
#[test]
fn a_write_tick_past_the_throttle_reports_read_ahead_progress() {
    use libfreemkv::Events as _;
    let device = "write_tick_progress_test_device";
    let (atomics, _wd, _lf, latest_bytes_read) = test_shared_atomics();
    let mut ui = test_ui_state();
    ui.device = device.to_string();
    let long_ago = Instant::now() - std::time::Duration::from_secs(2);
    let events = AutoripMuxEvents {
        ui,
        atomics,
        progress: Mutex::new(freemkv_engine::SpeedEstimator::new()),
        last_update: Mutex::new(long_ago),
        last_log: Mutex::new(Instant::now()),
        opened: AtomicBool::new(false),
    };
    latest_bytes_read.store(600_000, Ordering::Relaxed);
    events.event(&libfreemkv::Event::BytesWritten {
        bytes: 500_000,
        total: 1_000_000,
    });
    let rs = super::super::STATE.lock().unwrap().remove(device).unwrap();
    assert_eq!(rs.status, "ripping");
    assert_eq!(
        rs.progress_pct, 60,
        "read-ahead position, not the write's 50%"
    );
    assert!((rs.progress_gb - 600_000.0 / BYTES_PER_GIB).abs() < 1e-12);
}

// THE watchdog preservation check: `on_write_progress` must feed `wd_bytes`/`wd_last_frame`
// even on the throttled early-return path, so a healthy mux never false-escalates.
#[test]
fn autorip_mux_events_feed_watchdog_byte_atomic() {
    use libfreemkv::Events as _;
    let (atomics, wd_bytes, wd_last_frame, latest_bytes_read) = test_shared_atomics();
    let events = AutoripMuxEvents {
        ui: test_ui_state(),
        atomics,
        progress: Mutex::new(freemkv_engine::SpeedEstimator::new()),
        // `now` → the 1 s throttle fires and `on_write_progress` returns
        // early AFTER feeding the watchdog atomics: the feed must not be
        // gated behind the UI throttle.
        last_update: Mutex::new(Instant::now()),
        last_log: Mutex::new(Instant::now()),
        opened: AtomicBool::new(false),
    };

    // Reader side: feeds the UI read-ahead position + watchdog activity.
    events.event(&libfreemkv::Event::BytesRead {
        bytes: 4096,
        total: 8192,
    });
    assert_eq!(
        latest_bytes_read.load(Ordering::Relaxed),
        4096,
        "on_read_progress must feed latest_bytes_read"
    );
    assert!(
        wd_last_frame.load(Ordering::Relaxed) > 0,
        "on_read_progress must refresh wd_last_frame (watchdog activity)"
    );

    // Writer side (throttled): wd_bytes MUST still advance — this is the
    // load-bearing feed that keeps the hard watchdog from firing exit(1).
    events.event(&libfreemkv::Event::BytesWritten {
        bytes: 500_000,
        total: 1_000_000,
    });
    assert_eq!(
        wd_bytes.load(Ordering::Relaxed),
        500_000,
        "on_write_progress must feed wd_bytes even on the throttled path"
    );
    assert!(
        wd_last_frame.load(Ordering::Relaxed) > 0,
        "on_write_progress must refresh wd_last_frame"
    );

    // The opened flag drives `output_opened` in the outcome mapping.
    assert!(!events.opened.load(Ordering::Relaxed));
    events.event(&libfreemkv::Event::OutputOpened {
        title: &libfreemkv::DiscTitle::empty(),
    });
    assert!(
        events.opened.load(Ordering::Relaxed),
        "on_output_opened must set the opened flag"
    );
}

// Regression D: `on_sector_skipped` must store the skipped LBA into
// `rip_last_lba` (the UI playhead), refresh watchdog activity, and bump
// `input_errors`, matching the pre-refactor `make_stream_event_fn`.
#[test]
fn on_sector_skipped_stores_lba_into_rip_last_lba() {
    use libfreemkv::Events as _;
    let (atomics, _wd_bytes, wd_last_frame, _lbr) = test_shared_atomics();
    let rip_last_lba = atomics.rip_last_lba.clone();
    let input_errors = atomics.input_errors.clone();
    let events = AutoripMuxEvents {
        ui: test_ui_state(),
        atomics,
        progress: Mutex::new(freemkv_engine::SpeedEstimator::new()),
        last_update: Mutex::new(Instant::now()),
        last_log: Mutex::new(Instant::now()),
        opened: AtomicBool::new(false),
    };

    events.event(&libfreemkv::Event::SectorSkipped { lba: 4242 });
    assert_eq!(
        rip_last_lba.load(Ordering::Relaxed),
        4242,
        "on_sector_skipped must store the skipped LBA into rip_last_lba \
             (the UI last_sector / playhead), matching make_stream_event_fn"
    );
    assert_eq!(
        input_errors.load(Ordering::Relaxed),
        1,
        "on_sector_skipped must still bump input_errors (additive)"
    );
    assert!(
        wd_last_frame.load(Ordering::Relaxed) > 0,
        "on_sector_skipped must refresh wd_last_frame (watchdog activity)"
    );

    // A later skip advances the playhead to the new LBA.
    events.event(&libfreemkv::Event::SectorSkipped { lba: 9001 });
    assert_eq!(
        rip_last_lba.load(Ordering::Relaxed),
        9001,
        "a subsequent skip must move the playhead forward"
    );
    assert_eq!(input_errors.load(Ordering::Relaxed), 2);
}

// Regression D: `on_batch_size_changed` must store the new batch and emit
// the batch-change device-log line `make_stream_event_fn` used to
// produce; both reason variants must render without panicking.
#[test]
fn on_batch_size_changed_stores_batch_and_logs() {
    use libfreemkv::Events as _;
    let (atomics, ..) = test_shared_atomics();
    let rip_current_batch = atomics.rip_current_batch.clone();
    let mut ui = test_ui_state();
    ui.device = "batch_change_log_test_device".to_string();
    let events = AutoripMuxEvents {
        ui,
        atomics,
        progress: Mutex::new(freemkv_engine::SpeedEstimator::new()),
        last_update: Mutex::new(Instant::now()),
        last_log: Mutex::new(Instant::now()),
        opened: AtomicBool::new(false),
    };

    events.event(&libfreemkv::Event::BatchSizeChanged {
        new_size: 64,
        reason: libfreemkv::BatchSizeReason::Shrunk,
    });
    assert_eq!(
        rip_current_batch.load(Ordering::Relaxed),
        64,
        "on_batch_size_changed must store the new batch into rip_current_batch"
    );
    events.event(&libfreemkv::Event::BatchSizeChanged {
        new_size: 128,
        reason: libfreemkv::BatchSizeReason::Probed,
    });
    assert_eq!(rip_current_batch.load(Ordering::Relaxed), 128);
    let log = crate::server::log::get_device_log("batch_change_log_test_device", 10);
    assert!(
        log.iter().any(|l| l.ends_with("Batch size → 64 (shrunk)")),
        "{log:?}"
    );
    assert!(
        log.iter()
            .any(|l| l.ends_with("Batch size → 128 (probed up)")),
        "{log:?}"
    );
}

// `map_iso_mux_outcome` preserves the pre-migration Err classification:
// halt/FMTS-missing -> Err; completed run -> `completed=true`; NoStreams
// drain -> quarantine (`finalize_error=Some`, output opened).
#[test]
fn map_iso_mux_outcome_classifies_faithfully() {
    let start = Instant::now();
    // Completed run.
    let ok = map_iso_mux_outcome(
        Ok(libfreemkv::MuxOutcome {
            halted: false,
            completed: true,
            output_opened: true,
            bytes_written: 1234,
            errors: 0,
            lost_bytes: 0,
            streams: 2,
            // Stream indices the sink accepted frames for but couldn't put
            // in the finished container. Empty here — clean completed run.
            undelivered_streams: Vec::new(),
        }),
        true,
        "sr-test",
        0.0,
        start,
        0,
        0,
    )
    .expect("completed run maps to Ok");
    assert!(ok.completed && ok.output_opened);
    assert_eq!(ok.bytes_done, 1234);

    // Ok(..) with completed=false — a clean stop or join-timeout wedge —
    // must NOT report as a finished mux: a mutant widening the
    // `Ok(o) if o.completed` guard would file a damaged rip as good (rule 1).
    let not_done = map_iso_mux_outcome(
        Ok(libfreemkv::MuxOutcome {
            halted: false,
            completed: false,
            output_opened: true,
            bytes_written: 500,
            errors: 0,
            lost_bytes: 0,
            streams: 2,
            undelivered_streams: Vec::new(),
        }),
        true,
        "sr-test",
        0.0,
        start,
        0,
        0,
    )
    .expect("a non-completed Ok result still maps to Ok, just completed=false");
    assert!(
        !not_done.completed,
        "an Ok(..) result with completed=false must not be reported as a finished mux"
    );
    assert!(
        not_done.output_opened,
        "output_opened is carried through unchanged from the engine outcome"
    );
    assert_eq!(not_done.bytes_done, 500);

    // Halt during construction → propagated as Err for call-site handling.
    let halt_err: std::io::Error = libfreemkv::Error::Halted.into();
    assert!(
        map_iso_mux_outcome(Err(halt_err), false, "sr-test", 0.0, start, 0, 0).is_err(),
        "Halted must propagate as Err so the call site preserves staging"
    );

    // NoStreams (empty/undecryptable) with output opened → quarantine.
    let nostreams: std::io::Error = libfreemkv::Error::NoStreams.into();
    let mapped =
        map_iso_mux_outcome(Err(nostreams), true, "sr-test", 0.0, start, 0, 0).expect("mapped");
    assert!(!mapped.completed);
    assert!(mapped.output_opened);
    assert!(
        mapped.finalize_error.is_some(),
        "NoStreams must quarantine via finalize_error"
    );

    // Header-phase failure (no sink opened) → output_opened=false + finalize.
    let mkv_invalid: std::io::Error = libfreemkv::Error::MkvInvalid.into();
    let hdr =
        map_iso_mux_outcome(Err(mkv_invalid), false, "sr-test", 0.0, start, 0, 0).expect("mapped");
    assert!(!hdr.output_opened);
    assert!(hdr.finalize_error.is_some());

    // After the output opened, a coded read fault truncates a resumable rip (read_error,
    // never the finalize quarantine); an IO / invalid-MKV error is a finalize failure.
    let read_fault: std::io::Error = libfreemkv::Error::DecryptFailed.into();
    let mid =
        map_iso_mux_outcome(Err(read_fault), true, "sr-test", 0.0, start, 700, 3).expect("mapped");
    assert!(mid.output_opened && !mid.completed);
    assert!(mid.read_error.is_some() && mid.finalize_error.is_none());
    assert_eq!((mid.bytes_done, mid.errors), (700, 3));
    for e in [
        libfreemkv::Error::MkvInvalid.into(),
        std::io::Error::other(libfreemkv::Error::IoError {
            source: std::io::Error::other("disk full"),
        }),
    ] {
        let fin = map_iso_mux_outcome(Err(e), true, "sr-test", 0.0, start, 700, 0).expect("mapped");
        assert!(fin.output_opened && !fin.completed);
        assert!(fin.finalize_error.is_some() && fin.read_error.is_none());
    }

    // The loss and error counts of a completed run: lost bytes over the title's rate.
    let lossy = map_iso_mux_outcome(
        Ok(libfreemkv::MuxOutcome {
            halted: false,
            completed: true,
            output_opened: true,
            bytes_written: 1234,
            errors: 4,
            lost_bytes: 3_000_000,
            streams: 2,
            undelivered_streams: Vec::new(),
        }),
        true,
        "sr-test",
        1_000_000.0,
        start,
        0,
        0,
    )
    .expect("mapped");
    assert_eq!(lossy.errors, 4);
    assert!((lossy.lost_video_secs - 3.0).abs() < 1e-9);
}

// `map_iso_mux_outcome` must not drop `undelivered_streams` on the floor even when
// `completed = true` — a lossy outcome is never silent.
#[test]
fn map_iso_mux_outcome_surfaces_undelivered_streams_on_a_completed_run() {
    // The per-device log ring is a process-global static shared by sibling
    // tests using `"sr-test"`; reading it back would make both assertions
    // below unsound (sibling lines, uncleared ring). Mint a unique name.
    let device = "sr_mux_undelivered_streams_note_test";
    let start = Instant::now();
    let lossy = map_iso_mux_outcome(
        Ok(libfreemkv::MuxOutcome {
            halted: false,
            completed: true,
            output_opened: true,
            bytes_written: 1234,
            errors: 0,
            lost_bytes: 0,
            streams: 2,
            undelivered_streams: vec![1],
        }),
        true,
        device,
        0.0,
        start,
        0,
        0,
    )
    .expect("completed run maps to Ok");
    assert!(lossy.completed);
    // REPORTED, not merely carried: the device log is where this reaches
    // the operator. Exactly once — zero is a silent lossy "success", two
    // is the duplicate-wording bug this replaced.
    let logged = crate::server::log::get_device_log(device, 50);
    let notes: Vec<&String> = logged
        .iter()
        .filter(|l| l.contains("could not be delivered into the output"))
        .collect();
    assert_eq!(
        notes.len(),
        1,
        "a completed-but-lossy mux must report its undelivered streams \
             exactly once; got {logged:?}"
    );
    assert!(
        notes[0].contains("[1]"),
        "the note must name the streams that were dropped: {:?}",
        notes[0]
    );
}

// ONE event, ONE wording, ONE emitter — the note used to have two independently-maintained
// spellings across mux.rs and mod.rs.
#[test]
fn the_undelivered_streams_note_has_a_single_emitter() {
    let mux_src = crate::server::util::source_lf(include_str!("mux.rs"));
    let mod_src = crate::server::util::source_lf(include_str!("mod.rs"));
    assert!(
        mux_src.contains("fn undelivered_streams_note("),
        "the note's wording must live in one shared function"
    );
    assert!(
        !mod_src.contains("stream(s) were not delivered"),
        "rip_disc must not carry a second, independently-worded copy of \
             the undelivered-streams note"
    );
    assert!(
        !mod_src.contains("stream(s) could not be delivered"),
        "rip_disc must not re-emit the note at all — map_iso_mux_outcome \
             already logged it for every outcome that can carry one"
    );
}

// ── EngineMuxSink: engine events → watchdog atomics + RipState ──────────

fn engine_sink(
    device: &str,
) -> (
    EngineMuxSink,
    Arc<AtomicU64>,
    Arc<AtomicU64>,
    Arc<AtomicU64>,
) {
    let (atomics, wd_bytes, wd_last_frame, latest) = test_shared_atomics();
    let mut ui = test_ui_state();
    ui.device = device.to_string();
    let sink = EngineMuxSink::new(ui, atomics, libfreemkv::Halt::new());
    (sink, wd_bytes, wd_last_frame, latest)
}

fn tick(done: u64, total: u64, speed_bps: u64, eta: Option<u64>) -> freemkv_engine::Progress {
    freemkv_engine::Progress {
        pass: "mux".into(),
        bytes_done: done,
        bytes_total: total,
        speed_bps,
        eta_secs: eta,
        ..Default::default()
    }
}

fn state_of(device: &str) -> Option<super::super::RipState> {
    super::super::STATE.lock().unwrap().get(device).cloned()
}

// Every tick feeds the watchdog, throttled or not; the first one marks the output
// opened and pushes no state yet (the 1 s cadence starts there).
#[test]
fn engine_sink_first_tick_opens_and_feeds_the_watchdog() {
    use freemkv_engine::Sink;
    let device = "engine_sink_first_tick";
    let (sink, wd_bytes, wd_last_frame, latest) = engine_sink(device);
    assert!(!sink.opened.load(Ordering::Relaxed));
    sink.progress(&tick(4096, 8192, 0, None));
    assert!(
        sink.opened.load(Ordering::Relaxed),
        "a written frame means opened"
    );
    assert_eq!(wd_bytes.load(Ordering::Relaxed), 4096);
    assert_eq!(latest.load(Ordering::Relaxed), 4096);
    assert!(wd_last_frame.load(Ordering::Relaxed) > 0);
    assert!(
        state_of(device).is_none(),
        "no state push on the opening tick"
    );
    // Throttled second tick: still feeds the watchdog, still no push.
    sink.progress(&tick(6000, 8192, 0, None));
    assert_eq!(wd_bytes.load(Ordering::Relaxed), 6000);
    assert!(state_of(device).is_none());
}

// Past the throttle, the engine's own progress (bytes, speed, ETA) is what lands
// in `/api/state` — never re-derived here.
#[test]
fn engine_sink_tick_maps_engine_progress_onto_rip_state() {
    use freemkv_engine::Sink;
    let device = "engine_sink_tick_maps";
    let (sink, ..) = engine_sink(device);
    sink.progress(&tick(1, 1000, 0, None));
    *sink.last_update.lock().unwrap() = Instant::now() - Duration::from_secs(2);
    sink.progress(&tick(250, 1000, 2 * 1024 * 1024, Some(75)));
    let rs = state_of(device).expect("a tick past the throttle pushes state");
    assert_eq!(rs.status, "ripping");
    assert!(rs.disc_present);
    assert_eq!(rs.progress_pct, 25, "percent of the engine's bytes_total");
    assert_eq!(rs.speed_mbs, 2.0);
    assert_eq!(rs.eta, "1:15");
    assert_eq!(rs.pass_eta, "1:15");
    super::super::STATE.lock().unwrap().remove(device);
}

// No engine total yet: the percentage falls back to the caller's denominator.
#[test]
fn engine_sink_percent_falls_back_to_the_caller_total() {
    use freemkv_engine::Sink;
    let device = "engine_sink_pct_fallback";
    let (sink, ..) = engine_sink(device);
    sink.progress(&tick(1, 0, 0, None));
    *sink.last_update.lock().unwrap() = Instant::now() - Duration::from_secs(2);
    sink.progress(&tick(500_000, 0, 0, None));
    let rs = state_of(device).expect("state pushed");
    assert_eq!(rs.progress_pct, 50, "500k of the UI's 1M total");
    assert_eq!(rs.eta, "", "no engine estimate yet shows no ETA");
    super::super::STATE.lock().unwrap().remove(device);
}

// The title's result is what `map_iso_mux_outcome` classifies: kept from
// `TitleDone` (error code intact), and a title never started reads as a halt.
#[test]
fn engine_sink_keeps_the_title_result() {
    use freemkv_engine::{Event, Sink};
    let (sink, ..) = engine_sink("engine_sink_result");
    let halted = sink
        .take_result(&freemkv_engine::RipOutcome::Halted)
        .expect_err("no TitleDone means stopped before start");
    assert!(super::super::is_halt_error(&halted));

    let done = libfreemkv::MuxOutcome {
        halted: false,
        completed: true,
        output_opened: true,
        bytes_written: 1234,
        errors: 0,
        lost_bytes: 0,
        streams: 2,
        undelivered_streams: Vec::new(),
    };
    sink.event(&Event::TitleDone {
        idx: 0,
        dest: "mkv:///x.mkv",
        result: Ok(&done),
    });
    let ok = freemkv_engine::RipOutcome::Ok { titles_written: 1 };
    assert_eq!(sink.take_result(&ok).expect("ok").bytes_written, 1234);

    let fmts: std::io::Error = libfreemkv::Error::FmtsKeyMissing.into();
    sink.event(&Event::TitleDone {
        idx: 0,
        dest: "mkv:///x.mkv",
        result: Err(&fmts),
    });
    let err = sink.take_result(&ok).expect_err("err");
    assert!(super::super::is_fmts_key_missing_error(&err), "{err}");
}

// KU-E1: a key refusal is never a Stop. Before any title the loop's outcome carries it;
// as a `TitleDone(Err)` with no output it reaches the call site as the key error
// (E7034 "insert the disc", else a key deferral), not a quarantining setup failure.
#[test]
fn a_key_refusal_is_a_key_error_not_a_stop_or_damage() {
    use freemkv_engine::{Event, RipOutcome, Sink};
    let (sink, ..) = engine_sink("engine_sink_key_refusal");
    let refused = |code: u16| RipOutcome::Failed {
        title_index: 0,
        code: Some(code),
        kind: std::io::ErrorKind::Other,
        data: String::new(),
    };
    for code in [7022u16, 7026, 7032, 7034] {
        let e = sink.take_result(&refused(code)).expect_err("refused");
        assert!(!super::super::is_halt_error(&e), "E{code} is not a Stop");
        assert_eq!(super::super::io_key_refusal(&e), Some(code));
    }
    let with_data = RipOutcome::Failed {
        title_index: 0,
        code: Some(7022),
        kind: std::io::ErrorKind::Other,
        data: "abcd".into(),
    };
    let e = sink.take_result(&with_data).expect_err("refused");
    assert_eq!(super::super::io_key_refusal(&e), Some(7022));
    assert!(
        e.to_string().contains("abcd"),
        "the data reaches the message: {e}"
    );
    let no_key = sink.take_result(&RipOutcome::NoKey).expect_err("no key");
    assert_eq!(super::super::io_key_refusal(&no_key), Some(7022));

    for err in [
        libfreemkv::Error::AacsVidNeedsDisc,
        libfreemkv::Error::NoDiscKey {
            disc_hash: "ab".into(),
        },
        libfreemkv::Error::FmtsKeyMissing,
    ] {
        let code = err.code();
        let io: std::io::Error = err.into();
        sink.event(&Event::TitleDone {
            idx: 0,
            dest: "mkv:///x.mkv",
            result: Err(&io),
        });
        let result = sink.take_result(&RipOutcome::Halted);
        let Err(mapped) = map_iso_mux_outcome(result, false, "ku", 0.0, Instant::now(), 0, 0)
        else {
            panic!("E{code}: the call site classifies a key refusal");
        };
        assert_eq!(super::super::io_key_refusal(&mapped), Some(code));
    }
}

// `/api/stop` cancels the device's Halt; the engine polls it through the sink.
#[test]
fn engine_sink_cancels_on_the_device_halt() {
    use freemkv_engine::Sink;
    let (sink, ..) = engine_sink("engine_sink_halt");
    assert!(!sink.should_cancel());
    sink.halt.cancel();
    assert!(sink.should_cancel());
}

#[test]
fn fmt_eta_matches_the_dashboard_format() {
    assert_eq!(fmt_eta(0), "0:00");
    assert_eq!(fmt_eta(75), "1:15");
    assert_eq!(fmt_eta(3_725), "1:02:05");
    assert_eq!(fmt_eta(359_999), "99:59:59");
    assert_eq!(fmt_eta(360_000), "");
}

// ── mux_iso end to end through the engine (no media) ────────────────────

// A keyed image of the KU fixture, repointed at `path`: the mux opens its own reader
// there. The fixture's file is left in place for the test's lifetime.
fn image_of(path: &str) -> freemkv_engine::OpenedImage {
    let fx = crate::ku_fixture::bd_image();
    let dir = tempfile::tempdir().unwrap().keep();
    let iso = fx.write(&dir, "fixture.iso");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let keys = crate::ku_fixture::holding(&calls, crate::ku_fixture::K1);
    let opts = freemkv_engine::OpenImageOptions::resolve(keys);
    let src = freemkv_engine::ImageSource::Iso(iso);
    let mut image = freemkv_engine::open_image_with(&src, opts).expect("the fixture opens");
    image.source = freemkv_engine::ImageSource::Iso(path.into());
    image
}

fn inputs_for<'a>(device: &'a str, dest: &std::path::Path) -> MuxInputs<'a> {
    MuxInputs {
        device,
        display_name: "Test".into(),
        disc_format: "bluray".into(),
        tmdb_title: String::new(),
        tmdb_year: 0,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        duration: String::new(),
        codecs: String::new(),
        filename: "Test.mkv".into(),
        total_bytes: 0,
        title_bytes_per_sec: 0.0,
        total_passes: 0,
        bytes_total_disc: 0,
        max_retries: 0,
        bytes_unreadable_at_mux: 0,
        dest_url: format!("mkv://{}", dest.display()),
        batch: 60,
        staging_disc_dir: dest.parent().unwrap().to_path_buf(),
        sweep_damage: SweepDamageSnapshot::default(),
    }
}

fn atomics() -> MuxAtomics {
    MuxAtomics {
        latest_bytes_read: Arc::new(AtomicU64::new(0)),
        rip_last_lba: Arc::new(AtomicU64::new(0)),
        rip_current_batch: Arc::new(AtomicU16::new(0)),
        wd_last_frame: Arc::new(AtomicU64::new(crate::server::util::epoch_secs())),
        wd_bytes: Arc::new(AtomicU64::new(0)),
        input_errors: Arc::new(AtomicU32::new(0)),
    }
}

// An image the engine cannot open fails before any output: a setup failure the
// caller quarantines, with nothing left behind at the destination.
#[test]
fn mux_iso_maps_an_unopenable_image_to_a_setup_failure() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("Test.mkv");
    let image = image_of("/nonexistent/freemkv-server/none.iso");
    let out = mux_iso(
        inputs_for("mux_iso_unopenable", &dest),
        IsoMuxSource {
            image: &image,
            title_index: 0,
        },
        atomics(),
    )
    .expect("a setup failure maps into the outcome, not Err");
    assert!(!out.completed && !out.output_opened);
    let reason = out.finalize_error.expect("setup failure carries a reason");
    assert!(reason.contains("before output opened"), "{reason}");
    assert!(!dest.exists());
}

// A Stop registered before the mux starts is a halt: Err, so the caller keeps staging.
#[test]
fn mux_iso_honours_a_stop_pressed_before_it_starts() {
    let device = "mux_iso_prestopped";
    let halt = libfreemkv::Halt::new();
    halt.cancel();
    super::super::session::register_halt(device, halt);
    let dir = tempfile::tempdir().unwrap();
    let image = image_of("/nonexistent/freemkv-server/none.iso");
    let err = mux_iso(
        inputs_for(device, &dir.path().join("Test.mkv")),
        IsoMuxSource {
            image: &image,
            title_index: 0,
        },
        atomics(),
    )
    .err()
    .expect("a stopped mux returns the halt");
    assert!(super::super::is_halt_error(&err), "{err}");
    super::super::session::unregister_halt(device);
}
