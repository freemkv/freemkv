//! The episode plan a drive card lists while a TV disc rips: the same capture plan the
//! mux will commit, kept per job so the card can show it from the first pass on. A title
//! correction replans it from the scan it was planned from, as the mux will. Each episode
//! carries the playback time its not-yet-good sectors cover, read from the job's mapfile.

use crate::server::config::Config;
use crate::server::planner::MediaKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// One planned episode as the drive card shows it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CardEpisode {
    pub season: u16,
    pub episode: u16,
    /// TMDB episode name; empty when TMDB has none for it.
    pub name: String,
    pub duration: String,
    /// Playback time of this episode in sectors not read cleanly yet, as the movie's.
    pub at_risk_ms: f64,
    #[serde(skip)]
    title_index: usize,
}

// Scan inputs are copied once, before taking the cache lock. Corrections only
// retain their latest payload, and admitted workers share these immutable inputs.
struct Inputs {
    titles: Vec<libfreemkv::DiscTitle>,
    label: String,
    filename: String,
    mapfile: PathBuf,
}

#[derive(Clone)]
pub(crate) struct Correction {
    pub cfg: Config,
    pub revision: u64,
    pub kind: MediaKind,
    pub tmdb_id: u64,
    pub start: Option<u16>,
}

struct Planned {
    job: String,
    incarnation: Arc<()>,
    rows: Arc<()>,
    inputs: Arc<Inputs>,
    episodes: Vec<CardEpisode>,
    measured: Option<Instant>,
    measure_pending: bool,
    measuring: Option<Arc<()>>,
    latest: Correction,
    replan_pending: bool,
    replanning: Option<Arc<()>>,
}

const KEEP: usize = 8;
const MEASURE_EVERY: Duration = Duration::from_secs(5);
// Separate lanes keep stalled filesystem calls from consuming correction slots.
const MEASURE_CAP: usize = 2;
const REPLAN_CAP: usize = 2;

#[derive(Default)]
struct Plans {
    entries: Vec<Planned>,
    measuring: usize,
    replanning: usize,
    cursor: [usize; 2],
}

// The three blocking operations are a narrow test seam, rather than an executor:
// production uses the existing mapfile interpretation and planner unchanged.
trait CardWork: Send + Sync {
    fn damage(&self, path: &Path) -> Option<Vec<(u64, u64)>>;
    fn plan(&self, inputs: &Inputs, correction: &Correction) -> Vec<CardEpisode>;
    fn spawn(&self, task: Box<dyn FnOnce() + Send>) -> std::io::Result<()>;
}

struct CardIo;
impl CardWork for CardIo {
    fn damage(&self, path: &Path) -> Option<Vec<(u64, u64)>> {
        use freemkv_engine::SectorStatus as S;
        let map = freemkv_engine::Mapfile::load(path).ok()?;
        Some(map.ranges_with(&[S::NonTrimmed, S::NonScraped, S::Unreadable]))
    }

    fn plan(&self, inputs: &Inputs, correction: &Correction) -> Vec<CardEpisode> {
        let label = super::planning_label(&inputs.label, Some(correction.kind));
        super::plan_mux_outputs(
            &inputs.titles,
            &correction.cfg,
            correction.kind,
            label,
            correction.tmdb_id,
            &inputs.filename,
            correction.start,
        )
        .map(|outputs| card_episodes(&inputs.titles, label, &outputs))
        .unwrap_or_default()
    }

    fn spawn(&self, task: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
        std::thread::Builder::new()
            .name("card-plan".into())
            .spawn(task)
            .map(|_| ())
    }
}

struct Cache {
    plans: Mutex<Plans>,
    work: Arc<dyn CardWork>,
}

static PLANS: LazyLock<Arc<Cache>> = LazyLock::new(|| Cache::new(Arc::new(CardIo)));

#[derive(Clone, Copy)]
enum Lane {
    Measure,
    Replan,
}

// This permit owns an actual admitted worker, even after its entry is evicted.
// Only the matching operation may clear its marker. Drop also covers panic and
// failed spawn; a failed spawn retains pending work for the next cache call.
struct Permit {
    cache: Arc<Cache>,
    incarnation: Arc<()>,
    owner: Arc<()>,
    lane: Lane,
    started: bool,
    completed: bool,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut plans = self.cache.plans.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = plans
            .entries
            .iter_mut()
            .find(|p| Arc::ptr_eq(&p.incarnation, &self.incarnation))
        {
            match self.lane {
                Lane::Measure
                    if p.measuring
                        .as_ref()
                        .is_some_and(|m| Arc::ptr_eq(m, &self.owner)) =>
                {
                    p.measuring = None;
                    if !self.started {
                        p.measured = None;
                        p.measure_pending = true;
                    }
                }
                Lane::Replan
                    if p.replanning
                        .as_ref()
                        .is_some_and(|m| Arc::ptr_eq(m, &self.owner)) =>
                {
                    p.replanning = None;
                    if !self.completed {
                        p.replan_pending = true;
                    }
                }
                _ => {}
            }
        }
        match self.lane {
            Lane::Measure => plans.measuring -= 1,
            Lane::Replan => plans.replanning -= 1,
        }
        drop(plans);
        if self.started {
            // On panic, dispatch other entries, leaving this payload available
            // for a later retry without an immediate recurring panic loop.
            self.cache
                .dispatch((!self.completed).then_some(&self.incarnation));
        }
    }
}

struct Task {
    permit: Permit,
    inputs: Arc<Inputs>,
    rows: Arc<()>,
    episodes: Vec<CardEpisode>,
    correction: Option<Correction>,
}

impl Cache {
    fn new(work: Arc<dyn CardWork>) -> Arc<Self> {
        Arc::new(Self {
            plans: Mutex::new(Plans::default()),
            work,
        })
    }

    fn publish(
        self: &Arc<Self>,
        job: &str,
        inputs: Inputs,
        outputs: &[super::staging::Output],
        initial: Correction,
    ) {
        let episodes = card_episodes(&inputs.titles, &inputs.label, outputs);
        let inputs = Arc::new(inputs);
        let mut plans = self.plans.lock().unwrap_or_else(|e| e.into_inner());
        let latest = plans
            .entries
            .iter()
            .find(|p| p.job == job)
            .map(|p| &p.latest)
            .filter(|c| c.revision > initial.revision)
            .cloned();
        let replan_pending = latest.is_some();
        let latest = latest.unwrap_or(initial);
        plans.entries.retain(|p| p.job != job);
        plans.entries.push(Planned {
            job: job.into(),
            incarnation: Arc::new(()),
            rows: Arc::new(()),
            inputs,
            episodes,
            measured: None,
            measure_pending: false,
            measuring: None,
            replan_pending,
            latest,
            replanning: None,
        });
        let over = plans.entries.len().saturating_sub(KEEP);
        plans.entries.drain(..over);
        drop(plans);
        self.dispatch(None);
    }

    fn episodes(self: &Arc<Self>, job: &str) -> Option<Vec<CardEpisode>> {
        let mut plans = self.plans.lock().unwrap_or_else(|e| e.into_inner());
        let result = plans
            .entries
            .iter_mut()
            .find(|p| p.job == job)
            .and_then(|p| {
                if p.episodes.is_empty() {
                    return None;
                }
                if p.measured.is_none_or(|at| at.elapsed() >= MEASURE_EVERY) {
                    p.measure_pending = true;
                }
                Some(p.episodes.clone())
            });
        drop(plans);
        self.dispatch(None);
        result
    }

    fn replan(self: &Arc<Self>, job: &str, correction: Correction) {
        let mut plans = self.plans.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = plans.entries.iter_mut().find(|p| p.job == job)
            && correction.revision > p.latest.revision
        {
            p.latest = correction;
            p.replan_pending = true;
        }
        drop(plans);
        self.dispatch(None);
    }

    fn reconcile(self: &Arc<Self>, cfg: &Config, device: &str, job: &str) {
        let correction = {
            let state = super::STATE.lock().unwrap_or_else(|e| e.into_inner());
            state.get(device).filter(|s| s.job_id == job).and_then(|s| {
                s.user_metadata.as_ref().map(|m| Correction {
                    cfg: cfg.clone(),
                    revision: s.metadata_revision,
                    kind: m.media_type,
                    tmdb_id: m.tmdb_id,
                    start: m.episode_start,
                })
            })
        };
        if let Some(correction) = correction {
            self.replan(job, correction);
        }
    }

    // Round-robin admission over eight bounded entries. Every completion checks
    // all entries, so a saturated request needs no later poll to get dispatched.
    fn dispatch(self: &Arc<Self>, skip: Option<&Arc<()>>) {
        let mut plans = self.plans.lock().unwrap_or_else(|e| e.into_inner());
        let mut tasks = Vec::new();
        let len = plans.entries.len();
        for (lane_index, lane) in [Lane::Replan, Lane::Measure].into_iter().enumerate() {
            let cursor = plans.cursor[lane_index];
            for offset in 0..len {
                let index = (cursor + offset) % len;
                if skip.is_some_and(|id| Arc::ptr_eq(id, &plans.entries[index].incarnation)) {
                    continue;
                }
                let available = match lane {
                    Lane::Measure => plans.measuring < MEASURE_CAP,
                    Lane::Replan => plans.replanning < REPLAN_CAP,
                };
                let p = &mut plans.entries[index];
                let pending = match lane {
                    Lane::Measure => {
                        p.measure_pending && p.measuring.is_none() && !p.episodes.is_empty()
                    }
                    Lane::Replan => p.replan_pending && p.replanning.is_none(),
                };
                if !available || !pending {
                    continue;
                }
                let owner = Arc::new(());
                let correction = match lane {
                    Lane::Measure => {
                        p.measure_pending = false;
                        p.measuring = Some(owner.clone());
                        p.measured = Some(Instant::now());
                        None
                    }
                    Lane::Replan => {
                        p.replan_pending = false;
                        p.replanning = Some(owner.clone());
                        Some(p.latest.clone())
                    }
                };
                tasks.push(Task {
                    permit: Permit {
                        cache: self.clone(),
                        incarnation: p.incarnation.clone(),
                        owner,
                        lane,
                        started: false,
                        completed: false,
                    },
                    inputs: p.inputs.clone(),
                    rows: p.rows.clone(),
                    episodes: match lane {
                        Lane::Measure => p.episodes.clone(),
                        Lane::Replan => Vec::new(),
                    },
                    correction,
                });
                match lane {
                    Lane::Measure => plans.measuring += 1,
                    Lane::Replan => plans.replanning += 1,
                }
                // Polls that cannot admit work do not disturb lane fairness.
                plans.cursor[lane_index] = (index + 1) % len;
            }
        }
        drop(plans);
        for mut task in tasks {
            if let Err(error) = self.work.spawn(Box::new(move || {
                task.permit.started = true;
                task.run();
            })) {
                tracing::warn!(%error, "card background worker could not start");
            }
        }
    }
}

impl Task {
    fn run(mut self) {
        let cache = self.permit.cache.clone();
        match self.permit.lane {
            Lane::Measure => {
                if let Some(damage) = cache.work.damage(&self.inputs.mapfile) {
                    let risk: Vec<_> = self
                        .episodes
                        .iter()
                        .map(|e| {
                            self.inputs
                                .titles
                                .get(e.title_index)
                                .map_or(0.0, |t| at_risk_ms(t, &damage))
                        })
                        .collect();
                    let mut plans = cache.plans.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(p) = plans.entries.iter_mut().find(|p| {
                        Arc::ptr_eq(&p.incarnation, &self.permit.incarnation)
                            && Arc::ptr_eq(&p.rows, &self.rows)
                            && p.measuring
                                .as_ref()
                                .is_some_and(|m| Arc::ptr_eq(m, &self.permit.owner))
                    }) {
                        // Only measurements change; names/indices belong to the current plan.
                        for (row, risk) in p.episodes.iter_mut().zip(risk) {
                            row.at_risk_ms = risk;
                        }
                    }
                }
            }
            Lane::Replan => {
                let correction = self.correction.as_ref().expect("admitted correction");
                let episodes = cache.work.plan(&self.inputs, correction);
                let mut plans = cache.plans.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(p) = plans.entries.iter_mut().find(|p| {
                    Arc::ptr_eq(&p.incarnation, &self.permit.incarnation)
                        && p.latest.revision == correction.revision
                        && p.replanning
                            .as_ref()
                            .is_some_and(|m| Arc::ptr_eq(m, &self.permit.owner))
                }) {
                    p.episodes = episodes;
                    p.rows = Arc::new(());
                    p.measured = None;
                    p.measure_pending = !p.episodes.is_empty();
                }
            }
        }
        self.permit.completed = true;
    }
}

/// Record the revision used to compute the capture plan along with its rows.
pub(crate) fn publish(
    job: &str,
    titles: &[libfreemkv::DiscTitle],
    label: &str,
    filename: &str,
    mapfile: &Path,
    outputs: &[super::staging::Output],
    initial: Correction,
) {
    let inputs = Inputs {
        titles: titles.to_vec(),
        label: label.into(),
        filename: filename.into(),
        mapfile: mapfile.into(),
    };
    PLANS.publish(job, inputs, outputs, initial);
}

/// Reconcile a correction acknowledged before the capture plan was published.
pub(crate) fn reconcile(cfg: &Config, device: &str, job: &str) {
    PLANS.reconcile(cfg, device, job);
}

/// Return cached rows immediately; mapfile I/O is confined to admitted workers.
pub(crate) fn episodes(job: &str) -> Option<Vec<CardEpisode>> {
    PLANS.episodes(job)
}

/// Carry the durable revision and its matching correction together.
pub(crate) fn replan(
    cfg: Config,
    job: &str,
    revision: u64,
    kind: MediaKind,
    tmdb_id: u64,
    start: Option<u16>,
) {
    PLANS.replan(
        job,
        Correction {
            cfg,
            revision,
            kind,
            tmdb_id,
            start,
        },
    );
}

/// The card rows for a plan: one per episode output, or none for a single feature.
pub(crate) fn card_episodes(
    titles: &[libfreemkv::DiscTitle],
    label: &str,
    outputs: &[super::staging::Output],
) -> Vec<CardEpisode> {
    if outputs.len() < 2 {
        return Vec::new();
    }
    let season = crate::server::tmdb::season_from_label(label).unwrap_or(1);
    // An output without its episode number makes it no episode plan.
    let rows: Option<Vec<_>> = outputs
        .iter()
        .map(|o| {
            Some(CardEpisode {
                season,
                episode: o.episode?,
                name: o.episode_name.clone(),
                duration: titles
                    .get(o.title_index)
                    .map(|t| crate::server::util::format_duration_hm(t.duration_secs))
                    .unwrap_or_default(),
                at_risk_ms: 0.0,
                title_index: o.title_index,
            })
        })
        .collect();
    rows.unwrap_or_default()
}

/// Playback time `damage` (mapfile byte ranges) covers in `title`, at the title's mean
/// rate: the main-title figure libfreemkv reports, for any title.
pub(crate) fn at_risk_ms(title: &libfreemkv::DiscTitle, damage: &[(u64, u64)]) -> f64 {
    if title.duration_secs <= 0.0 || title.size_bytes == 0 {
        return 0.0;
    }
    let bps = title.size_bytes as f64 / title.duration_secs;
    libfreemkv::disc::bytes_bad_in_title(title, damage) as f64 * 1000.0 / bps
}

#[cfg(test)]
#[path = "card_plan_tests.rs"]
mod tests;
