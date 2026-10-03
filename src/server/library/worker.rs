//! The remux worker: one job at a time through `freemkv_engine::remux_iso`,
//! yielding the mux slot to any rip, with a watchdog that only ever cancels
//! its own job.

use super::arbiter::Arbiter;
use super::queue::{Failure, Job, JobNote, JobResult};
use super::{Dirs, Library, LineKind, Running, transcript};
use crate::server::config::Config;
use crate::server::health::{self, Fault, Problem};
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
    if let Some(dir) = remux_stage_dir() {
        clean_stale_staging(&dir);
    }
    while !shutting_down() {
        let snapshot = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
        let d = super::dirs(&snapshot);
        let output = |dir: &Path| health::preflight("output", dir);
        let now = crate::server::util::epoch_secs();
        let Some((job, epoch)) = next_job(lib, arbiter, &d, &output, now) else {
            nap(Duration::from_secs(1));
            continue;
        };
        run_job(lib, &snapshot, arbiter, epoch, job);
    }
    tracing::info!("library worker stopping");
}

/// The bounded output-folder check a job must pass before it starts.
pub(crate) type Preflight<'a> = &'a dyn Fn(&Path) -> Result<(), Problem>;

// The next job and the arbiter epoch it started under. Nothing while a rip runs, and
// nothing while a folder the remux needs is unhealthy: the job waits, held, in the queue.
fn next_job(
    lib: &Library,
    arbiter: &Arbiter,
    d: &Dirs,
    preflight: Preflight<'_>,
    now: u64,
) -> Option<(Job, u64)> {
    if arbiter.rip_active() {
        return None;
    }
    if lib.queue.peek_next().is_none() {
        lib.set_hold(None);
        return None;
    }
    if let Some(hold) = lib.blocked(d, now) {
        lib.set_hold(Some(hold));
        return None;
    }
    if let Err(p) = preflight(&d.library) {
        tracing::warn!(folder = %p.path.display(), error = %p.detail, "remux held: {}", p.message);
        lib.hold_for(&p, now);
        return None;
    }
    lib.set_hold(None);
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
    sink.close_open_line();
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
    let keys = crate::server::keysource::key_params(cfg);
    let result = if let Some(dir) = remux_stage_dir() {
        std::fs::create_dir_all(&dir).and_then(|()| {
            freemkv_engine::remux_iso_staged(
                &request,
                &keys,
                sink,
                &dir.join(format!("{}.mkv.partial", job.id)),
            )
        })
    } else {
        freemkv_engine::remux_iso(&request, &keys, sink)
    };
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

fn remux_stage_dir() -> Option<std::path::PathBuf> {
    health::remux_stage_dir()
}

// Only this directory and these numeric job files belong to the library worker.
// A killed container cannot run the engine's normal partial-file guard.
fn clean_stale_staging(dir: &Path) {
    let Ok(files) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in files.flatten() {
        let path = entry.path();
        let owned = path
            .file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_suffix(".mkv.partial"))
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
        if owned
            && path.is_file()
            && let Err(e) = std::fs::remove_file(&path)
        {
            tracing::warn!(path = %path.display(), error = %e, "stale remux stage could not be removed");
        }
    }
}

#[cfg(test)]
mod staged_cleanup_tests {
    use super::clean_stale_staging;

    #[test]
    fn startup_removes_only_library_job_partials() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("42.mkv.partial");
        let unrelated = dir.path().join("disc.mkv.partial");
        std::fs::write(&stale, b"interrupted").unwrap();
        std::fs::write(&unrelated, b"keep").unwrap();
        clean_stale_staging(dir.path());
        assert!(!stale.exists());
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep");
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
            // A stall is retried only when a folder it uses is unhealthy now.
            let phase = phase_now(lib);
            let problem = folder_problem(lib, job);
            if problem.is_some() {
                let why = (None, "stalled");
                if retry_storage_fault(
                    lib,
                    job,
                    why,
                    Fault::Unresponsive,
                    problem.as_ref(),
                    phase.as_deref(),
                    now,
                    sink,
                ) {
                    return;
                }
            }
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
            let phase = phase_of(&e).or_else(|| phase_now(lib));
            let code = freemkv_engine::error_code(&e);
            let text = error_text(&e);
            let fault = storage_fault(&e);
            let unhealthy = fault.and_then(|_| folder_problem(lib, job));
            if let Some(f) = fault
                && retry_storage_fault(
                    lib,
                    job,
                    (code, &text),
                    f,
                    unhealthy.as_ref(),
                    phase.as_deref(),
                    now,
                    sink,
                )
            {
                return;
            }
            // Not retried; still name a folder that is unhealthy right now, and what is safe.
            let message = failure_message(&text, phase.as_deref(), unhealthy.as_ref(), job, None);
            lib.queue.finish(
                job.id,
                JobResult::Failed {
                    code,
                    message: message.clone(),
                    finished_at: now,
                },
            );
            if !sink.error_shown() {
                sink.line(LineKind::Err, format!("{}: {text}", error_word()));
            }
            if let Some(p) = &unhealthy {
                sink.line(LineKind::Err, format!("{} {}", p.message, p.hint));
            }
            sink.line(LineKind::Out, safe_text(job).into());
        }
    }
}

/// The engine phase the job was in, as the running state last showed it.
fn phase_now(lib: &Library) -> Option<String> {
    lib.running()
        .map(|r| r.phase)
        .filter(|p| !p.is_empty() && p != "start")
}

// A timeout names the operation it stalled in (`E9073: copy`).
fn phase_of(e: &std::io::Error) -> Option<String> {
    let text = e.to_string();
    let (code, data) = freemkv_engine::parse_error_code(&text)?;
    (code == libfreemkv::error::E_TIMED_OUT && phase_words(data.trim()).is_some())
        .then(|| data.trim().to_string())
}

fn phase_words(phase: &str) -> Option<&'static str> {
    Some(match phase {
        "open" => "opening the ISO",
        "mux" => "muxing",
        "sync" => "flushing the new file to disk",
        "verify" => "verifying the new file",
        "copy" => "copying the new MKV into the output folder",
        "replace" => "moving the new MKV into place",
        _ => return None,
    })
}

/// Whether a failure is the storage's rather than the content's. A coded engine error is
/// about the disc image or the mux, except the timeouts of a stalled read, write or flush.
pub(crate) fn storage_fault(e: &std::io::Error) -> Option<Fault> {
    use libfreemkv::error::{E_SYNC_TIMEOUT, E_TIMED_OUT};
    match freemkv_engine::error_code(e) {
        Some(E_TIMED_OUT | E_SYNC_TIMEOUT) => Some(Fault::Unresponsive),
        Some(_) => None,
        None => Some(Fault::of(e)).filter(|f| *f != Fault::Other),
    }
}

/// A fresh, bounded look at every folder `job` uses: the first that fails, if any.
pub(crate) fn folder_problem(lib: &Library, job: &Job) -> Option<Problem> {
    let output = lib
        .snapshot()
        .dirs
        .as_ref()
        .map(|d| d.library.clone())
        .or_else(|| job.target.parent().map(Path::to_path_buf))?;
    if let Err(p) = health::preflight("output", &output) {
        return Some(p);
    }
    if let Some(dir) = job.iso.parent() {
        let iso_dir = dir.to_path_buf();
        let probe = move |d: &Path| std::fs::read_dir(d).map(drop);
        if let Err(p) =
            health::preflight_with("source ISO", &iso_dir, health::PREFLIGHT_TIMEOUT, probe)
        {
            return Some(p);
        }
    }
    let stage = remux_stage_dir()?;
    health::preflight("remux staging", &stage).err()
}

// Requeue a job a storage fault stopped, given `problem` from re-checking its folders once:
// at once if they are fine now (first time only), else after the backoff, once they check
// out. False when the folders are fine and it failed before, or the retries are used up.
#[allow(clippy::too_many_arguments)]
fn retry_storage_fault(
    lib: &Library,
    job: &Job,
    (code, text): (Option<u16>, &str),
    fault: Fault,
    problem: Option<&Problem>,
    phase: Option<&str>,
    now: u64,
    sink: &dyn LineSink,
) -> bool {
    // With every folder fine, a missing file or a refused write is the file's problem.
    let file_only = matches!(fault, Fault::Missing | Fault::Denied | Fault::ReadOnly);
    if problem.is_none() && (job.attempts > 0 || file_only) {
        return false;
    }
    let Some(at) = super::queue::retry_at(job, now, problem.is_none()) else {
        sink.line(
            LineKind::Err,
            format!(
                "Giving up after {} automatic retries over {}.",
                job.attempts,
                hms(now.saturating_sub(job.first_failed_at.unwrap_or(now)))
            ),
        );
        return false;
    };
    // Hook for the engine keeping a finished, verified staged file: its path goes here and the
    // job waits as `StagedWaiting` to copy it in. No engine API hands one back yet.
    let staged: Option<std::path::PathBuf> = None;
    let message = failure_message(text, phase, problem, job, Some(at));
    lib.queue
        .retry_later(job.id, Failure { code, message }, at, staged);
    if let Some(p) = problem {
        lib.hold_for(p, now);
        sink.line(LineKind::Err, format!("{} {}", p.message, p.hint));
    }
    sink.line(LineKind::Out, safe_text(job).into());
    sink.line(
        LineKind::Warn,
        if at <= now {
            "The folders answer again: retrying now.".to_string()
        } else {
            format!(
                "Waiting for the folder; retry {} in {} once it checks out.",
                job.attempts + 1,
                hms(at - now)
            )
        },
    );
    true
}

fn safe_text(job: &Job) -> &'static str {
    if job.replace {
        "Your existing MKV is unchanged."
    } else {
        "No MKV was written to the library."
    }
}

/// A failed job's message: the error, the phase it stopped in, the folder at fault, what
/// is safe, and when it is retried.
pub(crate) fn failure_message(
    text: &str,
    phase: Option<&str>,
    folder: Option<&Problem>,
    job: &Job,
    retry_at: Option<u64>,
) -> String {
    let mut out = text.trim_end_matches('.').to_string();
    out.push('.');
    if let Some(words) = phase.and_then(phase_words) {
        out.push_str(&format!(" It stopped while {words}."));
    }
    if let Some(p) = folder {
        out.push_str(&format!(" {} Folder: {}.", p.message, p.path.display()));
    }
    out.push(' ');
    out.push_str(safe_text(job));
    if retry_at.is_some() {
        out.push_str(" It is retried automatically once the folder checks out.");
    }
    out
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
    // E6000's status/sense hex tail is diagnostic noise; the CLI shows only the sector.
    let detail = if code == 6000 {
        data.split_whitespace().next().unwrap_or(data)
    } else {
        data
    };
    let text = crate::strings::fmt(&key, &[("detail", detail), ("hash", data)]);
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
    fn close_open_line(&self) {
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

/// The quick lane: one quick audit at a time, after rips and remuxes (Rip > Remux > Audit),
/// whatever the deep audit setting. It also tracks that setting and refills both lanes.
pub fn audit_loop(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>, arbiter: &Arbiter) {
    let enabled = || cfg.read().unwrap_or_else(|e| e.into_inner()).deep_audit;
    let mut filled = Instant::now();
    while !shutting_down() {
        let was = lib.deep_enabled();
        lib.set_deep_enabled(enabled());
        // Turning deep audit on owes every file its decode; refill now, else each minute.
        if (lib.deep_enabled() && !was) || filled.elapsed() >= Duration::from_secs(60) {
            filled = Instant::now();
            lib.refill_audits();
        }
        let library = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner())).library;
        if !quick_turn(lib, &library, remux_or_rip_busy(lib, arbiter)) {
            nap(Duration::from_secs(2));
        }
    }
}

/// The deep lane: one full decode at a time while deep audit is on, after rips and remuxes.
/// A rip, a remux, a pause or a stop interrupts it; the quick lane never does.
pub(crate) fn deep_audit_loop(lib: &Arc<Library>, cfg: &Arc<RwLock<Config>>, arbiter: &Arbiter) {
    let enabled = || cfg.read().unwrap_or_else(|e| e.into_inner()).deep_audit;
    let busy = || remux_or_rip_busy(lib, arbiter);
    while !shutting_down() {
        let ffmpeg = super::deep::ffmpeg();
        if !enabled() || busy() || ffmpeg.is_none() {
            nap(Duration::from_secs(2));
            continue;
        }
        let Some(path) = lib.audits.next_deep() else {
            nap(Duration::from_secs(2));
            continue;
        };
        let library = super::dirs(&cfg.read().unwrap_or_else(|e| e.into_inner())).library;
        let stop = || {
            shutting_down()
                || !enabled()
                || busy()
                || lib.audits.paused()
                || lib.audits.cancelled_deep()
        };
        deep_one(lib, &path, &library, &stop, ffmpeg);
        lib.touch_index();
    }
}

// Run the next quick audit unless a rip or remux has the disks; true if one ran.
fn quick_turn(lib: &Library, library: &Path, busy: bool) -> bool {
    if busy {
        return false;
    }
    let Some(path) = lib.audits.next() else {
        return false;
    };
    quick_one(lib, &path, library, lib.deep_enabled());
    lib.touch_index();
    true
}

// A remux running, or queued and not paused, goes before an audit; so does any rip.
fn remux_or_rip_busy(lib: &Library, arbiter: &Arbiter) -> bool {
    arbiter.rip_active() || lib.queue.has_work()
}

fn title_of(lib: &Library, path: &Path, library: &Path) -> String {
    lib.snapshot()
        .rows
        .iter()
        .find(|r| r.mkv.as_deref() == Some(path))
        .map(|r| r.title.clone())
        .unwrap_or_else(|| super::index::mkv_title(library, path))
}

// The quick read of one file; with deep audit on, a file it leaves owing a decode joins
// the deep lane.
pub(crate) fn quick_one(lib: &Library, path: &Path, library: &Path, deep_on: bool) {
    let Some(sig) = super::probe::FileSig::stat(path) else {
        lib.audits.finish();
        return;
    };
    lib.audits.start(path, title_of(lib, path, library));
    lib.touch_index();
    // A "Stop all" that came after the file was taken but before it started.
    if lib.audits.cancelled() {
        lib.audits.finish();
        return;
    }
    let now = crate::server::util::epoch_secs;
    // On a storage failure (no report) the next refill queues the file again, a minute on.
    if let Some(report) = super::probe::audit_fast(path) {
        lib.audits.record_fast(path, sig, report, now());
        if deep_on && lib.audits.deep_due_for(path, sig, now()) {
            lib.audits.enqueue_deep(path);
        }
    }
    lib.audits.finish();
}

// The full decode of one file whose quick audit stands. An interrupted decode goes back
// to the front of the deep lane; a stopped one does not.
fn deep_one(
    lib: &Library,
    path: &Path,
    library: &Path,
    stop: &dyn Fn() -> bool,
    ffmpeg: Option<std::path::PathBuf>,
) {
    let now = crate::server::util::epoch_secs;
    let sig = super::probe::FileSig::stat(path);
    let (Some(ffmpeg), Some(sig)) = (ffmpeg, sig) else {
        lib.audits.finish_deep();
        return;
    };
    // A file changed or re-audited since it was queued waits for its quick read.
    if !lib.audits.deep_due_for(path, sig, now()) {
        lib.audits.finish_deep();
        return;
    }
    lib.audits.start_deep(path, title_of(lib, path, library));
    lib.touch_index();
    if lib.audits.cancelled_deep() {
        lib.audits.finish_deep();
        return;
    }
    let duration = lib.audits.report(path, sig).and_then(|r| r.duration_secs);
    tracing::info!(file = %path.display(), "deep audit: decoding");
    let _ = std::fs::create_dir_all(&lib.log_dir);
    let err_file = lib.log_dir.join("deep-audit.stderr");
    let progress = |stage: &'static str, secs: f64| lib.audits.progress(stage, secs, duration);
    match super::deep::full_decode(&ffmpeg, path, library, duration, &err_file, stop, &progress) {
        Some(v) => lib.audits.record_deep(path, sig, v, now()),
        None if !lib.audits.cancelled_deep() => lib.audits.requeue_front(path.to_path_buf()),
        None => {}
    }
    lib.audits.finish_deep();
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
        let body = &src[..src.find("#[cfg(test)]\nmod tests").unwrap()];
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
}
