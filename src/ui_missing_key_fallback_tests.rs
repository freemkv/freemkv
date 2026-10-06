// A key this crate knows but the pinned i18n tag does not ship must render as readable
// English, never as the dotted path.
#[test]
fn a_key_absent_from_the_pinned_catalog_falls_back_to_the_canonical_text() {
    // `strings::get` echoes the path for an unknown key; that echo is the
    // exact condition the guard keys on.
    let unknown = "gui.format.__not_in_any_catalog__";
    assert_eq!(crate::strings::get(unknown), unknown);
    assert_eq!(crate::strings::get_or(unknown, "MKV"), "MKV");
}

/// Every offered format renders as something a human can read: never empty,
/// and never the raw key path.
#[test]
fn every_offered_format_renders_readable_text() {
    for group in super::output_formats(true, true) {
        for canonical in group {
            let shown = super::format_label(canonical);
            assert!(!shown.is_empty(), "{canonical} rendered empty");
            assert!(
                !shown.starts_with("gui."),
                "{canonical} rendered the raw key path {shown:?}"
            );
        }
    }
}
