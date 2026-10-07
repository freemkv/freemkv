use crate::server::config::{self, Config, WebhookEntry};
use crate::server::ripper;
use once_cell::sync::Lazy;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use tiny_http::{Header, Method, Response, Server, StatusCode};

/// Runtime debug flag - toggled via /api/debug POST.
pub static DEBUG_ENABLED: Lazy<Arc<RwLock<bool>>> = Lazy::new(|| Arc::new(RwLock::new(false)));

/// Check if debug logging is enabled.
///
/// Poison-tolerant: this runs on the mux hot path, so a panic elsewhere
/// while the write guard is held must not turn every later call into a
/// panic (which would kill the mux thread). Recover the inner value
/// instead of unwrapping.
pub fn debug_enabled() -> bool {
    *DEBUG_ENABLED
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// The web UI: real files under web/assets/, embedded at build time (no build
// step). Plain ES modules; `app.js` routes between the page modules.
const INDEX_HTML: &str = include_str!("web/assets/index.html");
const ASSETS: &[(&str, &str, &[u8])] = &[
    (
        "app.css",
        "text/css; charset=utf-8",
        include_bytes!("web/assets/app.css"),
    ),
    (
        "app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/app.js"),
    ),
    (
        "connection.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/connection.js"),
    ),
    (
        "bus.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/bus.js"),
    ),
    (
        "ui.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/ui.js"),
    ),
    (
        "chips.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/chips.js"),
    ),
    (
        "medialist.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/medialist.js"),
    ),
    (
        "libdata.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/libdata.js"),
    ),
    (
        "details.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/details.js"),
    ),
    (
        "auditview.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/auditview.js"),
    ),
    (
        "folders.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/folders.js"),
    ),
    (
        "console.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/console.js"),
    ),
    (
        "library.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/library.js"),
    ),
    (
        "remux.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/remux.js"),
    ),
    (
        "ripper.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/ripper.js"),
    ),
    (
        "settings.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/settings.js"),
    ),
    (
        "system.js",
        "text/javascript; charset=utf-8",
        include_bytes!("web/assets/system.js"),
    ),
    (
        "freemkv-icon.svg",
        "image/svg+xml",
        include_bytes!("web/assets/freemkv-icon.svg"),
    ),
    (
        "favicon.svg",
        "image/svg+xml",
        include_bytes!("web/assets/favicon.svg"),
    ),
];

// The app's own paths: each serves the shell, which routes client-side.
const PAGES: &[&str] = &[
    "/",
    "/index.html",
    "/library",
    "/remux",
    "/drives",
    "/ripper",
    "/settings",
    "/system",
];

pub fn run(cfg: &Arc<RwLock<Config>>) {
    let port = cfg.read().unwrap_or_else(|e| e.into_inner()).port;
    let addr = format!("0.0.0.0:{}", port);
    let server = match Server::http(&addr) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            // Bind failure is unrecoverable — without a UI we have a dead
            // daemon. Signal SHUTDOWN so main exits non-zero and the
            // container restart policy recovers us.
            crate::server::log::syslog(&format!(
                "FATAL: web server bind failed on {}: {} — signalling shutdown",
                addr, e
            ));
            tracing::error!(
                address = %addr,
                error = %e,
                "web bind failed; signalling shutdown so the container restart policy recovers us"
            );
            crate::server::SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
            return;
        }
    };
    crate::server::log::syslog(&format!("Web server listening on {}", addr));
    tracing::info!(address = %addr, "web server listening");

    for request in server.incoming_requests() {
        if crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        // Bound concurrent handlers so a flood can't fork the container.
        // Body-carrying requests get a lower cap: tiny_http 0.12 has no
        // read timeout, so a stalled sender could starve the healthcheck.
        let cap = if carries_body(&request) {
            MAX_INFLIGHT_BODY_HANDLERS
        } else {
            MAX_INFLIGHT_HANDLERS
        };
        let guard = match ConnGuard::try_acquire(&INFLIGHT_HANDLERS, cap) {
            Some(g) => g,
            None => {
                tracing::warn!(max = cap, "request rejected: in-flight handler cap reached");
                json_response(request, 503, r#"{"ok":false,"error":"server busy"}"#);
                continue;
            }
        };
        let cfg = Arc::clone(cfg);
        if let Err(e) = std::thread::Builder::new()
            .name("autorip-http".into())
            .spawn(move || {
                // Hold the admission token for the handler's lifetime;
                // dropped here on return/unwind, freeing the slot.
                let _guard = guard;
                handle_request(request, &cfg);
            })
        {
            tracing::error!(error = %e, "failed to spawn request handler thread");
            // guard drops here, freeing the reserved slot.
        }
    }
    tracing::info!("web server stopping");
}

/// Extract a header value by case-insensitive field name.
fn header_value<'a>(request: &'a tiny_http::Request, name: &str) -> Option<&'a str> {
    // `HeaderField::equiv` requires a `&'static str`; compare the field
    // name ourselves so we can take a borrowed `name`. HTTP header field
    // names are case-insensitive.
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// Pull the host\[:port\] authority out of a URL or a bare Host header value.
fn authority_of(s: &str) -> Option<String> {
    // Strip scheme (origin headers look like `http://host:port`); Host
    // headers are already bare. Then strip any path/query tail. The scheme is
    // the FIRST `://`: a Referer's path or query may embed another URL.
    let after_scheme = s.split_once("://").map_or(s, |(_, rest)| rest);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme)
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// Default TCP port implied by a URL scheme (used to normalize an authority
/// that omits its port). Only the two web schemes matter here.
fn default_port_for_scheme(s: &str) -> u16 {
    if s.starts_with("https://") {
        443
    } else {
        // http:// and bare Host values (no scheme) both default to 80,
        // which is the right comparison baseline for a same-origin POST.
        80
    }
}

// Normalize an authority (`host` or `host:port`) to canonical `host:port`,
// filling in `default_port` when omitted, so `http://host` (Origin) compares
// equal to `host:80` (Host header) instead of falsely reading cross-origin.
fn normalize_authority(authority: &str, default_port: u16) -> Option<String> {
    let a = authority_of(authority)?;
    // Bracketed IPv6 literal: [::1] or [::1]:8080.
    if let Some(rest) = a.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().ok()?,
            None if after.is_empty() => default_port,
            None => return None,
        };
        return Some(format!("[{host}]:{port}"));
    }
    match a.rsplit_once(':') {
        // Trailing ':NNN' is a port only if numeric; otherwise treat the
        // whole thing as a host (defensive — keeps a stray colon from
        // silently dropping the port).
        Some((host, p)) => match p.parse::<u16>() {
            Ok(port) => Some(format!("{host}:{port}")),
            Err(_) => Some(format!("{a}:{default_port}")),
        },
        None => Some(format!("{a}:{default_port}")),
    }
}

// Lightweight CSRF defense for state-changing POSTs: reject only when an Origin/Referer header
// is present and disagrees with Host (403); an absent header is allowed so curl/monitoring keep
// working.
fn is_cross_origin_post(request: &tiny_http::Request) -> bool {
    let origin = header_value(request, "Origin").or_else(|| header_value(request, "Referer"));
    let host = header_value(request, "Host");
    is_cross_origin(origin, host)
}

// Pure cross-origin decision over raw Origin/Referer + Host header values; `true` means reject.
// Absent/unparseable input can't prove cross-origin, so it's allowed.
fn is_cross_origin(origin: Option<&str>, host: Option<&str>) -> bool {
    let origin = match origin {
        None => return false,
        Some(o) if o.trim().is_empty() => return false,
        Some(o) => o,
    };
    // Origin carries the scheme, which fixes the default port so the
    // schemeless Host header normalizes to match it — otherwise
    // `http://host` wouldn't match `host:80`, falsely 403'ing same-origin.
    let default_port = default_port_for_scheme(origin.trim());
    let origin_norm = match normalize_authority(origin, default_port) {
        Some(h) => h,
        None => return false,
    };
    let host_norm = match host.and_then(|h| normalize_authority(h, default_port)) {
        Some(h) => h,
        None => return false,
    };
    origin_norm != host_norm
}

fn handle_request(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let url = request.url().to_string();
    let is_get = *request.method() == Method::Get;
    let is_post = *request.method() == Method::Post;

    // Defense-in-depth CSRF check: reject a state-changing POST whose
    // Origin/Referer host disagrees with our Host header. Absent header is
    // allowed so curl/monitoring scripts keep working (see helper doc).
    if is_post && is_cross_origin_post(&request) {
        return json_response(
            request,
            403,
            r#"{"ok":false,"error":"cross-origin request rejected"}"#,
        );
    }

    let path = url.split('?').next().unwrap_or("");
    if path == "/api/peers" || path.starts_with("/api/peers/") {
        crate::server::peers::handle(request, cfg);
    } else if is_get && PAGES.contains(&path) {
        serve_html(request);
    } else if is_get && path == "/favicon.svg" {
        serve_asset(request, "favicon.svg");
    } else if is_get && path.starts_with("/assets/") {
        serve_asset(request, &path["/assets/".len()..]);
    } else if is_get && url == "/api/state" {
        let staging_dir = cfg
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .staging_dir
            .clone();
        json_response(request, 200, &get_state_json(&staging_dir));
    } else if is_get && url == "/api/version" {
        json_response(
            request,
            200,
            &format!("{{\"version\":\"{}\"}}", crate::server::VERSION_LABEL),
        );
    } else if is_get && url == "/api/settings" {
        // Snapshot and drop the guard: rendering stats the keydb path, and
        // filesystem I/O (NFS) must not stall config writers.
        let c = match cfg.read() {
            Ok(c) => c.clone(),
            Err(_) => {
                return json_response(
                    request,
                    500,
                    r#"{"ok":false,"error":"config lock poisoned"}"#,
                );
            }
        };
        let json = settings_json_redacted(&c);
        json_response(request, 200, &json);
    } else if is_get && url == "/api/settings/schema" {
        json_response(
            request,
            200,
            &crate::server::settings_schema::schema_json().to_string(),
        );
    } else if is_post && url == "/api/settings" {
        handle_settings_post(request, cfg);
    } else if is_post && url == "/api/webhook/test" {
        handle_webhook_test(request, cfg);
    } else if is_get && url == "/api/system" {
        handle_system_info(request, cfg);
    } else if is_post && url == "/api/move-errors/clear-all" {
        crate::server::mover::clear_all_move_errors();
        json_response(request, 200, r#"{"ok":true}"#);
    } else if is_post && url.starts_with("/api/move-errors/clear?") {
        // Clear ONE move error by path. The path carries slashes/spaces, so it
        // arrives percent-encoded in the `path=` query param.
        let query = url.split_once('?').map(|x| x.1).unwrap_or("");
        let target = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("path="))
            .map(percent_decode)
            .unwrap_or_default();
        if target.is_empty() {
            return json_response(request, 400, r#"{"ok":false,"error":"missing path"}"#);
        }
        crate::server::mover::clear_move_error(&target);
        json_response(request, 200, r#"{"ok":true}"#);
    } else if is_post && url == "/api/mux-errors/clear-all" {
        crate::server::muxer::clear_all_mux_errors();
        json_response(request, 200, r#"{"ok":true}"#);
    } else if is_post && url.starts_with("/api/mux-errors/clear?") {
        // Clear ONE mux error by path (percent-encoded `path=` query param).
        let query = url.split_once('?').map(|x| x.1).unwrap_or("");
        let target = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("path="))
            .map(percent_decode)
            .unwrap_or_default();
        if target.is_empty() {
            return json_response(request, 400, r#"{"ok":false,"error":"missing path"}"#);
        }
        crate::server::muxer::clear_mux_error(&target);
        json_response(request, 200, r#"{"ok":true}"#);
    } else if is_get && url == "/api/logs/download" {
        handle_logs_download(request, cfg);
    } else if is_post && url == "/api/system/keyserver-test" {
        handle_keyserver_test(request, cfg);
    } else if is_get && url.starts_with("/api/logs/") {
        let rest = url.trim_start_matches("/api/logs/");
        let device = percent_decode(rest.split('?').next().unwrap_or(""));
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_device_log(request, &device);
    } else if is_post && url == "/api/debug" {
        handle_debug_toggle(request);
    } else if is_get && (url == "/api/debug" || url.starts_with("/api/debug?")) {
        handle_debug_log(request, &url);
    } else if is_get && url == "/events" {
        handle_sse(request, cfg);
    } else if is_post && url.starts_with("/api/scan/") {
        let device = url.trim_start_matches("/api/scan/");
        let device = percent_decode(device);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_scan(request, cfg, &device);
    } else if is_post && url.starts_with("/api/rip/") {
        let path = url.trim_start_matches("/api/rip/");
        // Split off the query string. URL form: /api/rip/<device>[?resume=yes|no]
        let (device_raw, query) = match path.split_once('?') {
            Some((d, q)) => (d, q),
            None => (path, ""),
        };
        let device = percent_decode(device_raw);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_rip(request, cfg, &device, query);
    } else if is_post && url.starts_with("/api/accept-loss/") {
        let device = percent_decode(url.trim_start_matches("/api/accept-loss/"));
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_accept_loss(request, cfg, &device);
    } else if is_post && url == "/api/update-keydb" {
        handle_update_keydb(request, cfg);
    } else if is_post && url.starts_with("/api/eject/") {
        let device = url.trim_start_matches("/api/eject/");
        let device = percent_decode(device);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_eject(request, &device);
    } else if is_post && url.starts_with("/api/stop/") {
        let device = url.trim_start_matches("/api/stop/");
        let device = percent_decode(device);
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_stop(request, &device);
    } else if is_get && url == "/api/review" {
        let staging = cfg
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .staging_dir
            .clone();
        let items = crate::server::review::list_held(&staging);
        json_response(
            request,
            200,
            &serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string()),
        );
    } else if is_post && url == "/api/review/resolve" {
        handle_review_resolve(request, cfg);
    } else if is_get && url.starts_with("/api/tmdb/search") {
        handle_tmdb_search(request, cfg, &url);
    } else if is_post && url.starts_with("/api/title/") {
        let device = percent_decode(url.trim_start_matches("/api/title/"));
        if !is_valid_device_name(&device) {
            return json_response(request, 400, r#"{"error":"invalid device name"}"#);
        }
        handle_title_override(request, &device);
    } else if url.starts_with("/api/library") {
        if let Some(request) = crate::server::library::api::handle(request, cfg) {
            json_response(request, 404, r#"{"error":"not found"}"#);
        }
    } else {
        json_response(request, 404, r#"{"error":"not found"}"#);
    }
}

// Defensive check for an operator poster URL later interpolated into an
// `<img src>` attribute: require http(s) and reject control chars/quotes so
// it can't break out of the attribute even if front-end escaping regresses.
fn is_valid_poster_url(url: &str) -> bool {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    !url.chars()
        .any(|c| c.is_control() || c == '"' || c == '\'' || c == '<' || c == '>')
}

// 404 body for per-device routes naming a device with no STATE entry: never enumerated, or a
// hot-plugged drive still in the poll loop's 60s first-seen settle window.
const UNKNOWN_DEVICE_BODY: &str = r#"{"ok":false,"error":"unknown or not yet initialized device"}"#;

// POST /api/title/<device>: operator's TMDB pick for the active disc.
// Body: {"title","year","poster_url","overview"}. Stored as a one-shot
// override `rip_disc` consumes; also reflected on the live card immediately.
fn handle_title_override(request: tiny_http::Request, device: &str) {
    // An override for an untracked drive has nothing to attach to and
    // would persist orphaned; reject before reading the body, matching
    // how other per-device routes validate (404 unknown).
    if !ripper::device_known(device) {
        return json_response(request, 404, UNKNOWN_DEVICE_BODY);
    }
    let (request, body) = match read_json_body(request) {
        Ok(rb) => rb,
        Err(()) => return,
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return json_response(request, 400, r#"{"ok":false,"error":"invalid json"}"#),
    };
    // Clamp operator-supplied free text on char boundaries before it's
    // persisted and re-broadcast to every dashboard client (mirrors the
    // 200-char `q` cap). Caps: title ~300, overview ~2000, poster_url ~1000.
    let title = clamp_chars(v["title"].as_str().unwrap_or("").trim(), 300);
    if title.is_empty() {
        return json_response(request, 400, r#"{"ok":false,"error":"title required"}"#);
    }
    let year = v["year"]
        .as_u64()
        .and_then(|y| u16::try_from(y).ok())
        .unwrap_or(0);
    let poster_raw = v["poster_url"].as_str().unwrap_or("");
    if !poster_raw.is_empty() && !is_valid_poster_url(poster_raw) {
        return json_response(request, 400, r#"{"ok":false,"error":"invalid poster_url"}"#);
    }
    let poster = clamp_chars(poster_raw, 1000);
    let overview = clamp_chars(v["overview"].as_str().unwrap_or(""), 2000);
    // Preserve the disc's DETECTED media_type when the caller omits one
    // (Manual Rename does): defaulting to "movie" would flip a TV disc and
    // collapse all its episodes into one file. Fall back only when unknown.
    let media_type = match v["media_type"].as_str().filter(|s| !s.is_empty()) {
        Some(mt) => normalize_media_type(mt),
        None => {
            // Recover a poisoned lock (`into_inner`) like every other STATE
            // consumer here — abandoning would fall through to the "movie"
            // default this arm exists to avoid.
            let current = ripper::STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(device)
                .map(|rs| rs.tmdb_media_type.clone())
                .unwrap_or_default();
            normalize_media_type(if current.is_empty() {
                "movie"
            } else {
                &current
            })
        }
    };
    // The picker posts back the chosen result's TMDB id (0 if the operator
    // typed a free-form title with no pick); carry it so the override matches
    // what a `lookup` match would have provided.
    let tmdb_id = v["tmdb_id"].as_u64().unwrap_or(0);
    ripper::set_title_override(
        device,
        crate::server::tmdb::TmdbResult {
            title: title.clone(),
            year,
            poster_url: poster.clone(),
            overview: overview.clone(),
            media_type: media_type.clone(),
            tmdb_id,
        },
    );
    // Reflect on the live card right away, exactly as the engine will use the
    // override: the previous match's poster/overview/type do not carry over.
    ripper::update_state_with(device, |s| {
        s.tmdb_title = title.clone();
        s.tmdb_year = year;
        s.tmdb_poster = poster.clone();
        s.tmdb_overview = overview.clone();
        s.tmdb_media_type = media_type.clone();
    });
    json_response(request, 200, r#"{"ok":true}"#);
}

/// `POST /api/review/resolve` — resolve a held rip. Body:
/// `{"dir":"<staging subdir>","action":"proceed|retitle|cancel","title":"…","year":2024}`.
fn handle_review_resolve(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let (request, body) = match read_json_body(request) {
        Ok(rb) => rb,
        Err(()) => return,
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return json_response(request, 400, r#"{"ok":false,"error":"invalid json"}"#),
    };
    // Cap operator-supplied strings before they reach a filesystem marker,
    // mirroring handle_title_override (clamp_chars by char count, not bytes).
    let dir = clamp_chars(v["dir"].as_str().unwrap_or("").trim(), 300);
    let staging = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .staging_dir
        .clone();
    let action = match v["action"].as_str().unwrap_or("") {
        "proceed" => crate::server::review::Resolve::Proceed,
        "cancel" => crate::server::review::Resolve::Cancel,
        "retitle" => {
            let title = clamp_chars(v["title"].as_str().unwrap_or("").trim(), 300);
            if title.is_empty() {
                return json_response(request, 400, r#"{"ok":false,"error":"title required"}"#);
            }
            let year = v["year"]
                .as_u64()
                .and_then(|y| u16::try_from(y).ok())
                .unwrap_or(0);
            crate::server::review::Resolve::Retitle { title, year }
        }
        _ => return json_response(request, 400, r#"{"ok":false,"error":"bad action"}"#),
    };
    match crate::server::review::resolve(&staging, &dir, action) {
        Ok(()) => json_response(request, 200, r#"{"ok":true}"#),
        Err(e) => {
            // Build the error payload with serde so backslashes, newlines,
            // and control chars in a filesystem error string are escaped
            // properly — manual quote-replacement produced malformed JSON.
            let body = serde_json::json!({ "ok": false, "error": e }).to_string();
            json_response(request, 400, &body)
        }
    }
}

/// `GET /api/tmdb/search?q=<query>` — candidate matches for the review picker.
fn handle_tmdb_search(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, url: &str) {
    // Parse via parse_query so `q` is found regardless of parameter order
    // (split_once("?q=") only matched q as the first query parameter, so
    // e.g. /api/tmdb/search?version=2&q=movie yielded an empty query).
    let q = parse_query(url).get("q").cloned().unwrap_or_default();
    let q = q.trim();
    // Reject empty queries and cap length so we never forward an abusive
    // request to TMDB.
    if q.is_empty() || q.len() > 200 {
        return json_response(request, 400, r#"{"error":"invalid query"}"#);
    }
    // Without a key every search is empty; say so rather than answering as if
    // TMDB had no matches.
    let key = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .tmdb_api_key
        .clone();
    if key.is_empty() {
        return json_response(
            request,
            400,
            r#"{"error":"no TMDB API key set; add one in Settings"}"#,
        );
    }
    // Global cooldown: an unauthenticated LAN client could otherwise flood
    // TMDB through this proxy. Gate on the time since the last forwarded
    // search; reply 429 if a request arrived too recently.
    {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        static LAST_TMDB_SEARCH: Mutex<Option<Instant>> = Mutex::new(None);
        const TMDB_MIN_INTERVAL: Duration = Duration::from_millis(500);
        let mut last = LAST_TMDB_SEARCH.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let Some(prev) = *last
            && now.duration_since(prev) < TMDB_MIN_INTERVAL
        {
            return json_response(request, 429, r#"{"error":"rate limited"}"#);
        }
        *last = Some(now);
    }
    let results = crate::server::tmdb::search(q, &key, 8);
    json_response(
        request,
        200,
        &serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string()),
    );
}

// ---------- Helpers ----------

fn serve_html(request: tiny_http::Request) {
    let header =
        Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    let html = INDEX_HTML.replace("{VERSION}", crate::server::VERSION_LABEL);
    // The shell is never cached, so a deploy is picked up on the next load.
    let response = Response::from_string(html).with_header(header).with_header(
        Header::from_bytes(
            &b"Cache-Control"[..],
            &b"no-store, no-cache, must-revalidate"[..],
        )
        .unwrap(),
    );
    let _ = request.respond(response);
}

// One embedded asset by name, or a 404. Revalidated on every load (`no-cache`)
// so the modules the shell imports always match the running build.
fn serve_asset(request: tiny_http::Request, name: &str) {
    let Some((_, ctype, body)) = ASSETS.iter().find(|(n, _, _)| *n == name) else {
        return json_response(request, 404, r#"{"error":"not found"}"#);
    };
    let etag = format!(
        "\"{}\"",
        crate::server::VERSION_LABEL.replace(['"', ' '], "")
    );
    if header_value(&request, "If-None-Match") == Some(etag.as_str()) {
        let _ = request.respond(Response::empty(304));
        return;
    }
    let response = Response::from_data(*body)
        .with_header(Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap())
        .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).unwrap())
        .with_header(Header::from_bytes(&b"ETag"[..], etag.as_bytes()).unwrap());
    let _ = request.respond(response);
}

pub(crate) fn json_response(request: tiny_http::Request, status: u16, body: &str) {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    // This is a local control app, not a website: NOTHING is cacheable. The API
    // responses (state/version/etc.) are polled live, so a cached body would show
    // stale rip state. Match the HTML shell's no-store.
    let response = Response::from_string(body)
        .with_status_code(StatusCode(status))
        .with_header(header)
        .with_header(
            Header::from_bytes(
                &b"Cache-Control"[..],
                &b"no-store, no-cache, must-revalidate"[..],
            )
            .unwrap(),
        );
    let _ = request.respond(response);
}

// Sentinel returned in place of a stored secret on GET /api/settings; a
// POST field carrying exactly this value is treated as "unchanged" so the
// UI can round-trip the redacted form without clobbering the real secret.
use crate::server::settings_schema::SECRET_SENTINEL;

// Mask a webhook URL for display: keep the origin (scheme://host[:port])
// but replace the path/query — where the secret token lives — with the
// sentinel; a value containing the sentinel round-trips as "unchanged".
pub(crate) fn mask_webhook_url(url: &str) -> String {
    // Origin = everything up to the first '/', '?', or '#' after `scheme://`.
    // Treating '?' and '#' as terminators prevents a token carried in a query
    // string (`https://host?token=SECRET`) from slipping through unredacted.
    if let Some(scheme_end) = url.find("://") {
        let after = scheme_end + 3;
        let origin_end = url[after..]
            .find(['/', '?', '#'])
            .map(|i| after + i)
            .unwrap_or(url.len());
        // If the authority carries HTTP basic-auth userinfo (`user:pass@host`),
        // the masked value would otherwise LEAK the credentials to the client.
        // Drop up to and including the last '@' so only `host[:port]` survives.
        let authority = &url[after..origin_end];
        let host_start = match authority.rfind('@') {
            Some(at) => after + at + 1,
            None => after,
        };
        return format!(
            "{}{}/{}",
            &url[..after],
            &url[host_start..origin_end],
            SECRET_SENTINEL
        );
    }
    // No scheme — nothing identifiable to preserve; fully mask.
    SECRET_SENTINEL.to_string()
}

// Mask a webhook URL, appending a stable `#<idx>` (its index in
// `webhook_urls`) so POST resolves by identity, not origin — two hooks
// sharing an origin would otherwise mask identically and collide.
pub(crate) fn mask_webhook_url_indexed(url: &str, idx: usize) -> String {
    format!("{}#{idx}", mask_webhook_url(url))
}

// True if `s` is a redacted placeholder from `mask_webhook_url[_indexed]`:
// ends with the sentinel, or `********#<digits>`. Strict on purpose — a URL
// that merely embeds the sentinel mid-path still gets validated.
fn is_masked_webhook(s: &str) -> bool {
    if s.ends_with(SECRET_SENTINEL) {
        return true;
    }
    if let Some((head, idx)) = s.rsplit_once('#') {
        return head.ends_with(SECRET_SENTINEL)
            && !idx.is_empty()
            && idx.bytes().all(|b| b.is_ascii_digit());
    }
    false
}

// One webhook as it arrives on POST /api/settings: a (possibly masked) URL
// plus its per-event flags.
type IncomingWebhook = WebhookEntry;

// Resolve an incoming webhook_urls array against stored entries, unmasking each placeholder by
// stable #idx (falling back to origin) — never by array position, since rows can be reordered.
pub(crate) fn resolve_webhook_entries(
    incoming: &[IncomingWebhook],
    existing: &[WebhookEntry],
) -> Result<Vec<WebhookEntry>, String> {
    let mut resolved: Vec<WebhookEntry> = Vec::with_capacity(incoming.len());
    for hook in incoming {
        let s = hook.url.as_str();
        // Resolve the URL only; the flags are always taken from `incoming`.
        let url = if is_masked_webhook(s) {
            // Preferred: resolve by the stable `#<idx>` identifier so two
            // same-origin webhooks round-trip unambiguously. The index must
            // be in range AND still mask to this placeholder, else reject.
            if let Some((origin_mask, idx_str)) = s.rsplit_once('#')
                && let Ok(idx) = idx_str.parse::<usize>()
            {
                match existing.get(idx) {
                    Some(stored) if mask_webhook_url(&stored.url) == origin_mask => {
                        stored.url.clone()
                    }
                    // Index stale (row deleted/reordered) — reject rather
                    // than guess.
                    _ => return Err(s.to_string()),
                }
            } else {
                // Fallback: no embedded index (older client). Match by origin;
                // only unambiguous when exactly one stored URL shares the origin.
                let matches: Vec<&WebhookEntry> = existing
                    .iter()
                    .filter(|stored| mask_webhook_url(&stored.url) == s)
                    .collect();
                match matches.as_slice() {
                    [one] => one.url.clone(),
                    _ => return Err(s.to_string()),
                }
            }
        } else {
            s.to_string()
        };
        if url.trim().is_empty() {
            continue;
        }
        resolved.push(WebhookEntry {
            url: url.clone(),
            post_rip: hook.post_rip,
            post_mux: hook.post_mux,
            post_move: hook.post_move,
            headers: hook
                .headers
                .iter()
                .map(|(name, value)| {
                    let value = if value == SECRET_SENTINEL {
                        // Bind the saved key to its exact saved URL, never a new host.
                        let matches: Vec<_> = existing.iter().filter(|e| e.url == url).collect();
                        let stored = if let Some((_, idx)) = s.rsplit_once('#') {
                            idx.parse::<usize>()
                                .ok()
                                .and_then(|i| existing.get(i))
                                .filter(|e| e.url == url)
                        } else if matches.len() == 1 {
                            Some(matches[0])
                        } else {
                            None
                        };
                        stored
                            .and_then(|entry| {
                                entry
                                    .headers
                                    .iter()
                                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                                    .map(|(_, value)| value.clone())
                            })
                            .ok_or_else(|| "Re-enter the webhook header value".to_string())?
                    } else {
                        value.clone()
                    };
                    Ok((name.clone(), value))
                })
                .collect::<Result<_, String>>()?,
        });
    }
    Ok(resolved)
}

// Serialize Config for GET /api/settings with credential fields redacted:
// no route is authenticated and the server binds 0.0.0.0, so cleartext
// keyserver_secret/tmdb_api_key would hand any LAN client the operator's key.
fn settings_json_redacted(c: &Config) -> String {
    crate::server::settings_schema::redacted(c).to_string()
}

// Cap on a request body read fully into memory. POST bodies are small JSON
// (settings/title/review/debug); without this cap an unauthenticated LAN
// client could stream a multi-GB body and OOM the container (DoS).
const MAX_REQUEST_BODY: u64 = 1024 * 1024;

/// Outcome of [`read_body_capped`].
enum BodyRead {
    /// Body read successfully, within the cap.
    Ok(String),
    /// The reader errored before EOF (truncated/disconnected client).
    Err,
    /// The body exceeded `MAX_REQUEST_BODY` before EOF.
    TooLarge,
}

// Read a body into a String, capped at MAX_REQUEST_BODY + 1 bytes: the
// extra byte lets an exactly-at-limit body pass while detecting oversize.
// Content-Length is never trusted; `take` bounds actual bytes read.
fn read_body_capped(request: &mut tiny_http::Request) -> BodyRead {
    let mut body = String::new();
    match request
        .as_reader()
        .take(MAX_REQUEST_BODY + 1)
        .read_to_string(&mut body)
    {
        Ok(_) => {
            if body.len() as u64 > MAX_REQUEST_BODY {
                BodyRead::TooLarge
            } else {
                BodyRead::Ok(body)
            }
        }
        Err(_) => BodyRead::Err,
    }
}

/// Read a JSON POST body with the shared size cap, replying with the
/// appropriate error status (400 bad body / 413 too large) on failure.
/// Returns `None` once a response has already been sent.
pub(crate) fn read_json_body(
    mut request: tiny_http::Request,
) -> Result<(tiny_http::Request, String), ()> {
    match read_body_capped(&mut request) {
        BodyRead::Ok(body) => Ok((request, body)),
        BodyRead::Err => {
            json_response(request, 400, r#"{"ok":false,"error":"bad body"}"#);
            Err(())
        }
        BodyRead::TooLarge => {
            json_response(
                request,
                413,
                r#"{"ok":false,"error":"request body too large"}"#,
            );
            Err(())
        }
    }
}

// Validate device names (sgN/diskN/CdRomN or ioreg:<digits>): reject
// slashes/traversal so a malformed URL like /api/rip/sg4/stop can't reach
// the rip handler with device="sg4/stop" (previously spawned a doomed thread).
pub(crate) fn is_valid_device_name(s: &str) -> bool {
    // Cross-OS device key (Linux `sgN`, macOS `diskN`, Windows `CdRomN`).
    // Alphanumeric names and numeric registry selectors form the boundary rejecting
    // separators/traversal (`sg4/stop`); not a "this drive exists" check.
    if let Some(id) = s.strip_prefix("ioreg:") {
        return !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && id.parse::<u64>().is_ok();
    }
    (3..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn device_path(device: &str) -> String {
    if device.starts_with("ioreg:") {
        device.to_string()
    } else if cfg!(windows) {
        format!(r"\\.\{device}")
    } else {
        format!("/dev/{device}")
    }
}

// media_type on a title override is the one field handle_title_override left
// unclamped/unallow-listed; the router only acts on "tv" vs everything else
// (and TMDB only ever yields these two), so allow-listing is the honest bound.
fn normalize_media_type(raw: &str) -> String {
    match raw.trim().to_ascii_lowercase().as_str() {
        "tv" => "tv".to_string(),
        _ => "movie".to_string(),
    }
}

// Clamp `s` to `max` Unicode scalar values without splitting a multi-byte
// char; bounds operator-supplied free text (title/overview/poster_url)
// before it's persisted and re-broadcast.
fn clamp_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}

// The three "could not find out" failure strings, as opposed to "this URL is not allowed"; kept
// as constants so is_transient_resolve_error can classify without duplicated literals.
pub(crate) const RESOLVE_TIMEOUT_MSG: &str = "DNS resolution timed out";
pub(crate) const RESOLVE_FAILED_PREFIX: &str = "could not resolve host: ";
pub(crate) const RESOLVE_NO_ADDRS_MSG: &str = "host did not resolve to any address";

// True when the error means the host could not be looked up right now (DNS blip), not a
// permanent verdict on the URL — a resolver blip is not evidence the remote service is down.
pub(crate) fn is_transient_resolve_error(msg: &str) -> bool {
    msg == RESOLVE_TIMEOUT_MSG
        || msg == RESOLVE_NO_ADDRS_MSG
        || msg.starts_with(RESOLVE_FAILED_PREFIX)
}

// Resolve host:port with a bounded deadline: ToSocketAddrs blocks and can hang for the OS
// resolver timeout, freezing the calling handler thread. Runs on a spawned thread, joined with
// a short deadline.
pub(crate) fn resolve_with_timeout(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    use std::sync::mpsc;
    use std::time::Duration;
    const DNS_TIMEOUT: Duration = Duration::from_secs(4);
    // A timed-out resolver thread can't be cancelled — it lingers until
    // `to_socket_addrs` returns. Cap detached resolvers in flight so repeated
    // timeouts can't leak them unboundedly; at the cap, fail fast instead.
    const MAX_INFLIGHT: usize = 8;
    static INFLIGHT: AtomicUsize = AtomicUsize::new(0);

    // RAII admission token: moved into the resolver closure so the slot is
    // held until the (possibly detached) worker returns; if `thread::spawn`
    // itself unwinds, the guard drops here instead, so it never leaks.
    let guard = match ConnGuard::try_acquire(&INFLIGHT, MAX_INFLIGHT) {
        Some(g) => g,
        None => return Err(RESOLVE_TIMEOUT_MSG.to_string()),
    };

    let host = host.to_string();
    // Bounded channel of capacity 1: the resolver's single send never blocks,
    // so the thread always exits cleanly even if the receiver has already
    // timed out and gone away.
    let (tx, rx) = mpsc::sync_channel::<Result<Vec<SocketAddr>, std::io::Error>>(1);
    // A refused thread (pid/thread exhaustion) is a resolve failure, not a
    // panic; the closure, and the guard in it, drop on the Err.
    if let Err(e) = std::thread::Builder::new().spawn(move || {
        let _g = guard;
        let res = (host.as_str(), port)
            .to_socket_addrs()
            .map(|it| it.collect::<Vec<SocketAddr>>());
        // Receiver may be gone after the timeout — ignore the send error.
        let _ = tx.send(res);
    }) {
        return Err(format!("{RESOLVE_FAILED_PREFIX}{e}"));
    }
    match rx.recv_timeout(DNS_TIMEOUT) {
        Ok(Ok(addrs)) => Ok(addrs),
        Ok(Err(e)) => Err(format!("{RESOLVE_FAILED_PREFIX}{e}")),
        Err(_) => Err(RESOLVE_TIMEOUT_MSG.to_string()),
    }
}

// Validate an operator-supplied fetch/POST URL: requires http(s), resolves the host once, and
// rejects only addresses that can never be reached (libfreemkv's rule; LAN is allowed). Returns resolved sockets so the caller
// can pin the connection.
pub(crate) fn validate_fetch_url(url: &str) -> Result<Vec<SocketAddr>, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("URL is empty".to_string());
    }
    // Minimal scheme + authority parse — no URL crate dep, mirroring the
    // hand-rolled parsers already in this module.
    let rest = if let Some(r) = url.strip_prefix("https://") {
        (r, 443u16)
    } else if let Some(r) = url.strip_prefix("http://") {
        (r, 80u16)
    } else {
        return Err("URL must start with http:// or https://".to_string());
    };
    let (authority, default_port) = rest;
    // Strip path/query/fragment — keep only the authority (host[:port]).
    let authority = authority.split(['/', '?', '#']).next().unwrap_or(authority);
    // Strip userinfo if present (user:pass@host).
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return Err("URL has no host".to_string());
    }
    // Split host:port, handling bracketed IPv6 literals [::1]:8080.
    let (host, port): (String, u16) = if let Some(stripped) = authority.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, after)) => {
                let p = after
                    .strip_prefix(':')
                    .map(|s| s.parse::<u16>().map_err(|_| "invalid port".to_string()))
                    .transpose()?
                    .unwrap_or(default_port);
                (h.to_string(), p)
            }
            None => return Err("malformed IPv6 host".to_string()),
        }
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        // Only treat the trailing ':' as a port separator if the right
        // side is numeric (avoids mis-splitting a bare IPv6 literal,
        // though those should be bracketed).
        match p.parse::<u16>() {
            Ok(p) => (h.to_string(), p),
            Err(_) => (authority.to_string(), default_port),
        }
    } else {
        (authority.to_string(), default_port)
    };
    if host.is_empty() {
        return Err("URL has no host".to_string());
    }

    // Resolve once, with a bounded deadline (see resolve_with_timeout).
    let addrs: Vec<SocketAddr> = resolve_with_timeout(&host, port)?;
    if addrs.is_empty() {
        return Err(RESOLVE_NO_ADDRS_MSG.to_string());
    }
    for a in &addrs {
        if libfreemkv::mux::is_blocked_ip(a.ip()) {
            return Err(format!("refusing to connect to invalid address {}", a.ip()));
        }
    }
    Ok(addrs)
}

// Validate an operator network output target. A bare host:port (no scheme);
// libfreemkv streams to it, so its own rule decides: LAN and loopback are
// fine, only addresses that can never be a peer are refused.
pub(crate) fn validate_network_target(target: &str) -> Result<(), String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("network target is empty".to_string());
    }
    // Split host:port, handling bracketed IPv6 literals [::1]:9000.
    let (host, port): (String, u16) = if let Some(stripped) = target.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, after)) => {
                let p = after
                    .strip_prefix(':')
                    .ok_or_else(|| "network target needs a port (host:port)".to_string())?
                    .parse::<u16>()
                    .map_err(|_| "invalid port".to_string())?;
                (h.to_string(), p)
            }
            None => return Err("malformed IPv6 host".to_string()),
        }
    } else {
        let (h, p) = target
            .rsplit_once(':')
            .ok_or_else(|| "network target needs a port (host:port)".to_string())?;
        let p = p.parse::<u16>().map_err(|_| "invalid port".to_string())?;
        (h.to_string(), p)
    };
    if host.is_empty() {
        return Err("network target has no host".to_string());
    }

    // Bounded DNS — same shared helper validate_fetch_url uses, so an
    // unauthenticated settings POST can't freeze the handler on a slow resolver.
    let addrs: Vec<SocketAddr> = resolve_with_timeout(&host, port)?;
    if addrs.is_empty() {
        return Err(RESOLVE_NO_ADDRS_MSG.to_string());
    }
    for a in &addrs {
        if libfreemkv::mux::is_blocked_ip(a.ip()) {
            return Err(format!("refusing to stream to invalid address {}", a.ip()));
        }
    }
    Ok(())
}

// Cap on pinned addresses: ureq's fixed 16-slot ResolvedSocketAddrs panics (out-of-bounds) on a
// 17th push, so cap to the first 16 validate_fetch_url already vetted.
const MAX_PINNED_ADDRS: usize = 16;

// The addresses a resolve may actually hand back, capped at MAX_PINNED_ADDRS. Separated from
// the Resolver impl so the cap is testable — ureq's resolver types aren't nameable outside the
// crate.
fn pinned_addrs(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    addrs.iter().copied().take(MAX_PINNED_ADDRS).collect()
}

// Pinned-address resolver behind guarded_agent_with_timeouts. Must be wired via
// Agent::with_parts — Agent::new_with_config compiles but silently uses the default
// (re-resolving) resolver.
#[derive(Debug)]
struct PinnedResolver(Vec<SocketAddr>);

impl ureq::unversioned::resolver::Resolver for PinnedResolver {
    fn resolve(
        &self,
        _uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        _timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addrs = pinned_addrs(&self.0);
        if addrs.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        let mut out = self.empty();
        for addr in addrs {
            out.push(addr);
        }
        Ok(out)
    }
}

// A short, URL-FREE description of a ureq failure: these summaries reach
// syslog/autorip.jsonl/unauthenticated endpoints, so each variant maps to a fixed label instead
// of ever formatting the error.
pub(crate) fn ureq_error_kind(e: &ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(code) => format!("HTTP {code}"),
        ureq::Error::Io(io) => match io.raw_os_error() {
            // An OS-generated error (has an errno): its Display is the
            // syscall's own message, derived purely from the errno and never
            // the URL — surface it instead of the useless generic label.
            Some(_) => format!("io: {io}"),
            // No errno — a ureq/std-synthesized io error whose payload we do
            // NOT trust to be URL-free. Fall back to the ErrorKind's fixed
            // description, which is a constant string and never the URL.
            None => format!("io: {}", io.kind()),
        },
        ureq::Error::Timeout(_) => "timeout".to_string(),
        ureq::Error::HostNotFound => "host not found".to_string(),
        ureq::Error::ConnectionFailed => "connection failed".to_string(),
        ureq::Error::TooManyRedirects => "too many redirects".to_string(),
        ureq::Error::Tls(_) => "tls error".to_string(),
        ureq::Error::BodyExceedsLimit(_) => "body exceeds limit".to_string(),
        _ => "transport error".to_string(),
    }
}

// Rolling stall detector (re-armed on every read that returns bytes): no progress for this long
// means the peer is dead. Replaces the ureq 2 timeout_read knob the 2→3 migration dropped.
pub(crate) const STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

// Chained after DefaultConnector to re-arm a ROLLING per-read idle bound on every body read,
// restoring the stall detection ureq 3.4.1 removed (#1194).
#[derive(Debug)]
struct IdleReCapConnector {
    idle: std::time::Duration,
}

impl<In: ureq::unversioned::transport::Transport> ureq::unversioned::transport::Connector<In>
    for IdleReCapConnector
{
    type Out = IdleReCapTransport<In>;

    fn connect(
        &self,
        _details: &ureq::unversioned::transport::ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| IdleReCapTransport {
            inner,
            idle: self.idle,
        }))
    }
}

#[derive(Debug)]
struct IdleReCapTransport<In> {
    inner: In,
    idle: std::time::Duration,
}

impl<In> IdleReCapTransport<In> {
    // Cap body reads at the idle bound. Global/PerCall mask the phase (ureq reports the earliest
    // deadline), so they are capped too: any caller setting timeout_global/timeout_per_call on a
    // guarded agent also gets its header wait clamped to idle. min keeps the tighter bound.
    fn cap(
        &self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> ureq::unversioned::transport::NextTimeout {
        use ureq::unversioned::transport::time::Duration as UreqDuration;
        if !matches!(
            timeout.reason,
            ureq::Timeout::RecvBody | ureq::Timeout::Global | ureq::Timeout::PerCall
        ) {
            return timeout;
        }
        let idle = UreqDuration::from_millis(self.idle.as_millis() as u64);
        let after = if timeout.after < idle {
            timeout.after
        } else {
            idle
        };
        ureq::unversioned::transport::NextTimeout {
            after,
            reason: timeout.reason,
        }
    }
}

impl<In: ureq::unversioned::transport::Transport> ureq::unversioned::transport::Transport
    for IdleReCapTransport<In>
{
    fn buffers(&mut self) -> &mut dyn ureq::unversioned::transport::Buffers {
        self.inner.buffers()
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(
        &mut self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<bool, ureq::Error> {
        let capped = self.cap(timeout);
        self.inner.await_input(capped)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

// Build the ONE DNS-pinned, redirect-blocking ureq agent, with caller-chosen
// connect/response/idle timeouts (ureq sets no defaults, so an unresponsive peer would
// otherwise block the thread forever).
pub(crate) fn guarded_agent_with_timeouts(
    pinned: Vec<SocketAddr>,
    connect: std::time::Duration,
    response: std::time::Duration,
    idle: std::time::Duration,
) -> ureq::Agent {
    // response bounds header arrival and (as timeout_recv_body) the TOTAL body transfer; idle
    // is the rolling stall bound, layered on by IdleReCapConnector since ureq 3 dropped it.
    let config = ureq::config::Config::builder()
        .max_redirects(0)
        .timeout_connect(Some(connect))
        .timeout_recv_response(Some(response))
        .timeout_recv_body(Some(response))
        .build();
    // `with_parts`, never `new_with_config` — see [`PinnedResolver`].
    // DefaultConnector opens the (TLS) socket; IdleReCapConnector wraps its
    // transport to re-arm the rolling idle bound on every body read.
    use ureq::unversioned::transport::Connector as _;
    ureq::Agent::with_parts(
        config,
        ureq::unversioned::transport::DefaultConnector::new().chain(IdleReCapConnector { idle }),
        PinnedResolver(pinned),
    )
}

// Agent for webhook delivery: a plain outbound POST with the standard resolver, deliberately
// NOT SSRF-guarded — a webhook targeting a LAN service (Home Assistant, a NAS) is the intended
// use.
pub(crate) fn webhook_agent() -> ureq::Agent {
    let config = ureq::config::Config::builder()
        .max_redirects(0)
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_body(Some(STALL_TIMEOUT))
        .build();
    ureq::Agent::new_with_config(config)
}

// SSRF-guarded HTTP GET: the single entry point for fetching an operator- supplied URL, instead
// of ureq::get directly (which bypasses the guard). `pub` for the lib facade re-export; only
// bin/tests call it.
pub fn guarded_get(url: &str) -> Result<ureq::http::Response<ureq::Body>, String> {
    guarded_get_within(url, KEYDB_TRANSFER_BUDGET)
}

// End-to-end ceiling on the unauthenticated /api KEYDB update; tighter than
// KEYDB_TRANSFER_BUDGET because this path holds an in-flight handler slot and the update flag
// that 429s everyone else.
pub(crate) const KEYDB_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

// The /api/update-keydb fetch: guarded agent plus a request-level end-to-end ceiling, which
// keeps this LAN-facing path at KEYDB_FETCH_TIMEOUT rather than KEYDB_TRANSFER_BUDGET.
fn keydb_update_call(
    pinned: Vec<SocketAddr>,
    url: &str,
    ceiling: std::time::Duration,
    idle: std::time::Duration,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    guarded_agent_with_timeouts(pinned, std::time::Duration::from_secs(5), ceiling, idle)
        .get(url)
        .config()
        .timeout_global(Some(ceiling))
        .build()
        .call()
}

// How long a KEYDB body may take IN TOTAL once headers are in — sized to a real single-digit-MB
// keydb export, not to KEYDB_MAX_BYTES's defensive 100 MiB DoS cap.
pub(crate) const KEYDB_TRANSFER_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// [`guarded_get`] with an explicit total-transfer budget.
pub(crate) fn guarded_get_within(
    url: &str,
    budget: std::time::Duration,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let pinned = validate_fetch_url(url)?;
    guarded_agent_with_timeouts(
        pinned,
        std::time::Duration::from_secs(5),
        budget,
        STALL_TIMEOUT,
    )
    .get(url)
    .call()
    // Do NOT embed `e` directly: ureq 2's Display carried the request URL,
    // leaking a token-bearing keydb_url to the system log. ureq 3 is
    // URL-free here, but `BadUri` still prints it — keep masking.
    .map_err(|e| format!("fetch failed: {}", ureq_error_kind(&e)))
}

// ── Connection caps: run() spawns one OS thread per connection and /events holds its thread
// until disconnect, so without a cap a LAN client can pin N threads and exhaust the container.
const MAX_INFLIGHT_HANDLERS: usize = 64;

// Lower than MAX_INFLIGHT_HANDLERS on purpose: the gap is reserved for bodyless requests so
// stalled POSTs can never starve the healthcheck (GET /api/state).
const MAX_INFLIGHT_BODY_HANDLERS: usize = 48;

// The gap is what the healthcheck survives on, checked at compile time since equalising the
// caps cannot even build.
const _: () = assert!(MAX_INFLIGHT_BODY_HANDLERS < MAX_INFLIGHT_HANDLERS);

// Whether a request will make its handler read a body off the socket; read from headers
// tiny_http already parsed, so this is free.
fn carries_body(request: &tiny_http::Request) -> bool {
    match request.body_length() {
        Some(0) => false,
        Some(_) => true,
        // No Content-Length. For a method that never has a body this is just
        // an ordinary GET; for anything else, assume the reader will wait.
        None => !matches!(
            request.method(),
            tiny_http::Method::Get | tiny_http::Method::Head | tiny_http::Method::Options
        ),
    }
}

// Max concurrent SSE (/events) streams; each pins a thread for its whole
// lifetime, so this is the tighter bound.
const MAX_SSE_CLIENTS: usize = 8;

static INFLIGHT_HANDLERS: AtomicUsize = AtomicUsize::new(0);
// Open /events streams, oldest first, each with a stop flag. A client that
// vanished can leave its stream blocked, so a new one over the cap evicts the
// oldest, and every stream ends after SSE_MAX_LIFETIME (the browser reconnects).
static SSE_STREAMS: std::sync::Mutex<std::collections::VecDeque<(u64, Arc<AtomicBool>)>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());
static SSE_NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
const SSE_MAX_LIFETIME: std::time::Duration = std::time::Duration::from_secs(600);

// Admit a stream, evicting the oldest past the cap. Returns its id and stop flag.
fn sse_admit() -> (u64, Arc<AtomicBool>) {
    let id = SSE_NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let stop = Arc::new(AtomicBool::new(false));
    let mut open = SSE_STREAMS.lock().unwrap_or_else(|e| e.into_inner());
    while open.len() >= MAX_SSE_CLIENTS {
        if let Some((old, flag)) = open.pop_front() {
            flag.store(true, Ordering::SeqCst);
            tracing::info!(stream = old, "SSE cap reached: closing the oldest stream");
        }
    }
    open.push_back((id, stop.clone()));
    (id, stop)
}

fn sse_leave(id: u64) {
    SSE_STREAMS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(i, _)| *i != id);
}

// RAII admission token for a counted connection slot: decrements its counter
// on drop so the slot frees on any exit path (return, panic-unwind).
// try_acquire returns None when the cap is already reached.
struct ConnGuard(&'static AtomicUsize);

impl ConnGuard {
    fn try_acquire(counter: &'static AtomicUsize, max: usize) -> Option<ConnGuard> {
        // A CAS loop that only increments while under the cap, so the count can never
        // exceed `max`.
        let mut n = counter.load(Ordering::SeqCst);
        while n < max {
            match counter.compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(ConnGuard(counter)),
                Err(now) => n = now,
            }
        }
        None
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
#[path = "web_web_tests.rs"]
mod web_tests;

fn text_response(request: tiny_http::Request, body: &str) {
    let header =
        Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..]).unwrap();
    let response = Response::from_string(body).with_header(header).with_header(
        Header::from_bytes(
            &b"Cache-Control"[..],
            &b"no-store, no-cache, must-revalidate"[..],
        )
        .unwrap(),
    );
    let _ = request.respond(response);
}

// Cached result of build_queue_views, shared across every concurrent
// /events (SSE) client and /api/state poller — trades a small, bounded
// staleness window for turning N concurrent directory scans into one.
struct QueueViewSnapshot {
    computed_at: std::time::Instant,
    mux_queue: Vec<String>,
    move_queue: Vec<String>,
    mux_full: usize,
    move_full: usize,
}

impl QueueViewSnapshot {
    fn views(&self) -> (Vec<String>, Vec<String>, usize, usize) {
        (
            self.mux_queue.clone(),
            self.move_queue.clone(),
            self.mux_full,
            self.move_full,
        )
    }
}

struct QueueViewCache {
    // None only between a key's first scan starting and finishing — nothing to serve yet.
    snapshot: Option<QueueViewSnapshot>,
    // Single-flight marker: a timestamp (not a bool, which only the setter could clear) for
    // when the owning scan started; trusted for QUEUE_VIEW_REFRESH_DEADLINE.
    refresh_started: Option<std::time::Instant>,
    // Threads inside scan_queue_views for this key now; maintained by
    // RefreshGuard and capped at QUEUE_VIEW_MAX_REFRESHERS.
    refreshers: usize,
}

// RAII owner of a key's single-flight marker: Drop (not the happy path)
// releases it, so a scan that panics inside read_dir hands the key back
// instead of stranding it until the deadline.
struct RefreshGuard {
    key: String,
    claimed_at: std::time::Instant,
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        {
            let mut map = QUEUE_VIEW_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = map.get_mut(&self.key) {
                entry.refreshers = entry.refreshers.saturating_sub(1);
                // Only clear the marker if it is still OURS. A refresher that
                // was presumed dead and taken over must not clear the marker
                // of the live refresher that replaced it.
                if entry.refresh_started == Some(self.claimed_at) {
                    entry.refresh_started = None;
                }
            }
        }
        QUEUE_VIEW_REFRESHED.notify_all();
    }
}

// Keyed by staging_dir rather than a single slot, since the path can change at runtime and two
// different staging dirs must not evict each other's cached scan.
static QUEUE_VIEW_CACHE: Lazy<std::sync::Mutex<std::collections::HashMap<String, QueueViewCache>>> =
    Lazy::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

// Signalled when an in-flight scan completes; only the cold-start case (no
// snapshot at all) waits on it — any stale snapshot is served immediately.
static QUEUE_VIEW_REFRESHED: Lazy<std::sync::Condvar> = Lazy::new(std::sync::Condvar::new);

// Safety valve for the cold-start wait: gives up after this long rather than parking forever.
// Deliberately does NOT scan for itself (that abandons single-flight).
const QUEUE_VIEW_COLD_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

// How long the per-key single-flight marker is TRUSTED before a refresher is presumed dead
// (panicked/wedged) and the next caller may take it over.
const QUEUE_VIEW_REFRESH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

// Hard ceiling on threads inside scan_queue_views for ONE key: the original
// plus one retry, so a takeover that also wedges cannot pile up unboundedly.
const QUEUE_VIEW_MAX_REFRESHERS: usize = 2;

// How long a cached queue view is served before the next caller triggers a
// fresh scan; kept under the ~1s SSE tick so staleness stays invisible.
const QUEUE_VIEW_CACHE_TTL: std::time::Duration = std::time::Duration::from_millis(750);

// How long an idle key is RETAINED, far longer than QUEUE_VIEW_CACHE_TTL, so the Phase-3 prune
// doesn't evict a still-active key mid-stale-while-revalidate.
const QUEUE_VIEW_CACHE_RETAIN: std::time::Duration = std::time::Duration::from_secs(300);

// Test-only seam around the staging-dir scan a cache miss performs, keyed by staging dir so
// tests stay isolated. Lets a test slow one dir's scan and count how many scans it received.
#[cfg(test)]
#[path = "web_queue_scan_probe_tests.rs"]
pub(crate) mod queue_scan_probe;

/// How long an in-flight refresh marker for `dir` is trusted.
#[cfg(not(test))]
fn queue_view_refresh_deadline(_dir: &str) -> std::time::Duration {
    QUEUE_VIEW_REFRESH_DEADLINE
}

#[cfg(test)]
fn queue_view_refresh_deadline(dir: &str) -> std::time::Duration {
    match queue_scan_probe::overrides(dir).0 {
        0 => QUEUE_VIEW_REFRESH_DEADLINE,
        ms => std::time::Duration::from_millis(ms),
    }
}

/// How long a cold caller parks for someone else's first scan of `dir`.
#[cfg(not(test))]
fn queue_view_cold_wait(_dir: &str) -> std::time::Duration {
    QUEUE_VIEW_COLD_WAIT
}

#[cfg(test)]
fn queue_view_cold_wait(dir: &str) -> std::time::Duration {
    match queue_scan_probe::overrides(dir).1 {
        0 => QUEUE_VIEW_COLD_WAIT,
        ms => std::time::Duration::from_millis(ms),
    }
}

/// The scan a cache miss performs, behind a test-only instrumentation seam.
fn scan_queue_views(staging_dir: &str) -> (Vec<String>, Vec<String>, usize, usize) {
    #[cfg(test)]
    queue_scan_probe::enter(staging_dir);
    build_queue_views(staging_dir)
}

// build_queue_views, shared across callers within QUEUE_VIEW_CACHE_TTL, single-flighted with no
// lock held across the scan so a slow staging dir can't park /api/state (the HEALTHCHECK
// probe).
fn build_queue_views_cached(staging_dir: &str) -> (Vec<String>, Vec<String>, usize, usize) {
    // Phase 1 — decide, under the lock, whether THIS caller scans. The lock
    // is never held across `scan_queue_views`: a slow-to-enumerate staging
    // dir must not park `/api/state`, which `--healthcheck` probes.
    enum Decision {
        /// Serve this (possibly stale) snapshot; do not touch the disk.
        Serve(Vec<String>, Vec<String>, usize, usize),
        /// This caller owns the refresh.
        Scan,
        /// Cold key with a scan already in flight — wait for its result.
        Wait,
    }

    let deadline = queue_view_refresh_deadline(staging_dir);
    let cold_wait = queue_view_cold_wait(staging_dir);
    let mut map = QUEUE_VIEW_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let waited_from = std::time::Instant::now();
    let claimed_at = loop {
        let decision = match map.get_mut(staging_dir) {
            // Never seen: claim the slot and scan.
            None => Decision::Scan,
            Some(entry) => {
                // "A refresh is in flight" is a marker YOUNGER than the
                // deadline. Past it the owner is presumed dead (panicked or
                // wedged in `read_dir`), so its claim no longer justifies waiting.
                let live_refresh = entry
                    .refresh_started
                    .is_some_and(|t| t.elapsed() < deadline);
                // Serve a snapshot if fresh, OR stale with a live refresh in
                // flight — queueing behind someone else's I/O is the stall we're
                // avoiding, and a sub-second-stale view is invisible to a poller.
                let serve = entry
                    .snapshot
                    .as_ref()
                    .filter(|s| s.computed_at.elapsed() < QUEUE_VIEW_CACHE_TTL || live_refresh)
                    .map(|s| s.views());
                // The takeover a dead marker permits is itself capped: past
                // the max threads already inside `read_dir` for this key,
                // adding another only burns another HTTP worker.
                let may_scan = !live_refresh && entry.refreshers < QUEUE_VIEW_MAX_REFRESHERS;
                match serve {
                    Some((mux, mv, mux_full, move_full)) => {
                        Decision::Serve(mux, mv, mux_full, move_full)
                    }
                    // Stale (or cold) with no live refresher: take the key
                    // over, unless we are already at the refresher cap.
                    None if may_scan => Decision::Scan,
                    // Capped out but we have SOMETHING: never block a warm
                    // caller — hand back the stale view.
                    None => match entry.snapshot.as_ref().map(|s| s.views()) {
                        Some((mux, mv, mux_full, move_full)) => {
                            Decision::Serve(mux, mv, mux_full, move_full)
                        }
                        None => Decision::Wait,
                    },
                }
            }
        };
        match decision {
            Decision::Serve(mux, mv, mux_full, move_full) => {
                return (mux, mv, mux_full, move_full);
            }
            Decision::Scan => {
                let claimed_at = std::time::Instant::now();
                let entry = map
                    .entry(staging_dir.to_string())
                    .or_insert_with(|| QueueViewCache {
                        snapshot: None,
                        refresh_started: None,
                        refreshers: 0,
                    });
                entry.refresh_started = Some(claimed_at);
                entry.refreshers += 1;
                break claimed_at;
            }
            // Cold key, live scan in flight: wait for its result instead of
            // launching a duplicate one. The wait RELEASES the map lock, so
            // every other staging dir and warm reader keeps running.
            Decision::Wait => {
                if waited_from.elapsed() >= cold_wait {
                    // Give up on THIS call rather than start a competing scan:
                    // scanning anyway consumes an HTTP worker every `cold_wait`
                    // while wedged, which is how HEALTHCHECK restarts the daemon.
                    tracing::warn!(
                        staging_dir = %staging_dir,
                        waited_ms = waited_from.elapsed().as_millis() as u64,
                        "queue view still cold after waiting for an in-flight staging scan; \
                         serving an empty queue view for this request"
                    );
                    return (Vec::new(), Vec::new(), 0, 0);
                }
                let (m, _) = QUEUE_VIEW_REFRESHED
                    .wait_timeout(map, std::time::Duration::from_millis(50))
                    .unwrap_or_else(|e| e.into_inner());
                map = m;
            }
        }
    };
    drop(map);
    // Marker released on EVERY exit from here on, panic included.
    let _refresh_guard = RefreshGuard {
        key: staging_dir.to_string(),
        claimed_at,
    };

    // Phase 2 — scan with NO lock held.
    let (mux_queue, move_queue, mux_full, move_full) = scan_queue_views(staging_dir);

    // Phase 3 — publish. The single-flight marker is released by
    // `_refresh_guard` as this function returns.
    let mut map = QUEUE_VIEW_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    // Opportunistic prune so ever-changing staging paths (or a test suite's
    // distinct tempdirs) can't grow this map forever. Keep this key, anything
    // scanning, and any snapshot within RETAIN (not the serve TTL — see const).
    map.retain(|k, v| {
        k == staging_dir
            || v.refreshers > 0
            || v.snapshot
                .as_ref()
                .is_some_and(|s| s.computed_at.elapsed() < QUEUE_VIEW_CACHE_RETAIN)
    });
    let entry = map
        .entry(staging_dir.to_string())
        .or_insert_with(|| QueueViewCache {
            snapshot: None,
            refresh_started: None,
            refreshers: 0,
        });
    // A presumed-dead refresher that comes back to life must not overwrite the
    // fresher snapshot its replacement already published. Anything computed
    // after we started is at least as current as what we hold.
    let superseded = entry
        .snapshot
        .as_ref()
        .is_some_and(|s| s.computed_at > claimed_at);
    if !superseded {
        entry.snapshot = Some(QueueViewSnapshot {
            computed_at: std::time::Instant::now(),
            mux_queue: mux_queue.clone(),
            move_queue: move_queue.clone(),
            mux_full,
            move_full,
        });
    }
    drop(map);
    QUEUE_VIEW_REFRESHED.notify_all();
    (mux_queue, move_queue, mux_full, move_full)
}

fn get_state_json(staging_dir: &str) -> String {
    // Recover-and-proceed on poison, like every other STATE consumer. This
    // was the ONE site that bailed with `Err(_) => "{}"`, forever returning
    // a blank dashboard with a permanently green HEALTHCHECK; it's still readable.
    let state = ripper::STATE.lock().unwrap_or_else(|e| e.into_inner());
    // `_move` is now an ARRAY of per-artifact bars (movie file + companion ISO
    // get one each); clone the whole Vec (empty = nothing moving). Recover on
    // poison (MOVE_STATE convention) — `.ok()` would drop live bars process-wide.
    let move_state = crate::server::mover::MOVE_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // Mux progress rides on the synthetic `_mux` device key in STATE (a
    // RipState seeded by the mux worker), serialized as part of `state`.
    // There is no separate live MuxState struct.
    let mut obj = serde_json::to_value(&*state).unwrap_or_else(|_| serde_json::json!({}));
    if !move_state.is_empty() {
        obj["_move"] = serde_json::to_value(&move_state).unwrap_or_default();
    }
    // Release the STATE lock before the staging-dir scan below: it does
    // filesystem I/O, and holding STATE across it would serialize the
    // ripper's once-per-tick progress writes against this once-per-second scan.
    drop(state);
    // SINGLE-SOURCE STAGE VIEW (fix C): Mux/Move queues ride the SAME state
    // payload as the per-device tiles, pushed every SSE tick, so all three
    // views derive from one consistent snapshot — no more stale/disagreeing queues.
    let (mux_queue, move_queue, _, _) = build_queue_views_cached(staging_dir);
    obj["_mux_queue"] = serde_json::to_value(&mux_queue).unwrap_or_default();
    obj["_move_queue"] = serde_json::to_value(&move_queue).unwrap_or_default();
    obj.to_string()
}

// Cap on serialized queue entries so a pathological subdir count can't
// produce an unbounded response; shared with handle_system_info's "+N more"
// math so the list and its overflow count can never drift apart.
const QUEUE_DISPLAY_CAP: usize = 100;

// Builds the Mux-queue and Move-queue display lists, shared by get_state_json and
// handle_system_info so both derive from one place. Mutual exclusion comes from state.json
// itself.
fn build_queue_views(staging_dir: &str) -> (Vec<String>, Vec<String>, usize, usize) {
    let active_move_dir = crate::server::mover::ACTIVE_MOVE_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // Move queue: staging dirs handed off to the mover (`state == Done`),
    // minus the one actively being moved (shown as live bars, not a queue row).
    let staging_entries = std::fs::read_dir(staging_dir)
        .inspect_err(|e| {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %staging_dir, error = %e, "staging dir unreadable; queues show empty");
            }
        })
        .ok();
    let mut move_queue: Vec<String> = staging_entries
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path().is_dir()
                        && crate::server::ripper::staging::read_state(&e.path())
                            .map(|s| s.state == crate::server::ripper::staging::StagingState::Done)
                            .unwrap_or_else(|| e.path().join(".done").exists())
                })
                .filter(|e| {
                    active_move_dir.as_deref() != Some(e.file_name().to_string_lossy().as_ref())
                })
                .map(|e| {
                    let name = e.file_name().to_string_lossy().replace('_', " ");
                    format!("{} (moving)", name)
                })
                .collect()
        })
        .unwrap_or_default();
    // Mux queue: staging dirs with a `.ripped` hand-off and no terminal /
    // move-queue / in-flight marker (see `pending_queue`).
    let mut mux_queue = crate::server::muxer::pending_queue(std::path::Path::new(staging_dir));
    // Uncapped totals captured before truncation so "+N more" math shares this
    // one snapshot with the displayed lists.
    let move_full_count = move_queue.len();
    let mux_full_count = mux_queue.len();
    move_queue.truncate(QUEUE_DISPLAY_CAP);
    mux_queue.truncate(QUEUE_DISPLAY_CAP);
    (mux_queue, move_queue, mux_full_count, move_full_count)
}

// The last `n` lines of `text`, oldest first.
fn last_lines(text: &str, n: usize) -> String {
    let mut lines: Vec<&str> = text.lines().rev().take(n).collect();
    lines.reverse();
    lines.join("\n")
}

fn handle_system_info(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    // Degrade gracefully on a poisoned lock like every other handler; copy
    // the two paths out and DROP the read guard before the I/O below, or a
    // held RwLock would block a concurrent cfg.write() (Settings-save).
    let (staging_dir, syslog_path) = match cfg.read() {
        Ok(c) => (
            c.staging_dir.clone(),
            format!("{}/device_system.log", c.log_dir()),
        ),
        Err(_) => {
            return json_response(
                request,
                500,
                r#"{"ok":false,"error":"config lock poisoned"}"#,
            );
        }
    };

    // Move + Mux queue lists come from the SAME shared builder the live
    // /api/state SSE payload uses, so the System page and live dashboard
    // can never disagree, and the "+N more" math shares one scan snapshot.
    let (mux_queue, move_queue, mux_full_count, move_full_count) =
        build_queue_views_cached(&staging_dir);

    // Mover errors: stuck staging dirs the user needs to act on.
    let move_errors: Vec<crate::server::mover::MoverError> = crate::server::mover::MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();

    let truncation_count = move_full_count.saturating_sub(QUEUE_DISPLAY_CAP)
        + mux_full_count.saturating_sub(QUEUE_DISPLAY_CAP);
    let mux_errors: Vec<crate::server::muxer::MuxerError> = crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .cloned()
        .collect();

    // System log: last 50 lines, in file order. Tail from the end with a
    // bounded read rather than slurping the whole file — device_system.log is
    // never rotated and the System page polls this endpoint every few seconds.
    let syslog = match tail_file(&syslog_path, SYSLOG_TAIL_BYTES) {
        Ok(text) => last_lines(&text, 50),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(path = %syslog_path, error = %e, "system log unreadable");
            format!("(system log unreadable: {})", e.kind())
        }
    };

    let c = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    let body = serde_json::json!({
        "version_label": crate::server::VERSION_LABEL,
        "libfreemkv": libfreemkv::VERSION_LABEL,
        "mounts": crate::server::health::mounts(),
        "keys": key_status(&c),
        "drives": drive_summary(),
        "log_dir": c.log_dir(),
        "move_queue": move_queue,
        "move_errors": move_errors,
        "mux_queue": mux_queue,
        "mux_errors": mux_errors,
        "truncation_count": truncation_count,
        "syslog": syslog,
        // Current runtime debug-logging state, so the System-page toggle
        // reflects reality on load (POST /api/debug flips it).
        "debug_enabled": debug_enabled(),
        "staged_kept": staged_kept_json(),
    });

    json_response(request, 200, &body.to_string());
}

// The finished remuxes kept on local staging, waiting for the output folder.
fn staged_kept_json() -> serde_json::Value {
    let (count, bytes) = crate::server::library::get().map_or((0, 0), |l| l.queue.staged_total());
    serde_json::json!({
        "count": count,
        "bytes": bytes,
        "dir": crate::server::health::remux_stage_dir(),
    })
}

// Where keys come from and whether each source is usable, for the System page.
fn key_status(c: &Config) -> serde_json::Value {
    let path = crate::server::keysource::keydb_path(c);
    let meta = std::fs::metadata(&path).ok();
    let modified = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    serde_json::json!({
        "keydb_path": path,
        "keydb_present": meta.is_some(),
        "keydb_bytes": meta.as_ref().map(|m| m.len()),
        "keydb_modified": modified,
        "keydb_url_set": !c.keydb_url.trim().is_empty(),
        "keyserver_set": !c.keyserver_url.trim().is_empty(),
        "keyserver_host": crate::server::web::mask_webhook_url(c.keyserver_url.trim())
            .trim_end_matches(crate::server::settings_schema::SECRET_SENTINEL)
            .trim_end_matches('/')
            .to_string(),
        "key_source": c.key_source,
        "tmdb_set": !c.tmdb_api_key.is_empty(),
    })
}

// Every drive the ripper knows, in device order, without the bulky fields.
fn drive_summary() -> Vec<serde_json::Value> {
    let state = ripper::STATE.lock().unwrap_or_else(|e| e.into_inner());
    let mut devs: Vec<serde_json::Value> = state
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, s)| {
            serde_json::json!({
                "device": k,
                "status": s.status,
                "disc_present": s.disc_present,
                "disc": if s.tmdb_title.is_empty() { &s.disc_name } else { &s.tmdb_title },
                "format": s.disc_format,
                "key_status": s.key_status,
            })
        })
        .collect();
    devs.sort_by(|a, b| {
        natural_key(a["device"].as_str().unwrap_or(""))
            .cmp(&natural_key(b["device"].as_str().unwrap_or("")))
    });
    devs
}

// Sort key putting `sg2` before `sg10`: the name with its trailing number split off.
fn natural_key(name: &str) -> (&str, u64, &str) {
    let digits = name.bytes().rev().take_while(u8::is_ascii_digit).count();
    let (head, num) = name.split_at(name.len() - digits);
    (head, num.parse().unwrap_or(0), name)
}

// POST /api/system/keyserver-test: ask the keyserver whether it answers.
fn handle_keyserver_test(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let c = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    if c.keyserver_url.trim().is_empty() {
        return json_response(
            request,
            400,
            r#"{"ok":false,"error":"no keyserver URL is set"}"#,
        );
    }
    let r = crate::server::keysource::probe_online_reachability(&c);
    let reachable = matches!(r, crate::server::keysource::ServiceReachability::Answered);
    // The test ran either way: `reachable` is its answer, not a request error.
    json_response(
        request,
        200,
        &serde_json::json!({"reachable": reachable, "result": format!("{r:?}")}).to_string(),
    );
}

// Cap per file in the log bundle, so a runaway log cannot exhaust memory.
const BUNDLE_FILE_CAP: u64 = 8 * 1024 * 1024;

// Most files one bundle carries, so a log dir full of rotated files cannot
// grow the in-memory zip without bound; the rest are named in the notes entry.
const BUNDLE_MAX_FILES: usize = 64;

// Zip entry listing every file the bundle could not carry in full.
const BUNDLE_NOTES: &str = "bundle-notes.txt";

// Zip the tail of each file. A file that cannot be read or written, or that
// is past BUNDLE_MAX_FILES, is named in BUNDLE_NOTES so the bundle never
// looks complete when it is not.
fn build_log_bundle(files: Vec<(String, std::path::PathBuf)>) -> zip::result::ZipResult<Vec<u8>> {
    use std::io::Write as _;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let mut notes = String::new();
    for (i, (name, path)) in files.into_iter().enumerate() {
        if i >= BUNDLE_MAX_FILES {
            notes.push_str(&format!(
                "{name}: omitted (bundle holds {BUNDLE_MAX_FILES} files)\n"
            ));
            continue;
        }
        let text = match tail_file(&path.to_string_lossy(), BUNDLE_FILE_CAP) {
            Ok(t) => t,
            Err(e) => {
                notes.push_str(&format!("{name}: unreadable ({})\n", e.kind()));
                continue;
            }
        };
        let written = zip
            .start_file(name.as_str(), opts)
            .map_err(|e| e.to_string())
            .and_then(|()| zip.write_all(text.as_bytes()).map_err(|e| e.to_string()));
        if let Err(e) = written {
            notes.push_str(&format!("{name}: not written ({e})\n"));
        }
    }
    if !notes.is_empty() {
        zip.start_file(BUNDLE_NOTES, opts)?;
        zip.write_all(notes.as_bytes())?;
    }
    Ok(zip.finish()?.into_inner())
}

// GET /api/logs/download: every log as one zip, each file capped to its tail.
fn handle_logs_download(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let log_dir = cfg.read().unwrap_or_else(|e| e.into_inner()).log_dir();
    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut dirs = vec![(String::new(), std::path::PathBuf::from(&log_dir))];
    while let Some((prefix, dir)) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let name = format!("{prefix}{}", e.file_name().to_string_lossy());
            match e.file_type() {
                Ok(t) if t.is_dir() && prefix.is_empty() => {
                    dirs.push((format!("{name}/"), e.path()))
                }
                Ok(t) if t.is_file() => files.push((name, e.path())),
                _ => {}
            }
        }
    }
    let json_log = std::path::PathBuf::from(crate::server::observe::json_log_path());
    if !json_log.starts_with(&log_dir) {
        files.push(("events.jsonl".into(), json_log));
    }
    files.sort();
    let Ok(bytes) = build_log_bundle(files) else {
        return json_response(
            request,
            500,
            r#"{"ok":false,"error":"could not build the zip"}"#,
        );
    };
    let fname = format!(
        "attachment; filename=\"freemkv-logs-{}.zip\"",
        crate::server::util::format_iso_datetime_filename()
    );
    let response = Response::from_data(bytes)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/zip"[..]).unwrap())
        .with_header(Header::from_bytes(&b"Content-Disposition"[..], fname.as_bytes()).unwrap())
        .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap());
    let _ = request.respond(response);
}

fn handle_device_log(request: tiny_http::Request, device: &str) {
    // Single source of truth for device-name validation. Dispatch already
    // gates on is_valid_device_name; re-checking here closes any latent
    // bypass if the handler is ever called directly.
    if !is_valid_device_name(device) {
        text_response(request, "invalid device");
        return;
    }
    // `?since=N`: only the lines after sequence N, as JSON, so a viewer keeps
    // following the ring after it wraps. Without it, the plain-text tail.
    let since = request
        .url()
        .split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|kv| kv.strip_prefix("since=")))
        .and_then(|v| v.parse::<u64>().ok());
    if let Some(since) = since {
        let (seq, lines) = crate::server::log::get_device_log_since(device, since);
        return json_response(
            request,
            200,
            &serde_json::json!({ "seq": seq, "lines": lines }).to_string(),
        );
    }
    let lines = crate::server::log::get_device_log(device, 2000);
    text_response(request, &lines.join("\n"));
}

// Upper bound on trailing bytes read when tailing a log. The JSONL event
// log uses rolling::never so it grows unbounded; 8 MiB comfortably holds
// the 5000-line n cap while keeping per-request allocation bounded.
const DEBUG_TAIL_BYTES: u64 = 8 * 1024 * 1024;

// Same idea for the system log: 50 lines, generously bounded.
const SYSLOG_TAIL_BYTES: u64 = 256 * 1024;

// Read up to the last max_bytes of a file, seeking from the end rather than
// slurping the whole file; a partial first line from a mid-file seek is
// acceptable and dropped by callers that split on \n.
fn tail_file(path: &str, max_bytes: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let read_from = len.saturating_sub(max_bytes);
    let truncated = read_from > 0;
    f.seek(SeekFrom::Start(read_from))?;
    let mut buf = Vec::with_capacity(len.saturating_sub(read_from).min(max_bytes) as usize);
    f.take(max_bytes).read_to_end(&mut buf)?;
    let mut s = String::from_utf8_lossy(&buf).into_owned();
    // When we seeked into the middle of the file, the first line is a
    // partial record — drop it so callers never parse a half line.
    if truncated && let Some(nl) = s.find('\n') {
        s.drain(..=nl);
    }
    Ok(s)
}

// GET /api/debug?n=N&level=L&device=D&q=substr: last N JSONL events, tailed
// from autorip.jsonl with optional level/device/substring filters. Output
// is raw JSONL (not a JSON array) for streamability, used by the Debug tab.
fn handle_debug_log(request: tiny_http::Request, url: &str) {
    let params = parse_query(url);
    let n: usize = params
        .get("n")
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
        .min(5000);
    let level = params.get("level").map(|s| s.to_lowercase());
    // Validate the device filter with the same strict predicate as every other
    // device handler; ignore an invalid value rather than letting an arbitrary
    // attacker-supplied substring into the line filter.
    let device = params
        .get("device")
        .filter(|d| is_valid_device_name(d))
        .cloned();
    // Restrict the free-text grep filter to printable ASCII (0x20..=0x7E):
    // the JSONL we grep is ASCII-only, so this keeps an attacker from
    // smuggling control bytes or arbitrary Unicode into the line filter.
    let q = params
        .get("q")
        .filter(|s| s.bytes().all(|b| (0x20..=0x7E).contains(&b)))
        .cloned();

    let path = crate::server::observe::json_log_path();
    let content = match tail_file(&path, DEBUG_TAIL_BYTES) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The non-rolling jsonl file may not exist on a fresh boot
            // before the first event flushes. Return empty rather than 404
            // — UI can poll without alerting.
            tracing::debug!(path = %path, error = %e, "debug: jsonl missing");
            return text_response(request, "");
        }
        Err(e) => {
            tracing::warn!(path = %path, error = %e, "debug: jsonl unreadable");
            return json_response(
                request,
                500,
                r#"{"ok":false,"error":"could not read the debug log"}"#,
            );
        }
    };

    let levels_at_or_above = |min: &str| -> &'static [&'static str] {
        match min {
            "error" => &["ERROR"],
            "warn" => &["WARN", "ERROR"],
            "info" => &["INFO", "WARN", "ERROR"],
            "debug" => &["DEBUG", "INFO", "WARN", "ERROR"],
            _ => &["TRACE", "DEBUG", "INFO", "WARN", "ERROR"],
        }
    };

    // Filter first, then keep the last `n`: a device's lines must not be
    // crowded out of the window by other devices' lines.
    // tracing-subscriber JSON format puts the level in `"level":"INFO"`.
    let level_needles: Option<Vec<String>> = level.as_deref().map(|l| {
        levels_at_or_above(l)
            .iter()
            .map(|lv| format!("\"level\":\"{lv}\""))
            .collect()
    });
    // Match `"device":"sg4"` exactly to avoid `sg40` matching `sg4`.
    let device_needle = device.as_deref().map(|d| format!("\"device\":\"{d}\""));
    let mut out: Vec<&str> = Vec::new();
    for line in content.lines() {
        if let Some(ref needles) = level_needles
            && !needles.iter().any(|nd| line.contains(nd.as_str()))
        {
            continue;
        }
        if let Some(ref nd) = device_needle
            && !line.contains(nd.as_str())
        {
            continue;
        }
        if let Some(ref needle) = q
            && !line.contains(needle)
        {
            continue;
        }
        out.push(line);
    }
    out.drain(..out.len().saturating_sub(n));
    text_response(request, &out.join("\n"));
}

// Parse ?key=value&key2=v2 into a HashMap. Naive: percent-decodes but does
// NOT translate + to space and has no array-style keys — sufficient for our
// debug filters and easier to audit than a URL parser dep.
fn parse_query(url: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let q = match url.split_once('?') {
        Some((_, q)) => q,
        None => return map,
    };
    // Bound the work: cap the number of pairs and the length of each key/value
    // so a hostile query string can't blow up the HashMap or the per-request
    // allocation.
    const MAX_PAIRS: usize = 32;
    const MAX_FIELD_LEN: usize = 256;
    // Truncate a &str to at most `n` bytes on a char boundary (raw query
    // fields may carry multibyte UTF-8, so a blind byte slice could panic).
    fn clamp(s: &str, n: usize) -> &str {
        if s.len() <= n {
            return s;
        }
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
    // The cap applies to the DECODED field (an encoded multibyte char is 9
    // raw bytes); the raw field is bounded at 3x so decoding stays cheap.
    let decode = |raw: &str| {
        clamp(
            &percent_decode(clamp(raw, 3 * MAX_FIELD_LEN)),
            MAX_FIELD_LEN,
        )
        .to_string()
    };
    for pair in q.split('&').take(MAX_PAIRS) {
        if let Some((k, v)) = pair.split_once('=') {
            map.insert(decode(k), decode(v));
        }
    }
    map
}

#[cfg(test)]
#[path = "web_parse_query_tests.rs"]
mod parse_query_tests;

// Deadline for the bounded settings-save: above any reasonable NFS write
// latency, short enough a wedged /config doesn't block the API thread
// forever. On timeout we 503; write-then-rename keeps the old file intact.
const SETTINGS_SAVE_DEADLINE_SECS: u64 = 15;

fn handle_webhook_test(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let Ok((request, body)) = read_json_body(request) else {
        return;
    };
    let hook = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| WebhookEntry::parse(0, &v).ok());
    let Some(hook) = hook else {
        return json_response(
            request,
            400,
            r#"{"error":"Invalid webhook URL or API key"}"#,
        );
    };
    let existing = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .webhook_urls
        .clone();
    let hooks = resolve_webhook_entries(&[hook], &existing);
    let Ok(hooks) = hooks else {
        return json_response(
            request,
            400,
            r#"{"error":"Re-enter the webhook URL and API key"}"#,
        );
    };
    let Some(hook) = hooks.first() else {
        return json_response(request, 400, r#"{"error":"Enter a webhook URL"}"#);
    };
    match crate::server::webhook::test(hook) {
        Ok(status) => json_response(
            request,
            200,
            &serde_json::json!({"ok":true,"status":status}).to_string(),
        ),
        Err(error) => json_response(
            request,
            400,
            &serde_json::json!({"error":error}).to_string(),
        ),
    }
}

fn handle_settings_post(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    let (request, body) = match read_json_body(request) {
        Ok(rb) => rb,
        Err(()) => return,
    };
    let patch: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            json_response(request, 400, r#"{"ok":false,"error":"invalid json"}"#);
            return;
        }
    };

    // Validate the whole patch against the schema BEFORE taking the write
    // guard: URL checks resolve DNS, and a refused field must leave nothing
    // half-applied.
    let current = match cfg.read() {
        Ok(c) => c.clone(),
        Err(_) => {
            return json_response(
                request,
                500,
                r#"{"ok":false,"error":"config lock poisoned"}"#,
            );
        }
    };
    let parsed = match crate::server::settings_schema::parse_patch(&patch, &current) {
        Ok(p) => p,
        Err(e) => {
            return json_response(
                request,
                400,
                &serde_json::json!({"ok": false, "error": e}).to_string(),
            );
        }
    };

    // Mutate inside the write guard, then snapshot+drop it BEFORE the save
    // (fs I/O can hang on NFS). `save_gen` is taken under the guard so save
    // order follows mutation order.
    let save_gen: u64;
    let snapshot: Config = {
        let mut c = match cfg.write() {
            Ok(c) => c,
            Err(_) => {
                return json_response(
                    request,
                    500,
                    r#"{"ok":false,"error":"config lock poisoned"}"#,
                );
            }
        };
        crate::server::settings_schema::apply(&mut c, &parsed);
        save_gen = config::next_save_generation();
        c.clone()
    }; // <-- write guard dropped here; readers unblock immediately
    crate::server::library::wake();

    // Apply the decrypt-thread setting LIVE: swaps libfreemkv's rayon pool;
    // in-flight work uses the old pool, the next rip picks up the new size.
    config::apply_decrypt_threads(snapshot.decrypt_threads);

    // Fail-loud-EARLY destination check: warn NOW if a configured directory
    // is missing/unwritable, rather than only discovering the dead mount
    // hours later at move time. Non-blocking — the save still succeeds.
    for (root, reason) in crate::server::mover::check_configured_destinations(&snapshot) {
        crate::server::log::syslog(&format!(
            "WARNING: configured destination '{root}' is not usable: {reason}. \
             Rips will be PRESERVED in staging (not moved) until this is fixed."
        ));
    }

    // A save that fails puts the running config back, unless a newer save
    // changed it since; otherwise the page would say "nothing changed" while
    // the daemon ran on the unsaved values.
    let applied = serde_json::to_value(&snapshot).ok();
    let roll_back = || roll_back_settings(cfg, applied.as_ref(), &current);

    // Queue on the coalescing writer and await it with a deadline: a hung
    // write (NFS) parks only that writer, and later saves supersede it.
    let rx = match config::save_coalesced(snapshot, save_gen) {
        Ok(rx) => rx,
        Err(e) => {
            tracing::error!(
                target: "web",
                error = %e,
                "failed to spawn settings-save thread; on-disk settings.json unchanged"
            );
            roll_back();
            return json_response(
                request,
                500,
                r#"{"ok":false,"error":"settings save failed: could not spawn save thread"}"#,
            );
        }
    };
    match rx.recv_timeout(std::time::Duration::from_secs(SETTINGS_SAVE_DEADLINE_SECS)) {
        Ok(Ok(())) => json_response(request, 200, r#"{"ok":true}"#),
        Ok(Err(e)) => {
            tracing::error!(
                target: "web",
                error = %e,
                "settings save failed; on-disk settings.json unchanged"
            );
            roll_back();
            json_response(
                request,
                500,
                &serde_json::json!({
                    "ok": false,
                    "error": format!("settings.json could not be written ({e}); nothing was changed")
                })
                .to_string(),
            )
        }
        Err(_) => {
            tracing::error!(
                target: "web",
                "settings save timed out after {SETTINGS_SAVE_DEADLINE_SECS}s; \
                 in-memory config updated, on-disk result unknown (written only \
                 if storage recovers before autorip restarts)"
            );
            json_response(
                request,
                503,
                r#"{"ok":false,"error":"settings save timed out; on-disk result unknown, it is written only if storage recovers before autorip restarts"}"#,
            )
        }
    }
}

// Put `previous` back as the running config, unless a newer save moved it on
// from `applied` (the values the failed save installed). Runs after the save
// has failed, never while one is in flight.
fn roll_back_settings(
    cfg: &Arc<RwLock<Config>>,
    applied: Option<&serde_json::Value>,
    previous: &Config,
) {
    let mut c = cfg.write().unwrap_or_else(|e| e.into_inner());
    if serde_json::to_value(&*c).ok().as_ref() == applied {
        *c = previous.clone();
        config::apply_decrypt_threads(previous.decrypt_threads);
    }
}

fn handle_sse(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    // /events holds its thread for the whole client session (1s poll
    // loop). At most MAX_SSE_CLIENTS stay open: the oldest gives way.
    let (id, stop) = sse_admit();
    struct Leave(u64);
    impl Drop for Leave {
        fn drop(&mut self) {
            sse_leave(self.0);
        }
    }
    let _leave = Leave(id);
    let opened = std::time::Instant::now();
    // Same-origin only, matching every other route — no ACAO. The service
    // is unauthenticated, so a wildcard would let any page the operator
    // visits cross-origin subscribe and read the full RipState.
    let headers = vec![
        Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap(),
        Header::from_bytes(&b"Cache-Control"[..], &b"no-cache"[..]).unwrap(),
        Header::from_bytes(&b"Connection"[..], &b"keep-alive"[..]).unwrap(),
    ];

    let mut response = Response::empty(200);
    for h in headers {
        response = response.with_header(h);
    }

    let mut stream = request.upgrade("sse", response);

    // Re-read the staging dir each tick (cheap) so a Settings change to
    // the staging path is reflected without restarting the SSE stream.
    let staging_dir = || {
        cfg.read()
            .unwrap_or_else(|e| e.into_inner())
            .staging_dir
            .clone()
    };

    let initial = format!("data: {}\n\n", get_state_json(&staging_dir()));
    if stream.write_all(initial.as_bytes()).is_err() {
        return;
    }
    let _ = stream.flush();

    let mut library = crate::server::library::api::SseCursor::default();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if stop.load(Ordering::SeqCst) || opened.elapsed() > SSE_MAX_LIFETIME {
            break;
        }
        let mut frame = format!("data: {}\n\n", get_state_json(&staging_dir()));
        let dirs = crate::server::library::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner()));
        if let Some(lib) = crate::server::library::api::sse_frame(&mut library, Some(&dirs)) {
            frame.push_str(&lib);
        }
        if stream.write_all(frame.as_bytes()).is_err() {
            break;
        }
        if stream.flush().is_err() {
            break;
        }
    }
}

fn handle_scan(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, device: &str) {
    if !ripper::device_known(device) {
        return json_response(request, 404, UNKNOWN_DEVICE_BODY);
    }
    // Check-and-claim: `try_claim_active_checked` reads thread liveness
    // FIRST outside the STATE lock, then folds status-check+set into a
    // single STATE lock, closing the TOCTOU of two concurrent scan POSTs.
    let Some(claim_gen) = ripper::try_claim_active_checked(device, false) else {
        json_response(request, 409, r#"{"ok":false,"error":"busy"}"#);
        return;
    };

    ripper::release_stopped_disc(device);
    let dev = device.to_string();
    let dev_path = device_path(device);
    let cfg = Arc::clone(cfg);
    ripper::update_state(
        &dev,
        ripper::RipState {
            device: dev.clone(),
            status: "scanning".to_string(),
            disc_present: true,
            ..Default::default()
        },
    );
    let dev_for_register = dev.clone();
    if let Err(e) = ripper::spawn_rip_thread(&dev_for_register, "scan", move || {
        // Catch the panic, as the rip spawn site and poll loop do — without
        // it a panic in `scan_disc` leaves the claim standing forever
        // (`status="scanning"`, every route 409ing until restart).
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ripper::scan_disc(&cfg, &dev, &dev_path);
        }))
        .is_err()
        {
            crate::server::log::device_log(&dev, "Scan thread panicked");
            ripper::update_state(
                &dev,
                ripper::RipState {
                    device: dev.clone(),
                    status: "error".to_string(),
                    disc_present: true,
                    last_error: "Internal error (panic)".to_string(),
                    ..Default::default()
                },
            );
        }
    }) {
        tracing::error!(device = %dev_for_register, error = %e, "failed to spawn scan thread");
        // Roll the device state back to idle so a failed spawn doesn't wedge
        // the busy-check forever. Shared helper so poll loop + handlers can't drift.
        ripper::rollback_failed_spawn(&dev_for_register, claim_gen);
        json_response(
            request,
            500,
            r#"{"ok":false,"error":"thread spawn failed"}"#,
        );
        return;
    }
    json_response(request, 200, r#"{"ok":true}"#);
}

// POST /api/rip/{device}[?resume=yes|no] — the ONLY path that starts disk
// work. resume=yes re-muxes an existing staging ISO (404 if none); resume=no
// wipes staging first; no param does a fresh sweep+mux, keeping any staging dir.
fn handle_rip(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, device: &str, query: &str) {
    if !ripper::device_known(device) {
        return json_response(request, 404, UNKNOWN_DEVICE_BODY);
    }
    let resume_mode = parse_resume_param(query);

    // Check-and-claim — see `handle_scan` for the ordering. Closes the TOCTOU
    // of two concurrent rip POSTs. Also marks the device "scanning": resume
    // is decided by the worker itself, keeping scan logic in one place.
    let Some(claim_gen) = ripper::try_claim_active_checked(device, false) else {
        json_response(request, 409, r#"{"ok":false,"error":"already ripping"}"#);
        return;
    };
    ripper::release_stopped_disc(device);
    let _ = spawn_rip_after_claim(request, cfg, device, resume_mode, claim_gen);
}

// Spawn the rip worker for `device`, assuming the caller ALREADY won the
// claim. Shared by handle_rip and handle_accept_loss. Returns true if
// spawned, so accept_loss can disarm its pre-written marker if unused.
#[must_use]
fn spawn_rip_after_claim(
    request: tiny_http::Request,
    cfg: &Arc<RwLock<Config>>,
    device: &str,
    resume_mode: ResumeMode,
    claim_gen: u64,
) -> bool {
    let dev = device.to_string();
    let dev_path = device_path(device);
    let cfg = Arc::clone(cfg);

    let dev_for_register = dev.clone();
    ripper::register_halt(&dev_for_register, libfreemkv::Halt::new());
    if let Err(e) = ripper::spawn_rip_thread(&dev_for_register, "rip", move || {
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ripper::handle_rip_request(&cfg, &dev, &dev_path, resume_mode);
        }))
        .is_err()
        {
            crate::server::log::device_log(&dev, "Rip thread panicked");
            ripper::update_state(
                &dev,
                ripper::RipState {
                    device: dev.clone(),
                    status: "error".to_string(),
                    last_error: "Internal error (panic)".to_string(),
                    ..Default::default()
                },
            );
        }
        ripper::unregister_halt(&dev);
    }) {
        tracing::error!(device = %dev_for_register, error = %e, "failed to spawn rip thread");
        // Roll the device state back to idle so a failed spawn doesn't wedge
        // the busy-check forever. Shared helper so poll loop + handlers can't drift.
        ripper::rollback_failed_spawn(&dev_for_register, claim_gen);
        json_response(
            request,
            500,
            r#"{"ok":false,"error":"thread spawn failed"}"#,
        );
        return false;
    }

    json_response(request, 200, r#"{"ok":true}"#);
    true
}

// Entry verdict for handle_accept_loss's ownership gates, factored out so
// they're unit-testable without a live Request; reverting the .muxing 409
// guard flips MuxInProgress and fails the pinned test.
#[derive(Debug, PartialEq, Eq)]
enum AcceptLossEntry {
    /// No staging dir on disk for this device — 404.
    NoStagingDir,
    /// The mux worker owns the dir (`.muxing`). Refuse (409): this handler's
    /// lock-free state.json write would otherwise clobber the worker's terminal
    /// quarantine, silently dropping the operator's Accept.
    MuxInProgress,
    /// Present and unowned — proceed to arm the override.
    Proceed,
    /// Dir or .muxing state unreadable (not NotFound: EACCES, ESTALE) — 503.
    StagingUnreadable,
}

// Only NotFound is "gone" (metadata follows links, so a dangling symlink is
// 404); any other stat/read error, including on the .muxing state, fails
// closed as StagingUnreadable (503, retry).
fn accept_loss_entry_for(dir: &std::path::Path) -> AcceptLossEntry {
    match std::fs::metadata(dir) {
        Ok(_) => match crate::server::ripper::staging::muxing_status(dir) {
            Ok(muxing) => accept_loss_entry_verdict(true, muxing),
            Err(_) => AcceptLossEntry::StagingUnreadable,
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            accept_loss_entry_verdict(false, false)
        }
        Err(_) => AcceptLossEntry::StagingUnreadable,
    }
}

fn accept_loss_entry_verdict(dir_exists: bool, is_muxing: bool) -> AcceptLossEntry {
    if !dir_exists {
        AcceptLossEntry::NoStagingDir
    } else if is_muxing {
        AcceptLossEntry::MuxInProgress
    } else {
        AcceptLossEntry::Proceed
    }
}

// POST /api/accept-loss/{device}: accept a recorded over-threshold loss and
// deliver the existing rip. Writes .accept-loss, clears terminal/abort
// markers, then re-muxes the EXISTING ISO — fixing the exhaust->.failed loop.
fn handle_accept_loss(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>, device: &str) {
    // Resolve via the one naming rule, not the title alone: with a boxset
    // in the drive, Accept must arm the marker on THIS disc's dir, not a
    // sibling disc that happens to share its title.
    let staging = {
        let c = match cfg.read() {
            Ok(c) => c,
            Err(e) => e.into_inner(),
        };
        match ripper::staging_basename_for_device(&c, device) {
            Some(base) => c.staging_device_dir(&base),
            None => {
                drop(c);
                json_response(
                    request,
                    404,
                    r#"{"ok":false,"error":"no disc state for device"}"#,
                );
                return;
            }
        }
    };
    let dir = std::path::Path::new(&staging);
    // Ownership gates factored into pure accept_loss_entry_verdict above;
    // refusing on .muxing avoids clobbering a just-written quarantine, since
    // this handler isn't otherwise serialized against the mux worker's RMWs.
    match accept_loss_entry_for(dir) {
        AcceptLossEntry::StagingUnreadable => {
            json_response(
                request,
                503,
                r#"{"ok":false,"error":"staging dir unreadable; retry"}"#,
            );
            return;
        }
        AcceptLossEntry::NoStagingDir => {
            json_response(
                request,
                404,
                r#"{"ok":false,"error":"no staging dir to accept"}"#,
            );
            return;
        }
        AcceptLossEntry::MuxInProgress => {
            json_response(
                request,
                409,
                r#"{"ok":false,"error":"mux in progress; retry after it finishes"}"#,
            );
            return;
        }
        AcceptLossEntry::Proceed => {}
    }
    // Claim the device BEFORE touching any on-disk marker: a rejected 409
    // must leave the staging dir untouched, or the next legitimate rip
    // would silently mux a loss that was never actually accepted.
    let Some(claim_gen) = ripper::try_claim_active_checked(device, false) else {
        json_response(request, 409, r#"{"ok":false,"error":"already ripping"}"#);
        return;
    };
    // Arm the one-shot override and move the dir off its terminal/abort
    // state to the re-muxable hand-off state, so resume re-mux proceeds
    // instead of being refused as failed.
    ripper::staging::write_accept_loss_marker(dir);
    // The write is best-effort and only logs; a rip spawned without the armed
    // override would refuse the same loss again, so refuse the Accept instead.
    if !ripper::staging::accept_loss_requested(dir) {
        ripper::rollback_failed_spawn(device, claim_gen);
        crate::server::log::device_log(
            device,
            "Accept-damage failed: the override could not be saved to the staging dir.",
        );
        return json_response(
            request,
            500,
            r#"{"ok":false,"error":"could not save the accept-damage override (check staging permissions)"}"#,
        );
    }
    ripper::staging::mutate_state_if_present(dir, ripper::staging::apply_accept_loss_reopen);
    // Legacy pre-migration dirs: strip the marker files the same way.
    let _ = std::fs::remove_file(dir.join(ripper::staging::FAILED_MARKER));
    ripper::staging::clear_aborted_loss_marker(dir);
    ripper::staging::clear_restart_count(dir);
    crate::server::log::device_log(
        device,
        "Accept-damage requested — re-muxing the existing ISO with the loss override.",
    );
    // Delegate to the already-claimed spawn path (resume_remux consumes
    // `.accept-loss`); do NOT go through handle_rip, which would try to
    // claim a second time and always lose against the claim just above.
    if !spawn_rip_after_claim(request, cfg, device, ResumeMode::Require, claim_gen) {
        // The OS refused the thread, so NOTHING will consume the override
        // just armed — the same stale-override hazard the claim-before-write
        // ordering above prevents, reached by the other door. Disarm.
        ripper::staging::clear_accept_loss_marker(dir);
        crate::server::log::device_log(
            device,
            "Accept-damage override disarmed: the rip thread could not be spawned, so no run will consume it.",
        );
    }
}

/// Resume-mode chosen by the caller of `/api/rip`. The dispatch logic
/// in `ripper::handle_rip_request` reads this and routes to the
/// appropriate code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeMode {
    /// `?resume=yes` — require an existing resumable staging dir,
    /// fail if none.
    Require,
    /// `on_insert=resume` — Default's guards, then resume eligible state, else as `Fresh`.
    Prefer,
    /// `?resume=no` — operator clean slate: wipe any existing staging dir first.
    Wipe,
    /// no `resume=` query param — fresh sweep+mux in place, unless the disc's
    /// staging is finished, held, loss-aborted or mux-owned.
    Default,
    /// `on_insert=rip` — Default's guards, then wipe stale partial/failed staging and sweep.
    Fresh,
}

fn parse_resume_param(query: &str) -> ResumeMode {
    for kv in query.split('&') {
        let (k, v) = match kv.split_once('=') {
            Some((k, v)) => (k, v),
            None => (kv, ""),
        };
        if k == "resume" {
            return match v {
                "yes" | "true" | "1" => ResumeMode::Require,
                "no" | "false" | "0" => ResumeMode::Wipe,
                _ => ResumeMode::Default,
            };
        }
    }
    ResumeMode::Default
}

#[cfg(test)]
#[path = "web_stop_report_tests.rs"]
mod stop_report_tests;

#[cfg(test)]
#[path = "web_worker_panic_tests.rs"]
mod worker_panic_tests;

#[cfg(test)]
#[path = "web_accept_loss_spawn_failure_tests.rs"]
mod accept_loss_spawn_failure_tests;

#[cfg(test)]
#[path = "web_dashboard_button_tests.rs"]
mod dashboard_button_tests;

#[cfg(test)]
#[path = "web_resume_param_tests.rs"]
mod resume_param_tests;

/// Shared cap for all three KEYDB download paths (startup, daily refresh, web
/// handler). All paths use `read_capped_keydb_body` so overflow is detected
/// rather than silently truncating at the cap.
pub(crate) const KEYDB_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Why a capped keydb body read failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeydbReadError {
    /// The underlying reader errored.
    Io,
    /// The body exceeded the byte cap (oversized plain-text keydb).
    TooLarge,
}

// Read a keydb body, rejecting anything over max_bytes. Read::take(max)
// would silently truncate an oversized body into a half-valid file; reading
// one byte past the cap instead makes an oversized body detectable.
pub(crate) fn read_capped_keydb_body<R: std::io::Read>(
    reader: R,
    max_bytes: u64,
) -> std::result::Result<Vec<u8>, KeydbReadError> {
    let mut buf = Vec::new();
    reader
        .take(max_bytes + 1)
        .read_to_end(&mut buf)
        .map_err(|_| KeydbReadError::Io)?;
    if buf.len() as u64 > max_bytes {
        return Err(KeydbReadError::TooLarge);
    }
    Ok(buf)
}

// 413 body for an oversized plain-text keydb; the limit shown is the enforced cap.
fn keydb_too_large_body() -> String {
    format!(
        r#"{{"ok":false,"error":"KEYDB too large (>{} MiB plain-text); use a gzip/zip URL"}}"#,
        KEYDB_MAX_BYTES / (1024 * 1024)
    )
}

fn handle_update_keydb(request: tiny_http::Request, cfg: &Arc<RwLock<Config>>) {
    // Serialize: only one keydb download may be in flight at a time. Each one
    // buffers the whole file into memory, so concurrent unauthenticated calls
    // could allocate many large buffers at once. A second caller gets 429.
    static KEYDB_UPDATE_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    if KEYDB_UPDATE_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return json_response(
            request,
            429,
            r#"{"ok":false,"error":"A KEYDB update is already in progress."}"#,
        );
    }
    // Release the in-flight flag on every exit path.
    struct InFlightGuard;
    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            KEYDB_UPDATE_IN_FLIGHT.store(false, Ordering::Release);
        }
    }
    let _in_flight = InFlightGuard;

    let keydb_url = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .keydb_url
        .clone();
    if keydb_url.is_empty() {
        json_response(
            request,
            400,
            r#"{"ok":false,"error":"No KEYDB URL configured. Set it in Settings."}"#,
        );
        return;
    }

    // SSRF guard at fetch time (defence-in-depth on the store-time check):
    // resolve+validate once, then pin the connection to those IPs so DNS
    // rebinding can't redirect the fetch between validation and connect.
    let pinned = match validate_fetch_url(&keydb_url) {
        Ok(addrs) => addrs,
        Err(e) => {
            let msg = serde_json::json!({
                "ok": false,
                "error": format!("KEYDB URL rejected: {e}")
            })
            .to_string();
            json_response(request, 400, &msg);
            return;
        }
    };
    // Cap is the shared KEYDB_MAX_BYTES (100 MiB); read_capped_keydb_body
    // returns 413 on an oversized body rather than silently truncating.
    let keydb_cap = KEYDB_MAX_BYTES;

    // Download via ureq (supports HTTPS) then save via libfreemkv
    let body = match keydb_update_call(pinned, &keydb_url, KEYDB_FETCH_TIMEOUT, STALL_TIMEOUT) {
        Ok(resp) => match read_capped_keydb_body(resp.into_body().into_reader(), keydb_cap) {
            Ok(buf) => buf,
            Err(KeydbReadError::Io) => {
                json_response(
                    request,
                    500,
                    r#"{"ok":false,"error":"Failed to read response body."}"#,
                );
                return;
            }
            Err(KeydbReadError::TooLarge) => {
                json_response(request, 413, &keydb_too_large_body());
                return;
            }
        },
        Err(ureq::Error::StatusCode(code)) => {
            let msg = format!(
                r#"{{"ok":false,"error":"Server returned HTTP {}. Check the URL in Settings."}}"#,
                code
            );
            json_response(request, 502, &msg);
            return;
        }
        Err(e) => {
            // Do NOT echo the configured KEYDB origin/hostname to the client —
            // that leaks server config. Keep detail in the log only, through
            // `ureq_error_kind`, since `ureq::Error`'s Display isn't URL-free.
            tracing::warn!(
                origin = %crate::server::webhook::webhook_url_origin(&keydb_url),
                error_kind = %ureq_error_kind(&e),
                "keydb update: could not connect to configured KEYDB server"
            );
            json_response(
                request,
                502,
                r#"{"ok":false,"error":"Could not connect to the configured KEYDB server. Check the URL in Settings."}"#,
            );
            return;
        }
    };

    // Write to the service-canonical keydb path, NOT libfreemkv's exe-local
    // default — otherwise "Update KEYDB" reports success while every AACS
    // rip keeps failing because the read side looks elsewhere.
    let saved = crate::server::keysource::save_keydb(cfg, &body);
    match saved {
        Ok(result) => {
            let body = serde_json::json!({
                "ok": true,
                "entries": result.entries,
                "bytes": result.bytes,
            });
            json_response(request, 200, &body.to_string());
        }
        Err(e) if e.code() == libfreemkv::error::E_KEYDB_WRITE => {
            // A write/persist failure is an environment problem (disk full,
            // permissions on the keys dir) — not invalid content. Surface it
            // distinctly so the operator fixes the right thing.
            json_response(
                request,
                500,
                r#"{"ok":false,"error":"Failed to save KEYDB to disk (check space/permissions)"}"#,
            );
        }
        Err(_) => {
            json_response(
                request,
                500,
                r#"{"ok":false,"error":"Downloaded file is not a valid KEYDB. Check the URL."}"#,
            );
        }
    }
}

fn handle_eject(request: tiny_http::Request, device: &str) {
    if !ripper::device_known(device) {
        return json_response(request, 404, UNKNOWN_DEVICE_BODY);
    }
    // Gate on rip status (BU40N is slot-loading; eject mid-rip is
    // irreversible), enforced server-side since POST is unauthenticated.
    // Claim first, closing the busy-check/eject TOCTOU via one STATE lock.
    if ripper::try_claim_active_checked(device, false).is_none() {
        return json_response(
            request,
            409,
            r#"{"ok":false,"error":"drive busy; stop the rip before ejecting"}"#,
        );
    }
    let device_path = device_path(device);
    crate::server::ripper::eject_drive(&device_path);
    ripper::update_state(
        device,
        ripper::RipState {
            device: device.to_string(),
            status: "idle".to_string(),
            ..Default::default()
        },
    );
    json_response(request, 200, r#"{"ok":true}"#);
}

fn handle_stop(request: tiny_http::Request, device: &str) {
    // Stop halts and drains the rip thread and collapses the row to idle,
    // preserving partial staging for resume. A claim made after `entry_gen`
    // (insert dispatch or a Rip POST during the drain) is a new rip: left alone.
    let entry_gen = ripper::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .map(|rs| rs.claim_gen);
    // Armed before the drain, which can take up to 60s, so the poll loop does
    // not re-dispatch the drive the operator is stopping; refreshed after it.
    if entry_gen.is_some() {
        ripper::hold_stopped_disc(device);
        ripper::set_stop_cooldown(device);
    }

    // Cancel the halt and drain (ripper::stop_and_drain). A timed-out drain
    // is not a stop — the worker still owns the drive — so report what
    // actually happened instead of the old "reset to idle either way".
    let drained = ripper::stop_and_drain(device, std::time::Duration::from_secs(60));
    if !drained {
        tracing::error!(
            device = %device,
            "rip thread did not drain within 60s of stop — the worker is still \
             running and this device stays held until it exits (a worker wedged \
             in a blocking drive ioctl needs a container restart)"
        );
        crate::server::log::device_log(
            device,
            "Stop: the rip thread did not exit within 60s. It is still running, so \
             this drive stays busy until it does — scan/rip/eject will be refused. \
             If it never exits, restart the container.",
        );
    }

    let report = stop_report(drained);
    if !apply_stop_to_state(device, entry_gen, &report) {
        json_response(request, 404, r#"{"ok":false,"error":"drive not found"}"#);
        return;
    }
    // An undrained worker still owns the drive; release only what Stop actually ended.
    if drained {
        ripper::release_stopped_drive(device, entry_gen);
    }
    ripper::set_stop_cooldown(device);
    json_response(request, 200, report.body);
}

// Publish a Stop's outcome on the device's row. Returns whether the device
// exists. A row re-claimed since `entry_gen` is left untouched: wiping it would
// show idle while the new rip runs.
fn apply_stop_to_state(device: &str, entry_gen: Option<u64>, report: &StopReport) -> bool {
    // Recover-and-proceed on poison (house convention): a poisoned STATE must
    // not turn a Stop into a silent 404.
    let mut state = ripper::STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(rs) = state.get_mut(device) else {
        return false;
    };
    if entry_gen.is_some_and(|g| rs.claim_gen != g) {
        tracing::warn!(
            device = %device,
            "stop: the device was re-claimed while draining; leaving the new rip's state"
        );
        return true;
    }
    // Full reset: keep device id + disc_present, drop everything else.
    let disc_still_in = rs.disc_present;
    *rs = ripper::RipState {
        device: device.to_string(),
        status: report.status.to_string(),
        disc_present: disc_still_in,
        last_error: report.last_error.to_string(),
        claim_gen: rs.claim_gen,
        ..Default::default()
    };
    true
}

// What a Stop reports, as a function of whether the rip thread ACTUALLY
// drained. Split out of handle_stop so "a stop that stopped nothing must
// not render as success" is testable without the real 60s drain budget.
struct StopReport {
    /// `RipState::status` to publish.
    status: &'static str,
    /// `RipState::last_error` to publish (empty on the clean path).
    last_error: &'static str,
    /// The JSON response body. Always HTTP 200: the Stop WAS delivered (the
    /// `Halt` is cancelled either way) and the UI must not spin — but `ok` is
    /// false when the worker is still running, so a client that checks the
    /// field cannot read a timed-out drain as a completed stop.
    body: &'static str,
}

fn stop_report(drained: bool) -> StopReport {
    if drained {
        StopReport {
            status: "idle",
            last_error: "",
            body: r#"{"ok":true}"#,
        }
    } else {
        StopReport {
            // NOT idle: the device is still held by a live worker. "error" is
            // the status the dashboard renders together with `last_error`, so
            // the reason lands on the card instead of only in the log.
            status: "error",
            last_error: "Stop timed out: the rip thread is still running; the drive stays busy until it exits",
            body: r#"{"ok":false,"error":"stop timed out: the rip thread is still running"}"#,
        }
    }
}

pub(crate) fn percent_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Need two hex digits AFTER '%' (i+3 <= len); the old `i+2 < len`
        // guard was off by one, dropping a trailing `%XX`. Both bytes must be
        // hex digits — `from_str_radix` alone accepts a sign, so `%+3` misdecodes.
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
            && let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
        {
            result.push(byte);
            i += 3;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

/// Toggle debug logging on/off at runtime. POST body can be empty or contain {"enabled":true/false}.
fn handle_debug_toggle(request: tiny_http::Request) {
    let (request, body) = match read_json_body(request) {
        Ok(rb) => rb,
        Err(()) => return,
    };

    // `{"enabled": <bool>}` sets the level; any other body (missing,
    // non-bool, or invalid JSON) defaults to OFF — a malformed/empty POST
    // must not silently turn on verbose debug logging.
    let enabled = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => v.get("enabled").and_then(|b| b.as_bool()).unwrap_or(false),
        Err(_) => false,
    };

    *DEBUG_ENABLED
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = enabled;

    // Swap the EnvFilter so libfreemkv's `tracing::debug!` events actually
    // surface in docker logs while debug is on — without this the toggle only
    // flips autorip's own checks and the library stays at warn.
    let filter_swapped = crate::server::observe::set_debug(enabled);

    tracing::info!(enabled, filter_swapped, "debug logging toggled");
    json_response(
        request,
        200,
        &serde_json::json!({
            "ok": true,
            "enabled": enabled,
            "filter_swapped": filter_swapped,
        })
        .to_string(),
    );
}
