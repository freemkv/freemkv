use crate::server::config::{Config, WebhookEntry};

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

impl WebhookEvent {
    // The `"event"` string receivers see for this stage.
    fn name(self) -> &'static str {
        match self {
            WebhookEvent::Rip => "rip_complete",
            WebhookEvent::Mux => "mux_complete",
            WebhookEvent::Move => "move_complete",
        }
    }
}

fn move_payload(title: &str, dest_path: &str) -> serde_json::Value {
    serde_json::json!({
        "event": WebhookEvent::Move.name(),
        "title": title,
        "output_path": dest_path,
    })
}

/// Notify that a file was moved to its final destination.
pub fn send_move(cfg: &Config, title: &str, dest_path: &str) {
    fire(cfg, &move_payload(title, dest_path), WebhookEvent::Move);
}

/// Payload for a `rip_complete` webhook notification. String fields are
/// pre-formatted for display; numeric fields are rounded by [`send_rich`]
/// before serialization.
pub struct RipEvent<'a> {
    /// Informational: [`send_rich`] names the event from its [`WebhookEvent`], so the
    /// payload's `"event"` always matches the hooks it was delivered to.
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
/// `event` also sets the `"event"` field receivers see.
pub fn send_rich(cfg: &Config, event: WebhookEvent, ev: &RipEvent) {
    fire(cfg, &rich_payload(event, ev), event);
}

fn rich_payload(event: WebhookEvent, ev: &RipEvent) -> serde_json::Value {
    serde_json::json!({
        "event": event.name(),
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
        "lost_video_secs": (ev.lost_video_secs * crate::server::util::MILLIS_PER_SEC).round()
            / crate::server::util::MILLIS_PER_SEC,
    })
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
    let mut n = counter.load(Ordering::Acquire);
    while n < max {
        match counter.compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(now) => n = now,
        }
    }
    false
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
    let hooks: Vec<_> = cfg
        .webhook_urls
        .iter()
        .filter(|h| {
            urls.contains(&h.url)
                && match event {
                    WebhookEvent::Rip => h.post_rip,
                    WebhookEvent::Mux => h.post_mux,
                    WebhookEvent::Move => h.post_move,
                }
        })
        .cloned()
        .collect();
    if urls.is_empty() {
        return;
    }
    let body = payload.to_string();

    if !try_acquire_slot(&INFLIGHT, MAX_INFLIGHT) {
        crate::server::log::syslog("Webhook dropped: too many concurrent deliveries in flight");
        return;
    }

    // Guard built HERE on the spawning thread, not inside the closure: if
    // spawn fails, a guard built inside would leave the slot claimed
    // forever. See `web.rs`'s `ConnGuard`, reshaped for the same reason.
    let guard = InflightGuard;
    let spawned = crate::server::daemon::spawn_background("webhook", move || {
        let _guard = guard;
        for hook in &hooks {
            // Deliberately NOT SSRF-guarded: aiming a webhook at a LAN
            // service (Home Assistant, a NAS) is intended use. Goes
            // through un-pinned `web::webhook_agent`; see its doc comment.
            let _ = deliver_authenticated(&hook.url, &body, &hook.headers);
        }
    });
    if spawned.is_err() {
        // The guard was moved into the closure that never ran, so the slot is
        // already released by the failed spawn's drop — nothing leaks. Say so
        // and carry on: a notification is not worth taking the rip down for.
        crate::server::log::syslog("Webhook dropped: could not spawn a delivery thread");
    }
}

// POST one payload to one URL. Split out of `fire` so the one HTTP call in this module can be
// tested against a loopback stub.
#[cfg(test)]
fn deliver(url: &str, body: &str) -> bool {
    deliver_authenticated(url, body, &Default::default())
}

pub(crate) fn test(hook: &WebhookEntry) -> Result<u16, String> {
    if !try_acquire_slot(&INFLIGHT, MAX_INFLIGHT) {
        return Err("Too many webhook requests; try again shortly".into());
    }
    let _guard = InflightGuard;
    let response = send(&hook.url, r#"{"event":"test"}"#, &hook.headers)
        .map_err(|e| crate::server::web::ureq_error_kind(&e))?;
    let status = response.status().as_u16();
    if response.status().is_success() {
        Ok(status)
    } else {
        Err(format!("HTTP {status}"))
    }
}

fn send(
    url: &str,
    body: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let agent = crate::server::web::webhook_agent();
    let mut request = agent.post(url).header("Content-Type", "application/json");
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request.send(body)
}

fn deliver_authenticated(
    url: &str,
    body: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> bool {
    match send(url, body, headers) {
        // NOT every `Ok` is a delivery: at `max_redirects(0)` ureq still
        // returns `Ok` for a 3xx, so an http->https redirected webhook used
        // to log "sent" while nothing delivered. This closes that gap.
        Ok(r) if r.status().is_success() => {
            // Log only the origin — the path may contain a secret token.
            crate::server::log::syslog(&format!("Webhook sent to {}", webhook_url_origin(url)));
            true
        }
        Ok(r) => {
            crate::server::log::syslog(&format!(
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
            let summary = crate::server::web::ureq_error_kind(&e);
            crate::server::log::syslog(&format!(
                "Webhook failed {}: {}",
                webhook_url_origin(url),
                summary
            ));
            false
        }
    }
}

#[cfg(test)]
#[path = "webhook_tests.rs"]
mod tests;
