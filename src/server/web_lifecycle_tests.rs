use super::*;

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
