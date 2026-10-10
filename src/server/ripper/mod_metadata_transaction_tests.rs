use super::*;

fn metadata(title: &str, media_type: &str) -> staging::UserMetadata {
    staging::UserMetadata {
        episode_start: None,
        title: title.into(),
        year: 2020,
        media_type: media_type.into(),
        tmdb_id: 10,
        poster_url: String::new(),
        overview: String::new(),
    }
}

fn review_state() -> staging::DiscState {
    let mut state = staging::DiscState::new(staging::StagingState::Review);
    state.media_type = "movie".into();
    state.outputs.push(staging::Output {
        filename: "movie.mkv".into(),
        ..Default::default()
    });
    state
}

fn retitle_fixture(
    device: &str,
    state: &staging::DiscState,
    selection: staging::UserMetadata,
) -> (Result<u64, String>, staging::DiscState, Vec<u8>, Vec<u8>) {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("job");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(staging::STATE_FILE);
    let before = serde_json::to_vec(state).unwrap();
    std::fs::write(&path, &before).unwrap();
    let cfg = Arc::new(RwLock::new(Config {
        staging_dir: temp.path().to_string_lossy().into_owned(),
        ..Default::default()
    }));
    update_state(
        device,
        RipState {
            job_id: "job".into(),
            disc_present: true,
            disc_identity: "identity".into(),
            disc_label: "label".into(),
            ..Default::default()
        },
    );
    let result = retitle_staging_for_device(&cfg, device, "job", selection);
    STATE.lock().unwrap().remove(device);
    let after = std::fs::read(&path).unwrap();
    (result, staging::read_state(&dir).unwrap(), before, after)
}

#[test]
fn review_correction_atomically_releases_current_metadata() {
    let (result, saved, _, _) = retitle_fixture(
        "review_atomic_release",
        &review_state(),
        metadata("Accepted", "movie"),
    );
    assert_eq!(result.unwrap(), 1);
    assert_eq!(saved.state, staging::StagingState::Done);
    assert_eq!(saved.title, "Accepted");
}

#[test]
fn review_unknown_type_movie_correction_does_not_require_missing_iso() {
    let mut state = review_state();
    state.media_type.clear();
    let (result, saved, _, _) = retitle_fixture(
        "review_atomic_unknown_movie",
        &state,
        metadata("Accepted", "movie"),
    );
    assert!(result.is_ok());
    assert_eq!(saved.state, staging::StagingState::Done);
    assert!(!saved.replan_required);
    assert!(saved.iso_path.is_empty());
}

#[test]
fn repeated_review_held_correction_never_promotes_previous_selection() {
    let mut state = review_state();
    state.user_metadata = Some(metadata("Previous", "movie"));
    state.metadata_revision = 1;
    let (result, saved, _, _) =
        retitle_fixture("review_atomic_repeat", &state, metadata("Latest", "movie"));
    assert_eq!(result.unwrap(), 2);
    assert_eq!(saved.state, staging::StagingState::Done);
    assert_eq!(saved.title, "Latest");
}

#[test]
fn review_plan_correction_atomically_requests_replan() {
    let mut state = review_state();
    state.max_retries = 3;
    state.iso_path = "capture.iso".into();
    let (result, saved, _, _) =
        retitle_fixture("review_atomic_replan", &state, metadata("Show", "tv"));
    assert!(result.is_ok());
    assert_eq!(saved.state, staging::StagingState::Ripped);
    assert!(saved.replan_required);
    assert_eq!(saved.title, "Show");
}

#[test]
fn rejected_review_correction_is_byte_for_byte_unchanged() {
    let mut state = review_state();
    state.user_metadata = Some(metadata("Previous", "movie"));
    state.metadata_revision = 1;
    let (result, _, before, after) =
        retitle_fixture("review_atomic_refusal", &state, metadata("Show", "tv"));
    assert!(result.is_err());
    assert_eq!(before, after);
}

#[test]
fn interrupted_reset_refuses_metadata_without_mutating_state() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = staging::DiscState::new(staging::StagingState::Stopped);
    state.reset_in_progress = true;
    let path = temp.path().join(staging::STATE_FILE);
    let before = serde_json::to_vec(&state).unwrap();
    std::fs::write(&path, &before).unwrap();
    assert!(staging::save_user_metadata(temp.path(), metadata("No", "movie")).is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn interrupted_reset_refuses_resume_until_reset_completes() {
    let mut state = staging::DiscState::new(staging::StagingState::Stopped);
    state.reset_in_progress = true;
    assert!(resume::check_resume_state(&state).is_err());
    state.reset_in_progress = false;
    assert!(resume::check_resume_state(&state).is_ok());
}

#[test]
fn held_planless_iso_correction_queues_disc_free_replan_without_touching_media() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = staging::DiscState::new(staging::StagingState::Review);
    state.title = "Show Season 1 Disc 2".into();
    state.media_type = "tv".into();
    state.disc_identity = "original-identity".into();
    state.iso_path = temp.path().join("disc.iso").to_string_lossy().into_owned();
    state.needs_disc = true;
    std::fs::write(&state.iso_path, b"retained capture").unwrap();
    std::fs::write(
        temp.path().join(staging::STATE_FILE),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let mut selection = metadata(&state.title, "tv");
    selection.episode_start = Some(4);
    staging::save_review_metadata(temp.path(), selection).unwrap();
    let saved = staging::read_state(temp.path()).unwrap();
    assert_eq!(saved.state, staging::StagingState::Ripped);
    assert!(saved.replan_required);
    assert!(!saved.needs_disc);
    assert_eq!(saved.episode_start, Some(4));
    assert_eq!(saved.disc_identity, "original-identity");
    assert_eq!(std::fs::read(&state.iso_path).unwrap(), b"retained capture");
}

#[test]
fn numbering_only_correction_replans_and_movie_clears_numbering() {
    let mut state = review_state();
    state.media_type = "tv".into();
    state.iso_path = "capture.iso".into();
    let mut selection = metadata("Show", "tv");
    selection.episode_start = Some(4);
    state.title = selection.title.clone();
    state.tmdb_id = selection.tmdb_id;
    state.episode_start = selection.episode_start;
    selection.episode_start = Some(7);
    let (result, saved, _, _) = retitle_fixture("numbering_only", &state, selection);
    assert!(result.is_ok());
    assert!(saved.replan_required);
    assert_eq!(saved.episode_start, Some(7));
    let mut movie = metadata("Movie", "movie");
    movie.episode_start = Some(7);
    let (result, saved, _, _) = retitle_fixture("movie_numbering_clear", &state, movie);
    assert!(result.is_ok());
    assert_eq!(saved.episode_start, None);
}
