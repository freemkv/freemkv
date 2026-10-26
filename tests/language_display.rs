//! Presentation-language diagnostics share their rendering across the CLI and UI.

use freemkv::strings;

// Locale selection is process-global; keep all locale checks in one test.
#[test]
fn presentation_language_diagnostic_preserves_text_and_strips_controls() {
    for locale in strings::SHIPPED_CODES {
        strings::set_locale(locale);
        let ordinary = strings::presentation_language_unknown("xx-Invalid_日本語");
        assert!(
            ordinary.contains("xx-Invalid_日本語"),
            "{locale}: {ordinary}"
        );
        for unsafe_value in [
            "xx\u{1b}[31m",
            "xx\r\nforged\t\u{7f}\u{85}",
            "xx\u{202e}forged\u{2066}\u{2069}\u{feff}",
        ] {
            let rendered = strings::presentation_language_unknown(unsafe_value);
            assert!(
                !rendered.chars().any(strings::is_unsafe_display_char),
                "{locale}: {rendered:?}"
            );
        }
    }
}
