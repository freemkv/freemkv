//! Whether two paths name the SAME file — the one definition both shells use.
//!
//! Every sink here opens its destination for writing before (or while) the
//! source is read, so "the destination IS the source" has to be answered
//! before a byte moves. It was answered twice: the CLI's `pipe` compared
//! canonical paths AND filesystem identity, while the GUI's `engine` compared
//! canonical paths alone — and the narrower copy is the one that misses a
//! hardlink, so the desktop app would truncate the user's only copy on a rip
//! the CLI refuses. Same shape as `title_identity`: one question, one answer,
//! declared by both crate roots.

/// Whether two paths name the same existing file.
///
/// Canonicalised, so `./Disc.iso`, `Disc.iso`, `sub/../Disc.iso`, an absolute
/// path to it and a symlink to it are all one file. A destination that does not
/// exist yet cannot be the source, so a failed canonicalize on either side is
/// "not the same file" — the guard must never refuse an ordinary rip because a
/// path could not be resolved.
///
/// Canonical equality is not sufficient on its own: a HARDLINK gives one file
/// two names that are each already canonical, so the paths differ while the
/// bytes are shared and writing either one destroys the other. Compare the
/// identity the filesystem actually uses as well.
pub fn same_file(source: Option<&std::path::Path>, dest: &std::path::Path) -> bool {
    let Some(source) = source else { return false };
    let (Ok(a), Ok(b)) = (std::fs::canonicalize(source), std::fs::canonicalize(dest)) else {
        return false;
    };
    if a == b {
        return true;
    }
    match (file_id(&a), file_id(&b)) {
        (Some(x), Some(y)) => x == y,
        // An identity this platform (or this file) cannot produce leaves the
        // canonical comparison standing on its own — never a refusal.
        _ => false,
    }
}

/// The identity the filesystem itself uses for a file, if it can be read.
///
/// The hardlink case is the whole point: no path comparison can see it.
#[cfg(unix)]
pub fn file_id(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some((m.dev(), m.ino()))
}

/// Neither Unix nor Windows: nothing to compare, so the canonical-path
/// comparison in [`same_file`] stands alone (it still resolves `.`, `..`,
/// relative spellings and symlinks).
#[cfg(not(any(unix, windows)))]
pub fn file_id(_path: &std::path::Path) -> Option<(u64, u64)> {
    None
}

/// Windows: `(volume serial, file index)` from `GetFileInformationByHandle`,
/// which is what NTFS itself uses to tell two names for one file apart. `std`
/// exposes the same numbers only behind an unstable feature, so the call is
/// declared here rather than pulling in a crate.
///
/// The handle is opened with no access rights and `FILE_FLAG_BACKUP_SEMANTICS`
/// deliberately: attribute-only access does not fight a sharing mode the sink
/// may already hold, and without the backup flag a DIRECTORY cannot be opened
/// at all — and this guard is asked about directories (`dir://` sources and
/// destinations), not only files. Any failure answers `None`, which leaves the
/// canonical-path comparison standing, so the guard can never refuse a rip
/// because a handle could not be opened.
///
/// NOTE: type-checked for `x86_64-pc-windows-msvc`, but not run — no Windows
/// machine builds this crate in the environment it was written in.
#[cfg(windows)]
pub fn file_id(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    /// `FILE_FLAG_BACKUP_SEMANTICS` — required to open a directory handle.
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;

    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: [u32; 2],
        last_access_time: [u32; 2],
        last_write_time: [u32; 2],
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            info: *mut ByHandleFileInformation,
        ) -> i32;
    }

    let f = std::fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    let mut info = ByHandleFileInformation::default();
    // SAFETY: `f` owns a live handle for the whole call, and `info` is a
    // correctly-sized, correctly-aligned `BY_HANDLE_FILE_INFORMATION` the
    // callee only writes into.
    let ok = unsafe { GetFileInformationByHandle(f.as_raw_handle().cast(), &mut info) };
    if ok == 0 {
        return None;
    }
    Some((
        u64::from(info.volume_serial_number),
        (u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low),
    ))
}

#[cfg(test)]
mod tests {
    use super::{file_id, same_file};

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "freemkv_file_identity_{}_{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).expect("fixture dir");
            Tmp(d)
        }
        fn file(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).expect("write fixture");
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The same file under two spellings is one file — the case a path
    /// comparison misses. The detour goes through `..` deliberately: `Path`'s
    /// own `==` normalises a `.` away, so a `./Disc.iso` fixture would pass
    /// even with the canonicalize deleted.
    #[test]
    fn the_same_file_under_two_spellings_is_recognised() {
        let t = Tmp::new("spellings");
        let abs = t.file("Disc.iso", b"not really an iso");
        std::fs::create_dir_all(t.0.join("sub")).expect("subdir");
        let detoured = t.0.join("sub").join("..").join("Disc.iso");
        assert_ne!(
            detoured.as_path(),
            abs.as_path(),
            "the fixture must not be equal as PATHS, or it proves nothing"
        );
        assert!(same_file(Some(&abs), &abs), "a path is itself");
        assert!(
            same_file(Some(&detoured), &abs),
            "sub/../Disc.iso and Disc.iso are one file"
        );
    }

    /// A HARDLINK: both names canonicalize to themselves, so only filesystem
    /// identity can see that writing either one destroys the other.
    #[cfg(unix)]
    #[test]
    fn a_hardlink_is_the_same_file_even_though_the_paths_differ() {
        let t = Tmp::new("hardlink");
        let a = t.file("Movie.mkv", b"one file, two names");
        let hard = t.0.join("Hard.mkv");
        std::fs::hard_link(&a, &hard).expect("hard link");
        assert_ne!(
            std::fs::canonicalize(&a).expect("canon a"),
            std::fs::canonicalize(&hard).expect("canon hard"),
            "a hardlink must NOT be equal by canonical path, or it proves nothing"
        );
        assert!(same_file(Some(&a), &hard));
        assert!(same_file(Some(&hard), &a), "and the other way round");
    }

    /// Different files still rip: the guard must not cost the ordinary case.
    #[test]
    fn two_different_files_are_not_the_same_file() {
        let t = Tmp::new("distinct");
        let a = t.file("In.iso", b"a");
        let b = t.file("Out.iso", b"b");
        assert!(!same_file(Some(&a), &b));
    }

    /// The ordinary case: the destination does not exist yet. It cannot be the
    /// source, and a failed canonicalize must never refuse the rip.
    #[test]
    fn a_destination_that_does_not_exist_yet_is_never_the_source() {
        let t = Tmp::new("missing_dest");
        let a = t.file("In.iso", b"a");
        let dest = t.0.join("does-not-exist.iso");
        assert!(!same_file(Some(&a), &dest));
        // And a source with no filesystem path at all (disc://) is never it.
        assert!(!same_file(None, &dest));
    }

    /// A DIRECTORY has an identity too — `dir://` sources and destinations go
    /// through this guard, so an implementation that only ever opens files
    /// would answer `None` for exactly the tree-destroying case.
    #[test]
    fn a_directory_has_a_readable_identity() {
        let t = Tmp::new("dir_identity");
        assert!(
            file_id(&t.0).is_some(),
            "a directory must yield a filesystem identity"
        );
        assert_eq!(file_id(&t.0), file_id(&t.0), "and a stable one");
    }
}
