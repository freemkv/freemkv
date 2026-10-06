// A value-taking logging flag must not swallow the NEXT FLAG as its value (e.g. --log-file
// --raw would eat --raw and run without it).
#[test]
fn a_logging_flag_does_not_swallow_the_following_flag() {
    for flag in ["--log-file", "--log-level"] {
        let args: Vec<String> = [flag, "--raw", "disc://", "iso:///out/d.iso"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (level, log_file, _) = super::parse_logging_flags(&args);
        assert!(
            log_file.as_deref() != Some("--raw"),
            "{flag} took the following FLAG as its value; the rip then runs \
                 without --raw and silently writes a decrypted image"
        );
        // And the flag must still be visible to the parser that wants it.
        assert!(
            args.iter().any(|a| a == "--raw"),
            "fixture invariant: --raw must still be in the argv"
        );
        let _ = level;
    }

    // A rejected value must stay AVAILABLE, not be eaten by the peek: the
    // sibling logging flag after it must still parse. `next_if` leaves the
    // token in place; a plain `next().filter(..)` would have consumed it.
    let args: Vec<String> = ["--log-file", "--log-level", "3"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (level, log_file, _) = super::parse_logging_flags(&args);
    assert_eq!(log_file, None, "--log-file had no value to take");
    assert_eq!(
        level,
        Some(3),
        "--log-level was swallowed by the flag before it"
    );

    // And the reverse: a rejected --log-level value leaves --log-file intact.
    let args: Vec<String> = ["--log-level", "--log-file", "a.log"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (level, log_file, _) = super::parse_logging_flags(&args);
    assert_eq!(level, None, "--log-level had no value to take");
    assert_eq!(
        log_file.as_deref(),
        Some("a.log"),
        "--log-file was swallowed by --log-level"
    );
}

// `rolling::never` panicked on a log it could not open; the path is reported instead.
#[test]
fn an_unopenable_log_file_is_none_instead_of_a_panic() {
    let dir = std::env::temp_dir().join(format!("freemkv-logfile-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("as-dir.log")).unwrap();
    assert!(super::open_log_file(dir.join("as-dir.log").to_str().unwrap()).is_none());
    std::fs::write(dir.join("file"), b"").unwrap();
    assert!(super::open_log_file(dir.join("file/x.log").to_str().unwrap()).is_none());
    let ok = dir.join("ok.log");
    assert!(super::open_log_file(ok.to_str().unwrap()).is_some());
    std::fs::remove_dir_all(&dir).unwrap();
}

use super::{SUBCOMMANDS, collect_urls, stream_info_lines, update_keys_dest};

// A missing source reads as the OS reports it, not as E5000 and its destination advice.
#[test]
fn info_on_a_missing_source_names_the_os_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.mkv");
    let url = format!("mkv://{}", path.display());
    let Err(e) = freemkv_engine::stream_info(&url, None, &libfreemkv::Halt::new()) else {
        panic!("{url} opened");
    };
    let cause = super::info_failure_cause(&e);
    let missing = std::fs::File::open(&path).unwrap_err();
    assert_eq!(cause, crate::pipe::fmt_err(&missing));
    assert!(!cause.contains("E5000"), "{cause}");
}

// G10 (design §1.1): `info` lists the streams of every stream container, mpg:// and
// mp4:// included.
#[test]
fn info_reads_every_stream_container() {
    for url in [
        "mkv:///m/a.mkv",
        "m2ts:///m/a.m2ts",
        "mp4:///m/a.mp4",
        "mpg:///m/a.mpg",
    ] {
        assert!(
            super::info_lists_streams(&libfreemkv::parse_url(url)),
            "{url}"
        );
    }
    assert!(!super::info_lists_streams(&libfreemkv::parse_url(
        "json:///m/a.json"
    )));
}

// M1b (mpg-output-design v5 §3): "`info` (`stream_info_lines`) and `json://` list it
// with the same label." The DVD MPEG-2 extension track shows its sentinel label raw.
fn mp2_extension_pair() -> Vec<libfreemkv::Stream> {
    use libfreemkv::{AudioChannels, AudioStream, Codec, LabelPurpose, SampleRate, Stream};
    let mk = |pid: u16, label: &str| {
        Stream::Audio(AudioStream {
            pid,
            codec: Codec::Mp2,
            channels: AudioChannels::Stereo,
            language: "eng".into(),
            sample_rate: SampleRate::S48,
            secondary: false,
            purpose: LabelPurpose::Normal,
            label: label.into(),
        })
    };
    vec![
        mk(0xC0, ""),
        mk(0xD0, libfreemkv::disc::MP2_EXTENSION_LABEL),
    ]
}

#[test]
fn info_lists_an_mp2_extension_track_with_its_label() {
    crate::strings::set_locale("en");
    let lines = stream_info_lines(&mp2_extension_pair());
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(!lines[0].contains(libfreemkv::disc::MP2_EXTENSION_LABEL));
    assert!(
        lines[1].ends_with(&format!(" — {}", libfreemkv::disc::MP2_EXTENSION_LABEL)),
        "{lines:?}"
    );
}

#[test]
fn json_lists_an_mp2_extension_track_with_the_same_label() {
    let dir = std::env::temp_dir().join(format!("fmkv-m1b-json-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.json");
    let title = libfreemkv::DiscTitle {
        streams: mp2_extension_pair(),
        ..libfreemkv::DiscTitle::empty()
    };
    let url = format!("json://{}", path.display());
    let mut sink = libfreemkv::output(&url, &title, None).unwrap();
    sink.finish().unwrap();
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let audio = doc["audio"].as_array().expect("json:// lists audio");
    assert_eq!(audio.len(), 2, "{doc}");
    assert_eq!(audio[1]["pid"], 0xD0);
    // `TitleProfile`'s `AudioTrack.name` is the stream label.
    assert_eq!(audio[1]["name"], libfreemkv::disc::MP2_EXTENSION_LABEL);
    let _ = std::fs::remove_dir_all(&dir);
}

// Covers the tag-assembly arms of stream_info_lines (purpose/secondary/ label) that the
// escape-stripping test below never reaches.
#[test]
fn stream_info_lines_render_purpose_secondary_and_label_tags() {
    use libfreemkv::{
        AudioChannels, AudioStream, Codec, ColorSpace, FrameRate, HdrFormat, LabelPurpose,
        LabelQualifier, Resolution, Stream, SubtitleStream, VideoStream,
    };
    crate::strings::set_locale("en");

    // Every purpose arm renders a distinct, non-empty tag.
    for (purpose, needle) in [
        (LabelPurpose::Commentary, "Commentary"),
        (LabelPurpose::Descriptive, "Descriptive"),
        (LabelPurpose::Score, "Score"),
    ] {
        let a = Stream::Audio(AudioStream {
            pid: 0x1100,
            codec: Codec::Ac3,
            channels: AudioChannels::Stereo,
            language: "eng".into(),
            sample_rate: SampleRate::S48,
            secondary: false,
            purpose,
            label: String::new(),
        });
        let line = stream_info_lines(&[a]).join("\n");
        assert!(line.contains(needle), "{purpose:?} → {line:?}");
    }

    // A secondary track with a codec-variant label: both the "Secondary"
    // tag and the label survive, joined in one parenthesised group.
    let tagged = Stream::Audio(AudioStream {
        pid: 0x1100,
        codec: Codec::TrueHd,
        channels: AudioChannels::Surround51,
        language: "fra".into(),
        sample_rate: SampleRate::S48,
        secondary: true,
        purpose: LabelPurpose::Commentary,
        label: "Atmos".into(),
    });
    let line = stream_info_lines(&[tagged]).join("\n");
    assert!(
        line.contains("Commentary") && line.contains("Secondary") && line.contains("Atmos"),
        "all three tags present: {line:?}"
    );

    // A video label renders after the resolution; a subtitle line carries
    // its language.
    let video = Stream::Video(VideoStream {
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
    });
    let sub = Stream::Subtitle(SubtitleStream {
        pid: 0x1200,
        codec: Codec::Pgs,
        language: "eng".into(),
        forced: false,
        qualifier: LabelQualifier::None,
        codec_data: None,
    });
    let lines = stream_info_lines(&[video, sub]);
    let joined = lines.join("\n");
    assert!(
        joined.contains("Feature"),
        "video label present: {joined:?}"
    );
    // The container `info` path prints the raw on-disc language tag
    // (sanitized), not a resolved name — unlike the disc-info listing.
    assert!(
        joined.contains("eng"),
        "subtitle language present: {joined:?}"
    );
}

use libfreemkv::SampleRate;

// v.label/a.label/a.language/s.language are disc/file-controlled; a crafted terminal escape
// in any must not survive to the terminal.
#[test]
fn stream_info_lines_strip_terminal_escapes_from_every_disc_controlled_field() {
    use libfreemkv::{
        AudioChannels, AudioStream, Codec, ColorSpace, FrameRate, HdrFormat, LabelPurpose,
        Resolution, SampleRate, SubtitleStream, VideoStream,
    };

    let hostile = "\x1b[2Jevil\x07";

    let video = libfreemkv::Stream::Video(VideoStream {
        pid: 0x1011,
        codec: Codec::Hevc,
        resolution: Resolution::Unknown,
        frame_rate: FrameRate::Unknown,
        hdr: HdrFormat::Sdr,
        color_space: ColorSpace::Bt709,
        display_aspect: None,
        secondary: false,
        label: hostile.to_string(),
        measured_cicp: None,
    });
    let audio = libfreemkv::Stream::Audio(AudioStream {
        pid: 0x1100,
        codec: Codec::TrueHd,
        channels: AudioChannels::Unknown,
        language: hostile.to_string(),
        sample_rate: SampleRate::Unknown,
        secondary: false,
        purpose: LabelPurpose::Normal,
        label: hostile.to_string(),
    });
    let subtitle = libfreemkv::Stream::Subtitle(SubtitleStream {
        pid: 0x1200,
        codec: Codec::Pgs,
        language: hostile.to_string(),
        forced: false,
        qualifier: libfreemkv::LabelQualifier::None,
        codec_data: None,
    });

    let lines = stream_info_lines(&[video, audio, subtitle]);
    let joined = lines.join("\n");
    assert!(
        !joined.contains('\x1b') && !joined.contains('\x07'),
        "control/escape chars must be stripped from every stream line, got {joined:?}"
    );
    // Sanity: the fix didn't just drop the field — the printable text
    // (still containing "evil") should survive, sanitized.
    assert!(
        joined.contains("evil"),
        "printable label text should survive sanitization, got {joined:?}"
    );
}

/// Walk every string value in a locale document.
fn each_string(v: &serde_json::Value, f: &mut impl FnMut(&str)) {
    match v {
        serde_json::Value::Object(m) => m.values().for_each(|x| each_string(x, f)),
        serde_json::Value::Array(a) => a.iter().for_each(|x| each_string(x, f)),
        serde_json::Value::String(s) => f(s),
        _ => {}
    }
}

// Subcommand names a string tells the user to TYPE, as opposed to "freemkv" just naming the
// product in a sentence.
fn commands_named_in(value: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut from = 0;
    while let Some(off) = value[from..].find("freemkv ") {
        let start = from + off + "freemkv ".len();
        from = start;
        let word: String = value[start..]
            .chars()
            .take_while(|c| (c.is_ascii_lowercase()) || c.is_ascii_digit() || *c == '-')
            .collect();
        if word.is_empty() {
            continue;
        }
        let rest = &value[start + word.len()..];
        // `freemkv disc://…` — the URL grammar, not a subcommand. An empty
        // remainder is the product name ending a sentence.
        if rest.starts_with("://") || rest.is_empty() {
            continue;
        }
        let spaces = rest.len() - rest.trim_start_matches(' ').len();
        let next = rest.trim_start_matches(' ');
        let is_command = match spaces {
            // `'freemkv info'` — a quoted command reference.
            0 => rest.starts_with(['\'', '»', '`', '"']),
            // `freemkv update-keys --url <u>` / `freemkv verify [disc://]`
            // / `freemkv verify disc:///dev/sg4`.
            1 => {
                next.starts_with(['-', '[', '<'])
                    || next.split(' ').next().is_some_and(|t| t.contains("://"))
            }
            // The description column of a `usage.ex.*` line.
            _ => true,
        };
        if is_command {
            out.insert(word);
        }
    }
    out
}

#[test]
fn every_command_named_in_a_locale_exists() {
    // Several locale strings used to instruct running `drive-info`/`disc-info`/
    // `remux`/`verify`, none of which are dispatched — they fall through to the
    // URL grammar and fail. Checked across ALL bundled locales (wrong in all 29).
    let mut offenders: std::collections::BTreeSet<(String, String)> = Default::default();
    for code in freemkv_i18n::SHIPPED_CODES {
        let raw = freemkv_i18n::bundled_locale_json(code)
            .unwrap_or_else(|| panic!("{code} listed as shipped but not loadable"));
        let doc: serde_json::Value =
            serde_json::from_str(raw).unwrap_or_else(|e| panic!("{code}.json invalid: {e}"));
        each_string(&doc, &mut |s| {
            for cmd in commands_named_in(s) {
                if !SUBCOMMANDS.contains(&cmd.as_str()) {
                    offenders.insert((code.to_string(), cmd));
                }
            }
        });
    }
    assert!(
        offenders.is_empty(),
        "locale strings tell the user to run subcommands that do not exist \
             (the dispatcher accepts {SUBCOMMANDS:?}): {offenders:?}"
    );
}

/// Regression: `update-keys --keydb <path>` must save the download to that
/// path. The flag used to be ignored (the keydb always went to the default
/// location); `update_keys_dest` now honors it.
#[test]
fn update_keys_honors_keydb_flag() {
    let args: Vec<String> = [
        "--url",
        "http://x/k.zip",
        "--keydb",
        "/custom/path/keydb.cfg",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        update_keys_dest(&args),
        std::path::PathBuf::from("/custom/path/keydb.cfg"),
        "--keydb must be the download destination"
    );
}

/// Without `--keydb`, the destination resolves through the standard
/// search/default policy — never the bogus override above.
#[test]
fn update_keys_without_keydb_flag_uses_standard_location() {
    let args: Vec<String> = ["--url", "http://x/k.zip"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_ne!(
        update_keys_dest(&args),
        std::path::PathBuf::from("/custom/path/keydb.cfg")
    );
    assert_eq!(
        update_keys_dest(&args),
        crate::pipe::resolved_keydb_path(&None)
    );
}

// `--keydb` followed by a flag has no value: the flag is not the destination.
#[test]
fn update_keys_keydb_does_not_swallow_the_following_flag() {
    let args = v(&["--keydb", "--url", "https://h/keydb.zip"]);
    assert_eq!(
        update_keys_dest(&args),
        crate::pipe::resolved_keydb_path(&None)
    );
    let args = v(&["--keydb", "/tmp/k.cfg", "--url", "https://h/keydb.zip"]);
    assert_eq!(
        update_keys_dest(&args),
        std::path::PathBuf::from("/tmp/k.cfg")
    );
}

fn v(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

#[test]
fn plain_two_urls() {
    assert_eq!(
        collect_urls(&v(&["disc://", "mkv://out.mkv"])),
        v(&["disc://", "mkv://out.mkv"])
    );
}

#[test]
fn stream_selection_flag_values_are_not_read_as_urls() {
    // Regression: `-a`/`-s` values (`none`/`eng`/…) must not be collected as a
    // third stream URL. Before the scheme://-only rewrite, an unlisted value-flag
    // let its value collect as a URL → 3 URLs → usage printed and nothing ran.
    assert_eq!(
        collect_urls(&v(&[
            "iso://d.iso",
            "mkv://out.mkv",
            "-t",
            "1",
            "-a",
            "none",
            "-s",
            "eng",
        ])),
        v(&["iso://d.iso", "mkv://out.mkv"])
    );
    // Flags-first ordering, and comma lists, are equally safe.
    assert_eq!(
        collect_urls(&v(&[
            "-a",
            "eng,spa",
            "-s",
            "none",
            "disc://",
            "mkv://out.mkv"
        ])),
        v(&["disc://", "mkv://out.mkv"])
    );
}

#[test]
fn value_flag_takes_non_url_value() {
    // -t 1 consumes "1"; the two URLs remain positional.
    assert_eq!(
        collect_urls(&v(&["disc://", "mkv://out.mkv", "-t", "1"])),
        v(&["disc://", "mkv://out.mkv"])
    );
    // --keydb with a real path value.
    assert_eq!(
        collect_urls(&v(&["--keydb", "keydb.cfg", "disc://", "mkv://out.mkv"])),
        v(&["disc://", "mkv://out.mkv"])
    );
}

#[test]
fn a_retired_flags_value_is_stepped_over_so_the_rejection_is_reached() {
    // `-k`/`--device`/`-d` are gone but took a value; that value must not
    // become a third positional (3 URLs = bare usage hint, no mention of the
    // removed flag). 2 URLs routes to the rip, where `parse_flags` names it.
    for retired in super::RETIRED_VALUE_FLAGS {
        assert_eq!(
            collect_urls(&v(&[retired, "value", "disc://", "mkv://out.mkv"])),
            v(&["disc://", "mkv://out.mkv"]),
            "`{retired}`'s value became a positional"
        );
        // …and a following stream URL is still NOT eaten (the same guard
        // the live value-flags get).
        assert_eq!(
            collect_urls(&v(&[retired, "disc://", "mkv://out.mkv"])),
            v(&["disc://", "mkv://out.mkv"]),
            "`{retired}` swallowed a positional URL"
        );
    }
}

#[test]
fn value_flag_does_not_swallow_positional_url() {
    // Regression: `--keydb` must not eat `disc://`, leaving a single URL
    // that silently routes to `info`. Both URLs must survive as positional.
    assert_eq!(
        collect_urls(&v(&["--keydb", "disc://", "mkv://out.mkv"])),
        v(&["disc://", "mkv://out.mkv"])
    );
    assert_eq!(
        collect_urls(&v(&["-t", "disc://", "mkv://out.mkv"])),
        v(&["disc://", "mkv://out.mkv"])
    );
}

#[test]
fn boolean_flags_ignored() {
    assert_eq!(
        collect_urls(&v(&["--multipass", "disc://", "iso://d.iso", "--raw"])),
        v(&["disc://", "iso://d.iso"])
    );
}

#[test]
fn key_url_value_is_not_a_positional() {
    // `--key-url`'s value is an https:// URL — it must be consumed as the
    // flag value, NOT reclassified as a third positional stream URL (which
    // would break the 2-URL rip dispatch). Only the two stream URLs remain.
    assert_eq!(
        collect_urls(&v(&[
            "disc://",
            "mkv://out.mkv",
            "--key-url",
            "https://keys.example/keys",
        ])),
        v(&["disc://", "mkv://out.mkv"])
    );
    // With a bearer token too.
    assert_eq!(
        collect_urls(&v(&[
            "--key-url",
            "https://keys.example/keys",
            "--key-auth",
            "tok",
            "disc://",
            "mkv://out.mkv",
        ])),
        v(&["disc://", "mkv://out.mkv"])
    );
}

#[test]
fn key_auth_token_value_consumed() {
    // `--key-auth`'s opaque token must be consumed, not kept as a positional.
    assert_eq!(
        collect_urls(&v(&["--key-auth", "tok", "disc://", "mkv://out.mkv"])),
        v(&["disc://", "mkv://out.mkv"])
    );
}
