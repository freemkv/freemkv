use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};

/// `output_format` value that means "deliver the whole-disc ISO image" rather
/// than a muxed title. The one place this literal lives — compare through it
/// (or [`crate::server::ripper::output_is_iso_image`]), never a bare `== "iso"`.
pub(crate) const OUTPUT_FORMAT_ISO: &str = "iso";
pub(crate) const OUTPUT_FORMAT_NETWORK: &str = "network";

// `#[serde(default)]` helper: an unspecified per-stage flag fires on that stage, preserving
// pre-1.6.8 behaviour where every webhook fired on completion.
fn default_true() -> bool {
    true
}

// Per-stage selection is opt-out; a legacy bare-string entry fires on every stage.
/// One configured webhook: the destination URL plus which of the three
/// pipeline stages (rip → mux → move) it fires on. The stages are distinct
/// events:
/// - `post_rip`: the disc read is done and the drive is free (sweep + patch
///   complete, `.ripped` handed off, disc ejected) — you can load the next
///   disc while the mux runs on a separate worker.
/// - `post_mux`: the `.mkv` has been produced from the staged ISO.
/// - `post_move`: the finished file has landed in its final library location.
#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct WebhookEntry {
    pub url: String,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
    /// Fire when the disc read finishes and the drive is free (the
    /// `rip_complete` payload).
    pub post_rip: bool,
    /// Fire when the mux produces the `.mkv` (the `mux_complete` payload).
    pub post_mux: bool,
    /// Fire this webhook when a moved file lands in its final library
    /// location (the `move_complete` payload).
    pub post_move: bool,
}

impl std::fmt::Debug for WebhookEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookEntry")
            .field("url", &"<redacted>")
            .field("headers", &"<redacted>")
            .field("post_rip", &self.post_rip)
            .field("post_mux", &self.post_mux)
            .field("post_move", &self.post_move)
            .finish()
    }
}

impl WebhookEntry {
    /// A URL entry that fires on every stage — the pre-1.6.8 "notify on
    /// completion" behaviour and the shape a bare legacy string migrates to.
    fn all_stages(url: String) -> Self {
        Self {
            url,
            post_rip: true,
            post_mux: true,
            post_move: true,
            headers: Default::default(),
        }
    }

    /// Parse one `webhook_urls` element: a legacy bare string or the object
    /// form, where an ABSENT flag defaults to `true`. `Err` names the offending
    /// field (`webhook_urls[i]` or `webhook_urls[i].<flag>`); blank URLs pass.
    pub(crate) fn parse(i: usize, v: &serde_json::Value) -> Result<Self, String> {
        if let Some(s) = v.as_str() {
            return Ok(Self::all_stages(s.to_string()));
        }
        let malformed = || format!("webhook_urls[{i}]");
        let obj = v.as_object().ok_or_else(malformed)?;
        let url = obj
            .get("url")
            .and_then(|u| u.as_str())
            .ok_or_else(malformed)?;
        let flag = |k: &str| match obj.get(k) {
            None => Ok(true),
            Some(b) => b.as_bool().ok_or_else(|| format!("webhook_urls[{i}].{k}")),
        };
        Ok(Self {
            url: url.to_string(),
            post_rip: flag("post_rip")?,
            post_mux: flag("post_mux")?,
            post_move: flag("post_move")?,
            headers: {
                let mut headers: std::collections::BTreeMap<String, String> = obj
                    .get("headers")
                    .map(|v| serde_json::from_value(v.clone()).map_err(|_| malformed()))
                    .transpose()?
                    .unwrap_or_default();
                // Migrate the short-lived service-specific configuration.
                if let Some(key) = obj
                    .get("jellyfin_api_key")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    headers
                        .entry("Authorization".into())
                        .or_insert_with(|| format!("MediaBrowser Token=\"{key}\""));
                }
                if headers.len() > 16 {
                    return Err(malformed());
                }
                let mut names = std::collections::HashSet::new();
                for (name, value) in &headers {
                    let lower = name.to_ascii_lowercase();
                    if name.len() > 128
                        || value.len() > 8192
                        || ureq::http::HeaderName::from_bytes(name.as_bytes()).is_err()
                        || !value.bytes().all(|b| b == b'\t' || (32..=126).contains(&b))
                        || matches!(
                            lower.as_str(),
                            "host" | "content-length" | "transfer-encoding" | "connection"
                        )
                        || !names.insert(lower)
                    {
                        return Err(format!("webhook_urls[{i}].headers"));
                    }
                }
                headers
            },
        })
    }

    // Loader form: a malformed element is dropped (with a warning), as is a
    // blank URL.
    pub(crate) fn from_json(i: usize, v: &serde_json::Value) -> Option<Self> {
        match Self::parse(i, v) {
            Ok(entry) => (!entry.url.trim().is_empty()).then_some(entry),
            Err(field) => {
                tracing::warn!(%field, "settings.json webhook entry malformed (non-bool flag or missing url) - dropped");
                None
            }
        }
    }
}

/// Custom `Deserialize` accepting the legacy bare string or the modern object,
/// so a settings.json (or `Config`) that predates per-event webhooks still
/// loads. Serialization always emits the object form (derived `Serialize`).
impl<'de> Deserialize<'de> for WebhookEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Self::parse(0, &value).map_err(serde::de::Error::custom)
    }
}

/// Runtime config. Single source of truth is `settings.json` on disk;
/// the UI POSTs updates to it via `/api/settings`.
///
/// Only `PORT`, `AUTORIP_DIR`, `AUTORIP_LOG_LEVEL`, `RIP_USER`, and `NFS_*` are read from the
/// environment (all bootstrap-only, needed before `settings.json` can be loaded). Every other
/// field is operator-facing and UI-only, with no env-var fallback.
#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    /// Bootstrap-only (env `PORT`, set before the server binds). Never
    /// persisted: `load_saved` never reads it back, so writing it into
    /// settings.json would only create misleading on-disk state that an
    /// operator could edit to no effect. `#[serde(default)]` keeps any
    /// stale value from an older settings.json deserializing cleanly.
    #[serde(skip_serializing, default = "default_port")]
    pub port: u16,
    pub staging_dir: String,
    pub output_dir: String,
    pub movie_dir: String,
    pub tv_dir: String,
    pub min_length_secs: u64,
    pub main_feature: bool,
    /// Fully-automatic TV: when a disc resolves as a series, rip every episode
    /// title (not just the main feature), auto-number them `S{NN}E{MM}` from
    /// TMDB, and file into `Show (Year)/Season NN/` — no operator step. Default
    /// true (it's *auto*rip). When false, a TV disc is held for review so the
    /// operator confirms selection/season/episodes before filing.
    #[serde(default = "default_true")]
    pub tv_auto: bool,
    pub auto_eject: bool,
    pub on_insert: String,      // "nothing", "scan", "rip", "resume"
    pub output_format: String,  // "mkv", "m2ts", "iso", "network"
    pub network_target: String, // e.g. "nas.example.com:9000" for network output
    pub on_read_error: String,  // "stop", "skip"
    /// Number of retry passes over the disc. 0 = single pass (read the disc
    /// once, no retries). 1..=10 = multi-pass: an initial sweep plus N retry
    /// passes over the bad ranges. This controls pass count only; the output
    /// container (MKV / M2TS / ISO) is selected by `output_format`.
    pub max_retries: u8,
    /// Promote the intermediate ISO into the output library alongside the muxed
    /// title (MKV/M2TS) after mux completes. Defaults to false — the ISO is
    /// pruned once the title is finalized. The disc mapfile is staging-only and
    /// never promoted.
    pub keep_iso: bool,
    /// Where a kept/output disc image (`.iso`) is filed. Resolved UNDER
    /// `output_dir` exactly like `movie_dir`/`tv_dir`: a RELATIVE value
    /// ("isos") joins onto `output_dir` (→ `/mnt/media/isos`); an ABSOLUTE
    /// value ("/mnt/archive/isos") targets another disk. EMPTY (default)
    /// keeps the legacy behaviour — the ISO is filed beside the muxed title.
    /// ISOs land FLAT in this folder (`<root>/<Title (Year)>.iso`), never a
    /// per-title subtree. Applies to both the `keep_iso` companion and a
    /// whole-disc `output_format = "iso"` rip.
    #[serde(default)]
    pub iso_dir: String,
    /// Abort rip if main-movie loss exceeds N seconds after retries.
    /// 0 = perfect rip required (abort on any remaining main-movie loss).
    pub abort_on_lost_secs: u64,
    /// When a disc has no usable keys: if true, capture it to an ISO anyway and
    /// defer the mux until keys are available; if false (default), abort the rip
    /// with an explicit message. The operator's "proceed vs abort" decision.
    pub capture_without_keys: bool,
    /// Maximum total time for entire rip across all passes (seconds). Prevents infinite hangs.
    pub max_rip_duration_secs: u64,
    /// Minimum per-pass wallclock budget (seconds), used when disc runtime is unknown.
    pub min_pass_budget_secs: u64,
    /// Transport failure recovery: delay after USB re-enumeration before retrying open (seconds).
    pub transport_recovery_delay_secs: u64,
    pub tmdb_api_key: String,
    pub keydb_path: Option<String>,
    pub keydb_url: String,
    /// Where keys come from, and only from: "local" (the keydb) or "online"
    /// (the key service at `keyserver_url`).
    pub key_source: String,
    /// Base URL of the online key service, used when `key_source` is "online".
    pub keyserver_url: String,
    /// Optional bearer token for the key service. Empty = none.
    pub keyserver_secret: String,
    /// Configured webhooks. Each carries its destination URL and which
    /// completion events it fires on (see [`WebhookEntry`]). Kept as
    /// `webhook_urls` for on-disk/serde compatibility with pre-1.6.7
    /// settings.json files, which stored a bare array of URL strings.
    pub webhook_urls: Vec<WebhookEntry>,
    pub autorip_dir: String,

    /// Number of threads for AACS decryption. 0 = auto (all available
    /// cores, capped at libfreemkv's MAX_THREADS). Applied at startup and
    /// whenever the UI POSTs a change via
    /// `libfreemkv::decrypt::set_decrypt_threads`.
    pub decrypt_threads: usize,

    /// How long to keep per-device `.log` files in `$AUTORIP_DIR/logs`
    /// before the in-process prune thread deletes them.
    pub log_retention_days: u64,

    /// Library: the folder of `Title/Title.mkv` files. Empty = the movie folder.
    #[serde(default)]
    pub library_dir: String,
    /// Library: the folder of source ISOs. Empty = the ISO folder above, if set.
    #[serde(default)]
    pub library_iso_dir: String,
    /// Library: also list ISOs one folder down (`dvd/`, `bd/`, ...). Off = top level only.
    #[serde(default)]
    pub library_iso_subfolders: bool,
    /// Library: decode every MKV in full with ffmpeg, in the background. Off by default.
    #[serde(default)]
    pub deep_audit: bool,
    /// Remux: days a finished MKV kept on local staging waits for the output folder before
    /// it is discarded at startup. 0 = the default (7).
    #[serde(default)]
    pub remux_staged_max_age_days: u64,
    /// Remux: GB the kept MKVs may hold on local staging; the oldest go first at startup.
    /// 0 = the default (a quarter of the staging disk, at most 500 GB).
    #[serde(default)]
    pub remux_staged_max_gb: u64,
}

// Manual `Debug` that redacts secret-bearing fields (tmdb_api_key,
// keyserver_secret, webhook_urls — the latter embed bearer tokens in
// their path/query) so `tracing::debug!(?cfg)` never leaks them.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn redact(s: &str) -> &'static str {
            if s.is_empty() {
                "<unset>"
            } else {
                "<redacted>"
            }
        }
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("staging_dir", &self.staging_dir)
            .field("output_dir", &self.output_dir)
            .field("movie_dir", &self.movie_dir)
            .field("tv_dir", &self.tv_dir)
            .field("min_length_secs", &self.min_length_secs)
            .field("main_feature", &self.main_feature)
            .field("tv_auto", &self.tv_auto)
            .field("auto_eject", &self.auto_eject)
            .field("on_insert", &self.on_insert)
            .field("output_format", &self.output_format)
            .field("network_target", &self.network_target)
            .field("on_read_error", &self.on_read_error)
            .field("max_retries", &self.max_retries)
            .field("keep_iso", &self.keep_iso)
            .field("iso_dir", &self.iso_dir)
            .field("abort_on_lost_secs", &self.abort_on_lost_secs)
            .field("capture_without_keys", &self.capture_without_keys)
            .field("max_rip_duration_secs", &self.max_rip_duration_secs)
            .field("min_pass_budget_secs", &self.min_pass_budget_secs)
            .field(
                "transport_recovery_delay_secs",
                &self.transport_recovery_delay_secs,
            )
            .field("tmdb_api_key", &redact(&self.tmdb_api_key))
            .field("keydb_path", &self.keydb_path)
            .field("keydb_url", &redact(&self.keydb_url))
            .field("key_source", &self.key_source)
            .field("keyserver_url", &redact(&self.keyserver_url))
            .field("keyserver_secret", &redact(&self.keyserver_secret))
            .field(
                "webhook_urls",
                &format!("[{} redacted]", self.webhook_urls.len()),
            )
            .field("autorip_dir", &self.autorip_dir)
            .field("decrypt_threads", &self.decrypt_threads)
            .field("log_retention_days", &self.log_retention_days)
            .field("library_dir", &self.library_dir)
            .field("library_iso_dir", &self.library_iso_dir)
            .field("library_iso_subfolders", &self.library_iso_subfolders)
            .field("deep_audit", &self.deep_audit)
            .field("remux_staged_max_age_days", &self.remux_staged_max_age_days)
            .field("remux_staged_max_gb", &self.remux_staged_max_gb)
            .finish()
    }
}

/// Default web bind port — used by `#[serde(default)]` on `port` when an
/// older settings.json carried the (now non-serialized) field. The live
/// value always comes from the `PORT` env var via [`load`].
pub(crate) fn default_port() -> u16 {
    8080
}

impl Default for Config {
    // First-boot defaults, from the settings schema. `PORT`/`AUTORIP_DIR` are
    // spliced in by [`load`], the only two knobs that come from env.
    fn default() -> Self {
        let mut m = crate::server::settings_schema::defaults();
        m.insert("autorip_dir".into(), serde_json::json!("/config"));
        serde_json::from_value(serde_json::Value::Object(m))
            .expect("the settings schema defaults form a Config")
    }
}

impl Config {
    pub fn staging_device_dir(&self, device: &str) -> String {
        format!("{}/{}", self.staging_dir, device)
    }
    pub fn log_dir(&self) -> String {
        format!("{}/logs", self.autorip_dir)
    }
    pub fn settings_file(&self) -> String {
        format!("{}/settings.json", self.autorip_dir)
    }
}

/// True if `p` is an existing directory we can create a file in.
fn dir_is_writable(p: &str) -> bool {
    let probe = std::path::Path::new(p).join(".autorip-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Resolve where autorip keeps all its state (settings.json, logs, keys,
/// staging, output). Identical logic on every OS, and always returns a
/// real absolute path the UI/logs can show verbatim.
///
/// Order:
///   1. `AUTORIP_DIR` env var, if set.
///   2. A writable `/config` (the Docker bind mount).
///   3. A `config` folder next to the executable.
///   4. Last resort: the working directory + `config`.
pub fn default_autorip_dir() -> String {
    if let Ok(d) = std::env::var("AUTORIP_DIR")
        && !d.is_empty()
    {
        return d;
    }
    if std::path::Path::new("/config").is_dir() && dir_is_writable("/config") {
        return "/config".to_string();
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        return parent.join("config").to_string_lossy().into_owned();
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd.join("config").to_string_lossy().into_owned();
    }
    "config".to_string()
}

// Parses `PORT`'s raw string into a bind port; `None` for anything not a
// valid 1..=65535 port (unparseable, or the reserved `0` sentinel) so the
// caller can warn and fall back to 8080 instead of binding ephemerally.
pub(crate) fn parse_port_env(s: &str) -> Option<u16> {
    match s.trim().parse::<u16>() {
        Ok(p) if p != 0 => Some(p),
        _ => None,
    }
}

// Builds the bootstrap `Config` from the two env-sourced values layered
// onto the hardcoded defaults; kept as its own function (not an inline
// literal) so a test can assert `port`/`autorip_dir` survive the `..`.
fn build_bootstrap_config(port: u16, autorip_dir: String) -> Config {
    Config {
        port,
        autorip_dir,
        ..Config::default()
    }
}

// True only when `dir` is still exactly the container-default path AND
// that path doesn't exist on disk (no bind mount) — an operator who
// customized the path, or has the real mount present, must NOT relocate.
fn should_relocate_bare_run_dir(dir: &str, default_path: &str, default_path_exists: bool) -> bool {
    dir == default_path && !default_path_exists
}

pub fn load() -> Arc<RwLock<Config>> {
    // Only the two bootstrap-only env vars are read here. Everything
    // else comes from settings.json (or Config::default if it's a
    // first boot with no settings file).
    let autorip_dir = default_autorip_dir();
    // PORT is bootstrap-only; a bad value must not silently bind 8080
    // while the operator thinks their value took. Warn and fall back.
    let port: u16 = match std::env::var("PORT") {
        Ok(s) => match parse_port_env(&s) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    value = %s,
                    "PORT env var is not a valid 1-65535 port; falling back to 8080"
                );
                default_port()
            }
        },
        Err(_) => default_port(),
    };

    let mut cfg = build_bootstrap_config(port, autorip_dir);
    cfg = load_saved(cfg);

    // Bare-run (no container): relocate default staging/output under the config
    // dir if those root paths don't exist, so the binary runs unmounted. Checks
    // existence (not writability) so transient NFS issues in Docker don't relocate.
    if should_relocate_bare_run_dir(
        &cfg.staging_dir,
        "/staging",
        std::path::Path::new("/staging").exists(),
    ) {
        // Native join keeps the platform separator (no mixed-slash path on Windows).
        cfg.staging_dir = std::path::Path::new(&cfg.autorip_dir)
            .join("staging")
            .to_string_lossy()
            .into_owned();
    }
    if should_relocate_bare_run_dir(
        &cfg.output_dir,
        "/output",
        std::path::Path::new("/output").exists(),
    ) {
        cfg.output_dir = std::path::Path::new(&cfg.autorip_dir)
            .join("output")
            .to_string_lossy()
            .into_owned();
    }
    // Bounded: staging and output may sit on a network mount that has stopped answering,
    // and startup (the web server with it) must not wait on it.
    let dirs = vec![
        cfg.log_dir(),
        format!("{}/freemkv", cfg.autorip_dir),
        cfg.staging_dir.clone(),
        cfg.output_dir.clone(),
    ];
    let created = ensure_dirs_bounded(dirs, crate::server::health::CHECK_TIMEOUT, |p| {
        std::fs::create_dir_all(p)
    });
    for (d, result) in created {
        if let Err(reason) = result {
            tracing::warn!(path = %d, error = %reason, "could not create required directory");
        }
    }

    // Apply the persisted decrypt thread count to libfreemkv's
    // global pool. Subsequent UI POSTs re-apply via the same fn.
    apply_decrypt_threads(cfg.decrypt_threads);

    Arc::new(RwLock::new(cfg))
}

// Create every one of `dirs` at once through the folder health check's `bounded`, each given
// up on after `limit` (its thread left to finish on its own), so folders on one hung mount
// cost one timeout between them. Results keep the input order.
fn ensure_dirs_bounded<F>(
    dirs: Vec<String>,
    limit: std::time::Duration,
    create: F,
) -> Vec<(String, Result<(), String>)>
where
    F: Fn(&std::path::Path) -> std::io::Result<()> + Clone + Send + 'static,
{
    use crate::server::health::{Bounded, bounded};
    std::thread::scope(|scope| {
        let running: Vec<_> = dirs
            .into_iter()
            .map(|d| {
                let create = create.clone();
                let owned = std::path::PathBuf::from(&d);
                let key = owned.clone();
                let h = scope.spawn(move || match bounded(&key, limit, move || create(&owned)) {
                    Bounded::Done(r) => r.map_err(|e| e.to_string()),
                    Bounded::TimedOut | Bounded::Busy => Err(format!(
                        "not responding after {}s (a stale network mount?)",
                        limit.as_secs().max(1)
                    )),
                });
                (d, h)
            })
            .collect();
        running
            .into_iter()
            .map(|(d, h)| {
                let r = h
                    .join()
                    .unwrap_or_else(|_| Err("the folder check panicked".into()));
                (d, r)
            })
            .collect()
    })
}

/// Apply the configured decrypt thread count to libfreemkv's global
/// rayon pool. 0 means "auto" — it resets libfreemkv to its own default
/// (all cores, capped), dropping any earlier explicit count. UI invokes this on settings POST so
/// changes take effect without restarting the container.
pub fn apply_decrypt_threads(n: usize) {
    libfreemkv::decrypt::set_decrypt_threads(n);
}

fn load_saved(mut cfg: Config) -> Config {
    let path = cfg.settings_file();
    let data = match std::fs::read_to_string(&path) {
        Ok(data) => data,
        Err(e) => {
            // ENOENT is the legitimate first-boot case — stay silent. Any
            // other io error (perms, NFS stale handle) means we're falling
            // back to defaults for a non-obvious reason; surface it.
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path, error = %e, "settings.json unreadable - using defaults");
            }
            return cfg;
        }
    };
    let saved = match serde_json::from_str::<serde_json::Value>(&data) {
        Ok(saved) => saved,
        Err(e) => {
            // A parse failure (e.g. partial write from a SIGKILL mid-save,
            // seen with Watchtower restarts) silently reverts all fields
            // to defaults. Warn so the operator can see why.
            tracing::warn!(path = %path, error = %e, "settings.json failed to parse - all settings reverting to defaults");
            return cfg;
        }
    };
    // Overlay saved settings onto defaults, each field on its own (see the schema).
    crate::server::settings_schema::load_into(&mut cfg, &saved);
    cfg
}

/// Persist `cfg` to `settings.json` atomically (temp file + fsync + rename).
///
/// Returns `Ok(())` only when the on-disk file was successfully replaced.
/// On serialize failure, temp-write failure, or rename failure the change
/// did NOT land on disk and an `Err` is returned (and logged) so the caller
/// can surface the failure instead of falsely reporting success.
pub fn save(cfg: &Config) -> std::io::Result<()> {
    let path = cfg.settings_file();
    let json = match serde_json::to_string_pretty(cfg) {
        Ok(json) => json,
        Err(e) => {
            tracing::warn!(error = %e, "settings serialize failed; settings.json unchanged");
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e));
        }
    };
    // Write atomically (temp file + fsync + rename) so a SIGKILL/OOM mid-write
    // can't truncate settings.json and reset all fields to defaults on restart.
    // Temp name includes pid + counter so concurrent saves never collide.
    static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = format!(
        "{path}.tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write as _;
        // settings.json holds secrets (tmdb_api_key, keyserver_secret, webhook
        // tokens). Create the temp file 0600 so those bytes are never briefly
        // world-readable before the rename publishes them.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!(error = %e, "settings write/fsync failed; settings.json unchanged");
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!(error = %e, "settings rename failed; settings.json unchanged");
        return Err(e);
    }
    Ok(())
}

/// Allocate the generation for a settings snapshot. Take it while holding the
/// config write guard so generation order matches in-memory mutation order.
pub fn next_save_generation() -> u64 {
    static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

type SaveWaiter = std::sync::mpsc::Sender<std::io::Result<()>>;

struct PendingSave {
    generation: u64,
    cfg: Config,
    waiters: Vec<SaveWaiter>,
}

#[derive(Default)]
struct SaveSlot {
    pending: Option<PendingSave>,
    writer_active: bool,
    persisted: u64,
}

// Per settings file: the newest snapshot not yet written, plus writer state.
static SAVE_SLOTS: std::sync::Mutex<std::collections::BTreeMap<String, SaveSlot>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

fn copy_io_result(r: &std::io::Result<()>) -> std::io::Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) => Err(std::io::Error::new(e.kind(), e.to_string())),
    }
}

/// Queue `cfg` (tagged with a [`next_save_generation`] value) for [`save`].
///
/// One writer thread per settings file; snapshots queued while it is busy
/// coalesce so only the newest is written, and a snapshot older than one
/// already persisted is acknowledged `Ok` without writing. A hung write
/// parks only that writer. The receiver yields the result covering this
/// snapshot; `Err` only when the writer thread could not be spawned.
pub fn save_coalesced(
    cfg: Config,
    generation: u64,
) -> std::io::Result<std::sync::mpsc::Receiver<std::io::Result<()>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let path = cfg.settings_file();
    let mut slots = SAVE_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    let slot = slots.entry(path.clone()).or_default();
    if generation <= slot.persisted {
        let _ = tx.send(Ok(()));
        return Ok(rx);
    }
    match slot.pending.as_mut() {
        Some(p) => {
            if generation > p.generation {
                p.generation = generation;
                p.cfg = cfg;
            }
            p.waiters.push(tx);
        }
        None => {
            slot.pending = Some(PendingSave {
                generation,
                cfg,
                waiters: vec![tx],
            });
        }
    }
    if !slot.writer_active {
        let writer_path = path.clone();
        std::thread::Builder::new()
            .name("autorip-settings-save".into())
            .spawn(move || run_save_writer(&writer_path))
            .inspect_err(|_| {
                slot.pending = None;
            })?;
        slot.writer_active = true;
    }
    Ok(rx)
}

fn run_save_writer(path: &str) {
    loop {
        let job = {
            let mut slots = SAVE_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slots.entry(path.to_string()).or_default();
            match slot.pending.take() {
                Some(job) if job.generation <= slot.persisted => {
                    for w in job.waiters {
                        let _ = w.send(Ok(()));
                    }
                    continue;
                }
                Some(job) => job,
                None => {
                    slot.writer_active = false;
                    return;
                }
            }
        };
        let result = save(&job.cfg);
        if result.is_ok() {
            let mut slots = SAVE_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
            let slot = slots.entry(path.to_string()).or_default();
            slot.persisted = slot.persisted.max(job.generation);
        }
        for w in job.waiters {
            let _ = w.send(copy_io_result(&result));
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
