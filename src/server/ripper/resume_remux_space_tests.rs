use super::remux_space_shortfall;

const GB: u64 = 1_000_000_000;

#[test]
fn remux_refuses_when_staging_cannot_hold_the_planned_outputs() {
    let msg = remux_space_shortfall(30 * GB, 0, Some(10 * GB), "/staging/sr0")
        .expect("10 GB free cannot hold 30 GB of episodes");
    assert!(msg.contains("/staging/sr0"), "{msg}");
    assert!(msg.contains("Staging Directory"), "{msg}");
    assert!(!msg.contains("STAGING_DIR"), "{msg}");
    // Earlier-attempt outputs get overwritten, so they count toward free space.
    assert_eq!(
        remux_space_shortfall(30 * GB, 20 * GB, Some(10 * GB), "/s"),
        None
    );
    assert_eq!(remux_space_shortfall(30 * GB, 0, Some(30 * GB), "/s"), None);
    assert_eq!(remux_space_shortfall(0, 0, Some(0), "/s"), None);
    assert_eq!(remux_space_shortfall(30 * GB, 0, None, "/s"), None);
}

#[test]
fn repeated_identical_space_refusal_is_noted_once() {
    let dir = std::path::PathBuf::from(format!("/nonexistent/space-{}", std::process::id()));
    assert!(
        super::note_space_refusal(&dir, 30 * GB),
        "first refusal logs"
    );
    assert!(
        !super::note_space_refusal(&dir, 30 * GB),
        "identical repeat is quiet"
    );
    assert!(
        super::note_space_refusal(&dir, 31 * GB),
        "a changed need logs again"
    );
    super::forget_space_refusal(&dir);
    assert!(
        super::note_space_refusal(&dir, 31 * GB),
        "logs again after clearing"
    );
    super::forget_space_refusal(&dir);
}

#[test]
fn space_refusal_threads_ripstate_to_the_worker_outcome() {
    let rs = crate::server::ripper::RipState {
        last_error: "Not enough staging disk space".to_string(),
        failure_space: true,
        ..crate::server::ripper::RipState::default()
    };
    let mut outcome = super::MuxHandoffOutcome::default();
    super::apply_failure_fields(&mut outcome, &rs);
    assert!(outcome.failure_space, "space bit must reach the mux worker");
    assert!(!outcome.failure_finalize && !outcome.failure_retryable);
}

// Wiring pin: resume_remux runs the space check (sized by mux_reserve_for) before
// the key round-trip, and flags the refusal on RipState.
#[test]
fn resume_remux_checks_space_before_key_resolution() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    let body = &src[src.find("\nfn remux_space_refusal(").unwrap()..];
    let body = &body[..body.find("\n}\n").unwrap()];
    assert!(body.contains("super::mux_reserve_for(cfg, titles, &fanout, primary)"));
    assert!(body.contains("remux_space_shortfall(required, existing, avail, &label)"));
    let f = &src[src.find("\npub fn resume_remux(").unwrap()..];
    let check = f
        .find("remux_space_refusal(&cfg_read, &staging_dir")
        .unwrap();
    let keys = f.find("keysource::open_staged_image(").unwrap();
    assert!(check < keys, "space check must precede key resolution");
    assert!(f[check..keys].contains("s.failure_space = true"));
}
