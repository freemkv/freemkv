use super::{ProbeFailTracker, ProbeFailure};
use std::cell::Cell;

#[test]
fn transient_failures_do_not_publish_and_success_restarts_grace() {
    let mut tracker = ProbeFailTracker::default();
    for _ in 0..2 {
        assert_eq!(
            tracker.on_probe_err("sg3", "/dev/sg3", no_enum),
            ProbeFailure::Pending
        );
        tracker.begin_tick();
    }
    tracker.clear("sg3");
    for _ in 0..2 {
        assert_eq!(
            tracker.on_probe_err("sg3", "/dev/sg3", no_enum),
            ProbeFailure::Pending
        );
        tracker.begin_tick();
    }
    assert_eq!(
        tracker.on_probe_err("sg3", "/dev/sg3", || paths(&["/dev/sg3"])),
        ProbeFailure::NewWedge
    );
    assert_eq!(
        tracker.on_probe_err("sg3", "/dev/sg3", no_enum),
        ProbeFailure::KnownWedge
    );
}

#[test]
fn recovered_probe_clears_warning_for_present_absent_and_settling() {
    use libfreemkv::DiscPresence::{Absent, Present, Settling};
    for presence in [Present, Absent, Settling] {
        let mut row = super::RipState {
            status: "error".into(),
            last_error: "Drive communication failed repeatedly (E4000). Retrying automatically."
                .into(),
            disc_present: true,
            disc_name: "Test disc".into(),
            ..Default::default()
        };
        super::clear_probe_error(&mut row, presence);
        assert_eq!(row.status, "idle");
        assert!(row.last_error.is_empty());
        assert_eq!(row.disc_present, presence != Absent);
        assert_eq!(row.disc_name, "Test disc");
    }
}

#[test]
fn successful_probe_preserves_worker_errors_and_active_jobs() {
    for (status, error) in [
        ("error", "Disc read failed"),
        (
            "scanning",
            "Drive communication failed repeatedly (E4000). Retrying automatically.",
        ),
    ] {
        let mut row = super::RipState {
            status: status.into(),
            last_error: error.into(),
            ..Default::default()
        };
        super::clear_probe_error(&mut row, libfreemkv::DiscPresence::Present);
        assert_eq!(row.status, status);
        assert_eq!(row.last_error, error);
    }
}

fn paths(p: &[&str]) -> Vec<String> {
    p.iter().map(|s| s.to_string()).collect()
}

fn no_enum() -> Vec<String> {
    panic!("must not enumerate here")
}

#[test]
fn enumeration_membership_separates_unplug_from_wedge() {
    let mut t = ProbeFailTracker::default();
    let r = t.classify_probe_err("sg1", "/dev/sg1", || paths(&["/dev/sg2"]));
    assert_eq!(r, ProbeFailure::HotUnplug);
    let mut t = ProbeFailTracker::default();
    let r = t.classify_probe_err("sg1", "/dev/sg1", || paths(&["/dev/sg1"]));
    assert_eq!(r, ProbeFailure::NewWedge);
}

#[test]
fn a_reported_wedge_is_not_re_enumerated_across_ticks_and_rescans() {
    let mut t = ProbeFailTracker::default();
    t.begin_tick();
    assert_eq!(
        t.classify_probe_err("sg1", "/dev/sg1", || paths(&["/dev/sg1"])),
        ProbeFailure::NewWedge
    );
    for _ in 0..3 {
        t.begin_tick();
        assert_eq!(
            t.classify_probe_err("sg1", "/dev/sg1", no_enum),
            ProbeFailure::KnownWedge
        );
    }
    t.on_rescan(paths(&["/dev/sg1"]));
    assert_eq!(
        t.classify_probe_err("sg1", "/dev/sg1", no_enum),
        ProbeFailure::KnownWedge
    );
}

#[test]
fn an_unplug_suspect_waits_for_the_rescan() {
    let mut t = ProbeFailTracker::default();
    assert_eq!(
        t.classify_probe_err("sg1", "/dev/sg1", Vec::new),
        ProbeFailure::HotUnplug
    );
    t.begin_tick();
    assert_eq!(
        t.classify_probe_err("sg1", "/dev/sg1", no_enum),
        ProbeFailure::HotUnplug
    );
    // Still listed at the rescan: re-check once against a post-probe enumeration.
    t.on_rescan(paths(&["/dev/sg1"]));
    let r = t.classify_probe_err("sg1", "/dev/sg1", || paths(&["/dev/sg1"]));
    assert_eq!(r, ProbeFailure::NewWedge);
}

#[test]
fn recovery_or_teardown_rearms_the_warning() {
    let mut t = ProbeFailTracker::default();
    t.classify_probe_err("sg1", "/dev/sg1", || paths(&["/dev/sg1"]));
    t.clear("sg1");
    t.begin_tick();
    let r = t.classify_probe_err("sg1", "/dev/sg1", || paths(&["/dev/sg1"]));
    assert_eq!(
        r,
        ProbeFailure::NewWedge,
        "a drive that recovered then re-wedged warns again"
    );
}

#[test]
fn a_stale_snapshot_cannot_declare_a_wedge() {
    // The rescan listed sg1, then it was unplugged before its probe failed.
    let calls = Cell::new(0);
    let mut t = ProbeFailTracker::default();
    t.on_rescan(paths(&["/dev/sg1"]));
    let r = t.classify_probe_err("sg1", "/dev/sg1", || {
        calls.set(calls.get() + 1);
        Vec::new()
    });
    assert_eq!(r, ProbeFailure::HotUnplug);
    assert_eq!(calls.get(), 1);
}

#[test]
fn a_snapshot_proves_absence_without_re_enumerating() {
    let mut t = ProbeFailTracker::default();
    t.on_rescan(paths(&["/dev/sg2"]));
    assert_eq!(
        t.classify_probe_err("sg1", "/dev/sg1", no_enum),
        ProbeFailure::HotUnplug
    );
}

#[test]
fn enumeration_is_bounded_per_tick() {
    let calls = Cell::new(0);
    let enumerate = || {
        calls.set(calls.get() + 1);
        paths(&["/dev/sg1"])
    };
    let mut t = ProbeFailTracker::default();
    t.begin_tick();
    assert_eq!(
        t.classify_probe_err("sg1", "/dev/sg1", enumerate),
        ProbeFailure::NewWedge
    );
    assert_eq!(
        t.classify_probe_err("sg2", "/dev/sg2", enumerate),
        ProbeFailure::HotUnplug
    );
    assert_eq!(
        calls.get(),
        1,
        "an absent drive reuses this tick's enumeration"
    );
    t.begin_tick();
    t.classify_probe_err("sg1", "/dev/sg1", enumerate);
    t.classify_probe_err("sg2", "/dev/sg2", enumerate);
    assert_eq!(
        calls.get(),
        1,
        "classified drives cost nothing on later ticks"
    );
}

#[test]
fn forget_removed_device_shares_the_busy_predicate() {
    let src = crate::server::util::source_lf(include_str!("mod.rs"));
    let start = src
        .find("fn forget_removed_device(device: &str) -> bool {")
        .expect("forget_removed_device must exist");
    let rest = &src[start..];
    let body = &rest[..rest.find("\n}\n").expect("function must end")];
    assert!(
        !body.contains("\"scanning\"") && body.contains("row_is_busy"),
        "forget_removed_device must use state::row_is_busy, not an inline copy of is_busy's predicate"
    );
}
