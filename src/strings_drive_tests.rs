//! Drive naming: `"{name}  VENDOR MODEL (rev FW)"`, plus the path when it differs from the name.
use super::*;

fn drive(path: &str, display_name: &str) -> libfreemkv::DriveInfo {
    libfreemkv::DriveInfo {
        path: path.into(),
        display_name: display_name.into(),
        vendor: "HL-DT-ST".into(),
        model: "BD-RE BU40N".into(),
        firmware: "1.04".into(),
    }
}

#[test]
fn label_is_vendor_model_and_revision() {
    assert_eq!(
        drive_label(&drive("/dev/sg1", "/dev/sr1")),
        "HL-DT-ST BD-RE BU40N (rev 1.04)"
    );
}

#[test]
fn label_replaces_control_characters() {
    let mut d = drive("/dev/sg1", "/dev/sr1");
    d.vendor = "EV\u{1b}IL".into();
    d.firmware = "1\n2".into();
    assert_eq!(drive_label(&d), "EV\u{fffd}IL BD-RE BU40N (rev 1\u{fffd}2)");
}

#[test]
fn display_has_two_spaces_between_name_and_label() {
    assert_eq!(
        drive_display(&drive(r"\\.\CdRom0", "E:")),
        "E:  HL-DT-ST BD-RE BU40N (rev 1.04)"
    );
}

#[test]
fn list_line_adds_the_path_only_when_it_differs_from_the_name() {
    assert_eq!(
        drive_list_line(&drive(r"\\.\CdRom0", "E:")),
        r"E:  HL-DT-ST BD-RE BU40N (rev 1.04)  \\.\CdRom0"
    );
    assert_eq!(
        drive_list_line(&drive("/dev/sr0", "/dev/sr0")),
        "/dev/sr0  HL-DT-ST BD-RE BU40N (rev 1.04)"
    );
}

#[test]
fn identical_drives_are_told_apart_by_name() {
    assert_ne!(
        drive_display(&drive(r"\\.\CdRom0", "E:")),
        drive_display(&drive(r"\\.\CdRom1", "F:"))
    );
}
