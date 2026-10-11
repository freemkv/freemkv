//! A replan plans from the raw volume label the capture recorded, and a new
//! capture or hand-off starts without an earlier attempt's hold reason.
use super::*;

fn scratch(tag: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!("planning-label-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn a_capture_records_the_label_its_replan_reads() {
    let mut st = staging::DiscState::new(staging::StagingState::Sweeping);
    staging::stamp_capture_label(&mut st, "SHOW_S2_D2");
    assert_eq!(planning_label(&st), "SHOW_S2_D2");
    assert_eq!(
        crate::server::tmdb::season_from_label(planning_label(&st)),
        Some(2)
    );
    assert_eq!(
        crate::server::tmdb::disc_from_label(planning_label(&st)),
        Some(2)
    );
}

// Jobs captured before the label was recorded carry it only in disc_name.
#[test]
fn an_earlier_capture_plans_from_its_disc_name() {
    let mut st = staging::DiscState::new(staging::StagingState::Review);
    st.disc_name = "SHOW_S2_D2".into();
    assert_eq!(planning_label(&st), "SHOW_S2_D2");
}

#[test]
fn a_new_sweep_and_a_hand_off_drop_a_stale_hold_reason() {
    let dir = scratch("reason");
    let mut st = staging::DiscState::new(staging::StagingState::AbortedLoss);
    st.failure_reason = Some("aborted: 3.1 s lost in main movie".into());
    staging::try_write_state(&dir, &st).unwrap();
    staging::start_sweep(&dir, |_| {}).unwrap();
    assert_eq!(staging::read_state(&dir).unwrap().failure_reason, None);

    staging::mutate_state(&dir, staging::StagingState::Ripped, |s| {
        s.failure_reason = Some("stale".into());
    })
    .unwrap();
    staging::mark_handoff(&dir, false, |_| {}).unwrap();
    let st = staging::read_state(&dir).unwrap();
    assert_eq!(st.state, staging::StagingState::Review);
    assert_eq!(st.failure_reason, None, "a title hold shows its own reason");
}
