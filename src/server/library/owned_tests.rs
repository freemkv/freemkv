use super::*;

#[test]
fn capture_keeps_only_this_isos_regular_mkvs_under_the_roots_sorted() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("movies");
    std::fs::create_dir_all(&root).unwrap();
    let iso = t.path().join("isos/Show.iso");
    let mut links = HashMap::new();
    let mut link = |name: &Path, owner: &Path, bytes: Option<&[u8]>| {
        if let Some(bytes) = bytes {
            std::fs::create_dir_all(name.parent().unwrap()).unwrap();
            std::fs::write(name, bytes).unwrap();
        }
        links.insert(name.to_path_buf(), owner.to_path_buf());
    };
    let b = root.join("Show/B.mkv");
    let a = root.join("Show/A.mkv");
    link(&b, &iso, Some(b"b"));
    link(&a, &iso, Some(b"a"));
    link(&root.join("Show/gone.mkv"), &iso, None);
    link(&root.join("Show/notes.txt"), &iso, Some(b"x"));
    link(
        &root.join("Other.mkv"),
        &t.path().join("other.iso"),
        Some(b"o"),
    );
    link(&t.path().join("elsewhere/E.mkv"), &iso, Some(b"e"));
    link(&root.join("Show/../Show/A.mkv"), &iso, Some(b"a"));
    #[cfg(unix)]
    {
        let alias = root.join("Show/alias.mkv");
        std::os::unix::fs::symlink(&a, &alias).unwrap();
        link(&alias, &iso, None);
    }
    let owned = capture(&links, &iso, std::slice::from_ref(&root)).unwrap();
    let paths: Vec<_> = owned.iter().map(|o| o.path.clone()).collect();
    assert_eq!(paths, vec![a.clone(), b]);
    assert_eq!(owned[0].stamp, Stamp::read(&a).unwrap());
}

#[test]
fn roots_are_the_library_and_a_separate_tv_folder() {
    let d = Dirs {
        library: "/media/movies".into(),
        tv: Some("/media/tv".into()),
        isos: None,
        iso_subfolders: false,
    };
    assert_eq!(
        roots(&d),
        vec![PathBuf::from("/media/movies"), PathBuf::from("/media/tv")]
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_sibling_is_skipped_unless_required() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("movies");
    let (good, locked) = (root.join("Ok/A.mkv"), root.join("Locked/B.mkv"));
    for p in [&good, &locked] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"x").unwrap();
    }
    let iso = t.path().join("A.iso");
    let links: HashMap<_, _> = [&good, &locked]
        .into_iter()
        .map(|p| (p.clone(), iso.clone()))
        .collect();
    let dir = locked.parent().unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let roots = std::slice::from_ref(&root);
    let lenient = capture_with(&links, &iso, roots, |p| p == good);
    let strict = capture(&links, &iso, roots);
    let target_required = capture_with(&links, &iso, roots, |_| true);
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    if std::fs::symlink_metadata(&locked).is_ok() && strict.is_ok() {
        return; // running as a user that ignores modes
    }
    let paths: Vec<_> = lenient.unwrap().into_iter().map(|o| o.path).collect();
    assert_eq!(paths, vec![good]);
    assert!(strict.is_err() && target_required.is_err());
}

#[test]
fn eligible_paths_filters_by_rules_without_touching_disk() {
    let iso = PathBuf::from("/i/A.iso");
    let root = PathBuf::from("/nonexistent/movies");
    let mine = root.join("A.mkv");
    let links: HashMap<_, _> = [
        (mine.clone(), iso.clone()),
        (root.join("n.txt"), iso.clone()),
        (PathBuf::from("/else/B.mkv"), iso.clone()),
        (root.join("C.mkv"), PathBuf::from("/i/other.iso")),
    ]
    .into();
    assert_eq!(eligible_paths(&links, &iso, &[root]), vec![mine]);
}
