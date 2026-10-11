use super::*;
use crate::server::library::links;
use crate::server::planner::{MediaMetadata, PlannedOutput, RemuxPlan};
use std::collections::HashMap;
use std::path::PathBuf;

const ISO: &str = "/i/A.iso";

// A Change match of ISO into `n` episodes under library/Show, with three old episodes
// linked to it, another ISO's file and an unlinked file beside them.
struct Correction {
    t: tempfile::TempDir,
    lib: Library,
    dirs: Dirs,
    job: Job,
    old: Vec<PathBuf>,
    other: PathBuf,
    unlinked: PathBuf,
}

impl Correction {
    fn new(n: usize) -> Self {
        let (t, lib, dirs) = library_with(&["B"]);
        lib.queue.add(vec![NewJob {
            title: "Show".into(),
            iso: ISO.into(),
            target: dirs.library.join("Show/Show.mkv"),
            replace: false,
        }]);
        let claimed = lib.queue.claim_next().unwrap();
        let plan = RemuxPlan {
            media: MediaMetadata::default(),
            outputs: (0..n)
                .map(|i| PlannedOutput {
                    id: format!("episode-{i}"),
                    title_index: i,
                    episode: Some(i as u16 + 1),
                    episode_name: String::new(),
                    filename: format!("Show_S01E{:02}.mkv", i + 1),
                })
                .collect(),
        };
        assert!(lib.queue.set_plan(claimed.id, plan));
        let mut job = lib.queue.snapshot().jobs[0].clone();
        job.retire = true;
        let config = t.path().join("config");
        let old: Vec<PathBuf> = (1..=3)
            .map(|e| dirs.library.join(format!("Old/Old E{e:02}.mkv")))
            .collect();
        std::fs::create_dir_all(dirs.library.join("Old")).unwrap();
        for (i, path) in old.iter().enumerate() {
            std::fs::write(path, format!("old episode {i}")).unwrap();
            links::record(&config, path, Path::new(ISO)).unwrap();
        }
        let other = dirs.library.join("Other.mkv");
        std::fs::write(&other, b"another disc").unwrap();
        links::record(&config, &other, Path::new("/i/other.iso")).unwrap();
        let unlinked = dirs.library.join("Show.mkv");
        std::fs::write(&unlinked, b"unlinked").unwrap();
        Self {
            t,
            lib,
            dirs,
            job,
            old,
            other,
            unlinked,
        }
    }

    fn run(
        &self,
        execute: impl FnMut(
            &freemkv_engine::RemuxJob,
            Option<&Path>,
            usize,
        ) -> std::io::Result<Option<String>>,
    ) -> Ending {
        let arbiter = Arbiter::new();
        let mut sink = test_sink(&self.lib, &arbiter);
        sink.job_id = self.job.id;
        remux_planned_with(
            &self.job,
            self.job.plan.as_ref().unwrap(),
            &config_of(&self.dirs),
            &sink,
            execute,
        )
    }

    fn links(&self) -> HashMap<PathBuf, PathBuf> {
        links::read(&self.t.path().join("config")).unwrap()
    }

    fn assert_bystanders_untouched(&self) {
        assert_eq!(std::fs::read(&self.other).unwrap(), b"another disc");
        assert_eq!(std::fs::read(&self.unlinked).unwrap(), b"unlinked");
    }
}

fn config_of(dirs: &Dirs) -> Config {
    Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    }
}

#[test]
fn correction_lands_all_new_outputs_then_removes_exactly_the_three_linked_old_ones() {
    let c = Correction::new(4);
    let mut calls = 0;
    let ending = c.run(|request, _, _| {
        calls += 1;
        assert!(
            c.old.iter().all(|p| p.exists()),
            "old files stay until all land"
        );
        std::fs::create_dir_all(request.target.parent().unwrap()).unwrap();
        std::fs::write(&request.target, movie())?;
        Ok(Some(current_stamp()))
    });
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(calls, 4);
    assert!(c.old.iter().all(|p| !p.exists()));
    c.assert_bystanders_untouched();
    let targets = c.job.output_targets();
    let mut expected: HashMap<PathBuf, PathBuf> = targets
        .iter()
        .map(|t| (t.clone(), PathBuf::from(ISO)))
        .collect();
    expected.insert(c.other.clone(), "/i/other.iso".into());
    assert_eq!(c.links(), expected);
    for target in &targets {
        assert_eq!(std::fs::read(target).unwrap(), movie());
    }
}

#[test]
fn old_file_changed_after_the_remux_started_is_kept_disowned_and_needs_action() {
    let c = Correction::new(2);
    let ending = c.run(|request, _, ordinal| {
        if ordinal == 0 {
            std::fs::write(&c.old[1], b"the user edited this episode").unwrap();
        }
        std::fs::write(&request.target, movie())?;
        Ok(Some(current_stamp()))
    });
    let Ending::Failed(e) = &ending else {
        panic!("{ending:?}")
    };
    assert!(e.to_string().contains("Old E02.mkv"), "{e}");
    assert!(
        e.to_string().starts_with("All 2 new files are in place."),
        "{e}"
    );
    assert!(
        storage_fault(e).is_none(),
        "Needs Action, not an automatic retry"
    );
    assert!(!c.old[0].exists() && !c.old[2].exists());
    assert_eq!(
        std::fs::read(&c.old[1]).unwrap(),
        b"the user edited this episode"
    );
    assert!(!c.links().contains_key(&c.old[1]), "disowned");
    let job = &c.lib.queue.snapshot().jobs[0];
    assert!(job.outputs.iter().all(|o| o.state == OutputState::Done));
    let roots = crate::server::library::owned::roots(&c.dirs);
    let again = crate::server::library::owned::capture(&c.links(), Path::new(ISO), &roots).unwrap();
    assert!(
        again.iter().all(|o| o.path != c.old[1]),
        "never captured again"
    );
}

#[test]
fn retire_identity_is_size_and_time_not_inode_end_to_end() {
    let c = Correction::new(2);
    let ending = c.run(|request, _, ordinal| {
        if ordinal == 0 {
            // An SMB remount: E01 is a new inode with the same size and time.
            let modified = std::fs::metadata(&c.old[0]).unwrap().modified().unwrap();
            let copy = c.old[0].with_extension("copy");
            std::fs::copy(&c.old[0], &copy).unwrap();
            let file = std::fs::File::options().write(true).open(&copy).unwrap();
            file.set_modified(modified).unwrap();
            drop(file);
            std::fs::rename(&copy, &c.old[0]).unwrap();
            // E03 is replaced by a same-size file with another time.
            let modified = std::fs::metadata(&c.old[2]).unwrap().modified().unwrap();
            std::fs::write(&c.old[2], b"old episode X").unwrap();
            let file = std::fs::File::options()
                .write(true)
                .open(&c.old[2])
                .unwrap();
            file.set_modified(modified + Duration::from_secs(10))
                .unwrap();
        }
        std::fs::write(&request.target, movie())?;
        Ok(Some(current_stamp()))
    });
    assert!(matches!(&ending, Ending::Failed(e) if e.to_string().contains("Old E03.mkv")));
    assert!(!c.old[0].exists() && !c.old[1].exists());
    assert_eq!(std::fs::read(&c.old[2]).unwrap(), b"old episode X");
}

#[test]
fn a_stop_during_retire_leaves_the_rest_linked_for_the_next_run() {
    let c = Correction::new(1);
    let ending = c.run(|request, _, _| {
        std::fs::write(&request.target, movie())?;
        c.lib
            .cancel_job
            .store(c.job.id, std::sync::atomic::Ordering::SeqCst);
        Ok(Some(current_stamp()))
    });
    assert!(
        matches!(ending, Ending::Stopped(JobNote::Cancelled)),
        "{ending:?}"
    );
    assert!(c.old.iter().all(|p| p.exists()));
    assert!(
        c.old
            .iter()
            .all(|p| c.links().get(p) == Some(&PathBuf::from(ISO)))
    );
}

#[test]
fn a_plain_remux_of_the_same_iso_overwrites_owned_targets_but_never_deletes() {
    let mut c = Correction::new(2);
    c.job.retire = false;
    let ending = c.run(|request, _, _| {
        std::fs::write(&request.target, movie())?;
        Ok(Some(current_stamp()))
    });
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert!(c.old.iter().all(|p| p.exists()));
    assert!(c.old.iter().all(|p| c.links().contains_key(p)));
    c.assert_bystanders_untouched();
}
