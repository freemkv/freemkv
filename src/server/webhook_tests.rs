// `send_rich` end-to-end through `fire`'s real spawn path to a loopback stub — the one
// rip-complete delivery this module exists for.

#[test]
fn custom_headers_and_test_status() {
    for status in [204, 401, 302] {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr();
        let worker = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            assert_eq!(request.url(), "/Library/Refresh");
            assert_eq!(
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Authorization"))
                    .unwrap()
                    .value
                    .as_str(),
                "MediaBrowser Token=\"secret-test-key\""
            );
            assert_eq!(
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("X-API-Key"))
                    .unwrap()
                    .value
                    .as_str(),
                "another-secret"
            );
            request.respond(tiny_http::Response::empty(status)).unwrap();
        });
        let hook = WebhookEntry::parse(0, &serde_json::json!({"url":format!("http://{address}/Library/Refresh"),"headers":{"Authorization":"MediaBrowser Token=\"secret-test-key\"","X-API-Key":"another-secret"}})).unwrap();
        let result = super::test(&hook);
        if status == 204 {
            assert_eq!(result.unwrap(), 204);
        } else {
            assert!(result.unwrap_err().contains(&status.to_string()));
        }
        worker.join().unwrap();
    }
}
#[test]
fn send_rich_delivers_the_rip_complete_payload_end_to_end() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    let listener =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");

    let (tx, rx) = std::sync::mpsc::channel();
    let _server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept failed");
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => buf.push(byte[0]),
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("content-length: ")
                    .or_else(|| l.strip_prefix("Content-Length: "))
            })
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = sock.read_exact(&mut body);
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let _ = sock.flush();
        let _ = tx.send(String::from_utf8_lossy(&body).to_string());
    });

    let cfg = Config {
        webhook_urls: vec![both(&format!("http://{pinned}/hook"))],
        ..Default::default()
    };
    let ev = RipEvent {
        event: "rip_complete",
        title: "Some Movie",
        year: 2024,
        format: "BluRay",
        poster_url: "",
        duration: "2h 14m",
        codecs: "HEVC + TrueHD",
        size_gb: 33.333,      // must round to 0.1 → 33.3
        speed_mbs: 12.345,    // must round to 0.1 → 12.3
        elapsed_secs: 1800.6, // must round to whole → 1801
        output_path: "/out/Some Movie.mkv",
        errors: 0,
        lost_video_secs: 0.0,
    };
    super::send_rich(&cfg, WebhookEvent::Rip, &ev);

    // Take the delivery with a DEADLINE: `fire` swallows transport errors,
    // so a wiring regression must fail the test, never hang the suite in
    // the stub's `accept`.
    let body = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("send_rich never delivered to the stub");
    let v: serde_json::Value = serde_json::from_str(&body).expect("valid JSON on the wire");
    assert_eq!(v["event"], "rip_complete");
    assert_eq!(v["title"], "Some Movie");
    assert_eq!(v["size_gb"], 33.3, "size_gb must be rounded to 0.1 GB");
    assert_eq!(
        v["speed_mbs"], 12.3,
        "speed_mbs must be rounded to 0.1 MB/s"
    );
    assert_eq!(
        v["elapsed_secs"], 1801,
        "elapsed_secs must round to whole seconds"
    );
}

// `deliver`'s transport-error arm: a connection refused at a dead port
// must return `false` and never panic. Closing a freshly-bound listener
// yields a port nothing is listening on.
#[test]
fn deliver_reports_a_refused_connection_as_undelivered() {
    use std::net::TcpListener;
    let addr = {
        let l = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind");
        l.local_addr().expect("addr")
        // `l` drops here — the port is now closed, so a connect is refused.
    };
    let delivered = super::deliver(&format!("http://{addr}/hook"), r#"{"event":"x"}"#);
    assert!(!delivered, "a refused connection is not a delivery");
}

// A redirect is NOT a delivery: `webhook_agent` sets `max_redirects(0)`, and at zero ureq's
// `max_redirects_do_error` is false, so a 3xx used to log "Webhook sent".
#[test]
fn a_redirect_is_not_reported_as_a_delivered_webhook() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    let listener =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");

    let _server = std::thread::spawn(move || {
        let Ok((mut sock, _peer)) = listener.accept() else {
            return;
        };
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => buf.push(byte[0]),
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("content-length: ")
                    .or_else(|| l.strip_prefix("Content-Length: "))
            })
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = sock.read_exact(&mut body);
        // The shape a real receiver produces for an http:// webhook URL.
        let _ = sock.write_all(
            b"HTTP/1.1 301 Moved Permanently\r\n\
                  Location: https://hooks.test/services/T000/B000/xxxx\r\n\
                  Content-Length: 0\r\n\r\n",
        );
        let _ = sock.flush();
    });

    let delivered = super::deliver(
        &format!("http://{pinned}/services/T000/B000/xxxx"),
        r#"{"event":"rip_complete"}"#,
    );
    assert!(
        !delivered,
        "a 301 means the payload went nowhere; reporting it as sent is \
             how a silently-broken webhook stays broken"
    );
}

// The webhook POST itself, driven to a real socket — the request that actually carries the
// user's event was never exercised before this.
#[test]
fn deliver_posts_json_with_the_content_type_header() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    let listener =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");

    let (tx, rx) = std::sync::mpsc::channel();
    let _server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept failed");
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        // Read headers, then exactly the promised body length.
        while !buf.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => buf.push(byte[0]),
            }
        }
        let head = String::from_utf8_lossy(&buf).to_string();
        let len: usize = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("content-length: ")
                    .or_else(|| l.strip_prefix("Content-Length: "))
            })
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = sock.read_exact(&mut body);
        let _ = sock.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        let _ = sock.flush();
        let _ = tx.send((head, String::from_utf8_lossy(&body).to_string()));
    });

    let delivered = super::deliver(
        &format!("http://{pinned}/services/T000/B000/xxxx"),
        r#"{"event":"rip_complete"}"#,
    );
    assert!(delivered, "a 204 is a delivery");

    // Take with a DEADLINE: `deliver` swallows transport errors (only
    // logs), so if pinned-resolver wiring regresses, no request arrives
    // and an unconditional `join()` would hang the suite instead of failing.
    let (head, body) = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("no request reached the stub — deliver() never sent one");
    let head_lc = head.to_lowercase();
    assert!(
        head.starts_with("POST /services/T000/B000/xxxx HTTP/1.1"),
        "unexpected request line: {:?}",
        head.lines().next()
    );
    assert!(
        head_lc.contains("content-type: application/json"),
        "the JSON content type was not sent: {head}"
    );
    assert_eq!(
        body, r#"{"event":"rip_complete"}"#,
        "the payload did not reach the wire"
    );
}

use super::*;

#[test]
fn webhook_url_origin_strips_token_path() {
    // Discord-style: secret token in the path must not appear in the log.
    let url = "https://discord.com/api/webhooks/123456/SECRET_TOKEN";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "https://discord.com");
    assert!(!origin.contains("SECRET_TOKEN"));
}

#[test]
fn webhook_url_origin_host_with_port() {
    let url = "http://jellyfin.example:8096/webhook/abc/SECRET";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "http://jellyfin.example:8096");
    assert!(!origin.contains("SECRET"));
}

#[test]
fn webhook_url_origin_bare_origin_no_path() {
    // No path — the whole URL is the origin.
    let url = "https://example.com";
    assert_eq!(webhook_url_origin(url), "https://example.com");
}

#[test]
fn webhook_url_origin_no_scheme_redacted() {
    assert_eq!(webhook_url_origin("not-a-url"), "<redacted>");
    assert_eq!(webhook_url_origin(""), "<redacted>");
}

#[test]
fn webhook_url_origin_strips_query_string_token() {
    // Token in query string (no path slash) must not appear in the log.
    let url = "https://hooks.example.com?token=SUPERSECRET";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "https://hooks.example.com");
    assert!(!origin.contains("SUPERSECRET"));
}

#[test]
fn webhook_url_origin_strips_fragment() {
    let url = "https://example.com#frag";
    assert_eq!(webhook_url_origin(url), "https://example.com");
}

/// Run `deliver` against `url` and return the system-log lines it wrote that mention `mark`.
fn deliver_log(url: &str, mark: &str) -> Vec<String> {
    let _guard = crate::server::log::env_guard();
    let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(format!("webhook-log-{}", std::process::id()));
    std::fs::create_dir_all(d.join("logs")).unwrap();
    // SAFETY: serialized by the guard above.
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let _ = super::deliver(url, r#"{"event":"x"}"#);
    let lines = crate::server::log::get_device_log("system", 500);
    let _ = std::fs::remove_dir_all(&d);
    lines.into_iter().filter(|l| l.contains(mark)).collect()
}

/// The failure lines `deliver` logs carry the origin and status, never the secret path.
#[test]
fn deliver_failure_logs_never_leak_the_url_token() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let _server = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
        }
    });
    let lines = deliver_log(
        &format!("http://{addr}/hook/STATUS_SECRET_TOKEN"),
        "Webhook",
    );
    let line = lines
        .iter()
        .find(|l| l.contains("HTTP 403"))
        .unwrap_or_else(|| panic!("no 403 line in {lines:?}"));
    assert!(line.contains(&format!("http://{addr}")));
    assert!(!lines.iter().any(|l| l.contains("STATUS_SECRET_TOKEN")));

    // Transport failure: nothing listens on the port any more.
    let dead = {
        let l = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        l.local_addr().unwrap()
    };
    let lines = deliver_log(
        &format!("http://{dead}/hook?token=TRANSPORT_SECRET"),
        "Webhook failed",
    );
    assert!(
        lines.iter().any(|l| l.contains(&format!("http://{dead}"))),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("TRANSPORT_SECRET")));
}

/// Every field of the rich payload, with its rounding; the event name follows the stage.
#[test]
fn rich_payload_carries_every_field_and_names_the_stage() {
    let ev = RipEvent {
        event: "rip_complete",
        title: "Some Movie",
        year: 2024,
        format: "UHD",
        poster_url: "https://img/p.jpg",
        duration: "2h 14m",
        codecs: "HEVC",
        size_gb: 33.333,
        speed_mbs: 12.345,
        elapsed_secs: 1800.6,
        output_path: "/out/Some Movie.mkv",
        errors: 7,
        lost_video_secs: 1.23456,
    };
    let v = rich_payload(WebhookEvent::Mux, &ev);
    assert_eq!(
        v["event"], "mux_complete",
        "the stage, not the caller's string"
    );
    assert_eq!(v["title"], "Some Movie");
    assert_eq!(v["year"], 2024);
    assert_eq!(v["format"], "UHD");
    assert_eq!(v["poster_url"], "https://img/p.jpg");
    assert_eq!(v["duration"], "2h 14m");
    assert_eq!(v["codecs"], "HEVC");
    assert_eq!(v["output_path"], "/out/Some Movie.mkv");
    assert_eq!(v["errors"], 7);
    assert_eq!(v["lost_video_secs"], 1.235, "rounded to milliseconds");
    assert_eq!(
        rich_payload(WebhookEvent::Rip, &ev)["event"],
        "rip_complete"
    );
}

/// `send_move` reaches hooks opted in to the move stage (and only those), with the
/// `move_complete` payload.
#[test]
fn send_move_delivers_to_move_hooks() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let _server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => buf.push(byte[0]),
            }
        }
        let head = String::from_utf8_lossy(&buf).to_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = sock.read_exact(&mut body);
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        let _ = tx.send(String::from_utf8_lossy(&body).to_string());
    });
    let cfg = Config {
        webhook_urls: vec![WebhookEntry {
            url: format!("http://{addr}/hook"),
            post_rip: false,
            post_mux: false,
            post_move: true,
            headers: Default::default(),
        }],
        ..Default::default()
    };
    super::send_move(&cfg, "Some Movie", "/lib/Some Movie.mkv");
    let body = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("send_move never reached the move-only hook");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["event"], "move_complete");
    assert_eq!(v["title"], "Some Movie");
    assert_eq!(v["output_path"], "/lib/Some Movie.mkv");
}

// Tests above use long hostnames, under which a past `scheme_end + 3`
// vs `* 3` typo happened to still compute the right origin. These use
// short hosts so that bug would visibly leak the token if it recurred.

#[test]
fn webhook_url_origin_short_host_path_token() {
    let url = "https://a.b/SECRET_TOKEN";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "https://a.b");
    assert!(!origin.contains("SECRET_TOKEN"));
}

#[test]
fn webhook_url_origin_short_host_query_token() {
    let url = "https://x?token=SECRET";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "https://x");
    assert!(!origin.contains("SECRET"));
}

#[test]
fn webhook_url_origin_short_host_nested_path_token() {
    let url = "https://ab.cd/tokenpath/SECRET";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "https://ab.cd");
    assert!(!origin.contains("SECRET"));
    assert!(!origin.contains("tokenpath"));
}

#[test]
fn webhook_url_origin_short_host_fragment_token() {
    let url = "https://a.b#SECRET_FRAGMENT_TOKEN";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "https://a.b");
    assert!(!origin.contains("SECRET_FRAGMENT_TOKEN"));
}

#[test]
fn webhook_url_origin_ip_literal_with_port_and_token() {
    let url = "http://1.2.3.4:9000/hook/SECRET_TOKEN";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "http://1.2.3.4:9000");
    assert!(!origin.contains("SECRET_TOKEN"));
}

#[test]
fn webhook_url_origin_no_scheme_with_embedded_secret_is_fully_redacted() {
    // A malformed/no-scheme "URL" that still contains something
    // token-shaped must never leak that content — the whole thing is
    // replaced with the fixed placeholder, not partially echoed back.
    let url = "hooks.example.com/SECRET_TOKEN?token=ALSO_SECRET";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "<redacted>");
    assert!(!origin.contains("SECRET_TOKEN"));
    assert!(!origin.contains("ALSO_SECRET"));
}

#[test]
fn webhook_url_origin_strips_basic_auth_userinfo() {
    // HTTP basic-auth userinfo can carry a bearer token
    // (`scheme://user:token@host/...`). It must not survive into the
    // logged origin any more than a path- or query-embedded token does.
    let url = "https://autorip:s3cr3t-token@hooks.example.com/notify";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "https://hooks.example.com");
    assert!(!origin.contains("s3cr3t-token"));
    assert!(!origin.contains("autorip:"));
    assert!(!origin.contains('@'));
}

#[test]
fn webhook_url_origin_strips_basic_auth_userinfo_short_host() {
    let url = "http://user:pw@a.b/hook";
    let origin = webhook_url_origin(url);
    assert_eq!(origin, "http://a.b");
    assert!(!origin.contains("pw"));
    assert!(!origin.contains('@'));
}

/// Build a "fires on every stage" entry — the common case and the
/// pre-1.6.8 default — so these tests read as tersely as the old
/// bare-string vectors they replaced.
fn both(url: &str) -> WebhookEntry {
    WebhookEntry {
        url: url.to_string(),
        post_rip: true,
        post_mux: true,
        post_move: true,
        headers: Default::default(),
    }
}

#[test]
fn active_urls_filters_blank_and_whitespace_entries() {
    let entries = vec![
        both(""),
        both("   "),
        both("https://real.example/hook"),
        both("\t\n"),
        both("https://second.example/hook"),
    ];
    // Blank filtering is independent of the event.
    for event in [WebhookEvent::Rip, WebhookEvent::Mux, WebhookEvent::Move] {
        assert_eq!(
            active_urls(&entries, event),
            vec![
                "https://real.example/hook".to_string(),
                "https://second.example/hook".to_string(),
            ]
        );
    }
}

#[test]
fn active_urls_all_blank_yields_empty() {
    let entries = vec![both(""), both("  ")];
    assert!(active_urls(&entries, WebhookEvent::Rip).is_empty());
    assert!(active_urls(&entries, WebhookEvent::Mux).is_empty());
    assert!(active_urls(&entries, WebhookEvent::Move).is_empty());
}

/// Per-stage opt-in is the whole point of the flags: a rip-only hook must
/// never appear in the mux or move dispatch list, a mux-only hook only in
/// the mux list, and so on, while an every-stage hook appears in all three.
#[test]
fn active_urls_selects_by_event_flag() {
    let entries = vec![
        WebhookEntry {
            url: "https://rip-only.example/hook".to_string(),
            post_rip: true,
            post_mux: false,
            post_move: false,
            headers: Default::default(),
        },
        WebhookEntry {
            url: "https://mux-only.example/hook".to_string(),
            post_rip: false,
            post_mux: true,
            post_move: false,
            headers: Default::default(),
        },
        WebhookEntry {
            url: "https://move-only.example/hook".to_string(),
            post_rip: false,
            post_mux: false,
            post_move: true,
            headers: Default::default(),
        },
        both("https://all.example/hook"),
    ];

    assert_eq!(
        active_urls(&entries, WebhookEvent::Rip),
        vec![
            "https://rip-only.example/hook".to_string(),
            "https://all.example/hook".to_string(),
        ]
    );
    assert_eq!(
        active_urls(&entries, WebhookEvent::Mux),
        vec![
            "https://mux-only.example/hook".to_string(),
            "https://all.example/hook".to_string(),
        ]
    );
    assert_eq!(
        active_urls(&entries, WebhookEvent::Move),
        vec![
            "https://move-only.example/hook".to_string(),
            "https://all.example/hook".to_string(),
        ]
    );
}

/// A hook that opted OUT of every stage is inert — it never dispatches,
/// even though its URL is non-blank. (The UI defaults new hooks to all
/// checked, but a raw config could carry this.)
#[test]
fn active_urls_entry_opted_out_of_both_never_fires() {
    let entries = vec![WebhookEntry {
        url: "https://silent.example/hook".to_string(),
        post_rip: false,
        post_mux: false,
        post_move: false,
        headers: Default::default(),
    }];
    assert!(active_urls(&entries, WebhookEvent::Rip).is_empty());
    assert!(active_urls(&entries, WebhookEvent::Mux).is_empty());
    assert!(active_urls(&entries, WebhookEvent::Move).is_empty());
}

// Drives `try_acquire_slot`/`release_slot` directly against a private counter (not the
// shared `INFLIGHT` static) through full acquire/release cycles.
#[test]
fn inflight_slot_cap_and_release_cycle() {
    let counter = AtomicUsize::new(0);
    let max = 3usize;

    // Fill up to the cap.
    assert!(try_acquire_slot(&counter, max));
    assert!(try_acquire_slot(&counter, max));
    assert!(try_acquire_slot(&counter, max));
    assert_eq!(counter.load(Ordering::Acquire), max);

    // At the cap: the next acquire must be rejected and must NOT bump
    // the counter past `max`.
    assert!(!try_acquire_slot(&counter, max));
    assert_eq!(counter.load(Ordering::Acquire), max);

    // Release one slot; a new acquire must now succeed.
    release_slot(&counter);
    assert_eq!(counter.load(Ordering::Acquire), max - 1);
    assert!(try_acquire_slot(&counter, max));
    assert_eq!(counter.load(Ordering::Acquire), max);

    // Release all 3 held slots and confirm the counter hits zero — the
    // guarantee `InflightGuard::drop` must uphold, else it ratchets up
    // forever and every webhook past the cap silently drops thereafter.
    release_slot(&counter);
    release_slot(&counter);
    release_slot(&counter);
    assert_eq!(counter.load(Ordering::Acquire), 0);

    // Fully available again after the drain.
    assert!(try_acquire_slot(&counter, max));
    release_slot(&counter);
}

#[test]
fn try_acquire_slot_rejects_when_max_is_zero() {
    let counter = AtomicUsize::new(0);
    assert!(!try_acquire_slot(&counter, 0));
    assert_eq!(counter.load(Ordering::Acquire), 0);
}
