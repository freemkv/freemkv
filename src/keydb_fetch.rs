//! TLS-capable keydb fetch for `freemkv update-keys`.
//!
//! `fetch(url)` retrieves keydb bytes over `http://` or `https://` via
//! `ureq` and returns the raw response body; hand it to
//! `freemkv_keysources::KeydbSource::save` for verify + atomic save.
//!
//! Hardened like the online key service: resolves the host, refuses
//! unreachable addresses (LAN is fine), pins the validated addresses into the agent, follows zero redirects,
//! and bounds the connect/read timeouts and the response body size.
//!

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
    let pinned = resolve_and_guard(url).map_err(|r| {
        tracing::warn!(host = %host_of(url), reason = %r, "keydb URL refused before connecting");
        r.into_error(url)
    })?;
    get_body(&hardened_agent(pinned), url, MAX_BODY_BYTES)
}

// One GET through `agent`: the body, or the error it maps to. The agent follows no redirect, so
// a 3xx comes back as a response and is refused here with the other non-2xx statuses.
fn get_body(agent: &ureq::Agent, url: &str, cap: u64) -> Result<Vec<u8>> {
    let resp = agent.get(url).call().map_err(|e| map_ureq_err(url, &e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::KeydbHttp {
            status: status.as_u16(),
        });
    }
    read_capped(resp.into_body().into_reader(), cap).map_err(|e| cap_error(&e, url))
}

// Which keydb error a capped read's failure is: an over-large body is E8002 (content), a dead
// socket is E8000 (network, retry).
fn cap_error(e: &CapError, url: &str) -> Error {
    match e {
        CapError::TooLarge => Error::KeydbInvalid,
        CapError::Io => Error::KeydbConnect { host: host_of(url) },
    }
}

// Why a capped read did not produce a body — the two outcomes [`cap_error`] tells apart.
#[derive(Debug)]
enum CapError {
    /// The body ran past the cap. A statement about the response.
    TooLarge,
    /// The socket failed part-way. A statement about the network.
    Io,
}

// Read at most `cap` bytes, rejecting anything larger. Split out of [`fetch`] so the cap is
// directly testable without real network I/O.
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

// Why [`resolve_and_guard`] refused a URL. A policy refusal must not read as a dead server.
#[derive(Debug)]
enum Refusal {
    /// Not `http://` / `https://`; carries the scheme.
    Scheme(String),
    /// The host resolved to an address no connection can reach.
    Blocked(IpAddr),
    /// No usable host, or it did not resolve: a connect-class failure.
    Unreachable(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Scheme(s) => write!(f, "unsupported scheme '{s}'"),
            Refusal::Blocked(ip) => write!(f, "refusing invalid address {ip}"),
            Refusal::Unreachable(why) => f.write_str(why),
        }
    }
}

impl Refusal {
    // E8006 is "...isn't allowed ({detail})", which already reads as a
    // policy refusal for a bare IP. `{detail}` must stay locale-neutral — do
    // not inject English words here; the raw address is the whole payload.
    fn into_error(self, url: &str) -> Error {
        match self {
            Refusal::Scheme(scheme) => Error::KeydbUnsupportedScheme { scheme },
            Refusal::Blocked(ip) => Error::KeydbUnsupportedScheme {
                scheme: ip.to_string(),
            },
            Refusal::Unreachable(_) => Error::KeydbConnect { host: host_of(url) },
        }
    }
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
    let rest = split_http_scheme(url).map_or(url, |(_, rest)| rest);
    authority_of(rest).to_string()
}

// The `http`/`https` scheme of `url` (matched case-insensitively, RFC 3986 §3.1) and the rest
// after `://`; the scheme comes back lowercase.
fn split_http_scheme(url: &str) -> Option<(&'static str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.eq_ignore_ascii_case("https") {
        Some(("https", rest))
    } else if scheme.eq_ignore_ascii_case("http") {
        Some(("http", rest))
    } else {
        None
    }
}

// `host[:port]` of the text after `://`, without any userinfo, path, query or fragment.
fn authority_of(rest: &str) -> &str {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.rsplit('@').next().unwrap_or(authority)
}

// `ResolvedSocketAddrs` is a fixed 16-slot array; keep only the first 16.
const MAX_PINNED_ADDRS: usize = 16;

// The pinned-address resolver behind [`hardened_agent`], mirroring the one in
// `freemkv-keysources::online`. Must be wired via `Agent::with_parts`, never `new_with_config`
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

/// An agent whose body reads re-arm `idle` (T25 shares T20's connector, stop design §2.7).
// The update check is GUI-only; the CLI binary builds this module without it.
#[allow(dead_code)]
pub(crate) fn idle_agent(config: Config, idle: Duration) -> ureq::Agent {
    use ureq::unversioned::transport::Connector as _;
    ureq::Agent::with_parts(
        config,
        DefaultConnector::new().chain(IdleReCapConnector { idle }),
        ureq::unversioned::resolver::DefaultResolver::default(),
    )
}

// Chained after DefaultConnector to re-arm a ROLLING per-read idle bound on every body read,
// restoring the stall detection ureq 3.4.1 removed (#1194).
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
    // Cap BODY reads, and any wait ureq leaves unbounded (with no body total the body
    // phase reports `NotHappening`), at the idle bound; connect and headers keep ureq's own.
    fn cap(
        &self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> ureq::unversioned::transport::NextTimeout {
        use ureq::unversioned::transport::time::Duration as UreqDuration;
        if timeout.reason != ureq::Timeout::RecvBody && !timeout.after.is_not_happening() {
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

// A ureq agent that follows zero redirects and pins DNS resolution to `pinned` (addresses
// already validated by [`resolve_and_guard`]).
fn hardened_agent(pinned: Vec<SocketAddr>) -> ureq::Agent {
    hardened_agent_with_timeouts(pinned, CONNECT_TIMEOUT, READ_TIMEOUT, STALL_TIMEOUT)
}

// The agent builder with caller-chosen timeouts so the rolling-idle behaviour
// is testable with short bounds. `response` bounds header arrival; `idle` is the
// rolling stall bound via IdleReCapConnector.
fn hardened_agent_with_timeouts(
    pinned: Vec<SocketAddr>,
    connect: Duration,
    response: Duration,
    idle: Duration,
) -> ureq::Agent {
    // Stop design v5 §2.7 (T20): "keep each no-answer bound and each rolling idle bound,
    // and **drop every total**". The body is bounded by `idle` and the byte cap only.
    let config = Config::builder()
        .max_redirects(0)
        .timeout_connect(Some(connect))
        .timeout_recv_response(Some(response))
        .timeout_recv_body(None)
        // Never the env proxy: PinnedResolver would answer the proxy's lookup with the keydb
        // server's address, and a real proxy re-resolves the host, bypassing the guard.
        .proxy(None)
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

/// Resolve `url`'s host and refuse unreachable addresses (LAN is allowed). Returns the pinned socket addresses on success, or why it refused.
fn resolve_and_guard(url: &str) -> std::result::Result<Vec<SocketAddr>, Refusal> {
    let no_route = |why: &str| Refusal::Unreachable(why.to_string());
    let (rest, default_port) = if let Some((scheme, r)) = split_http_scheme(url) {
        (r, if scheme == "https" { 443u16 } else { 80u16 })
    } else {
        return Err(match url.split_once("://") {
            Some((scheme, _)) if !scheme.is_empty() => Refusal::Scheme(scheme.to_string()),
            _ => no_route("URL must start with http:// or https://"),
        });
    };
    let authority = authority_of(rest);
    if authority.is_empty() {
        return Err(no_route("URL has no host"));
    }
    let (host, port): (String, u16) = if let Some(stripped) = authority.strip_prefix('[') {
        match stripped.split_once(']') {
            Some((h, after)) => {
                let p = after
                    .strip_prefix(':')
                    .map(|s| s.parse::<u16>().map_err(|_| no_route("invalid port")))
                    .transpose()?
                    .unwrap_or(default_port);
                (h.to_string(), p)
            }
            None => return Err(no_route("malformed IPv6 host")),
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
        return Err(no_route("URL has no host"));
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
            return Err(no_route("DNS resolution timed out"));
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
            Ok(Err(e)) => return Err(Refusal::Unreachable(format!("could not resolve host: {e}"))),
            Err(_) => return Err(no_route("DNS resolution timed out")),
        }
    };
    if addrs.is_empty() {
        return Err(no_route("host did not resolve to any address"));
    }
    for a in &addrs {
        if libfreemkv::mux::is_blocked_ip(a.ip()) {
            return Err(Refusal::Blocked(a.ip()));
        }
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
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

    // FT7a (T20, §5.0 pair (a)): "a local server trickles a byte every 0.5 × idle, total >
    // the old 120 s budget (scaled) → `Ok`". Scaled: idle 400 ms, so the old total is
    // 2.4 s (120 s at a 20 s idle); twenty bytes 200 ms apart take 4 s.
    #[test]
    fn keydb_fetch_slow_body_past_old_budget_succeeds() {
        use std::io::{Read as _, Write as _};
        let idle = Duration::from_millis(400);
        let (pinned, server) = keydb_stub(
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n",
            move |sock| {
                for _ in 0..20 {
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
        assert_eq!(body, vec![b'k'; 20], "the whole body must arrive");
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
