use super::*;

// A rip held for a missing staging folder says why, and starts again once the folder is there.
#[test]
fn a_rip_held_for_a_missing_staging_folder_starts_again_once_it_is_there() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("staging");
    let cfg = Config {
        staging_dir: root.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let device = "_staging_hold_test";
    let problem = crate::server::config::staging_root_ready(&cfg).unwrap_err();
    hold_for_staging(device, &problem, 100);
    let row = STATE.lock().unwrap().get(device).cloned().unwrap();
    assert_eq!(row.status, "error");
    assert!(
        row.last_error
            .contains("is missing — is the share mounted?"),
        "{}",
        row.last_error
    );
    assert!(!staging_hold_lifted(device, &cfg, 200), "still missing");
    std::fs::create_dir(&root).unwrap();
    assert!(
        !staging_hold_lifted(device, &cfg, 201),
        "looked at most every few seconds"
    );
    assert!(staging_hold_lifted(
        device,
        &cfg,
        200 + STAGING_HOLD_RECHECK_SECS
    ));
    assert!(!root.join("x").exists());
    assert!(!staging_hold_lifted(device, &cfg, 1_000), "lifted once");
    STATE.lock().unwrap().remove(device);
}

#[test]
fn a_device_never_held_is_not_restarted() {
    let cfg = Config::default();
    assert!(!staging_hold_lifted("_staging_never_held", &cfg, 1_000));
}
