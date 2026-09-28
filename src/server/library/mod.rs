//! The Library: every MKV and source ISO, which freemkv muxed each MKV, and a
//! queue that remuxes out-of-date titles from their ISO through the engine.
//!
//! [`index`] builds the cross-list, [`probe`] reads the muxed-with stamp and
//! runs the fast audit, [`queue`] persists the jobs, [`worker`] runs them one
//! at a time, [`arbiter`] gives rips the mux slot first, and [`api`] serves
//! `/api/library*`.

pub mod api;
pub mod arbiter;
pub mod index;
pub mod links;
pub mod probe;
pub mod queue;
pub mod worker;

use crate::server::config::Config;
use index::{Row, RowKind, RowNote};
use probe::{AuditReport, FileSig, MuxedWith, ProbeCache};
use queue::{Job, JobResult, NewJob, Queue};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// Where the Library looks, resolved from the settings.
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// One line of the running job's console.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ConsoleLine {
    pub seq: u64,
    pub ts: u64,
    pub text: String,
}

/// The running job as the UI shows it.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Running {
    pub job_id: u64,
    pub title: String,
    pub target: PathBuf,
    pub phase: String,
    pub pct: Option<f64>,
    pub speed_bps: u64,
    pub eta_secs: Option<u64>,
    pub started_at: u64,
    pub stalled_secs: u64,
}

const CONSOLE_LINES: usize = 1000;

#[derive(Default)]
struct Live {
    lines: VecDeque<ConsoleLine>,
    next_seq: u64,
    running: Option<Running>,
}

/// The Library's state: the queue, the probe cache and the live console.
pub struct Library {
    pub queue: Queue,
    pub probes: ProbeCache,
    config_dir: PathBuf,
    log_dir: PathBuf,
    live: Mutex<Live>,
    live_generation: AtomicU64,
    // Set by the watchdog to stop a job that stopped moving.
    stall_cancel: AtomicBool,
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

/// Start the remux worker and the background auditor.
pub fn start(cfg: &Arc<RwLock<Config>>) -> std::thread::JoinHandle<()> {
    let lib = instance(&cfg.read().unwrap_or_else(|e| e.into_inner()));
    {
        let (lib, cfg) = (lib.clone(), cfg.clone());
        let _ = std::thread::Builder::new()
            .name("library-audit".into())
            .spawn(move || worker::audit_loop(&lib, &cfg));
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
    pub writing_app: Option<String>,
    pub muxed_with: MuxedWith,
    /// Remuxable and either out of date or not muxed yet.
    pub needs_remux: bool,
    /// `None` until the auditor has reached this file.
    pub audit: Option<AuditReport>,
    pub job: Option<Job>,
    pub result: Option<JobResult>,
}

/// The cross-list plus whether any folder could not be fully read.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Listing {
    pub rows: Vec<RowView>,
    pub incomplete: bool,
}

impl Library {
    pub fn open(config_dir: &Path, log_dir: &Path) -> Self {
        Self {
            queue: Queue::open(config_dir),
            probes: ProbeCache::default(),
            config_dir: config_dir.to_path_buf(),
            log_dir: log_dir.to_path_buf(),
            live: Mutex::new(Live::default()),
            live_generation: AtomicU64::new(0),
            stall_cancel: AtomicBool::new(false),
        }
    }

    /// Scan and match; no per-file reads.
    pub fn classify(&self, d: &Dirs) -> (Vec<Row>, bool) {
        let mkvs = index::list_mkvs(&d.library);
        let isos = match &d.isos {
            Some(dir) => index::list_isos(dir, d.iso_subfolders),
            None => index::Listing::default(),
        };
        let links = links::load(&self.config_dir);
        let rows = index::classify(&d.library, &mkvs.files, &isos.files, &links);
        (rows, mkvs.incomplete || isos.incomplete)
    }

    /// The full cross-list with muxed-with, audit, job and result per row.
    pub fn listing(&self, d: &Dirs) -> Listing {
        let (rows, incomplete) = self.classify(d);
        let q = self.queue.snapshot();
        let running = probe::running_version();
        let rows = rows
            .into_iter()
            .map(|r| {
                let sig = r.mkv.as_deref().and_then(FileSig::stat);
                let writing_app = r
                    .mkv
                    .as_deref()
                    .and_then(|m| self.probes.writing_app(m))
                    .flatten();
                let muxed_with = MuxedWith::from_app(writing_app.as_deref(), running);
                let audit = r
                    .mkv
                    .as_deref()
                    .zip(sig)
                    .and_then(|(m, s)| self.probes.audit(m, s));
                let needs_remux = r.remuxable() && (r.mkv.is_none() || muxed_with.out_of_date());
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
                    writing_app,
                    muxed_with,
                    needs_remux,
                    audit,
                    job,
                    result,
                }
            })
            .collect();
        Listing { rows, incomplete }
    }

    /// Queue the remuxable rows `pick` selects. Returns how many were added.
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

    /// Append a console line.
    pub fn console(&self, text: impl Into<String>) {
        {
            let mut l = self.live();
            l.next_seq += 1;
            let line = ConsoleLine {
                seq: l.next_seq,
                ts: crate::server::util::epoch_secs(),
                text: text.into(),
            };
            if l.lines.len() == CONSOLE_LINES {
                l.lines.pop_front();
            }
            l.lines.push_back(line);
        }
        self.touch_live();
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

    pub fn running(&self) -> Option<Running> {
        self.live().running.clone()
    }

    fn set_running(&self, f: impl FnOnce(&mut Option<Running>)) {
        f(&mut self.live().running);
        self.touch_live();
    }
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

    #[test]
    fn the_console_is_a_bounded_ring() {
        let t = tempfile::tempdir().unwrap();
        let lib = Library::open(t.path(), t.path());
        for i in 0..(CONSOLE_LINES + 5) {
            lib.console(format!("line {i}"));
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
