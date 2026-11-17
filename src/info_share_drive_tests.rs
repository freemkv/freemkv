//! `share_drive` over a scripted transport: no real drive, the profile lands in a temp dir.
use super::{DriveFlags, share_drive};
use libfreemkv::scsi::{DataDirection, ScsiResult, ScsiTransport};

// GOOD for every CDB. GET CONFIGURATION answers the requested feature with a 12-byte
// descriptor; READ BUFFER 0xF1 carries the Renesas `SAT` signature when `renesas`.
struct Scripted {
    renesas: bool,
}

impl ScsiTransport for Scripted {
    fn execute(
        &mut self,
        cdb: &[u8],
        _dir: DataDirection,
        data: &mut [u8],
        _timeout_ms: u32,
    ) -> libfreemkv::error::Result<ScsiResult> {
        data.fill(0);
        match cdb[0] {
            0x46 if data.len() >= 24 => {
                data[3] = 20;
                data[8] = cdb[2];
                data[9] = cdb[3];
                data[11] = 12;
                data[12..24].copy_from_slice(b"SERIAL123456");
            }
            0x3C if cdb[2] == 0xF1 && data.len() >= 19 => {
                data[..12].copy_from_slice(b"PIONEERSN001");
                if self.renesas {
                    data[16..19].copy_from_slice(b"SAT");
                }
            }
            _ => {}
        }
        Ok(ScsiResult {
            status: 0,
            bytes_transferred: data.len(),
            sense: [0u8; 32],
        })
    }
}

fn drive(renesas: bool) -> libfreemkv::Drive {
    let mut d = libfreemkv::Drive::from_transport(Box::new(Scripted { renesas }));
    d.drive_id = libfreemkv::DriveId {
        vendor_id: "TESTVEND".into(),
        product_id: "BD-FAKE".into(),
        product_revision: "1.00".into(),
        vendor_specific: "NM00".into(),
        firmware_date: "202103041200".into(),
        serial_number: "ABC123".into(),
        raw_inquiry: vec![0x05, 0x80, 0x05, 0x32, 0x5B, 0, 0, 0],
        raw_gc_010c: vec![0x01, 0x0C, 0x00, 0x10],
    };
    d
}

fn base(tag: &str) -> std::path::PathBuf {
    let b = std::env::temp_dir().join(format!("fmkv-share-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&b);
    std::fs::create_dir_all(&b).unwrap();
    b
}

// The one profile directory `share_drive` made under `base`.
fn profile_dir(base: &std::path::Path) -> std::path::PathBuf {
    let dirs: Vec<_> = std::fs::read_dir(base)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs[0].clone()
}

#[test]
fn without_share_only_the_identity_is_printed_and_nothing_is_written() {
    let b = base("plain");
    share_drive(&mut drive(false), DriveFlags::default(), &b);
    assert_eq!(std::fs::read_dir(&b).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&b);
}

#[test]
fn share_writes_the_profile_files_toml_and_zip() {
    let b = base("share");
    let flags = DriveFlags {
        share: true,
        ..DriveFlags::default()
    };
    share_drive(&mut drive(false), flags, &b);
    let dir = profile_dir(&b);
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    assert!(name.starts_with("testvend-bd-fake"), "{name}");
    for f in ["inquiry.bin", "drive.toml", "profile.zip", "rb_f1.bin"] {
        assert!(dir.join(f).is_file(), "missing {f}");
    }
    let toml = std::fs::read_to_string(dir.join("drive.toml")).unwrap();
    assert!(toml.contains("manufacturer = \"TESTVEND\""), "{toml}");
    assert!(toml.contains("serial = \"ABC123\""), "{toml}");
    assert!(toml.contains("[read_buffer]"), "{toml}");
    assert!(toml.contains("0xF1 = \"rb_f1.bin\""), "{toml}");
    // A plain (non-Renesas) drive is never knocked.
    assert!(!dir.join("wb_41.bin").exists());
    let _ = std::fs::remove_dir_all(&b);
}

#[test]
fn a_masked_renesas_share_hides_serials_and_records_the_knock_windows() {
    let b = base("renesas");
    let flags = DriveFlags {
        share: true,
        mask: true,
        quiet: true,
        verbose: false,
    };
    share_drive(&mut drive(true), flags, &b);
    let dir = profile_dir(&b);
    let toml = std::fs::read_to_string(dir.join("drive.toml")).unwrap();
    assert!(!toml.contains("ABC123"), "serial not masked: {toml}");
    for f in [
        "rb_b0_04.bin",
        "rb_b0_500000.bin",
        "rb_f4.bin",
        "wb_41.bin",
        "rb_b0_04_postknock.bin",
        "rb_b0_500000_postknock.bin",
    ] {
        assert!(toml.contains(f), "{f} not listed: {toml}");
    }
    let rb = std::fs::read(dir.join("rb_f1.bin")).unwrap();
    assert_ne!(&rb[..12], b"PIONEERSN001", "rb_f1 serial not masked");
    let serial = std::fs::read(dir.join("gc_0108.bin")).unwrap();
    assert!(
        !serial.windows(6).any(|w| w == b"SERIAL"),
        "GET_CONFIG 0108 serial not masked"
    );
    let _ = std::fs::remove_dir_all(&b);
}
