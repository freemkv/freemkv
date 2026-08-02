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

/// Write a brand-new file with mode 0600 before any bytes land in it, so a
/// secret (the settings file's `keyserver_token`) is never briefly readable
/// at the process umask (typically 0644) between creation and a follow-up
/// `set_permissions` call. `create_new` also refuses to reuse an existing
/// path — this is only ever called on a fresh per-process temp filename.
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

/// Move an unusable settings file aside, returning where it went.
///
/// Never clobbers an earlier preserved copy: the first `.bad` is the one most
/// likely to hold real values (a later corruption may only be corrupting the
/// defaults this fallback just wrote), so it is the one that must survive.
fn preserve_unreadable(path: &std::path::Path) -> Option<PathBuf> {
    let candidates = std::iter::once(path.with_extension("json.bad"))
        .chain((2..10).map(|n| path.with_extension(format!("json.bad.{n}"))));
    for cand in candidates {
        if !cand.exists() {
            return std::fs::rename(path, &cand).ok().map(|()| cand);
        }
    }
    None
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
            // A file that exists but cannot be READ (permissions, a directory
            // in its place) is a problem — but there is nothing to preserve
            // and a rename would likely fail the same way, so leave it be.
            Err(e) => {
                let outcome = LoadOutcome::Unreadable {
                    path: path.to_path_buf(),
                    error: format!("{e}"),
                    preserved: None,
                };
                outcome.warn();
                return (Settings::default(), outcome);
            }
        };
        // A leading UTF-8 BOM is not whitespace to serde_json — it rejects the
        // document outright. PowerShell's `Out-File -Encoding utf8` writes one
        // and Notepad has historically saved UTF-8 with a BOM by default, so a
        // Windows user hand-editing this file to paste a keyserver token would
        // otherwise lose every setting they have.
        let body = text.strip_prefix('\u{feff}').unwrap_or(&text);
        match serde_json::from_str::<Settings>(body) {
            Ok(s) => (s, LoadOutcome::Loaded),
            Err(e) => {
                // The file still holds the user's key service token, output
                // folder and keydb path. Move it aside NOW: `load()` runs at
                // startup and the next `save()` — settings popup, window
                // resize — would write defaults straight over it.
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
        let path = settings_path();
        // This file holds `keyserver_token` in plaintext, so it gets the same
        // two guards a secret on disk needs: mode 0600 (a plain `fs::write`
        // leaves it at the process umask, typically 0644 — readable by every
        // other local account) and an atomic write (temp file in the SAME
        // directory, then rename). Without the rename, a crash/power-loss
        // mid-write leaves a truncated/corrupt JSON file; `Settings::load`
        // then silently falls back to defaults via `.ok()`, discarding the
        // user's keyserver_url/keyserver_token/keydb_path with no warning.
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        write_new_file_0600(&tmp, json.as_bytes()).map_err(|e| format!("{e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{e}")
        })?;
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

#[cfg(test)]
mod normalize_tests {
    use super::{
        LoadOutcome, Settings, default_keydb_path, dirs_movies, settings_path, support_dir,
    };

    /// A scratch directory of this test's own, so nothing here touches the
    /// real per-user support dir (and no `HOME` juggling is needed —
    /// `load_from` takes the path).
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fmkv-load-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The three cases `load()` must tell apart. Before this fix all three
    /// were the same code path — `unwrap_or_default()` — so a user whose
    /// settings had just been discarded got exactly what a first-run user
    /// gets, with nothing anywhere saying otherwise.
    #[test]
    fn load_distinguishes_no_file_from_a_good_file_from_an_unreadable_one() {
        let dir = scratch("outcome");

        // 1. No file: the normal first run. Defaults, and NOT reported.
        let missing = dir.join("gui-settings.json");
        let (s, outcome) = Settings::load_from(&missing);
        assert_eq!(outcome, LoadOutcome::Missing);
        assert_eq!(s.keyserver_url, Settings::default().keyserver_url);
        assert!(
            !missing.with_extension("json.bad").exists(),
            "a first run must not leave a .bad file"
        );

        // 2. A good file: parsed, values kept.
        let good = dir.join("good.json");
        std::fs::write(&good, r#"{"keyserver_token":"tok"}"#).unwrap();
        let (s, outcome) = Settings::load_from(&good);
        assert_eq!(outcome, LoadOutcome::Loaded);
        assert_eq!(s.keyserver_token, "tok");
        assert!(
            good.exists(),
            "a good file must be left exactly where it is"
        );

        // 3. Present but unparseable: reported, with the error and the path
        // the original was preserved at.
        let bad = dir.join("bad.json");
        std::fs::write(&bad, "{ not json").unwrap();
        let (s, outcome) = Settings::load_from(&bad);
        assert_eq!(s.keyserver_token, "", "must fall back to defaults");
        match outcome {
            LoadOutcome::Unreadable {
                path,
                error,
                preserved,
            } => {
                assert_eq!(path, bad);
                assert!(!error.is_empty(), "the parse error must be reported");
                let kept = preserved.expect("the original must be preserved");
                assert_eq!(std::fs::read_to_string(kept).unwrap(), "{ not json");
                assert!(!bad.exists(), "the unusable file must be moved aside");
            }
            other => panic!("expected Unreadable, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Preserving must never overwrite an already-preserved copy. The first
    /// `.bad` is the one likely to hold the real keyserver token; a second
    /// corruption is probably a corruption of the defaults written after the
    /// first, and must not be allowed to replace it.
    #[test]
    fn a_second_corruption_does_not_clobber_the_first_preserved_copy() {
        let dir = scratch("twice");
        let path = dir.join("gui-settings.json");

        std::fs::write(&path, "first — holds the real token").unwrap();
        let (_, first) = Settings::load_from(&path);
        std::fs::write(&path, "second — only defaults were in here").unwrap();
        let (_, second) = Settings::load_from(&path);

        let (
            LoadOutcome::Unreadable {
                preserved: Some(a), ..
            },
            LoadOutcome::Unreadable {
                preserved: Some(b), ..
            },
        ) = (first, second)
        else {
            panic!("both loads should have preserved the file")
        };
        assert_ne!(a, b, "the second corruption reused the first .bad name");
        assert_eq!(
            std::fs::read_to_string(&a).unwrap(),
            "first — holds the real token",
            "the first preserved copy was overwritten"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `normalize()` is private, called only from `load()`, and `load()` had no
    /// test — so the exact bug class its doc comment describes had ZERO
    /// coverage, and the whole function could be replaced with `()`.
    ///
    /// Each snapped field is a value the popup must be able to select and the
    /// engine must be able to match on. A stale string from an older build
    /// leaves the control rendering blank and the setting silently inert.
    #[test]
    fn normalize_snaps_every_stale_enum_back_to_its_default() {
        let d = Settings::default();
        let mut s = Settings {
            selection: "bogus".into(),
            rip_mode: "bogus".into(),
            key_source: "bogus".into(),
            log_level: "bogus".into(),
            container: "Matroska (.mkv)".into(), // a real pre-1.5 value
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.selection, d.selection);
        assert_eq!(s.rip_mode, d.rip_mode);
        assert_eq!(s.key_source, d.key_source);
        assert_eq!(s.log_level, d.log_level);
        assert_eq!(s.container, d.container);
    }

    /// A valid value must survive normalization untouched — otherwise the snap
    /// is not a repair, it is a reset of the user's choices on every load.
    #[test]
    fn normalize_leaves_every_recognized_value_alone() {
        let mut s = Settings {
            selection: "All titles".into(),
            rip_mode: "Single pass".into(),
            key_source: "keydb, then online".into(),
            log_level: "Debug".into(),
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.selection, "All titles");
        assert_eq!(s.rip_mode, "Single pass");
        assert_eq!(s.key_source, "keydb, then online");
        assert_eq!(s.log_level, "Debug");
    }

    /// A relative destination cannot be written to and renders blank. This is
    /// the fallback that catches an empty base directory — the exact shape of
    /// the unset-`HOME` bug, where `default_dest_dir()` came back as `"Movies"`
    /// and rips landed next to the process CWD.
    #[test]
    fn a_relative_destination_falls_back_and_an_absolute_one_does_not() {
        let d = Settings::default();

        for bad in ["", "   ", "Movies", "../out"] {
            let mut s = Settings {
                dest_dir: bad.into(),
                ..Settings::default()
            };
            s.normalize();
            assert_eq!(s.dest_dir, d.dest_dir, "{bad:?} should have fallen back");
        }

        let custom = if cfg!(windows) {
            r"C:\custom\output"
        } else {
            "/custom/output"
        };
        let mut s = Settings {
            dest_dir: custom.into(),
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.dest_dir, custom, "an absolute folder must be preserved");
    }

    /// A blank keydb location is never left blank: the default is a real path
    /// in the support directory, and an empty one means the Keys tab points at
    /// nothing while still reporting a configured local source.
    #[test]
    fn a_blank_keydb_path_falls_back_to_the_default_location() {
        let d = Settings::default();
        for blank in ["", "  ", "\t"] {
            let mut s = Settings {
                keydb_path: blank.into(),
                ..Settings::default()
            };
            s.normalize();
            assert_eq!(s.keydb_path, d.keydb_path);
        }
        let mut kept = Settings {
            keydb_path: "~/keys/keydb.cfg".into(),
            ..Settings::default()
        };
        kept.normalize();
        assert_eq!(kept.keydb_path, "~/keys/keydb.cfg");
    }

    /// The four `settings::*` path wrappers forward to `platform::*`. They are
    /// separate functions from the ones `platform.rs`'s own tests exercise, so
    /// each could be replaced with `Default::default()` independently — and an
    /// empty `settings_path()` means the settings file is written to `""`.
    #[test]
    fn the_derived_paths_are_absolute_and_distinct() {
        let support = support_dir();
        assert!(support.is_absolute(), "support_dir: {support:?}");
        assert!(support.ends_with("freemkv"), "support_dir: {support:?}");

        let settings = settings_path();
        assert!(settings.is_absolute(), "settings_path: {settings:?}");
        assert!(settings.ends_with("gui-settings.json"));
        assert!(settings.starts_with(&support));

        let keydb = default_keydb_path();
        assert!(std::path::Path::new(&keydb).is_absolute(), "keydb: {keydb}");
        assert!(keydb.ends_with("keydb.cfg"));

        let movies = dirs_movies();
        assert!(
            std::path::Path::new(&movies).is_absolute(),
            "movies: {movies}"
        );
        assert!(!movies.is_empty());
        // The output folder is not the app's own state directory.
        assert_ne!(movies, support.to_string_lossy());
    }

    /// Every key `get`/`set` names must round-trip, including the ones the
    /// external suite's key lists happened to omit — `log_level` for `get`,
    /// `raw` and `force` for `get_bool`. Those three match arms were dead to
    /// the whole test suite: `cli_parity_flags_persist` sets the fields but
    /// reads them back through the struct, not through the accessor.
    #[test]
    fn every_accessor_key_round_trips_including_the_ones_the_suite_missed() {
        let mut s = Settings::default();
        for key in [
            "dest_dir",
            "container",
            "filename_template",
            "selection",
            "min_title_secs",
            "rip_mode",
            "max_passes",
            "abort_lost_secs",
            "key_source",
            "keydb_path",
            "keydb_url",
            "keyserver_url",
            "keyserver_token",
            "language",
            "decrypt_threads",
            "log_level",
        ] {
            s.set(key, format!("value-for-{key}"));
            assert_eq!(
                s.get(key),
                format!("value-for-{key}"),
                "{key} did not round-trip through get/set"
            );
        }
        // An unknown key is inert in both directions, never a panic.
        s.set("not_a_key", "x".into());
        assert_eq!(s.get("not_a_key"), "");

        for key in ["keep_iso", "auto_eject", "raw", "force"] {
            s.set_bool(key, true);
            assert!(s.get_bool(key), "{key} did not round-trip as true");
            s.set_bool(key, false);
            assert!(!s.get_bool(key), "{key} did not round-trip as false");
        }
        assert!(!s.get_bool("not_a_key"));

        // The bool keys must be genuinely distinct fields — a match arm that
        // reads the neighbouring field looks correct under a one-key test.
        let mut t = Settings::default();
        t.set_bool("raw", true);
        assert!(t.raw && !t.force && !t.keep_iso);
        let mut u = Settings::default();
        u.set_bool("force", true);
        assert!(u.force && !u.raw && !u.keep_iso);
    }
}
