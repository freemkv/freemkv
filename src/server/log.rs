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

// Each ring line carries a sequence number that only ever grows (across every
// device), so a viewer can ask for "lines after N" even once the ring wraps.
type Ring = VecDeque<(u64, String)>;
static LOGS: once_cell::sync::Lazy<Mutex<HashMap<String, Ring>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

// Serializes the ring push + file append of `device_log` against the file rename + ring
// removal of `archive_device_log`, so a line never lands half in the old session and half in
// the new one. Taken before `LOGS`.
static FILE_IO: Mutex<()> = Mutex::new(());

// The autorip base dir (logs live under `<base>/logs`). Same resolution as config, so logs
// land somewhere writable without a container mount.
fn autorip_dir() -> String {
    crate::server::config::default_autorip_dir()
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
    format!(
        "{}/logs/device_{}.log",
        autorip_dir(),
        sanitize_device(device)
    )
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
    let ts = crate::server::util::format_iso_datetime();
    let line = format!("[{}] {}", ts, msg);

    let _io = FILE_IO.lock().unwrap_or_else(|e| e.into_inner());

    // In-memory ring (last RING_CAP lines/device, O(1) VecDeque eviction).
    // `new_session` = first line since the ring was empty; the file gets a
    // build banner then so redeploy mid-session doesn't mix builds' lines.
    let new_session = {
        let mut logs = LOGS.lock().unwrap_or_else(|e| e.into_inner());
        let log = logs.entry(device.to_string()).or_default();
        let was_empty = log.is_empty();
        let seq = NEXT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        log.push_back((seq, line.clone()));
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
                    crate::server::VERSION_LABEL
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

    drop(_io);

    // Structured event into the central log stream. `device` enables
    // `jq 'select(.fields.device == "sg4")'`; `build` stamps the binary on
    // every event so the central log is self-identifying across redeploys.
    tracing::info!(device = %device, build = %crate::server::VERSION_LABEL, "{}", msg);
}

/// Get the most recent `lines` log lines for a device, oldest-first.
pub fn get_device_log(device: &str, lines: usize) -> Vec<String> {
    let logs = LOGS.lock().unwrap_or_else(|e| e.into_inner());
    logs.get(device)
        .map(|log| {
            let start = log.len().saturating_sub(lines);
            log.iter().skip(start).map(|(_, l)| l.clone()).collect()
        })
        .unwrap_or_default()
}

/// A device's ring lines with a sequence number above `since`, oldest-first,
/// and the newest sequence number handed out so far (the next `since`).
pub fn get_device_log_since(device: &str, since: u64) -> (u64, Vec<(u64, String)>) {
    let logs = LOGS.lock().unwrap_or_else(|e| e.into_inner());
    let lines = logs
        .get(device)
        .map(|log| log.iter().filter(|(s, _)| *s > since).cloned().collect())
        .unwrap_or_default();
    let newest = NEXT_SEQ.load(std::sync::atomic::Ordering::Relaxed) - 1;
    (newest, lines)
}

// `{rips_dir}/{device}_{ts}.log`, or the first free `…_{ts}_{n}.log` when that name is taken
// (two archives in one second), so a rename never replaces an earlier archive.
fn unique_archive_path(rips_dir: &str, device: &str, ts: &str) -> String {
    let first = format!("{rips_dir}/{device}_{ts}.log");
    if !std::path::Path::new(&first).exists() {
        return first;
    }
    (2u32..)
        .map(|n| format!("{rips_dir}/{device}_{ts}_{n}.log"))
        .find(|p| !std::path::Path::new(p).exists())
        .expect("an unbounded counter finds a free name")
}

/// Move the device's current live log to `logs/rips/{device}_{iso_ts}.log`
/// and clear the in-memory buffer. Called at the start of a new scan and on
/// eject so each rip attempt gets its own self-contained archive — no more
/// "yesterday's 12h saga mixed with tonight's run" confusion.
///
/// No-op if the current log is empty or missing. Archive failures are
/// logged to stderr but never propagated — logging must never break a rip.
pub fn archive_device_log(device: &str) {
    let _io = FILE_IO.lock().unwrap_or_else(|e| e.into_inner());
    let current = device_log_path(device);
    let should_archive = std::fs::metadata(&current)
        .map(|m| m.len() > 0)
        .unwrap_or(false);

    // Tracks whether the on-disk archive happened. Nothing to archive counts
    // as "ok" to clear; only a real rename failure must keep the in-memory
    // ring intact, or the live UI would go empty with no on-disk trace.
    let mut archived_ok = !should_archive;

    if should_archive {
        let rips_dir = format!("{}/logs/rips", autorip_dir());
        if let Err(e) = std::fs::create_dir_all(&rips_dir) {
            tracing::warn!(
                device = %device,
                path = %rips_dir,
                error = %e,
                "log archive: cannot create rips dir"
            );
        } else {
            let archive = unique_archive_path(
                &rips_dir,
                &sanitize_device(device),
                &crate::server::util::format_iso_datetime_filename(),
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
/// startup and re-checked on the log-prune tick (daemon.rs) so a long-uptime
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
#[path = "log_tests.rs"]
mod tests;
