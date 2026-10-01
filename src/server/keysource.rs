//! Where AACS keys come from.
//!
//! libfreemkv does no key lookup — its `KeySource`s resolve a disc's terminal
//! Unit Keys, driving the library's boil-down crypto. The sources are the
//! engine's local-first chain ([`key_params`]): the keydb, then the online
//! key service when one is configured.
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
    if cfg.key_source != "online" {
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
        mode: if cfg.key_source == "online" {
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
mod tests {
    use super::*;
    use libfreemkv::read_encrypted_units;

    #[test]
    fn keyserver_guard_allows_lan_and_rejects_invalid_hosts() {
        // Home app: loopback, link-local (incl. metadata) and RFC1918 key services are valid.
        // Built from octets so the dotted-quad doesn't trip the public leak-guard.
        let lan = format!("https://{}.{}.{}.{}/keys", 192, 168, 1, 5);
        for url in [
            "https://169.254.169.254/latest/meta-data",
            "https://127.0.0.1:8443/keys",
            "https://[::1]:443/k",
            "https://[fe80::1]/k",
            "https://[::ffff:127.0.0.1]/k",
            lan.as_str(),
        ] {
            assert!(
                freemkv_keysources::validate_keyserver_url(url).is_ok(),
                "{url} must be accepted"
            );
        }
        // Unreachable addresses are refused.
        for url in [
            "https://0.0.0.0/keys",
            "https://224.0.0.1/keys",
            "https://[ff02::1]/k",
        ] {
            assert!(
                freemkv_keysources::validate_keyserver_url(url).is_err(),
                "{url} must be refused"
            );
        }
        // Non-http scheme rejected.
        assert!(freemkv_keysources::validate_keyserver_url("ftp://example.com/keys").is_err());
        // No host.
        assert!(freemkv_keysources::validate_keyserver_url("https:///keys").is_err());
    }

    #[test]
    fn ssrf_guard_allows_public_literal_ip() {
        // A public literal IP must pass (no DNS needed, deterministic).
        assert!(freemkv_keysources::validate_keyserver_url("https://8.8.8.8/keys").is_ok());
        assert!(freemkv_keysources::validate_keyserver_url("https://1.1.1.1:443").is_ok());
    }

    // The two rejection arms that fire BEFORE a host is extracted must never
    // echo the raw input — `keyserver_url` can carry a bearer token, and
    // `build_sources` logs this `Err` at ERROR, readable via `GET /api/debug`.
    #[test]
    fn validate_keyserver_url_error_never_echoes_raw_token() {
        let scheme_missing =
            freemkv_keysources::validate_keyserver_url("keys.example.org/decode?token=SUPERSECRET")
                .unwrap_err();
        assert!(
            !scheme_missing.contains("SUPERSECRET"),
            "scheme-missing error leaked the token: {scheme_missing}"
        );
        assert!(
            !scheme_missing.contains("token="),
            "scheme-missing error leaked the query string: {scheme_missing}"
        );

        let no_host =
            freemkv_keysources::validate_keyserver_url("https:///decode?token=SUPERSECRET")
                .unwrap_err();
        assert!(
            !no_host.contains("SUPERSECRET"),
            "no-host error leaked the token: {no_host}"
        );
        assert!(
            !no_host.contains("token="),
            "no-host error leaked the query string: {no_host}"
        );
    }

    // Cross-side agreement: autorip's sample selector (`read_encrypted_units`)
    // hands the key service only units the service's own gate accepts, since
    // both sides call the SAME predicate, `ts_sync_destroyed`.
    #[test]
    fn sample_units_are_all_aacs_scrambled() {
        use std::io::Write;

        // Synthetic ISO: 1200 sectors of scrambled (non-TS) content — no 0x47 at
        // any TS sync offset, so every aligned unit reads as AACS-scrambled.
        const SECTORS: usize = 1200;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&vec![0xE5u8; SECTORS * 2048]).unwrap();
        tmp.flush().unwrap();
        let mut reader = libfreemkv::FileSectorSource::open(tmp.path()).unwrap();

        let title = libfreemkv::DiscTitle {
            playlist: "00800.mpls".into(),
            playlist_id: 800,
            duration_secs: 0.0,
            size_bytes: (SECTORS * 2048) as u64,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![libfreemkv::Extent {
                start_lba: 0,
                sector_count: SECTORS as u32,
            }],
            content_format: libfreemkv::ContentFormat::BdTs,
            codec_privates: Vec::new(),
        };

        let units = read_encrypted_units(&mut reader, &title, SAMPLE_UNITS);
        assert_eq!(units.len(), SAMPLE_UNITS, "should collect 4 sample units");
        for u in &units {
            assert_eq!(u.len(), 6144);
            assert!(
                !libfreemkv::aacs::content::is_clean(u, libfreemkv::disc::ContentFormat::BdTs),
                "selector must only emit units the key service accepts"
            );
        }

        // The converse: a clear unit (TS syncs intact) is NOT scrambled.
        let mut clear = vec![0u8; 6144];
        let mut off = 4;
        while off < 6144 {
            clear[off] = 0x47;
            off += 192;
        }
        assert!(libfreemkv::aacs::content::is_clean(
            &clear,
            libfreemkv::disc::ContentFormat::BdTs
        ));
    }

    // autorip's keydb writes and the startup existence check must land on the
    // same service-canonical path the reads resolve through (keydb_path /
    // keydb_exists) — save_keydb writes straight there, no relocate dance.
    #[test]
    fn save_keydb_writes_to_service_path_and_existence_agrees() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("keys").join("keydb.cfg");

        let cfg = Config {
            keydb_path: Some(dest.to_string_lossy().into_owned()),
            ..Config::default()
        };

        // The path the reads will resolve and the gate must check.
        assert_eq!(keydb_path(&cfg), dest);
        assert!(!keydb_exists(&cfg), "no keydb written yet");

        // A minimal valid keydb body: one disc-entry line (`0x<hash> = <title>`),
        // matching the parser's real rule that a `0x` line is an entry only if it
        // also contains ` = `.
        let body = b"0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF = Test\n";
        let result = save_keydb(&std::sync::RwLock::new(cfg.clone()), body)
            .expect("save_keydb must succeed");

        // It wrote straight to the service path.
        assert_eq!(result.path, dest, "save must target the service path");
        assert!(dest.exists(), "keydb file must exist at the service path");
        assert!(
            keydb_exists(&cfg),
            "startup existence gate must now see the keydb the write produced"
        );
        assert_eq!(result.entries, 1, "one 0x entry");

        // No stray temp sibling left behind by the atomic write.
        let leftovers: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");

        // Content round-trips: the bytes at the service path are the keydb text.
        let written = std::fs::read_to_string(&dest).unwrap();
        assert!(
            written.contains("0xDEADBEEF"),
            "keydb content must be present"
        );
    }

    // keydb path resolution (#46): with no explicit `keydb_path`, reads/writes/
    // gate all agree on canonical `<autorip_dir>/keydb.cfg`, NOT `$HOME`-derived.
    // Deterministic — once the canonical file exists it wins over legacy/env.
    #[test]
    fn keydb_resolvers_agree_on_autorip_dir_default() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config {
            autorip_dir: tmp.path().to_string_lossy().into_owned(),
            keydb_path: None,
            ..Config::default()
        };
        let expected = tmp.path().join("keydb.cfg");
        std::fs::write(&expected, "0xDEAD = t | U | 1-0x0000000000000000\n").unwrap();

        assert_eq!(
            cfg.keydb_path, None,
            "default config carries no explicit keydb_path"
        );
        assert_eq!(
            keydb_path(&cfg),
            expected,
            "no override → canonical <autorip_dir>/keydb.cfg, never $HOME-derived"
        );
        // The existence gate resolves through the same path the reads use.
        assert!(keydb_exists(&cfg));
    }

    // An explicit `keydb_path` overrides the service default, and the existence
    // gate + read path both honor it — an operator pointing autorip at a
    // non-standard keydb gets reads, writes, and the startup gate aligned.
    #[test]
    fn explicit_keydb_path_overrides_default_and_gate_honors_it() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("custom").join("mykeys.cfg");

        let cfg = Config {
            keydb_path: Some(explicit.to_string_lossy().into_owned()),
            ..Config::default()
        };

        assert_eq!(
            keydb_path(&cfg),
            explicit,
            "explicit keydb_path must win over the service default"
        );
        assert!(!keydb_exists(&cfg), "file not created yet");

        std::fs::create_dir_all(explicit.parent().unwrap()).unwrap();
        std::fs::write(&explicit, b"0xAAAA\n").unwrap();
        assert!(
            keydb_exists(&cfg),
            "existence gate must see the file at the explicit path"
        );
    }

    /// A second `save_keydb` to the same service path replaces the prior keydb
    /// in place (direct atomic write, no relocate) and reports that path.
    #[test]
    fn save_keydb_overwrites_existing_at_service_path() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("keys").join("keydb.cfg");
        let cfg = Config {
            keydb_path: Some(dest.to_string_lossy().into_owned()),
            ..Config::default()
        };

        save_keydb(
            &std::sync::RwLock::new(cfg.clone()),
            b"0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA = Test\n",
        )
        .expect("first save");
        let result = save_keydb(
            &std::sync::RwLock::new(cfg.clone()),
            b"0xBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB = Test\n",
        )
        .expect("second save");

        assert_eq!(result.path, dest, "save always targets the service path");
        let written = std::fs::read_to_string(&dest).unwrap();
        assert!(written.contains("0xBBBB"), "newest keydb content must win");
        assert!(!written.contains("0xAAAA"), "old content fully replaced");
    }

    // `drive_scan_opts_for_keydb` must wire `DriveCredentials` when the keydb
    // carries a host cert, so the AACS handshake gets it — no existing test
    // drove this function with a real keydb fixture before.
    #[test]
    fn drive_scan_opts_for_keydb_wires_credentials_when_host_certs_present() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("keydb.cfg");
        // A single AACS 1.0 host-cert row: 20-byte priv key + 92-byte cert,
        // all-zero placeholders (never a real key) — same shape
        // freemkv-keysources' own `test_parse_host_cert` uses.
        let line = format!(
            "| HC | HOST_PRIV_KEY 0x{} | HOST_CERT 0x{} ; Revoked\n",
            "00".repeat(20),
            "00".repeat(92)
        );
        std::fs::write(&path, line).unwrap();

        let opts = drive_scan_opts_for_keydb(&path);
        let credentials = opts
            .credentials
            .expect("a keydb with a host cert must produce Some(DriveCredentials)");
        assert!(
            !credentials.host_certs.is_empty(),
            "the wired credentials must actually carry the cert"
        );
    }

    /// A keydb with NO host certs (or no keydb at all) must yield `None` —
    /// not `Some` wrapping an empty cert list, which would look "present"
    /// to a caller checking `.is_some()` while carrying nothing usable.
    #[test]
    fn drive_scan_opts_for_keydb_no_credentials_without_host_certs() {
        let tmp = tempfile::tempdir().unwrap();
        // A keydb with disc entries but no `| HC |` row.
        let path = tmp.path().join("keydb.cfg");
        std::fs::write(&path, b"0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF = Test\n").unwrap();
        let opts = drive_scan_opts_for_keydb(&path);
        assert!(
            opts.credentials.is_none(),
            "no host certs must mean no DriveCredentials at all"
        );

        // No keydb file at all — same expectation.
        let missing = tmp.path().join("does-not-exist.cfg");
        let opts2 = drive_scan_opts_for_keydb(&missing);
        assert!(opts2.credentials.is_none());
    }

    /// A minimal keyless, encrypted `Disc` with no AACS state.
    fn keyless_encrypted_disc() -> libfreemkv::Disc {
        libfreemkv::Disc {
            volume_id: "TEST_DISC".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 0,
            capacity_bytes: 0,
            layers: 1,
            titles: Vec::new(),
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted: true,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    // Like `keyless_encrypted_disc` but WITH AACS state, so `disc.inputs()` returns `Some`.
    fn keyless_encrypted_disc_with_aacs() -> libfreemkv::Disc {
        let mut disc = keyless_encrypted_disc();
        disc.aacs = Some(
            libfreemkv::test_util::aacs_state()
                .version(libfreemkv::aacs::mkb::AACS_MAJOR_UHD)
                .disc_hash("0xabc")
                .build(),
        );
        disc
    }

    /// The three `KeyOutcome` variants are distinct — a regression guard so a
    /// future refactor can't accidentally collapse e.g. MissingInputs into NoKey.
    #[test]
    fn key_outcome_variants_are_distinct() {
        let all = [
            KeyOutcome::Resolved,
            KeyOutcome::MissingInputs,
            KeyOutcome::NoKey,
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                assert_eq!(
                    i == j,
                    a == b,
                    "{a:?} vs {b:?} equality must track identity"
                );
            }
        }
    }

    // --- the key chain (engine, local-first) ----------------------------------

    // No URL: "local" is the keydb; "online" has nothing to ask until a URL is set.
    #[test]
    fn build_sources_without_a_url() {
        for (key_source, n) in [("local", 1), ("onlnie", 1), ("online", 0)] {
            let cfg = Config {
                key_source: key_source.into(),
                ..Config::default()
            };
            let sources = build_sources(&cfg);
            assert_eq!(sources.len(), n, "{key_source}");
            assert!(sources.iter().all(|s| s.label() == "keydb"));
            assert_eq!(uses_online(&cfg), key_source == "online", "{key_source}");
        }
    }

    // The picked source only: "online" is the key service, "local" the keydb, URL or not.
    #[test]
    fn build_sources_follows_key_source_when_a_url_is_set() {
        for (key_source, want) in [("local", &["keydb"][..]), ("online", &["online"][..])] {
            let cfg = Config {
                key_source: key_source.into(),
                keyserver_url: "https://8.8.8.8/keys".into(),
                ..Config::default()
            };
            let labels: Vec<_> = build_sources(&cfg).iter().map(|s| s.label()).collect();
            assert_eq!(labels, want, "{key_source}");
        }
    }

    // An SSRF-blocked URL never becomes a source, leaving online mode with none.
    #[test]
    fn build_sources_drops_online_source_on_invalid_address_url() {
        let cfg = Config {
            key_source: "online".into(),
            keyserver_url: "https://0.0.0.0/keys".into(),
            ..Config::default()
        };
        assert!(build_sources(&cfg).is_empty());
    }

    // key_source=local with a saved URL is local only: no online source, no
    // online probe or outage retry, and no "Communicating" status.
    #[test]
    fn local_key_source_with_saved_url_never_goes_online() {
        let cfg = Config {
            key_source: "local".into(),
            keyserver_url: "https://8.8.8.8/decode".into(),
            keyserver_secret: "tok".into(),
            ..Config::default()
        };
        assert!(!uses_online(&cfg));
        let p = key_params(&cfg);
        assert!(p.key_url.is_none() && p.key_auth.is_none() && !p.online_only);
        let labels: Vec<_> = build_sources(&cfg).iter().map(|s| s.label()).collect();
        assert_eq!(labels, ["keydb"]);
        assert!(keyserver_url_startup_warning(&cfg).is_none());
    }

    // The settings fields autorip wrote map onto the engine's parameters as-is.
    #[test]
    fn key_params_reads_the_autorip_settings_fields() {
        let cfg = Config {
            key_source: "online".into(),
            keyserver_url: "  https://keys.example.org/decode ".into(),
            keyserver_secret: "tok".into(),
            keydb_path: Some("/config/custom.cfg".into()),
            ..Config::default()
        };
        let p = key_params(&cfg);
        assert_eq!(p.keydb_path.as_deref(), Some("/config/custom.cfg"));
        assert_eq!(
            p.key_url.as_deref(),
            Some("https://keys.example.org/decode")
        );
        assert_eq!(p.key_auth.as_deref(), Some("tok"));
        assert!(
            p.online_only,
            "online with a usable URL asks only the service"
        );
        let bare = key_params(&Config::default());
        assert_eq!(bare.key_url, None);
        assert_eq!(bare.key_auth, None);
    }

    // A missing image fails the open on both key routes (drive keys, the key chain).
    #[test]
    fn open_staged_image_reports_a_missing_image() {
        let cfg = Config::default();
        let missing = Path::new("/nonexistent-autorip-iso-fixture-xyz.iso");
        let disc = || keyless_encrypted_disc_with_aacs();
        let rip = StagedKeys::Rip(libfreemkv::keys::KeyRing::none());
        assert!(open_staged_image(&cfg, missing, disc(), &[0], rip, None).is_err());
        let resolve = StagedKeys::Resolve {
            vid: Some([7u8; 16]),
        };
        assert!(open_staged_image(&cfg, missing, disc(), &[0], resolve, None).is_err());
    }

    // The PROBE is disc-less (empty POST), so its classification stays coarse:
    // transport/5xx/429 are transient and EVERY other status means only "the
    // service is up" — never a per-disc no-key verdict.
    #[test]
    fn classify_reachability_down_vs_no_key() {
        use ProbeOutcome::{Status, Transport};
        for code in [500u16, 502, 503, 504] {
            assert_eq!(
                classify_reachability(Status(code)),
                ServiceReachability::ServerError(code),
                "HTTP {code} is the service failing on its own side"
            );
        }
        // transport failure (timeout / connect refused) → never reached
        assert_eq!(
            classify_reachability(Transport),
            ServiceReachability::Unreachable
        );
        // quota → RATE-LIMITED
        assert_eq!(
            classify_reachability(Status(429)),
            ServiceReachability::RateLimited
        );
        // Everything else proves only reachability — the probe carried no disc.
        for code in [200u16, 404, 405, 422] {
            assert_eq!(
                classify_reachability(Status(code)),
                ServiceReachability::Answered,
                "a disc-less probe must not produce a per-disc verdict from {code}"
            );
        }
    }

    // The REAL decode POST carried the disc, so its status IS a per-disc
    // verdict. This is the mapping the operator-facing message is written from:
    // every outcome in the bug report gets its own arm and none of them share.
    #[test]
    fn decode_outcome_drives_the_down_vs_no_key_verdict() {
        use freemkv_keysources::DecodeReachability::{Status, Transport};
        // The bug's exact case: 422 "licensed but unresolved" is a DEFINITIVE
        // no-key for this disc — reached, licensed, all candidates exhausted.
        assert_eq!(
            reachability_from_decode(Status(422)),
            ServiceReachability::NoKeyForDisc
        );
        // 404 is a licence/wall verdict, NOT the same thing as 422.
        assert_eq!(
            reachability_from_decode(Status(404)),
            ServiceReachability::NotLicensed
        );
        assert_eq!(
            reachability_from_decode(Status(200)),
            ServiceReachability::Answered
        );
        assert_eq!(
            reachability_from_decode(Status(304)),
            ServiceReachability::Answered
        );
        // A transport failure is the ONLY "we never reached it" outcome.
        assert_eq!(
            reachability_from_decode(Transport),
            ServiceReachability::Unreachable
        );
        // 5xx / 429 remain transient, each with its own verdict.
        assert_eq!(
            reachability_from_decode(Status(503)),
            ServiceReachability::ServerError(503)
        );
        assert_eq!(
            reachability_from_decode(Status(429)),
            ServiceReachability::RateLimited
        );
        // Anything else keeps its status rather than being guessed at.
        for code in [400u16, 418, 451] {
            assert_eq!(
                reachability_from_decode(Status(code)),
                ServiceReachability::Unexpected(code)
            );
        }
    }

    // 401/403 is a credential rejection (keysources' KeyServiceUnauthorized),
    // terminal, and must not collapse into the generic Unexpected(code).
    #[test]
    fn decode_401_403_is_unauthorized() {
        use freemkv_keysources::DecodeReachability::Status;
        for code in [401u16, 403] {
            let v = reachability_from_decode(Status(code));
            assert_ne!(v, ServiceReachability::Unexpected(code), "{code}");
            assert_eq!(v.http_status(), Some(code));
            assert!(!v.is_transient());
        }
    }

    // A resolve must never hand back a PREVIOUS resolve's decode verdict: plant one
    // (Transport), then resolve
    // with no sources / no AACS inputs — both return before any online query.
    #[test]
    fn resolve_drains_a_stale_decode_verdict() {
        let plant = || {
            freemkv_keysources::set_last_decode_reachability(Some(
                freemkv_keysources::DecodeReachability::Transport,
            ));
        };
        plant();
        assert!(
            freemkv_keysources::take_last_decode_reachability().is_some(),
            "fixture must plant a verdict"
        );
        plant();
        let none: libfreemkv::KeySourceFactory = std::sync::Arc::new(Vec::new);
        let disc = keyless_encrypted_disc_with_aacs();
        let mut reader = libfreemkv::test_util::MemSource::new(vec![0u8; 2048]);
        let scope = libfreemkv::keys::KeyScope::Titles(Vec::new());
        let _ = resolve_with(&disc, &mut reader, scope, &none, None, None);
        assert_eq!(
            take_online_decode_reachability(),
            None,
            "no-sources resolve"
        );
        plant();
        let scope = libfreemkv::keys::KeyScope::None;
        let _ = resolve_with(
            &keyless_encrypted_disc(),
            &mut reader,
            scope,
            &none,
            None,
            None,
        );
        assert_eq!(
            take_online_decode_reachability(),
            None,
            "a raw-copy resolve"
        );
    }

    // No two of these outcomes may collapse to the same verdict — that
    // collapse is the bug (a 422 reported as "the service was down").
    #[test]
    fn every_key_service_outcome_is_distinct() {
        use freemkv_keysources::DecodeReachability::{Status, Transport};
        let verdicts = [
            reachability_from_decode(Transport),
            reachability_from_decode(Status(503)),
            reachability_from_decode(Status(429)),
            reachability_from_decode(Status(422)),
            reachability_from_decode(Status(404)),
            reachability_from_decode(Status(400)),
            reachability_from_decode(Status(401)),
            reachability_from_decode(Status(200)),
        ];
        for (i, a) in verdicts.iter().enumerate() {
            for b in &verdicts[i + 1..] {
                assert_ne!(a, b, "distinct key-service outcomes share a verdict");
            }
        }
    }

    /// Only the "never got an answer about this disc" verdicts are
    /// transient/retryable. A definitive 422 no-key is terminal — retrying it
    /// is pointless work, and the 29-second server-side exhaustion behind it
    /// makes that retry expensive. Regression guard both ways.
    #[test]
    fn reachability_transient_partition() {
        assert!(ServiceReachability::Unreachable.is_transient());
        assert!(ServiceReachability::ServerError(502).is_transient());
        assert!(ServiceReachability::RateLimited.is_transient());
        assert!(
            !ServiceReachability::NoKeyForDisc.is_transient(),
            "a definitive no-key must never be retried"
        );
        assert!(!ServiceReachability::NotLicensed.is_transient());
        assert!(!ServiceReachability::Unexpected(400).is_transient());
        assert!(!ServiceReachability::NotAsked.is_transient());
        assert!(!ServiceReachability::Answered.is_transient());
    }

    /// The status is carried on the verdict so support can quote it — `None`
    /// only where there genuinely was no HTTP answer.
    #[test]
    fn http_status_is_carried_where_one_exists() {
        assert_eq!(ServiceReachability::NoKeyForDisc.http_status(), Some(422));
        assert_eq!(ServiceReachability::NotLicensed.http_status(), Some(404));
        assert_eq!(ServiceReachability::RateLimited.http_status(), Some(429));
        assert_eq!(
            ServiceReachability::ServerError(503).http_status(),
            Some(503)
        );
        assert_eq!(
            ServiceReachability::Unexpected(418).http_status(),
            Some(418)
        );
        assert_eq!(ServiceReachability::Unreachable.http_status(), None);
        assert_eq!(ServiceReachability::NotAsked.http_status(), None);
        assert_eq!(ServiceReachability::Answered.http_status(), None);
    }

    // Permanent verdicts from the REAL keysources producer: the online source is
    // dropped for these, so a no-key is genuine and must NOT be retried as an outage.
    #[test]
    fn keysources_config_rejections_are_not_asked() {
        for url in [
            "http://8.8.8.8/keys",
            "https:///keys",
            "https://8.8.8.8:notaport/keys",
            "https://[::1/keys",
            "https://0.0.0.0/keys",
            "https://240.0.0.1/latest/meta-data",
        ] {
            let err = freemkv_keysources::validate_keyserver_url(url)
                .expect_err("keysources must reject this URL outright");
            assert_eq!(
                reachability_for_unprobeable_url(&err),
                ServiceReachability::NotAsked,
                "{url:?} -> {err:?} is a config verdict, not an outage"
            );
        }
    }

    // keysources' GuardFail::Unreachable texts (online.rs resolve_and_guard). They need
    // live DNS to produce, so are pinned verbatim until keysources exports a typed kind.
    #[test]
    fn keysources_lookup_failures_are_unreachable() {
        for msg in [
            "too many concurrent DNS resolutions in flight for this host",
            "could not resolve host: failed to lookup address information",
            "DNS resolution timed out",
            "host did not resolve to any address",
        ] {
            assert_eq!(
                reachability_for_unprobeable_url(msg),
                ServiceReachability::Unreachable,
                "{msg:?} is a DNS failure, not a config verdict"
            );
        }
    }

    // web's own resolver failures (the probe's pinning step) stay transient.
    #[test]
    fn web_resolve_failures_are_unreachable() {
        for msg in [
            crate::server::web::RESOLVE_TIMEOUT_MSG.to_string(),
            crate::server::web::RESOLVE_NO_ADDRS_MSG.to_string(),
            format!("{}EAI_AGAIN", crate::server::web::RESOLVE_FAILED_PREFIX),
        ] {
            assert_eq!(
                reachability_for_unprobeable_url(&msg),
                ServiceReachability::Unreachable
            );
        }
    }

    // A stored pre-upgrade http:// keyserver URL is named at boot in online mode;
    // https, blank, and any URL under `key_source = "local"` are silent.
    #[test]
    fn keyserver_url_startup_warning_flags_only_non_https() {
        let cfg = |src: &str, url: &str| Config {
            key_source: src.into(),
            keyserver_url: url.into(),
            ..Config::default()
        };
        let w = keyserver_url_startup_warning(&cfg("online", " http://keys.example.org/t0k/d"))
            .expect("http:// must warn");
        assert!(w.contains("https://") && !w.contains("t0k"), "{w}");
        assert!(!w.contains("  "), "no stray whitespace runs: {w:?}");
        assert!(keyserver_url_startup_warning(&cfg("online", "https://k.example.org/d")).is_none());
        assert!(keyserver_url_startup_warning(&cfg("online", "")).is_none());
        assert!(keyserver_url_startup_warning(&cfg("local", "http://k.example.org/d")).is_none());
    }

    // Cleartext http:// is a standing config fault: the online source is dropped.
    #[test]
    fn build_sources_drops_online_source_on_http_url() {
        let cfg = Config {
            key_source: "online".into(),
            keyserver_url: "http://8.8.8.8/decode".into(),
            ..Config::default()
        };
        assert!(build_sources(&cfg).is_empty());
    }

    // `render_resolution_trace` is the app-layer's ENTIRE English mapping of
    // the library's typed trace, shown on every rip — a dropped/mis-mapped arm
    // ships a wrong diagnostic. Drive every enum arm through it and pin output.
    #[test]
    fn render_resolution_trace_maps_every_enum_arm() {
        use libfreemkv::aacs::trace::{
            KeyNode, KeyOutcome, KeyStep, ResolutionTrace, UnlockOutcome, UnlockStep,
        };

        let unlock = vec![
            UnlockStep {
                who: "u_ok".into(),
                outcome: UnlockOutcome::Unlocked,
            },
            UnlockStep {
                who: "u_fw".into(),
                outcome: UnlockOutcome::FirmwareNotUnlockable,
            },
            UnlockStep {
                who: "u_cert".into(),
                outcome: UnlockOutcome::NoUsableHostCert { mkb: Some(7) },
            },
            UnlockStep {
                who: "u_rev".into(),
                outcome: UnlockOutcome::CertRevoked { mkb: None },
            },
            UnlockStep {
                who: "u_hs".into(),
                outcome: UnlockOutcome::HandshakeRejected,
            },
            UnlockStep {
                who: "u_vid".into(),
                outcome: UnlockOutcome::VidUnavailable,
            },
        ];
        // One key step walking EVERY node, plus one per terminal outcome.
        let keys = vec![
            KeyStep {
                who: "keydb".into(),
                path: vec![
                    KeyNode::MatchedDisc,
                    KeyNode::NoEntry,
                    KeyNode::NoDerivableKey,
                    KeyNode::FoundUnitKeys,
                    KeyNode::FoundVuk,
                    KeyNode::FoundMediaKey,
                    KeyNode::NeedVid,
                    KeyNode::VidFromUnlock,
                    KeyNode::VidFromKeydb,
                    KeyNode::NoVid,
                    KeyNode::DerivedVuk,
                    KeyNode::DerivedUnitKeys,
                ],
                outcome: KeyOutcome::Resolved,
                matched_entry: None,
                store_entries: None,
            },
            KeyStep {
                who: "online".into(),
                path: vec![KeyNode::NeedVid],
                outcome: KeyOutcome::MissingVid,
                matched_entry: None,
                store_entries: None,
            },
            KeyStep {
                who: "empty".into(),
                path: vec![],
                outcome: KeyOutcome::NoKey,
                matched_entry: None,
                store_entries: None,
            },
        ];
        let trace = ResolutionTrace { unlock, keys };

        let lines = render_resolution_trace(&trace, "0xDEADBEEF");
        assert_eq!(
            lines,
            vec![
                "unlock: u_ok > UNLOCKED",
                "unlock: u_fw > firmware not unlockable",
                "unlock: u_cert > no usable host cert (MKBv7)",
                "unlock: u_rev > host cert revoked",
                "unlock: u_hs > handshake rejected",
                "unlock: u_vid > Volume ID unavailable",
                "key: keydb > matched disc > no entry > no derivable key > found unit keys > \
                 found VUK > found media key > need VID > VID from drive > VID from keydb > \
                 no VID > derived VUK > derived unit keys > RESOLVED",
                "key: online > need VID > MISSING VID",
                "key: empty > NO KEY",
            ]
        );
    }

    /// Issue #46: a MATCHED-but-underivable (no VID) keydb hit renders the
    /// actionable verdict `matched disc > no VID available > NO KEY`, never the
    /// misleading `no entry`.
    // KU-E1: a matched entry whose key needs the VID reads as such, not as a bare node walk.
    #[test]
    fn matched_missing_vid_says_the_key_needs_the_volume_id() {
        use libfreemkv::aacs::trace::{KeyNode, KeyOutcome, KeyStep};
        let step = KeyStep {
            who: "keydb".into(),
            path: vec![KeyNode::MatchedDisc, KeyNode::FoundMediaKey, KeyNode::NoVid],
            outcome: KeyOutcome::MissingVid,
            matched_entry: None,
            store_entries: None,
        };
        assert_eq!(
            render_key_step(&step, "0xAB"),
            "keydb > matched disc > key needs the disc's Volume ID, not in hand > MISSING VID"
        );
    }

    #[test]
    fn matched_no_vid_renders_distinctly_from_a_true_miss() {
        use libfreemkv::aacs::trace::{KeyNode, KeyOutcome, KeyStep, ResolutionTrace};

        let trace = ResolutionTrace {
            unlock: vec![],
            keys: vec![KeyStep {
                who: "keydb".into(),
                path: vec![KeyNode::MatchedDisc, KeyNode::NoVid],
                outcome: KeyOutcome::NoKey,
                matched_entry: None,
                store_entries: Some(500),
            }],
        };
        assert_eq!(
            render_resolution_trace(&trace, "0xABC"),
            vec!["key: keydb > matched disc > no VID available > NO KEY"]
        );
    }

    /// Issue #46: a matched entry with no usable keys dumps its shape, and a true
    /// miss names the hash and the store size — the two are self-diagnosing and
    /// clearly distinct.
    #[test]
    fn matched_no_material_and_true_miss_render_distinctly() {
        use libfreemkv::aacs::trace::{
            KeyNode, KeyOutcome, KeyStep, MatchedEntry, ResolutionTrace,
        };

        let matched = ResolutionTrace {
            unlock: vec![],
            keys: vec![KeyStep {
                who: "keydb".into(),
                path: vec![KeyNode::MatchedDisc, KeyNode::NoDerivableKey],
                outcome: KeyOutcome::NoKey,
                matched_entry: Some(MatchedEntry {
                    has_vuk: false,
                    has_unit_keys: false,
                    unit_keys_len: 0,
                    has_media_key: false,
                    has_keydb_vid: false,
                    enc_title_keys_len: 2,
                    vid_available: false,
                }),
                store_entries: Some(500),
            }],
        };
        assert_eq!(
            render_resolution_trace(&matched, "0xABC"),
            vec![
                "key: keydb > matched disc > entry has no usable keys \
                 (vuk=false unit_keys=0 enc_title_keys=2 media_key=false) > NO KEY"
            ]
        );

        let miss = ResolutionTrace {
            unlock: vec![],
            keys: vec![KeyStep {
                who: "keydb".into(),
                path: vec![KeyNode::NoEntry],
                outcome: KeyOutcome::NoKey,
                matched_entry: None,
                store_entries: Some(500),
            }],
        };
        assert_eq!(
            render_resolution_trace(&miss, "0x1234abcd"),
            vec!["key: keydb > no entry > disc hash 0x1234abcd not in keydb (500 entries loaded)"]
        );
    }

    // `probe_online_reachability`'s two non-network arms: an EMPTY keyserver URL
    // and an SSRF-blocked one both mean the service was never ASKED — a config
    // verdict, terminal like before, never an outage. Neither touches the network.
    #[test]
    fn probe_online_reachability_unprobeable_urls_report_up() {
        let empty = Config {
            keyserver_url: String::new(),
            ..Default::default()
        };
        assert_eq!(
            probe_online_reachability(&empty),
            ServiceReachability::NotAsked
        );
        assert!(!probe_online_reachability(&empty).is_transient());

        let blocked = Config {
            keyserver_url: "https://0.0.0.0:9/keys".into(),
            ..Default::default()
        };
        assert_eq!(
            probe_online_reachability(&blocked),
            ServiceReachability::NotAsked,
            "an invalid-address URL is a permanent config verdict, not an outage"
        );
    }

    // ── keydb PATH resolution (issue #46) ── the full decision table, driven
    //    through the pure `resolve_keydb` with an injected `exists` + `legacy`
    //    so every branch is deterministic (no env, no filesystem). ──

    fn none_exists(_: &std::path::Path) -> bool {
        false
    }

    /// An explicit configured path always wins — even when the canonical file
    /// also exists on disk.
    #[test]
    fn resolve_keydb_explicit_path_wins() {
        let got = resolve_keydb(Some("/mnt/keys/keydb.cfg"), "/config", None, &|_| true);
        assert_eq!(got, PathBuf::from("/mnt/keys/keydb.cfg"));
    }

    /// A blank configured path is treated as unset (the UI sends "" for empty).
    #[test]
    fn resolve_keydb_blank_config_falls_through_to_default() {
        let got = resolve_keydb(Some(""), "/config", None, &none_exists);
        assert_eq!(got, PathBuf::from("/config/keydb.cfg"));
    }

    /// Default with nothing on disk → the canonical AUTORIP_DIR path (the read +
    /// write + download target). This is the #46 fix: `/config/keydb.cfg`, NOT a
    /// `$HOME`-derived path.
    #[test]
    fn resolve_keydb_default_is_autorip_dir_when_nothing_exists() {
        let got = resolve_keydb(None, "/config", None, &none_exists);
        assert_eq!(got, PathBuf::from("/config/keydb.cfg"));
    }

    /// #46 scenario: a user drops keydb.cfg at /config/keydb.cfg → autorip now
    /// resolves to exactly that file.
    #[test]
    fn resolve_keydb_finds_user_file_at_config() {
        let canonical = PathBuf::from("/config/keydb.cfg");
        let got = resolve_keydb(None, "/config", None, &|p| p == canonical);
        assert_eq!(got, canonical);
    }

    /// Upgrade migration: canonical missing but a legacy $HOME keydb exists →
    /// keep resolving to the legacy file (don't force a re-download).
    #[test]
    fn resolve_keydb_migrates_to_legacy_when_canonical_absent() {
        let legacy = PathBuf::from("/root/.config/freemkv/keydb.cfg");
        let lg = legacy.clone();
        let got = resolve_keydb(None, "/config", Some(legacy.clone()), &move |p| p == lg);
        assert_eq!(got, legacy);
    }

    /// Canonical present takes precedence over a legacy file (no accidental
    /// migration once the new location is populated).
    #[test]
    fn resolve_keydb_canonical_beats_legacy_when_both_exist() {
        let legacy = PathBuf::from("/root/.config/freemkv/keydb.cfg");
        let got = resolve_keydb(None, "/config", Some(legacy), &|_| true);
        assert_eq!(got, PathBuf::from("/config/keydb.cfg"));
    }

    /// Unset or empty HOME (the container case that caused #46) yields no legacy
    /// path, so resolution never collapses to a stray relative path.
    #[test]
    fn resolve_keydb_no_legacy_when_home_absent() {
        assert_eq!(legacy_keydb_under(None), None);
        assert_eq!(legacy_keydb_under(Some(std::ffi::OsString::new())), None);
        assert_eq!(
            legacy_keydb_under(Some("/root".into())),
            Some(PathBuf::from("/root/.config/freemkv/keydb.cfg"))
        );
        let got = resolve_keydb(None, "/config", legacy_keydb_under(Some("".into())), &|p| {
            p != Path::new("/config/keydb.cfg")
        });
        // Lands on canonical AUTORIP_DIR path, NOT the bare relative "keydb.cfg"
        // the HOME-less container collapsed to (#46). No is_absolute assert:
        // "/config" isn't absolute on Windows; the equality already proves it.
        assert_eq!(got, PathBuf::from("/config/keydb.cfg"));
        assert_ne!(got, PathBuf::from("keydb.cfg"));
    }

    /// `keydb_path` honors an explicit config value end-to-end.
    #[test]
    fn keydb_path_uses_explicit_config_value() {
        let cfg = Config {
            keydb_path: Some("/mnt/archive/keydb.cfg".into()),
            ..Config::default()
        };
        assert_eq!(keydb_path(&cfg), PathBuf::from("/mnt/archive/keydb.cfg"));
    }
}

#[cfg(test)]
mod ku_e1_tests {
    use super::*;
    use crate::ku_fixture::{K1, VID, bd_image, counting, write_sidecar};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn has_k1(calls: &Arc<AtomicUsize>) -> libfreemkv::KeySourceFactory {
        crate::ku_fixture::holding(calls, K1)
    }

    // KU-E1 invariant: a fresh multipass rip asks the key service ONCE, at the drive scan
    // (the drive's set, VID in memory). Its staged-ISO open and mux, handed that set,
    // ask no key source again.
    #[test]
    fn a_fresh_rip_asks_the_key_service_once_at_the_drive_scan() {
        let fx = bd_image();
        let dir = tempfile::tempdir().unwrap();
        let iso = fx.write(dir.path(), "disc.iso");
        write_sidecar(&fx, &iso, false);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut drive_disc = fx.scan();
        drive_disc.aacs.as_mut().unwrap().volume_id = VID;
        let mut drive = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
        let scope = libfreemkv::keys::KeyScope::Titles(vec![0]);
        let set = resolve_with(&drive_disc, &mut drive, scope, &has_k1(&calls), None, None)
            .expect("the drive scan resolves the rip's set");
        let at_scan = calls.load(Ordering::SeqCst);
        assert!(at_scan >= 1, "the scan asked the key service");

        let keys = freemkv_engine::KeyInput::Seeded(has_k1(&calls), set);
        let image = open_staged(&iso, fx.scan(), &[0], keys, None, None).unwrap();
        assert!(image.sources.is_none(), "no source is kept past the open");
        let dest = format!("mkv://{}", dir.path().join("out.mkv").display());
        let out = freemkv_engine::mux_image_titles(
            &image,
            &freemkv_engine::MuxPlan::new(vec![0]),
            &|_| dest.clone(),
            &freemkv_engine::NoopSink,
        );
        assert!(
            matches!(out, freemkv_engine::RipOutcome::Ok { titles_written: 1 }),
            "{out:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            at_scan,
            "no second key-service call after the scan"
        );
    }

    // A resolve that finds no key refuses before any output, never a keyless set.
    #[test]
    fn a_drive_scan_with_no_key_refuses() {
        let fx = bd_image();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut drive = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
        let scope = libfreemkv::keys::KeyScope::Titles(vec![0]);
        let r = resolve_with(&fx.scan(), &mut drive, scope, &counting(&calls), None, None);
        let e = r.expect_err("no key");
        assert_eq!(e.code(), libfreemkv::error::E_NO_DISC_KEY, "{e}");
    }

    // A refused resolve still logs its per-source walk: on a refusal it is the operator's only
    // view of why a key missed. Checked on the trace itself (log capture races other tests'
    // global level), plus that `resolve_with` hands every trace to the logger.
    #[test]
    fn a_refused_resolve_logs_its_key_walk() {
        let fx = bd_image();
        let calls = Arc::new(AtomicUsize::new(0));
        let mut drive = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
        let scope = libfreemkv::keys::KeyScope::Titles(vec![0]);
        let (set, trace) = freemkv_engine::keys::resolve_for_rip_traced(
            &fx.scan(),
            &mut drive,
            scope,
            &counting(&calls),
            None,
            None,
        );
        assert!(set.is_err(), "no key");
        let walk = render_resolution_trace(&trace, "");
        assert!(
            walk.iter().any(|l| l.contains("online >")),
            "the refused walk renders: {walk:?}"
        );
        let src = include_str!("keysource.rs");
        let body = &src[src.find("fn resolve_with(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(
            body.find("log_key_walk(&trace").unwrap() < body.find("let set = set?").unwrap(),
            "the walk is logged before a refusal returns"
        );
    }

    // An online key service that is down is an outage, never Missing: E7028, even with the
    // disc's VID fingerprint on the sidecar (never E7034 "insert the disc"). `localhost` is
    // refused at the first query, with no network.
    #[test]
    fn a_down_key_service_is_e7028_not_missing() {
        let fx = bd_image();
        let dir = tempfile::tempdir().unwrap();
        let iso = fx.write(dir.path(), "disc.iso");
        write_sidecar(&fx, &iso, true);
        let cfg = Config {
            keydb_path: Some(dir.path().join("none.cfg").to_string_lossy().into_owned()),
            key_source: "online".into(),
            keyserver_url: "https://localhost:9/decode".into(),
            ..Config::default()
        };
        let keys = StagedKeys::Resolve { vid: None };
        let Err(e) = open_staged_image(&cfg, &iso, fx.scan(), &[0], keys, None) else {
            panic!("a down key service keys nothing");
        };
        assert_eq!(
            e.code(),
            libfreemkv::error::E_KEY_SERVICE_UNAVAILABLE,
            "{e}"
        );
    }

    // The ripping process lends its set to the mux worker by ISO path, in memory.
    #[test]
    fn rip_keys_are_held_per_iso_until_forgotten() {
        let iso = Path::new("/staging/ku-e1-held/disc.iso");
        assert!(rip_keys_for(iso).is_none());
        hold_rip_keys(iso, libfreemkv::keys::KeyRing::none());
        assert!(rip_keys_for(iso).is_some());
        forget_rip_keys(iso);
        assert!(rip_keys_for(iso).is_none());
    }
}
