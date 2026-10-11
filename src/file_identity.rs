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
/// one file — so filesystem identity (the library's `file_identity`) is compared too.
pub fn same_file(source: Option<&std::path::Path>, dest: &std::path::Path) -> bool {
    let Some(source) = source else { return false };
    let (Ok(a), Ok(b)) = (std::fs::canonicalize(source), std::fs::canonicalize(dest)) else {
        return false;
    };
    if a == b {
        return true;
    }
    use libfreemkv::io::artifact_lock::file_identity;
    // An identity that cannot be read leaves the canonical comparison standing
    // on its own — never a refusal.
    matches!((file_identity(&a), file_identity(&b)), (Ok(x), Ok(y)) if x == y)
}

#[cfg(test)]
#[path = "file_identity_tests.rs"]
mod tests;
