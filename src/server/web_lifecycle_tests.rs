use super::*;
use std::io::Write as _;

#[test]
fn open_sse_handler_drains_promptly_on_shutdown() {
    // Isolate global cancellation from the rest of the parallel suite. This is
    // a single loopback request fixture, not a running daemon or media worker.
    const CHILD: &str = "FREEMKV_TEST_SSE_DRAIN_CHILD";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            crate::server::util::child_test(
                "server::web::lifecycle_tests::open_sse_handler_drains_promptly_on_shutdown",
                CHILD
            )
            .success()
        );
        return;
    }
    use std::io::{BufRead, BufReader};
    use std::time::Duration;
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Arc::new(RwLock::new(Config {
        autorip_dir: tmp.path().display().to_string(),
        staging_dir: tmp.path().join("staging").display().to_string(),
        ..Default::default()
    }));
    let server = Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("test-sse-handler".into())
        .spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            handle_sse(request, &cfg);
            done.send(()).unwrap();
        })
        .unwrap();
    let mut socket = std::net::TcpStream::connect(addr).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket.write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: sse\r\n\r\n").unwrap();
    let mut socket = BufReader::new(socket);
    loop {
        let mut line = String::new();
        assert_ne!(socket.read_line(&mut line).unwrap(), 0);
        if line.starts_with("data:") {
            break;
        }
    }
    // Receiving the first real frame is the barrier: the session is open and
    // would otherwise hold the tracked HTTP worker for its 600-second lifetime.
    crate::server::SHUTDOWN.store(true, Ordering::Release);
    finished
        .recv_timeout(Duration::from_secs(3))
        .expect("SSE must observe shutdown without client disconnect");
    worker.join().unwrap();
    assert!(SSE_STREAMS.lock().unwrap().is_empty());
}

// Shutdown waits for in-flight handlers, but only up to its bound.
#[test]
fn wait_for_zero_waits_for_held_handlers_within_its_bound() {
    use std::time::Duration;
    static HANDLERS: AtomicUsize = AtomicUsize::new(0);
    assert!(wait_for_zero(&HANDLERS, Duration::ZERO));
    let guard = ConnGuard::try_acquire(&HANDLERS, 1).unwrap();
    assert!(!wait_for_zero(&HANDLERS, Duration::from_millis(50)));
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        drop(guard);
    });
    assert!(wait_for_zero(&HANDLERS, Duration::from_secs(5)));
    release.join().unwrap();
}

// Only a launchd job label means a service manager will start the daemon again.
#[test]
fn only_a_launchd_job_label_counts_as_a_service_manager() {
    use std::ffi::OsStr;
    assert!(launchd_job(Some(OsStr::new("io.freemkv.library")), 1));
    for terminal in [
        None,
        Some(""),
        Some("0"),
        Some("application.com.apple.Terminal.1234"),
    ] {
        assert!(!launchd_job(terminal.map(OsStr::new), 1), "{terminal:?}");
    }
}

// A job label with a non-launchd parent (a script run under another job) is unsupervised,
// so the reboot route answers 409.
#[test]
fn a_job_label_without_a_launchd_parent_is_not_supervised() {
    use std::ffi::OsStr;
    assert!(!launchd_job(Some(OsStr::new("io.freemkv.library")), 4242));
}

// tiny_http's accept thread exits after reporting an accept error, so the port
// is dead for good: like a failed bind, that must stop the daemon visibly
// (container restart policy) rather than leave workers running with no UI.
#[test]
fn dead_listener_signals_shutdown() {
    const CHILD: &str = "FREEMKV_TEST_DEAD_LISTENER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        assert!(
            crate::server::util::child_test(
                "server::web::lifecycle_tests::dead_listener_signals_shutdown",
                CHILD
            )
            .success()
        );
        return;
    }
    let cfg = Arc::new(RwLock::new(Config::default()));
    serve(&cfg, |_| Err(std::io::Error::other("Too many open files")));
    assert!(crate::server::SHUTDOWN.load(Ordering::Acquire));
}

// #137: an SSE client that stops reading (half-open) cannot pin the handler thread: its
// connection gets a write timeout, so the blocked write fails and the session ends.
#[cfg(unix)]
#[test]
fn a_client_that_stops_reading_times_the_write_out() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    client
        .write_all(b"GET /events HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let request = server.recv().unwrap();
    let peer = *request.remote_addr().unwrap();
    assert!(super::set_write_timeout_for_peer(
        peer,
        std::time::Duration::from_millis(200)
    ));
    let mut stream = request.upgrade("sse", tiny_http::Response::empty(200));
    // The writer runs on its own thread: without the timeout it blocks for good, and the
    // test fails at the bound instead of hanging.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let chunk = vec![0u8; 64 * 1024];
        let err = loop {
            if let Err(e) = stream.write_all(&chunk).and_then(|_| stream.flush()) {
                break e;
            }
        };
        let _ = tx.send(err);
    });
    let err = rx
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the blocked write never timed out");
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "{err:?}"
    );
    // And the route sets it: handle_sse calls it before the upgrade.
    let src = include_str!("web.rs");
    let at = src.find("\nfn handle_sse(").unwrap();
    let body = &src[at..at + src[at..].find("\n}\n").unwrap()];
    let set = body
        .find("set_write_timeout_for_peer(peer, SSE_WRITE_TIMEOUT)")
        .expect("set");
    assert!(set < body.find("request.upgrade(").unwrap());
}

// Reboots are spaced: none within a minute of this process starting or of the last one.
#[test]
fn reboots_are_spaced_a_minute_apart() {
    let now = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let ago = |s| now - std::time::Duration::from_secs(s);
    assert!(reboot_allowed(now, None, None));
    assert!(reboot_allowed(now, Some(ago(61)), Some(ago(120))));
    assert!(!reboot_allowed(now, Some(ago(10)), None), "just started");
    assert!(
        !reboot_allowed(now, Some(ago(300)), Some(ago(30))),
        "just requested"
    );
}

// KeepAlive true, or conditions with SuccessfulExit false, as plutil extracts them: a
// condition dict is no raw scalar, so only its SuccessfulExit answers.
#[test]
fn only_keepalive_true_or_a_failed_exit_condition_restarts_the_job() {
    assert!(keep_alive_restarts(Some("true"), None));
    assert!(keep_alive_restarts(None, Some("false")));
    for (keep, exit) in [(Some("false"), None), (None, None), (None, Some("true"))] {
        assert!(!keep_alive_restarts(keep, exit), "{keep:?} {exit:?}");
    }
}

// Only KeepAlive true (or conditions) makes launchd start the job again after it exits.
#[cfg(target_os = "macos")]
#[test]
fn only_a_keepalive_plist_restarts_the_job() {
    let dir = tempfile::tempdir().unwrap();
    let plist = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(
            &p,
            format!("<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>{body}</dict></plist>"),
        )
        .unwrap();
        p
    };
    let keep = plist("t.plist", "<key>KeepAlive</key><true/>");
    let cond = plist(
        "c.plist",
        "<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>",
    );
    let off = plist("f.plist", "<key>KeepAlive</key><false/>");
    let none = plist("n.plist", "<key>Label</key><string>x</string>");
    // launchd does not start these again after an exit 75.
    let only_success = plist(
        "s.plist",
        "<key>KeepAlive</key><dict><key>SuccessfulExit</key><true/></dict>",
    );
    let crashed = plist(
        "k.plist",
        "<key>KeepAlive</key><dict><key>Crashed</key><true/></dict>",
    );
    let network = plist(
        "w.plist",
        "<key>KeepAlive</key><dict><key>NetworkState</key><true/></dict>",
    );
    let with_more = plist(
        "m.plist",
        "<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/><key>Crashed</key><true/></dict>",
    );
    assert!(plist_keeps_alive(&keep));
    assert!(plist_keeps_alive(&cond));
    assert!(plist_keeps_alive(&with_more));
    for p in [&off, &none, &only_success, &crashed, &network] {
        assert!(!plist_keeps_alive(p), "{}", p.display());
    }
    assert!(!plist_keeps_alive(&dir.path().join("missing.plist")));
}

// The loaded job decides when launchctl knows it: its keepalive property, else the plist
// file at its path. A job launchctl does not list in the user's domain (a LaunchDaemon) is
// refused.
#[test]
fn the_loaded_launchd_job_decides_over_the_installed_plist() {
    let loaded = std::path::Path::new("/Users/me/Library/LaunchAgents/loaded.plist");
    let stale = std::path::Path::new("/Users/me/Library/LaunchAgents/stale.plist");
    let keeps_alive = |p: &std::path::Path| p == loaded;
    let print = |path: &std::path::Path, props: &str| {
        format!(
            "gui/501/io.freemkv.library = {{\n\tpath = {}\n\ttype = LaunchAgent\n\tenvironment = {{\n\t\tpath = /nope\n\t}}\n\tproperties = {props}\n}}\n",
            path.display()
        )
    };
    let job = Launchctl::Loaded(print(loaded, "runatload | inferred program"));
    assert!(
        launchd_restarts(&job, stale, keeps_alive),
        "the loaded job's plist, not the installed one"
    );
    let listed_kept = Launchctl::Loaded(print(stale, "keepalive | runatload"));
    assert!(
        launchd_restarts(&listed_kept, stale, keeps_alive),
        "launchd lists the loaded job as kept alive"
    );
    let not_kept_anywhere = Launchctl::Loaded(print(stale, "runatload"));
    assert!(!launchd_restarts(&not_kept_anywhere, loaded, keeps_alive));
    assert!(
        !launchd_restarts(&Launchctl::NotLoaded, loaded, keeps_alive),
        "a LaunchDaemon"
    );
    assert!(launchd_restarts(
        &Launchctl::Unavailable,
        loaded,
        keeps_alive
    ));
    assert!(!launchd_restarts(
        &Launchctl::Unavailable,
        stale,
        keeps_alive
    ));
}

// launchd lists no "keepalive" property for a conditional KeepAlive {SuccessfulExit =
// false} (it shows a semaphores block): the loaded job's plist decides. Shape captured from
// `launchctl print gui/501/<label>` of such an agent.
#[test]
fn a_conditional_keepalive_of_the_loaded_job_restarts_it() {
    let plist = std::path::Path::new("/Users/me/Library/LaunchAgents/io.freemkv.library.plist");
    let print = format!(
        "gui/501/io.freemkv.library = {{\n\tactive count = 1\n\tpath = {}\n\ttype = LaunchAgent\n\
         \tstate = running\n\n\tenvironment = {{\n\t\tXPC_SERVICE_NAME => io.freemkv.library\n\t}}\n\n\
         \tdomain = gui/501 [100002]\n\tminimum runtime = 10\n\texit timeout = 5\n\truns = 2\n\
         \tsemaphores = {{\n\t\tsuccessful exit => 0\n\t}}\n\n\
         \tproperties = runatload | managed LWCR | has LWCR\n}}\n",
        plist.display()
    );
    let missing = std::path::Path::new("/Users/me/not-installed.plist");
    let job = Launchctl::Loaded(print);
    assert!(launchd_restarts(&job, missing, |p| p == plist));
    assert!(!launchd_restarts(&job, missing, |_| false));
}

// A metadata refusal is a conflict (409); storage that may answer again is 503, other
// storage failures 500 — never a conflict.
#[test]
fn metadata_errors_map_storage_failures_to_server_errors() {
    use std::io::{Error, ErrorKind};
    assert_eq!(metadata_error_status(&Error::other("not a held rip")), 409);
    for kind in [
        ErrorKind::TimedOut,
        ErrorKind::StaleNetworkFileHandle,
        ErrorKind::ResourceBusy,
    ] {
        assert_eq!(metadata_error_status(&Error::from(kind)), 503, "{kind:?}");
    }
    for kind in [ErrorKind::PermissionDenied, ErrorKind::StorageFull] {
        assert_eq!(metadata_error_status(&Error::from(kind)), 500, "{kind:?}");
    }
}
