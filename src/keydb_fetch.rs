//! TLS-capable keydb fetch for `freemkv update-keys`.
//!
//! libfreemkv's own keydb downloader (`libfreemkv::keydb::http_get`) is
//! deliberately dependency-light: raw `TcpStream`, plaintext HTTP only, no
//! TLS. That keeps the library lean, but it rejects an `https://` keydb URL
//! with `KeydbUnsupportedScheme`.
//!
//! The CLI already depends on `ureq` (for the online key service), so the
//! `update-keys` command routes its FETCH through `ureq` here — handling BOTH
//! `http://` and `https://` — and then hands the raw bytes to
//! `libfreemkv::keydb::save`, which reuses the library's existing
//! parse/verify/atomic-save path. No new dependency, and libfreemkv's own
//! `http_get` stays untouched for the daily-refresh thread.
//!
//! The fetch is hardened the same way as the online key service
//! (`freemkv-keysources::online`): resolve + SSRF-guard the host immediately
//! before the request and pin the validated addresses into the agent, follow
//! zero redirects (so a public URL can't 30x to an internal host), and bound
//! the connect/read timeouts and the response body size.

use libfreemkv::{Error, Result};
use std::io::Read;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

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
/// to `libfreemkv::keydb::save` for verify + atomic save.
pub fn fetch(url: &str) -> Result<Vec<u8>> {
    // An SSRF rejection (or a malformed/unsupported URL) surfaces as a connect
    // failure — the request never leaves the host. The user sees the localized
    // E8000 "could not connect" message keyed on the host.
    let pinned = resolve_and_guard(url).map_err(|_| Error::KeydbConnect { host: host_of(url) })?;
    let agent = hardened_agent(pinned);
    let resp = agent.get(url).call().map_err(|e| map_ureq_err(url, &e))?;
    let mut buf = Vec::new();
    // Read one byte past the cap so an over-cap body is detectable and rejected.
    resp.into_reader()
        .take(MAX_BODY_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|_| Error::KeydbConnect { host: host_of(url) })?;
    if buf.len() as u64 > MAX_BODY_BYTES {
        return Err(Error::KeydbInvalid);
    }
    Ok(buf)
}

/// Map a `ureq` transport/HTTP error to a libfreemkv keydb error so the CLI
/// renders it through the existing `error.E8xxx` locale strings.
fn map_ureq_err(url: &str, e: &ureq::Error) -> Error {
    match e {
        // A non-2xx HTTP status (the server answered, but not 200-ish).
        ureq::Error::Status(code, _) => Error::KeydbHttp { status: *code },
        // Transport-level failure (DNS, connect, TLS, timeout, dropped conn).
        ureq::Error::Transport(_) => Error::KeydbConnect { host: host_of(url) },
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

/// Build a ureq agent that follows zero redirects (so a public URL can't
/// 30x-redirect to an internal host) and pins DNS resolution to `pinned`
/// (the addresses already validated by [`resolve_and_guard`]).
fn hardened_agent(pinned: Vec<SocketAddr>) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .resolver(move |_netloc: &str| Ok(pinned.clone()))
        .build()
}

// ── SSRF guard (mirrors freemkv-keysources::online) ─────────────────────────
//
// The keydb URL is operator-supplied. An attacker who controls its DNS could
// rebind the host to 169.254.169.254 (cloud metadata) or an RFC1918 host. We
// resolve once just before the request, reject any blocked IP, and pin the
// validated addresses into the agent so a later DNS flip can't redirect the
// request; redirects(0) blocks a 30x to an internal host.

/// True when `ip` must never be the target of an outbound keydb fetch. Blocks
/// loopback, link-local (incl. 169.254.0.0/16 cloud metadata), RFC1918, CGNAT,
/// multicast, unspecified, reserved, and the IPv4-mapped equivalents.
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
    // Bounded DNS: `to_socket_addrs` is a blocking lookup that can hang for the
    // OS resolver timeout; run it on a thread and join with a deadline.
    let addrs: Vec<SocketAddr> = {
        use std::sync::mpsc;
        let host = host.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let res = (host.as_str(), port)
                .to_socket_addrs()
                .map(|it| it.collect::<Vec<SocketAddr>>());
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
}
