//! Durable ownership evidence for a whole capture delivery, not just this tick's files.
//!
//! A file is ours while it keeps the size and pinned mtime this journal recorded
//! (`util::Stamp`). No file is known by device/inode: SMB renumbers devices on remount
//! and derives inodes from names. Adopting a destination found after a crash also
//! needs its first and last 64 KiB to match the staged source, while that exists.
//! The publishing call records `published` after checking the destination's stamp.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::server::util::Stamp;

pub(super) const FILE: &str = crate::server::ripper::staging::DELIVERY_JOURNAL;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    source: PathBuf,
    destination: PathBuf,
    destination_parent_id: (u64, u64),
    source_identity: Stamp,
    prepared: Option<Stamp>,
    #[serde(default)]
    published: Option<Stamp>,
    completed: bool,
    // The destination as planned from settings; links and logs use this form.
    #[serde(default)]
    planned: Option<String>,
}

impl Entry {
    // The publication this journal proved last: rebound, else as prepared.
    fn owned(&self) -> io::Result<&Stamp> {
        self.published
            .as_ref()
            .or(self.prepared.as_ref())
            .ok_or_else(|| io::Error::other("delivery identity missing"))
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Cleanup {
    path: PathBuf,
    identity: Stamp,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Manifest {
    version: u32,
    directory: PathBuf,
    directory_id: (u64, u64),
    pub marker: serde_json::Value,
    pub outputs: Vec<crate::server::ripper::staging::Output>,
    pub roots: Vec<String>,
    entries: Vec<Entry>,
    cleanup: Vec<Cleanup>,
    #[serde(default)]
    retry: Option<RetryState>,
    // The move webhook fired for this delivery (it fires once, after linkage).
    #[serde(default)]
    announced: bool,
}

/// A delivery's retry budget or operator hold, mirrored from the mover's error row so a
/// restart cannot buy another full copy. `not_before: None` means held for Retry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RetryState {
    pub attempts: u32,
    pub not_before: Option<std::time::SystemTime>,
    pub reason: String,
    pub hint: String,
}

impl Manifest {
    pub fn load(dir: &Path) -> io::Result<Option<Self>> {
        let path = dir.join(FILE);
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
            Ok(m) if !m.is_file() => {
                return Err(io::Error::other("delivery manifest is not a regular file"));
            }
            Ok(_) => {}
        }
        let manifest: Self =
            serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)?;
        let directory = dir.canonicalize()?;
        if manifest.version != 1
            || manifest.directory != directory
            || manifest.directory_id != libfreemkv::io::artifact_lock::file_identity(&directory)?
            || manifest.entries.is_empty()
            || manifest.entries.iter().any(|e| {
                e.source.parent() != Some(directory.as_path())
                    || !e.destination.is_absolute()
                    || !manifest
                        .roots
                        .iter()
                        .any(|root| e.destination.starts_with(root))
            })
            || manifest
                .cleanup
                .iter()
                .any(|e| e.path.parent() != Some(directory.as_path()))
        {
            return Err(io::Error::other(
                "invalid delivery manifest; refusing recovery",
            ));
        }
        Ok(Some(manifest))
    }

    pub fn create(
        dir: &Path,
        marker: serde_json::Value,
        outputs: Vec<crate::server::ripper::staging::Output>,
        roots: Vec<String>,
        moves: &[(PathBuf, String)],
    ) -> io::Result<Self> {
        let directory = dir.canonicalize()?;
        let directory_id = libfreemkv::io::artifact_lock::file_identity(&directory)?;
        let roots = roots
            .into_iter()
            .map(|root| {
                Path::new(&root)
                    .canonicalize()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut entries = Vec::new();
        for (source, destination) in moves {
            let destination_planned = destination;
            let source_identity = Stamp::read(source)?;
            let source = source.canonicalize()?;
            let target = Path::new(destination);
            let parent = target
                .parent()
                .ok_or_else(|| io::Error::other("delivery target has no parent"))?
                .canonicalize()?;
            let destination = parent.join(
                target
                    .file_name()
                    .ok_or_else(|| io::Error::other("delivery target has no filename"))?,
            );
            let destination_parent_id = libfreemkv::io::artifact_lock::file_identity(&parent)?;
            match std::fs::symlink_metadata(&destination) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "unowned delivery destination exists",
                    ));
                }
            }
            if source.parent() != Some(directory.as_path())
                || entries.iter().any(|e: &Entry| e.destination == destination)
                || !roots.iter().any(|root| destination.starts_with(root))
            {
                return Err(io::Error::other(
                    "delivery plan has an invalid source or duplicate destination",
                ));
            }
            entries.push(Entry {
                source,
                destination,
                destination_parent_id,
                source_identity,
                prepared: None,
                published: None,
                completed: false,
                planned: Some(destination_planned.clone()),
            });
        }
        let mut cleanup = Vec::new();
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|name| name == FILE) {
                return Err(io::Error::other("delivery manifest already exists"));
            }
            cleanup.push(Cleanup {
                identity: Stamp::read(&path)?,
                path,
            });
        }
        let manifest = Self {
            version: 1,
            directory,
            directory_id,
            marker,
            outputs,
            roots,
            entries,
            cleanup,
            retry: None,
            announced: false,
        };
        manifest.save()?;
        Ok(manifest)
    }

    fn save(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        // An error may follow a successful rename. Never roll back or assume
        // the old journal survived: every next operation reloads from disk.
        crate::server::ripper::staging::write_marker_durable(&self.directory.join(FILE), &bytes)
    }

    pub fn announced(&self) -> bool {
        self.announced
    }

    pub fn mark_announced(dir: &Path) -> io::Result<()> {
        let mut manifest =
            Self::load(dir)?.ok_or_else(|| io::Error::other("delivery manifest disappeared"))?;
        manifest.announced = true;
        manifest.save()
    }

    pub fn retry(&self) -> Option<&RetryState> {
        self.retry.as_ref()
    }

    /// Persist (or clear) the retry state of the plan in `dir`; no journal, nothing to do.
    pub fn record_retry(dir: &Path, state: Option<RetryState>) -> io::Result<()> {
        let Some(mut manifest) = Self::load(dir)? else {
            return Ok(());
        };
        if manifest.retry != state {
            manifest.retry = state;
            manifest.save()?;
        }
        Ok(())
    }

    pub fn moves(&self) -> Vec<(PathBuf, String)> {
        self.entries
            .iter()
            .map(|e| {
                (
                    e.source.clone(),
                    e.planned
                        .clone()
                        .unwrap_or_else(|| e.destination.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    fn prepare(dir: &Path, index: usize, candidate: &Path) -> io::Result<()> {
        let mut manifest =
            Self::load(dir)?.ok_or_else(|| io::Error::other("delivery manifest disappeared"))?;
        let entry = manifest
            .entries
            .get_mut(index)
            .ok_or_else(|| io::Error::other("invalid delivery entry"))?;
        verify_parent(&entry.destination, entry.destination_parent_id)?;
        entry.source_identity.verify(&entry.source)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(candidate)?;
        libfreemkv::io::durable_sync_file(&file, None, |_, _| {})?;
        let identity = Stamp::read(candidate)?;
        if identity.size != entry.source_identity.size {
            return Err(io::Error::other("delivery candidate size changed"));
        }
        entry.prepared = Some(identity);
        manifest.save()
    }

    pub fn deliver(
        dir: &Path,
        index: usize,
        progress: &dyn Fn(u8, f64, f64, f64),
    ) -> io::Result<super::MoveOutcome> {
        let mut manifest =
            Self::load(dir)?.ok_or_else(|| io::Error::other("delivery manifest disappeared"))?;
        let entry = manifest
            .entries
            .get(index)
            .ok_or_else(|| io::Error::other("invalid delivery entry"))?
            .clone();
        verify_parent(&entry.destination, entry.destination_parent_id)?;
        super::sweep_temporaries(&entry.destination)?;
        // Ours only as prepared: its recorded size and pinned time, and on adoption its
        // head and tail; never a pathname, a valid-looking media file, or size alone.
        match std::fs::symlink_metadata(&entry.destination) {
            Ok(found) => {
                if entry.published.is_none() && found.is_file() && found.len() == 0 {
                    return Err(io::Error::other(format!(
                        "zero-length file at destination, likely an interrupted publication; \
                         inspect and remove it, then Retry: {}",
                        entry.destination.display()
                    )));
                }
                // Nothing of ours was ever prepared for it: a foreign file, not a hold.
                if entry.prepared.is_none() {
                    return Ok(super::MoveOutcome::Collision);
                }
                entry.owned()?.verify(&entry.destination)?;
                if !entry.completed {
                    adoptable(&entry)?;
                }
                sync_parent(&entry.destination)?;
                manifest.entries[index].completed = true;
                manifest.save()?;
                super::clear_stale_dest_error(&entry.destination);
                return Ok(super::MoveOutcome::Skipped);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && !entry.completed => {}
            Err(e) => return Err(e),
        }
        entry.source_identity.verify(&entry.source)?;
        let owned_dir = dir.to_path_buf();
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let outcome = super::move_file(
            &entry.source,
            &entry.destination,
            progress,
            Arc::new(move |candidate| Self::prepare(&owned_dir, index, candidate)),
            dir,
            published.clone(),
        );
        let finished = matches!(
            outcome,
            super::MoveOutcome::Moved | super::MoveOutcome::Skipped
        );
        let published = published.load(std::sync::atomic::Ordering::SeqCst);
        if published || finished {
            Self::settle(dir, index, published, finished)?;
        }
        Ok(outcome)
    }

    // `published`: our no_replace returned Ok in this call, under the target
    // lock, even if a later step failed. Only then may a changed id rebind.
    fn settle(dir: &Path, index: usize, published: bool, finished: bool) -> io::Result<()> {
        let mut manifest =
            Self::load(dir)?.ok_or_else(|| io::Error::other("delivery manifest disappeared"))?;
        let entry = manifest
            .entries
            .get_mut(index)
            .ok_or_else(|| io::Error::other("invalid delivery entry"))?;
        let prepared = entry
            .prepared
            .as_ref()
            .ok_or_else(|| io::Error::other("publication did not journal its identity"))?;
        if published {
            let current = Stamp::read(&entry.destination)?;
            if current != *prepared {
                return Err(io::Error::other(format!(
                    "published file differs from its prepared size or time: {}",
                    entry.destination.display()
                )));
            }
            entry.published = Some(current);
        } else {
            prepared.verify(&entry.destination)?;
        }
        entry.completed |= finished;
        manifest.save()
    }

    pub fn verify_delivered(&self) -> io::Result<()> {
        for entry in &self.entries {
            verify_parent(&entry.destination, entry.destination_parent_id)?;
            if !entry.completed {
                return Err(io::Error::other("delivery plan is incomplete"));
            }
            entry.owned()?.verify(&entry.destination)?;
            sync_parent(&entry.destination)?;
        }
        Ok(())
    }

    /// Cooperating library writers use these same sidecars. Hold the complete
    /// set in deterministic order through verification, linkage and cleanup.
    pub fn lock_targets(&self) -> io::Result<Vec<libfreemkv::io::ArtifactLock>> {
        self.lock_targets_within(LOCK_WAIT)
    }

    // Give up after `wait` with ResourceBusy so one busy target cannot starve every other
    // finished rip on the single mover thread; the next tick tries again.
    pub fn lock_targets_within(
        &self,
        wait: std::time::Duration,
    ) -> io::Result<Vec<libfreemkv::io::ArtifactLock>> {
        let started = std::time::Instant::now();
        let mut targets = Vec::new();
        for entry in &self.entries {
            verify_parent(&entry.destination, entry.destination_parent_id)?;
            targets.push(entry.destination.clone());
        }
        targets.sort();
        let halt = libfreemkv::Halt::new();
        let waiting_halt = halt.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = crate::server::daemon::spawn_background("mover-delivery-locks", move || {
            let result = targets
                .into_iter()
                .map(|target| {
                    libfreemkv::io::ArtifactLock::acquire(&target, &[], &waiting_halt)
                        .map_err(io::Error::other)
                })
                .collect::<io::Result<Vec<_>>>();
            let _ = tx.send(result);
        })?;
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(result) => {
                    let _ = waiter.join();
                    return result;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("delivery lock worker stopped"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if started.elapsed() >= wait {
                        halt.cancel();
                        return Err(io::Error::new(
                            io::ErrorKind::ResourceBusy,
                            "another library operation holds a destination lock",
                        ));
                    }
                    if crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
                        halt.cancel();
                        // The tracked worker releases any partial lock set on
                        // cancellation; lifecycle drain accounts for its exit.
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "delivery lock wait cancelled",
                        ));
                    }
                }
            }
        }
    }

    pub fn cleanup(&self) -> io::Result<()> {
        self.cleanup_with(&|| {})
    }

    // `removed` runs after the planned files are gone (tests add a late arrival there).
    fn cleanup_with(&self, removed: &dyn Fn()) -> io::Result<()> {
        if libfreemkv::io::artifact_lock::file_identity(&self.directory)? != self.directory_id {
            return Err(io::Error::other("staging directory identity changed"));
        }
        self.verify_delivered()?;
        // Refuse newly introduced or changed files. Retain the journal until
        // last so a crash halfway through cleanup still retries linkage/cleanup.
        for entry in std::fs::read_dir(&self.directory)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|name| name == FILE) {
                continue;
            }
            let saved = self
                .cleanup
                .iter()
                .find(|e| e.path == path)
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "unplanned staging artifact retained: {}",
                        path.display()
                    ))
                })?;
            saved.identity.verify(&path)?;
        }
        for entry in &self.cleanup {
            if let Some(delivered) = self.entries.iter().find(|e| e.source == entry.path) {
                verify_parent(&delivered.destination, delivered.destination_parent_id)?;
                delivered.owned()?.verify(&delivered.destination)?;
            }
            match entry.identity.verify(&entry.path) {
                Ok(()) => std::fs::remove_file(&entry.path)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        removed();
        libfreemkv::io::fsync::dir_checked(&self.directory)?;
        // The journal goes last and only from an otherwise empty folder: a late
        // arrival (an NFS silly-rename) would leave a journal-less, stranded folder.
        for entry in std::fs::read_dir(&self.directory)? {
            let path = entry?.path();
            if path.file_name().is_none_or(|name| name != FILE) {
                return Err(io::Error::other(format!(
                    "unplanned staging artifact retained: {}",
                    path.display()
                )));
            }
        }
        std::fs::remove_file(self.directory.join(FILE))?;
        std::fs::remove_dir(&self.directory)
    }
}

fn verify_parent(path: &Path, expected_id: (u64, u64)) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("delivery target has no parent"))?;
    if parent.canonicalize()? != parent
        || libfreemkv::io::artifact_lock::file_identity(parent)? != expected_id
    {
        return Err(io::Error::other("delivery target parent changed"));
    }
    Ok(())
}

// A destination with the prepared stamp found after a crash. A same-size file
// stamped before it was filled is not ours: its head and tail must match the
// source. A source already gone was renamed into place by our own publication.
fn adoptable(entry: &Entry) -> io::Result<()> {
    match std::fs::symlink_metadata(&entry.source) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
        Ok(_) => {}
    }
    if !same_head_and_tail(&entry.source, &entry.destination)? {
        return Err(io::Error::other(format!(
            "destination content differs from the staged source: {}",
            entry.destination.display()
        )));
    }
    Ok(())
}

// Compare the first and last 64 KiB of two files of equal length.
fn same_head_and_tail(a: &Path, b: &Path) -> io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: u64 = 64 * 1024;
    let windows = |path: &Path| -> io::Result<(Vec<u8>, Vec<u8>)> {
        let mut file = std::fs::File::open(path)?;
        let n = WINDOW.min(file.metadata()?.len());
        let mut head = vec![0; n as usize];
        file.read_exact(&mut head)?;
        let mut tail = vec![0; n as usize];
        file.seek(SeekFrom::End(-(n as i64)))?;
        file.read_exact(&mut tail)?;
        Ok((head, tail))
    };
    Ok(windows(a)? == windows(b)?)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    libfreemkv::io::fsync::dir_checked(
        path.parent()
            .ok_or_else(|| io::Error::other("delivery target has no parent"))?,
    )
}

#[cfg(test)]
#[path = "mover_manifest_tests.rs"]
mod tests;
