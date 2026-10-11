use super::*;
use crate::server::planner::{MediaKind, MediaMetadata, PlannedOutput, RemuxPlan};
use std::path::PathBuf;

fn tv_plan() -> RemuxPlan {
    RemuxPlan {
        media: MediaMetadata {
            title: "Show".into(),
            year: 2001,
            season: Some(1),
            kind: Some(MediaKind::Tv),
            ..Default::default()
        },
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

// The library of `library_with`, and a separate TV folder configured beside it.
fn with_tv(t: &tempfile::TempDir, dirs: &Dirs) -> (Config, Dirs) {
    let cfg = Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        output_dir: t.path().join("out").to_string_lossy().into_owned(),
        tv_dir: "tv".into(),
        ..Config::default()
    };
    let d = super::super::super::dirs(&cfg);
    std::fs::create_dir_all(d.tv.as_ref().unwrap()).unwrap();
    (
        cfg,
        Dirs {
            isos: dirs.isos.clone(),
            ..d
        },
    )
}

#[test]
fn an_empty_tv_folder_is_never_written_into() {
    let (t, _lib, dirs) = library_with(&["A"]);
    let (cfg, d) = with_tv(&t, &dirs);
    let plan = tv_plan();
    let (_, targets) = tv_destinations(&cfg, &plan).unwrap();
    assert!(targets[0].starts_with(d.tv.as_ref().unwrap()));
    // An unmounted share's mountpoint is an empty folder on the local disk.
    let error = planned_target_ok(&cfg, &plan, &targets[0]).unwrap_err();
    assert!(error.contains("empty"), "{error}");
    std::fs::create_dir_all(d.tv.as_ref().unwrap().join("Other Show")).unwrap();
    planned_target_ok(&cfg, &plan, &targets[0]).unwrap();
}

// A job whose frozen plan files under the TV folder.
fn queued_tv_job(lib: &Library, cfg: &Config, d: &Dirs) -> Job {
    lib.queue.add(vec![NewJob {
        title: "Show".into(),
        iso: d.isos.as_ref().unwrap().join("A.iso"),
        target: d.library.join("Show/Show.mkv"),
        replace: false,
    }]);
    let job = lib.queue.claim_next().unwrap();
    let plan = tv_plan();
    let (_, targets) = tv_destinations(cfg, &plan).unwrap();
    assert!(
        lib.queue
            .set_plan_targets(job.id, plan, Some(targets))
            .unwrap()
    );
    lib.queue.requeue(job.id, JobNote::Interrupted);
    lib.queue.snapshot().jobs[0].clone()
}

#[test]
fn a_job_filing_into_an_unhealthy_tv_folder_waits_for_it() {
    use crate::server::health::tests::{LAST_LOCK, bad_mount, set_mounts};
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (t, lib, dirs) = library_with(&["A"]);
    let (cfg, d) = with_tv(&t, &dirs);
    queued_tv_job(&lib, &cfg, &d);
    let tv = d.tv.clone().unwrap();
    set_mounts(vec![bad_mount("TV", &tv, Fault::Unresponsive)]);
    let held = next_job(&lib, &Arbiter::new(), &d, &|_| Ok(()), 1);
    set_mounts(Vec::new());
    assert!(held.is_none(), "claimed while its TV folder is down");
    assert_eq!(lib.hold().expect("held").path, tv);
}

#[test]
fn a_job_filing_into_the_tv_folder_is_preflighted_there() {
    let (t, lib, dirs) = library_with(&["A"]);
    let (cfg, d) = with_tv(&t, &dirs);
    queued_tv_job(&lib, &cfg, &d);
    let tv = d.tv.clone().unwrap();
    let preflight = |dir: &Path| {
        if dir == tv {
            Err(crate::server::health::Problem {
                role: "output",
                path: dir.to_path_buf(),
                fault: Fault::Unresponsive,
                message: "not responding".into(),
                hint: String::new(),
                detail: String::new(),
            })
        } else {
            Ok(())
        }
    };
    assert!(next_job(&lib, &Arbiter::new(), &d, &preflight, 1).is_none());
    assert_eq!(lib.hold().expect("held").path, tv);
}

#[cfg(unix)]
#[test]
fn a_tv_jobs_folder_problem_names_its_tv_folder() {
    use std::os::unix::fs::PermissionsExt;
    let (t, lib, dirs) = library_with(&["A"]);
    let (cfg, d) = with_tv(&t, &dirs);
    lib.rescan(&d);
    let job = queued_tv_job(&lib, &cfg, &d);
    let tv = d.tv.clone().unwrap();
    std::fs::set_permissions(&tv, std::fs::Permissions::from_mode(0o555)).unwrap();
    let problem = folder_problem(&lib, &job);
    std::fs::set_permissions(&tv, std::fs::Permissions::from_mode(0o755)).unwrap();
    if std::fs::metadata(&tv).is_ok() && problem.is_none() && unsafe { libc::geteuid() } == 0 {
        return; // root ignores modes
    }
    assert_eq!(problem.expect("the TV folder refuses writes").path, tv);
}

#[test]
fn a_change_match_check_hung_on_the_tv_folder_does_not_hold_the_library_folders_probe() {
    let (_t, cfg, lib, sources) = super::super::super::match_tests::corrected_library(&["Show"]);
    let d = super::super::super::dirs(&cfg);
    let tv = d.tv.clone().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let rx = std::sync::Arc::new(std::sync::Mutex::new(rx));
    let hung_tv = tv.clone();
    let (_, _, checked) = lib
        .match_view_with(
            &cfg,
            &sources[0],
            std::time::Duration::from_millis(100),
            move |_, _, roots: Vec<PathBuf>| {
                if roots.contains(&hung_tv) {
                    let _ = rx.lock().unwrap().recv();
                }
                Ok(Vec::new())
            },
        )
        .unwrap();
    assert!(!checked);
    let started = std::time::Instant::now();
    let answer =
        crate::server::health::bounded(&d.library, std::time::Duration::from_secs(2), || ());
    let waited = started.elapsed();
    tx.send(()).unwrap();
    assert!(
        matches!(answer, crate::server::health::Bounded::Done(())),
        "the library folder's probe slot is held by the TV check"
    );
    assert!(waited < std::time::Duration::from_secs(1), "{waited:?}");
}
