//! Durable, explicit source identity; automatic lookup must not override it.

use crate::server::planner::{MediaKind, MediaMetadata};
use crate::server::util::Stamp;
use crate::title_identity::TitleIdentity;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedMatch {
    pub revision: u64,
    pub media: MediaMetadata,
    /// The ISO the match was confirmed for; another disc later filed at the path differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso_stamp: Option<Stamp>,
    /// The operator's titles, in order (design §4.4): a Movie's feature or a TV episode set.
    /// Remapped by identity onto the ISO's scan when planned; a mismatch holds the remux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_pick: Option<Vec<TitleIdentity>>,
}

impl SavedMatch {
    /// Whether the match was confirmed for the disc now at its path (`now`, its stamp).
    pub fn is_for(&self, now: Option<Stamp>) -> bool {
        self.iso_stamp.is_none_or(|then| Some(then) == now)
    }
}

pub struct MatchStore {
    path: PathBuf,
    entries: Mutex<BTreeMap<PathBuf, SavedMatch>>,
    // Held across a save's durable write, so readers wait only for the swap.
    writer: Mutex<()>,
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
            validate(source, &saved.media, saved.title_pick.as_deref())
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
            writer: Mutex::new(()),
        })
    }

    pub fn get(&self, source: &Path) -> Option<SavedMatch> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(source)
            .cloned()
    }

    /// [`Self::save_for`] with the ISO as it is on disk now.
    #[cfg(test)]
    pub fn save(
        &self,
        source: &Path,
        expected_revision: u64,
        media: MediaMetadata,
    ) -> io::Result<SavedMatch> {
        self.save_for(
            source,
            expected_revision,
            media,
            super::links::iso_stamp(source),
        )
    }

    /// Compare-and-save prevents an old dialog overwriting a newer correction.
    /// Publish only after durable persistence succeeds; never erase corrupt state.
    /// `iso_stamp` is the disc the match is confirmed for.
    #[cfg(test)]
    pub fn save_for(
        &self,
        source: &Path,
        expected_revision: u64,
        media: MediaMetadata,
        iso_stamp: Option<Stamp>,
    ) -> io::Result<SavedMatch> {
        self.save_with_pick(source, expected_revision, media, None, iso_stamp)
    }

    /// Compare-and-save a match with the operator's titles (`None` = automatic selection);
    /// published only after the durable write. `iso_stamp`: the disc it is confirmed for.
    pub fn save_with_pick(
        &self,
        source: &Path,
        expected_revision: u64,
        media: MediaMetadata,
        title_pick: Option<Vec<TitleIdentity>>,
        iso_stamp: Option<Stamp>,
    ) -> io::Result<SavedMatch> {
        validate(source, &media, title_pick.as_deref())?;
        let _writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let (saved, next) = {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
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
                iso_stamp,
                title_pick,
            };
            let mut next = entries.clone();
            next.insert(source.to_path_buf(), saved.clone());
            (saved, next)
        };
        let bytes = serde_json::to_vec_pretty(&next).map_err(io::Error::other)?;
        // Only savers change the entries, and they wait on `writer`: nothing moved meanwhile.
        crate::server::ripper::staging::write_marker_durable(&self.path, &bytes)?;
        *self.entries.lock().unwrap_or_else(|e| e.into_inner()) = next;
        Ok(saved)
    }
}

fn validate(
    source: &Path,
    media: &MediaMetadata,
    pick: Option<&[TitleIdentity]>,
) -> io::Result<()> {
    // A pick suits its kind as on a drive: one Movie feature, or TV episodes (one alone needs
    // its First episode).
    if let Some(pick) = pick
        && let Err(error) = crate::server::titles::check_pick(
            media.kind.unwrap_or_default(),
            pick.len(),
            media.episode_start,
        )
    {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, error));
    }
    if !source.is_absolute()
        || source
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        || source.file_name().is_none()
        || media.title.trim().is_empty()
        || media.title.len() > 512
        || media.title.chars().any(char::is_control)
        // An exact name the operator typed has no TMDB id (`tmdb_id == 0`); it needs a title.
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

    // Remux under an exact name typed without a TMDB match: the title is what it needs.
    #[test]
    fn an_exact_name_without_a_tmdb_id_is_a_valid_match() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("samurai.iso");
        let store = MatchStore::open(dir.path()).unwrap();
        let mut typed = media(MediaKind::Tv);
        typed.title = "Samurai Champloo".into();
        typed.tmdb_id = 0;
        typed.season = Some(1);
        let saved = store.save(&iso, 0, typed.clone()).unwrap();
        assert_eq!(saved.media, typed);
        typed.title = "  ".into();
        assert!(
            store.save(&iso, 1, typed).is_err(),
            "a title is still required"
        );
    }

    // The operator's titles are saved with the match, in order, and read back; they must suit
    // the kind (one Movie feature; TV episodes, one alone with its First episode). A file
    // written before the field existed reads as automatic.
    #[test]
    fn a_title_pick_round_trips_with_the_match_and_suits_its_kind() {
        let dir = tempfile::tempdir().unwrap();
        let iso = dir.path().join("show.iso");
        let store = MatchStore::open(dir.path()).unwrap();
        let titles = crate::server::titles::tests::play_all_disc();
        let pick: Vec<_> = [3, 1]
            .iter()
            .map(|&i| TitleIdentity::of(&titles[i]))
            .collect();
        let saved = store
            .save_with_pick(&iso, 0, media(MediaKind::Tv), Some(pick.clone()), None)
            .unwrap();
        let reopened = MatchStore::open(dir.path()).unwrap().get(&iso).unwrap();
        assert_eq!(reopened, saved);
        assert_eq!(reopened.title_pick, Some(pick.clone()));
        assert_eq!(
            crate::server::planner::remap_pick(&titles, reopened.title_pick.as_deref().unwrap()),
            Some(vec![3, 1])
        );
        let one = Some(vec![pick[0].clone()]);
        for (kind, start, pick) in [
            (MediaKind::Movie, None, Some(pick.clone())),
            (MediaKind::Tv, None, one.clone()),
            (MediaKind::Tv, Some(1), Some(Vec::new())),
        ] {
            let mut value = media(kind);
            value.episode_start = start;
            let error = store
                .save_with_pick(&iso, 1, value, pick, None)
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        let mut tv = media(MediaKind::Tv);
        tv.episode_start = Some(4);
        assert!(store.save_with_pick(&iso, 1, tv, one, None).is_ok());
        let legacy =
            serde_json::json!({ "/iso/old.iso": { "revision": 1, "media": media(MediaKind::Tv) } });
        std::fs::write(store.path.clone(), legacy.to_string()).unwrap();
        let old = MatchStore::open(dir.path()).unwrap();
        assert_eq!(old.get(Path::new("/iso/old.iso")).unwrap().title_pick, None);
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
                    iso_stamp: None,
                    title_pick: None,
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
