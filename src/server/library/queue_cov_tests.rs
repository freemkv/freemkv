use super::*;
use crate::server::planner::{MediaMetadata, PlannedOutput};

fn plan(n: usize) -> RemuxPlan {
    RemuxPlan {
        media: MediaMetadata::default(),
        outputs: (1..=n)
            .map(|i| PlannedOutput {
                id: format!("o{i}"),
                title_index: i - 1,
                episode: None,
                episode_name: String::new(),
                filename: format!("E{i}.mkv"),
            })
            .collect(),
    }
}

fn new_job(dir: &Path) -> NewJob {
    NewJob {
        title: "A".into(),
        iso: dir.join("A.iso"),
        target: dir.join("lib/A/A.mkv"),
        replace: false,
    }
}

fn job_with(target: &Path, plan: Option<RemuxPlan>) -> Job {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![NewJob {
        title: "A".into(),
        iso: "/i/A.iso".into(),
        target: target.to_path_buf(),
        replace: false,
    }]);
    let mut job = q.snapshot().jobs[0].clone();
    job.plan = plan;
    job.outputs.clear();
    job
}

#[test]
fn an_unsaved_plan_names_its_files_beside_the_target_or_the_target_itself() {
    let target = Path::new("/m/Show/Show.mkv");
    let two = job_with(target, Some(plan(2)));
    assert_eq!(
        two.output_targets(),
        vec![
            PathBuf::from("/m/Show/E1.mkv"),
            PathBuf::from("/m/Show/E2.mkv")
        ]
    );
    let one = job_with(target, Some(plan(1)));
    assert_eq!(one.output_targets(), vec![target.to_path_buf()]);
    let none = job_with(target, None);
    assert_eq!(none.output_targets(), vec![target.to_path_buf()]);
}

#[test]
fn active_and_latest_jobs_are_found_by_target() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    let job = new_job(t.path());
    let target = job.target.clone();
    assert_eq!(q.add(vec![job]), 1);
    let snap = q.snapshot();
    assert_eq!(
        snap.active_for(&target).map(|j| j.id),
        Some(snap.jobs[0].id)
    );
    assert_eq!(
        snap.latest_for(&target).map(|j| j.id),
        Some(snap.jobs[0].id)
    );
    assert!(snap.active_for(Path::new("/elsewhere.mkv")).is_none());
    let id = q.claim_next().unwrap().id;
    q.finish(
        id,
        JobResult::Failed {
            code: None,
            message: "x".into(),
            finished_at: 0,
        },
    );
    let snap = q.snapshot();
    assert!(
        snap.active_for(&target).is_none(),
        "a failed job is not active"
    );
    assert_eq!(snap.latest_for(&target).map(|j| j.id), Some(id));
}

#[test]
fn a_paused_queue_offers_and_hands_out_nothing() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![new_job(t.path())]);
    q.set_paused(true);
    assert!(q.peek_next().is_none());
    assert!(q.claim_next().is_none());
    q.set_paused(false);
    assert!(q.peek_next().is_some());
    assert!(q.retry_unreadable(0), "a loaded queue needs no retry");
}

#[test]
fn edits_for_a_job_or_output_that_is_gone_change_nothing() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![new_job(t.path())]);
    let id = q.claim_next().unwrap().id;
    let before = q.snapshot();
    let gone = id + 100;
    q.finish(
        gone,
        JobResult::Failed {
            code: None,
            message: "x".into(),
            finished_at: 0,
        },
    );
    q.requeue(gone, JobNote::Interrupted);
    q.retry_later(
        gone,
        Failure {
            code: None,
            message: "x".into(),
        },
        0,
        None,
    );
    q.set_staged(gone, None);
    assert!(!q.begin_output(gone, "o1"));
    q.finish_output(gone, "o1", OutputState::Done);
    q.set_output_staged(gone, "o1", Some(t.path().join("k.mkv")));
    let stamp = crate::server::util::Stamp {
        size: 1,
        modified: std::time::SystemTime::UNIX_EPOCH,
    };
    q.set_output_landing(gone, "o1", stamp).unwrap();
    assert!(
        !q.freeze_plan(gone, plan(1), None, None, BTreeMap::new())
            .unwrap()
    );
    let e = q.set_owned(gone, Vec::new()).unwrap_err();
    assert!(e.to_string().contains("no longer exists"), "{e}");
    let staged = StagedFile {
        path: t.path().join("k.mkv"),
        bytes: 1,
        attempts: 1,
    };
    let e = q
        .retain_unproven_output(gone, "o1", staged.clone())
        .unwrap_err();
    assert!(e.to_string().contains("no longer exists"), "{e}");
    let e = q.retain_unproven_output(id, "o1", staged).unwrap_err();
    assert!(e.to_string().contains("no longer exists"), "{e}");
    assert_eq!(q.snapshot().jobs, before.jobs);
}

#[test]
fn a_plan_whose_targets_do_not_match_its_outputs_is_not_frozen() {
    let t = tempfile::tempdir().unwrap();
    let q = Queue::open(t.path());
    q.add(vec![new_job(t.path())]);
    let job = q.claim_next().unwrap();
    let one = Some(vec![t.path().join("one.mkv")]);
    assert!(
        !q.freeze_plan(job.id, plan(2), one, None, BTreeMap::new())
            .unwrap()
    );
    assert!(q.snapshot().jobs[0].plan.is_none());
    assert!(q.set_plan_targets(job.id, plan(2), None).unwrap());
    let saved = q.snapshot().jobs[0].clone();
    let dir = job.target.parent().unwrap();
    assert_eq!(
        saved
            .outputs
            .iter()
            .map(|o| o.target.clone())
            .collect::<Vec<_>>(),
        vec![dir.join("E1.mkv"), dir.join("E2.mkv")]
    );
    assert!(
        !q.set_plan_targets(job.id, plan(2), None).unwrap(),
        "a frozen plan stays"
    );
}

#[test]
fn a_damaged_queue_that_cannot_be_set_aside_holds_and_is_never_written() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join(QUEUE_FILE);
    std::fs::write(&path, b"{damaged").unwrap();
    for n in 0..1000u32 {
        let name = match n {
            0 => format!("{QUEUE_FILE}.unreadable"),
            n => format!("{QUEUE_FILE}.unreadable.{n}"),
        };
        std::fs::write(t.path().join(name), b"").unwrap();
    }
    let q = Queue::open(t.path());
    let why = q.unreadable().expect("the queue holds");
    assert!(why.contains("could not be set aside"), "{why}");
    assert!(why.contains("too many copies"), "{why}");
    assert_eq!(q.add(vec![new_job(t.path())]), 0);
    assert!(!q.begin_output(1, "o1"));
    let e = q.set_owned(1, Vec::new()).unwrap_err();
    assert!(e.to_string().contains("could not be set aside"), "{e}");
    assert_eq!(std::fs::read(&path).unwrap(), b"{damaged");
}
