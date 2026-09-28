//! Folder health for the System page, checked off the request path.
//!
//! A thread re-checks every configured folder each [`INTERVAL_SECS`]; each
//! check runs on its own short-lived thread and is given [`CHECK_TIMEOUT`],
//! so a hung NFS mount reads as "not responding" instead of hanging anything.
//! `GET /api/system` only reads the cached result.

use crate::server::config::Config;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const INTERVAL_SECS: u64 = 30;
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

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
}

static LAST: Mutex<Vec<Mount>> = Mutex::new(Vec::new());
// Folders whose last check has not come back. A hung mount keeps one thread
// blocked; it never gets a second one.
static IN_FLIGHT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The last completed check.
pub fn mounts() -> Vec<Mount> {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
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
        Some((s.f_bavail as u64 * frag, s.f_blocks as u64 * frag))
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
    }
}

// Asks the filesystem (access W_OK, EROFS included) instead of writing a probe file into
// the user's folders.
fn writable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `c` is a valid NUL-terminated path.
        unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(p).is_ok_and(|m| !m.permissions().readonly())
    }
}

/// Check one folder now, on the calling thread (it may block on a dead mount).
pub fn check(role: &'static str, path: &Path, want_write: bool) -> Mount {
    let started = Instant::now();
    let mut m = Mount {
        role,
        path: path.to_path_buf(),
        ok: false,
        problem: None,
        writable: None,
        free_bytes: None,
        total_bytes: None,
        latency_ms: None,
        checked_at: crate::server::util::epoch_secs(),
    };
    match std::fs::read_dir(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            m.problem = Some("missing".into());
            return m;
        }
        Err(e) => {
            m.problem = Some(format!("unreadable: {e}"));
            return m;
        }
    }
    if want_write {
        let w = writable(path);
        m.writable = Some(w);
        if !w {
            m.problem = Some("not writable".into());
        }
    }
    if let Some((free, total)) = statvfs(path) {
        m.free_bytes = Some(free);
        m.total_bytes = Some(total);
    }
    m.latency_ms = Some(started.elapsed().as_millis() as u64);
    m.ok = m.problem.is_none();
    m
}

// `check` on a throwaway thread, abandoned after CHECK_TIMEOUT. While a
// folder's earlier check is still stuck, it reads as not responding at once.
fn check_bounded(role: &'static str, path: PathBuf, want_write: bool) -> Mount {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.clone();
    {
        let mut busy = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if busy.contains(&path) {
            drop(busy);
            return not_responding(role, &path);
        }
        busy.push(path.clone());
    }
    let spawned = std::thread::Builder::new()
        .name("health-check".into())
        .spawn(move || {
            let m = check(role, &p, want_write);
            IN_FLIGHT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|x| *x != p);
            let _ = tx.send(m);
        });
    if spawned.is_err() {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|x| *x != path);
        return not_responding(role, &path);
    }
    rx.recv_timeout(CHECK_TIMEOUT)
        .unwrap_or_else(|_| not_responding(role, &path))
}

fn not_responding(role: &'static str, path: &Path) -> Mount {
    Mount {
        role,
        path: path.to_path_buf(),
        ok: false,
        problem: Some(format!(
            "not responding after {}s (a stale network mount?)",
            CHECK_TIMEOUT.as_secs()
        )),
        writable: None,
        free_bytes: None,
        total_bytes: None,
        latency_ms: None,
        checked_at: crate::server::util::epoch_secs(),
    }
}

/// Whether the last check of `path` found it usable. `None` if never checked.
pub fn folder_ok(path: &Path) -> Option<bool> {
    LAST.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|m| m.path == path)
        .map(|m| m.ok)
}

/// Check every folder once and publish the result.
pub fn refresh(cfg: &Config) {
    let results: Vec<Mount> = folders(cfg)
        .into_iter()
        .map(|(role, path, w)| check_bounded(role, path, w))
        .collect();
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = results;
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
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_checked_for_presence_and_writes() {
        let t = tempfile::tempdir().unwrap();
        let m = check("Output", t.path(), true);
        assert!(m.ok && m.writable == Some(true), "{m:?}");
        assert_eq!(
            std::fs::read_dir(t.path()).unwrap().count(),
            0,
            "the check wrote a file"
        );
        let gone = check("Output", &t.path().join("nope"), true);
        assert!(!gone.ok);
        assert_eq!(gone.problem.as_deref(), Some("missing"));
    }

    #[test]
    fn a_stuck_check_is_never_doubled() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("stuck");
        IN_FLIGHT.lock().unwrap().push(p.clone());
        let m = check_bounded("Library", p.clone(), false);
        assert!(!m.ok);
        assert!(m.problem.unwrap().contains("not responding"));
        assert_eq!(
            IN_FLIGHT
                .lock()
                .unwrap()
                .iter()
                .filter(|x| **x == p)
                .count(),
            1
        );
        IN_FLIGHT.lock().unwrap().retain(|x| *x != p);
        let fine = check_bounded("Library", t.path().to_path_buf(), false);
        assert!(fine.ok);
        assert!(!IN_FLIGHT.lock().unwrap().contains(&t.path().to_path_buf()));
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
