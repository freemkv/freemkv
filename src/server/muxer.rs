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
mod tests {
    use super::*;

    #[test]
    fn space_refusal_gets_a_staging_space_hint() {
        assert_eq!(
            super::worker_failure_hint(false, true),
            super::STAGING_SPACE_HINT
        );
        assert!(super::STAGING_SPACE_HINT.contains("Staging Directory in Settings"));
        assert!(super::worker_failure_hint(false, false).contains("finalize"));
        assert!(super::worker_failure_hint(true, false).contains("decryption keys"));
    }
    use tempfile::TempDir;

    // The absence probe is a stat that can block on a hung NFS mount; neither error map may be
    // locked while it runs.
    #[test]
    fn prune_stale_errors_probes_without_holding_locks() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let err_path = "/x/staging/prune-lock-err";
        let dis_path = "/x/staging/prune-lock-dismissed";
        record_error(err_path, "r", "h");
        MUX_DISMISSED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dis_path.to_string());
        let probed = Mutex::new(Vec::<(String, bool, bool)>::new());
        prune_stale_errors_with(|p| {
            if p == err_path || p == dis_path {
                let e = crate::server::mover::test_lock_is_free(&MUX_ERRORS);
                let d = crate::server::mover::test_lock_is_free(&MUX_DISMISSED);
                probed.lock().unwrap().push((p.to_string(), e, d));
                return true;
            }
            false
        });
        let probed = probed.into_inner().unwrap();
        let err_left = MUX_ERRORS.lock().unwrap().contains_key(err_path);
        let dis_left = MUX_DISMISSED.lock().unwrap().contains(dis_path);
        assert_eq!(
            probed.len(),
            2,
            "both maps' keys must be probed: {probed:?}"
        );
        for (p, e, d) in &probed {
            assert!(*e, "MUX_ERRORS held while probing {p}");
            assert!(*d, "MUX_DISMISSED held while probing {p}");
        }
        assert!(!err_left, "an absent dir's error card must be pruned");
        assert!(!dis_left, "an absent dir's dismissal must be pruned");
    }

    // Real-filesystem probe: gone dirs are pruned, a live dir keeps its card. Filtered to this
    // test's own paths so it can't prune other tests' fake /x/staging entries.
    #[test]
    fn prune_stale_errors_drops_only_absent_dirs() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let live = tmp.path().join("Live").to_string_lossy().to_string();
        let gone = tmp.path().join("Gone").to_string_lossy().to_string();
        let gone_dismissed = tmp
            .path()
            .join("GoneDismissed")
            .to_string_lossy()
            .to_string();
        std::fs::create_dir_all(&live).unwrap();
        record_error(&live, "r", "h");
        record_error(&gone, "r", "h");
        MUX_DISMISSED.lock().unwrap().insert(gone_dismissed.clone());
        let root = tmp.path().to_string_lossy().to_string();
        prune_stale_errors_with(|p| p.starts_with(&root) && definitely_absent(p));
        let live_kept = MUX_ERRORS.lock().unwrap().contains_key(&live);
        let gone_kept = MUX_ERRORS.lock().unwrap().contains_key(&gone);
        let dismissed_kept = MUX_DISMISSED.lock().unwrap().contains(&gone_dismissed);
        clear_error(&live);
        assert!(
            live_kept,
            "a still-present staging dir must keep its error card"
        );
        assert!(!gone_kept, "a vanished staging dir's card must be pruned");
        assert!(
            !dismissed_kept,
            "a vanished staging dir's dismissal must be pruned"
        );
    }

    #[test]
    fn definitely_absent_only_on_not_found() {
        let tmp = TempDir::new().unwrap();
        let present = tmp.path().join("present");
        std::fs::create_dir_all(&present).unwrap();
        assert!(!definitely_absent(&present.to_string_lossy()));
        assert!(definitely_absent(
            &tmp.path().join("missing").to_string_lossy()
        ));
    }

    // EACCES (or an NFS EIO/ESTALE) proves nothing about the dir: the card must survive it.
    #[cfg(unix)]
    #[test]
    fn definitely_absent_false_on_permission_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let locked = tmp.path().join("locked");
        let inner = locked.join("Disc");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let enforced = std::fs::symlink_metadata(&inner).is_err();
        let absent = definitely_absent(&inner.to_string_lossy());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !enforced {
            eprintln!("SKIP definitely_absent_false_on_permission_error: running as root");
            return;
        }
        assert!(!absent, "a permission error must not count as absent");
    }

    #[test]
    fn record_and_clear_error_round_trip() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        record_error("/x/staging/Foo", "test reason", "test hint");
        {
            let m = MUX_ERRORS.lock().unwrap();
            assert!(m.contains_key("/x/staging/Foo"));
            assert_eq!(m["/x/staging/Foo"].reason, "test reason");
        }
        clear_error("/x/staging/Foo");
        let m = MUX_ERRORS.lock().unwrap();
        assert!(!m.contains_key("/x/staging/Foo"));
    }

    // A present-but-unreadable state.json must be held ONCE by the worker: one
    // actionable card, no re-dispatch (so an operator dismissal sticks), and the
    // bad state.json left untouched — with or without a legacy `.ripped` remnant.
    #[test]
    fn unreadable_state_json_is_held_once_not_redispatched() {
        let _guard = crate::server::log::env_guard();
        // check_and_mux prunes every MUX_ERRORS key: serialize with the other card tests.
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        // SAFETY: env access in tests, serialized by env_guard.
        unsafe {
            std::env::set_var("AUTORIP_DIR", tmp.path());
        }
        let root = tmp.path().join("staging");
        let legacy = root.join("Legacy_Show");
        let bare = root.join("Bare_Show");
        for d in [&legacy, &bare] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(
                d.join(crate::server::ripper::staging::STATE_FILE),
                b"{ torn",
            )
            .unwrap();
        }
        std::fs::write(
            legacy.join(RIPPED_MARKER_NAME),
            serde_json::to_vec(&sample_marker()).unwrap(),
        )
        .unwrap();
        for d in [&legacy, &bare] {
            let snap = crate::server::ripper::staging::snapshot_staging_disc(d);
            assert_eq!(
                mux_dispatch_verdict(snap.as_ref()),
                MuxVerdict::SkipUnreadableState,
                "{}",
                d.display()
            );
        }

        let cfg = Arc::new(RwLock::new(Config {
            staging_dir: root.to_string_lossy().to_string(),
            ..Config::default()
        }));
        check_and_mux(&cfg);
        let path = legacy.to_string_lossy().to_string();
        let card = MUX_ERRORS.lock().unwrap().get(&path).cloned();
        let card = card.expect("the held dir must raise an operator card");
        assert!(
            card.reason
                .starts_with(crate::server::ripper::staging::STATE_HELD_PREFIX)
        );
        assert!(card.hint.contains("state.json"), "hint: {}", card.hint);
        assert!(
            MUX_ERRORS
                .lock()
                .unwrap()
                .contains_key(&bare.to_string_lossy().to_string()),
            "the snapshot-fallback (no .ripped) case must surface too"
        );

        // Operator dismisses; the next tick must not re-dispatch and undo that.
        clear_mux_error(&path);
        check_and_mux(&cfg);
        assert!(
            !MUX_ERRORS.lock().unwrap().contains_key(&path),
            "a held dir must not be re-dispatched (which undismisses the card)"
        );
        assert_eq!(
            std::fs::read(legacy.join(crate::server::ripper::staging::STATE_FILE)).unwrap(),
            b"{ torn",
            "the unreadable state.json must be left for the operator"
        );

        // Repaired state.json → the held card clears on the next tick.
        crate::server::ripper::staging::write_state(
            &bare,
            &crate::server::ripper::staging::DiscState::new(
                crate::server::ripper::staging::StagingState::Sweeping,
            ),
        );
        check_and_mux(&cfg);
        assert!(
            !MUX_ERRORS
                .lock()
                .unwrap()
                .contains_key(&bare.to_string_lossy().to_string()),
            "a repaired dir's held card must clear"
        );
        MUX_DISMISSED.lock().unwrap().remove(&path);
    }

    // A dismiss for a path with no recorded error must not grow MUX_DISMISSED.
    #[test]
    fn clearing_an_unrecorded_path_does_not_dismiss_it() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let junk = "/etc/hostname/not-a-staging-dir-webcaps-l2";
        clear_mux_error(junk);
        assert!(
            !MUX_DISMISSED.lock().unwrap().contains(junk),
            "an arbitrary path was added to MUX_DISMISSED"
        );
    }

    // The legitimate System-tab dismiss still suppresses re-recording. A real dir, so a
    // concurrent prune can't lift the dismissal mid-test.
    #[test]
    fn dismissing_a_recorded_error_suppresses_rerecording() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().to_string_lossy().to_string();
        record_error(&p, "r", "h");
        clear_mux_error(&p);
        record_error(&p, "r", "h");
        let rerecorded = MUX_ERRORS.lock().unwrap().contains_key(&p);
        undismiss(&p);
        clear_error(&p);
        assert!(!rerecorded, "a dismissed error re-surfaced");
    }

    // A quarantine refused because state.json is unreadable must show the held
    // card, not the "staging mount full / unwritable" one.
    #[test]
    fn terminal_quarantine_refused_on_unreadable_state_shows_held_card() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("Torn");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(crate::server::ripper::staging::STATE_FILE),
            b"{ torn",
        )
        .unwrap();
        let path = dir.to_string_lossy().to_string();
        assert!(!persist_terminal_mux_quarantine(&path, &dir, "E6008"));
        let card = MUX_ERRORS.lock().unwrap().remove(&path).expect("card");
        assert!(
            card.reason
                .starts_with(crate::server::ripper::staging::STATE_HELD_PREFIX),
            "{}",
            card.reason
        );
        assert!(!card.hint.contains("unwritable"), "{}", card.hint);
    }

    fn sample_marker() -> RippedMarker {
        RippedMarker {
            schema_version: RIPPED_MARKER_SCHEMA,
            iso_path: "/staging/Border_Town/Border_Town.iso".into(),
            mapfile_path: "/staging/Border_Town/Border_Town.iso.mapfile".into(),
            display_name: "Border Town".into(),
            disc_format: "uhd".into(),
            mkv_filename: "Border_Town.mkv".into(),
            tmdb_title: "Border Town".into(),
            tmdb_year: 2024,
            tmdb_poster: "https://image.tmdb.org/poster.jpg".into(),
            tmdb_overview: "Synopsis".into(),
            tmdb_media_type: "tv".into(),
            max_retries: 5,
            abort_on_lost_secs: 30,
            rip_elapsed_secs: 1234.0,
            rip_errors: 0,
            rip_lost_video_secs: 0.0,
            rip_last_sector: 32_000_000,
            origin_device: "sg0".into(),
            sweep_errors: 0,
            sweep_total_lost_ms: 0.0,
            sweep_main_lost_ms: 0.0,
            sweep_num_bad_ranges: 0,
            sweep_largest_gap_ms: 0.0,
            title_confident: false,
        }
    }

    #[test]
    fn marker_round_trip() {
        let tmp = TempDir::new().unwrap();
        let marker = sample_marker();
        write_marker(tmp.path(), &marker).unwrap();
        let back = read_marker(tmp.path()).unwrap();
        assert_eq!(back.display_name, "Border Town");
        assert_eq!(back.tmdb_year, 2024);
        assert_eq!(back.schema_version, RIPPED_MARKER_SCHEMA);
        // media_type must survive the hand-off: the resume mux path seeds it
        // into STATE and writes it into the `.done`/`.review` marker so the
        // mover routes a resumed TV rip to the TV library, not movies.
        assert_eq!(back.tmdb_media_type, "tv");
        // Every field survives (non-default values so a dropped field shows).
        let full = RippedMarker {
            rip_errors: 7,
            rip_lost_video_secs: 1.5,
            sweep_errors: 3,
            sweep_total_lost_ms: 120.0,
            sweep_main_lost_ms: 80.0,
            sweep_num_bad_ranges: 2,
            sweep_largest_gap_ms: 60.0,
            title_confident: true,
            ..sample_marker()
        };
        write_marker(tmp.path(), &full).unwrap();
        let back = read_marker(tmp.path()).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&full).unwrap()
        );
    }

    // Backward-compat: a pre-rc.4 marker lacks `tmdb_media_type`; it must
    // deserialize (serde default = empty) rather than fail resume, then the
    // resume mux path falls back to "movie" — same as the mover's default.
    #[test]
    fn marker_without_media_type_defaults_empty() {
        let json = r#"{
            "schema_version": 1,
            "iso_path": "/staging/Old/Old.iso",
            "mapfile_path": "/staging/Old/Old.iso.mapfile",
            "display_name": "Old",
            "disc_format": "uhd",
            "mkv_filename": "Old.mkv",
            "tmdb_title": "Old",
            "tmdb_year": 2020,
            "tmdb_poster": "",
            "tmdb_overview": "",
            "max_retries": 3,
            "abort_on_lost_secs": 0,
            "rip_elapsed_secs": 0.0,
            "rip_errors": 0,
            "rip_lost_video_secs": 0.0,
            "rip_last_sector": 0,
            "origin_device": "sg0"
        }"#;
        let marker: RippedMarker = serde_json::from_str(json).unwrap();
        assert_eq!(marker.tmdb_media_type, "");
    }

    #[test]
    fn read_marker_rejects_wrong_schema() {
        // `write_marker` always writes the CURRENT schema, so a bad
        // schema_version can't round-trip through it — exercise the LEGACY
        // fallback instead: a pre-migration `.ripped` file, no `state.json`.
        let tmp = TempDir::new().unwrap();
        let mut marker = sample_marker();
        marker.schema_version = 9999;
        let json = serde_json::to_vec(&marker).unwrap();
        std::fs::write(tmp.path().join(RIPPED_MARKER_NAME), json).unwrap();
        assert!(
            crate::server::ripper::staging::read_state(tmp.path()).is_none(),
            "no state.json must exist for this to exercise the legacy fallback"
        );
        let err = read_marker(tmp.path()).unwrap_err();
        assert!(format!("{err}").contains("schema_version"));
    }

    #[test]
    fn delete_marker_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        delete_marker(tmp.path()).expect("delete on missing path is OK");
        write_marker(tmp.path(), &sample_marker()).unwrap();
        delete_marker(tmp.path()).unwrap();
        assert!(!tmp.path().join(RIPPED_MARKER_NAME).exists());
    }

    #[test]
    fn pending_queue_lists_markers() {
        let tmp = TempDir::new().unwrap();
        let movie = tmp.path().join("Border_Town");
        std::fs::create_dir_all(&movie).unwrap();
        write_marker(&movie, &sample_marker()).unwrap();

        let other = tmp.path().join("No_Marker_Here");
        std::fs::create_dir_all(&other).unwrap();

        let q = pending_queue(tmp.path());
        assert_eq!(q.len(), 1);
        assert!(q[0].contains("Border Town"));
        assert!(q[0].contains("queued"));
    }

    // Regression: the completion guard must consult `snapshot_staging_disc`
    // (a primed, retried `read_dir` view), not a bare `Path::exists()`, so a
    // finished dir reports `completed == true` and isn't re-dispatched.
    #[test]
    fn completion_guard_sees_completed_via_snapshot() {
        let tmp = TempDir::new().unwrap();
        let movie = tmp.path().join("Border_Town");
        std::fs::create_dir_all(&movie).unwrap();
        write_marker(&movie, &sample_marker()).unwrap();
        crate::server::ripper::staging::write_completed_marker(&movie);

        let snap = crate::server::ripper::staging::snapshot_staging_disc(&movie)
            .expect("a populated dir must yield a snapshot");
        assert!(
            snap.completed,
            "snapshot must report completed=true for a dir with .completed; \
             the check_and_mux guard relies on this to avoid re-muxing a finished dir"
        );
    }

    #[test]
    fn pending_queue_skips_completed_dir() {
        // A successful mux can leave `.ripped` alongside `.completed` when
        // delete_marker fails post-mux (NFS); `.completed` is authoritative,
        // so such a dir must not show up as "(queued)" forever.
        let tmp = TempDir::new().unwrap();
        let movie = tmp.path().join("Border_Town");
        std::fs::create_dir_all(&movie).unwrap();
        write_marker(&movie, &sample_marker()).unwrap();
        crate::server::ripper::staging::write_completed_marker(&movie);

        let q = pending_queue(tmp.path());
        assert!(
            q.is_empty(),
            "a dir with .completed present must be skipped, got {q:?}"
        );
    }

    // Regression (re-mux-forever loop): a terminal failure writes `.failed`
    // without `.completed`, and `.ripped` is only deleted on success — a
    // `.ripped`+`.failed` dir must be TERMINAL or the worker loops forever.
    #[test]
    fn completion_guard_sees_failed_via_snapshot() {
        let tmp = TempDir::new().unwrap();
        let movie = tmp.path().join("Border_Town");
        std::fs::create_dir_all(&movie).unwrap();
        write_marker(&movie, &sample_marker()).unwrap();
        crate::server::ripper::staging::write_failed_marker(
            &movie,
            "mux finalize failed (unseekable output)",
        );

        let snap = crate::server::ripper::staging::snapshot_staging_disc(&movie)
            .expect("a populated dir must yield a snapshot");
        assert!(!snap.completed, ".failed dir is not .completed");
        assert!(
            snap.failed_reason.is_some(),
            "snapshot must report failed_reason for a .failed dir; the \
             check_and_mux guard relies on this to avoid the re-mux-forever loop"
        );
    }

    // Regression (bug #3, mutual exclusion): `.done` is written BEFORE the
    // terminal `.completed`, and the Move queue scans for `.done` — so once
    // a job enters the Move queue it must not also report "(queued)" here.
    #[test]
    fn pending_queue_skips_done_dir_mutual_exclusion() {
        let tmp = TempDir::new().unwrap();
        let movie = tmp.path().join("Border_Town");
        std::fs::create_dir_all(&movie).unwrap();
        write_marker(&movie, &sample_marker()).unwrap();
        // The mover hand-off marker is present but `.completed` is NOT yet
        // (the gap between the two durable writes). This dir is in the Move
        // queue; it must be absent from the Mux queue.
        crate::server::ripper::staging::mark_handoff(&movie, true, |_s| {}).unwrap();

        let q = pending_queue(tmp.path());
        assert!(
            q.is_empty(),
            "a dir with .done (in the Move queue) must NOT also be (queued) in the Mux queue, got {q:?}"
        );
    }

    // A `.review` dir (low-confidence hand-off held for the operator) is
    // likewise the mover's concern, not the mux worker's — it must not
    // double-list in the Mux queue.
    #[test]
    fn pending_queue_skips_review_dir() {
        let tmp = TempDir::new().unwrap();
        let movie = tmp.path().join("Border_Town");
        std::fs::create_dir_all(&movie).unwrap();
        write_marker(&movie, &sample_marker()).unwrap();
        crate::server::ripper::staging::mark_handoff(&movie, false, |_s| {}).unwrap();

        let q = pending_queue(tmp.path());
        assert!(q.is_empty(), "a .review dir must be skipped, got {q:?}");
    }

    // The dir currently being muxed carries `.muxing` and is surfaced as
    // the live in-flight mux via the synthetic `_mux` device — it must not
    // also appear as "(queued)" in the static pending list.
    #[test]
    fn pending_queue_skips_muxing_dir() {
        let tmp = TempDir::new().unwrap();
        let movie = tmp.path().join("Border_Town");
        std::fs::create_dir_all(&movie).unwrap();
        write_marker(&movie, &sample_marker()).unwrap();
        crate::server::ripper::staging::write_muxing_marker(&movie);

        let q = pending_queue(tmp.path());
        assert!(
            q.is_empty(),
            "a dir actively muxing (.muxing) must not also be (queued), got {q:?}"
        );
    }

    // Regression (bug #1): after hand-off the REAL device is already "done";
    // the post-mux revert only fires for a device still "ripping", so the
    // synthetic `_mux` worker can never push "done" back to "ripping".
    #[test]
    fn mux_worker_does_not_revert_done_origin_device() {
        let device = "sg_test_origin_already_done";
        // Hand-off set the real device straight to "done" (the new contract).
        crate::server::ripper::update_state(
            device,
            crate::server::ripper::RipState {
                device: device.to_string(),
                status: "done".to_string(),
                progress_pct: 100,
                disc_name: "Border Town".to_string(),
                disc_format: "uhd".to_string(),
                ..Default::default()
            },
        );
        let tile = crate::server::ripper::STATE
            .lock()
            .unwrap()
            .get(device)
            .map(|rs| (rs.status.clone(), rs.disc_name.clone()));
        let tile = tile.as_ref().map(|(s, n)| (s.as_str(), n.as_str()));
        assert!(
            !should_revert_origin_to_done(device, tile, "Border Town"),
            "a device already 'done' at hand-off must not be reverted by the mux worker"
        );
        // Cleanup so the synthetic entry doesn't leak into other tests.
        crate::server::ripper::STATE.lock().unwrap().remove(device);
    }

    // Companion (bug #1, other half): on the INLINE-MUX FALLBACK path (marker
    // write failed, tile left "ripping") the revert IS needed and must still
    // fire — the fix must not over-correct into never reverting.
    #[test]
    fn mux_worker_reverts_ripping_origin_on_inline_fallback() {
        let device = "sg_test_origin_still_ripping";
        crate::server::ripper::update_state(
            device,
            crate::server::ripper::RipState {
                device: device.to_string(),
                status: "ripping".to_string(),
                disc_name: "Border Town".to_string(),
                disc_format: "uhd".to_string(),
                ..Default::default()
            },
        );
        let tile = crate::server::ripper::STATE
            .lock()
            .unwrap()
            .get(device)
            .map(|rs| (rs.status.clone(), rs.disc_name.clone()));
        let tile = tile.as_ref().map(|(s, n)| (s.as_str(), n.as_str()));
        assert!(
            should_revert_origin_to_done(device, tile, "Border Town"),
            "a still-'ripping' origin device (inline-mux fallback) MUST be reverted to done"
        );
        crate::server::ripper::STATE.lock().unwrap().remove(device);
    }

    // The revert predicate edge cases: empty origin, synthetic origin, and a
    // vanished/absent device entry are all no-ops; only a real, still-ripping
    // device reverts.
    #[test]
    fn revert_origin_predicate_edge_cases() {
        assert!(
            !should_revert_origin_to_done("", Some(("ripping", "A")), "A"),
            "empty origin must not revert"
        );
        assert!(
            !should_revert_origin_to_done("_mux", Some(("ripping", "A")), "A"),
            "a synthetic origin must not revert"
        );
        assert!(
            !should_revert_origin_to_done("sg0", None, "A"),
            "a vanished device entry (None status) must not revert"
        );
        assert!(
            !should_revert_origin_to_done("sg0", Some(("done", "A")), "A"),
            "an already-done device must not revert"
        );
        assert!(
            should_revert_origin_to_done("sg0", Some(("ripping", "A")), "A"),
            "a real, still-ripping device must revert"
        );
        assert!(
            !should_revert_origin_to_done("sg0", Some(("ripping", "B")), "A"),
            "a device now ripping a different disc must not get this disc's done tile"
        );
    }

    #[test]
    fn pending_queue_skips_failed_dir() {
        let tmp = TempDir::new().unwrap();
        let movie = tmp.path().join("Border_Town");
        std::fs::create_dir_all(&movie).unwrap();
        write_marker(&movie, &sample_marker()).unwrap();
        crate::server::ripper::staging::write_failed_marker(
            &movie,
            "mux finalize failed (unseekable output)",
        );

        let q = pending_queue(tmp.path());
        assert!(
            q.is_empty(),
            "a dir with .failed present is terminal and must be skipped, got {q:?}"
        );
    }

    // Regression: after a worker mux success the origin tile must reach "done" carrying the
    // mux outcome's COMBINED loss/display fields, not the marker's sweep-only figures.
    #[test]
    fn origin_device_reaches_done_after_mux_success() {
        let device = "_test_origin_mux_done";
        let mut marker = sample_marker();
        marker.sweep_errors = 1;
        marker.sweep_total_lost_ms = 10.0;
        marker.sweep_num_bad_ranges = 3;
        marker.sweep_largest_gap_ms = 7.0;
        let outcome = crate::server::ripper::resume::MuxHandoffOutcome {
            success: true,
            errors: 42,
            total_lost_ms: 3500.0,
            main_lost_ms: 2000.0,
            lost_video_secs: 3.5,
            codecs: "HEVC / TrueHD".into(),
            duration: "2h 01m".into(),
            output_file: "Border Town.mkv".into(),
            ..Default::default()
        };

        let rs = origin_done_state(device, &marker, &outcome);
        crate::server::ripper::update_state(device, rs);
        let got = crate::server::ripper::STATE.lock().unwrap().remove(device);

        let rs = got.expect("device state must exist");
        assert_eq!(
            rs.status, "done",
            "origin device must be 'done' after mux success"
        );
        assert_eq!(rs.device, device);
        assert_eq!(rs.progress_pct, 100, "progress must be 100 on done");
        assert!(rs.disc_present);
        assert_eq!(rs.disc_name, marker.display_name);
        assert_eq!(rs.tmdb_title, marker.tmdb_title);
        assert_eq!(
            rs.errors, 42,
            "errors must be the combined mux outcome, not the sweep's"
        );
        assert_eq!(rs.total_lost_ms, 3500.0);
        assert_eq!(rs.main_lost_ms, 2000.0);
        assert_eq!(rs.lost_video_secs, 3.5);
        assert_eq!(rs.num_bad_ranges, 3, "range count comes from the marker");
        assert_eq!(rs.largest_gap_ms, 7.0);
        assert_eq!(rs.codecs, "HEVC / TrueHD");
        assert_eq!(rs.duration, "2h 01m");
        assert_eq!(rs.output_file, "Border Town.mkv");
    }

    // Regression: done-card damage telemetry must not be zeroed. The worker's done card
    // (`origin_done_state`) for a damaged rip must carry its loss through update_state,
    // which derives damage_severity from it.
    #[test]
    fn done_card_carries_sweep_damage_telemetry() {
        let device = "_test_done_damage_telemetry";
        let mut marker = sample_marker();
        marker.sweep_num_bad_ranges = 3;
        marker.sweep_largest_gap_ms = 1200.0;
        let outcome = crate::server::ripper::resume::MuxHandoffOutcome {
            success: true,
            errors: 42,
            total_lost_ms: 3500.0,
            main_lost_ms: 2000.0,
            ..Default::default()
        };

        crate::server::ripper::update_state(device, origin_done_state(device, &marker, &outcome));
        let rs = crate::server::ripper::STATE
            .lock()
            .unwrap()
            .remove(device)
            .expect("device state must exist");
        assert_eq!(rs.status, "done");
        assert_eq!(rs.errors, 42, "errors must carry through to done state");
        assert!(
            rs.total_lost_ms > 0.0,
            "total_lost_ms must be non-zero on damaged done card"
        );
        assert_eq!(rs.num_bad_ranges, 3);
        assert!(
            !rs.damage_severity.is_empty(),
            "damage_severity must be set for a damaged done card (got empty — update_state must derive it from errors/total_lost_ms)"
        );
    }

    // EXHAUSTIVE mux-worker dispatch matrix (rc4 hardening): drives the real
    // `snapshot_staging_disc` + `mux_dispatch_verdict` pair against a TempDir
    // for every marker combo, closing the gap that let da16f00 ship untested.

    /// The staging-side constant must equal the muxer's own marker name,
    /// or `snapshot_staging_disc` would observe `.ripped` under a different
    /// name than the worker writes and `has_ripped` would never be set.
    #[test]
    fn ripped_marker_name_matches_staging_constant() {
        assert_eq!(
            RIPPED_MARKER_NAME,
            crate::server::ripper::staging::RIPPED_MARKER,
            "the muxer's .ripped marker name and the staging-scan constant must agree"
        );
    }

    /// Marker tokens a dispatch-matrix row can place in a staging dir.
    #[derive(Clone, Copy)]
    enum M {
        Ripped,
        Completed,
        Failed,
        /// A non-JSON `.failed` body (review.rs operator-cancel). Pins that the
        /// dispatch verdict keys on marker PRESENCE (`has_failed`), not a
        /// parseable `failed_reason` (M2).
        FailedNonJson,
        Done,
        Review,
        Iso,
        Mapfile,
        Mkv,
        /// A `.aborted-loss` marker (mux completed but delivered loss exceeded
        /// the threshold). Deterministic media damage — the worker must STOP
        /// re-dispatching, not re-mux the ISO forever.
        AbortedLoss,
    }

    /// Build a populated per-disc staging dir for the given markers, run the
    /// real snapshot+verdict pair, and return the verdict.
    fn verdict_for(markers: &[M]) -> MuxVerdict {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("Disc");
        std::fs::create_dir_all(&dir).unwrap();
        for m in markers {
            match m {
                M::Ripped => {
                    // A real, schema-valid .ripped marker (so this dir is
                    // indistinguishable from a true hand-off).
                    write_marker(&dir, &sample_marker()).unwrap();
                }
                M::Completed => crate::server::ripper::staging::write_completed_marker(&dir),
                M::Failed => {
                    let _ =
                        crate::server::ripper::staging::write_failed_marker(&dir, "test failure");
                }
                M::FailedNonJson => {
                    // Legacy review.rs wrote a non-JSON `.failed` whose reason
                    // didn't parse; reproduce that by leaving `failure_reason`
                    // unset rather than writing a raw file `snapshot` would ignore.
                    crate::server::ripper::staging::mutate_state(
                        &dir,
                        crate::server::ripper::staging::StagingState::Failed,
                        |s| {
                            s.state = crate::server::ripper::staging::StagingState::Failed;
                            s.failure_reason = None;
                            s.muxing = false;
                        },
                    )
                    .unwrap();
                }
                M::Done => std::fs::write(dir.join(".done"), b"{}").unwrap(),
                M::Review => std::fs::write(dir.join(".review"), b"{}").unwrap(),
                M::Iso => std::fs::write(dir.join("Disc.iso"), b"x").unwrap(),
                M::Mapfile => std::fs::write(dir.join("Disc.iso.mapfile"), b"x").unwrap(),
                M::Mkv => std::fs::write(dir.join("Disc.mkv"), b"x").unwrap(),
                M::AbortedLoss => {
                    crate::server::ripper::staging::write_aborted_loss_marker(
                        &dir,
                        "aborted: 0.44s lost at mux, decrypt/codec (threshold 0s)",
                        1,
                    );
                }
            }
        }
        let snap = crate::server::ripper::staging::snapshot_staging_disc(&dir);
        mux_dispatch_verdict(snap.as_ref())
    }

    #[test]
    fn mux_dispatch_matrix() {
        use M::*;
        // (markers present, expected verdict, why)
        let table: &[(&[M], MuxVerdict, &str)] = &[
            // --- nothing / no hand-off marker -> no-op ---
            (&[], MuxVerdict::SkipNoMarker, "empty dir: nothing to mux"),
            (
                &[Iso],
                MuxVerdict::SkipNoMarker,
                "ISO but no .ripped: not the worker's job",
            ),
            (
                &[Iso, Mapfile],
                MuxVerdict::SkipNoMarker,
                "ISO+mapfile, no hand-off marker",
            ),
            (&[Mkv], MuxVerdict::SkipNoMarker, "stray MKV, no .ripped"),
            // --- the canonical dispatch case ---
            (
                &[Ripped],
                MuxVerdict::Dispatch,
                ".ripped only: the hand-off to mux",
            ),
            (
                &[Ripped, Iso, Mapfile],
                MuxVerdict::Dispatch,
                ".ripped + ISO + mapfile: normal hand-off",
            ),
            // --- terminal: .completed wins over everything ---
            (
                &[Ripped, Completed],
                MuxVerdict::SkipTerminal,
                ".ripped lingered after a successful mux (delete failed) — .completed is terminal, must NOT re-mux",
            ),
            (
                &[Completed],
                MuxVerdict::SkipTerminal,
                ".completed alone: finished",
            ),
            (
                &[Completed, Mkv],
                MuxVerdict::SkipTerminal,
                "finished with output present",
            ),
            // --- terminal: .failed is terminal too (the da16f00 fix) ---
            (
                &[Ripped, Failed],
                MuxVerdict::SkipTerminal,
                "THE BUG: a terminal mux failure wrote .failed but .ripped lingered — must be terminal, not re-dispatched forever",
            ),
            (
                &[Failed],
                MuxVerdict::SkipTerminal,
                ".failed alone: quarantined",
            ),
            (
                &[Ripped, Iso, Mapfile, Failed],
                MuxVerdict::SkipTerminal,
                "aborted hand-off with artifacts still present — terminal",
            ),
            // --- M2: a non-JSON `.failed` body is still terminal. The verdict
            //     keys on `has_failed` (presence), not `failed_reason`. A
            //     .ripped + non-JSON .failed must NOT re-dispatch forever. ---
            (
                &[FailedNonJson],
                MuxVerdict::SkipTerminal,
                "non-JSON .failed alone: terminal by presence",
            ),
            (
                &[Ripped, FailedNonJson],
                MuxVerdict::SkipTerminal,
                "M2: .ripped + non-JSON .failed (no parseable reason) — terminal, never re-dispatch",
            ),
            // --- conflict: .completed + .failed both present ---
            (
                &[Completed, Failed],
                MuxVerdict::SkipTerminal,
                "conflicting terminals: still terminal either way (skip)",
            ),
            (
                &[Ripped, Completed, Failed],
                MuxVerdict::SkipTerminal,
                ".ripped + both terminals: terminal, never re-mux",
            ),
            // --- .done / .review are NOT terminal for the mux worker: they're
            // the MOVER's hand-off, written alongside .completed. Without
            // .completed a lone .ripped+.done still Dispatches — documented here.
            (
                &[Ripped, Done],
                MuxVerdict::Dispatch,
                ".done without .completed does not gate the mux worker (.completed is the authoritative signal)",
            ),
            (
                &[Ripped, Review],
                MuxVerdict::Dispatch,
                ".review without .completed likewise does not gate the mux worker",
            ),
            // --- .aborted-loss STOPS the re-mux loop (the 60GB/tick bug) ---
            (
                &[Ripped, Iso, Mapfile, AbortedLoss],
                MuxVerdict::SkipAbortedLoss,
                ".ripped lingers but the mux already loss-aborted: must NOT re-mux (deterministic loss)",
            ),
            (
                &[AbortedLoss],
                MuxVerdict::SkipAbortedLoss,
                ".aborted-loss alone: resumable, operator-resolved — never auto-dispatch",
            ),
            (
                // `state.json` holds one `state` field, so the LAST writer
                // wins — order markers so `.completed` lands last, matching
                // "a finished dir stays terminal even after an aborted-loss".
                &[Ripped, AbortedLoss, Completed],
                MuxVerdict::SkipTerminal,
                ".completed wins over .aborted-loss: a finished dir stays terminal",
            ),
            (
                // Same reordering rationale: `.failed` must be the last write
                // so the resulting `state.json` ends in `Failed`.
                &[Ripped, AbortedLoss, Failed],
                MuxVerdict::SkipTerminal,
                ".failed wins over .aborted-loss: a quarantined dir stays terminal",
            ),
        ];
        for (markers, expected, why) in table {
            let got = verdict_for(markers);
            assert_eq!(got, *expected, "dispatch matrix row failed: {why}");
        }
    }

    /// UNKNOWN listing (snapshot None) must skip — never dispatch on a
    /// degraded read_dir view. Driven directly since a real per-entry NFS
    /// error can't be provoked from the local FS.
    #[test]
    fn mux_dispatch_unknown_snapshot_skips() {
        assert_eq!(mux_dispatch_verdict(None), MuxVerdict::SkipUnknown);
    }

    /// Named explicit cells the matrix also covers, called out per the rc4
    /// brief so a future reader sees them by name.
    #[test]
    fn mux_dispatch_ripped_only_dispatches() {
        assert_eq!(verdict_for(&[M::Ripped]), MuxVerdict::Dispatch);
    }
    #[test]
    fn mux_dispatch_ripped_plus_completed_skips() {
        assert_eq!(
            verdict_for(&[M::Ripped, M::Completed]),
            MuxVerdict::SkipTerminal
        );
    }
    #[test]
    fn mux_dispatch_ripped_plus_failed_skips_the_fixed_bug() {
        // The exact cell the infinite-loop bug lived in. Pin it hard.
        assert_eq!(
            verdict_for(&[M::Ripped, M::Failed]),
            MuxVerdict::SkipTerminal,
            ".ripped + .failed MUST be terminal (da16f00) — re-dispatch here is the loop bug"
        );
    }
    #[test]
    fn mux_dispatch_nothing_present_is_noop() {
        assert_eq!(verdict_for(&[]), MuxVerdict::SkipNoMarker);
    }

    // Quarantine decision: ONLY a structural finalize failure (`is_finalize`)
    // is terminal → `state → Failed`. This predicate stops the re-mux-forever
    // loop; the non-terminal rows below MUST stay resumable.
    #[test]
    fn mux_failure_is_terminal_truth_table() {
        assert!(
            mux_failure_is_terminal(MuxFailureClass {
                aborted_loss: false,
                has_worker_reason: true,
                is_finalize: true,
            }),
            "a structural finalize failure (E6008) MUST quarantine — this was the loop bug"
        );
        // The load-bearing row: a resumable, NON-finalize failure (a
        // mid-mux read error, fsync failure, mapfile TOCTOU) isn't a keyless
        // deferral, so the old `!failure_retryable` gate wrongly quarantined it.
        assert!(
            !mux_failure_is_terminal(MuxFailureClass {
                aborted_loss: false,
                has_worker_reason: true,
                is_finalize: false,
            }),
            "a resumable non-finalize failure (read error) must NOT be quarantined — the FIX-2 false-terminal"
        );
        assert!(
            !mux_failure_is_terminal(MuxFailureClass {
                aborted_loss: true,
                has_worker_reason: true,
                is_finalize: true,
            }),
            "an aborted-loss owns its own resumable state — never quarantine here"
        );
        assert!(
            !mux_failure_is_terminal(MuxFailureClass {
                aborted_loss: false,
                has_worker_reason: false,
                is_finalize: true,
            }),
            "no worker reason: defensive precondition — don't quarantine"
        );
    }

    // FIX 2 regression: the quarantine gate keys on `failure_finalize`, not the old
    // `!failure_retryable` (false for both resumable and terminal cases, which
    // false-quarantined resumable reads).
    #[test]
    fn resumable_worker_failure_not_quarantined_finalize_is() {
        use crate::server::ripper::resume::MuxHandoffOutcome;
        // A mid-mux read error: worker reason present, retryable=false (NOT a
        // keyless deferral), finalize=false. Resumable — must stay re-muxable.
        let read_error = MuxHandoffOutcome {
            success: false,
            failure_reason: Some("rip stopped: read error — drive I/O".to_string()),
            failure_retryable: false,
            failure_finalize: false,
            ..Default::default()
        };
        assert!(
            !mux_failure_is_terminal(MuxFailureClass {
                aborted_loss: false,
                has_worker_reason: read_error.failure_reason.is_some(),
                is_finalize: read_error.failure_finalize,
            }),
            "a resumable mid-mux read error must NOT be quarantined by the worker"
        );
        // A structural finalize failure (E6008): terminal — must quarantine.
        let finalize_error = MuxHandoffOutcome {
            success: false,
            failure_reason: Some("mux finalize failed: E6008".to_string()),
            failure_retryable: false,
            failure_finalize: true,
            ..Default::default()
        };
        assert!(
            mux_failure_is_terminal(MuxFailureClass {
                aborted_loss: false,
                has_worker_reason: finalize_error.failure_reason.is_some(),
                is_finalize: finalize_error.failure_finalize,
            }),
            "a structural finalize failure MUST be quarantined by the worker"
        );
    }

    // End-to-end: a Ripped dir dispatches; once the failure branch writes the
    // terminal state on a non-retryable finalize error, the verdict flips to
    // SkipTerminal so the dir is never re-dispatched (the E6008 loop, fixed).
    #[test]
    fn terminal_finalize_quarantine_stops_redispatch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("Decoy_Feature_3");
        std::fs::create_dir_all(&dir).unwrap();
        write_marker(&dir, &sample_marker()).unwrap();
        std::fs::write(dir.join("Decoy_Feature_3.iso"), b"x").unwrap();

        let s1 = crate::server::ripper::staging::snapshot_staging_disc(&dir);
        assert_eq!(
            mux_dispatch_verdict(s1.as_ref()),
            MuxVerdict::Dispatch,
            "a fresh Ripped hand-off must dispatch"
        );

        // Exactly the transition the failure branch performs when
        // `mux_failure_is_terminal` returns true.
        crate::server::ripper::staging::write_failed_marker(
            &dir,
            "mux produced no frames (empty/undecryptable output)",
        );

        let s2 = crate::server::ripper::staging::snapshot_staging_disc(&dir);
        assert_eq!(
            mux_dispatch_verdict(s2.as_ref()),
            MuxVerdict::SkipTerminal,
            "after the terminal-finalize quarantine the dir must never re-dispatch"
        );
    }

    // FIX (entry-side TOCTOU): `.muxing` must be stamped the INSTANT Dispatch commits, before
    // `read_marker`, so a concurrent web entry's `muxing_status` guard covers the whole dispatch.
    #[test]
    fn muxing_marker_stamped_before_marker_read() {
        let src = crate::server::util::source_lf(include_str!("muxer.rs"));
        let start = src
            .find("let verdict = mux_dispatch_verdict(snap.as_ref());")
            .expect("muxer.rs should dispatch on the verdict");
        let end = src[start..]
            .find("let outcome = crate::server::ripper::resume::remux_from_ripped_marker")
            .map(|i| start + i)
            .expect("muxer.rs should dispatch to remux_from_ripped_marker");
        let region = &src[start..end];
        // The stamp is live code in the Dispatch arm, not another arm or a comment.
        let arm = &region[region
            .find("MuxVerdict::Dispatch => {")
            .expect("the worker must match the Dispatch verdict")..];
        let arm = &arm[..arm[1..].find("MuxVerdict::").map_or(arm.len(), |i| i + 1)];
        assert!(
            arm.lines()
                .any(|l| l.trim() == "crate::server::ripper::staging::write_muxing_marker(&dir);"),
            "the Dispatch arm must stamp the .muxing lock"
        );
        let stamp = region
            .find("write_muxing_marker(&dir);")
            .expect("the worker must stamp the .muxing lock");
        let read = region
            .find("read_marker(&dir)")
            .expect("the worker must read the .ripped marker");
        assert!(
            stamp < read,
            "the .muxing lock must be stamped at verdict-commit, BEFORE read_marker — \
             else a concurrent entry sees is_muxing==false and races the muxer"
        );
    }

    // the terminal-quarantine site consumes `write_failed_marker`'s
    // return — a failed write must raise a LOUD operator card instead of
    // silently re-dispatching forever.
    #[test]
    fn persist_terminal_mux_quarantine_alarms_when_write_fails() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Happy path: a writable dir goes terminal, returns true, raises no card.
        let ok_tmp = TempDir::new().unwrap();
        let ok_dir = ok_tmp.path().join("Writable");
        std::fs::create_dir_all(&ok_dir).unwrap();
        crate::server::ripper::staging::write_state(
            &ok_dir,
            &crate::server::ripper::staging::DiscState::new(
                crate::server::ripper::staging::StagingState::Ripped,
            ),
        );
        let ok_path = ok_dir.to_string_lossy().to_string();
        clear_error(&ok_path);
        assert!(
            persist_terminal_mux_quarantine(&ok_path, &ok_dir, "E6008 no muxable frames"),
            "a writable staging dir must persist the terminal quarantine"
        );
        assert_eq!(
            crate::server::ripper::staging::read_state(&ok_dir).map(|s| s.state),
            Some(crate::server::ripper::staging::StagingState::Failed),
            "the terminal write must land → state Failed"
        );
        assert!(
            !MUX_ERRORS.lock().unwrap().contains_key(&ok_path),
            "a landed quarantine must NOT raise an operator card"
        );

        // Failure path: force `write_failed_marker` to fail by making state.json a
        // directory (the tmp→final rename can't clobber a dir). The site must raise
        // a loud operator card and report the write did NOT land.
        let bad_tmp = TempDir::new().unwrap();
        let bad_dir = bad_tmp.path().join("Unwritable");
        std::fs::create_dir_all(&bad_dir).unwrap();
        std::fs::create_dir_all(bad_dir.join(crate::server::ripper::staging::STATE_FILE)).unwrap();
        let bad_path = bad_dir.to_string_lossy().to_string();
        clear_error(&bad_path);
        assert!(
            !persist_terminal_mux_quarantine(&bad_path, &bad_dir, "E6008 no muxable frames"),
            "a failed terminal write must report it did NOT land"
        );
        assert!(
            MUX_ERRORS.lock().unwrap().contains_key(&bad_path),
            "a dropped terminal write must raise a LOUD operator card (not silently re-dispatch)"
        );
        clear_error(&bad_path);
    }

    // A terminal failure whose quarantine write didn't land keeps the quarantine card (the dir
    // will keep re-dispatching), not the generic finalize hint.
    #[test]
    fn unpersisted_terminal_quarantine_keeps_its_card() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("Unwritable");
        std::fs::create_dir_all(dir.join(crate::server::ripper::staging::STATE_FILE)).unwrap();
        let path = dir.to_string_lossy().to_string();
        clear_error(&path);
        assert!(
            !record_mux_failure(
                &path,
                &dir,
                "E6008 no muxable frames",
                "generic",
                true,
                true
            ),
            "an unpersisted quarantine is not a quarantine (the dir keeps its keys)"
        );
        let hint = MUX_ERRORS
            .lock()
            .unwrap()
            .get(&path)
            .map(|e| e.hint.clone());
        clear_error(&path);
        let hint = hint.expect("a card must be raised");
        assert_ne!(hint, "generic", "the quarantine card was overwritten");

        // A landed quarantine reports itself (its rip keys are then released).
        let ok = tmp.path().join("Writable");
        std::fs::create_dir_all(&ok).unwrap();
        crate::server::ripper::staging::write_state(
            &ok,
            &crate::server::ripper::staging::DiscState::new(
                crate::server::ripper::staging::StagingState::Ripped,
            ),
        );
        let ok_path = ok.to_string_lossy().to_string();
        assert!(record_mux_failure(
            &ok_path, &ok, "E6008", "generic", true, true
        ));
        assert!(!record_mux_failure(
            &ok_path,
            &ok,
            "read error",
            "generic",
            false,
            true
        ));
        clear_error(&ok_path);
    }

    // TRANSITION: ripped → completed. After `.completed` is written, a
    // lingering `.ripped` (delete failed) must still flip the verdict to
    // SkipTerminal, so the worker doesn't wipe the MKV and re-mux.
    #[test]
    fn mux_transition_ripped_to_completed_stops_dispatch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("Disc");
        std::fs::create_dir_all(&dir).unwrap();
        write_marker(&dir, &sample_marker()).unwrap();
        std::fs::write(dir.join("Disc.iso"), b"x").unwrap();

        // State 1: fresh hand-off → Dispatch.
        let s1 = crate::server::ripper::staging::snapshot_staging_disc(&dir);
        assert_eq!(mux_dispatch_verdict(s1.as_ref()), MuxVerdict::Dispatch);

        // Mux succeeds, writes .completed, but the .ripped delete fails
        // (simulated by leaving .ripped in place).
        crate::server::ripper::staging::write_completed_marker(&dir);

        // State 2: terminal → SkipTerminal (loop broken).
        let s2 = crate::server::ripper::staging::snapshot_staging_disc(&dir);
        assert_eq!(
            mux_dispatch_verdict(s2.as_ref()),
            MuxVerdict::SkipTerminal,
            "after .completed the worker must stop dispatching even if .ripped lingers"
        );
    }

    // TRANSITION: ripped → failed → not re-dispatched. Mirrors a terminal
    // mux failure branch (writes `.failed`); even if `.ripped` survives, the
    // verdict must stay terminal — the re-mux-forever loop is impossible.
    #[test]
    fn mux_transition_ripped_to_failed_stops_dispatch() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("Disc");
        std::fs::create_dir_all(&dir).unwrap();
        write_marker(&dir, &sample_marker()).unwrap();

        let s1 = crate::server::ripper::staging::snapshot_staging_disc(&dir);
        assert_eq!(mux_dispatch_verdict(s1.as_ref()), MuxVerdict::Dispatch);

        // A terminal mux failure quarantines the dir.
        crate::server::ripper::staging::write_failed_marker(
            &dir,
            "mux finalize failed (unseekable output)",
        );

        let s2 = crate::server::ripper::staging::snapshot_staging_disc(&dir);
        assert_eq!(
            mux_dispatch_verdict(s2.as_ref()),
            MuxVerdict::SkipTerminal,
            "after .failed the worker must never re-dispatch (the re-mux-forever loop)"
        );
    }

    // Catches the mutation that restores `pending_queue`'s silent `Err(_) => return
    // Vec::new()`: an unreadable staging root is not an empty queue and must be logged.
    #[test]
    fn pending_queue_logs_an_unreadable_staging_root() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Buf::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(buf.clone())
                .with_ansi(false),
        );

        // A path that cannot be listed. Under the workspace's gitignored
        // scratch root, per the project's never-/tmp convention.
        let missing = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!("autorip-no-such-staging-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);

        let queue = tracing::subscriber::with_default(subscriber, || pending_queue(&missing));

        assert!(
            queue.is_empty(),
            "an unreadable root still yields no jobs — the fix is the report, \
             not the return value"
        );
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("could not list staging root for the mux queue"),
            "an unreadable staging root must be reported, not rendered as an \
             empty queue; captured logs were:\n{out}"
        );
    }

    // Catches the mutation that restores `pending_queue`'s `.filter_map(|e| e.ok())`: a
    // source-pin since an NFS ESTALE on one entry can't be synthesised locally.
    #[test]
    fn pending_queue_does_not_flatten_away_a_per_entry_error() {
        let src = crate::server::util::source_lf(include_str!("muxer.rs"));
        let start = src
            .find("\npub fn pending_queue(staging_dir: &Path) -> Vec<String> {")
            .expect("muxer.rs must define pending_queue");
        let end = start
            + src[start..]
                .find("\n    out\n}")
                .expect("pending_queue still returns `out` at its end");
        // Comment lines are stripped: `pending_queue`'s own comment quotes the
        // defective shape verbatim, so a naive substring search would match
        // the explanation of the fix rather than the code.
        let body: String = src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.contains(".filter_map(|e| e.ok())"),
            "pending_queue must not discard a per-entry read_dir error: one \
             ESTALE silently drops a whole disc subdir from the mux queue"
        );
        assert!(
            body.contains("per-entry error listing staging root for the mux queue"),
            "the per-entry failure must be reported — an absent log is the bug"
        );
    }
}
