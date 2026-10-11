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
        (MediaKind::Movie, "MOVIE", 0),
        "Movie.mkv",
        None,
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
            (MediaKind::Movie, "MOVIE", 0),
            "Movie.mkv",
            None
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
