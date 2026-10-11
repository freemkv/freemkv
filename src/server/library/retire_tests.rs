use super::*;
use crate::server::util::Stamp;

struct Quiet;
impl freemkv_engine::Sink for Quiet {}

struct Fixture {
    _t: tempfile::TempDir,
    config: PathBuf,
    root: PathBuf,
    iso: PathBuf,
    target: PathBuf,
}

fn fixture() -> Fixture {
    fixture_in(&std::env::temp_dir())
}

fn fixture_in(base: &Path) -> Fixture {
    let t = tempfile::tempdir_in(base).unwrap();
    let config = t.path().join("config");
    let root = t.path().join("movies");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(root.join("Show")).unwrap();
    let target = root.join("Show/New.mkv");
    std::fs::write(&target, b"new").unwrap();
    Fixture {
        iso: t.path().join("A.iso"),
        _t: t,
        config,
        root,
        target,
    }
}

impl Fixture {
    fn old(&self, name: &str, link_to: Option<&Path>) -> Owned {
        let path = self.root.join("Show").join(name);
        std::fs::write(&path, name.as_bytes()).unwrap();
        if let Some(iso) = link_to {
            links::record(&self.config, &path, iso).unwrap();
        }
        Owned {
            stamp: Stamp::read(&path).unwrap(),
            path,
        }
    }

    fn run(&self, owned: &[Owned]) -> io::Result<Vec<PathBuf>> {
        let roots = [self.root.clone()];
        let r = Retire {
            iso: &self.iso,
            config_dir: &self.config,
            roots: &roots,
            sink: &Quiet,
            stopping: &|| false,
        };
        run(&r, owned, std::slice::from_ref(&self.target))
    }

    fn linked(&self, path: &Path) -> Option<PathBuf> {
        links::read(&self.config).unwrap().get(path).cloned()
    }
}

#[test]
fn a_linked_file_already_gone_counts_as_removed_and_its_link_goes() {
    let f = fixture();
    let old = f.old("Old.mkv", Some(&f.iso));
    std::fs::remove_file(&old.path).unwrap();
    assert!(f.run(std::slice::from_ref(&old)).unwrap().is_empty());
    assert_eq!(f.linked(&old.path), None);
}

#[test]
fn a_present_file_no_longer_linked_is_kept_and_links_are_unchanged() {
    let f = fixture();
    let old = f.old("Old.mkv", None);
    let before = links::read(&f.config).unwrap();
    assert_eq!(
        f.run(std::slice::from_ref(&old)).unwrap(),
        vec![old.path.clone()]
    );
    assert!(old.path.exists());
    assert_eq!(links::read(&f.config).unwrap(), before);
}

#[test]
fn a_file_linked_to_another_iso_is_kept_with_its_link() {
    let f = fixture();
    let other = f.iso.with_file_name("B.iso");
    let old = f.old("Old.mkv", Some(&other));
    assert_eq!(
        f.run(std::slice::from_ref(&old)).unwrap(),
        vec![old.path.clone()]
    );
    assert!(old.path.exists());
    assert_eq!(f.linked(&old.path), Some(other));
}

#[test]
fn an_entry_already_retired_is_a_no_op() {
    let f = fixture();
    let old = f.old("Old.mkv", Some(&f.iso));
    assert!(f.run(std::slice::from_ref(&old)).unwrap().is_empty());
    assert!(!old.path.exists());
    assert!(
        f.run(&[old]).unwrap().is_empty(),
        "a second pass changes nothing"
    );
}

#[test]
fn a_missing_folder_is_a_storage_fault_that_keeps_the_link() {
    let f = fixture();
    let gone = f.root.join("Gone/Old.mkv");
    let old = Owned {
        path: gone.clone(),
        stamp: f.old("Stamp.mkv", None).stamp,
    };
    links::record(&f.config, &gone, &f.iso).unwrap();
    let e = f.run(&[old]).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotConnected, "{e}");
    assert!(crate::server::library::worker::storage_fault(&e).is_some());
    assert_eq!(f.linked(&gone), Some(f.iso.clone()));
}

#[test]
fn a_new_file_that_is_no_longer_there_stops_retire_before_any_removal() {
    let f = fixture();
    let old = f.old("Old.mkv", Some(&f.iso));
    std::fs::remove_file(&f.target).unwrap();
    assert!(f.run(std::slice::from_ref(&old)).is_err());
    assert!(old.path.exists());
    assert_eq!(f.linked(&old.path), Some(f.iso.clone()));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&old.path, &f.target).unwrap();
        assert!(f.run(std::slice::from_ref(&old)).is_err());
        assert!(old.path.exists());
    }
}

#[test]
fn an_old_path_outside_the_roots_is_never_removed() {
    let f = fixture();
    let outside = f._t.path().join("elsewhere.mkv");
    std::fs::write(&outside, b"x").unwrap();
    let old = Owned {
        stamp: Stamp::read(&outside).unwrap(),
        path: outside.clone(),
    };
    assert!(f.run(&[old]).is_err());
    assert!(outside.exists());
}

#[test]
fn a_missing_new_file_does_not_fail_when_there_is_nothing_to_retire() {
    let f = fixture();
    let own = Owned {
        path: f.target.clone(),
        stamp: Stamp::read(&f.target).unwrap(),
    };
    std::fs::remove_file(&f.target).unwrap();
    assert!(f.run(&[own]).unwrap().is_empty());
    assert!(f.run(&[]).unwrap().is_empty());
    let old = f.old("Old.mkv", Some(&f.iso));
    let e = f.run(std::slice::from_ref(&old)).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotConnected);
    assert!(old.path.exists());
}

#[cfg(unix)]
#[test]
fn an_owned_file_swapped_for_a_symlink_is_kept_and_disowned_not_a_failed_pass() {
    let f = fixture();
    let swapped = f.old("Old E01.mkv", Some(&f.iso));
    let after = f.old("Old E02.mkv", Some(&f.iso));
    let elsewhere = f.root.join("Show/copy.mkv");
    std::fs::write(&elsewhere, b"a copy").unwrap();
    std::fs::remove_file(&swapped.path).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &swapped.path).unwrap();
    let kept = f.run(&[swapped.clone(), after.clone()]).unwrap();
    assert_eq!(kept, vec![swapped.path.clone()]);
    assert!(std::fs::symlink_metadata(&swapped.path).is_ok());
    assert_eq!(f.linked(&swapped.path), None, "disowned");
    assert!(!after.path.exists(), "the pass went on past it");
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"a copy");
}

// Amendment 4: no device comparison. A child dataset, subvolume or crossmnt export below
// the root has its own device id and is still the root's storage.
#[test]
fn an_old_file_on_a_nested_filesystem_below_its_root_is_retired() {
    use std::os::unix::fs::MetadataExt;
    // Linux (where CI runs it) always has one: tmpfs /dev/shm below devtmpfs /dev.
    let shm = Path::new("/dev/shm");
    let f = if shm.is_dir() {
        fixture_in(shm)
    } else {
        fixture()
    };
    let dev = |p: &Path| std::fs::metadata(p).unwrap().dev();
    let nested = f
        .root
        .ancestors()
        .find(|a| dev(a) != dev(&f.root))
        .map(Path::to_path_buf);
    let Some(root) = nested else {
        assert!(!cfg!(target_os = "linux"), "no nested filesystem on Linux");
        return; // no writable nested filesystem on this host
    };
    let old = f.old("Old.mkv", Some(&f.iso));
    let roots = [root];
    let r = Retire {
        iso: &f.iso,
        config_dir: &f.config,
        roots: &roots,
        sink: &Quiet,
        stopping: &|| false,
    };
    let kept = run(
        &r,
        std::slice::from_ref(&old),
        std::slice::from_ref(&f.target),
    )
    .unwrap();
    assert!(kept.is_empty());
    assert!(!old.path.exists());
}

#[test]
fn emptied_folders_are_removed_up_to_any_configured_root_never_past_it() {
    let f = fixture();
    let tv = f.root.join("tv");
    let path = tv.join("Old Show/Old Show.mkv");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"old").unwrap();
    links::record(&f.config, &path, &f.iso).unwrap();
    let old = Owned {
        stamp: Stamp::read(&path).unwrap(),
        path,
    };
    let roots = [f.root.clone(), tv.clone()];
    let r = Retire {
        iso: &f.iso,
        config_dir: &f.config,
        roots: &roots,
        sink: &Quiet,
        stopping: &|| false,
    };
    assert!(
        run(&r, &[old], std::slice::from_ref(&f.target))
            .unwrap()
            .is_empty()
    );
    assert!(
        !tv.join("Old Show").exists(),
        "the emptied title folder goes"
    );
    assert!(tv.is_dir(), "the TV root stays though it is empty");
}
