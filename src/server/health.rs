//! Folder health for the System, Library and Remux pages, checked off the request path.
//!
//! A thread re-checks every configured folder each [`INTERVAL_SECS`]; each
//! check runs on its own short-lived thread and is given [`CHECK_TIMEOUT`],
//! so a hung NFS mount reads as "not responding" instead of hanging anything.
//! [`preflight`] is the same bounded probe plus a create-and-delete write, run
//! before a remux or a move starts. A folder never has more than one probe in
//! flight: a probe the kernel never returns from is abandoned, not repeated.
//! `GET /api/system` and the Library only read the cached result.

use crate::server::config::Config;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const INTERVAL_SECS: u64 = 30;
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a preflight may take before the folder counts as not responding.
pub const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(10);

/// How a folder answered its last check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    #[default]
    Ok,
    /// It answered with an error.
    Unhealthy,
    /// It did not answer within the time limit.
    Unresponsive,
}

/// Why a folder cannot be used, by cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fault {
    /// ESTALE: the network share was remounted or exported again under it.
    Stale,
    /// EIO.
    Io,
    /// EROFS.
    ReadOnly,
    /// ENOENT: the folder (or the share it lives on) is not there.
    Missing,
    /// No answer in time, or ETIMEDOUT and the network errors.
    Unresponsive,
    /// It lies under the configured network share, and the share is not mounted: a write
    /// there would land on the container's own disk.
    Unmounted,
    /// EACCES / EPERM, or no write access.
    Denied,
    Other,
}

impl Fault {
    /// The cause of an I/O error, from its kind, its errno or its `(os error N)` text.
    pub fn of(e: &std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        let errno = e.raw_os_error().or_else(|| errno_in(&e.to_string()));
        if let Some(n) = errno
            && let Some(f) = Self::of_errno(n)
        {
            return f;
        }
        match e.kind() {
            K::StaleNetworkFileHandle => Fault::Stale,
            K::ReadOnlyFilesystem => Fault::ReadOnly,
            K::NotFound => Fault::Missing,
            K::PermissionDenied => Fault::Denied,
            K::TimedOut
            | K::NotConnected
            | K::HostUnreachable
            | K::NetworkUnreachable
            | K::NetworkDown => Fault::Unresponsive,
            _ => Fault::Other,
        }
    }

    fn of_errno(n: i32) -> Option<Self> {
        #[cfg(unix)]
        {
            Some(match n {
                libc::ESTALE => Fault::Stale,
                libc::EIO => Fault::Io,
                libc::EROFS => Fault::ReadOnly,
                libc::ENOENT => Fault::Missing,
                libc::EACCES | libc::EPERM => Fault::Denied,
                libc::ETIMEDOUT
                | libc::ENOTCONN
                | libc::EHOSTDOWN
                | libc::EHOSTUNREACH
                | libc::ENETUNREACH
                | libc::ENETDOWN => Fault::Unresponsive,
                _ => return None,
            })
        }
        #[cfg(not(unix))]
        {
            let _ = n;
            None
        }
    }

    /// The state a folder with this fault is in.
    pub fn state(self) -> State {
        if self == Fault::Unresponsive {
            State::Unresponsive
        } else {
            State::Unhealthy
        }
    }

    /// One plain sentence naming the folder by its role ("library", "output", ...).
    pub fn message(self, role: &str) -> String {
        let role = role.to_lowercase();
        match self {
            Fault::Stale => format!(
                "The {role} folder is unreachable — the network share needs to be remounted (stale file handle)."
            ),
            Fault::Io => format!(
                "The {role} folder reported an I/O error — the disk or the network share behind it may be failing."
            ),
            Fault::ReadOnly => format!(
                "The {role} folder is read-only — its share or disk was mounted (or remounted) read-only."
            ),
            Fault::Missing => {
                format!("The {role} folder is missing — the share it lives on may not be mounted.")
            }
            Fault::Unresponsive => format!(
                "The {role} folder is not responding — the network share may be down or hung."
            ),
            Fault::Unmounted => format!(
                "The {role} folder's network share is not mounted — nothing is written there until it is."
            ),
            Fault::Denied => format!(
                "freemkv is not allowed to use the {role} folder — check its owner and permissions."
            ),
            Fault::Other => format!("The {role} folder cannot be used right now."),
        }
    }

    /// What to do about it.
    pub fn hint(self) -> &'static str {
        match self {
            Fault::Stale | Fault::Unresponsive | Fault::Missing | Fault::Io => {
                "Remount the share on the host (or bring the NAS back); waiting work resumes on its own."
            }
            Fault::Unmounted => {
                "freemkv retries the mount on its own; check that the NAS is up. Waiting work resumes on its own."
            }
            Fault::ReadOnly => {
                "Remount the share read-write on the host; waiting work resumes on its own."
            }
            Fault::Denied => {
                "Give the container's user write access to the folder; waiting work resumes on its own."
            }
            Fault::Other => "Check the folder on the host; waiting work resumes on its own.",
        }
    }
}

// The errno in an error's text ("... (os error 116)"), for errors that lost their raw code.
fn errno_in(text: &str) -> Option<i32> {
    let at = text.rfind("(os error ")?;
    text[at + "(os error ".len()..]
        .split(')')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// One folder's state at the last check.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Mount {
    /// What the folder is for ("Staging", "Library", ...).
    pub role: &'static str,
    pub path: PathBuf,
    pub ok: bool,
    /// Why not, in words.
    pub problem: Option<String>,
    pub writable: Option<bool>,
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub latency_ms: Option<u64>,
    pub checked_at: u64,
    pub state: State,
    pub fault: Option<Fault>,
    /// The plain-language sentence and the fix, while it is not ok.
    pub message: Option<String>,
    pub hint: Option<String>,
    /// When `state` last changed.
    pub since: u64,
    /// The last check that found it usable.
    pub last_ok: Option<u64>,
    /// The latest problem, kept after the folder recovers.
    pub last_error: Option<String>,
    pub last_error_at: Option<u64>,
}

impl Mount {
    fn blank(role: &'static str, path: &Path) -> Self {
        let now = crate::server::util::epoch_secs();
        Mount {
            role,
            path: path.to_path_buf(),
            ok: false,
            problem: None,
            writable: None,
            free_bytes: None,
            total_bytes: None,
            latency_ms: None,
            checked_at: now,
            state: State::Ok,
            fault: None,
            message: None,
            hint: None,
            since: now,
            last_ok: None,
            last_error: None,
            last_error_at: None,
        }
    }

    // Mark it failed with `fault`; `problem` is the raw text.
    fn failed(mut self, fault: Fault, problem: String) -> Self {
        self.ok = false;
        self.state = fault.state();
        self.fault = Some(fault);
        self.message = Some(fault.message(self.role));
        self.hint = Some(fault.hint().into());
        self.last_error = Some(problem.clone());
        self.last_error_at = Some(self.checked_at);
        self.problem = Some(problem);
        self
    }

    fn passed(mut self) -> Self {
        self.ok = true;
        self.state = State::Ok;
        self.last_ok = Some(self.checked_at);
        self
    }
}

static LAST: Mutex<Vec<Mount>> = Mutex::new(Vec::new());
// Moves when a folder's state or message changes, so the Library's stream sends it.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The last completed check.
pub fn mounts() -> Vec<Mount> {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Bumped whenever a folder's state or message changes.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// The checked folder that holds `path` (the longest configured prefix).
pub fn status_for(path: &Path) -> Option<Mount> {
    LAST.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|m| path.starts_with(&m.path))
        .max_by_key(|m| m.path.components().count())
        .cloned()
}

/// The remux engine's local staging folder, when one is set.
pub fn remux_stage_dir() -> Option<PathBuf> {
    std::env::var_os("FREEMKV_REMUX_STAGING_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// The folders worth watching for `cfg`, deduplicated, in display order.
pub fn folders(cfg: &Config) -> Vec<(&'static str, PathBuf, bool)> {
    let under = |sub: &str| {
        if sub.is_empty() {
            None
        } else {
            Some(Path::new(&cfg.output_dir).join(sub))
        }
    };
    let lib = crate::server::library::dirs(cfg);
    let candidates = [
        ("Config", Some(PathBuf::from(&cfg.autorip_dir)), true),
        ("Staging", Some(PathBuf::from(&cfg.staging_dir)), true),
        ("Output", Some(PathBuf::from(&cfg.output_dir)), true),
        ("Movies", under(&cfg.movie_dir), true),
        ("TV", under(&cfg.tv_dir), true),
        ("ISOs", under(&cfg.iso_dir), true),
        ("Library", Some(lib.library), true),
        ("Source ISOs", lib.isos, false),
        ("Remux staging", remux_stage_dir(), true),
    ];
    let mut out: Vec<(&'static str, PathBuf, bool)> = Vec::new();
    for (role, path, write) in candidates {
        let Some(path) = path else { continue };
        if path.as_os_str().is_empty() || out.iter().any(|(_, p, _)| *p == path) {
            continue;
        }
        out.push((role, path, write));
    }
    out
}

// Blocks to bytes; a filesystem that reports an "unlimited" count saturates instead of wrapping
// (or panicking an overflow-checked build inside the check thread).
#[cfg(unix)]
fn scaled(blocks: u64, frag: u64) -> u64 {
    blocks.saturating_mul(frag)
}

/// The size of the filesystem holding `p`, in bytes (`None` where it cannot be read).
pub(crate) fn fs_capacity(p: &Path) -> Option<u64> {
    statvfs(p)
        .ok()
        .flatten()
        .map(|(_, total)| total)
        .filter(|t| *t > 0)
}

fn statvfs(p: &Path) -> std::io::Result<Option<(u64, u64)>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid NUL-terminated path and `s` a writable statvfs.
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let frag = s.f_frsize as u64;
        Ok(Some((
            scaled(s.f_bavail as u64, frag),
            scaled(s.f_blocks as u64, frag),
        )))
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        Ok(None)
    }
}

// Asks the filesystem (access W_OK, EROFS included) instead of writing a probe file into
// the user's folders. The error says why not.
fn writable(p: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: `c` is a valid NUL-terminated path.
        if unsafe { libc::access(c.as_ptr(), libc::W_OK) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        if std::fs::metadata(p)?.permissions().readonly() {
            Err(std::io::ErrorKind::PermissionDenied.into())
        } else {
            Ok(())
        }
    }
}

// Whether `path` lies under the share at `mountpoint` while `table` (the mount table, `None`
// where unreadable) lists no mount there: the folder is then a bare local directory.
fn bare_share(path: &Path, mountpoint: Option<&str>, table: Option<&str>) -> bool {
    let (Some(mp), Some(table)) = (mountpoint, table) else {
        return false;
    };
    path.starts_with(mp) && !crate::server::daemon::listed_in(table, mp)
}

// The configured share's mountpoint, when there is one.
fn share_mountpoint() -> Option<String> {
    #[cfg(unix)]
    {
        crate::server::daemon::nfs_share().map(|s| s.mountpoint)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Whether `path` lies under the configured network share while it is not mounted, so a
/// write there would land on the container's own disk. Reads the mount table, never the share.
pub(crate) fn share_unmounted(path: &Path) -> bool {
    let Some(mp) = share_mountpoint() else {
        return false;
    };
    bare_share(
        path,
        Some(&mp),
        crate::server::daemon::mount_table().as_deref(),
    )
}

const UNMOUNTED: &str = "the network share is not mounted";

/// Check one folder now, on the calling thread (it may block on a dead mount).
pub fn check(role: &'static str, path: &Path, want_write: bool) -> Mount {
    let started = Instant::now();
    let m = Mount::blank(role, path);
    if share_unmounted(path) {
        return m.failed(Fault::Unmounted, UNMOUNTED.into());
    }
    tracing::debug!(role, path = %path.display(), stage = "read_dir", "folder probe");
    match std::fs::read_dir(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return m.failed(Fault::Missing, "missing".into());
        }
        Err(e) => return m.failed(Fault::of(&e), format!("unreadable: {e}")),
    }
    let mut m = m;
    if want_write {
        tracing::debug!(role, path = %path.display(), stage = "access", "folder probe");
        let w = writable(path);
        m.writable = Some(w.is_ok());
        if let Err(e) = w {
            return m.failed(Fault::of(&e), format!("write access check failed: {e}"));
        }
    }
    tracing::debug!(role, path = %path.display(), stage = "statvfs", "folder probe");
    match statvfs(path) {
        Ok(Some((free, total))) => {
            m.free_bytes = Some(free);
            m.total_bytes = Some(total);
        }
        Ok(None) => {}
        Err(e) => return m.failed(Fault::of(&e), format!("capacity check failed: {e}")),
    }
    m.latency_ms = Some(started.elapsed().as_millis() as u64);
    m.passed()
}

/// How a bounded probe ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Bounded<T> {
    Done(T),
    /// It is still running after the limit; its thread is left to finish on its own.
    TimedOut,
    /// An earlier probe of the same folder never came back, so none was started.
    Busy,
}

// Folders whose probe has not come back, with when that probe's own limit ran out. A hung
// mount keeps one thread blocked; it never gets a second one.
struct InFlight {
    folders: Mutex<Vec<(PathBuf, Instant)>>,
    done: Condvar,
}

static IN_FLIGHT: InFlight = InFlight {
    folders: Mutex::new(Vec::new()),
    done: Condvar::new(),
};

fn release(key: &Path) {
    IN_FLIGHT
        .folders
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(p, _)| p != key);
    IN_FLIGHT.done.notify_all();
}

/// Run `probe` for the folder `key` on its own thread and wait at most `limit`.
/// A probe of `key` already running is waited for within the same limit; one past its own
/// limit means the folder hangs, and the answer is `Busy` at once.
pub fn bounded<T, F>(key: &Path, limit: Duration, probe: F) -> Bounded<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let deadline = Instant::now() + limit;
    {
        let mut busy = IN_FLIGHT.folders.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(late_at) = busy.iter().find(|(p, _)| p == key).map(|(_, t)| *t) {
            let now = Instant::now();
            if now >= deadline || now >= late_at {
                return Bounded::Busy;
            }
            busy = IN_FLIGHT
                .done
                .wait_timeout(busy, deadline.min(late_at) - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        busy.push((key.to_path_buf(), deadline));
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let owned = key.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("folder-probe".into())
        .spawn(move || {
            struct Release(PathBuf);
            impl Drop for Release {
                fn drop(&mut self) {
                    release(&self.0);
                }
            }
            let guard = Release(owned);
            let out = probe();
            drop(guard);
            let _ = tx.send(out);
        });
    if spawned.is_err() {
        release(key);
        return Bounded::Busy;
    }
    match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(v) => Bounded::Done(v),
        Err(_) => Bounded::TimedOut,
    }
}

// Keep stale-handle evidence even when its probe finishes after the caller timed out.
// A new mount generation makes late results from the detached mount irrelevant.
#[derive(Default)]
struct StaleReports {
    generation: u64,
    paths: std::collections::HashSet<PathBuf>,
}

impl StaleReports {
    fn record(&mut self, generation: u64, path: &Path, fault: Option<Fault>) {
        if generation == self.generation && fault == Some(Fault::Stale) {
            self.paths.insert(path.to_path_buf());
        }
    }

    fn renewed(&mut self, mountpoint: &Path) {
        self.generation = self.generation.wrapping_add(1);
        self.paths.retain(|p| !p.starts_with(mountpoint));
    }
}

static STALE_REPORTS: std::sync::LazyLock<Mutex<StaleReports>> =
    std::sync::LazyLock::new(|| Mutex::new(StaleReports::default()));

fn probe_generation() -> u64 {
    STALE_REPORTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .generation
}

fn report_fault(generation: u64, path: &Path, fault: Option<Fault>) {
    STALE_REPORTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(generation, path, fault);
}

// `check` through `bounded`, abandoned after CHECK_TIMEOUT.
fn check_bounded(role: &'static str, path: PathBuf, want_write: bool) -> Mount {
    let p = path.clone();
    let generation = probe_generation();
    match bounded(&path, CHECK_TIMEOUT, move || {
        let result = check(role, &p, want_write);
        report_fault(generation, &p, result.fault);
        tracing::debug!(role, path = %p.display(), fault = ?result.fault, "folder probe completed");
        result
    }) {
        Bounded::Done(m) => m,
        Bounded::TimedOut | Bounded::Busy => not_responding(role, &path, CHECK_TIMEOUT),
    }
}

fn not_responding(role: &'static str, path: &Path, after: Duration) -> Mount {
    Mount::blank(role, path).failed(
        Fault::Unresponsive,
        format!(
            "not responding after {}s (a stale network mount?)",
            after.as_secs()
        ),
    )
}

/// Whether the last check of `path` found it usable. `None` if never checked.
pub fn folder_ok(path: &Path) -> Option<bool> {
    LAST.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|m| m.path == path)
        .map(|m| m.ok)
}

// Run `check` for every folder at once, so folders on one dead mount cost one timeout between
// them, not one each. Results keep the input order.
fn check_each<F>(folders: Vec<(&'static str, PathBuf, bool)>, check: F) -> Vec<Mount>
where
    F: Fn(&'static str, PathBuf, bool) -> Mount + Sync,
{
    std::thread::scope(|scope| {
        let running: Vec<_> = folders
            .into_iter()
            .map(|(role, path, w)| {
                let check = &check;
                let again = path.clone();
                (role, again, scope.spawn(move || check(role, path, w)))
            })
            .collect();
        running
            .into_iter()
            .map(|(role, path, h)| {
                h.join()
                    .unwrap_or_else(|_| not_responding(role, &path, CHECK_TIMEOUT))
            })
            .collect()
    })
}

// Carry a folder's history (since, last good, last error) from its previous check.
fn merge(prev: Option<&Mount>, mut next: Mount) -> Mount {
    let Some(prev) = prev else { return next };
    if prev.state == next.state {
        next.since = prev.since;
    }
    if next.last_ok.is_none() {
        next.last_ok = prev.last_ok;
    }
    if next.last_error.is_none() {
        next.last_error = prev.last_error.clone();
        next.last_error_at = prev.last_error_at;
    }
    next
}

// Publish `results` (every folder) or one result (a preflight), keeping each folder's history.
fn publish(results: Vec<Mount>, replace_all: bool) {
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let shape = |v: &[Mount]| -> Vec<(PathBuf, State, Option<String>)> {
        v.iter()
            .map(|m| (m.path.clone(), m.state, m.message.clone()))
            .collect()
    };
    let before = shape(&last);
    let merged: Vec<Mount> = results
        .into_iter()
        .map(|m| {
            let previous = last.iter().find(|p| p.path == m.path);
            if previous.is_none_or(|p| p.fault != m.fault || p.state != m.state) {
                if m.ok {
                    tracing::info!(role = m.role, path = %m.path.display(), latency_ms = ?m.latency_ms, "folder healthy");
                } else {
                    tracing::warn!(role = m.role, path = %m.path.display(), fault = ?m.fault, detail = ?m.problem, "folder unavailable");
                }
            }
            merge(previous, m)
        })
        .collect();
    if replace_all {
        *last = merged;
    } else {
        for m in merged {
            match last.iter_mut().find(|p| p.path == m.path) {
                Some(slot) => {
                    *slot = Mount {
                        role: slot.role,
                        ..m
                    }
                }
                None => last.push(m),
            }
        }
    }
    if shape(&last) != before {
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
}

/// Check every folder once and publish the result.
pub fn refresh(cfg: &Config) {
    publish(check_each(folders(cfg), check_bounded), true);
}

/// Why a folder failed its preflight, in words the UI shows as they are.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Problem {
    pub role: &'static str,
    pub path: PathBuf,
    pub fault: Fault,
    pub message: String,
    pub hint: String,
    /// The raw error.
    pub detail: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}: {}) {}",
            self.message,
            self.path.display(),
            self.detail,
            self.hint
        )
    }
}

impl Problem {
    pub fn new(role: &'static str, path: &Path, fault: Fault, detail: String) -> Self {
        Problem {
            role,
            path: path.to_path_buf(),
            fault,
            message: fault.message(role),
            hint: fault.hint().into(),
            detail,
        }
    }

    /// The folder's state as a check would have published it.
    pub fn mount(&self) -> Mount {
        Mount::blank(self.role, &self.path).failed(self.fault, self.detail.clone())
    }
}

/// The write test: `dir` is a folder, and a file can be created in it and removed.
pub fn probe_write(dir: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "not a folder",
        ));
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(".freemkv-probe-{}-{nanos}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

/// Before a remux or a move starts: `dir` must answer a stat and a create-and-delete
/// within [`PREFLIGHT_TIMEOUT`]. Never blocks longer than that, whatever the mount does.
/// The answer is published as the folder's state.
pub fn preflight(role: &'static str, dir: &Path) -> Result<(), Problem> {
    preflight_with(role, dir, PREFLIGHT_TIMEOUT, probe_write)
}

/// [`preflight`] with the probe and the limit given: the seam the tests use.
pub fn preflight_with<F>(
    role: &'static str,
    dir: &Path,
    limit: Duration,
    probe: F,
) -> Result<(), Problem>
where
    F: FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
{
    let started = Instant::now();
    let owned = dir.to_path_buf();
    let result = if share_unmounted(dir) {
        Err(Problem::new(role, dir, Fault::Unmounted, UNMOUNTED.into()))
    } else {
        let generation = probe_generation();
        match bounded(dir, limit, move || {
            let result = probe(&owned);
            report_fault(generation, &owned, result.as_ref().err().map(Fault::of));
            result
        }) {
            Bounded::Done(Ok(())) => Ok(()),
            Bounded::Done(Err(e)) => Err(Problem::new(role, dir, Fault::of(&e), e.to_string())),
            Bounded::TimedOut => Err(Problem::new(
                role,
                dir,
                Fault::Unresponsive,
                format!("no answer within {}s", limit.as_secs().max(1)),
            )),
            Bounded::Busy => Err(Problem::new(
                role,
                dir,
                Fault::Unresponsive,
                "an earlier check of this folder never came back".into(),
            )),
        }
    };
    let mount = match &result {
        Ok(()) => {
            let mut m = Mount::blank(role, dir).passed();
            m.latency_ms = Some(started.elapsed().as_millis() as u64);
            m
        }
        Err(p) => p.mount(),
    };
    // Only a folder the health list already watches is updated; a preflight never adds rows.
    if LAST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|m| m.path == dir)
    {
        publish(vec![mount], false);
    }
    result
}

/// The least time between two remounts of the network share.
const REMOUNT_GAP: Duration = Duration::from_secs(60);

// Whether the share wants mounting afresh: it is not mounted, or a folder under it reports a
// stale handle; and the last attempt is old enough.
fn wants_remount(
    mounts: &[Mount],
    mountpoint: &Path,
    mounted: bool,
    last: Option<Instant>,
    now: Instant,
) -> bool {
    let stale = mounts
        .iter()
        .any(|m| m.fault == Some(Fault::Stale) && m.path.starts_with(mountpoint));
    (stale || !mounted) && last.is_none_or(|t| now.duration_since(t) >= REMOUNT_GAP)
}

// A stale handle on the container's own NFS mount only clears by mounting it afresh, and a
// failed mount is retried, so do that.
#[cfg(unix)]
fn heal(cfg: &Config) {
    static LAST_REMOUNT: Mutex<Option<Instant>> = Mutex::new(None);
    let Some(share) = crate::server::daemon::nfs_share() else {
        return;
    };
    // An unreadable table proves nothing: only a stale handle then triggers a remount.
    let mounted = crate::server::daemon::mount_table()
        .is_none_or(|t| crate::server::daemon::listed_in(&t, &share.mountpoint));
    let now = Instant::now();
    let mut evidence = mounts();
    {
        let reports = STALE_REPORTS.lock().unwrap_or_else(|e| e.into_inner());
        evidence.extend(reports.paths.iter().map(|path| {
            Mount::blank("Network share", path)
                .failed(Fault::Stale, "stale handle reported by probe".into())
        }));
    }
    {
        let mut last = LAST_REMOUNT.lock().unwrap_or_else(|e| e.into_inner());
        if !wants_remount(&evidence, Path::new(&share.mountpoint), mounted, *last, now) {
            return;
        }
        *last = Some(now);
    }
    tracing::warn!(mountpoint = %share.mountpoint, mounted, "recovering container-owned NFS mount");
    if crate::server::daemon::remount_nfs(&share, mounted) {
        STALE_REPORTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .renewed(Path::new(&share.mountpoint));
    }
    refresh(cfg);
}

/// Start the health thread.
pub fn start(cfg: &Arc<RwLock<Config>>) {
    let cfg = cfg.clone();
    let _ = std::thread::Builder::new()
        .name("health".into())
        .spawn(move || {
            while !crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
                let c = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
                refresh(&c);
                #[cfg(unix)]
                heal(&c);
                let until = Instant::now() + Duration::from_secs(INTERVAL_SECS);
                while Instant::now() < until
                    && !crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
                {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        });
}

#[cfg(test)]
#[path = "health_tests.rs"]
pub(crate) mod tests;
