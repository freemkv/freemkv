//! Persisted UI settings. Stored as JSON under the user's Application Support
//! directory — never in the bundle, which is not writable.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    // Output
    pub dest_dir: String,
    pub container: String,
    pub filename_template: String,
    pub keep_iso: bool,
    /// Eject the disc when the rip finishes reading it (mirrors autorip's
    /// `auto_eject`; default on).
    pub auto_eject: bool,
    /// Fire a native desktop notification when a rip completes. Default on;
    /// a file from before the field gets `true` via the container's
    /// `#[serde(default)]` (`Settings::default()`).
    pub notify_when_rip_finished: bool,
    // Selection
    pub selection: String,
    pub min_title_secs: String,
    /// Preferred AUDIO languages, comma-separated ("German, Spanish" / "de,
    /// es"). A SET, not a priority chain: every audio track matching ANY of
    /// them starts ticked. Empty (the default) = today's behaviour, every
    /// track ticked.
    pub audio_langs: String,
    /// Preferred NON-FORCED subtitle languages, same shape as `audio_langs`.
    pub sub_langs: String,
    /// Preferred FORCED-subtitle languages — its own independent set, NOT a
    /// filter applied on top of `sub_langs`. "German subtitles, and forced
    /// only if in English" is a single coherent request that one list cannot
    /// express, which is why this is a third field and not a flag.
    pub forced_sub_langs: String,
    /// Subtitle stream mode: `all`, `none`, or `forced`.
    pub subtitle_mode: String,
    /// Whether title-bar audio/subtitle choices are written to disk.
    pub persist_stream_preferences: bool,
    // Drive & I/O
    pub rip_mode: String,
    pub max_passes: String,
    pub abort_lost_secs: String,
    // Keys — no defaults: the user supplies these
    pub key_source: String,
    pub keydb_path: String,
    pub keydb_url: String,
    pub keyserver_url: String,
    pub keyserver_token: String,
    // Protection
    pub raw: bool,
    pub force: bool,
    pub log_level: String,
    // Advanced
    pub language: String,
    pub decrypt_threads: String,
    // Window
    pub win_w: f64,
    pub win_h: f64,
    /// Any keys this build does not know — a newer version's fields, or a
    /// hand-added one. Without this, `save()` would round-trip through the
    /// named fields only and silently drop them; `#[serde(flatten)]` captures
    /// them here so they survive load→save unchanged. An empty map flattens to
    /// no keys, so an ordinary settings file gains nothing.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dest_dir: dirs_movies(),
            // Canonical output-format string — must match a value `ui::output_formats`
            // produces so the Settings popup and main window dropdown can both
            // select it. (An older "Matroska (.mkv)" default matched none, blank.)
            container: "Selected titles → MKV".into(),
            filename_template: "{title}_t{n}".into(),
            keep_iso: false,
            // Mirror autorip's auto_eject default (on): pop the disc when the
            // read phase completes so the user can grab it / load the next.
            auto_eject: true,
            notify_when_rip_finished: true,
            selection: "Main film only".into(),
            min_title_secs: "120".into(),
            // Empty = no preference = exactly the pre-1.6.2 behaviour: a
            // ticked title ticks every one of its streams.
            audio_langs: String::new(),
            sub_langs: String::new(),
            forced_sub_langs: String::new(),
            subtitle_mode: "all".into(),
            persist_stream_preferences: true,
            rip_mode: "Multi-pass".into(),
            max_passes: "5".into(),
            abort_lost_secs: "0".into(),
            key_source: "Local keydb only".into(),
            keydb_path: default_keydb_path(),
            // Deliberately empty — the user supplies these; we never ship a
            // default endpoint.
            keydb_url: String::new(),
            keyserver_url: String::new(),
            keyserver_token: String::new(),
            raw: false,
            force: false,
            log_level: "Normal".into(),
            language: "auto".into(),
            decrypt_threads: "0".into(),
            win_w: 1180.0,
            win_h: 760.0,
            extra: serde_json::Map::new(),
        }
    }
}

fn home() -> PathBuf {
    crate::platform::home_dir()
}

/// Per-OS writable state directory — see `platform::support_dir`. Kept as a
/// re-export so the many existing `settings::support_dir()` call sites (and the
/// GUI log writer in `main.rs`) are unchanged.
pub fn support_dir() -> PathBuf {
    crate::platform::support_dir()
}

fn dirs_movies() -> String {
    crate::platform::default_dest_dir()
        .to_string_lossy()
        .into_owned()
}

fn default_keydb_path() -> String {
    support_dir()
        .join("keydb.cfg")
        .to_string_lossy()
        .into_owned()
}

fn settings_path() -> PathBuf {
    support_dir().join("gui-settings.json")
}

// Create with mode 0600 up front so the `keyserver_token` secret is never
// briefly readable at the process umask before a follow-up chmod.
// `create_new` refuses to reuse a path; only called on a fresh temp filename.
fn write_new_file_0600(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

/// What `Settings::load` found on disk.
///
/// The distinction that matters: `Missing` is a normal first run, `Unreadable`
/// means the user HAD settings and this launch is not using them. Collapsing
/// the two into `unwrap_or_default()` is what made the data loss invisible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// No settings file yet — first run. Defaults are correct here.
    Missing,
    /// The file was read and parsed.
    Loaded,
    /// A settings file exists but could not be used, so this session is
    /// running on defaults. `preserved` is where the original was moved to,
    /// or `None` when it could not be moved (or could not be read at all).
    Unreadable {
        path: PathBuf,
        error: String,
        preserved: Option<PathBuf>,
    },
}

impl LoadOutcome {
    /// Say so in the diagnostic log. Idempotent and cheap, so the startup path
    /// can call it again after the tracing subscriber is installed.
    ///
    /// Not routed to the GUI log pane: every line there goes through
    /// `freemkv-i18n`, which is a separate crate pinned to a release tag, so a
    /// new key cannot ship with this fix — it would resolve to nothing on the
    /// pinned build. The evidence the user can see is the preserved
    /// `gui-settings.json.bad` file sitting next to the settings file.
    pub fn warn(&self) {
        if let LoadOutcome::Unreadable {
            path,
            error,
            preserved,
        } = self
        {
            match preserved {
                Some(kept) => tracing::warn!(
                    "settings file {} could not be read ({error}) — running on \
                     DEFAULTS for this session; your previous settings were \
                     kept at {}",
                    path.display(),
                    kept.display()
                ),
                None => tracing::warn!(
                    "settings file {} could not be read ({error}) — running on \
                     DEFAULTS for this session; the file was left in place",
                    path.display()
                ),
            }
        }
    }
}

// Move an unusable settings file aside, returning where it went. Never
// clobbers an earlier preserved copy: the first `.bad` is likeliest to hold
// real values, so it must survive later corruption of the fallback defaults.
fn preserve_unreadable(path: &std::path::Path) -> Option<PathBuf> {
    let candidates = std::iter::once(path.with_extension("json.bad"))
        .chain((2..10).map(|n| path.with_extension(format!("json.bad.{n}"))));
    for cand in candidates {
        if !cand.exists() {
            return std::fs::rename(path, &cand).ok().map(|()| cand);
        }
    }
    // Every numbered slot is taken. Giving up isn't neutral: the file stays LIVE
    // and the next `save()` overwrites the one copy still holding real values.
    // Overflow the newest; `.bad` (oldest, likeliest to hold real values) stays.
    let overflow = path.with_extension("json.bad.overflow");
    std::fs::rename(path, &overflow).ok().map(|()| overflow)
}

impl Settings {
    /// Read one keyed value as a display string.
    pub fn get(&self, key: &str) -> String {
        match key {
            "dest_dir" => self.dest_dir.clone(),
            "container" => self.container.clone(),
            "filename_template" => self.filename_template.clone(),
            "selection" => self.selection.clone(),
            "min_title_secs" => self.min_title_secs.clone(),
            "audio_langs" => self.audio_langs.clone(),
            "sub_langs" => self.sub_langs.clone(),
            "forced_sub_langs" => self.forced_sub_langs.clone(),
            "subtitle_mode" => self.subtitle_mode.clone(),
            "rip_mode" => self.rip_mode.clone(),
            "max_passes" => self.max_passes.clone(),
            "abort_lost_secs" => self.abort_lost_secs.clone(),
            "key_source" => self.key_source.clone(),
            "keydb_path" => self.keydb_path.clone(),
            "keydb_url" => self.keydb_url.clone(),
            "keyserver_url" => self.keyserver_url.clone(),
            "keyserver_token" => self.keyserver_token.clone(),
            "language" => self.language.clone(),
            "decrypt_threads" => self.decrypt_threads.clone(),
            "log_level" => self.log_level.clone(),
            _ => String::new(),
        }
    }

    pub fn get_bool(&self, key: &str) -> bool {
        match key {
            "keep_iso" => self.keep_iso,
            "auto_eject" => self.auto_eject,
            "notify_when_rip_finished" => self.notify_when_rip_finished,
            "raw" => self.raw,
            "force" => self.force,
            "persist_stream_preferences" => self.persist_stream_preferences,
            _ => false,
        }
    }

    pub fn set(&mut self, key: &str, v: String) {
        match key {
            "dest_dir" => self.dest_dir = v,
            "container" => self.container = v,
            "filename_template" => self.filename_template = v,
            "selection" => self.selection = v,
            "min_title_secs" => self.min_title_secs = v,
            "audio_langs" => self.audio_langs = v,
            "sub_langs" => self.sub_langs = v,
            "forced_sub_langs" => self.forced_sub_langs = v,
            "subtitle_mode" => self.subtitle_mode = v,
            "rip_mode" => self.rip_mode = v,
            "max_passes" => self.max_passes = v,
            "abort_lost_secs" => self.abort_lost_secs = v,
            "key_source" => self.key_source = v,
            "keydb_path" => self.keydb_path = v,
            "keydb_url" => self.keydb_url = v,
            "keyserver_url" => self.keyserver_url = v,
            "keyserver_token" => self.keyserver_token = v,
            "language" => self.language = v,
            "decrypt_threads" => self.decrypt_threads = v,
            "log_level" => self.log_level = v,
            _ => {}
        }
    }

    pub fn set_bool(&mut self, key: &str, v: bool) {
        match key {
            "keep_iso" => self.keep_iso = v,
            "auto_eject" => self.auto_eject = v,
            "notify_when_rip_finished" => self.notify_when_rip_finished = v,
            "raw" => self.raw = v,
            "force" => self.force = v,
            "persist_stream_preferences" => self.persist_stream_preferences = v,
            _ => {}
        }
    }

    /// The user's settings, or the defaults when there are none to load.
    ///
    /// Never fails, but never fails *silently* either — see [`LoadOutcome`]
    /// and [`Settings::load_reporting`] for the caller that wants to know.
    pub fn load() -> Self {
        Self::load_reporting().0
    }

    /// `load()`, plus what actually happened on disk.
    ///
    /// The startup path uses this so the "your settings did not parse" warning
    /// can be repeated once the tracing subscriber exists: `init_gui_logging`
    /// is configured FROM the settings, so it is necessarily installed after
    /// this call and the warning emitted here reaches no log file.
    pub fn load_reporting() -> (Self, LoadOutcome) {
        let (mut s, outcome) = Self::load_from(&settings_path());
        s.normalize();
        (s, outcome)
    }

    /// Read and parse one settings file. Split out from `load()` so the
    /// behaviour can be tested against a temp path instead of the real
    /// per-user support directory.
    fn load_from(path: &std::path::Path) -> (Self, LoadOutcome) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            // No file yet is the normal first run, not a problem to report.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return (Settings::default(), LoadOutcome::Missing);
            }
            // A file that cannot be READ (permissions, a directory in its place) has
            // nothing to preserve and a rename would likely fail too; but one that is not
            // UTF-8 (an ANSI or UTF-16 hand edit) was read, so it is moved aside.
            Err(e) => {
                let preserved = (e.kind() == std::io::ErrorKind::InvalidData)
                    .then(|| preserve_unreadable(path))
                    .flatten();
                let outcome = LoadOutcome::Unreadable {
                    path: path.to_path_buf(),
                    error: format!("{e}"),
                    preserved,
                };
                outcome.warn();
                return (Settings::default(), outcome);
            }
        };
        // A leading UTF-8 BOM is not whitespace to serde_json — it rejects the
        // document outright. PowerShell and Notepad both write one by default, so
        // a Windows user hand-editing this file would otherwise lose every setting.
        let body = text.strip_prefix('\u{feff}').unwrap_or(&text);
        match serde_json::from_str::<Settings>(body) {
            Ok(s) => (s, LoadOutcome::Loaded),
            Err(e) => {
                // The file still holds the user's token/folder/keydb path. Move
                // it aside NOW: `load()` runs at startup and the next `save()`
                // would write defaults straight over it.
                let preserved = preserve_unreadable(path);
                let outcome = LoadOutcome::Unreadable {
                    path: path.to_path_buf(),
                    error: format!("{e}"),
                    preserved,
                };
                outcome.warn();
                (Settings::default(), outcome)
            }
        }
    }

    // Snap enum-valued and path fields back to defaults when unusable so the popup and engine
    // always have a value to select/match on.
    fn normalize(&mut self) {
        let d = Settings::default();
        // The canonical values come from the same table the dropdowns are built from, so
        // an added option can never be snapped back to the default on the next launch.
        let snap = |cur: &mut String, key: &str, def: &str| {
            if !crate::ui::enum_options(key)
                .iter()
                .any(|(canon, _)| *canon == cur.as_str())
            {
                *cur = def.to_string();
            }
        };
        snap(&mut self.selection, "selection", &d.selection);
        snap(&mut self.rip_mode, "rip_mode", &d.rip_mode);
        snap(&mut self.key_source, "key_source", &d.key_source);
        snap(&mut self.log_level, "log_level", &d.log_level);
        if !matches!(self.subtitle_mode.as_str(), "all" | "none" | "forced") {
            self.subtitle_mode = d.subtitle_mode.clone();
        }
        // The output container must be one of the canonical format strings the
        // dropdown offers, else it renders blank and the engine can't map it.
        let known = crate::ui::output_formats(true, true).concat();
        if !known.contains(&self.container.as_str()) {
            self.container = d.container.clone();
        }
        // Language persists as a locale code (or "auto"); fold any legacy
        // endonym / unknown value to a clean code.
        self.language = crate::ui::locale_code(&self.language).to_string();
        // A destination that isn't absolute (empty, or a stale "..." placeholder) can't be
        // written to — fall back to the default folder; `~/` is expanded first, as for
        // `keydb_path`. The test is per-OS: `starts_with('/')` reset every Windows path.
        self.dest_dir = shellexpand(&self.dest_dir);
        if !crate::platform::is_absolute(&self.dest_dir) {
            self.dest_dir = d.dest_dir.clone();
        }
        // keydb.cfg location, likewise: never leave it empty (the default is a
        // real path in Application Support).
        if self.keydb_path.trim().is_empty() {
            self.keydb_path = d.keydb_path.clone();
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = support_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{e}"))?;
        let json = serde_json::to_string_pretty(self).map_err(|e| format!("{e}"))?;
        let path = settings_path();
        // Holds `keyserver_token` in plaintext, so it needs mode 0600 (plain
        // `fs::write` leaves it at umask, often 0644) and an atomic write — a
        // crash mid-write else corrupts the JSON and `load()` silently drops it.
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        // The temp name is unique to THIS process, so anything on it is debris
        // from an earlier failed save; clear it first so saves recover instead of
        // permanently failing `create_new` (which still refuses a re-created symlink).
        let _ = std::fs::remove_file(&tmp);
        write_new_file_0600(&tmp, json.as_bytes()).map_err(|e| {
            // A write that failed PART-WAY has already created the file with the
            // secret in it. Only the rename branch used to clean up, so a full
            // disk left the token on disk under a name nobody would check.
            let _ = std::fs::remove_file(&tmp);
            format!("{e}")
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{e}")
        })?;
        Ok(())
    }

    /// Keydb status line for the Keys tab and the source strip, in the active locale.
    pub fn keydb_status(&self) -> String {
        use crate::strings::{fmt_or, get_or};
        let p = PathBuf::from(shellexpand(&self.keydb_path));
        match std::fs::metadata(&p) {
            Ok(m) => {
                let kb = m.len() / 1024;
                let age = m
                    .modified()
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| {
                        let days = d.as_secs() / 86_400;
                        if days == 0 {
                            get_or("gui.set.keydb_age_today", "today")
                        } else if days == 1 {
                            get_or("gui.set.keydb_age_yesterday", "yesterday")
                        } else {
                            fmt_or(
                                "gui.set.keydb_age_days",
                                "{days} days ago",
                                &[("days", &days.to_string())],
                            )
                        }
                    })
                    .unwrap_or_else(|| get_or("gui.set.keydb_age_unknown", "unknown"));
                fmt_or(
                    "gui.set.keydb_found",
                    "keydb found — {kb} KB, updated {age}",
                    &[("kb", &kb.to_string()), ("age", &age)],
                )
            }
            Err(_) => get_or("gui.set.keydb_not_found", "no keydb.cfg found"),
        }
    }
}

pub fn shellexpand(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest).to_string_lossy().into_owned()
    } else {
        p.to_string()
    }
}

/// Download and install the keydb from the configured URL.
///
/// The bytes go through `KeydbSource::save`, which handles zip/gz
/// decompression, validates at least one real entry, caps the decompressed
/// size against a decompression bomb, and writes atomically. Doing that here
/// by hand would be a second, worse implementation.
/// Blocking — call off the UI thread.
pub fn update_keydb(url: &str, dest: &str) -> Result<String, String> {
    use crate::strings::{fmt_or, get_or};
    if url.trim().is_empty() {
        return Err(get_or(
            "gui.log.keydb_no_url",
            "No keydb update URL set — add one in Settings ▸ Keys",
        ));
    }
    let dest = shellexpand(dest);
    if dest.trim().is_empty() {
        return Err(get_or(
            "gui.log.keydb_no_path",
            "No keydb.cfg location set — add one in Settings ▸ Keys",
        ));
    }
    // Route through the SAME hardened fetch the CLI's `update-keys` uses
    // (SSRF/private-IP guard, zero redirects, body cap). Used to be a bare
    // `ureq::get`, so the GUI lacked every guard the CLI applies to this URL.
    let buf = crate::keydb_fetch::fetch(url).map_err(|e| {
        fmt_or(
            "gui.log.keydb_download_failed",
            "Download failed: {error}",
            &[("error", &e.to_string())],
        )
    })?;
    if buf.is_empty() {
        return Err(get_or("gui.log.keydb_download_empty", "Download was empty"));
    }
    if let Some(parent) = std::path::Path::new(&dest).parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "cannot create keydb directory {}: {e} (the directory must be writable by freemkv)",
                parent.display()
            )
        })?;
    }
    let src = freemkv_keysources::KeydbSource::new(&dest);
    match src.save(&buf) {
        Ok(r) => Ok(fmt_or(
            "gui.log.keydb_updated",
            "keydb updated — {entries} entries ({kb} KB) written to {path}",
            &[
                ("entries", &r.entries.to_string()),
                ("kb", &(r.bytes / 1024).to_string()),
                ("path", &r.path.display().to_string()),
            ],
        )),
        Err(e) => Err(fmt_or(
            "gui.log.keydb_rejected",
            &format!(
                "keydb could not be written to {}: E{} (check that its directory is writable)",
                dest,
                e.code()
            ),
            &[("code", &e.code().to_string())],
        )),
    }
}

/// T25's no-answer and idle bounds (stop design v5 §2.7): DNS 10 s (as the keydb fetch's
/// `DNS_TIMEOUT`), connect 10 s, headers 10 s, body idle 10 s; no total.
#[derive(Clone, Copy)]
struct UpdateTimeouts {
    resolve: std::time::Duration,
    connect: std::time::Duration,
    headers: std::time::Duration,
    idle: std::time::Duration,
}

const UPDATE_TIMEOUTS: UpdateTimeouts = UpdateTimeouts {
    resolve: std::time::Duration::from_secs(10),
    connect: std::time::Duration::from_secs(10),
    headers: std::time::Duration::from_secs(10),
    idle: std::time::Duration::from_secs(10),
};

/// Ask GitHub for the newest published release tag.
///
/// Deliberately explicit about every outcome: an update check that silently
/// claims "you're up to date" when it never reached the server is worse than
/// no check at all. Blocking — call off the UI thread.
pub fn check_for_update(current: &str) -> String {
    const URL: &str = "https://api.github.com/repos/freemkv/freemkv/releases/latest";
    check_for_update_at(URL, current, UPDATE_TIMEOUTS)
}

fn update_config(t: UpdateTimeouts) -> ureq::config::Config {
    // Stop design v5 §2.7 (T25): "connect 10 s, headers 10 s, and a body idle of 10 s
    // through freemkv's `IdleReCapConnector`"; the `timeout_global(10 s)` total is gone.
    ureq::config::Config::builder()
        .timeout_resolve(Some(t.resolve))
        .timeout_connect(Some(t.connect))
        .timeout_recv_response(Some(t.headers))
        .timeout_recv_body(None)
        .build()
}

/// Whether `latest` is a later release than `current`. Compares the dotted numeric core
/// (a `-pre` or `+build` suffix is ignored); a version that does not parse that way
/// counts as different, so an odd tag is still surfaced rather than hidden.
fn is_newer(latest: &str, current: &str) -> bool {
    fn core(v: &str) -> Option<Vec<u64>> {
        let v = v.trim().trim_start_matches('v');
        let v = v.split(['-', '+']).next()?;
        v.split('.').map(|n| n.parse().ok()).collect()
    }
    match (core(latest), core(current)) {
        (Some(l), Some(c)) => l > c,
        _ => latest != current,
    }
}

fn update_failed(e: &dyn std::fmt::Display) -> String {
    crate::strings::fmt_or(
        "gui.log.update_failed",
        "Update check failed: {error}",
        &[("error", &e.to_string())],
    )
}

fn check_for_update_at(url: &str, current: &str, t: UpdateTimeouts) -> String {
    use crate::strings::{fmt_or, get_or};
    let resp = crate::keydb_fetch::idle_agent(update_config(t), t.idle)
        .get(url)
        .header("User-Agent", "freemkv-gui")
        .header("Accept", "application/vnd.github+json")
        .call();

    let body = match resp {
        // `Body::read_to_string` is NOT unbounded: ureq applies its own 10 MiB
        // `limit()` — no need for `keydb_fetch::read_capped` (the larger, uncapped path).
        Ok(r) => match r.into_body().read_to_string() {
            Ok(b) => b,
            Err(e) => return update_failed(&e),
        },
        Err(ureq::Error::StatusCode(404)) => {
            return get_or(
                "gui.log.update_none_published",
                "Update check: no releases published yet.",
            );
        }
        Err(ureq::Error::StatusCode(code)) => {
            return fmt_or(
                "gui.log.update_http_status",
                "Update check failed: server returned {code}",
                &[("code", &code.to_string())],
            );
        }
        Err(e) => return update_failed(&e),
    };

    let tag = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => v
            .get("tag_name")
            .and_then(|t| t.as_str())
            .map(|s| s.trim_start_matches('v').to_string()),
        Err(e) => {
            return fmt_or(
                "gui.log.update_bad_response",
                "Update check failed: bad response ({error})",
                &[("error", &e.to_string())],
            );
        }
    };

    match tag {
        Some(latest) if !is_newer(&latest, current) => fmt_or(
            "gui.log.update_latest",
            "You are running the latest version ({current}).",
            &[("current", current)],
        ),
        Some(latest) => fmt_or(
            "gui.log.update_available",
            "Update available: {latest} (you have {current}) — https://freemkv.org",
            &[("latest", &latest), ("current", current)],
        ),
        None => get_or(
            "gui.log.update_no_version",
            "Update check failed: no version in response",
        ),
    }
}

#[cfg(test)]
#[path = "settings_normalize_tests.rs"]
mod normalize_tests;

#[cfg(test)]
#[path = "settings_update_check_tests.rs"]
mod update_check_tests;
