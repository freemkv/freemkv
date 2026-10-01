//! Observability — single init point for the structured event log.
//!
//! Three sinks, written from the same tracing event stream:
//! `{AUTORIP_DIR}/logs/autorip.log` (daily-rolling, human-readable),
//! `{AUTORIP_DIR}/logs/autorip.jsonl` (size-capped, tailed by
//! `/api/debug`), and stderr (compact, captured by Docker).
//!
//! Filter level via `AUTORIP_LOG_LEVEL` (env-filter syntax). Default
//! [`FILTER_OFF`]: the daemon at info, the engine libraries at warn.
//!
//! The daemon's own events carry the module-path target `freemkv::server::…`
//! (they were `autorip::…` when it was its own crate), so every `autorip`
//! directive — built in or operator-supplied — is mirrored onto
//! `freemkv::server` (see [`with_server_targets`]). Explicit
//! `target: "autorip::…"` events are unchanged.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::reload;
use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt, util::SubscriberInitExt};

// EnvFilter directive used when /api/debug is OFF (the normal state): the daemon at info, the
// engine libraries at warn. /api/debug ON swaps in FILTER_ON.
const FILTER_OFF: &str = "autorip=info,freemkv::server=info,libfreemkv=warn,freemkv=warn";

// EnvFilter directive used when /api/debug is ON: debug globally, plus mux/stream/freemkv
// targets needed for drive + mux forensics.
const FILTER_ON: &str =
    "autorip=debug,freemkv::server=debug,libfreemkv=debug,freemkv=debug,mux=debug,stream=debug";

/// Mirror every `autorip` directive in `spec` onto `freemkv::server`, so an
/// operator's `AUTORIP_LOG_LEVEL=autorip=debug` still reaches the daemon's
/// module-path events. Other directives pass through untouched.
fn with_server_targets(spec: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for d in spec.split(',') {
        out.push(d.to_string());
        let t = d.trim();
        if let Some(rest) = t.strip_prefix("autorip")
            && (rest.is_empty()
                || rest.starts_with('=')
                || rest.starts_with("::")
                || rest.starts_with('['))
        {
            out.push(format!("freemkv::server{rest}"));
        }
    }
    out.join(",")
}

// Live-file size at which `autorip.jsonl` rotates to `autorip.jsonl.1`. Debug logging emits
// thousands of lines a minute, so the cap must hold a useful window of it; 256 MiB is 50x the
// system-log cap (log.rs) and bounds total disk use to about 512 MiB.
const JSONL_ROTATE_BYTES: u64 = 256 * 1024 * 1024;

/// Append-only file that rotates itself to `<path>.1` (replacing any older one) before a
/// write that would pass `limit`, then continues in a fresh file at `path`. Each `write` call
/// lands whole in one file, so a line is never split as long as callers write one line per
/// call (the tracing fmt layer does). Owned by the single non-blocking worker thread.
struct SizeCappedFile {
    path: std::path::PathBuf,
    limit: u64,
    file: Option<std::fs::File>,
    len: u64,
}

impl SizeCappedFile {
    fn open(path: std::path::PathBuf, limit: u64) -> std::io::Result<Self> {
        let file = Self::open_append(&path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path,
            limit,
            file: Some(file),
            len,
        })
    }

    // A writer that discards everything, for when the log file cannot be opened.
    fn disabled() -> Self {
        Self {
            path: std::path::PathBuf::new(),
            limit: u64::MAX,
            file: None,
            len: 0,
        }
    }

    fn open_append(path: &std::path::Path) -> std::io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let mut backup = self.path.clone().into_os_string();
        backup.push(".1");
        // Close the old handle first; a failed rename still reopens the live path below.
        self.file = None;
        let renamed = std::fs::rename(&self.path, &backup);
        self.file = Some(Self::open_append(&self.path)?);
        self.len = self
            .file
            .as_ref()
            .map_or(0, |f| f.metadata().map_or(0, |m| m.len()));
        renamed
    }
}

impl std::io::Write for SizeCappedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.file.is_none() {
            return Ok(buf.len());
        }
        if self.len > 0 && self.len + buf.len() as u64 > self.limit {
            // A failed rotation must not drop the event; keep appending to whatever is open.
            let _ = self.rotate();
        }
        let f = self.file.as_mut().ok_or(std::io::ErrorKind::NotFound)?;
        f.write_all(buf)?;
        self.len += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// Worker guards for the non-blocking file appenders. Must outlive the
/// process — flushed on drop. Stored in a static so `init()` can be called
/// from `main` without the caller having to thread guards through.
static GUARDS: once_cell::sync::OnceCell<Vec<WorkerGuard>> = once_cell::sync::OnceCell::new();

// Reload handle for the active EnvFilter, set by `init()` and swapped by
// `set_debug()`. Absent when init failed or AUTORIP_LOG_LEVEL was set
// explicitly, in which case `set_debug` becomes a no-op.
static RELOAD_HANDLE: once_cell::sync::OnceCell<reload::Handle<EnvFilter, Registry>> =
    once_cell::sync::OnceCell::new();

/// Initialize the tracing stack. Returns nothing.
///
/// Contract: call exactly once, early in `main`, before any threads are spawned. A sequential
/// second call is a no-op, but this is not a synchronization barrier.
pub fn init() {
    if GUARDS.get().is_some() {
        return;
    }

    let log_dir = log_dir();
    let _ = std::fs::create_dir_all(&log_dir);

    // Honour AUTORIP_LOG_LEVEL if set explicitly; /api/debug becomes a
    // no-op on the filter then (operator directive wins). Otherwise
    // install FILTER_OFF with a reload handle for /api/debug to use.
    let env_override = std::env::var("AUTORIP_LOG_LEVEL")
        .ok()
        .filter(|s| !s.is_empty());
    // An unparsable directive is reported (the sinks are not up yet, so on stderr) and ignored,
    // leaving the default filter AND the /api/debug toggle in force.
    let env_filter = env_override
        .as_deref()
        .and_then(|s| match parse_override(s) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("AUTORIP_LOG_LEVEL {s:?} ignored, using the default filter: {e}");
                None
            }
        });
    let overridden = env_filter.is_some();
    let initial_filter = env_filter.unwrap_or_else(|| EnvFilter::new(FILTER_OFF));
    let (filter, reload_handle) = reload::Layer::new(initial_filter);
    let reload_handle = if overridden {
        None
    } else {
        Some(reload_handle)
    };

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_ansi(false)
        .compact();

    let mut guards: Vec<WorkerGuard> = Vec::new();

    // Human-readable log: daily-rolled. Operators tailing for the day's
    // events get a manageable file size; older days archive to disk.
    let human_appender = rolling::daily(&log_dir, "autorip.log");
    let (human_writer, human_guard) = tracing_appender::non_blocking(human_appender);
    guards.push(human_guard);
    let human_layer = fmt::layer()
        .with_writer(human_writer)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true);

    // Machine-readable JSONL: size-capped, never date-rolled. `/api/debug` tails a stable
    // path, so the live file stays at `autorip.jsonl` and the previous one is `.1`.
    let json_appender = match SizeCappedFile::open(
        std::path::Path::new(&log_dir).join("autorip.jsonl"),
        JSONL_ROTATE_BYTES,
    ) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("autorip.jsonl unavailable, JSON log disabled: {e}");
            SizeCappedFile::disabled()
        }
    };
    let (json_writer, json_guard) = tracing_appender::non_blocking(json_appender);
    guards.push(json_guard);
    let json_layer = fmt::layer()
        .json()
        .with_writer(json_writer)
        .with_target(true)
        .with_thread_ids(true);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(human_layer)
        .with(json_layer)
        .init();

    if let Some(h) = reload_handle {
        let _ = RELOAD_HANDLE.set(h);
    }
    let _ = GUARDS.set(guards);
}

// The operator's `AUTORIP_LOG_LEVEL` spec as a filter (`autorip` directives mirrored onto the
// daemon's module targets), or why it does not parse.
fn parse_override(spec: &str) -> Result<EnvFilter, String> {
    EnvFilter::try_new(with_server_targets(spec)).map_err(|e| e.to_string())
}

/// Swap the active EnvFilter at runtime. Called by `/api/debug` to
/// flip between FILTER_OFF and FILTER_ON. No-op if `AUTORIP_LOG_LEVEL`
/// was set explicitly at startup (the operator's directive wins) or if
/// init() failed.
///
/// Returns `true` when the swap was applied, `false` when the handle
/// is absent (env override, init failure) so the caller can surface
/// the no-op in the API response.
pub fn set_debug(enabled: bool) -> bool {
    let Some(handle) = RELOAD_HANDLE.get() else {
        return false;
    };
    swap_filter(handle, enabled)
}

// Point `handle` at FILTER_ON or FILTER_OFF; `true` when the swap took.
fn swap_filter(handle: &reload::Handle<EnvFilter, Registry>, enabled: bool) -> bool {
    let directive = if enabled { FILTER_ON } else { FILTER_OFF };
    let new_filter = match EnvFilter::try_new(directive) {
        Ok(f) => f,
        Err(_) => return false,
    };
    handle.reload(new_filter).is_ok()
}

fn log_dir() -> String {
    // `<autorip_dir>/logs` — the same base as config/log, so all sinks agree on one
    // writable directory.
    format!("{}/logs", crate::server::config::default_autorip_dir())
}

/// Path of the JSONL stream — exposed so the web `/api/debug` endpoint
/// can tail it without re-deriving the layout.
pub fn json_log_path() -> String {
    format!("{}/autorip.jsonl", log_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Writing past the limit must rotate to `.1`, keep the live file under the limit, and keep
    // every line whole in exactly one of the two files.
    #[test]
    fn jsonl_rotates_by_size_without_splitting_lines() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("autorip.jsonl");
        let limit = 1000u64;
        let mut w = SizeCappedFile::open(path.clone(), limit).unwrap();
        let line = format!("{{\"m\":\"{}\"}}\n", "x".repeat(40));
        for _ in 0..30 {
            w.write_all(line.as_bytes()).unwrap();
        }
        w.flush().unwrap();
        let backup = dir.path().join("autorip.jsonl.1");
        assert!(backup.exists(), "rotation must produce .1");
        let live = std::fs::read_to_string(&path).unwrap();
        assert!(live.len() as u64 <= limit, "live file over the limit");
        let old = std::fs::read_to_string(&backup).unwrap();
        assert!(old.len() as u64 <= limit, ".1 over the limit");
        for l in live.lines().chain(old.lines()) {
            assert_eq!(format!("{l}\n"), line, "line split across files");
        }
    }

    /// Both filter strings must parse — a typo here would mean the
    /// /api/debug toggle silently no-ops in production. This is the
    /// cheapest possible guard.
    #[test]
    fn filter_strings_parse() {
        EnvFilter::try_new(FILTER_OFF).expect("FILTER_OFF must parse");
        EnvFilter::try_new(FILTER_ON).expect("FILTER_ON must parse");
    }

    // FILTER_ON must enable the `mux` and `stream` targets at debug —
    // guard against future edits that drop them.
    #[test]
    fn filter_on_includes_mux_and_stream_targets() {
        assert!(
            FILTER_ON.contains("mux=debug"),
            "FILTER_ON must enable target=\"mux\" at debug; got: {FILTER_ON}"
        );
        assert!(
            FILTER_ON.contains("stream=debug"),
            "FILTER_ON must enable target=\"stream\" at debug; got: {FILTER_ON}"
        );
        assert!(
            FILTER_ON.contains("libfreemkv=debug"),
            "FILTER_ON must raise libfreemkv to debug; got: {FILTER_ON}"
        );
    }

    /// FILTER_OFF must not accidentally turn on the verbose targets.
    /// If a future edit promotes them, /api/debug becomes meaningless
    /// because the steady-state already shows the events.
    #[test]
    fn filter_off_stays_quiet_on_mux_and_stream() {
        assert!(
            !FILTER_OFF.contains("mux=debug"),
            "FILTER_OFF must not enable mux at debug; got: {FILTER_OFF}"
        );
        assert!(
            !FILTER_OFF.contains("stream=debug"),
            "FILTER_OFF must not enable stream at debug; got: {FILTER_OFF}"
        );
        assert!(
            FILTER_OFF.contains("libfreemkv=warn"),
            "FILTER_OFF must keep libfreemkv at warn; got: {FILTER_OFF}"
        );
    }

    // FILTER_ON must surface DEBUG liveness heartbeats, emitted on
    // `target: "freemkv::heartbeat"`. Pins that the target is not excluded.
    #[test]
    fn filter_on_enables_heartbeats() {
        // Heartbeats are emitted on target `freemkv::heartbeat`, which matches
        // the `freemkv=debug` directive (NOT `libfreemkv=debug` — libfreemkv
        // namespaces its events under `freemkv::*`). FILTER_ON must carry it.
        assert!(
            FILTER_ON.contains("freemkv=debug"),
            "FILTER_ON must enable freemkv (and thus freemkv::heartbeat) at debug; got: {FILTER_ON}"
        );
        // And FILTER_OFF must keep them quiet (freemkv=warn).
        assert!(
            FILTER_OFF.contains("freemkv=warn"),
            "FILTER_OFF must keep heartbeats quiet; got: {FILTER_OFF}"
        );
    }

    // End-to-end: under FILTER_ON, a heartbeat event is recorded.
    #[test]
    fn debug_on_shows_heartbeats() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Buf::default();
        let layer = tracing_subscriber::fmt::layer()
            .with_writer(buf.clone())
            .with_ansi(false);
        let filter = EnvFilter::try_new(FILTER_ON).unwrap();
        let subscriber = tracing_subscriber::registry().with(filter).with(layer);

        tracing::subscriber::with_default(subscriber, || {
            // Mimic libfreemkv's heartbeat beat.
            tracing::debug!(
                target: "freemkv::heartbeat",
                phase = "css_crack",
                pos = 1234u64,
                total = 50000u64,
                "alive"
            );
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("alive") && out.contains("css_crack"),
            "FILTER_ON must surface the heartbeat; got:\n{out}"
        );
    }

    // The daemon's events are `freemkv::server::…` now; an operator's
    // `autorip` directive must still reach them, and `freemkv=warn` (for
    // libfreemkv's `freemkv::*` targets) must not swallow them.
    #[test]
    fn autorip_directives_reach_the_server_module_targets() {
        assert_eq!(
            with_server_targets("autorip=debug,libfreemkv=warn"),
            "autorip=debug,freemkv::server=debug,libfreemkv=warn"
        );
        assert_eq!(
            with_server_targets("autorip::ripper=trace"),
            "autorip::ripper=trace,freemkv::server::ripper=trace"
        );
        assert_eq!(with_server_targets("warn"), "warn");
        assert_eq!(with_server_targets("autoripper=info"), "autoripper=info");

        use tracing_subscriber::layer::SubscriberExt;
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Count(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Count {
            fn on_event(
                &self,
                _: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let subscriber = tracing_subscriber::registry()
            .with(EnvFilter::try_new(FILTER_OFF).unwrap())
            .with(Count(hits.clone()));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("a daemon info event under this module's own target");
        });
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "FILTER_OFF must let the daemon's own info events through"
        );
    }

    // `set_debug` must report `false` when init() never ran (no reload
    // handle). Surfaced to callers via the `filter_swapped` JSON field.
    #[test]
    fn set_debug_returns_false_without_init() {
        // RELOAD_HANDLE is a process-wide OnceCell; only assert the
        // negative case when it's genuinely absent (another test may
        // have called `observe::init()` first, making this moot).
        if RELOAD_HANDLE.get().is_none() {
            assert!(!set_debug(true));
            assert!(!set_debug(false));
        }
    }

    // The /api/debug swap on a live reload handle: FILTER_ON lets a debug heartbeat through,
    // FILTER_OFF silences it again.
    #[test]
    fn swap_filter_toggles_debug_events() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tracing_subscriber::layer::SubscriberExt;

        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        struct Count(std::sync::Arc<AtomicUsize>);
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Count {
            fn on_event(
                &self,
                _: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let (filter, handle) = reload::Layer::new(EnvFilter::new(FILTER_OFF));
        let subscriber = tracing_subscriber::registry()
            .with(filter)
            .with(Count(hits.clone()));
        tracing::subscriber::with_default(subscriber, || {
            let beat = || tracing::debug!(target: "freemkv::heartbeat", "alive");
            beat();
            assert_eq!(hits.load(Ordering::SeqCst), 0, "debug is off by default");
            assert!(swap_filter(&handle, true));
            beat();
            assert_eq!(hits.load(Ordering::SeqCst), 1, "FILTER_ON passes debug");
            assert!(swap_filter(&handle, false));
            beat();
            assert_eq!(hits.load(Ordering::SeqCst), 1, "FILTER_OFF silences it");
        });
    }

    // A typo'd AUTORIP_LOG_LEVEL is rejected (so init can say so), a good one parses.
    #[test]
    fn parse_override_rejects_a_bad_directive() {
        assert!(parse_override("autorip=debg").is_err());
        assert!(parse_override("autorip=debug").is_ok());
    }
}
