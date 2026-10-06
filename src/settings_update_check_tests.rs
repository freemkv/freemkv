use super::{UpdateTimeouts, check_for_update_at};
use std::time::Duration;

// A stub release endpoint on 127.0.0.1: read the request head, answer `head`, run `body`.
// The URL names the bound address: `localhost` tries `::1` first, and Windows takes
// about 2 s to report that refused connect, which a timing bound would count.
fn release_stub(
    head: &'static [u8],
    body: impl FnOnce(&mut std::net::TcpStream) + Send + 'static,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind stub");
    let port = listener.local_addr().expect("stub address").port();
    let server = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().expect("accept failed");
        let (mut req, mut byte) = (Vec::new(), [0u8; 1]);
        while !req.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => req.push(byte[0]),
            }
        }
        let _ = sock.write_all(head).and_then(|()| sock.flush());
        body(&mut sock);
    });
    (
        format!("http://127.0.0.1:{port}/repos/freemkv/freemkv/releases/latest"),
        server,
    )
}

fn scaled(idle: Duration) -> UpdateTimeouts {
    UpdateTimeouts {
        resolve: Duration::from_secs(5),
        connect: Duration::from_secs(5),
        headers: Duration::from_secs(5),
        idle,
    }
}

// T25 is "no answer" bounded at every step before the body; a lookup that never
// answers is one, bounded like the keydb fetch's DNS 10 s (T20). Per spec.
#[test]
fn update_check_bounds_the_dns_lookup() {
    let t = super::update_config(super::UPDATE_TIMEOUTS).timeouts();
    assert_eq!(t.resolve, Some(Duration::from_secs(10)));
    assert_eq!(t.connect, Some(Duration::from_secs(10)));
    assert_eq!(t.recv_response, Some(Duration::from_secs(10)));
    assert_eq!(t.recv_body, None, "no total");
}

// FT9a (stop design v5 §2.7, T25): "(a) body trickle past 10 s total (scaled) → a
// result". Scaled: idle (and the old total) 1 s; a byte every 250 ms, so the body runs
// 5 s while a busy CI runner still has 750 ms of slack per byte.
#[test]
fn update_check_slow_body_ok() {
    use std::io::Write as _;
    const BODY: &[u8] = br#"{"tag_name":"v9.9.9"}"#;
    let idle = Duration::from_secs(1);
    let (url, server) = release_stub(
        b"HTTP/1.1 200 OK\r\nContent-Length: 21\r\n\r\n",
        move |sock| {
            for b in BODY {
                std::thread::sleep(idle / 4);
                if sock.write_all(&[*b]).and_then(|()| sock.flush()).is_err() {
                    return;
                }
            }
        },
    );
    let msg = check_for_update_at(&url, "1.0.0", scaled(idle));
    let _ = server.join();
    assert!(msg.starts_with("Update available: 9.9.9"), "{msg}");
}

// A build ahead of the newest published release must not be told to "update" to it.
#[test]
fn update_check_does_not_advertise_a_downgrade() {
    const BODY: &[u8] = br#"{"tag_name":"v1.7.5"}"#;
    let (url, server) = release_stub(b"HTTP/1.1 200 OK\r\nContent-Length: 21\r\n\r\n", |sock| {
        use std::io::Write as _;
        let _ = sock.write_all(BODY);
    });
    let msg = check_for_update_at(&url, "1.8.0", scaled(Duration::from_secs(5)));
    let _ = server.join();
    assert!(msg.starts_with("You are running the latest"), "{msg}");
    assert!(super::is_newer("1.10.0", "1.9.9"), "numeric, not lexical");
    assert!(
        !super::is_newer("1.8.0", "1.8.0-rc1"),
        "pre-release suffix ignored"
    );
}

// FT9b (T25): "(b) → 'could not check' at idle" (§5.0: "within window + 1 s").
#[test]
fn update_check_stalled_fails() {
    use std::io::Read as _;
    let idle = Duration::from_millis(400);
    let (url, server) = release_stub(b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\n", |sock| {
        let _ = sock.read(&mut [0u8; 1]);
    });
    let started = std::time::Instant::now();
    let msg = check_for_update_at(&url, "1.0.0", scaled(idle));
    let elapsed = started.elapsed();
    let _ = server.join();
    assert!(msg.starts_with("Update check failed"), "{msg}");
    assert!(elapsed <= idle + Duration::from_secs(1), "held {elapsed:?}");
}
