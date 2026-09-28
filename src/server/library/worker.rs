//! The remux worker: one job at a time through `freemkv_engine::remux_iso`,
//! yielding the mux slot to any rip, with a watchdog that only ever cancels
//! its own job.

use super::arbiter::Arbiter;
use super::queue::{Job, JobNote, JobResult};
use super::{Library, Running};
use crate::server::config::Config;
use freemkv_engine::{Event, Level, Progress, Sink};
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

fn shutting_down() -> bool {
    crate::server::SHUTDOWN.load(Ordering::Relaxed)
}

fn nap(d: Duration) {
    let until = Instant::now() + d;
    while Instant::now() < until && !shutting_down() {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The worker loop: waits while a rip holds the slot or the queue is paused.
pub fn run(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>, arbiter: &Arbiter) {
    tracing::info!("library worker starting");
    while !shutting_down() {
        let Some((job, epoch)) = next_job(lib, arbiter) else {
            nap(Duration::from_secs(1));
            continue;
        };
        let snapshot = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
        run_job(lib, &snapshot, arbiter, epoch, job);
    }
    tracing::info!("library worker stopping");
}

// The next job and the arbiter epoch it started under; nothing while a rip runs.
fn next_job(lib: &Library, arbiter: &Arbiter) -> Option<(Job, u64)> {
    if arbiter.rip_active() {
        return None;
    }
    let epoch = arbiter.epoch();
    lib.queue.claim_next().map(|j| (j, epoch))
}

/// How one job ended, before it is written to the queue.
#[derive(Debug)]
pub(crate) enum Ending {
    Done {
        writing_app: Option<String>,
    },
    /// Cancelled for a rip, a shutdown, or a stall.
    Stopped(JobNote),
    Failed(std::io::Error),
}

fn run_job(lib: &Library, cfg: &Config, arbiter: &Arbiter, epoch: u64, job: Job) {
    let debug = lib.queue.snapshot().debug_log;
    let log = JobLog::create(&lib.log_path(&job.title));
    lib.stall_cancel.store(false, Ordering::SeqCst);
    lib.set_running(|r| {
        *r = Some(Running {
            job_id: job.id,
            title: job.title.clone(),
            target: job.target.clone(),
            phase: "start".into(),
            started_at: crate::server::util::epoch_secs(),
            ..Default::default()
        })
    });
    let sink = JobSink {
        lib,
        arbiter,
        epoch,
        log,
        debug,
        last_activity: AtomicU64::new(crate::server::util::epoch_secs()),
        logged_decile: AtomicU64::new(0),
    };
    sink.line(format!("Remux {} from {}", job.title, job.iso.display()));
    sink.line(format!("Target {}", job.target.display()));
    if debug {
        sink.line(format!("freemkv {}", crate::server::VERSION_LABEL));
    }
    let started = Instant::now();
    let done = AtomicBool::new(false);
    let ending = std::thread::scope(|scope| {
        let _ = std::thread::Builder::new()
            .name("library-watchdog".into())
            .spawn_scoped(scope, || watchdog(lib, job.id, &sink, &done));
        let ending = remux(&job, cfg, &sink);
        done.store(true, Ordering::SeqCst);
        ending
    });
    finish(lib, &job, ending, started.elapsed(), &sink);
    lib.set_running(|r| *r = None);
}

fn remux(job: &Job, cfg: &Config, sink: &JobSink<'_>) -> Ending {
    if !job.replace
        && let Some(parent) = job.target.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Ending::Failed(e);
    }
    let request = freemkv_engine::RemuxJob {
        iso: freemkv_engine::ImageSource::from_path(&job.iso),
        title: None,
        streams: freemkv_engine::StreamChoice::default(),
        target: job.target.clone(),
        replace: job.replace,
    };
    let result =
        freemkv_engine::remux_iso(&request, &crate::server::keysource::key_params(cfg), sink);
    match result {
        Ok(report) => Ending::Done {
            writing_app: report.writing_app,
        },
        Err(_) if sink.preempted() => Ending::Stopped(JobNote::Preempted),
        Err(_) if shutting_down() => Ending::Stopped(JobNote::Interrupted),
        Err(_) if sink.lib.stall_cancel.load(Ordering::SeqCst) => Ending::Stopped(JobNote::Stalled),
        Err(e) => Ending::Failed(e),
    }
}

/// Write a job's ending to the probe cache, the queue and the console. On success
/// the new stamp is recorded before the queue changes, so the row is current the
/// moment the UI hears the job finished.
pub(crate) fn finish(
    lib: &Library,
    job: &Job,
    ending: Ending,
    took: Duration,
    sink: &dyn LineSink,
) {
    let now = crate::server::util::epoch_secs();
    match ending {
        Ending::Done { writing_app } => {
            lib.probes.record(&job.target, writing_app.clone());
            let size = std::fs::metadata(&job.target).map(|m| m.len()).unwrap_or(0);
            lib.queue.finish(
                job.id,
                JobResult::Done {
                    size_bytes: size,
                    secs: took.as_secs(),
                    finished_at: now,
                    writing_app: writing_app.clone(),
                },
            );
            sink.line(format!(
                "Done: {:.1} GB in {}, muxed with {}",
                size as f64 / 1e9,
                hms(took.as_secs()),
                writing_app.as_deref().unwrap_or("an unknown writer")
            ));
            lib.probes.refresh_audit(&job.target);
        }
        Ending::Stopped(note @ JobNote::Stalled) => {
            lib.queue.note_running(job.id, note);
            lib.queue.finish(
                job.id,
                JobResult::Failed {
                    code: None,
                    message: "stalled".into(),
                    finished_at: now,
                },
            );
            sink.line("Stopped: no progress; the old MKV is unchanged".into());
        }
        Ending::Stopped(note) => {
            lib.queue.requeue(job.id, note);
            sink.line(
                match note {
                    JobNote::Preempted => "Stopped for a rip; back at the head of the queue",
                    _ => "Interrupted; back at the head of the queue",
                }
                .into(),
            );
        }
        Ending::Failed(e) => {
            let message = error_text(&e);
            lib.queue.finish(
                job.id,
                JobResult::Failed {
                    code: freemkv_engine::error_code(&e),
                    message: message.clone(),
                    finished_at: now,
                },
            );
            sink.line(format!("Failed: {message}; the old MKV is unchanged"));
        }
    }
}

// `E<code> <message>` from the locale when it has the code, else the raw text.
fn error_text(e: &std::io::Error) -> String {
    let raw = e.to_string();
    let Some((code, data)) = freemkv_engine::parse_error_code(&raw) else {
        return raw;
    };
    let key = format!("error.E{code}");
    let text = crate::strings::get(&key);
    if text == key {
        return raw;
    }
    format!(
        "E{code} {}",
        crate::strings::fmt(&key, &[("detail", data), ("hash", data)])
    )
}

fn hms(secs: u64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Somewhere a job's human-readable lines go.
pub(crate) trait LineSink {
    fn line(&self, text: String);
}

struct JobLog(Mutex<Option<std::fs::File>>);

impl JobLog {
    fn create(path: &std::path::Path) -> Self {
        let file = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::File::create(path))
            .inspect_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "library job log not writable")
            })
            .ok();
        Self(Mutex::new(file))
    }

    fn write(&self, text: &str) {
        if let Some(f) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = writeln!(f, "{} {text}", crate::server::util::format_iso_datetime());
        }
    }
}

struct JobSink<'a> {
    lib: &'a Library,
    arbiter: &'a Arbiter,
    epoch: u64,
    log: JobLog,
    debug: bool,
    last_activity: AtomicU64,
    logged_decile: AtomicU64,
}

impl JobSink<'_> {
    fn preempted(&self) -> bool {
        self.arbiter.rip_active() || self.arbiter.rip_started_since(self.epoch)
    }

    fn alive(&self) {
        self.last_activity
            .store(crate::server::util::epoch_secs(), Ordering::Relaxed);
    }

    fn phase(&self, name: &str) {
        self.lib.set_running(|r| {
            if let Some(r) = r {
                r.phase = name.to_string();
            }
        });
    }
}

impl LineSink for JobSink<'_> {
    fn line(&self, text: String) {
        self.log.write(&text);
        self.lib.console(text);
    }
}

impl Sink for JobSink<'_> {
    fn log(&self, level: Level, msg: &str) {
        self.alive();
        match level {
            Level::Trace | Level::Debug if !self.debug => {}
            Level::Trace | Level::Debug => self.line(format!("debug: {msg}")),
            Level::Warn => self.line(format!("warning: {msg}")),
            Level::Error => self.line(format!("error: {msg}")),
            Level::Info => self.line(msg.to_string()),
        }
    }

    fn progress(&self, p: &Progress) {
        self.alive();
        let pct = (p.bytes_total > 0)
            .then(|| (p.bytes_done as f64 * 100.0 / p.bytes_total as f64).min(100.0));
        self.lib.set_running(|r| {
            if let Some(r) = r {
                r.pct = pct;
                r.speed_bps = p.speed_bps;
                r.eta_secs = p.eta_secs;
                r.stalled_secs = 0;
            }
        });
        let Some(pct) = pct else { return };
        let decile = (pct / 10.0) as u64;
        if decile > self.logged_decile.swap(decile, Ordering::Relaxed) {
            let eta = p.eta_secs.map_or_else(|| "-".to_string(), hms);
            self.line(format!(
                "{:>3.0}%  {:.1} MB/s  ETA {eta}",
                pct,
                p.speed_bps as f64 / 1e6
            ));
        } else if self.debug {
            self.log
                .write(&format!("progress {pct:.2}% {} B/s", p.speed_bps));
        }
    }

    fn event(&self, e: &Event<'_>) {
        self.alive();
        match e {
            Event::Phase { name } => {
                self.phase(name);
                let text = match *name {
                    "open" => "Opening the image",
                    "mux" => "Muxing",
                    "verify" => "Verifying the new MKV",
                    "replace" => "Moving the new MKV into place",
                    other => other,
                };
                self.line(text.to_string());
            }
            Event::TitleStart { idx, .. } => self.line(format!("Title {}", idx + 1)),
            Event::TitleDone { result, .. } => match result {
                Ok(o) => self.line(format!(
                    "Muxed {:.1} GB, {} streams{}",
                    o.bytes_written as f64 / 1e9,
                    o.streams,
                    if o.completed { "" } else { " (stopped)" }
                )),
                Err(err) => self.line(format!("Mux failed: {}", error_text(err))),
            },
            Event::Verify {
                ok,
                runtime_secs,
                expected_secs,
                ..
            } => self.line(format!(
                "Verify {}: runtime {} of {}",
                if *ok { "passed" } else { "failed" },
                runtime_secs.map_or_else(|| "unknown".into(), |s| hms(s as u64)),
                hms(*expected_secs as u64)
            )),
            Event::Replaced { .. } => self.line("Replaced the old MKV".into()),
            _ => {}
        }
    }

    fn should_cancel(&self) -> bool {
        self.preempted() || shutting_down() || self.lib.stall_cancel.load(Ordering::SeqCst)
    }
}

/// How long a remux may go without any engine callback before it is warned
/// about, and before it is cancelled.
pub(crate) const STALL_WARN_SECS: u64 = 300;
pub(crate) const STALL_CANCEL_SECS: u64 = crate::server::ripper::mux::HARD_WATCHDOG_STALL_SECS;

/// What the watchdog does about a remux that has been quiet for `stall_secs`.
/// There is no process exit here: a stuck remux must never take a live rip down.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StallAction {
    None,
    Warn,
    CancelRemux,
}

pub(crate) fn stall_action(stall_secs: u64, warned: bool, cancelled: bool) -> StallAction {
    if stall_secs >= STALL_CANCEL_SECS && !cancelled {
        StallAction::CancelRemux
    } else if stall_secs >= STALL_WARN_SECS && !warned {
        StallAction::Warn
    } else {
        StallAction::None
    }
}

// Runs beside the remux until `done`; its only lever is the job's own cancel flag.
fn watchdog(lib: &Library, job_id: u64, sink: &JobSink<'_>, done: &AtomicBool) {
    let (mut warned, mut cancelled) = (false, false);
    while !done.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(500));
        let now = crate::server::util::epoch_secs();
        let stall = now.saturating_sub(sink.last_activity.load(Ordering::Relaxed));
        if stall >= 60 {
            lib.set_running(|r| {
                if let Some(r) = r {
                    r.stalled_secs = stall;
                }
            });
        }
        match stall_action(stall, warned, cancelled) {
            StallAction::None => warned &= stall >= STALL_WARN_SECS,
            StallAction::Warn => {
                warned = true;
                sink.line(format!("No progress for {}", hms(stall)));
            }
            StallAction::CancelRemux => {
                cancelled = true;
                lib.stall_cancel.store(true, Ordering::SeqCst);
                lib.queue.note_running(job_id, JobNote::Stalled);
                sink.line(format!(
                    "No progress for {}; cancelling this remux",
                    hms(stall)
                ));
            }
        }
    }
}

/// Audit every library MKV whose cached audit no longer matches it, then rest.
pub fn audit_loop(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>) {
    while !shutting_down() {
        let d = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner()));
        for m in super::index::list_mkvs(&d.library).files {
            if shutting_down() {
                return;
            }
            if lib.probes.refresh_audit(&m.path) {
                lib.touch_live();
            }
        }
        nap(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::super::probe::{MuxedWith, testmkv::mkv};
    use super::super::queue::{JobState, NewJob};
    use super::super::{Dirs, Library};
    use super::*;
    use std::path::Path;

    struct Lines(Mutex<Vec<String>>);
    impl LineSink for Lines {
        fn line(&self, text: String) {
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
        let lib = Library::open(&t.path().join("config"), &t.path().join("logs"));
        (t, lib, dirs)
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
    fn muxed_with_is_current_the_moment_a_remux_finishes_mid_batch() {
        let (_t, lib, dirs) = library_with(&["A", "B", "C"]);
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
        assert!(lines.0.lock().unwrap()[0].starts_with("Done:"));
    }

    #[test]
    fn a_file_changed_behind_the_queue_is_re_read_on_the_next_listing() {
        let (_t, lib, dirs) = library_with(&["A"]);
        assert!(matches!(
            row(&lib.listing(&dirs), "A").muxed_with,
            MuxedWith::Older { .. }
        ));
        let target = dirs.library.join("A/A.mkv");
        let longer = format!("{} with a longer stamp", current_stamp());
        std::fs::write(&target, mkv(&longer, Some(60.0), Some(58), true)).unwrap();
        assert_eq!(
            row(&lib.listing(&dirs), "A").writing_app.as_deref(),
            Some(longer.as_str())
        );
    }

    #[test]
    fn a_failure_keeps_its_code_and_a_preempted_job_goes_back_first() {
        let (_t, lib, dirs) = library_with(&["A", "B"]);
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
        assert!(next_job(&lib, &arbiter).is_none());
        assert_eq!(lib.queue.snapshot().count(JobState::Queued), 1);
        drop(slot);
        assert!(next_job(&lib, &arbiter).is_some());
    }

    #[test]
    fn a_rip_that_starts_mid_remux_is_not_blocked_and_stops_the_remux() {
        let (_t, lib, _dirs) = library_with(&[]);
        let arbiter = Arbiter::new();
        let sink = JobSink {
            lib: &lib,
            arbiter: &arbiter,
            epoch: arbiter.epoch(),
            log: JobLog(Mutex::new(None)),
            debug: false,
            last_activity: AtomicU64::new(0),
            logged_decile: AtomicU64::new(0),
        };
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
        let body = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
        assert!(
            !body.contains(concat!("process", "::exit")),
            "a remux never exits the daemon"
        );
    }
}
