use super::*;
use crate::server::library::links;
use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};
use std::path::PathBuf;

fn one_output(filename: &str) -> RemuxPlan {
    RemuxPlan {
        media: MediaMetadata::default(),
        outputs: vec![PlannedOutput {
            id: "main".into(),
            title_index: 0,
            episode: None,
            episode_name: String::new(),
            filename: filename.into(),
        }],
    }
}

fn config_of(dirs: &Dirs) -> Config {
    Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    }
}

// Queue `target` from `iso`, claim it and freeze a one-output plan at `target`.
fn planned_job(lib: &Library, iso: &Path, target: &Path, replace: bool, retire: bool) -> Job {
    let item = NewJob {
        title: "A".into(),
        iso: iso.to_path_buf(),
        target: target.to_path_buf(),
        replace,
    };
    if retire {
        lib.queue.add_fresh(vec![item], true);
    } else {
        lib.queue.add(vec![item]);
    }
    let job = lib.queue.claim_next().unwrap();
    let name = target.file_name().unwrap().to_string_lossy().into_owned();
    assert!(lib.queue.set_plan(job.id, one_output(&name)));
    lib.queue
        .snapshot()
        .jobs
        .into_iter()
        .find(|j| j.id == job.id)
        .unwrap()
}

fn run(lib: &Library, dirs: &Dirs, job: &Job) -> (Ending, usize) {
    let arbiter = Arbiter::new();
    let mut sink = test_sink(lib, &arbiter);
    sink.job_id = job.id;
    let mut calls = 0;
    let ending = remux_planned_with(
        job,
        job.plan.as_ref().unwrap(),
        &config_of(dirs),
        &sink,
        |request, _, _| {
            calls += 1;
            std::fs::create_dir_all(request.target.parent().unwrap())?;
            std::fs::write(&request.target, movie())?;
            Ok(Some(current_stamp()))
        },
    );
    (ending, calls)
}

fn iso_of(dirs: &Dirs, title: &str) -> PathBuf {
    dirs.isos.as_ref().unwrap().join(format!("{title}.iso"))
}

#[test]
fn a_fresh_plan_cannot_accept_a_landed_output_from_a_replaced_source() {
    for same_size in [false, true] {
        let (t, lib, dirs) = library_with(&["A"]);
        let target = dirs.library.join("A/A.mkv");
        let original = std::fs::read(&target).unwrap();
        let iso = iso_of(&dirs, "A");
        links::record(&t.path().join("config"), &target, &iso).unwrap();
        let before = links::iso_stamp(&iso).unwrap();
        if same_size {
            std::fs::File::options()
                .write(true)
                .open(&iso)
                .unwrap()
                .set_modified(before.modified + Duration::from_secs(60))
                .unwrap();
        } else {
            std::fs::write(&iso, b"a different disc at the same source path").unwrap();
        }
        assert_ne!(links::iso_stamp(&iso), Some(before));
        // Plan only after replacement: no frozen-plan provenance is needed to reject this.
        let job = planned_job(&lib, &iso, &target, false, false);
        let (ending, calls) = run(&lib, &dirs, &job);
        let Ending::Failed(error) = ending else {
            panic!("accepted prior-disc output: {ending:?}");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(calls, 0);
        assert!(!outputs_all_done(&lib.queue.snapshot(), job.id));
        assert_eq!(std::fs::read(&target).unwrap(), original);
    }
}

#[test]
fn landed_link_source_proof_preserves_unchanged_and_unknown_source_semantics() {
    let (t, lib, dirs) = library_with(&["A"]);
    let config = t.path().join("config");
    let target = dirs.library.join("A/A.mkv");
    let iso = iso_of(&dirs, "A");
    links::record(&config, &target, &iso).unwrap();
    assert!(landed_by_this_job(&config, &iso, &target));
    let job = planned_job(&lib, &iso, &target, false, false);
    lib.queue.set_owned(job.id, Vec::new()).unwrap();
    let (ending, calls) = run(&lib, &dirs, &job);
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(calls, 0);
    assert!(outputs_all_done(&lib.queue.snapshot(), job.id));

    std::fs::remove_file(&iso).unwrap();
    assert!(!landed_by_this_job(&config, &iso, &target));
    // Existing links with an MKV stamp but no source stamp retain owned::proof semantics.
    links::record(&config, &target, &iso).unwrap();
    assert!(links::entries(&config)[&target].iso_stamp.is_none());
    assert!(landed_by_this_job(&config, &iso, &target));
    // A path-only 1.8.3 link cannot vouch for the MKV itself.
    std::fs::write(
        config.join(links::LINKS_FILE),
        serde_json::to_vec(&serde_json::json!({ target.to_string_lossy(): iso.to_string_lossy() }))
            .unwrap(),
    )
    .unwrap();
    assert!(!landed_by_this_job(&config, &iso, &target));
}

#[test]
fn a_remux_never_overwrites_a_linked_file_another_tool_replaced_in_place() {
    let (t, lib, dirs) = library_with(&["A"]);
    let target = dirs.library.join("A/A.mkv");
    let iso = iso_of(&dirs, "A");
    links::record(&t.path().join("config"), &target, &iso).unwrap();
    // A media manager upgrades the file in place, at the linked path.
    std::fs::write(&target, b"an upgrade another tool wrote in place").unwrap();
    let job = planned_job(&lib, &iso, &target, true, false);
    let (ending, calls) = run(&lib, &dirs, &job);
    let Ending::Failed(e) = ending else {
        panic!("{ending:?}")
    };
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists, "{e}");
    assert_eq!(calls, 0);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"an upgrade another tool wrote in place"
    );
}

#[test]
fn a_row_linked_by_1_8_3_is_still_replaced_by_its_own_remux() {
    let (t, lib, dirs) = library_with(&["A"]);
    let target = dirs.library.join("A/A.mkv");
    let iso = iso_of(&dirs, "A");
    let legacy = serde_json::json!({ target.to_string_lossy(): iso.to_string_lossy() });
    std::fs::write(
        t.path().join("config").join(links::LINKS_FILE),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    let job = planned_job(&lib, &iso, &target, true, false);
    let (ending, calls) = run(&lib, &dirs, &job);
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(calls, 1);
    assert_eq!(std::fs::read(&target).unwrap(), movie());
}

// A Change match of `iso` into `New/New.mkv`, with `old` linked to it beforehand.
fn correction(lib: &Library, dirs: &Dirs, iso: &Path) -> Job {
    std::fs::create_dir_all(dirs.library.join("New")).unwrap();
    planned_job(lib, iso, &dirs.library.join("New/New.mkv"), false, true)
}

fn old_file(t: &tempfile::TempDir, dirs: &Dirs, iso: &Path) -> PathBuf {
    let old = dirs.library.join("Old/Old.mkv");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"the old file").unwrap();
    links::record(&t.path().join("config"), &old, iso).unwrap();
    old
}

#[test]
fn a_correction_keeps_a_linked_file_replaced_in_place_before_it_started() {
    let (t, lib, dirs) = library_with(&["B"]);
    let iso = iso_of(&dirs, "B");
    let old = old_file(&t, &dirs, &iso);
    std::fs::write(&old, b"the user's own upgrade, longer").unwrap();
    let job = correction(&lib, &dirs, &iso);
    let (ending, _) = run(&lib, &dirs, &job);
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(
        std::fs::read(&old).unwrap(),
        b"the user's own upgrade, longer"
    );
}

#[test]
fn a_correction_never_removes_a_file_linked_by_1_8_3() {
    let (t, lib, dirs) = library_with(&["B"]);
    let iso = iso_of(&dirs, "B");
    let old = dirs.library.join("Old/Old.mkv");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, b"made by 1.8.3").unwrap();
    let legacy = serde_json::json!({ old.to_string_lossy(): iso.to_string_lossy() });
    std::fs::write(
        t.path().join("config").join(links::LINKS_FILE),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();
    let job = correction(&lib, &dirs, &iso);
    let (ending, _) = run(&lib, &dirs, &job);
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(std::fs::read(&old).unwrap(), b"made by 1.8.3");
}

#[test]
fn a_disc_filed_later_at_the_same_iso_path_does_not_inherit_the_earlier_discs_files() {
    let (t, lib, dirs) = library_with(&["B"]);
    let iso = iso_of(&dirs, "B");
    let old = old_file(&t, &dirs, &iso);
    // The first disc's ISO is deleted; the next disc of the show is filed at the same path.
    std::fs::remove_file(&iso).unwrap();
    std::fs::write(&iso, b"the season 2 disc image").unwrap();
    let job = correction(&lib, &dirs, &iso);
    let (ending, _) = run(&lib, &dirs, &job);
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(std::fs::read(&old).unwrap(), b"the old file");
}

// A confirmed match for an ISO (a real image), then another disc filed at the same path.
fn match_then_refile() -> (tempfile::TempDir, Config, Library, PathBuf) {
    let (t, cfg, lib, sources) = super::super::super::match_tests::corrected_library(&["Show"]);
    let iso = sources[0].clone();
    let image = crate::ku_fixture::bd_image();
    std::fs::write(&iso, &image.img.image).unwrap();
    lib.rescan(&super::super::super::dirs(&cfg));
    lib.confirm_match_and_queue(&cfg, &iso, 0, super::super::super::match_tests::show())
        .unwrap();
    // The next disc of the show is filed at the same ISO path.
    std::fs::remove_file(&iso).unwrap();
    let mut other = image.img.image.clone();
    other.extend_from_slice(&[0; 2048]);
    std::fs::write(&iso, &other).unwrap();
    (t, cfg, lib, iso)
}

#[test]
fn a_change_match_saved_for_an_earlier_disc_at_the_iso_path_is_not_applied() {
    let (_t, cfg, lib, _iso) = match_then_refile();
    let mut job = lib.queue.claim_next().unwrap();
    assert!(job.retire);
    let error = maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap_err();
    assert!(error.to_string().contains("Change match"), "{error}");
    assert!(lib.queue.snapshot().jobs[0].plan.is_none());
}

#[test]
fn a_remux_of_a_disc_refiled_at_a_matched_iso_path_ignores_the_earlier_match() {
    let (_t, cfg, lib, iso) = match_then_refile();
    let id = lib.queue.snapshot().jobs[0].id;
    lib.queue.drop_job(id);
    let target = super::super::super::dirs(&cfg)
        .library
        .join("Show/Show.mkv");
    lib.queue.add(vec![NewJob {
        title: "Show".into(),
        iso,
        target,
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap();
    let plan = job.plan.expect("planned");
    assert_ne!(plan.media.title, "Show", "{plan:?}");
    assert_eq!(plan.media.tmdb_id, 0);
}

#[test]
fn a_remux_of_a_matched_iso_restamped_at_the_same_size_fails_rather_than_auto_planning() {
    let (_t, cfg, lib, sources) = super::super::super::match_tests::corrected_library(&["Show"]);
    let iso = sources[0].clone();
    std::fs::write(&iso, &crate::ku_fixture::bd_image().img.image).unwrap();
    lib.rescan(&super::super::super::dirs(&cfg));
    lib.confirm_match_and_queue(&cfg, &iso, 0, super::super::super::match_tests::show())
        .unwrap();
    let id = lib.queue.snapshot().jobs[0].id;
    lib.queue.drop_job(id);
    // Same size, new time: the same disc or another one, which cannot be told.
    let later = links::iso_stamp(&iso).unwrap().modified + std::time::Duration::from_secs(60);
    std::fs::File::options()
        .write(true)
        .open(&iso)
        .unwrap()
        .set_modified(later)
        .unwrap();
    let target = super::super::super::dirs(&cfg)
        .library
        .join("Show/Show.mkv");
    lib.queue.add(vec![NewJob {
        title: "Show".into(),
        iso,
        target,
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    assert!(!job.retire);
    let error = maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap_err();
    assert!(error.to_string().contains("Change match"), "{error}");
    assert!(lib.queue.snapshot().jobs[0].plan.is_none());
}
