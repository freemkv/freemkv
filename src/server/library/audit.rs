//! The audit queue: each MKV's audit verdict, persisted, and the queue that produces them.
//!
//! One audit is the quick structural read ([`super::probe::audit_fast`]) and, while the
//! Deep audit setting is on, the full decode ([`super::deep`]). The queue fills itself
//! with new, changed and never-audited files; Re-audit puts files back on it. The worker
//! runs it one file at a time, after rips and remuxes. Verdicts live in `audit.json`, so a
//! restart or a page refresh never loses them.

use super::deep::Verdict;
use super::probe::{AuditReport, FileSig};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const FILE: &str = "audit.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
    size: u64,
    mtime_ns: i128,
    report: AuditReport,
    at: u64,
    deep: Option<DeepRecord>,
}

impl Record {
    fn matches(&self, sig: FileSig) -> bool {
        self.size == sig.size && self.mtime_ns == sig.mtime_ns
    }
}

// A deep run: a verdict when `verdict.completed`, else an inconclusive try to retry.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct DeepRecord {
    verdict: Verdict,
    attempts: u32,
    last_try: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    results: HashMap<PathBuf, Record>,
    queue: VecDeque<PathBuf>,
    paused: bool,
}

/// A row's full-decode state as the Library shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeepView {
    /// `clean` | `corrupt` | `aborted` | `pending`.
    pub state: &'static str,
    pub verdict: Option<Verdict>,
}

/// The file being audited now.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Live {
    pub path: PathBuf,
    pub title: String,
    /// `quick`, `reading` or `decoding`.
    pub stage: &'static str,
    /// Of the whole audit, 0-100; `None` for the quick read.
    pub pct: Option<f64>,
}

/// The queue at a glance, for the activity strip.
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub paused: bool,
    pub queued: usize,
    pub running: Option<Live>,
}

pub struct Audits {
    file: PathBuf,
    st: Mutex<State>,
    live: Mutex<Option<Live>>,
    cancel: AtomicBool,
    generation: AtomicU64,
    // Progress only: the strip repaints, the listing is not refetched.
    progress_generation: AtomicU64,
}

// The share of the bar each deep stage fills: reading copies packets, decoding is the work.
const READING_SHARE: f64 = 15.0;

impl Audits {
    pub fn open(config_dir: &Path) -> Self {
        let file = config_dir.join(FILE);
        let st = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            file,
            st: Mutex::new(st),
            live: Mutex::new(None),
            cancel: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            progress_generation: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_live(&self) -> std::sync::MutexGuard<'_, Option<Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    // Persist and tell the page. The caller still holds `st`.
    fn changed(&self, st: &State) {
        if let Ok(json) = serde_json::to_vec(st) {
            let tmp = self.file.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.file);
            }
        }
        self.touch();
    }

    fn touch(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Moves whenever a verdict, the queue or the running file changed.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Moves as the running audit progresses.
    pub fn progress_generation(&self) -> u64 {
        self.progress_generation.load(Ordering::SeqCst)
    }

    /// The quick audit of `path` if it was read at `sig`.
    pub fn report(&self, path: &Path, sig: FileSig) -> Option<AuditReport> {
        self.lock()
            .results
            .get(path)
            .filter(|r| r.matches(sig))
            .map(|r| r.report.clone())
    }

    /// The full-decode state of `path` at `sig`; `None` while deep audit is off and no
    /// verdict exists.
    pub fn deep_view(&self, path: &Path, sig: FileSig, deep_on: bool) -> Option<DeepView> {
        let st = self.lock();
        let rec = st.results.get(path).filter(|r| r.matches(sig));
        match rec.and_then(|r| r.deep.as_ref()) {
            Some(d) if d.verdict.completed => Some(DeepView {
                state: if d.verdict.clean { "clean" } else { "corrupt" },
                verdict: Some(d.verdict.clone()),
            }),
            _ if !deep_on => None,
            Some(d) => Some(DeepView {
                state: "aborted",
                verdict: Some(d.verdict.clone()),
            }),
            None => Some(DeepView {
                state: "pending",
                verdict: None,
            }),
        }
    }

    /// Whether `path` waits in the queue.
    pub fn is_queued(&self, path: &Path) -> bool {
        self.lock().queue.iter().any(|p| p == path)
    }

    /// The paths waiting, as a set (one lock for a whole listing).
    pub fn queued_set(&self) -> HashSet<PathBuf> {
        self.lock().queue.iter().cloned().collect()
    }

    pub fn status(&self) -> Status {
        let st = self.lock();
        Status {
            paused: st.paused,
            queued: st.queue.len(),
            running: self.lock_live().clone(),
        }
    }

    pub fn paused(&self) -> bool {
        self.lock().paused
    }

    pub fn set_paused(&self, paused: bool) {
        let mut st = self.lock();
        st.paused = paused;
        self.changed(&st);
    }

    fn is_running(&self, path: &Path) -> bool {
        self.lock_live().as_ref().is_some_and(|l| l.path == path)
    }

    /// Queue `paths` not already waiting or running. Returns how many were added.
    pub fn enqueue(&self, paths: impl IntoIterator<Item = PathBuf>) -> usize {
        let mut st = self.lock();
        let mut n = 0;
        for p in paths {
            if st.queue.contains(&p) || self.is_running(&p) {
                continue;
            }
            st.queue.push_back(p);
            n += 1;
        }
        if n > 0 {
            self.changed(&st);
        }
        n
    }

    /// Re-audit `paths`: the full audit again, the deep decode included. Returns how many
    /// were queued (a file already waiting keeps its place).
    pub fn reaudit(&self, paths: &[PathBuf]) -> usize {
        {
            let mut st = self.lock();
            for p in paths {
                if let Some(r) = st.results.get_mut(p) {
                    r.deep = None;
                }
            }
        }
        let n = self.enqueue(paths.iter().cloned());
        self.changed(&self.lock());
        n
    }

    /// Queue what `files` still needs: never audited, changed since, or (deep audit on) no
    /// full decode yet or an inconclusive one whose retry is due. With `complete`, verdicts
    /// for files no longer present are dropped.
    pub fn fill(
        &self,
        files: &[(PathBuf, FileSig)],
        deep_on: bool,
        now: u64,
        complete: bool,
    ) -> usize {
        let wanted: Vec<PathBuf> = {
            let mut st = self.lock();
            if complete {
                let present: HashSet<&PathBuf> = files.iter().map(|(p, _)| p).collect();
                let before = st.results.len();
                st.results.retain(|p, _| present.contains(p));
                st.queue.retain(|p| present.contains(p));
                if st.results.len() != before {
                    self.changed(&st);
                }
            }
            files
                .iter()
                .filter(|(p, sig)| match st.results.get(p) {
                    Some(r) if r.matches(*sig) => deep_on && deep_due(r, now),
                    _ => true,
                })
                .map(|(p, _)| p.clone())
                .collect()
        };
        self.enqueue(wanted)
    }

    /// Take the next file to audit, unless paused.
    pub fn next(&self) -> Option<PathBuf> {
        let mut st = self.lock();
        if st.paused {
            return None;
        }
        let p = st.queue.pop_front()?;
        self.changed(&st);
        Some(p)
    }

    /// Put an interrupted file back at the front.
    pub fn requeue_front(&self, path: PathBuf) {
        let mut st = self.lock();
        if !st.queue.contains(&path) {
            st.queue.push_front(path);
            self.changed(&st);
        }
    }

    /// Empty the queue and stop the audit running now. Returns how many were waiting.
    pub fn stop_all(&self) -> usize {
        let mut st = self.lock();
        let n = st.queue.len();
        st.queue.clear();
        if self.lock_live().is_some() {
            self.cancel.store(true, Ordering::SeqCst);
        }
        self.changed(&st);
        n
    }

    /// True once after [`Self::stop_all`] asked the running audit to stop.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn start(&self, path: &Path, title: String) {
        self.cancel.store(false, Ordering::SeqCst);
        *self.lock_live() = Some(Live {
            path: path.to_path_buf(),
            title,
            stage: "quick",
            pct: None,
        });
        self.touch();
    }

    /// The deep stage has reached `secs` of a `duration`-second movie.
    pub fn progress(&self, stage: &'static str, secs: f64, duration: Option<f64>) {
        let frac = duration
            .filter(|d| *d > 0.0)
            .map(|d| (secs / d).clamp(0.0, 1.0));
        let pct = frac.map(|f| match stage {
            "reading" => f * READING_SHARE,
            _ => READING_SHARE + f * (100.0 - READING_SHARE),
        });
        if let Some(l) = self.lock_live().as_mut() {
            l.stage = stage;
            l.pct = pct.or(l.pct);
        }
        self.progress_generation.fetch_add(1, Ordering::SeqCst);
    }

    pub fn finish(&self) {
        *self.lock_live() = None;
        self.cancel.store(false, Ordering::SeqCst);
        self.touch();
    }

    /// Record a quick audit; a changed file loses its deep verdict.
    pub fn record_fast(&self, path: &Path, sig: FileSig, report: AuditReport, now: u64) {
        let mut st = self.lock();
        let deep = st
            .results
            .get(path)
            .filter(|r| r.matches(sig))
            .and_then(|r| r.deep.clone());
        st.results.insert(
            path.to_path_buf(),
            Record {
                size: sig.size,
                mtime_ns: sig.mtime_ns,
                report,
                at: now,
                deep,
            },
        );
        self.changed(&st);
    }

    /// Whether `path` at `sig` needs a full decode now.
    pub fn deep_due_for(&self, path: &Path, sig: FileSig, now: u64) -> bool {
        self.lock()
            .results
            .get(path)
            .filter(|r| r.matches(sig))
            .is_some_and(|r| deep_due(r, now))
    }

    /// Record a full-decode run, unless the file changed under it.
    pub fn record_deep(&self, path: &Path, sig: FileSig, verdict: Verdict, now: u64) {
        if FileSig::stat(path) != Some(sig) {
            return;
        }
        let mut st = self.lock();
        let Some(r) = st.results.get_mut(path).filter(|r| r.matches(sig)) else {
            return;
        };
        let attempts = match &r.deep {
            _ if verdict.completed => 0,
            Some(d) if !d.verdict.completed => d.attempts + 1,
            _ => 1,
        };
        r.deep = Some(DeepRecord {
            verdict,
            attempts,
            last_try: now,
        });
        self.changed(&st);
    }
}

// A file with a quick audit that has video still needs its full decode (or a due retry).
fn deep_due(r: &Record, now: u64) -> bool {
    if r.report.video_tracks == 0 {
        return false;
    }
    match &r.deep {
        None => true,
        Some(d) if d.verdict.completed => false,
        Some(d) => super::deep::retry_due(d.attempts, d.last_try, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::library::deep::{Run, classify};
    use crate::server::library::probe::testmkv::mkv;

    fn file(dir: &Path, name: &str) -> (PathBuf, FileSig) {
        let p = dir.join(name);
        std::fs::write(&p, mkv("freemkv 1.7.7", Some(60.0), Some(58), true)).unwrap();
        let sig = FileSig::stat(&p).unwrap();
        (p, sig)
    }

    fn audited(a: &Audits, p: &Path, sig: FileSig) {
        let report = super::super::probe::audit_fast(p).unwrap();
        a.record_fast(p, sig, report, 1);
    }

    fn verdict(timed_out: bool) -> Verdict {
        let run = Run {
            rc: Some(0),
            timed_out,
            ..Default::default()
        };
        classify(&run, "decode", 1)
    }

    #[test]
    fn the_queue_fills_with_what_needs_an_audit_and_persists() {
        let t = tempfile::tempdir().unwrap();
        let (a_path, a_sig) = file(t.path(), "A.mkv");
        let (b_path, b_sig) = file(t.path(), "B.mkv");
        let files = vec![(a_path.clone(), a_sig), (b_path.clone(), b_sig)];
        let a = Audits::open(t.path());
        assert_eq!(a.fill(&files, false, 1, true), 2);
        assert_eq!(a.fill(&files, false, 1, true), 0, "already waiting");
        assert_eq!(a.next(), Some(a_path.clone()));
        audited(&a, &a_path, a_sig);
        let reopened = Audits::open(t.path());
        assert!(
            reopened.report(&a_path, a_sig).is_some(),
            "the verdict survives a restart"
        );
        assert_eq!(reopened.status().queued, 1, "so does the queue");
        assert_eq!(
            reopened.fill(&files, false, 1, true),
            0,
            "A is done, B still waits"
        );
    }

    #[test]
    fn deep_audit_on_queues_the_full_decode_and_reaudit_redoes_it() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let a = Audits::open(t.path());
        a.fill(&[(p.clone(), sig)], true, 1, true);
        a.next();
        audited(&a, &p, sig);
        assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "pending");
        assert_eq!(
            a.fill(&[(p.clone(), sig)], true, 1, true),
            1,
            "the decode is owed"
        );
        a.next();
        a.record_deep(&p, sig, verdict(false), 2);
        assert_eq!(a.deep_view(&p, sig, false).unwrap().state, "clean");
        assert_eq!(a.fill(&[(p.clone(), sig)], true, 3, true), 0);
        assert_eq!(a.reaudit(std::slice::from_ref(&p)), 1);
        assert!(a.deep_view(&p, sig, true).unwrap().state == "pending");
        assert!(
            a.is_queued(&p),
            "the quick read is redone when its turn comes"
        );
        assert!(
            a.report(&p, sig).is_some(),
            "the row keeps its tracks meanwhile"
        );
    }

    #[test]
    fn an_inconclusive_decode_retries_with_backoff() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let a = Audits::open(t.path());
        audited(&a, &p, sig);
        a.record_deep(&p, sig, verdict(true), 1000);
        assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "aborted");
        assert!(!a.deep_due_for(&p, sig, 1000 + 599));
        assert!(a.deep_due_for(&p, sig, 1000 + 600));
    }

    #[test]
    fn stop_all_empties_the_queue_and_cancels_the_running_audit() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let (q, qs) = file(t.path(), "B.mkv");
        let a = Audits::open(t.path());
        a.fill(&[(p.clone(), sig), (q, qs)], false, 1, true);
        let running = a.next().unwrap();
        a.start(&running, "A".into());
        assert_eq!(a.stop_all(), 1);
        assert!(a.cancelled());
        a.finish();
        assert!(!a.cancelled());
        assert_eq!(a.status().queued, 0);
    }

    #[test]
    fn progress_fills_one_bar_across_both_stages() {
        let t = tempfile::tempdir().unwrap();
        let a = Audits::open(t.path());
        a.start(Path::new("/m/A.mkv"), "A".into());
        a.progress("reading", 50.0, Some(100.0));
        assert_eq!(a.status().running.unwrap().pct, Some(7.5));
        a.progress("decoding", 100.0, Some(100.0));
        assert_eq!(a.status().running.unwrap().pct, Some(100.0));
    }

    #[test]
    fn a_paused_queue_hands_out_nothing() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let a = Audits::open(t.path());
        a.fill(&[(p, sig)], false, 1, true);
        a.set_paused(true);
        assert!(a.next().is_none());
        a.set_paused(false);
        assert!(a.next().is_some());
    }
}
