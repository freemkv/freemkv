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

#[test]
fn a_kept_key_file_is_named_and_a_renamed_one_is_silent() {
    use libfreemkv::whole_disc::{DecryptedNames, NamesKept};
    crate::strings::set_locale("en");
    let kept = |cause| DecryptedNames::Kept { cause };
    let renamed = DecryptedNames::Renamed {
        mirror_sectors_restored: 1,
    };
    assert_eq!(names_note(None, true), None);
    assert_eq!(names_note(Some(&renamed), true), None);
    assert_eq!(names_note(Some(&renamed), false), None);
    for iso in [true, false] {
        for cause in [
            NamesKept::Damaged,
            NamesKept::Collision,
            NamesKept::Unexpected,
        ] {
            let note = names_note(Some(&kept(cause)), iso).expect("a kept name is reported");
            assert!(
                note.contains("Unit_Key_RO.inf") && note.contains("VLC"),
                "{note}"
            );
            // Only a damaged ISO can be renamed by a later run.
            let retry = iso && cause == NamesKept::Damaged;
            assert_eq!(
                note.contains("Unit_Key_RO.bak"),
                retry,
                "{cause:?} iso={iso}: {note}"
            );
        }
    }
}
