// freemkv info — Inspect a disc, ISO, or stream URL
// AGPL-3.0 — freemkv project
//
// Dispatches `freemkv info <url>` to the right renderer:
//   disc:// → cmd::disc_info, or cmd::drive_info when --share / -s is set
//   iso:// / mkv:// / m2ts:// → libfreemkv::input metadata dump

use crate::cmd::{disc_info, drive_info};
use crate::strings;

pub fn run(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: freemkv info <url>");
        std::process::exit(1);
    }

    let url = &args[0];
    let parsed = libfreemkv::parse_url(url);

    match &parsed {
        libfreemkv::StreamUrl::Disc { device } => {
            let mut disc_args = Vec::new();
            if let Some(d) = device {
                disc_args.push("-d".to_string());
                disc_args.push(d.to_string_lossy().to_string());
            }
            disc_args.extend_from_slice(&args[1..]);
            // --share routes to drive-info module (capture + GitHub submit)
            if disc_args.iter().any(|a| a == "--share" || a == "-s") {
                drive_info::run(&disc_args);
            } else {
                disc_info::run(&disc_args);
            }
        }
        libfreemkv::StreamUrl::M2ts { .. }
        | libfreemkv::StreamUrl::Mkv { .. }
        | libfreemkv::StreamUrl::Iso { .. } => {
            match libfreemkv::input(url, &libfreemkv::InputOptions::default()) {
                Ok(stream) => {
                    let meta = stream.info();
                    println!("File: {}", parsed.path_str());
                    if meta.duration_secs > 0.0 {
                        let d = meta.duration_secs;
                        println!(
                            "Duration: {}:{:02}:{:02}",
                            d as u64 / 3600,
                            (d as u64 % 3600) / 60,
                            d as u64 % 60
                        );
                    }
                    println!("Streams: {}", meta.streams.len());
                    for s in &meta.streams {
                        match s {
                            libfreemkv::Stream::Video(v) => {
                                let label = if v.label.is_empty() {
                                    String::new()
                                } else {
                                    format!(" — {}", v.label)
                                };
                                println!("  {} {}{}", v.codec, v.resolution, label);
                            }
                            libfreemkv::Stream::Audio(a) => {
                                let mut tags: Vec<String> = Vec::new();
                                let purpose_key = match a.purpose {
                                    libfreemkv::LabelPurpose::Commentary => {
                                        Some("stream.purpose.commentary")
                                    }
                                    libfreemkv::LabelPurpose::Descriptive => {
                                        Some("stream.purpose.descriptive")
                                    }
                                    libfreemkv::LabelPurpose::Score => Some("stream.purpose.score"),
                                    libfreemkv::LabelPurpose::Ime => Some("stream.purpose.ime"),
                                    libfreemkv::LabelPurpose::Normal => None,
                                };
                                if let Some(k) = purpose_key {
                                    tags.push(strings::get(k));
                                }
                                if a.secondary {
                                    tags.push(strings::get("stream.secondary"));
                                }
                                if !a.label.is_empty() {
                                    tags.push(a.label.clone());
                                }
                                let label = if tags.is_empty() {
                                    String::new()
                                } else {
                                    format!(" — {}", tags.join(", "))
                                };
                                println!("  {} {} {}{}", a.codec, a.channels, a.language, label);
                            }
                            libfreemkv::Stream::Subtitle(s) => {
                                println!("  {} {}", s.codec, s.language);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        libfreemkv::StreamUrl::Unknown { .. } => {
            eprintln!(
                "'{}' is not a valid URL — use scheme://path (e.g. disc://, mkv://movie.mkv)",
                url
            );
            std::process::exit(1);
        }
        _ => {
            eprintln!("Cannot get info for {}", url);
            std::process::exit(1);
        }
    }
}
