//! The daemon entry: `freemkv server [--bootstrap | --healthcheck | serve]`.

use super::{
    REBOOT, SHUTDOWN, TERMINATE, VERSION_LABEL, config, keysource, log, mover, muxer, observe,
    ripper, web,
};

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// Stop bounds (rips, then the other services, then web). Each step is a cancel resumed next
// start, so a bound only lets it land: a rip gets one drive recovery read (60 s) so its ISO
// is not cut mid-write; the services, cancelling since the stop request, get 30 s more.
const RIP_DRAIN_TIMEOUT: Duration = Duration::from_secs(60);
const SERVICE_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const WEB_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
// Each of the two in-progress marker sweeps (staging may sit on a share that stopped answering).
const MARKER_CLEAR_TIMEOUT: Duration = Duration::from_secs(3);
// EX_TEMPFAIL: the service manager (launchd KeepAlive) starts the daemon again.
const RESTART_EXIT_CODE: i32 = 75;
// Slack past the drain bounds for the stop's own tail (log writes, the relaunch).
const RELAUNCH_SLACK: Duration = Duration::from_secs(15);
// Hard bound on any stop (116 s from the request): past it the process exits, whatever still
// runs. Our service managers allow longer than this plus STOP_LOG_GRACE (pinned by a test),
// so this deadline, not their SIGKILL, governs.
const STOP_DEADLINE: Duration = Duration::from_secs(
    RIP_DRAIN_TIMEOUT.as_secs()
        + SERVICE_DRAIN_TIMEOUT.as_secs()
        + WEB_JOIN_TIMEOUT.as_secs()
        + 2 * MARKER_CLEAR_TIMEOUT.as_secs()
        + RELAUNCH_SLACK.as_secs(),
);
// How long the deadline's last log line may take before the exit goes ahead without it.
const STOP_LOG_GRACE: Duration = Duration::from_secs(3);
// Cadence of the KEYDB refresh and log prune threads, in one-second SHUTDOWN polls.
const DAILY: Duration = Duration::from_secs(24 * 3600);

/// Run the server shell. `argv` is everything after `freemkv server`.
///
/// Every terminal path either exits the process (`--healthcheck`,
/// `--version`, `--help`, an unknown argument) or returns once the daemon has
/// drained after SIGTERM/SIGINT, or, after a reboot request, replaces itself
/// (exec, Linux) or exits 75 for the service manager to start it again.
pub fn run(argv: Vec<String>) {
    // Before anything else: once a package upgrade replaces the binary, /proc/self/exe
    // no longer names the path to exec.
    let relaunch_info = Relaunch::capture();
    // v0.25.7: tiny built-in subcommands so the image doesn't need curl or a
    // separate entrypoint script. Each exits before observe::init so they
    // don't spam the tracing sinks on every 30-second healthcheck.
    let bootstrap = match argv.first().map(String::as_str) {
        Some("--healthcheck") => {
            std::process::exit(run_healthcheck());
        }
        Some("--version") | Some("-V") => {
            println!("freemkv server {}", VERSION_LABEL);
            std::process::exit(0);
        }
        Some("--help") | Some("-h") => {
            println!(
                "freemkv server {} — automated optical-disc rip service\n\n\
                 Usage:\n  \
                   freemkv server                  Run the daemon (bare — config under $AUTORIP_DIR, else /config, else ./config beside the binary)\n  \
                   freemkv server serve            Same as no-arg: run the daemon without container bootstrap\n  \
                   freemkv server --bootstrap      Initialize container env (NFS mount), then run the daemon\n  \
                   freemkv server --healthcheck    Probe http://127.0.0.1:$PORT/api/state (exit 0/1)\n  \
                   freemkv server --version        Print version and exit",
                VERSION_LABEL
            );
            std::process::exit(0);
        }
        Some("--bootstrap") => true,
        // Bare run (no Docker): daemon without container bootstrap, config
        // defaults under ~/.config/autorip. `serve` is an explicit alias.
        Some("serve") | None => false,
        Some(other) => {
            eprintln!("freemkv server: unknown argument '{other}' (try --help)");
            std::process::exit(2);
        }
    };

    // Before bootstrap: as Docker PID 1 a default-disposition SIGTERM is ignored.
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
    }
    // Armed before anything that can block: from the first stop request (signal, quit, reboot)
    // the process is gone within STOP_DEADLINE, whatever thread is stuck where.
    let progress = Arc::new(StopProgress::default());
    progress.start("startup and drive polling");
    if let Err(e) = spawn_stop_deadline(
        STOP_DEADLINE,
        || SHUTDOWN.load(Ordering::Acquire),
        progress.clone(),
        stop_exit_code,
        |line| {
            log::syslog(&line);
            observe::flush_before_exit();
        },
        exit_now,
    ) {
        eprintln!("freemkv server: could not start the stop deadline: {e}");
    }
    if bootstrap {
        // Errors are logged but non-fatal. Container-init is Linux-only;
        // elsewhere the daemon runs directly.
        #[cfg(unix)]
        run_bootstrap();
        #[cfg(not(unix))]
        eprintln!("freemkv server: --bootstrap is Linux-only; running the daemon directly");
    }

    // Panic hook FIRST — before observe::init, so a panic during tracing
    // setup still hits post-mortem handling. tracing::error! is a no-op
    // before init, but log::syslog still records, so it's still useful.
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("<non-string panic>");
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        // Both: structured event for the JSONL stream (greppable post-mortem)
        // AND the legacy syslog line so the per-device file + UI keep working.
        tracing::error!(thread = %thread, location = %loc, message = %msg, "panic");
        log::syslog(&format!("PANIC in thread '{thread}' at {loc}: {msg}"));
    }));

    // Tracing — sets up stderr + autorip.log + autorip.jsonl sinks. Filter
    // via AUTORIP_LOG_LEVEL (default `autorip=info,libfreemkv=warn`).
    observe::init();

    // Rotate the system log if it has grown large across restarts. Also
    // re-checked on the log-prune tick below, bounding a long-uptime daemon too.
    log::rotate_system_log_if_large();

    log::syslog(&format!(
        "autorip starting (v{}, edition 2024)",
        VERSION_LABEL
    ));
    tracing::info!(
        version = VERSION_LABEL,
        target = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "autorip starting"
    );

    // Load config
    let cfg = config::load();

    // Folder warnings come from the health monitor's first check, off the startup path.
    if let Some(msg) =
        keysource::keyserver_url_startup_warning(&cfg.read().unwrap_or_else(|e| e.into_inner()))
    {
        log::syslog(&msg);
    }

    // The local KEYDB only matters for the `local` key source. In `online`
    // mode keys come from the key service and a local keydb would only shadow
    // it (libfreemkv default-search), so skip the download entirely.
    let online_keys = keysource::uses_online(&cfg.read().unwrap_or_else(|e| e.into_inner()));

    // Ensure KEYDB exists — download on first boot if URL is configured
    if online_keys {
        log::syslog("Online key source — skipping local KEYDB download");
    } else if keysource::keydb_exists(&cfg.read().unwrap_or_else(|e| e.into_inner())) {
        log::syslog("KEYDB found");
    } else {
        let url = cfg
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keydb_url
            .clone();
        if !url.is_empty() {
            log::syslog("KEYDB not found, downloading...");
            // Route through the address guard (validate_fetch_url + pinned
            // resolver): LAN hosts are fine, unreachable addresses are refused.
            fetch_keydb(&cfg, &url, true);
        }
    }

    // Start mover thread. Joined on shutdown so an in-flight file move
    // isn't truncated into a partial OUTPUT_DIR file.
    let mover_handle = std::thread::spawn({
        let cfg = cfg.clone();
        move || mover::run(&cfg)
    });

    // Start mux worker thread — pipelines mux behind the drive so a disc can
    // rip on one device while a prior title muxes in the background. Joined
    // on shutdown so an in-flight mux isn't killed mid-write (truncated MKV).
    let muxer_handle = std::thread::spawn({
        let cfg = cfg.clone();
        move || muxer::run(&cfg)
    });

    // Library remux worker (plus its auditor). It yields the mux slot to any
    // rip and is joined on shutdown so a cancelled remux cleans its partial.
    let library_handle = crate::server::library::start(&cfg);
    let health_handle = crate::server::health::start(&cfg);

    // Start web server thread
    let web_handle = std::thread::spawn({
        let cfg = cfg.clone();
        move || web::run(&cfg)
    });

    // Start KEYDB auto-update thread — single source of truth for periodic
    // refresh. Pre-0.13 a cron entry also spawned a second binary that raced
    // this thread for /dev/sg* and port 8080; that path was removed.
    let keydb_handle = std::thread::spawn({
        let cfg2 = cfg.clone();
        move || {
            tracing::info!("keydb update thread starting (24h interval)");
            loop {
                if crate::server::sleep_unless_shutdown(DAILY) {
                    break;
                }
                // Online key source resolves out-of-band; no local keydb to keep
                // fresh (and refreshing one would only shadow the service).
                let (online, url) = {
                    let c = cfg2.read().unwrap_or_else(|e| e.into_inner());
                    (keysource::uses_online(&c), c.keydb_url.clone())
                };
                if online || url.is_empty() {
                    continue;
                }
                tracing::info!(url_origin = %crate::server::webhook::webhook_url_origin(&url), "keydb: starting daily update");
                // SSRF-guarded fetch (see keysource::refresh_keydb) — the daily
                // refresh must not bypass the address allow-list that the
                // settings save and manual update already enforce.
                fetch_keydb(&cfg2, &url, false);
            }
            tracing::info!("keydb update thread stopping");
        }
    });

    // Log prune thread — replaces the v0.25.5 cron-based cleanup. retention_days
    // comes from the Settings UI and is re-read each tick so a saved update
    // takes effect on the next run without a restart.
    let log_prune_handle = std::thread::spawn({
        let cfg = cfg.clone();
        move || {
            tracing::info!("log prune thread starting (24h interval)");
            loop {
                // Prune first, then wait: a daemon restarted more often than daily
                // would otherwise never reach a tick.
                log_prune_tick(&cfg);
                if crate::server::sleep_unless_shutdown(DAILY) {
                    break;
                }
            }
            tracing::info!("log prune thread stopping");
        }
    });

    // Main loop: poll drives (checks SHUTDOWN flag internally)
    ripper::drive_poll_loop(&cfg);

    SHUTDOWN.store(true, Ordering::Release);
    let workers = vec![
        ("mover", Some(mover_handle)),
        ("muxer", Some(muxer_handle)),
        ("library", library_handle),
        ("health", health_handle),
        ("keydb", Some(keydb_handle)),
        ("log-prune", Some(log_prune_handle)),
    ];
    let staging = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .staging_dir
        .clone();
    progress.finish("startup and drive polling");
    stop_services(
        Path::new(&staging),
        ripper::join_all_rip_threads,
        workers,
        Some(web_handle),
        &Bounds {
            rips: RIP_DRAIN_TIMEOUT,
            services: SERVICE_DRAIN_TIMEOUT,
            web: WEB_JOIN_TIMEOUT,
        },
        &progress,
    );
    if relaunch_requested() {
        relaunch(&relaunch_info);
    }
}

struct Bounds {
    rips: Duration,
    services: Duration,
    web: Duration,
}

// Graceful stop, bounded; whether everything finished. Markers are cleared first (and again
// for a late rip) so a SIGKILL mid-drain leaves resumable dirs. A component that does not
// stop in time is left running, never joined, and named in `progress` and the last log line.
fn stop_services(
    staging: &Path,
    join_rips: impl FnOnce(Duration) -> bool,
    workers: Vec<(&'static str, Option<JoinHandle<()>>)>,
    web: Option<JoinHandle<()>>,
    bounds: &Bounds,
    progress: &StopProgress,
) -> bool {
    let clear_markers = |progress: &StopProgress| {
        let staging = staging.to_path_buf();
        run_bounded(
            "staging markers",
            MARKER_CLEAR_TIMEOUT,
            progress,
            move || ripper::staging::clear_inprogress_markers(&staging),
        )
    };
    let mut done = clear_markers(progress);
    // Rips and services share one budget from here (the marker sweep has its own): time the
    // rips leave goes to the services.
    let services_deadline = Instant::now() + bounds.rips + bounds.services;
    progress.start("rip threads");
    for (name, _) in &workers {
        progress.start(name);
    }
    progress.start("web");
    progress.start("http handlers");
    if join_rips(bounds.rips) {
        progress.finish("rip threads");
    } else {
        done = false;
    }
    for (name, handle) in workers {
        let left = services_deadline.saturating_duration_since(Instant::now());
        done &= progress.finish_if(name, join_bounded(handle, name, left));
    }
    let web_deadline = Instant::now() + bounds.web;
    done &= progress.finish_if("web", join_bounded(web, "web", bounds.web));
    let left = web_deadline.saturating_duration_since(Instant::now());
    done &= progress.finish_if("http handlers", web::wait_idle(left));
    done &= clear_markers(progress);
    if done {
        log::syslog("autorip stopped");
    } else {
        log::syslog(&format!(
            "autorip stopped before every service drained (not stopped: {}); unfinished work is interrupted",
            progress.pending()
        ));
    }
    done
}

/// What the stop sequence is still waiting on, named for the log.
#[derive(Default)]
struct StopProgress(Mutex<Vec<&'static str>>);

impl StopProgress {
    fn start(&self, name: &'static str) {
        let mut pending = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !pending.contains(&name) {
            pending.push(name);
        }
    }

    fn finish(&self, name: &str) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|n| *n != name);
    }

    // `finish` when `stopped`; passes `stopped` through.
    fn finish_if(&self, name: &str, stopped: bool) -> bool {
        if stopped {
            self.finish(name);
        }
        stopped
    }

    // Never blocks: the deadline reads this while the stop sequence may be anywhere.
    fn pending(&self) -> String {
        match self.0.try_lock() {
            Ok(p) if p.is_empty() => "nothing (the exit itself stalled)".into(),
            Ok(p) => p.join(", "),
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner().join(", "),
            Err(std::sync::TryLockError::WouldBlock) => "unknown".into(),
        }
    }
}

// Run `f` on its own thread and wait up to `timeout`; one still running then (blocked in the
// kernel on a dead share, say) is left behind, never joined, and stays named in `progress`.
fn run_bounded(
    name: &'static str,
    timeout: Duration,
    progress: &StopProgress,
    f: impl FnOnce() + Send + 'static,
) -> bool {
    progress.start(name);
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("stop-{name}").replace(' ', "-"))
        .spawn(move || {
            f();
            let _ = tx.send(());
        });
    if spawned.is_err() {
        return false;
    }
    let ok = rx.recv_timeout(timeout).is_ok();
    if !ok {
        tracing::warn!(
            step = name,
            "did not finish within timeout; stopping without it"
        );
    }
    progress.finish_if(name, ok)
}

// A reboot was requested and no stop signal has overridden it.
fn relaunch_requested() -> bool {
    REBOOT.load(Ordering::Acquire) && !TERMINATE.load(Ordering::Acquire)
}

// The binary and full argv (argv[0] included) the daemon was started with.
struct Relaunch {
    exe: std::io::Result<PathBuf>,
    argv: Vec<OsString>,
}

impl Relaunch {
    fn capture() -> Self {
        Self {
            exe: std::env::current_exe(),
            argv: std::env::args_os().collect(),
        }
    }
}

// The exit code to use when `exec` did not replace the process: 0 if a stop signal
// arrived meanwhile, else 75 for the service manager.
fn relaunch_with(
    relaunch: &Relaunch,
    exec: impl FnOnce(&Path, &[OsString]) -> std::io::Error,
) -> i32 {
    if TERMINATE.load(Ordering::Acquire) {
        return 0;
    }
    let exe = match (&relaunch.exe, relaunch.argv.is_empty()) {
        (Ok(exe), false) => exe,
        _ => {
            log::syslog(
                "autorip reboot: startup binary unknown; exiting 75 for the service manager",
            );
            return RESTART_EXIT_CODE;
        }
    };
    let e = exec(exe, &relaunch.argv);
    if e.kind() == std::io::ErrorKind::Unsupported {
        log::syslog("autorip reboot: exiting 75 for the service manager to restart");
    } else {
        log::syslog(&format!(
            "autorip reboot: not re-executed ({e}); exiting 75 for the service manager to restart"
        ));
    }
    RESTART_EXIT_CODE
}

// Replace the process image in place; returns only on failure. Every fd is CLOEXEC.
#[cfg(all(unix, not(target_os = "macos")))]
fn exec_in_place(exe: &Path, argv: &[OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let Some((arg0, rest)) = argv.split_first() else {
        return std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty argv");
    };
    std::process::Command::new(exe).arg0(arg0).args(rest).exec()
}

// macOS: process exit is the sure way to drop the drive's IOKit exclusive access.
#[cfg(not(all(unix, not(target_os = "macos"))))]
fn exec_in_place(_: &Path, _: &[OsString]) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "relaunch is left to the service manager on this platform",
    )
}

fn relaunch(relaunch: &Relaunch) -> ! {
    log::syslog("autorip reboot: relaunching");
    observe::flush_before_exit();
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::process::exit(relaunch_with(relaunch, exec_in_place))
}

// The exit code of a stop: 75 for a reboot nothing vetoed (the service manager starts us
// again), else 0, as a plain stop that drained.
fn stop_exit_code() -> i32 {
    if relaunch_requested() {
        RESTART_EXIT_CODE
    } else {
        0
    }
}

// The hard stop deadline: once `stopping()` holds, wait `after`, log what is still pending
// (`log`, given at most STOP_LOG_GRACE), then `exit(code())`, the code decided then. Nothing
// is joined: a thread blocked in the kernel on a dead share must not hold the exit.
fn spawn_stop_deadline(
    after: Duration,
    stopping: impl Fn() -> bool + Send + 'static,
    progress: Arc<StopProgress>,
    code: impl FnOnce() -> i32 + Send + 'static,
    log: impl FnOnce(String) + Send + 'static,
    exit: impl FnOnce(i32) + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("stop-deadline".into())
        .spawn(move || {
            while !stopping() {
                std::thread::sleep(Duration::from_millis(100));
            }
            std::thread::sleep(after);
            let code = code();
            let line = format!(
                "autorip stop: not finished {}s after the stop request; still not stopped: {}; exiting now (code {code})",
                after.as_secs(),
                progress.pending()
            );
            let (tx, rx) = std::sync::mpsc::channel();
            let logger = std::thread::Builder::new()
                .name("stop-deadline-log".into())
                .spawn(move || {
                    log(line);
                    let _ = tx.send(());
                });
            if logger.is_ok() {
                let _ = rx.recv_timeout(STOP_LOG_GRACE);
            }
            exit(code);
        })
}

// Exit without running atexit handlers or taking any lock a wedged thread may hold.
fn exit_now(code: i32) {
    #[cfg(unix)]
    unsafe {
        libc::_exit(code)
    };
    #[cfg(not(unix))]
    std::process::exit(code);
}

// Fetch KEYDB from `url` (`keysource::refresh_keydb`) and log the outcome. `boot` picks the
// first-boot download's log wording over the daily refresh's.
fn fetch_keydb(cfg: &std::sync::RwLock<config::Config>, url: &str, boot: bool) {
    use keysource::KeydbError;
    let (saved, save_failed, too_large, read_failed, fetch_failed) = if boot {
        (
            "KEYDB downloaded",
            "KEYDB save failed",
            "KEYDB download failed: response exceeded size limit",
            "KEYDB download read failed",
            "KEYDB download failed",
        )
    } else {
        (
            "KEYDB updated",
            "KEYDB update failed",
            "KEYDB daily update: response exceeded size limit",
            "KEYDB daily update: response read failed",
            "KEYDB update failed",
        )
    };
    match keysource::refresh_keydb(cfg, url, crate::server::web::KEYDB_TRANSFER_BUDGET) {
        Ok(r) => log::syslog(&format!(
            "{saved}: {} entries -> {}",
            r.entries,
            r.path.display()
        )),
        Err(KeydbError::Save(e)) => log::syslog(&format!("{save_failed}: {e}")),
        Err(KeydbError::TooLarge) => log::syslog(too_large),
        Err(KeydbError::Read) => log::syslog(read_failed),
        Err(e) => log::syslog(&format!(
            "{fetch_failed} for {}: {e}",
            crate::server::webhook::webhook_url_origin(url)
        )),
    }
}

// Join `handle` (`None` counts as joined), giving up after `timeout` so a wedged
// worker can't pin shutdown; on timeout it is detached. Polls `is_finished`, as std
// has no join-with-timeout.
fn join_bounded(handle: Option<JoinHandle<()>>, name: &str, timeout: Duration) -> bool {
    let Some(handle) = handle else {
        return true;
    };
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            tracing::warn!(
                thread = name,
                "did not drain within timeout; stopping without it"
            );
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = handle.join();
    true
}

#[cfg(unix)]
extern "C" fn handle_signal(_sig: libc::c_int) {
    // A reboot also sets SHUTDOWN, so only a prior stop signal counts as the second signal.
    if TERMINATE.swap(true, Ordering::AcqRel) {
        unsafe { libc::_exit(1) };
    }
    // Release on the store / Acquire on the load so the flag is reliably
    // visible to the main loop's shutdown poll on weakly-ordered targets
    // (aarch64 container hosts).
    SHUTDOWN.store(true, Ordering::Release);
}

// The port the server binds for this `PORT` value (same rule as `config::load`), so the probe
// hits the port the daemon actually listens on.
fn healthcheck_port(raw: Option<&str>) -> u16 {
    raw.and_then(crate::server::config::parse_port_env)
        .unwrap_or_else(crate::server::config::default_port)
}

// Probe the local HTTP API and exit 0 (healthy) or 1 (unhealthy).
fn run_healthcheck() -> i32 {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let port = healthcheck_port(std::env::var("PORT").ok().as_deref());
    let addr: SocketAddr = match format!("127.0.0.1:{port}").parse() {
        Ok(a) => a,
        Err(_) => return 1,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(s) => s,
        Err(_) => return 1,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    // Minimal HTTP/1.1 request — no Host header niceties required by
    // tiny_http for the /api/state endpoint to respond.
    let req = b"GET /api/state HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(req).is_err() {
        return 1;
    }

    // A single read() isn't guaranteed to return the full 12-byte status
    // line (a short first TCP segment would falsely report unhealthy).
    // Loop until enough bytes, EOF, or the 2s read timeout fires.
    const STATUS_LEN: usize = "HTTP/1.1 200".len();
    let mut buf = [0u8; 64];
    let mut filled = 0usize;
    while filled < STATUS_LEN {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break, // EOF before a full status line
            Ok(n) => filled += n,
            Err(_) => return 1,
        }
    }
    let line = &buf[..filled];
    if line.starts_with(b"HTTP/1.1 200") || line.starts_with(b"HTTP/1.0 200") {
        0
    } else {
        1
    }
}

// Container bootstrap — replaces the v0.25.5 entrypoint.sh (drops bash, shadow, and the shell
// scripts). Linux-only; container-init concerns.
#[cfg(unix)]
fn run_bootstrap() {
    let autorip_dir = std::env::var("AUTORIP_DIR").unwrap_or_else(|_| "/config".to_string());
    // RIP_USER is interpolated raw into /etc/passwd, /etc/group and the KEYDB
    // symlink path; a newline or colon could corrupt the account database.
    // Validate against a conservative username shape, else fall back to default.
    let rip_user = match std::env::var("RIP_USER") {
        Ok(u) if is_valid_username(&u) => u,
        Ok(u) => {
            eprintln!(
                "bootstrap: RIP_USER {u:?} is not a valid username (^[a-z_][a-z0-9_-]{{0,31}}$); using 'autorip'"
            );
            "autorip".to_string()
        }
        Err(_) => "autorip".to_string(),
    };

    // Working directories
    for sub in ["logs", "freemkv"] {
        let p = format!("{autorip_dir}/{sub}");
        if let Err(e) = std::fs::create_dir_all(&p) {
            eprintln!("bootstrap: mkdir {p}: {e}");
        }
    }
    if let Err(e) = std::fs::create_dir_all("/staging") {
        eprintln!("bootstrap: mkdir /staging: {e}");
    }

    // User creation (no useradd — append to /etc/passwd + /etc/group).
    // Idempotent: skip if a line already starts with the username. Only
    // runs at uid 0; the container needs root for SCSI + mount(2) anyway.
    if unsafe { libc::getuid() } == 0 {
        ensure_user_entry(&rip_user);
        if let Err(e) = chown_recursive(std::path::Path::new("/staging"), &rip_user) {
            eprintln!("bootstrap: chown /staging: {e}");
        }
        if let Err(e) = chown_recursive(std::path::Path::new(&autorip_dir), &rip_user) {
            eprintln!("bootstrap: chown {autorip_dir}: {e}");
        }
    }

    // Symlink for KEYDB lookup path
    let freemkv_cfg = format!("/home/{rip_user}/.config/freemkv");
    if let Some(parent) = std::path::Path::new(&freemkv_cfg).parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("bootstrap: mkdir {}: {e}", parent.display());
    }
    if let Err(e) = link_keydb_dir(
        std::path::Path::new(&format!("{autorip_dir}/freemkv")),
        std::path::Path::new(&freemkv_cfg),
    ) {
        eprintln!("bootstrap: symlink {freemkv_cfg}: {e}");
    }

    // Snapshot env for the udev-triggered rip-on-insert path. udev-trigger.sh
    // sources this file, so a raw newline in a value (e.g. a bad TMDB_API_KEY)
    // could inject a line. Single-quote each value, escaping embedded quotes.
    if let Err(e) = write_env_snapshot(std::path::Path::new("/etc/autorip.env"), std::env::vars()) {
        eprintln!("bootstrap: write /etc/autorip.env: {e}");
    }

    // udev rule (the kernel's udev daemon runs on the host; container
    // sees disc-insert events via the shared /dev mount + udev-trigger.sh
    // calling our HTTP API)
    if let Err(e) = std::fs::create_dir_all("/etc/udev/rules.d") {
        eprintln!("bootstrap: mkdir /etc/udev/rules.d: {e}");
    }
    let udev_rule = "ACTION==\"change\", SUBSYSTEM==\"block\", KERNEL==\"sr[0-9]*\", \
                     ENV{ID_CDROM_MEDIA}==\"1\", ENV{ID_CDROM_MEDIA_STATE}!=\"blank\", \
                     RUN+=\"/usr/local/bin/udev-trigger.sh %k\"\n";
    if let Err(e) = std::fs::write("/etc/udev/rules.d/99-autorip.rules", udev_rule) {
        eprintln!("bootstrap: write udev rule: {e}");
    }

    // In-container NFS mount (v0.25.4 feature, kept). When NFS_HOST is
    // unset this is a no-op and the operator's docker-compose volumes:
    // line is the source of truth instead.
    if let Some(share) = nfs_share() {
        if let Err(e) = std::fs::create_dir_all(&share.mountpoint) {
            eprintln!(
                "bootstrap: cannot create NFS mountpoint {}: {e}",
                share.mountpoint
            );
        }
        if !is_mountpoint(&share.mountpoint) {
            mount_nfs(&share);
        } else {
            eprintln!("bootstrap: {} already mounted, skipping", share.mountpoint);
        }
    }
}

/// The in-container NFS share, when the operator configured one.
#[cfg(unix)]
pub(crate) struct NfsShare {
    pub source: String,
    pub mountpoint: String,
    pub opts: String,
}

#[cfg(unix)]
pub(crate) fn nfs_share() -> Option<NfsShare> {
    let (host, export, mountpoint) = (
        std::env::var("NFS_HOST").ok()?,
        std::env::var("NFS_EXPORT").ok()?,
        std::env::var("NFS_MOUNTPOINT").ok()?,
    );
    if host.is_empty() || export.is_empty() || mountpoint.is_empty() {
        return None;
    }
    // Default keeps `hard` (no silent I/O errors) but adds `retry=1` and
    // a bounded wait on mount, so an unreachable server degrades to an empty
    // mountpoint instead of stalling. Overridable via NFS_OPTS.
    let opts = std::env::var("NFS_OPTS")
        .unwrap_or_else(|_| "vers=4.1,nconnect=4,nolock,actimeo=3,hard,retry=1,_netdev".into());
    Some(NfsShare {
        source: format!("{host}:{export}"),
        mountpoint,
        opts,
    })
}

#[cfg(unix)]
pub(crate) fn mount_nfs(share: &NfsShare) -> bool {
    eprintln!(
        "bootstrap: mounting {} -> {} ({})",
        share.source, share.mountpoint, share.opts
    );
    let child = std::process::Command::new("/sbin/mount.nfs4")
        .arg("-o")
        .arg(&share.opts)
        .arg(&share.source)
        .arg(&share.mountpoint)
        .spawn();
    match child {
        Ok(child) => match wait_bounded(child, std::time::Duration::from_secs(30)) {
            Some(s) if s.success() => {
                eprintln!("bootstrap: NFS mount OK");
                true
            }
            Some(s) => {
                eprintln!(
                    "bootstrap: NFS mount FAILED ({s}); {} stays unmounted",
                    share.mountpoint
                );
                false
            }
            None => {
                eprintln!(
                    "bootstrap: NFS mount TIMED OUT after 30s (server unreachable?); {} stays unmounted",
                    share.mountpoint
                );
                false
            }
        },
        Err(e) => {
            eprintln!("bootstrap: NFS mount FAILED to spawn ({e})");
            false
        }
    }
}

/// Mount the share afresh: drop the current mount first when `mounted` (its handles went
/// stale), else just mount it (an earlier mount failed or timed out). A share cannot be
/// mounted over itself first, so writers are held meanwhile by the health checks, which
/// read the mount table live and report a share that is not there as unhealthy.
#[cfg(unix)]
pub(crate) fn remount_nfs(share: &NfsShare, mounted: bool) -> bool {
    if !mounted {
        return mount_nfs(share);
    }
    // Lazy: a stale mount may still have open files, which a plain umount refuses over.
    let gone = std::process::Command::new("/bin/umount")
        .arg("-l")
        .arg(&share.mountpoint)
        .spawn()
        .ok()
        .and_then(|c| wait_bounded(c, std::time::Duration::from_secs(15)))
        .is_some_and(|s| s.success());
    if !gone {
        eprintln!("health: umount -l {} failed", share.mountpoint);
        return false;
    }
    mount_nfs(share)
}

// Point `link` at `target`, replacing a stale symlink or file. A real directory at `link`
// (e.g. a legacy keydb volume) is never deleted: an empty one is replaced, a populated one is
// left alone and reported.
#[cfg(unix)]
fn link_keydb_dir(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(link) {
        if meta.is_dir() {
            std::fs::remove_dir(link)?;
        } else {
            std::fs::remove_file(link)?;
        }
    }
    std::os::unix::fs::symlink(target, link)
}

// Write the udev-trigger env snapshot to `path`, mode 0600 (it carries `TMDB_API_KEY`).
#[cfg(unix)]
fn write_env_snapshot(
    path: &std::path::Path,
    vars: impl Iterator<Item = (String, String)>,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    for (k, v) in vars {
        if matches!(
            k.as_str(),
            "TMDB_API_KEY"
                | "STAGING_DIR"
                | "OUTPUT_DIR"
                | "MOVIE_DIR"
                | "TV_DIR"
                | "MIN_LENGTH"
                | "MAIN_FEATURE"
                | "AUTO_EJECT"
                | "ON_INSERT"
                | "ABORT_ON_ERROR"
                | "AUTORIP_DIR"
                | "PORT"
                | "KEYDB_PATH"
                | "AUTORIP_LOG_LEVEL"
        ) {
            writeln!(f, "{k}={}", shell_single_quote(&v))?;
        }
    }
    // `mode` applies only on creation; tighten a pre-existing 0644 file too.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
}

// Wrap a value in single quotes for safe inclusion in a POSIX-shell
// `KEY=value` line that will be `.`-sourced. Embedded quotes use the
// standard `'\''` idiom, so the result is always exactly one shell token.
fn shell_single_quote(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('\'');
    for c in v.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// Validate a Unix username against `^[a-z_][a-z0-9_-]{0,31}$` — the
// conservative POSIX-portable shape. Rejects colon/newline (would corrupt
// /etc/passwd or /etc/group when interpolated) and empty/overlong values.
fn is_valid_username(user: &str) -> bool {
    let mut chars = user.chars();
    let Some(first) = chars.next() else {
        return false; // empty
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    if user.len() > 32 {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(unix)]
fn ensure_user_entry(user: &str) {
    use std::io::Write;
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    if !passwd.lines().any(|l| l.starts_with(&format!("{user}:")))
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open("/etc/passwd")
    {
        let _ = writeln!(f, "{user}:x:1000:1000::/home/{user}:/bin/sh");
    }
    let group = std::fs::read_to_string("/etc/group").unwrap_or_default();
    if !group.lines().any(|l| l.starts_with(&format!("{user}:")))
        && let Ok(mut f) = std::fs::OpenOptions::new().append(true).open("/etc/group")
    {
        let _ = writeln!(f, "{user}:x:1000:");
    }
}

#[cfg(unix)]
fn chown_recursive(path: &std::path::Path, user: &str) -> std::io::Result<()> {
    use std::ffi::CString;
    let c_user =
        CString::new(user).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // Look up uid/gid. With a freshly-written /etc/passwd line above
    // we know uid=gid=1000, but resolving keeps this honest if the
    // entry was already there with different IDs.
    let pwd = unsafe { libc::getpwnam(c_user.as_ptr()) };
    if pwd.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("getpwnam({user}) failed"),
        ));
    }
    let uid = unsafe { (*pwd).pw_uid };
    let gid = unsafe { (*pwd).pw_gid };

    fn lchown_path(p: &std::path::Path, uid: libc::uid_t, gid: libc::gid_t) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        if unsafe { libc::lchown(c_path.as_ptr(), uid, gid) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn walk(p: &std::path::Path, uid: libc::uid_t, gid: libc::gid_t) -> std::io::Result<()> {
        // lchown the entry itself (does NOT follow a symlink target — the
        // deliberate choice this fn already made).
        lchown_path(p, uid, gid)?;
        // Recurse only into REAL directories: entry.file_type() doesn't follow
        // symlinks, so a symlink-to-dir is treated as a leaf — this stops a
        // symlink from steering the walk (and chown) outside the intended tree.
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    walk(&entry.path(), uid, gid)?;
                } else {
                    // Files and symlinks: lchown the entry, never descend.
                    lchown_path(&entry.path(), uid, gid)?;
                }
            }
        }
        Ok(())
    }
    walk(path, uid, gid)
}

/// Strip trailing slashes from a mount path for comparison, preserving a
/// bare "/". So "/mnt/nfs/" and "/mnt/nfs" compare equal.
fn normalize_mount_path(s: &str) -> &str {
    let t = s.trim_end_matches('/');
    if t.is_empty() { "/" } else { t }
}

// Wait for `child` to exit, but give up after `timeout` and kill it so an
// unreachable NFS server can't block bootstrap for the full mount-retry
// window. Returns `Some(status)` if exited in time, `None` if killed.
fn wait_bounded(
    mut child: std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => return None,
        }
    }
}

fn is_mountpoint(path: &str) -> bool {
    listed_in(&mount_table().unwrap_or_default(), path)
}

/// This process's mount table (the `/proc/mounts` format); `None` where there is none to read.
pub(crate) fn mount_table() -> Option<String> {
    std::fs::read_to_string("/proc/self/mounts").ok()
}

/// Whether `table` lists a mount at `path`. Trailing slashes are ignored, so an
/// NFS_MOUNTPOINT of "/mnt/nfs/" still matches "/mnt/nfs" (else mount.nfs4 runs
/// against an already-mounted dir, which can hang on a hard mount).
pub(crate) fn listed_in(table: &str, path: &str) -> bool {
    let want = normalize_mount_path(path);
    table
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .any(|mp| normalize_mount_path(&unescape_mount_path(mp)) == want)
}

// The kernel writes space, tab, newline and backslash in a mount path as \ooo octal.
pub(crate) fn unescape_mount_path(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let code = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match code {
            Some(d) if b[i] == b'\\' => {
                out.push(
                    d.iter()
                        .fold(0u8, |n, c| n.wrapping_mul(8).wrapping_add(c - b'0')),
                );
                i += 4;
            }
            _ => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// One log-maintenance pass: the mtime-based prune, then a re-check of the live system log
// (which that prune can't reclaim while it is still being written).
fn log_prune_tick(cfg: &std::sync::RwLock<crate::server::config::Config>) {
    let (log_dir, retention_days) = {
        let c = cfg.read().unwrap_or_else(|e| e.into_inner());
        (c.log_dir(), c.log_retention_days)
    };
    if !log_dir.is_empty() {
        prune_old_logs(&log_dir, retention_days);
    }
    log::rotate_system_log_if_large();
}

// Delete `.log` files under `log_dir` older than `retention_days`. Replaces
// the v0.25.5 cron-based cleanup (no cron daemon needed). Single-shot; the
// caller drives the daily cadence.
fn prune_old_logs(log_dir: &str, retention_days: u64) {
    // `retention_days * 86_400` can overflow u64 for an absurd value
    // (silent wraparound in release could delete fresh logs). Guard
    // both the multiply and the subtraction.
    let cutoff = retention_days.checked_mul(86_400).and_then(|secs| {
        std::time::SystemTime::now().checked_sub(std::time::Duration::from_secs(secs))
    });
    let Some(cutoff) = cutoff else {
        return;
    };
    // The tracing daily appender holds `autorip.log.<today>` (UTC) open; if the
    // daemon logged nothing for > retention_days its mtime can fall before the
    // cutoff, so never prune the active appender file (see active_log_filenames).
    let active = active_log_filenames(&crate::server::util::format_date());
    // Recurse so the archive subdir (logs/rips/, where archive_device_log
    // writes per-rip files — the dir that actually grows over time) is
    // pruned too, not just the top-level live logs.
    let pruned = prune_dir_recursive(std::path::Path::new(log_dir), cutoff, &active);
    if pruned > 0 {
        log::syslog(&format!(
            "log prune: removed {pruned} files older than {retention_days}d from {log_dir}"
        ));
    }
}

/// The log filenames the tracing appenders are actively writing to, which
/// retention must never delete out from under an open FD. The human log rolls
/// daily as `autorip.log.<UTC-date>`; its bare base is included for the not-yet-
/// rolled case. (`autorip.jsonl` is non-rolling and already excluded by
/// `is_prunable_log_name`.) `today` is a parameter (rather than read here via
/// `format_date()`) so a test can inject a fixed date.
fn active_log_filenames(today: &str) -> Vec<String> {
    vec!["autorip.log".to_string(), format!("autorip.log.{today}")]
}

// Whether a filename is one of the log files retention applies to.
fn is_prunable_log_name(path: &std::path::Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name.ends_with(".log") || name.contains(".log.")
}

// Recursively delete log files under `dir` older than `cutoff` (descends into
// subdirs, e.g. logs/rips/), returning the count removed. IO errors on
// entries are swallowed — pruning is best-effort, must never break the daemon.
fn prune_dir_recursive(
    dir: &std::path::Path,
    cutoff: std::time::SystemTime,
    active: &[String],
) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut pruned = 0u32;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            pruned += prune_dir_recursive(&path, cutoff, active);
            continue;
        }
        if !is_prunable_log_name(&path) {
            continue;
        }
        // Never delete the file an appender currently holds open.
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| active.iter().any(|a| a == n))
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if mtime < cutoff && std::fs::remove_file(&path).is_ok() {
            pruned += 1;
        }
    }
    pruned
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
