//! The library files a source ISO made, as links.json records them: the only
//! existing files a remux of that ISO may overwrite, and that a Change match
//! may remove once every new file is in place. A link proves a file is ours only while
//! the file is as freemkv wrote it, from the disc now at that ISO path.

use super::Dirs;
use super::links::Link;
use crate::server::util::Stamp;
use std::collections::HashMap;
use std::io;
use std::path::{Component, Path, PathBuf};

/// A file made from the job's ISO, with its stamp from when the remux started.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Owned {
    pub path: PathBuf,
    pub stamp: Stamp,
}

/// The folders a remux files into: the library and, when it is separate, the TV folder.
pub(crate) fn roots(d: &Dirs) -> Vec<PathBuf> {
    std::iter::once(d.library.clone())
        .chain(d.tv.clone())
        .collect()
}

/// What sits at a path a job may replace or remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Present {
    Gone,
    File(Stamp),
    /// A symlink, a folder or another kind of file: never ours.
    Other,
}

/// What is at `path` now, without following a symlink.
pub(crate) fn present(path: &Path) -> io::Result<Present> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_file() => return Ok(Present::Other),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Present::Gone),
        Err(e) => return Err(e),
    }
    match Stamp::read(path) {
        Ok(stamp) => Ok(Present::File(stamp)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Present::Gone),
        Err(e) if not_regular(&e) => Ok(Present::Other),
        Err(e) => Err(e),
    }
}

// Swapped since the look above: a symlink fails the no-follow open with ELOOP, anything
// else that is not a regular file is Stamp::read's own error, which carries no errno.
fn not_regular(e: &io::Error) -> bool {
    #[cfg(unix)]
    if e.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    e.kind() == io::ErrorKind::Other && e.raw_os_error().is_none()
}

/// The `.mkv` regular files under `roots` that `links` records as made from `iso`, by
/// path, still as freemkv wrote them. Anything else is never ours; a file that cannot be
/// read is an error.
pub(crate) fn capture(
    links: &HashMap<PathBuf, Link>,
    iso: &Path,
    roots: &[PathBuf],
) -> io::Result<Vec<Owned>> {
    capture_with(links, iso, roots, |_| true)
}

/// [`capture`], but a file that cannot be read is skipped with a warning unless
/// `required` says the caller needs it.
pub(crate) fn capture_with(
    links: &HashMap<PathBuf, Link>,
    iso: &Path,
    roots: &[PathBuf],
    required: impl Fn(&Path) -> bool,
) -> io::Result<Vec<Owned>> {
    let iso_now = super::links::iso_stamp(iso);
    let mut owned = Vec::new();
    for (path, link) in links.iter().filter(|(_, l)| l.iso == iso) {
        if !eligible(path, roots) {
            tracing::warn!(path = %path.display(), "linked output is outside the library folders; ignored");
            continue;
        }
        let Some(recorded) = proof(link, iso_now) else {
            continue;
        };
        match present(path) {
            Ok(Present::File(stamp)) if stamp == recorded => owned.push(Owned {
                path: path.clone(),
                stamp,
            }),
            Ok(Present::File(_) | Present::Other) => {
                tracing::info!(path = %path.display(), "linked output changed since freemkv wrote it; not ours");
            }
            Ok(Present::Gone) => {}
            Err(e) if !required(path) => {
                tracing::warn!(path = %path.display(), error = %e, "linked output cannot be read; skipped");
            }
            Err(e) => return Err(e),
        }
    }
    owned.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(owned)
}

// The stamp a link vouches for: none for a 1.8.3 link, or one from an earlier disc filed
// at the same ISO path.
fn proof(link: &Link, iso_now: Option<Stamp>) -> Option<Stamp> {
    link.iso_stamp
        .is_none_or(|then| Some(then) == iso_now)
        .then_some(link.stamp)
        .flatten()
}

/// Whether `links` says the file at `path` is not one `iso` made as it is now: linked to
/// another ISO, or recorded with a stamp (which [`capture`] did not find there).
pub(crate) fn disowned(links: &HashMap<PathBuf, Link>, iso: &Path, path: &Path) -> bool {
    links.get(path).is_some_and(|l| {
        l.iso != iso
            || l.stamp.is_some()
            || l.iso_stamp
                .is_some_and(|then| Some(then) != super::links::iso_stamp(iso))
    })
}

/// The paths `links` records as made from `iso` that pass the folder rules and carry a
/// stamp, sorted. Touches no disk, so unlike [`capture`] it cannot hang on a dead share.
pub(crate) fn eligible_paths(
    links: &HashMap<PathBuf, Link>,
    iso: &Path,
    roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = links
        .iter()
        .filter(|(path, l)| l.iso == iso && l.stamp.is_some() && eligible(path, roots))
        .map(|(path, _)| path.clone())
        .collect();
    paths.sort();
    paths
}

fn eligible(path: &Path, roots: &[PathBuf]) -> bool {
    path.is_absolute()
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
        && !path.components().any(|c| matches!(c, Component::ParentDir))
        && roots.iter().any(|root| path.starts_with(root))
}

#[cfg(test)]
#[path = "owned_tests.rs"]
mod tests;
