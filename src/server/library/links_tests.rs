use super::*;

#[test]
fn final_owner_change_after_reconciliation_blocks_each_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.iso");
    let final_path = dir.path().join("new.mkv");
    let old = dir.path().join("old.mkv");
    std::fs::write(&old, b"old").unwrap();
    let targets = vec![final_path.clone()];
    reconcile_replacement(dir.path(), &targets, &source).unwrap();
    let other = dir.path().join("other.iso");
    record(dir.path(), &final_path, &other).unwrap();
    assert!(
        retire_replacement(dir.path(), &old, &source, true, &targets, || panic!(
            "must not remove any original after final ownership changed"
        ))
        .is_err()
    );
    assert_eq!(std::fs::read(&old).unwrap(), b"old");
    assert_eq!(read(dir.path()).unwrap().get(&final_path), Some(&other));
}

#[test]
fn confirmed_cleanup_is_not_provenance_and_conflicting_owners_block_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old.mkv");
    let source = dir.path().join("source.iso");
    std::fs::write(&old, b"old").unwrap();
    assert!(retire_owned(dir.path(), &old, &source, || panic!("unproven cleanup")).is_err());
    retire_confirmed(dir.path(), &old, &source, || Ok(())).unwrap();
    assert!(read(dir.path()).unwrap().is_empty());
    record(dir.path(), &old, &dir.path().join("other.iso")).unwrap();
    assert!(retire_confirmed(dir.path(), &old, &source, || panic!("conflicting cleanup")).is_err());
    assert!(
        publish_replacement(dir.path(), &old, &source, false, || panic!(
            "conflicting publication"
        ))
        .is_err()
    );
    assert_eq!(std::fs::read(&old).unwrap(), b"old");
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
