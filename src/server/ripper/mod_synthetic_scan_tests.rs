// A scan of the synthetic disc through `scan_disc_for`, a fake drive standing in for the
// device, and the rip that reuses the session it stores.
use super::*;
use libfreemkv::test_util::FakeTransport;

struct Scanned {
    device: String,
    device_path: String,
    cfg: Arc<RwLock<Config>>,
    staging: std::path::PathBuf,
    _root: tempfile::TempDir,
}

// Scan the fixture image (`image`, or a blank drive) with the keydb holding the disc's key
// when `keyed`.
fn scan(name: &str, image: bool, keyed: bool, tune: impl FnOnce(&mut Config)) -> Scanned {
    let s = prepare(name, image, keyed, tune);
    scan_disc_for(&s.cfg, &s.device, &s.device_path, false);
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
    rip_disc(&s.cfg, &s.device, &s.device_path, false);
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
    let s = prepare(name, true, true, tune);
    handle_rip_request(&s.cfg, &s.device, &s.device_path, mode);
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
        handle_rip_request(&s.cfg, &s.device, &s.device_path, mode);
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
