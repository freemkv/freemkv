use super::*;
use std::io::Write as _;

#[test]
fn open_sse_handler_drains_promptly_on_shutdown() {
    // Isolate global cancellation from the rest of the parallel suite. This is
    // a single loopback request fixture, not a running daemon or media worker.
    const CHILD: &str = "FREEMKV_TEST_SSE_DRAIN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "server::web::lifecycle_tests::open_sse_handler_drains_promptly_on_shutdown",
                    "--nocapture"
                ])
                .env(CHILD, "1")
                .status()
                .unwrap()
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
    let worker = crate::server::daemon::spawn_background("test-sse-handler", move || {
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

// A client that sends POST headers and then stalls must not pin its tracked
// handler (and so the restart drain) once the daemon starts draining. Bodies
// over 1 KiB reach the handler unread (tiny_http buffers only smaller ones).
#[test]
fn stalled_request_body_releases_handler_on_shutdown() {
    const CHILD: &str = "FREEMKV_TEST_STALLED_BODY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "server::web::lifecycle_tests::stalled_request_body_releases_handler_on_shutdown",
                    "--nocapture"
                ])
                .env(CHILD, "1")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    use std::time::Duration;
    let server = Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let (entered, reading) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let worker = crate::server::daemon::spawn_background("test-stalled-body", move || {
        let request = server
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        entered.send(()).unwrap();
        let read = read_json_body(request);
        done.send(read.is_err()).unwrap();
    })
    .unwrap();
    let mut socket = std::net::TcpStream::connect(addr).unwrap();
    socket
        .write_all(
            b"POST /api/settings HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5000\r\n\r\n{",
        )
        .unwrap();
    reading.recv_timeout(Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    crate::server::SHUTDOWN.store(true, Ordering::Release);
    let gave_up = finished
        .recv_timeout(Duration::from_secs(3))
        .expect("a stalled body read must observe shutdown");
    assert!(gave_up, "a partial body must never be acted on");
    worker.join().unwrap();
    drop(socket);
}

// A write stuck on a half-open client (no write timeout on the upgraded
// stream) must not keep the tracked SSE handler alive once draining starts.
#[test]
fn stuck_sse_write_releases_handler_on_shutdown() {
    const CHILD: &str = "FREEMKV_TEST_STUCK_SSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "server::web::lifecycle_tests::stuck_sse_write_releases_handler_on_shutdown",
                    "--nocapture"
                ])
                .env(CHILD, "1")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    use std::time::Duration;
    struct Stuck(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
    impl std::io::Write for Stuck {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            let _ = self.0.send(());
            let _ = self.1.recv(); // never released: a kernel send buffer that never drains
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RwLock::new(Config {
        autorip_dir: tmp.path().display().to_string(),
        staging_dir: tmp.path().join("staging").display().to_string(),
        ..Default::default()
    }));
    let (writing, blocked) = std::sync::mpsc::channel();
    let (_release, gate) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let worker = crate::server::daemon::spawn_background("test-stuck-sse", move || {
        let stop = AtomicBool::new(false);
        sse_session(Stuck(writing, gate), &cfg, &stop, std::time::Instant::now());
        done.send(()).unwrap();
    })
    .unwrap();
    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
    crate::server::SHUTDOWN.store(true, Ordering::Release);
    finished
        .recv_timeout(Duration::from_secs(3))
        .expect("SSE must observe shutdown while its socket write is stuck");
    worker.join().unwrap();
}

// tiny_http's accept thread exits after reporting an accept error, so the port
// is dead for good: like a failed bind, that must stop the daemon visibly
// (container restart policy) rather than leave workers running with no UI.
#[test]
fn dead_listener_signals_shutdown() {
    const CHILD: &str = "FREEMKV_TEST_DEAD_LISTENER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "server::web::lifecycle_tests::dead_listener_signals_shutdown",
                    "--nocapture"
                ])
                .env(CHILD, "1")
                .status()
                .unwrap()
                .success()
        );
        return;
    }
    let cfg = Arc::new(RwLock::new(Config::default()));
    serve(&cfg, |_| Err(std::io::Error::other("Too many open files")));
    assert!(crate::server::SHUTDOWN.load(Ordering::Acquire));
}
