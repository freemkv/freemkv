//! Where AACS keys come from.
//!
//! libfreemkv does no key lookup — its `KeySource`s resolve a disc's terminal
//! Unit Keys, driving the library's boil-down crypto. The sources are the
//! engine's key parameters ([`key_params`]): the one the operator picked in
//! settings — the local keydb, or the online key service — and only that one.
//!
//! A live drive scans KEYLESS, then resolves the rip's key set once ([`resolve_drive_keys`]);
//! a staged image opens through the engine ([`open_staged_image`]).

use std::path::{Path, PathBuf};

use freemkv_keysources::{KeySource, KeydbSource};
use libfreemkv::aacs::trace::ResolutionTrace;

use crate::server::config::Config;

// The keyserver URL gate is `freemkv_keysources::validate_keyserver_url` (https-only + address rule):
// settings save, `build_sources` and the probe all call it so they agree. web.rs keeps its own
// guard for other operator URLs; the probe uses it only to pin DNS.

/// How many 6144-byte aligned encrypted units a sample-needing source is given.
///
/// MUST be >= the online keyservice minimum: a request carrying fewer units is
/// SILENTLY SKIPPED by the online source (see [`libfreemkv::keysource::MIN_SAMPLE_UNITS`]),
/// recording no decode verdict, so the disc-less probe answers and the rip reports
/// a misleading generic "no key". Defined AS the floor so it tracks it, and the
/// compile-time assertion below turns any regression into a BUILD error.
pub const SAMPLE_UNITS: usize = libfreemkv::keysource::MIN_SAMPLE_UNITS;
const _: () = assert!(
    SAMPLE_UNITS >= libfreemkv::keysource::MIN_SAMPLE_UNITS,
    "autorip SAMPLE_UNITS must be >= the online keyservice's MIN_SAMPLE_UNITS \
     or every online key request is silently skipped",
);

/// What happened when resolving keys for a disc — carried back so the UI can
/// tell the user *why*, instead of a generic "missing keys".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// A source's key derived unit keys — the disc now carries keys.
    Resolved,
    /// Couldn't read the disc's key files, or the disc reported no titles.
    MissingInputs,
    /// No configured source produced a key that decrypts this disc. A source
    /// that *failed* (e.g. an unreachable key service) is NOT distinguished
    /// from one that simply had no key — both land here, so this is never
    /// enough on its own to tell an operator why. For the online source pair it
    /// with [`ServiceReachability`] (see [`take_online_decode_reachability`]);
    /// the per-source [`ResolutionTrace`] carries the finer-grained walk.
    NoKey,
}

/// The configured keydb path, or the service's standard default location.
///
/// This is the single source of truth for *where autorip's keydb lives* — both
/// the key *reads* (the scan/decrypt path) and the keydb *writes* (first-boot
/// download, daily refresh, the web "Update KEYDB" button) MUST resolve through
/// here so they agree. See [`save_keydb`] / [`keydb_exists`].
pub fn keydb_path(cfg: &Config) -> PathBuf {
    resolve_keydb(
        cfg.keydb_path.as_deref(),
        &cfg.autorip_dir,
        legacy_home_keydb(),
        &|p| p.exists(),
    )
}

/// Pure keydb-path resolution — injectable `exists`/`legacy` so the whole
/// decision table is unit-testable with no env or filesystem. Order: (1) an
/// explicit config path wins; (2) else the canonical `<autorip_dir>/keydb.cfg`
/// (`/config/keydb.cfg` in Docker) — NOT `$HOME`-derived, which the HOME-less
/// container collapsed to a stray relative path so `/config/keydb.cfg` was never
/// read (issue #46); (3) else a pre-existing legacy `$HOME/.config/freemkv/
/// keydb.cfg`, ONLY as an upgrade migration; (4) else canonical (write target).
fn resolve_keydb(
    configured: Option<&str>,
    autorip_dir: &str,
    legacy: Option<PathBuf>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> PathBuf {
    if let Some(p) = configured.filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    let canonical = PathBuf::from(autorip_dir).join("keydb.cfg");
    if exists(&canonical) {
        return canonical;
    }
    if let Some(legacy) = legacy.filter(|l| exists(l)) {
        return legacy;
    }
    canonical
}

// Legacy (pre-#46) default: `$HOME/.config/freemkv/keydb.cfg`. Consulted ONLY
// as a migration fallback when the canonical AUTORIP_DIR path has no file yet.
// An empty HOME (common in the container) yields None, never a stray relative path.
fn legacy_home_keydb() -> Option<PathBuf> {
    legacy_keydb_under(std::env::var_os("HOME"))
}

fn legacy_keydb_under(home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    home.filter(|h| !h.is_empty())
        .map(|home| PathBuf::from(home).join(".config/freemkv/keydb.cfg"))
}

/// Does autorip's keydb already exist at the service-canonical path?
///
/// The startup gate (daemon.rs) MUST use this — not an exe-local default — so the
/// "already have a keydb, skip download" decision is made against the file the
/// rip will actually load. Using the same resolver as the reads keeps the gate,
/// the writes, and the reads on one path. (Bug f750a5e fixed the reads but left
/// the startup gate and the writes on the exe-local path.)
pub fn keydb_exists(cfg: &Config) -> bool {
    keydb_path(cfg).exists()
}

/// Validate and persist raw keydb bytes to autorip's service-canonical path.
///
/// `KeydbSource::save` does the validation (zip/gz/plain extraction, entry-count
/// check) and the crash-safe atomic write (sibling-temp + fsync + rename)
/// straight to the path the source owns — here the service path resolved by
/// [`keydb_path`], where the reads also look. No relocation: the write target
/// and read target are the same path by construction.
///
/// Returns the `UpdateResult` (from `freemkv-keysources`) describing the write.
pub fn save_keydb(
    cfg: &std::sync::RwLock<Config>,
    data: &[u8],
) -> std::result::Result<freemkv_keysources::UpdateResult, libfreemkv::Error> {
    // Copy the inputs out under the guard; resolving (stats) and the save
    // (decompress + parse + fsync, up to 100 MiB) run after it is dropped.
    let (configured, autorip_dir) = {
        let c = cfg.read().unwrap_or_else(|e| e.into_inner());
        (c.keydb_path.clone(), c.autorip_dir.clone())
    };
    let path = resolve_keydb(
        configured.as_deref(),
        &autorip_dir,
        legacy_home_keydb(),
        &|p| p.exists(),
    );
    KeydbSource::new(path).save(data)
}

/// A boot-time warning for a stored online `keyserver_url` the rip will refuse
/// on scheme alone (e.g. an `http://` URL saved before https became mandatory).
/// Only in online mode: a local-mode rip never consults the URL. Pure — no DNS —
/// so it is safe on the startup path.
pub fn keyserver_url_startup_warning(cfg: &Config) -> Option<String> {
    if !uses_online(cfg) {
        return None;
    }
    let url = cfg.keyserver_url.trim();
    if url.is_empty() || url.starts_with("https://") {
        return None;
    }
    Some(format!(
        "WARNING: the stored Keyserver URL ({}) is not https://, so the online key source is DISABLED for every rip. Re-enter an https:// URL in Settings.",
        crate::server::webhook::webhook_url_origin(url)
    ))
}

/// ScanOptions for a **live-drive** structure scan. Lookup-free (the library
/// resolves no keys), plus the AACS host credentials for the authenticated
/// handshake — sourced from the keydb, *independent of `key_source`* (a locked
/// drive needs the cert even in online mode; an unlocked / firmware-unlocked
/// drive takes the OEM Volume-ID path and ignores them).
pub fn drive_scan_opts(cfg: &Config) -> libfreemkv::ScanOptions {
    drive_scan_opts_for_keydb(&keydb_path(cfg))
}

/// Live-drive [`ScanOptions`](libfreemkv::ScanOptions) with host credentials
/// sourced from a specific keydb path — the handshake's only keydb dependency
/// (an unlocked / firmware-unlocked drive ignores them).
pub fn drive_scan_opts_for_keydb(keydb: &Path) -> libfreemkv::ScanOptions {
    let host_certs = KeydbSource::new(keydb).host_certs();
    let credentials =
        (!host_certs.is_empty()).then_some(libfreemkv::DriveCredentials { host_certs });
    libfreemkv::ScanOptions {
        credentials,
        ..Default::default()
    }
}

/// The key source the user picked in settings, and only that one: `key_source = "online"`
/// asks the online key service; otherwise the local keydb.
pub fn key_params(cfg: &Config) -> freemkv_engine::KeyParams {
    crate::plan_core::key_params(&key_settings(cfg)).params()
}

/// The server's key settings as the front-end-neutral ones: `key_source = "online"` asks
/// only the key service, anything else only the local keydb.
pub fn key_settings(cfg: &Config) -> crate::plan_core::KeySettings {
    crate::plan_core::KeySettings {
        keydb_path: Some(keydb_path(cfg).to_string_lossy().into_owned()),
        key_url: Some(cfg.keyserver_url.trim().to_string()),
        key_auth: Some(cfg.keyserver_secret.clone()),
        mode: if uses_online(cfg) {
            crate::plan_core::KeyMode::OnlineOnly
        } else {
            crate::plan_core::KeyMode::LocalOnly
        },
    }
}

/// The ordered key sources for `cfg` (see [`key_params`]). A keydb that does
/// not exist yet and no usable online URL leaves every disc at NO KEY, so that
/// case is logged loudly (issue #46).
pub fn build_sources(cfg: &Config) -> Vec<Box<dyn KeySource>> {
    let params = key_params(cfg);
    let sources = freemkv_engine::key_sources(&params);
    if params.online_only {
        if sources.is_empty() {
            tracing::warn!(
                phase = "key_resolve",
                "online key source selected but no usable https:// keyserver URL is set — every disc will report NO KEY until one is"
            );
        }
    } else if !keydb_path(cfg).exists() {
        tracing::warn!(
            phase = "key_resolve",
            keydb_path = %keydb_path(cfg).display(),
            "local key source selected but NO keydb.cfg at the resolved path — every disc will report NO KEY until a keydb exists here; set 'KEYDB.cfg Location' or place the file at this path"
        );
    }
    sources
}

/// Resolve a live drive's key set for `scope`, once, right after its scan (KU §2.1): every
/// title the rip produces. The set is memory only and keys every later step of the rip
/// (sweep, patch, the live or staged-image mux, the mux worker's open), none of which asks
/// a key source again. `seed` is a set the rip already holds: its keys join the pool
/// first, so only what it does not cover is asked for. `Err` refuses before any output.
pub fn resolve_drive_keys(
    cfg: &Config,
    disc: &libfreemkv::Disc,
    drive: &mut dyn libfreemkv::SectorSource,
    scope: libfreemkv::keys::KeyScope,
    seed: Option<&libfreemkv::keys::KeyRing>,
    halt: Option<&libfreemkv::Halt>,
) -> Result<libfreemkv::keys::KeyRing, libfreemkv::Error> {
    warn_if_no_key_source(cfg);
    let factory = freemkv_engine::key_source_factory(&key_params(cfg));
    resolve_with(disc, drive, scope, &factory, seed, halt)
}

// `resolve_drive_keys` over given sources (the tests inject counting ones).
fn resolve_with(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: libfreemkv::keys::KeyScope,
    factory: &libfreemkv::KeySourceFactory,
    seed: Option<&libfreemkv::keys::KeyRing>,
    halt: Option<&libfreemkv::Halt>,
) -> Result<libfreemkv::keys::KeyRing, libfreemkv::Error> {
    // Drain an earlier decode verdict so the caller's take sees only this resolve's.
    let _ = take_online_decode_reachability();
    let (set, trace) =
        freemkv_engine::keys::resolve_for_rip_traced(disc, reader, scope, factory, seed, halt);
    let hash = disc.aacs.as_ref().map_or("", |a| a.disc_hash.as_str());
    log_key_walk(&trace, hash);
    let set = set?;
    let st = set.status();
    tracing::info!(
        phase = "key_resolve",
        requests = st.requests,
        proven = st.proven,
        keyed = st.keyed,
        lazy = st.lazy,
        origin = st.origin.unwrap_or("-"),
        forensic = ?st.forensic,
        "keys resolved up front for the rip"
    );
    Ok(set)
}

// A keydb that does not exist and no usable online URL leave every disc at NO KEY: say so
// loudly (issue #46).
fn warn_if_no_key_source(cfg: &Config) {
    let _ = build_sources(cfg);
}

// A fresh rip's key set, from its `.ripped` hand-off to the mux worker's open of the same
// ISO (the worker has no drive). Process memory only (J6): a restart forgets it.
static RIP_KEYS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, libfreemkv::keys::KeyRing>>,
> = std::sync::LazyLock::new(Default::default);

fn rip_keys_map()
-> std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, libfreemkv::keys::KeyRing>> {
    RIP_KEYS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keep the rip's key set for the staged `iso` until its mux is done (memory only).
pub fn hold_rip_keys(iso: &Path, keys: libfreemkv::keys::KeyRing) {
    rip_keys_map().insert(iso.to_path_buf(), keys);
}

/// The key set held for `iso`, if this process ripped it.
pub fn rip_keys_for(iso: &Path) -> Option<libfreemkv::keys::KeyRing> {
    rip_keys_map().get(iso).cloned()
}

/// Drop the key set held for `iso` (its mux delivered).
pub fn forget_rip_keys(iso: &Path) {
    rip_keys_map().remove(iso);
}

/// Where a staged image's keys come from (KU §3.2).
pub enum StagedKeys {
    /// The rip's up-front set (from its drive, or the inserted disc's scan): used as-is
    /// where it covers the titles, with no key-service call; the sources are asked only
    /// for what it lacks (e.g. forensic keys it left Pending, asked once from the image).
    Rip(libfreemkv::keys::KeyRing),
    /// No set in hand (a resume after a restart): the key chain, asked once, up front.
    /// `vid` only from a drive in hand; the mapfile holds none (J6).
    Resolve { vid: Option<[u8; 16]> },
}

/// Open a staged disc image through the engine, ready to mux `titles` from.
///
/// `disc` is the image's own scan (it is not scanned again). The keys for every one of
/// `titles` resolve here, once, before any output; no later mux of the image asks a key
/// source again. `Err` refuses before any output: E7022/E7032 (no key), E7026 (FMTS
/// forensic keys), E7028–30 (a key source failed), E7034 (only the disc's Volume ID can
/// finish the keys: insert the disc).
pub fn open_staged_image(
    cfg: &Config,
    iso: &Path,
    disc: libfreemkv::Disc,
    titles: &[usize],
    keys: StagedKeys,
    halt: Option<libfreemkv::Halt>,
) -> Result<freemkv_engine::OpenedImage, libfreemkv::Error> {
    let factory = freemkv_engine::key_source_factory(&key_params(cfg));
    let (input, vid) = match keys {
        StagedKeys::Rip(set) => (freemkv_engine::KeyInput::Seeded(factory, set), None),
        StagedKeys::Resolve { vid } => (freemkv_engine::KeyInput::Resolve(factory), vid),
    };
    open_staged(iso, disc, titles, input, vid, halt)
}

// `open_staged_image` over an already-built key input (the tests inject their sources).
fn open_staged(
    iso: &Path,
    disc: libfreemkv::Disc,
    titles: &[usize],
    keys: freemkv_engine::KeyInput,
    vid: Option<[u8; 16]>,
    halt: Option<libfreemkv::Halt>,
) -> Result<freemkv_engine::OpenedImage, libfreemkv::Error> {
    // Drain an earlier decode verdict so the caller's take sees only this resolve's.
    let _ = take_online_decode_reachability();
    let src = freemkv_engine::ImageSource::Iso(iso.to_path_buf());
    let opts = freemkv_engine::OpenImageOptions {
        keys,
        disc: Some(disc),
        scope: Some(libfreemkv::keys::KeyScope::Titles(titles.to_vec())),
        vid,
        halt,
    };
    let hash = disc_hash_of(&opts);
    let (opened, trace) = freemkv_engine::open_image_with_traced(&src, opts);
    // The walk, always: on a refusal it is the operator's only view of why a key missed.
    log_key_walk(&trace, &hash);
    let mut opened = opened?;
    // The set covers every title this open muxes; with no sources kept, a mux outside
    // them refuses instead of asking a key source mid-rip.
    opened.sources = None;
    Ok(opened)
}

fn disc_hash_of(opts: &freemkv_engine::OpenImageOptions) -> String {
    opts.disc
        .as_ref()
        .and_then(|d| d.aacs.as_ref())
        .map_or_else(String::new, |a| a.disc_hash.clone())
}

// The per-source key walk, always (success or refusal): the operator's only view of why a
// key missed. A matched entry also logs its shape (booleans and lengths, no key material).
fn log_key_walk(trace: &ResolutionTrace, disc_hash: &str) {
    for line in render_resolution_trace(trace, disc_hash) {
        tracing::info!(phase = "key_resolve", "{line}");
    }
    for step in &trace.keys {
        if let Some(m) = step.matched_entry {
            tracing::info!(
                phase = "key_resolve",
                source = %step.who,
                disc_hash,
                has_vuk = m.has_vuk,
                has_unit_keys = m.has_unit_keys,
                unit_keys_len = m.unit_keys_len,
                has_media_key = m.has_media_key,
                has_keydb_vid = m.has_keydb_vid,
                enc_title_keys_len = m.enc_title_keys_len,
                vid_available = m.vid_available,
                "matched keydb entry shape"
            );
        }
    }
}

/// Whether the rip asks the remote key service: only when `key_source` is
/// "online". A saved URL in local mode is never consulted.
pub fn uses_online(cfg: &Config) -> bool {
    cfg.key_source == "online"
}

/// What the online key service actually said about a disc — the verdict the
/// operator-facing message is written from.
///
/// The point of the enum is that "we never got an answer" and "we got a definitive answer of
/// *no*" are DIFFERENT OUTCOMES and must never share a message: only
/// [`Unreachable`](Self::Unreachable) / [`ServerError`](Self::ServerError) /
/// [`RateLimited`](Self::RateLimited) are worth retrying, and only
/// [`NoKeyForDisc`](Self::NoKeyForDisc) means the disc will never resolve from this service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceReachability {
    /// The service answered normally (2xx / 3xx) and simply held no key — or
    /// the verdict came from the bounded reachability probe, which proves the
    /// service is up but says NOTHING about this particular disc. Either way
    /// the ordinary "no key source has a key for this disc" text applies.
    Answered,
    /// Transport failure — connection refused, DNS failure, timeout, TLS error.
    /// Nothing answered, so nothing is known about this disc. Retryable.
    Unreachable,
    /// HTTP 5xx — the service was reached but failed on its own side, so it
    /// never got as far as an answer about this disc. Retryable.
    ServerError(u16),
    /// HTTP 429 — reached, but refusing requests for now (rate limit / quota).
    /// It said nothing about this disc. Retryable, after a wait.
    RateLimited,
    /// HTTP 422 — reached, licensed, and it DEFINITIVELY resolved no key for
    /// this disc (it exhausted every candidate source before answering).
    /// Terminal: retrying cannot change the answer.
    NoKeyForDisc,
    /// HTTP 404 — the request was refused as unlicensed / unknown, so the
    /// service never looked for a key. Terminal until the licence or URL is
    /// fixed; retrying unchanged cannot help.
    NotLicensed,
    /// HTTP 401 / 403 — the service rejected the configured credentials, so it
    /// never looked for a key. Terminal until the access token is fixed.
    Unauthorized(u16),
    /// Some other non-2xx status we have no specific meaning for. Terminal as
    /// far as automatic retry goes — report the status rather than guess.
    Unexpected(u16),
    /// The configured key-service URL could not be used at all (empty, wrong
    /// scheme, or an unreachable address), so the service was never asked.
    /// A standing misconfiguration, not an outage — terminal.
    NotAsked,
}

impl ServiceReachability {
    /// True for the retryable verdicts — the ones where the service never
    /// delivered a verdict about this disc and a later attempt genuinely may.
    /// A definitive no-key ([`NoKeyForDisc`](Self::NoKeyForDisc)) is NOT
    /// transient: retrying it is pointless work.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            ServiceReachability::Unreachable
                | ServiceReachability::ServerError(_)
                | ServiceReachability::RateLimited
        )
    }

    /// The HTTP status behind this verdict, when there was one. Carried so the
    /// operator-facing message can quote it for support without the classifier
    /// having to guess a cause. `None` when nothing answered.
    pub fn http_status(self) -> Option<u16> {
        match self {
            ServiceReachability::ServerError(code)
            | ServiceReachability::Unauthorized(code)
            | ServiceReachability::Unexpected(code) => Some(code),
            ServiceReachability::RateLimited => Some(429),
            ServiceReachability::NoKeyForDisc => Some(422),
            ServiceReachability::NotLicensed => Some(404),
            ServiceReachability::Answered
            | ServiceReachability::Unreachable
            | ServiceReachability::NotAsked => None,
        }
    }
}

/// The observable result of a single reachability probe, decoupled from the
/// HTTP client so the [`classify_reachability`] mapping can be unit-tested
/// against mocked outcomes (502/timeout → down, 429 → quota, 404/422 → no-key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The server returned an HTTP response with this status code.
    Status(u16),
    /// No HTTP response at all — connection refused, timed out, DNS/TLS error.
    Transport,
}

/// Map a **probe** outcome to a [`ServiceReachability`] verdict — transport →
/// [`Unreachable`](ServiceReachability::Unreachable), 5xx →
/// [`ServerError`](ServiceReachability::ServerError), 429 →
/// [`RateLimited`](ServiceReachability::RateLimited), anything else →
/// [`Answered`](ServiceReachability::Answered). DELIBERATELY coarser than the decode-side
/// mapping: the probe carries no disc, so it can never yield a per-disc verdict.
pub fn classify_reachability(outcome: ProbeOutcome) -> ServiceReachability {
    match outcome {
        ProbeOutcome::Transport => ServiceReachability::Unreachable,
        ProbeOutcome::Status(429) => ServiceReachability::RateLimited,
        ProbeOutcome::Status(code) if (500..=599).contains(&code) => {
            ServiceReachability::ServerError(code)
        }
        ProbeOutcome::Status(_) => ServiceReachability::Answered,
    }
}

// keysources' per-host DNS-cap refusal (GuardFail::Unreachable). Its other lookup failures share
// web's RESOLVE_* texts. Follow-up: keysources should export the typed transient/permanent kind.
const KEYSOURCES_DNS_CAP_MSG: &str = "too many concurrent DNS resolutions in flight for this host";

// True when a keyserver-URL validation error (keysources' or web's) is a failed lookup, not a
// permanent verdict on the URL. The ONE classifier for both `build_sources` and the probe.
fn url_error_is_transient(err: &str) -> bool {
    crate::server::web::is_transient_resolve_error(err) || err == KEYSOURCES_DNS_CAP_MSG
}

// What a URL we could not even validate says about the key SERVICE: a permanent verdict (bad
// scheme/host/SSRF) means it was never asked, a failed DNS lookup means it was unreachable.
fn reachability_for_unprobeable_url(err: &str) -> ServiceReachability {
    if url_error_is_transient(err) {
        ServiceReachability::Unreachable
    } else {
        ServiceReachability::NotAsked
    }
}

/// Read timeout for the reachability probe. Short and bounded — we only need to
/// learn *whether* the service answers, not to complete a key exchange.
const PROBE_TIMEOUT_SECS: u64 = 8;

/// ONE bounded reachability probe against `keyserver_url`, classified. A cheap
/// `POST` (empty body) with short timeouts and autorip's usual SSRF-pinning; any
/// HTTP answer proves the service is UP, only transport / 5xx / 429 are transient.
/// POST (not GET) is load-bearing: the key service is POST-only, so a GET
/// transport-fails (status `000`) and is misread as "DOWN", firing the pointless
/// 3x outage-retry on every definitive 4xx. A POST gets a real status → answered
/// → `Answered`. Empty/SSRF-blocked URLs report [`ServiceReachability::NotAsked`];
/// see `reachability_for_unprobeable_url`.
pub fn probe_online_reachability(cfg: &Config) -> ServiceReachability {
    let url = cfg.keyserver_url.trim();
    if url.is_empty() {
        return ServiceReachability::NotAsked;
    }
    // SSRF gate: the SAME validator `build_sources` gates the online source on,
    // so the probe and the key-resolve path can never disagree about whether a
    // URL is allowed. (The classifier lives once, in the keysources crate.)
    if let Err(e) = freemkv_keysources::validate_keyserver_url(url) {
        return reachability_for_unprobeable_url(&e);
    }
    // Pin DNS for the probe POST itself (anti-rebind between validate and
    // connect); `validate_fetch_url` re-resolves and returns the addresses to
    // pin the guarded agent to.
    let pinned = match crate::server::web::validate_fetch_url(url) {
        Ok(addrs) => addrs,
        Err(e) => return reachability_for_unprobeable_url(&e),
    };
    // Same pinned-resolver hardening as every operator-URL fetch in autorip
    // (`guarded_agent` owns it). This probe keeps its own short idle bound —
    // a longer shared default would defeat the point of this call site.
    let agent = crate::server::web::guarded_agent_with_timeouts(
        pinned,
        std::time::Duration::from_secs(4),
        std::time::Duration::from_secs(PROBE_TIMEOUT_SECS),
        std::time::Duration::from_secs(PROBE_TIMEOUT_SECS),
    );
    let outcome = match agent.post(url).send_empty() {
        Ok(resp) => ProbeOutcome::Status(resp.status().as_u16()),
        Err(ureq::Error::StatusCode(code)) => ProbeOutcome::Status(code),
        // ureq 3 fans v2's `Transport` out across `Io`, `Timeout`, `Tls`,
        // `HostNotFound` and more (enum is non_exhaustive). All mean the
        // same thing to this probe: the service never answered.
        Err(_) => ProbeOutcome::Transport,
    };
    classify_reachability(outcome)
}

/// The reachability verdict from the most recent online `/decode` POST on THIS
/// thread — the REAL decode's HTTP outcome — or `None` when no decode reached
/// the network since the last read (online source not attempted, or a path that
/// never POSTed). The redundant-probe eliminator: the ripper classifies
/// genuine-no-key vs transient from THIS verdict instead of a second empty POST
/// to the POST-only `/decode` (which logged a spurious `404` after every real
/// no-key). Reading CONSUMES the value; the call site probes only on `None`.
/// See `reachability_from_decode` for the mapping.
pub fn take_online_decode_reachability() -> Option<ServiceReachability> {
    freemkv_keysources::take_last_decode_reachability().map(reachability_from_decode)
}

/// Map a keysources [`DecodeReachability`](freemkv_keysources::DecodeReachability) — the real
/// `/decode` POST, which DID carry this disc — to a per-disc [`ServiceReachability`]. Unlike
/// [`classify_reachability`], a 422 here is a DEFINITIVE no-key for this disc, not an outage.
fn reachability_from_decode(
    outcome: freemkv_keysources::DecodeReachability,
) -> ServiceReachability {
    use freemkv_keysources::DecodeReachability as D;
    match outcome {
        D::Transport => ServiceReachability::Unreachable,
        D::Status(429) => ServiceReachability::RateLimited,
        D::Status(422) => ServiceReachability::NoKeyForDisc,
        D::Status(404) => ServiceReachability::NotLicensed,
        D::Status(code @ (401 | 403)) => ServiceReachability::Unauthorized(code),
        D::Status(code) if (500..=599).contains(&code) => ServiceReachability::ServerError(code),
        D::Status(code) if (200..=399).contains(&code) => ServiceReachability::Answered,
        D::Status(code) => ServiceReachability::Unexpected(code),
    }
}

/// Render a [`ResolutionTrace`] into human-readable `who > node > … > OUTCOME`
/// lines — one per unlocker and per key source consulted. The library trace is
/// English-free typed enums; ALL English mapping lives here in the app layer.
/// Shown on both success and failure so the operator always sees the walk.
///
/// `disc_hash` is the identifier being looked up, woven into a true-miss verdict
/// so the line is self-diagnosing (issue #46).
pub fn render_resolution_trace(trace: &ResolutionTrace, disc_hash: &str) -> Vec<String> {
    use libfreemkv::aacs::trace::UnlockOutcome;

    let mkb = |m: Option<u32>| match m {
        Some(n) => format!(" (MKBv{n})"),
        None => String::new(),
    };
    let mut lines = Vec::new();

    for step in &trace.unlock {
        // `who` is the unlocker's own name() — printed verbatim (no enum to map).
        let outcome = match step.outcome {
            UnlockOutcome::Unlocked => "UNLOCKED".to_string(),
            UnlockOutcome::FirmwareNotUnlockable => "firmware not unlockable".to_string(),
            UnlockOutcome::NoUsableHostCert { mkb: m } => {
                format!("no usable host cert{}", mkb(m))
            }
            UnlockOutcome::CertRevoked { mkb: m } => format!("host cert revoked{}", mkb(m)),
            UnlockOutcome::HandshakeRejected => "handshake rejected".to_string(),
            UnlockOutcome::VidUnavailable => "Volume ID unavailable".to_string(),
        };
        lines.push(format!("unlock: {} > {outcome}", step.who));
    }

    for step in &trace.keys {
        lines.push(format!("key: {}", render_key_step(step, disc_hash)));
    }

    lines
}

/// Render one key source's step into an ACTIONABLE verdict (issue #46). Two
/// de-conflated no-key shapes get a self-diagnosing line; everything else uses
/// the generic typed-node join.
fn render_key_step(step: &libfreemkv::aacs::trace::KeyStep, disc_hash: &str) -> String {
    use libfreemkv::aacs::trace::{KeyNode, KeyOutcome as KO};

    let who = &step.who;

    // TRUE MISS: the disc hash was not in the store. Name the hash AND the store
    // size so a reporter can confirm a wrong-pressing without extra logging.
    if step.path.as_slice() == [KeyNode::NoEntry] {
        return match step.store_entries {
            Some(n) => {
                format!(
                    "{who} > no entry > disc hash {disc_hash} not in keydb ({n} entries loaded)"
                )
            }
            None => format!("{who} > no entry > NO KEY"),
        };
    }

    // MATCHED, KEY NEEDS THE VID: the entry derives the key only with the disc's Volume ID,
    // which no drive supplied (an image resume after a restart: insert the disc).
    if step.outcome == KO::MissingVid && step.path.first() == Some(&KeyNode::MatchedDisc) {
        return format!(
            "{who} > matched disc > key needs the disc's Volume ID, not in hand > MISSING VID"
        );
    }

    // MATCHED BUT NO KEY: the disc WAS found; say WHY nothing derived.
    if step.outcome == KO::NoKey && step.path.first() == Some(&KeyNode::MatchedDisc) {
        if step.path.contains(&KeyNode::NoVid) {
            return format!("{who} > matched disc > no VID available > NO KEY");
        }
        if let Some(m) = step.matched_entry {
            return format!(
                "{who} > matched disc > entry has no usable keys \
                 (vuk={} unit_keys={} enc_title_keys={} media_key={}) > NO KEY",
                m.has_vuk, m.unit_keys_len, m.enc_title_keys_len, m.has_media_key
            );
        }
        return format!("{who} > matched disc > entry has no usable keys > NO KEY");
    }

    // GENERIC: the typed-node walk (success paths, source failures, etc.).
    let nodes: Vec<&str> = step
        .path
        .iter()
        .map(|n| match n {
            KeyNode::MatchedDisc => "matched disc",
            KeyNode::NoEntry => "no entry",
            KeyNode::NoDerivableKey => "no derivable key",
            KeyNode::FoundUnitKeys => "found unit keys",
            KeyNode::FoundVuk => "found VUK",
            KeyNode::FoundMediaKey => "found media key",
            KeyNode::NeedVid => "need VID",
            KeyNode::VidFromUnlock => "VID from drive",
            KeyNode::VidFromKeydb => "VID from keydb",
            KeyNode::NoVid => "no VID",
            KeyNode::DerivedVuk => "derived VUK",
            KeyNode::DerivedUnitKeys => "derived unit keys",
        })
        .collect();
    let outcome = match step.outcome {
        KO::Resolved => "RESOLVED",
        KO::MissingVid => "MISSING VID",
        KO::NoKey => "NO KEY",
    };
    let mut parts = vec![who.clone()];
    parts.extend(nodes.into_iter().map(str::to_string));
    parts.push(outcome.to_string());
    parts.join(" > ")
}

#[cfg(test)]
#[path = "keysource_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "keysource_ku_e1_tests.rs"]
mod ku_e1_tests;
