// The drive poll loop's startup staging scan and its shutdown, run in a child process
// because the shutdown flag is process-wide.
use super::*;

const CHILD: &str = "FREEMKV_TEST_POLL_LOOP_CHILD";

#[test]
fn the_poll_loop_scans_staging_at_start_and_stops_on_shutdown() {
    if std::env::var_os(CHILD).is_none() {
        let status = crate::server::util::child_test(
            "server::ripper::poll_loop_tests::the_poll_loop_scans_staging_at_start_and_stops_on_shutdown",
            CHILD,
        );
        assert!(status.success(), "the poll loop child failed: {status}");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let staging_root = tmp.path().join("staging");
    for (name, state) in [
        ("failed-job", staging::StagingState::Failed),
        ("stopped-job", staging::StagingState::Stopped),
    ] {
        let dir = staging_root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        staging::try_write_state(&dir, &staging::DiscState::new(state)).unwrap();
    }
    let cfg = Arc::new(RwLock::new(Config {
        staging_dir: staging_root.to_string_lossy().into_owned(),
        ..Config::default()
    }));
    crate::server::SHUTDOWN.store(false, Ordering::SeqCst);
    let looping = {
        let cfg = cfg.clone();
        std::thread::spawn(move || drive_poll_loop(&cfg))
    };
    std::thread::sleep(Duration::from_millis(1500));
    crate::server::SHUTDOWN.store(true, Ordering::SeqCst);
    let started = std::time::Instant::now();
    looping.join().unwrap();
    assert!(started.elapsed() < Duration::from_secs(30));
    assert!(staging_root.join("failed-job").exists());
}

const TICKS_CHILD: &str = "FREEMKV_TEST_POLL_TICKS_CHILD";

fn drive(name: &str) -> libfreemkv::DriveInfo {
    libfreemkv::DriveInfo {
        path: format!("/nonexistent/{name}"),
        display_name: name.into(),
        vendor: "FAKE".into(),
        model: "POLL".into(),
        firmware: "1.0".into(),
    }
}

// Present, swapped, settling, empty, and a drive that never answers.
fn scripted_presence(path: &str) -> libfreemkv::Result<(libfreemkv::DiscPresence, bool)> {
    use libfreemkv::DiscPresence::{Absent, Present, Settling};
    match path.rsplit('/').next().unwrap_or("") {
        "pollin" => Ok((Present, false)),
        "pollswap" => Ok((Present, true)),
        "pollspin" => Ok((Settling, false)),
        "pollempty" => Ok((Absent, false)),
        _ => Err(libfreemkv::Error::ScsiError {
            opcode: 0x00,
            status: libfreemkv::scsi::SCSI_STATUS_TRANSPORT_FAILURE,
            sense: None,
        }),
    }
}

#[test]
fn poll_ticks_dispatch_inserts_idle_empty_drives_and_surface_a_wedge() {
    if std::env::var_os(TICKS_CHILD).is_none() {
        let status = crate::server::util::child_test(
            "server::ripper::poll_loop_tests::poll_ticks_dispatch_inserts_idle_empty_drives_and_surface_a_wedge",
            TICKS_CHILD,
        );
        assert!(status.success(), "the poll ticks child failed: {status}");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let names = ["pollin", "pollswap", "pollspin", "pollempty", "pollwedge"];
    *FAKE_POLL.lock().unwrap() = Some(FakePoll {
        drives: names.iter().map(|n| drive(n)).collect(),
        presence: scripted_presence,
    });
    let cfg = Arc::new(RwLock::new(Config {
        staging_dir: tmp.path().join("staging").to_string_lossy().into_owned(),
        on_insert: "rip".into(),
        ..Config::default()
    }));
    crate::server::SHUTDOWN.store(false, Ordering::SeqCst);
    let looping = {
        let cfg = cfg.clone();
        std::thread::spawn(move || drive_poll_loop(&cfg))
    };
    // Three failed probes, two poll intervals apart, make a wedge.
    std::thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS * 2 + 4));
    crate::server::SHUTDOWN.store(true, Ordering::SeqCst);
    looping.join().unwrap();
    let rows = STATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let wedge = rows.get("pollwedge").expect("the wedged drive's tile");
    assert_eq!(wedge.status, "error");
    assert!(
        wedge.last_error.contains("communication failed"),
        "{}",
        wedge.last_error
    );
    assert_eq!(
        rows.get("pollempty").map(|r| r.status.as_str()),
        Some("idle")
    );
    for inserted in ["pollin", "pollswap"] {
        let row = rows.get(inserted).expect("the inserted drive's tile");
        assert!(
            row.status != "idle" || !row.last_error.is_empty(),
            "{row:?}"
        );
    }
}
