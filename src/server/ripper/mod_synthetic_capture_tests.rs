use super::*;
use libfreemkv::test_util::FakeTransport;

#[test]
fn a_held_synthetic_disc_completes_a_whole_disc_iso_capture() {
    let _state_guard = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let root = tempfile::tempdir().unwrap();
    let staging_root = root.path().join("staging");
    let output_root = root.path().join("output");
    std::fs::create_dir_all(&staging_root).unwrap();
    std::fs::create_dir_all(&output_root).unwrap();
    let fx = crate::ku_fixture::bd_image();
    let identity = jobs::aacs_identity(&fx.disc.aacs.as_ref().unwrap().disc_hash).unwrap();
    let job = format!("disc-{identity}");
    let device = format!("synthetic-iso-{}", std::process::id());
    let device_path = format!("/nonexistent/{device}");
    let (transport, handle) = FakeTransport::new();
    let mut drive =
        libfreemkv::Drive::from_transport(Box::new(transport.with_image(fx.img.image.clone())));
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let keys = freemkv_engine::keys::resolve_for_rip(
        &fx.disc,
        &mut drive,
        libfreemkv::keys::KeyScope::WholeDisc,
        &crate::ku_fixture::holding(&requests, crate::ku_fixture::K1),
        None,
        None,
    )
    .unwrap();
    store_session(
        &device,
        DriveSession {
            drive,
            disc: Some(fx.scan()),
            scanned: true,
            probed: true,
            tmdb: None,
            device_path: device_path.clone(),
            key_verdict: None,
            keys: Some(keys),
            key_error: None,
            key_reads: KeyReads::default(),
        },
    );
    update_state(
        &device,
        RipState {
            device: device.clone(),
            job_id: job.clone(),
            disc_identity: identity,
            disc_present: true,
            status: "idle".into(),
            ..Default::default()
        },
    );
    let cfg = Config {
        staging_dir: staging_root.to_string_lossy().into_owned(),
        output_dir: output_root.to_string_lossy().into_owned(),
        output_format: "iso".into(),
        min_length_secs: 0,
        max_retries: 1,
        keep_iso: true,
        auto_eject: false,
        tmdb_api_key: String::new(),
        ..Default::default()
    };
    let cfg = Arc::new(RwLock::new(cfg));
    rip_disc(&cfg, &device, &device_path, false);
    let row = STATE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&device)
        .unwrap();
    drop_session(&device);
    unregister_halt(&device);
    assert_eq!(
        row.status, "done",
        "capture failed: {} ({})",
        row.last_error, row.key_status
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "capture must reuse the scan's key ring"
    );
    assert_eq!(handle.live_handles(), 0);
    let job_path = staging_root.join(job);
    let map = std::fs::read_dir(&job_path)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|p| p.to_string_lossy().ends_with(".mapfile"))
        .expect("capture mapfile");
    let stats = freemkv_engine::Mapfile::load(&map).unwrap().stats();
    assert_eq!(stats.bytes_good, fx.img.image.len() as u64);
    assert_eq!(stats.bytes_unreadable, 0);
}

// One synthetic rip of the fixture disc: its card, the job's staging dir and the fake's
// open handles once the rip returned.
struct Rip {
    row: RipState,
    job_path: std::path::PathBuf,
    live_handles: usize,
    _root: tempfile::TempDir,
}

// Rip the fixture through `rip_disc_inner` with `tune` applied to a base config. `keyed`
// stores the scan's key ring in the session; without it the keydb is missing.
fn synthetic_rip(name: &str, keyed: bool, wipe: bool, tune: impl FnOnce(&mut Config)) -> Rip {
    synthetic_rip_on(name, keyed, wipe, |t, _| t, tune)
}

// `synthetic_rip` on a drive `shape` scripts, given the fixture it serves.
fn synthetic_rip_on(
    name: &str,
    keyed: bool,
    wipe: bool,
    shape: impl FnOnce(FakeTransport, &crate::ku_fixture::Fx) -> FakeTransport,
    tune: impl FnOnce(&mut Config),
) -> Rip {
    let root = tempfile::tempdir().unwrap();
    let staging_root = root.path().join("staging");
    let output_root = root.path().join("output");
    std::fs::create_dir_all(&staging_root).unwrap();
    std::fs::create_dir_all(&output_root).unwrap();
    let fx = crate::ku_fixture::bd_image();
    let identity = jobs::aacs_identity(&fx.disc.aacs.as_ref().unwrap().disc_hash).unwrap();
    let job = format!("disc-{identity}");
    let device = format!("synthetic-{name}-{}", std::process::id());
    let device_path = format!("/nonexistent/{device}");
    let (transport, handle) = FakeTransport::new();
    let transport = shape(transport.with_image(fx.img.image.clone()), &fx);
    let drive = libfreemkv::Drive::from_transport(Box::new(transport));
    // Resolved over the image, so a scripted drive fault never reaches the key walk.
    let keys = keyed.then(|| {
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        freemkv_engine::keys::resolve_for_rip(
            &fx.disc,
            &mut libfreemkv::test_util::MemSource::new(fx.img.image.clone()),
            libfreemkv::keys::KeyScope::WholeDisc,
            &crate::ku_fixture::holding(&requests, crate::ku_fixture::K1),
            None,
            None,
        )
        .unwrap()
    });
    store_session(
        &device,
        DriveSession {
            drive,
            disc: Some(fx.scan()),
            scanned: true,
            probed: true,
            tmdb: None,
            device_path: device_path.clone(),
            key_verdict: None,
            keys,
            key_error: None,
            key_reads: KeyReads::default(),
        },
    );
    update_state(
        &device,
        RipState {
            device: device.clone(),
            job_id: job.clone(),
            disc_identity: identity,
            disc_present: true,
            status: "idle".into(),
            ..Default::default()
        },
    );
    let mut cfg = Config {
        staging_dir: staging_root.to_string_lossy().into_owned(),
        output_dir: output_root.to_string_lossy().into_owned(),
        output_format: "mkv".into(),
        min_length_secs: 0,
        max_retries: 0,
        keep_iso: false,
        auto_eject: false,
        tmdb_api_key: String::new(),
        key_source: "local".into(),
        keydb_path: Some(
            root.path()
                .join("no-keydb.cfg")
                .to_string_lossy()
                .into_owned(),
        ),
        ..Default::default()
    };
    tune(&mut cfg);
    let cfg = Arc::new(RwLock::new(cfg));
    rip_disc_inner(&cfg, &device, &device_path, false, wipe, true);
    let row = STATE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&device)
        .unwrap();
    drop_session(&device);
    unregister_halt(&device);
    Rip {
        row,
        job_path: staging_root.join(job),
        live_handles: handle.live_handles(),
        _root: root,
    }
}

fn files_ending(dir: &std::path::Path, suffix: &str) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.to_string_lossy().ends_with(suffix))
                .collect()
        })
        .unwrap_or_default()
}

fn state_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[test]
fn a_single_pass_mkv_rip_muxes_live_from_the_drive() {
    let _guard = state_lock();
    let rip = synthetic_rip("live-mkv", true, false, |_| {});
    assert_eq!(
        rip.row.status, "done",
        "live mux failed: {} ({})",
        rip.row.last_error, rip.row.key_status
    );
    assert_eq!(rip.row.progress_pct, 100);
    assert!(!files_ending(&rip.job_path, ".mkv").is_empty());
    let st = staging::read_state(&rip.job_path).expect("hand-off state");
    assert!(
        matches!(
            st.state,
            staging::StagingState::Done
                | staging::StagingState::Review
                | staging::StagingState::Completed
        ),
        "{:?}",
        st.state
    );
    assert_eq!(st.outputs.len(), 1);
}

#[test]
fn a_single_pass_m2ts_rip_with_auto_eject_still_completes() {
    let _guard = state_lock();
    let rip = synthetic_rip("live-m2ts", true, false, |c| {
        c.output_format = "m2ts".into();
        c.auto_eject = true;
    });
    assert_eq!(rip.row.status, "done", "{}", rip.row.last_error);
    assert_eq!(rip.live_handles, 0);
}

#[test]
fn a_multipass_mkv_rip_hands_the_image_to_the_mux_worker() {
    let _guard = state_lock();
    let rip = synthetic_rip("handoff-mkv", true, false, |c| c.max_retries = 1);
    assert_eq!(
        rip.row.status, "done",
        "hand-off failed: {} ({})",
        rip.row.last_error, rip.row.key_status
    );
    let marker = crate::server::muxer::read_marker(&rip.job_path).expect(".ripped marker");
    assert_eq!(marker.max_retries, 1);
    assert!(marker.iso_path.ends_with(".iso"), "{}", marker.iso_path);
    assert!(std::path::Path::new(&marker.iso_path).exists());
}

#[test]
fn a_multipass_rip_keeping_its_iso_with_auto_eject_hands_off_and_ejects() {
    let _guard = state_lock();
    let rip = synthetic_rip("handoff-eject", true, false, |c| {
        c.max_retries = 1;
        c.keep_iso = true;
        c.auto_eject = true;
    });
    assert_eq!(rip.row.status, "done", "{}", rip.row.last_error);
    assert!(crate::server::muxer::read_marker(&rip.job_path).is_ok());
    assert_eq!(rip.live_handles, 0);
}

#[test]
fn an_iso_rip_with_wipe_and_auto_eject_completes() {
    let _guard = state_lock();
    let rip = synthetic_rip("iso-wipe", true, true, |c| {
        c.output_format = "iso".into();
        c.max_retries = 1;
        c.keep_iso = true;
        c.auto_eject = true;
    });
    assert_eq!(rip.row.status, "done", "{}", rip.row.last_error);
    assert!(!files_ending(&rip.job_path, ".iso").is_empty());
}

#[test]
fn a_keyless_disc_is_not_ripped_without_capture_without_keys() {
    let _guard = state_lock();
    let rip = synthetic_rip("keyless-refuse", false, false, |c| {
        c.capture_without_keys = false;
    });
    assert_eq!(rip.row.status, "error");
    assert!(!rip.row.last_error.is_empty());
    assert!(files_ending(&rip.job_path, ".mkv").is_empty());
}

#[test]
fn a_keyless_single_pass_rip_cannot_mux() {
    let _guard = state_lock();
    let rip = synthetic_rip("keyless-live", false, false, |c| {
        c.capture_without_keys = true;
    });
    assert_eq!(rip.row.status, "error");
    assert!(
        rip.row.last_error.contains("Cannot mux without keys"),
        "{}",
        rip.row.last_error
    );
}

#[test]
fn a_keyless_multipass_rip_captures_the_image_for_a_deferred_mux() {
    let _guard = state_lock();
    let rip = synthetic_rip("keyless-capture", false, false, |c| {
        c.capture_without_keys = true;
        c.max_retries = 1;
    });
    assert_ne!(rip.row.status, "ripping", "{}", rip.row.last_error);
    assert!(!files_ending(&rip.job_path, ".iso").is_empty());
}

#[test]
fn a_keyless_online_rip_with_no_key_service_set_is_refused() {
    let _guard = state_lock();
    let rip = synthetic_rip("keyless-online", false, false, |c| {
        c.key_source = "online".into();
        c.keyserver_url = String::new();
    });
    assert_eq!(rip.row.status, "error");
    assert!(!rip.row.last_error.is_empty());
}

// Sectors of the feature's clip that never read: a multipass MKV rip with no loss
// tolerance aborts on them and records a resumable loss abort.
#[test]
fn a_multipass_rip_that_loses_feature_sectors_aborts_on_the_loss() {
    let _guard = state_lock();
    let started = std::time::Instant::now();
    let rip = synthetic_rip_on(
        "loss-abort",
        true,
        false,
        |t, fx| {
            let (start, sectors) = fx.img.files[3];
            let bad = start + sectors / 2;
            t.rule(
                move |cdb| {
                    if cdb[0] != 0x28 {
                        return false;
                    }
                    let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]);
                    let len = u32::from(u16::from_be_bytes([cdb[7], cdb[8]]));
                    lba <= bad && bad < lba + len.max(1)
                },
                libfreemkv::test_util::FakeMode::Sense {
                    sense: libfreemkv::scsi::ScsiSense {
                        sense_key: 3,
                        asc: 0x11,
                        ascq: 0,
                    },
                    progress: None,
                },
            )
            .rule(|cdb| cdb[0] == 0x1B, libfreemkv::test_util::FakeMode::Fault)
        },
        |c| {
            c.max_retries = 1;
            c.abort_on_lost_secs = 0;
        },
    );
    assert_eq!(
        rip.row.status,
        "error",
        "{} ({:?})",
        rip.row.last_error,
        started.elapsed()
    );
    assert!(rip.row.loss_aborted, "{}", rip.row.last_error);
    let st = staging::read_state(&rip.job_path).expect("loss-abort state");
    assert_eq!(st.state, staging::StagingState::AbortedLoss);
    assert!(!files_ending(&rip.job_path, ".iso").is_empty());
}
