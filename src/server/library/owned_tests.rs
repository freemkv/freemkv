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
