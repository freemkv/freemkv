//! A host name, not a literal, goes through the bounded resolver thread.
use super::{Refusal, resolve_and_guard};

#[test]
fn a_named_host_is_resolved_on_the_bounded_resolver() {
    match resolve_and_guard("http://localhost:8080/keydb.cfg") {
        Ok(addrs) => {
            assert!(!addrs.is_empty());
            assert!(addrs.iter().all(|a| a.port() == 8080), "{addrs:?}");
        }
        Err(Refusal::Blocked(ip)) => assert!(ip.is_loopback(), "{ip}"),
        Err(other) => panic!("localhost did not resolve: {other}"),
    }
}
