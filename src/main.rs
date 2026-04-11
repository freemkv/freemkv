// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// AGPL-3.0 — freemkv project
//
// Usage: freemkv <source> <dest> [flags]
//        freemkv info <url> [flags]
//
// Examples:
//   freemkv disc:// Dune.mkv
//   freemkv disc:///dev/sg4 Dune.m2ts
//   freemkv Dune.m2ts Dune.mkv
//   freemkv disc:// network://10.1.7.11:9000
//   freemkv info disc://

mod info;
mod disc_info;
mod rip;
mod remux;
mod strings;
mod output;
mod pipe;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Parse --language before anything else
    let mut filtered = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if (args[i] == "--language" || args[i] == "--lang") && i + 1 < args.len() {
            strings::set_language(&args[i + 1]);
            i += 2;
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }
    let args = filtered;
    strings::init();

    if args.len() < 2 {
        usage();
        std::process::exit(0);
    }

    match args[1].as_str() {
        "info" => info_cmd(&args[2..]),
        "update-keys" => update_keys(&args[2..]),
        "version" | "--version" | "-V" => println!("{}", env!("CARGO_PKG_VERSION")),
        "help" | "--help" | "-h" => usage(),

        // Everything else: freemkv <source> <dest>
        _ => {
            // Collect URLs (non-flag args) and flags
            let mut urls = Vec::new();
            let mut flags = Vec::new();
            for arg in &args[1..] {
                if arg.starts_with('-') {
                    flags.push(arg.clone());
                } else {
                    urls.push(arg.clone());
                }
            }

            if urls.len() == 2 {
                pipe::run(&urls[0], &urls[1], &args[1..]);
            } else if urls.len() == 1 {
                // Single URL with no dest — show info
                info_cmd(&args[1..]);
            } else {
                eprintln!("Usage: freemkv <source> <dest> [flags]");
                eprintln!("       freemkv info <url>");
                eprintln!();
                eprintln!("Try 'freemkv help' for more.");
                std::process::exit(1);
            }
        }
    }
}

fn info_cmd(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: freemkv info <url>");
        std::process::exit(1);
    }

    let url = &args[0];
    let parsed = libfreemkv::parse_url(url);

    match parsed.scheme.as_str() {
        "disc" => {
            // Merge remaining flags into disc_info args
            let mut disc_args = Vec::new();
            if !parsed.path.is_empty() {
                disc_args.push("-d".to_string());
                disc_args.push(parsed.path);
            }
            disc_args.extend_from_slice(&args[1..]);
            disc_info::run(&disc_args);
        }
        "m2ts" | "mkv" => {
            // Show stream metadata
            use libfreemkv::IOStream;
            match libfreemkv::open_input(url, &libfreemkv::InputOptions::default()) {
                Ok(stream) => {
                    let meta = stream.info();
                    println!("File: {}", parsed.path);
                    if meta.duration_secs > 0.0 {
                        let d = meta.duration_secs;
                        println!("Duration: {}:{:02}:{:02}", d as u64 / 3600, (d as u64 % 3600) / 60, d as u64 % 60);
                    }
                    println!("Streams: {}", meta.streams.len());
                    for s in &meta.streams {
                        match s {
                            libfreemkv::Stream::Video(v) => {
                                let label = if v.label.is_empty() { String::new() } else { format!(" — {}", v.label) };
                                println!("  {:?} {}{}", v.codec, v.resolution, label);
                            }
                            libfreemkv::Stream::Audio(a) => {
                                let label = if a.label.is_empty() { String::new() } else { format!(" — {}", a.label) };
                                println!("  {:?} {} {}{}", a.codec, a.channels, a.language, label);
                            }
                            libfreemkv::Stream::Subtitle(s) => {
                                println!("  {:?} {}", s.codec, s.language);
                            }
                        }
                    }
                }
                Err(e) => { eprintln!("Error: {}", e); std::process::exit(1); }
            }
        }
        _ => {
            eprintln!("Cannot get info for scheme: {}", parsed.scheme);
            std::process::exit(1);
        }
    }
}

fn usage() {
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage: freemkv <source> <dest> [flags]");
    println!("       freemkv info <url> [flags]");
    println!();
    println!("Stream URLs:");
    println!("  disc://                Optical drive (auto-detect)");
    println!("  disc:///dev/sg4        Optical drive (specific device)");
    println!("  m2ts://path.m2ts      BD transport stream file");
    println!("  mkv://path.mkv        Matroska container file");
    println!("  network://host:port   TCP stream");
    println!();
    println!("  Bare paths infer scheme from extension:");
    println!("  Dune.mkv  →  mkv://Dune.mkv");
    println!("  Dune.m2ts →  m2ts://Dune.m2ts");
    println!();
    println!("Examples:");
    println!("  freemkv disc:// Dune.mkv                    Rip to MKV");
    println!("  freemkv disc:// Dune.m2ts                   Rip to raw");
    println!("  freemkv disc:// network://10.1.7.11:9000    Rip to network");
    println!("  freemkv Dune.m2ts Dune.mkv                  Remux file");
    println!("  freemkv network://0.0.0.0:9000 Dune.mkv     Remux from network");
    println!("  freemkv info disc://                         Show disc info");
    println!("  freemkv info Dune.m2ts                       Show file info");
    println!();
    println!("Flags:");
    println!("  -t, --title N       Which title (default: longest)");
    println!("  -k, --keydb PATH    KEYDB.cfg path");
    println!("  -v, --verbose       Show AACS debug info");
    println!("  -q, --quiet         Suppress output");
    println!("  -l, --list          List titles only (with disc://)");
    println!("  -s, --share         Submit drive profile (with info disc://)");
    println!("  -m, --mask          Mask serial numbers");
}

fn update_keys(args: &[String]) {
    let mut url: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--url" | "-u" => { i += 1; url = args.get(i).map(|s| s.as_str()); }
            _ => {}
        }
        i += 1;
    }
    let url = match url {
        Some(u) => u,
        None => { eprintln!("{}", strings::get("keys.usage")); std::process::exit(1); }
    };
    match libfreemkv::keydb::update(url) {
        Ok(result) => {
            println!("{}", strings::fmt("keys.updated", &[
                ("entries", &result.entries.to_string()),
                ("bytes", &result.bytes.to_string()),
            ]));
            println!("{}", strings::fmt("keys.saved", &[("path", &result.path.display().to_string())]));
        }
        Err(e) => { eprintln!("{}", e); std::process::exit(1); }
    }
}
