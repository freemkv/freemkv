// freemkv — i18n string loader
// AGPL-3.0 — freemkv project
//
// All user-facing text loaded from locales/*.json files.
// Files are compiled into the binary via include_str!().
// Language selected by LANG env var at startup.
//
// To add a language: copy locales/en.json → locales/fr.json and translate.

use serde_json::Value;
use std::sync::OnceLock;

static STRINGS: OnceLock<Value> = OnceLock::new();

/// English strings — compiled into the binary.
const EN_JSON: &str = include_str!("../locales/en.json");

/// Initialize strings for the current locale.
/// Call once at startup. Falls back to English if locale not found.
pub fn init() {
    let json: Value = serde_json::from_str(EN_JSON).expect("invalid en.json");
    let _ = STRINGS.set(json);
}

/// Get a string by dotted path (e.g. "disc.scanning", "error.no_drive").
/// Returns the path itself if not found — makes missing translations visible.
pub fn get(path: &str) -> String {
    let strings = STRINGS.get_or_init(|| {
        serde_json::from_str(EN_JSON).expect("invalid en.json")
    });

    let parts: Vec<&str> = path.split('.').collect();
    let mut node = strings;
    for part in &parts {
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

/// Get a string and replace {key} placeholders with values.
/// Example: fmt("rip.starting", &[("num", "1")]) → "Ripping title 1"
pub fn fmt(path: &str, args: &[(&str, &str)]) -> String {
    let mut s = get(path);
    for (key, val) in args {
        s = s.replace(&format!("{{{}}}", key), val);
    }
    s
}
