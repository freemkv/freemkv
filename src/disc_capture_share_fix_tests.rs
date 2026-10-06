use super::*;
use libfreemkv::disc::DiscRegion;
use libfreemkv::{DiscFormat, Error};
use std::path::PathBuf;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fmkv-share-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("scratch dir");
    d
}

fn put(root: &std::path::Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(p, bytes).expect("write fixture");
}

fn bare_disc(encrypted: bool, aacs_error: Option<Error>) -> Disc {
    Disc {
        volume_id: "T".to_string(),
        meta_title: None,
        format: DiscFormat::BluRay,
        capacity_sectors: 0,
        capacity_bytes: 0,
        layers: 1,
        titles: Vec::new(),
        region: DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted,
        aacs_error,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    }
}

fn summary() -> DiscSummary {
    DiscSummary {
        file_count: 1,
        total_bytes: 1,
        skipped: Vec::new(),
    }
}

// K4: a FAILED AACS step must say so (with its code), not blame a keyless scan.
#[test]
fn a_failed_aacs_step_is_reported_as_a_failure() {
    let disc = bare_disc(true, Some(Error::AacsNoKeys));
    let json = aacs_json(&disc);
    let code = format!("E{}", Error::AacsNoKeys.code());
    assert!(
        json.contains(&format!("\"aacs_error\": \"{code}\"")),
        "{json}"
    );
    assert!(!json.contains("keyless") && !json.contains("-v"), "{json}");
    let body = issue_body(&disc, &summary(), "QUJD").0;
    assert!(!body.contains("keyless") && body.contains(&code), "{body}");
}

// K4: no AACS on the disc (DVD / clear BD) is not a missing capture.
#[test]
fn a_disc_without_aacs_says_so() {
    let disc = bare_disc(false, None);
    let json = aacs_json(&disc);
    assert!(!json.contains("keyless") && !json.contains("-v"), "{json}");
    assert!(json.contains("\"aacs_present\": false"), "{json}");
    let body = issue_body(&disc, &summary(), "QUJD").0;
    assert!(!body.contains("keyless"), "{body}");
}

// L7: a real BD's zip base64 blows GitHub's 65536-char body cap; point at the file instead.
#[test]
fn an_oversized_zip_is_not_inlined() {
    let disc = bare_disc(false, None);
    let big = "A".repeat(200_000);
    let body = issue_body(&disc, &summary(), &big).0;
    assert!(
        body.chars().count() <= crate::info::BODY_INLINE_BUDGET_CHARS,
        "body is {} chars",
        body.chars().count()
    );
    assert!(body.contains("profile.zip"), "{body}");
    let small = issue_body(&disc, &summary(), "QUJD").0;
    assert!(
        small.contains("QUJD"),
        "a small zip is still inlined: {small}"
    );
}

fn bd_folder(tag: &str) -> PathBuf {
    let src = scratch(tag).join("disc");
    put(&src, "BDMV/index.bdmv", b"INDX0200");
    put(&src, "BDMV/PLAYLIST/00001.mpls", b"MPLS0200-playlist");
    put(&src, "BDMV/CLIPINF/00001.clpi", b"HDMV0200-clip");
    std::fs::create_dir_all(src.join("BDMV/STREAM")).expect("stream dir");
    src
}

// L1: only plain `/`-separated components survive, on every OS.
#[test]
fn only_plain_portable_paths_are_safe() {
    for ok in [
        "BDMV/PLAYLIST/00001.mpls",
        "VIDEO_TS/VTS_01_0.IFO",
        "BDMV/META/DL/bdmt_eng.xml",
    ] {
        assert!(is_safe_rel_path(ok), "{ok}");
    }
    for bad in [
        "",
        "/etc/x",
        "BDMV//x",
        "BDMV/../x",
        "..",
        "./x",
        "a\\..\\x.xml",
        "C:x.xml",
        "BDMV/a:b.xml",
        "BDMV/a*.xml",
        "BDMV/x.xml.",
        "BDMV/x.xml ",
        "BDMV/CON.xml",
        "BDMV/com1.mpls",
        "BDMV/CON .xml",
        "BDMV/CONIN$",
        "BDMV/conout$.txt",
        "BDMV/COM\u{B9}.mpls",
        "BDMV/LPT\u{B3}",
        "BDMV/COM0",
        "BDMV/LPT9",
        "BDMV/a\u{1}.xml",
        "BDMV/a\".xml",
        "BDMV/a|b",
    ] {
        assert!(!is_safe_rel_path(bad), "{bad:?} accepted");
    }
    assert!(
        is_safe_rel_path("BDMV/CONSOLE.xml"),
        "only exact DOS device stems are reserved"
    );
}

// T11: the success path writes nested names and zips exactly those entries.
#[test]
fn a_structure_fold_writes_nested_files_and_zips_them() {
    let src = bd_folder("ok");
    let out = src.parent().expect("parent").join("profile");
    std::fs::create_dir_all(&out).expect("profile dir");
    let mut img = libfreemkv::DirImage::open(&src).expect("dir image");
    let mut written = Vec::new();
    let s = fold_structure(&out, &mut written, &mut img)
        .expect("fold ok")
        .expect("files found");
    for rel in ["BDMV/PLAYLIST/00001.mpls", "BDMV/CLIPINF/00001.clpi"] {
        assert!(written.iter().any(|w| w == rel), "{rel} not in {written:?}");
        assert!(out.join(rel).is_file(), "{rel} not written");
    }
    assert_eq!(s.file_count, written.len());
    let zip = crate::info::zip_files(&out, &written).expect("zip");
    let mut a = zip::ZipArchive::new(std::io::Cursor::new(zip)).expect("valid zip");
    let names: Vec<String> = (0..a.len())
        .map(|i| a.by_index(i).expect("entry").name().to_string())
        .collect();
    assert_eq!(names, written);
}

// L1: a disc-supplied name carrying a separator or `..` never reaches the filesystem.
#[cfg(unix)]
#[test]
fn a_hostile_disc_name_is_skipped_not_joined() {
    let src = bd_folder("evil");
    put(&src, "BDMV/META/DL/x\\..\\..\\..\\..\\evil.xml", b"<x/>");
    put(&src, "BDMV/META/DL/ok.xml", b"<x/>");
    let out = src.parent().expect("parent").join("profile");
    std::fs::create_dir_all(&out).expect("profile dir");
    let mut img = libfreemkv::DirImage::open(&src).expect("dir image");
    let mut written = Vec::new();
    let _ = fold_structure(&out, &mut written, &mut img).expect("fold ok");
    assert!(
        written.iter().any(|w| w == "BDMV/META/DL/ok.xml"),
        "{written:?}"
    );
    assert!(
        !written.iter().any(|w| w.contains('\\') || w.contains("..")),
        "a hostile name was written: {written:?}"
    );
}

// Every file refused: the fold still returns (count 0 + reasons) so callers can say so.
#[test]
fn a_fold_where_every_write_fails_reports_zero_saved() {
    let src = bd_folder("allfail");
    let out = src.parent().expect("parent").join("profile");
    std::fs::write(&out, b"not a dir").expect("blocker file");
    let mut img = libfreemkv::DirImage::open(&src).expect("dir image");
    let mut written = Vec::new();
    let s = fold_structure(&out, &mut written, &mut img)
        .expect("fold ok")
        .expect("files found");
    assert_eq!(s.file_count, 0);
    assert!(written.is_empty() && !s.skipped.is_empty());
    assert!(all_skipped_line(&s).contains(&s.skipped.len().to_string()));
}

// L14: one unwritable file must not end the process; the rest still land.
#[test]
fn one_failed_write_skips_that_file_only() {
    let src = bd_folder("fail");
    let out = src.parent().expect("parent").join("profile");
    std::fs::create_dir_all(out.join("BDMV/PLAYLIST/00001.mpls")).expect("blocker");
    let mut img = libfreemkv::DirImage::open(&src).expect("dir image");
    let mut written = Vec::new();
    let _ = fold_structure(&out, &mut written, &mut img).expect("fold ok");
    assert!(
        written.iter().any(|w| w == "BDMV/CLIPINF/00001.clpi"),
        "{written:?}"
    );
    assert!(
        !written.iter().any(|w| w == "BDMV/PLAYLIST/00001.mpls"),
        "{written:?}"
    );
}
