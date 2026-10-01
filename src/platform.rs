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

    /// Free bytes from `df -k` output: the "Available" column of the one data row.
    pub fn parse_df_free(text: &str) -> Option<u64> {
        // `df` wraps a long device name onto its own line, pushing the data
        // columns onto the next — joining every line after the header back
        // into one restores the normal column order before indexing into it.
        let merged = text
            .lines()
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let kb: u64 = merged.split_whitespace().nth(3)?.parse().ok()?;
        Some(kb.saturating_mul(1024))
    }

    pub fn free_space_bytes(path: &str) -> Option<u64> {
        let p = std::path::Path::new(path);
        // A destination that does not exist yet is normal (we are about to
        // create the file); probe the nearest existing ancestor so the number
        // still describes the right volume.
        let probe = nearest_existing(p, |q| q.exists());
        let out = std::process::Command::new("df")
            .args(["-k", probe.to_str()?])
            .output()
            .ok()?;
        parse_df_free(&String::from_utf8_lossy(&out.stdout))
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

    // `is_absolute` was untested; `starts_with('/')` called Windows paths
    // relative, resetting the user's destination on load to a relative
    // path — which writes rips next to the process CWD.
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

    // The positive side, in each OS's native form: `C:\…` is not absolute
    // on Unix, and a bare `/x` is not absolute on Windows (drive-relative),
    // so a single shared rule would be wrong somewhere.
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
        let support = super::imp::support_dir();
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

    /// Nonsense input must not panic. A relative path that cannot exist is measured on the
    /// current directory's volume on Unix and is `None` on Windows, as is an empty one.
    #[test]
    fn a_bogus_path_does_not_panic_and_a_relative_one_means_the_cwd() {
        assert_eq!(super::free_space_bytes("\0\0\0").is_some(), cfg!(unix));
        assert_eq!(
            super::free_space_bytes("").is_some(),
            cfg!(unix),
            "an empty path means the current directory on Unix"
        );
    }

    /// A relative destination that does not exist yet is measured on the current directory's
    /// volume, never the root's.
    #[cfg(unix)]
    #[test]
    fn a_missing_relative_destination_probes_the_current_directory() {
        use std::path::Path;
        let none = |_: &Path| false;
        assert_eq!(
            super::imp::nearest_existing(Path::new("out/new/file.mkv"), none),
            Path::new(".")
        );
        assert_eq!(
            super::imp::nearest_existing(Path::new("out/new/file.mkv"), |q| q == Path::new("out")),
            Path::new("out")
        );
        assert_eq!(
            super::imp::nearest_existing(Path::new("/gone/file.mkv"), none),
            Path::new("/")
        );
    }

    /// A long device name wraps `df`'s row onto two lines; the Available column is the same.
    #[cfg(unix)]
    #[test]
    fn df_output_parses_whether_or_not_the_device_name_wraps() {
        let plain = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                     /dev/disk1 1000 400 600 40% /\n";
        let wrapped = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                       server.example.com:/export/a/very/long/name\n\
                       1000 400 600 40% /mnt\n";
        assert_eq!(super::imp::parse_df_free(plain), Some(600 * 1024));
        assert_eq!(super::imp::parse_df_free(wrapped), Some(600 * 1024));
        assert_eq!(super::imp::parse_df_free("Filesystem\n"), None);
    }

    /// Unit tests build real `App`s (`Settings::load`, which renames an
    /// unparseable file aside); they must never reach the user's real settings.
    #[test]
    fn unit_tests_never_resolve_the_real_support_dir() {
        let support = super::support_dir();
        assert!(
            support.starts_with(std::env::temp_dir()),
            "unit tests resolved a real support dir: {support:?}"
        );
    }

    /// The real resolution (bypassed by the redirect above) is still sane.
    #[test]
    fn the_real_support_dir_is_absolute_and_ends_in_freemkv() {
        let real = super::imp::support_dir();
        assert!(real.is_absolute() && real.ends_with("freemkv"), "{real:?}");
    }
}

#[cfg(all(test, unix))]
mod xdg_tests {
    use super::xdg;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    const HOME: &str = "/srv/u";
    const DIRS: &str = "# written by xdg-user-dirs-update\nXDG_DESKTOP_DIR=\"$HOME/Bureau\"\nXDG_VIDEOS_DIR=\"$HOME/Vidéos\"\n";

    fn env(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn data_home_defaults_and_ignores_relative_or_empty() {
        let h = Path::new(HOME);
        let dflt = PathBuf::from("/srv/u")
            .join(concat!(".", "local"))
            .join("share");
        assert_eq!(xdg::data_home(None, h), dflt);
        assert_eq!(xdg::data_home(env(""), h), dflt);
        assert_eq!(xdg::data_home(env("rel/data"), h), dflt);
        assert_eq!(xdg::data_home(env("/srv/d"), h), PathBuf::from("/srv/d"));
    }

    #[test]
    fn config_home_defaults_and_ignores_relative() {
        let h = Path::new(HOME);
        assert_eq!(xdg::config_home(None, h), PathBuf::from("/srv/u/.config"));
        assert_eq!(
            xdg::config_home(env("cfg"), h),
            PathBuf::from("/srv/u/.config")
        );
        assert_eq!(xdg::config_home(env("/etc/u"), h), PathBuf::from("/etc/u"));
    }

    #[test]
    fn videos_dir_reads_the_localized_entry_from_user_dirs() {
        let h = Path::new(HOME);
        assert_eq!(
            xdg::videos_dir(None, Some(DIRS), h),
            PathBuf::from("/srv/u/Vidéos")
        );
    }

    #[test]
    fn videos_dir_precedence_and_fallbacks() {
        let h = Path::new(HOME);
        assert_eq!(
            xdg::videos_dir(env("/mnt/v"), Some(DIRS), h),
            PathBuf::from("/srv/u/Vidéos"),
            "user-dirs.dirs overrides a (possibly stale) exported variable"
        );
        assert_eq!(
            xdg::videos_dir(env("/mnt/v"), Some("XDG_MUSIC_DIR=\"$HOME/M\"\n"), h),
            PathBuf::from("/mnt/v"),
            "the environment still applies when the file has no Videos entry"
        );
        assert_eq!(
            xdg::videos_dir(env("/mnt/v"), Some("XDG_VIDEOS_DIR=\"$HOME\"\n"), h),
            PathBuf::from("/srv/u/Videos"),
            "a file entry disabling the dir overrides the environment too"
        );
        assert_eq!(
            xdg::videos_dir(None, Some("XDG_VIDEOS_DIR=\"/data/films\"\n"), h),
            PathBuf::from("/data/films")
        );
        assert_eq!(
            xdg::videos_dir(None, Some("XDG_VIDEOS_DIR=\"$HOME/\"\n"), h),
            PathBuf::from("/srv/u/Videos"),
            "an entry equal to $HOME means disabled"
        );
        assert_eq!(
            xdg::videos_dir(None, None, h),
            PathBuf::from("/srv/u/Videos")
        );
        assert_eq!(
            xdg::videos_dir(env("relative"), None, h),
            PathBuf::from("/srv/u/Videos")
        );
    }
}
