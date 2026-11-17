//! `classify_resume` over every hint a startup sweep can hand it.
use super::staging::{ResumeAction, StagingResumeHint};
use super::*;

fn hint(dir: &Path, action: ResumeAction) -> StagingResumeHint {
    StagingResumeHint {
        dir: dir.to_path_buf(),
        action,
    }
}

fn preserved(has_iso: bool, has_mapfile: bool) -> ResumeAction {
    ResumeAction::ResumePreserved {
        attempt: 1,
        has_iso,
        has_mapfile,
        has_mkv: false,
    }
}

fn not_eligible(class: ResumeClass) -> bool {
    matches!(class, ResumeClass::NotEligible)
}

#[test]
fn terminal_and_live_hints_never_remux() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(matches!(
        classify_resume(&hint(tmp.path(), ResumeAction::AlreadyCompleted), 0),
        ResumeClass::AlreadyCompleted
    ));
    for action in [
        ResumeAction::AlreadyFailed { reason: "a".into() },
        ResumeAction::RestartLoopFailed { reason: "a".into() },
        ResumeAction::HeldUnreadableState { reason: "a".into() },
    ] {
        match classify_resume(&hint(tmp.path(), action), 0) {
            ResumeClass::AlreadyFailed { reason } => assert_eq!(reason, "a"),
            other => panic!("{other:?}"),
        }
    }
    assert!(not_eligible(classify_resume(
        &hint(tmp.path(), ResumeAction::InProgress),
        0
    )));
    for (iso, map) in [(false, true), (true, false)] {
        assert!(not_eligible(classify_resume(
            &hint(tmp.path(), preserved(iso, map)),
            0
        )));
    }
    // The snapshot said both exist, but the dir holds neither.
    assert!(not_eligible(classify_resume(
        &hint(tmp.path(), preserved(true, true)),
        0
    )));
}

#[test]
fn a_capture_remuxes_only_when_its_map_is_settled_and_its_image_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("Film.iso");
    let map = freemkv_engine::mapfile_path_for(&iso);
    let total = 64 * 2048u64;
    // An unreadable mapfile.
    std::fs::write(&iso, vec![0u8; total as usize]).unwrap();
    std::fs::create_dir(&map).unwrap();
    assert!(not_eligible(classify_resume(
        &hint(tmp.path(), preserved(true, true)),
        0
    )));
    // A settled map over a short image.
    std::fs::remove_dir(&map).unwrap();
    let mut m = freemkv_engine::Mapfile::create(&map, total, "t").unwrap();
    m.record(0, total, freemkv_engine::SectorStatus::Finished)
        .unwrap();
    m.flush().unwrap();
    std::fs::write(&iso, vec![0u8; 2048]).unwrap();
    assert!(not_eligible(classify_resume(
        &hint(tmp.path(), preserved(true, true)),
        0
    )));
    // Whole image, settled map: remux, named by the image.
    std::fs::write(&iso, vec![0u8; total as usize]).unwrap();
    let action = ResumeAction::ResumeAbortedLoss {
        attempt: 2,
        reason: "loss".into(),
        has_iso: true,
        has_mapfile: true,
        has_mkv: false,
    };
    match classify_resume(&hint(tmp.path(), action), 5) {
        ResumeClass::Remux {
            iso_path,
            mapfile_path,
            display_name,
            title_confident,
        } => {
            assert_eq!(iso_path, iso);
            assert_eq!(mapfile_path, map);
            assert_eq!(display_name, "Film");
            assert_eq!(title_confident, None);
        }
        other => panic!("{other:?}"),
    }
    // Sectors still pending: the sweep resumes instead.
    let mut m = freemkv_engine::Mapfile::load(&map).unwrap();
    m.record(0, 2048, freemkv_engine::SectorStatus::NonTried)
        .unwrap();
    m.flush().unwrap();
    assert!(not_eligible(classify_resume(
        &hint(tmp.path(), preserved(true, true)),
        0
    )));
}
