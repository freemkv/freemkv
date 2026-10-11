use super::*;
use crate::server::config::Config;

fn fixture() -> (tempfile::TempDir, PathBuf, Config, Manifest) {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("staging/job");
    let output = temp.path().join("output");
    let config = temp.path().join("config");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let marker = serde_json::json!({"title":"Show", "year":2024, "media_type":"movie"});
    std::fs::write(dir.join(".done"), serde_json::to_vec(&marker).unwrap()).unwrap();
    let mut moves = Vec::new();
    for name in ["episode1.mkv", "episode2.mkv", "disc.iso"] {
        let source = dir.join(name);
        std::fs::write(&source, b"retained test bytes").unwrap();
        moves.push((
            source,
            output
                .join(format!("owned_2_{name}"))
                .to_string_lossy()
                .into_owned(),
        ));
    }
    let cfg = Config {
        staging_dir: temp.path().join("staging").to_string_lossy().into_owned(),
        autorip_dir: config.to_string_lossy().into_owned(),
        output_dir: output.to_string_lossy().into_owned(),
        keep_iso: true,
        ..Default::default()
    };
    let manifest = Manifest::create(
        &dir,
        marker,
        Vec::new(),
        vec![output.to_string_lossy().into_owned()],
        &moves,
    )
    .unwrap();
    (temp, dir, cfg, manifest)
}

// Links are keyed by the destination as planned from settings, not its canonical form.
fn planned(entry: &Entry) -> PathBuf {
    PathBuf::from(entry.planned.clone().unwrap())
}

#[test]
fn partial_delivery_reuses_exact_plan_despite_changed_settings() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, mut cfg, manifest) = fixture();
    Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap();
    assert!(!manifest.entries[0].source.exists());
    cfg.keep_iso = false;
    cfg.output_dir = dir.join("wrong-new-root").to_string_lossy().into_owned();
    super::super::check_and_move(&cfg);
    assert!(
        !dir.exists(),
        "journaled batch must finish despite vanished first source"
    );
    let links = crate::server::library::links::load(Path::new(&cfg.autorip_dir));
    for entry in &manifest.entries[..2] {
        assert_eq!(
            links.get(&planned(entry)),
            Some(&planned(&manifest.entries[2]))
        );
        assert!(entry.destination.exists());
    }
}

#[test]
fn empty_media_directory_recovers_all_links_and_cleanup() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    for index in 0..3 {
        Manifest::deliver(&dir, index, &|_, _, _, _| {}).unwrap();
    }
    // Also reproduce a cleanup crash after the readiness marker was unlinked.
    std::fs::remove_file(dir.join(".done")).unwrap();
    super::super::check_and_move(&cfg);
    assert!(!dir.exists());
    let links = crate::server::library::links::load(Path::new(&cfg.autorip_dir));
    assert_eq!(links.len(), 2);
    assert_eq!(
        links.get(&planned(&manifest.entries[0])),
        Some(&planned(&manifest.entries[2]))
    );
}

#[test]
fn rename_before_completion_checkpoint_is_verified_not_recopied() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    Manifest::prepare(&dir, 0, &entry.source).unwrap();
    libfreemkv::io::publish::no_replace(&entry.source, &entry.destination).unwrap();
    assert!(!Manifest::load(&dir).unwrap().unwrap().entries[0].completed);
    let before = Identity::read(&entry.destination).unwrap();
    assert!(matches!(
        Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap(),
        super::super::MoveOutcome::Skipped
    ));
    assert_eq!(Identity::read(&entry.destination).unwrap(), before);
    assert!(Manifest::load(&dir).unwrap().unwrap().entries[0].completed);
}

#[test]
fn copied_candidate_identity_recovers_after_rename_with_source_retained() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    let candidate = entry.destination.with_extension("part");
    std::fs::copy(&entry.source, &candidate).unwrap();
    Manifest::prepare(&dir, 0, &candidate).unwrap();
    libfreemkv::io::publish::no_replace(&candidate, &entry.destination).unwrap();
    assert!(matches!(
        Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap(),
        super::super::MoveOutcome::Skipped
    ));
    assert!(entry.source.exists());
}

#[test]
fn stalled_publication_retries_exact_manifest_before_linked_cleanup() {
    use super::super::copy_monitor::{Activity, Monitor};
    use std::sync::{Arc, atomic::AtomicU64, mpsc};
    use std::time::{Duration, Instant};

    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    let candidate = entry.destination.with_extension("part");
    std::fs::copy(&entry.source, &candidate).unwrap();
    let prepared = Identity::read(&candidate).unwrap();
    Manifest::prepare(&dir, 0, &candidate).unwrap();
    let lease = crate::server::ripper::staging::job_lease(&dir);
    let owner = lease.clone();
    let job = dir.clone();
    let destination = entry.destination.clone();
    let (release_tx, release_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();
    let caller = std::thread::spawn(move || {
        let _owner = owner.lock().unwrap();
        let _targets = Manifest::load(&job)
            .unwrap()
            .unwrap()
            .lock_targets()
            .unwrap();
        let halt = libfreemkv::Halt::new();
        let activity = Activity::default();
        activity.set("publishing destination");
        let written = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let target = destination.clone();
        let worker = std::thread::spawn(move || {
            // Inject an already-entered publication: it cannot observe Halt
            // until the filesystem returns, and must not be detached.
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            tx.send(libfreemkv::io::publish::no_replace(&candidate, &target).map(|()| 19))
                .unwrap();
        });
        Monitor {
            job: &job,
            destination: &destination,
            written: &written,
            activity: &activity,
            halt: &halt,
            progress: &|_, _, _, _| {},
            source_size: 19,
            stall_window: Duration::from_millis(30),
            poll_interval: Duration::from_millis(2),
        }
        .wait(rx, worker)
    });
    entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let key = dir.to_string_lossy().into_owned();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !super::super::MOVE_ERRORS
        .lock()
        .unwrap()
        .get(&key)
        .is_some_and(|e| e.worker_active)
    {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(lease.try_lock().is_err());
    assert!(entry.source.exists());
    assert!(!entry.destination.exists());
    release_tx.send(()).unwrap();
    let failure = caller.join().unwrap().unwrap_err();
    assert!(matches!(
        super::super::copy_failure_outcome(&failure),
        super::super::MoveOutcome::Stalled
    ));
    assert!(entry.source.exists());
    assert_eq!(Identity::read(&entry.destination).unwrap(), prepared);
    assert!(!Manifest::load(&dir).unwrap().unwrap().entries[0].completed);

    // A corrupt link map blocks cleanup after real retry reconciliation.
    let links_path = Path::new(&cfg.autorip_dir).join(crate::server::library::links::LINKS_FILE);
    std::fs::write(&links_path, b"broken").unwrap();
    super::super::clear_move_error(&key);
    super::super::check_and_move(&cfg);
    assert!(
        Manifest::load(&dir).unwrap().unwrap().entries[0].completed,
        "retry must adopt the exact prepared publication"
    );
    assert!(
        entry.source.exists(),
        "links must be durable before source cleanup"
    );
    assert!(dir.join(FILE).exists());
    assert_eq!(Identity::read(&entry.destination).unwrap(), prepared);
    assert_eq!(std::fs::read(&links_path).unwrap(), b"broken");

    std::fs::write(&links_path, b"{}").unwrap();
    super::super::clear_move_error(&key);
    super::super::check_and_move(&cfg);
    assert!(!dir.exists());
    let links = crate::server::library::links::load(Path::new(&cfg.autorip_dir));
    assert_eq!(links.len(), 2);
    for entry in &manifest.entries[..2] {
        assert_eq!(
            links.get(&planned(entry)),
            Some(&planned(&manifest.entries[2]))
        );
    }
    assert_eq!(Identity::read(&entry.destination).unwrap(), prepared);
    // An additional pass cannot republish, delete the final, or create variants.
    super::super::check_and_move(&cfg);
    assert_eq!(Identity::read(&entry.destination).unwrap(), prepared);
    assert_eq!(std::fs::read_dir(&manifest.roots[0]).unwrap().count(), 3);
}

#[test]
fn equal_bytes_foreign_destination_cannot_be_adopted_or_linked() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    Manifest::prepare(&dir, 0, &entry.source).unwrap();
    std::fs::copy(&entry.source, &entry.destination).unwrap();
    assert!(Manifest::deliver(&dir, 0, &|_, _, _, _| {}).is_err());
    assert!(entry.source.exists());
    assert!(!Manifest::load(&dir).unwrap().unwrap().entries[0].completed);
}

#[test]
fn cleanup_preserves_replaced_or_new_staging_files_and_journal() {
    let (_temp, dir, _cfg, _) = fixture();
    for index in 0..3 {
        Manifest::deliver(&dir, index, &|_, _, _, _| {}).unwrap();
    }
    let manifest = Manifest::load(&dir).unwrap().unwrap();
    std::fs::write(dir.join("new-user-file"), b"do not delete").unwrap();
    assert!(manifest.cleanup().is_err());
    assert!(dir.join(FILE).exists());
    assert_eq!(
        std::fs::read(dir.join("new-user-file")).unwrap(),
        b"do not delete"
    );
}

#[test]
fn unreadable_manifest_never_becomes_a_new_delivery() {
    let (_temp, dir, _cfg, manifest) = fixture();
    std::fs::write(dir.join(FILE), b"broken").unwrap();
    assert!(Manifest::load(&dir).is_err());
    assert!(
        manifest
            .entries
            .iter()
            .all(|e| e.source.exists() && !e.destination.exists())
    );
}

#[test]
fn partial_failure_retry_links_the_whole_batch_without_variant_drift() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    let blocked = &manifest.entries[1].destination;
    std::fs::write(blocked, b"foreign winner").unwrap();
    super::super::check_and_move(&cfg);
    assert!(!manifest.entries[0].source.exists());
    assert!(manifest.entries[1].source.exists());
    assert!(!manifest.entries[2].source.exists());
    assert_eq!(std::fs::read(blocked).unwrap(), b"foreign winner");
    assert!(crate::server::library::links::load(Path::new(&cfg.autorip_dir)).is_empty());
    std::fs::remove_file(blocked).unwrap();
    super::super::clear_move_error(&dir.to_string_lossy());
    super::super::check_and_move(&cfg);
    assert!(!dir.exists());
    let links = crate::server::library::links::load(Path::new(&cfg.autorip_dir));
    assert_eq!(links.len(), 2);
    for entry in &manifest.entries[..2] {
        assert_eq!(
            links.get(&planned(entry)),
            Some(&planned(&manifest.entries[2]))
        );
    }
}

#[test]
fn failed_prepublication_checkpoint_preserves_source_and_destination_absence() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let blocked = dir.join(format!("{FILE}.tmp"));
    std::fs::create_dir(&blocked).unwrap();
    assert!(matches!(
        Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap(),
        super::super::MoveOutcome::Failed
    ));
    assert!(manifest.entries[0].source.exists());
    assert!(!manifest.entries[0].destination.exists());
    std::fs::remove_dir(blocked).unwrap();
    Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap();
    assert!(!manifest.entries[0].source.exists());
}

#[test]
fn replacing_a_completed_destination_holds_linkage_and_cleanup() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    for index in 0..3 {
        Manifest::deliver(&dir, index, &|_, _, _, _| {}).unwrap();
    }
    let destination = &manifest.entries[0].destination;
    let retained = destination.with_extension("original");
    std::fs::rename(destination, &retained).unwrap();
    std::fs::copy(&retained, destination).unwrap();
    super::super::check_and_move(&cfg);
    assert!(dir.join(FILE).exists());
    assert!(destination.exists());
    assert!(crate::server::library::links::load(Path::new(&cfg.autorip_dir)).is_empty());
}

#[test]
fn changed_filesystem_identity_is_an_explicit_hold_even_with_unchanged_bytes() {
    let (_temp, dir, _cfg, _) = fixture();
    Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap();
    let mut manifest = Manifest::load(&dir).unwrap().unwrap();
    let destination = manifest.entries[0].destination.clone();
    let bytes = std::fs::read(&destination).unwrap();
    // Simulate the identity discontinuity reported after an SMB remount.
    let entry = &mut manifest.entries[0];
    for recorded in [entry.prepared.as_mut(), entry.published.as_mut()]
        .into_iter()
        .flatten()
    {
        recorded.file_id.0 = recorded.file_id.0.wrapping_add(1);
    }
    manifest.save().unwrap();
    assert!(Manifest::deliver(&dir, 0, &|_, _, _, _| {}).is_err());
    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    assert!(dir.join(FILE).exists());
}

#[test]
fn replaced_destination_parent_is_not_rebound_by_its_path() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    let root = Path::new(&manifest.roots[0]);
    std::fs::rename(root, root.with_file_name("old-output")).unwrap();
    std::fs::create_dir(root).unwrap();
    super::super::check_and_move(&cfg);
    assert!(
        manifest
            .entries
            .iter()
            .all(|entry| entry.source.exists() && !entry.destination.exists())
    );
    assert!(dir.join(FILE).exists());
    assert!(crate::server::library::links::load(Path::new(&cfg.autorip_dir)).is_empty());
}

#[test]
fn changed_staging_directory_identity_refuses_journal_recovery() {
    let (_temp, dir, _cfg, mut manifest) = fixture();
    manifest.directory_id.0 = manifest.directory_id.0.wrapping_add(1);
    manifest.save().unwrap();
    assert!(Manifest::load(&dir).is_err());
    assert!(manifest.entries.iter().all(|entry| entry.source.exists()));
}

#[test]
fn copied_source_is_retained_when_final_or_source_identity_changes_before_cleanup() {
    let (_temp, dir, _cfg, manifest) = fixture();
    for (index, entry) in manifest.entries.iter().enumerate() {
        let candidate = entry.destination.with_extension("part");
        std::fs::copy(&entry.source, &candidate).unwrap();
        Manifest::prepare(&dir, index, &candidate).unwrap();
        libfreemkv::io::publish::no_replace(&candidate, &entry.destination).unwrap();
        Manifest::deliver(&dir, index, &|_, _, _, _| {}).unwrap();
    }
    let completed = Manifest::load(&dir).unwrap().unwrap();
    let source = &manifest.entries[0].source;
    let retained = source.with_extension("original");
    std::fs::rename(source, &retained).unwrap();
    std::fs::write(source, b"foreign staged replacement").unwrap();
    assert!(completed.cleanup().is_err());
    assert_eq!(
        std::fs::read(source).unwrap(),
        b"foreign staged replacement"
    );
    assert!(dir.join(FILE).exists());
    let destination = &manifest.entries[1].destination;
    std::fs::rename(destination, destination.with_extension("original")).unwrap();
    std::fs::write(destination, b"foreign final replacement").unwrap();
    assert!(completed.cleanup().is_err());
    assert!(manifest.entries[1].source.exists());
}

#[test]
fn target_lock_set_excludes_a_cooperating_writer_until_released() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let locks = manifest.lock_targets().unwrap();
    let target = manifest.entries[0].destination.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let other = std::thread::spawn(move || {
        let lock =
            libfreemkv::io::ArtifactLock::acquire(&target, &[], &libfreemkv::Halt::new()).unwrap();
        tx.send(()).unwrap();
        lock
    });
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(100))
            .is_err()
    );
    assert!(dir.join(FILE).exists());
    drop(locks);
    rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    drop(other.join().unwrap());
}

// smbfs against Samba's fruit:zero_file_id=yes derives ids from the name.
struct NameDerived(PathBuf);

impl NameDerived {
    fn register(root: &Path) -> Self {
        let root = root.canonicalize().unwrap();
        NAME_DERIVED.lock().unwrap().push(root.clone());
        Self(root)
    }
}

impl Drop for NameDerived {
    fn drop(&mut self) {
        NAME_DERIVED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|root| root != &self.0);
    }
}

#[test]
fn name_derived_ids_are_rebound_inside_the_publishing_call() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    let _ids = NameDerived::register(Path::new(&manifest.roots[0]));
    for index in 0..3 {
        assert!(matches!(
            Manifest::deliver(&dir, index, &|_, _, _, _| {}).unwrap(),
            super::super::MoveOutcome::Moved
        ));
    }
    let saved = Manifest::load(&dir).unwrap().unwrap();
    assert!(
        saved
            .entries
            .iter()
            .all(|e| e.completed && e.published.is_some())
    );
    saved.verify_delivered().unwrap();
    super::super::check_and_move(&cfg);
    assert!(!dir.exists(), "a rebound delivery links and cleans up");
    assert!(manifest.entries.iter().all(|e| e.destination.exists()));
}

#[test]
fn a_name_derived_mismatch_found_by_a_later_call_is_held() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let _ids = NameDerived::register(Path::new(&manifest.roots[0]));
    let entry = &manifest.entries[0];
    let candidate = entry.destination.with_extension("part");
    std::fs::copy(&entry.source, &candidate).unwrap();
    Manifest::prepare(&dir, 0, &candidate).unwrap();
    libfreemkv::io::publish::no_replace(&candidate, &entry.destination).unwrap();
    assert!(Manifest::deliver(&dir, 0, &|_, _, _, _| {}).is_err());
    assert!(entry.source.exists());
    assert!(!Manifest::load(&dir).unwrap().unwrap().entries[0].completed);
}

#[test]
fn a_publication_reported_as_stalled_is_still_rebound_then_completed_on_retry() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let _ids = NameDerived::register(Path::new(&manifest.roots[0]));
    let entry = &manifest.entries[0];
    let candidate = entry.destination.with_extension("part");
    std::fs::copy(&entry.source, &candidate).unwrap();
    Manifest::prepare(&dir, 0, &candidate).unwrap();
    libfreemkv::io::publish::no_replace(&candidate, &entry.destination).unwrap();
    // The worker published, but the monitor reported a stall.
    Manifest::settle(&dir, 0, true, false).unwrap();
    let saved = Manifest::load(&dir).unwrap().unwrap();
    assert!(saved.entries[0].published.is_some() && !saved.entries[0].completed);
    assert!(matches!(
        Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap(),
        super::super::MoveOutcome::Skipped
    ));
    assert!(Manifest::load(&dir).unwrap().unwrap().entries[0].completed);
}

#[test]
fn a_publication_whose_size_changed_is_not_rebound() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    let candidate = entry.destination.with_extension("part");
    std::fs::copy(&entry.source, &candidate).unwrap();
    Manifest::prepare(&dir, 0, &candidate).unwrap();
    libfreemkv::io::publish::no_replace(&candidate, &entry.destination).unwrap();
    std::fs::write(&entry.destination, b"shadowed").unwrap();
    assert!(Manifest::settle(&dir, 0, true, true).is_err());
    let saved = Manifest::load(&dir).unwrap().unwrap();
    assert!(saved.entries[0].published.is_none() && !saved.entries[0].completed);
}

#[test]
fn a_zero_length_destination_is_held_as_interrupted_publication() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    std::fs::write(&entry.destination, b"").unwrap();
    let error = Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap_err();
    assert!(error.to_string().contains("zero-length"), "{error}");
    assert_eq!(std::fs::metadata(&entry.destination).unwrap().len(), 0);
    assert!(entry.source.exists());
}

#[cfg(unix)]
#[test]
fn a_failed_delivery_keeps_its_backoff_across_a_restart() {
    use std::os::unix::fs::PermissionsExt;
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    let key = dir.to_string_lossy().into_owned();
    let source = &manifest.entries[0].source;
    // prepare() opens the candidate read-write: a read-only source fails it.
    std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o444)).unwrap();
    super::super::check_and_move(&cfg);
    assert!(source.exists() && !manifest.entries[0].destination.exists());
    assert_eq!(
        Manifest::load(&dir)
            .unwrap()
            .unwrap()
            .retry()
            .unwrap()
            .attempts,
        1
    );
    std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o644)).unwrap();

    super::super::reset_after_drain();
    super::super::check_and_move(&cfg);
    assert!(
        source.exists() && !manifest.entries[0].destination.exists(),
        "a restart must not buy another copy before the backoff expires"
    );
    let row = super::super::MOVE_ERRORS
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .unwrap();
    assert_eq!(row.attempts, 1);
    assert!(row.retry_after.is_some() && !row.retry_held);

    super::super::clear_all_move_errors();
    super::super::check_and_move(&cfg);
    assert!(!dir.exists(), "an operator Retry delivers and cleans up");
}

#[test]
fn a_held_delivery_stays_held_across_a_restart_until_retry() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    let key = dir.to_string_lossy().into_owned();
    let held = RetryState {
        attempts: 3,
        not_before: None,
        reason: "destination does not support a required delivery operation".into(),
        hint: "fix the destination, then choose Retry".into(),
    };
    Manifest::record_retry(&dir, Some(held.clone())).unwrap();
    super::super::reset_after_drain();
    super::super::check_and_move(&cfg);
    assert!(
        manifest
            .entries
            .iter()
            .all(|e| e.source.exists() && !e.destination.exists())
    );
    let row = super::super::MOVE_ERRORS
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .unwrap();
    assert!(row.retry_held && row.retry_after.is_none());
    assert_eq!(
        (row.reason.as_str(), row.attempts),
        (held.reason.as_str(), 3)
    );
    // Without Retry, later ticks stay held.
    super::super::check_and_move(&cfg);
    assert!(manifest.entries[0].source.exists());

    super::super::clear_move_error(&key);
    super::super::check_and_move(&cfg);
    assert!(!dir.exists(), "Retry clears the durable hold and delivers");
}

#[test]
fn a_busy_destination_lock_is_reported_instead_of_blocking_the_mover() {
    let (_temp, _dir, _cfg, manifest) = fixture();
    let held = libfreemkv::io::ArtifactLock::acquire(
        &manifest.entries[1].destination,
        &[],
        &libfreemkv::Halt::new(),
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = manifest.clone();
    std::thread::spawn(move || {
        let _ = tx.send(waiter.lock_targets_within(std::time::Duration::from_millis(200)));
    });
    let result = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("lock_targets must give up and report a busy lock");
    let error = result.expect_err("the held lock is reported");
    assert_eq!(error.kind(), io::ErrorKind::ResourceBusy, "{error}");
    drop(held);
    let locks = manifest.lock_targets().unwrap();
    assert_eq!(locks.len(), 3);
}

#[cfg(unix)]
#[test]
fn links_and_moves_keep_the_configured_destination_form() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("nas/media");
    std::fs::create_dir_all(&real).unwrap();
    let configured = temp.path().join("media");
    std::os::unix::fs::symlink(&real, &configured).unwrap();
    let dir = temp.path().join("staging/job");
    std::fs::create_dir_all(&dir).unwrap();
    let config = temp.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let marker = serde_json::json!({"title":"Show", "year":2024, "media_type":"movie"});
    std::fs::write(dir.join(".done"), serde_json::to_vec(&marker).unwrap()).unwrap();
    let mut moves = Vec::new();
    for name in ["movie.mkv", "disc.iso"] {
        std::fs::write(dir.join(name), b"bytes").unwrap();
        let planned = configured.join(name).to_string_lossy().into_owned();
        moves.push((dir.join(name), planned));
    }
    let manifest = Manifest::create(
        &dir,
        marker,
        Vec::new(),
        vec![configured.to_string_lossy().into_owned()],
        &moves,
    )
    .unwrap();
    assert_eq!(
        manifest.moves().iter().map(|m| &m.1).collect::<Vec<_>>(),
        moves.iter().map(|m| &m.1).collect::<Vec<_>>()
    );
    let cfg = Config {
        staging_dir: temp.path().join("staging").to_string_lossy().into_owned(),
        autorip_dir: config.to_string_lossy().into_owned(),
        output_dir: configured.to_string_lossy().into_owned(),
        keep_iso: true,
        ..Default::default()
    };
    super::super::check_and_move(&cfg);
    assert!(!dir.exists());
    let links = crate::server::library::links::load(&config);
    assert_eq!(
        links.get(&configured.join("movie.mkv")),
        Some(&configured.join("disc.iso")),
        "{links:?}"
    );
}

#[test]
fn an_unowned_destination_stays_a_collision_on_every_retry() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[1];
    std::fs::write(&entry.destination, b"foreign winner").unwrap();
    for _ in 0..2 {
        assert!(matches!(
            Manifest::deliver(&dir, 1, &|_, _, _, _| {}).unwrap(),
            super::super::MoveOutcome::Collision
        ));
    }
    assert_eq!(
        std::fs::read(&entry.destination).unwrap(),
        b"foreign winner"
    );
    assert!(entry.source.exists());
}

#[test]
fn a_held_journal_names_its_reason_in_the_error_row() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, manifest) = fixture();
    std::fs::write(&manifest.entries[0].destination, b"").unwrap();
    super::super::check_and_move(&cfg);
    let key = dir.to_string_lossy().into_owned();
    let row = super::super::MOVE_ERRORS
        .lock()
        .unwrap()
        .get(&key)
        .cloned()
        .unwrap();
    assert!(row.reason.contains("zero-length"), "{}", row.reason);
    assert!(
        row.hint.contains("never removed automatically"),
        "{}",
        row.hint
    );
    assert_eq!(
        std::fs::metadata(&manifest.entries[0].destination)
            .unwrap()
            .len(),
        0
    );
    super::super::clear_move_error(&key);
}

#[test]
fn a_file_arriving_during_cleanup_keeps_the_journal() {
    let (_temp, dir, _cfg, _) = fixture();
    for index in 0..3 {
        Manifest::deliver(&dir, index, &|_, _, _, _| {}).unwrap();
    }
    let manifest = Manifest::load(&dir).unwrap().unwrap();
    let late = dir.join(".nfs0000000000001");
    let error = manifest
        .cleanup_with(&|| std::fs::write(&late, b"silly rename").unwrap())
        .unwrap_err();
    assert!(error.to_string().contains("unplanned"), "{error}");
    assert!(dir.join(FILE).exists(), "the journal keeps the retry path");
    assert_eq!(std::fs::read(&late).unwrap(), b"silly rename");
}

#[test]
fn journal_recovery_clears_a_stale_destination_error() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    Manifest::prepare(&dir, 0, &entry.source).unwrap();
    libfreemkv::io::publish::no_replace(&entry.source, &entry.destination).unwrap();
    let key = entry.destination.to_string_lossy().into_owned();
    super::super::record_error_with(&key, "publication directory sync failed", "", |_| {});
    assert!(matches!(
        Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap(),
        super::super::MoveOutcome::Skipped
    ));
    assert!(!super::super::MOVE_ERRORS.lock().unwrap().contains_key(&key));
}

#[test]
fn the_move_webhook_is_owed_until_linkage_succeeds() {
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_temp, dir, cfg, _) = fixture();
    let key = dir.to_string_lossy().into_owned();
    let links_path = Path::new(&cfg.autorip_dir).join(crate::server::library::links::LINKS_FILE);
    std::fs::write(&links_path, b"broken").unwrap();
    super::super::check_and_move(&cfg);
    assert!(!Manifest::load(&dir).unwrap().unwrap().announced());
    // Linkage recovers on Retry, while cleanup is held by an unplanned file.
    std::fs::write(&links_path, b"{}").unwrap();
    std::fs::write(dir.join("unplanned"), b"keep").unwrap();
    super::super::clear_move_error(&key);
    super::super::check_and_move(&cfg);
    assert!(
        Manifest::load(&dir).unwrap().unwrap().announced(),
        "a delivery whose files were all moved on an earlier tick is still announced"
    );
    super::super::clear_move_error(&key);
}

fn temporary(destination: &Path, pid: u32) -> PathBuf {
    let mut name = destination.as_os_str().to_owned();
    name.push(format!(".part-{pid}"));
    PathBuf::from(name)
}

#[test]
fn an_interrupted_copy_left_on_the_share_is_removed_before_the_next_attempt() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    let residue = temporary(&entry.destination, 44447);
    std::fs::write(&residue, b"half a copy").unwrap();
    let unrelated = entry
        .destination
        .with_file_name("owned_2_episode1.mkv.part-notapid");
    std::fs::write(&unrelated, b"not ours").unwrap();
    assert!(matches!(
        Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap(),
        super::super::MoveOutcome::Moved
    ));
    assert!(!residue.exists(), "our own interrupted temporary is swept");
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"not ours");
}

#[test]
fn a_residue_that_cannot_be_removed_holds_the_delivery_and_names_it() {
    let (_temp, dir, _cfg, manifest) = fixture();
    let entry = &manifest.entries[0];
    let residue = temporary(&entry.destination, 51515);
    std::fs::create_dir(&residue).unwrap();
    std::fs::write(residue.join("inside"), b"x").unwrap();
    let error = Manifest::deliver(&dir, 0, &|_, _, _, _| {}).unwrap_err();
    assert!(error.to_string().contains(".part-51515"), "{error}");
    assert!(residue.join("inside").exists());
    assert!(entry.source.exists() && !entry.destination.exists());
}

// LIVE: FREEMKV_LIVE_SMB_DIR=/Volumes/media cargo test --features server --lib
// live_smb -- --ignored --nocapture. Works in a scratch dir it always removes.
#[cfg(unix)]
#[test]
#[ignore = "needs FREEMKV_LIVE_SMB_DIR on a network share"]
fn live_smb_publication_and_journaled_delivery() {
    use std::os::unix::fs::MetadataExt;
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let removed = std::fs::remove_dir_all(&self.0);
            eprintln!("live: removed {} -> {removed:?}", self.0.display());
        }
    }
    let Some(share) = std::env::var_os("FREEMKV_LIVE_SMB_DIR") else {
        eprintln!("live: FREEMKV_LIVE_SMB_DIR unset; skipped");
        return;
    };
    let _guard = super::super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = Path::new(&share).join(format!(".fmkv-live-{}-{nonce:x}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let scratch = Scratch(root.canonicalize().unwrap());
    let root = scratch.0.clone();

    // 1. A real no_replace on the share, then a collision.
    let (src, dst) = (root.join("publish.src"), root.join("publish.mkv"));
    std::fs::write(&src, b"published bytes").unwrap();
    let src_ino = std::fs::metadata(&src).unwrap().ino();
    libfreemkv::io::publish::no_replace(&src, &dst).unwrap();
    let dst_ino = std::fs::metadata(&dst).unwrap().ino();
    assert_eq!(std::fs::read(&dst).unwrap(), b"published bytes");
    assert!(!src.exists());
    eprintln!("live: no_replace ok; ino {src_ino} -> {dst_ino}");
    let late = root.join("late.src");
    std::fs::write(&late, b"late").unwrap();
    let error = libfreemkv::io::publish::no_replace(&late, &dst).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error:?}");
    assert_eq!(std::fs::read(&dst).unwrap(), b"published bytes");
    assert_eq!(std::fs::read(&late).unwrap(), b"late");
    eprintln!("live: no_replace collision -> {:?}", error.kind());

    // 2. A journaled mover delivery from local staging (copy path) to the share.
    let local = tempfile::tempdir().unwrap();
    let job = local.path().join("staging/Live");
    std::fs::create_dir_all(&job).unwrap();
    let output = root.join("Movies");
    std::fs::create_dir(&output).unwrap();
    let mut rip = vec![0x1A, 0x45, 0xDF, 0xA3];
    rip.extend((0..4 * 1024 * 1024u32).map(|i| (i % 251) as u8));
    let source = job.join("Live.mkv");
    std::fs::write(&source, &rip).unwrap();
    let destination = output.join("Live (2026).mkv");
    let plan = |job: &Path, source: &Path| {
        Manifest::create(
            job,
            serde_json::json!({"title": "Live"}),
            Vec::new(),
            vec![output.to_string_lossy().into_owned()],
            &[(
                source.to_path_buf(),
                destination.to_string_lossy().into_owned(),
            )],
        )
    };
    let manifest = plan(&job, &source).unwrap();
    let locks = manifest.lock_targets().unwrap();
    let outcome = Manifest::deliver(&job, 0, &|_, _, _, _| {}).unwrap();
    assert!(
        matches!(outcome, super::super::MoveOutcome::Moved),
        "{outcome:?}"
    );
    let delivered = Manifest::load(&job).unwrap().unwrap();
    let entry = &delivered.entries[0];
    let (prepared, published) = (
        entry.prepared.clone().unwrap(),
        entry.published.clone().unwrap(),
    );
    eprintln!(
        "live: delivered {outcome:?}; prepared id {:?} mtime {:?}; published id {:?} mtime {:?}",
        prepared.file_id, prepared.modified, published.file_id, published.modified
    );
    assert!(entry.completed);
    assert_eq!(std::fs::read(&destination).unwrap(), rip);
    delivered.verify_delivered().unwrap();
    delivered.cleanup().unwrap();
    for lock in locks {
        lock.delete().unwrap();
    }
    assert!(!job.exists(), "journal completed and staging cleaned up");
    eprintln!("live: journal completed, staging removed");

    // 3. A second delivery onto the existing target holds as a collision.
    let job2 = local.path().join("staging/Live2");
    std::fs::create_dir_all(&job2).unwrap();
    let second = job2.join("Live.mkv");
    std::fs::write(&second, b"second disc").unwrap();
    let refused = plan(&job2, &second).err().unwrap();
    assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
    let winner = output.join("Race (2026).mkv");
    let racing = winner.clone();
    let outcome = super::super::move_file(
        &second,
        &winner,
        &|_, _, _, _| {},
        Arc::new(move |_| std::fs::write(&racing, b"winner")),
        &job2,
        Default::default(),
    );
    assert!(
        matches!(outcome, super::super::MoveOutcome::Collision),
        "{outcome:?}"
    );
    assert_eq!(std::fs::read(&winner).unwrap(), b"winner");
    assert_eq!(std::fs::read(&destination).unwrap(), rip);
    assert_eq!(std::fs::read(&second).unwrap(), b"second disc");
    eprintln!(
        "live: plan onto existing -> {:?}; publish race -> {outcome:?}",
        refused.kind()
    );
}
