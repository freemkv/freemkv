// freemkv — i18n string loader
// MIT — freemkv project

// Loader lives in the shared `freemkv-i18n` crate (reused by `autorip`); this
// is a thin re-export so existing call sites keep working unchanged.
pub use freemkv_i18n::*;

/// A catalog string with a compiled-in English fallback, for keys this crate
/// knows and the PINNED `freemkv-i18n` tag does not ship yet.
///
/// `Cargo.toml` resolves `freemkv-i18n` from a release TAG, so between wiring a
/// new string here and re-tagging i18n in the release cascade the key simply is
/// not in the catalog — and `freemkv_i18n::get` answers a miss by returning the
/// dotted path itself ("makes missing translations visible", which is right for
/// a translator and wrong for a user). `usage.url.mp4` in `--help`, or
/// `error.log_level_needs_value` on a mistyped flag, is strictly worse than
/// untranslated English: it is still not localized AND it is no longer
/// readable.
///
/// This existed twice already, hand-rolled at the call site — `ui::format_label`
/// and `pipe::title_changed_message` each wrote out the same
/// `match get(key) { s if s == key => .. }`. A second definition beside a call
/// site is how the CLI and the GUI grew different answers to one question in
/// the first place, so there is one now and both of those call it.
///
/// The fallback is a stopgap by construction: once the key ships in a tagged
/// catalog, the `english` argument stops being reachable and the call can
/// collapse to a plain [`get`]. Nothing enforces that cleanup, deliberately —
/// a test that failed the moment i18n was re-tagged would block the release
/// cascade it is meant to follow. What IS enforced is that the user never sees
/// a dotted path: the `usage_en` / `usage_de` / `usage_fr` goldens capture the
/// whole help text verbatim, so a key with a typo in it and a missing fallback
/// both show up as a golden diff.
pub fn get_or(key: &str, english: &str) -> String {
    match get(key) {
        s if s == key => english.to_string(),
        s => s,
    }
}

/// [`get_or`] with `{placeholder}` substitution — the fallback half of
/// [`fmt`]. The substitution runs on whichever text won, so the English
/// stopgap and the translation take the same arguments and cannot drift.
pub fn fmt_or(key: &str, english: &str, args: &[(&str, &str)]) -> String {
    let mut s = get_or(key, english);
    for (name, value) in args {
        s = s.replace(&format!("{{{name}}}"), value);
    }
    s
}

/// Strip control/escape characters from untrusted on-disc metadata (volume
/// label, title name, stream labels) before it is DISPLAYED.
///
/// Lives in `strings` because this is the only module BOTH targets compile:
/// `disc_info` (the CLI's, declared by `main.rs`) needs it, and so does
/// `engine` (declared by `lib.rs`, and by `main.rs` only on macOS). Putting it
/// in `engine` compiled on macOS and broke the Linux CLI build — the shells
/// and the CLI genuinely are separate compilations, and only `strings`
/// spans them.
///
/// Distinct from `engine::sanitize_label`, which makes a label safe as a
/// FILENAME component and is lossier on purpose. This one removes only what
/// cannot be safely rendered, so a Japanese or Cyrillic label survives whole.
pub fn sanitize_display(s: &str) -> String {
    s.chars().filter(|&c| !is_unsafe_display_char(c)).collect()
}

/// Characters to strip before display: C0/C1 controls (including ESC, and the
/// newlines that would let a crafted disc forge a whole log line) AND the
/// Unicode format (Cf) characters `char::is_control()` misses —
/// bidirectional overrides/isolates (U+202A-202E, U+2066-2069), zero-width
/// spaces/joiners (U+200B-200F, U+2060-2064) and the BOM (U+FEFF), which can
/// reorder or hide how the rest of a line renders.
pub fn is_unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}')
}
