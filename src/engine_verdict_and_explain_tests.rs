use super::{explain, recovery_terminal_result};

// A recovery aborted for loss is an ERROR, never a completed rip, for
// both output kinds; a halt is an Ok cancel. Mutation caught: returning
// Ok for aborted_for_loss, or dropping halt-outranks-loss precedence.
#[test]
fn a_loss_abort_is_an_error_not_a_completed_rip() {
    // Halt → Ok cancel, image kept, both output kinds.
    assert!(matches!(
        recovery_terminal_result(true, false, true, "d.iso"),
        Some(Ok(_))
    ));
    assert!(matches!(
        recovery_terminal_result(true, false, false, "d.iso"),
        Some(Ok(_))
    ));
    // Loss abort → Err, both output kinds. This is the exit-0-over-damage bug.
    assert!(matches!(
        recovery_terminal_result(false, true, true, "d.iso"),
        Some(Err(_))
    ));
    assert!(matches!(
        recovery_terminal_result(false, true, false, "d.iso"),
        Some(Err(_))
    ));
    // Neither terminal → proceed to the normal write/mux path.
    assert!(recovery_terminal_result(false, false, true, "d.iso").is_none());
    // Halt outranks a coincident loss abort — "you stopped it" wins, and it
    // is an Ok cancel, not an Err.
    assert!(matches!(
        recovery_terminal_result(true, true, false, "d.iso"),
        Some(Ok(_))
    ));
}

// explain must equal what the CLI renders for the same error.E<code> key
// (GUI/CLI parity). Mutation caught: reverting to hard-coded English
// arms, or a placeholder-carrying code (E7022's {hash}) leaking `{…}`.
#[test]
fn explain_localizes_through_the_catalog() {
    // Routed codes equal the catalog string the CLI uses for the same key.
    for code in [9048u16, 7028, 7029, 7030, 6013] {
        assert_eq!(
            explain(code),
            crate::strings::error_message(u32::from(code)),
            "E{code} must render from the catalog, not hard-coded English"
        );
    }
    // No-key codes that carry runtime data the GUI lacks map to the
    // argument-free sibling — localized, and with NO leaked placeholder.
    for code in [7022u16, 8005] {
        let msg = explain(code);
        assert_eq!(msg, crate::strings::error_message(7018));
        assert!(!msg.contains('{'), "E{code} leaked a placeholder: {msg}");
    }
    // An unknown code keeps its number for a bug report, no placeholder.
    let unknown = explain(4242);
    assert!(unknown.contains("4242"), "{unknown}");
    assert!(!unknown.contains('{'), "{unknown}");
}
