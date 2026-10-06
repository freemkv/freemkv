use super::testmkv::mkv;
use super::*;

const RUNNING: (u32, u32, u32) = (1, 8, 0);

#[test]
fn muxed_with_compares_against_the_running_version() {
    let m = |a: Option<&str>| MuxedWith::from_app(a, RUNNING);
    assert_eq!(
        m(Some("freemkv 1.8.0 (gabc1234)")),
        MuxedWith::Current {
            version: "1.8.0".into()
        }
    );
    assert!(m(Some("freemkv 1.6.11 (g0)")).out_of_date());
    assert!(m(Some("freemkv 1.9.0")).out_of_date(), "any other version");
    assert!(m(Some("libmakemkv v1.17.5")).out_of_date());
    assert!(
        m(Some("freemkv")).out_of_date(),
        "the unversioned stamp older builds wrote"
    );
    assert_eq!(m(None), MuxedWith::Unknown);
    assert!(!m(Some("  ")).out_of_date());
}

#[test]
fn a_storage_error_is_not_cached_as_a_verdict() {
    let t = tempfile::tempdir().unwrap();
    // A directory opens but cannot be read: an I/O failure, not a file verdict.
    let p = t.path().join("odd.mkv");
    std::fs::create_dir(&p).unwrap();
    assert!(audit_fast(&p).is_none());
    let sig = FileSig::stat(&p).unwrap();
    let cache = ProbeCache::default();
    assert!(!cache.refresh_stamp(&p, sig));
    assert_eq!(cache.cached_stamp(&p, sig), None, "retried next pass");
    // A missing file is not a verdict either.
    assert!(audit_fast(&t.path().join("gone.mkv")).is_none());
    // A short file is: it really is not an MKV.
    let short = t.path().join("short.mkv");
    std::fs::write(&short, b"ab").unwrap();
    assert_eq!(audit_fast(&short).unwrap().issues, [AuditIssue::NotMkv]);
}

#[test]
fn stamps_shorten_to_program_and_version() {
    assert_eq!(
        short_label("mkvmerge v96.0 ('It's My Life') 64-bit"),
        "mkvmerge 96.0"
    );
    assert_eq!(short_label("freemkv 1.7.7 (gc8e67f1)"), "freemkv 1.7.7");
    assert_eq!(
        short_label("MakeMKV v1.17.5 linux(x64-release)"),
        "MakeMKV 1.17.5"
    );
    assert_eq!(short_label("freemkv"), "freemkv");
    assert_eq!(short_label(""), "");
}

#[test]
fn the_library_versions_classify_as_the_user_expects() {
    // The running 1.7.7 build against what the test library was written with.
    let m = |a: &str| MuxedWith::from_app(Some(a), (1, 7, 7));
    assert!(!m("freemkv 1.7.7 (gc8e67f1)").out_of_date());
    assert!(m("freemkv 1.6.11 (g1234567)").out_of_date());
    assert!(m("mkvmerge v96.0 ('It's My Life') 64-bit").out_of_date());
}

#[test]
fn cached_lookups_never_touch_the_disk() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("a.mkv");
    std::fs::write(&p, mkv("freemkv 1.6.11 (g1)", Some(60.0), Some(55), true)).unwrap();
    let sig = FileSig::stat(&p).unwrap();
    let cache = ProbeCache::default();
    assert_eq!(cache.cached_stamp(&p, sig), None, "not read yet");
    assert!(cache.refresh_stamp(&p, sig));
    assert!(!cache.refresh_stamp(&p, sig), "cached");
    std::fs::remove_file(&p).unwrap();
    assert_eq!(
        cache.cached_stamp(&p, sig),
        Some(Some("freemkv 1.6.11 (g1)".into()))
    );
}

#[test]
fn stamps_of_vanished_files_are_forgotten() {
    let t = tempfile::tempdir().unwrap();
    let (a, b) = (t.path().join("a.mkv"), t.path().join("b.mkv"));
    let cache = ProbeCache::default();
    let sig = FileSig {
        size: 1,
        mtime_ns: 1,
    };
    cache.record_at(&a, sig, None);
    cache.record_at(&b, sig, None);
    cache.retain(&[(a.clone(), sig)]);
    assert!(cache.cached_stamp(&a, sig).is_some());
    assert_eq!(cache.cached_stamp(&b, sig), None);
}

#[test]
fn the_running_version_is_the_crate_version() {
    let v = running_version();
    assert_eq!(
        format!("{}.{}.{}", v.0, v.1, v.2),
        env!("CARGO_PKG_VERSION").split('-').next().unwrap()
    );
}

#[test]
fn a_changed_file_is_re_read_without_anyone_asking() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("a.mkv");
    std::fs::write(&p, mkv("freemkv 1.6.11 (g1)", Some(60.0), Some(55), true)).unwrap();
    let cache = ProbeCache::default();
    assert_eq!(
        cache.writing_app(&p),
        Some(Some("freemkv 1.6.11 (g1)".into()))
    );
    // A longer stamp changes the size, so the cached entry no longer matches.
    std::fs::write(
        &p,
        mkv("freemkv 1.8.0 (gnewer)", Some(60.0), Some(55), true),
    )
    .unwrap();
    assert_eq!(
        cache.writing_app(&p),
        Some(Some("freemkv 1.8.0 (gnewer)".into()))
    );
    assert_eq!(cache.writing_app(&t.path().join("gone.mkv")), None);
}

#[test]
fn the_fast_audit_checks_structure() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("a.mkv");
    let audit = |bytes: Vec<u8>| {
        std::fs::write(&p, bytes).unwrap();
        audit_fast(&p).unwrap()
    };
    let good = audit(mkv("freemkv 1.8.0", Some(7200.0), Some(7195), true));
    assert!(good.ok, "{good:?}");
    assert_eq!(good.video_tracks, 1);
    assert_eq!(good.video, ["AVC"]);
    assert_eq!(good.runtime_secs, Some(7195.0));

    let short = audit(mkv("freemkv 1.8.0", Some(7200.0), Some(2400), true));
    assert!(!short.ok);
    assert!(matches!(
        short.issues[0],
        AuditIssue::RuntimeMismatch { .. }
    ));

    let no_cues = audit(mkv("freemkv 1.8.0", Some(60.0), None, true));
    assert!(no_cues.ok, "a missing index only warns");
    assert_eq!(no_cues.issues, [AuditIssue::NoCues]);

    let audio_only = audit(mkv("x", Some(60.0), Some(58), false));
    assert_eq!(audio_only.issues, [AuditIssue::NoVideo]);

    let no_duration = audit(mkv("x", None, Some(58), true));
    assert_eq!(no_duration.issues, [AuditIssue::NoDuration]);

    assert_eq!(audit(vec![0x47; 4096]).issues, [AuditIssue::NotMkv]);
    let mut torn = mkv("x", Some(60.0), Some(58), true);
    torn.truncate(20);
    assert!(matches!(
        audit(torn).issues[..],
        [AuditIssue::Unreadable { .. }]
    ));
}

#[test]
fn the_runtime_slack_is_the_larger_of_ten_seconds_and_two_percent() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("a.mkv");
    let ok = |dur: f64, cue: u64| {
        std::fs::write(&p, mkv("x", Some(dur), Some(cue), true)).unwrap();
        audit_fast(&p).unwrap().ok
    };
    // 2 % of 7200 s is 144 s: 100 short passes, 200 short fails.
    assert!(ok(7200.0, 7100));
    assert!(!ok(7200.0, 7000));
    // A short file gets the 10 s floor.
    assert!(ok(60.0, 51));
    assert!(!ok(60.0, 49));
}

#[test]
fn the_audit_counts_and_names_every_track_kind() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("a.mkv");
    let bytes = testmkv::mkv_tracks(
        "x",
        Some(60.0),
        Some(58),
        &[
            (1, "V_MPEGH/ISO/HEVC", "und"),
            (2, "A_TRUEHD", "eng"),
            (2, "A_AC3", "fra"),
            (17, "S_HDMV/PGS", "deu"),
        ],
    );
    std::fs::write(&p, bytes).unwrap();
    let r = audit_fast(&p).unwrap();
    assert_eq!(
        (r.video_tracks, r.audio_tracks, r.subtitle_tracks),
        (1, 2, 1)
    );
    assert_eq!(r.video, ["HEVC"]);
    assert_eq!(
        r.audio,
        [
            TrackFacts {
                codec: "TrueHD".into(),
                language: "eng".into()
            },
            TrackFacts {
                codec: "AC-3".into(),
                language: "fra".into()
            }
        ]
    );
    assert_eq!(r.subtitles, ["deu"]);
    let d = r.detail.as_ref().unwrap();
    assert!(!r.needs_detail());
    assert_eq!(d.audio[0].format, "TrueHD");
    assert_eq!(d.subtitles[0].language, "deu");
}

#[test]
fn every_verdict_carries_a_current_detail_so_nothing_is_read_twice() {
    let t = tempfile::tempdir().unwrap();
    let p = t.path().join("a.mkv");
    std::fs::write(&p, vec![0x47; 4096]).unwrap();
    let r = audit_fast(&p).unwrap();
    assert_eq!(r.issues, [AuditIssue::NotMkv]);
    assert!(!r.needs_detail());
    let old = AuditReport {
        detail: None,
        ..r.clone()
    };
    assert!(old.needs_detail());
    let stale = AuditReport {
        detail: Some(crate::server::library::media::MediaDetail::default()),
        ..r
    };
    assert!(
        stale.needs_detail(),
        "an older reader's detail is read again"
    );
}

#[test]
fn every_codec_id_has_its_name() {
    for (id, name) in [
        ("V_MPEGH/ISO/HEVC", "HEVC"),
        ("V_MPEG4/ISO/AVC", "AVC"),
        ("V_MPEG2", "MPEG-2"),
        ("V_MPEG1", "MPEG-1"),
        ("V_MS/VFW/FOURCC", "VC-1"),
        ("V_AV1", "AV1"),
        ("A_TRUEHD", "TrueHD"),
        ("A_MLP", "TrueHD"),
        ("A_DTS", "DTS"),
        ("A_EAC3", "E-AC-3"),
        ("A_AC3", "AC-3"),
        ("A_PCM/INT/LIT", "PCM"),
        ("A_FLAC", "FLAC"),
        ("A_AAC", "AAC"),
        ("A_OPUS", "Opus"),
        ("A_MPEG/L3", "MP3"),
        ("A_MPEG/L2", "MP2"),
        ("S_HDMV/PGS", "PGS"),
        ("S_VOBSUB", "VobSub"),
        ("S_TEXT/UTF8", "SRT"),
        ("X_UNKNOWN", "X_UNKNOWN"),
    ] {
        assert_eq!(codec_name(id), name, "{id}");
    }
}
