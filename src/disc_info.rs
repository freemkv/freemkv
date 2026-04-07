// freemkv disc-info — Show disc titles, streams, and sizes
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all logic in libfreemkv. This file only formats output.

use libfreemkv::{Disc, DiscFormat, DriveSession, ScanOptions, StreamKind, Codec, HdrFormat};

pub fn run(args: &[String]) {
    let mut device_path: Option<String> = None;
    let mut quiet = false;
    let mut full = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--device" | "-d" => { i += 1; device_path = args.get(i).cloned(); }
            "--quiet" | "-q" => quiet = true,
            "--full" | "-f" => full = true,
            "--help" | "-h" => {
                println!("Usage: freemkv disc-info [--device /dev/sgN] [--full] [--quiet]");
                return;
            }
            _ => { eprintln!("Unknown option: {}", args[i]); std::process::exit(1); }
        }
        i += 1;
    }

    let dev_path = device_path.unwrap_or_else(|| find_bd_drive().unwrap_or_else(|| {
        eprintln!("No Blu-ray drive found");
        std::process::exit(1);
    }));

    if !quiet {
        println!("freemkv {}", env!("CARGO_PKG_VERSION"));
        println!();
        println!("Scanning disc...");
        println!();
    }

    let mut session = match DriveSession::open(std::path::Path::new(&dev_path)) {
        Ok(s) => s,
        Err(e) => { eprintln!("{}", e); std::process::exit(1); }
    };

    let disc = match Disc::scan(&mut session, &ScanOptions::default()) {
        Ok(d) => d,
        Err(e) => { eprintln!("Scan failed: {}", e); std::process::exit(1); }
    };

    // Display
    if !quiet {
        // Disc title
        if let Some(ref title) = disc.meta_title {
            println!("Disc: {}", title);
        } else if !disc.volume_id.is_empty() {
            println!("Disc: {}", format_volume_id(&disc.volume_id));
        }

        // Format and capacity
        let format = match disc.format {
            DiscFormat::Uhd => "4K UHD",
            DiscFormat::BluRay => "Blu-ray",
            DiscFormat::Dvd => "DVD",
            DiscFormat::Unknown => "Blu-ray",
        };
        let gb = disc.capacity_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        println!("Format: {} ({}L, {:.1} GB)", format, disc.layers, gb);

        if disc.encrypted {
            println!("AACS: Encrypted");
        }
        println!();

        // Titles
        if disc.titles.is_empty() {
            println!("No titles found");
            return;
        }

        println!("Titles");
        println!();

        let max_titles = if full { disc.titles.len() } else { 5 };

        for (idx, title) in disc.titles.iter().take(max_titles).enumerate() {
            let hours = (title.duration_secs / 3600.0) as u32;
            let mins = ((title.duration_secs % 3600.0) / 60.0) as u32;
            let gb = title.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

            println!("  {:2}. {:14}  {:1}h {:02}m  {:>5.1} GB  {} clip{}",
                idx + 1, title.playlist, hours, mins, gb,
                title.clips.len(),
                if title.clips.len() != 1 { "s" } else { "" });

            // Video streams
            let videos: Vec<_> = title.streams.iter()
                .filter(|s| s.kind == StreamKind::Video)
                .collect();
            if !videos.is_empty() {
                println!();
                for (vi, v) in videos.iter().enumerate() {
                    let line = format_video(v);
                    if vi == 0 {
                        println!("      Video:     {}", line);
                    } else {
                        println!("                 {}", line);
                    }
                }
            }

            // Audio streams
            let audios: Vec<_> = title.streams.iter()
                .filter(|s| s.kind == StreamKind::Audio)
                .collect();
            if !audios.is_empty() {
                println!();
                for (ai, a) in audios.iter().enumerate() {
                    let line = format_audio(a, &disc.jar_labels, ai);
                    if ai == 0 {
                        println!("      Audio:     {}", line);
                    } else {
                        println!("                 {}", line);
                    }
                }
            }

            // Subtitle streams
            let subs: Vec<_> = title.streams.iter()
                .filter(|s| s.kind == StreamKind::Subtitle)
                .collect();
            if !subs.is_empty() {
                println!();
                for (si, s) in subs.iter().enumerate() {
                    let line = format_subtitle(s);
                    if si == 0 {
                        println!("      Subtitle:  {}", line);
                    } else {
                        println!("                 {}", line);
                    }
                }
            }

            println!();
        }

        if disc.titles.len() > max_titles {
            println!("      +{} more (use --full to show all)", disc.titles.len() - max_titles);
            println!();
        }
    }
}

// ── Formatting helpers ──────────────────────────────────────────────────────

fn format_video(s: &libfreemkv::Stream) -> String {
    let codec = codec_name(s.codec);
    let mut parts = vec![codec.to_string(), s.resolution.clone()];

    match s.hdr {
        HdrFormat::Hdr10 => parts.push("HDR10".into()),
        HdrFormat::DolbyVision => parts.push("Dolby Vision".into()),
        HdrFormat::Sdr => {}
    }

    if s.color_space == libfreemkv::ColorSpace::Bt2020 {
        parts.push("BT.2020".into());
    }

    if s.secondary && !s.label.is_empty() {
        parts.push(s.label.clone());
    }

    parts.join(" ")
}

fn format_audio(s: &libfreemkv::Stream, jar: &libfreemkv::jar::JarLabels, index: usize) -> String {
    let lang = lang_name(&s.language);
    let codec = codec_name(s.codec);
    let ch = &s.channels;

    // Try JAR label for this audio track
    let jar_label = jar.audio.get(index).map(|l| &l.description).filter(|d| !d.is_empty());

    if let Some(label) = jar_label {
        format!("{} {} {} ({})", lang, codec, ch, label)
    } else {
        format!("{} {} {}", lang, codec, ch)
    }
}

fn format_subtitle(s: &libfreemkv::Stream) -> String {
    let lang = lang_name(&s.language);

    // Check for forced subtitle (typically has a specific flag or is a duplicate language)
    if s.label.contains("forced") {
        format!("{} (forced)", lang)
    } else {
        lang.to_string()
    }
}

fn codec_name(c: Codec) -> &'static str {
    match c {
        Codec::Hevc => "HEVC",
        Codec::H264 => "H.264",
        Codec::Vc1 => "VC-1",
        Codec::Mpeg2 => "MPEG-2",
        Codec::TrueHd => "TrueHD",
        Codec::DtsHdMa => "DTS-HD MA",
        Codec::DtsHdHr => "DTS-HD HR",
        Codec::Dts => "DTS",
        Codec::Ac3 => "DD",
        Codec::Ac3Plus => "DD+",
        Codec::Lpcm => "LPCM",
        Codec::Pgs => "PGS",
        Codec::Unknown(_) => "?",
    }
}

fn lang_name(code: &str) -> &str {
    match code {
        "eng" => "English",
        "fra" | "fre" => "French",
        "deu" | "ger" => "German",
        "spa" => "Spanish",
        "ita" => "Italian",
        "jpn" => "Japanese",
        "zho" | "chi" => "Chinese",
        "kor" => "Korean",
        "por" => "Portuguese",
        "rus" => "Russian",
        "nld" | "dut" => "Dutch",
        "dan" => "Danish",
        "fin" => "Finnish",
        "nor" => "Norwegian",
        "swe" => "Swedish",
        "pol" => "Polish",
        "ces" | "cze" => "Czech",
        "hun" => "Hungarian",
        "ron" | "rum" => "Romanian",
        "tha" => "Thai",
        "ara" => "Arabic",
        "hin" => "Hindi",
        "ell" | "gre" => "Greek",
        "" => "?",
        other => other,
    }
}

fn format_volume_id(vol_id: &str) -> String {
    vol_id.replace('_', " ")
        .split_whitespace()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str().to_lowercase()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn find_bd_drive() -> Option<String> {
    // Check common Linux SG device paths
    for i in 0..16 {
        let path = format!("/dev/sg{}", i);
        if std::path::Path::new(&path).exists() {
            // Try to open and check if it's an optical drive
            if let Ok(session) = DriveSession::open_no_unlock(std::path::Path::new(&path)) {
                let vendor = session.drive_id.vendor_id.trim().to_lowercase();
                if vendor.contains("hl-dt") || vendor.contains("pioneer") || vendor.contains("asus") {
                    return Some(path);
                }
            }
        }
    }
    None
}
