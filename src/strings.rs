// freemkv — i18n string loader. MIT — freemkv project.

// Loader lives in the shared `freemkv-i18n` crate (reused by `autorip`); this
// is a thin re-export so existing call sites keep working unchanged.
pub use freemkv_i18n::*;

/// A catalog string with a compiled-in English fallback, for keys this crate
/// knows that the pinned `freemkv-i18n` tag does not ship yet.
///
/// `freemkv_i18n::get` answers a catalog miss by returning the dotted path
/// itself, which is right for a translator and wrong for a user; `get_or`
/// returns `english` instead whenever `key` did not resolve. Once the key
/// ships in a tagged catalog, the `english` argument stops being reachable
/// and the call can collapse to a plain [`get`].
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

/// Localized sentence for an engine selection-review reason key
/// (`SelectionReviewReason::key()`), or `no-titles`. An unknown key is shown
/// as-is rather than hidden.
pub fn review_reason_text(key: &str) -> String {
    let english = match key {
        "missing-episode-roster" => "the disc has no authored episode list",
        "incomplete-episode-roster" => "the disc's authored episode list is incomplete",
        "conflicting-episode-roster" => "the disc's authored episode lists conflict",
        "invalid-episode-roster" => "the disc's authored episode list is invalid",
        "incomplete-launch-proof" => "the disc's launch sequence is incomplete",
        "invalid-launch-proof" => "the disc's launch sequence is invalid",
        "missing-presentation-language" => "the disc has no presentation in the requested language",
        "invalid-presentation-language" => "the presentation language is not valid",
        "unmatched-presentation-language" => "no presentation matches the presentation language",
        "ambiguous-presentation-language" => {
            "more than one presentation matches the presentation language"
        }
        "no-titles" => "the disc has no usable titles",
        other => return other.to_string(),
    };
    get_or(
        &format!("error.selection_review_reason.{}", key.replace('-', "_")),
        english,
    )
}

// Lives here (not in `engine`) because this is the only module both the CLI
// (`disc_info`) and `engine` compile — putting it in `engine` broke the
// Linux CLI build, since `engine` is macOS-only under `main.rs`.
/// Strip control/escape characters from untrusted on-disc metadata (volume
/// label, title name, stream labels) before it is displayed.
///
/// Distinct from `engine::sanitize_label`, which makes a label safe as a
/// filename component and is lossier on purpose. This one removes only what
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
