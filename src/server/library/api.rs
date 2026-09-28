//! `/api/library*` and the Library's frames on the `/events` stream.

use super::queue::JobState;
use super::{Library, dirs, instance};
use crate::server::config::Config;
use crate::server::web::{json_response, percent_decode, read_json_body};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

fn snapshot(cfg: &Arc<RwLock<Config>>) -> Config {
    cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn query_param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
        .map(|v| percent_decode(&v.replace('+', " ")))
}

/// Serve a `/api/library*` request; one that is not ours is handed back.
pub fn handle(
    request: tiny_http::Request,
    cfg: &Arc<RwLock<Config>>,
) -> Option<tiny_http::Request> {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let get = *request.method() == tiny_http::Method::Get;
    let post = *request.method() == tiny_http::Method::Post;
    let c = snapshot(cfg);
    let lib = instance(&c);
    let d = dirs(&c);
    let ok = |request, n: usize| {
        json_response(
            request,
            200,
            &serde_json::json!({"ok": true, "queued": n}).to_string(),
        )
    };
    match (get, post, path.as_str()) {
        (true, _, "/api/library") => json_response(request, 200, &library_json(&lib, &c)),
        (true, _, "/api/library/console") => json_response(request, 200, &console_json(&lib)),
        (true, _, "/api/library/log") => {
            let title = query_param(&url, "title").unwrap_or_default();
            if title.is_empty() {
                {
                    json_response(request, 400, r#"{"ok":false,"error":"missing title"}"#);
                    return None;
                }
            }
            text_response(request, &log_tail(&lib, &title));
        }
        (_, true, "/api/library/queue/add") => {
            let Ok((request, body)) = read_json_body(request) else {
                return None;
            };
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let mut wanted: Vec<PathBuf> = v
                .get("targets")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str())
                        .map(PathBuf::from)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(t) = v.get("target").and_then(|t| t.as_str()) {
                wanted.push(PathBuf::from(t));
            }
            if wanted.is_empty() {
                {
                    json_response(request, 400, r#"{"ok":false,"error":"missing target"}"#);
                    return None;
                }
            }
            let n = lib.enqueue(&d, |r| {
                r.target.as_ref().is_some_and(|t| wanted.contains(t))
            });
            ok(request, n);
        }
        (_, true, "/api/library/queue/out-of-date") => {
            let n = lib.enqueue(&d, |r| r.needs_remux);
            ok(request, n);
        }
        (_, true, "/api/library/queue/all") => {
            let n = lib.enqueue(&d, |_| true);
            ok(request, n);
        }
        (_, true, "/api/library/queue/clear") => {
            let n = lib.queue.clear_finished();
            json_response(
                request,
                200,
                &serde_json::json!({"ok": true, "cleared": n}).to_string(),
            );
        }
        (_, true, "/api/library/queue/pause") | (_, true, "/api/library/queue/resume") => {
            lib.queue.set_paused(path.ends_with("/pause"));
            json_response(request, 200, &queue_json(&lib).to_string());
        }
        (_, true, "/api/library/debug") => {
            let Ok((request, body)) = read_json_body(request) else {
                return None;
            };
            let on = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("enabled")?.as_bool())
                .unwrap_or(false);
            lib.queue.set_debug_log(on);
            json_response(request, 200, &queue_json(&lib).to_string());
        }
        _ => return Some(request),
    }
    None
}

fn text_response(request: tiny_http::Request, body: &str) {
    let response = tiny_http::Response::from_string(body)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        );
    let _ = request.respond(response);
}

fn queue_json(lib: &Library) -> serde_json::Value {
    let q = lib.queue.snapshot();
    serde_json::json!({
        "paused": q.paused,
        "debug_log": q.debug_log,
        "queued": q.count(JobState::Queued),
        "running": q.running(),
        "done": q.count(JobState::Done),
        "failed": q.count(JobState::Failed),
        "jobs": q.jobs,
    })
}

/// The body of `GET /api/library`.
pub fn library_json(lib: &Library, cfg: &Config) -> String {
    let d = dirs(cfg);
    let listing = lib.listing(&d);
    let (major, minor, patch) = super::probe::running_version();
    serde_json::json!({
        "version": format!("{major}.{minor}.{patch}"),
        "version_label": crate::server::VERSION_LABEL,
        "library_dir": d.library,
        "iso_dir": d.isos,
        "iso_subfolders": d.iso_subfolders,
        "incomplete": listing.incomplete,
        "rows": listing.rows,
        "queue": queue_json(lib),
        "live": lib.running(),
    })
    .to_string()
}

fn console_json(lib: &Library) -> String {
    serde_json::json!({
        "running": lib.running(),
        "lines": lib.console_since(0),
        "queue": queue_json(lib),
    })
    .to_string()
}

// The last 256 KiB of a title's log.
fn log_tail(lib: &Library, title: &str) -> String {
    use std::io::{Read as _, Seek as _};
    const TAIL: u64 = 256 << 10;
    let Ok(mut f) = std::fs::File::open(lib.log_path(title)) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(std::io::SeekFrom::Start(len.saturating_sub(TAIL)));
    let mut buf = Vec::new();
    let _ = f.take(TAIL).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// What an `/events` client has already been sent.
#[derive(Default)]
pub struct SseCursor {
    generation: (u64, u64),
    seq: u64,
}

/// A `library` event for the SSE stream when anything moved since `cursor`.
/// Named, so a page that only listens for the rip state never sees it.
pub fn sse_frame(cursor: &mut SseCursor) -> Option<String> {
    let lib = super::get()?;
    let generation = lib.generation();
    if generation == cursor.generation {
        return None;
    }
    cursor.generation = generation;
    let lines = lib.console_since(cursor.seq);
    if let Some(last) = lines.last() {
        cursor.seq = last.seq;
    }
    let q = lib.queue.snapshot();
    let body = serde_json::json!({
        "queue_generation": generation.0,
        "running": lib.running(),
        "paused": q.paused,
        "queued": q.count(JobState::Queued),
        "lines": lines,
    });
    Some(format!("event: library\ndata: {body}\n\n"))
}
