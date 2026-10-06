// Catches the mutation dropping the disarm branch when
// spawn_rip_after_claim fails: .accept-loss is armed BEFORE spawning, so
// a refused thread must disarm it or the next rip inherits stale consent.
#[test]
fn a_failed_spawn_disarms_the_accept_loss_override() {
    let src = crate::server::util::source_lf(include_str!("web.rs"));
    let start = src
        .find("fn handle_accept_loss(")
        .expect("handle_accept_loss must exist");
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("function must end");
    let body: String = rest[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains("if !spawn_rip_after_claim("),
        "handle_accept_loss must CHECK whether the spawn succeeded — a \
             fire-and-forget call cannot disarm the override it armed"
    );
    assert!(
        body.contains("clear_accept_loss_marker("),
        "a spawn failure must disarm `.accept-loss`, or the override sits \
             on disk with no run to consume it and the NEXT rip of this disc \
             silently inherits the operator's consent"
    );
}
