use super::{excluded_lines, is_lossy, keep_for, lossy_lines, lost_mb};

fn title(streams: Vec<libfreemkv::Stream>) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        streams,
        ..libfreemkv::DiscTitle::empty()
    }
}
fn video(codec: libfreemkv::Codec) -> libfreemkv::Stream {
    libfreemkv::Stream::Video(libfreemkv::VideoStream {
        pid: 0x1011,
        codec,
        resolution: libfreemkv::Resolution::R1080p,
        frame_rate: libfreemkv::FrameRate::F23_976,
        hdr: libfreemkv::HdrFormat::Sdr,
        color_space: libfreemkv::ColorSpace::Bt709,
        display_aspect: None,
        secondary: false,
        label: String::new(),
        measured_cicp: None,
    })
}
fn audio(pid: u16, codec: libfreemkv::Codec) -> libfreemkv::Stream {
    libfreemkv::Stream::Audio(libfreemkv::AudioStream {
        pid,
        codec,
        channels: libfreemkv::AudioChannels::Stereo,
        language: "eng".into(),
        sample_rate: libfreemkv::SampleRate::S48,
        secondary: false,
        purpose: libfreemkv::LabelPurpose::Normal,
        label: String::new(),
    })
}

// G4 (mpg-output-design v5 §1.1, §5): one container-neutral pre-mux note for every
// destination, from libfreemkv's generic fit plan; `{keep}` is mkv unless every exclusion
// is the MPEG-2 extension.
#[test]
fn excluded_lines_name_the_container_and_each_track() {
    crate::strings::set_locale("en");
    let t = title(vec![
        video(libfreemkv::Codec::Hevc),
        audio(0x1100, libfreemkv::Codec::TrueHd),
        audio(0x1101, libfreemkv::Codec::Ac3),
    ]);
    let lines = excluded_lines("mp4:///o/x.mp4", &t);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].contains("MP4") && lines[0].contains("mkv://") && lines[0].contains('1'),
        "{lines:?}"
    );
    assert!(
        lines[1].contains(" 2:") && lines[1].contains("MP4"),
        "track 2, reason: {lines:?}"
    );
    let mpg = excluded_lines("mpg:///o/x.mpg", &t);
    assert_eq!(mpg.len(), 3, "HEVC and TrueHD, not the AC-3 (J24): {mpg:?}");
    assert!(mpg[0].contains("MPG"), "{mpg:?}");
    assert!(
        excluded_lines("mkv:///o/x.mkv", &t).is_empty(),
        "mkv keeps everything"
    );
}

// J23: "the generic pre-mux fit report leaves out declared-only extension tracks".
#[test]
fn j23_a_declared_only_mp2_extension_is_never_in_the_excluded_note() {
    crate::strings::set_locale("en");
    let main = vec![
        video(libfreemkv::Codec::Mpeg2),
        audio(0xC0, libfreemkv::Codec::Mp2),
    ];
    let mut with_ext = main.clone();
    with_ext.push(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
        pid: 0xD0,
        codec: libfreemkv::Codec::Mp2,
        channels: libfreemkv::AudioChannels::Stereo,
        language: "eng".into(),
        sample_rate: libfreemkv::SampleRate::S48,
        secondary: false,
        purpose: libfreemkv::LabelPurpose::Normal,
        label: libfreemkv::disc::MP2_EXTENSION_LABEL.into(),
    }));
    for dest in ["mkv:///o/x.mkv", "mp4:///o/x.mp4", "mpg:///o/x.mpg"] {
        let lines = excluded_lines(dest, &title(with_ext.clone()));
        assert!(
            !lines.iter().any(|l| l.contains(" 3:")),
            "{dest}: the declared-only extension is listed: {lines:?}"
        );
        assert_eq!(
            lines,
            excluded_lines(dest, &title(main.clone())),
            "{dest}: the extension changed the note"
        );
    }
}

#[test]
fn keep_is_mpg_only_when_every_exclusion_is_the_extension() {
    use libfreemkv::SkipReason as R;
    assert_eq!(keep_for(&[R::Mp2Extension]), "mpg");
    assert_eq!(keep_for(&[R::Mp2Extension, R::UnmappableAudio]), "mkv");
    assert_eq!(keep_for(&[R::BitmapSubtitle]), "mkv");
}

/// Every pre-mux reason renders its own resolving line (the retired
/// `mp4_skip_reasons_render_distinct_resolving_strings`, now container-neutral).
#[test]
fn skip_reasons_render_distinct_resolving_strings() {
    use libfreemkv::SkipReason as R;
    crate::strings::set_locale("en");
    let mut seen = std::collections::BTreeSet::new();
    for r in [
        R::BitmapSubtitle,
        R::UnmappableAudio,
        R::SecondaryVideo,
        R::UnmappableVideo,
        R::Mp2Extension,
        R::NoStreamId,
    ] {
        let key = super::reason_key(r);
        let msg = crate::strings::fmt(key, &[("container", "MPG")]);
        assert!(!msg.starts_with("mux."), "{r:?}: {key} does not resolve");
        assert!(!msg.contains('{'), "{r:?}: unfilled placeholder in {msg:?}");
        assert!(seen.insert(msg.clone()), "{r:?}: duplicate message {msg:?}");
    }
    assert_eq!(
        super::reason_key(R::UnmappableSubtitle),
        super::reason_key(R::BitmapSubtitle)
    );
    assert!(!crate::strings::get("mux.excluded_header").starts_with("mux."));
}

fn outcome(undelivered: Vec<usize>, errors: u64, lost_bytes: u64) -> libfreemkv::MuxOutcome {
    libfreemkv::MuxOutcome {
        halted: false,
        completed: true,
        output_opened: true,
        bytes_written: 4 << 30,
        errors,
        lost_bytes,
        streams: 3,
        undelivered_streams: undelivered,
    }
}

/// The clean run says nothing. An unconditional warning is worse than none:
/// it trains the user to ignore the line that matters.
#[test]
fn a_mux_that_lost_nothing_produces_no_lines() {
    let o = outcome(Vec::new(), 0, 0);
    assert!(lossy_lines(&o, "/out/movie.mkv").is_empty());
    assert!(!is_lossy(&o));
}

/// The dependent-view case: no stream is missing, and 3 MB of payload is.
#[test]
fn dropped_payload_bytes_are_named_with_their_size_and_file() {
    crate::strings::set_locale("en");
    let o = outcome(Vec::new(), 2, 3 << 20);
    let lines = lossy_lines(&o, "/out/movie.mkv");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("/out/movie.mkv"), "{lines:?}");
    assert!(lines[0].contains("3.00"), "{lines:?}");
    assert!(lines[0].contains("lost"), "{lines:?}");
    assert!(is_lossy(&o));
}

/// Lost bytes alone, with no error count, are still a loss and still named.
#[test]
fn lost_bytes_without_an_error_count_are_still_reported() {
    crate::strings::set_locale("en");
    let o = outcome(Vec::new(), 0, 3 << 20);
    let lines = lossy_lines(&o, "/out/movie.mkv");
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("3.00"), "{lines:?}");
    assert!(is_lossy(&o));
}

/// Both losses at once are both reported — one is not a substitute for the
/// other, and the tracks come first because they are the coarser fact.
#[test]
fn a_dropped_track_and_dropped_bytes_are_both_reported() {
    crate::strings::set_locale("en");
    let lines = lossy_lines(&outcome(vec![1], 1, 1 << 20), "/out/movie.mp4");
    assert_eq!(lines.len(), 3, "header, the track, the bytes: {lines:?}");
    assert!(lines[1].ends_with(" 2"), "1-based track number: {lines:?}");
    assert!(lines[2].contains("lost"), "{lines:?}");
}

/// A loss under a megabyte must not render as "0.00 MB lost" — the number
/// would contradict the line it is in.
#[test]
fn a_sub_megabyte_loss_never_rounds_away_to_nothing() {
    assert_eq!(lost_mb(0), "0.00");
    assert_eq!(lost_mb(1), "0.01", "one byte is still a loss");
    // A hundredth of a MiB is 10485.76 bytes, so 10485 still rounds up to
    // one hundredth and anything past it to two — UP, never down.
    assert_eq!(lost_mb(10_485), "0.01");
    assert_eq!(lost_mb(10_486), "0.02", "rounds UP, never down");
    assert_eq!(lost_mb(1 << 20), "1.00");
    assert_eq!(lost_mb(3 << 20), "3.00");
    assert_eq!(lost_mb(1_073_741_824), "1024.00");
}

/// Undelivered streams are not an MP4 problem: an mkv/m2ts loss must not
/// be told its tracks "can't be stored in an MP4".
#[test]
fn the_undelivered_header_does_not_blame_mp4_for_other_containers() {
    crate::strings::set_locale("en");
    for target in ["/out/movie.mkv", "/out/movie.m2ts"] {
        let lines = lossy_lines(&outcome(vec![0], 0, 0), target);
        assert_eq!(lines.len(), 2, "header and the track: {lines:?}");
        assert!(!lines[0].to_lowercase().contains("mp4"), "{lines:?}");
        assert!(lines[0].contains('1'), "the count is named: {lines:?}");
    }
}

/// `errors` without a byte count is still a loss the user must see.
#[test]
fn a_skip_event_with_no_byte_total_still_reports() {
    crate::strings::set_locale("en");
    let o = outcome(Vec::new(), 1, 0);
    assert!(is_lossy(&o));
    assert_eq!(lossy_lines(&o, "/out/movie.mkv").len(), 1);
}
