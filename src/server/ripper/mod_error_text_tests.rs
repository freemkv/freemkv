// Operator text for drive failures: every sense row, the non-SCSI labels, and the
// init-recovery guidance.
use super::*;

fn read_error(sense_key: u8, asc: u8) -> libfreemkv::Error {
    libfreemkv::Error::DiscRead {
        sector: 1_000_000,
        status: Some(2),
        sense: Some(libfreemkv::scsi::ScsiSense {
            sense_key,
            asc,
            ascq: 0,
        }),
    }
}

#[test]
fn every_sense_row_names_its_cause_and_action() {
    for (key, asc, cause) in [
        (3, 0x11, "bad sector"),
        (3, 0x02, "head positioning"),
        (3, 0x03, "head positioning"),
        (3, 0x31, "media error"),
        (4, 0x3E, "firmware unresponsive"),
        (4, 0x08, "drive hardware error"),
        (5, 0x24, "Invalid Field in CDB"),
        (5, 0x21, "drive rejected command"),
        (2, 0x04, "not ready"),
        (6, 0x29, "drive read error"),
    ] {
        let text = format_pass_error("Pass 2", &read_error(key, asc));
        assert!(
            text.starts_with("Pass 2 at 2.0 GB (sector 1000000) failed: "),
            "{text}"
        );
        assert!(text.contains(cause), "{key}/{asc:#x}: {text}");
        assert!(text.contains(" — "), "{text}");
    }
}

#[test]
fn non_scsi_failures_carry_a_short_label() {
    use libfreemkv::Error;
    for (e, label) in [
        (Error::Halted, "rip stopped by user"),
        (Error::DecryptFailed, "decryption failed"),
        (Error::NoStreams, "no playable streams"),
        (Error::DiscCapacityOverflow, "unusable disc capacity"),
        (Error::DiscCapacityMalformed, "unusable disc capacity"),
        (
            Error::DiscRead {
                sector: 0,
                status: None,
                sense: None,
            },
            "disc read error",
        ),
    ] {
        let text = pass1_last_error(&e);
        assert!(text.starts_with("Pass 1"), "{text}");
        assert!(text.contains(label), "{text}");
    }
    let io = format_pass_error(
        "Pass 3",
        &Error::IoError {
            source: std::io::Error::other("stale file handle"),
        },
    );
    assert_eq!(io, "Pass 3 failed: stale file handle");
    assert!(non_scsi_error_label(&Error::MplsParse).is_none());
    assert!(!pass1_last_error(&Error::MplsParse).is_empty());
}

#[test]
fn a_failed_reinit_names_a_wedged_firmware_or_the_error_category() {
    let device = format!("init-recovery-{}", std::process::id());
    let wedged = libfreemkv::Error::ScsiError {
        opcode: 0x12,
        status: 2,
        sense: Some(libfreemkv::scsi::ScsiSense {
            sense_key: 5,
            asc: 0x20,
            ascq: 0,
        }),
    };
    log_init_recovery_failure(&device, &wedged);
    let plain = libfreemkv::Error::ScsiError {
        opcode: 0x12,
        status: 2,
        sense: Some(libfreemkv::scsi::ScsiSense {
            sense_key: 2,
            asc: 0x04,
            ascq: 0,
        }),
    };
    log_init_recovery_failure(&device, &plain);
    log_init_recovery_failure(&device, &libfreemkv::Error::MplsParse);
    let log = crate::server::log::get_device_log(&device, 100).join("\n");
    for text in [
        "drive firmware wedged",
        "USER_ACTION_REQUIRED",
        "category=SCSI_ERROR",
        "category=ERROR_CODE_",
    ] {
        assert!(log.contains(text), "missing {text}: {log}");
    }
    crate::server::log::forget_device(&device);
}

#[test]
fn audio_purposes_render_their_english_tags() {
    use libfreemkv::LabelPurpose;
    assert_eq!(audio_purpose_tag(LabelPurpose::Normal), None);
    assert_eq!(
        audio_purpose_tag(LabelPurpose::Commentary),
        Some("Commentary")
    );
    assert_eq!(
        audio_purpose_tag(LabelPurpose::Descriptive),
        Some("Descriptive Audio")
    );
    assert_eq!(audio_purpose_tag(LabelPurpose::Score), Some("Score"));
    assert_eq!(audio_purpose_tag(LabelPurpose::Ime), Some("IME"));
}

#[test]
fn a_drive_that_never_reopens_exhausts_its_backoff() {
    let device = format!("backoff-exhausted-{}", std::process::id());
    let path = format!("/nonexistent/{device}");
    assert!(open_drive_with_backoff(&device, 2, &path, 0).is_none());
    let log = crate::server::log::get_device_log(&device, 100).join("\n");
    assert_eq!(log.matches("retrying in 0s").count(), 2, "{log}");
    assert!(log.contains("recovery path exhausted"), "{log}");
    assert!(log.contains("FAILED at Drive::open category="), "{log}");
    crate::server::log::forget_device(&device);
}
