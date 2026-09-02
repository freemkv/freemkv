//! TLS-capable keydb fetch for `freemkv update-keys`.
//!
//! `fetch(url)` retrieves keydb bytes over `http://` or `https://` via
//! `ureq` and returns the raw response body; hand it to
//! `freemkv_keysources::KeydbSource::save` for verify + atomic save.
//!
//! Hardened like the online key service: resolves + SSRF-guards the host,
//! pins the validated addresses into the agent, follows zero redirects,
//! and bounds the connect/read timeouts and the response body size.
//!
//! See docs/keydb-fetch.md for the full design rationale.

use libfreemkv::{Error, Result};
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

/// Connect timeout — a dead mirror must fail fast, not hang the CLI.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Read timeout — the keydb body is a few MiB; allow a slow link.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Bounded DNS resolution so a wedged resolver can't hang the CLI.
const DNS_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the fetched body. The published keydb is a few MiB; this
/// generous ceiling still caps a hostile server from streaming an unbounded
/// body to OOM the client. `save` independently caps the *decompressed* size.
const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Fetch keydb bytes from `url` over HTTP or HTTPS via `ureq`, with the same
/// SSRF / redirect / timeout hardening as the online key service. The returned
/// bytes are the raw response body (plain text, `.zip`, or `.gz`) — hand them
/// to `freemkv_keysources::KeydbSource::save` for verify + atomic save.
pub fn fetch(url: &str) -> Result<Vec<u8>> {
    // An SSRF rejection (or a malformed/unsupported URL) surfaces as a connect
    // failure — the request never leaves the host. The user sees the localized
    // E8000 "could not connect" message keyed on the host.
    let pinned = resolve_and_guard(url).map_err(|_| Error::KeydbConnect { host: host_of(url) })?;
    let agent = hardened_agent(pinned);
    let resp = agent.get(url).call().map_err(|e| map_ureq_err(url, &e))?;
    read_capped(resp.into_body().into_reader(), MAX_BODY_BYTES).map_err(|e| cap_error(&e, url))
}

// Which keydb error a capped read's failure is: an over-large body is
// E8002 (content), a dead socket is E8000 (network, retry). See
// docs/keydb-fetch.md#cap_error.
fn cap_error(e: &CapError, url: &str) -> Error {
    match e {
        CapError::TooLarge => Error::KeydbInvalid,
        CapError::Io => Error::KeydbConnect { host: host_of(url) },
    }
}

// Why a capped read did not produce a body — the two outcomes [`cap_error`]
// tells apart. See docs/keydb-fetch.md#caperror.
#[derive(Debug)]
enum CapError {
    /// The body ran past the cap. A statement about the response.
    TooLarge,
    /// The socket failed part-way. A statement about the network.
    Io,
}

// Read at most `cap` bytes, rejecting anything larger. Split out of
// [`fetch`] so the cap is directly testable without real network I/O — see
// docs/keydb-fetch.md#read_capped.
fn read_capped(r: impl std::io::Read, cap: u64) -> std::result::Result<Vec<u8>, CapError> {
    let mut buf = Vec::new();
    // One byte past the cap, so an over-cap body is DETECTABLE rather than
    // silently truncated to exactly the limit.
    r.take(cap + 1)
        .read_to_end(&mut buf)
        .map_err(|_| CapError::Io)?;
    if buf.len() as u64 > cap {
        return Err(CapError::TooLarge);
    }
    Ok(buf)
}

/// Map a `ureq` transport/HTTP error to a libfreemkv keydb error so the CLI
/// renders it through the existing `error.E8xxx` locale strings.
fn map_ureq_err(url: &str, e: &ureq::Error) -> Error {
    match e {
        // A non-2xx HTTP status (the server answered, but not 200-ish).
        ureq::Error::StatusCode(code) => Error::KeydbHttp { status: *code },
        // Everything else is transport-level (DNS, connect, TLS, timeout, dropped
        // conn). ureq 3 splits these across several non_exhaustive variants, so a
        // catch-all stays correct; the CLI only distinguishes "answered" (above).
        _ => Error::KeydbConnect { host: host_of(url) },
    }
}

/// Best-effort host extraction for error messages. Falls back to the whole URL.
fn host_of(url: &str) -> String {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    authority.to_string()
}

// Build a ureq agent that follows zero redirects and pins DNS resolution
// to `pinned` (addresses already validated by [`resolve_and_guard`]).
// `ResolvedSocketAddrs` is a fixed 16-slot array; keep only the first 16.
const MAX_PINNED_ADDRS: usize = 16;

// The pinned-address resolver behind [`hardened_agent`], mirroring the one
// in `freemkv-keysources::online`. Must be wired via `Agent::with_parts`,
// never `new_with_config` — see docs/keydb-fetch.md#hardened_agent.
#[derive(Debug)]
struct PinnedResolver(Vec<SocketAddr>);

impl Resolver for PinnedResolver {
    fn resolve(
        &self,
        _uri: &Uri,
        _config: &Config,
        _timeout: NextTimeout,
    ) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        // NOT this module's `Result` alias (which is libfreemkv's, and fixes
        // the error type) — the trait's signature is the std two-parameter one.
        let mut out = self.empty();
        for addr in self.0.iter().take(MAX_PINNED_ADDRS) {
            out.push(*addr);
        }
        if out.is_empty() {
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

fn hardened_agent(pinned: Vec<SocketAddr>) -> ureq::Agent {
    let config = Config::builder()
        .max_redirects(0)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(READ_TIMEOUT))
        // `timeout_recv_response` bounds only the response HEADERS. Without this
        // the BODY had no deadline, so a mirror trickling bytes after a 200 could
        // hang `update-keys` forever. This deadline is ROLLING, so a slow link completes.
        .timeout_recv_body(Some(READ_TIMEOUT))
        .build();
    // `with_parts`, never `new_with_config` — see [`PinnedResolver`].
    ureq::Agent::with_parts(config, DefaultConnector::new(), PinnedResolver(pinned))
}

// SSRF guard (mirrors freemkv-keysources::online): resolve once, reject
// blocked IPs, pin addresses. `is_blocked_ip` covers loopback, link-local
// (incl. cloud metadata), RFC1918, CGNAT, broadcast, TEST-NET, multicast.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40)
                || v4.octets()[0] == 0
                || v4.octets()[0] >= 240
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4().map(|m| is_blocked_ip(&IpAddr::V4(m))) == Some(true)
        }
    }
}

/// Resolve `url`'s host and validate every resulting address against the SSRF
/// guard. Returns the pinned socket addresses on success, or an error string
/// on rejection.
fn resolve_and_guard(url: &str) -> std::result::Result<Vec<SocketAddr>, String> {
    let (rest, default_port) = if let Some(r) = url.strip_prefix("https://") {
        (r, 443u16)
    } else if let Some(r) = url.strip_prefix("http://") {
        (r, 80u16)
    } else {
        return Err("URL must start with http:// or https://".into());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    if authority.is_empty() {
        return Err("URL has no host".into());
    }
    let (host, port): (String, u16) = if let Some(stripped) = authority.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, after)) => {
                let p = after
                    .strip_prefix(':')
                    .map(|s| s.parse::<u16>().map_err(|_| "invalid port".to_string()))
                    .transpose()?
                    .unwrap_or(default_port);
                (h.to_string(), p)
            }
            None => return Err("malformed IPv6 host".into()),
        }
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        match p.parse::<u16>() {
            Ok(p) => (h.to_string(), p),
            Err(_) => (authority.to_string(), default_port),
        }
    } else {
        (authority.to_string(), default_port)
    };
    if host.is_empty() {
        return Err("URL has no host".into());
    }
    // Bounded DNS: resolution runs on its own thread; we stop WAITING after
    // `DNS_TIMEOUT` without joining (can't cancel a parked resolver thread). Cap
    // threads in flight so repeated GUI clicks in an outage can't leak them.
    static DNS_IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    const MAX_DNS_IN_FLIGHT: usize = 4;

    let addrs: Vec<SocketAddr> = {
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;
        if DNS_IN_FLIGHT.load(Ordering::Relaxed) >= MAX_DNS_IN_FLIGHT {
            return Err("DNS resolution timed out".into());
        }
        let host = host.clone();
        let (tx, rx) = mpsc::channel();
        DNS_IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            let res = (host.as_str(), port)
                .to_socket_addrs()
                .map(|it| it.collect::<Vec<SocketAddr>>());
            // Decremented by the RESOLVER thread, not the waiter: the slot is
            // occupied until the lookup actually returns, which is the whole
            // resource being bounded.
            DNS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            let _ = tx.send(res);
        });
        match rx.recv_timeout(DNS_TIMEOUT) {
            Ok(Ok(addrs)) => addrs,
            Ok(Err(e)) => return Err(format!("could not resolve host: {e}")),
            Err(_) => return Err("DNS resolution timed out".into()),
        }
    };
    if addrs.is_empty() {
        return Err("host did not resolve to any address".into());
    }
    for a in &addrs {
        if is_blocked_ip(&a.ip()) {
            return Err(format!(
                "refusing to connect to non-public address {} (SSRF guard)",
                a.ip()
            ));
        }
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    // The keydb BODY is bounded, not just its headers (ureq 3's
    // `timeout_recv_response` covers headers only). See
    // docs/keydb-fetch.md#test-the_keydb_body_read_is_bounded_not_only_the_headers.
    #[test]
    fn the_keydb_body_read_is_bounded_not_only_the_headers() {
        let agent = hardened_agent(Vec::new());
        let t = agent.config().timeouts();
        assert_eq!(
            t.recv_body,
            Some(READ_TIMEOUT),
            "ureq 3's recv_response covers headers only; without recv_body the \
             body read has no deadline at all"
        );
        assert_eq!(t.recv_response, Some(READ_TIMEOUT));
        assert_eq!(t.connect, Some(CONNECT_TIMEOUT));
    }

    #[test]
    fn ssrf_guard_blocks_loopback_private_and_metadata() {
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(
            169, 254, 169, 254
        ))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1))));
        assert!(is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0xfd00, 0, 0, 0, 0, 0, 0, 1
        ))));
        assert!(is_blocked_ip(&IpAddr::V6(
            Ipv4Addr::new(127, 0, 0, 1).to_ipv6_mapped()
        )));
    }

    #[test]
    fn ssrf_guard_allows_public_ips() {
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!is_blocked_ip(&IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    // Each disjunct of the SSRF guard, isolated — each case here trips
    // exactly one clause, so a broken clause shows up. See
    // docs/keydb-fetch.md#test-every_ssrf_disjunct_blocks_on_its_own.
    #[test]
    fn every_ssrf_disjunct_blocks_on_its_own() {
        let cases: &[(&str, &str)] = &[
            ("192.0.2.1", "documentation / TEST-NET-1"),
            ("224.0.0.1", "multicast"),
            ("245.0.0.1", "reserved (>= 240), not broadcast"),
            ("100.64.0.1", "CGNAT"),
            ("0.1.2.3", "leading zero octet"),
            ("fe80::1", "IPv6 link-local"),
            ("fc00::1", "IPv6 unique-local"),
        ];
        for (ip, why) in cases {
            let parsed: IpAddr = ip.parse().expect("test address parses");
            assert!(
                is_blocked_ip(&parsed),
                "{ip} ({why}) must be blocked on its own"
            );
        }
    }

    // The body-size cap — the decompression-bomb defence. See
    // docs/keydb-fetch.md#test-read_capped_admits_up_to_the_cap_and_rejects_past_it.
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

    // A connection that dies mid-body is a TRANSPORT failure (E8000), not a
    // verdict about the content (E8002) — the two must not collapse into
    // one value. See docs/keydb-fetch.md#test-a_connection_that_dies_mid_body_is_not_an_invalid_keydb.
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

    // The CGNAT clause must not become over-broad — mutating its `&&` to
    // `||` must not silently start blocking ordinary public addresses. See
    // docs/keydb-fetch.md#test-the_cgnat_clause_does_not_block_ordinary_public_addresses.
    #[test]
    fn the_cgnat_clause_does_not_block_ordinary_public_addresses() {
        for ip in ["8.65.0.1", "1.100.0.1", "203.0.100.7"] {
            let parsed: IpAddr = ip.parse().expect("test address parses");
            assert!(
                !is_blocked_ip(&parsed),
                "{ip} is public and must not be blocked by the CGNAT clause"
            );
        }
        // And a real CGNAT address still is.
        assert!(is_blocked_ip(&"100.127.255.254".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn resolve_and_guard_rejects_internal_literals() {
        assert!(resolve_and_guard("http://127.0.0.1/keydb.zip").is_err());
        assert!(resolve_and_guard("http://169.254.169.254/keydb.zip").is_err());
        assert!(resolve_and_guard(&format!("https://{}.{}.{}.{}/k", 10, 0, 0, 5)).is_err());
        assert!(resolve_and_guard("http://[::1]:9000/k").is_err());
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

    #[test]
    fn host_of_extracts_authority() {
        assert_eq!(host_of("https://example.org/export/k.zip"), "example.org");
        assert_eq!(host_of("http://example.org:8080/k"), "example.org:8080");
        assert_eq!(host_of("https://user@example.org/k"), "example.org");
    }

    // Proves `hardened_agent` actually consults the pinned resolver rather
    // than falling back to live DNS (which would reopen the rebind window).
    // See docs/keydb-fetch.md#test-hardened_agent_connects_to_the_pinned_address_not_dns.
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
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
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
}
