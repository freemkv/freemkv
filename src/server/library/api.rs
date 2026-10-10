//! `/api/library*` and the Library's frames on the `/events` stream.
//!
//! Listings answer from index/queue snapshots. Mutations persist their state;
//! corrected-remux admission also verifies source and ownership identities.
//! Media scanning and muxing belong to the indexer and worker threads.

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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchRequest {
    source: PathBuf,
    expected_revision: u64,
    media: crate::server::planner::MediaMetadata,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmedMatchRequest {
    source: PathBuf,
    expected_revision: u64,
    media: crate::server::planner::MediaMetadata,
    ownership: super::ownership::Confirmation,
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
        (true, _, "/api/library/match") => {
            let source = PathBuf::from(query_param(&url, "source").unwrap_or_default());
            if not_ready(&lib)
                || !lib
                    .listing(&d)
                    .rows
                    .iter()
                    .any(|r| r.iso.as_ref() == Some(&source))
            {
                err(request, 404, "source ISO is not in the current library");
            } else {
                let preview = lib.preview_ownership(&c, &source);
                match preview {
                    Ok((saved, preview)) => json_response(
                        request,
                        200,
                        &json!({"source": source, "saved": saved,
                            "owned_outputs": preview.owned_outputs, "candidates": preview.candidates,
                            "preview_token": preview.preview_token,
                            "omitted_candidates": preview.omitted_candidates})
                            .to_string(),
                    ),
                    Err(error) => err(request, 409, &error.to_string()),
                }
            }
        }
        (_, true, "/api/library/match/remux") => {
            let Ok((request, body)) = read_json_body(request) else {
                return None;
            };
            let Ok(change) = serde_json::from_str::<ConfirmedMatchRequest>(&body) else {
                err(request, 400, "invalid confirmed source match request");
                return None;
            };
            match lib.confirm_match_and_queue(
                &c,
                &change.source,
                change.expected_revision,
                change.media,
                change.ownership,
            ) {
                Ok((saved, queued)) => json_response(
                    request,
                    200,
                    &json!({"ok": true, "saved": saved, "queued": queued}).to_string(),
                ),
                Err(error) => err(request, 409, &error.to_string()),
            }
        }
        (_, true, "/api/library/match") => {
            let Ok((request, body)) = read_json_body(request) else {
                return None;
            };
            let Ok(change) = serde_json::from_str::<MatchRequest>(&body) else {
                err(request, 400, "invalid source match request");
                return None;
            };
            match lib.change_source_match(
                &d,
                &change.source,
                change.expected_revision,
                change.media,
            ) {
                Ok(saved) => json_response(
                    request,
                    200,
                    &json!({"ok": true, "saved": saved}).to_string(),
                ),
                Err(error) => {
                    let status = match error.kind() {
                        std::io::ErrorKind::InvalidInput => 400,
                        std::io::ErrorKind::NotFound => 404,
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::AlreadyExists => 409,
                        _ => 500,
                    };
                    err(request, status, &error.to_string());
                }
            }
        }
        (true, _, "/api/library/raw") => {
            let path = query_param(&url, "path").unwrap_or_default();
            match lib.audits.raw(std::path::Path::new(&path)) {
                Some(raw) => json_response(request, 200, &json!({"sections": raw}).to_string()),
                None => err(request, 404, "no stored report for that file"),
            }
        }
        (true, _, "/api/library/console") => json_response(request, 200, &console_json(&lib)),
        (true, _, "/api/library/folders") => {
            json_response(request, 200, &folders_json(&lib, &d).to_string())
        }
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
            match lib.enqueue_checked(&d, pick) {
                Ok(n) => queued(request, n, eligible),
                Err(error) => err(request, 500, &error),
            }
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
            match lib.enqueue_checked(&d, pick) {
                Ok(n) => queued(request, n, eligible),
                Err(error) => err(request, 500, &error),
            }
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
        (_, true, "/api/library/staged/retry") | (_, true, "/api/library/staged/discard") => {
            let Ok((request, body)) = read_json_body(request) else {
                return None;
            };
            let Some(target) = targets_of(&body).pop() else {
                err(request, 400, "missing target");
                return None;
            };
            if path.ends_with("/retry") {
                if lib.queue.retry_staged_now(&target) {
                    json_response(request, 200, &json!({"ok": true, "queued": 1}).to_string());
                } else {
                    err(request, 404, "no finished file is waiting for that title");
                }
                return None;
            }
            let stage = crate::server::health::remux_stage_dir();
            match discard_kept(&lib, &target, stage.as_deref()) {
                Ok(()) => json_response(request, 200, r#"{"ok":true,"discarded":true}"#),
                Err((code, msg)) => err(request, code, &msg),
            }
        }
        (_, true, "/api/library/staged/clear") => {
            let root = PathBuf::from(&c.staging_dir);
            let (mut discarded, mut failed) = clear_idle_staging(&root);
            if let Some(remux_stage) = crate::server::health::remux_stage_dir()
                && remux_stage != root
            {
                let targets: Vec<PathBuf> = lib
                    .queue
                    .snapshot()
                    .jobs
                    .iter()
                    .filter(|j| j.staged.is_some())
                    .map(|j| j.target.clone())
                    .collect();
                for target in targets {
                    match discard_kept(&lib, &target, Some(&remux_stage)) {
                        Ok(()) => discarded += 1,
                        Err((_, msg)) => {
                            failed += 1;
                            tracing::warn!(target = %target.display(), error = %msg, "legacy remux staging file was not cleared");
                        }
                    }
                }
            }
            json_response(
                request,
                200,
                &json!({"ok": true, "discarded": discarded, "failed": failed}).to_string(),
            );
        }
        (_, true, "/api/library/queue/stop-all") => {
            let (running, removed) = lib.stop_all();
            let paused = lib.queue.snapshot().paused;
            json_response(
                request,
                200,
                &json!({"ok": true, "stopped": running, "removed": removed, "paused": paused})
                    .to_string(),
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

/// Remove only per-disc staging directories that are not owned by a live rip
/// or mux worker. Unknown/unreadable entries are left in place.
fn clear_idle_staging(root: &std::path::Path) -> (usize, usize) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::warn!(path = %root.display(), error = %e, "staging root could not be listed");
            return (0, 1);
        }
    };
    let mut removed = 0;
    let mut failed = 0;
    for entry in entries {
        let Ok(entry) = entry else {
            failed += 1;
            continue;
        };
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let job_lease = crate::server::ripper::staging::job_lease(&path);
        let Ok(_idle) = job_lease.try_lock() else {
            continue;
        };
        let Some(snapshot) = crate::server::ripper::staging::snapshot_staging_disc(&path) else {
            continue;
        };
        if snapshot.has_sweeping
            || snapshot.has_ripped
            || snapshot.has_muxing
            || snapshot.has_done
            || snapshot.had_entry_error
            || snapshot.state_unreadable.is_some()
            || crate::server::ripper::staging::read_state(&path).is_none()
        {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(path = %path.display(), error = %e, "idle staging entry could not be cleared");
            }
        }
    }
    (removed, failed)
}

// Delete the finished file kept for `target`. Only a kept pair directly in the staging folder
// is deleted, and never while its job is copying it in; the job forgets it first.
fn discard_kept(
    lib: &Library,
    target: &std::path::Path,
    stage: Option<&std::path::Path>,
) -> Result<(), (u16, String)> {
    let kept = lib
        .queue
        .snapshot()
        .jobs
        .iter()
        .rev()
        .find(|j| j.target == target && j.staged.is_some())
        .and_then(|j| j.staged.clone());
    let Some(kept) = kept else {
        return Err((404, "no finished file is kept for that title".into()));
    };
    if stage.is_none_or(|d| kept.parent() != Some(d)) {
        return Err((
            409,
            "the kept file is not in the remux staging folder; nothing was deleted".into(),
        ));
    }
    match lib.queue.take_staged(target) {
        Ok(path) if path == kept => {}
        Ok(_) | Err(false) => {
            return Err((409, "the kept file changed; reload and try again".into()));
        }
        Err(true) => {
            return Err((409, "it is being copied in right now; stop it first".into()));
        }
    }
    super::deliver::discard(&kept).map_err(|e| {
        tracing::warn!(path = %kept.display(), error = %e, "kept remux could not be discarded");
        (500, format!("could not delete {}: {e}", kept.display()))
    })
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

// The folders a remux uses, each with its last check, and why the queue waits (if it does).
fn folders_json(lib: &Library, d: &super::Dirs) -> serde_json::Value {
    json!({
        "folders": super::folder_views(d),
        "hold": lib.hold(),
    })
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
        "folders": super::folder_views(&d),
        "hold": lib.hold(),
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
    generation: (u64, u64, u64, u64, u64, u64),
    seq: u64,
    started: bool,
}

/// A `library` event for the SSE stream when anything moved since `cursor`.
/// Named, so a page that only listens for the rip state never sees it.
/// `dirs` names the folders whose health the frame carries.
pub fn sse_frame(cursor: &mut SseCursor, dirs: Option<&super::Dirs>) -> Option<String> {
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
        crate::server::health::generation(),
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
        "hold": lib.hold(),
        "folders": dirs.map(super::folder_views),
    });
    Some(format!("event: library\ndata: {body}\n\n"))
}

#[cfg(test)]
#[path = "api_tests.rs"]
mod tests;
