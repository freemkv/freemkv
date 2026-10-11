use super::*;
use std::io;

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

pub(super) fn corrected_library(
    isos: &[&str],
) -> (tempfile::TempDir, Config, Library, Vec<PathBuf>) {
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
    let sources: Vec<PathBuf> = isos
        .iter()
        .map(|name| {
            let source = d.isos.as_ref().unwrap().join(format!("{name}.iso"));
            std::fs::write(&source, name.as_bytes()).unwrap();
            source
        })
        .collect();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let lib = Library::open(&temp.path().join("config"), &temp.path().join("logs"));
    lib.rescan(&d);
    (temp, cfg, lib, sources)
}

pub(super) fn show() -> crate::server::planner::MediaMetadata {
    crate::server::planner::MediaMetadata {
        title: "Show".into(),
        year: 2001,
        tmdb_id: 77,
        kind: Some(crate::server::planner::MediaKind::Movie),
        ..Default::default()
    }
}

#[test]
fn confirming_a_match_for_an_idle_source_saves_it_and_queues_one_remux() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway"]);
    let (saved, queued) = lib
        .confirm_match_and_queue(&cfg, &sources[0], 0, show())
        .unwrap();
    assert_eq!((saved.revision, queued), (1, 1));
    assert_eq!(lib.source_match(&sources[0]).unwrap(), Some(saved));
    let jobs = lib.queue.snapshot().jobs;
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (jobs[0].iso.as_path(), jobs[0].title.as_str()),
        (sources[0].as_path(), "Show")
    );
}

#[test]
fn same_title_corrections_from_two_discs_each_get_their_own_job() {
    let (_t, cfg, lib, sources) = corrected_library(&["Show Disc 1", "Show Disc 2"]);
    for source in &sources {
        let (_, queued) = lib
            .confirm_match_and_queue(&cfg, source, 0, show())
            .unwrap();
        assert_eq!(queued, 1);
    }
    let jobs = lib.queue.snapshot().jobs;
    assert_eq!(jobs.len(), 2);
    assert_ne!(jobs[0].target, jobs[1].target);
}

#[test]
fn a_busy_source_refuses_the_match_and_leaves_it_unchanged() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway"]);
    let d = dirs(&cfg);
    lib.queue.add(vec![NewJob {
        title: "Castaway".into(),
        iso: sources[0].clone(),
        target: index::new_mkv_target(&d.library, "Castaway"),
        replace: false,
    }]);
    let error = lib
        .confirm_match_and_queue(&cfg, &sources[0], 0, show())
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
    assert_eq!(lib.source_match(&sources[0]).unwrap(), None);
    assert_eq!(lib.queue.snapshot().jobs.len(), 1);
}

#[test]
fn a_match_whose_remux_cannot_be_queued_stays_saved_and_reports_none_queued() {
    let (_t, cfg, lib, sources) = corrected_library(&["Show Disc 1"]);
    let d = dirs(&cfg);
    // Another ISO's job already holds this source's destination.
    lib.queue.add(vec![NewJob {
        title: "Other".into(),
        iso: d.isos.as_ref().unwrap().join("elsewhere.iso"),
        target: index::new_mkv_target(&d.library, "Show Disc 1"),
        replace: false,
    }]);
    let (saved, queued) = lib
        .confirm_match_and_queue(&cfg, &sources[0], 0, show())
        .unwrap();
    assert_eq!(queued, 0);
    assert_eq!(lib.source_match(&sources[0]).unwrap(), Some(saved));
    assert_eq!(lib.queue.snapshot().jobs.len(), 1);
}

#[test]
fn a_match_for_an_unindexed_path_saves_nothing() {
    let (t, cfg, lib, _) = corrected_library(&["Castaway"]);
    let outside = t.path().join("outside.iso");
    std::fs::write(&outside, b"x").unwrap();
    let error = lib
        .confirm_match_and_queue(&cfg, &outside, 0, show())
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!t.path().join("config/library-matches.json").exists());
}

#[test]
fn the_dialog_lists_exactly_the_files_a_change_match_captures() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway", "Other"]);
    let d = dirs(&cfg);
    let tv = d.tv.clone().unwrap();
    // Linked files, including one the last scan has not indexed yet.
    let movie = d.library.join("Cast Away (2000)/Cast Away (2000).mkv");
    let episode = tv.join("Show (2001)/Season 01/Show S01E01.mkv");
    let other = d.library.join("Other.mkv");
    let stray = d.library.join("Unlinked.mkv");
    for path in [&movie, &episode, &other, &stray] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"mkv").unwrap();
    }
    links::record(&lib.config_dir, &movie, &sources[0]).unwrap();
    links::record(&lib.config_dir, &episode, &sources[0]).unwrap();
    links::record(&lib.config_dir, &d.library.join("Gone.mkv"), &sources[0]).unwrap();
    links::record(&lib.config_dir, &other, &sources[1]).unwrap();
    let (_, linked) = lib.match_view(&cfg, &sources[0]).unwrap();
    let captured: Vec<_> = owned::capture(
        &links::read(&lib.config_dir).unwrap(),
        &sources[0],
        &owned::roots(&d),
    )
    .unwrap()
    .into_iter()
    .map(|o| o.path)
    .collect();
    assert_eq!(linked, captured);
    assert_eq!(linked, vec![movie, episode]);
}

#[test]
fn only_a_change_match_may_retire_old_files() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway", "Other"]);
    lib.confirm_match_and_queue(&cfg, &sources[0], 0, show())
        .unwrap();
    let job = serde_json::to_value(&lib.queue.snapshot().jobs[0]).unwrap();
    assert_eq!(job["retire"], true, "{job}");
    assert_eq!(job["replace"], false, "{job}");
    assert!(job.get("replacement").is_none(), "{job}");
}

#[test]
fn remux_of_an_iso_with_a_saved_match_replans_but_never_retires() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway", "Other"]);
    // Remux, Retry and Remux all on an ISO with a saved match replan, but never delete.
    let d = dirs(&cfg);
    lib.source_matches
        .as_ref()
        .unwrap()
        .save(&sources[1], 0, show())
        .unwrap();
    assert_eq!(
        lib.enqueue(&d, |r| r.iso.as_deref() == Some(&sources[1])),
        1
    );
    let job = serde_json::to_value(&lib.queue.snapshot().jobs[0]).unwrap();
    assert_eq!(job["iso"], serde_json::json!(sources[1]), "{job}");
    assert!(job.get("retire").is_none(), "{job}");
    assert!(job.get("replacement").is_none(), "{job}");
    assert!(job.get("plan").is_none(), "planned afresh at claim: {job}");
}
