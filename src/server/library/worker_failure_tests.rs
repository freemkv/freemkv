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
    let now = crate::server::util::epoch_secs();
    let iso = t.path().join("source.iso");
    let error = lib.source_match(&iso).unwrap_err();
    assert!(error.contains("library-matches.json"), "{error}");
    std::fs::write(&file, b"{}").unwrap();
    // A failed read is not repeated on every call, only once a minute.
    let error = lib.match_store_at(now + 1).err().unwrap().to_string();
    assert!(error.contains("library-matches.json"), "{error}");
    assert_eq!(lib.match_store_at(now + 60).unwrap().get(&iso), None);
    assert_eq!(lib.source_match(&iso).unwrap(), None);
}
