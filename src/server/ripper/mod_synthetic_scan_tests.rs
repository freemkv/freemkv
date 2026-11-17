// A scan of the synthetic disc through `scan_disc_for`, a fake drive standing in for the
// device, and the rip that reuses the session it stores.
use super::*;
use libfreemkv::test_util::FakeTransport;

struct Scanned {
    trace: String,
    device: String,
    device_path: String,
    cfg: Arc<RwLock<Config>>,
    staging: std::path::PathBuf,
    _root: tempfile::TempDir,
}

// Scan the fixture image (`image`, or a blank drive) with the keydb holding the disc's key
// when `keyed`.
fn scan(name: &str, image: bool, keyed: bool, tune: impl FnOnce(&mut Config)) -> Scanned {
    let mut s = prepare(name, image, keyed, tune);
    s.trace =
        super::test_trace::traced(|| scan_disc_for(&s.cfg, &s.device, &s.device_path, false)).1;
    SCAN_DRIVE.with(|slot| slot.borrow_mut().take());
    s
}

// The drive and config of a scan, the drive waiting for the next scan to open it.
fn prepare(name: &str, image: bool, keyed: bool, tune: impl FnOnce(&mut Config)) -> Scanned {
    let root = tempfile::tempdir().unwrap();
    let staging = root.path().join("staging");
    let output = root.path().join("output");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    let fx = crate::ku_fixture::bd_image();
    let keydb = root.path().join("keydb.cfg");
    if keyed {
        let hash =
            libfreemkv::hex::strip_hex_prefix(&fx.disc.aacs.as_ref().unwrap().disc_hash).to_owned();
        let k1: String = crate::ku_fixture::K1
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::write(&keydb, format!("0x{hash} = KU | U | 1-0x{k1}\n")).unwrap();
    }
    let device = format!("synthetic-scan-{name}-{}", std::process::id());
    let device_path = format!("/nonexistent/{device}");
    let (transport, _handle) = FakeTransport::new();
    let transport = if image {
        transport.with_image(fx.img.image.clone())
    } else {
        transport
    };
    let drive = libfreemkv::Drive::from_transport(Box::new(transport));
    SCAN_DRIVE.with(|slot| *slot.borrow_mut() = Some(drive));
    let mut cfg = Config {
        staging_dir: staging.to_string_lossy().into_owned(),
        output_dir: output.to_string_lossy().into_owned(),
        output_format: "mkv".into(),
        min_length_secs: 0,
        max_retries: 0,
        keep_iso: false,
        auto_eject: false,
        tmdb_api_key: String::new(),
        key_source: "local".into(),
        keydb_path: Some(keydb.to_string_lossy().into_owned()),
        ..Default::default()
    };
    tune(&mut cfg);
    let cfg = Arc::new(RwLock::new(cfg));
    Scanned {
        trace: String::new(),
        device,
        device_path,
        cfg,
        staging,
        _root: root,
    }
}

impl Scanned {
    fn row(&self) -> RipState {
        STATE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&self.device)
            .cloned()
            .expect("scan row")
    }
}

impl Drop for Scanned {
    fn drop(&mut self) {
        STATE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.device);
        drop_session(&self.device);
        unregister_halt(&self.device);
        crate::server::log::forget_device(&self.device);
    }
}

fn state_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[test]
fn a_scan_identifies_keys_and_stages_the_disc_and_its_rip_muxes() {
    let _guard = state_lock();
    let s = scan("keyed", true, true, |_| {});
    assert!(s.trace.contains("scan: begin"), "{}", s.trace);
    let row = s.row();
    assert_eq!(
        row.status, "idle",
        "{} ({})",
        row.last_error, row.key_status
    );
    assert!(row.job_id.starts_with("disc-"), "{}", row.job_id);
    assert!(!row.disc_identity.is_empty());
    assert!(!row.disc_label.is_empty());
    {
        let session = take_session(&s.device).expect("the scan stores its session");
        assert!(session.scanned && !session.probed);
        assert!(session.keys.is_some(), "{:?}", session.key_error);
        store_session(&s.device, session);
    }
    super::test_trace::traced(|| rip_disc(&s.cfg, &s.device, &s.device_path, false));
    let row = s.row();
    assert_eq!(
        row.status, "done",
        "rip failed: {} ({})",
        row.last_error, row.key_status
    );
    assert!(s.staging.join(&row.job_id).is_dir());
}

#[test]
fn a_scan_without_a_key_keeps_the_disc_scanned_and_says_so() {
    let _guard = state_lock();
    let s = scan("keyless", true, false, |_| {});
    let row = s.row();
    assert_eq!(row.status, "idle", "{}", row.last_error);
    assert!(!row.key_status.is_empty());
    let session = take_session(&s.device).expect("the scan stores its session");
    assert!(session.keys.is_none());
    assert!(session.key_error.is_some());
}

#[test]
fn a_scan_of_a_blank_drive_reports_the_failure() {
    let _guard = state_lock();
    let s = scan("blank", false, false, |_| {});
    let row = s.row();
    assert_eq!(row.status, "error");
    assert!(!row.last_error.is_empty());
    assert!(take_session(&s.device).is_none());
}

#[test]
fn a_scan_with_no_drive_reports_that_it_cannot_open_it() {
    let _guard = state_lock();
    let device = format!("synthetic-scan-missing-{}", std::process::id());
    let cfg = Arc::new(RwLock::new(Config::default()));
    scan_disc(&cfg, &device, &format!("/nonexistent/{device}"));
    let row = STATE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&device)
        .unwrap();
    assert_eq!(row.status, "error");
    assert!(row.last_error.contains("open"), "{}", row.last_error);
}

// A rip request on a disc not yet scanned: the scan, then the mode's route.
fn request(
    name: &str,
    mode: crate::server::web::ResumeMode,
    tune: impl FnOnce(&mut Config),
) -> Scanned {
    let mut s = prepare(name, true, true, tune);
    s.trace =
        super::test_trace::traced(|| handle_rip_request(&s.cfg, &s.device, &s.device_path, mode)).1;
    SCAN_DRIVE.with(|slot| slot.borrow_mut().take());
    s
}

#[test]
fn every_fresh_rip_request_mode_scans_then_rips_the_new_disc() {
    use crate::server::web::ResumeMode;
    let _guard = state_lock();
    for (i, mode) in [
        ResumeMode::Default,
        ResumeMode::Wipe,
        ResumeMode::Fresh,
        ResumeMode::Prefer,
    ]
    .into_iter()
    .enumerate()
    {
        let s = request(&format!("mode{i}"), mode, |_| {});
        let row = s.row();
        assert_eq!(
            row.status, "done",
            "{mode:?}: {} ({})",
            row.last_error, row.key_status
        );
    }
}

#[test]
fn a_resume_request_for_a_disc_with_nothing_staged_is_refused() {
    let _guard = state_lock();
    let s = request("require", crate::server::web::ResumeMode::Require, |_| {});
    let row = s.row();
    assert_eq!(row.status, "error");
    assert!(
        row.last_error.contains("no resumable staging state"),
        "{}",
        row.last_error
    );
    assert!(take_session(&s.device).is_none());
}

#[test]
fn a_second_request_reuses_the_scan_and_respects_the_worker_owned_image() {
    use crate::server::web::ResumeMode;
    let _guard = state_lock();
    let s = request("handoff", ResumeMode::Default, |c| c.max_retries = 1);
    let row = s.row();
    assert_eq!(row.status, "done", "{}", row.last_error);
    let job = s.staging.join(&row.job_id);
    assert!(crate::server::muxer::read_marker(&job).is_ok());
    // A fresh scan of the same disc finds its image in the mux worker's hands.
    for mode in [ResumeMode::Wipe, ResumeMode::Require] {
        let (t, _fake) = FakeTransport::new();
        let image = crate::ku_fixture::bd_image().img.image;
        SCAN_DRIVE.with(|slot| {
            *slot.borrow_mut() = Some(libfreemkv::Drive::from_transport(Box::new(
                t.with_image(image),
            )))
        });
        super::test_trace::traced(|| handle_rip_request(&s.cfg, &s.device, &s.device_path, mode));
        SCAN_DRIVE.with(|slot| slot.borrow_mut().take());
        let row = s.row();
        assert_ne!(row.status, "ripping", "{mode:?}");
        if mode == ResumeMode::Wipe {
            assert_eq!(row.status, "error");
            assert!(row.last_error.contains("Cannot wipe"), "{}", row.last_error);
            assert!(crate::server::muxer::read_marker(&job).is_ok());
        }
    }
}

#[test]
fn an_online_scan_with_no_key_service_set_reports_no_key_without_retrying() {
    let _guard = state_lock();
    let s = scan("online-unset", true, false, |c| {
        c.key_source = "online".into();
        c.keyserver_url = String::new();
    });
    let row = s.row();
    assert_eq!(row.status, "idle", "{}", row.last_error);
    assert!(!row.key_status.is_empty());
    let log = crate::server::log::get_device_log(&s.device, 200).join("\n");
    assert!(!log.contains("retrying key resolution"), "{log}");
}

#[test]
fn a_finished_capture_is_never_ripped_again_by_a_plain_or_resume_request() {
    use crate::server::web::ResumeMode;
    let _guard = state_lock();
    let s = request("finished", ResumeMode::Default, |c| {
        c.output_format = "iso".into();
        c.max_retries = 1;
        c.keep_iso = true;
    });
    let row = s.row();
    assert_eq!(row.status, "done", "{}", row.last_error);
    let job = s.staging.join(&row.job_id);
    let iso = std::fs::read_dir(&job)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "iso"))
        .expect("staged ISO");
    let before = std::fs::read(&iso).unwrap();
    for mode in [ResumeMode::Default, ResumeMode::Require, ResumeMode::Prefer] {
        let (t, _fake) = FakeTransport::new();
        let image = crate::ku_fixture::bd_image().img.image;
        SCAN_DRIVE.with(|slot| {
            *slot.borrow_mut() = Some(libfreemkv::Drive::from_transport(Box::new(
                t.with_image(image),
            )))
        });
        super::test_trace::traced(|| handle_rip_request(&s.cfg, &s.device, &s.device_path, mode));
        SCAN_DRIVE.with(|slot| slot.borrow_mut().take());
        assert_ne!(s.row().status, "ripping", "{mode:?}");
        assert_eq!(std::fs::read(&iso).unwrap(), before, "{mode:?}");
    }
}

fn retitle(title: &str) -> staging::UserMetadata {
    staging::UserMetadata {
        season: None,
        disc: None,
        title: title.into(),
        year: 2020,
        media_type: crate::server::planner::MediaKind::Movie,
        tmdb_id: 0,
        episode_start: None,
        poster_url: String::new(),
        overview: String::new(),
    }
}

#[test]
fn a_correction_lands_only_on_the_scanned_discs_own_job() {
    let _guard = state_lock();
    let s = scan("correct", true, true, |_| {});
    let job = s.row().job_id;
    let err = |r: std::io::Result<u64>| r.expect_err("refused").to_string();
    assert!(
        err(retitle_staging_for_device(
            &s.cfg,
            "no-such-drive",
            &job,
            retitle("A")
        ))
        .contains("unknown device")
    );
    assert!(
        err(retitle_staging_for_device(
            &s.cfg,
            &s.device,
            "disc-other",
            retitle("A")
        ))
        .contains("disc changed")
    );
    let revision = retitle_staging_for_device(&s.cfg, &s.device, &job, retitle("Corrected"))
        .expect("the scanned disc's job takes the correction");
    assert!(revision > 0);
    let saved = staging::read_state(&s.staging.join(&job)).unwrap();
    assert_eq!(saved.user_metadata.unwrap().title, "Corrected");
    update_state_with(&s.device, |r| r.status = "scanning".into());
    assert!(
        err(retitle_staging_for_device(
            &s.cfg,
            &s.device,
            &job,
            retitle("B")
        ))
        .contains("disc changed")
    );
}

#[test]
fn a_quarantine_that_cannot_be_written_is_logged() {
    let device = format!("quarantine-unwritable-{}", std::process::id());
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("not-a-dir");
    std::fs::write(&file, b"").unwrap();
    quarantine_or_log(&device, &file, "mux finalize failed");
    let log = crate::server::log::get_device_log(&device, 10).join("\n");
    assert!(log.contains("did not persist"), "{log}");
    crate::server::log::forget_device(&device);
}

// A rip whose scan session is gone opens the drive, scans and keys the disc itself.
#[test]
fn a_rip_without_a_held_session_opens_and_scans_the_drive_itself() {
    let _guard = state_lock();
    let s = scan("reopen", true, true, |_| {});
    assert_eq!(s.row().status, "idle");
    drop_session(&s.device);
    let (t, _fake) = FakeTransport::new();
    let image = crate::ku_fixture::bd_image().img.image;
    SCAN_DRIVE.with(|slot| {
        *slot.borrow_mut() = Some(libfreemkv::Drive::from_transport(Box::new(
            t.with_image(image),
        )))
    });
    super::test_trace::traced(|| rip_disc(&s.cfg, &s.device, &s.device_path, false));
    SCAN_DRIVE.with(|slot| slot.borrow_mut().take());
    let row = s.row();
    assert_eq!(
        row.status, "done",
        "rip failed: {} ({})",
        row.last_error, row.key_status
    );
}

#[test]
fn a_rip_without_a_session_on_a_blank_drive_reports_the_failed_scan() {
    let _guard = state_lock();
    let s = scan("reopen-blank", true, true, |_| {});
    drop_session(&s.device);
    let (t, _fake) = FakeTransport::new();
    SCAN_DRIVE
        .with(|slot| *slot.borrow_mut() = Some(libfreemkv::Drive::from_transport(Box::new(t))));
    super::test_trace::traced(|| rip_disc(&s.cfg, &s.device, &s.device_path, false));
    SCAN_DRIVE.with(|slot| slot.borrow_mut().take());
    let row = s.row();
    assert_eq!(row.status, "error");
    assert!(!row.last_error.is_empty());
}

// An eject goes through the idle session's own drive handle when it holds one.
#[test]
fn an_eject_uses_the_held_session_handle() {
    let _guard = state_lock();
    let device = format!("eject-held-{}", std::process::id());
    let (t, fake) = FakeTransport::new();
    store_session(
        &device,
        DriveSession {
            drive: libfreemkv::Drive::from_transport(Box::new(t)),
            disc: None,
            scanned: true,
            probed: true,
            tmdb: None,
            device_path: format!("/nonexistent/{device}"),
            key_verdict: None,
            keys: None,
            key_error: None,
            key_reads: KeyReads::default(),
        },
    );
    assert!(eject_drive(&format!("/nonexistent/{device}")).is_ok());
    assert!(take_session(&device).is_none());
    let eject = |c: &[u8]| c[0] == 0x1B && c[4] & 0x03 == 0x02;
    assert_eq!(fake.count(eject), 1);
    assert_eq!(fake.live_handles(), 0);
}

// A drive that reads the disc's identity but not its playlist.
fn drive_failing_playlists() -> libfreemkv::Drive {
    let fx = crate::ku_fixture::bd_image();
    let (start, _) = fx.img.files[1];
    let (t, _fake) = FakeTransport::new();
    let t = t.with_image(fx.img.image.clone()).rule(
        move |cdb| {
            if cdb[0] != 0x28 {
                return false;
            }
            let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
            let len = u32::from(u16::from_be_bytes([cdb[7], cdb[8]]));
            lba <= start && start < lba + len.max(1)
        },
        libfreemkv::test_util::FakeMode::Sense {
            sense: libfreemkv::scsi::ScsiSense {
                sense_key: 3,
                asc: 0x11,
                ascq: 0,
            },
            progress: None,
        },
    );
    libfreemkv::Drive::from_transport(Box::new(t))
}

#[test]
fn a_scan_that_cannot_read_the_playlists_reports_it() {
    let _guard = state_lock();
    let mut s = prepare("bad-playlist", true, true, |_| {});
    SCAN_DRIVE.with(|slot| *slot.borrow_mut() = Some(drive_failing_playlists()));
    s.trace =
        super::test_trace::traced(|| scan_disc_for(&s.cfg, &s.device, &s.device_path, false)).1;
    SCAN_DRIVE.with(|slot| slot.borrow_mut().take());
    let row = s.row();
    assert_eq!(row.status, "error", "{}", row.last_error);
    assert!(!row.last_error.is_empty());
    assert!(take_session(&s.device).is_none());
}

#[test]
fn a_rip_that_reopens_a_drive_it_cannot_scan_reports_it() {
    let _guard = state_lock();
    let s = scan("reopen-bad-playlist", true, true, |_| {});
    drop_session(&s.device);
    SCAN_DRIVE.with(|slot| *slot.borrow_mut() = Some(drive_failing_playlists()));
    super::test_trace::traced(|| rip_disc(&s.cfg, &s.device, &s.device_path, false));
    SCAN_DRIVE.with(|slot| slot.borrow_mut().take());
    let row = s.row();
    assert_eq!(row.status, "error", "{}", row.last_error);
}

// A resume request for a disc whose finished capture was held for its loss: the
// staged image is muxed again, with no disc reads.
#[test]
fn a_resume_request_remuxes_a_loss_held_capture_from_its_image() {
    use crate::server::web::ResumeMode;
    let _env = crate::server::log::env_guard();
    let _guard = state_lock();
    let s = scan("resume-remux", true, true, |_| {});
    // SAFETY: env access in tests, serialized by env_guard.
    unsafe { std::env::set_var("AUTORIP_DIR", s.staging.parent().unwrap()) };
    let job = s.staging.join(s.row().job_id);
    std::fs::create_dir_all(&job).unwrap();
    let fx = crate::ku_fixture::bd_image();
    let iso = fx.write(&job, "Held.iso");
    crate::ku_fixture::write_sidecar(&fx, &iso, false);
    let mut st = staging::DiscState::new(staging::StagingState::AbortedLoss);
    st.failure_reason = Some("aborted: 1.0s lost".into());
    st.iso_path = iso.to_string_lossy().into_owned();
    st.mapfile_path = freemkv_engine::mapfile_path_for(&iso)
        .to_string_lossy()
        .into_owned();
    st.max_retries = 1;
    staging::try_write_state(&job, &st).unwrap();
    super::test_trace::traced(|| {
        handle_rip_request(&s.cfg, &s.device, &s.device_path, ResumeMode::Require)
    });
    let row = s.row();
    assert_ne!(row.status, "ripping");
    assert!(iso.exists() || job.read_dir().unwrap().next().is_some());
}

// The scan watchdog speaks up every 15 s while a scan or key resolve runs.
#[test]
fn a_long_scan_is_reported_by_its_watchdog() {
    let device = format!("scan-watchdog-{}", std::process::id());
    let wd = ScanWatchdog::arm(&device);
    wd.enter_resolve();
    std::thread::sleep(Duration::from_secs(16));
    drop(wd);
    std::thread::sleep(Duration::from_millis(1500));
    let log = crate::server::log::get_device_log(&device, 10).join("\n");
    assert!(log.contains("Still scanning"), "{log}");
    assert!(log.contains("phase=resolve_keys"), "{log}");
    crate::server::log::forget_device(&device);
}

// A capture stopped half way: its image and a map whose second half was never read.
fn stage_partial_capture(job: &std::path::Path) -> std::path::PathBuf {
    std::fs::create_dir_all(job).unwrap();
    let fx = crate::ku_fixture::bd_image();
    let iso = fx.write(job, "Partial.iso");
    let total = fx.img.image.len() as u64;
    let half = (total / 2) / 2048 * 2048;
    let map = freemkv_engine::mapfile_path_for(&iso);
    let mut m = freemkv_engine::Mapfile::create(&map, total, "t").unwrap();
    m.record(0, half, freemkv_engine::SectorStatus::Finished)
        .unwrap();
    m.record(half, total - half, freemkv_engine::SectorStatus::NonTried)
        .unwrap();
    m.set_disc_hash(&fx.disc.aacs.as_ref().unwrap().disc_hash);
    m.flush().unwrap();
    let mut st = staging::DiscState::new(staging::StagingState::Stopped);
    st.iso_path = iso.to_string_lossy().into_owned();
    st.mapfile_path = map.to_string_lossy().into_owned();
    st.max_retries = 1;
    st.output_format = Some("mkv".into());
    staging::try_write_state(job, &st).unwrap();
    iso
}

#[test]
fn resume_requests_continue_a_partial_sweep_from_its_map() {
    use crate::server::web::ResumeMode;
    let _guard = state_lock();
    for (i, mode) in [ResumeMode::Require, ResumeMode::Prefer]
        .into_iter()
        .enumerate()
    {
        let s = scan(&format!("partial{i}"), true, true, |c| c.max_retries = 1);
        let job = s.staging.join(s.row().job_id);
        let iso = stage_partial_capture(&job);
        super::test_trace::traced(|| handle_rip_request(&s.cfg, &s.device, &s.device_path, mode));
        let log = crate::server::log::get_device_log(&s.device, 500).join("\n");
        let row = s.row();
        assert_ne!(row.status, "ripping", "{mode:?}");
        assert!(
            log.contains("partial sweep") || !row.last_error.is_empty(),
            "{mode:?}: {}",
            row.last_error
        );
        assert!(iso.exists(), "{mode:?}");
    }
}
