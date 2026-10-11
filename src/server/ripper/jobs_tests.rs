use super::*;
use staging::{DiscState, StagingState, UserMetadata};

fn selection(title: &str) -> UserMetadata {
    UserMetadata {
        episode_start: None,
        title: title.into(),
        year: 2000,
        media_type: crate::server::planner::MediaKind::Movie,
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
    let iso = dir.join("Castaway.iso");
    std::fs::write(&iso, b"retained capture").unwrap();
    staging::mutate_state(&dir, StagingState::Ripped, |s| {
        s.media_type = "tv".into();
        s.tmdb_id = 1;
        s.max_retries = 3;
        s.iso_path = iso.to_string_lossy().into_owned();
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

    let first = fresh_unverified_job(&staging_cfg(root.path())).unwrap();
    assert!(first.starts_with("unverified-"));
    assert_ne!(first, "known");
    assert!(root.path().join(&first).is_dir());

    let second = fresh_unverified_job(&staging_cfg(root.path())).unwrap();
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
                    fresh_unverified_job(&staging_cfg(root.path())).unwrap()
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

fn partial_capture(dir: &Path, stem: &str, map_hash: Option<&str>) {
    std::fs::write(dir.join(format!("{stem}.iso")), b"x").unwrap();
    let mut map =
        freemkv_engine::Mapfile::create(&dir.join(format!("{stem}.iso.mapfile")), 4096, "test")
            .unwrap();
    if let Some(hash) = map_hash {
        map.set_disc_hash(hash);
    }
    map.flush().unwrap();
}

#[test]
fn resume_probes_use_the_same_identity_rule_as_job_location() {
    let live = "0123456789abcdef0123456789abcdef01234567";
    let other = "fedcba9876543210fedcba9876543210fedcba98";
    let root = tempfile::tempdir().unwrap();
    // Saved identity names another disc; only its mapfile carries the live hash.
    let dir = job(root.path(), "OtherDisc", other);
    staging::clear_sweeping_marker(&dir);
    partial_capture(&dir, "OtherDisc", Some(live));
    assert!(!dir_matches_identity(&dir, live));
    assert_eq!(locate(root.path(), live).unwrap(), format!("disc-{live}"));
    let cfg = crate::server::config::Config {
        staging_dir: root.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let located = locate(root.path(), live).unwrap();
    assert_eq!(
        super::super::resumable_for_job(&cfg, &located),
        None,
        "a job that location would not adopt is not offered for resume"
    );
}

fn seed_job(device: &str, job: &str, identity: &str) {
    crate::server::ripper::update_state(
        device,
        crate::server::ripper::RipState {
            disc_present: true,
            disc_name: "Same Title".into(),
            disc_label: "SAME_LABEL".into(),
            job_id: job.into(),
            disc_identity: identity.into(),
            ..Default::default()
        },
    );
}

fn staging_cfg(root: &Path) -> crate::server::config::Config {
    crate::server::config::Config {
        staging_dir: root.to_string_lossy().into_owned(),
        ..Default::default()
    }
}

fn shared_cfg(root: &Path) -> std::sync::Arc<std::sync::RwLock<crate::server::config::Config>> {
    std::sync::Arc::new(std::sync::RwLock::new(crate::server::config::Config {
        staging_dir: root.to_string_lossy().into_owned(),
        ..Default::default()
    }))
}

#[test]
fn an_unverified_disc_is_never_offered_another_jobs_capture() {
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let root = tempfile::tempdir().unwrap();
    // An older title-named capture of the same title/label, identity-bearing or not.
    let legacy = root.path().join("Same_Title");
    std::fs::create_dir(&legacy).unwrap();
    partial_capture(&legacy, "Same_Title", Some(hash));
    let job = fresh_unverified_job(&staging_cfg(root.path())).unwrap();
    let cfg = shared_cfg(root.path());
    let device = "job-unverified-resume";
    seed_job(device, &job, "");
    let offered = super::super::resumable_for_device(&cfg, device);
    let remux = super::super::find_resumable_for_disc(&cfg, device);
    crate::server::ripper::STATE.lock().unwrap().remove(device);
    assert_eq!(offered, None);
    assert!(remux.is_none());
}

#[test]
fn two_boxset_discs_with_one_title_get_two_jobs() {
    let one = "1111111111111111111111111111111111111111";
    let two = "2222222222222222222222222222222222222222";
    let root = tempfile::tempdir().unwrap();
    // Disc one is mid-rip in a 1.8.3 title-named dir.
    let legacy = root.path().join("Boxset_Movie");
    std::fs::create_dir(&legacy).unwrap();
    partial_capture(&legacy, "Boxset_Movie", Some(one));
    let job_one = locate(root.path(), one).unwrap();
    let job_two = locate(root.path(), two).unwrap();
    assert_eq!(job_one, "Boxset_Movie");
    assert_eq!(job_two, format!("disc-{two}"));
    let cfg = shared_cfg(root.path());
    let device = "job-boxset-disc-two";
    seed_job(device, &job_two, two);
    let offered = super::super::resumable_for_device(&cfg, device);
    crate::server::ripper::STATE.lock().unwrap().remove(device);
    assert_eq!(
        offered, None,
        "disc two is never offered disc one's capture"
    );
}

#[test]
fn a_1_8_3_title_named_partial_still_resumes() {
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let root = tempfile::tempdir().unwrap();
    // 1.8.3 layout: title-named dir, no state.json identity, AACS hash in the map.
    let legacy = root.path().join("Old_Movie");
    std::fs::create_dir(&legacy).unwrap();
    partial_capture(&legacy, "Old_Movie", Some(hash));
    std::fs::write(legacy.join(staging::DISC_LABEL_FILE), b"OLD_MOVIE").unwrap();
    let job = locate(root.path(), hash).unwrap();
    assert_eq!(job, "Old_Movie", "adopted in place, never renamed");
    let cfg = shared_cfg(root.path());
    let device = "job-legacy-resume";
    seed_job(device, &job, hash);
    let offered = super::super::resumable_for_device(&cfg, device);
    crate::server::ripper::STATE.lock().unwrap().remove(device);
    assert_eq!(offered, Some(crate::server::ripper::Resumable::Sweep));
    // The first capture stamps the identity; the job is identity-keyed from then on.
    staging::begin_capture(&legacy, 0, hash).unwrap();
    assert_eq!(staging::read_state(&legacy).unwrap().disc_identity, hash);
    assert_eq!(locate(root.path(), hash).unwrap(), "Old_Movie");
}

#[test]
fn a_malformed_aacs_hash_is_not_an_identity() {
    assert_eq!(
        aacs_identity("0x0123456789ABCDEF0123456789abcdef01234567").as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(aacs_identity("not-a-hash"), None);
    assert_eq!(aacs_identity(""), None);
}

#[test]
fn a_capture_that_returns_before_its_sweep_stays_locatable() {
    let identity = "0123456789abcdef0123456789abcdef01234567";
    let root = tempfile::tempdir().unwrap();
    let job = format!("disc-{identity}");
    let dir = root.path().join(&job);
    std::fs::create_dir(&dir).unwrap();
    staging::begin_capture(&dir, 0, identity).unwrap();
    // An early return ("No titles", a selection review) drops SweepingGuard.
    staging::clear_sweeping_marker(&dir);
    assert_eq!(staging::read_state(&dir).unwrap().disc_identity, identity);
    assert_eq!(locate(root.path(), identity).unwrap(), job);
    let other = "fedcba9876543210fedcba9876543210fedcba98";
    assert!(staging::begin_capture(&dir, 0, other).is_err());
    assert_eq!(staging::read_state(&dir).unwrap().disc_identity, identity);
}

#[test]
fn a_disc_whose_identity_was_not_resolved_has_no_staging_job() {
    use crate::server::{config::Config, ripper};
    use std::sync::{Arc, RwLock};
    let root = tempfile::tempdir().unwrap();
    let title_dir = root.path().join("Same_Title");
    std::fs::create_dir(&title_dir).unwrap();
    partial_capture(&title_dir, "Same_Title", None);
    let cfg = Arc::new(RwLock::new(Config {
        staging_dir: root.path().to_string_lossy().into_owned(),
        ..Default::default()
    }));
    // Location refused (duplicate hold): the card keeps its title, no job.
    let device = "job-unresolved-identity";
    ripper::update_state(
        device,
        ripper::RipState {
            disc_present: true,
            disc_name: "Same Title".into(),
            disc_label: "SAME_LABEL".into(),
            ..Default::default()
        },
    );
    let offered = super::super::resumable_for_device(&cfg, device);
    let remux = super::super::find_resumable_for_disc(&cfg, device);
    ripper::STATE.lock().unwrap().remove(device);
    assert_eq!(offered, None, "a title-named dir is not this disc's job");
    assert!(remux.is_none());
}

#[test]
fn a_scan_that_read_disk_before_a_correction_published_keeps_the_correction() {
    // Retitle passed its not-scanning check, then the scan read rev 0 from disk
    // while the correction committed rev 1 and published it to the card.
    let mut row = crate::server::ripper::RipState {
        job_id: "job".into(),
        ..Default::default()
    };
    row.publish_user_metadata("job", 1, selection("Cast Away"));
    row.restore_job_metadata("job", "hash", None, 0);
    assert_eq!(row.user_metadata, Some(selection("Cast Away")));
    assert_eq!(row.metadata_revision, 1);
    // A different job's scan resets it.
    row.restore_job_metadata("other", "hash2", None, 0);
    assert!(row.user_metadata.is_none());
}

// A title saved from a raw-label fallback (TMDB down, no key) or a guess is not an
// operator choice: a later scan or rip looks the disc up again.
#[test]
fn only_a_correction_or_a_confident_lookup_skips_the_next_lookup() {
    let mut st = DiscState::new(StagingState::Stopped);
    st.title = "THE_OFFICE".into();
    assert!(
        metadata_from_state(st.clone()).is_none(),
        "raw label reused as metadata"
    );

    let mut confident = st.clone();
    confident.title = "The Office".into();
    confident.year = 2005;
    confident.tmdb_confident = true;
    assert_eq!(metadata_from_state(confident).unwrap().title, "The Office");

    let mut corrected = st;
    corrected.user_metadata = Some(selection("The Office"));
    assert_eq!(metadata_from_state(corrected).unwrap().title, "The Office");
}

#[test]
fn a_lookup_is_confident_only_when_it_matches_the_label_with_a_year() {
    let found = |title: &str, year| crate::server::tmdb::TmdbResult {
        title: title.into(),
        year,
        poster_url: String::new(),
        overview: String::new(),
        media_type: crate::server::planner::MediaKind::Movie,
        tmdb_id: 949,
    };
    let hit = found("Heat", 1995);
    assert!(lookup_confident("HEAT", Some(&hit)));
    assert!(!lookup_confident("HEAT", None));
    let guess = found("Heat Wave", 1990);
    assert!(!lookup_confident("HEAT", Some(&guess)));
}

// A `disc-<identity>` dir made just before its first state write (another drive's
// capture start, or a first write that failed) is adopted, never a permanent error.
#[test]
fn locate_adopts_its_own_job_dir_before_the_identity_is_written() {
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef01234567";
    let dir = root.path().join(format!("disc-{id}"));
    std::fs::create_dir(&dir).unwrap();
    assert_eq!(locate(root.path(), id).unwrap(), format!("disc-{id}"));
    std::fs::write(dir.join("state.json.tmp"), b"{").unwrap();
    assert_eq!(locate(root.path(), id).unwrap(), format!("disc-{id}"));
}

#[test]
fn locate_still_refuses_a_job_dir_holding_another_identity_or_unproved_data() {
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef01234567";
    let dir = root.path().join(format!("disc-{id}"));
    std::fs::create_dir(&dir).unwrap();
    let mut st = DiscState::new(StagingState::Stopped);
    st.disc_identity = "another-disc".into();
    staging::try_write_state(&dir, &st).unwrap();
    assert!(locate(root.path(), id).is_err());
    std::fs::remove_file(dir.join(staging::STATE_FILE)).unwrap();
    std::fs::write(dir.join("Movie.iso"), b"capture").unwrap();
    assert!(locate(root.path(), id).is_err());
}

// A partial listing or an entry that cannot be inspected may hide this disc's job:
// locate holds instead of starting a fresh job beside it.
#[test]
fn locate_refuses_a_partial_listing_or_an_uninspectable_entry() {
    let root = tempfile::tempdir().unwrap();
    job(root.path(), "Other", "other-hash");
    let listing = |names: &[&str], complete| {
        Some(super::super::StagingListing {
            names: names.iter().map(|n| n.to_string()).collect(),
            complete,
        })
    };
    assert_eq!(
        locate_listed(root.path(), "hash", listing(&["Other"], true)).unwrap(),
        "disc-hash"
    );
    assert!(locate_listed(root.path(), "hash", listing(&["Other"], false)).is_err());
    let gone = locate_listed(root.path(), "hash", listing(&["Other", "Vanished"], true));
    assert!(gone.unwrap_err().contains("Vanished"));
}

// A configured staging folder whose share is not mounted (its mount point is an empty
// folder) is not made on the local disk; nor is the isolated job inside it.
#[test]
fn an_unverified_job_is_not_made_under_an_unmounted_shares_mount_point() {
    let base = tempfile::tempdir().unwrap();
    let mount_point = base.path().join("nas");
    std::fs::create_dir(&mount_point).unwrap();
    let root = mount_point.join("staging");
    let error = fresh_unverified_job(&staging_cfg(&root)).unwrap_err();
    assert!(error.contains("empty"), "{error}");
    assert!(!root.exists());
    // Mounted (the share holds folders): made, one level.
    std::fs::create_dir(mount_point.join("Movies")).unwrap();
    let job = fresh_unverified_job(&staging_cfg(&root)).unwrap();
    assert!(root.join(job).is_dir());
}
