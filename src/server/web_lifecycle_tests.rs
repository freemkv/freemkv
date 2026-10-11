use super::*;
use std::io::Write as _;

#[test]
fn open_sse_handler_drains_promptly_on_shutdown() {
    // Isolate global cancellation from the rest of the parallel suite. This is
    // a single loopback request fixture, not a running daemon or media worker.
    const CHILD: &str = "FREEMKV_TEST_SSE_DRAIN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            crate::server::util::child_test(
                "server::web::lifecycle_tests::open_sse_handler_drains_promptly_on_shutdown",
                CHILD
            )
            .success()
        );
        return;
    }
    use std::io::{BufRead, BufReader};
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RwLock::new(Config {
        autorip_dir: tmp.path().display().to_string(),
        staging_dir: tmp.path().join("staging").display().to_string(),
        ..Default::default()
    }));
    let server = Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("test-sse-handler".into())
        .spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            handle_sse(request, &cfg);
            done.send(()).unwrap();
        })
        .unwrap();
    let mut socket = std::net::TcpStream::connect(addr).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: sse\r\n\r\n").unwrap();
    let mut socket = BufReader::new(socket);
    loop {
        let mut line = String::new();
        assert_ne!(socket.read_line(&mut line).unwrap(), 0);
        if line.starts_with("data:") {
            break;
        }
    }
    // Receiving the first real frame is the barrier: the session is open and
    // would otherwise hold the tracked HTTP worker for its 600-second lifetime.
    crate::server::SHUTDOWN.store(true, Ordering::Release);
    finished
        .recv_timeout(Duration::from_secs(3))
        .expect("SSE must observe shutdown without client disconnect");
    worker.join().unwrap();
    assert!(SSE_STREAMS.lock().unwrap().is_empty());
}

// Shutdown waits for in-flight handlers, but only up to its bound.
#[test]
fn wait_for_zero_waits_for_held_handlers_within_its_bound() {
    use std::time::Duration;
    static HANDLERS: AtomicUsize = AtomicUsize::new(0);
    assert!(wait_for_zero(&HANDLERS, Duration::ZERO));
    let guard = ConnGuard::try_acquire(&HANDLERS, 1).unwrap();
    assert!(!wait_for_zero(&HANDLERS, Duration::from_millis(50)));
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        drop(guard);
    });
    assert!(wait_for_zero(&HANDLERS, Duration::from_secs(5)));
    release.join().unwrap();
}

// Only a launchd job label means a service manager will start the daemon again.
#[cfg(target_os = "macos")]
#[test]
fn only_a_launchd_job_label_counts_as_a_service_manager() {
    use std::ffi::OsStr;
    assert!(launchd_job(Some(OsStr::new("io.freemkv.library")), 1));
    for terminal in [
        None,
        Some(""),
        Some("0"),
        Some("application.com.apple.Terminal.1234"),
    ] {
        assert!(!launchd_job(terminal.map(OsStr::new), 1), "{terminal:?}");
    }
}

// A job label with a non-launchd parent (a script run under another job) is unsupervised,
// so the reboot route answers 409.
#[cfg(target_os = "macos")]
#[test]
fn a_job_label_without_a_launchd_parent_is_not_supervised() {
    use std::ffi::OsStr;
    assert!(!launchd_job(Some(OsStr::new("io.freemkv.library")), 4242));
}

// tiny_http's accept thread exits after reporting an accept error, so the port
// is dead for good: like a failed bind, that must stop the daemon visibly
// (container restart policy) rather than leave workers running with no UI.
#[test]
fn dead_listener_signals_shutdown() {
    const CHILD: &str = "FREEMKV_TEST_DEAD_LISTENER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            crate::server::util::child_test(
                "server::web::lifecycle_tests::dead_listener_signals_shutdown",
                CHILD
            )
            .success()
        );
        return;
    }
    let cfg = Arc::new(RwLock::new(Config::default()));
    serve(&cfg, |_| Err(std::io::Error::other("Too many open files")));
    assert!(crate::server::SHUTDOWN.load(Ordering::Acquire));
}
