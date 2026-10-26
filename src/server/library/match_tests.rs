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
fn an_unverified_source_match_saves_nothing_and_queues_nothing() {
    let (t, cfg, mut lib, sources) = corrected_library(&["Castaway"]);
    lib.iso_stat = Arc::new(|_| None);
    let generation = lib.index_generation();
    let error = lib
        .confirm_match_and_queue(&cfg, &sources[0], 0, show())
        .unwrap_err();
    assert_ne!(error.kind(), io::ErrorKind::TimedOut);
    let message = error.to_string();
    assert!(
        message.contains(&sources[0].display().to_string()),
        "{message}"
    );
    for text in ["not saved", "Nothing was queued", "retry"] {
        assert!(message.contains(text), "{message}");
    }
    assert!(!t.path().join("config/library-matches.json").exists());
    assert_eq!(lib.source_match(&sources[0]).unwrap(), None);
    assert!(lib.queue.snapshot().jobs.is_empty());
    assert_eq!(lib.index_generation(), generation);
}

#[test]
fn an_unverified_source_match_preserves_the_previous_correction_and_revision() {
    let (t, cfg, mut lib, sources) = corrected_library(&["Castaway"]);
    let stamp = links::iso_stamp(&sources[0]).unwrap();
    let old = lib
        .match_store()
        .unwrap()
        .save_for(&sources[0], 0, show(), Some(stamp))
        .unwrap();
    let path = t.path().join("config/library-matches.json");
    let bytes = std::fs::read(&path).unwrap();
    let generation = lib.index_generation();
    lib.iso_stat = Arc::new(|_| None);
    let mut changed = show();
    changed.title = "Corrected title".into();
    lib.confirm_match_and_queue(&cfg, &sources[0], old.revision, changed.clone())
        .unwrap_err();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(lib.source_match(&sources[0]).unwrap(), Some(old.clone()));
    assert_eq!(
        matches::MatchStore::open(&lib.config_dir)
            .unwrap()
            .get(&sources[0]),
        Some(old.clone())
    );
    assert!(lib.queue.snapshot().jobs.is_empty());
    assert_eq!(lib.index_generation(), generation);
    // A successful bounded observation allows the unchanged expected revision to retry.
    lib.iso_stat = Arc::new(move |_| Some(stamp));
    let (saved, queued) = lib
        .confirm_match_and_queue(&cfg, &sources[0], old.revision, changed.clone())
        .unwrap();
    assert_eq!(saved.revision, old.revision + 1);
    assert_eq!(saved.media, changed);
    assert_eq!(saved.iso_stamp, Some(stamp));
    assert_eq!(queued, 1);
    assert!(saved.is_for(Some(stamp)));
    let other = crate::server::util::Stamp {
        size: stamp.size + 1,
        ..stamp
    };
    assert!(!saved.is_for(Some(other)));
    assert_eq!(
        matches::MatchStore::open(&lib.config_dir)
            .unwrap()
            .get(&sources[0]),
        Some(saved)
    );
}

#[test]
fn a_legacy_unstamped_match_still_opens_and_matches_any_source_stamp() {
    let (t, _cfg, lib, sources) = corrected_library(&["Castaway"]);
    let path = t.path().join("config/library-matches.json");
    let record = serde_json::json!({
        sources[0].to_string_lossy().to_string(): {"revision": 1, "media": show()}
    });
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let saved = matches::MatchStore::open(&lib.config_dir)
        .unwrap()
        .get(&sources[0])
        .unwrap();
    assert_eq!(saved.iso_stamp, None);
    assert!(saved.is_for(None));
    assert!(saved.is_for(links::iso_stamp(&sources[0])));
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
fn another_isos_job_at_the_same_title_does_not_block_a_correction() {
    let (_t, cfg, lib, sources) = corrected_library(&["Show Disc 1"]);
    let d = dirs(&cfg);
    // Another ISO's job holds the default destination of this title.
    lib.queue.add(vec![NewJob {
        title: "Other".into(),
        iso: d.isos.as_ref().unwrap().join("elsewhere.iso"),
        target: index::new_mkv_target(&d.library, "Show Disc 1"),
        replace: false,
    }]);
    let (saved, queued) = lib
        .confirm_match_and_queue(&cfg, &sources[0], 0, show())
        .unwrap();
    assert_eq!(queued, 1);
    assert_eq!(lib.source_match(&sources[0]).unwrap(), Some(saved));
    assert_eq!(lib.queue.snapshot().jobs.len(), 2);
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
    let (_, linked, checked) = lib.match_view(&cfg, &sources[0]).unwrap();
    assert!(checked);
    let captured: Vec<_> = owned::capture(
        &links::read_entries(&lib.config_dir).unwrap(),
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
fn a_hung_share_gives_the_rule_filtered_list_and_a_flag() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway"]);
    let d = dirs(&cfg);
    let movie = d.library.join("Cast Away (2000)/Cast Away (2000).mkv");
    std::fs::create_dir_all(movie.parent().unwrap()).unwrap();
    std::fs::write(&movie, b"mkv").unwrap();
    links::record(&lib.config_dir, &movie, &sources[0]).unwrap();
    links::record(&lib.config_dir, Path::new("/elsewhere/X.mkv"), &sources[0]).unwrap();
    let (_, linked, checked) = lib
        .match_view_with(
            &cfg,
            &sources[0],
            std::time::Duration::from_millis(50),
            |_, _, _| {
                std::thread::sleep(std::time::Duration::from_millis(600));
                Ok(Vec::new())
            },
        )
        .unwrap();
    assert!(!checked);
    assert_eq!(linked, vec![movie]);
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
    lib.match_store()
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

#[test]
fn two_isos_with_one_name_in_different_folders_each_get_their_own_correction() {
    let temp = tempfile::tempdir().unwrap();
    let cfg = Config {
        output_dir: temp.path().join("media").to_string_lossy().into_owned(),
        movie_dir: "movies".into(),
        library_iso_dir: temp.path().join("isos").to_string_lossy().into_owned(),
        library_iso_subfolders: true,
        ..Default::default()
    };
    let d = dirs(&cfg);
    std::fs::create_dir_all(&d.library).unwrap();
    std::fs::write(d.library.join(".keep"), b"x").unwrap();
    let sources: Vec<PathBuf> = ["Season 1", "Season 2"]
        .iter()
        .map(|folder| {
            let source = d.isos.as_ref().unwrap().join(folder).join("Disc 1.iso");
            std::fs::create_dir_all(source.parent().unwrap()).unwrap();
            std::fs::write(&source, folder.as_bytes()).unwrap();
            source
        })
        .collect();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    let lib = Library::open(&temp.path().join("config"), &temp.path().join("logs"));
    lib.rescan(&d);
    for source in &sources {
        let (_, queued) = lib
            .confirm_match_and_queue(&cfg, source, 0, show())
            .unwrap();
        assert_eq!(queued, 1, "{}", source.display());
    }
    let jobs = lib.queue.snapshot().jobs;
    assert_eq!(jobs.len(), 2);
    assert_ne!(jobs[0].target, jobs[1].target);
}

#[test]
fn the_listing_does_not_present_a_match_saved_for_an_earlier_disc_at_the_path() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway"]);
    let d = dirs(&cfg);
    lib.match_store()
        .unwrap()
        .save(&sources[0], 0, show())
        .unwrap();
    let row_match = |lib: &Library| {
        lib.listing(&d)
            .rows
            .into_iter()
            .find(|r| r.iso.as_deref() == Some(sources[0].as_path()))
            .unwrap()
            .source_match
    };
    lib.index_now(&d);
    assert!(row_match(&lib).is_some());
    let later =
        links::iso_stamp(&sources[0]).unwrap().modified + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&sources[0])
        .unwrap()
        .set_modified(later)
        .unwrap();
    lib.index_now(&d);
    assert_eq!(row_match(&lib), None);
}

#[test]
fn a_change_match_job_shows_on_its_iso_only_row_and_unqueues_from_it() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway"]);
    let d = dirs(&cfg);
    lib.confirm_match_and_queue(&cfg, &sources[0], 0, show())
        .unwrap();
    let row = lib
        .listing(&d)
        .rows
        .into_iter()
        .find(|r| r.iso.as_deref() == Some(sources[0].as_path()))
        .unwrap();
    assert_eq!(row.kind, RowKind::IsoOnly);
    let job = row
        .job
        .expect("the Change match job shows on its ISO-only row");
    assert_eq!(job.state, queue::JobState::Queued);
    assert_ne!(Some(&job.target), row.target.as_ref());
    assert_eq!(lib.remove_queued(&d, row.target.as_ref().unwrap()), 1);
    assert!(lib.queue.snapshot().jobs.is_empty());
}

#[test]
fn the_match_dialog_does_not_offer_a_match_saved_for_an_earlier_disc() {
    let (_t, cfg, lib, sources) = corrected_library(&["Show"]);
    lib.match_store()
        .unwrap()
        .save(&sources[0], 0, show())
        .unwrap();
    lib.rescan(&dirs(&cfg));
    let saved = lib.match_view(&cfg, &sources[0]).unwrap().0.unwrap();
    assert!(lib.match_is_current(&sources[0], &saved));
    // Another disc is filed at the same path; the listing no longer shows the old match.
    std::fs::write(&sources[0], b"a different disc").unwrap();
    lib.rescan(&dirs(&cfg));
    let listed = lib.listing(&dirs(&cfg));
    let row = listed
        .rows
        .iter()
        .find(|r| r.iso.as_ref() == Some(&sources[0]));
    assert!(row.unwrap().source_match.is_none());
    // The dialog still gets its revision to save over, flagged as not this disc's.
    let saved = lib.match_view(&cfg, &sources[0]).unwrap().0.unwrap();
    assert!(!lib.match_is_current(&sources[0], &saved));
}

#[test]
fn a_change_match_queued_from_a_row_with_an_mkv_shows_there_and_can_be_unqueued() {
    let (_t, cfg, lib, sources) = corrected_library(&["Castaway"]);
    let d = dirs(&cfg);
    let mkv = index::new_mkv_target(&d.library, "Castaway");
    std::fs::create_dir_all(mkv.parent().unwrap()).unwrap();
    std::fs::write(&mkv, b"old remux").unwrap();
    lib.rescan(&d);
    let row = |lib: &Library| {
        lib.listing(&d)
            .rows
            .into_iter()
            .find(|r| r.mkv.as_ref() == Some(&mkv))
            .unwrap()
    };
    assert_eq!(row(&lib).iso.as_ref(), Some(&sources[0]));
    let (_, queued) = lib
        .confirm_match_and_queue(&cfg, &sources[0], 0, show())
        .unwrap();
    assert_eq!(queued, 1);
    let shown = row(&lib)
        .job
        .expect("the queued Change match shows on the row");
    assert!(shown.retire && shown.state == queue::JobState::Queued);
    assert_eq!(lib.remove_queued(&d, &mkv), 1);
    assert!(lib.queue.snapshot().jobs.is_empty());
}
