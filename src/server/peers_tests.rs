use super::*;
#[test]
fn origins_and_proxy_are_scoped() {
    assert_eq!(
        origin("http://example.test:8080/drives/").unwrap(),
        "http://example.test:8080"
    );
    for bad in [
        "file:///tmp/a",
        "http://a@host",
        "http://host/api",
        "http://host?x=1",
        "http://host/#x",
    ] {
        assert!(origin(bad).is_err(), "{bad}");
    }
    assert!(allowed(&Method::Get, "/api/state"));
    assert!(allowed(&Method::Post, "/api/rip/sg3?resume=yes"));
    for bad in [
        "/api/settings",
        "/api/peers",
        "/api/rip/../settings",
        "/api/rip/%2f",
        "/api/logs/download",
    ] {
        assert!(!allowed(&Method::Post, bad));
    }
    assert!(!allowed(&Method::Get, "/api/stop/sg3"));
    for path in [
        "/events",
        "/api/system",
        "/api/review",
        "/api/tmdb/search?q=Dune",
        "/api/debug?device=sg0",
    ] {
        assert!(allowed(&Method::Get, path), "{path}");
        assert!(!allowed(&Method::Post, path), "{path}");
    }
    for path in [
        "/api/review/resolve",
        "/api/mux-errors/clear?path=x",
        "/api/mux-errors/clear-all",
        "/api/move-errors/clear?path=x",
        "/api/move-errors/clear-all",
    ] {
        assert!(allowed(&Method::Post, path), "{path}");
        assert!(!allowed(&Method::Get, path), "{path}");
    }
    for path in [
        "/api/peers/p123/api/system",
        "/api/settings",
        "/api/system/keyserver-test",
        "/api/review/../settings",
    ] {
        assert!(!allowed(&Method::Get, path), "{path}");
        assert!(!allowed(&Method::Post, path), "{path}");
    }
}
#[test]
fn events_relay_flushes_before_upstream_finishes() {
    use std::io::{BufRead, Write};
    use std::time::Duration;
    let upstream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", upstream.local_addr().unwrap());
    let (sent, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = upstream.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = std::io::BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        request.read_line(&mut line).unwrap();
        assert!(line.starts_with("GET /events "));
        loop {
            line.clear();
            request.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        stream
            .write_all(b"data: {\"sg0\":{\"status\":\"ripping\"}}\n\n")
            .unwrap();
        stream.flush().unwrap();
        // The client must see the first frame without waiting for EOF.
        received.recv_timeout(Duration::from_secs(5)).unwrap();
        stream.write_all(b"event: library\ndata: {}\n\n").unwrap();
        stream.flush().unwrap();
    });
    let proxy = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}/events", proxy.server_addr());
    let relay = std::thread::spawn(move || relay_events(proxy.recv().unwrap(), &origin));
    let mut response = agent().get(url).call().unwrap();
    assert_eq!(response.status(), 200);
    let mut reader = std::io::BufReader::new(response.body_mut().as_reader());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("ripping"));
    sent.send(()).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "event: library\n");
    worker.join().unwrap();
    relay.join().unwrap();
}

// Two independent HTTP servers, without poll/rip workers or physical-drive access.
struct Fixture {
    url: String,
    cfg: Arc<RwLock<Config>>,
    _dir: tempfile::TempDir,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Arc::new(RwLock::new(Config {
            autorip_dir: dir.path().to_str().unwrap().into(),
            ..Config::default()
        }));
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}", server.server_addr());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let config = cfg.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(request) = server
                    .recv_timeout(std::time::Duration::from_millis(30))
                    .unwrap()
                {
                    let cfg = config.clone();
                    std::thread::spawn(move || {
                        if request.url() == "/api/version" {
                            reply(request, 200, serde_json::json!({"version":"test"}));
                        } else if request.url() == "/api/state" {
                            reply(
                                request,
                                200,
                                serde_json::json!({"sg0":{"status":"ripping"}, "ioreg:123":{"status":"idle"}}),
                            );
                        } else if request.url().starts_with("/api/logs/") {
                            reply(request, 200, serde_json::json!({"lines":["remote log"]}));
                        } else if request.url().starts_with("/api/stop/") {
                            reply(request, 409, serde_json::json!({"error":"remote busy"}));
                        } else {
                            handle(request, &cfg);
                        }
                    });
                }
            }
        });
        Self {
            url,
            cfg,
            _dir: dir,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}
#[test]
fn two_libraries_pair_forward_disconnect_and_reject_self() {
    let a = Fixture::new();
    let b = Fixture::new();
    let body = serde_json::json!({"url": b.url, "name":"Other", "return_url": a.url}).to_string();
    assert!(connect(&a.cfg, &body).unwrap().is_none());
    let peers = read(&a.cfg).unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(read(&b.cfg).unwrap()[0].url, a.url);
    assert!(connect(&a.cfg, &body).unwrap().is_none());
    assert_eq!(read(&a.cfg).unwrap().len(), 1);
    let base = format!("{}/api/peers/{}", a.url, peers[0].id);
    let snapshot: serde_json::Value = agent()
        .get(format!("{base}/api/state"))
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert_eq!(snapshot["sg0"]["status"], "ripping");
    assert!(snapshot.get("ioreg:123").is_some());
    assert_eq!(
        agent()
            .post(format!("{base}/api/stop/sg0"))
            .send_empty()
            .unwrap()
            .status()
            .as_u16(),
        409
    );
    assert_eq!(
        agent()
            .get(format!("{base}/api/settings"))
            .call()
            .unwrap()
            .status()
            .as_u16(),
        403
    );
    assert_eq!(
        agent()
            .get(format!("{base}/api/logs/ioreg%3A123?since=0"))
            .call()
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    assert!(
        connect(&a.cfg, &serde_json::json!({"url": a.url}).to_string())
            .unwrap_err()
            .contains("this Library")
    );
    drop(b);
    assert_eq!(
        agent()
            .get(format!("{base}/api/state"))
            .call()
            .unwrap()
            .status()
            .as_u16(),
        502
    );
    connect(
        &a.cfg,
        &serde_json::json!({"remove": peers[0].id}).to_string(),
    )
    .unwrap();
    assert!(read(&a.cfg).unwrap().is_empty());
}

#[test]
fn slow_remote_stop_preserves_the_actual_response() {
    use std::time::Duration;
    let upstream = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", upstream.server_addr());
    let worker = std::thread::spawn(move || {
        let request = upstream
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        assert_eq!(request.method(), &Method::Post);
        assert_eq!(request.url(), "/api/stop/sg0");
        // Exceed the normal read deadline, as a draining drive can do.
        std::thread::sleep(Duration::from_secs(6));
        reply(
            request,
            409,
            serde_json::json!({"ok":false,"error":"worker still draining"}),
        );
    });
    let proxy = Fixture::new();
    save(
        &proxy.cfg,
        &[Peer {
            id: "pslow".into(),
            name: "Slow".into(),
            url: origin,
        }],
    )
    .unwrap();
    let mut response = agent()
        .post(format!("{}/api/peers/pslow/api/stop/sg0", proxy.url))
        .config()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .send_empty()
        .unwrap();
    assert_eq!(response.status().as_u16(), 409);
    let body: serde_json::Value = response.body_mut().read_json().unwrap();
    assert_eq!(body["error"], "worker still draining");
    worker.join().unwrap();
}
