//! Pipe — the core operation. Open source stream, open dest stream, copy.
//!
//! freemkv <source_url> <dest_url> [flags]

use crate::strings;
use crate::output::{Output, Level::Normal};
use std::io::{Read, Write};
use libfreemkv::IOStream;

pub fn run(source: &str, dest: &str, args: &[String]) {
    // Parse flags
    let mut verbose = false;
    let mut quiet = false;
    let mut keydb_path: Option<String> = None;
    let mut title_num: Option<usize> = None;
    let mut list_only = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-v" | "--verbose" => verbose = true,
            "-q" | "--quiet" => quiet = true,
            "-l" | "--list" => list_only = true,
            "-t" | "--title" => { i += 1; title_num = args.get(i).and_then(|s| s.parse::<usize>().ok().map(|n| n - 1)); }
            "-k" | "--keydb" => { i += 1; keydb_path = args.get(i).cloned(); }
            _ => {} // URLs handled by caller
        }
        i += 1;
    }

    let out = Output::new(verbose, quiet);

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);

    // Open input stream
    out.raw_inline(Normal, &format!("Opening {}... ", source));
    let input_opts = libfreemkv::InputOptions {
        keydb_path,
        title_index: title_num,
    };
    let mut input = match libfreemkv::open_input(source, &input_opts) {
        Ok(s) => { out.raw(Normal, "OK"); s }
        Err(e) => { out.raw(Normal, "FAILED"); eprintln!("  {}", e); std::process::exit(1); }
    };

    let meta = input.info().clone();

    // Show metadata
    out.raw(Normal, &format!("  Streams: {}", meta.streams.len()));
    for s in &meta.streams {
        match s {
            libfreemkv::Stream::Video(v) => {
                let label = if v.label.is_empty() { String::new() } else { format!(" — {}", v.label) };
                out.raw(Normal, &format!("    {:?} {}{}", v.codec, v.resolution, label));
            }
            libfreemkv::Stream::Audio(a) => {
                let label = if a.label.is_empty() { String::new() } else { format!(" — {}", a.label) };
                out.raw(Normal, &format!("    {:?} {} {}{}", a.codec, a.channels, a.language, label));
            }
            libfreemkv::Stream::Subtitle(s) => {
                out.raw(Normal, &format!("    {:?} {}", s.codec, s.language));
            }
        }
    }
    if meta.duration_secs > 0.0 {
        let d = meta.duration_secs;
        out.raw(Normal, &format!("  Duration: {}:{:02}:{:02}",
            d as u64 / 3600, (d as u64 % 3600) / 60, d as u64 % 60));
    }

    if list_only { return; }

    // Open output stream
    out.raw_inline(Normal, &format!("Opening {}... ", dest));
    let mut output = match libfreemkv::open_output(dest, &meta) {
        Ok(s) => { out.raw(Normal, "OK"); s }
        Err(e) => { out.raw(Normal, "FAILED"); eprintln!("  {}", e); std::process::exit(1); }
    };

    // Pipe: source → dest
    out.blank(Normal);
    out.raw_inline(Normal, "Copying... ");

    let start = std::time::Instant::now();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 192 * 1024]; // 1024 BD-TS packets

    loop {
        match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if output.write_all(&buf[..n]).is_err() { break; }
                total += n as u64;
            }
            Err(_) => break,
        }
    }

    let _ = output.finish();

    let elapsed = start.elapsed().as_secs_f64();
    let mb = total as f64 / (1024.0 * 1024.0);
    let (sz, unit) = if mb >= 1024.0 { (mb / 1024.0, "GB") } else { (mb, "MB") };

    out.raw(Normal, "done");
    out.blank(Normal);
    out.raw(Normal, &format!("  {:.1} {} in {:.0}s ({:.0} MB/s)", sz, unit, elapsed, mb / elapsed));
    out.raw(Normal, &format!("  {} → {}", source, dest));
}
