// Route and helper tests driven through real loopback requests (tiny_http::Request has no
// public constructor), plus the pure helpers behind them.
use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

// One request through handle_request: returns (status, response head, body bytes).
fn exchange(
    cfg: &Arc<RwLock<Config>>,
    method: &str,
    path: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> (u16, String, Vec<u8>) {
    let server = Server::http("127.0.0.1:0").expect("bind loopback server");
    let addr = server.server_addr().to_ip().expect("ip addr");
    let mut raw = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
    for (k, v) in headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    let body = body.unwrap_or_default();
    raw.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    let client = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream.write_all(raw.as_bytes()).expect("write request");
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp);
        resp
    });
    let request = server
        .recv_timeout(Duration::from_secs(5))
        .expect("server error")
        .expect("the request must arrive within 5s");
    handle_request(request, cfg);
    let resp = client.join().expect("client thread");
    let split = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or(resp.len());
    let head = String::from_utf8_lossy(&resp[..split]).to_string();
    let body = resp.get(split + 4..).unwrap_or_default().to_vec();
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or(0);
    (code, head, body)
}

fn roundtrip(
    cfg: &Arc<RwLock<Config>>,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> (u16, String) {
    let (code, _, body) = exchange(cfg, method, path, body, &[]);
    (code, String::from_utf8_lossy(&body).to_string())
}

fn cfg_in(dir: &std::path::Path) -> Arc<RwLock<Config>> {
    Arc::new(RwLock::new(Config {
        autorip_dir: dir.to_string_lossy().to_string(),
        staging_dir: dir.join("staging").to_string_lossy().to_string(),
        output_dir: dir.join("output").to_string_lossy().to_string(),
        ..Config::default()
    }))
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

// Read one HTTP request (head plus Content-Length body) so the client never sees a reset.
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
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        read_http_request(&mut sock);
        let _ = sock.write_all(&response);
    });
    (port, handle)
}

fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn remove_row(device: &str) {
    ripper::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

#[test]
fn pages_and_assets_are_served_with_revalidation() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    for page in ["/", "/library", "/settings"] {
        let (code, head, body) = exchange(&cfg, "GET", page, None, &[]);
        assert_eq!(code, 200, "{page}");
        assert!(head.to_ascii_lowercase().contains("no-store"), "{head}");
        assert!(String::from_utf8_lossy(&body).contains("/assets/app.js"));
    }
    let (code, head, body) = exchange(&cfg, "GET", "/assets/app.js", None, &[]);
    assert_eq!(code, 200);
    assert!(head.to_ascii_lowercase().contains("etag:"), "{head}");
    assert!(!body.is_empty());
    let etag = format!(
        "\"{}\"",
        crate::server::VERSION_LABEL.replace(['"', ' '], "")
    );
    let (code, _, _) = exchange(
        &cfg,
        "GET",
        "/assets/app.js",
        None,
        &[("If-None-Match", etag.as_str())],
    );
    assert_eq!(code, 304, "a matching ETag revalidates");
    let (code, _) = roundtrip(&cfg, "GET", "/assets/no-such-module.js", None);
    assert_eq!(code, 404);
    let (code, body) = roundtrip(&cfg, "GET", "/favicon.svg", None);
    assert_eq!(code, 200);
    assert!(body.contains("<svg"), "{body}");
}

#[test]
fn state_schema_and_review_routes_answer_json() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let (code, body) = roundtrip(&cfg, "GET", "/api/state", None);
    assert_eq!(code, 200);
    let v = json(&body);
    assert!(
        v["_mux_queue"].is_array() && v["_move_queue"].is_array(),
        "{v}"
    );
    let (code, body) = roundtrip(&cfg, "GET", "/api/settings/schema", None);
    assert_eq!(code, 200);
    json(&body);
    let (code, body) = roundtrip(&cfg, "GET", "/api/review", None);
    assert_eq!(code, 200);
    assert_eq!(json(&body), serde_json::json!([]));
}

#[test]
fn error_clear_routes_need_a_path() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    for kind in ["move-errors", "mux-errors"] {
        let (code, body) = roundtrip(&cfg, "POST", &format!("/api/{kind}/clear?path="), None);
        assert_eq!(code, 400, "{kind}: {body}");
        assert!(body.contains("missing path"), "{body}");
        let url = format!("/api/{kind}/clear?other=1&path=%2Fcov%2Fno-such-error-{kind}");
        let (code, body) = roundtrip(&cfg, "POST", &url, None);
        assert_eq!(code, 200, "{kind}: {body}");
    }
    let (_, body) = roundtrip(
        &cfg,
        "POST",
        "/api/move-errors/clear?path=%2Fcov%2Fno-such-move-error",
        None,
    );
    assert_eq!(
        json(&body)["scheduled"],
        false,
        "nothing was there to retry"
    );
}

#[test]
fn per_device_routes_reject_invalid_and_unknown_devices() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    for (method, url) in [
        ("GET", "/api/logs/a%2Fb"),
        ("POST", "/api/scan/a%2Fb"),
        ("POST", "/api/accept-loss/a%2Fb"),
        ("POST", "/api/eject/a%2Fb"),
        ("POST", "/api/title/a%2Fb"),
    ] {
        let (code, body) = roundtrip(&cfg, method, url, None);
        assert_eq!(code, 400, "{url}: {body}");
        assert!(body.contains("invalid device name"), "{url}: {body}");
    }
    let (code, _) = roundtrip(&cfg, "POST", "/api/rip/zzcovunknownrip?resume=yes", None);
    assert_eq!(code, 404);
    assert!(!ripper::device_known("zzcovunknownrip"));
    let (code, body) = roundtrip(
        &cfg,
        "POST",
        "/api/title/zzcovunknowntitle",
        Some(r#"{"title":"X"}"#),
    );
    assert_eq!(code, 404, "{body}");
    assert!(body.contains("unknown or not yet initialized device"));
}

#[test]
fn title_override_validates_its_body_before_saving() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    let device = "sgcovtitle7";
    ripper::update_state(
        device,
        ripper::RipState {
            device: device.to_string(),
            status: "idle".to_string(),
            ..Default::default()
        },
    );
    let url = format!("/api/title/{device}");
    let cases = [
        ("not json", "invalid json"),
        (r#"{"title":"   "}"#, "title required"),
        (
            r#"{"title":"Dune","poster_url":"javascript:alert(1)"}"#,
            "invalid poster_url",
        ),
    ];
    for (body, error) in cases {
        let (code, resp) = roundtrip(&cfg, "POST", &url, Some(body));
        assert_eq!(code, 400, "{body}: {resp}");
        assert!(resp.contains(error), "{body}: {resp}");
    }
    remove_row(device);
}

#[test]
fn accept_loss_gates_on_the_staging_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let (code, body) = roundtrip(&cfg, "POST", "/api/accept-loss/sgcovnojob3", None);
    assert_eq!(code, 404, "{body}");
    assert!(body.contains("no disc state"), "{body}");

    let device = "sgcovaccept3";
    let job = "covacceptjob3";
    ripper::update_state(
        device,
        ripper::RipState {
            device: device.to_string(),
            status: "error".to_string(),
            job_id: job.to_string(),
            ..Default::default()
        },
    );
    let url = format!("/api/accept-loss/{device}");
    let (code, body) = roundtrip(&cfg, "POST", &url, None);
    assert_eq!(code, 404, "{body}");
    assert!(body.contains("no staging dir"), "{body}");

    let dir = tmp.path().join("staging").join(job);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("state.json"), b"{ not json").unwrap();
    let (code, body) = roundtrip(&cfg, "POST", &url, None);
    assert_eq!(code, 503, "an unreadable state fails closed: {body}");

    std::fs::remove_file(dir.join("state.json")).unwrap();
    std::fs::write(dir.join(ripper::staging::MUXING_MARKER), b"").unwrap();
    let (code, body) = roundtrip(&cfg, "POST", &url, None);
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("mux in progress"), "{body}");
    assert!(
        !dir.join(".accept-loss").exists(),
        "a refused accept arms nothing"
    );
    remove_row(device);
}

#[test]
fn review_resolve_rejects_bad_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let held = staging.join("heldnotitle");
    std::fs::create_dir_all(&held).unwrap();
    ripper::staging::write_state(
        &held,
        &ripper::staging::DiscState::new(ripper::staging::StagingState::Review),
    );
    let identity = r#"{"playlist":"00800.mpls","playlist_id":800,"extents":[[0,10]]}"#;
    let episodes_bad_dir = format!(r#"{{"action":"episodes","dir":"../x","titles":[{identity}]}}"#);
    let cases: Vec<(String, &str)> = vec![
        ("not json".into(), "invalid json"),
        (r#"{"action":"bogus","dir":"x"}"#.into(), "bad action"),
        (r#"{"action":"proceed","dir":"../x"}"#.into(), "invalid dir"),
        (
            r#"{"action":"proceed","dir":"heldnotitle"}"#.into(),
            "title required",
        ),
        (
            r#"{"action":"retitle","dir":"nostate","title":"  "}"#.into(),
            "title required",
        ),
        (
            r#"{"action":"retitle","dir":"nostate","title":"T","year":2001}"#.into(),
            "not a held rip",
        ),
        (
            r#"{"action":"cancel","dir":"nostate"}"#.into(),
            "not a held rip",
        ),
        (
            r#"{"action":"episodes","dir":"x","titles":"nope"}"#.into(),
            "\"ok\":false",
        ),
        (episodes_bad_dir, "invalid dir"),
    ];
    for (body, error) in cases {
        let (code, resp) = roundtrip(&cfg, "POST", "/api/review/resolve", Some(body.as_str()));
        assert_eq!(code, 400, "{body}: {resp}");
        assert!(resp.contains(error), "{body}: {resp}");
    }
}

#[test]
fn tmdb_search_rejects_an_empty_query() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    let (code, body) = roundtrip(&cfg, "GET", "/api/tmdb/search?q=%20%20", None);
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("invalid query"));
}

#[test]
fn a_poisoned_config_lock_answers_500_not_a_hang() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let poisoner = Arc::clone(&cfg);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.write().unwrap();
        panic!("poison the config lock");
    })
    .join();
    assert!(cfg.is_poisoned());
    for (method, url, body) in [
        ("GET", "/api/settings", None),
        ("POST", "/api/settings", Some("{}")),
        ("GET", "/api/system", None),
    ] {
        let (code, resp) = roundtrip(&cfg, method, url, body);
        assert_eq!(code, 500, "{method} {url}: {resp}");
        assert!(resp.contains("config lock poisoned"), "{resp}");
    }
    // Handlers that recover the guard still answer.
    let (code, body) = roundtrip(&cfg, "POST", "/api/accept-loss/sgcovpoison4", None);
    assert_eq!(code, 404, "{body}");
}

#[test]
fn system_info_reports_the_keydb_and_the_system_log() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let keydb = tmp.path().join("keydb.cfg");
    std::fs::write(&keydb, b"0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF = Test\n").unwrap();
    cfg.write().unwrap().keydb_path = Some(keydb.to_string_lossy().to_string());
    let logs = tmp.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let syslog = logs.join("device_system.log");
    std::fs::write(&syslog, "one\ntwo\n").unwrap();

    let (code, body) = roundtrip(&cfg, "GET", "/api/system", None);
    assert_eq!(code, 200, "{body}");
    let v = json(&body);
    assert_eq!(v["syslog"], "one\ntwo");
    assert_eq!(v["keys"]["keydb_present"], true);
    assert!(v["keys"]["keydb_modified"].is_u64(), "{v}");

    // A log that cannot be read says so rather than looking empty.
    std::fs::remove_file(&syslog).unwrap();
    std::fs::create_dir(&syslog).unwrap();
    let (code, body) = roundtrip(&cfg, "GET", "/api/system", None);
    assert_eq!(code, 200, "{body}");
    let v = json(&body);
    assert!(
        v["syslog"]
            .as_str()
            .is_some_and(|s| s.starts_with("(system log unreadable")),
        "{v}"
    );
}

#[test]
fn keyserver_test_answers_unreachable_for_a_dead_server() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    cfg.write().unwrap().keyserver_url = format!("http://127.0.0.1:{}/k", closed_port());
    let (code, body) = roundtrip(&cfg, "POST", "/api/system/keyserver-test", None);
    assert_eq!(code, 200, "{body}");
    assert_eq!(json(&body)["reachable"], false, "{body}");
}

#[test]
fn logs_download_zips_the_log_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let logs = tmp.path().join("logs");
    std::fs::create_dir_all(logs.join("sub")).unwrap();
    std::fs::write(logs.join("a.log"), "alpha\n").unwrap();
    std::fs::write(logs.join("sub").join("b.log"), "beta\n").unwrap();
    let (code, head, body) = exchange(&cfg, "GET", "/api/logs/download", None, &[]);
    assert_eq!(code, 200, "{head}");
    assert!(head.contains("application/zip"), "{head}");
    assert!(head.contains("freemkv-logs-"), "{head}");
    assert!(body.starts_with(b"PK"), "a zip archive");
}

#[test]
fn debug_log_route_answers() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    let (code, body) = roundtrip(&cfg, "GET", "/api/debug?n=5&level=warn", None);
    assert_eq!(code, 200, "{body}");
}

#[test]
fn debug_log_filters_by_level_device_text_and_count() {
    let content = [
        r#"{"level":"INFO","device":"sg4","msg":"alpha"}"#,
        r#"{"level":"WARN","device":"sg40","msg":"beta"}"#,
        r#"{"level":"ERROR","device":"sg4","msg":"gamma needle"}"#,
        r#"{"level":"DEBUG","msg":"delta"}"#,
        r#"{"level":"TRACE","msg":"epsilon"}"#,
    ]
    .join("\n");
    let count = |url: &str| {
        let out = filter_debug_log(url, &content);
        if out.is_empty() {
            0
        } else {
            out.lines().count()
        }
    };
    assert_eq!(count("/api/debug"), 5);
    assert_eq!(count("/api/debug?level=error"), 1);
    assert_eq!(count("/api/debug?level=warn"), 2);
    assert_eq!(count("/api/debug?level=INFO"), 3);
    assert_eq!(count("/api/debug?level=debug"), 4);
    assert_eq!(count("/api/debug?level=trace"), 5);
    assert_eq!(count("/api/debug?device=sg4"), 2, "sg40 is not sg4");
    assert_eq!(
        count("/api/debug?device=..%2Fx"),
        5,
        "an invalid device is ignored"
    );
    assert_eq!(count("/api/debug?q=needle"), 1);
    assert_eq!(
        count("/api/debug?q=%07"),
        5,
        "a control-byte filter is ignored"
    );
    assert_eq!(
        filter_debug_log("/api/debug?n=1", &content),
        r#"{"level":"TRACE","msg":"epsilon"}"#
    );
}

#[test]
fn webhook_test_route_reports_each_outcome() {
    let cfg = Arc::new(RwLock::new(Config::default()));
    let route = "/api/webhook/test";
    let (code, body) = roundtrip(&cfg, "POST", route, Some("not json"));
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("Invalid webhook URL"), "{body}");
    let (code, body) = roundtrip(&cfg, "POST", route, Some(r#"{"url":""}"#));
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("Enter a webhook URL"), "{body}");
    let masked = r#"{"url":"http://127.0.0.1:1/x","headers":{"X-Key":"********"}}"#;
    let (code, body) = roundtrip(&cfg, "POST", route, Some(masked));
    assert_eq!(code, 400, "no saved key to resolve: {body}");
    assert!(body.contains("Re-enter"), "{body}");

    let (port, server) = upstream(
        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    );
    let hook = format!(r#"{{"url":"http://127.0.0.1:{port}/hook"}}"#);
    let (code, body) = roundtrip(&cfg, "POST", route, Some(hook.as_str()));
    server.join().unwrap();
    assert_eq!(code, 200, "{body}");
    assert_eq!(json(&body)["status"], 204, "{body}");

    let (port, server) = upstream(
        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
    );
    let hook = format!(r#"{{"url":"http://127.0.0.1:{port}/hook"}}"#);
    let (code, body) = roundtrip(&cfg, "POST", route, Some(hook.as_str()));
    server.join().unwrap();
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("HTTP 500"), "{body}");
}

#[test]
fn url_and_target_validation_reject_malformed_authorities() {
    for (url, error) in [
        ("http://[::1", "malformed IPv6 host"),
        ("http://[::1]:port/", "invalid port"),
        ("http://:80/", "URL has no host"),
    ] {
        assert_eq!(validate_fetch_url(url).unwrap_err(), error, "{url}");
    }
    for (target, error) in [
        ("[::1]", "network target needs a port (host:port)"),
        ("[::1]:port", "invalid port"),
        ("[::1", "malformed IPv6 host"),
        (":9000", "network target has no host"),
    ] {
        assert_eq!(
            validate_network_target(target).unwrap_err(),
            error,
            "{target}"
        );
    }
}

#[test]
fn device_path_and_clamp_chars() {
    assert_eq!(device_path("ioreg:42"), "ioreg:42");
    #[cfg(unix)]
    assert_eq!(device_path("sg0"), "/dev/sg0");
    assert_eq!(clamp_chars("héllo", 2), "hé");
    assert_eq!(clamp_chars("hi", 5), "hi");
}

#[test]
fn state_json_carries_a_cards_episode_list() {
    let tmp = tempfile::tempdir().unwrap();
    let device = "sgcovcards6";
    ripper::update_state(
        device,
        ripper::RipState {
            device: device.to_string(),
            status: "idle".to_string(),
            job_id: "covcardsjob6".to_string(),
            ..Default::default()
        },
    );
    let out = get_state_json_with_cards(&tmp.path().to_string_lossy(), |job: &str| {
        (job == "covcardsjob6").then(Vec::new)
    });
    remove_row(device);
    let v = json(&out);
    assert_eq!(v[device]["episodes"], serde_json::json!([]), "{v}");
}

#[test]
fn roll_back_restores_only_the_failed_save() {
    let previous = Config::default();
    let cfg = Arc::new(RwLock::new(Config {
        movie_dir: "Applied".into(),
        ..previous.clone()
    }));
    let applied = serde_json::to_value(&*cfg.read().unwrap()).ok();
    roll_back_settings(&cfg, applied.as_ref(), &previous);
    assert_eq!(
        cfg.read().unwrap().movie_dir,
        previous.movie_dir,
        "rolled back"
    );

    // A newer save moved the config on: leave it.
    cfg.write().unwrap().movie_dir = "Newer".into();
    roll_back_settings(&cfg, applied.as_ref(), &previous);
    assert_eq!(cfg.read().unwrap().movie_dir, "Newer");
}

struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("client gone"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn sse_session_ends_when_the_first_write_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = cfg_in(tmp.path());
    let stop = AtomicBool::new(false);
    let started = std::time::Instant::now();
    sse_session(FailingWriter, &cfg, &stop, std::time::Instant::now());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "no tick was waited"
    );
}
