use super::*;

#[test]
fn forget_drops_only_a_link_to_the_named_iso() {
    let dir = tempfile::tempdir().unwrap();
    let (ours, theirs) = (Path::new("/m/A.mkv"), Path::new("/m/B.mkv"));
    record(dir.path(), ours, Path::new("/i/A.iso")).unwrap();
    record(dir.path(), theirs, Path::new("/i/B.iso")).unwrap();
    forget(dir.path(), theirs, Path::new("/i/A.iso")).unwrap();
    forget(dir.path(), ours, Path::new("/i/A.iso")).unwrap();
    let links = read(dir.path()).unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(
        links.get(theirs).map(PathBuf::as_path),
        Some(Path::new("/i/B.iso"))
    );
}

#[test]
fn unreadable_links_refuse_writes_without_erasing_provenance_or_prior_backup() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join(LINKS_FILE), b"{ not json").unwrap();
    let backup = t.path().join("library-links.json.unreadable");
    std::fs::write(&backup, b"previous recovery evidence").unwrap();
    assert!(record(t.path(), Path::new("/m/A.mkv"), Path::new("/i/A.iso")).is_err());
    assert_eq!(
        std::fs::read(t.path().join(LINKS_FILE)).unwrap(),
        b"{ not json"
    );
    assert_eq!(
        std::fs::read(backup).unwrap(),
        b"previous recovery evidence"
    );
}

#[test]
fn a_delivery_links_every_episode_to_its_single_iso() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_str().unwrap();
    record_delivery(dir, ["/m/A (2000)/A (2000).mkv", "/i/A (2000).iso"]).unwrap();
    record_delivery(dir, ["/m/B/B.mkv"]).unwrap();
    record_delivery(dir, ["/m/S/e1.mkv", "/m/S/e2.mkv", "/i/S.iso"]).unwrap();
    let links = load(t.path());
    assert_eq!(links.len(), 3);
    for episode in ["/m/S/e1.mkv", "/m/S/e2.mkv"] {
        assert_eq!(
            links.get(Path::new(episode)),
            Some(&PathBuf::from("/i/S.iso"))
        );
    }
    assert_eq!(
        links.get(Path::new("/m/A (2000)/A (2000).mkv")),
        Some(&PathBuf::from("/i/A (2000).iso"))
    );
}

#[test]
fn completed_delivery_reports_provenance_failure_for_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(LINKS_FILE);
    std::fs::write(&path, b"broken provenance").unwrap();
    assert!(
        record_delivery(
            dir.path().to_str().unwrap(),
            ["/m/show/e1.mkv", "/m/show/e2.mkv", "/i/show.iso"],
        )
        .is_err()
    );
    assert_eq!(std::fs::read(path).unwrap(), b"broken provenance");
}

#[test]
fn multiple_source_isos_do_not_authorize_output_links() {
    let t = tempfile::tempdir().unwrap();
    record_delivery(
        t.path().to_str().unwrap(),
        ["/m/a.mkv", "/i/a.iso", "/i/b.iso"],
    )
    .unwrap();
    assert!(load(t.path()).is_empty());
}

#[test]
fn a_new_link_joins_the_earlier_ones() {
    let t = tempfile::tempdir().unwrap();
    record(t.path(), Path::new("/m/A.mkv"), Path::new("/i/A.iso")).unwrap();
    record(t.path(), Path::new("/m/B.mkv"), Path::new("/i/B.iso")).unwrap();
    let links = load(t.path());
    assert_eq!(links.len(), 2);
    assert_eq!(
        links.get(Path::new("/m/A.mkv")),
        Some(&PathBuf::from("/i/A.iso"))
    );
}

// Recording links already there is not a durable rewrite of the file.
#[cfg(unix)]
#[test]
fn recording_the_same_links_again_writes_nothing() {
    use std::os::unix::fs::MetadataExt;
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join(LINKS_FILE);
    record(t.path(), Path::new("/m/A.mkv"), Path::new("/i/A.iso")).unwrap();
    let before = std::fs::metadata(&file).unwrap().ino();
    record(t.path(), Path::new("/m/A.mkv"), Path::new("/i/A.iso")).unwrap();
    assert_eq!(std::fs::metadata(&file).unwrap().ino(), before);
    record(t.path(), Path::new("/m/A.mkv"), Path::new("/i/B.iso")).unwrap();
    assert_ne!(
        std::fs::metadata(&file).unwrap().ino(),
        before,
        "a change is saved"
    );
}
