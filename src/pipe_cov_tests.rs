//! CLI paths no other test reached: stream selection on a real image, a folder
//! extraction, loose-clip keys, the drive-error renderers and the progress printers.
use super::*;
use crate::ku_fixtures::*;
use crate::rip_keys::with_sources;

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn run_cli(source: &str, dest: &str, a: &[&str]) -> (i32, String) {
    crate::output::capture(|| super::run(source, dest, &args(a)))
}

fn loud() -> Output {
    Output::new(false, false)
}

// `fx`'s files written out as a disc folder under `root`.
fn write_tree(fx: &Fx, root: &std::path::Path, titles: usize) {
    let mut src = fx.source();
    let fs = libfreemkv::read_filesystem(&mut src).unwrap();
    let mut paths = vec!["BDMV/index.bdmv".to_string(), "AACS/Unit_Key_RO.inf".into()];
    for i in 0..titles {
        paths.push(format!("BDMV/PLAYLIST/{i:05}.mpls"));
        paths.push(format!("BDMV/CLIPINF/{i:05}.clpi"));
        paths.push(format!("BDMV/STREAM/{i:05}.m2ts"));
    }
    for p in &paths {
        let at = root.join(p);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, fs.read_file(&mut src, &format!("/{p}")).unwrap()).unwrap();
    }
}

fn pass(kind: libfreemkv::progress::PassKind) -> libfreemkv::progress::PassProgress {
    libfreemkv::progress::PassProgress {
        kind,
        work_done: 500,
        work_total: 1_000,
        bytes_good_total: 4_000,
        bytes_unreadable_total: 0,
        bytes_pending_total: 0,
        bytes_retryable_total: 0,
        bytes_total_disc: 10_000,
        disc_duration_secs: Some(7200.0),
        bytes_bad_in_main_title: 0,
        main_title_duration_secs: None,
        main_title_size_bytes: None,
        located: Default::default(),
    }
}

// `-a` on an image: a present language rips, an absent one is refused for a single
// title, and a tag that is no language fails with the title's languages listed.
#[test]
fn audio_language_selection_runs_against_the_scanned_image() {
    let fx = bd_image(&[None], 1);
    let dir = TempDir::new("cov-sel");
    let iso = fx.write(dir.path(), "disc.iso");
    let src = format!("iso://{}", iso.display());
    let rip = |name: &str, a: &[&str]| {
        let dest = format!("mkv://{}", dir.path().join(name).display());
        with_sources(factory(&[], &Calls::default()), || run_cli(&src, &dest, a))
    };
    let (code, text) = rip("eng.mkv", &["-a", "eng"]);
    assert_eq!(code, 0, "{text}");
    let (code, text) = rip("fra.mkv", &["-a", "fra"]);
    assert_eq!(code, 1, "{text}");
    let (code, text) = rip("bad.mkv", &["-a", "notalanguage"]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("notalanguage"), "{text}");
}

// An unknown tag on a title without tagged audio or subtitles names none available.
#[test]
fn an_unknown_language_on_a_title_with_subtitles_lists_them() {
    use libfreemkv::{Codec, LabelQualifier, Stream, SubtitleStream};
    let e = freemkv_engine::StreamSelError::UnknownLanguage { tag: "zz".into() };
    let empty = libfreemkv::DiscTitle::empty();
    let none = render_stream_sel_error(&e, &empty);
    assert!(none.contains(&strings::get("error.stream_none")), "{none}");
    let titled = libfreemkv::DiscTitle {
        streams: vec![Stream::Subtitle(SubtitleStream {
            pid: 0x1200,
            codec: Codec::Pgs,
            language: "deu".into(),
            forced: false,
            qualifier: LabelQualifier::None,
            codec_data: None,
        })],
        ..libfreemkv::DiscTitle::empty()
    };
    assert!(render_stream_sel_error(&e, &titled).contains("deu"));
}

// A `dir://` destination from an image extracts its file tree through `whole_disc`.
#[test]
fn an_image_to_a_folder_reports_the_extraction() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cov-tree");
    let iso = fx.write(dir.path(), "disc.iso");
    let out = dir.path().join("tree");
    let src = format!("iso://{}", iso.display());
    let dest = format!("dir://{}", out.display());
    let f = factory(&[(Answer::Keydb, &[K1])], &Calls::default());
    let (_code, text) = with_sources(f, || run_cli(&src, &dest, &[]));
    let extracting = strings::fmt("dir.extracting", &[("path", &out.display().to_string())]);
    assert!(text.contains(&extracting), "{text}");
}

// An image copied onto itself is refused before the engine opens anything.
#[test]
fn whole_disc_refuses_an_image_onto_itself() {
    let dir = TempDir::new("cov-self");
    let iso = dir.path().join("disc.iso");
    std::fs::write(&iso, vec![0u8; 4096]).unwrap();
    let url = format!("iso://{}", iso.display());
    let plan = cli_plan(&url, &url, &KeyConfig::default(), (false, false, false));
    let never = libfreemkv::Halt::new();
    let (code, text) = crate::output::capture(|| whole_disc(&plan, &never, &loud()));
    assert_eq!(code, 1);
    assert!(
        text.contains(&strings::get("error.dest_is_source")),
        "{text}"
    );
    assert_eq!(
        std::fs::read(&iso).unwrap().len(),
        4096,
        "the source is untouched"
    );
}

// A loose clip's keys come from its disc folder: found, refused, or none at all.
#[test]
fn info_clip_keys_walks_up_to_the_disc_folder() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cov-clip");
    let tree = dir.path().join("DISC");
    write_tree(&fx, &tree, 1);
    let clip = tree.join("BDMV/STREAM/00000.m2ts");
    let url = format!("m2ts://{}", clip.display());
    let found = with_sources(
        factory(&[(Answer::Keydb, &[K1])], &Calls::default()),
        || info_clip_keys(&url, None),
    );
    assert!(matches!(found, Ok(Some(_))));
    let refused = crate::output::capture(|| {
        with_sources(factory(&[], &Calls::default()), || {
            info_clip_keys(&url, None)
        })
    })
    .0;
    assert!(refused.is_err());

    let lone = dir.path().join("lone.m2ts");
    std::fs::write(&lone, libfreemkv::test_util::synthetic_bd_clip(16)).unwrap();
    let lone_url = format!("m2ts://{}", lone.display());
    let none = with_sources(factory(&[], &Calls::default()), || {
        info_clip_keys(&lone_url, None)
    });
    assert!(matches!(none, Ok(None)));
    assert!(matches!(info_clip_keys("mkv:///x.mkv", None), Ok(None)));
}

// A loose clip whose disc folder holds no key for it stops before any output.
#[test]
fn a_loose_clip_without_its_key_is_refused_before_output() {
    let fx = bd_image(&[Some(K1)], 1);
    let dir = TempDir::new("cov-clip-rip");
    let tree = dir.path().join("DISC");
    write_tree(&fx, &tree, 1);
    let clip = tree.join("BDMV/STREAM/00000.m2ts");
    let src = format!("m2ts://{}", clip.display());
    let mkv = dir.path().join("clip.mkv");
    let dest = format!("mkv://{}", mkv.display());
    let (code, text) = with_sources(factory(&[], &Calls::default()), || {
        run_cli(&src, &dest, &[])
    });
    assert_eq!(code, 1, "{text}");
    assert!(!mkv.exists(), "{text}");
}

// `info -v`'s key status over a drive: no source holds the key, so there is no status.
#[test]
fn resolve_info_keys_over_a_fake_drive() {
    use libfreemkv::test_util::FakeTransport;
    let fx = bd_image(&[Some(K1)], 1);
    let (t, _fake) = FakeTransport::new();
    let mut drive = libfreemkv::Drive::from_transport(Box::new(t.with_image(fx.img.image.clone())));
    let quiet = Output::new(false, true);
    let none = with_sources(factory(&[], &Calls::default()), || {
        resolve_info_keys(&mut drive, &fx.disc, &None, &quiet)
    });
    assert!(none.is_none());
    let _ = with_sources(
        factory(&[(Answer::Keydb, &[K1])], &Calls::default()),
        || resolve_info_keys(&mut drive, &fx.disc, &None, &quiet),
    );
}

#[test]
fn drive_open_errors_render_for_the_user() {
    crate::strings::set_locale("en");
    let auto = libfreemkv::Error::DeviceNotFound {
        path: String::new(),
    };
    assert_eq!(
        render_drive_open_error(&auto),
        strings::get("error.no_drive")
    );
    assert_eq!(
        render_drive_open_error(&libfreemkv::Error::Halted),
        strings::get("rip.interrupted")
    );
    let other = libfreemkv::Error::NoStreams;
    assert_eq!(render_drive_open_error(&other), render_error(&other));
    assert_eq!(
        reopen_failure(&other).result,
        freemkv_engine::TitleResult::Failed
    );
    assert!(!disc_scan_all_titles_failed().is_empty());
    assert!(matches!(
        device_target("disc:///dev/fmkv-cov-none"),
        libfreemkv::DeviceTarget::Path(_)
    ));
    assert!(matches!(
        device_target("disc://"),
        libfreemkv::DeviceTarget::Autodetect
    ));
}

#[test]
fn key_flags_feed_the_drive_handshake_and_the_ssrf_warning() {
    let dir = TempDir::new("cov-keys");
    let missing = Some(dir.path().join("no-keydb.cfg").display().to_string());
    let keys = KeyConfig {
        keydb_path: missing.clone(),
        key_url: Some("http://127.0.0.1/keys".into()),
        key_auth: None,
    };
    assert_eq!(keys.keydb_path(), &missing);
    assert!(drive_credentials(keys.keydb_path()).is_none());
    let ((), text) = crate::output::capture(|| warn_ssrf_rejected(&keys, &loud()));
    assert!(!text.is_empty(), "a rejected key URL is reported");
}

#[test]
fn the_title_loop_sink_reports_a_skipped_stub() {
    crate::strings::set_locale("en");
    let out = loud();
    let sink = CliTitleLoopSink { out: &out };
    let ((), text) = crate::output::capture(|| {
        use freemkv_engine::Sink as _;
        sink.event(&freemkv_engine::Event::TitleSkipped {
            idx: 1,
            empty: true,
        });
        sink.event(&freemkv_engine::Event::TitleSkipped {
            idx: 2,
            empty: false,
        });
        sink.event(&freemkv_engine::Event::Phase { name: "mux" });
    });
    assert!(
        text.contains(&strings::fmt("rip.title_skipped_empty", &[("num", "2")])),
        "{text}"
    );
    assert!(
        text.contains(&strings::fmt("rip.title_skipped", &[("num", "3")])),
        "{text}"
    );
}

// The mux progress line repaints once half a second has passed since the last one.
#[test]
fn write_progress_repaints_after_the_throttle() {
    let events = CliMuxEvents::new(loud(), "mkv:///cov.mkv".into(), false);
    let now = std::time::Instant::now();
    let earlier = now
        .checked_sub(std::time::Duration::from_secs(2))
        .unwrap_or(now);
    *events.start.lock().unwrap() = Some(earlier);
    *events.last_update.lock().unwrap() = earlier;
    events.on_write_progress(5 << 20, 10 << 20);
    assert!(*events.last_update.lock().unwrap() >= now);

    let start = earlier;
    print_progress(0, 0, &start);
    print_progress(0, 10 << 20, &start);
    print_progress(5 << 20, 10 << 20, &start);
    print_progress(2 << 30, 4 << 30, &start);
    print_completion_summary(&Output::new(false, true), 2 << 30, start);
}

#[test]
fn disc_progress_covers_every_pass_kind_and_title_damage() {
    use libfreemkv::progress::PassKind;
    let mut unknown = pass(PassKind::Sweep);
    unknown.bytes_total_disc = 0;
    assert!(disc_progress_line(&unknown, 0, None).is_none());
    assert_eq!(fmt_disc_damage(&unknown), strings::get("rip.damage_none"));
    for kind in [
        PassKind::Trim { reverse: false },
        PassKind::Scrape { reverse: true },
        PassKind::Verify,
    ] {
        let line = disc_progress_line(&pass(kind), 1 << 20, Some(90)).expect("sized");
        assert!(!line.contains("?:??"), "{line}");
    }
    print_disc_progress(&unknown, 0, None);

    let mut damaged = pass(PassKind::Sweep);
    damaged.bytes_unreadable_total = 1_000;
    damaged.bytes_bad_in_main_title = 10;
    damaged.main_title_duration_secs = Some(7200.0);
    damaged.main_title_size_bytes = Some(10_000);
    assert_ne!(fmt_disc_damage(&damaged), strings::get("rip.damage_none"));
}

#[test]
fn stream_info_shows_labels_purposes_and_secondary_audio() {
    use libfreemkv::{
        AudioChannels, AudioStream, Codec, ColorSpace, FrameRate, HdrFormat, LabelPurpose,
        Resolution, SampleRate, Stream, VideoStream,
    };
    for p in [
        LabelPurpose::Commentary,
        LabelPurpose::Descriptive,
        LabelPurpose::Score,
        LabelPurpose::Ime,
    ] {
        assert!(audio_purpose_key(p).is_some());
    }
    assert!(audio_purpose_key(LabelPurpose::Normal).is_none());
    let title = libfreemkv::DiscTitle {
        streams: vec![
            Stream::Video(VideoStream {
                pid: 0x1011,
                codec: Codec::Hevc,
                resolution: Resolution::R2160p,
                frame_rate: FrameRate::F23_976,
                hdr: HdrFormat::Hdr10,
                color_space: ColorSpace::Bt2020,
                display_aspect: None,
                secondary: false,
                label: "Feature".into(),
                measured_cicp: None,
            }),
            Stream::Audio(AudioStream {
                pid: 0x1100,
                codec: Codec::Ac3,
                channels: AudioChannels::Stereo,
                language: "eng".into(),
                sample_rate: SampleRate::S48,
                secondary: true,
                purpose: LabelPurpose::Commentary,
                label: String::new(),
            }),
        ],
        ..libfreemkv::DiscTitle::empty()
    };
    let ((), text) = crate::output::capture(|| print_stream_info(&loud(), &title));
    assert!(text.contains("Feature"), "{text}");
}

#[test]
fn render_copy_grades_a_stop_an_empty_read_and_a_lossy_disc() {
    let quiet = Output::new(false, true);
    let iso = std::path::Path::new("/nonexistent/fmkv-cov.iso");
    let copy = |halted, good, unreadable| freemkv_engine::CopyResult {
        bytes_total: good + unreadable,
        bytes_good: good,
        bytes_unreadable: unreadable,
        bytes_pending: 0,
        recovered_this_pass: 0,
        complete: false,
        halted,
    };
    let now = std::time::Instant::now();
    let grade = |r: &freemkv_engine::CopyResult, disc: Option<&libfreemkv::Disc>| -> i32 {
        crate::output::capture(|| render_copy(r, disc, iso, None, now, &quiet)).0
    };
    assert_eq!(grade(&copy(true, 1 << 20, 0), None), 1);
    assert_eq!(grade(&copy(false, 0, 1 << 20), None), 1);
    let fx = bd_image(&[None], 1);
    assert_eq!(
        grade(&copy(false, 9 << 20, 1 << 20), Some(&fx.disc)),
        DISC_COPY_DAMAGED_EXIT
    );
}

#[test]
fn render_extract_reports_a_stop() {
    let res = libfreemkv::ExtractResult {
        files: vec![libfreemkv::FileResult {
            path: "BDMV/index.bdmv".into(),
            bytes_good: 2048,
            bytes_unreadable: 0,
            complete: true,
        }],
        halted: true,
        ..Default::default()
    };
    let (ok, text) = crate::output::capture(|| render_extract(&res, &loud()));
    assert!(!ok);
    assert!(text.contains(&strings::get("rip.interrupted")), "{text}");
}
