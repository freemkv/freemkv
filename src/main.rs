//! freemkv — the `freemkv` command: the CLI, plus a way into the desktop GUI.
//!
//! * the **CLI** — the gold-standard `freemkv` command line, replicated 1:1
//!   (`cli_entry` + `pipe`/`info`/`disc_info`/… copied verbatim), and
//! * the native desktop **GUI** — AppKit (`mac`) on macOS, Win32
//!   (`freemkv::windows`) on Windows — over the shared `ui`/`engine`/`settings`
//!   core.
//!
//! Two builds of this one binary ship, both named `freemkv`. The CLI build has no desktop shell
//! at all; the app build (`--features gui`) opens the window on a bare launch and runs the CLI for
//! any arguments, via `freemkv::app_entry::wants_gui`. A third build, the server
//! (`--features server`), adds `freemkv server`: the unattended rip daemon of the container image
//! (`freemkv::server`); every other invocation is the CLI.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// ── CLI shell (the gold-standard freemkv CLI, replicated verbatim) ──────────
mod cli_entry;
mod disc_capture;
// Also declared in `lib.rs`: `pipe` (here) and `engine` (GUI) must report the
// same wording for a copy that recovered less than the whole disc.
mod disc_copy_verdict;
mod disc_info;
// Also declared in `lib.rs`: both shells must refuse a rip whose destination
// IS its source, and comparing paths alone (instead of this) missed hardlinks.
mod file_identity;
mod info;
mod keydb_fetch;
// Also declared in `lib.rs`: the CLI and GUI each rendered half of what
// `MuxOutcome` carried, and the half neither rendered was the byte loss.
mod lossy;
mod messaging;
mod output;
mod pipe;
// Also declared in `lib.rs`: the CLI and the GUI route container sources by one table.
mod sources;
mod strings;
// Also declared in `lib.rs`: `pipe` (here) and `engine` (GUI) both re-scan
// between picking a title and muxing it, and need ONE shared answer type.
mod title_identity;

// ── GUI shell — macOS ───────────────────────────────────────────────────────
// Compiles into the app build only where AppKit is; Windows' shell lives in
// the lib (`freemkv::win_app`) instead, reused by the windowed image.
#[cfg(all(feature = "gui", target_os = "macos"))]
mod engine;
// The KU fixture `engine`'s tests use.
#[cfg(all(test, feature = "gui", target_os = "macos"))]
mod ku_fixture;
#[cfg(all(feature = "gui", target_os = "macos"))]
mod mac;
#[cfg(all(feature = "gui", target_os = "macos"))]
mod platform;
#[cfg(all(feature = "gui", target_os = "macos"))]
mod settings;
#[cfg(all(feature = "gui", target_os = "macos"))]
mod ui;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Server build only: `freemkv server ...` runs the daemon; its arguments are
    // the daemon's own (`--bootstrap`, `--healthcheck`, ...), not the CLI's.
    #[cfg(feature = "server")]
    if args.get(1).map(String::as_str) == Some("server") {
        freemkv::server::run(args[2..].to_vec());
        return;
    }

    // App build only: a bare launch opens the desktop shell; everything else
    // is the CLI. Linux is glibc only (the musl CLI has no GTK4).
    #[cfg(all(
        feature = "gui",
        any(
            target_os = "macos",
            target_os = "windows",
            all(target_os = "linux", target_env = "gnu")
        )
    ))]
    if freemkv::app_entry::wants_gui(&args, display_available()) {
        // `freemkv --lang de gui`: the flag picks this run's language, as it does for the CLI.
        let lang = freemkv::app_entry::set_launch_language(&args);
        run_gui(lang);
        return;
    }

    // The gold-standard CLI. `cli_entry::run` owns exit codes (it calls
    // `std::process::exit` on every terminal path, exactly as the old `main`
    // did), so a normal return here is the success path.
    cli_entry::run(args);
}

// Launch the desktop shell: macOS builds it here (AppKit is a module of
// this binary); Windows hands off to the windowed image.
#[cfg(all(feature = "gui", target_os = "macos"))]
fn run_gui(_launch_language: bool) {
    let (cfg, loaded) = settings::Settings::load_reporting();

    // "Auto" follows the OS language. A Finder-launched `.app` inherits no
    // LANG, so the i18n crate's env detection would fall back to English —
    // `apply_locale` reads the OS preferred language instead.
    freemkv::app_entry::apply_locale(&cfg.language, mac::system_locale_code);

    // Diagnostic logging, mirroring the CLI: only install a tracing subscriber
    // when the user asks for detail; otherwise the library's tracing events are
    // dropped and no log file is written.
    freemkv::app_entry::init_gui_logging(&cfg.log_level);
    // Again, now that there is somewhere for it to go: the subscriber is
    // configured from `cfg.log_level`, so the warning `load_reporting` already
    // emitted had no subscriber to receive it.
    loaded.warn();

    // Development harness (scan / rip / screenshot hooks). Debug builds only —
    // the shipped release binary has no environment switches.
    #[cfg(debug_assertions)]
    if dev_harness() {
        return;
    }
    mac::run();
}

// Shipped as `freemkv.com`, which cmd/PowerShell resolve first (PATHEXT). A window from it
// would keep the console attached, so start the windowed sibling and return the prompt;
// in-process when none is found, or for a launch `--lang` the argless sibling cannot receive.
#[cfg(all(feature = "gui", target_os = "windows"))]
fn run_gui(launch_language: bool) {
    let Ok(me) = std::env::current_exe() else {
        return freemkv::win_app::run();
    };
    if launch_language {
        return freemkv::win_app::run();
    }
    let windowed = freemkv::app_entry::windowed_candidates(&me)
        .into_iter()
        .find(|p| p.is_file());
    match windowed {
        Some(p) if std::process::Command::new(&p).spawn().is_ok() => {}
        _ => freemkv::win_app::run(),
    }
}

// GTK's status is the process's: a GTK/libadwaita init failure must not exit 0.
#[cfg(all(feature = "gui", target_os = "linux", target_env = "gnu"))]
fn run_gui(_launch_language: bool) {
    let code = freemkv::linux_app::run();
    if code != 0 {
        std::process::exit(code);
    }
}

// Can a window be drawn? Only Linux can lack one: over SSH, a bare `freemkv`
// from the app build must print usage like the CLI build, not fail in GTK.
#[cfg(all(feature = "gui", any(target_os = "macos", target_os = "windows")))]
fn display_available() -> bool {
    true
}

#[cfg(all(feature = "gui", target_os = "linux", target_env = "gnu"))]
fn display_available() -> bool {
    freemkv::app_entry::display_present(std::env::var_os)
}

/// Dev-only entry points. Returns true when it handled the invocation.
/// macOS debug builds only — it drives the GUI core (`engine`/`settings`/`ui`),
/// which only compiles on desktop targets.
#[cfg(all(feature = "gui", debug_assertions, target_os = "macos"))]
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
                // The headless harness never saw a scan, so it has no
                // identities to promise — which leaves the engine's selection
                // check inert, exactly as this path behaved before it existed.
                title_ids: Vec::new(),
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
                // No per-title breakdown from this harness: the union above
                // applies to every title, which is what it always did.
                title_pids: engine::TitleStreams::Unspecified,
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
        // `Acquire` pairs with `engine::start_rip`'s `Release` store. Locks below
        // recover from poison instead of unwrapping, else a panicking worker's
        // message gets buried when it also kills the reader thread.
        while !st.finished.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(300));
            for l in st.lines.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
                println!("  {l}");
            }
            let p = *st.prog.lock().unwrap_or_else(|e| e.into_inner());
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
        for l in st.lines.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
            println!("  {l}");
        }
        println!("SUMMARY: {}", st.summary_now());
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
