//! The CLI half of KU-F1 over a real image file (keys-upfront design §7.3): one resolve
//! per rip, before any output; E7034 before any output; nothing key-shaped on disk.
use crate::ku_fixtures::*;
use crate::rip_keys::with_sources;

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn run_cli(source: &str, dest: &str, a: &[&str]) -> (i32, String) {
    crate::output::capture(|| super::run(source, dest, &args(a)))
}

/// FK1 (KU §2.1 invariant 1, "exactly one `KeyRing::resolve`" before "R's first
/// output byte"): `-t 1,2,3` and `-t all` over a 3-title, 2-group image make exactly
/// 2 requests, every one before any output exists. FK10: no key or VID on disk.
#[test]
fn cli_rip_resolves_once_per_rip() {
    let fx = bd_image(&[Some(K1), Some(K2), Some(K1)], 2);
    for titles in [&["-t", "1", "-t", "2", "-t", "3"][..], &["-t", "all"][..]] {
        let dir = TempDir::new("fk1");
        let iso = fx.write(dir.path(), "disc.iso");
        let out = dir.path().join("out");
        let calls = Calls::default();
        calls.watch(&out);
        let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
        let src = format!("iso://{}", iso.display());
        let dest = format!("mkv://{}/", out.display());
        let (code, text) = with_sources(f, || run_cli(&src, &dest, titles));
        assert_eq!(code, 0, "{text}");
        assert_eq!(calls.len(), 2, "one request per key group: {text}");
        assert!(calls.all().iter().all(|c| c.outputs == 0), "asked mid-rip");
        assert_eq!(files_under(&out).len(), 3, "{text}");
        assert_no_secret_on_disk(dir.path(), &[K1, K2, VID]);
    }
}

// `-t 1 -t 2 mkv://<dir>/out.mkv` from `src` writes `out_t1.mkv` and `out_t2.mkv` beside
// the given name, and no `out.mkv`.
fn assert_fans_out_beside(src: &str, dir: &std::path::Path) {
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    let dest = format!("mkv://{}", out.join("out.mkv").display());
    let (code, text) = with_sources(factory(&[], &Calls::default()), || {
        run_cli(src, &dest, &["-t", "1", "-t", "2"])
    });
    assert_eq!(code, 0, "{text}");
    let mut names: Vec<String> = files_under(&out)
        .iter()
        .map(|p| p.strip_prefix(&out).unwrap().display().to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["out_t1.mkv", "out_t2.mkv"], "{text}");
}

/// Several titles of an image onto a file name fan out beside it, one file per title.
#[test]
fn image_titles_onto_a_file_name_fan_out_beside_it() {
    let fx = bd_image(&[None, None], 1);
    let dir = TempDir::new("fanout-iso");
    let iso = fx.write(dir.path(), "disc.iso");
    assert_fans_out_beside(&format!("iso://{}", iso.display()), dir.path());
}

/// Several titles of a folder onto a file name fan out beside it, as for an image.
#[test]
fn folder_titles_onto_a_file_name_fan_out_beside_it() {
    let fx = bd_image(&[None, None], 1);
    let dir = TempDir::new("fanout-dir");
    // The image's files, written out as a disc folder.
    let tree = dir.path().join("tree");
    let mut src = fx.source();
    let fs = libfreemkv::read_filesystem(&mut src).unwrap();
    let mut paths = vec!["BDMV/index.bdmv".to_string(), "AACS/Unit_Key_RO.inf".into()];
    for i in 0..2 {
        paths.push(format!("BDMV/PLAYLIST/{i:05}.mpls"));
        paths.push(format!("BDMV/CLIPINF/{i:05}.clpi"));
        paths.push(format!("BDMV/STREAM/{i:05}.m2ts"));
    }
    for p in &paths {
        let at = tree.join(p);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, fs.read_file(&mut src, &format!("/{p}")).unwrap()).unwrap();
    }
    assert_fans_out_beside(&format!("dir://{}", tree.display()), dir.path());
}

/// FK1/FK10 for a decrypted image: iso:// → iso:// resolves the whole disc once (both
/// groups), before the destination exists, and writes no key.
#[test]
fn cli_image_decrypt_resolves_once() {
    let fx = bd_image(&[Some(K1), Some(K2)], 2);
    let dir = TempDir::new("fk1iso");
    let iso = fx.write(dir.path(), "disc.iso");
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let calls = Calls::default();
    calls.watch(&out);
    let f = factory(&[(Answer::Online, &[K1, K2])], &calls);
    let src = format!("iso://{}", iso.display());
    let dest = format!("iso://{}", out.join("plain.iso").display());
    let (code, text) = with_sources(f, || run_cli(&src, &dest, &[]));
    assert_eq!(code, 0, "{text}");
    assert_eq!(calls.len(), 2, "{text}");
    assert!(calls.all().iter().all(|c| c.outputs == 0), "asked mid-copy");
    assert_no_secret_on_disk(dir.path(), &[K1, K2, VID]);
}

/// M2: Ctrl-C during the up-front resolve prints the interrupt, not a bare E6010.
#[test]
fn a_ctrl_c_during_the_resolve_reads_as_interrupted() {
    let out = crate::output::Output::new(false, false);
    let (set, text) =
        crate::output::capture(|| super::report_keys(Err(libfreemkv::Error::Halted), &out));
    assert!(set.is_none());
    assert!(
        text.contains(&crate::strings::get("rip.interrupted")),
        "{text}"
    );
    assert!(!text.contains("E6010"), "{text}");
}

/// FK3 (KU §7.3): the CLI reaches the shared table's requests and verdicts; the GUI's
/// `engine` test checks the same table, so the two shells never deviate.
#[test]
fn cli_and_gui_same_requests_same_verdicts() {
    for case in fk3_cases() {
        let dir = TempDir::new(case.name);
        let iso = case.image(dir.path());
        let calls = Calls::default();
        let src = format!("iso://{}", iso.display());
        let dest = format!("mkv://{}/", dir.path().join("out").display());
        let (code, text) = with_sources(case.sources(&calls), || {
            run_cli(&src, &dest, &["-t", "all"])
        });
        assert_eq!(calls.len(), case.requests, "{}: {text}", case.name);
        match case.code {
            None => assert_eq!(code, 0, "{}: {text}", case.name),
            Some(c) => assert_eq!(named_code(&text), Some(c), "{}: {text}", case.name),
        }
    }
}

static FT10_CTRL_C: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// FT10 (stop design v5 §4.3 item 2): "A key call in flight stops on the first Ctrl-C";
// the token is driven by a test flag (`watching`), and the resolve "returns `Halted` ≤ 1 s;
// no ISO or partial created". The never-answering service is `Answer::Hang`.
#[test]
fn ctrl_c_interrupts_inflight_key_query() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("ft10");
    let iso = fx.write(dir.path(), "disc.iso");
    let plain = dir.path().join("plain.iso");
    let calls = Calls::default();
    let f = factory(&[(Answer::Hang, &[K1])], &calls);
    let (src, dest) = (
        format!("iso://{}", iso.display()),
        format!("iso://{}", plain.display()),
    );
    let token = crate::cli_stop::watching(&FT10_CTRL_C);
    let seen = calls.clone();
    let press = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while seen.len() == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        FT10_CTRL_C.store(true, std::sync::atomic::Ordering::Release);
        std::time::Instant::now()
    });
    let out = crate::output::Output::new(false, true);
    let plan = super::cli_plan(
        &src,
        &dest,
        &super::KeyConfig::default(),
        (false, false, false),
    );
    let ok = with_sources(f, || super::whole_disc(&plan, &token, &out)) == 0;
    let pressed = press.join().unwrap();
    assert!(!ok, "a stopped copy is not a success");
    assert!(
        pressed.elapsed() <= std::time::Duration::from_secs(1),
        "Stop took too long"
    );
    assert_eq!(calls.len(), 1, "the key call was in flight");
    assert!(!plain.exists(), "no ISO");
    assert_eq!(files_under(dir.path()), vec![iso], "no partial");
}

// §2.5: "Kept after Stop … where it guards the resumable artifact"; ST-I2's "progress
// kept" (§5.7) says so after the interrupt, and only while the image is on disk. Per spec.
#[test]
fn an_interrupted_copy_says_progress_kept_while_the_image_is_on_disk() {
    let dir = TempDir::new("cli-kept");
    let iso = dir.path().join("Movie.iso");
    let (stopped, kept) = (
        crate::strings::get("rip.interrupted"),
        crate::strings::get("stop.progress_kept"),
    );
    assert_eq!(
        super::interrupted_text(&iso),
        stopped,
        "no image: nothing kept"
    );
    std::fs::write(&iso, b"partial").unwrap();
    assert_eq!(super::interrupted_text(&iso), format!("{stopped}\n{kept}"));
}

static LOCK_CTRL_C: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// Stop design v5 §2.5: "The lock is taken only by the freemkv CLI/GUI and the engine
// `_with` entries"; "Deleted on success … Kept after Stop". While another process holds
// `<final>.lock` the CLI writes nothing and a Ctrl-C ends the wait. Per spec.
#[test]
fn a_whole_disc_copy_holds_the_artifact_lock() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cli-lock");
    let iso = fx.write(dir.path(), "disc.iso");
    let plain = dir.path().join("plain.iso");
    let sidecar = dir.path().join("plain.iso.lock");
    let (src, dest) = (
        format!("iso://{}", iso.display()),
        format!("iso://{}", plain.display()),
    );
    let out = crate::output::Output::new(false, true);
    let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let never = libfreemkv::Halt::new();
    let plan = super::cli_plan(
        &src,
        &dest,
        &super::KeyConfig::default(),
        (false, false, false),
    );
    let ok = with_sources(f, || super::whole_disc(&plan, &never, &out)) == 0;
    assert!(ok && plain.exists(), "the copy ran");
    assert!(!sidecar.exists(), "deleted on success");

    std::fs::remove_file(&plain).unwrap();
    let held = libfreemkv::io::ArtifactLock::acquire(&plain, &[], &never).unwrap();
    let token = crate::cli_stop::watching(&LOCK_CTRL_C);
    let press = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(300));
        LOCK_CTRL_C.store(true, std::sync::atomic::Ordering::Release);
    });
    let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let loud = crate::output::Output::new(false, false);
    let (code, text) =
        crate::output::capture(|| with_sources(f, || super::whole_disc(&plan, &token, &loud)));
    let ok = code == 0;
    press.join().unwrap();
    drop(held);
    assert!(!ok, "a copy that never got the lock is not a success");
    assert!(
        !plain.exists(),
        "nothing written under another holder's lock"
    );
    assert!(
        text.contains(&crate::strings::get("rip.interrupted")),
        "{text}"
    );
}

/// FK11, CLI half (KU §4.2, J11/J12; USER 2026-09-28: no `--vid-from`): an image whose
/// key only the VID derives (KS-16 "Kvu = AES-G(Km, IDv)") asks once, then exits with
/// the shared E7034 text before any output. Nothing key-shaped is written.
#[test]
fn cli_surfaces_e7034_before_any_output() {
    use libfreemkv::spec::keys::KS_16_KVU;
    assert!(KS_16_KVU.text.contains("Kvu = AES-G(Km, IDv)"));
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("fk11");
    let iso = fx.write(dir.path(), "capture.iso");
    sidecar(&fx, &iso, true);
    let src = format!("iso://{}", iso.display());
    let mkv = dir.path().join("movie.mkv");
    let dest = format!("mkv://{}", mkv.display());
    let calls = Calls::default();
    let f = factory(&[(Answer::OnlineNeedsVid, &[K1])], &calls);
    let (code, text) = with_sources(f, || run_cli(&src, &dest, &[]));
    assert_eq!(code, 1, "{text}");
    assert_eq!(calls.len(), 1, "asked once, without the VID");
    assert!(text.contains(&crate::strings::get("error.E7034")), "{text}");
    assert!(!text.contains("--vid-from"), "no flag is suggested: {text}");
    assert!(!mkv.exists(), "E7034 comes before any output");
    assert_no_secret_on_disk(dir.path(), &[K1, VID]);
    let (code, _) = run_cli(&src, &dest, &["--vid-from", "disc://"]);
    assert_eq!(code, 1, "--vid-from is not a flag");
}
