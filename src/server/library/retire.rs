//! Removing a corrected ISO's old files once every new file is in place. Stateless:
//! each run re-reads the files and links.json, so a resumed job redoes only what is left.

use super::links;
use super::owned::{Owned, Present, present};
use std::io;
use std::path::{Path, PathBuf};

/// What one retire pass works with.
pub(super) struct Retire<'a> {
    pub iso: &'a Path,
    pub config_dir: &'a Path,
    pub roots: &'a [PathBuf],
    pub sink: &'a dyn freemkv_engine::Sink,
    /// True once the job must stop (Stop, a rip, shutdown or a stall).
    pub stopping: &'a dyn Fn() -> bool,
}

/// Remove every `owned` file that is not one of `targets`, if it is still as captured and
/// still linked to the ISO. Returns the old files kept; an error leaves the rest for a retry.
pub(super) fn run(
    r: &Retire<'_>,
    owned: &[Owned],
    targets: &[PathBuf],
) -> io::Result<Vec<PathBuf>> {
    // Nothing to retire: a missing new file is then not this pass's concern.
    if owned.iter().all(|o| targets.contains(&o.path)) {
        return Ok(Vec::new());
    }
    for target in targets {
        match std::fs::symlink_metadata(target) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                return Err(io::Error::other(crate::strings::fmt_or(
                    "server.library.retire.new_not_regular",
                    "the new file {path} is no longer a regular file; no old file was removed",
                    &[("path", &target.display().to_string())],
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    crate::strings::fmt_or(
                        "server.library.retire.new_missing",
                        "the new file {path} cannot be found (is the share mounted?); no old file was removed",
                        &[("path", &target.display().to_string())],
                    ),
                ));
            }
            Err(e) => return Err(e),
        }
    }
    let mut kept = Vec::new();
    for old in owned.iter().filter(|o| !targets.contains(&o.path)) {
        if (r.stopping)() {
            return Err(libfreemkv::Error::Halted.into());
        }
        let root = r
            .roots
            .iter()
            .find(|root| old.path.starts_with(root))
            .ok_or_else(|| outside(&old.path))?;
        super::worker::safe_destination(root, &old.path).map_err(io::Error::other)?;
        // A missing folder is a share that went away, not a file someone removed.
        if old
            .path
            .parent()
            .is_some_and(|dir| matches!(std::fs::symlink_metadata(dir), Err(e) if e.kind() == io::ErrorKind::NotFound))
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                crate::strings::fmt_or(
                    "server.library.retire.folder_missing",
                    "the folder of {path} cannot be found (is the share mounted?); it was not removed",
                    &[("path", &old.path.display().to_string())],
                ),
            ));
        }
        let lock = super::deliver::with_halt(r.sink, |halt| {
            libfreemkv::io::ArtifactLock::acquire(&old.path, &[], halt)
        })
        .map_err(io::Error::from)?;
        let removed = retire_one(r, old);
        if let Err(e) = lock.delete() {
            tracing::warn!(path = %old.path.display(), error = %e, "retire lock could not be removed");
        }
        if removed? {
            remove_emptied_folders(&old.path, root);
        } else {
            kept.push(old.path.clone());
        }
    }
    Ok(kept)
}

// The title folder (and a show folder above a season folder) a removed file leaves empty;
// never `root`, and never a folder with anything left in it (remove_dir refuses those).
fn remove_emptied_folders(removed: &Path, root: &Path) {
    for dir in removed.ancestors().skip(1).take(2) {
        if dir == root || !dir.starts_with(root) || std::fs::remove_dir(dir).is_err() {
            return;
        }
        if let Some(parent) = dir.parent() {
            libfreemkv::io::fsync::dir(parent);
        }
    }
}

fn outside(path: &Path) -> io::Error {
    io::Error::other(crate::strings::fmt_or(
        "server.library.retire.outside",
        "{path} is not under the library folders; it was not removed",
        &[("path", &path.display().to_string())],
    ))
}

// True when the old file is gone (removed now or before); false when it is kept.
fn retire_one(r: &Retire<'_>, old: &Owned) -> io::Result<bool> {
    let linked = links::read(r.config_dir)?
        .get(&old.path)
        .map(PathBuf::as_path)
        == Some(r.iso);
    let unchanged = match present(&old.path)? {
        Present::File(stamp) => Some(stamp == old.stamp),
        Present::Gone => None,
        // Not a regular file any more: someone else's.
        Present::Other => Some(false),
    };
    match (unchanged, linked) {
        (None, true) => links::forget(r.config_dir, &old.path, r.iso).map(|()| true),
        (None, false) => Ok(true),
        (Some(true), true) => {
            if let Err(e) = std::fs::remove_file(&old.path)
                && e.kind() != io::ErrorKind::NotFound
            {
                return Err(e);
            }
            if let Some(dir) = old.path.parent() {
                libfreemkv::io::fsync::dir(dir);
            }
            links::forget(r.config_dir, &old.path, r.iso).map(|()| true)
        }
        // Changed since the remux started: disowned, so it is never removed later.
        (Some(false), true) => links::forget(r.config_dir, &old.path, r.iso).map(|()| false),
        (Some(_), false) => Ok(false),
    }
}

/// A retire that kept old files: every new file landed, and nothing is left to finish.
#[derive(Debug)]
pub(super) struct KeptOld(pub String);

impl std::fmt::Display for KeptOld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeptOld {}

/// The failure a retire with kept files ends in: every new file landed, these old ones stay.
pub(super) fn kept_message(new: usize, kept: &[PathBuf]) -> String {
    let paths: Vec<String> = kept.iter().map(|p| p.display().to_string()).collect();
    crate::strings::fmt_or(
        "server.library.retire.kept",
        "All {new} new files are in place. {count} old file(s) made from this ISO were kept because they changed after this correction started, or are no longer recorded as made from it: {paths}. They are no longer tracked; delete them yourself if they are not wanted.",
        &[
            ("new", &new.to_string()),
            ("count", &kept.len().to_string()),
            ("paths", &paths.join(", ")),
        ],
    )
}

#[cfg(test)]
#[path = "retire_tests.rs"]
mod tests;
