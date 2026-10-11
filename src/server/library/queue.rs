//! The remux queue, persisted as JSON in the config folder.
//!
//! FIFO, one job at a time. Every change uses a phase-aware durable write
//! (temp file, fsync, rename, folder fsync). A job found
//! running at load was cut off by a restart: it goes back to the head of the
//! queue, and its stale `.partial` is removed. The old MKV was never touched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::server::planner::RemuxPlan;

/// The queue file's name in the config folder.
pub const QUEUE_FILE: &str = "library-queue.json";
const SCHEMA: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
}

/// Why a job is back in the queue or was stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobNote {
    /// The daemon restarted while it ran.
    Restarted,
    /// A rip started and took the mux slot.
    Preempted,
    /// The daemon was shutting down.
    Interrupted,
    /// It made no progress for too long and was cancelled.
    Stalled,
    /// Stopped from the UI ("Stop all").
    Cancelled,
    /// Held: the output (or ISO) folder failed its check; it starts once the folder is back.
    WaitingForFolder,
    /// Muxed and verified on local staging, waiting for the output folder to copy it in.
    StagedWaiting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputState {
    Pending,
    Running,
    Done,
    Failed,
}

/// Durable delivery state for one output of an immutable remux plan.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutputProgress {
    pub id: String,
    pub target: PathBuf,
    pub state: OutputState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<PathBuf>,
    /// The new file as it was about to land: found at the target, it landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<crate::server::util::Stamp>,
}

/// One queued remux.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub id: u64,
    pub title: String,
    pub iso: PathBuf,
    pub target: PathBuf,
    /// Frozen source-to-output decisions. `None` is the legacy single-output
    /// movie job shape and is deliberately retained for schema compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<RemuxPlan>,
    /// Mutable execution state kept separately from the immutable plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputProgress>,
    /// The existing files this job may overwrite, captured once before its first output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned: Option<Vec<super::owned::Owned>>,
    /// A Change match: once every output is in place, its other owned files are removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retire: bool,
    /// An MKV existed at `target` when the job was queued and may be replaced.
    pub replace: bool,
    pub state: JobState,
    pub queued_at: u64,
    #[serde(default)]
    pub started_at: Option<u64>,
    #[serde(default)]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub note: Option<JobNote>,
    /// Why the job failed, kept on the job itself so the queue explains it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// Automatic retries after a storage fault so far.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_failed_at: Option<u64>,
    /// Not started again before this time (the backoff after a storage fault).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<u64>,
    /// The finished, verified MKV the engine kept on local staging: a retry only copies it in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<PathBuf>,
    /// Its size, and how many deliveries of it have failed so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_attempts: Option<u32>,
}

/// A finished file kept on local staging, as a job records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub attempts: u32,
}

impl Job {
    // Active, or retaining output that a correction of its source would orphan.
    fn holds_source(&self) -> bool {
        matches!(self.state, JobState::Queued | JobState::Running)
            || self.staged.is_some()
            || self.outputs.iter().any(|o| o.staged.is_some())
    }

    /// Whether `target` is this job's row target or one of its output targets.
    pub(crate) fn owns_target(&self, target: &Path) -> bool {
        self.target == target || self.outputs.iter().any(|o| o.target == target)
    }

    pub(crate) fn output_targets(&self) -> Vec<PathBuf> {
        if self.plan.is_some() && !self.outputs.is_empty() {
            return self.outputs.iter().map(|o| o.target.clone()).collect();
        }
        match &self.plan {
            Some(plan) => plan
                .outputs
                .iter()
                .map(|o| {
                    if plan.outputs.len() == 1 {
                        self.target.clone()
                    } else {
                        self.target
                            .parent()
                            .unwrap_or_else(|| Path::new("."))
                            .join(&o.filename)
                    }
                })
                .collect(),
            None => vec![self.target.clone()],
        }
    }

    fn set_staged(&mut self, staged: Option<StagedFile>) {
        // An output still pointing at the file the job lets go of would resume a dead path.
        if let Some(old) = self.staged.take()
            && staged.as_ref().is_none_or(|s| s.path != old)
        {
            for output in &mut self.outputs {
                if output.staged.as_ref() == Some(&old) {
                    output.staged = None;
                }
            }
        }
        self.staged_bytes = staged.as_ref().map(|s| s.bytes);
        self.staged_attempts = staged.as_ref().map(|s| s.attempts);
        self.staged = staged.map(|s| s.path);
    }

    // A queued job with a kept file leaves the queue as failed, so the file stays offered.
    fn park(&mut self, message: &str) {
        self.state = JobState::Failed;
        self.note = Some(JobNote::Cancelled);
        self.not_before = None;
        self.finished_at = Some(crate::server::util::epoch_secs());
        let code = self.failure.as_ref().and_then(|f| f.code);
        self.failure = Some(Failure {
            code,
            message: message.into(),
        });
    }
}

const GONE: &str = "The finished MKV kept on local staging is gone (it expired, or staging ran \
                    short of space). Queue a fresh remux.";
const PARKED: &str = "Taken out of the queue. The finished MKV is still kept on local staging: \
                      Retry copies it in, Discard deletes it.";

// Take the queued jobs `pick` names out of the queue; how many left it.
fn unqueue(f: &mut QueueFile, pick: impl Fn(&Job) -> bool) -> usize {
    let before = f.jobs.len();
    let mut parked = 0;
    f.jobs.retain_mut(|j| {
        if j.state != JobState::Queued || !pick(j) {
            return true;
        }
        if j.staged.is_none() {
            return false;
        }
        j.park(PARKED);
        parked += 1;
        true
    });
    before - f.jobs.len() + parked
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Job {
    /// Queued and past any backoff.
    pub fn ready(&self, now: u64) -> bool {
        self.state == JobState::Queued && self.not_before.is_none_or(|t| t <= now)
    }
}

/// A storage fault is retried for this long after the first one, then left failed.
pub const RETRY_GIVE_UP_SECS: u64 = 24 * 3600;
const RETRY_MAX_ATTEMPTS: u32 = 30;

/// The wait before retry number `attempt` (1-based): 1 min, 5 min, 15 min, then hourly.
pub fn retry_delay(attempt: u32) -> u64 {
    match attempt {
        0 | 1 => 60,
        2 => 5 * 60,
        3 => 15 * 60,
        _ => 3600,
    }
}

/// When `job`, just failed by a storage fault at `now`, may start again; `None` to give up.
/// `resolved` says a fresh check of its folders passed, so the first retry need not wait.
pub fn retry_at(job: &Job, now: u64, resolved: bool) -> Option<u64> {
    let first = job.first_failed_at.unwrap_or(now);
    let attempt = job.attempts + 1;
    if now.saturating_sub(first) >= RETRY_GIVE_UP_SECS || attempt > RETRY_MAX_ATTEMPTS {
        return None;
    }
    if resolved && job.attempts == 0 {
        return Some(now);
    }
    Some(now + retry_delay(attempt))
}

/// A failed job's reason: the library error code, when there is one, and the text.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Failure {
    pub code: Option<u16>,
    pub message: String,
}

/// What to queue.
#[derive(Clone, Debug)]
pub struct NewJob {
    pub title: String,
    pub iso: PathBuf,
    pub target: PathBuf,
    pub replace: bool,
}

/// The last outcome for a target, kept after the job is cleared from the queue.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum JobResult {
    Done {
        size_bytes: u64,
        secs: u64,
        finished_at: u64,
        writing_app: Option<String>,
    },
    Failed {
        /// The library error code, when the failure carried one.
        code: Option<u16>,
        message: String,
        finished_at: u64,
    },
}

/// Everything the queue file holds.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueueFile {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub debug_log: bool,
    #[serde(default)]
    pub jobs: Vec<Job>,
    /// Keyed by target path.
    #[serde(default)]
    pub results: BTreeMap<String, JobResult>,
}

impl QueueFile {
    pub fn running(&self) -> Option<&Job> {
        self.jobs.iter().find(|j| j.state == JobState::Running)
    }

    /// The job `claim_next` would take at `now`, ignoring pause and a running job.
    pub fn next_ready(&self, now: u64) -> Option<&Job> {
        self.jobs.iter().find(|j| j.ready(now))
    }

    pub fn count(&self, state: JobState) -> usize {
        self.jobs.iter().filter(|j| j.state == state).count()
    }

    /// The queued or running job for `target`, if any.
    pub fn active_for(&self, target: &Path) -> Option<&Job> {
        self.jobs.iter().find(|j| {
            j.owns_target(target) && matches!(j.state, JobState::Queued | JobState::Running)
        })
    }

    /// The latest job for `target` in any state.
    pub fn latest_for(&self, target: &Path) -> Option<&Job> {
        self.jobs.iter().rev().find(|j| j.owns_target(target))
    }
}

/// The engine writes its in-progress file beside the target.
pub fn partial_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    target.with_file_name(name)
}

/// The persistent queue.
pub struct Queue {
    path: PathBuf,
    state: Mutex<QueueFile>,
    // Serialises writers and remembers the newest generation on disk, so an
    // older snapshot can never land after a newer one.
    saved: Mutex<u64>,
    generation: AtomicU64,
    durability_pending: AtomicBool,
    // Set when the file on disk could not be loaded: it is never written over.
    unreadable: Option<String>,
}

// The queue file's bytes (`None`: there is none), tried a few times: a share that answers
// EIO or ESTALE for a moment must not cost the queue.
fn read_retrying(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    let mut tries = 0;
    loop {
        match std::fs::read(path) {
            Ok(bytes) => return Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) if tries >= 2 => return Err(e),
            Err(_) => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(200 * tries));
            }
        }
    }
}

// Unlike a plain io::Result, preserve whether rename committed the new bytes.
// Queue memory must follow publication even when the final directory sync fails.
fn write_queue(
    path: &Path,
    bytes: &[u8],
    sync_directory: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), (bool, std::io::Error)> {
    use crate::server::ripper::staging::{MarkerWriteError, write_marker_phased};
    write_marker_phased(path, bytes, sync_directory).map_err(|failure| match failure {
        MarkerWriteError::BeforePublish(error) => (false, error),
        MarkerWriteError::AfterPublish(error) => (true, error),
    })
}

fn write_queue_durable(path: &Path, bytes: &[u8]) -> Result<(), (bool, std::io::Error)> {
    write_queue(path, bytes, libfreemkv::io::fsync::dir_checked)
}

impl Queue {
    /// Keep admission and plan changes excluded until the orphan is unlinked.
    pub(crate) fn remove_orphan_partial(&self, path: &Path) -> bool {
        self.remove_orphan_partial_with(path, |p| std::fs::remove_file(p))
    }

    fn remove_orphan_partial_with(
        &self,
        path: &Path,
        unlink: impl FnOnce(&Path) -> std::io::Result<()>,
    ) -> bool {
        // Most candidates were never written; look outside the lock (the share may hang).
        if matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return false;
        }
        let f = self.lock();
        if f.jobs
            .iter()
            .filter(|j| j.state == JobState::Running)
            .flat_map(Job::output_targets)
            .any(|p| partial_path(&p) == path)
        {
            return false;
        }
        let removed = unlink(path).is_ok();
        drop(f);
        removed
    }

    /// Load (or start) the queue in `config_dir`, re-queueing a job a restart cut off. A
    /// damaged file is set aside under a name of its own and the queue starts empty; a file
    /// that cannot be read (after a few tries) is left as it is, and the queue holds
    /// without ever writing over it ([`Self::unreadable`]).
    pub fn open(config_dir: &Path) -> Self {
        let path = config_dir.join(QUEUE_FILE);
        let mut unreadable = None;
        let mut file = match read_retrying(&path) {
            Ok(Some(bytes)) => serde_json::from_slice::<QueueFile>(&bytes).unwrap_or_else(|e| {
                match super::set_aside(&path) {
                    Ok(aside) => {
                        tracing::warn!(path = %path.display(), aside = %aside.display(), error = %e, "library queue damaged; set aside, starting empty");
                    }
                    Err(aside_error) => {
                        tracing::error!(path = %path.display(), error = %aside_error, "damaged library queue could not be set aside");
                        unreadable = Some(format!(
                            "{} is damaged ({e}) and could not be set aside ({aside_error}); the remux queue is held and the file is left as it is.",
                            path.display()
                        ));
                    }
                }
                QueueFile::default()
            }),
            Ok(None) => QueueFile::default(),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "library queue unreadable; held, file left as it is");
                unreadable = Some(format!(
                    "{} cannot be read ({e}); the remux queue is held and the file is left as it is. Fix the config folder, then restart.",
                    path.display()
                ));
                QueueFile::default()
            }
        };
        file.schema = SCHEMA;
        for job in &mut file.jobs {
            if job.state == JobState::Running {
                job.state = JobState::Queued;
                job.note = Some(JobNote::Restarted);
            }
        }
        for job in file.jobs.iter().filter(|j| j.state == JobState::Queued) {
            for target in std::iter::once(job.target.clone()).chain(job.output_targets()) {
                let _ = std::fs::remove_file(partial_path(&target));
            }
        }
        let q = Self {
            path,
            state: Mutex::new(file),
            saved: Mutex::new(0),
            generation: AtomicU64::new(1),
            durability_pending: AtomicBool::new(true),
            unreadable,
        };
        q.persist_now();
        q
    }

    /// Why the queue file could not be loaded: nothing is claimed or written until a restart.
    pub fn unreadable(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }

    /// The queue file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bumped on every change; the UI refetches when it moves.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> QueueFile {
        self.lock().clone()
    }

    /// A job is running, or one waits and the queue is not paused.
    pub fn has_work(&self) -> bool {
        let f = self.lock();
        let now = crate::server::util::epoch_secs();
        f.running().is_some() || (!f.paused && f.next_ready(now).is_some())
    }

    /// The job that would start next, if the queue is free to start one.
    pub fn peek_next(&self) -> Option<Job> {
        let f = self.lock();
        if f.paused || f.running().is_some() {
            return None;
        }
        f.next_ready(crate::server::util::epoch_secs()).cloned()
    }

    /// Queue `items` in order. A target already queued or running is skipped, and so is one
    /// with a finished file kept on local staging (it is retried or discarded instead).
    pub fn add(&self, items: Vec<NewJob>) -> usize {
        self.mutate(|f| Self::append(f, items, true, false))
    }

    /// Queue `items` to be planned afresh at claim, never reusing an earlier plan, and
    /// not while another job of the same ISO is active or keeps a finished file.
    /// `retire` marks a Change match, whose old files go once every new one is in place.
    pub(crate) fn add_fresh(&self, items: Vec<NewJob>, retire: bool) -> usize {
        self.mutate(|f| Self::append(f, items, false, retire))
    }

    /// Exclude claims while saving a correction for an otherwise idle source.
    pub(crate) fn with_idle_source<T>(
        &self,
        source: &Path,
        save: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let state = self.lock();
        if state
            .jobs
            .iter()
            .any(|j| j.iso == source && j.holds_source())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "source has active or retained output; stop or resolve that job first",
            ));
        }
        let result = save();
        drop(state);
        result
    }

    fn append(
        f: &mut QueueFile,
        items: Vec<NewJob>,
        reuse_saved_plans: bool,
        retire: bool,
    ) -> usize {
        let now = crate::server::util::epoch_secs();
        // Indexed once, so a bulk add is linear in the queue rather than in queue x items:
        // each target's jobs in queue order, and the sources a job holds.
        let mut by_target: std::collections::HashMap<PathBuf, Vec<usize>> =
            std::collections::HashMap::new();
        let mut held: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for (i, j) in f.jobs.iter().enumerate() {
            for target in std::iter::once(&j.target).chain(j.outputs.iter().map(|o| &o.target)) {
                let at = by_target.entry(target.clone()).or_default();
                if at.last() != Some(&i) {
                    at.push(i);
                }
            }
            if j.holds_source() {
                held.insert(j.iso.clone());
            }
        }
        let existing = f.jobs.len();
        let mut replaced: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut added = Vec::new();
        for item in items {
            if !reuse_saved_plans && held.contains(&item.iso) {
                continue;
            }
            let job_at = |i: usize| {
                if i < existing {
                    &f.jobs[i]
                } else {
                    &added[i - existing]
                }
            };
            // Entries an earlier item of this batch replaces are gone already.
            let owners: Vec<usize> = by_target
                .get(&item.target)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&i| i >= existing || !replaced.contains(&f.jobs[i].target))
                .collect();
            let kept = owners.last().is_some_and(|&i| job_at(i).staged.is_some());
            let active = owners
                .iter()
                .any(|&i| matches!(job_at(i).state, JobState::Queued | JobState::Running));
            if kept || active {
                continue;
            }
            let prior = owners.iter().rev().find_map(|&i| {
                let j = job_at(i);
                if !reuse_saved_plans || j.iso != item.iso {
                    return None;
                }
                let output = j.outputs.iter().find(|o| o.target == item.target)?;
                let mut plan = j.plan.clone()?;
                plan.outputs.retain(|o| o.id == output.id);
                Some((
                    plan,
                    vec![OutputProgress {
                        id: output.id.clone(),
                        target: item.target.clone(),
                        state: OutputState::Pending,
                        staged: None,
                        landing: None,
                    }],
                ))
            });
            let (plan, outputs) = match prior {
                Some((p, o)) => (Some(p), o),
                None => (None, Vec::new()),
            };
            replaced.insert(item.target.clone());
            f.next_id += 1;
            let job = Job {
                id: f.next_id,
                title: item.title,
                iso: item.iso,
                target: item.target,
                plan,
                outputs,
                owned: None,
                retire,
                replace: item.replace,
                state: JobState::Queued,
                queued_at: now,
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
            };
            by_target
                .entry(job.target.clone())
                .or_default()
                .push(existing + added.len());
            held.insert(job.iso.clone());
            added.push(job);
        }
        // A new job replaces the finished entries that named its target.
        let n = added.len();
        f.jobs.retain(|j| !replaced.contains(&j.target));
        f.jobs.extend(added);
        n
    }

    /// Take the oldest queued job past its backoff and mark it running. `None` while
    /// paused. An idle queue is left alone: no generation bump, no write.
    pub fn claim_next(&self) -> Option<Job> {
        if self.unreadable.is_some() {
            return None;
        }
        if self.durability_pending.load(Ordering::SeqCst) {
            self.persist_now();
        }
        let now = crate::server::util::epoch_secs();
        {
            let f = self.lock();
            if self.durability_pending.load(Ordering::SeqCst)
                || f.paused
                || f.running().is_some()
                || f.next_ready(now).is_none()
            {
                return None;
            }
        }
        self.mutate(|f| {
            if self.durability_pending.load(Ordering::SeqCst) || f.paused || f.running().is_some() {
                return None;
            }
            let job = f.jobs.iter_mut().find(|j| j.ready(now))?;
            job.state = JobState::Running;
            job.failure = None;
            job.started_at = Some(crate::server::util::epoch_secs());
            job.finished_at = None;
            Some(job.clone())
        })
    }

    /// [`Self::set_plan_targets`] with targets derived from `job.target` (tests).
    #[cfg(test)]
    pub fn set_plan(&self, id: u64, plan: RemuxPlan) -> bool {
        self.set_plan_targets(id, plan, None).unwrap_or(false)
    }

    /// Attach the immutable plan produced for a claimed job before its first output, so
    /// a restart resumes the same source/output decisions instead of planning again.
    /// `None` targets derive from `job.target` (its folder for several outputs).
    pub(crate) fn set_plan_targets(
        &self,
        id: u64,
        plan: RemuxPlan,
        targets: Option<Vec<PathBuf>>,
    ) -> std::io::Result<bool> {
        self.mutate_durable(|f| {
            let Some(job) = f.jobs.iter_mut().find(|j| j.id == id) else {
                return false;
            };
            if job.plan.is_some() || job.staged.is_some() {
                return false;
            }
            if targets
                .as_ref()
                .is_some_and(|t| t.len() != plan.outputs.len())
            {
                return false;
            }
            let multi = plan.outputs.len() > 1;
            job.outputs = plan
                .outputs
                .iter()
                .enumerate()
                .map(|(index, output)| OutputProgress {
                    id: output.id.clone(),
                    target: if let Some(targets) = &targets {
                        targets[index].clone()
                    } else if multi {
                        job.target
                            .parent()
                            .unwrap_or_else(|| Path::new("."))
                            .join(&output.filename)
                    } else {
                        job.target.clone()
                    },
                    state: OutputState::Pending,
                    staged: None,
                    landing: None,
                })
                .collect();
            job.plan = Some(plan);
            true
        })
    }

    /// Durably record the files job `id` may overwrite; kept for every later run of it.
    pub(crate) fn set_owned(
        &self,
        id: u64,
        owned: Vec<super::owned::Owned>,
    ) -> std::io::Result<()> {
        let saved = self.mutate_durable(|f| {
            let Some(job) = f.jobs.iter_mut().find(|j| j.id == id) else {
                return false;
            };
            job.owned = Some(owned);
            true
        })?;
        if !saved {
            return Err(std::io::Error::other("remux job no longer exists"));
        }
        Ok(())
    }

    pub fn begin_output(&self, id: u64, output_id: &str) -> bool {
        self.mutate_if(|f| {
            let Some(output) = f
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .and_then(|j| j.outputs.iter_mut().find(|o| o.id == output_id))
            else {
                return (false, false);
            };
            if output.state == OutputState::Done {
                return (false, false);
            }
            let changed = output.state != OutputState::Running;
            output.state = OutputState::Running;
            (true, changed)
        })
    }

    pub fn finish_output(&self, id: u64, output_id: &str, state: OutputState) {
        self.mutate_if(|f| {
            let Some(output) = f
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .and_then(|j| j.outputs.iter_mut().find(|o| o.id == output_id))
            else {
                return ((), false);
            };
            let landing = if state == OutputState::Done {
                None
            } else {
                output.landing
            };
            let changed = output.state != state || output.landing != landing;
            output.state = state;
            output.landing = landing;
            ((), changed)
        });
    }

    /// Record the file output `output_id` of job `id` is about to land as, before it lands.
    pub(crate) fn set_output_landing(
        &self,
        id: u64,
        output_id: &str,
        landing: crate::server::util::Stamp,
    ) {
        self.mutate(|f| {
            if let Some(output) = f
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .and_then(|j| j.outputs.iter_mut().find(|o| o.id == output_id))
            {
                output.landing = Some(landing);
            }
        });
    }

    pub fn set_output_staged(&self, id: u64, output_id: &str, staged: Option<PathBuf>) {
        self.mutate_if(|f| {
            let Some(output) = f
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .and_then(|j| j.outputs.iter_mut().find(|o| o.id == output_id))
                .filter(|o| o.staged != staged)
            else {
                return ((), false);
            };
            output.staged = staged;
            ((), true)
        });
    }

    /// Record how job `id` ended.
    pub fn finish(&self, id: u64, result: JobResult) {
        self.mutate(|f| {
            let Some(job) = f.jobs.iter_mut().find(|j| j.id == id) else {
                return;
            };
            job.state = match result {
                JobResult::Done { .. } => JobState::Done,
                JobResult::Failed { .. } => JobState::Failed,
            };
            job.failure = match &result {
                JobResult::Failed { code, message, .. } => Some(Failure {
                    code: *code,
                    message: message.clone(),
                }),
                JobResult::Done { .. } => None,
            };
            job.finished_at = Some(crate::server::util::epoch_secs());
            if matches!(result, JobResult::Done { .. }) {
                job.set_staged(None);
            }
            let key = job.target.to_string_lossy().into_owned();
            f.results.insert(key, result);
        });
    }

    /// Put a running job back at the head of the queue.
    pub fn requeue(&self, id: u64, note: JobNote) {
        self.mutate(|f| {
            let Some(pos) = f.jobs.iter().position(|j| j.id == id) else {
                return;
            };
            let mut job = f.jobs.remove(pos);
            job.state = JobState::Queued;
            job.started_at = None;
            job.note = Some(note);
            let head = f
                .jobs
                .iter()
                .position(|j| j.state == JobState::Queued)
                .unwrap_or(f.jobs.len());
            f.jobs.insert(head, job);
        });
    }

    /// Put a job a storage fault stopped back at the head of the queue, to start no earlier
    /// than `not_before` and only once its folder checks out. With `staged` (a finished file
    /// kept on local staging) it waits as `StagedWaiting`, else as `WaitingForFolder`.
    pub fn retry_later(
        &self,
        id: u64,
        failure: Failure,
        not_before: u64,
        staged: Option<StagedFile>,
    ) {
        let now = crate::server::util::epoch_secs();
        self.mutate(|f| {
            let Some(pos) = f.jobs.iter().position(|j| j.id == id) else {
                return;
            };
            let mut job = f.jobs.remove(pos);
            job.state = JobState::Queued;
            job.started_at = None;
            job.note = Some(if staged.is_some() {
                JobNote::StagedWaiting
            } else {
                JobNote::WaitingForFolder
            });
            job.failure = Some(failure);
            job.attempts += 1;
            job.first_failed_at.get_or_insert(now);
            job.not_before = Some(not_before);
            job.set_staged(staged);
            let head = f
                .jobs
                .iter()
                .position(|j| j.state == JobState::Queued)
                .unwrap_or(f.jobs.len());
            f.jobs.insert(head, job);
        });
    }

    /// Record the finished file job `id` keeps on local staging (`None`: it has none).
    pub fn set_staged(&self, id: u64, staged: Option<StagedFile>) {
        self.mutate_if(|f| {
            let Some(j) = f.jobs.iter_mut().find(|j| j.id == id) else {
                return ((), false);
            };
            let before = j.clone();
            j.set_staged(staged);
            ((), *j != before)
        });
    }

    /// Queue the kept file for `target` to be copied in now, at the head of the queue, with a
    /// fresh series of automatic retries. False when there is none, or it is running.
    pub fn retry_staged_now(&self, target: &Path) -> bool {
        self.mutate(|f| {
            let Some(pos) = f.jobs.iter().rposition(|j| {
                j.owns_target(target) && j.staged.is_some() && j.state != JobState::Running
            }) else {
                return false;
            };
            let mut job = f.jobs.remove(pos);
            job.state = JobState::Queued;
            job.note = Some(JobNote::StagedWaiting);
            job.started_at = None;
            job.finished_at = None;
            job.not_before = None;
            job.attempts = 0;
            job.first_failed_at = None;
            let head = f
                .jobs
                .iter()
                .position(|j| j.state == JobState::Queued)
                .unwrap_or(f.jobs.len());
            f.jobs.insert(head, job);
            true
        })
    }

    /// Detach the kept file of `target`'s job so it can be deleted: a queued job leaves the
    /// queue, a failed one keeps its failure. `Err(true)` while that job runs, `Err(false)`
    /// when there is no kept file.
    pub fn take_staged(&self, target: &Path) -> Result<PathBuf, bool> {
        self.mutate(|f| {
            let pos = f
                .jobs
                .iter()
                .rposition(|j| j.owns_target(target) && j.staged.is_some())
                .ok_or(false)?;
            if f.jobs[pos].state == JobState::Running {
                return Err(true);
            }
            let path = f.jobs[pos].staged.clone().ok_or(false)?;
            for output in &mut f.jobs[pos].outputs {
                if output.staged.as_ref() == Some(&path) {
                    output.staged = None;
                }
            }
            if f.jobs[pos].state == JobState::Queued {
                f.jobs.remove(pos);
            } else {
                f.jobs[pos].set_staged(None);
            }
            Ok(path)
        })
    }

    /// Match the queue to the kept files found on local staging at startup: each one is
    /// a job waiting as `StagedWaiting` (its own, another job for that target, or a new one),
    /// and a job whose kept file is gone forgets it. Returns how many jobs were added.
    pub fn adopt_staged(&self, kept: Vec<(NewJob, StagedFile, Failure)>) -> usize {
        let now = crate::server::util::epoch_secs();
        self.mutate(|f| {
            let found: std::collections::HashSet<PathBuf> =
                kept.iter().map(|(_, s, _)| s.path.clone()).collect();
            for j in f.jobs.iter_mut() {
                for output in &mut j.outputs {
                    if output.staged.as_ref().is_some_and(|p| !found.contains(p)) {
                        output.staged = None;
                    }
                }
                if j.staged.as_ref().is_some_and(|p| !found.contains(p)) {
                    j.set_staged(None);
                    if j.state == JobState::Queued {
                        j.park(GONE);
                    }
                }
            }
            let mut added = 0;
            for (item, staged, failure) in kept {
                if let Some((j, at)) = f.jobs.iter_mut().rev().find_map(|j| {
                    let at = j.outputs.iter().position(|o| o.target == item.target)?;
                    (j.iso == item.iso).then_some((j, at))
                }) {
                    if j.state != JobState::Running {
                        let output = &mut j.outputs[at];
                        output.staged = Some(staged.path.clone());
                        output.state = OutputState::Pending;
                        j.state = JobState::Queued;
                        j.not_before = None;
                        j.finished_at = None;
                        j.note = Some(JobNote::StagedWaiting);
                        j.failure.get_or_insert(failure);
                        j.set_staged(Some(staged));
                    }
                    continue;
                }
                if let Some(j) = f
                    .jobs
                    .iter_mut()
                    .rev()
                    .find(|j| j.target == item.target && j.state != JobState::Running)
                {
                    if j.state != JobState::Queued {
                        j.state = JobState::Queued;
                        j.not_before = None;
                        j.finished_at = None;
                    }
                    j.note = Some(JobNote::StagedWaiting);
                    j.failure.get_or_insert(failure);
                    j.set_staged(Some(staged));
                    continue;
                }
                f.next_id += 1;
                f.jobs.push(Job {
                    id: f.next_id,
                    title: item.title,
                    iso: item.iso,
                    target: item.target,
                    plan: None,
                    outputs: Vec::new(),
                    owned: None,
                    retire: false,
                    replace: item.replace,
                    state: JobState::Queued,
                    queued_at: now,
                    started_at: None,
                    finished_at: None,
                    note: Some(JobNote::StagedWaiting),
                    failure: Some(failure),
                    attempts: 0,
                    first_failed_at: None,
                    not_before: None,
                    staged: Some(staged.path),
                    staged_bytes: Some(staged.bytes),
                    staged_attempts: Some(staged.attempts),
                });
                added += 1;
            }
            added
        })
    }

    /// The finished files kept on local staging the queue knows of: how many, and their bytes.
    pub fn staged_total(&self) -> (usize, u64) {
        let f = self.lock();
        let kept = f.jobs.iter().filter(|j| j.staged.is_some());
        (
            kept.clone().count(),
            kept.filter_map(|j| j.staged_bytes).sum(),
        )
    }

    /// Mark the running job (it keeps running until the engine returns).
    pub fn note_running(&self, id: u64, note: JobNote) {
        self.mutate(|f| {
            if let Some(j) = f.jobs.iter_mut().find(|j| j.id == id) {
                j.note = Some(note);
            }
        });
    }

    /// Drop done and failed jobs from the list; their per-title results stay.
    pub fn clear_finished(&self) -> usize {
        self.mutate(|f| {
            let before = f.jobs.len();
            f.jobs
                .retain(|j| matches!(j.state, JobState::Queued | JobState::Running));
            before - f.jobs.len()
        })
    }

    /// Drop the queued (not running) job for `target`. Returns how many went. A job with a
    /// kept file stays listed as failed, so the file is still offered.
    pub fn remove_queued(&self, target: &Path) -> usize {
        self.mutate(|f| unqueue(f, |j| j.owns_target(target)))
    }

    /// Drop every queued job and un-pause: an empty queue has nothing to
    /// hold, so it never shows as paused. The running job carries on.
    pub fn clear_queued(&self) -> usize {
        self.mutate(|f| {
            f.paused = false;
            unqueue(f, |_| true)
        })
    }

    /// Remove a job.
    pub fn drop_job(&self, id: u64) {
        self.mutate(|f| f.jobs.retain(|j| j.id != id));
    }

    pub fn set_paused(&self, paused: bool) {
        self.mutate(|f| f.paused = paused);
    }

    pub fn set_debug_log(&self, on: bool) {
        self.mutate(|f| f.debug_log = on);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueFile> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    // [`Self::mutate`] for a change that may turn out to be none: `f` says whether it changed
    // anything, and nothing is bumped or written when it did not.
    fn mutate_if<T>(&self, f: impl FnOnce(&mut QueueFile) -> (T, bool)) -> T {
        let (out, changed) = {
            let mut g = self.lock();
            let (out, changed) = f(&mut g);
            if changed {
                self.generation.fetch_add(1, Ordering::SeqCst);
            }
            (out, changed)
        };
        if changed {
            self.persist_now();
        }
        out
    }

    fn mutate<T>(&self, f: impl FnOnce(&mut QueueFile) -> T) -> T {
        let out = {
            let mut g = self.lock();
            let out = f(&mut g);
            self.generation.fetch_add(1, Ordering::SeqCst);
            out
        };
        self.persist_now();
        out
    }

    /// A landed update remains visible on sync failure, but cannot be claimed
    /// until persistence succeeds. An error does not promise rollback.
    fn mutate_durable<T>(&self, f: impl FnOnce(&mut QueueFile) -> T) -> std::io::Result<T> {
        self.try_mutate_durable(|state| Ok(f(state)))
    }

    fn try_mutate_durable<T>(
        &self,
        f: impl FnOnce(&mut QueueFile) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        self.try_mutate_durable_with(f, write_queue_durable)
    }

    fn try_mutate_durable_with<T>(
        &self,
        f: impl FnOnce(&mut QueueFile) -> std::io::Result<T>,
        write: impl FnOnce(&Path, &[u8]) -> Result<(), (bool, std::io::Error)>,
    ) -> std::io::Result<T> {
        if let Some(why) = &self.unreadable {
            return Err(std::io::Error::other(why.clone()));
        }
        let mut state = self.lock();
        let mut next = state.clone();
        let out = f(&mut next)?;
        if next == *state && !self.durability_pending.load(Ordering::SeqCst) {
            return Ok(out);
        }
        let json = serde_json::to_vec_pretty(&next).map_err(std::io::Error::other)?;
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        let result = write(&self.path, &json);
        if let Err((false, error)) = result {
            return Err(error);
        }
        *state = next;
        *saved = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.durability_pending
            .store(result.is_err(), Ordering::SeqCst);
        if let Err((_, error)) = result {
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "queue update published but directory durability unconfirmed; intent retained: {error}"
                ),
            ));
        }
        Ok(out)
    }

    // Snapshot under the state lock, write outside it (the config folder may be NFS).
    fn persist_now(&self) {
        let (generation, json) = {
            let g = self.lock();
            (self.generation(), serde_json::to_vec_pretty(&*g))
        };
        let Ok(json) = json else { return };
        self.persist_snapshot(generation, &json, write_queue_durable);
    }

    fn persist_snapshot(
        &self,
        generation: u64,
        json: &[u8],
        write: impl FnOnce(&Path, &[u8]) -> Result<(), (bool, std::io::Error)>,
    ) {
        if self.unreadable.is_some() {
            return;
        }
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        if generation < *saved
            || (generation == *saved && !self.durability_pending.load(Ordering::SeqCst))
        {
            return;
        }
        match write(&self.path, json) {
            Ok(()) => {
                *saved = generation;
                self.durability_pending.store(false, Ordering::SeqCst);
            }
            Err((published, e)) => {
                if published {
                    *saved = generation;
                }
                self.durability_pending.store(true, Ordering::SeqCst);
                tracing::warn!(path = %self.path.display(), error = %e, "library queue write failed")
            }
        }
    }
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
