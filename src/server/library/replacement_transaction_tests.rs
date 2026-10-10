use super::*;

#[test]
fn confirmed_legacy_three_to_one_and_one_to_three_keep_unselected_files() {
    for (old_count, new_count) in [(3, 1), (1, 3)] {
        let (dir, source, old, _, mut io) = fixture();
        let mut replacement = Replacement::capture(&source, &Default::default()).unwrap();
        replacement.bind_roots(&[dir.path().to_owned()]).unwrap();
        for path in old.iter().take(old_count) {
            replacement
                .confirm(Replacement::candidate(path).unwrap())
                .unwrap();
        }
        let untouched = dir.path().join("unrelated.mkv");
        std::fs::write(&untouched, b"unrelated").unwrap();
        let targets: Vec<_> = (0..new_count)
            .map(|i| dir.path().join(format!("new-{i}.mkv")))
            .collect();
        io.saved = replacement.clone();
        io.fail_prepare = Some(new_count - 1);
        assert!(replacement.run(&source, &targets, 9, &mut io).is_err());
        assert!(old.iter().all(|path| path.exists()));
        io.fail_prepare = None;
        let mut recovered: Replacement =
            serde_json::from_slice(&serde_json::to_vec(&io.saved).unwrap()).unwrap();
        assert!(
            recovered
                .old_outputs
                .iter()
                .all(|old| old.ownership == Ownership::UserConfirmed)
        );
        recovered.run(&source, &targets, 9, &mut io).unwrap();
        assert!(old.iter().take(old_count).all(|path| !path.exists()));
        assert!(old.iter().skip(old_count).all(|path| path.exists()));
        assert!(targets.iter().all(|path| path.exists()));
        assert_eq!(std::fs::read(&untouched).unwrap(), b"unrelated");
        assert!(source.exists());
    }
}

struct TestIo {
    saved: Replacement,
    fail_prepare: Option<usize>,
    fail_publish: bool,
    fail_retire: bool,
    prepares: Vec<usize>,
    checkpoints: usize,
    remove_at_checkpoint: Option<(usize, PathBuf)>,
    ownership_config: Option<(PathBuf, PathBuf)>,
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
    ) -> io::Result<()> {
        if let Some(before) = before {
            before.verify(target)?;
        } else if target.exists() {
            return Err(io::Error::other("target collision"));
        }
        std::fs::rename(candidate, target)?;
        if self.fail_publish {
            return Err(io::Error::other("crash after rename"));
        }
        Ok(())
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
