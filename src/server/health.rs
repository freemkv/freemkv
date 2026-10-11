//! Folder health for the System, Library and Remux pages, checked off the request path.
//!
//! A thread re-checks every configured folder each [`INTERVAL_SECS`]; each
//! check runs on its own short-lived thread, so a hung NFS mount never hangs
//! anything else. [`preflight`] is the same bounded probe plus a
//! create-and-delete write, run before a remux or a move starts. A folder never
//! has more than one probe in flight: a probe the kernel never returns from is
//! abandoned, not repeated. `GET /api/system` and the Library only read the
//! cached result.
//!
//! One verdict per folder, whoever asked: only an answer (published by the probe
//! itself, however late it comes) or a probe out for [`HANG_AFTER`] sets it. A
//! caller that stops waiting sooner decides nothing for anyone else, so a share
//! spinning its disks up (seconds per stat) reads slow, never down.

use crate::server::config::Config;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const INTERVAL_SECS: u64 = 30;
/// How long the health thread (and a settings save) waits for a check before moving on; a
/// check still running then is left to publish its answer when it comes.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// A probe out this long without an answer means the folder hangs: it is "not responding",
/// and further probes of it are refused at once until that probe returns. A NAS waking its
/// disks answers well inside it.
pub const HANG_AFTER: Duration = Duration::from_secs(60);
/// How long a preflight waits for its answer: until the folder counts as hung.
pub const PREFLIGHT_TIMEOUT: Duration = HANG_AFTER;

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

/// Whether the health monitor already reports `path` unusable, and so already logged it.
pub fn already_reported(path: &Path) -> bool {
    status_for(path).is_some_and(|m| !m.ok)
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
        (READ_ONLY_ROLE, lib.isos, false),
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
    /// No answer within the caller's limit (its probe, or the earlier one it waited for, is
    /// left to finish on its own). Slow, not yet hung.
    TimedOut,
    /// An earlier probe of the same folder has been out for the hang bound, so none was
    /// started.
    Busy,
}

// Folders whose probe has not come back, with when that probe started. A hung mount keeps
// one thread blocked; it never gets a second one.
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

/// How long the probe of `key` now in flight has been out, if one is.
pub fn in_flight_for(key: &Path) -> Option<Duration> {
    IN_FLIGHT
        .folders
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(p, _)| p == key)
        .map(|(_, started)| started.elapsed())
}

// The probe of `key` in flight for at least `hang_after`: how long it has been out.
fn hung_for(key: &Path, hang_after: Duration) -> Option<Duration> {
    in_flight_for(key).filter(|age| *age >= hang_after)
}

/// A started probe: wait for it with [`Pending::wait`].
pub struct Pending<T> {
    rx: Option<std::sync::mpsc::Receiver<T>>,
    deadline: Instant,
    // Refused because an earlier probe hangs (else: the wait for it ran out first).
    hung: bool,
}

/// Begin `probe` for the folder `key` on its own thread. A probe of `key` already running is
/// waited for within `limit` (then this one starts); one out for [`HANG_AFTER`] means the
/// folder hangs, and the answer is `Busy` at once. A probe that outlives its caller is left
/// running: it may touch only its own probe artifacts.
pub fn begin<T, F>(key: &Path, limit: Duration, probe: F) -> Pending<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    begin_until(key, Instant::now() + limit, probe)
}

/// [`begin`] with the deadline given, so a batch can share one deadline across its folders.
pub fn begin_until<T, F>(key: &Path, deadline: Instant, probe: F) -> Pending<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    begin_with(key, deadline, HANG_AFTER, probe)
}

// [`begin_until`] with the hang bound given: the seam the tests use.
fn begin_with<T, F>(key: &Path, deadline: Instant, hang_after: Duration, probe: F) -> Pending<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let refused = |hung| Pending {
        rx: None,
        deadline,
        hung,
    };
    {
        let mut held = IN_FLIGHT.folders.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(started) = held.iter().find(|(p, _)| p == key).map(|(_, t)| *t) {
            let now = Instant::now();
            let hangs_at = started + hang_after;
            if now >= hangs_at {
                return refused(true);
            }
            if now >= deadline {
                return refused(false);
            }
            held = IN_FLIGHT
                .done
                .wait_timeout(held, deadline.min(hangs_at) - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        held.push((key.to_path_buf(), Instant::now()));
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
        return refused(false);
    }
    Pending {
        rx: Some(rx),
        deadline,
        hung: false,
    }
}

impl<T> Pending<T> {
    /// The probe's answer, or `TimedOut` once its limit has run out; `Busy` if an earlier
    /// probe hangs.
    pub fn wait(self) -> Bounded<T> {
        let Some(rx) = self.rx else {
            return if self.hung {
                Bounded::Busy
            } else {
                Bounded::TimedOut
            };
        };
        match rx.recv_timeout(self.deadline.saturating_duration_since(Instant::now())) {
            Ok(v) => Bounded::Done(v),
            Err(_) => Bounded::TimedOut,
        }
    }
}

/// [`begin`] and wait: `probe` for `key` within `limit`.
pub fn bounded<T, F>(key: &Path, limit: Duration, probe: F) -> Bounded<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    begin(key, limit, probe).wait()
}

/// Run `f` on every item at once, each bounded by `limit` on its folder, so folders on one
/// dead mount cost one timeout between them. Results keep the input order.
pub fn bounded_each<I, T, F>(items: Vec<(PathBuf, I)>, limit: Duration, f: F) -> Vec<Bounded<T>>
where
    I: Send + 'static,
    T: Send + 'static,
    F: Fn(I) -> T + Send + Sync + 'static,
{
    bounded_each_with(items, limit, HANG_AFTER, f)
}

fn bounded_each_with<I, T, F>(
    items: Vec<(PathBuf, I)>,
    limit: Duration,
    hang_after: Duration,
    f: F,
) -> Vec<Bounded<T>>
where
    I: Send + 'static,
    T: Send + 'static,
    F: Fn(I) -> T + Send + Sync + 'static,
{
    let f = Arc::new(f);
    let deadline = Instant::now() + limit;
    let started: Vec<_> = items
        .into_iter()
        .map(|(key, item)| {
            let f = f.clone();
            begin_with(&key, deadline, hang_after, move || f(item))
        })
        .collect();
    started.into_iter().map(Pending::wait).collect()
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

// `check` for one folder, reporting a stale handle even when the caller gave up on it.
fn check_reporting(role: &'static str, path: PathBuf, want_write: bool) -> Mount {
    let generation = probe_generation();
    let result = check(role, &path, want_write);
    report_fault(generation, &path, result.fault);
    tracing::debug!(role, path = %path.display(), fault = ?result.fault, "folder probe completed");
    result
}

// Check every folder at once, waiting up to `wait` between them; results keep the input
// order. `None`: no answer yet but not hung either, so no verdict: its probe publishes the
// answer when it comes. A probe out for `hang_after` makes the folder "not responding".
fn check_folders(
    folders: Vec<(&'static str, PathBuf, bool)>,
    wait: Duration,
    hang_after: Duration,
) -> Vec<Option<Mount>> {
    let items = folders
        .iter()
        .map(|(role, path, w)| (path.clone(), (*role, path.clone(), *w)))
        .collect();
    let results = bounded_each_with(items, wait, hang_after, |(role, path, w)| {
        let m = check_reporting(role, path, w);
        publish_answer(m.clone());
        m
    });
    folders
        .iter()
        .zip(results)
        .map(|((role, path, _), r)| match r {
            Bounded::Done(m) => Some(m),
            Bounded::TimedOut | Bounded::Busy => {
                hung_for(path, hang_after).map(|age| not_responding(role, path, age))
            }
        })
        .collect()
}

fn not_responding(role: &'static str, path: &Path, after: Duration) -> Mount {
    Mount::blank(role, path).failed(Fault::Unresponsive, no_answer_for(after))
}

fn no_answer_for(after: Duration) -> String {
    format!(
        "not responding: no answer for {}s (a hung network mount?)",
        after.as_secs()
    )
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

/// The one watched folder that is only read, never written.
const READ_ONLY_ROLE: &str = "Source ISOs";

// The syslog line for a writable folder that has just stopped working; where finished rips
// land, it says they wait in staging.
fn unusable_line(role: &str, m: &Mount) -> String {
    let mut line = format!(
        "WARNING: {role} folder '{}' is not usable: {}.",
        m.path.display(),
        m.problem.as_deref().unwrap_or("unknown problem")
    );
    if matches!(
        role.to_lowercase().as_str(),
        "output" | "movies" | "tv" | "isos"
    ) {
        line.push_str(
            " Finished rips will be PRESERVED in staging (not moved) until this is fixed \
             (check the directory exists and its bind-mount/NAS share is present and writable).",
        );
    }
    line
}

// One row of a full publish: a new result, or the folder's current row kept as it is (its
// check is still running).
enum Slot {
    New(Box<Mount>),
    Keep(PathBuf),
}

// The folders the health thread watches, by role: a late answer for one of them is published
// even before its first row exists.
static WATCHED: Mutex<Vec<(&'static str, PathBuf)>> = Mutex::new(Vec::new());

// Publish a probe's answer, whenever it comes, for a folder the health list shows or watches.
fn publish_answer(mut m: Mount) {
    let watched = WATCHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(_, p)| *p == m.path)
        .map(|(role, _)| *role);
    let shown = LAST
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|x| x.path == m.path);
    if let Some(role) = watched {
        m.role = role;
    }
    if watched.is_some() || shown {
        publish(vec![m], false);
    }
}

// Publish `results` (every folder) or one result (a preflight), keeping each folder's history.
fn publish(results: Vec<Mount>, replace_all: bool) {
    publish_slots(
        results
            .into_iter()
            .map(|m| Slot::New(Box::new(m)))
            .collect(),
        replace_all,
    );
}

fn publish_slots(results: Vec<Slot>, replace_all: bool) {
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let shape = |v: &[Mount]| -> Vec<(PathBuf, State, Option<String>)> {
        v.iter()
            .map(|m| (m.path.clone(), m.state, m.message.clone()))
            .collect()
    };
    let before = shape(&last);
    let mut lines = Vec::new();
    let mut merged: Vec<Mount> = Vec::new();
    for slot in results {
        let m = match slot {
            Slot::New(m) => *m,
            // Unchanged, so neither logged nor merged again.
            Slot::Keep(path) => {
                merged.extend(last.iter().find(|p| p.path == path).cloned());
                continue;
            }
        };
        let previous = last.iter().find(|p| p.path == m.path);
        let role = previous.map_or(m.role, |p| p.role);
        if !m.ok && role != READ_ONLY_ROLE && previous.is_none_or(|p| p.ok) {
            lines.push(unusable_line(role, &m));
        }
        if previous.is_none_or(|p| p.fault != m.fault || p.state != m.state) {
            if m.ok {
                tracing::info!(role = m.role, path = %m.path.display(), latency_ms = ?m.latency_ms, "folder healthy");
            } else {
                tracing::warn!(role = m.role, path = %m.path.display(), fault = ?m.fault, detail = ?m.problem, "folder unavailable");
            }
        }
        merged.push(merge(previous, m));
    }
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
    drop(last);
    for line in lines {
        crate::server::log::syslog(&line);
    }
}

/// Check every folder once and publish the result. A folder that has not answered within
/// [`CHECK_TIMEOUT`] keeps its last verdict until its check answers or hangs.
pub fn refresh(cfg: &Config) {
    let folders = folders(cfg);
    *WATCHED.lock().unwrap_or_else(|e| e.into_inner()) =
        folders.iter().map(|(r, p, _)| (*r, p.clone())).collect();
    let outcomes = check_folders(folders.clone(), CHECK_TIMEOUT, HANG_AFTER);
    let slots = folders
        .into_iter()
        .zip(outcomes)
        .map(|((_, path, _), m)| m.map_or(Slot::Keep(path), |m| Slot::New(Box::new(m))))
        .collect();
    publish_slots(slots, true);
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

/// The detail of a preflight that found a file where the folder should be.
pub const NOT_A_FOLDER: &str = "not a folder";

/// The write test: `dir` is a folder, and a file can be created in it and removed.
pub fn probe_write(dir: &Path) -> std::io::Result<()> {
    probe_write_with(dir, |p| std::fs::remove_file(p))
}

// [`probe_write`] with the removal given: the seam the tests use.
fn probe_write_with(
    dir: &Path,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let meta = std::fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            NOT_A_FOLDER,
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
    // Writability is proven; a marker left behind is a warning, never a failed folder.
    if let Err(e) = remove(&probe) {
        tracing::warn!(probe = %probe.display(), error = %e, "writability probe left behind");
    }
    Ok(())
}

/// Before a remux or a move starts: `dir` must answer a stat and a create-and-delete
/// within [`PREFLIGHT_TIMEOUT`] (a slow share is waited for; one whose probe is out for
/// [`HANG_AFTER`] is refused, at once once that is known). Never blocks longer than that,
/// whatever the mount does. The answer is published as the folder's state.
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
    preflight_hanging_after(role, dir, limit, HANG_AFTER, probe)
}

// [`preflight_with`] with the hang bound given: the seam the tests use.
fn preflight_hanging_after<F>(
    role: &'static str,
    dir: &Path,
    limit: Duration,
    hang_after: Duration,
    probe: F,
) -> Result<(), Problem>
where
    F: FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
{
    let started = preflight_begin(role, dir, Instant::now() + limit, hang_after, probe);
    preflight_finish(role, dir, limit, hang_after, started)
}

/// [`preflight_with`] for several folders at once, so folders on one dead mount cost one
/// limit between them. Results keep the input order.
pub fn preflight_each<F>(
    role: &'static str,
    dirs: &[PathBuf],
    limit: Duration,
    probe: F,
) -> Vec<Result<(), Problem>>
where
    F: Fn(&Path) -> std::io::Result<()> + Copy + Send + 'static,
{
    let deadline = Instant::now() + limit;
    let started: Vec<_> = dirs
        .iter()
        .map(|d| preflight_begin(role, d, deadline, HANG_AFTER, probe))
        .collect();
    dirs.iter()
        .zip(started)
        .map(|(d, s)| preflight_finish(role, d, limit, HANG_AFTER, s))
        .collect()
}

// A preflight under way: refused up front, or its probe running.
type Started = Result<Pending<std::io::Result<()>>, Problem>;

// The probe publishes its own answer, however late: the folder's verdict is the answer, not
// whether this caller was still waiting for it.
fn preflight_begin<F>(
    role: &'static str,
    dir: &Path,
    deadline: Instant,
    hang_after: Duration,
    probe: F,
) -> Started
where
    F: FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
{
    if share_unmounted(dir) {
        return Err(Problem::new(role, dir, Fault::Unmounted, UNMOUNTED.into()));
    }
    let owned = dir.to_path_buf();
    let generation = probe_generation();
    Ok(begin_with(dir, deadline, hang_after, move || {
        let started = Instant::now();
        let result = probe(&owned);
        report_fault(generation, &owned, result.as_ref().err().map(Fault::of));
        publish_answer(match &result {
            Ok(()) => {
                let mut m = Mount::blank(role, &owned).passed();
                m.latency_ms = Some(started.elapsed().as_millis() as u64);
                m
            }
            Err(e) => Problem::new(role, &owned, Fault::of(e), e.to_string()).mount(),
        });
        result
    }))
}

fn preflight_finish(
    role: &'static str,
    dir: &Path,
    limit: Duration,
    hang_after: Duration,
    begun: Started,
) -> Result<(), Problem> {
    let unresponsive = |detail: String| Problem::new(role, dir, Fault::Unresponsive, detail);
    let pending = match begun {
        Ok(pending) => pending,
        Err(p) => {
            publish_answer(p.mount());
            return Err(p);
        }
    };
    match pending.wait() {
        // Already published by the probe.
        Bounded::Done(Ok(())) => Ok(()),
        Bounded::Done(Err(e)) => Err(Problem::new(role, dir, Fault::of(&e), e.to_string())),
        // A probe out for the hang bound is the verdict; a shorter wait is only this caller's.
        Bounded::TimedOut | Bounded::Busy => match hung_for(dir, hang_after) {
            Some(age) => {
                let p = unresponsive(no_answer_for(age));
                publish_answer(p.mount());
                Err(p)
            }
            None => Err(unresponsive(format!(
                "no answer within {}s yet (still checking)",
                limit.as_secs().max(1)
            ))),
        },
    }
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

/// Warnings for destination roots that are empty or relative: the checks resolve them against
/// the working directory, so such a root can look healthy. Pure; no I/O.
pub fn root_warnings(cfg: &Config) -> Vec<String> {
    [("Output", &cfg.output_dir), ("Staging", &cfg.staging_dir)]
        .into_iter()
        .filter_map(|(role, dir)| {
            let why = if dir.trim().is_empty() {
                "is not set"
            } else if !Path::new(dir).is_absolute() {
                "is not an absolute path"
            } else {
                return None;
            };
            Some(format!(
                "WARNING: {role} folder '{dir}' {why}; use an absolute path or finished rips may land in the wrong place."
            ))
        })
        .collect()
}

/// Start the health thread; `None` if it could not be spawned.
pub fn start(cfg: &Arc<RwLock<Config>>) -> Option<std::thread::JoinHandle<()>> {
    let cfg = cfg.clone();
    std::thread::Builder::new()
        .name("health".into())
        .spawn(move || {
            let first = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
            for line in root_warnings(&first) {
                crate::server::log::syslog(&line);
            }
            while !crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
                let c = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
                refresh(&c);
                #[cfg(unix)]
                heal(&c);
                crate::server::sleep_unless_shutdown(Duration::from_secs(INTERVAL_SECS));
            }
        })
        .ok()
}

#[cfg(test)]
#[path = "health_tests.rs"]
pub(crate) mod tests;
