use super::*;

// LAST is process-wide; tests that publish to it take this.
pub(crate) static LAST_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    // This test thread's "the server is stopping" (the process-wide flag is shared).
    pub(crate) static STOPPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Folders some caller has waited on for an earlier probe.
static WAITED_ON: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

pub(crate) fn waiting(key: &Path) {
    let mut w = WAITED_ON.lock().unwrap_or_else(|e| e.into_inner());
    if !w.iter().any(|p| p == key) {
        w.push(key.to_path_buf());
    }
}

fn someone_waited_on(key: &Path) -> bool {
    WAITED_ON.lock().unwrap().iter().any(|p| p == key)
}

// Whether a probe of `path` is in flight on its current mount.
fn probing(path: &Path) -> bool {
    in_flight_for(path, mount_key(path)).is_some()
}

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

/// A probe of `path` out for longer than the hang bound, as on a mount the kernel never
/// returns from; dropping it is the kernel letting go.
pub(crate) struct HungProbe(u64);

pub(crate) fn hung_probe(path: &Path) -> HungProbe {
    stuck_probe(path, mount_key(path), None)
}

// A probe of `path` on `mount` out since `started` (`None`: longer than any bound).
fn stuck_probe(path: &Path, mount: MountKey, started: Option<Instant>) -> HungProbe {
    let id = NEXT_PROBE.fetch_add(1, Ordering::Relaxed);
    IN_FLIGHT.probes.lock().unwrap().push(Probe {
        id,
        path: path.to_path_buf(),
        mount,
        started,
        seq: observed_now(),
    });
    HungProbe(id)
}

impl Drop for HungProbe {
    fn drop(&mut self) {
        release(self.0);
    }
}

/// Put `mounts` in place as if a check had just published them.
pub(crate) fn set_mounts(mounts: Vec<Mount>) {
    publish(mounts, true);
}

pub(crate) fn ok_mount(role: &'static str, path: &Path) -> Mount {
    Mount::blank(role, path).passed()
}

pub(crate) fn checking_mount(role: &'static str, path: &Path) -> Mount {
    Mount::checking(role, path)
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
    assert_eq!(m.fault, Some(Fault::Denied));
    assert!(
        m.problem
            .as_deref()
            .unwrap()
            .contains("write access check failed")
    );
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
    let hung = hung_probe(&p);
    let started = Instant::now();
    let m = check_folders(
        vec![("Library", p.clone(), false)],
        CHECK_TIMEOUT,
        HANG_AFTER,
    )
    .remove(0)
    .expect("a hung folder has a verdict");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a hung folder answers at once"
    );
    assert!(!m.ok);
    assert_eq!(m.state, State::Unresponsive);
    assert!(m.problem.unwrap().contains("not responding"));
    let n = IN_FLIGHT
        .probes
        .lock()
        .unwrap()
        .iter()
        .filter(|x| x.path == p)
        .count();
    assert_eq!(n, 1);
    drop(hung);
    let fine = check_folders(
        vec![("Library", t.path().to_path_buf(), false)],
        CHECK_TIMEOUT,
        HANG_AFTER,
    )
    .remove(0)
    .unwrap();
    assert!(fine.ok);
    assert!(!probing(t.path()));
}

#[test]
fn a_hung_probe_returns_within_its_limit_and_is_never_started_twice() {
    let key = PathBuf::from("/test/hung-probe");
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let runs = Arc::new(AtomicU64::new(0));
    let r = runs.clone();
    let hang = Duration::from_millis(200);
    let started = Instant::now();
    let out = begin_with(&key, Wait::Until(Instant::now() + hang), hang, move || {
        r.fetch_add(1, Ordering::SeqCst);
        let _ = rx.recv(); // a kernel call that never returns
    })
    .wait();
    let took = started.elapsed();
    assert_eq!(out, Bounded::TimedOut);
    assert!(took < Duration::from_millis(1500), "{took:?}");
    // Out for the hang bound: the next probe of the same folder neither waits nor spawns.
    let r = runs.clone();
    let started = Instant::now();
    let again = begin_with(&key, Wait::Until(Instant::now() + hang), hang, move || {
        r.fetch_add(1, Ordering::SeqCst);
    })
    .wait();
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
    while probing(&key) {
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
    let hang = Duration::from_millis(300);
    let started = Instant::now();
    let p = preflight_hanging_after("Output", &hung, hang, hang, |_| {
        std::thread::sleep(Duration::from_secs(3));
        Ok(())
    })
    .unwrap_err();
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert_eq!(p.fault, Fault::Unresponsive);
    let again = Instant::now();
    let p = preflight_hanging_after("Output", &hung, hang, hang, |_| Ok(())).unwrap_err();
    assert!(
        again.elapsed() < Duration::from_millis(100),
        "no second probe of a hung folder"
    );
    assert!(p.detail.contains("no answer for"), "{}", p.detail);
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
    let items: Vec<_> = (0..5)
        .map(|i| (PathBuf::from(format!("/dead/{i}")), i))
        .collect();
    let started = Instant::now();
    let out = bounded_each(items, Duration::from_secs(2), |i| {
        std::thread::sleep(Duration::from_millis(300));
        i
    });
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "5 x 300ms checks ran one after another: {:?}",
        started.elapsed()
    );
    assert_eq!(out, (0..5).map(Bounded::Done).collect::<Vec<_>>());
}

#[test]
fn hung_folders_in_one_batch_cost_one_timeout() {
    let items: Vec<_> = (0..3)
        .map(|i| (PathBuf::from(format!("/hung-batch/{i}")), i))
        .collect();
    let started = Instant::now();
    let out = bounded_each(items, Duration::from_millis(300), |i| {
        std::thread::sleep(Duration::from_secs(2));
        i
    });
    assert!(started.elapsed() < Duration::from_millis(900));
    assert!(out.iter().all(|r| *r == Bounded::TimedOut), "{out:?}");
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

#[test]
fn a_panicking_probe_does_not_permanently_block_the_folder() {
    let key = Path::new("/test/panicking-probe");
    assert_eq!(
        bounded(key, Duration::from_secs(1), || -> () {
            panic!("probe failed")
        }),
        Bounded::TimedOut
    );
    assert_eq!(
        bounded(key, Duration::from_secs(1), || 42),
        Bounded::Done(42)
    );
}

#[cfg(unix)]
#[test]
fn capacity_errors_keep_their_os_error() {
    let dir = tempfile::tempdir().unwrap();
    let error = statvfs(&dir.path().join("missing")).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
    assert_eq!(Fault::of(&error), Fault::Missing);
    assert!(statvfs(dir.path()).unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn stale_preflight_result_survives_its_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let (release, wait) = std::sync::mpsc::channel();
    let result = preflight_with("Library", &path, Duration::from_millis(20), move |_| {
        wait.recv().unwrap();
        Err(std::io::Error::from_raw_os_error(libc::ESTALE))
    });
    assert_eq!(result.unwrap_err().fault, Fault::Unresponsive);
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut reports = STALE_REPORTS.lock().unwrap();
        if reports.paths.remove(&path) {
            break;
        }
        drop(reports);
        assert!(Instant::now() < deadline, "late ESTALE was discarded");
        std::thread::yield_now();
    }
}

#[test]
fn remount_discards_old_generation_reports_only() {
    let mut reports = StaleReports::default();
    let path = Path::new("/mnt/nfs/movies");
    reports.record(0, path, Some(Fault::Stale));
    reports.record(0, Path::new("/other"), Some(Fault::Denied));
    assert_eq!(reports.paths.len(), 1);
    reports.renewed(Path::new("/mnt/nfs"));
    assert!(reports.paths.is_empty());
    reports.record(0, path, Some(Fault::Stale));
    assert!(
        reports.paths.is_empty(),
        "detached mount must not cause another remount"
    );
    reports.record(1, path, Some(Fault::Stale));
    assert!(reports.paths.contains(path));
}

#[test]
fn a_writable_folder_going_bad_gets_a_syslog_line() {
    let mut m = ok_mount("Movies", Path::new("/mnt/movies"));
    m = m.failed(Fault::Missing, "missing".into());
    let line = unusable_line("Movies", &m);
    assert!(line.contains("Movies folder '/mnt/movies' is not usable: missing"));
    assert!(line.contains("PRESERVED in staging"));
    let line = unusable_line("Library", &m);
    assert!(!line.contains("PRESERVED"), "{line}");
}

#[test]
fn a_batch_over_busy_folders_costs_one_limit_not_one_per_folder() {
    let keys: Vec<PathBuf> = (0..3)
        .map(|i| PathBuf::from(format!("/test/batch-busy-{i}")))
        .collect();
    let running: Vec<_> = keys
        .iter()
        .map(|k| {
            begin(k, Duration::from_secs(5), || {
                std::thread::sleep(Duration::from_millis(1500));
            })
        })
        .collect();
    let items: Vec<_> = keys.iter().map(|k| (k.clone(), ())).collect();
    let t = Instant::now();
    let out = bounded_each(items, Duration::from_millis(400), |_| ());
    let took = t.elapsed();
    assert!(took < Duration::from_millis(800), "took {took:?}");
    assert!(out.iter().all(|o| *o != Bounded::Done(())));
    for p in running {
        let _ = p.wait();
    }
}

#[test]
fn empty_or_relative_roots_are_warned_about() {
    let cfg = Config {
        output_dir: "/data/out".into(),
        staging_dir: "/data/stage".into(),
        ..Config::default()
    };
    assert!(root_warnings(&cfg).is_empty());
    let cfg = Config {
        output_dir: String::new(),
        staging_dir: "stage".into(),
        ..Config::default()
    };
    let w = root_warnings(&cfg);
    assert_eq!(w.len(), 2);
    assert!(w[0].contains("Output") && w[0].contains("not set"));
    assert!(w[1].contains("Staging") && w[1].contains("absolute"));
}

#[test]
fn an_unusable_folder_counts_as_already_reported() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let down = Path::new("/test/reported-down");
    let up = Path::new("/test/reported-up");
    publish(
        vec![
            Mount::blank("Output", down).failed(Fault::Other, "gone".into()),
            Mount::blank("Movies", up).passed(),
        ],
        true,
    );
    assert!(already_reported(&down.join("Movies")));
    assert!(!already_reported(up));
    assert!(!already_reported(Path::new("/test/unknown")));
}

#[test]
fn a_probe_file_that_cannot_be_removed_keeps_the_folder_usable() {
    let t = tempfile::tempdir().unwrap();
    let result = probe_write_with(t.path(), |_| {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    });
    assert!(result.is_ok(), "{result:?}");
}

// A probe held until the test lets it answer: (the probe, the gate that releases it).
fn gated() -> (
    impl FnOnce(&Path) -> std::io::Result<()> + Send + 'static,
    std::sync::mpsc::Sender<()>,
) {
    let (open, gate) = std::sync::mpsc::channel::<()>();
    (
        move |_: &Path| {
            let _ = gate.recv();
            Ok(())
        },
        open,
    )
}

// Wait (bounded) for `cond`, which a probe thread makes true.
fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// Live 2026-10-11: a folder whose probe answers after one caller's short wait is slow, not
// hung: the next caller must not be refused with "never came back".
#[test]
fn a_slow_answer_does_not_latch_the_folder_busy() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_path_buf();
    let (probe, open) = gated();
    let first = preflight_with("Output", &dir, Duration::from_millis(20), probe).unwrap_err();
    assert!(first.detail.contains(STILL_CHECKING), "{}", first.detail);
    let second = {
        let dir = dir.clone();
        std::thread::spawn(move || {
            preflight_with("Output", &dir, Duration::from_secs(30), |_| Ok(()))
        })
    };
    open.send(()).unwrap();
    assert_eq!(second.join().unwrap(), Ok(()));
}

// A caller that stops waiting does not decide the folder's verdict; the late answer does.
#[test]
fn a_late_answer_is_the_verdict() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_path_buf();
    publish(vec![ok_mount("Movies", &dir)], false);
    let (probe, open) = gated();
    assert!(preflight_with("output", &dir, Duration::from_millis(20), probe).is_err());
    assert_eq!(
        status_for(&dir).unwrap().state,
        State::Ok,
        "a short wait flipped the verdict"
    );
    publish(vec![bad_mount("Movies", &dir, Fault::Unresponsive)], false);
    open.send(()).unwrap();
    eventually("the late answer was discarded", || {
        status_for(&dir).unwrap().ok
    });
    set_mounts(Vec::new());
}

// A preflight that waits to the hang bound counts it from the call, waiting for an earlier
// probe included: it never waits two bounds. Its own probe, out less than the bound when the
// wait runs out, is "still checking", not hung; its answer, when it comes, is the verdict.
#[test]
fn a_preflight_waiting_to_the_hang_bound_publishes_the_hang() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_path_buf();
    publish(vec![ok_mount("Movies", &dir)], false);
    let hang = Duration::from_secs(2);
    // Someone else's probe answers part-way through the wait: our own probe starts late.
    let earlier = stuck_probe(&dir, mount_key(&dir), Some(Instant::now()));
    let (probe, open) = gated();
    let called = Instant::now();
    let caller = {
        let dir = dir.clone();
        std::thread::spawn(move || preflight_hanging_after("output", &dir, hang, hang, probe))
    };
    eventually("the caller waits on the earlier probe", || {
        someone_waited_on(&dir)
    });
    std::thread::sleep(hang / 4);
    drop(earlier);
    let p = caller.join().unwrap().unwrap_err();
    assert!(called.elapsed() < hang + hang / 4, "{:?}", called.elapsed());
    assert_eq!(p.fault, Fault::Unresponsive);
    assert!(p.detail.contains(STILL_CHECKING), "{}", p.detail);
    open.send(()).unwrap();
    eventually("the answer is the verdict", || status_for(&dir).unwrap().ok);
    set_mounts(Vec::new());
}

// A check out longer than the health thread's wait reads "checking" (not usable, not failed);
// one out for the hang bound "not responding", at once.
#[test]
fn a_slow_check_reads_checking_until_it_hangs() {
    let p = PathBuf::from("/test/slow-not-hung");
    let row = vec![("Movies", p.clone(), false)];
    // Out longer than the health thread's wait, far from the hang bound.
    let young = stuck_probe(
        &p,
        mount_key(&p),
        Some(Instant::now() - Duration::from_millis(50)),
    );
    let m = check_folders(
        row.clone(),
        Duration::from_millis(30),
        Duration::from_secs(3600),
    )
    .remove(0)
    .expect("a verdict");
    assert_eq!(m.state, State::Checking);
    assert!(!m.ok && !m.state.failed());
    assert_eq!(m.latency_ms, None, "not known yet");
    assert!(m.message.unwrap().contains("still checking"));
    drop(young);
    let _old = stuck_probe(&p, mount_key(&p), None);
    let t = Instant::now();
    let m = check_folders(row, Duration::from_secs(5), Duration::from_secs(3600))
        .remove(0)
        .expect("hung");
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert_eq!(m.state, State::Unresponsive);
    assert!(m.problem.unwrap().contains("no answer for"));
}

// Live race: the hang verdict is reached at 60.0 s and the answer lands at 60.2 s; whichever
// is published last, the newer evidence stays.
#[test]
fn older_evidence_never_replaces_a_newer_row() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = Path::new("/test/race");
    let hung = not_responding("Movies", p, HANG_AFTER);
    let answer = answered(ok_mount("Movies", p));
    publish(vec![answer.clone()], false);
    publish(vec![hung.clone()], false);
    assert!(status_for(p).unwrap().ok, "a late single publish");
    publish_slots(vec![Slot::New(Box::new(hung))], true);
    assert!(status_for(p).unwrap().ok, "a late full publish");
    let checking = Mount::checking("Movies", p);
    publish(vec![checking], false);
    assert_eq!(status_for(p).unwrap().state, State::Checking, "newer wins");
    set_mounts(Vec::new());
}

// A probe stuck on a mount that has been replaced does not hold the new mount's probes.
#[test]
fn a_probe_stuck_on_a_replaced_mount_does_not_hold_the_new_one() {
    let p = Path::new("/test/remounted");
    let old = MountKey {
        generation: 0,
        mount: Some(1),
    };
    let new = MountKey {
        generation: 0,
        mount: Some(2),
    };
    let _stuck = stuck_probe(p, old, None);
    let soon = || Wait::Until(Instant::now() + Duration::from_secs(5));
    assert_eq!(
        begin_keyed(p, old, soon(), HANG_AFTER, || 1).wait(),
        Bounded::Busy
    );
    assert_eq!(
        begin_keyed(p, new, soon(), HANG_AFTER, || 2).wait(),
        Bounded::Done(2)
    );
    // Without a kernel mount id, health's own remount generation tells the mounts apart.
    let q = Path::new("/test/remounted-no-id");
    let _stuck = stuck_probe(q, key_for(0, None), None);
    assert_eq!(
        begin_keyed(q, key_for(1, None), soon(), HANG_AFTER, || 3).wait(),
        Bounded::Done(3),
        "health's own remount counts there"
    );
}

// Health remounting its own share must not re-key folders on other mounts: a probe hung on
// an unrelated share stays the only one for its folder.
#[test]
fn a_remount_of_one_share_does_not_double_probes_on_another() {
    let p = Path::new("/test/other-share");
    let _stuck = stuck_probe(p, key_for(0, Some(7)), None);
    let after_remount = key_for(1, Some(7));
    assert_eq!(after_remount, key_for(0, Some(7)));
    let soon = Wait::Until(Instant::now() + Duration::from_secs(5));
    assert_eq!(
        begin_keyed(p, after_remount, soon, HANG_AFTER, || 1).wait(),
        Bounded::Busy,
        "a second thread stuck on the same hung share"
    );
}

// A verdict drawn from a probe being out ranks below that probe's own answer, even when the
// verdict is built (and published) after the answer landed.
#[test]
fn a_verdict_about_a_probe_never_outranks_its_answer() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = PathBuf::from("/test/answer-vs-verdict");
    let probe = stuck_probe(&p, mount_key(&p), Some(Instant::now()));
    // The probe answers (its answer is seen after it started)...
    publish(vec![answered(ok_mount("Movies", &p))], false);
    // ...and a round that found it still out publishes its verdict afterwards.
    let row = vec![("Movies", p.clone(), false)];
    let hung = check_folders(row.clone(), Duration::ZERO, Duration::ZERO).remove(0);
    let hung = hung.expect("hung");
    assert_eq!(hung.state, State::Unresponsive);
    publish(vec![hung], false);
    assert!(
        status_for(&p).unwrap().ok,
        "the hang verdict overwrote the answer"
    );
    let checking = check_folders(row, Duration::ZERO, Duration::from_secs(3600)).remove(0);
    let checking = checking.expect("checking");
    assert_eq!(checking.state, State::Checking);
    publish(vec![checking], false);
    assert!(
        status_for(&p).unwrap().ok,
        "the checking verdict overwrote the answer"
    );
    drop(probe);
    set_mounts(Vec::new());
}

// Every path resolves to the mount holding it, from the mount table alone.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn a_path_resolves_to_its_mount() {
    let t = tempfile::tempdir().unwrap();
    assert!(mount_id(Path::new("/")).is_some());
    assert!(mount_id(&t.path().join("not/there")).is_some());
}

// A stop ends every wait at once: for an earlier probe, and for this caller's own.
#[test]
fn a_stop_ends_a_wait_at_once() {
    let p = Path::new("/test/stop-wait");
    let _young = stuck_probe(p, mount_key(p), Some(Instant::now()));
    let (probe, open) = gated();
    let q = PathBuf::from("/test/stop-own");
    let own = begin_with(&q, Wait::UntilHung, HANG_AFTER, move || {
        probe(Path::new("/"))
    });
    STOPPING.with(|s| s.set(true));
    let t = Instant::now();
    let waited = begin_with(p, Wait::UntilHung, HANG_AFTER, || ()).wait();
    let mine = own.wait();
    STOPPING.with(|s| s.set(false));
    assert_eq!(waited, Bounded::TimedOut);
    assert!(matches!(mine, Bounded::TimedOut));
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    drop(open);
}

// A preflight row (no capacity figures) keeps the last ones the health check read.
#[test]
fn a_preflight_row_keeps_the_last_capacity() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let p = Path::new("/test/capacity");
    let mut full = ok_mount("Movies", p);
    full.free_bytes = Some(5);
    full.total_bytes = Some(10);
    set_mounts(vec![full]);
    publish(vec![ok_mount("output", p)], false);
    let m = status_for(p).unwrap();
    assert_eq!((m.free_bytes, m.total_bytes), (Some(5), Some(10)));
    set_mounts(Vec::new());
}

// A full publish keeps the last verdict, in its place, for a folder still being checked.
#[test]
fn a_folder_still_being_checked_keeps_its_last_verdict() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (a, b) = (Path::new("/test/keep-a"), Path::new("/test/keep-b"));
    set_mounts(vec![
        ok_mount("Movies", a),
        bad_mount("TV", b, Fault::Stale),
    ]);
    publish_slots(
        vec![
            Slot::Keep(a.to_path_buf()),
            Slot::New(Box::new(ok_mount("TV", b))),
        ],
        true,
    );
    let rows = mounts();
    assert_eq!(
        rows.iter()
            .map(|m| (m.path.as_path(), m.ok))
            .collect::<Vec<_>>(),
        vec![(a, true), (b, true)]
    );
    set_mounts(Vec::new());
}

// A watched folder's late answer lands even before its first row exists.
#[test]
fn a_late_first_answer_for_a_watched_folder_is_published() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_path_buf();
    WATCHED.lock().unwrap().push(("Movies", dir.clone()));
    let (probe, open) = gated();
    let r = preflight_with("output", &dir, Duration::from_millis(20), probe);
    assert!(r.unwrap_err().detail.contains(STILL_CHECKING));
    open.send(()).unwrap();
    eventually("late answer not published", || status_for(&dir).is_some());
    let m = status_for(&dir).unwrap();
    assert!(m.ok);
    assert_eq!(m.role, "Movies");
    WATCHED.lock().unwrap().retain(|(_, p)| *p != dir);
    set_mounts(Vec::new());
}

// Linux: the mount holding a path is the longest mount point over it, escapes decoded; a
// remount shows up as a new id.
#[test]
fn mountinfo_names_the_mount_holding_a_path() {
    let table = "22 1 0:21 / / rw - ext4 /dev/sda1 rw\n\
                 98 22 0:50 / /mnt/my\\040share rw - nfs4 nas:/m rw\n\
                 99 98 0:51 / /mnt/my\\040share/tv rw - nfs4 nas:/t rw\n";
    let at = |p: &str| mount_id_in(table, Path::new(p));
    assert_eq!(at("/mnt/my share/movies/A.mkv"), Some(98));
    assert_eq!(at("/mnt/my share/tv/S01"), Some(99));
    assert_eq!(at("/mnt/my sharex"), Some(22));
    assert_eq!(at("/staging"), Some(22));
    let remounted = table.replace("98 22", "140 22").replace("99 98", "141 140");
    assert_eq!(
        mount_id_in(&remounted, Path::new("/mnt/my share/movies")),
        Some(140)
    );
}

// A stop requested during a round's checks skips its remount: umount plus mount.nfs4 can
// take 45 s, which the stop's drain budget does not have.
#[test]
fn a_round_that_sees_a_stop_does_not_heal() {
    let healed = std::cell::Cell::new(false);
    round(|| STOPPING.with(|s| s.set(true)), || healed.set(true));
    STOPPING.with(|s| s.set(false));
    assert!(!healed.get());
    round(|| {}, || healed.set(true));
    assert!(healed.get(), "a normal round heals");
}

// One folder whose probe is still out (another caller's) does not hold up starting the
// others: it reads as not answered, and every other folder is checked within the limit.
#[test]
fn a_busy_folder_does_not_hold_up_the_rest_of_the_batch() {
    let busy = PathBuf::from("/test/batch-queue-busy");
    let free = PathBuf::from("/test/batch-queue-free");
    let running = begin(&busy, Duration::from_secs(5), || {
        std::thread::sleep(Duration::from_millis(1500));
    });
    let t = Instant::now();
    let out = bounded_each(
        vec![(busy.clone(), 1), (free.clone(), 2)],
        Duration::from_millis(600),
        |n| n,
    );
    let took = t.elapsed();
    let _ = running.wait();
    assert_ne!(
        out[0],
        Bounded::Done(1),
        "the busy folder is not probed twice"
    );
    assert_eq!(out[1], Bounded::Done(2), "the free folder is checked");
    assert!(took < Duration::from_millis(500), "took {took:?}");
}

// A preflight waits at most one hang bound in all, from the call: waiting for another
// caller's probe and then for its own never adds up to two.
#[test]
fn a_preflight_waits_one_hang_bound_in_all() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_path_buf();
    let hang = Duration::from_millis(1000);
    let earlier = begin(&dir, Duration::from_secs(5), || {
        std::thread::sleep(Duration::from_millis(800));
    });
    let started = Instant::now();
    let result = preflight_hanging_after("Output", &dir, hang, hang, |_| {
        std::thread::sleep(Duration::from_millis(900));
        Ok(())
    });
    let took = started.elapsed();
    let _ = earlier.wait();
    assert!(
        took < Duration::from_millis(1400),
        "took {took:?}: {result:?}"
    );
}

// A writability probe left behind (its removal failed) is ours to sweep: a later
// successful preflight removes our stale markers, and nothing else.
#[test]
fn leftover_probe_files_are_swept_by_a_later_preflight() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let old = std::time::SystemTime::now() - Duration::from_secs(2 * 3600);
    let stale = dir.join(".freemkv-probe-123-456");
    let fresh = dir.join(".freemkv-probe-124-457");
    let foreign = dir.join(".freemkv-probe-notes");
    let folder = dir.join(".freemkv-probe-125-458");
    for f in [&stale, &fresh, &foreign] {
        std::fs::write(f, b"").unwrap();
    }
    std::fs::create_dir(&folder).unwrap();
    for f in [&stale, &foreign] {
        std::fs::File::options()
            .write(true)
            .open(f)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }
    // Our own marker whose removal failed just now.
    probe_write_with(dir, |_| {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    })
    .unwrap();
    let ours: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!(".freemkv-probe-{}-", std::process::id()))
        })
        .collect();
    assert_eq!(ours.len(), 1, "fixture: a marker left behind");
    probe_write(dir).unwrap();
    assert!(!stale.exists(), "a stale marker is swept");
    assert!(!ours[0].exists(), "our own leftover is swept");
    assert!(
        fresh.exists(),
        "a recent marker may be another server's live probe"
    );
    assert!(foreign.exists() && folder.exists(), "only our marker files");
}

// The mover's preflight of a watched root is published under that folder's own name, not
// as "the output folder".
#[test]
fn a_preflight_answer_is_worded_for_the_folder_it_is_about() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = PathBuf::from("/test/role-worded/Movies");
    WATCHED.lock().unwrap().push(("Movies", path.clone()));
    publish_answer(Problem::new("output", &path, Fault::Missing, "missing".into()).mount());
    let shown = status_for(&path);
    WATCHED.lock().unwrap().retain(|(_, p)| *p != path);
    LAST.lock().unwrap().retain(|m| m.path != path);
    let shown = shown.expect("published");
    let message = shown.message.unwrap_or_default();
    assert!(message.contains("movies folder"), "{message}");
}

// One rule for writability: once a real write test (a preflight) has failed for a folder,
// the health check's access() verdict cannot clear it; the health check runs the same
// write test for that folder until it passes. Otherwise it writes nothing.
#[test]
fn a_failed_write_test_is_not_overridden_by_access() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let path = t.path().to_path_buf();
    WATCHED.lock().unwrap().push(("Movies", path.clone()));
    static CALLS: AtomicU64 = AtomicU64::new(0);
    fn quota(_: &Path) -> std::io::Result<()> {
        CALLS.fetch_add(1, Ordering::SeqCst);
        Err(std::io::Error::from_raw_os_error(libc::EDQUOT))
    }
    let healthy = check_with("Movies", &path, true, quota);
    let calls_while_healthy = CALLS.load(Ordering::SeqCst);
    let failed = preflight_with("Movies", &path, Duration::from_secs(5), quota);
    let checked = check_with("Movies", &path, true, quota);
    WATCHED.lock().unwrap().retain(|(_, p)| *p != path);
    LAST.lock().unwrap().retain(|m| m.path != path);
    assert!(healthy.ok, "{healthy:?}");
    assert_eq!(
        calls_while_healthy, 0,
        "no write test while access() agrees"
    );
    assert!(failed.is_err());
    assert!(
        !checked.ok,
        "access() overrode the failed write test: {checked:?}"
    );
}
