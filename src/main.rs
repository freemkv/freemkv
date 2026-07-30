//! freemkv — the `freemkv` command: the CLI, plus a way into the desktop GUI.
//!
//! * the **CLI** — the gold-standard `freemkv` command line, replicated 1:1
//!   (`cli_entry` + `pipe`/`info`/`disc_info`/… copied verbatim), and
//! * the native desktop **GUI** — AppKit (`mac`) on macOS, Win32
//!   (`freemkv::windows`) on Windows — over the shared `ui`/`engine`/`settings`
//!   core.
//!
//! The dispatcher routes a CLI-style invocation (any args, or a bare launch
//! from a terminal) to the CLI shell — byte-for-byte identical to the old
//! `freemkv` binary — and a windowed launch (a `.app` double-click, or an
//! explicit `freemkv gui`) to the desktop shell. The decision itself is
//! `freemkv::app_entry::wants_gui`, where it can be unit-tested.
//!
//! **Windows note.** This image is console-subsystem and stays the CLI on a
//! bare launch, because there is nothing to distinguish an Explorer
//! double-click from a `cmd` invocation. The double-clickable image on Windows
//! is the sibling binary `freemkv-gui.exe` (`src/bin/freemkv-gui.rs`), which is
//! windows-subsystem and opens the same shell directly.

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

// ── GUI shell — macOS ───────────────────────────────────────────────────────
// The shared GUI core (`ui`/`engine`/`settings`/`platform`) and the AppKit
// shell are compiled into this binary only where AppKit is. On Linux the
// binary is pure CLI (like the historical `freemkv`), so none of this compiles
// in — no dead code, no unused deps, and `clippy -D warnings` stays clean on a
// Linux runner. (The lib target still exposes the core on every platform, so
// CI's portable-core check keeps `ui.rs`/`engine.rs` honest.)
//
// Windows is NOT here: its shell lives in the lib (`freemkv::win_app`) so the
// second binary, `freemkv-gui.exe`, can open the same window. Declaring
// `mod windows` here as well would compile those 4.4k lines a second time into
// every binary.
#[cfg(target_os = "macos")]
mod engine;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
mod platform;
#[cfg(target_os = "macos")]
mod settings;
#[cfg(target_os = "macos")]
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // A windowed launch opens the desktop shell; everything else is the CLI.
    // On Linux there is no desktop shell, so this whole branch is compiled out
    // and `freemkv` is always the CLI.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    if freemkv::app_entry::wants_gui(&args, launched_windowed()) {
        run_gui();
        return;
    }

    // The gold-standard CLI. `cli_entry::run` owns exit codes (it calls
    // `std::process::exit` on every terminal path, exactly as the old `main`
    // did), so a normal return here is the success path.
    cli_entry::run(args);
}

/// Launch the desktop shell for this platform.
///
/// macOS builds it here (the AppKit shell is a module of this binary); Windows
/// hands off to the lib, which is where the Win32 shell lives so that
/// `freemkv-gui.exe` can open the very same window.
#[cfg(target_os = "macos")]
fn run_gui() {
    let cfg = settings::Settings::load();

    // "Auto" follows the OS language. A Finder-launched `.app` inherits no
    // LANG, so the i18n crate's env detection would fall back to English —
    // `apply_locale` reads the OS preferred language instead.
    freemkv::app_entry::apply_locale(&cfg.language, mac::system_locale_code);

    // Diagnostic logging, mirroring the CLI: only install a tracing subscriber
    // when the user asks for detail; otherwise the library's tracing events are
    // dropped and no log file is written.
    freemkv::app_entry::init_gui_logging(&cfg.log_level);

    // Development harness (scan / rip / screenshot hooks). Debug builds only —
    // the shipped release binary has no environment switches.
    #[cfg(debug_assertions)]
    if dev_harness() {
        return;
    }
    mac::run();
}

#[cfg(target_os = "windows")]
fn run_gui() {
    freemkv::win_app::run();
}

/// Was this image started *as a window*, with no argument to say so?
///
/// macOS: a Finder double-click runs the binary from inside the `.app` bundle
/// and passes no arguments, so the path is the only evidence there is.
#[cfg(target_os = "macos")]
fn launched_windowed() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.contains(".app/Contents/MacOS/")))
        .unwrap_or(false)
}

/// Windows: never. This image is console-subsystem, and an Explorer
/// double-click of it is indistinguishable from a `cmd` invocation — guessing
/// (parent process, console ownership) would make the CLI contract depend on
/// how the terminal spawned it. The windowed image is the separate
/// `freemkv-gui.exe`, which does not come through this dispatcher at all.
#[cfg(target_os = "windows")]
fn launched_windowed() -> bool {
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
                max_passes: std::env::var("FMKV_MAXPASSES")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0),
                multipass: std::env::var("FMKV_MAXPASSES")
                    .ok()
                    .and_then(|v| v.trim().parse::<u32>().ok())
                    .is_some_and(|n| n > 0),
                abort_lost_secs: std::env::var("FMKV_ABORTLOST")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0),
                keep_iso: std::env::var("FMKV_KEEPISO").is_ok(),
                // Dev harness: default off so a headless rip never pops the tray
                // unless asked (FMKV_EJECT set).
                auto_eject: std::env::var("FMKV_EJECT").is_ok(),
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
            std::env::var("FMKV_VERBOSE").is_ok(),
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
