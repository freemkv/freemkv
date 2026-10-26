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

/// Localized sentence for an engine selection-review reason. Exhaustive over the engine's
/// enum, so a new reason fails to compile until it has text.
// The GUI and server render reviews; the CLI binary compiles this module but has none to show.
#[allow(dead_code)]
pub fn review_reason_text(reason: freemkv_engine::SelectionReviewReason) -> String {
    use freemkv_engine::SelectionReviewReason as R;
    let english = match reason {
        R::MissingEpisodeRoster => "the disc has no authored episode list",
        R::IncompleteEpisodeRoster => "the disc's authored episode list is incomplete",
        R::ConflictingEpisodeRoster => "the disc's authored episode lists conflict",
        R::InvalidEpisodeRoster => "the disc's authored episode list is invalid",
    };
    get_or(
        &format!(
            "error.selection_review_reason.{}",
            reason.key().replace('-', "_")
        ),
        english,
    )
}

/// The one rendering of a selection that needs review: `E9086 <localized reason>`.
#[allow(dead_code)]
pub fn selection_review_text(review: &freemkv_engine::SelectionReview) -> String {
    format!(
        "E{} {}",
        libfreemkv::error::E_SELECTION_REVIEW_REQUIRED,
        review_reason_text(review.reason)
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

/// Whether a typed presentation language is usable: blank (no preference) or a language tag
/// title selection understands. `all` and `none` are audio/subtitle words; as ISO 639-3 `all`
/// is "Allar", so both are refused here.
#[allow(dead_code)]
pub fn is_presentation_language(tag: &str) -> bool {
    let t = tag.trim();
    t.is_empty()
        || (!t.eq_ignore_ascii_case("all")
            && !t.eq_ignore_ascii_case("none")
            // ISO 639-2 special codes: not languages a title can be in.
            && !["und", "mul", "zxx", "mis"]
                .iter()
                .any(|c| t.eq_ignore_ascii_case(c))
            && freemkv_engine::is_language_tag(t))
}

/// The refusal text for a presentation language that is not a language. Not `E9083`: its
/// `all/none` advice does not apply to this field.
#[allow(dead_code)]
pub fn presentation_language_unknown(value: &str) -> String {
    fmt_or(
        "error.presentation_language_unknown",
        "Unknown presentation language \"{value}\". Use an ISO 639 language code such as en, de or fra.",
        &[("value", &sanitize_display(value))],
    )
}
