use super::{
    CliMuxEvents, fmt_damage_time, fmt_eta, fmt_speed, print_excluded, print_stream_info,
    render_resolution_trace,
};
use crate::output::{Output, capture};

fn loud() -> Output {
    Output::new(false, false)
}

fn video() -> libfreemkv::Stream {
    libfreemkv::Stream::Video(libfreemkv::VideoStream {
        pid: 0x1011,
        codec: libfreemkv::Codec::Hevc,
        resolution: libfreemkv::Resolution::R2160p,
        frame_rate: libfreemkv::FrameRate::F23_976,
        hdr: libfreemkv::HdrFormat::Hdr10,
        color_space: libfreemkv::ColorSpace::Bt2020,
        display_aspect: None,
        secondary: false,
        label: String::new(),
        measured_cicp: None,
    })
}

fn audio(codec: libfreemkv::Codec) -> libfreemkv::Stream {
    libfreemkv::Stream::Audio(libfreemkv::AudioStream {
        pid: 0x1100,
        codec,
        channels: libfreemkv::AudioChannels::Surround51,
        language: "eng".into(),
        sample_rate: libfreemkv::SampleRate::S48,
        secondary: false,
        purpose: libfreemkv::LabelPurpose::Normal,
        label: String::new(),
    })
}

fn subtitle() -> libfreemkv::Stream {
    libfreemkv::Stream::Subtitle(libfreemkv::SubtitleStream {
        pid: 0x1200,
        codec: libfreemkv::Codec::Pgs,
        language: "eng".into(),
        forced: false,
        qualifier: libfreemkv::LabelQualifier::None,
        codec_data: None,
    })
}

fn title(streams: Vec<libfreemkv::Stream>, duration_secs: f64) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        playlist: "00800.mpls".into(),
        playlist_id: 800,
        duration_secs,
        size_bytes: 40 << 30,
        clips: Vec::new(),
        streams,
        chapters: Vec::new(),
        extents: Vec::new(),
        content_format: libfreemkv::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

#[test]
fn fmt_speed_scales_through_mb_kb_b_and_stall() {
    assert_eq!(fmt_speed(12.5), "12.5 MB/s");
    assert_eq!(fmt_speed(0.5), "512 KB/s");
    assert_eq!(fmt_speed(0.0004), "419 B/s");
    assert_eq!(fmt_speed(0.0), "stalled");
}

#[test]
fn fmt_eta_shows_mmss_hhmmss_and_a_placeholder_for_unknown() {
    assert_eq!(fmt_eta(0.0), "?:??");
    assert_eq!(fmt_eta(f64::INFINITY), "?:??");
    assert_eq!(fmt_eta(65.0), "1:05");
    assert_eq!(fmt_eta(3725.0), "1:02:05");
}

#[test]
fn fmt_damage_time_picks_the_unit_by_magnitude() {
    assert_eq!(fmt_damage_time(7200.0), "2.0h");
    assert_eq!(fmt_damage_time(90.0), "2m");
    assert_eq!(fmt_damage_time(5.0), "5s");
    assert_eq!(fmt_damage_time(0.25), "0.25s");
    assert_eq!(fmt_damage_time(0.004), "4ms");
}

#[test]
fn print_stream_info_lists_every_stream_and_the_runtime() {
    crate::strings::set_locale("en");
    let t = title(
        vec![video(), audio(libfreemkv::Codec::TrueHd), subtitle()],
        7530.0,
    );
    let (_, printed) = capture(|| print_stream_info(&loud(), &t));
    assert!(printed.contains("Streams: 3"), "got:\n{printed}");
    assert!(printed.contains("HEVC 2160p"), "got:\n{printed}");
    assert!(printed.contains("TrueHD 5.1 eng"), "got:\n{printed}");
    assert!(printed.contains("PGS eng"), "got:\n{printed}");
    // 7530s = 2:05:30, rendered h:mm:ss.
    assert!(printed.contains("2:05:30"), "runtime line, got:\n{printed}");
}

#[test]
fn print_stream_info_omits_the_runtime_when_unknown() {
    crate::strings::set_locale("en");
    let t = title(vec![video()], 0.0);
    let (_, printed) = capture(|| print_stream_info(&loud(), &t));
    assert!(
        !printed.contains("Duration"),
        "no runtime line, got:\n{printed}"
    );
}

// D4 parity: a mux that fails before its output opens prints no pre-mux note, in the CLI
// as in the GUI (engine.rs `a_mux_that_fails_before_open_logs_no_note`).
#[test]
fn a_mux_that_fails_before_open_prints_no_note() {
    crate::strings::set_locale("en");
    let dir = std::env::temp_dir().join(format!("fmkv-d4-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.mpg");
    crate::lossy::mpg_source_fixture(&src);
    let dest = format!("mp4://{}/missing/x.mp4", dir.display());
    let events = std::sync::Arc::new(super::CliMuxEvents::new(loud(), dest.clone(), false));
    let url = format!("mpg://{}", src.display());
    let opts = libfreemkv::MuxOptions {
        skip_errors: false,
        batch_sectors: 64,
        raw: false,
        selection: libfreemkv::StreamSelection::default(),
        title_index: 0,
    };
    let (res, printed) = capture(|| {
        libfreemkv::mux_url(
            &url,
            None,
            &dest,
            &opts,
            &libfreemkv::Ctx::default().with_events(events),
        )
    });
    let _ = std::fs::remove_dir_all(&dir);
    assert!(res.is_err(), "no such output directory");
    assert!(!printed.contains("left out"), "got:\n{printed}");
}

#[test]
fn print_excluded_names_the_tracks_the_container_cannot_carry() {
    crate::strings::set_locale("en");
    // TrueHD (unmappable audio) + PGS (bitmap subtitle) cannot ride in MP4.
    let t = title(
        vec![video(), audio(libfreemkv::Codec::TrueHd), subtitle()],
        7530.0,
    );
    let (_, printed) = capture(|| print_excluded(&loud(), "mp4:///out/x.mp4", &t));
    assert!(
        printed.contains("left out"),
        "the header warns, got:\n{printed}"
    );
    assert!(
        printed.contains("Track"),
        "each skip is named, got:\n{printed}"
    );

    // mkv:// carries everything: a no-op, whatever the title carries.
    let (_, none) = capture(|| print_excluded(&loud(), "mkv:///out/x.mkv", &t));
    assert!(none.is_empty(), "mkv:// prints nothing, got:\n{none}");

    // An all-mappable title (H.264 + AC-3) also prints nothing.
    let clean = title(
        vec![
            libfreemkv::Stream::Video(libfreemkv::VideoStream {
                pid: 0x1011,
                codec: libfreemkv::Codec::H264,
                resolution: libfreemkv::Resolution::R1080p,
                frame_rate: libfreemkv::FrameRate::F23_976,
                hdr: libfreemkv::HdrFormat::Sdr,
                color_space: libfreemkv::ColorSpace::Bt709,
                display_aspect: None,
                secondary: false,
                label: String::new(),
                measured_cicp: None,
            }),
            audio(libfreemkv::Codec::Ac3),
        ],
        7530.0,
    );
    let (_, empty) = capture(|| print_excluded(&loud(), "mp4:///out/x.mp4", &clean));
    assert!(
        empty.is_empty(),
        "a fully-mappable title is silent, got:\n{empty}"
    );
}

#[test]
fn on_output_opened_prints_the_stream_block_and_arms_the_clock() {
    crate::strings::set_locale("en");
    let t = title(vec![video(), audio(libfreemkv::Codec::TrueHd)], 60.0);
    let events = CliMuxEvents::new(loud(), "mkv:///out/x.mkv".into(), false);
    assert!(
        events.start().is_none(),
        "the clock is unset until the sink opens"
    );
    let (_, printed) = capture(|| events.on_output_opened(&t));
    assert!(
        printed.contains("Streams: 2"),
        "the stream block prints, got:\n{printed}"
    );
    assert!(
        events.start().is_some(),
        "on_output_opened arms the completion clock"
    );
}

#[test]
fn on_output_opened_prints_the_excluded_note_first_like_the_gui() {
    crate::strings::set_locale("en");
    // The GUI logs the note before the mux starts; the CLI's earliest title
    // hook is on_output_opened, so the note must lead everything it prints.
    let t = title(vec![video(), audio(libfreemkv::Codec::TrueHd)], 60.0);
    let events = CliMuxEvents::new(loud(), "mp4:///out/x.mp4".into(), false);
    let (_, printed) = capture(|| events.on_output_opened(&t));
    let note = printed.find("left out").expect("the note prints");
    let streams = printed.find("Streams: 2").expect("the stream block prints");
    let opening = printed
        .find("mp4:///out/x.mp4")
        .expect("the open line prints");
    assert!(
        note < streams && note < opening,
        "the excluded note leads, got:\n{printed}"
    );
}

#[test]
fn on_write_progress_is_silent_when_quiet() {
    // A `stdio://` rip routes to stderr and runs quiet — a progress repaint
    // must never interleave into the piped byte stream.
    let events = CliMuxEvents::new(Output::new(false, true), "stdio://".into(), false);
    let (_, printed) = capture(|| events.on_write_progress(1_000, 2_000));
    assert!(
        printed.is_empty(),
        "quiet suppresses progress, got:\n{printed}"
    );
}

#[test]
fn render_resolution_trace_maps_unlock_and_key_steps() {
    use libfreemkv::aacs::trace::{
        KeyNode, KeyOutcome, KeyStep, ResolutionTrace, UnlockOutcome, UnlockStep,
    };
    let mut trace = ResolutionTrace::new();
    trace.unlock.push(UnlockStep {
        who: "firmware".into(),
        outcome: UnlockOutcome::Unlocked,
    });
    trace.unlock.push(UnlockStep {
        who: "host-cert".into(),
        outcome: UnlockOutcome::NoUsableHostCert { mkb: Some(64) },
    });
    trace.keys.push(KeyStep {
        who: "keydb".into(),
        path: vec![
            KeyNode::MatchedDisc,
            KeyNode::FoundVuk,
            KeyNode::DerivedUnitKeys,
        ],
        outcome: KeyOutcome::Resolved,
        matched_entry: None,
        store_entries: None,
    });
    trace.keys.push(KeyStep {
        who: "online".into(),
        path: vec![KeyNode::NoEntry],
        outcome: KeyOutcome::NoKey,
        matched_entry: None,
        store_entries: None,
    });

    let lines = render_resolution_trace(&trace);
    assert_eq!(
        lines,
        vec![
            "unlock: firmware > UNLOCKED".to_string(),
            "unlock: host-cert > no usable host cert (MKBv64)".to_string(),
            "key: keydb > matched disc > found VUK > derived unit keys > RESOLVED".to_string(),
            "key: online > no entry > NO KEY".to_string(),
        ]
    );

    // An empty trace renders nothing.
    assert!(render_resolution_trace(&ResolutionTrace::new()).is_empty());
}
