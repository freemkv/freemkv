// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// AGPL-3.0 — freemkv project
//
// Usage: freemkv <source> <dest> [flags]
//        freemkv info <url> [flags]
//
// Examples:
//   freemkv disc:// mkv://Dune.mkv
//   freemkv disc:///dev/sg4 m2ts://Dune.m2ts
//   freemkv m2ts://Dune.m2ts mkv://Dune.mkv
//   freemkv disc:// network://10.1.7.11:9000
//   freemkv info disc://

mod disc_info;
mod info;
mod output;
mod pipe;
mod strings;

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
            // Flags that consume the next argument as a value
            const VALUE_FLAGS: &[&str] = &["-t", "--title", "-k", "--keydb"];

            // Collect URLs (non-flag args) and flags
            let mut urls = Vec::new();
            let mut flags = Vec::new();
            let mut skip_next = false;
            for arg in &args[1..] {
                if skip_next {
                    skip_next = false;
                    continue;
                }
                if arg.starts_with('-') {
                    flags.push(arg.clone());
                    if VALUE_FLAGS.contains(&arg.as_str()) {
                        skip_next = true;
                    }
                } else {
                    urls.push(arg.clone());
                }
            }

            if urls.len() == 2 {
                if !pipe::run(&urls[0], &urls[1], &args[1..]) {
                    std::process::exit(1);
                }
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

    match &parsed {
        libfreemkv::StreamUrl::Disc { device } => {
            let mut disc_args = Vec::new();
            if let Some(ref d) = device {
                disc_args.push("-d".to_string());
                disc_args.push(d.to_string_lossy().to_string());
            }
            disc_args.extend_from_slice(&args[1..]);
            // --share routes to drive-info module (capture + GitHub submit)
            if disc_args.iter().any(|a| a == "--share" || a == "-s") {
                info::run(&disc_args);
            } else {
                disc_info::run(&disc_args);
            }
        }
        libfreemkv::StreamUrl::M2ts { .. }
        | libfreemkv::StreamUrl::Mkv { .. }
        | libfreemkv::StreamUrl::Iso { .. } => {
            match libfreemkv::open_input(url, &libfreemkv::InputOptions::default()) {
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
                                let label = if a.label.is_empty() {
                                    String::new()
                                } else {
                                    format!(" — {}", a.label)
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

fn usage() {
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage: freemkv <source> <dest> [flags]");
    println!("       freemkv info <url> [flags]");
    println!();
    println!("Stream URLs:");
    println!("  disc://                  Optical drive (auto-detect)");
    println!("  disc:///dev/sg4          Optical drive (Linux)");
    println!("  disc://D:                Optical drive (Windows)");
    println!("  mkv://path.mkv           Matroska file");
    println!("  m2ts://path.m2ts         BD transport stream file");
    println!("  network://host:port      TCP stream");
    println!("  stdio://                 Stdin/stdout pipe");
    println!("  iso://image.iso          Blu-ray ISO image");
    println!("  null://                  Discard (benchmarking)");
    println!();
    println!("  All URLs require a scheme:// prefix.");
    println!("  File paths follow the scheme: mkv://./Dune.mkv, m2ts:///tmp/Dune.m2ts");
    println!();
    println!("Examples:");
    println!("  freemkv disc:// mkv://Dune.mkv                     Rip disc to MKV");
    println!("  freemkv disc:// m2ts://Dune.m2ts                   Rip disc to m2ts");
    println!("  freemkv disc:///dev/sg4 mkv://Dune.mkv             Rip specific drive");
    println!("  freemkv disc:// mkv://Movie.mkv                    Rip all titles");
    println!("  freemkv disc:// mkv://Movie.mkv -t 1               Rip main feature only");
    println!("  freemkv disc:// mkv://Movie.mkv -t 1 -t 3          Rip titles 1 and 3");
    println!("  freemkv disc:// iso://Disc.iso                     Full disc to ISO (auto-resumes)");
    println!("  freemkv disc:// iso://Disc.iso --raw               Full disc, no decryption (auto-resumes)");
    println!("  freemkv iso://Disc.iso mkv://Movie.mkv             ISO to MKV");
    println!("  freemkv m2ts://Movie.m2ts mkv://Movie.mkv          Remux m2ts to MKV");
    println!("  freemkv disc:// network://10.1.7.11:9000           Stream to network");
    println!("  freemkv network://0.0.0.0:9000 mkv://Movie.mkv    Receive from network");
    println!("  freemkv disc:// stdio://                           Pipe to stdout");
    println!("  freemkv disc:// null://                            Benchmark read speed");
    println!("  freemkv info disc://                               Show disc info");
    println!();
    println!("Flags:");
    println!("  -t, --title N       Select title (1-based, repeatable). Default: all.");
    println!("  -k, --keydb PATH    KEYDB.cfg path");
    println!("  -v, --verbose       Show AACS/drive debug info");
    println!("  -q, --quiet         Suppress output");
    println!("      --raw           Skip decryption (raw encrypted output)");
    println!("  -s, --share         Submit drive profile (with info disc://)");
    println!("  -m, --mask          Mask serial numbers (with --share)");
}

fn update_keys(args: &[String]) {
    let mut url: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--url" | "-u" => {
                i += 1;
                url = args.get(i).map(|s| s.as_str());
            }
            _ => {}
        }
        i += 1;
    }
    let url = match url {
        Some(u) => u,
        None => {
            eprintln!("{}", strings::get("keys.usage"));
            std::process::exit(1);
        }
    };
    match libfreemkv::keydb::update(url) {
        Ok(result) => {
            println!(
                "{}",
                strings::fmt(
                    "keys.updated",
                    &[
                        ("entries", &result.entries.to_string()),
                        ("bytes", &result.bytes.to_string()),
                    ]
                )
            );
            println!(
                "{}",
                strings::fmt(
                    "keys.saved",
                    &[("path", &result.path.display().to_string())]
                )
            );
        }
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    }
}
