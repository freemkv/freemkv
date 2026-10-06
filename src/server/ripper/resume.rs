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
    /// `bytes_pending == 0` and the ISO is at least the mapfile's size. No
    /// loss check: `resume_remux` gates loss on the engine's verdict. Carries
    /// the resolved paths so the actor doesn't have to re-walk the directory.
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
/// has_mapfile`, mapfile loads with `bytes_pending == 0`, and the ISO is at least the
/// mapfile's `bytes_total`. Loss is not checked here (`_abort_on_lost_secs` is unused):
/// `resume_remux` gates it on the engine's title-scoped verdict.
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
    // staging-snapshot booleans tell us they exist but not their names.
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

    // The ISO's OWN stem, not the staging dir's name. `rip_disc` builds every
    // file inside a staging dir from `sanitize_path_compact(display_name)`.
    // A stem-less ISO would point delete_partial_output at a dotfile; bail loudly instead.
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
/// without touching `scan_image` or `mux_iso`.
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

// Fill the success half of a MuxHandoffOutcome from the `_mux` done state: its display
// fields, the bad-ranges drilldown (recomputed from the mapfile) and the COMBINED sweep +
// mux-time loss, so the origin device's done card matches the `_mux` and fresh-rip cards.
fn apply_success_fields(outcome: &mut MuxHandoffOutcome, rs: &super::RipState) {
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

// The done card's codecs: the post-mux STATE value when set, else the pre-mux snapshot.
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

// Whether `name` is a bare file name: no directory part, not `.`/`..`, not empty.
fn is_plain_leaf(name: &str) -> bool {
    Path::new(name).file_name().is_some_and(|n| n == name)
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

    let _keys_release = ReleaseKeysWhenSettled(&iso_path);

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
            let msg =
                "internal error: the settings lock is poisoned — restart the server".to_string();
            reset_status_after_ripping(device, "idle", &display_name, "", "", Some(msg));
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
    // Every planned output is written and deleted at `<staging>/<filename>`: one that is
    // not a plain file name (a `..`, a separator) would land outside staging.
    if let Some(bad) = plan_outputs.iter().find(|o| !is_plain_leaf(&o.filename)) {
        let msg = format!(
            "this disc's saved plan names an output outside its staging folder ({:?}) — not re-muxing; start a fresh rip",
            bad.filename
        );
        crate::server::log::device_log(device, &format!("Auto-resume aborted: {msg}"));
        reset_status_after_ripping(device, "error", &display_name, "", "", Some(msg));
        return;
    }
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
    // File names are `sanitize_path_compact(display_name)`, as `rip_disc` writes them (a
    // `.ripped` hand-off carries the raw title; an ISO stem is already in that form).
    let file_stem = crate::server::util::sanitize_path_compact(&display_name);
    delete_partial_output(&staging_dir, &file_stem);

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
            let msg = super::format_lib_error("reading the saved disc image", &e);
            crate::server::log::device_log(device, &format!("Auto-resume aborted: {msg}"));
            // scan_disc already moved this device to status="scanning"; bailing
            // without the reset below would strand that row. The reason is the
            // worker hand-off's failure card.
            reset_status_after_ripping(device, "idle", &display_name, "", "", Some(msg));
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
        let msg = "the saved disc image has no usable title (it may be truncated) — start a fresh rip to rebuild it".to_string();
        crate::server::log::device_log(device, &format!("Auto-resume aborted: {msg}"));
        // Same wedge as the scan_image failure above: reset scanning → idle
        // so the "already ripping" gate doesn't reject every later /api/rip.
        reset_status_after_ripping(device, "idle", &display_name, "", "", Some(msg));
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

    // Loss re-validation against the primary title (the classifier checks no loss).
    // `.get` guards a stale/out-of-range plan index.
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
            // The classifier already loaded this mapfile cleanly; a failure here is a
            // TOCTOU (file removed/corrupted/IO error). Abort and let the next pass
            // re-classify against fresh state.
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
        // Scope the loss exactly as the fresh rip does (whole-disc for ISO output,
        // in-title otherwise) so the two paths reach the same verdict.
        let output_is_iso = super::output_is_iso_image(&cfg_read.output_format);
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
            // Quarantine to a RESUMABLE `.aborted-loss` exactly as the mux-time loss
            // gate below does: the operator can accept the loss or re-insert the disc
            // for another recovery pass.
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
        // status="ripping" is not set yet (the update_state further below); record
        // the deferral reason without flagging a hard failure.
        defer_status_after_ripping(device, &display_name, &disc_format, &duration, reason);
        return;
    }

    let output_format = cfg_read.output_format.clone();
    let ext = super::output_extension_for(&output_format, disc);
    // TV fan-out: the primary episode's staging leaf comes from the plan; movies
    // keep the `{sanitized display_name}.{ext}` name.
    let filename = if is_fanout {
        plan_outputs[0].filename.clone()
    } else {
        format!("{}.{}", file_stem, ext)
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

    // Halt token for /api/stop to cancel through the mux (and every TV episode mux, which
    // look it up by device). Swapped in carrying a Stop that already landed on the spawn's
    // token; unregistered when this function returns.
    super::swap_halt_carrying_cancel(device, libfreemkv::Halt::new());
    let _halt_registration = HaltRegistration(device);

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
                return;
            }
            // FMTS forensic-key deferral: base keys resolved but the online-only
            // forensic index keys did not (`Error::FmtsKeyMissing`). Defer; the mux
            // worker re-attempts once a keydb/online update supplies keys.
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
                return;
            }
            // Any other key refusal: a keyless deferral, as a refused open is.
            if super::io_key_refusal(&e).is_some() {
                let reach = crate::server::keysource::take_online_decode_reachability();
                let (log_line, reason) = super::deferred_keyless_texts(&cfg_read, disc, reach);
                crate::server::log::device_log(device, &log_line);
                defer_status_after_ripping(device, &display_name, &disc_format, &duration, reason);
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
            return;
        }
    };

    if !mux_outcome.output_opened || !mux_outcome.completed {
        // Mirror rip_disc's incomplete-mux handling (mod.rs): a mid-mux finalize
        // failure or hard producer read error must surface with its reason, so the
        // operator sees why it stopped and that staging is still resumable.
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

    // Operator-facing loss for a resume = sweep loss (mapfile Unreadable) + demux
    // loss (decrypt/codec skips at mux); they are disjoint, so they add.
    let done_errors = done_sweep_damage.errors.saturating_add(mux_outcome.errors);
    let done_lost_video_secs =
        done_sweep_damage.main_lost_ms / crate::server::util::MILLIS_PER_SEC + demux_lost_secs;

    // Mux-time loss gate (a loss is a loss). Gate the total in-title loss
    // (sweep + mux-time decrypt/codec) against abort_on_lost_secs before filing.
    // ISO output is exempt (whole-disc, gated by 100% elsewhere).
    {
        // Honour `.accept-loss` here too: the same effective threshold as the sweep
        // gate above, so an accepted loss passes both gates.
        let effective_abort =
            resume_effective_abort(accept_loss, &output_format, cfg_read.abort_on_lost_secs);
        // The SAME `mux_loss_aborts` the fresh-rip path (`rip_disc` in mod.rs) uses.
        // `completed` is true: the incomplete-mux case returned above.
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
        let episodes_outcome =
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
        // A Stop is not a dropped episode: nothing is handed off and staging stays
        // intact for the next resume, as for a Stop during the primary mux.
        if matches!(episodes_outcome, freemkv_engine::RipOutcome::Halted) {
            crate::server::log::device_log(
                device,
                "Auto-resume mux stopped by user; staging preserved.",
            );
            return;
        }
        if delivered.len() < plan_outputs.len() {
            crate::server::log::syslog(&format!(
                "TV resume delivered {} of {} episodes: {} — the rest were dropped (see the device log)",
                delivered.len(),
                plan_outputs.len(),
                crate::server::log::sanitize_log_msg(&display_name),
            ));
        }
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
            // undurable episode was dropped above (its partial file deleted); the
            // list always contains at least the primary episode.
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
        // staging: that would strand an output the mover never sees.
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
        // config: this dir is resumed FROM a staged ISO, an intermediate by
        // definition. `keep_iso` still protects an ISO the operator kept.
        marker_tmdb.as_ref().map(|m| m.max_retries).unwrap_or(1),
        super::retain_intermediate_iso(cfg_read.keep_iso, &output_format),
    );

    // Prefer the codecs the mux frame loop wrote into STATE (the `_mux`
    // worker path seeds an empty codecs and only the mux fills it), so the
    // done card and the webhook below report the same codec string.
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
            // Carry sweep + mux-time damage so the done card reflects real
            // damage and the severity classifier rates the disc on the loss
            // actually in the MKV.
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

    // Fire the mux-stage webhook, mirroring rip_disc's terminal branch, for
    // both the cold auto-resume (`?resume=yes`) path and the `.ripped`
    // hand-off: this is the distinct mux_complete stage.
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

    // Honor auto_eject after a successful resume the same way rip_disc does
    // (`should_auto_eject` skips the synthetic `_mux` device).
    if super::should_auto_eject(cfg_read.auto_eject, device) {
        let device_path = format!("/dev/{}", device);
        super::eject_drive(&device_path);
    }
}

// On drop, forgets the rip's held key set once this resume left the dir settled (delivered,
// quarantined `.failed`, or held `.aborted-loss`): no later drive-less mux of it will come.
// A Stop or a retryable failure keeps them for the worker's next attempt.
struct ReleaseKeysWhenSettled<'a>(&'a Path);

impl Drop for ReleaseKeysWhenSettled<'_> {
    fn drop(&mut self) {
        let settled = self
            .0
            .parent()
            .and_then(staging::snapshot_staging_disc)
            .is_some_and(|s| s.completed || s.has_failed || s.has_aborted_loss);
        if settled {
            crate::server::keysource::forget_rip_keys(self.0);
        }
    }
}

// Unregisters the device's Halt on drop, so every exit of `resume_remux` after the
// registration releases it.
struct HaltRegistration<'a>(&'a str);

impl Drop for HaltRegistration<'_> {
    fn drop(&mut self) {
        super::unregister_halt(self.0);
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

    // Pre-seed STATE so `resume_remux`'s TMDB-from-STATE lookup finds the
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
        // Hand-off consumed. Drop any legacy marker so this dir doesn't get
        // re-queued; `.completed` already guards a failed delete, but a lingering
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
                apply_success_fields(&mut outcome, rs);
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
#[path = "resume_remux_space_tests.rs"]
mod remux_space_tests;

#[cfg(test)]
#[path = "resume_find_iso_tests.rs"]
mod find_iso_tests;

// A hard failure must not be advertised to the operator as a deferral that will fix itself.
#[cfg(test)]
#[path = "resume_failure_retryability_tests.rs"]
mod failure_retryability_tests;

// Resume's wiring into the shared `.done`/`.review` policy (mod.rs
// `title_is_confident` / `handoff_marker_name`). resume_remux previously
// that `mux_loss_aborts` had. These pin the call-site plumbing.
#[cfg(test)]
#[path = "resume_title_confidence_routing_tests.rs"]
mod title_confidence_routing_tests;

// Convergence M (finding 5): `remux_from_ripped_marker` detects whether
// `resume_remux` succeeded by checking for `.completed`. It must use the
// that matter.
#[cfg(test)]
#[path = "resume_completion_detection_tests.rs"]
mod completion_detection_tests;

// Regression guard for the resume abort-gate scoping. The fresh-rip
// post-retry abort check scopes loss by `output_format` via
// in lockstep.
#[cfg(test)]
#[path = "resume_resume_abort_scope_tests.rs"]
mod resume_abort_scope_tests;

#[cfg(test)]
#[path = "resume_resume_remux_log_archive_tests.rs"]
mod resume_remux_log_archive_tests;

#[cfg(test)]
#[path = "resume_resume_remux_unreadable_plan_tests.rs"]
mod resume_remux_unreadable_plan_tests;

#[cfg(test)]
#[path = "resume_resume_remux_scan_gate_tests.rs"]
mod resume_remux_scan_gate_tests;

#[cfg(test)]
#[path = "resume_resume_remux_webhook_tests.rs"]
mod resume_remux_webhook_tests;

#[cfg(test)]
#[path = "resume_resume_iso_auto_eject_tests.rs"]
mod resume_iso_auto_eject_tests;

#[cfg(test)]
#[path = "resume_resume_handoff_contract_tests.rs"]
mod resume_handoff_contract_tests;

#[cfg(test)]
// These tests pin the post-mux loss REPORTING contract at source level: a
// resume folds mux-time (demux/decrypt) loss into the operator-facing figures,
// the PRE-mux threshold.)
#[path = "resume_post_mux_loss_reporting_tests.rs"]
mod post_mux_loss_reporting_tests;

#[cfg(test)]
#[path = "resume_sweep_damage_marker_tests.rs"]
mod sweep_damage_marker_tests;

// Convergence round 4 (H1 + M4): the cold operator-resume mux path acquires the
// `.muxing` exclusion lock so a concurrent Wipe / second cold resume can't
// re-muxing the same possibly-corrupt output forever on the `_mux` worker loop.
#[cfg(test)]
#[path = "resume_resume_lock_and_fsync_tests.rs"]
mod resume_lock_and_fsync_tests;

#[cfg(test)]
#[path = "resume_accept_loss_override_tests.rs"]
mod accept_loss_override_tests;

#[cfg(test)]
#[path = "resume_resume_titles_tests.rs"]
mod resume_titles_tests;

#[cfg(test)]
#[path = "resume_refuse_open_tests.rs"]
mod refuse_open_tests;

#[cfg(test)]
#[path = "resume_vid_needs_disc_tests.rs"]
mod vid_needs_disc_tests;
