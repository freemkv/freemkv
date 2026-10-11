//! A replan or selection refusal holds the job in Review instead of leaving it
//! Ripped for the mux worker to re-dispatch every tick.
use super::*;

fn scratch(tag: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!("plan-hold-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn corrected_job(dir: &Path) -> (PathBuf, PathBuf) {
    let iso = dir.join("Show.iso");
    let map = dir.join("Show.iso.mapfile");
    std::fs::write(&iso, b"image").unwrap();
    std::fs::write(&map, b"map").unwrap();
    let mut st = staging::DiscState::new(staging::StagingState::Ripped);
    st.replan_required = true;
    st.iso_path = iso.to_string_lossy().into_owned();
    st.mapfile_path = map.to_string_lossy().into_owned();
    st.outputs = vec![staging::Output {
        filename: "Show.mkv".into(),
        ..Default::default()
    }];
    staging::try_write_state(dir, &st).unwrap();
    (iso, map)
}

#[test]
fn a_refused_replan_is_held_for_review_and_not_redispatched() {
    let dir = scratch("refused");
    let (iso, map) = corrected_job(&dir);
    let reason =
        super::super::selection_review_message(&crate::server::planner::PlanError::EpisodeRange);
    hold_plan_for_review("_mux", (&dir, &iso, &map), "Show", reason.clone());

    let st = staging::read_state(&dir).unwrap();
    assert_eq!(st.state, staging::StagingState::Review);
    assert_eq!(st.failure_reason.as_deref(), Some(reason.as_str()));
    assert_eq!(st.outputs.len(), 1, "the saved plan is kept");
    assert!(iso.exists() && map.exists(), "the capture is kept");
    // The next mux-worker tick must not dispatch it again.
    let snap = staging::snapshot_staging_disc(&dir);
    assert!(!matches!(
        crate::server::muxer::mux_dispatch_verdict(snap.as_ref()),
        crate::server::muxer::MuxVerdict::Dispatch
    ));
    // It is listed in Review, where the operator corrects it.
    let held = crate::server::review::list_held(&dir.parent().unwrap().to_string_lossy());
    assert!(held.iter().any(|h| h.reason == reason));
}

#[test]
fn an_operator_correction_of_the_hold_replans_once() {
    let dir = scratch("corrected");
    let (iso, map) = corrected_job(&dir);
    hold_plan_for_review("_mux", (&dir, &iso, &map), "Show", "held".into());
    let metadata = staging::UserMetadata {
        title: "Show".into(),
        year: 2001,
        media_type: crate::server::planner::MediaKind::Tv,
        episode_start: Some(5),
        tmdb_id: 7,
        poster_url: String::new(),
        overview: String::new(),
    };
    staging::save_review_metadata(&dir, metadata).unwrap();
    let st = staging::read_state(&dir).unwrap();
    assert_eq!(st.state, staging::StagingState::Ripped);
    assert!(st.replan_required);
}
