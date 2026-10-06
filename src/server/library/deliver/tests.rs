use super::*;
use crate::server::library::probe::testmkv::mkv;
use std::sync::atomic::AtomicBool;

/// A kept pair as [`super::keep`] writes it, in `stage`: `<name>.staged.mkv` holding
/// `mkv_bytes` and its sidecar, first kept at `created_at` after a failed copy.
pub(crate) fn write_kept(
    stage: &Path,
    name: &str,
    target: &Path,
    mkv_bytes: &[u8],
    created_at: u64,
) -> PathBuf {
    std::fs::create_dir_all(stage).unwrap();
    let staged = stage.join(format!("{name}{KEPT_MKV}"));
    std::fs::write(&staged, mkv_bytes).unwrap();
    let probe = libfreemkv::probe_mkv_with_cues(io::Cursor::new(mkv_bytes)).ok();
    let record = Sidecar {
        format: SIDECAR_FORMAT,
        target: target.to_path_buf(),
        replace: false,
        target_before: TargetStamp::Absent,
        iso: PathBuf::from("/i/A.iso"),
        iso_len: None,
        iso_mtime_secs: None,
        title: Some(0),
        size: mkv_bytes.len() as u64,
        runtime_secs: probe.as_ref().and_then(muxed_runtime),
        expected_secs: None,
        writing_app: probe.and_then(|p| p.writing_app),
        engine_version: "test".into(),
        created_at,
        attempts: 1,
        last_attempt_at: created_at,
        failed_phase: "copy".into(),
        error: "Stale file handle (os error 70)".into(),
        error_code: None,
    };
    write_sidecar(&stage.join(format!("{name}{KEPT_JSON}")), &record).unwrap();
    staged
}

fn movie() -> Vec<u8> {
    mkv("freemkv 1.0.0 (gtest)", Some(60.0), Some(58), true)
}

#[derive(Default)]
struct Events {
    seen: Mutex<Vec<String>>,
    cancel: AtomicBool,
}

impl Events {
    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn phases(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter_map(|s| s.strip_prefix("phase:").map(str::to_string))
            .collect()
    }
}

impl Sink for Events {
    fn log(&self, _level: Level, msg: &str) {
        self.seen.lock().unwrap().push(format!("log:{msg}"));
    }

    fn event(&self, e: &Event<'_>) {
        let s = match e {
            Event::Phase { name } => format!("phase:{name}"),
            Event::Verify { ok, .. } => format!("verified:{ok}"),
            Event::Replaced { .. } => "replaced".into(),
            _ => return,
        };
        self.seen.lock().unwrap().push(s);
    }

    fn should_cancel(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

// The injected faults: each makes one step of the delivery stall or fail.
#[derive(Default)]
struct Faulty {
    stall_copy: bool,
    short_copy: bool,
    sync_fails: bool,
    read_fails: bool,
    read_instead: Option<Vec<u8>>,
    land_fails: bool,
}

struct Stuck;

impl Write for Stuck {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        std::thread::sleep(Duration::from_secs(2));
        Err(io::Error::other("gave up"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Claims every byte but writes one fewer of each chunk.
struct Short(std::fs::File);

impl Write for Short {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write_all(&buf[..buf.len().saturating_sub(1)])?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn eio() -> io::Error {
    io::Error::from_raw_os_error(5)
}

impl DeliverIo for Faulty {
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn Write + Send>> {
        let file = OsIo.create_new(path)?;
        if self.stall_copy {
            return Ok(Box::new(Stuck));
        }
        if self.short_copy {
            drop(file);
            let f = std::fs::OpenOptions::new().write(true).open(path)?;
            return Ok(Box::new(Short(f)));
        }
        Ok(file)
    }

    fn sync(
        &self,
        file: &std::fs::File,
        halt: &Halt,
        on_progress: &mut dyn FnMut(u64, u64),
    ) -> io::Result<()> {
        if self.sync_fails && file.metadata()?.is_file() {
            return Err(eio());
        }
        OsIo.sync(file, halt, on_progress)
    }

    fn open_read(&self, path: &Path) -> io::Result<Box<dyn ReadSeek>> {
        let is_copy = path.to_string_lossy().ends_with(".partial");
        if self.read_fails && is_copy {
            return Err(eio());
        }
        if let Some(bytes) = self.read_instead.as_ref().filter(|_| is_copy) {
            return Ok(Box::new(io::Cursor::new(bytes.clone())));
        }
        OsIo.open_read(path)
    }

    fn land(&self, partial: &Path, target: &Path, replace: bool) -> io::Result<()> {
        if self.land_fails {
            return Err(eio());
        }
        land(partial, target, replace)
    }

    fn timing(&self) -> Timing {
        Timing {
            stall: Duration::from_millis(200),
            activity_every: Duration::from_millis(1),
            lock_beat: Duration::from_millis(10),
        }
    }
}

struct Setup {
    _t: tempfile::TempDir,
    stage: PathBuf,
    mkv: PathBuf,
    target: PathBuf,
    iso: PathBuf,
    probe: libfreemkv::MkvProbe,
}

fn setup() -> Setup {
    let t = tempfile::tempdir().unwrap();
    let stage = t.path().join("stage");
    let library = t.path().join("library");
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::create_dir_all(library.join("A")).unwrap();
    let local = stage_file(&stage, 7);
    std::fs::write(&local, movie()).unwrap();
    let probe = libfreemkv::probe_mkv_with_cues(std::fs::File::open(&local).unwrap()).unwrap();
    let iso = t.path().join("A.iso");
    std::fs::write(&iso, b"iso").unwrap();
    Setup {
        stage,
        mkv: local,
        target: library.join("A/A.mkv"),
        iso,
        probe,
        _t: t,
    }
}

fn run(
    s: &Setup,
    replace: bool,
    sink: &Events,
    halt: &Halt,
    io: &dyn DeliverIo,
) -> io::Result<bool> {
    let f = Finished {
        file: &s.mkv,
        target: &s.target,
        replace,
        iso: &s.iso,
        title: Some(2),
        expected_secs: Some(60.0),
        verified: &s.probe,
        target_before: TargetStamp::of(&s.target),
    };
    deliver_finished(&f, sink, halt, io)
}

fn lock_of(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

// A storage failure keeps the verified file: the kept pair, the step, nothing left behind.
fn assert_kept(s: &Setup, e: &io::Error, phase: &str) -> KeptInfo {
    let k = kept_of(e).unwrap_or_else(|| panic!("not kept: {e}"));
    assert_eq!(k.phase, phase, "{e}");
    assert!(!s.mkv.exists(), "the job's file became the kept one");
    assert_eq!(std::fs::read(&k.path).unwrap(), movie());
    assert!(!partial_path(&s.target).exists());
    assert!(!lock_of(&s.target).exists());
    let info = read(&k.path).unwrap();
    assert_eq!(
        (
            &info.target,
            info.attempts,
            info.record.failed_phase.as_str()
        ),
        (&s.target, 1, phase)
    );
    assert_eq!(info.record.title, Some(2));
    assert_eq!(info.record.runtime_secs, muxed_runtime(&s.probe));
    assert_eq!(info.record.expected_secs, Some(60.0));
    assert_eq!(info.record.iso_len, Some(3));
    assert_eq!(e.to_string(), k.cause.to_string(), "Display is the cause's");
    assert_eq!(pending(&s.stage).len(), 1);
    info
}

fn assert_nothing_kept(s: &Setup) {
    assert!(!s.mkv.exists());
    assert!(pending(&s.stage).is_empty() && orphans(&s.stage).is_empty());
    assert!(!partial_path(&s.target).exists());
    assert!(!lock_of(&s.target).exists());
}

#[test]
fn a_verified_file_is_copied_in_and_the_local_one_removed() {
    let s = setup();
    let sink = Events::default();
    assert!(!run(&s, false, &sink, &Halt::new(), &OsIo).unwrap());
    assert_eq!(std::fs::read(&s.target).unwrap(), movie());
    assert_nothing_kept(&s);
    assert_eq!(sink.phases(), ["copy", "sync", "verify", "replace"]);
    assert!(sink.seen().contains(&"verified:true".to_string()));
    assert!(!sink.seen().contains(&"replaced".to_string()));
}

#[test]
fn an_existing_file_is_replaced_only_when_asked() {
    let s = setup();
    std::fs::write(&s.target, b"old").unwrap();
    let sink = Events::default();
    let e = run(&s, false, &sink, &Halt::new(), &OsIo).unwrap_err();
    assert_eq!(
        libfreemkv::error_code(&e),
        Some(libfreemkv::error::E_REMUX_TARGET_EXISTS)
    );
    assert!(
        kept_of(&e).is_none(),
        "a target that is there keeps nothing"
    );
    assert_nothing_kept(&s);
    assert_eq!(std::fs::read(&s.target).unwrap(), b"old");

    let s = setup();
    std::fs::write(&s.target, b"old").unwrap();
    let sink = Events::default();
    assert!(run(&s, true, &sink, &Halt::new(), &OsIo).unwrap());
    assert_eq!(std::fs::read(&s.target).unwrap(), movie());
    assert!(sink.seen().contains(&"replaced".to_string()));
    assert_nothing_kept(&s);
}

#[test]
fn a_stalled_copy_times_out_and_keeps_the_file() {
    let s = setup();
    let io = Faulty {
        stall_copy: true,
        ..Faulty::default()
    };
    let e = run(&s, false, &Events::default(), &Halt::new(), &io).unwrap_err();
    assert_eq!(
        libfreemkv::error_code(&e),
        Some(libfreemkv::error::E_TIMED_OUT)
    );
    let info = assert_kept(&s, &e, "copy");
    assert_eq!(info.error_code, Some(libfreemkv::error::E_TIMED_OUT));
    assert!(!s.target.exists());
}

#[test]
fn a_missing_output_folder_keeps_the_file() {
    let s = setup();
    let gone = s
        .target
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("nowhere/A");
    let s = Setup {
        target: gone.join("A.mkv"),
        ..s
    };
    let e = run(&s, false, &Events::default(), &Halt::new(), &OsIo).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    assert_kept(&s, &e, "copy");
}

#[cfg(unix)]
#[test]
fn a_read_only_output_folder_keeps_the_file() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup();
    let dir = s.target.parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Root writes anyway; nothing to show there.
    if std::fs::write(dir.join("probe"), b"").is_ok() {
        return;
    }
    let e = run(&s, false, &Events::default(), &Halt::new(), &OsIo).unwrap_err();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    assert_kept(&s, &e, "copy");
}

#[test]
fn a_failed_sync_verify_or_rename_keeps_the_file_with_its_os_error() {
    let cases = [
        (
            Faulty {
                sync_fails: true,
                ..Faulty::default()
            },
            "sync",
        ),
        (
            Faulty {
                read_fails: true,
                ..Faulty::default()
            },
            "verify",
        ),
        (
            Faulty {
                land_fails: true,
                ..Faulty::default()
            },
            "replace",
        ),
    ];
    for (io, phase) in cases {
        let s = setup();
        let e = run(&s, false, &Events::default(), &Halt::new(), &io).unwrap_err();
        assert_eq!(
            kept_of(&e).unwrap().cause.raw_os_error(),
            Some(5),
            "{phase}"
        );
        assert_kept(&s, &e, phase);
        assert!(!s.target.exists(), "{phase}");
    }
}

#[test]
fn a_copy_that_does_not_match_keeps_the_good_local_file() {
    let s = setup();
    let io = Faulty {
        short_copy: true,
        ..Faulty::default()
    };
    let e = run(&s, false, &Events::default(), &Halt::new(), &io).unwrap_err();
    assert_eq!(
        libfreemkv::error_code(&e),
        Some(libfreemkv::error::E_STAGED_COPY_SIZE_MISMATCH)
    );
    assert_kept(&s, &e, "copy");

    // Same size, other content: the copy's runtime is not the local file's.
    let s = setup();
    let mut other = mkv("freemkv 1.0.0 (gtest)", Some(60.0), Some(30), true);
    other.resize(movie().len(), 0);
    let io = Faulty {
        read_instead: Some(other),
        ..Faulty::default()
    };
    let sink = Events::default();
    let e = run(&s, false, &sink, &Halt::new(), &io).unwrap_err();
    assert_eq!(
        libfreemkv::error_code(&e),
        Some(libfreemkv::error::E_REMUX_VERIFY_FAILED)
    );
    assert!(sink.seen().contains(&"verified:false".to_string()));
    assert_kept(&s, &e, "verify");
}

#[test]
fn a_stop_keeps_nothing() {
    let s = setup();
    let halt = Halt::new();
    let io = Faulty {
        stall_copy: true,
        ..Faulty::default()
    };
    let stopper = {
        let halt = halt.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            halt.cancel();
        })
    };
    let e = run(&s, false, &Events::default(), &halt, &io).unwrap_err();
    stopper.join().unwrap();
    assert!(libfreemkv::is_halt(&e), "{e}");
    assert_nothing_kept(&s);

    // The job's own Stop arrives through the sink.
    let s = setup();
    let sink = Events::default();
    sink.cancel.store(true, Ordering::SeqCst);
    let e = run(&s, false, &sink, &Halt::new(), &OsIo).unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    assert_nothing_kept(&s);
    assert!(!s.target.exists());
}

fn kept_after_a_failure() -> (Setup, PathBuf) {
    let s = setup();
    let io = Faulty {
        land_fails: true,
        ..Faulty::default()
    };
    let e = run(&s, false, &Events::default(), &Halt::new(), &io).unwrap_err();
    let path = kept_of(&e).unwrap().path.clone();
    (s, path)
}

#[test]
fn a_resume_copies_the_kept_file_in_and_removes_the_pair() {
    let (s, kept) = kept_after_a_failure();
    let sink = Events::default();
    let done = resume(&kept, &sink, &Halt::new(), &OsIo).unwrap();
    assert_eq!(done.writing_app.as_deref(), Some("freemkv 1.0.0 (gtest)"));
    assert_eq!(std::fs::read(&s.target).unwrap(), movie());
    assert!(pending(&s.stage).is_empty() && orphans(&s.stage).is_empty());
    assert!(std::fs::read_dir(&s.stage).unwrap().next().is_none());
    assert_eq!(
        sink.phases(),
        ["verify", "copy", "sync", "verify", "replace"],
        "the kept file is re-checked, then only the delivery runs"
    );
}

#[test]
fn a_resume_that_fails_again_keeps_the_pair_and_counts_the_attempt() {
    let (s, kept) = kept_after_a_failure();
    let io = Faulty {
        sync_fails: true,
        ..Faulty::default()
    };
    let e = resume(&kept, &Events::default(), &Halt::new(), &io).unwrap_err();
    let k = kept_of(&e).unwrap();
    assert_eq!((k.path.as_path(), k.phase), (kept.as_path(), "sync"));
    let info = read(&kept).unwrap();
    assert_eq!(
        (info.attempts, info.record.failed_phase.as_str()),
        (2, "sync")
    );
    assert!(!s.target.exists() && !partial_path(&s.target).exists());
    assert!(!lock_of(&s.target).exists());
}

#[test]
fn a_resume_leaves_the_pair_when_the_library_file_changed_or_on_a_stop() {
    let (s, kept) = kept_after_a_failure();
    std::fs::write(&s.target, b"someone else's").unwrap();
    let e = resume(&kept, &Events::default(), &Halt::new(), &OsIo).unwrap_err();
    assert_eq!(
        libfreemkv::error_code(&e),
        Some(libfreemkv::error::E_REMUX_TARGET_EXISTS)
    );
    assert!(kept_of(&e).is_none());
    assert!(is_kept(&kept));
    assert_eq!(read(&kept).unwrap().attempts, 1);
    assert_eq!(std::fs::read(&s.target).unwrap(), b"someone else's");

    let (_s, kept) = kept_after_a_failure();
    let sink = Events::default();
    sink.cancel.store(true, Ordering::SeqCst);
    let e = resume(&kept, &sink, &Halt::new(), &OsIo).unwrap_err();
    assert!(libfreemkv::is_halt(&e), "{e}");
    assert!(is_kept(&kept), "a Stop leaves the kept file offered");
    assert_eq!(read(&kept).unwrap().attempts, 1);
}

#[test]
fn an_unusable_kept_file_is_deleted_with_its_sidecar() {
    // Shorter than recorded.
    let (s, kept) = kept_after_a_failure();
    std::fs::write(&kept, b"short").unwrap();
    let e = resume(&kept, &Events::default(), &Halt::new(), &OsIo).unwrap_err();
    assert_eq!(
        libfreemkv::error_code(&e),
        Some(libfreemkv::error::E_STAGED_COPY_SIZE_MISMATCH)
    );
    assert!(std::fs::read_dir(&s.stage).unwrap().next().is_none());

    // The same size, but no longer an MKV.
    let (s, kept) = kept_after_a_failure();
    std::fs::write(&kept, vec![0u8; movie().len()]).unwrap();
    let e = resume(&kept, &Events::default(), &Halt::new(), &OsIo).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    assert!(std::fs::read_dir(&s.stage).unwrap().next().is_none());
    assert!(!s.target.exists());

    // A damaged sidecar.
    let (s, kept) = kept_after_a_failure();
    std::fs::write(kept.with_extension("json"), b"{").unwrap();
    let e = resume(&kept, &Events::default(), &Halt::new(), &OsIo).unwrap_err();
    assert_eq!(
        libfreemkv::error_code(&e),
        Some(libfreemkv::error::E_REMUX_STAGING_INVALID)
    );
    assert!(std::fs::read_dir(&s.stage).unwrap().next().is_none());
}

#[test]
fn the_staging_folder_lists_kept_pairs_and_debris_and_stays_bounded() {
    let t = tempfile::tempdir().unwrap();
    let stage = t.path();
    let a = write_kept(stage, "A.1", Path::new("/m/A/A.mkv"), &[1; 100], 20);
    let b = write_kept(stage, "B.1", Path::new("/m/B/B.mkv"), &[1; 300], 10);
    let lone = stage.join("Lone.2.staged.mkv");
    std::fs::write(&lone, b"no sidecar").unwrap();
    let half = stage.join("A.1.staged.json.tmp");
    std::fs::write(&half, b"{").unwrap();
    let job_file = stage.join("9.mkv.partial");
    std::fs::write(&job_file, b"x").unwrap();
    let newer = stage.join("N.1.staged.json");
    std::fs::write(stage.join("N.1.staged.mkv"), b"n").unwrap();
    std::fs::write(&newer, br#"{"format": 99}"#).unwrap();

    let found: Vec<PathBuf> = pending(stage).into_iter().map(|i| i.staged).collect();
    assert_eq!(found, [b.clone(), a.clone()], "oldest first");
    assert_eq!(orphans(stage), [half.clone(), lone.clone()]);
    assert!(is_kept(&a) && is_kept(&a.with_extension("json")) && is_kept(&newer));
    assert!(!is_kept(&lone) && !is_kept(&job_file));

    let all = pending(stage);
    assert!(expired(&all[0], Duration::from_secs(60)));
    let future = KeptInfo {
        created_at: SystemTime::now() + Duration::from_secs(3600),
        ..all[0].clone()
    };
    assert!(!expired(&future, Duration::ZERO), "a clock that moved back");
    let over: Vec<&Path> = over_budget(&all, 250)
        .into_iter()
        .map(|i| i.staged.as_path())
        .collect();
    assert_eq!(over, [b.as_path()], "the oldest goes until the rest fit");
    assert!(over_budget(&all, 400).is_empty());

    assert_eq!(
        libfreemkv::error_code(&discard(&job_file).unwrap_err()),
        Some(libfreemkv::error::E_REMUX_STAGING_INVALID)
    );
    assert!(job_file.exists(), "not a kept name: untouched");
    discard(&a.with_extension("json")).unwrap();
    assert!(!a.exists() && !a.with_extension("json").exists() && !half.exists());
    discard(&a).unwrap();
}

#[test]
fn one_target_has_one_kept_name_and_the_sidecar_is_written_whole() {
    let (one, _) = kept_paths(Path::new("/s"), Path::new("/m/A: B?/A: B?.mkv"));
    let (same, _) = kept_paths(Path::new("/s"), Path::new("/m/A: B?/A: B?.mkv"));
    let (other, _) = kept_paths(Path::new("/s"), Path::new("/m/A2/A: B?.mkv"));
    assert_eq!(one, same);
    assert_ne!(one, other);
    let name = one.file_name().unwrap().to_str().unwrap();
    assert!(
        name.starts_with("A_ B_.") && name.ends_with(KEPT_MKV),
        "{name}"
    );

    let (s, kept) = kept_after_a_failure();
    assert_eq!(
        std::fs::read_dir(&s.stage).unwrap().count(),
        2,
        "the MKV and its sidecar, no temp file"
    );
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(kept.with_extension("json")).unwrap()).unwrap();
    for field in [
        "target",
        "replace",
        "iso",
        "iso_len",
        "iso_mtime_secs",
        "title",
        "size",
        "runtime_secs",
        "expected_secs",
        "writing_app",
        "engine_version",
        "created_at",
        "attempts",
        "last_attempt_at",
        "failed_phase",
        "error",
        "error_code",
    ] {
        assert!(raw.get(field).is_some(), "{field}");
    }
}

#[test]
fn delayed_cleanup_keeps_the_artifact_locked_without_blocking_the_caller() {
    let t = tempfile::tempdir().unwrap();
    let target = t.path().join("movie.mkv");
    let lock = ArtifactLock::acquire(&target, &[], &Halt::new()).unwrap();
    let path = lock.path().to_path_buf();
    let contender = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let (finished, done) = std::sync::mpsc::channel();
    let started = Instant::now();
    assert!(
        !bounded_cleanup(
            move || {
                wait.recv().unwrap();
                lock.delete().unwrap();
                finished.send(()).unwrap();
            },
            Duration::from_millis(20)
        )
        .unwrap()
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        contender.try_lock().is_err(),
        "cleanup must retain exclusive ownership"
    );
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(!path.exists());
}

#[test]
fn copy_checkpoints_bound_buffering_and_surface_sync_failures() {
    let calls = std::cell::Cell::new(0);
    let mut writer = CheckpointWriter {
        writer: Vec::new(),
        pending: 0,
        limit: 4,
        sync: |_: &mut Vec<u8>| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                Err(io::Error::from(io::ErrorKind::StorageFull))
            } else {
                Ok(())
            }
        },
    };
    writer.write_all(b"ab").unwrap();
    assert_eq!(calls.get(), 0);
    writer.write_all(b"cd").unwrap();
    assert_eq!(calls.get(), 1);
    writer.write_all(b"ef").unwrap();
    assert_eq!(
        writer.write_all(b"gh").unwrap_err().kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(calls.get(), 2);
}
