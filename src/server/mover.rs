use crate::server::config::Config;
use crate::server::planner::MediaKind;
use crate::server::tmdb;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

#[path = "mover_copy_monitor.rs"]
mod copy_monitor;
#[path = "mover_manifest.rs"]
mod manifest;

/// Progress for ONE artifact being moved (e.g. the `.mkv` movie, or its
/// companion `.iso`). Read by the System page's renderMoves() via SSE.
///
/// A single completed rip can move more than one artifact from one staging
/// dir — a movie file and, when `keep_iso` is on, its ISO — so [`MOVE_STATE`]
/// holds a Vec of these, one per planned file, and the UI draws one progress
/// bar per entry (`X-Men: Apocalypse (mkv)` and `X-Men: Apocalypse (iso)`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MoveState {
    /// The title (raw TMDB title, unsanitized — e.g. `X-Men: Apocalypse`).
    pub name: String,
    /// Which artifact this bar tracks: `"iso"`, `"mkv"`, `"m2ts"`, … — derived
    /// from the source file extension. Empty if unknown. The UI labels the bar
    /// `"{name} ({artifact})"`.
    pub artifact: String,
    pub phase: MovePhase,
    pub progress_pct: u8,
    pub progress_gb: f64,
    pub total_gb: f64,
    pub speed_mbs: f64,
    pub eta: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MovePhase {
    Queued,
    Copying,
    Finalizing,
    Delivered,
    Blocked,
}

fn block_move(index: usize) {
    let mut states = MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = states.get_mut(index) {
        entry.phase = MovePhase::Blocked;
        entry.speed_mbs = 0.0;
        entry.eta.clear();
    }
}

/// Live per-artifact move bars for the staging dir currently being moved.
/// Empty when nothing is moving. One entry per planned file, in move order;
/// because moves within a dir are sequential, at any instant one entry climbs
/// while later ones sit at 0% and earlier ones read 100%.
pub static MOVE_STATE: once_cell::sync::Lazy<Mutex<Vec<MoveState>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(Vec::new()));

/// The on-disk basename of the staging dir currently being moved, or `None`.
/// The Move queue (`build_queue_views`) selects dirs in `StagingState::Done`,
/// and the actively-moving dir stays in `Done` for the duration of the copy —
/// so without this it would appear BOTH as its live progress bars (`_move`) and
/// as a "(moving)" queue row (`_move_queue`), i.e. listed twice. The queue scan
/// reads this and skips the active dir, so the exclusion is by exact on-disk
/// name (robust to any title punctuation the filesystem sanitizer drops) rather
/// than a fragile client-side string match.
pub static ACTIVE_MOVE_DIR: once_cell::sync::Lazy<Mutex<Option<String>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));

// Serializes tests that mutate/observe the process-global move statics
// (MOVE_STATE, ACTIVE_MOVE_DIR, MOVE_ERRORS, MUX_ERRORS). pub(crate) + crate-level so
// tests here and in web.rs lock the SAME mutex and cannot race each other.
#[cfg(test)]
pub(crate) static TEST_STATE_LOCK: Mutex<()> = Mutex::new(());

// Whether another thread can take `m`. Retries briefly so a parallel test's momentary lock
// isn't mistaken for the CALLING thread holding it (which never frees within the window).
#[cfg(test)]
pub(crate) fn test_lock_is_free<T>(m: &Mutex<T>) -> bool {
    (0..200).any(|_| {
        let free = m.try_lock().is_ok();
        if !free {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        free
    })
}

// Clears MOVE_STATE and ACTIVE_MOVE_DIR when the per-directory pass leaves scope, by any path:
// normal completion, a failure continue, or an unwind.
struct MoveStateGuard;

impl MoveStateGuard {
    /// Arm the guard and mark `dir_basename` as the actively-moving staging dir
    /// (so the Move queue excludes it). The returned guard clears both statics
    /// on drop.
    fn arm(dir_basename: String) -> Self {
        *ACTIVE_MOVE_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir_basename);
        MoveStateGuard
    }
}

impl Drop for MoveStateGuard {
    fn drop(&mut self) {
        // Recover-and-proceed on poison: skipping the clear is exactly the
        // stuck-bar-for-the-process-lifetime failure this guard prevents.
        MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *ACTIVE_MOVE_DIR.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

// The artifact tag for a source file, from its extension: "iso", "mkv",
// "m2ts", "mk3d", …. Empty when there is no extension.
fn artifact_label(src: &Path) -> String {
    src.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Per-staging-dir error surfaced to the System page so the user can act
/// on it (e.g. orphaned source files that the container can't unlink due
/// to NFS squash perms). Keyed by staging dir path. The stored entry is
/// always refreshed, but the syslog line is only emitted when the
/// `reason` changes — so repeating the same error on every loop tick
/// updates the UI without spamming the log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MoverError {
    pub path: String,
    pub reason: String,
    pub hint: String,
    pub attempts: u32,
    pub retry_held: bool,
    pub waiting_for_folder: bool,
    pub worker_active: bool,
    /// An attempt for this row is running now, so the row reports the last failure only.
    pub retrying: bool,
    // The destination root this row is waiting on, so its recovery can clear every waiter.
    #[serde(skip)]
    waiting_root: Option<std::path::PathBuf>,
    #[serde(skip)]
    copy_stalled: bool,
    #[serde(skip)]
    retry_after: Option<std::time::Instant>,
    // The staging dir whose delivery raised this destination-keyed row; it goes with that dir.
    #[serde(skip)]
    owner: Option<String>,
    // The journaled wait for a missing title folder: the one row whose Retry consents to
    // recreating it. A root wait (unresponsive, stale, unmounted...) never does.
    #[serde(skip)]
    missing_folder: bool,
    // A declared stall's committed backoff, applied once its blocked worker returns.
    #[serde(skip)]
    stall_resume: Option<Option<std::time::Instant>>,
}

fn retry_delay(attempts: u32) -> Option<std::time::Duration> {
    match attempts {
        1 => Some(std::time::Duration::from_secs(30)),
        2 => Some(std::time::Duration::from_secs(120)),
        _ => None,
    }
}

fn mark_retrying(path: &str) {
    if let Some(error) = MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(path)
    {
        error.retrying = true;
    }
}

// Clears the row's "Retrying now." when the pass that set it ends, by whatever exit; a pass
// that recorded a new failure has already rewritten it.
struct RetryingGuard(String);

impl Drop for RetryingGuard {
    fn drop(&mut self) {
        if let Some(error) = MOVE_ERRORS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&self.0)
        {
            error.retrying = false;
        }
    }
}

// The destination lock sidecars of one pass. Every exit deletes them (a cooperating writer
// re-creates its own), so no `<destination>.lock` is ever left in the library.
struct TargetLocks(Vec<libfreemkv::io::ArtifactLock>);

impl Drop for TargetLocks {
    fn drop(&mut self) {
        for lock in self.0.drain(..) {
            if let Err(e) = lock.delete() {
                tracing::warn!(error = %e, "delivery lock sidecar retained");
            }
        }
    }
}

fn move_retry_ready(path: &str, now: std::time::Instant) -> bool {
    MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .is_none_or(|error| error.retry_after.is_some_and(|deadline| now >= deadline))
}

// Only an attempted delivery consumes the retry budget. A health probe waiting
// for a mount must remain able to recover as soon as that mount responds.
fn schedule_move_retry(path: &str) {
    let attempts = MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .map(|error| error.attempts.saturating_add(1));
    if let Some(attempts) = attempts {
        commit_retry(path, attempts, retry_delay(attempts));
    }
}

// A stall is an attempt, committed (journal first) as it is declared: a restart while the
// worker is still blocked cannot buy a fresh copy. The row holds until the worker returns.
fn commit_stall(path: &str) {
    schedule_move_retry(path);
    if let Some(row) = MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(path)
    {
        row.stall_resume = Some(row.retry_after);
        row.retry_after = None;
        row.retry_held = true;
    }
}

// The stalled worker returned: the backoff committed at the stall applies from now on.
fn finish_stall(path: &str) {
    let mut errors = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    match errors
        .get_mut(path)
        .and_then(|row| Some((row.stall_resume.take()?, row)))
    {
        Some((resume, row)) => {
            row.retry_after = resume;
            row.retry_held = resume.is_none();
        }
        None => {
            drop(errors);
            schedule_move_retry(path);
        }
    }
}

fn schedule_failed_delivery(path: &str, outcomes: &[MoveOutcome]) {
    if outcomes.iter().any(|outcome| {
        matches!(
            outcome,
            MoveOutcome::Failed | MoveOutcome::SizeMismatch | MoveOutcome::PostCopyInvalid
        )
    }) {
        schedule_move_retry(path);
    }
}

pub static MOVE_ERRORS: once_cell::sync::Lazy<Mutex<BTreeMap<String, MoverError>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(BTreeMap::new()));

// Stop automatic retries until the operator chooses Retry.
fn hold_for_operator(path: &str) {
    let attempts = MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .map(|error| error.attempts);
    if let Some(attempts) = attempts {
        commit_retry(path, attempts, None);
    }
}

// Operator Retry requests not yet applied to a delivery journal's retry state. The flag
// records that the retried row was the journaled missing-title-folder wait.
static RETRY_REQUESTS: once_cell::sync::Lazy<Mutex<std::collections::HashMap<String, bool>>> =
    once_cell::sync::Lazy::new(Default::default);

// The longest backoff [`retry_delay`] gives: no restored wait is ever longer.
const LONGEST_BACKOFF: std::time::Duration = std::time::Duration::from_secs(120);

// Derive the row's retry fields from a retry state, the journal's record of them.
fn apply_retry(row: &mut MoverError, state: &manifest::RetryState) {
    apply_retry_at(row, state, std::time::Instant::now());
}

// `apply_retry` as of `now`. The journal's wall-clock time is staging data: a far-future
// value (corruption, a clock stepped back) is clamped, never added unchecked.
fn apply_retry_at(row: &mut MoverError, state: &manifest::RetryState, now: std::time::Instant) {
    row.attempts = state.attempts;
    row.retry_after = state.not_before.map(|t| {
        let left = t
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default()
            .min(LONGEST_BACKOFF);
        now.checked_add(left).unwrap_or(now)
    });
    row.retry_held = row.retry_after.is_none();
}

// A journaled dir's retry state is written first and is authoritative, so neither a
// restart nor a reboot buys another full-file copy; the row is then derived from it. A
// state that cannot be saved holds the delivery: the stale journal would reset the count.
fn commit_retry(path: &str, attempts: u32, delay: Option<std::time::Duration>) {
    let Some(row) = MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .cloned()
    else {
        return;
    };
    let state = manifest::RetryState {
        attempts,
        not_before: delay.map(|d| std::time::SystemTime::now() + d),
        reason: row.reason,
        hint: row.hint,
    };
    let saved = manifest::Manifest::record_retry(Path::new(path), Some(state.clone()));
    let mut errors = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(row) = errors.get_mut(path) else {
        return;
    };
    apply_retry(row, &state);
    let Err(error) = saved else {
        return;
    };
    tracing::warn!(%error, dir = path, "mover: retry state not persisted");
    row.retry_after = None;
    row.retry_held = true;
    row.reason = format!(
        "{}; automatic retries stopped because the retry state could not be saved: {error}",
        row.reason
    );
    row.hint = "Free space on the staging storage or make it writable, then choose Retry. Staged source files are retained.".into();
    let line = format!("Move held: {path} — {}", row.reason);
    drop(errors);
    crate::server::log::syslog(&line);
}

// Whether a journaled dir may attempt delivery now, and whether the operator asked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Attempt {
    Wait,
    Due,
    Requested,
    // A Retry of the missing-folder wait itself: the operator accepts recreating it.
    Recreate,
}

// Rebuild a missing error row from the journal, or apply a pending operator Retry.
fn restore_retry(path: &str, saved: &manifest::Manifest) -> Attempt {
    let requested = RETRY_REQUESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
    if let Some(folder_wait) = requested {
        if let Err(error) = manifest::Manifest::record_retry(Path::new(path), None) {
            record_error(
                path,
                &format!("cannot clear the delivery retry state: {error}"),
                "Repair the staging storage, then choose Retry; nothing was copied.",
            );
            return Attempt::Wait;
        }
        return if folder_wait {
            Attempt::Recreate
        } else {
            Attempt::Requested
        };
    }
    let now = std::time::Instant::now();
    let Some(state) = saved.retry() else {
        return if move_retry_ready(path, now) {
            Attempt::Due
        } else {
            Attempt::Wait
        };
    };
    let mut errors = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    // The row already holds this state (its deadline clamped when first derived) or one the
    // journal failed to record: it stands. Re-deriving would clamp afresh every pass.
    if errors
        .get(path)
        .is_some_and(|row| row.attempts >= state.attempts)
    {
        drop(errors);
        return if move_retry_ready(path, now) {
            Attempt::Due
        } else {
            Attempt::Wait
        };
    }
    let row = errors
        .entry(path.to_string())
        .or_insert_with(|| MoverError {
            path: path.to_string(),
            reason: state.reason.clone(),
            hint: state.hint.clone(),
            attempts: 0,
            retry_held: false,
            waiting_for_folder: false,
            waiting_root: None,
            worker_active: false,
            retrying: false,
            copy_stalled: false,
            retry_after: None,
            owner: None,
            missing_folder: false,
            stall_resume: None,
        });
    apply_retry_at(row, state, now);
    drop(errors);
    if move_retry_ready(path, now) {
        Attempt::Due
    } else {
        Attempt::Wait
    }
}

fn record_error(path: &str, reason: &str, hint: &str) {
    record_error_with(path, reason, hint, crate::server::log::syslog);
}

// record_error with the log sink injected. The MOVE_ERRORS guard is dropped
// before `log` runs: syslog does blocking (NFS) I/O, mirroring muxer::record_error.
fn record_error_with(path: &str, reason: &str, hint: &str, log: impl FnOnce(&str)) {
    record_error_at_stage(path, reason, hint, None, log);
}

fn record_error_at_stage(
    path: &str,
    reason: &str,
    hint: &str,
    waiting_root: Option<&Path>,
    log: impl FnOnce(&str),
) {
    let same_reason = {
        let mut m = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
        let same_reason = m.get(path).map(|e| e.reason == reason).unwrap_or(false);
        let attempts = m.get(path).map_or(0, |e| e.attempts);
        let retry_after = m
            .get(path)
            .map_or(Some(std::time::Instant::now()), |e| e.retry_after);
        m.insert(
            path.to_string(),
            MoverError {
                path: path.to_string(),
                reason: reason.to_string(),
                hint: hint.to_string(),
                attempts,
                retry_held: retry_after.is_none(),
                waiting_for_folder: waiting_root.is_some(),
                waiting_root: waiting_root.map(Path::to_path_buf),
                worker_active: false,
                retrying: false,
                copy_stalled: false,
                retry_after,
                owner: None,
                missing_folder: false,
                stall_resume: None,
            },
        );
        same_reason
    };
    if !same_reason {
        log(&format!("Move blocked: {} — {}", path, reason));
    }
}

fn clear_recovered_folder_wait(path: &str) {
    let mut errors = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    if errors
        .get(path)
        .is_some_and(|error| error.waiting_for_folder && error.attempts == 0)
    {
        errors.remove(path);
    }
}

// A root that passed preflight is healthy for every item that was waiting on it; rows with
// attempts>0 are failed-delivery history and stay.
fn clear_recovered_folder_waits_for(root: &Path) {
    let mut errors = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    errors.retain(|_, e| !(e.attempts == 0 && e.waiting_root.as_deref() == Some(root)));
}

// Drop the row for `path` and every destination row its delivery raised.
fn clear_error(path: &str) {
    MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|key, e| key != path && e.owner.as_deref() != Some(path));
}

// Drop any MOVE_ERRORS row keyed by this DESTINATION path, on every exit from move_file that
// leaves valid bytes at dest.
fn clear_stale_dest_error(dest: &Path) {
    clear_error(&dest.to_string_lossy());
}

thread_local! {
    // The cause of this thread's latest failed delivery step, for the delivery's own row.
    static LAST_FAILURE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn note_failure(detail: String) {
    LAST_FAILURE.with(|f| *f.borrow_mut() = Some(detail));
}

fn take_failure() -> Option<String> {
    LAST_FAILURE.with(|f| f.borrow_mut().take())
}

// A row keyed by a delivery's destination, owned by its staging dir `job`: the step and
// its OS error as the reason, an instruction as the hint. The delivery's row gets the cause.
fn record_dest_error(dest: &Path, job: &Path, step: &str, error: &dyn std::fmt::Display) {
    let key = dest.to_string_lossy();
    let reason = format!("{step}: {error}");
    note_failure(reason.clone());
    let hint = crate::strings::get_or(
        "server.mover.destination_step_failed",
        "Check the destination folder's permissions, free space and mount. The delivery's own row shows when it is tried again.",
    );
    record_error(&key, &reason, &hint);
    if let Some(row) = MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(key.as_ref())
    {
        row.owner = Some(job.to_string_lossy().into_owned());
    }
}

// Both retry maps, always locked MOVE_ERRORS first: one order everywhere, so Retry, Retry
// all and the mover's prune can never deadlock.
fn with_retry_maps<R>(
    f: impl FnOnce(&mut BTreeMap<String, MoverError>, &mut std::collections::HashMap<String, bool>) -> R,
) -> R {
    let mut errors = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    let mut requests = RETRY_REQUESTS.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut errors, &mut requests)
}

// Drop MOVE_ERRORS rows whose staging dir is gone. `seen` is every staging child this pass
// listed; a key (or a destination row's owner) is dropped only if it is a direct child of
// staging_root AND was absent. A Retry request survives only for a listed staging child.
fn prune_move_errors(staging_root: &str, seen: &std::collections::HashSet<String>) {
    let root = Path::new(staging_root);
    let live = |key: &str| Path::new(key).parent() != Some(root) || seen.contains(key);
    with_retry_maps(|errors, requests| {
        errors.retain(|key, e| live(key) && e.owner.as_deref().is_none_or(live));
        requests.retain(|key, _| Path::new(key).parent() == Some(root) && seen.contains(key));
    });
}

/// Operator-initiated Retry: clear this error and its retry budget. The next
/// mover tick may attempt delivery again; no media is removed by this action.
/// False when nothing was scheduled: no such row, or its copy worker is still blocked.
pub fn clear_move_error(path: &str) -> bool {
    with_retry_maps(|errors, requests| {
        if errors.get(path).is_some_and(|e| !e.worker_active)
            && let Some(row) = errors.remove(path)
        {
            *requests.entry(path.to_string()).or_insert(false) |= row.missing_folder;
            return true;
        }
        false
    })
}

/// Operator-initiated Retry all: reset errors and their retry budgets.
pub fn clear_all_move_errors() {
    with_retry_maps(|errors, requests| {
        errors.retain(|path, e| {
            if !e.worker_active {
                *requests.entry(path.clone()).or_insert(false) |= e.missing_folder;
            }
            e.worker_active
        });
    });
}

/// Outcome of moving a single file. Distinguishes between an active move
/// (Moved) and a no-op re-check (Skipped) so the caller can
/// suppress webhook spam and log noise on subsequent loop ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveOutcome {
    /// Dest already exists with the same size as src — already moved on
    /// a previous tick. Source may or may not still be present.
    Skipped,
    /// Atomic rename succeeded — src is gone, dest has the bytes.
    Moved,
    /// Copy itself failed. Retried on the delivery's backoff.
    Failed,
    /// This attempt stalled and drained. Immutable even if the operator
    /// clears its UI error before the enclosing batch processes the result.
    Stalled,
    /// Shutdown cancelled the copy. Not a failure: no attempt is counted and
    /// the delivery resumes on the next start.
    Interrupted,
    /// A required destination operation is unsupported; operator action is
    /// needed, not another automatic full-file copy.
    Unsupported,
    /// Post-copy size check found dst != src even though the copy returned
    /// success. Surfaces distinctly so a half-copied destination (e.g. NFS
    /// server ran out of space mid-copy without surfacing an error) isn't
    /// treated as a successful move. Caller leaves the staging dir alone —
    /// dst is the broken copy, src is the source of truth.
    SizeMismatch,
    /// Post-copy validation failed for a NON-size reason: a structural
    /// check (missing EBML head, short/garbled tail, insufficient TS sync)
    /// or an unreadable destination. Kept distinct from `SizeMismatch` so
    /// the operator gets an accurate hint — an ENOSPC/short-write hint is
    /// wrong for a structurally-invalid-but-correctly-sized copy. Like
    /// `SizeMismatch`, the caller leaves the staging dir alone (src is the
    /// source of truth) and retries on the delivery's backoff.
    PostCopyInvalid,
    /// Destination already exists as a DIFFERENT file (present, non-empty, and a
    /// different size than src, or the same size with different content). The
    /// destination is never overwritten by publication. A wrong title match can resolve two distinct
    /// discs to the same `Title (Year)/Title (Year).ext` path; overwriting would
    /// destroy a good prior rip. We refuse the move, leave the new file in
    /// staging, and surface a collision error for the operator to resolve.
    Collision,
}

// Errors from the post-copy validation step inside move_file. Distinct from
// MoveOutcome (the move-loop's view) — this is the validation helper's view
// of why the copy was rejected; unit-testable via check_post_copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveError {
    /// The post-copy `stat()` of src and dst disagreed on length. `cp`
    /// returned 0 but the destination is short (or, in pathological
    /// cases, longer than src). Reported via the same `record_error`
    /// path that surfaces other move failures on the System page.
    SizeDoesNotMatch { src_size: u64, dst_size: u64 },
    /// MKV-specific: the destination didn't start with the EBML magic
    /// `1A 45 DF A3`. Either the cp truncated at the head, or the
    /// destination wasn't really an MKV to begin with.
    MkvBadHead,
    /// MKV-specific: the destination is too short, or its tail bytes
    /// couldn't be read back. This is a truncation/readability gate, not
    /// a structural EBML parse — see `check_post_copy_mkv`.
    MkvBadTail,
    /// TS / m2ts: not enough sync bytes (0x47) at TS-packet boundaries
    /// in the file head or tail to consider the file structurally
    /// sound. Likely a truncated cp.
    M2tsBadSync,
    /// Could not open the destination for read (NFS gone away, perm,
    /// etc.). Treat as a serious post-copy condition that warrants
    /// quarantine.
    Unreadable(String),
}

impl std::error::Error for MoveError {}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveError::SizeDoesNotMatch { src_size, dst_size } => write!(
                f,
                "post-cp size mismatch: src={} bytes, dst={} bytes",
                src_size, dst_size
            ),
            MoveError::MkvBadHead => {
                write!(f, "destination MKV missing EBML header (1A 45 DF A3)")
            }
            MoveError::MkvBadTail => write!(f, "destination MKV tail too short or unreadable"),
            MoveError::M2tsBadSync => write!(
                f,
                "destination m2ts has insufficient TS sync (0x47) at packet boundaries"
            ),
            MoveError::Unreadable(e) => write!(f, "destination unreadable: {}", e),
        }
    }
}

// Stat a path while bypassing the NFS attribute cache — opens a fresh FD and
// fstats it. Use instead of std::fs::metadata within an attribute-cache
// window (NFS acregmin, default 3s) of a write by another process.
fn fresh_metadata(path: &Path) -> std::io::Result<std::fs::Metadata> {
    let f = std::fs::File::open(path)?;
    f.metadata()
}

// Copy src -> dest in 4 MiB chunks, publishing the running bytes-written count into `written`
// as we go, so the move loop can show real progress without stat()-ing the destination.
#[cfg(test)]
fn copy_counting(
    src: &Path,
    dest: &Path,
    written: &std::sync::atomic::AtomicU64,
    halt: &libfreemkv::halt::Halt,
) -> std::io::Result<u64> {
    copy_counting_with_halt(
        src,
        dest,
        written,
        &|| crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed),
        Some(halt),
        &|_| Ok(()),
        None,
    )
}

// copy_counting with the abort signal injected, for testability without touching the
// process-global crate::server::SHUTDOWN (which every mover test shares).
#[cfg(test)]
fn copy_counting_cancellable(
    src: &Path,
    dest: &Path,
    written: &std::sync::atomic::AtomicU64,
    cancel: &dyn Fn() -> bool,
) -> std::io::Result<u64> {
    copy_counting_with_halt(src, dest, written, cancel, None, &|_| Ok(()), None)
}

fn copy_counting_with_halt(
    src: &Path,
    dest: &Path,
    written: &std::sync::atomic::AtomicU64,
    cancel: &dyn Fn() -> bool,
    halt: Option<&libfreemkv::halt::Halt>,
    before_publish: &dyn Fn(&Path) -> std::io::Result<()>,
    activity: Option<&copy_monitor::Activity>,
) -> std::io::Result<u64> {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;
    let stage = |operation| {
        if let Some(activity) = activity {
            activity.set(operation);
        }
    };
    let cancelled = || cancel() || halt.is_some_and(|halt| halt.is_cancelled());
    if cancelled() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "copy cancelled before start",
        ));
    }

    // Write to a sibling temp on the DEST filesystem, fsync it, then rename(2)
    // over the final name. Writing directly risks a truncated file at the
    // real name if killed mid-copy, wedging the move on a phantom Collision.
    let tmp = {
        let mut name = dest.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".part-{}", std::process::id()));
        dest.with_file_name(name)
    };
    // Remove any stale temp from a prior interrupted run before we start.
    stage("removing stale destination temporary file");
    remove_temporary(&tmp);
    let copy_to_tmp = || -> std::io::Result<u64> {
        let context = |operation: &str, path: &Path, error: std::io::Error| {
            std::io::Error::new(
                copy_error_kind(&error),
                format!("{operation} {}: {error}", path.display()),
            )
        };
        stage("opening staged source");
        let mut reader = std::fs::File::open(src).map_err(|e| context("open source", src, e))?;
        stage("creating destination temporary file");
        let mut writer = create_temporary(&tmp)
            .map_err(|e| context("create destination temporary file", &tmp, e))?;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let mut total = 0u64;
        loop {
            // Honour SIGTERM between chunks: without this the shutdown join
            // blocks for the whole remaining copy until docker stop's grace
            // expires and SIGKILL lands mid-write; Interrupted unlinks the temp.
            if cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "copy aborted: shutdown requested",
                ));
            }
            stage("reading staged source");
            let n = reader
                .read(&mut buf)
                .map_err(|e| context("read source", src, e))?;
            if n == 0 {
                break;
            }
            if cancelled() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "copy cancelled after read",
                ));
            }
            stage("writing destination temporary file");
            writer
                .write_all(&buf[..n])
                .map_err(|e| context("write destination", &tmp, e))?;
            total += n as u64;
            written.store(total, Ordering::Relaxed);
        }
        // The journal's Stamp is size plus this explicit time; a deferred SMB close
        // would otherwise restamp it. Best effort: a mismatch only holds the delivery.
        stage("pinning destination modification time");
        if let Err(error) = reader
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| writer.set_modified(crate::server::util::pinned_mtime(t)))
        {
            tracing::warn!(%error, path = %tmp.display(), "mover: cannot pin destination time");
        }
        stage("flushing destination temporary file");
        writer
            .flush()
            .map_err(|e| context("flush destination", &tmp, e))?;
        // Flush before publication and source removal. The shared primitive
        // handles macOS network volumes which reject F_FULLFSYNC but support
        // fsync, and bounds a flush that stops making progress.
        stage("syncing destination temporary file");
        libfreemkv::io::durable_sync_file(&writer, halt, |_, _| {
            if let Some(activity) = activity {
                activity.beat();
            }
        })
        .map_err(|e| context("sync destination", &tmp, e))?;
        Ok(total)
    };

    let total = match copy_to_tmp() {
        Ok(t) => t,
        Err(e) => {
            // Drop the partial temp so the next attempt starts clean and
            // no orphan lingers on the dest fs.
            stage("removing cancelled destination temporary file");
            remove_temporary(&tmp);
            return Err(e);
        }
    };

    // fsync the dest parent dir so the temp's dirent is durable, rename(2)
    // over the final name, then fsync again so the rename is durable before
    // move_file unlinks the source: a crash never leaves a truncated file.
    if let Some(parent) = dest.parent() {
        stage("syncing destination directory before publication");
        libfreemkv::io::fsync::dir(parent);
    }
    if cancel() || halt.is_some_and(|halt| halt.is_cancelled()) {
        remove_temporary(&tmp);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "copy aborted before publication",
        ));
    }
    stage("validating and checkpointing destination candidate");
    if let Err(e) = before_publish(&tmp) {
        remove_temporary(&tmp);
        return Err(e);
    }
    if cancel() || halt.is_some_and(|halt| halt.is_cancelled()) {
        remove_temporary(&tmp);
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "publication cancelled",
        ));
    }
    stage("publishing destination");
    if let Err(e) = libfreemkv::io::publish::no_replace(&tmp, dest) {
        remove_temporary(&tmp);
        return Err(std::io::Error::new(
            e.kind(),
            format!("publish destination {}: {e}", dest.display()),
        ));
    }
    if let Some(parent) = dest.parent() {
        stage("syncing published destination directory");
        libfreemkv::io::fsync::dir(parent);
    }
    Ok(total)
}

// Remove `<dest>.part-<pid>` temporaries of interrupted copies. Callers hold the
// job lease and target lock, so none is live; anything not removable is returned.
fn sweep_temporaries(dest: &Path) -> std::io::Result<()> {
    let (Some(parent), Some(name)) = (dest.parent(), dest.file_name()) else {
        return Ok(());
    };
    let mut prefix = name.to_os_string();
    prefix.push(".part-");
    let prefix = prefix.to_string_lossy().into_owned();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = file_name.strip_prefix(&prefix) else {
            continue;
        };
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let path = entry.path();
        let removed = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => std::fs::remove_file(&path),
            Ok(_) => Err(std::io::Error::other("not a regular file")),
            Err(e) => Err(e),
        };
        match removed {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!(
                        "leftover partial copy from an interrupted delivery could not be removed: {}: {e}",
                        path.display()
                    ),
                ));
            }
        }
    }
    Ok(())
}

// A failed removal of our own temporary is retried, and surfaced, by the next sweep.
fn remove_temporary(tmp: &Path) {
    if let Err(error) = std::fs::remove_file(tmp)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%error, path = %tmp.display(), "mover: destination temporary retained");
    }
}

// Exclusive and never through a link: the name is predictable on a shared folder,
// and the stale copy was just removed, so anything there now is not ours.
fn create_temporary(tmp: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
    options.open(tmp)
}

// Some Rust/platform combinations classify ENOTSUP as Uncategorized. Preserve
// that distinction before wrapping the OS error with operation/path context.
#[cfg(unix)]
fn copy_error_kind(error: &std::io::Error) -> std::io::ErrorKind {
    match error.raw_os_error() {
        Some(code) if code == libc::ENOTSUP || code == libc::EOPNOTSUPP => {
            std::io::ErrorKind::Unsupported
        }
        _ => error.kind(),
    }
}

#[cfg(not(unix))]
fn copy_error_kind(error: &std::io::Error) -> std::io::ErrorKind {
    error.kind()
}

// Verify a destination MKV via the EBML head magic (1A 45 DF A3) and
// readable tail bytes. A truncation/readability gate, NOT a structural
// EBML parse — the mux already validated the stream when it wrote the file.
fn check_post_copy_mkv(dst: &Path) -> Result<(), MoveError> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(dst).map_err(|e| MoveError::Unreadable(e.to_string()))?;

    // Head: EBML magic 1A 45 DF A3 in the first 4 bytes.
    let mut head = [0u8; 4];
    f.read_exact(&mut head)
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    if head != [0x1A, 0x45, 0xDF, 0xA3] {
        return Err(MoveError::MkvBadHead);
    }

    // Tail: confirm the last 8 bytes are readable (not truncated to zero).
    // We do NOT structurally parse EBML — the mux already validated the
    // stream on write; this only catches a cp that truncated the output.
    let size = f
        .metadata()
        .map_err(|e| MoveError::Unreadable(e.to_string()))?
        .len();
    if size < 5 {
        return Err(MoveError::MkvBadTail);
    }
    let tail_len = 8u64.min(size);
    f.seek(SeekFrom::End(-(tail_len as i64)))
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    let mut tail = [0u8; 8];
    let read = f
        .read(&mut tail[..tail_len as usize])
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    if read < tail_len as usize {
        return Err(MoveError::MkvBadTail);
    }
    Ok(())
}

// Verify a destination m2ts has plausible TS sync bytes (0x47) at 192-byte
// BD-TS packet boundaries (4-byte arrival-time prefix + 188-byte TS payload,
// sync at offset 4) in the head and tail; a truncated cp won't align.
fn check_post_copy_m2ts(dst: &Path) -> Result<(), MoveError> {
    use std::io::{Read, Seek, SeekFrom};

    const PKT: u64 = 192;
    const SYNC_OFFSET: usize = 4;
    const SAMPLE_PACKETS: u64 = 8;
    const THRESHOLD: usize = 6; // out of 2 * SAMPLE_PACKETS (head + tail = 16 samples)

    let mut f = std::fs::File::open(dst).map_err(|e| MoveError::Unreadable(e.to_string()))?;
    let size = f
        .metadata()
        .map_err(|e| MoveError::Unreadable(e.to_string()))?
        .len();
    // Require room for two DISTINCT, non-overlapping sample windows: with a
    // single window a small file's head and tail windows would overlap, so
    // one intact head could double-count and let a tail-truncated cp pass.
    if size < PKT * SAMPLE_PACKETS * 2 {
        return Err(MoveError::M2tsBadSync);
    }

    let mut count = 0usize;
    let mut buf = vec![0u8; (PKT * SAMPLE_PACKETS) as usize];

    // Head
    f.read_exact(&mut buf)
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    for i in 0..SAMPLE_PACKETS as usize {
        let off = i * PKT as usize + SYNC_OFFSET;
        if buf[off] == 0x47 {
            count += 1;
        }
    }

    // Tail
    f.seek(SeekFrom::End(-((PKT * SAMPLE_PACKETS) as i64)))
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    f.read_exact(&mut buf)
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    for i in 0..SAMPLE_PACKETS as usize {
        let off = i * PKT as usize + SYNC_OFFSET;
        if buf[off] == 0x47 {
            count += 1;
        }
    }

    // 6 / 16 sync bytes is loose, giving cushion for a non-BD-TS m2ts
    // variant with a different prefix layout, while still catching a
    // truncated cp where the tail packets are all garbage.
    if count < THRESHOLD {
        return Err(MoveError::M2tsBadSync);
    }
    Ok(())
}

/// Verify a destination by size only, using a fresh-FD stat that
/// bypasses the NFS attribute cache. Used for ISO files (no
/// lightweight structural check available without parsing UDF).
fn check_post_copy_size(src: &Path, dst: &Path) -> Result<(), MoveError> {
    // Do NOT default to 0 on a stat failure: the old `unwrap_or(0)` let a
    // failed dst stat plus a failed src stat validate as 0 == 0, after
    // which move_file would remove_file(src) and destroy the only copy.
    let dst_size = fresh_metadata(dst)
        .map_err(|e| MoveError::Unreadable(format!("dst stat failed: {e}")))?
        .len();
    let src_size = fresh_metadata(src)
        .map_err(|e| MoveError::Unreadable(format!("src stat failed: {e}")))?
        .len();
    if src_size != dst_size {
        return Err(MoveError::SizeDoesNotMatch { src_size, dst_size });
    }
    Ok(())
}

type StructuralCheck = fn(&Path) -> Result<(), MoveError>;

// The format-aware structural check for a destination, routed by extension
// (case-insensitive). None for formats without one (iso, unknown).
fn structural_check(dst: &Path) -> Option<StructuralCheck> {
    let ext = dst
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        // mk3d is byte-identical Matroska — same check as mkv.
        "mkv" | "mk3d" => Some(check_post_copy_mkv),
        "m2ts" => Some(check_post_copy_m2ts),
        _ => None,
    }
}

// Format-aware post-cp validation: the structural check (when the format has one), then a
// fresh-FD size compare.
pub(crate) fn check_post_copy(src: &Path, dst: &Path) -> Result<(), MoveError> {
    // Structural checks only inspect a fixed head/tail window, so a cp
    // truncated beyond it still passes (DATA-LOSS: move_file then unlinks
    // the source). Always pair with the fresh-FD size compare too.
    if let Some(check) = structural_check(dst) {
        check(dst)?;
    }
    check_post_copy_size(src, dst)
}

// One pass of the mover loop: take a config SNAPSHOT, release the lock, then move. Returns
// false if the config could not be read. The move is injected so this is testable.
fn mover_tick(cfg: &Arc<RwLock<Config>>, do_move: impl FnOnce(&Config)) -> bool {
    let snapshot = match cfg.read() {
        Ok(c) => c.clone(),
        Err(e) => {
            tracing::warn!(error = %e, "mover: config lock poisoned, retrying");
            return false;
        }
    };
    do_move(&snapshot);
    true
}

pub fn run(cfg: &Arc<RwLock<Config>>) {
    use std::sync::atomic::Ordering;
    tracing::info!("mover loop starting");
    while !crate::server::SHUTDOWN.load(Ordering::Relaxed) {
        if !mover_tick(cfg, check_and_move) {
            std::thread::sleep(std::time::Duration::from_secs(10));
            continue;
        }
        // SHUTDOWN-responsive sleep — break early on signal so SIGTERM
        // doesn't have to wait the full 10 s tick.
        for _ in 0..100 {
            if crate::server::SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    tracing::info!("mover loop stopping");
}

// Staging dirs already surfaced as stranded (a Fault from
// classify_done_absence). The mover rescans every ~10s, so track warned
// dirs to report each ONCE instead of re-WARNing every tick.
static STRANDED_WARNED: once_cell::sync::Lazy<Mutex<std::collections::HashSet<String>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashSet::new()));

// Staged outputs of 1.8.3 hand-offs already reported as filed by it, logged once each.
static MOVED_AWAY_LOGGED: once_cell::sync::Lazy<
    Mutex<std::collections::HashSet<std::path::PathBuf>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashSet::new()));

// A hand-off v1.8.3 wrote: its state.json predates `disc_identity`, which this build
// always writes and which arrived with the delivery journal.
fn pre_journal_handoff(dir: &Path) -> bool {
    std::fs::read(dir.join(crate::server::ripper::staging::STATE_FILE)).is_ok_and(|bytes| {
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .is_ok_and(|v| v.is_object() && v.get("disc_identity").is_none())
    })
}

// The outputs still to deliver, and those 1.8.3 already moved away: a planned leaf whose
// staged path is positively absent. Anything else stays planned (and is held if missing).
fn split_moved_away(
    dir: &Path,
    outputs: Vec<crate::server::ripper::staging::Output>,
) -> (
    Vec<crate::server::ripper::staging::Output>,
    Vec<crate::server::ripper::staging::Output>,
) {
    let (gone, kept): (Vec<_>, Vec<_>) = outputs.into_iter().partition(|output| {
        Path::new(&output.filename).file_name() == Some(output.filename.as_ref())
            && std::fs::symlink_metadata(dir.join(&output.filename))
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    });
    for output in &gone {
        let staged = dir.join(&output.filename);
        if MOVED_AWAY_LOGGED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(staged.clone())
        {
            crate::server::log::syslog(&format!(
                "{}: already filed by the earlier version (no longer staged); not delivered again",
                staged.display()
            ));
        }
    }
    (kept, gone)
}

// With an output moved away, every other file must be planned or ripper bookkeeping: a
// staged file under another name is never left to the journal's cleanup to delete.
fn legacy_unlisted_file(
    dir: &Path,
    planned: &[std::path::PathBuf],
) -> std::io::Result<Option<String>> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default();
        let map_of_planned = path.extension().is_some_and(|e| e == "mapfile")
            && planned.contains(&path.with_extension(""));
        if planned.contains(&path)
            || crate::server::ripper::staging::is_ripper_bookkeeping(name)
            || map_of_planned
            || orphan_mapfile(&path)
        {
            continue;
        }
        return Ok(Some(name.to_string_lossy().into_owned()));
    }
    Ok(None)
}

// A recovery map whose image is gone: it holds no media.
fn orphan_mapfile(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "mapfile")
        && std::fs::symlink_metadata(path.with_extension(""))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

// Every output of a 1.8.3 hand-off was already filed by it: only bookkeeping is left, so
// the rip is complete. Removes that bookkeeping and the folder; nothing else.
fn finish_filed_elsewhere(dir: &Path, display_name: &str) {
    let key = dir.to_string_lossy().to_string();
    let removed = (|| -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let name = path.file_name().unwrap_or_default();
            if crate::server::ripper::staging::is_ripper_bookkeeping(name) || orphan_mapfile(&path)
            {
                std::fs::remove_file(&path)?;
            }
        }
        std::fs::remove_dir(dir)
    })();
    match removed {
        Ok(()) => {
            clear_error(&key);
            crate::server::log::syslog(&format!(
                "Move complete: {display_name} (filed by the earlier version)"
            ));
        }
        Err(e) => record_error(
            &key,
            &format!("staging cleanup failed: {e}"),
            "The rip was already filed; remove its staging folder by hand.",
        ),
    }
}

// Forget in-memory mover state, as a process restart would.
#[cfg(test)]
pub(super) fn forget_process_state() {
    MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    *ACTIVE_MOVE_DIR.lock().unwrap_or_else(|e| e.into_inner()) = None;
    MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    RETRY_REQUESTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    STRANDED_WARNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

// Drop STRANDED_WARNED entries whose staging dir is gone. Mirrors prune_move_errors, same
// liveness guarantee.
fn prune_stranded_warned(seen: &std::collections::HashSet<String>) {
    let mut m = STRANDED_WARNED.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|key| seen.contains(key));
}

/// How the mover should treat a failed `.done` read on a staging dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoneAbsence {
    /// `.done` is absent but a governing marker (`.sweeping`/`.muxing`/
    /// `.ripped`/`.completed`/`.failed`/`.review`) shows the ripper/mux worker
    /// still owns the dir — expected "not ready yet" state. Quiet debug, skip
    /// (no WARN).
    InProgress,
    /// A real fault: non-NotFound read error (NFS ESTALE, EACCES), or a
    /// NotFound on a stranded dir with no governing marker. Worth a WARN.
    Fault,
}

// Classify a.done read error. NotFound + any governing marker present is the by-design
// in-progress state; everything else is a fault.
fn classify_done_absence(err_kind: std::io::ErrorKind, dir: &Path) -> DoneAbsence {
    if err_kind == std::io::ErrorKind::NotFound {
        // The staging dir was removed between the `.done` read and now (move
        // finalised, or stop-cleanup): not a stranded dir, so don't WARN.
        if !dir.exists() {
            return DoneAbsence::InProgress;
        }
        // `.sweeping`/`.muxing` mean "not ready", not stranded `Fault` (else
        // a 182-warn flood on one healthy disc). Use retrying
        // `snapshot_staging_disc`, not bare `exists()`, to dodge cold-NFS false negatives.
        let governed = crate::server::ripper::staging::snapshot_staging_disc(dir)
            .map(|s| {
                s.has_sweeping
                    || s.has_muxing
                    || s.has_ripped
                    || s.completed
                    || s.has_failed
                    || s.has_review
            })
            .unwrap_or(false);
        if governed {
            return DoneAbsence::InProgress;
        }
    }
    DoneAbsence::Fault
}

fn check_and_move(cfg: &Config) {
    // Scan staging directory for completed rips (dirs whose state.json is in
    // StagingState::Done; a legacy `.done` file is the fallback for un-migrated
    // dirs — see the per-dir readiness check below).
    let staging_root = &cfg.staging_dir;
    let entries = match std::fs::read_dir(staging_root) {
        Ok(e) => e,
        Err(e) => {
            // Don't swallow this: a dropped NFS mount or a deleted staging
            // root surfaces here, and a silent return makes the mover look
            // healthy while moving nothing. Make it observable.
            tracing::warn!(
                staging = %staging_root,
                error = %e,
                "mover: failed to read staging directory; skipping this pass"
            );
            return;
        }
    };

    // Every staging child this pass listed, for the `MOVE_ERRORS` prune
    // below, including skipped dirs — a still-ripping dir is present but
    // not ready, and must keep any error row it already has.
    let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut root_listing_complete = true;

    for entry in entries {
        // Don't silently drop a per-entry error (e.g. NFS ESTALE on a
        // specific dentry): on a loaded share a completed rip would be
        // missed for the whole tick with no trace. Log and skip.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                root_listing_complete = false;
                tracing::warn!(
                    staging = %staging_root,
                    error = %e,
                    "mover: per-entry error listing staging root; skipping entry"
                );
                continue;
            }
        };
        let dir = entry.path();
        // An entry observed before a failed stat is not confirmed absent.
        seen_dirs.insert(dir.to_string_lossy().to_string());
        if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        if !move_retry_ready(&dir.to_string_lossy(), std::time::Instant::now()) {
            continue;
        }
        let job_lease = crate::server::ripper::staging::job_lease(&dir);
        let Ok(_job_owner) = job_lease.try_lock() else {
            continue;
        };
        mark_retrying(&dir.to_string_lossy());
        let _retrying = RetryingGuard(dir.to_string_lossy().into_owned());

        let delivery = match manifest::Manifest::load(&dir) {
            Ok(delivery) => delivery,
            Err(e) => {
                record_error(
                    &dir.to_string_lossy(),
                    &format!("delivery journal unreadable: {e}"),
                    "Repair the journal or staging storage; no files have been adopted or discarded.",
                );
                continue;
            }
        };
        let attempt = match &delivery {
            Some(saved) => restore_retry(&dir.to_string_lossy(), saved),
            None => {
                // Nothing to reset without a journal: the Retry is spent on this pass.
                RETRY_REQUESTS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(dir.to_string_lossy().as_ref());
                Attempt::Due
            }
        };
        if attempt == Attempt::Wait {
            continue;
        }

        let marker_path = dir.join(".done");

        // Readiness comes from `state.json` when present: a dir is the
        // mover's to file iff `state == Done`; every other state is "not
        // mine yet". Legacy dirs with no `state.json` fall back to `.done`.
        let (marker, state_outputs): (
            serde_json::Value,
            Vec<crate::server::ripper::staging::Output>,
        ) = if let Some(saved) = &delivery {
            (saved.marker.clone(), saved.outputs.clone())
        } else {
            match crate::server::ripper::staging::read_state(&dir) {
                Some(st) => {
                    if st.state != crate::server::ripper::staging::StagingState::Done {
                        // Not handed off to the mover (in progress, held for
                        // review, terminal, or completed) — by-design "not
                        // ready", so keep it quiet: no per-tick WARN spam.
                        tracing::debug!(
                            dir = %dir.display(),
                            state = ?st.state,
                            "mover: staging dir not in Done state; skipping"
                        );
                        continue;
                    }
                    (mover_marker_value(&st), st.outputs.clone())
                }
                None => {
                    // Legacy `.done` file path. No pre-flight exists() check: it
                    // races with the read (the file can appear/disappear between
                    // syscalls); the read arms are the atomic gate.
                    match std::fs::read_to_string(&marker_path) {
                        Ok(data) => match serde_json::from_str(&data) {
                            Ok(v) => (v, Vec::new()),
                            Err(e) => {
                                // Empty/torn `.done` → NOT READY: skip rather than
                                // blind-move under a garbage name.
                                tracing::warn!(
                                    marker = %marker_path.display(),
                                    error = %e,
                                    "mover: .done marker is empty/unparsable; skipping staging dir (not ready)"
                                );
                                continue;
                            }
                        },
                        Err(e) => {
                            // An ABSENT `.done` on a governed dir is expected
                            // in-progress state — quiet debug, skip (the
                            // 182-warn bug). Only a stranded dir is a fault.
                            if classify_done_absence(e.kind(), &dir) == DoneAbsence::InProgress {
                                tracing::debug!(
                                    dir = %dir.display(),
                                    "mover: staging dir in progress (no .done yet); skipping"
                                );
                                continue;
                            }
                            // Stranded/unreadable dir (Fault). WARN ONCE per
                            // dir — the mover rescans every ~10s and would
                            // otherwise re-WARN forever. First → WARN, then debug.
                            let first = STRANDED_WARNED
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(dir.to_string_lossy().to_string());
                            if first {
                                tracing::warn!(
                                    marker = %marker_path.display(),
                                    error = %e,
                                    "mover: failed to read .done marker; skipping staging dir (stranded: no state.json/.done; further per-tick warns for this dir suppressed)"
                                );
                            } else {
                                tracing::debug!(
                                    marker = %marker_path.display(),
                                    error = %e,
                                    "mover: stranded staging dir (no state.json/.done); still skipping"
                                );
                            }
                            continue;
                        }
                    }
                }
            }
        };

        let disc_name = marker["disc_name"].as_str().unwrap_or("").to_string();
        let display_name = marker["title"].as_str().unwrap_or(&disc_name).to_string();

        // A parsable-but-content-empty marker carries no usable destination
        // name; filing it would route the MKV to the output root under an
        // empty name. Treat as NOT READY and skip — never blind-move.
        if display_name.trim().is_empty() {
            tracing::warn!(
                marker = %marker_path.display(),
                "mover: .done marker has empty title and disc_name; skipping staging dir (not ready)"
            );
            continue;
        }

        // Build TMDB result from marker
        let tmdb_result = if !marker["title"].is_null() {
            Some(tmdb::TmdbResult {
                title: marker["title"].as_str().unwrap_or("").to_string(),
                // Clamp before the cast: a year above 65535 would wrap to a
                // small number (e.g. 70000 -> 4464) and mislabel the folder.
                // 9999 is well past any real release year.
                year: marker["year"].as_u64().unwrap_or(0).min(9999) as u16,
                poster_url: marker["poster_url"].as_str().unwrap_or("").to_string(),
                overview: marker["overview"].as_str().unwrap_or("").to_string(),
                media_type: MediaKind::parse(marker["media_type"].as_str().unwrap_or("")),
                tmdb_id: marker["tmdb_id"].as_u64().unwrap_or(0),
            })
        } else {
            None
        };
        // Season number parsed from the disc label at rip time (null for movies
        // / unmarked labels). The TV branch of `build_destination` uses it to
        // place the rip under `Show (Year)/Season NN/`.
        let season = marker["season"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok());

        // Find ripped files. `keep_iso=false` means don't promote the
        // intermediate ISO (pre-0.25.10 moved 90+ GB ISOs live); filtered
        // here, except `output_format == "iso"`, where it IS the deliverable.
        let move_iso =
            cfg.keep_iso || crate::server::ripper::output_is_iso_image(&cfg.output_format);
        let (mut ripped_files, listing_complete): (Vec<std::path::PathBuf>, bool) =
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    collect_ripped_files(entries.map(|r| r.map(|e| e.path())), move_iso, &dir)
                }
                Err(e) => {
                    // Enumerating the staging dir failed (e.g. transient NFS
                    // read_dir error). Without this arm the dir is skipped
                    // silently forever — a `.done` marker never acted on.
                    record_error(
                        &dir.to_string_lossy(),
                        &format!("cannot list staging directory {}: {}", dir.display(), e),
                        "check that the staging mount is healthy and readable",
                    );
                    continue;
                }
            };

        // For a TV rip, `state_outputs` is the AUTHORITATIVE deliverable
        // list — file exactly those episodes. The dir scan can also surface a
        // leftover partial from a failed mux; unfiltered it'd promote as complete.
        if !state_outputs.is_empty() {
            let allowed: std::collections::HashSet<&str> =
                state_outputs.iter().map(|o| o.filename.as_str()).collect();
            ripped_files.retain(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // Keep the intermediate ISO: `outputs[]` never lists it, but a
                // `keep_iso` rip legitimately promotes it via `move_iso`;
                // dropping it here would let staging teardown destroy it.
                is_iso_file(name) || allowed.contains(name)
            });
        }

        // 1.8.3 delivered without a journal and may have been cut off mid-delivery. An output
        // it already moved out of staging is left where it was filed: never claimed, linked
        // or delivered again. Existing destinations are never adopted (the strict rule).
        let legacy = delivery.is_none() && pre_journal_handoff(&dir);
        let (state_outputs, moved_away) = if legacy {
            split_moved_away(&dir, state_outputs)
        } else {
            (state_outputs, Vec::new())
        };
        if !moved_away.is_empty() {
            match legacy_unlisted_file(&dir, &ripped_files) {
                Ok(None) => {}
                Ok(Some(name)) => {
                    record_error(
                        &dir.to_string_lossy(),
                        &format!("staging holds a file this hand-off does not list: {name}"),
                        "Nothing was moved or deleted. Rename it to the output it is, or remove it if it is not wanted, then choose Retry.",
                    );
                    continue;
                }
                Err(e) => {
                    record_error(
                        &dir.to_string_lossy(),
                        &format!("cannot list staging directory {}: {e}", dir.display()),
                        "check that the staging mount is healthy and readable",
                    );
                    continue;
                }
            }
            if ripped_files.is_empty() {
                finish_filed_elsewhere(&dir, &display_name);
                continue;
            }
        }

        if ripped_files.is_empty() && delivery.is_none() {
            // Nothing the mover should promote. Skip; the dir's lifetime
            // is governed by the ripper (which owns the state.json transition
            // and its own ISO-prune in the keep_iso=false multipass path).
            continue;
        }

        let dir_str = dir.to_string_lossy().to_string();

        // From here the pass can publish move progress and leave via several
        // `continue`s, so arm the RAII clear now. This also marks the dir's
        // basename as actively-moving so the Move queue excludes it.
        let active_dir_name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let _move_state = MoveStateGuard::arm(active_dir_name);

        // Build destination paths. For a TV rip, `state_outputs` maps each file
        // to its episode; the mover renames the leaf to `Show S{NN}E{MM}[ -
        // Name].ext` under `Show (Year)/Season NN/`. A movie leaf is unchanged.
        let mut planned_moves: Vec<(std::path::PathBuf, String)> = Vec::new();
        for file_path in &ripped_files {
            let filename = file_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let episode_leaf = tv_episode_leaf(&tmdb_result, &state_outputs, &filename, season);
            let leaf = episode_leaf.as_deref().unwrap_or(&filename);
            let dest = build_destination(cfg, &tmdb_result, leaf, season);
            planned_moves.push((file_path.clone(), dest));
        }
        if let Some(saved) = &delivery {
            planned_moves = saved.moves();
        }

        // Nothing below touches a destination root before it passes a bounded write test:
        // a stale or hung share holds the move (output kept in staging) and never blocks here.
        // Each root is tested once per attempt.
        let mut root_checks = RootChecks::default();
        let mut folder_missing = false;
        let unavailable = if let Some(saved) = &delivery {
            match saved
                .roots
                .iter()
                .find_map(|root| root_checks.check(Path::new(root)).err())
            {
                Some(problem) => Some(problem),
                None if attempt != Attempt::Recreate => {
                    let missing = missing_folder(&planned_moves);
                    folder_missing = missing.is_some();
                    missing
                }
                None => None,
            }
        } else {
            unreachable_root(cfg, &tmdb_result, &planned_moves, &mut root_checks)
        };
        // A check the server's stop cut short says nothing about the folder: no row, no log.
        if root_checks.stopped {
            continue;
        }
        if let Some(p) = unavailable {
            record_error_at_stage(
                &dir_str,
                &format!(
                    "waiting for the output folder: {} ({}: {})",
                    p.message,
                    p.path.display(),
                    p.detail
                ),
                &format!("{} The rip stays in staging until then.", p.hint),
                Some(&p.path),
                crate::server::log::syslog,
            );
            if folder_missing
                && let Some(row) = MOVE_ERRORS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&dir_str)
            {
                row.missing_folder = true;
            }
            continue;
        }
        clear_recovered_folder_wait(&dir_str);
        let passed_roots: Vec<std::path::PathBuf> = match &delivery {
            Some(saved) => saved.roots.iter().map(std::path::PathBuf::from).collect(),
            None => planned_roots(cfg, &tmdb_result, &planned_moves)
                .into_iter()
                .map(std::path::PathBuf::from)
                .collect(),
        };
        for root in &passed_roots {
            clear_recovered_folder_waits_for(root);
        }

        // Two discs of one boxset share a TMDB title/filename; `disc_variant`
        // gives disc 2 `Title (Year)_2.mkv` instead of a collision. ONE variant
        // covers the whole file set (MKV+ISO matched); a STAT failure aborts as `uncertain`.
        if delivery.is_none() {
            let mut uncertain = None;
            let mut residue = None;
            // Names another disc's journal froze are taken, published yet or not.
            let reserved = manifest::Manifest::reserved_elsewhere(Path::new(staging_root), &dir);
            let variant = crate::server::util::disc_variant(|n| {
                if uncertain.is_some() || residue.is_some() {
                    return false;
                }
                for (_, dest) in planned_moves.iter() {
                    let candidate = dest_with_variant(dest, n);
                    if is_reserved(&candidate, &reserved) {
                        return false;
                    }
                    match unowned_dest_claim(&candidate) {
                        DestClaim::Claimable => {}
                        DestClaim::OtherFile => return false,
                        DestClaim::EmptyFile => {
                            residue = Some(candidate);
                            return false;
                        }
                        DestClaim::Unknown => {
                            uncertain = Some(candidate);
                            return false;
                        }
                    }
                }
                true
            });
            if let Some(path) = residue {
                record_error(
                    &dir_str,
                    &format!(
                        "zero-length file at the destination, likely an interrupted publication: {}",
                        absolute_for_log(&path)
                    ),
                    "Nothing was moved or deleted, and no other name was used. Inspect the empty file; delete it if it is not a library file, then choose Retry. The rip stays in staging.",
                );
                hold_for_operator(&dir_str);
                continue;
            }
            match variant {
                Some(n) => {
                    for (_, dest) in planned_moves.iter_mut() {
                        *dest = dest_with_variant(dest, n);
                    }
                }
                None if uncertain.is_some() => {
                    // Never the base names it could not rule out: nothing is frozen until
                    // the name can be examined.
                    let path = uncertain.unwrap_or_default();
                    record_error(
                        &dir_str,
                        &format!(
                            "uncertain destination: cannot examine the destination {}; nothing was moved",
                            absolute_for_log(&path)
                        ),
                        "Check the output folder's permissions and mount. The rip stays in staging and is tried again on the next pass.",
                    );
                    continue;
                }
                None => {
                    // Every variant name is held by a different file: hold, never overwrite.
                    record_error(
                        &dir_str,
                        &format!(
                            "every disc-variant destination name is taken by a different file: {display_name}"
                        ),
                        "Resolve the library conflict (rename or remove files, or correct the title), then choose Retry. The rip stays in staging.",
                    );
                    hold_for_operator(&dir_str);
                    continue;
                }
            }
        }

        // FAIL-LOUD destination-root validation (Mercy incident hardening):
        // confirm the root exists/is a dir/is writable before creating a
        // subdir — never `create_dir_all` (swallows a rip into the overlay).
        let mut dest_roots: Vec<String> = Vec::new();
        for (src, _dest) in &planned_moves {
            let fname = src.file_name().unwrap_or_default().to_string_lossy();
            let r = destination_root_for(cfg, &tmdb_result, &fname);
            if !dest_roots.iter().any(|existing| existing == &r) {
                dest_roots.push(r);
            }
        }
        if let Some(saved) = &delivery {
            dest_roots = saved.roots.clone();
        }
        let blocked_root = dest_roots.iter().find_map(|r| {
            validate_root_checked(r, &mut root_checks)
                .err()
                .map(|reason| (r.clone(), reason))
        });
        if root_checks.stopped {
            continue;
        }
        if let Some((dest_root, reason)) = blocked_root {
            // Health already logged an unusable root; the error row still updates.
            let logged = crate::server::health::already_reported(Path::new(&dest_root));
            let sink = |line: &str| {
                if !logged {
                    crate::server::log::syslog(line);
                }
            };
            record_error_with(
                &dir_str,
                &format!(
                    "destination not available — refusing to move (output preserved in staging): {reason}"
                ),
                "the destination directory/mount is missing or not writable. \
                 Check the configured movie/tv/output directory exists and its \
                 bind-mount (e.g. the NAS share) is present and writable. The \
                 mover will NOT auto-create the root — fix the mount, then it \
                 retries on the next tick.",
                sink,
            );
            if !logged {
                crate::server::log::syslog(&format!(
                    "Move BLOCKED — destination root {} unavailable: {} (output preserved in staging: {})",
                    absolute_for_log(&dest_root),
                    reason,
                    dir.display()
                ));
            }
            continue;
        }

        // Refused before any folder is made: a refusal leaves nothing in the library.
        if delivery.is_none() {
            if !listing_complete {
                record_error(
                    &dir_str,
                    "cannot freeze delivery after an incomplete staging listing",
                    "Retry after staging storage recovers; no files have been moved.",
                );
                continue;
            }
            if state_outputs.iter().any(|output| {
                !ripped_files
                    .iter()
                    .any(|p| p.file_name().is_some_and(|n| n == output.filename.as_str()))
            }) {
                record_error(
                    &dir_str,
                    "a planned output is missing before delivery",
                    "Restore the staged output; existing library files cannot establish ownership without a delivery journal.",
                );
                continue;
            }
        }

        // Create destination directories below the root confirmed above, one level at a
        // time. A journaled delivery's folders existed at freeze: only Retry on its
        // missing-folder row recreates them.
        let folders: Vec<(std::path::PathBuf, std::path::PathBuf)> = match &delivery {
            None => planned_moves
                .iter()
                .filter_map(|(src, dest)| {
                    let fname = src.file_name().unwrap_or_default().to_string_lossy();
                    let root = destination_root_for(cfg, &tmdb_result, &fname);
                    Some((root.into(), Path::new(dest).parent()?.to_path_buf()))
                })
                .collect(),
            Some(saved) if attempt == Attempt::Recreate => saved.folders(),
            Some(_) => Vec::new(),
        };
        let mut created = Vec::new();
        let mut dest_ok = true;
        for (root, folder) in &folders {
            match create_below_root(root, folder, root_checks.identity(root)) {
                Ok(made) => created.extend(made),
                Err(e) => {
                    record_error(
                        &dir_str,
                        &format!(
                            "cannot create destination directory {}: {e}",
                            absolute_for_log(&folder.to_string_lossy())
                        ),
                        "check write permissions on the output / movie / tv directory",
                    );
                    dest_ok = false;
                    break;
                }
            }
        }
        if !dest_ok {
            remove_created(&created);
            continue;
        }

        if delivery.is_none() {
            match manifest::Manifest::create(
                &dir,
                marker.clone(),
                state_outputs.clone(),
                dest_roots,
                &planned_moves,
            ) {
                Ok(saved) => {
                    planned_moves = saved.moves();
                }
                Err(e) => {
                    remove_created(&created);
                    record_error(
                        &dir_str,
                        &format!("cannot persist delivery plan: {e}"),
                        "Staged sources are retained; repair storage or the conflicting destination and retry.",
                    );
                    continue;
                }
            }
        }

        let target_locks = match manifest::Manifest::load(&dir).and_then(|m| {
            m.ok_or_else(|| std::io::Error::other("delivery manifest disappeared"))?
                .lock_targets()
        }) {
            Ok(locks) => TargetLocks(locks),
            Err(e) => {
                if shutdown_interrupted(&e) {
                    log_interrupted_delivery(&dir_str);
                    continue;
                }
                record_error(
                    &dir_str,
                    &format!("delivery ownership lock unavailable: {e}"),
                    "Retry after the other library operation finishes; source files and journal are retained.",
                );
                continue;
            }
        };

        // Move files. Each artifact (movie file, and with keep_iso its ISO)
        // gets its OWN progress bar, not one aggregate. Seed one MOVE_STATE
        // entry per file up front (0%) so all bars appear immediately.
        {
            let seeded: Vec<MoveState> = planned_moves
                .iter()
                .map(|(src, _)| MoveState {
                    name: {
                        let artifact = artifact_label(src);
                        if artifact == "mkv" {
                            let filename = src
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string();
                            tv_episode_leaf(&tmdb_result, &state_outputs, &filename, season)
                                .and_then(|leaf| {
                                    std::path::Path::new(&leaf)
                                        .file_stem()
                                        .map(|stem| stem.to_string_lossy().into_owned())
                                })
                                .unwrap_or_else(|| display_name.clone())
                        } else {
                            display_name.clone()
                        }
                    },
                    artifact: artifact_label(src),
                    phase: MovePhase::Queued,
                    progress_pct: 0,
                    progress_gb: 0.0,
                    total_gb: fresh_metadata(src).map(|m| m.len()).unwrap_or(0) as f64
                        / crate::server::util::BYTES_PER_GIB,
                    speed_mbs: 0.0,
                    eta: String::new(),
                })
                .collect();
            *MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner()) = seeded;
        }
        let mut outcomes: Vec<MoveOutcome> = Vec::new();
        let mut held: Option<String> = None;
        take_failure();
        let mut announced_moving = false;
        for (i, (src, dest)) in planned_moves.iter().enumerate() {
            let on_progress = move |pct: u8, gb: f64, total_gb: f64, speed: f64| {
                // Per-file ETA from THIS file's own remaining bytes (not an
                // aggregate) — the bar and its ETA describe one artifact.
                let eta = if speed > 1.0 && total_gb > gb {
                    let secs = ((total_gb - gb) * 1024.0 / speed) as u32;
                    format!("{}:{:02}", secs / 60, secs % 60)
                } else {
                    String::new()
                };
                let mut ms = MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = ms.get_mut(i) {
                    entry.phase = if pct >= 100 {
                        MovePhase::Finalizing
                    } else {
                        MovePhase::Copying
                    };
                    entry.progress_pct = pct;
                    entry.progress_gb = gb;
                    entry.total_gb = total_gb;
                    entry.speed_mbs = if pct >= 100 { 0.0 } else { speed };
                    entry.eta = if pct >= 100 { String::new() } else { eta };
                }
            };
            let outcome = match manifest::Manifest::deliver(&dir, i, &on_progress) {
                Ok(outcome) => outcome,
                Err(e) => {
                    crate::server::log::syslog(&format!("Delivery journal held {}: {e}", dest));
                    block_move(i);
                    held.get_or_insert(e.to_string());
                    MoveOutcome::Failed
                }
            };
            outcomes.push(outcome);
            {
                let mut ms = MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = ms.get_mut(i) {
                    entry.phase = match outcome {
                        MoveOutcome::Moved | MoveOutcome::Skipped => MovePhase::Delivered,
                        _ => MovePhase::Blocked,
                    };
                    entry.speed_mbs = 0.0;
                    entry.eta.clear();
                }
            }
            if matches!(outcome, MoveOutcome::Stalled | MoveOutcome::Interrupted) {
                // The worker has drained; a stall consumes a retry attempt, an
                // interruption just stops. Publish no further artifact this tick.
                break;
            }
            // Peg this artifact's bar to 100% on success. Skipped (idempotent
            // re-move) reports no progress, so it'd sit at 0% otherwise;
            // Moved may stop short of 100. Failed/Collision stay stalled.
            if matches!(outcome, MoveOutcome::Moved | MoveOutcome::Skipped) {
                let mut ms = MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = ms.get_mut(i) {
                    entry.progress_pct = 100;
                    entry.progress_gb = entry.total_gb;
                    entry.speed_mbs = 0.0;
                    entry.eta = String::new();
                }
            }
            match outcome {
                MoveOutcome::Collision | MoveOutcome::Interrupted => {}
                MoveOutcome::Skipped => {
                    // Quiet — already moved on a prior tick; no log noise.
                }
                MoveOutcome::Moved => {
                    if !announced_moving {
                        crate::server::log::syslog(&format!(
                            "Moving: {} ({} files)",
                            display_name,
                            ripped_files.len()
                        ));
                        announced_moving = true;
                    }
                    // Log the FULL ABSOLUTE destination so the operator can
                    // see exactly where bytes landed — never a cwd-relative
                    // path that could hide a wrong-filesystem write (Mercy incident).
                    crate::server::log::syslog(&format!(
                        "Moved {} → {}",
                        src.file_name().unwrap_or_default().to_string_lossy(),
                        absolute_for_log(dest)
                    ));
                }
                MoveOutcome::Failed | MoveOutcome::Unsupported | MoveOutcome::Stalled => {
                    crate::server::log::syslog(&format!(
                        "Failed to move {} → {}",
                        src.display(),
                        absolute_for_log(dest)
                    ));
                }
                MoveOutcome::SizeMismatch => {
                    crate::server::log::syslog(&format!(
                        "Move blocked (post-cp size mismatch): {:?} -> {}",
                        src, dest
                    ));
                }
                MoveOutcome::PostCopyInvalid => {
                    crate::server::log::syslog(&format!(
                        "Move blocked (post-cp validation failed — structural/unreadable): {:?} -> {}",
                        src, dest
                    ));
                }
            }
        }

        let any_collision = outcomes.iter().any(|o| matches!(o, MoveOutcome::Collision));
        let any_failed = outcomes.iter().any(|o| matches!(o, MoveOutcome::Failed));
        let any_unsupported = outcomes
            .iter()
            .any(|o| matches!(o, MoveOutcome::Unsupported));
        let any_size_mismatch = outcomes
            .iter()
            .any(|o| matches!(o, MoveOutcome::SizeMismatch));
        let any_post_copy_invalid = outcomes
            .iter()
            .any(|o| matches!(o, MoveOutcome::PostCopyInvalid));

        if outcomes
            .iter()
            .any(|o| matches!(o, MoveOutcome::Interrupted))
        {
            log_interrupted_delivery(&dir_str);
            continue;
        }
        if outcomes.iter().any(|o| matches!(o, MoveOutcome::Stalled)) {
            finish_stall(&dir_str);
            continue;
        }
        if any_unsupported {
            record_error(
                &dir_str,
                "destination does not support a required delivery operation",
                "Automatic retries stopped. See the operation and destination in the copy error; fix the destination, then choose Retry. Staged source files are retained.",
            );
            hold_for_operator(&dir_str);
            continue;
        }

        if any_collision {
            record_error(
                &dir_str,
                "destination already exists as a different file — not overwriting",
                "another disc of the same title is normally filed alongside as `_2`, `_3`, ... — reaching this means that could not be done: every variant name is taken, or the destination changed mid-move. Verify/rename the existing library file, or correct the title, then re-run; the new rip is preserved in staging.",
            );
            schedule_failed_delivery(&dir_str, &outcomes);
            continue;
        }

        // Surface size-mismatch distinctly so the operator knows the dest is
        // the broken side (src is intact). Checked before `any_failed` so a
        // mixed batch surfaces the more diagnostic reason.
        if any_size_mismatch {
            record_error(
                &dir_str,
                "post-cp validation failed: destination size does not match source",
                SIZE_MISMATCH_HINT,
            );
            schedule_failed_delivery(&dir_str, &outcomes);
            continue;
        }

        if any_post_copy_invalid {
            record_error(
                &dir_str,
                "post-cp validation failed: destination is structurally invalid or unreadable",
                POST_COPY_INVALID_HINT,
            );
            schedule_failed_delivery(&dir_str, &outcomes);
            continue;
        }

        if let Some(reason) = held {
            record_error(
                &dir_str,
                &format!("delivery held: {reason}"),
                "The library file does not match what this delivery prepared, and it is never removed automatically. Inspect it, remove it if it is broken or not this rip, then choose Retry. Staged sources are retained.",
            );
            schedule_failed_delivery(&dir_str, &outcomes);
            continue;
        }

        if any_failed {
            // Leave the dir alone; it is retried on its backoff, then held for Retry.
            // Surface a summary error so the UI shows what's failing.
            let reason = match take_failure() {
                Some(cause) => format!("copy to destination failed: {cause}"),
                None => "copy to destination failed".to_string(),
            };
            record_error(
                &dir_str,
                &reason,
                "See device_system.log for the underlying error. The delivery is tried again after 30 s, then 2 min; after that it waits for Retry.",
            );
            schedule_failed_delivery(&dir_str, &outcomes);
            continue;
        }

        let delivered = match manifest::Manifest::load(&dir).and_then(|m| {
            let m = m.ok_or_else(|| std::io::Error::other("delivery manifest disappeared"))?;
            m.verify_delivered()?;
            Ok(m)
        }) {
            Ok(m) => m,
            Err(e) => {
                record_error(
                    &dir_str,
                    &format!("cannot verify delivered ownership: {e}"),
                    "Staging metadata is retained; restore the exact delivered files before retrying.",
                );
                continue;
            }
        };
        // Remember the entire frozen output set, including earlier passes. Linkage and the
        // webhook happen once per delivery: `announced` records both, so a re-pass whose
        // teardown failed earlier neither rewrites the links nor wakes the indexer again.
        if !delivered.announced() {
            if let Err(error) = crate::server::library::links::record_delivery(
                &cfg.autorip_dir,
                planned_moves.iter().map(|(_, d)| d.as_str()),
            ) {
                record_error(
                    &dir_str,
                    &format!("files delivered, but source linkage could not be saved: {error}"),
                    "The delivered files and staging metadata are retained. Repair library-links.json or its folder permissions, then choose Retry.",
                );
                schedule_move_retry(&dir_str);
                continue;
            }
            crate::server::webhook::send_move(
                cfg,
                &display_name,
                webhook_output_path(&planned_moves),
            );
            if let Err(error) = manifest::Manifest::mark_announced(&dir) {
                tracing::warn!(%error, dir = %dir.display(), "mover: move webhook may repeat");
            }
        }

        // Every file this pass could SEE is accounted for, not every file that IS there: a
        // listing error drops an entry the destructive remove_dir_all teardown would then
        // silently delete. Treat like a failed copy: leave the dir; it retries on its backoff.
        if !listing_complete {
            teardown_blocked(
                &dir_str,
                &format!(
                    "staging teardown skipped: the directory could not be fully listed: {}",
                    dir.display()
                ),
                "The files are delivered; staging is kept until it can be listed in full. Check the staging mount; the teardown retries after a backoff, then waits for Retry.",
            );
            continue;
        }

        // Try to tear down the staging dir; if it can't be removed (typically
        // because the orphan source files can't be unlinked), surface the
        // dir on the UI with a remediation hint.
        let cleanup_err = delivered.cleanup().err();

        if cleanup_err.is_none() {
            drop(target_locks);
            clear_error(&dir_str);
            crate::server::log::syslog(&format!("Move complete: {}", display_name));
        } else if let Some(e) = cleanup_err {
            teardown_blocked(
                &dir_str,
                &format!("staging cleanup failed: {}", e),
                "The files are delivered. Inspect changed or unplanned staging artifacts; they are not automatically deleted. The teardown retries after a backoff, then waits for Retry.",
            );
        }

        // MOVE_STATE is cleared by `_move_state`'s Drop as this iteration
        // ends — including via every `continue` above.
    }

    if root_listing_complete {
        prune_move_errors(staging_root, &seen_dirs);
    }
    // Same unbounded-growth prune for the stranded-dir one-time-warn dedup set.
    prune_stranded_warned(&seen_dirs);
}

// What a failed copy's row tells the operator to do.
const SIZE_MISMATCH_HINT: &str = "Check the destination filesystem for ENOSPC / short writes. The mover removes only its temp file and tries again after 30 s, then 2 min; after that it waits for Retry (if a dst file remains, remove it).";
const POST_COPY_INVALID_HINT: &str = "The copy is the correct size but failed a format/readability check (truncated header/tail, bad TS sync, or unreadable dst); see device_system.log for the specific check. The mover removes only its temp file and tries again after 30 s, then 2 min; after that it waits for Retry (if a dst file remains, remove it).";

// A delivered dir whose staging teardown cannot run: shown, and retried on the delivery's
// backoff (then held), never re-run every tick.
fn teardown_blocked(dir: &str, reason: &str, hint: &str) {
    record_error(dir, reason, hint);
    schedule_move_retry(dir);
}

// The move webhook's `output_path`: the media file, not the ISO archive, and the
// first by name (a TV rip's first episode) so the listing order can't change it.
fn webhook_output_path(planned_moves: &[(std::path::PathBuf, String)]) -> &str {
    let dests = || planned_moves.iter().map(|(_, d)| d.as_str());
    dests()
        .filter(|d| !is_iso_file(d))
        .min()
        .or_else(|| dests().min())
        .unwrap_or("")
}

// The deliverable files in one staging dir, and whether the listing was COMPLETE (false on any
// per-entry error — must not count as a completed move).
fn collect_ripped_files<I>(
    entries: I,
    move_iso: bool,
    dir: &Path,
) -> (Vec<std::path::PathBuf>, bool)
where
    I: IntoIterator<Item = std::io::Result<std::path::PathBuf>>,
{
    let mut files = Vec::new();
    let mut complete = true;
    for entry in entries {
        let p = match entry {
            Ok(p) => p,
            Err(e) => {
                complete = false;
                record_error(
                    &dir.to_string_lossy(),
                    &format!(
                        "per-entry error listing staging directory {}: {}",
                        dir.display(),
                        e
                    ),
                    "check that the staging mount is healthy and readable; \
                     staging contents are unknown for this directory",
                );
                continue;
            }
        };
        if p.extension()
            .and_then(|x| x.to_str())
            // Match case-insensitively: a disc labelled `.MKV`/`.ISO` (or any
            // mixed case from an external tool) is the same deliverable and must
            // not be silently skipped by an exact-case compare.
            .map(|ext| match ext.to_ascii_lowercase().as_str() {
                // mk3d is byte-identical Matroska (3D main feature) —
                // deliver it exactly like mkv.
                "mkv" | "mk3d" | "m2ts" => true,
                "iso" => move_iso,
                _ => false,
            })
            .unwrap_or(false)
        {
            files.push(p);
        }
    }
    (files, complete)
}

// Render disc variant `n` onto a destination path by suffixing its file
// STEM: `.../Title (2024).mkv` at variant 3 -> `.../Title (2024)_3.mkv`.
// Variant 1 returns the path unchanged (first/only disc keeps its name).
fn dest_with_variant(dest: &str, variant: u32) -> String {
    if variant <= 1 {
        return dest.to_string();
    }
    let p = Path::new(dest);
    let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
        return dest.to_string();
    };
    let suffixed = crate::server::util::disc_variant_name(stem, variant);
    let name = match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{suffixed}.{ext}"),
        None => suffixed,
    };
    p.with_file_name(name).to_string_lossy().into_owned()
}

/// Whether a move may take a particular destination path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestClaim {
    /// Free, or already holding THIS rip's own output (an idempotent re-move).
    Claimable,
    /// Held by a different file. Try the next disc variant.
    OtherFile,
    /// A zero-length regular file, likely an interrupted publication's claim:
    /// an operator hold, never a reason to take another name.
    EmptyFile,
    /// We could not find out. NOT a licence to try another name — see
    /// [`dest_claim`].
    Unknown,
}

// Whether `candidate` is one of `reserved`, as planned or through its folder's real path.
fn is_reserved(candidate: &str, reserved: &[std::path::PathBuf]) -> bool {
    let path = Path::new(candidate);
    let canonical = path
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .zip(path.file_name())
        .map(|(parent, name)| parent.join(name));
    reserved
        .iter()
        .any(|r| r == path || canonical.as_ref() == Some(r))
}

// A new plan owns no existing files, even when their bytes happen to match.
// Retries use the frozen manifest's exact pre-publication identity instead.
fn unowned_dest_claim(dest: &str) -> DestClaim {
    match std::fs::symlink_metadata(dest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DestClaim::Claimable,
        // A file where the folder goes: no file can be there; creating the folder reports it.
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => DestClaim::Claimable,
        Err(_) => DestClaim::Unknown,
        Ok(m) if m.is_file() => match fresh_metadata(Path::new(dest)) {
            Err(_) => DestClaim::Unknown,
            Ok(fresh) if fresh.len() == 0 => DestClaim::EmptyFile,
            Ok(_) => DestClaim::OtherFile,
        },
        Ok(_) => DestClaim::OtherFile,
    }
}

/// True when `filename` is a disc image (`.iso`, case-insensitive). Governs
/// whether a delivered file is routed to the configured `iso_dir` archive.
fn is_iso_file(filename: &str) -> bool {
    Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

/// Build the legacy-shaped mover metadata `Value` from a unified [`crate::server::ripper::staging::DiscState`],
/// so the rest of `check_and_move` reads `marker["title"]`/`["season"]`/… exactly
/// as it did from the old `.done` JSON body.
fn mover_marker_value(st: &crate::server::ripper::staging::DiscState) -> serde_json::Value {
    serde_json::json!({
        "title": st.title,
        "disc_name": st.disc_name,
        "format": st.disc_format,
        "year": st.year,
        "media_type": st.media_type,
        "tmdb_id": st.tmdb_id,
        "season": st.season,
        "disc": st.disc_number,
        "poster_url": st.tmdb_poster,
        "overview": st.tmdb_overview,
        "date": st.date,
    })
}

// The TV-episode leaf name for a staging `filename`, or None when this is
// not a TV episode output. Names as "Show S{NN}E{MM}[ - Episode Name].ext";
// build_destination folders it under "Show (Year)/Season NN/".
pub(crate) fn tv_episode_leaf(
    tmdb: &Option<tmdb::TmdbResult>,
    outputs: &[crate::server::ripper::staging::Output],
    filename: &str,
    season: Option<u16>,
) -> Option<String> {
    let result = tmdb.as_ref()?;
    if !result.media_type.is_tv() {
        return None;
    }
    let out = outputs.iter().find(|o| o.filename == filename)?;
    let episode = out.episode?;
    let season = season.unwrap_or(1);
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mkv");
    let safe_title = crate::server::util::sanitize_path_display(&result.title);
    if out.episode_name.is_empty() {
        Some(format!("{safe_title} S{season:02}E{episode:02}.{ext}"))
    } else {
        // Sanitize the episode name too: it comes from TMDB and can carry path
        // separators / reserved chars (e.g. "Part 1/2", ":") that would escape
        // the season folder or break the write, just like the title.
        let safe_episode = crate::server::util::sanitize_path_display(&out.episode_name);
        Some(format!(
            "{safe_title} S{season:02}E{episode:02} - {safe_episode}.{ext}"
        ))
    }
}

pub(crate) fn build_destination(
    cfg: &Config,
    tmdb: &Option<tmdb::TmdbResult>,
    filename: &str,
    season: Option<u16>,
) -> String {
    // Kept/output disc images archive to their own FLAT folder when `iso_dir` is configured,
    // sibling to the library, not beside the muxed title — one file per title, no per-title
    // subfolder. Empty `iso_dir` falls through below.
    if is_iso_file(filename) && !cfg.iso_dir.is_empty() {
        let root = resolve_media_root(&cfg.output_dir, &cfg.iso_dir);
        let leaf = match tmdb {
            Some(result) => {
                let safe_title = crate::server::util::sanitize_path_display(&result.title);
                let year_str = if result.year > 0 {
                    format!(" ({})", result.year)
                } else {
                    String::new()
                };
                format!("{safe_title}{year_str}.iso")
            }
            // No TMDB match: keep the (sanitized) source filename, mirroring the
            // no-tmdb fall-through the movie/tv branches use.
            None => crate::server::util::sanitize_path_display(filename),
        };
        return join_path(&root, &leaf);
    }
    // Source extension wins. Pre-0.25.7 this hardcoded ".mkv", which collided when
    // keep_iso=true left the mux output and source ISO both planning to the same path,
    // alternately overwriting each other (2026-05-20).
    let src_ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mkv");
    if let Some(result) = tmdb {
        let safe_title = crate::server::util::sanitize_path_display(&result.title);
        match result.media_type {
            MediaKind::Movie if !cfg.movie_dir.is_empty() => {
                let year_str = if result.year > 0 {
                    format!(" ({})", result.year)
                } else {
                    String::new()
                };
                // The movie root is `movie_dir` resolved UNDER `output_dir`: relative joins
                // onto output_dir, absolute wins (back-compat). Pre-fix a relative "movies"
                // resolved against container root / — the 2026-06 "Mercy" incident.
                let root = resolve_media_root(&cfg.output_dir, &cfg.movie_dir);
                let dir = join_path(&root, &format!("{safe_title}{year_str}"));
                // Filename carries the year too, matching the folder and the Plex/Jellyfin
                // `Title (Year)/Title (Year).ext` convention (pre-fix the file was bare
                // `Title.ext`).
                let name = format!("{safe_title}{year_str}.{src_ext}");
                join_path(&dir, &name)
            }
            MediaKind::Tv if !cfg.tv_dir.is_empty() => {
                // Same join fix as the movie branch: `tv_dir` resolved under
                // `output_dir` (relative joins, absolute wins).
                let root = resolve_media_root(&cfg.output_dir, &cfg.tv_dir);
                // Jellyfin/Plex TV layout: `Show (Year)/Season NN/`, zero-padded. Season is
                // parsed from the disc label at rip time; when absent, default to 1 rather
                // than dumping loose under the show.
                let year_str = if result.year > 0 {
                    format!(" ({})", result.year)
                } else {
                    String::new()
                };
                let show_dir = join_path(&root, &format!("{safe_title}{year_str}"));
                let dir = join_path(&show_dir, &format!("Season {:02}", season.unwrap_or(1)));
                // Sanitize the leaf too: the movie branch derives its leaf from a sanitized
                // title, but this branch used the RAW source filename, so a path separator or
                // traversal sequence could escape tv_dir.
                let safe_filename = crate::server::util::sanitize_path_display(filename);
                join_path(&dir, &safe_filename)
            }
            _ => {
                // Sanitize the leaf for consistency with the movie/tv
                // branches (they sanitize; this fallback used the raw leaf,
                // so e.g. "..mkv" would reach output_dir verbatim).
                join_path(
                    &cfg.output_dir,
                    &crate::server::util::sanitize_path_display(filename),
                )
            }
        }
    } else {
        join_path(
            &cfg.output_dir,
            &crate::server::util::sanitize_path_display(filename),
        )
    }
}

// Join a leaf (or relative subpath) onto a base dir via Path::join, so the OS
// path separator is used and a trailing slash on the base can't produce a `//`
// in the delivered path. Replaces the old `format!("{base}/{leaf}")` joins.
fn join_path(base: &str, leaf: &str) -> String {
    // POSIX '/' on every platform: autorip's library paths are Linux/NFS-style and
    // Windows accepts '/' as a separator, so the output is stable across builds
    // (Path::join would otherwise emit '\' on the Windows target).
    Path::new(base)
        .join(leaf)
        .to_string_lossy()
        .replace('\\', "/")
}

// Resolve a media subdirectory (movie_dir/tv_dir/iso_dir) UNDER output_dir via Path::join: a
// relative sub joins onto output_dir, an absolute sub replaces it (back-compat).
fn resolve_media_root(output_dir: &str, sub: &str) -> String {
    if sub.is_empty() {
        return output_dir.replace('\\', "/");
    }
    Path::new(output_dir)
        .join(sub)
        .to_string_lossy()
        .replace('\\', "/")
}

// The configured destination ROOT directory that governs a planned move, mirroring
// build_destination's root selection exactly. Validated present+writable BEFORE creating any
// subdir tree.
pub(crate) fn destination_root(cfg: &Config, tmdb: &Option<tmdb::TmdbResult>) -> String {
    if let Some(result) = tmdb {
        match result.media_type {
            MediaKind::Movie if !cfg.movie_dir.is_empty() => {
                return resolve_media_root(&cfg.output_dir, &cfg.movie_dir);
            }
            MediaKind::Tv if !cfg.tv_dir.is_empty() => {
                return resolve_media_root(&cfg.output_dir, &cfg.tv_dir);
            }
            _ => {}
        }
    }
    resolve_media_root(&cfg.output_dir, "")
}

// The configured root a given output FILE lands under:.iso uses iso_dir when set, everything
// else uses destination_root.
fn destination_root_for(cfg: &Config, tmdb: &Option<tmdb::TmdbResult>, filename: &str) -> String {
    if is_iso_file(filename) && !cfg.iso_dir.is_empty() {
        return resolve_media_root(&cfg.output_dir, &cfg.iso_dir);
    }
    destination_root(cfg, tmdb)
}

// Fail-loud destination-root validation, so the caller preserves output in staging on failure.
// A relative or empty root is refused without touching a disk; the rest is health's bounded
// preflight (the root must already exist as a writable folder; it is never created).
#[cfg(test)]
fn validate_destination_root(root: &str) -> Result<(), String> {
    validate_root_checked(root, &mut RootChecks::default())
}

// `validate_destination_root` reusing a write test `checks` already ran for `root`.
fn validate_root_checked(root: &str, checks: &mut RootChecks) -> Result<(), String> {
    root_shape(root)?;
    checks
        .check(Path::new(root))
        .map_err(|p| root_reason(root, &p))
}

// Create `folder` one level at a time below `root`, which must still be the folder its check
// passed (`identity`): a share dropped since is never recreated on the local disk. On
// failure nothing it made is left; on success it returns what it made.
fn create_below_root(
    root: &Path,
    folder: &Path,
    identity: Option<(u64, u64)>,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    let below = folder.strip_prefix(root).map_err(|_| {
        std::io::Error::other(format!("not under its destination root {}", root.display()))
    })?;
    let gone = || {
        std::io::Error::other(format!(
            "the destination root {} changed since its check (a share that dropped or was remounted?)",
            root.display()
        ))
    };
    let meta = std::fs::metadata(root).map_err(|_| gone())?;
    if !meta.is_dir()
        || identity.is_some_and(|id| file_identity(&meta) != Some(id))
        || crate::server::health::share_unmounted(root)
    {
        return Err(gone());
    }
    let mut created = Vec::new();
    let mut at = root.to_path_buf();
    for part in below.components() {
        let std::path::Component::Normal(name) = part else {
            remove_created(&created);
            return Err(std::io::Error::other(
                "unexpected destination path component",
            ));
        };
        at.push(name);
        match std::fs::create_dir(&at) {
            Ok(()) => created.push(at.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && at.is_dir() => {}
            Err(e) => {
                remove_created(&created);
                return Err(e);
            }
        }
    }
    Ok(created)
}

// Remove folders this pass made, deepest first, where still empty.
fn remove_created(created: &[std::path::PathBuf]) {
    for folder in created.iter().rev() {
        let _ = std::fs::remove_dir(folder);
    }
}

// A folder's (device, inode), where the platform has them.
fn file_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

// The bounded write test of each destination root, run at most once per move attempt.
struct RootChecks {
    // A check was cut short by the server stopping.
    stopped: bool,
    // Each passed root's identity, taken as its check answered.
    identities: Vec<(std::path::PathBuf, (u64, u64))>,
    limit: std::time::Duration,
    probe: fn(&Path) -> std::io::Result<()>,
    done: Vec<(
        std::path::PathBuf,
        Result<(), crate::server::health::Problem>,
    )>,
}

impl Default for RootChecks {
    fn default() -> Self {
        Self::with(
            crate::server::health::PREFLIGHT_TIMEOUT,
            crate::server::health::probe_write,
        )
    }
}

impl RootChecks {
    // The limit and the probe given (the tests inject a slow one).
    fn with(limit: std::time::Duration, probe: fn(&Path) -> std::io::Result<()>) -> Self {
        RootChecks {
            stopped: false,
            identities: Vec::new(),
            limit,
            probe,
            done: Vec::new(),
        }
    }

    fn check(&mut self, root: &Path) -> Result<(), crate::server::health::Problem> {
        if let Some((_, done)) = self.done.iter().find(|(p, _)| p == root) {
            return done.clone();
        }
        #[cfg(test)]
        test_hooks::preflighted(root);
        let role = crate::server::health::watched_role(root).unwrap_or("output");
        let result = crate::server::health::preflight_with(role, root, self.limit, self.probe);
        if result.is_ok()
            && let Some(id) = std::fs::metadata(root)
                .ok()
                .as_ref()
                .and_then(file_identity)
        {
            self.identities.push((root.to_path_buf(), id));
        }
        #[cfg(test)]
        test_hooks::after_preflight(root);
        self.stopped |= result.as_ref().is_err_and(|p| p.stopped());
        self.done.push((root.to_path_buf(), result.clone()));
        result
    }
}

impl RootChecks {
    // The identity `root` had when its check passed.
    fn identity(&self, root: &Path) -> Option<(u64, u64)> {
        self.identities
            .iter()
            .find(|(p, _)| p == root)
            .map(|(_, id)| *id)
    }
}

// `validate_destination_root` with the limit and the probe given.
#[cfg(test)]
fn validate_destination_root_with(
    root: &str,
    limit: std::time::Duration,
    probe: fn(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    validate_root_checked(root, &mut RootChecks::with(limit, probe))
}

// What needs no I/O: an empty root would write cwd-relative, and a relative root resolves
// against the process cwd (how the Mercy incident wrote inside the container).
fn root_shape(root: &str) -> Result<(), String> {
    if root.is_empty() {
        return Err("destination root is empty (no output/movie/tv directory configured)".into());
    }
    if !Path::new(root).is_absolute() {
        return Err(format!(
            "destination root '{root}' is not an absolute path; \
             a destination mount must be configured as an absolute path \
             (e.g. /mnt/media/movies) so it can never resolve \
             relative to the container's working directory"
        ));
    }
    Ok(())
}

// A failed preflight of `root`, worded for the move's error and the startup/settings warning.
fn root_reason(root: &str, p: &crate::server::health::Problem) -> String {
    use crate::server::health::{Fault, NOT_A_FOLDER};
    match p.fault {
        Fault::Missing => format!(
            "destination root '{root}' does not exist (mount missing?): {}",
            p.detail
        ),
        _ if p.detail == NOT_A_FOLDER => {
            format!("destination root '{root}' exists but is not a directory")
        }
        Fault::Unresponsive if p.detail.contains(crate::server::health::STILL_CHECKING) => {
            format!(
                "destination root '{root}' has not answered yet: {} (a NAS may be waking its disks)",
                p.detail
            )
        }
        Fault::Unresponsive => format!(
            "destination root '{root}' is not responding: {} (a stale network mount?)",
            p.detail
        ),
        Fault::Denied | Fault::ReadOnly | Fault::Other => {
            format!("destination root '{root}' is not writable: {}", p.detail)
        }
        _ => format!("destination root '{root}' is not usable: {}", p.detail),
    }
}

// The distinct absolute destination roots `planned` writes into.
fn planned_roots(
    cfg: &Config,
    tmdb: &Option<tmdb::TmdbResult>,
    planned: &[(std::path::PathBuf, String)],
) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    for (src, _) in planned {
        let r = destination_root_for(
            cfg,
            tmdb,
            &src.file_name().unwrap_or_default().to_string_lossy(),
        );
        if !r.is_empty() && Path::new(&r).is_absolute() && !roots.contains(&r) {
            roots.push(r);
        }
    }
    roots
}

// The first destination folder of a journaled plan that is gone: it existed at freeze, so
// its share may have fallen off its mount point. Recreating it is the operator's call.
fn missing_folder(
    planned: &[(std::path::PathBuf, String)],
) -> Option<crate::server::health::Problem> {
    use crate::server::health::{Fault, Problem};
    planned.iter().find_map(|(_, dest)| {
        let folder = Path::new(dest).parent()?;
        match std::fs::symlink_metadata(folder) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut problem = Problem::new("output", folder, Fault::Missing, e.to_string());
                problem.hint = format!(
                    "{} If the folder was removed on purpose, choose Retry to recreate it.",
                    problem.hint
                );
                Some(problem)
            }
            _ => None,
        }
    })
}

// The first destination root of `planned` that is stale, unmounted, failing or not answering.
// A root that is missing, read-only or refused is left to `validate_destination_root`'s messages.
fn unreachable_root(
    cfg: &Config,
    tmdb: &Option<tmdb::TmdbResult>,
    planned: &[(std::path::PathBuf, String)],
    checks: &mut RootChecks,
) -> Option<crate::server::health::Problem> {
    use crate::server::health::Fault;
    planned_roots(cfg, tmdb, planned).iter().find_map(|r| {
        checks.check(Path::new(r)).err().filter(|p| {
            matches!(
                p.fault,
                Fault::Stale | Fault::Io | Fault::Unresponsive | Fault::Unmounted
            )
        })
    })
}

/// Fail-loud-EARLY destination check: validates every configured, non-empty destination root
/// (movie/tv/output). The roots are preflighted at once and given up on after the folder health
/// check's limit, so a hung network mount reads "not responding" instead of blocking the
/// caller (a settings save).
pub(crate) fn check_configured_destinations(cfg: &Config) -> Vec<(String, String)> {
    check_configured_destinations_with(
        cfg,
        crate::server::health::CHECK_TIMEOUT,
        crate::server::health::probe_write,
    )
}

// `check_configured_destinations` with the limit and the probe given (the tests inject a slow one).
fn check_configured_destinations_with<F>(
    cfg: &Config,
    limit: std::time::Duration,
    probe: F,
) -> Vec<(String, String)>
where
    F: Fn(&Path) -> std::io::Result<()> + Copy + Send + 'static,
{
    let roots = configured_destination_roots(cfg);
    let dirs: Vec<std::path::PathBuf> = roots
        .iter()
        .filter(|r| root_shape(r).is_ok())
        .map(std::path::PathBuf::from)
        .collect();
    let mut probed =
        crate::server::health::preflight_each("output", &dirs, limit, probe).into_iter();
    roots
        .into_iter()
        .filter_map(|root| {
            let checked = match root_shape(&root) {
                Err(reason) => Err(reason),
                Ok(()) => match probed.next() {
                    Some(Err(p)) => Err(root_reason(&root, &p)),
                    _ => Ok(()),
                },
            };
            checked.err().map(|reason| (root, reason))
        })
        .collect()
}

// The distinct resolved destination roots `check_configured_destinations` validates.
fn configured_destination_roots(cfg: &Config) -> Vec<String> {
    // Validate the RESOLVED roots — the same joined paths the move actually
    // uses, not the raw relative `movie_dir`/`tv_dir`. Otherwise this would
    // flag a valid relative "movies" as "not absolute", or miss a bad join.
    let movie_root = if cfg.movie_dir.is_empty() {
        None
    } else {
        Some(resolve_media_root(&cfg.output_dir, &cfg.movie_dir))
    };
    let tv_root = if cfg.tv_dir.is_empty() {
        None
    } else {
        Some(resolve_media_root(&cfg.output_dir, &cfg.tv_dir))
    };
    // Deduplicate identical resolved roots (movie_dir resolving to the same
    // path as output_dir is common) so the operator doesn't see the same
    // warning twice.
    let mut seen: Vec<String> = Vec::new();
    for root in [movie_root, tv_root, Some(cfg.output_dir.replace('\\', "/"))]
        .into_iter()
        .flatten()
    {
        if root.is_empty() || seen.contains(&root) {
            continue;
        }
        seen.push(root);
    }
    seen
}

// Render a destination path as an ABSOLUTE path for logging, so the mover
// never logs a cwd-relative path that hides a wrong-filesystem write.
// A relative path is joined onto the process cwd.
fn absolute_for_log(dest: &str) -> String {
    let p = Path::new(dest);
    if p.is_absolute() {
        return dest.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p).to_string_lossy().to_string(),
        Err(_) => dest.to_string(),
    }
}

type BeforePublish = std::sync::Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

fn publish_rename(src: &Path, dest: &Path, job: &Path) -> std::io::Result<()> {
    #[cfg(not(test))]
    let _ = job;
    #[cfg(test)]
    if test_hooks::force_copy(job) {
        return Err(std::io::ErrorKind::CrossesDevices.into());
    }
    libfreemkv::io::publish::no_replace(src, dest)
}

fn move_file(
    src: &Path,
    dest: &Path,
    on_progress: &dyn Fn(u8, f64, f64, f64),
    before_publish: BeforePublish,
    job: &Path,
) -> MoveOutcome {
    // Journal recovery alone may adopt an existing destination. Never infer
    // transaction ownership from its size or structure; only a journaled prepared
    // stamp (plus matching head/tail while the source remains) is adopted.
    match std::fs::symlink_metadata(dest) {
        Ok(_) => return MoveOutcome::Collision,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return MoveOutcome::Failed,
    }
    let src_meta = fresh_metadata(src);

    if let Err(e) = before_publish(src) {
        record_dest_error(dest, job, "publication checkpoint failed", &e);
        return MoveOutcome::Failed;
    }
    match publish_rename(src, dest, job) {
        Ok(()) => {
            if let Some(parent) = dest.parent()
                && let Err(e) = libfreemkv::io::fsync::dir_checked(parent)
            {
                record_dest_error(dest, job, "publication directory sync failed", &e);
                return MoveOutcome::Failed;
            }
            clear_stale_dest_error(dest);
            return MoveOutcome::Moved;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return MoveOutcome::Collision,
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {}
        Err(e) => {
            record_dest_error(dest, job, "publication failed", &e);
            return if e.kind() == std::io::ErrorKind::Unsupported {
                MoveOutcome::Unsupported
            } else {
                MoveOutcome::Failed
            };
        }
    }

    let dest_str = dest.to_string_lossy().to_string();
    // Never delete a final pathname based on an earlier absence check.
    let src_size = src_meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let total_gb = src_size as f64 / crate::server::util::BYTES_PER_GIB;

    // In-process copy on a worker thread (`copy_counting`), counting bytes
    // for live progress — not NFS stat(), which lags and would pin the bar
    // at 0%. Post-copy validation runs before unlink; src stays intact on failure.
    let src_owned = src.to_path_buf();
    let dest_owned = dest.to_path_buf();
    #[cfg(test)]
    let hook_job = job.to_path_buf();
    let written = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let written_w = std::sync::Arc::clone(&written);
    let flush_halt = libfreemkv::halt::Halt::new();
    let worker_halt = flush_halt.clone();
    let activity = Arc::new(copy_monitor::Activity::default());
    let worker_activity = activity.clone();
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<u64>>();
    let copy_handle = match std::thread::Builder::new()
        .name("mover-copy".into())
        .spawn(move || {
            let result = copy_counting_with_halt(
                &src_owned,
                &dest_owned,
                &written_w,
                &|| crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed),
                Some(&worker_halt),
                &|candidate| {
                    check_candidate(&src_owned, &dest_owned, candidate)?;
                    #[cfg(test)]
                    test_hooks::before_publish(&hook_job);
                    before_publish(candidate)
                },
                Some(&worker_activity),
            );
            let _ = tx.send(result);
        }) {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(error = %e, "could not start mover copy");
            return MoveOutcome::Failed;
        }
    };

    let result = copy_monitor::Monitor {
        job,
        destination: dest,
        written: &written,
        activity: &activity,
        halt: &flush_halt,
        progress: on_progress,
        source_size: src_size,
        stall_window: copy_stall_window(job),
        poll_interval: copy_poll_interval(),
    }
    .wait(rx, copy_handle);
    match result {
        Ok(_bytes) => {
            on_progress(100, total_gb, total_gb, 0.0);
            if let Some(parent) = dest.parent()
                && let Err(e) = libfreemkv::io::fsync::dir_checked(parent)
            {
                record_dest_error(dest, job, "publication directory sync failed", &e);
                return MoveOutcome::Failed;
            }
            clear_stale_dest_error(dest);
            // Retire sources only after exact identities and durable links.
            MoveOutcome::Moved
        }
        Err(e) => {
            if !copy_monitor::was_stalled(&e) && !shutdown_interrupted(&e) {
                crate::server::log::syslog(&format!("Copy failed for {dest_str}: {e}"));
                note_failure(e.to_string());
            }
            copy_failure_outcome(&e)
        }
    }
}

// The candidate ends in .part-PID, not the media extension: check by `dest`'s type.
// The MoveError travels inside the io::Error so the outcome keeps its kind.
fn check_candidate(src: &Path, dest: &Path, candidate: &Path) -> std::io::Result<()> {
    if let Some(check) = structural_check(dest) {
        check(candidate).map_err(std::io::Error::other)?;
    }
    check_post_copy(src, candidate).map_err(std::io::Error::other)
}

// Cancellation (the only source of `Interrupted` here) caused by server shutdown.
fn shutdown_interrupted(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::Interrupted
        && crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
}

fn log_interrupted_delivery(dir: &str) {
    crate::server::log::syslog(&format!(
        "Delivery of {dir} interrupted by shutdown; it resumes on the next start"
    ));
}

#[cfg(not(test))]
fn copy_stall_window(_job: &Path) -> std::time::Duration {
    std::time::Duration::from_secs(60)
}
#[cfg(not(test))]
fn copy_poll_interval() -> std::time::Duration {
    std::time::Duration::from_millis(250)
}
#[cfg(test)]
fn copy_stall_window(job: &Path) -> std::time::Duration {
    test_hooks::stall_window(job)
}
#[cfg(test)]
fn copy_poll_interval() -> std::time::Duration {
    std::time::Duration::from_millis(5)
}

// Test seams: a short stall window and a hook that blocks inside the copy worker,
// scoped to jobs under one path so concurrent tests are not affected.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    pub type Hook = Arc<dyn Fn() + Send + Sync>;
    type Entry = (u64, PathBuf, Duration, Hook);
    static HOOKS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    static NEXT: AtomicU64 = AtomicU64::new(0);
    /// Removes only the hooks its own `set` installed.
    pub struct Reset(u64);
    impl Drop for Reset {
        fn drop(&mut self) {
            HOOKS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(id, ..)| *id != self.0);
        }
    }
    pub fn set(scope: &Path, window: Duration, hook: Hook) -> Reset {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        HOOKS.lock().unwrap_or_else(|e| e.into_inner()).push((
            id,
            scope.to_path_buf(),
            window,
            hook,
        ));
        Reset(id)
    }
    fn scoped(job: &Path) -> Option<(Duration, Hook)> {
        HOOKS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(_, scope, ..)| job.starts_with(scope))
            .map(|(_, _, window, hook)| (*window, hook.clone()))
    }
    pub fn force_copy(job: &Path) -> bool {
        scoped(job).is_some()
    }
    pub fn stall_window(job: &Path) -> Duration {
        scoped(job).map_or(Duration::from_secs(60), |(w, _)| w)
    }
    pub fn before_publish(job: &Path) {
        if let Some((_, hook)) = scoped(job) {
            hook();
        }
    }
    type Counter = (u64, PathBuf, Arc<AtomicU64>);
    static PREFLIGHTS: Mutex<Vec<Counter>> = Mutex::new(Vec::new());
    /// Counts the write tests of roots under its scope until dropped.
    pub struct Preflights(u64, Arc<AtomicU64>);
    impl Preflights {
        pub fn count(&self) -> u64 {
            self.1.load(Ordering::SeqCst)
        }
    }
    impl Drop for Preflights {
        fn drop(&mut self) {
            PREFLIGHTS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(id, ..)| *id != self.0);
        }
    }
    pub fn count_preflights(scope: &Path) -> Preflights {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let n = Arc::new(AtomicU64::new(0));
        PREFLIGHTS.lock().unwrap_or_else(|e| e.into_inner()).push((
            id,
            scope.to_path_buf(),
            n.clone(),
        ));
        Preflights(id, n)
    }
    type Watch = (u64, PathBuf, Hook);
    static AFTER_PREFLIGHT: Mutex<Vec<Watch>> = Mutex::new(Vec::new());
    /// Runs `hook` after each write test of a root under `scope` has answered, until dropped.
    pub struct Watching(u64);
    impl Drop for Watching {
        fn drop(&mut self) {
            AFTER_PREFLIGHT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(id, ..)| *id != self.0);
        }
    }
    pub fn on_preflight(scope: &Path, hook: Hook) -> Watching {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        AFTER_PREFLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, scope.to_path_buf(), hook));
        Watching(id)
    }
    pub fn after_preflight(root: &Path) {
        let hooks: Vec<Hook> = AFTER_PREFLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, scope, _)| root.starts_with(scope))
            .map(|(_, _, hook)| hook.clone())
            .collect();
        for hook in hooks {
            hook();
        }
    }
    pub fn preflighted(root: &Path) {
        for (_, scope, n) in PREFLIGHTS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            if root.starts_with(scope) {
                n.fetch_add(1, Ordering::SeqCst);
            }
        }
    }
}

fn copy_failure_outcome(error: &std::io::Error) -> MoveOutcome {
    if copy_monitor::was_stalled(error) {
        return MoveOutcome::Stalled;
    }
    if shutdown_interrupted(error) {
        return MoveOutcome::Interrupted;
    }
    if let Some(invalid) = error.get_ref().and_then(|e| e.downcast_ref::<MoveError>()) {
        return match invalid {
            MoveError::SizeDoesNotMatch { .. } => MoveOutcome::SizeMismatch,
            _ => MoveOutcome::PostCopyInvalid,
        };
    }
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => MoveOutcome::Collision,
        std::io::ErrorKind::Unsupported => MoveOutcome::Unsupported,
        _ => MoveOutcome::Failed,
    }
}

#[cfg(test)]
#[path = "mover_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mover_upgrade_tests.rs"]
mod upgrade_tests;

#[cfg(test)]
#[path = "mover_retry_tests.rs"]
mod retry_tests;
