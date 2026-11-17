use super::*;
use freemkv_engine::Sink;

#[test]
fn recovery_milestones_publish_pass_state_damage_and_actionable_logs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.mapfile");
    let fx = crate::ku_fixture::bd_image();
    let device = format!("recovery-milestones-{}", std::process::id());
    let ctx = PassContext {
        device: device.clone(),
        display_name: "Synthetic capture".into(),
        disc_format: "BD".into(),
        tmdb_title: String::new(),
        tmdb_year: 0,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        tmdb_media_type: None,
        duration: String::new(),
        codecs: String::new(),
        filename: String::new(),
        bytes_total_disc: fx.img.image.len() as u64,
        bytes_sweep: fx.img.image.len() as u64,
        batch: 32,
        max_retries: 2,
    };
    let map = freemkv_engine::Mapfile::create(&path, ctx.bytes_total_disc, "test").unwrap();
    let halt = Arc::new(AtomicBool::new(false));
    let sink = ServerPassSink::new(
        &ctx,
        &fx.disc.titles[0],
        2048.0,
        3,
        true,
        &path,
        halt.clone(),
    );
    let io_error = std::io::Error::other("synthetic map unavailable");
    for event in [
        RecoveryEvent::PassStart {
            pass: 1,
            good: 0,
            pending: ctx.bytes_total_disc,
            unreadable: 0,
        },
        RecoveryEvent::PassDone {
            pass: 1,
            good: 2048,
            pending: 2048,
            unreadable: 0,
            recovered: 0,
            halted: false,
            wedged: false,
        },
        RecoveryEvent::PatchesStart {
            max: 2,
            pending: 2048,
        },
        RecoveryEvent::PassStart {
            pass: 2,
            good: 2048,
            pending: 2048,
            unreadable: 0,
        },
        RecoveryEvent::PassDone {
            pass: 2,
            good: 2048,
            pending: 2048,
            unreadable: 0,
            recovered: 0,
            halted: false,
            wedged: true,
        },
        RecoveryEvent::PassDone {
            pass: 3,
            good: 2048,
            pending: 2048,
            unreadable: 0,
            recovered: 0,
            halted: true,
            wedged: false,
        },
        RecoveryEvent::Stopped { pass: 3 },
        RecoveryEvent::MapUnreadable {
            pass: 2,
            error: &io_error,
        },
        RecoveryEvent::Converged { pass: 3 },
        RecoveryEvent::NoProgress {
            pass: 3,
            recovered: 0,
        },
        RecoveryEvent::Promoted {
            map: &map,
            intact: true,
        },
        RecoveryEvent::LossUnmeasured { error: &io_error },
    ] {
        sink.event(&freemkv_engine::Event::Recovery(&event));
    }
    let row = state::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&device)
        .unwrap();
    assert_eq!(row.pass, 2);
    assert_eq!(row.total_passes, 3);
    assert_eq!(row.errors, 0);
    assert!(!sink.should_cancel());
    halt.store(true, Ordering::Relaxed);
    assert!(sink.should_cancel());
    let log = crate::server::log::get_device_log(&device, 100).join("\n");
    for text in [
        "disc → ISO",
        "retrying bad ranges",
        "DRIVE WEDGED",
        "(halt)",
        "100% recovered",
        "STRATEGY_FAILURE",
        "forcing abort",
    ] {
        assert!(log.contains(text), "missing {text}: {log}");
    }
    crate::server::log::forget_device(&device);
}
