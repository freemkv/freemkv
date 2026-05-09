// freemkv info — Inspect a disc, ISO, or stream URL
// AGPL-3.0 — freemkv project
//
// Dispatches `freemkv info <url>` to the right renderer:
//   disc://                     → cmd::disc_info
//   disc:// with --share / -s   → cmd::drive_info
//   iso:// / mkv:// / m2ts://   → libfreemkv::input metadata dump
//
// 0.18: the metadata-dump path reads `info()` through `FrameSource`. The
// `libfreemkv::input` URL dispatcher still hands back `Box<dyn pes::Stream>`
// during the deprecation window — see `pipe.rs` for the local
// `PesSource` adapter shape that bridges that into FrameSource. For
// `info()`-only callers like this one, a tiny inline wrap-and-call is
// enough; we don't pull in the full adapter just to read metadata.

use crate::cmd::{disc_info, drive_info};
use crate::strings;
use libfreemkv::pes::FrameSource;

/// Local adapter: wrap the boxed `pes::Stream` returned by
/// `libfreemkv::input` and expose `FrameSource` so the rest of this file
/// only sees the new trait surface. Mirror of `pipe::PesSource`, kept
/// local because `cmd/info.rs` only needs the read half and only `info()`.
#[allow(deprecated)]
struct InfoSource {
    inner: Box<dyn libfreemkv::PesStream>,
}

// SAFETY: every concrete `pes::Stream` returned by `libfreemkv::input`
// (DiscStream, MkvStream, M2tsStream, NetworkStream, StdioStream) is
// itself `Send` — see the long-form rationale in `pipe.rs`. CLI is
// single-threaded; the Send claim is conservative.
#[allow(deprecated)]
unsafe impl Send for InfoSource {}

#[allow(deprecated)]
impl FrameSource for InfoSource {
    fn read(&mut self) -> std::io::Result<Option<libfreemkv::pes::PesFrame>> {
        libfreemkv::PesStream::read(&mut *self.inner)
    }

    fn info(&self) -> &libfreemkv::DiscTitle {
        libfreemkv::PesStream::info(&*self.inner)
    }

    fn codec_private(&self, track: usize) -> Option<Vec<u8>> {
        libfreemkv::PesStream::codec_private(&*self.inner, track)
    }

    fn headers_ready(&self) -> bool {
        libfreemkv::PesStream::headers_ready(&*self.inner)
    }
}

pub(crate) fn run(args: &[String]) {
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
                    let stream = InfoSource { inner: stream };
                    let meta = FrameSource::info(&stream);
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
