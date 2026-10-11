use super::*;

struct TestIo {
    saved: Replacement,
    fail_prepare: Option<usize>,
    fail_publish: bool,
    fail_retire: bool,
    prepares: Vec<usize>,
    checkpoints: usize,
    remove_at_checkpoint: Option<(usize, PathBuf)>,
    ownership_config: Option<(PathBuf, PathBuf)>,
    // Publish as smbfs with name-derived ids does: a new id, same size and time.
    publish_with_new_identity: bool,
}

impl RunIo for TestIo {
    fn reconcile_new_set(&mut self, targets: &[PathBuf]) -> io::Result<()> {
        if let Some((config, source)) = &self.ownership_config {
            super::super::links::reconcile_replacement(config, targets, source)?;
        }
        Ok(())
    }
    fn guard_new_set(&mut self, _targets: &[PathBuf]) -> io::Result<()> {
        Ok(())
    }
    fn checkpoint(&mut self) -> io::Result<()> {
        self.checkpoints += 1;
        if let Some((at, path)) = &self.remove_at_checkpoint
            && *at == self.checkpoints
        {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
    fn prepare(&mut self, ordinal: usize, candidate: &Path) -> io::Result<Option<String>> {
        if self.fail_prepare == Some(ordinal) {
            return Err(io::Error::other("mux failed"));
        }
        self.prepares.push(ordinal);
        std::fs::write(candidate, format!("verified new {ordinal}"))?;
        Ok(Some("freemkv test".into()))
    }
    fn publish(
        &mut self,
        candidate: &Path,
        target: &Path,
        before: Option<&FileIdentity>,
    ) -> io::Result<FileIdentity> {
        if let Some(before) = before {
            before.verify(target)?;
        } else if target.exists() {
            return Err(io::Error::other("target collision"));
        }
        if self.publish_with_new_identity {
            let modified = std::fs::metadata(candidate)?.modified()?;
            std::fs::copy(candidate, target)?;
            std::fs::File::options()
                .write(true)
                .open(target)?
                .set_modified(modified)?;
            std::fs::remove_file(candidate)?;
        } else {
            std::fs::rename(candidate, target)?;
        }
        if self.fail_publish {
            return Err(io::Error::other("crash after rename"));
        }
        FileIdentity::read_fresh(target)
    }
    fn retire(&mut self, path: &Path, identity: &FileIdentity) -> io::Result<()> {
        if self.fail_retire {
            return Err(io::Error::other("cleanup unavailable"));
        }
        identity.verify(path)?;
        std::fs::remove_file(path)
    }
    fn persist(&mut self, replacement: &Replacement) -> io::Result<()> {
        self.saved = serde_json::from_slice(&serde_json::to_vec(replacement).unwrap()).unwrap();
        Ok(())
    }
}

fn fixture() -> (
    tempfile::TempDir,
    PathBuf,
    Vec<PathBuf>,
    Replacement,
    TestIo,
) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("disc.iso");
    std::fs::write(&source, b"source").unwrap();
    let old: Vec<_> = (1..=3)
        .map(|i| dir.path().join(format!("old-episode-{i}.mkv")))
        .collect();
    for path in &old {
        std::fs::write(path, b"old episode").unwrap();
    }
    let links = old.iter().map(|p| (p.clone(), source.clone())).collect();
    let replacement = Replacement::capture(&source, &links).unwrap();
    let io = TestIo {
        saved: replacement.clone(),
        fail_prepare: None,
        fail_publish: false,
        fail_retire: false,
        prepares: vec![],
        checkpoints: 0,
        remove_at_checkpoint: None,
        ownership_config: None,
        publish_with_new_identity: false,
    };
    (dir, source, old, replacement, io)
}

#[test]
fn restart_after_rename_refuses_conflicting_final_link_before_cleanup() {
    let (dir, source, old, mut replacement, mut io) = fixture();
    let targets = vec![dir.path().join("new-movie.mkv")];
    io.fail_publish = true;
    assert!(replacement.run(&source, &targets, 10, &mut io).is_err());
    let other = dir.path().join("other.iso");
    super::super::links::record(dir.path(), &targets[0], &other).unwrap();
    io.ownership_config = Some((dir.path().to_owned(), source.clone()));
    io.fail_publish = false;
    let mut recovered = io.saved.clone();
    assert!(recovered.run(&source, &targets, 10, &mut io).is_err());
    assert!(old.iter().all(|path| path.exists()));
    assert_eq!(
        super::super::links::read(dir.path())
            .unwrap()
            .get(&targets[0]),
        Some(&other)
    );
    assert!(source.exists());
}

#[test]
fn restart_reconciles_absent_or_matching_final_links_before_retirement() {
    for link_persisted in [false, true] {
        let (dir, source, old, mut replacement, mut io) = fixture();
        let targets = vec![dir.path().join("new-movie.mkv")];
        io.fail_publish = true;
        assert!(replacement.run(&source, &targets, 11, &mut io).is_err());
        if link_persisted {
            super::super::links::record(dir.path(), &targets[0], &source).unwrap();
        }
        io.ownership_config = Some((dir.path().to_owned(), source.clone()));
        io.fail_publish = false;
        let mut recovered = io.saved.clone();
        recovered.run(&source, &targets, 11, &mut io).unwrap();
        assert_eq!(
            super::super::links::read(dir.path())
                .unwrap()
                .get(&targets[0]),
            Some(&source)
        );
        assert!(old.iter().all(|path| !path.exists()));
        assert!(source.exists());
    }
}

#[test]
fn disappearance_at_cleanup_entry_preserves_all_old_outputs() {
    let (dir, source, old, mut replacement, mut io) = fixture();
    let targets = vec![dir.path().join("movie.mkv")];
    io.remove_at_checkpoint = Some((3, targets[0].clone()));
    assert!(replacement.run(&source, &targets, 7, &mut io).is_err());
    assert!(old.iter().all(|p| p.exists()));
}

#[test]
fn all_candidates_verify_before_any_old_output_is_replaced_or_removed() {
    let (dir, source, old, mut replacement, mut io) = fixture();
    let targets = vec![old[0].clone(), dir.path().join("new-second.mkv")];
    io.fail_prepare = Some(1);
    assert!(replacement.run(&source, &targets, 1, &mut io).is_err());
    for path in &old {
        assert_eq!(std::fs::read(path).unwrap(), b"old episode");
    }
    assert!(!targets[1].exists());
    io.fail_prepare = None;
    let mut restarted = io.saved.clone();
    restarted.run(&source, &targets, 1, &mut io).unwrap();
    assert_eq!(io.prepares, vec![0, 1], "verified candidate is reused");
    assert_eq!(std::fs::read(&targets[0]).unwrap(), b"verified new 0");
    assert!(targets[1].exists());
    assert!(!old[1].exists() && !old[2].exists());
    assert_eq!(std::fs::read(source).unwrap(), b"source");
}

#[test]
fn restart_after_rename_recognizes_exact_file_and_finishes_cleanup() {
    let (dir, source, old, mut replacement, mut io) = fixture();
    let targets = vec![dir.path().join("movie.mkv")];
    io.fail_publish = true;
    assert!(replacement.run(&source, &targets, 2, &mut io).is_err());
    assert!(old.iter().all(|p| p.exists()));
    io.fail_publish = false;
    let mut restarted = io.saved.clone();
    restarted.run(&source, &targets, 2, &mut io).unwrap();
    assert_eq!(io.prepares, vec![0]);
    assert!(old.iter().all(|p| !p.exists()));
}

#[test]
fn changed_new_or_old_file_blocks_cleanup() {
    for change_new in [true, false] {
        let (dir, source, old, mut replacement, mut io) = fixture();
        let targets = vec![dir.path().join("movie.mkv")];
        io.fail_retire = true;
        assert!(replacement.run(&source, &targets, 3, &mut io).is_err());
        let changed = if change_new { &targets[0] } else { &old[0] };
        std::fs::write(changed, b"someone else's changed file").unwrap();
        io.fail_retire = false;
        let mut restarted = io.saved.clone();
        assert!(restarted.run(&source, &targets, 3, &mut io).is_err());
        assert!(old.iter().all(|p| p.exists()));
        assert_eq!(
            std::fs::read(changed).unwrap(),
            b"someone else's changed file"
        );
    }
}

#[test]
fn a_new_published_identity_is_rebound_inside_the_publishing_call() {
    let (dir, source, old, mut replacement, mut io) = fixture();
    let targets = vec![dir.path().join("movie.mkv")];
    io.publish_with_new_identity = true;
    replacement.run(&source, &targets, 4, &mut io).unwrap();
    assert!(old.iter().all(|p| !p.exists()));
    assert_eq!(std::fs::read(&targets[0]).unwrap(), b"verified new 0");
    let published = io.saved.prepared[0].published.clone().unwrap();
    published.verify(&targets[0]).unwrap();
    assert_ne!(
        io.saved.prepared[0]
            .identity
            .as_ref()
            .unwrap()
            .filesystem_id,
        published.filesystem_id
    );
}

#[test]
fn a_new_identity_found_only_after_a_crash_is_held() {
    let (dir, source, old, mut replacement, mut io) = fixture();
    let targets = vec![dir.path().join("movie.mkv")];
    io.publish_with_new_identity = true;
    io.fail_publish = true;
    assert!(replacement.run(&source, &targets, 5, &mut io).is_err());
    io.fail_publish = false;
    let mut restarted = io.saved.clone();
    assert!(restarted.run(&source, &targets, 5, &mut io).is_err());
    assert!(old.iter().all(|p| p.exists()));
    assert!(targets[0].exists());
}

#[test]
fn candidate_time_is_pinned_before_its_identity_is_journaled() {
    let (dir, source, _old, mut replacement, mut io) = fixture();
    let targets = vec![dir.path().join("movie.mkv")];
    replacement.run(&source, &targets, 6, &mut io).unwrap();
    let modified = io.saved.prepared[0].identity.as_ref().unwrap().modified;
    let since = modified.duration_since(std::time::UNIX_EPOCH).unwrap();
    assert_eq!((since.as_secs() % 2, since.subsec_nanos()), (0, 0));
}
