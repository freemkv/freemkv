//! Explicit LAN Library connections. Only Drives-page APIs may cross a connection;
//! state remains local, preventing recursive federation and duplicated jobs.
use super::{config::Config, web};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, RwLock};
use tiny_http::{Method, Request};

static STORE: Mutex<()> = Mutex::new(());
#[derive(Clone, Serialize, Deserialize)]
struct Peer {
    id: String,
    name: String,
    url: String,
}
fn file(cfg: &Arc<RwLock<Config>>) -> std::path::PathBuf {
    std::path::Path::new(&cfg.read().unwrap_or_else(|e| e.into_inner()).autorip_dir)
        .join("libraries.json")
}
fn read(cfg: &Arc<RwLock<Config>>) -> Result<Vec<Peer>, String> {
    match std::fs::read(file(cfg)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.to_string()),
    }
}
fn origin(raw: &str) -> Result<String, String> {
    let raw = raw.trim().trim_end_matches('/').trim_end_matches("/drives");
    let uri: ureq::http::Uri = raw.parse().map_err(|_| "Invalid Library URL")?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.authority().is_none()
        || uri.authority().unwrap().as_str().contains('@')
        || !matches!(uri.path(), "" | "/")
        || uri.query().is_some()
        || raw.contains('#')
    {
        return Err("Use an http:// or https:// Library URL without credentials or a path".into());
    }
    Ok(format!(
        "{}://{}",
        uri.scheme_str().unwrap(),
        uri.authority().unwrap()
    )
    .to_lowercase())
}
fn agent() -> ureq::Agent {
    // LAN destinations are intentional, but redirects must not escape the saved origin.
    ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build(),
    )
}
fn reply(request: Request, status: u16, value: serde_json::Value) {
    web::json_response(request, status, &value.to_string());
}
fn allowed(method: &Method, target: &str) -> bool {
    let path = target.split('?').next().unwrap_or("");
    if *method == Method::Get
        && matches!(
            path,
            "/events"
                | "/api/state"
                | "/api/version"
                | "/api/system"
                | "/api/review"
                | "/api/tmdb/search"
                | "/api/debug"
        )
    {
        return true;
    }
    if *method == Method::Post
        && matches!(
            path,
            "/api/review/resolve"
                | "/api/mux-errors/clear"
                | "/api/mux-errors/clear-all"
                | "/api/move-errors/clear"
                | "/api/move-errors/clear-all"
        )
    {
        return true;
    }
    let Some(rest) = path.strip_prefix("/api/") else {
        return false;
    };
    let Some((action, device)) = rest.split_once('/') else {
        return false;
    };
    super::web::is_valid_device_name(&super::web::percent_decode(device))
        && device != "download"
        && ((*method == Method::Get && action == "logs")
            || (*method == Method::Post
                && matches!(
                    action,
                    "scan" | "rip" | "stop" | "eject" | "title" | "accept-loss"
                )))
}
fn new_id() -> String {
    format!(
        "p{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
fn identity(cfg: &Arc<RwLock<Config>>) -> Result<String, String> {
    let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let path = file(cfg).with_file_name("library-id");
    match std::fs::read_to_string(&path) {
        Ok(id) => Ok(id),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let id = new_id();
            std::fs::write(path, &id).map_err(|e| e.to_string())?;
            Ok(id)
        }
        Err(e) => Err(e.to_string()),
    }
}
fn save(cfg: &Arc<RwLock<Config>>, peers: &[Peer]) -> Result<(), String> {
    let path = file(cfg);
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(peers).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path).map_err(|e| e.to_string())
}
fn connect(cfg: &Arc<RwLock<Config>>, body: &str) -> Result<Option<String>, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| "Invalid JSON")?;
    if let Some(id) = v["remove"].as_str() {
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut peers = read(cfg)?;
        peers.retain(|p| p.id != id);
        save(cfg, &peers)?;
        return Ok(None);
    }
    let url = origin(v["url"].as_str().unwrap_or(""))?;
    let name = v["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&url)
        .trim();
    if name.len() > 100 {
        return Err("Library name must be at most 100 characters".into());
    }
    let return_url = v["return_url"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(origin)
        .transpose()?;
    let local_id = identity(cfg)?;
    // No lock across network I/O: simultaneous connects and self-detection must not deadlock.
    let agent = agent();
    let mut response = agent
        .get(format!("{url}/api/version"))
        .call()
        .map_err(|_| "Library did not answer")?;
    let version: serde_json::Value = response
        .body_mut()
        .with_config()
        .limit(4096)
        .read_json()
        .map_err(|_| "Not a Library server")?;
    if !response.status().is_success() || !version["version"].is_string() {
        return Err("Not a Library server".into());
    }
    if let Ok(mut response) = agent.get(format!("{url}/api/peers/identity")).call()
        && let Ok(remote) = response
            .body_mut()
            .with_config()
            .limit(4096)
            .read_json::<serde_json::Value>()
        && remote["id"].as_str() == Some(&local_id)
    {
        return Err("This URL points to this Library. Enter the other machine’s URL.".into());
    }
    {
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut peers = read(cfg)?;
        if let Some(peer) = peers.iter_mut().find(|p| p.url == url) {
            peer.name = name.into();
        } else {
            if peers.len() >= 8 {
                return Err("At most eight Libraries can be connected".into());
            }
            peers.push(Peer {
                id: new_id(),
                name: name.into(),
                url: url.clone(),
            });
        }
        save(cfg, &peers)?;
    }
    if let Some(return_url) = return_url {
        let result = agent
            .post(format!("{url}/api/peers"))
            .send_json(serde_json::json!({"url": return_url, "name": return_url}));
        let paired = result.ok().is_some_and(|mut r| {
            r.status().is_success()
                && r.body_mut()
                    .with_config()
                    .limit(4096)
                    .read_json::<serde_json::Value>()
                    .is_ok_and(|v| v["ok"] == true)
        });
        if !paired {
            return Ok(Some("Connected here, but the other Library could not connect back. Check this machine’s reachable URL and the remote Library version, then connect again.".into()));
        }
    }
    Ok(None)
}
pub fn handle(request: Request, cfg: &Arc<RwLock<Config>>) {
    let url = request.url().to_string();
    if url == "/api/peers" && *request.method() == Method::Get {
        let _guard = STORE.lock().unwrap_or_else(|e| e.into_inner());
        match read(cfg) {
            Ok(peers) => reply(request, 200, serde_json::json!(peers)),
            Err(e) => reply(request, 500, serde_json::json!({"error": e})),
        }
        return;
    }
    if url == "/api/peers/identity" && *request.method() == Method::Get {
        return match identity(cfg) {
            Ok(id) => reply(request, 200, serde_json::json!({"id": id})),
            Err(e) => reply(request, 500, serde_json::json!({"error": e})),
        };
    }
    if url == "/api/peers" && *request.method() == Method::Post {
        let Ok((request, body)) = web::read_json_body(request) else {
            return;
        };
        let result = connect(cfg, &body);
        match result {
            Ok(warning) => reply(
                request,
                200,
                serde_json::json!({"ok": true, "warning": warning}),
            ),
            Err(e) => reply(request, 400, serde_json::json!({"ok": false, "error": e})),
        }
        return;
    }
    let Some((id, rest)) = url
        .strip_prefix("/api/peers/")
        .and_then(|s| s.split_once('/'))
    else {
        return reply(
            request,
            404,
            serde_json::json!({"error": "Unknown connection"}),
        );
    };
    let target = format!("/{rest}");
    if !allowed(request.method(), &target) {
        return reply(
            request,
            403,
            serde_json::json!({"error": "Only Drives-page APIs can be forwarded"}),
        );
    }
    let peer = read(cfg)
        .ok()
        .and_then(|ps| ps.into_iter().find(|p| p.id == id));
    let Some(peer) = peer else {
        return reply(
            request,
            404,
            serde_json::json!({"error": "Unknown Library"}),
        );
    };
    if target == "/events" {
        return relay_events(request, &peer.url);
    }
    let is_post = *request.method() == Method::Post;
    let (request, body) = if is_post {
        let Ok(pair) = web::read_json_body(request) else {
            return;
        };
        pair
    } else {
        (request, String::new())
    };
    let agent = agent();
    let destination = format!("{}{target}", peer.url);
    let response = if is_post {
        agent
            .post(destination)
            .content_type("application/json")
            .send(body.as_bytes())
    } else {
        agent.get(destination).call()
    };
    match response {
        Ok(mut response) => {
            let status = response.status().as_u16();
            match response
                .body_mut()
                .with_config()
                .limit(2 * 1024 * 1024)
                .read_to_string()
            {
                Ok(body) => web::json_response(request, status, &body),
                Err(_) => reply(
                    request,
                    502,
                    serde_json::json!({"error": "Could not read remote Library response"}),
                ),
            }
        }
        Err(_) => reply(
            request,
            502,
            serde_json::json!({"error": "Remote Library is offline or timed out"}),
        ),
    }
}

// Streams bypass the JSON proxy's body buffering. Bound their number and lifetime
// so an abandoned browser or silent upstream cannot retain a worker indefinitely.
fn relay_events(request: Request, origin: &str) {
    use std::io::{Read, Write};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
    if !super::webhook::try_acquire_slot(&INFLIGHT, 16) {
        return reply(
            request,
            503,
            serde_json::json!({"error":"Too many remote streams"}),
        );
    }
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            super::webhook::release_slot(&INFLIGHT);
        }
    }
    let _slot = Slot;
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_recv_response(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(60)))
            .build(),
    );
    let Ok(mut upstream) = agent.get(format!("{origin}/events")).call() else {
        return reply(
            request,
            502,
            serde_json::json!({"error":"Remote Library stream unavailable"}),
        );
    };
    if upstream.status().as_u16() != 200
        || !upstream
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.split(';').next() == Some("text/event-stream"))
    {
        return reply(
            request,
            502,
            serde_json::json!({"error":"Remote Library does not provide an event stream"}),
        );
    }
    let response = tiny_http::Response::empty(200)
        .with_header(tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap())
        .with_header(tiny_http::Header::from_bytes("Cache-Control", "no-cache").unwrap())
        .with_header(tiny_http::Header::from_bytes("X-Accel-Buffering", "no").unwrap());
    let mut downstream = request.upgrade("sse", response);
    let mut reader = upstream.body_mut().as_reader();
    let mut buf = [0; 8192];
    while let Ok(n) = reader.read(&mut buf) {
        if n == 0 || downstream.write_all(&buf[..n]).is_err() || downstream.flush().is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
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
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
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
        let body =
            serde_json::json!({"url": b.url, "name":"Other", "return_url": a.url}).to_string();
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
}
