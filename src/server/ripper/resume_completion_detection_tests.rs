use crate::server::ripper::staging;

fn tmpdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!(
            "autorip-resume-complete-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
    // See the note in the find-iso `tmpdir`: clear stale contents so a
    // reused scratch path (persistent dir + CI pid reuse) starts empty.
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// Calls the real mux_handoff_success helper (not a hand-rolled copy)
// so this proves what remux_from_ripped_marker will observe: true
// after write_completed_marker, so clear_error runs and nothing sticks.
#[test]
fn snapshot_reports_completed_after_marker_write() {
    let d = tmpdir();
    staging::write_completed_marker(&d);
    assert!(
        super::mux_handoff_success(&d),
        "mux_handoff_success must report true after write_completed_marker"
    );
}

/// And without the marker (halt / scan_image failure / mux loop break) it
/// must report `false` so the failure path records the error.
#[test]
fn snapshot_reports_not_completed_without_marker() {
    let d = tmpdir();
    // A partial dir with ISO/mapfile but no `.completed`.
    std::fs::write(d.join("Movie.iso"), b"x").unwrap();
    std::fs::write(d.join("Movie.iso.mapfile"), b"x").unwrap();
    assert!(
        !super::mux_handoff_success(&d),
        "mux_handoff_success must report false without the .completed marker"
    );
}

// build_mux_handoff_outcome must carry `success` through both ways,
// guarding against a struct-literal mutant that silently drops it
// and reports a genuinely successful resumed mux as failed.
#[test]
fn build_mux_handoff_outcome_carries_success_both_ways() {
    assert!(super::build_mux_handoff_outcome(true).success);
    assert!(!super::build_mux_handoff_outcome(false).success);
}
