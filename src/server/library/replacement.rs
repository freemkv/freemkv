//! Frozen source and output ownership for identity-changing remuxes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    canonical: PathBuf,
    size: u64,
    modified: SystemTime,
    #[serde(default)]
    filesystem_id: Option<(u64, u64)>,
}

impl FileIdentity {
    pub(super) fn read(path: &Path) -> io::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() {
            return Err(io::Error::other(
                "replacement requires a regular file, not a symlink",
            ));
        }
        let filesystem_id = Some(libfreemkv::io::artifact_lock::file_identity(path)?);
        Ok(Self {
            canonical: path.canonicalize()?,
            size: metadata.len(),
            modified: metadata.modified()?,
            filesystem_id,
        })
    }

    /// Identity read through a fresh open and fstat, not cached path attributes.
    pub(super) fn read_fresh(path: &Path) -> io::Result<Self> {
        if !std::fs::symlink_metadata(path)?.is_file() {
            return Err(io::Error::other(
                "replacement requires a regular file, not a symlink",
            ));
        }
        let metadata = std::fs::File::open(path)?.metadata()?;
        Ok(Self {
            canonical: path.canonicalize()?,
            size: metadata.len(),
            modified: metadata.modified()?,
            filesystem_id: Some(libfreemkv::io::artifact_lock::file_identity(path)?),
        })
    }

    pub(super) fn same_file(&self, other: &Self) -> bool {
        self.canonical == other.canonical
            || self
                .filesystem_id
                .is_some_and(|id| other.filesystem_id == Some(id))
    }

    pub(crate) fn verify(&self, path: &Path) -> io::Result<()> {
        if Self::read(path)? != *self {
            return Err(io::Error::other(format!(
                "file changed since replacement was queued: {}",
                path.display()
            )));
        }
        Ok(())
    }

    // Inside the publishing call only: ids may follow names (smbfs), so the
    // target must match the candidate's size and explicitly pinned time.
    fn verify_published(&self, published: &Self) -> io::Result<()> {
        if published.size != self.size || published.modified != self.modified {
            return Err(io::Error::other("replacement destination changed"));
        }
        Ok(())
    }

    fn verify_moved(&self, path: &Path) -> io::Result<()> {
        let current = Self::read(path)?;
        if self.filesystem_id.is_none()
            || self.filesystem_id != current.filesystem_id
            || self.size != current.size
            || self.modified != current.modified
        {
            return Err(io::Error::other("replacement destination changed"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedOutput {
    pub path: PathBuf,
    pub(super) identity: FileIdentity,
    #[serde(default)]
    pub ownership: Ownership,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ownership {
    #[default]
    RecordedLink,
    UserConfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replacement {
    source: FileIdentity,
    pub old_outputs: Vec<OwnedOutput>,
    #[serde(default)]
    prepared: Vec<Prepared>,
    #[serde(default)]
    retired: Vec<PathBuf>,
    #[serde(default)]
    complete: bool,
    #[serde(default)]
    roots: Vec<OutputRoot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct OutputRoot {
    path: PathBuf,
    canonical: PathBuf,
    identity: (u64, u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Prepared {
    target: PathBuf,
    candidate: PathBuf,
    identity: Option<FileIdentity>,
    #[serde(default)]
    published: Option<FileIdentity>,
    writing_app: Option<String>,
}

impl Prepared {
    // The identity last proven at the target: rebound on publication, else prepared.
    fn owned(&self) -> io::Result<&FileIdentity> {
        self.published
            .as_ref()
            .or(self.identity.as_ref())
            .ok_or_else(|| io::Error::other("unverified replacement"))
    }
}

// Pin the engine-written candidate's time before its identity is journaled,
// so a deferred SMB close cannot restamp it at publication. Best effort.
fn pin_mtime(path: &Path) {
    let pinned = std::fs::File::options()
        .write(true)
        .open(path)
        .and_then(|file| {
            let modified = file.metadata()?.modified()?;
            file.set_modified(crate::server::util::pinned_mtime(modified))
        });
    if let Err(error) = pinned {
        tracing::warn!(%error, path = %path.display(), "replacement: cannot pin candidate time");
    }
}

/// Filesystem actions supplied by the worker, including cancellation and locks.
pub(crate) trait RunIo {
    fn checkpoint(&mut self) -> io::Result<()>;
    fn guard_new_set(&mut self, targets: &[PathBuf]) -> io::Result<()>;
    fn reconcile_new_set(&mut self, targets: &[PathBuf]) -> io::Result<()>;
    fn prepare(&mut self, ordinal: usize, candidate: &Path) -> io::Result<Option<String>>;
    /// Publish under the target lock; return the target identity read there.
    fn publish(
        &mut self,
        candidate: &Path,
        target: &Path,
        before: Option<&FileIdentity>,
    ) -> io::Result<FileIdentity>;
    fn retire(&mut self, path: &Path, identity: &FileIdentity) -> io::Result<()>;
    fn retire_output(&mut self, output: &OwnedOutput) -> io::Result<()> {
        self.retire(&output.path, &output.identity)
    }
    fn persist(&mut self, replacement: &Replacement) -> io::Result<()>;
}

impl Replacement {
    pub fn capture(source: &Path, links: &HashMap<PathBuf, PathBuf>) -> io::Result<Self> {
        let source_identity = FileIdentity::read(source)?;
        let mut old_outputs: Vec<OwnedOutput> = Vec::new();
        for (path, owner) in links {
            if owner != source {
                continue;
            }
            if !path.is_absolute()
                || !path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("mkv"))
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(io::Error::other("invalid linked replacement output"));
            }
            let identity = match FileIdentity::read(path) {
                Ok(value) => value,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if identity.same_file(&source_identity)
                || old_outputs.iter().any(|o| o.identity.same_file(&identity))
            {
                return Err(io::Error::other("ambiguous replacement file alias"));
            }
            old_outputs.push(OwnedOutput {
                path: path.clone(),
                identity,
                ownership: Ownership::RecordedLink,
            });
        }
        old_outputs.sort_by(|a, b| a.path.cmp(&b.path));
        source_identity.verify(source)?;
        Ok(Self {
            source: source_identity,
            old_outputs,
            prepared: Vec::new(),
            retired: Vec::new(),
            complete: false,
            roots: Vec::new(),
        })
    }

    pub fn verify_source(&self, source: &Path) -> io::Result<()> {
        for root in &self.roots {
            if std::fs::symlink_metadata(&root.path)?
                .file_type()
                .is_symlink()
                || root.path.canonicalize()? != root.canonical
                || libfreemkv::io::artifact_lock::file_identity(&root.path)? != root.identity
            {
                return Err(io::Error::other("replacement output root changed"));
            }
        }
        self.source.verify(source)
    }

    pub(super) fn bind_roots(&mut self, roots: &[PathBuf]) -> io::Result<()> {
        self.roots = roots
            .iter()
            .map(|path| {
                if !path.is_absolute() || !std::fs::symlink_metadata(path)?.is_dir() {
                    return Err(io::Error::other(
                        "replacement root must be an absolute directory",
                    ));
                }
                Ok(OutputRoot {
                    path: path.clone(),
                    canonical: path.canonicalize()?,
                    identity: libfreemkv::io::artifact_lock::file_identity(path)?,
                })
            })
            .collect::<io::Result<_>>()?;
        Ok(())
    }

    pub(super) fn confirm(&mut self, output: OwnedOutput) -> io::Result<()> {
        if output.ownership != Ownership::UserConfirmed
            || output.identity.same_file(&self.source)
            || self
                .old_outputs
                .iter()
                .any(|old| old.identity.same_file(&output.identity))
        {
            return Err(io::Error::other("ambiguous confirmed output alias"));
        }
        output.identity.verify(&output.path)?;
        self.old_outputs.push(output);
        Ok(())
    }

    pub(super) fn verify_admission(&self, source: &Path) -> io::Result<()> {
        self.verify_source(source)?;
        for output in &self.old_outputs {
            output.identity.verify(&output.path)?;
        }
        Ok(())
    }

    pub(super) fn authorizes_existing(&self, path: &Path) -> bool {
        self.old_outputs
            .iter()
            .any(|output| output.path == path && output.identity.verify(path).is_ok())
    }

    pub(super) fn candidate(path: &Path) -> io::Result<OwnedOutput> {
        Ok(OwnedOutput {
            path: path.to_path_buf(),
            identity: FileIdentity::read(path)?,
            ownership: Ownership::UserConfirmed,
        })
    }

    pub(super) fn output_size(output: &OwnedOutput) -> u64 {
        output.identity.size
    }

    pub(crate) fn candidates(&self) -> impl Iterator<Item = &Path> {
        self.prepared.iter().map(|p| p.candidate.as_path())
    }

    pub(crate) fn has_work(&self) -> bool {
        !self.prepared.is_empty() && !self.complete
    }

    pub(crate) fn run(
        &mut self,
        source: &Path,
        targets: &[PathBuf],
        job_id: u64,
        io: &mut dyn RunIo,
    ) -> io::Result<Option<String>> {
        self.verify_source(source)?;
        if targets.is_empty()
            || targets
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != targets.len()
        {
            return Err(io::Error::other("invalid replacement destinations"));
        }
        if self.prepared.is_empty() {
            let mut prepared = Vec::new();
            for target in targets {
                let mut name = target.as_os_str().to_owned();
                name.push(format!(".freemkv-replacement-{job_id}"));
                let candidate = PathBuf::from(name);
                match std::fs::symlink_metadata(&candidate) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                    Ok(_) => return Err(io::Error::other("replacement candidate already exists")),
                }
                prepared.push(Prepared {
                    target: target.clone(),
                    candidate,
                    identity: None,
                    published: None,
                    writing_app: None,
                });
            }
            self.prepared = prepared;
            io.persist(self)?;
        }
        if self.prepared.iter().map(|p| &p.target).ne(targets.iter()) {
            return Err(io::Error::other("replacement destinations changed"));
        }
        // No original is replaced until every candidate has passed verification.
        for ordinal in 0..self.prepared.len() {
            io.checkpoint()?;
            let output = &self.prepared[ordinal];
            if let Some(identity) = &output.identity {
                match identity.verify(&output.candidate) {
                    Ok(()) => continue,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        output.owned()?.verify_moved(&output.target)?;
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            let writing_app = io.prepare(ordinal, &output.candidate)?;
            pin_mtime(&output.candidate);
            let identity = FileIdentity::read(&output.candidate)?;
            self.prepared[ordinal].identity = Some(identity);
            self.prepared[ordinal].writing_app = writing_app;
            io.persist(self)?;
        }
        self.verify_source(source)?;
        for ordinal in 0..self.prepared.len() {
            io.checkpoint()?;
            self.verify_source(source)?;
            let output = &self.prepared[ordinal];
            let identity = output
                .identity
                .as_ref()
                .ok_or_else(|| io::Error::other("unverified replacement"))?;
            match std::fs::symlink_metadata(&output.candidate) {
                Ok(_) => {
                    identity.verify(&output.candidate)?;
                    let before = self
                        .old_outputs
                        .iter()
                        .find(|old| old.path == output.target)
                        .map(|old| &old.identity);
                    let published = io.publish(&output.candidate, &output.target, before)?;
                    identity.verify_published(&published)?;
                    self.prepared[ordinal].published = Some(published);
                    io.persist(self)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let output = &self.prepared[ordinal];
            output.owned()?.verify_moved(&output.target)?;
        }
        io.guard_new_set(targets)?;
        // A missing candidate after restart is accepted only when the final path
        // still names its recorded or rebound identity (never a guessed copy).
        for output in &self.prepared {
            output.owned()?.verify_moved(&output.target)?;
        }
        io.reconcile_new_set(targets)?;
        for old in self.old_outputs.clone() {
            io.checkpoint()?;
            self.verify_source(source)?;
            for output in &self.prepared {
                output.owned()?.verify_moved(&output.target)?;
            }
            if targets.contains(&old.path) || self.retired.contains(&old.path) {
                continue;
            }
            match old.identity.verify(&old.path) {
                Ok(()) => io.retire_output(&old)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => io.retire_output(&old)?,
                Err(e) => return Err(e),
            }
            self.retired.push(old.path);
            io.persist(self)?;
        }
        self.complete = true;
        io.persist(self)?;
        Ok(self.prepared.last().and_then(|p| p.writing_app.clone()))
    }
}

#[cfg(test)]
#[path = "replacement_transaction_tests.rs"]
mod transaction_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_exact_not_title_or_directory_based() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.iso");
        let episode = dir.path().join("episode.mkv");
        let unrelated = dir.path().join("same-title.mkv");
        for path in [&source, &episode, &unrelated] {
            std::fs::write(path, b"data").unwrap();
        }
        let links = HashMap::from([
            (episode.clone(), source.clone()),
            (unrelated, dir.path().join("other.iso")),
        ]);
        let capture = Replacement::capture(&source, &links).unwrap();
        assert_eq!(capture.old_outputs.len(), 1);
        assert_eq!(capture.old_outputs[0].path, episode);
        let reopened: Replacement =
            serde_json::from_slice(&serde_json::to_vec(&capture).unwrap()).unwrap();
        assert_eq!(reopened, capture);
        reopened.verify_source(&source).unwrap();
        std::fs::write(&source, b"replacement source").unwrap();
        assert!(reopened.verify_source(&source).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn source_aliases_and_symlinked_outputs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.iso");
        let alias = dir.path().join("alias.mkv");
        std::fs::write(&source, b"keep iso").unwrap();
        std::fs::hard_link(&source, &alias).unwrap();
        assert!(Replacement::capture(&source, &HashMap::from([(alias, source.clone())])).is_err());
        let symlink = dir.path().join("link.mkv");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        assert!(
            Replacement::capture(&source, &HashMap::from([(symlink, source.clone())])).is_err()
        );
        assert_eq!(std::fs::read(source).unwrap(), b"keep iso");
    }
}
