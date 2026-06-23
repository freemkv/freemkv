// freemkv disc-info — Show disc titles, streams, and sizes
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all logic in libfreemkv. This file only formats output.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::{
    AudioStream, Codec, ColorSpace, Disc, DiscFormat, Drive, HdrFormat, LabelPurpose,
    LabelQualifier, ScanOptions, Stream, SubtitleStream, VideoStream,
};

pub fn run(args: &[String]) {
    let mut device_path: Option<String> = None;
    let mut quiet = false;
    let mut verbose = false;
    let mut full = false;
    let mut basic = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--device" | "-d" => {
                i += 1;
                match args.get(i) {
                    Some(v) => device_path = Some(v.clone()),
                    None => {
                        eprintln!(
                            "{}",
                            strings::fmt(
                                "error.flag_needs_value",
                                &[("flag", "--device"), ("example", "--device /dev/sg0")]
                            )
                        );
                        std::process::exit(1);
                    }
                }
            }
            "--quiet" | "-q" => quiet = true,
            // `--log-level N` sets the tracing level (in main::init_logging);
            // here it also widens stdout detail at level >= 2. Accept + skip
            // its value so it isn't treated as a positional / unknown option.
            "--log-level" => {
                i += 1;
                if args.get(i).and_then(|s| s.parse::<u8>().ok()).unwrap_or(1) >= 2 {
                    verbose = true;
                }
            }
            "--log-file" => {
                i += 1; // skip the path value
            }
            "--full" | "-f" => full = true,
            "--basic" | "-b" => basic = true,
            "--help" | "-h" => {
                println!("{}", strings::get("disc.usage"));
                return;
            }
            _ => {
                eprintln!(
                    "{}",
                    strings::fmt("app.unknown_option", &[("opt", &args[i])])
                );
                std::process::exit(1);
            }
        }
        i += 1;
    }

    let out = Output::new(verbose, quiet);

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);
    out.print(Normal, "disc.scanning");
    out.blank(Normal);

    let mut drive = match device_path {
        Some(ref p) => Drive::open(std::path::Path::new(p)).unwrap_or_else(|e| {
            eprintln!("{}", crate::pipe::fmt_err(&e));
            std::process::exit(1);
        }),
        None => libfreemkv::find_drive().unwrap_or_else(|| {
            eprintln!("{}", strings::get("error.no_drive"));
            std::process::exit(1);
        }),
    };
    // init()/probe_disc() are intentionally best-effort: drives without the
    // probed firmware support return UnsupportedDrive, which is fine here —
    // `Disc::scan` below is the authoritative gate and reports the real error.
    let _ = drive.wait_ready();
    let _ = drive.init();
    let _ = drive.probe_disc();

    let disc = match Disc::scan(&mut drive, &ScanOptions::default()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "{}",
                strings::fmt(
                    "error.scan_failed",
                    &[("detail", &crate::pipe::fmt_err(&e))]
                )
            );
            std::process::exit(1);
        }
    };

    // Disc title
    if let Some(ref title) = disc.meta_title {
        out.raw(Normal, &format!("{}: {}", strings::get("disc.disc"), title));
    } else if !disc.volume_id.is_empty() {
        out.raw(
            Normal,
            &format!(
                "{}: {}",
                strings::get("disc.disc"),
                format_volume_id(&disc.volume_id)
            ),
        );
    }

    // Format and capacity. An unclassified disc must NOT masquerade as Blu-ray
    // — report it distinctly so data/future/unknown discs aren't misread.
    let unknown = strings::get("disc.format_unknown");
    let format = match disc.format {
        DiscFormat::Uhd => "4K UHD",
        DiscFormat::BluRay => "Blu-ray",
        DiscFormat::Dvd => "DVD",
        DiscFormat::Unknown => &unknown,
    };
    let gb = disc.capacity_bytes as f64 / 1_000_000_000.0; // decimal GB, matches disc-marketed capacity
    out.raw(
        Normal,
        &format!(
            "{}: {} ({}L, {:.1} GB)",
            strings::get("disc.format"),
            format,
            disc.layers,
            gb
        ),
    );
    if disc.encrypted {
        if disc.css.is_some() {
            out.print(Normal, "disc.css_encrypted");
        } else {
            out.print(Normal, "disc.aacs_encrypted");
        }
    }

    // Verbose: AACS details
    if verbose {
        if let Some(ref aacs) = disc.aacs {
            out.raw(
                Normal,
                &format!(
                    "AACS {}.0, MKB v{}",
                    aacs.version,
                    aacs.mkb_version.unwrap_or(0)
                ),
            );
            out.raw(Normal, &format!("Disc hash: {}", aacs.disc_hash));
            out.raw(
                Normal,
                &format!(
                    "Keys: {} ({} unit keys)",
                    aacs.key_source.name(),
                    aacs.unit_keys.len()
                ),
            );
        }
        out.raw(
            Normal,
            &format!(
                "Drive: {} {} {}",
                drive.drive_id.vendor_id.trim(),
                drive.drive_id.product_id.trim(),
                drive.drive_id.product_revision.trim()
            ),
        );
        out.raw(Normal, &format!("Device: {}", drive.device_path()));
    }

    // Release the drive fd before printing titles
    drive.close();

    out.blank(Normal);

    print_titles(&out, &disc, full, verbose, basic);
}

/// Print a full, localized title list for an already-scanned `Disc` using a
/// fresh `Normal`-level `Output`. This is the entry point for callers that have
/// a scanned disc but no `Output`/verbosity context of their own — notably the
/// `info iso://` path, which scans an ISO **keylessly** (no AACS key needed to
/// list titles) and reuses the exact per-title formatting the drive (`disc://`)
/// path produces: duration, size, clip count, and video/audio/subtitle streams.
///
/// `full` shows every title (otherwise the first 5, with a "+N more" footer).
pub fn print_disc_titles(disc: &Disc, full: bool) {
    let out = Output::new(false, false);
    print_titles(&out, disc, full, false, false);
}

/// Shared title-list renderer. Used by both `run` (drive scan, honoring its
/// verbose/basic flags) and `print_disc_titles` (ISO scan, plain Normal output).
/// Builds the lines via [`title_lines`] then emits them through `out` at
/// `Normal` level (so `--quiet` still suppresses them, `--verbose` shows them).
fn print_titles(out: &Output, disc: &Disc, full: bool, verbose: bool, basic: bool) {
    for line in title_lines(disc, full, verbose, basic) {
        out.raw(Normal, &line);
    }
}

/// Pure formatter: build the full, localized title-list output as a vector of
/// lines (empty strings are blank separators). No I/O — kept side-effect-free
/// so it can be unit-tested against a synthetic `Disc` without capturing stdout.
///
/// This is the single source of truth for the per-title layout shared by the
/// `disc://` (drive) and `iso://` (keyless ISO) info paths.
fn title_lines(disc: &Disc, full: bool, verbose: bool, basic: bool) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    if disc.titles.is_empty() {
        lines.push(strings::get("disc.no_titles"));
        return lines;
    }

    lines.push(strings::get("disc.titles"));
    lines.push(String::new());

    let max_titles = if full { disc.titles.len() } else { 5 };

    // Stream rows align their value column to one shared indent derived from the
    // widest of the three (localized) labels, so the layout holds for any locale
    // instead of hardcoding English label widths against a fixed 17-space
    // continuation. The labels don't vary per title, so compute it once. Skipped
    // in `--basic` (no stream rows are printed) to avoid the unused binding.
    let indent = if basic {
        0
    } else {
        [
            strings::get("disc.video"),
            strings::get("disc.audio"),
            strings::get("disc.subtitle"),
        ]
        .iter()
        .map(|l| label_indent(l))
        .max()
        .unwrap_or(17)
    };

    for (idx, title) in disc.titles.iter().take(max_titles).enumerate() {
        // Truncate to whole seconds once, then split with integer math — exact
        // and avoids float-precision display artifacts on the h/m breakdown.
        let total_secs = title.duration_secs as u64;
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        let gb = title.size_bytes as f64 / 1_000_000_000.0; // decimal GB, matches disc-marketed capacity
        let clip_word = if title.clips.len() != 1 {
            strings::get("disc.clips")
        } else {
            strings::get("disc.clip")
        };

        lines.push(format!(
            "  {:2}. {:14}  {:2}h {:02}m  {:>5.1} GB  {} {}",
            idx + 1,
            title.playlist,
            hours,
            mins,
            gb,
            title.clips.len(),
            clip_word
        ));

        if basic {
            continue;
        }

        // Video
        let videos: Vec<&VideoStream> = title
            .streams
            .iter()
            .filter_map(|s| {
                if let Stream::Video(v) = s {
                    Some(v)
                } else {
                    None
                }
            })
            .collect();
        if !videos.is_empty() {
            lines.push(String::new());
            let label = strings::get("disc.video");
            for (vi, v) in videos.iter().enumerate() {
                let line = format_video(v, verbose);
                if vi == 0 {
                    lines.push(format!("{}{}", label_prefix(&label, indent), line));
                } else {
                    lines.push(format!("{:indent$}{}", "", line, indent = indent));
                }
            }
        }

        // Audio
        let audios: Vec<&AudioStream> = title
            .streams
            .iter()
            .filter_map(|s| {
                if let Stream::Audio(a) = s {
                    Some(a)
                } else {
                    None
                }
            })
            .collect();
        if !audios.is_empty() {
            lines.push(String::new());
            let label = strings::get("disc.audio");
            for (ai, a) in audios.iter().enumerate() {
                let line = format_audio(a, verbose);
                if ai == 0 {
                    lines.push(format!("{}{}", label_prefix(&label, indent), line));
                } else {
                    lines.push(format!("{:indent$}{}", "", line, indent = indent));
                }
            }
        }

        // Subtitles
        let subs: Vec<&SubtitleStream> = title
            .streams
            .iter()
            .filter_map(|s| {
                if let Stream::Subtitle(sub) = s {
                    Some(sub)
                } else {
                    None
                }
            })
            .collect();
        if !subs.is_empty() {
            lines.push(String::new());
            let label = strings::get("disc.subtitle");
            for (si, s) in subs.iter().enumerate() {
                let line = format_subtitle(s);
                if si == 0 {
                    lines.push(format!("{}{}", label_prefix(&label, indent), line));
                } else {
                    lines.push(format!("{:indent$}{}", "", line, indent = indent));
                }
            }
        }

        lines.push(String::new());
    }

    if disc.titles.len() > max_titles {
        lines.push(strings::fmt(
            "disc.more_titles",
            &[("count", &(disc.titles.len() - max_titles).to_string())],
        ));
        lines.push(String::new());
    }

    lines
}

/// Column at which a stream's value text should start, given its label: the
/// 6-space lead + the (localized) label + the colon + a 2-space gap. Taking the
/// max of these across the Video/Audio/Subtitle labels reproduces the historical
/// English layout (`Subtitle` is the widest at 8 chars → column 17) while
/// staying correct for longer localized labels (e.g. `Untertitel`,
/// `Sottotitoli`) that previously overran the hardcoded 17-space indent.
fn label_indent(label: &str) -> usize {
    6 + label.chars().count() + 1 + 2
}

/// First-line prefix for a stream group: 6-space lead, the label, a colon, then
/// padding so the value text begins exactly at `indent`.
fn label_prefix(label: &str, indent: usize) -> String {
    let head = format!("      {}:", label);
    let pad = indent.saturating_sub(head.chars().count());
    format!("{head}{:pad$}", "", pad = pad)
}

// ── Formatting ──────────────────────────────────────────────────────────────

fn format_video(v: &VideoStream, verbose: bool) -> String {
    let mut parts = vec![codec_name(v.codec).to_string(), v.resolution.to_string()];
    if v.frame_rate != libfreemkv::FrameRate::Unknown {
        parts.push(format!("{}fps", v.frame_rate));
    }
    if v.hdr != HdrFormat::Sdr {
        parts.push(hdr_name(v.hdr).to_string());
    }
    if v.color_space == ColorSpace::Bt2020 {
        parts.push("BT.2020".into());
    }
    // A secondary Dolby Vision video stream is the enhancement layer (the
    // library no longer carries the English descriptor — it's localized here).
    if v.secondary && v.hdr == HdrFormat::DolbyVision {
        parts.push(strings::get("disc.dolby_vision_el"));
    } else if v.secondary && !v.label.is_empty() {
        parts.push(v.label.clone());
    }
    if verbose {
        parts.push(format!("[PID 0x{:04X}]", v.pid));
    }
    parts.join(" ")
}

fn format_audio(a: &AudioStream, verbose: bool) -> String {
    let lang = lang_name(&a.language);
    let codec = codec_name(a.codec);
    let mut s = format!("{} {} {}", lang, codec, a.channels);
    if verbose {
        s.push_str(&format!(" {} [PID 0x{:04X}]", a.sample_rate, a.pid));
    }

    // Combine label (codec/variant info from the library) with locale-rendered
    // purpose / secondary tags. Library guarantees no English in `label`.
    let mut tags: Vec<String> = Vec::new();
    if let Some(key) = purpose_key(a.purpose) {
        tags.push(strings::get(key));
    }
    if a.secondary {
        tags.push(strings::get("stream.secondary"));
    }
    if !a.label.is_empty() {
        tags.push(a.label.clone());
    }
    if !tags.is_empty() {
        s.push_str(&format!(" ({})", tags.join(", ")));
    }
    s
}

fn format_subtitle(s: &SubtitleStream) -> String {
    let lang = lang_name(&s.language);
    let mut tags: Vec<String> = Vec::new();
    if s.forced {
        tags.push(strings::get("disc.forced"));
    }
    if let Some(key) = qualifier_key(s.qualifier) {
        tags.push(strings::get(key));
    }
    if tags.is_empty() {
        lang.to_string()
    } else {
        format!("{} ({})", lang, tags.join(", "))
    }
}

/// Map `LabelPurpose` to its locale string key. `Normal` returns None — no tag.
fn purpose_key(p: LabelPurpose) -> Option<&'static str> {
    match p {
        LabelPurpose::Commentary => Some("stream.purpose.commentary"),
        LabelPurpose::Descriptive => Some("stream.purpose.descriptive"),
        LabelPurpose::Score => Some("stream.purpose.score"),
        LabelPurpose::Ime => Some("stream.purpose.ime"),
        LabelPurpose::Normal => None,
    }
}

/// Map `LabelQualifier` to its locale string key. `Forced` is rendered via
/// `disc.forced` from the existing forced flag, so we skip it here.
fn qualifier_key(q: LabelQualifier) -> Option<&'static str> {
    match q {
        LabelQualifier::Sdh => Some("stream.qualifier.sdh"),
        LabelQualifier::DescriptiveService => Some("stream.qualifier.descriptive_service"),
        LabelQualifier::None | LabelQualifier::Forced => None,
    }
}

fn codec_name(c: Codec) -> String {
    match c {
        Codec::Ac3 => "DD".into(),
        Codec::Ac3Plus => "DD+".into(),
        Codec::DvdSub => "DVD Sub".into(),
        Codec::Unknown(ct) => format!("0x{:02x}", ct),
        other => other.name().into(),
    }
}

fn hdr_name(h: HdrFormat) -> &'static str {
    h.name()
}

fn lang_name(code: &str) -> String {
    if code.is_empty() {
        return "?".to_string();
    }
    isolang::Language::from_639_3(code)
        .or_else(|| isolang::Language::from_639_1(code))
        .map(|l| l.to_name().to_string())
        .unwrap_or_else(|| code.to_string())
}

fn format_volume_id(vol_id: &str) -> String {
    vol_id
        .replace('_', " ")
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(ch) => format!("{}{}", ch.to_uppercase(), c.as_str().to_lowercase()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfreemkv::disc::DiscRegion;
    use libfreemkv::{
        AudioChannels, ColorSpace, ContentFormat, DiscFormat, DiscTitle, FrameRate, HdrFormat,
        LabelPurpose, LabelQualifier, Resolution, SampleRate,
    };

    /// A minimal synthetic encrypted disc with one rich title (video + audio +
    /// subtitle). Mirrors what a keyless ISO scan yields for `info iso://` —
    /// titles are populated, but no AACS key is resolved (`aacs: None`,
    /// `encrypted: true`). Listing must work all the same.
    fn synthetic_disc() -> Disc {
        let video = Stream::Video(VideoStream {
            pid: 0x1011,
            codec: Codec::Hevc,
            resolution: Resolution::Unknown,
            frame_rate: FrameRate::Unknown,
            hdr: HdrFormat::Sdr,
            color_space: ColorSpace::Bt709,
            secondary: false,
            label: String::new(),
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
        // Regression: the scan-failure handler keyed the format arg "error" while
        // the `error.scan_failed` locale string uses `{detail}` — so the real
        // cause was dropped and the user saw the literal `{detail}`. The handler
        // must key the arg "detail" (and route the cause through `fmt_err`).
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
        // The substituted cause text is present (no raw E#### code leaks because
        // it routed through `fmt_err`).
        assert!(
            !rendered.contains("E1002"),
            "raw error code must not leak, got:\n{rendered}"
        );
        assert!(
            rendered.starts_with("Scan failed:") && rendered.len() > "Scan failed:".len() + 1,
            "expected the cause appended after the prefix, got:\n{rendered}"
        );
    }

    #[test]
    fn open_failure_renders_through_fmt_err_no_raw_code_leaks() {
        // Regression: the `disc-info --device` open-failure handler did
        // `eprintln!("{}", e)`, printing libfreemkv's raw `E####: <data>`
        // Display and bypassing the i18n renderer. It must route the error
        // through `pipe::fmt_err`, which localizes the code (E1001 explains the
        // disk-group / privilege fix) and never leaks a raw `E####`.
        let rendered = crate::pipe::fmt_err(&libfreemkv::Error::DevicePermission {
            path: "/dev/sg0".to_string(),
        });
        assert!(
            !rendered.contains("E1001"),
            "raw error code must not leak, got:\n{rendered}"
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
}
