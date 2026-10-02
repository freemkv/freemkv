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

    /// Empty both lanes and stop the audits running now. Returns how many were waiting.
    pub fn stop_all(&self) -> usize {
        let mut st = self.lock();
        let n = st.queued().len();
        st.queue.clear();
        st.deep_queue.clear();
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
        a.finish();
        assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "pending");
        assert_eq!(
            a.fill(&[(p.clone(), sig)], true, 1, true),
            1,
            "the decode is owed"
        );
        assert!(a.next().is_none(), "on the deep lane, not the quick one");
        assert_eq!(a.next_deep(), Some(p.clone()));
        a.record_deep(&p, sig, verdict(false), 2);
        a.finish_deep();
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
        a.start_deep(Path::new("/m/A.mkv"), "A".into());
        a.progress("reading", 50.0, Some(100.0));
        assert_eq!(a.status().running.unwrap().pct, Some(7.5));
        a.progress("decoding", 100.0, Some(100.0));
        assert_eq!(a.status().running.unwrap().pct, Some(100.0));
    }

    #[test]
    fn a_stop_between_next_and_start_still_stops_that_file() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let a = Audits::open(t.path());
        a.fill(&[(p.clone(), sig)], false, 1, true);
        let taken = a.next().unwrap();
        assert_eq!(a.enqueue([taken.clone()]), 0, "it is running, not waiting");
        a.stop_all();
        assert!(a.cancelled(), "the stop reaches the file already taken");
        a.start(&taken, "A".into());
        assert!(a.cancelled(), "starting does not forget the stop");
    }

    #[test]
    fn an_unreadable_state_file_is_kept_not_overwritten() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(FILE), b"{ not json").unwrap();
        let a = Audits::open(t.path());
        let (p, sig) = file(t.path(), "A.mkv");
        a.fill(&[(p, sig)], false, 1, true);
        let kept = std::fs::read(t.path().join("audit.json.unreadable")).unwrap();
        assert_eq!(kept, b"{ not json");
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

    #[test]
    fn a_complete_fill_drops_vanished_files_and_an_incomplete_one_keeps_them() {
        let t = tempfile::tempdir().unwrap();
        let (a_path, a_sig) = file(t.path(), "A.mkv");
        let (b_path, b_sig) = file(t.path(), "B.mkv");
        let a = Audits::open(t.path());
        audited(&a, &a_path, a_sig);
        audited(&a, &b_path, b_sig);
        a.enqueue([b_path.clone()]);
        a.fill(&[(a_path.clone(), a_sig)], false, 1, false);
        assert!(
            a.report(&b_path, b_sig).is_some(),
            "an incomplete scan proves nothing"
        );
        assert!(a.is_queued(&b_path));
        a.fill(&[(a_path.clone(), a_sig)], false, 1, true);
        assert!(a.report(&b_path, b_sig).is_none());
        assert!(!a.is_queued(&b_path));
        assert!(
            a.report(&a_path, a_sig).is_some(),
            "present files keep theirs"
        );
    }

    #[test]
    fn a_changed_file_loses_its_deep_verdict_and_is_requeued() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let a = Audits::open(t.path());
        audited(&a, &p, sig);
        a.record_deep(&p, sig, verdict(false), 2);
        assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "clean");
        std::fs::write(
            &p,
            mkv("freemkv 1.7.7 rewritten", Some(60.0), Some(58), true),
        )
        .unwrap();
        let new = FileSig::stat(&p).unwrap();
        assert_ne!(new, sig);
        assert!(
            a.report(&p, new).is_none(),
            "the old report is not the new file's"
        );
        assert_eq!(a.fill(&[(p.clone(), new)], true, 3, true), 1, "requeued");
        audited(&a, &p, new);
        assert_eq!(a.deep_view(&p, new, true).unwrap().state, "pending");
    }

    #[test]
    fn a_decode_of_a_file_changed_mid_run_is_not_recorded() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let a = Audits::open(t.path());
        audited(&a, &p, sig);
        std::fs::write(
            &p,
            mkv("freemkv 1.7.7 rewritten", Some(60.0), Some(58), true),
        )
        .unwrap();
        a.record_deep(&p, sig, verdict(false), 2);
        assert_eq!(a.deep_view(&p, sig, true).unwrap().state, "pending");
    }

    #[test]
    fn an_interrupted_file_goes_back_first_and_the_backoff_doubles() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let (q, _) = file(t.path(), "B.mkv");
        let a = Audits::open(t.path());
        a.enqueue_deep(&q);
        a.requeue_front(p.clone());
        assert_eq!(a.next_deep(), Some(p.clone()), "front, not back");
        a.requeue_front(q.clone());
        a.requeue_front(q.clone());
        assert_eq!(a.status().queued, 1, "no duplicate in the queue");

        audited(&a, &p, sig);
        a.record_deep(&p, sig, verdict(true), 1000);
        a.record_deep(&p, sig, verdict(true), 2000);
        // Two failed tries: the delay is 1200 s, not 600.
        assert!(!a.deep_due_for(&p, sig, 2000 + 1199));
        assert!(a.deep_due_for(&p, sig, 2000 + 1200));
        // With deep audit off, an inconclusive try shows nothing.
        assert!(a.deep_view(&p, sig, false).is_none());
    }

    #[test]
    fn a_state_file_from_before_the_deep_lane_loads_into_the_quick_lane() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let (q, qs) = file(t.path(), "B.mkv");
        {
            let a = Audits::open(t.path());
            audited(&a, &p, sig);
            a.enqueue([q.clone()]);
        }
        let mut old: serde_json::Value =
            serde_json::from_slice(&std::fs::read(t.path().join(FILE)).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("deep_queue");
        assert_eq!(
            old.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["paused", "queue", "results"]
        );
        std::fs::write(t.path().join(FILE), old.to_string()).unwrap();
        let a = Audits::open(t.path());
        assert!(a.report(&p, sig).is_some(), "results load unchanged");
        assert!(!t.path().join("audit.json.unreadable").exists());
        assert_eq!(a.status().queued, 1);
        assert_eq!(
            a.fill(&[(p.clone(), sig), (q.clone(), qs)], true, 1, true),
            1,
            "A owes its decode on the deep lane; B already waits"
        );
        assert_eq!(a.next(), Some(q), "the old queue is the quick lane");
        assert_eq!(a.next_deep(), Some(p));
    }

    // audit.json as 1.7.7 wrote it, before the media detail: two verdicts (one with a
    // corrupt deep run), a quick and a deep queue.
    const STATE_1_7_7: &str = include_str!("testdata/audit-1.7.7.json");

    #[test]
    fn a_state_file_from_1_7_7_loads_whole_and_marks_its_detail_pending() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join(FILE), STATE_1_7_7).unwrap();
        let check = |a: &Audits| {
            let st = a.lock();
            assert_eq!(st.results.len(), 2);
            let arrival = &st.results[Path::new("/srv/movies/Arrival (2016)/Arrival (2016).mkv")];
            assert!(arrival.report.ok && arrival.report.detail.is_none());
            assert!(arrival.report.needs_detail());
            assert_eq!(arrival.report.audio[0].codec, "TrueHD");
            let deep = &arrival.deep.as_ref().unwrap().verdict;
            assert_eq!(
                (deep.reason.as_str(), deep.errors, deep.clean),
                ("decode_errors", 2, false)
            );
            assert_eq!((deep.forensic.as_ref(), deep.reached_secs), (None, None));
            let heat = &st.results[Path::new("/srv/movies/Heat (1995)/Heat (1995).mkv")];
            assert!(!heat.report.ok);
            assert!(matches!(
                heat.report.issues[0],
                super::super::probe::AuditIssue::RuntimeMismatch { .. }
            ));
            assert_eq!(st.queue.len(), 1);
            assert_eq!(st.deep_queue.len(), 1);
        };
        let a = Audits::open(t.path());
        assert!(!t.path().join("audit.json.unreadable").exists());
        check(&a);
        // Written back by this version and read again: nothing lost on the way.
        a.set_paused(false);
        check(&Audits::open(t.path()));
    }

    // A report as stored before the media detail existed.
    fn audited_without_detail(a: &Audits, p: &Path, sig: FileSig) {
        let mut report = super::super::probe::audit_fast(p).unwrap();
        report.detail = None;
        a.record_fast(p, sig, report, 1);
    }

    #[test]
    fn stored_reports_get_their_detail_a_few_at_a_time_on_an_idle_quick_lane() {
        let t = tempfile::tempdir().unwrap();
        let files: Vec<(PathBuf, FileSig)> = (0..5)
            .map(|i| file(t.path(), &format!("{i}.mkv")))
            .collect();
        let a = Audits::open(t.path());
        for (p, sig) in &files {
            audited_without_detail(&a, p, *sig);
        }
        let (p0, s0) = files[0].clone();
        a.record_deep(&p0, s0, verdict(false), 5);
        let now = 10_000;
        assert_eq!(a.fill(&files, false, now, true), BACKFILL_BATCH);
        assert_eq!(a.fill(&files, false, now + 1, true), 0, "they wait already");
        while let Some(p) = a.next() {
            let sig = FileSig::stat(&p).unwrap();
            a.record_fast(&p, sig, super::super::probe::audit_fast(&p).unwrap(), now);
            a.finish();
        }
        assert!(!a.report(&p0, s0).unwrap().needs_detail());
        assert!(a.report(&p0, s0).unwrap().ok);
        assert_eq!(
            a.deep_view(&p0, s0, true).unwrap().state,
            "clean",
            "the deep verdict stays"
        );
        assert_eq!(
            a.fill(&files, false, now + 30, true),
            0,
            "not before a minute has passed"
        );
        let a = Audits::open(t.path());
        let (p5, s5) = file(t.path(), "new.mkv");
        let mut with_new = files.clone();
        with_new.push((p5.clone(), s5));
        assert_eq!(
            a.fill(&with_new, false, now + 120, true),
            1,
            "new work first, no backfill beside it"
        );
        assert_eq!(a.next(), Some(p5.clone()));
        audited(&a, &p5, s5);
        a.finish();
        assert_eq!(a.fill(&with_new, false, now + 180, true), 2, "the rest");
        assert_eq!(a.raw(&p0).map(|r| r.is_empty()), Some(false));
    }

    #[test]
    fn stop_all_and_pause_act_on_both_lanes() {
        let t = tempfile::tempdir().unwrap();
        let (p, sig) = file(t.path(), "A.mkv");
        let (q, _) = file(t.path(), "B.mkv");
        let (r, _) = file(t.path(), "C.mkv");
        let a = Audits::open(t.path());
        audited(&a, &p, sig);
        a.enqueue([q.clone(), r.clone()]);
        a.enqueue_deep(&p);
        a.set_paused(true);
        assert!(a.next().is_none() && a.next_deep().is_none());
        a.set_paused(false);
        let deep = a.next_deep().unwrap();
        a.start_deep(&deep, "A".into());
        let quick = a.next().unwrap();
        assert_eq!(a.running_set().len(), 2);
        assert_eq!(a.status().running.unwrap().path, p, "the decode shows");
        assert_eq!(a.stop_all(), 1);
        assert!(a.cancelled() && a.cancelled_deep());
        a.finish();
        assert!(
            !a.cancelled() && a.cancelled_deep(),
            "each lane clears its own stop"
        );
        a.finish_deep();
        assert_eq!(quick, q);
        assert_eq!(a.status().queued, 0);
    }
}
