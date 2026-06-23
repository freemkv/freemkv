// freemkv — i18n string loader
// AGPL-3.0 — freemkv project
//
// English is compiled into the binary (always available).
// Other languages loaded from disk at runtime — drop a JSON file, done.
//
// Language priority:
//   1. --language flag (set via set_language() before init())
//   2. LC_ALL / LC_MESSAGES / LANG env var (POSIX precedence)
//   3. English fallback
//
// Search paths for locale files:
//   1. <binary dir>/locales/xx.json (next to the binary)
//   2. ./locales/xx.json (working directory)
//   3. ~/.config/freemkv/locales/xx.json
//   4. /usr/share/freemkv/locales/xx.json
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
///
/// Once `init()`/`get()` has locked in the active locale (`STRINGS`), this
/// override is dead: the language is already chosen. A call at that point is a
/// caller-ordering bug, so make it visible instead of silently no-opping.
pub fn set_language(lang: &str) {
    if STRINGS.get().is_some() {
        debug_assert!(
            false,
            "set_language(\"{lang}\") called after strings were initialized; the override is ignored"
        );
        eprintln!("warning: --language ignored (set after locale was initialized)");
        return;
    }
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
    // POSIX precedence: LC_ALL overrides every other locale variable, then
    // the category-specific LC_MESSAGES, then LANG as the fallback default.
    for var in &["LC_ALL", "LC_MESSAGES", "LANG"] {
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
    // A missing file is fine (the loader tries the next search path), but a
    // file that exists yet fails to parse is almost certainly an operator
    // mistake — surface it instead of silently falling back to English.
    match serde_json::from_str(&data) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!(
                "freemkv: ignoring invalid locale file {}: {}",
                path.display(),
                e
            );
            None
        }
    }
}

/// "fr_FR.UTF-8" → "fr". Inputs are untrusted (the `--language` CLI flag and
/// the `LC_*`/`LANG` env vars), so this must never panic. The two-letter code is
/// taken by *character*, not byte, and validated as ASCII letters — anything
/// else (multibyte leading chars, digits, punctuation) falls back to English.
fn normalize_code(s: &str) -> String {
    // Strip any territory/codeset/modifier suffix (`fr_FR.UTF-8@euro` → `fr`)
    // before taking the language part.
    let lang = s
        .trim()
        .split(['_', '.', '-', '@', ':'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    let code: String = lang.chars().take(2).collect();
    if code.chars().count() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        code
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect all dotted key paths from a JSON value (e.g. "app.usage", "error.E1000").
    fn collect_keys(value: &Value, prefix: &str, out: &mut Vec<String>) {
        if let Some(obj) = value.as_object() {
            for (k, v) in obj {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                if v.is_object() {
                    collect_keys(v, &path, out);
                } else {
                    out.push(path);
                }
            }
        }
    }

    fn verify_locale(code: &str, data: &str) {
        let locale: Value = serde_json::from_str(data)
            .unwrap_or_else(|e| panic!("{}.json: invalid JSON: {}", code, e));

        let en: Value = serde_json::from_str(LOCALE_EN).unwrap();
        let mut en_keys = Vec::new();
        collect_keys(&en, "", &mut en_keys);

        let mut locale_keys = Vec::new();
        collect_keys(&locale, "", &mut locale_keys);

        // Every English key must exist in the locale
        let mut missing = Vec::new();
        for key in &en_keys {
            if !locale_keys.contains(key) {
                missing.push(key.clone());
            }
        }
        assert!(
            missing.is_empty(),
            "{}.json missing {} keys: {:?}",
            code,
            missing.len(),
            missing
        );

        // Every {placeholder} in English must appear in the translation
        for key in &en_keys {
            let en_val = lookup(&en, key);
            let locale_val = lookup(&locale, key);
            for placeholder in placeholders(&en_val) {
                assert!(
                    locale_val.contains(&placeholder),
                    "{}.json key '{}': missing placeholder {} (got: '{}')",
                    code,
                    key,
                    placeholder,
                    locale_val
                );
            }
        }
    }

    /// Extract `{word}` placeholders from a format string, matching exactly how
    /// `fmt` substitutes them: a balanced single `{...}` with no nested braces.
    /// Escaped/doubled braces (`{{`, `}}`) are skipped so a literal `{{val}}`
    /// does not register a malformed `{{val}` placeholder.
    fn placeholders(s: &str) -> Vec<String> {
        let bytes = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'{' {
                // Doubled `{{` is an escape, not a placeholder — skip both.
                if bytes.get(i + 1) == Some(&b'{') {
                    i += 2;
                    continue;
                }
                if let Some(rel_end) = s[i + 1..].find('}') {
                    let inner = &s[i + 1..i + 1 + rel_end];
                    // A real placeholder has no nested brace inside it.
                    if !inner.contains('{') {
                        out.push(format!("{{{}}}", inner));
                    }
                    i += 1 + rel_end + 1;
                    continue;
                }
            }
            i += 1;
        }
        out
    }

    #[test]
    fn normalize_code_does_not_panic_on_multibyte() {
        // Regression: byte-slicing `s[..2]` panicked on a leading multibyte
        // char (e.g. `--language あ`, `LC_ALL=€a`). Untrusted input must never
        // panic — it must fall back to English (or the ASCII language part).
        for input in ["あx", "€a", "Ⓐb", "😀x", "あ", "", ".", "_", "@", "ñ"] {
            let code = normalize_code(input);
            assert!(
                code == "en" || (code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic())),
                "normalize_code({input:?}) = {code:?}: must be 'en' or a 2-letter ASCII code"
            );
        }
    }

    #[test]
    fn normalize_code_extracts_language_part() {
        assert_eq!(normalize_code("fr_FR.UTF-8"), "fr");
        assert_eq!(normalize_code("de"), "de");
        assert_eq!(normalize_code("PT_BR"), "pt");
        assert_eq!(normalize_code("en-US"), "en");
        assert_eq!(normalize_code("es.UTF-8@modifier"), "es");
        // Non-letters fall back rather than producing a bogus code.
        assert_eq!(normalize_code("12"), "en");
        assert_eq!(normalize_code("x"), "en");
    }

    #[test]
    fn placeholders_skips_doubled_braces() {
        assert_eq!(placeholders("hi {name}!"), vec!["{name}".to_string()]);
        assert_eq!(placeholders("a {x} b {y}"), vec!["{x}", "{y}"]);
        // Doubled braces are escapes, not placeholders.
        assert!(placeholders("{{val}}").is_empty());
        assert_eq!(placeholders("{{lit}} {real}"), vec!["{real}".to_string()]);
        assert!(placeholders("no placeholders").is_empty());
    }

    #[test]
    fn locale_en_loads() {
        let _: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");
    }

    #[test]
    fn open_failed_key_exists_and_fills_placeholders() {
        // Regression: `error.open_failed` was referenced by the drive-info /
        // CLI device-open path but never defined in any locale. A missing key
        // makes `lookup` return the dotted path verbatim, so `fmt` had no
        // `{device}`/`{error}` to substitute and the user saw the bare string
        // "error.open_failed". Guard that the key exists with both placeholders.
        let en: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");
        let val = lookup(&en, "error.open_failed");
        assert_ne!(
            val, "error.open_failed",
            "error.open_failed missing from en.json (lookup returned the key verbatim)"
        );
        let ph = placeholders(&val);
        assert!(
            ph.contains(&"{device}".to_string()) && ph.contains(&"{error}".to_string()),
            "error.open_failed must contain {{device}} and {{error}} placeholders, got: '{}'",
            val
        );

        // And the full fmt() path produces a clean message with the values
        // substituted and no leftover placeholders or bare key.
        let out = fmt(
            "error.open_failed",
            &[("device", "/dev/sg0"), ("error", "permission denied")],
        );
        assert!(
            out.contains("/dev/sg0"),
            "device not substituted: '{}'",
            out
        );
        assert!(
            out.contains("permission denied"),
            "error not substituted: '{}'",
            out
        );
        assert!(!out.contains("{device}") && !out.contains("{error}"));
        assert_ne!(out, "error.open_failed");
    }

    #[test]
    fn locale_es_loads() {
        verify_locale("es", LOCALE_ES);
    }

    #[test]
    fn locale_fr_loads() {
        verify_locale("fr", LOCALE_FR);
    }

    #[test]
    fn locale_de_loads() {
        verify_locale("de", LOCALE_DE);
    }

    #[test]
    fn locale_it_loads() {
        verify_locale("it", LOCALE_IT);
    }

    #[test]
    fn locale_pt_loads() {
        verify_locale("pt", LOCALE_PT);
    }

    #[test]
    fn locale_nl_loads() {
        verify_locale("nl", LOCALE_NL);
    }

    /// Regression guard: EVERY error code that libfreemkv can produce must have
    /// an `error.E<code>` string in en.json. Without a string, the CLI's
    /// `fmt_err` falls back to the generic wrapper and the user sees a raw
    /// `E<code>: <internal data>`. We construct one instance of every `Error`
    /// variant, collect its `.code()`, and assert the locale key exists.
    ///
    /// `Error` is `#[non_exhaustive]`, so this list must be kept in sync by hand
    /// when a new variant is added — that is the point: a new code with no
    /// string trips this test instead of shipping a bare code to users. The
    /// companion `all_error_code_constants_are_unique` test in libfreemkv pins
    /// the constant set; this one pins locale coverage of it.
    #[test]
    fn every_error_code_has_an_en_string() {
        use libfreemkv::Error;
        let en: Value = serde_json::from_str(LOCALE_EN).expect("en.json invalid");

        let p = || "p".to_string();
        let variants: Vec<Error> = vec![
            Error::DeviceNotFound { path: p() },
            Error::DevicePermission { path: p() },
            Error::DeviceNotReady { path: p() },
            Error::DeviceResetFailed { path: p() },
            Error::ScsiInterfaceUnavailable { path: p() },
            Error::DeviceLocked { path: p(), kr: 0 },
            Error::IoKitPluginFailed { path: p(), kr: 0 },
            Error::UnsupportedDrive {
                vendor_id: p(),
                product_id: p(),
                product_revision: p(),
            },
            Error::ProfileParse,
            Error::UnsupportedPlatform { target: p() },
            Error::PlatformNotImplemented { platform: p() },
            Error::UnlockFailed,
            Error::SignatureMismatch {
                expected: [0; 4],
                got: [0; 4],
            },
            Error::ScsiError {
                opcode: 0,
                status: 0,
                sense: None,
            },
            Error::InvalidCdbLength { len: 0, max: 0 },
            Error::IoError {
                source: std::io::Error::from_raw_os_error(13),
            },
            Error::DiscRead {
                sector: 0,
                status: None,
                sense: None,
            },
            Error::Halted,
            Error::MplsParse,
            Error::ClpiParse,
            Error::UdfNotFound { path: p() },
            Error::UdfBufferTooSmall,
            Error::DiscTitleRange { index: 0, count: 0 },
            Error::IfoParse,
            Error::MkvInvalid,
            Error::NoStreams,
            Error::MapfileInvalid { kind: "hex" },
            Error::AacsNoKeys,
            Error::AacsCertShort,
            Error::AacsAgidAlloc,
            Error::AacsCertRejected,
            Error::AacsCertRead,
            Error::AacsCertVerify,
            Error::AacsKeyRead,
            Error::AacsKeyRejected,
            Error::AacsKeyVerify,
            Error::AacsVidRead,
            Error::AacsVidMac,
            Error::AacsDataKey,
            Error::DecryptFailed,
            Error::CssAuthFailed,
            Error::AacsHostCertRejected,
            Error::AacsRawReadUnsupported,
            Error::AacsVidUnavailable,
            Error::AacsMkUnavailable,
            Error::AacsVukNotInKeydb,
            Error::DriveProfileMissing,
            Error::VidCdbUnavailable,
            Error::NoDiscKey { disc_hash: p() },
            Error::CssKeyMissing,
            Error::AacsNoHostCert { path: p() },
            Error::KeydbConnect { host: p() },
            Error::KeydbHttp { status: 0 },
            Error::KeydbInvalid,
            Error::KeydbWrite { path: p() },
            Error::KeydbParse,
            Error::KeydbLoad { path: p() },
            Error::KeydbUnsupportedScheme { scheme: p() },
            Error::KeydbTooManyRedirects,
            Error::StreamReadOnly,
            Error::StreamWriteOnly,
            Error::StreamUrlInvalid { url: p() },
            Error::StreamUrlMissingPath { scheme: p() },
            Error::StreamUrlMissingPort { addr: p() },
            Error::NetworkAddrBlocked { addr: p() },
            Error::MuxEmpty,
            Error::PesFrameTooLarge { size: 0 },
            Error::PesInvalidMagic,
            Error::PesTrackTooLarge { track: 0 },
            Error::IsoTooLarge { path: p() },
            Error::NoMetadata,
            Error::DiscUrlNotDirect,
            Error::HevcParamParse,
            Error::MuxTrackRange {
                track: 0,
                tracks: 0,
            },
            Error::Fmp4Unimplemented,
            Error::DemuxThreadPanicked,
            Error::PipelineJoinTimeout,
            Error::PipelineConsumerPanicked,
            Error::SweepConsumerGone,
            Error::PipelineConsumerGone,
            Error::DiscCapacityOverflow,
            Error::ExtentNotUnitAligned,
            Error::M2tsPacketMalformed,
            Error::DiscCapacityMalformed,
        ];

        // E9012 (Fmp4Unimplemented) and E9021 (M2tsPacketMalformed) are internal
        // invariants the CLI never surfaces to a user, but we still want a string
        // so the generic wrapper never echoes a raw code. Every variant above is
        // checked uniformly — no exceptions.
        let mut missing = Vec::new();
        for e in &variants {
            let key = format!("error.E{}", e.code());
            let got = lookup(&en, &key);
            if got == key {
                missing.push(key);
            }
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "en.json is missing {} error code string(s): {:?}",
            missing.len(),
            missing
        );
    }
}
