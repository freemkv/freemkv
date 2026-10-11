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
    let st = staging::read_state(&dir).unwrap();
    assert_eq!(st.state, staging::StagingState::Review);
    assert_eq!(st.failure_reason.as_deref(), Some("held"));
    assert!(crate::server::keysource::rip_keys_for(&iso).is_some());
    crate::server::keysource::forget_rip_keys(&iso);
}

#[test]
fn a_hold_that_cannot_be_written_raises_a_card() {
    let dir = scratch("unwritable");
    std::fs::write(dir.join(staging::STATE_FILE), b"{not json").unwrap();
    hold_capture_for_review(
        "dev-hold-2",
        &dir,
        ("/x/Show.iso", "/x/Show.iso.mapfile"),
        None,
        "held",
    );
    let cards = crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let card = cards.get(dir.to_string_lossy().as_ref());
    assert!(
        card.is_some_and(|c| c.reason == "held"),
        "no card for the lost hold"
    );
}
