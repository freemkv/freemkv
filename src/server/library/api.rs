//! `/api/library*` and the Library's frames on the `/events` stream.
//!
//! Every handler here answers from memory: the index snapshot, the queue and
//! the console. Filesystem work belongs to the indexer and the worker threads
//! (the per-title log read is the one exception, and it takes no lock).

use super::queue::JobState;
use super::{Library, dirs, instance};
use crate::server::config::Config;
use crate::server::web::{json_response, percent_decode, read_json_body};
use serde_json::json;
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

fn err(request: tiny_http::Request, code: u16, msg: &str) {
    json_response(
        request,
        code,
        &json!({"ok": false, "error": msg}).to_string(),
    );
}

fn targets_of(body: &str) -> Vec<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let mut out: Vec<PathBuf> = v
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
        out.push(PathBuf::from(t));
    }
    out
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
    // A bulk queue before the first scan would silently queue nothing.
    let not_ready = |lib: &Library| lib.snapshot().dirs.as_ref() != Some(&d);
    let queued = |request, n: usize, eligible: usize| {
        json_response(
            request,
            200,
            &json!({"ok": true, "queued": n, "eligible": eligible}).to_string(),
        )
    };
    match (get, post, path.as_str()) {
        (true, _, "/api/library") if query_param(&url, "download").is_some() => {
            download(request, &library_json(&lib, &c));
        }
        (true, _, "/api/library") => json_response(request, 200, &library_json(&lib, &c)),
        (true, _, "/api/library/raw") => {
            let path = query_param(&url, "path").unwrap_or_default();
            match lib.audits.raw(std::path::Path::new(&path)) {
                Some(raw) => json_response(request, 200, &json!({"sections": raw}).to_string()),
                None => err(request, 404, "no stored report for that file"),
            }
        }
        (true, _, "/api/library/console") => json_response(request, 200, &console_json(&lib)),
        (true, _, "/api/library/log") => {
            let title = query_param(&url, "title").unwrap_or_default();
            if title.is_empty() {
                err(request, 400, "missing title");
                return None;
            }
            let text = log_tail(&lib, &title);
            if query_param(&url, "raw").is_some() {
                text_response(request, &text);
            } else {
                let lines = super::parse_log(&text);
                json_response(
                    request,
                    200,
                    &json!({"title": title, "lines": lines}).to_string(),
                );
            }
        }
        (_, true, "/api/library/rescan") => {
            lib.wake_indexer();
            json_response(request, 200, r#"{"ok":true}"#);
        }
        (_, true, "/api/library/reaudit") => {
            let Ok((request, body)) = read_json_body(request) else {
                return None;
            };
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let n = if v.get("all").and_then(|a| a.as_bool()) == Some(true) {
                lib.reaudit(None)
            } else {
                let paths: Vec<PathBuf> = v
                    .get("paths")
                    .and_then(|p| p.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|p| p.as_str())
                            .map(PathBuf::from)
                            .collect()
                    })
                    .unwrap_or_default();
                if paths.is_empty() {
                    err(request, 400, "missing paths");
                    return None;
                }
                lib.reaudit(Some(&paths))
            };
            json_response(
                request,
                200,
                &json!({"ok": true, "requeued": n}).to_string(),
            );
        }
        (_, true, "/api/library/queue/add") => {
            let Ok((request, body)) = read_json_body(request) else {
                return None;
            };
            let wanted = targets_of(&body);
            if wanted.is_empty() {
                err(request, 400, "missing target");
                return None;
            }
            if not_ready(&lib) {
                err(
                    request,
                    409,
                    "the library is still being scanned; try again in a moment",
                );
                return None;
            }
            let eligible = wanted.len();
            let wanted: std::collections::HashSet<PathBuf> = wanted.into_iter().collect();
            let pick = |r: &super::RowView| r.target.as_ref().is_some_and(|t| wanted.contains(t));
            let creates = lib
                .listing(&d)
                .rows
                .iter()
                .any(|r| pick(r) && r.mkv.is_none());
            if let Some(why) = lib.queue_block(&d, creates) {
                err(request, 409, &why);
                return None;
            }
            let n = lib.enqueue(&d, pick);
            queued(request, n, eligible);
        }
        (_, true, "/api/library/queue/out-of-date") | (_, true, "/api/library/queue/all") => {
            if not_ready(&lib) {
                err(
                    request,
                    409,
                    "the library is still being scanned; try again in a moment",
                );
                return None;
            }
            if let Some(why) = lib.queue_block(&d, true) {
                err(request, 409, &why);
                return None;
            }
            let all = path.ends_with("/all");
            let pick = |r: &super::RowView| all || r.needs_remux;
            let eligible = lib
                .listing(&d)
                .rows
                .iter()
                .filter(|r| r.target.is_some() && r.iso.is_some() && pick(r))
                .count();
            let n = lib.enqueue(&d, pick);
            queued(request, n, eligible);
        }
        (_, true, "/api/library/queue/remove") => {
            let Ok((request, body)) = read_json_body(request) else {
                return None;
            };
            let targets = targets_of(&body);
            if targets.is_empty() {
                err(request, 400, "missing target");
                return None;
            }
            let n = targets
                .iter()
                .map(|t| lib.queue.remove_queued(t))
                .sum::<usize>();
            json_response(request, 200, &json!({"ok": true, "removed": n}).to_string());
        }
        (_, true, "/api/library/queue/stop-all") => {
            let (running, removed) = lib.stop_all();
            json_response(
                request,
                200,
                &json!({"ok": true, "stopped": running, "removed": removed}).to_string(),
            );
        }
        (_, true, "/api/library/queue/clear-queued") => {
            let n = lib.queue.clear_queued();
            json_response(request, 200, &json!({"ok": true, "removed": n}).to_string());
        }
        (_, true, "/api/library/queue/clear") => {
            let n = lib.queue.clear_finished();
            json_response(request, 200, &json!({"ok": true, "cleared": n}).to_string());
        }
        (_, true, "/api/library/audit/pause") | (_, true, "/api/library/audit/resume") => {
            lib.audits.set_paused(path.ends_with("/pause"));
            json_response(request, 200, &audit_json(&lib).to_string());
        }
        (_, true, "/api/library/audit/stop-all") => {
            let running = lib.audits.status().running.is_some();
            let removed = lib.audits.stop_all();
            lib.touch_index();
            json_response(
                request,
                200,
                &json!({"ok": true, "stopped": running, "removed": removed}).to_string(),
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
            let Some(on) = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("enabled")?.as_bool())
            else {
                err(request, 400, "missing enabled");
                return None;
            };
            lib.queue.set_debug_log(on);
            json_response(request, 200, &queue_json(&lib).to_string());
        }
        _ => return Some(request),
    }
    None
}

// The listing as a file: `freemkv-library-<date>.json`, saved, not shown.
fn download(request: tiny_http::Request, body: &str) {
    let disposition = format!(
        "attachment; filename=\"freemkv-library-{}.json\"",
        crate::server::util::format_date()
    );
    let response = tiny_http::Response::from_string(body)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Disposition"[..], disposition.as_bytes())
                .expect("ascii header"),
        )
        .with_header(
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header"),
        );
    let _ = request.respond(response);
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
    json!({
        "paused": q.paused,
        "debug_log": q.debug_log,
        "queued": q.count(JobState::Queued),
        "running": q.running(),
        "done": q.count(JobState::Done),
        "failed": q.count(JobState::Failed),
        "jobs": q.jobs,
    })
}

fn audit_json(lib: &Library) -> serde_json::Value {
    json!(lib.audits.status())
}

/// The body of `GET /api/library`. Reads memory only.
pub fn library_json(lib: &Library, cfg: &Config) -> String {
    let d = dirs(cfg);
    let listing = lib.listing(&d);
    let (major, minor, patch) = super::probe::running_version();
    json!({
        "version": format!("{major}.{minor}.{patch}"),
        "version_label": crate::server::VERSION_LABEL,
        "library_dir": d.library,
        "iso_dir": d.isos,
        "iso_subfolders": d.iso_subfolders,
        "incomplete": listing.incomplete,
        "scanning": listing.scanning,
        "indexing": lib.indexing(),
        "scanned_at": listing.scanned_at,
        "scan_ms": listing.scan_ms,
        "probing": listing.probing,
        "auditing": listing.auditing,
        "index_generation": lib.index_generation(),
        "rows": listing.rows,
        "queue": queue_json(lib),
        "live": lib.running(),
        "audits": audit_json(lib),
        "deep_audit": lib.deep_enabled(),
    })
    .to_string()
}

fn console_json(lib: &Library) -> String {
    let (job, title, lines) = lib.console_job();
    json!({
        "job": job,
        "title": title,
        "running": lib.running(),
        "lines": lines,
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
    let text = String::from_utf8_lossy(&buf).into_owned();
    if len > TAIL {
        text.split_once('\n')
            .map_or(text.clone(), |(_, rest)| rest.to_string())
    } else {
        text
    }
}

/// What an `/events` client has already been sent.
#[derive(Default)]
pub struct SseCursor {
    generation: (u64, u64, u64, u64, u64),
    seq: u64,
    started: bool,
}

/// A `library` event for the SSE stream when anything moved since `cursor`.
/// Named, so a page that only listens for the rip state never sees it.
pub fn sse_frame(cursor: &mut SseCursor) -> Option<String> {
    let lib = super::get()?;
    if !cursor.started {
        // A new client fetches the console once; the stream only adds to it.
        cursor.started = true;
        cursor.seq = lib.last_seq();
    }
    let (q, live) = lib.generation();
    let generation = (
        q,
        live,
        lib.index_generation(),
        lib.audits.generation(),
        lib.audits.progress_generation(),
    );
    if generation == cursor.generation {
        return None;
    }
    cursor.generation = generation;
    let lines = lib.console_since(cursor.seq);
    if let Some(last) = lines.last() {
        cursor.seq = last.seq;
    }
    let snap = lib.queue.snapshot();
    let body = json!({
        "queue_generation": q,
        "index_generation": generation.2,
        "audit_generation": generation.3,
        "audits": audit_json(&lib),
        "indexing": lib.indexing(),
        "running": lib.running(),
        "job_title": lib.job_title(),
        "paused": snap.paused,
        "queued": snap.count(JobState::Queued),
        "lines": lines,
    });
    Some(format!("event: library\ndata: {body}\n\n"))
}
