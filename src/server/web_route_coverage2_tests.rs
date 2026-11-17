// More route and helper tests over real loopback requests. Tests that set the
// process-wide SHUTDOWN or hold the KEYDB in-flight flag run alone in a child process.
use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

const CHILD: &str = "FREEMKV_TEST_WEB_COV2_CHILD";

// Run `name` alone in a child process; true when this is that child.
fn in_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let status = crate::server::util::child_test(
        &format!("server::web::route_coverage2_tests::{name}"),
        CHILD,
    );
    assert!(status.success(), "{name} failed in its child process");
    false
}

fn raw_request(method: &str, path: &str, body: &[u8]) -> Vec<u8> {
    let mut raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    raw
}

fn send(addr: SocketAddr, raw: Vec<u8>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("read timeout");
        stream.write_all(&raw).expect("write request");
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp);
        resp
    })
}

fn status_of(resp: &[u8]) -> u16 {
    String::from_utf8_lossy(resp)
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

fn body_of(resp: &[u8]) -> String {
    let start = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(resp.len(), |p| p + 4);
    String::from_utf8_lossy(&resp[start..]).to_string()
}

// One request handed to `handler`: returns (status, body).
fn exchange_with(
    method: &str,
    path: &str,
    body: &[u8],
    handler: impl FnOnce(tiny_http::Request),
) -> (u16, String) {
    let server = Server::http("127.0.0.1:0").expect("bind loopback server");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let client = send(addr, raw_request(method, path, body));
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .expect("server error")
        .expect("the request must arrive within 5s");
    handler(request);
    let resp = client.join().expect("client thread");
    (status_of(&resp), body_of(&resp))
}

fn call(cfg: &Arc<RwLock<Config>>, method: &str, path: &str, body: &[u8]) -> (u16, String) {
    exchange_with(method, path, body, |r| handle_request(r, cfg))
}

fn cfg_in(dir: &std::path::Path) -> Arc<RwLock<Config>> {
    Arc::new(RwLock::new(Config {
        autorip_dir: dir.to_string_lossy().to_string(),
        staging_dir: dir.join("staging").to_string_lossy().to_string(),
        output_dir: dir.join("output").to_string_lossy().to_string(),
        ..Config::default()
    }))
}

fn row(device: &str, status: &str, job: &str) {
    ripper::update_state(
        device,
        ripper::RipState {
            device: device.to_string(),
            status: status.to_string(),
            job_id: job.to_string(),
            ..Default::default()
        },
    );
}

fn remove_row(device: &str) {
    ripper::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

fn status(device: &str) -> String {
    ripper::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .map(|r| r.status.clone())
        .unwrap_or_default()
}

// Read one HTTP request head plus its Content-Length body.
fn read_http_request(sock: &mut TcpStream) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match sock.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return,
        }
    }
    let text = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let len = text
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    let _ = sock.read_exact(&mut body);
}

// A one-shot loopback upstream answering with `response`.
fn upstream(response: Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        read_http_request(&mut sock);
        let _ = sock.write_all(&response);
    });
    (port, handle)
}

#[test]
fn serve_admits_caps_and_stops_then_refuses_new_work() {
    if !in_child("serve_admits_caps_and_stops_then_refuses_new_work") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let server = Server::http("127.0.0.1:0").expect("bind loopback server");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let get = || raw_request("GET", "/api/version", b"");
    let mut clients = Vec::new();
    let mut held = Vec::new();
    let mut first = None;
    let mut step = 0;
    serve(&cfg, |_| {
        step += 1;
        match step {
            1 => return Ok(None),
            2 => clients.push(send(addr, get())),
            3 => {
                first = clients.pop().map(|c| c.join().expect("client thread"));
                assert!(wait_for_zero(&INFLIGHT_HANDLERS, Duration::from_secs(10)));
                while let Some(g) =
                    ConnGuard::try_acquire(&INFLIGHT_HANDLERS, MAX_INFLIGHT_HANDLERS)
                {
                    held.push(g);
                }
                clients.push(send(addr, get()));
            }
            4 => clients.push(send(addr, raw_request("POST", "/api/debug", b"{}"))),
            5 => {
                held.clear();
                crate::server::SHUTDOWN.store(true, Ordering::SeqCst);
                clients.push(send(addr, get()));
            }
            _ => return Ok(None),
        }
        server.recv_timeout(Duration::from_secs(10))
    });
    drop(server);
    let first = first.expect("the first request was answered");
    assert_eq!(status_of(&first), 200, "{}", body_of(&first));
    let codes: Vec<u16> = clients
        .into_iter()
        .map(|c| status_of(&c.join().expect("client thread")))
        .collect();
    assert_eq!(codes.len(), 3, "{codes:?}");
    assert_eq!(codes[..2], [503, 503], "both caps refuse: {codes:?}");

    // A dead accept thread stops the loop and marks the web server failed.
    serve(&cfg, |_| Err(std::io::Error::other("accept thread gone")));
    assert!(crate::server::WEB_FAILED.load(Ordering::SeqCst));

    // run() binds, sees the shutdown and returns; a taken port fails the bind.
    let free = TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    cfg.write().unwrap().port = free;
    run(&cfg);
    let blocker = TcpListener::bind("0.0.0.0:0").unwrap();
    cfg.write().unwrap().port = blocker.local_addr().unwrap().port();
    run(&cfg);
    drop(blocker);

    let (code, body) = call(&cfg, "GET", "/events", b"");
    assert_eq!(code, 503, "{body}");
    let mut sink: Vec<u8> = Vec::new();
    sse_session(
        &mut sink,
        &cfg,
        &AtomicBool::new(false),
        std::time::Instant::now(),
    );
    assert!(sink.is_empty(), "nothing is streamed while stopping");

    // Every rip spawn is refused once a stop began; the claim is released.
    for (device, url) in [
        ("zzcov2kidscan", "/api/scan/zzcov2kidscan"),
        ("zzcov2kidrip", "/api/rip/zzcov2kidrip?resume=no"),
    ] {
        row(device, "idle", "");
        let (code, body) = call(&cfg, "POST", url, b"");
        assert_eq!(code, 500, "{url}: {body}");
        assert!(body.contains("thread spawn failed"), "{url}: {body}");
        assert!(!ripper::is_busy(device), "{url}");
    }
    let device = "zzcov2kidaccept";
    let job = "cov2kidacceptjob";
    let dir = tmp.path().join("staging").join(job);
    std::fs::create_dir_all(&dir).unwrap();
    row(device, "error", job);
    let (code, body) = call(&cfg, "POST", &format!("/api/accept-loss/{device}"), b"");
    assert_eq!(code, 500, "{body}");
    assert!(
        !dir.join(ripper::staging::ACCEPT_LOSS_MARKER).exists(),
        "a refused spawn disarms the override"
    );
}

#[test]
fn keydb_update_refuses_a_second_download_and_names_a_save_failure() {
    if !in_child("keydb_update_refuses_a_second_download_and_names_a_save_failure") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let slow = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        read_http_request(&mut sock);
        accepted_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(30));
        let _ = sock
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    });
    cfg.write().unwrap().keydb_url = format!("http://127.0.0.1:{port}/k");
    let first = {
        let cfg = Arc::clone(&cfg);
        std::thread::spawn(move || call(&cfg, "POST", "/api/update-keydb", b""))
    };
    accepted_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the first download reaches the upstream");
    let (code, body) = call(&cfg, "POST", "/api/update-keydb", b"");
    assert_eq!(code, 429, "{body}");
    release_tx.send(()).unwrap();
    slow.join().unwrap();
    let (code, body) = first.join().unwrap();
    assert_eq!(code, 502, "{body}");

    // A keydb path under a regular file cannot be written.
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"").unwrap();
    cfg.write().unwrap().keydb_path = Some(blocker.join("keydb.cfg").to_string_lossy().to_string());
    let valid = "0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF = Test\n";
    let (port, served) = upstream(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{valid}",
            valid.len()
        )
        .into_bytes(),
    );
    cfg.write().unwrap().keydb_url = format!("http://127.0.0.1:{port}/k");
    let (code, body) = call(&cfg, "POST", "/api/update-keydb", b"");
    served.join().unwrap();
    assert_eq!(code, 500, "{body}");
    assert!(body.contains("Failed to save KEYDB"), "{body}");
}

#[cfg(target_os = "linux")]
#[test]
fn scan_and_rip_routes_start_a_worker_that_reports_a_missing_drive() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    for (device, url) in [
        ("zzcov2scan", "/api/scan/zzcov2scan"),
        ("zzcov2rip", "/api/rip/zzcov2rip?resume=yes"),
    ] {
        row(device, "idle", "");
        let (code, body) = call(&cfg, "POST", url, b"");
        assert_eq!(code, 200, "{url}: {body}");
        assert!(ripper::join_rip_thread(device, Duration::from_secs(60)).is_ok());
        assert_eq!(status(device), "error", "{url}: no such drive");
        remove_row(device);
    }
}

#[test]
fn scan_and_rip_routes_refuse_a_busy_drive() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    for (device, url, error) in [
        ("zzcov2scanbusy", "/api/scan/zzcov2scanbusy", "busy"),
        ("zzcov2ripbusy", "/api/rip/zzcov2ripbusy", "already ripping"),
        (
            "zzcov2ejectbusy",
            "/api/eject/zzcov2ejectbusy",
            "drive busy",
        ),
    ] {
        row(device, "ripping", "");
        let (code, body) = call(&cfg, "POST", url, b"");
        assert_eq!(code, 409, "{url}: {body}");
        assert!(body.contains(error), "{url}: {body}");
        assert_eq!(status(device), "ripping", "{url}");
        remove_row(device);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn eject_of_a_missing_drive_answers_its_error_and_frees_the_row() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    let device = "zzcov2eject";
    row(device, "idle", "");
    let (code, body) = call(&cfg, "POST", &format!("/api/eject/{device}"), b"");
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("\"ok\":false"), "{body}");
    {
        let s = ripper::STATE.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(s[device].status, "idle");
        assert!(s[device].disc_present, "the disc did not leave");
    }
    remove_row(device);
}

#[cfg(target_os = "linux")]
#[test]
fn accept_loss_reopens_the_dir_and_hands_it_to_a_worker() {
    use crate::server::ripper::staging::{DiscState, StagingState};
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let device = "zzcov2accept";
    let job = "cov2acceptjob";
    let dir = tmp.path().join("staging").join(job);
    std::fs::create_dir_all(&dir).unwrap();
    ripper::staging::write_state(&dir, &DiscState::new(StagingState::AbortedLoss));
    std::fs::write(dir.join(ripper::staging::FAILED_MARKER), b"").unwrap();
    row(device, "error", job);
    let (code, body) = call(&cfg, "POST", &format!("/api/accept-loss/{device}"), b"");
    assert_eq!(code, 200, "{body}");
    assert!(!dir.join(ripper::staging::FAILED_MARKER).exists());
    assert!(ripper::join_rip_thread(device, Duration::from_secs(60)).is_ok());
    assert_eq!(status(device), "error", "no such drive to re-mux from");
    remove_row(device);
}

#[test]
fn a_stop_leaves_a_row_reclaimed_while_it_drained() {
    let device = "zzcov2reclaim";
    ripper::update_state(
        device,
        ripper::RipState {
            device: device.to_string(),
            status: "ripping".to_string(),
            claim_gen: 7,
            ..Default::default()
        },
    );
    assert!(apply_stop_to_state(device, Some(6), &stop_report(true)));
    assert_eq!(status(device), "ripping", "the new rip's row stands");
    remove_row(device);
}

#[test]
fn unreadable_bodies_are_refused_on_every_json_route() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let device = "zzcov2badbody";
    row(device, "idle", "");
    let bad: &[u8] = &[0xff, 0xfe, 0xfd];
    for url in [
        "/api/webhook/test".to_string(),
        "/api/review/resolve".to_string(),
        "/api/debug".to_string(),
        format!("/api/title/{device}"),
        format!("/api/titles/{device}"),
    ] {
        let (code, body) = call(&cfg, "POST", &url, bad);
        assert_eq!(code, 400, "{url}: {body}");
        assert!(body.contains("bad body"), "{url}: {body}");
    }
    remove_row(device);
}

#[test]
fn review_resolve_cancels_a_held_rip_and_refuses_a_missing_one() {
    use crate::server::ripper::staging::{DiscState, StagingState};
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let held = tmp.path().join("staging").join("heldcov2");
    std::fs::create_dir_all(&held).unwrap();
    ripper::staging::write_state(&held, &DiscState::new(StagingState::Review));
    let (code, body) = call(
        &cfg,
        "POST",
        "/api/review/resolve",
        br#"{"action":"cancel","dir":"heldcov2"}"#,
    );
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        ripper::staging::read_state(&held).map(|s| s.state),
        Some(StagingState::Failed)
    );
    let (code, body) = call(
        &cfg,
        "POST",
        "/api/review/resolve",
        br#"{"action":"proceed","dir":"nostatecov2"}"#,
    );
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("not a held rip"), "{body}");
}

#[test]
fn logs_download_skips_a_missing_dir_and_nested_folders() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let (code, _) = call(&cfg, "GET", "/api/logs/download", b"");
    assert_eq!(code, 200, "no log dir is still a bundle");
    let logs = tmp.path().join("logs");
    std::fs::create_dir_all(logs.join("sub").join("deeper")).unwrap();
    std::fs::write(logs.join("sub").join("b.log"), "beta\n").unwrap();
    let (code, _) = call(&cfg, "GET", "/api/logs/download", b"");
    assert_eq!(code, 200);
}

#[test]
fn log_bundle_notes_a_file_it_could_not_write() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("a.log");
    std::fs::write(&path, "hello").unwrap();
    let files = vec![
        ("a.log".to_string(), path.clone()),
        ("a.log".to_string(), path),
    ];
    let bytes = build_log_bundle(files).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut notes = String::new();
    zip.by_name(BUNDLE_NOTES)
        .unwrap()
        .read_to_string(&mut notes)
        .unwrap();
    assert!(notes.contains("a.log: not written"), "{notes}");
}

#[test]
fn device_log_handler_rechecks_the_device_name() {
    let (code, body) = exchange_with("GET", "/api/logs/x", b"", |r| handle_device_log(r, "a/b"));
    assert_eq!(code, 200);
    assert_eq!(body, "invalid device");
}

#[test]
fn a_fetch_url_whose_host_cannot_be_looked_up_is_refused() {
    let err = validate_fetch_url("http://a\0b:port/").unwrap_err();
    assert!(err.starts_with(RESOLVE_FAILED_PREFIX), "{err}");
}

#[cfg(unix)]
#[test]
fn socket_peer_reads_v6_peers_and_skips_other_sockets() {
    use std::os::fd::AsRawFd;
    let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
    assert_eq!(socket_peer(a.as_raw_fd()), None);
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    assert!(!set_write_timeout_for_peer(closed, Duration::from_secs(1)));
    // A runner without IPv6 loopback has nothing more to check.
    let Ok(listener) = TcpListener::bind("[::1]:0") else {
        return;
    };
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).unwrap();
    assert_eq!(socket_peer(client.as_raw_fd()), Some(addr));
}

// The process-wide Library opens in its first caller's autorip_dir, so that dir must outlive the test.
fn library_cfg() -> Arc<RwLock<Config>> {
    cfg_in(&tempfile::tempdir().unwrap().keep())
}

#[test]
fn library_routes_answer_before_the_first_scan() {
    let cfg = library_cfg();
    let root = std::path::PathBuf::from(cfg.read().unwrap().autorip_dir.clone());
    let source = root.join("isos").join("Missing.iso");
    let (code, body) = call(
        &cfg,
        "GET",
        &format!("/api/library/match?source={}", source.display()),
        b"",
    );
    assert_eq!(code, 404, "{body}");
    let change = serde_json::json!({"source": source, "expected_revision": 0,
        "media": {"title": "Missing", "year": 2000, "tmdb_id": 1, "season": null, "disc": null, "kind": "movie"}});
    let (code, body) = call(
        &cfg,
        "POST",
        "/api/library/match/remux",
        change.to_string().as_bytes(),
    );
    assert_eq!(code, 409, "an ISO that cannot be read is refused: {body}");
    assert!(body.contains("Nothing was queued"), "{body}");

    let (code, _) = call(&cfg, "GET", "/api/library/raw?path=/nope/a.mkv", b"");
    assert_eq!(code, 404);
    let (code, body) = call(&cfg, "GET", "/api/library/log?title=Missing&raw=1", b"");
    assert_eq!(code, 200, "{body}");
    let (code, _) = call(&cfg, "POST", "/api/library/rescan", b"");
    assert_eq!(code, 200);

    let cases = [
        (
            "/api/library/reaudit",
            r#"{"paths":["/nope/a.mkv"]}"#,
            200,
            "requeued",
        ),
        ("/api/library/reaudit", "{}", 400, "missing paths"),
        ("/api/library/reaudit", r#"{"all":true}"#, 200, "requeued"),
        (
            "/api/library/queue/add",
            r#"{"targets":["/nope/a.mkv","/nope/b.mkv"]}"#,
            409,
            "still being scanned",
        ),
        ("/api/library/queue/add", "{}", 400, "missing target"),
        (
            "/api/library/queue/remove",
            r#"{"targets":["/nope/a.mkv","/nope/b.mkv"]}"#,
            200,
            "\"removed\":0",
        ),
        ("/api/library/queue/remove", "{}", 400, "missing target"),
        ("/api/library/queue/clear", "", 200, "cleared"),
        ("/api/library/audit/pause", "", 200, "{"),
        ("/api/library/audit/resume", "", 200, "{"),
        ("/api/library/audit/stop-all", "", 200, "removed"),
        ("/api/library/debug", "{}", 400, "missing enabled"),
        (
            "/api/library/debug",
            r#"{"enabled":false}"#,
            200,
            "debug_log",
        ),
    ];
    for (url, body, want, text) in cases {
        let (code, resp) = call(&cfg, "POST", url, body.as_bytes());
        assert_eq!(code, want, "{url} {body}: {resp}");
        assert!(resp.contains(text), "{url} {body}: {resp}");
    }

    for url in [
        "/api/library/match/remux",
        "/api/library/reaudit",
        "/api/library/queue/add",
        "/api/library/queue/remove",
        "/api/library/staged/retry",
        "/api/library/debug",
    ] {
        let (code, body) = call(&cfg, "POST", url, &[0xff, 0xfe]);
        assert_eq!(code, 400, "{url}: {body}");
        assert!(body.contains("bad body"), "{url}: {body}");
    }
}
