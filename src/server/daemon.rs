//! The daemon entry: `freemkv server [--bootstrap | --healthcheck | serve]`.

use super::{SHUTDOWN, VERSION_LABEL, config, keysource, log, mover, muxer, observe, ripper, web};

use std::sync::atomic::Ordering;

/// Run the server shell. `argv` is everything after `freemkv server`.
///
/// Every terminal path either exits the process (`--healthcheck`,
/// `--version`, `--help`, an unknown argument) or returns once the daemon has
/// drained after SIGTERM/SIGINT.
pub fn run(argv: Vec<String>) {
    // v0.25.7: tiny built-in subcommands so the image doesn't need curl or a
    // separate entrypoint script. Each exits before observe::init so they
    // don't spam the tracing sinks on every 30-second healthcheck.
    match argv.first().map(String::as_str) {
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
                   freemkv server                  Run the daemon (bare — config under ~/.config/autorip)\n  \
                   freemkv server serve            Same as no-arg: run the daemon without container bootstrap\n  \
                   freemkv server --bootstrap      Initialize container env (NFS mount), then run the daemon\n  \
                   freemkv server --healthcheck    Probe http://127.0.0.1:$PORT/api/state (exit 0/1)\n  \
                   freemkv server --version        Print version and exit",
                VERSION_LABEL
            );
            std::process::exit(0);
        }
        Some("--bootstrap") => {
            // Bootstrap then fall through to the daemon below. Errors are
            // logged but non-fatal. Container-init is Linux-only; elsewhere
            // this is a no-op and the daemon runs directly.
            #[cfg(unix)]
            run_bootstrap();
            #[cfg(not(unix))]
            eprintln!("freemkv server: --bootstrap is Linux-only; running the daemon directly");
        }
        // Bare run (no Docker): daemon without container bootstrap, config
        // defaults under ~/.config/autorip. `serve` is an explicit alias.
        Some("serve") => {}
        Some(other) => {
            eprintln!("freemkv server: unknown argument '{other}' (try --help)");
            std::process::exit(2);
        }
        None => {}
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

    // Signal handler for graceful shutdown
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

    // Fail-loud-EARLY destination check: warn if a configured movie/tv/output
    // dir is missing/not writable (e.g. a lost NAS bind-mount). Non-blocking —
    // finished rips stay in staging meanwhile — but surfaces the problem at boot.
    {
        let c = cfg.read().unwrap_or_else(|e| e.into_inner());
        for (root, reason) in mover::check_configured_destinations(&c) {
            log::syslog(&format!(
                "WARNING: configured destination '{root}' is not usable at startup: {reason}. \
                 Finished rips will be PRESERVED in staging (not moved) until this is fixed \
                 (check the directory exists and its bind-mount/NAS share is present and writable)."
            ));
        }
        if let Some(msg) = keysource::keyserver_url_startup_warning(&c) {
            log::syslog(&msg);
        }
    }

    // The local KEYDB only matters for the `local` key source. In `online`
    // mode keys come from the key service and a local keydb would only shadow
    // it (libfreemkv default-search), so skip the download entirely.
    let online_keys = cfg.read().unwrap_or_else(|e| e.into_inner()).key_source == "online";

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
            // Route through the SSRF guard (validate_fetch_url + pinned
            // resolver) — a bare ureq::get here would let an operator-set
            // keydb_url reach loopback / RFC1918 / cloud-metadata.
            match web::guarded_get(&url) {
                Ok(resp) => {
                    match web::read_capped_keydb_body(
                        resp.into_body().into_reader(),
                        web::KEYDB_MAX_BYTES,
                    ) {
                        Ok(buf) => {
                            let saved = keysource::save_keydb(&cfg, &buf);
                            match saved {
                                Ok(r) => log::syslog(&format!(
                                    "KEYDB downloaded: {} entries -> {}",
                                    r.entries,
                                    r.path.display()
                                )),
                                Err(e) => log::syslog(&format!("KEYDB save failed: {e}")),
                            }
                        }
                        Err(web::KeydbReadError::TooLarge) => {
                            log::syslog("KEYDB download failed: response exceeded size limit")
                        }
                        Err(web::KeydbReadError::Io) => log::syslog("KEYDB download read failed"),
                    }
                }
                Err(e) => log::syslog(&format!(
                    "KEYDB download failed for {}: {e}",
                    crate::server::webhook::webhook_url_origin(&url)
                )),
            }
        }
    }

    // Start mover thread. Joined on shutdown (see end of main) so an
    // in-flight file move isn't truncated into a partial OUTPUT_DIR file.
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

    // Start web server thread
    let _web_handle = std::thread::spawn({
        let cfg = cfg.clone();
        move || web::run(&cfg)
    });

    // Start KEYDB auto-update thread — single source of truth for periodic
    // refresh. Pre-0.13 a cron entry also spawned a second binary that raced
    // this thread for /dev/sg* and port 8080; that path was removed.
    let _keydb_handle = std::thread::spawn({
        let cfg2 = cfg.clone();
        move || {
            tracing::info!("keydb update thread starting (24h interval)");
            'outer: loop {
                // 24h sleep in 1s chunks so SHUTDOWN is observed within ~1s.
                for _ in 0..(24 * 3600) {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    if SHUTDOWN.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                }
                // Online key source resolves out-of-band; no local keydb to keep
                // fresh (and refreshing one would only shadow the service).
                let (online, url) = {
                    let c = cfg2.read().unwrap_or_else(|e| e.into_inner());
                    (c.key_source == "online", c.keydb_url.clone())
                };
                if online || url.is_empty() {
                    continue;
                }
                tracing::info!(url_origin = %crate::server::webhook::webhook_url_origin(&url), "keydb: starting daily update");
                // SSRF-guarded fetch (see web::guarded_get) — the daily
                // refresh must not bypass the address allow-list that the
                // settings save and manual update already enforce.
                match web::guarded_get(&url) {
                    Ok(resp) => {
                        match web::read_capped_keydb_body(
                            resp.into_body().into_reader(),
                            web::KEYDB_MAX_BYTES,
                        ) {
                            Ok(buf) => {
                                let saved = keysource::save_keydb(&cfg2, &buf);
                                match saved {
                                    Ok(r) => log::syslog(&format!(
                                        "KEYDB updated: {} entries -> {}",
                                        r.entries,
                                        r.path.display()
                                    )),
                                    Err(e) => log::syslog(&format!("KEYDB update failed: {e}")),
                                }
                            }
                            Err(web::KeydbReadError::TooLarge) => {
                                log::syslog("KEYDB daily update: response exceeded size limit")
                            }
                            Err(web::KeydbReadError::Io) => {
                                log::syslog("KEYDB daily update: response read failed")
                            }
                        }
                    }
                    Err(e) => log::syslog(&format!(
                        "KEYDB update failed for {}: {e}",
                        crate::server::webhook::webhook_url_origin(&url)
                    )),
                }
            }
            tracing::info!("keydb update thread stopping");
        }
    });

    // Log prune thread — replaces the v0.25.5 cron-based cleanup. retention_days
    // comes from the Settings UI and is re-read each tick so a saved update
    // takes effect on the next run without a restart.
    let _log_prune_handle = std::thread::spawn({
        let cfg = cfg.clone();
        move || {
            tracing::info!("log prune thread starting (24h interval)");
            'outer: loop {
                // Wait first; on a fresh container the logs dir has only
                // a few minutes of data and pruning is a no-op anyway.
                for _ in 0..(24 * 3600) {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    if SHUTDOWN.load(Ordering::Relaxed) {
                        break 'outer;
                    }
                }
                let (log_dir, retention_days) = {
                    let c = cfg.read().unwrap_or_else(|e| e.into_inner());
                    (c.log_dir(), c.log_retention_days)
                };
                // Re-check the live system log too — the mtime-based prune
                // above can't reclaim a file still being written.
                log::rotate_system_log_if_large();
                if log_dir.is_empty() {
                    continue;
                }
                prune_old_logs(&log_dir, retention_days);
            }
            tracing::info!("log prune thread stopping");
        }
    });

    // Main loop: poll drives (checks SHUTDOWN flag internally)
    ripper::drive_poll_loop(&cfg);

    // Graceful shutdown is NOT a failure: clear in-progress markers up front
    // so the next start resumes cleanly. Robust even if the drain below is
    // SIGKILLed mid-drain by docker's stop-grace — markers are gone by then.
    {
        let c = cfg.read().unwrap_or_else(|e| e.into_inner());
        ripper::staging::clear_inprogress_markers(std::path::Path::new(&c.staging_dir));
    }

    // Drain any rip threads still mid-flight so we don't exit while
    // libfreemkv holds a SCSI session. Bounded so a stuck drive can't
    // pin shutdown indefinitely.
    ripper::join_all_rip_threads(std::time::Duration::from_secs(60));

    // Drain the mover and muxer too: both loop on SHUTDOWN and return after
    // the current unit, so joining avoids a truncated file or partial MKV.
    // Bounded so a wedged NFS write or stuck mux can't pin shutdown forever.
    join_bounded(mover_handle, "mover", std::time::Duration::from_secs(120));
    join_bounded(muxer_handle, "muxer", std::time::Duration::from_secs(120));
    join_bounded(
        library_handle,
        "library",
        std::time::Duration::from_secs(120),
    );

    log::syslog("autorip stopped");
}

// Join `handle`, giving up after `timeout` so a wedged worker can't pin
// shutdown. Polls `is_finished` (no join-with-timeout in std); worker is
// expected to observe SHUTDOWN, with the timeout as a stuck-I/O backstop.
fn join_bounded(handle: std::thread::JoinHandle<()>, name: &str, timeout: std::time::Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !handle.is_finished() {
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                thread = name,
                "did not drain within timeout; exiting anyway"
            );
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let _ = handle.join();
}

#[cfg(unix)]
extern "C" fn handle_signal(_sig: libc::c_int) {
    if SHUTDOWN.load(Ordering::Acquire) {
        // Second signal — force exit
        unsafe { libc::_exit(1) };
    }
    // Release on the store / Acquire on the load so the flag is reliably
    // visible to the main loop's shutdown poll on weakly-ordered targets
    // (aarch64 container hosts).
    SHUTDOWN.store(true, Ordering::Release);
}

// Probe the local HTTP API and exit 0 (healthy) or 1 (unhealthy).
fn run_healthcheck() -> i32 {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
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
    use std::io::Write;
    use std::os::unix::fs::symlink;

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
    let _ = std::fs::remove_file(&freemkv_cfg);
    let _ = std::fs::remove_dir_all(&freemkv_cfg);
    if let Err(e) = symlink(format!("{autorip_dir}/freemkv"), &freemkv_cfg) {
        eprintln!("bootstrap: symlink {freemkv_cfg}: {e}");
    }

    // Snapshot env for the udev-triggered rip-on-insert path. udev-trigger.sh
    // sources this file, so a raw newline in a value (e.g. a bad TMDB_API_KEY)
    // could inject a line. Single-quote each value, escaping embedded quotes.
    if let Ok(mut f) = std::fs::File::create("/etc/autorip.env") {
        for (k, v) in std::env::vars() {
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
                let _ = writeln!(f, "{k}={}", shell_single_quote(&v));
            }
        }
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
    if let (Ok(host), Ok(export), Ok(mountpoint)) = (
        std::env::var("NFS_HOST"),
        std::env::var("NFS_EXPORT"),
        std::env::var("NFS_MOUNTPOINT"),
    ) && !host.is_empty()
        && !export.is_empty()
        && !mountpoint.is_empty()
    {
        // Default keeps `hard` (no silent I/O errors) but adds `retry=1` and
        // a bounded wait below, so an unreachable server at startup degrades
        // to an empty mountpoint instead of stalling. Overridable via NFS_OPTS.
        let opts = std::env::var("NFS_OPTS")
            .unwrap_or_else(|_| "vers=4.1,nconnect=4,nolock,actimeo=3,hard,retry=1,_netdev".into());
        if let Err(e) = std::fs::create_dir_all(&mountpoint) {
            eprintln!("bootstrap: cannot create NFS mountpoint {mountpoint}: {e}");
        }
        if !is_mountpoint(&mountpoint) {
            let source = format!("{host}:{export}");
            eprintln!("bootstrap: mounting {source} -> {mountpoint} ({opts})");
            let child = std::process::Command::new("/sbin/mount.nfs4")
                .arg("-o")
                .arg(&opts)
                .arg(&source)
                .arg(&mountpoint)
                .spawn();
            match child {
                Ok(child) => match wait_bounded(child, std::time::Duration::from_secs(30)) {
                    Some(s) if s.success() => eprintln!("bootstrap: NFS mount OK"),
                    Some(s) => eprintln!(
                        "bootstrap: NFS mount FAILED ({s}); container will start with empty {mountpoint}"
                    ),
                    None => eprintln!(
                        "bootstrap: NFS mount TIMED OUT after 30s (server unreachable?); \
                             container will start with empty {mountpoint}"
                    ),
                },
                Err(e) => eprintln!("bootstrap: NFS mount FAILED to spawn ({e})"),
            }
        } else {
            eprintln!("bootstrap: {mountpoint} already mounted, skipping");
        }
    }
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
    // Normalize trailing slashes: an operator-set NFS_MOUNTPOINT of "/mnt/nfs/"
    // must still match "/mnt/nfs" in /proc/mounts, else mount.nfs4 runs
    // against an already-mounted dir (can hang on a hard mount).
    let want = normalize_mount_path(path);
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    mounts
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1))
        .any(|mp| normalize_mount_path(mp) == want)
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
mod tests {
    use super::*;

    // FIX: on a long-uptime daemon the system log only shrank at boot, so it
    // grew unbounded between restarts. Pins that the log-prune tick (24h,
    // same thread as `prune_old_logs`) also re-checks the rotation size.
    #[test]
    fn log_prune_thread_rechecks_system_log_rotation() {
        let src = crate::server::util::source_lf(include_str!("daemon.rs"));
        let start = src
            .find("Log prune thread")
            .expect("daemon.rs should have the log prune thread");
        let end = src[start..]
            .find("Main loop: poll drives")
            .map(|i| start + i)
            .expect("daemon.rs should have the main poll loop after the prune thread");
        assert!(
            src[start..end].contains("rotate_system_log_if_large()"),
            "the log-prune tick must re-check the system log's rotation size, \
             not only at startup"
        );
    }

    // Log retention has to see ROLLED files: `tracing-appender`'s daily
    // rotation writes `autorip.log.YYYY-MM-DD`, whose `Path::extension()`
    // is the date, so an `extension() == "log"` check would skip every rolled file.

    #[test]
    fn a_rolled_daily_log_is_prunable() {
        use std::path::Path;
        assert!(
            is_prunable_log_name(Path::new("/l/autorip.log.2026-05-01")),
            "the rolled daily is the file that actually accumulates"
        );
        assert!(is_prunable_log_name(Path::new("/l/autorip.log")));
        assert!(is_prunable_log_name(Path::new("/l/device_sg0.log")));
        assert!(is_prunable_log_name(Path::new(
            "/l/rips/2026-05-01_disc.log"
        )));
    }

    /// The jsonl is NOT swept up by the widened match. Its unbounded growth is
    /// a separate decision, recorded where it is made, and quietly deleting it
    /// here would take `GET /api/debug`'s history with it.
    #[test]
    fn the_jsonl_is_not_caught_by_the_widened_match() {
        use std::path::Path;
        assert!(!is_prunable_log_name(Path::new("/l/autorip.jsonl")));
        assert!(!is_prunable_log_name(Path::new(
            "/l/autorip.jsonl.2026-05-01"
        )));
        assert!(!is_prunable_log_name(Path::new("/l/notes.txt")));
    }

    #[test]
    fn valid_usernames_accepted() {
        for u in [
            "autorip",
            "rip",
            "_svc",
            "a",
            "rip-user_1",
            "abcdefghijklmnopqrstuvwxyz012345",
        ] {
            assert!(is_valid_username(u), "{u:?} should be valid");
        }
    }

    #[test]
    fn invalid_usernames_rejected() {
        for u in [
            "",                                  // empty
            "1rip",                              // leading digit
            "-rip",                              // leading dash
            "Rip",                               // uppercase
            "rip:x",                             // colon (passwd injection)
            "rip\nroot:x:0:0",                   // newline injection
            "abcdefghijklmnopqrstuvwxyz0123456", // 33 chars, too long
            "rip user",                          // space
        ] {
            assert!(!is_valid_username(u), "{u:?} should be rejected");
        }
    }

    #[test]
    fn shell_single_quote_wraps_and_escapes() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        assert_eq!(shell_single_quote("a b"), "'a b'");
        // Newline stays inside the single quotes — cannot start a new line.
        assert_eq!(shell_single_quote("a\nb"), "'a\nb'");
        // Embedded single quote uses the '\'' idiom.
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        // Shell metacharacters are inert inside single quotes.
        assert_eq!(shell_single_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn normalize_mount_path_trims_trailing_slash() {
        assert_eq!(normalize_mount_path("/mnt/nfs/"), "/mnt/nfs");
        assert_eq!(normalize_mount_path("/mnt/nfs"), "/mnt/nfs");
        assert_eq!(normalize_mount_path("/mnt/nfs///"), "/mnt/nfs");
        assert_eq!(normalize_mount_path("/"), "/");
        assert_eq!(normalize_mount_path("///"), "/");
    }

    // Backdating a file's mtime needs libc::utimes, a cfg(unix)-only dep not
    // linked on Windows. autorip only rips on Linux, so this is exercised
    // there; the Windows build just needs to compile.
    #[cfg(unix)]
    #[test]
    fn prune_recurses_into_subdirs_and_only_touches_old_logs() {
        // Repo-local scratch, never /tmp (wiped on reboot; remove_dir_all
        // cleanup is skipped if the test is killed). Anchor to the crate's
        // own target/ dir so artifacts are cleaned by `cargo clean`.
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!("autorip-prune-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let rips = d.join("rips");
        std::fs::create_dir_all(&rips).unwrap();

        // Old archived log in the subdir (the dir that actually grows).
        let old = rips.join("sg0_old.log");
        std::fs::write(&old, b"x").unwrap();
        // A non-.log file in the subdir must be left alone.
        let keep_nonlog = rips.join("notes.txt");
        std::fs::write(&keep_nonlog, b"x").unwrap();
        // A fresh top-level log must survive a cutoff in the past.
        let fresh = d.join("device_sg0.log");
        std::fs::write(&fresh, b"x").unwrap();

        // Backdate the archived log well past the cutoff.
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
        filetime_set(&old, old_time);

        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86_400);
        let pruned = prune_dir_recursive(&d, cutoff, &[]);

        assert_eq!(pruned, 1, "only the old archived log should be pruned");
        assert!(!old.exists(), "old archived log should be gone");
        assert!(keep_nonlog.exists(), "non-.log file must be kept");
        assert!(fresh.exists(), "fresh log must be kept");
        let _ = std::fs::remove_dir_all(&d);
    }

    // active_log_filenames must use the caller's injected date, not re-read the
    // clock itself — otherwise two calls straddling UTC midnight could disagree.
    #[test]
    fn active_log_filenames_uses_the_injected_date_not_the_clock() {
        let names = active_log_filenames("2000-01-01");
        assert_eq!(
            names,
            vec![
                "autorip.log".to_string(),
                "autorip.log.2000-01-01".to_string()
            ]
        );
    }

    // The active tracing appender file must survive a prune even when its mtime
    // is well past the cutoff — deleting it would orphan the open FD.
    #[cfg(unix)]
    #[test]
    fn prune_never_deletes_the_active_appender_log() {
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!("autorip-prune-active-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();

        // Read the clock exactly once: reusing crate::server::util::format_date() here
        // and again for active_log_filenames() could straddle a UTC-midnight
        // rollover and disagree on "today", flaking the assertions below.
        let today = crate::server::util::format_date();

        // Today's rolled human log — the file the daily appender holds open.
        let active_name = format!("autorip.log.{today}");
        let active = d.join(&active_name);
        std::fs::write(&active, b"x").unwrap();
        // A stale rolled log from a prior day — a legitimate prune target.
        let stale = d.join("autorip.log.1999-01-01");
        std::fs::write(&stale, b"x").unwrap();

        // Backdate BOTH well past the cutoff so only the active-name skip saves it.
        let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 86_400);
        filetime_set(&active, old_time);
        filetime_set(&stale, old_time);

        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 86_400);
        let pruned = prune_dir_recursive(&d, cutoff, &active_log_filenames(&today));

        assert_eq!(pruned, 1, "only the stale rolled log should be pruned");
        assert!(active.exists(), "the active appender log must be kept");
        assert!(!stale.exists(), "the stale rolled log should be gone");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Set a file's mtime via libc::utimes (no extra crate dependency).
    #[cfg(unix)]
    fn filetime_set(path: &std::path::Path, t: std::time::SystemTime) {
        use std::os::unix::ffi::OsStrExt;
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as libc::time_t;
        let tv = libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let rc = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
        assert_eq!(rc, 0, "utimes failed");
    }

    // Covers a wedged mover/muxer thread: shutdown must neither hang forever nor abandon an
    // in-flight move too early.
    #[test]
    fn join_bounded_waits_for_a_healthy_worker_but_abandons_a_wedged_one() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        // A worker that finishes well inside the timeout must be joined, and
        // its work must be observable afterwards.
        let done = Arc::new(AtomicBool::new(false));
        let d = done.clone();
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            d.store(true, Ordering::SeqCst);
        });
        let t0 = Instant::now();
        super::join_bounded(h, "healthy", Duration::from_secs(5));
        assert!(
            done.load(Ordering::SeqCst),
            "a worker that finished must have been joined, not abandoned"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "must return as soon as the worker finishes, not sit out the timeout"
        );

        // A worker that outlives its deadline must be abandoned at roughly the
        // timeout — NOT waited on until it happens to finish.
        let h = std::thread::spawn(|| std::thread::sleep(Duration::from_secs(30)));
        let t0 = Instant::now();
        super::join_bounded(h, "wedged", Duration::from_millis(150));
        let waited = t0.elapsed();
        assert!(
            waited < Duration::from_secs(5),
            "a wedged worker must not pin shutdown: waited {waited:?}"
        );
        assert!(
            waited >= Duration::from_millis(100),
            "must actually give the worker its timeout: waited {waited:?}"
        );
    }

    // Reject fail-open lock-poison handling throughout production source.
    #[test]
    fn no_fail_open_lock_poison_forms_in_src() {
        use std::path::{Path, PathBuf};

        // Blanks comments and string/char literals (→ spaces, newlines kept) so a
        // needle quoted in a comment or string is never mistaken for a real call
        // site, and braces inside strings can't skew the test-module brace match.
        fn blank_comments_and_strings(src: &str) -> String {
            let b = src.as_bytes();
            let mut out = vec![b' '; b.len()];
            let mut i = 0;
            while i < b.len() {
                let c = b[i];
                if c == b'\n' {
                    out[i] = b'\n';
                    i += 1;
                    continue;
                }
                if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
                    while i < b.len() && b[i] != b'\n' {
                        i += 1;
                    }
                    continue;
                }
                if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    i += 2;
                    while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                        if b[i] == b'\n' {
                            out[i] = b'\n';
                        }
                        i += 1;
                    }
                    i = (i + 2).min(b.len());
                    continue;
                }
                if c == b'r' && i + 1 < b.len() && (b[i + 1] == b'"' || b[i + 1] == b'#') {
                    let mut j = i + 1;
                    let mut hashes = 0;
                    while j < b.len() && b[j] == b'#' {
                        hashes += 1;
                        j += 1;
                    }
                    if j < b.len() && b[j] == b'"' {
                        j += 1;
                        while j < b.len() {
                            if b[j] == b'"' {
                                let mut k = 0;
                                while k < hashes && j + 1 + k < b.len() && b[j + 1 + k] == b'#' {
                                    k += 1;
                                }
                                if k == hashes {
                                    j += 1 + hashes;
                                    break;
                                }
                            }
                            if b[j] == b'\n' {
                                out[j] = b'\n';
                            }
                            j += 1;
                        }
                        i = j;
                        continue;
                    }
                }
                if c == b'b' && i + 1 < b.len() && b[i + 1] == b'"' {
                    i += 1; // fall through to the normal-string handler at the quote
                }
                if b[i] == b'"' {
                    let mut j = i + 1;
                    while j < b.len() {
                        if b[j] == b'\\' {
                            j += 2;
                            continue;
                        }
                        if b[j] == b'"' {
                            j += 1;
                            break;
                        }
                        if b[j] == b'\n' {
                            out[j] = b'\n';
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                if c == b'\'' {
                    let mut j = i + 1;
                    if j < b.len() && b[j] == b'\\' {
                        j += 2;
                    } else {
                        j += 1;
                    }
                    if j < b.len() && b[j] == b'\'' {
                        i = j + 1; // char literal — blanked
                        continue;
                    }
                    out[i] = c; // lifetime tick — keep
                    i += 1;
                    continue;
                }
                out[i] = c;
                i += 1;
            }
            String::from_utf8(out).expect("ascii-preserving transform")
        }

        // Remove every `#[cfg(test)]` item (blanked, newlines preserved) so
        // test code is never scanned. Runs on the already-blanked text, so the
        // brace match sees only structural braces.
        fn strip_test_items(src: &str) -> String {
            let b = src.as_bytes();
            let needle = b"#[cfg(test)]";
            let mut out = src.as_bytes().to_vec();
            let mut i = 0;
            while i + needle.len() <= b.len() {
                if &b[i..i + needle.len()] == needle {
                    let mut j = i + needle.len();
                    while j < b.len() && b[j] != b'{' && b[j] != b';' {
                        j += 1;
                    }
                    if j < b.len() && b[j] == b'{' {
                        let mut depth = 0i32;
                        let mut k = j;
                        while k < b.len() {
                            if b[k] == b'{' {
                                depth += 1;
                            } else if b[k] == b'}' {
                                depth -= 1;
                                if depth == 0 {
                                    k += 1;
                                    break;
                                }
                            }
                            k += 1;
                        }
                        for m in i..k.min(out.len()) {
                            if out[m] != b'\n' {
                                out[m] = b' ';
                            }
                        }
                        i = k;
                        continue;
                    }
                }
                i += 1;
            }
            String::from_utf8(out).unwrap()
        }

        fn violations(path: &Path, code: &str) -> Vec<String> {
            // Join method-chain line breaks so multiline forms
            // (`cfg\n.read()\n.map(..)\n.unwrap_or_default()`) collapse onto one
            // line and match the same needles as the inline forms.
            let mut joined: Vec<String> = Vec::new();
            for raw in code.lines() {
                let t = raw.trim_start();
                if t.starts_with('.') && !joined.is_empty() {
                    joined.last_mut().unwrap().push_str(t);
                } else {
                    joined.push(raw.to_string());
                }
            }
            let locks = [".lock()", ".read()", ".write()"];
            let mut hits = Vec::new();
            for line in &joined {
                if !locks.iter().any(|m| line.contains(m)) {
                    continue;
                }
                // A: `.lock().ok()` and friends (`.ok()?`, `.ok().and_then`, `.ok().map`).
                let a = locks.iter().any(|m| line.contains(&format!("{m}.ok()")));
                // B: `if let Ok(..)`/`while let Ok(..)`/`let Ok(..) = .. else` on a lock.
                let b = line.contains("let Ok(");
                // C: ANY `.lock()`/`.read()`/`.write()` then an `.unwrap_or*` that
                //    discards the poison (bare / `_default` / `_else` / `_or(x)` /
                //    `.map(..).unwrap_or*`); `into_inner` lines recover, so allowed.
                let c = line.contains(".unwrap_or") && !line.contains("into_inner");
                if a || b || c {
                    hits.push(format!("{}: {}", path.display(), line.trim()));
                }
            }
            hits
        }

        fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    collect(&p, files);
                } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
                    files.push(p);
                }
            }
        }

        let src_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("server");
        let mut files = Vec::new();
        collect(&src_root, &mut files);
        files.sort();
        assert!(!files.is_empty(), "no .rs files found under {src_root:?}");

        let mut all = Vec::new();
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap();
            let src = crate::server::util::source_lf(&src).into_owned();
            let code = strip_test_items(&blank_comments_and_strings(&src));
            all.extend(violations(f, &code));
        }

        assert!(
            all.is_empty(),
            "fail-open lock-poison form(s) reintroduced in non-test code — \
             recover via `unwrap_or_else(|e| e.into_inner())` (or surface an \
             HTTP 500 / logged retry via `match … Err(_) => …`):\n{}",
            all.join("\n")
        );
    }
}
