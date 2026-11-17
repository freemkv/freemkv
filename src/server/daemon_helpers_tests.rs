//! Daemon helpers with no process-wide side effects: the health probe, the KEYDB fetch
//! logging, bounded child waits, chown and the NFS mount fallbacks.
use super::*;
use std::io::{Read, Write};

// A one-shot HTTP listener answering every request with `reply`.
fn serve(reply: &'static [u8]) -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = sock.read(&mut buf);
        let _ = sock.write_all(reply);
    });
    (port, h)
}

// A port nothing listens on: bound, then released.
fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[test]
fn the_health_probe_passes_only_on_a_200_status_line() {
    let (port, h) = serve(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    assert_eq!(probe_health(port), 0);
    h.join().unwrap();
    let (port, h) = serve(b"HTTP/1.0 200 OK\r\n\r\n");
    assert_eq!(probe_health(port), 0);
    h.join().unwrap();
    let (port, h) = serve(b"HTTP/1.1 503 Service Unavailable\r\n\r\n");
    assert_eq!(probe_health(port), 1);
    h.join().unwrap();
    // EOF before a full status line.
    let (port, h) = serve(b"HTTP/1");
    assert_eq!(probe_health(port), 1);
    h.join().unwrap();
    assert_eq!(probe_health(closed_port()), 1);
}

fn keydb_cfg(path: &std::path::Path) -> std::sync::RwLock<config::Config> {
    std::sync::RwLock::new(config::Config {
        keydb_path: Some(path.to_string_lossy().into_owned()),
        ..config::Config::default()
    })
}

#[test]
fn fetch_keydb_saves_a_good_download_in_both_wordings() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("keydb.cfg");
    let cfg = keydb_cfg(&dest);
    for boot in [true, false] {
        let _ = std::fs::remove_file(&dest);
        let (port, h) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 42\r\nConnection: close\r\n\r\n\
              0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF = Test\n",
        );
        fetch_keydb(&cfg, &format!("http://127.0.0.1:{port}/k"), boot);
        h.join().unwrap();
        assert!(dest.is_file(), "boot={boot}: keydb not saved");
    }
}

#[test]
fn fetch_keydb_logs_each_failure_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("keydb.cfg");
    let cfg = keydb_cfg(&dest);
    for boot in [true, false] {
        let (port, h) = serve(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        fetch_keydb(&cfg, &format!("http://127.0.0.1:{port}/k"), boot);
        h.join().unwrap();
        fetch_keydb(&cfg, "ftp://example.com/k", boot);
    }
    assert!(!dest.exists());
    // A save under a regular file cannot create its directory.
    let blocker = tmp.path().join("file");
    std::fs::write(&blocker, b"x").unwrap();
    let cfg = keydb_cfg(&blocker.join("sub").join("keydb.cfg"));
    for boot in [true, false] {
        let (port, h) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Length: 42\r\nConnection: close\r\n\r\n\
              0xDEADBEEFDEADBEEFDEADBEEFDEADBEEF = Test\n",
        );
        fetch_keydb(&cfg, &format!("http://127.0.0.1:{port}/k"), boot);
        h.join().unwrap();
    }
}

#[cfg(unix)]
#[test]
fn wait_bounded_returns_a_quick_exit_and_kills_a_slow_child() {
    let quick = std::process::Command::new("true").spawn().unwrap();
    let st = wait_bounded(quick, Duration::from_secs(10)).expect("exited in time");
    assert!(st.success());
    let slow = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let t = Instant::now();
    assert!(wait_bounded(slow, Duration::from_millis(200)).is_none());
    assert!(t.elapsed() < Duration::from_secs(10));
}

#[cfg(unix)]
fn current_user() -> String {
    let pwd = unsafe { libc::getpwuid(libc::getuid()) };
    assert!(!pwd.is_null());
    unsafe { std::ffi::CStr::from_ptr((*pwd).pw_name) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(unix)]
#[test]
fn chown_recursive_walks_the_tree_without_following_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("tree");
    std::fs::create_dir_all(root.join("a").join("b")).unwrap();
    std::fs::write(root.join("a").join("f"), b"x").unwrap();
    std::os::unix::fs::symlink("/nonexistent-target", root.join("dangling")).unwrap();
    chown_recursive(&root, &current_user()).expect("chown to self succeeds");
    let err = chown_recursive(&root, "fmkv-no-such-user-zz").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    let err = chown_recursive(&root, "bad\0user").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let err = chown_recursive(&tmp.path().join("missing"), &current_user()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn is_mountpoint_agrees_with_the_mount_table() {
    let table = mount_table().unwrap_or_default();
    assert_eq!(is_mountpoint("/"), listed_in(&table, "/"));
    assert!(!is_mountpoint("/fmkv-surely-not-mounted"));
}

#[cfg(unix)]
#[test]
fn an_unmountable_share_reports_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let share = NfsShare {
        source: "127.0.0.1:/fmkv-no-such-export".into(),
        mountpoint: tmp.path().display().to_string(),
        opts: "vers=4.1,soft,timeo=1,retrans=0,retry=0".into(),
    };
    assert!(!mount_nfs(&share));
    assert!(!remount_nfs(&share, false));
    // Not mounted, so the lazy unmount fails and no mount is attempted.
    assert!(!remount_nfs(&share, true));
}
