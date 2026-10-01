//! Resume incomplete muxing from a staged ISO.
//!
//! Companion to `staging::resume_or_quarantine_staging`: that pass
//! classifies staging-dir state after a restart; this module decides
//! what to do with it, remuxing straight from the ISO when Pass 1
//! finished but mux never wrote the final MKV.
//!
//! `classify_resume` is a pure classifier; `resume_remux` is the actor that performs the side
//! effects.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64};
use std::sync::{Arc, RwLock};

use crate::server::config::Config;

use super::staging::{self, ResumeAction, StagingResumeHint};

// No live drive at resume time (we mux from a staged ISO), so this probes a non-optical,
// non-existent node on purpose.
const DEFAULT_BATCH_PROBE_PATH: &str = "/dev/null";

/// Classification of a `ResumePreserved` staging hint. Anything that
/// isn't `ResumePreserved` is mapped here too so the orchestrator can
/// fan out a single `match`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum ResumeClass {
    /// Auto-resume candidate: ISO + mapfile both on disk, mapfile is
    /// `bytes_pending == 0` and the bad bytes that overlap the muxable
    /// title fit inside `abort_on_lost_secs`. Carries the resolved
    /// paths so the actor doesn't have to re-walk the directory.
    Remux {
        iso_path: PathBuf,
        mapfile_path: PathBuf,
        /// Sanitized display name — a FILE basename (the staged ISO's
        /// `file_stem()`), NOT the staging subdirectory's name, which
        /// carries the `_2`-style boxset disc suffix that files inside it
        /// never take. Used for the MKV filename, dest URL, and the
        /// `display_name` in `MuxInputs`. The original TMDB-resolved
        /// title isn't available at resume time (no fresh scan_disc
        /// has run yet); the sanitized form is what every other
        /// downstream path keys on anyway.
        display_name: String,
        /// Operator-confidence carried from the fresh-rip hand-off, when
        /// known. `Some(true)` means the rip side already decided the title
        /// is auto-file-worthy (exact match OR an explicit operator
        /// override); `resume_remux` ORs it into its own match check so an
        /// override whose chosen title differs from the disc's own label
        /// isn't second-guessed into `.review`. `None` on the cold
        /// auto-resume path (no hand-off marker, no override concept) —
        /// `resume_remux` then relies on the match check alone.
        title_confident: Option<bool>,
    },
    /// Hint is `ResumePreserved` but doesn't satisfy the auto-resume
    /// criteria. The orchestrator should fall through to the regular
    /// disc-insertion flow (which may itself reuse the partial state
    /// via libfreemkv's sweep_opts.resume on the next Pass 1).
    NotEligible,
    /// Hint was `AlreadyCompleted` — nothing to do. The mover (if
    /// configured) will pick the staged output up when its state.json is in
    /// `StagingState::Done` (a legacy `.done` file is the fallback).
    AlreadyCompleted,
    /// Hint was `AlreadyFailed` / `RestartLoopFailed` — leave it for
    /// the operator. `reason` is forwarded for surfacing in the UI.
    AlreadyFailed { reason: String },
}

/// Pure classifier. No I/O beyond reading the mapfile (which the
/// orchestrator was going to do at mux time anyway). Returns a
/// verdict that fully describes what should happen next.
///
/// `Remux` requires: hint is `ResumePreserved`/`ResumeAbortedLoss` with `has_iso &&
/// has_mapfile`, mapfile loads with `bytes_pending == 0`, and any bad bytes overlapping the
/// muxable title fit within `abort_on_lost_secs`.
pub fn classify_resume(hint: &StagingResumeHint, _abort_on_lost_secs: u64) -> ResumeClass {
    match &hint.action {
        ResumeAction::AlreadyCompleted => return ResumeClass::AlreadyCompleted,
        ResumeAction::AlreadyFailed { reason } => {
            return ResumeClass::AlreadyFailed {
                reason: reason.clone(),
            };
        }
        ResumeAction::RestartLoopFailed { reason }
        | ResumeAction::HeldUnreadableState { reason } => {
            return ResumeClass::AlreadyFailed {
                reason: reason.clone(),
            };
        }
        // Dir is actively owned/in progress (`.sweeping` sweep running, or
        // `.muxing` mux worker holds it) — the live worker owns the
        // transition; treat as NotEligible and leave it alone.
        ResumeAction::InProgress => return ResumeClass::NotEligible,
        // Both ResumePreserved and ResumeAbortedLoss carry an intact ISO + mapfile
        // and must be re-checked for `Remux` eligibility against the current loss
        // threshold (a repeated over-threshold result eventually goes `.failed`).
        ResumeAction::ResumePreserved { .. } | ResumeAction::ResumeAbortedLoss { .. } => {}
    }
    let (has_iso, has_mapfile) = match &hint.action {
        ResumeAction::ResumePreserved {
            has_iso,
            has_mapfile,
            ..
        }
        | ResumeAction::ResumeAbortedLoss {
            has_iso,
            has_mapfile,
            ..
        } => (*has_iso, *has_mapfile),
        _ => return ResumeClass::NotEligible,
    };
    if !has_iso || !has_mapfile {
        return ResumeClass::NotEligible;
    }

    // Resolve the ISO + mapfile filenames by walking the dir. The
    // staging-snapshot booleans tell us they exist but not their
    // restarts rather than reconstructing the expected names).
    let (iso_path, mapfile_path) = match find_iso_and_mapfile(&hint.dir) {
        Some(p) => p,
        None => return ResumeClass::NotEligible,
    };

    // Mapfile load. A corrupt mapfile means the post-Pass-1 state is
    // ambiguous — fall back to a full re-rip.
    let map = match freemkv_engine::Mapfile::load(&mapfile_path) {
        Ok(m) => m,
        Err(e) => {
            // Don't swallow this: a corrupt/unreadable mapfile silently
            // demotes resume to a full re-rip, which looks like the
            // resume logic "just didn't fire". Make it observable.
            tracing::warn!(
                mapfile = %mapfile_path.display(),
                error = %e,
                "resume: mapfile load failed; classifying as not-eligible (full re-rip)"
            );
            return ResumeClass::NotEligible;
        }
    };
    // No Volume ID is read from the mapfile: it holds only its fingerprint (J6), and the
    // resume's keys resolve in memory (see `resume_staged_keys`).
    let stats = map.stats();

    // ISO-size validation. The `bytes_pending==0` and coverage gates below both
    // trust the mapfile's `bytes_total`. If that total is short of the real
    // on-disk ISO size, the image is truncated/incomplete — reject and re-sweep fresh.
    match std::fs::metadata(&iso_path) {
        Ok(meta) if meta.len() < stats.bytes_total => {
            tracing::warn!(
                iso = %iso_path.display(),
                iso_len = meta.len(),
                bytes_total = stats.bytes_total,
                "resume: ISO is shorter than mapfile total_size; classifying as not-eligible (fresh sweep)"
            );
            return ResumeClass::NotEligible;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                iso = %iso_path.display(),
                error = %e,
                "resume: cannot stat ISO; classifying as not-eligible (fresh sweep)"
            );
            return ResumeClass::NotEligible;
        }
    }

    if stats.bytes_pending != 0 {
        // Pass 1 didn't fully settle the disc (some sectors still
        // NonTried / NonTrimmed / NonScraped) — let the regular rip
        // path resume sweep + retry instead of jumping to mux.
        return ResumeClass::NotEligible;
    }

    // No loss pre-filter: resume_remux applies the engine's title-scoped loss verdict.

    // The ISO's OWN stem, not the staging dir's name. `rip_disc` builds every
    // file inside a staging dir from `sanitize_path_compact(display_name)`
    // point delete_partial_output at a dotfile; bail loudly instead.
    let display_name = match iso_path.file_stem() {
        Some(n) => n.to_string_lossy().into_owned(),
        None => {
            tracing::warn!(iso = %iso_path.display(), "resume: staged ISO has no file_stem component; not eligible");
            return ResumeClass::NotEligible;
        }
    };

    ResumeClass::Remux {
        iso_path,
        mapfile_path,
        display_name,
        // Cold auto-resume from preserved staging: no `.ripped` hand-off
        // and no operator-override concept here, so confidence is unknown.
        // resume_remux falls back to its own match check.
        title_confident: None,
    }
}

// Walk a staging dir and find the unique .iso plus its matching mapfile
// (<iso>.mapfile). None if there's no ISO, >1 ISO (ambiguous), or no
// mapfile keyed to that exact ISO name (avoids read_dir-order bugs).
pub(super) fn find_iso_and_mapfile(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut isos: Vec<PathBuf> = Vec::new();
    let mut mapfiles: Vec<PathBuf> = Vec::new();
    let read_dir_iter = match std::fs::read_dir(dir) {
        Ok(iter) => iter,
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(),
                error = %e,
                "resume: read_dir failed (NFS ESTALE or missing dir?) — \
                 staging contents unknown, not resuming from this dir"
            );
            return None;
        }
    };
    for entry in read_dir_iter {
        // Don't `.flatten()` away per-entry errors: a partial NFS
        // degradation can error on individual DirEntry I/O while the dir
        // per-entry defense in `snapshot_staging_disc` (staging.rs).
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    dir = %dir.display(),
                    error = %e,
                    "resume: read_dir entry errored (partial NFS degradation?) — \
                     staging contents unknown, not resuming from this dir"
                );
                return None;
            }
        };
        let p = entry.path();
        let name = match p.file_name() {
            Some(n) => n.to_string_lossy().into_owned(),
            None => continue,
        };
        if name.ends_with(".mapfile") {
            mapfiles.push(p);
        } else if name.ends_with(".iso") {
            isos.push(p);
        }
    }
    // Exactly one ISO, or we can't say which staging artefact is the
    // real one — refuse to guess.
    if isos.len() != 1 {
        if isos.len() > 1 {
            tracing::warn!(
                dir = %dir.display(),
                count = isos.len(),
                "resume: multiple .iso files in staging dir — ambiguous, not resuming"
            );
        }
        return None;
    }
    let iso = isos.into_iter().next()?;
    let iso_name = iso.file_name()?.to_string_lossy().into_owned();
    // Canonical mapfile is `<iso-name>.mapfile` (the orchestrator names
    // it `<sanitized>.iso.mapfile`). Match exactly on that.
    let want = format!("{}.mapfile", iso_name);
    let mapfile = mapfiles.into_iter().find(|m| {
        m.file_name()
            .map(|n| n.to_string_lossy() == want.as_str())
            .unwrap_or(false)
    })?;
    Some((iso, mapfile))
}

/// Delete the partial MKV (or `.m2ts`) at `<dir>/<sanitized>.<ext>`.
/// Best-effort: missing file is success, any other error is logged
/// and ignored — the mux step will overwrite whatever's there.
///
/// Extracted as a free function so the unit test can exercise it
/// without touching `Disc::scan_image` or `run_mux`.
pub fn delete_partial_output(staging_disc_dir: &Path, sanitized_name: &str) {
    for ext in ["mkv", "mk3d", "m2ts"] {
        let p = staging_disc_dir.join(format!("{}.{}", sanitized_name, ext));
        match std::fs::remove_file(&p) {
            Ok(_) => tracing::info!(path = %p.display(), "removed partial mux output for resume"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %p.display(), error = %e, "could not delete partial mux output (continuing)")
            }
        }
    }
}

// Reset device to a terminal idle/error UI state at any early-return site in resume_remux,
// before or after "ripping" was set (un-sticks the "already ripping" API gate either way).
fn reset_status_after_ripping(
    device: &str,
    terminal_status: &str,
    display_name: &str,
    disc_format: &str,
    duration: &str,
    last_error: Option<String>,
) {
    let err = last_error.unwrap_or_default();
    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: terminal_status.to_string(),
            disc_present: true,
            disc_name: display_name.to_string(),
            disc_format: disc_format.to_string(),
            duration: duration.to_string(),
            last_error: err,
            ..Default::default()
        },
    );
}

// Fill the NON-success half of a MuxHandoffOutcome from the terminal `_mux` state. Retryability
// comes from the recorded `failure_deferred` bit, never inferred from status text.
fn apply_failure_fields(outcome: &mut MuxHandoffOutcome, rs: &super::RipState) {
    if rs.last_error.is_empty() {
        return;
    }
    outcome.failure_reason = Some(rs.last_error.clone());
    outcome.failure_retryable = rs.failure_deferred;
    // Carry the structural-finalize distinction the worker's quarantine gates
    // on: a deferral is never a finalize failure, and a resumable read error
    // sets neither, leaving it re-muxable.
    outcome.failure_finalize = rs.failure_finalize;
    outcome.failure_space = rs.failure_space;
}

// Quarantine an incomplete-mux staging dir iff the mux died on a structural finalize failure
// (terminal; re-mux would reproduce it). Returns whether the `.failed` write landed.
fn quarantine_incomplete_mux(staging_dir: &Path, finalize_error: Option<&str>) -> bool {
    match finalize_error {
        Some(finalize) => staging::write_failed_marker(staging_dir, finalize),
        None => false,
    }
}

// reset_status_after_ripping for DEFERRAL exits (keys not available yet; staging intact).
// Deferral is recorded on RipState::failure_deferred rather than inferred from status text.
fn defer_status_after_ripping(
    device: &str,
    display_name: &str,
    disc_format: &str,
    duration: &str,
    reason: String,
) {
    reset_status_after_ripping(
        device,
        "idle",
        display_name,
        disc_format,
        duration,
        Some(reason),
    );
    super::update_state_with(device, |s| s.failure_deferred = true);
}

// Every title this resume muxes, in range of the image: a TV plan's episodes, else title 0.
// The open resolves keys for all of them at once (one key-service ask per resume).
fn resume_titles(disc: &libfreemkv::Disc, is_fanout: bool, plan: &[staging::Output]) -> Vec<usize> {
    let mut titles: Vec<usize> = if is_fanout {
        plan.iter().map(|o| o.title_index).collect()
    } else {
        vec![0]
    };
    titles.retain(|&i| i < disc.titles.len());
    titles.sort_unstable();
    titles.dedup();
    if titles.is_empty() {
        titles.push(0);
    }
    titles
}

// A resume's keys, memory only: the inserted disc's up-front set, else the set the process
// that ripped this image resolved at its scan, else the key chain, asked once (with the
// inserted disc's VID, if any). The flag: a drive in hand lent its scan.
fn resume_staged_keys(device: &str, iso: &Path) -> (crate::server::keysource::StagedKeys, bool) {
    use crate::server::keysource::StagedKeys;
    let (drive_keys, drive_vid) = super::session::session_rip_keys(device);
    let from_drive = drive_keys.is_some() || drive_vid.is_some();
    let keys = match drive_keys.or_else(|| crate::server::keysource::rip_keys_for(iso)) {
        Some(set) => StagedKeys::Rip(set),
        None => StagedKeys::Resolve { vid: drive_vid },
    };
    (keys, from_drive)
}

// E7034: the keys need the disc's Volume ID and no drive supplied it.
fn is_vid_needs_disc(e: &libfreemkv::Error) -> bool {
    matches!(e, libfreemkv::Error::AacsVidNeedsDisc)
}

/// Surface a refused resume open once. E7034 is a hold the mux worker does not retry (the
/// disc finishes it); a key miss or key-service failure defers like a keyless capture; a
/// sidecar that is corrupt or another disc's is terminal (a retry reads the same map); a
/// Stop preserves staging; anything else aborts as before.
fn refuse_resume_open(
    cfg: &Config,
    device: &str,
    display_name: &str,
    (iso_path, staging_dir): (&Path, &Path),
    from_drive: bool,
    e: &libfreemkv::Error,
) {
    let log = |line: &str| crate::server::log::device_log(device, line);
    match e {
        libfreemkv::Error::Halted => {
            log("Auto-resume stopped by user while resolving keys; staging preserved.");
            reset_status_after_ripping(device, "idle", display_name, "", "", None);
        }
        libfreemkv::Error::AacsVidNeedsDisc => {
            let msg = super::vid_needs_disc_text();
            log(&format!("Auto-resume waiting for the disc: {msg}"));
            reset_status_after_ripping(device, "error", display_name, "", "", Some(msg));
        }
        libfreemkv::Error::MapfileInvalid { kind } if *kind == "disc-mismatch" && from_drive => {
            // The inserted disc, not the image, is wrong: leave the staging alone.
            let msg = "The disc in the drive is not the disc this staged image was ripped \
                       from. Insert the original disc to finish it."
                .to_string();
            log(&format!("Auto-resume aborted: {msg}"));
            reset_status_after_ripping(device, "error", display_name, "", "", Some(msg));
        }
        libfreemkv::Error::MapfileInvalid { .. } => {
            let msg = super::format_lib_error("checking the saved recovery map", e);
            log(&format!("Auto-resume aborted: {msg}"));
            if !staging::write_failed_marker(staging_dir, &msg) {
                log("The terminal quarantine could not be written; staging is unwritable.");
            }
            reset_status_after_ripping(device, "error", display_name, "", "", Some(msg));
        }
        libfreemkv::Error::FmtsKeyMissing => {
            log(
                "Auto-resume: FMTS forensic keys unavailable — mux deferred. Staging \
                 preserved; will mux automatically once keys are available.",
            );
            let reason = "Ripped to ISO — forensic keys unavailable, mux deferred.".to_string();
            defer_status_after_ripping(device, display_name, "", "", reason);
        }
        e if super::is_key_refusal(e) => {
            let decode_reach = crate::server::keysource::take_online_decode_reachability();
            let src = freemkv_engine::ImageSource::Iso(iso_path.to_path_buf());
            let (log_line, reason) = match freemkv_engine::scan_image(&src) {
                Ok((disc, _)) => super::deferred_keyless_texts(cfg, &disc, decode_reach),
                Err(_) => {
                    let msg = super::format_lib_error("resolving the disc image's keys", e);
                    (msg.clone(), msg)
                }
            };
            log(&log_line);
            defer_status_after_ripping(device, display_name, "", "", reason);
        }
        e => {
            let msg = super::format_lib_error("reading the saved disc image", e);
            log(&format!("Auto-resume aborted: {msg}"));
            reset_status_after_ripping(device, "error", display_name, "", "", Some(msg));
        }
    }
}

// resume_remux callers/behavior notes:
fn resolve_done_codecs(post_mux_state: Option<String>, pre_mux_snapshot: String) -> String {
    post_mux_state
        .filter(|c| !c.is_empty())
        .unwrap_or(pre_mux_snapshot)
}

// Resolve the media_type written into the resume .done/.review marker.
// Mirrors the mover's default-to-"movie" so a cold auto-resume (empty
// STATE) writes an explicit value instead of relying on that fallback.
fn resolve_media_type(carried: &str) -> String {
    if carried.is_empty() {
        "movie".to_string()
    } else {
        carried.to_string()
    }
}

// Handle a durability-gate (fsync) failure on the resume mux output. Caps the _mux worker's
// re-dispatch loop via.restart_count/RESTART_LIMIT the same way resume_or_quarantine_staging
// does.
fn handle_resume_fsync_failure(device: &str, staging_dir: &Path, output_desc: &str) -> bool {
    let count = staging::increment_restart_count(staging_dir).unwrap_or_else(|e| {
        // A failed counter bump must not green-light an infinite loop, but it
        // also can't know the true count — log and treat as below-limit so the
        // next tick re-reads/re-bumps from disk rather than quarantining blindly.
        tracing::warn!(
            staging = %staging_dir.display(),
            error = %e,
            "resume: failed to bump .restart_count after fsync failure"
        );
        0
    });
    if count >= staging::RESTART_LIMIT {
        let reason =
            format!("{output_desc} fsync failed repeatedly ({count} attempts); giving up",);
        crate::server::log::device_log(
            device,
            &format!("Auto-resume: {reason} — quarantining staging (.failed)."),
        );
        // Consult the terminal-write return. If it did NOT land (unwritable
        // staging), do NOT tear down the restart cap and do NOT drop `.ripped`:
        // preserved-for-retry, which is what it actually is.
        if !staging::write_failed_marker(staging_dir, &reason) {
            crate::server::log::syslog(&format!(
                "Auto-resume fsync-failure quarantine FAILED to persist (state.json write error) — {} will keep retrying until the staging mount recovers",
                staging_dir.display()
            ));
            crate::server::log::device_log(
                device,
                &format!(
                    "Auto-resume: {reason}, but the terminal quarantine could NOT be written (staging unwritable) — restart cap preserved; will retry until the mount recovers."
                ),
            );
            // Raise the same operator card the muxer site raises for its own
            // dropped terminal-write (`persist_terminal_mux_quarantine` in
            // `check_and_mux`, so it has no downstream card without this call.
            if device != "_mux" {
                crate::server::muxer::record_error(
                    &staging_dir.to_string_lossy(),
                    &reason,
                    "the terminal quarantine could not be written to state.json (staging mount full / unwritable); auto-resume will keep retrying until the mount recovers — free space or fix permissions on the staging share",
                );
            }
            return false;
        }
        staging::clear_restart_count(staging_dir);
        // Drop the `.ripped` hand-off so the mux worker can't re-queue this
        // now-terminal dir (belt-and-suspenders with the `.failed` guard).
        if let Err(e) = crate::server::muxer::delete_marker(staging_dir) {
            tracing::warn!(
                staging = %staging_dir.display(),
                error = %e,
                "resume: failed to delete .ripped after fsync-failure quarantine; .failed guard prevents re-mux"
            );
        }
        true
    } else {
        false
    }
}

// Raise the operator card `handle_resume_fsync_failure` raises for its own
// dropped terminal-write, on a dropped `.aborted-loss` write — so a
// persistent write failure can't cause silent infinite re-dispatch.
fn record_loss_abort_write_failure(device: &str, staging_dir: &Path, reason: &str) {
    crate::server::log::syslog(&format!(
        "Auto-resume loss-abort quarantine FAILED to persist (state.json write error) — {} will keep retrying until the staging mount recovers",
        staging_dir.display()
    ));
    if device != "_mux" {
        crate::server::muxer::record_error(
            &staging_dir.to_string_lossy(),
            reason,
            "the loss-abort quarantine could not be written to state.json (staging mount full / unwritable); auto-resume will keep retrying until the mount recovers — free space or fix permissions on the staging share",
        );
    }
}

// Hold a dir whose state.json exists but can't be read: leave staging (ISO, partial
// output, the bad state.json) untouched and surface it, since the recorded deliverable
// plan (movie vs. TV episodes) is unknown and a guess would prune the ISO.
fn hold_unreadable_plan(
    device: &str,
    staging_dir: &Path,
    display_name: &str,
    why: &staging::StateUnreadable,
) {
    let reason = why.held_reason();
    crate::server::log::device_log(
        device,
        &format!(
            "{}{} — not re-muxing, since the deliverable plan (movie vs. TV episodes) is unknown. Staging and the ISO are kept ({}).",
            staging::STATE_HELD_PREFIX,
            why.log_text(),
            staging_dir.display()
        ),
    );
    if device != "_mux" {
        crate::server::muxer::record_error(&staging_dir.to_string_lossy(), &reason, why.hint());
    }
    reset_status_after_ripping(device, "error", display_name, "", "", Some(reason));
}

// RAII exclusion lock for the cold operator-resume mux path: writes.muxing so a concurrent
// ResumeMode::Wipe can't delete the ISO out from under an in-flight mux.
struct ResumeMuxingGuard<'a> {
    dir: &'a Path,
    /// True when the synthetic `_mux` worker device already owns the lock — we
    /// then neither write nor clear it, leaving the worker's `MuxingGuard` in
    /// sole charge.
    worker_owned: bool,
}

impl<'a> ResumeMuxingGuard<'a> {
    /// Write `.muxing` (unless the `_mux` worker already holds it) and return a
    /// guard that clears it on drop.
    fn acquire(device: &str, dir: &'a Path) -> Self {
        let worker_owned = device == "_mux";
        if !worker_owned {
            staging::write_muxing_marker(dir);
        }
        Self { dir, worker_owned }
    }
}

impl Drop for ResumeMuxingGuard<'_> {
    fn drop(&mut self) {
        if !self.worker_owned {
            staging::clear_muxing_marker(self.dir);
        }
    }
}

// Resume's call-site wiring into the shared title_is_confident policy, pulled out so the
// argument plumbing is unit-testable without a real Disc::scan_image.
fn resume_title_confident(
    tmdb_api_key: &str,
    carried_confident: Option<bool>,
    disc_label: &str,
    title_for_match: &str,
    tmdb_year: u16,
) -> bool {
    super::title_is_confident(
        tmdb_api_key,
        carried_confident.unwrap_or(false),
        disc_label,
        title_for_match,
        tmdb_year,
    )
}

// The loss threshold a resumed rip is judged against, shared by EVERY loss gate in resume_remux
// so the two gates can't independently recompute and diverge.
fn resume_effective_abort(accept_loss: bool, output_format: &str, configured: u64) -> u64 {
    if accept_loss {
        u64::MAX
    } else {
        super::effective_abort_secs(output_format, configured)
    }
}

// Refusal message when staging can't hold the re-mux outputs. Outputs left by an earlier
// attempt are overwritten, so their bytes count as free; unknown free space is not a refusal.
fn remux_space_shortfall(
    required: u64,
    existing_output_bytes: u64,
    avail: Option<u64>,
    staging: &str,
) -> Option<String> {
    let avail = avail?;
    if avail.saturating_add(existing_output_bytes) >= required {
        return None;
    }
    let gib = |b: u64| b as f64 / crate::server::util::BYTES_PER_GIB;
    Some(format!(
        "Not enough staging disk space to mux the saved disc image — need ≥ {:.1} GiB free at {} for the planned outputs, have {:.1} GiB. Free up space or set Staging Directory in Settings to a larger volume; the disc image is kept for a retry.",
        gib(required.saturating_sub(existing_output_bytes)),
        staging,
        gib(avail),
    ))
}

// The re-mux space check: planned outputs (ISO already staged) vs free space at the dir.
// Returns (required bytes, operator message) when staging is too full.
fn remux_space_refusal(
    cfg: &Config,
    staging_dir: &Path,
    titles: &[libfreemkv::DiscTitle],
    plan_outputs: &[staging::Output],
) -> Option<(u64, String)> {
    let fanout: Vec<usize> = if plan_outputs.len() > 1 {
        plan_outputs.iter().map(|o| o.title_index).collect()
    } else {
        Vec::new()
    };
    let primary = titles.first().map(|t| t.size_bytes).unwrap_or(0);
    let required = super::mux_reserve_for(cfg, titles, &fanout, primary);
    let existing: u64 = plan_outputs
        .iter()
        .filter_map(|o| staging_dir.join(&o.filename).metadata().ok())
        .fold(0u64, |acc, m| acc.saturating_add(m.len()));
    let label = staging_dir.to_string_lossy();
    let avail = staging::staging_free_bytes(&label);
    remux_space_shortfall(required, existing, avail, &label).map(|msg| (required, msg))
}

// Last refused `required` per staging dir, so a persistently full disk logs once.
static SPACE_REFUSED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, u64>>,
> = std::sync::LazyLock::new(Default::default);

// True when this refusal is new for the dir (first, or a different requirement).
fn note_space_refusal(staging_dir: &Path, required: u64) -> bool {
    SPACE_REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(staging_dir.to_path_buf(), required)
        != Some(required)
}

fn forget_space_refusal(staging_dir: &Path) {
    SPACE_REFUSED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(staging_dir);
}

pub fn resume_remux(cfg: &Arc<RwLock<Config>>, device: &str, classification: ResumeClass) {
    let _mux_slot = crate::server::library::arbiter::claim_for_rip();
    let ResumeClass::Remux {
        iso_path,
        mapfile_path,
        display_name,
        title_confident: carried_confident,
    } = classification
    else {
        // Caller bug — should never happen. Log loudly so a future
        // refactor catches it.
        tracing::error!(
            device = %device,
            "resume_remux called with non-Remux classification"
        );
        return;
    };

    // Archive the prior session's per-device log so the live log shows only this
    // resumed-mux operation (as scan_disc / fresh-rip do); otherwise it interleaves
    // with the prior scan's log, making errors hard to correlate.
    crate::server::log::archive_device_log(device);

    let cfg_read = match cfg.read() {
        Ok(c) => c.clone(),
        Err(_) => {
            // status="ripping" is not set yet here so there is no stuck
            // gate, but leave a trace so the silently-vanished resume is
            // diagnosable instead of disappearing with zero explanation.
            crate::server::log::device_log(device, "Auto-resume aborted: config lock poisoned");
            return;
        }
    };

    let staging_dir = iso_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(&cfg_read.staging_dir));

    // The deliverable plan the rip recorded in `state.json` (movie = 1 output;
    // TV = one per episode). Absent = legacy/movie; present-but-unreadable is
    // held, else a TV fanout would deliver as one movie and prune the ISO.
    let plan_outputs: Vec<staging::Output> = match staging::read_state_checked(&staging_dir) {
        staging::StateRead::Valid(s) => s.outputs,
        staging::StateRead::Absent => Vec::new(),
        staging::StateRead::Unreadable(u) => {
            hold_unreadable_plan(device, &staging_dir, &display_name, &u);
            return;
        }
    };
    crate::server::muxer::clear_error_with_prefix(
        &staging_dir.to_string_lossy(),
        staging::STATE_HELD_PREFIX,
    );
    let is_fanout = plan_outputs.len() > 1;

    // One-shot operator override: `.accept-loss` makes the abort gates below
    // treat the threshold as unlimited and re-mux the EXISTING ISO. Consumed
    // only at the hand-off (not at entry) so a transient failure doesn't lose it.
    let accept_loss = staging::accept_loss_requested(&staging_dir);
    if accept_loss {
        crate::server::log::device_log(
            device,
            "Operator accepted the recorded loss — delivering the existing rip despite over-threshold damage.",
        );
    }

    crate::server::log::device_log(
        device,
        &format!(
            "Auto-resume: re-muxing ISO from staging ({})",
            staging_dir.display()
        ),
    );

    // 1. Delete the partial MKV/m2ts if present.
    delete_partial_output(&staging_dir, &display_name);

    // Acquire the `.muxing` exclusion lock for this mux. On the cold
    // operator-resume path the dir carries only the ISO and no live worker, so
    // this guard serializes the mux; its Drop clears the marker when done.
    let _muxing_guard = ResumeMuxingGuard::acquire(device, &staging_dir);

    // 2. Keyless structure scan through the engine, for the cheap gates below
    //    (usable title, staging space) before any key-service round-trip.
    let disc = match freemkv_engine::scan_image(&freemkv_engine::ImageSource::Iso(iso_path.clone()))
    {
        Ok((d, _reader)) => d,
        Err(e) => {
            crate::server::log::device_log(
                device,
                &format!(
                    "Auto-resume aborted: {}",
                    super::format_lib_error("reading the saved disc image", &e)
                ),
            );
            // scan_disc already moved this device to status="scanning"; bailing
            // without the reset below would strand that row. For a later
            // re-dispatch the reset is a harmless no-op — nothing gates on it.
            super::update_state(
                device,
                super::RipState {
                    device: device.to_string(),
                    status: "idle".to_string(),
                    ..Default::default()
                },
            );
            return;
        }
    };

    // Defensive title check — `scan_image` succeeds for any UDF disc
    // but a truncated ISO can still yield zero-duration titles.
    let title_ok = disc
        .titles
        .first()
        .map(|t| t.duration_secs > 0.0)
        .unwrap_or(false);
    if !title_ok {
        crate::server::log::device_log(
            device,
            "Auto-resume aborted: scan_image produced no usable title",
        );
        // Same wedge as the scan_image failure above: reset scanning → idle
        // so the "already ripping" gate doesn't reject every later /api/rip.
        super::update_state(
            device,
            super::RipState {
                device: device.to_string(),
                status: "idle".to_string(),
                ..Default::default()
            },
        );
        return;
    }

    // Space preflight before the key round-trip: a full staging volume must not cost a
    // key-service call per worker tick. Logged once per (dir, required) until it clears.
    if let Some((required, msg)) =
        remux_space_refusal(&cfg_read, &staging_dir, &disc.titles, &plan_outputs)
    {
        if note_space_refusal(&staging_dir, required) {
            crate::server::log::device_log(device, &format!("Auto-resume aborted: {msg}"));
        }
        reset_status_after_ripping(device, "error", &display_name, "", "", Some(msg));
        super::update_state_with(device, |s| s.failure_space = true);
        return;
    }
    forget_space_refusal(&staging_dir);

    // Open the image through the engine. Its keys resolve here, once, for every title
    // this resume muxes (the primary and a TV plan's other episodes), so no later mux of
    // it asks a key source. The Volume ID is never on disk (J6): only a drive has it.
    let titles = resume_titles(&disc, is_fanout, &plan_outputs);
    let (staged_keys, from_drive) = resume_staged_keys(device, &iso_path);
    let halt = super::device_halt(device);
    let image = match crate::server::keysource::open_staged_image(
        &cfg_read,
        &iso_path,
        disc,
        &titles,
        staged_keys,
        halt,
    ) {
        Ok(image) => {
            staging::set_needs_disc(&staging_dir, false);
            image
        }
        Err(e) => {
            staging::set_needs_disc(&staging_dir, is_vid_needs_disc(&e));
            let paths = (iso_path.as_path(), staging_dir.as_path());
            refuse_resume_open(&cfg_read, device, &display_name, paths, from_drive, &e);
            return;
        }
    };
    let disc = &image.disc;

    // Real-bitrate re-validation: recompute bytes-bad-in-title (vs the
    // classifier's whole-disc estimate) and re-check abort_on_lost_secs.
    // `.get` guards a stale/out-of-range index like `.first` did above.
    let primary_index = if is_fanout {
        plan_outputs[0].title_index
    } else {
        0
    };
    let title = match disc.titles.get(primary_index) {
        Some(t) => t.clone(),
        None => {
            crate::server::log::device_log(
                device,
                "Auto-resume aborted: no title after key resolution",
            );
            reset_status_after_ripping(
                device,
                "idle",
                &display_name,
                "",
                "",
                Some("no title after key resolution".to_string()),
            );
            return;
        }
    };
    let title_bytes_per_sec: f64 = freemkv_engine::title_bytes_per_sec(&title);
    // Compute disc_format + duration up front so the abort/early-return
    // paths below can surface them in the UI state (they were previously
    // only available after the mux-build block).
    let disc_format = match disc.format {
        libfreemkv::DiscFormat::Uhd => "uhd",
        libfreemkv::DiscFormat::Fmts => "fmts",
        libfreemkv::DiscFormat::BluRay => "bluray",
        libfreemkv::DiscFormat::HdDvd => "hddvd",
        libfreemkv::DiscFormat::Dvd => "dvd",
        libfreemkv::DiscFormat::Unknown => "unknown",
    }
    .to_string();
    let duration = crate::server::util::format_duration_hm(title.duration_secs);
    let map = match freemkv_engine::Mapfile::load(&mapfile_path) {
        Ok(m) => m,
        Err(e) => {
            // The classifier already loaded this mapfile cleanly; a
            // failure here is a TOCTOU (file removed/corrupted/IO error
            // next pass re-classify against fresh state.
            let msg = format!(
                "Could not read this disc's saved recovery map, so remaining data loss cannot be re-checked — start a fresh rip to rebuild it ({e})."
            );
            crate::server::log::device_log(device, &format!("Auto-resume aborted: {msg}"));
            reset_status_after_ripping(
                device,
                "idle",
                &display_name,
                &disc_format,
                &duration,
                Some(msg),
            );
            return;
        }
    };
    {
        use freemkv_engine::SectorStatus;
        let bad_ranges = map.ranges_with(&[SectorStatus::Unreadable]);
        // Scope the loss exactly as the fresh-rip post-retry abort gate does
        // (`abort_lost_ms` in mod.rs): for `output_format == "iso"` every
        // ABORT — the two paths must reach the same verdict.
        let output_is_iso = super::output_is_iso_image(&cfg_read.output_format);
        let _ = title_bytes_per_sec;
        // ISO output is whole-disc and must be byte-complete: the per-title
        // tolerance is ignored (forced to 0), matching the fresh-rip gate.
        // `.accept-loss` raises the threshold to unlimited for the override.
        let effective_abort = resume_effective_abort(
            accept_loss,
            &cfg_read.output_format,
            cfg_read.abort_on_lost_secs,
        );
        // The engine's one loss verdict, the same the fresh rip gets.
        let verdict =
            freemkv_engine::loss_verdict(output_is_iso, &[&title], &bad_ranges, effective_abort);
        let lost_secs = if verdict.lost_ms.is_finite() {
            verdict.lost_ms / crate::server::util::MILLIS_PER_SEC
        } else {
            0.0
        };
        if verdict.aborts {
            // "disc loss" for raw ISO (whole-disc scope), "title loss" for a
            // muxed MKV/M2TS (in-title scope) — matching how `lost_secs` was
            // computed just above.
            let scope = if super::output_is_iso_image(&cfg_read.output_format) {
                "disc"
            } else {
                "title"
            };
            crate::server::log::device_log(
                device,
                &format!(
                    "Auto-resume aborted: {scope} loss {:.2}s exceeds threshold {}s",
                    lost_secs, effective_abort
                ),
            );
            // Quarantine to a RESUMABLE `.aborted-loss` exactly as the §4 mux-time
            // loss gate below does. WITHOUT this the sweep gate wrote NO terminal or
            // a fresh rip can still deliver), mirroring §4.
            let loss_reason = format!(
                "aborted: {scope} loss {:.2}s exceeds threshold {}s (sweep)",
                lost_secs, effective_abort
            );
            if !staging::mark_aborted_on_loss_reporting_landed(&staging_dir, &loss_reason) {
                record_loss_abort_write_failure(device, &staging_dir, &loss_reason);
            }
            reset_status_after_ripping(
                device,
                "error",
                &display_name,
                &disc_format,
                &duration,
                Some(format!(
                    "{scope} loss {:.2}s exceeds threshold {}s",
                    lost_secs, effective_abort
                )),
            );
            return;
        }
    }

    // 4. Build MuxInputs + run mux exactly as rip_disc does.
    // (`disc_format` + `duration` were computed up front, above.)
    let batch = libfreemkv::disc::detect_max_batch_sectors(DEFAULT_BATCH_PROBE_PATH);

    // Keyless-capture deferral: the ISO was swept raw (no keys), but the MUX needs
    // them. An AACS miss already refused at the open; this catches an uncracked CSS
    // disc, from the rip's key set (the image's disc carries no banked key).
    let undecryptable = matches!(
        freemkv_engine::keys::key_status(disc, &image.keys),
        libfreemkv::keys::DecryptStatus::AacsKeysMissing(_)
            | libfreemkv::keys::DecryptStatus::CssNotCracked(_)
    );
    if undecryptable && !super::output_is_iso_image(&cfg_read.output_format) {
        let decode_reach = crate::server::keysource::take_online_decode_reachability();
        let (log_line, reason) = super::deferred_keyless_texts(&cfg_read, disc, decode_reach);
        crate::server::log::device_log(device, &log_line);
        // We have not set status="ripping" yet (that happens via the
        // update_state call further below). reset_status_after_ripping
        // deferral reason without flagging a hard failure.
        defer_status_after_ripping(device, &display_name, &disc_format, &duration, reason);
        return;
    }

    let output_format = cfg_read.output_format.clone();
    let ext = super::output_extension_for(&output_format, disc);
    // TV fan-out: the primary episode's staging leaf comes from the plan; movies
    // keep the `{display_name}.{ext}` name (unchanged).
    let filename = if is_fanout {
        plan_outputs[0].filename.clone()
    } else {
        format!("{}.{}", display_name, ext)
    };
    let staging_str = staging_dir.to_string_lossy().into_owned();
    let output_path = format!("{}/{}", staging_str, filename);
    let dest_url = if staging::is_network_output(&output_format, &cfg_read.network_target) {
        format!("network://{}", cfg_read.network_target)
    } else {
        // Scheme is the container (mkv/m2ts), NOT the `.mk3d` filename extension —
        // a 3D rip muxes through `mkv://` (libfreemkv has no `mk3d://` scheme).
        format!(
            "{}://{}",
            super::output_scheme_for(&output_format),
            output_path
        )
    };

    let total_bytes = if disc.capacity_bytes > 0 {
        disc.capacity_bytes
    } else {
        title.size_bytes
    };

    // Halt token: register a fresh one so /api/stop has something to
    // cancel during the mux. Mirrors `rip_disc`'s pattern.
    super::register_halt(device, libfreemkv::Halt::new());
    // Registered so `/api/stop` (and `mux_iso`'s own `device_halt` lookup) can
    // cancel the resume mux; the bound handle itself is unused now that
    // the mux looks the token up by device.
    let _halt_token = match super::device_halt(device) {
        Some(h) => h,
        None => {
            // `register_halt` no-ops when the HALTS mutex is poisoned, so
            // device_halt then returns None. The fallback token below was
            // the degraded stop guarantee is at least visible in the log.
            crate::server::log::device_log(
                device,
                "Warning: halt registry unavailable (poisoned); this resume mux will not be stoppable via /api/stop",
            );
            libfreemkv::Halt::new()
        }
    };

    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "ripping".to_string(),
            disc_present: true,
            disc_name: display_name.clone(),
            disc_format: disc_format.clone(),
            output_file: filename.clone(),
            duration: duration.clone(),
            ..Default::default()
        },
    );

    // Build the DiscStream from the ISO reader (re-open — we consumed
    // `iso_reader` for scan_image; constructing a new one is cheap and
    // gives the mux a clean position-zero handle).
    let iso_reader_for_mux = match libfreemkv::FileSectorSource::open(&iso_path) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!(
                "Could not re-open the saved disc image to finish muxing — the staging file may have been moved or the staging volume is unavailable ({e})."
            );
            crate::server::log::device_log(device, &format!("Auto-resume aborted: {msg}"));
            // Reset from "ripping" (set above) → "error" so the next
            // /api/rip isn't blocked by the "already ripping" gate.
            reset_status_after_ripping(
                device,
                "error",
                &display_name,
                &disc_format,
                &duration,
                Some(msg),
            );
            super::unregister_halt(device);
            return;
        }
    };
    // Sweep damage snapshot, re-derived from the mapfile and scoped to the
    // title being muxed.
    let sweep_damage_for_resume = {
        use freemkv_engine::SectorStatus;
        let (bad_ranges, num_bad_ranges, bad_ranges_truncated, total_lost_ms, largest_gap_ms) =
            super::state::build_bad_ranges(&map, &title, title_bytes_per_sec);
        let main_title_bad = map.ranges_with(&[SectorStatus::Unreadable]);
        let main_title_bad_bytes = libfreemkv::disc::bytes_bad_in_title(&title, &main_title_bad);
        let main_lost_ms = if title_bytes_per_sec > 0.0 {
            main_title_bad_bytes as f64 * crate::server::util::MILLIS_PER_SEC / title_bytes_per_sec
        } else {
            0.0
        };
        let errors = (map.stats().bytes_unreadable / crate::server::util::SECTOR_BYTES) as u32;
        super::mux::SweepDamageSnapshot {
            errors,
            total_lost_ms,
            main_lost_ms,
            bad_ranges,
            num_bad_ranges,
            bad_ranges_truncated,
            largest_gap_ms,
        }
    };

    // Clone before move into MuxInputs so the done-state update below
    // can carry sweep damage into the terminal RipState.
    let done_sweep_damage = sweep_damage_for_resume.clone();

    // The pre-opened `iso_reader_for_mux` above is a reachability/permission
    // probe only; the engine mux (inside `mux_iso`) opens its own reader.
    drop(iso_reader_for_mux);

    // Progress + watchdog atomics shared between this function's terminal-state
    // updates, the engine mux sink, and `mux_iso`'s `MuxAtomics`.
    let latest_bytes_read = Arc::new(AtomicU64::new(0));
    let rip_last_lba = Arc::new(AtomicU64::new(0));
    let rip_current_batch = Arc::new(AtomicU16::new(batch));
    let wd_last_frame = Arc::new(AtomicU64::new(crate::server::util::epoch_secs()));
    let mux_input_errors = Arc::new(AtomicU32::new(0));

    // The resume mux runs through the engine (via `mux::mux_iso`) exactly like
    // the fresh multipass path; the Err arm keeps the deferral semantics.
    let iso_src = super::mux::IsoMuxSource {
        image: &image,
        title_index: primary_index,
    };

    // TMDB metadata source of truth: the DURABLE on-disk `.ripped` marker, NOT
    // in-memory STATE — STATE is populated by the fresh-rip scan and is EMPTY on
    // a cold operator-resume, so the durable marker is what we read from.
    let state_tmdb = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned();
    let marker_tmdb = crate::server::muxer::read_marker(&staging_dir).ok();
    let state_codecs = state_tmdb
        .as_ref()
        .map(|rs| rs.codecs.clone())
        .unwrap_or_default();
    let (tmdb_title, tmdb_year, tmdb_poster, tmdb_overview, tmdb_media_type) = match &marker_tmdb {
        Some(m) => (
            m.tmdb_title.clone(),
            m.tmdb_year,
            m.tmdb_poster.clone(),
            m.tmdb_overview.clone(),
            m.tmdb_media_type.clone(),
        ),
        None => state_tmdb
            .as_ref()
            .map(|rs| {
                (
                    rs.tmdb_title.clone(),
                    rs.tmdb_year,
                    rs.tmdb_poster.clone(),
                    rs.tmdb_overview.clone(),
                    rs.tmdb_media_type.clone(),
                )
            })
            .unwrap_or_default(),
    };
    // The mover routes by `media_type` (movie_dir vs tv_dir), defaulting
    // missing/empty to "movie". Resolve the same default here so a cold
    // auto-resume writes an explicit value instead of relying on the reader.
    let media_type = resolve_media_type(&tmdb_media_type);

    // Title-confidence gate — routes through the SAME `title_is_confident`
    // (mod.rs) the fresh-rip completion path uses (the Done/Review hand-off is
    // the same concept), so confidence is purely the match check.
    let disc_label = disc
        .meta_title
        .as_deref()
        .unwrap_or(&disc.volume_id)
        .to_string();
    let title_for_match = if tmdb_title.is_empty() {
        display_name.clone()
    } else {
        tmdb_title.clone()
    };
    // When TMDB is NOT configured (no API key), there is no metadata source
    // that could ever yield a confident match, so EVERY rip would otherwise
    // degrades to `false`, exactly matching "no override concept" below.
    let title_confident = resume_title_confident(
        &cfg_read.tmdb_api_key,
        carried_confident,
        &disc_label,
        &title_for_match,
        tmdb_year,
    );

    // ISO output: deliver the whole-disc image, don't re-mux a title. Mirrors
    // the fresh-rip ISO terminal: the mover validates + moves `.iso`, and the
    // prune below retains it for ISO output.
    if super::output_is_iso_image(&output_format) {
        if !staging::durability_gate_passes(false, || staging::fsync_output_file(&iso_path)) {
            let quarantined = handle_resume_fsync_failure(device, &staging_dir, "ISO image output");
            let detail = if quarantined {
                "ISO image not durable (fsync failed repeatedly); quarantined (.failed)"
            } else {
                "ISO image not durable (fsync failed); preserved for retry"
            };
            crate::server::log::device_log(
                device,
                &format!(
                    "Auto-resume: durability gate failed (could not fsync ISO image); {detail}."
                ),
            );
            reset_status_after_ripping(
                device,
                "error",
                &display_name,
                &disc_format,
                &duration,
                Some(detail.to_string()),
            );
            super::unregister_halt(device);
            return;
        }
        let marker_name = staging::handoff_label(title_confident);
        // One `state.json` hand-off transition. The metadata mirrors the
        // fresh-rip ISO path field-for-field; `season`/`tmdb_id`/`disc` are NOT
        // not drop the TV routing the fresh rip recorded.
        let iso_leaf = iso_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Err(e) = staging::mark_handoff(&staging_dir, title_confident, |s| {
            s.title = display_name.clone();
            s.disc_name = disc_label.clone();
            s.disc_format = disc_format.clone();
            s.year = tmdb_year;
            s.media_type = media_type.clone();
            s.tmdb_poster = tmdb_poster.clone();
            s.tmdb_overview = tmdb_overview.clone();
            s.resumed = true;
            s.outputs = vec![staging::Output {
                filename: iso_leaf,
                ..Default::default()
            }];
        }) {
            crate::server::log::device_log(
                device,
                &format!(
                    "Auto-resume: {} state write failed ({}). Preserving staging for retry.",
                    marker_name, e
                ),
            );
            reset_status_after_ripping(
                device,
                "error",
                &display_name,
                &disc_format,
                &duration,
                Some(format!("{} marker write failed: {}", marker_name, e)),
            );
            super::unregister_halt(device);
            return;
        }
        staging::write_completed_marker(&staging_dir);
        staging::clear_restart_count(&staging_dir);
        if accept_loss {
            // Consume the one-shot override on this success path too (like the MKV
            // hand-off below): the ISO reached the operator, so don't let a stale
            // `.accept-loss` raise the threshold on a future re-run.
            staging::clear_accept_loss_marker(&staging_dir);
        }
        let iso_name = iso_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        crate::server::log::device_log(
            device,
            &format!("Auto-resume: ISO output complete — disc image staged as {iso_name}"),
        );
        super::update_state_with(device, |s| {
            s.status = "done".to_string();
            s.output_file = iso_name;
        });
        super::unregister_halt(device);
        // Honor auto_eject on the resume ISO path the same way the
        // resume MKV terminal (below) and the fresh-rip ISO terminal
        // already ejected, and the drive may hold a different disc.
        if super::should_auto_eject(cfg_read.auto_eject, device) {
            let device_path = format!("/dev/{}", device);
            super::eject_drive(&device_path);
        }
        return;
    }

    let mux_outcome = match super::mux::mux_iso(
        super::mux::MuxInputs {
            device,
            display_name: display_name.clone(),
            disc_format: disc_format.clone(),
            tmdb_title: tmdb_title.clone(),
            tmdb_year,
            tmdb_poster: tmdb_poster.clone(),
            tmdb_overview: tmdb_overview.clone(),
            duration: duration.clone(),
            codecs: state_codecs.clone(),
            filename: filename.clone(),
            total_bytes,
            title_bytes_per_sec,
            // Auto-resume bypasses sweep/retry — surface the *same*
            // `total_passes` (`max_retries + 2`) a fresh sweep+retries+mux
            // would use, so the UI renders `pass N/N · muxing` identically.
            total_passes: cfg_read.max_retries.saturating_add(2),
            bytes_total_disc: disc.capacity_bytes,
            // Pass the real max_retries and bytes_unreadable so that
            // total_pct_byte_weight accounts for the already-completed sweep.
            // the sweep work was already done.
            max_retries: cfg_read.max_retries,
            bytes_unreadable_at_mux: map.stats().bytes_unreadable,
            dest_url,
            batch,
            staging_disc_dir: staging_dir.clone(),
            sweep_damage: sweep_damage_for_resume,
        },
        iso_src,
        super::mux::MuxAtomics {
            latest_bytes_read: latest_bytes_read.clone(),
            rip_last_lba: rip_last_lba.clone(),
            rip_current_batch: rip_current_batch.clone(),
            wd_last_frame: wd_last_frame.clone(),
            wd_bytes: Arc::new(AtomicU64::new(0)),
            input_errors: mux_input_errors,
        },
    ) {
        Ok(o) => o,
        Err(e) => {
            // A Stop during the resume-mux CSS crack surfaces as `Error::Halted`:
            // leave staging intact (the .ripped marker + ISO + mapfile stay) so
            // the next resume retries, without flagging a spurious error.
            if super::is_halt_error(&e) {
                crate::server::log::device_log(
                    device,
                    "Auto-resume mux stopped by user; staging preserved.",
                );
                super::unregister_halt(device);
                return;
            }
            // E7034 from the mux's top-up: hold for the disc, like a refused open.
            if super::io_key_refusal(&e) == Some(libfreemkv::error::E_AACS_VID_NEEDS_DISC) {
                staging::set_needs_disc(&staging_dir, true);
                let msg = super::vid_needs_disc_text();
                crate::server::log::device_log(
                    device,
                    &format!("Auto-resume waiting for the disc: {msg}"),
                );
                reset_status_after_ripping(
                    device,
                    "error",
                    &display_name,
                    &disc_format,
                    &duration,
                    Some(msg),
                );
                super::unregister_halt(device);
                return;
            }
            // FMTS forensic-key deferral: base keys resolved but the online-only
            // forensic index keys did not (`Error::FmtsKeyMissing`). Muxing now
            // the mux worker re-attempts once a keydb/online update supplies keys.
            if super::is_fmts_key_missing_error(&e) {
                crate::server::log::device_log(
                    device,
                    "Auto-resume: FMTS forensic keys unavailable — mux deferred. Staging \
                     preserved; will mux automatically once keys are available.",
                );
                defer_status_after_ripping(
                    device,
                    &display_name,
                    &disc_format,
                    &duration,
                    "Ripped to ISO — forensic keys unavailable, mux deferred.".to_string(),
                );
                super::unregister_halt(device);
                return;
            }
            // Any other key refusal: a keyless deferral, as a refused open is.
            if super::io_key_refusal(&e).is_some() {
                let reach = crate::server::keysource::take_online_decode_reachability();
                let (log_line, reason) = super::deferred_keyless_texts(&cfg_read, disc, reach);
                crate::server::log::device_log(device, &log_line);
                defer_status_after_ripping(device, &display_name, &disc_format, &duration, reason);
                super::unregister_halt(device);
                return;
            }
            // Any other setup failure is structural — surface as "error" (staging
            // preserved, no `.failed`), mirroring the pre-migration build arm.
            tracing::error!(target: "mux", device=%device, "image mux failed: {e}");
            let msg = format!(
                "Mux setup failed — the disc's title or stream layout could not be prepared for muxing. The source may be damaged or use an unsupported format ({e})."
            );
            crate::server::log::device_log(device, &format!("Auto-resume aborted: {msg}"));
            reset_status_after_ripping(
                device,
                "error",
                &display_name,
                &disc_format,
                &duration,
                Some(msg),
            );
            super::unregister_halt(device);
            return;
        }
    };

    super::unregister_halt(device);

    if !mux_outcome.output_opened || !mux_outcome.completed {
        // Mirror rip_disc's incomplete-mux handling (mod.rs): a mid-mux
        // finalize failure or hard producer read error must surface, not be
        // operator no clue why it stopped or that staging is still resumable.
        let (log_prefix, ui_status, ui_failure_reason) = super::incomplete_mux_status(
            mux_outcome.finalize_error.as_deref(),
            mux_outcome.read_error.as_deref(),
        );
        crate::server::log::device_log(
            device,
            &format!(
                "Auto-resume mux did not complete ({log_prefix}) — preserving partial state for next restart",
            ),
        );
        // A finalize error is TERMINAL (structural — e.g. E6008, no muxable
        // frames / unseekable output), unlike a read_error (resumable, MKV just
        // quarantined.
        let is_finalize = mux_outcome.finalize_error.is_some();
        let landed = quarantine_incomplete_mux(&staging_dir, mux_outcome.finalize_error.as_deref());
        if let Some(finalize) = mux_outcome.finalize_error.as_deref() {
            if landed {
                crate::server::log::device_log(
                    device,
                    &format!(
                        "Auto-resume mux finalize failed ({finalize}) — quarantined (state → Failed); staging preserved for inspection",
                    ),
                );
            } else {
                // Terminal write dropped (staging full / unwritable). Surface it the
                // same LOUD way the worker site does; the dir keeps its prior state
                // and retries until the mount recovers.
                crate::server::log::syslog(&format!(
                    "Auto-resume mux quarantine FAILED to persist (state.json write error) — {} will keep re-dispatching until the staging mount recovers",
                    staging_dir.display()
                ));
                crate::server::log::device_log(
                    device,
                    &format!(
                        "Auto-resume mux finalize failed ({finalize}) but the terminal quarantine could NOT be written (staging unwritable) — the mux will retry until the mount recovers",
                    ),
                );
            }
        }
        // Reset from "ripping" → the verdict status so the next /api/rip isn't
        // blocked by the "already ripping" gate. On a read_error / halt staging
        // stays resumable; a finalize error was just quarantined above.
        reset_status_after_ripping(
            device,
            &ui_status,
            &display_name,
            &disc_format,
            &duration,
            ui_failure_reason,
        );
        // Record the structural-finalize distinction on the (just-reset) `_mux`
        // state AFTER the reset wipes it (mirroring `defer_status_after_ripping`):
        // a later tick may re-surface the row, so set the bit here to survive it.
        if is_finalize {
            super::update_state_with(device, |s| s.failure_finalize = true);
        }
        return;
    }

    // A loss is a loss. Mux-time (decrypt/codec) loss is missing in-title data
    // the sweep never saw: the sweep §3 gate can only see mapfile-Unreadable
    // loss, not decrypt/codec loss.
    let demux_lost_secs = mux_outcome.lost_video_secs;

    // Operator-facing loss for a resume = sweep loss + demux loss. A resume can
    // decrypt/codec skips at mux), so they add.
    let done_errors = done_sweep_damage.errors.saturating_add(mux_outcome.errors);
    let done_lost_video_secs =
        done_sweep_damage.main_lost_ms / crate::server::util::MILLIS_PER_SEC + demux_lost_secs;

    // Mux-time loss gate (a loss is a loss). Gate the total in-title loss
    // (sweep + mux-time decrypt/codec) against abort_on_lost_secs before filing
    // complete it). ISO output is exempt (whole-disc, gated by 100% elsewhere).
    {
        // HONOUR `.accept-loss` HERE TOO. This gate used to RECOMPUTE the
        // threshold from raw config while the sweep gate above (§3) used
        // run, both gates.
        let effective_abort =
            resume_effective_abort(accept_loss, &output_format, cfg_read.abort_on_lost_secs);
        // Route through the SAME `mux_loss_aborts` the fresh-rip path
        // (`rip_disc` in mod.rs) uses — its doc comment calls it out as "the
        // above returned), matching the function's `completed` precondition.
        if super::mux_loss_aborts(
            true,
            super::output_is_iso_image(&output_format),
            done_lost_video_secs,
            demux_lost_secs,
            effective_abort,
        ) {
            crate::server::log::device_log(
                device,
                &format!(
                    "Auto-resume ABORT: mux-time loss — {:.2}s missing in main movie (decrypt/codec) exceeds threshold ({}s). A loss is a loss.",
                    done_lost_video_secs, effective_abort
                ),
            );
            let loss_reason = format!(
                "aborted: {:.2}s lost at mux, decrypt/codec (threshold {}s)",
                done_lost_video_secs, effective_abort
            );
            if !staging::mark_aborted_on_loss_reporting_landed(&staging_dir, &loss_reason) {
                record_loss_abort_write_failure(device, &staging_dir, &loss_reason);
            }
            reset_status_after_ripping(
                device,
                "error",
                &display_name,
                &disc_format,
                &duration,
                Some(format!(
                    "aborted — {:.2}s lost at mux, decrypt/codec (threshold {}s)",
                    done_lost_video_secs, effective_abort
                )),
            );
            return;
        }
    }

    // 5. Success — write .completed, drop the hand-off marker, clear
    // .restart_count. Honors the SAME title-confidence gate as fresh-rip
    // (.done vs .review); fsyncs first, preserving staging on failure.
    let is_network = staging::is_network_output(&output_format, &cfg_read.network_target);
    if !staging::durability_gate_passes(is_network, || {
        staging::fsync_output_file(std::path::Path::new(&output_path))
    }) {
        let quarantined = handle_resume_fsync_failure(device, &staging_dir, "mux output");
        let detail = if quarantined {
            "mux output not durable (fsync failed repeatedly); quarantined (.failed)"
        } else {
            "mux output not durable (fsync failed); preserved for retry"
        };
        crate::server::log::device_log(
            device,
            &format!("Auto-resume: durability gate failed (could not fsync mux output); {detail}."),
        );
        reset_status_after_ripping(
            device,
            "error",
            &display_name,
            &disc_format,
            &duration,
            Some(detail.to_string()),
        );
        return;
    }
    // TV fan-out: the primary episode is muxed + durable above and seeds the
    // hand-off `outputs[]`. Now mux the REMAINING episodes from the same ISO, one
    // file each, reusing the disc's already-loaded structure. No-op for movies.
    let mut delivered: Vec<staging::Output> = plan_outputs.first().cloned().into_iter().collect();
    // Network output streams to a SINGLE sink — it can't take N distinct episode
    // files and there's no mover step to relocate local ones, so don't fan out
    // for a network target: deliver the primary episode only.
    if is_fanout && is_network {
        crate::server::log::device_log(
            device,
            "TV disc with network output — delivering the first episode only (a network sink takes a single stream).",
        );
    }
    if is_fanout && !is_network {
        // The engine's episode loop: a failed episode is dropped (its partial file deleted)
        // and the rest still deliver; a Stop ends it.
        let episodes: Vec<usize> = (1..plan_outputs.len()).collect();
        freemkv_engine::run_episodes(&episodes, &freemkv_engine::NoopSink, |i| {
            let extra = &plan_outputs[i];
            let ep_output_path = format!("{staging_str}/{}", extra.filename);
            // Best-effort delete of any partial/undurable output for this episode,
            // so a failed episode is never left on disk for the mover to file.
            let drop_partial = |reason: &str| {
                let _ = std::fs::remove_file(&ep_output_path);
                crate::server::log::device_log(
                    device,
                    &format!(
                        "TV episode E{:02} {reason} — dropped (best-guess auto)",
                        extra.episode.unwrap_or(0)
                    ),
                );
            };
            let Some(ep_title) = disc.titles.get(extra.title_index).cloned() else {
                drop_partial(&format!(
                    "title index {} not present on the disc",
                    extra.title_index
                ));
                return Err(std::io::Error::other("title not on the disc").into());
            };
            let ep_bps: f64 = freemkv_engine::title_bytes_per_sec(&ep_title);
            let ep_dest_url = format!(
                "{}://{}",
                super::output_scheme_for(&output_format),
                ep_output_path
            );
            let ep_inputs = super::mux::MuxInputs {
                device,
                display_name: display_name.clone(),
                disc_format: disc_format.clone(),
                tmdb_title: tmdb_title.clone(),
                tmdb_year,
                tmdb_poster: tmdb_poster.clone(),
                tmdb_overview: tmdb_overview.clone(),
                duration: crate::server::util::format_duration_hm(ep_title.duration_secs),
                codecs: state_codecs.clone(),
                filename: extra.filename.clone(),
                total_bytes,
                title_bytes_per_sec: ep_bps,
                total_passes: cfg_read.max_retries.saturating_add(2),
                bytes_total_disc: disc.capacity_bytes,
                max_retries: cfg_read.max_retries,
                bytes_unreadable_at_mux: map.stats().bytes_unreadable,
                dest_url: ep_dest_url,
                batch,
                staging_disc_dir: staging_dir.clone(),
                // Extra episodes share the disc's captured ISO; per-title sweep
                // damage isn't separately tracked (auto/best-guess).
                sweep_damage: super::mux::SweepDamageSnapshot::default(),
            };
            let ep_src = super::mux::IsoMuxSource {
                image: &image,
                title_index: extra.title_index,
            };
            let ep_atomics = super::mux::MuxAtomics {
                latest_bytes_read: Arc::new(AtomicU64::new(0)),
                rip_last_lba: Arc::new(AtomicU64::new(0)),
                rip_current_batch: Arc::new(AtomicU16::new(batch)),
                wd_last_frame: Arc::new(AtomicU64::new(crate::server::util::epoch_secs())),
                wd_bytes: Arc::new(AtomicU64::new(0)),
                input_errors: Arc::new(AtomicU32::new(0)),
            };
            match super::mux::mux_iso(ep_inputs, ep_src, ep_atomics) {
                Ok(o) if o.output_opened && o.completed => {
                    // Durability gate, same standard as the primary output: only
                    // an episode that fsync'd to stable storage is delivered. A
                    // possibly-truncated episode to the mover.
                    if staging::fsync_output_file(std::path::Path::new(&ep_output_path)) {
                        delivered.push(extra.clone());
                        crate::server::log::device_log(
                            device,
                            &format!(
                                "TV episode E{:02} muxed → {}",
                                extra.episode.unwrap_or(0),
                                extra.filename
                            ),
                        );
                        Ok(())
                    } else {
                        drop_partial("output not durable (fsync failed)");
                        Err(std::io::Error::other("output not durable").into())
                    }
                }
                Ok(_) => {
                    drop_partial("did not complete muxing");
                    Err(std::io::Error::other("did not complete muxing").into())
                }
                Err(e) => {
                    drop_partial(&format!("mux failed ({e})"));
                    Err(e.into())
                }
            }
        });
    }

    let marker_name = staging::handoff_label(title_confident);
    // One `state.json` hand-off transition — the dir-fsync inside it is the crash
    // barrier: the durable hand-off is observed before the later transition
    // overwrites the sweep's `Ripped` state, so a resume must not drop TV routing.
    let mkv_leaf = filename.clone();
    if let Err(e) = staging::mark_handoff(&staging_dir, title_confident, |s| {
        s.title = display_name.clone();
        s.disc_name = disc_label.clone();
        s.disc_format = disc_format.clone();
        s.year = tmdb_year;
        s.media_type = media_type.clone();
        s.tmdb_poster = tmdb_poster.clone();
        s.tmdb_overview = tmdb_overview.clone();
        s.resumed = true;
        if is_fanout {
            // Hand off ONLY the episodes that actually muxed durably. A failed /
            // undurable episode was dropped above (its partial file deleted), so
            // always contains at least the primary episode.
            s.outputs = delivered;
        } else if s.outputs.len() <= 1 {
            // Movie / cold legacy resume: seed the single output when no plan is
            // present.
            s.outputs = vec![staging::Output {
                filename: mkv_leaf,
                ..Default::default()
            }];
        }
    }) {
        // The hand-off marker is what the mover / review UI keys on. If it
        // fails to write (NFS / perms), do NOT write .completed or clear
        // never sees.
        crate::server::log::device_log(
            device,
            &format!(
                "Auto-resume: {} state write failed ({}). \
                 Preserving staging for next-restart retry.",
                marker_name, e
            ),
        );
        reset_status_after_ripping(
            device,
            "error",
            &display_name,
            &disc_format,
            &duration,
            Some(format!("{} marker write failed: {}", marker_name, e)),
        );
        return;
    }
    staging::write_completed_marker(&staging_dir);
    staging::clear_restart_count(&staging_dir);
    if accept_loss {
        // The override is one-shot and it has now been SPENT: this run reached
        // the hand-off, so the accepted-loss output is on its way to the mover.
        // operator's consent away on unrelated transient failures.
        staging::clear_accept_loss_marker(&staging_dir);
    }
    if !title_confident {
        crate::server::log::device_log(
            device,
            "Auto-resume: title match not confident — held for operator review (.review)",
        );
    }

    // Prune the disc-sized intermediate ISO + its mapfile unless keep_iso is
    // set, mirroring rip_disc's inline terminal path. The inline resume terminal
    // and the fresh-rip completion routes both now share `prune_intermediate_iso`.
    super::prune_intermediate_iso(
        device,
        &iso_path,
        &mapfile_path,
        // The RIP-TIME max_retries the ISO was produced with, NOT the current
        // config. This dir is resumed FROM a staged ISO, so that ISO exists
        // definition. `keep_iso` still protects an ISO the operator kept.
        marker_tmdb.as_ref().map(|m| m.max_retries).unwrap_or(1),
        super::retain_intermediate_iso(cfg_read.keep_iso, &output_format),
    );

    // Prefer the codecs the mux frame loop wrote into STATE (the
    // `_mux` worker path seeds an empty codecs and only fills it
    // below report the same codec string.
    let done_codecs = resolve_done_codecs(
        super::STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device)
            .map(|rs| rs.codecs.clone()),
        state_codecs,
    );

    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "done".to_string(),
            disc_present: true,
            disc_name: display_name.clone(),
            disc_format: disc_format.clone(),
            progress_pct: 100,
            output_file: staging_str.clone(),
            duration: duration.clone(),
            // Carry the TMDB metadata + codecs into the done card, mirroring
            // rip_disc's terminal state — without these the done-card for a
            // resumed rip loses the poster, title, year, and codec badge.
            tmdb_title,
            tmdb_year,
            tmdb_poster: tmdb_poster.clone(),
            tmdb_overview,
            codecs: done_codecs.clone(),
            // Carry sweep damage so the done card reflects real damage
            // instead of showing a clean result for a damaged rip. `errors`
            // classifier rates the disc on the loss actually in the MKV.
            errors: done_errors,
            lost_video_secs: done_lost_video_secs,
            total_lost_ms: done_sweep_damage.total_lost_ms
                + demux_lost_secs * crate::server::util::MILLIS_PER_SEC,
            main_lost_ms: done_sweep_damage.main_lost_ms
                + demux_lost_secs * crate::server::util::MILLIS_PER_SEC,
            bad_ranges: done_sweep_damage.bad_ranges.clone(),
            num_bad_ranges: done_sweep_damage.num_bad_ranges,
            bad_ranges_truncated: done_sweep_damage.bad_ranges_truncated,
            largest_gap_ms: done_sweep_damage.largest_gap_ms,
            ..Default::default()
        },
    );
    crate::server::log::device_log(device, "Auto-resume complete");

    // Fire the mux-stage webhook, mirroring rip_disc's terminal
    // branch. Both the cold auto-resume (`?resume=yes`) path and the
    // `.ripped` hand-off — this is the distinct mux_complete stage.
    crate::server::webhook::send_rich(
        &cfg_read,
        crate::server::webhook::WebhookEvent::Mux,
        &crate::server::webhook::RipEvent {
            event: "mux_complete",
            title: &display_name,
            year: tmdb_year,
            format: &disc_format,
            poster_url: &tmdb_poster,
            duration: &duration,
            codecs: &done_codecs,
            size_gb: mux_outcome.bytes_done as f64 / crate::server::util::BYTES_PER_GIB,
            speed_mbs: mux_outcome.speed_mbs,
            elapsed_secs: mux_outcome.elapsed_secs,
            output_path: &staging_str,
            // Sweep loss + demux loss (same combined figures as the done
            // card) so the completion notification reports the real loss in
            // the delivered MKV, not the sweep-mapfile-only subset.
            errors: done_errors,
            lost_video_secs: done_lost_video_secs,
        },
    );

    // Honor auto_eject after a successful resume the same way
    // (the drive may even hold a different disc by now).
    if super::should_auto_eject(cfg_read.auto_eject, device) {
        let device_path = format!("/dev/{}", device);
        super::eject_drive(&device_path);
    }
}

// Runs a mux-from-staging pass as if it were an auto-resume against a synthetic "_mux" device
// key. Result carries mux-derived display fields the origin device's done-state needs.
#[derive(Default)]
pub(crate) struct MuxHandoffOutcome {
    pub success: bool,
    pub codecs: String,
    pub duration: String,
    pub output_file: String,
    // Full bad-ranges drilldown (+ truncation count).
    pub bad_ranges: Vec<super::state::BadRange>,
    pub bad_ranges_truncated: u32,
    // Combined sweep + mux-time loss figures.
    pub lost_video_secs: f64,
    pub errors: u32,
    pub total_lost_ms: f64,
    pub main_lost_ms: f64,
    // Real non-success reason, read off _mux device state; empty on success.
    pub failure_reason: Option<String>,
    // True for a keyless DEFERRAL (retryable) vs a hard failure.
    pub failure_retryable: bool,
    // True only for a structural FINALIZE failure — the sole class the mux worker may
    // quarantine.
    pub failure_finalize: bool,
    // True when the re-mux was refused up front for lack of staging space (retryable).
    pub failure_space: bool,
    // True for the E7034 hold: only the disc can finish the keys (not retried).
    pub failure_needs_disc: bool,
}

// Whether resume_remux finished this staging dir cleanly (.completed written). Probes via
// snapshot_staging_disc, not a bare Path::exists(), to avoid an NFS cold-cache false-negative.
pub(crate) fn mux_handoff_success(staging_dir: &std::path::Path) -> bool {
    crate::server::ripper::staging::snapshot_staging_disc(staging_dir)
        .map(|s| s.completed)
        .unwrap_or(false)
}

// Build the initial MuxHandoffOutcome from the success signal, pulled out so the `success`
// field assignment is directly unit-testable.
fn build_mux_handoff_outcome(success: bool) -> MuxHandoffOutcome {
    MuxHandoffOutcome {
        success,
        ..Default::default()
    }
}

pub(crate) fn remux_from_ripped_marker(
    cfg: &Arc<RwLock<Config>>,
    staging_dir: &std::path::Path,
    marker: &crate::server::muxer::RippedMarker,
) -> MuxHandoffOutcome {
    let iso_path = std::path::PathBuf::from(&marker.iso_path);
    let mapfile_path = std::path::PathBuf::from(&marker.mapfile_path);
    let mux_device = "_mux";

    // Pre-seed STATE so `run_mux`'s TMDB-from-STATE lookup finds the
    // metadata we want on the history record. Codecs gets filled by the
    // worker's scan_image below; the initial seed writes an empty string.
    super::update_state(
        mux_device,
        super::RipState {
            device: mux_device.to_string(),
            tmdb_title: marker.tmdb_title.clone(),
            tmdb_year: marker.tmdb_year,
            tmdb_poster: marker.tmdb_poster.clone(),
            tmdb_overview: marker.tmdb_overview.clone(),
            tmdb_media_type: marker.tmdb_media_type.clone(),
            ..Default::default()
        },
    );

    let classification = ResumeClass::Remux {
        iso_path: iso_path.clone(),
        mapfile_path: mapfile_path.clone(),
        display_name: marker.display_name.clone(),
        // Carry the fresh-rip confidence verdict (incl. operator override)
        // from the hand-off marker so resume_remux doesn't recompute it
        // from the match check alone.
        title_confident: Some(marker.title_confident),
    };
    resume_remux(cfg, mux_device, classification);

    // Success signal: `resume_remux` wrote `.completed` to staging.
    // Anything else (halt, scan_image failure, mux loop break)
    // leaves `.completed` absent.
    let success = mux_handoff_success(staging_dir);
    let mut outcome = build_mux_handoff_outcome(success);
    outcome.failure_needs_disc =
        !success && staging::snapshot_staging_disc(staging_dir).is_some_and(|snap| snap.needs_disc);
    if success {
        crate::server::keysource::forget_rip_keys(&iso_path);
        // Hand-off consumed. Drop the marker so this dir doesn't get
        // re-queued on the next muxer tick. If the delete fails, surface
        // marker is still worth a warning so the operator can clear it.
        if let Err(e) = crate::server::muxer::delete_marker(staging_dir) {
            tracing::warn!(
                staging = %staging_dir.display(),
                error = %e,
                "failed to delete .ripped marker after successful mux; .completed guard prevents re-mux"
            );
        }
    }
    // Read the synthetic `_mux` state BEFORE removing the entry (success carries
    // the mux-derived display fields; failure applies the failure fields). Recover
    // on poison — a guarded `.ok()` would leak the ghost `_mux` entry and skip cleanup.
    let mut s = super::STATE.lock().unwrap_or_else(|e| e.into_inner());
    {
        if let Some(rs) = s.get(mux_device) {
            if success {
                outcome.codecs = rs.codecs.clone();
                outcome.duration = rs.duration.clone();
                outcome.output_file = rs.output_file.clone();
                // Carry the full bad-ranges drilldown (recomputed from the
                // mapfile by resume_remux) so the origin device's done card
                // matches the `_mux` and fresh-rip cards instead of being empty.
                outcome.bad_ranges = rs.bad_ranges.clone();
                outcome.bad_ranges_truncated = rs.bad_ranges_truncated;
                // Carry the COMBINED sweep + mux-time loss the `_mux`
                // done-state computed (sweep mapfile loss folded with
                // instead of understating loss in the delivered MKV.
                outcome.lost_video_secs = rs.lost_video_secs;
                outcome.errors = rs.errors;
                outcome.total_lost_ms = rs.total_lost_ms;
                outcome.main_lost_ms = rs.main_lost_ms;
            } else {
                // Never inferred from the terminal status: three hard-failure
                // exits in `resume_remux` write "idle" too, so that inference
                // owns the grading (and is where the tests reach it).
                apply_failure_fields(&mut outcome, rs);
            }
        }
        // Clean up the synthetic STATE entry so the device tile grid
        // (which already filters underscore keys, but still — be tidy)
        // doesn't accumulate per-mux ghosts.
        s.remove(mux_device);
    }
    outcome
}

// Tests live in `tests/resume_remux.rs` (integration tests) — they
// pattern-match on `ResumeClass` and exercise `classify_resume`. But
// `find_iso_and_mapfile` is `pub(super)`, so it's unit-tested in-module here.

#[cfg(test)]
mod remux_space_tests {
    use super::remux_space_shortfall;

    const GB: u64 = 1_000_000_000;

    #[test]
    fn remux_refuses_when_staging_cannot_hold_the_planned_outputs() {
        let msg = remux_space_shortfall(30 * GB, 0, Some(10 * GB), "/staging/sr0")
            .expect("10 GB free cannot hold 30 GB of episodes");
        assert!(msg.contains("/staging/sr0"), "{msg}");
        assert!(msg.contains("Staging Directory"), "{msg}");
        assert!(!msg.contains("STAGING_DIR"), "{msg}");
        // Earlier-attempt outputs get overwritten, so they count toward free space.
        assert_eq!(
            remux_space_shortfall(30 * GB, 20 * GB, Some(10 * GB), "/s"),
            None
        );
        assert_eq!(remux_space_shortfall(30 * GB, 0, Some(30 * GB), "/s"), None);
        assert_eq!(remux_space_shortfall(0, 0, Some(0), "/s"), None);
        assert_eq!(remux_space_shortfall(30 * GB, 0, None, "/s"), None);
    }

    #[test]
    fn repeated_identical_space_refusal_is_noted_once() {
        let dir = std::path::PathBuf::from(format!("/nonexistent/space-{}", std::process::id()));
        assert!(
            super::note_space_refusal(&dir, 30 * GB),
            "first refusal logs"
        );
        assert!(
            !super::note_space_refusal(&dir, 30 * GB),
            "identical repeat is quiet"
        );
        assert!(
            super::note_space_refusal(&dir, 31 * GB),
            "a changed need logs again"
        );
        super::forget_space_refusal(&dir);
        assert!(
            super::note_space_refusal(&dir, 31 * GB),
            "logs again after clearing"
        );
        super::forget_space_refusal(&dir);
    }

    #[test]
    fn space_refusal_threads_ripstate_to_the_worker_outcome() {
        let rs = crate::server::ripper::RipState {
            last_error: "Not enough staging disk space".to_string(),
            failure_space: true,
            ..crate::server::ripper::RipState::default()
        };
        let mut outcome = super::MuxHandoffOutcome::default();
        super::apply_failure_fields(&mut outcome, &rs);
        assert!(outcome.failure_space, "space bit must reach the mux worker");
        assert!(!outcome.failure_finalize && !outcome.failure_retryable);
    }

    // Wiring pin: resume_remux runs the space check (sized by mux_reserve_for) before
    // the key round-trip, and flags the refusal on RipState.
    #[test]
    fn resume_remux_checks_space_before_key_resolution() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        let body = &src[src.find("\nfn remux_space_refusal(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(body.contains("super::mux_reserve_for(cfg, titles, &fanout, primary)"));
        assert!(body.contains("remux_space_shortfall(required, existing, avail, &label)"));
        let f = &src[src.find("\npub fn resume_remux(").unwrap()..];
        let check = f
            .find("remux_space_refusal(&cfg_read, &staging_dir")
            .unwrap();
        let keys = f.find("keysource::open_staged_image(").unwrap();
        assert!(check < keys, "space check must precede key resolution");
        assert!(f[check..keys].contains("s.failure_space = true"));
    }
}

#[cfg(test)]
mod find_iso_tests {
    use super::find_iso_and_mapfile;
    use std::fs;

    fn tmpdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        // Repo-local scratch, never /tmp — anchor to the crate's own
        // target/ dir so artifacts are cleaned by `cargo clean` (mirrors
        // the staging.rs test helper).
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!(
                "autorip-find-iso-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
            ));
        // Wipe any stale contents first: `target/test-scratch` persists across
        // runs and a CI pid can be reused between debug/release test binaries,
        // so a leftover `subdir` would make `create_dir` fail with AlreadyExists.
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn pairs_iso_with_matching_mapfile() {
        let d = tmpdir();
        fs::write(d.join("Movie.iso"), b"x").unwrap();
        fs::write(d.join("Movie.iso.mapfile"), b"x").unwrap();
        let (iso, map) = find_iso_and_mapfile(&d).expect("should pair");
        assert!(iso.ends_with("Movie.iso"));
        assert!(map.ends_with("Movie.iso.mapfile"));
    }

    #[test]
    fn rejects_when_mapfile_does_not_match_iso() {
        let d = tmpdir();
        fs::write(d.join("Movie.iso"), b"x").unwrap();
        // Mapfile keyed to a *different* ISO name — must not be paired.
        fs::write(d.join("Other.iso.mapfile"), b"x").unwrap();
        assert!(find_iso_and_mapfile(&d).is_none());
    }

    #[test]
    fn rejects_multiple_isos_as_ambiguous() {
        let d = tmpdir();
        fs::write(d.join("A.iso"), b"x").unwrap();
        fs::write(d.join("A.iso.mapfile"), b"x").unwrap();
        fs::write(d.join("B.iso"), b"x").unwrap();
        assert!(find_iso_and_mapfile(&d).is_none());
    }

    #[test]
    fn rejects_missing_mapfile() {
        let d = tmpdir();
        fs::write(d.join("Movie.iso"), b"x").unwrap();
        assert!(find_iso_and_mapfile(&d).is_none());
    }

    // Regression: the loop no longer uses `.flatten()` (which silently
    // dropped per-DirEntry I/O errors). Per-entry error handling must not
    // break the happy path with extra unrelated entries alongside the pair.
    #[test]
    fn pairs_despite_extra_entries() {
        let d = tmpdir();
        fs::write(d.join("Movie.iso"), b"x").unwrap();
        fs::write(d.join("Movie.iso.mapfile"), b"x").unwrap();
        // Noise the scan must skip over.
        fs::write(d.join("Movie.mkv"), b"x").unwrap();
        fs::write(d.join(".keep"), b"x").unwrap();
        fs::create_dir(d.join("subdir")).unwrap();
        let (iso, map) = find_iso_and_mapfile(&d).expect("should pair");
        assert!(iso.ends_with("Movie.iso"));
        assert!(map.ends_with("Movie.iso.mapfile"));
    }
}

// A hard failure must not be advertised to the operator as a deferral that will fix itself.
#[cfg(test)]
mod failure_retryability_tests {
    use super::*;

    fn state_of(device: &str) -> crate::server::ripper::RipState {
        super::super::STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device)
            .cloned()
            .expect("the terminal write must have created a STATE entry")
    }

    /// Grade a terminal state exactly as `remux_from_ripped_marker`'s
    /// non-success branch does, and hand back the outcome the muxer reads.
    fn graded(device: &str) -> MuxHandoffOutcome {
        let rs = state_of(device);
        let mut outcome = build_mux_handoff_outcome(false);
        apply_failure_fields(&mut outcome, &rs);
        outcome
    }

    #[test]
    fn a_hard_failure_that_lands_on_idle_is_not_reported_as_retryable() {
        // Unique per test: STATE is process-global.
        let dev = format!("_retryable-hard-{}", std::process::id());
        // Verbatim shape of resume_remux's unreadable-mapfile exit.
        reset_status_after_ripping(
            &dev,
            "idle",
            "Some Disc",
            "bluray",
            "1:52",
            Some("Could not read this disc's saved recovery map".to_string()),
        );
        assert_eq!(
            state_of(&dev).status,
            "idle",
            "the fixture must reproduce the trap"
        );

        let outcome = graded(&dev);

        assert_eq!(
            outcome.failure_reason.as_deref(),
            Some("Could not read this disc's saved recovery map"),
            "the operator must be told the real reason the dir didn't advance"
        );
        assert!(
            !outcome.failure_retryable,
            "a hard failure must not be graded retryable just because its \
             terminal status is \"idle\" — that puts \"will mux automatically \
             once keys are available\" on a corrupt ISO's error card"
        );
    }

    #[test]
    fn a_keyless_deferral_is_reported_as_retryable() {
        let dev = format!("_retryable-deferral-{}", std::process::id());
        defer_status_after_ripping(
            &dev,
            "Some Disc",
            "bluray",
            "1:52",
            "Ripped to ISO — no keys, mux deferred.".to_string(),
        );
        assert_eq!(
            state_of(&dev).status,
            "idle",
            "a deferral still reads as idle"
        );

        let outcome = graded(&dev);

        assert!(
            outcome.failure_retryable,
            "the keyless deferral is the case that IS retryable — the ISO \
             stays staged and muxes itself once keys land"
        );
    }

    // A non-success with NO recorded error records nothing at all, so
    // crate::server::muxer's Some("")-vs-None dispatch falls through to its own
    // .aborted-loss / failed-reason fallbacks instead of a blank card.
    #[test]
    fn a_non_success_without_a_recorded_error_reports_no_reason() {
        let dev = format!("_retryable-silent-{}", std::process::id());
        reset_status_after_ripping(&dev, "idle", "Some Disc", "bluray", "1:52", None);
        assert!(
            state_of(&dev).last_error.is_empty(),
            "the fixture must leave last_error empty"
        );

        let outcome = graded(&dev);

        assert!(
            outcome.failure_reason.is_none(),
            "an empty last_error must leave failure_reason None so the muxer \
             uses its own fallback hints"
        );
        assert!(!outcome.failure_retryable);
    }

    // Pins remux_from_ripped_marker's source-level wiring to this grading (can't drive it
    // directly without a full mux pipeline) — same technique used for resume_remux's webhook
    // call sites.
    #[test]
    fn the_non_success_branch_routes_through_apply_failure_fields() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        let start = src
            .find("pub(crate) fn remux_from_ripped_marker(")
            .expect("remux_from_ripped_marker must exist");
        let region = &src[start..];
        let end = region
            .find("\n/// Resolve keys for a resumed disc")
            .unwrap_or(region.len());
        let region = &region[..end];
        assert!(
            region.contains("apply_failure_fields(&mut outcome, rs);"),
            "remux_from_ripped_marker's non-success branch must grade through \
             apply_failure_fields, not a hand-rolled status inference"
        );
        assert!(
            !region.contains("rs.status == \"idle\""),
            "remux_from_ripped_marker must never infer retryability from the \
             terminal status: three hard-failure exits write \"idle\" too"
        );
    }
}

// Resume's wiring into the shared `.done`/`.review` policy (mod.rs
// `title_is_confident` / `handoff_marker_name`). resume_remux previously
// that `mux_loss_aborts` had. These pin the call-site plumbing.
#[cfg(test)]
mod title_confidence_routing_tests {
    use super::super::handoff_marker_name;
    use super::resume_title_confident;

    /// No TMDB key configured → always confident, regardless of carried
    /// state or match, matching `title_is_confident`'s "operators running
    /// keyless expect the disc-label filename" rule.
    #[test]
    fn no_api_key_is_always_confident() {
        assert!(resume_title_confident(
            "",
            None,
            "BD_ROM_R1",
            "Casablanca",
            1942
        ));
        assert!(resume_title_confident(
            "   ",
            Some(false),
            "BD_ROM_R1",
            "Casablanca",
            1942
        ));
    }

    // carried_confident = Some(true) must be OR'd in even when the
    // current match check alone says no: an operator's deliberate pick
    // must not be second-guessed back into .review on resume.
    #[test]
    fn carried_confident_true_overrides_a_weak_match() {
        assert!(resume_title_confident(
            "tmdb-key",
            Some(true),
            "BD_ROM_R1",
            "Some Guessed Title",
            0,
        ));
    }

    // carried_confident = None (cold auto-resume) must fall through to
    // the plain match check, not be treated as confident by default.
    #[test]
    fn no_carried_state_falls_through_to_the_match_check() {
        assert!(
            !resume_title_confident("tmdb-key", None, "BD_ROM_R1", "Casablanca", 1942),
            "a disc-label title with no year match must not be confident"
        );
        assert!(resume_title_confident(
            "tmdb-key",
            None,
            "THE_MATRIX",
            "The Matrix",
            1999,
        ));
    }

    /// The marker-name choice itself: confident → `.done`, not → `.review`.
    #[test]
    fn marker_name_follows_confidence() {
        assert_eq!(handoff_marker_name(true), ".done");
        assert_eq!(handoff_marker_name(false), ".review");
    }

    // resume_remux's real call site can't be driven end-to-end (needs a
    // real Disc::scan_image), so pin the exact argument order at the
    // source level as a stopgap against a silent argument swap.
    #[test]
    fn resume_remux_calls_resume_title_confident_with_disc_label_then_match_title() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        assert!(
            src.contains(
                "resume_title_confident(\n        &cfg_read.tmdb_api_key,\n        carried_confident,\n        &disc_label,\n        &title_for_match,\n        tmdb_year,\n    )"
            ),
            "resume_remux must call resume_title_confident(tmdb_api_key, carried_confident, \
             disc_label, title_for_match, tmdb_year) in that exact argument order"
        );
    }

    // The cold-auto-resume ISO completion branch is a separate call site
    // from the MKV one, so it needs its own pin: it must route through
    // handoff_marker_name, not keep its own .done/.review ternary.
    #[test]
    fn iso_completion_uses_handoff_marker_name() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        let start = src
            .find("ISO output: deliver the whole-disc image")
            .expect("resume.rs should have the ISO completion branch");
        // Bound at the MKV-path's own gate note (pinned separately by
        // `completed_mux_with_loss_gated_by_abort_on_lost_secs`), NOT at the
        // call instead of the ISO site's.
        let end = src[start..]
            .find("A loss is a loss. Mux-time")
            .map(|i| start + i)
            .expect("resume.rs should have the mux-time-loss note after the ISO branch");
        let region = &src[start..end];
        assert!(
            region.contains("mark_handoff("),
            "the ISO completion branch must hand off via staging::mark_handoff, not a duplicated ternary"
        );
    }
}

// Convergence M (finding 5): `remux_from_ripped_marker` detects whether
// `resume_remux` succeeded by checking for `.completed`. It must use the
// that matter.
#[cfg(test)]
mod completion_detection_tests {
    use crate::server::ripper::staging;

    fn tmpdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!(
                "autorip-resume-complete-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
            ));
        // See the note in the find-iso `tmpdir`: clear stale contents so a
        // reused scratch path (persistent dir + CI pid reuse) starts empty.
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // Calls the real mux_handoff_success helper (not a hand-rolled copy)
    // so this proves what remux_from_ripped_marker will observe: true
    // after write_completed_marker, so clear_error runs and nothing sticks.
    #[test]
    fn snapshot_reports_completed_after_marker_write() {
        let d = tmpdir();
        staging::write_completed_marker(&d);
        assert!(
            super::mux_handoff_success(&d),
            "mux_handoff_success must report true after write_completed_marker"
        );
    }

    /// And without the marker (halt / scan_image failure / mux loop break) it
    /// must report `false` so the failure path records the error.
    #[test]
    fn snapshot_reports_not_completed_without_marker() {
        let d = tmpdir();
        // A partial dir with ISO/mapfile but no `.completed`.
        std::fs::write(d.join("Movie.iso"), b"x").unwrap();
        std::fs::write(d.join("Movie.iso.mapfile"), b"x").unwrap();
        assert!(
            !super::mux_handoff_success(&d),
            "mux_handoff_success must report false without the .completed marker"
        );
    }

    // build_mux_handoff_outcome must carry `success` through both ways,
    // guarding against a struct-literal mutant that silently drops it
    // and reports a genuinely successful resumed mux as failed.
    #[test]
    fn build_mux_handoff_outcome_carries_success_both_ways() {
        assert!(super::build_mux_handoff_outcome(true).success);
        assert!(!super::build_mux_handoff_outcome(false).success);
    }
}

// Regression guard for the resume abort-gate scoping. The fresh-rip
// post-retry abort check scopes loss by `output_format` via
// in lockstep.
#[cfg(test)]
mod resume_abort_scope_tests {
    fn title_lba(start_lba: u32, sector_count: u32) -> libfreemkv::DiscTitle {
        let mut t = libfreemkv::DiscTitle::empty();
        t.extents.push(libfreemkv::disc::Extent {
            start_lba,
            sector_count,
        });
        t
    }

    #[test]
    fn iso_resume_counts_out_of_title_loss() {
        // Out-of-title unreadable range only. For output_format=iso the
        // resume gate must see positive loss (whole-disc scope) — same as
        // a fresh ISO rip would, so both abort under abort_on_lost_secs=0.
        let bps = 8_250_000.0;
        let title = title_lba(1000, 1000);
        let bad = vec![(0u64, 50 * 2048)];
        let lost_secs =
            freemkv_engine::abort_lost_ms(/* output_is_iso */ true, &title, &bad, bps)
                / crate::server::util::MILLIS_PER_SEC;
        assert!(
            lost_secs > 0.0,
            "iso resume must count whole-disc (out-of-title) loss"
        );
    }

    #[test]
    fn mkv_resume_ignores_out_of_title_loss() {
        // Same out-of-title range, mkv/m2ts output → in-title scope → 0,
        // so the resume gate proceeds to mux (matching fresh-rip mkv).
        let bps = 8_250_000.0;
        let title = title_lba(1000, 1000);
        let bad = vec![(0u64, 50 * 2048)];
        let lost_secs =
            freemkv_engine::abort_lost_ms(/* output_is_iso */ false, &title, &bad, bps)
                / crate::server::util::MILLIS_PER_SEC;
        assert_eq!(
            lost_secs, 0.0,
            "mkv resume must ignore out-of-title loss (in-title scope)"
        );
    }
}

#[cfg(test)]
mod resume_remux_log_archive_tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!(
                "autorip-resume-archive-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
            ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("logs")).unwrap();
        p
    }

    // Regression: resume_remux did not archive the prior session's per-device log on entry
    // (unlike scan_disc/rip_disc), so "scan then resume" interleaved log entries.
    #[test]
    fn resume_remux_archives_prior_device_log() {
        // Held for the whole test: AUTORIP_DIR is process-wide and cargo runs
        // tests in parallel, so re-pointing it without this guard corrupts
        // concurrent tests. `env_guard()` also RESTORES the prior value on drop.
        let _guard = crate::server::log::env_guard();
        let d = tmpdir();
        // Route logs to the tempdir for this test. SAFETY: env access in
        // tests; the assertion that matters reads the in-memory ring
        // (keyed by the unique device name), not the env-routed file.
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }

        let dev = format!("test_resume_archive_sg_{}", std::process::id());

        // Seed a prior session's log line, as a scan/rip would leave behind.
        crate::server::log::device_log(&dev, "PRIOR-SESSION-SCAN-LINE");
        assert!(
            crate::server::log::get_device_log(&dev, 100)
                .iter()
                .any(|l| l.contains("PRIOR-SESSION-SCAN-LINE")),
            "prior line should be present before resume"
        );

        // A Remux classification pointing at a non-existent ISO so the
        // function archives + logs the resume line, then aborts on open.
        let class = ResumeClass::Remux {
            iso_path: d.join("does-not-exist.iso"),
            mapfile_path: d.join("does-not-exist.iso.mapfile"),
            display_name: "Nonexistent".to_string(),
            title_confident: None,
        };
        let cfg = Arc::new(RwLock::new(Config::default()));

        resume_remux(&cfg, &dev, class);

        let live = crate::server::log::get_device_log(&dev, 100);
        assert!(
            !live.iter().any(|l| l.contains("PRIOR-SESSION-SCAN-LINE")),
            "prior session line must be archived out of the live log, got: {:?}",
            live
        );
        assert!(
            live.iter().any(|l| l.contains("Auto-resume: re-muxing")),
            "live log should contain the new resume entry, got: {:?}",
            live
        );

        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod resume_remux_unreadable_plan_tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!(
                "autorip-resume-unreadable-plan-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
            ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("logs")).unwrap();
        p
    }

    // A state.json that EXISTS but can't be trusted must hold the dir: no mux, no
    // partial-output delete, state.json left for the operator, an error surfaced.
    fn assert_held(state_bytes: &[u8], tag: &str) {
        let _guard = crate::server::log::env_guard();
        // Then the mover/muxer statics lock: this asserts on MUX_ERRORS and STATE.
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = tmpdir();
        // SAFETY: env access in tests, serialized by env_guard.
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }
        let staging = d.join("Show_S01D1");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join(staging::STATE_FILE), state_bytes).unwrap();
        let partial = staging.join("Show_S01D1.mkv");
        std::fs::write(&partial, b"partial").unwrap();

        let dev = format!("test_resume_unreadable_{tag}_{}", std::process::id());
        let class = ResumeClass::Remux {
            iso_path: staging.join("Show_S01D1.iso"),
            mapfile_path: staging.join("Show_S01D1.iso.mapfile"),
            display_name: "Show_S01D1".to_string(),
            title_confident: None,
        };
        let cfg = Arc::new(RwLock::new(Config::default()));
        resume_remux(&cfg, &dev, class);

        let live = crate::server::log::get_device_log(&dev, 200);
        assert!(
            !live.iter().any(|l| l.contains("Auto-resume: re-muxing")),
            "an unreadable plan must not proceed to the mux, got: {live:?}"
        );
        assert!(
            live.iter().any(|l| l.contains("Auto-resume held")),
            "the hold must be explained in the device log, got: {live:?}"
        );
        assert!(partial.exists(), "held dir must be left untouched");
        assert_eq!(
            std::fs::read(staging.join(staging::STATE_FILE)).unwrap(),
            state_bytes,
            "the unreadable state.json must be preserved for the operator"
        );
        let rs = crate::server::ripper::STATE.lock().unwrap().remove(&dev);
        let rs = rs.expect("device state must be set");
        assert_eq!(rs.status, "error");
        assert!(rs.last_error.contains("state.json"), "{}", rs.last_error);
        let path = staging.to_string_lossy().to_string();
        let card = crate::server::muxer::MUX_ERRORS
            .lock()
            .unwrap()
            .get(&path)
            .cloned();
        let card = card.expect("an operator error card must be raised for the held dir");
        // A parse/schema failure is not transient: point at repair, and warn
        // that deleting state.json delivers a TV disc as one title.
        assert!(card.hint.contains("ONE title"), "hint: {}", card.hint);
        assert!(!card.hint.contains("retry"), "hint: {}", card.hint);

        // Repaired: the next resume clears the held card (then fails on the
        // missing ISO, which is fine here).
        staging::write_state(
            &staging,
            &staging::DiscState::new(staging::StagingState::Ripped),
        );
        resume_remux(
            &cfg,
            &dev,
            ResumeClass::Remux {
                iso_path: staging.join("Show_S01D1.iso"),
                mapfile_path: staging.join("Show_S01D1.iso.mapfile"),
                display_name: "Show_S01D1".to_string(),
                title_confident: None,
            },
        );
        crate::server::ripper::STATE.lock().unwrap().remove(&dev);
        assert!(
            !crate::server::muxer::MUX_ERRORS
                .lock()
                .unwrap()
                .contains_key(&path),
            "a readable state.json must clear the held card"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_state_json_holds_instead_of_delivering() {
        assert_held(b"{ this is not json", "corrupt");
    }

    #[test]
    fn foreign_schema_state_json_holds_instead_of_delivering() {
        assert_held(br#"{"schema": 1, "state": "ripped"}"#, "foreign");
    }

    // A transient read error (EIO/ESTALE) is not corruption: its hint must say
    // retry, never "delete state.json".
    #[test]
    fn transient_read_error_hint_says_retry_not_delete() {
        let io = staging::StateUnreadable::io(&std::io::Error::other("Stale file handle"));
        assert!(io.transient);
        assert!(io.hint().contains("retry"), "{}", io.hint());
        assert!(!io.hint().contains("delete"), "{}", io.hint());
        let bad = staging::StateUnreadable::invalid("state.json is unparseable".into());
        assert!(!bad.transient);
        assert!(bad.hint().contains("ONE title"), "{}", bad.hint());
    }
}

#[cfg(test)]
mod resume_remux_scan_gate_tests {
    //! Drive `resume_remux` through the real `libfreemkv::scan_iso` seam with a
    //! synthetic minimal UDF image (an empty root directory, so `scan_iso`
    //! SUCCEEDS but the disc carries zero titles). That exercises the
    //! title-gate abort branch — `scan_iso` ok, `title_ok == false` — which no
    //! prior test reached (the archive test aborts one step earlier, on a
    //! non-openable ISO). The sector builders are ported verbatim from
    //! libfreemkv's `tests/scan_iso.rs` fixture.
    use super::*;
    use std::collections::BTreeMap;

    const SECTOR_SIZE: usize = 2048;

    fn make_avdp_sector(vds_lba: u32) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE];
        s[0..2].copy_from_slice(&2u16.to_le_bytes());
        s[16..20].copy_from_slice(&vds_lba.to_le_bytes());
        s[20..24].copy_from_slice(&(6u32 * SECTOR_SIZE as u32).to_le_bytes());
        s
    }
    fn make_pvd_sector(volume_id: &str) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE];
        s[0..2].copy_from_slice(&1u16.to_le_bytes());
        if !volume_id.is_empty() {
            let id_bytes = volume_id.as_bytes();
            s[24] = 8;
            let copy_len = id_bytes.len().min(30);
            s[25..25 + copy_len].copy_from_slice(&id_bytes[..copy_len]);
            s[55] = (1 + copy_len) as u8;
        }
        s
    }
    fn make_partition_desc(partition_start: u32) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE];
        s[0..2].copy_from_slice(&5u16.to_le_bytes());
        s[188..192].copy_from_slice(&partition_start.to_le_bytes());
        s
    }
    fn make_lvd_sector_simple() -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE];
        s[0..2].copy_from_slice(&6u16.to_le_bytes());
        s[268..272].copy_from_slice(&1u32.to_le_bytes());
        s
    }
    fn make_terminator() -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE];
        s[0..2].copy_from_slice(&8u16.to_le_bytes());
        s
    }
    fn make_fsd_sector(root_meta_lba: u32) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE];
        s[0..2].copy_from_slice(&256u16.to_le_bytes());
        s[400..404].copy_from_slice(&(SECTOR_SIZE as u32).to_le_bytes());
        s[404..408].copy_from_slice(&root_meta_lba.to_le_bytes());
        s
    }
    fn make_dir_icb(data_meta_lba: u32, data_len: u32) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR_SIZE];
        s[0..2].copy_from_slice(&266u16.to_le_bytes());
        s[56..64].copy_from_slice(&(data_len as u64).to_le_bytes());
        s[208..212].copy_from_slice(&0u32.to_le_bytes());
        s[212..216].copy_from_slice(&8u32.to_le_bytes());
        s[216..220].copy_from_slice(&data_len.to_le_bytes());
        s[220..224].copy_from_slice(&data_meta_lba.to_le_bytes());
        s
    }
    fn make_parent_fid() -> Vec<u8> {
        let fid_len = (38 + 3) & !3;
        let mut fid = vec![0u8; fid_len];
        fid[0..2].copy_from_slice(&257u16.to_le_bytes());
        fid[18] = 0x08;
        fid[19] = 0;
        fid
    }
    fn minimal_udf_sectors() -> BTreeMap<u32, Vec<u8>> {
        let partition_start: u32 = 512;
        let mut sectors: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        sectors.insert(256, make_avdp_sector(32));
        sectors.insert(32, make_pvd_sector("TEST_DISC"));
        sectors.insert(33, make_partition_desc(partition_start));
        sectors.insert(34, make_lvd_sector_simple());
        sectors.insert(35, make_terminator());
        sectors.insert(partition_start, make_fsd_sector(1));
        let parent_fid = make_parent_fid();
        let dir_data_len = parent_fid.len() as u32;
        sectors.insert(partition_start + 1, make_dir_icb(2, dir_data_len));
        let mut sector = vec![0u8; SECTOR_SIZE];
        sector[..parent_fid.len()].copy_from_slice(&parent_fid);
        sectors.insert(partition_start + 2, sector);
        sectors
    }
    fn write_iso(path: &std::path::Path, sectors: &BTreeMap<u32, Vec<u8>>) {
        let max_lba = *sectors.keys().max().unwrap();
        let mut image = vec![0u8; (max_lba as usize + 1) * SECTOR_SIZE];
        for (&lba, data) in sectors {
            let off = lba as usize * SECTOR_SIZE;
            image[off..off + SECTOR_SIZE].copy_from_slice(data);
        }
        std::fs::write(path, &image).expect("write iso");
    }

    fn tmpdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!(
                "autorip-resume-scangate-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
            ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("logs")).unwrap();
        p
    }

    // A minimal UDF ISO scans cleanly but carries no titles: resume_remux
    // must log the "no usable title" abort and reset scanning -> idle so
    // handle_rip's "already ripping" gate doesn't wedge later /api/rip.
    #[test]
    fn resume_remux_aborts_when_scanned_iso_has_no_usable_title() {
        let _guard = crate::server::log::env_guard();
        let d = tmpdir();
        // SAFETY: env access in tests, serialized by env_guard; the assertions
        // read the in-memory STATE + device-log ring, not the env-routed file.
        unsafe {
            std::env::set_var("AUTORIP_DIR", &d);
        }

        let staging = d.join("Some_Disc_2024");
        std::fs::create_dir_all(&staging).unwrap();
        let iso_path = staging.join("Some_Disc_2024.iso");
        write_iso(&iso_path, &minimal_udf_sectors());

        let dev = format!("test_resume_scangate_sg_{}", std::process::id());
        // The live dispatch would already have moved this device to "scanning";
        // seed that so the abort's scanning→idle reset is observable.
        crate::server::ripper::update_state(
            &dev,
            crate::server::ripper::RipState {
                device: dev.clone(),
                status: "scanning".to_string(),
                disc_present: true,
                ..Default::default()
            },
        );

        let class = ResumeClass::Remux {
            iso_path: iso_path.clone(),
            mapfile_path: staging.join("Some_Disc_2024.iso.mapfile"),
            display_name: "Some_Disc_2024".to_string(),
            title_confident: None,
        };
        let cfg = Arc::new(RwLock::new(Config::default()));

        resume_remux(&cfg, &dev, class);

        let live = crate::server::log::get_device_log(&dev, 200);
        assert!(
            live.iter().any(|l| l.contains("no usable title")),
            "the title-gate abort line must be logged, got: {live:?}"
        );
        let status = crate::server::ripper::STATE
            .lock()
            .unwrap()
            .get(&dev)
            .map(|s| s.status.clone());
        assert_eq!(
            status.as_deref(),
            Some("idle"),
            "a title-gate abort must reset scanning → idle so the rip gate unwedges"
        );

        crate::server::ripper::STATE.lock().unwrap().remove(&dev);
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod resume_remux_webhook_tests {
    // Regression: resume_remux's success path must fire the completion webhook like rip_disc
    // does (both cold auto-resume and the _mux hand-off go through it). Pinned at source level.
    #[test]
    fn success_path_fires_completion_webhook() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        let start = src
            .find("Auto-resume complete")
            .expect("resume.rs should log \"Auto-resume complete\" on the success path");
        // Bound the search to the success region: from the completion log
        // line up to the auto_eject comment that immediately follows it.
        let end = src[start..]
            .find("Honor auto_eject after a successful resume")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let region = &src[start..end];
        assert!(
            region.contains("crate::server::webhook::send_rich"),
            "resume_remux success path must fire send_rich (the mux_complete \
             webhook), matching rip_disc; none found between \"Auto-resume \
             complete\" and the auto_eject branch"
        );
        assert!(
            region.contains("event: \"mux_complete\""),
            "the resume completion webhook is the mux stage, so it must use the \
             mux_complete event name (the drive-free rip_complete fires earlier, \
             on the sweep worker)"
        );
    }
}

#[cfg(test)]
mod resume_iso_auto_eject_tests {
    // Regression: resume_remux's ISO-output success path must honor auto_eject like the MKV
    // terminal does (pre-fix it returned without ejecting). Pinned at source level.
    #[test]
    fn resume_iso_success_path_honors_auto_eject() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        let start = src
            .find("Auto-resume: ISO output complete")
            .expect("resume.rs should log \"Auto-resume: ISO output complete\" on the ISO path");
        // Bound the search to the ISO success region: from the ISO
        // completion log line up to the run_mux call that opens the MKV
        // path (the next distinct terminal in the function).
        let end = src[start..]
            .find("super::mux::mux_iso")
            .map(|i| start + i)
            .expect("resume.rs should call mux_iso after the ISO branch");
        let region = &src[start..end];
        assert!(
            region.contains("should_auto_eject(cfg_read.auto_eject, device)"),
            "resume_remux ISO success path must gate eject through \
             should_auto_eject(cfg_read.auto_eject, device) — which encodes \
             both \"only when enabled\" and \"never for a synthetic _mux \
             device\" — matching the MKV terminal and the fresh-rip ISO \
             terminal; none found between the ISO completion log and run_mux"
        );
        assert!(
            region.contains("super::eject_drive"),
            "the ISO auto_eject branch must call super::eject_drive"
        );
    }

    // The MKV resume terminal's eject must likewise route through the
    // shared should_auto_eject predicate, not a bare auto_eject check
    // that would let the _mux worker re-eject the physical drive.
    #[test]
    fn resume_mkv_terminal_gates_eject_through_predicate() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        let start = src
            .find("Honor auto_eject after a successful resume")
            .expect("resume.rs should have the MKV terminal auto_eject comment");
        let region = &src[start..(start + 1000).min(src.len())];
        assert!(
            region.contains("should_auto_eject(cfg_read.auto_eject, device)"),
            "the MKV resume terminal must gate eject through should_auto_eject"
        );
    }
}

#[cfg(test)]
mod resume_handoff_contract_tests {
    //! The resume mux path (`remux_from_ripped_marker` -> `resume_remux`)
    //! must honor the SAME done + eject + queue-membership contract as the
    //! fresh-rip path: eject routes through `should_auto_eject`, and on
    //! success the dir writes `.done`/`.review` + `.completed` and deletes
    //! `.ripped`, landing in the Move queue only. These tests pin the
    //! marker-state outcomes without standing up a real ISO + mux pipeline.
    use crate::server::ripper::staging;
    use tempfile::TempDir;

    // On a CONFIDENT resume success the dir holds .done + .completed
    // (and .ripped deleted). pending_queue must skip it (Move queue only).
    #[test]
    fn resume_success_marker_state_is_move_queue_only() {
        let tmp = TempDir::new().unwrap();
        let disc = tmp.path().join("Resumed_Title");
        std::fs::create_dir_all(&disc).unwrap();
        // Post-resume-success marker state (the .ripped delete succeeded).
        std::fs::write(disc.join(staging::DONE_MARKER), b"{}").unwrap();
        staging::write_completed_marker(&disc);

        // Mux queue: must NOT contain it (no .ripped, and .done/.completed
        // are terminal/move-queue markers anyway).
        let mux = crate::server::muxer::pending_queue(tmp.path());
        assert!(
            mux.is_empty(),
            "a resumed-and-completed dir must not be (queued) for mux"
        );

        // The snapshot reports completed → the mux worker won't re-dispatch.
        let snap = staging::snapshot_staging_disc(&disc).expect("populated dir yields snapshot");
        assert!(
            snap.completed,
            "resume success must leave .completed for the mover"
        );
    }

    // If the post-resume .ripped delete FAILS (NFS), the dir still holds
    // .ripped + .done + .completed; it must STILL be Move-queue only and
    // the mux worker's dispatch verdict must be terminal (no re-mux).
    #[test]
    fn resume_success_with_lingering_ripped_is_still_move_only() {
        let tmp = TempDir::new().unwrap();
        let disc = tmp.path().join("Resumed_Title");
        std::fs::create_dir_all(&disc).unwrap();
        crate::server::muxer::write_marker(
            &disc,
            &crate::server::muxer::RippedMarker {
                schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
                iso_path: "/x/Resumed_Title/Resumed_Title.iso".into(),
                mapfile_path: "/x/Resumed_Title/Resumed_Title.iso.mapfile".into(),
                display_name: "Resumed Title".into(),
                disc_format: "uhd".into(),
                mkv_filename: "Resumed_Title.mkv".into(),
                tmdb_title: "Resumed Title".into(),
                tmdb_year: 2024,
                tmdb_poster: String::new(),
                tmdb_overview: String::new(),
                tmdb_media_type: "movie".into(),
                max_retries: 5,
                abort_on_lost_secs: 0,
                rip_elapsed_secs: 0.0,
                rip_errors: 0,
                rip_lost_video_secs: 0.0,
                rip_last_sector: 0,
                origin_device: "sg0".into(),
                sweep_errors: 0,
                sweep_total_lost_ms: 0.0,
                sweep_main_lost_ms: 0.0,
                sweep_num_bad_ranges: 0,
                sweep_largest_gap_ms: 0.0,
                title_confident: true,
            },
        )
        .unwrap();
        std::fs::write(disc.join(staging::DONE_MARKER), b"{}").unwrap();
        staging::write_completed_marker(&disc);

        let mux = crate::server::muxer::pending_queue(tmp.path());
        assert!(
            mux.is_empty(),
            "a completed resume must be Move-queue only even if .ripped lingers, got {mux:?}"
        );
        let snap = staging::snapshot_staging_disc(&disc).expect("snapshot");
        assert_eq!(
            crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
            crate::server::muxer::MuxVerdict::SkipTerminal,
            "the mux worker must treat a completed dir as terminal, never re-dispatch"
        );
    }

    /// A LOW-CONFIDENCE resume success writes `.review` (not `.done`) +
    /// `.completed`. Same mutual-exclusion outcome: never in the Mux queue.
    #[test]
    fn resume_review_success_is_not_in_mux_queue() {
        let tmp = TempDir::new().unwrap();
        let disc = tmp.path().join("Held_Resume");
        std::fs::create_dir_all(&disc).unwrap();
        std::fs::write(disc.join(staging::REVIEW_MARKER), b"{}").unwrap();
        staging::write_completed_marker(&disc);

        let mux = crate::server::muxer::pending_queue(tmp.path());
        assert!(
            mux.is_empty(),
            "a .review+.completed resume must not be (queued) for mux"
        );
    }
}

#[cfg(test)]
// These tests pin the post-mux loss REPORTING contract at source level: a
// resume folds mux-time (demux/decrypt) loss into the operator-facing figures,
// the PRE-mux threshold.)
mod post_mux_loss_reporting_tests {
    // Regression: a resume must report sweep loss + demux loss to the operator, not the sweep
    // mapfile alone (previously demux-time loss was invisible). Pinned at source level.
    #[test]
    fn resume_reports_demux_loss_on_accepted_rip() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        // Bound to the accepted-success region: from the combined-loss
        // (sweep + demux) computation up to the auto-eject tail.
        let start = src
            .find("Operator-facing loss for a resume")
            .expect("resume.rs should compute combined resume loss");
        let end = src[start..]
            .find("Honor auto_eject after a successful resume")
            .map(|i| start + i)
            .expect("resume.rs should have the auto_eject tail after success");
        let region = &src[start..end];

        // The combined figures must be derived from BOTH sweep damage and the
        // demux loss signal.
        assert!(
            region.contains("done_sweep_damage.errors.saturating_add(mux_outcome.errors)"),
            "accepted resume must add demux errors to sweep errors"
        );
        assert!(
            region.contains(
                "done_sweep_damage.main_lost_ms / crate::server::util::MILLIS_PER_SEC + demux_lost_secs"
            ),
            "accepted resume must add demux lost seconds to sweep main loss"
        );
        // Both the done card and the webhook must consume the combined figures,
        // not the sweep-only fields.
        assert!(
            region.contains("errors: done_errors"),
            "done card / webhook must report combined errors (done_errors)"
        );
        assert!(
            region.contains("lost_video_secs: done_lost_video_secs"),
            "done card / webhook must report combined loss (done_lost_video_secs)"
        );
        // Guard against regressing to the sweep-only webhook figures.
        assert!(
            !region.contains(
                "lost_video_secs: done_sweep_damage.main_lost_ms / crate::server::util::MILLIS_PER_SEC"
            ),
            "webhook must not report sweep-only loss, hiding demux loss"
        );
    }

    // Regression: mux-incomplete must route through incomplete_mux_status
    // (mirroring rip_disc) so a mid-mux error surfaces instead of a
    // silent idle/None verdict indistinguishable from /api/stop.
    #[test]
    fn resume_incomplete_mux_surfaces_read_error_not_silent_idle() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        // Bound to the mux-incomplete early-return: from the guard up to the
        // "A loss is a loss" mux-time-loss note that immediately follows it. The
        // new anchor brackets just the ~35-line guard block.
        let start = src
            .find("if !mux_outcome.output_opened || !mux_outcome.completed {")
            .expect("resume.rs should have the mux-incomplete guard");
        let end = src[start..]
            .find("A loss is a loss. Mux-time")
            .map(|i| start + i)
            .expect("resume.rs should have the mux-time-loss note after the guard");
        let region = &src[start..end];

        assert!(
            region.contains("super::incomplete_mux_status("),
            "resume mux-incomplete branch must route through incomplete_mux_status, \
             matching rip_disc, so finalize/read-error causes surface"
        );
        assert!(
            region.contains("mux_outcome.read_error.as_deref()"),
            "resume mux-incomplete branch must pass mux_outcome.read_error so a \
             mid-mux drive/ISO read error surfaces as status=error with the cause"
        );
        assert!(
            region.contains("mux_outcome.finalize_error.as_deref()"),
            "resume mux-incomplete branch must pass mux_outcome.finalize_error so a \
             structural mux finalize failure surfaces as status=failed"
        );
        // Guard against regressing to the silent idle/None verdict that
        // discarded the cause and looked like a clean /api/stop.
        assert!(
            !region.contains(
                "reset_status_after_ripping(device, \"idle\", &display_name, \
                 &disc_format, &duration, None)"
            ),
            "resume mux-incomplete branch must not hardcode idle/None, hiding the \
             read-error cause and aliasing a failure to a clean stop"
        );
    }

    // FIX 1 — behavioural coverage the source-substring test above lacks:
    // drives quarantine_incomplete_mux's two real arms (finalize_error ->
    // terminal Failed/SkipTerminal; read_error -> stays resumable/Dispatch).
    #[test]
    fn incomplete_mux_finalize_quarantines_read_error_stays_resumable() {
        use crate::server::muxer::{MuxVerdict, mux_dispatch_verdict};
        use crate::server::ripper::staging::{
            self, DiscState, StagingState, snapshot_staging_disc,
        };

        let tmp = tempfile::TempDir::new().unwrap();

        // Arm 1: a structural finalize failure quarantines to terminal.
        let finalize_dir = tmp.path().join("Finalize_Fail");
        std::fs::create_dir_all(&finalize_dir).unwrap();
        staging::write_state(&finalize_dir, &DiscState::new(StagingState::Ripped));
        assert_eq!(
            mux_dispatch_verdict(snapshot_staging_disc(&finalize_dir).as_ref()),
            MuxVerdict::Dispatch,
            "a fresh Ripped hand-off must dispatch before the quarantine"
        );
        let quarantined =
            super::quarantine_incomplete_mux(&finalize_dir, Some("mux produced no frames (E6008)"));
        assert!(
            quarantined,
            "a finalize_error must report a terminal quarantine"
        );
        assert_eq!(
            snapshot_staging_disc(&finalize_dir)
                .and_then(|_| staging::read_state(&finalize_dir))
                .map(|s| s.state),
            Some(StagingState::Failed),
            "a finalize_error must transition state → Failed"
        );
        assert_eq!(
            mux_dispatch_verdict(snapshot_staging_disc(&finalize_dir).as_ref()),
            MuxVerdict::SkipTerminal,
            "after the finalize quarantine the dir must never re-dispatch"
        );

        // Arm 2: a mid-mux read error (finalize_error == None) stays resumable.
        let read_dir = tmp.path().join("Read_Error");
        std::fs::create_dir_all(&read_dir).unwrap();
        staging::write_state(&read_dir, &DiscState::new(StagingState::Ripped));
        let quarantined = super::quarantine_incomplete_mux(&read_dir, None);
        assert!(!quarantined, "a read_error must NOT quarantine");
        assert_eq!(
            staging::read_state(&read_dir).map(|s| s.state),
            Some(StagingState::Ripped),
            "a read_error must leave the dir in the resumable Ripped state"
        );
        assert_eq!(
            mux_dispatch_verdict(snapshot_staging_disc(&read_dir).as_ref()),
            MuxVerdict::Dispatch,
            "a read_error dir must stay re-muxable (Dispatch), not be quarantined"
        );
    }

    // quarantine_incomplete_mux returns whether the terminal write actually LANDED, not
    // merely whether the failure was a finalize — an unwritable mount must return false.
    #[test]
    fn quarantine_incomplete_mux_returns_false_when_write_dropped() {
        use crate::server::ripper::staging::{self, StagingState};

        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("Unwritable");
        std::fs::create_dir_all(&dir).unwrap();
        // Force the terminal state.json write to fail (a dir can't be renamed over).
        std::fs::create_dir_all(dir.join(staging::STATE_FILE)).unwrap();

        let landed = super::quarantine_incomplete_mux(&dir, Some("mux produced no frames (E6008)"));
        assert!(
            !landed,
            "a finalize failure whose terminal write was DROPPED must return false"
        );
        // And the dir did NOT reach the terminal Failed state on disk.
        assert_ne!(
            staging::read_state(&dir).map(|s| s.state),
            Some(StagingState::Failed),
            "a dropped write must not leave a persisted terminal state"
        );
    }

    // production wiring end to end: RipState.failure_finalize must thread through
    // apply_failure_fields to the worker's terminal gate and a persisted Failed state.
    #[test]
    fn finalize_finalize_threads_ripstate_to_worker_gate_and_persists_failed() {
        use crate::server::muxer::{MuxFailureClass, mux_failure_is_terminal};
        use crate::server::muxer::{MuxVerdict, mux_dispatch_verdict};
        use crate::server::ripper::staging::{
            self, DiscState, StagingState, snapshot_staging_disc,
        };

        // 1. A terminal finalize failure as `resume_remux` records it on the `_mux`
        //    RipState: a real error string + the structural-finalize bit.
        let rs = crate::server::ripper::RipState {
            last_error: "mux finalize failed: E6008 no muxable frames".to_string(),
            failure_finalize: true,
            failure_deferred: false,
            ..crate::server::ripper::RipState::default()
        };

        // 2. The handoff builder threads it into the outcome the worker consumes.
        let mut outcome = super::MuxHandoffOutcome::default();
        super::apply_failure_fields(&mut outcome, &rs);
        assert!(
            outcome.failure_finalize,
            "the finalize bit must thread RipState → MuxHandoffOutcome (the reverted FIX)"
        );
        assert_eq!(
            outcome.failure_reason.as_deref(),
            Some("mux finalize failed: E6008 no muxable frames")
        );

        // 3. The worker's terminal gate, fed from that outcome, says TERMINAL.
        assert!(
            mux_failure_is_terminal(MuxFailureClass {
                aborted_loss: false,
                has_worker_reason: outcome.failure_reason.is_some(),
                is_finalize: outcome.failure_finalize,
            }),
            "a threaded finalize failure must drive the worker gate to quarantine"
        );

        // 4. End to end: the transition the gate authorises persists Failed and the
        //    next dispatch verdict is SkipTerminal — the dir never re-muxes.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("Finalize_E2E");
        std::fs::create_dir_all(&dir).unwrap();
        staging::write_state(&dir, &DiscState::new(StagingState::Ripped));
        assert_eq!(
            mux_dispatch_verdict(snapshot_staging_disc(&dir).as_ref()),
            MuxVerdict::Dispatch,
            "a fresh Ripped hand-off dispatches before the quarantine"
        );
        assert!(crate::server::muxer::persist_terminal_mux_quarantine(
            &dir.to_string_lossy(),
            &dir,
            outcome.failure_reason.as_deref().unwrap(),
        ));
        assert_eq!(
            snapshot_staging_disc(&dir)
                .and_then(|_| staging::read_state(&dir))
                .map(|s| s.state),
            Some(StagingState::Failed),
            "the threaded terminal finalize must persist state → Failed"
        );
        assert_eq!(
            mux_dispatch_verdict(snapshot_staging_disc(&dir).as_ref()),
            MuxVerdict::SkipTerminal,
            "after the quarantine the dir must never re-dispatch"
        );
    }

    // the sweep-loss abort path must quarantine to a resumable.aborted-loss like the
    // mux-time loss gate does (else the worker re-dispatches the doomed dir forever).
    #[test]
    fn sweep_loss_abort_quarantines_to_resumable_aborted_loss() {
        // (a) Source-level: the §3 sweep-loss abort block quarantines via the marker.
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        let start = src
            .find("\"disc loss\" for raw ISO (whole-disc scope)")
            .expect("resume.rs should have the §3 sweep-loss scope note");
        let end = src[start..]
            .find("4. Build MuxInputs + run mux")
            .map(|i| start + i)
            .expect("resume.rs should have the mux-build step after the §3 gate");
        let region = &src[start..end];
        // Assert on the CALL form (`staging::mark_aborted_on_loss(`), not the bare
        // identifier — a prose mention of the symbol in a nearby comment must not
        // satisfy this (the vacuous-substring trap).
        assert!(
            region.contains("staging::mark_aborted_on_loss_reporting_landed("),
            "the §3 sweep-loss abort must quarantine to a resumable .aborted-loss (mirror §4), \
             else the worker re-dispatches the doomed dir forever"
        );

        // (b) Behavioural: the marker the §3 path now writes flips the worker's
        //     dispatch verdict to SkipAbortedLoss, stopping the re-dispatch loop.
        use crate::server::muxer::{MuxVerdict, mux_dispatch_verdict};
        use crate::server::ripper::staging::{
            self, DiscState, StagingState, snapshot_staging_disc,
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("Sweep_Loss");
        std::fs::create_dir_all(&dir).unwrap();
        staging::write_state(&dir, &DiscState::new(StagingState::Ripped));
        assert_eq!(
            mux_dispatch_verdict(snapshot_staging_disc(&dir).as_ref()),
            MuxVerdict::Dispatch,
            "a fresh Ripped hand-off dispatches before the sweep-loss quarantine"
        );
        let _ = staging::mark_aborted_on_loss(
            &dir,
            "aborted: disc loss 12.50s exceeds threshold 0s (sweep)",
        );
        assert_eq!(
            mux_dispatch_verdict(snapshot_staging_disc(&dir).as_ref()),
            MuxVerdict::SkipAbortedLoss,
            "after the sweep-loss quarantine the dir must not re-dispatch (SkipAbortedLoss)"
        );
    }

    // v1.2.0 invariant ("a loss is a loss"): a COMPLETED mux carrying mux-time loss is gated on
    // abort_on_lost_secs just like read-time loss, reported always, never silently dropped.
    #[test]
    fn completed_mux_with_loss_gated_by_abort_on_lost_secs() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        // The completed-mux success region: from the "A loss is a loss" mux-time-
        // loss note through the auto-eject tail.
        let start = src
            .find("A loss is a loss. Mux-time")
            .expect("resume.rs should have the mux-time-loss note");
        let end = src[start..]
            .find("Honor auto_eject after a successful resume")
            .map(|i| start + i)
            .expect("resume.rs should have the auto_eject tail after success");
        let region = &src[start..end];

        // (a) The loss is still REPORTED, never silently dropped.
        assert!(
            region.contains("demux_lost_secs"),
            "completed-mux success region must report demux-time loss"
        );
        // (b) Within threshold it hands off to Done/Review, routed through the
        // shared `handoff_label` title-confidence policy (staging) so this
        // completion route can't drift from the fresh-rip one.
        assert!(
            region.contains("handoff_label(title_confident)"),
            "completed mux must hand off to .done (confident) or .review (not)"
        );
        assert!(
            region.contains("mark_handoff("),
            "completed mux must write the unified hand-off state so the mover/operator picks it up"
        );
        // (c) A loss is a loss: mux-time loss OVER abort_on_lost_secs quarantines
        //     to a RESUMABLE .aborted-loss, gated on the threshold.
        assert!(
            region.contains("mark_aborted_on_loss"),
            "mux-time loss over threshold must quarantine to a resumable .aborted-loss"
        );
        assert!(
            region.contains("abort_on_lost_secs") || region.contains("effective_abort"),
            "the mux-time loss gate must consult the abort_on_lost_secs threshold"
        );
    }
}

#[cfg(test)]
mod sweep_damage_marker_tests {
    // Regression: resume_remux previously passed a zeroed
    // SweepDamageSnapshot; RippedMarker now carries sweep_* fields.
    // Verifies the round-trip serialization of those fields.
    #[test]
    fn ripped_marker_sweep_fields_round_trip() {
        let marker = crate::server::muxer::RippedMarker {
            schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
            iso_path: "/staging/Foo/Foo.iso".into(),
            mapfile_path: "/staging/Foo/Foo.iso.mapfile".into(),
            display_name: "Foo".into(),
            disc_format: "uhd".into(),
            mkv_filename: "Foo.mkv".into(),
            tmdb_title: "Foo".into(),
            tmdb_year: 2024,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            tmdb_media_type: String::new(),
            max_retries: 3,
            abort_on_lost_secs: 0,
            rip_elapsed_secs: 0.0,
            rip_errors: 0,
            rip_lost_video_secs: 1.23,
            rip_last_sector: 0,
            origin_device: "sg0".into(),
            sweep_errors: 77,
            sweep_total_lost_ms: 2500.0,
            sweep_main_lost_ms: 1200.0,
            sweep_num_bad_ranges: 5,
            sweep_largest_gap_ms: 900.0,
            title_confident: false,
        };

        // Serialize then deserialize (mirrors write_marker / read_marker).
        let json = serde_json::to_string(&marker).expect("serialize");
        let back: crate::server::muxer::RippedMarker =
            serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back.sweep_errors, 77);
        assert!((back.sweep_total_lost_ms - 2500.0).abs() < 0.001);
        assert!((back.sweep_main_lost_ms - 1200.0).abs() < 0.001);
        assert_eq!(back.sweep_num_bad_ranges, 5);
        assert!((back.sweep_largest_gap_ms - 900.0).abs() < 0.001);
    }

    /// Backward-compat: a marker JSON without sweep_* fields (pre-v0.25.12)
    /// must deserialize successfully with sweep_* defaulting to zero.
    #[test]
    fn ripped_marker_missing_sweep_fields_default_to_zero() {
        // JSON without any sweep_* keys — simulates an old marker on disk.
        let json = r#"{
            "schema_version": 1,
            "iso_path": "/staging/Bar/Bar.iso",
            "mapfile_path": "/staging/Bar/Bar.iso.mapfile",
            "display_name": "Bar",
            "disc_format": "bluray",
            "mkv_filename": "Bar.mkv",
            "tmdb_title": "Bar",
            "tmdb_year": 2020,
            "tmdb_poster": "",
            "tmdb_overview": "",
            "max_retries": 5,
            "abort_on_lost_secs": 30,
            "rip_elapsed_secs": 0.0,
            "rip_errors": 0,
            "rip_lost_video_secs": 0.0,
            "rip_last_sector": 0,
            "origin_device": "sg0"
        }"#;
        let marker: crate::server::muxer::RippedMarker =
            serde_json::from_str(json).expect("old marker must deserialize");
        // schema_version check is done by read_marker, not serde; skip it here.
        assert_eq!(marker.sweep_errors, 0, "missing field must default to 0");
        assert_eq!(
            marker.sweep_total_lost_ms, 0.0,
            "missing field must default to 0.0"
        );
        assert_eq!(
            marker.sweep_main_lost_ms, 0.0,
            "missing field must default to 0.0"
        );
        assert_eq!(
            marker.sweep_num_bad_ranges, 0,
            "missing field must default to 0"
        );
        assert_eq!(
            marker.sweep_largest_gap_ms, 0.0,
            "missing field must default to 0.0"
        );
        assert!(
            !marker.title_confident,
            "missing title_confident must default to false (prior match-check-only behavior)"
        );
    }

    // Regression: an operator title override must survive the.ripped hand-off so resume_remux
    // auto-files into.done. Before the fix RippedMarker didn't carry the verdict.
    #[test]
    fn ripped_marker_title_confident_round_trips() {
        let mut marker = crate::server::muxer::RippedMarker {
            schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
            iso_path: "/staging/Baz/Baz.iso".into(),
            mapfile_path: "/staging/Baz/Baz.iso.mapfile".into(),
            display_name: "Operator Chosen Title".into(),
            disc_format: "uhd".into(),
            mkv_filename: "Operator_Chosen_Title.mkv".into(),
            tmdb_title: "Operator Chosen Title".into(),
            tmdb_year: 2024,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            tmdb_media_type: String::new(),
            max_retries: 3,
            abort_on_lost_secs: 0,
            rip_elapsed_secs: 0.0,
            rip_errors: 0,
            rip_lost_video_secs: 0.0,
            rip_last_sector: 0,
            origin_device: "sg0".into(),
            sweep_errors: 0,
            sweep_total_lost_ms: 0.0,
            sweep_main_lost_ms: 0.0,
            sweep_num_bad_ranges: 0,
            sweep_largest_gap_ms: 0.0,
            title_confident: true,
        };
        let json = serde_json::to_string(&marker).expect("serialize");
        let back: crate::server::muxer::RippedMarker =
            serde_json::from_str(&json).expect("deserialize");
        assert!(
            back.title_confident,
            "operator-confident verdict must survive the .ripped hand-off"
        );

        // And the low-confidence case round-trips as false.
        marker.title_confident = false;
        let json = serde_json::to_string(&marker).expect("serialize");
        let back: crate::server::muxer::RippedMarker =
            serde_json::from_str(&json).expect("deserialize");
        assert!(!back.title_confident);
    }

    // Regression: the resumed-rip done card must carry codecs, preferring
    // the post-mux STATE value over the (possibly empty) pre-mux
    // snapshot either way the done card must not be blank.
    #[test]
    fn resolve_done_codecs_prefers_post_mux_then_snapshot() {
        // _mux path: pre-mux snapshot empty, post-mux STATE has real codecs.
        assert_eq!(
            super::resolve_done_codecs(Some("HEVC · TrueHD".into()), String::new()),
            "HEVC · TrueHD",
            "post-mux codecs must win when present"
        );
        // User-triggered path: STATE empty post-mux, snapshot carries codecs.
        assert_eq!(
            super::resolve_done_codecs(Some(String::new()), "AVC · DTS".into()),
            "AVC · DTS",
            "empty post-mux STATE must fall back to the pre-mux snapshot"
        );
        // No STATE entry at all → snapshot.
        assert_eq!(
            super::resolve_done_codecs(None, "AVC · DTS".into()),
            "AVC · DTS",
            "absent STATE must fall back to the pre-mux snapshot"
        );
        // Both populated → post-mux is the fresher truth.
        assert_eq!(
            super::resolve_done_codecs(Some("HEVC".into()), "AVC".into()),
            "HEVC"
        );
    }

    // Regression: resume .done/.review markers omitted media_type, so
    // the mover filed TV-show resumes under the movie library. Now
    // resolve_media_type mirrors the mover's own default.
    #[test]
    fn resolve_media_type_defaults_empty_to_movie() {
        assert_eq!(
            super::resolve_media_type("tv"),
            "tv",
            "a carried TV media_type must survive into the marker, not collapse to movie"
        );
        assert_eq!(super::resolve_media_type("movie"), "movie");
        assert_eq!(
            super::resolve_media_type(""),
            "movie",
            "empty (cold resume) must resolve to the mover's own default"
        );
    }

    // Regression: check_and_mux's secondary done-state update was dropping the
    // codec/duration/output_file badges because remux_from_ripped_marker returned a bare bool.
    #[test]
    fn mux_handoff_outcome_captures_mux_derived_fields() {
        // A private device key so this doesn't race the shared "_mux".
        let key = "_mux_test_capture";
        let bad_ranges = vec![
            super::super::state::BadRange {
                lba: 100,
                count: 32,
                duration_ms: 1500.0,
                chapter: Some(2),
                time_offset_secs: Some(42.0),
            },
            super::super::state::BadRange {
                lba: 5000,
                count: 8,
                duration_ms: 375.0,
                chapter: None,
                time_offset_secs: None,
            },
        ];
        super::super::update_state(
            key,
            super::super::RipState {
                device: key.to_string(),
                status: "done".to_string(),
                codecs: "HEVC · TrueHD".into(),
                duration: "2:14".into(),
                output_file: "/staging/Foo".into(),
                bad_ranges: bad_ranges.clone(),
                bad_ranges_truncated: 3,
                // Combined sweep + mux-time loss the `_mux` done-state writes:
                // these must be captured so the origin device's done card
                // reports real loss in the delivered MKV, not the sweep-only subset.
                errors: 7,
                lost_video_secs: 12.5,
                total_lost_ms: 12500.0,
                main_lost_ms: 9000.0,
                ..Default::default()
            },
        );

        // Mirror the capture-then-remove block in remux_from_ripped_marker.
        let mut outcome = super::MuxHandoffOutcome {
            success: true,
            ..Default::default()
        };
        if let Ok(mut s) = super::super::STATE.lock() {
            if let Some(rs) = s.get(key) {
                outcome.codecs = rs.codecs.clone();
                outcome.duration = rs.duration.clone();
                outcome.output_file = rs.output_file.clone();
                outcome.bad_ranges = rs.bad_ranges.clone();
                outcome.bad_ranges_truncated = rs.bad_ranges_truncated;
                outcome.lost_video_secs = rs.lost_video_secs;
                outcome.errors = rs.errors;
                outcome.total_lost_ms = rs.total_lost_ms;
                outcome.main_lost_ms = rs.main_lost_ms;
            }
            s.remove(key);
        }

        assert_eq!(outcome.codecs, "HEVC · TrueHD");
        assert_eq!(outcome.duration, "2:14");
        assert_eq!(outcome.output_file, "/staging/Foo");
        // The bad-ranges drilldown list + truncation count must survive the
        // capture so the origin device's done card isn't left with an empty
        // drilldown for a damaged disc.
        assert_eq!(outcome.bad_ranges.len(), 2);
        assert_eq!(outcome.bad_ranges[0].lba, 100);
        assert_eq!(outcome.bad_ranges[1].count, 8);
        assert_eq!(outcome.bad_ranges_truncated, 3);
        // Combined sweep + mux-time loss figures must survive the capture so
        // the origin device's done card reports the loss in the delivered MKV
        // (matching the `_mux` tile/webhook), not the sweep-only marker subset.
        assert_eq!(outcome.errors, 7);
        assert_eq!(outcome.lost_video_secs, 12.5);
        assert_eq!(outcome.total_lost_ms, 12500.0);
        assert_eq!(outcome.main_lost_ms, 9000.0);
        // STATE entry is cleaned up so the origin update can't read it later.
        assert!(super::super::STATE.lock().unwrap().get(key).is_none());
    }
}

// Convergence round 4 (H1 + M4): the cold operator-resume mux path acquires the
// `.muxing` exclusion lock so a concurrent Wipe / second cold resume can't
// re-muxing the same possibly-corrupt output forever on the `_mux` worker loop.
#[cfg(test)]
mod resume_lock_and_fsync_tests {
    use super::*;
    use crate::server::ripper::staging;

    fn tmpdir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(format!(
                "autorip-resume-lock-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed),
            ));
        // See the note in the find-iso `tmpdir`: clear stale contents so a
        // reused scratch path (persistent dir + CI pid reuse) starts empty.
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // H1: a cold operator-resume must write .muxing for the duration of
    // the mux so a concurrent Wipe / second cold resume is blocked; the
    // guard clears the marker on drop.
    #[test]
    fn cold_resume_guard_writes_and_clears_muxing() {
        let d = tmpdir();
        // The muxing lock is a field set on the existing (Ripped) state, so seed
        // a Ripped state.json for the guard to attach the lock to.
        staging::write_state(&d, &staging::DiscState::new(staging::StagingState::Ripped));
        assert!(!staging::read_state(&d).map(|s| s.muxing).unwrap_or(false));
        {
            let _g = ResumeMuxingGuard::acquire("sg0", &d);
            assert!(
                staging::read_state(&d).expect("state").muxing,
                "cold-resume guard must set the muxing lock while the mux is in flight"
            );
            // While held, the snapshot the ownership/blocked checks consult
            // reports the dir as owned.
            let snap = staging::snapshot_staging_disc(&d).expect("snapshot");
            assert!(snap.has_muxing);
        }
        assert!(
            !staging::read_state(&d).expect("state").muxing,
            "the muxing lock must be cleared on guard drop (covers early-return / panic)"
        );
    }

    // H1: the _mux worker already holds the lock via its own MuxingGuard,
    // so resume_remux's guard must NOT touch the marker (a clear would
    // release the worker's exclusion mid-dispatch).
    #[test]
    fn worker_mux_device_does_not_double_manage_muxing() {
        let d = tmpdir();
        // Simulate the worker having set the muxing lock before dispatch. The
        // lock is a field on the Ripped state, so seed that state first.
        staging::write_state(&d, &staging::DiscState::new(staging::StagingState::Ripped));
        staging::write_muxing_marker(&d);
        assert!(staging::read_state(&d).expect("state").muxing);
        {
            let _g = ResumeMuxingGuard::acquire("_mux", &d);
            assert!(
                staging::read_state(&d).expect("state").muxing,
                "worker's lock stays put"
            );
        }
        assert!(
            staging::read_state(&d).expect("state").muxing,
            "the `_mux` guard must leave the worker's muxing lock intact on drop"
        );
    }

    /// M4: below `RESTART_LIMIT`, a fsync failure bumps `.restart_count` and
    /// preserves staging (no `.failed`) for the next retry.
    #[test]
    fn fsync_failure_below_limit_preserves_and_bumps() {
        let d = tmpdir();
        // Seed a `.ripped` so we can assert it survives below the limit.
        std::fs::write(d.join(".ripped"), b"{}").unwrap();
        let quarantined = handle_resume_fsync_failure("_mux", &d, "mux output");
        assert!(!quarantined, "first failure must not quarantine");
        assert_eq!(staging::restart_count(&d), 1);
        assert!(!d.join(".failed").exists(), "no .failed below the limit");
        assert!(d.join(".ripped").exists(), ".ripped preserved for retry");
    }

    /// M4: once `.restart_count` reaches `RESTART_LIMIT`, the repeated fsync
    /// failure promotes the dir to terminal `.failed`, drops `.ripped` so the
    /// worker can't re-queue it, and clears the counter.
    #[test]
    fn fsync_failure_at_limit_quarantines() {
        let d = tmpdir();
        std::fs::write(d.join(".ripped"), b"{}").unwrap();
        // Pre-seed the count to one below the limit so the next bump trips it.
        staging::write_marker_durable(
            &d.join(".restart_count"),
            format!("{}\n", staging::RESTART_LIMIT - 1).as_bytes(),
        )
        .unwrap();
        let quarantined = handle_resume_fsync_failure("_mux", &d, "mux output");
        assert!(quarantined, "reaching RESTART_LIMIT must quarantine");
        let snap = staging::snapshot_staging_disc(&d).expect("snapshot");
        assert!(snap.has_failed, "state Failed written (terminal)");
        assert!(
            !d.join(".ripped").exists(),
            ".ripped dropped so the worker can't re-queue the terminal dir"
        );
        assert!(
            !snap.has_ripped,
            "state is no longer Ripped so the worker can't re-queue the terminal dir"
        );
        assert_eq!(
            staging::restart_count(&d),
            0,
            ".restart_count cleared after quarantine"
        );
    }

    // FIX (fsync cap preservation): at RESTART_LIMIT, if the terminal.failed write does NOT
    // land, handle_resume_fsync_failure must NOT tear down the restart cap or report
    // quarantine.
    #[test]
    fn fsync_failure_at_limit_dropped_write_preserves_cap() {
        let d = tmpdir();
        std::fs::write(d.join(".ripped"), b"{}").unwrap();
        // Make the terminal state.json write fail: a directory can't be renamed over.
        std::fs::create_dir(d.join(staging::STATE_FILE)).unwrap();
        // Legacy counter one below the limit; the next bump trips it (state.json is
        // a dir, so `read_state` is None and the legacy `.restart_count` path runs).
        staging::write_marker_durable(
            &d.join(".restart_count"),
            format!("{}\n", staging::RESTART_LIMIT - 1).as_bytes(),
        )
        .unwrap();
        let quarantined = handle_resume_fsync_failure("_mux", &d, "mux output");
        assert!(
            !quarantined,
            "a dropped terminal write must NOT report a successful quarantine"
        );
        assert_eq!(
            staging::restart_count(&d),
            staging::RESTART_LIMIT,
            "the restart cap must be PRESERVED (not cleared) when the terminal write is dropped"
        );
        assert!(
            d.join(".ripped").exists(),
            ".ripped must stay so a dir that never went terminal isn't stranded"
        );
    }

    // OPERATOR-CARD PARITY: on the cold operator-resume path a dropped terminal write must
    // raise an operator card (record_error) the same way the muxer site does.
    #[test]
    fn fsync_dropped_write_raises_operator_card() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = tmpdir();
        std::fs::write(d.join(".ripped"), b"{}").unwrap();
        // Force the terminal state.json write to fail (a dir can't be renamed
        // over), same trick as `fsync_failure_at_limit_dropped_write_preserves_cap`.
        std::fs::create_dir(d.join(staging::STATE_FILE)).unwrap();
        staging::write_marker_durable(
            &d.join(".restart_count"),
            format!("{}\n", staging::RESTART_LIMIT - 1).as_bytes(),
        )
        .unwrap();
        let path_key = d.to_string_lossy().to_string();
        crate::server::muxer::clear_error(&path_key);

        // A REAL device (cold operator-resume), not the `"_mux"` worker.
        let quarantined = handle_resume_fsync_failure("sg0", &d, "mux output");

        assert!(
            !quarantined,
            "a dropped terminal write must still report NOT quarantined"
        );
        assert!(
            crate::server::muxer::MUX_ERRORS
                .lock()
                .unwrap()
                .contains_key(&path_key),
            "a dropped terminal write on the cold operator-resume path must raise \
             an operator card (MUX_ERRORS) — syslog/device_log alone are not \
             visible on the System page"
        );
        crate::server::muxer::clear_error(&path_key);
    }

    // Mirrors `fsync_dropped_write_raises_operator_card`: a dropped `.aborted-loss` write must
    // also raise an operator card, not just silently retry forever.
    #[test]
    fn loss_abort_dropped_write_raises_operator_card() {
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let d = tmpdir();
        // Force the state.json write to fail (a dir can't be renamed over).
        std::fs::create_dir(d.join(staging::STATE_FILE)).unwrap();
        let path_key = d.to_string_lossy().to_string();
        crate::server::muxer::clear_error(&path_key);

        let landed = staging::mark_aborted_on_loss_reporting_landed(&d, "loss exceeds threshold");
        assert!(!landed, "the forced write failure must report landed=false");
        record_loss_abort_write_failure("sg0", &d, "loss exceeds threshold");

        assert!(
            crate::server::muxer::MUX_ERRORS
                .lock()
                .unwrap()
                .contains_key(&path_key),
            "a dropped .aborted-loss write on a real device must raise an \
             operator card (MUX_ERRORS)"
        );
        crate::server::muxer::clear_error(&path_key);
    }
}

#[cfg(test)]
mod accept_loss_override_tests {
    use super::resume_effective_abort;

    // Catches the mutation that recomputes the abort threshold from raw config at one loss gate
    // while the other honours.accept-loss — the two-gates-one-run disagreement.
    #[test]
    fn the_accept_loss_override_raises_the_threshold_for_every_resume_gate() {
        assert_eq!(
            resume_effective_abort(true, "mkv", 5),
            u64::MAX,
            "with the override armed no loss gate may abort"
        );
        assert_eq!(
            resume_effective_abort(true, "iso", 0),
            u64::MAX,
            "the override outranks even the ISO byte-complete rule — the \
             operator is looking at the recorded damage when they press it"
        );
        assert_eq!(
            resume_effective_abort(false, "mkv", 5),
            super::super::effective_abort_secs("mkv", 5),
            "without the override the threshold is exactly the configured one"
        );
        assert_eq!(
            resume_effective_abort(false, "iso", 30),
            0,
            "without the override ISO still forces byte-complete"
        );
    }

    // Catches a re-introduced SECOND, hand-rolled threshold computation:
    // every loss gate must route through resume_effective_abort, so
    // effective_abort_secs may be named exactly once (comments stripped).
    #[test]
    fn resume_has_exactly_one_threshold_computation() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        // Production code only: the test modules below name these same
        // functions, and counting them would make this pin count itself.
        let src = &src[..src.find("#[cfg(test)]").expect("this file has tests")];
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| !l.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n");
        let direct = code.matches("super::effective_abort_secs(").count();
        assert_eq!(
            direct, 1,
            "the resume path must compute its abort threshold in ONE place \
             (resume_effective_abort); found {direct} direct calls to \
             effective_abort_secs, which is how the sweep gate and the mux gate \
             came to disagree about `.accept-loss` in the same run"
        );
    }

    // `.accept-loss` is READ at entry but CLEARED only at a hand-off. Each
    // delivery path (raw-ISO branch, MKV mux) consumes it, so each clear must
    // follow ITS OWN path's completion-marker write, with nothing risky between.
    #[test]
    fn the_accept_loss_marker_is_consumed_only_once_the_rip_is_delivered() {
        let src = crate::server::util::source_lf(include_str!("resume.rs"));
        // Production code only: the test modules below name these same
        // functions, and counting them would make this pin count itself.
        let src = &src[..src.find("#[cfg(test)]").expect("this file has tests")];
        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| !l.trim_start().starts_with("///"))
            .collect::<Vec<_>>()
            .join("\n");
        let read_at = code
            .find("staging::accept_loss_requested(")
            .expect("resume_remux must read the marker");
        let cleared: Vec<usize> = code
            .match_indices("staging::clear_accept_loss_marker(")
            .map(|(i, _)| i)
            .collect();
        assert!(
            !cleared.is_empty(),
            "the override must be consumed on delivery, in at least one place"
        );
        let mut delivered_by: Vec<usize> = Vec::new();
        for &c in &cleared {
            let w = code[..c]
                .rfind("staging::write_completed_marker(")
                .expect("every `.accept-loss` clear must follow a completion-marker write");
            assert!(w > read_at, "a clear's delivery must come after the read");
            let between = &code[w..c];
            for risky in ["mux_iso(", "mark_handoff(", "return"] {
                assert!(
                    !between.contains(risky),
                    "`.accept-loss` must be cleared right after its own path's \
                     write_completed_marker, but `{risky}` sits between them — a \
                     transient failure there would spend the operator's consent"
                );
            }
            assert!(
                !delivered_by.contains(&w),
                "two `.accept-loss` clears share one completion-marker write: one \
                 of them has moved ahead of its own path's delivery"
            );
            delivered_by.push(w);
        }
    }
}

#[cfg(test)]
mod vid_needs_disc_tests {
    use super::*;
    use crate::ku_fixture::{bd_image, write_sidecar};

    // KU-E1 (J6, J11): the mapfile holds no Volume ID. A resume with no drive in hand whose
    // keys need it refuses E7034 up front: one clear "insert the disc" state that the mux
    // worker then holds (never re-dispatched), with the ISO and mapfile untouched.
    #[test]
    fn a_resume_whose_keys_need_the_disc_is_held_not_retried() {
        let (staging, outcome, _t) = resume_after_restart(Keys::MediaKeyOnly);
        assert!(!outcome.success);
        assert!(outcome.failure_needs_disc, "held for the disc");
        assert!(!outcome.failure_retryable && !outcome.failure_finalize);
        let reason = outcome.failure_reason.unwrap_or_default();
        assert!(
            reason.starts_with("E7034 ") && reason.contains("Insert the disc to finish"),
            "{reason}"
        );

        let snap = staging::snapshot_staging_disc(&staging).unwrap();
        assert!(snap.needs_disc && snap.has_ripped && !snap.has_failed);
        assert_eq!(
            crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
            crate::server::muxer::MuxVerdict::SkipNeedsDisc,
            "the worker must not re-dispatch it"
        );

        // Inserting the disc makes it drive-resumable again (the worker still skips it).
        assert!(!super::super::resumable_dir_blocked(&snap));
        staging::set_needs_disc(&staging, false);
        let snap = staging::snapshot_staging_disc(&staging).unwrap();
        assert_eq!(
            crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
            crate::server::muxer::MuxVerdict::Dispatch
        );
    }

    // J23: when no VID could help (no media-key path, no VID-consuming source) a resume after
    // a restart is a plain "no key yet" (E7022): the retryable keyless deferral, never held.
    #[test]
    fn a_resume_no_vid_would_help_is_a_retryable_deferral() {
        let (staging, outcome, _t) = resume_after_restart(Keys::None);
        assert!(!outcome.success);
        assert!(!outcome.failure_needs_disc);
        assert!(
            outcome.failure_retryable,
            "a keyless deferral re-muxes once keys land"
        );
        let snap = staging::snapshot_staging_disc(&staging).unwrap();
        assert!(!snap.needs_disc && !snap.has_failed);
        assert_eq!(
            crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
            crate::server::muxer::MuxVerdict::Dispatch
        );
    }

    // An online key service that is down is an outage (E7028), never Missing: the resume
    // defers retryably (the worker re-asks next tick), never holds for the disc or fails.
    #[test]
    fn a_resume_with_the_key_service_down_is_a_retryable_outage() {
        let (staging, outcome, _t) = resume_after_restart(Keys::ServiceDown);
        assert!(!outcome.success);
        assert!(
            !outcome.failure_needs_disc,
            "an outage is never 'insert the disc'"
        );
        assert!(outcome.failure_retryable, "the worker retries it");
        assert!(!outcome.failure_finalize);
        let reason = outcome.failure_reason.unwrap_or_default();
        assert!(
            reason.starts_with("Ripped to ISO — no keys, mux deferred"),
            "{reason}"
        );
        let snap = staging::snapshot_staging_disc(&staging).unwrap();
        assert!(
            !snap.needs_disc && !snap.has_failed,
            "never held, never .failed"
        );
        assert_eq!(
            crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
            crate::server::muxer::MuxVerdict::Dispatch
        );
    }

    // The key chain a restarted resume has.
    enum Keys {
        // A keydb entry with only a media key: the VID would finish it (J23).
        MediaKeyOnly,
        // No key source holds anything.
        None,
        // An online service that refuses at the first query (E7028).
        ServiceDown,
    }

    // A `.ripped` KU fixture resumed by the mux worker with no set in memory (a restart).
    fn resume_after_restart(
        keys: Keys,
    ) -> (std::path::PathBuf, MuxHandoffOutcome, tempfile::TempDir) {
        let _guard = crate::server::log::env_guard();
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let t = tempfile::tempdir().unwrap();
        // SAFETY: env access in tests, serialized by env_guard.
        unsafe {
            std::env::set_var("AUTORIP_DIR", t.path());
        }
        let staging = t.path().join("staging").join("KU_Disc");
        std::fs::create_dir_all(&staging).unwrap();
        let fx = bd_image();
        let iso = fx.write(&staging, "KU_Disc.iso");
        let mapfile = write_sidecar(&fx, &iso, true);
        let keydb = t.path().join("keydb.cfg");
        if matches!(keys, Keys::MediaKeyOnly) {
            crate::ku_fixture::write_media_key_keydb(&fx, &keydb);
        }
        // `localhost` resolves to a private address: refused at the first query, no network.
        let keyserver_url = match keys {
            Keys::ServiceDown => "https://localhost:9/decode".to_string(),
            _ => String::new(),
        };
        let before = (
            std::fs::read(&iso).unwrap(),
            std::fs::read(&mapfile).unwrap(),
        );
        let marker = crate::server::muxer::RippedMarker {
            schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
            iso_path: iso.to_string_lossy().into_owned(),
            mapfile_path: mapfile.to_string_lossy().into_owned(),
            display_name: "KU_Disc".into(),
            disc_format: "bluray".into(),
            mkv_filename: "KU_Disc.mkv".into(),
            tmdb_title: String::new(),
            tmdb_year: 0,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            tmdb_media_type: String::new(),
            max_retries: 1,
            abort_on_lost_secs: 0,
            rip_elapsed_secs: 0.0,
            rip_errors: 0,
            rip_lost_video_secs: 0.0,
            rip_last_sector: 0,
            origin_device: String::new(),
            sweep_errors: 0,
            sweep_total_lost_ms: 0.0,
            sweep_main_lost_ms: 0.0,
            sweep_num_bad_ranges: 0,
            sweep_largest_gap_ms: 0.0,
            title_confident: true,
        };
        crate::server::muxer::write_marker(&staging, &marker).unwrap();
        let cfg = Arc::new(RwLock::new(Config {
            staging_dir: staging.parent().unwrap().to_string_lossy().into_owned(),
            keydb_path: Some(keydb.to_string_lossy().into_owned()),
            keyserver_url,
            ..Config::default()
        }));

        let outcome = remux_from_ripped_marker(&cfg, &staging, &marker);
        let after = (
            std::fs::read(&iso).unwrap(),
            std::fs::read(&mapfile).unwrap(),
        );
        assert!(after == before, "the ISO and its mapfile are untouched");
        (staging, outcome, t)
    }
}
