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
        fn on_event(&self, _: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
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
        fn on_event(&self, _: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
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
