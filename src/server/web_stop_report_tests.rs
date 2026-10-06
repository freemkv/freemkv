use super::{apply_stop_to_state, stop_report};
use crate::server::ripper::{RipState, STATE};

// A claim made while Stop drains is a new rip: Stop must not wipe it to idle.
#[test]
fn a_stop_does_not_wipe_a_claim_made_while_it_drained() {
    let dev = "stopgen_reclaimed";
    STATE.lock().unwrap().insert(
        dev.into(),
        RipState {
            device: dev.into(),
            status: "scanning".into(),
            claim_gen: 5,
            ..Default::default()
        },
    );
    assert!(apply_stop_to_state(dev, Some(4), &stop_report(true)));
    {
        let s = STATE.lock().unwrap();
        assert_eq!(s[dev].status, "scanning");
        assert_eq!(s[dev].claim_gen, 5);
    }
    assert!(apply_stop_to_state(dev, Some(5), &stop_report(true)));
    assert_eq!(STATE.lock().unwrap()[dev].status, "idle");
    // A timed-out drain publishes the error row, not idle.
    assert!(apply_stop_to_state(dev, Some(5), &stop_report(false)));
    {
        let s = STATE.lock().unwrap();
        assert_eq!(s[dev].status, "error");
        assert!(!s[dev].last_error.is_empty());
        assert_eq!(s[dev].claim_gen, 5, "the claim identity survives a Stop");
    }
    assert!(!apply_stop_to_state(
        "stopgen_absent",
        None,
        &stop_report(true)
    ));
    STATE.lock().unwrap().remove(dev);
}

// Catches the mutation making a timed-out Stop report the clean-stop
// answer (idle + ok:true): a failure rendered as success. handle_stop
// used to reset to "idle" regardless of drain, leaving every later route 409ing.
#[test]
fn a_stop_that_did_not_drain_is_not_reported_as_a_clean_stop() {
    let clean = stop_report(true);
    assert_eq!(
        clean.status, "idle",
        "a drained stop leaves the device idle"
    );
    assert!(clean.last_error.is_empty(), "no error on the clean path");
    assert!(
        clean.body.contains(r#""ok":true"#),
        "a drained stop answers ok:true; got {}",
        clean.body
    );

    let timed_out = stop_report(false);
    assert_ne!(
        timed_out.status, "idle",
        "a stop whose worker is still running must NOT publish idle — the \
             device is still held and every route will refuse it"
    );
    assert!(
        !timed_out.last_error.is_empty(),
        "the reason the device is still busy must reach the state row the \
             dashboard renders, not just the server log"
    );
    assert!(
        timed_out.body.contains(r#""ok":false"#),
        "a stop that stopped nothing must not answer ok:true; got {}",
        timed_out.body
    );
}
