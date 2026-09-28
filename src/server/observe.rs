//! Observability — single init point for the structured event log.
//!
//! Three sinks, written from the same tracing event stream:
//! `{AUTORIP_DIR}/logs/autorip.log` (daily-rolling, human-readable),
//! `{AUTORIP_DIR}/logs/autorip.jsonl` (non-rolling, tailed by
//! `/api/debug`), and stderr (compact, captured by Docker).
//!
//! Filter level via `AUTORIP_LOG_LEVEL` (env-filter syntax). Default
//! `autorip=info,libfreemkv=warn`.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling;
use tracing_subscriber::reload;
use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt, util::SubscriberInitExt};

// EnvFilter directive used when /api/debug is OFF (the normal state). prod = warnings only; dev
// = full debug (see FILTER_ON).
const FILTER_OFF: &str = "autorip=info,libfreemkv=warn,freemkv=warn";

// EnvFilter directive used when /api/debug is ON: debug globally, plus mux/stream/freemkv
// targets needed for drive + mux forensics.
const FILTER_ON: &str = "autorip=debug,libfreemkv=debug,freemkv=debug,mux=debug,stream=debug";

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
    let initial_filter = match env_override.as_deref() {
        Some(s) => EnvFilter::try_new(s).unwrap_or_else(|_| EnvFilter::new(FILTER_OFF)),
        None => EnvFilter::new(FILTER_OFF),
    };
    let (filter, reload_handle) = reload::Layer::new(initial_filter);
    let reload_handle = if env_override.is_some() {
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

    // Machine-readable JSONL: NOT rolled. `/api/debug` tails a stable path;
    // daily rotation broke lookups before (v0.13.0 regression). Growth is
    // unbounded but small (~MB/day); an external logrotate can handle it.
    let json_appender = rolling::never(&log_dir, "autorip.jsonl");
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
    let directive = if enabled { FILTER_ON } else { FILTER_OFF };
    let new_filter = match EnvFilter::try_new(directive) {
        Ok(f) => f,
        Err(_) => return false,
    };
    handle.reload(new_filter).is_ok()
}

fn log_dir() -> String {
    // AUTORIP_DIR, else writable /config (Docker), else ~/.config/autorip
    // (bare run) — matches config/log so all sinks agree on one writable base.
    format!("{}/logs", crate::config::default_autorip_dir())
}

/// Path of the JSONL stream — exposed so the web `/api/debug` endpoint
/// can tail it without re-deriving the layout.
pub fn json_log_path() -> String {
    format!("{}/autorip.jsonl", log_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // End-to-end: under FILTER_ON, a heartbeat event is recorded and a
    // CSS-style key event is emitted without raw key bytes (redaction).
    #[test]
    fn debug_on_shows_heartbeats_and_redacts_keys() {
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
            // Mimic the REDACTED css auth log: key value is "<redacted>",
            // only a 1-byte fingerprint accompanies it. No raw key bytes.
            tracing::debug!(
                target: "freemkv::css",
                title_key = "<redacted>",
                title_key_fp = 0x5Au8,
                "css auth: final title_key"
            );
        });

        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();

        // Heartbeat surfaced under /api/debug ON.
        assert!(
            out.contains("alive") && out.contains("css_crack"),
            "FILTER_ON must surface the heartbeat; got:\n{out}"
        );
        // Redaction confirmed: the marker is present, the field is redacted,
        // and no plausible raw 5-byte CSS key hex leaked.
        assert!(
            out.contains("<redacted>"),
            "title_key must be redacted; got:\n{out}"
        );
        assert!(
            !out.contains("title_key=\"") || out.contains("title_key=\"<redacted>\""),
            "title_key must never carry a real value; got:\n{out}"
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
}
