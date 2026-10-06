use super::*;
use std::net::Ipv4Addr;

// FT8 (stop design v5 §2.7, T20): "v5 **deletes `BODY_TRANSFER_BUDGET`**:
// `timeout_recv_body(None)`". Per spec; do not change without a spec citation.
#[test]
fn keydb_agent_has_no_total_body_budget() {
    let agent = hardened_agent(Vec::new());
    let t = agent.config().timeouts();
    assert_eq!(t.recv_body, None, "a body total is gone (HR1: stall-only)");
    assert_eq!(
        t.recv_response,
        Some(READ_TIMEOUT),
        "headers: 60 s no answer"
    );
    assert_eq!(t.connect, Some(CONNECT_TIMEOUT), "connect: 10 s no answer");
}

// A stub keydb server: read the request head, answer `head`, then run `body`.
fn keydb_stub(
    head: &'static [u8],
    body: impl FnOnce(&mut std::net::TcpStream) + Send + 'static,
) -> (SocketAddr, std::thread::JoinHandle<()>) {
    use std::io::{Read as _, Write as _};
    let listener =
        std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");
    let server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept failed");
        let mut req = Vec::new();
        let mut byte = [0u8; 1];
        while !req.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => req.push(byte[0]),
            }
        }
        let _ = sock.write_all(head);
        let _ = sock.flush();
        body(&mut sock);
    });
    (pinned, server)
}

// FT7a (T20, §5.0 pair (a)): a server trickling a byte every 0.5 x idle for longer than
// the old budget (scaled: idle 1 s, 14 bytes 500 ms apart = 7 s vs 6 s) -> `Ok`. The
// 500 ms of slack keeps a slow runner's late wake-up inside the idle window.
#[test]
fn keydb_fetch_slow_body_past_old_budget_succeeds() {
    use std::io::{Read as _, Write as _};
    let idle = Duration::from_millis(1000);
    let (pinned, server) = keydb_stub(
        b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\n\r\n",
        move |sock| {
            for _ in 0..14 {
                std::thread::sleep(idle / 2);
                if sock.write_all(b"k").and_then(|()| sock.flush()).is_err() {
                    return;
                }
            }
        },
    );
    let agent = hardened_agent_with_timeouts(
        vec![pinned],
        Duration::from_secs(5),
        Duration::from_secs(30),
        idle,
    );
    let started = std::time::Instant::now();
    let resp = agent
        .get("http://keydb-mirror.test/keydb.zip")
        .call()
        .expect("headers must arrive");
    let mut body = Vec::new();
    let read = resp.into_body().into_reader().read_to_end(&mut body);
    let _ = server.join();
    assert!(
        started.elapsed() > idle * 6,
        "the trickle outlasted the old total"
    );
    assert!(
        read.is_ok(),
        "a progressing body was cut off: {:?}",
        read.err()
    );
    assert_eq!(body, vec![b'k'; 14], "the whole body must arrive");
}

// FT7b (T20, §5.0 pair (b)): a stalled body "must fire within window + 1 s".
#[test]
fn keydb_fetch_stalled_body_fails_after_idle() {
    use std::io::Read as _;
    let (pinned, server) = keydb_stub(
        b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n",
        // Promise a megabyte, send none; end when the client drops.
        |sock| {
            let _ = sock.read(&mut [0u8; 1]);
        },
    );
    let idle = Duration::from_secs(1);
    let agent = hardened_agent_with_timeouts(
        vec![pinned],
        Duration::from_secs(5),
        Duration::from_secs(30),
        idle,
    );
    let resp = agent
        .get("http://keydb-mirror.test/keydb.zip")
        .call()
        .expect("headers must arrive");
    let started = std::time::Instant::now();
    let mut body = Vec::new();
    let read = resp.into_body().into_reader().read_to_end(&mut body);
    let elapsed = started.elapsed();
    let _ = server.join();
    assert!(read.is_err(), "a stalled body must not read as success");
    assert!(
        elapsed <= idle + Duration::from_secs(1),
        "held {elapsed:?} past the idle window"
    );
}

fn stub_agent(pinned: SocketAddr) -> ureq::Agent {
    hardened_agent_with_timeouts(
        vec![pinned],
        Duration::from_secs(5),
        Duration::from_secs(30),
        Duration::from_secs(5),
    )
}

// A non-2xx answer is the HTTP-status error, a redirect included: the agent follows none,
// so a mirror's 301 is reported rather than saved as keydb content.
#[test]
fn a_non_success_status_is_a_keydb_http_error() {
    for (head, want) in [
            (
                &b"HTTP/1.1 301 Moved Permanently\r\nLocation: http://keydb-mirror.test/new\r\nContent-Length: 0\r\n\r\n"[..],
                301u16,
            ),
            (&b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"[..], 404),
            (&b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n"[..], 503),
        ] {
            let head: &'static [u8] = Box::leak(head.to_vec().into_boxed_slice());
            let (pinned, server) = keydb_stub(head, |_| {});
            let got = get_body(&stub_agent(pinned), "http://keydb-mirror.test/keydb.zip", 1024);
            let _ = server.join();
            assert!(
                matches!(got, Err(Error::KeydbHttp { status }) if status == want),
                "{want}: {got:?}"
            );
        }
}

// `get_body` hands the cap to the body read: over it is E8002, at it is the body.
#[test]
fn get_body_enforces_its_cap() {
    let ok = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nkeys";
    let (pinned, server) = keydb_stub(ok, |_| {});
    let got = get_body(&stub_agent(pinned), "http://keydb-mirror.test/k", 4);
    let _ = server.join();
    assert_eq!(got.expect("a body at the cap"), b"keys");
    let (pinned, server) = keydb_stub(ok, |_| {});
    let got = get_body(&stub_agent(pinned), "http://keydb-mirror.test/k", 3);
    let _ = server.join();
    assert!(matches!(got, Err(Error::KeydbInvalid)), "{got:?}");
}

// The body-size cap — the decompression-bomb defence.
#[test]
fn read_capped_admits_up_to_the_cap_and_rejects_past_it() {
    // Exactly at the cap is fine — an off-by-one here would reject valid
    // keydbs at the boundary.
    let body = vec![b'x'; 64];
    let got = read_capped(std::io::Cursor::new(body.clone()), 64).expect("64 <= 64");
    assert_eq!(
        got.len(),
        64,
        "a body exactly at the cap must be returned whole"
    );
    assert_eq!(got, body);

    // Under the cap.
    assert_eq!(
        read_capped(std::io::Cursor::new(vec![b'x'; 63]), 64)
            .expect("63 < 64")
            .len(),
        63
    );

    // One byte over: rejected, NOT truncated. Truncation would hand a
    // half-parsed keydb to the caller as if it were whole.
    assert!(
        read_capped(std::io::Cursor::new(vec![b'x'; 65]), 64).is_err(),
        "a body past the cap must be rejected"
    );
    // Far over.
    assert!(read_capped(std::io::Cursor::new(vec![b'x'; 100_000]), 64).is_err());
    // Empty is fine at the read layer; emptiness is the caller's check.
    assert_eq!(
        read_capped(std::io::Cursor::new(Vec::new()), 64)
            .unwrap()
            .len(),
        0
    );
}

// A connection that dies mid-body is a TRANSPORT failure (E8000), not a verdict about the
// content (E8002) — the two must not collapse into one value.
#[test]
fn a_connection_that_dies_mid_body_is_not_an_invalid_keydb() {
    struct Reset;
    impl std::io::Read for Reset {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "peer went away mid-download",
            ))
        }
    }

    let too_large = read_capped(std::io::Cursor::new(vec![b'x'; 65]), 64)
        .expect_err("a body past the cap is refused");
    let dropped = read_capped(Reset, 64).expect_err("a dead socket is a failure");
    assert_ne!(
        std::mem::discriminant(&too_large),
        std::mem::discriminant(&dropped),
        "an over-large body and a dropped connection are different \
             failures and must not collapse into one error"
    );

    // And each must reach the user as the right one of the two messages.
    const URL: &str = "https://mirror.example.org/keydb.zip";
    assert!(
        matches!(cap_error(&too_large, URL), Error::KeydbInvalid),
        "an over-large body IS a verdict about the content (E8002)"
    );
    match cap_error(&dropped, URL) {
        Error::KeydbConnect { host } => assert_eq!(host, "mirror.example.org"),
        other => panic!("a dropped connection must read as E8000, got {other:?}"),
    }
}

#[test]
fn resolve_and_guard_allows_lan_literals_and_rejects_invalid_ones() {
    // Home app: loopback, RFC1918 and link-local literals (no DNS) are valid.
    let lan = format!("https://{}.{}.{}.{}/k", 10, 0, 0, 5);
    for url in [
        "http://127.0.0.1/keydb.zip",
        "http://169.254.169.254/keydb.zip",
        "http://[::1]:9000/k",
        lan.as_str(),
    ] {
        assert!(resolve_and_guard(url).is_ok(), "{url} must be accepted");
    }
    for url in [
        "http://0.0.0.0/k",
        "http://224.0.0.1/k",
        "http://240.0.0.1/k",
        "http://[::]/k",
    ] {
        assert!(resolve_and_guard(url).is_err(), "{url} must be refused");
    }
}

// URL schemes are case-insensitive (RFC 3986 §3.1); an upper-case one is not "unsupported".
#[test]
fn an_upper_case_scheme_is_accepted() {
    let ok = resolve_and_guard("HTTPS://1.1.1.1/keydb.zip").expect("HTTPS is https");
    assert_eq!(ok[0].port(), 443);
    let ok = resolve_and_guard("Http://1.1.1.1/keydb.zip").expect("Http is http");
    assert_eq!(ok[0].port(), 80);
    assert_eq!(
        host_of("HTTPS://user@example.org:8443/x"),
        "example.org:8443"
    );
}

#[test]
fn resolve_and_guard_rejects_bad_scheme() {
    // Crucially: ftp/file must be rejected, but https is NOW accepted
    // (the whole point of this module) — see resolve_and_guard_accepts_*.
    assert!(resolve_and_guard("ftp://example.com/k").is_err());
    assert!(resolve_and_guard("file:///etc/passwd").is_err());
    assert!(resolve_and_guard("not a url").is_err());
    assert!(resolve_and_guard("").is_err());
}

#[test]
fn resolve_and_guard_accepts_public_literal_both_schemes() {
    // https:// is the new capability — a public literal must be accepted
    // and default to port 443.
    let addrs =
        resolve_and_guard("https://8.8.8.8/keydb.zip").expect("public https must be accepted");
    assert_eq!(addrs[0].port(), 443);
    // http:// still works and defaults to port 80.
    let addrs =
        resolve_and_guard("http://1.1.1.1/keydb.zip").expect("public http must be accepted");
    assert_eq!(addrs[0].port(), 80);
    // Explicit port honored.
    let addrs = resolve_and_guard("https://1.1.1.1:8443/k").expect("explicit port");
    assert_eq!(addrs[0].port(), 8443);
}

// A policy refusal must not read as a dead server (E8000 "cannot connect").
// L130: `{detail}` must stay the bare address — locale-neutral, never an
// English phrase spliced into every locale's E8006 text.
#[test]
fn a_guard_refusal_is_not_reported_as_could_not_connect() {
    let bad = fetch("http://0.0.0.0/keydb.zip").unwrap_err();
    assert!(
        matches!(&bad, Error::KeydbUnsupportedScheme { scheme } if scheme == "0.0.0.0"),
        "an address refusal must name the refused address, got {bad:?}"
    );
    let ftp = fetch("ftp://example.com/k").unwrap_err();
    assert!(
        matches!(&ftp, Error::KeydbUnsupportedScheme { scheme } if scheme == "ftp"),
        "got {ftp:?}"
    );
    assert_eq!(ftp.code(), 8006);
    // Not a URL at all: there is no scheme to name, so it stays E8000.
    assert!(matches!(
        fetch("not a url").unwrap_err(),
        Error::KeydbConnect { .. }
    ));
}

// ureq 3 defaults to `Proxy::try_from_env()`, and PinnedResolver would answer the proxy's
// lookup with the keydb server's address. Checked in a child so no test mutates the env.
#[test]
fn the_keydb_agent_never_uses_an_environment_proxy() {
    const CHILD: &str = "FMKV_KEYDB_PROXY_CHILD";
    const NAME: &str = "keydb_fetch::tests::the_keydb_agent_never_uses_an_environment_proxy";
    if std::env::var_os(CHILD).is_some() {
        assert!(
            hardened_agent(Vec::new()).config().proxy().is_none(),
            "the pinned agent picked up a proxy from the environment"
        );
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([NAME, "--exact", "--test-threads=1"])
        .env(CHILD, "1")
        .env("ALL_PROXY", "http://proxy.example:3128")
        .env("HTTPS_PROXY", "http://proxy.example:3128")
        .env("HTTP_PROXY", "http://proxy.example:3128")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "child run failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn host_of_extracts_authority() {
    assert_eq!(host_of("https://example.org/export/k.zip"), "example.org");
    assert_eq!(host_of("http://example.org:8080/k"), "example.org:8080");
    assert_eq!(host_of("https://user@example.org/k"), "example.org");
}

// Proves `hardened_agent` actually consults the pinned resolver rather than falling back to
// live DNS (which would reopen the rebind window).
#[test]
fn hardened_agent_connects_to_the_pinned_address_not_dns() {
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::mpsc;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");
    let (tx, rx) = mpsc::channel();

    let server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("stub listener accept failed");
        let _ = tx.send(());
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => head.push(byte[0]),
            }
        }
        let _ =
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
        let _ = sock.flush();
        head
    });

    let sent = hardened_agent(vec![pinned])
        .get("http://keydb-mirror.test/keydb.zip")
        .call();

    rx.recv_timeout(Duration::from_secs(10)).expect(
        "hardened_agent never connected to the pinned address — the custom \
             resolver is not being consulted, so a DNS rebind between the guard \
             and the fetch can still redirect the request",
    );
    let resp = sent.expect("the pinned round-trip must complete");
    assert_eq!(resp.status(), 200, "the stub server's reply must come back");
    let head = server.join().expect("stub server panicked");
    let head = String::from_utf8_lossy(&head);
    assert!(
        head.contains("keydb-mirror.test"),
        "the pinned agent must still address the original host; got: {head}"
    );
}
