// freemkv disc-info — Show disc titles, streams, and sizes
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all logic in libfreemkv. This file only formats output.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::{
    AudioStream, Codec, ColorSpace, Disc, DiscFormat, DriveSession, HdrFormat, ScanOptions, Stream,
    SubtitleStream, VideoStream,
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

    let dev_path = device_path.unwrap_or_else(|| {
        libfreemkv::find_drive().unwrap_or_else(|| {
            eprintln!("{}", strings::get("error.no_bluray_drive"));
            std::process::exit(1);
        })
    });

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);
    out.print(Normal, "disc.scanning");
    out.blank(Normal);

    let mut session = match DriveSession::open(std::path::Path::new(&dev_path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    if let Err(e) = session.wait_ready() {
        eprintln!(
            "{}",
            strings::fmt("error.not_ready", &[("error", &e.to_string())])
        );
        std::process::exit(1);
    }

    let disc = match Disc::scan(&mut session, &ScanOptions::default()) {
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
        out.print(Normal, "disc.aacs_encrypted");
    }
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
                let line = format_video(v);
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
                let line = format_audio(a, basic);
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

fn format_video(v: &VideoStream) -> String {
    let mut parts = vec![codec_name(v.codec).to_string(), v.resolution.clone()];
    if v.hdr != HdrFormat::Sdr {
        parts.push(hdr_name(v.hdr).to_string());
    }
    if v.color_space == ColorSpace::Bt2020 {
        parts.push("BT.2020".into());
    }
    if v.secondary && !v.label.is_empty() {
        parts.push(v.label.clone());
    }
    parts.join(" ")
}

fn format_audio(a: &AudioStream, basic: bool) -> String {
    let lang = lang_name(&a.language);
    let codec = codec_name(a.codec);
    if !basic && !a.label.is_empty() {
        format!("{} {} {} ({})", lang, codec, a.channels, a.label)
    } else {
        format!("{} {} {}", lang, codec, a.channels)
    }
}

fn format_subtitle(s: &SubtitleStream) -> String {
    let lang = lang_name(&s.language);
    if s.forced {
        format!("{} ({})", lang, strings::get("disc.forced"))
    } else {
        lang.to_string()
    }
}

fn codec_name(c: Codec) -> String {
    match c {
        Codec::Hevc => "HEVC".into(),
        Codec::H264 => "H.264".into(),
        Codec::Vc1 => "VC-1".into(),
        Codec::Mpeg2 => "MPEG-2".into(),
        Codec::TrueHd => "TrueHD".into(),
        Codec::DtsHdMa => "DTS-HD MA".into(),
        Codec::DtsHdHr => "DTS-HD HR".into(),
        Codec::Dts => "DTS".into(),
        Codec::Ac3 => "DD".into(),
        Codec::Ac3Plus => "DD+".into(),
        Codec::Lpcm => "LPCM".into(),
        Codec::Pgs => "PGS".into(),
        Codec::DvdSub => "DVD Sub".into(),
        Codec::Unknown(ct) => format!("0x{:02x}", ct),
    }
}

fn hdr_name(h: HdrFormat) -> &'static str {
    match h {
        HdrFormat::Sdr => "SDR",
        HdrFormat::Hdr10 => "HDR10",
        HdrFormat::DolbyVision => "Dolby Vision",
    }
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
