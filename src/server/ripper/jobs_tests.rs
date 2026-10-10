use super::*;
use staging::{DiscState, StagingState, UserMetadata};

fn selection(title: &str) -> UserMetadata {
    UserMetadata {
        episode_start: None,
        title: title.into(),
        year: 2000,
        media_type: "movie".into(),
        tmdb_id: 8358,
        poster_url: String::new(),
        overview: "Selected by the operator".into(),
    }
}

fn job(root: &Path, name: &str, identity: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let mut st = DiscState::new(StagingState::Sweeping);
    st.disc_identity = identity.into();
    st.title = "Castaway".into();
    st.media_type = "movie".into();
    staging::try_write_state(&dir, &st).unwrap();
    dir
}

#[test]
fn correction_preserves_capture_bytes_and_paths_and_reuses_job() {
    let root = tempfile::tempdir().unwrap();
    let dir = job(root.path(), "Castaway", "disc-hash");
    let iso = dir.join("Castaway.iso");
    let map = dir.join("Castaway.iso.mapfile");
    std::fs::write(&iso, b"captured sectors").unwrap();
    std::fs::write(&map, b"original recovery map").unwrap();
    staging::mutate_state(&dir, StagingState::Sweeping, |s| {
        s.iso_path = iso.to_string_lossy().into_owned();
        s.mapfile_path = map.to_string_lossy().into_owned();
    })
    .unwrap();
    let mut worker = staging::read_state(&dir).unwrap();
    staging::save_user_metadata(&dir, selection("Cast Away")).unwrap();
    worker.state = StagingState::Ripped;
    staging::try_write_state(&dir, &worker).unwrap();
    assert_eq!(locate(root.path(), "disc-hash").unwrap(), "Castaway");
    assert_eq!(
        metadata(root.path(), "Castaway").unwrap().title,
        "Cast Away"
    );
    let restored = staging::read_state(&dir).unwrap();
    assert_eq!(restored.iso_path, iso.to_string_lossy());
    assert_eq!(restored.mapfile_path, map.to_string_lossy());
    assert_eq!(std::fs::read(iso).unwrap(), b"captured sectors");
    assert_eq!(std::fs::read(map).unwrap(), b"original recovery map");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn stopping_keeps_operator_metadata_and_capture_identity() {
    let root = tempfile::tempdir().unwrap();
    let dir = job(root.path(), "original", "hash");
    staging::save_user_metadata(&dir, selection("Cast Away")).unwrap();
    staging::clear_sweeping_marker(&dir);
    let stopped = staging::read_state(&dir).unwrap();
    assert_eq!(stopped.state, StagingState::Stopped);
    assert_eq!(stopped.title, "Cast Away");
    assert_eq!(locate(root.path(), "hash").unwrap(), "original");
    assert!(!staging::snapshot_staging_disc(&dir).unwrap().has_sweeping);
}

#[test]
fn repeated_edits_and_same_title_metadata_updates_are_durable() {
    let root = tempfile::tempdir().unwrap();
    let dir = job(root.path(), "job", "hash");
    let stale = staging::read_state(&dir).unwrap();
    staging::save_user_metadata(&dir, selection("Cast Away")).unwrap();
    let mut latest = selection("Cast Away");
    latest.year = 2001;
    latest.tmdb_id = 42;
    latest.overview = "Corrected overview".into();
    staging::save_user_metadata(&dir, latest.clone()).unwrap();
    staging::try_write_state(&dir, &stale).unwrap();
    let saved = staging::read_state(&dir).unwrap();
    assert_eq!(saved.user_metadata, Some(latest));
    assert_eq!(saved.year, 2001);
    assert_eq!(saved.tmdb_id, 42);
    assert_eq!(saved.metadata_revision, 2);
}

#[test]
fn same_title_different_discs_are_separate_and_duplicate_identity_is_held() {
    let root = tempfile::tempdir().unwrap();
    job(root.path(), "disc-one", "one");
    job(root.path(), "disc-two", "two");
    assert_eq!(locate(root.path(), "one").unwrap(), "disc-one");
    assert_eq!(locate(root.path(), "two").unwrap(), "disc-two");
    job(root.path(), "old-name", "one");
    assert!(
        locate(root.path(), "one")
            .unwrap_err()
            .contains("more than one")
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 3);
}

#[test]
fn legacy_aacs_map_is_adopted_without_renaming() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("OldMovie");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("OldMovie.iso"), vec![0; 4096]).unwrap();
    let mut map =
        freemkv_engine::Mapfile::create(&dir.join("OldMovie.iso.mapfile"), 4096, "test").unwrap();
    let hash = "0123456789abcdef0123456789abcdef01234567";
    map.set_disc_hash(hash);
    map.flush().unwrap();
    assert_eq!(locate(root.path(), hash).unwrap(), "OldMovie");
}

#[test]
fn unreadable_or_colliding_state_is_not_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let dir = job(root.path(), "disc-hash", "hash");
    std::fs::write(dir.join(staging::STATE_FILE), b"{broken").unwrap();
    assert!(locate(root.path(), "hash").is_err());
    assert!(staging::save_user_metadata(&dir, selection("New")).is_err());
    assert!(staging::try_write_state(&dir, &DiscState::new(StagingState::Sweeping)).is_err());
    assert_eq!(
        std::fs::read(dir.join(staging::STATE_FILE)).unwrap(),
        b"{broken"
    );
}

#[test]
fn delivery_boundary_refuses_corrections_without_mutating_state() {
    let root = tempfile::tempdir().unwrap();
    let dir = job(root.path(), "job", "hash");
    staging::mutate_state(&dir, StagingState::Done, |s| s.state = StagingState::Done).unwrap();
    let before = std::fs::read(dir.join(staging::STATE_FILE)).unwrap();
    assert!(staging::save_user_metadata(&dir, selection("New")).is_err());
    assert_eq!(
        std::fs::read(dir.join(staging::STATE_FILE)).unwrap(),
        before
    );
}

#[test]
fn tv_identity_change_replans_before_delivery_and_rejects_stale_plan_commit() {
    let root = tempfile::tempdir().unwrap();
    let dir = job(root.path(), "job", "hash");
    staging::mutate_state(&dir, StagingState::Ripped, |s| {
        s.media_type = "tv".into();
        s.tmdb_id = 1;
        s.max_retries = 3;
        s.outputs.push(staging::Output {
            filename: "episode.mkv".into(),
            episode: Some(1),
            ..Default::default()
        });
    })
    .unwrap();
    let mut worker = staging::read_state(&dir).unwrap();
    let mut m = selection("Correct Show");
    m.media_type = "tv".into();
    staging::save_user_metadata(&dir, m).unwrap();
    worker.state = StagingState::Done;
    staging::try_write_state(&dir, &worker).unwrap();
    let saved = staging::read_state(&dir).unwrap();
    assert_eq!(saved.state, StagingState::Ripped);
    assert!(saved.replan_required);
    assert!(staging::save_replanned_outputs(&dir, 0, vec![]).is_err());
    staging::save_replanned_outputs(&dir, saved.metadata_revision, saved.outputs).unwrap();
    assert!(!staging::read_state(&dir).unwrap().replan_required);
}

struct Samples {
    byte: u8,
    short: bool,
}
impl libfreemkv::SectorSource for Samples {
    fn read_sectors(
        &mut self,
        _: u32,
        _: u16,
        buf: &mut [u8],
        _: bool,
    ) -> libfreemkv::Result<usize> {
        buf.fill(self.byte);
        Ok(if self.short { 0 } else { buf.len() })
    }
}

#[test]
fn non_aacs_identity_is_content_and_capacity_based_and_refuses_short_reads() {
    let capacity = 2048 * 1024;
    let mut reader = Samples {
        byte: 1,
        short: false,
    };
    let original = sample_fingerprint(capacity, &mut reader).unwrap();
    assert_eq!(sample_fingerprint(capacity, &mut reader).unwrap(), original);
    assert_ne!(
        sample_fingerprint(capacity * 2, &mut reader).unwrap(),
        original
    );
    reader.byte = 2;
    assert_ne!(sample_fingerprint(capacity, &mut reader).unwrap(), original);
    reader.short = true;
    assert!(sample_fingerprint(capacity, &mut reader).is_err());
}

#[test]
fn unreadable_identity_gets_an_isolated_namespace_without_claiming_identity() {
    let root = tempfile::tempdir().unwrap();
    let existing = job(root.path(), "existing", "known");
    assert!(existing.exists());

    let first = fresh_unverified_job(root.path()).unwrap();
    assert!(first.starts_with("unverified-"));
    assert_ne!(first, "known");
    assert!(root.path().join(&first).is_dir());

    let second = fresh_unverified_job(root.path()).unwrap();
    assert_ne!(second, first);
    assert!(second.starts_with("unverified-"));
}

#[test]
fn concurrent_unverified_scans_reserve_distinct_jobs() {
    let root = tempfile::tempdir().unwrap();
    let barrier = std::sync::Barrier::new(8);
    let jobs = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    fresh_unverified_job(root.path()).unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<std::collections::HashSet<_>>()
    });
    assert_eq!(jobs.len(), 8);
    for job in jobs {
        assert!(root.path().join(job).is_dir());
    }
}

#[test]
fn staging_job_path_rejects_traversal() {
    for name in ["", "..", "../outside", "/outside", "a/b", "a\\b"] {
        assert!(path(Path::new("/staging"), name).is_err(), "{name}");
    }
}

#[test]
fn stale_device_request_cannot_retitle_a_replacement_disc() {
    use crate::server::{config::Config, ripper};
    use std::sync::{Arc, RwLock};
    let root = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RwLock::new(Config {
        staging_dir: root.path().to_string_lossy().into_owned(),
        ..Default::default()
    }));
    let device = "job-stale-request";
    ripper::update_state(
        device,
        ripper::RipState {
            disc_present: true,
            job_id: "new-job".into(),
            disc_identity: "new-disc".into(),
            ..Default::default()
        },
    );
    let result = ripper::retitle_staging_for_device(&cfg, device, "old-job", selection("Wrong"));
    ripper::STATE.lock().unwrap().remove(device);
    assert!(result.is_err());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn stale_progress_preserves_user_selection_but_new_disc_does_not_inherit_it() {
    use crate::server::ripper;
    let device = "job-progress-metadata";
    let stale = ripper::RipState {
        disc_present: true,
        disc_name: "Castaway".into(),
        disc_label: "CASTAWAY".into(),
        job_id: "one".into(),
        disc_identity: "one".into(),
        status: "ripping".into(),
        ..Default::default()
    };
    ripper::update_state(device, stale.clone());
    ripper::update_state_with(device, |s| {
        s.user_metadata = Some(selection("Cast Away"));
        s.disc_name = "Cast Away".into();
    });
    ripper::update_state(device, stale.clone());
    assert_eq!(
        ripper::STATE.lock().unwrap()[device].tmdb_title,
        "Cast Away"
    );
    ripper::update_state(
        device,
        ripper::RipState {
            job_id: "two".into(),
            disc_identity: "two".into(),
            ..stale
        },
    );
    let row = ripper::STATE.lock().unwrap().remove(device).unwrap();
    assert!(row.user_metadata.is_none());
}

#[test]
fn metadata_only_stopped_job_survives_startup_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let dir = job(root.path(), "job", "hash");
    staging::save_user_metadata(&dir, selection("Cast Away")).unwrap();
    staging::clear_sweeping_marker(&dir);
    staging::resume_or_quarantine_staging(root.path().to_str().unwrap());
    assert_eq!(staging::read_state(&dir).unwrap().title, "Cast Away");
}
