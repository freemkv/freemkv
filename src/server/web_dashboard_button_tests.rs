// A device action must never be a bare fetch: that discards the answer,
// so a 409 (a claim held by an unwinding worker) would read as success.
// Every action goes through `driveAction`, which reports a refusal.
#[test]
fn no_device_action_discards_the_servers_answer() {
    let (_, _, body) = super::ASSETS
        .iter()
        .find(|(n, _, _)| *n == "ripper.js")
        .unwrap();
    let src = std::str::from_utf8(body).unwrap();
    assert!(
        !src.contains("fetch("),
        "ripper.js must call the API through api()"
    );
    for ep in [
        "/api/scan/",
        "/api/rip/",
        "/api/eject/",
        "/api/stop/",
        "/api/accept-loss/",
    ] {
        assert!(src.contains(ep), "the drive card lost its {ep} action");
    }
    assert!(src.contains("act(btn, () => api('POST', url)"));
}
