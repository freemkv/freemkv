//! Durable ownership evidence for a whole capture delivery, not just this tick's files.
//!
//! Automatic recovery requires stable filesystem identities (device/inode on
//! Unix, volume/file index on Windows). Some SMB remounts change those IDs:
//! that is an explicit operator hold, not permission to adopt equal bytes.
//! Sources, destinations and their parent directories are never rebound by
//! size, hashes, names, or changed output settings.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub(super) const FILE: &str = ".delivery.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    file_id: (u64, u64),
    size: u64,
    modified: std::time::SystemTime,
}

impl Identity {
    fn read(path: &Path) -> io::Result<Self> {
        if !std::fs::symlink_metadata(path)?.is_file() {
            return Err(io::Error::other("delivery requires a regular file"));
        }
        let file_id = libfreemkv::io::artifact_lock::file_identity(path)?;
        let metadata = std::fs::File::open(path)?.metadata()?;
        if libfreemkv::io::artifact_lock::file_identity(path)? != file_id {
            return Err(io::Error::other(
                "delivery file changed while reading identity",
            ));
        }
        Ok(Self {
            file_id,
            size: metadata.len(),
            modified: metadata.modified()?,
        })
    }

    fn verify(&self, path: &Path) -> io::Result<()> {
        if &Self::read(path)? != self {
            return Err(io::Error::other(format!(
                "delivery file identity changed: {}",
                path.display()
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    source: PathBuf,
    destination: PathBuf,
    destination_parent_id: (u64, u64),
    source_identity: Identity,
    prepared: Option<Identity>,
    completed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Cleanup {
    path: PathBuf,
    identity: Identity,
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
            let source_identity = Identity::read(source)?;
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
                completed: false,
            });
        }
        let mut cleanup = Vec::new();
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|name| name == FILE) {
                return Err(io::Error::other("delivery manifest already exists"));
            }
            cleanup.push(Cleanup {
                identity: Identity::read(&path)?,
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

    pub fn moves(&self) -> Vec<(PathBuf, String)> {
        self.entries
            .iter()
            .map(|e| {
                (
                    e.source.clone(),
                    e.destination.to_string_lossy().into_owned(),
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
        let identity = Identity::read(candidate)?;
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
        // Never infer ownership from pathname, size, sampled content, or valid media.
        match std::fs::symlink_metadata(&entry.destination) {
            Ok(_) => {
                entry
                    .prepared
                    .as_ref()
                    .ok_or_else(|| io::Error::other("unowned destination blocks delivery"))?
                    .verify(&entry.destination)?;
                sync_parent(&entry.destination)?;
                manifest.entries[index].completed = true;
                manifest.save()?;
                return Ok(super::MoveOutcome::Skipped);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && !entry.completed => {}
            Err(e) => return Err(e),
        }
        entry.source_identity.verify(&entry.source)?;
        let owned_dir = dir.to_path_buf();
        let outcome = super::move_file_journaled(
            &entry.source,
            &entry.destination,
            progress,
            Arc::new(move |candidate| Self::prepare(&owned_dir, index, candidate)),
            dir,
        );
        if matches!(
            outcome,
            super::MoveOutcome::Moved
                | super::MoveOutcome::MovedDirty
                | super::MoveOutcome::Skipped
        ) {
            manifest = Self::load(dir)?
                .ok_or_else(|| io::Error::other("delivery manifest disappeared"))?;
            manifest.entries[index]
                .prepared
                .as_ref()
                .ok_or_else(|| io::Error::other("publication did not journal its identity"))?
                .verify(&entry.destination)?;
            manifest.entries[index].completed = true;
            manifest.save()?;
        }
        Ok(outcome)
    }

    pub fn verify_delivered(&self) -> io::Result<()> {
        for entry in &self.entries {
            verify_parent(&entry.destination, entry.destination_parent_id)?;
            if !entry.completed {
                return Err(io::Error::other("delivery plan is incomplete"));
            }
            entry
                .prepared
                .as_ref()
                .ok_or_else(|| io::Error::other("delivery identity missing"))?
                .verify(&entry.destination)?;
            sync_parent(&entry.destination)?;
        }
        Ok(())
    }

    /// Cooperating library writers use these same sidecars. Hold the complete
    /// set in deterministic order through verification, linkage and cleanup.
    pub fn lock_targets(&self) -> io::Result<Vec<libfreemkv::io::ArtifactLock>> {
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
                delivered
                    .prepared
                    .as_ref()
                    .ok_or_else(|| io::Error::other("delivery identity missing"))?
                    .verify(&delivered.destination)?;
            }
            match entry.identity.verify(&entry.path) {
                Ok(()) => std::fs::remove_file(&entry.path)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        libfreemkv::io::fsync::dir_checked(&self.directory)?;
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

fn sync_parent(path: &Path) -> io::Result<()> {
    libfreemkv::io::fsync::dir_checked(
        path.parent()
            .ok_or_else(|| io::Error::other("delivery target has no parent"))?,
    )
}

#[cfg(test)]
#[path = "mover_manifest_tests.rs"]
mod tests;
