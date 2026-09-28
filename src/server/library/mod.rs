//! The Library: every MKV and source ISO, which freemkv muxed each MKV, and a
//! queue that remuxes out-of-date titles from their ISO through the engine.
//!
//! [`index`] builds the cross-list, [`probe`] reads the muxed-with stamp and
//! runs the fast audit, [`queue`] persists the jobs, [`worker`] runs them one
//! at a time, [`arbiter`] gives rips the mux slot first, [`audit`] queues each MKV's
//! audit ([`deep`] decodes it in full when the setting is on), and [`api`] serves `/api/library*`.

pub mod api;
pub mod arbiter;
pub mod audit;
pub mod deep;
pub mod index;
pub mod links;
pub mod probe;
pub mod queue;
pub mod transcript;
pub mod worker;

use crate::server::config::Config;
use index::{Row, RowKind, RowNote};
use probe::{AuditReport, FileSig, MuxedWith, ProbeCache};
use queue::{Job, JobResult, NewJob, Queue};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};

/// Where the Library looks, resolved from the settings.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Dirs {
    pub library: PathBuf,
    /// `None` when no ISO folder is configured: every row is then MKV-only.
    pub isos: Option<PathBuf>,
    pub iso_subfolders: bool,
}

fn under_output(output: &str, sub: &str) -> PathBuf {
    Path::new(output).join(sub)
}

/// The library folder defaults to where rips land as movies; the ISO folder to
/// where kept ISOs are filed.
pub fn dirs(cfg: &Config) -> Dirs {
    let library = if !cfg.library_dir.is_empty() {
        PathBuf::from(&cfg.library_dir)
    } else {
        under_output(&cfg.output_dir, &cfg.movie_dir)
    };
    let isos = if !cfg.library_iso_dir.is_empty() {
        Some(PathBuf::from(&cfg.library_iso_dir))
    } else if !cfg.iso_dir.is_empty() {
        Some(under_output(&cfg.output_dir, &cfg.iso_dir))
    } else {
        None
    };
    Dirs {
        library,
        isos,
        iso_subfolders: cfg.library_iso_subfolders,
    }
}

/// How a console line reads: the command, plain output, or a coloured outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Cmd,
    Out,
    Ok,
    Warn,
    Err,
    Debug,
}

impl LineKind {
    fn tag(self) -> &'static str {
        match self {
            LineKind::Cmd => "cmd",
            LineKind::Out => "out",
            LineKind::Ok => "ok",
            LineKind::Warn => "warn",
            LineKind::Err => "err",
            LineKind::Debug => "debug",
        }
    }

    fn from_tag(tag: &str) -> Self {
        match tag {
            "cmd" => LineKind::Cmd,
            "ok" => LineKind::Ok,
            "warn" => LineKind::Warn,
            "err" => LineKind::Err,
            "debug" => LineKind::Debug,
            _ => LineKind::Out,
        }
    }
}

/// One line of a job's console, as a terminal would have shown it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct ConsoleLine {
    pub seq: u64,
    pub ts: u64,
    /// The job that printed it, so a client can start clean for the next job.
    pub job: u64,
    pub kind: LineKind,
    pub text: String,
}

/// The running job as the UI shows it.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Running {
    pub job_id: u64,
    pub title: String,
    pub target: PathBuf,
    pub iso: PathBuf,
    pub phase: String,
    pub pct: Option<f64>,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub started_at: u64,
    pub stalled_secs: u64,
    /// The progress line a terminal would keep rewriting in place.
    pub line: String,
}

const CONSOLE_LINES: usize = 2000;

#[derive(Default)]
struct Live {
    lines: VecDeque<ConsoleLine>,
    next_seq: u64,
    running: Option<Running>,
    // The job whose lines the console shows: the running one, else the last.
    job: u64,
    job_title: String,
}

/// What the last scan of the library and ISO folders found. Built off the
/// request path; `GET /api/library` only ever reads it.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// The folders this snapshot describes; `None` before the first scan.
    pub dirs: Option<Dirs>,
    pub mkvs: Vec<index::MkvFile>,
    pub isos: Vec<index::IsoFile>,
    pub rows: Vec<Row>,
    /// Size and mtime of each MKV as the scan saw it.
    pub sigs: HashMap<PathBuf, FileSig>,
    pub incomplete: bool,
    /// The library folder listed with no entries at all (an unmounted share
    /// looks exactly like this).
    pub library_empty: bool,
    pub scanned_at: Option<u64>,
    pub scan_ms: u64,
}

/// The Library's state: the queue, the probe cache, the index and the console.
pub struct Library {
    pub queue: Queue,
    pub probes: ProbeCache,
    pub audits: audit::Audits,
    // The deep_audit setting as the deep loop last read it.
    deep_on: AtomicBool,
    config_dir: PathBuf,
    log_dir: PathBuf,
    live: Mutex<Live>,
    live_generation: AtomicU64,
    // Set by the watchdog to stop a job that stopped moving.
    stall_cancel: AtomicBool,
    // The job "Stop all" cancelled (0 = none); the worker's sink polls it.
    cancel_job: AtomicU64,
    index: RwLock<Arc<Snapshot>>,
    index_generation: AtomicU64,
    // The indexer sleeps on this; `wake` sets it to rescan now.
    wake: (Mutex<bool>, Condvar),
    busy: AtomicBool,
}

static INSTANCE: OnceLock<Arc<Library>> = OnceLock::new();

/// The daemon's Library, opened on first use from the settings' config folder.
pub fn instance(cfg: &Config) -> Arc<Library> {
    INSTANCE
        .get_or_init(|| {
            Arc::new(Library::open(
                Path::new(&cfg.autorip_dir),
                &Path::new(&cfg.log_dir()).join("library"),
            ))
        })
        .clone()
}

/// The Library if something has opened it.
pub fn get() -> Option<Arc<Library>> {
    INSTANCE.get().cloned()
}

/// Ask the indexer to rescan now (settings changed, a rip landed, a button).
pub fn wake() {
    if let Some(lib) = get() {
        lib.wake_indexer();
    }
}

/// Start the remux worker and the background indexer.
pub fn start(cfg: &Arc<RwLock<Config>>) -> std::thread::JoinHandle<()> {
    let lib = instance(&cfg.read().unwrap_or_else(|e| e.into_inner()));
    {
        let (lib, cfg) = (lib.clone(), cfg.clone());
        let _ = std::thread::Builder::new()
            .name("library-index".into())
            .spawn(move || worker::index_loop(&lib, &cfg));
    }
    {
        let (lib, cfg) = (lib.clone(), cfg.clone());
        let _ = std::thread::Builder::new()
            .name("library-audit".into())
            .spawn(move || worker::audit_loop(&lib, &cfg, &arbiter::ARBITER));
    }
    let cfg = cfg.clone();
    std::thread::Builder::new()
        .name("library-remux".into())
        .spawn(move || worker::run(&lib, &cfg, &arbiter::ARBITER))
        .expect("spawn the library worker")
}

/// One row of `GET /api/library`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RowView {
    pub title: String,
    pub key: String,
    pub kind: RowKind,
    pub note: Option<RowNote>,
    pub linked: bool,
    pub mkv: Option<PathBuf>,
    pub iso: Option<PathBuf>,
    pub target: Option<PathBuf>,
    pub size_bytes: Option<u64>,
    /// The MKV's modification time, seconds since the epoch.
    pub modified: Option<u64>,
    pub writing_app: Option<String>,
    /// The stamp cut to "program version" for the table.
    pub muxed_label: Option<String>,
    pub muxed_with: MuxedWith,
    /// False while the MKV's header has not been read yet.
    pub probed: bool,
    /// Remuxable and either out of date or not muxed yet.
    pub needs_remux: bool,
    /// `None` until the auditor has reached this file.
    pub audit: Option<AuditReport>,
    /// The full-decode audit; `None` while the setting is off and no verdict exists.
    pub deep: Option<audit::DeepView>,
    /// Waiting in the audit queue.
    pub audit_queued: bool,
    /// Being audited now.
    pub audit_running: bool,
    pub job: Option<Job>,
    pub result: Option<JobResult>,
}

/// The cross-list as of the last scan.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Listing {
    pub rows: Vec<RowView>,
    /// A folder could not be fully read; rows it may hold are kept from before.
    pub incomplete: bool,
    /// No scan of these folders has finished yet.
    pub scanning: bool,
    pub scanned_at: Option<u64>,
    pub scan_ms: u64,
    /// MKVs whose header is still to be read.
    pub probing: usize,
    /// MKVs queued or being audited.
    pub auditing: usize,
}

impl Library {
    pub fn open(config_dir: &Path, log_dir: &Path) -> Self {
        Self {
            queue: Queue::open(config_dir),
            probes: ProbeCache::default(),
            audits: audit::Audits::open(config_dir),
            deep_on: AtomicBool::new(false),
            config_dir: config_dir.to_path_buf(),
            log_dir: log_dir.to_path_buf(),
            live: Mutex::new(Live::default()),
            live_generation: AtomicU64::new(0),
            stall_cancel: AtomicBool::new(false),
            cancel_job: AtomicU64::new(0),
            index: RwLock::new(Arc::new(Snapshot::default())),
            index_generation: AtomicU64::new(0),
            wake: (Mutex::new(false), Condvar::new()),
            busy: AtomicBool::new(false),
        }
    }

    /// The last scan. Cheap: an `Arc` clone under a read lock.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_snapshot(&self, s: Snapshot) {
        *self.index.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(s);
        self.touch_index();
    }

    /// Moves whenever a scan, a header read or an audit changed what a listing shows.
    pub fn index_generation(&self) -> u64 {
        self.index_generation.load(Ordering::SeqCst)
    }

    pub(crate) fn touch_index(&self) {
        self.index_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Whether the deep audit is on, as its loop last read the setting.
    pub fn deep_enabled(&self) -> bool {
        self.deep_on.load(Ordering::SeqCst)
    }

    pub(crate) fn set_deep_enabled(&self, on: bool) {
        if self.deep_on.swap(on, Ordering::SeqCst) != on {
            self.touch_index();
        }
    }

    /// Whether the indexer is scanning or reading headers right now.
    pub fn indexing(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn wake_indexer(&self) {
        let (flag, cv) = &self.wake;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }

    // Sleep up to `d` unless woken; true if woken.
    fn wait_for_wake(&self, d: std::time::Duration) -> bool {
        let (flag, cv) = &self.wake;
        let g = flag.lock().unwrap_or_else(|e| e.into_inner());
        let (mut g, _) = cv
            .wait_timeout_while(g, d, |woken| !*woken)
            .unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *g)
    }

    fn take_wake(&self) -> bool {
        std::mem::take(&mut *self.wake.0.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Scan the folders and swap in a new snapshot. Filesystem work only; no
    /// lock is held across it. Only a NotFound drops a file a scan missed.
    pub fn rescan(&self, d: &Dirs) {
        let started = std::time::Instant::now();
        let mut mkvs = index::list_mkvs(&d.library);
        let mut isos = match &d.isos {
            Some(dir) => index::list_isos(dir, d.iso_subfolders),
            None => index::Listing::default(),
        };
        let prev = self.snapshot();
        let same = prev.dirs.as_ref() == Some(d);
        if same && mkvs.incomplete {
            for m in &prev.mkvs {
                if !mkvs.files.iter().any(|f| f.path == m.path) && !gone(&m.path) {
                    mkvs.files.push(m.clone());
                }
            }
        }
        if same && isos.incomplete {
            for i in &prev.isos {
                if !isos.files.iter().any(|f| f.path == i.path) && !gone(&i.path) {
                    isos.files.push(i.clone());
                }
            }
        }
        let mut sigs = HashMap::new();
        mkvs.files.retain(|m| match std::fs::metadata(&m.path) {
            Ok(meta) => {
                sigs.insert(m.path.clone(), FileSig::of(&meta));
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => {
                if let Some(s) = prev.sigs.get(&m.path) {
                    sigs.insert(m.path.clone(), *s);
                }
                true
            }
        });
        let links = links::load(&self.config_dir);
        let rows = index::classify(&d.library, &mkvs.files, &isos.files, &links);
        let library_empty = std::fs::read_dir(&d.library).is_ok_and(|mut r| r.next().is_none());
        self.set_snapshot(Snapshot {
            library_empty,
            dirs: Some(d.clone()),
            mkvs: mkvs.files,
            isos: isos.files,
            rows,
            sigs,
            incomplete: mkvs.incomplete || isos.incomplete,
            scanned_at: Some(crate::server::util::epoch_secs()),
            scan_ms: started.elapsed().as_millis() as u64,
        });
    }

    /// Queue the audits `files` still needs (new, changed, never audited, or a full decode
    /// owed while deep audit is on). `complete`: the scan saw every file.
    pub(crate) fn fill_audits(&self, files: &[(PathBuf, FileSig)], complete: bool) {
        let now = crate::server::util::epoch_secs();
        if self.audits.fill(files, self.deep_enabled(), now, complete) > 0 {
            self.touch_index();
        }
    }

    /// The MKVs of the last scan, at the size and mtime it saw.
    pub(crate) fn mkv_files(&self) -> Vec<(PathBuf, FileSig)> {
        let snap = self.snapshot();
        snap.rows
            .iter()
            .filter_map(|r| r.mkv.as_ref())
            .filter_map(|m| Some((m.clone(), *snap.sigs.get(m)?)))
            .collect()
    }

    /// Scan, then read every header and queue every MKV's audit that is owed. The
    /// indexer's pass; tests call it to index synchronously. Stops early (false)
    /// when woken, so a settings change never waits behind a long pass.
    pub fn index_now(&self, d: &Dirs) -> bool {
        self.busy.store(true, Ordering::SeqCst);
        // This pass is the answer to any wake that came before it.
        self.take_wake();
        self.rescan(d);
        let snap = self.snapshot();
        let files: Vec<(PathBuf, FileSig)> = snap
            .rows
            .iter()
            .filter_map(|r| r.mkv.as_ref())
            .filter_map(|m| Some((m.clone(), *snap.sigs.get(m)?)))
            .collect();
        let mut complete = true;
        let mut last = std::time::Instant::now();
        let mut tick = |changed: bool| {
            if changed && last.elapsed() >= std::time::Duration::from_millis(400) {
                self.touch_index();
                last = std::time::Instant::now();
            }
        };
        for (path, sig) in &files {
            if worker::shutting_down() || self.take_wake() {
                self.wake_indexer();
                complete = false;
                break;
            }
            tick(self.probes.refresh_stamp(path, *sig));
        }
        self.touch_index();
        if complete {
            self.fill_audits(&files, !snap.incomplete);
        }
        self.busy.store(false, Ordering::SeqCst);
        self.touch_index();
        complete
    }

    /// Queue the full audit of `paths` again (every MKV when `None`), the deep decode
    /// included while it is on. Returns how many were queued.
    pub fn reaudit(&self, paths: Option<&[PathBuf]>) -> usize {
        let n = match paths {
            Some(ps) => self.audits.reaudit(ps),
            None => {
                let all: Vec<PathBuf> = self.mkv_files().into_iter().map(|(p, _)| p).collect();
                self.audits.reaudit(&all)
            }
        };
        self.touch_index();
        n
    }

    /// A remux just wrote `target`: record its stamp and fold it into the
    /// snapshot so its row is current before the next scan.
    pub fn note_landed(&self, target: &Path, writing_app: Option<String>) {
        let Some(sig) = FileSig::stat(target) else {
            self.wake_indexer();
            return;
        };
        self.probes.record_at(target, sig, writing_app);
        // Edited under the write lock, so a scan finishing now cannot lose it.
        {
            let mut guard = self.index.write().unwrap_or_else(|e| e.into_inner());
            let mut next = (**guard).clone();
            next.sigs.insert(target.to_path_buf(), sig);
            for row in next.rows.iter_mut() {
                if row.target.as_deref() == Some(target) && row.kind == RowKind::IsoOnly {
                    row.kind = RowKind::Remux;
                    row.mkv = Some(target.to_path_buf());
                }
            }
            if !next.mkvs.iter().any(|m| m.path == target)
                && let Some(dirs) = &next.dirs
            {
                let title = index::mkv_title(&dirs.library, target);
                next.mkvs.push(index::MkvFile {
                    path: target.to_path_buf(),
                    title,
                });
            }
            *guard = Arc::new(next);
        }
        self.audits.enqueue([target.to_path_buf()]);
        self.touch_index();
    }

    /// The cross-list from memory: no filesystem access, ever.
    pub fn listing(&self, d: &Dirs) -> Listing {
        let snap = self.snapshot();
        if snap.dirs.as_ref() != Some(d) {
            self.wake_indexer();
            return Listing {
                rows: Vec::new(),
                incomplete: false,
                scanning: true,
                scanned_at: None,
                scan_ms: 0,
                probing: 0,
                auditing: 0,
            };
        }
        let q = self.queue.snapshot();
        let running = probe::running_version();
        let (mut probing, mut auditing) = (0, 0);
        let deep_on = self.deep_enabled();
        let queued = self.audits.queued_set();
        let running_audit = self.audits.status().running.map(|l| l.path);
        let rows = snap
            .rows
            .iter()
            .cloned()
            .map(|r| {
                let sig = r.mkv.as_ref().and_then(|m| snap.sigs.get(m).copied());
                let stamp = r
                    .mkv
                    .as_deref()
                    .zip(sig)
                    .and_then(|(m, s)| self.probes.cached_stamp(m, s));
                let probed = r.mkv.is_none() || stamp.is_some();
                probing += usize::from(!probed);
                let writing_app = stamp.flatten();
                let muxed_with = MuxedWith::from_app(writing_app.as_deref(), running);
                let audit = r
                    .mkv
                    .as_deref()
                    .zip(sig)
                    .and_then(|(m, s)| self.audits.report(m, s));
                let deep = r
                    .mkv
                    .as_deref()
                    .zip(sig)
                    .filter(|_| audit.is_some())
                    .and_then(|(m, s)| self.audits.deep_view(m, s, deep_on));
                let audit_queued = r.mkv.as_ref().is_some_and(|m| queued.contains(m));
                let audit_running = r.mkv.is_some() && r.mkv == running_audit;
                auditing += usize::from(audit_queued || audit_running);
                let needs_remux =
                    r.remuxable() && (r.mkv.is_none() || (probed && muxed_with.out_of_date()));
                let (job, result) = match &r.target {
                    Some(t) => (
                        q.latest_for(t).cloned(),
                        q.results.get(&*t.to_string_lossy()).cloned(),
                    ),
                    None => (None, None),
                };
                RowView {
                    title: r.title,
                    key: r.key,
                    kind: r.kind,
                    note: r.note,
                    linked: r.linked,
                    mkv: r.mkv,
                    iso: r.iso,
                    target: r.target,
                    size_bytes: sig.map(|s| s.size),
                    modified: sig.map(|s| (s.mtime_ns.max(0) / 1_000_000_000) as u64),
                    muxed_label: writing_app.as_deref().map(probe::short_label),
                    writing_app,
                    muxed_with,
                    probed,
                    needs_remux,
                    audit,
                    deep,
                    audit_queued,
                    audit_running,
                    job,
                    result,
                }
            })
            .collect();
        Listing {
            rows,
            incomplete: snap.incomplete,
            scanning: false,
            scanned_at: snap.scanned_at,
            scan_ms: snap.scan_ms,
            probing,
            auditing,
        }
    }

    /// Why queueing must wait, if it must. `creates` says whether the jobs
    /// would write new MKVs (rather than replace ones that exist): an empty
    /// library beside a full ISO folder looks like an unmounted share, and a
    /// new MKV would then land on the local disk under the mountpoint.
    pub fn queue_block(&self, d: &Dirs, creates: bool) -> Option<String> {
        if crate::server::health::folder_ok(&d.library) == Some(false) {
            return Some(format!(
                "The library folder {} is not answering (see System). Nothing was queued.",
                d.library.display()
            ));
        }
        let snap = self.snapshot();
        if snap.incomplete {
            return Some(
                "A library or ISO folder could not be fully read on the last scan. Nothing was queued; it will be once a full scan succeeds."
                    .into(),
            );
        }
        if creates && snap.library_empty && !snap.isos.is_empty() {
            return Some(format!(
                "The library folder {} is empty while the ISO folder is not. If the library share is not mounted, new MKVs would be written to the local disk. Nothing was queued; if the library really is empty, create any folder in it first.",
                d.library.display()
            ));
        }
        None
    }

    /// Stop everything: cancel the running remux (its partial is deleted and
    /// the old MKV kept) and drop every queued job. Leaves the queue unpaused.
    /// Returns whether a job was running and how many queued ones went.
    pub fn stop_all(&self) -> (bool, usize) {
        let removed = self.queue.clear_queued();
        let running = self.queue.snapshot().running().map(|j| j.id);
        if let Some(id) = running {
            self.cancel_job.store(id, Ordering::SeqCst);
        }
        (running.is_some(), removed)
    }

    /// Whether "Stop all" cancelled job `id`.
    pub fn cancelled(&self, id: u64) -> bool {
        id != 0 && self.cancel_job.load(Ordering::SeqCst) == id
    }

    /// Delete `.mkv.partial` files under the library's title folders that no
    /// running job owns: leftovers from a crash or a failed or cleared job.
    /// Returns how many went.
    pub fn sweep_partials(&self, d: &Dirs) -> usize {
        let owned = self
            .queue
            .snapshot()
            .running()
            .map(|j| queue::partial_path(&j.target));
        let Ok(titles) = std::fs::read_dir(&d.library) else {
            return 0;
        };
        let mut n = 0;
        for dir in titles.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            for f in files.flatten().map(|e| e.path()) {
                let partial = f
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.to_ascii_lowercase().ends_with(".mkv.partial"));
                if partial
                    && owned.as_deref() != Some(f.as_path())
                    && std::fs::remove_file(&f).is_ok()
                {
                    tracing::info!(path = %f.display(), "removed an orphaned remux partial");
                    n += 1;
                }
            }
        }
        n
    }

    /// Queue the remuxable rows `pick` selects, from memory. Returns how many
    /// were added.
    pub fn enqueue(&self, d: &Dirs, pick: impl Fn(&RowView) -> bool) -> usize {
        let jobs: Vec<NewJob> = self
            .listing(d)
            .rows
            .into_iter()
            .filter(|r| r.kind == RowKind::Remux || r.kind == RowKind::IsoOnly)
            .filter(|r| pick(r))
            .filter_map(|r| {
                Some(NewJob {
                    title: r.title,
                    iso: r.iso?,
                    replace: r.mkv.is_some(),
                    target: r.target?,
                })
            })
            .collect();
        self.queue.add(jobs)
    }

    /// Per-title log file. The title is reduced to one safe path segment.
    pub fn log_path(&self, title: &str) -> PathBuf {
        self.log_dir
            .join(format!("{}.log", index::safe_segment(title)))
    }

    /// Moves when the queue, the running job or the console changes.
    pub fn generation(&self) -> (u64, u64) {
        (
            self.queue.generation(),
            self.live_generation.load(Ordering::SeqCst),
        )
    }

    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn touch_live(&self) {
        self.live_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Append a console line for `job`.
    pub fn console(&self, job: u64, kind: LineKind, text: impl Into<String>) {
        {
            let mut l = self.live();
            l.next_seq += 1;
            l.job = job;
            let line = ConsoleLine {
                seq: l.next_seq,
                ts: crate::server::util::epoch_secs(),
                job,
                kind,
                text: text.into(),
            };
            if l.lines.len() == CONSOLE_LINES {
                l.lines.pop_front();
            }
            l.lines.push_back(line);
        }
        self.touch_live();
    }

    /// The newest console line's sequence number (0 when empty).
    pub fn last_seq(&self) -> u64 {
        self.live().lines.back().map_or(0, |l| l.seq)
    }

    /// Console lines after `seq`, oldest first.
    pub fn console_since(&self, seq: u64) -> Vec<ConsoleLine> {
        self.live()
            .lines
            .iter()
            .filter(|l| l.seq > seq)
            .cloned()
            .collect()
    }

    /// The job the console follows (running, else the last one), its title
    /// and its lines.
    pub fn console_job(&self) -> (u64, String, Vec<ConsoleLine>) {
        let l = self.live();
        let job = l.running.as_ref().map_or(l.job, |r| r.job_id);
        (
            job,
            l.job_title.clone(),
            l.lines.iter().filter(|x| x.job == job).cloned().collect(),
        )
    }

    /// The title of the job the console follows.
    pub fn job_title(&self) -> String {
        self.live().job_title.clone()
    }

    pub fn running(&self) -> Option<Running> {
        self.live().running.clone()
    }

    fn set_running(&self, f: impl FnOnce(&mut Option<Running>)) {
        {
            let mut l = self.live();
            f(&mut l.running);
            if let Some((id, title)) = l.running.as_ref().map(|r| (r.job_id, r.title.clone())) {
                l.job = id;
                l.job_title = title;
            }
        }
        self.touch_live();
    }
}

// A file a scan did not list is gone only when a stat says NotFound.
fn gone(p: &Path) -> bool {
    matches!(std::fs::metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Read a per-title log written by the worker: `ts<TAB>kind<TAB>text` lines.
pub fn parse_log(text: &str) -> Vec<ConsoleLine> {
    text.lines()
        .enumerate()
        .map(|(i, l)| {
            let mut parts = l.splitn(3, '\t');
            let (ts, kind, body) = (parts.next(), parts.next(), parts.next());
            match (ts.and_then(|t| t.parse().ok()), kind, body) {
                (Some(ts), Some(kind), Some(body)) => ConsoleLine {
                    seq: i as u64 + 1,
                    ts,
                    job: 0,
                    kind: LineKind::from_tag(kind),
                    text: body.to_string(),
                },
                _ => ConsoleLine {
                    seq: i as u64 + 1,
                    ts: 0,
                    job: 0,
                    kind: LineKind::Out,
                    text: l.to_string(),
                },
            }
        })
        .collect()
}

/// One log line in the per-title log format [`parse_log`] reads.
pub fn log_line(kind: LineKind, text: &str) -> String {
    format!(
        "{}\t{}\t{}",
        crate::server::util::epoch_secs(),
        kind.tag(),
        text.replace(['\n', '\r'], " ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_fall_back_to_where_rips_are_filed() {
        let mut c = Config {
            output_dir: "/media".into(),
            ..Config::default()
        };
        let d = dirs(&c);
        assert_eq!(d.library, PathBuf::from("/media"));
        assert_eq!(d.isos, None);
        assert!(!d.iso_subfolders);
        c.movie_dir = "movies".into();
        c.iso_dir = "isos".into();
        let d = dirs(&c);
        assert_eq!(d.library, PathBuf::from("/media/movies"));
        assert_eq!(d.isos, Some(PathBuf::from("/media/isos")));
        c.library_dir = "/lib".into();
        c.library_iso_dir = "/src".into();
        c.library_iso_subfolders = true;
        assert_eq!(
            dirs(&c),
            Dirs {
                library: "/lib".into(),
                isos: Some("/src".into()),
                iso_subfolders: true
            }
        );
    }

    fn dirs_in(t: &Path) -> Dirs {
        Dirs {
            library: t.join("lib"),
            isos: Some(t.join("isos")),
            iso_subfolders: false,
        }
    }

    #[test]
    fn an_empty_library_beside_isos_blocks_new_mkvs() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs_in(t.path());
        std::fs::create_dir_all(&d.library).unwrap();
        std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
        std::fs::write(d.isos.as_ref().unwrap().join("A (2000).iso"), b"x").unwrap();
        let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
        lib.index_now(&d);
        assert!(lib.queue_block(&d, true).unwrap().contains("empty"));
        assert_eq!(
            lib.queue_block(&d, false),
            None,
            "replacing is not creating"
        );
        std::fs::create_dir(d.library.join("Something")).unwrap();
        lib.index_now(&d);
        assert_eq!(lib.queue_block(&d, true), None);
    }

    #[test]
    fn orphaned_partials_are_swept_but_the_running_one_is_kept() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs_in(t.path());
        let a = d.library.join("A");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::write(a.join("A.mkv.partial"), b"half").unwrap();
        std::fs::write(a.join("A.mkv"), b"whole").unwrap();
        let b = d.library.join("B");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(b.join("B.mkv.partial"), b"running").unwrap();
        let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
        lib.queue.add(vec![queue::NewJob {
            title: "B".into(),
            iso: "/i/B.iso".into(),
            target: b.join("B.mkv"),
            replace: false,
        }]);
        lib.queue.claim_next().unwrap();
        assert_eq!(lib.sweep_partials(&d), 1);
        assert!(!a.join("A.mkv.partial").exists());
        assert!(a.join("A.mkv").exists());
        assert!(
            b.join("B.mkv.partial").exists(),
            "the running job's partial stays"
        );
    }

    #[test]
    fn the_console_is_a_bounded_ring() {
        let t = tempfile::tempdir().unwrap();
        let lib = Library::open(t.path(), t.path());
        for i in 0..(CONSOLE_LINES + 5) {
            lib.console(1, LineKind::Out, format!("line {i}"));
        }
        let all = lib.console_since(0);
        assert_eq!(all.len(), CONSOLE_LINES);
        assert_eq!(all[0].text, "line 5");
        let tail = lib.console_since(all[all.len() - 2].seq);
        assert_eq!(tail.len(), 1);
    }

    #[test]
    fn a_log_name_cannot_leave_the_log_folder() {
        let t = tempfile::tempdir().unwrap();
        let lib = Library::open(t.path(), &t.path().join("logs"));
        let p = lib.log_path("../../etc/passwd");
        assert_eq!(p.parent(), Some(t.path().join("logs").as_path()));
    }
}
