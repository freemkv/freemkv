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
