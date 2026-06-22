// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// AGPL-3.0 — freemkv project
//
// Usage: freemkv <source> <dest> [flags]
//        freemkv info <url> [flags]
//
// Examples:
//   freemkv disc:// mkv://Movie.mkv
//   freemkv disc:///dev/sg4 m2ts://Movie.m2ts
//   freemkv m2ts://Movie.m2ts mkv://Movie.mkv
//   freemkv disc:// network://192.0.2.10:9000
//   freemkv info disc://

mod disc_info;
mod info;
mod output;
mod pipe;
mod strings;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Worker guard for the optional non-blocking file log layer. Held for the
/// life of the process so buffered records are flushed on exit; `None` when
/// `--log-file` isn't given.
static LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

/// Initialise tracing. Always installs a subscriber, so `--log-level` and
/// `--log-file` actually drive log output. Logs go to STDERR (stdout stays
/// clean for `mkv://` / `m2ts://` piping); with `--log-file` a non-blocking
/// file layer is added too.
///
/// Filter precedence: `RUST_LOG` wins if set; otherwise `--log-level N` maps
/// 1→warn, 2→info, 3→debug, 4→trace for the `freemkv` and `libfreemkv`
/// targets (everything else stays at error). Default level is 1.
fn init_logging(args: &[String]) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    // `--log-level N` (1=warn, 2=info, 3=debug, 4=trace). Default 1. The
    // per-subcommand parsers read the same flag to widen stdout detail at >=2.
    let mut level_num = 1u8;
    let mut log_file: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--log-level" => {
                if let Some(n) = it.next().and_then(|s| s.parse::<u8>().ok()) {
                    level_num = n.clamp(1, 4);
                }
            }
            "--log-file" => {
                if let Some(p) = it.next() {
                    log_file = Some(p.clone());
                }
            }
            _ => {}
        }
    }

    let env_filter = if std::env::var("RUST_LOG").is_ok() {
        EnvFilter::from_default_env()
    } else {
        let level = match level_num {
            1 => "warn",
            2 => "info",
            3 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("error,freemkv={level},libfreemkv={level}"))
    };

    // STDERR layer — always. Keep stdout clean for stream piping.
    let stderr_layer = fmt::layer().with_writer(std::io::stderr);

    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(stderr_layer);

    if let Some(path) = log_file {
        let p = std::path::Path::new(&path);
        let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
        let file_appender = match (dir, p.file_name()) {
            (Some(dir), Some(name)) => tracing_appender::rolling::never(dir, name),
            (None, Some(name)) => tracing_appender::rolling::never(".", name),
            _ => {
                eprintln!("--log-file: invalid path '{path}'");
                registry.init();
                return;
            }
        };
        let (nb, guard) = tracing_appender::non_blocking(file_appender);
        let _ = LOG_GUARD.set(guard);
        let file_layer = fmt::layer().with_ansi(false).with_writer(nb);
        registry.with(file_layer).init();
    } else {
        registry.init();
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    init_logging(&args);

    // Plug in the LibreDrive firmware unlocker. libfreemkv ships only the
    // pluggable `Unlocker` seam; this one line registers the firmware-unlock
    // implementation so matching drives are unlocked at drive-prep. Remove it
    // (and the freemkv-unlock-ld dep) and the CLI still builds — drives fall
    // back to the host-cert AACS handshake.
    libfreemkv::register_unlocker(Box::new(freemkv_unlock_ld::LibreDrive::new()));

    // Parse --language before anything else.
    //
    // Apply the same is-URL guard `collect_urls` uses: a value-flag must not
    // swallow a following positional stream URL. `freemkv --language disc://
    // mkv://out.mkv` would otherwise eat `disc://` as the "language", leaving a
    // single URL that silently degrades into an info/usage no-op. The same
    // applies to a following flag token (e.g. `freemkv --language --verbose
    // ...`): a leading `-` means the value is missing, not a language code. If
    // the next token is a URL, a flag, or --language is the last token, the
    // value is missing: warn and leave the token as positional. Strings aren't
    // initialized yet, so this diagnostic is necessarily plain English.
    let mut filtered = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--language" || args[i] == "--lang" {
            match args.get(i + 1) {
                Some(v) if !is_url(v) && !v.starts_with('-') => {
                    strings::set_language(v);
                    i += 2;
                }
                _ => {
                    eprintln!("{}: requires a language code (e.g. --language de)", args[i]);
                    i += 1;
                }
            }
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }
    let args = filtered;
    strings::init();

    if args.len() < 2 {
        // Bare invocation with no subcommand/URL: print usage but exit non-zero
        // so a scripted `freemkv; echo $?` (e.g. a misconfigured wrapper) sees a
        // failure rather than a false success. Explicit `help`/`--help`/`-h`
        // still exits 0 (handled below).
        usage();
        std::process::exit(2);
    }

    match args[1].as_str() {
        "info" => info_cmd(&args[2..]),
        "verify" => verify_cmd(&args[2..]),
        "update-keys" => update_keys(&args[2..]),
        "version" | "--version" | "-V" => println!("{}", env!("CARGO_PKG_VERSION")),
        "help" | "--help" | "-h" => usage(),

        // Everything else: freemkv <source> <dest>
        _ => {
            let urls = collect_urls(&args[1..]);

            if urls.len() == 2 {
                if !pipe::run(&urls[0], &urls[1], &args[1..]) {
                    std::process::exit(1);
                }
            } else if urls.len() == 1 {
                // Single URL with no dest — show info. `info_cmd` treats its
                // `args[0]` as the URL, but a preceding flag (e.g. `freemkv
                // --verbose disc://`) would otherwise sit at `args[0]` and be
                // parsed as the URL. `collect_urls` already resolved the real
                // URL token, so put it first and append the remaining (non-URL)
                // flag tokens so downstream flags like `-d`/`--share` survive.
                let mut info_args = vec![urls[0].clone()];
                info_args.extend(args[1..].iter().filter(|a| **a != urls[0]).cloned());
                info_cmd(&info_args);
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

/// True if `s` looks like a stream URL (`scheme://...`).
fn is_url(s: &str) -> bool {
    s.contains("://")
}

/// Split positional stream URLs out of an argument list, accounting for
/// value-taking flags (`-t`, `-k`).
///
/// A value-flag normally consumes the following token as its value, but it must
/// NOT swallow a positional stream URL (`scheme://...`): `freemkv -k disc://
/// mkv://out.mkv` would otherwise let `-k` eat `disc://`, leaving a single URL
/// that silently routes to `info` instead of ripping. So if a value-flag is
/// followed by a URL token, the URL is kept as positional and the flag's value
/// is treated as absent (pipe::run then reports the missing value).
fn collect_urls(args: &[String]) -> Vec<String> {
    // Flags that consume the next argument as a value.
    const VALUE_FLAGS: &[&str] = &[
        "-t",
        "--title",
        "-k",
        "--keydb",
        "--log-file",
        "--log-level",
    ];

    let mut urls = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            // The greedy flag's "value" is actually a positional URL —
            // reclassify it as a URL instead of consuming it.
            if is_url(arg) {
                urls.push(arg.clone());
            }
            continue;
        }
        if arg.starts_with('-') {
            if VALUE_FLAGS.contains(&arg.as_str()) {
                skip_next = true;
            }
        } else {
            urls.push(arg.clone());
        }
    }
    urls
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
        libfreemkv::StreamUrl::Iso { path } => {
            // Listing titles needs NO AACS key — only clear UDF navigation.
            // Scan the ISO keylessly and reuse disc_info's full title list
            // (duration, size, clip count, video/audio/subtitle streams).
            // Going through the key-gated `input()` here would hit libfreemkv's
            // no-key gate and surface E7022 for an encrypted disc, and would
            // only ever open a single title. `-k`/`--keydb` is accepted but the
            // listing never requires it. `--full` shows every title.
            let full = args[1..].iter().any(|a| a == "--full" || a == "-f");
            let mut reader = match libfreemkv::FileSectorSource::open(path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };
            let capacity =
                <libfreemkv::FileSectorSource as libfreemkv::SectorSource>::capacity_sectors(
                    &reader,
                );
            let disc = match libfreemkv::Disc::scan_image(
                &mut reader,
                capacity,
                &libfreemkv::ScanOptions::default(),
            ) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            };
            println!("freemkv {}", env!("CARGO_PKG_VERSION"));
            println!();
            disc_info::print_disc_titles(&disc, full);
        }
        libfreemkv::StreamUrl::M2ts { .. } | libfreemkv::StreamUrl::Mkv { .. } => {
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
    // init()/probe_disc() are best-effort: many drives lack the firmware
    // support they probe for and `Disc::scan` below is the authoritative gate.
    // But don't print an unconditional "OK" that masks a genuine failure —
    // report WARN and the error so a later scan failure isn't mysterious.
    let _ = drive.wait_ready();
    match drive.init() {
        Ok(_) => eprintln!("OK"),
        Err(e) => eprintln!("WARN ({})", e),
    }

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

    // Results. Guard the divisor: a title whose extents sum to zero sectors
    // would otherwise print `NaN%` on every row (mirrors the library's own
    // `VerifyResult::readable_pct` zero-guard).
    let pct = |n: u64| pct_of(n, result.total_sectors);
    println!();
    println!("Results:");
    println!(
        "  Good:        {:>12}  ({:.4}%)",
        result.good,
        pct(result.good)
    );
    if result.slow > 0 {
        println!(
            "  Slow:        {:>12}  ({:.4}%)",
            result.slow,
            pct(result.slow)
        );
    }
    if result.recovered > 0 {
        println!(
            "  Recovered:   {:>12}  ({:.4}%)",
            result.recovered,
            pct(result.recovered)
        );
    }
    if result.bad > 0 {
        println!(
            "  Bad:         {:>12}  ({:.4}%)",
            result.bad,
            pct(result.bad)
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
            // `count` is a half-open span; the displayed range is inclusive, so
            // the last LBA is start + count - 1. Guard count==0 (would underflow
            // and contradict the trailing "0 sectors").
            let last_lba = inclusive_last_lba(range.start_lba, range.count);
            println!(
                "  {} sectors {}-{} ({:.1} GB{}): {} sectors",
                status_str, range.start_lba, last_lba, gb, ch_str, range.count
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

/// `n` as a percentage of `total`, guarding the zero divisor (which would yield
/// `NaN%`). Returns 0.0 when `total == 0`.
fn pct_of(n: u64, total: u64) -> f64 {
    if total > 0 {
        n as f64 / total as f64 * 100.0
    } else {
        0.0
    }
}

/// Inclusive last LBA of a half-open `[start, start+count)` range, for display.
/// Guards `count == 0` so the printed span matches the trailing sector count
/// instead of underflowing / overshooting by one.
fn inclusive_last_lba(start_lba: u32, count: u32) -> u32 {
    start_lba.saturating_add(count.saturating_sub(1))
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
    println!("  File paths follow the scheme: mkv://./Movie.mkv, m2ts://./Movie.m2ts");
    println!();
    println!("Examples:");
    println!("  freemkv disc:// mkv://Movie.mkv                     Rip disc to MKV");
    println!("  freemkv disc:// m2ts://Movie.m2ts                   Rip disc to m2ts");
    println!("  freemkv disc:///dev/sg4 mkv://Movie.mkv             Rip specific drive");
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
    println!("  freemkv disc:// iso://Disc.iso --multipass        Re-run to patch bad sectors");
    println!("  freemkv iso://Disc.iso mkv://Movie.mkv             ISO to MKV");
    println!("  freemkv m2ts://Movie.m2ts mkv://Movie.mkv          Remux m2ts to MKV");
    println!("  freemkv disc:// network://192.0.2.10:9000           Stream to network");
    println!("  freemkv network://0.0.0.0:9000 mkv://Movie.mkv    Receive from network");
    println!("  freemkv disc:// stdio://                           Pipe to stdout");
    println!("  freemkv disc:// null://                            Benchmark read speed");
    println!("  freemkv info disc://                               Show disc info");
    println!();
    println!("Flags:");
    println!("  -t, --title N       Select title (1-based, repeatable). Default: all.");
    println!("  -k, --keydb PATH    KEYDB.cfg path");
    println!("      --log-level N   1=warnings 2=info 3=debug 4=trace (default 1).");
    println!("                      Use 3 for bug reports. Logs go to STDERR so");
    println!("                      stdout stays pipe-clean.");
    println!("      --log-file PATH Also write logs to PATH (for bug reports).");
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

#[cfg(test)]
mod tests {
    use super::{collect_urls, inclusive_last_lba, pct_of};

    #[test]
    fn pct_of_guards_zero_total() {
        // The verify NaN% bug: a zero-sector title divided by total_sectors==0.
        assert_eq!(pct_of(0, 0), 0.0);
        assert_eq!(pct_of(5, 0), 0.0);
        // Normal cases still compute.
        assert_eq!(pct_of(50, 100), 50.0);
        assert_eq!(pct_of(1, 4), 25.0);
        assert!(pct_of(0, 0).is_finite(), "must not be NaN");
    }

    #[test]
    fn inclusive_last_lba_matches_count() {
        // Single bad sector at LBA 100 → "100-100", not the contradictory
        // "100-101" the half-open `start + count` produced.
        assert_eq!(inclusive_last_lba(100, 1), 100);
        assert_eq!(inclusive_last_lba(100, 5), 104);
        // count==0 must not underflow.
        assert_eq!(inclusive_last_lba(100, 0), 100);
        assert_eq!(inclusive_last_lba(0, 0), 0);
        // A range that reaches the top of the address space must not overflow
        // the `start + (count-1)` add (saturating_add caps at u32::MAX).
        assert_eq!(inclusive_last_lba(u32::MAX, 1), u32::MAX);
        assert_eq!(inclusive_last_lba(u32::MAX - 1, 5), u32::MAX);
        assert_eq!(inclusive_last_lba(u32::MAX, 1000), u32::MAX);
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn plain_two_urls() {
        assert_eq!(
            collect_urls(&v(&["disc://", "mkv://out.mkv"])),
            v(&["disc://", "mkv://out.mkv"])
        );
    }

    #[test]
    fn value_flag_takes_non_url_value() {
        // -t 1 consumes "1"; the two URLs remain positional.
        assert_eq!(
            collect_urls(&v(&["disc://", "mkv://out.mkv", "-t", "1"])),
            v(&["disc://", "mkv://out.mkv"])
        );
        // -k with a real path value.
        assert_eq!(
            collect_urls(&v(&["-k", "keydb.cfg", "disc://", "mkv://out.mkv"])),
            v(&["disc://", "mkv://out.mkv"])
        );
    }

    #[test]
    fn value_flag_does_not_swallow_positional_url() {
        // Regression: `-k` must not eat `disc://`, leaving a single URL that
        // silently routes to `info`. Both URLs must survive as positional.
        assert_eq!(
            collect_urls(&v(&["-k", "disc://", "mkv://out.mkv"])),
            v(&["disc://", "mkv://out.mkv"])
        );
        assert_eq!(
            collect_urls(&v(&["-t", "disc://", "mkv://out.mkv"])),
            v(&["disc://", "mkv://out.mkv"])
        );
    }

    #[test]
    fn boolean_flags_ignored() {
        assert_eq!(
            collect_urls(&v(&["--multipass", "disc://", "iso://d.iso", "--raw"])),
            v(&["disc://", "iso://d.iso"])
        );
    }
}
