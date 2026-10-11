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
    /// A check has been out for the hang bound without an answer.
    Unresponsive,
    /// A check has been out longer than [`CHECK_TIMEOUT`] but not yet the hang bound: not
    /// usable yet (work waits), not failed either.
    Checking,
}

impl State {
    /// It answered with an error or hangs: the folder has failed, not merely slow.
    pub fn failed(self) -> bool {
        matches!(self, State::Unhealthy | State::Unresponsive)
    }
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
    // When this result's evidence was seen, in publish order: an older result never
    // replaces a newer one, whichever is published last.
    #[serde(skip)]
    observed: u64,
}

// Orders the evidence behind published results.
static OBSERVED: AtomicU64 = AtomicU64::new(1);

fn observed_now() -> u64 {
    OBSERVED.fetch_add(1, Ordering::SeqCst)
}

impl Mount {
    fn blank(role: &'static str, path: &Path) -> Self {
        let now = crate::server::util::epoch_secs();
        Mount {
            observed: observed_now(),
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

    // A check out longer than the health thread waits, not yet hung. No latency (it is not
    // known yet) and no age in the words: the row is published once per round, so a number
    // would freeze; `since` says how long it has read this way.
    fn checking(role: &'static str, path: &Path) -> Self {
        let mut m = Mount::blank(role, path);
        m.state = State::Checking;
        m.problem = Some("checking — no answer yet".into());
        m.message = Some(checking_message(role));
        m.hint = Some(CHECKING_HINT.into());
        m
    }

    // Ranked as evidence seen at `seq` (the start of the probe it is about).
    fn ranked_at(mut self, seq: u64) -> Self {
        self.observed = seq;
        self
    }
}

const CHECKING_HINT: &str = "Nothing to do yet: work for this folder waits until it answers (a NAS may be waking its disks).";

/// The sentence for a folder whose check is out without an answer yet.
pub fn checking_message(role: &str) -> String {
    format!(
        "The {} folder has not answered yet — still checking.",
        role.to_lowercase()
    )
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

/// Whether the health monitor already reports `path` failed, and so already logged it.
pub fn already_reported(path: &Path) -> bool {
    status_for(path).is_some_and(|m| m.state.failed())
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

// `m` as seen now: its evidence is the answer that just came in.
fn answered(mut m: Mount) -> Mount {
    m.observed = observed_now();
    m
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

// Which mount a probe ran against: the kernel's id of the mount holding the path, or where
// there is none, health's own remount generation. A probe stuck on a mount that has since been
// replaced does not hold up the new one; one stuck on any other mount still does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MountKey {
    generation: u64,
    mount: Option<u64>,
}

fn mount_key(path: &Path) -> MountKey {
    key_for(probe_generation(), mount_id(path))
}

// The generation counts only without a kernel mount id: health's remount of its own share
// changes that share's id, and must not re-key every other folder (each would get a second
// probe thread stuck beside the first).
fn key_for(generation: u64, mount: Option<u64>) -> MountKey {
    MountKey {
        generation: if mount.is_none() { generation } else { 0 },
        mount,
    }
}

// The kernel's id of the mount holding `path`, read from the mount table (never the mount).
// `path` is compared as written: a configured folder that is a symlink into another mount
// resolves to the mount holding the link, not its target (its probe then follows the link).
#[cfg(target_os = "linux")]
fn mount_id(path: &Path) -> Option<u64> {
    mount_id_in(&std::fs::read_to_string("/proc/self/mountinfo").ok()?, path)
}

// The id (first field) of the `mountinfo` line whose mount point (fifth) holds `path`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn mount_id_in(mountinfo: &str, path: &Path) -> Option<u64> {
    mountinfo
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let id = f.next()?.parse::<u64>().ok()?;
            let mp = crate::server::daemon::unescape_mount_path(f.nth(3)?);
            Some((id, PathBuf::from(mp)))
        })
        .filter(|(_, mp)| path.starts_with(mp))
        .max_by_key(|(_, mp)| mp.components().count())
        .map(|(id, _)| id)
}

#[cfg(target_os = "macos")]
fn mount_id(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt as _;
    // SAFETY: a null buffer asks only for the count.
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return None;
    }
    let cap = n as usize + 8;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(cap);
    let bytes = (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: `buf` has room for `cap` entries; MNT_NOWAIT never asks a mount to answer.
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if got < 0 {
        return None;
    }
    // SAFETY: getfsstat filled `got` entries.
    unsafe { buf.set_len(got as usize) };
    buf.iter()
        .map(|s| {
            // SAFETY: f_mntonname is NUL-terminated by the kernel.
            let name = unsafe { std::ffi::CStr::from_ptr(s.f_mntonname.as_ptr()) };
            // SAFETY: fsid_t is two i32s; read as one u64 (it is unique per mount).
            let fsid = unsafe {
                std::ptr::read_unaligned((&s.f_fsid as *const libc::fsid_t).cast::<u64>())
            };
            (
                fsid,
                PathBuf::from(std::ffi::OsStr::from_bytes(name.to_bytes())),
            )
        })
        .filter(|(_, mp)| path.starts_with(mp))
        .max_by_key(|(_, mp)| mp.components().count())
        .map(|(id, _)| id)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn mount_id(_: &Path) -> Option<u64> {
    None
}

// One probe that has not come back: its folder, the mount it ran against, and when it
// started (`None`: earlier than anything a caller can measure, so hung for any bound). A hung
// mount keeps one thread blocked; it never gets a second one.
struct Probe {
    id: u64,
    path: PathBuf,
    mount: MountKey,
    started: Option<Instant>,
    // Evidence order when it started: a verdict drawn from this probe being out ranks below
    // the probe's own answer, whenever either is published.
    seq: u64,
}

impl Probe {
    fn age(&self) -> Duration {
        self.started.map_or(Duration::MAX, |t| t.elapsed())
    }
}

struct InFlight {
    probes: Mutex<Vec<Probe>>,
    done: Condvar,
}

static IN_FLIGHT: InFlight = InFlight {
    probes: Mutex::new(Vec::new()),
    done: Condvar::new(),
};
static NEXT_PROBE: AtomicU64 = AtomicU64::new(0);

fn release(id: u64) {
    IN_FLIGHT
        .probes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|p| p.id != id);
    IN_FLIGHT.done.notify_all();
}

// How long the probe of `path` on `mount` has been out, and its start's evidence order.
fn in_flight_for(path: &Path, mount: MountKey) -> Option<(Duration, u64)> {
    IN_FLIGHT
        .probes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|p| p.path == path && p.mount == mount)
        .map(|p| (p.age(), p.seq))
}

// The probe of `path` on `mount` in flight for at least `hang_after`.
fn hung_for(path: &Path, mount: MountKey, hang_after: Duration) -> Option<(Duration, u64)> {
    in_flight_for(path, mount).filter(|(age, _)| *age >= hang_after)
}

// Whether the server is stopping: a waiting caller gives up at once. Tests drive their own
// thread's flag instead of the process-wide one other tests share.
fn stopping() -> bool {
    #[cfg(test)]
    {
        tests::STOPPING.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        crate::server::SHUTDOWN.load(Ordering::Relaxed)
    }
}

// The longest a wait sleeps before it looks at `stopping` again.
const STOP_POLL: Duration = Duration::from_millis(200);

/// A started probe: wait for it with [`Pending::wait`].
pub struct Pending<T> {
    rx: Option<std::sync::mpsc::Receiver<T>>,
    deadline: Instant,
    // Refused because an earlier probe hangs (else: the wait for it ran out first).
    hung: bool,
    mount: MountKey,
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
    begin_with(key, Wait::Until(deadline), HANG_AFTER, probe)
}

// How long a caller waits for an answer.
#[derive(Clone, Copy)]
enum Wait {
    Until(Instant),
    // Until an answer or the hang verdict, whichever comes first.
    UntilHung,
}

// [`begin_until`] with the wait and the hang bound given.
fn begin_with<T, F>(key: &Path, wait: Wait, hang_after: Duration, probe: F) -> Pending<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    begin_keyed(key, mount_key(key), wait, hang_after, probe)
}

// [`begin_with`] on the mount given: the seam the tests use.
fn begin_keyed<T, F>(
    key: &Path,
    mount: MountKey,
    wait: Wait,
    hang_after: Duration,
    probe: F,
) -> Pending<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let far = Instant::now() + Duration::from_secs(365 * 24 * 3600);
    let deadline = match wait {
        Wait::Until(t) => t,
        Wait::UntilHung => far,
    };
    let refused = |hung| Pending {
        rx: None,
        deadline,
        hung,
        mount,
    };
    let id = NEXT_PROBE.fetch_add(1, Ordering::Relaxed);
    let started = {
        let mut held = IN_FLIGHT.probes.lock().unwrap_or_else(|e| e.into_inner());
        while let Some(age) = held
            .iter()
            .find(|p| p.path == key && p.mount == mount)
            .map(Probe::age)
        {
            if age >= hang_after {
                return refused(true);
            }
            let now = Instant::now();
            if now >= deadline || stopping() {
                return refused(false);
            }
            let slice = (hang_after - age).min(deadline - now).min(STOP_POLL);
            #[cfg(test)]
            tests::waiting(key);
            held = IN_FLIGHT
                .done
                .wait_timeout(held, slice)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        // Nothing is started for a server that is stopping.
        if stopping() {
            return refused(false);
        }
        let started = Instant::now();
        held.push(Probe {
            id,
            path: key.to_path_buf(),
            mount,
            started: Some(started),
            seq: observed_now(),
        });
        started
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("folder-probe".into())
        .spawn(move || {
            struct Release(u64);
            impl Drop for Release {
                fn drop(&mut self) {
                    release(self.0);
                }
            }
            let guard = Release(id);
            let out = probe();
            // Answer before leaving the in-flight list, so a caller that finds the probe gone
            // also finds its answer.
            let _ = tx.send(out);
            drop(guard);
        });
    if spawned.is_err() {
        release(id);
        return refused(false);
    }
    Pending {
        rx: Some(rx),
        // Our own probe is waited for until it hangs, counted from when it started.
        deadline: match wait {
            Wait::Until(t) => t,
            Wait::UntilHung => started + hang_after,
        },
        hung: false,
        mount,
    }
}

impl<T> Pending<T> {
    /// The probe's answer, or `TimedOut` once its limit has run out (or the server stops);
    /// `Busy` if an earlier probe hangs.
    pub fn wait(self) -> Bounded<T> {
        let Some(rx) = self.rx else {
            return if self.hung {
                Bounded::Busy
            } else {
                Bounded::TimedOut
            };
        };
        loop {
            let left = self.deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left.min(STOP_POLL)) {
                Ok(v) => return Bounded::Done(v),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Bounded::TimedOut,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if left <= STOP_POLL || stopping() {
                        // An answer that landed with the deadline still counts.
                        return rx.try_recv().map_or(Bounded::TimedOut, Bounded::Done);
                    }
                }
            }
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
        .into_iter()
        .map(|(b, _)| b)
        .collect()
}

// [`bounded_each`] with the hang bound given; each result with the mount its probe ran on.
fn bounded_each_with<I, T, F>(
    items: Vec<(PathBuf, I)>,
    limit: Duration,
    hang_after: Duration,
    f: F,
) -> Vec<(Bounded<T>, MountKey)>
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
            begin_with(&key, Wait::Until(deadline), hang_after, move || f(item))
        })
        .collect();
    started
        .into_iter()
        .map(|p| {
            let mount = p.mount;
            (p.wait(), mount)
        })
        .collect()
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
    let result = answered(check(role, &path, want_write));
    report_fault(generation, &path, result.fault);
    tracing::debug!(role, path = %path.display(), fault = ?result.fault, "folder probe completed");
    result
}

// Check every folder at once, waiting up to `wait` between them; results keep the input
// order. A probe out longer than `wait` reads "checking", one out for `hang_after` "not
// responding"; `None` for a probe out less than `wait` (one started by someone else just now).
// Either way the probe publishes its answer when it comes.
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
        .map(|((role, path, _), (r, mount))| match r {
            Bounded::Done(m) => Some(m),
            Bounded::TimedOut | Bounded::Busy => match in_flight_for(path, mount) {
                Some((age, seq)) if age >= hang_after => {
                    Some(not_responding(role, path, age).ranked_at(seq))
                }
                Some((age, seq)) if age >= wait => Some(Mount::checking(role, path).ranked_at(seq)),
                _ => None,
            },
        })
        .collect()
}

fn not_responding(role: &'static str, path: &Path, after: Duration) -> Mount {
    Mount::blank(role, path).failed(
        Fault::Unresponsive,
        format!(
            "not responding: {} (a hung network mount?)",
            no_answer_for(after)
        ),
    )
}

fn no_answer_for(after: Duration) -> String {
    format!("no answer for {}s", after.as_secs())
}

// Carry a folder's history (since, last good, last error) from its previous check, and its last
// capacity figures while it is not failed and the new result has none (a preflight, a check
// still running).
fn merge(prev: Option<&Mount>, mut next: Mount) -> Mount {
    let Some(prev) = prev else { return next };
    if !next.state.failed() && next.total_bytes.is_none() {
        next.free_bytes = prev.free_bytes;
        next.total_bytes = prev.total_bytes;
        // A usable row never inherits "not writable" (a passed preflight just wrote there).
        next.writable = next.writable.or(prev.writable.filter(|w| *w || !next.ok));
    }
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
        // Evidence older than the row's (a hang verdict reached just before the late answer
        // that is already shown) never replaces it, whichever was published last.
        if let Some(newer) = previous.filter(|p| p.observed > m.observed) {
            merged.push(newer.clone());
            continue;
        }
        let role = previous.map_or(m.role, |p| p.role);
        if m.state.failed() && role != READ_ONLY_ROLE && previous.is_none_or(|p| !p.state.failed())
        {
            lines.push(unusable_line(role, &m));
        }
        if previous.is_none_or(|p| p.fault != m.fault || p.state != m.state) {
            if m.ok {
                tracing::info!(role = m.role, path = %m.path.display(), latency_ms = ?m.latency_ms, "folder healthy");
            } else if m.state == State::Checking {
                // A NAS waking its disks flips this often: not a warning.
                tracing::info!(role = m.role, path = %m.path.display(), "folder slow to answer; still checking");
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
/// [`CHECK_TIMEOUT`] reads "checking" until its check answers or hangs.
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
    let started = preflight_begin(role, dir, wait_for(limit, hang_after), hang_after, probe);
    preflight_finish(role, dir, limit, hang_after, started)
}

// A limit that reaches the hang bound waits for the answer or the hang verdict, counted from
// when the probe actually started (not from this call); a shorter one waits that long.
fn wait_for(limit: Duration, hang_after: Duration) -> Wait {
    if limit >= hang_after {
        Wait::UntilHung
    } else {
        Wait::Until(Instant::now() + limit)
    }
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
    let wait = wait_for(limit, HANG_AFTER);
    let started: Vec<_> = dirs
        .iter()
        .map(|d| preflight_begin(role, d, wait, HANG_AFTER, probe))
        .collect();
    dirs.iter()
        .zip(started)
        .map(|(d, s)| preflight_finish(role, d, limit, HANG_AFTER, s))
        .collect()
}

/// In the detail of a preflight that stopped waiting before the folder answered or hung.
pub const STILL_CHECKING: &str = "still checking";

// The detail of a preflight cut short because the server is stopping.
const STOPPED: &str = "the server is stopping; still checking";

impl Problem {
    /// The preflight was cut short by the server stopping: it says nothing about the folder.
    pub fn stopped(&self) -> bool {
        self.detail == STOPPED
    }
}

// A preflight under way: refused up front, or its probe running.
type Started = Result<Pending<std::io::Result<()>>, Problem>;

// The probe publishes its own answer, however late: the folder's verdict is the answer, not
// whether this caller was still waiting for it.
fn preflight_begin<F>(
    role: &'static str,
    dir: &Path,
    wait: Wait,
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
    Ok(begin_with(dir, wait, hang_after, move || {
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
    let mount = pending.mount;
    match pending.wait() {
        // Already published by the probe.
        Bounded::Done(Ok(())) => Ok(()),
        Bounded::Done(Err(e)) => Err(Problem::new(role, dir, Fault::of(&e), e.to_string())),
        // A probe out for the hang bound is the verdict; a shorter wait is only this caller's.
        Bounded::TimedOut | Bounded::Busy => match hung_for(dir, mount, hang_after) {
            Some((age, seq)) => {
                let p = unresponsive(no_answer_for(age));
                publish_answer(p.mount().ranked_at(seq));
                Err(p)
            }
            None if stopping() => Err(unresponsive(STOPPED.into())),
            None => Err(unresponsive(format!(
                "no answer within {}s yet ({STILL_CHECKING})",
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
