use super::*;

// A link as freemkv records it: the file's stamp when it exists.
fn link(path: &Path, iso: &Path) -> Link {
    Link {
        iso: iso.to_path_buf(),
        stamp: Stamp::read(path).ok(),
        iso_stamp: None,
        episode: false,
    }
}

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
        links.insert(name.to_path_buf(), link(name, owner));
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
    link(&root.join("Show/../Show/A.mkv"), &iso, None);
    // A symlink is never ours, even linked with the stamp of the file it points to.
    #[cfg(unix)]
    {
        let alias = root.join("Show/alias.mkv");
        std::os::unix::fs::symlink(&a, &alias).unwrap();
        let linked = self::link(&a, &iso);
        assert!(linked.stamp.is_some());
        links.insert(alias, linked);
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
        .map(|p| (p.clone(), link(p, &iso)))
        .collect();
    let dir = locked.parent().unwrap();
    let permissions = std::fs::metadata(dir).unwrap().permissions();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Probe before restoring access, independently of either capture result.
    let ignores_modes = std::fs::symlink_metadata(&locked).is_ok();
    let roots = std::slice::from_ref(&root);
    let lenient = capture_with(&links, &iso, roots, |p| p == good);
    let strict = capture(&links, &iso, roots);
    let target_required = capture_with(&links, &iso, roots, |_| true);
    std::fs::set_permissions(dir, permissions).unwrap();
    if ignores_modes {
        return; // running as a user that ignores modes
    }
    let paths: Vec<_> = lenient.unwrap().into_iter().map(|o| o.path).collect();
    assert_eq!(paths, vec![good]);
    assert!(strict.is_err(), "capture must require every linked sibling");
    assert!(target_required.is_err());
}

#[test]
fn eligible_paths_filters_by_rules_without_touching_disk() {
    let iso = PathBuf::from("/i/A.iso");
    let root = PathBuf::from("/nonexistent/movies");
    let mine = root.join("A.mkv");
    let stamped = |iso: &Path| Link {
        stamp: Some(Stamp {
            size: 1,
            modified: std::time::UNIX_EPOCH,
        }),
        ..link(Path::new("/nonexistent"), iso)
    };
    let links: HashMap<_, _> = [
        (mine.clone(), stamped(&iso)),
        (root.join("n.txt"), stamped(&iso)),
        (PathBuf::from("/else/B.mkv"), stamped(&iso)),
        (root.join("C.mkv"), stamped(Path::new("/i/other.iso"))),
        (
            root.join("Legacy.mkv"),
            link(Path::new("/nonexistent"), &iso),
        ),
    ]
    .into();
    assert_eq!(eligible_paths(&links, &iso, &[root]), vec![mine]);
}
