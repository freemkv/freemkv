//! The library files a source ISO made, as links.json records them: the only
//! existing files a remux of that ISO may overwrite, and that a Change match
//! may remove once every new file is in place.

use super::Dirs;
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

/// The `.mkv` regular files under `roots` that `links` records as made from `iso`,
/// by path. Anything else is never ours; a file that cannot be read is an error.
pub(crate) fn capture(
    links: &HashMap<PathBuf, PathBuf>,
    iso: &Path,
    roots: &[PathBuf],
) -> io::Result<Vec<Owned>> {
    capture_with(links, iso, roots, |_| true)
}

/// [`capture`], but a file that cannot be read is skipped with a warning unless
/// `required` says the caller needs it.
pub(crate) fn capture_with(
    links: &HashMap<PathBuf, PathBuf>,
    iso: &Path,
    roots: &[PathBuf],
    required: impl Fn(&Path) -> bool,
) -> io::Result<Vec<Owned>> {
    let mut owned = Vec::new();
    for path in links
        .iter()
        .filter(|(_, owner)| owner.as_path() == iso)
        .map(|(path, _)| path)
    {
        if !eligible(path, roots) {
            tracing::warn!(path = %path.display(), "linked output is outside the library folders; ignored");
            continue;
        }
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                tracing::warn!(path = %path.display(), "linked output is not a regular file; ignored");
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) if !required(path) => {
                tracing::warn!(path = %path.display(), error = %e, "linked output cannot be read; skipped");
                continue;
            }
            Err(e) => return Err(e),
        }
        match Stamp::read(path) {
            Ok(stamp) => owned.push(Owned {
                path: path.clone(),
                stamp,
            }),
            Err(e) if !required(path) => {
                tracing::warn!(path = %path.display(), error = %e, "linked output cannot be read; skipped");
            }
            Err(e) => return Err(e),
        }
    }
    owned.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(owned)
}

/// The paths `links` records as made from `iso` that pass the folder rules, sorted.
/// Touches no disk, so unlike [`capture`] it cannot hang on a dead share.
pub(crate) fn eligible_paths(
    links: &HashMap<PathBuf, PathBuf>,
    iso: &Path,
    roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = links
        .iter()
        .filter(|(path, owner)| owner.as_path() == iso && eligible(path, roots))
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
