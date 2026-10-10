use super::super::probe::{MuxedWith, testmkv::mkv};
use super::super::queue::{JobState, NewJob};
use super::super::{Dirs, Library};
use super::*;
use std::path::Path;

struct Lines(Mutex<Vec<String>>);

#[test]
fn presentation_preference_unplanned_movie_cannot_skip_source_planning() {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    lib.queue.add(vec![NewJob {
        title: "Movie".into(),
        iso: t.path().join("missing.iso"),
        target: t.path().join("movie.mkv"),
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    let cfg = Config {
        tv_auto: false,
        presentation_language: "de".into(),
        ..Default::default()
    };
    assert!(maybe_attach_tv_plan(&lib, &cfg, &mut job).is_err());
}

#[test]
fn presentation_preference_single_output_is_frozen_in_queue() {
    use crate::server::planner::{MediaMetadata, MoviePlanner};
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    lib.queue.add(vec![NewJob {
        title: "Movie".into(),
        iso: t.path().join("source.iso"),
        target: t.path().join("movie.mkv"),
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    let cfg = Config {
        presentation_language: "de".into(),
        ..Default::default()
    };
    let plan = MoviePlanner::from_config(&cfg)
        .plan(
            &job.iso,
            &crate::selection_test_fixtures::launch_titles(),
            &MediaMetadata::default(),
            &job.target,
        )
        .unwrap();
    persist_auto_plan(&lib, &cfg, &mut job, plan).unwrap();
    assert_eq!(
        job.plan
            .as_ref()
            .expect("single output must not be discarded")
            .outputs[0]
            .title_index,
        1
    );
    assert_eq!(
        lib.queue.snapshot().jobs[0].plan.as_ref().unwrap().outputs[0].title_index,
        1
    );
}

#[test]
fn replacement_stop_message_does_not_claim_old_files_are_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let mut job = job_for(dir.path(), 1);
    std::fs::write(&job.iso, b"source").unwrap();
    job.replacement = Some(
        super::super::replacement::Replacement::capture(
            &job.iso,
            &std::collections::HashMap::new(),
        )
        .unwrap(),
    );
    let text = safe_text(&job);
    assert!(text.contains("already delivered files are not rolled back"));
    assert!(!text.contains("unchanged"));
    assert!(!text.contains("No MKV was written"));
}

#[test]
fn corrected_destinations_never_replace_unrelated_files() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source.iso");
    let target = t.path().join("movie.mkv");
    let mut links = std::collections::HashMap::new();
    assert!(validate_corrected_targets(&source, std::slice::from_ref(&target), &links).is_ok());
    std::fs::write(&target, b"existing").unwrap();
    assert!(validate_corrected_targets(&source, std::slice::from_ref(&target), &links).is_err());
    links.insert(target.clone(), t.path().join("other.iso"));
    assert!(validate_corrected_targets(&source, std::slice::from_ref(&target), &links).is_err());
    links.insert(target.clone(), source.clone());
    assert!(validate_corrected_targets(&source, std::slice::from_ref(&target), &links).is_ok());
    assert_eq!(std::fs::read(target).unwrap(), b"existing");
}

#[cfg(unix)]
#[test]
fn corrected_destinations_reject_even_recorded_symlinks() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("source.iso");
    let original = t.path().join("original.mkv");
    let target = t.path().join("movie.mkv");
    std::fs::write(&original, b"untouched").unwrap();
    std::os::unix::fs::symlink(&original, &target).unwrap();
    let links = std::collections::HashMap::from([(target.clone(), source.clone())]);
    assert!(validate_corrected_targets(&source, &[target], &links).is_err());
    assert_eq!(std::fs::read(original).unwrap(), b"untouched");
}

#[test]
fn explicit_remux_match_is_not_skipped_by_disabled_auto_lookup() {
    use crate::server::planner::{MediaKind, MediaMetadata};
    for kind in [MediaKind::Movie, MediaKind::Tv] {
        let t = tempfile::tempdir().unwrap();
        let lib = Library::open(t.path(), &t.path().join("logs"));
        let iso = t.path().join("missing.iso");
        lib.source_matches
            .as_ref()
            .unwrap()
            .save(
                &iso,
                0,
                MediaMetadata {
                    title: "Corrected".into(),
                    year: 2000,
                    tmdb_id: 1,
                    kind: Some(kind),
                    ..Default::default()
                },
            )
            .unwrap();
        lib.queue.add(vec![NewJob {
            title: "Wrong".into(),
            iso,
            target: t.path().join("wrong.mkv"),
            replace: false,
        }]);
        let mut job = lib.queue.claim_next().unwrap();
        let cfg = Config {
            tv_auto: false,
            tmdb_api_key: String::new(),
            ..Default::default()
        };
        assert!(
            maybe_attach_tv_plan(&lib, &cfg, &mut job).is_err(),
            "an explicit correction must attempt source planning, not silently use the old job"
        );
        assert!(job.plan.is_none());
        assert!(!job.target.exists());
    }
}

#[test]
fn unreadable_remux_matches_block_automatic_fallback() {
    let t = tempfile::tempdir().unwrap();
    std::fs::write(t.path().join("library-matches.json"), b"broken").unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    lib.queue.add(vec![NewJob {
        title: "Wrong".into(),
        iso: t.path().join("source.iso"),
        target: t.path().join("wrong.mkv"),
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    assert!(maybe_attach_tv_plan(&lib, &Config::default(), &mut job).is_err());
    assert!(!job.target.exists());
}

#[test]
fn corrected_frozen_plan_rejects_a_replaced_iso_before_execution() {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    let iso = t.path().join("source.iso");
    std::fs::write(&iso, b"original image").unwrap();
    let media = crate::server::planner::MediaMetadata {
        title: "Movie".into(),
        year: 2000,
        tmdb_id: 1,
        kind: Some(crate::server::planner::MediaKind::Movie),
        ..Default::default()
    };
    lib.queue
        .add_corrected(
            NewJob {
                title: "Wrong".into(),
                iso: iso.clone(),
                target: t.path().join("Movie.mkv"),
                replace: false,
            },
            super::super::matches::SavedMatch {
                revision: 1,
                media: media.clone(),
            },
        )
        .unwrap();
    let claimed = lib.queue.claim_next().unwrap();
    assert!(lib.queue.set_plan(
        claimed.id,
        crate::server::planner::RemuxPlan {
            version: crate::server::planner::PLAN_VERSION,
            source_iso: iso.clone(),
            media,
            outputs: vec![crate::server::planner::PlannedOutput {
                id: "movie".into(),
                title_index: 0,
                episode: None,
                episode_name: String::new(),
                filename: "Movie.mkv".into(),
            }],
        }
    ));
    let mut job = lib.queue.snapshot().jobs[0].clone();
    std::fs::write(&iso, b"new and different image").unwrap();
    let error = maybe_attach_tv_plan(&lib, &Config::default(), &mut job).unwrap_err();
    assert!(error.to_string().contains("file changed"));
    assert!(!job.target.exists());
}

#[test]
fn successful_or_skipped_tv_planning_enters_remux_once() {
    let calls = std::cell::Cell::new(0);
    let ending = after_tv_planning(Ok(()), || {
        calls.set(calls.get() + 1);
        Ending::Done { writing_app: None }
    });
    assert!(matches!(ending, Ending::Done { .. }));
    assert_eq!(calls.get(), 1);
}

#[test]
fn planner_errors_require_review_without_entering_remux_or_requeueing() {
    use crate::server::planner::PlanError;
    for error in [
        PlanError::NoTitles,
        PlanError::NoOutputs,
        PlanError::DuplicateOutput("episode.mkv".into()),
        PlanError::SelectionNeedsReview("missing authored roster".into()),
    ] {
        let detail = format!("{error:?}");
        let (_t, lib, dirs) = library_with(&["B"]);
        lib.queue.add(vec![NewJob {
            title: "Show".into(),
            iso: dirs.isos.unwrap().join("Show.iso"),
            target: dirs.library.join("Show.mkv"),
            replace: false,
        }]);
        let job = lib.queue.claim_next().unwrap();
        let called = std::cell::Cell::new(false);
        let ending = after_tv_planning(tv_plan_result(Err(error)).map(|_| ()), || {
            called.set(true);
            Ending::Done { writing_app: None }
        });
        assert!(
            !called.get(),
            "planner failure must never fall back to movie remux"
        );
        let arbiter = Arbiter::new();
        let mut sink = test_sink(&lib, &arbiter);
        sink.job_id = job.id;
        finish(&lib, &job, ending, Duration::ZERO, &sink);
        assert!(matches!(lib.queue.snapshot().results.values().next(),
            Some(JobResult::Failed { message, .. }) if message.contains("Title plan needs review") && message.contains(&detail)));
        assert!(lib.queue.claim_next().is_none());
        assert!(!job.target.exists());
    }
}
impl LineSink for Lines {
    fn line(&self, _kind: LineKind, text: String) {
        self.0.lock().unwrap().push(text);
    }
}

fn current_stamp() -> String {
    format!("freemkv {} (gtest)", env!("CARGO_PKG_VERSION"))
}

// Pads an older stamp to the current one's length, so only the bytes differ.
fn old_stamp_like(current: &str) -> String {
    let base = "freemkv 1.6.11 (g";
    format!("{base}{})", "0".repeat(current.len() - base.len() - 1))
}

fn library_with(titles: &[&str]) -> (tempfile::TempDir, Library, Dirs) {
    let t = tempfile::tempdir().unwrap();
    let dirs = Dirs {
        library: t.path().join("movies"),
        tv: None,
        isos: Some(t.path().join("isos")),
        iso_subfolders: false,
    };
    let old = old_stamp_like(&current_stamp());
    for title in titles {
        let mkv_path = dirs.library.join(title).join(format!("{title}.mkv"));
        std::fs::create_dir_all(mkv_path.parent().unwrap()).unwrap();
        std::fs::write(&mkv_path, mkv(&old, Some(60.0), Some(58), true)).unwrap();
        std::fs::create_dir_all(dirs.isos.as_ref().unwrap()).unwrap();
        std::fs::write(
            dirs.isos.as_ref().unwrap().join(format!("{title}.iso")),
            b"iso",
        )
        .unwrap();
    }
    std::fs::create_dir_all(t.path().join("config")).unwrap();
    let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
    (t, lib, dirs)
}

fn test_sink<'a>(lib: &'a Library, arbiter: &'a Arbiter) -> JobSink<'a> {
    JobSink {
        lib,
        job_id: 7,
        arbiter,
        epoch: arbiter.epoch(),
        log: JobLog(Mutex::new(None)),
        debug: false,
        iso_url: "iso:///i/A.iso".into(),
        last_activity: AtomicU64::new(0),
        term: Mutex::new(Term::default()),
    }
}

fn row<'a>(l: &'a super::super::Listing, title: &str) -> &'a super::super::RowView {
    l.rows.iter().find(|r| r.title == title).unwrap()
}

// Overwrite `path` with `bytes` and put the old mtime back: size and mtime
// then match the cached signature, so only a recorded stamp can tell.
fn replace_keeping_sig(path: &Path, bytes: &[u8]) {
    let before = std::fs::metadata(path).unwrap();
    assert_eq!(before.len(), bytes.len() as u64);
    std::fs::write(path, bytes).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(before.modified().unwrap())
        .unwrap();
}

#[test]
fn enqueue_reports_unavailable_matches_instead_of_silent_zero() {
    let (_dir, mut lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.source_matches = Err("damaged match store".into());
    let error = lib.enqueue_checked(&dirs, |_| true).unwrap_err();
    assert!(error.contains("damaged match store"));
    assert!(error.contains("0 jobs queued"));
    assert!(lib.queue.snapshot().jobs.is_empty());
}

#[test]
fn muxed_with_is_current_the_moment_a_remux_finishes_mid_batch() {
    let (_t, lib, dirs) = library_with(&["A", "B", "C"]);
    lib.index_now(&dirs);
    let before = lib.listing(&dirs);
    for t in ["A", "B", "C"] {
        assert!(matches!(
            row(&before, t).muxed_with,
            MuxedWith::Older { .. }
        ));
        assert!(row(&before, t).needs_remux);
    }
    assert_eq!(lib.enqueue(&dirs, |r| r.needs_remux), 3);
    let a = lib.queue.claim_next().unwrap();
    assert_eq!(a.title, "A");

    // The engine lands the new MKV; its stamp arrives with the report.
    let stamp = current_stamp();
    replace_keeping_sig(&a.target, &mkv(&stamp, Some(60.0), Some(58), true));
    let lines = Lines(Mutex::new(Vec::new()));
    finish(
        &lib,
        &a,
        Ending::Done {
            writing_app: Some(stamp.clone()),
        },
        Duration::from_secs(5),
        &lines,
    );

    // B and C are still queued: the batch is far from idle.
    let q = lib.queue.snapshot();
    assert_eq!(q.count(JobState::Queued), 2);
    let after = lib.listing(&dirs);
    let a_row = row(&after, "A");
    assert!(
        matches!(&a_row.muxed_with, MuxedWith::Current { .. }),
        "A must read as current straight away, got {:?}",
        a_row.muxed_with
    );
    assert!(!a_row.needs_remux);
    assert_eq!(a_row.writing_app.as_deref(), Some(stamp.as_str()));
    assert!(matches!(a_row.result, Some(JobResult::Done { .. })));
    assert!(matches!(
        row(&after, "B").muxed_with,
        MuxedWith::Older { .. }
    ));
    assert!(lines.0.lock().unwrap()[0].starts_with("Remuxed in"));
}

#[test]
fn a_file_changed_behind_the_queue_is_re_read_on_the_next_index_pass() {
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    assert!(matches!(
        row(&lib.listing(&dirs), "A").muxed_with,
        MuxedWith::Older { .. }
    ));
    let target = dirs.library.join("A/A.mkv");
    let longer = format!("{} with a longer stamp", current_stamp());
    std::fs::write(&target, mkv(&longer, Some(60.0), Some(58), true)).unwrap();
    lib.index_now(&dirs);
    assert_eq!(
        row(&lib.listing(&dirs), "A").writing_app.as_deref(),
        Some(longer.as_str())
    );
}

#[test]
fn a_failure_keeps_its_code_and_a_preempted_job_goes_back_first() {
    let (_t, lib, dirs) = library_with(&["A", "B"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let lines = Lines(Mutex::new(Vec::new()));
    let a = lib.queue.claim_next().unwrap();
    finish(
        &lib,
        &a,
        Ending::Stopped(JobNote::Preempted),
        Duration::ZERO,
        &lines,
    );
    let again = lib.queue.claim_next().unwrap();
    assert_eq!(again.id, a.id, "a preempted job keeps its place");
    assert_eq!(again.note, Some(JobNote::Preempted));
    let e = std::io::Error::other("E6000: 12 0x03");
    finish(&lib, &again, Ending::Failed(e), Duration::ZERO, &lines);
    let r = lib
        .queue
        .snapshot()
        .results
        .values()
        .next()
        .cloned()
        .unwrap();
    assert!(
        matches!(
            r,
            JobResult::Failed {
                code: Some(6000),
                ..
            }
        ),
        "{r:?}"
    );
}

#[test]
fn no_remux_starts_while_a_rip_holds_the_slot() {
    let (_t, lib, _dirs) = library_with(&[]);
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: "/i/A.iso".into(),
        target: "/m/A/A.mkv".into(),
        replace: true,
    }]);
    let arbiter = Arbiter::new();
    let slot = arbiter.rip();
    let (d, now) = (dirs_at(Path::new("/m")), 0);
    assert!(next_job(&lib, &arbiter, &d, &|_| Ok(()), now).is_none());
    assert_eq!(lib.queue.snapshot().count(JobState::Queued), 1);
    drop(slot);
    assert!(next_job(&lib, &arbiter, &d, &|_| Ok(()), now).is_some());
}

#[test]
fn a_rip_that_starts_mid_remux_is_not_blocked_and_stops_the_remux() {
    let (_t, lib, _dirs) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    std::thread::scope(|s| {
        let remux = s.spawn(|| {
            let t = Instant::now();
            while !sink.should_cancel() {
                assert!(t.elapsed() < Duration::from_secs(10), "never cancelled");
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        std::thread::sleep(Duration::from_millis(50));
        let t = Instant::now();
        let slot = arbiter.rip();
        assert!(t.elapsed() < Duration::from_millis(50), "the rip waited");
        drop(slot);
        remux.join().unwrap();
    });
    assert!(sink.preempted(), "a rip that already ended still counts");
}

#[test]
fn a_new_mkv_is_never_created_in_an_empty_or_foreign_folder() {
    let t = tempfile::tempdir().unwrap();
    let d = super::super::Dirs {
        library: t.path().join("lib"),
        tv: None,
        isos: None,
        iso_subfolders: false,
    };
    let target = d.library.join("A/A.mkv");
    assert!(
        safe_to_create(&d, &target)
            .unwrap_err()
            .contains("cannot be read")
    );
    std::fs::create_dir_all(&d.library).unwrap();
    assert!(safe_to_create(&d, &target).unwrap_err().contains("empty"));
    std::fs::create_dir_all(d.library.join("B")).unwrap();
    assert!(safe_to_create(&d, &target).is_ok());
    assert!(safe_to_create(&d, &t.path().join("elsewhere/A.mkv")).is_err());
}

// KU-E1: a remux whose keys need the disc's Volume ID (never on disk, J6) fails E7034
// before any output. The row says to insert the disc, the job is not re-queued, and
// the old MKV is byte-for-byte unchanged.
#[test]
fn a_remux_needing_the_disc_says_insert_it_and_is_not_retried() {
    let (t, lib, dirs) = library_with(&["A"]);
    let fx = crate::ku_fixture::bd_image();
    let iso = dirs.isos.as_ref().unwrap().join("A.iso");
    std::fs::write(&iso, &fx.img.image).unwrap();
    crate::ku_fixture::write_sidecar(&fx, &iso, true);
    let keydb = t.path().join("keydb.cfg");
    crate::ku_fixture::write_media_key_keydb(&fx, &keydb);
    let mkv_path = dirs.library.join("A/A.mkv");
    let old_mkv = std::fs::read(&mkv_path).unwrap();
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let job = lib.queue.claim_next().expect("the stale MKV is queued");
    assert_eq!(job.iso, iso);
    let cfg = Config {
        keydb_path: Some(keydb.to_string_lossy().into_owned()),
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let arbiter = Arbiter::new();
    let ending = remux(&job, &cfg, &test_sink(&lib, &arbiter));
    let lines = Lines(Mutex::new(Vec::new()));
    finish(&lib, &job, ending, Duration::ZERO, &lines);

    let q = lib.queue.snapshot();
    let r = q.results.values().next().cloned().unwrap();
    let JobResult::Failed { code, message, .. } = r else {
        panic!("{r:?}");
    };
    assert_eq!(code, Some(libfreemkv::error::E_AACS_VID_NEEDS_DISC));
    assert!(message.contains("Insert the disc to finish"), "{message}");
    assert_eq!(q.count(JobState::Failed), 1);
    assert!(lib.queue.claim_next().is_none(), "never retried by itself");
    assert_eq!(
        std::fs::read(&mkv_path).unwrap(),
        old_mkv,
        "the old MKV is kept"
    );
    let said = lines.0.lock().unwrap().join("\n");
    assert!(said.contains("Your existing MKV is unchanged"), "{said}");
}

#[test]
fn a_failure_prints_its_error_once() {
    let (_t, lib, _dirs) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let e = std::io::Error::other("E7013: aa");
    let outcome: Result<&libfreemkv::MuxOutcome, &std::io::Error> = Err(&e);
    sink.event(&Event::TitleDone {
        idx: 0,
        dest: "mkv:///x.partial",
        result: outcome,
    });
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: "/i/A.iso".into(),
        target: "/m/A/A.mkv".into(),
        replace: true,
    }]);
    let job = lib.queue.claim_next().unwrap();
    finish(
        &lib,
        &job,
        Ending::Failed(std::io::Error::other("E7013: aa")),
        Duration::ZERO,
        &sink,
    );
    let errs = lib
        .console_since(0)
        .into_iter()
        .filter(|l| l.kind == LineKind::Err)
        .count();
    assert_eq!(errs, 1, "one error line, not one per layer");
    let f = lib.queue.snapshot().jobs[0].failure.clone().unwrap();
    assert_eq!(f.code, Some(7013));
    assert!(f.message.starts_with("E7013"), "{f:?}");
}

#[test]
fn stop_all_leaves_nothing_running_queued_paused_or_partial() {
    let (_t, lib, dirs) = library_with(&["A", "B", "C"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    lib.queue.set_paused(false);
    let a = lib.queue.claim_next().unwrap();
    let partial = super::super::queue::partial_path(&a.target);
    std::fs::write(&partial, b"half").unwrap();
    lib.queue.set_paused(true);
    let arbiter = Arbiter::new();
    let mut sink = test_sink(&lib, &arbiter);
    sink.job_id = a.id;
    lib.set_running(|r| {
        *r = Some(super::super::Running {
            job_id: a.id,
            ..Default::default()
        })
    });
    assert!(!sink.should_cancel());
    assert!(!lib.running().unwrap().stopping);
    let before = lib.generation();
    assert_eq!(lib.stop_all(), (true, 2));
    assert!(lib.running().unwrap().stopping);
    assert_ne!(
        lib.generation(),
        before,
        "stop must reach live clients immediately"
    );
    assert!(sink.should_cancel(), "the running remux sees the stop");
    // The engine owns cleanup under its artifact lock; the worker records the ending.
    std::fs::remove_file(&partial).unwrap();
    finish(
        &lib,
        &a,
        Ending::Stopped(JobNote::Cancelled),
        Duration::ZERO,
        &sink,
    );
    let q = lib.queue.snapshot();
    assert!(
        q.jobs
            .iter()
            .all(|j| !matches!(j.state, JobState::Queued | JobState::Running)),
        "{q:?}"
    );
    assert!(!q.paused);
    assert!(!partial.exists());
    assert!(a.target.exists(), "the old MKV is kept");
}

#[test]
fn a_retry_appends_to_the_title_log() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("A.log");
    JobLog::create(&p).write(LineKind::Out, "first");
    JobLog::create(&p).write(LineKind::Out, "second");
    let text = std::fs::read_to_string(&p).unwrap();
    assert!(text.contains("first") && text.contains("second"), "{text}");
    assert!(text.contains("another attempt"));
}

#[test]
fn a_disc_read_failure_shows_the_sector_not_the_sense_bytes() {
    let e = std::io::Error::other("E6000: 7476928 0x02/0x03/0x11/0x00");
    let text = error_text(&e);
    assert!(text.contains("7476928"), "{text}");
    assert!(!text.contains("0x02"), "{text}");
}

#[test]
fn a_stop_after_the_file_was_taken_skips_its_audit() {
    let (_t, lib, dirs) = library_with(&["A"]);
    let path = dirs.library.join("A/A.mkv");
    lib.audits.enqueue([path.clone()]);
    assert_eq!(lib.audits.next().as_ref(), Some(&path));
    lib.audits.stop_all();
    quick_one(&lib, &path, &dirs.library, false);
    let sig = super::super::probe::FileSig::stat(&path).unwrap();
    assert!(lib.audits.report(&path, sig).is_none());
    assert!(lib.audits.status().running.is_none());
}

#[test]
fn a_file_the_storage_cannot_read_is_not_requeued_at_once() {
    let (_t, lib, dirs) = library_with(&[]);
    // A directory stats fine but cannot be read as a file.
    let odd = dirs.library.join("odd.mkv");
    std::fs::create_dir_all(&odd).unwrap();
    lib.audits.enqueue([odd.clone()]);
    let path = lib.audits.next().unwrap();
    quick_one(&lib, &path, &dirs.library, false);
    assert!(!lib.audits.is_queued(&odd), "left for the next refill");
    assert!(lib.audits.status().running.is_none());
}

#[test]
fn the_watchdog_only_ever_cancels_its_own_remux() {
    assert_eq!(stall_action(10, false, false), StallAction::None);
    assert_eq!(
        stall_action(STALL_WARN_SECS, false, false),
        StallAction::Warn
    );
    assert_eq!(
        stall_action(STALL_WARN_SECS, true, false),
        StallAction::None
    );
    assert_eq!(
        stall_action(STALL_CANCEL_SECS, true, false),
        StallAction::CancelRemux
    );
    assert_eq!(stall_action(u64::MAX, true, true), StallAction::None);
    let src = crate::server::util::source_lf(include_str!("worker.rs"));
    let body = &src;
    assert!(
        !body.contains(concat!("process", "::exit")),
        "a remux never exits the daemon"
    );
}

fn job_for(dir: &Path, id: u64) -> Job {
    Job {
        id,
        title: "A".into(),
        iso: dir.join("missing.iso"),
        target: dir.join("A/A.mkv"),
        plan: None,
        selected_match: None,
        replacement: None,
        outputs: Vec::new(),
        replace: true,
        state: JobState::Running,
        queued_at: 0,
        started_at: None,
        finished_at: None,
        note: None,
        failure: None,
        attempts: 0,
        first_failed_at: None,
        not_before: None,
        staged: None,
        staged_bytes: None,
        staged_attempts: None,
    }
}

#[test]
fn a_stalled_or_stopped_remux_is_told_to_cancel_and_ends_as_what_stopped_it() {
    let t = tempfile::tempdir().unwrap();
    let (_l, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let cfg = Config::default();
    let job = job_for(t.path(), 7);
    assert!(!sink.should_cancel());
    let ending = remux(&job, &cfg, &sink);
    assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");

    lib.stall_cancel.store(true, Ordering::SeqCst);
    assert!(
        sink.should_cancel(),
        "the watchdog's flag reaches the engine"
    );
    let ending = remux(&job, &cfg, &sink);
    assert!(
        matches!(ending, Ending::Stopped(JobNote::Stalled)),
        "{ending:?}"
    );

    lib.cancel_job.store(7, Ordering::SeqCst);
    let ending = remux(&job, &cfg, &sink);
    assert!(
        matches!(ending, Ending::Stopped(JobNote::Cancelled)),
        "{ending:?}"
    );

    lib.cancel_job.store(0, Ordering::SeqCst);
    lib.stall_cancel.store(false, Ordering::SeqCst);
    let slot = arbiter.rip();
    let ending = remux(&job, &cfg, &sink);
    drop(slot);
    assert!(
        matches!(ending, Ending::Stopped(JobNote::Preempted)),
        "{ending:?}"
    );
}

#[test]
fn the_watchdog_raises_the_stall_flag_for_a_silent_remux() {
    let (_t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter); // last activity: the epoch
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| watchdog(&lib, 7, &sink, &done));
        std::thread::sleep(Duration::from_millis(900));
        done.store(true, Ordering::SeqCst);
    });
    assert!(lib.stall_cancel.load(Ordering::SeqCst));
    assert!(
        lib.console_since(0)
            .iter()
            .any(|l| l.text.contains("cancelling this remux"))
    );
}

#[test]
fn a_stalled_job_fails_and_an_interrupted_one_goes_back_in_the_queue() {
    let (_t, lib, dirs) = library_with(&["A", "B"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let lines = Lines(Mutex::new(Vec::new()));
    let a = lib.queue.claim_next().unwrap();
    finish(
        &lib,
        &a,
        Ending::Stopped(JobNote::Stalled),
        Duration::ZERO,
        &lines,
    );
    let q = lib.queue.snapshot();
    let done = q.jobs.iter().find(|j| j.id == a.id).unwrap();
    assert_eq!(done.state, JobState::Failed);
    assert_eq!(done.failure.as_ref().unwrap().message, "stalled");
    assert!(lines.0.lock().unwrap()[0].contains("no progress"));

    let b = lib.queue.claim_next().unwrap();
    finish(
        &lib,
        &b,
        Ending::Stopped(JobNote::Interrupted),
        Duration::ZERO,
        &lines,
    );
    let q = lib.queue.snapshot();
    let back = q.jobs.iter().find(|j| j.id == b.id).unwrap();
    assert_eq!(
        (back.state, back.note),
        (JobState::Queued, Some(JobNote::Interrupted))
    );
    assert!(!q.results.contains_key(&*b.target.to_string_lossy()));
}

#[test]
fn audits_yield_to_rips_and_to_unpaused_remuxes() {
    let (_t, lib, _d) = library_with(&[]);
    let arbiter = Arbiter::new();
    assert!(!remux_or_rip_busy(&lib, &arbiter));
    let slot = arbiter.rip();
    assert!(remux_or_rip_busy(&lib, &arbiter));
    drop(slot);
    lib.queue.add(vec![NewJob {
        title: "A".into(),
        iso: "/i/A.iso".into(),
        target: "/m/A/A.mkv".into(),
        replace: true,
    }]);
    assert!(remux_or_rip_busy(&lib, &arbiter));
    lib.queue.set_paused(true);
    assert!(
        !remux_or_rip_busy(&lib, &arbiter),
        "a paused queue yields the disks"
    );
}

#[cfg(unix)]
fn fake_ffmpeg(dir: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("ffmpeg");
    std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

#[cfg(unix)]
#[test]
fn a_decode_that_is_interrupted_goes_back_first_and_a_finished_one_is_recorded() {
    let (t, lib, dirs) = library_with(&["A", "B"]);
    let (a, b) = (dirs.library.join("A/A.mkv"), dirs.library.join("B/B.mkv"));
    let sig = super::super::probe::FileSig::stat(&a).unwrap();
    quick_one(&lib, &a, &dirs.library, true);
    assert_eq!(
        lib.audits.next_deep().as_ref(),
        Some(&a),
        "the quick read owes a decode"
    );
    lib.audits.enqueue_deep(&b);
    // `exec` so the kill reaches the sleep itself.
    let slow = fake_ffmpeg(t.path(), "exec sleep 30");
    deep_one(&lib, &a, &dirs.library, &|| true, Some(slow));
    assert_eq!(
        lib.audits.next_deep().as_ref(),
        Some(&a),
        "the interrupted file is first"
    );
    assert!(
        lib.audits.report(&a, sig).is_some(),
        "its quick audit stands"
    );
    assert_eq!(
        lib.audits.deep_view(&a, sig, true).unwrap().state,
        "pending"
    );
    let quick = fake_ffmpeg(t.path(), "exit 0");
    deep_one(&lib, &a, &dirs.library, &|| false, Some(quick));
    assert_eq!(lib.audits.deep_view(&a, sig, true).unwrap().state, "clean");
    assert!(lib.audits.status().running.is_none());
}

#[cfg(unix)]
#[test]
fn quick_audits_finish_while_a_decode_runs() {
    let (t, lib, dirs) = library_with(&["A", "B", "C"]);
    lib.index_now(&dirs);
    lib.set_deep_enabled(true);
    let a = dirs.library.join("A/A.mkv");
    let sig = |p: &Path| super::super::probe::FileSig::stat(p).unwrap();
    quick_one(&lib, &a, &dirs.library, true);
    assert_eq!(lib.audits.next_deep().as_ref(), Some(&a));
    let slow = fake_ffmpeg(t.path(), "exec sleep 30");
    let done = AtomicBool::new(false);
    let stopped_early = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            deep_one(
                &lib,
                &a,
                &dirs.library,
                &|| done.load(Ordering::SeqCst),
                Some(slow),
            );
            stopped_early.store(!done.load(Ordering::SeqCst), Ordering::SeqCst);
        });
        while lib.audits.status().running.as_ref().map(|l| &l.path) != Some(&a) {
            std::thread::sleep(Duration::from_millis(10));
        }
        lib.refill_audits();
        while quick_turn(&lib, &dirs.library, false) {}
        for t in ["B", "C"] {
            let p = dirs.library.join(format!("{t}/{t}.mkv"));
            assert!(
                lib.audits.report(&p, sig(&p)).is_some(),
                "{t} audited meanwhile"
            );
        }
        let live = lib.audits.status().running.unwrap();
        assert_eq!(live.path, a, "the strip still shows the decode");
        let l = lib.listing(&dirs);
        assert!(row(&l, "A").audit_running);
        assert!(row(&l, "B").audit_queued, "B now owes its own decode");
        done.store(true, Ordering::SeqCst);
    });
    assert!(
        !stopped_early.load(Ordering::SeqCst),
        "the quick lane never stopped the decode"
    );
    assert_eq!(
        lib.audits.next_deep().as_ref(),
        Some(&a),
        "the stopped decode waits"
    );
}

#[test]
fn quick_audits_run_with_deep_audit_off() {
    let (_t, lib, dirs) = library_with(&["A", "B"]);
    lib.index_now(&dirs);
    lib.set_deep_enabled(false);
    lib.refill_audits();
    while quick_turn(&lib, &dirs.library, false) {}
    for t in ["A", "B"] {
        let p = dirs.library.join(format!("{t}/{t}.mkv"));
        let s = super::super::probe::FileSig::stat(&p).unwrap();
        assert!(lib.audits.report(&p, s).is_some());
        assert!(lib.audits.deep_view(&p, s, false).is_none());
    }
    assert_eq!(
        lib.audits.status().queued,
        0,
        "no decode is queued while off"
    );
    lib.audits.enqueue([dirs.library.join("A/A.mkv")]);
    assert!(
        !quick_turn(&lib, &dirs.library, true),
        "a rip or remux goes first"
    );
}

fn dirs_at(library: &Path) -> Dirs {
    Dirs {
        library: library.to_path_buf(),
        tv: None,
        isos: None,
        iso_subfolders: false,
    }
}

fn stale() -> Problem {
    Problem::new(
        "output",
        Path::new("/nas/movies"),
        Fault::Stale,
        "Stale file handle (os error 116)".into(),
    )
}

#[test]
fn a_failed_preflight_holds_the_job_and_it_starts_once_the_folder_answers() {
    let _g = crate::server::health::tests::LAST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let arbiter = Arbiter::new();
    let now = 1_000;
    let calls = AtomicU64::new(0);
    let failing = |_: &Path| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(stale())
    };
    assert!(next_job(&lib, &arbiter, &dirs, &failing, now).is_none());
    let hold = lib.hold().expect("held");
    assert!(hold.message.contains("remounted"), "{hold:?}");
    assert_eq!(
        lib.queue.snapshot().count(JobState::Queued),
        1,
        "still queued, not failed"
    );
    // Until the recheck time it is not even probed again.
    let fine = |_: &Path| Ok(());
    assert!(next_job(&lib, &arbiter, &dirs, &fine, now + 5).is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (job, _) = next_job(&lib, &arbiter, &dirs, &fine, now + health::INTERVAL_SECS)
        .expect("resumes on its own");
    assert_eq!(job.title, "A");
    assert!(lib.hold().is_none());
}

#[test]
fn an_unhealthy_folder_holds_new_jobs_but_never_stops_the_running_one() {
    use crate::server::health::tests::{LAST_LOCK, bad_mount, ok_mount, set_mounts};
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_t, lib, dirs) = library_with(&["A", "B"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let arbiter = Arbiter::new();
    let ok = |_: &Path| Ok(());
    let (a, _) = next_job(&lib, &arbiter, &dirs, &ok, 1).unwrap();
    set_mounts(vec![bad_mount("Library", &dirs.library, Fault::Stale)]);
    let sink = test_sink(&lib, &arbiter);
    assert!(
        !sink.should_cancel(),
        "the probe never cancels running work"
    );
    lib.queue.drop_job(a.id);
    assert!(next_job(&lib, &arbiter, &dirs, &ok, 2).is_none());
    let hold = lib.hold().unwrap();
    assert_eq!(
        (hold.role, hold.path.as_path()),
        ("output", dirs.library.as_path())
    );
    assert!(
        hold.message.starts_with("The output folder is unreachable"),
        "{}",
        hold.message
    );
    set_mounts(vec![ok_mount("Library", &dirs.library)]);
    assert_eq!(
        next_job(&lib, &arbiter, &dirs, &ok, 3).unwrap().0.title,
        "B"
    );
    assert!(lib.hold().is_none());
    set_mounts(Vec::new());
}

#[test]
fn an_output_folder_on_an_unmounted_share_holds_the_queue() {
    use crate::server::health::tests::{LAST_LOCK, bad_mount, set_mounts};
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let arbiter = Arbiter::new();
    let ok = |_: &Path| Ok(());
    set_mounts(vec![bad_mount("Library", &dirs.library, Fault::Unmounted)]);
    assert!(next_job(&lib, &arbiter, &dirs, &ok, 1).is_none());
    let hold = lib.hold().expect("held");
    assert!(hold.message.contains("not mounted"), "{}", hold.message);
    assert_eq!(lib.queue.snapshot().count(JobState::Queued), 1);
    set_mounts(Vec::new());
}

fn running_in(lib: &Library, job: &Job, phase: &str) {
    lib.set_running(|r| {
        *r = Some(Running {
            job_id: job.id,
            phase: phase.into(),
            ..Default::default()
        })
    });
}

#[test]
fn a_stale_output_folder_requeues_the_job_with_a_backoff_and_says_why() {
    let _g = crate::server::health::tests::LAST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let a = lib.queue.claim_next().unwrap();
    running_in(&lib, &a, "copy");
    // The share went away under the library: its folder no longer answers.
    std::fs::rename(&dirs.library, t.path().join("unmounted")).unwrap();
    let lines = Lines(Mutex::new(Vec::new()));
    #[cfg(unix)]
    let e = std::io::Error::from_raw_os_error(libc::ESTALE);
    #[cfg(not(unix))]
    let e = std::io::Error::from(std::io::ErrorKind::StaleNetworkFileHandle);
    finish(&lib, &a, Ending::Failed(e), Duration::ZERO, &lines);
    let q = lib.queue.snapshot();
    let j = &q.jobs[0];
    assert_eq!(
        (j.state, j.note, j.attempts),
        (JobState::Queued, Some(JobNote::WaitingForFolder), 1)
    );
    let now = crate::server::util::epoch_secs();
    assert!(j.not_before.unwrap() >= now + 50, "{:?}", j.not_before);
    let msg = &j.failure.as_ref().unwrap().message;
    assert!(
        msg.contains("stopped while copying the new MKV into the output folder"),
        "{msg}"
    );
    assert!(msg.contains("The output folder is missing"), "{msg}");
    assert!(msg.contains("Your existing MKV is unchanged"), "{msg}");
    assert!(
        !q.results.contains_key(&*a.target.to_string_lossy()),
        "not recorded as failed"
    );
    assert!(lib.hold().is_some(), "the queue holds for the folder");
    let said = lines.0.lock().unwrap().join("\n");
    assert!(said.contains("Remount the share"), "{said}");
}

#[test]
fn a_timeout_with_the_folders_fine_is_retried_once_then_left_failed() {
    let _g = crate::server::health::tests::LAST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (_t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let lines = Lines(Mutex::new(Vec::new()));
    let a = lib.queue.claim_next().unwrap();
    finish(
        &lib,
        &a,
        Ending::Failed(std::io::Error::other("E9073: copy")),
        Duration::ZERO,
        &lines,
    );
    let now = crate::server::util::epoch_secs();
    let again = lib
        .queue
        .claim_next()
        .expect("re-resolved: retried at once");
    assert!(again.not_before.unwrap() <= now);
    assert_eq!(again.attempts, 1);
    finish(
        &lib,
        &again,
        Ending::Failed(std::io::Error::other("E9073: copy")),
        Duration::ZERO,
        &lines,
    );
    let q = lib.queue.snapshot();
    assert_eq!(q.jobs[0].state, JobState::Failed);
    let msg = &q.jobs[0].failure.as_ref().unwrap().message;
    assert!(msg.starts_with("E9073"), "{msg}");
    assert!(msg.contains("stopped while copying"), "{msg}");
    assert!(msg.contains("Your existing MKV is unchanged"), "{msg}");
}

#[test]
fn content_failures_are_never_retried() {
    let (_t, lib, dirs) = library_with(&["A", "B"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let lines = Lines(Mutex::new(Vec::new()));
    for err in ["E9077: runtime-mismatch 10.0/60.0 /x", "E9078: 1"] {
        let j = lib.queue.claim_next().unwrap();
        assert_eq!(storage_fault(&std::io::Error::other(err)), None);
        finish(
            &lib,
            &j,
            Ending::Failed(std::io::Error::other(err)),
            Duration::ZERO,
            &lines,
        );
    }
    let q = lib.queue.snapshot();
    assert_eq!(q.count(JobState::Failed), 2);
    assert!(lib.queue.claim_next().is_none());
}

#[test]
fn storage_errors_are_told_from_content_errors() {
    assert_eq!(
        storage_fault(&std::io::Error::other("E9073: verify")),
        Some(Fault::Unresponsive)
    );
    assert_eq!(
        storage_fault(&std::io::Error::other("E9056")),
        Some(Fault::Unresponsive)
    );
    assert_eq!(storage_fault(&std::io::Error::other("E7013: aa")), None);
    assert_eq!(
        storage_fault(&std::io::ErrorKind::TimedOut.into()),
        Some(Fault::Unresponsive)
    );
    #[cfg(unix)]
    for (n, f) in [
        (libc::ESTALE, Fault::Stale),
        (libc::EIO, Fault::Io),
        (libc::EROFS, Fault::ReadOnly),
    ] {
        assert_eq!(
            storage_fault(&std::io::Error::from_raw_os_error(n)),
            Some(f)
        );
    }
    assert_eq!(
        phase_of(&std::io::Error::other("E9073: copy")).as_deref(),
        Some("copy")
    );
    assert_eq!(
        phase_of(&std::io::Error::other("E9073: artifact_lock")),
        None
    );
}

#[test]
fn the_failure_message_names_the_phase_the_folder_and_what_is_safe() {
    let t = tempfile::tempdir().unwrap();
    let mut job = job_for(t.path(), 1);
    let m = failure_message(
        "E9073 Timed out",
        Some("sync"),
        Some(&stale()),
        &job,
        Some(5),
    );
    assert_eq!(
        m,
        "E9073 Timed out. It stopped while flushing the new file to disk. The output folder is \
             unreachable — the network share needs to be remounted (stale file handle). \
             Folder: /nas/movies. Your existing MKV is unchanged. It is retried automatically once the \
             folder checks out."
    );
    job.replace = false;
    let m = failure_message("E9077 x", Some("verify"), None, &job, None);
    assert_eq!(
        m,
        "E9077 x. It stopped while verifying the new file. No MKV was written to the library."
    );
}

// A kept pair as the delivery writes it: the MKV and its sidecar, in `stage`.
fn write_kept(
    stage: &Path,
    name: &str,
    target: &Path,
    mkv_bytes: &[u8],
    created_at: u64,
) -> std::path::PathBuf {
    super::deliver::tests::write_kept(stage, name, target, mkv_bytes, created_at)
}

fn kept_error(path: &Path, cause: std::io::Error) -> std::io::Error {
    let kind = cause.kind();
    let sidecar = path.with_extension("json");
    let kept = super::deliver::Kept {
        path: path.to_path_buf(),
        sidecar,
        phase: "copy",
        cause,
    };
    std::io::Error::new(kind, kept)
}

fn movie() -> Vec<u8> {
    mkv(&current_stamp(), Some(60.0), Some(58), true)
}

#[path = "worker_fanout_tests.rs"]
mod fanout;

#[test]
fn a_kept_file_after_a_storage_fault_waits_to_be_copied_in() {
    let _g = crate::server::health::tests::LAST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let a = lib.queue.claim_next().unwrap();
    let kept = write_kept(&t.path().join("stage"), "A.1", &a.target, &movie(), 1);
    std::fs::rename(&dirs.library, t.path().join("unmounted")).unwrap();
    #[cfg(unix)]
    let cause = std::io::Error::from_raw_os_error(libc::ESTALE);
    #[cfg(not(unix))]
    let cause = std::io::Error::from(std::io::ErrorKind::StaleNetworkFileHandle);
    let lines = Lines(Mutex::new(Vec::new()));
    let e = kept_error(&kept, cause);
    finish(&lib, &a, Ending::Failed(e), Duration::ZERO, &lines);
    let q = lib.queue.snapshot();
    let j = &q.jobs[0];
    assert_eq!(
        (j.state, j.note, j.attempts),
        (JobState::Queued, Some(JobNote::StagedWaiting), 1)
    );
    assert_eq!(j.staged.as_deref(), Some(kept.as_path()));
    assert_eq!(j.staged_bytes, Some(movie().len() as u64));
    let msg = &j.failure.as_ref().unwrap().message;
    assert!(msg.contains("waits on local staging"), "{msg}");
    assert!(lib.hold().is_some(), "held for the output folder");
    let said = lines.0.lock().unwrap().join("\n");
    assert!(
        said.contains("only the copy into the output folder is retried"),
        "{said}"
    );
}

#[test]
fn a_kept_file_whose_copy_did_not_match_fails_and_stays_offered() {
    let (t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let a = lib.queue.claim_next().unwrap();
    let kept = write_kept(&t.path().join("stage"), "A.1", &a.target, &movie(), 1);
    let e = kept_error(&kept, std::io::Error::other("E9080: 10/20"));
    let lines = Lines(Mutex::new(Vec::new()));
    finish(&lib, &a, Ending::Failed(e), Duration::ZERO, &lines);
    let q = lib.queue.snapshot();
    let j = &q.jobs[0];
    assert_eq!(
        j.state,
        JobState::Failed,
        "a content failure is not retried by itself"
    );
    assert_eq!(j.staged.as_deref(), Some(kept.as_path()));
    let msg = &j.failure.as_ref().unwrap().message;
    assert!(msg.starts_with("E9080"), "{msg}");
    assert!(msg.contains("Retry copies it in"), "{msg}");
    assert!(lib.queue.claim_next().is_none());
    assert_eq!(
        lib.queue.add(vec![NewJob {
            title: "A".into(),
            iso: a.iso.clone(),
            target: a.target.clone(),
            replace: true,
        }]),
        0,
        "a target with a kept file is retried or discarded, not queued again"
    );
    assert!(lib.queue.retry_staged_now(&a.target));
    let again = lib.queue.claim_next().unwrap();
    assert_eq!((again.id, again.note), (a.id, Some(JobNote::StagedWaiting)));
}

#[test]
fn a_retry_with_a_kept_file_only_copies_it_in() {
    let (t, lib, dirs) = library_with(&["B"]);
    let target = dirs.library.join("A/A.mkv");
    let kept = write_kept(&t.path().join("stage"), "A.1", &target, &movie(), 1);
    let mut job = job_for(t.path(), 7);
    job.target = target.clone();
    job.replace = false;
    job.staged = Some(kept.clone());
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let cfg = Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let ending = remux(&job, &cfg, &sink);
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        movie(),
        "the kept file landed"
    );
    assert!(
        !kept.exists() && !kept.with_extension("json").exists(),
        "and the pair is gone"
    );
}

#[test]
fn a_library_file_that_changed_leaves_the_kept_file_and_offers_discard() {
    let (t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let mut a = lib.queue.claim_next().unwrap();
    // The sidecar says the target was absent; it is there now.
    let kept = write_kept(&t.path().join("stage"), "A.1", &a.target, &movie(), 1);
    a.staged = Some(kept.clone());
    let arbiter = Arbiter::new();
    let sink = test_sink(&lib, &arbiter);
    let cfg = Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        ..Config::default()
    };
    let ending = remux(&a, &cfg, &sink);
    let Ending::Failed(e) = ending else {
        panic!("{ending:?}");
    };
    assert_eq!(
        freemkv_engine::error_code(&e),
        Some(libfreemkv::error::E_REMUX_TARGET_EXISTS)
    );
    let lines = Lines(Mutex::new(Vec::new()));
    finish(&lib, &a, Ending::Failed(e), Duration::ZERO, &lines);
    let j = lib.queue.snapshot().jobs[0].clone();
    assert_eq!(j.state, JobState::Failed);
    assert_eq!(
        j.staged.as_deref(),
        Some(kept.as_path()),
        "the pair is left"
    );
    let msg = j.failure.unwrap().message;
    assert!(msg.contains("changed after this remux began"), "{msg}");
    assert!(msg.contains("Discard it"), "{msg}");
    assert!(kept.exists());
}

#[test]
fn a_stopped_copy_in_leaves_the_kept_file() {
    let (t, lib, dirs) = library_with(&["A"]);
    lib.index_now(&dirs);
    lib.enqueue(&dirs, |_| true);
    let mut a = lib.queue.claim_next().unwrap();
    let kept = write_kept(&t.path().join("stage"), "A.1", &a.target, &movie(), 1);
    a.staged = Some(kept.clone());
    let lines = Lines(Mutex::new(Vec::new()));
    finish(
        &lib,
        &a,
        Ending::Stopped(JobNote::Cancelled),
        Duration::ZERO,
        &lines,
    );
    let j = lib.queue.snapshot().jobs[0].clone();
    assert_eq!(
        (j.state, j.staged.as_deref()),
        (JobState::Failed, Some(kept.as_path()))
    );
    assert!(kept.exists());
}

#[test]
fn startup_housekeeping_bounds_staging_and_requeues_what_survives() {
    let t = tempfile::tempdir().unwrap();
    let cfg_dir = t.path().join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    // A queue file from before kept files existed, with a job for C.
    std::fs::write(
        cfg_dir.join(super::super::queue::QUEUE_FILE),
        r#"{"schema":1,"next_id":1,"jobs":[{"id":1,"title":"C","iso":"/i/C.iso",
                "target":"/m/C/C.mkv","replace":true,"state":"failed","queued_at":1}]}"#,
    )
    .unwrap();
    let lib = Library::open(&cfg_dir, &t.path().join("logs"));
    let stage = t.path().join("stage");
    let now = crate::server::util::epoch_secs();
    let day = 86_400;
    let old = write_kept(
        &stage,
        "Old.1",
        Path::new("/m/Old/Old.mkv"),
        &[1; 100],
        now - 8 * day,
    );
    let big = write_kept(
        &stage,
        "Big.1",
        Path::new("/m/Big/Big.mkv"),
        &[1; 300],
        now - 2 * day,
    );
    let a = write_kept(&stage, "A.1", Path::new("/m/A/A.mkv"), &[1; 100], now - day);
    let c = write_kept(&stage, "C.1", Path::new("/m/C/C.mkv"), &[1; 100], now);
    let orphan = stage.join("Lone.2.staged.mkv");
    std::fs::write(&orphan, b"no sidecar").unwrap();
    let half = stage.join("A.1.staged.json.tmp");
    std::fs::write(&half, b"{").unwrap();
    let interrupted = stage.join("9.mkv.partial");
    std::fs::write(&interrupted, b"x").unwrap();
    let limits = StagingLimits {
        max_age: Duration::from_secs(7 * day),
        max_bytes: 250,
    };
    let gone = housekeep_staging(&lib, &stage, &limits, Path::new("/m"));
    assert_eq!(gone, 2, "the expired file and the oldest over the budget");
    for p in [&old, &big, &orphan, &half, &interrupted] {
        assert!(!p.exists(), "{} removed", p.display());
    }
    assert!(a.exists() && c.exists());
    let q = lib.queue.snapshot();
    let job = |t: &str| {
        q.jobs
            .iter()
            .find(|j| j.target == Path::new(t))
            .unwrap()
            .clone()
    };
    let ja = job("/m/A/A.mkv");
    assert_eq!(
        (
            ja.state,
            ja.note,
            ja.staged.as_deref(),
            ja.staged_bytes,
            ja.title.as_str()
        ),
        (
            JobState::Queued,
            Some(JobNote::StagedWaiting),
            Some(a.as_path()),
            Some(100),
            "A"
        )
    );
    let jc = job("/m/C/C.mkv");
    assert_eq!(
        (jc.id, jc.state, jc.staged.as_deref()),
        (1, JobState::Queued, Some(c.as_path())),
        "the old job is reused"
    );
    assert_eq!(q.jobs.len(), 2);
    // Gone on the next start: the job forgets it and leaves the queue.
    super::deliver::discard(&a).unwrap();
    housekeep_staging(&lib, &stage, &limits, Path::new("/m"));
    let ja = lib
        .queue
        .snapshot()
        .jobs
        .into_iter()
        .find(|j| j.target == Path::new("/m/A/A.mkv"))
        .unwrap();
    assert_eq!((ja.state, ja.staged), (JobState::Failed, None));
    assert!(ja.failure.unwrap().message.contains("is gone"));
}

#[test]
fn the_default_staging_budget_is_a_quarter_of_the_disk_at_most_500_gb() {
    let t = tempfile::tempdir().unwrap();
    let l = StagingLimits::of(&Config::default(), t.path());
    assert_eq!(l.max_age, Duration::from_secs(7 * 86_400));
    assert!(l.max_bytes > 0 && l.max_bytes <= STAGED_MAX_BYTES);
    let cfg = Config {
        remux_staged_max_age_days: 2,
        remux_staged_max_gb: 3,
        ..Config::default()
    };
    let l = StagingLimits::of(&cfg, t.path());
    assert_eq!(
        (l.max_age.as_secs(), l.max_bytes),
        (2 * 86_400, 3_000_000_000)
    );
}
