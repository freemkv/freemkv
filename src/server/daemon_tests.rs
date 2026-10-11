use super::*;

// On a long-uptime daemon the system log only shrank at boot; the prune tick must
// also rotate an oversized one.
#[test]
fn log_prune_tick_rotates_an_oversized_system_log() {
    let _guard = crate::server::log::env_guard();
    let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(format!("daemon-tick-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("logs")).unwrap();
    // SAFETY: serialized by the guard above.
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let live = d.join("logs/device_system.log");
    std::fs::write(&live, vec![b'x'; 6 * 1024 * 1024]).unwrap();
    let cfg = std::sync::RwLock::new(crate::server::config::Config {
        autorip_dir: d.to_string_lossy().into_owned(),
        ..Default::default()
    });
    log_prune_tick(&cfg);
    let left = std::fs::metadata(&live).map(|m| m.len()).unwrap_or(0);
    assert!(
        left < 1024 * 1024,
        "oversized system log must be rotated out"
    );
    assert!(d.join("logs/rips").is_dir());
    let _ = std::fs::remove_dir_all(&d);
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
fn healthcheck_probes_the_port_the_server_binds() {
    assert_eq!(healthcheck_port(None), 8080);
    assert_eq!(healthcheck_port(Some("9000")), 9000);
    assert_eq!(healthcheck_port(Some(" 9000 ")), 9000);
    assert_eq!(healthcheck_port(Some("0")), 8080);
    assert_eq!(healthcheck_port(Some("")), 8080);
    assert_eq!(healthcheck_port(Some("junk")), 8080);
}

#[cfg(unix)]
fn scratch(tag: &str) -> std::path::PathBuf {
    let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(format!("daemon-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(unix)]
#[test]
fn env_snapshot_is_private_and_failures_surface() {
    use std::os::unix::fs::PermissionsExt;
    let d = scratch("env");
    let f = d.join("autorip.env");
    std::fs::write(&f, "old").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    let vars = vec![
        ("TMDB_API_KEY".to_string(), "k'ey".to_string()),
        ("HOME".to_string(), "/nope".to_string()),
    ];
    write_env_snapshot(&f, vars.into_iter()).unwrap();
    assert_eq!(
        std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "TMDB_API_KEY='k'\\''ey'\n"
    );
    assert!(write_env_snapshot(&d.join("missing/x.env"), std::iter::empty()).is_err());
    let _ = std::fs::remove_dir_all(&d);
}

#[cfg(unix)]
#[test]
fn keydb_link_never_deletes_a_populated_directory() {
    let d = scratch("link");
    let target = d.join("cfg/freemkv");
    std::fs::create_dir_all(&target).unwrap();
    let link = d.join("home/.config/freemkv");
    std::fs::create_dir_all(&link).unwrap();
    std::fs::write(link.join("keydb.cfg"), "keys").unwrap();
    assert!(link_keydb_dir(&target, &link).is_err());
    assert_eq!(
        std::fs::read_to_string(link.join("keydb.cfg")).unwrap(),
        "keys"
    );
    // Empty dir and stale symlink are replaced.
    std::fs::remove_file(link.join("keydb.cfg")).unwrap();
    link_keydb_dir(&target, &link).unwrap();
    assert_eq!(std::fs::read_link(&link).unwrap(), target);
    link_keydb_dir(&target, &link).unwrap();
    let _ = std::fs::remove_dir_all(&d);
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
    assert!(super::join_bounded(
        Some(h),
        "healthy",
        Duration::from_secs(5)
    ));
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
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let h = std::thread::spawn(move || {
        let _ = wait.recv();
    });
    let t0 = Instant::now();
    assert!(!super::join_bounded(
        Some(h),
        "wedged",
        Duration::from_millis(150)
    ));
    let waited = t0.elapsed();
    assert!(
        waited < Duration::from_secs(5),
        "a wedged worker must not pin shutdown: waited {waited:?}"
    );
    assert!(
        waited >= Duration::from_millis(100),
        "must actually give the worker its timeout: waited {waited:?}"
    );
    drop(release);
    assert!(super::join_bounded(None, "absent", Duration::ZERO));
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

// Run `name` alone in a child test process: these tests drive the process-wide
// SHUTDOWN/REBOOT/TERMINATE flags and the real signal handler.
fn in_child(name: &str) -> bool {
    const CHILD: &str = "FREEMKV_TEST_DAEMON_SIGNAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("server::daemon::tests::{name}"),
            "--nocapture",
        ])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(
        status.success(),
        "{name} failed in its child process: {status}"
    );
    false
}

// Short bounds for the stop tests: nothing in them is meant to drain slowly.
fn short_bounds() -> Bounds {
    Bounds {
        rips: Duration::ZERO,
        services: Duration::from_millis(100),
        web: Duration::from_millis(100),
    }
}

// A reboot sets SHUTDOWN itself; the first real SIGTERM during its drain is a
// stop request to honour gracefully, not a "second signal" force-exit.
#[cfg(unix)]
#[test]
fn first_sigterm_during_reboot_drain_is_a_graceful_stop() {
    if !in_child("first_sigterm_during_reboot_drain_is_a_graceful_stop") {
        return;
    }
    REBOOT.store(true, Ordering::Release);
    SHUTDOWN.store(true, Ordering::Release);
    handle_signal(libc::SIGTERM);
    assert!(SHUTDOWN.load(Ordering::Acquire));
    assert!(TERMINATE.load(Ordering::Acquire));
    assert!(
        !relaunch_requested(),
        "a stop request vetoes the pending reboot"
    );
}

// A stop signal that lands while the reboot drains turns it into a plain stop.
#[cfg(unix)]
#[test]
fn sigterm_during_reboot_drain_returns_without_relaunch() {
    if !in_child("sigterm_during_reboot_drain_returns_without_relaunch") {
        return;
    }
    let staging = tempfile::tempdir().unwrap();
    REBOOT.store(true, Ordering::Release);
    SHUTDOWN.store(true, Ordering::Release);
    assert!(relaunch_requested());
    let rips = |_: Duration| {
        handle_signal(libc::SIGTERM);
        true
    };
    assert!(stop_services(
        staging.path(),
        rips,
        vec![],
        None,
        &short_bounds()
    ));
    assert!(!relaunch_requested());
    let code = relaunch_with(&Relaunch::capture(), |_, _| {
        panic!("a stopped daemon must not re-exec")
    });
    assert_eq!(code, 0);
}

// A second stop signal force-exits with status 1 instead of waiting for the drain.
#[cfg(unix)]
#[test]
fn second_stop_signal_force_exits() {
    const CHILD: &str = "FREEMKV_TEST_SECOND_SIGNAL_CHILD";
    if std::env::var_os(CHILD).is_some() {
        handle_signal(libc::SIGTERM);
        assert!(TERMINATE.load(Ordering::Acquire));
        handle_signal(libc::SIGINT);
        unreachable!("the second signal must _exit(1)");
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "server::daemon::tests::second_stop_signal_force_exits",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1), "{status}");
}

// A drain stuck on one worker gives up at its bound and the reboot still proceeds.
#[test]
fn wedged_worker_still_reaches_relaunch_after_timeout() {
    if !in_child("wedged_worker_still_reaches_relaunch_after_timeout") {
        return;
    }
    let staging = tempfile::tempdir().unwrap();
    REBOOT.store(true, Ordering::Release);
    SHUTDOWN.store(true, Ordering::Release);
    let (release, wedged) = std::sync::mpsc::channel::<()>();
    let mover = std::thread::spawn(move || {
        let _ = wedged.recv();
    });
    let started = std::time::Instant::now();
    let workers = vec![("mover", Some(mover))];
    assert!(!stop_services(
        staging.path(),
        |_| true,
        workers,
        None,
        &short_bounds()
    ));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(relaunch_requested());
    let mut execs = 0;
    let code = relaunch_with(&Relaunch::capture(), |_, _| {
        execs += 1;
        std::io::Error::other("test")
    });
    assert_eq!((code, execs), (RESTART_EXIT_CODE, 1));
    drop(release);
}

// An idle reboot re-execs the binary and the full argv (argv[0] included) the daemon
// was started with; an exec that returns means exit 75 for the service manager.
#[test]
fn idle_reboot_relaunches_with_the_startup_binary_and_argv() {
    if !in_child("idle_reboot_relaunches_with_the_startup_binary_and_argv") {
        return;
    }
    let staging = tempfile::tempdir().unwrap();
    REBOOT.store(true, Ordering::Release);
    SHUTDOWN.store(true, Ordering::Release);
    assert!(stop_services(
        staging.path(),
        |_| true,
        vec![],
        None,
        &short_bounds()
    ));
    assert!(relaunch_requested());
    let mut calls = Vec::new();
    let code = relaunch_with(&Relaunch::capture(), |exe, argv| {
        calls.push((exe.to_path_buf(), argv.to_vec()));
        std::io::Error::other("test")
    });
    assert_eq!(code, RESTART_EXIT_CODE);
    let expected_argv: Vec<std::ffi::OsString> = std::env::args_os().collect();
    assert_eq!(
        calls,
        vec![(std::env::current_exe().unwrap(), expected_argv)]
    );
}

// With no binary or no argv to re-exec there is nothing to try: exit 75 at once.
#[test]
fn exec_failure_or_unknown_binary_exits_75() {
    if !in_child("exec_failure_or_unknown_binary_exits_75") {
        return;
    }
    REBOOT.store(true, Ordering::Release);
    let never = |_: &std::path::Path, _: &[std::ffi::OsString]| -> std::io::Error {
        panic!("nothing to exec")
    };
    let unknown = Relaunch {
        exe: Err(std::io::Error::other("no /proc")),
        argv: std::env::args_os().collect(),
    };
    assert_eq!(relaunch_with(&unknown, never), RESTART_EXIT_CODE);
    let no_argv = Relaunch {
        exe: Ok(std::env::current_exe().unwrap()),
        argv: Vec::new(),
    };
    assert_eq!(relaunch_with(&no_argv, never), RESTART_EXIT_CODE);
}

// A reboot tail that stalls past its bound (a hung log write) still exits 75; a stop
// signal disarms it.
#[test]
fn relaunch_deadline_exits_75_unless_a_stop_signal_vetoed_it() {
    if !in_child("relaunch_deadline_exits_75_unless_a_stop_signal_vetoed_it") {
        return;
    }
    REBOOT.store(true, Ordering::Release);
    let (tx, fired) = std::sync::mpsc::channel();
    let deadline = spawn_relaunch_deadline(Duration::from_millis(50), move |code| {
        let _ = tx.send(code);
    })
    .unwrap();
    assert_eq!(
        fired.recv_timeout(Duration::from_secs(5)),
        Ok(RESTART_EXIT_CODE)
    );
    deadline.join().unwrap();

    TERMINATE.store(true, Ordering::Release);
    let (tx, fired) = std::sync::mpsc::channel();
    spawn_relaunch_deadline(Duration::from_millis(50), move |code| {
        let _ = tx.send(code);
    })
    .unwrap()
    .join()
    .unwrap();
    assert!(
        fired.try_recv().is_err(),
        "a stop signal must disarm the deadline"
    );
}

// End to end on Linux: exec keeps the pid and closes the CLOEXEC listener, so the
// new image can bind the same port.
#[cfg(target_os = "linux")]
#[test]
fn exec_in_place_replaces_the_process_and_frees_the_port() {
    const NAME: &str =
        "server::daemon::tests::exec_in_place_replaces_the_process_and_frees_the_port";
    if !in_child("exec_in_place_replaces_the_process_and_frees_the_port") {
        return;
    }
    if std::env::var_os("FREEMKV_TEST_EXEC_STAGE").is_some() {
        let pid: u32 = std::env::var("FREEMKV_TEST_EXEC_PID")
            .unwrap()
            .parse()
            .unwrap();
        let port = std::env::var("FREEMKV_TEST_EXEC_PORT").unwrap();
        assert_eq!(std::process::id(), pid, "exec must keep the pid");
        std::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .expect("exec must close the old listener");
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // SAFETY: this isolated child process is single-threaded at this point.
    unsafe {
        std::env::set_var("FREEMKV_TEST_EXEC_STAGE", "2");
        std::env::set_var("FREEMKV_TEST_EXEC_PID", std::process::id().to_string());
        std::env::set_var("FREEMKV_TEST_EXEC_PORT", port.to_string());
    }
    let exe = std::env::current_exe().unwrap();
    let relaunch = Relaunch {
        exe: Ok(exe.clone()),
        argv: vec![
            exe.into(),
            "--exact".into(),
            NAME.into(),
            "--nocapture".into(),
        ],
    };
    REBOOT.store(true, Ordering::Release);
    let code = relaunch_with(&relaunch, exec_in_place);
    panic!("exec returned (exit code {code})");
}

fn muxing_staging() -> (tempfile::TempDir, std::path::PathBuf) {
    let staging = tempfile::tempdir().unwrap();
    let disc = staging.path().join("disc");
    std::fs::create_dir_all(&disc).unwrap();
    let marker = disc.join(ripper::staging::MUXING_MARKER);
    std::fs::write(&marker, b"").unwrap();
    (staging, marker)
}

// A stop or a reboot is not a crash: markers are cleared before the drain (a SIGKILL
// at the stop grace finds them clean) and again after it (a rip a late request started).
#[test]
fn markers_are_cleared_before_the_drain_and_again_after_it() {
    if !in_child("markers_are_cleared_before_the_drain_and_again_after_it") {
        return;
    }
    SHUTDOWN.store(true, Ordering::Release);
    for reboot in [false, true] {
        REBOOT.store(reboot, Ordering::Release);
        let (staging, marker) = muxing_staging();
        let rips = |_: Duration| {
            assert!(
                !marker.exists(),
                "marker still set during the drain (reboot={reboot})"
            );
            std::fs::write(&marker, b"").unwrap();
            false
        };
        assert!(!stop_services(
            staging.path(),
            rips,
            vec![],
            None,
            &short_bounds()
        ));
        assert!(
            !marker.exists(),
            "late marker survived the stop (reboot={reboot})"
        );
    }
}
