//! Process-global locale state, so this binary holds exactly one test.

use freemkv::{strings, ui};

// The preferred-language pickers and their summaries name each language in the UI
// language: under German they read "Deutsch, Englisch", not "German, English".
#[test]
fn picker_languages_are_named_in_the_active_locale() {
    strings::set_locale("de");
    for (code, english) in ui::PICKER_LANGUAGES {
        let key = format!("gui.lang.{code}");
        assert_ne!(strings::get(&key), key, "the catalog has no {key}");
        assert_eq!(ui::lang_display_name(code), strings::get(&key));
        assert_eq!(
            ui::lang_display_name(&code.to_uppercase()),
            strings::get(&key)
        );
        assert!(!english.is_empty());
    }
    let (deu, eng) = (ui::lang_display_name("deu"), ui::lang_display_name("eng"));
    assert_ne!(deu, "German", "German is still named in English");
    assert_eq!(ui::lang_summary("deu,eng"), format!("{deu}, {eng}"));
    // The stored form stays the ISO code whatever the UI language.
    assert_eq!(ui::lang_toggle("", "deu"), "deu");

    strings::set_locale("en");
    assert_eq!(ui::lang_summary("deu,eng"), "German, English");
}
