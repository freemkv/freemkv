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
#[derive(Clone, Serialize, PartialEq, Eq, Debug)]
pub struct WebhookEntry {
    pub url: String,
    /// Fire when the disc read finishes and the drive is free (the
    /// `rip_complete` payload).
    pub post_rip: bool,
    /// Fire when the mux produces the `.mkv` (the `mux_complete` payload).
    pub post_mux: bool,
    /// Fire this webhook when a moved file lands in its final library
    /// location (the `move_complete` payload).
    pub post_move: bool,
}

impl WebhookEntry {
    /// A URL entry that fires on every stage — the pre-1.6.8 "notify on
    /// completion" behaviour and the shape a bare legacy string migrates to.
    fn both(url: String) -> Self {
        Self {
            url,
            post_rip: true,
            post_mux: true,
            post_move: true,
        }
    }

    /// Parse one `webhook_urls` element: a legacy bare string or the object
    /// form, where an ABSENT flag defaults to `true`. `Err` names the offending
    /// field (`webhook_urls[i]` or `webhook_urls[i].<flag>`); blank URLs pass.
    pub(crate) fn parse(i: usize, v: &serde_json::Value) -> Result<Self, String> {
        if let Some(s) = v.as_str() {
            return Ok(Self::both(s.to_string()));
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
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Url(String),
            Obj {
                url: String,
                #[serde(default = "default_true")]
                post_rip: bool,
                #[serde(default = "default_true")]
                post_mux: bool,
                #[serde(default = "default_true")]
                post_move: bool,
            },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Url(url) => WebhookEntry::both(url),
            Raw::Obj {
                url,
                post_rip,
                post_mux,
                post_move,
            } => WebhookEntry {
                url,
                post_rip,
                post_mux,
                post_move,
            },
        })
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
    pub output_format: String,  // "mkv", "m2ts", "iso"
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
            .finish()
    }
}

/// Default web bind port — used by `#[serde(default)]` on `port` when an
/// older settings.json carried the (now non-serialized) field. The live
/// value always comes from the `PORT` env var via [`load`].
fn default_port() -> u16 {
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
fn parse_port_env(s: &str) -> Option<u16> {
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
                8080
            }
        },
        Err(_) => 8080,
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
    for d in [
        cfg.log_dir(),
        format!("{}/freemkv", cfg.autorip_dir),
        cfg.staging_dir.clone(),
        cfg.output_dir.clone(),
    ] {
        if let Err(e) = std::fs::create_dir_all(&d) {
            tracing::warn!(path = %d, error = %e, "could not create required directory");
        }
    }

    // Apply the persisted decrypt thread count to libfreemkv's
    // global pool. Subsequent UI POSTs re-apply via the same fn.
    apply_decrypt_threads(cfg.decrypt_threads);

    Arc::new(RwLock::new(cfg))
}

/// Apply the configured decrypt thread count to libfreemkv's global
/// rayon pool. 0 means "auto" — let libfreemkv fall back to its own
/// default (all cores, capped). UI invokes this on settings POST so
/// changes take effect without restarting the container.
pub fn apply_decrypt_threads(n: usize) {
    if n > 0 {
        libfreemkv::decrypt::set_decrypt_threads(n);
    }
    // n == 0 leaves the existing setting in place; there's no libfreemkv
    // "reset to default" hook, and lazy init already picked the right default.
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
mod tests {
    use super::*;

    // Per project convention, tests never touch /tmp (wiped on reboot).
    // Anchor scratch under the workspace's target/ (gitignored), not /tmp.
    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!(
                "autorip-config-test-{}-{}-{}",
                std::process::id(),
                tag,
                n
            ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg_in(dir: &std::path::Path) -> Config {
        Config {
            autorip_dir: dir.to_string_lossy().to_string(),
            ..Config::default()
        }
    }

    // H9: an older snapshot queued after a newer one must never land last.
    #[test]
    fn save_coalesced_never_lets_an_older_snapshot_land_last() {
        let d = scratch("save_order");
        let mut older = cfg_in(&d);
        older.tmdb_api_key = "older".into();
        let mut newer = cfg_in(&d);
        newer.tmdb_api_key = "newer".into();
        let path = newer.settings_file();
        let g_old = next_save_generation();
        let g_new = next_save_generation();
        let rx_new = save_coalesced(newer, g_new).expect("spawn writer");
        let rx_old = save_coalesced(older, g_old).expect("spawn writer");
        let wait = std::time::Duration::from_secs(10);
        rx_new.recv_timeout(wait).unwrap().expect("newer save");
        rx_old
            .recv_timeout(wait)
            .unwrap()
            .expect("a superseded save is acknowledged Ok");
        let data = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(
            parsed["tmdb_api_key"].as_str(),
            Some("newer"),
            "an older snapshot finishing last must not overwrite the newest one"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // A failed write reports Err to its waiter and does not mark it persisted.
    #[test]
    fn save_coalesced_reports_write_failure() {
        let d = scratch("save_fail");
        let cfg = cfg_in(&d.join("missing-dir"));
        let rx = save_coalesced(cfg, next_save_generation()).expect("spawn writer");
        let r = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        assert!(r.is_err(), "a save into a missing dir must report Err");
        let _ = std::fs::remove_dir_all(&d);
    }

    // H5/H10: a webhook entry with a non-bool flag is dropped AND reported.
    #[test]
    fn load_saved_warns_on_non_bool_webhook_flag() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::fmt::MakeWriter;
        use tracing_subscriber::layer::SubscriberExt;

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let d = scratch("webhook_flag");
        let base = cfg_in(&d);
        std::fs::write(
            base.settings_file(),
            serde_json::json!({
                "webhook_urls": [
                    {"url": "https://example.com/bad", "post_mux": "yes"},
                    {"url": "https://example.com/good", "post_rip": false},
                ]
            })
            .to_string(),
        )
        .unwrap();
        let buf = Buf::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(buf.clone())
                .with_ansi(false),
        );
        let cfg = tracing::subscriber::with_default(subscriber, || load_saved(base));
        assert_eq!(
            cfg.webhook_urls,
            vec![WebhookEntry {
                url: "https://example.com/good".into(),
                post_rip: false,
                post_mux: true,
                post_move: true,
            }],
            "the malformed entry is dropped; the valid one (absent flags -> true) is kept"
        );
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            out.contains("webhook_urls[0].post_mux"),
            "dropping a webhook with a non-bool flag must be logged, naming the field; logs:\n{out}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_writes_atomically_and_leaves_no_temp() {
        let d = scratch("save_ok");
        let mut cfg = cfg_in(&d);
        cfg.tmdb_api_key = "abc123".into();
        save(&cfg).expect("save must succeed to a writable dir");

        let path = cfg.settings_file();
        let data = std::fs::read_to_string(&path).expect("settings.json written");
        let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(parsed["tmdb_api_key"].as_str(), Some("abc123"));
        // The sibling temp file must be cleaned up (renamed away).
        assert!(
            !std::path::Path::new(&format!("{path}.tmp")).exists(),
            "temp file should not linger after a successful save"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn concurrent_saves_never_corrupt_settings_json() {
        // Two settings saves can run concurrently (one thread per request).
        // The unique-per-call temp name (pid + counter) gives each its own
        // sibling temp, so every rename publishes one writer's COMPLETE bytes.
        let d = scratch("concurrent");
        const N: usize = 16;
        let handles: Vec<_> = (0..N)
            .map(|i| {
                let dir = d.clone();
                std::thread::spawn(move || {
                    let mut cfg = cfg_in(&dir);
                    // Distinct, generously-sized payload per thread so an
                    // interleave would produce invalid JSON, not a value
                    // that happens to parse.
                    cfg.tmdb_api_key = format!("key-{i}-{}", "x".repeat(4096));
                    save(&cfg)
                })
            })
            .collect();
        for h in handles {
            h.join().expect("save thread panicked").expect("save Err");
        }

        // Final file is valid JSON (no interleave corruption).
        let path = cfg_in(&d).settings_file();
        let data = std::fs::read_to_string(&path).expect("settings.json written");
        let parsed: serde_json::Value = serde_json::from_str(&data)
            .expect("settings.json must be valid JSON after concurrency");
        let key = parsed["tmdb_api_key"].as_str().unwrap_or("");
        assert!(
            key.starts_with("key-") && key.ends_with(&"x".repeat(4096)),
            "final key must be one writer's COMPLETE value, got {} chars",
            key.len()
        );

        // No temp turds linger (every save renamed its own unique temp away).
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no .tmp files should remain, found {}",
            leftovers.len()
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_leaves_prior_settings_untouched_when_write_fails() {
        // Failure-mode contract: if the temp write/fsync fails, the rename
        // must never run, so a pre-existing settings.json is preserved.
        // Force the failure via a path whose parent doesn't exist (ENOENT).
        let d = scratch("save_fail");
        let good = cfg_in(&d);
        // Seed a valid prior file.
        let mut prior = good.clone();
        prior.tmdb_api_key = "PRIOR".into();
        save(&prior).expect("seeding the prior settings.json must succeed");
        let good_path = good.settings_file();
        let before = std::fs::read_to_string(&good_path).unwrap();

        // Now attempt a save whose temp open will fail: settings_file()
        // lives under a non-existent subdirectory, so OpenOptions::open
        // returns ENOENT and save() must bail before any rename.
        let bad = cfg_in(&d.join("does-not-exist"));
        let mut changed = bad.clone();
        changed.tmdb_api_key = "SHOULD_NOT_LAND".into();
        // This save is EXPECTED to fail (ENOENT) — the point of the test.
        assert!(save(&changed).is_err(), "save into a missing dir must Err");

        // The good file is byte-for-byte intact.
        let after = std::fs::read_to_string(&good_path).unwrap();
        assert_eq!(before, after, "prior settings.json must be untouched");
        // No temp turd left behind in the bad location either.
        assert!(!std::path::Path::new(&format!("{}.tmp", bad.settings_file())).exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn load_saved_clamps_pathological_durations() {
        let d = scratch("clamp");
        let path = cfg_in(&d).settings_file();
        std::fs::write(
            &path,
            serde_json::json!({
                "max_rip_duration_secs": u64::MAX,
                "min_pass_budget_secs": u64::MAX,
                "log_retention_days": u64::MAX,
            })
            .to_string(),
        )
        .unwrap();
        let cfg = load_saved(cfg_in(&d));
        assert!(cfg.max_rip_duration_secs <= 30 * 24 * 3600);
        assert!(cfg.min_pass_budget_secs <= 30 * 24 * 3600);
        assert!(cfg.log_retention_days <= 3650);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn port_not_serialized_into_settings_json() {
        let d = scratch("port");
        let cfg = cfg_in(&d);
        save(&cfg).expect("save must succeed to a writable dir");
        let data = std::fs::read_to_string(cfg.settings_file()).unwrap();
        assert!(
            !data.contains("\"port\""),
            "port is bootstrap-only and must not be persisted: {data}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn decrypt_threads_clamped_on_load() {
        let d = scratch("decrypt_clamp");
        let base = cfg_in(&d);
        std::fs::write(base.settings_file(), r#"{"decrypt_threads": 100000}"#).unwrap();
        let loaded = load_saved(base);
        assert_eq!(loaded.decrypt_threads, 256, "huge value must clamp to 256");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn decrypt_threads_small_value_preserved() {
        let d = scratch("decrypt_small");
        let base = cfg_in(&d);
        std::fs::write(base.settings_file(), r#"{"decrypt_threads": 8}"#).unwrap();
        let loaded = load_saved(base);
        assert_eq!(loaded.decrypt_threads, 8);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_settings_reverts_to_defaults_without_panicking() {
        let d = scratch("corrupt");
        let base = cfg_in(&d);
        // Partial write — invalid JSON. Must not panic and must keep defaults.
        std::fs::write(base.settings_file(), r#"{"max_retries": 5, "abort_on_l"#).unwrap();
        let loaded = load_saved(cfg_in(&d));
        assert_eq!(loaded.max_retries, Config::default().max_retries);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn missing_settings_file_uses_defaults() {
        let d = scratch("missing");
        // No settings.json written.
        let loaded = load_saved(cfg_in(&d));
        assert_eq!(loaded.max_retries, Config::default().max_retries);
        assert_eq!(
            loaded.abort_on_lost_secs,
            Config::default().abort_on_lost_secs
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_then_load_roundtrips_and_returns_ok() {
        let d = scratch("roundtrip");
        let mut base = cfg_in(&d);
        base.abort_on_lost_secs = 30;
        base.max_retries = 3;
        base.decrypt_threads = 4;
        save(&base).expect("save must succeed to a writable dir");
        let loaded = load_saved(cfg_in(&d));
        assert_eq!(loaded.abort_on_lost_secs, 30);
        assert_eq!(loaded.max_retries, 3);
        assert_eq!(loaded.decrypt_threads, 4);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_to_unwritable_dir_returns_err() {
        // settings_file() under a path whose parent does not exist -> open
        // of the .tmp fails -> Err, not a false success.
        let base = Config {
            autorip_dir: "/nonexistent-autorip-dir-xyz/sub".into(),
            ..Config::default()
        };
        assert!(save(&base).is_err());
    }

    /// Write a settings.json containing `json` under `dir`, then run it
    /// through `load_saved`. Mirrors `cfg_in` but seeds the file first.
    fn load_with(dir: &std::path::Path, json: &str) -> Config {
        std::fs::write(cfg_in(dir).settings_file(), json).unwrap();
        load_saved(cfg_in(dir))
    }

    #[test]
    fn library_fields_are_additive_and_tolerant() {
        let d = scratch("library-fields");
        let cfg = load_with(&d, r#"{"movie_dir": "Films"}"#);
        assert_eq!(cfg.library_dir, "");
        assert_eq!(cfg.library_iso_dir, "");
        assert!(
            !cfg.library_iso_subfolders,
            "top-level ISOs only by default"
        );
        let cfg = load_with(
            &d,
            r#"{"library_dir": "/lib", "library_iso_dir": 7, "library_iso_subfolders": "yes", "movie_dir": "Films"}"#,
        );
        assert_eq!(cfg.library_dir, "/lib");
        assert_eq!(cfg.library_iso_dir, "", "wrong type keeps the default");
        assert!(!cfg.library_iso_subfolders);
        assert_eq!(cfg.movie_dir, "Films");
        let cfg = load_with(&d, r#"{"library_iso_subfolders": true}"#);
        assert!(cfg.library_iso_subfolders);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_abort_on_error_false_migrates_to_skip() {
        let d = scratch("abort_false");
        // Pre-migration settings.json: only the legacy bool, no
        // on_read_error field. False must become the looser "skip",
        // not silently fall through to the default "stop".
        let cfg = load_with(&d, r#"{"abort_on_error": false}"#);
        assert_eq!(cfg.on_read_error, "skip");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn legacy_abort_on_error_true_migrates_to_stop() {
        let d = scratch("abort_true");
        let cfg = load_with(&d, r#"{"abort_on_error": true}"#);
        assert_eq!(cfg.on_read_error, "stop");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn explicit_on_read_error_wins_over_legacy_key() {
        let d = scratch("explicit_wins");
        // A migrated settings.json keeps the stale abort_on_error key
        // alongside the modern field; the explicit field must win so
        // re-loading doesn't flip the policy back.
        let cfg = load_with(&d, r#"{"on_read_error": "skip", "abort_on_error": true}"#);
        assert_eq!(cfg.on_read_error, "skip");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_legacy_key_uses_default() {
        let d = scratch("no_legacy");
        let cfg = load_with(&d, r#"{}"#);
        assert_eq!(cfg.on_read_error, "stop");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn on_insert_resume_setting_is_loaded() {
        let d = scratch("on_insert_resume");
        let cfg = load_with(&d, r#"{"on_insert":"resume"}"#);
        assert_eq!(cfg.on_insert, "resume");
        let _ = std::fs::remove_dir_all(&d);
    }

    // Parses a checked-in, hand-authored settings.json fixture through the
    // real `load_saved` and asserts every field independently — not a
    // serialize/deserialize self-roundtrip, so a dropped/mis-keyed field fails.
    #[test]
    fn real_settings_json_fixture_parses_field_by_field() {
        const FIXTURE: &str = include_str!("../../tests/server/fixtures/settings.json");
        let d = scratch("fixture");
        let cfg = load_with(&d, FIXTURE);

        assert_eq!(cfg.staging_dir, "/staging-local");
        assert_eq!(cfg.output_dir, "/alt-output");
        assert_eq!(cfg.movie_dir, "movies");
        assert_eq!(cfg.tv_dir, "tv");
        assert_eq!(cfg.min_length_secs, 900);
        assert!(!cfg.main_feature);
        assert!(!cfg.auto_eject);
        assert_eq!(cfg.on_insert, "rip");
        assert_eq!(cfg.output_format, "iso");
        assert_eq!(cfg.network_target, "nas.example.com:9000");
        assert_eq!(cfg.on_read_error, "skip");
        assert_eq!(cfg.max_retries, 3);
        assert!(cfg.keep_iso);
        assert_eq!(cfg.abort_on_lost_secs, 30);
        assert!(cfg.capture_without_keys);
        assert_eq!(cfg.max_rip_duration_secs, 14400);
        assert_eq!(cfg.min_pass_budget_secs, 1800);
        assert_eq!(cfg.transport_recovery_delay_secs, 10);
        assert_eq!(cfg.tmdb_api_key, "deadbeefcafef00ddeadbeefcafef00d");
        assert_eq!(
            cfg.keydb_path.as_deref(),
            Some("/root/.config/freemkv/keydb.cfg")
        );
        assert_eq!(
            cfg.keydb_url,
            "https://keydb.example.org/export/keydb_eng.zip"
        );
        assert_eq!(cfg.key_source, "online");
        assert_eq!(cfg.keyserver_url, "https://keys.example.org/decode");
        assert_eq!(cfg.keyserver_secret, "s3cr3t-token");
        assert_eq!(cfg.decrypt_threads, 4);
        assert_eq!(cfg.log_retention_days, 14);
        // The empty webhook URL in the fixture array must be filtered out.
        // The fixture stores bare legacy strings, which must load as
        // "fire on every stage" (pre-1.6.8 behaviour).
        assert_eq!(
            cfg.webhook_urls,
            vec![
                WebhookEntry {
                    url: "https://discord.com/api/webhooks/1/abc".to_string(),
                    post_rip: true,
                    post_mux: true,
                    post_move: true,
                },
                WebhookEntry {
                    url: "https://jellyfin.example.org/hook".to_string(),
                    post_rip: true,
                    post_mux: true,
                    post_move: true,
                },
            ],
            "empty webhook URLs must be dropped on load; bare strings fire on every stage"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // Exercises a mixed array: object form keeps its flags, bare string
    // fires every stage, a missing flag defaults true, and a blank/urlless
    // entry drops — the real shape settings.json may carry after upgrade.
    #[test]
    fn webhook_entries_load_mixed_string_and_object_forms() {
        let d = scratch("webhook-forms");
        let json = r#"{
            "webhook_urls": [
                "https://legacy.example/hook",
                {"url": "https://both.example/hook"},
                {"url": "https://rip-only.example/hook", "post_rip": true, "post_mux": false, "post_move": false},
                {"url": "https://move-only.example/hook", "post_rip": false, "post_mux": false, "post_move": true},
                {"url": "https://legacy-flags.example/hook", "post_rip": false, "post_move": true},
                {"url": "   "},
                {"post_rip": true},
                "  "
            ]
        }"#;
        std::fs::write(d.join("settings.json"), json).unwrap();
        let cfg = load_saved(cfg_in(&d));
        assert_eq!(
            cfg.webhook_urls,
            vec![
                WebhookEntry {
                    url: "https://legacy.example/hook".into(),
                    post_rip: true,
                    post_mux: true,
                    post_move: true,
                },
                WebhookEntry {
                    url: "https://both.example/hook".into(),
                    post_rip: true,
                    post_mux: true,
                    post_move: true,
                },
                WebhookEntry {
                    url: "https://rip-only.example/hook".into(),
                    post_rip: true,
                    post_mux: false,
                    post_move: false,
                },
                WebhookEntry {
                    url: "https://move-only.example/hook".into(),
                    post_rip: false,
                    post_mux: false,
                    post_move: true,
                },
                // A pre-1.6.8 object with no post_mux key: the mux stage
                // defaults ON so the upgrade never silently drops the
                // completion notification that used to ride post_rip.
                WebhookEntry {
                    url: "https://legacy-flags.example/hook".into(),
                    post_rip: false,
                    post_mux: true,
                    post_move: true,
                },
            ],
            "object flags load verbatim; bare string and missing flags default to fire-on-every-stage; blank/urlless entries drop"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // Per-event webhook flags must round-trip through `save`/`load_saved`
    // unchanged: a saved "move only" hook must still be "move only" on reload.
    #[test]
    fn webhook_entries_survive_save_load_round_trip() {
        let d = scratch("webhook-roundtrip");
        let mut cfg = cfg_in(&d);
        cfg.webhook_urls = vec![
            WebhookEntry {
                url: "https://both.example/hook".into(),
                post_rip: true,
                post_mux: true,
                post_move: true,
            },
            WebhookEntry {
                url: "https://move-only.example/hook".into(),
                post_rip: false,
                post_mux: false,
                post_move: true,
            },
        ];
        save(&cfg).expect("save");
        let reloaded = load_saved(cfg_in(&d));
        assert_eq!(reloaded.webhook_urls, cfg.webhook_urls);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Malformed / wrong-typed values at the settings.json trust boundary must
    /// each fall back to the field default WITHOUT wiping the rest of the file.
    /// Drives the real per-field type-gating + enum validation in `load_saved`.
    #[test]
    fn malformed_values_fall_back_to_defaults_field_independently() {
        let d = scratch("malformed");
        // Every field has a wrong type or an invalid enum value, EXCEPT
        // movie_dir which is well-formed — proving the bad fields don't wipe
        // the good one (independent gating).
        let json = r#"{
            "max_retries": "three",
            "abort_on_lost_secs": -5,
            "main_feature": "yes",
            "on_insert": "explode",
            "output_format": "garbage",
            "on_read_error": "panic",
            "key_source": "telepathy",
            "decrypt_threads": "lots",
            "movie_dir": "Films"
        }"#;
        let cfg = load_with(&d, json);
        let def = Config::default();

        // Wrong-typed / out-of-range numerics keep defaults.
        assert_eq!(
            cfg.max_retries, def.max_retries,
            "string max_retries → default"
        );
        assert_eq!(
            cfg.abort_on_lost_secs, def.abort_on_lost_secs,
            "negative abort_on_lost_secs (not as_u64) → default"
        );
        assert_eq!(cfg.main_feature, def.main_feature, "string bool → default");
        assert_eq!(
            cfg.decrypt_threads, def.decrypt_threads,
            "string usize → default"
        );
        // Invalid enum strings keep defaults (validated against allowed sets).
        assert_eq!(cfg.on_insert, def.on_insert, "unknown on_insert → default");
        assert_eq!(
            cfg.output_format, def.output_format,
            "unknown output_format → default"
        );
        assert_eq!(
            cfg.on_read_error, def.on_read_error,
            "unknown on_read_error → default"
        );
        assert_eq!(
            cfg.key_source, def.key_source,
            "unknown key_source → default"
        );
        // The one well-formed field still loads — bad neighbours didn't wipe it.
        assert_eq!(
            cfg.movie_dir, "Films",
            "a valid field survives bad neighbours"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A settings.json that fails to parse entirely (e.g. a partial write from
    /// a SIGKILL mid-save) must revert ALL persisted fields to defaults rather
    /// than panicking or loading garbage. Drives the real parse-failure branch.
    #[test]
    fn unparseable_settings_json_reverts_to_defaults() {
        let d = scratch("unparseable");
        let cfg = load_with(&d, "{ this is not valid json ");
        let def = Config::default();
        assert_eq!(cfg.max_retries, def.max_retries);
        assert_eq!(cfg.staging_dir, def.staging_dir);
        assert_eq!(cfg.output_format, def.output_format);
        let _ = std::fs::remove_dir_all(&d);
    }

    // Numeric knobs above their trust-boundary ceiling are clamped on load;
    // complements `load_saved_clamps_pathological_durations` by exercising
    // the retention + decrypt_threads ceilings via fixture-shaped JSON.
    #[test]
    fn over_ceiling_numeric_knobs_are_clamped_on_load() {
        let d = scratch("clamp_ceiling");
        let json = r#"{
            "log_retention_days": 999999,
            "decrypt_threads": 100000,
            "max_retries": 250
        }"#;
        let cfg = load_with(&d, json);
        assert_eq!(cfg.log_retention_days, 3650, "retention clamps to 10y");
        assert_eq!(cfg.decrypt_threads, 256, "decrypt_threads clamps to 256");
        assert_eq!(cfg.max_retries, 10, "max_retries clamps to 10");
        let _ = std::fs::remove_dir_all(&d);
    }

    // A realistic duration well under the 30-day ceiling must survive `load_saved` unclamped,
    // pinned to an absolute value (not the production literal).
    #[test]
    fn realistic_mid_range_duration_survives_unclamped() {
        let d = scratch("mid_range_duration");
        let path = cfg_in(&d).settings_file();
        std::fs::write(
            &path,
            serde_json::json!({
                "max_rip_duration_secs": 21_600u64, // 6h — realistic, not pathological
            })
            .to_string(),
        )
        .unwrap();
        let cfg = load_saved(cfg_in(&d));
        assert_eq!(
            cfg.max_rip_duration_secs, 21_600,
            "a realistic 6h duration must not be clamped"
        );
        // The shipped 8h UHD default itself must also survive a fresh load
        // (no settings.json override) — this is the value the whole ceiling
        // exists to NOT interfere with in normal operation.
        assert_eq!(Config::default().max_rip_duration_secs, 28_800);
        let _ = std::fs::remove_dir_all(&d);
    }

    // `Debug for Config` must mask every secret-bearing field. No existing
    // test called `format!("{:?}", cfg)`, so a future edit swapping
    // `redact(&self.x)` for `&self.x` would otherwise leak silently.
    #[test]
    fn debug_redacts_all_secret_fields() {
        let cfg = Config {
            tmdb_api_key: "tmdb-real-secret-abc123".into(),
            keydb_url: "https://keydb.example.org/export/keydb_eng.zip?token=KEYDB_SECRET".into(),
            keyserver_url: "https://keys.example.org/decode".into(),
            keyserver_secret: "keyserver-bearer-token-xyz789".into(),
            webhook_urls: vec![
                WebhookEntry {
                    url: "https://discord.com/api/webhooks/1/DISCORD_SECRET_TOKEN".into(),
                    post_rip: true,
                    post_mux: true,
                    post_move: true,
                },
                WebhookEntry {
                    url: "https://hooks.example.com?token=WEBHOOK_SECRET".into(),
                    post_rip: true,
                    post_mux: true,
                    post_move: false,
                },
            ],
            ..Config::default()
        };

        let debug_output = format!("{:?}", cfg);

        // None of the raw secrets may appear anywhere in the output.
        assert!(!debug_output.contains("tmdb-real-secret-abc123"));
        assert!(!debug_output.contains("KEYDB_SECRET"));
        assert!(!debug_output.contains("keyserver-bearer-token-xyz789"));
        assert!(!debug_output.contains("DISCORD_SECRET_TOKEN"));
        assert!(!debug_output.contains("WEBHOOK_SECRET"));
        // keyserver_url may carry a token, so it's always redacted too.

        // The redaction markers must be present too, proving the fields
        // were visited and masked rather than absent from a no-op Debug.
        assert!(debug_output.contains("<redacted>"));
        assert!(debug_output.contains("2 redacted")); // webhook_urls count
        // Non-secret fields must still print normally — Debug stays useful.
        assert!(debug_output.contains("Config"));
        assert!(debug_output.contains("port"));
    }

    // On an all-empty-secrets `Config::default()`, Debug must show "<unset>"
    // not "<redacted>" — proving `redact()` distinguishes "no secret" from
    // "secret present and hidden" rather than a fixed placeholder.
    #[test]
    fn debug_marks_empty_secrets_as_unset_not_redacted() {
        let cfg = Config::default();
        let debug_output = format!("{:?}", cfg);
        assert!(debug_output.contains("<unset>"));
    }

    // `should_relocate_bare_run_dir` — the common case (unmodified path,
    // real Docker mount present) must NOT relocate, or every normal
    // deployment would redirect rips into the container's ephemeral overlay.
    #[test]
    fn should_relocate_only_when_default_path_and_mount_absent() {
        // Default path, mount present (normal Docker deployment) -> do NOT relocate.
        assert!(!should_relocate_bare_run_dir("/staging", "/staging", true));
        // Default path, mount absent (bare-run binary, no container) -> relocate.
        assert!(should_relocate_bare_run_dir("/staging", "/staging", false));
        // Customized path -> never relocate, regardless of mount state.
        assert!(!should_relocate_bare_run_dir(
            "/mnt/media/staging",
            "/staging",
            true
        ));
        assert!(!should_relocate_bare_run_dir(
            "/mnt/media/staging",
            "/staging",
            false
        ));
        // Same shape for the output_dir case.
        assert!(!should_relocate_bare_run_dir("/output", "/output", true));
        assert!(should_relocate_bare_run_dir("/output", "/output", false));
    }

    /// `build_bootstrap_config` must carry the env-derived `port` and
    /// `autorip_dir` through into the returned `Config`, not silently fall
    /// back to `Config::default()`'s values via the struct-update `..`.
    #[test]
    fn build_bootstrap_config_carries_env_derived_fields() {
        let cfg = build_bootstrap_config(9999, "/custom/autorip/dir".to_string());
        assert_eq!(cfg.port, 9999);
        assert_eq!(cfg.autorip_dir, "/custom/autorip/dir");
        // Values not sourced from these two env vars still come from
        // Config::default() via the struct-update.
        assert_eq!(cfg.staging_dir, Config::default().staging_dir);
    }

    /// `parse_port_env` — the pure guard behind `load()`'s `PORT` handling.
    /// `"0"` is the reserved "ephemeral/unset" sentinel and must be
    /// rejected (falls back to 8080 with a warning), not silently bound.
    #[test]
    fn parse_port_env_rejects_zero_and_garbage_accepts_valid_port() {
        assert_eq!(parse_port_env("8081"), Some(8081));
        assert_eq!(parse_port_env("1"), Some(1));
        assert_eq!(parse_port_env("65535"), Some(65535));
        assert_eq!(parse_port_env("0"), None);
        assert_eq!(parse_port_env("not-a-number"), None);
        assert_eq!(parse_port_env(""), None);
        assert_eq!(parse_port_env("-1"), None);
        assert_eq!(parse_port_env("99999"), None); // out of u16 range
    }

    /// `dir_is_writable` against a real filesystem: an existing, writable
    /// directory must return true; a nonexistent directory (parent doesn't
    /// exist either) must return false rather than panicking.
    #[test]
    fn dir_is_writable_true_for_real_dir_false_for_missing() {
        let d = scratch("writable_probe");
        assert!(dir_is_writable(d.to_str().unwrap()));
        assert!(!dir_is_writable("/nonexistent-autorip-probe-dir-xyz/sub"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
