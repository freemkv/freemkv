// Catches the mutation that puts `if let Ok(mut s) = STATE.lock()` back into
// `forget_removed_device`, which silently skips removal on a poisoned STATE.
#[test]
fn forget_removed_device_recovers_a_poisoned_state_lock() {
    let src = crate::server::util::source_lf(include_str!("mod.rs"));
    let start = src
        .find("fn forget_removed_device(device: &str) -> bool {")
        .expect("forget_removed_device must exist");
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("function must end");
    let body: String = rest[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !body.contains("if let Ok("),
        "forget_removed_device must not skip its teardown on a poisoned \
             lock — recover the guard (`unwrap_or_else(|e| e.into_inner())`) \
             like every other lock site in this crate"
    );
    assert!(
        body.contains("unwrap_or_else(|e| e.into_inner())"),
        "the STATE removal must poison-recover, or a panicked worker leaves \
             a phantom drive row that nothing ever clears"
    );
}
