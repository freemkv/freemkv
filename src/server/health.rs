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
    statvfs(p).map(|(_, total)| total).filter(|t| *t > 0)
}

fn statvfs(p: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid NUL-terminated path and `s` a writable statvfs.
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        let frag = s.f_frsize as u64;
        Some((
            scaled(s.f_bavail as u64, frag),
            scaled(s.f_blocks as u64, frag),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
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

/// Check one folder now, on the calling thread (it may block on a dead mount).
pub fn check(role: &'static str, path: &Path, want_write: bool) -> Mount {
    let started = Instant::now();
    let m = Mount::blank(role, path);
    match std::fs::read_dir(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return m.failed(Fault::Missing, "missing".into());
        }
        Err(e) => return m.failed(Fault::of(&e), format!("unreadable: {e}")),
    }
    let mut m = m;
    if want_write {
        let w = writable(path);
        m.writable = Some(w.is_ok());
        if let Err(e) = w {
            let fault = match Fault::of(&e) {
                Fault::ReadOnly => Fault::ReadOnly,
                _ => Fault::Denied,
            };
            return m.failed(fault, "not writable".into());
        }
    }
    if let Some((free, total)) = statvfs(path) {
        m.free_bytes = Some(free);
        m.total_bytes = Some(total);
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
            let out = probe();
            release(&owned);
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

// `check` through `bounded`, abandoned after CHECK_TIMEOUT.
fn check_bounded(role: &'static str, path: PathBuf, want_write: bool) -> Mount {
    let p = path.clone();
    match bounded(&path, CHECK_TIMEOUT, move || check(role, &p, want_write)) {
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
        .map(|m| merge(last.iter().find(|p| p.path == m.path), m))
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
    let result = match bounded(dir, limit, move || probe(&owned)) {
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

/// Start the health thread.
pub fn start(cfg: &Arc<RwLock<Config>>) {
    let cfg = cfg.clone();
    let _ = std::thread::Builder::new()
        .name("health".into())
        .spawn(move || {
            while !crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
                let c = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
                refresh(&c);
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
pub(crate) mod tests {
    use super::*;

    // LAST is process-wide; tests that publish to it take this.
    pub(crate) static LAST_LOCK: Mutex<()> = Mutex::new(());

    /// Put `mounts` in place as if a check had just published them.
    pub(crate) fn set_mounts(mounts: Vec<Mount>) {
        publish(mounts, true);
    }

    pub(crate) fn ok_mount(role: &'static str, path: &Path) -> Mount {
        Mount::blank(role, path).passed()
    }

    pub(crate) fn bad_mount(role: &'static str, path: &Path, fault: Fault) -> Mount {
        Mount::blank(role, path).failed(fault, "test".into())
    }

    fn errno(n: i32) -> std::io::Error {
        std::io::Error::from_raw_os_error(n)
    }

    #[test]
    fn a_folder_is_checked_for_presence_and_writes() {
        let t = tempfile::tempdir().unwrap();
        let m = check("Output", t.path(), true);
        assert!(m.ok && m.writable == Some(true), "{m:?}");
        assert_eq!(m.state, State::Ok);
        assert_eq!(
            std::fs::read_dir(t.path()).unwrap().count(),
            0,
            "the check wrote a file"
        );
        let gone = check("Output", &t.path().join("nope"), true);
        assert!(!gone.ok);
        assert_eq!(gone.problem.as_deref(), Some("missing"));
        assert_eq!(gone.fault, Some(Fault::Missing));
        assert!(gone.message.unwrap().contains("output folder is missing"));
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_folder_is_reported_not_writable() {
        use std::os::unix::fs::PermissionsExt as _;
        let t = tempfile::tempdir().unwrap();
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let m = check("Output", t.path(), true);
        let ro = std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o755));
        ro.unwrap();
        if unsafe { libc::geteuid() } == 0 {
            return; // root passes access(W_OK) on a 0555 dir
        }
        assert!(!m.ok, "{m:?}");
        assert_eq!(m.writable, Some(false));
        assert_eq!(m.problem.as_deref(), Some("not writable"));
        assert_eq!(m.fault, Some(Fault::Denied));
        // A folder only read from is fine.
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = check("Source ISOs", t.path(), false);
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(r.ok, "{r:?}");
    }

    #[test]
    fn a_stuck_check_is_never_doubled() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("stuck");
        let late = Instant::now() - Duration::from_secs(1);
        IN_FLIGHT.folders.lock().unwrap().push((p.clone(), late));
        let started = Instant::now();
        let m = check_bounded("Library", p.clone(), false);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a hung folder answers at once"
        );
        assert!(!m.ok);
        assert_eq!(m.state, State::Unresponsive);
        assert!(m.problem.unwrap().contains("not responding"));
        let n = IN_FLIGHT
            .folders
            .lock()
            .unwrap()
            .iter()
            .filter(|(x, _)| *x == p)
            .count();
        assert_eq!(n, 1);
        release(&p);
        let fine = check_bounded("Library", t.path().to_path_buf(), false);
        assert!(fine.ok);
        assert!(
            !IN_FLIGHT
                .folders
                .lock()
                .unwrap()
                .iter()
                .any(|(x, _)| x == t.path())
        );
    }

    #[test]
    fn a_hung_probe_returns_within_its_limit_and_is_never_started_twice() {
        let key = PathBuf::from("/test/hung-probe");
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let runs = Arc::new(AtomicU64::new(0));
        let r = runs.clone();
        let started = Instant::now();
        let out = bounded(&key, Duration::from_millis(200), move || {
            r.fetch_add(1, Ordering::SeqCst);
            let _ = rx.recv(); // a kernel call that never returns
        });
        let took = started.elapsed();
        assert_eq!(out, Bounded::TimedOut);
        assert!(took < Duration::from_millis(1500), "{took:?}");
        // The next probe of the same folder neither waits nor spawns.
        let r = runs.clone();
        let started = Instant::now();
        let again = bounded(&key, Duration::from_millis(200), move || {
            r.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(again, Bounded::Busy);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "one probe in flight per folder"
        );
        // Once the kernel lets go, the folder is probed again.
        drop(tx);
        let t = Instant::now();
        while IN_FLIGHT
            .folders
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| *p == key)
        {
            assert!(t.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            bounded(&key, Duration::from_secs(1), || 7),
            Bounded::Done(7)
        );
    }

    #[test]
    fn a_probe_already_running_but_not_yet_late_is_waited_for() {
        let key = PathBuf::from("/test/slow-probe");
        let first = {
            let key = key.clone();
            std::thread::spawn(move || {
                bounded(&key, Duration::from_secs(2), || {
                    std::thread::sleep(Duration::from_millis(150));
                    1
                })
            })
        };
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            bounded(&key, Duration::from_secs(2), || 2),
            Bounded::Done(2)
        );
        assert_eq!(first.join().unwrap(), Bounded::Done(1));
    }

    #[test]
    fn preflight_maps_each_failure_to_its_fault_in_bounded_time() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(preflight("Library", t.path()), Ok(()));
        assert_eq!(
            std::fs::read_dir(t.path()).unwrap().count(),
            0,
            "the probe file is removed"
        );
        let missing = preflight("Library", &t.path().join("gone")).unwrap_err();
        assert_eq!(missing.fault, Fault::Missing);
        #[cfg(unix)]
        for (n, fault, words) in [
            (libc::ESTALE, Fault::Stale, "remounted"),
            (libc::EIO, Fault::Io, "I/O error"),
            (libc::EROFS, Fault::ReadOnly, "read-only"),
            (libc::EACCES, Fault::Denied, "not allowed"),
            (libc::ETIMEDOUT, Fault::Unresponsive, "not responding"),
        ] {
            let dir = t.path().join(format!("errno-{n}"));
            let p = preflight_with("Library", &dir, Duration::from_secs(1), move |_| {
                Err(errno(n))
            })
            .unwrap_err();
            assert_eq!(p.fault, fault);
            assert!(p.message.contains(words), "{}", p.message);
            assert!(p.message.contains("library folder"), "{}", p.message);
        }
        let hung = t.path().join("hung");
        let started = Instant::now();
        let p = preflight_with("Output", &hung, Duration::from_millis(300), |_| {
            std::thread::sleep(Duration::from_secs(3));
            Ok(())
        })
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_millis(1500));
        assert_eq!(p.fault, Fault::Unresponsive);
        let again = Instant::now();
        let p =
            preflight_with("Output", &hung, Duration::from_millis(300), |_| Ok(())).unwrap_err();
        assert!(
            again.elapsed() < Duration::from_millis(100),
            "no second probe of a hung folder"
        );
        assert!(p.detail.contains("never came back"));
    }

    // A FIFO with no writer blocks open(2) for read in the kernel: a real hang, like a hard mount.
    #[cfg(unix)]
    #[test]
    fn a_probe_blocked_in_the_kernel_is_abandoned_on_time() {
        let t = tempfile::tempdir().unwrap();
        let fifo = t.path().join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        let f = fifo.clone();
        let p = preflight_with("Output", &fifo, Duration::from_millis(500), move |_| {
            std::fs::read(&f).map(drop)
        })
        .unwrap_err();
        let took = started.elapsed();
        assert!(took < Duration::from_millis(2000), "{took:?}");
        assert_eq!(p.fault, Fault::Unresponsive);
        // Unblock the leaked probe so the test leaves no thread behind.
        let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
    }

    #[test]
    fn errors_that_lost_their_code_are_read_from_their_text() {
        let e = std::io::Error::other("unreadable: Stale file handle (os error 116)");
        #[cfg(target_os = "linux")]
        assert_eq!(Fault::of(&e), Fault::Stale);
        assert_eq!(errno_in(&e.to_string()), Some(116));
        assert_eq!(
            Fault::of(&std::io::ErrorKind::TimedOut.into()),
            Fault::Unresponsive
        );
        assert_eq!(
            Fault::of(&std::io::Error::other("E9077: empty /x")),
            Fault::Other
        );
    }

    #[test]
    fn a_folder_keeps_its_history_across_checks() {
        let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p = Path::new("/test/history");
        let mut good = ok_mount("Library", p);
        good.checked_at = 100;
        good.last_ok = Some(100);
        good.since = 100;
        set_mounts(vec![good]);
        let g0 = generation();
        let mut bad = bad_mount("Library", p, Fault::Stale);
        bad.checked_at = 200;
        bad.since = 200;
        set_mounts(vec![bad.clone()]);
        let m = status_for(&p.join("A/A.mkv")).unwrap();
        assert_eq!(m.state, State::Unhealthy);
        assert_eq!((m.since, m.last_ok), (200, Some(100)));
        assert!(generation() > g0, "a state change is published");
        let g1 = generation();
        bad.checked_at = 230;
        bad.since = 230;
        set_mounts(vec![bad]);
        assert_eq!(
            status_for(p).unwrap().since,
            200,
            "still unhealthy since the first failure"
        );
        assert_eq!(generation(), g1, "an unchanged state is not re-sent");
        let mut back = ok_mount("Library", p);
        back.checked_at = 260;
        back.since = 260;
        back.last_ok = Some(260);
        set_mounts(vec![back]);
        let m = status_for(p).unwrap();
        assert_eq!((m.state, m.last_ok, m.since), (State::Ok, Some(260), 260));
        assert_eq!(
            m.last_error.as_deref(),
            Some("test"),
            "the last error is kept"
        );
        set_mounts(Vec::new());
    }

    #[test]
    fn folders_are_checked_concurrently_in_order() {
        let folders: Vec<_> = (0..5)
            .map(|i| ("Output", PathBuf::from(format!("/dead/{i}")), false))
            .collect();
        let started = Instant::now();
        let out = check_each(folders, |role, path, _| {
            std::thread::sleep(Duration::from_millis(300));
            ok_mount(role, &path)
        });
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "5 x 300ms checks ran one after another: {:?}",
            started.elapsed()
        );
        let paths: Vec<_> = out.iter().map(|m| m.path.clone()).collect();
        assert_eq!(paths[0], Path::new("/dead/0"));
        assert_eq!(paths[4], Path::new("/dead/4"));
    }

    #[cfg(unix)]
    #[test]
    fn an_unlimited_block_count_saturates() {
        assert_eq!(scaled(u64::MAX, 4096), u64::MAX);
        assert_eq!(scaled(10, 4096), 40_960);
    }

    #[test]
    fn folders_are_listed_once_each() {
        let c = Config {
            output_dir: "/o".into(),
            movie_dir: "m".into(),
            ..Config::default()
        };
        let f = folders(&c);
        assert!(
            f.iter()
                .any(|(r, p, _)| *r == "Movies" && p == Path::new("/o/m"))
        );
        // The library defaults to the movie folder: listed once, as Movies.
        assert_eq!(
            f.iter().filter(|(_, p, _)| p == Path::new("/o/m")).count(),
            1
        );
    }
}
