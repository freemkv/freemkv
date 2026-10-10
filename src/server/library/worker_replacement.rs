use super::*;
use crate::server::library::replacement::{FileIdentity, Replacement, RunIo};
use std::io;

pub(super) fn run(
    job: &Job,
    plan: &crate::server::planner::RemuxPlan,
    cfg: &Config,
    sink: &JobSink<'_>,
) -> Ending {
    let result = execute(job, plan, cfg, sink);
    match result {
        Ok(writing_app) => Ending::Done { writing_app },
        Err(error) => cancelled_ending(sink).unwrap_or(Ending::Failed(error)),
    }
}

fn execute(
    job: &Job,
    plan: &crate::server::planner::RemuxPlan,
    cfg: &Config,
    sink: &JobSink<'_>,
) -> io::Result<Option<String>> {
    let mut replacement = job
        .replacement
        .clone()
        .ok_or_else(|| io::Error::other("missing replacement journal"))?;
    let targets = job.output_targets();
    if plan.version != crate::server::planner::PLAN_VERSION
        || plan.source_iso != job.iso
        || plan.outputs.is_empty()
        || plan.outputs.len() != targets.len()
        || plan.outputs.len() != job.outputs.len()
        || plan
            .outputs
            .iter()
            .zip(&job.outputs)
            .any(|(p, o)| p.id != o.id)
    {
        return Err(io::Error::other(
            "replacement plan does not match queued outputs",
        ));
    }
    if tv_destinations(cfg, plan).is_some_and(|(_, current)| current != targets) {
        return Err(io::Error::other(
            "TV destinations changed; replacement requires review",
        ));
    }
    let mut runner = Runner {
        job,
        plan,
        cfg,
        sink,
        commit_locks: Vec::new(),
    };
    for target in &targets {
        runner.validate(target)?;
    }
    for old in &replacement.old_outputs {
        runner.validate(&old.path)?;
    }
    let writing_app = replacement.run(&job.iso, &targets, job.id, &mut runner)?;
    for output in &job.outputs {
        sink.lib.note_landed(&output.target, writing_app.clone());
        sink.lib
            .queue
            .finish_output(job.id, &output.id, OutputState::Done);
    }
    for lock in runner.commit_locks.drain(..) {
        if let Err(error) = lock.delete() {
            tracing::warn!(%error, "replacement finished but its lock sidecar remains");
        }
    }
    sink.lib.wake_indexer();
    Ok(writing_app)
}

struct Runner<'a, 'b> {
    job: &'a Job,
    plan: &'a crate::server::planner::RemuxPlan,
    cfg: &'a Config,
    sink: &'a JobSink<'b>,
    commit_locks: Vec<libfreemkv::io::ArtifactLock>,
}

impl Runner<'_, '_> {
    fn validate(&self, path: &Path) -> io::Result<()> {
        let movies = super::super::dirs(self.cfg).library;
        let tv = Path::new(&self.cfg.output_dir).join(&self.cfg.tv_dir);
        if !path.is_absolute() {
            return Err(io::Error::other("replacement path must be absolute"));
        }
        let root = if path.starts_with(&movies) {
            movies
        } else {
            tv
        };
        safe_destination(&root, path, true).map_err(io::Error::other)
    }
}

impl RunIo for Runner<'_, '_> {
    fn reconcile_new_set(&mut self, targets: &[std::path::PathBuf]) -> io::Result<()> {
        super::super::links::reconcile_replacement(
            &self.sink.lib.config_dir,
            targets,
            &self.job.iso,
        )
    }
    fn guard_new_set(&mut self, targets: &[std::path::PathBuf]) -> io::Result<()> {
        let mut ordered = targets.to_vec();
        ordered.sort();
        deliver::with_halt(self.sink, |halt| -> io::Result<()> {
            for target in ordered {
                self.commit_locks
                    .push(libfreemkv::io::ArtifactLock::acquire(&target, &[], halt)?);
            }
            Ok(())
        })
    }
    fn checkpoint(&mut self) -> io::Result<()> {
        if self.sink.should_cancel() || cancelled_ending(self.sink).is_some() {
            return Err(libfreemkv::Error::Halted.into());
        }
        Ok(())
    }

    fn prepare(&mut self, ordinal: usize, candidate: &Path) -> io::Result<Option<String>> {
        self.checkpoint()?;
        self.validate(candidate)?;
        let output = &self.plan.outputs[ordinal];
        self.sink.lib.queue.begin_output(self.job.id, &output.id);
        self.sink.lib.set_running(|r| {
            if let Some(r) = r {
                r.target = self.job.outputs[ordinal].target.clone();
            }
        });
        if let Some(parent) = candidate.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err(io::Error::other(
                        "replacement candidate is not a regular file",
                    ));
                }
                // A crash can occur after the engine lands the reserved candidate
                // but before its identity is journalled. Verify against the source.
                let source = freemkv_engine::ImageSource::from_path(&self.job.iso);
                let (disc, _reader) =
                    freemkv_engine::scan_image(&source).map_err(io::Error::other)?;
                let title = disc
                    .titles
                    .get(output.title_index)
                    .ok_or_else(|| io::Error::other("replacement title is missing"))?;
                return freemkv_engine::verify_mkv(candidate, title).map(|p| p.writing_app);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let request = freemkv_engine::RemuxJob {
            iso: freemkv_engine::ImageSource::from_path(&self.job.iso),
            title: Some(output.title_index),
            streams: Default::default(),
            target: candidate.to_path_buf(),
            replace: false,
        };
        let keys = crate::server::keysource::key_params(self.cfg);
        let report = if let Some(stage) = remux_stage_dir() {
            std::fs::create_dir_all(&stage)?;
            let partial = stage.join(format!("replacement-{}-{ordinal}.mkv.partial", self.job.id));
            freemkv_engine::remux_iso_staged(&request, &keys, self.sink, &partial)
        } else {
            freemkv_engine::remux_iso(&request, &keys, self.sink)
        }?;
        Ok(report.writing_app)
    }

    fn publish(
        &mut self,
        candidate: &Path,
        target: &Path,
        before: Option<&FileIdentity>,
    ) -> io::Result<()> {
        self.validate(target)?;
        deliver::with_halt(self.sink, |halt| -> io::Result<()> {
            let lock = libfreemkv::io::ArtifactLock::acquire(target, &[candidate], halt)?;
            if halt.is_cancelled() {
                return Err(libfreemkv::Error::Halted.into());
            }
            let require_link = before.is_some()
                && self.job.replacement.as_ref().is_some_and(|r| {
                    r.old_outputs.iter().any(|old| {
                        old.path == target
                            && old.ownership
                                == crate::server::library::replacement::Ownership::RecordedLink
                    })
                });
            super::super::links::publish_replacement(
                &self.sink.lib.config_dir,
                target,
                &self.job.iso,
                require_link,
                || {
                    let same = libfreemkv::io::artifact_lock::file_identity(target).ok()
                        == Some(libfreemkv::io::artifact_lock::file_identity(candidate)?);
                    if same {
                        std::fs::remove_file(candidate)?;
                    } else if let Some(before) = before {
                        before.verify(target)?;
                        std::fs::rename(candidate, target)?;
                    } else {
                        publish_new(candidate, target)?;
                    }
                    if let Some(parent) = target.parent() {
                        libfreemkv::io::fsync::dir(parent);
                    }
                    Ok(())
                },
            )?;
            lock.delete()?;
            Ok(())
        })
    }

    fn retire(&mut self, path: &Path, identity: &FileIdentity) -> io::Result<()> {
        self.validate(path)?;
        deliver::with_halt(self.sink, |halt| -> io::Result<()> {
            let lock = libfreemkv::io::ArtifactLock::acquire(path, &[], halt)?;
            if halt.is_cancelled() {
                return Err(libfreemkv::Error::Halted.into());
            }
            super::super::links::retire_replacement(
                &self.sink.lib.config_dir,
                path,
                &self.job.iso,
                false,
                &self.job.output_targets(),
                || {
                    match identity.verify(path) {
                        Ok(()) => std::fs::remove_file(path)?,
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                    if let Some(parent) = path.parent() {
                        libfreemkv::io::fsync::dir(parent);
                    }
                    Ok(())
                },
            )?;
            lock.delete()?;
            Ok(())
        })
    }

    fn retire_output(
        &mut self,
        output: &crate::server::library::replacement::OwnedOutput,
    ) -> io::Result<()> {
        use crate::server::library::replacement::Ownership;
        if output.ownership == Ownership::RecordedLink {
            return self.retire(&output.path, &output.identity);
        }
        self.validate(&output.path)?;
        deliver::with_halt(self.sink, |halt| -> io::Result<()> {
            let lock = libfreemkv::io::ArtifactLock::acquire(&output.path, &[], halt)?;
            if halt.is_cancelled() {
                return Err(libfreemkv::Error::Halted.into());
            }
            super::super::links::retire_replacement(
                &self.sink.lib.config_dir,
                &output.path,
                &self.job.iso,
                true,
                &self.job.output_targets(),
                || {
                    match output.identity.verify(&output.path) {
                        Ok(()) => std::fs::remove_file(&output.path)?,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                    if let Some(parent) = output.path.parent() {
                        libfreemkv::io::fsync::dir(parent);
                    }
                    Ok(())
                },
            )?;
            lock.delete()?;
            Ok(())
        })
    }

    fn persist(&mut self, replacement: &Replacement) -> io::Result<()> {
        self.sink
            .lib
            .queue
            .save_replacement(self.job.id, replacement.clone())
    }
}

/// Publish without overwriting a concurrently created, unrelated destination.
/// macOS network shares may support exclusive rename but not hard links.
fn publish_new(candidate: &Path, target: &Path) -> io::Result<()> {
    libfreemkv::io::publish::no_replace(candidate, target)
}

#[cfg(test)]
mod publication_tests {
    use super::*;

    #[test]
    fn new_publication_never_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("candidate");
        let target = dir.path().join("target");
        std::fs::write(&candidate, b"new").unwrap();
        std::fs::write(&target, b"existing").unwrap();
        assert_eq!(
            publish_new(&candidate, &target).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");
        assert_eq!(std::fs::read(&candidate).unwrap(), b"new");
        std::fs::remove_file(&target).unwrap();
        publish_new(&candidate, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!candidate.exists());
    }
}
