//! Whole-image jobs use durable capture authority, never incidental current settings.
use super::*;

struct Job {
    _temp: tempfile::TempDir,
    dir: PathBuf,
    iso: PathBuf,
    map: PathBuf,
    cfg: Arc<RwLock<Config>>,
    before: (Vec<u8>, Vec<u8>),
}

impl Job {
    fn new(saved_format: Option<&str>, current: &str, keys: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        // SAFETY: each caller holds env_guard for this fixture's lifetime.
        unsafe { std::env::set_var("AUTORIP_DIR", temp.path()) };
        let dir = temp.path().join("staging").join("Original");
        std::fs::create_dir_all(&dir).unwrap();
        let key = crate::ku_fixture::K1;
        let fx = crate::ku_fixtures::bd_image(&[Some(key), Some(key)], 1);
        assert_eq!(fx.disc.titles.len(), 2);
        let config = Config {
            output_format: current.into(),
            tv_auto: true,
            staging_dir: dir.parent().unwrap().to_string_lossy().into_owned(),
            ..Default::default()
        };
        assert!(matches!(
            crate::server::planner::check_episode_selection(
                &fx.disc.titles,
                &config,
                Some(MediaKind::Tv)
            ),
            Err(crate::server::planner::PlanError::Selection(_))
        ));
        let iso = fx.write(&dir, "Original.iso");
        crate::ku_fixtures::sidecar(&fx, &iso, true);
        let map = freemkv_engine::mapfile_path_for(&iso);
        let keydb = temp.path().join("keydb.cfg");
        if keys {
            let hash = libfreemkv::hex::strip_hex_prefix(&fx.disc.aacs.as_ref().unwrap().disc_hash);
            let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
            std::fs::write(&keydb, format!("0x{hash} = ISO | U | 1-0x{hex}\n")).unwrap();
        }
        let mut saved = staging::DiscState::new(staging::StagingState::Ripped);
        saved.output_format = saved_format.map(str::to_owned);
        saved.media_type = MediaKind::Tv;
        saved.disc_name = "Original".into();
        saved.disc_label = "SHOW_S1_D2".into();
        saved.title = "Original".into();
        saved.iso_path = iso.to_string_lossy().into_owned();
        saved.mapfile_path = map.to_string_lossy().into_owned();
        saved.max_retries = 2;
        saved.season = Some(1);
        saved.disc_number = Some(2);
        staging::try_write_state(&dir, &saved).unwrap();
        let before = (std::fs::read(&iso).unwrap(), std::fs::read(&map).unwrap());
        let cfg = Arc::new(RwLock::new(Config {
            keydb_path: Some(keydb.to_string_lossy().into_owned()),
            ..config
        }));
        Self {
            _temp: temp,
            dir,
            iso,
            map,
            cfg,
            before,
        }
    }

    fn run(&self) -> MuxHandoffOutcome {
        let marker = staging::read_state(&self.dir).unwrap().to_ripped_marker();
        remux_from_ripped_marker(&self.cfg, &self.dir, &marker)
    }

    fn assert_capture_retained(&self) {
        assert_eq!(std::fs::read(&self.iso).unwrap(), self.before.0);
        assert_eq!(std::fs::read(&self.map).unwrap(), self.before.1);
        assert!(!self.dir.join("Original.mkv").exists());
        assert!(!self.dir.join("Original.m2ts").exists());
    }

    fn assert_iso_handoff(&self) -> staging::DiscState {
        let saved = staging::read_state(&self.dir).unwrap();
        assert!(!saved.replan_required);
        assert_eq!(
            saved.outputs,
            vec![staging::Output {
                filename: "Original.iso".into(),
                ..Default::default()
            }]
        );
        let snap = staging::snapshot_staging_disc(&self.dir).unwrap();
        assert!(snap.completed);
        assert_eq!(
            crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
            crate::server::muxer::MuxVerdict::SkipTerminal
        );
        self.assert_capture_retained();
        assert!(std::fs::read_dir(&self.dir).unwrap().all(|entry| {
            !matches!(
                entry
                    .unwrap()
                    .path()
                    .extension()
                    .and_then(|ext| ext.to_str()),
                Some("mkv" | "mk3d" | "m2ts")
            )
        }));
        saved
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        super::super::STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove("_mux");
        crate::server::muxer::MUX_ERRORS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.dir.to_str().unwrap());
    }
}

fn correction(id: u64) -> staging::UserMetadata {
    staging::UserMetadata {
        title: "Corrected show".into(),
        year: 2025,
        media_type: MediaKind::Tv,
        tmdb_id: id,
        episode_start: Some(7),
        poster_url: String::new(),
        overview: "Corrected description".into(),
    }
}

#[test]
fn pending_whole_iso_without_title_plan_resumes_under_both_settings() {
    let _env = crate::server::log::env_guard();
    let _state = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for current in ["iso", "mkv"] {
        let job = Job::new(Some("iso"), current, true);
        let outcome = job.run();
        assert!(outcome.success, "{:?}", outcome.failure_reason);
        job.assert_iso_handoff();
        assert_eq!(job.cfg.read().unwrap().output_format, current);
    }
}

#[test]
fn corrected_whole_iso_replans_only_actual_image_and_keeps_metadata() {
    let _env = crate::server::log::env_guard();
    let _state = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let job = Job::new(Some("iso"), "mkv", true);
    staging::mutate_state(&job.dir, staging::StagingState::Review, |s| {
        s.state = staging::StagingState::Review;
        s.outputs = vec![staging::Output {
            filename: "Original.iso".into(),
            ..Default::default()
        }];
    })
    .unwrap();
    let revision = staging::save_review_metadata(&job.dir, correction(42)).unwrap();
    let changed = staging::read_state(&job.dir).unwrap();
    assert!(changed.replan_required);
    assert_eq!(changed.state, staging::StagingState::Ripped);
    // Whole-image delivery makes no presentation-language or episode choice.
    job.cfg.write().unwrap().presentation_language = "jpn".into();
    let outcome = job.run();
    assert!(outcome.success, "{:?}", outcome.failure_reason);
    let saved = job.assert_iso_handoff();
    assert_eq!(saved.metadata_revision, revision);
    assert_eq!(saved.user_metadata, Some(correction(42)));
    assert_eq!(
        (saved.tmdb_id, saved.year, saved.media_type),
        (42, 2025, MediaKind::Tv)
    );
    assert_eq!((saved.season, saved.disc_number), (Some(1), Some(2)));
    assert_eq!(saved.iso_path, changed.iso_path);
    assert_eq!(saved.mapfile_path, changed.mapfile_path);
    assert!(!staging::read_state(&job.dir).unwrap().replan_required);
}

#[test]
fn current_iso_does_not_authorize_saved_mkv_or_unproven_legacy_reselection() {
    let _env = crate::server::log::env_guard();
    let _state = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (format, has_plan) in [(Some("mkv"), true), (None, true), (None, false)] {
        let job = Job::new(format, "iso", true);
        let old_outputs = if has_plan {
            vec![staging::Output {
                filename: "kept.mkv".into(),
                ..Default::default()
            }]
        } else {
            Vec::new()
        };
        staging::mutate_state_if_present(&job.dir, |s| {
            s.outputs = old_outputs.clone();
            s.replan_required = true;
        });
        std::fs::write(job.dir.join("kept.mkv"), b"partial").unwrap();
        let outcome = job.run();
        assert!(!outcome.success);
        assert!(
            outcome
                .failure_reason
                .as_deref()
                .unwrap_or("")
                .contains("E9086"),
            "{:?}",
            outcome.failure_reason
        );
        let saved = staging::read_state(&job.dir).unwrap();
        assert!(saved.title_identity_review);
        assert_eq!(saved.outputs, old_outputs);
        assert_eq!(std::fs::read(job.dir.join("kept.mkv")).unwrap(), b"partial");
        job.assert_capture_retained();
    }
}

#[test]
fn whole_iso_selection_exemption_preserves_key_deferral_and_whole_disc_loss_gate() {
    let _env = crate::server::log::env_guard();
    let _state = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let keyless = Job::new(Some("iso"), "mkv", false);
    let outcome = keyless.run();
    assert!(!outcome.success);
    assert!(outcome.failure_retryable, "{:?}", outcome.failure_reason);
    keyless.assert_capture_retained();
    drop(keyless);
    let damaged = Job::new(Some("iso"), "mkv", true);
    let mut map = freemkv_engine::Mapfile::load(&damaged.map).unwrap();
    map.record(0, 2048, freemkv_engine::SectorStatus::Unreadable)
        .unwrap();
    map.flush().unwrap();
    let outcome = damaged.run();
    assert!(!outcome.success);
    assert!(
        outcome
            .failure_reason
            .as_deref()
            .unwrap_or("")
            .contains("disc loss"),
        "{:?}",
        outcome.failure_reason
    );
    assert_eq!(std::fs::read(&damaged.iso).unwrap(), damaged.before.0);
}

#[test]
fn whole_image_replan_transaction_rejects_a_stale_metadata_revision() {
    let _env = crate::server::log::env_guard();
    let _state = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let job = Job::new(Some("iso"), "iso", true);
    staging::mutate_state(&job.dir, staging::StagingState::Review, |s| {
        s.state = staging::StagingState::Review;
        s.outputs = vec![staging::Output {
            filename: "Original.iso".into(),
            ..Default::default()
        }];
    })
    .unwrap();
    let revision = staging::save_review_metadata(&job.dir, correction(42)).unwrap();
    staging::save_user_metadata(&job.dir, correction(43)).unwrap();
    let newest = staging::read_state(&job.dir).unwrap();
    assert!(
        staging::save_replanned_outputs(
            &job.dir,
            revision,
            vec![staging::Output {
                filename: "Original.iso".into(),
                ..Default::default()
            }]
        )
        .is_err()
    );
    let saved = staging::read_state(&job.dir).unwrap();
    assert_eq!(saved, newest);
    assert!(saved.replan_required);
    job.assert_capture_retained();
}
