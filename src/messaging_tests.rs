use super::*;

#[test]
fn every_code_is_error_level() {
    // Locked: every libfreemkv code is a terminal failure render.
    for code in [1000u16, 2000, 3000, 5000, 6009, 7022, 8001, 9023] {
        assert_eq!(level_for(code), Level::Error);
    }
}

#[test]
fn level_locale_keys_are_distinct() {
    assert_ne!(Level::Warn.locale_key(), Level::Info.locale_key());
    assert_ne!(Level::Info.locale_key(), Level::Error.locale_key());
    assert_ne!(Level::Warn.locale_key(), Level::Error.locale_key());
}
