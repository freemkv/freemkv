use super::{PollAction, poll_action};
use libfreemkv::DiscPresence::{self, Absent, Present, Settling};

// Feeds a presence sequence for one drive through the poll loop's per-tick decision;
// returns (dispatches, session removals).
fn run(seq: &[DiscPresence]) -> (usize, usize) {
    let (mut had, mut dispatched, mut removed) = (false, 0, 0);
    for &p in seq {
        let a = poll_action(p, had, false);
        match a {
            PollAction::Present(t) if t.dispatch => dispatched += 1,
            PollAction::Absent { removed: true } => removed += 1,
            _ => {}
        }
        had = a.latch();
    }
    (dispatched, removed)
}

#[test]
fn a_disc_spinning_up_after_a_usb_reset_keeps_its_session() {
    let (dispatched, removed) = run(&[Present, Settling, Settling, Present, Present]);
    assert_eq!(dispatched, 1, "the re-spin must not re-trigger a rip");
    assert_eq!(removed, 0, "the re-spin must not drop the session");
}

#[test]
fn an_empty_tray_closing_never_dispatches_a_rip() {
    assert_eq!(run(&[Absent, Settling, Settling, Absent, Absent]), (0, 0));
}

#[test]
fn an_absent_drive_never_rips() {
    assert_eq!(run(&[Absent, Absent, Absent]), (0, 0));
}

// A drive recovering from a wedge may answer Settling first; its stale
// "firmware unresponsive" tile must not outlive the recovery.
#[test]
fn a_settling_drive_with_no_known_disc_shows_idle() {
    assert!(poll_action(Settling, false, false).shows_idle());
    assert!(
        !poll_action(Settling, true, false).shows_idle(),
        "a known disc re-spinning keeps its tile"
    );
    assert!(poll_action(Absent, true, false).shows_idle());
    assert!(!poll_action(Present, false, false).shows_idle());
}

#[test]
fn a_disc_that_settles_into_present_is_ripped_once() {
    assert_eq!(run(&[Absent, Settling, Present, Present]), (1, 0));
}
