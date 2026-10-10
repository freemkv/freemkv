//! One identity-bound preview and one explicit save-and-queue action.

use super::*;
use std::io;

fn roots(cfg: &Config) -> Vec<PathBuf> {
    let d = dirs(cfg);
    let mut roots = vec![d.library];
    if let Some(tv) = d.tv
        && tv.exists()
    {
        roots.push(tv);
    }
    roots
}

#[cfg(test)]
#[path = "ownership_api_tests.rs"]
mod tests;

impl Library {
    fn require_indexed_source(&self, d: &Dirs, source: &Path) -> io::Result<()> {
        let snapshot = self.snapshot();
        if snapshot.dirs.as_ref() != Some(d)
            || snapshot.incomplete
            || !snapshot.isos.iter().any(|iso| iso.path == source)
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "source ISO is not in the complete current library scan",
            ));
        }
        Ok(())
    }

    pub(super) fn preview_ownership(
        &self,
        cfg: &Config,
        source: &Path,
    ) -> io::Result<(Option<matches::SavedMatch>, ownership::Preview)> {
        let d = dirs(cfg);
        let snapshot = self.snapshot();
        self.require_indexed_source(&d, source)?;
        self.queue.with_idle_source(source, || Ok(()))?;
        let saved = self.source_match(source).map_err(io::Error::other)?;
        let revision = saved.as_ref().map_or(0, |s| s.revision);
        // Only the complete index supplies candidates. Never recursively walk a NAS
        // from a request, and never hold queue/match/preview locks for this IO.
        let mut prepared = ownership::Previews::default();
        let preview = prepared.create(
            source,
            revision,
            roots(cfg),
            snapshot.mkvs.iter().map(|file| file.path.clone()).collect(),
            &links::read(&self.config_dir)?,
        )?;
        let _gate = self.match_gate.lock().unwrap_or_else(|e| e.into_inner());
        if !Arc::ptr_eq(&snapshot, &self.snapshot())
            || self
                .source_match(source)
                .map_err(io::Error::other)?
                .as_ref()
                .map_or(0, |s| s.revision)
                != revision
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "library changed during ownership preview; retry",
            ));
        }
        self.queue.with_idle_source(source, || {
            self.ownership_previews
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .install(prepared)?;
            Ok((saved, preview))
        })
    }

    pub(super) fn confirm_match_and_queue(
        &self,
        cfg: &Config,
        source: &Path,
        expected_revision: u64,
        media: crate::server::planner::MediaMetadata,
        confirmation: ownership::Confirmation,
    ) -> io::Result<(matches::SavedMatch, usize)> {
        let _gate = self.match_gate.lock().unwrap_or_else(|e| e.into_inner());
        let d = dirs(cfg);
        self.require_indexed_source(&d, source)?;
        if let Some(reason) = self.queue_block(&d, true) {
            return Err(io::Error::other(reason));
        }
        let store = self
            .source_matches
            .as_ref()
            .map_err(|e| io::Error::other(e.clone()))?;
        let frozen = self
            .ownership_previews
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .consume(
                source,
                expected_revision,
                &roots(cfg),
                confirmation,
                &links::read(&self.config_dir)?,
            )?;
        let saved = self.queue.with_idle_source(source, || {
            if store.get(source).map_or(0, |s| s.revision) != expected_revision {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "source match changed; reopen Change match",
                ));
            }
            store.save(source, expected_revision, media)
        })?;
        let job = NewJob {
            title: saved.media.title.clone(),
            iso: source.to_path_buf(),
            target: index::new_mkv_target(&d.library, &saved.media.title),
            replace: true,
        };
        let queued = self
            .queue
            .add_corrected_authorized(job, saved.clone(), frozen)?;
        self.touch_index();
        Ok((saved, queued))
    }
}
