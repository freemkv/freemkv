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
    if std::env::var("RUST_LOG").is_ok() {
        tracing_subscriber::fmt::init();
    }
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
        "verify" => verify_cmd(&args[2..]),
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
            if let Some(d) = device {
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

fn verify_cmd(args: &[String]) {
    let url = args.first().map(|s| s.as_str()).unwrap_or("disc://");
    let parsed = libfreemkv::parse_url(url);

    let device = match &parsed {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => p.clone(),
        libfreemkv::StreamUrl::Disc { device: None } => match libfreemkv::find_drive() {
            Some(d) => std::path::PathBuf::from(d.device_path()),
            None => {
                eprintln!("No drive found");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("verify only works with disc:// URLs");
            std::process::exit(1);
        }
    };

    println!("freemkv {}\n", env!("CARGO_PKG_VERSION"));

    // Open and scan
    eprint!("Opening drive...");
    let mut drive = match libfreemkv::Drive::open(&device) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("FAILED\n{}", e);
            std::process::exit(1);
        }
    };
    let _ = drive.wait_ready();
    let _ = drive.init();
    eprintln!("OK");

    eprint!("Scanning...");
    let scan_opts = libfreemkv::ScanOptions::default();
    let disc = match libfreemkv::Disc::scan(&mut drive, &scan_opts) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("FAILED\n{}", e);
            std::process::exit(1);
        }
    };
    if disc.titles.is_empty() {
        eprintln!("No titles found");
        std::process::exit(1);
    }
    let title = &disc.titles[0];
    let disc_name = disc.meta_title.as_deref().unwrap_or(&disc.volume_id);
    let total_sectors: u64 = title.extents.iter().map(|e| e.sector_count as u64).sum();
    let total_gb = total_sectors as f64 * 2048.0 / 1_073_741_824.0;
    eprintln!(
        "{} ({:.1} GB, {} sectors)",
        disc_name, total_gb, total_sectors
    );

    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());
    let _ = drive.probe_disc();

    eprintln!("\nVerifying...");
    let start = std::time::Instant::now();
    let last_print = std::sync::Mutex::new(std::time::Instant::now());

    let result = libfreemkv::verify::verify_title(
        &mut drive,
        title,
        batch,
        Some(&|p: &libfreemkv::progress::PassProgress| {
            let mut lp = last_print.lock().unwrap();
            if lp.elapsed().as_secs_f64() >= 1.0 || p.work_done == p.work_total {
                let pct = if p.work_total > 0 {
                    p.work_done * 100 / p.work_total
                } else {
                    0
                };
                let elapsed = start.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 {
                    p.bytes_good_total as f64 / (1024.0 * 1024.0) / elapsed
                } else {
                    0.0
                };
                eprint!(
                    "\r  {}% · {:.1} MB/s · {} / {} sectors",
                    pct, speed, p.work_done, p.work_total
                );
                *lp = std::time::Instant::now();
            }
            true // continue
        }),
    );
    eprintln!(); // newline after progress

    // Results
    println!();
    println!("Results:");
    println!(
        "  Good:        {:>12}  ({:.4}%)",
        result.good,
        result.good as f64 / result.total_sectors as f64 * 100.0
    );
    if result.slow > 0 {
        println!(
            "  Slow:        {:>12}  ({:.4}%)",
            result.slow,
            result.slow as f64 / result.total_sectors as f64 * 100.0
        );
    }
    if result.recovered > 0 {
        println!(
            "  Recovered:   {:>12}  ({:.4}%)",
            result.recovered,
            result.recovered as f64 / result.total_sectors as f64 * 100.0
        );
    }
    if result.bad > 0 {
        println!(
            "  Bad:         {:>12}  ({:.4}%)",
            result.bad,
            result.bad as f64 / result.total_sectors as f64 * 100.0
        );
    }

    if !result.ranges.is_empty() {
        println!();
        for range in &result.ranges {
            let status_str = match range.status {
                libfreemkv::verify::SectorStatus::Slow => "SLOW",
                libfreemkv::verify::SectorStatus::Recovered => "RECOVERED",
                libfreemkv::verify::SectorStatus::Bad => "BAD",
                _ => continue,
            };
            let gb = range.byte_offset as f64 / 1_073_741_824.0;
            let chapter_info = libfreemkv::verify::VerifyResult::chapter_at_offset(
                &title.chapters,
                range.byte_offset,
                title.duration_secs,
                title.size_bytes,
            );
            let ch_str = match chapter_info {
                Some((ch, secs)) => {
                    let m = secs as u32 / 60;
                    let s = secs as u32 % 60;
                    format!(" — Chapter {}, {:02}:{:02}", ch, m, s)
                }
                None => String::new(),
            };
            println!(
                "  {} sectors {}-{} ({:.1} GB{}): {} sectors",
                status_str,
                range.start_lba,
                range.start_lba + range.count,
                gb,
                ch_str,
                range.count
            );
        }
    }

    let elapsed = result.elapsed_secs;
    let m = elapsed as u32 / 60;
    let s = elapsed as u32 % 60;
    println!();
    println!(
        "Verdict: {:.4}% readable in {}:{:02}",
        result.readable_pct(),
        m,
        s
    );

    if result.is_perfect() {
        println!("         Disc is perfect.");
    } else if result.bad > 0 {
        println!(
            "         {} unrecoverable sectors in {} cluster(s).",
            result.bad,
            result
                .ranges
                .iter()
                .filter(|r| r.status == libfreemkv::verify::SectorStatus::Bad)
                .count()
        );
    }

    std::process::exit(if result.bad > 0 { 1 } else { 0 });
}

fn usage() {
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage: freemkv <source> <dest> [flags]");
    println!("       freemkv info <url> [flags]");
    println!("       freemkv verify [disc://]");
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
    println!(
        "  freemkv disc:// iso://Disc.iso                     Full disc to ISO (auto-resumes)"
    );
    println!(
        "  freemkv disc:// iso://Disc.iso --raw               Full disc, no decryption (auto-resumes)"
    );
    println!(
        "  freemkv disc:// iso://Disc.iso --multipass        Sweep with mapfile for multipass recovery"
    );
    println!(
        "  freemkv iso://Disc.iso iso://Disc.iso --multipass Patch bad sectors (one retry pass)"
    );
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
    println!("      --multipass    Write/update mapfile for multipass recovery");
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
