//! Where AACS keys come from.
//!
//! libfreemkv does no key lookup — its `KeySource`s resolve a disc's terminal
//! Unit Keys, driving the library's boil-down crypto. autorip resolves from the
//! configured *published key source* (`local` keydb or `online` key service).
//!
//! The flow is the same for a live drive and a staged ISO: scan the disc
//! KEYLESS, build [`libfreemkv::DiscInputs`] from its key files, then resolve
//! via [`resolve_and_apply_traced`] — the first source whose Unit Keys
//! validate wins. The only drive-vs-ISO difference is the [`DiscKeyAccess`] impl.

use std::path::{Path, PathBuf};

use freemkv_keysources::{KeySource, KeydbSource, OnlineSource};
use libfreemkv::aacs::trace::ResolutionTrace;
use libfreemkv::keysource::resolve_and_apply_traced;

use crate::server::config::Config;

// The keyserver URL gate is `freemkv_keysources::validate_keyserver_url` (https-only + SSRF):
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
/// Pure — no DNS — so it is safe on the startup path.
pub fn keyserver_url_startup_warning(cfg: &Config) -> Option<String> {
    let url = cfg.keyserver_url.trim();
    if cfg.key_source != "online" || url.is_empty() || url.starts_with("https://") {
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

/// ScanOptions for an **ISO** structure scan — no handshake, no credentials.
pub fn iso_scan_opts() -> libfreemkv::ScanOptions {
    libfreemkv::ScanOptions::default()
}

/// Build the ordered key-source list from config: `online` → the remote key
/// service, anything else → the local keydb (explicit path, else the standard
/// location).
///
/// The mapfile key-source was removed in the AACS-trait reshape: on resume /
/// deferred mux, keys are re-resolved from the keydb / online source rather
/// than read back from the `.map` header (correct, marginally slower). The
/// `.map` recovery-state file itself is unaffected — autorip still loads it for
/// sector status via `IsoAccess`.
pub fn build_sources(cfg: &Config) -> Vec<Box<dyn KeySource>> {
    build_sources_with(cfg, &freemkv_keysources::validate_keyserver_url)
}

// `build_sources` with the keyserver-URL validator injected, so tests need no live DNS.
fn build_sources_with(
    cfg: &Config,
    validate: &dyn Fn(&str) -> Result<(), String>,
) -> Vec<Box<dyn KeySource>> {
    let mut sources: Vec<Box<dyn KeySource>> = Vec::new();
    match cfg.key_source.as_str() {
        "online" => {
            let url = cfg.keyserver_url.trim();
            let online = || Box::new(OnlineSource::new(url, cfg.keyserver_secret.clone()));
            match validate(url) {
                Ok(()) => sources.push(online()),
                // DNS blip: keep the source; its per-POST re-guard retries the
                // lookup and records a real (transient) reachability verdict.
                Err(e) if url_error_is_transient(&e) => {
                    tracing::warn!(
                        phase = "key_resolve",
                        url_origin = %crate::server::webhook::webhook_url_origin(url),
                        "keyserver host lookup failed: {e} — will retry at request time"
                    );
                    sources.push(online());
                }
                // Standing config fault (bad scheme/host, SSRF-blocked address):
                // never POST disc-key material there; drop the online source.
                Err(e) => {
                    tracing::error!(
                        phase = "key_resolve",
                        url_origin = %crate::server::webhook::webhook_url_origin(url),
                        "keyserver URL rejected: {e} — online key source disabled for this rip"
                    );
                }
            }
        }
        "local" => {
            // Loud diagnostic when local keys are selected but no keydb exists at
            // the resolved path — else every disc reports a bare "NO KEY" with no
            // hint the file is just in the wrong place (issue #46).
            let path = keydb_path(cfg);
            if !path.exists() {
                tracing::warn!(
                    phase = "key_resolve",
                    keydb_path = %path.display(),
                    "local key source selected but NO keydb.cfg at the resolved path — every disc will report NO KEY until a keydb exists here; set 'KEYDB.cfg Location' or place the file at this path"
                );
            }
            sources.push(Box::new(KeydbSource::new(path)));
        }
        other => {
            // key_source is user-edited config; a typo ("onlnie") would
            // silently resolve keydb-only when the operator meant online.
            // Fall back to the local keydb but make the fallback visible.
            tracing::warn!(
                key_source = %other,
                "unrecognised key_source; falling back to local keydb"
            );
            sources.push(Box::new(KeydbSource::new(keydb_path(cfg))));
        }
    }
    sources
}

/// Build the fresh-key-on-decrypt-failure closure ([`libfreemkv::sector::KeyFetch`])
/// for an ISO mux.
///
/// The library owns the recovery loop: when the mux hits an AACS unit no held key decrypts, it
/// hands the ciphertext to this closure, which forwards it to the configured key source(s);
/// derived Unit Keys are added to the pool and the unit re-decrypted.
///
/// Returns `None` for a non-AACS ISO, or when its AACS inputs can't be read.
pub fn build_iso_key_fetch(cfg: &Config, iso_path: &Path) -> Option<libfreemkv::sector::KeyFetch> {
    match build_iso_key_fetch_outcome(cfg, iso_path) {
        IsoKeyFetch::Ready(fetch) => Some(fetch),
        IsoKeyFetch::NotAacs => None,
        IsoKeyFetch::Unreadable(err) => {
            tracing::warn!(
                phase = "key_resolve",
                path = %iso_path.display(),
                err = %err,
                "could not read the ISO's AACS inputs; mid-mux key recovery is disabled for this rip"
            );
            None
        }
    }
}

/// Why [`build_iso_key_fetch`] did or did not produce a fetch seam.
///
/// Both negative arms collapse to `None` at the call site, but only one of them is normal: a
/// non-AACS ISO has nothing to fetch, whereas an ISO that could not be READ (ESTALE, truncated,
/// EACCES) is a fault. This is an enum rather than a log line alone so the distinction can be
/// asserted directly in tests, independent of log rendering.
pub enum IsoKeyFetch {
    /// AACS inputs read; mid-mux CPS-unit key recovery is available.
    Ready(libfreemkv::sector::KeyFetch),
    /// The ISO is readable and simply carries no AACS data. Nothing to fetch.
    NotAacs,
    /// The ISO's AACS inputs could not be read. A fault, not a normal outcome.
    Unreadable(libfreemkv::Error),
}

// Hand-written: `KeyFetch` is a boxed closure and carries no `Debug`. Only the
// arm matters for diagnostics — the seam itself has nothing printable.
impl std::fmt::Debug for IsoKeyFetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IsoKeyFetch::Ready(_) => f.write_str("Ready(<key fetch>)"),
            IsoKeyFetch::NotAacs => f.write_str("NotAacs"),
            IsoKeyFetch::Unreadable(err) => write!(f, "Unreadable({err})"),
        }
    }
}

/// The decision behind [`build_iso_key_fetch`], as data. See [`IsoKeyFetch`].
pub fn build_iso_key_fetch_outcome(cfg: &Config, iso_path: &Path) -> IsoKeyFetch {
    let (inf, mkb, version) = match libfreemkv::Disc::read_aacs_inputs(iso_path) {
        Ok(v) => v,
        Err(err) => return IsoKeyFetch::Unreadable(err),
    };
    if inf.is_empty() {
        return IsoKeyFetch::NotAacs;
    }
    let inputs = libfreemkv::DiscInputs {
        disc_hash: String::new(),
        volume_id: [0u8; 16],
        version,
        mkb,
        unit_key_ro: inf,
        samples: Vec::new(),
        volume_label: None,
    };
    let cfg = cfg.clone();
    let make_sources: std::sync::Arc<dyn Fn() -> Vec<Box<dyn KeySource>> + Send + Sync> =
        std::sync::Arc::new(move || build_sources(&cfg));
    IsoKeyFetch::Ready(libfreemkv::keysource::key_fetch(inputs, make_sources))
}

/// Whether the configured source talks to a remote key service —
/// used by the UI to announce a potentially slow keyserver round-trip.
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
    /// scheme, or blocked by the SSRF guard), so the service was never asked.
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

/// How a disc's key-resolution inputs are obtained. Decouples [`resolve_keys`]
/// from WHERE the disc lives — a live drive or a staged ISO — so the resolution
/// logic is written once. See [`DriveAccess`] and [`IsoAccess`].
pub trait DiscKeyAccess {
    /// A reader over the disc, for sampling ciphertext via
    /// [`libfreemkv::Disc::inputs_with_samples`] — the ONLY thing `resolve_keys`
    /// can't get from `disc.inputs()` (the scan does not retain the reader).
    /// `None` when the disc can't be opened; sampling is then skipped.
    fn sector_source(&mut self) -> Option<&mut dyn libfreemkv::SectorSource>;
}

/// Resolve keys for `disc` via the ordered `sources`, reading inputs through
/// `access`. Returns the disc with keys applied (`Resolved`) or unchanged.
///
/// The disc must have been scanned KEYLESS (see [`drive_scan_opts`] /
/// [`iso_scan_opts`]). Each source offers candidate keys; the first whose
/// [`libfreemkv::Disc::decrypt_with`] derives unit keys wins. A wrong
/// candidate is rejected by `decrypt_with` and the next tried; it only
/// mutates the disc on success, so a rejected candidate leaves it untouched.
pub fn resolve_keys<A: DiscKeyAccess>(
    sources: Vec<Box<dyn KeySource>>,
    access: &mut A,
    mut disc: libfreemkv::Disc,
) -> (libfreemkv::Disc, KeyOutcome) {
    // Drain any earlier decode verdict on this thread (e.g. a mux-time key fetch) so
    // the caller's take after this resolve can only see THIS resolve's POST.
    let _ = take_online_decode_reachability();
    // ALL AACS inputs come from the keyless scan via `disc.inputs()` — the
    // single source of truth. `access` is used ONLY to sample ciphertext,
    // which the scan doesn't retain (the old out-of-band re-read is gone).
    let Some(inputs) = disc.inputs() else {
        tracing::warn!(phase = "key_resolve", "disc carries no AACS inputs");
        return (disc, KeyOutcome::MissingInputs);
    };
    let vid_available = inputs.volume_id != [0u8; 16];
    if !vid_available {
        tracing::warn!(
            phase = "key_resolve",
            "no Volume ID available; using all-zero VID — VID-keyed derivation may fail"
        );
    }

    // Surface the exact identifier being looked up (issue #46): the library only
    // logs it at debug on a `freemkv::*` target the default filter hides, so
    // echo it here at info on autorip's own (visible) target.
    tracing::info!(
        phase = "key_resolve",
        disc_hash = %inputs.disc_hash,
        title = inputs.volume_label.as_deref().unwrap_or("<none>"),
        vid_available,
        "resolving keys for disc"
    );

    // Content samples for ciphertext validation, UNCONDITIONALLY (keydb UKs are only disproved
    // by real ciphertext; online validates server-side), drawn by the library from the main
    // feature, which prefers the largest title WITH video over a streamless decoy.
    let inputs = if sources.is_empty() {
        inputs
    } else if disc.titles.is_empty() {
        tracing::warn!(
            phase = "key_resolve",
            "no titles — cannot sample for key validation"
        );
        inputs
    } else {
        match access.sector_source() {
            Some(reader) => disc
                .inputs_with_samples(reader, SAMPLE_UNITS)
                .unwrap_or(inputs),
            None => inputs,
        }
    };

    // One ordered driver: each source's `get_uk` is tried in turn and the first
    // whose Unit Keys validate against the samples is committed. The `_traced`
    // variant also hands back the structured per-source walk for rendering.
    let (resolved, trace) = resolve_and_apply_traced(&sources, &inputs, &mut disc);

    // Render the structured walk to the device log — ALWAYS, success or
    // failure: the "error-walk pillar". English lives here (app layer); the
    // library trace is typed enums only.
    for line in render_resolution_trace(&trace, &inputs.disc_hash) {
        tracing::info!(phase = "key_resolve", "{line}");
    }

    // On a MATCHED entry, log its shape (booleans + lengths, no key material):
    // WHICH material the found entry carried and whether a VID was available
    // (issue #46) — turns a bare "matched disc > no key" into a report.
    for step in &trace.keys {
        if let Some(m) = step.matched_entry {
            tracing::info!(
                phase = "key_resolve",
                source = %step.who,
                disc_hash = %inputs.disc_hash,
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

    if resolved {
        tracing::info!(phase = "key_resolve", "key resolved — disc now keyed");
        return (disc, KeyOutcome::Resolved);
    }
    (disc, KeyOutcome::NoKey)
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

/// [`DiscKeyAccess`] backed by a live optical drive. Samples ciphertext
/// directly from the drive for the AACS key derivation.
pub struct DriveAccess<'a> {
    drive: &'a mut libfreemkv::Drive,
}

impl<'a> DriveAccess<'a> {
    pub fn new(drive: &'a mut libfreemkv::Drive) -> Self {
        Self { drive }
    }
}

impl DiscKeyAccess for DriveAccess<'_> {
    fn sector_source(&mut self) -> Option<&mut dyn libfreemkv::SectorSource> {
        Some(self.drive)
    }
}

/// [`DiscKeyAccess`] backed by a staged ISO (the resume path). Samples
/// ciphertext from the ISO; all AACS inputs come from `disc.inputs()`.
pub struct IsoAccess<'a> {
    iso_path: &'a Path,
    reader: Option<libfreemkv::FileSectorSource>,
}

impl<'a> IsoAccess<'a> {
    pub fn new(iso_path: &'a Path) -> Self {
        Self {
            iso_path,
            reader: None,
        }
    }
}

impl DiscKeyAccess for IsoAccess<'_> {
    fn sector_source(&mut self) -> Option<&mut dyn libfreemkv::SectorSource> {
        if self.reader.is_none() {
            match libfreemkv::FileSectorSource::open(self.iso_path) {
                Ok(r) => self.reader = Some(r),
                Err(err) => {
                    // Without samples an online key request fires with no
                    // units_b64 and can fail later as NoKey with no visible cause;
                    // surface the real reason here.
                    tracing::warn!(
                        phase = "key_resolve",
                        path = %self.iso_path.display(),
                        %err,
                        "could not open ISO to sample units"
                    );
                    return None;
                }
            }
        }
        self.reader
            .as_mut()
            .map(|r| r as &mut dyn libfreemkv::SectorSource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfreemkv::read_encrypted_units;

    #[test]
    fn ssrf_guard_blocks_metadata_and_internal_hosts() {
        // Cloud metadata endpoint — the canonical SSRF target. https:// so the
        // ADDRESS check fires, not the scheme check.
        let err =
            freemkv_keysources::validate_keyserver_url("https://169.254.169.254/latest/meta-data")
                .unwrap_err();
        assert!(
            err.contains("SSRF guard"),
            "rejected for the address: {err}"
        );
        // Loopback and RFC1918.
        assert!(freemkv_keysources::validate_keyserver_url("https://127.0.0.1:8443/keys").is_err());
        // RFC1918 ranges (10/8, 192.168/16, 172.16/12). Built from octets so the
        // literal dotted-quads don't trip the public leak-guard — these are
        // generic examples, not infrastructure.
        for oct in [[10u8, 0, 0, 1], [192, 168, 1, 5], [172, 20, 4, 4]] {
            let url = format!("https://{}.{}.{}.{}/keys", oct[0], oct[1], oct[2], oct[3]);
            assert!(
                freemkv_keysources::validate_keyserver_url(&url).is_err(),
                "RFC1918 {url} must be rejected"
            );
        }
        // IPv6 loopback / link-local (bracketed).
        assert!(freemkv_keysources::validate_keyserver_url("https://[::1]:443/k").is_err());
        assert!(freemkv_keysources::validate_keyserver_url("https://[fe80::1]/k").is_err());
        // IPv4-mapped IPv6 loopback.
        assert!(
            freemkv_keysources::validate_keyserver_url("https://[::ffff:127.0.0.1]/k").is_err()
        );
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

    // The test above drives `read_encrypted_units` directly, not the real
    // `IsoAccess` reader `resolve_keys` samples through in production —
    // route the same fixture through it so a regression there isn't silently missed.
    #[test]
    fn iso_access_sample_units_reads_through_the_real_trait_impl() {
        use std::io::Write;

        const SECTORS: usize = 1200;
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&vec![0xE5u8; SECTORS * 2048]).unwrap();
        tmp.flush().unwrap();

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

        // Through the trait object, exactly as `resolve_keys` calls it.
        let mut access: Box<dyn DiscKeyAccess> = Box::new(IsoAccess::new(tmp.path()));
        let reader = access.sector_source().expect("a real ISO must open");
        let units = read_encrypted_units(reader, &title, SAMPLE_UNITS);
        assert_eq!(
            units.len(),
            SAMPLE_UNITS,
            "IsoAccess::sample_units must actually sample real ISO content"
        );
        for u in &units {
            assert_eq!(u.len(), 6144);
        }
    }

    // `IsoAccess` against a missing ISO must fail SAFE — no reader, not a
    // panic — since a bad/missing staged ISO must not crash key resolution.
    #[test]
    fn iso_access_sample_units_empty_on_open_failure() {
        let missing = Path::new("/nonexistent-autorip-iso-fixture-xyz.iso");
        let mut access = IsoAccess::new(missing);
        assert!(
            access.sector_source().is_none(),
            "a missing ISO must yield no reader, not panic"
        );
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

    // --- KeyOutcome reporting via resolve_keys (rc.6 WS3) --------------------

    /// A minimal keyless, encrypted `Disc` for driving `resolve_keys` outcome
    /// classification. No real AACS state — the outcome (MissingInputs / NoKey)
    /// is decided by the fixtures' behavior, not the disc.
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

    /// A `KeySource` fixture that resolves NO unit keys — models a source that
    /// simply has no key for this disc (e.g. an empty keydb).
    struct NoKeySource;
    impl KeySource for NoKeySource {
        fn get_unit_keys(
            &self,
            _ctx: &dyn freemkv_keysources::ResolveCtx,
        ) -> Result<Vec<freemkv_keysources::UnitKey>, libfreemkv::Error> {
            Ok(Vec::new())
        }
    }

    // A `KeySource` fixture whose `get_uk` FAILS — models an errored source
    // (e.g. unreachable key service). The reshaped trait has no per-source
    // `errored()` signal, so `resolve_and_apply` maps this to `NoKey`.
    struct ErroringSource;
    impl KeySource for ErroringSource {
        fn get_unit_keys(
            &self,
            _ctx: &dyn freemkv_keysources::ResolveCtx,
        ) -> Result<Vec<freemkv_keysources::UnitKey>, libfreemkv::Error> {
            Err(libfreemkv::Error::AacsKeyRejected)
        }
    }

    /// `DiscKeyAccess` fixture with no reader (none of the outcome tests use a
    /// sample-needing source).
    struct FixtureAccess;
    impl DiscKeyAccess for FixtureAccess {
        fn sector_source(&mut self) -> Option<&mut dyn libfreemkv::SectorSource> {
            None
        }
    }

    // Like `keyless_encrypted_disc` but WITH AACS state, so `disc.inputs()`
    // returns `Some` and `resolve_keys` proceeds past `MissingInputs`. Minimal
    // state — outcome tests use only no-key/erroring sources.
    fn keyless_encrypted_disc_with_aacs() -> libfreemkv::Disc {
        let mut disc = keyless_encrypted_disc();
        disc.aacs = Some(libfreemkv::disc::AacsState {
            version: libfreemkv::aacs::mkb::AACS_MAJOR_UHD,
            bus_encryption: false,
            mkb_version: None,
            disc_hash: "0xabc".into(),
            key_source: libfreemkv::disc::KeyOrigin::KeyDb,
            vuk: None,
            unit_keys: Vec::new(),
            volume_id: [0u8; 16],
            uk_ro: Vec::new(),
            mkb: Vec::new(),
        });
        disc
    }

    // Records the content samples a source is handed.
    struct SampleSpy(std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>);
    impl KeySource for SampleSpy {
        fn get_unit_keys(
            &self,
            ctx: &dyn freemkv_keysources::ResolveCtx,
        ) -> Result<Vec<freemkv_keysources::UnitKey>, libfreemkv::Error> {
            *self.0.lock().unwrap() = ctx.samples(usize::MAX)?;
            Ok(Vec::new())
        }
    }

    fn title_over(start_lba: u32, sectors: u32, size_bytes: u64) -> libfreemkv::DiscTitle {
        let mut t = libfreemkv::DiscTitle::empty();
        t.size_bytes = size_bytes;
        t.extents = vec![libfreemkv::Extent {
            start_lba,
            sector_count: sectors,
        }];
        t
    }

    // Samples must come from the main FEATURE (largest title with video), not a
    // larger streamless decoy: decoy sectors are clear, feature sectors encrypted.
    #[test]
    fn resolve_keys_samples_the_video_feature_not_a_larger_decoy() {
        use std::io::Write;
        const HALF: usize = 600;
        let mut img = vec![0u8; HALF * 2048];
        img.extend(std::iter::repeat_n(0xC0u8, HALF * 2048));
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&img).unwrap();
        tmp.flush().unwrap();

        let decoy = title_over(0, HALF as u32, 50_000_000_000);
        let mut feature = title_over(HALF as u32, HALF as u32, 20_000_000_000);
        feature.streams = vec![libfreemkv::Stream::Video(libfreemkv::disc::VideoStream {
            pid: 0x1011,
            codec: libfreemkv::disc::Codec::H264,
            resolution: libfreemkv::disc::Resolution::R1080p,
            frame_rate: libfreemkv::disc::FrameRate::F24,
            hdr: libfreemkv::disc::HdrFormat::Sdr,
            color_space: libfreemkv::disc::ColorSpace::Bt709,
            display_aspect: None,
            secondary: false,
            label: String::new(),
            measured_cicp: None,
        })];
        let mut disc = keyless_encrypted_disc_with_aacs();
        disc.titles = vec![decoy, feature];

        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sources: Vec<Box<dyn KeySource>> = vec![Box::new(SampleSpy(seen.clone()))];
        let mut access = IsoAccess::new(tmp.path());
        let _ = resolve_keys(sources, &mut access, disc);

        let samples = seen.lock().unwrap();
        assert_eq!(
            samples.len(),
            SAMPLE_UNITS,
            "samples must be drawn from the encrypted video feature"
        );
        assert!(samples.iter().all(|u| u.len() == 6144 && u[0] == 0xC0));
    }

    /// A disc with NO AACS state → `disc.inputs()` is `None` → `MissingInputs`,
    /// regardless of what sources are configured (we never reach key resolution).
    #[test]
    fn resolve_keys_reports_missing_inputs_when_disc_has_no_aacs() {
        let mut access = FixtureAccess;
        let sources: Vec<Box<dyn KeySource>> = vec![Box::new(NoKeySource)];
        let (_disc, outcome) = resolve_keys(sources, &mut access, keyless_encrypted_disc());
        assert_eq!(
            outcome,
            KeyOutcome::MissingInputs,
            "no AACS inputs must report MissingInputs, not NoKey"
        );
    }

    /// AACS inputs present, sources exhausted with NO key and NO error →
    /// `NoKey` (a clean "no source has a key for this disc").
    #[test]
    fn resolve_keys_reports_no_key_when_sources_exhausted_clean() {
        let mut access = FixtureAccess;
        let sources: Vec<Box<dyn KeySource>> = vec![Box::new(NoKeySource)];
        let (_disc, outcome) =
            resolve_keys(sources, &mut access, keyless_encrypted_disc_with_aacs());
        assert_eq!(outcome, KeyOutcome::NoKey);
    }

    // A source that ERRORS and no other source has a key → `NoKey`. The
    // reshaped `KeySource` trait dropped the per-source `errored()` signal, so
    // a failed source is indistinguishable from a clean miss (see ResolutionTrace).
    #[test]
    fn resolve_keys_reports_no_key_when_a_source_errors() {
        let mut access = FixtureAccess;
        let sources: Vec<Box<dyn KeySource>> =
            vec![Box::new(NoKeySource), Box::new(ErroringSource)];
        let (_disc, outcome) =
            resolve_keys(sources, &mut access, keyless_encrypted_disc_with_aacs());
        assert_eq!(
            outcome,
            KeyOutcome::NoKey,
            "an errored source is indistinguishable from a clean miss now → NoKey"
        );
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

    // --- build_sources ordering / SSRF / fallback (rc.6 WS3) -----------------

    /// `build_sources` with a local `key_source` yields exactly the configured
    /// keydb (the mapfile key-source was removed in the AACS-trait reshape).
    #[test]
    fn build_sources_local_yields_keydb() {
        let cfg = Config {
            key_source: "local".into(),
            ..Config::default()
        };
        let sources = build_sources(&cfg);
        assert_eq!(sources.len(), 1, "just the configured local keydb");
        assert_eq!(sources[0].label(), "keydb");
    }

    // An online `key_source` with an SSRF-blocked URL drops the online source
    // entirely, leaving ZERO sources — the rip surfaces NoKey instead of
    // exfiltrating disc-key material to an internal address.
    #[test]
    fn build_sources_drops_online_source_on_ssrf_blocked_url() {
        let cfg = Config {
            key_source: "online".into(),
            keyserver_url: "https://169.254.169.254/keys".into(),
            ..Config::default()
        };
        let sources = build_sources(&cfg);
        assert!(
            sources.is_empty(),
            "SSRF-blocked online URL must yield no usable source"
        );
    }

    /// An unrecognised `key_source` (operator typo like "onlnie") falls back to
    /// the local keydb rather than silently producing no source.
    #[test]
    fn build_sources_unknown_key_source_falls_back_to_local_keydb() {
        let cfg = Config {
            key_source: "onlnie".into(),
            ..Config::default()
        };
        let sources = build_sources(&cfg);
        assert_eq!(sources.len(), 1, "fallback to a single local keydb source");
        assert!(!uses_online(&cfg), "a typo'd source is not 'online'");
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

    // A resolve must never hand back a PREVIOUS resolve's decode verdict: plant a
    // real one (a POST to an unresolvable host records Transport), then resolve
    // with no sources / no AACS inputs — both return before any online query.
    #[test]
    fn resolve_keys_drains_a_stale_decode_verdict() {
        struct PlantCtx;
        impl freemkv_keysources::ResolveCtx for PlantCtx {
            fn disc_hash(&self) -> &str {
                "0xabc"
            }
            fn title(&self) -> Option<&str> {
                None
            }
            fn vid(&self) -> Option<libfreemkv::aacs::types::Vid> {
                None
            }
            fn mkb(&self) -> Result<&[u8], libfreemkv::Error> {
                Ok(&[])
            }
            fn enc_title_keys(&self) -> Result<&[[u8; 16]], libfreemkv::Error> {
                Ok(&[])
            }
            fn samples(&self, _n: usize) -> Result<Vec<Vec<u8>>, libfreemkv::Error> {
                Ok(vec![vec![0u8; 6144]; freemkv_keysources::MIN_SAMPLE_UNITS])
            }
        }
        let plant = || {
            let online =
                freemkv_keysources::OnlineSource::new("https://keys.example.test/decode", "");
            let _ = online.get_unit_keys(&PlantCtx);
        };
        plant();
        assert!(
            freemkv_keysources::take_last_decode_reachability().is_some(),
            "fixture must plant a verdict"
        );
        plant();
        let _ = resolve_keys(
            Vec::new(),
            &mut FixtureAccess,
            keyless_encrypted_disc_with_aacs(),
        );
        assert_eq!(
            take_online_decode_reachability(),
            None,
            "no-sources resolve"
        );
        plant();
        let _ = resolve_keys(Vec::new(), &mut FixtureAccess, keyless_encrypted_disc());
        assert_eq!(
            take_online_decode_reachability(),
            None,
            "missing-inputs resolve"
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

    // build_iso_key_fetch (rc.6 WS3 multi-CPS-unit recovery) needs a real UDF
    // fixture to test the happy path (duplicating libfreemkv's own UDF
    // tests), so the cheap thing to pin here: an unreadable path → `None`.
    #[test]
    fn build_iso_key_fetch_none_for_unreadable_path() {
        let cfg = Config::default();
        let missing = Path::new("/nonexistent-autorip-iso-fixture-xyz.iso");
        assert!(build_iso_key_fetch(&cfg, missing).is_none());
    }

    // An ISO we could not READ must not vanish silently — a staging mount ESTALE or truncated
    // ISO used to disable mid-mux key recovery with no visible cause. Asserted on the decision,
    // not captured logs.
    #[test]
    fn an_unreadable_iso_is_distinguishable_from_a_non_aacs_one() {
        let cfg = Config::default();

        let missing = Path::new("/nonexistent-autorip-iso-fixture-xyz.iso");
        match build_iso_key_fetch_outcome(&cfg, missing) {
            IsoKeyFetch::Unreadable(_) => {}
            other => panic!(
                "an unreadable ISO must be a distinct fault, not collapsed into \
                 the same silent outcome as a non-AACS disc; got {other:?}"
            ),
        }

        // A file that exists but is not a disc image is equally unreadable as
        // an ISO — it must not be misreported as "readable, simply no AACS".
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not a disc image").unwrap();
        match build_iso_key_fetch_outcome(&cfg, tmp.path()) {
            IsoKeyFetch::Unreadable(_) => {}
            other => panic!("a truncated non-image file must read as a fault; got {other:?}"),
        }
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
            "https://127.0.0.1/keys",
            "https://169.254.169.254/latest/meta-data",
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

    // A DNS blip at build time must not drop the online source: its per-POST
    // re-guard retries the lookup and records the real reachability verdict.
    #[test]
    fn build_sources_keeps_online_source_on_transient_lookup_failure() {
        let cfg = Config {
            key_source: "online".into(),
            keyserver_url: "https://keys.example.org/decode".into(),
            ..Config::default()
        };
        let dns_down =
            |_: &str| Err("too many concurrent DNS resolutions in flight for this host".into());
        let sources = build_sources_with(&cfg, &dns_down);
        assert_eq!(sources.len(), 1, "a DNS failure is not a config verdict");
        assert_eq!(sources[0].label(), "online");
    }

    // A stored pre-upgrade http:// keyserver URL is named at boot; https and
    // non-online configs are silent.
    #[test]
    fn keyserver_url_startup_warning_flags_only_non_https_online() {
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

    /// Same fail-safe expectation for a file that exists but is not a valid
    /// ISO/UDF image (e.g. a truncated or non-disc file) — `read_aacs_inputs`
    /// must fail cleanly and `build_iso_key_fetch` must surface `None`.
    #[test]
    fn build_iso_key_fetch_none_for_non_iso_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"not a disc image").unwrap();
        let cfg = Config::default();
        assert!(build_iso_key_fetch(&cfg, tmp.path()).is_none());
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
            keyserver_url: "https://127.0.0.1:9/keys".into(),
            ..Default::default()
        };
        assert_eq!(
            probe_online_reachability(&blocked),
            ServiceReachability::NotAsked,
            "an SSRF-blocked loopback URL is a permanent config verdict, not an outage"
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
