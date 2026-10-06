use super::*;

#[test]
fn an_unreadable_links_file_is_kept_when_a_new_link_is_recorded() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join(LINKS_FILE), b"{ not json").unwrap();
    record(t.path(), Path::new("/m/A.mkv"), Path::new("/i/A.iso")).unwrap();
    assert_eq!(load(t.path()).len(), 1);
    let kept = std::fs::read(t.path().join("library-links.json.unreadable")).unwrap();
    assert_eq!(kept, b"{ not json");
}

#[test]
fn a_delivery_of_one_mkv_and_one_iso_is_recorded() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_str().unwrap();
    record_delivery(dir, ["/m/A (2000)/A (2000).mkv", "/i/A (2000).iso"]);
    record_delivery(dir, ["/m/B/B.mkv"]);
    record_delivery(dir, ["/m/S/e1.mkv", "/m/S/e2.mkv", "/i/S.iso"]);
    let links = load(t.path());
    assert_eq!(links.len(), 1);
    assert_eq!(
        links.get(Path::new("/m/A (2000)/A (2000).mkv")),
        Some(&PathBuf::from("/i/A (2000).iso"))
    );
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
