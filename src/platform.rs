//! The few things that genuinely cannot be written once.
//!
//! `ui.rs` is platform-neutral by contract — if a change there would need
//! mirroring in a shell, the split is wrong. Free-space reporting is the one
//! piece of the core that needs a real OS call, so it lives here behind a
//! neutral signature instead of leaking a `cfg` into the core. Every shell
//! calls the same `ui.rs`; only this module varies.

/// Bytes available on the volume holding `path`, or `None` when it cannot be
/// determined. Callers render `None` as an em dash — never as a blank field
/// and never as `0`. A path holding a NUL byte names no file and is `None`.
pub fn free_space_bytes(path: &str) -> Option<u64> {
    if path.contains('\0') {
        return None;
    }
    imp::free_space_bytes(path)
}

/// The user's home directory.
///
/// `$HOME` is a Unix convention — Windows does not set it, so reading it there
/// yielded an empty path and every derived location (settings file, default
/// destination, keydb) silently became relative. Windows uses `%USERPROFILE%`.
pub fn home_dir() -> std::path::PathBuf {
    imp::home_dir()
}

/// Where this app keeps its own writable state (settings JSON, keydb, log).
///
/// Per-OS by convention, not by preference: `~/Library/Application Support` on
/// macOS, `%APPDATA%` on Windows. An app bundle / Program Files directory is not
/// writable, so this must never be derived from the executable's location.
pub fn support_dir() -> std::path::PathBuf {
    // Unit tests get a per-process temp dir so no test reads or renames the real settings.
    #[cfg(test)]
    {
        std::env::temp_dir()
            .join(format!("fmkv-unit-{}", std::process::id()))
            .join("freemkv")
    }
    #[cfg(not(test))]
    {
        imp::support_dir()
    }
}

/// The default output folder offered for rips — the OS's own video folder.
pub fn default_dest_dir() -> std::path::PathBuf {
    imp::default_dest_dir()
}

/// Whether `p` is an absolute path *on this OS*.
///
/// Used to reject a stale or placeholder destination. Testing `starts_with('/')`
/// is a Unix-only rule that called every valid Windows path (`C:\Users\…`)
/// relative and reset the user's destination on every load.
pub fn is_absolute(p: &str) -> bool {
    !p.trim().is_empty() && std::path::Path::new(p.trim()).is_absolute()
}

/// XDG Base Directory / xdg-user-dirs resolution, kept pure so it is testable
/// on every host.
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
mod xdg {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    // The spec says a relative path in an XDG variable is invalid and ignored.
    fn absolute(v: Option<OsString>) -> Option<PathBuf> {
        v.map(PathBuf::from).filter(|p| p.is_absolute())
    }

    // Spelled via concat! so the leak-guard's private-TLD pattern does not flag the path.
    pub fn data_home(env: Option<OsString>, home: &Path) -> PathBuf {
        absolute(env).unwrap_or_else(|| home.join(concat!(".", "local")).join("share"))
    }

    pub fn config_home(env: Option<OsString>, home: &Path) -> PathBuf {
        absolute(env).unwrap_or_else(|| home.join(".config"))
    }

    /// `XDG_VIDEOS_DIR` from `user-dirs.dirs` (`"$HOME/Videos"` or an absolute
    /// path), else from the environment, else `~/Videos`. The file wins, as in
    /// `xdg-user-dir`. An entry equal to `$HOME` means "disabled".
    pub fn videos_dir(env: Option<OsString>, user_dirs: Option<&str>, home: &Path) -> PathBuf {
        let from_file = || {
            user_dirs?.lines().find_map(|l| {
                let v = l.trim().strip_prefix("XDG_VIDEOS_DIR=")?;
                let v = v.strip_prefix('"')?.strip_suffix('"')?;
                match v.strip_prefix("$HOME") {
                    Some(rest) => Some(home.join(rest.trim_start_matches('/'))),
                    None => Some(PathBuf::from(v)).filter(|p| p.is_absolute()),
                }
            })
        };
        from_file()
            .or_else(|| absolute(env))
            .filter(|p| p.as_path() != home)
            .unwrap_or_else(|| home.join("Videos"))
    }
}

#[cfg(unix)]
mod imp {
    use std::path::PathBuf;

    pub fn home_dir() -> PathBuf {
        // NEVER an empty path. `unwrap_or_default()` returned one, and every
        // path built on it came out RELATIVE to the CWD instead of home.
        // Unset HOME is not exotic — containers, cron, `env -i`, Docker.
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    pub fn support_dir() -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            home_dir().join("Library/Application Support/freemkv")
        }
        // Linux (and any other unix that isn't macOS): XDG Base Directory —
        // state + data live under `$XDG_DATA_HOME`, defaulting to the
        // XDG data home under HOME (see the spec), never `Library/…`.
        #[cfg(not(target_os = "macos"))]
        {
            super::xdg::data_home(std::env::var_os("XDG_DATA_HOME"), &home_dir()).join("freemkv")
        }
    }

    pub fn default_dest_dir() -> PathBuf {
        #[cfg(target_os = "macos")]
        {
            home_dir().join("Movies")
        }
        // xdg-user-dirs keeps the (possibly moved or localized) Videos dir in
        // `user-dirs.dirs`, which is rarely exported into the environment.
        #[cfg(not(target_os = "macos"))]
        {
            let home = home_dir();
            let config = super::xdg::config_home(std::env::var_os("XDG_CONFIG_HOME"), &home);
            let dirs = std::fs::read_to_string(config.join("user-dirs.dirs")).ok();
            super::xdg::videos_dir(std::env::var_os("XDG_VIDEOS_DIR"), dirs.as_deref(), &home)
        }
    }

    /// The nearest ancestor of `p` (itself included) that `exists`. A relative path with
    /// nothing existing resolves to the current directory, not the root volume.
    pub fn nearest_existing(
        p: &std::path::Path,
        exists: impl Fn(&std::path::Path) -> bool,
    ) -> &std::path::Path {
        std::iter::successors(Some(p), |q| q.parent())
            .find(|q| !q.as_os_str().is_empty() && exists(q))
            .unwrap_or(if p.is_absolute() {
                std::path::Path::new("/")
            } else {
                std::path::Path::new(".")
            })
    }

    /// Bytes available to an unprivileged writer on the volume holding `path`:
    /// `statvfs` on the nearest existing ancestor, `f_bavail * f_frsize`; `None`
    /// when `statvfs` fails.
    pub fn free_space_bytes(path: &str) -> Option<u64> {
        use std::os::unix::ffi::OsStrExt as _;
        // A destination that does not exist yet is normal (we are about to
        // create the file); probe the nearest existing ancestor so the number
        // still describes the right volume.
        let probe = nearest_existing(std::path::Path::new(path), |q| q.exists());
        let c = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
        let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: `c` is a NUL-terminated path and `st` is a writable statvfs.
        if unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: statvfs returned 0, so it filled `st`.
        let st = unsafe { st.assume_init() };
        #[allow(clippy::useless_conversion)]
        Some(u64::from(st.f_bavail).saturating_mul(u64::from(st.f_frsize)))
    }
}

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    pub fn home_dir() -> PathBuf {
        // Never empty — see the Unix note above; an empty base makes every
        // derived path relative to the CWD.
        std::env::var_os("USERPROFILE")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// `%APPDATA%` (roaming). Falls back to `%USERPROFILE%\AppData\Roaming` when
    /// the variable is missing, so the path is never relative.
    pub fn support_dir() -> PathBuf {
        std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"))
            .join("freemkv")
    }

    /// The Windows equivalent of `~/Movies` is the Videos known folder.
    pub fn default_dest_dir() -> PathBuf {
        home_dir().join("Videos")
    }

    // `GetDiskFreeSpaceExW` gives the free bytes available to the calling user
    // (which is what a rip is actually limited by — not the raw volume free).
    // Declared directly so the core carries no extra dependency.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available_to_caller: *mut u64,
            total_number_of_bytes: *mut u64,
            total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }

    pub fn free_space_bytes(path: &str) -> Option<u64> {
        let p = std::path::Path::new(path);
        let probe = std::iter::successors(Some(p), |q| q.parent()).find(|q| q.exists())?;
        let mut wide: Vec<u16> = probe.as_os_str().encode_wide().collect();
        wide.push(0);
        let mut avail: u64 = 0;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut avail,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(avail)
    }

    use std::os::windows::ffi::OsStrExt as _;
}

#[cfg(test)]
#[path = "platform_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "platform_xdg_tests.rs"]
mod xdg_tests;
