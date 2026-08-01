//! The few things that genuinely cannot be written once.
//!
//! `ui.rs` is platform-neutral by contract — if a change there would need
//! mirroring in a shell, the split is wrong. Free-space reporting is the one
//! piece of the core that needs a real OS call, so it lives here behind a
//! neutral signature instead of leaking a `cfg` (or a Unix-only `df`) into the
//! core. Every shell calls the same `ui.rs`; only this module varies.

/// Bytes available on the volume holding `path`, or `None` when it cannot be
/// determined. Callers render `None` as an em dash — never as a blank field
/// and never as `0`.
pub fn free_space_bytes(path: &str) -> Option<u64> {
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
    imp::support_dir()
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

#[cfg(unix)]
mod imp {
    use std::path::PathBuf;

    pub fn home_dir() -> PathBuf {
        // NEVER an empty path. `unwrap_or_default()` returned one, and every
        // path built on it then came out RELATIVE: `shellexpand("~/x")` gave
        // "x", `support_dir()` gave "Library/Application Support/freemkv",
        // `default_dest_dir()` gave "Movies". Settings, the keydb and rips all
        // landed relative to the process's CWD instead of the user's home.
        // Unset HOME is not exotic — a container, a systemd unit, a cron job
        // or `env -i` all produce it, and autorip ships in Docker.
        std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }

    pub fn support_dir() -> PathBuf {
        home_dir().join("Library/Application Support/freemkv")
    }

    pub fn default_dest_dir() -> PathBuf {
        home_dir().join("Movies")
    }

    pub fn free_space_bytes(path: &str) -> Option<u64> {
        let p = std::path::Path::new(path);
        // A destination that does not exist yet is normal (we are about to
        // create the file); probe the nearest existing ancestor so the number
        // still describes the right volume.
        let probe = std::iter::successors(Some(p), |q| q.parent())
            .find(|q| q.exists())
            .unwrap_or(std::path::Path::new("/"));
        let out = std::process::Command::new("df")
            .args(["-k", probe.to_str()?])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        // `df` wraps long device names onto a second line, so the available
        // column is not reliably on line 2 — take the last non-empty line.
        let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
        let kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
        Some(kb.saturating_mul(1024))
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
mod tests {
    /// The volume holding the temp dir always exists and always reports a
    /// figure — a blank/zero here is the "Information panel is empty" defect.
    #[test]
    fn free_space_is_reported_for_a_real_directory() {
        let n = super::free_space_bytes(
            std::env::temp_dir()
                .to_str()
                .expect("temp dir is valid UTF-8"),
        );
        // `> 0` was satisfied by a hard-coded `Some(1)`, which is exactly what
        // a broken `df` parse would look like. Any real volume with room for a
        // rip has far more than a mebibyte free.
        assert!(
            n.is_some_and(|b| b > 1_048_576),
            "no plausible free space reported: {n:?}"
        );
    }

    /// `is_absolute` had no test at all, direct or indirect — its only caller
    /// is `Settings::normalize`, which was itself untested. It exists because
    /// `starts_with('/')` called every valid Windows path relative and reset
    /// the user's destination folder on every load; forced to a constant it
    /// either resets a good folder or accepts a relative one, and a relative
    /// destination is how a rip ends up written next to the process CWD.
    #[test]
    fn is_absolute_rejects_empty_blank_and_relative_paths() {
        assert!(!super::is_absolute(""));
        assert!(!super::is_absolute("   "));
        assert!(!super::is_absolute("\t\n"));
        assert!(!super::is_absolute("Movies"));
        assert!(!super::is_absolute("../x"));
        assert!(!super::is_absolute("./Movies"));
        assert!(!super::is_absolute("Library/Application Support/freemkv"));
    }

    /// The positive side, in each OS's own native form. Both are asserted on
    /// both platforms where the answer is unambiguous: a `C:\…` path is not
    /// absolute on Unix, and a bare `/x` is not absolute on Windows (it is
    /// drive-relative), so a single shared rule would be wrong somewhere.
    #[test]
    fn is_absolute_accepts_the_platform_native_absolute_form() {
        #[cfg(unix)]
        {
            assert!(super::is_absolute("/opt/u/Movies"));
            assert!(super::is_absolute("  /opt/u/Movies  "));
            assert!(!super::is_absolute(r"C:\Users\u\Movies"));
        }
        #[cfg(windows)]
        {
            assert!(super::is_absolute(r"C:\Users\u\Movies"));
            assert!(super::is_absolute(r"  C:\Users\x\Movies  "));
            assert!(super::is_absolute(r"\\server\share\x"));
            assert!(!super::is_absolute(r"\Movies"));
        }
    }

    /// Every derived path stays ABSOLUTE with no home variable set.
    ///
    /// This is the regression test for the real bug the mutation runner found:
    /// `unwrap_or_default()` produced an EMPTY base, so `support_dir()` became
    /// `"Library/Application Support/freemkv"` and the settings file, the
    /// downloaded keydb and the rip output all landed relative to the process
    /// CWD. Unset `HOME` is not exotic — a container, a systemd unit or `env
    /// -i` all produce it, and autorip ships in Docker.
    ///
    /// `tests/settings.rs` covers the Unix side by removing `HOME`; on Windows
    /// the `imp` module reads `USERPROFILE` and `APPDATA` instead, so that test
    /// compiles there but observes whatever the environment happens to hold.
    /// This is the Windows counterpart. It cannot be run from a Mac, but it did
    /// not exist at all, so there was nothing accidentally passing.
    #[cfg(windows)]
    #[test]
    fn derived_paths_stay_absolute_without_userprofile_or_appdata() {
        // Serialized against the other env-mutating tests in this binary.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let profile = std::env::var_os("USERPROFILE");
        let appdata = std::env::var_os("APPDATA");
        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("APPDATA");
        }

        let home = super::home_dir();
        let support = super::support_dir();
        let dest = super::default_dest_dir();

        unsafe {
            if let Some(v) = profile {
                std::env::set_var("USERPROFILE", v);
            }
            if let Some(v) = appdata {
                std::env::set_var("APPDATA", v);
            }
        }

        assert!(home.is_absolute(), "home_dir went relative: {home:?}");
        assert!(
            support.is_absolute(),
            "support_dir went relative: {support:?}"
        );
        assert!(
            dest.is_absolute(),
            "default_dest_dir went relative: {dest:?}"
        );
        // An empty base is the specific shape of the bug: it makes the derived
        // path a bare suffix rather than a rooted location.
        assert!(support.ends_with("freemkv"));
        assert_ne!(
            support,
            std::path::Path::new("AppData")
                .join("Roaming")
                .join("freemkv")
        );
    }

    /// A destination that does not exist yet still resolves, via its nearest
    /// existing ancestor — the normal case when naming an output file.
    #[test]
    fn a_not_yet_created_destination_resolves_via_its_parent() {
        let mut p = std::env::temp_dir();
        p.push("freemkv-does-not-exist-yet/out.mkv");
        assert!(super::free_space_bytes(p.to_str().unwrap()).is_some_and(|b| b > 0));
    }

    /// Nonsense input must not panic.
    #[test]
    fn a_bogus_path_is_none_not_a_panic() {
        let _ = super::free_space_bytes("");
        let _ = super::free_space_bytes("\0\0\0");
    }
}
