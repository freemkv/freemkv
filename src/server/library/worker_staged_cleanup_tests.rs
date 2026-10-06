use super::clean_stale_staging;

#[test]
fn startup_removes_only_library_job_partials() {
    let dir = tempfile::tempdir().unwrap();
    let stale = ["42.mkv.partial", "42.mkv", "42.mkv.lock"].map(|n| dir.path().join(n));
    let unrelated =
        ["disc.mkv.partial", "disc.mkv", "A.1.staged.mkv", "42.iso"].map(|n| dir.path().join(n));
    for p in stale.iter().chain(&unrelated) {
        std::fs::write(p, b"x").unwrap();
    }
    clean_stale_staging(dir.path());
    assert!(stale.iter().all(|p| !p.exists()));
    assert!(unrelated.iter().all(|p| p.exists()));
}
