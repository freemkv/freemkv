//! The Library: every MKV and source ISO, which freemkv muxed each MKV, and a
//! queue that remuxes out-of-date titles from their ISO through the engine.
//!
//! [`index`] builds the cross-list, [`probe`] reads the muxed-with stamp and
//! runs the fast audit ([`media`] reads what each MKV carries), [`queue`] persists the jobs, [`worker`] runs them one
//! at a time, [`arbiter`] gives rips the mux slot first, [`audit`] queues each MKV's
//! audit ([`deep`] decodes it in full when the setting is on), and [`api`] serves `/api/library*`.

pub mod api;
pub mod arbiter;
pub mod audit;
pub mod deep;
pub mod index;
pub mod links;
pub mod media;
pub mod probe;
pub mod queue;
pub mod transcript;
pub mod worker;

use crate::server::config::Config;
use crate::server::health;
use index::{Row, RowKind, RowNote};
use probe::{AuditReport, FileSig, MuxedWith, ProbeCache};
use queue::{Job, JobResult, NewJob, Queue};
use std::collections::{HashMap, HashSet, VecDeque};
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

/// Why the remux queue holds its next job: a folder it needs failed its check.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Hold {
    /// "output", "source ISO" or "remux staging".
    pub role: &'static str,
    pub path: PathBuf,
    pub state: health::State,
    pub message: String,
    pub hint: String,
    pub since: u64,
    // Set when a preflight (not the background check) failed: not before this is it tried again.
    #[serde(skip)]
    recheck_at: Option<u64>,
}

impl Hold {
    fn from_problem(p: &health::Problem, now: u64) -> Self {
        Hold {
            role: p.role,
            path: p.path.clone(),
            state: p.fault.state(),
            message: p.message.clone(),
            hint: p.hint.clone(),
            since: now,
            recheck_at: Some(now + health::INTERVAL_SECS),
        }
    }
}

/// One folder the Library depends on, as its pages show it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct FolderView {
    /// "output", "source ISO" or "remux staging".
    pub role: &'static str,
    pub path: PathBuf,
    /// The last check of the folder (or of the configured folder that holds it).
    pub health: Option<health::Mount>,
    /// While it is not ok: what is wrong, named by `role`, and the fix.
    pub message: Option<String>,
    pub hint: Option<String>,
}

impl FolderView {
    pub fn ok(&self) -> bool {
        self.health
            .as_ref()
            .is_none_or(|m| m.state == health::State::Ok)
    }
}

/// The folders a remux uses: where MKVs land, where the ISOs are, and the local staging.
pub fn folder_views(d: &Dirs) -> Vec<FolderView> {
    let mut out = vec![("output", d.library.clone())];
    out.extend(d.isos.clone().map(|p| ("source ISO", p)));
    out.extend(health::remux_stage_dir().map(|p| ("remux staging", p)));
    out.into_iter()
        .map(|(role, path)| {
            let health = health::status_for(&path);
            let fault = health
                .as_ref()
                .filter(|m| m.state != health::State::Ok)
                .map(|m| m.fault.unwrap_or(health::Fault::Other));
            FolderView {
                role,
                message: fault.map(|f| f.message(role)),
                hint: fault.map(|f| f.hint().to_string()),
                health,
                path,
            }
        })
        .collect()
}

// The first folder of `views` whose last check failed, as a hold.
fn folder_gate(views: &[FolderView]) -> Option<Hold> {
    let v = views.iter().find(|v| !v.ok())?;
    let m = v.health.as_ref()?;
    Some(Hold {
        role: v.role,
        path: v.path.clone(),
        state: m.state,
        message: v.message.clone().unwrap_or_default(),
        hint: v.hint.clone().unwrap_or_default(),
        since: m.since,
        recheck_at: None,
    })
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

impl Snapshot {
    /// A finished scan of `d` that read every folder and found the library populated:
    /// only then does an MKV missing from `mkvs` mean the file is gone. An empty
    /// library may be an unmounted share.
    fn lists_everything(&self, d: &Dirs) -> bool {
        self.dirs.as_ref() == Some(d)
            && self.scanned_at.is_some()
            && !self.incomplete
            && !self.library_empty
    }
}

/// The Library's state: the queue, the probe cache, the index and the console.
pub struct Library {
    pub queue: Queue,
    pub probes: ProbeCache,
    pub audits: audit::Audits,
    // The deep_audit setting as the quick-lane loop last read it.
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
    // Counts `note_landed` edits, so a scan that began before one is not swapped in over it.
    landed: AtomicU64,
    index_generation: AtomicU64,
    // The indexer sleeps on this; `wake` sets it to rescan now.
    wake: (Mutex<bool>, Condvar),
    busy: AtomicBool,
    hold: Mutex<Option<Hold>>,
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

/// Start the remux worker, the background indexer and both audit lanes.
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
    {
        let (lib, cfg) = (lib.clone(), cfg.clone());
        let _ = std::thread::Builder::new()
            .name("library-deep-audit".into())
            .spawn(move || worker::deep_audit_loop(&lib, &cfg, &arbiter::ARBITER));
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
            landed: AtomicU64::new(0),
            index_generation: AtomicU64::new(0),
            wake: (Mutex::new(false), Condvar::new()),
            busy: AtomicBool::new(false),
            hold: Mutex::new(None),
        }
    }

    /// The last scan. Cheap: an `Arc` clone under a read lock.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.index.read().unwrap_or_else(|e| e.into_inner()).clone()
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

    /// Turning deep audit off empties the deep lane; turning it on is followed by a refill.
    pub(crate) fn set_deep_enabled(&self, on: bool) {
        if !on {
            self.audits.clear_deep();
        }
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
    /// lock is held across it. Only a NotFound drops a file a scan missed. A remux
    /// that lands mid-scan discards it and asks for another.
    pub fn rescan(&self, d: &Dirs) {
        let landed_at = self.landed.load(Ordering::SeqCst);
        let scan = self.scan(d);
        self.commit_scan(scan, landed_at);
    }

    // Swap `scan` in unless a remux landed since `landed_at`: the scan may predate its file.
    fn commit_scan(&self, scan: Snapshot, landed_at: u64) {
        {
            let mut guard = self.index.write().unwrap_or_else(|e| e.into_inner());
            if self.landed.load(Ordering::SeqCst) == landed_at {
                *guard = Arc::new(scan);
                drop(guard);
                self.touch_index();
                return;
            }
        }
        self.wake_indexer();
    }

    fn scan(&self, d: &Dirs) -> Snapshot {
        let started = std::time::Instant::now();
        let mut mkvs = index::list_mkvs(&d.library);
        let mut isos = match &d.isos {
            Some(dir) => index::list_isos(dir, d.iso_subfolders),
            None => index::Listing::default(),
        };
        let prev = self.snapshot();
        let same = prev.dirs.as_ref() == Some(d);
        if same && mkvs.incomplete {
            let seen: HashSet<&Path> = mkvs.files.iter().map(|f| f.path.as_path()).collect();
            let missed: Vec<_> = prev
                .mkvs
                .iter()
                .filter(|m| !seen.contains(m.path.as_path()) && !gone(&m.path))
                .cloned()
                .collect();
            mkvs.files.extend(missed);
        }
        if same && isos.incomplete {
            let seen: HashSet<&Path> = isos.files.iter().map(|f| f.path.as_path()).collect();
            let missed: Vec<_> = prev
                .isos
                .iter()
                .filter(|i| !seen.contains(i.path.as_path()) && !gone(&i.path))
                .cloned()
                .collect();
            isos.files.extend(missed);
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
        Snapshot {
            library_empty,
            dirs: Some(d.clone()),
            mkvs: mkvs.files,
            isos: isos.files,
            rows,
            sigs,
            incomplete: mkvs.incomplete || isos.incomplete,
            scanned_at: Some(crate::server::util::epoch_secs()),
            scan_ms: started.elapsed().as_millis() as u64,
        }
    }

    /// Queue the audits `files` still needs (new, changed, never audited, or a full decode
    /// owed while deep audit is on). Never drops a stored verdict.
    pub(crate) fn fill_audits(&self, files: &[(PathBuf, FileSig)]) {
        let now = crate::server::util::epoch_secs();
        if self.audits.fill(files, self.deep_enabled(), now, false) > 0 {
            self.touch_index();
        }
    }

    /// The audit worker's periodic refill from the last scan. Queues only: before the
    /// first scan the snapshot is empty, which proves nothing about the library.
    pub(crate) fn refill_audits(&self) {
        self.fill_audits(&self.mkv_files());
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
        let files = self.mkv_files();
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
            // The list as of now: a remux may have landed during the header pass.
            let snap = self.snapshot();
            let files = self.mkv_files();
            self.fill_audits(&files);
            if snap.lists_everything(d) {
                let present: HashSet<PathBuf> = snap.mkvs.iter().map(|m| m.path.clone()).collect();
                self.audits.prune(&present);
                self.probes.retain(&files);
            }
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
        // Edited under the write lock; a scan that began earlier is dropped (see `commit_scan`).
        {
            let mut guard = self.index.write().unwrap_or_else(|e| e.into_inner());
            self.landed.fetch_add(1, Ordering::SeqCst);
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
        let mut latest: HashMap<&Path, &Job> = HashMap::new();
        for j in &q.jobs {
            latest.insert(j.target.as_path(), j);
        }
        let running = probe::running_version();
        let (mut probing, mut auditing) = (0, 0);
        let deep_on = self.deep_enabled();
        let queued = self.audits.queued_set();
        let running_audits = self.audits.running_set();
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
                    .and_then(|(m, s)| self.audits.report(m, s))
                    .map(without_raw);
                let deep = r
                    .mkv
                    .as_deref()
                    .zip(sig)
                    .filter(|_| audit.is_some())
                    .and_then(|(m, s)| self.audits.deep_view(m, s, deep_on));
                let audit_queued = r.mkv.as_ref().is_some_and(|m| queued.contains(m));
                let audit_running = r.mkv.as_ref().is_some_and(|m| running_audits.contains(m));
                auditing += usize::from(audit_queued || audit_running);
                let needs_remux =
                    r.remuxable() && (r.mkv.is_none() || (probed && muxed_with.out_of_date()));
                let (job, result) = match &r.target {
                    Some(t) => (
                        latest.get(t.as_path()).map(|j| (*j).clone()),
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
        if let Some(h) = folder_gate(&folder_views(d)) {
            return Some(format!(
                "{} ({}) {} Nothing was queued.",
                h.message,
                h.path.display(),
                h.hint
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

    /// Why the queue is holding its next job, if it is.
    pub fn hold(&self) -> Option<Hold> {
        self.hold.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    // A hold on the same folder keeps the time it began; any change reaches the stream.
    pub(crate) fn set_hold(&self, next: Option<Hold>) {
        let changed = {
            let mut h = self.hold.lock().unwrap_or_else(|e| e.into_inner());
            let next = next.map(|mut n| {
                if let Some(old) = h.as_ref().filter(|o| o.path == n.path) {
                    n.since = n.since.min(old.since);
                }
                n
            });
            let changed = *h != next;
            *h = next;
            changed
        };
        if changed {
            self.touch_live();
        }
    }

    /// Whether the next remux must wait for a folder at `now`: one failed its last
    /// check, or a preflight failed and its recheck time has not come.
    pub(crate) fn blocked(&self, d: &Dirs, now: u64) -> Option<Hold> {
        if let Some(h) = folder_gate(&folder_views(d)) {
            return Some(h);
        }
        self.hold()
            .filter(|h| h.recheck_at.is_some_and(|t| now < t))
    }

    /// Hold the queue for the preflight failure `p`.
    pub(crate) fn hold_for(&self, p: &health::Problem, now: u64) {
        self.set_hold(Some(Hold::from_problem(p, now)));
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
// The raw header report is served on its own (`/api/library/raw`), not in every listing.
fn without_raw(mut a: AuditReport) -> AuditReport {
    if let Some(d) = a.detail.as_mut() {
        d.raw.clear();
    }
    a
}

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
    fn a_scan_that_began_before_a_remux_landed_does_not_undo_it() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs_in(t.path());
        std::fs::create_dir_all(d.library.join("Other")).unwrap();
        std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
        std::fs::write(d.isos.as_ref().unwrap().join("A (2000).iso"), b"x").unwrap();
        let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
        lib.index_now(&d);
        let landed_at = lib.landed.load(Ordering::SeqCst);
        let scan = lib.scan(&d);
        let target = d.library.join("A (2000)/A (2000).mkv");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(
            &target,
            probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true),
        )
        .unwrap();
        lib.note_landed(&target, None);
        lib.commit_scan(scan, landed_at);
        let snap = lib.snapshot();
        assert!(snap.sigs.contains_key(&target), "the landed sig survives");
        assert!(snap.rows.iter().any(|r| r.mkv.as_ref() == Some(&target)));
    }

    #[test]
    fn an_empty_library_folder_does_not_erase_the_audits() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs_in(t.path());
        let mkv = d.library.join("A/A.mkv");
        std::fs::create_dir_all(mkv.parent().unwrap()).unwrap();
        std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
        std::fs::write(
            &mkv,
            probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true),
        )
        .unwrap();
        let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
        lib.index_now(&d);
        let sig = FileSig::stat(&mkv).unwrap();
        let report = probe::audit_fast(&mkv).unwrap();
        lib.audits.record_fast(&mkv, sig, report, 1);
        std::fs::remove_dir_all(d.library.join("A")).unwrap();
        lib.index_now(&d);
        assert!(
            lib.audits.report(&mkv, sig).is_some(),
            "an empty folder may be an unmounted share"
        );
    }

    #[test]
    fn a_restart_keeps_every_stored_audit_until_a_file_is_really_gone() {
        let t = tempfile::tempdir().unwrap();
        let d = Dirs {
            library: t.path().join("media/movies"),
            isos: Some(t.path().join("media/iso")),
            iso_subfolders: false,
        };
        let cfg = t.path().join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
        std::fs::write(
            d.isos.as_ref().unwrap().join("2 Fast 2 Furious (2003).iso"),
            b"x",
        )
        .unwrap();
        let bytes = probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true);
        let names = [
            "2 Fast 2 Furious (2003)/2 Fast 2 Furious (2003).mkv",
            "Constantine (2005)/Constantine (2005).mkv",
            "Dune (1984)/Dune (1984).mkv",
            "Dune (2021)/Dune (2021).mkv",
            // Two cuts, neither the feature: an ambiguous row with no MKV of its own.
            "Alien (1979)/Alien Theatrical.mkv",
            "Alien (1979)/Alien Director.mkv",
        ];
        let mut stored = Vec::new();
        {
            let audits = audit::Audits::open(&cfg);
            for n in names {
                let p = d.library.join(n);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, &bytes).unwrap();
                let sig = FileSig::stat(&p).unwrap();
                audits.record_fast(&p, sig, probe::audit_fast(&p).unwrap(), 1);
                stored.push((p, sig));
            }
        }
        let lib = Library::open(&cfg, &t.path().join("logs"));
        lib.set_deep_enabled(true);
        // The audit worker's first refill can run before the indexer's first scan.
        lib.refill_audits();
        lib.index_now(&d);
        lib.refill_audits();
        lib.index_now(&d);
        let reopened = audit::Audits::open(&cfg);
        for (p, sig) in &stored {
            assert!(
                reopened.report(p, *sig).is_some(),
                "{} kept its audit",
                p.display()
            );
        }
        let (gone, gone_sig) = stored[1].clone();
        std::fs::remove_file(&gone).unwrap();
        lib.index_now(&d);
        assert!(
            lib.audits.report(&gone, gone_sig).is_none(),
            "a deleted file loses it"
        );
        assert!(lib.audits.report(&stored[0].0, stored[0].1).is_some());
        assert!(lib.audits.report(&stored[4].0, stored[4].1).is_some());
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

    #[cfg(unix)]
    #[test]
    fn an_incomplete_scan_keeps_what_it_could_not_list_unless_it_is_gone() {
        use std::os::unix::fs::PermissionsExt as _;
        let t = tempfile::tempdir().unwrap();
        let d = dirs_in(t.path());
        std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
        let bytes = probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true);
        for name in ["A", "B", "C"] {
            let dir = d.library.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{name}.mkv")), &bytes).unwrap();
        }
        let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
        lib.index_now(&d);
        let (b, c) = (d.library.join("B"), d.library.join("C"));
        let lock = |p: &Path, mode| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        lock(&b, 0o0);
        if std::fs::read_dir(&b).is_ok() {
            lock(&b, 0o755);
            return; // root reads anything
        }
        lib.rescan(&d);
        let snap = lib.snapshot();
        assert!(snap.incomplete);
        let b_mkv = b.join("B.mkv");
        assert!(
            snap.mkvs.iter().any(|m| m.path == b_mkv),
            "kept from before"
        );
        assert!(snap.sigs.contains_key(&b_mkv), "with its earlier signature");
        std::fs::remove_dir_all(&c).unwrap();
        lib.rescan(&d);
        let snap = lib.snapshot();
        lock(&b, 0o755);
        assert!(snap.mkvs.iter().any(|m| m.path == b_mkv));
        assert!(
            !snap.mkvs.iter().any(|m| m.path == c.join("C.mkv")),
            "NotFound is gone"
        );
    }

    #[test]
    fn a_landed_remux_turns_its_iso_only_row_into_a_remux_row_and_queues_its_audit() {
        let t = tempfile::tempdir().unwrap();
        let d = dirs_in(t.path());
        std::fs::create_dir_all(d.library.join("Other")).unwrap();
        std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
        std::fs::write(d.isos.as_ref().unwrap().join("A (2000).iso"), b"x").unwrap();
        let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
        lib.index_now(&d);
        let target = d.library.join("A (2000)/A (2000).mkv");
        let row = |lib: &Library| {
            lib.snapshot()
                .rows
                .iter()
                .find(|r| r.target.as_ref() == Some(&target))
                .cloned()
                .unwrap()
        };
        assert_eq!(row(&lib).kind, RowKind::IsoOnly);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(
            &target,
            probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true),
        )
        .unwrap();
        lib.note_landed(&target, Some("freemkv 1.0.0".into()));
        let r = row(&lib);
        assert_eq!((r.kind, r.mkv.as_ref()), (RowKind::Remux, Some(&target)));
        let snap = lib.snapshot();
        assert!(
            snap.mkvs
                .iter()
                .any(|m| m.path == target && m.title == "A (2000)")
        );
        assert!(lib.audits.is_queued(&target));
        let sig = FileSig::stat(&target).unwrap();
        assert_eq!(
            lib.probes.cached_stamp(&target, sig),
            Some(Some("freemkv 1.0.0".into()))
        );
    }

    #[test]
    fn the_title_log_round_trips_and_reads_old_lines() {
        let line = log_line(LineKind::Warn, "two\nlines\rhere");
        assert_eq!(line.matches('\n').count(), 0);
        let text = format!(
            "{line}\nplain legacy line\n5\tmystery\tbody\n{}",
            log_line(LineKind::Err, "tab\tinside")
        );
        let lines = parse_log(&text);
        assert_eq!(lines.len(), 4);
        assert_eq!(
            (lines[0].kind, lines[0].text.as_str()),
            (LineKind::Warn, "two lines here")
        );
        assert!(lines[0].ts > 0);
        assert_eq!(
            (lines[1].kind, lines[1].ts, lines[1].text.as_str()),
            (LineKind::Out, 0, "plain legacy line")
        );
        assert_eq!(
            (lines[2].kind, lines[2].ts, lines[2].text.as_str()),
            (LineKind::Out, 5, "body")
        );
        assert_eq!(
            (lines[3].kind, lines[3].text.as_str()),
            (LineKind::Err, "tab\tinside")
        );
        assert_eq!(lines[3].seq, 4);
    }
}
