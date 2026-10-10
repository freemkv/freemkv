//! Delivering a finished remux into the library folder, and keeping it when only that fails.
//!
//! With a remux staging folder, the engine muxes, syncs, verifies and lands the MKV on local
//! staging; [`remux_via_stage`] then takes `<target>.lock`, copies the file to
//! `<target>.partial`, syncs and verifies the copy, lands it on the target and syncs its
//! folder. When the local file verified and a storage step of that delivery fails, the file
//! is kept as `<stage>/<name>.staged.mkv` beside an atomically written `<name>.staged.json`
//! sidecar, and the error carries a [`Kept`]. [`resume`] later runs only the delivery;
//! [`pending`], [`orphans`], [`discard`], [`expired`] and [`over_budget`] keep the staging
//! folder bounded.

use freemkv_engine::{Event, Level, Progress, Sink};
use libfreemkv::Halt;
use libfreemkv::halt::{Liveness, Stall, StallTimer, WAIT_SLICE};
use libfreemkv::io::ArtifactLock;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SIDECAR_FORMAT: u32 = 1;
const KEPT_MKV: &str = ".staged.mkv";
const KEPT_JSON: &str = ".staged.json";
const KEPT_TMP: &str = ".staged.json.tmp";
// A sidecar is a few hundred bytes; anything far larger is not one.
const SIDECAR_MAX: u64 = 1 << 20;
const STEM_MAX: usize = 80;
// Two probes of the same bytes agree; this only absorbs a float's JSON round trip.
const RUNTIME_EPSILON: f64 = 0.001;

/// The delivery's waits: production values by default, a parameter so tests scale time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// A copy or verify with no bytes moved for this long is `TimedOut` (E9073).
    pub stall: Duration,
    /// Progress is reported at most this often.
    pub activity_every: Duration,
    /// How often sync and verify progress refreshes the file's mtime for a lock waiter.
    pub lock_beat: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(60),
            activity_every: Duration::from_millis(250),
            lock_beat: libfreemkv::io::artifact_lock::ARTIFACT_LOCK_WINDOW / 10,
        }
    }
}

// Bound buffered writes so a multi-GB delivery cannot fill the NFS RPC queue
// ahead of directory checks. The final durable sync runs before landing.
struct CheckpointWriter<W, F> {
    writer: W,
    pending: usize,
    limit: usize,
    sync: F,
}

impl<W: Write, F: FnMut(&mut W) -> io::Result<()>> Write for CheckpointWriter<W, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.writer.write(bytes)?;
        self.pending = self.pending.saturating_add(n);
        if self.pending >= self.limit {
            (self.sync)(&mut self.writer)?;
            self.pending = 0;
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// A reader the verify can hand to its worker thread.
pub(crate) trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

/// The file primitives of a delivery; a seam so a test can stall or fail each step.
pub(crate) trait DeliverIo: Sync {
    /// Create `path` for the copy; an existing file is refused, never truncated.
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(Box::new(CheckpointWriter {
            writer: file,
            pending: 0,
            limit: 64 * 1024 * 1024,
            sync: sync_copy_checkpoint,
        }))
    }
    /// Make `file` durable: stall-based, `halt`-aware, reporting `(bytes_done, total)`.
    fn sync(
        &self,
        file: &std::fs::File,
        halt: &Halt,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<()> {
        libfreemkv::io::durable_sync_file(file, Some(halt), on_progress)
    }
    /// Open `path` for the verify reads.
    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }
    /// Move the verified `partial` onto `target` (see [`land`]).
    fn land(&self, partial: &Path, target: &Path, replace: bool) -> io::Result<()> {
        land(partial, target, replace)
    }
    fn timing(&self) -> Timing {
        Timing::default()
    }
}

/// The production primitives.
pub(crate) struct OsIo;

impl DeliverIo for OsIo {}

fn sync_copy_checkpoint(file: &mut std::fs::File) -> io::Result<()> {
    // Rust's macOS sync_data requests F_FULLFSYNC, which SMB may reject.
    // The shared flusher retains durability via fsync on that specific error.
    #[cfg(target_os = "macos")]
    {
        libfreemkv::io::durable_sync_file(file, None, &mut |_, _| {})
    }
    #[cfg(not(target_os = "macos"))]
    file.sync_data()
}

/// A delivery failed after the local file verified, and that file was kept for [`resume`].
/// Carried inside the returned [`io::Error`] (see [`kept_of`]), which keeps the cause's
/// [`io::ErrorKind`], Display and error code.
#[derive(Debug)]
pub(crate) struct Kept {
    /// The kept MKV, `<stage>/<name>.staged.mkv`.
    pub path: PathBuf,
    /// Its sidecar, `<stage>/<name>.staged.json`.
    #[allow(dead_code)]
    pub sidecar: PathBuf,
    /// The step that failed: `"copy"`, `"sync"`, `"verify"` (the library copy), `"replace"`,
    /// or `"target"` (a resume could not examine the target).
    pub phase: &'static str,
    /// The failure itself, with its OS error intact.
    pub cause: io::Error,
}

impl std::fmt::Display for Kept {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.cause.fmt(f)
    }
}

impl std::error::Error for Kept {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause.source()
    }
}

/// The [`Kept`] inside an error from [`remux_via_stage`] or [`resume`].
pub(crate) fn kept_of(e: &io::Error) -> Option<&Kept> {
    e.get_ref()?.downcast_ref::<Kept>()
}

fn wrap(path: PathBuf, sidecar: PathBuf, phase: &'static str, cause: io::Error) -> io::Error {
    let kind = cause.kind();
    io::Error::new(
        kind,
        Kept {
            path,
            sidecar,
            phase,
            cause,
        },
    )
}

/// What sat at the target when a remux began; a resume refuses a target that changed since.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub(crate) enum TargetStamp {
    Absent,
    Present {
        len: u64,
        mtime_secs: Option<u64>,
        mtime_nanos: Option<u32>,
    },
    Unknown,
}

impl TargetStamp {
    pub(crate) fn of(path: &Path) -> Self {
        match std::fs::symlink_metadata(path) {
            Ok(m) => Self::present(&m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::Absent,
            Err(_) => Self::Unknown,
        }
    }

    fn present(m: &std::fs::Metadata) -> Self {
        let t = m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        Self::Present {
            len: m.len(),
            mtime_secs: t.map(|d| d.as_secs()),
            mtime_nanos: t.map(|d| d.subsec_nanos()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Sidecar {
    format: u32,
    target: PathBuf,
    replace: bool,
    target_before: TargetStamp,
    iso: PathBuf,
    iso_len: Option<u64>,
    iso_mtime_secs: Option<u64>,
    title: Option<usize>,
    size: u64,
    runtime_secs: Option<f64>,
    expected_secs: Option<f64>,
    writing_app: Option<String>,
    engine_version: String,
    created_at: u64,
    attempts: u32,
    last_attempt_at: u64,
    failed_phase: String,
    error: String,
    error_code: Option<u16>,
}

#[derive(Deserialize)]
struct FormatOnly {
    format: u32,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Sidecar {
    fn failed(&mut self, phase: &str, cause: &io::Error) {
        self.last_attempt_at = now_secs();
        self.failed_phase = phase.to_string();
        self.error = cause.to_string();
        self.error_code = libfreemkv::error_code(cause);
    }
}

/// One kept remux, read from its sidecar ([`pending`], [`read`]).
#[derive(Clone, Debug)]
pub(crate) struct KeptInfo {
    /// The kept MKV and its sidecar.
    pub staged: PathBuf,
    pub sidecar: PathBuf,
    /// Where it lands, and whether an existing file there was to be replaced.
    pub target: PathBuf,
    pub replace: bool,
    /// The image it was muxed from.
    pub iso: PathBuf,
    /// Bytes the kept file holds on the staging disk.
    pub size: u64,
    pub created_at: SystemTime,
    /// Failed deliveries so far (1 when first kept).
    pub attempts: u32,
    /// The last failure's Display and error code.
    pub error: String,
    pub error_code: Option<u16>,
    record: Sidecar,
}

impl KeptInfo {
    fn of(record: Sidecar, staged: PathBuf, sidecar: PathBuf) -> Self {
        Self {
            staged,
            sidecar,
            target: record.target.clone(),
            replace: record.replace,
            iso: record.iso.clone(),
            size: record.size,
            created_at: UNIX_EPOCH + Duration::from_secs(record.created_at),
            attempts: record.attempts,
            error: record.error.clone(),
            error_code: record.error_code,
            record,
        }
    }
}

// Cancellation: the caller's token, or the job sink's `should_cancel`; sticky once seen.
struct Stop<'a> {
    halt: &'a Halt,
    sink: &'a dyn Sink,
    seen: AtomicBool,
}

pub(crate) fn with_halt<T>(sink: &dyn Sink, action: impl FnOnce(&Halt) -> T) -> T {
    let halt = Halt::new();
    Stop::new(&halt, sink).linked(action)
}

impl<'a> Stop<'a> {
    fn new(halt: &'a Halt, sink: &'a dyn Sink) -> Self {
        Self {
            halt,
            sink,
            seen: AtomicBool::new(false),
        }
    }

    fn is_cancelled(&self) -> bool {
        if self.seen.load(Ordering::Acquire) {
            return true;
        }
        let now = self.halt.is_cancelled() || self.sink.should_cancel();
        if now {
            self.seen.store(true, Ordering::Release);
        }
        now
    }

    // A `Halt` a bridge thread cancels within one `WAIT_SLICE` of `self`, for libfreemkv waits.
    fn linked<R>(&self, f: impl FnOnce(&Halt) -> R) -> R {
        let child = Halt::new();
        if self.is_cancelled() {
            child.cancel();
        }
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            let bridge = s.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    if self.is_cancelled() {
                        child.cancel();
                        return;
                    }
                    std::thread::park_timeout(WAIT_SLICE);
                }
            });
            let _end = EndBridge(&done, bridge.thread().clone());
            f(&child)
        })
    }
}

// Ends a bridge on every exit, a panic included, so the scope can join it.
struct EndBridge<'a>(&'a AtomicBool, std::thread::Thread);

impl Drop for EndBridge<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
        self.1.unpark();
    }
}

fn halted() -> io::Error {
    libfreemkv::Error::Halted.into()
}

// Cleanup may itself block on NFS. Its worker retains the artifact lock until the
// partial is removed, so a later delivery cannot race its delayed deletion.
struct RemoteGuard<'a> {
    path: Option<PathBuf>,
    lock: Option<ArtifactLock>,
    sink: &'a dyn Sink,
}

fn bounded_cleanup(cleanup: impl FnOnce() + Send + 'static, limit: Duration) -> io::Result<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    match crate::server::daemon::spawn_background("library-cleanup", move || {
        cleanup();
        let _ = tx.send(());
    }) {
        Ok(_) => match rx.recv_timeout(limit) {
            Ok(()) => Ok(true),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(false),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("cleanup worker stopped unexpectedly"))
            }
        },
        Err(e) => Err(e),
    }
}

impl Drop for RemoteGuard<'_> {
    fn drop(&mut self) {
        let path = self.path.take();
        let lock = self.lock.take();
        let cleanup = bounded_cleanup(
            move || {
                if let Some(path) = path {
                    match std::fs::remove_file(&path) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => {
                            tracing::warn!(path = %path.display(), error = %e, "could not remove delivery partial");
                        }
                        _ => {}
                    }
                }
                if let Some(lock) = lock
                    && let Err(e) = lock.delete()
                {
                    tracing::warn!(error = %e, "could not remove delivery lock");
                }
            },
            Duration::from_secs(1),
        );
        match cleanup {
            Ok(true) => {}
            Ok(false) => self.sink.log(Level::Warn, "Delivery cleanup is waiting for storage; the target remains locked until cleanup finishes."),
            Err(e) => self.sink.log(Level::Warn, &format!("Could not start delivery cleanup: {e}")),
        }
    }
}

// Removes a local file unless disarmed; emptied first, as a leaked copy worker may hold it.
struct LocalGuard(Option<PathBuf>);

impl Drop for LocalGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            drop_file(&path);
        }
    }
}

fn drop_file(path: &Path) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_len(0);
    }
    let _ = std::fs::remove_file(path);
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn partial_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    target.with_file_name(name)
}

// Only NotFound means absent: EIO/ESTALE surface, so a flaky mount never reads as empty.
fn target_present(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn target_exists(target: &Path) -> io::Error {
    let path = target.display().to_string();
    libfreemkv::Error::RemuxTargetExists { path }.into()
}

fn refuse_existing(target: &Path, replace: bool) -> io::Result<()> {
    if !replace && target_present(target)? {
        return Err(target_exists(target));
    }
    Ok(())
}

/// Move `partial` onto `target`. Without `replace`, a hard link lands it only if the target is
/// still absent. Unsupported filesystems fail closed, never using an overwriting rename.
pub(crate) fn land(partial: &Path, target: &Path, replace: bool) -> io::Result<()> {
    if replace {
        return std::fs::rename(partial, target);
    }
    libfreemkv::io::publish::no_replace(partial, target)
}

fn progress(pass: &'static str, bytes_done: u64, bytes_total: u64) -> Progress {
    Progress {
        pass: std::borrow::Cow::Borrowed(pass),
        bytes_done,
        bytes_total,
        ..Default::default()
    }
}

// Reports an increase of `done` at most once per `every`; the final value always.
struct Activity {
    every: Duration,
    last_at: Option<Instant>,
    last: u64,
}

impl Activity {
    fn new(every: Duration) -> Self {
        Self {
            every,
            last_at: None,
            last: 0,
        }
    }

    fn due(&mut self, done: u64, last: bool) -> bool {
        let grew = done > self.last;
        let spaced = self.last_at.is_none_or(|t| t.elapsed() >= self.every);
        if grew && (spaced || last) {
            self.last = done;
            self.last_at = Some(Instant::now());
            return true;
        }
        false
    }
}

// A lock waiter sees the holder live only while `<target>.partial` changes: sync and verify
// change nothing, so their progress bumps its mtime, at most once per `every`.
struct LockBeat {
    file: Option<std::fs::File>,
    every: Duration,
    last: Option<Instant>,
}

impl LockBeat {
    fn new(file: Option<std::fs::File>, every: Duration) -> Self {
        Self {
            file,
            every,
            last: None,
        }
    }

    fn progressed(&mut self) {
        if self.last.is_some_and(|t| t.elapsed() < self.every) {
            return;
        }
        if let Some(f) = &self.file {
            let _ = f.set_modified(SystemTime::now());
        }
        self.last = Some(Instant::now());
    }
}

// Counts the bytes each read returns; seeks count nothing.
struct Counting<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl<R: Seek> Seek for Counting<R> {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

// Waits on a worker whose progress is the byte count `moved`: `Halted` on a Stop,
// `TimedOut { op }` after `timing.stall` without progress. Either return leaks the worker.
fn watch_worker<T>(
    rx: &std::sync::mpsc::Receiver<T>,
    moved: &AtomicU64,
    op: &'static str,
    stop: &Stop<'_>,
    timing: Timing,
    mut beat: LockBeat,
    report: &dyn Fn(u64) -> Progress,
) -> io::Result<T> {
    let liveness = Liveness::new();
    let mut timer = StallTimer::new(timing.stall, &liveness);
    let (mut every, mut seen) = (Activity::new(timing.activity_every), 0);
    loop {
        let done = rx.recv_timeout(WAIT_SLICE);
        let now = moved.load(Ordering::Relaxed);
        if now > seen {
            seen = now;
            liveness.bump();
            beat.progressed();
        }
        if every.due(now, done.is_ok()) {
            stop.sink.progress(&report(now));
        }
        match done {
            Ok(result) => return Ok(result),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(libfreemkv::Error::WorkerLost { op }.into());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if stop.is_cancelled() {
            return Err(halted());
        }
        if timer.poll(&liveness) == Stall::Expired {
            return Err(libfreemkv::Error::TimedOut { op }.into());
        }
    }
}

// The copy on a worker: a Stop within a slice, `TimedOut { op: "copy" }` after the stall
// window with no bytes written, and a size that must match the source's.
fn copy_to(
    source: &Path,
    destination: &Path,
    stop: &Stop<'_>,
    io: &dyn DeliverIo,
) -> io::Result<()> {
    let timing = io.timing();
    let mut src = std::fs::File::open(source)?;
    let total = src.metadata()?.len();
    let mut dst = io.create_new(destination)?;
    let copied = Arc::new(AtomicU64::new(0));
    let quit = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let (count, give_up) = (copied.clone(), quit.clone());
    crate::server::daemon::spawn_background("library-deliver-copy", move || {
        let result = copy_all(&mut src, &mut *dst, &count, &give_up);
        drop((src, dst));
        let _ = tx.send(result);
    })?;
    let started = Instant::now();
    let report = |done: u64| {
        let speed = (done as f64 / started.elapsed().as_secs_f64().max(0.001)) as u64;
        Progress {
            speed_bps: speed,
            eta_secs: (speed > 0).then(|| total.saturating_sub(done) / speed),
            ..progress("copy", done, total)
        }
    };
    let beat = LockBeat::new(None, timing.lock_beat);
    let watched = watch_worker(&rx, &copied, "copy", stop, timing, beat, &report);
    if watched.is_err() {
        quit.store(true, Ordering::Relaxed);
    }
    let done = watched??;
    let have = match done {
        n if n != total => n,
        _ => std::fs::metadata(destination)?.len(),
    };
    if have != total {
        let want = total;
        return Err(libfreemkv::Error::StagedCopySizeMismatch { have, want }.into());
    }
    Ok(())
}

fn copy_all(
    src: &mut std::fs::File,
    dst: &mut dyn Write,
    copied: &AtomicU64,
    quit: &AtomicBool,
) -> io::Result<u64> {
    let mut buf = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    loop {
        if quit.load(Ordering::Relaxed) {
            return Err(halted());
        }
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        done += n as u64;
        copied.store(done, Ordering::Relaxed);
    }
    dst.flush()?;
    Ok(done)
}

// The copy's durable sync, as its own `"sync"` phase with real progress.
fn sync_file(path: &Path, stop: &Stop<'_>, io: &dyn DeliverIo) -> io::Result<()> {
    let timing = io.timing();
    stop.sink.event(&Event::Phase { name: "sync" });
    // Read-write: Windows' FlushFileBuffers needs a handle with GENERIC_WRITE.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut every = Activity::new(timing.activity_every);
    let mut beat = LockBeat::new(file.try_clone().ok(), timing.lock_beat);
    stop.linked(|h| {
        io.sync(&file, h, &mut |done, total| {
            beat.progressed();
            if every.due(done, done >= total) {
                stop.sink.progress(&progress("sync", done, total));
            }
        })
    })
}

// Makes the landing durable through the same halt-aware sync; Unix only (NTFS journals it).
fn sync_parent(path: &Path, stop: &Stop<'_>, io: &dyn DeliverIo) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let dir = std::fs::File::open(dir)?;
        stop.linked(|h| io.sync(&dir, h, &mut |_, _| {}))?;
    }
    #[cfg(not(unix))]
    let _ = (path, stop, io);
    Ok(())
}

/// What a copy must show to pass: the local file's size, and its verified runtime.
#[derive(Clone, Copy, Debug)]
struct Expect {
    size: u64,
    runtime: Option<f64>,
}

fn muxed_runtime(probe: &libfreemkv::MkvProbe) -> Option<f64> {
    probe.last_cue_secs.or(probe.duration_secs)
}

fn verify_failed(path: &Path, kind: libfreemkv::RemuxVerifyKind) -> io::Error {
    let path = path.display().to_string();
    libfreemkv::Error::RemuxVerifyFailed { kind, path }.into()
}

// One verify, its own `"verify"` phase and verdict: the size, then a probe of the header and
// Cues on a stall-watched worker, which must carry tracks and the expected runtime.
fn verify(
    path: &Path,
    want: Expect,
    stop: &Stop<'_>,
    io: &dyn DeliverIo,
) -> io::Result<libfreemkv::MkvProbe> {
    stop.sink.event(&Event::Phase { name: "verify" });
    let verified = verify_inner(path, want, stop, io);
    if !verified.as_ref().is_err_and(libfreemkv::is_halt) {
        stop.sink.event(&Event::Verify {
            path,
            ok: verified.is_ok(),
            runtime_secs: verified.as_ref().ok().and_then(muxed_runtime),
            expected_secs: want.runtime.unwrap_or(0.0),
        });
    }
    verified
}

fn verify_inner(
    path: &Path,
    want: Expect,
    stop: &Stop<'_>,
    io: &dyn DeliverIo,
) -> io::Result<libfreemkv::MkvProbe> {
    use libfreemkv::RemuxVerifyKind;
    let timing = io.timing();
    let have = std::fs::metadata(path)?.len();
    if have == 0 {
        return Err(verify_failed(path, RemuxVerifyKind::Empty));
    }
    if have != want.size {
        let want = want.size;
        return Err(libfreemkv::Error::StagedCopySizeMismatch { have, want }.into());
    }
    let read = Arc::new(AtomicU64::new(0));
    let reader = Counting {
        inner: io::BufReader::new(io.open_read(path)?),
        read: read.clone(),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    crate::server::daemon::spawn_background("library-deliver-verify", move || {
        let _ = tx.send(libfreemkv::probe_mkv_with_cues(reader));
    })?;
    let beat_file = std::fs::OpenOptions::new().write(true).open(path).ok();
    let beat = LockBeat::new(beat_file, timing.lock_beat);
    let report = |done| progress("verify", done, have);
    let probe = watch_worker(&rx, &read, "verify", stop, timing, beat, &report)??;
    if probe.tracks.is_empty() {
        return Err(verify_failed(path, RemuxVerifyKind::NoTracks));
    }
    if let Some(want_secs) = want.runtime {
        let Some(have_secs) = muxed_runtime(&probe).filter(|r| r.is_finite()) else {
            return Err(verify_failed(path, RemuxVerifyKind::NoRuntime));
        };
        if (have_secs - want_secs).abs() > RUNTIME_EPSILON {
            let kind = RemuxVerifyKind::RuntimeMismatch {
                have_secs,
                want_secs,
            };
            return Err(verify_failed(path, kind));
        }
    }
    Ok(probe)
}

// Everything after the local verify: `<target>.lock`, then copy, sync, verify, land and the
// folder sync. `Ok(replaced)`, or the failed phase and its error; `<target>.partial` goes on
// every failure. A resume passes `before`, the target as the remux found it.
fn deliver(
    local: &Path,
    target: &Path,
    replace: bool,
    want: Expect,
    before: Option<TargetStamp>,
    stop: &Stop<'_>,
    io: &dyn DeliverIo,
) -> Result<bool, (&'static str, io::Error)> {
    let watch = [local];
    let lock = stop
        .linked(|h| ArtifactLock::acquire(target, &watch, h))
        .map_err(|e| ("copy", io::Error::from(e)))?;
    let mut remote = RemoteGuard {
        path: None,
        lock: Some(lock),
        sink: stop.sink,
    };
    if let Some(before) = before {
        let now = match std::fs::symlink_metadata(target) {
            Ok(m) => TargetStamp::present(&m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => TargetStamp::Absent,
            Err(e) => return Err(("target", e)),
        };
        let changed = matches!(now, TargetStamp::Present { .. })
            && before != TargetStamp::Unknown
            && now != before;
        if changed {
            return Err(("target", target_exists(target)));
        }
    }
    let partial = partial_path(target);
    remote.path = Some(partial.clone());
    let replaced = deliver_steps(local, target, &partial, replace, want, stop, io)?;
    remote.path = None;
    // The landing committed: a Stop or a failure during the folder sync cuts only the sync short.
    match sync_parent(target, stop, io) {
        Err(e) if libfreemkv::is_halt(&e) => stop.sink.log(
            Level::Warn,
            "stopped during the folder sync after the file landed; it may not be durable yet",
        ),
        Err(e) => stop.sink.log(
            Level::Warn,
            &format!("the file landed but its folder sync failed: {e}"),
        ),
        Ok(()) => {}
    }
    Ok(replaced)
}

fn deliver_steps(
    local: &Path,
    target: &Path,
    partial: &Path,
    replace: bool,
    want: Expect,
    stop: &Stop<'_>,
    io: &dyn DeliverIo,
) -> Result<bool, (&'static str, io::Error)> {
    stop.sink.event(&Event::Phase { name: "copy" });
    let copy = || -> io::Result<()> {
        refuse_existing(target, replace)?;
        remove_if_present(partial)?;
        copy_to(local, partial, stop, io)
    };
    copy().map_err(|e| ("copy", e))?;
    sync_file(partial, stop, io).map_err(|e| ("sync", e))?;
    verify(partial, want, stop, io).map_err(|e| ("verify", e))?;

    stop.sink.event(&Event::Phase { name: "replace" });
    let replace_step = || -> io::Result<bool> {
        // Re-checked: the target may have appeared while the file was copied.
        refuse_existing(target, replace)?;
        let replaced = target_present(target)?;
        // The landing is the commit: a Stop before it leaves the target untouched.
        if stop.is_cancelled() {
            return Err(halted());
        }
        io.land(partial, target, replace)?;
        Ok(replaced)
    };
    replace_step().map_err(|e| ("replace", e))
}

// A failure keeps the verified local file unless it was a Stop, or the target appeared.
fn keeps(e: &io::Error, stop: &Stop<'_>) -> bool {
    !libfreemkv::is_halt(e)
        && !stop.is_cancelled()
        && libfreemkv::error_code(e) != Some(libfreemkv::error::E_REMUX_TARGET_EXISTS)
}

/// A remux whose verified MKV is on local staging, ready to be delivered.
pub(crate) struct Finished<'a> {
    /// The verified MKV on local staging; consumed: delivered, kept or removed.
    pub file: &'a Path,
    pub target: &'a Path,
    pub replace: bool,
    pub iso: &'a Path,
    pub title: Option<usize>,
    /// The title's declared runtime, when it declares one.
    pub expected_secs: Option<f64>,
    pub verified: &'a libfreemkv::MkvProbe,
    pub target_before: TargetStamp,
}

/// Deliver `f.file` to `f.target`. `Ok(replaced)` removes the local file; a storage failure
/// keeps it with a sidecar and returns the cause wrapped in a [`Kept`]; a Stop or a target
/// that appeared removes it and returns the error as is.
pub(crate) fn deliver_finished(
    f: &Finished<'_>,
    sink: &dyn Sink,
    halt: &Halt,
    io: &dyn DeliverIo,
) -> io::Result<bool> {
    let mut local = LocalGuard(Some(f.file.to_path_buf()));
    let size = std::fs::metadata(f.file)?.len();
    let want = Expect {
        size,
        runtime: muxed_runtime(f.verified).filter(|r| r.is_finite()),
    };
    let stop = Stop::new(halt, sink);
    match deliver(f.file, f.target, f.replace, want, None, &stop, io) {
        Ok(replaced) => {
            if replaced {
                sink.event(&Event::Replaced { path: f.target });
            }
            Ok(replaced)
        }
        Err((phase, e)) if keeps(&e, &stop) => Err(keep(f, &mut local, want, phase, e, sink)),
        Err((_, e)) => Err(e),
    }
}

// The kept pair for `target` in `stage`: the target's stem and a hash of its whole path.
fn kept_paths(stage: &Path, target: &Path) -> (PathBuf, PathBuf) {
    // FNV-1a: stable across builds, and only a name.
    let hash = target
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    let stem = target.file_stem().unwrap_or_default().to_string_lossy();
    let stem: String = stem
        .chars()
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(STEM_MAX)
        .collect();
    let name = format!("{stem}.{:012x}", hash >> 16);
    (
        stage.join(format!("{name}{KEPT_MKV}")),
        stage.join(format!("{name}{KEPT_JSON}")),
    )
}

// The pair `path` (either half) belongs to; `None` for a name that is not a kept one.
fn pair_of(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let name = path.file_name()?.to_str()?;
    let base = [KEPT_MKV, KEPT_JSON]
        .iter()
        .find_map(|s| name.strip_suffix(s))
        .filter(|b| !b.is_empty())?;
    Some((
        path.with_file_name(format!("{base}{KEPT_MKV}")),
        path.with_file_name(format!("{base}{KEPT_JSON}")),
    ))
}

fn tmp_of(sidecar: &Path) -> PathBuf {
    let mut name = sidecar.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    sidecar.with_file_name(name)
}

fn folder_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

// Best effort: the staging folder is local, and the files in it are already durable.
fn sync_folder(dir: &Path) {
    #[cfg(unix)]
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

// Temp file, fsync, rename, folder sync: a reader sees the old sidecar or the new one.
fn write_sidecar(path: &Path, record: &Sidecar) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
    let tmp = tmp_of(path);
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    sync_folder(folder_of(path));
    Ok(())
}

fn record_of(f: &Finished<'_>, want: Expect, phase: &str, cause: &io::Error) -> Sidecar {
    let iso = std::fs::metadata(f.iso).ok();
    let now = now_secs();
    let mut record = Sidecar {
        format: SIDECAR_FORMAT,
        target: f.target.to_path_buf(),
        replace: f.replace,
        target_before: f.target_before,
        iso: f.iso.to_path_buf(),
        iso_len: iso.as_ref().map(std::fs::Metadata::len),
        iso_mtime_secs: iso
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs()),
        title: f.title,
        size: want.size,
        runtime_secs: want.runtime,
        expected_secs: f.expected_secs.filter(|s| s.is_finite() && *s > 0.0),
        writing_app: f.verified.writing_app.clone(),
        engine_version: crate::server::VERSION_LABEL.to_string(),
        created_at: now,
        attempts: 1,
        last_attempt_at: now,
        failed_phase: String::new(),
        error: String::new(),
        error_code: None,
    };
    record.failed(phase, cause);
    record
}

// Keep the verified local file under its kept name with its sidecar, and wrap `cause`; if
// keeping fails, the file goes (`local` stays armed) and `cause` is returned as is.
fn keep(
    f: &Finished<'_>,
    local: &mut LocalGuard,
    want: Expect,
    phase: &'static str,
    cause: io::Error,
    sink: &dyn Sink,
) -> io::Error {
    let (path, sidecar) = kept_paths(folder_of(f.file), f.target);
    let record = record_of(f, want, phase, &cause);
    if let Err(e) = std::fs::rename(f.file, &path) {
        return not_kept(f.file, &e, cause, sink);
    }
    local.0 = Some(path.clone());
    sync_folder(folder_of(&path));
    if let Err(e) = write_sidecar(&sidecar, &record) {
        return not_kept(&path, &e, cause, sink);
    }
    local.0 = None;
    sink.log(
        Level::Warn,
        &format!(
            "the {phase} step failed ({cause}); kept the verified file at {} to finish later",
            path.display()
        ),
    );
    wrap(path, sidecar, phase, cause)
}

fn not_kept(at: &Path, why: &io::Error, cause: io::Error, sink: &dyn Sink) -> io::Error {
    sink.log(
        Level::Warn,
        &format!("could not keep the verified file {}: {why}", at.display()),
    );
    cause
}

enum ReadError {
    // Not a kept name, or a sidecar of a newer format: left alone.
    NotOurs,
    Io(io::Error),
    // The sidecar exists but is not one.
    Corrupt,
}

fn staging_invalid() -> io::Error {
    libfreemkv::Error::RemuxStagingInvalid.into()
}

fn read_pair(path: &Path) -> Result<KeptInfo, ReadError> {
    let (staged, sidecar) = pair_of(path).ok_or(ReadError::NotOurs)?;
    let mut bytes = Vec::new();
    let file = std::fs::File::open(&sidecar).map_err(ReadError::Io)?;
    file.take(SIDECAR_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > SIDECAR_MAX {
        return Err(ReadError::Corrupt);
    }
    let format = serde_json::from_slice::<FormatOnly>(&bytes).map_err(|_| ReadError::Corrupt)?;
    if format.format > SIDECAR_FORMAT {
        return Err(ReadError::NotOurs);
    }
    let record: Sidecar = serde_json::from_slice(&bytes).map_err(|_| ReadError::Corrupt)?;
    let target_ok = record.target.is_absolute() && record.target.file_name().is_some();
    if record.format != SIDECAR_FORMAT || !target_ok {
        return Err(ReadError::Corrupt);
    }
    Ok(KeptInfo::of(record, staged, sidecar))
}

/// The kept remux whose MKV or sidecar is `path`. A name that is not a kept one, or a damaged
/// or newer-format sidecar, is `RemuxStagingInvalid` (E9079); a missing sidecar is `NotFound`.
pub(crate) fn read(path: &Path) -> io::Result<KeptInfo> {
    match read_pair(path) {
        Ok(info) => Ok(info),
        Err(ReadError::Io(e)) => Err(e),
        Err(ReadError::NotOurs | ReadError::Corrupt) => Err(staging_invalid()),
    }
}

/// Whether `path` is half of a kept remux: its MKV exists and its sidecar reads (or is of a
/// newer format). Anything else of those names is debris (see [`orphans`]).
pub(crate) fn is_kept(path: &Path) -> bool {
    let Some((staged, _)) = pair_of(path) else {
        return false;
    };
    let complete = || std::fs::metadata(&staged).is_ok_and(|m| m.is_file());
    match read_pair(path) {
        Ok(_) | Err(ReadError::NotOurs) => complete(),
        Err(ReadError::Io(_) | ReadError::Corrupt) => false,
    }
}

fn kept_names(stage: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(stage) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                [KEPT_MKV, KEPT_JSON, KEPT_TMP]
                    .iter()
                    .any(|s| n.len() > s.len() && n.ends_with(s))
            })
        })
        .collect();
    paths.sort();
    paths
}

/// Every kept remux in `stage`, oldest first. Unreadable folders, damaged sidecars and
/// sidecars without their MKV are skipped (see [`orphans`]).
pub(crate) fn pending(stage: &Path) -> Vec<KeptInfo> {
    let mut found: Vec<KeptInfo> = kept_names(stage)
        .iter()
        .filter(|p| p.to_str().is_some_and(|s| s.ends_with(KEPT_JSON)))
        .filter_map(|p| read_pair(p).ok())
        .filter(|i| std::fs::metadata(&i.staged).is_ok_and(|m| m.is_file()))
        .collect();
    found.sort_by_key(|i| (i.created_at, i.staged.clone()));
    found
}

/// Files in `stage` named like a kept remux that are not one: an MKV without a readable
/// sidecar, a sidecar without its MKV, a sidecar left half-written. Safe to delete while no
/// remux runs over this folder.
pub(crate) fn orphans(stage: &Path) -> Vec<PathBuf> {
    kept_names(stage)
        .into_iter()
        .filter(|p| !is_kept(p))
        .collect()
}

/// Delete the kept remux `path` names (its MKV or its sidecar): both files and any
/// half-written sidecar. A name that is not a kept one is refused (`RemuxStagingInvalid`) and
/// nothing is touched; files already gone are not an error.
pub(crate) fn discard(path: &Path) -> io::Result<()> {
    let (staged, sidecar) = pair_of(path).ok_or_else(staging_invalid)?;
    let mut first = Ok(());
    for p in [staged, tmp_of(&sidecar), sidecar] {
        if let Err(e) = remove_if_present(&p) {
            first = first.and(Err(e));
        }
    }
    first
}

/// Whether `info` was first kept more than `max_age` ago. A clock that moved backwards
/// expires nothing.
pub(crate) fn expired(info: &KeptInfo, max_age: Duration) -> bool {
    SystemTime::now()
        .duration_since(info.created_at)
        .is_ok_and(|age| age > max_age)
}

/// The oldest of `pending` to discard so the rest hold at most `max_bytes` (none when they
/// already fit).
pub(crate) fn over_budget(pending: &[KeptInfo], max_bytes: u64) -> Vec<&KeptInfo> {
    let mut by_age: Vec<&KeptInfo> = pending.iter().collect();
    by_age.sort_by_key(|i| i.created_at);
    let mut total: u64 = pending.iter().map(|i| i.size).sum();
    by_age
        .into_iter()
        .take_while(|i| {
            let over = total > max_bytes;
            total = total.saturating_sub(i.size);
            over
        })
        .collect()
}

/// What a finished delivery produced.
#[derive(Debug)]
pub(crate) struct Delivered {
    pub writing_app: Option<String>,
}

/// Deliver a kept remux (`path` is its MKV or its sidecar): re-check the kept file (size, then
/// a probe of its header and Cues against its recorded runtime) and run only the delivery.
/// Success removes the pair; a storage failure keeps it again ([`Kept`], `attempts` counted);
/// a Stop, or a target that changed since the remux began (E9084), leaves it as is. A kept
/// file that is gone, short or no longer verifies is deleted with its sidecar.
pub(crate) fn resume(
    path: &Path,
    sink: &dyn Sink,
    halt: &Halt,
    io: &dyn DeliverIo,
) -> io::Result<Delivered> {
    let info = match read_pair(path) {
        Ok(info) => info,
        Err(ReadError::Io(e)) => return Err(e),
        Err(ReadError::NotOurs) => return Err(staging_invalid()),
        Err(ReadError::Corrupt) => {
            sink.log(
                Level::Warn,
                &format!("discarding {}: damaged sidecar", path.display()),
            );
            let _ = discard(path);
            return Err(staging_invalid());
        }
    };
    let have = match std::fs::metadata(&info.staged) {
        Ok(m) => m.len(),
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                let _ = discard(&info.sidecar);
            }
            return Err(e);
        }
    };
    if have != info.size {
        sink.log(
            Level::Warn,
            &format!("discarding {}: size changed", info.staged.display()),
        );
        let _ = discard(&info.staged);
        let want = info.size;
        return Err(libfreemkv::Error::StagedCopySizeMismatch { have, want }.into());
    }
    let target_partial = partial_path(&info.target);
    if info.staged == info.target || info.staged == target_partial {
        return Err(staging_invalid());
    }
    let stop = Stop::new(halt, sink);
    let want = Expect {
        size: info.size,
        runtime: info.record.runtime_secs,
    };
    let verified = match verify(&info.staged, want, &stop, io) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::InvalidData && !libfreemkv::is_halt(&e) => {
            sink.log(
                Level::Warn,
                &format!("discarding {}: {e}", info.staged.display()),
            );
            let _ = discard(&info.staged);
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    let before = Some(info.record.target_before);
    match deliver(
        &info.staged,
        &info.target,
        info.replace,
        want,
        before,
        &stop,
        io,
    ) {
        Ok(replaced) => {
            if let Err(e) = discard(&info.staged) {
                let shown = info.staged.display();
                sink.log(Level::Warn, &format!("could not remove {shown}: {e}"));
            }
            if replaced {
                sink.event(&Event::Replaced { path: &info.target });
            }
            Ok(Delivered {
                writing_app: verified.writing_app,
            })
        }
        Err((phase, e)) if keeps(&e, &stop) => Err(keep_again(&info, phase, e, sink)),
        Err((_, e)) => Err(e),
    }
}

// A resume failed for a storage reason: the pair stays, its sidecar counting the attempt.
fn keep_again(
    info: &KeptInfo,
    phase: &'static str,
    cause: io::Error,
    sink: &dyn Sink,
) -> io::Error {
    let mut record = info.record.clone();
    record.attempts = record.attempts.saturating_add(1);
    record.failed(phase, &cause);
    if let Err(e) = write_sidecar(&info.sidecar, &record) {
        let shown = info.sidecar.display();
        sink.log(Level::Warn, &format!("could not update {shown}: {e}"));
    }
    sink.log(
        Level::Warn,
        &format!(
            "the {phase} step failed again ({cause}); {} is kept",
            info.staged.display()
        ),
    );
    wrap(info.staged.clone(), info.sidecar.clone(), phase, cause)
}

// Forwards every call, noting the title the engine opened: its index and declared runtime.
struct Capture<'a> {
    inner: &'a dyn Sink,
    title: Mutex<(Option<usize>, Option<f64>)>,
}

impl Sink for Capture<'_> {
    fn log(&self, level: Level, msg: &str) {
        self.inner.log(level, msg);
    }

    fn title_opened(&self, title: &libfreemkv::DiscTitle) {
        self.title.lock().unwrap_or_else(|e| e.into_inner()).1 = Some(title.duration_secs);
        self.inner.title_opened(title);
    }

    fn progress(&self, p: &Progress) {
        self.inner.progress(p);
    }

    fn completed(&self, outcome: &freemkv_engine::Outcome) {
        self.inner.completed(outcome);
    }

    fn event(&self, e: &Event<'_>) {
        if let Event::TitleStart { idx, .. } = e {
            self.title.lock().unwrap_or_else(|e| e.into_inner()).0 = Some(*idx);
        }
        self.inner.event(e);
    }

    fn should_cancel(&self) -> bool {
        self.inner.should_cancel()
    }
}

/// The job's numbered files in the staging folder, as [`remux_via_stage`] names them.
pub(crate) fn stage_file(stage: &Path, job_id: u64) -> PathBuf {
    stage.join(format!("{job_id}.mkv"))
}

/// Remux `request` through local staging: the engine muxes, syncs, verifies and lands the MKV
/// in `stage` (as `<job_id>.mkv`), then [`deliver_finished`] copies it into the library.
pub(crate) fn remux_via_stage(
    request: &freemkv_engine::RemuxJob,
    keys: &freemkv_engine::KeyParams,
    sink: &dyn Sink,
    stage: &Path,
    job_id: u64,
) -> io::Result<freemkv_engine::RemuxReport> {
    // Refused before anything is read, as a direct remux refuses it.
    refuse_existing(&request.target, request.replace)?;
    let target_before = TargetStamp::of(&request.target);
    std::fs::create_dir_all(stage)?;
    let local = stage_file(stage, job_id);
    remove_if_present(&local)?;
    let staged = freemkv_engine::RemuxJob {
        target: local.clone(),
        replace: false,
        ..request.clone()
    };
    let capture = Capture {
        inner: sink,
        title: Mutex::new((None, None)),
    };
    let mut report = freemkv_engine::remux_iso(&staged, keys, &capture)?;
    let (title, expected_secs) = *capture.title.lock().unwrap_or_else(|e| e.into_inner());
    let finished = Finished {
        file: &local,
        target: &request.target,
        replace: request.replace,
        iso: request.iso.path(),
        title,
        expected_secs,
        verified: &report.verified,
        target_before,
    };
    report.replaced = deliver_finished(&finished, sink, &Halt::new(), &OsIo)?;
    Ok(report)
}

#[cfg(test)]
pub(crate) mod tests;
