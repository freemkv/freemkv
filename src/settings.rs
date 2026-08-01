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
    // Selection
    pub selection: String,
    pub min_title_secs: String,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            dest_dir: dirs_movies(),
            // Canonical output-format string — must be one of the values
            // `ui::output_formats` produces, so the Settings popup and the main
            // window's format dropdown can both select it. (An older
            // "Matroska (.mkv)" default matched no popup item and rendered
            // blank.)
            container: "Selected titles → MKV".into(),
            filename_template: "{title}_t{n}".into(),
            keep_iso: false,
            // Mirror autorip's auto_eject default (on): pop the disc when the
            // read phase completes so the user can grab it / load the next.
            auto_eject: true,
            selection: "Main film only".into(),
            min_title_secs: "120".into(),
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

impl Settings {
    /// Read one keyed value as a display string.
    pub fn get(&self, key: &str) -> String {
        match key {
            "dest_dir" => self.dest_dir.clone(),
            "container" => self.container.clone(),
            "filename_template" => self.filename_template.clone(),
            "selection" => self.selection.clone(),
            "min_title_secs" => self.min_title_secs.clone(),
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
            "raw" => self.raw,
            "force" => self.force,
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
            "raw" => self.raw = v,
            "force" => self.force = v,
            _ => {}
        }
    }

    pub fn load() -> Self {
        let mut s: Settings = std::fs::read_to_string(settings_path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        s.normalize();
        s
    }

    /// Every setting must carry a value the popup can select and the engine can
    /// match on. `#[serde(default)]` fills fields missing from the file; this
    /// additionally snaps an enum-valued field back to its default when the
    /// persisted string is not one of the recognized options (a stale file from
    /// an older build, or a hand-edit). Free-form fields (paths, URLs, numbers)
    /// are left as-is.
    fn normalize(&mut self) {
        let d = Settings::default();
        let snap = |cur: &mut String, opts: &[&str], def: &str| {
            if !opts.contains(&cur.as_str()) {
                *cur = def.to_string();
            }
        };
        snap(
            &mut self.selection,
            &["Main film only", "All titles", "Longest title"],
            &d.selection,
        );
        snap(
            &mut self.rip_mode,
            &["Multi-pass", "Single pass"],
            &d.rip_mode,
        );
        snap(
            &mut self.key_source,
            &[
                "Local keydb only",
                "Online key service only",
                "keydb, then online",
            ],
            &d.key_source,
        );
        snap(
            &mut self.log_level,
            &["Quiet", "Normal", "Verbose", "Debug"],
            &d.log_level,
        );
        // The output container must be one of the canonical format strings the
        // dropdown offers, else it renders blank and the engine can't map it.
        let known = crate::ui::output_formats(true, true).concat();
        if !known.contains(&self.container.as_str()) {
            self.container = d.container.clone();
        }
        // Language persists as a locale code (or "auto"); fold any legacy
        // endonym / unknown value to a clean code.
        self.language = crate::ui::locale_code(&self.language).to_string();
        // A destination that isn't an absolute path (empty, or a stale "..."
        // placeholder from an older build) can't be written to and shows blank
        // in the field — fall back to the default output folder. The test is
        // per-OS: `starts_with('/')` called every valid Windows path relative
        // and silently reset the user's destination on each load.
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
        std::fs::write(settings_path(), json).map_err(|e| format!("{e}"))?;
        Ok(())
    }

    /// Keydb status line for the Keys tab and the source strip.
    pub fn keydb_status(&self) -> String {
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
                            "today".to_string()
                        } else if days == 1 {
                            "yesterday".to_string()
                        } else {
                            format!("{days} days ago")
                        }
                    })
                    .unwrap_or_else(|| "unknown".into());
                format!("keydb found — {kb} KB, updated {age}")
            }
            Err(_) => "no keydb.cfg found".to_string(),
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
    if url.trim().is_empty() {
        return Err("No keydb update URL set — add one in Settings ▸ Keys".into());
    }
    let dest = shellexpand(dest);
    if dest.trim().is_empty() {
        return Err("No keydb.cfg location set — add one in Settings ▸ Keys".into());
    }
    // Route through the SAME hardened fetch the CLI's `update-keys` uses:
    // SSRF/private-IP guard on the resolved address, zero redirects, and a body
    // cap. This used to be a bare `ureq::get`, so the GUI button downloaded an
    // arbitrary user-supplied URL with redirects followed and no size limit —
    // none of the guards the CLI applies to the very same untrusted resource.
    let buf = crate::keydb_fetch::fetch(url).map_err(|e| format!("Download failed: {e}"))?;
    if buf.is_empty() {
        return Err("Download was empty".into());
    }
    if let Some(parent) = std::path::Path::new(&dest).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{e}"))?;
    }
    let src = freemkv_keysources::KeydbSource::new(&dest);
    match src.save(&buf) {
        Ok(r) => Ok(format!(
            "keydb updated — {} entries ({} KB) written to {}",
            r.entries,
            r.bytes / 1024,
            r.path.display()
        )),
        Err(e) => Err(format!("keydb rejected: E{}", e.code())),
    }
}

/// Ask GitHub for the newest published release tag.
///
/// Deliberately explicit about every outcome: an update check that silently
/// claims "you're up to date" when it never reached the server is worse than
/// no check at all. Blocking — call off the UI thread.
pub fn check_for_update(current: &str) -> String {
    const URL: &str = "https://api.github.com/repos/freemkv/freemkv-gui/releases/latest";
    let resp = ureq::get(URL)
        .set("User-Agent", "freemkv-gui")
        .set("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(10))
        .call();

    let body = match resp {
        Ok(r) => match r.into_string() {
            Ok(b) => b,
            Err(e) => return format!("Update check failed: {e}"),
        },
        Err(ureq::Error::Status(404, _)) => {
            return "Update check: no releases published yet.".into();
        }
        Err(ureq::Error::Status(code, _)) => {
            return format!("Update check failed: server returned {code}");
        }
        Err(e) => return format!("Update check failed: {e}"),
    };

    let tag = match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(v) => v
            .get("tag_name")
            .and_then(|t| t.as_str())
            .map(|s| s.trim_start_matches('v').to_string()),
        Err(e) => return format!("Update check failed: bad response ({e})"),
    };

    match tag {
        Some(latest) if latest == current => {
            format!("You are running the latest version ({current}).")
        }
        Some(latest) => {
            format!("Update available: {latest} (you have {current}) — https://freemkv.org")
        }
        None => "Update check failed: no version in response".into(),
    }
}
