//! FT2 (stop design §2.5 "Follow-ups use the held handle"): an eject ends the drive's
//! one handle through `DiscSession::finish(Finish::Eject)`, never a second open.
use super::{RunState, UiSink, eject_disc, eject_source_with};
use libfreemkv::test_util::{FakeHandle, FakeTransport};
use libfreemkv::{DiscSession, Drive, Halt};
use std::sync::Arc;

// SS-6 MMC-6 Table 633: LoEj 1, Start 0 = "Eject the disc if permitted".
fn is_eject(c: &[u8]) -> bool {
    c[0] == 0x1B && c[4] & 0x03 == 0x02
}

// A drive under `halt` whose token is already cancelled, as after a Stopped rip: a
// fresh `Drive::eject` is refused there, only `finish(Eject)` still reaches the tray.
fn stopped_drive() -> (Drive, FakeHandle) {
    let halt = Halt::new();
    let (t, fake) = FakeTransport::new();
    let t = t.watch(&halt).allow_after_cancel(is_eject);
    let drive = Drive::from_transport_with(Box::new(t), &halt);
    halt.cancel();
    (drive, fake)
}

#[test]
fn eject_disc_and_eject_source_use_held_handle() {
    let (drive, fake) = stopped_drive();
    let state = Arc::new(RunState::default());
    eject_disc(DiscSession::from_drive(drive), &UiSink(state.clone()));
    assert_eq!(
        fake.count(is_eject),
        1,
        "LoEj via finish: {:02x?}",
        fake.cdbs()
    );
    assert_eq!(fake.live_handles(), 0, "the held handle is closed");
    let lines = state
        .lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(lines.iter().any(|l| l.starts_with("ejected ")), "{lines:?}");

    let (drive, fake) = stopped_drive();
    let mut slot = Some(drive);
    let mut opens = 0;
    let res = eject_source_with("disc://", |_| {
        opens += 1;
        slot.take().ok_or_else(|| "opened twice".to_string())
    });
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(opens, 1, "one open");
    assert_eq!(
        fake.count(is_eject),
        1,
        "LoEj via finish: {:02x?}",
        fake.cdbs()
    );
    assert_eq!(fake.live_handles(), 0);
}
