use super::*;

// LAST is process-wide; tests that publish to it take this.
pub(crate) static LAST_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    // This test thread's "the server is stopping" (the process-wide flag is shared).
    pub(crate) static STOPPING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Sweeps of leftover write-test files held until the test lets them run, by folder.
pub(crate) static HELD_SWEEPS: Mutex<Vec<(PathBuf, std::sync::mpsc::Receiver<()>)>> =
    Mutex::new(Vec::new());

pub(crate) fn sweep_starting(dir: &Path) {
    let gate = {
        let mut held = HELD_SWEEPS.lock().unwrap_or_else(|e| e.into_inner());
        let at = held.iter().position(|(p, _)| p == dir);
        at.map(|i| held.remove(i).1)
    };
    if let Some(gate) = gate {
        let _ = gate.recv();
    }
}

// Folders some caller has waited on for an earlier probe.
static WAITED_ON: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

static WAITING_SIGNAL: Mutex<Option<(PathBuf, std::sync::mpsc::Sender<()>)>> = Mutex::new(None);

pub(crate) fn waiting(key: &Path) {
    let mut signal = WAITING_SIGNAL.lock().unwrap();
    if signal.as_ref().is_some_and(|(path, _)| path == key) {
        signal.take().unwrap().1.send(()).unwrap();
    }
    drop(signal);
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
    let generation = probe_generation();
    IN_FLIGHT.probes.lock().unwrap().push(Probe {
        generation,
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
    // The probe answers before it leaves the in-flight list.
    eventually("the probe has left", || !probing(t.path()));
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
fn a_preflight_run_out_after_waiting_on_another_reads_still_checking() {
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
    // Late in the wait: counted from its own start, its probe would run to 3.5 s.
    std::thread::sleep(hang * 3 / 4);
    drop(earlier);
    let p = caller.join().unwrap().unwrap_err();
    assert!(
        called.elapsed() < hang + hang * 3 / 8,
        "{:?}",
        called.elapsed()
    );
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

// The health list: one folder whose probe is still out (another caller's) does not hold up
// the round; it reads as not answered at once, and every other folder is checked.
#[test]
fn a_busy_folder_does_not_hold_up_the_rest_of_the_batch() {
    let busy = PathBuf::from("/test/batch-queue-busy");
    let free = PathBuf::from("/test/batch-queue-free");
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let running = begin(&busy, Duration::from_secs(30), move || {
        let _ = gate.recv();
    });
    let t = Instant::now();
    // Queued behind the busy folder, the round would wait out its whole 5 s.
    let out = check_folders(
        vec![("Movies", busy.clone(), false), ("TV", free.clone(), false)],
        Duration::from_secs(5),
        Duration::from_secs(3600),
    );
    let took = t.elapsed();
    release.send(()).unwrap();
    let _ = running.wait();
    assert!(out[0].is_none(), "the busy folder is not probed twice");
    assert!(out[1].is_some(), "the free folder is checked");
    assert!(took < Duration::from_millis(2500), "took {took:?}");
}

// Every other batch (Change match, the Library scan) waits within its limit for another
// caller's probe of the same folder, without holding up the rest of the batch.
#[test]
fn a_batch_waits_within_its_limit_for_a_probe_already_out() {
    let busy = PathBuf::from("/test/batch-wait-busy");
    let free = PathBuf::from("/test/batch-wait-free");
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let running = begin(&busy, Duration::from_secs(30), move || {
        let _ = gate.recv();
    });
    let (ran, free_ran) = std::sync::mpsc::channel::<()>();
    let ran = Mutex::new(ran);
    let batch = {
        let (busy, free) = (busy.clone(), free.clone());
        std::thread::spawn(move || {
            bounded_each(
                vec![(busy, 1), (free, 2)],
                Duration::from_secs(10),
                move |n| {
                    if n == 2 {
                        let _ = ran.lock().unwrap().send(());
                    }
                    n
                },
            )
        })
    };
    let started_free = free_ran.recv_timeout(Duration::from_secs(5));
    release.send(()).unwrap();
    let out = batch.join().unwrap();
    let _ = running.wait();
    assert!(
        started_free.is_ok(),
        "the free folder waited on the busy one"
    );
    assert_eq!(out, vec![Bounded::Done(1), Bounded::Done(2)]);
}

// A preflight waits at most one hang bound in all, from the call: waiting for another
// caller's probe and then for its own never adds up to two.
#[test]
fn a_preflight_waits_one_hang_bound_in_all() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_path_buf();
    let hang = Duration::from_secs(3);
    let (release_earlier, earlier_gate) = std::sync::mpsc::channel::<()>();
    let earlier = begin(&dir, Duration::from_secs(30), move || {
        let _ = earlier_gate.recv();
    });
    let (probe, release_own) = gated();
    let started = Instant::now();
    let caller = {
        let dir = dir.clone();
        std::thread::spawn(move || preflight_hanging_after("Output", &dir, hang, hang, probe))
    };
    // The earlier probe answers 2 s in; two bounds would then run to 5 s.
    std::thread::sleep(Duration::from_secs(2));
    release_earlier.send(()).unwrap();
    let result = caller.join().unwrap();
    let took = started.elapsed();
    let _ = release_own.send(());
    let _ = earlier.wait();
    assert!(
        took < Duration::from_millis(4200),
        "took {took:?}: {result:?}"
    );
    assert!(
        result.is_err_and(|p| p.detail.contains(STILL_CHECKING)),
        "its own probe is still out"
    );
}

// The probe sweep's remembered leftovers are process-wide and capped: the tests that fill
// them run one at a time, so one never drops another's.
static SWEEP_TEST_LOCK: Mutex<()> = Mutex::new(());

// A writability probe left behind (its removal failed) is ours to sweep: a later
// successful preflight removes our stale markers, and nothing else.
#[test]
fn leftover_probe_files_are_swept_by_a_later_preflight() {
    let _g = SWEEP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    eventually("the failed write test's sweep is done", || {
        !PROBE_SWEEP.lock().unwrap().running.contains(dir)
    });
    probe_write(dir).unwrap();
    eventually("a stale marker is swept", || !stale.exists());
    eventually("our own leftover is swept", || !ours[0].exists());
    assert!(
        fresh.exists(),
        "a recent marker may be another server's live probe"
    );
    assert!(foreign.exists() && folder.exists(), "only our marker files");
}

// A folder where our marker can never be removed (create allowed, delete denied) is still
// swept of stale markers, at most once an hour, and what we remember of ours stays bounded.
#[test]
fn markers_that_cannot_be_removed_are_still_swept_and_bounded() {
    let _g = SWEEP_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let dir = t.path();
    let aged = |name: &str| {
        let p = dir.join(name);
        std::fs::write(&p, b"").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(2 * 3600))
            .unwrap();
        p
    };
    let swept_at = || PROBE_SWEEP.lock().unwrap().last.get(dir).copied();
    let stale = aged(".freemkv-probe-123-456");
    let denied = |_: &Path| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    probe_write_with(dir, denied).unwrap();
    eventually("a stale marker is swept", || !stale.exists());
    eventually("the sweep ends", || {
        !PROBE_SWEEP.lock().unwrap().running.contains(dir)
    });
    let first = swept_at().expect("swept");
    let again = aged(".freemkv-probe-123-789");
    for _ in 0..MAX_LEFTOVERS + 10 {
        probe_write_with(dir, denied).unwrap();
    }
    let ours = PROBE_SWEEP
        .lock()
        .unwrap()
        .leftovers
        .iter()
        .filter(|p| p.parent() == Some(dir))
        .count();
    assert!(ours <= MAX_LEFTOVERS, "{ours} remembered");
    assert_eq!(swept_at(), Some(first), "swept again within the hour");
    assert!(again.exists(), "swept again within the hour");
    // An hour on, the next marker that cannot be removed sweeps the folder again.
    PROBE_SWEEP.lock().unwrap().last.remove(dir);
    probe_write_with(dir, denied).unwrap();
    eventually("swept again after the hour", || !again.exists());
    eventually("the last sweep ends", || {
        !PROBE_SWEEP.lock().unwrap().running.contains(dir)
    });
    PROBE_SWEEP
        .lock()
        .unwrap()
        .leftovers
        .retain(|p| p.parent() != Some(dir));
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

// A failed write test outlives verdicts that are not write tests (a slow round's "checking",
// a hang, a read error): only a write test that passes clears it.
#[test]
fn a_failed_write_test_outlives_other_verdicts_until_one_passes() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let t = tempfile::tempdir().unwrap();
    let path = t.path().to_path_buf();
    WATCHED.lock().unwrap().push(("Movies", path.clone()));
    fn quota(_: &Path) -> std::io::Result<()> {
        Err(std::io::Error::from_raw_os_error(libc::EDQUOT))
    }
    fn fine(_: &Path) -> std::io::Result<()> {
        Ok(())
    }
    let failed = preflight_with("Movies", &path, Duration::from_secs(5), quota);
    publish(vec![Mount::checking("Movies", &path)], true);
    let after_checking = check_with("Movies", &path, true, quota);
    publish(vec![after_checking.clone()], false);
    publish(vec![not_responding("Movies", &path, HANG_AFTER)], false);
    let after_hang = check_with("Movies", &path, true, quota);
    publish(vec![after_hang.clone()], false);
    let passed = check_with("Movies", &path, true, fine);
    publish(vec![passed.clone()], false);
    let cleared = check_with("Movies", &path, true, quota);
    WATCHED.lock().unwrap().retain(|(_, p)| *p != path);
    LAST.lock().unwrap().retain(|m| m.path != path);
    assert!(failed.is_err());
    assert!(
        !after_checking.ok,
        "a checking row cleared it: {after_checking:?}"
    );
    assert!(!after_hang.ok, "a hang row cleared it: {after_hang:?}");
    assert!(passed.ok, "{passed:?}");
    assert!(cleared.ok, "a passing write test clears it: {cleared:?}");
}

// The sweep lists the whole folder, which on a slow share takes time: the write test answers
// first, and the sweep runs after it, off the timed probe.
#[test]
fn a_write_test_answers_before_its_sweep() {
    let t = tempfile::tempdir().unwrap();
    let dir = t.path().to_path_buf();
    let stale = dir.join(".freemkv-probe-9-9");
    std::fs::write(&stale, b"").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200))
        .unwrap();
    let (open, gate) = std::sync::mpsc::channel();
    HELD_SWEEPS.lock().unwrap().push((dir.clone(), gate));
    let (tx, answered) = std::sync::mpsc::channel();
    let probed = dir.clone();
    std::thread::spawn(move || {
        let _ = tx.send(probe_write(&probed));
    });
    let answer = answered.recv_timeout(Duration::from_secs(10));
    let _ = open.send(());
    assert!(
        matches!(answer, Ok(Ok(()))),
        "the answer waited for the sweep"
    );
    eventually("swept after the answer", || !stale.exists());
}

// Bound on a test waiting for a production path to reach its gate.
const GATE_WAIT: Duration = Duration::from_secs(10);

// A detached preflight may answer after recovery; its completion rank cannot make its
// old-mount evidence current again.
#[test]
fn remount_rejects_detached_preflight_answers() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = Path::new("/test/recovered-share/movies");
    for late_failure in [true, false] {
        set_mounts(vec![ok_mount("Movies", path)]);
        let (entered, running) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let pending = preflight_begin(
            "output",
            path,
            // Queue behind the last round's probe, which leaves the list just after answering.
            Wait::Until(Instant::now() + Duration::from_secs(5)),
            HANG_AFTER,
            move |_| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                if late_failure {
                    Err(std::io::ErrorKind::StaleNetworkFileHandle.into())
                } else {
                    Ok(())
                }
            },
        );
        let mount = pending.as_ref().unwrap().mount;
        running
            .recv_timeout(GATE_WAIT)
            .expect("the preflight probe never started");
        assert!(preflight_finish("output", path, Duration::ZERO, HANG_AFTER, pending).is_err());
        STALE_REPORTS
            .lock()
            .unwrap()
            .renewed(Path::new("/test/recovered-share"));
        let fresh = if late_failure {
            ok_mount("Movies", path)
        } else {
            bad_mount("Movies", path, Fault::Denied)
        };
        let observed = fresh.observed;
        publish(vec![fresh], false);
        release.send(()).unwrap();
        assert_eq!(
            begin_keyed(
                path,
                mount,
                Wait::Until(Instant::now() + Duration::from_secs(5)),
                HANG_AFTER,
                || ()
            )
            .wait(),
            Bounded::Done(())
        );
        let current = status_for(path).unwrap();
        assert_eq!(
            current.observed, observed,
            "old preflight replaced recovered health"
        );
        assert!(!current.write_failed && !current.write_passed);
        assert_eq!(current.ok, late_failure);
        assert!(!STALE_REPORTS.lock().unwrap().paths.contains(path));
    }
    set_mounts(Vec::new());
}

// Pause one named publication at the commit boundary, with its production locks held.
struct HeldPublication {
    path: PathBuf,
    entered: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
static HELD_PUBLICATION: Mutex<Option<HeldPublication>> = Mutex::new(None);

pub(crate) fn publication_committing(rows: &[Mount]) {
    let gate = {
        let mut held = HELD_PUBLICATION.lock().unwrap();
        if held
            .as_ref()
            .is_some_and(|gate| rows.iter().any(|m| m.path == gate.path))
        {
            held.take()
        } else {
            None
        }
    };
    if let Some(gate) = gate {
        gate.entered.send(()).unwrap();
        gate.release.recv().unwrap();
    }
}

#[test]
fn remount_publication_and_renewal_have_one_commit_boundary() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = PathBuf::from("/test/atomic-renewal/movies");
    let old = ok_mount("Movies", &path).with_probe_generation(probe_generation());
    let (entered, inside) = std::sync::mpsc::channel();
    let (release, gate) = std::sync::mpsc::channel();
    *HELD_PUBLICATION.lock().unwrap() = Some(HeldPublication {
        path: path.clone(),
        entered,
        release: gate,
    });
    let publish = std::thread::spawn(move || publish(vec![old], false));
    inside
        .recv_timeout(GATE_WAIT)
        .expect("the publication never reached its commit");
    let reports_locked = matches!(
        STALE_REPORTS.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    );
    let last_locked = matches!(LAST.try_lock(), Err(std::sync::TryLockError::WouldBlock));
    release.send(()).unwrap();
    publish.join().unwrap();
    assert!(
        reports_locked && last_locked,
        "renewal must not pass an admitted publication before its LAST commit"
    );
    assert!(
        status_for(&path).unwrap().ok,
        "publication won the boundary"
    );
    STALE_REPORTS
        .lock()
        .unwrap()
        .renewed(Path::new("/test/atomic-renewal"));
    let current =
        bad_mount("Movies", &path, Fault::Denied).with_probe_generation(probe_generation());
    publish_slots(vec![Slot::New(Box::new(current))], false);
    let observed = status_for(&path).unwrap().observed;
    let obsolete = ok_mount("Movies", &path).with_probe_generation(probe_generation() - 1);
    publish_slots(vec![Slot::New(Box::new(obsolete))], false);
    assert_eq!(
        status_for(&path).unwrap().observed,
        observed,
        "renewal won the boundary"
    );
    set_mounts(Vec::new());
}

#[test]
fn remount_rejected_refresh_slots_keep_current_rows_and_other_roots() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mp = Path::new("/test/refreshed-share");
    let p = mp.join("movies");
    let absent = mp.join("absent");
    let other = Path::new("/test/unaffected-share/movies");
    let epoch = probe_generation();
    let obsolete = bad_mount("Movies", &p, Fault::Stale).with_probe_generation(epoch);
    let obsolete_absent = bad_mount("ISOs", &absent, Fault::Stale).with_probe_generation(epoch);
    STALE_REPORTS.lock().unwrap().renewed(mp);
    let current = ok_mount("Movies", &p).with_probe_generation(probe_generation());
    let observed = current.observed;
    set_mounts(vec![current, ok_mount("TV", other)]);
    let unaffected = bad_mount("TV", other, Fault::Denied).with_probe_generation(epoch);
    // Completion-time order would otherwise make these detached answers win.
    let obsolete = answered(obsolete);
    publish(vec![obsolete.clone()], false);
    let before = generation();
    publish_slots(
        vec![
            Slot::New(Box::new(obsolete)),
            Slot::New(Box::new(unaffected)),
            Slot::New(Box::new(obsolete_absent)),
        ],
        true,
    );
    assert_eq!(status_for(&p).unwrap().observed, observed);
    assert_eq!(status_for(other).unwrap().fault, Some(Fault::Denied));
    assert!(status_for(&absent).is_none());
    assert_eq!(
        mounts().len(),
        2,
        "rejected requested slots keep their place"
    );
    assert!(
        generation() > before,
        "unaffected old-origin answers are admitted"
    );
    let current = bad_mount("Movies", &p, Fault::Io).with_probe_generation(probe_generation());
    publish(vec![current], false);
    assert_eq!(status_for(&p).unwrap().fault, Some(Fault::Io));
    // No MountKey or kernel identity is required for these path-scoped decisions.
    assert_eq!(key_for(epoch, None).mount, None);
    set_mounts(Vec::new());
}

#[test]
fn remount_rejects_deferred_unmounted_preflight_refusals() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = Path::new("/test/deferred-refusal/movies");
    let problem = Problem::new("output", path, Fault::Unmounted, UNMOUNTED.into());
    let deferred: Started = Err(Box::new((problem.clone(), probe_generation())));
    STALE_REPORTS
        .lock()
        .unwrap()
        .renewed(Path::new("/test/deferred-refusal"));
    let current = ok_mount("Movies", path).with_probe_generation(probe_generation());
    let observed = current.observed;
    set_mounts(vec![current]);
    assert_eq!(
        preflight_finish("output", path, Duration::ZERO, HANG_AFTER, deferred),
        Err(problem)
    );
    assert_eq!(status_for(path).unwrap().observed, observed);
    set_mounts(Vec::new());
}

#[test]
fn remount_timeout_verdicts_keep_the_origin_and_rank_of_the_in_flight_probe() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mp = Path::new("/test/timeout-origin");
    let path = mp.join("movies");
    let key = key_for(0, Some(991));
    let old = stuck_probe(&path, key, Some(Instant::now()));
    STALE_REPORTS.lock().unwrap().renewed(mp);
    // No newer answer is needed for an obsolete timeout verdict to be refused.
    set_mounts(Vec::new());
    WATCHED.lock().unwrap().push(("Movies", path.clone()));
    let pending = Pending {
        rx: None,
        deadline: Instant::now(),
        hung: true,
        mount: key,
    };
    assert!(
        preflight_finish("Movies", &path, Duration::ZERO, Duration::ZERO, Ok(pending)).is_err()
    );
    assert!(status_for(&path).is_none());
    for hang in [HANG_AFTER, Duration::ZERO] {
        let row = pending_verdict("Movies", &path, key, Duration::ZERO, hang).unwrap();
        publish_slots(vec![Slot::New(Box::new(row))], true);
        assert!(status_for(&path).is_none());
    }
    drop(old);
    let current = stuck_probe(&path, key, Some(Instant::now()));
    let (_, seq, _) = in_flight_for(&path, key).unwrap();
    for (hang, state) in [
        (HANG_AFTER, State::Checking),
        (Duration::ZERO, State::Unresponsive),
    ] {
        let row = pending_verdict("Movies", &path, key, Duration::ZERO, hang).unwrap();
        publish_slots(vec![Slot::New(Box::new(row))], true);
        let shown = status_for(&path).unwrap();
        assert_eq!(shown.observed, seq);
        assert_eq!(shown.state, state);
    }
    WATCHED.lock().unwrap().retain(|(_, p)| *p != path);
    drop(current);
    set_mounts(Vec::new());
}

#[test]
fn remount_rejects_actual_check_answer_in_both_publication_routes() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = PathBuf::from("/test/check-origin/movies");
    let (entered, inside) = std::sync::mpsc::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let checking = {
        let path = path.clone();
        std::thread::spawn(move || {
            check_reporting_with("Movies", path, true, |role, path, _| {
                entered.send(()).unwrap();
                gate.recv().unwrap();
                bad_mount(role, path, Fault::Stale)
            })
        })
    };
    inside
        .recv_timeout(GATE_WAIT)
        .expect("the check never started");
    STALE_REPORTS
        .lock()
        .unwrap()
        .renewed(Path::new("/test/check-origin"));
    let current = ok_mount("Movies", &path).with_probe_generation(probe_generation());
    let observed = current.observed;
    set_mounts(vec![current]);
    let generation = generation();
    release.send(()).unwrap();
    let detached = checking.join().unwrap();
    assert!(
        detached.observed > observed,
        "completion rank is intentionally retained"
    );
    publish_answer(detached.clone());
    publish_slots(vec![Slot::New(Box::new(detached))], true);
    assert_eq!(status_for(&path).unwrap().observed, observed);
    assert_eq!(
        super::generation(),
        generation,
        "obsolete answers change no UI generation"
    );
    assert!(!STALE_REPORTS.lock().unwrap().paths.contains(&path));
    assert!(!wants_remount(
        &mounts(),
        Path::new("/test/check-origin"),
        true,
        None,
        Instant::now()
    ));
    set_mounts(Vec::new());
}

#[test]
fn remount_queued_probe_captures_its_origin_after_the_queue() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = Path::new("/test/queued-origin/movies");
    let key = key_for(0, Some(993));
    let stuck = stuck_probe(path, key, Some(Instant::now()));
    let (entered, queued) = std::sync::mpsc::channel();
    *WAITING_SIGNAL.lock().unwrap() = Some((path.to_path_buf(), entered));
    let pending = std::thread::spawn(move || {
        begin_keyed_origin(
            path,
            key,
            Wait::Until(Instant::now() + Duration::from_secs(5)),
            HANG_AFTER,
            |generation| generation,
        )
    });
    queued
        .recv_timeout(GATE_WAIT)
        .expect("the probe never queued behind the stuck one");
    STALE_REPORTS
        .lock()
        .unwrap()
        .renewed(Path::new("/test/queued-origin"));
    let expected = probe_generation();
    drop(stuck);
    assert_eq!(pending.join().unwrap().wait(), Bounded::Done(expected));
}

#[test]
fn remount_completed_preflight_results_are_returned_without_republication() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = Path::new("/test/completed-preflight/movies");
    set_mounts(vec![ok_mount("Movies", path)]);
    let observed = status_for(path).unwrap().observed;
    for error in [false, true] {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(if error {
            Err(std::io::ErrorKind::PermissionDenied.into())
        } else {
            Ok(())
        })
        .unwrap();
        let pending = Pending {
            rx: Some(rx),
            deadline: Instant::now() + Duration::from_secs(5),
            hung: false,
            mount: key_for(0, None),
        };
        let result = preflight_finish("output", path, Duration::ZERO, HANG_AFTER, Ok(pending));
        assert_eq!(result.is_err(), error);
        assert_eq!(status_for(path).unwrap().observed, observed);
    }
    set_mounts(Vec::new());
}
