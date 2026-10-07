//! Per-device rip state, the global STATE map, and the per-frame
//! `update_state` building blocks (PassContext / PassProgressState /
//! push_pass_state / set_pass_progress / build_bad_ranges).

use crate::server::util::{BYTES_PER_GIB, BYTES_PER_MIB, MILLIS_PER_SEC, SECTOR_BYTES};
use std::sync::Mutex;

/// One contiguous bad range as seen in the UI. Derived from the mapfile
/// during a multi-pass rip; chapter/time-offset come from the scanned title's
/// playlist metadata when the bad region lands in AV content.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BadRange {
    pub lba: u64,
    pub count: u32,
    pub duration_ms: f64,
    pub chapter: Option<u32>,
    pub time_offset_secs: Option<f64>,
}

/// Whether — and how — a disc's partial staging state can be resumed. Set on
/// [`RipState::resumable`] at scan time and rendered by the dashboard as a
/// Resume button. Serializes to a lowercase tag (`"remux"` / `"sweep"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Resumable {
    /// Sweep finished (no bytes pending) but the final MKV is missing — Resume
    /// just re-muxes the staged ISO (no disc reads).
    Remux,
    /// Partial sweep: the mapfile still has pending (NonTrimmed / non-tried)
    /// bytes. Resume continues Pass 1 from the mapfile, reading only the
    /// missing ranges.
    Sweep,
}

// TODO: replace the stringly-typed `status` with DeviceStage and
// PipelineStage enums. Deferred: web.rs buildSteps hard-depends on these
// exact status strings, so the cutover must land with the frontend rework.
/// State broadcast for web UI.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RipState {
    pub device: String,
    pub status: String, // "idle", "scanning", "ripping", "moving", "done", "error"
    pub disc_present: bool,
    pub disc_name: String,
    /// The disc's RAW volume label (`DiscId::name()`), before the TMDB lookup.
    ///
    /// Distinguishes discs of a boxset that share one [`Self::disc_name`] (TMDB title).
    ///
    /// Server-side bookkeeping only — not serialized, the UI shows the TMDB
    /// title. Carried forward across state pushes by [`update_state`] (see
    /// there), because nearly every caller builds a fresh `RipState`.
    #[serde(skip)]
    pub disc_label: String,
    /// This device's terminal state is a DEFERRAL, not a failure: the work
    /// stopped for a reason that fixes itself (keys arrive), staging is
    /// intact, and the next pass will pick it up unchanged.
    ///
    /// Set only by the deferral exits themselves; NOT inferred from `status == "idle"`.
    ///
    /// Server-side bookkeeping only — not serialized. Deliberately NOT
    /// carried forward across state pushes: it describes one terminal push.
    #[serde(skip)]
    pub failure_deferred: bool,
    /// This device's terminal state is a structural FINALIZE failure (the MKV
    /// could not be finalized — e.g. E6008 no muxable frames / unseekable
    /// output), as opposed to a resumable mid-mux read error.
    ///
    /// Set only by the mux-incomplete finalize exit in `resume_remux`.
    ///
    /// Server-side bookkeeping only — not serialized. Deliberately NOT
    /// carried forward across state pushes: it describes one terminal push.
    #[serde(skip)]
    pub failure_finalize: bool,
    /// The re-mux was refused up front because staging lacks space for its outputs
    /// (retryable; staging intact). Server-side only; not carried across pushes.
    #[serde(skip)]
    pub failure_space: bool,
    pub disc_format: String, // "uhd", "bluray", "dvd"
    pub progress_pct: u8,
    pub progress_gb: f64,
    pub speed_mbs: f64,
    pub eta: String,
    pub errors: u32,
    /// Estimated seconds of video lost to skipped sectors. Uses the title's
    /// actual bitrate, not a hardcoded constant — the UI should prefer this
    /// over computing from `errors` client-side.
    pub lost_video_secs: f64,
    /// Last sector read (LBA). Shows forward motion through a bad zone even
    /// when bytes_written is stalled waiting for the demuxer.
    pub last_sector: u64,
    /// Current adaptive batch size. Equal to `preferred_batch` during clean
    /// reads; drops on failure, climbs back with sustained success.
    pub current_batch: u16,
    /// Kernel-reported preferred batch size (from detect_max_batch_sectors).
    pub preferred_batch: u16,
    /// Current pass number (1 = initial disc→ISO copy, 2..=N = retry patches,
    /// N+1 = mux). Zero when not in multi-pass mode.
    pub pass: u8,
    /// Total number of passes in this rip (max_retries + 1 + mux). Zero when
    /// not in multi-pass mode.
    pub total_passes: u8,
    /// Bytes confirmed good across all passes so far (from mapfile stats).
    /// **Bucket: GOOD** — sectors successfully read at least once.
    pub bytes_good: u64,
    /// Bytes still pending retry (`NonTrimmed` / `NonScraped` in the
    /// mapfile). Pass 2-N will revisit these. After the final retry pass,
    /// any remaining `Pending` bytes are reclassified as `Unreadable`.
    /// **Bucket: MAYBE** — drive returned a marginal-read sense; smaller
    /// block size may recover them.
    pub bytes_maybe: u64,
    /// Bytes the drive has given up on (`Unreadable` in the mapfile).
    /// **Bucket: LOST** — terminal; no more retries are scheduled.
    pub bytes_lost: u64,
    /// Total disc size in bytes (for pass-relative progress).
    pub bytes_total_disc: u64,
    /// Bad sector ranges from the mapfile. Capped at 50 entries (biggest by
    /// duration) to keep SSE payloads bounded; `bad_ranges_truncated` reports
    /// how many more exist.
    pub bad_ranges: Vec<BadRange>,
    pub num_bad_ranges: u32,
    pub bad_ranges_truncated: u32,
    /// Sum of `Unreadable` ranges' durations — the actual video time
    /// lost to this rip. Companion to [`Self::bytes_lost`]. UI's red
    /// "no chance" pill renders this.
    pub total_lost_ms: f64,
    /// Sum of `Unreadable` ranges' durations that fall within the
    /// main-feature title's extents. Mirrors `total_lost_ms` but
    /// scoped to the longest title only — enables the UI to render
    /// "(Xs in main movie)".
    pub main_lost_ms: f64,
    /// **Main-feature time still AT RISK** — the honest live "Maybe" metric. The duration of
    /// every not-yet-good range (`NonTrimmed` + `NonScraped` + `Unreadable`) that falls within
    /// the main title's extents. Unlike [`Self::main_lost_ms`], this is non-zero mid-rip and
    /// melts toward it as retry passes resolve pending sectors.
    pub main_at_risk_ms: f64,
    /// Largest single contiguous bad range's duration. Tells the difference
    /// between 1000 × 1ms gaps (unnoticeable) vs 1 × 1s gap (noticeable glitch).
    pub largest_gap_ms: f64,
    /// True when this rip aborted because main-movie loss exceeded the
    /// threshold and a resumable `.aborted-loss` staging (the complete ISO) is
    /// on disk. The UI shows the **Accept damage & deliver** off-ramp when set —
    /// the operator can deliver the rip as-is instead of re-ripping.
    pub loss_aborted: bool,
    pub last_error: String,
    pub output_file: String,
    pub tmdb_title: String,
    pub tmdb_year: u16,
    pub tmdb_poster: String,
    pub tmdb_overview: String,
    /// TMDB media type ("movie" or "tv"). Carried into STATE so the
    /// auto-resume mux path can write a correct `media_type` into the
    /// `.done`/`.review` hand-off marker — otherwise the mover defaults
    /// every resumed rip to "movie" and files TV shows under the movie
    /// library. Empty string when unresolved.
    pub tmdb_media_type: String,
    pub duration: String,
    pub codecs: String,

    // ── v0.13.16 PipelineStats: the 5 user-visible numbers ────────────────
    /// Per-pass progress percent (0-100). Computed from libfreemkv's
    /// `work_done / work_total`. UI bar reads this directly — no math.
    pub pass_progress_pct: u8,
    /// Per-pass ETA, formatted as "MM:SS" or "HH:MM:SS". Empty when speed
    /// is too low to estimate.
    pub pass_eta: String,
    /// Total rip progress percent (0-100), summed across all passes +
    /// estimated retry work + mux. UI total bar reads this directly.
    pub total_progress_pct: u8,
    /// Total rip ETA across all remaining passes including mux estimate.
    pub total_eta: String,

    /// Damage severity tier (0.13.22). Computed from `errors` (bad
    /// sector count) and `total_lost_ms` (cumulative playback time lost).
    /// UI renders a colored badge: clean (green) / cosmetic (yellow) /
    /// moderate (orange) / serious (red).
    pub damage_severity: String,

    /// Operator-readable failure reason for `status == "failed"`.
    /// Populated when the resume-on-startup logic finds a `.failed`
    /// marker in a disc's staging dir (e.g. "restart loop detected at
    /// patch phase"). Distinct from `last_error` because `last_error`
    /// gets overwritten on every transient hiccup; this one survives
    /// across renders for the operator-decision view. Optional /
    /// `skip_serializing_if = "Option::is_none"` so older dashboards
    /// that don't know the field don't see a stray `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,

    /// v0.25.7: epoch-seconds timestamp of when the current rip
    /// transitioned into an active state (`scanning` or `ripping`).
    /// 0 when no rip is in flight. The UI uses this to render a live
    /// elapsed-time counter next to the Stop button — JS computes
    /// `now - started_epoch_secs` so the display advances every tick
    /// without server pressure. Preserved across `update_state` calls
    /// for the same rip; cleared when status returns to `idle`.
    pub started_epoch_secs: u64,
    /// Key readiness determined at scan time, for the dashboard tile:
    /// "Ready to rip", "Missing keys — `<reason>`", or "" (unknown).
    pub key_status: String,

    /// Resume affordance computed at scan time. `None` when there's no
    /// resumable staging for this disc (Rip only); `Some(_)` makes the
    /// dashboard show a Resume button alongside Rip. Omitted from the JSON
    /// when `None` so older dashboards don't see a stray field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resumable: Option<Resumable>,

    /// Monotonic claim generation, bumped by every successful
    /// [`try_claim_active`]. Lets a detached worker (e.g. a verify thread)
    /// tell whether the device it claimed is *still* the one it owns: if a
    /// newer claim (a rip, scan, eject, or a fresh verify) has landed since,
    /// the generation will have moved and the stale worker must NOT reset the
    /// device to idle — doing so would clobber the new owner's claim. Not
    /// serialized: a pure server-side bookkeeping field the UI never reads.
    #[serde(skip)]
    pub claim_gen: u64,
}

impl Default for RipState {
    fn default() -> Self {
        Self {
            device: String::new(),
            status: "idle".to_string(),
            disc_present: false,
            disc_name: String::new(),
            disc_label: String::new(),
            failure_deferred: false,
            failure_finalize: false,
            failure_space: false,
            disc_format: String::new(),
            progress_pct: 0,
            progress_gb: 0.0,
            speed_mbs: 0.0,
            eta: String::new(),
            errors: 0,
            lost_video_secs: 0.0,
            last_sector: 0,
            current_batch: 0,
            preferred_batch: 0,
            pass: 0,
            total_passes: 0,
            bytes_good: 0,
            bytes_maybe: 0,
            bytes_lost: 0,
            bytes_total_disc: 0,
            bad_ranges: Vec::new(),
            num_bad_ranges: 0,
            bad_ranges_truncated: 0,
            total_lost_ms: 0.0,
            main_lost_ms: 0.0,
            main_at_risk_ms: 0.0,
            largest_gap_ms: 0.0,
            loss_aborted: false,
            last_error: String::new(),
            output_file: String::new(),
            tmdb_title: String::new(),
            tmdb_year: 0,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            tmdb_media_type: String::new(),
            duration: String::new(),
            codecs: String::new(),
            pass_progress_pct: 0,
            pass_eta: String::new(),
            total_progress_pct: 0,
            total_eta: String::new(),
            damage_severity: String::new(),
            failure_reason: None,
            started_epoch_secs: 0,
            key_status: String::new(),
            resumable: None,
            claim_gen: 0,
        }
    }
}

/// Compute the damage-severity badge string from autorip's RipState
/// fields. Wraps freemkv-engine's `classify_damage` so the UI gets a stable
/// lowercase string ("clean" / "cosmetic" / "moderate" / "serious").
pub(super) fn damage_severity_for(errors: u32, total_lost_ms: f64) -> String {
    use freemkv_engine::DamageSeverity;
    // Direct match instead of round-tripping through serde_json::to_value
    // on every (throttled) progress callback. Strings match libfreemkv's
    // `#[serde(rename_all = "lowercase")]` repr so the UI is unchanged.
    match freemkv_engine::classify_damage(errors as u64, total_lost_ms) {
        DamageSeverity::Clean => "clean",
        DamageSeverity::Cosmetic => "cosmetic",
        DamageSeverity::Moderate => "moderate",
        DamageSeverity::Serious => "serious",
    }
    .to_string()
}

// Global state for web UI.
pub static STATE: once_cell::sync::Lazy<Mutex<std::collections::HashMap<String, RipState>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Operator-chosen TMDB title overrides, keyed by device. Set from the Drives
/// card's "✎ change" picker BEFORE a manual rip; consumed once by `rip_disc`,
/// where it takes precedence over the scan's auto-match so the rip files under
/// the operator's pick (and counts as confident → no review hold).
pub static TITLE_OVERRIDES: once_cell::sync::Lazy<
    Mutex<std::collections::HashMap<String, crate::server::tmdb::TmdbResult>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Record an operator title override for `device` (from the Drives card picker).
pub fn set_title_override(device: &str, r: crate::server::tmdb::TmdbResult) {
    // Recover-and-proceed on poison (same convention as is_busy/update_state):
    // silently dropping the override would lose the operator's title pick.
    let mut m = TITLE_OVERRIDES.lock().unwrap_or_else(|e| e.into_inner());
    m.insert(device.to_string(), r);
}

/// Take (and clear) the operator title override for `device`, if any.
pub fn take_title_override(device: &str) -> Option<crate::server::tmdb::TmdbResult> {
    let mut m = TITLE_OVERRIDES.lock().unwrap_or_else(|e| e.into_inner());
    m.remove(device)
}

// An explicit Stop suppresses automatic work for this insertion, not just a timer.
static STOPPED_INSERTIONS: once_cell::sync::Lazy<Mutex<std::collections::HashMap<String, bool>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

pub fn hold_stopped_disc(device: &str) {
    STOPPED_INSERTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(device.into(), false);
}

pub fn release_stopped_disc(device: &str) {
    STOPPED_INSERTIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

// Some USB drives briefly report no medium while cancellation releases the drive.
// Require two absent observations before ending a stopped insertion.
pub(super) fn stopped_disc_presence(
    device: &str,
    presence: libfreemkv::DiscPresence,
) -> libfreemkv::DiscPresence {
    use libfreemkv::DiscPresence;
    let mut stopped = STOPPED_INSERTIONS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(absent) = stopped.get_mut(device) else {
        return presence;
    };
    if presence == DiscPresence::Absent {
        if *absent {
            stopped.remove(device);
            DiscPresence::Absent
        } else {
            *absent = true;
            DiscPresence::Settling
        }
    } else {
        *absent = false;
        presence
    }
}

pub(super) fn try_claim_insert(device: &str) -> Option<u64> {
    // Serialize the automatic claim with Stop arming its hold.
    let stopped = STOPPED_INSERTIONS.lock().unwrap_or_else(|e| e.into_inner());
    if stopped.contains_key(device) {
        return None;
    }
    try_claim_active(device)
}

// Stop cooldowns: device -> the MONOTONIC instant the cooldown expires. `Instant`, not an
// `epoch_secs()` deadline, so it can't step backwards (NTP/clock-reset/VM-resume).
pub(super) static STOP_COOLDOWNS: once_cell::sync::Lazy<
    Mutex<std::collections::HashMap<String, std::time::Instant>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

const STOP_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);

pub fn set_stop_cooldown(device: &str) {
    let expires = std::time::Instant::now() + STOP_COOLDOWN;
    // Recover-and-proceed on poison (same convention as is_busy/update_state).
    let mut cd = STOP_COOLDOWNS.lock().unwrap_or_else(|e| e.into_inner());
    cd.insert(device.to_string(), expires);
}

pub(super) fn is_in_cooldown(device: &str) -> bool {
    let now = std::time::Instant::now();
    let cd = STOP_COOLDOWNS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&expires) = cd.get(device) {
        return now < expires;
    }
    false
}

/// Drop auxiliary per-device state on hot-unplug so changing device paths do not
/// accumulate stale entries. Recovers poisoned locks before cleanup.
pub(super) fn forget_device_state(device: &str) {
    release_stopped_disc(device);
    TITLE_OVERRIDES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
    STOP_COOLDOWNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
    super::session::forget_device_session_state(device);
}

/// True when `device` is a known drive tracked in STATE. Used by routes
/// that mutate per-device state (e.g. the title override) to reject a
/// request for an unknown device with 404 rather than silently storing an
/// override for a drive that doesn't exist. Recovers a poisoned guard for
/// the same reason `is_busy` does (a stale poison must not make every
/// device look unknown).
pub fn device_known(device: &str) -> bool {
    let s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    s.contains_key(device)
}

// `current_disc_name` lived here but was removed with the boxset fix: the
// display name alone can't identify a disc (every disc of a set shares one
// TMDB title). Staging lookups now go through `staging_basename_for_device`.

pub fn is_busy(device: &str) -> bool {
    // Recover a poisoned guard instead of treating poison as "not busy":
    // this is the double-rip guard, and swallowing the error would let a
    // second rip launch concurrently on the same drive. See log.rs convention.
    let s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    s.get(device).is_some_and(row_is_busy)
}

/// The double-rip guard's predicate over a STATE row, for callers already holding the lock.
pub fn row_is_busy(row: &RipState) -> bool {
    row.status == "scanning" || row.status == "ripping"
}

pub fn update_state(device: &str, mut state: RipState) {
    // 0.13.22: derive damage_severity from errors + total_lost_ms on
    // every push so the UI badge stays in sync with the latest counters.
    state.damage_severity = damage_severity_for(state.errors, state.total_lost_ms);

    // v0.25.7: auto-maintain started_epoch_secs so a fresh default-zeroed
    // RipState from rip_disc/scan_disc/watchdog doesn't reset the UI's
    // elapsed-time counter. Recover a poisoned mutex like `is_busy` does.
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    // Preserve claim_gen: callers push fresh RipStates via ..Default::default(),
    // so without this the stale-worker ownership check would keep resetting.
    let prev_claim_gen = s.get(device).map(|p| p.claim_gen).unwrap_or(0);
    if state.claim_gen == 0 {
        state.claim_gen = prev_claim_gen;
    }
    // Preserve disc_label like claim_gen (it distinguishes boxset discs
    // sharing one TMDB title), else it's erased on the first progress push.
    // Guarded on disc_name unchanged/non-empty so a new disc doesn't inherit it.
    if state.disc_label.is_empty()
        && !state.disc_name.is_empty()
        && let Some(prev) = s.get(device)
        && prev.disc_name == state.disc_name
    {
        state.disc_label = prev.disc_label.clone();
    }
    let prev_started = s.get(device).map(|p| p.started_epoch_secs).unwrap_or(0);
    let now_active = is_active_status(&state.status);
    let was_active = s.get(device).is_some_and(|p| is_active_status(&p.status));

    if state.started_epoch_secs == 0 {
        if now_active && was_active && prev_started > 0 {
            // Continuing an in-flight rip — keep the original start
            state.started_epoch_secs = prev_started;
        } else if now_active {
            // Transition into active — stamp now
            state.started_epoch_secs = crate::server::util::epoch_secs();
        }
        // else: idle / done / error / failed → leave at 0 (clears
        // the elapsed-counter in the UI)
    }
    s.insert(device.to_string(), state);
}

fn is_active_status(s: &str) -> bool {
    matches!(s, "scanning" | "ripping")
}

/// Mutate a device's RipState via a closure. **Use this** instead of
/// `update_state` when changing specific fields without wanting to wipe
/// the rest. The `..Default::default()` pattern caused at least three
/// regressions (v0.11.20 watchdog, v0.11.17 errors-on-completion, v0.12.0
/// pass-progress fields) where a "small" state push silently zeroed a
/// field the UI was rendering.
///
/// Creates a default-initialized RipState if the device isn't in the map
/// yet so the first call after boot doesn't silently no-op.
pub fn update_state_with<F: FnOnce(&mut RipState)>(device: &str, f: F) {
    // Recover from a poisoned STATE mutex rather than silently dropping
    // the mutation — see `update_state` / `is_busy` / log.rs.
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = s.entry(device.to_string()).or_insert_with(|| RipState {
        device: device.to_string(),
        ..Default::default()
    });
    f(entry);
    // Re-derive damage_severity after the mutation, matching `update_state`:
    // closures that bump errors/total_lost_ms (patch-pass, watchdog) would
    // otherwise leave a stale severity badge, since this path skips that.
    entry.damage_severity = damage_severity_for(entry.errors, entry.total_lost_ms);
}

/// Atomically claim a device for active work. If it is already `scanning`/`ripping`, returns
/// `None` (the caller should reject with 409); otherwise marks it `scanning` and returns the
/// new `claim_gen`. Folds the busy-check and the status-set into ONE `STATE` lock, closing a
/// TOCTOU between a separate check and a separate `update_state`
///
/// Thin wrapper over [`try_claim_active_checked`] with `known = true` — see
/// that function's doc for when a caller must pass `false` instead.
pub fn try_claim_active(device: &str) -> Option<u64> {
    try_claim_active_checked(device, true)
}

/// Same contract as [`try_claim_active`], but `known` tells it whether the
/// caller has already verified `device` names a real, currently-enumerated
/// drive. When `known` is `false` and the device has no existing STATE entry,
/// the claim is refused instead of creating one — closes an unauthenticated
/// resource-exhaustion path. Pass `true` only when `device` came from the
/// poll loop's own enumerated drive list, or was cross-checked against it.
///
/// Refuses the claim if EITHER the status is scanning/ripping OR the rip thread is still alive.
pub fn try_claim_active_checked(device: &str, known: bool) -> Option<u64> {
    // Liveness first, outside the STATE lock: a worker starts only after a claim
    // sets `scanning`, so one started after this check fails the status check
    // below, and one that exits after it only makes this refusal conservative.
    if super::session::rip_thread_running(device) {
        tracing::warn!(
            device = %device,
            "refusing claim: a worker thread for this device is still running \
             (its status is already terminal, but it has not exited yet)"
        );
        return None;
    }
    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if s.get(device).is_some_and(row_is_busy) {
        return None;
    }
    if !known && !s.contains_key(device) {
        return None;
    }
    let entry = s.entry(device.to_string()).or_insert_with(|| RipState {
        device: device.to_string(),
        ..Default::default()
    });
    entry.status = "scanning".to_string();
    entry.disc_present = true;
    // Bump claim_gen so a stale detached worker can detect the device was
    // re-claimed and decline to reset it to idle. Saturating so it never wraps.
    entry.claim_gen = entry.claim_gen.saturating_add(1);
    // The generation IS the claim's identity, and it is returned so the caller
    // can hand it to `rollback_failed_spawn` and undo THIS claim and no other.
    // See that function for the wedge that a device-only rollback produced.
    Some(entry.claim_gen)
}

/// Shared context for the progress callbacks of a multi-pass rip. Built once
/// before pass 1 and borrowed by every pass's progress sink, so the callbacks
/// share the same immutable values without reallocating.
pub(super) struct PassContext {
    pub(super) device: String,
    pub(super) display_name: String,
    pub(super) disc_format: String,
    pub(super) tmdb_title: String,
    pub(super) tmdb_year: u16,
    pub(super) tmdb_poster: String,
    pub(super) tmdb_overview: String,
    pub(super) tmdb_media_type: String,
    pub(super) duration: String,
    pub(super) codecs: String,
    pub(super) filename: String,
    /// The disc's capacity: the scale of the UI's disc map (bad ranges at their real LBA).
    pub(super) bytes_total_disc: u64,
    /// The bytes the sweep reads: an MKV rip's staged scope, else the whole disc. Sizes the
    /// progress totals, so a scoped sweep reads 100% when its scope is done.
    pub(super) bytes_sweep: u64,
    /// Preferred batch size (kernel-reported max sectors per CDB) — surfaced
    /// in RipState during Pass 1 / Pass 2+ so the UI shows a non-zero
    /// `preferred_batch` / `current_batch`. Pass 1 never shrinks the batch
    /// (freemkv_engine::sweep uses a fixed size); current_batch == preferred_batch
    /// throughout. The DiscStream batch halver only operates during the
    /// mux phase and is reported via the direct-mode stream loop.
    pub(super) batch: u16,
    /// Configured retry-pass count. Used by `push_pass_state` to estimate the
    /// total-bar workload — only `max_retries × bytes_unreadable` worth of work
    /// is queued for retry passes (not the entire pending set, which during
    /// Pass 1 is the whole disc and produced a wildly inflated total ETA).
    /// 0 = single-pass mode (no ISO, no retries, no separate mux phase).
    pub(super) max_retries: u8,
}

/// Walk the title's extents to find the byte offset *within the title* for a
/// given disc LBA. None if the LBA falls outside every extent (UDF metadata
/// or other non-AV area, where chapter mapping doesn't apply).
pub(super) fn byte_offset_in_title(lba: u32, title: &libfreemkv::DiscTitle) -> Option<u64> {
    let mut cumulative = 0u64;
    for ext in &title.extents {
        // start_lba/sector_count are disc-supplied and untrusted: a corrupt
        // image could overflow u32 here. Widen to u64 so the containment
        // test can't wrap and falsely match.
        let end = ext.start_lba as u64 + ext.sector_count as u64;
        if lba >= ext.start_lba && (lba as u64) < end {
            return Some(cumulative + (lba - ext.start_lba) as u64 * SECTOR_BYTES);
        }
        cumulative += ext.sector_count as u64 * SECTOR_BYTES;
    }
    None
}

fn range_chapter(lba: u32, title: &libfreemkv::DiscTitle) -> (Option<u32>, Option<f64>) {
    if let Some(byte_offset) = byte_offset_in_title(lba, title)
        && let Some((ch, t)) = libfreemkv::disc::chapter_at_offset(
            &title.chapters,
            byte_offset,
            title.duration_secs,
            title.size_bytes,
        )
    {
        return (Some(ch as u32), Some(t));
    }
    (None, None)
}

/// Build the **terminal** bad-range list (`Unreadable` only) — the done-card /
/// abort snapshot, where "bad" means the drive has finally given up. Thin
/// wrapper over [`located_ranges`].
pub(crate) fn build_bad_ranges(
    map: &freemkv_engine::Mapfile,
    title: &libfreemkv::DiscTitle,
    bps: f64,
) -> (Vec<BadRange>, u32, u32, f64, f64) {
    located_ranges(map, title, bps, &[freemkv_engine::SectorStatus::Unreadable])
}

/// Build a located range list (LBA + sectors + duration + chapter) for the
/// given mapfile `statuses`, capped at 50 by duration (largest first); the
/// truncation count lets the UI say "+X more". `NonTried` is never included.
pub(crate) fn located_ranges(
    map: &freemkv_engine::Mapfile,
    title: &libfreemkv::DiscTitle,
    bps: f64,
    statuses: &[freemkv_engine::SectorStatus],
) -> (Vec<BadRange>, u32, u32, f64, f64) {
    let raw = map.ranges_with(statuses);
    let total_count = raw.len() as u32;
    let mut ranges: Vec<BadRange> = raw
        .iter()
        .map(|(pos, size)| {
            let lba = pos / SECTOR_BYTES;
            let count = (size / SECTOR_BYTES) as u32;
            let duration_ms = if bps > 0.0 {
                (*size as f64) / bps * MILLIS_PER_SEC
            } else {
                0.0
            };
            let (chapter, time_offset_secs) = range_chapter(lba as u32, title);
            BadRange {
                lba,
                count,
                duration_ms,
                chapter,
                time_offset_secs,
            }
        })
        .collect();
    ranges.sort_by(|a, b| {
        b.duration_ms
            .partial_cmp(&a.duration_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let total_lost_ms: f64 = ranges.iter().map(|r| r.duration_ms).sum();
    let largest_gap_ms = ranges.first().map(|r| r.duration_ms).unwrap_or(0.0);
    let truncated = ranges.len().saturating_sub(50) as u32;
    ranges.truncate(50);
    (
        ranges,
        total_count,
        truncated,
        total_lost_ms,
        largest_gap_ms,
    )
}

// `RipProgress` / `from_map` were deleted in the 1.2.0 mapfile-free rework:
// `push_pass_state` now reads the drilldown from `PassProgress.located`.

/// Per-pass progress state: one `freemkv_engine::SpeedEstimator` (speed/ETA
/// math lives there — see its docs) plus autorip's per-pass bookkeeping. Held
/// in a RefCell inside the callback closure so interior mutability keeps the closure `Fn`.
#[derive(Debug)]
pub(super) struct PassProgressState {
    // The engine's canonical speed/ETA estimator, promoted from autorip's own
    // math so every front-end shares it. A fresh instance per pass.
    pub(super) speed: freemkv_engine::SpeedEstimator,
    /// Wall-clock of the last throttled callback. The progress closure
    /// checks this to skip work when less than 250 ms have passed.
    pub(super) last_update: std::time::Instant,
    /// Wall-clock of the last device-log line emitted from this pass.
    pub(super) last_log: std::time::Instant,
    /// Last `work_done` reported by libfreemkv's `Progress` trait — bytes
    /// processed in this pass so far. Drives `pass_progress_pct`.
    pub(super) last_work_done: u64,
    /// Last `work_total` reported by libfreemkv's `Progress` trait — total
    /// bytes this pass will process. Drives `pass_progress_pct` denominator.
    pub(super) last_work_total: u64,
    // `bytes_unreadable` snapshotted on this pass's first `push_pass_state` callback, frozen
    // for the rest of the pass so the total-progress denominator doesn't inflate mid-pass.
    pub(super) frozen_bytes_lost: Option<u64>,
}

/// Above this, the displayed ETA is shown as a steady ">Nh" rather than a
/// precise-looking huge number. On a dead-media residue `remaining / rate`
/// explodes and whipsaws; clamping it keeps the display honest and stable.
pub(super) const ETA_CAP_SECS: u64 = 6 * 3600;

// An ETA for display: `s`, `m:ss` or `h:mm:ss`, or a steady ">Nh" above
// [`ETA_CAP_SECS`] (a dead-media near-zero rate makes `remaining / rate` whipsaw).
fn capped_eta(secs: u64) -> String {
    if secs > ETA_CAP_SECS {
        return format!(">{}h", ETA_CAP_SECS / 3600);
    }
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}

impl PassProgressState {
    pub(super) fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            speed: freemkv_engine::SpeedEstimator::new(),
            last_update: now,
            last_log: now,
            last_work_done: 0,
            last_work_total: 0,
            frozen_bytes_lost: None,
        }
    }
}

/// Push a fresh RipState snapshot for the current pass. Feeds work_done into
/// the engine `SpeedEstimator` for displayed speed + ETA (the main stream
/// loop's tracker isn't running during `sweep`/`patch`); buckets/drilldown come from `PassProgress.located`.
pub(super) fn push_pass_state(
    ctx: &PassContext,
    p: &libfreemkv::progress::PassProgress,
    bps: f64,
    pass: u8,
    total_passes: u8,
    state: &std::sync::Mutex<PassProgressState>,
) {
    // Buckets come straight from the library's progress contract `p`.
    // GOOD = Finished, MAYBE = retry-eligible, LOST = terminal Unreadable.
    let bytes_good = p.bytes_good_total;
    let bytes_maybe = p.bytes_retryable_total;
    let bytes_lost = p.bytes_unreadable_total;
    let total_lost_ms = if bps > 0.0 {
        bytes_lost as f64 * MILLIS_PER_SEC / bps
    } else {
        0.0
    };
    // Owned by the done-card verdict (resume.rs); structurally 0 mid-rip since
    // Unreadable is only promoted after the final pass. UI reads
    // `p.located.main_at_risk_ms` for the honest at-risk time instead.
    let main_lost_ms = 0.0;
    // Freeze bytes_unreadable on this pass's first callback: reading it live
    // let the Pass-1 denominator grow, stalling total_pct. `bytes_lost` above
    // stays live; only this frozen figure feeds total-progress.
    let retry_denom_bytes = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        *s.frozen_bytes_lost.get_or_insert(bytes_lost)
    };
    // `errors` is the user-visible skipped-sector count: terminal-bad
    // sectors only (`bytes_lost`). Pending bytes are not "errors" — they
    // may still recover.
    let errors = (bytes_lost / SECTOR_BYTES) as u32;
    // v0.13.16: pass_progress_pct = work_done / work_total (per-pass).
    // The legacy progress_pct stays populated as a copy (back-compat for
    // any consumer reading the old field).
    let (last_pos, last_work_total) = {
        let s = state.lock().unwrap_or_else(|e| e.into_inner());
        (s.last_work_done, s.last_work_total)
    };
    let pass_pct = if let Some(p) = (last_pos * 100).checked_div(last_work_total) {
        p.min(100) as u8
    } else {
        0
    };
    // Total bar: total_work = capacity + max_retries*bytes_unreadable + mux_estimate.
    // Retry passes only re-read the bad set, not all of bytes_pending; using
    // bytes_pending made total ≈ 6x capacity, showing Pass 1 as ~16% not ~50%.
    let cfg_max_retries = ctx.max_retries as u64;
    let mux_estimate_bytes = if cfg_max_retries > 0 {
        ctx.bytes_sweep // mux re-reads the ISO, ~1× the swept bytes of I/O
    } else {
        0
    };
    let total_work_estimated = ctx
        .bytes_sweep
        .saturating_add(cfg_max_retries.saturating_mul(retry_denom_bytes))
        .saturating_add(mux_estimate_bytes);
    // Pass 1: total_done = last_pos. Retry pass: capacity + (pass-2)*bytes_lost
    // + last_pos. Uses the same frozen retry_denom_bytes as the denominator.
    let total_done: u64 = if pass <= 1 {
        last_pos
    } else {
        let prior_retry_count = pass.saturating_sub(2) as u64;
        ctx.bytes_sweep
            .saturating_add(prior_retry_count.saturating_mul(retry_denom_bytes))
            .saturating_add(last_pos)
    };
    let total_pct = if let Some(p) = (total_done * 100).checked_div(total_work_estimated) {
        p.min(100) as u8
    } else {
        0
    };
    // Legacy field — keep populated for back-compat. Equals pass_pct.
    let pct = pass_pct;

    // Speed = rate of last_pos (work_done), NOT bytes_good: v0.13.15 tracked
    // bytes_good rate, reading 0 during skip-forward zones where work_done
    // advances but bytes_good is frozen, even though the bar was moving.
    let (speed_mbs, pass_eta_str, total_eta_str) = {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();
        // Patch passes (pass > 1) hold a fixed 10s speed window — bursty
        // recovery should read responsively, not be smoothed over a minute.
        s.speed.set_responsive(pass > 1);
        let display_speed = s.speed.observe(now, last_pos);
        // ETA uses the long-running average, not the windowed display speed,
        // since a transient slow region can whipsaw the window. Falls back to
        // display_speed during ETA_WARMUP_SECS while the average is noisy.
        let eta_speed = s.speed.eta_speed_mbs(now, display_speed);
        s.last_update = now;
        // Floor is 0.1 KB/s, not the old 10 KB/s, which blanked the ETA right
        // at the patch rate (~12 KB/s).
        let pass_eta = if eta_speed > 0.0001 && last_work_total > last_pos {
            let rem_mb = (last_work_total - last_pos) as f64 / BYTES_PER_MIB;
            capped_eta((rem_mb / eta_speed) as u64)
        } else {
            String::new()
        };
        let total_eta = if eta_speed > 0.0001 && total_work_estimated > total_done {
            let rem_mb = (total_work_estimated - total_done) as f64 / BYTES_PER_MIB;
            capped_eta((rem_mb / eta_speed) as u64)
        } else {
            String::new()
        };
        (display_speed, pass_eta, total_eta)
    };
    // Back-compat: legacy `eta` mirrors pass_eta.
    let eta = pass_eta_str.clone();

    update_state(
        &ctx.device,
        RipState {
            device: ctx.device.clone(),
            status: "ripping".to_string(),
            disc_present: true,
            disc_name: ctx.display_name.clone(),
            disc_format: ctx.disc_format.clone(),
            progress_pct: pct,
            progress_gb: last_pos as f64 / BYTES_PER_GIB,
            // Populate last_sector during sweep too, not just mux: previously
            // left at Default(0), so the UI playhead never moved during sweep.
            last_sector: map_head_bytes(ctx, last_pos) / SECTOR_BYTES,
            speed_mbs,
            eta,
            errors,
            lost_video_secs: total_lost_ms / MILLIS_PER_SEC,
            output_file: ctx.filename.clone(),
            tmdb_title: ctx.tmdb_title.clone(),
            tmdb_year: ctx.tmdb_year,
            tmdb_poster: ctx.tmdb_poster.clone(),
            tmdb_overview: ctx.tmdb_overview.clone(),
            tmdb_media_type: ctx.tmdb_media_type.clone(),
            duration: ctx.duration.clone(),
            codecs: ctx.codecs.clone(),
            pass,
            total_passes,
            bytes_good,
            bytes_maybe,
            bytes_lost,
            bytes_total_disc: ctx.bytes_total_disc,
            // Live drilldown shows the located MAYBE ranges (pending + lost), so
            // a patch pass is visible instead of a black box. Rendered by the
            // library (`p.located`); autorip only maps it to its JSON DTO.
            bad_ranges: p
                .located
                .ranges
                .iter()
                .map(|r| BadRange {
                    lba: r.lba,
                    count: r.count,
                    duration_ms: r.duration_ms,
                    chapter: r.chapter,
                    time_offset_secs: r.time_offset_secs,
                })
                .collect(),
            num_bad_ranges: p.located.num_ranges,
            bad_ranges_truncated: p.located.truncated,
            total_lost_ms,
            main_lost_ms,
            main_at_risk_ms: p.located.main_at_risk_ms,
            largest_gap_ms: p.located.largest_gap_ms,
            preferred_batch: ctx.batch,
            current_batch: ctx.batch,
            pass_progress_pct: pass_pct,
            pass_eta: pass_eta_str,
            total_progress_pct: total_pct,
            total_eta: total_eta_str,
            ..Default::default()
        },
    );

    // Periodic device-log line (60s, matching the main stream loop) so a long
    // pass doesn't go silent. Reports swept position (advances during a
    // skip-forward bad zone) separately from bytes_good (real recovery).
    {
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        if s.last_log.elapsed().as_secs() >= 60 {
            s.last_log = std::time::Instant::now();
            let pos_gb = last_pos as f64 / BYTES_PER_GIB;
            let good_gb = bytes_good as f64 / BYTES_PER_GIB;
            let total_gb = ctx.bytes_sweep as f64 / BYTES_PER_GIB;
            let speed_str = if speed_mbs >= 1.0 {
                format!("{speed_mbs:.1} MB/s")
            } else {
                format!("{:.0} KB/s", speed_mbs * 1024.0)
            };
            let bad_str = if bytes_lost > 0 {
                format!(
                    ", {} skipped ({:.2} MB)",
                    errors,
                    bytes_lost as f64 / BYTES_PER_MIB
                )
            } else {
                String::new()
            };
            crate::server::log::device_log(
                &ctx.device,
                &format!(
                    "Pass {pass}/{total_passes}: swept {:.1} GB / {:.1} GB ({}%), good {:.1} GB, {}{}",
                    pos_gb, total_gb, pct, good_gb, speed_str, bad_str
                ),
            );
        }
    }
}

// The sweep's read head on the disc map's scale: `swept` of `bytes_sweep` as the same share of
// the disc, so a scoped sweep's bar fills the map as its scope does (identity for a whole disc).
pub(super) fn map_head_bytes(ctx: &PassContext, swept: u64) -> u64 {
    if ctx.bytes_sweep == 0 || ctx.bytes_sweep == ctx.bytes_total_disc {
        return swept;
    }
    let head = u128::from(swept) * u128::from(ctx.bytes_total_disc) / u128::from(ctx.bytes_sweep);
    u64::try_from(head)
        .unwrap_or(u64::MAX)
        .min(ctx.bytes_total_disc)
}

/// Build a RipState snapshot for a multi-pass rip in a specific pass. Immutable
/// per-rip fields come from `ctx`; the rest are per-pass dynamic values.
/// Status is always "ripping"; pass=total_passes indicates the mux phase.
pub(super) fn set_pass_progress(
    ctx: &PassContext,
    pass: u8,
    total_passes: u8,
    bytes_good: u64,
    bytes_maybe: u64,
    bytes_lost: u64,
) {
    let pct = if let Some(p) = (bytes_good * 100).checked_div(ctx.bytes_sweep) {
        p.min(100) as u8
    } else {
        0
    };
    // update_state_with (not a full RipState) so cumulative fields survive
    // the pass boundary instead of zeroing. Per-pass fields ARE reset below:
    // carrying pass 1's 99% made pass 2 read "pass 1/7 · 99%" through settle.
    update_state_with(&ctx.device, |s| {
        s.status = "ripping".to_string();
        s.disc_present = true;
        s.disc_name = ctx.display_name.clone();
        s.disc_format = ctx.disc_format.clone();
        s.progress_pct = pct;
        s.progress_gb = bytes_good as f64 / BYTES_PER_GIB;
        s.output_file = ctx.filename.clone();
        s.tmdb_title = ctx.tmdb_title.clone();
        s.tmdb_year = ctx.tmdb_year;
        s.tmdb_poster = ctx.tmdb_poster.clone();
        s.tmdb_overview = ctx.tmdb_overview.clone();
        s.tmdb_media_type = ctx.tmdb_media_type.clone();
        s.duration = ctx.duration.clone();
        s.codecs = ctx.codecs.clone();
        s.pass = pass;
        s.total_passes = total_passes;
        s.bytes_good = bytes_good;
        s.bytes_maybe = bytes_maybe;
        s.bytes_lost = bytes_lost;
        s.bytes_total_disc = ctx.bytes_total_disc;
        s.preferred_batch = ctx.batch;
        s.current_batch = ctx.batch;
        // Reset per-pass bar/ETA/speed at the pass boundary so a new pass
        // starts at 0% instead of inheriting the prior pass's 99%.
        // total_progress_pct is left untouched; push_pass_state refills these.
        s.pass_progress_pct = 0;
        s.pass_eta = String::new();
        s.eta = String::new();
        s.speed_mbs = 0.0;
    });
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
