use super::{DriveSession, eject_drive, register_halt, store_session, take_session};
use libfreemkv::test_util::FakeTransport;
use libfreemkv::{Drive, Halt};

// SS-6 MMC-6 Table 633: LoEj 1, Start 0 = "Eject the disc if permitted".
fn is_eject(c: &[u8]) -> bool {
    c[0] == 0x1B && c[4] & 0x03 == 0x02
}

// FT2 (stop design §2.5): `/api/eject` on a device whose idle session holds the drive
// ejects through that handle's `finish`; the device path cannot be opened a second time.
#[test]
fn eject_drive_uses_the_held_session_handle() {
    let dev = format!("ft2_eject_{}", std::process::id());
    let halt = Halt::new();
    let (t, fake) = FakeTransport::new();
    let session = DriveSession {
        drive: Drive::from_transport(Box::new(t)),
        disc: None,
        scanned: false,
        probed: false,
        tmdb: None,
        device_path: format!("/nonexistent/{dev}"),
        key_verdict: None,
        keys: None,
        key_error: None,
        key_reads: Default::default(),
    };
    store_session(&dev, session);
    register_halt(&dev, halt.clone());
    let _ = eject_drive(&format!("/nonexistent/{dev}"));
    assert!(halt.is_cancelled());
    assert_eq!(fake.count(is_eject), 1, "{:02x?}", fake.cdbs());
    assert_eq!(fake.live_handles(), 0, "the held handle is closed");
    assert!(take_session(&dev).is_none());
}

#[test]
fn a_not_ready_reopen_after_a_failed_first_eject_counts_as_ejected() {
    use super::eject_verdict;
    let not_ready = || libfreemkv::Error::DeviceNotReady { path: "x".into() };
    assert!(eject_verdict(true, Err(not_ready())).is_ok());
    // Without a failed first eject, not ready is just a failure.
    assert!(eject_verdict(false, Err(not_ready())).is_err());
    // Other errors stay errors.
    let gone = libfreemkv::Error::DeviceNotFound { path: "x".into() };
    assert_eq!(eject_verdict(true, Err(gone)).unwrap_err().code(), 1000);
    assert!(eject_verdict(true, Ok(())).is_ok());
}
