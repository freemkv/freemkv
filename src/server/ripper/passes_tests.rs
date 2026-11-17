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

fn sense(sense_key: u8, asc: u8) -> Option<libfreemkv::scsi::ScsiSense> {
    Some(libfreemkv::scsi::ScsiSense {
        sense_key,
        asc,
        ascq: 0,
    })
}

#[test]
fn a_failed_patch_pass_logs_its_category_and_the_action_it_calls_for() {
    let device = format!("patch-failure-{}", std::process::id());
    let halt = AtomicBool::new(false);
    let cases: Vec<(libfreemkv::Error, &str)> = vec![
        (
            libfreemkv::Error::ScsiError {
                opcode: 0x28,
                status: libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE,
                sense: None,
            },
            "USB bridge crashed",
        ),
        (
            libfreemkv::Error::DiscRead {
                sector: 7,
                status: Some(2),
                sense: sense(4, 0x08),
            },
            "drive may be failing",
        ),
        (
            libfreemkv::Error::ScsiError {
                opcode: 0x28,
                status: 2,
                sense: sense(5, 0x20),
            },
            "firmware wedged",
        ),
        (
            libfreemkv::Error::DeviceNotFound {
                path: "/dev/sg9".into(),
            },
            "DEVICE_ERROR",
        ),
        (
            libfreemkv::Error::IoError {
                source: std::io::Error::other("bus reset"),
            },
            "ERROR_CODE_5",
        ),
    ];
    for (pass, (error, expect)) in cases.iter().enumerate() {
        log_patch_failure(&device, &halt, pass as u32 + 2, error);
        let log = crate::server::log::get_device_log(&device, 200).join("\n");
        assert!(log.contains(expect), "missing {expect}: {log}");
    }
    let log = crate::server::log::get_device_log(&device, 200).join("\n");
    assert!(log.contains("STRATEGY_FAILURE: patch_recovery"), "{log}");
    assert!(log.contains("DISC_READ_ERROR"), "{log}");
    halt.store(true, Ordering::Relaxed);
    log_patch_failure(
        &device,
        &halt,
        9,
        &libfreemkv::Error::ScsiError {
            opcode: 0x28,
            status: 2,
            sense: sense(3, 0x11),
        },
    );
    let log = crate::server::log::get_device_log(&device, 200).join("\n");
    assert!(log.contains("PASS 9 CANCELLED: user halt"), "{log}");
    assert!(log.contains("sense_key=03 ASC=11"), "{log}");
    crate::server::log::forget_device(&device);
}

fn session_on(transport: libfreemkv::test_util::FakeTransport, path: &str) -> DriveSession {
    DriveSession {
        drive: libfreemkv::Drive::from_transport(Box::new(transport)),
        disc: None,
        scanned: true,
        probed: true,
        tmdb: None,
        device_path: path.into(),
        key_verdict: None,
        keys: None,
        key_error: None,
        key_reads: Default::default(),
    }
}

#[test]
fn a_transport_fault_whose_drive_cannot_be_found_again_gives_up() {
    use freemkv_engine::PassHost;
    for stopped in [false, true] {
        let device = format!("transport-recover-{stopped}-{}", std::process::id());
        let path = "/nonexistent/recover-drive";
        let (t, _fake) = libfreemkv::test_util::FakeTransport::new();
        let mut session = session_on(t, path);
        let mut host = ServerPassHost {
            device: &device,
            device_path: path,
            session: &mut session,
            halt: Arc::new(AtomicBool::new(stopped)),
            user_halt: Arc::new(AtomicBool::new(false)),
            delay_secs: 0,
            attempt: 0,
            gave_up: false,
            halted: false,
        };
        let fault = libfreemkv::Error::ScsiError {
            opcode: 0x28,
            status: libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE,
            sense: None,
        };
        assert!(!host.recover_transport(3, &fault));
        assert_eq!(host.attempt, 3);
        assert_eq!((host.halted, host.gave_up), (stopped, !stopped));
        let log = crate::server::log::get_device_log(&device, 100).join("\n");
        let expect = if stopped {
            "cancelled (halt)"
        } else {
            "could not re-discover drive"
        };
        assert!(log.contains(expect), "{log}");
        crate::server::log::forget_device(&device);
    }
}

#[test]
fn a_spin_cycle_that_fails_before_a_patch_pass_settles_instead() {
    use freemkv_engine::PassHost;
    let device = format!("spin-cycle-{}", std::process::id());
    let path = "/nonexistent/spin-drive";
    let (t, _fake) = libfreemkv::test_util::FakeTransport::new();
    let t = t.rule(|cdb| cdb[0] == 0x1B, libfreemkv::test_util::FakeMode::Fault);
    let mut session = session_on(t, path);
    let mut host = ServerPassHost {
        device: &device,
        device_path: path,
        session: &mut session,
        halt: Arc::new(AtomicBool::new(false)),
        user_halt: Arc::new(AtomicBool::new(true)),
        delay_secs: 0,
        attempt: 0,
        gave_up: false,
        halted: false,
    };
    let started = std::time::Instant::now();
    host.before_patch(2);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let log = crate::server::log::get_device_log(&device, 100).join("\n");
    assert!(log.contains("spin-cycle before pass 2 failed"), "{log}");
    crate::server::log::forget_device(&device);
}
