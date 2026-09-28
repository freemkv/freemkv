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
            Err(_) => QueueFile::default(),
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
                });
                n += 1;
            }
            n
        })
    }

    /// Take the oldest queued job and mark it running. `None` while paused.
    /// An idle queue is left alone: no generation bump, no write.
    pub fn claim_next(&self) -> Option<Job> {
        {
            let f = self.lock();
            if f.paused || f.running().is_some() || f.count(JobState::Queued) == 0 {
                return None;
            }
        }
        self.mutate(|f| {
            if f.paused || f.running().is_some() {
                return None;
            }
            let job = f.jobs.iter_mut().find(|j| j.state == JobState::Queued)?;
            job.state = JobState::Running;
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

    /// Drop every queued job; the running one carries on.
    pub fn clear_queued(&self) -> usize {
        self.mutate(|f| {
            let before = f.jobs.len();
            f.jobs.retain(|j| j.state != JobState::Queued);
            before - f.jobs.len()
        })
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
}
