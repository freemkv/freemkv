//! Rip orchestrator — drive poll loop + scan/rip/eject entry points.
//!
//! State types, thread/halt bookkeeping, and staging-dir helpers live in
//! sibling sub-modules (`state`, `session`, `staging`). The high-level
//! orchestration — `drive_poll_loop`, `scan_disc`, `rip_disc`,
//! `eject_drive` — stays here. The `mux` sub-module holds the active
//! parallel mux "highway" (consumer/producer split + watchdog); the
//! multipass recovery runs in the engine (`freemkv_engine::run_with`), driven
//! through `passes::ServerPassHost`.

pub(crate) mod mux;
mod passes;
pub mod resume;
mod session;
pub mod staging;
pub mod state;

// Re-export every symbol the crate/tests address as `crate::server::ripper::*`.
// `#[allow(unused_imports)]` stays: the binary build doesn't use every
// re-export, but `lib.rs` and `tests/` do.
#[allow(unused_imports)]
pub use mux::{bounded_call, watchdog_bump_restart_count};
#[allow(unused_imports)]
pub use session::{
    RegisterError, device_halt, join_all_rip_threads, join_rip_thread, register_halt,
    register_rip_thread, release_stopped_drive, rollback_failed_spawn, spawn_rip_thread,
    stop_and_drain, swap_halt_carrying_cancel, take_rip_thread, unregister_halt,
};
#[allow(unused_imports)]
pub use state::{
    BadRange, Resumable, RipState, STATE, device_known, hold_stopped_disc, is_busy,
    release_stopped_disc, set_stop_cooldown, set_title_override, take_title_override,
    try_claim_active, try_claim_active_checked, update_state, update_state_with,
};

// Internal-use imports for the orchestrator code that lives in this
// file. Sub-module-private helpers (`pub(super)`) are reachable from
// here because we are the parent of `state` / `session` / `staging`.

use crate::server::util::{BYTES_PER_GIB, MILLIS_PER_SEC};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::server::config::Config;

// Live-drive structure scan options: lookup-free, plus AACS host credentials for the
// handshake. With capture_without_keys an unreadable AACS key file does not stop the
// scan: the disc is captured raw (libfreemkv records E7031 and refuses keys).
pub(crate) fn scan_opts_for(cfg: &Config) -> libfreemkv::ScanOptions {
    libfreemkv::ScanOptions {
        raw_copy: cfg.capture_without_keys,
        ..crate::server::keysource::drive_scan_opts(cfg)
    }
}

// Scan-phase watchdog: emits a WARN every 15s while structure scan / key resolve are in flight,
// so a wedged drive is visible instead of leaving the UI stuck silently.
struct ScanWatchdog {
    active: Arc<AtomicBool>,
    // Coarse phase marker the watcher reports: 0 = scan, 1 = resolve_keys.
    phase: Arc<std::sync::atomic::AtomicU8>,
}

impl ScanWatchdog {
    fn arm(device: &str) -> Self {
        let active = Arc::new(AtomicBool::new(true));
        let phase = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let active_w = active.clone();
        let phase_w = phase.clone();
        let device = device.to_string();
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut warned = false;
            // A due deadline, not `elapsed % 15`: sleep overshoot can skip an exact multiple.
            let mut next_warn = 15;
            while active_w.load(Ordering::Relaxed) {
                // Poll in short slices so the guard drop is observed
                // promptly, but only WARN on 15s boundaries.
                std::thread::sleep(Duration::from_secs(1));
                if !active_w.load(Ordering::Relaxed) {
                    break;
                }
                let elapsed = start.elapsed().as_secs();
                if elapsed >= next_warn {
                    next_warn = (elapsed / 15 + 1) * 15;
                    let last_phase = match phase_w.load(Ordering::Relaxed) {
                        0 => "scan",
                        _ => "resolve_keys",
                    };
                    tracing::warn!(
                        device = %device,
                        elapsed_secs = elapsed,
                        last_phase,
                        "scan still running"
                    );
                    crate::server::log::device_log(
                        &device,
                        &format!(
                            "Still scanning ({}s elapsed, phase={})...",
                            elapsed, last_phase
                        ),
                    );
                    warned = true;
                }
            }
            if warned {
                tracing::info!(
                    device = %device,
                    elapsed_secs = start.elapsed().as_secs(),
                    "scan watchdog stood down (scan/resolve returned)"
                );
            }
        });
        Self { active, phase }
    }

    /// Mark that the key-resolve phase has begun, so the WARN reports it.
    fn enter_resolve(&self) {
        self.phase.store(1, Ordering::Relaxed);
    }
}

impl Drop for ScanWatchdog {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
    }
}

/// Whether the disc's main feature (first title) carries an MVC dependent
/// (right-eye) view — i.e. a Blu-ray 3D rip. Drives the `.mk3d` output
/// extension so media servers/players recognise the file as stereoscopic 3D.
pub(crate) fn disc_is_3d(disc: &libfreemkv::Disc) -> bool {
    disc.titles.first().is_some_and(|t| {
        t.streams
            .iter()
            .any(|s| matches!(s, libfreemkv::Stream::Video(v) if v.is_mvc_dependent()))
    })
}

// True when a mux-construction `io::Error` is a user Stop (E6010) vs a structural failure.
pub(crate) fn is_halt_error(e: &std::io::Error) -> bool {
    freemkv_engine::error_code(e) == Some(libfreemkv::error::E_HALTED)
}

// A staged image's up-front key refusal (KU §3.2): no key (E7022/E7032), FMTS forensic keys
// (E7026), a key source failed (E7028–30), or only the disc's VID can finish (E7034).
pub(crate) fn is_key_refusal(e: &libfreemkv::Error) -> bool {
    KEY_REFUSAL_CODES.contains(&e.code())
}

const KEY_REFUSAL_CODES: [u16; 7] = {
    use libfreemkv::error as c;
    [
        c::E_NO_DISC_KEY,
        c::E_WHOLE_DISC_KEY_MISSING,
        c::E_FMTS_KEY_MISSING,
        c::E_KEY_SERVICE_UNAVAILABLE,
        c::E_KEY_SERVICE_UNAUTHORIZED,
        c::E_KEY_SERVICE_RATE_LIMITED,
        c::E_AACS_VID_NEEDS_DISC,
    ]
};

// The code of a key refusal (see `is_key_refusal`) carried through an `io::Error`, e.g. a
// mux's `TitleDone(Err)`; `None` for anything else.
pub(crate) fn io_key_refusal(e: &std::io::Error) -> Option<u16> {
    freemkv_engine::error_code(e).filter(|code| KEY_REFUSAL_CODES.contains(code))
}

// True when a mux-construction `io::Error` is a missing-FMTS-forensic-key error (E7026): base
// AACS keys resolved but online forensic keys did not.
pub(crate) fn is_fmts_key_missing_error(e: &std::io::Error) -> bool {
    freemkv_engine::error_code(e) == Some(libfreemkv::error::E_FMTS_KEY_MISSING)
}

// What the rip muxes from: the staged ISO (multipass, opened through the
// engine at mux time) or the live drive (single-pass).
enum MuxSource {
    StagedImage,
    Drive(Box<dyn libfreemkv::SectorSource>),
}

// The staged image's index for the drive-scanned `title`: the same playlist,
// else the first title (the drive rip's pick is always its first title).
fn image_title_index(image: &libfreemkv::Disc, title: &libfreemkv::DiscTitle) -> usize {
    image
        .titles
        .iter()
        .position(|t| !title.playlist.is_empty() && t.playlist == title.playlist)
        .unwrap_or(0)
}

// Output file extension for a rip of `disc`: `mk3d` for a 3D main
// feature (RFC 9559 §27.18.3), `m2ts` for TS passthrough, else `mkv`.
// `.mk3d` is byte-identical Matroska; only the extension differs.
pub(crate) fn output_extension_for(output_format: &str, disc: &libfreemkv::Disc) -> &'static str {
    match output_format {
        "m2ts" => "m2ts",
        _ if disc_is_3d(disc) => "mk3d",
        _ => "mkv",
    }
}

// libfreemkv output URL scheme for a rip (`mkv`/`m2ts`); distinct from
// output_extension_for since libfreemkv has no `mk3d://` scheme — using
// the `mk3d` extension as scheme fails the mux with StreamUrlInvalid.
pub(crate) fn output_scheme_for(output_format: &str) -> &'static str {
    match output_format {
        "m2ts" => "m2ts",
        _ => "mkv",
    }
}

// The rip's key set, or why its resolve refused (KU §2.1). Memory only.
type KeyResult = Result<libfreemkv::keys::KeyRing, libfreemkv::Error>;

// Resolve the rip's key set off the live drive, once (see `keysource::resolve_drive_keys`).
fn resolve_rip_keys(
    device: &str,
    cfg: &Config,
    drive: &mut libfreemkv::Drive,
    disc: &libfreemkv::Disc,
    scope: &libfreemkv::keys::KeyScope,
    seed: Option<&libfreemkv::keys::KeyRing>,
) -> KeyResult {
    let halt = device_halt(device);
    crate::server::keysource::resolve_drive_keys(
        cfg,
        disc,
        drive,
        scope.clone(),
        seed,
        halt.as_ref(),
    )
}

// What a rip of `disc` decrypts (KU §2.5): the whole disc for an ISO output (delivered
// decrypted, as the CLI and GUI deliver one), else title 0 (the rip's feature) and every
// episode a TV plan fans out to.
fn rip_key_scope(
    disc: &libfreemkv::Disc,
    cfg: &Config,
    media_type: &str,
    disc_name: &str,
) -> libfreemkv::keys::KeyScope {
    if output_is_iso_image(&cfg.output_format) {
        return libfreemkv::keys::KeyScope::WholeDisc;
    }
    if disc.titles.is_empty() {
        return libfreemkv::keys::KeyScope::None;
    }
    let mut titles = fanout_episode_indices(&disc.titles, cfg, media_type, disc_name);
    titles.push(0);
    titles.sort_unstable();
    titles.dedup();
    libfreemkv::keys::KeyScope::Titles(titles)
}

/// The engine plan for a server rip of the drive at `device_path` writing `dest` (an
/// output URL): the server's half of the one plan parser every front end shares. The
/// server rips the main feature, decrypts (it has no raw option), and recovers over passes
/// when `max_retries` asks for them.
pub fn server_plan(cfg: &Config, device_path: &str, dest: &str) -> freemkv_engine::Plan {
    crate::plan_core::plan(crate::plan_core::PlanRequest {
        source: format!("disc://{device_path}"),
        dest: dest.to_string(),
        titles: freemkv_engine::Selection::MainMovie,
        streams: freemkv_engine::StreamChoice::default(),
        raw: false,
        multipass: uses_multipass(cfg.max_retries),
        keys: crate::server::keysource::key_settings(cfg),
        force: false,
    })
}

/// The server's log line for the plan a rip runs. Exhaustive on purpose (anti-drift §2): a
/// field added to the engine's `Plan` fails to compile here until the server handles it.
pub fn plan_line(p: &freemkv_engine::Plan) -> String {
    let freemkv_engine::Plan {
        source,
        dest,
        titles,
        streams,
        raw,
        multipass,
        keys,
        force,
    } = p;
    let freemkv_engine::KeyParamsData {
        keydb_path,
        key_url,
        key_auth,
        online_only,
        cert_keydb,
    } = keys;
    format!(
        "plan: {source} -> {dest} titles={titles:?} streams_all={} raw={raw} multipass={multipass} \
         force={force} keydb={} online={} auth={} online_only={online_only} certs={}",
        streams.is_all(),
        keydb_path.is_some(),
        key_url.is_some(),
        key_auth.is_some(),
        cert_keydb.is_some(),
    )
}

// Whether the rip's set covers `scope` for `disc`, forensic keys aside (a multipass rip asks
// for Pending ones once, from its image). `covers` holds for any non-AACS set, so an AACS
// disc's titles also need an AACS set.
fn keys_cover(
    disc: &libfreemkv::Disc,
    set: &libfreemkv::keys::KeyRing,
    scope: &libfreemkv::keys::KeyScope,
) -> bool {
    let aacs_titles = disc.aacs.is_some() && *scope != libfreemkv::keys::KeyScope::None;
    set.is_for(&disc.media_id()) && set.covers(scope) && (set.is_aacs() || !aacs_titles)
}

// Whether the rip can decrypt what it produces: no scope needs no key; otherwise the set's
// status (CSS and clear discs as the library reads them).
fn rip_keyed(
    disc: &libfreemkv::Disc,
    scope: &libfreemkv::keys::KeyScope,
    keys: &KeyResult,
) -> bool {
    use libfreemkv::keys::DecryptStatus as S;
    if *scope == libfreemkv::keys::KeyScope::None || !disc.encrypted {
        return true;
    }
    keys.as_ref().is_ok_and(|set| {
        matches!(
            freemkv_engine::keys::key_status(disc, set),
            S::Ready | S::NotEncrypted | S::ForensicPending
        )
    })
}

// Human-readable key readiness for the dashboard tile: "Ready to rip", "Capture without keys —
// …", or "Missing keys — <reason>". The tile keys its action button off the "Missing keys"
// prefix. `error` is why the rip's resolve refused, if it did.
fn key_readiness(
    disc: &libfreemkv::Disc,
    keyed: bool,
    error: Option<&libfreemkv::Error>,
    capture_without_keys: bool,
    online: Option<crate::server::keysource::ServiceReachability>,
) -> String {
    if keyed {
        return "Ready to rip".to_string();
    }
    if capture_without_keys {
        return "Capture without keys — no decryption".to_string();
    }
    // What the service ACTUALLY said outranks everything below: the library funnels every
    // non-2xx into one "could not be reached" code, which reported a definitive 422 no-key as
    // an outage.
    if let Some(reach) = online {
        if let Some(status) = key_service_transient_status(reach) {
            return status;
        }
        if let Some(reason) = key_service_no_key_reason(reach) {
            return format!("Missing keys — {reason}");
        }
    }
    // Prefer the disc's own AACS-resolution error (`disc.aacs_error`) over the
    // coarse `KeyOutcome`: it's the true cause (e.g. E7025 bus key unavailable)
    // and renders via the shared `freemkv_i18n` catalog, matching the CLI.
    use libfreemkv::error as ec;
    let reason = if let Some(err) = disc.aacs_error.as_ref() {
        freemkv_i18n::error_message(u32::from(err.code()))
    } else {
        match error {
            Some(e) if matches!(e.code(), ec::E_NO_DISC_KEY | ec::E_WHOLE_DISC_KEY_MISSING) => {
                "no key source has a key for this disc".to_string()
            }
            Some(e) => strip_error_prefix(&aacs_failure_message(Some(e))).to_string(),
            // No refusal yet keyless (e.g. an encrypted disc with no readable key file).
            None => {
                let msg = keyless_failure_message(disc);
                strip_error_prefix(&msg).to_string()
            }
        }
    };
    format!("Missing keys — {reason}")
}

// What the pre-rip FMTS forensic-key gate should do, given whether the complete map resolved
// and the operator's capture setting.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum FmtsGate {
    /// The forensic keys came with the rip's set — rip normally.
    Proceed,
    /// Map incomplete but the operator opted into capture-without-keys — sweep the
    /// raw ISO now, defer the forensic mux until the keys are available.
    CaptureOnly,
    /// Map incomplete and capture-without-keys is off — do not rip (skip the disc).
    Skip,
}

/// Pure decision for the FMTS pre-rip gate (crypto/drive resolution is done by
/// the rip's up-front key set; this is just the policy).
fn fmts_gate_decision(map_resolved: bool, capture_without_keys: bool) -> FmtsGate {
    if map_resolved {
        FmtsGate::Proceed
    } else if capture_without_keys {
        FmtsGate::CaptureOnly
    } else {
        FmtsGate::Skip
    }
}

// Side-effect routing for each FMTS gate outcome, split out as a pure, unit-testable function.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct FmtsGatePlan {
    defer_forensic_mux: bool,
    quarantine: bool,
}

fn fmts_gate_plan(gate: FmtsGate) -> FmtsGatePlan {
    match gate {
        FmtsGate::Proceed => FmtsGatePlan {
            defer_forensic_mux: false,
            quarantine: false,
        },
        FmtsGate::CaptureOnly => FmtsGatePlan {
            defer_forensic_mux: true,
            quarantine: false,
        },
        FmtsGate::Skip => FmtsGatePlan {
            defer_forensic_mux: false,
            quarantine: true,
        },
    }
}

// ─── Online key service: DOWN vs genuine no-key ─────────────────────────────
// The online keysource swallows every failure into an empty result; these
// helpers classify DOWN vs genuine no-key and bounded-retry a transient outage.
const KEY_SERVICE_RETRY_ATTEMPTS: u32 = 3;

/// `key_status` / `last_error` text for a key service we never reached at all
/// (connection refused, DNS failure, timeout, TLS error). The ONLY outcome for
/// which "temporary, we'll retry" is an honest thing to say.
const KEY_SERVICE_UNREACHABLE_STATUS: &str = "Key service unreachable — autorip could not \
    connect to the online key service, so it never said anything about this disc. \
    Nothing is known yet about whether this disc has a key. This is usually temporary; \
    autorip will retry. (no reply received)";

/// `key_status` / `last_error` text for a rate-limited (quota) key service.
const KEY_SERVICE_QUOTA_STATUS: &str = "Key service busy — the online key service is \
    refusing requests for now because too many have been sent. It has not looked at this \
    disc yet. Waiting and trying again should work. (service replied HTTP 429)";

/// `key_status` / `last_error` text for HTTP 422: the service WAS reached, is
/// licensed, and definitively resolved no key for this disc after exhausting
/// every candidate source. Retrying cannot change the answer — saying "the
/// service was down, wait a few minutes" here sends the operator into an
/// endless retry on a disc that will never resolve.
const KEY_SERVICE_NO_KEY_REASON: &str = "the online key service answered and has no key \
    for this disc. It searched every source it has before answering, so trying again \
    will not change the result. To keep a copy anyway, turn on \"capture without keys\" \
    to save the disc as an encrypted image; otherwise try a different key source. \
    (service replied HTTP 422)";

/// Resume-path text for HTTP 422: the ISO is already captured, so no capture advice.
const KEY_SERVICE_NO_KEY_DEFERRED: &str = "The online key service answered and has no key \
    for this disc yet; the ISO is kept for when one becomes available, e.g. through a key \
    database update. (service replied HTTP 422)";

/// `key_status` / `last_error` text for HTTP 404: the service refused the
/// request as unlicensed / unknown, so it never looked for a key.
const KEY_SERVICE_UNLICENSED_REASON: &str = "the online key service would not accept the \
    request, so it never looked for a key for this disc. Check the Keyserver URL \
    and Keyserver API Secret in Settings — trying again without changing them will not help. \
    (service replied HTTP 404)";

/// `key_status` / `last_error` text for a key service we never asked, because
/// the configured URL is unusable (empty, wrong scheme, SSRF-blocked).
const KEY_SERVICE_NOT_ASKED_REASON: &str = "the online key service was never contacted \
    because the address configured for it cannot be used. Fix the key-service address in \
    Settings. (no request was sent)";

/// Reason text for HTTP 401 / 403: the service rejected the configured credentials.
fn key_service_unauthorized_reason(code: u16) -> String {
    format!(
        "the online key service rejected the credentials configured for it, so it never \
         looked for a key for this disc. Fix the Keyserver API Secret in Settings — \
         trying again without changing it will not help. (service replied HTTP {code})"
    )
}

/// Reason text for an unexpected non-2xx status: say plainly that we do not
/// know, and quote the status, rather than guessing at a cause.
fn key_service_unexpected_reason(code: u16) -> String {
    format!(
        "the online key service replied with something autorip does not recognise, so \
         nothing is known about this disc's key. Check the key-service address in Settings, \
         and report this if it keeps happening. (service replied HTTP {code})"
    )
}

/// Status text for a key service that failed on its own side (HTTP 5xx) — it
/// was reached, but never got as far as an answer about this disc.
fn key_service_server_error_status(code: u16) -> String {
    format!(
        "Key service error — the online key service is reachable but is failing on its own \
         side, so it never said anything about this disc. This is usually temporary; autorip \
         will retry. (service replied HTTP {code})"
    )
}

// Should the rip re-attempt online key resolution before proceeding? Fires only for an
// ENCRYPTED disc the online key service left with NO keys, with capture-without-keys off.
fn should_retry_online_keys(
    uses_online: bool,
    capture_without_keys: bool,
    encrypted: bool,
    keys_missing: bool,
) -> bool {
    uses_online && !capture_without_keys && encrypted && keys_missing
}

/// Backoff before the Nth (1-based) online-key retry: 8s, 16s, 32s (capped).
fn key_service_backoff(attempt: u32) -> std::time::Duration {
    let shift = attempt.saturating_sub(1).min(3);
    std::time::Duration::from_secs(8u64.saturating_mul(1u64 << shift))
}

/// Map a TRANSIENT reachability verdict — one where the service never gave a
/// verdict about this disc — to its own standalone operator-facing status line.
/// `None` for every verdict that IS an answer about this disc (including the
/// definitive 422 no-key): those are not outages and must not borrow outage
/// wording. See [`key_service_no_key_reason`] for their text.
fn key_service_transient_status(
    reach: crate::server::keysource::ServiceReachability,
) -> Option<String> {
    use crate::server::keysource::ServiceReachability;
    match reach {
        ServiceReachability::Unreachable => Some(KEY_SERVICE_UNREACHABLE_STATUS.to_string()),
        ServiceReachability::ServerError(code) => Some(key_service_server_error_status(code)),
        ServiceReachability::RateLimited => Some(KEY_SERVICE_QUOTA_STATUS.to_string()),
        ServiceReachability::Answered
        | ServiceReachability::NoKeyForDisc
        | ServiceReachability::NotLicensed
        | ServiceReachability::Unauthorized(_)
        | ServiceReachability::Unexpected(_)
        | ServiceReachability::NotAsked => None,
    }
}

/// Map a TERMINAL verdict — the service delivered an answer (or was never askable) — to the
/// reason clause shown after the "Missing keys — " prefix. `None` for an ordinary 2xx no-key
/// (keep the generic text) and for the transient verdicts, which get
/// [`key_service_transient_status`] instead.
fn key_service_no_key_reason(
    reach: crate::server::keysource::ServiceReachability,
) -> Option<String> {
    use crate::server::keysource::ServiceReachability;
    match reach {
        ServiceReachability::NoKeyForDisc => Some(KEY_SERVICE_NO_KEY_REASON.to_string()),
        ServiceReachability::NotLicensed => Some(KEY_SERVICE_UNLICENSED_REASON.to_string()),
        ServiceReachability::Unauthorized(code) => Some(key_service_unauthorized_reason(code)),
        ServiceReachability::Unexpected(code) => Some(key_service_unexpected_reason(code)),
        ServiceReachability::NotAsked => Some(KEY_SERVICE_NOT_ASKED_REASON.to_string()),
        ServiceReachability::Answered
        | ServiceReachability::Unreachable
        | ServiceReachability::ServerError(_)
        | ServiceReachability::RateLimited => None,
    }
}

// Record a TERMINAL key-service verdict structurally, status code and all —
// the machine-greppable copy of what the user-facing string carries in its
// trailing parenthetical.
fn log_terminal_key_verdict(reach: crate::server::keysource::ServiceReachability) {
    tracing::info!(
        phase = "key_resolve",
        verdict = ?reach,
        http_status = reach.http_status(),
        retryable = false,
        "online key service delivered a definitive verdict for this disc — not retrying"
    );
}

// Classify the key service after a refused online resolution and bounded-retry a transient
// outage. The verdict is `Some` whenever the rip is still keyless.
fn retry_online_keys_on_outage(
    device: &str,
    cfg: &Config,
    drive: &mut libfreemkv::Drive,
    (disc, scope): (&libfreemkv::Disc, &libfreemkv::keys::KeyScope),
    refused: libfreemkv::Error,
    decode_reach: Option<crate::server::keysource::ServiceReachability>,
) -> (
    KeyResult,
    Option<crate::server::keysource::ServiceReachability>,
) {
    // Classify from the REAL decode's HTTP outcome — no second empty probe (its
    // 0-byte POST to the POST-only `/decode` logged a spurious `404` after every
    // real no-key). Probe only when the decode made no HTTP answer (`None`).
    let reach =
        decode_reach.unwrap_or_else(|| crate::server::keysource::probe_online_reachability(cfg));
    if !reach.is_transient() {
        // The service ANSWERED about this disc (or could never be asked). No
        // retry — a 422 "no key for this disc" took the server ~30s of
        // exhausting every candidate source; repeating it changes nothing.
        log_terminal_key_verdict(reach);
        return (Err(refused), Some(reach));
    }
    crate::server::log::device_log(
        device,
        "Online key service appears DOWN (not a missing key) — retrying key resolution.",
    );
    let mut last = refused;
    let mut last_reach = reach;
    for attempt in 1..=KEY_SERVICE_RETRY_ATTEMPTS {
        if crate::server::SHUTDOWN.load(Ordering::Relaxed) {
            break;
        }
        let backoff = key_service_backoff(attempt);
        crate::server::log::device_log(
            device,
            &format!(
                "Key-service retry {attempt}/{KEY_SERVICE_RETRY_ATTEMPTS} in {}s...",
                backoff.as_secs()
            ),
        );
        if !wait_unless_stopped(device, backoff) {
            crate::server::log::device_log(device, "Stopped during the key-service retry wait.");
            return (Err(last), Some(last_reach));
        }
        // Re-attempt the full resolution — the real retry against the service.
        let result = resolve_rip_keys(device, cfg, drive, disc, scope, None);
        // Consume THIS retry's decode outcome immediately (before the next loop
        // overwrites it), so the re-classify below reads the real POST.
        let retry_reach = crate::server::keysource::take_online_decode_reachability();
        match result {
            Ok(set) => {
                crate::server::log::device_log(
                    device,
                    "Key service recovered — keys resolved on retry.",
                );
                return (Ok(set), None);
            }
            Err(e) => last = e,
        }
        // Still no key — is the service back (genuine no-key now) or still down?
        last_reach =
            retry_reach.unwrap_or_else(|| crate::server::keysource::probe_online_reachability(cfg));
        if !last_reach.is_transient() {
            crate::server::log::device_log(
                device,
                "Key service answered but has no key — genuine missing key for this disc.",
            );
            log_terminal_key_verdict(last_reach);
            return (Err(last), Some(last_reach));
        }
    }
    crate::server::log::device_log(
        device,
        "Key service still unavailable after retries — leaving disc in a retryable state \
         (a later insert / rescan will pick it up). Not ejecting.",
    );
    (Err(last), Some(last_reach))
}

// Sleep `backoff` on the device's Halt; `false` once its Stop lands (plain sleep with none).
fn wait_unless_stopped(device: &str, backoff: Duration) -> bool {
    match device_halt(device) {
        Some(halt) => session::sleep_unless_halted(&halt, backoff),
        None => {
            std::thread::sleep(backoff);
            true
        }
    }
}

// Verdict rip_disc seeds the outage classifier with: this rip's fresh decode verdict, else
// the one scan_disc banked on the reused session. `None` makes the classifier probe.
fn rip_seed_verdict(
    fresh_decode: Option<crate::server::keysource::ServiceReachability>,
    scanned: Option<crate::server::keysource::ServiceReachability>,
) -> Option<crate::server::keysource::ServiceReachability> {
    fresh_decode.or(scanned)
}

// Does the rip need one fresh resolve before classifying? Only for a config-class verdict
// BANKED by the scan (Settings may have changed since); never after rip_disc's own resolve.
fn seed_needs_reresolve(
    fresh_decode: Option<crate::server::keysource::ServiceReachability>,
    banked: Option<crate::server::keysource::ServiceReachability>,
) -> bool {
    use crate::server::keysource::ServiceReachability as R;
    fresh_decode.is_none()
        && matches!(
            banked,
            Some(R::Unauthorized(_) | R::NotAsked | R::NotLicensed | R::Unexpected(_))
        )
}

// `last_error` for a keyless disc not ripped with capture-without-keys off.
// `msg` may already carry the "No keys — " lead; drop it rather than print it twice.
fn keyless_not_ripping_error(msg: &str) -> String {
    match msg.strip_prefix("No keys — ") {
        Some(reason) => format!("No keys — not ripping: {reason}"),
        None => format!("No keys — not ripping. {msg}"),
    }
}

use session::{
    DriveSession, drop_session, rip_thread_running, session_is_scanned, store_session, take_session,
};
use staging::staging_free_bytes;
use state::{PassContext, PassProgressState, is_in_cooldown, push_pass_state};

// ─── Poll loop ─────────────────────────────────────────────────────────────

const POLL_INTERVAL_SECS: u64 = 5;

// Extract the trailing path component (`sg4` from `/dev/sg4`, `disk2`
// from `/dev/disk2`, `CdRom0` from `\\.\CdRom0`) for use as a device
// key: autorip's state map keys by this short name, not the full path.
fn device_key(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

// Tear down per-device state for a drive that vanished from the enumeration (hot-unplug);
// deferred while its worker is still live.
fn forget_removed_device(device: &str) -> bool {
    // TOCTOU fix: re-check liveness and drop the STATE row in ONE critical
    // section (the old split check→remove let a rip dispatched in the gap lose its
    // live row). `rip_thread_running` locks RIP_THREADS not STATE, so this is safe.
    {
        let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if s.get(device).is_some_and(state::row_is_busy) || rip_thread_running(device) {
            tracing::warn!(
                device = %device,
                "drive vanished from enumeration while a worker still holds it — \
                 deferring teardown to preserve the double-rip guard"
            );
            return false;
        }
        s.remove(device);
    }
    drop_session(device);
    // No eject/scan boundary fires here, so the device's in-memory log
    // ring would otherwise linger for the container's lifetime. Evict it
    // like archive_device_log does on the planned-eject path.
    crate::server::log::forget_device(device);
    // Evict the remaining per-device maps so nothing accumulates as device
    // paths churn; `forget_device_state`'s doc has the authoritative inventory.
    state::forget_device_state(device);
    true
}

/// How the poll loop reacts to a failed disc probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeFailure {
    Pending,
    /// Absent from the enumeration: unplugged, left for the next rescan.
    HotUnplug,
    /// Newly found enumerated but not answering: warn and surface the wedge in the UI.
    NewWedge,
    /// Already reported wedged: stay quiet until it answers or is removed.
    KnownWedge,
}

// Probe-failure bookkeeping for the poll loop. A wedge is remembered until the drive answers or
// is removed, an unplug suspect until the next rescan; neither is re-enumerated meanwhile, so a
// failing drive can't force `list_drives` (INQUIRY to every drive, busy ones too) every tick.
#[derive(Default)]
struct ProbeFailTracker {
    failures: std::collections::HashMap<String, u8>,
    wedged: std::collections::HashSet<String>,
    unplug_suspect: std::collections::HashSet<String>,
    /// This tick's latest enumeration, always taken before the probe being classified.
    tick_enum: Option<Vec<String>>,
}

impl ProbeFailTracker {
    fn begin_tick(&mut self) {
        self.tick_enum = None;
    }

    fn on_rescan(&mut self, fresh: Vec<String>) {
        self.unplug_suspect.clear();
        self.tick_enum = Some(fresh);
    }

    /// The drive answered, or it was torn down: forget any failure recorded for it.
    fn clear(&mut self, device: &str) {
        self.failures.remove(device);
        self.wedged.remove(device);
        self.unplug_suspect.remove(device);
    }

    fn on_probe_err(
        &mut self,
        device: &str,
        path: &str,
        enumerate: impl FnOnce() -> Vec<String>,
    ) -> ProbeFailure {
        // Three consecutive failures span at least two poll intervals (10 seconds).
        let failures = self.failures.entry(device.to_string()).or_default();
        *failures = failures.saturating_add(1);
        if *failures < 3 {
            return ProbeFailure::Pending;
        }
        self.classify_probe_err(device, path, enumerate)
    }

    fn classify_probe_err(
        &mut self,
        device: &str,
        path: &str,
        enumerate: impl FnOnce() -> Vec<String>,
    ) -> ProbeFailure {
        if self.wedged.contains(device) {
            return ProbeFailure::KnownWedge;
        }
        if self.unplug_suspect.contains(device) {
            return ProbeFailure::HotUnplug;
        }
        // An older snapshot proves absence, but only one taken after this failed probe may
        // declare a wedge: the drive could have been unplugged in between.
        let listed = |snap: &Vec<String>| snap.iter().any(|p| p == path);
        let present = match self.tick_enum.as_ref().map(listed) {
            Some(false) => false,
            _ => listed(self.tick_enum.insert(enumerate())),
        };
        if present {
            self.wedged.insert(device.to_string());
            ProbeFailure::NewWedge
        } else {
            self.unplug_suspect.insert(device.to_string());
            ProbeFailure::HotUnplug
        }
    }
}

/// What one poll tick does about a disc it can see in a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InsertTick {
    /// Run the auto-scan / auto-rip trigger for this disc now.
    dispatch: bool,
    /// Carry this device into the next tick's "already seen" set.
    latch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoResumeAction {
    Fresh,
    Sweep,
    Remux,
}

fn auto_resume_action(resumable: Option<Resumable>) -> AutoResumeAction {
    match resumable {
        Some(Resumable::Sweep) => AutoResumeAction::Sweep,
        Some(Resumable::Remux) => AutoResumeAction::Remux,
        None => AutoResumeAction::Fresh,
    }
}

fn auto_insert_rip_mode(on_insert: &str) -> Option<crate::server::web::ResumeMode> {
    match on_insert {
        "rip" => Some(crate::server::web::ResumeMode::Fresh),
        "resume" => Some(crate::server::web::ResumeMode::Prefer),
        _ => None,
    }
}

// Fresh unattended rip (1.7.7): discard this disc's staging, whatever it holds, then sweep.
// Only staging the mux worker owns, another drive sweeps, or that can't be read stands down.
fn auto_rip_fresh(cfg: &Arc<RwLock<Config>>, device: &str, device_path: &str) {
    // Held from before the wipe until rip_disc returns: the same disc in a second drive
    // stands down instead of wiping this rip's dir before its `.sweeping` lands.
    let cfg_read = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    let staging_dir = staging_basename_for_device(&cfg_read, device)
        .map(|b| std::path::Path::new(&cfg_read.staging_dir).join(b));
    let _claim = match staging_dir {
        Some(dir) => match claim_fresh_rip(&dir.to_string_lossy(), device) {
            Some(claim) => Some(claim),
            None => {
                crate::server::log::device_log(
                    device,
                    "Another drive is ripping this disc right now — NOT re-ripping.",
                );
                stand_down_idle(device, false);
                return;
            }
        },
        None => None,
    };
    if staging_hold_stands_down(cfg, device, GuardFor::Insert) {
        return;
    }
    wipe_staging_for_disc(cfg, device);
    rip_disc(cfg, device, device_path, false);
}

// Staging dirs a fresh rip owns (dir → device), from its wipe until it returns.
static FRESH_RIP_CLAIMS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(Default::default);

struct FreshRipClaim {
    dir: String,
}

impl Drop for FreshRipClaim {
    fn drop(&mut self) {
        FRESH_RIP_CLAIMS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.dir);
    }
}

// Claim staging `dir` for a fresh rip on `device`; `None` while another device holds it.
fn claim_fresh_rip(dir: &str, device: &str) -> Option<FreshRipClaim> {
    let mut claims = FRESH_RIP_CLAIMS.lock().unwrap_or_else(|e| e.into_inner());
    if claims.get(dir).is_some_and(|owner| owner != device) {
        return None;
    }
    claims.insert(dir.to_string(), device.to_string());
    Some(FreshRipClaim {
        dir: dir.to_string(),
    })
}

// Decide both halves of a tick's response to an observed disc; the two answers must agree.
// `dispatch` is suppressed during the post-Stop cooldown.
fn insert_tick(is_new_insert: bool, in_cooldown: bool) -> InsertTick {
    let dispatch = is_new_insert && !in_cooldown;
    InsertTick {
        dispatch,
        // Latch what this tick dispatched, plus anything already latched (a
        // resident disc, which must keep NOT re-triggering). The only case
        // left unlatched is the one the cooldown deferred.
        latch: dispatch || !is_new_insert,
    }
}

/// A poll tick's reaction to one drive's presence answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollAction {
    /// No disc: drop a known session (`removed`) and show the drive idle.
    Absent { removed: bool },
    /// Presence not settled: neither an insert nor a removal.
    Hold { latch: bool },
    /// A disc is loaded.
    Present(InsertTick),
}

impl PollAction {
    /// Carry this device into the next tick's "already seen" set.
    fn latch(self) -> bool {
        match self {
            PollAction::Absent { .. } => false,
            PollAction::Hold { latch } => latch,
            PollAction::Present(t) => t.latch,
        }
    }

    /// Show the drive idle this tick, replacing any stale tile (e.g. a cleared wedge).
    fn shows_idle(self) -> bool {
        matches!(
            self,
            PollAction::Absent { .. } | PollAction::Hold { latch: false }
        )
    }
}

fn poll_action(
    presence: libfreemkv::DiscPresence,
    had_disc: bool,
    in_cooldown: bool,
) -> PollAction {
    match presence {
        libfreemkv::DiscPresence::Present => {
            PollAction::Present(insert_tick(!had_disc, in_cooldown))
        }
        libfreemkv::DiscPresence::Absent => PollAction::Absent { removed: had_disc },
        // Settling (a disc re-spinning, or a tray closing empty): keep whatever we had.
        _ => PollAction::Hold { latch: had_disc },
    }
}

// Replace a device's row with the poll loop's view unless a worker has claimed it, checked
// and written under one STATE lock (a claim landing in between must not be clobbered).
fn publish_poll_row(device: &str, row: RipState) -> bool {
    let mut published = false;
    update_state_with(device, |cur| {
        if state::row_is_busy(cur) {
            return;
        }
        let claim_gen = cur.claim_gen;
        *cur = RipState { claim_gen, ..row };
        published = true;
    });
    published
}

// Clear only the poller's warning, leaving worker errors and active jobs intact.
fn clear_probe_error(row: &mut RipState, presence: libfreemkv::DiscPresence) {
    if row.status == "error"
        && row
            .last_error
            .starts_with("Drive communication failed repeatedly (")
    {
        row.last_error.clear();
        row.status = "idle".to_string();
        match presence {
            libfreemkv::DiscPresence::Present => row.disc_present = true,
            libfreemkv::DiscPresence::Absent => row.disc_present = false,
            _ => {}
        }
    }
}

/// Poll drives for disc insertion. Only triggers on state change
/// (no disc → disc present), not on disc already being there.
///
/// autorip never touches hardware paths, sysfs, SCSI, or USB directly; the lib's
/// `list_drives()` / `disc_presence(path)` do the platform enumeration and disc-presence probe
/// (no internal recovery). autorip just iterates the snapshot, tracks logical state
/// (idle/scanning/ripping/cooldown), and spawns rip threads.
pub fn drive_poll_loop(cfg: &Arc<RwLock<Config>>) {
    // Re-enumerate drives every RESCAN_INTERVAL_SECS so a USB unplug+replug
    // (which may rename the device node) is detected without a container restart.
    const RESCAN_INTERVAL_SECS: u64 = 30;
    // Startup staging scan: quarantine terminally-failed dirs, preserve
    // resumable ones. Resume is recomputed on demand via find_resumable_for_disc.
    {
        let c = cfg.read().unwrap_or_else(|e| e.into_inner());
        let hints = staging::resume_or_quarantine_staging(&c.staging_dir);
        tracing::info!(
            staging_dir = %c.staging_dir,
            entries = hints.len(),
            "staging resume scan complete"
        );
        for hint in &hints {
            // Classify for the log only (resume itself is recomputed on
            // demand via find_resumable_for_disc); no map is retained.
            let class = resume::classify_resume(
                hint,
                effective_abort_secs(&c.output_format, c.abort_on_lost_secs),
            );
            tracing::info!(
                dir = %hint.dir.display(),
                action = ?hint.action,
                classification = ?class,
                "staging resume hint"
            );
        }
    }

    let initial_drives = libfreemkv::list_drives();
    let mut drive_paths: Vec<String> = initial_drives.iter().map(|d| d.path.clone()).collect();
    for d in &initial_drives {
        tracing::info!(
            device = %device_key(&d.path),
            path = %d.path,
            vendor = %d.vendor,
            model = %d.model,
            firmware = %d.firmware,
            "drive enumerated"
        );
    }
    let mut last_rescan = std::time::Instant::now();

    let mut had_disc: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut probe_fail = ProbeFailTracker::default();
    let mut device_first_seen: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    for d in &initial_drives {
        let key = device_key(&d.path);
        device_first_seen.insert(
            key,
            std::time::Instant::now() - std::time::Duration::from_secs(60),
        );
    }

    tracing::info!(
        interval_secs = POLL_INTERVAL_SECS,
        drive_count = drive_paths.len(),
        "drive poll loop starting"
    );

    while !crate::server::SHUTDOWN.load(Ordering::Relaxed) {
        probe_fail.begin_tick();
        // Periodic hot-plug reconcile: re-enumerate drives and diff against
        // the cached path list. New devices start being polled; removed devices
        // have their session cleared so the UI doesn't show a phantom drive.
        if last_rescan.elapsed().as_secs() >= RESCAN_INTERVAL_SECS {
            last_rescan = std::time::Instant::now();
            let fresh = libfreemkv::list_drives();
            let fresh_paths: Vec<String> = fresh.iter().map(|d| d.path.clone()).collect();
            probe_fail.on_rescan(fresh_paths.clone());
            // Added: in fresh but not in drive_paths.
            for d in &fresh {
                if !drive_paths.contains(&d.path) {
                    let key = device_key(&d.path);
                    device_first_seen
                        .entry(key)
                        .or_insert(std::time::Instant::now());
                    tracing::info!(
                        device = %device_key(&d.path),
                        path = %d.path,
                        vendor = %d.vendor,
                        model = %d.model,
                        firmware = %d.firmware,
                        "drive enumerated (hot-plug)"
                    );
                }
            }
            // Removed: in drive_paths but not in fresh_paths. A busy drive's
            // teardown is deferred by forget_removed_device, so its path is
            // carried into the new drive_paths for the next rescan to retry.
            let mut deferred_removals: Vec<String> = Vec::new();
            for path in &drive_paths {
                if !fresh_paths.contains(path) {
                    let device = device_key(path);
                    tracing::info!(device = %device, path = %path, "drive removed (hot-unplug)");
                    if !forget_removed_device(&device) {
                        deferred_removals.push(path.clone());
                        continue;
                    }
                    had_disc.remove(&device);
                    probe_fail.clear(&device);
                    device_first_seen.remove(&device);
                }
            }
            drive_paths = fresh_paths;
            drive_paths.extend(deferred_removals);
        }

        {
            let mut current_with_disc: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for path in &drive_paths {
                let device = device_key(path);

                // Don't probe drives a worker still holds. `rip_thread_running`
                // covers eject_drive's tail that `is_busy` alone misses, where
                // probing would overwrite a terminal STATE row with a bogus error.
                if is_busy(&device) || rip_thread_running(&device) {
                    current_with_disc.insert(device.clone());
                    continue;
                }

                if device_first_seen
                    .get(&device)
                    .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60))
                {
                    continue;
                }

                // One TUR per drive; `Err` is an unresponsive or unplugged drive,
                // `Settling` (spin-up, tray closing) changes nothing this tick.
                let probe = match session::held_drive_presence(&device) {
                    Some(p) => Ok(p),
                    None => libfreemkv::disc_presence(std::path::Path::new(path)),
                };
                let presence = match probe {
                    Ok(p) => {
                        probe_fail.clear(&device);
                        let p = state::stopped_disc_presence(&device, p);
                        update_state_with(&device, |row| clear_probe_error(row, p));
                        p
                    }
                    Err(e) => {
                        // Presence unknown, like Settling: keep the latch so a resident disc
                        // is not re-dispatched as a fresh insert once the drive answers.
                        if had_disc.contains(&device) {
                            current_with_disc.insert(device.clone());
                        }
                        // A drive gone from the enumeration is unplugged, not wedged: skip
                        // the tile and let the next rescan clean up.
                        let enumerate = || {
                            libfreemkv::list_drives()
                                .into_iter()
                                .map(|d| d.path)
                                .collect()
                        };
                        let class = probe_fail.on_probe_err(&device, path, enumerate);
                        if class == ProbeFailure::Pending {
                            continue;
                        }
                        if class == ProbeFailure::HotUnplug {
                            tracing::debug!(
                                device = %device,
                                path = %path,
                                error = %e,
                                "disc_presence failed and drive is absent from enumeration — hot-unplug, not a firmware wedge; deferring to rescan"
                            );
                            continue;
                        }
                        if class == ProbeFailure::NewWedge {
                            tracing::warn!(
                                device = %device,
                                path = %path,
                                error = %e,
                                "disc_presence repeatedly failed — drive communication error"
                            );
                            // Surface the wedge in the UI; the Ok(_) arm clears it once the
                            // drive recovers. A worker that claimed the drive since the busy
                            // check explains the failed probe: no wedge, keep its tile.
                            let shown = publish_poll_row(
                                &device,
                                RipState {
                                    device: device.clone(),
                                    status: "error".to_string(),
                                    disc_present: had_disc.contains(&device),
                                    last_error: format!(
                                        "Drive communication failed repeatedly ({}). Retrying automatically.",
                                        e
                                    ),
                                    ..Default::default()
                                },
                            );
                            if !shown {
                                probe_fail.clear(&device);
                            }
                        } else {
                            tracing::debug!(
                                device = %device,
                                error = %e,
                                "disc_presence still failing"
                            );
                        }
                        continue;
                    }
                };

                let is_new_insert = !had_disc.contains(&device);
                // One is_in_cooldown read for both halves: asking twice could
                // straddle the expiry and dispatch without latching.
                let action = poll_action(presence, !is_new_insert, is_in_cooldown(&device));
                if action.latch() {
                    current_with_disc.insert(device.clone());
                }
                if action.shows_idle() {
                    publish_poll_row(
                        &device,
                        RipState {
                            device: device.clone(),
                            status: "idle".to_string(),
                            ..Default::default()
                        },
                    );
                }
                let tick = match action {
                    PollAction::Present(tick) => tick,
                    PollAction::Hold { .. } => {
                        tracing::debug!(device = %device, "disc presence settling; no insert or removal");
                        continue;
                    }
                    PollAction::Absent { removed } => {
                        if removed {
                            tracing::info!(device = %device, "disc removed");
                            drop_session(&device);
                        }
                        continue;
                    }
                };

                if is_new_insert && tick.dispatch {
                    tracing::info!(device = %device, "disc inserted");
                } else if is_new_insert {
                    tracing::debug!(
                        device = %device,
                        "disc present during the post-stop cooldown; \
                         deferring the insert trigger to the next tick"
                    );
                }

                if tick.dispatch {
                    let on_insert = cfg
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .on_insert
                        .clone();

                    if on_insert == "nothing" {
                        update_state(
                            &device,
                            RipState {
                                device: device.clone(),
                                status: "idle".to_string(),
                                disc_present: true,
                                ..Default::default()
                            },
                        );
                        continue;
                    }

                    // Claim like /api/scan and /api/rip do (try_claim_active_checked):
                    // the old separate check+set was a TOCTOU letting two rip
                    // threads claim one drive.
                    let Some(claim_gen) = state::try_claim_insert(&device) else {
                        continue;
                    };

                    tracing::info!(
                        device = %device,
                        on_insert = %on_insert,
                        "spawning scan/rip thread"
                    );

                    // try_claim_active already set status/disc_present under
                    // the STATE lock, so no separate update_state is needed.

                    let cfg = cfg.clone();
                    let dev_path = path.clone();
                    let device_for_thread = device.clone();

                    // Allocate the rip's Halt token at spawn so /api/stop can
                    // find it via device_halt even before rip_disc starts;
                    // rip_disc and the cleanup paths unregister it on exit.
                    register_halt(&device, libfreemkv::Halt::new());

                    // Auto-rip may either force a fresh sweep or prefer
                    // resumable staging state, as selected in settings.
                    let auto_mode = auto_insert_rip_mode(&on_insert);
                    let cfg_for_thread = cfg.clone();
                    let dev_path_for_thread = dev_path.clone();
                    if let Err(e) = spawn_rip_thread(&device, "rip", move || {
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            scan_disc(&cfg, &device_for_thread, &dev_path);
                            if let Some(mode) = auto_mode {
                                let cancelled = device_halt(&device_for_thread)
                                    .map(|h| h.is_cancelled())
                                    .unwrap_or(false);
                                if !cancelled {
                                    handle_rip_request(
                                        &cfg_for_thread,
                                        &device_for_thread,
                                        &dev_path_for_thread,
                                        mode,
                                    );
                                }
                            }
                            unregister_halt(&device_for_thread);
                        }))
                        .is_err()
                        {
                            tracing::error!(
                                device = %device_for_thread,
                                "scan/rip thread panicked"
                            );
                            crate::server::log::device_log(&device_for_thread, "Thread panicked");
                            drop_session(&device_for_thread);
                            unregister_halt(&device_for_thread);
                            update_state(
                                &device_for_thread,
                                RipState {
                                    device: device_for_thread.clone(),
                                    status: "error".to_string(),
                                    last_error: "Internal error (panic)".to_string(),
                                    ..Default::default()
                                },
                            );
                        }
                    }) {
                        tracing::warn!(
                            device = %device,
                            error = %e,
                            "failed to spawn rip thread"
                        );
                        // A bare warn here would leak the Halt and wedge the
                        // device in "scanning" forever; mirror the web
                        // handlers' rollback instead.
                        rollback_failed_spawn(&device, claim_gen);
                    }
                } else if !is_new_insert && !is_busy(&device) {
                    let mut s = STATE.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(rs) = s.get_mut(&device) {
                        rs.disc_present = true;
                    }
                }
            }

            had_disc = current_with_disc;
        }

        // SHUTDOWN-responsive sleep — break early on signal so SIGTERM
        // doesn't have to wait the full 5 s tick to take effect.
        for _ in 0..(POLL_INTERVAL_SECS * 10) {
            if crate::server::SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    tracing::info!("drive poll loop stopping");
}

// ─── Scan ──────────────────────────────────────────────────────────────
// Push an "error" state after a poisoned config lock forced an early
// return; without this the tile stays wedged in "scanning" forever.
fn mark_config_lock_poisoned(device: &str, op: &str) {
    crate::server::log::device_log(device, &format!("{op} aborted: config lock poisoned"));
    update_state(
        device,
        RipState {
            device: device.to_string(),
            status: "error".to_string(),
            disc_present: true,
            last_error: "Internal error: config lock poisoned".to_string(),
            ..Default::default()
        },
    );
}

/// Scan a disc — open, init, identify, TMDB, full scan. Stores session for rip.
pub fn scan_disc(cfg: &Arc<RwLock<Config>>, device: &str, device_path: &str) {
    // Snapshot Config and drop the read guard immediately (see rip_disc):
    // scans can take 10-30s on damaged discs, long enough to block a
    // racing settings POST.
    let cfg_read = match cfg.read() {
        Ok(c) => c.clone(),
        Err(_) => {
            mark_config_lock_poisoned(device, "Scan");
            return;
        }
    };

    update_state(
        device,
        RipState {
            device: device.to_string(),
            status: "scanning".to_string(),
            disc_present: true,
            ..Default::default()
        },
    );

    crate::server::log::archive_device_log(device);
    crate::server::log::device_log(device, "Opening drive...");

    // Drive open + SCSI bring-up runs inside the engine's session open; the owned
    // drive comes back out after the scan (into_drive), so the rest of
    // scan_disc is untouched.
    crate::server::log::device_log(device, "Initializing...");
    let mut session = match freemkv_engine::drive::open_session(
        libfreemkv::DeviceTarget::Path(std::path::PathBuf::from(device_path)),
        libfreemkv::KeySpec::default(),
    ) {
        Ok(s) => s,
        Err(e) => {
            let msg = format_lib_error("Cannot open drive", &e);
            crate::server::log::device_log(device, &msg);
            update_state(
                device,
                RipState {
                    device: device.to_string(),
                    status: "error".to_string(),
                    last_error: msg,
                    ..Default::default()
                },
            );
            return;
        }
    };

    // Fast identify — disc name only, no playlists
    crate::server::log::device_log(device, "Identifying disc...");
    let disc_id = match session.identify() {
        Ok(id) => id,
        Err(e) => {
            let msg = format_lib_error("Could not read the disc", &e);
            crate::server::log::device_log(device, &msg);
            update_state(
                device,
                RipState {
                    device: device.to_string(),
                    status: "error".to_string(),
                    last_error: msg,
                    ..Default::default()
                },
            );
            return;
        }
    };

    let id_name = disc_id.name().to_string();

    crate::server::log::device_log(device, &format!("Disc: {}", id_name));

    // TMDB lookup — fast, user sees poster while full scan runs
    let tmdb = crate::server::tmdb::lookup(&id_name, &cfg_read.tmdb_api_key);
    let display_name = tmdb
        .as_ref()
        .map(|t| t.title.clone())
        .unwrap_or_else(|| id_name.clone());

    // Show identify results immediately — no format badge until full scan confirms UHD vs BD
    update_state(
        device,
        RipState {
            device: device.to_string(),
            status: "scanning".to_string(),
            disc_present: true,
            disc_name: display_name.clone(),
            // The raw volume label, carried alongside the TMDB title from the
            // first state push onward: it is the only thing that distinguishes
            // the discs of a boxset, which all resolve to one title.
            disc_label: id_name.clone(),
            disc_format: String::new(),
            tmdb_title: tmdb.as_ref().map(|t| t.title.clone()).unwrap_or_default(),
            tmdb_year: tmdb.as_ref().map(|t| t.year).unwrap_or(0),
            tmdb_poster: tmdb
                .as_ref()
                .map(|t| t.poster_url.clone())
                .unwrap_or_default(),
            tmdb_overview: tmdb
                .as_ref()
                .map(|t| t.overview.clone())
                .unwrap_or_default(),
            ..Default::default()
        },
    );

    // Full scan — titles, streams, AACS keys
    crate::server::log::device_log(device, "Scanning titles...");
    let scan_opts = scan_opts_for(&cfg_read);
    // Arm the scan-phase watchdog: WARNs every 15s while scan/resolve runs,
    // torn down by the drop-guard when this block returns.
    let scan_wd = ScanWatchdog::arm(device);
    let scan_t0 = std::time::Instant::now();
    tracing::info!(device = %device, "scan: begin");
    if let Err(e) = session.scan(scan_opts) {
        let msg = format_lib_error("Disc scan", &e);
        crate::server::log::device_log(device, &msg);
        update_state(
            device,
            RipState {
                device: device.to_string(),
                status: "error".to_string(),
                last_error: msg,
                ..Default::default()
            },
        );
        return;
    }
    // Decompose the session into the owned disc + live drive the rest of
    // scan_disc (the key resolve, unlocker matrix, store_session) uses.
    let Some(disc) = session.take_disc() else {
        let msg = "Disc scan failed: the scan produced no disc".to_string();
        crate::server::log::device_log(device, &msg);
        update_state(
            device,
            RipState {
                device: device.to_string(),
                status: "error".to_string(),
                last_error: msg,
                ..Default::default()
            },
        );
        return;
    };
    // into_drive is fallible: stage_drive_as_reader moves the drive out, so an
    // empty slot is reachable through ordinary API use rather than being a
    // caller error. Report it the same way a failed scan is reported.
    let mut drive = match session.into_drive() {
        Ok(d) => d,
        Err(e) => {
            let msg = format_lib_error("Disc scan", &e);
            crate::server::log::device_log(device, &msg);
            update_state(
                device,
                RipState {
                    device: device.to_string(),
                    status: "error".to_string(),
                    last_error: msg,
                    ..Default::default()
                },
            );
            return;
        }
    };
    tracing::info!(device = %device, elapsed_ms = scan_t0.elapsed().as_millis() as u64, "scan: structure done");

    // User-facing unlocker matrix — which unlockers RAN, emitted right after disc-identify and
    // BEFORE the keyserver (depends only on drive-init + scan state, not key resolution).
    {
        let matrix = disc
            .unlocker_matrix(&drive)
            .into_iter()
            .map(|(name, ok)| format!("{name}: {}", if ok { "yes" } else { "no" }))
            .collect::<Vec<_>>()
            .join(", ");
        crate::server::log::device_log(device, &format!("Unlockers — {matrix}"));
    }

    // The rip's key set, resolved ONCE here, right after the scan, for every title the rip
    // produces (KU §2.1); memory only, it keys the whole rip. Online can take a minute or
    // two, so the status says so. A DVD's CSS needs no key source (the resolve is a no-op).
    let media_type = tmdb.as_ref().map(|t| t.media_type.as_str()).unwrap_or("");
    let scan_name = disc
        .meta_title
        .clone()
        .unwrap_or_else(|| disc.volume_id.clone());
    let key_scope = rip_key_scope(&disc, &cfg_read, media_type, &scan_name);
    if announces_online_resolve(&cfg_read, &disc, &key_scope) {
        crate::server::log::device_log(device, "Communicating with online keyserver...");
        update_state_with(device, |s| {
            s.key_status = "Communicating with online keyserver…".to_string();
        });
    }
    scan_wd.enter_resolve();
    let resolve_t0 = std::time::Instant::now();
    tracing::info!(device = %device, "resolve_keys: begin");
    let keys = resolve_rip_keys(device, &cfg_read, &mut drive, &disc, &key_scope, None);
    // Capture the real decode's reachability now, before anything else can overwrite the
    // per-thread slot — it classifies a no-key without a second empty probe.
    let decode_reach = crate::server::keysource::take_online_decode_reachability();
    tracing::info!(device = %device, elapsed_ms = resolve_t0.elapsed().as_millis() as u64, "resolve_keys: end");
    // Down-vs-no-key: bounded-retry a transient online outage rather than reporting a
    // permanent "no keys found". `key_reach` is `Some` only when the rip is still keyless.
    let (keys, key_reach) = match keys {
        Err(e) if crate::server::keysource::uses_online(&cfg_read) => {
            let at = (&disc, &key_scope);
            retry_online_keys_on_outage(device, &cfg_read, &mut drive, at, e, decode_reach)
        }
        keys => (keys, None),
    };
    // Scan + resolve are done; stand the watchdog down explicitly (drop also
    // covers any early return above).
    drop(scan_wd);
    // Every key-service outcome gets its OWN tile text — `key_readiness` picks
    // it from the verdict (outage vs definitive no-key vs licence wall vs
    // unexpected status) rather than from one collapsed error code.
    let keyed = rip_keyed(&disc, &key_scope, &keys);
    let key_status = key_readiness(
        &disc,
        keyed,
        keys.as_ref().err(),
        cfg_read.capture_without_keys,
        key_reach,
    );
    let (keys, key_error) = match keys {
        Ok(set) => (Some(set), None),
        Err(e) => (None, Some(e)),
    };

    // Update format from full scan (UHD vs BD now known)
    let disc_name = disc
        .meta_title
        .as_deref()
        .unwrap_or(&disc.volume_id)
        .to_string();
    let disc_format = match disc.format {
        libfreemkv::DiscFormat::Uhd => "uhd",
        libfreemkv::DiscFormat::Fmts => "fmts",
        libfreemkv::DiscFormat::BluRay => "bluray",
        libfreemkv::DiscFormat::HdDvd => "hddvd",
        libfreemkv::DiscFormat::Dvd => "dvd",
        libfreemkv::DiscFormat::Unknown => "unknown",
    }
    .to_string();

    crate::server::log::device_log(
        device,
        &format!(
            "Scanned: {} ({}, {} titles)",
            disc_name,
            disc_format,
            disc.titles.len()
        ),
    );

    // Extract title info before storing session
    let duration = disc
        .titles
        .first()
        .map(|t| crate::server::util::format_duration_hm(t.duration_secs))
        .unwrap_or_default();
    let codecs = disc.titles.first().map(format_codecs).unwrap_or_default();

    // Store session — drive stays open for rip
    store_session(
        device,
        DriveSession {
            drive,
            disc: Some(disc),
            scanned: true,
            probed: false,
            tmdb: tmdb.clone(),
            device_path: device_path.to_string(),
            key_verdict: key_reach,
            keys,
            key_error,
        },
    );

    // 0.20.7: if resume-on-startup flipped this disc's staging dir to
    // `.failed` (restart loop), surface it on the dashboard before a fresh
    // rip; `failure_reason` overrides the normal idle status when present.
    let staging_disc = cfg_read.staging_device_dir(&staging::staging_basename(
        std::path::Path::new(&cfg_read.staging_dir),
        &display_name,
        &id_name,
    ));
    let failure_reason = staging::read_failed_reason(std::path::Path::new(&staging_disc));
    let (status_str, last_error_str, failure_field) = match failure_reason.as_ref() {
        Some(r) => ("failed".to_string(), r.clone(), Some(r.clone())),
        None => ("idle".to_string(), String::new(), None),
    };

    // Does this disc have resumable partial staging? Drives the dashboard's
    // Resume-vs-Rip choice. Computed before `display_name` moves into the state.
    let resumable = resumable_for_disc(&cfg_read, &display_name, &id_name);

    update_state(
        device,
        RipState {
            device: device.to_string(),
            status: status_str,
            disc_present: true,
            disc_name: display_name,
            disc_label: id_name.clone(),
            disc_format,
            tmdb_title: tmdb.as_ref().map(|t| t.title.clone()).unwrap_or_default(),
            tmdb_year: tmdb.as_ref().map(|t| t.year).unwrap_or(0),
            tmdb_poster: tmdb
                .as_ref()
                .map(|t| t.poster_url.clone())
                .unwrap_or_default(),
            tmdb_overview: tmdb
                .as_ref()
                .map(|t| t.overview.clone())
                .unwrap_or_default(),
            duration,
            codecs,
            last_error: last_error_str,
            failure_reason: failure_field,
            key_status,
            resumable,
            ..Default::default()
        },
    );
}

// ─── Rip ───────────────────────────────────────────────────────────────────

/// Entry point for `/api/rip[?resume=yes|no]`. Scans the disc to
/// identify it, then dispatches to `resume_remux` or `rip_disc`
/// depending on the resume mode requested by the caller and the
/// presence of resumable staging state.
///
/// This is the *only* path that starts disk-writing work: the HTTP API / UI,
/// and disc insertion when `on_insert` is `rip` (`Fresh`) or `resume` (`Prefer`).
pub fn handle_rip_request(
    cfg: &Arc<RwLock<Config>>,
    device: &str,
    device_path: &str,
    mode: crate::server::web::ResumeMode,
) {
    // Skip the scan if already scanned since insertion — a redundant scan
    // clears the UI poster/title and re-runs TMDB for no benefit. Eject +
    // re-insert calls drop_session, so a stale session can't survive it.
    if !session_is_scanned(device) {
        scan_disc(cfg, device, device_path);
    } else {
        crate::server::log::device_log(
            device,
            "Skipping redundant scan — disc already identified since insertion.",
        );
    }
    let cancelled = device_halt(device)
        .map(|h| h.is_cancelled())
        .unwrap_or(false);
    if cancelled {
        return;
    }
    // A failed scan left no identity, so every staging guard would pass blind; its error
    // state stands rather than sweeping into a dir nothing checked.
    if !session_is_scanned(device) {
        crate::server::log::device_log(device, "Not ripping: the disc scan did not complete.");
        return;
    }
    dispatch_rip_request(cfg, device, device_path, mode);
}

// Post-scan half of `handle_rip_request`: route the scanned disc by resume mode.
fn dispatch_rip_request(
    cfg: &Arc<RwLock<Config>>,
    device: &str,
    device_path: &str,
    mode: crate::server::web::ResumeMode,
) {
    // E7034: this disc is what its staged image waits for; finish that mux, never re-rip.
    if mode != crate::server::web::ResumeMode::Wipe
        && disc_staging_hold(cfg, device, false) == Some(StagingHold::NeedsDisc)
        && let Some(class) = find_resumable_for_disc(cfg, device)
    {
        crate::server::log::device_log(
            device,
            "Disc inserted for its staged image (E7034) — finishing the mux from the image",
        );
        resume::resume_remux(cfg, device, class);
        drop_session(device);
        return;
    }
    match mode {
        crate::server::web::ResumeMode::Require => {
            if resume_refused_by_staging(cfg, device) {
                return;
            }
            if resumable_for_device(cfg, device) == Some(Resumable::Sweep) {
                // Continue Pass N from the mapfile, re-reading only not-good
                // ranges instead of the whole disc; `passes = N` is the
                // recovery budget, nothing is ever abandoned as "dead".
                crate::server::log::device_log(
                    device,
                    "Resume requested: continuing partial sweep from mapfile",
                );
                rip_disc(cfg, device, device_path, true);
            } else if let Some(class) = find_resumable_for_disc(cfg, device) {
                // Mapfile is 100% recovered — just re-mux the staged ISO, no
                // disc reads.
                crate::server::log::device_log(device, "Resume requested: re-muxing existing ISO");
                resume::resume_remux(cfg, device, class);
                drop_session(device);
            } else {
                crate::server::log::device_log(
                    device,
                    "Resume requested but no resumable staging state found for this disc",
                );
                update_state(
                    device,
                    RipState {
                        device: device.to_string(),
                        status: "error".to_string(),
                        last_error:
                            "Resume requested but no resumable staging state found for this disc"
                                .to_string(),
                        ..Default::default()
                    },
                );
                drop_session(device);
            }
        }
        crate::server::web::ResumeMode::Prefer => {
            // 1.7.7: continue a started disc, else rip fresh.
            if staging_hold_stands_down(cfg, device, GuardFor::Insert) {
                return;
            }
            match auto_resume_action(resumable_for_device(cfg, device)) {
                AutoResumeAction::Sweep => {
                    crate::server::log::device_log(
                        device,
                        "Auto-resume: continuing partial sweep from mapfile",
                    );
                    rip_disc(cfg, device, device_path, true);
                }
                AutoResumeAction::Remux => {
                    if let Some(class) = find_resumable_for_disc(cfg, device) {
                        crate::server::log::device_log(
                            device,
                            "Auto-resume: re-muxing existing ISO",
                        );
                        resume::resume_remux(cfg, device, class);
                        drop_session(device);
                    } else {
                        auto_rip_fresh(cfg, device, device_path);
                    }
                }
                AutoResumeAction::Fresh => auto_rip_fresh(cfg, device, device_path),
            }
        }
        crate::server::web::ResumeMode::Wipe => {
            // Never wipe a dir the mux worker is actively reading: an
            // in-flight `remove_dir_all` yanks the ISO out from under it,
            // permanently losing the staging dir with no retry possible.
            if disc_owned_by_worker(cfg, device) {
                crate::server::log::device_log(
                    device,
                    "Refusing to wipe staging: the mux worker is reading this disc's staged ISO (.ripped/.muxing). Wait for the mux to finish, then retry.",
                );
                update_state_with(device, |s| {
                    s.status = "error".to_string();
                    s.last_error =
                        "Cannot wipe: staged ISO is owned by the mux worker. Retry after mux completes."
                            .to_string();
                });
                drop_session(device);
                return;
            }
            wipe_staging_for_disc(cfg, device);
            rip_disc(cfg, device, device_path, false);
        }
        crate::server::web::ResumeMode::Fresh => auto_rip_fresh(cfg, device, device_path),
        crate::server::web::ResumeMode::Default => {
            if !staging_hold_stands_down(cfg, device, GuardFor::InPlace) {
                rip_disc(cfg, device, device_path, false);
            }
        }
    }
}

/// Why a non-destructive rip must leave this disc's existing staging dir alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagingHold {
    /// The dir exists but could not be read cleanly: its lifecycle is unknown.
    Unreadable,
    /// Listed empty, but a non-recursive rmdir refused: the listing may be hiding files.
    UnconfirmedEmpty,
    /// Finished (`.completed` / `.done`): awaiting or past the mover.
    Completed,
    /// `.ripped` / `.muxing`: the mux worker is reading this ISO.
    OwnedByWorker,
    /// `.review`: finished output held for operator title confirmation.
    HeldForReview,
    /// `.aborted-loss`: swept ISO awaiting the operator's Accept / another pass.
    LossAborted,
    /// `.sweeping` by another drive's live rip of the same disc.
    LiveSweep,
    /// A staged image held for this disc (E7034): inserting it finishes the mux.
    NeedsDisc,
}

/// What the caller of [`staging_hold_stands_down`] is about to do to the staging dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardFor {
    /// Sweep in place (Default).
    InPlace,
    /// On-insert Rip / Resume: only staging owned by the mux worker, swept by another
    /// drive, or unreadable holds; an empty-looking listing is not trusted.
    Insert,
}

// Staging guards for Default and the on-insert modes (Fresh, Prefer). Returns true,
// after surfacing why, when this disc's staging must not be re-swept.
fn staging_hold_stands_down(cfg: &Arc<RwLock<Config>>, device: &str, purpose: GuardFor) -> bool {
    let hold = match disc_staging_hold(cfg, device, purpose == GuardFor::Insert) {
        None => return false,
        Some(
            StagingHold::Completed
            | StagingHold::LossAborted
            | StagingHold::HeldForReview
            | StagingHold::NeedsDisc,
        ) if purpose == GuardFor::Insert => return false,
        Some(hold) => hold,
    };
    let why = match hold {
        StagingHold::Completed => {
            crate::server::log::device_log(
                device,
                "Disc already ripped (.completed marker present) — skipping unattended re-rip. Click Rip to force a fresh rip.",
            );
            let prev = STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(device)
                .cloned();
            update_state(
                device,
                RipState {
                    device: device.to_string(),
                    status: "idle".to_string(),
                    disc_present: true,
                    disc_name: prev
                        .as_ref()
                        .map(|p| p.disc_name.clone())
                        .unwrap_or_default(),
                    disc_format: prev
                        .as_ref()
                        .map(|p| p.disc_format.clone())
                        .unwrap_or_default(),
                    tmdb_title: prev
                        .as_ref()
                        .map(|p| p.tmdb_title.clone())
                        .unwrap_or_default(),
                    tmdb_year: prev.as_ref().map(|p| p.tmdb_year).unwrap_or(0),
                    tmdb_poster: prev
                        .as_ref()
                        .map(|p| p.tmdb_poster.clone())
                        .unwrap_or_default(),
                    ..Default::default()
                },
            );
            drop_session(device);
            return true;
        }
        StagingHold::OwnedByWorker => {
            "Disc rip already staged and owned by the mux worker (.ripped/.muxing) — skipping unattended re-sweep."
        }
        StagingHold::LossAborted => {
            "Disc has a loss-aborted staged ISO awaiting an operator decision — NOT re-ripping. Use 'Accept damage' to deliver it, or 'Resume' to run another recovery pass."
        }
        StagingHold::HeldForReview => {
            "Disc has a finished rip held for title review — NOT re-ripping. Confirm or cancel it under Review first."
        }
        StagingHold::LiveSweep => {
            "Another drive is sweeping this disc's staging dir right now — NOT re-ripping."
        }
        StagingHold::NeedsDisc => {
            "This disc's staged image is waiting for it (E7034) but could not be resumed — NOT re-ripping. Use Resume to finish it."
        }
        StagingHold::Unreadable => {
            "Cannot read this disc's staging dir cleanly (staging share degraded?) — NOT re-ripping, so a finished rip can't be destroyed. Retry once staging is readable."
        }
        StagingHold::UnconfirmedEmpty => {
            "This disc's staging dir lists as empty but could not be removed (stale network-share listing?) — NOT wiping it. Retry once staging is readable, or use Rip to start over."
        }
    };
    crate::server::log::device_log(device, why);
    // The UI renders Accept/Resume on loss_aborted && !active.
    stand_down_idle(device, hold == StagingHold::LossAborted);
    true
}

// The operator's Resume skips the insert guards (A6 covers finished dirs) but must not sweep
// over staging it can't read or another drive is sweeping. True after reporting why.
fn resume_refused_by_staging(cfg: &Arc<RwLock<Config>>, device: &str) -> bool {
    let why = match disc_staging_hold(cfg, device, false) {
        Some(StagingHold::Unreadable) => {
            "Cannot read this disc's staging dir cleanly (staging share degraded?) — not resuming. Retry once staging is readable."
        }
        Some(StagingHold::LiveSweep) => {
            "Another drive is sweeping this disc's staging dir right now — not resuming."
        }
        _ => return false,
    };
    crate::server::log::device_log(device, why);
    update_state_with(device, |s| {
        s.status = "error".to_string();
        s.last_error = why.to_string();
    });
    drop_session(device);
    true
}

// The hold on the scanned disc's own staging dir, read fail-closed: a dir that exists but
// can't be snapshotted cleanly is `Unreadable`, never "nothing staged". `for_wipe` removes a
// dir that listed empty only if a non-recursive rmdir confirms it.
fn disc_staging_hold(
    cfg: &Arc<RwLock<Config>>,
    device: &str,
    for_wipe: bool,
) -> Option<StagingHold> {
    // Recover a poisoned lock instead of failing open.
    let cfg_read = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    let sanitized = staging_basename_for_device(&cfg_read, device)?;
    let root = std::path::Path::new(&cfg_read.staging_dir);
    let dir = root.join(&sanitized);
    match std::fs::symlink_metadata(&dir) {
        Ok(_) => {}
        // A cold-cache lookup can miss an existing dir: confirm with the retrying listing.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return match list_staging_basenames(root) {
                Some(names)
                    if !names
                        .iter()
                        .any(|b| staging_dir_matches_disc(b, &sanitized)) =>
                {
                    None
                }
                None if std::fs::symlink_metadata(root)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    None
                }
                _ => Some(StagingHold::Unreadable),
            };
        }
        Err(_) => return Some(StagingHold::Unreadable),
    }
    let snap = match staging::snapshot_staging_disc(&dir) {
        Some(s) if !s.had_entry_error && s.state_unreadable.is_none() => s,
        _ => return Some(StagingHold::Unreadable),
    };
    // An empty listing can be a cold-cache lie; only a non-recursive rmdir proves it empty.
    if for_wipe && snap.saw_no_entries {
        return match std::fs::remove_dir(&dir) {
            Ok(()) => None,
            Err(_) => Some(StagingHold::UnconfirmedEmpty),
        };
    }
    snapshot_hold(&snap).or_else(|| {
        (snap.has_sweeping && another_drive_sweeping(&cfg_read, device, &sanitized))
            .then_some(StagingHold::LiveSweep)
    })
}

// Pure: the hold a cleanly-read snapshot imposes. `.done` counts on its own for old-format
// dirs that crashed between the `.done` and `.completed` writes.
fn snapshot_hold(snap: &staging::StagingSnapshot) -> Option<StagingHold> {
    if snap.needs_disc && !snap.has_muxing {
        Some(StagingHold::NeedsDisc)
    } else if snap.has_ripped || snap.has_muxing {
        Some(StagingHold::OwnedByWorker)
    } else if snap.has_review {
        Some(StagingHold::HeldForReview)
    } else if snap.completed || snap.has_done {
        Some(StagingHold::Completed)
    } else if snap.has_aborted_loss {
        Some(StagingHold::LossAborted)
    } else {
        None
    }
}

// Is another drive's live rip thread working in the same staging dir (same disc in two drives)?
// A `.sweeping` dir no thread owns is a crash leftover.
fn another_drive_sweeping(cfg: &Config, device: &str, sanitized: &str) -> bool {
    let others: Vec<String> = STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .filter(|d| d.as_str() != device)
        .cloned()
        .collect();
    others.iter().any(|d| {
        rip_thread_running(d) && staging_basename_for_device(cfg, d).as_deref() == Some(sanitized)
    })
}

// Leave a skipped disc idle (not stuck "scanning") and release its drive session.
fn stand_down_idle(device: &str, loss_aborted: bool) {
    update_state_with(device, |s| {
        s.status = "idle".to_string();
        s.disc_present = true;
        s.loss_aborted = loss_aborted;
    });
    drop_session(device);
}

// True if a staging-dir basename is the resume/completion match for a sanitized disc name.
// EXACT equality only — a prefix match would collide.
fn staging_dir_matches_disc(basename: &str, sanitized: &str) -> bool {
    basename == sanitized
}

// List the immediate-child basenames of the staging root with the same NFS cold-cache
// discipline as staging::snapshot_staging_disc; retries read_dir on error and unions results.
fn list_staging_basenames(staging_dir: &std::path::Path) -> Option<Vec<String>> {
    let mut saw_read_ok = false;
    // Insertion-ordered union of every basename observed across passes; the
    // set guards against duplicating a name seen in more than one pass.
    let mut union: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for attempt in 0..3 {
        if let Ok(entries) = std::fs::read_dir(staging_dir) {
            saw_read_ok = true;
            let mut had_entry_error = false;
            for entry in entries {
                match entry {
                    Ok(e) => {
                        if let Some(n) = e.path().file_name() {
                            let name = n.to_string_lossy().into_owned();
                            if seen.insert(name.clone()) {
                                union.push(name);
                            }
                        }
                    }
                    // Don't `.flatten()` away per-entry errors: a partial NFS
                    // degradation can error on one DirEntry while the dir is
                    // genuinely populated. Retry rather than trust this pass.
                    Err(_) => had_entry_error = true,
                }
            }
            if !had_entry_error {
                // Clean, complete listing — trust it immediately. We still
                // return the accumulated union: any name from a prior degraded
                // pass that this clean pass happened not to surface stays in.
                return Some(union);
            }
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    if saw_read_ok {
        // Every pass that opened had at least one entry error; return the union
        // of every basename we observed rather than None, so a disc whose dir
        // appeared in any pass is still matchable.
        Some(union)
    } else {
        // Never opened the directory across all retries — UNKNOWN. Behave
        // like the old `read_dir(...).ok()?` (no listing → no match).
        None
    }
}

/// The staging-dir basename for the disc currently in `device`, or None
/// when STATE has no disc name for it (nothing scanned — the sanitized
/// empty name would otherwise point at the staging ROOT).
///
/// The single entry point for every STATE-reading caller that needs a staging path. Reads both
/// halves of the disc's identity (TMDB display title + raw volume label) and hands them to
/// [`staging::staging_basename`], the one place the naming rule lives.
pub fn staging_basename_for_device(cfg: &Config, device: &str) -> Option<String> {
    // Recover from a poisoned mutex rather than silently returning None:
    // callers read this to decide "already ripped?" / "resumable?", and a
    // dropped answer either re-rips a finished disc or hides a resume.
    let (display_name, disc_label) = {
        let s = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let rs = s.get(device)?;
        (rs.disc_name.clone(), rs.disc_label.clone())
    };
    if display_name.is_empty() {
        return None;
    }
    Some(staging::staging_basename(
        std::path::Path::new(&cfg.staging_dir),
        &display_name,
        &disc_label,
    ))
}

// Does the currently-scanned disc have a staging dir OWNED by the mux worker (`.ripped`
// pending, or `.muxing` held)? Refuses a fresh sweep that would truncate the ISO the worker is
// reading.
fn disc_owned_by_worker(cfg: &Arc<RwLock<Config>>, device: &str) -> bool {
    // Recover a poisoned lock instead of failing open: returning false here
    // risks a fresh sweep truncating an in-flight mux ISO the worker is reading.
    let cfg_read = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(sanitized) = staging_basename_for_device(&cfg_read, device) else {
        return false;
    };
    let staging_root = std::path::Path::new(&cfg_read.staging_dir);
    staging_disc_owned_by_worker(staging_root, &sanitized)
}

/// Pure core of `disc_owned_by_worker`: does a staging dir whose basename
/// exactly matches `sanitized` carry `.ripped` or `.muxing`? Split out (no
/// `STATE`/`Config` reads) so the H1 exclusion is unit-testable.
fn staging_disc_owned_by_worker(staging_root: &std::path::Path, sanitized: &str) -> bool {
    let Some(basenames) = list_staging_basenames(staging_root) else {
        return false;
    };
    for basename in basenames {
        let path = staging_root.join(&basename);
        if !staging_dir_matches_disc(&basename, sanitized) {
            continue;
        }
        // NFS-resilient snapshot, not two bare `.exists()` stats: a cold-cache
        // mount after a restart can hide the marker from a raw stat, letting
        // Default auto-rip fall through to rip_disc and O_TRUNC the ISO.
        if let Some(snap) = staging::snapshot_staging_disc(&path)
            && ((snap.has_ripped && !snap.needs_disc) || snap.has_muxing)
        {
            return true;
        }
    }
    false
}

// Is this staging dir blocked from drive-resume (Remux) by an owner, held, or terminal marker?
// Pure projection of the snapshot booleans so the H1/M3 skip rules are unit-testable.
fn resumable_dir_blocked(snap: &staging::StagingSnapshot) -> bool {
    // `completed` also blocks: with `keep_iso = true` the ISO survives past
    // completion, so a manual Require on a just-finished dir could otherwise
    // pass this gate and delete_partial_output would destroy the delivered MKV.
    (snap.has_ripped && !snap.needs_disc)
        || snap.has_muxing
        || snap.has_review
        || snap.has_failed
        || snap.completed
}

// Look at the staging dirs for a Remux-eligible entry matching the sanitized display_name of
// the currently-scanned disc; returns the `ResumeClass::Remux` payload if found, else None.
fn find_resumable_for_disc(cfg: &Arc<RwLock<Config>>, device: &str) -> Option<resume::ResumeClass> {
    // Recover from a poisoned mutex rather than silently returning None (which
    // would fail to resume a valid staged ISO). Matches disc_staging_hold.
    let cfg_read = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    let sanitized = staging_basename_for_device(&cfg_read, device)?;
    // NFS-resilient listing, not `read_dir(...).flatten()`, which would
    // silently drop the disc's dir on a cold-cache error and fall through
    // to a fresh sweep instead of resuming the existing ISO.
    let staging_root = std::path::Path::new(&cfg_read.staging_dir);
    let basenames = list_staging_basenames(staging_root)?;
    for basename in basenames {
        let path = staging_root.join(&basename);
        // EXACT name match: a prefix match would collide (e.g. "Feature" vs
        // "Feature_2") and resume onto a different title's partial ISO.
        if staging_dir_matches_disc(&basename, &sanitized) {
            // User-initiated resume goes straight to the remux-eligibility
            // check, still refusing OWNED (.ripped/.muxing), HELD (.review),
            // or TERMINAL (.failed) dirs — see resumable_dir_blocked above.
            let snap = staging::snapshot_staging_disc(&path)?;
            // Owned/held/terminal dirs are not drive-resumable — see
            // `resumable_dir_blocked` for the per-marker reasoning (H1/M3).
            if resumable_dir_blocked(&snap) {
                continue;
            }
            if !snap.has_iso || !snap.has_mapfile {
                continue;
            }
            let (iso_path, mapfile_path) = resume::find_iso_and_mapfile(&path)?;
            let map = match freemkv_engine::Mapfile::load(&mapfile_path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let stats = map.stats();
            if stats.bytes_pending != 0 {
                continue;
            }
            // Same truncation guard as classify_resume: the mapfile can read
            // fully-swept while its ISO was truncated afterward (crash/OOM/
            // disk-full). Refuse and re-sweep fresh rather than resume onto it.
            if std::fs::metadata(&iso_path).is_ok_and(|m| m.len() < stats.bytes_total) {
                continue;
            }
            // FILE basename (ISO stem), never `basename` — that's the dir
            // name carrying the `_2` boxset suffix the files never take.
            // Same invariant as classify_resume; resume_remux names output from it.
            let display_name = match iso_path.file_stem() {
                Some(n) => n.to_string_lossy().into_owned(),
                None => continue,
            };
            return Some(resume::ResumeClass::Remux {
                iso_path,
                mapfile_path,
                display_name,
                // Cold disc-insert resume from preserved staging: no `.ripped`
                // hand-off and no operator-override concept, so confidence is
                // unknown — resume_remux falls back to its own match check.
                title_confident: None,
            });
        }
    }
    None
}

// True if `seg` is safe to use as a single staging-directory path segment: rejects empty,
// all-dots, path separators, absolute paths. Independent of the sanitizer on purpose.
fn is_safe_staging_segment(seg: &str) -> bool {
    !seg.is_empty()
        && !seg.chars().all(|c| c == '.')
        && !seg.contains('/')
        && !seg.contains('\\')
        && std::path::Path::new(seg).components().count() == 1
        && matches!(
            std::path::Path::new(seg).components().next(),
            Some(std::path::Component::Normal(_))
        )
}

/// Wipe the staging subdir for the currently-scanned disc. Used by
/// `/api/rip?resume=no` to give the user an explicit clean slate
/// before a fresh sweep.
fn wipe_staging_for_disc(cfg: &Arc<RwLock<Config>>, device: &str) {
    // Recover a poisoned lock instead of silently no-op'ing the user's explicit
    // clean-slate request: bailing here leaves stale staging for the fresh sweep.
    let cfg_read = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    // Wipe THIS disc's dir, not merely the one its title names: with a boxset
    // in the drive, `Movie` may belong to disc 1 while disc 2 owns `Movie_2`,
    // and wiping by title would destroy the wrong disc's staging.
    let Some(sanitized) = staging_basename_for_device(&cfg_read, device) else {
        return;
    };
    // Defence-in-depth: never let an untrusted disc label sanitize to a
    // segment that escapes the staging root — else `join("..")` +
    // `remove_dir_all` would delete its parent.
    if !is_safe_staging_segment(&sanitized) {
        crate::server::log::device_log(
            device,
            &format!("Refusing to wipe staging: unsafe sanitized dir name {sanitized:?}"),
        );
        return;
    }
    let staging_root = std::path::Path::new(&cfg_read.staging_dir);
    let path = staging_root.join(&sanitized);
    // Belt-and-braces: confirm the join stays strictly inside the
    // staging root before removing anything.
    if path.parent() != Some(staging_root) {
        crate::server::log::device_log(
            device,
            &format!(
                "Refusing to wipe staging: {} is not a direct child of {}",
                path.display(),
                staging_root.display()
            ),
        );
        return;
    }
    if path.exists() {
        match std::fs::remove_dir_all(&path) {
            Ok(_) => crate::server::log::device_log(
                device,
                &format!("Wiped staging dir for fresh rip: {}", path.display()),
            ),
            Err(e) => crate::server::log::device_log(
                device,
                &format!("Failed to wipe staging dir {}: {}", path.display(), e),
            ),
        }
    }
}

// Detect whether `display_name`'s disc has resumable staging state and of what kind: Remux
// only when the mapfile is 100% Finished (no pending, no unreadable bytes), else Sweep.
fn resumable_for_disc(cfg: &Config, display_name: &str, disc_label: &str) -> Option<Resumable> {
    if display_name.is_empty() {
        return None;
    }
    // Disc-specific, not title-specific: `disc_label` is what stops disc 2 of a
    // boxset being offered disc 1's partial ISO to "resume" onto.
    let sanitized = staging::staging_basename(
        std::path::Path::new(&cfg.staging_dir),
        display_name,
        disc_label,
    );
    // NFS-resilient listing, not `read_dir(...).flatten()`, which could hide
    // an existing resumable dir and make the operator re-sweep instead of
    // resuming. Mirrors disc_staging_hold.
    let staging_root = std::path::Path::new(&cfg.staging_dir);
    let basenames = list_staging_basenames(staging_root)?;
    for basename in basenames {
        let path = staging_root.join(&basename);
        // EXACT match only — a prefix match invites the collision class
        // (`Redshift` prefixing `Redshift_2`) staging_dir_matches_disc fixes.
        if basename != sanitized {
            continue;
        }
        // A terminal `.failed` (or held `.review`) dir is NOT resumable: a
        // re-rip wouldn't clear stale `.failed`, and the mux worker would
        // skip it forever. Mirrors resumable_dir_blocked; forces a Wipe.
        if let Some(snap) = staging::snapshot_staging_disc(&path) {
            // Nor a finished rip (`.completed`/`.done`): the mover may be copying it.
            if snap.has_failed || snap.has_review || snap.completed || snap.has_done {
                return None;
            }
            // A dir the mux worker owns (.ripped/.muxing) must NOT be offered
            // as resumable — resuming would race a fresh sweep against the
            // worker's reads. Mirrors disc_owned_by_worker's Wipe guard.
            if (snap.has_ripped && !snap.needs_disc) || snap.has_muxing {
                return None;
            }
        }
        let (_iso_path, mapfile_path) = match resume::find_iso_and_mapfile(&path) {
            Some(p) => p,
            None => continue,
        };
        let map = match freemkv_engine::Mapfile::load(&mapfile_path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let st = map.stats();
        // Any not-good data (pending or previously Unreadable) is retryable —
        // there is NO terminal "won't retry" state. Only a mapfile that is
        // 100% Finished resumes straight to remux.
        return Some(if st.bytes_pending == 0 && st.bytes_unreadable == 0 {
            Resumable::Remux
        } else {
            Resumable::Sweep
        });
    }
    None
}

/// STATE-reading wrapper of [`resumable_for_disc`] used by the `?resume=yes`
/// action (the disc has been scanned, so its name is in STATE).
fn resumable_for_device(cfg: &Arc<RwLock<Config>>, device: &str) -> Option<Resumable> {
    let cfg_read = cfg.read().unwrap_or_else(|e| e.into_inner()).clone();
    let (display_name, disc_label) = {
        let s = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let rs = s.get(device)?;
        (rs.disc_name.clone(), rs.disc_label.clone())
    };
    resumable_for_disc(&cfg_read, &display_name, &disc_label)
}

// RAII guard that unregisters a device's halt-map entry on drop, so
// every `rip_disc` exit path (errors, normal tail, panics) cleans up
// the entry. See the v0.13.6 halt-map-leak class.
struct HaltGuard {
    device: String,
}

impl Drop for HaltGuard {
    fn drop(&mut self) {
        unregister_halt(&self.device);
    }
}

// RAII guard that clears the `.sweeping` in-progress marker on drop, held for the whole
// `rip_disc` body so every early-return branch and panic clears it.
struct SweepingGuard {
    staging: std::path::PathBuf,
}

impl Drop for SweepingGuard {
    fn drop(&mut self) {
        staging::clear_sweeping_marker(&self.staging);
    }
}

// Install this rip attempt's initial Halt, CARRYING the outgoing token's cancel so a Stop
// landing between the pre-call cancel check and this line isn't silently discarded.
fn install_rip_halt(device: &str) {
    swap_halt_carrying_cancel(device, libfreemkv::Halt::new());
}

// Report a post-mux failure that leaves the staging dir RESUMABLE (not `.failed`), and set a
// terminal `status` so `is_busy()` doesn't stick true forever.
fn abort_post_mux_preserving_staging(device: &str, log_line: &str, last_error: &str) {
    crate::server::log::device_log(device, log_line);
    update_state_with(device, |s| {
        // "error", not "failed": `failed` pairs with a `.failed` marker, and
        // neither call site here writes one. Matches the mux-time loss-abort
        // return above, the other resumable-but-over exit from this function.
        s.status = "error".to_string();
        if s.last_error.is_empty() {
            s.last_error = last_error.to_string();
        }
    });
}

// Fire the drive-free `rip_complete` webhook: disc read finished, drive free. FIRST of three
// pipeline hooks (rip → mux → move).
#[allow(clippy::too_many_arguments)]
fn fire_rip_complete_webhook(
    cfg: &Config,
    device: &str,
    display_name: &str,
    disc_format: &str,
    tmdb_poster: &str,
    tmdb_year: u16,
    duration: &str,
    codecs: &str,
    iso_path_str: &str,
) {
    let (errors, lost_video_secs) = {
        let s = state::STATE.lock().unwrap_or_else(|e| e.into_inner());
        s.get(device)
            .map(|rs| (rs.errors, rs.main_lost_ms / MILLIS_PER_SEC))
            .unwrap_or((0, 0.0))
    };
    let size_gb = std::fs::metadata(iso_path_str)
        .map(|m| m.len() as f64 / BYTES_PER_GIB)
        .unwrap_or(0.0);
    crate::server::webhook::send_rich(
        cfg,
        crate::server::webhook::WebhookEvent::Rip,
        &crate::server::webhook::RipEvent {
            event: "rip_complete",
            title: display_name,
            year: tmdb_year,
            format: disc_format,
            poster_url: tmdb_poster,
            duration,
            codecs,
            size_gb,
            speed_mbs: 0.0,
            elapsed_secs: 0.0,
            output_path: iso_path_str,
            errors,
            lost_video_secs,
        },
    );
}

/// Rip a disc. Reuses the existing drive session from scan_disc.
/// If no session exists, opens fresh (for on_insert=rip).
///
/// `resume_sweep` continues an existing partial sweep: when true, Pass 1's
/// first attempt runs with libfreemkv `SweepOptions.resume = true`, so the
/// existing ISO + mapfile are kept and only the missing (NonTrimmed /
/// non-tried) ranges are read. When false, Pass 1 starts fresh (the mapfile
/// is recreated and the ISO truncated) — the classic full sweep.
pub fn rip_disc(cfg: &Arc<RwLock<Config>>, device: &str, device_path: &str, resume_sweep: bool) {
    // Rip has the mux slot first: a running Library remux stops and re-queues.
    let _mux_slot = crate::server::library::arbiter::claim_for_rip();
    // Replace the spawn site's fresh Halt with one backed by the drive's
    // halt-flag once open, so Stop also pre-empts in-flight Drive::read
    // calls; the swap carries a Stop already landed on the spawn-site token.
    install_rip_halt(device);

    // RAII cleanup for the halt-map entry: every exit path must drop this
    // device's Halt (leaking it was the v0.13.6 bug class). Idempotent, so
    // it composes safely with the eject path that also unregisters.
    let _halt_guard = HaltGuard {
        device: device.to_string(),
    };

    // Per-device log is archived/cleared at SCAN start, NOT here.

    // Snapshot Config and drop the read guard immediately (blocking it
    // queues GETs behind it on Linux's writer-priority RwLock).
    let cfg_read = match cfg.read() {
        Ok(c) => c.clone(),
        Err(_) => {
            // `_halt_guard` still unregisters the Halt token on return.
            mark_config_lock_poisoned(device, "Rip");
            return;
        }
    };

    // Preserve UI state. Recover a poisoned STATE lock rather than dropping it
    // (`.ok()` → None): the paired `update_state` below already recovers via
    // into_inner, so this reader must too or poison blanks the disc-card metadata.
    let prev = STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned();
    update_state(
        device,
        RipState {
            device: device.to_string(),
            status: "scanning".to_string(),
            disc_present: true,
            disc_name: prev
                .as_ref()
                .map(|p| p.disc_name.clone())
                .unwrap_or_default(),
            disc_format: prev
                .as_ref()
                .map(|p| p.disc_format.clone())
                .unwrap_or_default(),
            tmdb_title: prev
                .as_ref()
                .map(|p| p.tmdb_title.clone())
                .unwrap_or_default(),
            tmdb_year: prev.as_ref().map(|p| p.tmdb_year).unwrap_or(0),
            tmdb_poster: prev
                .as_ref()
                .map(|p| p.tmdb_poster.clone())
                .unwrap_or_default(),
            tmdb_overview: prev
                .as_ref()
                .map(|p| p.tmdb_overview.clone())
                .unwrap_or_default(),
            ..Default::default()
        },
    );

    // Reachability of the fresh-scan decode POST, set below when this rip
    // resolves keys just now. A reused session carries scan_disc's verdict instead.
    let mut resume_decode_reach: Option<crate::server::keysource::ServiceReachability> = None;
    // Take the existing session, or open fresh
    let mut session = match take_session(device) {
        Some(s) if s.scanned => {
            crate::server::log::device_log(device, "Reusing drive session");
            s
        }
        existing => {
            // No session or not scanned — open fresh
            if existing.is_some() {
                drop_session(device);
            }
            crate::server::log::device_log(device, "Opening drive...");
            let mut drive = match freemkv_engine::drive::open(std::path::Path::new(device_path)) {
                Ok(d) => d,
                Err(e) => {
                    let msg = format_lib_error("Cannot open drive", &e);
                    crate::server::log::device_log(device, &msg);
                    update_state(
                        device,
                        RipState {
                            device: device.to_string(),
                            status: "error".to_string(),
                            last_error: msg,
                            ..Default::default()
                        },
                    );
                    return;
                }
            };
            if let Err(e) = drive.wait_ready() {
                tracing::warn!(device = %device, error = %e, "drive wait_ready failed (continuing)");
            }
            crate::server::log::device_log(device, "Initializing...");
            if let Err(e) = drive.init() {
                tracing::warn!(device = %device, error = %e, "drive init failed (continuing)");
            }
            // Engage the drive's disc-type read mode before any read. Idempotent.
            if let Err(e) = drive.probe_disc() {
                tracing::warn!(device = %device, error = %e, "drive probe_disc failed (continuing)");
            }

            let scan_opts = scan_opts_for(&cfg_read);
            crate::server::log::device_log(device, "Scanning titles...");
            // Scan-phase watchdog (same as scan_disc): WARNs every 15s while
            // scan/resolve runs, torn down by the drop-guard.
            let scan_wd = ScanWatchdog::arm(device);
            let scan_t0 = std::time::Instant::now();
            tracing::info!(device = %device, "scan: begin");
            let disc = match libfreemkv::Disc::scan(&mut drive, &scan_opts) {
                Ok(d) => d,
                Err(e) => {
                    let msg = format_lib_error("Disc scan", &e);
                    crate::server::log::device_log(device, &msg);
                    update_state(
                        device,
                        RipState {
                            device: device.to_string(),
                            status: "error".to_string(),
                            last_error: msg,
                            ..Default::default()
                        },
                    );
                    return;
                }
            };
            tracing::info!(device = %device, elapsed_ms = scan_t0.elapsed().as_millis() as u64, "scan: structure done");
            let disc_name = disc
                .meta_title
                .as_deref()
                .unwrap_or(&disc.volume_id)
                .to_string();

            let tmdb = crate::server::tmdb::lookup(&disc_name, &cfg_read.tmdb_api_key);

            // The rip's key set, once, right after the scan (KU §2.1); see scan_disc.
            let media_type = tmdb.as_ref().map(|t| t.media_type.as_str()).unwrap_or("");
            let scope = rip_key_scope(&disc, &cfg_read, media_type, &disc_name);
            scan_wd.enter_resolve();
            let keys = resolve_rip_keys(device, &cfg_read, &mut drive, &disc, &scope, None);
            // Capture the real decode's reachability from this fresh resolve so the outage
            // retry below classifies a no-key without a second empty probe.
            resume_decode_reach = crate::server::keysource::take_online_decode_reachability();
            drop(scan_wd);
            let (keys, key_error) = match keys {
                Ok(set) => (Some(set), None),
                Err(e) => (None, Some(e)),
            };

            DriveSession {
                drive,
                disc: Some(disc),
                scanned: true,
                probed: false,
                tmdb,
                device_path: device_path.to_string(),
                key_verdict: None,
                keys,
                key_error,
            }
        }
    };

    let disc = match session.disc.take() {
        Some(d) => d,
        None => {
            tracing::error!(
                device = %device,
                "DriveSession had no disc — every code path that builds a session must set Some(disc); reaching this branch is a logic bug"
            );
            crate::server::log::device_log(device, "Internal error: session has no disc");
            update_state(
                device,
                RipState {
                    device: device.to_string(),
                    status: "error".to_string(),
                    last_error: "Internal error: session has no disc".to_string(),
                    ..Default::default()
                },
            );
            drop_session(device);
            return;
        }
    };

    let disc_name = disc
        .meta_title
        .as_deref()
        .unwrap_or(&disc.volume_id)
        .to_string();
    let disc_format = match disc.format {
        libfreemkv::DiscFormat::Uhd => "uhd",
        libfreemkv::DiscFormat::Fmts => "fmts",
        libfreemkv::DiscFormat::BluRay => "bluray",
        libfreemkv::DiscFormat::HdDvd => "hddvd",
        libfreemkv::DiscFormat::Dvd => "dvd",
        libfreemkv::DiscFormat::Unknown => "unknown",
    }
    .to_string();
    // Pass 1 reads the WHOLE DISC, so total must be capacity_bytes — using
    // titles[0].size_bytes was the v0.13.12 bug showing "0.0 GB / 0.0 GB".
    // Mux phase below re-derives its own total from the input stream.
    let total_bytes = if disc.capacity_bytes > 0 {
        disc.capacity_bytes
    } else {
        disc.titles.first().map(|t| t.size_bytes).unwrap_or(0)
    };

    // An operator title override (Ripper card's "✎ change" picker) takes
    // precedence over the scan's auto-match; falls back to the scan result.
    // A picked title is trusted (treated as confident → no review hold).
    let title_override = take_title_override(device);
    let overridden = title_override.is_some();
    let tmdb_owned: Option<crate::server::tmdb::TmdbResult> =
        title_override.or_else(|| session.tmdb.clone());
    let tmdb = &tmdb_owned;
    let tmdb_title = tmdb.as_ref().map(|t| t.title.clone()).unwrap_or_default();
    let tmdb_year = tmdb.as_ref().map(|t| t.year).unwrap_or(0);
    let tmdb_poster = tmdb
        .as_ref()
        .map(|t| t.poster_url.clone())
        .unwrap_or_default();
    let tmdb_overview = tmdb
        .as_ref()
        .map(|t| t.overview.clone())
        .unwrap_or_default();
    // No TMDB result → EMPTY string, the mover's documented no-match sentinel:
    // routing_media_type coalesces "" to the movie root. A literal "unknown"
    // here previously fell through to the output-root dump instead.
    let tmdb_media_type = tmdb
        .as_ref()
        .map(|t| t.media_type.clone())
        .unwrap_or_default();
    // TMDB numeric id (0 = no match). Persisted in the hand-off marker so a
    // consumer can enrich metadata by id later; the mover reads it back.
    let tmdb_id = tmdb.as_ref().map(|t| t.tmdb_id).unwrap_or(0);

    let display_name = if tmdb_title.is_empty() {
        disc_name.clone()
    } else {
        tmdb_title.clone()
    };
    // Confident = exact title match WITH a year; decides auto-file (.done) vs
    // hold-for-review (.review). No TMDB key means no match is ever possible,
    // so "no API key" counts as confident too, else every rip would review-hold.
    let title_confident = title_is_confident(
        &cfg_read.tmdb_api_key,
        overridden,
        &disc_name,
        &display_name,
        tmdb_year,
    );

    crate::server::log::device_log(
        device,
        &format!(
            "Disc: {} ({}, {} titles)",
            disc_name,
            disc_format,
            disc.titles.len()
        ),
    );

    if disc.titles.is_empty() {
        crate::server::log::device_log(device, "No titles found");
        update_state(
            device,
            RipState {
                device: device.to_string(),
                status: "error".to_string(),
                last_error: "No titles".to_string(),
                ..Default::default()
            },
        );
        return;
    }

    // The main movie, picked by the engine exactly as the CLI and GUI pick it.
    let main = freemkv_engine::resolve_selection(&disc, &freemkv_engine::Selection::MainMovie);
    let main_idx = main.first().copied().unwrap_or(0);
    let title = disc.titles[main_idx].clone();
    let duration = crate::server::util::format_duration_hm(title.duration_secs);
    let codecs = format_codecs(&title);

    // Down-vs-no-key (rip path): the final key-service verdict. A TRANSIENT one
    // bounded-retries then parks the disc below; a terminal one names what the
    // service actually said instead of failing with a generic "no keys".
    let banked_verdict = session.key_verdict.take();
    let reresolve = seed_needs_reresolve(resume_decode_reach, banked_verdict);
    let mut seed_verdict = rip_seed_verdict(resume_decode_reach, banked_verdict);
    // The rip's key set, resolved once at the scan (KU §2.1). A scope that outgrew the scan's
    // (a title override made it a TV rip) tops up only the titles it lacks.
    let key_scope = rip_key_scope(&disc, &cfg_read, &tmdb_media_type, &disc_name);
    let mut rip_keys: KeyResult = match (session.keys.take(), session.key_error.take()) {
        (Some(set), _) if keys_cover(&disc, &set, &key_scope) => Ok(set),
        (Some(set), _) => resolve_rip_keys(
            device,
            &cfg_read,
            &mut session.drive,
            &disc,
            &key_scope,
            Some(&set),
        ),
        (None, Some(e)) => Err(e),
        (None, None) => resolve_rip_keys(
            device,
            &cfg_read,
            &mut session.drive,
            &disc,
            &key_scope,
            None,
        ),
    };
    let mut keyed = rip_keyed(&disc, &key_scope, &rip_keys);
    // Runs under capture-without-keys too: the fixed setting may still find the key.
    if reresolve && !keyed && crate::server::keysource::uses_online(&cfg_read) {
        // The operator may have fixed Settings since the scan: resolve once with the current config.
        crate::server::log::device_log(
            device,
            "Re-resolving keys with the current key-service settings...",
        );
        update_state_with(device, |s| {
            s.key_status = "Communicating with online keyserver…".to_string();
        });
        rip_keys = resolve_rip_keys(
            device,
            &cfg_read,
            &mut session.drive,
            &disc,
            &key_scope,
            None,
        );
        seed_verdict = crate::server::keysource::take_online_decode_reachability();
        keyed = rip_keyed(&disc, &key_scope, &rip_keys);
        let status = key_readiness(
            &disc,
            keyed,
            rip_keys.as_ref().err(),
            cfg_read.capture_without_keys,
            seed_verdict,
        );
        update_state_with(device, |s| s.key_status = status);
    }
    let mut key_verdict: Option<crate::server::keysource::ServiceReachability> = None;
    if should_retry_online_keys(
        crate::server::keysource::uses_online(&cfg_read),
        cfg_read.capture_without_keys,
        disc.encrypted,
        !keyed,
    ) && let Err(e) = rip_keys
    {
        let at = (&disc, &key_scope);
        let (retried, reach) =
            retry_online_keys_on_outage(device, &cfg_read, &mut session.drive, at, e, seed_verdict);
        rip_keys = retried;
        key_verdict = reach;
        keyed = rip_keyed(&disc, &key_scope, &rip_keys);
    }
    // FMTS forensic keys missing (or Pending for a live single-pass mux, which cannot ask
    // later) go to the FMTS gate below, not the base no-key decision.
    let fmts_missing = matches!(rip_keys, Err(libfreemkv::Error::FmtsKeyMissing))
        || (!uses_multipass(cfg_read.max_retries)
            && rip_keys.as_ref().is_ok_and(|s| s.forensic_pending()));

    // No-keys decision: a keyless encrypted disc can still be swept to a raw
    // ISO (only the mux needs keys). `capture_without_keys` decides: enabled
    // → capture now, defer mux; disabled → don't rip, surface the reason.
    let keys_missing = !keyed && !fmts_missing;
    if keys_missing {
        // A persistent outage is NOT a missing key: park in a retryable/pending
        // state so a later insert/rescan retries. ONLY transient verdicts park —
        // a definitive answer falls through below, re-asking cannot change it.
        if let Some(status_msg) = key_verdict.and_then(key_service_transient_status) {
            crate::server::log::device_log(device, &format!("Not ripping now — {status_msg}"));
            update_state_with(device, |s| {
                s.status = "idle".to_string();
                s.key_status = status_msg.clone();
                s.last_error = status_msg.clone();
            });
            unregister_halt(device);
            return;
        }
        // What the service actually said beats the library's collapsed E7028
        // "could not be reached" code, which is only true for an outage.
        let msg = match (key_verdict.and_then(key_service_no_key_reason), &rip_keys) {
            (Some(reason), _) => format!("No keys — {reason}"),
            (None, Err(e)) if disc.aacs_error.is_none() => aacs_failure_message(Some(e)),
            (None, _) => keyless_failure_message(&disc),
        };
        if cfg_read.capture_without_keys {
            crate::server::log::device_log(
                device,
                &format!(
                    "{msg}\nNo keys yet — capturing to ISO; mux deferred until keys are available."
                ),
            );
        } else {
            crate::server::log::device_log(
                device,
                &format!(
                    "{msg}\nNo keys — not ripping. Enable \"capture without keys\" to save an ISO for later."
                ),
            );
            update_state_with(device, |s| {
                s.status = "error".to_string();
                s.last_error = keyless_not_ripping_error(&msg);
            });
            unregister_halt(device);
            return;
        }
    }

    // Probe for speed — only needed for rip, not scan
    if !session.probed {
        crate::server::log::device_log(device, "Probing disc speed...");
        let _ = session.drive.probe_disc();
        session.probed = true;
    }

    // Detect the kernel-reported max batch size (fallback: 60 sectors).
    // Pre-fix this was hardcoded to 1, misleading the API's `current_batch`
    // display and making the mux phase read the ISO one sector at a time.
    let batch = libfreemkv::disc::detect_max_batch_sectors(device_path);
    let format = disc.content_format;

    let output_format = cfg_read.output_format.clone();

    // ISO output needs whole-disc-scoped abort accounting, but single-pass
    // streams only the selected title and never produces a whole-disc ISO.
    // Refuse the incoherent combination and point at multi-pass instead.
    if iso_output_needs_multipass(&output_format, cfg_read.max_retries) {
        crate::server::log::device_log(
            device,
            "ISO output requires multi-pass mode — single-pass streams only the \
             selected title and cannot capture a whole-disc image. Enable multi-pass \
             mode (Retry Passes > 0) to rip an ISO.",
        );
        update_state_with(device, |s| {
            s.status = "error".to_string();
            if s.last_error.is_empty() {
                s.last_error =
                    "ISO output requires multi-pass mode (enable Retry Passes).".to_string();
            }
        });
        unregister_halt(device);
        return;
    }

    let ext = output_extension_for(&output_format, &disc);

    // `disc_name` is the RAW volume label, the only thing distinguishing two
    // discs of a boxset behind one shared TMDB title. Must resolve to the
    // same dir disc_staging_hold/find_resumable_for_disc just checked.
    let staging = cfg_read.staging_device_dir(&staging::staging_basename(
        std::path::Path::new(&cfg_read.staging_dir),
        &display_name,
        &disc_name,
    ));
    // Under an unmounted share the dir would be made on the container's own disk.
    let made = if crate::server::health::share_unmounted(std::path::Path::new(&staging)) {
        Err(std::io::Error::other("its network share is not mounted"))
    } else {
        std::fs::create_dir_all(&staging)
    };
    if let Err(e) = made {
        // Bail loudly instead of pressing on: a missing staging dir
        // makes the free-space preflight skip its check and the sweep
        // later dies with a confusing ENOENT/EACCES far from the cause.
        crate::server::log::device_log(
            device,
            &format!("Cannot create staging dir {staging}: {e}"),
        );
        update_state_with(device, |s| {
            s.status = "error".to_string();
            if s.last_error.is_empty() {
                s.last_error = format!("cannot create staging dir: {e}");
            }
        });
        unregister_halt(device);
        return;
    }
    // Stamp the dir with this disc's raw volume label so the NEXT disc of the
    // same boxset routes to its own dir instead of reading this `.completed`.
    // Also adopts a legacy pre-label dir; never overwrites a different label.
    staging::adopt_disc_label(std::path::Path::new(&staging), &disc_name);
    // Write `.sweeping` before Pass 1 to govern the whole sweep+patch window;
    // without it a crash mid-sweep leaves the dir ungoverned (restart-count
    // toward `.failed`, mover WARN-floods). Replaced by `.ripped`/`.failed`.
    if let Some(why) = staging::seed_sweeping_for_live_rip(std::path::Path::new(&staging)) {
        crate::server::log::device_log(
            device,
            &format!(
                "Replaced an unreadable state.json ({why}); the plan is rebuilt from the disc. The old file is kept as {}.",
                staging::UNREADABLE_STATE_ASIDE
            ),
        );
        crate::server::muxer::clear_error_with_prefix(&staging, staging::STATE_HELD_PREFIX);
    }
    // RAII cleanup for `.sweeping`: terminal-marker writers clear it first,
    // so this only fires on error/panic, preventing a stale `.sweeping` from
    // stranding the dir InProgress across restarts.
    let _sweeping_guard = SweepingGuard {
        staging: std::path::PathBuf::from(&staging),
    };
    // FILE names, NOT the dir basename — plain sanitize(display_name), no
    // `_2` disc suffix (the dir already separates discs). delete_partial_output
    // and the mover's TV/fallback delivery both key off this exact form.
    let filename = format!(
        "{}.{}",
        crate::server::util::sanitize_path_compact(&display_name),
        ext
    );
    let output_path = format!("{}/{}", staging, filename);
    // Intermediate-ISO + mapfile paths for multipass, derived once here
    // (previously rebuilt at ~5 scattered sites). Plain title, no disc
    // suffix — same reasoning as `filename` above.
    let iso_filename = format!(
        "{}.iso",
        crate::server::util::sanitize_path_compact(&display_name)
    );
    let iso_path_str = format!("{staging}/{iso_filename}");
    let mapfile_path_str = format!("{iso_path_str}.mapfile");
    let dest_url = if staging::is_network_output(&output_format, &cfg_read.network_target) {
        format!("network://{}", cfg_read.network_target)
    } else {
        // Scheme is the container (mkv/m2ts), NOT the filename extension: a 3D rip
        // writes `Title.mk3d` but must still mux through `mkv://` (no `mk3d://` scheme).
        format!("{}://{}", output_scheme_for(&output_format), output_path)
    };

    // The file this rip delivers: an ISO output's deliverable is the image itself.
    let delivered_file = delivered_file_name(&output_format, &filename, &iso_filename).to_string();
    crate::server::log::device_log(
        device,
        &format!("Ripping {} to {}", display_name, delivered_file),
    );
    // A single-pass rip runs the server plan as parsed; multipass logs the plan it runs below.
    if !uses_multipass(cfg_read.max_retries) {
        crate::server::log::device_log(
            device,
            &plan_line(&server_plan(&cfg_read, device_path, &dest_url)),
        );
    }

    update_state(
        device,
        RipState {
            device: device.to_string(),
            status: "ripping".to_string(),
            disc_present: true,
            disc_name: display_name.clone(),
            // Set explicitly rather than relying on `update_state`'s carry —
            // an operator title override changes `disc_name` mid-flight, which
            // (correctly) suppresses the carry, and the label must survive it.
            disc_label: disc_name.clone(),
            disc_format: disc_format.clone(),
            output_file: delivered_file.clone(),
            tmdb_title: tmdb_title.clone(),
            tmdb_year,
            tmdb_poster: tmdb_poster.clone(),
            tmdb_overview: tmdb_overview.clone(),
            duration: duration.clone(),
            codecs: codecs.clone(),
            ..Default::default()
        },
    );

    // Per-title bitrate for lost-video-time display: the engine's one conversion.
    let title_bytes_per_sec: f64 = freemkv_engine::title_bytes_per_sec(&title);

    // Shared state read by event callbacks and the rip loop (copies atomics
    // into RipState every ~1s). The watchdog timestamp updates on ANY sector
    // event, not just frame writes, so skipped sectors aren't seen as stalled.
    let wd_last_frame = Arc::new(AtomicU64::new(crate::server::util::epoch_secs()));
    let latest_bytes_read = Arc::new(AtomicU64::new(0));
    let rip_last_lba = Arc::new(AtomicU64::new(0));
    let rip_current_batch = Arc::new(AtomicU16::new(batch));

    // Wire the drive's halt-flag into the per-device Halt token, swapping
    // the top-of-function placeholder for one viewing the same AtomicBool
    // the drive's recovery loops poll — so cancel() reaches libfreemkv too.
    let drive_halt_arc = session.drive.halt_flag();
    let halt_token = libfreemkv::Halt::from_arc(drive_halt_arc.clone());
    // Carry a Stop that landed on the OLD placeholder token during this
    // window, else the first click would cancel a token nobody reads again.
    // Check+insert+carry happens under one HALTS-lock acquisition (TOCTOU).
    swap_halt_carrying_cancel(device, halt_token.clone());
    // The drive's raw halt flag, read directly by the Stop checks below.
    let halt = drive_halt_arc;

    // Re-read under a poison-recovering lock, like every other cfg read in this file.
    let transport_recovery_delay_secs = cfg
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .transport_recovery_delay_secs;

    // The user-stop halt — the existing flag; the passes observe it through the engine.
    let user_halt = halt.clone();

    // Multi-pass (max_retries > 0) goes through an ISO intermediate before
    // mux; single-pass streams disc→MKV directly. Lifted out of the
    // multipass branch so the outer-scope mux loop can reference it.
    let total_passes: u8 = plan_passes(cfg_read.max_retries).total_passes;
    // Captured from the multipass branch so the mux call site can pass it
    // into MuxInputs for total-progress weighting; stays 0 in single-pass.
    let mut bytes_unreadable_at_mux: u64 = 0;
    // Damage snapshot from the final sweep/patch pass, carried forward into
    // every mux-phase push_state call so /api/state damage fields don't
    // zero out the moment mux starts. Defaults (all-zero) for direct mode.
    let mut sweep_damage_snapshot = mux::SweepDamageSnapshot::default();
    // In-title loss from the abort gate, hoisted so the final status=done
    // update reuses it instead of recomputing from whole-disc bytes_unreadable
    // (which inflates the card when out-of-title menus are scratched).
    let mut main_lost_ms_for_history_outer = 0.0f64;

    // FMTS CaptureOnly deferral: set when the rip's set lacks the forensic keys with
    // capture-without-keys on. Muxing now would emit garbage — defer and preserve the ISO.
    let mut defer_forensic_mux = false;

    // FMTS forensic keys came with the rip's up-front set (KU §5): the gate decides from it
    // before the sweep, instead of after an hour-long one. A multipass rip whose set left
    // them Pending proceeds: its image asks for them once at the mux (KU §5.4).
    if disc.format == libfreemkv::DiscFormat::Fmts && key_scope != libfreemkv::keys::KeyScope::None
    {
        let gate = fmts_gate_decision(!fmts_missing, cfg_read.capture_without_keys);
        // Pure side-effect routing (unit-tested via `fmts_gate_plan`): CaptureOnly sets
        // the deferred-mux flag; Skip quarantines the staging dir. Driving both from the
        // plan keeps the gate's behavior mutation-verifiable.
        let plan = fmts_gate_plan(gate);
        defer_forensic_mux = plan.defer_forensic_mux;
        match gate {
            FmtsGate::Proceed => {
                crate::server::log::device_log(
                    device,
                    "FMTS: forensic keys resolved up front with the rip's key set.",
                );
            }
            FmtsGate::CaptureOnly => {
                // `defer_forensic_mux` is now set, so the mux-skip below (or
                // resume_remux's re-defer) arranges the deferral this log
                // promises — previously a no-op that muxed base-only garbage.
                crate::server::log::device_log(
                    device,
                    fmts_capture_only_log(uses_multipass(cfg_read.max_retries)),
                );
            }
            FmtsGate::Skip => {
                crate::server::log::device_log(
                    device,
                    "FMTS: forensic keys missing — not ripping. Enable \
                         \"capture without keys\" to save an ISO for later.",
                );
                // Clean up like every sibling early-failure exit (write
                // `.failed` + clear restart count) instead of leaving an
                // orphaned dir. Driven by plan.quarantine (unit-tested).
                if plan.quarantine {
                    let staging_disc_path = std::path::Path::new(&staging);
                    quarantine_or_log(
                        device,
                        staging_disc_path,
                        "FMTS forensic keys missing — not ripping.",
                    );
                    staging::clear_restart_count(staging_disc_path);
                }
                update_state_with(device, |s| {
                    s.status = "error".to_string();
                    s.last_error = "FMTS forensic keys missing — not ripping.".to_string();
                });
                unregister_halt(device);
                return;
            }
        }
    }

    // An ISO deliverable is decrypted in place by the passes, like the CLI's and GUI's.
    let iso_decrypts = output_is_iso_image(&cfg_read.output_format);
    let mux_source = if uses_multipass(cfg_read.max_retries) {
        let iso_path = std::path::Path::new(&iso_path_str);
        let bytes_total_disc = (session.drive.read_capacity().unwrap_or(0) as u64) * 2048;

        // Pre-flight: require enough free space for the remaining ISO data and
        // the planned mux outputs, else a too-small disk ENOSPCs mid-rip.
        // AUTORIP_SKIP_DISKCHECK=1 bypasses this for diagnostics only.
        let skip_diskcheck =
            skip_diskcheck_value(std::env::var("AUTORIP_SKIP_DISKCHECK").ok().as_deref());
        if bytes_total_disc == 0 && !skip_diskcheck {
            // read_capacity() returned 0/unknown, so the image size is
            // uncomputable; tell the operator why the check didn't run.
            crate::server::log::device_log(
                device,
                "disk-space preflight skipped: drive reported unknown capacity (read_capacity=0); \
                 a too-small staging volume will ENOSPC mid-rip",
            );
        }
        if bytes_total_disc > 0 && !skip_diskcheck {
            let remaining_iso_bytes = if resume_sweep {
                resume_remaining_iso_bytes(
                    std::path::Path::new(&mapfile_path_str),
                    iso_path,
                    bytes_total_disc,
                )
            } else {
                None
            };
            let title_output_bytes = mux_output_reserve_bytes(
                &disc.titles,
                &cfg_read,
                &tmdb_media_type,
                &disc_name,
                title.size_bytes,
            );
            let required = disk_space_required_bytes(
                bytes_total_disc,
                title_output_bytes,
                remaining_iso_bytes,
            );
            if let Some(avail) = staging_free_bytes(&staging) {
                if avail < required {
                    let msg = disk_space_preflight_message(required, &staging, avail);
                    crate::server::log::device_log(device, &msg);
                    update_state_with(device, |s| {
                        s.status = "error".to_string();
                        s.last_error = msg.clone();
                    });
                    unregister_halt(device);
                    drop_session(device);
                    return;
                }
            } else {
                // statvfs failed (missing path / unmounted volume / non-POSIX
                // fs), so free space can't be computed; tell the operator why
                // rather than silently skipping. Mirrors the unknown-capacity branch.
                crate::server::log::device_log(
                    device,
                    &format!(
                        "disk-space preflight skipped: could not read free space at {} \
                         (path missing or volume not mounted?); a too-small or unmounted \
                         staging volume will ENOSPC mid-rip",
                        staging,
                    ),
                );
            }
        }

        // Shared pass context + title reference for progress callbacks.
        let pass_ctx = PassContext {
            device: device.to_string(),
            display_name: display_name.clone(),
            disc_format: disc_format.clone(),
            tmdb_title: tmdb_title.clone(),
            tmdb_year,
            tmdb_poster: tmdb_poster.clone(),
            tmdb_overview: tmdb_overview.clone(),
            tmdb_media_type: tmdb_media_type.clone(),
            duration: duration.clone(),
            codecs: codecs.clone(),
            filename: delivered_file.clone(),
            batch,
            bytes_total_disc,
            max_retries: cfg_read.max_retries,
        };
        let title_for_progress = title.clone();
        let bps_progress = title_bytes_per_sec;

        // The engine's recovery over the held drive (shared with the CLI and app); `passes`
        // re-opens the drive and spin-cycles it. An ISO output decrypts; a staged image stays
        // raw for the mux, the rip's set stamping its identity.
        let mapfile_path = std::path::PathBuf::from(&mapfile_path_str);
        let pass_sink = passes::ServerPassSink::new(
            &pass_ctx,
            &title_for_progress,
            bps_progress,
            total_passes,
            output_is_iso_image(&cfg_read.output_format),
            &mapfile_path,
            user_halt.clone(),
        );
        let mut host = passes::ServerPassHost {
            device,
            device_path,
            session: &mut session,
            halt: halt.clone(),
            user_halt: user_halt.clone(),
            delay_secs: transport_recovery_delay_secs,
            resume: resume_sweep,
            attempt: 0,
            gave_up: false,
            halted: false,
        };
        let plan = freemkv_engine::Plan {
            source: format!("disc://{device_path}"),
            dest: format!("iso://{iso_path_str}"),
            titles: freemkv_engine::Selection::Titles(vec![main_idx]),
            raw: !iso_decrypts,
            multipass: true,
            ..freemkv_engine::Plan::default()
        };
        crate::server::log::device_log(device, &plan_line(&plan));
        let with = freemkv_engine::RunWith {
            keys: rip_keys.as_ref().ok().cloned(),
            held: Some(freemkv_engine::Held::Host {
                disc: &disc,
                host: &mut host,
            }),
            passes: Some(freemkv_engine::MultipassOpts {
                max_passes: u32::from(cfg_read.max_retries),
                abort_on_lost_secs: cfg_read.abort_on_lost_secs,
                is_iso_output: output_is_iso_image(&cfg_read.output_format),
            }),
            // The staged ISO carries no artifact lock: staging markers govern it.
            locked: true,
            halt: Some(halt_token.clone()),
            ..freemkv_engine::RunWith::default()
        };
        let recovered = freemkv_engine::run_with(&plan, with, &pass_sink);
        let (attempt, gave_up, halted_in_recovery) = (host.attempt, host.gave_up, host.halted);
        let result = match recovered {
            Ok(freemkv_engine::Report::Image {
                recovery: Some(r), ..
            }) => r,
            Ok(_) => {
                tracing::error!(device = %device, "recovery returned no pass verdict");
                let msg = "Internal error: recovery returned no pass verdict";
                crate::server::log::device_log(device, msg);
                update_state_with(device, |s| {
                    s.status = "error".to_string();
                    s.last_error = msg.to_string();
                });
                unregister_halt(device);
                return;
            }
            // A Stop during a transport recovery was already logged.
            Err(_) if halted_in_recovery => return,
            Err(e) if halt.load(Ordering::Relaxed) => {
                crate::server::log::device_log(device, &format!("Pass 1 cancelled (halt): {e}"));
                // `_halt_guard` unregisters this device's Halt token on drop (i.e. on this
                // `return`); no explicit call needed.
                return;
            }
            Err(e) if !gave_up => {
                crate::server::log::device_log(device, &format!("Pass 1 failed: {e}"));
                let user_msg = pass1_last_error(&e);
                update_state(
                    device,
                    RipState {
                        device: device.to_string(),
                        status: "error".to_string(),
                        disc_present: true,
                        last_error: user_msg,
                        disc_name: display_name.clone(),
                        disc_format: disc_format.clone(),
                        tmdb_title: tmdb_title.clone(),
                        tmdb_year,
                        tmdb_poster: tmdb_poster.clone(),
                        tmdb_overview: tmdb_overview.clone(),
                        duration: duration.clone(),
                        codecs: codecs.clone(),
                        ..Default::default()
                    },
                );
                unregister_halt(device);
                return;
            }
            Err(e) => {
                // All attempts exhausted or unrecoverable.

                // Determine which recovery strategy failed and why
                let failure_reason = if attempt >= passes::MAX_PASS1_ATTEMPTS {
                    "transport_failure_recovery_exhausted".to_string()
                } else {
                    "unrecoverable_error".to_string()
                };

                crate::server::log::device_log(
                    device,
                    &format!(
                        "Pass 1: recovery failed at attempt {}/{}, strategy={}",
                        // `attempt` is already 1-based (incremented at the top
                        // of the loop), so print it directly — `attempt + 1`
                        // overcounted, yielding e.g. "12/10" at exhaustion.
                        attempt.min(passes::MAX_PASS1_ATTEMPTS),
                        passes::MAX_PASS1_ATTEMPTS,
                        failure_reason
                    ),
                );

                // format_pass_error turns sense data into an actionable
                // message (e.g. "power-cycle the drive"); fall back to plain
                // text only if no error was captured.
                let user_msg = pass1_last_error(&e);

                update_state(
                    device,
                    RipState {
                        device: device.to_string(),
                        status: "error".to_string(),
                        disc_present: true,
                        last_error: user_msg,
                        disc_name: display_name.clone(),
                        disc_format: disc_format.clone(),
                        tmdb_title: tmdb_title.clone(),
                        tmdb_year,
                        tmdb_poster: tmdb_poster.clone(),
                        tmdb_overview: tmdb_overview.clone(),
                        duration: duration.clone(),
                        codecs: codecs.clone(),
                        ..Default::default()
                    },
                );

                // Log recovery guidance for user action based on failure type
                if failure_reason == "transport_failure_recovery_exhausted" {
                    crate::server::log::device_log(
                        device,
                        &format!(
                            "RECOVERY_GUIDANCE: Transport failure recovery exhausted after {} attempts. Check logs for specific error category (SCSI_ERROR, DEVICE_ERROR). If ILLEGAL REQUEST errors present, drive firmware wedged — eject disc and power-cycle USB drive before retrying.",
                            passes::MAX_PASS1_ATTEMPTS
                        ),
                    );

                    crate::server::log::device_log(
                        device,
                        &format!(
                            "NEXT_STEPS: 1) Check /api/logs/{device} for STRATEGY_FAILURE entries. 2) Identify which phase failed (Drive::open/wait_ready/init). 3) If firmware wedged, power-cycle the drive and retry.",
                        ),
                    );
                } else {
                    crate::server::log::device_log(
                        device,
                        "RECOVERY_GUIDANCE: Unrecoverable error occurred before transport failure recovery could complete. Check logs for first ERROR entry to identify root cause.",
                    );
                }

                unregister_halt(device);
                return;
            }
        };
        let bytes_unreadable = result.unreadable_bytes;

        // End-of-recovery promotion (multi-pass only) ran in the engine; a user STOP skips it
        // so un-retried ranges stay resumable.
        if result.halted || user_halt.load(Ordering::Relaxed) {
            crate::server::log::device_log(
                device,
                "Rip stopped by user — preserving partial sweep for resume.",
            );
            unregister_halt(device);
            return;
        }

        let mut main_lost_ms_for_history = 0.0f64;
        if uses_multipass(cfg_read.max_retries) {
            // The engine's one loss verdict.
            main_lost_ms_for_history = result.main_lost_ms;
            // Mirror into the outer binding so the final done/stopped state update (after
            // run_mux) can use the same in-title value without re-reading the mapfile.
            main_lost_ms_for_history_outer = main_lost_ms_for_history;

            // ISO output is whole-disc and must be byte-complete: the per-title
            // tolerance is ignored (forced to 0). MKV/M2TS use the configured value.
            let effective_abort =
                effective_abort_secs(&cfg_read.output_format, cfg_read.abort_on_lost_secs);
            if result.aborted_for_loss {
                crate::server::log::device_log(
                    device,
                    &format!(
                        "ABORT: strategy=abort_check triggered — {:.2}s lost in main movie (threshold: {}s)",
                        main_lost_ms_for_history / MILLIS_PER_SEC,
                        effective_abort
                    ),
                );

                crate::server::log::device_log(
                    device,
                    &format!(
                        "STRATEGY_FAILURE: abort_check FAILED — data loss ({:.2}s) exceeds threshold ({}s)",
                        main_lost_ms_for_history / MILLIS_PER_SEC,
                        effective_abort
                    ),
                );

                crate::server::log::device_log(
                    device,
                    &if output_is_iso_image(&cfg_read.output_format) {
                        "RECOVERY_GUIDANCE: ISO output is a whole-disc image and requires 100% — abort_on_lost_secs does not apply (it is a MUXED-output setting, ignored for ISO). The loss is unrecoverable media: clean or replace the disc, or choose MKV output to tolerate non-title damage.".to_string()
                    } else if effective_abort == 0 {
                        "RECOVERY_GUIDANCE: abort_on_lost_secs=0 requires a perfect rip — ANY unrecoverable loss in the main movie aborts here. To let a rip complete despite some loss, RAISE abort_on_lost_secs to the number of seconds of main-movie loss you can tolerate (e.g. 5 or 30).".to_string()
                    } else {
                        format!(
                            "RECOVERY_GUIDANCE: abort_on_lost_secs={}s limit exceeded — raise abort_on_lost_secs further or accept the loss after disc recovery.",
                            effective_abort
                        )
                    },
                );
                update_state_with(device, |s| {
                    s.status = "error".to_string();
                    // Surface the Accept-damage off-ramp: the complete ISO is on
                    // disk as a resumable `.aborted-loss`, so the operator can
                    // deliver it as-is instead of re-ripping.
                    s.loss_aborted = true;
                    if s.last_error.is_empty() {
                        s.last_error = format!(
                            "aborted — {} lost in main movie ({})",
                            fmt_loss(main_lost_ms_for_history),
                            fmt_threshold(effective_abort)
                        );
                    }
                });
                // Record the abort as RESUMABLE `.aborted-loss`, not `.failed`:
                // deterministic media damage a plain re-rip won't fix.
                record_rip_loss_abort(
                    device,
                    std::path::Path::new(&staging),
                    &format!(
                        "aborted: {} lost in main movie ({})",
                        fmt_loss(main_lost_ms_for_history),
                        fmt_threshold(effective_abort)
                    ),
                );
                unregister_halt(device);
                return; // Skip mux entirely
            }

            if main_lost_ms_for_history > 0.0 {
                crate::server::log::device_log(
                    device,
                    &format!(
                        "Main movie loss after retries: {:.2}s (threshold: {}s)",
                        main_lost_ms_for_history / MILLIS_PER_SEC,
                        effective_abort
                    ),
                );
            } else {
                crate::server::log::device_log(device, "All data recovered — proceeding with mux.");
            }
        }

        // Mux gating: skip mux + return cleanly if user pressed stop.
        if user_halt.load(Ordering::Relaxed) {
            crate::server::log::device_log(device, "Rip cancelled — skipping mux.");
            unregister_halt(device);
            return;
        }
        // Passes are bounded only by stall watchdogs (per-pass cap removed).
        // ISO output: skip the title mux, hand over `<name>.iso` directly.
        if output_is_iso_image(&cfg_read.output_format) {
            let iso_path = std::path::Path::new(&iso_path_str);
            // Durability gate mirroring the MKV path: a crash must not leave a
            // `.done` pointing at a page-cache-only ISO. If fsync fails, withhold
            // the markers and preserve staging for retry.
            if !staging::durability_gate_passes(false, || staging::fsync_output_file(iso_path)) {
                crate::server::log::device_log(
                    device,
                    "Durability gate failed: could not fsync ISO image to stable storage; \
                     withholding .done/.completed and preserving staging for retry",
                );
                update_state_with(device, |s| {
                    if s.last_error.is_empty() {
                        s.last_error =
                            "ISO image not durable (fsync failed); rip preserved for retry"
                                .to_string();
                    }
                });
                unregister_halt(device);
                return;
            }
            let staging_path = std::path::Path::new(&staging);
            // Confident match → `state: Done`; otherwise `state: Review`. One
            // `state.json` transition carries mover metadata + the ISO output,
            // plus TV metadata so the mover can fold it under `Show (Year)/Season NN/`.
            let marker_name = staging::handoff_label(title_confident);
            let iso_leaf = iso_filename.clone();
            if let Err(e) = staging::mark_handoff(staging_path, title_confident, |s| {
                s.title = display_name.clone();
                s.disc_name = disc_name.clone();
                s.disc_format = disc_format.clone();
                s.year = tmdb_year;
                s.media_type = tmdb_media_type.clone();
                s.tmdb_id = tmdb_id;
                s.tmdb_poster = tmdb_poster.clone();
                s.tmdb_overview = tmdb_overview.clone();
                s.season = crate::server::tmdb::season_from_label(&disc_name);
                s.disc_number = crate::server::tmdb::disc_from_label(&disc_name);
                s.outputs = vec![staging::Output {
                    filename: iso_leaf,
                    ..Default::default()
                }];
            }) {
                crate::server::log::device_log(
                    device,
                    &format!(
                        "{marker_name} state write failed ({e}); ISO is staged but the mover cannot pick it up"
                    ),
                );
                update_state_with(device, |s| {
                    if s.last_error.is_empty() {
                        s.last_error = format!("{marker_name} state write failed: {e}");
                    }
                });
                unregister_halt(device);
                return;
            }
            staging::write_completed_marker(staging_path);
            staging::clear_restart_count(staging_path);
            crate::server::log::device_log(
                device,
                &format!("ISO output complete — disc image staged as {iso_filename}"),
            );
            update_state_with(device, |s| {
                s.status = "done".to_string();
                s.output_file = iso_filename.clone();
            });
            // Rip stage done (ISO delivery — no mux stage follows; the ISO is
            // the deliverable and the mover fires move_complete later). Fire
            // the drive-free hook at the eject decision point.
            fire_rip_complete_webhook(
                &cfg_read,
                device,
                &display_name,
                &disc_format,
                &tmdb_poster,
                tmdb_year,
                &duration,
                &codecs,
                &iso_path_str,
            );
            if should_auto_eject(cfg_read.auto_eject, device) {
                if let Some(h) = device_halt(device) {
                    h.cancel();
                }
                drop(session);
                eject_drive(device_path);
            } else {
                drop(session);
                unregister_halt(device);
            }
            return;
        }

        // v0.25.3 parallel pipeline hand-off: write `.ripped` so the muxer worker
        // picks up staging; mux/post-mux now runs in `remux_from_ripped_marker`.
        // Snapshot post-promotion damage into the marker for resume to restore.
        let marker_damage = sweep_damage_from_state(device);
        let marker = crate::server::muxer::RippedMarker {
            schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
            iso_path: iso_path_str.clone(),
            mapfile_path: mapfile_path_str.clone(),
            display_name: display_name.clone(),
            disc_format: disc_format.clone(),
            mkv_filename: filename.clone(),
            tmdb_title: tmdb_title.clone(),
            tmdb_year,
            tmdb_poster: tmdb_poster.clone(),
            tmdb_overview: tmdb_overview.clone(),
            tmdb_media_type: tmdb_media_type.clone(),
            max_retries: cfg_read.max_retries,
            abort_on_lost_secs: cfg_read.abort_on_lost_secs as u32,
            rip_elapsed_secs: 0.0, // mux worker re-derives elapsed from its own start
            rip_errors: 0,
            rip_lost_video_secs: main_lost_ms_for_history / MILLIS_PER_SEC,
            rip_last_sector: rip_last_lba.load(Ordering::Relaxed),
            origin_device: device.to_string(),
            sweep_errors: marker_damage.as_ref().map(|d| d.errors).unwrap_or(0),
            sweep_total_lost_ms: marker_damage
                .as_ref()
                .map(|d| d.total_lost_ms)
                .unwrap_or(0.0),
            sweep_main_lost_ms: marker_damage
                .as_ref()
                .map(|d| d.main_lost_ms)
                .unwrap_or(0.0),
            sweep_num_bad_ranges: marker_damage
                .as_ref()
                .map(|d| d.num_bad_ranges)
                .unwrap_or(0),
            sweep_largest_gap_ms: marker_damage
                .as_ref()
                .map(|d| d.largest_gap_ms)
                .unwrap_or(0.0),
            // Carry the fresh-rip confidence verdict (folds in the operator
            // override) so resume_remux doesn't second-guess a deliberate pick.
            title_confident,
        };
        // TV-routing metadata `RippedMarker` doesn't carry, plus the deliverable PLAN
        // (`outputs[]`), so it propagates through mux/resume into the mover.
        let plan = plan_mux_outputs(
            &disc.titles,
            &cfg_read,
            &tmdb_media_type,
            &disc_name,
            tmdb_id,
            &filename,
        );
        let staging_path = std::path::Path::new(&staging);
        if let Err(e) = hand_off_to_mux_worker(staging_path, &marker, rip_keys.as_ref().ok(), |s| {
            s.tmdb_id = tmdb_id;
            s.disc_name = disc_name.clone();
            s.season = crate::server::tmdb::season_from_label(&disc_name);
            s.disc_number = crate::server::tmdb::disc_from_label(&disc_name);
            s.outputs = plan;
        }) {
            // Couldn't hand off — fall back to the inline mux below
            // by NOT taking the early-return branch. Log the failure
            // so the cause is on the device log.
            crate::server::log::device_log(
                device,
                &format!(".ripped marker write failed ({e}); falling back to inline mux"),
            );
        } else {
            crate::server::log::device_log(
                device,
                "Sweep + patch complete; handed off to mux worker via .ripped marker.",
            );
            // Status: "done" — the DISC READ is complete; mux is a SEPARATE phase
            // tracked via the synthetic `_mux` device, which can never revert
            // this tile back to "ripping" (previously it did). Carry damage fields too.
            let row = handoff_done_row(
                device,
                RipState {
                    device: device.to_string(),
                    output_file: filename.clone(),
                    disc_present: true,
                    disc_name: display_name.clone(),
                    disc_format: disc_format.clone(),
                    tmdb_title: tmdb_title.clone(),
                    tmdb_year,
                    tmdb_poster: tmdb_poster.clone(),
                    tmdb_overview: tmdb_overview.clone(),
                    duration: duration.clone(),
                    codecs: codecs.clone(),
                    ..Default::default()
                },
            );
            update_state(device, row);
            // Rip stage done: the ISO is staged and the drive is now free.
            // Fire the drive-free hook here, at the eject decision point,
            // BEFORE the separate mux worker later fires mux_complete.
            fire_rip_complete_webhook(
                &cfg_read,
                device,
                &display_name,
                &disc_format,
                &tmdb_poster,
                tmdb_year,
                &duration,
                &codecs,
                &iso_path_str,
            );
            if should_auto_eject(cfg_read.auto_eject, device) {
                // eject_drive handles drain + drop_session + unregister_halt
                // internally. Cancel the halt first so any in-flight work
                // exits cleanly before the eject SCSI command issues.
                if let Some(h) = device_halt(device) {
                    h.cancel();
                }
                drop(session);
                eject_drive(device_path);
            } else {
                drop(session);
                unregister_halt(device);
            }
            return;
        }

        // Fallback inline-mux path (only reached if the marker write
        // above failed). Closes drive, opens ISO, runs mux as before.
        crate::server::log::device_log(device, "Drive released; muxing ISO → MKV.");
        // Rip stage done even on the fallback path: the drive is released here,
        // before the inline mux runs, so fire the drive-free hook now (the
        // inline mux fires mux_complete when the .mkv is written, below).
        fire_rip_complete_webhook(
            &cfg_read,
            device,
            &display_name,
            &disc_format,
            &tmdb_poster,
            tmdb_year,
            &duration,
            &codecs,
            &iso_path_str,
        );
        drop(session);

        // Capture bytes_unreadable for the mux call site (outside this branch),
        // used to size the total-progress denominator once retries are done.
        bytes_unreadable_at_mux = bytes_unreadable;

        // Entering mux phase — push final mapfile state so the UI keeps the
        // bad-range list visible through mux and into the "done" view. The lib
        // builds the snapshot from the mapfile (autorip never parses it).
        let mux_state = std::sync::Mutex::new(PassProgressState::new());
        if let Some(snap) = freemkv_engine::progress_snapshot_from_mapfile(
            std::path::Path::new(&mapfile_path_str),
            Some(&title_for_progress),
            libfreemkv::progress::PassKind::Mux,
            pass_ctx.bytes_total_disc,
        ) {
            push_pass_state(
                &pass_ctx,
                &snap,
                bps_progress,
                total_passes,
                total_passes,
                &mux_state,
            );
        }
        // Snapshot the damage fields just written to STATE so mux carries them
        // forward each tick; without it, push_state's Default would zero them.
        sweep_damage_snapshot = sweep_damage_from_state(device).unwrap_or_default();
        MuxSource::StagedImage
    } else {
        MuxSource::Drive(Box::new(session.drive))
    };

    // Keyless-capture mux-skip: keys are missing, so muxing now would write
    // garbage — SKIP mux and PRESERVE staging for a deferred mux. Multipass
    // normally already returned via `.ripped`; single-pass has no ISO to defer to.
    if keys_missing || defer_forensic_mux {
        let msg = keyless_failure_message(&disc);
        if uses_multipass(cfg_read.max_retries) {
            let (log_line, state_err) = if defer_forensic_mux {
                (
                    format!(
                        "Ripped to ISO — forensic keys unavailable, mux deferred. ISO + mapfile \
                         preserved in staging ({staging}); auto-resume will mux once keys are available."
                    ),
                    "Ripped to ISO — forensic keys unavailable, mux deferred.".to_string(),
                )
            } else {
                (
                    format!(
                        "Ripped to ISO — no keys, mux deferred. ISO + mapfile preserved in staging \
                         ({staging}); auto-resume will mux once keys are available. {msg}"
                    ),
                    format!("Ripped to ISO — no keys, mux deferred. {msg}"),
                )
            };
            crate::server::log::device_log(device, &log_line);
            update_state_with(device, |s| {
                s.status = "idle".to_string();
                s.last_error = state_err;
            });
        } else {
            let (log_line, state_err) = if defer_forensic_mux {
                (
                    "FMTS single-pass rip — forensic keys unavailable, cannot mux (no ISO captured). \
                     Enable multi-pass mode to capture a deferred-mux ISO."
                        .to_string(),
                    "Forensic keys unavailable — cannot mux. \
                     (multi-pass mode captures an ISO for deferred mux.)"
                        .to_string(),
                )
            } else {
                (
                    format!(
                        "Single-pass rip with no keys — cannot mux (no ISO captured). \
                         Enable multi-pass mode to capture a deferred-mux ISO. {msg}"
                    ),
                    format!(
                        "Cannot mux without keys. {msg} (multi-pass mode captures an ISO for deferred mux.)"
                    ),
                )
            };
            crate::server::log::device_log(device, &log_line);
            update_state_with(device, |s| {
                s.status = "error".to_string();
                s.last_error = state_err;
            });
        }
        unregister_halt(device);
        return;
    }

    // Debug log reader type for mux - confirms ISO vs drive source
    tracing::debug!(target: "mux", " mux using reader: {}", if uses_multipass(cfg_read.max_retries) { "ISO file (multipass)" } else { "physical drive" });

    // DiscStream gets the per-device `Halt` at construction; Stop interrupts `fill_extents` at
    // the next retry boundary (dense bad-sector regions).
    let mux_total_bytes = mux_progress_denominator(cfg_read.max_retries, total_bytes, &title);

    let _mux_span =
        tracing::span!(tracing::Level::TRACE, "rip_disc::run_mux", device=%device, total_bytes)
            .entered();
    let mux_input_errors = Arc::new(AtomicU32::new(0));
    let mux_inputs = mux::MuxInputs {
        device,
        display_name: display_name.clone(),
        disc_format: disc_format.clone(),
        tmdb_title: tmdb_title.clone(),
        tmdb_year,
        tmdb_poster: tmdb_poster.clone(),
        tmdb_overview: tmdb_overview.clone(),
        duration: duration.clone(),
        codecs: codecs.clone(),
        filename: filename.clone(),
        total_bytes: mux_total_bytes,
        title_bytes_per_sec,
        total_passes,
        bytes_total_disc: disc.capacity_bytes,
        max_retries: cfg_read.max_retries,
        bytes_unreadable_at_mux,
        dest_url: dest_url.clone(),
        batch,
        // Hand the mux watchdog the per-disc staging dir so its
        // hard-escalation path (5-minute stall → exit + Docker
        // restart) can bump `.restart_count` before exiting.
        staging_disc_dir: std::path::PathBuf::from(&staging),
        sweep_damage: sweep_damage_snapshot.clone(),
    };
    let mux_atomics = mux::MuxAtomics {
        latest_bytes_read: latest_bytes_read.clone(),
        rip_last_lba: rip_last_lba.clone(),
        rip_current_batch: rip_current_batch.clone(),
        wd_last_frame: wd_last_frame.clone(),
        wd_bytes: Arc::new(AtomicU64::new(0)),
        input_errors: mux_input_errors,
    };

    let mux_outcome = match mux_source {
        MuxSource::StagedImage => {
            // Multipass: mux the staged ISO through the engine, keyed with what the
            // drive already resolved (FMTS forensic keys included) — no second lookup.
            let iso_path = std::path::Path::new(&iso_path_str);
            let staged_keys = match &rip_keys {
                Ok(set) => crate::server::keysource::StagedKeys::Rip(set.clone()),
                Err(_) => crate::server::keysource::StagedKeys::Resolve {
                    vid: disc
                        .aacs
                        .as_ref()
                        .map(|a| a.volume_id)
                        .filter(|v| *v != [0u8; 16]),
                },
            };
            let opened = freemkv_engine::scan_image(&freemkv_engine::ImageSource::Iso(
                iso_path.to_path_buf(),
            ))
            .and_then(|(image_disc, _)| {
                let idx = image_title_index(&image_disc, &title);
                crate::server::keysource::open_staged_image(
                    &cfg_read,
                    iso_path,
                    image_disc,
                    &[idx],
                    staged_keys,
                    Some(halt_token.clone()),
                )
            });
            let image = match opened {
                Ok(image) => {
                    crate::server::log::device_log(
                        device,
                        &format!(
                            "ISO opened successfully: {} sectors",
                            image.disc.capacity_sectors
                        ),
                    );
                    image
                }
                Err(libfreemkv::Error::Halted) => {
                    crate::server::log::device_log(
                        device,
                        "Rip stopped by user while keying the ISO — staging preserved for resume.",
                    );
                    unregister_halt(device);
                    return;
                }
                Err(e) if is_key_refusal(&e) => {
                    // Keys, not the image: keep the ISO for a resume, never `.failed`.
                    let msg = format_lib_error("Open ISO", &e);
                    crate::server::log::device_log(
                        device,
                        &format!("{msg}\nStaging preserved ({staging}) for a resume."),
                    );
                    update_state_with(device, |s| {
                        s.status = "error".to_string();
                        s.last_error = msg.clone();
                    });
                    unregister_halt(device);
                    return;
                }
                Err(e) => {
                    let msg = format_lib_error("Open ISO", &e);
                    crate::server::log::device_log(device, &msg);
                    // Cannot open the ISO for mux — this repeats every startup, so
                    // quarantine with `.failed` and let restart classify it terminal.
                    let staging_disc_path = std::path::Path::new(&staging);
                    quarantine_or_log(device, staging_disc_path, &msg);
                    staging::clear_restart_count(staging_disc_path);
                    update_state(
                        device,
                        RipState {
                            device: device.to_string(),
                            status: "failed".to_string(),
                            disc_present: true,
                            last_error: msg.clone(),
                            failure_reason: Some(msg),
                            disc_name: display_name,
                            disc_format,
                            tmdb_title,
                            tmdb_year,
                            tmdb_poster,
                            tmdb_overview,
                            duration,
                            codecs,
                            ..Default::default()
                        },
                    );
                    unregister_halt(device);
                    return;
                }
            };
            let iso_src = mux::IsoMuxSource {
                title_index: image_title_index(&image.disc, &title),
                image: &image,
            };
            match mux::mux_iso(mux_inputs, iso_src, mux_atomics) {
                Ok(o) => o,
                Err(e) => {
                    // A Stop pressed during the CSS crack surfaces as `Error::Halted`
                    // — a user halt, not structural: preserve staging, no `.failed`.
                    if is_halt_error(&e) {
                        crate::server::log::device_log(
                            device,
                            "Rip stopped by user during mux setup — staging preserved for resume.",
                        );
                        unregister_halt(device);
                        return;
                    }
                    // A key refusal is not the image's fault: keep the ISO for a resume.
                    if io_key_refusal(&e).is_some() {
                        let msg =
                            format!("Mux refused for keys — staging preserved for a resume ({e}).");
                        crate::server::log::device_log(device, &msg);
                        update_state_with(device, |s| {
                            s.status = "error".to_string();
                            s.last_error = msg.clone();
                        });
                        unregister_halt(device);
                        return;
                    }
                    // A pipeline BUILD failure is structural and permanent — retries
                    // won't fix it. Quarantine with `.failed` (mirrors header-phase path below).
                    tracing::error!(target: "mux", device=%device, "image mux setup failed: {e}");
                    let msg = format!(
                        "Mux setup failed — the disc's title or stream layout could not be prepared for muxing. The source may be damaged or use an unsupported format ({e})."
                    );
                    crate::server::log::device_log(device, &msg);
                    let staging_disc_path = std::path::Path::new(&staging);
                    quarantine_or_log(device, staging_disc_path, &msg);
                    staging::clear_restart_count(staging_disc_path);
                    update_state_with(device, |s| {
                        s.status = "failed".to_string();
                        s.last_error = msg.clone();
                        s.failure_reason = Some(msg.clone());
                    });
                    unregister_halt(device);
                    return;
                }
            }
        }
        MuxSource::Drive(reader) => {
            // Drive single-pass path (STEP 4c-ii): live inline `DiscStream` via
            // `mux_stream`. Stays INLINE, not the prefetch highway, since
            // `fill_extents`' adaptive batch-retry only fires on the inline reader.
            let live_src = mux::LiveMuxSource {
                reader,
                title: libfreemkv::ScannedTitle {
                    title,
                    format,
                    disc_name: Some(
                        disc.meta_title
                            .clone()
                            .unwrap_or_else(|| disc.volume_id.clone()),
                    ),
                    volume_id: disc.volume_id.clone(),
                },
                keys: rip_keys.as_ref().ok().cloned(),
                skip_errors: skip_read_errors(&cfg_read.on_read_error),
            };
            match mux::mux_live(mux_inputs, live_src, mux_atomics) {
                Ok(o) => o,
                Err(e) => {
                    // Same classification as the multipass branch: a Stop pressed
                    // during the CSS crack surfaces as `Error::Halted` — a user
                    // halt, not structural: preserve staging, no `.failed`.
                    if is_halt_error(&e) {
                        crate::server::log::device_log(
                            device,
                            "Rip stopped by user during mux setup — staging preserved for resume.",
                        );
                        unregister_halt(device);
                        return;
                    }
                    // A build failure (or a scrambled-but-uncrackable CSS DVD →
                    // CssKeyMissing) is structural — retries won't fix it. Quarantine
                    // with `.failed` (mirrors the multipass branch and header-phase path).
                    tracing::error!(target: "mux", device=%device, "mux_stream (live) setup failed: {e}");
                    let msg = format!(
                        "Mux setup failed — the disc's title or stream layout could not be prepared for muxing. The source may be damaged or use an unsupported format ({e})."
                    );
                    crate::server::log::device_log(device, &msg);
                    let staging_disc_path = std::path::Path::new(&staging);
                    quarantine_or_log(device, staging_disc_path, &msg);
                    staging::clear_restart_count(staging_disc_path);
                    update_state_with(device, |s| {
                        s.status = "failed".to_string();
                        s.last_error = msg.clone();
                        s.failure_reason = Some(msg.clone());
                    });
                    unregister_halt(device);
                    return;
                }
            }
        }
    };

    // Output never opened: `None` is a clean stop (halt/EOF pre-headers) —
    // preserve as resumable. `Some(msg)` means the stream was structurally
    // unusable — quarantine + surface it rather than leaving a dir resume can't fix.
    let header_phase = header_phase_disposition(
        mux_outcome.output_opened,
        mux_outcome.finalize_error.as_deref(),
    );
    if let HeaderPhase::ResumableStop | HeaderPhase::Failed(_) = header_phase {
        unregister_halt(device);
        if let HeaderPhase::Failed(reason) = header_phase {
            crate::server::log::device_log(device, &format!("Mux failed: {reason}"));
            let staging_disc_path = std::path::Path::new(&staging);
            quarantine_or_log(
                device,
                staging_disc_path,
                &format!("mux header phase failed: {reason}"),
            );
            staging::clear_restart_count(staging_disc_path);
            let failure_reason = Some(format!("mux header phase failed: {reason}"));
            update_state(
                device,
                RipState {
                    device: device.to_string(),
                    status: "failed".to_string(),
                    disc_present: true,
                    disc_name: display_name.clone(),
                    disc_format: disc_format.clone(),
                    tmdb_title: tmdb_title.clone(),
                    tmdb_year,
                    tmdb_poster: tmdb_poster.clone(),
                    tmdb_overview: tmdb_overview.clone(),
                    duration: duration.clone(),
                    codecs: codecs.clone(),
                    last_error: failure_reason.clone().unwrap_or_default(),
                    failure_reason,
                    ..Default::default()
                },
            );
        }
        return;
    }

    // Clean up halt flag
    unregister_halt(device);

    let completed = mux_outcome.completed;
    let bytes_done = mux_outcome.bytes_done;
    let elapsed = mux_outcome.elapsed_secs;
    let speed = mux_outcome.speed_mbs;
    // 0.20.8 fix #1: if `MuxSink::close` failed in `output.finish()`, the MKV
    // is structurally invalid (unseekable). Quarantine with `.failed`; skipped
    // for halt/timeout/panic, which the existing "stopped" retry path handles.
    let finalize_error = mux_outcome.finalize_error.clone();
    // A hard producer read error is distinct from a user halt: both yield
    // `completed=false` with no `finalize_error`, but only halt falls through
    // to silent "stopped → idle" — a read failure must surface as an error.
    let read_error = mux_outcome.read_error.clone();
    // Undelivered streams are NOT re-reported here: `map_iso_mux_outcome`
    // already logs `undelivered_streams_note` into this same log, so a summary
    // copy would be a second, differently-worded line for one event.
    let mut final_errors = mux_outcome.errors;
    let final_last_sector = rip_last_lba.load(Ordering::Relaxed);
    let final_current_batch = rip_current_batch.load(Ordering::Relaxed);
    let mut final_lost_secs = mux_outcome.lost_video_secs;
    // Demux-time loss (fails decrypt at mux, or codec-skip zero-fills): the
    // in-title estimate single-pass/resume also fold in. Captured BEFORE the
    // multipass overwrite below replaces `final_lost_secs`; mux never aborts on it.
    let demux_lost_secs = mux_outcome.lost_video_secs;
    // In multipass mode the `input.errors` counter above counts ISO→MKV demux
    // skips (usually zero — ISO reads don't fail). The real bad-sector count
    // lives in the mapfile sidecar. Prefer that when present.
    if uses_multipass(cfg_read.max_retries)
        && let Ok(map) = freemkv_engine::Mapfile::load(std::path::Path::new(&mapfile_path_str))
    {
        let stats = map.stats();
        // Only Unreadable counts as "lost" — NonTried/NonTrimmed/NonScraped at
        // the end means the rip was interrupted, not those bytes damaged.
        let bad_bytes = stats.bytes_unreadable;
        final_errors = (bad_bytes / 2048) as u32;
        // Use the in-title-scoped loss already computed by abort_lost_ms() (same
        // gate used above). Whole-disc `bad_bytes / bps` inflates the 'done' card
        // when out-of-title menus/trailers are scratched but the gate accepted it.
        final_lost_secs = if main_lost_ms_for_history_outer > 0.0 {
            main_lost_ms_for_history_outer / MILLIS_PER_SEC
        } else {
            // Zero here means no bad sectors (or bytes_unreadable == 0); fall
            // back to the mux outcome's own lost_video_secs in that case.
            mux_outcome.lost_video_secs
        };
    }

    // Mux-time loss is gated against `abort_on_lost_secs` below (sole
    // enforcement point). Emit a final summary line so the log ends
    // clean, not on a stale progress tick; history snapshot reads LOGS.
    if completed {
        crate::server::log::device_log(
            device,
            &format!(
                "Mux complete: {:.1} GB in {}s ({:.1} MB/s avg)",
                bytes_done as f64 / BYTES_PER_GIB,
                elapsed.round() as u64,
                speed
            ),
        );
    } else if let Some(reason) = finalize_error.as_ref() {
        crate::server::log::device_log(device, &format!("Mux failed: {reason}"));
    }

    // ── Mux-time loss gate (a loss is a loss) ─────────────────────────────
    // Catches mux-time (decrypt/codec) loss the pre-mux gate can't see. Over
    // threshold → RESUMABLE `.aborted-loss`. ISO is exempt; only fires on mux-caused loss.
    {
        let effective_abort =
            effective_abort_secs(&cfg_read.output_format, cfg_read.abort_on_lost_secs);
        let read_lost_secs = main_lost_ms_for_history_outer / MILLIS_PER_SEC;
        let total_lost_secs = read_lost_secs + demux_lost_secs;
        if mux_loss_aborts(
            completed,
            output_is_iso_image(&cfg_read.output_format),
            total_lost_secs,
            demux_lost_secs,
            effective_abort,
        ) {
            crate::server::log::device_log(
                device,
                &format!(
                    "ABORT: mux-time loss — {:.2}s missing in main movie (decrypt/codec) exceeds threshold ({}s). A loss is a loss.",
                    total_lost_secs, effective_abort
                ),
            );
            update_state_with(device, |s| {
                s.status = "error".to_string();
                s.loss_aborted = true;
                if s.last_error.is_empty() {
                    s.last_error = format!(
                        "aborted — {} lost at mux, decrypt/codec ({})",
                        fmt_loss(total_lost_secs * MILLIS_PER_SEC),
                        fmt_threshold(effective_abort)
                    );
                }
            });
            record_rip_loss_abort(
                device,
                std::path::Path::new(&staging),
                &format!(
                    "aborted: {:.2}s lost at mux, decrypt/codec ({})",
                    total_lost_secs,
                    fmt_threshold(effective_abort)
                ),
            );
            unregister_halt(device);
            return;
        }
    }

    // Write the staging markers (.done/.completed/.failed) the mover and resume
    // detector depend on. (The per-rip history record removed in 0.30.1 — see web.rs.)
    {
        if completed {
            // Durability gate: fsync the finished MKV/M2TS before any success marker,
            // since mux finish()'s bounded fsync returns Ok even on timeout/halt.
            // Skipped for network:// output; on failure, withhold markers and retry.
            let is_network = staging::is_network_output(&output_format, &cfg_read.network_target);
            if !staging::durability_gate_passes(is_network, || {
                staging::fsync_output_file(std::path::Path::new(&output_path))
            }) {
                abort_post_mux_preserving_staging(
                    device,
                    "Durability gate failed: could not fsync mux output to stable storage; \
                     withholding .done/.completed and preserving staging for retry",
                    "mux output not durable (fsync failed); rip preserved for retry",
                );
                return;
            }
            // Confident match → hand straight to the mover (.done). Otherwise HOLD
            // for review (.review) rather than auto-file under a guessed name; a
            // would-overwrite collision is still caught later by the mover's own guard.
            let marker_name = staging::handoff_label(title_confident);
            // One durable `state.json` transition: the staging-dir fsync is the
            // crash barrier, so a crash can't leave a dir without a hand-off.
            // Carries mover metadata + season/tmdb_id/disc.
            let staging_disc_path = std::path::Path::new(&staging);
            let mkv_leaf = filename.clone();
            if let Err(e) = staging::mark_handoff(staging_disc_path, title_confident, |s| {
                s.title = display_name.clone();
                s.disc_name = disc_name.clone();
                s.disc_format = disc_format.clone();
                s.year = tmdb_year;
                s.media_type = tmdb_media_type.clone();
                s.tmdb_id = tmdb_id;
                s.tmdb_poster = tmdb_poster.clone();
                s.tmdb_overview = tmdb_overview.clone();
                s.season = crate::server::tmdb::season_from_label(&disc_name);
                s.disc_number = crate::server::tmdb::disc_from_label(&disc_name);
                s.outputs = vec![staging::Output {
                    filename: mkv_leaf,
                    ..Default::default()
                }];
            }) {
                // The MKV is staged, but the mover keys off this marker — without
                // it the file sits forever with no signal. Surface staged-but-unqueued.
                abort_post_mux_preserving_staging(
                    device,
                    &format!(
                        "{marker_name} marker write failed ({e}); MKV is staged but the mover cannot pick it up"
                    ),
                    &format!("MKV staged but {marker_name} marker write failed: {e}"),
                );
                // The hand-off marker never landed. Do NOT proceed to `.completed`:
                // that would look terminal-complete with no mover signal and no
                // resume re-run. Return early so a later attempt re-writes it.
                return;
            }
            if !title_confident {
                crate::server::log::device_log(
                    device,
                    &format!(
                        "Held for review: uncertain title match for \"{}\" — confirm/correct in the UI",
                        display_name
                    ),
                );
            }
            // `write_completed_marker` does NOT downgrade the `Done`/`Review`
            // state (resume's "finished" check covers both); it just releases the lock.
            staging::write_completed_marker(staging_disc_path);
            staging::clear_restart_count(staging_disc_path);
        } else if let Some(reason) = finalize_error.as_ref() {
            // 0.20.8 fix #1: `output.finish()` errored, so the MKV's Cues/
            // segment-size header wasn't written — unseekable/invalid. Quarantine
            // with `.failed` so mover skips it and resume treats it as terminal-failed.
            let staging_disc_path = std::path::Path::new(&staging);
            quarantine_or_log(
                device,
                staging_disc_path,
                &format!("mux finalize failed: {reason}"),
            );
            staging::clear_restart_count(staging_disc_path);
        }
    }

    if !completed {
        // 0.20.8 fix #1: a finalize error means the MKV is broken. Log +
        // surface `status="failed"` so the tile flips red with the reason;
        // otherwise fall through to "stopped → idle" (halt/write error/wedge).
        let (log_prefix, ui_status, ui_failure_reason) =
            incomplete_mux_status(finalize_error.as_deref(), read_error.as_deref());
        crate::server::log::device_log(
            device,
            &format!(
                "{}: {:.1} GB in {:.0}s ({:.0} MB/s), {} skipped (~{:.3}s lost)",
                log_prefix,
                bytes_done as f64 / BYTES_PER_GIB,
                elapsed,
                speed,
                final_errors,
                final_lost_secs,
            ),
        );
        update_state(
            device,
            RipState {
                device: device.to_string(),
                status: ui_status,
                disc_present: true,
                disc_name: display_name.clone(),
                disc_format: disc_format.clone(),
                errors: final_errors,
                lost_video_secs: final_lost_secs,
                last_sector: final_last_sector,
                current_batch: final_current_batch,
                preferred_batch: batch,
                tmdb_title: tmdb_title.clone(),
                tmdb_year,
                tmdb_poster: tmdb_poster.clone(),
                tmdb_overview: tmdb_overview.clone(),
                duration: duration.clone(),
                codecs: codecs.clone(),
                last_error: ui_failure_reason.clone().unwrap_or_default(),
                failure_reason: ui_failure_reason,
                ..Default::default()
            },
        );
        return;
    }

    // Done figures fold in demux-time loss like single-pass/resume do, so
    // identical rips match. Single-pass already equals demux/errors as-is;
    // multi-pass overwrote both with sweep-mapfile values, so add the demux figures.
    let (done_errors, done_lost_secs, done_demux_extra_ms) = done_headline(
        uses_multipass(cfg_read.max_retries),
        (final_errors, final_lost_secs),
        main_lost_ms_for_history_outer,
        (mux_outcome.errors, demux_lost_secs),
    );

    crate::server::log::device_log(
        device,
        &format!(
            "Complete: {:.1} GB in {:.0}s ({:.0} MB/s), {} skipped (~{:.3}s lost)",
            bytes_done as f64 / BYTES_PER_GIB,
            elapsed,
            speed,
            done_errors,
            done_lost_secs,
        ),
    );

    update_state(
        device,
        RipState {
            device: device.to_string(),
            status: "done".to_string(),
            disc_present: true,
            disc_name: display_name.clone(),
            disc_format: disc_format.clone(),
            progress_pct: 100,
            errors: done_errors,
            lost_video_secs: done_lost_secs,
            last_sector: final_last_sector,
            current_batch: final_current_batch,
            preferred_batch: batch,
            output_file: staging.clone(),
            tmdb_title: tmdb_title.clone(),
            tmdb_year,
            tmdb_poster: tmdb_poster.clone(),
            tmdb_overview: tmdb_overview.clone(),
            duration: duration.clone(),
            codecs: codecs.clone(),
            // Carry sweep damage so the done card reflects it. Single-pass has no
            // mapfile, so the all-zero snapshot would starve classify_damage's
            // ms-branch; derive from `final_lost_secs` instead. Multipass keeps the real one.
            total_lost_ms: done_card_lost_ms(
                uses_multipass(cfg_read.max_retries),
                final_lost_secs,
                sweep_damage_snapshot.total_lost_ms,
                done_demux_extra_ms,
            ),
            // Single-pass has no mapfile, so `main_lost_ms` would stay 0.0 even
            // when the demux skipped in-title sectors; `final_lost_secs` already
            // holds that loss, so mirror the `total_lost_ms` branch above.
            main_lost_ms: done_card_lost_ms(
                uses_multipass(cfg_read.max_retries),
                final_lost_secs,
                sweep_damage_snapshot.main_lost_ms,
                done_demux_extra_ms,
            ),
            bad_ranges: sweep_damage_snapshot.bad_ranges.clone(),
            num_bad_ranges: sweep_damage_snapshot.num_bad_ranges,
            bad_ranges_truncated: sweep_damage_snapshot.bad_ranges_truncated,
            largest_gap_ms: sweep_damage_snapshot.largest_gap_ms,
            ..Default::default()
        },
    );

    // Prune intermediate ISO + mapfile unless keep_iso is set. Shared with the
    // resume/`.ripped` completion path (resume::resume_remux) so the
    // keep_iso=false reclaim can't diverge between the two completion routes.
    prune_intermediate_iso(
        device,
        std::path::Path::new(&iso_path_str),
        std::path::Path::new(&mapfile_path_str),
        cfg_read.max_retries,
        retain_intermediate_iso(cfg_read.keep_iso, &cfg_read.output_format),
    );

    crate::server::log::device_log(device, "Mux complete");
    // Mux stage: the `.mkv` now exists. In this inline-fallback path the rip
    // (drive-free) webhook already fired before the mux began; this is the
    // separate mux_complete notification.
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
            codecs: &codecs,
            size_gb: bytes_done as f64 / BYTES_PER_GIB,
            speed_mbs: speed,
            elapsed_secs: elapsed,
            output_path: &staging,
            // Sweep loss + demux loss (same combined figures as the done card)
            // so the completion notification reports the real loss in the
            // delivered MKV, not the sweep-mapfile-only subset.
            errors: done_errors,
            lost_video_secs: done_lost_secs,
        },
    );

    // Eject LAST: `eject_drive` archives the device log partway through, so
    // every remaining log line must be emitted first or it lands in the NEXT
    // rip's ring. Routed through `should_auto_eject`, where the eject-once rule lives.
    if should_auto_eject(cfg_read.auto_eject, device) {
        eject_drive(device_path);
    }
}

// Pure decision: should this completion path auto-eject the drive? Only when `auto_eject` is on
// AND the device is not a synthetic, underscore-prefixed worker (`_mux`, etc).
pub(crate) fn should_auto_eject(auto_eject: bool, device: &str) -> bool {
    auto_eject && !device.starts_with('_')
}

pub fn eject_drive(device_path: &str) {
    let dev = device_key(device_path);
    let dev = dev.as_str();
    // Halt and drain any in-flight rip on this device BEFORE dropping
    // the session — otherwise the rip thread could still be inside a
    // libfreemkv call holding the Drive while we yank it.
    if let Some(halt) = device_halt(dev) {
        halt.cancel();
    }
    if join_rip_thread(dev, Duration::from_secs(60)).is_err() {
        tracing::warn!(device = %dev, "rip thread did not drain within 60s of eject");
    }
    // Stop design §2.5: eject through `finish` on the handle the idle session holds; with
    // none (or a dead one, e.g. after a USB re-enumeration) open the drive once.
    let held = take_session(dev).map(|s| s.drive);
    unregister_halt(dev);
    crate::server::log::archive_device_log(dev);
    // Pre-0.25.2 both branches here used `let _ =` and any failure was
    // invisible: the user-facing symptom was "auto_eject is set but the
    // disc stayed put, no log line, no idea why". Surface both.
    if let Some(d) = held {
        match libfreemkv::DiscSession::from_drive(d).finish(libfreemkv::Finish::Eject) {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!(device = %dev, error = %e, "eject on the held handle failed; reopening")
            }
        }
    }
    match freemkv_engine::drive::open(std::path::Path::new(device_path)) {
        Ok(drive) => {
            let session = libfreemkv::DiscSession::from_drive(drive);
            if let Err(e) = session.finish(libfreemkv::Finish::Eject) {
                crate::server::log::device_log(dev, &format!("eject failed: {e}"));
                tracing::warn!(device = %dev, error = %e, "eject command failed");
            }
        }
        Err(e) => {
            crate::server::log::device_log(dev, &format!("eject skipped — drive open failed: {e}"));
            tracing::warn!(device = %dev, error = %e, "eject skipped — drive open failed");
        }
    }
}

// `sanitize_filename` / `format_duration` live in `util`.

pub(crate) fn format_codecs(title: &libfreemkv::DiscTitle) -> String {
    let mut parts = Vec::new();
    // Primary video
    for s in &title.streams {
        if let libfreemkv::Stream::Video(v) = s
            && !v.secondary
        {
            let mut desc = format!("{} {}", v.codec.name(), v.resolution);
            if v.hdr != libfreemkv::HdrFormat::Sdr {
                desc.push_str(&format!(" {}", v.hdr.name()));
            }
            parts.push(desc);
            break;
        }
    }
    // First primary audio only
    for s in &title.streams {
        if let libfreemkv::Stream::Audio(a) = s
            && !a.secondary
        {
            let mut audio = format!("{} {}", a.codec.name(), a.channels);
            // autorip is English-only — inline the purpose tags directly.
            if let Some(tag) = audio_purpose_tag(a.purpose) {
                audio.push_str(&format!(" {}", tag));
            }
            parts.push(audio);
            break;
        }
    }
    parts.join(" · ")
}

/// English purpose label for autorip rendering. None for Normal streams.
/// libfreemkv keeps strings out of the library; autorip is English-only so we
/// inline the words here rather than going through i18n.
fn audio_purpose_tag(p: libfreemkv::LabelPurpose) -> Option<&'static str> {
    match p {
        libfreemkv::LabelPurpose::Commentary => Some("Commentary"),
        libfreemkv::LabelPurpose::Descriptive => Some("Descriptive Audio"),
        libfreemkv::LabelPurpose::Score => Some("Score"),
        libfreemkv::LabelPurpose::Ime => Some("IME"),
        libfreemkv::LabelPurpose::Normal => None,
    }
}

// Pick the mux-phase progress denominator (percent + ETA). Multipass reads whole disc capacity;
// single-pass scopes to the title's extent sum instead, so its progress reaches 100%.
fn mux_progress_denominator(
    max_retries: u8,
    total_bytes: u64,
    title: &libfreemkv::DiscTitle,
) -> u64 {
    if uses_multipass(max_retries) {
        return total_bytes;
    }
    let extent_bytes: u64 = title
        .extents
        .iter()
        .map(|e| e.sector_count as u64 * 2048)
        .sum();
    if extent_bytes > 0 {
        extent_bytes
    } else {
        total_bytes
    }
}

// Whether mux-time (decrypt/codec) loss must quarantine the rip. SOLE enforcement point for
// mux-time loss (pre-mux gate only reads the mapfile Unreadable set).
fn mux_loss_aborts(
    completed: bool,
    is_iso: bool,
    total_lost_secs: f64,
    demux_lost_secs: f64,
    effective_abort: u64,
) -> bool {
    // Bind rather than write `!(demux_lost_secs > 0.0)`: clippy rejects that
    // negation, and `<= 0.0` is NOT equivalent — NaN comparisons are always
    // false, so `<=` would wrongly let NaN reach the threshold check.
    let mux_contributed_loss = demux_lost_secs > 0.0;
    if !completed || is_iso || !mux_contributed_loss {
        return false;
    }
    if effective_abort == 0 {
        total_lost_secs > 0.0
    } else {
        total_lost_secs > effective_abort as f64
    }
}

// Does this `max_retries` setting select the MULTI-PASS rip route? One predicate for a decision
// taken in eight places along `rip_disc` that must all agree.
pub(crate) fn uses_multipass(max_retries: u8) -> bool {
    max_retries > 0
}

// The done card's `total_lost_ms` / `main_lost_ms`, in ONE place. Single-pass has no mapfile,
// so it carries `final_lost_secs` instead.
pub(super) fn done_card_lost_ms(
    multipass: bool,
    final_lost_secs: f64,
    snapshot_lost_ms: f64,
    demux_extra_ms: f64,
) -> f64 {
    if multipass {
        snapshot_lost_ms + demux_extra_ms
    } else {
        final_lost_secs * crate::server::util::MILLIS_PER_SEC
    }
}

// The done card's headline (errors, lost secs, demux extra ms). Single-pass `final_*` already
// are the mux figures. Multipass adds the demux figures to the sweep's: the in-title read loss
// (`read_lost_ms`), never `final_lost_secs`, which falls back to the demux loss itself.
fn done_headline(
    multipass: bool,
    (final_errors, final_lost_secs): (u32, f64),
    read_lost_ms: f64,
    (mux_errors, demux_lost_secs): (u32, f64),
) -> (u32, f64, f64) {
    if !multipass {
        return (final_errors, final_lost_secs, 0.0);
    }
    (
        final_errors.saturating_add(mux_errors),
        read_lost_ms / MILLIS_PER_SEC + demux_lost_secs,
        demux_lost_secs * MILLIS_PER_SEC,
    )
}

// Is the resolved title trustworthy enough to auto-file the finished rip, or must it be HELD
// for operator review? One disjunction decides `.done` vs `.review` for both routes.
fn title_is_confident(
    tmdb_api_key: &str,
    overridden: bool,
    disc_name: &str,
    display_name: &str,
    tmdb_year: u16,
) -> bool {
    tmdb_api_key.trim().is_empty()
        || overridden
        || crate::server::tmdb::is_confident_match(disc_name, display_name, tmdb_year)
}

// Quarantine a staging dir as `.failed`; if that did not persist (state.json
// unreadable, or staging unwritable) say so in the device log instead of
// silently carrying on as if the dir were terminal.
fn quarantine_or_log(device: &str, staging_disc_path: &std::path::Path, reason: &str) {
    if !staging::write_failed_marker(staging_disc_path, reason) {
        crate::server::log::device_log(
            device,
            &format!(
                "The .failed quarantine for {} did not persist (state.json unreadable or staging unwritable); the dir is left as-is for the operator.",
                staging_disc_path.display()
            ),
        );
    }
}

// Hand the swept ISO to the mux worker: lend it the rip's key set (memory only, J6), record
// the deliverable plan while this rip still owns the dir, then flip state.json to Ripped,
// which lets the worker claim it. A failed hand-off takes the keys back.
fn hand_off_to_mux_worker(
    staging_path: &std::path::Path,
    marker: &crate::server::muxer::RippedMarker,
    keys: Option<&libfreemkv::keys::KeyRing>,
    plan: impl FnOnce(&mut staging::DiscState),
) -> std::io::Result<()> {
    let iso = std::path::Path::new(&marker.iso_path);
    if let Some(set) = keys {
        crate::server::keysource::hold_rip_keys(iso, set.clone());
    }
    staging::mutate_state_if_present(staging_path, plan);
    let written = crate::server::muxer::write_marker(staging_path, marker);
    if written.is_err() {
        crate::server::keysource::forget_rip_keys(iso);
    }
    written
}

// The sweep damage figures the passes pushed into `device`'s STATE row.
fn sweep_damage_from_state(device: &str) -> Option<mux::SweepDamageSnapshot> {
    let s = state::STATE.lock().unwrap_or_else(|e| e.into_inner());
    s.get(device).map(|rs| mux::SweepDamageSnapshot {
        errors: rs.errors,
        total_lost_ms: rs.total_lost_ms,
        main_lost_ms: rs.main_lost_ms,
        bad_ranges: rs.bad_ranges.clone(),
        num_bad_ranges: rs.num_bad_ranges,
        bad_ranges_truncated: rs.bad_ranges_truncated,
        largest_gap_ms: rs.largest_gap_ms,
    })
}

// The tile row at the `.ripped` hand-off: the disc read is done (100%) and the sweep damage
// stays on the card while the mux runs separately.
fn handoff_done_row(device: &str, card: RipState) -> RipState {
    let d = sweep_damage_from_state(device).unwrap_or_default();
    RipState {
        status: "done".to_string(),
        progress_pct: 100,
        errors: d.errors,
        total_lost_ms: d.total_lost_ms,
        main_lost_ms: d.main_lost_ms,
        bad_ranges: d.bad_ranges,
        num_bad_ranges: d.num_bad_ranges,
        bad_ranges_truncated: d.bad_ranges_truncated,
        largest_gap_ms: d.largest_gap_ms,
        ..card
    }
}

// Record a rip's loss abort as `.aborted-loss`; a write that did not land is surfaced in the
// device log and as an operator card, never left to re-dispatch silently.
fn record_rip_loss_abort(device: &str, staging_disc_path: &std::path::Path, reason: &str) {
    if staging::mark_aborted_on_loss_reporting_landed(staging_disc_path, reason) {
        return;
    }
    crate::server::log::device_log(
        device,
        &format!(
            "The .aborted-loss marker for {} did not persist (staging unwritable?); the dir is left in its prior state.",
            staging_disc_path.display()
        ),
    );
    crate::server::muxer::record_error(
        &staging_disc_path.to_string_lossy(),
        reason,
        "the loss-abort marker could not be written to state.json (staging mount full / unwritable) — free space or fix permissions on the staging share",
    );
}

/// The legacy hand-off marker name (`.done`/`.review`). The completion paths now
/// transition `state.json` via [`staging::mark_handoff`] / [`staging::handoff_label`];
/// this is retained only for the tests that pin the `.done`/`.review` vocabulary.
#[cfg(test)]
fn handoff_marker_name(title_confident: bool) -> &'static str {
    if title_confident { ".done" } else { ".review" }
}

// Episode titles a TV disc fans out to under `tv_auto`; empty = one movie-style output.
// Pure (no TMDB): the preflight sizes the same plan `plan_mux_outputs` later names.
fn fanout_episode_indices(
    titles: &[libfreemkv::DiscTitle],
    cfg: &Config,
    media_type: &str,
    disc_name: &str,
) -> Vec<usize> {
    let is_tv = media_type == "tv" || crate::server::tmdb::season_from_label(disc_name).is_some();
    if !cfg.tv_auto || !is_tv {
        return Vec::new();
    }
    // The episode cluster: drops the play-all sum-title, extras/menus, dupes.
    let indices = freemkv_engine::episode_titles(titles);
    // A single feature that merely carries a TV label (e.g. a TV movie) is one output.
    if indices.len() <= 1 {
        return Vec::new();
    }
    indices
}

// Staging bytes the mux phase writes before the ISO is pruned, for a fresh rip's plan.
fn mux_output_reserve_bytes(
    titles: &[libfreemkv::DiscTitle],
    cfg: &Config,
    media_type: &str,
    disc_name: &str,
    selected_title_bytes: u64,
) -> u64 {
    let fanout = fanout_episode_indices(titles, cfg, media_type, disc_name);
    mux_reserve_for(cfg, titles, &fanout, selected_title_bytes)
}

// Bytes muxed into staging for `fanout` episode titles (empty = one selected-title output).
// ISO output is the image itself; a network sink never lands in staging.
pub(super) fn mux_reserve_for(
    cfg: &Config,
    titles: &[libfreemkv::DiscTitle],
    fanout: &[usize],
    selected_title_bytes: u64,
) -> u64 {
    if output_is_iso_image(&cfg.output_format)
        || staging::is_network_output(&cfg.output_format, &cfg.network_target)
    {
        return 0;
    }
    if fanout.is_empty() {
        return selected_title_bytes;
    }
    fanout
        .iter()
        .filter_map(|&i| titles.get(i))
        .fold(0u64, |acc, t| acc.saturating_add(t.size_bytes))
}

// Decide the deliverables a captured disc produces: titles to mux out of the ISO + staging
// filename of each. Movie → one output; TV under `tv_auto` → one per episode, `S{NN}E{MM}`.
fn plan_mux_outputs(
    titles: &[libfreemkv::DiscTitle],
    cfg: &Config,
    media_type: &str,
    disc_name: &str,
    tmdb_id: u64,
    movie_filename: &str,
) -> Vec<staging::Output> {
    let one_output = || {
        vec![staging::Output {
            filename: movie_filename.to_string(),
            ..Default::default()
        }]
    };
    let indices = fanout_episode_indices(titles, cfg, media_type, disc_name);
    if indices.is_empty() {
        return one_output();
    }
    let season_num = crate::server::tmdb::season_from_label(disc_name).unwrap_or(1);
    // TMDB episode list, best-effort (empty on any failure → sequential naming).
    let episodes = crate::server::tmdb::season_episodes(tmdb_id, season_num, &cfg.tmdb_api_key);
    plan_episode_outputs(titles, &indices, disc_name, &episodes, movie_filename)
}

// Name the fanned-out episode `indices` against the season's TMDB `episodes`.
fn plan_episode_outputs(
    titles: &[libfreemkv::DiscTitle],
    indices: &[usize],
    disc_name: &str,
    episodes: &[crate::server::tmdb::Episode],
    movie_filename: &str,
) -> Vec<staging::Output> {
    let season_num = crate::server::tmdb::season_from_label(disc_name).unwrap_or(1);
    let title_secs: Vec<f64> = indices.iter().map(|&i| titles[i].duration_secs).collect();
    // Multi-disc offset: start from the uniform-split guess `(disc-1)*count+1`,
    // then let `align_disc_offset` repair uneven splits when runtimes carry
    // signal. With no signal it ties and returns this same fallback (never worse).
    let disc_num = crate::server::tmdb::disc_from_label(disc_name)
        .unwrap_or(1)
        .max(1);
    let fallback_start = 1u16.saturating_add(
        disc_num
            .saturating_sub(1)
            .saturating_mul(indices.len() as u16),
    );
    let start = crate::server::tmdb::align_disc_offset(&title_secs, episodes, fallback_start);
    let assignments = crate::server::tmdb::map_episodes(&title_secs, episodes, start);
    // Staging leaves derive from the movie leaf's stem + extension so they share
    // the output format and stay unique per episode. The mover renames each to
    // `Show S{NN}E{MM}[ - Name].ext` at file time (see `mover::tv_episode_leaf`).
    let path = std::path::Path::new(movie_filename);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("title");
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mkv");
    indices
        .iter()
        .zip(assignments)
        .map(|(&idx, a)| staging::Output {
            filename: format!("{stem}_S{season_num:02}E{:02}.{ext}", a.episode),
            title_index: idx,
            episode: Some(a.episode),
            episode_name: a.name,
            moved: false,
        })
        .collect()
}

// The FMTS CaptureOnly line: only a multipass rip captures an ISO to defer the mux to.
fn fmts_capture_only_log(multipass: bool) -> &'static str {
    if multipass {
        "FMTS: forensic keys unavailable — capturing raw ISO now \
         (Capture Discs Without Keys is on); mux deferred until keys arrive."
    } else {
        "FMTS: forensic keys unavailable — a single-pass rip captures no ISO to defer \
         the mux to, so nothing is ripped. Enable multi-pass mode to capture one."
    }
}

// The staging leaf a rip delivers: the ISO image for ISO output, else the muxed title file.
fn delivered_file_name<'a>(
    output_format: &str,
    filename: &'a str,
    iso_filename: &'a str,
) -> &'a str {
    if output_is_iso_image(output_format) {
        iso_filename
    } else {
        filename
    }
}

// Whether the rip's deliverable is the whole-disc ISO itself rather than a muxed MKV/M2TS
// title. Single predicate every deliverable / prune / mux-skip decision keys off.
pub(crate) fn output_is_iso_image(output_format: &str) -> bool {
    output_format == crate::server::config::OUTPUT_FORMAT_ISO
}

// Effective main-movie-loss tolerance for the abort gate. ISO output must be byte-complete, so
// `abort_on_lost_secs` is forced to 0 ("require 100%") for it.
fn effective_abort_secs(output_format: &str, configured: u64) -> u64 {
    freemkv_engine::effective_abort_secs(output_is_iso_image(output_format), configured)
}

/// Human-readable main-movie loss for UI / markers. Sub-second loss shows
/// milliseconds (so a 12 KB / ~1 ms gap reads as "1 ms", not a confusing
/// "0.00s"); a second or more shows seconds. NaN (unquantifiable) is spelled out.
fn fmt_loss(lost_ms: f64) -> String {
    if !lost_ms.is_finite() {
        "an unknown amount".to_string()
    } else if lost_ms < crate::server::util::MILLIS_PER_SEC {
        format!("{:.0} ms", lost_ms.max(0.0))
    } else {
        format!("{:.2}s", lost_ms / crate::server::util::MILLIS_PER_SEC)
    }
}

/// Human-readable abort threshold: 0 means "perfect rip required" (any loss
/// aborts), otherwise the configured seconds.
fn fmt_threshold(secs: u64) -> String {
    if secs == 0 {
        "perfect rip required".to_string()
    } else {
        format!("threshold {secs}s")
    }
}

/// Whether the intermediate ISO must be retained as the deliverable rather than
/// pruned. True when the operator asked to keep it (`keep_iso`) OR when ISO is
/// the selected output (the ISO *is* the deliverable — see `output_is_iso_image`).
fn retain_intermediate_iso(keep_iso: bool, output_format: &str) -> bool {
    keep_iso || output_is_iso_image(output_format)
}

// Whether an `output_format == "iso"` rip must be rejected because it was requested in
// single-pass mode: only multi-pass captures a real, whole-disc-scoped ISO.
fn iso_output_needs_multipass(output_format: &str, max_retries: u8) -> bool {
    output_is_iso_image(output_format) && !uses_multipass(max_retries)
}

// Multipass recovery-loop STRATEGY DECISIONS live in `freemkv-engine` (`multipass.rs`),
// shared with the CLI and GUI; the rest are reached only by this module's tests.
use freemkv_engine::plan_passes;
#[cfg(test)]
use freemkv_engine::{
    PatchDecision, bad_sector_statuses, end_of_recovery_promotion, patch_made_progress,
    patch_pass_decision, pre_pass_converged, scope_bad_bytes, scope_converged,
};

// Prune the disc-sized intermediate ISO and mapfile sidecar on a successful multipass
// completion, unless `keep_iso` is set. Shared by both completion routes.
fn prune_intermediate_iso(
    device: &str,
    iso_path: &std::path::Path,
    mapfile_path: &std::path::Path,
    max_retries: u8,
    keep_iso: bool,
) {
    if !uses_multipass(max_retries) || keep_iso {
        return;
    }
    match std::fs::remove_file(iso_path) {
        Ok(_) => crate::server::log::device_log(device, "Pruned intermediate ISO"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => crate::server::log::device_log(device, &format!("ISO prune warning: {e}")),
    }
    // Mirror the ISO arm: a lingering mapfile in staging could be misread as a
    // partial rip by the resume classifier on next startup, so surface any
    // unexpected removal error instead of swallowing it.
    match std::fs::remove_file(mapfile_path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => crate::server::log::device_log(device, &format!("mapfile prune warning: {e}")),
    }
}

/// What the orchestrator must do with a mux outcome, decided from the header
/// phase alone.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum HeaderPhase<'a> {
    /// An output file was opened — carry on to the normal completion path
    /// (loss gate, fsync, `.done` / `.review`, `.completed`).
    Produced,
    /// No output was opened and no reason was recorded: a clean stop during the
    /// header read. Leave the staging dir resumable, write no marker.
    ResumableStop,
    /// No output was opened and the mux recorded why: the stream is
    /// structurally unusable. Quarantine (`.failed`) and surface it.
    Failed(&'a str),
}

// Route a mux outcome by its header phase. Folded into one predicate
// so `output_opened` is consulted EXACTLY once (a double-test bug
// previously sent a successful mux down the no-output path — rule 1).
fn header_phase_disposition(output_opened: bool, finalize_error: Option<&str>) -> HeaderPhase<'_> {
    match finalize_error {
        _ if output_opened => HeaderPhase::Produced,
        Some(reason) => HeaderPhase::Failed(reason),
        None => HeaderPhase::ResumableStop,
    }
}

// Does `on_read_error` mean "skip bad sectors" (zero-fill) or "stop"?
// Only `"skip"` enables concealment; anything else leaves errors
// surfacing on `/api/state` instead of being buried as complete.
fn skip_read_errors(on_read_error: &str) -> bool {
    on_read_error == "skip"
}

// Log prefix / status / last_error for a mux with `completed == false`: finalize_error (failed)
// > read_error (error) > neither (idle).
fn incomplete_mux_status(
    finalize_error: Option<&str>,
    read_error: Option<&str>,
) -> (String, String, Option<String>) {
    if let Some(reason) = finalize_error {
        (
            format!("Failed (mux finalize): {reason}"),
            "failed".to_string(),
            Some(format!("mux finalize failed: {reason}")),
        )
    } else if let Some(cause) = read_error {
        (
            format!("Failed (read error): {cause}"),
            "error".to_string(),
            Some(format!("rip stopped: read error — {cause}")),
        )
    } else {
        ("Stopped".to_string(), "idle".to_string(), None)
    }
}

// Operator-facing message for the "encrypted disc, no usable keys" failure, dispatched from the
// whole disc (prefers `css_error` over `aacs_error` when both could apply).
fn keyless_failure_message(disc: &libfreemkv::Disc) -> String {
    keyless_failure_message_for(disc.css_error.as_ref(), disc.aacs_error.as_ref())
}

// Whether the scan's key resolve asks the online key service, so the tile says
// "Communicating with online keyserver": online mode only, never for a DVD's CSS.
fn announces_online_resolve(
    cfg: &Config,
    disc: &libfreemkv::Disc,
    scope: &libfreemkv::keys::KeyScope,
) -> bool {
    crate::server::keysource::uses_online(cfg)
        && !matches!(disc.format, libfreemkv::DiscFormat::Dvd)
        && *scope != libfreemkv::keys::KeyScope::None
}

// (log line, state reason) for the resume path's keyless mux deferral, from the resume
// decode's verdict (`decode_reach`); probes only when that decode made no HTTP answer.
// A terminal verdict won't clear by waiting, so it must not promise an automatic mux.
pub(crate) fn deferred_keyless_texts(
    cfg: &Config,
    disc: &libfreemkv::Disc,
    decode_reach: Option<crate::server::keysource::ServiceReachability>,
) -> (String, String) {
    const LEAD: &str = "Ripped to ISO — no keys, mux deferred";
    let reach = crate::server::keysource::uses_online(cfg).then(|| {
        decode_reach.unwrap_or_else(|| crate::server::keysource::probe_online_reachability(cfg))
    });
    // A 422 here is not terminal: the ISO is kept, and a later key-DB update may bring the key.
    let terminal = reach
        .filter(|r| *r != crate::server::keysource::ServiceReachability::NoKeyForDisc)
        .and_then(key_service_no_key_reason);
    if let Some(reason) = terminal {
        return (
            format!(
                "{LEAD}: {reason}\nStaging preserved; resume the rip to mux once the cause \
                 above is fixed."
            ),
            format!("{LEAD}: {reason}"),
        );
    }
    let msg = match reach {
        Some(crate::server::keysource::ServiceReachability::NoKeyForDisc) => {
            KEY_SERVICE_NO_KEY_DEFERRED.to_string()
        }
        r => r
            .and_then(key_service_transient_status)
            .unwrap_or_else(|| keyless_failure_message(disc)),
    };
    (
        format!(
            "{msg}\n{LEAD}. Staging preserved; will mux automatically once keys are available."
        ),
        format!("{LEAD}. {msg}"),
    )
}

/// CSS-over-AACS priority dispatch, split out from [`keyless_failure_message`]
/// so the `.or()` ordering (css_error preferred when both are set, and
/// consulted at all) is unit-testable without constructing a full `Disc`.
fn keyless_failure_message_for(
    css_error: Option<&libfreemkv::Error>,
    aacs_error: Option<&libfreemkv::Error>,
) -> String {
    aacs_failure_message(css_error.or(aacs_error))
}

// User-facing message for the "encrypted disc, no keys resolved" failure, dispatched code-based
// on `Disc::aacs_error`. Render format: `Error: E<code> <message>` via `error_line`.
fn aacs_failure_message(err: Option<&libfreemkv::Error>) -> String {
    use libfreemkv::error as ec;

    // CssKeyMissing is a CSS (DVD) crack failure, not an AACS resolution
    // failure — surface it with CSS-specific messaging before the AACS
    // numeric dispatch so the operator isn't pointed at a key source.
    if let Some(libfreemkv::Error::CssKeyMissing) = err {
        return error_line(
            ec::E_CSS_KEY_MISSING,
            "Could not unscramble the disc. This is a CSS-protected disc and no title \
             key could be recovered. The disc may be damaged or use an unsupported \
             protection variant.",
        );
    }

    // KeydbLoad is a structural pre-condition, not an AACS resolution failure —
    // handle it before the numeric dispatch. `path` is either the sentinel
    // (nothing configured) or a real path that failed to load (include it).
    if let Some(libfreemkv::Error::KeydbLoad { path }) = err {
        const KEYDB_SENTINEL: &str = "<no keydb in search paths>";
        if path == KEYDB_SENTINEL {
            return error_line(
                ec::E_KEYDB_LOAD,
                "No keys are available. Configure a key source in Settings.",
            );
        }
        return error_line(
            ec::E_KEYDB_LOAD,
            &format!(
                "A configured key source failed to load: {path}. Check that the path \
                 exists and is readable."
            ),
        );
    }

    let Some(e) = err else {
        // Defensive fallback. scan_with always sets aacs_error when
        // encrypted && aacs.is_none(); if we land here something is
        // structurally off (e.g. callers building Disc by hand).
        return "This disc is encrypted and no keys were found. Check the key source \
                in Settings."
            .to_string();
    };

    let code = e.code();
    // Intentional overlap: dedicated arms sit above the `7000..=7999` catch-all;
    // match-order gives the dispatch we want, so the overlapping-arm lint is a false positive.
    #[allow(clippy::match_overlapping_arm)]
    match code {
        // E7000 — generic "everything tried, nothing worked" catch-all.
        ec::E_AACS_NO_KEYS => error_line(
            code,
            "No keys are available for this disc. It could not be resolved and no key \
             derivation path worked.",
        ),

        // Host cert rejected by the drive's HRL (codes 7003/7005/7007/7015).
        // "Update keys" intentionally NOT suggested — the key source has the
        // cert, the drive HRL is blocking it; fresh keys don't change cert content.
        ec::E_AACS_CERT_REJECTED
        | ec::E_AACS_CERT_VERIFY
        | ec::E_AACS_KEY_REJECTED
        | ec::E_AACS_HOST_CERT_REJECTED => error_line(
            code,
            "The drive rejected every available host certificate. The drive needs a \
             firmware unrevoke or raw-read mode to rip this disc.",
        ),

        // Drive does not support raw-read mode AND no host certs are
        // available for cert auth. Distinct from cert-rejected
        // because we never got far enough to attempt cert exchange.
        ec::E_AACS_RAW_READ_UNSUPPORTED => error_line(
            code,
            "The drive does not support raw-read mode and no usable host certificate \
             is available. This drive cannot rip this disc.",
        ),

        // VID retrieval failed (cert path: 7009/7010; raw-read path: 7017).
        // Either way the disc isn't in any key source, or Path 1 would have hit first.
        ec::E_AACS_VID_READ | ec::E_AACS_VID_MAC | ec::E_AACS_VID_UNAVAILABLE => error_line(
            code,
            "The drive did not return the disc Volume ID during AACS authentication, \
             so keys could not be derived and the disc could not be resolved.",
        ),

        // MK derivation failed. VID succeeded but no media key in the key
        // source walks this disc's MKB (7011) and no further fallback is
        // available (7018).
        ec::E_AACS_DATA_KEY | ec::E_AACS_MK_UNAVAILABLE => error_line(
            code,
            "The Volume ID was read, but no media key from any key source unlocks this \
             disc's media key block.",
        ),

        // Disc-hash lookup in a key source missed and no other path is
        // available. Typically downstream of VID being unavailable so
        // the derivation paths short-circuit.
        ec::E_AACS_VUK_NOT_IN_KEYDB => error_line(
            code,
            "This disc could not be resolved. Its disc hash was not found in any key \
             source.",
        ),

        // No host cert available — OEM auth can't run with nothing to
        // authenticate with. Distinct from cert-rejected: there a cert
        // existed but was HRL-blocked; here none was present.
        ec::E_AACS_NO_HOST_CERT => error_line(
            code,
            "No host certificate is available, so the OEM authentication route cannot \
             run.",
        ),

        // Drive identity didn't match any bundled profile, so the
        // per-drive CDB templates needed for the OEM VID route aren't
        // available.
        ec::E_DRIVE_PROFILE_MISSING => error_line(
            code,
            "This drive is not in the profile database, so the OEM Volume ID route \
             cannot run.",
        ),

        // Drive profile is present but carries no VID-retrieval CDB
        // template (older profile entry, or a drive class without an OEM
        // VID path).
        ec::E_VID_CDB_UNAVAILABLE => error_line(
            code,
            "This drive's profile has no Volume ID command (it is an older profile), \
             so the OEM Volume ID route cannot run.",
        ),

        // Key-SOURCE failures: the source never answered, so they must not read
        // as "no key". The status-less residual — where autorip holds the HTTP
        // status, `key_service_no_key_reason` reports that and beats these.
        ec::E_KEY_SERVICE_UNAVAILABLE => error_line(
            code,
            "The online key service did not answer, so nothing is known yet about \
             whether this disc has a key. Check the key-service address in Settings \
             and your network connection, then try again.",
        ),

        ec::E_KEY_SERVICE_UNAUTHORIZED => error_line(
            code,
            "The online key service rejected the credentials configured for it, so it \
             never looked for a key. Fix the key-service access token in Settings — \
             waiting will not help.",
        ),

        ec::E_KEY_SERVICE_RATE_LIMITED => error_line(
            code,
            "The online key service is refusing requests for now because too many have \
             been sent, so it never looked for a key. Wait a while and try again.",
        ),

        ec::E_AACS_KEY_FILE_UNREADABLE => error_line(
            code,
            "This disc's AACS key file (Unit_Key_RO.inf) is missing or could not be read, \
             so no key can be checked against it. Clean the disc and try again; if it still \
             fails, the disc may be damaged.",
        ),

        // KU §3.2 up-front refusals: no key source holds a key that opens the content.
        ec::E_NO_DISC_KEY | ec::E_WHOLE_DISC_KEY_MISSING => error_line(
            code,
            "No key source has a key that opens this disc's content. Add or update a \
             key source in Settings.",
        ),

        ec::E_AACS_VID_NEEDS_DISC => error_line(code, VID_NEEDS_DISC_MESSAGE),

        // Host certs were offered, but every one failed a local check before any
        // drive round-trip — a keydb problem, not a rejection (distinct from
        // E_AACS_NO_HOST_CERT: certs WERE present here).
        ec::E_AACS_NO_USABLE_HOST_CERT => error_line(
            code,
            "Host certificates were found in your key sources, but none of them is \
             usable. Refresh your key database and try again.",
        ),

        // Other 7xxx — known AACS category but unmapped. Use a
        // generic-but-honest message rather than `({e:?})` debug-dump.
        7000..=7999 => error_line(
            code,
            "AACS key resolution failed at an unrecognized stage. Please report this \
             at https://github.com/freemkv/freemkv/issues.",
        ),

        // Non-AACS code on the aacs_error slot — structurally
        // unexpected. Preserve the code; drop the `{e:?}` debug dump.
        _ => error_line(
            code,
            "An unexpected error occurred while resolving keys. Enable debug logging \
             via /api/debug for details.",
        ),
    }
}

/// E7034 in words: the keys need the disc's Volume ID, which is read from the disc and
/// never saved (J6), so only the disc can finish the mux.
pub(crate) const VID_NEEDS_DISC_MESSAGE: &str = "Insert the disc to finish. This disc's keys \
     can only be finished with its Volume ID, which is read from the disc and never saved. \
     The ripped image is kept; inserting the disc muxes it without re-reading it.";

/// E7034 as the device tile and the mux worker's card show it (one string, so the card
/// de-dupes across ticks).
pub(crate) fn vid_needs_disc_text() -> String {
    format!(
        "E{} {VID_NEEDS_DISC_MESSAGE}",
        libfreemkv::error::E_AACS_VID_NEEDS_DISC
    )
}

// Render a user-facing error line in the locked rc.6 format:
// `Error: E<code> <message>`. Single source of the format so every
// operator-facing string in this module renders identically.
fn error_line(code: u16, message: &str) -> String {
    format!("Error: E{code} {message}")
}

// Strip the leading `Error: E<code> ` prefix from an `error_line`
// string, returning just the plain-English message (e.g. for the
// key-readiness tile). Unchanged if the prefix isn't present.
fn strip_error_prefix(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("Error: E") else {
        return s;
    };
    // Skip the numeric code, then the single separating space.
    let after_code = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    after_code.strip_prefix(' ').unwrap_or(s)
}

// Operator-facing message for the multipass disk-space preflight failure; NOT a libfreemkv
// `Error`, rendered as-is in the UI banner.
fn disk_space_preflight_message(required: u64, staging: &str, avail: u64) -> String {
    format!(
        "Insufficient staging disk space — need ≥ {:.1} GiB free at {} (remaining disc image plus planned mux output estimate), have {:.1} GiB. Free up space or set Staging Directory in Settings to a larger volume.",
        required as f64 / BYTES_PER_GIB,
        staging,
        avail as f64 / BYTES_PER_GIB,
    )
}

// Truthy-only opt-out for AUTORIP_SKIP_DISKCHECK: `0`/`false`/empty keep the check on.
fn skip_diskcheck_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let v = v.trim();
        v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
    })
}

// Unswept bytes of a resumable image, or None (fresh estimate) when the mapfile is
// missing/unreadable, sized for another disc, or the ISO on disk is truncated.
fn resume_remaining_iso_bytes(
    mapfile_path: &std::path::Path,
    iso_path: &std::path::Path,
    bytes_total_disc: u64,
) -> Option<u64> {
    freemkv_engine::Mapfile::load(mapfile_path)
        .ok()
        .filter(|map| {
            map.total_size() == bytes_total_disc
                && iso_path
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() >= bytes_total_disc)
        })
        .map(|map| map.stats().bytes_nontried)
}

fn disk_space_required_bytes(
    capacity_bytes: u64,
    title_bytes: u64,
    remaining_iso_bytes: Option<u64>,
) -> u64 {
    remaining_iso_bytes
        .unwrap_or(capacity_bytes)
        .min(capacity_bytes)
        .saturating_add(title_bytes)
}

// Short English label for a non-SCSI libfreemkv error variant, used
// in `format_pass_error`'s no-sense arm. Unmapped variants fall back
// to a generic phrase so a new libfreemkv variant never breaks the build.
fn non_scsi_error_label(e: &libfreemkv::Error) -> &'static str {
    use libfreemkv::Error;
    match e {
        Error::Halted => "rip stopped by user",
        Error::MapfileInvalid { .. } => "recovery mapfile invalid",
        Error::DiscRead { .. } => "disc read error",
        Error::DecryptFailed => "decryption failed",
        Error::NoStreams => "no playable streams on disc",
        Error::DiscCapacityOverflow | Error::DiscCapacityMalformed => {
            "drive reported unusable disc capacity"
        }
        _ => "unexpected error",
    }
}

// What the operator can do about a MEDIUM ERROR: this message reports an operation that
// already failed, so nothing is skipped or retried on its own.
const MEDIA_DAMAGE_ACTION: &str = "clean the disc and retry the rip";

// The tile's `last_error` when Pass 1 fails: the translated cause, never a strategy id.
fn pass1_last_error(e: &libfreemkv::Error) -> String {
    format_pass_error("Pass 1", e)
}

// Translate a libfreemkv read-error into a user-facing /api/state last_error message (sector
// location + plain-English cause).
fn format_pass_error(pass_label: &str, e: &libfreemkv::Error) -> String {
    // Pull sector + sense out of the structured error variants.
    let sector = match e {
        libfreemkv::Error::DiscRead { sector, .. } => Some(*sector),
        _ => None,
    };
    let sense = e.scsi_sense();

    let location = match sector {
        Some(s) => format!(
            " at {:.1} GB (sector {})",
            (s as f64 * 2048.0) / 1_000_000_000.0,
            s
        ),
        None => String::new(),
    };

    let Some(sense) = sense else {
        // Non-SCSI error: libfreemkv's Display is code-only (e.g. "E6010"
        // for Halted). For IoError use the inner io::Error message; for
        // other variants, prefix a short English label so it names what failed.
        let detail = match e {
            libfreemkv::Error::IoError { source } => source.to_string(),
            other => format!("{} ({})", other, non_scsi_error_label(other)),
        };
        return format!("{}{} failed: {}", pass_label, location, detail);
    };

    // SCSI sense-key reference (SPC-4 §4.5):
    //   2 NOT_READY, 3 MEDIUM_ERROR, 4 HARDWARE_ERROR,
    //   5 ILLEGAL_REQUEST, 6 UNIT_ATTENTION, 7 DATA_PROTECT, ...
    let (cause, action) = match (sense.sense_key, sense.asc) {
        // MEDIUM_ERROR — physical media damage.
        (3, 0x11) => ("bad sector (media damage)", MEDIA_DAMAGE_ACTION),
        (3, 0x02) | (3, 0x03) => (
            "head positioning failure (media damage)",
            MEDIA_DAMAGE_ACTION,
        ),
        (3, _) => ("media error (physical damage)", MEDIA_DAMAGE_ACTION),
        // HARDWARE_ERROR — drive firmware-level fault.
        (4, 0x3E) => (
            "drive firmware unresponsive (LOGICAL UNIT NOT CONFIGURED)",
            "power-cycle the drive and retry the rip",
        ),
        (4, _) => (
            "drive hardware error",
            "power-cycle the drive and retry the rip",
        ),
        // ILLEGAL_REQUEST — drive refuses the command. Almost
        // always wedge-state on this drive class.
        (5, 0x24) => (
            "drive rejected command (Invalid Field in CDB — wedge state)",
            "power-cycle the drive and retry the rip",
        ),
        (5, _) => (
            "drive rejected command",
            "power-cycle the drive and retry the rip",
        ),
        // NOT_READY — usually transient, but if we got here it's
        // persistent enough that retries already failed.
        (2, _) => (
            "drive reports not ready",
            "wait a few seconds and retry; if it persists, power-cycle the drive",
        ),
        _ => (
            "drive read error",
            "see autorip logs for the full SCSI sense breakdown",
        ),
    };

    format!("{}{} failed: {} — {}", pass_label, location, cause, action)
}

// Render a libfreemkv setup/scan/mux error as a plain-English, operator-facing line for
// `last_error`/the device log, without leaking a raw `E####` code.
fn format_lib_error(phase: &str, e: &libfreemkv::Error) -> String {
    use libfreemkv::Error;

    // Drive read failures carry SCSI sense — reuse the sense decoder so the
    // operator gets the same "media damage / power-cycle the drive" guidance
    // the pass-error path produces, rather than a bare sector dump.
    if e.scsi_sense().is_some() {
        return format_pass_error(phase, e);
    }

    let detail = match e {
        // ── Drive / device layer (1xxx) ───────────────────────────────
        Error::DeviceNotFound { .. } => {
            "the drive could not be found. It may have been unplugged or moved to a \
             different device path — check the connection and rescan."
        }
        Error::DevicePermission { .. } => {
            "autorip is not allowed to access the drive. The container needs \
             `privileged: true` and `/dev:/dev` mounted — verify the compose file."
        }
        Error::DeviceNotReady { .. } => {
            "the drive is not ready. Make sure a disc is loaded and seated, wait a few \
             seconds, then retry."
        }
        Error::DeviceResetFailed { .. } | Error::DeviceLocked { .. } => {
            "the drive is wedged and could not be reset. Eject the disc and \
             power-cycle the drive, then retry."
        }
        Error::ScsiInterfaceUnavailable { .. } | Error::IoKitPluginFailed { .. } => {
            "autorip could not open a command channel to the drive. The container \
             needs `privileged: true` and `/dev:/dev` — verify the compose file, then \
             restart the container."
        }
        Error::UnsupportedDrive { .. } | Error::ProfileParse => {
            "this drive model is not supported for ripping."
        }
        Error::UnsupportedPlatform { .. } | Error::PlatformNotImplemented { .. } => {
            "this operation is not supported on this platform."
        }

        // ── Unlock / signature (3xxx) ─────────────────────────────────
        Error::UnlockFailed | Error::SignatureMismatch { .. } => {
            "the drive could not be unlocked for raw reads. It may need a firmware \
             flash or a supported drive to rip this disc."
        }

        // ── SCSI / IO without sense data ──────────────────────────────
        Error::ScsiError { .. } | Error::InvalidCdbLength { .. } => {
            "the drive returned a command error. Eject the disc and power-cycle the \
             drive, then retry."
        }
        Error::IoError { source } => return format!("{phase} failed: {source}"),

        // ── Disc structure / scan (6xxx) ──────────────────────────────
        Error::DiscRead { .. } => {
            "the disc could not be read. It may be dirty, scratched, or unreadable in \
             this drive — clean the disc and retry, or try another drive."
        }
        Error::UdfNotFound { .. } => {
            "no filesystem was found on the disc. It may be blank, unfinalized, or not \
             a video disc."
        }
        Error::MplsParse | Error::ClpiParse | Error::IfoParse | Error::DiscTitleRange { .. } => {
            "the disc's title structure could not be read. The disc may be damaged or \
             use an unsupported layout."
        }
        Error::NoStreams => {
            "no playable video was found on the disc. It may be damaged or not a \
             standard video disc."
        }
        Error::MkvInvalid => "the muxed output is not a valid MKV file.",
        Error::Mp4Invalid => "the source MP4 file is malformed or truncated.",
        Error::Halted => "the rip was stopped.",
        Error::MapfileInvalid { .. } => {
            "the recovery map for a previous attempt is corrupt. Start a fresh rip to \
             rebuild it."
        }

        // ── Decryption (7xxx) — defer to the AACS/CSS humanizer ────────
        Error::DecryptFailed
        | Error::AacsKeyFileUnreadable
        | Error::AacsNoUsableHostCert
        | Error::CssKeyMissing
        | Error::CssAuthFailed
        | Error::NoDiscKey { .. }
        | Error::WholeDiscKeyMissing
        | Error::AacsVidNeedsDisc
        | Error::KeyServiceUnavailable
        | Error::KeyServiceUnauthorized
        | Error::KeyServiceRateLimited => {
            return format!(
                "{phase} failed: {}",
                strip_error_prefix(&aacs_failure_message(Some(e)))
            );
        }

        // ── Mux / output (9xxx) ───────────────────────────────────────
        Error::IsoTooLarge { .. } | Error::DiscCapacityOverflow | Error::DiscCapacityMalformed => {
            "the drive reported an unusable disc capacity. Clean the disc and retry, or \
             try another drive."
        }
        Error::NoMetadata => "the disc carries no usable metadata.",
        Error::MuxEmpty => {
            "the disc produced no output. It may be damaged or contain no playable \
             video."
        }
        Error::HevcParamParse
        | Error::PesInvalidMagic
        | Error::PesFrameTooLarge { .. }
        | Error::PesTrackTooLarge { .. }
        | Error::MuxTrackRange { .. }
        | Error::M2tsPacketMalformed => {
            "the disc's video stream could not be parsed for muxing. The source may be \
             damaged or use an unsupported encoding."
        }
        Error::DemuxThreadPanicked
        | Error::PipelineJoinTimeout
        | Error::PipelineConsumerPanicked
        | Error::PipelineConsumerGone
        | Error::SweepConsumerGone => {
            "the mux pipeline failed unexpectedly. Retry the rip; if it persists, \
             enable debug logging via /api/debug and report it."
        }

        // Any other variant: a generic, honest line with no leaked code.
        _ => {
            "an unexpected error occurred. Enable debug logging via /api/debug for \
             details."
        }
    };

    format!("{phase} failed: {detail}")
}

// Open a drive during transport-failure recovery with exponential backoff; `None` once
// exhausted or stopped. TODO(step1-followup): not yet folded into DiscSession::recover.
fn open_drive_with_backoff(
    device: &str,
    attempt: u32,
    path: &str,
    transport_recovery_delay_secs: u64,
) -> Option<libfreemkv::Drive> {
    for retry in 0..3 {
        match freemkv_engine::drive::open(std::path::Path::new(path)) {
            Ok(d) => return Some(d),
            Err(e) if retry < 2 => {
                let backoff_secs = transport_recovery_delay_secs * (1u64 << retry);
                crate::server::log::device_log(
                    device,
                    &format!(
                        "Pass 1 attempt {attempt}: Drive::open({}) failed, retrying in {}s: error={} sense_key={:?} ASC={:?}",
                        path,
                        backoff_secs,
                        e.code(),
                        e.scsi_sense().map(|s| s.sense_key),
                        e.scsi_sense().map(|s| s.asc)
                    ),
                );
                if !wait_unless_stopped(device, std::time::Duration::from_secs(backoff_secs)) {
                    crate::server::log::device_log(
                        device,
                        &format!("Pass 1 attempt {attempt}: Drive::open retry cancelled (halt)"),
                    );
                    return None;
                }
            }
            Err(e) => {
                crate::server::log::device_log(
                    device,
                    &format!(
                        "Pass 1 attempt {attempt}: Drive::open({}) failed strategy=transport_failure_recovery error={} sense_key={:?} ASC={:?} — recovery path exhausted",
                        path,
                        e.code(),
                        e.scsi_sense().map(|s| s.sense_key),
                        e.scsi_sense().map(|s| s.asc)
                    ),
                );

                let failure_category = if e.code() == 4000 {
                    "SCSI_ERROR"
                } else if e.code() >= 1000 && e.code() < 2000 {
                    "DEVICE_ERROR"
                } else {
                    &format!("ERROR_CODE_{}", e.code())
                };

                crate::server::log::device_log(
                    device,
                    &format!(
                        "STRATEGY_FAILURE: transport_failure_recovery FAILED at Drive::open category={} error_code={}",
                        failure_category,
                        e.code()
                    ),
                );

                return None;
            }
        }
    }

    // Unreachable: the loop either returns Some on success or None on the
    // final Err arm. Treat any fall-through as exhausted.
    None
}

// Emit the post-`Drive::init` failure diagnostic for a transport
// recovery re-open: ILLEGAL REQUEST (ASC=0x20) means wedged firmware
// (USER_ACTION_REQUIRED), else a plain STRATEGY_FAILURE.
fn log_init_recovery_failure(device: &str, e: &libfreemkv::Error) {
    let is_wedged_firmware =
        e.code() == 4000 && e.scsi_sense().map(|s| s.asc == 0x20).unwrap_or(false);

    if is_wedged_firmware {
        crate::server::log::device_log(
            device,
            "STRATEGY_FAILURE: transport_failure_recovery FAILED at Drive::init with ILLEGAL_REQUEST (ASC=0x20) — drive firmware wedged",
        );
        crate::server::log::device_log(
            device,
            "USER_ACTION_REQUIRED: Eject disc and physically power-cycle USB optical drive to clear firmware state before retrying",
        );
    } else {
        let failure_category = if e.code() == 4000 {
            "SCSI_ERROR".to_string()
        } else {
            format!("ERROR_CODE_{}", e.code())
        };

        crate::server::log::device_log(
            device,
            &format!(
                "STRATEGY_FAILURE: transport_failure_recovery FAILED at Drive::init category={} error_code={}",
                failure_category,
                e.code()
            ),
        );
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mod_held_eject_tests.rs"]
mod held_eject_tests;

#[cfg(test)]
#[path = "mod_insert_tick_tests.rs"]
mod insert_tick_tests;

#[cfg(test)]
#[path = "mod_poll_presence_tests.rs"]
mod poll_presence_tests;

#[cfg(test)]
#[path = "mod_teardown_poison_tests.rs"]
mod teardown_poison_tests;

#[cfg(test)]
#[path = "mod_tv_plan_tests.rs"]
mod tv_plan_tests;

#[cfg(test)]
#[path = "mod_probe_failure_tests.rs"]
mod probe_failure_tests;

#[cfg(test)]
#[path = "mod_quarantine_persist_tests.rs"]
mod quarantine_persist_tests;

#[cfg(test)]
#[path = "mod_stop_and_claim_tests.rs"]
mod stop_and_claim_tests;
