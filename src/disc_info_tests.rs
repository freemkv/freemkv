use super::*;
use libfreemkv::disc::DiscRegion;

// `info` flag validation is the same for every URL scheme: `disc:// --typo`
// used to exit 1, but `iso://x.iso --typo` silently listed titles because that
// route scanned args for `--full` via `.any()`. Both now go through `parse_info_flags`.

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn an_unknown_info_flag_is_rejected() {
    for bad in ["--fulll", "--typo", "-z", "--verbse", "--no-such-thing"] {
        assert_eq!(
            parse_info_flags(&args(&[bad])),
            InfoParse::Unknown(bad.to_string()),
            "{bad} must be reported, not silently dropped"
        );
    }
}

#[test]
fn an_unknown_flag_is_rejected_even_after_valid_ones() {
    // The offending token must be the one named, not the first flag seen.
    assert_eq!(
        parse_info_flags(&args(&["--full", "--quiet", "--oops"])),
        InfoParse::Unknown("--oops".to_string())
    );
}

#[test]
fn known_info_flags_are_honoured() {
    let InfoParse::Ok(f) = parse_info_flags(&args(&[
        "--full",
        "--quiet",
        "--verbose",
        "--basic",
        "--keydb",
        "/tmp/k.cfg",
    ])) else {
        panic!("a list of valid flags must parse");
    };
    assert!(f.full && f.quiet && f.verbose && f.basic);
    assert_eq!(f.keydb.as_deref(), Some("/tmp/k.cfg"));
    // Short forms too.
    let InfoParse::Ok(f) = parse_info_flags(&args(&["-f", "-q", "-v", "-b"])) else {
        panic!("short forms must parse");
    };
    assert!(f.full && f.quiet && f.verbose && f.basic);
}

// Regression: value-taking flags used to consume the following token
// unconditionally, so `--keydb --full` set the keydb path to "--full"
// and dropped `--full`. A token starting with `-` is a flag, not a value.
#[test]
fn a_value_flag_does_not_swallow_the_flag_that_follows_it() {
    let InfoParse::Ok(f) = parse_info_flags(&args(&["--keydb", "--full"])) else {
        panic!("a missing --keydb value must not turn --full into one");
    };
    assert_eq!(f.keydb, None, "--full is a flag, not a keydb path");
    assert!(f.full, "--full was the user's request and must still apply");

    // Same for the two logging flags, whose values are consumed here only
    // so they are not mistaken for positionals.
    let InfoParse::Ok(f) = parse_info_flags(&args(&["--log-level", "--basic"])) else {
        panic!("a missing --log-level value must not eat --basic");
    };
    assert!(f.basic);
    let InfoParse::Ok(f) = parse_info_flags(&args(&["--log-file", "--quiet"])) else {
        panic!("a missing --log-file value must not eat --quiet");
    };
    assert!(f.quiet);

    // A real value is still a value — including a keydb path that merely
    // sits next to a flag.
    let InfoParse::Ok(f) = parse_info_flags(&args(&["--keydb", "/tmp/k.cfg", "--full"])) else {
        panic!("a well-formed list must still parse");
    };
    assert_eq!(f.keydb.as_deref(), Some("/tmp/k.cfg"));
    assert!(f.full);
}

#[test]
fn flag_values_are_not_mistaken_for_unknown_options() {
    // `--log-level 3` / `--log-file p.txt` are consumed by logging init; the
    // VALUE must be skipped, or it lands in the unknown-option branch.
    let InfoParse::Ok(f) = parse_info_flags(&args(&[
        "--log-level",
        "3",
        "--log-file",
        "p.txt",
        "--full",
    ])) else {
        panic!("logging flags and their values must be consumed");
    };
    assert!(f.full);
    assert!(f.verbose, "--log-level 3 widens stdout detail");
}

#[test]
fn help_is_reported_rather_than_printed_by_the_parser() {
    assert_eq!(parse_info_flags(&args(&["--help"])), InfoParse::Help);
    assert_eq!(parse_info_flags(&args(&["-h"])), InfoParse::Help);
}

#[test]
fn no_flags_is_all_defaults() {
    assert_eq!(parse_info_flags(&[]), InfoParse::Ok(Box::default()));
}

use libfreemkv::{
    AudioChannels, ColorSpace, ContentFormat, DiscFormat, DiscTitle, FrameRate, HdrFormat,
    LabelPurpose, LabelQualifier, Resolution, SampleRate,
};

// A minimal synthetic encrypted disc with one rich title, mirroring a
// keyless ISO scan: titles populated, no AACS key resolved.
fn synthetic_disc() -> Disc {
    let video = Stream::Video(VideoStream {
        pid: 0x1011,
        codec: Codec::Hevc,
        resolution: Resolution::Unknown,
        frame_rate: FrameRate::Unknown,
        hdr: HdrFormat::Sdr,
        color_space: ColorSpace::Bt709,
        display_aspect: None,
        secondary: false,
        label: String::new(),
        measured_cicp: None,
    });
    let audio = Stream::Audio(AudioStream {
        pid: 0x1100,
        codec: Codec::TrueHd,
        channels: AudioChannels::Unknown,
        language: "eng".to_string(),
        sample_rate: SampleRate::Unknown,
        secondary: false,
        purpose: LabelPurpose::Normal,
        label: String::new(),
    });
    let subtitle = Stream::Subtitle(SubtitleStream {
        pid: 0x1200,
        codec: Codec::Pgs,
        language: "eng".to_string(),
        forced: false,
        qualifier: LabelQualifier::None,
        codec_data: None,
    });

    let title = DiscTitle {
        playlist: "00800.mpls".to_string(),
        playlist_id: 800,
        duration_secs: 7530.0, // 2h 05m
        size_bytes: 50 * 1024 * 1024 * 1024,
        clips: Vec::new(),
        streams: vec![video, audio, subtitle],
        chapters: Vec::new(),
        extents: Vec::new(),
        content_format: ContentFormat::BdTs,
        codec_privates: Vec::new(),
    };

    Disc {
        volume_id: "TEST_DISC".to_string(),
        meta_title: None,
        format: DiscFormat::Uhd,
        capacity_sectors: 0,
        capacity_bytes: 0,
        layers: 1,
        titles: vec![title],
        region: DiscRegion::Free,
        aacs: None, // no key resolved — exactly the `info iso://` keyless case
        css: None,
        encrypted: true,
        aacs_error: None,
        css_error: None,
        content_format: ContentFormat::BdTs,
    }
}

// `info iso://` names an AACS image's keydb lookup key without any key: the hash a
// key entry for the disc is written against.
#[test]
fn image_info_names_the_disc_hash_of_an_aacs_image() {
    let hash = "0xFEEDFACE00000000000000000000000000000000";
    let mut disc = synthetic_disc();
    disc.format = DiscFormat::BluRay;
    disc.aacs = Some(libfreemkv::test_util::aacs_state().disc_hash(hash).build());
    let flags = InfoFlags::default();
    let ((), text) = crate::output::capture(|| print_disc_titles(&disc, &flags));
    assert!(text.contains(&format!("Disc hash: {hash}")), "{text}");
    // A CSS or clear image has no such line.
    disc.aacs = None;
    disc.encrypted = false;
    let ((), text) = crate::output::capture(|| print_disc_titles(&disc, &flags));
    assert!(!text.contains("Disc hash"), "{text}");
}

#[test]
fn encryption_label_never_mislabels_a_failed_css_dvd_as_aacs() {
    let mut disc = synthetic_disc();
    // A CSS DVD whose title-key crack failed: encrypted, no css state, a
    // css_error recorded, format DVD, no aacs. Must render CSS, NOT AACS.
    disc.format = DiscFormat::Dvd;
    disc.encrypted = true;
    disc.css = None;
    disc.aacs = None;
    disc.css_error = Some(libfreemkv::Error::MkvInvalid); // any css-path error
    assert_eq!(encryption_label(&disc), Some(EncLabel::Css));

    // A resolved CSS DVD → CSS.
    disc.css_error = None;
    // (css stays None in the fixture; a resolved DVD would set css=Some, but
    // the css_error path above is the regression case. Verify the AACS paths.)

    // A UHD carrier → AACS 2.0.
    let mut uhd = synthetic_disc();
    uhd.format = DiscFormat::Uhd;
    uhd.encrypted = true;
    uhd.css = None;
    uhd.css_error = None;
    assert_eq!(
        encryption_label(&uhd),
        Some(EncLabel::Aacs("AACS 2.0".to_string()))
    );

    // An FMTS carrier → AACS 2.1.
    uhd.format = DiscFormat::Fmts;
    assert_eq!(
        encryption_label(&uhd),
        Some(EncLabel::Aacs("AACS 2.1".to_string()))
    );

    // Unencrypted → no line.
    uhd.encrypted = false;
    assert_eq!(encryption_label(&uhd), None);

    // The case `is_aacs_format` exists for: encrypted, no CSS signal, no `aacs`
    // struct, not an AACS carrier. That's generic unresolved encryption, not
    // "AACS 1.0" — labelling it AACS would claim detection of something specific.
    let mut unknown = synthetic_disc();
    unknown.format = DiscFormat::Dvd;
    unknown.encrypted = true;
    unknown.css = None;
    unknown.css_error = None;
    unknown.aacs = None;
    assert_eq!(encryption_label(&unknown), Some(EncLabel::GenericAacs));

    // Same disc on a real AACS carrier IS an AACS label — so the
    // distinction is the format, not the missing `aacs` struct.
    unknown.format = DiscFormat::BluRay;
    assert_eq!(
        encryption_label(&unknown),
        Some(EncLabel::Aacs("AACS 1.0".to_string()))
    );

    // And every carrier format is recognised as one.
    for f in [
        DiscFormat::BluRay,
        DiscFormat::Uhd,
        DiscFormat::Fmts,
        DiscFormat::HdDvd,
    ] {
        let mut d = synthetic_disc();
        d.format = f;
        assert!(super::is_aacs_format(&d), "{f:?} is an AACS carrier");
    }
    let mut dvd = synthetic_disc();
    dvd.format = DiscFormat::Dvd;
    assert!(!super::is_aacs_format(&dvd), "a DVD is not an AACS carrier");
}

#[test]
fn sanitize_strips_terminal_escape_sequences() {
    // Untrusted on-disc strings (title, volume label, stream label, language)
    // must have control/escape bytes stripped before printing, so a crafted
    // disc cannot inject terminal escapes (color/cursor/OSC).
    let hostile = "Ti\x1b[2Jtle\x07\x1b]0;pwn\x1b\\";
    let clean = sanitize(hostile);
    assert!(
        !clean.contains('\x1b') && !clean.contains('\x07'),
        "control/escape chars stripped, got {clean:?}"
    );
    assert_eq!(clean, "Ti[2Jtle]0;pwn\\", "printable text preserved");
    // The lang_name fallback for an unrecognized code sanitizes too.
    assert!(!lang_name("\x1b[31mzz").contains('\x1b'));
    // Unicode format (Cf) chars — bidi override, zero-width, BOM — are
    // stripped too (char::is_control does NOT catch these).
    let bidi = "abc\u{202E}gnp\u{200B}\u{FEFF}xyz";
    let cleaned = sanitize(bidi);
    assert_eq!(cleaned, "abcgnpxyz", "bidi/zero-width/BOM stripped");
}

#[test]
fn title_lines_count_clips_only_for_titles_made_of_them() {
    let mut disc = synthetic_disc();
    let row = |d: &Disc| title_lines(d, false, false, true)[2].clone();
    assert!(
        !row(&disc).contains(&strings::get("disc.clips")),
        "{}",
        row(&disc)
    );
    let clip = libfreemkv::Clip {
        clip_id: "00001".into(),
        in_time: 0,
        out_time: 0,
        duration_secs: 0.0,
        source_packets: 0,
        feed_span: None,
    };
    disc.titles[0].clips = vec![clip; 3];
    assert!(row(&disc).ends_with(&format!("3 {}", strings::get("disc.clips"))));
}

#[test]
fn title_lines_lists_encrypted_disc_without_key() {
    // The bug: `info iso://<encrypted>` returned E7022 and listed no titles
    // because it went through the key-gated `input()`. The keyless title
    // list must render the title with its streams and never emit E7022.
    let disc = synthetic_disc();
    let lines = title_lines(&disc, false, false, false);
    let joined = lines.join("\n");

    assert!(
        !joined.contains("E7022"),
        "title list must not surface the no-key error, got:\n{joined}"
    );
    // The title row (playlist + duration) is present.
    assert!(
        joined.contains("00800.mpls"),
        "expected the title's playlist, got:\n{joined}"
    );
    assert!(
        joined.contains("2h 05m"),
        "expected the formatted duration, got:\n{joined}"
    );
    // Stream rows are present (rich per-title output, not just the row).
    assert!(
        joined.contains("HEVC"),
        "expected the video codec, got:\n{joined}"
    );
    assert!(
        joined.contains("English"),
        "expected the audio/subtitle language, got:\n{joined}"
    );
}

#[test]
fn scan_failed_substitutes_detail_and_drops_no_placeholder() {
    // Regression: the handler keyed the format arg "error" while `error.scan_failed`
    // uses `{detail}`, so the real cause was dropped and users saw the literal
    // `{detail}`. Must key "detail" and route the cause through `fmt_err`.
    let rendered = strings::fmt(
        "error.scan_failed",
        &[(
            "detail",
            &crate::pipe::fmt_err(&libfreemkv::Error::DeviceNotReady {
                path: "/dev/sr0".to_string(),
            }),
        )],
    );
    assert!(
        !rendered.contains("{detail}"),
        "placeholder must be substituted, got:\n{rendered}"
    );
    // WS2: the cause routes through `fmt_err`, which now PREFIXES the
    // language-neutral `E<code>` token (code-forward) ahead of the
    // localized message — the code is shown, not stripped.
    assert!(
        rendered.contains("E1002"),
        "expected the code-forward E1002 token, got:\n{rendered}"
    );
    assert!(
        rendered.starts_with("Scan failed:") && rendered.len() > "Scan failed:".len() + 1,
        "expected the cause appended after the prefix, got:\n{rendered}"
    );
}

#[test]
fn open_failure_renders_through_fmt_err_shows_code() {
    // Regression: the open-failure handler did `eprintln!("{}", e)`, printing
    // libfreemkv's raw `E####: <data>` and bypassing the i18n renderer. Must route
    // through `pipe::fmt_err`, which localizes and shows `E<code>` as a code-forward prefix.
    let rendered = crate::pipe::fmt_err(&libfreemkv::Error::DevicePermission {
        path: "/dev/sg0".to_string(),
    });
    assert!(
        rendered.starts_with("E1001 "),
        "expected the code-forward E1001 prefix, got:\n{rendered}"
    );
    assert!(
        rendered.contains("/dev/sg0"),
        "expected the device path in the localized message, got:\n{rendered}"
    );
    // The localized E1001 text names the actionable fix (disk group / privileges).
    assert!(
        rendered.to_lowercase().contains("disk group")
            || rendered.to_lowercase().contains("privile"),
        "expected the actionable remediation text, got:\n{rendered}"
    );
}

#[test]
fn title_lines_empty_disc_reports_no_titles() {
    let mut disc = synthetic_disc();
    disc.titles.clear();
    let lines = title_lines(&disc, true, false, false);
    assert_eq!(lines, vec![strings::get("disc.no_titles")]);
}

#[test]
fn title_lines_basic_omits_streams() {
    // `--basic` shows only the title row, no stream detail.
    let disc = synthetic_disc();
    let joined = title_lines(&disc, false, false, true).join("\n");
    assert!(joined.contains("00800.mpls"));
    assert!(
        !joined.contains("HEVC"),
        "basic mode must omit stream rows, got:\n{joined}"
    );
}

#[test]
fn label_alignment_preserves_english_layout() {
    // The historical English layout put every stream value at column 17
    // (`Subtitle` is the widest label at 8 chars: 6 + 8 + 1 + 2 = 17). The
    // derived indent must reproduce that exactly so nothing shifts.
    assert_eq!(label_indent("Subtitle"), 17);
    assert_eq!(label_indent("Video"), 14);
    assert_eq!(label_indent("Audio"), 14);

    // First-line prefixes pad to the shared (max) indent of 17, matching the
    // old hardcoded `      Video:     ` / `      Subtitle:  ` strings.
    assert_eq!(label_prefix("Video", 17), "      Video:     ");
    assert_eq!(label_prefix("Subtitle", 17), "      Subtitle:  ");
}

#[test]
fn label_alignment_holds_for_longer_localized_label() {
    // A longer localized subtitle label (German `Untertitel`, Italian
    // `Sottotitoli`) must drive a wider shared indent instead of overrunning
    // a hardcoded 17-space continuation. The value column tracks the label.
    let indent = label_indent("Sottotitoli"); // 6 + 11 + 1 + 2 = 20
    assert_eq!(indent, 20);
    let prefix = label_prefix("Sottotitoli", indent);
    assert_eq!(prefix.chars().count(), indent);
    assert!(prefix.starts_with("      Sottotitoli:"));
    assert!(prefix.ends_with("  "));
}

// Pure formatters (665-806): every `disc-info` line is one of these functions'
// return value. Pure string builders, but nothing exercised their branches, so a
// mutant swapping "DD+" for "DD" or dropping `[PID …]` passed CI. Locale pinned here.

fn video(codec: Codec, resolution: Resolution) -> VideoStream {
    VideoStream {
        pid: 0x1011,
        codec,
        resolution,
        frame_rate: FrameRate::Unknown,
        hdr: HdrFormat::Sdr,
        color_space: ColorSpace::Bt709,
        display_aspect: None,
        secondary: false,
        label: String::new(),
        measured_cicp: None,
    }
}

fn audio(codec: Codec, channels: AudioChannels, language: &str) -> AudioStream {
    AudioStream {
        pid: 0x1100,
        codec,
        channels,
        language: language.to_string(),
        sample_rate: SampleRate::Unknown,
        secondary: false,
        purpose: LabelPurpose::Normal,
        label: String::new(),
    }
}

fn subtitle(language: &str) -> SubtitleStream {
    SubtitleStream {
        pid: 0x1200,
        codec: Codec::Pgs,
        language: language.to_string(),
        forced: false,
        qualifier: LabelQualifier::None,
        codec_data: None,
    }
}

#[test]
fn format_video_assembles_codec_resolution_and_only_the_flags_present() {
    strings::set_locale("en");
    // Bare: codec + resolution, no fps (Unknown), no HDR (SDR), no BT.2020.
    let v = video(Codec::Hevc, Resolution::R1080p);
    assert_eq!(format_video(&v, false), "HEVC 1080p");
    // Verbose appends the PID, upper-case hex, zero-padded to four digits.
    assert_eq!(format_video(&v, true), "HEVC 1080p [PID 0x1011]");

    // Frame rate, HDR name and BT.2020 each add exactly one part when set.
    let mut rich = video(Codec::Hevc, Resolution::R2160p);
    rich.frame_rate = FrameRate::F23_976;
    rich.hdr = HdrFormat::DolbyVision;
    rich.color_space = ColorSpace::Bt2020;
    assert_eq!(
        format_video(&rich, false),
        "HEVC 2160p 23.976fps Dolby Vision BT.2020"
    );

    // A secondary Dolby Vision stream is the enhancement layer (localized).
    let mut el = video(Codec::Hevc, Resolution::R2160p);
    el.secondary = true;
    el.hdr = HdrFormat::DolbyVision;
    assert_eq!(
        format_video(&el, false),
        "HEVC 2160p Dolby Vision Dolby Vision EL"
    );

    // A secondary non-DV stream with a label shows the sanitized label.
    let mut labelled = video(Codec::H264, Resolution::R1080p);
    labelled.secondary = true;
    labelled.label = "PiP\x1b[2J".to_string();
    let out = format_video(&labelled, false);
    assert!(out.contains("PiP") && !out.contains('\x1b'), "got {out}");
}

#[test]
fn format_audio_renders_lang_codec_channels_and_tags() {
    strings::set_locale("en");
    let a = audio(Codec::TrueHd, AudioChannels::Surround51, "eng");
    assert_eq!(format_audio(&a, false), "English TrueHD 5.1");
    // Verbose inserts sample rate and PID before the tags.
    assert_eq!(
        format_audio(&a, true),
        "English TrueHD 5.1 unknown [PID 0x1100]"
    );

    // Purpose + secondary + label collect into one parenthesised group.
    let mut tagged = audio(Codec::Ac3, AudioChannels::Stereo, "fra");
    tagged.purpose = LabelPurpose::Commentary;
    tagged.secondary = true;
    tagged.label = "Director".to_string();
    assert_eq!(
        format_audio(&tagged, false),
        "French DD stereo (Commentary, Secondary, Director)"
    );
}

#[test]
fn format_subtitle_renders_lang_forced_and_qualifier() {
    strings::set_locale("en");
    assert_eq!(format_subtitle(&subtitle("eng"), false), "English");
    assert_eq!(
        format_subtitle(&subtitle("eng"), true),
        "English [PID 0x1200]"
    );

    let mut forced = subtitle("jpn");
    forced.forced = true;
    forced.qualifier = LabelQualifier::Sdh;
    assert_eq!(format_subtitle(&forced, false), "Japanese (forced, SDH)");
}

#[test]
fn region_name_covers_free_bluray_and_dvd() {
    assert_eq!(region_name(&DiscRegion::Free), "Region-free");
    // An empty BD list reads as region-free; an empty DVD list is a mask
    // prohibiting every region.
    assert_eq!(region_name(&DiscRegion::BluRay(vec![])), "Region-free");
    assert_eq!(region_name(&DiscRegion::Dvd(vec![])), "None");
    assert_eq!(region_name(&DiscRegion::Unknown), "Unknown");
    assert_eq!(
        region_name(&DiscRegion::BluRay(vec![
            BdRegion::A,
            BdRegion::B,
            BdRegion::C
        ])),
        "A/B/C"
    );
    assert_eq!(region_name(&DiscRegion::Dvd(vec![1, 2])), "1, 2");
}

#[test]
fn hex_bytes_is_lower_case_two_digit_no_separator() {
    assert_eq!(hex_bytes(&[]), "");
    assert_eq!(hex_bytes(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
}

#[test]
fn codec_name_maps_the_special_cases_and_falls_through_to_name() {
    assert_eq!(codec_name(Codec::Ac3), "DD");
    assert_eq!(codec_name(Codec::Ac3Plus), "DD+");
    assert_eq!(codec_name(Codec::DvdSub), "DVD Sub");
    assert_eq!(codec_name(Codec::Unknown(0x1b)), "0x1b");
    // Everything else defers to the library's own display name.
    assert_eq!(codec_name(Codec::Hevc), "HEVC");
    assert_eq!(codec_name(Codec::TrueHd), "TrueHD");
    assert_eq!(codec_name(Codec::Pgs), "PGS");
}

#[test]
fn aacs_generation_labels_the_carrier_and_falls_back_to_the_minor() {
    let mut disc = synthetic_disc();
    disc.format = DiscFormat::Fmts;
    assert_eq!(aacs_generation(&disc), "AACS 2.1");
    disc.format = DiscFormat::Uhd;
    assert_eq!(aacs_generation(&disc), "AACS 2.0");
    // Non-carrier with no aacs struct falls back to 1.0.
    disc.format = DiscFormat::BluRay;
    disc.aacs = None;
    assert_eq!(aacs_generation(&disc), "AACS 1.0");
}

#[test]
fn lang_name_resolves_codes_sanitizes_unknowns_and_marks_empty() {
    assert_eq!(lang_name("eng"), "English");
    assert_eq!(lang_name("en"), "English"); // 639-1 fallback
    assert_eq!(lang_name(""), "?");
    // An unrecognized code is returned raw, but sanitized of escapes.
    let out = lang_name("\x1b[31mzz");
    assert!(!out.contains('\x1b'), "got {out}");
}

#[test]
fn format_volume_id_title_cases_underscored_labels() {
    assert_eq!(format_volume_id("THE_MOVIE_2020"), "The Movie 2020");
    assert_eq!(format_volume_id("bluray_disc"), "Bluray Disc");
    assert_eq!(format_volume_id(""), "");
}

#[test]
fn title_lines_truncates_to_five_and_footers_the_remainder() {
    // A disc with more titles than the non-`--full` cap (5) lists exactly
    // five and closes with the localized "+N more" footer. `synthetic_disc`
    // has one title, so this many-title footer path had no coverage.
    strings::set_locale("en");
    let mut disc = synthetic_disc();
    let one = disc.titles[0].clone();
    disc.titles = std::iter::repeat_n(one, 8).collect();
    let joined = title_lines(&disc, false, false, false).join("\n");
    // Five rows shown (1..=5), the sixth is not.
    assert!(joined.contains("  1. "), "first title shown: {joined}");
    assert!(joined.contains("  5. "), "fifth title shown: {joined}");
    assert!(!joined.contains("  6. "), "sixth title truncated: {joined}");
    // The footer names the 3 remaining.
    assert!(
        joined.contains('3') && joined.to_lowercase().contains("more"),
        "expected the +N more footer, got: {joined}"
    );
    // `--full` shows every title and prints no footer.
    let full = title_lines(&disc, true, false, false).join("\n");
    assert!(full.contains("  8. "), "full lists the eighth: {full}");
}

#[test]
fn title_lines_aligns_continuation_rows_for_multi_stream_groups() {
    // A title with two of each stream kind exercises the `vi/ai/si > 0`
    // continuation-line arms (the indented rows with no label prefix), which
    // the single-stream `synthetic_disc` never reaches.
    strings::set_locale("en");
    let mut disc = synthetic_disc();
    let title = &mut disc.titles[0];
    title.streams = vec![
        Stream::Video(video(Codec::Hevc, Resolution::R2160p)),
        Stream::Video(video(Codec::H264, Resolution::R1080p)),
        Stream::Audio(audio(Codec::TrueHd, AudioChannels::Surround71, "eng")),
        Stream::Audio(audio(Codec::Ac3, AudioChannels::Stereo, "fra")),
        Stream::Subtitle(subtitle("eng")),
        Stream::Subtitle(subtitle("jpn")),
    ];
    let joined = title_lines(&disc, true, false, false).join("\n");
    // Both members of each group render; the second of each is a
    // continuation row (present, distinct language/codec).
    assert!(
        joined.contains("HEVC") && joined.contains("H.264"),
        "{joined}"
    );
    assert!(
        joined.contains("English") && joined.contains("French"),
        "{joined}"
    );
    assert!(joined.contains("Japanese"), "{joined}");
}

// `info -v` is pasted into bug reports: planted key bytes must never render,
// while the non-secret facts (source, count, hash, MKB, VID) still do.
#[test]
fn the_aacs_block_never_renders_key_material() {
    let aacs = libfreemkv::test_util::aacs_state()
        .mkb_version(Some(77))
        .disc_hash("0xfeedface")
        .volume_id([0x9C; 16])
        .uk_ro(vec![0xEE; 16])
        .mkb(vec![0x11; 16])
        .build();
    // KU §3.3: `info` reads the set's `status()` — the count is the set's, never
    // the banked keys (KU §11.6: "The key count in `info` … comes from the resolved set").
    let mut status = libfreemkv::keys::KeyRing::none().status();
    status.proven = 1;
    status.origin = Some("keydb");
    let ((), text) = crate::output::capture(|| {
        emit_aacs_block(&Output::new(true, false), &aacs, Some(&status));
    });
    let lower = text.to_ascii_lowercase();
    for secret in ["eeeeeeee", "11111111"] {
        assert!(
            !lower.contains(secret),
            "key bytes {secret} leaked:\n{text}"
        );
    }
    assert!(!text.contains("VUK") && !text.contains("CPS"), "{text}");
    assert!(text.contains("Keys: keydb (1 unit keys)"), "{text}");
    // An HD DVD set keyed without proof says so.
    status.best_effort = true;
    let ((), text) = crate::output::capture(|| {
        emit_aacs_block(&Output::new(true, false), &aacs, Some(&status));
    });
    assert!(text.contains("unverified"), "{text}");
    let ((), text) = crate::output::capture(|| {
        emit_aacs_block(&Output::new(true, false), &aacs, None);
    });
    assert!(text.contains("Keys: none (0 unit keys)"), "{text}");
    assert!(text.contains("Disc hash: 0xfeedface"), "{text}");
    assert!(text.contains("MKB v77"), "{text}");
    assert!(text.contains("VID: 0x9c9c"), "{text}");
}
