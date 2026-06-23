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

/// Default diagnostic log path when `--log-level` is given without an explicit
/// `--log-file`. Written in the working directory, matching the fatal-error
/// hint ("re-run with --log-level 3 (writes ./log.txt)").
const DEFAULT_LOG_FILE: &str = "log.txt";

/// Initialise tracing.
///
/// Two-channel design: the **terminal** (Channel 1) is always clean — curated
/// progress, status, and the final result block only. **Zero `tracing`
/// DEBUG/TRACE (or any tracing level) ever reaches the terminal.** Tracing is a
/// diagnostic stream that only exists when the user explicitly asks for it, and
/// it goes to a **file** (Channel 2), never stdout/stderr.
///
/// A file log is written only when one of these is set:
///   * `--log-level N` — N maps 1→warn, 2→info, 3→debug, 4→trace for the
///     `freemkv` / `libfreemkv` targets (everything else stays at error).
///   * `--log-file PATH` — write to PATH (default level 3/debug if `--log-level`
///     is absent, so a lone `--log-file` still captures useful detail).
///   * `RUST_LOG` — power-user override of the filter; still file-only.
///
/// With none of these set, no subscriber is installed at all: the library's
/// `tracing` events are dropped and the terminal stays pristine. The file
/// destination defaults to `./log.txt`; ANSI is off and timestamps are on so
/// the log is clean and copy-pasteable for a bug report.
fn init_logging(args: &[String]) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    // Parse the two logging flags. `--log-level N` (1=warn..4=trace); the
    // per-subcommand parsers read the same flag to widen stdout detail at >=2.
    let mut level_num: Option<u8> = None;
    let mut log_file: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--log-level" => {
                if let Some(n) = it.next().and_then(|s| s.parse::<u8>().ok()) {
                    level_num = Some(n.clamp(1, 4));
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

    let rust_log = std::env::var("RUST_LOG").is_ok();

    // No `--log-level`, no `--log-file`, no `RUST_LOG`: the user didn't ask for
    // a diagnostic log. Install NOTHING — the terminal stays clean and the
    // library's tracing events are silently dropped. This is the common path.
    if level_num.is_none() && log_file.is_none() && !rust_log {
        return;
    }

    // A diagnostic log was requested. Build the filter: RUST_LOG wins; else map
    // the numeric level (defaulting to debug when only `--log-file` was given,
    // since the user clearly wants detail).
    let env_filter = if rust_log {
        EnvFilter::from_default_env()
    } else {
        let level = match level_num.unwrap_or(3) {
            1 => "warn",
            2 => "info",
            3 => "debug",
            _ => "trace",
        };
        EnvFilter::new(format!("error,freemkv={level},libfreemkv={level}"))
    };

    // File-only sink. NEVER stdout/stderr — the terminal is Channel 1 and must
    // stay free of tracing. Default to ./log.txt; ANSI off, timestamps on.
    let path = log_file.unwrap_or_else(|| DEFAULT_LOG_FILE.to_string());
    let p = std::path::Path::new(&path);
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
    let file_appender = match (dir, p.file_name()) {
        (Some(dir), Some(name)) => tracing_appender::rolling::never(dir, name),
        (None, Some(name)) => tracing_appender::rolling::never(".", name),
        _ => {
            // An invalid `--log-file` path is a fatal misconfiguration of the
            // diagnostic channel — report it cleanly on the terminal (this is a
            // CLI diagnostic, not a tracing event) and continue without a file.
            eprintln!("--log-file: invalid path '{path}' — no diagnostic log written");
            return;
        }
    };
    let (nb, guard) = tracing_appender::non_blocking(file_appender);
    let _ = LOG_GUARD.set(guard);
    let file_layer = fmt::layer().with_ansi(false).with_writer(nb);
    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .init();
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
                    // `pipe::run` has already printed the curated cause/result
                    // on the terminal; exit non-zero so a scripted `$?` sees the
                    // failure. (The pretty fatal block for cause-bearing errors
                    // is emitted inside the rip path where the cause is known.)
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

/// Print the curated fatal-error block and exit non-zero.
///
/// This is the single terminal-facing error path (Channel 1). It prints a
/// clean, localized block — never a raw error code, never a tracing event:
/// ```text
/// ✗ <operation> failed: <clean cause>.
///   For a diagnostic log, re-run with --log-level 3 (writes ./log.txt).
/// ```
/// `op_key` is a locale key for the operation name (`error.op_rip`, etc.);
/// `cause` is the already-localized, human-readable cause (typically from
/// [`pipe::fmt_err`], which renders `E<code>` → a plain-English message with
/// its own remediation). The diagnostic-log hint tells the user how to capture
/// a file log for a bug report — without ever spilling tracing onto the
/// terminal by default.
///
/// The block goes to STDERR so stdout stays pipe-clean for `mkv://`/`m2ts://`
/// streaming; the leading mark is ANSI-free when stderr is redirected.
fn fatal(op_key: &str, cause: &str) -> ! {
    let op = strings::get(op_key);
    eprintln!();
    eprintln!(
        "{} {}.",
        fail_mark(),
        strings::fmt("error.fatal_header", &[("op", &op), ("cause", cause)])
    );
    eprintln!("  {}", strings::get("error.fatal_diagnostic_hint"));
    std::process::exit(1);
}

/// The leading mark for the fatal-error block: a red `✗` on a real terminal, a
/// plain `x` when stderr is redirected to a file/pipe (so a pasted bug-report
/// log has no stray ANSI/Unicode noise).
fn fail_mark() -> &'static str {
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        "\x1b[31m✗\x1b[0m"
    } else {
        "x"
    }
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
        "--key-url",
        "--key-auth",
        "--log-file",
        "--log-level",
    ];

    // `--key-url`'s value is itself an `http(s)://` URL — so unlike `-t`/`-k`
    // (whose value is never a stream URL), its value MUST be consumed even though
    // it matches `is_url`. Otherwise the reclassify-as-positional fallback below
    // would treat the key-service URL as a third stream URL and break the
    // 2-URL rip dispatch. Track whether the flag we're skipping for is key-url.
    let mut urls = Vec::new();
    let mut skip_next = false;
    let mut skip_is_key_url = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            let consume_key_url = skip_is_key_url;
            skip_is_key_url = false;
            // `--key-url`'s value is always consumed (it's the key-service URL,
            // not a positional stream URL). For the other value-flags, a value
            // that looks like a stream URL is actually a misplaced positional —
            // reclassify it so `-k disc:// mkv://out` still rips.
            if !consume_key_url && is_url(arg) {
                urls.push(arg.clone());
            }
            continue;
        }
        if arg.starts_with('-') {
            if VALUE_FLAGS.contains(&arg.as_str()) {
                skip_next = true;
                skip_is_key_url = arg == "--key-url";
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
                Err(e) => fatal("error.op_info", &pipe::fmt_err(&e)),
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
                Err(e) => fatal("error.op_info", &pipe::fmt_err(&e)),
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
                Err(e) => fatal("error.op_info", &pipe::fmt_err(&e)),
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
                eprintln!("{}", strings::get("error.no_drive"));
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("{}", strings::get("verify.only_disc"));
            std::process::exit(1);
        }
    };

    println!("freemkv {}\n", env!("CARGO_PKG_VERSION"));

    // Open and scan
    eprint!("{}", strings::get("verify.opening"));
    let mut drive = match libfreemkv::Drive::open(&device) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", strings::get("verify.failed"));
            fatal("error.op_verify", &pipe::fmt_err(&e));
        }
    };
    // init()/probe_disc() are best-effort: many drives lack the firmware
    // support they probe for and `Disc::scan` below is the authoritative gate.
    // But don't print an unconditional "OK" that masks a genuine failure —
    // report WARN and the error so a later scan failure isn't mysterious.
    let _ = drive.wait_ready();
    match drive.init() {
        Ok(_) => eprintln!("{}", strings::get("verify.ok")),
        Err(e) => eprintln!(
            "{}",
            strings::fmt("verify.warn", &[("error", &pipe::fmt_err(&e))])
        ),
    }

    eprint!("{}", strings::get("verify.scanning"));
    let scan_opts = libfreemkv::ScanOptions::default();
    let disc = match libfreemkv::Disc::scan(&mut drive, &scan_opts) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}", strings::get("verify.failed"));
            fatal("error.op_verify", &pipe::fmt_err(&e));
        }
    };
    if disc.titles.is_empty() {
        fatal("error.op_verify", &strings::get("error.no_titles"));
    }
    let title = &disc.titles[0];
    let disc_name = disc.meta_title.as_deref().unwrap_or(&disc.volume_id);
    let total_sectors: u64 = title.extents.iter().map(|e| e.sector_count as u64).sum();
    let total_gb = total_sectors as f64 * 2048.0 / 1_073_741_824.0;
    eprintln!(
        "{}",
        strings::fmt(
            "verify.disc_summary",
            &[
                ("name", disc_name),
                ("size", &format!("{total_gb:.1}")),
                ("sectors", &total_sectors.to_string()),
            ]
        )
    );

    let batch = libfreemkv::disc::detect_max_batch_sectors(drive.device_path());
    let _ = drive.probe_disc();

    eprintln!("\n{}", strings::get("verify.verifying"));
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
                    "\r  {}",
                    strings::fmt(
                        "verify.progress",
                        &[
                            ("pct", &pct.to_string()),
                            ("speed", &format!("{speed:.1}")),
                            ("done", &p.work_done.to_string()),
                            ("total", &p.work_total.to_string()),
                        ]
                    )
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
    // One results row: a localized label, right-aligned count, and percentage.
    // The label word varies in width across languages; the count keeps its own
    // right-aligned column so the numbers still line up within a run.
    let row = |label_key: &str, n: u64| {
        let label = strings::get(label_key);
        println!("  {:<11} {:>12}  ({:.4}%)", format!("{label}:"), n, pct(n));
    };
    println!();
    println!("{}", strings::get("verify.results"));
    row("verify.good", result.good);
    if result.slow > 0 {
        row("verify.slow", result.slow);
    }
    if result.recovered > 0 {
        row("verify.recovered", result.recovered);
    }
    if result.bad > 0 {
        row("verify.bad", result.bad);
    }

    if !result.ranges.is_empty() {
        println!();
        for range in &result.ranges {
            let status_str = match range.status {
                libfreemkv::verify::SectorStatus::Slow => strings::get("verify.status_slow"),
                libfreemkv::verify::SectorStatus::Recovered => {
                    strings::get("verify.status_recovered")
                }
                libfreemkv::verify::SectorStatus::Bad => strings::get("verify.status_bad"),
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
                    format!(
                        " — {}",
                        strings::fmt(
                            "verify.chapter",
                            &[
                                ("num", &ch.to_string()),
                                ("min", &m.to_string()),
                                ("sec", &format!("{s:02}")),
                            ]
                        )
                    )
                }
                None => String::new(),
            };
            // `count` is a half-open span; the displayed range is inclusive, so
            // the last LBA is start + count - 1. Guard count==0 (would underflow
            // and contradict the trailing "0 sectors").
            let last_lba = inclusive_last_lba(range.start_lba, range.count);
            println!(
                "  {}",
                strings::fmt(
                    "verify.range",
                    &[
                        ("status", &status_str),
                        ("start", &range.start_lba.to_string()),
                        ("end", &last_lba.to_string()),
                        ("size", &format!("{gb:.1}")),
                        ("chapter", &ch_str),
                        ("count", &range.count.to_string()),
                    ]
                )
            );
        }
    }

    let elapsed = result.elapsed_secs;
    let m = elapsed as u32 / 60;
    let s = elapsed as u32 % 60;
    println!();
    println!(
        "{}",
        strings::fmt(
            "verify.verdict",
            &[
                ("pct", &format!("{:.4}", result.readable_pct())),
                ("min", &m.to_string()),
                ("sec", &format!("{s:02}")),
            ]
        )
    );

    if result.is_perfect() {
        println!("         {}", strings::get("verify.perfect"));
    } else if result.bad > 0 {
        let clusters = result
            .ranges
            .iter()
            .filter(|r| r.status == libfreemkv::verify::SectorStatus::Bad)
            .count();
        println!(
            "         {}",
            strings::fmt(
                "verify.unrecoverable",
                &[
                    ("count", &result.bad.to_string()),
                    ("clusters", &clusters.to_string()),
                ]
            )
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
    println!("  -k, --keydb PATH    KEYDB.cfg path (local key source)");
    println!("      --key-url URL   Online key-service base URL (http/https).");
    println!("                      With --keydb the keydb is tried first (local-");
    println!("                      first); alone it is the only key source.");
    println!("      --key-auth TOKEN Bearer token sent to the key service (optional).");
    println!("      --log-level N   1=warn 2=info 3=debug 4=trace. Off by default —");
    println!("                      the terminal stays clean. Set it to write a");
    println!("                      diagnostic log to ./log.txt (use 3 for bug reports).");
    println!("      --log-file PATH Write the diagnostic log to PATH instead of ./log.txt.");
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
        Err(e) => fatal("error.op_update_keys", &pipe::fmt_err(&e)),
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

    #[test]
    fn key_url_value_is_not_a_positional() {
        // `--key-url`'s value is an https:// URL — it must be consumed as the
        // flag value, NOT reclassified as a third positional stream URL (which
        // would break the 2-URL rip dispatch). Only the two stream URLs remain.
        assert_eq!(
            collect_urls(&v(&[
                "disc://",
                "mkv://out.mkv",
                "--key-url",
                "https://keys.example/keys",
            ])),
            v(&["disc://", "mkv://out.mkv"])
        );
        // With a bearer token too.
        assert_eq!(
            collect_urls(&v(&[
                "--key-url",
                "https://keys.example/keys",
                "--key-auth",
                "tok",
                "disc://",
                "mkv://out.mkv",
            ])),
            v(&["disc://", "mkv://out.mkv"])
        );
    }

    #[test]
    fn key_auth_token_value_consumed() {
        // `--key-auth`'s opaque token must be consumed, not kept as a positional.
        assert_eq!(
            collect_urls(&v(&["--key-auth", "tok", "disc://", "mkv://out.mkv"])),
            v(&["disc://", "mkv://out.mkv"])
        );
    }
}
