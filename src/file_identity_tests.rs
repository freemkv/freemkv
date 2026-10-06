use super::{file_id, same_file};

struct Tmp(std::path::PathBuf);
impl Tmp {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!(
            "freemkv_file_identity_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("fixture dir");
        Tmp(d)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).expect("write fixture");
        p
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// The same file under two spellings is one file — the case a path
// comparison misses. Detour via `..` because `Path`'s own `==` normalises
// `.` away, so a `./Disc.iso` fixture would pass even without canonicalize.
#[test]
fn the_same_file_under_two_spellings_is_recognised() {
    let t = Tmp::new("spellings");
    let abs = t.file("Disc.iso", b"not really an iso");
    std::fs::create_dir_all(t.0.join("sub")).expect("subdir");
    let detoured = t.0.join("sub").join("..").join("Disc.iso");
    assert_ne!(
        detoured.as_path(),
        abs.as_path(),
        "the fixture must not be equal as PATHS, or it proves nothing"
    );
    assert!(same_file(Some(&abs), &abs), "a path is itself");
    assert!(
        same_file(Some(&detoured), &abs),
        "sub/../Disc.iso and Disc.iso are one file"
    );
}

// A HARDLINK: both names canonicalize to themselves, so only filesystem identity sees that
// writing either one destroys the other. Runs on Windows too.
#[test]
fn a_hardlink_is_the_same_file_even_though_the_paths_differ() {
    let t = Tmp::new("hardlink");
    let a = t.file("Movie.mkv", b"one file, two names");
    let hard = t.0.join("Hard.mkv");
    std::fs::hard_link(&a, &hard).expect(
        "could not create a hard link in the temp dir — on Windows this \
             needs an NTFS %TEMP% (FAT32/exFAT cannot do it). Not skipped on \
             purpose: skipping is what left this module's only unsafe block \
             untested for three releases",
    );
    assert_ne!(
        std::fs::canonicalize(&a).expect("canon a"),
        std::fs::canonicalize(&hard).expect("canon hard"),
        "a hardlink must NOT be equal by canonical path, or it proves nothing"
    );
    assert!(same_file(Some(&a), &hard));
    assert!(same_file(Some(&hard), &a), "and the other way round");
}

/// Different files still rip: the guard must not cost the ordinary case.
#[test]
fn two_different_files_are_not_the_same_file() {
    let t = Tmp::new("distinct");
    let a = t.file("In.iso", b"a");
    let b = t.file("Out.iso", b"b");
    assert!(!same_file(Some(&a), &b));
}

/// The ordinary case: the destination does not exist yet. It cannot be the
/// source, and a failed canonicalize must never refuse the rip.
#[test]
fn a_destination_that_does_not_exist_yet_is_never_the_source() {
    let t = Tmp::new("missing_dest");
    let a = t.file("In.iso", b"a");
    let dest = t.0.join("does-not-exist.iso");
    assert!(!same_file(Some(&a), &dest));
    // And a source with no filesystem path at all (disc://) is never it.
    assert!(!same_file(None, &dest));
}

/// A DIRECTORY has an identity too — `dir://` sources and destinations go
/// through this guard, so an implementation that only ever opens files
/// would answer `None` for exactly the tree-destroying case.
#[test]
fn a_directory_has_a_readable_identity() {
    let t = Tmp::new("dir_identity");
    assert!(
        file_id(&t.0).is_some(),
        "a directory must yield a filesystem identity"
    );
    assert_eq!(file_id(&t.0), file_id(&t.0), "and a stable one");
}
