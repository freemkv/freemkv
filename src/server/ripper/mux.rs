//! Mux orchestration — the server's wrappers over the two mux drivers plus
//! the machinery it keeps on its own side of the seam: the hard watchdog, the
//! shared `MuxAtomics` it reads, the bridges that feed those atomics + the
//! per-frame UI state, and the `MuxOutcome` → staging/marker classification.
//!
//! Two entry points: [`mux_iso`] (multipass/resume, a staged image through the
//! engine's `mux_image_titles`) and [`mux_live`] (live single-pass through
//! libfreemkv's `mux_stream`), both mapped via `map_iso_mux_outcome`.

use crate::server::util::{BYTES_PER_GIB, BYTES_PER_MIB, MILLIS_PER_SEC};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::session::device_halt;
use super::state::{RipState, update_state};

/// Hard watchdog escalation threshold. When the producer's "last frame /
/// drive activity" timestamp hasn't moved in this many seconds, the rip
/// thread is presumed stuck inside an unkillable syscall (hung NFS write,
/// wedged decrypt, frozen ioctl). Graceful teardown is then impossible, so
/// we exit the process and rely on Docker `restart: unless-stopped` to
/// bring autorip back; `resume_or_quarantine_staging` then decides whether
/// to retry or quarantine via `.failed`.
pub const HARD_WATCHDOG_STALL_SECS: u64 = 1200;

/// Deadline for the hard watchdog's pre-exit `.restart_count` bump.
pub const WATCHDOG_BUMP_DEADLINE: Duration = Duration::from_secs(5);

/// Run `op` on a named helper thread and wait at most `deadline` for it.
/// Returns `true` if it finished in time; `false` on timeout or spawn failure.
pub fn bounded_call(name: &str, deadline: Duration, op: impl FnOnce() + Send + 'static) -> bool {
    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(0);
    let _ = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            op();
            let _ = tx.send(());
        });
    rx.recv_timeout(deadline).is_ok()
}

/// The hard watchdog's `.restart_count` bump, bounded by `WATCHDOG_BUMP_DEADLINE`
/// so a wedged NFS syscall can't block the `exit(1)` that follows.
pub fn watchdog_bump_restart_count(device: &str, staging_disc_dir: &std::path::Path) -> bool {
    let bump_dir = staging_disc_dir.to_path_buf();
    let done = bounded_call(
        "autorip-watchdog-counter-bump",
        WATCHDOG_BUMP_DEADLINE,
        move || {
            let _ = crate::server::ripper::staging::increment_restart_count(&bump_dir);
        },
    );
    if !done {
        eprintln!(
            "[mux/{}] watchdog: counter bump timed out; proceeding to exit anyway",
            device
        );
        tracing::error!(
            target: "mux",
            device = %device,
            "watchdog: counter bump timed out; proceeding to exit anyway"
        );
    }
    done
}

// Total Progress % during mux: same byte-weighted formula `state.rs` uses for sweep/patch, so
// the bar progresses smoothly across the handoff.
fn total_pct_byte_weight(
    bytes_total_disc: u64,
    max_retries: u8,
    bytes_unreadable_at_mux: u64,
    mux_pct: u8,
) -> u8 {
    if max_retries == 0 || bytes_total_disc == 0 {
        return mux_pct.min(100);
    }
    // u128 to keep multiplication overflow-safe on > 4 GB discs.
    let cap = bytes_total_disc as u128;
    let retry_total = (max_retries as u128) * (bytes_unreadable_at_mux as u128);
    let total_work = cap + retry_total + cap;
    if total_work == 0 {
        return mux_pct.min(100);
    }
    let mux_done = cap * (mux_pct as u128) / 100;
    let total_done = cap + retry_total + mux_done;
    ((total_done * 100 / total_work).min(100)) as u8
}

// Damage fields from the final sweep/patch pass, carried forward so they stay visible in
// /api/state during mux instead of zeroing out.
#[derive(Default, Clone)]
pub(crate) struct SweepDamageSnapshot {
    pub(crate) errors: u32,
    pub(crate) total_lost_ms: f64,
    pub(crate) main_lost_ms: f64,
    pub(crate) bad_ranges: Vec<super::state::BadRange>,
    pub(crate) num_bad_ranges: u32,
    pub(crate) bad_ranges_truncated: u32,
    pub(crate) largest_gap_ms: f64,
}

pub(crate) struct MuxInputs<'a> {
    pub(crate) device: &'a str,
    pub(crate) display_name: String,
    pub(crate) disc_format: String,
    pub(crate) tmdb_title: String,
    pub(crate) tmdb_year: u16,
    pub(crate) tmdb_poster: String,
    pub(crate) tmdb_overview: String,
    pub(crate) duration: String,
    pub(crate) codecs: String,
    pub(crate) filename: String,
    /// Total expected bytes for the mux phase (used for percent + ETA).
    /// Falls back to the input title's `size_bytes` if 0 is passed.
    pub(crate) total_bytes: u64,
    /// Per-title bitrate; used to convert skipped sectors → estimated
    /// lost video time for the UI.
    pub(crate) title_bytes_per_sec: f64,
    /// `max_retries + 2` in multipass mode, 0 in direct mode. Threaded
    /// through every per-frame `update_state` so the dashboard's
    /// pass/total bars don't snap back to a "fresh rip" view.
    pub(crate) total_passes: u8,
    /// Disc capacity in bytes — same value `state.rs` uses to compute
    /// the sweep + mux contributions to the total-progress denominator.
    /// Plumbed from `disc.capacity_bytes` at the orchestrator level.
    pub(crate) bytes_total_disc: u64,
    /// User-configured max retry passes (`cfg_read.max_retries`). Used
    /// as the multiplier on `bytes_unreadable` for the retry-phase
    /// contribution to total work, mirroring `state.rs`.
    pub(crate) max_retries: u8,
    /// `bytes_unreadable` at mux start — i.e. after every retry pass
    /// has finished. Drives the retry-phase contribution to the
    /// total-progress denominator. Zero on a clean disc (every bad
    /// sector recovered) — in that case the retry phase contributes
    /// nothing and total = sweep+mux only, so mux opens at ~50%.
    pub(crate) bytes_unreadable_at_mux: u64,
    /// Pre-resolved mux output URL (e.g. `mkv:///srv/.../foo.mkv`,
    /// `network://host:port`). Resolved by the orchestrator because URL
    /// construction depends on `cfg.network_target` + `output_format`.
    pub(crate) dest_url: String,
    /// Kernel-reported preferred batch size; surfaced in `RipState` so
    /// the UI keeps showing it through the mux phase.
    pub(crate) batch: u16,
    /// Per-disc staging directory (e.g. `/staging/MyDisc/`). Used by
    /// the hard watchdog to bump `.restart_count` before
    /// `std::process::exit(1)` so the post-restart resume logic can
    /// promote the disc to `.failed` once `RESTART_LIMIT` is reached.
    pub(crate) staging_disc_dir: PathBuf,
    /// Damage fields snapshotted from the final sweep/patch pass.
    /// Carried into every per-frame `push_state` so /api/state preserves
    /// damage visibility during the mux phase. Defaults to zero/empty for
    /// direct (single-pass) mode.
    pub(crate) sweep_damage: SweepDamageSnapshot,
}

// Outcome of a mux driver, used by the orchestrator to drive the post-mux history record +
// final state push.
pub(crate) struct MuxOutcome {
    /// True iff the read loop drained `frame_rx` to EOF AND the post-loop
    /// `pipe.finish_with_halt(...)` returned `Ok`. This is the orchestrator's gate for writing
    /// `.done` / `.completed` markers in `staging` — the on-disk success signal for the
    /// resume-on-startup detector and the mover thread.
    pub(crate) completed: bool,
    pub(crate) bytes_done: u64,
    pub(crate) elapsed_secs: f64,
    pub(crate) speed_mbs: f64,
    /// Demux skip count from the input stream (`DiscStream::errors`).
    /// Multipass callers usually overwrite this with the mapfile's
    /// `bytes_unreadable / 2048` because demux skips during ISO mux
    /// are typically zero — the real bad-sector count lives in the
    /// mapfile sidecar.
    pub(crate) errors: u32,
    /// Estimated lost video seconds derived from `errors`. Same
    /// override pattern as `errors` when a mapfile is available.
    pub(crate) lost_video_secs: f64,
    /// True iff the output stream was successfully opened (i.e. we got
    /// past header buffering and `libfreemkv::output(...)` returned
    /// Ok). The orchestrator gates history-record writing on this:
    /// stops or open-failures before the output exists leave no
    /// salvageable artefact, so they get an early-return path
    /// (matching pre-split behaviour). Stops or write errors after
    /// the output is open leave a partial MKV in staging and a
    /// "stopped" history record describing it.
    pub(crate) output_opened: bool,
    /// Set when `MuxSink::close()` failed to finalise the MKV (most
    /// commonly: the Cues seek-back at EBML close raised an I/O error,
    /// leaving an unseekable / structurally-invalid output). Carries
    /// the formatted error so the orchestrator can put it in the
    /// `.failed` marker reason. `Some(_)` implies `completed == false`.
    ///
    /// Pre-0.20.8 the close error was swallowed (logged only) and
    /// `.done` / `.completed` got written for unseekable MKVs — the
    /// validation audit's #1 "Reasonable tier" item.
    pub(crate) finalize_error: Option<String>,
    /// Set (with the specific cause) when the producer thread aborted
    /// mid-stream on a hard read error, truncating the MKV. Distinct from
    /// `finalize_error` (structural MKV defect, quarantine): a read error
    /// leaves the disc resumable and is NOT a user-initiated stop, so the
    /// orchestrator reports `status="error"` with a clear `last_error`
    /// instead of the silent "stopped → idle" path an operator halt takes.
    pub(crate) read_error: Option<String>,
}

/// Per-frame UI state that the consumer needs to fill in the
/// `update_state` payload. Cloned once into the `MuxSink` and reused
/// every frame — none of these fields change during mux.
struct UiState {
    device: String,
    display_name: String,
    disc_format: String,
    tmdb_title: String,
    tmdb_year: u16,
    tmdb_poster: String,
    tmdb_overview: String,
    duration: String,
    codecs: String,
    filename: String,
    batch: u16,
    total_bytes: u64,
    title_bytes_per_sec: f64,
    total_passes: u8,
    /// Disc capacity, used by `total_pct_byte_weight` to size the
    /// total-progress denominator.
    bytes_total_disc: u64,
    /// Configured max retry passes; multiplier on `bytes_unreadable_at_mux`
    /// for the retry-phase contribution to total work.
    max_retries: u8,
    /// `bytes_unreadable` at mux start (after every retry pass finished).
    bytes_unreadable_at_mux: u64,
    /// Damage fields from the final sweep/patch pass. Kept constant across
    /// all mux-phase `push_state` calls so the damage pill / bad-ranges list
    /// stays visible rather than reverting to default-zero on the first tick.
    sweep_damage: SweepDamageSnapshot,
}

// Cross-thread atomics the consumer reads on every per-frame `update_state`; producer writes
// from the reader thread, consumer + watchdog read them.
#[derive(Clone)]
struct SharedAtomics {
    /// Last byte position reported by the drive's BytesRead event.
    /// Preferred over `output.bytes_written()` for the progress bar
    /// because reads run ahead of writes when the channel is full.
    latest_bytes_read: Arc<AtomicU64>,
    rip_last_lba: Arc<AtomicU64>,
    rip_current_batch: Arc<AtomicU16>,
    /// Watchdog "last activity" timestamp. The drive + stream event
    /// callbacks update it from the reader thread; the consumer also
    /// updates it after each frame write. The watchdog reads it.
    wd_last_frame: Arc<AtomicU64>,
    /// Bytes written by the output sink. Consumer writes; watchdog
    /// reads (used to render the "stalled at X GB" UI).
    wd_bytes: Arc<AtomicU64>,
    /// Snapshot of `input.errors` after the most recent `read()`. The
    /// producer updates it after every frame; the consumer reads it
    /// inside `apply` to surface the skip-event count. Atomic so we don't
    /// need to put the input stream behind a mutex.
    input_errors: Arc<AtomicU32>,
    /// Snapshot of `input.lost_bytes` after the most recent `read()` —
    /// the actual bytes zero-filled past read errors. Used (not
    /// `input_errors`) to compute `lost_video_secs`: an AACS skip event
    /// covers a whole 6144-byte unit, so `errors * 2048` understates loss
    /// by the alignment factor. Produced/consumed like `input_errors`.
    input_lost_bytes: Arc<AtomicU64>,
}

// Build + push the per-frame mux `update_state` payload; shared by the live `MuxSink` and the
// image-mux `EngineMuxSink` so both render an identical `RipState`.
#[allow(clippy::too_many_arguments)]
fn push_mux_state(
    ui: &UiState,
    atomics: &SharedAtomics,
    pct: u8,
    speed: f64,
    eta: String,
    bytes_done: u64,
    lost_video_secs: f64,
    errors: u32,
) {
    if crate::server::web::debug_enabled() {
        eprintln!(
            "[DEBUG] MuxSink::push_state: pct={}, bytes_done={:.2}GB, speed={}MB/s",
            pct,
            bytes_done as f64 / BYTES_PER_GIB,
            speed
        );
    }
    update_state(
        &ui.device,
        RipState {
            device: ui.device.clone(),
            status: "ripping".to_string(),
            disc_present: true,
            disc_name: ui.display_name.clone(),
            disc_format: ui.disc_format.clone(),
            progress_pct: pct,
            progress_gb: bytes_done as f64 / BYTES_PER_GIB,
            speed_mbs: speed,
            eta: eta.clone(),
            // Demux `errors` is usually 0 during mux (ISO reads don't fail), so
            // carry the sweep/patch bad-sector count instead, keeping the
            // damage pill visible to operators polling /api/state during mux.
            errors: if ui.sweep_damage.errors > 0 {
                ui.sweep_damage.errors
            } else {
                errors
            },
            lost_video_secs: if ui.sweep_damage.total_lost_ms > 0.0 {
                ui.sweep_damage.total_lost_ms / MILLIS_PER_SEC
            } else {
                lost_video_secs
            },
            last_sector: atomics.rip_last_lba.load(Ordering::Relaxed),
            current_batch: atomics.rip_current_batch.load(Ordering::Relaxed),
            preferred_batch: ui.batch,
            output_file: ui.filename.clone(),
            tmdb_title: ui.tmdb_title.clone(),
            tmdb_year: ui.tmdb_year,
            tmdb_poster: ui.tmdb_poster.clone(),
            tmdb_overview: ui.tmdb_overview.clone(),
            duration: ui.duration.clone(),
            codecs: ui.codecs.clone(),
            // Carry multipass identity per frame so the UI doesn't snap back to
            // "fresh rip" at mux start (pass == total_passes = "on mux pass").
            // Total progress uses sweep/patch's byte-weighted formula for a smooth handoff.
            pass: ui.total_passes,
            total_passes: ui.total_passes,
            pass_progress_pct: pct,
            pass_eta: eta.clone(),
            total_progress_pct: total_pct_byte_weight(
                ui.bytes_total_disc,
                ui.max_retries,
                ui.bytes_unreadable_at_mux,
                pct,
            ),
            total_eta: eta,
            // Carry sweep-phase damage fields so they remain visible
            // in /api/state during the entire mux phase.
            total_lost_ms: ui.sweep_damage.total_lost_ms,
            main_lost_ms: ui.sweep_damage.main_lost_ms,
            bad_ranges: ui.sweep_damage.bad_ranges.clone(),
            num_bad_ranges: ui.sweep_damage.num_bad_ranges,
            bad_ranges_truncated: ui.sweep_damage.bad_ranges_truncated,
            largest_gap_ms: ui.sweep_damage.largest_gap_ms,
            ..Default::default()
        },
    );
}

// Build the specific cause string for a hard producer `read()` error from its
// `libfreemkv::Error` code, re-surfaced with an English label.
fn producer_read_error_cause(e: &std::io::Error) -> String {
    match freemkv_engine::error_code(e) {
        Some(code) if code != libfreemkv::error::E_IO_ERROR => {
            // `Display` is code-only, so `{e}` is a bare `E####` for
            // argument-less variants. Attach an English label so the operator
            // sees a human cause, matching sweep/patch's error labeling.
            format!(
                "read error mid-stream (E{code}): {}",
                coded_error_label(code)
            )
        }
        _ => format!("read error mid-stream: {e}"),
    }
}

// Short English label for a coded `libfreemkv` fault reaching the mux producer as an
// `io::Error` (Display is code-only, so we map the code here). Mirrors sweep/patch's
// `non_scsi_error_label`.
fn coded_error_label(code: u16) -> &'static str {
    use libfreemkv::error as ec;
    match code {
        c if c == ec::E_DECRYPT_FAILED => "decryption failed",
        c if c == ec::E_DISC_READ => "disc read error",
        c if c == ec::E_HALTED => "rip stopped by user",
        c if c == ec::E_MAPFILE_INVALID => "recovery mapfile invalid",
        c if c == ec::E_NO_STREAMS => "no playable streams on disc",
        c if c == ec::E_DISC_CAPACITY_OVERFLOW || c == ec::E_DISC_CAPACITY_MALFORMED => {
            "drive reported unusable disc capacity"
        }
        _ => "read failed mid-stream",
    }
}

/// Drop guard that stops the mux watchdog thread when the owning mux call
/// ([`mux_live`] live single-pass, or [`mux_iso`] ISO/multipass+resume) returns.
struct WatchdogGuard(Arc<AtomicBool>);
impl Drop for WatchdogGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

// Spawn the mux watchdog thread (soft stall UI + hard exit(1) escalation).
// Shared verbatim by `mux_live` and `mux_iso` for identical escalation
// semantics; reads `wd_last_frame`/`wd_bytes`, which callers feed.
fn spawn_mux_watchdog(
    inputs: &MuxInputs<'_>,
    wd_active: Arc<AtomicBool>,
    wd_last_frame: Arc<AtomicU64>,
    wd_bytes: Arc<AtomicU64>,
) {
    let active = wd_active.clone();
    let last_frame = wd_last_frame.clone();
    let wbytes = wd_bytes.clone();
    let wd_device = inputs.device.to_string();
    let wd_display = inputs.display_name.clone();
    let wd_format = inputs.disc_format.clone();
    let wd_tmdb_title = inputs.tmdb_title.clone();
    let wd_tmdb_poster = inputs.tmdb_poster.clone();
    let wd_tmdb_overview = inputs.tmdb_overview.clone();
    let wd_duration = inputs.duration.clone();
    let wd_codecs = inputs.codecs.clone();
    let wd_total = inputs.total_bytes;
    let wd_tmdb_year = inputs.tmdb_year;
    let wd_filename = inputs.filename.clone();
    let wd_staging_disc_dir = inputs.staging_disc_dir.clone();
    // Intentionally detached (no JoinHandle): holds only Arc<Atomic*> clones,
    // self-terminates when `active` goes false (WatchdogGuard drop), and hard
    // escalation just calls exit(1) directly — nothing left to join anyway.
    std::thread::spawn(move || {
        let mut was_stalled = false;
        let mut last_log_secs: u64 = 0;
        while active.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(15));
            if !active.load(Ordering::Relaxed) {
                break;
            }
            let now = crate::server::util::epoch_secs();
            let last = last_frame.load(Ordering::Relaxed);
            let stall_secs = now.saturating_sub(last);

            // Hard escalation: the thread is stuck in an un-returning syscall
            // (hung NFS, wedged decrypt, frozen ioctl), so graceful cleanup is
            // impossible — bump `.restart_count` and `exit(1)` for Docker to recover.
            if stall_secs >= HARD_WATCHDOG_STALL_SECS {
                let bytes_good = wbytes.load(Ordering::Relaxed);
                let msg = format!(
                    "hard watchdog escalating: stalled {}s at {:.2} GB; exiting process for container restart",
                    stall_secs,
                    bytes_good as f64 / BYTES_PER_GIB,
                );
                // CRITICAL: do NOT call `device_log` — the log lives on the
                // same NFS `/config` we may be escalating because it's wedged.
                // `eprintln!`/`tracing::error!` go to docker logs, so `exit(1)` isn't blocked.
                eprintln!("[mux/{}] {}", wd_device, msg);
                tracing::error!(
                    target: "mux",
                    device = %wd_device,
                    bytes_good,
                    stall_secs,
                    staging = %wd_staging_disc_dir.display(),
                    "hard watchdog escalating; exiting process for container restart"
                );
                // Best-effort: bump the restart counter (errors ignored, exiting
                // anyway) so RESTART_LIMIT can engage, bounded so a wedged NFS
                // mount still lets us `exit(1)`.
                watchdog_bump_restart_count(&wd_device, &wd_staging_disc_dir);
                // No `drop(_wd_guard)` — that's the producer's
                // local; we're a detached watchdog thread. The
                // OS will tear down every thread on exit(1).
                std::process::exit(1);
            }

            if stall_secs >= 30 {
                // Compute bytes/gb/pct/stall_str once and reuse for
                // both the log line and the UI update — a single
                // `wbytes` read so the two can't disagree.
                let bytes = wbytes.load(Ordering::Relaxed);
                let gb = bytes as f64 / BYTES_PER_GIB;
                let pct = if let Some(p) = (bytes * 100).checked_div(wd_total) {
                    p.min(100) as u8
                } else {
                    0
                };
                let stall_str = {
                    let m = stall_secs / 60;
                    let s = stall_secs % 60;
                    if m > 0 {
                        format!("{}m {:02}s", m, s)
                    } else {
                        format!("{}s", s)
                    }
                };
                let should_log = !was_stalled || stall_secs >= last_log_secs + 60;
                if should_log {
                    last_log_secs = stall_secs;
                    crate::server::log::device_log(
                        &wd_device,
                        &format!(
                            "Drive stalled at {:.1} GB ({}%) — waiting for read ({})",
                            gb, pct, stall_str
                        ),
                    );
                }
                super::state::update_state_with(&wd_device, |s| {
                    // Don't clobber terminal state: the 15 s wake tick can fire
                    // after `handle_stop` set "idle" or a rip finished as
                    // "done"/"complete"/"failed"/"error" — that status wins.
                    match s.status.as_str() {
                        "idle" | "done" | "complete" | "failed" | "error" => return,
                        _ => {}
                    }
                    s.device = wd_device.clone();
                    s.status = "ripping".to_string();
                    s.disc_present = true;
                    s.disc_name = wd_display.clone();
                    s.disc_format = wd_format.clone();
                    s.progress_pct = pct;
                    s.progress_gb = gb;
                    s.speed_mbs = 0.0;
                    s.eta = format!("stalled {}", stall_str);
                    s.output_file = wd_filename.clone();
                    s.tmdb_title = wd_tmdb_title.clone();
                    s.tmdb_year = wd_tmdb_year;
                    s.tmdb_poster = wd_tmdb_poster.clone();
                    s.tmdb_overview = wd_tmdb_overview.clone();
                    s.duration = wd_duration.clone();
                    s.codecs = wd_codecs.clone();
                    // errors / lost_video_secs / last_sector / current_batch
                    // / preferred_batch / pass / total_passes / bytes_*
                    // / bad_ranges / largest_gap_ms intentionally untouched.
                });
                was_stalled = true;
            } else if was_stalled {
                crate::server::log::device_log(&wd_device, "Drive recovered — reads resumed");
                was_stalled = false;
                last_log_secs = 0;
            }
        }
    });
}

/// One throttled progress tick, as both mux bridges render it.
struct MuxTick<'a> {
    bytes_done: u64,
    total_bytes: u64,
    pct: u8,
    /// MB/s.
    speed: f64,
    eta: &'a str,
}

// `h:mm:ss` / `m:ss`, blank past 99:59:59 (an estimate that far out means nothing).
fn fmt_eta(secs: u64) -> String {
    if secs > 359_999 {
        return String::new();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

// The 60 s device-log progress line (a separate cadence from the 1 s `update_state`).
fn log_mux_tick(device: &str, last_log: &Mutex<Instant>, now: Instant, t: &MuxTick<'_>) {
    let mut last = last_log.lock().unwrap_or_else(|p| p.into_inner());
    if now.duration_since(*last).as_secs() < 60 {
        return;
    }
    *last = now;
    let gb = t.bytes_done as f64 / BYTES_PER_GIB;
    let speed_str = if t.speed >= 1.0 {
        format!("{:.1} MB/s", t.speed)
    } else {
        format!("{:.0} KB/s", t.speed * 1024.0)
    };
    let eta_str = if t.eta.is_empty() {
        String::new()
    } else {
        format!(" ETA {}", t.eta)
    };
    let line = if t.total_bytes > 0 {
        let total_gb = t.total_bytes as f64 / BYTES_PER_GIB;
        format!(
            "{gb:.1} GB / {total_gb:.1} GB ({}%) {speed_str}{eta_str}",
            t.pct
        )
    } else {
        format!("{gb:.1} GB {speed_str}")
    };
    crate::server::log::device_log(device, &line);
}

// Shared atomic counters the mux drivers feed via their progress bridges and the
// hard watchdog reads; the orchestrator builds these before calling a driver.
#[derive(Clone)]
pub(crate) struct MuxAtomics {
    pub(crate) latest_bytes_read: Arc<AtomicU64>,
    pub(crate) rip_last_lba: Arc<AtomicU64>,
    pub(crate) rip_current_batch: Arc<AtomicU16>,
    pub(crate) wd_last_frame: Arc<AtomicU64>,
    pub(crate) wd_bytes: Arc<AtomicU64>,
    pub(crate) input_errors: Arc<AtomicU32>,
}

// ── ISO / multipass + resume mux through the engine ─────────────────────────
/// A staged disc image opened through the engine, and the title of it to mux.
pub(crate) struct IsoMuxSource<'a> {
    /// The image, scanned and keyed (see `keysource::open_staged_image`).
    pub(crate) image: &'a freemkv_engine::OpenedImage,
    /// Index into `image.disc.titles`.
    pub(crate) title_index: usize,
}

// The engine `Sink` for an image mux: feeds the same watchdog atomics and per-frame
// `RipState` the live bridge does, cancels on the device's halt, and keeps the title's
// result (`Event::TitleDone`) for `map_iso_mux_outcome`.
struct EngineMuxSink {
    ui: UiState,
    atomics: SharedAtomics,
    halt: libfreemkv::Halt,
    last_update: Mutex<Instant>,
    last_log: Mutex<Instant>,
    /// A frame reached the output, so it was opened.
    opened: AtomicBool,
    result: Mutex<Option<std::io::Result<libfreemkv::MuxOutcome>>>,
}

impl EngineMuxSink {
    fn new(ui: UiState, atomics: SharedAtomics, halt: libfreemkv::Halt) -> Self {
        let now = Instant::now();
        Self {
            ui,
            atomics,
            halt,
            last_update: Mutex::new(now),
            last_log: Mutex::new(now),
            opened: AtomicBool::new(false),
            result: Mutex::new(None),
        }
    }

    fn touch(&self) {
        self.atomics
            .wd_last_frame
            .store(crate::server::util::epoch_secs(), Ordering::Relaxed);
    }

    // The title's result. None means the engine stopped before starting it: a halt.
    // The title's result, else the loop's `outcome`: a refusal before any title (a key
    // refusal before `TitleStart`) must not read as a Stop.
    fn take_result(
        &self,
        outcome: &freemkv_engine::RipOutcome,
    ) -> std::io::Result<libfreemkv::MuxOutcome> {
        self.result
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_else(|| Err(outcome_error(outcome)))
    }
}

impl freemkv_engine::Sink for EngineMuxSink {
    fn log(&self, level: freemkv_engine::Level, msg: &str) {
        self.touch();
        match level {
            freemkv_engine::Level::Trace | freemkv_engine::Level::Debug => {
                tracing::debug!(target: "mux", device = %self.ui.device, "{msg}");
            }
            _ => crate::server::log::device_log(&self.ui.device, msg),
        }
    }

    fn progress(&self, p: &freemkv_engine::Progress) {
        self.touch();
        self.atomics.wd_bytes.store(p.bytes_done, Ordering::Relaxed);
        self.atomics
            .latest_bytes_read
            .store(p.bytes_done, Ordering::Relaxed);
        let now = Instant::now();
        if !self.opened.swap(true, Ordering::Relaxed) {
            crate::server::log::device_log(&self.ui.device, "Output opened — muxing");
            *self.last_update.lock().unwrap_or_else(|e| e.into_inner()) = now;
            *self.last_log.lock().unwrap_or_else(|e| e.into_inner()) = now;
            return;
        }
        {
            let mut last = self.last_update.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(*last).as_secs_f64() < 1.0 {
                return;
            }
            *last = now;
        }
        let total_bytes = if p.bytes_total > 0 {
            p.bytes_total
        } else {
            self.ui.total_bytes
        };
        let pct = (p.bytes_done * 100)
            .checked_div(total_bytes)
            .map_or(0, |v| v.min(100) as u8);
        let speed = p.speed_bps as f64 / BYTES_PER_MIB;
        let eta = p.eta_secs.map(fmt_eta).unwrap_or_default();
        let tick = MuxTick {
            bytes_done: p.bytes_done,
            total_bytes,
            pct,
            speed,
            eta: &eta,
        };
        log_mux_tick(&self.ui.device, &self.last_log, now, &tick);
        let errors = self.atomics.input_errors.load(Ordering::Relaxed);
        push_mux_state(
            &self.ui,
            &self.atomics,
            pct,
            speed,
            eta,
            p.bytes_done,
            0.0,
            errors,
        );
    }

    fn event(&self, e: &freemkv_engine::Event<'_>) {
        self.touch();
        if let freemkv_engine::Event::TitleDone { result, .. } = e {
            let owned = match result {
                Ok(o) => Ok((*o).clone()),
                Err(err) => Err(std::io::Error::new(err.kind(), err.to_string())),
            };
            *self.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(owned);
        }
    }

    fn should_cancel(&self) -> bool {
        self.halt.is_cancelled()
    }
}

// autorip's `libfreemkv::MuxEvents` bridge for the live single-pass mux: updates the shared
// atomics + per-frame UI push the watchdog and `/api/state` read.
struct AutoripMuxEvents {
    ui: UiState,
    atomics: SharedAtomics,
    progress: Mutex<freemkv_engine::SpeedEstimator>,
    /// 1 s `update_state` throttle (was `MuxSink::last_update`).
    last_update: Mutex<Instant>,
    /// 60 s device-log throttle (was `MuxSink::last_log`).
    last_log: Mutex<Instant>,
    /// True once `output()` opened. Feeds `output_opened` in the mapping.
    opened: AtomicBool,
}

impl libfreemkv::MuxEvents for AutoripMuxEvents {
    fn on_output_opened(&self, _title: &libfreemkv::DiscTitle) {
        self.opened.store(true, Ordering::Relaxed);
        crate::server::log::device_log(&self.ui.device, "Output opened — muxing");
        // Reset the throttles so the first progress tick lands promptly after
        // the sink opens (matches the pre-migration `start`-relative cadence).
        let now = Instant::now();
        *self.last_update.lock().unwrap_or_else(|e| e.into_inner()) = now;
        *self.last_log.lock().unwrap_or_else(|e| e.into_inner()) = now;
    }

    fn on_read_progress(&self, bytes_read: u64, _bytes_total: u64) {
        // Reader-side BytesRead: keeps the watchdog fresh during header reads
        // (no SCSI READ_TIMEOUT backstop on the ISO path) and feeds the
        // read-ahead position the UI prefers over write-lagged output.
        self.atomics
            .wd_last_frame
            .store(crate::server::util::epoch_secs(), Ordering::Relaxed);
        self.atomics
            .latest_bytes_read
            .store(bytes_read, Ordering::Relaxed);
    }

    fn on_write_progress(&self, bytes_written: u64, _bytes_total: u64) {
        // Writer-side per-frame — mirrors `MuxSink::apply`. Feed the watchdog's
        // activity timestamp AND its good-byte counter (both read by
        // `spawn_mux_watchdog`), then push throttled UI state.
        self.atomics
            .wd_last_frame
            .store(crate::server::util::epoch_secs(), Ordering::Relaxed);
        self.atomics
            .wd_bytes
            .store(bytes_written, Ordering::Relaxed);

        // 1 s `update_state` cadence (same throttle as the pre-split loop).
        let now = Instant::now();
        {
            let mut last = match self.last_update.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if now.duration_since(*last).as_secs_f64() < 1.0 {
                return;
            }
            *last = now;
        }

        // Progress uses the ISO *read* position (read-ahead) when available,
        // falling back to the *write* position only until the first BytesRead
        // event fires — identical to `MuxSink::apply`.
        let lbr = self.atomics.latest_bytes_read.load(Ordering::Relaxed);
        let bytes_done = if lbr > 0 { lbr } else { bytes_written };
        let pct = if let Some(p) = (bytes_done * 100).checked_div(self.ui.total_bytes) {
            p.min(100) as u8
        } else {
            0
        };
        let (speed, speed_for_eta) = {
            let mut progress = match self.progress.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let display_speed = progress.observe(now, bytes_done);
            let eta_speed = progress.eta_speed_mbs(now, display_speed);
            (display_speed, eta_speed)
        };
        let eta = if speed_for_eta > 0.0 && self.ui.total_bytes > bytes_done {
            fmt_eta(
                ((self.ui.total_bytes - bytes_done) as f64 / BYTES_PER_MIB / speed_for_eta) as u64,
            )
        } else {
            String::new()
        };
        let tick = MuxTick {
            bytes_done,
            total_bytes: self.ui.total_bytes,
            pct,
            speed,
            eta: &eta,
        };
        log_mux_tick(&self.ui.device, &self.last_log, now, &tick);

        let skip_errors = self.atomics.input_errors.load(Ordering::Relaxed);
        // Live lost-video-secs from bytes zero-filled so far; the file highway's
        // `input_lost_bytes` usually stays 0 mid-run, so this only refines the
        // mid-mux UI — the authoritative total comes from `MuxOutcome.lost_bytes`.
        let lost_bytes = self.atomics.input_lost_bytes.load(Ordering::Relaxed);
        let lost_video_secs = if self.ui.title_bytes_per_sec > 0.0 {
            lost_bytes as f64 / self.ui.title_bytes_per_sec
        } else {
            0.0
        };
        push_mux_state(
            &self.ui,
            &self.atomics,
            pct,
            speed,
            eta,
            bytes_done,
            lost_video_secs,
            skip_errors,
        );
    }

    fn on_sector_skipped(&self, lba: u32) {
        self.atomics
            .wd_last_frame
            .store(crate::server::util::epoch_secs(), Ordering::Relaxed);
        // Store the skipped LBA into `rip_last_lba` (UI last_sector/playhead)
        // and log the per-skip line — fires on the LIVE inline single-pass path
        // from `DiscStream::fill_extents`; `input_errors` bump surfaces the skip count.
        self.atomics
            .rip_last_lba
            .store(lba as u64, Ordering::Relaxed);
        self.atomics.input_errors.fetch_add(1, Ordering::Relaxed);
        crate::server::log::device_log(
            &self.ui.device,
            &format!("Sector {} skipped (zero-filled)", lba),
        );
    }

    fn on_batch_size_changed(&self, batch: u16, reason: libfreemkv::event::BatchSizeReason) {
        self.atomics
            .rip_current_batch
            .store(batch, Ordering::Relaxed);
        // Log the batch-change line; fires on the live inline single-pass path
        // from the adaptive sizer in `DiscStream::fill_extents`, keeping the
        // operator-facing record of when/why the read batch adapted.
        let label = match reason {
            libfreemkv::event::BatchSizeReason::Shrunk => "shrunk",
            libfreemkv::event::BatchSizeReason::Probed => "probed up",
        };
        crate::server::log::device_log(
            &self.ui.device,
            &format!("Batch size → {} ({})", batch, label),
        );
    }

    fn on_read_error(&self, _lba: u32) {
        self.atomics
            .wd_last_frame
            .store(crate::server::util::epoch_secs(), Ordering::Relaxed);
    }
}

/// Build a [`UiState`] from the orchestrator's [`MuxInputs`]. Shared by both
/// mux drivers ([`mux_iso`] / [`mux_live`]) for their progress bridges.
fn ui_state_from_inputs(inputs: &MuxInputs<'_>, total_bytes: u64) -> UiState {
    UiState {
        device: inputs.device.to_string(),
        display_name: inputs.display_name.clone(),
        disc_format: inputs.disc_format.clone(),
        tmdb_title: inputs.tmdb_title.clone(),
        tmdb_year: inputs.tmdb_year,
        tmdb_poster: inputs.tmdb_poster.clone(),
        tmdb_overview: inputs.tmdb_overview.clone(),
        duration: inputs.duration.clone(),
        codecs: inputs.codecs.clone(),
        filename: inputs.filename.clone(),
        batch: inputs.batch,
        total_bytes,
        title_bytes_per_sec: inputs.title_bytes_per_sec,
        total_passes: inputs.total_passes,
        bytes_total_disc: inputs.bytes_total_disc,
        max_retries: inputs.max_retries,
        bytes_unreadable_at_mux: inputs.bytes_unreadable_at_mux,
        sweep_damage: inputs.sweep_damage.clone(),
    }
}

// The ONE wording for "this mux completed, and the file still does not match the pre-mux plan"
// — `map_iso_mux_outcome` is the one emitter so this wording doesn't diverge from `rip_disc`'s
// copy.
fn undelivered_streams_note(streams: &[usize]) -> String {
    format!(
        "Mux completed but {} stream(s) could not be delivered into the output \
         (streams={:?}) — the file does not match the pre-mux plan",
        streams.len(),
        streams
    )
}

// Map a mux result into autorip's `MuxOutcome` + staging decisions, preserving the
// pre-migration Err classification.
#[allow(clippy::too_many_arguments)]
fn map_iso_mux_outcome(
    result: std::io::Result<libfreemkv::MuxOutcome>,
    opened: bool,
    device: &str,
    title_bytes_per_sec: f64,
    start: Instant,
    partial_bytes: u64,
    input_errors: u32,
) -> std::io::Result<MuxOutcome> {
    let elapsed_secs = start.elapsed().as_secs_f64();
    let speed_of = |bytes: u64| {
        if elapsed_secs > 0.0 {
            bytes as f64 / BYTES_PER_MIB / elapsed_secs
        } else {
            0.0
        }
    };
    let lost_secs = |lost_bytes: u64| {
        if title_bytes_per_sec > 0.0 {
            lost_bytes as f64 / title_bytes_per_sec
        } else {
            0.0
        }
    };
    match result {
        Ok(o) if o.completed => {
            // Non-empty `undelivered_streams` means a lossy-but-successful
            // export despite `completed == true`. Dormant today (only the
            // `mp4://` sink populates it), but log loudly the moment it's not.
            if !o.undelivered_streams.is_empty() {
                crate::server::log::device_log(
                    device,
                    &undelivered_streams_note(&o.undelivered_streams),
                );
            }
            Ok(MuxOutcome {
                completed: true,
                bytes_done: o.bytes_written,
                elapsed_secs,
                speed_mbs: speed_of(o.bytes_written),
                errors: u32::try_from(o.errors).unwrap_or(u32::MAX),
                lost_video_secs: lost_secs(o.lost_bytes),
                output_opened: true,
                finalize_error: None,
                read_error: None,
            })
        }
        Ok(o) => {
            // A clean operator stop (halt) or a join-timeout wedge: resumable,
            // no error marker — the orchestrator's "stopped" path handles it.
            crate::server::log::device_log(
                device,
                "Mux did not complete (operator stop or wedge) — staging preserved for resume",
            );
            Ok(MuxOutcome {
                completed: false,
                bytes_done: o.bytes_written,
                elapsed_secs,
                speed_mbs: speed_of(o.bytes_written),
                errors: u32::try_from(o.errors).unwrap_or(u32::MAX),
                lost_video_secs: lost_secs(o.lost_bytes),
                output_opened: o.output_opened,
                finalize_error: None,
                read_error: None,
            })
        }
        Err(e) => {
            // Propagate what the call site classifies itself: a Stop (staging kept) and a
            // key refusal (E7034 "insert the disc", else a key deferral), never damage.
            if super::is_halt_error(&e) || super::io_key_refusal(&e).is_some() {
                return Err(e);
            }
            if !opened {
                // Header-phase / construction failure with no sink: structural,
                // quarantine via the orchestrator's `!output_opened` path.
                let msg =
                    format!("Header resolution or mux setup failed before output opened ({e})");
                crate::server::log::device_log(device, &format!("Mux failed: {msg}"));
                return Ok(MuxOutcome {
                    completed: false,
                    bytes_done: 0,
                    elapsed_secs,
                    speed_mbs: 0.0,
                    errors: input_errors,
                    lost_video_secs: 0.0,
                    output_opened: false,
                    finalize_error: Some(msg),
                    read_error: None,
                });
            }
            let code = freemkv_engine::error_code(&e);
            if code == Some(libfreemkv::error::E_NO_STREAMS) {
                // Empty / undecryptable output — structural, quarantine.
                let msg = "mux produced no frames (empty/undecryptable output)".to_string();
                crate::server::log::device_log(
                    device,
                    "Mux produced no frames/bytes — refusing to mark complete (empty/undecryptable output)",
                );
                return Ok(MuxOutcome {
                    completed: false,
                    bytes_done: partial_bytes,
                    elapsed_secs,
                    speed_mbs: speed_of(partial_bytes),
                    errors: input_errors,
                    lost_video_secs: 0.0,
                    output_opened: true,
                    finalize_error: Some(msg),
                    read_error: None,
                });
            }
            match code {
                Some(c)
                    if c != libfreemkv::error::E_IO_ERROR
                        && c != libfreemkv::error::E_MKV_INVALID =>
                {
                    // A coded read fault surfaced mid-mux (DiscRead / Decrypt /
                    // Mapfile …). The MKV is truncated but the disc stays
                    // resumable — same as the old producer-read-error path.
                    let cause = producer_read_error_cause(&e);
                    crate::server::log::device_log(
                        device,
                        &format!("Mux incomplete: read error mid-stream — {cause} (MKV truncated)"),
                    );
                    Ok(MuxOutcome {
                        completed: false,
                        bytes_done: partial_bytes,
                        elapsed_secs,
                        speed_mbs: speed_of(partial_bytes),
                        errors: input_errors,
                        lost_video_secs: 0.0,
                        output_opened: true,
                        finalize_error: None,
                        read_error: Some(cause),
                    })
                }
                _ => {
                    // A finalize / IO error (output.finish() failed, write
                    // error, unseekable MKV): structural, quarantine.
                    let msg = format!("{e}");
                    crate::server::log::device_log(device, &format!("Mux pipeline failed: {msg}"));
                    Ok(MuxOutcome {
                        completed: false,
                        bytes_done: partial_bytes,
                        elapsed_secs,
                        speed_mbs: speed_of(partial_bytes),
                        errors: input_errors,
                        lost_video_secs: 0.0,
                        output_opened: true,
                        finalize_error: Some(msg),
                        read_error: None,
                    })
                }
            }
        }
    }
}

// The error a title-less `RipOutcome` stands for: its code, a disc-level no-key (E7022),
// or a Stop.
fn outcome_error(outcome: &freemkv_engine::RipOutcome) -> std::io::Error {
    match outcome {
        // `E<code>: <data>`, the library's own error form, so `error_code` still reads it.
        freemkv_engine::RipOutcome::Failed {
            code: Some(code),
            data,
            ..
        } if !data.is_empty() => std::io::Error::other(format!("E{code}: {data}")),
        freemkv_engine::RipOutcome::Failed {
            code: Some(code), ..
        } => std::io::Error::other(format!("E{code}")),
        freemkv_engine::RipOutcome::Failed { kind, data, .. } => std::io::Error::new(
            *kind,
            format!("image mux failed before any title {data}")
                .trim_end()
                .to_string(),
        ),
        freemkv_engine::RipOutcome::NoKey => {
            std::io::Error::other(format!("E{}", libfreemkv::error::E_NO_DISC_KEY))
        }
        _ => libfreemkv::Error::Halted.into(),
    }
}

// Run the ISO/multipass (and resume) mux through the engine's `mux_image_titles`; live
// single-pass sibling is `mux_live`. `Err` only for the two call-site classifications
// (halt, FMTS deferral); everything else maps into `MuxOutcome`.
pub(crate) fn mux_iso(
    inputs: MuxInputs<'_>,
    src: IsoMuxSource<'_>,
    atomics_in: MuxAtomics,
) -> std::io::Result<MuxOutcome> {
    tracing::info!(target: "autorip::mux", phase = "mux", "begin");
    struct MuxPhaseGuard(std::time::Instant);
    impl Drop for MuxPhaseGuard {
        fn drop(&mut self) {
            tracing::info!(
                target: "autorip::mux",
                phase = "mux",
                elapsed_ms = self.0.elapsed().as_millis() as u64,
                "end"
            );
        }
    }
    let _mux_phase_guard = MuxPhaseGuard(std::time::Instant::now());

    // ── Watchdog (identical spawn to `mux_live`) ─────────────────────────────
    let wd_active = Arc::new(AtomicBool::new(true));
    let _wd_guard = WatchdogGuard(wd_active.clone());
    let wd_bytes = atomics_in.wd_bytes.clone();
    spawn_mux_watchdog(
        &inputs,
        wd_active.clone(),
        atomics_in.wd_last_frame.clone(),
        wd_bytes.clone(),
    );

    // Same per-device Halt the orchestrator threaded through sweep/patch and
    // the `/api/stop` handler cancels. Absent-token fallback = never-cancelled.
    let halt_token = device_halt(inputs.device).unwrap_or_default();

    // Progress denominator: caller's `total_bytes`, else the title's size.
    let total_bytes = if inputs.total_bytes > 0 {
        inputs.total_bytes
    } else {
        src.image
            .disc
            .titles
            .get(src.title_index)
            .map_or(0, |t| t.size_bytes)
    };

    let shared = SharedAtomics {
        latest_bytes_read: atomics_in.latest_bytes_read.clone(),
        rip_last_lba: atomics_in.rip_last_lba.clone(),
        rip_current_batch: atomics_in.rip_current_batch.clone(),
        wd_last_frame: atomics_in.wd_last_frame.clone(),
        wd_bytes: wd_bytes.clone(),
        input_errors: atomics_in.input_errors.clone(),
        input_lost_bytes: Arc::new(AtomicU64::new(0)),
    };
    let start = Instant::now();
    let sink = EngineMuxSink::new(
        ui_state_from_inputs(&inputs, total_bytes),
        shared,
        halt_token,
    );

    // One title, every stream, decrypted; a stub is fatal, never skipped.
    let mut plan = freemkv_engine::MuxPlan::new(vec![src.title_index]);
    plan.explicit_selection = true;
    plan.mux.batch_sectors = inputs.batch;

    crate::server::log::device_log(
        inputs.device,
        &format!("Opening output: {}", inputs.dest_url),
    );
    let dest = inputs.dest_url.clone();
    let outcome = freemkv_engine::mux_image_titles(src.image, &plan, &|_| dest.clone(), &sink);

    let result = sink.take_result(&outcome);
    let opened = sink.opened.load(Ordering::Relaxed);
    let partial_bytes = wd_bytes.load(Ordering::Relaxed);
    let final_errors = atomics_in.input_errors.load(Ordering::Relaxed);
    map_iso_mux_outcome(
        result,
        opened,
        inputs.device,
        inputs.title_bytes_per_sec,
        start,
        partial_bytes,
        final_errors,
    )
}

// ── LIVE single-pass mux via libfreemkv's `mux_stream` ───────────────────────
// The live path (max_retries==0) uses the inline `DiscStream`, not the
// prefetch highway.
pub(crate) struct LiveMuxSource {
    /// The raw live-drive sector source (`session.drive` boxed). `mux_stream`
    /// moves it into `DiscStream::new` (whose reader param is exactly
    /// `Box<dyn SectorSource>`) — the inline reader, never the highway wrapper.
    pub(crate) reader: Box<dyn libfreemkv::SectorSource>,
    /// The scanned title to mux off the live drive.
    pub(crate) title: libfreemkv::DiscTitle,
    /// Container format (TS vs PS demux selection).
    pub(crate) format: libfreemkv::ContentFormat,
    /// The rip's up-front key set (KU §2.1): its keys, its FMTS forensic map and the
    /// on-arrival proof. `None` (or a non-AACS set) decrypts no AACS; a DVD cracks its CSS
    /// title key in the stream.
    pub(crate) keys: Option<libfreemkv::keys::ResolvedKeySet>,
    /// Skip-past-read-errors (zero-fill + continue) — wired onto
    /// `DiscStream::skip_errors` (was `on_read_error == "skip"`).
    pub(crate) skip_errors: bool,
}

// Run the LIVE single-pass mux via `libfreemkv::mux_stream` on the inline `DiscStream`, with
// the same watchdog, UI push and outcome mapping as `mux_iso`.
pub(crate) fn mux_live(
    inputs: MuxInputs<'_>,
    src: LiveMuxSource,
    atomics_in: MuxAtomics,
) -> std::io::Result<MuxOutcome> {
    tracing::info!(target: "autorip::mux", phase = "mux", "begin");
    struct MuxPhaseGuard(std::time::Instant);
    impl Drop for MuxPhaseGuard {
        fn drop(&mut self) {
            tracing::info!(
                target: "autorip::mux",
                phase = "mux",
                elapsed_ms = self.0.elapsed().as_millis() as u64,
                "end"
            );
        }
    }
    let _mux_phase_guard = MuxPhaseGuard(std::time::Instant::now());

    // ── Watchdog (identical spawn to `mux_iso` / `run_mux`) ──────────────────
    let wd_active = Arc::new(AtomicBool::new(true));
    let _wd_guard = WatchdogGuard(wd_active.clone());
    let wd_bytes = atomics_in.wd_bytes.clone();
    spawn_mux_watchdog(
        &inputs,
        wd_active.clone(),
        atomics_in.wd_last_frame.clone(),
        wd_bytes.clone(),
    );

    // Same per-device Halt the orchestrator threads through sweep/patch and
    // `/api/stop` cancels; absent-token fallback = never-cancelled. Covers the
    // CSS key setup that runs at `DiscStream` construction.
    let halt_token = device_halt(inputs.device).unwrap_or_default();

    // Progress denominator: caller's `total_bytes` (the single-pass extent sum
    // from `mux_progress_denominator`), else the title's size.
    let total_bytes = if inputs.total_bytes > 0 {
        inputs.total_bytes
    } else {
        src.title.size_bytes
    };

    // The events bridge shares the orchestrator's watchdog/UI atomics;
    // `on_sector_skipped`/`on_batch_size_changed` fire here (from
    // `DiscStream::fill_extents`) and route through as usual.
    let shared = SharedAtomics {
        latest_bytes_read: atomics_in.latest_bytes_read.clone(),
        rip_last_lba: atomics_in.rip_last_lba.clone(),
        rip_current_batch: atomics_in.rip_current_batch.clone(),
        wd_last_frame: atomics_in.wd_last_frame.clone(),
        wd_bytes: wd_bytes.clone(),
        input_errors: atomics_in.input_errors.clone(),
        // Live loss during the run isn't carried per-unit by the reader events;
        // the AUTHORITATIVE lost total is taken from `MuxOutcome.lost_bytes` at
        // the end (see `map_iso_mux_outcome`).
        input_lost_bytes: Arc::new(AtomicU64::new(0)),
    };
    let start = Instant::now();
    let events = Arc::new(AutoripMuxEvents {
        ui: ui_state_from_inputs(&inputs, total_bytes),
        atomics: shared,
        progress: Mutex::new(freemkv_engine::SpeedEstimator::new()),
        last_update: Mutex::new(start),
        last_log: Mutex::new(start),
        opened: AtomicBool::new(false),
    });

    // Single-pass never previews ciphertext (`raw = false`) — matches the
    // pre-migration `DiscStream::new(.., false, ..)`.
    let opts = libfreemkv::MuxOptions {
        skip_errors: src.skip_errors,
        batch_sectors: inputs.batch,
        raw: false,
        selection: Default::default(),
        // No per-frame send deadline (T27): a slow sink blocks; Stop and the hard
        // watchdog (HARD_WATCHDOG_STALL_SECS) are the bound.
        ..Default::default()
    };

    crate::server::log::device_log(
        inputs.device,
        &format!("Opening output: {}", inputs.dest_url),
    );
    let source = libfreemkv::MuxSource::Live {
        reader: src.reader,
        title: src.title,
        format: src.format,
    };

    let result = libfreemkv::mux_with_keys(
        source,
        src.keys.as_ref(),
        &inputs.dest_url,
        &opts,
        &halt_token,
        events.clone() as Arc<dyn libfreemkv::MuxEvents>,
    );

    let opened = events.opened.load(Ordering::Relaxed);
    let partial_bytes = wd_bytes.load(Ordering::Relaxed);
    let final_errors = atomics_in.input_errors.load(Ordering::Relaxed);
    map_iso_mux_outcome(
        result,
        opened,
        inputs.device,
        inputs.title_bytes_per_sec,
        start,
        partial_bytes,
        final_errors,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The hard escalation must bump through the bounded production helper
    // (tests/watchdog.rs covers its timeout) before `exit(1)`, never inline.
    #[test]
    fn hard_watchdog_bumps_via_the_bounded_helper_before_exit() {
        let src = crate::server::util::source_lf(include_str!("mux.rs"));
        let start = src
            .find("fn spawn_mux_watchdog(")
            .expect("watchdog spawner");
        let body = &src[start..];
        let esc = body
            .find("if stall_secs >= HARD_WATCHDOG_STALL_SECS {")
            .expect("hard escalation branch");
        let exit = esc + body[esc..].find("std::process::exit(1)").expect("exit(1)");
        let region = &body[esc..exit];
        assert!(
            region.contains("watchdog_bump_restart_count(&wd_device, &wd_staging_disc_dir)"),
            "the hard watchdog must call watchdog_bump_restart_count before exit(1)"
        );
        assert!(
            !region.contains("increment_restart_count("),
            "the counter bump must not be inlined (unbounded) in the escalation"
        );
    }

    const DISC: u64 = 60_000_000_000; // 60 GB stand-in for a UHD

    // Regression: a hard producer read error must surface the SPECIFIC coded cause, not a
    // generic truncation string, so an operator sees the real fault (decrypt / DiscRead / AACS)
    // in `last_error`.
    #[test]
    fn producer_read_error_cause_preserves_coded_root_cause() {
        // A decrypt failure manifesting mid-stream.
        let decrypt_io: std::io::Error = libfreemkv::Error::DecryptFailed.into();
        let decrypt_code = libfreemkv::Error::DecryptFailed.code();
        let cause = producer_read_error_cause(&decrypt_io);
        // The annotated parenthetical form must actually be emitted — not just
        // an incidental `E####` in the message tail (guards the dead `else`
        // branch the code-extraction round-trip used to leave unreachable).
        assert!(
            cause.contains(&format!("(E{decrypt_code})")),
            "decrypt cause must name the coded fault in the annotation, got: {cause}"
        );
        assert!(cause.contains("read error mid-stream"), "got: {cause}");

        // A coded disc read error (the genuine bad-sector / drive fault).
        let disc_err = libfreemkv::Error::DiscRead {
            sector: 12345,
            status: None,
            sense: None,
        };
        let disc_code = disc_err.code();
        let disc_io: std::io::Error = disc_err.into();
        let cause = producer_read_error_cause(&disc_io);
        assert!(
            cause.contains(&format!("(E{disc_code})")),
            "disc-read cause must name the coded fault in the annotation, got: {cause}"
        );
    }

    // Regression (rc4): the cause must carry an English description, not a bare duplicated
    // `E####` (was `read error mid-stream (E7013): E7013`).
    #[test]
    fn producer_read_error_cause_carries_english_label() {
        let decrypt_io: std::io::Error = libfreemkv::Error::DecryptFailed.into();
        let decrypt_code = libfreemkv::Error::DecryptFailed.code();
        let cause = producer_read_error_cause(&decrypt_io);
        assert!(
            cause.contains("decryption failed"),
            "decrypt cause must read in English, got: {cause}"
        );
        // The bare code must not appear as the trailing description (the
        // original leaked `(E7013): E7013` defect).
        assert!(
            !cause.ends_with(&format!("E{decrypt_code}")),
            "cause must not end with a bare duplicated code, got: {cause}"
        );
        assert!(
            !cause.contains(&format!("): E{decrypt_code}")),
            "cause must not render the code as its own description, got: {cause}"
        );

        // A coded disc-read fault gets its English label too.
        let disc_io: std::io::Error = libfreemkv::Error::DiscRead {
            sector: 42,
            status: None,
            sense: None,
        }
        .into();
        let cause = producer_read_error_cause(&disc_io);
        assert!(
            cause.contains("disc read error"),
            "disc-read cause must read in English, got: {cause}"
        );
    }

    /// A plain (non-coded) io error must NOT get a spurious `E####`
    /// numeric prefix — its message round-trips to the generic IoError
    /// code, so only the `{e}` tail describes it.
    #[test]
    fn producer_read_error_cause_handles_plain_io_error() {
        let plain = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read");
        let cause = producer_read_error_cause(&plain);
        assert!(cause.contains("short read"), "got: {cause}");
        assert!(
            !cause.contains(&format!("(E{})", libfreemkv::error::E_IO_ERROR)),
            "plain io error must not carry a synthetic code prefix, got: {cause}"
        );
        // No parenthetical annotation at all for a non-coded message.
        assert!(
            !cause.contains("(E"),
            "plain io error must not carry any code annotation, got: {cause}"
        );

        // A coded error that maps to the generic IoError code must also not
        // gain a spurious `(E5000)` annotation — only its tail names it.
        let io_coded: std::io::Error =
            libfreemkv::Error::from(std::io::Error::other("boom")).into();
        let cause = producer_read_error_cause(&io_coded);
        assert!(
            !cause.contains(&format!("(E{})", libfreemkv::error::E_IO_ERROR)),
            "IoError-coded fault must not carry the generic annotation, got: {cause}"
        );
    }

    // Clean disc: retry term vanishes, total_work reduces to 2x capacity, so
    // mux opens at exactly 50% and climbs linearly to 100%.
    #[test]
    fn clean_disc_mux_opens_at_50_percent() {
        // max_retries planned 5, but bytes_unreadable=0 → retries
        // contribute nothing whether 0 or 5 actually ran.
        assert_eq!(total_pct_byte_weight(DISC, 5, 0, 0), 50);
        assert_eq!(total_pct_byte_weight(DISC, 5, 0, 50), 75);
        assert_eq!(total_pct_byte_weight(DISC, 5, 0, 100), 100);
        // Same disc, max_retries planned 0 (couldn't have happened
        // here since multipass implies max_retries > 0, but the
        // helper falls through to direct-mode behaviour anyway).
        assert_eq!(total_pct_byte_weight(DISC, 0, 0, 50), 50);
    }

    /// Damaged disc with residual `bytes_unreadable`: retry term
    /// inflates the denominator, mux opens lower than 50% because
    /// the rip "did more total work than just sweep+mux."
    #[test]
    fn damaged_disc_mux_opens_below_50_percent() {
        // 1 GB unreadable, max_retries=5 → retry term = 5 GB.
        // total_work = 60 + 5 + 60 = 125 GB.
        // mux start: total_done = 60 + 5 + 0 = 65. 65/125 = 52%.
        assert_eq!(total_pct_byte_weight(DISC, 5, 1_000_000_000, 0), 52);
        // mux halfway: total_done = 60 + 5 + 30 = 95. 95/125 = 76%.
        assert_eq!(total_pct_byte_weight(DISC, 5, 1_000_000_000, 50), 76);
        // mux done: 100.
        assert_eq!(total_pct_byte_weight(DISC, 5, 1_000_000_000, 100), 100);
    }

    /// Direct-mux / single-pass mode (`max_retries == 0`): there are
    /// no separate phases — total tracks current 1:1.
    #[test]
    fn direct_mode_passthrough() {
        assert_eq!(total_pct_byte_weight(DISC, 0, 0, 0), 0);
        assert_eq!(total_pct_byte_weight(DISC, 0, 0, 42), 42);
        assert_eq!(total_pct_byte_weight(DISC, 0, 0, 100), 100);
    }

    /// Bound + edge cases: zero inputs, overshoot.
    #[test]
    fn edge_cases() {
        // Zero capacity (drive read failed) → fall through to mux pct.
        assert_eq!(total_pct_byte_weight(0, 5, 0, 73), 73);
        // pct overshoot doesn't push total past 100.
        assert_eq!(total_pct_byte_weight(DISC, 5, 0, 200), 100);
        assert_eq!(total_pct_byte_weight(DISC, 5, 1_000_000_000, 200), 100);
    }

    // ── sweep_damage snapshot carry-forward (telemetry audit Fix 1) ── Verify
    // `SweepDamageSnapshot` fields survive the `UiState` round-trip into
    // `push_state`'s `RipState`, by replicating its selection logic.
    #[test]
    fn sweep_damage_snapshot_non_zero_overrides_default() {
        // Simulate the logic inside push_state for errors and lost_video_secs.
        let snapshot_errors: u32 = 42;
        let snapshot_total_lost_ms: f64 = 3700.0;
        let live_errors: u32 = 0; // typical during ISO mux — no demux skips
        let live_lost_secs: f64 = 0.0;

        // Replicate the selection logic from push_state.
        let final_errors = if snapshot_errors > 0 {
            snapshot_errors
        } else {
            live_errors
        };
        let final_lost_secs = if snapshot_total_lost_ms > 0.0 {
            snapshot_total_lost_ms / MILLIS_PER_SEC
        } else {
            live_lost_secs
        };

        assert_eq!(
            final_errors, 42,
            "non-zero sweep snapshot errors must survive into push_state"
        );
        assert!(
            (final_lost_secs - 3.7).abs() < 0.001,
            "non-zero sweep snapshot total_lost_ms must survive as lost_video_secs"
        );
    }

    /// When the sweep was clean (zero errors, zero lost ms), the live mux
    /// counters should be used — not the zero snapshot values.
    #[test]
    fn sweep_damage_snapshot_zero_passes_through_live_counters() {
        let snapshot_errors: u32 = 0;
        let snapshot_total_lost_ms: f64 = 0.0;
        let live_errors: u32 = 5;
        let live_lost_secs: f64 = 0.25;

        let final_errors = if snapshot_errors > 0 {
            snapshot_errors
        } else {
            live_errors
        };
        let final_lost_secs = if snapshot_total_lost_ms > 0.0 {
            snapshot_total_lost_ms / MILLIS_PER_SEC
        } else {
            live_lost_secs
        };

        assert_eq!(
            final_errors, 5,
            "zero-snapshot must fall through to live errors"
        );
        assert!(
            (final_lost_secs - 0.25).abs() < 0.001,
            "zero-snapshot must fall through to live lost_video_secs"
        );
    }

    // ── resume progress starts at >0 (telemetry audit Fix 2) ── When
    // max_retries > 0, a resumed rip (mux_pct=0) opens above 0% since the
    // helper credits the already-completed sweep.
    #[test]
    fn resume_progress_starts_above_zero_when_max_retries_nonzero() {
        // Clean disc (bytes_unreadable=0, retry term vanishes): total_work =
        // 2×cap. At mux start (mux_pct=0), total_done = cap, so total_pct =
        // cap / (2*cap) * 100 = 50%.
        let pct = total_pct_byte_weight(DISC, 3, 0, 0);
        assert_eq!(
            pct, 50,
            "resume with max_retries=3 and clean disc should open at 50%, not 0%"
        );
    }

    // max_retries=0 falls through to mux_pct directly (correct for
    // single-pass/direct mode) — guard against accidentally changing it.
    #[test]
    fn direct_mode_progress_matches_mux_pct() {
        // max_retries=0 → direct-mode passthrough: total_pct == mux_pct.
        assert_eq!(total_pct_byte_weight(DISC, 0, 0, 0), 0);
        assert_eq!(total_pct_byte_weight(DISC, 0, 0, 50), 50);
        assert_eq!(total_pct_byte_weight(DISC, 0, 0, 100), 100);
    }

    // ── AutoripMuxEvents bridge feeds the watchdog byte atomics ──────────────

    fn test_shared_atomics() -> (
        SharedAtomics,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    ) {
        let wd_bytes = Arc::new(AtomicU64::new(0));
        let wd_last_frame = Arc::new(AtomicU64::new(0));
        let latest_bytes_read = Arc::new(AtomicU64::new(0));
        let atomics = SharedAtomics {
            latest_bytes_read: latest_bytes_read.clone(),
            rip_last_lba: Arc::new(AtomicU64::new(0)),
            rip_current_batch: Arc::new(AtomicU16::new(0)),
            wd_last_frame: wd_last_frame.clone(),
            wd_bytes: wd_bytes.clone(),
            input_errors: Arc::new(AtomicU32::new(0)),
            input_lost_bytes: Arc::new(AtomicU64::new(0)),
        };
        (atomics, wd_bytes, wd_last_frame, latest_bytes_read)
    }

    fn test_ui_state() -> UiState {
        UiState {
            device: "sr-test".to_string(),
            display_name: String::new(),
            disc_format: String::new(),
            tmdb_title: String::new(),
            tmdb_year: 0,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            duration: String::new(),
            codecs: String::new(),
            filename: String::new(),
            batch: 0,
            total_bytes: 1_000_000,
            title_bytes_per_sec: 0.0,
            total_passes: 0,
            bytes_total_disc: 0,
            max_retries: 0,
            bytes_unreadable_at_mux: 0,
            sweep_damage: SweepDamageSnapshot::default(),
        }
    }

    // `push_mux_state` is the only writer of live per-frame `RipState` during mux; a mutant
    // that reverts status/disc_present to defaults would make a busy device look idle.
    #[test]
    fn push_mux_state_reports_ripping_and_disc_present() {
        let device = "push_mux_state_test_device";
        let mut ui = test_ui_state();
        ui.device = device.to_string();
        let (atomics, ..) = test_shared_atomics();

        push_mux_state(&ui, &atomics, 10, 5.0, "1:00".into(), 1000, 0.0, 0);

        let rs = super::super::STATE
            .lock()
            .unwrap()
            .get(device)
            .cloned()
            .expect("push_mux_state must write a STATE entry for the device");
        assert_eq!(rs.status, "ripping");
        assert!(
            rs.disc_present,
            "a live mux tick must report disc_present=true"
        );
        super::super::STATE.lock().unwrap().remove(device);
    }

    // THE watchdog preservation check: `on_write_progress` must feed `wd_bytes`/`wd_last_frame`
    // even on the throttled early-return path, so a healthy mux never false-escalates.
    #[test]
    fn autorip_mux_events_feed_watchdog_byte_atomic() {
        use libfreemkv::MuxEvents;
        let (atomics, wd_bytes, wd_last_frame, latest_bytes_read) = test_shared_atomics();
        let events = AutoripMuxEvents {
            ui: test_ui_state(),
            atomics,
            progress: Mutex::new(freemkv_engine::SpeedEstimator::new()),
            // `now` → the 1 s throttle fires and `on_write_progress` returns
            // early AFTER feeding the watchdog atomics: the feed must not be
            // gated behind the UI throttle.
            last_update: Mutex::new(Instant::now()),
            last_log: Mutex::new(Instant::now()),
            opened: AtomicBool::new(false),
        };

        // Reader side: feeds the UI read-ahead position + watchdog activity.
        events.on_read_progress(4096, 8192);
        assert_eq!(
            latest_bytes_read.load(Ordering::Relaxed),
            4096,
            "on_read_progress must feed latest_bytes_read"
        );
        assert!(
            wd_last_frame.load(Ordering::Relaxed) > 0,
            "on_read_progress must refresh wd_last_frame (watchdog activity)"
        );

        // Writer side (throttled): wd_bytes MUST still advance — this is the
        // load-bearing feed that keeps the hard watchdog from firing exit(1).
        events.on_write_progress(500_000, 1_000_000);
        assert_eq!(
            wd_bytes.load(Ordering::Relaxed),
            500_000,
            "on_write_progress must feed wd_bytes even on the throttled path"
        );
        assert!(
            wd_last_frame.load(Ordering::Relaxed) > 0,
            "on_write_progress must refresh wd_last_frame"
        );

        // The opened flag drives `output_opened` in the outcome mapping.
        assert!(!events.opened.load(Ordering::Relaxed));
        events.on_output_opened(&libfreemkv::DiscTitle::empty());
        assert!(
            events.opened.load(Ordering::Relaxed),
            "on_output_opened must set the opened flag"
        );
    }

    // Regression D: `on_sector_skipped` must store the skipped LBA into
    // `rip_last_lba` (the UI playhead), refresh watchdog activity, and bump
    // `input_errors`, matching the pre-refactor `make_stream_event_fn`.
    #[test]
    fn on_sector_skipped_stores_lba_into_rip_last_lba() {
        use libfreemkv::MuxEvents;
        let (atomics, _wd_bytes, wd_last_frame, _lbr) = test_shared_atomics();
        let rip_last_lba = atomics.rip_last_lba.clone();
        let input_errors = atomics.input_errors.clone();
        let events = AutoripMuxEvents {
            ui: test_ui_state(),
            atomics,
            progress: Mutex::new(freemkv_engine::SpeedEstimator::new()),
            last_update: Mutex::new(Instant::now()),
            last_log: Mutex::new(Instant::now()),
            opened: AtomicBool::new(false),
        };

        events.on_sector_skipped(4242);
        assert_eq!(
            rip_last_lba.load(Ordering::Relaxed),
            4242,
            "on_sector_skipped must store the skipped LBA into rip_last_lba \
             (the UI last_sector / playhead), matching make_stream_event_fn"
        );
        assert_eq!(
            input_errors.load(Ordering::Relaxed),
            1,
            "on_sector_skipped must still bump input_errors (additive)"
        );
        assert!(
            wd_last_frame.load(Ordering::Relaxed) > 0,
            "on_sector_skipped must refresh wd_last_frame (watchdog activity)"
        );

        // A later skip advances the playhead to the new LBA.
        events.on_sector_skipped(9001);
        assert_eq!(
            rip_last_lba.load(Ordering::Relaxed),
            9001,
            "a subsequent skip must move the playhead forward"
        );
        assert_eq!(input_errors.load(Ordering::Relaxed), 2);
    }

    // Regression D: `on_batch_size_changed` must store the new batch and emit
    // the batch-change device-log line `make_stream_event_fn` used to
    // produce; both reason variants must render without panicking.
    #[test]
    fn on_batch_size_changed_stores_batch_and_logs() {
        use libfreemkv::MuxEvents;
        let (atomics, ..) = test_shared_atomics();
        let rip_current_batch = atomics.rip_current_batch.clone();
        let events = AutoripMuxEvents {
            ui: test_ui_state(),
            atomics,
            progress: Mutex::new(freemkv_engine::SpeedEstimator::new()),
            last_update: Mutex::new(Instant::now()),
            last_log: Mutex::new(Instant::now()),
            opened: AtomicBool::new(false),
        };

        events.on_batch_size_changed(64, libfreemkv::event::BatchSizeReason::Shrunk);
        assert_eq!(
            rip_current_batch.load(Ordering::Relaxed),
            64,
            "on_batch_size_changed must store the new batch into rip_current_batch"
        );
        // Both variants must render (and log) without panicking.
        events.on_batch_size_changed(128, libfreemkv::event::BatchSizeReason::Probed);
        assert_eq!(rip_current_batch.load(Ordering::Relaxed), 128);
    }

    // `map_iso_mux_outcome` preserves the pre-migration Err classification:
    // halt/FMTS-missing -> Err; completed run -> `completed=true`; NoStreams
    // drain -> quarantine (`finalize_error=Some`, output opened).
    #[test]
    fn map_iso_mux_outcome_classifies_faithfully() {
        let start = Instant::now();
        // Completed run.
        let ok = map_iso_mux_outcome(
            Ok(libfreemkv::MuxOutcome {
                completed: true,
                output_opened: true,
                bytes_written: 1234,
                errors: 0,
                lost_bytes: 0,
                streams: 2,
                // Stream indices the sink accepted frames for but couldn't put
                // in the finished container. Empty here — clean completed run.
                undelivered_streams: Vec::new(),
            }),
            true,
            "sr-test",
            0.0,
            start,
            0,
            0,
        )
        .expect("completed run maps to Ok");
        assert!(ok.completed && ok.output_opened);
        assert_eq!(ok.bytes_done, 1234);

        // Ok(..) with completed=false — a clean stop or join-timeout wedge —
        // must NOT report as a finished mux: a mutant widening the
        // `Ok(o) if o.completed` guard would file a damaged rip as good (rule 1).
        let not_done = map_iso_mux_outcome(
            Ok(libfreemkv::MuxOutcome {
                completed: false,
                output_opened: true,
                bytes_written: 500,
                errors: 0,
                lost_bytes: 0,
                streams: 2,
                undelivered_streams: Vec::new(),
            }),
            true,
            "sr-test",
            0.0,
            start,
            0,
            0,
        )
        .expect("a non-completed Ok result still maps to Ok, just completed=false");
        assert!(
            !not_done.completed,
            "an Ok(..) result with completed=false must not be reported as a finished mux"
        );
        assert!(
            not_done.output_opened,
            "output_opened is carried through unchanged from the engine outcome"
        );
        assert_eq!(not_done.bytes_done, 500);

        // Halt during construction → propagated as Err for call-site handling.
        let halt_err: std::io::Error = libfreemkv::Error::Halted.into();
        assert!(
            map_iso_mux_outcome(Err(halt_err), false, "sr-test", 0.0, start, 0, 0).is_err(),
            "Halted must propagate as Err so the call site preserves staging"
        );

        // NoStreams (empty/undecryptable) with output opened → quarantine.
        let nostreams: std::io::Error = libfreemkv::Error::NoStreams.into();
        let mapped =
            map_iso_mux_outcome(Err(nostreams), true, "sr-test", 0.0, start, 0, 0).expect("mapped");
        assert!(!mapped.completed);
        assert!(mapped.output_opened);
        assert!(
            mapped.finalize_error.is_some(),
            "NoStreams must quarantine via finalize_error"
        );

        // Header-phase failure (no sink opened) → output_opened=false + finalize.
        let mkv_invalid: std::io::Error = libfreemkv::Error::MkvInvalid.into();
        let hdr = map_iso_mux_outcome(Err(mkv_invalid), false, "sr-test", 0.0, start, 0, 0)
            .expect("mapped");
        assert!(!hdr.output_opened);
        assert!(hdr.finalize_error.is_some());
    }

    // `map_iso_mux_outcome` must not drop `undelivered_streams` on the floor even when
    // `completed = true` — a lossy outcome is never silent.
    #[test]
    fn map_iso_mux_outcome_surfaces_undelivered_streams_on_a_completed_run() {
        // The per-device log ring is a process-global static shared by sibling
        // tests using `"sr-test"`; reading it back would make both assertions
        // below unsound (sibling lines, uncleared ring). Mint a unique name.
        let device = "sr_mux_undelivered_streams_note_test";
        let start = Instant::now();
        let lossy = map_iso_mux_outcome(
            Ok(libfreemkv::MuxOutcome {
                completed: true,
                output_opened: true,
                bytes_written: 1234,
                errors: 0,
                lost_bytes: 0,
                streams: 2,
                undelivered_streams: vec![1],
            }),
            true,
            device,
            0.0,
            start,
            0,
            0,
        )
        .expect("completed run maps to Ok");
        assert!(lossy.completed);
        // REPORTED, not merely carried: the device log is where this reaches
        // the operator. Exactly once — zero is a silent lossy "success", two
        // is the duplicate-wording bug this replaced.
        let logged = crate::server::log::get_device_log(device, 50);
        let notes: Vec<&String> = logged
            .iter()
            .filter(|l| l.contains("could not be delivered into the output"))
            .collect();
        assert_eq!(
            notes.len(),
            1,
            "a completed-but-lossy mux must report its undelivered streams \
             exactly once; got {logged:?}"
        );
        assert!(
            notes[0].contains("[1]"),
            "the note must name the streams that were dropped: {:?}",
            notes[0]
        );
    }

    // ONE event, ONE wording, ONE emitter — the note used to have two independently-maintained
    // spellings across mux.rs and mod.rs.
    #[test]
    fn the_undelivered_streams_note_has_a_single_emitter() {
        let mux_src = crate::server::util::source_lf(include_str!("mux.rs"));
        let mod_src = crate::server::util::source_lf(include_str!("mod.rs"));
        assert!(
            mux_src.contains("fn undelivered_streams_note("),
            "the note's wording must live in one shared function"
        );
        assert!(
            !mod_src.contains("stream(s) were not delivered"),
            "rip_disc must not carry a second, independently-worded copy of \
             the undelivered-streams note"
        );
        assert!(
            !mod_src.contains("stream(s) could not be delivered"),
            "rip_disc must not re-emit the note at all — map_iso_mux_outcome \
             already logged it for every outcome that can carry one"
        );
    }

    // ── EngineMuxSink: engine events → watchdog atomics + RipState ──────────

    fn engine_sink(
        device: &str,
    ) -> (
        EngineMuxSink,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    ) {
        let (atomics, wd_bytes, wd_last_frame, latest) = test_shared_atomics();
        let mut ui = test_ui_state();
        ui.device = device.to_string();
        let sink = EngineMuxSink::new(ui, atomics, libfreemkv::Halt::new());
        (sink, wd_bytes, wd_last_frame, latest)
    }

    fn tick(done: u64, total: u64, speed_bps: u64, eta: Option<u64>) -> freemkv_engine::Progress {
        freemkv_engine::Progress {
            pass: "mux".into(),
            bytes_done: done,
            bytes_total: total,
            speed_bps,
            eta_secs: eta,
            ..Default::default()
        }
    }

    fn state_of(device: &str) -> Option<super::super::RipState> {
        super::super::STATE.lock().unwrap().get(device).cloned()
    }

    // Every tick feeds the watchdog, throttled or not; the first one marks the output
    // opened and pushes no state yet (the 1 s cadence starts there).
    #[test]
    fn engine_sink_first_tick_opens_and_feeds_the_watchdog() {
        use freemkv_engine::Sink;
        let device = "engine_sink_first_tick";
        let (sink, wd_bytes, wd_last_frame, latest) = engine_sink(device);
        assert!(!sink.opened.load(Ordering::Relaxed));
        sink.progress(&tick(4096, 8192, 0, None));
        assert!(
            sink.opened.load(Ordering::Relaxed),
            "a written frame means opened"
        );
        assert_eq!(wd_bytes.load(Ordering::Relaxed), 4096);
        assert_eq!(latest.load(Ordering::Relaxed), 4096);
        assert!(wd_last_frame.load(Ordering::Relaxed) > 0);
        assert!(
            state_of(device).is_none(),
            "no state push on the opening tick"
        );
        // Throttled second tick: still feeds the watchdog, still no push.
        sink.progress(&tick(6000, 8192, 0, None));
        assert_eq!(wd_bytes.load(Ordering::Relaxed), 6000);
        assert!(state_of(device).is_none());
    }

    // Past the throttle, the engine's own progress (bytes, speed, ETA) is what lands
    // in `/api/state` — never re-derived here.
    #[test]
    fn engine_sink_tick_maps_engine_progress_onto_rip_state() {
        use freemkv_engine::Sink;
        let device = "engine_sink_tick_maps";
        let (sink, ..) = engine_sink(device);
        sink.progress(&tick(1, 1000, 0, None));
        *sink.last_update.lock().unwrap() = Instant::now() - Duration::from_secs(2);
        sink.progress(&tick(250, 1000, 2 * 1024 * 1024, Some(75)));
        let rs = state_of(device).expect("a tick past the throttle pushes state");
        assert_eq!(rs.status, "ripping");
        assert!(rs.disc_present);
        assert_eq!(rs.progress_pct, 25, "percent of the engine's bytes_total");
        assert_eq!(rs.speed_mbs, 2.0);
        assert_eq!(rs.eta, "1:15");
        assert_eq!(rs.pass_eta, "1:15");
        super::super::STATE.lock().unwrap().remove(device);
    }

    // No engine total yet: the percentage falls back to the caller's denominator.
    #[test]
    fn engine_sink_percent_falls_back_to_the_caller_total() {
        use freemkv_engine::Sink;
        let device = "engine_sink_pct_fallback";
        let (sink, ..) = engine_sink(device);
        sink.progress(&tick(1, 0, 0, None));
        *sink.last_update.lock().unwrap() = Instant::now() - Duration::from_secs(2);
        sink.progress(&tick(500_000, 0, 0, None));
        let rs = state_of(device).expect("state pushed");
        assert_eq!(rs.progress_pct, 50, "500k of the UI's 1M total");
        assert_eq!(rs.eta, "", "no engine estimate yet shows no ETA");
        super::super::STATE.lock().unwrap().remove(device);
    }

    // The title's result is what `map_iso_mux_outcome` classifies: kept from
    // `TitleDone` (error code intact), and a title never started reads as a halt.
    #[test]
    fn engine_sink_keeps_the_title_result() {
        use freemkv_engine::{Event, Sink};
        let (sink, ..) = engine_sink("engine_sink_result");
        let halted = sink
            .take_result(&freemkv_engine::RipOutcome::Halted)
            .expect_err("no TitleDone means stopped before start");
        assert!(super::super::is_halt_error(&halted));

        let done = libfreemkv::MuxOutcome {
            completed: true,
            output_opened: true,
            bytes_written: 1234,
            errors: 0,
            lost_bytes: 0,
            streams: 2,
            undelivered_streams: Vec::new(),
        };
        sink.event(&Event::TitleDone {
            idx: 0,
            dest: "mkv:///x.mkv",
            result: Ok(&done),
        });
        let ok = freemkv_engine::RipOutcome::Ok { titles_written: 1 };
        assert_eq!(sink.take_result(&ok).expect("ok").bytes_written, 1234);

        let fmts: std::io::Error = libfreemkv::Error::FmtsKeyMissing.into();
        sink.event(&Event::TitleDone {
            idx: 0,
            dest: "mkv:///x.mkv",
            result: Err(&fmts),
        });
        let err = sink.take_result(&ok).expect_err("err");
        assert!(super::super::is_fmts_key_missing_error(&err), "{err}");
    }

    // KU-E1: a key refusal is never a Stop. Before any title the loop's outcome carries it;
    // as a `TitleDone(Err)` with no output it reaches the call site as the key error
    // (E7034 "insert the disc", else a key deferral), not a quarantining setup failure.
    #[test]
    fn a_key_refusal_is_a_key_error_not_a_stop_or_damage() {
        use freemkv_engine::{Event, RipOutcome, Sink};
        let (sink, ..) = engine_sink("engine_sink_key_refusal");
        let refused = |code: u16| RipOutcome::Failed {
            title_index: 0,
            code: Some(code),
            kind: std::io::ErrorKind::Other,
            data: String::new(),
        };
        for code in [7022u16, 7026, 7032, 7034] {
            let e = sink.take_result(&refused(code)).expect_err("refused");
            assert!(!super::super::is_halt_error(&e), "E{code} is not a Stop");
            assert_eq!(super::super::io_key_refusal(&e), Some(code));
        }
        let with_data = RipOutcome::Failed {
            title_index: 0,
            code: Some(7022),
            kind: std::io::ErrorKind::Other,
            data: "abcd".into(),
        };
        let e = sink.take_result(&with_data).expect_err("refused");
        assert_eq!(super::super::io_key_refusal(&e), Some(7022));
        assert!(
            e.to_string().contains("abcd"),
            "the data reaches the message: {e}"
        );
        let no_key = sink.take_result(&RipOutcome::NoKey).expect_err("no key");
        assert_eq!(super::super::io_key_refusal(&no_key), Some(7022));

        for err in [
            libfreemkv::Error::AacsVidNeedsDisc,
            libfreemkv::Error::NoDiscKey {
                disc_hash: "ab".into(),
            },
            libfreemkv::Error::FmtsKeyMissing,
        ] {
            let code = err.code();
            let io: std::io::Error = err.into();
            sink.event(&Event::TitleDone {
                idx: 0,
                dest: "mkv:///x.mkv",
                result: Err(&io),
            });
            let result = sink.take_result(&RipOutcome::Halted);
            let Err(mapped) = map_iso_mux_outcome(result, false, "ku", 0.0, Instant::now(), 0, 0)
            else {
                panic!("E{code}: the call site classifies a key refusal");
            };
            assert_eq!(super::super::io_key_refusal(&mapped), Some(code));
        }
    }

    // `/api/stop` cancels the device's Halt; the engine polls it through the sink.
    #[test]
    fn engine_sink_cancels_on_the_device_halt() {
        use freemkv_engine::Sink;
        let (sink, ..) = engine_sink("engine_sink_halt");
        assert!(!sink.should_cancel());
        sink.halt.cancel();
        assert!(sink.should_cancel());
    }

    #[test]
    fn fmt_eta_matches_the_dashboard_format() {
        assert_eq!(fmt_eta(0), "0:00");
        assert_eq!(fmt_eta(75), "1:15");
        assert_eq!(fmt_eta(3_725), "1:02:05");
        assert_eq!(fmt_eta(359_999), "99:59:59");
        assert_eq!(fmt_eta(360_000), "");
    }

    // ── mux_iso end to end through the engine (no media) ────────────────────

    // A keyed image of the KU fixture, repointed at `path`: the mux opens its own reader
    // there. The fixture's file is left in place for the test's lifetime.
    fn image_of(path: &str) -> freemkv_engine::OpenedImage {
        let fx = crate::ku_fixture::bd_image();
        let dir = tempfile::tempdir().unwrap().keep();
        let iso = fx.write(&dir, "fixture.iso");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let keys = crate::ku_fixture::holding(&calls, crate::ku_fixture::K1);
        let opts = freemkv_engine::OpenImageOptions::resolve(keys);
        let src = freemkv_engine::ImageSource::Iso(iso);
        let mut image = freemkv_engine::open_image_with(&src, opts).expect("the fixture opens");
        image.source = freemkv_engine::ImageSource::Iso(path.into());
        image
    }

    fn inputs_for<'a>(device: &'a str, dest: &std::path::Path) -> MuxInputs<'a> {
        MuxInputs {
            device,
            display_name: "Test".into(),
            disc_format: "bluray".into(),
            tmdb_title: String::new(),
            tmdb_year: 0,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            duration: String::new(),
            codecs: String::new(),
            filename: "Test.mkv".into(),
            total_bytes: 0,
            title_bytes_per_sec: 0.0,
            total_passes: 0,
            bytes_total_disc: 0,
            max_retries: 0,
            bytes_unreadable_at_mux: 0,
            dest_url: format!("mkv://{}", dest.display()),
            batch: 60,
            staging_disc_dir: dest.parent().unwrap().to_path_buf(),
            sweep_damage: SweepDamageSnapshot::default(),
        }
    }

    fn atomics() -> MuxAtomics {
        MuxAtomics {
            latest_bytes_read: Arc::new(AtomicU64::new(0)),
            rip_last_lba: Arc::new(AtomicU64::new(0)),
            rip_current_batch: Arc::new(AtomicU16::new(0)),
            wd_last_frame: Arc::new(AtomicU64::new(crate::server::util::epoch_secs())),
            wd_bytes: Arc::new(AtomicU64::new(0)),
            input_errors: Arc::new(AtomicU32::new(0)),
        }
    }

    // An image the engine cannot open fails before any output: a setup failure the
    // caller quarantines, with nothing left behind at the destination.
    #[test]
    fn mux_iso_maps_an_unopenable_image_to_a_setup_failure() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("Test.mkv");
        let image = image_of("/nonexistent/freemkv-server/none.iso");
        let out = mux_iso(
            inputs_for("mux_iso_unopenable", &dest),
            IsoMuxSource {
                image: &image,
                title_index: 0,
            },
            atomics(),
        )
        .expect("a setup failure maps into the outcome, not Err");
        assert!(!out.completed && !out.output_opened);
        let reason = out.finalize_error.expect("setup failure carries a reason");
        assert!(reason.contains("before output opened"), "{reason}");
        assert!(!dest.exists());
    }

    // A Stop registered before the mux starts is a halt: Err, so the caller keeps staging.
    #[test]
    fn mux_iso_honours_a_stop_pressed_before_it_starts() {
        let device = "mux_iso_prestopped";
        let halt = libfreemkv::Halt::new();
        halt.cancel();
        super::super::session::register_halt(device, halt);
        let dir = tempfile::tempdir().unwrap();
        let image = image_of("/nonexistent/freemkv-server/none.iso");
        let err = mux_iso(
            inputs_for(device, &dir.path().join("Test.mkv")),
            IsoMuxSource {
                image: &image,
                title_index: 0,
            },
            atomics(),
        )
        .err()
        .expect("a stopped mux returns the halt");
        assert!(super::super::is_halt_error(&err), "{err}");
        super::super::session::unregister_halt(device);
    }
}
