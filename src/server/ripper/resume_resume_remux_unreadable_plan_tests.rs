use super::*;

fn tmpdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!(
            "autorip-resume-unreadable-plan-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(p.join("logs")).unwrap();
    p
}

// A state.json that EXISTS but can't be trusted must hold the dir: no mux, no
// partial-output delete, state.json left for the operator, an error surfaced.
fn assert_held(state_bytes: &[u8], tag: &str) {
    let _guard = crate::server::log::env_guard();
    // Then the mover/muxer statics lock: this asserts on MUX_ERRORS and STATE.
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let d = tmpdir();
    // SAFETY: env access in tests, serialized by env_guard.
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let staging = d.join("Show_S01D1");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join(staging::STATE_FILE), state_bytes).unwrap();
    let partial = staging.join("Show_S01D1.mkv");
    std::fs::write(&partial, b"partial").unwrap();

    let dev = format!("test_resume_unreadable_{tag}_{}", std::process::id());
    let class = ResumeClass::Remux {
        iso_path: staging.join("Show_S01D1.iso"),
        mapfile_path: staging.join("Show_S01D1.iso.mapfile"),
        display_name: "Show_S01D1".to_string(),
        title_confident: None,
    };
    let cfg = Arc::new(RwLock::new(Config::default()));
    resume_remux(&cfg, &dev, class);

    let live = crate::server::log::get_device_log(&dev, 200);
    assert!(
        !live.iter().any(|l| l.contains("Auto-resume: re-muxing")),
        "an unreadable plan must not proceed to the mux, got: {live:?}"
    );
    assert!(
        live.iter().any(|l| l.contains("Auto-resume held")),
        "the hold must be explained in the device log, got: {live:?}"
    );
    assert!(partial.exists(), "held dir must be left untouched");
    assert_eq!(
        std::fs::read(staging.join(staging::STATE_FILE)).unwrap(),
        state_bytes,
        "the unreadable state.json must be preserved for the operator"
    );
    let rs = crate::server::ripper::STATE.lock().unwrap().remove(&dev);
    let rs = rs.expect("device state must be set");
    assert_eq!(rs.status, "error");
    assert!(rs.last_error.contains("state.json"), "{}", rs.last_error);
    let path = staging.to_string_lossy().to_string();
    let card = crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap()
        .get(&path)
        .cloned();
    let card = card.expect("an operator error card must be raised for the held dir");
    // A parse/schema failure is not transient: point at repair, and warn
    // that deleting state.json delivers a TV disc as one title.
    assert!(card.hint.contains("ONE title"), "hint: {}", card.hint);
    assert!(!card.hint.contains("retry"), "hint: {}", card.hint);

    // Repaired: the next resume clears the held card (then fails on the
    // missing ISO, which is fine here).
    std::fs::remove_file(staging.join(staging::STATE_FILE)).unwrap();
    staging::write_state(
        &staging,
        &staging::DiscState::new(staging::StagingState::Ripped),
    );
    resume_remux(
        &cfg,
        &dev,
        ResumeClass::Remux {
            iso_path: staging.join("Show_S01D1.iso"),
            mapfile_path: staging.join("Show_S01D1.iso.mapfile"),
            display_name: "Show_S01D1".to_string(),
            title_confident: None,
        },
    );
    crate::server::ripper::STATE.lock().unwrap().remove(&dev);
    assert!(
        !crate::server::muxer::MUX_ERRORS
            .lock()
            .unwrap()
            .contains_key(&path),
        "a readable state.json must clear the held card"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn corrupt_state_json_holds_instead_of_delivering() {
    assert_held(b"{ this is not json", "corrupt");
}

#[test]
fn foreign_schema_state_json_holds_instead_of_delivering() {
    assert_held(br#"{"schema": 1, "state": "ripped"}"#, "foreign");
}

// A transient read error (EIO/ESTALE) is not corruption: its hint must say
// retry, never "delete state.json".
#[test]
fn transient_read_error_hint_says_retry_not_delete() {
    let io = staging::StateUnreadable::io(&std::io::Error::other("Stale file handle"));
    assert!(io.transient);
    assert!(io.hint().contains("retry"), "{}", io.hint());
    assert!(!io.hint().contains("delete"), "{}", io.hint());
    let bad = staging::StateUnreadable::invalid("state.json is unparseable".into());
    assert!(!bad.transient);
    assert!(bad.hint().contains("ONE title"), "{}", bad.hint());
}
