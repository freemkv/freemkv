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

#[cfg(unix)]
mod imp {
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
        let line = text.lines().filter(|l| !l.trim().is_empty()).next_back()?;
        let kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
        Some(kb.saturating_mul(1024))
    }
}

#[cfg(windows)]
mod imp {
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
        assert!(n.is_some_and(|b| b > 0), "no free space reported: {n:?}");
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
