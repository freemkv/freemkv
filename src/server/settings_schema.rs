//! The one definition of every operator setting.
//!
//! [`FIELDS`] lists each setting's key, label, type, help, group and default.
//! The same table drives:
//! - [`Config::default`](crate::server::config::Config) (the defaults),
//! - loading `settings.json` ([`load_into`], every field tolerated on its own),
//! - `GET /api/settings` redaction ([`redacted`]) and `/api/settings/schema`,
//! - `POST /api/settings` validation ([`parse_patch`]) and application ([`apply`]),
//! - the Settings form, which the web UI builds from the schema JSON.
//!
//! Adding a setting means adding a `Config` field and one [`Field`] here.

use crate::server::config::{Config, WebhookEntry};
use serde_json::{Value, json};

/// Placeholder GET returns in place of a stored secret; POSTed back it means "unchanged".
pub const SECRET_SENTINEL: &str = "********";

/// Where a setting sits on the Settings page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum Group {
    Disc,
    Ripping,
    Recovery,
    Output,
    Library,
    Keys,
    Metadata,
    Notifications,
    Performance,
    Advanced,
    /// Loaded from settings.json and kept, but never shown and never set by a
    /// save: settings this server does not offer (it always rips the main movie).
    Hidden,
}

impl Group {
    pub const ALL: [Group; 10] = [
        Group::Disc,
        Group::Ripping,
        Group::Recovery,
        Group::Output,
        Group::Library,
        Group::Keys,
        Group::Metadata,
        Group::Notifications,
        Group::Performance,
        Group::Advanced,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Group::Disc => "Disc",
            Group::Ripping => "Ripping",
            Group::Recovery => "Recovery",
            Group::Output => "Output folders",
            Group::Library => "Library",
            Group::Keys => "Keys",
            Group::Metadata => "Metadata",
            Group::Notifications => "Notifications",
            Group::Performance => "Performance",
            Group::Advanced => "Advanced",
            Group::Hidden => "",
        }
    }
}

/// Which absolute-path rule a folder setting follows on save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRule {
    /// A mount root or library folder: absolute, no `..`. Empty = unset.
    Absolute,
    /// A folder under the output folder: relative or absolute, no `..`.
    Under,
}

/// Which outbound check a URL setting gets on save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UrlRule {
    /// Fetched by the daemon (keydb download): http(s) and a reachable address (LAN allowed).
    Fetch,
    /// The key service: the keysources crate's https + address rule.
    Keyserver,
}

/// One choice: its stored value, its label, and one line on what it does.
pub type Opt = (&'static str, &'static str, &'static str);

/// A setting's type, which fixes how it loads, validates, redacts and renders.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Text,
    Path(PathRule),
    /// Shown as the sentinel once set; the sentinel posted back is ignored.
    Secret,
    /// Shown with its path masked (tokens live there); a masked value is ignored.
    Url(UrlRule),
    /// A bare `host:port` the rip streams to, behind libfreemkv's address rule.
    Target,
    Bool,
    /// Clamped to `max` on load and on save.
    Number {
        max: u64,
    },
    /// One of `(value, label)`; anything else is refused on save, defaulted on load.
    Choice(&'static [Opt]),
    /// `Option<String>` shown as its file name only.
    KeydbPath,
    Webhooks,
    /// Derived, never stored: the rip mode shown over `max_retries`.
    RipMode,
    /// Read-only line the GET fills in (for example the resolved keydb path).
    Info,
    /// A button that POSTs to `endpoint`.
    Action {
        endpoint: &'static str,
        button: &'static str,
    },
}

impl Kind {
    fn type_name(self) -> &'static str {
        match self {
            Kind::Text | Kind::Target => "text",
            Kind::Path(_) => "path",
            Kind::Secret => "secret",
            Kind::Url(_) => "url",
            Kind::Bool => "bool",
            Kind::Number { .. } => "number",
            Kind::Choice(_) | Kind::RipMode => "choice",
            Kind::KeydbPath => "path",
            Kind::Webhooks => "webhooks",
            Kind::Info => "info",
            Kind::Action { .. } => "action",
        }
    }

    // Whether `v` has this kind's JSON type (its rules are checked separately).
    fn accepts(self, v: &Value) -> bool {
        match self {
            Kind::Bool => v.is_boolean(),
            Kind::Number { .. } => v.is_u64(),
            Kind::Webhooks => v.is_array(),
            Kind::Info | Kind::Action { .. } => false,
            _ => v.is_string(),
        }
    }

    // Stored in settings.json (false for derived and display-only kinds).
    fn stored(self) -> bool {
        !matches!(self, Kind::RipMode | Kind::Info | Kind::Action { .. })
    }
}

/// A field's default, typed the way it is stored.
#[derive(Clone, Copy, Debug)]
pub enum Def {
    Str(&'static str),
    Bool(bool),
    Num(u64),
    /// `null`: an unset optional (only `keydb_path`).
    Unset,
    /// An empty list (only `webhook_urls`).
    Empty,
    /// Not stored.
    None,
}

impl Def {
    fn value(self) -> Option<Value> {
        match self {
            Def::Str(s) => Some(json!(s)),
            Def::Bool(b) => Some(json!(b)),
            Def::Num(n) => Some(json!(n)),
            Def::Unset => Some(Value::Null),
            Def::Empty => Some(json!([])),
            Def::None => None,
        }
    }
}

/// Show a field only while another field has (or lacks) a value.
#[derive(Clone, Copy, Debug)]
pub struct When {
    pub key: &'static str,
    pub value: &'static str,
}

/// One operator setting.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub group: Group,
    pub help: &'static str,
    pub kind: Kind,
    pub default: Def,
    pub placeholder: &'static str,
    pub show_if: Option<When>,
    pub hide_if: Option<When>,
}

const fn field(
    key: &'static str,
    label: &'static str,
    group: Group,
    kind: Kind,
    default: Def,
    help: &'static str,
) -> Field {
    Field {
        key,
        label,
        group,
        help,
        kind,
        default,
        placeholder: "",
        show_if: None,
        hide_if: None,
    }
}

impl Field {
    const fn placeholder(mut self, p: &'static str) -> Self {
        self.placeholder = p;
        self
    }
    const fn show_if(mut self, key: &'static str, value: &'static str) -> Self {
        self.show_if = Some(When { key, value });
        self
    }
    const fn hide_if(mut self, key: &'static str, value: &'static str) -> Self {
        self.hide_if = Some(When { key, value });
        self
    }
}

const MAX_DURATION_SECS: u64 = 30 * 24 * 3600;
const MAX_RETENTION_DAYS: u64 = 3650;

use Def::{Bool as B, Num as N, Str as S};
use Group as G;

const ON_INSERT: &[Opt] = &[
    ("nothing", "Do nothing", "Wait until you press Rip."),
    ("scan", "Scan", "Read the disc's titles and wait for you."),
    ("rip", "Rip", "Always start a fresh rip."),
    (
        "resume",
        "Resume",
        "Carry on this disc's unfinished rip; otherwise start a fresh one.",
    ),
];
const OUTPUT_FORMAT: &[Opt] = &[
    (
        "mkv",
        "MKV",
        "One video file per title, playable almost anywhere.",
    ),
    ("m2ts", "M2TS", "The disc's own video format, unchanged."),
    ("iso", "ISO (disc copy)", "An exact copy of the whole disc."),
    (
        "network",
        "Network",
        "Send the video to another machine instead of saving a file.",
    ),
];
const ON_READ_ERROR: &[Opt] = &[
    (
        "stop",
        "Stop",
        "Stop the rip at the first spot that can't be read.",
    ),
    ("skip", "Skip", "Leave that spot blank and keep going."),
];
const RIP_MODE: &[Opt] = &[
    (
        "single",
        "Single pass",
        "Read the disc once, straight to the file. Fastest; best for discs in good shape.",
    ),
    (
        "multi",
        "Multi pass",
        "Copy the disc, go back over the parts that didn't read, then make the file. For scratched discs.",
    ),
];
const KEY_SOURCE: &[Opt] = &[
    ("local", "Keys file", "Use the keys file on this server."),
    ("online", "Online key service", "Ask an online key service."),
];

/// Every setting, in the order the Settings page shows them.
pub static FIELDS: &[Field] = &[
    field("on_insert", "When a disc goes in", G::Disc, Kind::Choice(ON_INSERT), S("scan"),
        ""),
    field("auto_eject", "Eject when done", G::Disc, Kind::Bool, B(true),
        "Eject the disc when its rip is done."),
    field("output_format", "What to make", G::Ripping, Kind::Choice(OUTPUT_FORMAT), S("mkv"),
        "What each rip produces. The server rips the main movie; for extras or other titles, use the freemkv app or command line."),
    field("network_target", "Network address", G::Ripping, Kind::Target, S(""),
        "The other machine's address and port, for example nas.example.com:9000.")
        .placeholder("nas.example.com:9000").show_if("output_format", "network"),
    field("main_feature", "Main movie only", G::Hidden, Kind::Bool, B(true),
        "Rip only the movie itself, not the extras.").hide_if("output_format", "iso"),
    field("min_length_secs", "Shortest title to rip (seconds)", G::Hidden, Kind::Number { max: MAX_DURATION_SECS }, N(600),
        "Skip titles shorter than this (600 = 10 minutes).").hide_if("output_format", "iso"),
    field("tv_auto", "Rip TV seasons automatically", G::Ripping, Kind::Bool, B(true),
        "Rip every episode of a TV disc and name them from TMDB. Off: TV discs wait for you on the Drives page."),
    field("rip_mode", "Reading", G::Recovery, Kind::RipMode, Def::None,
        ""),
    field("on_read_error", "When part of the disc can't be read", G::Recovery, Kind::Choice(ON_READ_ERROR), S("stop"),
        "")
        .show_if("rip_mode", "single"),
    field("max_retries", "Retries", G::Recovery, Kind::Number { max: 10 }, N(1),
        "How many times to go back over the parts that didn't read. 5 recovers most damage.")
        .show_if("rip_mode", "multi"),
    field("abort_on_lost_secs", "Missing movie time allowed (seconds)", G::Recovery, Kind::Number { max: MAX_DURATION_SECS }, N(0),
        "If more of the movie than this still can't be read after every retry, the rip stops so you can decide. 0 = the movie must be complete.")
        .show_if("rip_mode", "multi").hide_if("output_format", "iso"),
    field("keep_iso", "Keep a copy of the whole disc", G::Recovery, Kind::Bool, B(false),
        "Also keep the disc copy (an ISO) beside the finished file.")
        .show_if("rip_mode", "multi"),
    field("staging_dir", "Working folder", G::Output, Kind::Path(PathRule::Absolute), S("/staging"),
        "Where rips are made before they move to your library. A fast local disk is best."),
    field("output_dir", "Output folder", G::Output, Kind::Path(PathRule::Absolute), S("/output"),
        "Where finished rips are saved."),
    field("movie_dir", "Movies", G::Output, Kind::Path(PathRule::Under), S(""),
        "Folder for movies, inside the output folder. Blank = the output folder.").placeholder("Same as the output folder"),
    field("tv_dir", "TV shows", G::Output, Kind::Path(PathRule::Under), S(""),
        "Folder for TV shows, inside the output folder. Blank = the output folder.").placeholder("Same as the output folder"),
    field("iso_dir", "Disc copies", G::Output, Kind::Path(PathRule::Under), S(""),
        "Folder for kept disc copies (ISOs). Blank = next to each movie.")
        .placeholder("Beside the title"),
    field("library_dir", "Library folder", G::Library, Kind::Path(PathRule::Absolute), S(""),
        "The folder with your movie files. Blank = the Movies folder.").placeholder("The Movies folder"),
    field("library_iso_dir", "Disc copy folder", G::Library, Kind::Path(PathRule::Absolute), S(""),
        "The folder with your disc copies (ISOs). Blank = the Disc copies folder.").placeholder("The ISO folder"),
    field("library_iso_subfolders", "Look in sub-folders too", G::Library, Kind::Bool, B(false),
        "Also find disc copies one folder down, for example dvd/ or bd/."),
    field("deep_audit", "Deep audit", G::Library, Kind::Bool, B(false),
        "Play every movie file through in full, in the background, to find damage the quick audit can't see. Slow: one file at a time, paused while ripping or remuxing."),
    field("key_source", "Keys come from", G::Keys, Kind::Choice(KEY_SOURCE), S("local"),
        "Blu-ray discs need keys to unlock them."),
    field("keydb_path", "Keys file location", G::Keys, Kind::KeydbPath, Def::Unset,
        "Blank = keydb.cfg in the config folder.").placeholder("/config/keydb.cfg").show_if("key_source", "local"),
    field("keydb_resolved", "Keys file in use", G::Keys, Kind::Info, Def::None,
        "The keys file the server reads right now.").show_if("key_source", "local"),
    field("keydb_url", "Keys file download address", G::Keys, Kind::Url(UrlRule::Fetch), S(""),
        "Where to download a fresh keys file from.").show_if("key_source", "local"),
    field("update_keydb", "", G::Keys, Kind::Action { endpoint: "/api/update-keydb", button: "Download keys now" }, Def::None,
        "Download the keys file from the address above now.").show_if("key_source", "local"),
    field("keyserver_url", "Key service address", G::Keys, Kind::Url(UrlRule::Keyserver), S(""),
        "The key service's full address.").show_if("key_source", "online"),
    field("keyserver_secret", "Key service password", G::Keys, Kind::Secret, S(""),
        "The password or token, if the key service needs one.").show_if("key_source", "online"),
    field("test_keyserver", "", G::Keys, Kind::Action { endpoint: "/api/system/keyserver-test", button: "Test the keyserver" }, Def::None,
        "Check that the key service answers.").show_if("key_source", "online"),
    field("capture_without_keys", "Copy discs without keys", G::Keys, Kind::Bool, B(false),
        "When no keys are found, copy the disc now and finish it when keys arrive. Off: the disc is skipped."),
    field("tmdb_api_key", "TMDB API key", G::Metadata, Kind::Secret, S(""),
        "Lets the server look up movie names, years and posters on themoviedb.org."),
    field("webhook_urls", "Webhooks", G::Notifications, Kind::Webhooks, Def::Empty,
        "Tell other apps (Discord, Jellyfin, …) when a disc is read, a file is made, or it reaches your library."),
    field("decrypt_threads", "CPU cores for unlocking", G::Performance, Kind::Number { max: 256 }, N(0),
        "How many CPU cores unlock discs. 0 = all of them."),
    field("log_retention_days", "Keep logs for (days)", G::Performance, Kind::Number { max: MAX_RETENTION_DAYS }, N(30),
        "Drive logs older than this are deleted."),
    field("max_rip_duration_secs", "Rip time limit (seconds)", G::Hidden, Kind::Number { max: MAX_DURATION_SECS }, N(28_800),
        "Longest a rip may run across every pass."),
    field("min_pass_budget_secs", "Minimum pass budget (seconds)", G::Hidden, Kind::Number { max: MAX_DURATION_SECS }, N(5_400),
        "Per-pass time budget when the disc runtime is unknown."),
    field("remux_staged_max_age_days", "Kept remux age limit (days)", G::Hidden, Kind::Number { max: MAX_RETENTION_DAYS }, N(0),
        "Days a finished remux kept on local staging waits for the output folder. 0 = 7 days."),
    field("remux_staged_max_gb", "Kept remux space limit (GB)", G::Hidden, Kind::Number { max: 1_000_000 }, N(0),
        "Space the kept remuxes may take on local staging. 0 = a quarter of that disk, at most 500 GB."),
    field("transport_recovery_delay_secs", "USB drive reconnect wait (seconds)", G::Hidden, Kind::Number { max: MAX_DURATION_SECS }, N(5),
        "How long to wait for a USB drive to come back after it drops out."),
];

/// The field named `key`.
pub fn get(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.key == key)
}

/// The stored defaults as a JSON object; `Config::default` deserializes it.
pub fn defaults() -> serde_json::Map<String, Value> {
    FIELDS
        .iter()
        .filter(|f| f.kind.stored())
        .filter_map(|f| Some((f.key.to_string(), f.default.value()?)))
        .collect()
}

/// Whether a value is checked for loading an old file or for a save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `settings.json`: a bad value keeps the default; paths are not policed.
    Load,
    /// `POST /api/settings`: a bad value refuses the whole save.
    Save,
}

fn has_parent_dir(p: &std::path::Path) -> bool {
    p.components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

fn keydb_redacted_roundtrip(v: &str, current: &Config) -> bool {
    !v.is_empty()
        && !v.contains('/')
        && current.keydb_path.as_deref().is_some_and(|stored| {
            std::path::Path::new(stored)
                .file_name()
                .is_some_and(|n| n == std::ffi::OsStr::new(v))
        })
}

impl Field {
    /// Check `v` for this field. `Ok(None)` means "leave the stored value".
    pub fn parse(&self, v: &Value, mode: Mode, current: &Config) -> Result<Option<Value>, String> {
        let key = self.key;
        let text = || v.as_str().ok_or_else(|| format!("{key}: expected text"));
        match self.kind {
            Kind::Text => Ok(Some(json!(text()?))),
            Kind::Path(rule) => {
                let s = text()?;
                if mode == Mode::Save && !s.is_empty() {
                    let p = std::path::Path::new(s);
                    match rule {
                        PathRule::Absolute if !p.is_absolute() || has_parent_dir(p) => {
                            return Err(format!("{key} must be an absolute path with no '..'"));
                        }
                        PathRule::Under if has_parent_dir(p) => {
                            return Err(format!("{key} must not contain '..'"));
                        }
                        _ => {}
                    }
                }
                Ok(Some(json!(s)))
            }
            Kind::Secret => {
                let s = text()?;
                Ok((s != SECRET_SENTINEL).then(|| json!(s)))
            }
            Kind::Url(rule) => {
                let s = text()?;
                if mode == Mode::Load {
                    return Ok(Some(json!(s)));
                }
                if s.contains(SECRET_SENTINEL) {
                    return Ok(None);
                }
                let s = s.trim();
                if !s.is_empty() {
                    let checked = match rule {
                        UrlRule::Fetch => crate::server::web::validate_fetch_url(s).map(|_| ()),
                        UrlRule::Keyserver => freemkv_keysources::validate_keyserver_url(s),
                    };
                    checked.map_err(|e| format!("{key} rejected: {e}"))?;
                }
                Ok(Some(json!(s)))
            }
            Kind::Target => {
                let s = text()?;
                if mode == Mode::Save && !s.trim().is_empty() {
                    crate::server::web::validate_network_target(s)
                        .map_err(|e| format!("{key} rejected: {e}"))?;
                }
                Ok(Some(json!(s)))
            }
            Kind::Bool => v
                .as_bool()
                .map(|b| Some(json!(b)))
                .ok_or_else(|| format!("{key}: expected true or false")),
            Kind::Number { max } => v
                .as_u64()
                .map(|n| Some(json!(n.min(max))))
                .ok_or_else(|| format!("{key}: expected a whole number")),
            Kind::Choice(options) => match v.as_str() {
                Some(s) if options.iter().any(|(o, _, _)| *o == s) => Ok(Some(json!(s))),
                _ => Err(format!("invalid value for {key}")),
            },
            Kind::RipMode => match v.as_str() {
                Some(s @ ("single" | "multi")) => Ok(Some(json!(s))),
                _ => Err(format!("invalid value for {key}")),
            },
            Kind::KeydbPath => {
                let s = text()?;
                if mode == Mode::Load {
                    return Ok(Some(json!(s)));
                }
                if keydb_redacted_roundtrip(s, current) {
                    return Ok(None);
                }
                if s.is_empty() {
                    return Ok(Some(Value::Null));
                }
                let p = std::path::Path::new(s);
                if !p.is_absolute()
                    || has_parent_dir(p)
                    || p.extension().and_then(|e| e.to_str()) != Some("cfg")
                {
                    return Err("keydb_path must be an absolute .cfg path with no '..'".into());
                }
                Ok(Some(json!(s)))
            }
            Kind::Webhooks => {
                let arr = v
                    .as_array()
                    .ok_or_else(|| format!("{key}: expected a list"))?;
                let entries: Vec<WebhookEntry> = match mode {
                    Mode::Load => arr
                        .iter()
                        .enumerate()
                        .filter_map(|(i, v)| WebhookEntry::from_json(i, v))
                        .collect(),
                    Mode::Save => {
                        let incoming = arr
                            .iter()
                            .enumerate()
                            .map(|(i, v)| WebhookEntry::parse(i, v))
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|f| format!("{f}: flags must be booleans and url a string"))?;
                        crate::server::web::resolve_webhook_entries(
                            &incoming,
                            &current.webhook_urls,
                        )
                        .map_err(|_| {
                            "ambiguous masked webhook entry; re-enter the full webhook URL"
                                .to_string()
                        })?
                    }
                };
                Ok(Some(
                    serde_json::to_value(entries).unwrap_or_else(|_| json!([])),
                ))
            }
            Kind::Info | Kind::Action { .. } => Ok(None),
        }
    }
}

/// Overlay `saved` (a parsed `settings.json`) onto `cfg`, one field at a time:
/// a missing, mistyped or out-of-vocabulary value keeps what `cfg` had.
pub fn load_into(cfg: &mut Config, saved: &Value) {
    let Ok(mut v) = serde_json::to_value(&*cfg) else {
        return;
    };
    let current = cfg.clone();
    for f in FIELDS.iter().filter(|f| f.kind.stored()) {
        let Some(raw) = saved.get(f.key).filter(|x| !x.is_null()) else {
            continue;
        };
        match f.parse(raw, Mode::Load, &current) {
            Ok(Some(x)) => v[f.key] = x,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(field = f.key, error = %e, "settings.json value ignored - using default")
            }
        }
    }
    // An old file's `abort_on_error` stands in for a missing `on_read_error`.
    if saved.get("on_read_error").and_then(Value::as_str).is_none()
        && let Some(stop) = saved.get("abort_on_error").and_then(Value::as_bool)
    {
        v["on_read_error"] = json!(if stop { "stop" } else { "skip" });
    }
    match serde_json::from_value::<Config>(v) {
        Ok(mut next) => {
            next.port = cfg.port;
            *cfg = next;
        }
        Err(e) => tracing::warn!(error = %e, "settings.json could not be applied - using defaults"),
    }
}

/// A validated `POST /api/settings` body, ready for [`apply`].
#[derive(Debug, Default)]
pub struct Patch {
    values: Vec<(&'static str, Value)>,
    port: Option<u16>,
    legacy_on_read_error: Option<&'static str>,
}

/// Validate every known key of a POST body against `current`. Any bad value
/// refuses the whole patch, so nothing lands half-applied. This runs the URL
/// checks (DNS), so call it before taking the config write lock.
pub fn parse_patch(body: &Value, current: &Config) -> Result<Patch, String> {
    let mut patch = Patch::default();
    let effective = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                serde_json::to_value(current)
                    .ok()?
                    .get(key)?
                    .as_str()
                    .map(str::to_owned)
            })
    };
    for f in FIELDS.iter().filter(|f| f.group != Group::Hidden) {
        let Some(raw) = body.get(f.key).filter(|x| !x.is_null()) else {
            continue;
        };
        // A value of the wrong type is skipped and the rest saved, as 1.7.7
        // did; only a value of the right type that breaks a rule refuses the save.
        if !f.kind.accepts(raw) {
            tracing::debug!(field = f.key, "settings POST: wrong-typed value ignored");
            continue;
        }
        // A stream target that is not in use (an old one kept from autorip) is
        // stored as it is, not checked, so it cannot block every other save.
        let inactive = matches!(f.kind, Kind::Target)
            && f.show_if
                .is_some_and(|w| effective(w.key).as_deref() != Some(w.value));
        let mode = if inactive { Mode::Load } else { Mode::Save };
        if let Some(v) = f.parse(raw, mode, current)? {
            patch.values.push((f.key, v));
        }
    }
    // Bootstrap-only and never stored, but a running server still takes it.
    if let Some(p) = body.get("port").and_then(Value::as_u64) {
        patch.port = Some(
            u16::try_from(p)
                .ok()
                .filter(|p| *p != 0)
                .ok_or("port must be 1..=65535")?,
        );
    }
    if body.get("on_read_error").and_then(Value::as_str).is_none() {
        patch.legacy_on_read_error = match body.get("abort_on_error").and_then(Value::as_bool) {
            Some(true) => Some("stop"),
            Some(false) => Some("skip"),
            None => None,
        };
    }
    Ok(patch)
}

/// Write a validated patch into `cfg`. Infallible: [`parse_patch`] checked it.
pub fn apply(cfg: &mut Config, patch: &Patch) {
    let Ok(mut v) = serde_json::to_value(&*cfg) else {
        return;
    };
    let mut rip_mode = None;
    for (key, value) in &patch.values {
        if *key == "rip_mode" {
            rip_mode = value.as_str().map(str::to_owned);
        } else {
            v[*key] = value.clone();
        }
    }
    if let Some(mode) = patch.legacy_on_read_error {
        v["on_read_error"] = json!(mode);
    }
    let port = patch.port.unwrap_or(cfg.port);
    if let Ok(mut next) = serde_json::from_value::<Config>(v) {
        next.port = port;
        match rip_mode.as_deref() {
            Some("single") => next.max_retries = 0,
            Some(_) if next.max_retries == 0 => next.max_retries = 1,
            _ => {}
        }
        *cfg = next;
    }
}

/// `GET /api/settings`: the stored settings with secrets masked, plus the
/// derived `rip_mode` and the resolved KEYDB line.
pub fn redacted(c: &Config) -> Value {
    let mut v = serde_json::to_value(c).unwrap_or_else(|_| json!({}));
    for f in FIELDS {
        let cur = v.get(f.key).cloned().unwrap_or(Value::Null);
        let shown = match f.kind {
            Kind::Secret => match cur.as_str() {
                Some(s) if !s.is_empty() => json!(SECRET_SENTINEL),
                _ => cur,
            },
            Kind::Url(_) => match cur.as_str() {
                Some(s) if !s.is_empty() => json!(crate::server::web::mask_webhook_url(s)),
                _ => cur,
            },
            Kind::KeydbPath => match cur.as_str() {
                Some(s) if !s.is_empty() => json!(
                    std::path::Path::new(s)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ),
                _ => cur,
            },
            Kind::Webhooks => {
                let mut list = cur;
                if let Some(arr) = list.as_array_mut() {
                    for (i, entry) in arr.iter_mut().enumerate() {
                        if let Some(headers) =
                            entry.get_mut("headers").and_then(Value::as_object_mut)
                        {
                            for value in headers.values_mut() {
                                *value = json!(SECRET_SENTINEL);
                            }
                        }
                        if let Some(u) = entry.get("url").and_then(Value::as_str)
                            && !u.is_empty()
                        {
                            entry["url"] =
                                json!(crate::server::web::mask_webhook_url_indexed(u, i));
                        }
                    }
                }
                list
            }
            Kind::RipMode => json!(if c.max_retries > 0 { "multi" } else { "single" }),
            Kind::Info if f.key == "keydb_resolved" => {
                let p = crate::server::keysource::keydb_path(c);
                json!(format!(
                    "{}  —  {}",
                    p.display(),
                    if p.exists() {
                        "file present"
                    } else {
                        "NOT FOUND (discs that need it will report no key)"
                    }
                ))
            }
            _ => continue,
        };
        v[f.key] = shown;
    }
    v
}

fn options_json(opts: &[Opt]) -> Value {
    opts.iter()
        .map(|(v, l, h)| json!({"value": v, "label": l, "help": h}))
        .collect()
}

/// `GET /api/settings/schema`: the groups and fields the Settings form renders.
pub fn schema_json() -> Value {
    let fields: Vec<Value> = FIELDS
        .iter()
        .filter(|f| f.group != Group::Hidden)
        .map(|f| {
            let mut o = json!({
                "key": f.key,
                "label": f.label,
                "group": f.group,
                "help": f.help,
                "type": f.kind.type_name(),
                "placeholder": f.placeholder,
                "default": f.default.value(),
                "show_if": f.show_if.map(|w| json!({"key": w.key, "value": w.value})),
                "hide_if": f.hide_if.map(|w| json!({"key": w.key, "value": w.value})),
            });
            match f.kind {
                Kind::Choice(opts) => o["options"] = options_json(opts),
                Kind::RipMode => o["options"] = options_json(RIP_MODE),
                Kind::Number { max } => o["max"] = json!(max),
                Kind::Action { endpoint, button } => {
                    o["endpoint"] = json!(endpoint);
                    o["button"] = json!(button);
                }
                _ => {}
            }
            o
        })
        .collect();
    let groups: Vec<Value> = Group::ALL
        .iter()
        .map(|g| json!({"id": g, "title": g.title()}))
        .collect();
    json!({ "groups": groups, "fields": fields })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_config_field_but_the_bootstrap_ones_is_in_the_schema() {
        let v = serde_json::to_value(Config::default()).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        for k in &keys {
            if *k == "autorip_dir" {
                continue;
            }
            assert!(get(k).is_some(), "Config::{k} has no schema field");
        }
        for f in FIELDS.iter().filter(|f| f.kind.stored()) {
            assert!(
                keys.contains(&f.key),
                "schema field {} is not a Config field",
                f.key
            );
        }
    }

    #[test]
    fn every_choice_says_what_each_option_does() {
        for f in FIELDS {
            let opts: &[Opt] = match f.kind {
                Kind::Choice(o) => o,
                Kind::RipMode => RIP_MODE,
                _ => continue,
            };
            for (v, l, h) in opts {
                assert!(
                    !l.is_empty() && !h.is_empty(),
                    "{}={v} needs a label and a line",
                    f.key
                );
            }
        }
        let s = schema_json();
        let insert = s["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "on_insert")
            .unwrap();
        assert_eq!(insert["options"][2]["help"], "Always start a fresh rip.");
    }

    #[test]
    fn keys_are_unique_and_labelled() {
        for (i, f) in FIELDS.iter().enumerate() {
            assert!(
                FIELDS[..i].iter().all(|g| g.key != f.key),
                "duplicate {}",
                f.key
            );
            let per_option = matches!(f.kind, Kind::Choice(_) | Kind::RipMode);
            assert!(per_option || !f.help.is_empty(), "{} has no help", f.key);
            if !matches!(f.kind, Kind::Action { .. }) {
                assert!(!f.label.is_empty(), "{} has no label", f.key);
            }
        }
    }

    #[test]
    fn conditions_name_real_fields_and_values() {
        for f in FIELDS {
            for w in f.show_if.iter().chain(f.hide_if.iter()) {
                let other = get(w.key).unwrap_or_else(|| panic!("{} depends on {}", f.key, w.key));
                let values: &[Opt] = match other.kind {
                    Kind::Choice(o) => o,
                    Kind::RipMode => RIP_MODE,
                    _ => panic!("{} depends on a non-choice {}", f.key, w.key),
                };
                assert!(
                    values.iter().any(|(v, _, _)| *v == w.value),
                    "{}: {} has no {}",
                    f.key,
                    w.key,
                    w.value
                );
            }
        }
    }

    #[test]
    fn defaults_are_the_documented_first_boot_values() {
        let c = Config::default();
        assert_eq!(c.staging_dir, "/staging");
        assert_eq!(c.output_dir, "/output");
        assert_eq!(c.on_insert, "scan");
        assert_eq!(c.max_retries, 1);
        assert_eq!(c.max_rip_duration_secs, 28_800);
        assert_eq!(c.min_pass_budget_secs, 5_400);
        assert_eq!(c.log_retention_days, 30);
        assert!(c.main_feature && c.tv_auto && c.auto_eject);
        assert_eq!(c.keydb_path, None);
        assert_eq!(c.port, 8080);
        assert_eq!(c.autorip_dir, "/config");
    }

    #[test]
    fn an_unused_network_target_does_not_block_a_save() {
        // A target validation refuses (unspecified address).
        let lan = "0.0.0.0:9000".to_string();
        let c = Config {
            output_format: "mkv".into(),
            network_target: lan.clone(),
            ..Config::default()
        };
        let body = json!({"network_target": lan, "auto_eject": false});
        assert!(
            parse_patch(&body, &c).is_ok(),
            "mkv output: the target is not checked"
        );
        let body = json!({"network_target": lan, "output_format": "network"});
        assert!(
            parse_patch(&body, &c)
                .unwrap_err()
                .contains("network_target")
        );
    }

    #[test]
    fn dead_settings_load_but_are_not_on_the_form() {
        let s = schema_json();
        let keys: Vec<&str> = s["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["key"].as_str().unwrap())
            .collect();
        assert!(!keys.contains(&"max_rip_duration_secs"));
        assert!(!keys.contains(&"min_pass_budget_secs"));
        assert!(
            !s["groups"]
                .as_array()
                .unwrap()
                .iter()
                .any(|g| g["id"] == "Hidden")
        );
        let mut c = Config::default();
        load_into(&mut c, &json!({"max_rip_duration_secs": 100}));
        assert_eq!(c.max_rip_duration_secs, 100);
    }

    #[test]
    fn a_broken_rule_refuses_the_save_but_a_wrong_type_is_skipped() {
        let mut c = Config::default();
        let body = json!({"auto_eject": false, "output_format": "garbage"});
        assert_eq!(
            parse_patch(&body, &c).unwrap_err(),
            "invalid value for output_format"
        );
        // 1.7.7 skipped a mistyped field and saved the rest.
        let body = json!({"max_retries": "three", "auto_eject": false, "decrypt_threads": -5});
        let p = parse_patch(&body, &c).unwrap();
        apply(&mut c, &p);
        assert!(!c.auto_eject);
        assert_eq!(c.max_retries, Config::default().max_retries);
        assert_eq!(c.decrypt_threads, Config::default().decrypt_threads);
    }

    #[test]
    fn apply_keeps_the_port_and_maps_rip_mode() {
        let mut c = Config {
            port: 9123,
            max_retries: 3,
            ..Config::default()
        };
        let p = parse_patch(&json!({"rip_mode": "single", "auto_eject": false}), &c).unwrap();
        apply(&mut c, &p);
        assert_eq!((c.port, c.max_retries, c.auto_eject), (9123, 0, false));
        let p = parse_patch(&json!({"rip_mode": "multi"}), &c).unwrap();
        apply(&mut c, &p);
        assert_eq!(c.max_retries, 1);
    }

    #[test]
    fn redaction_masks_every_secret_kind() {
        let c = Config {
            tmdb_api_key: "k".into(),
            keyserver_secret: "s".into(),
            keyserver_url: "https://h.example/tok/decode".into(),
            keydb_path: Some("/secret/place/keydb.cfg".into()),
            ..Config::default()
        };
        let v = redacted(&c);
        assert_eq!(v["tmdb_api_key"], SECRET_SENTINEL);
        assert_eq!(v["keyserver_secret"], SECRET_SENTINEL);
        assert_eq!(
            v["keyserver_url"],
            format!("https://h.example/{SECRET_SENTINEL}")
        );
        assert_eq!(v["keydb_path"], "keydb.cfg");
        assert_eq!(v["rip_mode"], "multi");
        assert!(v["keydb_resolved"].as_str().unwrap().contains("keydb.cfg"));
    }

    #[test]
    fn the_schema_json_carries_what_the_form_needs() {
        let s = schema_json();
        let f = s["fields"].as_array().unwrap();
        let fmt = f.iter().find(|f| f["key"] == "output_format").unwrap();
        assert_eq!(fmt["type"], "choice");
        assert_eq!(fmt["options"].as_array().unwrap().len(), 4);
        assert_eq!(fmt["default"], "mkv");
        let mode = f.iter().find(|f| f["key"] == "rip_mode").unwrap();
        assert_eq!(mode["options"][1]["value"], "multi");
        assert_eq!(s["groups"].as_array().unwrap().len(), Group::ALL.len());
    }

    #[test]
    fn an_untouched_form_keeps_the_masked_values() {
        let mut c = Config {
            tmdb_api_key: "k".into(),
            keyserver_secret: "s".into(),
            keyserver_url: "https://h.example/tok/decode".into(),
            keydb_path: Some("/secret/place/keydb.cfg".into()),
            ..Config::default()
        };
        let before = c.clone();
        let body = redacted(&c);
        let p = parse_patch(&body, &c).unwrap();
        apply(&mut c, &p);
        assert_eq!(c.tmdb_api_key, before.tmdb_api_key);
        assert_eq!(c.keyserver_secret, before.keyserver_secret);
        assert_eq!(c.keyserver_url, before.keyserver_url);
        assert_eq!(c.keydb_path, before.keydb_path);
    }

    #[test]
    fn save_time_path_rules_refuse_bad_paths() {
        let c = Config::default();
        for body in [
            json!({"staging_dir": "../../etc"}),
            json!({"output_dir": "relative"}),
            json!({"output_dir": "/a/../b"}),
            json!({"movie_dir": "../escape"}),
            json!({"keydb_path": "relative/keydb.cfg"}),
            json!({"keydb_path": "/a/../keydb.cfg"}),
            json!({"keydb_path": "/a/keydb.txt"}),
        ] {
            assert!(parse_patch(&body, &c).is_err(), "{body} must be refused");
        }
        assert!(
            parse_patch(
                &json!({"staging_dir": "/ok/stage", "keydb_path": "/k/keydb.cfg"}),
                &c
            )
            .is_ok()
        );
    }

    #[test]
    fn the_legacy_abort_flag_maps_to_on_read_error() {
        let mut c = Config::default();
        let p = parse_patch(&json!({"abort_on_error": true}), &c).unwrap();
        apply(&mut c, &p);
        assert_eq!(c.on_read_error, "stop");
        let p = parse_patch(&json!({"abort_on_error": false}), &c).unwrap();
        apply(&mut c, &p);
        assert_eq!(c.on_read_error, "skip");
        // An explicit on_read_error wins over the old flag.
        let p = parse_patch(
            &json!({"abort_on_error": true, "on_read_error": "skip"}),
            &c,
        )
        .unwrap();
        apply(&mut c, &p);
        assert_eq!(c.on_read_error, "skip");

        let mut c = Config::default();
        load_into(&mut c, &json!({"abort_on_error": true}));
        assert_eq!(c.on_read_error, "stop");
        load_into(&mut c, &json!({"abort_on_error": false}));
        assert_eq!(c.on_read_error, "skip");
        load_into(
            &mut c,
            &json!({"abort_on_error": true, "on_read_error": "skip"}),
        );
        assert_eq!(c.on_read_error, "skip");
    }

    #[test]
    fn numbers_clamp_ports_validate_and_bad_loads_keep_the_default() {
        let mut c = Config::default();
        let p = parse_patch(&json!({"log_retention_days": 999_999_999u64}), &c).unwrap();
        apply(&mut c, &p);
        assert_eq!(c.log_retention_days, MAX_RETENTION_DAYS);

        for port in [0u64, 65_536, 70_000] {
            assert!(
                parse_patch(&json!({"port": port}), &c).is_err(),
                "port {port}"
            );
        }
        let p = parse_patch(&json!({"port": 65_535}), &c).unwrap();
        apply(&mut c, &p);
        assert_eq!(c.port, 65_535);

        let mut c = Config::default();
        load_into(
            &mut c,
            &json!({"on_insert": "bogus", "log_retention_days": 7}),
        );
        assert_eq!(c.on_insert, Config::default().on_insert);
        assert_eq!(c.log_retention_days, 7);
    }
}
