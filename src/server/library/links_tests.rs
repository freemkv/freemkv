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
fn a_damaged_links_file_is_set_aside_under_a_new_name_and_recording_continues() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join(LINKS_FILE), b"{ not json").unwrap();
    let backup = t.path().join("library-links.json.unreadable");
    std::fs::write(&backup, b"previous recovery evidence").unwrap();
    record(t.path(), Path::new("/m/A.mkv"), Path::new("/i/A.iso")).unwrap();
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        b"previous recovery evidence"
    );
    assert_eq!(
        std::fs::read(t.path().join("library-links.json.unreadable.1")).unwrap(),
        b"{ not json"
    );
    assert_eq!(
        load(t.path()).get(Path::new("/m/A.mkv")),
        Some(&PathBuf::from("/i/A.iso"))
    );
}

#[cfg(unix)]
#[test]
fn a_links_file_that_cannot_be_read_is_left_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join(LINKS_FILE);
    std::fs::write(&file, b"{}").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&file).is_ok() {
        return; // running as a user that ignores modes
    }
    let result = record(t.path(), Path::new("/m/A.mkv"), Path::new("/i/A.iso"));
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(result.is_err());
    assert_eq!(std::fs::read(&file).unwrap(), b"{}");
    assert!(!t.path().join("library-links.json.unreadable").exists());
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

#[test]
fn a_link_records_the_file_and_the_iso_as_they_were_and_1_8_3_entries_stay_readable() {
    let t = tempfile::tempdir().unwrap();
    let (mkv, iso) = (t.path().join("A.mkv"), t.path().join("A.iso"));
    std::fs::write(&mkv, b"movie").unwrap();
    std::fs::write(&iso, b"image").unwrap();
    let legacy = serde_json::json!({ "/m/Old.mkv": "/i/Old.iso" });
    std::fs::write(
        t.path().join(LINKS_FILE),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    record(t.path(), &mkv, &iso).unwrap();
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(t.path().join(LINKS_FILE)).unwrap()).unwrap();
    assert_eq!(json["/m/Old.mkv"], "/i/Old.iso", "{json}");
    let entry = &json[mkv.to_str().unwrap()];
    assert_eq!(entry["iso"], iso.to_str().unwrap(), "{json}");
    assert_eq!(entry["stamp"]["size"], 5, "{json}");
    assert_eq!(entry["iso_stamp"]["size"], 5, "{json}");
}

#[test]
fn recording_from_a_disc_refiled_at_the_same_iso_path_drops_the_earlier_discs_links() {
    let t = tempfile::tempdir().unwrap();
    let iso = t.path().join("Show.iso");
    let (s1, s2) = (t.path().join("S1.mkv"), t.path().join("S2.mkv"));
    std::fs::write(&iso, b"season 1").unwrap();
    std::fs::write(&s1, b"episode").unwrap();
    std::fs::write(&s2, b"episode").unwrap();
    record(t.path(), &s1, &iso).unwrap();
    std::fs::remove_file(&iso).unwrap();
    std::fs::write(&iso, b"season 2 disc").unwrap();
    record(t.path(), &s2, &iso).unwrap();
    let links = load(t.path());
    assert_eq!(links.len(), 1, "{links:?}");
    assert!(links.contains_key(&s2));
}
