use super::{preflight_validate, same_file, source_path_of, url_path_of};

struct Tmp(std::path::PathBuf);
impl Tmp {
    fn new(name: &str) -> Self {
        let d =
            std::env::temp_dir().join(format!("freemkv_same_file_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::create_dir_all(&d);
        Tmp(d)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).expect("write fixture");
        p
    }
    fn dir(&self, name: &str) -> std::path::PathBuf {
        let p = self.0.join(name);
        std::fs::create_dir_all(&p).expect("create fixture dir");
        p
    }
}
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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

/// A different file is not refused — the guard must not break ordinary
/// rips, which are the overwhelming majority.
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

/// Both `iso://` and `dir://` carry a path the guard has to see; anything
/// else (a live drive) has none.
#[test]
fn the_guard_sees_the_path_behind_every_file_backed_scheme() {
    assert!(source_path_of("iso:///media/Disc.iso").is_some());
    assert!(source_path_of("dir:///media/BDMV").is_some());
    assert!(
        source_path_of("disc://").is_none(),
        "a live drive has no path to compare"
    );
}

// ── Every file-backed scheme, not just iso:// ────────────────────────────
// The round-3 guard covered only `Iso`/`Dir`, truncating other schemes'
// still-open input. Now lives in `preflight_validate`, before any write.

/// The path behind a URL, for EVERY scheme that names one — the source
/// side and the destination side both.
#[test]
fn every_scheme_that_names_a_file_yields_its_path() {
    for url in [
        "mkv:///m/Movie.mkv",
        "m2ts:///m/Movie.m2ts",
        "mp4:///m/Movie.mp4",
        "mpg:///m/Movie.mpg",
        "iso:///m/Disc.iso",
        "dir:///m/BDMV",
        "demux:///m/tracks",
        "video:///m/tracks",
        "audio:///m/tracks",
        "sub:///m/tracks",
        "fvi:///m/Movie.fvi",
        "chapters:///m/Movie.xml",
        "json:///m/Movie.json",
    ] {
        assert!(
            url_path_of(&libfreemkv::parse_url(url)).is_some(),
            "{url} names a file on disk; the guard must be able to see it"
        );
    }
    // The schemes with no filesystem path at all.
    for url in [
        "disc://",
        "disc:///dev/sg0",
        "network://1.2.3.4:9000",
        "stdio://",
        "null://",
    ] {
        assert!(
            url_path_of(&libfreemkv::parse_url(url)).is_none(),
            "{url} has no path to compare"
        );
    }
}

/// Run the preflight gate the way `run` does.
fn preflight(source: &str, dest: &str) -> Result<(), String> {
    let ps = libfreemkv::parse_url(source);
    let pd = libfreemkv::parse_url(dest);
    // `force = true` so a `dir://` destination is not refused for merely
    // being non-empty — the same-file refusal must not depend on which
    // other check happens to fire first.
    preflight_validate(source, dest, &ps, &pd, false, false, true, false)
}

fn refused(source: &str, dest: &str) -> String {
    match preflight(source, dest) {
        Err(m) => m,
        Ok(()) => panic!("{source} → {dest} was ACCEPTED; it destroys the source"),
    }
}

/// THE defect. For each file-backed scheme, `X` as both source and
/// destination is refused — and the source still holds its original bytes
/// afterwards. Contents, not existence: a truncated file still exists.
#[test]
fn a_file_backed_scheme_never_writes_over_its_own_source() {
    let t = Tmp::new("self_dest");
    const BODY: &[u8] = b"the user's only copy of the movie";
    for (scheme, name) in [
        ("mkv", "Movie.mkv"),
        ("m2ts", "Movie.m2ts"),
        ("mp4", "Movie.mp4"),
        ("mpg", "Movie.mpg"),
        ("iso", "Disc.iso"),
    ] {
        let p = t.file(name, BODY);
        let url = format!("{scheme}://{}", p.display());
        let msg = refused(&url, &url);
        assert!(
            msg.contains("overwrite the source"),
            "{scheme}:// self-destination must be refused by name, got: {msg}"
        );
        assert_eq!(
            std::fs::read(&p).expect("source still readable"),
            BODY,
            "{scheme}:// self-destination truncated the source"
        );
    }
    // `dir://` is a source AND a sink; extracting a tree over itself is the
    // same defect with a directory instead of a file.
    let d = t.dir("BDMV");
    std::fs::write(d.join("index.bdmv"), BODY).expect("write tree file");
    let url = format!("dir://{}", d.display());
    refused(&url, &url);
    assert_eq!(
        std::fs::read(d.join("index.bdmv")).expect("tree file survives"),
        BODY,
        "dir:// self-destination clobbered the source tree"
    );
}

/// The pairing does not have to share a scheme. Every WRITE-ONLY sink
/// aimed at the source's own path is the same truncation.
#[test]
fn a_write_only_sink_never_aims_at_the_source_path() {
    let t = Tmp::new("cross_scheme");
    const BODY: &[u8] = b"the user's only copy of the disc image";
    let p = t.file("Disc.iso", BODY);
    let source = format!("iso://{}", p.display());
    for dest_scheme in ["mkv", "m2ts", "mp4", "fvi", "chapters", "json"] {
        let dest = format!("{dest_scheme}://{}", p.display());
        let msg = refused(&source, &dest);
        assert!(
            msg.contains("overwrite the source"),
            "iso:// → {dest_scheme}:// onto the same path must be refused, got: {msg}"
        );
        assert_eq!(
            std::fs::read(&p).expect("source still readable"),
            BODY,
            "iso:// → {dest_scheme}:// truncated the source"
        );
    }
    // The per-track directory sinks, pointed at the source TREE.
    let d = t.dir("BDMV");
    std::fs::write(d.join("index.bdmv"), BODY).expect("write tree file");
    let source = format!("dir://{}", d.display());
    for dest_scheme in ["demux", "video", "audio", "sub", "dir"] {
        let dest = format!("{dest_scheme}://{}", d.display());
        refused(&source, &dest);
    }
    assert_eq!(
        std::fs::read(d.join("index.bdmv")).expect("tree file survives"),
        BODY
    );
}

/// The case a string compare misses. `./Movie.mkv` and `Movie.mkv` are one
/// file; so is a symlink to it, and so is a hardlink — which canonicalize
/// alone does NOT resolve, because both names are already canonical.
#[test]
fn a_second_spelling_of_the_source_is_still_the_source() {
    let t = Tmp::new("spelling_dest");
    const BODY: &[u8] = b"one file, several names";
    let p = t.file("Movie.mkv", BODY);
    let source = format!("mkv://{}", p.display());

    // `sub/../Movie.mkv` — `Path`'s `==` cannot normalise `..` away, so
    // only a real canonicalize resolves this.
    let detour = t.dir("sub").join("..").join("Movie.mkv");
    assert_ne!(detour.as_path(), p.as_path(), "the fixture proves nothing");
    refused(&source, &format!("mkv://{}", detour.display()));

    // A symlink pointing at the source.
    #[cfg(unix)]
    {
        let link = t.0.join("Link.mkv");
        std::os::unix::fs::symlink(&p, &link).expect("symlink");
        refused(&source, &format!("mkv://{}", link.display()));

        // A HARDLINK. Both names canonicalize to themselves, so a
        // canonical-path compare says "different file" and the write
        // destroys the source anyway. Only dev+inode catches it.
        let hard = t.0.join("Hard.mkv");
        std::fs::hard_link(&p, &hard).expect("hard link");
        assert_ne!(
            std::fs::canonicalize(&p).expect("canon src"),
            std::fs::canonicalize(&hard).expect("canon hard"),
            "a hardlink must NOT be equal by canonical path, or it proves nothing"
        );
        refused(&source, &format!("mkv://{}", hard.display()));
    }

    assert_eq!(
        std::fs::read(&p).expect("source still readable"),
        BODY,
        "an aliased destination truncated the source"
    );
}

/// Different files still rip. The guard must not cost the ordinary case,
/// which is every real invocation.
#[test]
fn two_different_files_are_still_accepted() {
    let t = Tmp::new("distinct_dest");
    let a = t.file("a.mkv", b"aaa");
    let b = t.file("b.mkv", b"bbb");
    for (src_scheme, dest_scheme) in [("mkv", "mkv"), ("iso", "mkv"), ("m2ts", "mp4")] {
        let source = format!("{src_scheme}://{}", a.display());
        let dest = format!("{dest_scheme}://{}", b.display());
        assert!(
            preflight(&source, &dest).is_ok(),
            "{source} → {dest} is two different files and must be allowed"
        );
    }
    // And the overwhelmingly common shape: a destination that does not
    // exist yet.
    let fresh = t.0.join("new.mkv");
    assert!(
        preflight(
            &format!("iso://{}", a.display()),
            &format!("mkv://{}", fresh.display()),
        )
        .is_ok(),
        "a destination that does not exist yet can never be the source"
    );
}

#[test]
fn the_preflight_gate_actually_calls_the_guard() {
    let src = include_str!("pipe.rs").replace("\r\n", "\n");
    let start = src
        .find("\nfn preflight_validate(")
        .expect("preflight_validate definition present");
    let end = start
        + src[start..]
            .find("\n/// Validate a `dir://` destination")
            .expect("the next item still ends preflight_validate");
    let body = &src[start..end];
    assert!(
        body.contains("same_file(") && body.contains("url_path_of("),
        "preflight_validate must refuse a destination that IS the source, \
             before any sink is opened for writing"
    );
}
