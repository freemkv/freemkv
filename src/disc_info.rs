// freemkv disc-info — Show disc titles, streams, and sizes
// AGPL-3.0 — freemkv project
//
// CLI is dumb — all logic in libfreemkv. This file only formats output.

use libfreemkv::{Disc, DiscFormat, DriveSession, ScanOptions, Stream,
                  VideoStream, AudioStream, SubtitleStream, Codec, HdrFormat, ColorSpace};

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

    if quiet { return; }

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
    if disc.encrypted { println!("AACS: Encrypted"); }
    println!();

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

        // Video
        let videos: Vec<&VideoStream> = title.streams.iter()
            .filter_map(|s| if let Stream::Video(v) = s { Some(v) } else { None })
            .collect();
        if !videos.is_empty() {
            println!();
            for (vi, v) in videos.iter().enumerate() {
                let line = format_video(v);
                if vi == 0 { println!("      Video:     {}", line); }
                else { println!("                 {}", line); }
            }
        }

        // Audio
        let audios: Vec<&AudioStream> = title.streams.iter()
            .filter_map(|s| if let Stream::Audio(a) = s { Some(a) } else { None })
            .collect();
        if !audios.is_empty() {
            println!();
            for (ai, a) in audios.iter().enumerate() {
                let line = format_audio(a);
                if ai == 0 { println!("      Audio:     {}", line); }
                else { println!("                 {}", line); }
            }
        }

        // Subtitles
        let subs: Vec<&SubtitleStream> = title.streams.iter()
            .filter_map(|s| if let Stream::Subtitle(sub) = s { Some(sub) } else { None })
            .collect();
        if !subs.is_empty() {
            println!();
            for (si, s) in subs.iter().enumerate() {
                let line = format_subtitle(s);
                if si == 0 { println!("      Subtitle:  {}", line); }
                else { println!("                 {}", line); }
            }
        }

        println!();
    }

    if disc.titles.len() > max_titles {
        println!("      +{} more (use --full to show all)", disc.titles.len() - max_titles);
        println!();
    }
}

// ── Formatting ──────────────────────────────────────────────────────────────

fn format_video(v: &VideoStream) -> String {
    let mut parts = vec![codec_name(v.codec).to_string(), v.resolution.clone()];
    if v.hdr != HdrFormat::Sdr { parts.push(hdr_name(v.hdr).to_string()); }
    if v.color_space == ColorSpace::Bt2020 { parts.push("BT.2020".into()); }
    if v.secondary && !v.label.is_empty() { parts.push(v.label.clone()); }
    parts.join(" ")
}

fn format_audio(a: &AudioStream) -> String {
    let lang = lang_name(&a.language);
    let codec = codec_name(a.codec);
    if !a.label.is_empty() {
        format!("{} {} {} ({})", lang, codec, a.channels, a.label)
    } else {
        format!("{} {} {}", lang, codec, a.channels)
    }
}

fn format_subtitle(s: &SubtitleStream) -> String {
    let lang = lang_name(&s.language);
    if s.forced { format!("{} (forced)", lang) } else { lang.to_string() }
}

fn codec_name(c: Codec) -> &'static str {
    match c {
        Codec::Hevc => "HEVC", Codec::H264 => "H.264", Codec::Vc1 => "VC-1",
        Codec::Mpeg2 => "MPEG-2", Codec::TrueHd => "TrueHD", Codec::DtsHdMa => "DTS-HD MA",
        Codec::DtsHdHr => "DTS-HD HR", Codec::Dts => "DTS", Codec::Ac3 => "DD",
        Codec::Ac3Plus => "DD+", Codec::Lpcm => "LPCM", Codec::Pgs => "PGS",
        Codec::Unknown(_) => "?",
    }
}

fn hdr_name(h: HdrFormat) -> &'static str {
    match h { HdrFormat::Sdr => "SDR", HdrFormat::Hdr10 => "HDR10", HdrFormat::DolbyVision => "Dolby Vision" }
}

fn lang_name(code: &str) -> String {
    if code.is_empty() { return "?".to_string(); }
    isolang::Language::from_639_3(code)
        .or_else(|| isolang::Language::from_639_1(code))
        .map(|l| l.to_name().to_string())
        .unwrap_or_else(|| code.to_string())
}

fn format_volume_id(vol_id: &str) -> String {
    vol_id.replace('_', " ").split_whitespace()
        .map(|w| { let mut c = w.chars(); match c.next() {
            Some(ch) => format!("{}{}", ch.to_uppercase(), c.as_str().to_lowercase()),
            None => String::new(),
        }}).collect::<Vec<_>>().join(" ")
}

fn find_bd_drive() -> Option<String> {
    for i in 0..16 {
        let path = format!("/dev/sg{}", i);
        if std::path::Path::new(&path).exists() {
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
