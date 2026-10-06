use super::find_iso_and_mapfile;
use std::fs;

fn tmpdir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    // Repo-local scratch, never /tmp — anchor to the crate's own
    // target/ dir so artifacts are cleaned by `cargo clean` (mirrors
    // the staging.rs test helper).
    let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!(
            "autorip-find-iso-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
    // Wipe any stale contents first: `target/test-scratch` persists across
    // runs and a CI pid can be reused between debug/release test binaries,
    // so a leftover `subdir` would make `create_dir` fail with AlreadyExists.
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn pairs_iso_with_matching_mapfile() {
    let d = tmpdir();
    fs::write(d.join("Movie.iso"), b"x").unwrap();
    fs::write(d.join("Movie.iso.mapfile"), b"x").unwrap();
    let (iso, map) = find_iso_and_mapfile(&d).expect("should pair");
    assert!(iso.ends_with("Movie.iso"));
    assert!(map.ends_with("Movie.iso.mapfile"));
}

#[test]
fn rejects_when_mapfile_does_not_match_iso() {
    let d = tmpdir();
    fs::write(d.join("Movie.iso"), b"x").unwrap();
    // Mapfile keyed to a *different* ISO name — must not be paired.
    fs::write(d.join("Other.iso.mapfile"), b"x").unwrap();
    assert!(find_iso_and_mapfile(&d).is_none());
}

#[test]
fn rejects_multiple_isos_as_ambiguous() {
    let d = tmpdir();
    fs::write(d.join("A.iso"), b"x").unwrap();
    fs::write(d.join("A.iso.mapfile"), b"x").unwrap();
    fs::write(d.join("B.iso"), b"x").unwrap();
    assert!(find_iso_and_mapfile(&d).is_none());
}

#[test]
fn rejects_missing_mapfile() {
    let d = tmpdir();
    fs::write(d.join("Movie.iso"), b"x").unwrap();
    assert!(find_iso_and_mapfile(&d).is_none());
}

// Regression: the loop no longer uses `.flatten()` (which silently
// dropped per-DirEntry I/O errors). Per-entry error handling must not
// break the happy path with extra unrelated entries alongside the pair.
#[test]
fn pairs_despite_extra_entries() {
    let d = tmpdir();
    fs::write(d.join("Movie.iso"), b"x").unwrap();
    fs::write(d.join("Movie.iso.mapfile"), b"x").unwrap();
    // Noise the scan must skip over.
    fs::write(d.join("Movie.mkv"), b"x").unwrap();
    fs::write(d.join(".keep"), b"x").unwrap();
    fs::create_dir(d.join("subdir")).unwrap();
    let (iso, map) = find_iso_and_mapfile(&d).expect("should pair");
    assert!(iso.ends_with("Movie.iso"));
    assert!(map.ends_with("Movie.iso.mapfile"));
}
