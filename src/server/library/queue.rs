//! The remux queue, persisted as JSON in the config folder.
//!
//! FIFO, one job at a time. Every change is written through the daemon's
//! durable write (temp file, fsync, rename, folder fsync). A job found
//! running at load was cut off by a restart: it goes back to the head of the
//! queue, and its stale `.partial` is removed. The old MKV was never touched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The queue file's name in the config folder.
pub const QUEUE_FILE: &str = "library-queue.json";
const SCHEMA: u32 = 1;

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
    /// The hook for keeping a finished staged file: nothing sets it until the engine can
    /// hand that file back, so today a job only ever waits as `WaitingForFolder`.
    StagedWaiting,
}

/// One queued remux.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Job {
    pub id: u64,
    pub title: String,
    pub iso: PathBuf,
    pub target: PathBuf,
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
    /// With `StagedWaiting`: the finished file on local staging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<PathBuf>,
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
        self.jobs
            .iter()
            .find(|j| j.target == target && matches!(j.state, JobState::Queued | JobState::Running))
    }

    /// The latest job for `target` in any state.
    pub fn latest_for(&self, target: &Path) -> Option<&Job> {
        self.jobs.iter().rev().find(|j| j.target == target)
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
}

impl Queue {
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
            let _ = std::fs::remove_file(partial_path(&job.target));
        }
        let q = Self {
            path,
            state: Mutex::new(file),
            saved: Mutex::new(0),
            generation: AtomicU64::new(1),
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

    /// Queue `items` in order. A target already queued or running is skipped.
    pub fn add(&self, items: Vec<NewJob>) -> usize {
        self.mutate(|f| {
            let mut n = 0;
            let now = crate::server::util::epoch_secs();
            for item in items {
                if f.active_for(&item.target).is_some() {
                    continue;
                }
                f.jobs.retain(|j| j.target != item.target);
                f.next_id += 1;
                f.jobs.push(Job {
                    id: f.next_id,
                    title: item.title,
                    iso: item.iso,
                    target: item.target,
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
                });
                n += 1;
            }
            n
        })
    }

    /// Take the oldest queued job past its backoff and mark it running. `None` while
    /// paused. An idle queue is left alone: no generation bump, no write.
    pub fn claim_next(&self) -> Option<Job> {
        let now = crate::server::util::epoch_secs();
        {
            let f = self.lock();
            if f.paused || f.running().is_some() || f.next_ready(now).is_none() {
                return None;
            }
        }
        self.mutate(|f| {
            if f.paused || f.running().is_some() {
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
    /// than `not_before` and only once its folder checks out. `staged` is the hook for a
    /// finished file kept on local staging (`StagedWaiting`); `None` waits as `WaitingForFolder`.
    pub fn retry_later(&self, id: u64, failure: Failure, not_before: u64, staged: Option<PathBuf>) {
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
            job.staged = staged;
            let head = f
                .jobs
                .iter()
                .position(|j| j.state == JobState::Queued)
                .unwrap_or(f.jobs.len());
            f.jobs.insert(head, job);
        });
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

    /// Drop the queued (not running) job for `target`. Returns how many went.
    pub fn remove_queued(&self, target: &Path) -> usize {
        self.mutate(|f| {
            let before = f.jobs.len();
            f.jobs
                .retain(|j| !(j.target == target && j.state == JobState::Queued));
            before - f.jobs.len()
        })
    }

    /// Drop every queued job and un-pause: an empty queue has nothing to
    /// hold, so it never shows as paused. The running job carries on.
    pub fn clear_queued(&self) -> usize {
        self.mutate(|f| {
            let before = f.jobs.len();
            f.jobs.retain(|j| j.state != JobState::Queued);
            f.paused = false;
            before - f.jobs.len()
        })
    }

    /// Remove job `id` outright (a cancelled job leaves no trace in the list).
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

    // Snapshot under the state lock, write outside it (the config folder may be NFS).
    fn persist_now(&self) {
        let (generation, json) = {
            let g = self.lock();
            (self.generation(), serde_json::to_vec_pretty(&*g))
        };
        let Ok(json) = json else { return };
        let mut saved = self.saved.lock().unwrap_or_else(|e| e.into_inner());
        if generation <= *saved {
            return;
        }
        match crate::server::ripper::staging::write_marker_durable(&self.path, &json) {
            Ok(()) => *saved = generation,
            Err(e) => {
                tracing::warn!(path = %self.path.display(), error = %e, "library queue write failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(dir: &Path, name: &str) -> NewJob {
        NewJob {
            title: name.into(),
            iso: dir.join(format!("{name}.iso")),
            target: dir.join(name).join(format!("{name}.mkv")),
            replace: true,
        }
    }

    #[test]
    fn jobs_run_in_order_one_at_a_time_and_pause() {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        assert_eq!(q.add(vec![job(t.path(), "a"), job(t.path(), "b")]), 2);
        assert_eq!(q.add(vec![job(t.path(), "a")]), 0, "already queued");
        let a = q.claim_next().unwrap();
        assert_eq!(a.title, "a");
        assert!(q.claim_next().is_none(), "one at a time");
        q.finish(
            a.id,
            JobResult::Failed {
                code: Some(6000),
                message: "E6000".into(),
                finished_at: 1,
            },
        );
        q.set_paused(true);
        assert!(q.claim_next().is_none());
        q.set_paused(false);
        assert_eq!(q.claim_next().unwrap().title, "b");
        // A finished target can be queued again; it replaces the old entry.
        assert_eq!(q.add(vec![job(t.path(), "a")]), 1);
        assert_eq!(
            q.snapshot().jobs.iter().filter(|j| j.title == "a").count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_queue_file_that_cannot_be_read_is_kept() {
        use std::os::unix::fs::PermissionsExt as _;
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join(QUEUE_FILE);
        std::fs::write(&file, br#"{"jobs":[]}"#).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o0)).unwrap();
        if std::fs::read(&file).is_ok() {
            return; // root reads anything
        }
        Queue::open(t.path());
        let kept = t.path().join("library-queue.json.unreadable");
        std::fs::set_permissions(&kept, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(&kept).unwrap(), br#"{"jobs":[]}"#);
    }

    #[test]
    fn has_work_is_a_running_job_or_a_waiting_one_unpaused() {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        assert!(!q.has_work());
        q.add(vec![job(t.path(), "a")]);
        assert!(q.has_work());
        q.set_paused(true);
        assert!(!q.has_work());
        q.set_paused(false);
        q.claim_next().unwrap();
        q.set_paused(true);
        assert!(q.has_work(), "a running job still holds the disks");
    }

    #[test]
    fn a_restart_requeues_the_running_job_first_and_keeps_results() {
        let t = tempfile::tempdir().unwrap();
        let target_b;
        {
            let q = Queue::open(t.path());
            q.add(vec![
                job(t.path(), "a"),
                job(t.path(), "b"),
                job(t.path(), "c"),
            ]);
            let a = q.claim_next().unwrap();
            q.finish(
                a.id,
                JobResult::Done {
                    size_bytes: 42,
                    secs: 3,
                    finished_at: 9,
                    writing_app: Some("freemkv 1.8.0".into()),
                },
            );
            let b = q.claim_next().unwrap();
            target_b = b.target.clone();
            std::fs::create_dir_all(b.target.parent().unwrap()).unwrap();
            std::fs::write(&b.target, b"old").unwrap();
            std::fs::write(partial_path(&b.target), b"half").unwrap();
            q.set_debug_log(true);
            // Dropped here while `b` is running: the process died.
        }
        let q = Queue::open(t.path());
        let f = q.snapshot();
        assert!(f.debug_log);
        let b = f.jobs.iter().find(|j| j.title == "b").unwrap();
        assert_eq!(b.state, JobState::Queued);
        assert_eq!(b.note, Some(JobNote::Restarted));
        assert!(!partial_path(&target_b).exists(), "stale partial cleaned");
        assert_eq!(
            std::fs::read(&target_b).unwrap(),
            b"old",
            "old MKV untouched"
        );
        assert!(matches!(
            f.results.values().next(),
            Some(JobResult::Done { size_bytes: 42, .. })
        ));
        assert_eq!(
            q.claim_next().unwrap().title,
            "b",
            "the cut-off job goes first"
        );
        assert_eq!(q.clear_finished(), 1);
        assert_eq!(
            q.snapshot().results.len(),
            1,
            "results outlive the cleared job"
        );
    }

    #[test]
    fn a_failure_is_kept_on_the_job_and_survives_a_restart() {
        let t = tempfile::tempdir().unwrap();
        {
            let q = Queue::open(t.path());
            q.add(vec![job(t.path(), "a")]);
            let a = q.claim_next().unwrap();
            q.finish(
                a.id,
                JobResult::Failed {
                    code: Some(7013),
                    message: "E7013 Decryption failed".into(),
                    finished_at: 1,
                },
            );
        }
        let q = Queue::open(t.path());
        let f = q.snapshot();
        assert_eq!(
            f.jobs[0].failure,
            Some(Failure {
                code: Some(7013),
                message: "E7013 Decryption failed".into()
            })
        );
        // A retry clears it while it runs.
        q.add(vec![job(t.path(), "a")]);
        assert_eq!(q.claim_next().unwrap().failure, None);
    }

    #[test]
    fn an_idle_queue_is_not_rewritten_by_polling() {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        let file = t.path().join(QUEUE_FILE);
        let (g, m) = (
            q.generation(),
            std::fs::metadata(&file).unwrap().modified().unwrap(),
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        for _ in 0..5 {
            assert!(q.claim_next().is_none());
        }
        assert_eq!(q.generation(), g, "no change, no generation bump");
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            m,
            "no write"
        );
        q.set_paused(true);
        q.add(vec![job(t.path(), "a")]);
        let g = q.generation();
        assert!(q.claim_next().is_none());
        assert_eq!(q.generation(), g, "a paused queue is not rewritten either");
    }

    #[test]
    fn a_preempted_job_goes_back_to_the_head() {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        q.add(vec![job(t.path(), "a"), job(t.path(), "b")]);
        let a = q.claim_next().unwrap();
        q.add(vec![job(t.path(), "c")]);
        q.requeue(a.id, JobNote::Preempted);
        assert_eq!(q.claim_next().unwrap().title, "a");
    }

    #[test]
    fn an_unreadable_queue_file_is_set_aside() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(QUEUE_FILE), b"{ not json").unwrap();
        let q = Queue::open(t.path());
        assert!(q.snapshot().jobs.is_empty());
        assert!(t.path().join("library-queue.json.unreadable").exists());
        let back: QueueFile =
            serde_json::from_slice(&std::fs::read(t.path().join(QUEUE_FILE)).unwrap()).unwrap();
        assert_eq!(back.schema, SCHEMA);
    }

    #[test]
    fn removing_and_dropping_touch_only_what_they_name() {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        q.add(vec![
            job(t.path(), "a"),
            job(t.path(), "b"),
            job(t.path(), "c"),
        ]);
        let a = q.claim_next().unwrap();
        assert_eq!(
            q.remove_queued(&a.target),
            0,
            "a running job is not removable"
        );
        assert_eq!(q.remove_queued(&job(t.path(), "b").target), 1);
        q.note_running(a.id, JobNote::Stalled);
        assert_eq!(q.snapshot().running().unwrap().note, Some(JobNote::Stalled));
        q.note_running(a.id + 100, JobNote::Cancelled);
        assert_eq!(q.snapshot().running().unwrap().note, Some(JobNote::Stalled));
        q.drop_job(a.id + 1);
        let titles: Vec<_> = q.snapshot().jobs.iter().map(|j| j.title.clone()).collect();
        assert_eq!(titles, ["a", "c"].map(String::from), "b went, a and c stay");
        q.drop_job(a.id);
        assert_eq!(q.snapshot().jobs.len(), 1);
    }

    // A queue file written before the retry fields and notes existed.
    const OLD_QUEUE: &str = r#"{
  "schema": 1,
  "next_id": 3,
  "paused": false,
  "debug_log": true,
  "jobs": [
    {"id": 2, "title": "B", "iso": "/i/B.iso", "target": "/m/B/B.mkv", "replace": true,
     "state": "failed", "queued_at": 5, "started_at": 6, "finished_at": 7, "note": "stalled",
     "failure": {"code": 9073, "message": "E9073 copy"}},
    {"id": 3, "title": "C", "iso": "/i/C.iso", "target": "/m/C/C.mkv", "replace": false,
     "state": "queued", "queued_at": 8, "started_at": null, "finished_at": null, "note": null}
  ],
  "results": {"/m/B/B.mkv": {"outcome": "failed", "code": 9073, "message": "E9073 copy", "finished_at": 7}}
}"#;

    #[test]
    fn an_old_queue_file_still_loads() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(QUEUE_FILE), OLD_QUEUE).unwrap();
        let q = Queue::open(t.path());
        let f = q.snapshot();
        assert!(f.debug_log);
        assert_eq!(f.jobs.len(), 2);
        let c = &f.jobs[1];
        assert_eq!(
            (c.attempts, c.not_before, c.staged.as_ref()),
            (0, None, None)
        );
        assert!(!t.path().join("library-queue.json.unreadable").exists());
        assert_eq!(q.claim_next().unwrap().title, "C");
        // What it writes back carries none of the new fields while they are unset.
        let back = std::fs::read_to_string(t.path().join(QUEUE_FILE)).unwrap();
        assert!(
            !back.contains("not_before") && !back.contains("attempts"),
            "{back}"
        );
    }

    #[test]
    fn a_storage_fault_backs_off_1_5_15_then_hourly_and_gives_up_after_a_day() {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        q.add(vec![job(t.path(), "a")]);
        let mut a = q.claim_next().unwrap();
        let now = 1_000_000;
        assert_eq!(
            retry_at(&a, now, true),
            Some(now),
            "a fresh check passed: at once"
        );
        assert_eq!(retry_at(&a, now, false), Some(now + 60));
        let mut waits = Vec::new();
        for _ in 0..5 {
            waits.push(retry_at(&a, now, false).unwrap() - now);
            a.attempts += 1;
            a.first_failed_at = Some(now);
        }
        assert_eq!(waits, [60, 300, 900, 3600, 3600]);
        assert_eq!(
            retry_at(&a, now, true),
            Some(now + 3600),
            "only the first retry skips the wait"
        );
        assert_eq!(
            retry_at(&a, now + RETRY_GIVE_UP_SECS, false),
            None,
            "a day on: give up"
        );
        a.attempts = RETRY_MAX_ATTEMPTS;
        assert_eq!(retry_at(&a, now, false), None);
    }

    #[test]
    fn a_job_waiting_out_its_backoff_is_held_and_others_go_first() {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        q.add(vec![job(t.path(), "a"), job(t.path(), "b")]);
        let a = q.claim_next().unwrap();
        let failure = Failure {
            code: Some(9073),
            message: "E9073 copy".into(),
        };
        let later = crate::server::util::epoch_secs() + 3600;
        q.retry_later(a.id, failure.clone(), later, None);
        let f = q.snapshot();
        let held = &f.jobs[0];
        assert_eq!(
            (held.state, held.note, held.attempts, held.not_before),
            (
                JobState::Queued,
                Some(JobNote::WaitingForFolder),
                1,
                Some(later)
            )
        );
        assert_eq!(
            held.failure.as_ref(),
            Some(&failure),
            "the queue says why it waits"
        );
        assert!(held.first_failed_at.is_some());
        assert_eq!(
            q.claim_next().unwrap().title,
            "b",
            "a backoff never blocks the rest"
        );
        let b = q.snapshot().running().unwrap().id;
        q.drop_job(b);
        assert!(q.claim_next().is_none(), "a waits out its backoff");
        assert!(
            !q.has_work(),
            "a waiting job does not hold the disks from audits"
        );
        assert_eq!(
            q.add(vec![job(t.path(), "a")]),
            0,
            "still queued: not queued twice"
        );
        // The staged hook: a kept file waits under its own note and survives a restart.
        q.retry_later(a.id, failure, 0, Some("/stage/1.mkv.partial".into()));
        drop(q);
        let q = Queue::open(t.path());
        let j = q.claim_next().unwrap();
        assert_eq!(j.note, Some(JobNote::StagedWaiting));
        assert_eq!(j.staged.as_deref(), Some(Path::new("/stage/1.mkv.partial")));
        assert_eq!(j.attempts, 2);
    }

    #[test]
    fn clearing_the_queued_jobs_unpauses_and_spares_the_running_one() {
        let t = tempfile::tempdir().unwrap();
        let q = Queue::open(t.path());
        q.add(vec![job(t.path(), "a"), job(t.path(), "b")]);
        q.claim_next().unwrap();
        q.set_paused(true);
        assert_eq!(q.clear_queued(), 1);
        let f = q.snapshot();
        assert!(!f.paused);
        assert_eq!(f.jobs.len(), 1);
        assert!(f.running().is_some());
    }
}
