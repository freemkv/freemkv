//! A finished capture whose plan the operator must settle is held in Review with
//! its keys kept; a hold that cannot be written is surfaced, never swallowed.
use super::*;

fn scratch(tag: &str) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!("capture-hold-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn a_plan_hold_keeps_the_rip_keys_for_the_corrected_remux() {
    let dir = scratch("keys");
    staging::try_write_state(
        &dir,
        &staging::DiscState::new(staging::StagingState::Sweeping),
    )
    .unwrap();
    let iso = dir.join("Show.iso");
    let map = dir.join("Show.iso.mapfile");
    hold_capture_for_review(
        "dev-hold",
        &dir,
        (&iso.to_string_lossy(), &map.to_string_lossy()),
        Some(&libfreemkv::keys::KeyRing::none()),
        "held",
    );
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove("dev-hold");
    let st = staging::read_state(&dir).unwrap();
    assert_eq!(st.state, staging::StagingState::Review);
    assert_eq!(st.failure_reason.as_deref(), Some("held"));
    assert!(crate::server::keysource::rip_keys_for(&iso).is_some());
    crate::server::keysource::forget_rip_keys(&iso);
}

// Cards are process-wide: a clear-all elsewhere must not run between the hold and the look.
fn card_for(dir: &std::path::Path) -> Option<crate::server::muxer::MuxerError> {
    crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(dir.to_string_lossy().as_ref())
}

#[test]
fn a_hold_that_cannot_be_written_raises_a_card() {
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = scratch("unwritable");
    std::fs::write(dir.join(staging::STATE_FILE), b"{not json").unwrap();
    hold_capture_for_review(
        "dev-hold-2",
        &dir,
        ("/x/Show.iso", "/x/Show.iso.mapfile"),
        None,
        "held",
    );
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove("dev-hold-2");
    let card = card_for(&dir).expect("no card for the lost hold");
    assert_eq!(card.reason, "held");
    assert!(card.hint.contains("could not be saved"), "{}", card.hint);
}

// The drive card says a capture's hold was not saved, as a resume's hold does.
#[test]
fn a_capture_hold_that_could_not_be_saved_says_so_on_the_drive() {
    crate::strings::set_locale("en");
    let dir = scratch("unsaved-drive").join("gone");
    let device = "_capture_hold_unsaved_test";
    hold_capture_for_review(
        device,
        &dir,
        ("/x/Show.iso", "/x/Show.iso.mapfile"),
        None,
        "needs review",
    );
    crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(dir.to_string_lossy().as_ref());
    let row = STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device)
        .unwrap();
    assert_eq!(row.status, "error");
    assert!(
        row.last_error.starts_with("needs review"),
        "{}",
        row.last_error
    );
    assert!(
        row.last_error.contains("Could not save the review hold:"),
        "{}",
        row.last_error
    );
}

// A capture held at the hand-off is surfaced as a resume's hold is: a card naming why.
#[test]
fn a_capture_held_for_review_is_surfaced_like_a_resume_hold() {
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = scratch("surfaced");
    staging::try_write_state(
        &dir,
        &staging::DiscState::new(staging::StagingState::Sweeping),
    )
    .unwrap();
    hold_capture_for_review(
        "dev-hold-3",
        &dir,
        ("/x/Show.iso", "/x/Show.iso.mapfile"),
        None,
        "held",
    );
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove("dev-hold-3");
    assert_eq!(
        staging::read_state(&dir).unwrap().state,
        staging::StagingState::Review
    );
    let card = card_for(&dir).expect("no card for the hold");
    assert_eq!(card.reason, "held");
}

// The rip-time hold as a capture leaves it: Sweeping, its failed plan's outputs cleared,
// no hand-off. A correction in Review then hands the saved image to the mux worker.
#[test]
fn a_rip_time_hold_is_dispatched_by_the_mux_worker_once_corrected() {
    let dir = scratch("rip-time");
    let iso = dir.join("Show.iso");
    let map = dir.join("Show.iso.mapfile");
    std::fs::write(&iso, b"image").unwrap();
    std::fs::write(&map, b"map").unwrap();
    staging::begin_capture(&dir, 2, "hash").unwrap();
    let failed = crate::server::planner::check_episode_range(Some(u16::MAX), 2).map(|()| vec![]);
    staging::start_sweep(&dir, |s| {
        s.outputs = vec![staging::Output {
            filename: "Old.mkv".into(),
            ..Default::default()
        }];
        staging::stamp_capture_plan(s, Some(&failed));
        s.iso_path = iso.to_string_lossy().into_owned();
        s.mapfile_path = map.to_string_lossy().into_owned();
    })
    .unwrap();
    let st = staging::read_state(&dir).unwrap();
    assert_eq!(st.state, staging::StagingState::Sweeping);
    assert!(st.outputs.is_empty());
    hold_capture_for_review(
        "dev-hold-rip-time",
        &dir,
        (&iso.to_string_lossy(), &map.to_string_lossy()),
        None,
        "held",
    );
    STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove("dev-hold-rip-time");
    let verdict = || {
        crate::server::muxer::mux_dispatch_verdict(staging::snapshot_staging_disc(&dir).as_ref())
    };
    assert_ne!(verdict(), crate::server::muxer::MuxVerdict::Dispatch);
    staging::save_review_metadata(
        &dir,
        staging::UserMetadata {
            title: "Show".into(),
            year: 2001,
            media_type: crate::server::planner::MediaKind::Movie,
            episode_start: None,
            tmdb_id: 1,
            poster_url: String::new(),
            overview: String::new(),
        },
    )
    .unwrap();
    let st = staging::read_state(&dir).unwrap();
    assert_eq!(st.state, staging::StagingState::Ripped);
    assert!(st.replan_required && !st.title_identity_review);
    assert_eq!(verdict(), crate::server::muxer::MuxVerdict::Dispatch);
    let marker = crate::server::muxer::read_marker(&dir).unwrap();
    assert_eq!(marker.iso_path, iso.to_string_lossy());
    assert_eq!(marker.mapfile_path, map.to_string_lossy());
}

// The rip's own call site: a plan that fails after the sweep holds the capture, and the drive
// keeps the hold's say (here, that it could not be saved), not the bare reason.
#[test]
fn a_failed_plan_at_the_hand_off_keeps_the_unsaved_hold_on_the_drive() {
    crate::strings::set_locale("en");
    let dir = scratch("failed-plan").join("gone");
    let device = "_capture_failed_plan_test";
    hold_failed_plan(
        device,
        &dir,
        ("/x/Show.iso", "/x/Show.iso.mapfile"),
        None,
        &crate::server::planner::PlanError::EpisodeRange,
    );
    crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(dir.to_string_lossy().as_ref());
    let row = STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device)
        .unwrap();
    assert_eq!(row.status, "error");
    assert!(
        row.last_error.contains("Could not save the review hold:"),
        "{}",
        row.last_error
    );
}
