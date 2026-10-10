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
            links.get(&entry.destination),
            Some(&manifest.entries[2].destination)
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
        links.get(&manifest.entries[0].destination),
        Some(&manifest.entries[2].destination)
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
            links.get(&entry.destination),
            Some(&manifest.entries[2].destination)
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
            links.get(&entry.destination),
            Some(&manifest.entries[2].destination)
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
    let recorded = manifest.entries[0].prepared.as_mut().unwrap();
    recorded.file_id.0 = recorded.file_id.0.wrapping_add(1);
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
