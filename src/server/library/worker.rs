//! The remux worker: one job at a time through `freemkv_engine::remux_iso`,
//! yielding the mux slot to any rip, with a watchdog that only ever cancels
//! its own job.

use super::arbiter::Arbiter;
use super::queue::{Job, JobNote, JobResult, JobState};
use super::{Library, LineKind, Running, transcript};
use crate::server::config::Config;
use freemkv_engine::{Event, Level, Progress, Sink};
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub(crate) fn shutting_down() -> bool {
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
            iso: job.iso.clone(),
            phase: "start".into(),
            started_at: crate::server::util::epoch_secs(),
            ..Default::default()
        })
    });
    let sink = JobSink {
        lib,
        job_id: job.id,
        arbiter,
        epoch,
        log,
        debug,
        iso_url: format!("iso://{}", job.iso.display()),
        last_activity: AtomicU64::new(crate::server::util::epoch_secs()),
        term: Mutex::new(Term::default()),
    };
    sink.emit(
        LineKind::Cmd,
        format!(
            "$ freemkv iso://{} mkv://{}",
            job.iso.display(),
            job.target.display()
        ),
    );
    sink.emit(
        LineKind::Out,
        format!("freemkv {}", crate::server::VERSION_LABEL),
    );
    sink.emit(LineKind::Out, String::new());
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
    sink.close_open_line(matches!(ending, Ending::Done { .. }));
    finish(lib, &job, ending, started.elapsed(), &sink);
    lib.set_running(|r| *r = None);
}

fn remux(job: &Job, cfg: &Config, sink: &JobSink<'_>) -> Ending {
    if !job.replace
        && let Err(why) = safe_to_create(&super::dirs(cfg), &job.target)
    {
        return Ending::Failed(std::io::Error::other(why));
    }
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
        Err(_) if sink.lib.cancelled(job.id) => Ending::Stopped(JobNote::Cancelled),
        Err(_) if sink.preempted() => Ending::Stopped(JobNote::Preempted),
        Err(_) if shutting_down() => Ending::Stopped(JobNote::Interrupted),
        Err(_) if sink.lib.stall_cancel.load(Ordering::SeqCst) => Ending::Stopped(JobNote::Stalled),
        Err(e) => Ending::Failed(e),
    }
}

/// The last look before a remux creates a new MKV: the target must sit under
/// the library folder, and that folder must be listable and not empty (an
/// unmounted share is an empty mountpoint on the local disk).
pub(crate) fn safe_to_create(d: &super::Dirs, target: &std::path::Path) -> Result<(), String> {
    if !target.starts_with(&d.library) {
        return Err(format!(
            "{} is not under the library folder",
            target.display()
        ));
    }
    match std::fs::read_dir(&d.library).map(|mut r| r.next().is_some()) {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "the library folder {} is empty (is the share mounted?); nothing was written",
            d.library.display()
        )),
        Err(e) => Err(format!(
            "the library folder {} cannot be read ({e}); nothing was written",
            d.library.display()
        )),
    }
}

/// Write a job's ending to the probe cache, the index, the queue and the
/// console. On success the new stamp is recorded before the queue changes, so
/// the row is current the moment the UI hears the job finished.
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
            lib.note_landed(&job.target, writing_app.clone());
            let size = lib.snapshot().sigs.get(&job.target).map_or(0, |s| s.size);
            lib.queue.finish(
                job.id,
                JobResult::Done {
                    size_bytes: size,
                    secs: took.as_secs(),
                    finished_at: now,
                    writing_app: writing_app.clone(),
                },
            );
            sink.line(
                LineKind::Ok,
                format!(
                    "Remuxed in {}: {}",
                    hms(took.as_secs()),
                    writing_app.as_deref().unwrap_or("no writing-app stamp")
                ),
            );
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
            sink.line(
                LineKind::Err,
                "Stopped: no progress. The old MKV is unchanged.".into(),
            );
        }
        Ending::Stopped(JobNote::Cancelled) => {
            lib.queue.drop_job(job.id);
            let _ = std::fs::remove_file(super::queue::partial_path(&job.target));
            sink.line(
                LineKind::Warn,
                "Stopped. The existing MKV is unchanged.".into(),
            );
        }
        Ending::Stopped(note) => {
            lib.queue.requeue(job.id, note);
            sink.line(
                LineKind::Warn,
                match note {
                    JobNote::Preempted => {
                        "Stopped: a rip needs the mux slot. Back at the head of the queue."
                    }
                    _ => "Interrupted. Back at the head of the queue.",
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
            if !sink.error_shown() {
                sink.line(LineKind::Err, format!("{}: {message}", error_word()));
            }
            if job.replace {
                sink.line(LineKind::Out, "The old MKV is unchanged.".into());
            }
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
    let text = crate::strings::fmt(&key, &[("detail", data), ("hash", data)]);
    let level = format!("{}: ", error_word());
    format!("E{code} {}", text.strip_prefix(&level).unwrap_or(&text))
}

fn error_word() -> String {
    crate::strings::get(crate::messaging::Level::Error.locale_key())
}

// The CLI's `render_error`: the level word once, then the coded message.
fn error_line(e: &std::io::Error) -> String {
    format!("{}: {}", error_word(), error_text(e))
}

fn hms(secs: u64) -> String {
    format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Somewhere a job's human-readable lines go.
pub(crate) trait LineSink {
    fn line(&self, kind: LineKind, text: String);
    /// The job's error has already been printed (the engine reported it as
    /// the title failed), so the ending must not print it again.
    fn error_shown(&self) -> bool {
        false
    }
}

struct JobLog(Mutex<Option<std::fs::File>>);

impl JobLog {
    fn create(path: &std::path::Path) -> Self {
        // Append: a retry keeps the earlier attempts' transcripts above it.
        let earlier = std::fs::metadata(path).is_ok_and(|m| m.len() > 0);
        let file = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
            })
            .map(|mut f| {
                if earlier {
                    let _ = writeln!(f, "{}", super::log_line(LineKind::Out, ""));
                    let _ = writeln!(
                        f,
                        "{}",
                        super::log_line(
                            LineKind::Debug,
                            &format!("──── another attempt, {} ────", crate::server::util::format_iso_datetime())
                        )
                    );
                }
                f
            })
            .inspect_err(|e| {
                tracing::warn!(path = %path.display(), error = %e, "library job log not writable")
            })
            .ok();
        Self(Mutex::new(file))
    }

    fn write(&self, kind: LineKind, text: &str) {
        if let Some(f) = self.0.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = writeln!(f, "{}", super::log_line(kind, text));
        }
    }
}

// What the transcript has printed so far, to lay lines out the way the CLI does.
#[derive(Default)]
struct Term {
    // The title's header lines, known once the engine hands over the title.
    title: Option<TitleText>,
    opened: bool,
    mux_started: Option<Instant>,
    // The in-place progress line; printed for good when the title finishes.
    progress: String,
    error_shown: bool,
}

struct TitleText {
    duration: String,
    size_gb: String,
    streams: Vec<String>,
}

struct JobSink<'a> {
    lib: &'a Library,
    job_id: u64,
    arbiter: &'a Arbiter,
    epoch: u64,
    log: JobLog,
    debug: bool,
    iso_url: String,
    last_activity: AtomicU64,
    term: Mutex<Term>,
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

    fn term(&self) -> std::sync::MutexGuard<'_, Term> {
        self.term.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, kind: LineKind, text: String) {
        self.log.write(kind, &text);
        self.lib.console(self.job_id, kind, text);
    }

    // The image is open: the CLI's "Title N (...)", "Opening iso://...OK" and stream block.
    fn print_opened(&self, idx: usize) {
        let title = {
            let mut t = self.term();
            if t.opened {
                return;
            }
            t.opened = true;
            t.title.take()
        };
        if let Some(t) = &title {
            self.emit(
                LineKind::Out,
                crate::strings::fmt(
                    "rip.title_info",
                    &[
                        ("num", &(idx + 1).to_string()),
                        ("duration", &t.duration),
                        ("size", &t.size_gb),
                    ],
                ),
            );
        }
        self.emit(LineKind::Out, format!("{}{}", opening(&self.iso_url), ok()));
        for line in title.iter().flat_map(|t| t.streams.iter()) {
            self.emit(LineKind::Out, line.clone());
        }
    }

    // End the job's transcript the way a terminal leaves it: an unfinished
    // "Opening ..." line, and the last progress line, stay on screen.
    fn close_open_line(&self, _ok: bool) {
        let (opened, progress) = {
            let mut t = self.term();
            (t.opened, std::mem::take(&mut t.progress))
        };
        if !opened {
            self.emit(LineKind::Out, opening(&self.iso_url));
        }
        if !progress.is_empty() {
            self.emit(LineKind::Out, progress);
        }
    }
}

fn opening(url: &str) -> String {
    crate::strings::fmt("rip.opening", &[("device", url)])
}

fn ok() -> String {
    crate::strings::get("rip.ok")
}

impl LineSink for JobSink<'_> {
    fn line(&self, kind: LineKind, text: String) {
        self.emit(kind, text);
    }

    fn error_shown(&self) -> bool {
        self.term().error_shown
    }
}

impl Sink for JobSink<'_> {
    fn log(&self, level: Level, msg: &str) {
        self.alive();
        match level {
            Level::Trace | Level::Debug if !self.debug => {}
            Level::Trace | Level::Debug => self.emit(LineKind::Debug, msg.to_string()),
            Level::Warn => self.emit(LineKind::Warn, msg.to_string()),
            Level::Error => self.emit(LineKind::Err, msg.to_string()),
            // The engine's own progress notes are not what the CLI prints.
            Level::Info if self.debug => self.emit(LineKind::Debug, msg.to_string()),
            Level::Info => {}
        }
    }

    fn title_opened(&self, title: &libfreemkv::DiscTitle) {
        self.alive();
        self.term().title = Some(TitleText {
            duration: title.duration_display(),
            size_gb: format!("{:.1}", title.size_gb()),
            streams: transcript::stream_lines(title),
        });
    }

    fn progress(&self, p: &Progress) {
        self.alive();
        let line = transcript::progress_line(p.bytes_done, p.bytes_total, p.speed_bps, p.eta_secs);
        let pct = (p.bytes_total > 0)
            .then(|| (p.bytes_done as f64 * 100.0 / p.bytes_total as f64).min(100.0));
        self.term().progress = line.clone();
        if self.debug {
            self.log.write(LineKind::Debug, &line);
        }
        self.lib.set_running(|r| {
            if let Some(r) = r {
                r.pct = pct;
                r.bytes_done = p.bytes_done;
                r.bytes_total = p.bytes_total;
                r.speed_bps = p.speed_bps;
                r.eta_secs = p.eta_secs;
                r.stalled_secs = 0;
                r.line = line;
            }
        });
    }

    fn event(&self, e: &Event<'_>) {
        self.alive();
        match e {
            Event::Phase { name } => self.phase(name),
            Event::TitleStart { idx, dest } => {
                self.print_opened(*idx);
                self.term().mux_started = Some(Instant::now());
                self.emit(LineKind::Out, format!("{}{}", opening(dest), ok()));
            }
            Event::TitleDone { result, .. } => {
                let (progress, started) = {
                    let mut t = self.term();
                    (std::mem::take(&mut t.progress), t.mux_started)
                };
                if !progress.is_empty() {
                    self.emit(LineKind::Out, progress);
                }
                self.lib.set_running(|r| {
                    if let Some(r) = r {
                        r.line.clear();
                    }
                });
                match result {
                    Ok(o) => {
                        self.emit(LineKind::Out, String::new());
                        let secs = started.map_or(0.0, |s| s.elapsed().as_secs_f64());
                        let kind = if o.completed {
                            LineKind::Ok
                        } else {
                            LineKind::Warn
                        };
                        self.emit(kind, transcript::complete_line(o.bytes_written, secs));
                    }
                    Err(err) => {
                        self.term().error_shown = true;
                        self.emit(LineKind::Err, error_line(err));
                    }
                }
            }
            Event::Verify {
                ok,
                runtime_secs,
                expected_secs,
                path,
            } => self.emit(
                if *ok { LineKind::Out } else { LineKind::Err },
                transcript::verify_line(path, *ok, *runtime_secs, *expected_secs),
            ),
            Event::Replaced { path } => {
                self.emit(LineKind::Out, format!("Replaced {}", path.display()))
            }
            _ => {}
        }
    }

    fn should_cancel(&self) -> bool {
        self.preempted()
            || shutting_down()
            || self.lib.stall_cancel.load(Ordering::SeqCst)
            || self.lib.cancelled(self.job_id)
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
                sink.line(LineKind::Warn, format!("No progress for {}", hms(stall)));
            }
            StallAction::CancelRemux => {
                cancelled = true;
                lib.stall_cancel.store(true, Ordering::SeqCst);
                lib.queue.note_running(job_id, JobNote::Stalled);
                sink.line(
                    LineKind::Err,
                    format!("No progress for {}; cancelling this remux", hms(stall)),
                );
            }
        }
    }
}

/// How often the indexer rescans when nothing wakes it.
pub(crate) const RESCAN_SECS: u64 = 60;

/// Keep the in-memory index current: scan, read headers, audit, then sleep
/// until woken or the rescan interval passes.
pub fn index_loop(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>) {
    let d = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner()));
    let swept = lib.sweep_partials(&d);
    if swept > 0 {
        tracing::info!(count = swept, "removed orphaned remux partials at startup");
    }
    while !shutting_down() {
        let d = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner()));
        if !lib.index_now(&d) {
            continue;
        }
        let until = Instant::now() + Duration::from_secs(RESCAN_SECS);
        while !shutting_down() && Instant::now() < until {
            if lib.wait_for_wake(Duration::from_secs(1)) {
                break;
            }
        }
    }
}

/// The audit worker: one file at a time from the audit queue, after rips and remuxes
/// (Rip > Remux > Audit). Each audit is the quick read, then the full decode while deep
/// audit is on. A decode a rip or remux interrupts goes back to the front of the queue.
pub fn audit_loop(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>, arbiter: &Arbiter) {
    let enabled = || cfg.read().unwrap_or_else(|e| e.into_inner()).deep_audit;
    let busy = || remux_or_rip_busy(lib, arbiter);
    let mut filled = Instant::now();
    while !shutting_down() {
        let was = lib.deep_enabled();
        lib.set_deep_enabled(enabled());
        // Turning deep audit on owes every file its decode; refill now, else each minute.
        if (lib.deep_enabled() && !was) || filled.elapsed() >= Duration::from_secs(60) {
            filled = Instant::now();
            if !lib.indexing() {
                let complete = !lib.snapshot().incomplete;
                lib.fill_audits(&lib.mkv_files(), complete);
            }
        }
        if busy() {
            nap(Duration::from_secs(2));
            continue;
        }
        let Some(path) = lib.audits.next() else {
            nap(Duration::from_secs(2));
            continue;
        };
        let library = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner())).library;
        let stop = || {
            shutting_down() || !enabled() || busy() || lib.audits.paused() || lib.audits.cancelled()
        };
        audit_one(lib, &path, &library, enabled(), &stop);
        lib.touch_index();
    }
}

// A remux running, or queued and not paused, goes before an audit; so does any rip.
fn remux_or_rip_busy(lib: &Library, arbiter: &Arbiter) -> bool {
    let q = lib.queue.snapshot();
    arbiter.rip_active() || q.running().is_some() || (!q.paused && q.count(JobState::Queued) > 0)
}

// Audit one file: the quick read, then (deep on) the full decode.
pub(crate) fn audit_one(
    lib: &Library,
    path: &Path,
    library: &Path,
    deep_on: bool,
    stop: &dyn Fn() -> bool,
) {
    let Some(sig) = super::probe::FileSig::stat(path) else {
        return;
    };
    let title = lib
        .snapshot()
        .rows
        .iter()
        .find(|r| r.mkv.as_deref() == Some(path))
        .map(|r| r.title.clone())
        .unwrap_or_else(|| super::index::mkv_title(library, path));
    lib.audits.start(path, title);
    lib.touch_index();
    let now = crate::server::util::epoch_secs;
    match super::probe::audit_fast(path) {
        Some(report) => {
            let duration = report.duration_secs;
            lib.audits.record_fast(path, sig, report, now());
            let ffmpeg = super::deep::ffmpeg();
            if let Some(ffmpeg) =
                ffmpeg.filter(|_| deep_on && lib.audits.deep_due_for(path, sig, now()))
            {
                tracing::info!(file = %path.display(), "deep audit: decoding");
                let _ = std::fs::create_dir_all(&lib.log_dir);
                let err_file = lib.log_dir.join("deep-audit.stderr");
                let progress =
                    |stage: &'static str, secs: f64| lib.audits.progress(stage, secs, duration);
                match super::deep::full_decode(&ffmpeg, path, library, &err_file, stop, &progress) {
                    Some(v) => lib.audits.record_deep(path, sig, v, now()),
                    None if !lib.audits.cancelled() => lib.audits.requeue_front(path.to_path_buf()),
                    None => {}
                }
            }
        }
        // The storage failed, not the file: try again after the queue.
        None => {
            lib.audits.finish();
            lib.audits.enqueue([path.to_path_buf()]);
            return;
        }
    }
    lib.audits.finish();
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
        assert!(next_job(&lib, &arbiter).is_none());
        assert_eq!(lib.queue.snapshot().count(JobState::Queued), 1);
        drop(slot);
        assert!(next_job(&lib, &arbiter).is_some());
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
        assert!(said.contains("The old MKV is unchanged"), "{said}");
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
        assert!(!sink.should_cancel());
        assert_eq!(lib.stop_all(), (true, 2));
        assert!(sink.should_cancel(), "the running remux sees the stop");
        // The engine returns; the worker records the ending.
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
