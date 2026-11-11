//! A multipass capture records its plan at capture start, so a remux after a
//! loss-abort (Accept damage) or a Stop before the mux binds its outputs.
use super::*;

fn scratch(tag: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!("capture-plan-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn multipass() -> Config {
    Config {
        max_retries: 2,
        ..Default::default()
    }
}

#[test]
fn accept_damage_after_a_loss_abort_remuxes_the_planned_title() {
    let dir = scratch("accept");
    let titles = crate::selection_test_fixtures::variant_titles();
    let plan = super::super::capture_plan(
        &multipass(),
        &titles,
        &super::super::test_request(MediaKind::Movie, "MOVIE", 0, None),
        "Movie.mkv",
    );
    assert!(plan.as_ref().is_some_and(|p| p.outputs.is_ok()));
    staging::start_sweep(&dir, |s| {
        staging::stamp_capture_plan(s, plan.as_ref().map(|p| &p.outputs));
    })
    .unwrap();
    // The sweep loss-aborts; the operator accepts the damage.
    assert!(staging::mark_aborted_on_loss_reporting_landed(
        &dir,
        "aborted: 2 s lost"
    ));
    staging::mutate_state(
        &dir,
        staging::StagingState::Ripped,
        staging::apply_accept_loss_reopen,
    )
    .unwrap();
    let st = staging::read_state(&dir).unwrap();
    assert_eq!(st.state, staging::StagingState::Ripped);
    let bound = rebind_resume_outputs(&titles, &st.outputs).expect("remux binds the plan");
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].filename, "Movie.mkv");
}

#[test]
fn a_single_pass_rip_records_no_plan_at_capture_start() {
    let titles = crate::selection_test_fixtures::variant_titles();
    let cfg = Config {
        max_retries: 0,
        ..Default::default()
    };
    assert!(
        super::super::capture_plan(
            &cfg,
            &titles,
            &super::super::test_request(MediaKind::Movie, "MOVIE", 0, None),
            "Movie.mkv"
        )
        .is_none()
    );
}

#[test]
fn a_new_capture_plan_returns_the_superseded_files() {
    let mut st = staging::DiscState::new(staging::StagingState::Stopped);
    st.outputs = vec![staging::Output {
        filename: "Dune.mkv".into(),
        ..Default::default()
    }];
    let next = Some(Ok(vec![staging::Output {
        filename: "Dune Part Two.mkv".into(),
        ..Default::default()
    }]));
    assert_eq!(
        staging::stamp_capture_plan(&mut st, next.as_ref()),
        ["Dune.mkv"]
    );
    assert_eq!(st.outputs[0].filename, "Dune Part Two.mkv");
}

#[test]
fn a_recapture_drops_an_older_titles_partial_but_never_the_capture() {
    let dir = scratch("partials");
    for leaf in [
        "Dune.mkv",
        "Dune.iso",
        "Dune.iso.mapfile",
        "Dune Part Two.mkv",
        "notes.txt",
    ] {
        std::fs::write(dir.join(leaf), b"x").unwrap();
    }
    let gone = staging::superseded_partials(
        &dir,
        &[],
        &["Dune Part Two.mkv".into()],
        &["Dune.iso", "Dune.iso.mapfile"],
    );
    assert_eq!(gone, vec![dir.join("Dune.mkv")]);
    std::fs::write(dir.join(staging::DELIVERY_JOURNAL), b"{}").unwrap();
    assert!(
        staging::superseded_partials(&dir, &["Dune.mkv".into()], &[], &[]).is_empty(),
        "a delivery owns the job's files"
    );
}

// A held capture swept only part of the disc (Keep ISO off): its episode suggestion offers
// only the titles the sweep read; anything else needs the disc.
#[test]
fn a_held_captures_suggestion_is_limited_to_its_sweep() {
    use freemkv_engine::{Mapfile, SectorStatus, mapfile_path_for};
    let dir = scratch("swept");
    let fx = crate::ku_fixtures::bd_image(&[None, None, None], 1);
    let iso = fx.write(&dir, "Show.iso");
    let len = fx.img.image.len() as u64;
    let (start, sectors) = fx.clip(2);
    let (gap_start, gap_end) = (u64::from(start) * 2048, u64::from(start + sectors) * 2048);
    let map_path = mapfile_path_for(&iso);
    let mut map = Mapfile::create(&map_path, len, "t").unwrap();
    map.record(0, gap_start, SectorStatus::Finished).unwrap();
    map.record(gap_end, len - gap_end, SectorStatus::Finished)
        .unwrap();
    map.flush().unwrap();
    let confirm = crate::server::planner::EpisodeConfirm {
        reason: freemkv_engine::SelectionReviewReason::MissingEpisodeRoster,
        suggested: vec![0, 1, 2],
        clusters: vec![vec![0, 1, 2]],
        alternates: Vec::new(),
    };
    let held = swept_confirm(&fx.disc, confirm.clone(), &iso, &map_path);
    assert_eq!(held.suggested, [0, 1]);
    assert_eq!(held.clusters, [vec![0, 1]]);
    // A whole-disc capture offers everything.
    crate::ku_fixtures::sidecar(&fx, &iso, false);
    assert_eq!(
        swept_confirm(&fx.disc, confirm.clone(), &iso, &map_path),
        confirm
    );
}

// Resume rebuilds a held plan from the operator's pick, remapped by identity onto the
// rescan of the saved image: exactly the picked titles, in the picked order.
#[test]
fn resume_remaps_the_operator_pick() {
    let titles = crate::selection_test_fixtures::separate_titles();
    assert!(titles.len() >= 2);
    let mut st = staging::DiscState::new(staging::StagingState::Ripped);
    st.disc_label = "SHOW_S1_D1".into();
    st.title = "Show".into();
    st.title_pick = Some(vec![
        crate::title_identity::TitleIdentity::of(&titles[1]),
        crate::title_identity::TitleIdentity::of(&titles[0]),
    ]);
    let mut rescan = titles.clone();
    rescan.swap(0, 1);
    let request = resume_request(Some(&st), &rescan, MediaKind::Movie, "");
    assert_eq!(request.media.kind, Some(MediaKind::Tv));
    assert_eq!(request.pick, Some(vec![0, 1]));
    assert!(check_resume_selection(&rescan, &multipass(), &request, false, true).is_ok());
    let plan = super::super::plan_mux_outputs(&rescan, &multipass(), &request, "Show.mkv").unwrap();
    assert_eq!(
        plan.iter()
            .map(|o| (o.title_index, o.episode, o.title_identity.clone()))
            .collect::<Vec<_>>(),
        [
            (0, Some(1), st.title_pick.as_ref().map(|p| p[0].clone())),
            (1, Some(2), st.title_pick.as_ref().map(|p| p[1].clone())),
        ]
    );
    // A pick the image no longer has holds; it never falls back to an automatic choice.
    let request = resume_request(Some(&st), &rescan[..1], MediaKind::Movie, "");
    assert_eq!(
        check_resume_selection(&rescan[..1], &multipass(), &request, false, true),
        Err(crate::server::planner::PlanError::PickMismatch)
    );
}

// A resume that holds for its episodes keeps the suggestion, limited to what the capture
// swept, with the job and on the card; a stale pick is cleared so the next resume selects again.
#[test]
fn a_held_resume_keeps_its_swept_suggestion_and_clears_a_stale_pick() {
    use crate::server::planner::{EpisodeConfirm, PlanError};
    use freemkv_engine::{Mapfile, SectorStatus, mapfile_path_for};
    let dir = scratch("held-resume");
    let fx = crate::ku_fixtures::bd_image(&[None, None, None], 1);
    let iso = fx.write(&dir, "Show.iso");
    let len = fx.img.image.len() as u64;
    let (start, sectors) = fx.clip(2);
    let (gap_start, gap_end) = (u64::from(start) * 2048, u64::from(start + sectors) * 2048);
    let map_path = mapfile_path_for(&iso);
    let mut map = Mapfile::create(&map_path, len, "t").unwrap();
    map.record(0, gap_start, SectorStatus::Finished).unwrap();
    map.record(gap_end, len - gap_end, SectorStatus::Finished)
        .unwrap();
    map.flush().unwrap();
    let mut st = staging::DiscState::new(staging::StagingState::Ripped);
    st.title = "Show".into();
    st.iso_path = "Show.iso".into();
    st.title_pick = Some(vec![crate::title_identity::TitleIdentity::of(
        &fx.disc.titles[0],
    )]);
    staging::try_write_state(&dir, &st).unwrap();
    let device = "a_held_resume_keeps_its_swept_suggestion";
    super::super::update_state(
        device,
        super::super::RipState {
            device: device.into(),
            ..Default::default()
        },
    );
    let confirm = EpisodeConfirm {
        reason: freemkv_engine::SelectionReviewReason::MissingEpisodeRoster,
        suggested: vec![0, 1, 2],
        clusters: vec![vec![0, 1, 2]],
        alternates: Vec::new(),
    };
    let paths = (dir.as_path(), iso.as_path(), map_path.as_path());
    let selection = PlanError::Selection(confirm);
    hold_resume_selection(device, paths, "Show", (&fx.disc, &selection));
    let held = staging::read_state(&dir).unwrap();
    assert_eq!(held.state, staging::StagingState::Review);
    let swept = held
        .episode_confirm
        .expect("the suggestion is kept with the job");
    assert_eq!(
        (swept.suggested, swept.clusters),
        (vec![0, 1], vec![vec![0, 1]])
    );
    assert!(
        held.title_pick.is_some(),
        "a hold to confirm keeps the pick"
    );
    let row = super::super::STATE
        .lock()
        .unwrap()
        .get(device)
        .cloned()
        .unwrap();
    assert_eq!(row.plan.confirm.map(|c| c.suggested), Some(vec![0, 1]));

    hold_resume_selection(device, paths, "Show", (&fx.disc, &PlanError::PickMismatch));
    let held = staging::read_state(&dir).unwrap();
    assert!(held.title_pick.is_none(), "a stale pick is cleared");
    assert!(held.episode_confirm.is_none());
    let row = super::super::STATE.lock().unwrap().remove(device).unwrap();
    assert!(
        row.last_error.contains("not on this disc"),
        "{}",
        row.last_error
    );
    let _ = std::fs::remove_dir_all(&dir);
}
