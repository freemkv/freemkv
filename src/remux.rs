//! freemkv remux — Convert m2ts to MKV using stream-to-stream pipeline.
//!
//! Input: M2tsReader (reads m2ts with metadata header or bare m2ts)
//! Output: MkvStream (writes Matroska container)

use crate::strings;
use crate::output::{Output, Level::Normal};
use std::io::{BufWriter, BufReader, Read, Write};
use libfreemkv::IOStream;

/// I/O buffer size for file read/write (4 MB).
const IO_BUF_SIZE: usize = 4 * 1024 * 1024;

/// MKV lookahead buffer for codec header detection (10 MB).
const MKV_LOOKAHEAD: usize = 10 * 1024 * 1024;

/// Transfer buffer size for stream-to-stream copy (192 KB = 1024 BD-TS packets).
const TRANSFER_BUF_SIZE: usize = 192 * 1024;

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

    out.raw(Normal, &format!("freemkv remux v{}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);
    out.raw(Normal, &format!("{}:  {} ({:.1} GB)", strings::get("remux.input"), input_path, file_size as f64 / (1024.0 * 1024.0 * 1024.0)));
    out.raw(Normal, &format!("{}: {}", strings::get("remux.output_label"), output_path));

    // Open input stream — handles FMKV metadata header or bare m2ts fallback
    out.print_inline(Normal, "remux.scanning_streams");
    out.raw_inline(Normal, " ");

    let mut input = match libfreemkv::M2tsStream::open(BufReader::with_capacity(IO_BUF_SIZE, infile)) {
        Ok(r) => {
            out.raw(Normal, &format!("{} ({} streams)", strings::get("rip.ok"), r.info().streams.len()));
            r
        }
        Err(e) => {
            out.print(Normal, "rip.failed");
            eprintln!("{}", strings::fmt("remux.scan_failed", &[("error", &e.to_string())]));
            std::process::exit(1);
        }
    };

    let meta = input.info().clone();

    // Print stream info
    for s in &meta.streams {
        match s {
            libfreemkv::Stream::Video(v) => {
                let label = if v.label.is_empty() { String::new() } else { format!(" — {}", v.label) };
                out.raw(Normal, &format!("  {:?} {}{}", v.codec, v.resolution, label));
            }
            libfreemkv::Stream::Audio(a) => {
                let label = if a.label.is_empty() { String::new() } else { format!(" — {}", a.label) };
                out.raw(Normal, &format!("  {:?} {} {}{}", a.codec, a.channels, a.language, label));
            }
            libfreemkv::Stream::Subtitle(s) => out.raw(Normal, &format!("  {:?} {}", s.codec, s.language)),
        }
    }
    if meta.duration_secs > 0.0 {
        let dur = meta.duration_secs;
        out.raw(Normal, &format!("  Duration: {}:{:02}:{:02}",
            dur as u64 / 3600, (dur as u64 % 3600) / 60, dur as u64 % 60));
    }

    // Create output stream
    let outfile = match std::fs::File::create(&output_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}", strings::fmt("rip.cannot_create", &[("path", &output_path), ("error", &e.to_string())]));
            std::process::exit(1);
        }
    };
    let mut output = libfreemkv::MkvStream::new(BufWriter::with_capacity(IO_BUF_SIZE, outfile))
        .meta(&meta)
        .max_buffer(MKV_LOOKAHEAD);

    // Pipe: input stream → output stream
    out.blank(Normal);
    out.print_inline(Normal, "remux.remuxing");
    out.raw_inline(Normal, " ");

    let start = std::time::Instant::now();
    let mut total_read = 0u64;
    let mut buf = vec![0u8; TRANSFER_BUF_SIZE];

    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                output.write_all(&buf[..n]).unwrap();
                total_read += n as u64;
            }
            Err(e) => {
                eprintln!("\n{}", strings::fmt("rip.read_error", &[("error", &e.to_string())]));
                break;
            }
        }
    }

    let _ = output.finish();

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
