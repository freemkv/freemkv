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

// The startup destination check runs off the startup path: a check stuck on a hung mount
// leaves startup (and the web server after it) free to carry on.
#[test]
fn the_startup_destination_check_never_blocks_startup() {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let started = std::time::Instant::now();
    let handle = spawn_destination_check(config::Config::default(), move |_| {
        let _ = rx.recv(); // a stat on a share that never answers
        Vec::new()
    })
    .expect("spawn the destination check");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(500),
        "{:?}",
        started.elapsed()
    );
    assert!(!handle.is_finished(), "the check is still blocked");
    drop(tx);
    handle.join().unwrap();
}
