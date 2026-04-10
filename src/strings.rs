// freemkv — i18n string loader
// AGPL-3.0 — freemkv project
//
// All user-facing text loaded from locales/*.json files.
// Files are compiled into the binary via include_str!().
//
// Language priority:
//   1. --language flag (set via set_language() before init())
//   2. LANG / LC_MESSAGES env var
//   3. English fallback
//
// To add a language: copy locales/en.json → locales/xx.json and translate.
// The new file is compiled in automatically via the match below.

use serde_json::Value;
use std::sync::OnceLock;

static STRINGS: OnceLock<Value> = OnceLock::new();
static LANG_OVERRIDE: OnceLock<String> = OnceLock::new();

// ── Compiled-in locale files ───────────────────────────────────────────────

const EN_JSON: &str = include_str!("../locales/en.json");
const ES_JSON: &str = include_str!("../locales/es.json");

fn locale_data(code: &str) -> &'static str {
    match code {
        "es" => ES_JSON,
        _ => EN_JSON,
    }
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Set language override from --language flag. Call before init().
pub fn set_language(lang: &str) {
    let _ = LANG_OVERRIDE.set(lang.to_string());
}

/// Initialize strings for the current locale.
/// Call once at startup after set_language() (if any).
pub fn init() {
    let code = detect_language();
    let data = locale_data(&code);
    let json: Value = serde_json::from_str(data).expect("invalid locale json");
    let _ = STRINGS.set(json);
}

/// Get a string by dotted path (e.g. "disc.scanning", "error.no_drive").
/// Returns the path itself if not found — makes missing translations visible.
pub fn get(path: &str) -> String {
    let strings = STRINGS.get_or_init(|| {
        serde_json::from_str(EN_JSON).expect("invalid en.json")
    });
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
    // 1. Explicit override from --language flag
    if let Some(lang) = LANG_OVERRIDE.get() {
        return normalize_code(lang);
    }

    // 2. Environment: LC_MESSAGES, LC_ALL, LANG
    for var in &["LC_MESSAGES", "LC_ALL", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            if !val.is_empty() && val != "C" && val != "POSIX" {
                return normalize_code(&val);
            }
        }
    }

    // 3. English fallback
    "en".to_string()
}

/// Extract 2-letter language code from locale string.
/// "fr_FR.UTF-8" → "fr", "es" → "es", "en_US" → "en"
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
