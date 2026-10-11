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
    // The `_mux` row is shared with the lease-wait tests.
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
    // The `_mux` row is shared with the lease-wait tests.
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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

// A Ripped job whose saved state asks for a replan as TV from `label`, over the disc image
// `image`, resumed by the mux worker.
fn resume_replan(tag: &str, image: &[u8], label: &str) -> (PathBuf, staging::DiscState) {
    let dir = scratch(tag);
    let iso = dir.join("Show.iso");
    let map = dir.join("Show.iso.mapfile");
    std::fs::write(&iso, image).unwrap();
    std::fs::write(&map, b"# map").unwrap();
    let mut st = staging::DiscState::new(staging::StagingState::Ripped);
    st.replan_required = true;
    st.media_type = crate::server::planner::MediaKind::Tv;
    st.disc_label = label.into();
    st.title = "Show".into();
    st.iso_path = iso.to_string_lossy().into_owned();
    st.mapfile_path = map.to_string_lossy().into_owned();
    staging::try_write_state(&dir, &st).unwrap();
    let class = ResumeClass::Remux {
        iso_path: iso,
        mapfile_path: map,
        display_name: "Show".into(),
        title_confident: Some(true),
    };
    let device = format!("plan-hold-{tag}");
    let cfg = Arc::new(RwLock::new(Config::default()));
    resume_remux_within(&cfg, &device, class, std::time::Duration::ZERO, || ());
    super::super::STATE.lock().unwrap().remove(&device);
    crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(dir.to_string_lossy().as_ref());
    let st = staging::read_state(&dir).unwrap();
    (dir, st)
}

#[test]
fn resume_holds_a_selection_it_cannot_make_in_review() {
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Three titles and no authored episode list: TV planning cannot pick the episodes.
    let k = crate::ku_fixture::K1;
    let image = crate::ku_fixtures::bd_image(&[Some(k), Some(k), Some(k)], 1)
        .img
        .image;
    let (dir, st) = resume_replan("selection", &image, "SHOW_S1_D1");
    assert_eq!(
        st.state,
        staging::StagingState::Review,
        "{:?}",
        st.failure_reason
    );
    assert!(
        st.failure_reason
            .unwrap_or_default()
            .contains("authored episode list")
    );
    let snap = staging::snapshot_staging_disc(&dir);
    assert_ne!(
        crate::server::muxer::mux_dispatch_verdict(snap.as_ref()),
        crate::server::muxer::MuxVerdict::Dispatch
    );
}

// A hold that could not be saved says so where the operator looks: the drive status.
#[test]
fn a_review_hold_that_could_not_be_saved_says_so_on_the_drive() {
    crate::strings::set_locale("en");
    let dir = scratch("unsaved").join("gone");
    let (iso, map) = (dir.join("Show.iso"), dir.join("Show.iso.mapfile"));
    let device = "_review_hold_unsaved_test";
    hold_plan_for_review(device, (&dir, &iso, &map), "Show", "needs review".into());
    let row = super::super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device)
        .unwrap();
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

// A resume whose saved plan cannot be rebound to the image, and whose Review hold could
// not be saved, says so on the drive too.
#[cfg(unix)]
#[test]
fn a_resume_rebind_hold_that_could_not_be_saved_says_so_on_the_drive() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root writes into the read-only folder anyway
    }
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::strings::set_locale("en");
    let k = crate::ku_fixture::K1;
    let image = crate::ku_fixtures::bd_image(&[Some(k), Some(k), Some(k)], 1)
        .img
        .image;
    let dir = scratch("rebind-unsaved");
    let iso = dir.join("Show.iso");
    let map = dir.join("Show.iso.mapfile");
    std::fs::write(&iso, image).unwrap();
    std::fs::write(&map, b"# map").unwrap();
    let mut st = staging::DiscState::new(staging::StagingState::Ripped);
    st.title = "Show".into();
    st.outputs = vec![staging::Output {
        filename: "Show.mkv".into(),
        ..Default::default()
    }];
    st.iso_path = iso.to_string_lossy().into_owned();
    st.mapfile_path = map.to_string_lossy().into_owned();
    staging::try_write_state(&dir, &st).unwrap();
    // The share refuses writes: the Review hold cannot be saved.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let class = ResumeClass::Remux {
        iso_path: iso,
        mapfile_path: map,
        display_name: "Show".into(),
        title_confident: Some(true),
    };
    let device = "plan-hold-rebind-unsaved";
    let cfg = Arc::new(RwLock::new(Config::default()));
    resume_remux_within(&cfg, device, class, std::time::Duration::ZERO, || ());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(dir.to_string_lossy().as_ref());
    let row = super::super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device)
        .unwrap();
    assert!(
        row.last_error.contains("predates title identity proof"),
        "{}",
        row.last_error
    );
    assert!(
        row.last_error.contains("Could not save the review hold:"),
        "{}",
        row.last_error
    );
}
