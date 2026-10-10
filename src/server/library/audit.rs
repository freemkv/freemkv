//! The audit queues: each MKV's audit verdict, persisted, and the two lanes that produce them.
//!
//! The quick lane runs the structural read ([`super::probe::audit_fast`]) on every new,
//! changed and never-audited file, whatever the Deep audit setting says. The deep lane runs
//! the full decode ([`super::deep`]) while that setting is on, on files whose quick audit
//! stands. Each lane runs one file at a time, after rips and remuxes, and neither waits on
//! the other. Verdicts live in `audit.json`, so a restart or a page refresh never loses them.

use super::deep::Verdict;
use super::probe::{AuditReport, FileSig};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const FILE: &str = "audit.json";

// A report stored before the media reader gets it read on the quick lane: a few files a
// minute, and only while no other quick audit waits, so new work never queues behind it.
const BACKFILL_BATCH: usize = 3;
const BACKFILL_EVERY_SECS: u64 = 60;

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
    /// The quick lane.
    queue: VecDeque<PathBuf>,
    /// The deep lane; absent from a file written before it existed.
    #[serde(default)]
    deep_queue: VecDeque<PathBuf>,
    paused: bool,
}

impl State {
    fn queued(&self) -> HashSet<&PathBuf> {
        self.queue.iter().chain(&self.deep_queue).collect()
    }
}

/// A row's full-decode state as the Library shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DeepView {
    /// `clean` | `corrupt` | `aborted` | `pending`.
    pub state: &'static str,
    pub verdict: Option<Verdict>,
}

/// A file being audited now.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Live {
    pub path: PathBuf,
    pub title: String,
    /// `quick`, `reading` or `decoding`.
    pub stage: &'static str,
    /// Of the whole audit, 0-100; `None` for the quick read.
    pub pct: Option<f64>,
}

/// The queues at a glance, for the activity strip.
#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub paused: bool,
    /// Files waiting in either lane, each counted once.
    pub queued: usize,
    /// The deep decode while one runs, else the quick read.
    pub running: Option<Live>,
}

// One lane's running file and its stop flag.
#[derive(Default)]
struct Slot {
    live: Mutex<Option<Live>>,
    cancel: AtomicBool,
}

impl Slot {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn path(&self) -> Option<PathBuf> {
        self.lock().as_ref().map(|l| l.path.clone())
    }
}

pub struct Audits {
    file: PathBuf,
    st: Mutex<State>,
    quick: Slot,
    deep: Slot,
    generation: AtomicU64,
    // Progress only: the strip repaints, the listing is not refetched.
    progress_generation: AtomicU64,
    // When the last detail backfill was queued.
    backfilled_at: AtomicU64,
}

// The share of the bar each deep stage fills: reading copies packets, decoding is the work.
const READING_SHARE: f64 = 15.0;

impl Audits {
    pub fn open(config_dir: &Path) -> Self {
        let file = config_dir.join(FILE);
        let st = match std::fs::read(&file) {
            Ok(b) => serde_json::from_slice(&b).unwrap_or_else(|e| {
                tracing::warn!(path = %file.display(), error = %e, "audit state unreadable; starting empty");
                set_aside(&file);
                State::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => {
                tracing::warn!(path = %file.display(), error = %e, "audit state unreadable; starting empty");
                set_aside(&file);
                State::default()
            }
        };
        Self {
            file,
            st: Mutex::new(st),
            quick: Slot::default(),
            deep: Slot::default(),
            generation: AtomicU64::new(0),
            progress_generation: AtomicU64::new(0),
            backfilled_at: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    // Persist and tell the page. The caller still holds `st`.
    fn changed(&self, st: &State) {
        self.persist(st);
        self.touch();
    }

    fn persist(&self, st: &State) {
        let tmp = self.file.with_extension("json.tmp");
        let saved = serde_json::to_vec(st)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(&tmp, json))
            .and_then(|()| std::fs::rename(&tmp, &self.file));
        if let Err(e) = saved {
            tracing::warn!(path = %self.file.display(), error = %e, "audit state write failed");
        }
    }

    fn touch(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Moves whenever a verdict, a queue or a running file changed.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Moves as the running deep decode progresses.
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

    /// Every header field of `path` as its last quick audit read them.
    pub fn raw(&self, path: &Path) -> Option<Vec<super::media::RawSection>> {
        let st = self.lock();
        let d = st.results.get(path)?.report.detail.as_ref()?;
        Some(d.raw.clone())
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

    /// Whether `path` waits in either lane.
    pub fn is_queued(&self, path: &Path) -> bool {
        let st = self.lock();
        st.queue.iter().chain(&st.deep_queue).any(|p| p == path)
    }

    /// The paths waiting in either lane, as a set (one lock for a whole listing).
    pub fn queued_set(&self) -> HashSet<PathBuf> {
        self.lock().queued().into_iter().cloned().collect()
    }

    /// The files being audited now, in either lane.
    pub(crate) fn running_set(&self) -> HashSet<PathBuf> {
        self.quick
            .path()
            .into_iter()
            .chain(self.deep.path())
            .collect()
    }

    pub fn status(&self) -> Status {
        let st = self.lock();
        Status {
            paused: st.paused,
            queued: st.queued().len(),
            running: self
                .deep
                .lock()
                .clone()
                .or_else(|| self.quick.lock().clone()),
        }
    }

    pub fn paused(&self) -> bool {
        self.lock().paused
    }

    /// Pauses both lanes.
    pub fn set_paused(&self, paused: bool) {
        let mut st = self.lock();
        st.paused = paused;
        self.changed(&st);
    }

    /// Queue the quick audit of `paths` not already waiting or running in that lane.
    /// Returns how many were added.
    pub fn enqueue(&self, paths: impl IntoIterator<Item = PathBuf>) -> usize {
        let mut st = self.lock();
        let n = push_lane(&mut st.queue, self.quick.path(), paths);
        if n > 0 {
            self.changed(&st);
        }
        n
    }

    /// Queue the full decode of `path` unless it waits or runs in the deep lane already.
    pub(crate) fn enqueue_deep(&self, path: &Path) {
        let mut st = self.lock();
        if push_lane(&mut st.deep_queue, self.deep.path(), [path.to_path_buf()]) > 0 {
            self.changed(&st);
        }
    }

    /// Re-audit `paths`: the full audit again, the deep decode included once the quick
    /// read is redone. Returns how many were queued (a file already waiting keeps its place).
    pub fn reaudit(&self, paths: &[PathBuf]) -> usize {
        {
            let mut st = self.lock();
            for p in paths {
                if let Some(r) = st.results.get_mut(p) {
                    r.deep = None;
                }
            }
            let redo: HashSet<&PathBuf> = paths.iter().collect();
            st.deep_queue.retain(|p| !redo.contains(p));
        }
        let n = self.enqueue(paths.iter().cloned());
        self.changed(&self.lock());
        n
    }

    /// Queue what `files` still needs: the quick read when never audited or changed since;
    /// with deep audit on, the full decode when none has finished and a retry is due. With
    /// `complete`, verdicts for files no longer present are dropped. A stored report without
    /// the media detail is read again on the quick lane, throttled (see `BACKFILL_BATCH`);
    /// its verdict, deep verdict and queue places are untouched.
    pub fn fill(
        &self,
        files: &[(PathBuf, FileSig)],
        deep_on: bool,
        now: u64,
        complete: bool,
    ) -> usize {
        let mut st = self.lock();
        if st.paused {
            return 0;
        }
        if complete {
            let present: HashSet<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
            prune_locked(self, &mut st, &present);
        }
        let (mut quick, mut deep) = (Vec::new(), Vec::new());
        for (p, sig) in files {
            match st.results.get(p) {
                Some(r) if r.matches(*sig) => {
                    if deep_on && deep_due(r, now) {
                        deep.push(p.clone());
                    }
                }
                _ => quick.push(p.clone()),
            }
        }
        let last = self.backfilled_at.load(Ordering::SeqCst);
        if quick.is_empty()
            && st.queue.is_empty()
            && self.quick.path().is_none()
            && now >= last.saturating_add(BACKFILL_EVERY_SECS)
        {
            quick.extend(
                files
                    .iter()
                    .filter(|(p, sig)| {
                        st.results
                            .get(p)
                            .is_some_and(|r| r.matches(*sig) && r.report.needs_detail())
                    })
                    .take(BACKFILL_BATCH)
                    .map(|(p, _)| p.clone()),
            );
            if !quick.is_empty() {
                self.backfilled_at.store(now, Ordering::SeqCst);
            }
        }
        let n = push_lane(&mut st.queue, self.quick.path(), quick)
            + push_lane(&mut st.deep_queue, self.deep.path(), deep);
        if n > 0 {
            self.changed(&st);
        }
        n
    }

    /// Drop the verdicts and queued entries of every file not in `present`. The caller
    /// guarantees `present` is a complete listing of the library.
    pub(crate) fn prune(&self, present: &HashSet<PathBuf>) {
        prune_locked(self, &mut self.lock(), present);
    }

    /// Empty the deep lane (deep audit was turned off). A running decode stops on its own.
    pub(crate) fn clear_deep(&self) {
        let mut st = self.lock();
        if !st.deep_queue.is_empty() {
            st.deep_queue.clear();
            self.changed(&st);
        }
    }

    /// Take the next quick audit, unless paused. It counts as running from here, so a
    /// stop or an enqueue before [`Self::start`] sees it. Not persisted: a restart redoes
    /// the file that was in flight.
    pub fn next(&self) -> Option<PathBuf> {
        self.take(&self.quick, |st| st.queue.pop_front(), "quick")
    }

    /// Take the next full decode, unless paused; as [`Self::next`] for the deep lane.
    pub(crate) fn next_deep(&self) -> Option<PathBuf> {
        self.take(&self.deep, |st| st.deep_queue.pop_front(), "reading")
    }

    fn take(
        &self,
        slot: &Slot,
        pop: impl FnOnce(&mut State) -> Option<PathBuf>,
        stage: &'static str,
    ) -> Option<PathBuf> {
        let mut st = self.lock();
        if st.paused {
            return None;
        }
        let p = pop(&mut st)?;
        slot.cancel.store(false, Ordering::SeqCst);
        *slot.lock() = Some(Live {
            path: p.clone(),
            title: String::new(),
            stage,
            pct: None,
        });
        self.touch();
        Some(p)
    }

    /// Put an interrupted decode back at the front of the deep lane.
    pub fn requeue_front(&self, path: PathBuf) {
        let mut st = self.lock();
        if !st.deep_queue.contains(&path) {
            st.deep_queue.push_front(path);
            self.changed(&st);
        }
    }

    /// Empty both lanes and stop the audits running now. Stop is terminal for the
    /// current audit run: refill stays disabled until the user explicitly resumes.
    /// Returns how many were waiting.
    pub fn stop_all(&self) -> usize {
        let mut st = self.lock();
        let n = st.queued().len();
        st.queue.clear();
        st.deep_queue.clear();
        st.paused = true;
        for slot in [&self.quick, &self.deep] {
            if slot.lock().is_some() {
                slot.cancel.store(true, Ordering::SeqCst);
            }
        }
        self.changed(&st);
        n
    }

    /// True once after [`Self::stop_all`] asked the running quick audit to stop.
    pub fn cancelled(&self) -> bool {
        self.quick.cancel.load(Ordering::SeqCst)
    }

    /// True once after [`Self::stop_all`] asked the running decode to stop.
    pub(crate) fn cancelled_deep(&self) -> bool {
        self.deep.cancel.load(Ordering::SeqCst)
    }

    /// Name the file the quick lane audits; a stop asked since [`Self::next`] stays asked.
    pub fn start(&self, path: &Path, title: String) {
        Self::name(&self.quick, path, title, "quick");
        self.touch();
    }

    /// Name the file the deep lane decodes; a stop asked since taking it stays asked.
    pub(crate) fn start_deep(&self, path: &Path, title: String) {
        Self::name(&self.deep, path, title, "reading");
        self.touch();
    }

    fn name(slot: &Slot, path: &Path, title: String, stage: &'static str) {
        *slot.lock() = Some(Live {
            path: path.to_path_buf(),
            title,
            stage,
            pct: None,
        });
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
        if let Some(l) = self.deep.lock().as_mut() {
            l.stage = stage;
            l.pct = pct.or(l.pct);
        }
        self.progress_generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The quick lane's file is done.
    pub fn finish(&self) {
        Self::clear(&self.quick);
        self.touch();
    }

    /// The deep lane's file is done.
    pub(crate) fn finish_deep(&self) {
        Self::clear(&self.deep);
        self.touch();
    }

    fn clear(slot: &Slot) {
        *slot.lock() = None;
        slot.cancel.store(false, Ordering::SeqCst);
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

// Append `paths` to `lane` unless waiting there already or `running` in it.
fn push_lane(
    lane: &mut VecDeque<PathBuf>,
    running: Option<PathBuf>,
    paths: impl IntoIterator<Item = PathBuf>,
) -> usize {
    let mut queued: HashSet<PathBuf> = lane.iter().cloned().collect();
    let mut n = 0;
    for p in paths {
        if running.as_ref() == Some(&p) || !queued.insert(p.clone()) {
            continue;
        }
        lane.push_back(p);
        n += 1;
    }
    n
}

// Keep an unreadable state file for the owner; the next write must not replace it.
fn set_aside(file: &Path) {
    let _ = std::fs::rename(file, file.with_extension("json.unreadable"));
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

fn prune_locked(a: &Audits, st: &mut State, present: &HashSet<PathBuf>) {
    let before = (st.results.len(), st.queue.len(), st.deep_queue.len());
    st.results.retain(|p, _| present.contains(p));
    st.queue.retain(|p| present.contains(p));
    st.deep_queue.retain(|p| present.contains(p));
    if (st.results.len(), st.queue.len(), st.deep_queue.len()) != before {
        a.changed(st);
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
