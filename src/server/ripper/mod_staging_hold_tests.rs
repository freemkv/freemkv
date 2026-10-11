use super::*;
use crate::server::web::ResumeMode;

const PRESENT: libfreemkv::DiscPresence = libfreemkv::DiscPresence::Present;

fn missing_root(base: &std::path::Path) -> (std::path::PathBuf, Arc<RwLock<Config>>) {
    let root = base.join("staging");
    let cfg = Config {
        staging_dir: root.to_string_lossy().into_owned(),
        ..Config::default()
    };
    (root, Arc::new(RwLock::new(cfg)))
}

// A scanned disc in "scanning", as a claim leaves it before the post-scan dispatch.
fn seed_scanned(device: &str) {
    STATE.lock().unwrap().insert(
        device.to_string(),
        RipState {
            device: device.to_string(),
            status: "scanning".into(),
            disc_name: "Held Disc".into(),
            job_id: "Held_Disc".into(),
            ..Default::default()
        },
    );
}

fn forget(device: &str) {
    STATE.lock().unwrap().remove(device);
    STAGING_HELD.lock().unwrap().remove(device);
}

// The poll tick's view of a resident disc: whether it dispatches an insert.
fn dispatches(device: &str, cfg: &Arc<RwLock<Config>>, now: u64) -> bool {
    match presence_tick(device, PRESENT, true, cfg, now, false) {
        (_, PollAction::Present(tick)) => tick.dispatch,
        _ => false,
    }
}

// An inserted disc's rip waits for a missing staging folder, says why, and the poll loop
// replays it as an insert once the folder is there, once.
#[test]
fn a_held_insert_is_replayed_once_when_the_staging_folder_appears() {
    let base = tempfile::tempdir().unwrap();
    let (root, cfg) = missing_root(base.path());
    let device = "_staging_hold_insert_test";
    seed_scanned(device);
    dispatch_rip_request(&cfg, device, "/nonexistent/hold-drive", ResumeMode::Fresh);
    let row = STATE.lock().unwrap().get(device).cloned().unwrap();
    assert_eq!(row.status, "error");
    assert!(
        row.last_error
            .contains("is missing — is the share mounted?"),
        "{}",
        row.last_error
    );
    assert!(staging_held(device), "an insert's rip waits");
    let held_at = *STAGING_HELD.lock().unwrap().get(device).unwrap();
    let later = held_at + STAGING_HOLD_RECHECK_SECS;
    assert!(!dispatches(device, &cfg, later), "still missing");
    std::fs::create_dir(&root).unwrap();
    assert!(
        !dispatches(device, &cfg, later + 1),
        "looked at most every few seconds"
    );
    assert!(dispatches(device, &cfg, later + STAGING_HOLD_RECHECK_SECS));
    assert!(
        !dispatches(device, &cfg, later + 10 * STAGING_HOLD_RECHECK_SECS),
        "replayed once"
    );
    forget(device);
}

// An operator's Rip, Resume or Start over with the staging folder missing is refused with
// the reason; nothing waits, so the folder appearing later starts no rip of any kind.
#[test]
fn an_operator_rip_with_the_staging_folder_missing_is_refused_not_held() {
    for mode in [ResumeMode::Default, ResumeMode::Require, ResumeMode::Wipe] {
        let base = tempfile::tempdir().unwrap();
        let (root, cfg) = missing_root(base.path());
        cfg.write().unwrap().on_insert = "rip".into();
        let device = "_staging_hold_operator_test";
        seed_scanned(device);
        dispatch_rip_request(&cfg, device, "/nonexistent/hold-drive", mode);
        let row = STATE.lock().unwrap().get(device).cloned().unwrap();
        assert_eq!(row.status, "error", "{mode:?}");
        assert!(
            row.last_error
                .contains("is missing — is the share mounted?"),
            "{mode:?}: {}",
            row.last_error
        );
        assert!(!staging_held(device), "{mode:?} must not be held");
        assert!(!root.exists(), "{mode:?} made the staging folder");
        std::fs::create_dir(&root).unwrap();
        assert!(
            !dispatches(device, &cfg, u64::MAX / 2),
            "{mode:?} came back as an insert"
        );
        forget(device);
    }
}

// A rip the operator starts while an insert is held replaces the hold: once that rip is
// done, the folder being there re-dispatches nothing.
#[test]
fn a_manual_rip_clears_a_held_insert() {
    let base = tempfile::tempdir().unwrap();
    let (root, cfg) = missing_root(base.path());
    let device = "_staging_hold_manual_test";
    seed_scanned(device);
    STATE.lock().unwrap().get_mut(device).unwrap().status = "idle".into();
    let problem = crate::server::config::staging_root_ready(&cfg.read().unwrap()).unwrap_err();
    hold_for_staging(device, &problem, 100, true);
    std::fs::create_dir(&root).unwrap();
    assert!(try_claim_active_checked(device, true).is_some());
    update_state_with(device, |s| s.status = "done".into());
    assert!(!staging_held(device));
    assert!(!dispatches(
        device,
        &cfg,
        100 + 10 * STAGING_HOLD_RECHECK_SECS
    ));
    forget(device);
}

// A staging path that is a file names the path and the cause, not a generic English line.
#[test]
fn a_staging_fault_other_than_missing_names_the_path_and_the_cause() {
    crate::strings::set_locale("en");
    let base = tempfile::tempdir().unwrap();
    let file = base.path().join("staging");
    std::fs::write(&file, b"").unwrap();
    let cfg = Config {
        staging_dir: file.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let device = "_staging_hold_fault_test";
    let problem = crate::server::config::staging_root_ready(&cfg).unwrap_err();
    hold_for_staging(device, &problem, 100, false);
    let row = STATE.lock().unwrap().get(device).cloned().unwrap();
    let path = file.display().to_string();
    assert!(
        row.last_error.contains(&path) && row.last_error.contains("not a folder"),
        "{}",
        row.last_error
    );
    assert_eq!(staging_problem_text(&problem), row.last_error);
    forget(device);
}

#[test]
fn a_device_never_held_is_not_restarted() {
    let cfg = Config::default();
    assert!(!staging_hold_lifted("_staging_never_held", &cfg, 1_000));
}
