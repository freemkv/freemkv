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
    /// User identity captured at enqueue, independent of subsequent lookup state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_match: Option<super::matches::SavedMatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<super::replacement::Replacement>,
    /// Mutable execution state kept separately from the immutable plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<OutputProgress>,
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
    fn has_replacement_work(&self) -> bool {
        self.replacement.as_ref().is_some_and(|r| r.has_work())
    }

    fn artifact_targets(&self) -> Vec<PathBuf> {
        let mut paths = self.output_targets();
        if let Some(replacement) = &self.replacement {
            paths.extend(replacement.candidates().map(Path::to_path_buf));
        }
        paths
    }
    fn owns_target(&self, target: &Path) -> bool {
        self.target == target
            || self.outputs.iter().any(|o| o.target == target)
            || self
                .replacement
                .as_ref()
                .is_some_and(|r| r.candidates().any(|p| p == target))
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
        if j.staged.is_none() && !j.has_replacement_work() {
            return false;
        }
        j.park(if j.has_replacement_work() {
            "Replacement stopped. Retry resumes the saved output transaction."
        } else {
            PARKED
        });
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
        let f = self.lock();
        if f.jobs
            .iter()
            .filter(|j| j.state == JobState::Running)
            .flat_map(Job::artifact_targets)
            .any(|p| partial_path(&p) == path)
        {
            return false;
        }
        let removed = unlink(path).is_ok();
        drop(f);
        removed
    }

    /// Load (or start) the queue in `config_dir`, re-queueing a job a restart cut off.
    pub fn open(config_dir: &Path) -> Self {
        let path = config_dir.join(QUEUE_FILE);
        let mut file = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<QueueFile>(&bytes).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "library queue unreadable; starting empty");
                let _ = std::fs::rename(&path, path.with_extension("json.unreadable"));
                QueueFile::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => QueueFile::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "library queue unreadable; starting empty");
                let _ = std::fs::rename(&path, path.with_extension("json.unreadable"));
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
            for target in std::iter::once(job.target.clone()).chain(job.artifact_targets()) {
                let _ = std::fs::remove_file(partial_path(&target));
            }
        }
        let q = Self {
            path,
            state: Mutex::new(file),
            saved: Mutex::new(0),
            generation: AtomicU64::new(1),
            durability_pending: AtomicBool::new(true),
        };
        q.persist_now();
        q
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
        self.mutate(|f| Self::append(f, items, true, None, None))
    }

    /// Start new planning after an identity correction, never reuse an episode plan.
    #[cfg(test)]
    pub fn add_replanned(&self, items: Vec<NewJob>) -> usize {
        self.mutate(|f| Self::append(f, items, false, None, None))
    }

    pub(crate) fn add_corrected(
        &self,
        item: NewJob,
        selected: super::matches::SavedMatch,
    ) -> std::io::Result<usize> {
        self.add_corrected_inner(item, selected, None)
    }

    pub(crate) fn add_corrected_authorized(
        &self,
        item: NewJob,
        selected: super::matches::SavedMatch,
        frozen: super::replacement::Replacement,
    ) -> std::io::Result<usize> {
        frozen.verify_admission(&item.iso)?;
        self.add_corrected_inner(item, selected, Some(frozen))
    }

    fn add_corrected_inner(
        &self,
        item: NewJob,
        selected: super::matches::SavedMatch,
        frozen: Option<super::replacement::Replacement>,
    ) -> std::io::Result<usize> {
        let config_dir = self
            .path
            .parent()
            .ok_or_else(|| std::io::Error::other("queue has no config directory"))?;
        self.try_mutate_durable(|f| {
            if f.jobs.iter().any(|j| {
                j.iso == item.iso && matches!(j.state, JobState::Queued | JobState::Running)
            }) {
                return Ok(0);
            }
            if let Some(job) = f
                .jobs
                .iter_mut()
                .rev()
                .find(|j| j.iso == item.iso && j.has_replacement_work())
            {
                if frozen.is_some() {
                    return Err(std::io::Error::other(
                        "unfinished replacement must retain its original authorization",
                    ));
                }
                if job.selected_match.as_ref() != Some(&selected) {
                    return Err(std::io::Error::other(
                        "unfinished replacement uses a different match",
                    ));
                }
                job.state = JobState::Queued;
                job.note = None;
                job.failure = None;
                job.finished_at = None;
                job.not_before = None;
                return Ok(1);
            }
            let replacement = match frozen {
                Some(frozen) => frozen,
                None => super::replacement::Replacement::capture(
                    &item.iso,
                    &super::links::read(config_dir)?,
                )?,
            };
            Ok(Self::append(
                f,
                vec![item],
                false,
                Some(selected),
                Some(replacement),
            ))
        })
    }

    /// Exclude claims while saving a correction for an otherwise idle source.
    pub(crate) fn with_idle_source<T>(
        &self,
        source: &Path,
        save: impl FnOnce() -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let state = self.lock();
        if state.jobs.iter().any(|j| {
            j.iso == source
                && (matches!(j.state, JobState::Queued | JobState::Running)
                    || j.staged.is_some()
                    || j.has_replacement_work()
                    || j.outputs.iter().any(|o| o.staged.is_some()))
        }) {
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
        selected_match: Option<super::matches::SavedMatch>,
        replacement: Option<super::replacement::Replacement>,
    ) -> usize {
        let mut n = 0;
        let now = crate::server::util::epoch_secs();
        for item in items {
            if !reuse_saved_plans
                && f.jobs.iter().any(|j| {
                    j.iso == item.iso
                        && (matches!(j.state, JobState::Queued | JobState::Running)
                            || j.staged.is_some()
                            || j.outputs.iter().any(|o| o.staged.is_some()))
                })
            {
                continue;
            }
            let kept = f
                .latest_for(&item.target)
                .is_some_and(|j| j.staged.is_some() || j.has_replacement_work());
            if f.jobs
                .iter()
                .any(|j| j.iso == item.iso && j.has_replacement_work())
            {
                continue;
            }
            if kept || f.active_for(&item.target).is_some() {
                continue;
            }
            let prior = f.jobs.iter().rev().find_map(|j| {
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
                    }],
                ))
            });
            let (plan, outputs) = match prior {
                Some((p, o)) => (Some(p), o),
                None => (None, Vec::new()),
            };
            f.jobs.retain(|j| j.target != item.target);
            f.next_id += 1;
            f.jobs.push(Job {
                id: f.next_id,
                title: item.title,
                iso: item.iso,
                target: item.target,
                plan,
                selected_match: selected_match.clone(),
                replacement: replacement.clone(),
                outputs,
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
            });
            n += 1;
        }
        n
    }

    /// Take the oldest queued job past its backoff and mark it running. `None` while
    /// paused. An idle queue is left alone: no generation bump, no write.
    pub fn claim_next(&self) -> Option<Job> {
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

    /// Attach the immutable plan produced for a claimed job. The worker calls
    /// this before the first output so a restart can resume from the same
    /// source/output decisions instead of planning again.
    pub fn set_plan(&self, id: u64, plan: RemuxPlan) -> bool {
        self.set_plan_targets(id, plan, None)
    }

    pub(crate) fn save_replacement(
        &self,
        id: u64,
        replacement: super::replacement::Replacement,
    ) -> std::io::Result<()> {
        let saved = self.mutate_durable(|f| {
            let Some(job) = f.jobs.iter_mut().find(|j| j.id == id) else {
                return false;
            };
            if job.replacement.is_none() {
                return false;
            }
            job.replacement = Some(replacement);
            true
        })?;
        if !saved {
            return Err(std::io::Error::other("replacement job disappeared"));
        }
        Ok(())
    }

    pub(crate) fn set_plan_targets(
        &self,
        id: u64,
        plan: RemuxPlan,
        targets: Option<Vec<PathBuf>>,
    ) -> bool {
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
                })
                .collect();
            job.plan = Some(plan);
            true
        })
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "remux plan was not durably saved");
            false
        })
    }

    pub fn begin_output(&self, id: u64, output_id: &str) -> bool {
        self.mutate(|f| {
            let Some(output) = f
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .and_then(|j| j.outputs.iter_mut().find(|o| o.id == output_id))
            else {
                return false;
            };
            if output.state == OutputState::Done {
                return false;
            }
            output.state = OutputState::Running;
            true
        })
    }

    pub fn finish_output(&self, id: u64, output_id: &str, state: OutputState) {
        self.mutate(|f| {
            if let Some(output) = f
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .and_then(|j| j.outputs.iter_mut().find(|o| o.id == output_id))
            {
                output.state = state;
            }
        });
    }

    pub fn set_output_staged(&self, id: u64, output_id: &str, staged: Option<PathBuf>) {
        self.mutate(|f| {
            if let Some(output) = f
                .jobs
                .iter_mut()
                .find(|j| j.id == id)
                .and_then(|j| j.outputs.iter_mut().find(|o| o.id == output_id))
            {
                output.staged = staged;
            }
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
        self.mutate(|f| {
            if let Some(j) = f.jobs.iter_mut().find(|j| j.id == id) {
                j.set_staged(staged);
            }
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
                if let Some(j) = f.jobs.iter_mut().rev().find(|j| {
                    j.iso == item.iso && j.outputs.iter().any(|o| o.target == item.target)
                }) {
                    if j.state != JobState::Running {
                        let output = j
                            .outputs
                            .iter_mut()
                            .find(|o| o.target == item.target)
                            .unwrap();
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
                    selected_match: None,
                    replacement: None,
                    outputs: Vec::new(),
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
            f.jobs.retain(|j| {
                matches!(j.state, JobState::Queued | JobState::Running) || j.has_replacement_work()
            });
            before - f.jobs.len()
        })
    }

    /// Drop the queued (not running) job for `target`. Returns how many went. A job with a
    /// kept file stays listed as failed, so the file is still offered.
    pub fn remove_queued(&self, target: &Path) -> usize {
        self.mutate(|f| unqueue(f, |j| j.target == target))
    }

    /// Drop every queued job and un-pause: an empty queue has nothing to
    /// hold, so it never shows as paused. The running job carries on.
    pub fn clear_queued(&self) -> usize {
        self.mutate(|f| {
            f.paused = false;
            unqueue(f, |_| true)
        })
    }

    /// Remove a job, retaining a stopped replacement until its transaction resolves.
    pub fn drop_job(&self, id: u64) {
        self.mutate(|f| {
            f.jobs.retain_mut(|j| {
                if j.id != id {
                    return true;
                }
                if j.has_replacement_work() {
                    j.park("Replacement stopped. Retry resumes the saved output transaction.");
                    return true;
                }
                false
            })
        });
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
