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

/// Header timeout — how long to wait for the response head to arrive.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Rolling idle/stall bound on body reads, re-armed per read by
/// [`IdleReCapConnector`] since ureq 3 exposes no such knob. A genuine stall
/// trips it; a slow-but-progressing body does not.
const STALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Total-transfer ceiling on the keydb body once headers are in — sized to a
/// real few-MiB keydb over a slow link, not a per-read bound.
const BODY_TRANSFER_BUDGET: Duration = Duration::from_secs(120);

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

// Chained after DefaultConnector to re-arm a ROLLING per-read idle bound on
// every body read, restoring the stall detection ureq 3.4.1 removed (#1194).
// See docs/keydb-fetch.md#hardened_agent.
#[derive(Debug)]
struct IdleReCapConnector {
    idle: Duration,
}

impl<In: ureq::unversioned::transport::Transport> ureq::unversioned::transport::Connector<In>
    for IdleReCapConnector
{
    type Out = IdleReCapTransport<In>;

    fn connect(
        &self,
        _details: &ureq::unversioned::transport::ConnectionDetails,
        chained: Option<In>,
    ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
        Ok(chained.map(|inner| IdleReCapTransport {
            inner,
            idle: self.idle,
        }))
    }
}

#[derive(Debug)]
struct IdleReCapTransport<In> {
    inner: In,
    idle: Duration,
}

impl<In> IdleReCapTransport<In> {
    // Cap only BODY reads (reason RecvBody) at the idle bound; connect and
    // header phases keep ureq's own timeouts. min keeps the tighter of a small
    // total-body budget and idle.
    fn cap(
        &self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> ureq::unversioned::transport::NextTimeout {
        use ureq::unversioned::transport::time::Duration as UreqDuration;
        if timeout.reason != ureq::Timeout::RecvBody {
            return timeout;
        }
        let idle = UreqDuration::from_millis(self.idle.as_millis() as u64);
        let after = if timeout.after < idle {
            timeout.after
        } else {
            idle
        };
        ureq::unversioned::transport::NextTimeout {
            after,
            reason: ureq::Timeout::RecvBody,
        }
    }
}

impl<In: ureq::unversioned::transport::Transport> ureq::unversioned::transport::Transport
    for IdleReCapTransport<In>
{
    fn buffers(&mut self) -> &mut dyn ureq::unversioned::transport::Buffers {
        self.inner.buffers()
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> std::result::Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(
        &mut self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> std::result::Result<bool, ureq::Error> {
        let capped = self.cap(timeout);
        self.inner.await_input(capped)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

fn hardened_agent(pinned: Vec<SocketAddr>) -> ureq::Agent {
    hardened_agent_with_timeouts(
        pinned,
        CONNECT_TIMEOUT,
        READ_TIMEOUT,
        BODY_TRANSFER_BUDGET,
        STALL_TIMEOUT,
    )
}

// The agent builder with caller-chosen timeouts so the rolling-idle behaviour
// is testable with short bounds. `response` bounds header arrival; `budget` is
// the TOTAL body ceiling; `idle` is the rolling stall bound via IdleReCapConnector.
fn hardened_agent_with_timeouts(
    pinned: Vec<SocketAddr>,
    connect: Duration,
    response: Duration,
    budget: Duration,
    idle: Duration,
) -> ureq::Agent {
    // Since ureq 3.4.1 (#1194) timeout_recv_body is an ABSOLUTE total-body
    // deadline that no longer re-arms, and recv_response no longer caps the
    // body. Set the total ceiling here; layer rolling idle via the connector.
    let config = Config::builder()
        .max_redirects(0)
        .timeout_connect(Some(connect))
        .timeout_recv_response(Some(response))
        .timeout_recv_body(Some(budget))
        .build();
    // `with_parts`, never `new_with_config` — see [`PinnedResolver`].
    // DefaultConnector opens the (TLS) socket; IdleReCapConnector wraps its
    // transport to re-arm the rolling idle bound on every body read.
    use ureq::unversioned::transport::Connector as _;
    ureq::Agent::with_parts(
        config,
        DefaultConnector::new().chain(IdleReCapConnector { idle }),
        PinnedResolver(pinned),
    )
}

/// SSRF guard (mirrors freemkv-keysources::online): resolve once, reject
/// blocked IPs, pin addresses. `is_blocked_ip` covers loopback, link-local
/// (incl. cloud metadata), RFC1918, CGNAT, broadcast, TEST-NET, multicast,
/// the 198.18.0.0/15 benchmarking range and 192.0.0.0/24 IETF assignments.
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
                // 198.18.0.0/15 — RFC 2544 benchmarking range.
                || (v4.octets()[0] == 198 && (v4.octets()[1] & 0xfe) == 18)
                // 192.0.0.0/24 — IETF protocol assignments (RFC 6890).
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0)
                || v4.octets()[0] == 0
                || v4.octets()[0] >= 240
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            // 6to4 (2002::/16) embeds an IPv4 in segments[1..3]; Teredo
            // (2001:0000::/32) embeds the client IPv4 in the last two segments,
            // each XOR 0xffff — re-check both, or an internal target tunnels in.
            let sixtofour = (seg[0] == 0x2002)
                .then(|| std::net::Ipv4Addr::from(((seg[1] as u32) << 16) | (seg[2] as u32)));
            let teredo = (seg[0] == 0x2001 && seg[1] == 0x0000).then(|| {
                std::net::Ipv4Addr::from(
                    (((seg[6] ^ 0xffff) as u32) << 16) | ((seg[7] ^ 0xffff) as u32),
                )
            });
            // NAT64 well-known prefix 64:ff9b::/96 (RFC 6052) embeds the IPv4 in
            // the last 32 bits (segments[6..8]); re-check it too so an internal
            // target does not slip through a NAT64 translator.
            let nat64 = (seg[0] == 0x0064 && seg[1] == 0xff9b)
                .then(|| std::net::Ipv4Addr::from(((seg[6] as u32) << 16) | (seg[7] as u32)));
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                || v6.to_ipv4().map(|m| is_blocked_ip(&IpAddr::V4(m))) == Some(true)
                || sixtofour.is_some_and(|v4| is_blocked_ip(&IpAddr::V4(v4)))
                || teredo.is_some_and(|v4| is_blocked_ip(&IpAddr::V4(v4)))
                || nat64.is_some_and(|v4| is_blocked_ip(&IpAddr::V4(v4)))
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
        // Check-and-increment in one atomic op — a separate load-then-add left a
        // window where concurrent callers could all pass the check together.
        if DNS_IN_FLIGHT.fetch_add(1, Ordering::Relaxed) >= MAX_DNS_IN_FLIGHT {
            DNS_IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            return Err("DNS resolution timed out".into());
        }
        let host = host.clone();
        let (tx, rx) = mpsc::channel();
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
            Some(BODY_TRANSFER_BUDGET),
            "ureq 3.4.1+ recv_response covers headers only and recv_body is the \
             TOTAL body deadline; without it the body read has no deadline at all"
        );
        assert_eq!(t.recv_response, Some(READ_TIMEOUT));
        assert_eq!(t.connect, Some(CONNECT_TIMEOUT));
    }

    // A KEYDB body that is SLOW but PROGRESSING must finish. ureq 3.4.1 (#1194)
    // made timeout_recv_body a TOTAL deadline that no longer re-arms; the idle
    // re-cap restores the rolling bound so a steady body is not killed.
    #[test]
    fn a_slow_but_progressing_keydb_body_is_not_killed_by_the_idle_bound() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
        let pinned = listener.local_addr().expect("stub listener address");

        let server = std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("accept failed");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
            }
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\n\r\n");
            let _ = sock.flush();
            // Forty bytes, 100 ms apart: ~4 s of body, no single gap near the
            // idle bound. A ROLLING bound survives; a TOTAL interpretation fails.
            for _ in 0..40 {
                if sock.write_all(b"k").is_err() {
                    return;
                }
                let _ = sock.flush();
                std::thread::sleep(Duration::from_millis(100));
            }
        });

        // 100ms per-gap vs 1s idle bound (10x margin), ~4s total vs 1s (4x, so a
        // rolling bound passes and a total interpretation of idle fails).
        let agent = hardened_agent_with_timeouts(
            vec![pinned],
            Duration::from_secs(5),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(1),
        );
        let resp = agent
            .get("http://keydb-mirror.test/keydb.zip")
            .call()
            .expect("headers must arrive");
        let mut body = Vec::new();
        let read = resp.into_body().into_reader().read_to_end(&mut body);
        let _ = server.join();

        assert!(
            read.is_ok(),
            "a steadily-progressing body was aborted: {:?}",
            read.err()
        );
        assert_eq!(body, vec![b'k'; 40], "the whole body must arrive");
    }

    // The other half: a peer sending headers then NOTHING must be cut off by the
    // rolling idle bound, not held for the whole total budget.
    #[test]
    fn a_stalled_keydb_body_is_cut_off_by_the_idle_bound_not_the_total_budget() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
        let pinned = listener.local_addr().expect("stub listener address");

        let server = std::thread::spawn(move || {
            let (mut sock, _peer) = listener.accept().expect("accept failed");
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => head.push(byte[0]),
                }
            }
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n");
            let _ = sock.flush();
            // Promise a megabyte and send none of it; block on a read so the
            // stub ends the moment the client drops, not on a sleep.
            let mut sink = [0u8; 1];
            let _ = sock.read(&mut sink);
        });

        let idle = Duration::from_secs(1);
        let agent = hardened_agent_with_timeouts(
            vec![pinned],
            Duration::from_secs(5),
            Duration::from_secs(30),
            // A total budget far larger than the idle bound, so only the idle
            // bound can be what ends this.
            Duration::from_secs(120),
            idle,
        );
        let started = std::time::Instant::now();
        let resp = agent
            .get("http://keydb-mirror.test/keydb.zip")
            .call()
            .expect("headers must arrive");
        let mut body = Vec::new();
        let read = resp.into_body().into_reader().read_to_end(&mut body);
        let elapsed = started.elapsed();

        assert!(read.is_err(), "a stalled body must not read as success");
        assert!(
            elapsed < Duration::from_secs(20),
            "a stalled peer was held for {elapsed:?} — the idle bound did not fire"
        );
        let _ = server.join();
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
            ("198.18.0.1", "benchmarking 198.18.0.0/15 (low half)"),
            ("198.19.255.254", "benchmarking 198.18.0.0/15 (high half)"),
            ("192.0.0.1", "IETF protocol assignments 192.0.0.0/24"),
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

    // 6to4 (2002::/16) and Teredo (2001:0000::/32) tunnel an IPv4 inside an
    // IPv6 address; the guard must decode and re-check that embedded IPv4 or an
    // internal target slips through the tunnel — parity with keysources.
    #[test]
    fn ssrf_guard_blocks_embedded_ipv4_via_6to4_and_teredo() {
        // 6to4 for 127.0.0.1: 2002:7f00:0001:: (embedded in segments[1..3]).
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2002, 0x7f00, 0x0001, 0, 0, 0, 0, 0
        ))));
        // 6to4 for 169.254.169.254 (cloud metadata): 2002:a9fe:a9fe::.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2002, 0xa9fe, 0xa9fe, 0, 0, 0, 0, 0
        ))));
        // Teredo for 127.0.0.1: client IPv4 lives in the last two segments XOR
        // 0xffff, so 0x7f00^0xffff=0x80ff and 0x0001^0xffff=0xfffe.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2001, 0x0000, 0, 0, 0, 0, 0x80ff, 0xfffe
        ))));
        // A 6to4 wrapping a PUBLIC IPv4 (8.8.8.8 → 2002:0808:0808::) is allowed.
        assert!(!is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x2002, 0x0808, 0x0808, 0, 0, 0, 0, 0
        ))));
    }

    // NAT64 well-known prefix 64:ff9b::/96 (RFC 6052) translates IPv4 targets
    // into IPv6; the guard must decode the trailing IPv4 and re-check it or an
    // internal address slips through the translator — parity with keysources.
    #[test]
    fn ssrf_guard_blocks_nat64_wellknown_prefix() {
        // NAT64 for 169.254.169.254 (cloud metadata): 64:ff9b::a9fe:a9fe.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x0064, 0xff9b, 0, 0, 0, 0, 0xa9fe, 0xa9fe
        ))));
        // NAT64 for 127.0.0.1: 64:ff9b::7f00:0001.
        assert!(is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x0064, 0xff9b, 0, 0, 0, 0, 0x7f00, 0x0001
        ))));
        // NAT64 wrapping a PUBLIC IPv4 (8.8.8.8 → 64:ff9b::0808:0808) is allowed.
        assert!(!is_blocked_ip(&IpAddr::V6(Ipv6Addr::new(
            0x0064, 0xff9b, 0, 0, 0, 0, 0x0808, 0x0808
        ))));
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
