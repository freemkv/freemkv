//! Process-global locale state, so this binary holds exactly one test.

use freemkv::{app_entry, strings};

// A GUI launched in an explicit language must still honour a live switch to
// "Auto": the startup language may not pin itself for the whole session.
#[test]
fn a_live_switch_to_auto_is_not_pinned_by_the_startup_language() {
    // Startup order: the saved language is applied before any lookup.
    app_entry::apply_locale("Italiano", || None);
    let italian = strings::get("gui.btn.cancel");
    strings::set_locale("en");
    assert_ne!(
        italian,
        strings::get("gui.btn.cancel"),
        "no Italian catalog"
    );

    let env_is_italian = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .any(|v| std::env::var(v).is_ok_and(|s| s.starts_with("it")));
    if env_is_italian {
        return;
    }
    strings::set_locale("auto");
    assert_ne!(
        strings::get("gui.btn.cancel"),
        italian,
        "Auto stayed Italian"
    );
}
