use super::*;

// Drive `refuse_resume_open` on a fresh Ripped staging dir; returns the device's
// resulting (status, last_error, failure_deferred) and whether the dir went `.failed`.
fn refuse(e: libfreemkv::Error, from_drive: bool) -> (String, String, bool, bool) {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().join("Disc");
    std::fs::create_dir_all(&dir).unwrap();
    staging::write_state(
        &dir,
        &staging::DiscState::new(staging::StagingState::Ripped),
    );
    let device = format!("_refuse_open_{}_{}", e.code(), from_drive);
    let iso = dir.join("Disc.iso");
    refuse_resume_open(
        &Config::default(),
        &device,
        "Disc",
        (&iso, &dir),
        from_drive,
        &e,
    );
    let rs = super::super::STATE.lock().unwrap().remove(&device).unwrap();
    let failed = staging::snapshot_staging_disc(&dir).is_some_and(|s| s.has_failed);
    (rs.status, rs.last_error, rs.failure_deferred, failed)
}

#[test]
fn a_stop_while_resolving_keys_is_idle_not_an_error() {
    let (status, err, _, failed) = refuse(libfreemkv::Error::Halted, false);
    assert_eq!((status.as_str(), err.as_str(), failed), ("idle", "", false));
}

#[test]
fn a_wrong_disc_in_the_drive_leaves_staging_alone() {
    let e = || libfreemkv::Error::MapfileInvalid {
        kind: "disc-mismatch",
    };
    let (status, err, _, failed) = refuse(e(), true);
    assert_eq!(status, "error");
    assert!(err.contains("Insert the original disc"), "{err}");
    assert!(!failed, "the inserted disc is wrong, not the staging");
    // With no drive in hand the sidecar itself is another disc's: terminal.
    let (_, _, _, failed) = refuse(e(), false);
    assert!(failed, "a mismatched sidecar is quarantined");
}

#[test]
fn a_corrupt_sidecar_is_quarantined() {
    let e = libfreemkv::Error::MapfileInvalid { kind: "corrupt" };
    let (status, _, _, failed) = refuse(e, true);
    assert_eq!(status, "error");
    assert!(failed);
}

#[test]
fn missing_fmts_keys_defer_the_mux() {
    let (status, err, deferred, failed) = refuse(libfreemkv::Error::FmtsKeyMissing, false);
    assert_eq!(status, "idle");
    assert!(deferred && !failed);
    assert!(err.contains("forensic keys"), "{err}");
}
