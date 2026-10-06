use super::*;

// LAST is process-wide; tests that publish to it take this.
pub(crate) static LAST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn a_stale_share_is_remounted_once_per_gap() {
    let mp = Path::new("/mnt/nfs");
    let stale = vec![bad_mount(
        "Library",
        Path::new("/mnt/nfs/movies"),
        Fault::Stale,
    )];
    let t0 = Instant::now();
    assert!(wants_remount(&stale, mp, true, None, t0));
    assert!(!wants_remount(
        &stale,
        mp,
        true,
        Some(t0),
        t0 + Duration::from_secs(5)
    ));
    assert!(wants_remount(&stale, mp, true, Some(t0), t0 + REMOUNT_GAP));
}

#[test]
fn a_share_left_unmounted_is_retried_once_per_gap() {
    let mp = Path::new("/mnt/nfs");
    let table = "proc /proc proc rw 0 0\n/dev/sda1 / ext4 rw 0 0\n";
    let mounted = crate::server::daemon::listed_in(table, "/mnt/nfs");
    assert!(!mounted);
    // After a failed mount the folders read as healthy local dirs, or as missing.
    let bare = vec![
        ok_mount("Output", Path::new("/mnt/nfs")),
        bad_mount("Library", Path::new("/mnt/nfs/movies"), Fault::Missing),
    ];
    let t0 = Instant::now();
    assert!(wants_remount(&bare, mp, mounted, None, t0));
    assert!(!wants_remount(
        &bare,
        mp,
        mounted,
        Some(t0),
        t0 + Duration::from_secs(30)
    ));
    assert!(wants_remount(
        &bare,
        mp,
        mounted,
        Some(t0),
        t0 + REMOUNT_GAP
    ));
    assert!(!wants_remount(&bare, mp, true, None, t0));
}

#[test]
fn a_folder_under_an_unmounted_share_is_bare() {
    let mounted = "srv:/export /mnt/nfs nfs4 rw,vers=4.1 0 0\n/dev/sda1 / ext4 rw 0 0\n";
    let unmounted = "/dev/sda1 / ext4 rw 0 0\n/dev/sdb1 /mnt/nfs2 ext4 rw 0 0\n";
    let movies = Path::new("/mnt/nfs/movies");
    assert!(bare_share(movies, Some("/mnt/nfs"), Some(unmounted)));
    assert!(bare_share(
        Path::new("/mnt/nfs"),
        Some("/mnt/nfs/"),
        Some(unmounted)
    ));
    assert!(!bare_share(movies, Some("/mnt/nfs"), Some(mounted)));
    assert!(!bare_share(movies, Some("/mnt/nfs/"), Some(mounted)));
    // Outside the share, no share configured, or no table to read: nothing is proven.
    assert!(!bare_share(
        Path::new("/data/stage"),
        Some("/mnt/nfs"),
        Some(unmounted)
    ));
    assert!(!bare_share(movies, None, Some(unmounted)));
    assert!(!bare_share(movies, Some("/mnt/nfs"), None));
    // The kernel escapes a space in a mount path as \040.
    let spaced = "srv:/e /mnt/my\\040share nfs4 rw 0 0\n";
    assert!(!bare_share(
        Path::new("/mnt/my share/tv"),
        Some("/mnt/my share"),
        Some(spaced)
    ));
}

#[test]
fn only_a_stale_handle_under_the_share_triggers_a_remount() {
    let mp = Path::new("/mnt/nfs");
    let t0 = Instant::now();
    let elsewhere = vec![bad_mount("Staging", Path::new("/data/stage"), Fault::Stale)];
    let other_fault = vec![bad_mount(
        "Library",
        Path::new("/mnt/nfs/movies"),
        Fault::Io,
    )];
    let healthy = vec![ok_mount("Library", Path::new("/mnt/nfs/movies"))];
    assert!(!wants_remount(&elsewhere, mp, true, None, t0));
    assert!(!wants_remount(&other_fault, mp, true, None, t0));
    assert!(!wants_remount(&healthy, mp, true, None, t0));
}

/// Put `mounts` in place as if a check had just published them.
pub(crate) fn set_mounts(mounts: Vec<Mount>) {
    publish(mounts, true);
}

pub(crate) fn ok_mount(role: &'static str, path: &Path) -> Mount {
    Mount::blank(role, path).passed()
}

pub(crate) fn bad_mount(role: &'static str, path: &Path, fault: Fault) -> Mount {
    Mount::blank(role, path).failed(fault, "test".into())
}

fn errno(n: i32) -> std::io::Error {
    std::io::Error::from_raw_os_error(n)
}

#[test]
fn a_folder_is_checked_for_presence_and_writes() {
    let t = tempfile::tempdir().unwrap();
    let m = check("Output", t.path(), true);
    assert!(m.ok && m.writable == Some(true), "{m:?}");
    assert_eq!(m.state, State::Ok);
    assert_eq!(
        std::fs::read_dir(t.path()).unwrap().count(),
        0,
        "the check wrote a file"
    );
    let gone = check("Output", &t.path().join("nope"), true);
    assert!(!gone.ok);
    assert_eq!(gone.problem.as_deref(), Some("missing"));
    assert_eq!(gone.fault, Some(Fault::Missing));
    assert!(gone.message.unwrap().contains("output folder is missing"));
}

#[cfg(unix)]
#[test]
fn a_read_only_folder_is_reported_not_writable() {
    use std::os::unix::fs::PermissionsExt as _;
    let t = tempfile::tempdir().unwrap();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let m = check("Output", t.path(), true);
    let ro = std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o755));
    ro.unwrap();
    if unsafe { libc::geteuid() } == 0 {
        return; // root passes access(W_OK) on a 0555 dir
    }
    assert!(!m.ok, "{m:?}");
    assert_eq!(m.writable, Some(false));
    assert_eq!(m.problem.as_deref(), Some("not writable"));
    assert_eq!(m.fault, Some(Fault::Denied));
    // A folder only read from is fine.
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let r = check("Source ISOs", t.path(), false);
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.ok, "{r:?}");
}

#[test]
fn a_stuck_check_is_never_doubled() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("stuck");
    let late = Instant::now() - Duration::from_secs(1);
    IN_FLIGHT.folders.lock().unwrap().push((p.clone(), late));
    let started = Instant::now();
    let m = check_bounded("Library", p.clone(), false);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a hung folder answers at once"
    );
    assert!(!m.ok);
    assert_eq!(m.state, State::Unresponsive);
    assert!(m.problem.unwrap().contains("not responding"));
    let n = IN_FLIGHT
        .folders
        .lock()
        .unwrap()
        .iter()
        .filter(|(x, _)| *x == p)
        .count();
    assert_eq!(n, 1);
    release(&p);
    let fine = check_bounded("Library", t.path().to_path_buf(), false);
    assert!(fine.ok);
    assert!(
        !IN_FLIGHT
            .folders
            .lock()
            .unwrap()
            .iter()
            .any(|(x, _)| x == t.path())
    );
}

#[test]
fn a_hung_probe_returns_within_its_limit_and_is_never_started_twice() {
    let key = PathBuf::from("/test/hung-probe");
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let runs = Arc::new(AtomicU64::new(0));
    let r = runs.clone();
    let started = Instant::now();
    let out = bounded(&key, Duration::from_millis(200), move || {
        r.fetch_add(1, Ordering::SeqCst);
        let _ = rx.recv(); // a kernel call that never returns
    });
    let took = started.elapsed();
    assert_eq!(out, Bounded::TimedOut);
    assert!(took < Duration::from_millis(1500), "{took:?}");
    // The next probe of the same folder neither waits nor spawns.
    let r = runs.clone();
    let started = Instant::now();
    let again = bounded(&key, Duration::from_millis(200), move || {
        r.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(again, Bounded::Busy);
    assert!(started.elapsed() < Duration::from_millis(100));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "one probe in flight per folder"
    );
    // Once the kernel lets go, the folder is probed again.
    drop(tx);
    let t = Instant::now();
    while IN_FLIGHT
        .folders
        .lock()
        .unwrap()
        .iter()
        .any(|(p, _)| *p == key)
    {
        assert!(t.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        bounded(&key, Duration::from_secs(1), || 7),
        Bounded::Done(7)
    );
}

#[test]
fn a_probe_already_running_but_not_yet_late_is_waited_for() {
    let key = PathBuf::from("/test/slow-probe");
    let first = {
        let key = key.clone();
        std::thread::spawn(move || {
            bounded(&key, Duration::from_secs(2), || {
                std::thread::sleep(Duration::from_millis(150));
                1
            })
        })
    };
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(
        bounded(&key, Duration::from_secs(2), || 2),
        Bounded::Done(2)
    );
    assert_eq!(first.join().unwrap(), Bounded::Done(1));
}

#[test]
fn preflight_maps_each_failure_to_its_fault_in_bounded_time() {
    let t = tempfile::tempdir().unwrap();
    assert_eq!(preflight("Library", t.path()), Ok(()));
    assert_eq!(
        std::fs::read_dir(t.path()).unwrap().count(),
        0,
        "the probe file is removed"
    );
    let missing = preflight("Library", &t.path().join("gone")).unwrap_err();
    assert_eq!(missing.fault, Fault::Missing);
    #[cfg(unix)]
    for (n, fault, words) in [
        (libc::ESTALE, Fault::Stale, "remounted"),
        (libc::EIO, Fault::Io, "I/O error"),
        (libc::EROFS, Fault::ReadOnly, "read-only"),
        (libc::EACCES, Fault::Denied, "not allowed"),
        (libc::ETIMEDOUT, Fault::Unresponsive, "not responding"),
    ] {
        let dir = t.path().join(format!("errno-{n}"));
        let p = preflight_with("Library", &dir, Duration::from_secs(1), move |_| {
            Err(errno(n))
        })
        .unwrap_err();
        assert_eq!(p.fault, fault);
        assert!(p.message.contains(words), "{}", p.message);
        assert!(p.message.contains("library folder"), "{}", p.message);
    }
    let hung = t.path().join("hung");
    let started = Instant::now();
    let p = preflight_with("Output", &hung, Duration::from_millis(300), |_| {
        std::thread::sleep(Duration::from_secs(3));
        Ok(())
    })
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(p.fault, Fault::Unresponsive);
    let again = Instant::now();
    let p = preflight_with("Output", &hung, Duration::from_millis(300), |_| Ok(())).unwrap_err();
    assert!(
        again.elapsed() < Duration::from_millis(100),
        "no second probe of a hung folder"
    );
    assert!(p.detail.contains("never came back"));
}

// A FIFO with no writer blocks open(2) for read in the kernel: a real hang, like a hard mount.
#[cfg(unix)]
#[test]
fn a_probe_blocked_in_the_kernel_is_abandoned_on_time() {
    let t = tempfile::tempdir().unwrap();
    let fifo = t.path().join("fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let started = Instant::now();
    let f = fifo.clone();
    let p = preflight_with("Output", &fifo, Duration::from_millis(500), move |_| {
        std::fs::read(&f).map(drop)
    })
    .unwrap_err();
    let took = started.elapsed();
    assert!(took < Duration::from_millis(2000), "{took:?}");
    assert_eq!(p.fault, Fault::Unresponsive);
    // Unblock the leaked probe so the test leaves no thread behind.
    let _ = std::fs::OpenOptions::new().write(true).open(&fifo);
}

#[test]
fn errors_that_lost_their_code_are_read_from_their_text() {
    let e = std::io::Error::other("unreadable: Stale file handle (os error 116)");
    #[cfg(target_os = "linux")]
    assert_eq!(Fault::of(&e), Fault::Stale);
    assert_eq!(errno_in(&e.to_string()), Some(116));
    assert_eq!(
        Fault::of(&std::io::ErrorKind::TimedOut.into()),
        Fault::Unresponsive
    );
    assert_eq!(
        Fault::of(&std::io::Error::other("E9077: empty /x")),
        Fault::Other
    );
}

#[test]
fn a_folder_keeps_its_history_across_checks() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = Path::new("/test/history");
    let mut good = ok_mount("Library", p);
    good.checked_at = 100;
    good.last_ok = Some(100);
    good.since = 100;
    set_mounts(vec![good]);
    let g0 = generation();
    let mut bad = bad_mount("Library", p, Fault::Stale);
    bad.checked_at = 200;
    bad.since = 200;
    set_mounts(vec![bad.clone()]);
    let m = status_for(&p.join("A/A.mkv")).unwrap();
    assert_eq!(m.state, State::Unhealthy);
    assert_eq!((m.since, m.last_ok), (200, Some(100)));
    assert!(generation() > g0, "a state change is published");
    let g1 = generation();
    bad.checked_at = 230;
    bad.since = 230;
    set_mounts(vec![bad]);
    assert_eq!(
        status_for(p).unwrap().since,
        200,
        "still unhealthy since the first failure"
    );
    assert_eq!(generation(), g1, "an unchanged state is not re-sent");
    let mut back = ok_mount("Library", p);
    back.checked_at = 260;
    back.since = 260;
    back.last_ok = Some(260);
    set_mounts(vec![back]);
    let m = status_for(p).unwrap();
    assert_eq!((m.state, m.last_ok, m.since), (State::Ok, Some(260), 260));
    assert_eq!(
        m.last_error.as_deref(),
        Some("test"),
        "the last error is kept"
    );
    set_mounts(Vec::new());
}

#[test]
fn folders_are_checked_concurrently_in_order() {
    let folders: Vec<_> = (0..5)
        .map(|i| ("Output", PathBuf::from(format!("/dead/{i}")), false))
        .collect();
    let started = Instant::now();
    let out = check_each(folders, |role, path, _| {
        std::thread::sleep(Duration::from_millis(300));
        ok_mount(role, &path)
    });
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "5 x 300ms checks ran one after another: {:?}",
        started.elapsed()
    );
    let paths: Vec<_> = out.iter().map(|m| m.path.clone()).collect();
    assert_eq!(paths[0], Path::new("/dead/0"));
    assert_eq!(paths[4], Path::new("/dead/4"));
}

#[cfg(unix)]
#[test]
fn an_unlimited_block_count_saturates() {
    assert_eq!(scaled(u64::MAX, 4096), u64::MAX);
    assert_eq!(scaled(10, 4096), 40_960);
}

#[test]
fn folders_are_listed_once_each() {
    let c = Config {
        output_dir: "/o".into(),
        movie_dir: "m".into(),
        ..Config::default()
    };
    let f = folders(&c);
    assert!(
        f.iter()
            .any(|(r, p, _)| *r == "Movies" && p == Path::new("/o/m"))
    );
    // The library defaults to the movie folder: listed once, as Movies.
    assert_eq!(
        f.iter().filter(|(_, p, _)| p == Path::new("/o/m")).count(),
        1
    );
}
