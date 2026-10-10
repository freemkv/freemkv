//! Durable, explicit source identity; automatic lookup must not override it.

use crate::server::planner::{MediaKind, MediaMetadata};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMatch {
    pub revision: u64,
    pub media: MediaMetadata,
}

pub struct MatchStore {
    path: PathBuf,
    entries: Mutex<BTreeMap<PathBuf, SavedMatch>>,
}

impl MatchStore {
    pub fn open(config_dir: &Path) -> io::Result<Self> {
        let path = config_dir.join("library-matches.json");
        let entries: BTreeMap<PathBuf, SavedMatch> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        for (source, saved) in &entries {
            validate(source, &saved.media)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if saved.revision == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid match revision",
                ));
            }
        }
        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    pub fn get(&self, source: &Path) -> Option<SavedMatch> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(source)
            .cloned()
    }

    /// Compare-and-save prevents an old dialog overwriting a newer correction.
    /// Publish only after durable persistence succeeds; never erase corrupt state.
    pub fn save(
        &self,
        source: &Path,
        expected_revision: u64,
        media: MediaMetadata,
    ) -> io::Result<SavedMatch> {
        validate(source, &media)?;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let revision = entries.get(source).map_or(0, |m| m.revision);
        if revision != expected_revision {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "source match changed; refresh before saving",
            ));
        }
        let saved = SavedMatch {
            revision: revision
                .checked_add(1)
                .ok_or_else(|| io::Error::other("match revision exhausted"))?,
            media,
        };
        let mut next = entries.clone();
        next.insert(source.to_path_buf(), saved.clone());
        let bytes = serde_json::to_vec_pretty(&next).map_err(io::Error::other)?;
        crate::server::ripper::staging::write_marker_durable(&self.path, &bytes)?;
        *entries = next;
        Ok(saved)
    }
}

fn validate(source: &Path, media: &MediaMetadata) -> io::Result<()> {
    if !source.is_absolute()
        || source
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || source.file_name().is_none()
        || media.title.trim().is_empty()
        || media.title.len() > 512
        || media.title.chars().any(char::is_control)
        || media.tmdb_id == 0
        || media.kind.is_none()
        || media.season == Some(0)
        || media.disc == Some(0)
        || media.episode_start == Some(0)
        || (media.kind == Some(MediaKind::Movie)
            && (media.season.is_some() || media.disc.is_some() || media.episode_start.is_some()))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid source match",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(kind: MediaKind) -> MediaMetadata {
        MediaMetadata {
            title: "Cast Away".into(),
            year: 2000,
            tmdb_id: 8358,
            kind: Some(kind),
            ..Default::default()
        }
    }

    #[test]
    fn movie_tv_corrections_survive_reopen_and_reject_stale_dialogs() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("original.iso");
        let store = MatchStore::open(dir.path()).unwrap();
        let first = store.save(&iso, 0, media(MediaKind::Tv)).unwrap();
        assert_eq!(first.revision, 1);
        assert!(store.save(&iso, 0, media(MediaKind::Movie)).is_err());
        let second = store.save(&iso, 1, media(MediaKind::Movie)).unwrap();
        drop(store);
        let reopened = MatchStore::open(dir.path()).unwrap();
        assert_eq!(reopened.get(&iso), Some(second));
        let third = reopened.save(&iso, 2, media(MediaKind::Tv)).unwrap();
        assert_eq!(MatchStore::open(dir.path()).unwrap().get(&iso), Some(third));
    }

    #[test]
    fn persistence_failure_does_not_publish_match() {
        let dir = tempfile::tempdir().unwrap();
        let store = MatchStore::open(dir.path()).unwrap();
        std::fs::create_dir(&store.path).unwrap();
        let iso = dir.path().join("source.iso");
        assert!(store.save(&iso, 0, media(MediaKind::Movie)).is_err());
        assert_eq!(store.get(&iso), None);
    }

    #[test]
    fn confirmed_episode_start_is_durable_and_movie_requires_it_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("disc-2.iso");
        let store = MatchStore::open(dir.path()).unwrap();
        let mut tv = media(MediaKind::Tv);
        tv.disc = Some(2);
        tv.episode_start = Some(4);
        let saved = store.save(&iso, 0, tv.clone()).unwrap();
        assert_eq!(MatchStore::open(dir.path()).unwrap().get(&iso), Some(saved));

        tv.episode_start = Some(0);
        assert_eq!(
            store.save(&iso, 1, tv).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let mut movie = media(MediaKind::Movie);
        movie.episode_start = Some(4);
        assert_eq!(
            store.save(&iso, 1, movie.clone()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        movie.episode_start = None;
        let saved = store.save(&iso, 1, movie).unwrap();
        assert_eq!(saved.revision, 2);
        assert_eq!(MatchStore::open(dir.path()).unwrap().get(&iso), Some(saved));
    }

    #[test]
    fn invalid_persisted_identity_is_rejected_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library-matches.json");
        for (source, revision, title) in [
            ("relative.iso", 1, "Movie"),
            ("/iso/source.iso", 0, "Movie"),
            ("/iso/source.iso", 1, ""),
        ] {
            let mut identity = media(MediaKind::Movie);
            identity.title = title.into();
            let contents = serde_json::to_vec(&BTreeMap::from([(
                PathBuf::from(source),
                SavedMatch {
                    revision,
                    media: identity,
                },
            )]))
            .unwrap();
            std::fs::write(&path, &contents).unwrap();
            assert!(
                MatchStore::open(dir.path()).is_err(),
                "accepted invalid persisted identity"
            );
            assert_eq!(std::fs::read(&path).unwrap(), contents);
        }
    }

    #[test]
    fn corrupt_matches_are_not_silently_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library-matches.json");
        std::fs::write(&path, b"broken").unwrap();
        assert!(MatchStore::open(dir.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"broken");
    }

    #[test]
    fn concurrent_dialogs_have_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("source.iso");
        let store = MatchStore::open(dir.path()).unwrap();
        let barrier = std::sync::Barrier::new(8);
        let successes = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (store, iso, barrier) = (&store, &iso, &barrier);
                    scope.spawn(move || {
                        barrier.wait();
                        store.save(iso, 0, media(MediaKind::Movie)).is_ok()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum::<usize>()
        });
        assert_eq!(successes, 1);
        assert_eq!(
            MatchStore::open(dir.path()).unwrap().get(&iso),
            store.get(&iso)
        );
    }

    #[test]
    fn invalid_choices_leave_prior_match_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("source.iso");
        let store = MatchStore::open(dir.path()).unwrap();
        let original = store.save(&iso, 0, media(MediaKind::Movie)).unwrap();
        let mut cases = Vec::new();
        for title in [
            "".to_owned(),
            "  ".to_owned(),
            "bad\nname".to_owned(),
            "x".repeat(513),
        ] {
            let mut value = media(MediaKind::Movie);
            value.title = title;
            cases.push(value);
        }
        let mut value = media(MediaKind::Movie);
        value.kind = None;
        cases.push(value);
        let mut value = media(MediaKind::Movie);
        value.tmdb_id = 0;
        cases.push(value);
        for kind in [MediaKind::Movie, MediaKind::Tv] {
            let mut value = media(kind);
            value.season = Some(0);
            cases.push(value);
        }
        let mut value = media(MediaKind::Movie);
        value.season = Some(1);
        cases.push(value);
        for value in cases {
            assert!(store.save(&iso, 1, value).is_err());
            assert_eq!(store.get(&iso), Some(original.clone()));
        }
        assert_eq!(
            MatchStore::open(dir.path()).unwrap().get(&iso),
            Some(original)
        );
    }

    #[test]
    fn corrections_are_scoped_to_one_source_not_title() {
        let dir = tempfile::tempdir().unwrap();
        let store = MatchStore::open(dir.path()).unwrap();
        let a = dir.path().join("disc1.iso");
        let b = dir.path().join("disc2.iso");
        let original = store.save(&b, 0, media(MediaKind::Tv)).unwrap();
        store.save(&a, 0, media(MediaKind::Movie)).unwrap();
        assert_eq!(store.get(&b), Some(original));
        for path in [
            Path::new("relative.iso"),
            Path::new("/x/../source.iso"),
            Path::new("/"),
        ] {
            assert!(store.save(path, 0, media(MediaKind::Movie)).is_err());
        }
    }
}
