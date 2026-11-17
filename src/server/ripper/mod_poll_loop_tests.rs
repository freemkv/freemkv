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
