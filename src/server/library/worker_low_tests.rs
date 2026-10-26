use super::*;
use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};

fn two_episodes() -> RemuxPlan {
    RemuxPlan {
        media: MediaMetadata::default(),
        outputs: (1..=2)
            .map(|n| PlannedOutput {
                id: format!("episode-{n}"),
                title_index: n,
                episode: Some(n as u16),
                episode_name: String::new(),
                filename: format!("Show_S01E0{n}.mkv"),
            })
            .collect(),
    }
}

#[test]
fn a_changed_library_file_is_named_by_the_output_that_changed_not_the_row() {
    let (t, lib, dirs) = library_with(&[]);
    lib.queue.add(vec![NewJob {
        title: "Show".into(),
        iso: "/i/Show.iso".into(),
        target: dirs.library.join("Show/Show.mkv"),
        replace: false,
    }]);
    let job = lib.queue.claim_next().unwrap();
    assert!(lib.queue.set_plan(job.id, two_episodes()));
    let kept = t.path().join("stage/Show_S01E02.0123456789ab.staged.mkv");
    lib.queue
        .set_output_staged(job.id, "episode-2", Some(kept.clone()));
    lib.queue.set_staged(
        job.id,
        Some(StagedFile {
            path: kept,
            bytes: 1,
            attempts: 1,
        }),
    );
    let job = lib.queue.snapshot().jobs[0].clone();
    let episode = job.outputs[1].target.clone();
    let e: std::io::Error = libfreemkv::Error::RemuxTargetExists {
        path: episode.display().to_string(),
    }
    .into();
    finish(
        &lib,
        &job,
        Ending::Failed(e),
        Duration::ZERO,
        &Lines(Mutex::new(Vec::new())),
    );
    let failure = lib.queue.snapshot().jobs[0].failure.clone().unwrap();
    let row = job.target.display().to_string();
    let changed = format!("The library file {}", episode.display());
    assert!(failure.message.contains(&changed), "{}", failure.message);
    assert!(!failure.message.contains(&row), "{}", failure.message);
}

#[test]
fn a_cleared_field_that_was_already_clear_writes_nothing() {
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: "/i/A.iso".into(),
        target: dirs.library.join("A/A.mkv"),
        replace: true,
    }]);
    let job = lib.queue.claim_next().unwrap();
    assert!(lib.queue.set_plan(job.id, two_episodes()));
    let before = lib.queue.generation();
    lib.queue.set_output_staged(job.id, "episode-1", None);
    lib.queue.set_staged(job.id, None);
    assert_eq!(lib.queue.generation(), before);
    lib.queue
        .finish_output(job.id, "episode-1", OutputState::Done);
    let done = lib.queue.generation();
    assert!(done > before);
    lib.queue
        .finish_output(job.id, "episode-1", OutputState::Done);
    assert_eq!(lib.queue.generation(), done);
}

#[cfg(unix)]
#[test]
fn a_plan_that_cannot_be_saved_says_so_and_keeps_its_storage_fault() {
    use std::os::unix::fs::PermissionsExt;
    let (t, lib, dirs) = library_with(&["A"]);
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: "/i/A.iso".into(),
        target: dirs.library.join("A/A.mkv"),
        replace: true,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    let config = t.path().join("config");
    let permissions = std::fs::metadata(&config).unwrap().permissions();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Probe the filesystem independently of the behavior under test, while locked.
    let ignores_modes = std::fs::write(config.join("permission-probe"), b"").is_ok();
    let cfg = Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let result = persist_auto_plan(&lib, &cfg, &mut job, two_episodes());
    std::fs::set_permissions(&config, permissions).unwrap();
    if ignores_modes {
        return; // running as a user that ignores modes
    }
    let e = result.expect_err("saving a plan must fail in an unwritable config directory");
    assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "{e}");
    assert!(e.to_string().contains("library-queue.json"), "{e}");
}

#[test]
fn a_scan_that_overlaps_a_landing_keeps_both_the_landing_and_what_it_found() {
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    let landed_at = lib.landed.load(Ordering::SeqCst);
    // A rip filed B while the scan ran; the scan saw it.
    let b = dirs.library.join("B/B.mkv");
    std::fs::create_dir_all(b.parent().unwrap()).unwrap();
    std::fs::write(&b, movie()).unwrap();
    let scan = lib.scan(&dirs);
    let landed = dirs.library.join("C/C.mkv");
    std::fs::create_dir_all(landed.parent().unwrap()).unwrap();
    std::fs::write(&landed, movie()).unwrap();
    lib.note_landed(&landed, Some(current_stamp()));
    lib.commit_scan(scan, landed_at);
    let snap = lib.snapshot();
    assert!(snap.sigs.contains_key(&b), "the scan was kept");
    assert!(snap.sigs.contains_key(&landed), "the landing survived it");
}
