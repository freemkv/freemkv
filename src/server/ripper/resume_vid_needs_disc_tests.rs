use super::*;
use crate::ku_fixture::{bd_image, write_sidecar};

// KU-E1 (J6, J11): the mapfile holds no Volume ID. A resume with no drive in hand whose
// keys need it refuses E7034 up front: one clear "insert the disc" state that the mux
// worker then holds (never re-dispatched), with the ISO and mapfile untouched.
#[test]
fn a_resume_whose_keys_need_the_disc_is_held_not_retried() {
    let (staging, outcome, _t) = resume_after_restart(Keys::MediaKeyOnly);
    assert!(!outcome.success);
    assert!(outcome.failure_needs_disc, "held for the disc");
    assert!(!outcome.failure_retryable && !outcome.failure_finalize);
    let reason = outcome.failure_reason.unwrap_or_default();
    assert!(
        reason.starts_with("E7034 ") && reason.contains("Insert the disc to finish"),
        "{reason}"
    );

    let snap = staging::snapshot_staging_disc(&staging).unwrap();
    assert!(snap.needs_disc && snap.has_ripped && !snap.has_failed);
    assert_eq!(
        crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
        crate::server::muxer::MuxVerdict::SkipNeedsDisc,
        "the worker must not re-dispatch it"
    );

    // Inserting the disc makes it drive-resumable again (the worker still skips it).
    assert!(!super::super::resumable_dir_blocked(&snap));
    staging::set_needs_disc(&staging, false);
    let snap = staging::snapshot_staging_disc(&staging).unwrap();
    assert_eq!(
        crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
        crate::server::muxer::MuxVerdict::Dispatch
    );
}

// J23: when no VID could help (no media-key path, no VID-consuming source) a resume after
// a restart is a plain "no key yet" (E7022): the retryable keyless deferral, never held.
#[test]
fn a_resume_no_vid_would_help_is_a_retryable_deferral() {
    let (staging, outcome, _t) = resume_after_restart(Keys::None);
    assert!(!outcome.success);
    assert!(!outcome.failure_needs_disc);
    assert!(
        outcome.failure_retryable,
        "a keyless deferral re-muxes once keys land"
    );
    let snap = staging::snapshot_staging_disc(&staging).unwrap();
    assert!(!snap.needs_disc && !snap.has_failed);
    assert_eq!(
        crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
        crate::server::muxer::MuxVerdict::Dispatch
    );
}

// An online key service that is down is an outage (E7028), never Missing: the resume
// defers retryably (the worker re-asks next tick), never holds for the disc or fails.
#[test]
fn a_resume_with_the_key_service_down_is_a_retryable_outage() {
    let (staging, outcome, _t) = resume_after_restart(Keys::ServiceDown);
    assert!(!outcome.success);
    assert!(
        !outcome.failure_needs_disc,
        "an outage is never 'insert the disc'"
    );
    assert!(outcome.failure_retryable, "the worker retries it");
    assert!(!outcome.failure_finalize);
    let reason = outcome.failure_reason.unwrap_or_default();
    assert!(
        reason.starts_with("Ripped to ISO — no keys, mux deferred"),
        "{reason}"
    );
    let snap = staging::snapshot_staging_disc(&staging).unwrap();
    assert!(
        !snap.needs_disc && !snap.has_failed,
        "never held, never .failed"
    );
    assert_eq!(
        crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
        crate::server::muxer::MuxVerdict::Dispatch
    );
}

// The key chain a restarted resume has.
enum Keys {
    // A keydb entry with only a media key: the VID would finish it (J23).
    MediaKeyOnly,
    // No key source holds anything.
    None,
    // An online service that refuses at the first query (E7028).
    ServiceDown,
    // A keydb entry holding the clip's unit key: the resume can deliver.
    UnitKey,
}

// A `.ripped` KU fixture resumed by the mux worker with no set in memory (a restart).
fn resume_after_restart(keys: Keys) -> (std::path::PathBuf, MuxHandoffOutcome, tempfile::TempDir) {
    let (staging, outcome, t, iso, mapfile, before) = resume_ripped(keys, |_, _, _| {});
    let after = (
        std::fs::read(&iso).unwrap(),
        std::fs::read(&mapfile).unwrap(),
    );
    assert!(after == before, "the ISO and its mapfile are untouched");
    (staging, outcome, t)
}

type Resumed = (
    std::path::PathBuf,
    MuxHandoffOutcome,
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    (Vec<u8>, Vec<u8>),
);

// The fixture's `.ripped` dir resumed through the mux worker; `setup` adjusts the staging
// dir and the marker before it is written.
fn resume_ripped(
    keys: Keys,
    setup: impl FnOnce(&std::path::Path, &mut crate::server::muxer::RippedMarker, &mut Config),
) -> Resumed {
    let _guard = crate::server::log::env_guard();
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    // SAFETY: env access in tests, serialized by env_guard.
    unsafe {
        std::env::set_var("AUTORIP_DIR", t.path());
    }
    let staging = t.path().join("staging").join("KU_Disc");
    std::fs::create_dir_all(&staging).unwrap();
    let fx = bd_image();
    let iso = fx.write(&staging, "KU_Disc.iso");
    let mapfile = write_sidecar(&fx, &iso, true);
    let keydb = t.path().join("keydb.cfg");
    match keys {
        Keys::MediaKeyOnly => crate::ku_fixture::write_media_key_keydb(&fx, &keydb),
        Keys::UnitKey => {
            let hash = &fx.disc.aacs.as_ref().unwrap().disc_hash;
            let hash = libfreemkv::hex::strip_hex_prefix(hash);
            let k1: String = crate::ku_fixture::K1
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            std::fs::write(&keydb, format!("0x{hash} = KU | U | 1-0x{k1}\n")).unwrap();
        }
        _ => {}
    }
    // `localhost` resolves to a private address: refused at the first query, no network.
    let keyserver_url = match keys {
        Keys::ServiceDown => "https://localhost:9/decode".to_string(),
        _ => String::new(),
    };
    let before = (
        std::fs::read(&iso).unwrap(),
        std::fs::read(&mapfile).unwrap(),
    );
    let mut marker = crate::server::muxer::RippedMarker {
        schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
        iso_path: iso.to_string_lossy().into_owned(),
        mapfile_path: mapfile.to_string_lossy().into_owned(),
        display_name: "KU_Disc".into(),
        disc_format: "bluray".into(),
        mkv_filename: "KU_Disc.mkv".into(),
        tmdb_title: String::new(),
        tmdb_year: 0,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        tmdb_media_type: String::new(),
        max_retries: 1,
        abort_on_lost_secs: 0,
        rip_elapsed_secs: 0.0,
        rip_errors: 0,
        rip_lost_video_secs: 0.0,
        rip_last_sector: 0,
        origin_device: String::new(),
        sweep_errors: 0,
        sweep_total_lost_ms: 0.0,
        sweep_main_lost_ms: 0.0,
        sweep_num_bad_ranges: 0,
        sweep_largest_gap_ms: 0.0,
        title_confident: true,
    };
    let mut cfg = Config {
        staging_dir: staging.parent().unwrap().to_string_lossy().into_owned(),
        keydb_path: Some(keydb.to_string_lossy().into_owned()),
        keyserver_url,
        ..Config::default()
    };
    let mut saved = staging::DiscState::new(staging::StagingState::Ripped);
    saved.outputs = vec![staging::Output {
        filename: marker.mkv_filename.clone(),
        title_identity: Some(crate::title_identity::TitleIdentity::of(&fx.disc.titles[0])),
        ..Default::default()
    }];
    staging::write_state(&staging, &saved);
    setup(&staging, &mut marker, &mut cfg);
    crate::server::muxer::write_marker(&staging, &marker).unwrap();
    let cfg = Arc::new(RwLock::new(cfg));

    let outcome = remux_from_ripped_marker(&cfg, &staging, &marker);
    (staging, outcome, t, iso, mapfile, before)
}

// An unreadable staged image is a failure the worker can name: the hand-off reports
// its reason (the System card), not a bare "did not complete".
#[test]
fn unproved_identity_holds_before_partial_cleanup_or_mux() {
    for missing in [true, false] {
        let (staging, outcome, _t, iso, mapfile, before) =
            resume_ripped(Keys::UnitKey, |dir, _, _| {
                staging::mutate_state_if_present(dir, |state| {
                    state.outputs[0].title_identity = if missing {
                        None
                    } else {
                        let mut different = bd_image().disc.titles[0].clone();
                        different.playlist_id += 1;
                        Some(crate::title_identity::TitleIdentity::of(&different))
                    };
                });
                std::fs::write(dir.join("KU_Disc.mkv"), b"keep existing output").unwrap();
            });
        assert!(!outcome.success);
        assert!(
            outcome
                .failure_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("identity"))
        );
        assert_eq!(
            std::fs::read(staging.join("KU_Disc.mkv")).unwrap(),
            b"keep existing output"
        );
        assert_eq!(
            (std::fs::read(iso).unwrap(), std::fs::read(mapfile).unwrap()),
            before
        );
    }
}

#[test]
fn legacy_identity_review_confirmation_replans_saved_iso_without_disc() {
    let (dir, held, temp, iso, mapfile, before) = resume_ripped(Keys::UnitKey, |dir, _, _| {
        staging::mutate_state_if_present(dir, |state| {
            state.outputs[0].title_identity = None;
            state.user_metadata = Some(staging::UserMetadata {
                title: "KU_Disc".into(),
                year: 0,
                media_type: "movie".into(),
                tmdb_id: 0,
                episode_start: None,
                poster_url: String::new(),
                overview: String::new(),
            });
        });
    });
    assert!(!held.success);
    let _guard = crate::server::log::env_guard();
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: serialized with the other environment-dependent capture fixtures.
    unsafe {
        std::env::set_var("AUTORIP_DIR", temp.path());
    }
    let state = staging::read_state(&dir).unwrap();
    assert_eq!(state.state, staging::StagingState::Review);
    assert!(state.title_identity_review);
    assert!(
        held.failure_reason
            .as_deref()
            .unwrap()
            .contains("Review queue")
    );
    // Replaying an older worker snapshot or correction cannot approve this hold.
    let mut stale = state.clone();
    stale.state = staging::StagingState::Ripped;
    stale.title_identity_review = false;
    staging::try_write_state(&dir, &stale).unwrap();
    assert_eq!(
        staging::read_state(&dir).unwrap().state,
        staging::StagingState::Review
    );
    staging::save_review_metadata(
        &dir,
        staging::UserMetadata {
            title: "KU_Disc".into(),
            year: 0,
            media_type: "movie".into(),
            tmdb_id: 0,
            episode_start: None,
            poster_url: String::new(),
            overview: String::new(),
        },
    )
    .unwrap();
    let corrected = staging::read_state(&dir).unwrap();
    assert!(corrected.replan_required);
    assert!(!corrected.title_identity_review);
    assert_eq!(corrected.state, staging::StagingState::Ripped);
    let marker = corrected.to_ripped_marker();
    let cfg = Arc::new(RwLock::new(Config {
        staging_dir: dir.parent().unwrap().to_string_lossy().into_owned(),
        keep_iso: true,
        keydb_path: Some(temp.path().join("keydb.cfg").to_string_lossy().into_owned()),
        ..Default::default()
    }));
    let result = remux_from_ripped_marker(&cfg, &dir, &marker);
    assert!(result.success, "{:?}", result.failure_reason);
    assert!(
        staging::read_state(&dir)
            .unwrap()
            .outputs
            .iter()
            .all(|output| output.title_identity.is_some())
    );
    assert_eq!(
        (std::fs::read(iso).unwrap(), std::fs::read(mapfile).unwrap()),
        before
    );
}

#[test]
fn an_unreadable_staged_image_reports_why() {
    let (_staging, outcome, ..) = resume_ripped(Keys::None, |staging, _, _| {
        let iso = staging.join("KU_Disc.iso");
        let len = std::fs::metadata(&iso).unwrap().len() as usize;
        std::fs::write(&iso, vec![0u8; len]).unwrap();
    });
    assert!(!outcome.success);
    let reason = outcome
        .failure_reason
        .expect("the failure must carry a reason");
    assert!(reason.contains("disc image"), "{reason}");
}

// A `.ripped` hand-off carries the raw title: the movie output is named in the sanitized
// form `rip_disc` writes, inside staging even for a title with path characters.
#[test]
fn a_resumed_movie_output_is_named_inside_staging() {
    let (staging, outcome, _t, ..) = resume_ripped(Keys::UnitKey, |_, m, _| {
        m.display_name = "../KU Disc".into();
    });
    assert!(outcome.success, "{:?}", outcome.failure_reason);
    let leaf = format!(
        "{}.mkv",
        crate::server::util::sanitize_path_compact("../KU Disc")
    );
    assert!(staging.join(&leaf).exists(), "{leaf}");
    assert!(!staging.parent().unwrap().join("KU Disc.mkv").exists());
}

// A saved plan whose output name leaves staging is refused, never written or deleted.
#[test]
fn a_plan_output_outside_staging_is_refused() {
    let (staging, outcome, _t, ..) = resume_ripped(Keys::UnitKey, |staging, _, _| {
        let mut st = staging::DiscState::new(staging::StagingState::Ripped);
        st.outputs = vec![
            staging::Output {
                filename: "KU_Disc_S01E01.mkv".into(),
                ..Default::default()
            },
            staging::Output {
                filename: "../escape.mkv".into(),
                ..Default::default()
            },
        ];
        staging::write_state(staging, &st);
    });
    assert!(!outcome.success);
    assert!(!staging.parent().unwrap().join("escape.mkv").exists());
}

// The sweep loss gate scopes loss as a fresh rip does: an unreadable sector outside the
// title is no loss for a muxed MKV, but an ISO output is whole-disc and must be complete.
#[test]
fn the_resume_loss_gate_scopes_out_of_title_loss_by_output() {
    let unreadable_sector_0 = |staging: &std::path::Path| {
        let path = freemkv_engine::mapfile_path_for(&staging.join("KU_Disc.iso"));
        let mut map = freemkv_engine::Mapfile::load(&path).unwrap();
        map.record(0, 2048, freemkv_engine::SectorStatus::Unreadable)
            .unwrap();
        map.flush().unwrap();
    };
    let (_, mkv, ..) = resume_ripped(Keys::UnitKey, |staging, _, _| unreadable_sector_0(staging));
    assert!(mkv.success, "{:?}", mkv.failure_reason);
    let (staging, iso, _t, ..) = resume_ripped(Keys::UnitKey, |staging, _, cfg| {
        unreadable_sector_0(staging);
        cfg.output_format = "iso".into();
    });
    assert!(!iso.success, "an ISO output with a hole is not delivered");
    assert!(
        staging::read_aborted_loss(&staging).is_some(),
        "{:?}",
        iso.failure_reason
    );
}

// A resume that settles the dir without delivering (here an ISO output with a hole,
// held `.aborted-loss`) forgets the rip's held keys too; a retryable one keeps them.
#[test]
fn a_settled_failed_resume_forgets_the_held_rip_keys() {
    let hold = |staging: &std::path::Path| {
        crate::server::keysource::hold_rip_keys(
            &staging.join("KU_Disc.iso"),
            libfreemkv::keys::KeyRing::none(),
        );
    };
    let (staging, outcome, _t, ..) = resume_ripped(Keys::UnitKey, |staging, _, cfg| {
        hold(staging);
        let path = freemkv_engine::mapfile_path_for(&staging.join("KU_Disc.iso"));
        let mut map = freemkv_engine::Mapfile::load(&path).unwrap();
        map.record(0, 2048, freemkv_engine::SectorStatus::Unreadable)
            .unwrap();
        map.flush().unwrap();
        cfg.output_format = "iso".into();
    });
    assert!(!outcome.success);
    let iso = staging.join("KU_Disc.iso");
    assert!(
        crate::server::keysource::rip_keys_for(&iso).is_none(),
        "the keys of a loss-aborted rip must not outlive it"
    );

    // Retryable (no key source holds a key): the set stays for the next attempt.
    let (staging, outcome, _t, ..) = resume_ripped(Keys::None, |staging, _, _| {
        std::fs::write(staging.join("KU_Disc.iso"), b"").unwrap();
        hold(staging);
    });
    let iso = staging.join("KU_Disc.iso");
    assert!(!outcome.success);
    assert!(crate::server::keysource::rip_keys_for(&iso).is_some());
    crate::server::keysource::forget_rip_keys(&iso);
}

// With the unit key in the keydb the worker resume delivers, and the real hand-off leaves
// the dir in the Move queue only: completed, never (queued) or re-dispatched for mux.
#[test]
fn a_resume_with_the_unit_key_delivers_to_the_move_queue_only() {
    let (staging, outcome, _t, ..) = resume_ripped(Keys::UnitKey, |_, _, _| {});
    assert!(outcome.success, "{:?}", outcome.failure_reason);
    let snap = staging::snapshot_staging_disc(&staging).unwrap();
    assert!(snap.completed && snap.has_done && !snap.has_ripped);
    assert_eq!(
        crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
        crate::server::muxer::MuxVerdict::SkipTerminal
    );
    assert!(crate::server::muxer::pending_queue(staging.parent().unwrap()).is_empty());
}
