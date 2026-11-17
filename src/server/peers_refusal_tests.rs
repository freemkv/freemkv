//! The refusals a connection and the proxy give: bad input, a silent or foreign upstream, an
//! unknown Library and a stream the remote does not offer.
use super::*;
use std::io::{Read, Write};

fn cfg() -> (tempfile::TempDir, Arc<RwLock<Config>>) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RwLock::new(Config {
        autorip_dir: dir.path().to_str().unwrap().into(),
        ..Config::default()
    }));
    (dir, cfg)
}

fn closed_origin() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://{}", l.local_addr().unwrap())
}

// An upstream answering every connection with `reply` until the test ends.
fn upstream(reply: &'static [u8]) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", l.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut s) = stream else { return };
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = s.write_all(reply);
        }
    });
    origin
}

#[test]
fn connect_refuses_bad_names_silent_hosts_and_non_libraries() {
    let (_d, cfg) = cfg();
    let long = "x".repeat(101);
    let body = serde_json::json!({"url": closed_origin(), "name": long}).to_string();
    assert!(connect(&cfg, &body).unwrap_err().contains("100"));
    let body = serde_json::json!({"url": closed_origin()}).to_string();
    assert_eq!(connect(&cfg, &body).unwrap_err(), "Library did not answer");
    let html = upstream(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnope");
    let body = serde_json::json!({"url": html}).to_string();
    assert_eq!(connect(&cfg, &body).unwrap_err(), "Not a Library server");
    let other = upstream(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"other\":1}",
    );
    let body = serde_json::json!({"url": other}).to_string();
    assert_eq!(connect(&cfg, &body).unwrap_err(), "Not a Library server");
    assert!(read(&cfg).unwrap().is_empty());
}

#[test]
fn a_library_that_cannot_connect_back_is_kept_with_a_warning() {
    let (_d, cfg) = cfg();
    let lib = upstream(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 18\r\nConnection: close\r\n\r\n{\"version\":\"test\"}",
    );
    let body =
        serde_json::json!({"url": lib, "name": "Lib", "return_url": closed_origin()}).to_string();
    let warning = connect(&cfg, &body).unwrap();
    assert!(warning.is_some_and(|w| w.contains("could not connect back")));
    assert_eq!(read(&cfg).unwrap().len(), 1);
}

// Serve one request through `handle` and return its status.
fn through_handle(cfg: &Arc<RwLock<Config>>, method: &str, path: &str) -> u16 {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}{path}", server.server_addr());
    let cfg = cfg.clone();
    let worker = std::thread::spawn(move || handle(server.recv().unwrap(), &cfg));
    let status = if method == "POST" {
        agent()
            .post(url)
            .content_type("application/json")
            .send("{}".as_bytes())
    } else {
        agent().get(url).call()
    }
    .unwrap()
    .status()
    .as_u16();
    worker.join().unwrap();
    status
}

#[test]
fn the_proxy_refuses_unknown_routes_libraries_and_streams() {
    let (_d, cfg) = cfg();
    assert_eq!(through_handle(&cfg, "GET", "/api/peers"), 200);
    assert_eq!(through_handle(&cfg, "GET", "/api/peers/identity"), 200);
    assert_eq!(through_handle(&cfg, "GET", "/api/peers/nothing"), 404);
    assert_eq!(through_handle(&cfg, "GET", "/api/peers/p1/api/state"), 404);
    assert_eq!(
        through_handle(&cfg, "GET", "/api/peers/p1/api/settings"),
        403
    );
    let body = serde_json::json!({"url": "file:///x"}).to_string();
    assert!(connect(&cfg, &body).is_err());
    assert_eq!(through_handle(&cfg, "POST", "/api/peers"), 400);
    // A saved Library that is offline or serves no event stream.
    let plain = upstream(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi",
    );
    let peers = vec![
        Peer {
            id: "pdead".into(),
            name: "Dead".into(),
            url: closed_origin(),
        },
        Peer {
            id: "pplain".into(),
            name: "Plain".into(),
            url: plain,
        },
    ];
    save(&cfg, &peers).unwrap();
    assert_eq!(through_handle(&cfg, "GET", "/api/peers/pdead/events"), 502);
    assert_eq!(
        through_handle(&cfg, "GET", "/api/peers/pdead/api/state"),
        502
    );
    assert_eq!(through_handle(&cfg, "GET", "/api/peers/pplain/events"), 502);
    assert_eq!(
        through_handle(&cfg, "GET", "/api/peers/pplain/api/state"),
        200
    );
}
