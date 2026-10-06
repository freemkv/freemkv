//! Whether two paths name the SAME file — the one definition both shells use.
//!
//! Every sink opens its destination for writing before (or while) the source
//! is read, so "the destination IS the source" must be answered before a byte
//! moves. Canonical-path comparison alone misses a hardlink; see
//! [`same_file`](crate::file_identity::same_file) for the full check. Same
//! shape as `title_identity`: one
//! question, one answer, declared by both crate roots.

/// Whether two paths name the same existing file.
///
/// Canonicalised, so `./Disc.iso`, `Disc.iso`, `sub/../Disc.iso`, an absolute
/// path to it, and a symlink to it are all one file. A destination that does
/// not exist yet cannot be the source, so a failed canonicalize on either
/// side answers `false` rather than refusing the rip.
///
/// Canonical equality alone misses a hardlink — two canonical names sharing
/// one file — so filesystem identity ([`file_id`]) is compared too.
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
/// the numbers NTFS itself uses to tell two names for one file apart. `std`
/// exposes them only behind an unstable feature, so the call is declared here.
///
/// Opened with no access rights and `FILE_FLAG_BACKUP_SEMANTICS`, so it never
/// fights a sharing mode the sink may hold and can still open a directory
/// (`dir://` sources and destinations go through this guard too). Any
/// failure answers `None`, leaving the canonical-path comparison to stand.
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
#[path = "file_identity_tests.rs"]
mod tests;
