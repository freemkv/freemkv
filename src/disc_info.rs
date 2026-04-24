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
                device_path = args.get(i).cloned();
            }
            "--quiet" | "-q" => quiet = true,
            "--verbose" | "-v" => verbose = true,
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
            eprintln!("{}", e);
            std::process::exit(1);
        }),
        None => libfreemkv::find_drive().unwrap_or_else(|| {
            eprintln!("{}", strings::get("error.no_bluray_drive"));
            std::process::exit(1);
        }),
    };
    let _ = drive.wait_ready();
    let _ = drive.init();
    let _ = drive.probe_disc();

    let disc = match Disc::scan(&mut drive, &ScanOptions::default()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "{}",
                strings::fmt("error.scan_failed", &[("error", &e.to_string())])
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

    // Format and capacity
    let format = match disc.format {
        DiscFormat::Uhd => "4K UHD",
        DiscFormat::BluRay => "Blu-ray",
        DiscFormat::Dvd => "DVD",
        DiscFormat::Unknown => "Blu-ray",
    };
    let gb = disc.capacity_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
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

    if disc.titles.is_empty() {
        out.print(Normal, "disc.no_titles");
        return;
    }

    out.print(Normal, "disc.titles");
    out.blank(Normal);

    let max_titles = if full { disc.titles.len() } else { 5 };

    for (idx, title) in disc.titles.iter().take(max_titles).enumerate() {
        let hours = (title.duration_secs / 3600.0) as u32;
        let mins = ((title.duration_secs % 3600.0) / 60.0) as u32;
        let gb = title.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        let clip_word = if title.clips.len() != 1 {
            strings::get("disc.clips")
        } else {
            strings::get("disc.clip")
        };

        out.raw(
            Normal,
            &format!(
                "  {:2}. {:14}  {:1}h {:02}m  {:>5.1} GB  {} {}",
                idx + 1,
                title.playlist,
                hours,
                mins,
                gb,
                title.clips.len(),
                clip_word
            ),
        );

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
            out.blank(Normal);
            let label = strings::get("disc.video");
            for (vi, v) in videos.iter().enumerate() {
                let line = format_video(v, verbose);
                if vi == 0 {
                    out.raw(Normal, &format!("      {}:     {}", label, line));
                } else {
                    out.raw(Normal, &format!("                 {}", line));
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
            out.blank(Normal);
            let label = strings::get("disc.audio");
            for (ai, a) in audios.iter().enumerate() {
                let line = format_audio(a, verbose);
                if ai == 0 {
                    out.raw(Normal, &format!("      {}:     {}", label, line));
                } else {
                    out.raw(Normal, &format!("                 {}", line));
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
            out.blank(Normal);
            let label = strings::get("disc.subtitle");
            for (si, s) in subs.iter().enumerate() {
                let line = format_subtitle(s);
                if si == 0 {
                    out.raw(Normal, &format!("      {}:  {}", label, line));
                } else {
                    out.raw(Normal, &format!("                 {}", line));
                }
            }
        }

        out.blank(Normal);
    }

    if disc.titles.len() > max_titles {
        out.fmt(
            Normal,
            "disc.more_titles",
            &[("count", &(disc.titles.len() - max_titles).to_string())],
        );
        out.blank(Normal);
    }
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
    if v.secondary && !v.label.is_empty() {
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
