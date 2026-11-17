//! The GUI's per-title drive loop past its reopen, over fake drives: `TEST_REOPEN` answers
//! each title's reopen once the real one (a device path no machine has) fails.
use super::*;
use crate::ku_fixtures::*;
use crate::rip_keys::with_sources;
use libfreemkv::test_util::FakeTransport;
use std::cell::Cell;
use std::rc::Rc;

const SRC: &str = "disc:///dev/fmkv-cov2-no-such-drive";
const TRACKS: &str = "Selected titles → separate track files";

fn req(out: &std::path::Path, format: &str, titles: Vec<usize>) -> RipRequest {
    RipRequest {
        source: SRC.into(),
        dest_dir: out.display().to_string(),
        titles,
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

// A drive serving `image`, scanned with no key call.
fn session(image: Vec<u8>, raw: bool) -> Result<libfreemkv::DiscSession, libfreemkv::Error> {
    let (t, _fake) = FakeTransport::new();
    let drive = libfreemkv::Drive::from_transport(Box::new(t.with_image(image)));
    let mut s = libfreemkv::DiscSession::from_drive(drive);
    s.scan(libfreemkv::ScanOptions {
        raw_copy: raw,
        ..Default::default()
    })?;
    Ok(s)
}

// Run the drive rip `r` over `first`, each title's reopen answered by `reopen`; the result,
// the run's log and how many reopens were asked.
fn rip(
    r: &RipRequest,
    first: &Fx,
    reopen: Vec<u8>,
) -> (Result<String, String>, Vec<String>, usize) {
    let st = Arc::new(RunState::default());
    let asked = Rc::new(Cell::new(0usize));
    let counted = asked.clone();
    let open = move || {
        counted.set(counted.get() + 1);
        session(reopen.clone(), false)
    };
    TEST_REOPEN.with(|t| *t.borrow_mut() = Some(Box::new(open)));
    let image = first.img.image.clone();
    let scan = move |_: libfreemkv::DeviceTarget,
                     _: Option<libfreemkv::DriveCredentials>,
                     raw: bool| session(image, raw);
    let f = factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default());
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_sources(f, || {
            run_disc_scanning(r, &UiSink(st.clone()), &st, None, scan)
        })
    }));
    TEST_REOPEN.with(|t| *t.borrow_mut() = None);
    let res = res.unwrap_or_else(|p| std::panic::resume_unwind(p));
    (res, lines(&st), asked.get())
}

// `fx`'s image with one byte of its title-key file changed: the same layout, another disc.
fn other_disc(fx: &Fx) -> Vec<u8> {
    let mut image = fx.img.image.clone();
    let (lba, _) = *fx.metadata.last().expect("the title-key file");
    let at = lba as usize * 2048;
    let key = image[at..at + 2048]
        .windows(16)
        .position(|w| w.iter().all(|&b| b == 0xEE))
        .expect("an encrypted unit key");
    image[at + key] ^= 0xFF;
    image
}

// The reopened drive holds the same disc: the title muxes (or fails) under its own number.
#[test]
fn a_reopened_title_muxes_under_its_number() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cov2-same");
    let r = req(&dir.path().join("out"), "MKV", vec![0]);
    let (res, log, asked) = rip(&r, &fx, fx.img.image.clone());
    assert_eq!(asked, 1, "{res:?} {log:?}");
    assert!(
        log.iter()
            .any(|l| l.starts_with("title 1 ") || l.starts_with("Title 1: ")),
        "{res:?} {log:?}"
    );
}

// Two titles to track files, each reopened into its own subfolder.
#[test]
fn reopened_titles_demux_one_after_another() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = TempDir::new("cov2-demux");
    let r = req(&dir.path().join("out"), TRACKS, vec![0, 1]);
    let (res, log, asked) = rip(&r, &fx, fx.img.image.clone());
    assert!(asked >= 1, "{res:?} {log:?}");
    assert!(
        log.iter()
            .any(|l| l.starts_with("title 1 ") || l.starts_with("Title 1: ")),
        "{res:?} {log:?}"
    );
}

// A reopen that lists fewer titles refuses the missing one by name, and writes nothing.
#[test]
fn a_reopen_missing_the_picked_title_says_so() {
    let two = bd_image(&[Some(K1), Some(K2)], 2);
    let one = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cov2-fewer");
    let r = req(&dir.path().join("out"), "MKV", vec![1]);
    let (res, log, asked) = rip(&r, &two, one.img.image.clone());
    assert!(res.is_err(), "{log:?}");
    assert_eq!(asked, 1);
    assert!(
        log.iter()
            .any(|l| l.starts_with("Title 2 ") && l.contains("no longer on the disc")),
        "{log:?}"
    );
}

// A reopen that finds another disc refuses the rip's keys for it before any output.
#[test]
fn a_reopen_of_another_disc_is_refused() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cov2-other");
    let out = dir.path().join("out");
    let r = req(&out, "MKV", vec![0]);
    let (res, log, asked) = rip(&r, &fx, other_disc(&fx));
    assert!(res.is_err(), "{log:?}");
    assert_eq!(asked, 1);
    assert!(log.iter().any(|l| l.starts_with("Title 1: ")), "{log:?}");
    assert!(files_under(&out).is_empty(), "{:?}", files_under(&out));
}

#[test]
fn every_disc_format_has_a_name() {
    use libfreemkv::DiscFormat as F;
    assert_eq!(format_name(&F::Fmts), "4K UHD (AACS 2.1 FMTS)");
    assert_eq!(format_name(&F::BluRay), "Blu-ray");
    assert_eq!(format_name(&F::HdDvd), "HD-DVD");
    assert_eq!(format_name(&F::Dvd), "DVD");
    assert_eq!(
        format_name(&F::Unknown),
        crate::strings::get("disc.format_unknown")
    );
}
