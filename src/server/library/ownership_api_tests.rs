use super::*;

#[test]
fn tv_scan_deduplicates_overlap_and_marks_unreadable_root_incomplete() {
    let temp = tempfile::tempdir().unwrap();
    let movies = temp.path().join("movies");
    let tv = movies.join("tv");
    std::fs::create_dir_all(&tv).unwrap();
    std::fs::write(tv.join("episode.mkv"), b"episode").unwrap();
    let lib = Library::open(temp.path(), &temp.path().join("logs"));
    let mut d = Dirs {
        library: movies,
        tv: Some(tv),
        isos: None,
        iso_subfolders: false,
    };
    let scan = lib.scan(&d);
    assert!(!scan.incomplete);
    assert_eq!(scan.mkvs.len(), 1);
    d.tv = Some(temp.path().join("not-created"));
    assert!(!lib.scan(&d).incomplete);
    let inaccessible = temp.path().join("not-directory");
    std::fs::write(&inaccessible, b"not a directory").unwrap();
    d.tv = Some(inaccessible);
    assert!(lib.scan(&d).incomplete);
}

#[test]
fn unlinked_tv_outputs_without_jobs_are_preview_candidates() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Config {
        output_dir: temp.path().join("media").to_string_lossy().into_owned(),
        movie_dir: "movies".into(),
        tv_dir: "tv".into(),
        library_dir: String::new(),
        library_iso_dir: temp.path().join("isos").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let d = dirs(&cfg);
    std::fs::create_dir_all(&d.library).unwrap();
    std::fs::write(d.library.join(".keep"), b"configured movie root").unwrap();
    std::fs::create_dir_all(d.tv.as_ref().unwrap()).unwrap();
    std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
    let source = d.isos.as_ref().unwrap().join("Castaway.iso");
    let old = d.tv.as_ref().unwrap().join("Misidentified S01E01.mkv");
    std::fs::write(&source, b"source").unwrap();
    std::fs::write(&old, b"old episode").unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let lib = Library::open(&temp.path().join("config"), &temp.path().join("logs"));
    lib.rescan(&d);
    assert!(lib.queue.snapshot().jobs.is_empty());
    assert!(links::read(&lib.config_dir).unwrap().is_empty());
    let (_, preview) = lib.preview_ownership(&cfg, &source).unwrap();
    assert_eq!(preview.candidates.len(), 1);
    assert_eq!(preview.candidates[0].path, old);
    assert!(preview.owned_outputs.is_empty());
    assert!(links::read(&lib.config_dir).unwrap().is_empty());
    let (_, queued) = lib
        .confirm_match_and_queue(
            &cfg,
            &source,
            0,
            crate::server::planner::MediaMetadata {
                title: "Cast Away".into(),
                year: 2000,
                tmdb_id: 8358,
                kind: Some(crate::server::planner::MediaKind::Movie),
                ..Default::default()
            },
            ownership::Confirmation {
                preview_token: preview.preview_token,
                selected_candidates: vec![0],
                confirm_ownership: true,
            },
        )
        .unwrap();
    assert_eq!(queued, 1);
    assert!(links::read(&lib.config_dir).unwrap().is_empty());
    let reopened = Library::open(&temp.path().join("config"), &temp.path().join("logs"));
    let jobs = reopened.queue.snapshot().jobs;
    let replacement = jobs[0].replacement.as_ref().unwrap();
    assert_eq!(replacement.old_outputs[0].path, old);
    assert_eq!(
        replacement.old_outputs[0].ownership,
        replacement::Ownership::UserConfirmed
    );
}
