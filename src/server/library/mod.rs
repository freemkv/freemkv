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
mod deliver;
pub mod index;
pub mod links;
pub mod matches;
pub mod media;
mod owned;
pub mod probe;
pub mod queue;
mod retire;
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
use std::sync::{Arc, Condvar, Mutex, RwLock};

/// Where the Library looks, resolved from the settings.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Dirs {
    pub library: PathBuf,
    /// Additional TV output root; never an ISO-only row's default movie target.
    pub tv: Option<PathBuf>,
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
    let tv = under_output(&cfg.output_dir, &cfg.tv_dir);
    let tv = (!cfg.output_dir.is_empty() && tv != library).then_some(tv);
    Dirs {
        library,
        tv,
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
    pub stopping: bool,
    /// The progress line a terminal would keep rewriting in place.
    pub line: String,
}

const CONSOLE_LINES: usize = 2000;
/// How long the Change match dialog waits on the share before listing from the links alone.
const MATCH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Default)]
struct Live {
    lines: VecDeque<ConsoleLine>,
    next_seq: u64,
    running: Option<Running>,
    // The job whose lines the console shows: the running one, else the last.
    job: u64,
    job_title: String,
}

/// Why the remux queue holds its next job: a folder it needs failed its check, or the queue
/// file could not be loaded.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Hold {
    /// The folder's role ("output", [`TV_ROLE`], "source ISO", "remux staging"), or
    /// [`QUEUE_FILE_ROLE`].
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

// What one root's Change-match check is given: that root's links, the ISO, and the root.
type CaptureInput = (HashMap<PathBuf, links::Link>, PathBuf, Vec<PathBuf>);

// How a matched ISO is stamped; tests stand in a hung or a watched share.
type IsoStat = Arc<dyn Fn(&Path) -> Option<crate::server::util::Stamp> + Send + Sync>;

fn iso_stamp_within(
    d: &Dirs,
    iso: &Path,
    limit: std::time::Duration,
    stat: impl FnOnce(&Path) -> Option<crate::server::util::Stamp> + Send + 'static,
) -> std::io::Result<Option<crate::server::util::Stamp>> {
    let key = d
        .isos
        .clone()
        .filter(|root| iso.starts_with(root))
        .or_else(|| iso.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| iso.to_path_buf());
    let owned = iso.to_path_buf();
    match health::bounded(&key, limit, move || stat(&owned)) {
        health::Bounded::Done(stamp) => Ok(stamp),
        health::Bounded::TimedOut | health::Bounded::Busy => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            crate::strings::fmt_or(
                "server.library.nothing_queued",
                "{reason} Nothing was queued.",
                &[(
                    "reason",
                    &crate::strings::fmt_or(
                        "server.library.iso_unresponsive",
                        "The source ISO folder {path} is not responding — the network share may be down or hung.",
                        &[("path", &key.display().to_string())],
                    ),
                )],
            ),
        )),
    }
}

fn current_for(snap: &Snapshot, source: &Path, saved: &matches::SavedMatch) -> bool {
    snap.iso_stamps
        .get(source)
        .is_none_or(|now| saved.is_for(*now))
}

// How long a saved-matches file that could not be read waits before it is read again.
const MATCHES_RETRY_SECS: u64 = 60;

/// The role of a separate TV folder among [`folder_views`].
pub(crate) const TV_ROLE: &str = "TV output";
/// The hold's role while the queue file could not be loaded; it is a file, not a folder.
pub(crate) const QUEUE_FILE_ROLE: &str = "queue file";

/// The folders a remux uses: where MKVs land (and TV episodes, when that folder is separate),
/// where the ISOs are, and the local staging.
pub fn folder_views(d: &Dirs) -> Vec<FolderView> {
    let mut out = vec![("output", d.library.clone())];
    out.extend(d.tv.clone().map(|p| (TV_ROLE, p)));
    out.extend(d.isos.clone().map(|p| ("source ISO", p)));
    out.extend(health::remux_stage_dir().map(|p| ("remux staging", p)));
    out.into_iter()
        .map(|(role, path)| {
            let health = health::status_for(&path);
            // A folder whose check is still out is not usable yet, but has not failed.
            let (message, hint) = match health.as_ref() {
                Some(m) if m.state == health::State::Checking => {
                    (Some(health::checking_message(role)), m.hint.clone())
                }
                Some(m) if m.state != health::State::Ok => {
                    let f = m.fault.unwrap_or(health::Fault::Other);
                    (Some(f.message(role)), Some(f.hint().to_string()))
                }
                _ => (None, None),
            };
            FolderView {
                role,
                message,
                hint,
                health,
                path,
            }
        })
        .collect()
}

// The first folder of `views` whose last check failed, as a hold. The TV folder counts only
// with `tv`: a job that files nothing there does not wait for it.
fn folder_gate(views: &[FolderView], tv: bool) -> Option<Hold> {
    let v = views
        .iter()
        .find(|v| (tv || v.role != TV_ROLE) && !v.ok())?;
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
    /// Each ISO with a stamped saved match, as the scan saw it.
    pub iso_stamps: HashMap<PathBuf, Option<crate::server::util::Stamp>>,
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
    // Opened on first good read, never under this lock: listing readers never wait on it.
    source_matches: Mutex<Option<Arc<matches::MatchStore>>>,
    // The last open failure: when (epoch seconds) and its error; tried again a minute later.
    matches_failed: Mutex<Option<(u64, std::io::ErrorKind, String)>>,
    match_gate: Mutex<()>,
    iso_stat: IsoStat,
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
    // Counts `note_landed` edits; a scan that began before one folds it in (`landings`).
    landed: AtomicU64,
    // The latest landings by that count, for scans that began before them.
    landings: Mutex<VecDeque<(u64, PathBuf, FileSig)>>,
    index_generation: AtomicU64,
    // The indexer sleeps on this; `wake` sets it to rescan now.
    wake: (Mutex<bool>, Condvar),
    busy: AtomicBool,
    hold: Mutex<Option<Hold>>,
}

static INSTANCE: Mutex<Option<Arc<Library>>> = Mutex::new(None);

/// The daemon's Library, opened on first use from the settings' config folder.
pub fn instance(cfg: &Config) -> Arc<Library> {
    INSTANCE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(|| {
            Arc::new(Library::open(
                Path::new(&cfg.autorip_dir),
                &Path::new(&cfg.log_dir()).join("library"),
            ))
        })
        .clone()
}

/// The Library if something has opened it.
pub fn get() -> Option<Arc<Library>> {
    INSTANCE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Ask the indexer to rescan now (settings changed, a rip landed, a button).
pub fn wake() {
    if let Some(lib) = get() {
        lib.wake_indexer();
    }
}

/// Start the remux worker, the background indexer and both audit lanes.
/// `None` when the service group thread could not be started; shutdown is then requested.
pub fn start(cfg: &Arc<RwLock<Config>>) -> Option<std::thread::JoinHandle<()>> {
    let lib = instance(&cfg.read().unwrap_or_else(|e| e.into_inner()));
    let mut tasks: Vec<LibraryTask> = Vec::new();
    type Run = fn(&Arc<Library>, &Arc<RwLock<Config>>);
    let workers: [(&str, Run); 4] = [
        ("library-index", worker::index_loop),
        ("library-audit", |l, c| {
            worker::audit_loop(l, c, &arbiter::ARBITER)
        }),
        ("library-deep-audit", |l, c| {
            worker::deep_audit_loop(l, c, &arbiter::ARBITER)
        }),
        ("library-remux", |l, c| worker::run(l, c, &arbiter::ARBITER)),
    ];
    for (name, run) in workers {
        let (lib, cfg) = (lib.clone(), cfg.clone());
        tasks.push((name, Box::new(move || run(&lib, &cfg))));
    }
    start_tasks(tasks)
}

type LibraryTask = (&'static str, Box<dyn FnOnce() + Send>);

fn start_tasks(tasks: Vec<LibraryTask>) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("library-services".into())
        .spawn(move || {
            let mut handles = Vec::new();
            for (name, task) in tasks {
                match std::thread::Builder::new().name(name.into()).spawn(task) {
                    Ok(handle) => handles.push(handle),
                    Err(e) => {
                        tracing::error!(service = name, error = %e, "library service could not start");
                        crate::server::SHUTDOWN.store(true, Ordering::Release);
                        break;
                    }
                }
            }
            for handle in handles {
                let _ = handle.join();
            }
        })
        .inspect_err(|e| {
            tracing::error!(error = %e, "library services could not start");
            crate::server::SHUTDOWN.store(true, Ordering::Release);
        })
        .ok()
}

/// One row of `GET /api/library`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RowView {
    pub title: String,
    pub source_match: Option<matches::SavedMatch>,
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
        let lib = Self {
            queue: Queue::open(config_dir),
            source_matches: Mutex::new(None),
            matches_failed: Mutex::new(None),
            match_gate: Mutex::new(()),
            iso_stat: Arc::new(links::iso_stamp),
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
            landings: Mutex::new(VecDeque::new()),
            index_generation: AtomicU64::new(0),
            wake: (Mutex::new(false), Condvar::new()),
            busy: AtomicBool::new(false),
            hold: Mutex::new(None),
        };
        let _ = lib.match_store();
        lib
    }

    // The ISO's stamp, read under the ISO folder's probe slot within the Change match check's
    // limit: a request thread never waits on a hung share, and is refused instead.
    fn iso_stamp_bounded(
        &self,
        d: &Dirs,
        iso: &Path,
    ) -> std::io::Result<Option<crate::server::util::Stamp>> {
        let stat = self.iso_stat.clone();
        iso_stamp_within(d, iso, MATCH_CHECK_TIMEOUT, move |p| stat(p))
    }

    /// The saved matches, opened now if they are not yet: an unreadable file is an error
    /// that names it, logged once, and read again at most once a minute.
    pub(crate) fn match_store(&self) -> std::io::Result<Arc<matches::MatchStore>> {
        self.match_store_at(crate::server::util::epoch_secs())
    }

    pub(crate) fn match_store_at(&self, now: u64) -> std::io::Result<Arc<matches::MatchStore>> {
        if let Some(store) = self.loaded_store() {
            return Ok(store);
        }
        let failed = |at: u64, kind, why: &str| {
            let message = matches_unreadable(&self.config_dir, why);
            (now < at.saturating_add(MATCHES_RETRY_SECS))
                .then(|| std::io::Error::new(kind, message))
        };
        let last = self
            .matches_failed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some((at, kind, why)) = &last
            && let Some(e) = failed(*at, *kind, why)
        {
            return Err(e);
        }
        match matches::MatchStore::open(&self.config_dir) {
            Ok(store) => {
                if self
                    .matches_failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .is_some()
                {
                    tracing::info!("saved source matches are readable again");
                }
                let mut slot = self
                    .source_matches
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                Ok(slot.get_or_insert_with(|| Arc::new(store)).clone())
            }
            Err(e) => {
                if last.is_none() {
                    tracing::error!(error = %e, dir = %self.config_dir.display(), "library-matches.json cannot be read; remuxes wait until it is repaired");
                }
                let why = e.to_string();
                *self
                    .matches_failed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some((now, e.kind(), why.clone()));
                Err(failed(now, e.kind(), &why).unwrap_or(e))
            }
        }
    }

    fn loaded_store(&self) -> Option<Arc<matches::MatchStore>> {
        self.source_matches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Unreadable saved corrections are an error, never an automatic lookup.
    pub fn source_match(&self, source: &Path) -> Result<Option<matches::SavedMatch>, String> {
        self.match_store()
            .map(|store| store.get(source))
            .map_err(|e| e.to_string())
    }

    /// Whether `saved` was confirmed for the disc the last scan found at `source`; one saved
    /// for an earlier disc filed at that path is not this disc's.
    pub(crate) fn match_is_current(&self, source: &Path, saved: &matches::SavedMatch) -> bool {
        current_for(&self.snapshot(), source, saved)
    }

    // The saved match as already loaded: no disk access, for the listing.
    fn loaded_match(&self, source: &Path) -> Option<matches::SavedMatch> {
        self.loaded_store().and_then(|store| store.get(source))
    }

    fn require_indexed_source(&self, d: &Dirs, source: &Path) -> std::io::Result<()> {
        let snapshot = self.snapshot();
        if snapshot.dirs.as_ref() != Some(d)
            || snapshot.incomplete
            || !snapshot.isos.iter().any(|iso| iso.path == source)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "source ISO is not in the complete current library scan",
            ));
        }
        Ok(())
    }

    /// The saved match for `source` and the files recorded as made from it: exactly
    /// the old files a Change match would replace or remove. The flag is false when the
    /// share did not answer in time and the list is from the links alone.
    pub(crate) fn match_view(
        &self,
        cfg: &Config,
        source: &Path,
    ) -> std::io::Result<(Option<matches::SavedMatch>, Vec<PathBuf>, bool)> {
        self.match_view_with(cfg, source, MATCH_CHECK_TIMEOUT, |links, iso, roots| {
            owned::capture(&links, &iso, &roots)
        })
    }

    pub(crate) fn match_view_with<F>(
        &self,
        cfg: &Config,
        source: &Path,
        limit: std::time::Duration,
        capture: F,
    ) -> std::io::Result<(Option<matches::SavedMatch>, Vec<PathBuf>, bool)>
    where
        F: Fn(
                HashMap<PathBuf, links::Link>,
                PathBuf,
                Vec<PathBuf>,
            ) -> std::io::Result<Vec<owned::Owned>>
            + Send
            + Sync
            + 'static,
    {
        let saved = self.match_store()?.get(source);
        let links = links::read_entries(&self.config_dir)?;
        let d = dirs(cfg);
        let roots = owned::roots(&d);
        let by_rules = owned::eligible_paths(&links, source, &roots);
        // One probe per root, each under that root's own probe slot: a hung TV share
        // never holds the library folder's.
        let mut per_root: Vec<(PathBuf, CaptureInput)> = roots
            .iter()
            .map(|r| {
                (
                    r.clone(),
                    (HashMap::new(), source.to_path_buf(), vec![r.clone()]),
                )
            })
            .collect();
        for (path, link) in links {
            let deepest = per_root
                .iter_mut()
                .filter(|(root, _)| path.starts_with(root))
                .max_by_key(|(root, _)| root.components().count());
            if let Some((_, (mine, _, _))) = deepest {
                mine.insert(path, link);
            }
        }
        let capture = std::sync::Arc::new(capture);
        let answers = health::bounded_each(per_root, limit, move |(links, iso, roots)| {
            capture(links, iso, roots)
        });
        let mut found = Vec::new();
        for answer in answers {
            match answer {
                health::Bounded::Done(owned) => found.extend(owned?.into_iter().map(|o| o.path)),
                _ => return Ok((saved, by_rules, false)),
            }
        }
        found.sort();
        Ok((saved, found, true))
    }

    /// Save `media` as the identity of an indexed, idle `source`, then queue its remux.
    /// The match stays saved even when no job could be queued (`0`).
    pub(crate) fn confirm_match_and_queue(
        &self,
        cfg: &Config,
        source: &Path,
        expected_revision: u64,
        media: crate::server::planner::MediaMetadata,
    ) -> std::io::Result<(matches::SavedMatch, usize)> {
        let d = dirs(cfg);
        let disc = self.iso_stamp_bounded(&d, source)?;
        let _gate = self.match_gate.lock().unwrap_or_else(|e| e.into_inner());
        self.require_indexed_source(&d, source)?;
        if let Some(reason) = self.queue_block(&d, true) {
            return Err(std::io::Error::other(reason));
        }
        let store = self.match_store()?;
        let iso_title = self
            .snapshot()
            .isos
            .iter()
            .find(|iso| iso.path == source)
            .map(|iso| iso.title.clone())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "source ISO is not indexed")
            })?;
        let saved = self.queue.with_idle_source(source, || {
            store.save_for(source, expected_revision, media, disc)
        })?;
        self.touch_index();
        // Keyed by the source path, so two ISOs of one title never share a queue identity;
        // a planned job writes only its plan's outputs, never this path.
        let key = format!("{iso_title} [{:012x}]", index::path_hash(source) >> 16);
        let job = NewJob {
            title: saved.media.title.clone(),
            iso: source.to_path_buf(),
            target: index::new_mkv_target(&d.library, &key),
            replace: false,
        };
        let queued = self.queue.add_fresh(vec![job], true);
        Ok((saved, queued))
    }

    /// Drop the queued job of the row at `target`, and its ISO's Change match job, which
    /// writes under its own name. Returns how many went.
    pub fn remove_queued(&self, d: &Dirs, target: &Path) -> usize {
        let snap = self.snapshot();
        let source = snap
            .rows
            .iter()
            .filter(|_| snap.dirs.as_ref() == Some(d))
            .find(|r| r.target.as_deref() == Some(target))
            .and_then(|r| r.iso.as_deref());
        self.queue.remove_queued(target, source)
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
    /// that lands mid-scan is folded in; a Change match's retire wakes the indexer for
    /// another scan.
    pub fn rescan(&self, d: &Dirs) {
        let landed_at = self.landed.load(Ordering::SeqCst);
        let scan = self.scan(d);
        self.commit_scan(scan, landed_at);
    }

    // Swap `scan` in, with every remux that landed since `landed_at` folded into it: the scan
    // may predate those files.
    fn commit_scan(&self, mut scan: Snapshot, landed_at: u64) {
        {
            let mut guard = self.index.write().unwrap_or_else(|e| e.into_inner());
            let landings = self.landings.lock().unwrap_or_else(|e| e.into_inner());
            for (_, target, sig) in landings.iter().filter(|(seq, _, _)| *seq > landed_at) {
                fold_landed(&mut scan, target, *sig);
            }
            *guard = Arc::new(scan);
        }
        self.touch_index();
    }

    fn scan(&self, d: &Dirs) -> Snapshot {
        let started = std::time::Instant::now();
        let mut mkvs = index::list_mkvs(&d.library);
        if let Some(tv) = &d.tv
            && !tv.starts_with(&d.library)
        {
            // A not-yet-created TV directory is empty, not an inaccessible scan.
            if !matches!(std::fs::symlink_metadata(tv), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
            {
                let extra = index::list_mkvs(tv);
                mkvs.incomplete |= extra.incomplete;
                let mut seen: HashSet<_> =
                    mkvs.files.iter().map(|file| file.path.clone()).collect();
                mkvs.files.extend(
                    extra
                        .files
                        .into_iter()
                        .filter(|file| seen.insert(file.path.clone())),
                );
            }
        }
        let entries = links::entries(&self.config_dir);
        let links: HashMap<PathBuf, PathBuf> = entries
            .iter()
            .map(|(mkv, l)| (mkv.clone(), l.iso.clone()))
            .collect();
        // Path to position, so merging links and delivered outputs is linear.
        let mut at: HashMap<PathBuf, usize> = HashMap::new();
        for (i, m) in mkvs.files.iter().enumerate() {
            at.entry(m.path.clone()).or_insert(i);
        }
        let candidates: Vec<PathBuf> = links
            .keys()
            .cloned()
            .chain(self.queue.snapshot().jobs.into_iter().flat_map(|job| {
                job.outputs
                    .into_iter()
                    .filter(|o| o.state == queue::OutputState::Done)
                    .map(|o| o.target)
            }))
            .filter(|p| !at.contains_key(p))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let checked = check_targets(candidates, INJECT_STAT_LIMIT, stat_target);
        self.forget_gone_links(d, &entries, &checked);
        let injectable = |p: &Path| checked.get(p).is_none_or(|t| *t != Target::Gone);
        for target in links.keys() {
            if !at.contains_key(target) && injectable(target) {
                at.insert(target.clone(), mkvs.files.len());
                mkvs.files.push(index::MkvFile {
                    path: target.clone(),
                    title: target
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                });
            }
        }
        // Delivered TV episodes may live in a separate configured TV root.
        for job in self.queue.snapshot().jobs {
            for output in job
                .outputs
                .iter()
                .filter(|o| o.state == queue::OutputState::Done)
            {
                let title = output
                    .target
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if let Some(&i) = at.get(&output.target) {
                    mkvs.files[i].title = title;
                } else if injectable(&output.target) {
                    at.insert(output.target.clone(), mkvs.files.len());
                    mkvs.files.push(index::MkvFile {
                        path: output.target.clone(),
                        title,
                    });
                }
            }
        }
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
                .filter(|m| {
                    !seen.contains(m.path.as_path())
                        && (checked.get(&m.path) == Some(&Target::Unverified) || !gone(&m.path))
                })
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
        mkvs.files
            .retain(|m| match stat_unless_unverified(&checked, &m.path) {
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
        let iso_stamps = isos
            .files
            .iter()
            .filter(|i| {
                self.loaded_match(&i.path)
                    .is_some_and(|m| m.iso_stamp.is_some())
            })
            .map(|i| (i.path.clone(), links::iso_stamp(&i.path)))
            .collect();
        let rows = index::classify(&d.library, &mkvs.files, &isos.files, &links);
        let library_empty = std::fs::read_dir(&d.library).is_ok_and(|mut r| r.next().is_none());
        Snapshot {
            library_empty,
            dirs: Some(d.clone()),
            mkvs: mkvs.files,
            isos: isos.files,
            rows,
            sigs,
            iso_stamps,
            incomplete: mkvs.incomplete || isos.incomplete,
            scanned_at: Some(crate::server::util::epoch_secs()),
            scan_ms: started.elapsed().as_millis() as u64,
        }
    }

    // Drop the links whose MKV a stat found missing, so they are not looked for on every
    // scan. Only under a root that lists entries (an empty one may be an unmounted share)
    // and from a folder that still exists (a nested mount or pool disk may be offline).
    fn forget_gone_links(
        &self,
        d: &Dirs,
        entries: &HashMap<PathBuf, links::Link>,
        checked: &HashMap<PathBuf, Target>,
    ) {
        let populated: Vec<&PathBuf> = std::iter::once(&d.library)
            .chain(d.tv.as_ref())
            .filter(|root| std::fs::read_dir(root).is_ok_and(|mut r| r.next().is_some()))
            .collect();
        let gone: HashMap<PathBuf, links::Link> = entries
            .iter()
            .filter(|(mkv, _)| checked.get(*mkv) == Some(&Target::Gone))
            .filter(|(mkv, _)| populated.iter().any(|root| mkv.starts_with(root)))
            .filter(|(mkv, _)| mkv.parent().is_some_and(|p| p.is_dir()))
            .map(|(mkv, l)| (mkv.clone(), l.clone()))
            .collect();
        match links::forget_gone(&self.config_dir, &gone) {
            Ok(0) => {}
            Ok(n) => tracing::info!(count = n, "forgot links to library files that are gone"),
            Err(e) => {
                tracing::warn!(error = %e, "links to gone library files could not be forgotten")
            }
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
        // Edited under the write lock; a scan that began earlier folds it in (see `commit_scan`).
        {
            let mut guard = self.index.write().unwrap_or_else(|e| e.into_inner());
            let seq = self.landed.fetch_add(1, Ordering::SeqCst) + 1;
            let mut landings = self.landings.lock().unwrap_or_else(|e| e.into_inner());
            if landings.len() == LANDINGS_KEPT {
                landings.pop_front();
            }
            landings.push_back((seq, target.to_path_buf(), sig));
            let mut next = (**guard).clone();
            fold_landed(&mut next, target, sig);
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
        // A Change match job writes under its own hashed name: it shows on its ISO's rows.
        let mut matched = HashMap::new();
        for j in &q.jobs {
            latest.insert(j.target.as_path(), j);
            if j.retire {
                matched.insert(j.iso.as_path(), j);
            }
            for output in &j.outputs {
                latest.insert(output.target.as_path(), j);
            }
        }
        let running = probe::running_version();
        let (mut probing, mut auditing) = (0, 0);
        let deep_on = self.deep_enabled();
        let queued = self.audits.queued_set();
        let running_audits = self.audits.running_set();
        let mut source_rows = snap.rows.clone();
        let attached: HashSet<&Path> = snap.rows.iter().filter_map(|r| r.iso.as_deref()).collect();
        let colliding: HashMap<&Path, usize> = snap
            .rows
            .iter()
            .flat_map(|r| {
                r.several_isos
                    .iter()
                    .map(|i| (i.as_path(), r.several_isos.len()))
            })
            .collect();
        for iso in &snap.isos {
            if !attached.contains(iso.path.as_path()) {
                source_rows.push(Row {
                    key: format!("source:{}", iso.path.display()),
                    title: iso.title.clone(),
                    kind: RowKind::Ambiguous,
                    note: Some(RowNote::SeveralIsos {
                        count: colliding.get(iso.path.as_path()).copied().unwrap_or(1),
                    }),
                    mkv: None,
                    iso: Some(iso.path.clone()),
                    target: None,
                    linked: false,
                    several_isos: Vec::new(),
                });
            }
        }
        let rows = source_rows
            .into_iter()
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
                // A row with an MKV shows it only while it is queued or running.
                let by_source = r
                    .iso
                    .as_deref()
                    .and_then(|iso| matched.get(iso))
                    .filter(|m| {
                        r.mkv.is_none()
                            || matches!(m.state, queue::JobState::Queued | queue::JobState::Running)
                    })
                    .filter(|m| {
                        let by_row = r.target.as_deref().and_then(|t| latest.get(t));
                        by_row.is_none_or(|j| j.id < m.id)
                    });
                let (job, result) = match &r.target {
                    _ if by_source.is_some() => {
                        let j = by_source.map(|j| (*j).clone());
                        let result = j
                            .as_ref()
                            .and_then(|j| q.results.get(&*j.target.to_string_lossy()))
                            .cloned();
                        (j, result)
                    }
                    Some(t) => (
                        latest.get(t.as_path()).map(|j| (*j).clone()),
                        q.results
                            .get(&*t.to_string_lossy())
                            .or_else(|| {
                                latest
                                    .get(t.as_path())
                                    .and_then(|j| q.results.get(&*j.target.to_string_lossy()))
                            })
                            .cloned(),
                    ),
                    None => (None, None),
                };
                // A match confirmed for an earlier disc filed at this path is not this row's.
                let source_match = r.iso.as_deref().and_then(|iso| {
                    self.loaded_match(iso)
                        .filter(|m| current_for(&snap, iso, m))
                });
                let title = source_match
                    .as_ref()
                    .map(|m| {
                        if m.media.year == 0 {
                            m.media.title.clone()
                        } else {
                            format!("{} ({})", m.media.title, m.media.year)
                        }
                    })
                    .unwrap_or(r.title);
                RowView {
                    title,
                    source_match,
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
        if let Some(why) = self.queue.unreadable() {
            return Some(crate::strings::fmt_or(
                "server.library.nothing_queued",
                "{reason} Nothing was queued.",
                &[("reason", &why)],
            ));
        }
        if let Some(h) = folder_gate(&folder_views(d), false) {
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
    /// check, or a preflight failed and its recheck time has not come. `tv` says the job
    /// files into the separate TV folder.
    pub(crate) fn blocked(&self, d: &Dirs, now: u64, tv: bool) -> Option<Hold> {
        if let Some(h) = folder_gate(&folder_views(d), tv) {
            return Some(h);
        }
        self.hold()
            .filter(|h| h.recheck_at.is_some_and(|t| now < t))
    }

    /// Hold the queue while its file could not be loaded; true when it is held so. The
    /// file is read again at most once a minute, and the hold clears once it loads.
    pub(crate) fn hold_for_unreadable_queue(&self, now: u64) -> bool {
        if self.queue.unreadable().is_none() {
            return false;
        }
        if self.queue.retry_unreadable(now) {
            self.set_hold(None);
            return false;
        }
        let Some(why) = self.queue.unreadable() else {
            return false;
        };
        self.set_hold(Some(Hold {
            role: QUEUE_FILE_ROLE,
            path: self.queue.path().to_path_buf(),
            state: health::State::Unhealthy,
            message: why,
            hint: crate::strings::get_or(
                "server.library.queue_held_hint",
                "Nothing in the remux queue runs or changes until this file can be read.",
            ),
            since: now,
            recheck_at: None,
        }));
        true
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
            self.touch_live();
        }
        (running.is_some(), removed)
    }

    /// Whether "Stop all" cancelled job `id`.
    pub fn cancelled(&self, id: u64) -> bool {
        id != 0 && self.cancel_job.load(Ordering::SeqCst) == id
    }

    /// Delete unowned movie partials, known output partials, and remux partials in the show
    /// and season folders of the TV folder (the library when there is none), skipping one
    /// whose `<target>.lock` another writer holds. Returns how many went.
    pub fn sweep_partials(&self, d: &Dirs) -> usize {
        let mut candidates: HashSet<PathBuf> = self
            .queue
            .snapshot()
            .jobs
            .iter()
            .flat_map(|j| std::iter::once(j.target.clone()).chain(j.output_targets()))
            .chain(links::load(&self.config_dir).into_keys())
            .map(|p| queue::partial_path(&p))
            .collect();
        // `<library>/<title>/` holds a movie; `<tv>/<show>/<season>/` an episode, and with no
        // separate TV folder `<library>/<show>/<season>/`.
        match &d.tv {
            Some(tv) => {
                candidates.extend(partials_under(&d.library, &[1]));
                candidates.extend(partials_under(tv, &[2]));
            }
            None => candidates.extend(partials_under(&d.library, &[1, 2])),
        }
        candidates
            .into_iter()
            .filter(|path| {
                if !self.queue.remove_orphan_partial(path) {
                    return false;
                }
                tracing::info!(path = %path.display(), "removed an orphaned remux partial");
                true
            })
            .count()
    }

    #[cfg(test)]
    pub fn enqueue(&self, d: &Dirs, pick: impl Fn(&RowView) -> bool) -> usize {
        self.enqueue_checked(d, pick).expect("test queue admission")
    }

    /// Admit selected rows, reporting durable-admission failures to the caller
    /// instead of making an unavailable match store look like an empty selection.
    pub fn enqueue_checked(
        &self,
        d: &Dirs,
        pick: impl Fn(&RowView) -> bool,
    ) -> Result<usize, String> {
        // The ISOs with a saved match are stamped before the gate: a hung share never holds it.
        let mut stamps = HashMap::new();
        for r in self.listing(d).rows.into_iter().filter(|r| pick(r)) {
            if let Some(iso) = r.iso.filter(|iso| self.loaded_match(iso).is_some())
                && !stamps.contains_key(&iso)
            {
                let stamp = self.iso_stamp_bounded(d, &iso).map_err(|e| e.to_string())?;
                stamps.insert(iso, stamp);
            }
        }
        let _gate = self.match_gate.lock().unwrap_or_else(|e| e.into_inner());
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
        // Plain rows go in one queue write. A row whose ISO has a saved match is planned
        // afresh from it and may overwrite only files linked to that ISO.
        let mut added = 0;
        let mut plain = Vec::new();
        let mut matched = Vec::new();
        for mut job in jobs {
            let selected = self.source_match(&job.iso).map_err(|error| {
                added += self.queue.add(std::mem::take(&mut plain));
                added += self.add_matched(std::mem::take(&mut matched));
                format!("{added} jobs queued; saved source matches unavailable: {error}")
            })?;
            // A match confirmed for an earlier disc filed at this path is not this disc's.
            // One saved after the stamps were read goes in plain; the worker applies it at claim.
            if !selected.is_some_and(|m| stamps.get(&job.iso).is_some_and(|now| m.is_for(*now))) {
                plain.push(job);
                continue;
            }
            job.replace = false;
            matched.push(job);
        }
        if !plain.is_empty() {
            added += self.queue.add(plain);
        }
        Ok(added + self.add_matched(matched))
    }

    // Queue rows planned from their ISO's saved match, in one write: a linked episode plans
    // only itself; any other row plans the whole disc, once for all of its ISO's rows.
    fn add_matched(&self, mut rows: Vec<NewJob>) -> usize {
        if rows.is_empty() {
            return 0;
        }
        let links = links::entries(&self.config_dir);
        let episode = |j: &NewJob| {
            links
                .get(&j.target)
                .is_some_and(|l| l.episode && l.iso == j.iso)
        };
        let whole: HashSet<PathBuf> = rows
            .iter()
            .filter(|j| !episode(j))
            .map(|j| j.iso.clone())
            .collect();
        let mut planned = HashSet::new();
        rows.retain(|j| !whole.contains(&j.iso) || (!episode(j) && planned.insert(j.iso.clone())));
        self.queue.add_fresh(rows, false)
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
        self.live().running.clone().map(|mut r| {
            r.stopping = self.cancelled(r.job_id);
            r
        })
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

/// How many folders a partial sweep looks into at most.
const SWEEP_FOLDERS: usize = 10_000;

// The `.mkv.partial` files in the folders each of `depths` levels below `root` (not in
// `root` itself), in one walk that follows no symlink, into at most SWEEP_FOLDERS folders.
fn partials_under(root: &Path, depths: &[usize]) -> Vec<PathBuf> {
    let deepest = depths.iter().copied().max().unwrap_or(0);
    let mut found = Vec::new();
    let mut level = vec![root.to_path_buf()];
    let mut visited = 0;
    for at in 0..=deepest {
        let mut next = Vec::new();
        for dir in level {
            if visited == SWEEP_FOLDERS {
                return found;
            }
            visited += 1;
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let partial = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.to_ascii_lowercase().ends_with(".mkv.partial"));
                if partial && depths.contains(&at) {
                    found.push(path);
                } else if at < deepest && entry.file_type().is_ok_and(|t| t.is_dir()) {
                    next.push(path);
                }
            }
        }
        level = next;
    }
    found
}

/// How many landings are remembered for scans that began before them.
const LANDINGS_KEPT: usize = 1024;

// A remux's new file at `target`, as a snapshot shows it before any scan has listed it.
fn fold_landed(next: &mut Snapshot, target: &Path, sig: FileSig) {
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
}

// A file a scan did not list is gone only when a stat says NotFound.
// The raw header report is served on its own (`/api/library/raw`), not in every listing.
fn without_raw(mut a: AuditReport) -> AuditReport {
    if let Some(d) = a.detail.as_mut() {
        d.raw.clear();
    }
    a
}

/// How long all recorded targets together may take to stat in one scan.
const INJECT_STAT_LIMIT: std::time::Duration = std::time::Duration::from_secs(2);

/// What a bounded look at a recorded target found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    /// It stats, or failed with something other than NotFound: keep its row.
    Present,
    Gone,
    /// The share did not answer in time: keep the row, stat nothing more.
    Unverified,
}

/// How many folders a scan stats at once.
const TARGET_PROBES: usize = 16;

// Stat every path under one shared deadline, one probe per folder, at most TARGET_PROBES
// folders at a time; once a folder does not answer, the rest stay unverified rather than
// pin more threads on a dead share. Only NotFound drops a row.
fn check_targets(
    paths: Vec<PathBuf>,
    limit: std::time::Duration,
    stat: fn(&Path) -> std::io::Result<()>,
) -> HashMap<PathBuf, Target> {
    let deadline = std::time::Instant::now() + limit;
    let mut folders: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for p in paths {
        let folder = p.parent().map(Path::to_path_buf).unwrap_or_default();
        folders.entry(folder).or_default().push(p);
    }
    let items: Vec<_> = folders.into_iter().collect();
    let mut out = HashMap::new();
    let mut answering = true;
    for batch in items.chunks(TARGET_PROBES) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if !answering || left.is_zero() {
            out.extend(
                batch
                    .iter()
                    .flat_map(|(_, files)| files.iter().map(|p| (p.clone(), Target::Unverified))),
            );
            continue;
        }
        let answers = health::bounded_each(batch.to_vec(), left, move |files: Vec<PathBuf>| {
            files
                .into_iter()
                .map(|p| match stat(&p) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (p, Target::Gone),
                    _ => (p, Target::Present),
                })
                .collect::<Vec<_>>()
        });
        for ((_, files), answer) in batch.iter().zip(answers) {
            match answer {
                health::Bounded::Done(found) => out.extend(found),
                _ => {
                    answering = false;
                    out.extend(files.iter().map(|p| (p.clone(), Target::Unverified)));
                }
            }
        }
    }
    out
}

fn stat_target(p: &Path) -> std::io::Result<()> {
    std::fs::metadata(p).map(|_| ())
}

// The stat the scan ends with, except for a path whose bounded look timed out: that one is
// kept with its earlier signature rather than stat'd again without a limit.
fn stat_unless_unverified(
    checked: &HashMap<PathBuf, Target>,
    p: &Path,
) -> std::io::Result<std::fs::Metadata> {
    if checked.get(p) == Some(&Target::Unverified) {
        return Err(std::io::Error::other("unverified"));
    }
    std::fs::metadata(p)
}

fn gone(p: &Path) -> bool {
    matches!(std::fs::metadata(p), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Move the damaged file `path` to `<name>.unreadable` (`.unreadable.N` when that is taken),
/// so an earlier copy set aside is never overwritten. Returns where it went.
pub(crate) fn set_aside(path: &Path) -> std::io::Result<PathBuf> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for n in 0..1000u32 {
        let aside = path.with_file_name(match n {
            0 => format!("{name}.unreadable"),
            n => format!("{name}.unreadable.{n}"),
        });
        match std::fs::symlink_metadata(&aside) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::rename(path, &aside)?;
                return Ok(aside);
            }
            Err(e) => return Err(e),
            Ok(_) => {}
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("too many copies of {name} are already set aside"),
    ))
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
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "match_tests.rs"]
mod match_tests;

fn matches_unreadable(config_dir: &Path, why: &str) -> String {
    crate::strings::fmt_or(
        "server.library.matches_unreadable",
        "the saved matches (library-matches.json in {dir}) cannot be read: {error}",
        &[("dir", &config_dir.display().to_string()), ("error", why)],
    )
}
