//! The server's side of the engine's recovery passes (one multipass implementation, the
//! engine's, used by the CLI, the app and the server). The host owns the held drive: it
//! brings it back after a USB bridge crash, un-wedges it before each patch pass, and keeps
//! the passes going past a failed or wedged pass (autorip 1.7.7). The sink narrates every
//! pass into the device log and the pass tiles, and measures the loss the abort gate
//! decides on from the promoted mapfile.

use super::session::{self, DriveSession, drop_session, rediscover_drive};
use super::state::{
    self, PassContext, PassProgressState, push_pass_state, set_pass_progress, update_state_with,
};
use super::{log_init_recovery_failure, open_drive_with_backoff};
use crate::server::util::{BYTES_PER_GIB, BYTES_PER_MIB, MILLIS_PER_SEC};
use freemkv_engine::RecoveryEvent;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// Pass 1 attempts before transport-failure recovery gives up.
pub(super) const MAX_PASS1_ATTEMPTS: u32 = 10;

/// The held drive the passes read, and what the server does between them.
pub(super) struct ServerPassHost<'a> {
    pub(super) device: &'a str,
    pub(super) device_path: &'a str,
    pub(super) session: &'a mut DriveSession,
    /// The device's halt flag (the drive's), which ends a recovery wait.
    pub(super) halt: Arc<AtomicBool>,
    pub(super) user_halt: Arc<AtomicBool>,
    pub(super) delay_secs: u64,
    /// The operator's Resume: the sweep resumes the mapfile from its first attempt.
    pub(super) resume: bool,
    /// Pass 1's attempt count so far.
    pub(super) attempt: u32,
    /// The sweep's transport recovery gave up, or its attempts ran out (the
    /// recovery-exhausted messages apply).
    pub(super) gave_up: bool,
    /// A Stop ended a transport recovery.
    pub(super) halted: bool,
}

impl freemkv_engine::PassHost for ServerPassHost<'_> {
    fn reader(&mut self) -> &mut dyn libfreemkv::SectorSource {
        &mut self.session.drive
    }

    fn resume_sweep(&self) -> Option<bool> {
        Some(self.resume)
    }

    fn sweep_attempt(&mut self, attempt: u32) -> bool {
        self.attempt = attempt;
        if attempt > MAX_PASS1_ATTEMPTS {
            crate::server::log::device_log(self.device, "Pass 1: max attempts reached");
            self.gave_up = true;
            return false;
        }
        true
    }

    // Pass 1 with transport-failure recovery: the Initio USB-SATA bridge crashes on damaged
    // sectors, causing a USB re-enumeration (sg device renumbers). Re-open the drive at its
    // new path; the engine sweeps again, resuming the mapfile.
    fn recover_transport(&mut self, attempt: u32, _e: &libfreemkv::Error) -> bool {
        let recovered = self.recover(attempt);
        self.gave_up = !recovered && !self.halted;
        recovered
    }

    // Un-wedge the drive in SOFTWARE before each retry pass: grinding a bad cluster leaves it
    // in a HARDWARE_ERROR wedge needing a power-cycle. spin_cycle() does that WITHOUT ejecting
    // (slot-loading drive).
    fn before_patch(&mut self, pass: u32) {
        if let Err(e) = self.session.drive.spin_cycle() {
            // spin_cycle's SCSI command failed (dead bus / file-backed resume). Fall back to
            // a short passive idle for SOME recovery time — a bridge transport fault
            // self-recovers in ~15s of idle.
            crate::server::log::device_log(
                self.device,
                &format!("drive spin-cycle before pass {pass} failed ({e}); settling 15 s instead"),
            );
            // Short idle in 1 s slices so a user halt stays responsive.
            for _ in 0..15 {
                if self.user_halt.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        } else {
            crate::server::log::device_log(
                self.device,
                &format!("drive spin-cycled (soft un-wedge, no eject) before pass {pass}"),
            );
        }
    }

    // A failed patch pass ends the retries; the rip continues with what was read.
    fn patch_failed(&mut self, pass: u32, e: &libfreemkv::Error) -> bool {
        // Categorize the failure for debugging
        let error_category = if e.code() == 4000 {
            "SCSI_ERROR"
        } else if e.code() >= 6000 && e.code() < 7000 {
            "DISC_READ_ERROR"
        } else if e.code() >= 1000 && e.code() < 2000 {
            "DEVICE_ERROR"
        } else {
            &format!("ERROR_CODE_{}", e.code())
        };

        let sense_info = e.scsi_sense().map(|s| {
            format!(
                "sense_key={:02x} ASC={:02x} ASCQ={:02x}",
                s.sense_key, s.asc, s.ascq
            )
        });

        if self.user_halt.load(Ordering::Relaxed) {
            crate::server::log::device_log(
                self.device,
                &format!(
                    "PASS {} CANCELLED: user halt category={} error_code={}",
                    pass,
                    error_category,
                    e.code()
                ),
            );

            if let Some(info) = sense_info {
                crate::server::log::device_log(self.device, &info);
            }
        } else {
            crate::server::log::device_log(
                self.device,
                &format!(
                    "PASS {} FAILED: strategy=patch_recovery category={} error_code={} {}",
                    pass,
                    error_category,
                    e.code(),
                    sense_info.unwrap_or_default()
                ),
            );

            // Log which recovery phase failed
            crate::server::log::device_log(
                self.device,
                &format!(
                    "STRATEGY_FAILURE: patch_recovery FAILED at disc.patch() with category={} (sense_key={:?}, ASC={:?})",
                    error_category,
                    e.scsi_sense().map(|s| s.sense_key),
                    e.scsi_sense().map(|s| s.asc)
                ),
            );

            // Provide actionable guidance based on error type
            if e.code() == 4000 && e.is_scsi_transport_failure() {
                crate::server::log::device_log(
                    self.device,
                    "ACTION_REQUIRED: Transport failure detected — USB bridge crashed. Eject disc and power-cycle drive before retrying.",
                );
            } else if e.code() >= 6000
                && e.scsi_sense()
                    .map(|s| s.is_hardware_error())
                    .unwrap_or(false)
            {
                crate::server::log::device_log(
                    self.device,
                    "ACTION_REQUIRED: Drive hardware error detected — drive may be failing. Consider replacing optical drive.",
                );
            } else if e.code() == 4000 && e.scsi_sense().map(|s| s.asc == 0x20).unwrap_or(false) {
                crate::server::log::device_log(
                    self.device,
                    "ACTION_REQUIRED: ILLEGAL REQUEST (ASC=0x20) — drive firmware wedged. Power-cycle USB drive to clear state.",
                );
            }
        }

        true
    }

    // A wedged patch pass counts like any other: the no-progress rule decides.
    fn continue_after_wedge(&mut self) -> bool {
        true
    }
}

impl ServerPassHost<'_> {
    // Drop the stale drive, wait for the USB re-enumeration, re-open the drive at its
    // original path or a shifted sg number. `false` when it cannot be brought back.
    fn recover(&mut self, attempt: u32) -> bool {
        // Transport failure — bridge crashed: drop the stale drive, wait for the USB
        // re-enumeration, re-open on the new path.
        crate::server::log::device_log(
            self.device,
            &format!(
                "Pass 1 attempt {attempt}: transport failure (bridge crash), waiting for USB re-enumeration"
            ),
        );
        drop_session(self.device);

        // Wait for USB re-enumeration (delay snapshotted at the top of
        // `rip_disc`), then re-discover the drive at its original path or a
        // shifted sg number. A Stop ends either wait.
        let recovery_halt = libfreemkv::Halt::from_arc(self.halt.clone());
        let new_path = if session::sleep_unless_halted(
            &recovery_halt,
            std::time::Duration::from_secs(self.delay_secs),
        ) {
            rediscover_drive(self.device, self.device_path, &recovery_halt)
        } else {
            None
        };
        if recovery_halt.is_cancelled() {
            crate::server::log::device_log(
                self.device,
                "Pass 1 cancelled (halt) during transport-failure recovery",
            );
            self.halted = true;
            return false;
        }
        match (new_path.as_deref(), &self.device_path) {
            (Some(p), _) if p != self.device_path => {
                crate::server::log::device_log(
                    self.device,
                    &format!(
                        "Pass 1 attempt {attempt}: drive rediscovered at {p} (original={}), attempting re-open",
                        self.device_path
                    ),
                );

                // Retry Drive::open with exponential backoff (firmware may not be ready yet).
                let mut drive =
                    match open_drive_with_backoff(self.device, attempt, p, self.delay_secs) {
                        Some(d) => d,
                        None => return false,
                    };

                if let Err(e) = drive.wait_ready() {
                    crate::server::log::device_log(
                        self.device,
                        &format!(
                            "Pass 1 attempt {attempt}: Drive::wait_ready({}) failed strategy=transport_failure_recovery error={} — recovery path exhausted",
                            p,
                            e.code()
                        ),
                    );

                    let failure_category = if e.code() == 4000 {
                        "SCSI_ERROR"
                    } else {
                        &format!("ERROR_CODE_{}", e.code())
                    };

                    crate::server::log::device_log(
                        self.device,
                        &format!(
                            "STRATEGY_FAILURE: transport_failure_recovery FAILED at Drive::wait_ready category={} error_code={}",
                            failure_category,
                            e.code()
                        ),
                    );

                    return false;
                }

                if let Err(e) = drive.init() {
                    crate::server::log::device_log(
                        self.device,
                        &format!(
                            "Pass 1 attempt {attempt}: Drive::init({}) failed strategy=transport_failure_recovery error={} sense_key={:?} ASC={:?} — recovery path exhausted",
                            p,
                            e.code(),
                            e.scsi_sense().map(|s| s.sense_key),
                            e.scsi_sense().map(|s| s.asc)
                        ),
                    );

                    log_init_recovery_failure(self.device, &e);

                    return false;
                }

                // Engage disc-type read mode before any read
                // (idempotent); mirrors scan_disc and the other
                // open paths, which all call probe_disc() after init().
                if let Err(e) = drive.probe_disc() {
                    tracing::warn!(device = %self.device, error = %e, "drive probe_disc failed (continuing)");
                }

                self.session.drive = drive;
                self.session.device_path = p.to_string();

                crate::server::log::device_log(
                    self.device,
                    &format!(
                        "PASS 1/{}: transport_failure_recovery SUCCESS — resuming from mapfile at {}",
                        attempt + 1,
                        p
                    ),
                );
            }

            (Some(p), _) if p == self.device_path => {
                crate::server::log::device_log(
                    self.device,
                    &format!(
                        "Pass 1 attempt {attempt}: drive still at original path {}, attempting re-open",
                        p
                    ),
                );

                // Retry Drive::open with exponential backoff (firmware
                // may not be ready yet) — same as the new-path arm, since
                // a same-sg re-enumeration leaves firmware just as cold.
                let mut drive =
                    match open_drive_with_backoff(self.device, attempt, p, self.delay_secs) {
                        Some(d) => d,
                        None => return false,
                    };

                if let Err(e) = drive.wait_ready() {
                    crate::server::log::device_log(
                        self.device,
                        &format!(
                            "Pass 1 attempt {attempt}: Drive::wait_ready({}) failed strategy=transport_failure_recovery error={} — recovery path exhausted",
                            p,
                            e.code()
                        ),
                    );

                    let failure_category = if e.code() == 4000 {
                        "SCSI_ERROR"
                    } else {
                        &format!("ERROR_CODE_{}", e.code())
                    };

                    crate::server::log::device_log(
                        self.device,
                        &format!(
                            "STRATEGY_FAILURE: transport_failure_recovery FAILED at Drive::wait_ready category={} error_code={}",
                            failure_category,
                            e.code()
                        ),
                    );

                    return false;
                }

                if let Err(e) = drive.init() {
                    crate::server::log::device_log(
                        self.device,
                        &format!(
                            "Pass 1 attempt {attempt}: Drive::init({}) failed strategy=transport_failure_recovery error={} sense_key={:?} ASC={:?} — recovery path exhausted",
                            p,
                            e.code(),
                            e.scsi_sense().map(|s| s.sense_key),
                            e.scsi_sense().map(|s| s.asc)
                        ),
                    );

                    // Same wedged-firmware diagnostic as the
                    // new-path arm: same-sg re-enumeration too
                    // means the firmware needs a power-cycle.
                    log_init_recovery_failure(self.device, &e);

                    return false;
                }

                // Engage disc-type read mode before any read
                // (idempotent); mirrors scan_disc and the other
                // open paths, which all call probe_disc() after init().
                if let Err(e) = drive.probe_disc() {
                    tracing::warn!(device = %self.device, error = %e, "drive probe_disc failed (continuing)");
                }

                self.session.drive = drive;
                self.session.device_path = p.to_string();

                crate::server::log::device_log(
                    self.device,
                    &format!(
                        "PASS 1/{}: transport_failure_recovery SUCCESS — resuming from mapfile at {}",
                        attempt + 1,
                        p
                    ),
                );
            }

            (None, _) => {
                crate::server::log::device_log(
                    self.device,
                    "Pass 1: could not re-discover drive after transport failure strategy=usb_re_enumeration FAILED",
                );

                // Log detailed breakdown of what was tried
                let sg_num = self
                    .device_path
                    .rsplit('/')
                    .next()
                    .and_then(|s| s.strip_prefix("sg").and_then(|n| n.parse::<i32>().ok()))
                    .unwrap_or(-1);

                crate::server::log::device_log(
                    self.device,
                    &format!(
                        "usb_re_enumeration strategy tried probe paths: sg{} (original), sg{}, sg{}, sg{}, sg{}, sg{}, sg{}",
                        sg_num,
                        sg_num - 1,
                        sg_num + 1,
                        sg_num - 2,
                        sg_num + 2,
                        sg_num - 3,
                        sg_num + 3
                    ),
                );

                crate::server::log::device_log(
                    self.device,
                    "STRATEGY_FAILURE: usb_re_enumeration FAILED — no valid drive path found after USB re-enumeration",
                );

                return false;
            }

            // Fallback for any other case (shouldn't happen but compiler requires exhaustiveness)
            _ => {
                crate::server::log::device_log(
                    self.device,
                    "STRATEGY_FAILURE: usb_re_enumeration FAILED — unexpected match state",
                );

                return false;
            }
        }
        true
    }
}

/// The passes' narration: the device log, the pass tiles, and the loss at the promotion.
pub(super) struct ServerPassSink<'a> {
    pub(super) ctx: &'a PassContext,
    pub(super) title: &'a libfreemkv::DiscTitle,
    pub(super) bps: f64,
    pub(super) total_passes: u8,
    pub(super) is_iso: bool,
    pub(super) mapfile: &'a std::path::Path,
    pub(super) user_halt: Arc<AtomicBool>,
    pass: AtomicU8,
    state: Mutex<PassProgressState>,
}

impl<'a> ServerPassSink<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        ctx: &'a PassContext,
        title: &'a libfreemkv::DiscTitle,
        bps: f64,
        total_passes: u8,
        is_iso: bool,
        mapfile: &'a std::path::Path,
        user_halt: Arc<AtomicBool>,
    ) -> Self {
        ServerPassSink {
            ctx,
            title,
            bps,
            total_passes,
            is_iso,
            mapfile,
            user_halt,
            pass: AtomicU8::new(1),
            state: Mutex::new(PassProgressState::new()),
        }
    }

    fn device(&self) -> &str {
        &self.ctx.device
    }

    fn reset_pass(&self, pass: u32) {
        self.pass
            .store(pass.min(u8::MAX as u32) as u8, Ordering::Relaxed);
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = PassProgressState::new();
    }

    // Runs every read block (~64 KB); throttled to a 250 ms UI push (libfreemkv's snapshot
    // republish cadence). Tracks the last sample for ETA.
    fn pass_progress(&self, p: &libfreemkv::progress::PassProgress) {
        {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            s.last_work_done = p.work_done;
            s.last_work_total = p.work_total;
            if s.last_update.elapsed().as_millis() < 250 {
                return;
            }
        }
        let pass = self.pass.load(Ordering::Relaxed);
        push_pass_state(self.ctx, p, self.bps, pass, self.total_passes, &self.state);
    }

    fn milestone(&self, e: &RecoveryEvent<'_>) {
        let device = self.device();
        let total_passes = self.total_passes;
        match *e {
            RecoveryEvent::PassStart { pass: 1, .. } => {
                // Pass 1: disc → ISO (fast sweep, skip-forward on failure).
                crate::server::log::device_log(
                    device,
                    &format!("Pass 1/{total_passes}: disc → ISO"),
                );
                set_pass_progress(self.ctx, 1, total_passes, 0, 0, 0);
                self.reset_pass(1);
            }
            RecoveryEvent::PassStart {
                pass,
                good,
                pending,
                unreadable,
            } => {
                let p = pass.min(u8::MAX as u32) as u8;
                // Flip the UI to the new pass BEFORE the settle, so the tile shows
                // "pass N · retrying · 0%" immediately instead of carrying the prior
                // pass's stale 99% through the drive settle.
                set_pass_progress(self.ctx, p, total_passes, good, pending, unreadable);
                self.reset_pass(pass);
                // Paint the map at pass start, BEFORE the settle. Otherwise the bar sits
                // all-green until the patch loop's first emission — most visible on resume.
                if let Some(snap) = freemkv_engine::progress_snapshot_from_mapfile(
                    self.mapfile,
                    Some(self.title),
                    libfreemkv::progress::PassKind::Trim { reverse: true },
                    self.ctx.bytes_total_disc,
                ) {
                    push_pass_state(self.ctx, &snap, self.bps, p, total_passes, &self.state);
                }
                crate::server::log::device_log(
                    device,
                    &format!(
                        "PASS {pass}/{total_passes}: retrying bad ranges (bpt=1) bytes_pending={pending}"
                    ),
                );
            }
            RecoveryEvent::PassDone {
                pass: 1,
                good,
                unreadable,
                pending,
                ..
            } => crate::server::log::device_log(
                device,
                &format!(
                    "Pass 1 done: {:.2} GB good, {:.2} MB unreadable, {:.2} MB pending",
                    good as f64 / BYTES_PER_GIB,
                    unreadable as f64 / BYTES_PER_MIB,
                    pending as f64 / BYTES_PER_MIB,
                ),
            ),
            RecoveryEvent::PassDone {
                pass,
                unreadable,
                pending,
                recovered,
                wedged,
                halted,
                ..
            } => {
                let exit_str = if halted {
                    " (halt)"
                } else if wedged {
                    " (DRIVE WEDGED: fast-fail sense — retries aborted, needs spin-cycle/power-cycle)"
                } else {
                    ""
                };
                // All three buckets: recovered this pass, still-pending, and given-up
                // unreadable (0 until the promotion).
                crate::server::log::device_log(
                    device,
                    &format!(
                        "Pass {pass} done: recovered {:.2} MB this pass; {:.2} MB still bad, {:.2} MB unreadable{exit_str}",
                        recovered as f64 / BYTES_PER_MIB,
                        pending as f64 / BYTES_PER_MIB,
                        unreadable as f64 / BYTES_PER_MIB,
                    ),
                );
            }
            RecoveryEvent::PatchesStart { max, pending } => crate::server::log::device_log(
                device,
                &format!(
                    "PASS 2-{max}: retry loop starting max_retries={max} bytes_pending={pending}"
                ),
            ),
            RecoveryEvent::Stopped { pass } => crate::server::log::device_log(
                device,
                &format!("PASS {pass} STOPPED: user halt before retry pass"),
            ),
            RecoveryEvent::MapUnreadable { pass, error } => crate::server::log::device_log(
                device,
                &format!("PASS {pass}: could not read the mapfile ({error}); running it"),
            ),
            RecoveryEvent::Converged { pass } => {
                // ISO needs the whole disc clean, MKV/M2TS only the muxed title.
                let scope_label = if self.is_iso {
                    "whole disc"
                } else {
                    "muxed title"
                };
                crate::server::log::device_log(
                    device,
                    &format!(
                        "PASS {pass} SKIPPED: {scope_label} is 100% recovered in mapfile — proceeding to mux"
                    ),
                );
            }
            RecoveryEvent::NoProgress { pass, recovered } => {
                crate::server::log::device_log(
                    device,
                    &format!(
                        "PASS {pass} STOPPED: strategy=patch_recovery exhausted — no progress (recovered={} MB) after all retry attempts",
                        recovered as f64 / BYTES_PER_MIB
                    ),
                );
                crate::server::log::device_log(
                    device,
                    "STRATEGY_FAILURE: patch_recovery exhausted — drive cannot recover more data from bad sectors with current settings",
                );
                crate::server::log::device_log(
                    device,
                    "RECOVERY_GUIDANCE: Consider increasing max_retries or abort_on_lost_secs if tolerating some data loss is acceptable.",
                );
            }
            RecoveryEvent::Promoted { map, intact } => self.promoted(map, intact),
            RecoveryEvent::LossUnmeasured { .. } => {
                // The engine's verdict aborts: the loss can't be measured without the mapfile.
                crate::server::log::device_log(
                    device,
                    "Recovery mapfile could not be loaded to verify loss — forcing abort (cannot confirm a clean rip)",
                );
                tracing::error!(
                    device = %device,
                    mapfile = %self.mapfile.display(),
                    "end_of_recovery_promote: mapfile load failed at abort-decision point; forcing abort (loss unquantifiable)"
                );
            }
            _ => {}
        }
    }

    // The promoted mapfile (NonTrimmed → Unreadable after the final retry pass): re-derive the
    // damage fields for the UI before the marker snapshot reads them. The loss verdict is the
    // engine's.
    fn promoted(&self, map: &freemkv_engine::Mapfile, intact: bool) {
        let device = self.device();
        tracing::info!(
            device = %device,
            bytes_unreadable = map.stats().bytes_unreadable,
            "end_of_recovery_promote: NonTrimmed -> Unreadable after final retry pass"
        );
        if !intact {
            tracing::error!(
                device = %device,
                "end_of_recovery_promote: damage record is \
                 incomplete — treating loss as unquantifiable"
            );
        }
        let (
            promoted_bad_ranges,
            promoted_num_bad,
            promoted_truncated,
            promoted_total_lost_ms,
            promoted_largest_gap_ms,
        ) = state::build_bad_ranges(map, self.title, self.bps);
        let promoted_main_title_bad = map.ranges_with(&[freemkv_engine::SectorStatus::Unreadable]);
        let promoted_main_bad_bytes =
            libfreemkv::disc::bytes_bad_in_title(self.title, &promoted_main_title_bad);
        let promoted_main_lost_ms = if self.bps > 0.0 {
            promoted_main_bad_bytes as f64 * MILLIS_PER_SEC / self.bps
        } else {
            0.0
        };
        let promoted_errors = (map.stats().bytes_unreadable / 2048) as u32;
        update_state_with(device, |s| {
            s.errors = promoted_errors;
            s.total_lost_ms = promoted_total_lost_ms;
            s.main_lost_ms = promoted_main_lost_ms;
            s.bad_ranges = promoted_bad_ranges;
            s.num_bad_ranges = promoted_num_bad;
            s.bad_ranges_truncated = promoted_truncated;
            s.largest_gap_ms = promoted_largest_gap_ms;
        });
    }
}

impl freemkv_engine::Sink for ServerPassSink<'_> {
    // The engine's own pass lines go to the trace; the device log is narrated from the
    // milestones in the server's words.
    fn log(&self, _level: freemkv_engine::Level, msg: &str) {
        tracing::debug!(target: "autorip::passes", device = %self.ctx.device, "{msg}");
    }

    fn event(&self, e: &freemkv_engine::Event<'_>) {
        match e {
            freemkv_engine::Event::Pass(p) => self.pass_progress(p),
            freemkv_engine::Event::Recovery(r) => self.milestone(r),
            _ => {}
        }
    }

    fn should_cancel(&self) -> bool {
        self.user_halt.load(Ordering::Relaxed)
    }
}
