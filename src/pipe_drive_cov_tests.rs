//! The CLI's drive rips over fake drives: the up-front scan, the rip's one resolve and
//! each title's reopen, with no hardware (`TEST_DRIVE` answers every drive open).
use super::*;
use crate::ku_fixtures::*;
use crate::rip_keys::with_sources;
use std::cell::Cell;
use std::rc::Rc;

// Autodetect: an explicitly named device must exist before any open, so tests name none.
const SRC: &str = "disc://";
const MISSING: &str = "disc:///dev/fmkv-cov-no-such-drive";

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn run_cli(source: &str, dest: &str, a: &[&str]) -> (i32, String) {
    crate::output::capture(|| super::run(source, dest, &args(a)))
}

// A drive serving `image`, scanned the way `open_scan` scans it.
fn fake_drive(image: Vec<u8>, raw: bool) -> Result<libfreemkv::DiscSession, libfreemkv::Error> {
    let (t, _fake) = libfreemkv::test_util::FakeTransport::new();
    let drive = libfreemkv::Drive::from_transport(Box::new(t.with_image(image)));
    let mut s = libfreemkv::DiscSession::from_drive(drive);
    s.scan(libfreemkv::ScanOptions {
        raw_copy: raw,
        ..Default::default()
    })?;
    Ok(s)
}

// Run `f` with every drive open on this thread answered by `open`.
#[allow(clippy::type_complexity)]
fn with_drive<T>(
    open: impl FnMut(bool) -> Result<libfreemkv::DiscSession, libfreemkv::Error> + 'static,
    f: impl FnOnce() -> T,
) -> T {
    TEST_DRIVE.with(|t| *t.borrow_mut() = Some(Box::new(open)));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    TEST_DRIVE.with(|t| *t.borrow_mut() = None);
    out.unwrap_or_else(|p| std::panic::resume_unwind(p))
}

// A drive that serves the first image on its first open and the second on every later one,
// counting the opens.
#[allow(clippy::type_complexity)]
fn swapping(
    first: Vec<u8>,
    later: Vec<u8>,
    opens: &Rc<Cell<usize>>,
) -> impl FnMut(bool) -> Result<libfreemkv::DiscSession, libfreemkv::Error> + 'static {
    let opens = opens.clone();
    move |raw| {
        let n = opens.get();
        opens.set(n + 1);
        fake_drive(if n == 0 { first.clone() } else { later.clone() }, raw)
    }
}

// `fx`'s image with one byte of its title-key file changed: the same layout, another disc.
fn other_disc(fx: &Fx) -> Vec<u8> {
    let mut image = fx.img.image.clone();
    let (lba, _) = *fx.metadata.last().expect("the title-key file");
    let at = lba as usize * 2048;
    let sector = &image[at..at + 2048];
    let key = sector
        .windows(16)
        .position(|w| w.iter().all(|&b| b == 0xEE))
        .expect("an encrypted unit key");
    image[at + key] ^= 0xFF;
    image
}

fn keyed() -> libfreemkv::KeySourceFactory {
    factory(&[(Answer::Keydb, &[K1, K2])], &Calls::default())
}

// A drive that will not open: autodetect finding none reads "no drive", and `-t all`
// says it could not list the titles. Nothing is written either way.
#[test]
fn a_drive_that_will_not_open_stops_before_any_output() {
    let dir = TempDir::new("pdc-noopen");
    let mkv = dir.path().join("out.mkv");
    let dest = format!("mkv://{}", mkv.display());
    let gone = |_: bool| -> Result<libfreemkv::DiscSession, libfreemkv::Error> {
        Err(libfreemkv::Error::DeviceNotFound {
            path: String::new(),
        })
    };
    let (code, text) = with_drive(gone, || run_cli(SRC, &dest, &["-t", "1"]));
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains(&crate::strings::get("error.no_drive")),
        "{text}"
    );
    let (code, text) = with_drive(gone, || run_cli(SRC, &dest, &["-t", "all"]));
    assert_eq!(code, 1, "{text}");
    assert!(text.contains(&disc_scan_all_titles_failed()), "{text}");
    assert!(!mkv.exists());
}

// The real open of a device path no machine has fails the rip with exit 1.
#[test]
fn a_missing_device_fails_the_drive_scan() {
    let dir = TempDir::new("pdc-missing");
    let dest = format!("mkv://{}", dir.path().join("out.mkv").display());
    let (code, text) = run_cli(MISSING, &dest, &["-t", "1"]);
    assert_eq!(code, 1, "{text}");
    let iso = format!("iso://{}", dir.path().join("out.iso").display());
    let (code, text) = run_cli(MISSING, &iso, &[]);
    assert_eq!(code, 1, "{text}");
}

// One title off a fake drive: scanned once, resolved once, then reopened for its mux.
#[test]
fn a_drive_title_is_scanned_resolved_and_reopened() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("pdc-one");
    let dest = format!("mkv://{}", dir.path().join("out.mkv").display());
    let opens = Rc::new(Cell::new(0));
    let open = swapping(fx.img.image.clone(), fx.img.image.clone(), &opens);
    let (_code, text) = with_sources(keyed(), || {
        with_drive(open, || run_cli(SRC, &dest, &["-t", "1"]))
    });
    assert_eq!(opens.get(), 2, "one scan and one reopen: {text}");
    assert!(text.contains(&crate::strings::get("rip.ok")), "{text}");
}

// Two named titles, every title, and the scan's main-title choice, beside one file name.
#[test]
fn drive_titles_are_each_reopened() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let cases: [&[&str]; 3] = [&["-t", "1", "-t", "2"], &["-t", "all"], &[]];
    for a in cases {
        let dir = TempDir::new("pdc-titles");
        let dest = format!("mkv://{}", dir.path().join("out.mkv").display());
        let opens = Rc::new(Cell::new(0));
        let open = swapping(fx.img.image.clone(), fx.img.image.clone(), &opens);
        let (_code, text) = with_sources(keyed(), || with_drive(open, || run_cli(SRC, &dest, a)));
        assert!(opens.get() >= 2, "{a:?}: {text}");
    }
}

// A disc with no key for its title stops after the resolve, before any reopen.
#[test]
fn a_drive_without_its_key_stops_after_the_resolve() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("pdc-nokey");
    let mkv = dir.path().join("out.mkv");
    let dest = format!("mkv://{}", mkv.display());
    let opens = Rc::new(Cell::new(0));
    let open = swapping(fx.img.image.clone(), fx.img.image.clone(), &opens);
    let (code, text) = with_sources(factory(&[], &Calls::default()), || {
        with_drive(open, || run_cli(SRC, &dest, &["-t", "1"]))
    });
    assert_eq!(code, 1, "{text}");
    assert_eq!(opens.get(), 1, "no reopen without keys: {text}");
    assert!(!mkv.exists());
}

// A reopen that finds another disc refuses the rip's keys for it; one that finds fewer
// titles refuses the missing one; one whose drive is gone fails the title.
#[test]
fn a_reopen_that_finds_another_disc_writes_nothing() {
    let one = bd_image(&[Some(K1)], 1);
    let two = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = TempDir::new("pdc-swap");
    let mkv = dir.path().join("out.mkv");
    let dest = format!("mkv://{}", mkv.display());

    let opens = Rc::new(Cell::new(0));
    let open = swapping(one.img.image.clone(), other_disc(&one), &opens);
    let (code, text) = with_sources(keyed(), || {
        with_drive(open, || run_cli(SRC, &dest, &["-t", "1"]))
    });
    assert_ne!(code, 0, "{text}");
    assert_eq!(opens.get(), 2, "{text}");
    assert!(!mkv.exists(), "{text}");

    let fewer = TempDir::new("pdc-fewer");
    let all = format!("mkv://{}", fewer.path().join("out.mkv").display());
    let opens = Rc::new(Cell::new(0));
    let open = swapping(two.img.image.clone(), one.img.image.clone(), &opens);
    let (_code, text) = with_sources(keyed(), || {
        with_drive(open, || run_cli(SRC, &all, &["-t", "all"]))
    });
    assert!(opens.get() >= 2, "{text}");

    let opens = Rc::new(Cell::new(0));
    let counted = opens.clone();
    let image = one.img.image.clone();
    let open = move |raw: bool| {
        let n = counted.get();
        counted.set(n + 1);
        if n == 0 {
            fake_drive(image.clone(), raw)
        } else {
            Err(libfreemkv::Error::DeviceNotFound {
                path: String::new(),
            })
        }
    };
    let (code, text) = with_sources(keyed(), || {
        with_drive(open, || run_cli(SRC, &dest, &["-t", "1"]))
    });
    assert_ne!(code, 0, "{text}");
    assert_eq!(opens.get(), 2, "{text}");
    assert!(!mkv.exists(), "{text}");
}

// `-a` on a drive title: a language the title has is kept, one it lacks is refused.
#[test]
fn audio_language_selection_runs_against_the_reopened_title() {
    let fx = bd_image(&[Some(K1)], 1);
    for lang in ["eng", "fra"] {
        let dir = TempDir::new("pdc-lang");
        let dest = format!("mkv://{}", dir.path().join("out.mkv").display());
        let opens = Rc::new(Cell::new(0));
        let open = swapping(fx.img.image.clone(), fx.img.image.clone(), &opens);
        let (_code, text) = with_sources(keyed(), || {
            with_drive(open, || run_cli(SRC, &dest, &["-t", "1", "-a", lang]))
        });
        assert_eq!(opens.get(), 2, "{lang}: {text}");
    }
}

// Flag and destination checks that need no drive.
#[test]
fn flag_and_destination_edges() {
    assert!(parse_flags(&args(&["--log-level", "0"])).is_err());
    assert!(parse_flags(&args(&["--log-file", "run.log"])).is_ok());
    assert!(matches!(
        parse_stream_spec(" , "),
        Ok(freemkv_engine::StreamFilter::All)
    ));
    assert!(validate_dir_dest(std::path::Path::new(""), "dir://", false).is_err());
    let dir = TempDir::new("pdc-edges");
    let file = dir.path().join("plain.txt");
    std::fs::write(&file, b"x").unwrap();
    assert!(validate_dir_input(&file.join("below")).is_err());
}

// A file the user cannot read or write is refused as an image source and as a destination
// (a root runner can do both; then both pass).
#[cfg(unix)]
#[test]
fn an_unreadable_file_is_refused_where_permissions_hold() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("pdc-perms");
    let locked = dir.path().join("locked.iso");
    std::fs::write(&locked, b"x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let source = validate_iso_input(&locked);
    let dest = validate_file_dest(&locked);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(source.is_err(), dest.is_err(), "{source:?} {dest:?}");
}
