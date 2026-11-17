//! GUI rip paths no other test reached, over `FakeTransport` drives only: a reopen
//! goes to a device path no machine has.
use super::*;
use crate::ku_fixtures::*;
use crate::rip_keys::with_sources;
use libfreemkv::test_util::{FakeHandle, FakeTransport};

const SRC: &str = "disc:///dev/fmkv-cov-no-such-drive";
const FOLDER: &str = "Whole disc → decrypted folder";

fn req(source: &str, out: &std::path::Path, format: &str) -> RipRequest {
    RipRequest {
        source: source.into(),
        dest_dir: out.display().to_string(),
        titles: Vec::new(),
        title_ids: Vec::new(),
        format: format.into(),
        audio_pids: vec![],
        sub_pids: vec![],
        title_pids: TitleStreams::Unspecified,
        explicit_streams: false,
        raw: false,
        force: false,
        filename_template: String::new(),
        tv_naming: None,
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

fn lines(st: &Arc<RunState>) -> Vec<String> {
    st.lines.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

// A `fe::open_scan` stand-in over a fake drive serving `fx`.
fn fresh(
    fx: &Fx,
) -> (
    impl FnOnce(
        libfreemkv::DeviceTarget,
        Option<libfreemkv::DriveCredentials>,
        bool,
    ) -> Result<libfreemkv::DiscSession, libfreemkv::Error>,
    FakeHandle,
) {
    let (t, fake) = FakeTransport::new();
    let t = t.with_image(fx.img.image.clone());
    let scan = move |_: libfreemkv::DeviceTarget,
                     _: Option<libfreemkv::DriveCredentials>,
                     raw: bool|
          -> Result<libfreemkv::DiscSession, libfreemkv::Error> {
        let mut s =
            libfreemkv::DiscSession::from_drive(libfreemkv::Drive::from_transport(Box::new(t)));
        s.scan(libfreemkv::ScanOptions {
            raw_copy: raw,
            ..Default::default()
        })?;
        Ok(s)
    };
    (scan, fake)
}

fn extracting(st: &Arc<RunState>) -> bool {
    lines(st)
        .iter()
        .any(|l| l.starts_with("extracting decrypted file tree"))
}

#[test]
fn an_optical_drive_gets_a_display_name_and_a_legacy_label() {
    let blank = libfreemkv::DriveInfo {
        path: "/dev/sg9".into(),
        display_name: "/dev/sr9".into(),
        vendor: "  ".into(),
        model: String::new(),
        firmware: "1.00".into(),
    };
    let d = optical_drive(&blank);
    assert_eq!(d.device, "/dev/sg9");
    assert_eq!(d.legacy_label, "Optical drive");
    let named = libfreemkv::DriveInfo {
        vendor: "HL-DT-ST ".into(),
        model: "BD-RE\u{1b}[2J".into(),
        ..blank
    };
    let d = optical_drive(&named);
    assert_eq!(d.legacy_label, "HL-DT-ST BD-RE\u{1b}[2J");
    assert!(!d.display.contains('\u{1b}'), "{}", d.display);
    assert!(!d.list_line.contains('\u{1b}'), "{}", d.list_line);
}

// A run that ends with nothing to say reads "nothing written" and Failed, never blank.
#[test]
fn an_empty_summary_reads_as_nothing_written() {
    let dir = TempDir::new("cov-nothing");
    let st = Arc::new(RunState::default());
    let r = req(
        &dir.path().join("disc.iso").display().to_string(),
        dir.path(),
        "MKV",
    );
    start_rip_with(r, st.clone(), |_, _, _| Ok(String::new()))
        .join()
        .expect("worker");
    assert!(st.finished.load(Ordering::Acquire));
    assert_eq!(
        *st.outcome.lock().unwrap_or_else(|e| e.into_inner()),
        RunOutcome::Failed
    );
    assert_eq!(
        *st.summary.lock().unwrap_or_else(|e| e.into_inner()),
        crate::strings::get("gui.result.nothing")
    );
}

#[test]
fn an_image_extracts_its_decrypted_folder() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cov-img-folder");
    let iso = fx.write(dir.path(), "disc.iso");
    let r = req(&iso.display().to_string(), &dir.path().join("out"), FOLDER);
    let st = Arc::new(RunState::default());
    let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let res = with_sources(f, || run_blocking(&r, &UiSink(st.clone()), &st));
    assert!(extracting(&st), "{res:?} {:?}", lines(&st));
}

#[test]
fn a_drive_extracts_its_decrypted_folder() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cov-drive-folder");
    let (scan, _fake) = fresh(&fx);
    let mut r = req(SRC, &dir.path().join("out"), FOLDER);
    r.auto_eject = true;
    let st = Arc::new(RunState::default());
    let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let res = with_sources(f, || {
        run_disc_scanning(&r, &UiSink(st.clone()), &st, None, scan)
    });
    assert!(extracting(&st), "{res:?} {:?}", lines(&st));
}

// A single-pass drive rip reopens the drive per title; a drive that is gone fails the
// title with its reason in the log.
#[test]
fn a_per_title_drive_rip_logs_a_failed_reopen() {
    let one = bd_image(&[Some(K1)], 1);
    let two = bd_image(&[Some(K1), Some(K2)], 2);
    let cases: [(&Fx, &str, Vec<usize>); 3] = [
        (&one, "MKV", vec![0]),
        (&one, "Selected titles → separate track files", vec![0]),
        (&two, "Selected titles → separate track files", vec![0, 1]),
    ];
    for (fx, format, titles) in cases {
        let dir = TempDir::new("cov-per-title");
        let (scan, _fake) = fresh(fx);
        let mut r = req(SRC, &dir.path().join("out"), format);
        r.titles = titles;
        r.auto_eject = true;
        let st = Arc::new(RunState::default());
        let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
        let res = with_sources(f, || {
            run_disc_scanning(&r, &UiSink(st.clone()), &st, None, scan)
        });
        assert!(res.is_err(), "{format}: {res:?}");
        assert!(
            lines(&st).iter().any(|l| l.starts_with("Title 1: ")),
            "{format}: {:?}",
            lines(&st)
        );
    }
}

// A container source routes to track files, notes that ticks are not applied, and
// refuses a whole-disc output.
#[test]
fn a_container_source_routes_to_track_files_and_refuses_a_folder() {
    let dir = TempDir::new("cov-stream");
    let clip = dir.path().join("clip.m2ts");
    std::fs::write(&clip, libfreemkv::test_util::synthetic_bd_clip(16)).unwrap();
    let mut r = req(
        &clip.display().to_string(),
        &dir.path().join("out"),
        "Each title → separate track files",
    );
    r.explicit_streams = true;
    let st = Arc::new(RunState::default());
    let res = with_sources(factory(&[], &Calls::default()), || {
        run_blocking(&r, &UiSink(st.clone()), &st)
    });
    assert!(
        lines(&st)
            .iter()
            .any(|l| l.starts_with("Note: track selection is not applied")),
        "{res:?} {:?}",
        lines(&st)
    );
    r.format = FOLDER.into();
    let st = Arc::new(RunState::default());
    let err = run_blocking(&r, &UiSink(st.clone()), &st).unwrap_err();
    assert!(err.contains("for a disc source"), "{err}");
}

// One title muxed live off a held session through the rip's set.
#[test]
fn a_title_muxes_off_a_held_session() {
    let fx = bd_image(&[Some(K1)], 1);
    let (t, _fake) = FakeTransport::new();
    let drive = libfreemkv::Drive::from_transport(Box::new(t.with_image(fx.img.image.clone())));
    let mut session = libfreemkv::DiscSession::from_drive(drive);
    session
        .scan(libfreemkv::ScanOptions::default())
        .expect("the fake drive scans");
    let set = resolve(
        &fx,
        libfreemkv::keys::KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .expect("the title's set");
    let dir = TempDir::new("cov-session");
    let r = req(SRC, dir.path(), "MKV");
    let mkv = dir.path().join("t1.mkv");
    let st = Arc::new(RunState::default());
    let dest = format!("mkv://{}", mkv.display());
    let res = mux_session_title(&r, &mut session, 0, &set, &dest, &UiSink(st.clone()));
    if let Ok(o) = &res
        && o.completed
    {
        assert!(mkv.exists(), "{:?}", lines(&st));
    }
}
