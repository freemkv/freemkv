//! Removing a corrected ISO's old files once every new file is in place. Stateless:
//! each run re-reads the files and links.json, so a resumed job redoes only what is left.

use super::links;
use super::owned::Owned;
use crate::server::util::Stamp;
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
                return Err(io::Error::other(format!(
                    "the new file {} is no longer a regular file; no old file was removed",
                    target.display()
                )));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    format!(
                        "the new file {} cannot be found (is the share mounted?); no old file was removed",
                        target.display()
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
        super::worker::safe_destination(root, &old.path, true).map_err(io::Error::other)?;
        // A missing folder is a share that went away, not a file someone removed.
        if old
            .path
            .parent()
            .is_some_and(|dir| matches!(std::fs::symlink_metadata(dir), Err(e) if e.kind() == io::ErrorKind::NotFound))
        {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!(
                    "the folder of {} cannot be found (is the share mounted?); it was not removed",
                    old.path.display()
                ),
            ));
        }
        // A folder on another device than its root is a dropped share with a local stand-in.
        let dir = old.path.parent().ok_or_else(|| outside(&old.path))?;
        if !same_device(root, dir)? {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                format!(
                    "the folder of {} is not on the same storage as its library folder (is the share mounted?); it was not removed",
                    old.path.display()
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
        if !removed? {
            kept.push(old.path.clone());
        }
    }
    Ok(kept)
}

fn same_device(root: &Path, dir: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(root)?.dev() == std::fs::metadata(dir)?.dev())
}

fn outside(path: &Path) -> io::Error {
    io::Error::other(format!(
        "{} is not under the library folders; it was not removed",
        path.display()
    ))
}

// True when the old file is gone (removed now or before); false when it is kept.
fn retire_one(r: &Retire<'_>, old: &Owned) -> io::Result<bool> {
    let linked = links::read(r.config_dir)?
        .get(&old.path)
        .map(PathBuf::as_path)
        == Some(r.iso);
    let unchanged = match Stamp::read(&old.path) {
        Ok(stamp) => Some(stamp == old.stamp),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        // Not a regular file any more: someone else's.
        Err(e) if e.kind() == io::ErrorKind::Other => Some(false),
        Err(e) => return Err(e),
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

/// The failure a retire with kept files ends in: every new file landed, these old ones stay.
pub(super) fn kept_message(new: usize, kept: &[PathBuf]) -> String {
    let paths: Vec<String> = kept.iter().map(|p| p.display().to_string()).collect();
    format!(
        "All {new} new files are in place. {} old file(s) made from this ISO were kept because \
         they changed after this correction started, or are no longer recorded as made from it: \
         {}. They are no longer tracked; delete them yourself if they are not wanted.",
        kept.len(),
        paths.join(", ")
    )
}

#[cfg(test)]
#[path = "retire_tests.rs"]
mod tests;
