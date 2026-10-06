use super::*;

fn halted(dev: &str) -> libfreemkv::Halt {
    let halt = libfreemkv::Halt::new();
    halt.cancel();
    register_halt(dev, halt.clone());
    halt
}

// A Stop during a key-service backoff ends the wait at once instead of after 8-32s.
#[test]
fn a_stop_ends_the_retry_wait_promptly() {
    let dev = format!("fa_wait_{}", std::process::id());
    let _halt = halted(&dev);
    let t0 = std::time::Instant::now();
    assert!(!wait_unless_stopped(&dev, Duration::from_secs(5)));
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    unregister_halt(&dev);
}

// A Stop during transport recovery ends the Drive::open backoff instead of sleeping it out.
#[test]
fn a_stop_ends_the_drive_reopen_backoff() {
    let dev = format!("fa_reopen_{}", std::process::id());
    let _halt = halted(&dev);
    let t0 = std::time::Instant::now();
    let path = format!("/nonexistent/{dev}");
    assert!(open_drive_with_backoff(&dev, 1, &path, 3).is_none());
    assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    unregister_halt(&dev);
}

// The poll loop's idle/error view never replaces a row a worker claimed after its busy check.
#[test]
fn the_poll_view_leaves_a_claimed_row_alone() {
    let dev = format!("fa_poll_{}", std::process::id());
    let claim_gen = try_claim_active(&dev).expect("claim");
    let idle = || RipState {
        device: dev.clone(),
        status: "idle".to_string(),
        ..Default::default()
    };
    assert!(!publish_poll_row(&dev, idle()));
    let row = STATE.lock().unwrap().get(&dev).cloned().unwrap();
    assert_eq!(row.status, "scanning", "the claim stands");

    update_state_with(&dev, |s| s.status = "done".to_string());
    assert!(publish_poll_row(&dev, idle()));
    let row = STATE.lock().unwrap().get(&dev).cloned().unwrap();
    assert_eq!(row.status, "idle");
    assert_eq!(row.claim_gen, claim_gen, "the claim generation is kept");
    STATE.lock().unwrap().remove(&dev);
}

// The same disc in two drives: the second fresh rip cannot claim the staging dir the
// first is about to wipe and sweep, until the first returns.
#[test]
fn a_fresh_rip_claim_excludes_another_drive_until_dropped() {
    let base = format!("/staging/FA_Claim_{}", std::process::id());
    let first = claim_fresh_rip(&base, "fa_drive_a").expect("first claim");
    assert!(claim_fresh_rip(&base, "fa_drive_b").is_none());
    drop(first);
    assert!(claim_fresh_rip(&base, "fa_drive_b").is_some());
}

// A disc whose scan failed is not ripped: rip_disc would rescan and sweep into a staging
// dir no guard checked (the failed scan left no name to check it by).
#[test]
fn a_failed_scan_does_not_go_on_to_rip() {
    let _env = crate::server::log::env_guard();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("logs")).unwrap();
    // SAFETY: serialized by the env guard held for the whole test.
    unsafe { std::env::set_var("AUTORIP_DIR", tmp.path()) };
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let cfg = Arc::new(RwLock::new(Config {
        staging_dir: staging.to_string_lossy().into_owned(),
        ..Config::default()
    }));
    let dev = format!("fa_scanfail_{}", std::process::id());
    let path = format!("/nonexistent/{dev}");
    handle_rip_request(&cfg, &dev, &path, crate::server::web::ResumeMode::Fresh);
    let opens = crate::server::log::get_device_log(&dev, 200)
        .iter()
        .filter(|l| l.ends_with("Opening drive..."))
        .count();
    assert_eq!(opens, 1, "only the scan opened the drive");
    STATE.lock().unwrap().remove(&dev);
}

// A Windows device path keys the eject by the same short name STATE/HALTS use.
#[test]
fn eject_addresses_a_backslash_path_by_its_device_key() {
    let key = format!("FaCdRom{}", std::process::id());
    let halt = libfreemkv::Halt::new();
    register_halt(&key, halt.clone());
    eject_drive(&format!(r"\\.\{key}"));
    assert!(halt.is_cancelled(), "the device's rip was stopped");
    assert!(device_halt(&key).is_none(), "and its halt unregistered");
}

// An ISO output delivers the image: the log line and tile name it, not a `.mkv`.
#[test]
fn an_iso_rip_names_its_image_as_the_delivered_file() {
    assert_eq!(
        delivered_file_name(
            crate::server::config::OUTPUT_FORMAT_ISO,
            "Foo.mkv",
            "Foo.iso"
        ),
        "Foo.iso"
    );
    assert_eq!(
        delivered_file_name("mkv", "Foo.mk3d", "Foo.iso"),
        "Foo.mk3d"
    );
}

// A single-pass FMTS rip captures no ISO, so its log must not promise one.
#[test]
fn fmts_capture_only_promises_an_iso_only_in_multipass() {
    assert!(fmts_capture_only_log(true).contains("capturing raw ISO now"));
    let single = fmts_capture_only_log(false);
    assert!(!single.contains("capturing raw ISO now"), "{single}");
    assert!(!single.contains("mux deferred"), "{single}");
}

// A loss-abort marker that did not land is said in the device log, not swallowed.
#[test]
fn an_unwritten_loss_abort_marker_is_reported() {
    let _env = crate::server::log::env_guard();
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("logs")).unwrap();
    // SAFETY: serialized by the env guard held for the whole test.
    unsafe { std::env::set_var("AUTORIP_DIR", tmp.path()) };
    // A regular file where the staging dir should be: no state.json can be written.
    let not_a_dir = tmp.path().join("Film");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let dev = format!("fa_lossabort_{}", std::process::id());
    record_rip_loss_abort(&dev, &not_a_dir, "aborted: 3s lost");
    let log = crate::server::log::get_device_log(&dev, 50);
    assert!(
        log.iter()
            .any(|l| l.contains(".aborted-loss marker") && l.contains("did not persist")),
        "{log:?}"
    );
    crate::server::muxer::clear_error(&not_a_dir.to_string_lossy());
}

fn marker(iso: &std::path::Path) -> crate::server::muxer::RippedMarker {
    crate::server::muxer::RippedMarker {
        schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
        iso_path: iso.to_string_lossy().into_owned(),
        mapfile_path: format!("{}.mapfile", iso.display()),
        display_name: "Show".to_string(),
        disc_format: "bluray".to_string(),
        mkv_filename: "Show.mkv".to_string(),
        tmdb_title: "Show".to_string(),
        tmdb_year: 2020,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        tmdb_media_type: "tv".to_string(),
        max_retries: 1,
        abort_on_lost_secs: 0,
        rip_elapsed_secs: 0.0,
        rip_errors: 0,
        rip_lost_video_secs: 0.0,
        rip_last_sector: 0,
        origin_device: "fa_handoff".to_string(),
        sweep_errors: 0,
        sweep_total_lost_ms: 0.0,
        sweep_main_lost_ms: 0.0,
        sweep_num_bad_ranges: 0,
        sweep_largest_gap_ms: 0.0,
        title_confident: true,
    }
}

fn episodes(n: u16) -> Vec<staging::Output> {
    (1..=n)
        .map(|e| staging::Output {
            filename: format!("Show_S01E{e:02}.mkv"),
            title_index: e as usize,
            episode: Some(e),
            ..Default::default()
        })
        .collect()
}

// The hand-off records the TV plan in the same Ripped state the worker claims, and lends
// the worker the rip's keys.
#[test]
fn the_handoff_lands_the_plan_with_the_ripped_state() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    staging::write_sweeping_marker(dir);
    let iso = dir.join("Show.iso");
    let keys = libfreemkv::keys::KeyRing::none();
    hand_off_to_mux_worker(dir, &marker(&iso), Some(&keys), |s| s.outputs = episodes(3))
        .expect("hand-off");
    let st = staging::read_state(dir).expect("state");
    assert_eq!(st.state, staging::StagingState::Ripped);
    assert_eq!(
        st.outputs,
        episodes(3),
        "the fan-out plan, not one default output"
    );
    assert!(crate::server::keysource::rip_keys_for(&iso).is_some());
    crate::server::keysource::forget_rip_keys(&iso);
}

// A hand-off that did not land keeps no key set in memory for a worker that never comes.
#[test]
fn a_failed_handoff_takes_the_keys_back() {
    let tmp = tempfile::tempdir().unwrap();
    let not_a_dir = tmp.path().join("Show");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let iso = tmp.path().join("Show.iso");
    let keys = libfreemkv::keys::KeyRing::none();
    assert!(hand_off_to_mux_worker(&not_a_dir, &marker(&iso), Some(&keys), |_| {}).is_err());
    assert!(crate::server::keysource::rip_keys_for(&iso).is_none());
}

#[test]
fn loss_text_switches_to_seconds_at_one_second() {
    assert_eq!(fmt_loss(999.4), "999 ms");
    assert_eq!(fmt_loss(1000.0), "1.00s");
    assert_eq!(fmt_loss(-5.0), "0 ms");
    assert_eq!(fmt_loss(f64::NAN), "an unknown amount");
    assert_eq!(fmt_loss(f64::INFINITY), "an unknown amount");
}

#[test]
fn threshold_text_names_a_perfect_rip_at_zero() {
    assert_eq!(fmt_threshold(0), "perfect rip required");
    assert_eq!(fmt_threshold(30), "threshold 30s");
}
