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
#[path = "keydb_fetch_tests.rs"]
mod tests;
