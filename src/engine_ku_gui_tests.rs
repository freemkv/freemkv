//! The GUI half of KU-F1 over a real image file (keys-upfront design §7.3): Open,
//! preflight and Run make one resolution; E7034 and the insert-the-disc Retry; the
//! same requests and verdicts as the CLI (FK3); nothing key-shaped on disk (FK10).
use super::*;
use crate::ku_fixtures::*;
use crate::rip_keys::{with_drive, with_sources};

fn req(iso: &std::path::Path, out: &std::path::Path, titles: Vec<usize>) -> RipRequest {
    RipRequest {
        source: iso.display().to_string(),
        dest_dir: out.display().to_string(),
        titles,
        title_ids: Vec::new(),
        format: "MKV".into(),
        audio_pids: vec![],
        sub_pids: vec![],
        title_pids: TitleStreams::Unspecified,
        explicit_streams: false,
        raw: false,
        force: false,
        filename_template: String::new(),
        decrypt_threads: 0,
        multipass: false,
        max_passes: 0,
        abort_lost_secs: 0,
        keep_iso: false,
        auto_eject: false,
        keys: KeyConfig::default(),
        seed: None,
        vid_from: None,
    }
}

fn run(r: &RipRequest) -> (Result<String, String>, Arc<RunState>) {
    let st = Arc::new(RunState::default());
    let out = run_blocking(r, &UiSink(st.clone()), &st);
    (out, st)
}

// FT14 (stop design v5 §4.3, §5.5): a GUI open of an `iso://` source against a
// never-answering key service; its token is cancelled → the open "returns `Halted` ≤ 1 s;
// no key strip update". Per spec; do not change without a spec citation.
#[test]
fn gui_open_token_cancels_inflight_key_lookup() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("ft14");
    let iso = fx.write(dir.path(), "disc.iso");
    let calls = Calls::default();
    let f = factory(&[(Answer::Hang, &[K1])], &calls);
    let tok = OpenToken::default();
    let stop = tok.halt.clone();
    let seen = calls.clone();
    let press = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while seen.len() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        stop.cancel();
        std::time::Instant::now()
    });
    let r = with_sources(f, || {
        scan_with_keys_under(&iso.display().to_string(), &KeyConfig::default(), &tok)
    });
    let pressed = press.join().unwrap();
    assert!(
        pressed.elapsed() <= std::time::Duration::from_secs(1),
        "Stop took too long"
    );
    assert_eq!(calls.len(), 1, "the key lookup was in flight");
    let e = r.expect_err("a stopped open yields no scan, so no key strip update");
    assert!(
        e.contains(&format!("E{}", libfreemkv::Error::Halted.code())),
        "{e}"
    );
}

// Stop design v5 §2.5: "The lock is taken only by the freemkv CLI/GUI and the engine
// `_with` entries"; "Deleted on success … Kept after Stop". While another process holds
// `<final>.lock` the GUI writes nothing and Stop ends the wait. Per spec.
#[test]
fn gui_iso_holds_the_artifact_lock() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("gui-lock");
    let iso = fx.write(dir.path(), "disc.iso");
    let out = dir.path().join("out");
    let mut r = req(&iso, &out, Vec::new());
    r.format = "ISO image".into();
    let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    with_sources(f, || run(&r).0.expect("the copy"));
    let written = files_under(&out);
    let image = written
        .iter()
        .find(|p| p.extension() == Some("iso".as_ref()));
    let image = image.expect("the image").clone();
    assert!(
        !written
            .iter()
            .any(|p| p.extension() == Some("lock".as_ref())),
        "deleted on success: {written:?}"
    );

    std::fs::remove_dir_all(&out).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let never = libfreemkv::Halt::new();
    let held = libfreemkv::io::ArtifactLock::acquire(&image, &[], &never).unwrap();
    let st = Arc::new(RunState::default());
    let stop = st.clone();
    let press = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        stop.cancel.store(true, Ordering::SeqCst);
    });
    let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let res = with_sources(f, || run_blocking(&r, &UiSink(st.clone()), &st));
    press.join().unwrap();
    drop(held);
    assert!(
        !image.exists(),
        "nothing written under another holder's lock: {res:?}"
    );
}

// §2.5: "Kept after Stop … where it guards the resumable artifact"; ST-I2's "progress
// kept" (§5.7) says so. A Stop that left no image keeps nothing. Per spec.
#[test]
fn a_stop_that_keeps_the_image_says_progress_kept() {
    let dir = TempDir::new("gui-kept");
    let iso = dir.path().join("Movie.iso");
    let sidecar = dir.path().join("Movie.iso.lock");
    let kept = crate::strings::get("stop.progress_kept");
    let never = libfreemkv::Halt::new();
    // (image on disk, the copy halted) → (sidecar kept, "progress kept" said). A Stop
    // pressed after the copy finished halted nothing: the op succeeded (§2.5).
    for (on_disk, halted, says) in [
        (true, true, true),
        (false, true, false),
        (true, false, false),
    ] {
        let st = Arc::new(RunState::default());
        st.cancel.store(true, Ordering::SeqCst);
        if on_disk {
            std::fs::write(&iso, b"partial").unwrap();
        }
        let lock = crate::artifact_lock::hold_iso(&iso, &never).unwrap();
        release_iso_lock(lock, &Ok("done".into()), halted, &iso, &st);
        let lines = st.lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let case = format!("{on_disk}/{halted}: {lines:?}");
        assert_eq!(lines.contains(&kept), says, "{case}");
        assert_eq!(sidecar.exists(), says, "{case}");
        let _ = std::fs::remove_file(&iso);
        let _ = std::fs::remove_file(&sidecar);
    }
}

// Stop design v5 §4.3, "The open token": "Both are threaded into the scan … and into the
// up-front resolution". FT15e/FT15g (a) need a live drive, so this reads the wiring.
#[test]
fn the_open_token_reaches_the_drive_scan_and_the_resolve() {
    let src = include_str!("engine.rs").replace("\r\n", "\n");
    let body = |name: &str| {
        let a = src.find(name).expect(name);
        src[a..a + src[a..].find("\n}\n").unwrap()].to_string()
    };
    let scan = body("\nfn drive_scan(");
    assert!(scan.contains("fe::open_scan_with(") && scan.contains("&tok.halt,"));
    assert!(scan.contains("&tok.progress,"), "{scan}");
    // FT15g(a): the cold keydb parse for the drive's host certs holds `busy()`.
    let creds = scan.find("session_credentials(keys)").expect("credentials");
    let busy = scan
        .find("tok.progress.busy()")
        .expect("busy over the parse");
    assert!(busy < creds && creds < scan.find("fe::open_scan_with(").unwrap());
    let open = body("\npub fn scan_disc_with_keys(");
    assert!(open.contains("drive_scan(source, keys, tok)"), "{open}");
    assert!(
        open.contains("crate::rip_keys::resolve_observed("),
        "{open}"
    );
    assert!(open.contains("&tok.progress,"), "{open}");
}

// §2.5: the lock is "created at op start and held for the whole op". The drive's ISO
// and staging copy need a live drive, so this reads the wiring; the image arm runs it.
#[test]
fn gui_drive_iso_holds_the_artifact_lock() {
    let src = include_str!("engine.rs").replace("\r\n", "\n");
    let a = src
        .find("\nfn run_disc_scanning(")
        .expect("run_disc_scanning");
    let body = &src[a..a + src[a..].find("\nfn mux_staged_titles(").expect("next fn")];
    let lock = body.find("hold_iso_lock(").expect("the lock");
    assert!(
        lock < body
            .find("fe::run_with(&plan, with, sink)")
            .expect("the copy")
    );
    assert!(body.contains("release_iso_lock(lock, &res, halted,"));
}

/// FK2 (KU §2.5: "GUI open | `Titles([main])` for status. The result seeds the rip's
/// `resolve`"): Open, preflight and Start over a 3-title, 2-group image make 2 requests
/// in total, all before any output. FK10: no key or VID on disk.
#[test]
fn gui_rip_resolves_once_per_rip() {
    let fx = bd_image(&[Some(K1), Some(K2), Some(K1)], 2);
    let dir = TempDir::new("fk2");
    let iso = fx.write(dir.path(), "disc.iso");
    let out = dir.path().join("out");
    let calls = Calls::default();
    calls.watch(&out);
    let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
    with_sources(f, || {
        let sc = scan_with_keys(&iso.display().to_string(), &KeyConfig::default()).unwrap();
        assert_eq!(calls.len(), 1, "Open resolves the main title's group");
        let seed = sc.keys.clone().expect("Open holds the seed set");
        let path = iso.display().to_string();
        let blocked = preflight_with_keys(&path, "/tmp", &[], Some(&seed)).unwrap();
        assert!(blocked.is_empty(), "{blocked:?}");
        assert_eq!(calls.len(), 1, "preflight asks nothing");
        let mut r = req(&iso, &out, vec![0, 1, 2]);
        r.seed = Some(seed);
        let (done, _) = run(&r);
        done.expect("the rip");
    });
    assert_eq!(calls.len(), 2, "Start asks only for the group Open lacked");
    assert!(calls.all().iter().all(|c| c.outputs == 0), "asked mid-rip");
    assert_eq!(files_under(&out).len(), 3);
    assert_no_secret_on_disk(dir.path(), &[K1, K2, VID]);
}

fn drive() -> Result<libfreemkv::Disc, libfreemkv::Error> {
    Ok(drive_disc(&bd_image(&[Some(K1)], 1)))
}

fn no_drive() -> Result<libfreemkv::Disc, libfreemkv::Error> {
    Err(libfreemkv::Error::DeviceNotFound {
        path: String::new(),
    })
}

fn other_disc() -> Result<libfreemkv::Disc, libfreemkv::Error> {
    let mut d = drive_disc(&bd_image(&[Some(K2)], 1));
    d.aacs.as_mut().unwrap().disc_hash = "ab".repeat(20);
    Ok(d)
}

/// M1 (KU §4.2 "Retry does the same `open_scan` and re-open"): a Retry that fails before
/// its resolve (no drive, the wrong disc, a Stop) stays the Retry; one the key sources
/// answered does not.
#[test]
fn a_retry_that_never_resolved_stays_armed() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("m1");
    let iso = fx.write(dir.path(), "capture.iso");
    sidecar(&fx, &iso, true);
    let out = dir.path().join("out");
    let mut retry = req(&iso, &out, vec![0]);
    retry.vid_from = Some("disc://".into());
    let retry_with = |scan: fn() -> Result<libfreemkv::Disc, libfreemkv::Error>, stop: bool| {
        let calls = Calls::default();
        let f = factory(&[(Answer::OnlineNeedsVid, &[K1])], &calls);
        let st = Arc::new(RunState::default());
        st.cancel.store(stop, Ordering::SeqCst);
        let r = with_drive(scan, || {
            with_sources(f, || run_blocking(&retry, &UiSink(st.clone()), &st))
        });
        (r, st.needs_disc.load(Ordering::SeqCst))
    };
    let (r, armed) = retry_with(no_drive, false);
    assert!(r.is_err() && armed, "no drive: {r:?}");
    let (r, armed) = retry_with(other_disc, false);
    assert!(r.is_err() && armed, "the wrong disc: {r:?}");
    let (r, armed) = retry_with(drive, true);
    assert!(r.is_err() && armed, "a Stop: {r:?}");
    let (r, armed) = retry_with(drive, false);
    assert!(r.is_ok() && !armed, "the disc's VID finished it: {r:?}");
}

/// FK11 (KU §4.2 GUI row: “An "Insert the disc" prompt … and Retry”, no picker):
/// the E7034 text and the insert-the-disc step, flagged for the shell; Retry scans the
/// drive (no key call) and makes the one more request, with the VID (KS-16).
#[test]
fn gui_surfaces_e7034_then_retry_finishes() {
    use libfreemkv::spec::keys::KS_29_VID_FROM_MEDIA;
    assert!(KS_29_VID_FROM_MEDIA.text.contains("from the media"));
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("fk11");
    let iso = fx.write(dir.path(), "capture.iso");
    sidecar(&fx, &iso, true);
    let out = dir.path().join("out");
    let path = iso.display().to_string();
    // A Start with no Open (a staged ISO's mux): one request, then E7034 and Retry.
    let calls = Calls::default();
    let f = factory(&[(Answer::OnlineNeedsVid, &[K1])], &calls);
    let (done, st) = with_sources(f, || run(&req(&iso, &out, vec![0])));
    let msg = done.unwrap_err();
    assert!(msg.contains(&explain(7034)), "{msg}");
    assert!(msg.contains(&insert_disc_retry()), "{msg}");
    assert!(
        st.needs_disc.load(Ordering::SeqCst),
        "the shell offers Retry"
    );
    assert_eq!(calls.len(), 1);
    assert!(files_under(&out).is_empty(), "before any output");
    // Open already knows; Start is then the Retry: "at most one more resolve, now with
    // the VID" (KU §4.2), after a drive scan with no key call.
    let calls = Calls::default();
    let f = factory(&[(Answer::OnlineNeedsVid, &[K1])], &calls);
    with_sources(f, || {
        let sc = scan_with_keys(&path, &KeyConfig::default()).unwrap();
        assert!(sc.needs_disc && sc.keys.is_none(), "{sc:?}");
        let mut retry = req(&iso, &out, vec![0]);
        retry.vid_from = Some("disc://".into());
        let (done, _) = with_drive(drive, || run(&retry));
        done.expect("Retry with the disc");
    });
    let vids: Vec<_> = calls.all().iter().map(|c| c.vid).collect();
    assert_eq!(vids, vec![None, Some(VID)], "Open, then Retry with the VID");
    assert_eq!(files_under(&out).len(), 1);
    assert_no_secret_on_disk(dir.path(), &[K1, VID]);
}

/// M4 (KU §4.3 "GUI multipass: staged raw ISO → mux | Yes | The set from Start is reused for
/// the mux (`open_image_with(Known)` or `Seeded`)"; J14 no rescan): the staged image's
/// playlists are unreadable, yet every title muxes from the drive's scan, 0 requests.
#[test]
fn a_staged_mux_reuses_the_rips_set_and_the_drive_scan() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = TempDir::new("m4");
    let iso = fx.write(dir.path(), "staged.iso");
    sidecar(&fx, &iso, false);
    let mut bytes = std::fs::read(&iso).unwrap();
    let playlists = &fx.metadata[..fx.metadata.len() - 1];
    for &(start, n) in playlists {
        bytes[start as usize * 2048..(start + n) as usize * 2048].fill(0);
    }
    std::fs::write(&iso, bytes).unwrap();
    let calls = Calls::default();
    let scope = libfreemkv::keys::KeyScope::Titles(vec![0, 1]);
    let set = resolve(&fx, scope, &[(Answer::Keydb, &[K1, K2])], &calls).unwrap();
    let out = dir.path().join("out");
    let st = Arc::new(RunState::default());
    let f = factory(&[(Answer::Keydb, &[K1, K2])], &calls);
    let r = req(&iso, &out, vec![0, 1]);
    let path = iso.display().to_string();
    let done = with_sources(f, || {
        mux_staged_titles(
            &r,
            &path,
            rescan(&fx),
            set,
            &[0, 1],
            "DISC",
            &UiSink(st.clone()),
            &st,
        )
    });
    done.expect("both titles mux from the drive's scan");
    assert_eq!(calls.len(), 1, "only Start's resolve asked");
    assert_eq!(files_under(&out).len(), 2);
}

/// FT2 (§2.5): a raw disc→ISO rip with auto_eject ejects on the scan's own handle once
/// the read is done: the one open is the scan, and LoEj reaches that handle.
#[test]
fn disc_iso_rip_ejects_on_the_held_handle() {
    use libfreemkv::test_util::FakeTransport;
    let fx = bd_image(&[None], 1);
    let dir = TempDir::new("ft2-iso");
    let (t, fake) = FakeTransport::new();
    let t = t.with_image(fx.img.image.clone());
    let mut r = req(std::path::Path::new("disc://"), dir.path(), vec![]);
    r.source = "disc://".into();
    r.format = "Whole disc → ISO image".into();
    r.raw = true;
    r.auto_eject = true;
    let state = Arc::new(RunState::default());
    let mut opens = 0;
    let res = run_disc_scanning(&r, &UiSink(state.clone()), &state, |_, _, raw| {
        opens += 1;
        let drive = libfreemkv::Drive::from_transport(Box::new(t));
        let mut s = libfreemkv::DiscSession::from_drive(drive);
        let opts = libfreemkv::ScanOptions {
            raw_copy: raw,
            ..Default::default()
        };
        s.scan(opts)?;
        Ok(s)
    });
    let lines = state
        .lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(res.is_ok(), "{res:?} {lines:?}");
    assert_eq!(opens, 1, "the scan's open is the only one");
    let eject = |c: &[u8]| c[0] == 0x1B && c[4] & 0x03 == 0x02;
    assert_eq!(fake.count(eject), 1, "{lines:?}");
    assert!(lines.iter().any(|l| l == "ejected test"), "{lines:?}");
    assert_eq!(fake.live_handles(), 0);
}

/// FT4 (stop design §2.6): through the GUI worker a Stop reads Stopped; a stall timeout
/// (E9073) or a `Halted` with no Stop reads Failed. A Stop while "Finishing…" after
/// every title is written stays a cancel (user decision).
#[test]
fn gui_worker_stop_renders_stopped_and_timed_out_renders_failed() {
    let dir = TempDir::new("ft4");
    let r = || req(&dir.path().join("disc.iso"), dir.path(), vec![0]);
    let run = |res: Result<String, String>, stop: bool| {
        let st = Arc::new(RunState::default());
        start_rip_with(r(), st.clone(), move |_, _, st| {
            st.cancel.store(stop, Ordering::SeqCst);
            res
        })
        .join()
        .expect("worker");
        assert!(st.finished.load(Ordering::Acquire));
        let outcome = *st.outcome.lock().unwrap_or_else(|e| e.into_inner());
        let lines = st.lines.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (crate::ui::result_heading(outcome), outcome, lines)
    };
    let halted = format!("{}", libfreemkv::Error::Halted);
    let (heading, outcome, _) = run(Err(halted.clone()), true);
    assert_eq!(outcome, RunOutcome::Cancelled);
    assert_eq!(heading, crate::strings::get("gui.result.cancelled"));
    assert_eq!(run(Err(halted), false).1, RunOutcome::Failed);

    let e = libfreemkv::Error::TimedOut {
        op: "artifact_lock",
    };
    let iso = dir.path().join("Movie.iso");
    let text = crate::artifact_lock::lock_failed(&e, &iso).expect("E9073 text");
    let (heading, outcome, lines) = run(Err(text.clone()), false);
    assert_eq!(outcome, RunOutcome::Failed);
    assert_eq!(heading, crate::strings::get("gui.result.nothing"));
    assert!(lines.contains(&text), "{lines:?}");

    let (_, outcome, _) = run(Ok("2 titles written".into()), true);
    assert_eq!(outcome, RunOutcome::Cancelled, "Stop while Finishing…");
}

/// M2 (KU §2.3 step 13 "Halt is checked before each piece, probe and request"): a Stop
/// during the up-front resolve is a cancel, never a failure; any other error still fails.
#[test]
fn a_stop_during_the_resolve_reads_as_cancelled() {
    let halted = format!("{}", libfreemkv::Error::Halted);
    let (text, verdict) = run_verdict(Err(halted.clone()), true);
    assert_eq!(verdict, RunOutcome::Cancelled);
    assert_eq!(text, crate::strings::get("rip.interrupted"));
    assert_eq!(run_verdict(Err(halted), false).1, RunOutcome::Failed);
    assert_eq!(
        run_verdict(Ok("done".into()), true).1,
        RunOutcome::Cancelled
    );
    assert_eq!(
        run_verdict(Ok("done".into()), false).1,
        RunOutcome::Completed
    );
}

/// FK3 (KU §7.3): the GUI reaches the shared table's requests and verdicts, the same
/// table the CLI's test checks.
#[test]
fn cli_and_gui_same_requests_same_verdicts() {
    for case in fk3_cases() {
        let dir = TempDir::new(case.name);
        let iso = case.image(dir.path());
        let calls = Calls::default();
        let out = dir.path().join("out");
        let r = req(&iso, &out, vec![0, 1][..case.clips.len()].to_vec());
        let (done, _) = with_sources(case.sources(&calls), || run(&r));
        assert_eq!(calls.len(), case.requests, "{}: {done:?}", case.name);
        match (case.code, done) {
            (None, done) => assert!(done.is_ok(), "{}: {done:?}", case.name),
            (Some(c), Err(msg)) => {
                assert_eq!(named_code(&msg), Some(c), "{}: {msg}", case.name)
            }
            (Some(_), Ok(s)) => panic!("{}: refused expected, got {s}", case.name),
        }
    }
}

/// FK7 (KU §2.5 “GUI "Keep encrypted", GUI `raw_copy`”): a raw disc copy scans with
/// `raw_copy` and makes no key request; the drive half is covered by `rip_keys`.
#[test]
fn a_raw_gui_copy_asks_no_key_source() {
    assert_eq!(
        disc_copy_scope(OutKind::IsoImage, true),
        libfreemkv::keys::KeyScope::None
    );
    let whole = libfreemkv::keys::KeyScope::WholeDisc;
    assert_eq!(disc_copy_scope(OutKind::IsoImage, false), whole);
    assert_eq!(disc_copy_scope(OutKind::DecryptedFolder, true), whole);
}
