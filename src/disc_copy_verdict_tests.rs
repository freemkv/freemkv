use super::*;

#[test]
fn the_no_data_error_names_the_unreadable_amount() {
    crate::strings::set_locale("en");
    let msg = iso_no_data_error(1_048_576);
    assert!(msg.contains("1.0"), "unreadable MB must appear: {msg}");
    // MiB with one decimal: 3.5 MiB, not 3.67 MB or 3670016 bytes.
    let msg = iso_no_data_error(3_670_016);
    assert!(msg.contains("3.5"), "{msg}");
}

#[test]
fn the_retry_hint_is_not_empty() {
    crate::strings::set_locale("en");
    assert!(retry_with_multipass_hint().contains("--multipass"));
}
