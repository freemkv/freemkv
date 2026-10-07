//! Start reuses what Open left open (the server's "Reusing drive session"): no second
//! open or scan, and no second resolve when Open's set covers the rip. A changed disc,
//! other key settings, another source or a raw copy open afresh, after the held handle is
//! closed. The hold ends on another Open, Eject, the disc leaving, the open's token or the
//! lease. Every drive here is a `FakeTransport`; no test touches a real drive.
use super::*;
use crate::ku_fixtures::*;
use crate::rip_keys::with_sources;
use libfreemkv::keys::KeyScope;
use libfreemkv::scsi::{DataDirection, ScsiResult, ScsiTransport};
use libfreemkv::test_util::{FakeHandle, FakeMode, FakeTransport};
use std::sync::atomic::AtomicU8;

// A device path no machine has: a slip past the fakes fails instead of finding a drive.
const SRC: &str = "disc:///dev/fmkv-test-no-such-drive";

fn req(out: &std::path::Path) -> RipRequest {
    RipRequest {
        source: SRC.into(),
        dest_dir: out.display().to_string(),
        titles: Vec::new(),
        title_ids: Vec::new(),
        format: "Whole disc → ISO image".into(),
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

fn is_read(c: &[u8]) -> bool {
    matches!(c[0], 0x28 | 0xA8)
}

// SS-6 MMC-6 Table 633: LoEj 1, Start 0 = "Eject the disc if permitted".
fn is_eject(c: &[u8]) -> bool {
    c[0] == 0x1B && c[4] & 0x03 == 0x02
}

fn lines(st: &Arc<RunState>) -> Vec<String> {
    st.lines.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// A drive serving `fx`'s image, with its scan: Open's half, before the key call.
fn scanned(fx: &Fx) -> (libfreemkv::DiscSession, libfreemkv::Disc, FakeHandle) {
    let (t, fake) = FakeTransport::new();
    let drive = libfreemkv::Drive::from_transport(Box::new(t.with_image(fx.img.image.clone())));
    let mut s = libfreemkv::DiscSession::from_drive(drive);
    s.scan(libfreemkv::ScanOptions::default())
        .expect("the fake drive scans");
    let disc = s.take_disc().expect("a disc");
    (s, disc, fake)
}

// Open over a fake drive: the scan, the main title's resolve, and the hold it leaves.
fn open(fx: &Fx, calls: &Calls) -> (HeldDrive, KeySet, FakeHandle) {
    open_with(fx, Answer::Keydb, calls)
}

// [`open`] with the key sources answering `answer`.
fn open_with(fx: &Fx, answer: Answer, calls: &Calls) -> (HeldDrive, KeySet, FakeHandle) {
    let (mut s, disc, fake) = scanned(fx);
    let f = factory(&[(answer, &[K1, K2])], calls);
    let main = fe::resolve_selection(&disc, &fe::Selection::MainMovie);
    let (set, _) = crate::rip_keys::resolve(
        &disc,
        s.source_mut().unwrap(),
        KeyScope::Titles(main),
        &f,
        None,
        None,
    );
    let set = set.expect("Open's set");
    let open = libfreemkv::Halt::new();
    let held = held_drive(
        SRC,
        s,
        disc,
        Some(set.clone()),
        &KeyConfig::default(),
        &open,
    );
    (held.expect("a held drive"), set, fake)
}

// A fresh `fe::open_scan` stand-in over another fake drive serving `fx`.
type OpenScan = Box<
    dyn FnOnce(
        libfreemkv::DeviceTarget,
        Option<libfreemkv::DriveCredentials>,
        bool,
    ) -> Result<libfreemkv::DiscSession, libfreemkv::Error>,
>;

fn fresh(fx: &Fx) -> (OpenScan, FakeHandle) {
    let (t, fake) = FakeTransport::new();
    let t = t.with_image(fx.img.image.clone());
    let scan: OpenScan = Box::new(move |_, _, raw| {
        let mut s =
            libfreemkv::DiscSession::from_drive(libfreemkv::Drive::from_transport(Box::new(t)));
        s.scan(libfreemkv::ScanOptions {
            raw_copy: raw,
            ..Default::default()
        })?;
        Ok(s)
    });
    (scan, fake)
}

/// Start over the drive Open holds: no open, no scan, the same verdict and requests as a
/// fresh open, strictly fewer reads; the rip closes the held handle when it is done.
#[test]
fn start_reuses_the_drive_and_scan_open_left() {
    let fx = bd_image(&[Some(K1)], 1);
    let calls = Calls::default();
    let (held, set, fake) = open(&fx, &calls);
    assert_eq!(calls.len(), 1, "Open resolves the main title");
    assert!(!fake.tray_locked(), "the held drive's button still ejects");
    let open_reads = fake.count(is_read);

    let dir = TempDir::new("held-reuse");
    let mut r = req(dir.path());
    r.seed = Some(set);
    let st = Arc::new(RunState::default());
    let mut opens = 0;
    let f = factory(&[(Answer::Keydb, &[K1])], &calls);
    let res = with_sources(f, || {
        run_disc_scanning(&r, &UiSink(st.clone()), &st, Some(held), |_, _, _| {
            opens += 1;
            Err(libfreemkv::Error::DeviceNotFound {
                path: String::new(),
            })
        })
    });
    let held_lines = lines(&st);
    assert!(res.is_ok(), "{res:?} {held_lines:?}");
    assert_eq!(opens, 0, "Start opens nothing");
    assert!(
        held_lines
            .iter()
            .any(|l| l == "reusing the drive and scan from Open")
    );
    assert_eq!(calls.len(), 1, "the seeded resolve asks nothing more");
    assert_eq!(fake.live_handles(), 0, "the rip ends the held handle");
    let held_reads = fake.count(is_read) - open_reads;
    let iso = |d: &std::path::Path| {
        files_under(d)
            .iter()
            .any(|p| p.extension() == Some("iso".as_ref()))
    };
    assert!(iso(dir.path()), "the image");

    // The same Start with nothing held: one open and its scan, the same verdict.
    let dir = TempDir::new("held-fresh");
    let mut r = req(dir.path());
    r.seed = Some(open(&fx, &Calls::default()).1);
    let st = Arc::new(RunState::default());
    let (scan, fresh_fake) = fresh(&fx);
    let fresh_calls = Calls::default();
    let f = factory(&[(Answer::Keydb, &[K1])], &fresh_calls);
    let res = with_sources(f, || {
        run_disc_scanning(&r, &UiSink(st.clone()), &st, None, scan)
    });
    assert!(res.is_ok(), "{res:?} {:?}", lines(&st));
    assert_eq!(fresh_calls.len(), 0, "the same requests");
    assert!(iso(dir.path()));
    assert!(
        held_reads < fresh_fake.count(is_read),
        "fewer reads: {held_reads} vs {}",
        fresh_fake.count(is_read)
    );
}

/// A Start Open's set covers (the main title) makes no second resolve over the held disc;
/// one it does not cover still resolves once, seeded, asking only for what Open lacked.
#[test]
fn opens_set_is_the_rips_when_it_covers_the_rip() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let calls = Calls::default();
    let (held, set, _fake) = open(&fx, &calls);
    assert_eq!(calls.len(), 1, "Open resolves the main title's group");
    let dir = TempDir::new("held-covers");
    let mut r = req(dir.path());
    r.format = "MKV".into();
    r.multipass = true;
    r.titles = vec![0];
    r.seed = Some(set.clone());
    let st = Arc::new(RunState::default());
    let builds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inner = factory(&[(Answer::Keydb, &[K1, K2])], &calls);
    let counted = builds.clone();
    let f: libfreemkv::KeySourceFactory = Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        inner()
    });
    let res = with_sources(f, || {
        run_disc_scanning(&r, &UiSink(st.clone()), &st, Some(held), |_, _, _| {
            panic!("Start opened the drive")
        })
    });
    let said = lines(&st);
    assert!(res.is_ok(), "{res:?} {said:?}");
    let covered = "keys: the set Open resolved covers this rip; no second resolve";
    assert!(said.iter().any(|l| l == covered), "{said:?}");
    assert_eq!(
        builds.load(Ordering::SeqCst),
        0,
        "no resolve built a source"
    );
    assert_eq!(calls.len(), 1, "no request at Start");
    assert!(
        files_under(dir.path())
            .iter()
            .any(|p| p.extension() == Some("mkv".as_ref()))
    );

    // The whole disc needs the group Open did not resolve: one seeded resolve, one request.
    let calls = Calls::default();
    let (held, set, _fake) = open_with(&fx, Answer::Online, &calls);
    assert_eq!(calls.len(), 1);
    let dir = TempDir::new("held-tops-up");
    let mut r = req(dir.path());
    r.seed = Some(set);
    let st = Arc::new(RunState::default());
    let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
    let res = with_sources(f, || {
        run_disc_scanning(&r, &UiSink(st.clone()), &st, Some(held), |_, _, _| {
            panic!("Start opened the drive")
        })
    });
    assert!(res.is_ok(), "{res:?} {:?}", lines(&st));
    assert!(!lines(&st).iter().any(|l| l == covered));
    assert_eq!(calls.len(), 2, "Start asks only for the group Open lacked");
}

/// The server's `keys_cover`: the same disc, the rip's scope, an AACS set for AACS titles.
#[test]
fn open_keys_cover_needs_this_disc_and_the_rips_scope() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let main = resolve(
        &fx,
        KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .expect("the main title's set");
    assert!(open_keys_cover(&fx.disc, &main, &KeyScope::Titles(vec![0])));
    assert!(!open_keys_cover(
        &fx.disc,
        &main,
        &KeyScope::Titles(vec![0, 1])
    ));
    assert!(!open_keys_cover(&fx.disc, &main, &KeyScope::WholeDisc));
    let other = bd_image(&[Some(K2)], 1);
    assert!(
        !open_keys_cover(&other.disc, &main, &KeyScope::Titles(vec![0])),
        "another disc"
    );
    let none = libfreemkv::keys::KeyRing::none();
    assert!(
        !open_keys_cover(&fx.disc, &none, &KeyScope::Titles(vec![0])),
        "no AACS key for AACS titles"
    );
}

/// Each way the hold does not fit Start opens the drive afresh, after closing the held
/// handle (one open of a drive at a time on macOS).
#[test]
fn start_opens_afresh_when_the_hold_does_not_fit() {
    let fx = bd_image(&[Some(K1)], 1);
    type Case = (&'static str, fn(&mut HeldDrive, &mut RipRequest));
    let cases: [Case; 4] = [
        ("another source", |h, _| {
            h.source = "disc:///dev/fmkv-other".into()
        }),
        ("other key settings", |_, r| {
            r.keys.keydb_path = "/x/keydb.cfg".into()
        }),
        ("a raw copy", |_, r| r.raw = true),
        ("a different disc", |h, _| {
            h.disc = rescan(&bd_image(&[Some(K1), Some(K2)], 2))
        }),
    ];
    for (what, change) in cases {
        let (mut held, _, fake) = open(&fx, &Calls::default());
        let dir = TempDir::new("held-misfit");
        let mut r = req(dir.path());
        change(&mut held, &mut r);
        let st = Arc::new(RunState::default());
        let mut opens = 0;
        let res = run_disc_scanning(&r, &UiSink(st.clone()), &st, Some(held), |_, _, _| {
            assert_eq!(fake.live_handles(), 0, "{what}: closed before the open");
            opens += 1;
            Err(libfreemkv::Error::DeviceNotFound {
                path: String::new(),
            })
        });
        assert!(res.is_err(), "{what}");
        assert_eq!(opens, 1, "{what}");
        let changed = lines(&st)
            .iter()
            .any(|l| l == "the disc changed since it was opened; scanning it again");
        assert_eq!(changed, what == "a different disc", "{what}");
    }
}

/// `same_disc`: present, the same volume id and capacity. A drive spinning up, or one
/// holding another disc, is not Open's.
#[test]
fn same_disc_checks_presence_and_identity() {
    let fx = bd_image(&[Some(K1)], 1);
    let (s, disc, _fake) = scanned(&fx);
    let mut drive = s.into_drive().unwrap();
    assert!(same_disc(&mut drive, &disc));
    let bigger = rescan(&bd_image(&[Some(K1), Some(K2)], 2));
    assert!(!same_disc(&mut drive, &bigger), "another capacity");
    let mut renamed = rescan(&fx);
    renamed.volume_id = "SOMETHING_ELSE".into();
    assert!(!same_disc(&mut drive, &renamed), "another volume id");

    // MMC-6 TEST UNIT READY: NOT READY, becoming ready.
    let not_ready = libfreemkv::scsi::ScsiSense {
        sense_key: 0x02,
        asc: 0x04,
        ascq: 0x01,
    };
    let (t, _fake) = FakeTransport::new();
    let t = t.with_image(fx.img.image.clone()).rule(
        |c| c[0] == 0x00,
        FakeMode::Sense {
            sense: not_ready,
            progress: None,
        },
    );
    let mut drive = libfreemkv::Drive::from_transport(Box::new(t));
    assert!(!same_disc(&mut drive, &disc), "spinning up");
}

// A drive answering GET EVENT STATUS NOTIFICATION with a media event: byte 5 is the media
// status (MMC-6 §6.7; bit 1 media present, bit 0 tray open), changed by the test.
struct Media(Arc<AtomicU8>);

impl ScsiTransport for Media {
    fn execute(
        &mut self,
        cdb: &[u8],
        _: DataDirection,
        data: &mut [u8],
        _: u32,
    ) -> libfreemkv::Result<ScsiResult> {
        data.fill(0);
        if cdb[0] == 0x4A && data.len() >= 8 {
            data[1] = 6;
            data[2] = 0x04;
            data[5] = self.0.load(Ordering::SeqCst);
        }
        Ok(ScsiResult {
            status: 0,
            bytes_transferred: data.len(),
            sense: [0; 32],
        })
    }
}

const PRESENT: u8 = 0x02;
const TRAY_OPEN: u8 = 0x01;

// A hold on `source` over a `Media` drive, with Open's token.
fn media_hold(source: &str) -> (HeldDrive, Arc<AtomicU8>, FakeHandle, libfreemkv::Halt) {
    let status = Arc::new(AtomicU8::new(PRESENT));
    let (t, fake) = FakeTransport::new();
    let t = t.with_inner(Box::new(Media(status.clone())));
    let drive = libfreemkv::Drive::from_transport(Box::new(t));
    let open = libfreemkv::Halt::new();
    let h = HeldDrive {
        serial: 0,
        source: source.into(),
        drive,
        disc: rescan(&bd_image(&[Some(K1)], 1)),
        keys: None,
        config: KeyConfig::default(),
        open: open.clone(),
        renewed: std::time::Instant::now(),
    };
    (h, status, fake, open)
}

/// The idle disc watch asks the held drive, never the registry (macOS hides held media);
/// the disc leaving releases it, and right after a release the answer is "unknown".
#[test]
fn presence_asks_the_held_drive_and_the_disc_leaving_releases_it() {
    let hold = DriveHold::new();
    let (h, status, fake, _open) = media_hold(SRC);
    let serial = hold.hold(h);
    let t0 = std::time::Instant::now();
    let never = |_: &str| -> Option<bool> { panic!("asked the registry while held") };
    assert_eq!(disc_present_with(&hold, SRC, never), Some(true));
    // A present disc renews the lease.
    let later = t0 + HOLD_LEASE / 2;
    assert_eq!(hold.presence(SRC, later), Some(Some(true)));
    assert!(hold.expire(serial, later + HOLD_LEASE / 2 + HOLD_REAP_EVERY));

    status.store(TRAY_OPEN, Ordering::SeqCst);
    assert_eq!(disc_present_with(&hold, SRC, never), Some(false));
    assert_eq!(fake.live_handles(), 0, "the disc left: released");
    assert!(!hold.expire(serial, later), "nothing left to watch");
    // macOS republishes the media after a release: not "gone", not the registry yet.
    assert_eq!(hold.presence(SRC, std::time::Instant::now()), Some(None));
    let settled = std::time::Instant::now() + HOLD_SETTLE;
    assert_eq!(hold.presence(SRC, settled), None);
    assert_eq!(
        disc_present_with(&hold, "disc:///dev/other", |_| Some(false)),
        Some(false)
    );

    // A check for another source releases a hold left on this one.
    let (h, _, fake, _open) = media_hold(SRC);
    hold.hold(h);
    assert_eq!(
        disc_present_with(&hold, "disc:///dev/other", |_| Some(true)),
        Some(true)
    );
    assert_eq!(fake.live_handles(), 0);
}

/// Close leaves the hold to its open's token (cancelled on Close, another Open, Quit) or to
/// the lease the idle disc watch stops renewing; a newer hold replaces an older one.
#[test]
fn the_hold_ends_on_its_token_the_lease_or_a_newer_hold() {
    let hold = DriveHold::new();
    let (h, _, fake, open) = media_hold(SRC);
    let t0 = h.renewed;
    let serial = hold.hold(h);
    assert!(hold.expire(serial, t0 + HOLD_LEASE / 2), "inside the lease");
    open.cancel();
    assert!(!hold.expire(serial, t0), "Close cancelled the open");
    assert_eq!(fake.live_handles(), 0);

    let (h, _, fake, _open) = media_hold(SRC);
    let t0 = h.renewed;
    let serial = hold.hold(h);
    assert!(
        !hold.expire(serial, t0 + HOLD_LEASE),
        "the watch stopped renewing"
    );
    assert_eq!(fake.live_handles(), 0);

    let (h, _, older, _open) = media_hold(SRC);
    let old = hold.hold(h);
    let (h, _, newer, _open) = media_hold(SRC);
    let new = hold.hold(h);
    assert_eq!(older.live_handles(), 0, "a newer Open releases the older");
    assert!(
        !hold.expire(old, std::time::Instant::now()),
        "its reaper stops"
    );
    assert!(hold.expire(new, std::time::Instant::now()));
    hold.release();
    assert_eq!(newer.live_handles(), 0);

    // An open already cancelled when it finished holds nothing.
    let leaked: &'static DriveHold = Box::leak(Box::new(DriveHold::new()));
    let (h, _, fake, open) = media_hold(SRC);
    open.cancel();
    hold_for_start(leaked, Some(h));
    assert!(leaked.take().is_none());
    assert_eq!(fake.live_handles(), 0);
    // A live one is held, and its reaper releases it once the open is cancelled.
    let (h, _, fake, open) = media_hold(SRC);
    hold_for_start(leaked, Some(h));
    assert_eq!(fake.live_handles(), 1, "held");
    open.cancel();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while fake.live_handles() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(HOLD_REAP_EVERY / 5);
    }
    assert_eq!(fake.live_handles(), 0, "the reaper released it");
}

/// Eject ends the held handle through `finish(Eject)`, never a second open; a hold on
/// another source is released before that source's own open.
#[test]
fn eject_uses_the_held_handle() {
    let hold = DriveHold::new();
    let (h, _, fake, _open) = media_hold(SRC);
    hold.hold(h);
    let res = eject_held_or(&hold, SRC, |_| Err("opened the drive again".into()));
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(fake.count(is_eject), 1);
    assert_eq!(fake.live_handles(), 0);
    assert!(hold.take().is_none());

    let (h, _, held, _open) = media_hold("disc:///dev/other");
    hold.hold(h);
    let (t, fresh) = FakeTransport::new();
    let mut opens = 0;
    let res = eject_held_or(&hold, SRC, |_| {
        opens += 1;
        assert_eq!(held.live_handles(), 0, "released before the open");
        Ok(libfreemkv::Drive::from_transport(Box::new(t)))
    });
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(opens, 1);
    assert_eq!(fresh.count(is_eject), 1);
}

/// Open needs a live drive, so this reads the wiring: another Open releases the hold before
/// it opens anything, the disc Open scans is held after its resolve, Start takes the hold,
/// and the insert-the-disc Retry releases it before its drive scan.
#[test]
fn opens_hold_and_release_the_drive_in_order() {
    let src = include_str!("engine.rs").replace("\r\n", "\n");
    let body = |name: &str| {
        let a = src.find(name).expect(name);
        src[a..a + src[a..].find("\n}\n").unwrap()].to_string()
    };
    let open = body("\npub fn scan_disc_with_keys(");
    let at = |s: &str| open.find(s).unwrap_or_else(|| panic!("{s}: {open}"));
    assert!(at("HOLD.release();") < at("drive_scan(source, keys, tok)"));
    assert!(at("resolve_observed(") < at("hold_for_start(&HOLD, held)"));
    for name in [
        "\npub fn scan_with_keys_under(",
        "\npub fn scan_stream_under(",
    ] {
        assert!(body(name).contains("HOLD.release();"), "{name}");
    }
    assert!(body("\nfn run_disc(").contains("HOLD.take()"));
    let retry = body("\nfn open_rip_image(");
    let release = retry.find("HOLD.release();").expect("released");
    assert!(release < retry.find("crate::rip_keys::drive_scan(").unwrap());
    assert!(body("\npub fn eject_source(").contains("eject_held_or(&HOLD,"));
    assert!(body("\npub fn disc_present(").contains("disc_present_with(&HOLD,"));
}

/// Close releases the held drive at once (macOS allows one open of a drive), not at the
/// idle timeout: `close_source` calls the release.
#[test]
fn close_releases_the_held_drive() {
    let ui = include_str!("ui.rs").replace("\r\n", "\n");
    let at = ui.find("fn close_source(&mut self)").expect("close_source");
    let body = &ui[at..at + ui[at..].find("\n    }\n").expect("end")];
    assert!(
        body.contains("crate::engine::release_held_drive();"),
        "{body}"
    );
}
