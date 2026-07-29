//! freemkv — one binary, two shells over the shared `freemkv-engine`:
//!
//! * the **CLI** — the gold-standard `freemkv` command line, replicated 1:1
//!   (`cli_entry` + `pipe`/`info`/`disc_info`/… copied verbatim), and
//! * the native desktop **GUI** (`mac`, AppKit — macOS only) over the shared
//!   `ui`/`engine`/`settings` core.
//!
//! The dispatcher routes a CLI-style invocation (any args, or a bare launch
//! from a terminal) to the CLI shell — byte-for-byte identical to the old
//! `freemkv` binary — and a windowed launch (a `.app` double-click, or an
//! explicit `freemkv gui`) to the desktop shell.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ── CLI shell (the gold-standard freemkv CLI, replicated verbatim) ──────────
mod cli_entry;
mod disc_info;
mod info;
mod keydb_fetch;
mod messaging;
mod output;
mod pipe;
mod strings;

// ── GUI shell — DESKTOP TARGETS ONLY ────────────────────────────────────────
// The shared GUI core (`ui`/`engine`/`settings`/`platform`) and the AppKit
// shell exist only where there is a desktop shell to drive them. On Linux the
// binary is pure CLI (like the historical `freemkv`), so none of this compiles
// in — no dead code, no unused deps, and `clippy -D warnings` stays clean on a
// Linux runner. (The lib target still exposes the core on every platform, so
// CI's portable-core check keeps `ui.rs`/`engine.rs` honest.)
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod engine;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod platform;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod settings;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod ui;

#[cfg(target_os = "macos")]
mod mac;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // A windowed launch opens the desktop shell; everything else is the CLI.
    // On Linux there is no desktop shell, so this whole branch is compiled out
    // and `freemkv` is always the CLI.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if wants_gui(&args) {
        run_gui();
        return;
    }

    // The gold-standard CLI. `cli_entry::run` owns exit codes (it calls
    // `std::process::exit` on every terminal path, exactly as the old `main`
    // did), so a normal return here is the success path.
    cli_entry::run(args);
}

/// Launch the desktop shell for this platform.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_gui() {
    let cfg = settings::Settings::load();

    // Apply the saved interface language before the shell builds anything, so
    // the first string lookup resolves in the right locale. (A later change in
    // Settings switches live via `strings::set_locale`.)
    let code = ui::locale_code(&cfg.language);
    if code == "auto" {
        // "Auto" follows the OS language. A Finder-launched `.app` inherits no
        // LANG, so the crate's env detection would fall back to English —
        // read the macOS preferred language directly instead.
        #[cfg(target_os = "macos")]
        if let Some(sys) = mac::system_locale_code() {
            strings::set_locale(&sys);
        }
    } else {
        strings::set_language(code);
    }

    // Diagnostic logging, mirroring the CLI: only install a tracing subscriber
    // when the user asks for detail; otherwise the library's tracing events are
    // dropped and no log file is written.
    init_gui_logging(&cfg);

    // Development harness (scan / rip / screenshot hooks). Debug builds only —
    // the shipped release binary has no environment switches.
    #[cfg(all(debug_assertions, target_os = "macos"))]
    if dev_harness() {
        return;
    }
    #[cfg(target_os = "macos")]
    mac::run();
    #[cfg(target_os = "windows")]
    eprintln!("the Windows desktop shell is not built yet — run `freemkv <args>` for the CLI");
}

/// Diagnostic-log guard for the GUI (keeps the non-blocking writer alive).
#[cfg(any(target_os = "macos", target_os = "windows"))]
static GUI_LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

/// Install a file tracing subscriber from the GUI's log settings. Only "Verbose"
/// or the "Log debug messages" toggle turn it on (mapping to debug / trace);
/// Quiet and Normal install nothing, exactly like the CLI's common path. The log
/// is written to `log.txt` in the app-support dir, never the terminal.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn init_gui_logging(s: &settings::Settings) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let level = if s.debug_log {
        "trace"
    } else {
        match s.log_level.as_str() {
            "Verbose" => "debug",
            // Quiet / Normal: no diagnostic file log.
            _ => return,
        }
    };

    let dir = settings::support_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let file_appender = tracing_appender::rolling::never(&dir, "log.txt");
    let (nb, guard) = tracing_appender::non_blocking(file_appender);
    let _ = GUI_LOG_GUARD.set(guard);
    let filter = EnvFilter::new(format!("error,freemkv={level},libfreemkv={level}"));
    // try_init: never panic if a subscriber somehow already exists.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_ansi(false).with_writer(nb))
        .try_init();
}

/// True when this invocation should open the desktop UI rather than the CLI:
/// an explicit `freemkv gui`, or a bare launch from inside a macOS `.app`
/// bundle (a Finder double-click passes no arguments). A bare launch from a
/// terminal falls through to the CLI, so `freemkv` alone still prints usage and
/// exits 2 — preserving the CLI contract byte-for-byte.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn wants_gui(args: &[String]) -> bool {
    if args.get(1).map(String::as_str) == Some("gui") {
        return true;
    }
    args.len() < 2 && launched_from_app_bundle()
}

#[cfg(target_os = "macos")]
fn launched_from_app_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.contains(".app/Contents/MacOS/")))
        .unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn launched_from_app_bundle() -> bool {
    // No bundle concept on Windows; the GUI is reached via `freemkv gui` (or a
    // shortcut that passes it). A bare launch stays CLI.
    false
}

/// Dev-only entry points. Returns true when it handled the invocation.
/// macOS debug builds only — it drives the GUI core (`engine`/`settings`/`ui`),
/// which only compiles on desktop targets.
#[cfg(all(debug_assertions, target_os = "macos"))]
fn dev_harness() -> bool {
    // FMKV_FMTS=disc|file lists the output options offered for that source kind.
    if let Ok(k) = std::env::var("FMKV_FMTS") {
        println!("(checked in the UI; see popup_fmt_for) kind={k}");
        return true;
    }

    if let Ok(p) = std::env::var("FMKV_STREAM") {
        match engine::scan_stream(&p) {
            Ok(sc) => {
                println!("label={}  rows={}", sc.label, sc.rows.len());
                for r in &sc.rows {
                    println!(
                        "{}{:<10} {}  pid={:?}",
                        "  ".repeat(r.depth as usize),
                        r.type_s,
                        r.desc,
                        r.pid
                    );
                }
            }
            Err(e) => println!("error: {e}"),
        }
        return true;
    }

    // FMKV_TITLES_DUMP=<iso> — are same-looking titles actually distinct?
    if let Ok(p) = std::env::var("FMKV_TITLES_DUMP") {
        if let Ok((d, _r)) = libfreemkv::scan_iso(std::path::Path::new(&p), Default::default()) {
            println!(
                "{:>3}  {:<14} {:>4}  {:>10}  {:>12}  {:>6}  first-extent",
                "idx", "playlist", "plid", "duration", "size", "strms"
            );
            for (i, t) in d.titles.iter().enumerate() {
                let e = t.extents.first().map(|e| e.start_lba).unwrap_or(0);
                println!(
                    "{:>3}  {:<14} {:>4}  {:>10.1}  {:>12}  {:>6}  {}",
                    i,
                    t.playlist,
                    t.playlist_id,
                    t.duration_secs,
                    t.size_bytes,
                    t.streams.len(),
                    e
                );
            }
        }
        return true;
    }

    if let Ok(p) = std::env::var("FMKV_CAP") {
        if let Ok((d, _r)) = libfreemkv::scan_iso(std::path::Path::new(&p), Default::default()) {
            let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            println!("file       = {sz} bytes");
            println!(
                "capacity   = {} bytes ({} sectors)",
                d.capacity_bytes, d.capacity_sectors
            );
            println!("titles     = {}", d.titles.len());
            let sum: u64 = d.titles.iter().map(|t| t.size_bytes).sum();
            println!("titles sum = {sum} bytes");
            if d.capacity_bytes > sz {
                println!("=> IMAGE IS TRUNCATED by {} bytes", d.capacity_bytes - sz);
            }
            println!("\n idx  size        max-extent-end   beyond-EOF?");
            for (i, t) in d.titles.iter().enumerate().take(30) {
                let end = t
                    .extents
                    .iter()
                    .map(|e| (e.start_lba as u64 + e.sector_count as u64) * 2048)
                    .max()
                    .unwrap_or(0);
                println!(
                    " {:>3}  {:>10}  {:>14}   {}",
                    i,
                    t.size_bytes,
                    end,
                    if end > sz { "YES" } else { "no" }
                );
            }
        }
        return true;
    }

    // FMKV_RIP="<iso> <destdir>" runs the real rip headlessly.
    if let Ok(src) = std::env::var("FMKV_RIP") {
        // Separate vars: paths contain spaces.
        let dst = std::env::var("FMKV_OUT").unwrap_or_else(|_| "/tmp/riptest".into());
        let st = std::sync::Arc::new(engine::RunState::default());
        engine::start_rip(
            engine::RipRequest {
                source: src,
                dest_dir: dst,
                titles: std::env::var("FMKV_TITLES")
                    .ok()
                    .map(|v| v.split(',').filter_map(|x| x.trim().parse().ok()).collect())
                    .unwrap_or_default(),
                format: std::env::var("FMKV_FORMAT")
                    .unwrap_or_else(|_| "Selected titles → MKV".into()),
                audio_pids: std::env::var("FMKV_APIDS")
                    .ok()
                    .map(|v| {
                        v.split(',')
                            .filter(|x| !x.is_empty())
                            .filter_map(|x| x.trim().parse().ok())
                            .collect()
                    })
                    .unwrap_or_default(),
                sub_pids: std::env::var("FMKV_SPIDS")
                    .ok()
                    .map(|v| {
                        v.split(',')
                            .filter(|x| !x.is_empty())
                            .filter_map(|x| x.trim().parse().ok())
                            .collect()
                    })
                    .unwrap_or_default(),
                explicit_streams: std::env::var("FMKV_APIDS").is_ok(),
                raw: std::env::var("FMKV_RAW").is_ok(),
                force: true,
                filename_template: std::env::var("FMKV_TEMPLATE").unwrap_or_default(),
                decrypt_threads: std::env::var("FMKV_THREADS")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0),
                keys: engine::KeyConfig::from_settings(&settings::Settings::load()),
            },
            st.clone(),
        );
        while !st.finished.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(300));
            for l in st.lines.lock().unwrap().drain(..) {
                println!("  {l}");
            }
            let p = *st.prog.lock().unwrap();
            if p.bytes_total > 0 {
                println!(
                    "  {:.0}%  speed={:.1} MB/s  eta={}",
                    p.bytes_done as f64 * 100.0 / p.bytes_total as f64,
                    p.speed_bps as f64 / 1e6,
                    p.eta_secs
                        .map(|e| format!("{}s", e))
                        .unwrap_or_else(|| "—".into())
                );
            }
        }
        for l in st.lines.lock().unwrap().drain(..) {
            println!("  {l}");
        }
        println!("SUMMARY: {}", st.summary.lock().unwrap());
        return true;
    }

    // FMKV_KEYDB="<url> <dest>" exercises the real download+install path.
    if let Ok(a) = std::env::var("FMKV_KEYDB") {
        let mut it = a.splitn(2, ' ');
        let (u, d) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        match settings::update_keydb(u, d) {
            Ok(m) => println!("OK: {m}"),
            Err(e) => println!("ERR: {e}"),
        }
        return true;
    }

    // FMKV_SCAN=<iso> exercises the engine bridge headlessly.
    if let Ok(p) = std::env::var("FMKV_SCAN") {
        match engine::scan_with_keys(
            &p,
            &engine::KeyConfig::from_settings(&settings::Settings::load()),
        ) {
            Ok(sc) => {
                println!(
                    "label={}  titles={}  keys={}",
                    sc.label, sc.title_count, sc.key_summary
                );
                for r in sc.rows.iter().take(14) {
                    println!(
                        "{}{:<10} {}",
                        "  ".repeat(r.depth as usize),
                        r.type_s,
                        r.desc
                    );
                }
                match engine::preflight_with_keys(
                    &p,
                    "/tmp/out",
                    &[],
                    &engine::KeyConfig::from_settings(&settings::Settings::load()),
                ) {
                    Ok(v) if v.is_empty() => println!("preflight: READY"),
                    Ok(v) => println!("preflight blocked: {v:?}"),
                    Err(e) => println!("preflight err: {e}"),
                }
            }
            Err(e) => println!("scan error: {e}"),
        }
        return true;
    }

    false
}
