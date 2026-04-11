// freemkv — i18n string loader
// AGPL-3.0 — freemkv project
//
// English is compiled into the binary (always available).
// Other languages loaded from disk at runtime — drop a JSON file, done.
//
// Language priority:
//   1. --language flag (set via set_language() before init())
//   2. LANG / LC_MESSAGES env var
//   3. English fallback
//
// Search paths for locale files:
//   1. ./locales/xx.json (next to binary)
//   2. ~/.config/freemkv/locales/xx.json
//   3. /usr/share/freemkv/locales/xx.json
//
// To add a language: create locales/xx.json (copy en.json structure) and
// place it in any search path. No code changes needed.

use serde_json::Value;
use std::sync::OnceLock;

static STRINGS: OnceLock<Value> = OnceLock::new();
static LANG_OVERRIDE: OnceLock<String> = OnceLock::new();

// ── Shipped languages (auto-generated from locales/*.json by build.rs) ─────
include!(concat!(env!("OUT_DIR"), "/locales_generated.rs"));

/// Set language override from --language flag. Call before init().
pub fn set_language(lang: &str) {
    let _ = LANG_OVERRIDE.set(lang.to_string());
}

/// Initialize strings for the current locale.
/// Priority: bundled locale → disk locale → English fallback.
pub fn init() {
    let code = detect_language();
    let json = if let Some(data) = bundled_locale(&code) {
        // Shipped language — compiled in
        serde_json::from_str(data).expect("invalid bundled locale")
    } else if let Some(v) = load_locale_file(&code) {
        // Community language — loaded from disk
        v
    } else {
        // Fallback
        serde_json::from_str(LOCALE_EN).expect("invalid en.json")
    };
    let _ = STRINGS.set(json);
}

/// Get a string by dotted path (e.g. "disc.scanning", "error.no_drive").
/// Returns the path itself if not found — makes missing translations visible.
pub fn get(path: &str) -> String {
    let strings = STRINGS.get_or_init(|| serde_json::from_str(LOCALE_EN).expect("invalid en.json"));
    lookup(strings, path)
}

/// Get a string and replace {key} placeholders with values.
pub fn fmt(path: &str, args: &[(&str, &str)]) -> String {
    let mut s = get(path);
    for (key, val) in args {
        s = s.replace(&format!("{{{}}}", key), val);
    }
    s
}

// ── Internal ───────────────────────────────────────────────────────────────

fn detect_language() -> String {
    if let Some(lang) = LANG_OVERRIDE.get() {
        return normalize_code(lang);
    }
    for var in &["LC_MESSAGES", "LC_ALL", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            if !val.is_empty() && val != "C" && val != "POSIX" {
                return normalize_code(&val);
            }
        }
    }
    "en".to_string()
}

/// Try to load xx.json from search paths.
fn load_locale_file(code: &str) -> Option<Value> {
    let filename = format!("{}.json", code);

    // 1. Next to the binary
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = dir.join("locales").join(&filename);
            if let Some(v) = try_load(&path) {
                return Some(v);
            }
        }
    }

    // 2. Working directory
    let path = std::path::PathBuf::from("locales").join(&filename);
    if let Some(v) = try_load(&path) {
        return Some(v);
    }

    // 3. ~/.config/freemkv/locales/
    if let Ok(home) = std::env::var("HOME") {
        let path = std::path::PathBuf::from(home)
            .join(".config/freemkv/locales")
            .join(&filename);
        if let Some(v) = try_load(&path) {
            return Some(v);
        }
    }

    // 4. /usr/share/freemkv/locales/
    let path = std::path::PathBuf::from("/usr/share/freemkv/locales").join(&filename);
    if let Some(v) = try_load(&path) {
        return Some(v);
    }

    None
}

fn try_load(path: &std::path::Path) -> Option<Value> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// "fr_FR.UTF-8" → "fr"
fn normalize_code(s: &str) -> String {
    let s = s.trim().to_lowercase();
    if s.len() >= 2 {
        s[..2].to_string()
    } else {
        "en".to_string()
    }
}

fn lookup(strings: &Value, path: &str) -> String {
    let mut node = strings;
    for part in path.split('.') {
        match node.get(part) {
            Some(v) => node = v,
            None => return path.to_string(),
        }
    }
    match node.as_str() {
        Some(s) => s.to_string(),
        None => path.to_string(),
    }
}
