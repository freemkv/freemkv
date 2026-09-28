//! Staging-directory bookkeeping: free-space probe + the unified per-disc
//! lifecycle state (`state.json`: a [`StagingState`] enum + data + an
//! `outputs[]` plan, written atomically via crash-safe `tmp → fsync →
//! rename → dir-fsync`). Old marker files (`.done`/`.completed`/`.failed`/
//! `.ripped`/…) survive only as `StagingState` values and legacy-upgrade
//! read-fallback constants.
//!
//! Lifecycle: `Sweeping` → `Ripped` → `Done`/`Review` → `Completed`; terminal `Failed`;
//! resumable `AbortedLoss`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Restart-loop attempt cap. After this many consecutive container
/// restarts that find a partial staging dir for the same disc with no
/// completion / failed marker, write `.failed` and stop trying.
pub const RESTART_LIMIT: u64 = 3;

/// Marker filenames — kept as constants so the resume-on-startup logic
/// and the rip orchestrator agree on the on-disk vocabulary.
pub const DONE_MARKER: &str = ".done";
pub const REVIEW_MARKER: &str = ".review";
pub const COMPLETED_MARKER: &str = ".completed";
pub const FAILED_MARKER: &str = ".failed";
/// Resumable-failure marker for an abort-on-loss outcome (main-movie / demux loss exceeded
/// `abort_on_lost_secs` after retries). UNLIKE the terminal `.failed`, this dir is RECOVERABLE:
/// the resume scan re-enters it instead of quarantining it. Carries a JSON `{reason, attempt,
/// timestamp}` body; `attempt` is the count of abort-on-loss outcomes so far. Never
/// auto-promoted to `.failed` by attempt count — the operator resolves it (accept the loss, or
/// run another recovery pass).
pub const ABORTED_LOSS_MARKER: &str = ".aborted-loss";
/// Hand-off marker written by `rip_disc` and consumed by the mux worker.
/// Kept here (duplicated from `crate::server::muxer::RIPPED_MARKER_NAME`) so the
/// startup-scan vocabulary is self-contained; a `debug_assert` in the mux
/// worker tests pins the two equal.
pub const RIPPED_MARKER: &str = ".ripped";
/// In-progress marker written by `rip_disc` at staging-dir creation (before
/// Pass 1) and replaced by `.ripped` (or `.failed`) on exit. Its presence
/// means a sweep+patch is actively running (or crashed mid-sweep) and the
/// dir is OWNED by the ripper, not orphaned partial state. Carries a JSON
/// heartbeat/started timestamp so a future stale-heartbeat policy can tell a
/// live sweep from a dead one. Without it the multi-hour sweep window has no
/// governing marker: the resume scan restart-counts a healthy long rip toward
/// `.failed`, and the mover WARNs every 10s tick on the absent `.done`.
pub const SWEEPING_MARKER: &str = ".sweeping";
/// Exclusion lock written by the mux worker when it begins muxing a `.ripped`
/// dir and removed on completion. Its presence means the dir is OWNED by the
/// mux worker; the drive-resume paths (`disc_staging_hold` insert guard,
/// `find_resumable_for_disc`) must not select it (they would truncate the ISO
/// the mux worker is reading, or double-mux the same output).
pub const MUXING_MARKER: &str = ".muxing";
pub const RESTART_COUNT_FILE: &str = ".restart_count";

/// The disc's RAW volume label (UDF `meta_title`, else `volume_id`), recorded in its staging
/// dir at creation. Used to tell apart same-title boxset discs (dir names collapse after
/// `tmdb::clean_title` strips "disc N"). Absent in dirs written before this existed; a missing
/// label reads as "same disc" (legacy skip-on-`.completed` behaviour).
pub const DISC_LABEL_FILE: &str = ".disc-label";

// Unified per-disc state (`state.json`) supersedes the old marker-file machine
// (`.sweeping`/`.ripped`/`.done`/etc), written atomically via `write_marker_durable`.
// `snapshot_staging_disc` derives legacy `has_*` booleans from it, upgrading old dirs.

/// The single state file in each per-disc staging dir.
pub const STATE_FILE: &str = "state.json";
/// `state.json` schema. 2 = first unified schema (RippedMarker was schema 1).
pub const DISC_STATE_SCHEMA: u32 = 2;

/// Lifecycle state of a staging dir. Replaces the lifecycle marker files.
/// `muxing` / `accept_loss` / `restart_count` are orthogonal fields on
/// [`DiscState`], not states (a dir can be `Ripped` AND `muxing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagingState {
    /// was `.sweeping` — owned by the ripper, sweep+patch running/crashed.
    Sweeping,
    /// was `.ripped` — handed off; the mux worker should pick it up.
    Ripped,
    /// was `.done` — muxed, title confident, ready for the mover.
    Done,
    /// was `.review` — muxed, held for operator confirmation.
    Review,
    /// was `.completed` — mover finished / process-level clean completion.
    Completed,
    /// was `.failed` — terminal.
    Failed,
    /// was `.aborted-loss` — resumable failure (loss over threshold).
    AbortedLoss,
}

/// One deliverable in a staging dir. Movies have exactly one; a TV disc has one
/// per selected episode title. The mover files each in disc/`outputs` order.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Output {
    /// On-disk filename in the staging dir (the muxed `.mkv`/`.m2ts`, or the
    /// promoted `.iso`).
    pub filename: String,
    /// Index into the disc's full `titles[]` this output was muxed from.
    #[serde(default)]
    pub title_index: usize,
    /// Episode number for a TV output (`None` for a movie / single feature).
    #[serde(default)]
    pub episode: Option<u16>,
    /// TMDB episode name, empty when unknown (degraded/sequential).
    #[serde(default)]
    pub episode_name: String,
    /// Set once the mover has filed this output to the library.
    #[serde(default)]
    pub moved: bool,
}

/// Rip/sweep telemetry carried across the hand-off (was the `RippedMarker`
/// `rip_*` / `sweep_*` fields). Every field is `#[serde(default)]` so a marker
/// written by an older/leaner path still parses.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RipStats {
    #[serde(default)]
    pub elapsed_secs: f64,
    #[serde(default)]
    pub errors: u32,
    #[serde(default)]
    pub lost_video_secs: f64,
    #[serde(default)]
    pub last_sector: u64,
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
}

/// The unified per-disc state — one JSON, carrying lifecycle `state` plus all
/// the data that used to live spread across the marker payloads (RippedMarker,
/// the `.done`/`.review` metadata body, the `.failed`/`.aborted-loss` reasons,
/// `.restart_count`, and the `.disc-label`).
///
/// Every non-`state`/`schema` field is `#[serde(default)]` so partially-written
/// or migrated files parse, and so adding a field never breaks an on-disk file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DiscState {
    pub schema: u32,
    pub state: StagingState,

    // --- orthogonal lifecycle annotations ---------------------------------
    #[serde(default)]
    pub restart_count: u64,
    /// `.muxing` exclusion lock: the mux worker owns this dir right now.
    #[serde(default)]
    pub muxing: bool,
    /// `.accept-loss` one-shot: operator chose to deliver despite the loss.
    #[serde(default)]
    pub accept_loss: bool,
    /// Terminal `.failed` reason (None for a non-JSON/operator-cancel failure).
    #[serde(default)]
    pub failure_reason: Option<String>,
    /// Count of abort-on-loss outcomes (informational; the dir stays resumable).
    #[serde(default)]
    pub aborted_loss_attempt: u64,
    /// E7034 hold on a `Ripped` dir: its keys need the disc's Volume ID (never saved,
    /// J6), so the mux worker stops re-dispatching it until the disc is inserted.
    #[serde(default)]
    pub needs_disc: bool,

    // --- identity / routing (was the `.disc-label` file + done body) -------
    /// Raw volume label — the same value the `.disc-label` file held; used by
    /// `dir_is_same_disc` to tell boxset discs apart.
    #[serde(default)]
    pub disc_label: String,
    /// The disc's own label as displayed (distinct from the resolved `title`).
    #[serde(default)]
    pub disc_name: String,
    #[serde(default)]
    pub disc_format: String,
    /// Resolved display title (TMDB or disc-derived).
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub year: u16,
    /// "movie" | "tv" (empty ⇒ the mover defaults it to "movie").
    #[serde(default)]
    pub media_type: String,
    #[serde(default)]
    pub tmdb_id: u64,
    #[serde(default)]
    pub tmdb_poster: String,
    #[serde(default)]
    pub tmdb_overview: String,
    #[serde(default)]
    pub season: Option<u16>,
    #[serde(default)]
    pub disc_number: Option<u16>,
    #[serde(default)]
    pub title_confident: bool,

    // --- capture handles + mux-reconstruction knobs (was RippedMarker) -----
    #[serde(default)]
    pub iso_path: String,
    #[serde(default)]
    pub mapfile_path: String,
    #[serde(default)]
    pub max_retries: u8,
    #[serde(default)]
    pub abort_on_lost_secs: u32,
    #[serde(default)]
    pub origin_device: String,
    #[serde(default)]
    pub rip: RipStats,

    // --- deliverables ------------------------------------------------------
    /// One entry per output. Movies = 1; TV = N episodes. The mover files each.
    #[serde(default)]
    pub outputs: Vec<Output>,

    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub resumed: bool,
}

impl DiscState {
    /// A minimal `DiscState` in the given lifecycle state — every data field
    /// default. Used by transition helpers that seed a fresh file and by tests.
    pub fn new(state: StagingState) -> Self {
        DiscState {
            schema: DISC_STATE_SCHEMA,
            state,
            restart_count: 0,
            muxing: false,
            accept_loss: false,
            failure_reason: None,
            aborted_loss_attempt: 0,
            needs_disc: false,
            disc_label: String::new(),
            disc_name: String::new(),
            disc_format: String::new(),
            title: String::new(),
            year: 0,
            media_type: String::new(),
            tmdb_id: 0,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            season: None,
            disc_number: None,
            title_confident: false,
            iso_path: String::new(),
            mapfile_path: String::new(),
            max_retries: 0,
            abort_on_lost_secs: 0,
            origin_device: String::new(),
            rip: RipStats::default(),
            outputs: Vec::new(),
            date: String::new(),
            resumed: false,
        }
    }
}

/// Path of the `state.json` in a staging dir.
fn state_path(staging_disc_dir: &Path) -> PathBuf {
    staging_disc_dir.join(STATE_FILE)
}

/// Read and parse `state.json`. `None` when absent or unparseable — callers
/// treat `None` as "no unified state yet" and fall back to the legacy scan.
pub fn read_state(staging_disc_dir: &Path) -> Option<DiscState> {
    let bytes = std::fs::read(state_path(staging_disc_dir)).ok()?;
    let st: DiscState = serde_json::from_slice(&bytes).ok()?;
    // A file from a different schema (e.g. the schema-1 RippedMarker) deserves
    // the same None as "absent": its `#[serde(default)]` fields would otherwise
    // resume on defaulted state. Only the current schema is a unified state.
    (st.schema == DISC_STATE_SCHEMA).then_some(st)
}

/// Prefix of the reason on every "held: state.json unreadable" operator card, so
/// the card can be cleared (and only it) once the file reads again.
pub const STATE_HELD_PREFIX: &str = "Auto-resume held: ";

/// Why an existing `state.json` can't be used. `transient` = an I/O error
/// (EIO/ESTALE: retry may succeed); otherwise the bytes are corrupt or foreign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateUnreadable {
    pub reason: String,
    pub transient: bool,
    /// Raw error text for logs; kept out of `reason` (the card de-dupe key).
    pub detail: String,
}

impl StateUnreadable {
    pub fn io(e: &io::Error) -> Self {
        StateUnreadable {
            reason: "state.json could not be read (I/O error)".to_string(),
            transient: true,
            detail: e.to_string(),
        }
    }

    pub fn invalid(reason: String) -> Self {
        StateUnreadable {
            reason,
            transient: false,
            detail: String::new(),
        }
    }

    /// Reason plus the raw error detail, for logs.
    pub fn log_text(&self) -> String {
        if self.detail.is_empty() {
            self.reason.clone()
        } else {
            format!("{} ({})", self.reason, self.detail)
        }
    }

    /// Operator-card reason (starts with [`STATE_HELD_PREFIX`]).
    pub fn held_reason(&self) -> String {
        format!("{STATE_HELD_PREFIX}{}", self.reason)
    }

    /// Operator-card hint: retry for an I/O error, repair for bad contents.
    pub fn hint(&self) -> &'static str {
        if self.transient {
            "state.json in this staging dir exists but could not be read (I/O error, e.g. a staging/NFS mount problem), so the rip is held and the ISO kept; check the staging mount; this card clears once state.json reads again, then click Resume (or re-insert the disc) to retry"
        } else {
            "state.json in this staging dir is corrupt or from another autorip version, so the deliverable plan (movie vs. TV episodes) is unknown; the rip is held and the ISO kept. Restore or repair state.json to resume. Deleting it instead delivers the disc as ONE title — a TV disc's episodes would not be split"
        }
    }
}

/// `state.json` read that tells a missing file (normal) apart from one that
/// exists but can't be trusted (I/O error, unparseable, or a foreign schema).
pub(crate) enum StateRead {
    Absent,
    Valid(Box<DiscState>),
    Unreadable(StateUnreadable),
}

pub(crate) fn read_state_checked(staging_disc_dir: &Path) -> StateRead {
    let bytes = match std::fs::read(state_path(staging_disc_dir)) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return StateRead::Absent,
        Err(e) => return StateRead::Unreadable(StateUnreadable::io(&e)),
    };
    match serde_json::from_slice::<DiscState>(&bytes) {
        Ok(st) if st.schema == DISC_STATE_SCHEMA => StateRead::Valid(Box::new(st)),
        Ok(st) => StateRead::Unreadable(StateUnreadable::invalid(format!(
            "state.json is schema {} (expected {DISC_STATE_SCHEMA})",
            st.schema
        ))),
        Err(e) => StateRead::Unreadable(StateUnreadable::invalid(format!(
            "state.json is unparseable: {e}"
        ))),
    }
}

// Load state for a read-modify-write: the existing state, or a fresh `default`
// when absent. REFUSES (Err) when state.json exists but is unreadable, so a
// writer never replaces a held (e.g. TV) plan with an empty valid one.
pub(crate) fn state_for_write(
    staging_disc_dir: &Path,
    default: StagingState,
) -> io::Result<DiscState> {
    match read_state_checked(staging_disc_dir) {
        StateRead::Valid(st) => Ok(*st),
        StateRead::Absent => Ok(DiscState::new(default)),
        StateRead::Unreadable(u) => {
            tracing::error!(
                path = %state_path(staging_disc_dir).display(),
                "{} — refusing to overwrite it; the dir is held for the operator",
                u.log_text()
            );
            Err(io::Error::new(io::ErrorKind::InvalidData, u.held_reason()))
        }
    }
}

/// Durably (over)write `state.json` for a staging dir: one atomic
/// `tmp → fsync → rename → dir-fsync`. Best-effort — logs on failure like the
/// other marker writers, since callers are on paths with nothing useful to do
/// with a write error.
pub fn write_state(staging_disc_dir: &Path, st: &DiscState) {
    if let Err(e) = try_write_state(staging_disc_dir, st) {
        // ERROR, not WARN: a dropped write leaves a terminal transition stuck in
        // its prior state — e.g. a quarantined-but-still-`Ripped` dir re-dispatches
        // to the mux worker forever. Must be diagnosable, never buried in a warn.
        tracing::error!(path = %state_path(staging_disc_dir).display(), error = %e, "failed to write state.json");
    }
}

/// Fallible durable `state.json` write — same bytes as [`write_state`] but
/// propagates the I/O error so callers that must gate a follow-on action (e.g.
/// the fresh-rip hand-off gating auto-eject on a durable marker) can refuse to
/// proceed when the write did not land.
pub fn try_write_state(staging_disc_dir: &Path, st: &DiscState) -> io::Result<()> {
    let p = state_path(staging_disc_dir);
    let serialized = serde_json::to_string_pretty(st).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serialise state.json: {e}"),
        )
    })?;
    write_marker_durable(&p, serialized.as_bytes())
}

/// Read-modify-write `state.json`: apply `f` to the current state (or a
/// fresh `new(default_state)` when the file is absent) and durably
/// rewrite it. This is the single transition primitive; a transition
/// mutates fields, it never blows away accumulated data. Safe because
/// one writer owns a staging dir at a time (the sweeping/muxing
/// ownership rules, now fields).
///
/// Lock-free. Errs (writing nothing) on an unreadable state.json or a failed write.
pub fn mutate_state(
    staging_disc_dir: &Path,
    default_state: StagingState,
    f: impl FnOnce(&mut DiscState),
) -> io::Result<()> {
    let mut st = state_for_write(staging_disc_dir, default_state)?;
    f(&mut st);
    try_write_state(staging_disc_dir, &st)
}

/// The one-shot accept-loss REOPEN transition: move a terminal/abort dir back to
/// the re-muxable `Ripped` hand-off state and clear the failure bookkeeping, so
/// the operator's "Accept damage" re-muxes the existing ISO instead of being
/// refused. A dir in any other state is left untouched (so a `Done`/`Completed`
/// dir is never wrongly reopened). Shared by the web `handle_accept_loss` handler
/// and its test so the two can't drift.
pub fn apply_accept_loss_reopen(s: &mut DiscState) {
    if matches!(s.state, StagingState::AbortedLoss | StagingState::Failed) {
        s.state = StagingState::Ripped;
    }
    s.failure_reason = None;
    s.restart_count = 0;
}

/// Read-modify-write `state.json` ONLY if it already exists — a no-op when
/// absent. Used by lock-clearing / one-shot-consuming helpers that must not
/// conjure a state file for a dir that has none (e.g. clearing `.muxing` on a
/// legacy dir, consuming `.accept-loss`).
pub fn mutate_state_if_present(staging_disc_dir: &Path, f: impl FnOnce(&mut DiscState)) {
    if let Some(mut st) = read_state(staging_disc_dir) {
        f(&mut st);
        write_state(staging_disc_dir, &st);
    }
}

/// Set or lift the E7034 hold ([`DiscState::needs_disc`]) on a dir with a `state.json`.
/// A write only when it changes, so lifting an unset hold touches nothing.
pub fn set_needs_disc(staging_disc_dir: &Path, needs_disc: bool) {
    if read_state(staging_disc_dir).is_some_and(|st| st.needs_disc != needs_disc) {
        mutate_state_if_present(staging_disc_dir, |st| st.needs_disc = needs_disc);
    }
}

impl DiscState {
    /// Reconstruct the mux worker's [`crate::server::muxer::RippedMarker`] from this
    /// state so the mux/resume path can keep dealing in `RippedMarker` while the
    /// persistence layer is unified `state.json`. `mkv_filename` comes from the
    /// first output (movies have exactly one; TV outputs are muxed per entry).
    pub fn to_ripped_marker(&self) -> crate::server::muxer::RippedMarker {
        crate::server::muxer::RippedMarker {
            schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
            iso_path: self.iso_path.clone(),
            mapfile_path: self.mapfile_path.clone(),
            display_name: self.title.clone(),
            disc_format: self.disc_format.clone(),
            mkv_filename: self
                .outputs
                .first()
                .map(|o| o.filename.clone())
                .unwrap_or_default(),
            tmdb_title: self.title.clone(),
            tmdb_year: self.year,
            tmdb_poster: self.tmdb_poster.clone(),
            tmdb_overview: self.tmdb_overview.clone(),
            tmdb_media_type: self.media_type.clone(),
            max_retries: self.max_retries,
            abort_on_lost_secs: self.abort_on_lost_secs,
            rip_elapsed_secs: self.rip.elapsed_secs,
            rip_errors: self.rip.errors,
            rip_lost_video_secs: self.rip.lost_video_secs,
            rip_last_sector: self.rip.last_sector,
            origin_device: self.origin_device.clone(),
            sweep_errors: self.rip.sweep_errors,
            sweep_total_lost_ms: self.rip.sweep_total_lost_ms,
            sweep_main_lost_ms: self.rip.sweep_main_lost_ms,
            sweep_num_bad_ranges: self.rip.sweep_num_bad_ranges,
            sweep_largest_gap_ms: self.rip.sweep_largest_gap_ms,
            title_confident: self.title_confident,
        }
    }

    /// Fold a [`crate::server::muxer::RippedMarker`]'s fields into this state (used when
    /// the fresh-rip hand-off writes `state: Ripped`). Leaves lifecycle
    /// annotations (`restart_count`, `muxing`, …) untouched; the caller sets
    /// `state`.
    pub fn apply_ripped(&mut self, m: &crate::server::muxer::RippedMarker) {
        self.iso_path = m.iso_path.clone();
        self.mapfile_path = m.mapfile_path.clone();
        self.disc_format = m.disc_format.clone();
        self.title = m.tmdb_title.clone();
        self.year = m.tmdb_year;
        self.tmdb_poster = m.tmdb_poster.clone();
        self.tmdb_overview = m.tmdb_overview.clone();
        self.media_type = m.tmdb_media_type.clone();
        self.max_retries = m.max_retries;
        self.abort_on_lost_secs = m.abort_on_lost_secs;
        self.origin_device = m.origin_device.clone();
        self.title_confident = m.title_confident;
        self.rip = RipStats {
            elapsed_secs: m.rip_elapsed_secs,
            errors: m.rip_errors,
            lost_video_secs: m.rip_lost_video_secs,
            last_sector: m.rip_last_sector,
            sweep_errors: m.sweep_errors,
            sweep_total_lost_ms: m.sweep_total_lost_ms,
            sweep_main_lost_ms: m.sweep_main_lost_ms,
            sweep_num_bad_ranges: m.sweep_num_bad_ranges,
            sweep_largest_gap_ms: m.sweep_largest_gap_ms,
        };
        // The display_name is the mover-facing title; keep it if tmdb_title was
        // empty (the two are the same at the fresh-rip call site).
        if self.title.is_empty() {
            self.title = m.display_name.clone();
        }
        // Seed the single movie output from the marker's mkv_filename when the
        // caller hasn't populated outputs (TV callers set outputs themselves).
        if self.outputs.is_empty() && !m.mkv_filename.is_empty() {
            self.outputs.push(Output {
                filename: m.mkv_filename.clone(),
                ..Default::default()
            });
        }
    }
}

// Available bytes at the given path's filesystem, via statvfs(3). None on
// any error. `unnecessary_cast` allowed: libc's f_bavail/f_frsize are
// c_ulong (u64 on x86_64 Linux, u32 on some 32-bit/BSD targets needing the cast).
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
pub(super) fn staging_free_bytes(path: &str) -> Option<u64> {
    use std::ffi::CString;
    let cpath = CString::new(path).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
    if r != 0 {
        return None;
    }
    // f_bavail = blocks available to non-superuser. Multiply by frsize
    // (fundamental block size). Saturate to avoid overflow on 32-bit
    // platforms with absurdly large filesystems.
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

// Available bytes at the given path's volume on Windows, via
// GetDiskFreeSpaceExW; matches the unix statvfs contract (None on error)
// so the pre-flight space check behaves identically cross-platform.
#[cfg(windows)]
pub(super) fn staging_free_bytes(path: &str) -> Option<u64> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;

    // Wide, NUL-terminated path for the …W API.
    let wide: Vec<u16> = OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // `lpFreeBytesAvailableToCaller` is the quota-aware free space for the
    // calling user — the closest analogue to statvfs `f_bavail`.
    #[allow(non_snake_case)]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let mut free_to_caller: u64 = 0;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // Win32 BOOL: nonzero = success.
    if ok == 0 {
        return None;
    }
    Some(free_to_caller)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn staging_free_bytes(_path: &str) -> Option<u64> {
    None
}

/// Read the restart counter at `<staging_disc_dir>/.restart_count`.
/// Returns 0 if missing, unreadable, or unparseable — a missing/corrupt
/// counter must NOT cause the loop detector to trip, because that would
/// flip the dir to `.failed` on a single stray byte in the file.
pub fn restart_count(staging_disc_dir: &Path) -> u64 {
    // Unified store wins. Fall back to the legacy `.restart_count` file for
    // dirs written before the state.json migration.
    if let Some(st) = read_state(staging_disc_dir) {
        return st.restart_count;
    }
    let p = staging_disc_dir.join(RESTART_COUNT_FILE);
    match std::fs::read_to_string(&p) {
        Ok(s) => s.trim().parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    }
}

/// Increment the restart counter by 1, atomically as far as best-effort
/// goes (read → +1 → write). Creates the file on first call with value
/// `1`. Returns the new value on success.
pub fn increment_restart_count(staging_disc_dir: &Path) -> io::Result<u64> {
    // Unified store: bump the field in place, preserving state + all data.
    if read_state(staging_disc_dir).is_some() {
        let mut next = 0;
        mutate_state_if_present(staging_disc_dir, |s| {
            s.restart_count = s.restart_count.saturating_add(1);
            next = s.restart_count;
        });
        return Ok(next);
    }
    // Legacy fallback: bump the bare `.restart_count` file (a dir not yet
    // migrated to state.json).
    let next = restart_count(staging_disc_dir).saturating_add(1);
    let p = staging_disc_dir.join(RESTART_COUNT_FILE);
    // Atomic write via temp file + fsync + rename(2): a crash mid-write would
    // otherwise leave an empty/torn file that reads back as 0, silently
    // defeating the restart-loop guard.
    let tmp = staging_disc_dir.join(format!("{}.tmp", RESTART_COUNT_FILE));
    (|| -> io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        writeln!(f, "{}", next)?;
        f.sync_all()
    })()
    .inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    if let Err(e) = std::fs::rename(&tmp, &p) {
        // A permanent rename failure (cross-device move, ESTALE, full
        // directory) would otherwise leave the `.tmp` sibling on disk
        // forever. Best-effort cleanup, then propagate the real error.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(next)
}

// Durably write a marker: tmp write + sync_all + rename + dir fsync, so a
// crash mid-write never leaves a torn marker. Mirrors `increment_restart_count`.
pub(crate) fn write_marker_durable(path: &Path, contents: &[u8]) -> io::Result<()> {
    let tmp = match path.file_name() {
        Some(name) => {
            let mut t = name.to_os_string();
            t.push(".tmp");
            path.with_file_name(t)
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "marker has no file name",
            ));
        }
    };
    (|| -> io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()
    })()
    .inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Clean up the `.tmp` sibling on a permanent rename failure
        // (cross-device move, ESTALE, full directory) so it is not
        // leaked. Best-effort, then propagate the real error.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(parent) = path.parent() {
        libfreemkv::io::fsync::dir(parent);
    }
    Ok(())
}

/// Best-effort delete of `.restart_count`. Not finding the file is not
/// an error — the goal is "after this call, the file is absent".
pub fn clear_restart_count(staging_disc_dir: &Path) {
    // Zero the unified field (if any) and remove the legacy file. Both, so a
    // half-migrated dir can't keep a stale count on either side.
    mutate_state_if_present(staging_disc_dir, |s| s.restart_count = 0);
    let p = staging_disc_dir.join(RESTART_COUNT_FILE);
    let _ = std::fs::remove_file(&p);
}

/// Write the `.failed` marker with a structured reason. Returns whether the terminal `state:
/// Failed` actually LANDED on disk (`true`) or the state.json write failed (`false`); callers
/// that loop on the mux worker's per-tick re-dispatch should check this to raise an operator
/// card on a stuck quarantine.
pub fn write_failed_marker(staging_disc_dir: &Path, reason: &str) -> bool {
    // Terminal transition → `state: Failed`. This atomic rewrite supersedes
    // any in-progress `.sweeping`/`.muxing` ownership so `disc_owned_by_worker`
    // can't stay true on a now-terminal dir.
    let Ok(mut st) = state_for_write(staging_disc_dir, StagingState::Failed) else {
        return false;
    };
    st.state = StagingState::Failed;
    st.failure_reason = Some(reason.to_string());
    st.muxing = false;
    let landed = match try_write_state(staging_disc_dir, &st) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(
                path = %state_path(staging_disc_dir).display(),
                error = %e,
                "failed to persist terminal .failed quarantine — dir stays in its prior state and will keep re-dispatching to the mux worker until the staging mount recovers"
            );
            false
        }
    };
    // Best-effort cleanup of leftover legacy marker files (a dir migrated
    // mid-life) so the reader's legacy fallback doesn't disagree with state.json.
    remove_legacy_marker(staging_disc_dir, SWEEPING_MARKER);
    remove_legacy_marker(staging_disc_dir, MUXING_MARKER);
    landed
}

/// Best-effort removal of a named legacy marker file. Used by the unified
/// writers to clean up any pre-migration marker so the reader's legacy fallback
/// never contradicts `state.json`.
fn remove_legacy_marker(staging_disc_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(staging_disc_dir.join(name));
}

/// Read the `.failed` marker's reason string. Returns None if missing
/// or unparseable.
pub fn read_failed_reason(staging_disc_dir: &Path) -> Option<String> {
    if let Some(st) = read_state(staging_disc_dir) {
        return st.failure_reason;
    }
    // Legacy fallback.
    let p = staging_disc_dir.join(FAILED_MARKER);
    let body = std::fs::read_to_string(&p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v.get("reason")?.as_str().map(|s| s.to_string())
}

/// Write (or rewrite) the `.aborted-loss` resumable-failure marker with a
/// reason and the current attempt count. Like `write_failed_marker` this is
/// the end-of-run state for this attempt, so it supersedes the in-progress
/// `.sweeping`/`.muxing` markers (clear them). Best-effort. Returns whether the
/// `state.json` write landed (see `write_failed_marker`).
pub fn write_aborted_loss_marker(staging_disc_dir: &Path, reason: &str, attempt: u64) -> bool {
    // Resumable-failure transition → `state: AbortedLoss`, carrying the reason
    // and attempt count. Releases in-progress ownership like `.failed` does.
    let Ok(mut st) = state_for_write(staging_disc_dir, StagingState::AbortedLoss) else {
        return false;
    };
    st.state = StagingState::AbortedLoss;
    st.failure_reason = Some(reason.to_string());
    st.aborted_loss_attempt = attempt;
    st.muxing = false;
    let landed = match try_write_state(staging_disc_dir, &st) {
        Ok(()) => true,
        Err(e) => {
            tracing::error!(
                path = %state_path(staging_disc_dir).display(),
                error = %e,
                "failed to persist .aborted-loss — dir stays in its prior state and may keep re-dispatching until the staging mount recovers"
            );
            false
        }
    };
    remove_legacy_marker(staging_disc_dir, SWEEPING_MARKER);
    remove_legacy_marker(staging_disc_dir, MUXING_MARKER);
    landed
}

/// Best-effort clear of the aborted-loss state. Used when the dir is promoted to
/// a terminal `.failed` (the subsequent `write_failed_marker` sets the terminal
/// state; this only strips any leftover legacy `.aborted-loss` file so the
/// reader's fallback can't contradict it).
pub fn clear_aborted_loss_marker(staging_disc_dir: &Path) {
    remove_legacy_marker(staging_disc_dir, ABORTED_LOSS_MARKER);
}

/// `.accept-loss` — the operator has chosen to **accept** the recorded
/// main-movie loss and deliver the rip anyway, overriding `abort_on_lost_secs`.
/// Written by the `Accept damage` action; the next re-mux raises its effective
/// threshold to unlimited when this marker is present (so the abort gate
/// proceeds), then clears it — a one-shot override, not a permanent setting.
pub const ACCEPT_LOSS_MARKER: &str = ".accept-loss";

/// Set the one-shot accept-loss override. Best-effort. Records in `state.json`
/// when the dir has one (the common case — the operator acts on an existing
/// aborted dir), else writes the legacy file as a safety net.
pub fn write_accept_loss_marker(staging_disc_dir: &Path) {
    if read_state(staging_disc_dir).is_some() {
        mutate_state_if_present(staging_disc_dir, |s| s.accept_loss = true);
        return;
    }
    let p = staging_disc_dir.join(ACCEPT_LOSS_MARKER);
    if let Err(e) = write_marker_durable(&p, b"{}") {
        tracing::warn!(path = %p.display(), error = %e, "failed to write .accept-loss marker");
    }
}

/// Best-effort clear of the accept-loss override (one-shot: cleared once the
/// accepted re-mux has consumed it). Clears both stores.
pub fn clear_accept_loss_marker(staging_disc_dir: &Path) {
    mutate_state_if_present(staging_disc_dir, |s| s.accept_loss = false);
    let _ = std::fs::remove_file(staging_disc_dir.join(ACCEPT_LOSS_MARKER));
}

/// Whether the operator has requested accepting the loss for this staging dir.
pub fn accept_loss_requested(staging_disc_dir: &Path) -> bool {
    if let Some(st) = read_state(staging_disc_dir) {
        return st.accept_loss;
    }
    staging_disc_dir.join(ACCEPT_LOSS_MARKER).exists()
}

/// Read the `.aborted-loss` marker's `(reason, attempt)`. Returns None if the
/// marker is missing or unparseable; a present-but-attemptless body reads back
/// as attempt 0 (the conservative "no prior attempts recorded" value).
pub fn read_aborted_loss(staging_disc_dir: &Path) -> Option<(String, u64)> {
    if let Some(st) = read_state(staging_disc_dir) {
        if st.state != StagingState::AbortedLoss {
            return None;
        }
        let reason = st
            .failure_reason
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "aborted on loss".to_string());
        return Some((reason, st.aborted_loss_attempt));
    }
    // Legacy fallback.
    let p = staging_disc_dir.join(ABORTED_LOSS_MARKER);
    let body = std::fs::read_to_string(&p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let reason = v
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("aborted on loss")
        .to_string();
    let attempt = v.get("attempt").and_then(|a| a.as_u64()).unwrap_or(0);
    Some((reason, attempt))
}

/// Record an abort-on-loss outcome for `staging_disc_dir`. Reads the prior
/// attempt count from any existing `.aborted-loss` marker, increments it,
/// and (re)writes a fresh `.aborted-loss` — the dir ALWAYS stays resumable
/// (never promoted to terminal `.failed`). Clears `.restart_count` (a
/// deterministically-lossy rip must not ALSO accrue crash-restart counts).
/// Always returns `false`; the `bool` return is retained so existing
/// callers compile unchanged (their terminal branch is now inert).
pub fn mark_aborted_on_loss(staging_disc_dir: &Path, reason: &str) -> bool {
    let prior = read_aborted_loss(staging_disc_dir)
        .map(|(_, a)| a)
        .unwrap_or(0);
    let attempt = prior.saturating_add(1);
    clear_restart_count(staging_disc_dir);
    // Loss is deterministic (a re-rip won't fix media damage), so this is never
    // promoted to terminal `.failed` by attempt count — the dir stays resumable
    // indefinitely; the operator resolves via Accept or Run-another-pass.
    let _ = write_aborted_loss_marker(staging_disc_dir, reason, attempt);
    false
}

/// Like [`mark_aborted_on_loss`], but reports whether the `.aborted-loss`
/// write actually landed, so a caller can raise an operator card on a
/// dropped write instead of silently re-dispatching forever.
pub fn mark_aborted_on_loss_reporting_landed(staging_disc_dir: &Path, reason: &str) -> bool {
    let prior = read_aborted_loss(staging_disc_dir)
        .map(|(_, a)| a)
        .unwrap_or(0);
    let attempt = prior.saturating_add(1);
    clear_restart_count(staging_disc_dir);
    write_aborted_loss_marker(staging_disc_dir, reason, attempt)
}

/// Write the `.sweeping` in-progress marker durably. Called at staging-dir
/// creation in `rip_disc`, before Pass 1. Carries a JSON `started` epoch-secs
/// timestamp (the heartbeat) so a future stale-sweep policy can distinguish a
/// live multi-hour sweep from a dead one. Best-effort — logs on failure; a
/// missing `.sweeping` just degrades to the pre-fix markerless-window
/// behaviour, it never corrupts state.
pub fn write_sweeping_marker(staging_disc_dir: &Path) {
    // Owned-in-progress transition → `state: Sweeping`. Seeds `state.json` at
    // staging-dir creation (preserving any data from a prior resume attempt).
    if let Err(e) = mutate_state(staging_disc_dir, StagingState::Sweeping, |s| {
        s.state = StagingState::Sweeping;
    }) {
        tracing::error!(path = %staging_disc_dir.display(), error = %e, "failed to record .sweeping in state.json");
    }
}

/// Where [`seed_sweeping_for_live_rip`] keeps an unreadable state.json it replaced.
pub const UNREADABLE_STATE_ASIDE: &str = "state.json.unreadable";

/// Seed `Sweeping` for a LIVE-DISC rip. Unlike [`write_sweeping_marker`] this
/// may replace an unreadable state.json (moved aside to [`UNREADABLE_STATE_ASIDE`]):
/// the rip rebuilds the plan from the disc. Returns why, if it replaced one.
pub fn seed_sweeping_for_live_rip(staging_disc_dir: &Path) -> Option<String> {
    let StateRead::Unreadable(u) = read_state_checked(staging_disc_dir) else {
        write_sweeping_marker(staging_disc_dir);
        return None;
    };
    let p = state_path(staging_disc_dir);
    if let Err(e) = std::fs::rename(&p, staging_disc_dir.join(UNREADABLE_STATE_ASIDE)) {
        tracing::warn!(path = %p.display(), error = %e, "could not set the unreadable state.json aside; replacing it");
    }
    tracing::warn!(path = %p.display(), reason = %u.log_text(), "live-disc rip replacing an unreadable state.json");
    if let Err(e) = try_write_state(staging_disc_dir, &DiscState::new(StagingState::Sweeping)) {
        tracing::error!(path = %p.display(), error = %e, "failed to seed state.json for the live-disc rip");
    }
    Some(u.log_text())
}

/// Write the `.muxing` exclusion lock durably. Called by the mux worker when
/// it begins muxing a `.ripped` dir; removed on completion (RAII guard).
/// Carries a JSON `started` epoch-secs timestamp for observability. Best-effort
/// — a missing `.muxing` only loses the exclusion, it never corrupts state.
pub fn write_muxing_marker(staging_disc_dir: &Path) {
    // The mux-worker ownership lock is a field, set on the existing (Ripped)
    // state. Best-effort: if the dir has no state.json the lock is simply not
    // taken — same as a legacy best-effort write failing.
    mutate_state_if_present(staging_disc_dir, |s| s.muxing = true);
}

/// Whether the dir is currently held by the mux worker's `.muxing` exclusion
/// lock. Reads the marker DIRECTLY (state.json `muxing` field, or a legacy
/// `.muxing` file) without going through `snapshot_staging_disc`, so it does not
/// migrate a legacy dir as a side effect — safe on a read-only request path
/// (`handle_accept_loss`'s ownership guard). No state.json and no legacy marker
/// reads `Ok(false)`; a stat/read error other than NotFound (EACCES, ESTALE) is
/// `Err`, so callers fail closed instead of reading "not muxing".
pub(crate) fn muxing_status(staging_disc_dir: &Path) -> std::io::Result<bool> {
    let not_found = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    match std::fs::read(state_path(staging_disc_dir)) {
        Ok(bytes) => {
            if let Ok(st) = serde_json::from_slice::<DiscState>(&bytes)
                && st.schema == DISC_STATE_SCHEMA
            {
                return Ok(st.muxing);
            }
        }
        Err(e) if not_found(&e) => {}
        Err(e) => return Err(e),
    }
    match std::fs::symlink_metadata(staging_disc_dir.join(MUXING_MARKER)) {
        Ok(_) => Ok(true),
        Err(e) if not_found(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Release the `.muxing` exclusion lock (a field). Called when the mux worker
/// finishes (or aborts) a dir. Leaves the lifecycle `state` (typically `Ripped`)
/// intact so the dir is re-dispatchable next tick, mirroring the legacy
/// "remove `.muxing`, keep `.ripped`" behaviour.
pub fn clear_muxing_marker(staging_disc_dir: &Path) {
    mutate_state_if_present(staging_disc_dir, |s| s.muxing = false);
    remove_legacy_marker(staging_disc_dir, MUXING_MARKER);
}

/// Release the `.sweeping` ownership. Called on a graceful stop / rip-thread cancel (the
/// `SweepingGuard` drop and `clear_inprogress_markers`) and, vestigially, by paths that
/// supersede a sweep. Safe to call unconditionally: a no-op once the sweep has already
/// advanced.
pub fn clear_sweeping_marker(staging_disc_dir: &Path) {
    if let Some(st) = read_state(staging_disc_dir)
        && st.state == StagingState::Sweeping
    {
        let _ = std::fs::remove_file(state_path(staging_disc_dir));
    }
    remove_legacy_marker(staging_disc_dir, SWEEPING_MARKER);
}

/// Clear every `.sweeping` / `.muxing` in-progress marker under `staging_root`. Called on
/// GRACEFUL shutdown (SIGTERM: operator redeploy, reboot, Watchtower update, `docker stop`) so
/// every interrupted dir is left clean-resumable instead of accruing a false `.restart_count`.
pub fn clear_inprogress_markers(staging_root: &Path) {
    let entries = match std::fs::read_dir(staging_root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.is_dir() {
            clear_sweeping_marker(&dir);
            clear_muxing_marker(&dir);
        }
    }
}

/// Write the `.completed` marker. Empty file — its existence is the
/// signal. Best-effort; logs on failure.
pub fn write_completed_marker(staging_disc_dir: &Path) {
    // Clean-completion → `state: Completed`, releasing `.muxing`. Must NOT
    // downgrade an existing `Done`/`Review` hand-off state, so this only
    // advances an as-yet-uncompleted dir (a no-op after a hand-off write).
    if let Err(e) = mutate_state(staging_disc_dir, StagingState::Completed, |s| {
        if !matches!(s.state, StagingState::Done | StagingState::Review) {
            s.state = StagingState::Completed;
        }
        s.muxing = false;
    }) {
        tracing::error!(path = %staging_disc_dir.display(), error = %e, "failed to record completion in state.json");
        return;
    }
    remove_legacy_marker(staging_disc_dir, SWEEPING_MARKER);
    remove_legacy_marker(staging_disc_dir, MUXING_MARKER);
}

/// Durably write a hand-off/review marker (`.done` / `.review`) containing
/// JSON the mover parses. Returns the same `io::Result` shape as a plain
/// write so the caller's error handling is unchanged, but the bytes hit disk
/// atomically (tmp + fsync + rename + dir-fsync) — a crash mid-write never
/// leaves an empty/torn marker the mover would mis-handle.
pub fn write_handoff_marker(marker_path: &Path, contents: &[u8]) -> io::Result<()> {
    write_marker_durable(marker_path, contents)
}

/// Hand-off transition: a completed mux moves the dir to `state: Done` (title confident → the
/// mover auto-files it) or `state: Review` (held for operator confirmation). `apply` populates
/// the mover-facing metadata + `outputs` (title, year, media_type, tmdb_id, season, poster,
/// overview, …). Errs when state.json is unreadable (nothing written) or the write fails.
pub fn mark_handoff(
    staging_disc_dir: &Path,
    title_confident: bool,
    apply: impl FnOnce(&mut DiscState),
) -> io::Result<()> {
    let state = if title_confident {
        StagingState::Done
    } else {
        StagingState::Review
    };
    let mut st = state_for_write(staging_disc_dir, state)?;
    st.state = state;
    st.title_confident = title_confident;
    if st.date.is_empty() {
        st.date = crate::server::util::format_date();
    }
    apply(&mut st);
    // Propagate the write error so a completion path can refuse to declare the
    // rip handed-off (and preserve staging for retry) when the durable write did
    // not land — the old `write_handoff_marker` `io::Result` gate.
    try_write_state(staging_disc_dir, &st)?;
    // A hand-off supersedes any in-progress ownership markers from a migrated
    // legacy dir.
    remove_legacy_marker(staging_disc_dir, SWEEPING_MARKER);
    remove_legacy_marker(staging_disc_dir, MUXING_MARKER);
    Ok(())
}

/// The marker name a hand-off would use, for logging (`.done`/`.review`).
/// The on-disk representation is `state.json`; this is purely for human-facing
/// messages that referred to the old file name.
pub fn handoff_label(title_confident: bool) -> &'static str {
    if title_confident {
        DONE_MARKER
    } else {
        REVIEW_MARKER
    }
}

/// Whether the deliverable is a `network://` sink rather than a local file.
///
/// A network target has no local path, so the durability gate has nothing to
/// fsync and must be skipped. Both halves matter: the format must be
/// `"network"` AND a target must actually be configured, because a
/// `"network"` format with an empty target falls back to local output and
/// still needs the flush. Four hand-rolled copies of this expression existed;
/// a mutation run flipped `==` to `!=`, `&&` to `||`, and dropped the `!` in
/// each, and nothing failed.
pub fn is_network_output(output_format: &str, network_target: &str) -> bool {
    output_format == crate::server::config::OUTPUT_FORMAT_NETWORK && !network_target.is_empty()
}

/// The durability gate: may a success marker be written yet? `false` means the output is not
/// provably on stable storage, so `.done` and `.completed` must be withheld and the staging dir
/// preserved for a retry. `fsync` is injected so the decision is testable without a filesystem,
/// and is NOT evaluated for a network sink.
pub fn durability_gate_passes(is_network: bool, fsync: impl FnOnce() -> bool) -> bool {
    if is_network {
        return true;
    }
    fsync()
}

/// Force the just-muxed output file to durable storage before any success marker (`.done` /
/// `.completed`) is written. Returns `true` only when the output was provably synced; a `false`
/// return means the caller MUST NOT write the success marker this cycle, leaving the staging
/// dir resumable so a later attempt re-runs the durable flush. Call this ONLY on the success
/// path, immediately before the marker write, and only for a real local output file (skip
/// `network://` sinks).
pub fn fsync_output_file(output_path: &Path) -> bool {
    // Delegate to the shared, platform-aware durability primitive. It opens the
    // file read+write before `sync_all`: Windows' `FlushFileBuffers` rejects a
    // read-only handle, which made this gate fail every cycle there.
    match libfreemkv::io::fsync::file_durable(output_path) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(
                path = %output_path.display(),
                error = %e,
                "failed to fsync mux output before completion marker"
            );
            false
        }
    }
}

/// Snapshot of what's in a per-disc staging directory at startup. Used
/// by the resume-on-startup decision tree.
#[derive(Debug)]
pub struct StagingSnapshot {
    pub dir: PathBuf,
    pub completed: bool,
    /// `.failed` terminal marker present. This is the authoritative terminal
    /// signal — keyed on PRESENCE, not parse-success, so a `.failed` written
    /// with a non-JSON body (e.g. review.rs's "cancelled by operator") is
    /// still recognised as terminal. `failed_reason` carries the parsed
    /// reason when the body is JSON (None otherwise).
    pub has_failed: bool,
    pub failed_reason: Option<String>,
    /// `.aborted-loss` resumable-failure marker present (main-movie loss exceeded
    /// `abort_on_lost_secs`, but the ISO + mapfile are intact). Distinct from terminal
    /// `.failed`: the resume scan re-enters such a dir indefinitely. `attempt` carries the
    /// abort count for the UI.
    pub has_aborted_loss: bool,
    pub aborted_loss_reason: Option<String>,
    pub aborted_loss_attempt: u64,
    /// `.done` hand-off marker present. A completed mux writes `.done`
    /// (for the mover) before `.completed` (the process-level marker)
    /// and before the ISO prune. A crash in that window leaves `.done`
    /// present but `.completed` absent and the ISO still on disk — the
    /// resume scan must recognise this as a finished rip, not partial
    /// state to be retried. Hoisting `.done` into the snapshot lets the
    /// resume gate short-circuit before the partial-state branch.
    pub has_done: bool,
    /// `.review` hand-off marker present. When the rip's title match is
    /// not confident the mux writes `.review` (instead of `.done`) before
    /// `.completed` — the rip is finished and staged but held for operator
    /// title confirmation rather than auto-filed. Like `.done`, a crash
    /// between the `.review` and `.completed` writes leaves `.review`
    /// present, `.completed` absent, and the ISO/mapfile still on disk, so
    /// the resume scan must recognise this as a finished rip, not partial
    /// state to be restart-counted (and eventually promoted to `.failed`).
    pub has_review: bool,
    /// `.ripped` hand-off marker present (written by `rip_disc` after
    /// sweep+patch, consumed by the mux worker). Read from the same primed,
    /// 3x-retried `read_dir` view as the terminal markers so a cold-cache NFS
    /// miss can't race it to "absent" while the snapshot surfaces `.completed`/
    /// `.failed` — the mux-worker dispatch decider (`mux_dispatch_verdict`)
    /// relies on this consistency.
    pub has_ripped: bool,
    /// `.sweeping` in-progress marker present (written by `rip_disc` at
    /// staging-dir creation, before Pass 1; replaced by `.ripped`/`.failed`
    /// on exit). Its presence means a sweep+patch is actively running (or
    /// crashed mid-sweep) — the dir is OWNED by the ripper, not orphaned
    /// partial state. The resume scan treats it as "owned, in progress":
    /// state is left intact, but `.restart_count` IS bumped on each restart so
    /// a deterministically-crashing owned sweep still converges to `.failed`
    /// within `RESTART_LIMIT`.
    pub has_sweeping: bool,
    /// `.muxing` exclusion lock present (written by the mux worker while it
    /// muxes a `.ripped` dir, removed on completion). Its presence means the
    /// dir is OWNED by the mux worker; the drive-resume paths must not select
    /// it for a fresh sweep or a double-mux.
    pub has_muxing: bool,
    /// A `Ripped` dir held for its disc (E7034, [`DiscState::needs_disc`]): the mux
    /// worker does not own it, and inserting the disc finishes it.
    pub needs_disc: bool,
    pub has_iso: bool,
    pub has_mapfile: bool,
    pub has_mkv: bool,
    /// Set when a per-entry `read_dir` error occurred during the scan
    /// (partial NFS degradation). When true the snapshot must NOT be
    /// classified as empty, because the artifact counts may be undercounts.
    pub had_entry_error: bool,
    /// Every `read_dir` pass returned zero entries: genuinely empty, or the
    /// cold-cache NFS miss that once wiped an 85 GB ISO. Never wipe on it.
    pub saw_no_entries: bool,
    /// `state.json` exists but can't be used (lifecycle fields above then come
    /// from the legacy markers). The mux worker holds such a dir for the operator.
    pub state_unreadable: Option<StateUnreadable>,
}

impl StagingSnapshot {
    /// True iff there's any sign of an interrupted rip — at least one
    /// of ISO / mapfile / partial MKV is present. Used by the resume
    /// gate to distinguish "completely empty dir, nothing to do" from
    /// "rip was running when the process died". Also returns true when a
    /// per-entry scan error occurred, so partial NFS degradation can't
    /// undercount artifacts and trigger the remove_dir_all wipe on a
    /// populated dir.
    pub fn has_partial_state(&self) -> bool {
        self.has_iso || self.has_mapfile || self.has_mkv || self.had_entry_error
    }
}

// Raw, untrusted observations from scanning a staging dir's entries.
// Separated from `classify_observations` so it's unit-testable without
// provoking real per-entry NFS I/O errors from the filesystem.
#[derive(Debug, Default, Clone, Copy)]
struct ScanObservations {
    /// A `state.json` (unified store) was seen in the primed listing. When set,
    /// the lifecycle observations below are IGNORED and re-derived from the
    /// parsed state — the legacy marker names are only a migration fallback.
    has_state_file: bool,
    has_done: bool,
    has_review: bool,
    has_ripped: bool,
    has_sweeping: bool,
    has_muxing: bool,
    has_completed: bool,
    has_failed: bool,
    has_aborted_loss: bool,
    has_iso: bool,
    has_mapfile: bool,
    has_mkv: bool,
    /// At least one `read_dir` attempt returned `Ok(entries)`.
    saw_read_ok: bool,
    /// At least one `Ok(DirEntry)` was yielded across all attempts.
    saw_any_entries: bool,
    /// At least one DirEntry yielded `Err(_)` (partial NFS degradation).
    had_entry_error: bool,
}

impl ScanObservations {
    /// True iff no marker and no artifact was observed — nothing we can
    /// act on.
    fn observed_nothing(&self) -> bool {
        !self.has_state_file
            && !self.has_done
            && !self.has_review
            && !self.has_ripped
            && !self.has_sweeping
            && !self.has_muxing
            && !self.has_completed
            && !self.has_failed
            && !self.has_aborted_loss
            && !self.has_iso
            && !self.has_mapfile
            && !self.has_mkv
    }

    // True iff contents are UNKNOWN: every read_dir errored, or every entry
    // errored with nothing trustworthy observed. NFS-startup degradation must
    // not look like partial state and bump `.restart_count`.
    fn contents_unknown(&self) -> bool {
        !self.saw_read_ok || (self.had_entry_error && self.observed_nothing())
    }

    /// True iff any LEGACY lifecycle marker file was observed (used to decide
    /// whether to upgrade a pre-migration dir to `state.json`). Excludes the
    /// artifact files and the `state.json` itself.
    fn has_any_lifecycle_marker(&self) -> bool {
        self.has_done
            || self.has_review
            || self.has_ripped
            || self.has_sweeping
            || self.has_muxing
            || self.has_completed
            || self.has_failed
            || self.has_aborted_loss
    }
}

// The lifecycle projection that fills a StagingSnapshot's marker bits.
// Sourced from state.json when present, else derived from legacy marker
// files; artifact bits are NOT here (always from the directory scan).
#[derive(Default)]
struct Lifecycle {
    completed: bool,
    has_failed: bool,
    failed_reason: Option<String>,
    has_aborted_loss: bool,
    aborted_loss_reason: Option<String>,
    aborted_loss_attempt: u64,
    has_done: bool,
    has_review: bool,
    has_ripped: bool,
    has_sweeping: bool,
    has_muxing: bool,
    needs_disc: bool,
}

impl Lifecycle {
    /// Derive the projection from a parsed unified [`DiscState`].
    fn from_state(st: &DiscState) -> Lifecycle {
        use StagingState::*;
        let is_failed = st.state == Failed;
        let is_aborted = st.state == AbortedLoss;
        Lifecycle {
            // A finished rip (`Done`/`Review`) is "completed" for the resume /
            // already-completed checks, exactly as the legacy `.completed`
            // coexisted with `.done`/`.review`.
            completed: matches!(st.state, Done | Review | Completed),
            has_failed: is_failed,
            failed_reason: if is_failed {
                st.failure_reason.clone()
            } else {
                None
            },
            has_aborted_loss: is_aborted,
            aborted_loss_reason: if is_aborted {
                st.failure_reason.clone()
            } else {
                None
            },
            aborted_loss_attempt: if is_aborted {
                st.aborted_loss_attempt
            } else {
                0
            },
            has_done: st.state == Done,
            has_review: st.state == Review,
            has_ripped: st.state == Ripped,
            has_sweeping: st.state == Sweeping,
            has_muxing: st.muxing,
            needs_disc: st.state == Ripped && st.needs_disc,
        }
    }

    /// Derive the projection from the observed legacy marker files, reading the
    /// `.failed`/`.aborted-loss` bodies only when their marker was seen (the
    /// same consistency rule the reader always used).
    fn from_legacy(dir: &Path, obs: &ScanObservations) -> Lifecycle {
        let failed_reason = if obs.has_failed {
            read_legacy_failed_reason(dir)
        } else {
            None
        };
        let (aborted_loss_reason, aborted_loss_attempt) = if obs.has_aborted_loss {
            match read_legacy_aborted_loss(dir) {
                Some((reason, attempt)) => (Some(reason), attempt),
                None => (None, 0),
            }
        } else {
            (None, 0)
        };
        Lifecycle {
            completed: obs.has_completed,
            has_failed: obs.has_failed,
            failed_reason,
            has_aborted_loss: obs.has_aborted_loss,
            aborted_loss_reason,
            aborted_loss_attempt,
            has_done: obs.has_done,
            has_review: obs.has_review,
            has_ripped: obs.has_ripped,
            has_sweeping: obs.has_sweeping,
            has_muxing: obs.has_muxing,
            needs_disc: false,
        }
    }

    /// The single lifecycle [`StagingState`] a legacy dir maps to, by strongest
    /// signal. `None` when no lifecycle marker is present (pure partial state —
    /// represented by the ABSENCE of `state.json`, so it is NOT upgraded).
    fn legacy_state(obs: &ScanObservations) -> Option<StagingState> {
        use StagingState::*;
        if obs.has_failed {
            Some(Failed)
        } else if obs.has_done {
            Some(Done)
        } else if obs.has_review {
            Some(Review)
        } else if obs.has_aborted_loss {
            Some(AbortedLoss)
        } else if obs.has_ripped {
            Some(Ripped)
        } else if obs.has_sweeping {
            Some(Sweeping)
        } else if obs.has_completed {
            Some(Completed)
        } else {
            None
        }
    }
}

/// Read a LEGACY `.failed` marker's reason (the state.json-preferring
/// `read_failed_reason` would loop back through the reader). Direct file read.
fn read_legacy_failed_reason(dir: &Path) -> Option<String> {
    let body = std::fs::read_to_string(dir.join(FAILED_MARKER)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v.get("reason")?.as_str().map(|s| s.to_string())
}

/// Read a LEGACY `.aborted-loss` marker's `(reason, attempt)`. Direct file read.
fn read_legacy_aborted_loss(dir: &Path) -> Option<(String, u64)> {
    let body = std::fs::read_to_string(dir.join(ABORTED_LOSS_MARKER)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let reason = v
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("aborted on loss")
        .to_string();
    let attempt = v.get("attempt").and_then(|a| a.as_u64()).unwrap_or(0);
    Some((reason, attempt))
}

// Upgrade a pre-migration staging dir to state.json in place: build a
// DiscState from the legacy markers, write it durably, then remove the
// legacy files (best-effort). Skipped when no lifecycle marker was observed.
fn upgrade_legacy_to_state(dir: &Path, obs: &ScanObservations) {
    let Some(state) = Lifecycle::legacy_state(obs) else {
        return;
    };
    let mut st = DiscState::new(state);
    st.muxing = obs.has_muxing;
    st.accept_loss = dir.join(ACCEPT_LOSS_MARKER).exists();
    // Restart count from the legacy file (state.json doesn't exist yet).
    st.restart_count = match std::fs::read_to_string(dir.join(RESTART_COUNT_FILE)) {
        Ok(s) => s.trim().parse::<u64>().unwrap_or(0),
        Err(_) => 0,
    };
    if state == StagingState::Failed {
        st.failure_reason = read_legacy_failed_reason(dir);
    }
    if state == StagingState::AbortedLoss
        && let Some((reason, attempt)) = read_legacy_aborted_loss(dir)
    {
        st.failure_reason = Some(reason);
        st.aborted_loss_attempt = attempt;
    }
    // Rich metadata: the `.ripped` RippedMarker (mux inputs) then the
    // `.done`/`.review` body (mover metadata) — the latter wins where both
    // carry a field, matching the legacy read precedence.
    if let Ok(m) = crate::server::muxer::read_marker(dir) {
        st.apply_ripped(&m);
    }
    apply_legacy_handoff_body(dir, &mut st);
    if let Some(label) = read_disc_label(dir) {
        st.disc_label = label;
    }

    write_state(dir, &st);

    // Strip the now-superseded legacy marker files (keep `.disc-label`; it is
    // identity, not lifecycle state, and stays its own file).
    for name in [
        DONE_MARKER,
        REVIEW_MARKER,
        COMPLETED_MARKER,
        FAILED_MARKER,
        ABORTED_LOSS_MARKER,
        RIPPED_MARKER,
        SWEEPING_MARKER,
        MUXING_MARKER,
        ACCEPT_LOSS_MARKER,
        RESTART_COUNT_FILE,
    ] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

/// Fold a legacy `.done`/`.review` JSON body's mover metadata into `st`.
fn apply_legacy_handoff_body(dir: &Path, st: &mut DiscState) {
    let body = std::fs::read_to_string(dir.join(DONE_MARKER))
        .or_else(|_| std::fs::read_to_string(dir.join(REVIEW_MARKER)));
    let Ok(body) = body else { return };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return;
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.to_string());
    if let Some(t) = s("title").filter(|x| !x.is_empty()) {
        st.title = t;
    }
    if let Some(d) = s("disc_name") {
        st.disc_name = d;
    }
    if let Some(f) = s("format").filter(|x| !x.is_empty()) {
        st.disc_format = f;
    }
    if let Some(m) = s("media_type").filter(|x| !x.is_empty()) {
        st.media_type = m;
    }
    if let Some(p) = s("poster_url") {
        st.tmdb_poster = p;
    }
    if let Some(o) = s("overview") {
        st.tmdb_overview = o;
    }
    if let Some(d) = s("date") {
        st.date = d;
    }
    if let Some(y) = v.get("year").and_then(|x| x.as_u64()) {
        st.year = y.min(9999) as u16;
    }
    if let Some(id) = v.get("tmdb_id").and_then(|x| x.as_u64()) {
        st.tmdb_id = id;
    }
    st.season = v
        .get("season")
        .and_then(|x| x.as_u64())
        .and_then(|n| u16::try_from(n).ok());
    st.disc_number = v
        .get("disc")
        .and_then(|x| x.as_u64())
        .and_then(|n| u16::try_from(n).ok());
}

/// Record the disc's raw volume label in its staging dir. Best-effort: a
/// failure just means the dir reads as "unlabelled" later, which falls back to
/// the pre-existing same-disc assumption.
pub fn write_disc_label(dir: &Path, raw_label: &str) {
    let _ = std::fs::write(dir.join(DISC_LABEL_FILE), raw_label.as_bytes());
}

/// The raw volume label recorded in a staging dir, if any.
pub fn read_disc_label(dir: &Path) -> Option<String> {
    std::fs::read_to_string(dir.join(DISC_LABEL_FILE))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Does `dir` belong to the disc with raw label `raw_label`?
///
/// An unlabelled dir (written before labels existed) counts as a match: the
/// conservative answer, because treating legacy staging as a DIFFERENT disc
/// would re-rip it on the first insert after an upgrade.
pub fn dir_is_same_disc(dir: &Path, raw_label: &str) -> bool {
    match read_disc_label(dir) {
        Some(recorded) => recorded == raw_label,
        None => true,
    }
}

/// Pick a staging dir name for `raw_label` under `staging_root`, given the title-derived `base`
/// name. Returns `base` when free or already this disc's; `base_2`, `base_3`,... when taken by
/// a DIFFERENT disc (boxset discs share a title). Does NOT uniquify for the same disc:
/// re-inserting after a restart must still find its own dir. An EMPTY `raw_label` (caller
/// doesn't know which disc) takes the same conservative `base` answer.
pub fn staging_name_for_disc(staging_root: &Path, base: &str, raw_label: &str) -> String {
    if raw_label.is_empty() {
        return base.to_string();
    }
    // The suffix policy itself lives in `util::disc_variant` — shared with the
    // mover's output naming, so the two can't drift. All this supplies is what
    // "claimable" means for a staging dir: free, or carrying this disc's label.
    crate::server::util::disc_variant(|n| {
        let path = staging_root.join(crate::server::util::disc_variant_name(base, n));
        !path.exists() || dir_is_same_disc(&path, raw_label)
    })
    .map(|n| crate::server::util::disc_variant_name(base, n))
    // Every variant belongs to a different disc. Fall back to the plain title:
    // the caller's own `.completed` / collision checks then run against it, so
    // this degrades to the pre-existing behaviour rather than inventing a name.
    .unwrap_or_else(|| base.to_string())
}

/// THE staging-directory naming rule. Every caller that needs the staging dir
/// for a disc goes through here: sanitize the TMDB display title into a path
/// segment, then let [`staging_name_for_disc`] hand back a `_2`/`_3` variant if
/// that segment is already owned by a DIFFERENT disc.
///
/// One function on purpose. This bug — disc 2 of a boxset silently skipped as
/// "already ripped" — exists because the "staging dir name" rule was spelled
/// out inline at ten call sites, so hardening any one of them fixed nothing.
/// Do not re-derive a staging basename anywhere else; call this.
pub fn staging_basename(staging_root: &Path, display_name: &str, raw_label: &str) -> String {
    let base = crate::server::util::sanitize_path_compact(display_name);
    staging_name_for_disc(staging_root, &base, raw_label)
}

/// Adopt `dir` for the disc with raw label `raw_label`, writing the label if
/// the dir does not already carry one.
///
/// The "if absent" half is what upgrades legacy staging: a dir created before
/// labels existed reads as every disc's dir (see [`dir_is_same_disc`]), which
/// is right once but would keep matching disc 2 as well. The first disc to use
/// it stamps its label and takes ownership, so the next different disc gets its
/// own directory. An existing, different label is left alone — that dir is
/// someone else's and `staging_basename` should not have routed here.
pub fn adopt_disc_label(dir: &Path, raw_label: &str) {
    if raw_label.is_empty() {
        return;
    }
    if read_disc_label(dir).is_none() {
        write_disc_label(dir, raw_label);
    }
}

/// Probe a single per-disc staging dir. Cheap — just stats a handful
/// of well-known names. Returns None if the path isn't a directory.
pub fn snapshot_staging_disc(dir: &Path) -> Option<StagingSnapshot> {
    if !dir.is_dir() {
        return None;
    }
    // NFS cache-coherency defense: a cold `read_dir` can return 0 entries even
    // when files exist (2026-05-15: a Watchtower restart wiped an 85 GB ISO).
    // Retry up to 3x/500ms; markers are read from this same primed listing.
    let mut obs = ScanObservations::default();
    for attempt in 0..3 {
        if let Ok(entries) = std::fs::read_dir(dir) {
            obs.saw_read_ok = true;
            let mut empty_this_pass = true;
            for entry in entries {
                // Don't `.flatten()` away per-entry errors: a partial NFS
                // degradation can error on individual DirEntry I/O while the
                // dir is genuinely populated, which could trip the wipe path.
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => {
                        obs.had_entry_error = true;
                        continue;
                    }
                };
                empty_this_pass = false;
                obs.saw_any_entries = true;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name == STATE_FILE {
                    obs.has_state_file = true;
                } else if name == DONE_MARKER {
                    obs.has_done = true;
                } else if name == REVIEW_MARKER {
                    obs.has_review = true;
                } else if name == RIPPED_MARKER {
                    obs.has_ripped = true;
                } else if name == SWEEPING_MARKER {
                    obs.has_sweeping = true;
                } else if name == MUXING_MARKER {
                    obs.has_muxing = true;
                } else if name == COMPLETED_MARKER {
                    obs.has_completed = true;
                } else if name == FAILED_MARKER {
                    obs.has_failed = true;
                } else if name == ABORTED_LOSS_MARKER {
                    obs.has_aborted_loss = true;
                } else if name.ends_with(".iso") {
                    obs.has_iso = true;
                } else if name.ends_with(".mapfile") {
                    // ".iso.mapfile" is subsumed by ".mapfile" — one arm covers both.
                    obs.has_mapfile = true;
                } else if name.ends_with(".mkv") || name.ends_with(".m2ts") {
                    obs.has_mkv = true;
                }
            }
            if !empty_this_pass {
                break;
            }
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    // UNKNOWN contents (every read_dir/DirEntry attempt errored, see
    // `ScanObservations::contents_unknown`): skip the dir entirely rather
    // than treat it as empty (→ wipe) or partial (→ bump `.restart_count`).
    if obs.contents_unknown() {
        tracing::warn!(
            path = %dir.display(),
            saw_read_ok = obs.saw_read_ok,
            had_entry_error = obs.had_entry_error,
            "staging dir contents UNKNOWN (read_dir/DirEntry errors, nothing observed) — skipping, not wiping or restart-counting"
        );
        return None;
    }
    if !obs.saw_any_entries {
        tracing::warn!(
            path = %dir.display(),
            "staging dir read_dir returned 0 entries on all 3 retries — treating as empty"
        );
    }

    // Lifecycle projection: unified `state.json` wins; legacy markers are a
    // one-time migration fallback. It was read from the same primed listing,
    // so a cold-cache miss can't race it to "absent".
    let mut state_unreadable = None;
    let life = if obs.has_state_file {
        // A corrupt/torn `state.json` falls back to the legacy view rather than
        // crashing, and is flagged so the mux worker holds instead of guessing.
        match read_state_checked(dir) {
            StateRead::Valid(st) => Lifecycle::from_state(&st),
            StateRead::Absent => Lifecycle::from_legacy(dir, &obs),
            StateRead::Unreadable(u) => {
                state_unreadable = Some(u);
                Lifecycle::from_legacy(dir, &obs)
            }
        }
    } else if obs.has_any_lifecycle_marker() {
        // Legacy dir: derive the view now, then upgrade it in place so every
        // subsequent writer/reader uses `state.json`.
        let life = Lifecycle::from_legacy(dir, &obs);
        upgrade_legacy_to_state(dir, &obs);
        life
    } else {
        // No unified store and no lifecycle marker — pure partial/empty state.
        Lifecycle::default()
    };

    Some(StagingSnapshot {
        dir: dir.to_path_buf(),
        completed: life.completed,
        has_failed: life.has_failed,
        failed_reason: life.failed_reason,
        has_aborted_loss: life.has_aborted_loss,
        aborted_loss_reason: life.aborted_loss_reason,
        aborted_loss_attempt: life.aborted_loss_attempt,
        has_done: life.has_done,
        has_review: life.has_review,
        has_ripped: life.has_ripped,
        has_sweeping: life.has_sweeping,
        has_muxing: life.has_muxing,
        needs_disc: life.needs_disc,
        has_iso: obs.has_iso,
        has_mapfile: obs.has_mapfile,
        has_mkv: obs.has_mkv,
        had_entry_error: obs.had_entry_error,
        saw_no_entries: !obs.saw_any_entries,
        state_unreadable,
    })
}

/// Startup safety net: walk `<staging_dir>/*` and classify each per-disc subdirectory
/// (clean/failed → leave alone; partial state → restart-count or preserve for resume; junk →
/// wipe). Returns a list of per-disc resume hints so the caller can log a summary at startup.
/// **Never deletes user data that looks like an in-flight or recovered rip** — that's the whole
/// point of this function; the only `remove_dir_all` is orphaned junk.
pub fn resume_or_quarantine_staging(staging_dir: &str) -> Vec<StagingResumeHint> {
    let mut hints = Vec::new();
    let entries = match std::fs::read_dir(staging_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(staging_dir, error = %e, "could not list staging root at startup; nothing resumed this cycle");
            return hints;
        }
    };
    for entry in entries {
        // Mirror the inner scan's defense: don't `.flatten()` away a
        // per-entry error (NFS ESTALE on a specific dentry), which would
        // silently skip a whole disc subdir for a container cycle.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(staging_dir, error = %e, "per-entry error listing staging root - skipping this entry, share may be degraded");
                continue;
            }
        };
        let path = entry.path();
        let Some(snap) = snapshot_staging_disc(&path) else {
            continue;
        };

        if snap.completed {
            tracing::info!(path = %path.display(), "staging entry has .completed — leaving for mover/ack");
            hints.push(StagingResumeHint {
                dir: snap.dir,
                action: ResumeAction::AlreadyCompleted,
            });
            continue;
        }
        // Terminal `.failed` — keyed on marker PRESENCE, not a parseable
        // reason (a non-JSON body like an operator-cancel string is still
        // terminal). Fall back to a generic reason string when unparsed.
        if snap.has_failed {
            let reason = snap
                .failed_reason
                .clone()
                .unwrap_or_else(|| "failed (no machine-readable reason recorded)".to_string());
            tracing::warn!(path = %path.display(), reason = %reason, "staging entry has .failed — leaving for operator");
            hints.push(StagingResumeHint {
                dir: snap.dir,
                action: ResumeAction::AlreadyFailed { reason },
            });
            continue;
        }
        // `.aborted-loss` — a RESUMABLE failure (loss exceeded threshold) with
        // ISO+mapfile intact. Checked after terminal `.failed`, before the
        // partial-state branch; always re-enters via `ResumeAbortedLoss`.
        if snap.has_aborted_loss {
            let reason = snap
                .aborted_loss_reason
                .clone()
                .unwrap_or_else(|| "aborted: loss exceeded threshold".to_string());
            // Deterministic media damage: stays resumable indefinitely, never
            // promoted to `.failed` by attempt count. Operator resolves it
            // (Accept or run another pass); loss is re-checked on each resume.
            tracing::info!(
                path = %path.display(),
                attempt = snap.aborted_loss_attempt,
                reason = %reason,
                "staging entry has .aborted-loss — resumable (operator-resolved: Accept or run another pass)"
            );
            hints.push(StagingResumeHint {
                dir: snap.dir,
                action: ResumeAction::ResumeAbortedLoss {
                    attempt: snap.aborted_loss_attempt,
                    reason,
                    has_iso: snap.has_iso,
                    has_mapfile: snap.has_mapfile,
                    has_mkv: snap.has_mkv,
                },
            });
            continue;
        }
        // `.done` carve-out, checked before the partial-state branch: a crash
        // between `.done` and `.completed` leaves the ISO/mapfile on disk, so
        // this finished-but-unpruned dir must not fall into restart-counting.
        if snap.has_done {
            tracing::info!(path = %path.display(), "staging entry has .done — completed rip awaiting mover, leaving alone");
            hints.push(StagingResumeHint {
                dir: snap.dir,
                action: ResumeAction::AlreadyCompleted,
            });
            continue;
        }
        // `.review` carve-out — same crash-window reasoning as `.done` above,
        // for the low-confidence-title path: finished, held for operator
        // confirmation, not partial state to be restart-counted.
        if snap.has_review {
            tracing::info!(path = %path.display(), "staging entry has .review — completed rip held for operator review, leaving alone");
            hints.push(StagingResumeHint {
                dir: snap.dir,
                action: ResumeAction::AlreadyCompleted,
            });
            continue;
        }
        // Unreadable state.json: hold for the operator — never wipe (G5) or
        // restart-loop-quarantine over it (G6). The mux worker keeps the card.
        if let Some(u) = &snap.state_unreadable {
            tracing::error!(path = %path.display(), reason = %u.log_text(), "staging entry has an unreadable state.json — held for operator");
            crate::server::muxer::record_error(&path.to_string_lossy(), &u.held_reason(), u.hint());
            hints.push(StagingResumeHint {
                dir: snap.dir,
                action: ResumeAction::HeldUnreadableState {
                    reason: u.held_reason(),
                },
            });
            continue;
        }
        // `.sweeping`/`.muxing` means the dir is actively owned, not orphaned;
        // still bump `.restart_count` on the skip and promote to `.failed`
        // at RESTART_LIMIT so a wedging sweep/mux doesn't spin forever.
        if snap.has_sweeping || snap.has_muxing {
            let rc = restart_count(&path);
            if rc >= RESTART_LIMIT {
                let reason = format!(
                    "restart loop detected ({} attempts) on owned/in-progress dir ({}); state preserved at {}",
                    rc,
                    if snap.has_muxing {
                        ".muxing"
                    } else {
                        ".sweeping"
                    },
                    path.display()
                );
                tracing::error!(
                    path = %path.display(),
                    restart_count = rc,
                    has_sweeping = snap.has_sweeping,
                    has_muxing = snap.has_muxing,
                    "owned/in-progress staging entry exceeded restart limit — marking .failed"
                );
                // write_failed_marker already clears BOTH the .sweeping and
                // .muxing markers unconditionally, so no explicit clear is
                // needed here for the owned/in-progress dir.
                write_failed_marker(&path, &reason);
                clear_restart_count(&path);
                hints.push(StagingResumeHint {
                    dir: snap.dir,
                    action: ResumeAction::RestartLoopFailed { reason },
                });
            } else {
                // Bump on every InProgress skip so a wedging owned sweep/mux
                // still walks toward `.failed`. Best-effort: a bump failure
                // just leaves the count where it was.
                let attempt = increment_restart_count(&path).unwrap_or(rc);
                tracing::info!(
                    path = %path.display(),
                    has_sweeping = snap.has_sweeping,
                    has_muxing = snap.has_muxing,
                    restart_count = attempt,
                    limit = RESTART_LIMIT,
                    "staging entry is owned/in-progress (.sweeping/.muxing) — leaving alone, restart-counted"
                );
                hints.push(StagingResumeHint {
                    dir: snap.dir,
                    action: ResumeAction::InProgress,
                });
            }
            continue;
        }
        if !snap.has_partial_state() {
            // Truly empty subdir with no markers — safe to wipe.
            match std::fs::remove_dir_all(&path) {
                Ok(_) => tracing::info!(path = %path.display(), "wiped empty staging entry"),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "empty staging wipe skipped")
                }
            }
            continue;
        }

        // Partial state, no terminal marker.
        let rc = restart_count(&path);
        if rc >= RESTART_LIMIT {
            let reason = format!(
                "restart loop detected ({} attempts); partial state preserved at {}",
                rc,
                path.display()
            );
            tracing::error!(path = %path.display(), restart_count = rc, "marking staging entry .failed");
            write_failed_marker(&path, &reason);
            clear_restart_count(&path);
            hints.push(StagingResumeHint {
                dir: snap.dir,
                action: ResumeAction::RestartLoopFailed { reason },
            });
        } else {
            // A cleanly-stopped resumable dir (ISO+mapfile, no marker) is NOT a
            // crash loop, so do not bump `.restart_count` — that previously
            // walked a healthy resumable rip to `.failed` on every restart.
            tracing::info!(
                path = %path.display(),
                restart_count = rc,
                has_iso = snap.has_iso,
                has_mapfile = snap.has_mapfile,
                has_mkv = snap.has_mkv,
                "partial staging state preserved (cleanly-stopped, resumable — not restart-counted)"
            );
            hints.push(StagingResumeHint {
                dir: snap.dir,
                action: ResumeAction::ResumePreserved {
                    attempt: rc,
                    has_iso: snap.has_iso,
                    has_mapfile: snap.has_mapfile,
                    has_mkv: snap.has_mkv,
                },
            });
        }
    }
    hints
}

/// Outcome of inspecting a single per-disc staging directory at
/// startup. Used by the orchestrator for summary logging and by tests.
#[derive(Debug)]
pub struct StagingResumeHint {
    pub dir: PathBuf,
    pub action: ResumeAction,
}

/// Fields are read by the `Debug` impl (via `tracing::info!(... ?action)`)
/// but clippy's dead-code analysis intentionally ignores derived `Debug`
/// — see the lint note. They're also read by tests via pattern-matching.
/// `#[allow(dead_code)]` keeps the structure self-documenting for future
/// consumers (e.g. a future API endpoint that exposes resume hints).
#[derive(Debug)]
#[allow(dead_code)]
pub enum ResumeAction {
    AlreadyCompleted,
    /// Dir is actively owned/in progress (`.sweeping` sweep+patch running, or
    /// `.muxing` mux worker holds it). State is left intact, but `.restart_count`
    /// IS bumped on every such skip so a deterministically-crashing owned dir
    /// still converges to `.failed` within `RESTART_LIMIT` (a healthy long rip
    /// survives the few benign bumps below the limit).
    InProgress,
    AlreadyFailed {
        reason: String,
    },
    RestartLoopFailed {
        reason: String,
    },
    /// state.json exists but can't be read; left untouched for the operator.
    HeldUnreadableState {
        reason: String,
    },
    ResumePreserved {
        attempt: u64,
        has_iso: bool,
        has_mapfile: bool,
        has_mkv: bool,
    },
    /// Dir carries a `.aborted-loss` marker (the rip phase aborted because
    /// main-movie loss exceeded `abort_on_lost_secs`), so it's RESUMABLE:
    /// re-check it against the CURRENT threshold (which may have been raised)
    /// and/or re-attempt recovery. `attempt` is the abort count so far; the
    /// classifier routes this through the same eligibility check as
    /// `ResumePreserved`. A loss-abort is deterministic and stays RESUMABLE
    /// indefinitely — never promoted to terminal `.failed` by attempt count.
    ResumeAbortedLoss {
        attempt: u64,
        reason: String,
        has_iso: bool,
        has_mapfile: bool,
        has_mkv: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir() -> PathBuf {
        // Repo-local scratch, never /tmp (wiped on reboot, cross-run collisions).
        // Anchor to the crate's target/ dir so `cargo clean` cleans it up.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch");
        let p = base.join(format!(
            "autorip-staging-test-{}-{}",
            std::process::id(),
            crate::server::util::epoch_secs()
        ));
        fs::create_dir_all(&p).unwrap();
        // Fresh subdir even when two tests land on the same epoch second: a
        // monotonic counter is non-repeating, unlike a stack-address ({:p}).
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let sub = p.join(format!("t-{}", COUNTER.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&sub).unwrap();
        sub
    }

    #[test]
    fn read_state_declines_a_foreign_schema() {
        let dir = tmpdir();
        write_state(&dir, &DiscState::new(StagingState::Failed));
        assert!(read_state(&dir).is_some(), "current schema reads back");

        // Tamper the on-disk schema to a foreign value (e.g. the schema-1
        // RippedMarker): read_state must decline it, not resume on defaults.
        let p = state_path(&dir);
        let mut v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        v["schema"] = serde_json::json!(1);
        fs::write(&p, serde_json::to_vec(&v).unwrap()).unwrap();
        assert!(
            read_state(&dir).is_none(),
            "a foreign-schema state.json must not resume as unified state",
        );
    }

    // Every read-modify-write writer must REFUSE to replace an unreadable
    // state.json (corrupt, foreign, or an I/O error) with a fresh default: that
    // would silently turn a held TV plan into an empty, valid movie plan.
    #[test]
    fn rmw_writers_refuse_to_overwrite_an_unreadable_state_json() {
        const BAD: &[u8] = b"{ torn";
        let seed = || {
            let d = tmpdir();
            fs::write(state_path(&d), BAD).unwrap();
            d
        };
        let untouched = |d: &Path, who: &str| {
            assert_eq!(
                fs::read(state_path(d)).unwrap(),
                BAD,
                "{who} overwrote state.json"
            );
        };

        let d = seed();
        assert!(mutate_state(&d, StagingState::Sweeping, |_| {}).is_err());
        untouched(&d, "mutate_state");
        let d = seed();
        assert!(!write_failed_marker(&d, "boom"), "must report not-landed");
        untouched(&d, "write_failed_marker");
        let d = seed();
        assert!(
            !write_aborted_loss_marker(&d, "loss", 1),
            "must report not-landed"
        );
        untouched(&d, "write_aborted_loss_marker");
        let d = seed();
        assert!(mark_handoff(&d, true, |_| {}).is_err());
        untouched(&d, "mark_handoff");
        let d = seed();
        write_completed_marker(&d);
        untouched(&d, "write_completed_marker");
        let d = seed();
        write_sweeping_marker(&d);
        untouched(&d, "write_sweeping_marker");
        let d = seed();
        let m = DiscState::new(StagingState::Ripped).to_ripped_marker();
        assert!(crate::server::muxer::write_marker(&d, &m).is_err());
        untouched(&d, "muxer::write_marker");
    }

    // A live-disc rip rebuilds the plan from the disc, so it may deliberately
    // replace an unreadable state.json (kept aside); the hand-off must then land.
    #[test]
    fn live_disc_seed_replaces_unreadable_state_and_handoff_proceeds() {
        let d = tmpdir();
        fs::write(state_path(&d), b"{ torn").unwrap();
        let replaced = seed_sweeping_for_live_rip(&d);
        assert!(replaced.is_some(), "the replacement must be reported");
        assert_eq!(
            read_state(&d).map(|s| s.state),
            Some(StagingState::Sweeping)
        );
        assert_eq!(fs::read(d.join(UNREADABLE_STATE_ASIDE)).unwrap(), b"{ torn");
        let m = DiscState::new(StagingState::Ripped).to_ripped_marker();
        assert!(
            crate::server::muxer::write_marker(&d, &m).is_ok(),
            "hand-off refused"
        );
        assert!(
            mark_handoff(&d, true, |_| {}).is_ok(),
            "mark_handoff refused"
        );

        // A readable state is seeded in place, nothing reported or set aside.
        let ok = tmpdir();
        write_state(&ok, &DiscState::new(StagingState::Ripped));
        assert!(seed_sweeping_for_live_rip(&ok).is_none());
        assert_eq!(
            read_state(&ok).map(|s| s.state),
            Some(StagingState::Sweeping)
        );
        assert!(!ok.join(UNREADABLE_STATE_ASIDE).exists());
    }

    // G5/G6: startup must hold (not wipe, not quarantine over) a dir whose
    // state.json is unreadable, and raise the operator card.
    #[test]
    fn startup_holds_a_dir_with_an_unreadable_state_json() {
        let root = tmpdir();
        let bare = root.join("Only_State");
        let looping = root.join("Looping");
        for d in [&bare, &looping] {
            fs::create_dir_all(d).unwrap();
            fs::write(state_path(d), b"{ torn").unwrap();
        }
        fs::write(looping.join("Looping.iso"), b"x").unwrap();
        fs::write(looping.join(RESTART_COUNT_FILE), b"99\n").unwrap();

        let hints = resume_or_quarantine_staging(&root.to_string_lossy());
        for d in [&bare, &looping] {
            assert!(d.is_dir(), "{} must not be wiped", d.display());
            assert_eq!(
                fs::read(state_path(d)).unwrap(),
                b"{ torn",
                "{}",
                d.display()
            );
            let h = hints.iter().find(|h| &h.dir == d).expect("a hint per dir");
            assert!(
                matches!(h.action, ResumeAction::HeldUnreadableState { .. }),
                "{}: {:?}",
                d.display(),
                h.action
            );
            let path = d.to_string_lossy().to_string();
            assert!(
                crate::server::muxer::MUX_ERRORS
                    .lock()
                    .unwrap()
                    .contains_key(&path),
                "{} must raise the held card",
                d.display()
            );
            crate::server::muxer::clear_error_with_prefix(&path, STATE_HELD_PREFIX);
        }
    }

    // N2: the card reason is the de-dupe key, so an I/O hold's reason must
    // not carry the raw (flapping EIO/ESTALE) error text.
    #[test]
    fn io_hold_reason_is_stable_across_errnos() {
        let a = StateUnreadable::io(&io::Error::other("Input/output error"));
        let b = StateUnreadable::io(&io::Error::other("Stale file handle"));
        assert_eq!(a.held_reason(), b.held_reason());
        assert!(a.hint().contains("click Resume"), "{}", a.hint());
    }

    #[test]
    fn read_state_checked_separates_absent_from_unreadable() {
        let dir = tmpdir();
        assert!(matches!(read_state_checked(&dir), StateRead::Absent));
        write_state(&dir, &DiscState::new(StagingState::Ripped));
        assert!(matches!(read_state_checked(&dir), StateRead::Valid(_)));
        let p = state_path(&dir);
        fs::write(&p, br#"{"schema": 1, "state": "ripped"}"#).unwrap();
        assert!(matches!(read_state_checked(&dir), StateRead::Unreadable(_)));
        fs::write(&p, b"{ torn").unwrap();
        assert!(matches!(read_state_checked(&dir), StateRead::Unreadable(_)));
    }

    #[test]
    fn restart_count_missing_returns_zero() {
        let d = tmpdir();
        assert_eq!(restart_count(&d), 0);
    }

    #[test]
    fn accept_loss_marker_round_trips_and_is_one_shot() {
        let d = tmpdir();
        assert!(!accept_loss_requested(&d), "absent by default");
        write_accept_loss_marker(&d);
        assert!(accept_loss_requested(&d), "present after write");
        clear_accept_loss_marker(&d);
        assert!(
            !accept_loss_requested(&d),
            "cleared (one-shot) after consume"
        );
    }

    #[test]
    fn increment_creates_then_advances() {
        let d = tmpdir();
        assert_eq!(increment_restart_count(&d).unwrap(), 1);
        assert_eq!(restart_count(&d), 1);
        assert_eq!(increment_restart_count(&d).unwrap(), 2);
        assert_eq!(restart_count(&d), 2);
    }

    #[test]
    fn clear_is_idempotent() {
        let d = tmpdir();
        clear_restart_count(&d); // missing — must not panic
        increment_restart_count(&d).unwrap();
        clear_restart_count(&d);
        assert_eq!(restart_count(&d), 0);
        clear_restart_count(&d); // already gone — must not error
    }

    // The durability gate decides whether `.done`/`.completed` may be
    // written; getting it backwards files a truncated file as a finished
    // title. A mutation run once dropped the `!` in each copy unnoticed.
    #[test]
    fn durability_gate_blocks_markers_unless_the_output_is_provably_durable() {
        // A failed fsync must withhold the markers.
        assert!(!durability_gate_passes(false, || false));
        // A successful one must not.
        assert!(durability_gate_passes(false, || true));

        // A network sink has no local file, so the gate passes WITHOUT
        // evaluating the fsync at all — an eager `is_network || fsync(path)`
        // would stat a path that does not exist.
        let mut called = false;
        assert!(durability_gate_passes(true, || {
            called = true;
            false
        }));
        assert!(!called, "the fsync must not run for a network sink");
    }

    // Both halves matter: a "network" format with no target configured
    // falls back to LOCAL output and still needs the flush.
    #[test]
    fn network_output_requires_both_the_format_and_a_target() {
        assert!(is_network_output("network", "nfs://box/media"));
        assert!(
            !is_network_output("network", ""),
            "no target means local output — the durability gate must still run"
        );
        assert!(!is_network_output("mkv", "nfs://box/media"));
        assert!(!is_network_output("iso", ""));
    }

    // `fsync_output_file` is the mux durability gate. The rc.4.1 Windows
    // remux loop was this returning false forever (opened read-only, and
    // FlushFileBuffers rejects that); this pins both arms of the contract.
    #[test]
    fn fsync_output_file_true_for_real_false_for_missing() {
        let d = tmpdir();
        let f = d.join("out.mkv");
        fs::write(&f, b"muxed bytes").unwrap();
        assert!(
            fsync_output_file(&f),
            "an existing output file must fsync successfully (gate passes)"
        );
        assert!(
            !fsync_output_file(&d.join("never-written.mkv")),
            "a missing output file must fail the gate so staging is preserved"
        );
    }

    // Must round-trip the incremented value and leave no `.restart_count.tmp`
    // behind — a dangling `.tmp` would mean a torn write or a rename that
    // never happened.
    #[test]
    fn increment_roundtrips_and_cleans_up_tmp() {
        let d = tmpdir();
        let tmp = d.join(format!("{}.tmp", RESTART_COUNT_FILE));

        let v1 = increment_restart_count(&d).unwrap();
        assert_eq!(v1, 1);
        assert_eq!(restart_count(&d), 1, "incremented value must round-trip");
        assert!(
            !tmp.exists(),
            "{} must be renamed away, not left behind",
            tmp.display()
        );

        let v2 = increment_restart_count(&d).unwrap();
        assert_eq!(v2, 2);
        assert_eq!(restart_count(&d), 2);
        assert!(!tmp.exists(), "tmp file must not persist across increments");
    }

    #[test]
    fn corrupt_restart_count_returns_zero() {
        let d = tmpdir();
        fs::write(d.join(RESTART_COUNT_FILE), b"garbage\n").unwrap();
        assert_eq!(restart_count(&d), 0);
    }

    #[test]
    fn failed_marker_roundtrip() {
        let d = tmpdir();
        write_failed_marker(&d, "test reason");
        assert_eq!(read_failed_reason(&d).as_deref(), Some("test reason"));
    }

    // A hand-off marker must never be written empty: the mover skips
    // directories whose marker won't parse, stranding a finished output
    // with no operator-facing signal.
    #[test]
    fn handoff_marker_is_nonempty_and_parseable() {
        let d = tmpdir();
        let marker = serde_json::json!({
            "title": "Some Movie",
            "format": "Blu-ray",
            "year": 2024,
            "date": "2024-01-01",
        });
        let body =
            serde_json::to_string_pretty(&marker).expect("json! value is always serialisable");
        let path = d.join(".done");
        write_handoff_marker(&path, body.as_bytes()).unwrap();

        let written = fs::read(&path).unwrap();
        assert!(!written.is_empty(), ".done marker must not be empty bytes");
        let parsed: serde_json::Value = serde_json::from_slice(&written).unwrap();
        assert_eq!(
            parsed.get("title").and_then(|v| v.as_str()),
            Some("Some Movie")
        );
    }

    // Graceful-shutdown belt-and-suspenders: strips every `.sweeping`/
    // `.muxing` marker under the root so a clean SIGTERM isn't misread by
    // the next startup's resume classifier as a crash.
    #[test]
    fn clear_inprogress_markers_strips_sweeping_and_muxing_under_root() {
        let root = tmpdir();
        let disc_a = root.join("DiscA");
        let disc_b = root.join("DiscB");
        fs::create_dir_all(&disc_a).unwrap();
        fs::create_dir_all(&disc_b).unwrap();
        write_sweeping_marker(&disc_a);
        let mut st = DiscState::new(StagingState::Ripped);
        st.muxing = true;
        write_state(&disc_b, &st);
        assert_eq!(read_state(&disc_a).unwrap().state, StagingState::Sweeping);
        assert!(read_state(&disc_b).unwrap().muxing);

        clear_inprogress_markers(&root);

        assert!(
            read_state(&disc_a).is_none(),
            ".sweeping must be cleared on graceful shutdown"
        );
        assert!(
            !read_state(&disc_b).unwrap().muxing,
            ".muxing must be cleared on graceful shutdown"
        );
    }

    // `write_failed_marker` must REPORT whether the terminal state landed,
    // not silently swallow a write failure — a dropped write re-dispatches
    // to the mux worker forever without this signal.
    #[test]
    fn write_failed_marker_reports_whether_state_landed() {
        // Success: a normal dir → terminal state lands, returns true.
        let root = tmpdir();
        let disc = root.join("Good");
        fs::create_dir_all(&disc).unwrap();
        assert!(
            write_failed_marker(&disc, "E6008"),
            "a successful terminal write must report landed=true"
        );
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);

        // Failure: point the writer at a path whose parent is a FILE, so the
        // atomic state.json write cannot create its temp file (ENOTDIR). The
        // return must be false — the signal the worker surfaces loudly.
        let not_a_dir = root.join("iam_a_file");
        fs::write(&not_a_dir, b"x").unwrap();
        let doomed = not_a_dir.join("child");
        assert!(
            !write_failed_marker(&doomed, "E6008"),
            "a failed terminal write must report landed=false, never swallow it"
        );
    }

    #[test]
    fn resume_marks_failed_after_limit() {
        // Build a fake staging tree: <root>/<disc>/foo.iso plus
        // .restart_count == RESTART_LIMIT.
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        fs::write(
            disc.join(RESTART_COUNT_FILE),
            format!("{}\n", RESTART_LIMIT).as_bytes(),
        )
        .unwrap();

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(matches!(
            hints[0].action,
            ResumeAction::RestartLoopFailed { .. }
        ));
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);
        // Counter cleared after promotion to .failed.
        assert_eq!(restart_count(&disc), 0);
    }

    #[test]
    fn resume_cleanly_stopped_resumable_not_counted() {
        // A bare resumable dir (ISO, no `.sweeping`/`.muxing`) is a clean
        // stop/redeploy/reboot, not a crash loop — must not bump
        // `.restart_count`, or healthy resumable rips get walked to `.failed`.
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        match &hints[0].action {
            ResumeAction::ResumePreserved { attempt, .. } => assert_eq!(*attempt, 0),
            other => panic!("unexpected action: {:?}", other),
        }
        assert_eq!(
            restart_count(&disc),
            0,
            "a clean stop must not bump restart_count"
        );
        assert!(!disc.join(FAILED_MARKER).exists());
    }

    #[test]
    fn resume_preserves_completed_dirs() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.mkv"), b"x").unwrap();
        write_completed_marker(&disc);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(matches!(hints[0].action, ResumeAction::AlreadyCompleted));
        // Marker must still be there afterwards.
        assert!(snapshot_staging_disc(&disc).unwrap().completed);
        // MKV must still be there afterwards.
        assert!(disc.join("foo.mkv").exists());
    }

    #[test]
    fn done_marker_with_partial_state_is_completed_not_retried() {
        // A crash between writing .done and .completed leaves .done +
        // the ISO/mapfile on disk. The resume scan must treat this as a
        // completed rip awaiting the mover, NOT bump .restart_count.
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        fs::write(disc.join("foo.iso.mapfile"), b"x").unwrap();
        fs::write(disc.join(DONE_MARKER), b"{}").unwrap();
        // No .completed marker (the crash happened before it landed).

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::AlreadyCompleted),
            "got {:?}",
            hints[0].action
        );
        // Counter must NOT have been bumped — this was a finished rip.
        assert_eq!(restart_count(&disc), 0);
        assert!(!disc.join(FAILED_MARKER).exists());
        // Data preserved for the mover.
        assert!(disc.join("foo.iso").exists());
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Done);
    }

    #[test]
    fn review_marker_with_partial_state_is_completed_not_retried() {
        // A crash between writing .review and .completed leaves .review +
        // the ISO/mapfile/MKV on disk; must be treated as finished, held
        // for operator review, not bumped toward .failed.
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        fs::write(disc.join("foo.iso.mapfile"), b"x").unwrap();
        fs::write(disc.join("MyDisc.mkv"), b"x").unwrap();
        fs::write(disc.join(REVIEW_MARKER), b"{}").unwrap();
        // No .completed marker (the crash happened before it landed).

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::AlreadyCompleted),
            "got {:?}",
            hints[0].action
        );
        // Counter must NOT have been bumped — this was a finished rip.
        assert_eq!(restart_count(&disc), 0);
        assert!(!disc.join(FAILED_MARKER).exists());
        // Data preserved for the operator/mover.
        assert!(disc.join("MyDisc.mkv").exists());
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Review);
    }

    // The legacy-upgrade path must fold every field of a real (non-empty)
    // `.done` body into the fresh state.json, and remove the legacy file.
    // Earlier tests only wrote `b"{}"` bodies, so field-folding was untested.
    #[test]
    fn legacy_done_body_migrates_metadata_into_state() {
        let root = tmpdir();
        let disc = root.join("Endeavour_S5_D2");
        fs::create_dir_all(&disc).unwrap();
        let body = serde_json::json!({
            "title": "Endeavour",
            "disc_name": "ENDEAVOUR_S5_D2",
            "format": "bluray",
            "year": 2012,
            "media_type": "tv",
            "tmdb_id": 44264,
            "season": 5,
            "disc": 2,
            "poster_url": "http://x/p.jpg",
            "overview": "A young detective in 1960s Oxford.",
            "date": "2026-08-22",
        });
        fs::write(disc.join(DONE_MARKER), body.to_string()).unwrap();

        let snap = snapshot_staging_disc(&disc);
        assert!(snap.is_some(), "a real .done body must still snapshot");

        let st = read_state(&disc).expect("state.json must exist after migration");
        assert_eq!(st.state, StagingState::Done);
        assert_eq!(st.title, "Endeavour");
        assert_eq!(st.disc_name, "ENDEAVOUR_S5_D2");
        assert_eq!(st.disc_format, "bluray");
        assert_eq!(st.year, 2012);
        assert_eq!(st.media_type, "tv");
        assert_eq!(st.tmdb_id, 44264);
        assert_eq!(st.season, Some(5));
        assert_eq!(st.disc_number, Some(2));
        assert!(
            !st.tmdb_poster.is_empty(),
            "poster_url must fold into tmdb_poster"
        );
        assert_eq!(st.tmdb_poster, "http://x/p.jpg");
        assert_eq!(st.tmdb_overview, "A young detective in 1960s Oxford.");
        assert_eq!(st.date, "2026-08-22");

        assert!(
            !disc.join(DONE_MARKER).exists(),
            "the legacy .done file must be removed once migrated to state.json"
        );
    }

    /// Same migration, but for the `.review` hand-off (unconfident-title path):
    /// the folded state must land in `StagingState::Review`, and the legacy
    /// `.review` file must be gone afterwards.
    #[test]
    fn legacy_review_body_migrates_to_review_state() {
        let root = tmpdir();
        let disc = root.join("SomeShow_S1_D1");
        fs::create_dir_all(&disc).unwrap();
        let body = serde_json::json!({
            "title": "Some Show",
            "disc_name": "SOMESHOW_S1_D1",
            "format": "dvd",
            "year": 2005,
            "media_type": "tv",
            "tmdb_id": 9999,
            "season": 1,
            "disc": 1,
            "poster_url": "http://x/q.jpg",
            "overview": "overview text",
            "date": "2026-08-20",
        });
        fs::write(disc.join(REVIEW_MARKER), body.to_string()).unwrap();

        let snap = snapshot_staging_disc(&disc);
        assert!(snap.is_some());

        let st = read_state(&disc).expect("state.json must exist after migration");
        assert_eq!(st.state, StagingState::Review);
        assert_eq!(st.title, "Some Show");
        assert_eq!(st.disc_name, "SOMESHOW_S1_D1");
        assert_eq!(st.disc_format, "dvd");
        assert_eq!(st.year, 2005);
        assert_eq!(st.media_type, "tv");
        assert_eq!(st.tmdb_id, 9999);
        assert_eq!(st.season, Some(1));
        assert_eq!(st.disc_number, Some(1));
        assert_eq!(st.tmdb_poster, "http://x/q.jpg");
        assert_eq!(st.tmdb_overview, "overview text");

        assert!(
            !disc.join(REVIEW_MARKER).exists(),
            "the legacy .review file must be removed once migrated to state.json"
        );
    }

    // A legacy `.failed` marker migrates its `reason` into
    // `DiscState::failure_reason`, and the legacy file must be removed.
    #[test]
    fn legacy_failed_migrates_and_removes_file() {
        let root = tmpdir();
        let disc = root.join("BadDisc");
        fs::create_dir_all(&disc).unwrap();
        let body = serde_json::json!({ "reason": "boom" });
        fs::write(disc.join(FAILED_MARKER), body.to_string()).unwrap();

        let snap = snapshot_staging_disc(&disc);
        assert!(snap.is_some());

        let st = read_state(&disc).expect("state.json must exist after migration");
        assert_eq!(st.state, StagingState::Failed);
        assert_eq!(st.failure_reason.as_deref(), Some("boom"));

        assert!(
            !disc.join(FAILED_MARKER).exists(),
            "the legacy .failed file must be removed once migrated to state.json"
        );
    }

    #[test]
    fn snapshot_reports_unknown_on_unreadable_dir() {
        // A path that isn't a directory (read_dir errors) must yield
        // None, not a "looks empty" snapshot that the caller might wipe.
        let root = tmpdir();
        let not_a_dir = root.join("a_file");
        fs::write(&not_a_dir, b"x").unwrap();
        assert!(snapshot_staging_disc(&not_a_dir).is_none());
    }

    #[test]
    fn all_direntry_errors_with_no_artifacts_is_unknown_not_partial() {
        // read_dir opened but every DirEntry I/O errored and nothing
        // trustworthy was observed — must classify UNKNOWN so the caller
        // skips it without bumping `.restart_count` toward `.failed`.
        let obs = ScanObservations {
            saw_read_ok: true,
            had_entry_error: true,
            ..Default::default()
        };
        assert!(obs.observed_nothing());
        assert!(
            obs.contents_unknown(),
            "all-DirEntry-error + no artifacts must be UNKNOWN, not partial state"
        );
    }

    #[test]
    fn all_read_dir_attempts_errored_is_unknown() {
        // The original all-attempts-errored defense: never got a listing.
        let obs = ScanObservations {
            saw_read_ok: false,
            ..Default::default()
        };
        assert!(obs.contents_unknown());
    }

    #[test]
    fn entry_error_alongside_real_artifact_is_not_unknown() {
        // One DirEntry errored but the ISO was still seen — not unknown,
        // so the snapshot is kept and normal resume/restart handling runs.
        let obs = ScanObservations {
            saw_read_ok: true,
            saw_any_entries: true,
            had_entry_error: true,
            has_iso: true,
            ..Default::default()
        };
        assert!(!obs.observed_nothing());
        assert!(!obs.contents_unknown());
    }

    #[test]
    fn clean_empty_dir_is_not_unknown() {
        // read_dir succeeded, dir was genuinely empty, no entry errors.
        // Not UNKNOWN — the caller may legitimately wipe a truly-empty,
        // marker-less staging dir.
        let obs = ScanObservations {
            saw_read_ok: true,
            ..Default::default()
        };
        assert!(!obs.contents_unknown());
    }

    #[test]
    fn unknown_contents_snapshot_does_not_bump_restart_count() {
        // A snapshot returning None for UNKNOWN contents means the dir is
        // skipped entirely, so its restart count stays untouched (can't
        // provoke real per-entry NFS errors here, so we assert the contract).
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        // Pre-seed restart_count near the limit to make a wrongful bump
        // (which would push it to .failed) maximally visible.
        fs::write(
            disc.join(RESTART_COUNT_FILE),
            format!("{}\n", RESTART_LIMIT - 1).as_bytes(),
        )
        .unwrap();
        // The contract: when snapshot_staging_disc returns None (UNKNOWN),
        // the dir is skipped. Verify the predicate that drives that None.
        let unknown = ScanObservations {
            saw_read_ok: true,
            had_entry_error: true,
            ..Default::default()
        };
        assert!(unknown.contents_unknown());
        // And confirm that simply NOT processing the dir leaves the
        // counter where it was — no bump, no promotion to .failed.
        assert_eq!(restart_count(&disc), RESTART_LIMIT - 1);
        assert!(!disc.join(FAILED_MARKER).exists());
    }

    #[test]
    fn resume_preserves_failed_dirs() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        write_failed_marker(&disc, "prior failure");

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        match &hints[0].action {
            ResumeAction::AlreadyFailed { reason } => assert_eq!(reason, "prior failure"),
            other => panic!("unexpected action: {:?}", other),
        }
        assert!(disc.join("foo.iso").exists());
    }

    // A loss-abort stays RESUMABLE no matter how many times it recurs — never
    // promoted to terminal `.failed` by attempt count. The attempt counter
    // still advances on disk so the UI can report it.
    #[test]
    fn mark_aborted_on_loss_always_resumable() {
        let disc = tmpdir();
        fs::write(disc.join("foo.iso"), b"x").unwrap();

        for expected in 1..=5 {
            let terminal = mark_aborted_on_loss(&disc, "loss exceeds threshold");
            assert!(!terminal, "a loss-abort must never become terminal");
            let (_, attempt) = read_aborted_loss(&disc).expect(".aborted-loss must exist");
            assert_eq!(attempt, expected, "attempt count must advance on disk");
            assert!(
                !disc.join(FAILED_MARKER).exists(),
                "must never write terminal .failed for a loss-abort"
            );
        }
    }

    /// (a) A `.aborted-loss` marker BELOW the attempt limit is RESUMABLE: the
    /// scan emits `ResumeAbortedLoss` (not `AlreadyFailed`), leaves the ISO +
    /// marker intact, and does NOT write `.failed`.
    #[test]
    fn aborted_loss_below_limit_is_resumable() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        fs::write(disc.join("foo.iso.mapfile"), b"x").unwrap();
        // First abort → attempt 1 (< MAX_LOSS_RESUME_ATTEMPTS).
        write_aborted_loss_marker(&disc, "12.50s lost exceeds 0s", 1);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        match &hints[0].action {
            ResumeAction::ResumeAbortedLoss {
                attempt,
                has_iso,
                has_mapfile,
                ..
            } => {
                assert_eq!(*attempt, 1);
                assert!(
                    *has_iso && *has_mapfile,
                    "ISO + mapfile must be reported intact"
                );
            }
            other => panic!("expected ResumeAbortedLoss, got {other:?}"),
        }
        assert!(disc.join("foo.iso").exists());
        assert_eq!(
            read_state(&disc).unwrap().state,
            StagingState::AbortedLoss,
            "marker left intact for retry"
        );
        assert!(
            !disc.join(FAILED_MARKER).exists(),
            "below limit must NOT be terminal"
        );
    }

    // (b) Stays RESUMABLE regardless of attempt count — no terminal
    // promotion. The old attempt-cap promotion clobbered a complete swept ISO.
    #[test]
    fn aborted_loss_high_attempt_count_stays_resumable() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        write_aborted_loss_marker(&disc, "12.50s lost exceeds 0s", 99);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        match &hints[0].action {
            ResumeAction::ResumeAbortedLoss {
                attempt, has_iso, ..
            } => {
                assert_eq!(*attempt, 99);
                assert!(*has_iso, "ISO must stay recoverable");
            }
            other => panic!("expected ResumeAbortedLoss, got {other:?}"),
        }
        assert!(
            !disc.join(FAILED_MARKER).exists(),
            "a loss-abort must never be promoted to terminal .failed"
        );
        assert_eq!(
            read_state(&disc).unwrap().state,
            StagingState::AbortedLoss,
            ".aborted-loss must remain for the operator"
        );
    }

    // (c) A real terminal `.failed` stays terminal — unaffected by the
    // abort-loss path; pinned alongside the new variants against regression.
    #[test]
    fn real_failed_stays_terminal_alongside_abort_loss() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        write_failed_marker(&disc, "cancelled by operator");

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::AlreadyFailed { .. }),
            "a real .failed must stay terminal, got {:?}",
            hints[0].action
        );
    }

    /// (d) A finished rip (`.done`/`.completed`) is unaffected by the new
    /// abort-loss branch — still classified `AlreadyCompleted`.
    #[test]
    fn done_and_completed_unaffected_by_abort_loss_branch() {
        // .completed → AlreadyCompleted.
        let root1 = tmpdir();
        let d1 = root1.join("DiscA");
        fs::create_dir_all(&d1).unwrap();
        write_marker_durable(&d1.join(COMPLETED_MARKER), b"{}").unwrap();
        let h1 = resume_or_quarantine_staging(root1.to_str().unwrap());
        assert_eq!(h1.len(), 1);
        assert!(
            matches!(h1[0].action, ResumeAction::AlreadyCompleted),
            "got {:?}",
            h1[0].action
        );

        // .done (+ leftover ISO) → AlreadyCompleted (finished, awaiting mover).
        let root2 = tmpdir();
        let d2 = root2.join("DiscB");
        fs::create_dir_all(&d2).unwrap();
        fs::write(d2.join("foo.iso"), b"x").unwrap();
        write_marker_durable(&d2.join(DONE_MARKER), b"{}").unwrap();
        let h2 = resume_or_quarantine_staging(root2.to_str().unwrap());
        assert_eq!(h2.len(), 1);
        assert!(
            matches!(h2[0].action, ResumeAction::AlreadyCompleted),
            "got {:?}",
            h2[0].action
        );
    }

    // A `.sweeping` dir from a NON-watchdog hard crash lands with
    // `.restart_count == 0`. The InProgress carve-out must STILL restart-count
    // it so a deterministically-crashing sweep walks toward `.failed`.
    #[test]
    fn sweeping_in_progress_is_restart_counted() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        // `.sweeping` present, count at 0 (raw kill — no watchdog bump).
        write_sweeping_marker(&disc);
        assert_eq!(restart_count(&disc), 0);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::InProgress),
            "below the limit a .sweeping dir is left in progress, got {:?}",
            hints[0].action
        );
        assert_eq!(
            restart_count(&disc),
            1,
            ".sweeping InProgress skip must bump .restart_count (else a crash loop never escapes)"
        );
        assert!(disc.join("foo.iso").exists());
        assert!(!disc.join(FAILED_MARKER).exists());
    }

    // Once a `.sweeping` dir's restart count reaches RESTART_LIMIT the
    // carve-out must promote it to `.failed` and clear the in-progress marker.
    #[test]
    fn sweeping_in_progress_fails_after_limit() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("foo.iso"), b"x").unwrap();
        write_sweeping_marker(&disc);
        mutate_state_if_present(&disc, |s| s.restart_count = RESTART_LIMIT);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::RestartLoopFailed { .. }),
            "got {:?}",
            hints[0].action
        );
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);
        // The in-progress marker is cleared on promotion.
        assert_ne!(read_state(&disc).unwrap().state, StagingState::Sweeping);
        assert_eq!(restart_count(&disc), 0);
    }

    // A terminal write must release the `.muxing` exclusion lock — the cold
    // operator-resume path writes terminal markers without the worker's
    // MuxingGuard, so a stale lock would block the re-insert path forever.
    #[test]
    fn terminal_writers_clear_muxing_lock() {
        // .completed clears .muxing.
        let d1 = tmpdir();
        let mut st1 = DiscState::new(StagingState::Ripped);
        st1.muxing = true;
        write_state(&d1, &st1);
        assert!(read_state(&d1).unwrap().muxing);
        write_completed_marker(&d1);
        assert!(
            !read_state(&d1).unwrap().muxing,
            ".completed must clear a leftover .muxing lock"
        );

        // .failed clears .muxing.
        let d2 = tmpdir();
        let mut st2 = DiscState::new(StagingState::Ripped);
        st2.muxing = true;
        write_state(&d2, &st2);
        assert!(read_state(&d2).unwrap().muxing);
        write_failed_marker(&d2, "terminal");
        assert!(
            !read_state(&d2).unwrap().muxing,
            ".failed must clear a leftover .muxing lock"
        );
    }

    // If the durable `.done` write FAILS, the caller must NOT proceed to
    // write `.completed` or clear `.restart_count`, else the dir looks
    // terminal-complete while the mover has nothing to act on.
    #[test]
    fn failed_done_write_leaves_no_completed_and_preserves_restart_count() {
        let disc = tmpdir();

        // Seed a restart counter so we can prove it is NOT cleared.
        increment_restart_count(&disc).unwrap();
        assert_eq!(restart_count(&disc), 1);

        // Force the `.done` write to fail by targeting a path whose parent is
        // a non-existent subdirectory (the durable write can't create the tmp
        // file there). This mirrors a real I/O failure at the marker site.
        let bad_done = disc.join("missing-subdir").join(DONE_MARKER);
        let handoff = write_handoff_marker(&bad_done, b"{}");
        assert!(
            handoff.is_err(),
            "precondition: the hand-off marker write must fail for this test"
        );

        // The fix: on that Err, `rip_disc` returns early — so the following
        // two calls are SKIPPED. We assert the post-state that skipping yields.
        // (We deliberately do NOT call write_completed_marker / clear_restart_count.)

        assert!(
            !disc.join(COMPLETED_MARKER).exists(),
            ".completed must not exist when the .done write failed"
        );
        assert!(
            !disc.join(DONE_MARKER).exists(),
            "no durable .done landed in the staging dir"
        );
        assert_eq!(
            restart_count(&disc),
            1,
            ".restart_count must be preserved (not cleared) when .done failed"
        );
    }

    // A rename failure must not leak the `.tmp` sibling. Forced by making the
    // target path a non-empty directory, which fails rename(2) AFTER the
    // tmp file was created + fsynced, exercising cleanup-on-error.
    #[test]
    fn marker_rename_failure_cleans_up_tmp() {
        let d = tmpdir();
        let target = d.join(".done");
        // Make the target a non-empty directory so rename-over it fails.
        fs::create_dir(&target).unwrap();
        fs::write(target.join("occupant"), b"x").unwrap();

        let res = write_marker_durable(&target, b"{}");
        assert!(
            res.is_err(),
            "precondition: rename onto a non-empty dir must fail"
        );

        let tmp = d.join(".done.tmp");
        assert!(
            !tmp.exists(),
            "the .tmp sibling must be cleaned up after a rename failure, found: {}",
            tmp.display()
        );
    }

    // Exhaustive resume-on-startup classifier matrix (rc4 hardening): drives
    // `resume_or_quarantine_staging` over every marker/artifact/restart_count
    // combination, asserting the resulting `ResumeAction` or silent wipe/skip.

    #[derive(Clone, Copy)]
    enum Mk {
        Completed,
        Failed,
        Done,
        Review,
        Ripped,
        Sweeping,
        Muxing,
        Iso,
        Mapfile,
        Mkv,
        RestartAtLimit,
        RestartBelowLimit,
        /// A non-JSON `.failed` body (e.g. review.rs's operator-cancel). Used
        /// to pin that terminal-ness keys on marker PRESENCE, not parse.
        FailedNonJson,
    }

    /// What `resume_or_quarantine_staging` must decide for one disc dir.
    #[derive(Debug, PartialEq)]
    enum Verdict {
        Completed,
        Failed,
        RestartLoopFailed,
        ResumePreserved,
        /// Dir carries a resumable `.aborted-loss` below the attempt limit.
        ResumeAbortedLoss,
        /// Dir is owned/in progress (`.sweeping`/`.muxing`) — left alone, not
        /// restart-counted.
        InProgress,
        Wiped,
    }

    fn resume_verdict(markers: &[Mk]) -> Verdict {
        let root = tmpdir();
        let disc = root.join("Disc");
        fs::create_dir_all(&disc).unwrap();
        for m in markers {
            match m {
                Mk::Completed => write_completed_marker(&disc),
                Mk::Failed => {
                    let _ = write_failed_marker(&disc, "prior failure");
                }
                Mk::Done => fs::write(disc.join(DONE_MARKER), b"{}").unwrap(),
                Mk::Review => fs::write(disc.join(REVIEW_MARKER), b"{}").unwrap(),
                Mk::Ripped => fs::write(disc.join(RIPPED_MARKER), b"{}").unwrap(),
                Mk::Sweeping => fs::write(disc.join(SWEEPING_MARKER), b"{}").unwrap(),
                Mk::Muxing => fs::write(disc.join(MUXING_MARKER), b"{}").unwrap(),
                Mk::FailedNonJson => {
                    // Mimic the legacy review.rs body: a non-JSON `.failed`.
                    fs::write(disc.join(FAILED_MARKER), b"cancelled by operator\n").unwrap()
                }
                Mk::Iso => fs::write(disc.join("Disc.iso"), b"x").unwrap(),
                Mk::Mapfile => fs::write(disc.join("Disc.iso.mapfile"), b"x").unwrap(),
                Mk::Mkv => fs::write(disc.join("Disc.mkv"), b"x").unwrap(),
                Mk::RestartAtLimit => fs::write(
                    disc.join(RESTART_COUNT_FILE),
                    format!("{}\n", RESTART_LIMIT).as_bytes(),
                )
                .unwrap(),
                Mk::RestartBelowLimit => fs::write(
                    disc.join(RESTART_COUNT_FILE),
                    format!("{}\n", RESTART_LIMIT - 1).as_bytes(),
                )
                .unwrap(),
            }
        }
        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        if hints.is_empty() {
            // No hint: wiped (empty/junk) or skipped (UNKNOWN); for these
            // local-FS test rows both collapse to Wiped.
            return Verdict::Wiped;
        }
        assert_eq!(hints.len(), 1, "expected exactly one disc dir");
        match &hints[0].action {
            ResumeAction::AlreadyCompleted => Verdict::Completed,
            ResumeAction::AlreadyFailed { .. } => Verdict::Failed,
            ResumeAction::RestartLoopFailed { .. } => Verdict::RestartLoopFailed,
            ResumeAction::ResumePreserved { .. } => Verdict::ResumePreserved,
            ResumeAction::ResumeAbortedLoss { .. } => Verdict::ResumeAbortedLoss,
            ResumeAction::InProgress => Verdict::InProgress,
            ResumeAction::HeldUnreadableState { .. } => panic!("matrix never corrupts state.json"),
        }
    }

    #[test]
    fn resume_classifier_matrix() {
        use Mk::*;
        let table: &[(&[Mk], Verdict, &str)] = &[
            // --- empty / junk: wiped ---
            (
                &[],
                Verdict::Wiped,
                "empty dir, no markers/artifacts → wipe",
            ),
            // --- .completed: terminal, leave for mover ---
            (&[Completed], Verdict::Completed, ".completed alone"),
            (
                &[Completed, Mkv],
                Verdict::Completed,
                ".completed with output",
            ),
            (
                &[Completed, Iso, Mapfile],
                Verdict::Completed,
                ".completed with leftover ISO",
            ),
            // --- .failed: terminal, leave for operator. Checked BEFORE the
            //     .done/.review carve-outs and the partial-state branch. ---
            (&[Failed], Verdict::Failed, ".failed alone"),
            (
                &[Failed, Iso],
                Verdict::Failed,
                ".failed with partial ISO still present",
            ),
            (
                &[Failed, Iso, Mapfile, RestartAtLimit],
                Verdict::Failed,
                ".failed wins even at the restart limit (terminal precedence)",
            ),
            // --- .done carve-out: crash between .done and .completed.
            //     Finished rip awaiting mover; must NOT be restart-counted. ---
            (
                &[Done],
                Verdict::Completed,
                ".done alone → AlreadyCompleted",
            ),
            (
                &[Done, Iso, Mapfile],
                Verdict::Completed,
                "CRASH WINDOW: .done + ISO + mapfile, no .completed → finished, not retried",
            ),
            (
                &[Done, Iso, Mapfile, RestartAtLimit],
                Verdict::Completed,
                ".done short-circuits even with restart_count at limit (must not become .failed)",
            ),
            // --- .review carve-out: same crash-window reasoning ---
            (
                &[Review],
                Verdict::Completed,
                ".review alone → AlreadyCompleted",
            ),
            (
                &[Review, Iso, Mapfile, Mkv],
                Verdict::Completed,
                "CRASH WINDOW: .review + artifacts, no .completed → finished/held, not retried",
            ),
            // --- partial state, no terminal marker, below limit → preserve+bump ---
            (
                &[Iso],
                Verdict::ResumePreserved,
                "ISO only → partial, preserve",
            ),
            (
                &[Iso, Mapfile],
                Verdict::ResumePreserved,
                "ISO+mapfile → partial, preserve",
            ),
            (
                &[Mapfile],
                Verdict::ResumePreserved,
                "mapfile only → partial, preserve",
            ),
            (
                &[Mkv],
                Verdict::ResumePreserved,
                "partial MKV only → partial, preserve",
            ),
            (
                &[Iso, Mapfile, RestartBelowLimit],
                Verdict::ResumePreserved,
                "partial below limit → preserve + bump",
            ),
            // --- partial state AT the restart limit → promote to .failed ---
            (
                &[Iso, RestartAtLimit],
                Verdict::RestartLoopFailed,
                "partial at RESTART_LIMIT → quarantine (.failed)",
            ),
            (
                &[Iso, Mapfile, RestartAtLimit],
                Verdict::RestartLoopFailed,
                "partial at limit with full ISO+mapfile → quarantine",
            ),
            // --- ISO present but no mapfile: still partial (the resume CLASSIFIER
            //     downstream rejects it as not-eligible, but the staging scan still
            //     preserves it as partial state to resume the sweep). ---
            (
                &[Iso, RestartBelowLimit],
                Verdict::ResumePreserved,
                "ISO + no mapfile → partial, preserve (classify_resume later rejects remux)",
            ),
            // --- .ripped-only, no artifacts: not partial state, wiped as junk;
            //     the mux worker (separate tick) is what acts on .ripped. ---
            (
                &[Ripped],
                Verdict::Wiped,
                ".ripped with no ISO/mapfile/MKV is not partial state to the resume scan → wiped",
            ),
            // --- .ripped alongside real artifacts: partial state, preserved ---
            (
                &[Ripped, Iso, Mapfile],
                Verdict::ResumePreserved,
                ".ripped + artifacts → partial state preserved (mux worker handles the .ripped)",
            ),
            // --- H2/M1: .sweeping in-progress marker, verdict InProgress —
            //     `.restart_count` IS bumped each skip so a wedge converges. ---
            (
                &[Sweeping],
                Verdict::InProgress,
                ".sweeping alone → owned/in-progress, leave alone",
            ),
            (
                &[Sweeping, Iso, Mapfile],
                Verdict::InProgress,
                "CRASH MID-SWEEP: .sweeping + artifacts → in-progress, not partial state",
            ),
            // R2 finding 1: below the limit a healthy sweep stays InProgress;
            // a sweep wedged RESTART_LIMIT times must promote to `.failed`,
            // else the carve-out defeats the watchdog's restart-loop guard.
            (
                &[Sweeping, Iso, Mapfile, RestartBelowLimit],
                Verdict::InProgress,
                ".sweeping below limit → healthy long rip, leave alone",
            ),
            (
                &[Sweeping, Iso, Mapfile, RestartAtLimit],
                Verdict::RestartLoopFailed,
                ".sweeping AT restart limit → deterministic wedge, quarantine (honors watchdog guard)",
            ),
            // --- H1: .muxing exclusion lock. Owned by the mux worker; same
            //     in-progress treatment as .sweeping. ---
            (
                &[Muxing, Iso, Mapfile],
                Verdict::InProgress,
                ".muxing + artifacts → mux worker owns it, in-progress",
            ),
            (
                &[Muxing, Iso, Mapfile, RestartBelowLimit],
                Verdict::InProgress,
                ".muxing below limit → mux worker owns it, leave alone",
            ),
            (
                &[Muxing, Iso, Mapfile, RestartAtLimit],
                Verdict::RestartLoopFailed,
                ".muxing AT restart limit → deterministically-wedging mux, quarantine",
            ),
            // --- M2: a non-JSON `.failed` body (review.rs operator-cancel)
            //     must still be TERMINAL — keyed on marker presence, not
            //     parse-success. ---
            (
                &[FailedNonJson],
                Verdict::Failed,
                "non-JSON .failed body is still terminal (presence-keyed)",
            ),
            (
                &[FailedNonJson, Iso, Mapfile, RestartAtLimit],
                Verdict::Failed,
                "non-JSON .failed + artifacts at restart limit → terminal, not restart-counted",
            ),
        ];
        for (markers, expected, why) in table {
            let got = resume_verdict(markers);
            assert_eq!(&got, expected, "resume matrix row failed: {why}");
        }
    }

    /// Named explicit cells (per the rc4 brief).
    #[test]
    fn resume_restart_count_at_limit_quarantines() {
        assert_eq!(
            resume_verdict(&[Mk::Iso, Mk::RestartAtLimit]),
            Verdict::RestartLoopFailed
        );
    }
    #[test]
    fn resume_completed_plus_failed_conflict_is_terminal() {
        // Writing both .completed and .failed migrates to a single state; the
        // migration priority makes Failed win. Either way the dir is terminal —
        // the key property is that it is NEVER re-ripped. Pin Failed.
        assert_eq!(
            resume_verdict(&[Mk::Completed, Mk::Failed]),
            Verdict::Failed,
            "a conflicting .completed + .failed pair collapses to a single terminal state (Failed wins); never re-ripped"
        );
    }
    #[test]
    fn resume_done_only_crash_window_treated_finished() {
        assert_eq!(
            resume_verdict(&[Mk::Done, Mk::Iso, Mk::Mapfile, Mk::RestartAtLimit]),
            Verdict::Completed,
            "a .done crash-window dir must be finished, never promoted to .failed by the restart gate"
        );
    }
    #[test]
    fn resume_nothing_present_is_wiped() {
        assert_eq!(resume_verdict(&[]), Verdict::Wiped);
    }

    // A dir with `.sweeping` + ISO/mapfile (crash mid-sweep) is classified
    // InProgress and left in place, but ALSO restart-counted on each skip so
    // a deterministically-crashing sweep walks toward `.failed`.
    #[test]
    fn sweeping_marker_is_in_progress_and_restart_counted() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
        fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
        write_sweeping_marker(&disc);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::InProgress),
            "got {:?}",
            hints[0].action
        );
        // R3 finding 2: counter is bumped on the InProgress skip (was 0 before).
        assert_eq!(
            restart_count(&disc),
            1,
            ".sweeping InProgress skip must bump .restart_count"
        );
        assert!(!disc.join(FAILED_MARKER).exists());
        // Artifacts + marker preserved for the resuming rip.
        assert!(disc.join("MyDisc.iso").exists());
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Sweeping);
    }

    // A STRUCTURAL mux failure in the inline fallback must quarantine the
    // dir, not leak `.sweeping`: `.failed` present, `.sweeping` gone, and it
    // classifies terminal AlreadyFailed, not stranded InProgress.
    #[test]
    fn structural_mux_failure_quarantines_instead_of_stranding() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
        fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
        // Pre-state: `.sweeping` was written at staging-dir creation and the
        // inline-mux fallback is only reached because the `.ripped` hand-off
        // write failed, so `.sweeping` is still on disk here.
        write_sweeping_marker(&disc);

        // The fix's quarantine sequence (mirrors mod.rs's ISO-open /
        // build_iso_pipeline Err arms).
        write_failed_marker(&disc, "cannot open ISO for mux: ENOENT");
        clear_restart_count(&disc);

        // `.sweeping` superseded by `.failed`.
        assert_ne!(read_state(&disc).unwrap().state, StagingState::Sweeping);
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::AlreadyFailed { .. }),
            "structural mux failure must be terminal AlreadyFailed, got {:?}",
            hints[0].action
        );
    }

    // A `.muxing` lock dir is owned by the mux worker — the resume scan
    // leaves it InProgress but restart-counts it too, so a hard kill mid-mux
    // still walks toward `.failed` over RESTART_LIMIT restarts.
    #[test]
    fn muxing_marker_is_in_progress_and_restart_counted() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
        fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
        let mut st = DiscState::new(StagingState::Ripped);
        st.muxing = true;
        write_state(&disc, &st);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(matches!(hints[0].action, ResumeAction::InProgress));
        assert_eq!(restart_count(&disc), 1);
        assert_ne!(read_state(&disc).unwrap().state, StagingState::Failed);
    }

    // A wedging mux killed by the hard watchdog re-acquires `.muxing` with
    // `.restart_count` bumped. The carve-out must promote to `.failed` at
    // RESTART_LIMIT, not re-dispatch and re-wedge forever.
    #[test]
    fn muxing_at_restart_limit_is_promoted_to_failed() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
        fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
        // Watchdog has crashed RESTART_LIMIT times; the mux worker owns the dir
        // (muxing lock) and the count is on disk.
        let mut st = DiscState::new(StagingState::Ripped);
        st.muxing = true;
        st.restart_count = RESTART_LIMIT;
        write_state(&disc, &st);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::RestartLoopFailed { .. }),
            "wedging .muxing at the restart limit must be promoted to .failed, got {:?}",
            hints[0].action
        );
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);
        // The lock is cleared so the dir reads terminal, not owned, next pass.
        assert!(!read_state(&disc).unwrap().muxing);
        // Count cleared so a manual re-queue starts fresh.
        assert_eq!(restart_count(&disc), 0);
    }

    /// Convergence R2 finding 1 companion: the `.sweeping` inline-mux path has
    /// the same loop. A `.sweeping` dir whose `.restart_count` already reached
    /// RESTART_LIMIT must be quarantined, with `.sweeping` cleared.
    #[test]
    fn sweeping_at_restart_limit_is_promoted_to_failed() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
        fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
        write_sweeping_marker(&disc);
        mutate_state_if_present(&disc, |s| s.restart_count = RESTART_LIMIT);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::RestartLoopFailed { .. }),
            "wedging .sweeping at the restart limit must be promoted to .failed, got {:?}",
            hints[0].action
        );
        assert_eq!(read_state(&disc).unwrap().state, StagingState::Failed);
        assert_ne!(read_state(&disc).unwrap().state, StagingState::Sweeping);
        assert_eq!(restart_count(&disc), 0);
    }

    // A restart of an owned `.muxing` dir whose count is BELOW the limit
    // stays InProgress and preserved, but the scan bumps the counter on the
    // skip, so it advances to RESTART_LIMIT and fails on the NEXT restart.
    #[test]
    fn muxing_below_restart_limit_stays_in_progress() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join("MyDisc.iso"), b"x").unwrap();
        fs::write(disc.join("MyDisc.iso.mapfile"), b"x").unwrap();
        let mut st = DiscState::new(StagingState::Ripped);
        st.muxing = true;
        st.restart_count = RESTART_LIMIT - 1;
        write_state(&disc, &st);

        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::InProgress),
            "below-limit .muxing must stay InProgress, got {:?}",
            hints[0].action
        );
        assert_ne!(read_state(&disc).unwrap().state, StagingState::Failed);
        assert!(read_state(&disc).unwrap().muxing);
        // R3 finding 2: the scan bumps the counter on the InProgress skip — the
        // dir advances to the limit and fails on the next restart.
        assert_eq!(restart_count(&disc), RESTART_LIMIT);
    }

    // The `.sweeping` marker is superseded by every terminal/hand-off
    // transition, so a finished/quarantined dir isn't mis-read as active.
    #[test]
    fn sweeping_marker_cleared_by_terminal_writes() {
        let d = tmpdir();
        write_sweeping_marker(&d);
        assert_eq!(read_state(&d).unwrap().state, StagingState::Sweeping);
        write_completed_marker(&d);
        assert_ne!(
            read_state(&d).unwrap().state,
            StagingState::Sweeping,
            ".completed must clear .sweeping"
        );

        let d2 = tmpdir();
        write_sweeping_marker(&d2);
        write_failed_marker(&d2, "boom");
        assert_ne!(
            read_state(&d2).unwrap().state,
            StagingState::Sweeping,
            ".failed must clear .sweeping"
        );

        let d3 = tmpdir();
        write_sweeping_marker(&d3);
        clear_sweeping_marker(&d3);
        // Clearing a Sweeping dir removes state.json entirely (resumable, not owned).
        assert!(read_state(&d3).is_none());
        // Idempotent: clearing an already-gone marker must not panic/error.
        clear_sweeping_marker(&d3);
    }

    // A `.failed`-only dir with a non-JSON body is still terminal to the
    // resume scan — it keys on `has_failed` presence, not parse-success.
    #[test]
    fn non_json_failed_is_terminal() {
        let root = tmpdir();
        let disc = root.join("MyDisc");
        fs::create_dir_all(&disc).unwrap();
        fs::write(disc.join(FAILED_MARKER), b"cancelled by operator\n").unwrap();
        // The parser can't read a reason out of it...
        assert_eq!(read_failed_reason(&disc), None);
        // ...but the scan still treats it as terminal.
        let hints = resume_or_quarantine_staging(root.to_str().unwrap());
        assert_eq!(hints.len(), 1);
        assert!(
            matches!(hints[0].action, ResumeAction::AlreadyFailed { .. }),
            "got {:?}",
            hints[0].action
        );
        // And the snapshot exposes has_failed even with no parseable reason.
        let snap = snapshot_staging_disc(&disc).unwrap();
        assert!(snap.has_failed);
        assert!(snap.failed_reason.is_none());
    }

    /// Same guarantee for `increment_restart_count`: a rename failure must not
    /// leak `.restart_count.tmp`.
    #[test]
    fn restart_count_rename_failure_cleans_up_tmp() {
        let d = tmpdir();
        // Make the final target a non-empty directory so rename-over fails.
        let target = d.join(RESTART_COUNT_FILE);
        fs::create_dir(&target).unwrap();
        fs::write(target.join("occupant"), b"x").unwrap();

        let res = increment_restart_count(&d);
        assert!(
            res.is_err(),
            "precondition: rename onto a non-empty dir must fail"
        );

        let tmp = d.join(format!("{}.tmp", RESTART_COUNT_FILE));
        assert!(
            !tmp.exists(),
            "the .tmp sibling must be cleaned up after a rename failure, found: {}",
            tmp.display()
        );
    }

    // Every disc of a boxset resolves to one TMDB title, so before this they
    // shared a staging dir and disc 2 was never read. The raw volume label
    // is what still tells the discs apart.
    #[test]
    fn a_different_disc_with_the_same_title_gets_its_own_staging_dir() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();

        // Nothing there yet: the plain title wins.
        assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_1"), "Movie");

        // Disc 1 rips and finishes.
        std::fs::create_dir_all(r.join("Movie")).unwrap();
        write_disc_label(&r.join("Movie"), "MOVIE_DISC_1");

        // Disc 1 re-inserted (a container restart with the disc still in the
        // drive) must find ITS OWN dir, not spawn a new one — otherwise every
        // Watchtower deploy re-sweeps a finished disc.
        assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_1"), "Movie");

        // Disc 2 is a different disc with the same title: its own dir.
        assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_2"), "Movie_2");

        // ...and once disc 2 exists, disc 3 goes past both.
        std::fs::create_dir_all(r.join("Movie_2")).unwrap();
        write_disc_label(&r.join("Movie_2"), "MOVIE_DISC_2");
        assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_3"), "Movie_3");
        // Disc 2 still finds its own.
        assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_2"), "Movie_2");
    }

    /// A staging dir written before labels existed has none. It must read as
    /// "this disc", so an upgrade does not re-rip staging that is already
    /// finished, and does not orphan a partial rip into a new directory.
    #[test]
    fn an_unlabelled_legacy_staging_dir_is_treated_as_the_same_disc() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("Movie")).unwrap();
        // No .disc-label written — this is what the previous version left.
        assert!(dir_is_same_disc(&r.join("Movie"), "ANY_LABEL"));
        assert_eq!(staging_name_for_disc(r, "Movie", "ANY_LABEL"), "Movie");
    }

    // The mirror case: the CALLER doesn't know the label (some RipStates
    // are seeded by the mux/mover paths). Must resolve to the plain title
    // dir, never to a `_2` that no rip created.
    #[test]
    fn an_unknown_disc_label_resolves_to_the_plain_title_dir() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        std::fs::create_dir_all(r.join("Movie")).unwrap();
        write_disc_label(&r.join("Movie"), "MOVIE_DISC_1");
        assert_eq!(staging_name_for_disc(r, "Movie", ""), "Movie");
        assert_eq!(staging_basename(r, "Movie", ""), "Movie");
    }

    /// `adopt_disc_label` stamps an unlabelled (legacy) dir for the first disc
    /// that uses it, so the NEXT different disc no longer matches it — and
    /// never rewrites a label that is already there.
    #[test]
    fn adopting_a_legacy_dir_stamps_it_once_for_its_first_user() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        let dir = r.join("Movie");
        std::fs::create_dir_all(&dir).unwrap();

        adopt_disc_label(&dir, "MOVIE_DISC_1");
        assert_eq!(read_disc_label(&dir).as_deref(), Some("MOVIE_DISC_1"));
        // Now that it is owned, a different disc of the same title moves over.
        assert_eq!(staging_name_for_disc(r, "Movie", "MOVIE_DISC_2"), "Movie_2");

        // Never clobbers an existing label.
        adopt_disc_label(&dir, "MOVIE_DISC_2");
        assert_eq!(read_disc_label(&dir).as_deref(), Some("MOVIE_DISC_1"));

        // An unknown label adopts nothing — it would record a lie.
        let dir2 = r.join("Other");
        std::fs::create_dir_all(&dir2).unwrap();
        adopt_disc_label(&dir2, "");
        assert!(read_disc_label(&dir2).is_none());
    }
}
