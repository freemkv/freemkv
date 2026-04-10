//! freemkv remux — Convert m2ts to MKV without a disc drive.

use crate::strings;
use crate::output::{Output, Level::Normal};
use std::io::{BufWriter, BufReader, Read, Write};

pub fn run(args: &[String]) {
    if args.is_empty() {
        eprintln!("{}", strings::get("remux.usage"));
        std::process::exit(1);
    }

    let input_path = &args[0];
    let output_path = if args.len() > 1 {
        args[1].clone()
    } else {
        input_path.replace(".m2ts", ".mkv")
    };

    if output_path == *input_path {
        eprintln!("{}", strings::get("remux.same_path"));
        std::process::exit(1);
    }

    let out = Output::new(false, false);

    let infile = match std::fs::File::open(input_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}", strings::fmt("error.open_failed", &[("device", input_path), ("error", &e.to_string())]));
            std::process::exit(1);
        }
    };
    let file_size = infile.metadata().map(|m| m.len()).unwrap_or(0);
    let mut reader = BufReader::with_capacity(4 * 1024 * 1024, infile);

    out.raw(Normal, &format!("freemkv remux v{}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);
    out.raw(Normal, &format!("{}:  {} ({:.1} GB)", strings::get("remux.input"), input_path, file_size as f64 / (1024.0 * 1024.0 * 1024.0)));
    out.raw(Normal, &format!("{}: {}", strings::get("remux.output_label"), output_path));

    out.print_inline(Normal, "remux.scanning_streams");
    out.raw_inline(Normal, " ");
    let mut scan_buf = vec![0u8; 1024 * 1024];
    let scan_bytes = reader.read(&mut scan_buf).unwrap_or(0);

    let streams = match scan_ts_streams(&scan_buf[..scan_bytes]) {
        Some(s) if !s.is_empty() => {
            out.raw(Normal, &format!("{} ({} streams)", strings::get("rip.ok"), s.len()));
            s
        }
        _ => {
            out.print(Normal, "rip.failed");
            eprintln!("{}", strings::get("remux.scan_failed"));
            std::process::exit(1);
        }
    };

    for s in &streams {
        match s {
            libfreemkv::Stream::Video(v) => out.raw(Normal, &format!("  {:?} {}", v.codec, v.resolution)),
            libfreemkv::Stream::Audio(a) => out.raw(Normal, &format!("  {:?} {} {}", a.codec, a.channels, a.language)),
            libfreemkv::Stream::Subtitle(s) => out.raw(Normal, &format!("  {:?} {}", s.codec, s.language)),
        }
    }

    let title = libfreemkv::Title {
        playlist: String::new(),
        playlist_id: 0,
        duration_secs: 0.0,
        size_bytes: file_size,
        clips: Vec::new(),
        streams,
        extents: Vec::new(),
    };

    let outfile = match std::fs::File::create(&output_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}", strings::fmt("rip.cannot_create", &[("path", &output_path), ("error", &e.to_string())]));
            std::process::exit(1);
        }
    };
    let buf_writer = BufWriter::with_capacity(4 * 1024 * 1024, outfile);
    let mut mkv = libfreemkv::MkvStream::new(buf_writer)
        .title(&title)
        .max_buffer(10 * 1024 * 1024);

    out.blank(Normal);
    out.print_inline(Normal, "remux.remuxing");
    out.raw_inline(Normal, " ");

    let start = std::time::Instant::now();
    let mut total_read = scan_bytes as u64;

    mkv.write_all(&scan_buf[..scan_bytes]).unwrap();

    let mut buf = vec![0u8; 192 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                mkv.write_all(&buf[..n]).unwrap();
                total_read += n as u64;
            }
            Err(e) => {
                eprintln!("\n{}", strings::fmt("rip.read_error", &[("error", &e.to_string())]));
                break;
            }
        }
    }

    let _ = mkv.finish();

    let elapsed = start.elapsed().as_secs_f64();
    let mb = total_read as f64 / (1024.0 * 1024.0);
    out.print(Normal, "remux.done");
    out.blank(Normal);
    out.fmt(Normal, "remux.remuxed", &[
        ("size", &format!("{:.1}", mb / 1024.0)),
        ("time", &format!("{:.0}", elapsed)),
        ("speed", &format!("{:.0}", mb / elapsed)),
    ]);
    out.raw(Normal, &format!("{}: {}", strings::get("remux.output_label"), output_path));
}

/// Quick scan of TS data to find streams via PAT/PMT.
fn scan_ts_streams(data: &[u8]) -> Option<Vec<libfreemkv::Stream>> {
    let mut pat_pmt_pid: Option<u16> = None;
    let mut streams = Vec::new();
    let mut offset = 0;

    while offset + 192 <= data.len() {
        if data[offset + 4] != 0x47 {
            offset += 1;
            continue;
        }
        let pid = (((data[offset + 5] & 0x1F) as u16) << 8) | data[offset + 6] as u16;
        let pusi = data[offset + 5] & 0x40 != 0;

        if pid == 0 && pusi {
            let payload_start = offset + 4 + 4;
            if payload_start + 12 < data.len() {
                let pointer = data[payload_start] as usize;
                let pat_start = payload_start + 1 + pointer;
                if pat_start + 12 < data.len() && data[pat_start] == 0x00 {
                    let section_len = (((data[pat_start + 1] & 0x0F) as usize) << 8) | data[pat_start + 2] as usize;
                    let entries_start = pat_start + 8;
                    let _entries_end = pat_start + 3 + section_len - 4;
                    if entries_start + 4 <= data.len() {
                        let pmt_pid = (((data[entries_start + 2] & 0x1F) as u16) << 8) | data[entries_start + 3] as u16;
                        pat_pmt_pid = Some(pmt_pid);
                    }
                }
            }
        }
        offset += 192;
    }

    let pmt_pid = pat_pmt_pid?;

    offset = 0;
    while offset + 192 <= data.len() {
        if data[offset + 4] != 0x47 { offset += 1; continue; }
        let pid = (((data[offset + 5] & 0x1F) as u16) << 8) | data[offset + 6] as u16;
        let pusi = data[offset + 5] & 0x40 != 0;

        if pid == pmt_pid && pusi {
            let payload_start = offset + 4 + 4;
            if payload_start + 1 >= data.len() { offset += 192; continue; }
            let pointer = data[payload_start] as usize;
            let pmt_start = payload_start + 1 + pointer;
            if pmt_start + 12 >= data.len() { offset += 192; continue; }
            if data[pmt_start] != 0x02 { offset += 192; continue; }

            let section_len = (((data[pmt_start + 1] & 0x0F) as usize) << 8) | data[pmt_start + 2] as usize;
            let prog_info_len = (((data[pmt_start + 10] & 0x0F) as usize) << 8) | data[pmt_start + 11] as usize;
            let mut pos = pmt_start + 12 + prog_info_len;
            let end = pmt_start + 3 + section_len - 4;

            while pos + 5 <= data.len() && pos < end {
                let stream_type = data[pos];
                let es_pid = (((data[pos + 1] & 0x1F) as u16) << 8) | data[pos + 2] as u16;
                let es_info_len = (((data[pos + 3] & 0x0F) as usize) << 8) | data[pos + 4] as usize;

                let stream = match stream_type {
                    0x1B => Some(libfreemkv::Stream::Video(libfreemkv::VideoStream {
                        pid: es_pid, codec: libfreemkv::Codec::H264,
                        resolution: "1080p".into(), frame_rate: String::new(),
                        hdr: libfreemkv::HdrFormat::Sdr, color_space: libfreemkv::ColorSpace::Bt709,
                        secondary: false, label: String::new(),
                    })),
                    0x24 => Some(libfreemkv::Stream::Video(libfreemkv::VideoStream {
                        pid: es_pid, codec: libfreemkv::Codec::Hevc,
                        resolution: "2160p".into(), frame_rate: String::new(),
                        hdr: libfreemkv::HdrFormat::Sdr, color_space: libfreemkv::ColorSpace::Bt709,
                        secondary: false, label: String::new(),
                    })),
                    0xEA => Some(libfreemkv::Stream::Video(libfreemkv::VideoStream {
                        pid: es_pid, codec: libfreemkv::Codec::Vc1,
                        resolution: "1080p".into(), frame_rate: String::new(),
                        hdr: libfreemkv::HdrFormat::Sdr, color_space: libfreemkv::ColorSpace::Bt709,
                        secondary: false, label: String::new(),
                    })),
                    0x02 => Some(libfreemkv::Stream::Video(libfreemkv::VideoStream {
                        pid: es_pid, codec: libfreemkv::Codec::Mpeg2,
                        resolution: "1080i".into(), frame_rate: String::new(),
                        hdr: libfreemkv::HdrFormat::Sdr, color_space: libfreemkv::ColorSpace::Bt709,
                        secondary: false, label: String::new(),
                    })),
                    0x81 | 0x83 => Some(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
                        pid: es_pid, codec: libfreemkv::Codec::Ac3,
                        channels: "5.1".into(), language: "und".into(),
                        sample_rate: "48kHz".into(), secondary: false, label: String::new(),
                    })),
                    0x84 | 0xA1 => Some(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
                        pid: es_pid, codec: libfreemkv::Codec::Ac3Plus,
                        channels: "5.1".into(), language: "und".into(),
                        sample_rate: "48kHz".into(), secondary: false, label: String::new(),
                    })),
                    0x85 | 0x86 => Some(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
                        pid: es_pid, codec: libfreemkv::Codec::DtsHdMa,
                        channels: "5.1".into(), language: "und".into(),
                        sample_rate: "48kHz".into(), secondary: false, label: String::new(),
                    })),
                    0x82 => Some(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
                        pid: es_pid, codec: libfreemkv::Codec::Dts,
                        channels: "5.1".into(), language: "und".into(),
                        sample_rate: "48kHz".into(), secondary: false, label: String::new(),
                    })),
                    0x90 => Some(libfreemkv::Stream::Subtitle(libfreemkv::SubtitleStream {
                        pid: es_pid, codec: libfreemkv::Codec::Pgs,
                        language: "und".into(), forced: false,
                    })),
                    _ => None,
                };

                if let Some(s) = stream {
                    streams.push(s);
                }
                pos += 5 + es_info_len;
            }
            break;
        }
        offset += 192;
    }

    if streams.is_empty() { None } else { Some(streams) }
}
