use super::*;
use crate::server::library::deep::{Run, classify};
use crate::server::library::probe::testmkv::mkv;

fn file(dir: &Path, name: &str) -> (PathBuf, FileSig) {
    let p = dir.join(name);
    std::fs::write(&p, mkv("freemkv 1.7.7", Some(60.0), Some(58), true)).unwrap();
    let sig = FileSig::stat(&p).unwrap();
    (p, sig)
}

fn audited(a: &Audits, p: &Path, sig: FileSig) {
    let report = super::super::probe::audit_fast(p).unwrap();
    a.record_fast(p, sig, report, 1);
}

fn verdict(timed_out: bool) -> Verdict {
    let run = Run {
        rc: Some(0),
        timed_out,
        ..Default::default()
    };
    classify(&run, "decode", 1)
}

#[test]
fn the_queue_fills_with_what_needs_an_audit_and_persists() {
    let t = tempfile::tempdir().unwrap();
    let (a_path, a_sig) = file(t.path(), "A.mkv");
    let (b_path, b_sig) = file(t.path(), "B.mkv");
    let files = vec![(a_path.clone(), a_sig), (b_path.clone(), b_sig)];
    let a = Audits::open(t.path());
    assert_eq!(a.fill(&files, false, 1, true), 2);
    assert_eq!(a.fill(&files, false, 1, true), 0, "already waiting");
    assert_eq!(a.next(), Some(a_path.clone()));
    audited(&a, &a_path, a_sig);
    let reopened = Audits::open(t.path());
    assert!(
        reopened.report(&a_path, a_sig).is_some(),
        "the verdict survives a restart"
    );
    assert_eq!(reopened.status().queued, 1, "so does the queue");
    assert_eq!(
        reopened.fill(&files, false, 1, true),
        0,
        "A is done, B still waits"
    );
}

#[test]
fn deep_audit_on_queues_the_full_decode_and_reaudit_redoes_it() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let a = Audits::open(t.path());
    a.fill(&[(p.clone(), sig)], true, 1, true);
    a.next();
    audited(&a, &p, sig);
    a.finish();
    assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "pending");
    assert_eq!(
        a.fill(&[(p.clone(), sig)], true, 1, true),
        1,
        "the decode is owed"
    );
    assert!(a.next().is_none(), "on the deep lane, not the quick one");
    assert_eq!(a.next_deep(), Some(p.clone()));
    a.record_deep(&p, sig, verdict(false), 2);
    a.finish_deep();
    assert_eq!(a.deep_view(&p, sig, false).unwrap().state, "clean");
    assert_eq!(a.fill(&[(p.clone(), sig)], true, 3, true), 0);
    assert_eq!(a.reaudit(std::slice::from_ref(&p)), 1);
    assert!(a.deep_view(&p, sig, true).unwrap().state == "pending");
    assert!(
        a.is_queued(&p),
        "the quick read is redone when its turn comes"
    );
    assert!(
        a.report(&p, sig).is_some(),
        "the row keeps its tracks meanwhile"
    );
}

#[test]
fn an_inconclusive_decode_retries_with_backoff() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let a = Audits::open(t.path());
    audited(&a, &p, sig);
    a.record_deep(&p, sig, verdict(true), 1000);
    assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "aborted");
    assert!(!a.deep_due_for(&p, sig, 1000 + 599));
    assert!(a.deep_due_for(&p, sig, 1000 + 600));
}

#[test]
fn stop_all_empties_the_queue_and_cancels_the_running_audit() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let (q, qs) = file(t.path(), "B.mkv");
    let a = Audits::open(t.path());
    a.fill(&[(p.clone(), sig), (q, qs)], false, 1, true);
    let running = a.next().unwrap();
    a.start(&running, "A".into());
    assert_eq!(a.stop_all(), 1);
    assert!(a.cancelled());
    a.finish();
    assert!(!a.cancelled());
    assert_eq!(a.status().queued, 0);
    assert!(a.status().paused, "stop remains stopped until resume");
}

#[test]
fn a_stopped_audit_does_not_refill_until_resumed() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let a = Audits::open(t.path());
    a.fill(&[(p.clone(), sig)], false, 1, true);
    assert_eq!(a.stop_all(), 1);
    assert!(a.paused());
    assert_eq!(a.fill(&[(p.clone(), sig)], false, 2, true), 0);
    assert_eq!(a.status().queued, 0);
    a.set_paused(false);
    assert_eq!(a.fill(&[(p, sig)], false, 2, true), 1);
    assert!(a.next().is_some());
}

#[test]
fn progress_fills_one_bar_across_both_stages() {
    let t = tempfile::tempdir().unwrap();
    let a = Audits::open(t.path());
    a.start_deep(Path::new("/m/A.mkv"), "A".into());
    a.progress("reading", 50.0, Some(100.0));
    assert_eq!(a.status().running.unwrap().pct, Some(7.5));
    a.progress("decoding", 100.0, Some(100.0));
    assert_eq!(a.status().running.unwrap().pct, Some(100.0));
}

#[test]
fn a_stop_between_next_and_start_still_stops_that_file() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let a = Audits::open(t.path());
    a.fill(&[(p.clone(), sig)], false, 1, true);
    let taken = a.next().unwrap();
    assert_eq!(a.enqueue([taken.clone()]), 0, "it is running, not waiting");
    a.stop_all();
    assert!(a.cancelled(), "the stop reaches the file already taken");
    a.start(&taken, "A".into());
    assert!(a.cancelled(), "starting does not forget the stop");
}

#[test]
fn an_unreadable_state_file_is_kept_not_overwritten() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join(FILE), b"{ not json").unwrap();
    let a = Audits::open(t.path());
    let (p, sig) = file(t.path(), "A.mkv");
    a.fill(&[(p, sig)], false, 1, true);
    let kept = std::fs::read(t.path().join("audit.json.unreadable")).unwrap();
    assert_eq!(kept, b"{ not json");
}

#[test]
fn a_paused_queue_hands_out_nothing() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let a = Audits::open(t.path());
    a.fill(&[(p, sig)], false, 1, true);
    a.set_paused(true);
    assert!(a.next().is_none());
    a.set_paused(false);
    assert!(a.next().is_some());
}

#[test]
fn a_complete_fill_drops_vanished_files_and_an_incomplete_one_keeps_them() {
    let t = tempfile::tempdir().unwrap();
    let (a_path, a_sig) = file(t.path(), "A.mkv");
    let (b_path, b_sig) = file(t.path(), "B.mkv");
    let a = Audits::open(t.path());
    audited(&a, &a_path, a_sig);
    audited(&a, &b_path, b_sig);
    a.enqueue([b_path.clone()]);
    a.fill(&[(a_path.clone(), a_sig)], false, 1, false);
    assert!(
        a.report(&b_path, b_sig).is_some(),
        "an incomplete scan proves nothing"
    );
    assert!(a.is_queued(&b_path));
    a.fill(&[(a_path.clone(), a_sig)], false, 1, true);
    assert!(a.report(&b_path, b_sig).is_none());
    assert!(!a.is_queued(&b_path));
    assert!(
        a.report(&a_path, a_sig).is_some(),
        "present files keep theirs"
    );
}

#[test]
fn a_changed_file_loses_its_deep_verdict_and_is_requeued() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let a = Audits::open(t.path());
    audited(&a, &p, sig);
    a.record_deep(&p, sig, verdict(false), 2);
    assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "clean");
    std::fs::write(
        &p,
        mkv("freemkv 1.7.7 rewritten", Some(60.0), Some(58), true),
    )
    .unwrap();
    let new = FileSig::stat(&p).unwrap();
    assert_ne!(new, sig);
    assert!(
        a.report(&p, new).is_none(),
        "the old report is not the new file's"
    );
    assert_eq!(a.fill(&[(p.clone(), new)], true, 3, true), 1, "requeued");
    audited(&a, &p, new);
    assert_eq!(a.deep_view(&p, new, true).unwrap().state, "pending");
}

#[test]
fn a_decode_of_a_file_changed_mid_run_is_not_recorded() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let a = Audits::open(t.path());
    audited(&a, &p, sig);
    std::fs::write(
        &p,
        mkv("freemkv 1.7.7 rewritten", Some(60.0), Some(58), true),
    )
    .unwrap();
    a.record_deep(&p, sig, verdict(false), 2);
    assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "pending");
}

#[test]
fn an_interrupted_file_goes_back_first_and_the_backoff_doubles() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let (q, _) = file(t.path(), "B.mkv");
    let a = Audits::open(t.path());
    a.enqueue_deep(&q);
    a.requeue_front(p.clone());
    assert_eq!(a.next_deep(), Some(p.clone()), "front, not back");
    a.requeue_front(q.clone());
    a.requeue_front(q.clone());
    assert_eq!(a.status().queued, 1, "no duplicate in the queue");

    audited(&a, &p, sig);
    a.record_deep(&p, sig, verdict(true), 1000);
    a.record_deep(&p, sig, verdict(true), 2000);
    // Two failed tries: the delay is 1200 s, not 600.
    assert!(!a.deep_due_for(&p, sig, 2000 + 1199));
    assert!(a.deep_due_for(&p, sig, 2000 + 1200));
    // With deep audit off, an inconclusive try shows nothing.
    assert!(a.deep_view(&p, sig, false).is_none());
}

#[test]
fn a_state_file_from_before_the_deep_lane_loads_into_the_quick_lane() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let (q, qs) = file(t.path(), "B.mkv");
    {
        let a = Audits::open(t.path());
        audited(&a, &p, sig);
        a.enqueue([q.clone()]);
    }
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(t.path().join(FILE)).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("deep_queue");
    assert_eq!(
        old.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["paused", "queue", "results"]
    );
    std::fs::write(t.path().join(FILE), old.to_string()).unwrap();
    let a = Audits::open(t.path());
    assert!(a.report(&p, sig).is_some(), "results load unchanged");
    assert!(!t.path().join("audit.json.unreadable").exists());
    assert_eq!(a.status().queued, 1);
    assert_eq!(
        a.fill(&[(p.clone(), sig), (q.clone(), qs)], true, 1, true),
        1,
        "A owes its decode on the deep lane; B already waits"
    );
    assert_eq!(a.next(), Some(q), "the old queue is the quick lane");
    assert_eq!(a.next_deep(), Some(p));
}

// audit.json as 1.7.7 wrote it, before the media detail: two verdicts (one with a
// corrupt deep run), a quick and a deep queue.
const STATE_1_7_7: &str = include_str!("testdata/audit-1.7.7.json");

#[test]
fn a_state_file_from_1_7_7_loads_whole_and_marks_its_detail_pending() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join(FILE), STATE_1_7_7).unwrap();
    let check = |a: &Audits| {
        let st = a.lock();
        assert_eq!(st.results.len(), 2);
        let arrival = &st.results[Path::new("/srv/movies/Arrival (2016)/Arrival (2016).mkv")];
        assert!(arrival.report.ok && arrival.report.detail.is_none());
        assert!(arrival.report.needs_detail());
        assert_eq!(arrival.report.audio[0].codec, "TrueHD");
        let deep = &arrival.deep.as_ref().unwrap().verdict;
        assert_eq!(
            (deep.reason.as_str(), deep.errors, deep.clean),
            ("decode_errors", 2, false)
        );
        assert_eq!((deep.forensic.as_ref(), deep.reached_secs), (None, None));
        let heat = &st.results[Path::new("/srv/movies/Heat (1995)/Heat (1995).mkv")];
        assert!(!heat.report.ok);
        assert!(matches!(
            heat.report.issues[0],
            super::super::probe::AuditIssue::RuntimeMismatch { .. }
        ));
        assert_eq!(st.queue.len(), 1);
        assert_eq!(st.deep_queue.len(), 1);
    };
    let a = Audits::open(t.path());
    assert!(!t.path().join("audit.json.unreadable").exists());
    check(&a);
    // Written back by this version and read again: nothing lost on the way.
    a.set_paused(false);
    check(&Audits::open(t.path()));
}

// A report as stored before the media detail existed.
fn audited_without_detail(a: &Audits, p: &Path, sig: FileSig) {
    let mut report = super::super::probe::audit_fast(p).unwrap();
    report.detail = None;
    a.record_fast(p, sig, report, 1);
}

#[test]
fn stored_reports_get_their_detail_a_few_at_a_time_on_an_idle_quick_lane() {
    let t = tempfile::tempdir().unwrap();
    let files: Vec<(PathBuf, FileSig)> = (0..5)
        .map(|i| file(t.path(), &format!("{i}.mkv")))
        .collect();
    let a = Audits::open(t.path());
    for (p, sig) in &files {
        audited_without_detail(&a, p, *sig);
    }
    let (p0, s0) = files[0].clone();
    a.record_deep(&p0, s0, verdict(false), 5);
    let now = 10_000;
    assert_eq!(a.fill(&files, false, now, true), BACKFILL_BATCH);
    assert_eq!(a.fill(&files, false, now + 1, true), 0, "they wait already");
    while let Some(p) = a.next() {
        let sig = FileSig::stat(&p).unwrap();
        a.record_fast(&p, sig, super::super::probe::audit_fast(&p).unwrap(), now);
        a.finish();
    }
    assert!(!a.report(&p0, s0).unwrap().needs_detail());
    assert!(a.report(&p0, s0).unwrap().ok);
    assert_eq!(
        a.deep_view(&p0, s0, true).unwrap().state,
        "clean",
        "the deep verdict stays"
    );
    assert_eq!(
        a.fill(&files, false, now + 30, true),
        0,
        "not before a minute has passed"
    );
    let a = Audits::open(t.path());
    let (p5, s5) = file(t.path(), "new.mkv");
    let mut with_new = files.clone();
    with_new.push((p5.clone(), s5));
    assert_eq!(
        a.fill(&with_new, false, now + 120, true),
        1,
        "new work first, no backfill beside it"
    );
    assert_eq!(a.next(), Some(p5.clone()));
    audited(&a, &p5, s5);
    a.finish();
    assert_eq!(a.fill(&with_new, false, now + 180, true), 2, "the rest");
    assert_eq!(a.raw(&p0).map(|r| r.is_empty()), Some(false));
}

#[test]
fn stop_all_and_pause_act_on_both_lanes() {
    let t = tempfile::tempdir().unwrap();
    let (p, sig) = file(t.path(), "A.mkv");
    let (q, _) = file(t.path(), "B.mkv");
    let (r, _) = file(t.path(), "C.mkv");
    let a = Audits::open(t.path());
    audited(&a, &p, sig);
    a.enqueue([q.clone(), r.clone()]);
    a.enqueue_deep(&p);
    a.set_paused(true);
    assert!(a.next().is_none() && a.next_deep().is_none());
    a.set_paused(false);
    let deep = a.next_deep().unwrap();
    a.start_deep(&deep, "A".into());
    let quick = a.next().unwrap();
    assert_eq!(a.running_set().len(), 2);
    assert_eq!(a.status().running.unwrap().path, p, "the decode shows");
    assert_eq!(a.stop_all(), 1);
    assert!(a.cancelled() && a.cancelled_deep());
    a.finish();
    assert!(
        !a.cancelled() && a.cancelled_deep(),
        "each lane clears its own stop"
    );
    a.finish_deep();
    assert_eq!(quick, q);
    assert_eq!(a.status().queued, 0);
}
