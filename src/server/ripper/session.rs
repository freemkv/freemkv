//! Per-device drive sessions, halt/stop bookkeeping, and the registry
//! of in-flight rip threads.
//!
//! Each rip-thread spawn site allocates a single [`libfreemkv::Halt`]
//! token, registers it in [`HALTS`] keyed by device, and threads
//! `halt.clone()` through every cancellable phase (sweep / patch /
//! mux). The HTTP `/api/stop/{device}` handler looks up the device's
//! `Halt` and calls `.cancel()`; phase loops poll
//! `halt.is_cancelled()` at their tops.

use libfreemkv::Halt;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

// Global table of rip-thread JoinHandles keyed by device. Populated when the
// poll loop spawns the scan/rip thread; consumed by `join_rip_thread`
// (called from `handle_stop`, `eject_drive`, and the shutdown path).
static RIP_THREADS: once_cell::sync::Lazy<
    Mutex<std::collections::HashMap<String, JoinHandle<()>>>,
> = once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Outcome of attempting to register a rip-thread `JoinHandle`.
///
/// `register_rip_thread` returns `Err(RegisterError)` instead of
/// silently overwriting (and thus orphaning) a prior handle that is
/// still running. The variant carries the rejected handle back to the
/// caller so it is never dropped on the floor — `spawn_rip_thread`
/// joins it so the just-spawned worker can't leak, and a test can
/// inspect it.
#[derive(Debug)]
pub enum RegisterError {
    /// A prior rip thread for this device is still *running*. We refuse
    /// to overwrite it (dropping a running handle breaks
    /// drain-before-wipe — the v0.13.6 bug class). The new handle the
    /// caller passed in is returned untouched so the caller can reap it.
    PriorThreadRunning(JoinHandle<()>),
}

/// Register a rip-thread JoinHandle for `device`. The device map holds at
/// most one handle: if the prior one has finished, it is reaped and the
/// new one takes its place (`Ok(())`); if the prior one is still running,
/// it is left in place and this returns
/// `Err(RegisterError::PriorThreadRunning(handle))`, handing the caller's
/// handle back so it can be reaped instead of leaked.
///
/// Called (via [`spawn_rip_thread`]) from the poll-loop and web spawn
/// sites, and from `tests/halt_drain.rs`.
pub fn register_rip_thread(device: &str, handle: JoinHandle<()>) -> Result<(), RegisterError> {
    // Recover from poison instead of dropping the handle: a dropped
    // JoinHandle can never be reaped, breaking drain-before-wipe (v0.13.6
    // bug class). Same convention as update_state/is_busy/log.rs.
    let mut t = RIP_THREADS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prior) = t.get(device) {
        if prior.is_finished() {
            // Safe to join under the lock: is_finished()==true means
            // join() won't block. Reap quietly — no warning.
            if let Some(prior) = t.remove(device)
                && let Err(e) = prior.join()
            {
                tracing::error!(
                    device = %device,
                    "reaped prior rip thread had panicked: {:?}", e
                );
            }
        } else {
            // A still-running prior would be orphaned by an overwrite.
            // Refuse and hand the new handle back to be reaped.
            tracing::warn!(
                device = %device,
                "register_rip_thread: prior rip thread still running — refusing to overwrite (the new worker will be drained)"
            );
            return Err(RegisterError::PriorThreadRunning(handle));
        }
    }
    t.insert(device.to_string(), handle);
    Ok(())
}

pub fn take_rip_thread(device: &str) -> Option<JoinHandle<()>> {
    // Recover from poison rather than `.ok()?`: a poisoned RIP_THREADS means a
    // worker panicked, exactly when `handle_stop` must still recover the
    // handle to drain before wiping staging (else v0.13.6 stop-without-drain).
    RIP_THREADS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device)
}

/// Spawn a rip-related worker thread and register its `JoinHandle` in
/// `RIP_THREADS` atomically. Use this for every scan/rip code path —
/// `handle_stop` relies on the registration to drain the thread before
/// wiping staging. Bypassing this (`std::thread::spawn` directly)
/// reintroduces the v0.13.6 bug: stop returning before a handle was
/// registered.
///
/// `role` tags the OS thread name; `device` is the registration key.
/// Registration happens BEFORE the worker runs `f`.
pub fn spawn_rip_thread<F>(device: &str, role: &str, f: F) -> std::io::Result<()>
where
    F: FnOnce() + Send + 'static,
{
    let name = format!("{}-{}", role, device);
    // Per-thread span carrying build + device for the worker's whole life;
    // tracing spans are thread-local, so events from this crate AND from
    // libfreemkv (called synchronously here) can be attributed to a build.
    let span_build = crate::server::VERSION_LABEL;
    let span_device = device.to_string();
    let span_role = role.to_string();
    // Registration gate (see doc): `recv()` is `Ok` only if we won the slot;
    // `Err` means the sender was dropped (rejection, or an unwind between
    // spawn and decision), so the worker must abort before running `f`.
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let wrapped = move || {
        if go_rx.recv().is_err() {
            // Not registered: another worker owns this device — abort here so
            // a duplicate `/api/rip` POST can't run a second rip.
            return;
        }
        let _span =
            tracing::info_span!("worker", build = span_build, device = %span_device, role = %span_role)
                .entered();
        f();
    };
    let handle = std::thread::Builder::new().name(name).spawn(wrapped)?;
    match register_rip_thread(device, handle) {
        Ok(()) => {
            // Release the worker. A send error is impossible in practice (the
            // receiver cannot be dropped before its `recv` returns) and would
            // only mean the worker is already gone, so it is not actionable.
            let _ = go_tx.send(());
            Ok(())
        }
        Err(RegisterError::PriorThreadRunning(new_handle)) => {
            // A prior worker still owns the slot; we just spawned a duplicate.
            // Drop the gate's sender so it aborts at `recv` without touching
            // the incumbent's staging/Halt, join it, then fail the rollback.
            drop(go_tx);
            if let Err(e) = new_handle.join() {
                tracing::error!(
                    device = %device,
                    "duplicate-spawn worker panicked while being drained: {:?}", e
                );
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "a rip thread is already running for this device",
            ))
        }
    }
}

/// Wait (up to `timeout`) for the rip thread for `device` to exit. Returns
/// `Ok(())` if the thread finished within the window or no thread was
/// registered; `Err(())` on timeout.
///
/// Best-effort drain: a timeout is a warning, not a fatal error — the
/// `Halt` was already cancelled by the stop path, so the thread will exit
/// eventually; the timeout just bounds how long the caller blocks.
///
/// The handle is POLLED IN PLACE, every 25 ms — it never leaves
/// `RIP_THREADS` for the duration of the wait.
#[allow(clippy::result_unit_err)]
pub fn join_rip_thread(device: &str, timeout: Duration) -> Result<(), ()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // One short lock per poll. `Observed` is computed under the lock and
        // the guard is dropped before we sleep, join, or return.
        enum Observed {
            /// No handle registered — nothing to drain.
            Absent,
            /// The registered handle is THIS thread (see the self-join note).
            SelfJoin,
            Finished,
            Running,
        }
        let observed = {
            // Recover from poison: a poisoned map means a worker panicked,
            // exactly when the stop path must still drain before staging is
            // touched. Same convention as everywhere else in this module.
            let t = RIP_THREADS.lock().unwrap_or_else(|e| e.into_inner());
            match t.get(device) {
                None => Observed::Absent,
                Some(h) if h.thread().id() == std::thread::current().id() => Observed::SelfJoin,
                Some(h) if h.is_finished() => Observed::Finished,
                Some(_) => Observed::Running,
            }
        };
        match observed {
            Observed::Absent => return Ok(()),
            // Self-join: we're *on* the registered rip thread (e.g. eject_drive
            // from rip_disc's own auto-eject tail). is_finished() can never
            // become true here, so return now and leave the handle registered.
            Observed::SelfJoin => return Ok(()),
            Observed::Finished => break,
            Observed::Running => {
                if std::time::Instant::now() >= deadline {
                    return Err(());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    // Finished: remove and reap. A concurrent joiner may have got there first,
    // in which case the entry is gone and there is nothing left to do.
    // `is_finished() == true` guarantees `join()` returns without blocking.
    if let Some(handle) = take_rip_thread(device)
        && let Err(e) = handle.join()
    {
        // join() returns Err(payload) if the thread panicked. The thread DID
        // finish (so we return Ok), but surface the panic so stop / eject /
        // shutdown don't treat a panicked rip as a clean exit.
        tracing::error!(device = %device, "rip thread panicked: {:?}", e);
    }
    Ok(())
}

/// Drain every known rip thread within a single shared `timeout`
/// budget (NOT per-device). The stop path cancels every device's
/// `Halt` token before this is called, so the threads are already
/// winding down in parallel — bounding each `join_rip_thread` by the
/// full `timeout` would let an N-drive shutdown block up to
/// N×`timeout`. Compute one deadline up front and hand each join the
/// time remaining against it, so a 4-drive shutdown is capped at 1×
/// `timeout` total.
pub fn join_all_rip_threads(timeout: Duration) {
    let devices: Vec<String> = RIP_THREADS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect();
    // Cancel every active rip's halt FIRST, then join: cancelling makes the
    // rip return via its guard Drop impls, leaving a clean resumable dir —
    // otherwise a stale `.sweeping` reads as a crash. (Bitten 2026-06-30.)
    for device in &devices {
        if let Some(h) = device_halt(device) {
            h.cancel();
        }
    }
    let deadline = std::time::Instant::now() + timeout;
    for device in devices {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if join_rip_thread(&device, remaining).is_err() {
            tracing::warn!(device = %device, "rip thread did not drain within timeout");
        }
    }
}

// Per-device cooperative-cancel tokens; the rip thread spawn site allocates one Halt per rip
// and stashes its clone here so the HTTP stop handler (and `eject_drive`) can find it.
static HALTS: once_cell::sync::Lazy<Mutex<std::collections::HashMap<String, Halt>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

/// Stash the rip thread's [`Halt`] for later lookup by the stop /
/// eject paths. Called once at the top of every rip; any prior token
/// for the same device is dropped.
pub fn register_halt(device: &str, halt: Halt) {
    // Recover from poison: a silently-dropped registration means
    // /api/stop can never find this device's token, turning Stop into a
    // silent no-op. Recover-and-proceed (same convention as update_state).
    let mut halts = HALTS.lock().unwrap_or_else(|e| e.into_inner());
    halts.insert(device.to_string(), halt);
}

/// Look up the device's currently-registered [`Halt`]. Returns `None`
/// if no rip thread is registered for `device`. Cloning the returned
/// token is cheap (Arc bump) — clones share the underlying flag with
/// the rip-side clones already threaded into sweep / patch / mux.
pub fn device_halt(device: &str) -> Option<Halt> {
    // Recover from poison: returning None on poison would make /api/stop a
    // silent no-op (it looks up the token through here). Recover-and-proceed.
    let halts = HALTS.lock().unwrap_or_else(|e| e.into_inner());
    halts.get(device).cloned()
}

/// Stop a device's rip thread and drain it: cancel the per-device [`Halt`]
/// token (which every rip-phase clone polls), then block until the thread
/// finishes or `timeout` elapses. Returns `true` if the thread drained
/// (finished, or there was none) within the budget, `false` on timeout.
///
/// This is the core of the HTTP `/api/stop` handler, extracted so the
/// stop→drain contract is testable by driving the REAL function rather
/// than a replica. `handle_stop` calls this, then layers on the
/// verify-worker drain + STATE reset specific to the web path.
pub fn stop_and_drain(device: &str, timeout: Duration) -> bool {
    if let Some(halt) = device_halt(device) {
        halt.cancel();
    }
    join_rip_thread(device, timeout).is_ok()
}

/// Atomically swap in a new [`Halt`] for `device`, carrying forward a Stop
/// that landed on the outgoing token. Under a SINGLE acquisition of the
/// `HALTS` lock this: reads the outgoing token's `is_cancelled()`, inserts
/// `new`, and — if the old token was already cancelled — cancels `new`.
///
/// Closes a TOCTOU race at the placeholder→real token swap in `rip_disc`:
/// doing read/insert/cancel as three separate steps let a concurrent
/// `/api/stop` landing between the read and the insert get lost, hanging
/// the drain. Holding the lock across all three serialises the two paths.
pub fn swap_halt_carrying_cancel(device: &str, new: Halt) {
    // Recover from poison (same convention as register_halt / device_halt):
    // a dropped swap would strand /api/stop on a token no phase loop reads.
    let mut halts = HALTS.lock().unwrap_or_else(|e| e.into_inner());
    let prior_cancelled = halts.get(device).map(|h| h.is_cancelled()).unwrap_or(false);
    if prior_cancelled {
        // Cancel BEFORE inserting so the token is already in its final state
        // when it becomes visible; a concurrent /api/stop now either sees the
        // old token (and we carry it) or the new cancelled one.
        new.cancel();
    }
    halts.insert(device.to_string(), new);
}

/// Drop the device's registered [`Halt`]. Called from the rip-thread
/// cleanup paths (every early-return branch in `rip_disc`) so a
/// subsequent rip on the same device starts with a fresh token.
pub fn unregister_halt(device: &str) {
    // Recover from poison rather than leaking a stale token that would
    // shadow the next rip's fresh Halt. Recover-and-proceed.
    let mut halts = HALTS.lock().unwrap_or_else(|e| e.into_inner());
    halts.remove(device);
}

/// Roll a device back to idle after a failed `spawn_rip_thread`, undoing
/// the claim identified by `claim_gen` (the value the caller's own
/// [`super::try_claim_active_checked`] returned) and nothing else. The
/// single rollback used by both web handlers and the disc-insert poll
/// loop; the disc is assumed still present.
///
/// Scoped to `claim_gen`, not a liveness check: a failed spawn may be this
/// device's own claim, or a duplicate that lost to a still-running
/// incumbent — rolling back unconditionally would vandalise the winner.
pub fn rollback_failed_spawn(device: &str, claim_gen: u64) {
    {
        // Recover-and-proceed on poison (module convention). Guard is dropped
        // before `unregister_halt` / `update_state` so no two locks are held.
        let s = super::STATE.lock().unwrap_or_else(|e| e.into_inner());
        let current = s.get(device).map(|r| r.claim_gen);
        if current != Some(claim_gen) {
            tracing::warn!(
                device = %device,
                rolling_back = claim_gen,
                in_force = ?current,
                "declining spawn rollback: the device has been re-claimed since \
                 (the spawn that failed was the duplicate, not the incumbent)"
            );
            return;
        }
    }
    super::unregister_halt(device);
    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "idle".to_string(),
            disc_present: true,
            ..Default::default()
        },
    );
}

// ─── Per-device drive session ──────────────────────────────────────────────

/// Persistent drive session — survives across scan → rip transitions.
/// Dropped on eject, stop, or error.
pub(super) struct DriveSession {
    pub(super) drive: libfreemkv::Drive,
    pub(super) disc: Option<libfreemkv::Disc>,
    pub(super) scanned: bool,
    pub(super) probed: bool,
    pub(super) tmdb: Option<crate::server::tmdb::TmdbResult>,
    pub(super) device_path: String,
    /// scan_disc's final key-service verdict for a still-keyless disc, so rip_disc
    /// classifies from the real `/decode` answer instead of re-probing.
    pub(super) key_verdict: Option<crate::server::keysource::ServiceReachability>,
    /// The rip's key set, resolved once right after the scan (KU §2.1); memory only.
    pub(super) keys: Option<libfreemkv::keys::KeyRing>,
    /// Why the scan's resolve refused (no key, a key-source failure, …), when it did.
    pub(super) key_error: Option<libfreemkv::Error>,
}

/// Global drive sessions — one per device.
static SESSIONS: once_cell::sync::Lazy<Mutex<std::collections::HashMap<String, DriveSession>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

// Last-known disc identity per device (UDF Volume Identifier). Kept separate from
// `DriveSession` so it OUTLIVES the session for `rediscover_drive`.
static DISC_IDENTITY: once_cell::sync::Lazy<Mutex<std::collections::HashMap<String, String>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashMap::new()));

pub(super) fn take_session(device: &str) -> Option<DriveSession> {
    // Recover-and-proceed on poison (matching store_session / register_halt):
    // returning None here would have the caller open a fresh drive even though
    // a usable session was sitting under the poisoned lock.
    SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device)
}

// Record (or clear) the device's cached disc identity for the rediscovery path. An empty
// `volume_id` still CLEARS any previous entry (skipping the write would leave a stale
// identity).
pub(super) fn cache_disc_identity(device: &str, volume_id: &str) {
    let vid = volume_id.trim();
    // Recover-and-proceed on poison (module convention): a skipped clear is
    // a stale identity, which is worse than no identity.
    let mut ids = DISC_IDENTITY.lock().unwrap_or_else(|e| e.into_inner());
    if vid.is_empty() {
        ids.remove(device);
    } else {
        ids.insert(device.to_string(), vid.to_string());
    }
}

pub(super) fn store_session(device: &str, session: DriveSession) {
    // Cache the scanned disc's volume identifier before storing, so the
    // rediscovery path can match it after the session is dropped.
    cache_disc_identity(
        device,
        session
            .disc
            .as_ref()
            .map(|d| d.volume_id.as_str())
            .unwrap_or(""),
    );
    // Recover-and-proceed on poison (matching register_halt / register_rip_thread):
    // dropping the session silently would make session_is_scanned return false
    // and fire a redundant 10-30s re-scan (clearing TMDB metadata in the UI).
    SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(device.to_string(), session);
}

// Is a rip/scan worker for `device` still running? A fact `is_busy` cannot give: a worker
// writes its TERMINAL status, then keeps running its tail.
pub(super) fn rip_thread_running(device: &str) -> bool {
    // Recover-and-proceed on poison (module convention): a poisoned map means
    // a worker panicked, and reporting "nothing is running" there is the
    // permission-to-tear-down this predicate exists to withhold.
    RIP_THREADS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .is_some_and(|h| !h.is_finished())
}

// Evict per-device state on hot-unplug teardown: DISC_IDENTITY always, plus RIP_THREADS/HALTS
// only once the rip thread has exited. Returns true if a finished handle was reaped.
pub(super) fn forget_device_session_state(device: &str) -> bool {
    // Recover-and-proceed on poison (module convention): a skipped eviction
    // here is the unbounded growth this function exists to prevent.
    DISC_IDENTITY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
    let reaped = {
        let mut t = RIP_THREADS.lock().unwrap_or_else(|e| e.into_inner());
        if t.get(device).is_some_and(|h| h.is_finished()) {
            // is_finished() == true means join() cannot block, so joining
            // under the lock is safe (same argument as register_rip_thread).
            if let Some(h) = t.remove(device)
                && let Err(e) = h.join()
            {
                tracing::error!(
                    device = %device,
                    "reaped rip thread of a removed device had panicked: {:?}", e
                );
            }
            true
        } else {
            false
        }
    };
    // Take HALTS only after RIP_THREADS is released — never nest the two.
    if reaped {
        unregister_halt(device);
    }
    reaped
}

/// The UDF Volume Identifier last scanned for `device`, if any. Used by
/// `rediscover_drive` to verify a re-enumerated candidate carries the
/// SAME disc, not an unrelated one in a neighbouring drive.
pub(super) fn expected_volume_id(device: &str) -> Option<String> {
    // Recover-and-proceed on poison (module convention): `.ok()?` would return
    // None on a poisoned lock, driving `rediscover_drive` into its UNVERIFIED
    // branch where it can latch the wrong disc in a neighbouring drive.
    DISC_IDENTITY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned()
}

// True iff `device` has a stored `DriveSession` with `scanned == true`. Lets
// `handle_rip_request` skip a redundant re-scan (which clears the TMDB poster/title).
pub(super) fn session_is_scanned(device: &str) -> bool {
    // Recover-and-proceed on poison (module convention): `.ok()` alone would
    // abandon this check forever after any panic elsewhere under SESSIONS.
    SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .map(|sess| sess.scanned)
        .unwrap_or(false)
}

/// The key set and Volume ID the scan of `device`'s disc resolved (memory only): what an
/// inserted disc lends its staged image's resume.
pub(super) fn session_rip_keys(
    device: &str,
) -> (Option<libfreemkv::keys::KeyRing>, Option<[u8; 16]>) {
    let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(sess) = sessions.get(device) else {
        return (None, None);
    };
    let vid = sess
        .disc
        .as_ref()
        .and_then(|d| d.aacs.as_ref())
        .map(|a| a.volume_id)
        .filter(|v| *v != [0u8; 16]);
    (sess.keys.clone(), vid)
}

pub(super) fn drop_session(device: &str) {
    // Recover-and-proceed on poison (matching store_session / take_session):
    // silently no-op'ing here would leak a stale DriveSession in the map, so a
    // later store_session would overwrite it and warn about a missing removal.
    SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

/// After a USB re-enumeration (bridge crash), the sg device number may
/// change. Probe the original path and its neighbors to find the drive
/// that still has the disc. Returns the new device path (e.g. "/dev/sg5").
pub(super) fn rediscover_drive(
    device: &str,
    original_path: &str,
    halt: &libfreemkv::Halt,
) -> Option<String> {
    rediscover_drive_with(
        device,
        original_path,
        expected_volume_id(device).as_deref(),
        |p| libfreemkv::disc_presence(std::path::Path::new(p)),
        probe_volume_id,
        || sleep_unless_halted(halt, SETTLE_PAUSE),
    )
}

/// Sleep `dur`, waking early on `halt`; false if halted.
pub(super) fn sleep_unless_halted(halt: &libfreemkv::Halt, dur: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + dur;
    while !halt.is_cancelled() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(libfreemkv::halt::WAIT_SLICE));
    }
    false
}

// Pause between re-probes of a candidate whose presence is still settling.
const SETTLE_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);
const SETTLE_RETRIES: u32 = 10;

// `rediscover_drive` with its hardware I/O injected; `pause` returns false once halted.
fn rediscover_drive_with(
    device: &str,
    original_path: &str,
    expected_vid: Option<&str>,
    mut presence: impl FnMut(&str) -> libfreemkv::Result<libfreemkv::DiscPresence>,
    mut volume_id: impl FnMut(&str) -> Option<String>,
    mut pause: impl FnMut() -> bool,
) -> Option<String> {
    use libfreemkv::DiscPresence::{Present, Settling};
    // TODO(step1-followup): not moved into DiscSession — entangled with
    // disc-identity/device_log/sg-shift logic; left per contract Q3. Only
    // valid for /dev/sgN; bail rather than risk latching a wrong drive.
    let sg_num = match original_path
        .rsplit('/')
        .next()
        .and_then(|s| s.strip_prefix("sg"))
        .and_then(|n| n.parse::<i32>().ok())
    {
        Some(n) => n,
        None => {
            tracing::warn!(
                device = %device,
                path = %original_path,
                "rediscover_drive: path is not /dev/sgN, skipping rediscovery"
            );
            return None;
        }
    };

    // Stable disc identifier from the last scan. A candidate at a SHIFTED sg
    // number must carry the same disc before we accept it, else a neighbour
    // with an unrelated disc could win; when absent, fall back unverified.
    for delta in [0i32, -1, 1, -2, 2, -3, 3] {
        let probe_num = sg_num + delta;
        if probe_num < 0 {
            continue;
        }
        let path = format!("/dev/sg{probe_num}");
        // The original node settling is our drive spinning up (the re-open path waits
        // for it); a settling neighbour is re-probed, bounded, until it settles.
        let mut answer = presence(&path);
        let mut retries = 0;
        while !path_unchanged(delta) && matches!(answer, Ok(Settling)) && retries < SETTLE_RETRIES {
            if !pause() {
                return None;
            }
            retries += 1;
            answer = presence(&path);
        }
        let accept = match answer {
            Ok(Present) => true,
            Ok(Settling) => path_unchanged(delta),
            _ => false,
        };
        if !accept {
            continue;
        }

        // delta == 0: same physical device node, so it's by definition the
        // same drive — accept without a disc-identity read, which would
        // just be a redundant probe of a drive we already trust.
        if path_unchanged(delta) {
            tracing::info!(
                device = %device,
                new_path = %path,
                "rediscovered drive after USB re-enumeration (path unchanged)"
            );
            return Some(path);
        }

        // Shifted sg number — could be the intended drive or a neighbour.
        // Verify the candidate's disc identity if known; with no stored
        // identity, keep the legacy disc-present behaviour but flag unverified.
        let Some(expected) = expected_vid else {
            tracing::warn!(
                device = %device,
                new_path = %path,
                "rediscovered drive after USB re-enumeration (UNVERIFIED — no stored disc identity to confirm it is the same disc)"
            );
            return Some(path);
        };

        let probed = volume_id(&path);
        if candidate_identity_confirmed(probed.as_deref(), expected) {
            let vid = probed.as_deref().unwrap_or_default();
            tracing::info!(
                device = %device,
                new_path = %path,
                volume_id = %vid_for_log(vid),
                "rediscovered drive after USB re-enumeration (disc identity confirmed)"
            );
            return Some(path);
        }
        match probed {
            Some(vid) => {
                tracing::warn!(
                    device = %device,
                    candidate = %path,
                    candidate_volume_id = %vid_for_log(&vid),
                    expected_volume_id = %vid_for_log(expected),
                    "skipping rediscovery candidate — disc identity mismatch (unrelated disc in a neighbouring drive)"
                );
            }
            None => {
                tracing::warn!(
                    device = %device,
                    candidate = %path,
                    "skipping rediscovery candidate — could not read disc identity to confirm match"
                );
            }
        }
    }
    None
}

// True when a rediscovery probe's sg-number delta indicates the candidate
// path is unchanged (same physical device node, no identity verification
// needed). Extracted from `rediscover_drive` for unit-testability.
fn path_unchanged(delta: i32) -> bool {
    delta == 0
}

// Whether a shifted-sg candidate's probed volume id confirms it carries the
// expected disc (`probed` is `None` if the identity read itself failed).
// Extracted from `rediscover_drive` for unit-testability without hardware I/O.
fn candidate_identity_confirmed(probed: Option<&str>, expected: &str) -> bool {
    probed == Some(expected)
}

// A disc-supplied UDF Volume Identifier, made safe to put in a log field — these `tracing`
// fields reach `autorip.log`/stderr unescaped, so a crafted disc could inject ANSI.
fn vid_for_log(vid: &str) -> String {
    crate::server::log::sanitize_log_msg(vid)
}

// Read the UDF Volume Identifier of the disc in the drive at `path`, for
// rediscovery identity matching. Returns None on any failure; the caller
// treats that as "not a confirmed match" and keeps probing.
fn probe_volume_id(path: &str) -> Option<String> {
    // TODO(step1-followup): NOT migrated to DiscSession, which treats
    // wait_ready/init failures as advisory instead of fail-fast-to-None —
    // folding it in would change rediscovery's short-circuit semantics.
    let mut drive = libfreemkv::Drive::open(std::path::Path::new(path)).ok()?;
    drive.wait_ready().ok()?;
    drive.init().ok()?;
    let id = libfreemkv::Disc::identify(&mut drive).ok()?;
    drive.close();
    let vid = id.volume_id.trim();
    if vid.is_empty() {
        None
    } else {
        Some(vid.to_string())
    }
}

#[cfg(test)]
mod rediscover_tests {
    use super::{rediscover_drive_with, sleep_unless_halted};
    use libfreemkv::DiscPresence::{self, Absent, Present, Settling};
    use std::collections::HashMap;

    // Rediscovery from /dev/sg4 against scripted per-path presence answers (the last
    // answer repeats; unscripted paths are Absent). Returns the result and probe counts.
    fn rediscover(
        script: &[(&str, &[DiscPresence])],
        vids: &[(&str, &str)],
        keep_waiting: bool,
    ) -> (Option<String>, HashMap<String, usize>) {
        let mut probes: HashMap<String, usize> = HashMap::new();
        let found = rediscover_drive_with(
            "sg4",
            "/dev/sg4",
            Some("VID"),
            |p| {
                let n = probes.entry(p.to_string()).or_default();
                *n += 1;
                let seq = script.iter().find(|(q, _)| *q == p).map(|(_, s)| *s);
                Ok(seq.map_or(Absent, |s| s[(*n - 1).min(s.len() - 1)]))
            },
            |p| {
                vids.iter()
                    .find(|(q, _)| *q == p)
                    .map(|(_, v)| v.to_string())
            },
            || keep_waiting,
        );
        (found, probes)
    }

    // The original node spinning up after a USB reset is our drive; the re-open
    // path (open_drive_with_backoff, wait_ready) already waits for it.
    #[test]
    fn the_original_node_spinning_up_is_accepted_without_waiting() {
        let (found, probes) = rediscover(&[("/dev/sg4", &[Settling, Present])], &[], true);
        assert_eq!(found.as_deref(), Some("/dev/sg4"));
        assert_eq!(probes["/dev/sg4"], 1);
    }

    #[test]
    fn a_neighbour_spinning_up_is_accepted_once_present() {
        let (found, probes) = rediscover(
            &[("/dev/sg3", &[Settling, Settling, Present])],
            &[("/dev/sg3", "VID")],
            true,
        );
        assert_eq!(found.as_deref(), Some("/dev/sg3"));
        assert_eq!(
            probes["/dev/sg3"], 3,
            "a settling neighbour must be re-probed"
        );
    }

    #[test]
    fn a_settling_neighbour_that_turns_out_empty_is_not_accepted() {
        let (found, _) = rediscover(
            &[("/dev/sg3", &[Settling, Absent]), ("/dev/sg5", &[Present])],
            &[("/dev/sg3", "VID"), ("/dev/sg5", "VID")],
            true,
        );
        assert_eq!(found.as_deref(), Some("/dev/sg5"));
    }

    #[test]
    fn a_neighbour_that_never_settles_is_retried_a_bounded_number_of_times() {
        let (found, probes) = rediscover(&[("/dev/sg3", &[Settling])], &[], true);
        assert_eq!(
            found, None,
            "a never-settling neighbour must not be accepted"
        );
        assert!(
            (2..=super::SETTLE_RETRIES as usize + 1).contains(&probes["/dev/sg3"]),
            "bounded re-probing; got {}",
            probes["/dev/sg3"]
        );
    }

    #[test]
    fn a_halt_stops_rediscovery_while_a_neighbour_settles() {
        let (found, probes) = rediscover(
            &[("/dev/sg3", &[Settling]), ("/dev/sg5", &[Present])],
            &[("/dev/sg5", "VID")],
            false,
        );
        assert_eq!(
            found, None,
            "a halted rediscovery must not hand back a drive"
        );
        assert_eq!(probes["/dev/sg3"], 1, "no re-probe after the halt");
        assert!(
            !probes.contains_key("/dev/sg5"),
            "no further candidates after the halt"
        );
    }

    #[test]
    fn sleep_unless_halted_wakes_promptly_on_a_halt() {
        let halt = libfreemkv::Halt::new();
        halt.cancel();
        let t0 = std::time::Instant::now();
        assert!(!sleep_unless_halted(
            &halt,
            std::time::Duration::from_secs(5)
        ));
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            t0.elapsed()
        );
    }
}

#[cfg(test)]
mod rollback_tests {
    use super::*;

    // A disc-supplied volume-id must not carry terminal escapes into a log (rediscovery's
    // `tracing` fields reach autorip.log/stderr unescaped).
    #[test]
    fn a_volume_id_reaches_a_log_field_with_no_terminal_escapes() {
        // ESC [ 2 J is "clear screen"; a bare CR hides the line before it.
        let hostile = "DISC\x1b[2J\rHARMLESS\x07";
        let logged = vid_for_log(hostile);
        assert!(
            !logged.chars().any(|c| c.is_control()),
            "a disc-supplied volume-id must reach a log field with no control \
             bytes; got {logged:?}"
        );
        assert!(
            logged.contains("DISC") && logged.contains("HARMLESS"),
            "sanitising must not destroy the identifier's readable text; got {logged:?}"
        );
    }

    // Catches the mutation dropping rollback_failed_spawn's generation check, and the one
    // restoring the round-1 rip_thread_running early return.
    #[test]
    fn rollback_scoped_to_its_own_claim_spares_the_winner_and_clears_the_loser() {
        let dev = format!("rollback-live-worker-test-{}", std::process::id());
        let _ = super::take_rip_thread(&dev);
        let winner_gen = super::super::try_claim_active(&dev).expect("claim must succeed");
        let winner_halt = Halt::new();
        super::super::register_halt(&dev, winner_halt);

        // A worker that is still on the CPU — the incumbent/winner.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        super::spawn_rip_thread(&dev, "rip", move || {
            let _ = release_rx.recv();
        })
        .expect("the first spawn owns the device");

        // The loser rolls back a claim that is NO LONGER in force (the winner's
        // generation is the current one). Nothing of the winner's may move.
        let stale_gen = winner_gen.saturating_sub(1);
        super::rollback_failed_spawn(&dev, stale_gen);
        assert!(
            super::super::device_halt(&dev).is_some(),
            "a rollback for a superseded claim must NOT unregister the live \
             worker's Halt — /api/stop would then have no token with which to \
             cancel the running rip"
        );
        assert_eq!(
            super::super::STATE
                .lock()
                .unwrap()
                .get(&dev)
                .map(|r| r.status.clone()),
            Some("scanning".to_string()),
            "a rollback for a superseded claim must NOT idle the winner"
        );

        // The H2 wedge: the claim IN FORCE is rolled back while a worker is
        // still alive — the shape of a losing `/api/rip` whose spawn came back
        // `PriorThreadRunning`. Round 1 returned early here, wedging "scanning".
        super::super::register_halt(&dev, Halt::new());
        super::rollback_failed_spawn(&dev, winner_gen);
        assert!(
            super::super::device_halt(&dev).is_none(),
            "rolling back the claim in force must also release the Halt that \
             claim registered, or the next rip inherits a stale token"
        );
        assert_eq!(
            super::super::STATE
                .lock()
                .unwrap()
                .get(&dev)
                .map(|r| r.status.clone()),
            Some("idle".to_string()),
            "rolling back the claim that is in force must clear it — leaving it \
             set wedges every route on this device at 409 with no thread and no \
             Halt to recover from"
        );

        drop(release_tx);
        let _ = super::join_rip_thread(&dev, std::time::Duration::from_secs(10));
        super::super::STATE.lock().unwrap().remove(&dev);
        let _ = super::take_rip_thread(&dev);
        super::super::unregister_halt(&dev);
    }

    #[test]
    fn rollback_failed_spawn_clears_halt_and_idles() {
        let dev = format!("rollback-test-{}", std::process::id());
        // Simulate the pre-spawn state: claim (sets status=scanning) +
        // register a Halt, exactly as the poll loop / web handlers do.
        let claim_gen = super::super::try_claim_active(&dev).expect("claim must succeed");
        super::super::register_halt(&dev, Halt::new());
        assert!(super::super::device_halt(&dev).is_some(), "halt registered");

        super::super::rollback_failed_spawn(&dev, claim_gen);

        // Halt is gone (no leak) and the device is idle with the disc still
        // present, so a future scan/rip is not wedged at 409.
        assert!(
            super::super::device_halt(&dev).is_none(),
            "rollback must unregister the halt"
        );
        let snap = super::super::STATE
            .lock()
            .unwrap()
            .get(&dev)
            .cloned()
            .expect("state entry exists");
        assert_eq!(snap.status, "idle", "must roll back to idle");
        assert!(snap.disc_present, "disc still present after rollback");
        assert!(!super::super::is_busy(&dev), "device no longer busy");
        // The map lookup above only proves the KEY is right, not that the
        // struct's own `device` field in rollback's RipState literal survived.
        assert_eq!(
            snap.device, dev,
            "device field in the rollback RipState must match"
        );
    }

    #[test]
    fn swap_halt_carrying_cancel_carries_forward_a_pending_cancel() {
        // HIGH-ish (rule 3, TOCTOU): swap_halt_carrying_cancel exists so a Stop
        // landing on the OUTGOING placeholder between allocation and swap isn't
        // lost. Pin both: cancelled outgoing carries forward; clean one doesn't.
        let dev = format!("swap-halt-test-{}", std::process::id());

        // Case 1: outgoing token already cancelled (a Stop raced the swap).
        let placeholder = Halt::new();
        super::super::register_halt(&dev, placeholder.clone());
        placeholder.cancel();
        let real = Halt::new();
        super::super::swap_halt_carrying_cancel(&dev, real.clone());
        assert!(
            real.is_cancelled(),
            "a Stop that landed on the outgoing placeholder must carry \
             forward onto the freshly-swapped-in token, or the drain hangs \
             waiting for a Halt nobody ever cancels"
        );

        // Case 2: outgoing token NOT cancelled — the new token must stay
        // live, not get spuriously cancelled by the swap itself.
        let dev2 = format!("swap-halt-test-clean-{}", std::process::id());
        let placeholder2 = Halt::new();
        super::super::register_halt(&dev2, placeholder2);
        let real2 = Halt::new();
        super::super::swap_halt_carrying_cancel(&dev2, real2.clone());
        assert!(
            !real2.is_cancelled(),
            "swapping in a new Halt must not cancel it when nothing asked \
             to stop"
        );
    }

    #[test]
    fn path_unchanged_only_for_zero_delta() {
        // delta==0 skips disc-identity verification entirely (same device
        // node => trusted by construction); any nonzero delta is a SHIFTED
        // candidate and must go through the identity check instead.
        assert!(path_unchanged(0));
        assert!(!path_unchanged(1));
        assert!(!path_unchanged(-1));
        assert!(!path_unchanged(3));
    }

    #[test]
    fn candidate_identity_confirmed_requires_exact_match() {
        // A shifted-sg candidate is accepted only if its probed volume id
        // EXACTLY matches expected; a failed probe or a different disc's id
        // must be rejected, or a neighbour's disc could hijack the rip session.
        assert!(candidate_identity_confirmed(
            Some("DISC_VOL_123"),
            "DISC_VOL_123"
        ));
        assert!(!candidate_identity_confirmed(
            Some("SOME_OTHER_DISC"),
            "DISC_VOL_123"
        ));
        assert!(!candidate_identity_confirmed(None, "DISC_VOL_123"));
    }

    // Swapping in a disc with NO volume label must not leave the previous disc's identity
    // cached (the old `filter`-and-skip form left it stale).
    #[test]
    fn an_unlabelled_disc_clears_the_previous_discs_cached_identity() {
        let dev = format!("disc-identity-swap-{}", std::process::id());

        cache_disc_identity(&dev, "FIRST_DISC_VOL");
        assert_eq!(
            expected_volume_id(&dev).as_deref(),
            Some("FIRST_DISC_VOL"),
            "a labelled disc is cached"
        );

        // Operator swaps in a disc with no UDF volume label.
        cache_disc_identity(&dev, "");
        assert_eq!(
            expected_volume_id(&dev),
            None,
            "an unlabelled disc must leave NO identity, not the ejected \
             disc's — verifying a rediscovery candidate against a disc that \
             is no longer in the drive is how the wrong disc gets attached"
        );

        // Whitespace-only is the same non-label (the label is trimmed).
        cache_disc_identity(&dev, "SECOND_DISC_VOL");
        cache_disc_identity(&dev, "   ");
        assert_eq!(expected_volume_id(&dev), None);
    }

    // Regression: hot-unplug teardown must not leak this module's per-device maps
    // (RIP_THREADS/DISC_IDENTITY/HALTS), as it used to.
    #[test]
    fn forgetting_a_removed_device_reaps_its_finished_thread_and_identity() {
        // Fixture name unique to this test: RIP_THREADS / DISC_IDENTITY /
        // HALTS are process-global and shared across the whole test binary.
        let dev = format!("forget-reap-{}", std::process::id());

        // A worker that exits immediately, registered exactly as the poll
        // loop registers a real rip thread.
        spawn_rip_thread(&dev, "rip", || {}).expect("spawn must succeed");
        register_halt(&dev, Halt::new());
        DISC_IDENTITY
            .lock()
            .unwrap()
            .insert(dev.clone(), "VOL_FORGET_REAP".to_string());

        // Watchdog: wait for the worker to exit before asserting the reap,
        // without blocking the suite forever. 5s is ample margin for an
        // empty closure's spawn+exit; a hung regression fails here instead.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let finished = RIP_THREADS
                .lock()
                .unwrap()
                .get(&dev)
                .is_some_and(|h| h.is_finished());
            if finished {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker thread did not exit within 5s"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        super::super::state::forget_device_state(&dev);

        assert!(
            !RIP_THREADS.lock().unwrap().contains_key(&dev),
            "a finished JoinHandle must be reaped when the device is torn \
             down, not left in RIP_THREADS forever"
        );
        assert!(
            !DISC_IDENTITY.lock().unwrap().contains_key(&dev),
            "the cached disc identity must be dropped when the device is \
             torn down — nothing else ever removes from DISC_IDENTITY"
        );
        assert!(
            super::super::device_halt(&dev).is_none(),
            "the halt token of an exited thread must go with its handle"
        );
    }

    // The other half of the contract: a still-RUNNING rip thread must keep its registration, or
    // a later drain returns while it is mid-write.
    #[test]
    fn forgetting_a_device_leaves_a_still_running_thread_registered() {
        let dev = format!("forget-keep-running-{}", std::process::id());
        let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_gate = gate.clone();
        spawn_rip_thread(&dev, "rip", move || {
            // Watchdog: 5 s is the ceiling, not the expectation — release
            // comes almost immediately. This only stops a regression from
            // parking this thread for the life of the suite.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !worker_gate.load(std::sync::atomic::Ordering::SeqCst)
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
        .expect("spawn must succeed");
        register_halt(&dev, Halt::new());

        super::super::state::forget_device_state(&dev);

        assert!(
            RIP_THREADS.lock().unwrap().contains_key(&dev),
            "a still-running rip thread's handle must survive teardown — \
             dropping it makes the thread unjoinable and breaks \
             drain-before-wipe"
        );
        assert!(
            super::super::device_halt(&dev).is_some(),
            "a still-running thread's Halt must stay reachable so /api/stop \
             can still cancel it"
        );

        gate.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = join_rip_thread(&dev, Duration::from_secs(5));
    }

    // Regression: take_session/drop_session must recover from a poisoned SESSIONS lock, not
    // silently no-op as the old `.lock().ok()?` form did.
    #[test]
    fn session_helpers_recover_from_poison() {
        // Poison SESSIONS by panicking while the guard is held.
        let _ = std::panic::catch_unwind(|| {
            let _guard = SESSIONS.lock().unwrap();
            panic!("intentional poison");
        });
        assert!(SESSIONS.is_poisoned(), "lock must be poisoned for the test");

        let dev = format!("poison-test-{}", std::process::id());
        // Neither helper may panic on the poisoned lock; both must run.
        assert!(
            take_session(&dev).is_none(),
            "take_session on poisoned lock returns None for an absent device, not panic"
        );
        drop_session(&dev); // must not panic
        // session_is_scanned must also recover, not abandon (the old `.ok()`
        // form would wedge it at `false` forever for the rest of the binary).
        assert!(
            !session_is_scanned(&dev),
            "session_is_scanned on poisoned lock must recover and answer, not panic"
        );
    }

    // Catches the mutation deleting join_rip_thread's self-join branch: it runs ON its own
    // thread from eject_drive, where is_finished() can never become true.
    #[test]
    fn join_rip_thread_called_on_its_own_thread_returns_at_once() {
        let dev = format!("self-join-test-{}", std::process::id());
        let _ = super::take_rip_thread(&dev);
        let (tx, rx) = std::sync::mpsc::channel::<(std::time::Duration, bool, bool)>();
        let dev_inner = dev.clone();
        super::spawn_rip_thread(&dev, "rip", move || {
            let t0 = std::time::Instant::now();
            // A budget far longer than any test may block for: if the
            // self-join branch is gone this sleeps for all 30 s.
            let outcome = super::join_rip_thread(&dev_inner, std::time::Duration::from_secs(30));
            let still_registered = super::rip_thread_running(&dev_inner);
            let _ = tx.send((t0.elapsed(), outcome.is_ok(), still_registered));
        })
        .expect("spawn");

        let (elapsed, ok, still_registered) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a self-join must return immediately, not sit out its timeout");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "self-join returned after {elapsed:?} — it must not poll its own thread"
        );
        assert!(ok, "a self-join is not a drain failure");
        assert!(
            still_registered,
            "a self-join must leave the handle registered — it is what keeps the \
             device unclaimable for the rest of the worker's tail"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while super::rip_thread_running(&dev) {
            assert!(std::time::Instant::now() < deadline, "worker should exit");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = super::join_rip_thread(&dev, std::time::Duration::from_secs(5));
    }
}
