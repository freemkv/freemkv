//! Background mux worker — pipelines mux behind the drive thread.
//!
//! Mirrors [`crate::server::mover`]: a 10-second tick loop polls the staging dir for
//! disc state, dispatching each `state: Ripped` dir (`mux_dispatch_verdict`)
//! through the resume-mux path, then transitioning to `Done`/`Review` via
//! `staging::mark_handoff`. On failure it records a `MuxerError` and leaves
//! the dir `Ripped` for next-tick retry / operator inspection. Single-pass
//! live-disc rips (`cfg.max_retries == 0`) stay inline; this worker no-ops.

use crate::server::config::Config;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

/// Hand-off marker written by `ripper::rip_disc` after sweep + patch
/// complete, picked up by this worker on the next tick. Lives at
/// `<staging>/<disc>/.ripped`.
///
/// Captures the minimum the mux side needs that isn't re-derived from the
/// ISO + mapfile + scan_image — TMDB metadata, display naming, cfg-bound
/// knobs, and rip-side stats for the history record. Title-related fields
/// (streams, codecs, duration) are re-derived by `Disc::scan_image`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RippedMarker {
    pub schema_version: u32, // currently 1
    pub iso_path: String,
    pub mapfile_path: String,
    pub display_name: String,
    pub disc_format: String,
    pub mkv_filename: String,
    pub tmdb_title: String,
    pub tmdb_year: u16,
    pub tmdb_poster: String,
    pub tmdb_overview: String,
    /// TMDB media type ("movie" or "tv"). `#[serde(default)]` (empty
    /// string) for backward-compat with pre-rc.4 markers that predate
    /// this field; the resume mux path falls back to "movie" when empty,
    /// matching the mover's own default.
    #[serde(default)]
    pub tmdb_media_type: String,
    pub max_retries: u8,
    pub abort_on_lost_secs: u32,
    pub rip_elapsed_secs: f64,
    pub rip_errors: u32,
    pub rip_lost_video_secs: f64,
    pub rip_last_sector: u64,
    pub origin_device: String, // for logging only
    // Sweep-damage snapshot for telemetry continuity on resume.
    // Optional (serde default) for backward-compat with pre-v0.25.12
    // markers that don't have these fields.
    #[serde(default)]
    pub sweep_errors: u32,
    #[serde(default)]
    pub sweep_total_lost_ms: f64,
    #[serde(default)]
    pub sweep_main_lost_ms: f64,
    #[serde(default)]
    pub sweep_num_bad_ranges: u32,
    #[serde(default)]
    pub sweep_largest_gap_ms: f64,
    /// Operator-confidence of the resolved title at hand-off time. True when the fresh-rip path
    /// decided the title is trustworthy enough to auto-file (`.done`) — an exact normalized
    /// match with a year, or an explicit operator override via the '✎ change' picker.
    ///
    /// Optional (serde default `false`) for backward-compat with pre-rc.4
    /// markers that lack the field — those fall back to the match check alone.
    #[serde(default)]
    pub title_confident: bool,
}

pub const RIPPED_MARKER_NAME: &str = ".ripped";
pub const RIPPED_MARKER_SCHEMA: u32 = 1;

pub fn write_marker(staging_dir: &Path, marker: &RippedMarker) -> std::io::Result<()> {
    // The `.ripped` hand-off is now `state: Ripped` in `state.json`. Fold the
    // marker in (preserving accumulated data / a TV caller's `outputs`) and
    // persist; propagate I/O errors so eject can be refused on a failed hand-off.
    let mut st = crate::server::ripper::staging::state_for_write(staging_dir, RIPPED_STATE)?;
    st.state = RIPPED_STATE;
    st.apply_ripped(marker);
    crate::server::ripper::staging::try_write_state(staging_dir, &st)?;
    // The hand-off supersedes the in-progress `.sweeping` state; clearing is a
    // no-op on `state.json` now that `state == Ripped`, but it strips any legacy
    // `.sweeping` file on a migrated dir.
    crate::server::ripper::staging::clear_sweeping_marker(staging_dir);
    Ok(())
}

const RIPPED_STATE: crate::server::ripper::staging::StagingState =
    crate::server::ripper::staging::StagingState::Ripped;

pub fn read_marker(staging_dir: &Path) -> std::io::Result<RippedMarker> {
    // Unified store wins: reconstruct the `RippedMarker` the mux path deals in.
    if let Some(st) = crate::server::ripper::staging::read_state(staging_dir) {
        return Ok(st.to_ripped_marker());
    }
    // Legacy fallback: a pre-migration `.ripped` file.
    let path = staging_dir.join(RIPPED_MARKER_NAME);
    let bytes = std::fs::read(path)?;
    let marker: RippedMarker = serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if marker.schema_version != RIPPED_MARKER_SCHEMA {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "unsupported .ripped schema_version {} (expected {})",
                marker.schema_version, RIPPED_MARKER_SCHEMA
            ),
        ));
    }
    Ok(marker)
}

/// Strip any lingering legacy `.ripped` file on mux success (the lifecycle
/// transition `Ripped` → `Done`/`Review` → `Completed` lives in `state.json`).
/// An absent file is `Ok`; any other remove error is returned.
pub fn delete_marker(staging_dir: &Path) -> std::io::Result<()> {
    let path = staging_dir.join(RIPPED_MARKER_NAME);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Per-staging-dir error surfaced to the System page so the user can
/// act on it (e.g. `MuxFinalize` after an NFS hiccup that left the MKV
/// unseekable). Keyed by staging dir path; same `reason` for the same
/// path is idempotent — no log spam on retry ticks.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MuxerError {
    pub path: String,
    pub reason: String,
    pub hint: String,
}

pub static MUX_ERRORS: once_cell::sync::Lazy<Mutex<BTreeMap<String, MuxerError>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(BTreeMap::new()));

/// Paths the operator has dismissed (the System-tab ✕ / Clear-all). A dismissed path is
/// suppressed from re-recording, so a persistently-erroring dir stays cleared instead of
/// reappearing every tick. Lifted when the dir is freshly DISPATCHED (a new mux attempt may
/// produce a new error worth showing) or when the dir is pruned (gone from staging).
pub static MUX_DISMISSED: once_cell::sync::Lazy<Mutex<std::collections::BTreeSet<String>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(std::collections::BTreeSet::new()));

pub(crate) fn reset_after_drain() {
    MUX_ERRORS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    MUX_DISMISSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

// Operator hint for a loss-abort error card: deterministic media damage
// that won't clear on its own, so point at the two real resolutions
// instead of implying a retry will help.
pub(crate) const ABORTED_LOSS_HINT: &str = "the delivered title lost more data than 'abort_on_lost_secs' allows, so this rip will NOT auto-retry (an identical re-mux reproduces the same loss). Re-insert the disc to Accept & deliver it as-is or run another recovery pass — or raise 'abort_on_lost_secs' in Settings first, then re-insert to deliver it automatically.";

pub(crate) fn record_error(path: &str, reason: &str, hint: &str) {
    record_error_announced(path, reason, hint, true);
}

// `record_error`, but `announce = false` suppresses the syslog line (a repeat the
// dispatch-time clear would otherwise re-announce every tick).
fn record_error_announced(path: &str, reason: &str, hint: &str, announce: bool) {
    // Capture whether this is a new reason under the lock, then DROP the
    // guard before the syslog write (syslog does blocking NFS I/O) so it
    // doesn't block other record_error/clear_error calls or the System page.
    let same_reason = {
        let mut m = MUX_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
        // Operator dismissed this path: honor it. Checked under MUX_ERRORS (the
        // clear_mux_error lock order) so a concurrent dismissal can't be lost.
        if MUX_DISMISSED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(path)
        {
            return;
        }
        let same_reason = m.get(path).map(|e| e.reason == reason).unwrap_or(false);
        m.insert(
            path.to_string(),
            MuxerError {
                path: path.to_string(),
                reason: reason.to_string(),
                hint: hint.to_string(),
            },
        );
        same_reason
    };
    if announce && !same_reason {
        crate::server::log::syslog(&format!("Mux blocked: {} — {}", path, reason));
    }
}

// Whether `path`'s current card carries `hint` (identifies a repeat of the same cause).
fn error_hint_is(path: &str, hint: &str) -> bool {
    MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .is_some_and(|e| e.hint == hint)
}

pub(crate) fn clear_error(path: &str) {
    MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// Clear `path`'s card only if its reason starts with `prefix` (a condition
/// that has since resolved), leaving any other failure's card in place.
pub(crate) fn clear_error_with_prefix(path: &str, prefix: &str) {
    let mut m = MUX_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    if m.get(path).is_some_and(|e| e.reason.starts_with(prefix)) {
        m.remove(path);
    }
}

/// Operator-initiated clear of a single mux error (the System-tab ✕). Removes
/// the card AND marks the path dismissed so a persistently-erroring dir doesn't
/// re-surface it on the next tick; the dismissal is lifted on the dir's next
/// fresh dispatch (or when it's pruned from staging). Only a path with a recorded error is
/// dismissed, so arbitrary `?path=` values can't grow MUX_DISMISSED.
pub fn clear_mux_error(path: &str) {
    let mut m = MUX_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    if m.remove(path).is_some() {
        MUX_DISMISSED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_string());
    }
}

/// Operator-initiated clear of ALL mux errors (the System-tab "Clear all").
pub fn clear_all_mux_errors() {
    let mut m = MUX_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    {
        let mut d = MUX_DISMISSED.lock().unwrap_or_else(|e| e.into_inner());
        for k in m.keys() {
            d.insert(k.clone());
        }
    }
    m.clear();
}

/// Lift any dismissal for `path` — called when the dir is freshly dispatched so
/// a NEW mux attempt's error (if any) can surface again.
fn undismiss(path: &str) {
    MUX_DISMISSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// Drop error cards (and dismissals) whose staging dir no longer exists — an
/// "old error hanging around" for a disc that has been delivered, deleted, or
/// moved out of staging. Keeps the System page showing only live jobs.
fn prune_stale_errors() {
    prune_stale_errors_with(definitely_absent);
}

// prune_stale_errors with the absence probe injected. Neither lock is held while probing:
// the probe is a stat, which blocks on a hung NFS mount and would stall every mux error reader.
fn prune_stale_errors_with(absent: impl Fn(&str) -> bool) {
    let mut candidates: Vec<String> = MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    candidates.extend(
        MUX_DISMISSED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned(),
    );
    candidates.sort();
    candidates.dedup();
    let stale: Vec<String> = candidates.into_iter().filter(|p| absent(p)).collect();
    if stale.is_empty() {
        return;
    }
    {
        let mut m = MUX_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
        for p in &stale {
            m.remove(p);
        }
    }
    // Removal trusts the unlocked probe (re-probing here would be I/O under the lock). Worst
    // case, a dir recreated mid-prune loses its dismissal and its next error card shows once.
    let mut d = MUX_DISMISSED.lock().unwrap_or_else(|e| e.into_inner());
    for p in &stale {
        d.remove(p);
    }
}

/// True only when the staging dir is DEFINITIVELY gone (stat returned NotFound).
/// `Path::exists()` collapses every stat error — EACCES, EIO, ESTALE on an NFS
/// blip — into `false`, which would evict a still-live error card the moment the
/// mount hiccups. Prune only on a real NotFound; treat any other error as
/// "still there, unknown" and keep the card.
fn definitely_absent(path: &str) -> bool {
    matches!(
        std::fs::symlink_metadata(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound
    )
}

/// Worker entry point — spawn from `main` alongside the mover thread.
///
/// A 10-second tick loop: each tick scans the staging dir for `.ripped`
/// hand-off markers (`check_and_mux`) and dispatches each through the
/// resume-mux path (`remux_from_ripped_marker`). On success the dir gets
/// a `.done`/`.completed` marker (handed to the mover) and `.ripped` is
/// deleted; on failure `.ripped` stays for next-tick retry and a
/// `MuxerError` surfaces to the System page. SHUTDOWN-responsive.
pub fn run(cfg: &Arc<RwLock<Config>>) {
    use std::sync::atomic::Ordering;
    tracing::info!("mux loop starting");
    while !crate::server::SHUTDOWN.load(Ordering::Relaxed) {
        // A poisoned RwLock never un-poisons, so a bare `is_err()` here would
        // spin forever (worker never muxes/exits, /api/state stays "healthy").
        // Recover from poison instead (see check_and_mux's `into_inner`).
        check_and_mux(cfg);
        // SHUTDOWN-responsive sleep — same pattern as the mover so
        // SIGTERM doesn't have to wait the full 10 s tick.
        for _ in 0..100 {
            if crate::server::SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    tracing::info!("mux loop stopping");
}

// Verdict for whether the mux worker should act on one staging dir this
// tick. Pure projection of the dir's marker state, unit-testable via
// `mux_dispatch_verdict`; `check_and_mux` turns `Dispatch` into a real mux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MuxVerdict {
    /// `.ripped` present, no terminal marker, listing trustworthy — run the mux.
    Dispatch,
    /// `.completed` or `.failed` present — finished or quarantined, never re-mux.
    SkipTerminal,
    /// `.aborted-loss` present — the mux ran to completion but the delivered
    /// title carried more decrypt/codec loss than `abort_on_lost_secs` allows.
    /// That loss is DETERMINISTIC media damage: re-muxing the same ISO with the
    /// same keys reproduces the exact same loss, so auto-retrying every tick
    /// just re-muxes forever. This is RESUMABLE and operator-resolved (Accept &
    /// deliver, run another recovery pass, or raise the threshold), matching the
    /// drive-side classifier in `staging.rs`. The worker surfaces the reason and
    /// stops re-dispatching.
    SkipAbortedLoss,
    /// `state.json` exists but can't be used, so the deliverable plan is unknown.
    /// Held for the operator (one card, no re-dispatch, state.json untouched).
    SkipUnreadableState,
    /// A `.ripped` dir held for its disc (E7034): only the disc's Volume ID can finish
    /// its keys, so re-muxing without it asks the key service for the same refusal.
    /// Held with one card until the disc is inserted (which finishes it).
    SkipNeedsDisc,
    /// No `.ripped` hand-off marker — nothing for the worker to do here.
    SkipNoMarker,
    /// Snapshot is `None` — the dir's contents are UNKNOWN (read_dir / DirEntry
    /// errors mid-scan). Skip this tick rather than dispatch on an untrustworthy
    /// listing; retry next tick.
    SkipUnknown,
}

// Pure dispatch decider. `snap` is `snapshot_staging_disc` (`None` ⇒ UNKNOWN). Order:
// None→SkipUnknown, terminal→SkipTerminal, unreadable state.json→SkipUnreadableState,
// aborted-loss→SkipAbortedLoss, no marker→SkipNoMarker, else→Dispatch.
pub(crate) fn mux_dispatch_verdict(
    snap: Option<&crate::server::ripper::staging::StagingSnapshot>,
) -> MuxVerdict {
    let Some(snap) = snap else {
        return MuxVerdict::SkipUnknown;
    };
    // Terminal on `.failed` PRESENCE, not a parseable reason: review.rs
    // writes a non-JSON `.failed` whose `failed_reason` is None, and keying
    // on `failed_reason.is_some()` would re-dispatch that dir forever.
    if snap.completed || snap.has_failed {
        return MuxVerdict::SkipTerminal;
    }
    if snap.state_unreadable.is_some() {
        return MuxVerdict::SkipUnreadableState;
    }
    // A loss-abort is deterministic media damage — retrying re-muxes the whole
    // ISO every tick for the same result. Stop and surface the reason for the
    // operator (Accept, another pass, raised threshold), per `staging.rs`.
    if snap.has_aborted_loss {
        return MuxVerdict::SkipAbortedLoss;
    }
    if snap.needs_disc {
        return MuxVerdict::SkipNeedsDisc;
    }
    if !snap.has_ripped {
        return MuxVerdict::SkipNoMarker;
    }
    MuxVerdict::Dispatch
}

// RAII cleanup for the `.muxing` exclusion lock: cleared on every exit of a
// check_and_mux iteration (success, failure, or panic) so a crashed mux never
// strands a stale lock hiding the dir from drive-resume paths.
struct MuxingGuard<'a>(&'a Path);

impl Drop for MuxingGuard<'_> {
    fn drop(&mut self) {
        crate::server::ripper::staging::clear_muxing_marker(self.0);
    }
}

// Whether a mux-worker failure is TERMINAL (state → Failed, so `mux_dispatch_verdict` stops
// re-Dispatching) vs resumable. Terminal IFF a structural FINALIZE failure.
pub(crate) struct MuxFailureClass {
    /// The mux completed but delivered loss exceeded threshold (`.aborted-loss`).
    /// It owns its own resumable state and must never be quarantined here.
    pub(crate) aborted_loss: bool,
    /// The worker learned a concrete failure reason from the `_mux` device state
    /// (a finalize always carries one — kept as a defensive precondition).
    pub(crate) has_worker_reason: bool,
    /// A structural FINALIZE failure surfaced (`failure_finalize`) — the sole
    /// terminal signal.
    pub(crate) is_finalize: bool,
}

pub(crate) fn mux_failure_is_terminal(class: MuxFailureClass) -> bool {
    !class.aborted_loss && class.has_worker_reason && class.is_finalize
}

// Persist the terminal `.failed` quarantine; if the state.json write does NOT land, surface it
// LOUD (syslog + operator card) instead of silently leaving the dir re-dispatching forever.
pub(crate) fn persist_terminal_mux_quarantine(path_str: &str, dir: &Path, reason: &str) -> bool {
    let landed = crate::server::ripper::staging::write_failed_marker(dir, reason);
    if !landed {
        if let crate::server::ripper::staging::StateRead::Unreadable(u) =
            crate::server::ripper::staging::read_state_checked(dir)
        {
            record_error(path_str, &u.held_reason(), u.hint());
            return false;
        }
        crate::server::log::syslog(&format!(
            "Mux quarantine FAILED to persist (state.json write error) — {path_str} will keep re-dispatching until the staging mount recovers"
        ));
        record_error(
            path_str,
            reason,
            "the terminal quarantine could not be written to state.json (staging mount full / unwritable); the mux will keep retrying until the mount recovers — free space or fix permissions on the staging share",
        );
    }
    landed
}

// Raise the card for a failed worker mux; true when it quarantined the dir. A TERMINAL failure
// (structural finalize, e.g. E6008) transitions state → Failed so dispatch stops; when that
// write doesn't land, its own card (why the dir keeps retrying) is the one kept.
fn record_mux_failure(
    path_str: &str,
    dir: &Path,
    reason: &str,
    hint: &str,
    terminal: bool,
    announce: bool,
) -> bool {
    let quarantined = terminal && persist_terminal_mux_quarantine(path_str, dir, reason);
    if terminal && !quarantined {
        return false;
    }
    record_error_announced(path_str, reason, hint, announce);
    quarantined
}

// Find all staging dirs with a `.ripped` marker and dispatch each through
// the resume-mux path. Serialized — only one mux runs at a time in this
// worker thread; concurrent muxes are explicitly out of scope.
fn check_and_mux(cfg_arc: &Arc<RwLock<Config>>) {
    // Recover from a poisoned config lock rather than returning (which,
    // combined with the per-tick loop, would silently wedge the worker
    // forever). This borrow only reads the staging path.
    let staging_root = cfg_arc
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .staging_dir
        .clone();
    // Clear out error cards for staging dirs that have since been delivered,
    // deleted, or moved away — otherwise they linger on the System page.
    prune_stale_errors();
    let entries = match std::fs::read_dir(&staging_root) {
        Ok(e) => e,
        Err(e) => {
            // A dropped NFS mount or a deleted staging dir would otherwise
            // silently freeze every future tick. Surface it so the operator
            // sees a paused mux queue instead of a frozen one.
            tracing::warn!("mux: cannot read staging dir {staging_root:?}: {e}");
            record_error(
                &staging_root,
                &format!("cannot read staging dir: {e}"),
                "check the staging mount (NFS) is up and the dir exists; mux is paused until it is readable",
            );
            return;
        }
    };
    // The staging dir is readable again — clear any prior "cannot read"
    // error so the System page doesn't show a stale alarm.
    clear_error(&staging_root);
    for entry in entries {
        // A per-entry error (NFS stat hiccup, a racing rename) must not
        // silently drop a staged dir from the mux queue and strand a
        // finished rip. Surface it and move on to the next entry.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("mux: skipping unreadable staging entry: {e}");
                record_error(
                    &staging_root,
                    &format!("unreadable staging entry: {e}"),
                    "a staging dir entry could not be read (NFS stat error / racing rename); it is skipped this tick and retried next tick",
                );
                continue;
            }
        };
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // Never re-mux a finished dir: if `.ripped` survives a failed
        // post-mux delete, `.completed` (via the primed/retried
        // `snapshot_staging_disc`) still breaks the loop, per `mux_dispatch_verdict`.
        let snap = crate::server::ripper::staging::snapshot_staging_disc(&dir);
        let verdict = mux_dispatch_verdict(snap.as_ref());
        if !matches!(
            verdict,
            MuxVerdict::SkipUnreadableState | MuxVerdict::SkipUnknown
        ) {
            clear_error_with_prefix(
                &dir.to_string_lossy(),
                crate::server::ripper::staging::STATE_HELD_PREFIX,
            );
        }
        match verdict {
            MuxVerdict::Dispatch => {
                // Stamp `.muxing` the INSTANT Dispatch commits (before reading the
                // marker) so `muxing_status` covers the whole dispatch — writing it later
                // left a TOCTOU where a web entry raced the state.json read-modify-write.
                crate::server::ripper::staging::write_muxing_marker(&dir);
            }
            MuxVerdict::SkipAbortedLoss => {
                // Delivered loss exceeded threshold — DON'T re-mux (deterministic,
                // would reproduce identically). Surface reason + hint once and leave
                // the dir untouched; `record_error` de-dupes by reason (no log spam).
                let reason = snap
                    .as_ref()
                    .and_then(|s| s.aborted_loss_reason.clone())
                    .unwrap_or_else(|| "aborted: loss exceeded threshold".to_string());
                record_error(&dir.to_string_lossy(), &reason, ABORTED_LOSS_HINT);
                continue;
            }
            MuxVerdict::SkipNeedsDisc => {
                // E7034: one de-duped card and no dispatch — the disc, not a retry, fixes it.
                record_error(
                    &dir.to_string_lossy(),
                    &crate::server::ripper::vid_needs_disc_text(),
                    NEEDS_DISC_HINT,
                );
                continue;
            }
            MuxVerdict::SkipUnreadableState => {
                // Hold like SkipAbortedLoss: one de-duped card, no dispatch (so no
                // undismiss, no mux-log spam) and state.json left for the operator.
                if let Some(u) = snap.as_ref().and_then(|s| s.state_unreadable.as_ref()) {
                    record_error(&dir.to_string_lossy(), &u.held_reason(), u.hint());
                }
                continue;
            }
            MuxVerdict::SkipTerminal | MuxVerdict::SkipNoMarker | MuxVerdict::SkipUnknown => {
                continue;
            }
        }
        // Own the `.muxing` lock (stamped at verdict-commit) for the rest of this
        // iteration. Created BEFORE `read_marker` so its Drop clears it on the
        // marker-read `continue` paths too — no stuck lock on a malformed marker.
        let _guard = MuxingGuard(&dir);
        let marker = match read_marker(&dir) {
            Ok(m) => m,
            // TOCTOU: the `.exists()` check and this read race a concurrent
            // cleanup. A vanished marker isn't malformed — skip silently rather
            // than recording a spurious "No such file" error that sticks around.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                let path_str = dir.to_string_lossy().to_string();
                record_error(
                    &path_str,
                    &format!("malformed .ripped marker: {e}"),
                    "delete the .ripped file (or the whole staging dir) and re-run the rip; the marker schema may be out of date",
                );
                continue;
            }
        };
        // Sanitised once here, feeding three sinks below (two tracing fields +
        // syslog): `display_name` falls back to the disc's own raw meta_title /
        // volume_id — attacker-controlled bytes — when no TMDB match was found.
        let title = crate::server::log::sanitize_log_msg(&marker.display_name);
        tracing::info!(
            staging = %dir.display(),
            title = %title,
            "mux worker: dispatching .ripped marker"
        );
        crate::server::log::syslog(&format!("Muxing: {} (worker)", title));
        // Exclusion lock for the mux duration (stamped at verdict-commit, owned
        // by `_guard`) blocks concurrent re-inserts/double-mux until `.completed`/
        // `.failed`/`.ripped` take over; also clear any stale error card now.
        let prior_space_refusal = error_hint_is(&dir.to_string_lossy(), STAGING_SPACE_HINT);
        clear_error(&dir.to_string_lossy());
        // A fresh dispatch may produce a new/different error — lift any prior
        // operator dismissal so a genuinely new failure can surface again.
        undismiss(&dir.to_string_lossy());
        let mux_slot = crate::server::library::arbiter::claim_for_rip();
        let outcome =
            crate::server::ripper::resume::remux_from_ripped_marker(cfg_arc, &dir, &marker);
        drop(mux_slot);
        if outcome.success {
            clear_error(&dir.to_string_lossy());
            tracing::info!(staging = %dir.display(), title = %title, "mux worker: completed");
            crate::server::log::syslog(&format!("Muxed: {}", title));
            // Defensive: drive the origin device to "done" ONLY if it's still
            // "ripping" (a no-op on the normal path; fires for the inline-mux
            // fallback). Never reverts a real "done" tile or a reused device.
            let origin = &marker.origin_device;
            if !origin.is_empty() {
                let origin_tile = crate::server::ripper::STATE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(origin.as_str())
                    .map(|rs| (rs.status.clone(), rs.disc_name.clone()));
                let origin_tile = origin_tile.as_ref().map(|(s, n)| (s.as_str(), n.as_str()));
                if should_revert_origin_to_done(origin, origin_tile, &marker.display_name) {
                    crate::server::ripper::update_state(
                        origin,
                        origin_done_state(origin, &marker, &outcome),
                    );
                }
            }
        } else {
            let path_str = dir.to_string_lossy().to_string();
            // Surface the ACTUAL failure reason: prefer `outcome.failure_reason`
            // over a stale `.aborted-loss` marker, but check `.aborted-loss`
            // FIRST (read once) since a completed-but-over-threshold mux needs it.
            let aborted_loss = crate::server::ripper::staging::read_aborted_loss(&dir);
            let (reason, hint) = if let Some((r, _)) = &aborted_loss {
                (r.clone(), ABORTED_LOSS_HINT.to_string())
            } else if let Some(r) = outcome.failure_reason.clone() {
                let hint = if outcome.failure_needs_disc {
                    NEEDS_DISC_HINT
                } else {
                    worker_failure_hint(outcome.failure_retryable, outcome.failure_space)
                };
                (r, hint.to_string())
            } else {
                // Defensive fallback (no reason came back from the worker):
                // read the staging markers as before, falling through to
                // `read_failed_reason` or a generic message.
                let reason = crate::server::ripper::staging::read_failed_reason(&dir)
                    .unwrap_or_else(|| {
                        "mux worker dispatch did not complete (see _mux device log)".to_string()
                    });
                (
                    reason,
                    "the mux failed to finalize/write the output — staging is preserved; check the _mux device log for the failure detail and re-run the mux".to_string(),
                )
            };
            let terminal = mux_failure_is_terminal(MuxFailureClass {
                aborted_loss: aborted_loss.is_some(),
                has_worker_reason: outcome.failure_reason.is_some(),
                is_finalize: outcome.failure_finalize,
            });
            let repeat = outcome.failure_space && prior_space_refusal;
            if record_mux_failure(&path_str, &dir, &reason, &hint, terminal, !repeat) {
                // Quarantined: no later mux of this ISO will use the rip's held keys.
                crate::server::keysource::forget_rip_keys(Path::new(&marker.iso_path));
            }
        }
    }
}

// The origin device's "done" tile after a successful worker mux: loss totals and display
// fields come from the mux outcome (sweep + mux-time), identity/TMDB from the marker.
fn origin_done_state(
    origin: &str,
    marker: &RippedMarker,
    outcome: &crate::server::ripper::resume::MuxHandoffOutcome,
) -> crate::server::ripper::RipState {
    crate::server::ripper::RipState {
        device: origin.to_string(),
        status: "done".to_string(),
        disc_present: true,
        disc_name: marker.display_name.clone(),
        disc_format: marker.disc_format.clone(),
        progress_pct: 100,
        // Combined sweep + mux-time loss (the `_mux`
        // done-state folds decrypt skips into mapfile totals);
        // `marker.sweep_*` alone would understate it.
        errors: outcome.errors,
        total_lost_ms: outcome.total_lost_ms,
        main_lost_ms: outcome.main_lost_ms,
        num_bad_ranges: marker.sweep_num_bad_ranges,
        largest_gap_ms: marker.sweep_largest_gap_ms,
        // Bad-ranges drilldown isn't in the marker (summary
        // counts only) so plumb it from the mux outcome;
        // otherwise the tile shows a count but an empty list.
        bad_ranges: outcome.bad_ranges.clone(),
        bad_ranges_truncated: outcome.bad_ranges_truncated,
        tmdb_title: marker.tmdb_title.clone(),
        tmdb_year: marker.tmdb_year,
        tmdb_poster: marker.tmdb_poster.clone(),
        tmdb_overview: marker.tmdb_overview.clone(),
        // Carry mux-derived display fields (codecs, duration,
        // output_file) so the origin device's done card matches
        // the inline fresh-rip card instead of dropping them.
        codecs: outcome.codecs.clone(),
        duration: outcome.duration.clone(),
        output_file: outcome.output_file.clone(),
        // Combined sweep + mux-time loss (see `errors` above);
        // `marker.rip_lost_video_secs` alone would understate
        // it on a disc with accepted mux-phase decrypt loss.
        lost_video_secs: outcome.lost_video_secs,
        ..Default::default()
    }
}

pub(crate) const NEEDS_DISC_HINT: &str = "insert this disc into any drive: the mux finishes from the staged image without re-reading the disc; until then the mux is not retried";

pub(crate) const STAGING_SPACE_HINT: &str = "staging is too full to hold this disc's mux outputs — free space on the staging volume or set Staging Directory in Settings to a larger volume; the disc image stays staged and the mux retries automatically";

// Operator hint for a worker-reported mux failure, by cause.
fn worker_failure_hint(retryable: bool, space: bool) -> &'static str {
    if space {
        STAGING_SPACE_HINT
    } else if retryable {
        // Keyless deferral: re-muxes automatically once keys land.
        "no decryption keys yet — the disc stays staged and will mux automatically once keys are available; if this persists, check the key source in Settings"
    } else {
        "the mux failed to finalize/write the output — staging is preserved; check the _mux device log for the failure detail and re-run the mux"
    }
}

// Should the mux worker drive the origin device to "done"? Only if its tile (status, disc_name)
// is still "ripping" this disc (the inline-mux fallback path) and not a synthetic `_` origin.
pub(crate) fn should_revert_origin_to_done(
    origin: &str,
    tile: Option<(&str, &str)>,
    disc_name: &str,
) -> bool {
    !origin.is_empty() && !origin.starts_with('_') && tile == Some(("ripping", disc_name))
}

/// Scan the staging dir for pending mux jobs. Returns display names
/// for the System page's Mux Queue panel.
pub fn pending_queue(staging_dir: &Path) -> Vec<String> {
    let entries = match std::fs::read_dir(staging_dir) {
        Ok(e) => e,
        Err(e) => {
            // An unreadable staging root is NOT an empty mux queue — a bare
            // `Vec::new()` would render a degraded share as "nothing queued".
            // Log it so the absence of jobs is attributable (see staging.rs).
            tracing::warn!(
                staging_dir = %staging_dir.display(),
                error = %e,
                "could not list staging root for the mux queue; reporting an empty queue this refresh"
            );
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in entries {
        // Don't `.filter_map(|e| e.ok())` a per-entry error away: an ESTALE
        // on one NFS dentry would silently drop a queued title. Same defense
        // as the staging-root scan above.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    staging_dir = %staging_dir.display(),
                    error = %e,
                    "per-entry error listing staging root for the mux queue - skipping this entry, share may be degraded"
                );
                continue;
            }
        };
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // Route queue membership through the unified state snapshot, not bare
        // `.exists()`: "(queued)" iff `Ripped`, not muxing, and not terminal /
        // handed to the mover (`has_done`/`has_review` catch the legacy crash window).
        let Some(snap) = crate::server::ripper::staging::snapshot_staging_disc(&dir) else {
            continue;
        };
        // Skip `.completed`/`.failed` (terminal), `.done`/`.review` (mutual
        // exclusion — already in the Move queue), `.muxing` (live in the `_mux`
        // tile), and `.aborted-loss` (resumable, shown via its own error card).
        if !snap.has_ripped
            || snap.needs_disc
            || snap.has_muxing
            || snap.completed
            || snap.has_failed
            || snap.has_done
            || snap.has_review
            || snap.has_aborted_loss
        {
            continue;
        }
        if let Ok(m) = read_marker(&dir) {
            out.push(format!("{} (queued)", m.display_name));
        } else {
            // Malformed marker — still surface the dir name so the
            // operator notices it sitting in the queue.
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().replace('_', " ").to_string())
                .unwrap_or_default();
            out.push(format!("{} (malformed)", name));
        }
    }
    out
}

#[cfg(test)]
#[path = "muxer_tests.rs"]
mod tests;
