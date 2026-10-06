use super::*;

#[test]
fn a_rip_slot_is_counted_and_released() {
    let a = Arbiter::new();
    assert!(!a.rip_active());
    let e = a.epoch();
    {
        let _one = a.rip();
        let _two = a.rip();
        assert!(a.rip_active());
    }
    assert!(!a.rip_active());
    assert!(
        a.rip_started_since(e),
        "the starts stay visible after release"
    );
}

#[test]
fn no_rip_since_the_epoch_means_not_preempted() {
    let a = Arbiter::new();
    let e = a.epoch();
    assert!(!a.rip_started_since(e));
    drop(a.rip());
    assert!(a.rip_started_since(e));
    assert!(!a.rip_started_since(a.epoch()), "a later epoch is clean");
}
