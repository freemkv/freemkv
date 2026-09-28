use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::Mutex;

/// Per-device in-memory ring buffer cap (lines). The file log is the
/// durable record; this is just the live UI view.
const RING_CAP: usize = 500;

// Size threshold above which the non-device `system` log file is rotated
// into `logs/rips/` on startup — it has no eject/scan boundary, so without
// this it would grow unbounded for the container lifetime.
const SYSTEM_LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

static LOGS: once_cell::sync::Lazy<Mutex<HashMap<String, VecDeque<String>>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

fn log_dir() -> String {
    // Same resolution as config: AUTORIP_DIR, else writable /config (Docker),
    // else ~/.config/autorip (bare run) — so logs land somewhere writable
    // without a container mount.
    crate::config::default_autorip_dir()
}

// Neutralize a device string into a safe single path component for the log filename, so no
// caller can escape `logs/` via `/`, `\`, or `..`.
fn sanitize_device(device: &str) -> String {
    if device.is_empty()
        || device == "."
        || device == ".."
        || device.contains('/')
        || device.contains('\\')
        || device.contains("..")
    {
        tracing::warn!(device = %device, "unsafe device name neutralized to 'invalid' for log path");
        return "invalid".to_string();
    }
    device.to_string()
}

fn device_log_path(device: &str) -> String {
    format!("{}/logs/device_{}.log", log_dir(), sanitize_device(device))
}

// Strip terminal control/escape bytes from log content, so a crafted disc
// string (UDF volume-id, `bdmt` title) can't inject ANSI escapes into an
// operator's terminal or the on-disk log. Any control byte becomes `?`.
pub(crate) fn sanitize_log_msg(msg: &str) -> String {
    msg.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Log a message for a specific device. Writes to three sinks: the
/// in-memory ring (last `RING_CAP` lines/device, read by the web UI's
/// `/api/logs/{device}` endpoint), the per-device file
/// `{AUTORIP_DIR}/logs/device_{dev}.log` (archived per-rip via
/// [`archive_device_log`]), and a tracing `info` event with a `device`
/// field (flows into `autorip.log` / `autorip.jsonl`, see `observe.rs`).
/// Per-device file/ring lines are ISO-8601 timestamped:
/// `[YYYY-MM-DDTHH:MM:SSZ] msg`.
pub fn device_log(device: &str, msg: &str) {
    // Sanitize ONCE, up front, so EVERY sink (ring, file, and the structured
    // tracing event below) gets the escape-free text — not just the file line.
    let msg = sanitize_log_msg(msg);
    let msg = msg.as_str();
    let ts = crate::util::format_iso_datetime();
    let line = format!("[{}] {}", ts, msg);

    // In-memory ring (last RING_CAP lines/device, O(1) VecDeque eviction).
    // `new_session` = first line since the ring was empty; the file gets a
    // build banner then so redeploy mid-session doesn't mix builds' lines.
    let new_session = {
        let mut logs = LOGS.lock().unwrap_or_else(|e| e.into_inner());
        let log = logs.entry(device.to_string()).or_default();
        let was_empty = log.is_empty();
        log.push_back(line.clone());
        if log.len() > RING_CAP {
            log.pop_front();
        }
        was_empty
    };

    // File log — per-device, append-only between archive points. Disk-full
    // or NFS stale-handle here must not break a rip (logging isn't
    // load-bearing), but should stay observable rather than fully silent.
    let path = device_log_path(device);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut f) => {
            if new_session {
                let _ = writeln!(
                    f,
                    "[{}] ▸ autorip {} — log session start",
                    ts,
                    crate::VERSION_LABEL
                );
            }
            if let Err(e) = writeln!(f, "{}", line) {
                tracing::warn!(device = %device, path = %path, error = %e, "device log write failed");
            }
        }
        Err(e) => {
            tracing::warn!(device = %device, path = %path, error = %e, "device log open failed");
        }
    }

    // Structured event into the central log stream. `device` enables
    // `jq 'select(.fields.device == "sg4")'`; `build` stamps the binary on
    // every event so the central log is self-identifying across redeploys.
    tracing::info!(device = %device, build = %crate::VERSION_LABEL, "{}", msg);
}

/// Get the most recent `lines` log lines for a device, oldest-first.
pub fn get_device_log(device: &str, lines: usize) -> Vec<String> {
    let logs = LOGS.lock().unwrap_or_else(|e| e.into_inner());
    logs.get(device)
        .map(|log| {
            // Single allocation of just the tail slice, computed while
            // holding the lock — no intermediate double-reverse Vecs.
            let start = log.len().saturating_sub(lines);
            log.iter().skip(start).cloned().collect()
        })
        .unwrap_or_default()
}

/// Move the device's current live log to `logs/rips/{device}_{iso_ts}.log`
/// and clear the in-memory buffer. Called at the start of a new scan and on
/// eject so each rip attempt gets its own self-contained archive — no more
/// "yesterday's 12h saga mixed with tonight's run" confusion.
///
/// No-op if the current log is empty or missing. Archive failures are
/// logged to stderr but never propagated — logging must never break a rip.
pub fn archive_device_log(device: &str) {
    let current = device_log_path(device);
    let should_archive = std::fs::metadata(&current)
        .map(|m| m.len() > 0)
        .unwrap_or(false);

    // Tracks whether the on-disk archive happened. Nothing to archive counts
    // as "ok" to clear; only a real rename failure must keep the in-memory
    // ring intact, or the live UI would go empty with no on-disk trace.
    let mut archived_ok = !should_archive;

    if should_archive {
        let rips_dir = format!("{}/logs/rips", log_dir());
        if let Err(e) = std::fs::create_dir_all(&rips_dir) {
            tracing::warn!(
                device = %device,
                path = %rips_dir,
                error = %e,
                "log archive: cannot create rips dir"
            );
        } else {
            let archive = format!(
                "{}/{}_{}.log",
                rips_dir,
                sanitize_device(device),
                crate::util::format_iso_datetime_filename(),
            );
            match std::fs::rename(&current, &archive) {
                Ok(()) => archived_ok = true,
                Err(e) => tracing::warn!(
                    device = %device,
                    src = %current,
                    dst = %archive,
                    error = %e,
                    "log archive: rename failed; keeping in-memory ring so the live view stays populated"
                ),
            }
        }
    }

    // Only clear the in-memory ring once the live file is safely archived
    // (or there was nothing to archive). On a rename failure we leave the
    // ring so the live view still reflects the on-disk log.
    if archived_ok {
        LOGS.lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(device);
    }
}

/// Drop a device's in-memory ring buffer without archiving it.
///
/// Called when a drive is hot-unplugged: there is no eject/scan boundary
/// to trigger [`archive_device_log`], so without this the device's `LOGS`
/// entry would linger for the container's lifetime. The on-disk
/// `device_*.log` is left in place — it is the durable record, reclaimed
/// on the next scan's `archive_device_log` if the device returns; this
/// only evicts the live UI ring for a device that is gone.
pub fn forget_device(device: &str) {
    LOGS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

/// Log to system log (not device-specific).
pub fn syslog(msg: &str) {
    device_log("system", msg);
}

/// Rotate the non-device `system` log into `logs/rips/` if it has grown past
/// `SYSTEM_LOG_ROTATE_BYTES`. Unlike per-device logs (archived on each
/// scan/eject boundary), the system log has no natural archive point, so
/// without this it grows unbounded for the container's lifetime. Called at
/// startup and re-checked on the log-prune tick (main.rs) so a long-uptime
/// daemon still bounds it; reuses `archive_device_log`'s rename-into-rips
/// behaviour. Best-effort and never propagates — logging must not break startup.
pub fn rotate_system_log_if_large() {
    let path = device_log_path("system");
    let too_big = std::fs::metadata(&path)
        .map(|m| m.len() > SYSTEM_LOG_ROTATE_BYTES)
        .unwrap_or(false);
    if too_big {
        archive_device_log("system");
    }
}

// Serializes tests that manipulate the process-wide `AUTORIP_DIR` env var (crate scope: racing
// writers live in other modules too, e.g. `ripper::resume`). Acquire via [`env_guard`].
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds [`ENV_LOCK`] and restores `AUTORIP_DIR` to its prior value on drop, so
/// a test's tempdir can never outlive the test as a stale global.
#[cfg(test)]
pub(crate) struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: env access in tests, serialized by the lock this guard holds
        // — no other guarded test can observe the intermediate state.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var("AUTORIP_DIR", v),
                None => std::env::remove_var("AUTORIP_DIR"),
            }
        }
    }
}

/// Take the `AUTORIP_DIR` test lock, capturing the current value so it is
/// restored when the returned guard drops. Hold it for the WHOLE test.
#[cfg(test)]
pub(crate) fn env_guard() -> EnvGuard {
    let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    EnvGuard {
        _lock: lock,
        prev: std::env::var_os("AUTORIP_DIR"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        // Per project convention, never /tmp (wiped on reboot). Anchor
        // under the workspace's target/ (gitignored) instead.
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!(
                "autorip-log-test-{}-{}-{}",
                std::process::id(),
                tag,
                n
            ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("logs")).unwrap();
        d
    }

    #[test]
    fn device_log_writes_iso_timestamped_line() {
        let _guard = crate::log::env_guard();
        let d = tmpdir("iso_ts");
        // Route the test's logs to the tempdir.
        // SAFETY: env access in single-threaded tests.
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let dev = format!("test_sg_{}", std::process::id());
        device_log(&dev, "hello");
        let content = std::fs::read_to_string(device_log_path(&dev)).unwrap();
        // Format: [YYYY-MM-DDTHH:MM:SSZ] hello
        assert!(content.starts_with('['));
        assert!(content.contains(']'));
        assert!(content.trim_end().ends_with("hello"));
        let bracket = &content[1..21]; // 20-char ISO datetime inside brackets
        assert_eq!(bracket.len(), 20);
        assert!(bracket.ends_with('Z'));
        assert_eq!(bracket.as_bytes()[10], b'T');
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn new_session_writes_build_banner_to_file_not_ring() {
        let _guard = crate::log::env_guard();
        let d = tmpdir("build_banner");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let dev = format!("test_banner_{}", std::process::id());
        device_log(&dev, "first line");
        device_log(&dev, "second line");

        // File: a build banner precedes the first line (session anchor), and the
        // build label is present so the slice is attributable.
        let content = std::fs::read_to_string(device_log_path(&dev)).unwrap();
        assert!(
            content.contains("log session start") && content.contains(crate::VERSION_LABEL),
            "file must carry a build banner: {content}"
        );
        assert_eq!(
            content.matches("log session start").count(),
            1,
            "exactly one banner per session, not per line"
        );
        // The banner must come before the first real line.
        let banner_at = content.find("log session start").unwrap();
        let first_at = content.find("first line").unwrap();
        assert!(banner_at < first_at, "banner must precede the first line");

        // Ring (live UI view) is unchanged — banner is file-only, so line
        // accounting stays exactly as before.
        let ring = get_device_log(&dev, 100);
        assert_eq!(
            ring.len(),
            2,
            "ring holds only the two real lines, no banner"
        );
        assert!(ring.iter().all(|l| !l.contains("log session start")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn archive_device_log_moves_to_rips_dir() {
        let _guard = crate::log::env_guard();
        let d = tmpdir("archive_move");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let dev = format!("test_mv_{}", std::process::id());
        device_log(&dev, "pre-archive");
        let live = device_log_path(&dev);
        assert!(std::path::Path::new(&live).exists());

        archive_device_log(&dev);

        // Live file gone after archive.
        assert!(!std::path::Path::new(&live).exists());

        // Rips dir has exactly one file matching the device name.
        let rips_dir = d.join("logs").join("rips");
        let archived: Vec<_> = std::fs::read_dir(&rips_dir)
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(&dev))
            .collect();
        assert_eq!(archived.len(), 1, "expected one archived log file");

        let content = std::fs::read_to_string(archived[0].path()).unwrap();
        assert!(content.contains("pre-archive"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn archive_device_log_no_op_when_empty() {
        let _guard = crate::log::env_guard();
        let d = tmpdir("archive_empty");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let dev = format!("test_empty_{}", std::process::id());
        // Don't call device_log — file doesn't exist yet. archive_device_log
        // must not panic or create a junk archive entry.
        archive_device_log(&dev);
        let rips_dir = d.join("logs").join("rips");
        if rips_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&rips_dir)
                .unwrap()
                .filter_map(|r| r.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(&dev))
                .collect();
            assert!(entries.is_empty());
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn archive_device_log_clears_in_memory_buffer() {
        let _guard = crate::log::env_guard();
        let d = tmpdir("archive_buf");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let dev = format!("test_buf_{}", std::process::id());
        device_log(&dev, "first");
        device_log(&dev, "second");
        assert!(!get_device_log(&dev, 100).is_empty());

        archive_device_log(&dev);
        assert!(get_device_log(&dev, 100).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn archive_failure_keeps_in_memory_ring() {
        // If the on-disk archive fails, the in-memory ring MUST survive so
        // the live UI doesn't go empty while the log is still on disk. Force
        // create_dir_all("logs/rips") to fail via a regular file in its place.
        let _guard = crate::log::env_guard();
        let d = tmpdir("archive_fail");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        // Plant a file at logs/rips so the dir can't be created.
        std::fs::write(d.join("logs").join("rips"), b"not a dir").unwrap();

        let dev = format!("test_fail_{}", std::process::id());
        device_log(&dev, "live line");
        assert!(!get_device_log(&dev, 100).is_empty());
        let live = device_log_path(&dev);
        assert!(std::path::Path::new(&live).exists());

        archive_device_log(&dev);

        // Ring preserved (archive failed), live file still on disk.
        assert!(
            !get_device_log(&dev, 100).is_empty(),
            "in-memory ring must be kept when archive rename/setup fails"
        );
        assert!(
            std::path::Path::new(&live).exists(),
            "live log file must remain on disk after a failed archive"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn forget_device_clears_ring_without_archiving() {
        // Hot-unplug eviction: forget_device drops the in-memory ring but
        // leaves the on-disk device log in place (no archive).
        let _guard = crate::log::env_guard();
        let d = tmpdir("forget");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let dev = format!("test_forget_{}", std::process::id());
        device_log(&dev, "before unplug");
        assert!(!get_device_log(&dev, 100).is_empty());
        let live = device_log_path(&dev);
        assert!(std::path::Path::new(&live).exists());

        forget_device(&dev);

        // Ring evicted...
        assert!(
            get_device_log(&dev, 100).is_empty(),
            "in-memory ring must be evicted on hot-unplug"
        );
        // ...but the durable on-disk log is left untouched (no archive).
        assert!(
            std::path::Path::new(&live).exists(),
            "device log file must remain on disk after forget_device"
        );
        let rips_dir = d.join("logs").join("rips");
        if rips_dir.exists() {
            let archived: Vec<_> = std::fs::read_dir(&rips_dir)
                .unwrap()
                .filter_map(|r| r.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(&dev))
                .collect();
            assert!(archived.is_empty(), "forget_device must not archive");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sanitize_log_msg_strips_ansi_and_control_bytes() {
        // Log-injection defense: a crafted disc string could inject ANSI
        // escapes into a terminal/log; every control byte (incl. ESC
        // \u{1b}) becomes '?', while ordinary printable text survives.
        assert_eq!(
            sanitize_log_msg("\u{1b}[2J\u{1b}[1;1H"),
            "?[2J?[1;1H",
            "each ESC must become '?', the rest of the CSI text is preserved"
        );

        // A bare ESC is replaced.
        assert_eq!(sanitize_log_msg("a\u{1b}b"), "a?b");

        // Other C0 controls (NUL, BEL, backspace, TAB) and DEL and a C1
        // control (0x9b, single-char CSI) all map to '?'.
        assert_eq!(
            sanitize_log_msg("x\u{0}\u{7}\u{8}\t\u{7f}\u{9b}y"),
            "x??????y",
            "all C0/DEL/C1 control bytes must be neutralized"
        );

        // Newlines are control characters too — the function replaces them
        // (it does NOT preserve line structure), so verify that behavior
        // explicitly rather than assuming they pass through.
        assert_eq!(sanitize_log_msg("line1\nline2"), "line1?line2");

        // Ordinary text — including multibyte UTF-8 — is preserved verbatim.
        assert_eq!(
            sanitize_log_msg("WRAITHLINE_PART_TWO — café 日本語"),
            "WRAITHLINE_PART_TWO — café 日本語",
            "printable UTF-8 must pass through unchanged"
        );
        assert_eq!(sanitize_log_msg(""), "");
    }

    #[test]
    fn sanitize_device_neutralizes_traversal() {
        // The hard invariant at the construction point: a device with a
        // path separator or traversal sequence can't escape logs/.
        for bad in ["../etc/passwd", "a/b", "..", "", ".", "x\\y"] {
            let path = format!("{}/logs/device_{}.log", "/cfg", super::sanitize_device(bad));
            assert!(
                !path.contains(".."),
                "path must not contain traversal for {bad:?}: {path}"
            );
            assert_eq!(
                path.matches("/logs/device_").count(),
                1,
                "device must be a single component for {bad:?}: {path}"
            );
            // The component after device_ must not introduce a new dir.
            assert!(!path.contains("device_../") && !path.contains("device_a/b"));
        }
        // A normal device name passes through unchanged.
        assert_eq!(super::sanitize_device("sg0"), "sg0");
    }

    #[test]
    fn get_device_log_respects_line_limit() {
        let _guard = crate::log::env_guard();
        let d = tmpdir("line_limit");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let dev = format!("test_lim_{}", std::process::id());
        for i in 0..5 {
            device_log(&dev, &format!("line {i}"));
        }
        let lines = get_device_log(&dev, 3);
        assert_eq!(lines.len(), 3);
        // Tail of the buffer — last 3 lines are 2, 3, 4.
        assert!(lines[2].contains("line 4"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ring_evicts_oldest_past_cap() {
        let _guard = crate::log::env_guard();
        let d = tmpdir("ring_cap");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let dev = format!("test_ring_{}", std::process::id());
        for i in 0..(RING_CAP + 50) {
            device_log(&dev, &format!("line {i}"));
        }
        // Ring is capped: asking for more than the cap returns at most cap.
        let all = get_device_log(&dev, RING_CAP + 1000);
        assert_eq!(all.len(), RING_CAP, "ring must be capped at RING_CAP");
        // Oldest lines evicted: the first retained line is line 50.
        assert!(all[0].contains("line 50"), "got: {}", all[0]);
        assert!(all[RING_CAP - 1].contains(&format!("line {}", RING_CAP + 49)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rotate_system_log_archives_only_when_large() {
        let _guard = crate::log::env_guard();
        let d = tmpdir("sys_rotate");
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        // Small system log: must NOT rotate.
        syslog("small system message");
        rotate_system_log_if_large();
        assert!(
            std::path::Path::new(&device_log_path("system")).exists(),
            "small system log must be left in place"
        );

        // Grow it past the threshold, then rotate.
        let big = "x".repeat((SYSTEM_LOG_ROTATE_BYTES + 1024) as usize);
        std::fs::write(device_log_path("system"), big).unwrap();
        rotate_system_log_if_large();
        // Invariant: the OVERSIZED log is gone, not that no log exists.
        // `syslog()` runs transitively from other tests without the env
        // guard and can recreate the file post-rotation via AUTORIP_DIR.
        let live_path = device_log_path("system");
        let live_len = std::fs::metadata(&live_path).map(|m| m.len()).unwrap_or(0);
        assert!(
            live_len <= SYSTEM_LOG_ROTATE_BYTES,
            "oversized system log must be rotated out (live log is {live_len} bytes)"
        );
        let rips_dir = d.join("logs").join("rips");
        let archived: Vec<_> = std::fs::read_dir(&rips_dir)
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("system"))
            .collect();
        assert_eq!(archived.len(), 1, "expected one archived system log");
        let _ = std::fs::remove_dir_all(&d);
    }
}
