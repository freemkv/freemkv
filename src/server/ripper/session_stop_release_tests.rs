use super::{
    DriveSession, presence_from_status, release_stopped_drive, store_session, take_session,
};
use crate::server::ripper::{RipState, STATE};
use libfreemkv::test_util::FakeTransport;
use libfreemkv::{DiscPresence, Drive, DriveStatus};

fn held_session(dev: &str) -> libfreemkv::test_util::FakeHandle {
    let (t, fake) = FakeTransport::new();
    store_session(
        dev,
        DriveSession {
            drive: Drive::from_transport(Box::new(t)),
            disc: None,
            scanned: true,
            probed: false,
            tmdb: None,
            device_path: format!("/nonexistent/{dev}"),
            key_verdict: None,
            keys: None,
            key_error: None,
            key_reads: Default::default(),
        },
    );
    fake
}

fn set_claim_gen(dev: &str, claim_gen: u64) {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).insert(
        dev.to_string(),
        RipState {
            device: dev.to_string(),
            claim_gen,
            ..Default::default()
        },
    );
}

// A held drive answering through its own handle: only an empty or open tray is a removal.
#[test]
fn held_drive_status_maps_to_presence() {
    assert_eq!(
        presence_from_status(DriveStatus::DiscPresent),
        DiscPresence::Present
    );
    assert_eq!(
        presence_from_status(DriveStatus::NoDisc),
        DiscPresence::Absent
    );
    assert_eq!(
        presence_from_status(DriveStatus::TrayOpen),
        DiscPresence::Absent
    );
    assert_eq!(
        presence_from_status(DriveStatus::NotReady),
        DiscPresence::Settling
    );
    assert_eq!(
        presence_from_status(DriveStatus::Unknown),
        DiscPresence::Settling
    );
}

// Stop closes the idle session's handle, so macOS republishes the disc instead of the
// poller reading our own exclusive hold as an eject.
#[test]
fn stop_releases_the_held_drive() {
    let dev = format!("stop_release_{}", std::process::id());
    let fake = held_session(&dev);
    set_claim_gen(&dev, 7);
    release_stopped_drive(&dev, Some(7));
    assert!(take_session(&dev).is_none());
    assert_eq!(fake.live_handles(), 0, "the held handle is closed");
    STATE.lock().unwrap_or_else(|e| e.into_inner()).remove(&dev);
}

// A job claimed during the drain owns the drive now; Stop must not yank its session.
#[test]
fn stop_leaves_a_reclaimed_drive_alone() {
    let dev = format!("stop_reclaimed_{}", std::process::id());
    let fake = held_session(&dev);
    set_claim_gen(&dev, 8);
    release_stopped_drive(&dev, Some(7));
    assert!(take_session(&dev).is_some());
    assert_eq!(fake.live_handles(), 0);
    STATE.lock().unwrap_or_else(|e| e.into_inner()).remove(&dev);
}
