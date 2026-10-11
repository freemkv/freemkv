use super::*;
use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};

fn one_output() -> RemuxPlan {
    RemuxPlan {
        media: MediaMetadata::default(),
        outputs: vec![PlannedOutput {
            id: "main".into(),
            title_index: 0,
            episode: None,
            episode_name: String::new(),
            filename: "A.mkv".into(),
        }],
    }
}

// Job `A` replacing (or creating) `<library>/A/A.mkv` with a one-output plan.
fn planned(lib: &Library, dirs: &Dirs, replace: bool) -> Job {
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: dirs.isos.as_ref().unwrap().join("A.iso"),
        target: dirs.library.join("A/A.mkv"),
        replace,
    }]);
    let job = lib.queue.claim_next().unwrap();
    assert!(lib.queue.set_plan(job.id, one_output()));
    lib.queue.snapshot().jobs[0].clone()
}

#[test]
fn a_job_whose_output_landed_never_says_nothing_was_written() {
    for replace in [true, false] {
        let (_t, lib, dirs) = library_with(&["A"]);
        let job = planned(&lib, &dirs, replace);
        lib.queue.finish_output(job.id, "main", OutputState::Done);
        let job = lib.queue.snapshot().jobs[0].clone();
        let text = failure_message("links could not be saved", None, None, &job, None);
        assert!(!text.contains("No MKV was written"), "{text}");
        assert!(!text.contains("existing MKV is unchanged"), "{text}");
        assert!(text.contains("in the library"), "{text}");
    }
}

#[cfg(unix)]
#[test]
fn links_that_cannot_be_saved_after_landing_say_the_file_is_in_place() {
    use std::os::unix::fs::PermissionsExt;
    let (t, lib, dirs) = library_with(&["A"]);
    let job = planned(&lib, &dirs, true);
    let links = t
        .path()
        .join("config")
        .join(super::super::super::links::LINKS_FILE);
    std::fs::write(&links, b"{}").unwrap();
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root ignores modes
    }
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = job.id;
    let cfg = Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let ending = remux_planned_with(
        &job,
        job.plan.as_ref().unwrap(),
        &cfg,
        &sink,
        |request, _, _| {
            // The links file becomes unreadable once the owned files were captured.
            std::fs::set_permissions(&links, std::fs::Permissions::from_mode(0o000))?;
            std::fs::write(&request.target, movie())?;
            Ok(Some(current_stamp()))
        },
    );
    std::fs::set_permissions(&links, std::fs::Permissions::from_mode(0o644)).unwrap();
    let Ending::Failed(e) = ending else {
        panic!("{ending:?}")
    };
    let text = e.to_string();
    assert!(text.contains("is in place"), "{text}");
    assert!(text.contains("library-links.json"), "{text}");
}

#[test]
fn saved_matches_unreadable_at_start_are_read_again_once_repaired() {
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join("library-matches.json");
    std::fs::write(&file, b"broken").unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    let iso = t.path().join("source.iso");
    let error = lib.source_match(&iso).unwrap_err();
    assert!(error.contains("library-matches.json"), "{error}");
    // Timed from the failure as recorded, not a clock read of the test's own.
    let now = lib.matches_failed.lock().unwrap().as_ref().unwrap().0;
    std::fs::write(&file, b"{}").unwrap();
    // A failed read is not repeated on every call, only once a minute.
    let error = lib.match_store_at(now + 1).err().unwrap().to_string();
    assert!(error.contains("library-matches.json"), "{error}");
    assert_eq!(lib.match_store_at(now + 60).unwrap().get(&iso), None);
    assert_eq!(lib.source_match(&iso).unwrap(), None);
}

#[test]
fn a_failed_folder_check_is_named_once_in_the_failure() {
    let dir = tempfile::tempdir().unwrap();
    let job = super::job_for(dir.path(), 1);
    let p = super::stale();
    let text = failure_message(&problem_error(&p).to_string(), None, Some(&p), &job, None);
    let folder = p.path.display().to_string();
    assert_eq!(text.matches(&p.message).count(), 1, "{text}");
    assert_eq!(text.matches(&folder).count(), 1, "{text}");
    assert!(text.contains(&p.detail), "{text}");
}

#[test]
fn a_change_match_console_names_its_iso_and_planned_files_not_its_queue_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = super::job_for(dir.path(), 1);
    job.retire = true;
    job.target = dir
        .path()
        .join("lib/Heat [3fa2b1c0d9e8]/Heat [3fa2b1c0d9e8].mkv");
    let unplanned = command_line(&job);
    assert!(!unplanned.contains("3fa2b1c0d9e8"), "{unplanned}");
    assert!(
        unplanned.contains(&*job.iso.to_string_lossy()),
        "{unplanned}"
    );
    let season = dir.path().join("tv/Show/Season 01");
    job.plan = Some(one_output());
    job.outputs = (1..=2)
        .map(|e| super::super::super::queue::OutputProgress {
            id: format!("e{e}"),
            target: season.join(format!("Show S01E0{e}.mkv")),
            state: OutputState::Pending,
            staged: None,
            landing: None,
        })
        .collect();
    let planned = command_line(&job);
    assert!(!planned.contains("3fa2b1c0d9e8"), "{planned}");
    assert!(
        planned.ends_with(&format!("mkv://{}", season.display())),
        "{planned}"
    );
}

// A kept file is copied to the output it was made for; a Change match's queue key is never
// named, and the ISO always is.
#[test]
fn a_kept_file_console_names_its_iso_and_output_not_the_queue_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = super::job_for(dir.path(), 1);
    job.retire = true;
    job.target = dir
        .path()
        .join("lib/Heat [3fa2b1c0d9e8]/Heat [3fa2b1c0d9e8].mkv");
    let kept = dir.path().join("stage/Show_S01E02.0123456789ab.staged.mkv");
    let unplanned = kept_line(&job, &kept);
    assert!(!unplanned.contains("3fa2b1c0d9e8"), "{unplanned}");
    assert!(
        unplanned.contains(&*job.iso.to_string_lossy()),
        "{unplanned}"
    );
    assert!(unplanned.contains(&*kept.to_string_lossy()), "{unplanned}");
    let season = dir.path().join("tv/Show/Season 01");
    job.plan = Some(one_output());
    job.outputs = (1..=2)
        .map(|e| super::super::super::queue::OutputProgress {
            id: format!("e{e}"),
            target: season.join(format!("Show S01E0{e}.mkv")),
            state: OutputState::Pending,
            staged: (e == 2).then(|| kept.clone()),
            landing: None,
        })
        .collect();
    let planned = kept_line(&job, &kept);
    assert!(!planned.contains("3fa2b1c0d9e8"), "{planned}");
    let output = season.join("Show S01E02.mkv");
    assert!(planned.ends_with(&*output.to_string_lossy()), "{planned}");
}
