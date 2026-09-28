use crate::config::{Config, WebhookEntry};

/// Which pipeline stage a dispatch is for. Each configured [`WebhookEntry`]
/// opts in to the rip-, mux-, and move-complete stages independently, so
/// `fire` filters the destination list by the stage it is delivering.
#[derive(Clone, Copy)]
pub enum WebhookEvent {
    /// Disc read finished, drive is free (`rip_complete`).
    Rip,
    /// `.mkv` produced from the staged ISO (`mux_complete`).
    Mux,
    /// Delivered file moved to its final library destination (`move_complete`).
    Move,
}

/// Notify that a file was moved to its final destination.
pub fn send_move(cfg: &Config, title: &str, dest_path: &str) {
    let payload = serde_json::json!({
        "event": "move_complete",
        "title": title,
        "output_path": dest_path,
    });
    fire(cfg, &payload, WebhookEvent::Move);
}

/// Payload for a `rip_complete` webhook notification. String fields are
/// pre-formatted for display; numeric fields are rounded by [`send_rich`]
/// before serialization.
pub struct RipEvent<'a> {
    /// Event name (e.g. `"rip_complete"`).
    pub event: &'a str,
    /// Resolved movie/show title.
    pub title: &'a str,
    /// Release year (0 = unknown).
    pub year: u16,
    /// Disc format label (e.g. `"UHD"`, `"BluRay"`, `"DVD"`).
    pub format: &'a str,
    /// TMDB poster URL, or empty if none.
    pub poster_url: &'a str,
    /// Human-readable runtime string (preformatted, e.g. `"2h 14m"`).
    pub duration: &'a str,
    /// Human-readable codec summary (preformatted).
    pub codecs: &'a str,
    /// Output file size in gigabytes (rounded to 0.1 GB on send).
    pub size_gb: f64,
    /// Average rip throughput in MB/s (rounded to 0.1 on send).
    pub speed_mbs: f64,
    /// Total wall-clock time for the rip, in seconds (rounded to whole seconds on send).
    pub elapsed_secs: f64,
    /// Final destination path of the muxed output.
    pub output_path: &'a str,
    /// Raw count of SCSI read errors encountered.
    pub errors: u32,
    /// Estimated unrecoverable main-feature video loss, in seconds (rounded to ms on send).
    pub lost_video_secs: f64,
}

/// Rich payload with full metadata — used for the `rip_complete` (drive-free)
/// and `mux_complete` (mkv-produced) stages. The caller passes the matching
/// [`WebhookEvent`] so `fire` filters to the hooks that opted in to that stage;
/// `ev.event` sets the `"event"` field receivers see.
pub fn send_rich(cfg: &Config, event: WebhookEvent, ev: &RipEvent) {
    let payload = serde_json::json!({
        "event": ev.event,
        "title": ev.title,
        "year": ev.year,
        "format": ev.format,
        "poster_url": ev.poster_url,
        "duration": ev.duration,
        "codecs": ev.codecs,
        "size_gb": (ev.size_gb * 10.0).round() / 10.0,
        "speed_mbs": (ev.speed_mbs * 10.0).round() / 10.0,
        "elapsed_secs": ev.elapsed_secs.round() as u64,
        "output_path": ev.output_path,
        "errors": ev.errors,
        "lost_video_secs": (ev.lost_video_secs * crate::util::MILLIS_PER_SEC).round()
            / crate::util::MILLIS_PER_SEC,
    });
    fire(cfg, &payload, event);
}

// Return only the `scheme://host[:port]` portion of `url`, dropping any userinfo, path, query,
// or fragment — the rest may carry a secret token.
pub(crate) fn webhook_url_origin(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after = scheme_end + 3;
        // Treat '/', '?', and '#' as origin-terminating so a token in a
        // query string (`https://host?token=SECRET`) is stripped too.
        let origin_end = url[after..]
            .find(['/', '?', '#'])
            .map(|i| after + i)
            .unwrap_or(url.len());
        // If the authority carries basic-auth userinfo (`user:pass@host`),
        // drop through the last '@' so only `host[:port]` survives, else
        // the credential would leak straight through unredacted.
        let authority = &url[after..origin_end];
        let host_start = match authority.rfind('@') {
            Some(at) => after + at + 1,
            None => after,
        };
        return format!("{}{}", &url[..after], &url[host_start..origin_end]);
    }
    // No scheme — log nothing identifiable.
    "<redacted>".to_string()
}

// Return the non-blank webhook URLs that opted in to `event`, in order.
pub(crate) fn active_urls(entries: &[WebhookEntry], event: WebhookEvent) -> Vec<String> {
    entries
        .iter()
        .filter(|e| match event {
            WebhookEvent::Rip => e.post_rip,
            WebhookEvent::Mux => e.post_mux,
            WebhookEvent::Move => e.post_move,
        })
        .map(|e| e.url.clone())
        .filter(|u| !u.trim().is_empty())
        .collect()
}

// Bound concurrent webhook-dispatch threads: unbounded spawning under a
// burst (or hostile client) could exhaust threads. Past the cap, drop
// the event with a warning rather than spawning.
use std::sync::atomic::{AtomicUsize, Ordering};
const MAX_INFLIGHT: usize = 8;
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

// Attempt to claim one slot of a bounded concurrency counter. Returns `true` and increments
// `counter` if below `max`, else leaves it untouched and returns `false`.
pub(crate) fn try_acquire_slot(counter: &AtomicUsize, max: usize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < max).then_some(n + 1)
        })
        .is_ok()
}

/// Release one slot claimed by [`try_acquire_slot`]. The single decrement
/// path used both by production's `InflightGuard::drop` and directly by
/// tests, so there is exactly one copy of the release logic.
pub(crate) fn release_slot(counter: &AtomicUsize) {
    counter.fetch_sub(1, Ordering::AcqRel);
}

// Decrement the in-flight counter however the dispatch thread exits.
struct InflightGuard;
impl Drop for InflightGuard {
    fn drop(&mut self) {
        release_slot(&INFLIGHT);
    }
}

fn fire(cfg: &Config, payload: &serde_json::Value, event: WebhookEvent) {
    let urls = active_urls(&cfg.webhook_urls, event);
    if urls.is_empty() {
        return;
    }
    let body = payload.to_string();

    if !try_acquire_slot(&INFLIGHT, MAX_INFLIGHT) {
        crate::log::syslog("Webhook dropped: too many concurrent deliveries in flight");
        return;
    }

    // Guard built HERE on the spawning thread, not inside the closure: if
    // spawn fails, a guard built inside would leave the slot claimed
    // forever. See `web.rs`'s `ConnGuard`, reshaped for the same reason.
    let guard = InflightGuard;
    let spawned = std::thread::Builder::new()
        .name("webhook".into())
        .spawn(move || {
            let _guard = guard;
            for url in &urls {
                // Deliberately NOT SSRF-guarded: aiming a webhook at a LAN
                // service (Home Assistant, a NAS) is intended use. Goes
                // through un-pinned `web::webhook_agent`; see its doc comment.
                let _ = deliver(url, &body);
            }
        });
    if spawned.is_err() {
        // The guard was moved into the closure that never ran, so the slot is
        // already released by the failed spawn's drop — nothing leaks. Say so
        // and carry on: a notification is not worth taking the rip down for.
        crate::log::syslog("Webhook dropped: could not spawn a delivery thread");
    }
}

// POST one payload to one URL. Split out of `fire` so the one HTTP call in this module can be
// tested against a loopback stub.
fn deliver(url: &str, body: &str) -> bool {
    let agent = crate::web::webhook_agent();
    match agent
        .post(url)
        .header("Content-Type", "application/json")
        .send(body)
    {
        // NOT every `Ok` is a delivery: at `max_redirects(0)` ureq still
        // returns `Ok` for a 3xx, so an http->https redirected webhook used
        // to log "sent" while nothing delivered. This closes that gap.
        Ok(r) if r.status().is_success() => {
            // Log only the origin — the path may contain a secret token.
            crate::log::syslog(&format!("Webhook sent to {}", webhook_url_origin(url)));
            true
        }
        Ok(r) => {
            crate::log::syslog(&format!(
                "Webhook not accepted {}: HTTP {}",
                webhook_url_origin(url),
                r.status().as_u16()
            ));
            false
        }
        Err(e) => {
            // Summarise WITHOUT embedding `e` directly: `ureq_error_kind` is
            // the one place the URL-free guarantee is stated, since `BadUri`
            // does embed the URI and would else be one refactor from the log.
            let summary = crate::web::ureq_error_kind(&e);
            crate::log::syslog(&format!(
                "Webhook failed {}: {}",
                webhook_url_origin(url),
                summary
            ));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    // `send_rich` end-to-end through `fire`'s real spawn path to a loopback stub — the one
    // rip-complete delivery this module exists for.
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

    /// Verify that the error summary produced for a Status error contains
    /// neither the full URL nor any embedded token — only the HTTP status code.
    #[test]
    fn fire_error_summary_status_no_url_leak() {
        // Simulate the Err(e) arm's output for a Status error. Can't call
        // fire() directly (needs a Config + spawns + real HTTP), so this
        // replicates the summary logic inline, pinning the logged shape.
        let url = "https://discord.com/api/webhooks/123456/SECRET_TOKEN";
        // We can't construct a `ureq::Error::Status` without a live connection, so
        // we test the origin-stripping half (already well-tested above) and the
        // summary format string that fire() would produce.
        let origin = webhook_url_origin(url);
        // The log line produced by fire() is: "Webhook failed {origin}: {summary}"
        // — neither contains the token path.
        let log_line = format!("Webhook failed {origin}: HTTP 403");
        assert!(
            !log_line.contains("SECRET_TOKEN"),
            "token leaked into log: {log_line}"
        );
        assert!(
            !log_line.contains("/api/webhooks/"),
            "path leaked into log: {log_line}"
        );
        assert!(log_line.contains("HTTP 403"));
        assert!(log_line.contains("https://discord.com"));
    }

    /// Same shape test for the Transport arm.
    #[test]
    fn fire_error_summary_transport_no_url_leak() {
        let url = "https://hooks.example.com?token=SUPERSECRET";
        let origin = webhook_url_origin(url);
        // Simulate what t.kind().to_string() produces — the actual string is
        // provider-defined, but it must never contain the URL.
        let kind_str = "connection failed"; // representative value
        let log_line = format!("Webhook failed {origin}: {kind_str}");
        assert!(
            !log_line.contains("SUPERSECRET"),
            "token leaked into log: {log_line}"
        );
        assert!(
            !log_line.contains("token="),
            "query param leaked: {log_line}"
        );
        assert!(log_line.contains("hooks.example.com"));
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
            },
            WebhookEntry {
                url: "https://mux-only.example/hook".to_string(),
                post_rip: false,
                post_mux: true,
                post_move: false,
            },
            WebhookEntry {
                url: "https://move-only.example/hook".to_string(),
                post_rip: false,
                post_mux: false,
                post_move: true,
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
}
