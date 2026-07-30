// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool
// MIT — freemkv project
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
// (module declarations + the global allocator live in main.rs — this file is
// the CLI shell entry point, invoked by the dispatcher for CLI-style args.)

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
                // VAL-1 / VAL-4: validate the value rather than silently
                // swallowing bad input or silently clamping 0 → 1.
                // Strings aren't loaded yet here, so plain English is fine.
                match it.next() {
                    Some(s) => match s.parse::<u8>() {
                        Ok(0) => eprintln!("--log-level: value 0 is out of range (1–4), ignored"),
                        Ok(n) => level_num = Some(n.clamp(1, 4)),
                        Err(_) => {
                            eprintln!("--log-level: expected a number 1–4, got '{s}', ignored")
                        }
                    },
                    None => eprintln!(
                        "--log-level: requires a value (1=warn, 2=info, 3=debug, 4=trace)"
                    ),
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

/// Every word the dispatcher in [`run`] matches `args[1]` against. Anything NOT
/// here falls through to the source→destination URL grammar, so a string that
/// tells the user to run `freemkv <word>` for some other `<word>` is telling
/// them to run a command that does not exist — which is exactly how
/// `drive-info`, `disc-info`, `remux` and `verify` shipped in the catalogues.
/// Kept in step with the match arms by `every_command_named_in_a_locale_exists`.
#[cfg(test)]
pub(crate) const SUBCOMMANDS: &[&str] = &["info", "update-keys", "version", "help", "gui"];

/// CLI shell entry point — the gold-standard `freemkv` CLI, replicated 1:1.
///
/// Invoked by `main.rs`'s dispatcher for CLI-style invocations. `args` is the
/// full `std::env::args()` vector (arg 0 = program name), matching what the
/// standalone CLI's `main` received, so every downstream parser is unchanged.
pub fn run(args: Vec<String>) {
    init_logging(&args);

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
                    crate::strings::set_language(v);
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
    crate::strings::init();

    if args.len() < 2 {
        // Bare invocation with no subcommand/URL: print usage but exit non-zero
        // so a scripted `freemkv; echo $?` (e.g. a misconfigured wrapper) sees a
        // failure rather than a false success. Explicit `help`/`--help`/`-h`
        // still exits 0 (handled below).
        usage();
        std::process::exit(2);
    }

    match args[1].as_str() {
        // `freemkv <cmd> --help` / `freemkv <cmd> -h` print command-specific help.
        // Handled before the per-command dispatch so the flag never reaches the
        // command's own argument parser.
        "info" if wants_help(&args[2..]) => help_info(),
        "update-keys" if wants_help(&args[2..]) => help_update_keys(),

        "info" => info_cmd(&args[2..]),
        "update-keys" => update_keys(&args[2..]),
        // NOTE: there is deliberately NO `remux` (or any conversion) verb. The
        // operation IS the URL pair: `freemkv <source-url> <dest-url> [opts]`.
        // e.g. `freemkv iso://Disc.iso -t 1 mkv://Movie.mkv`. Source→dest is the
        // whole grammar; a conversion "command" would be redundant.
        "version" | "--version" | "-V" => println!("{}", libfreemkv::VERSION_LABEL),
        // `freemkv help`, `freemkv --help`, `freemkv -h`: top-level usage.
        // `freemkv help <command>`: command-specific help.
        "help" | "--help" | "-h" => match args.get(2).map(|s| s.as_str()) {
            Some("info") => help_info(),
            Some("update-keys") => help_update_keys(),
            Some("version") | Some("help") | None => usage(),
            Some(other) => {
                eprintln!(
                    "{}",
                    crate::strings::fmt("help.unknown_command", &[("cmd", other)])
                );
                usage();
                std::process::exit(2);
            }
        },

        // Everything else: freemkv <source> <dest>
        _ => {
            let urls = collect_urls(&args[1..]);

            if urls.len() == 2 {
                if !crate::pipe::run(&urls[0], &urls[1], &args[1..]) {
                    // `crate::pipe::run` has already printed the curated cause/result
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
                eprintln!("{}", crate::strings::get("error.usage_hint"));
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
/// [`crate::pipe::fmt_err`], which renders `E<code>` → a plain-English message with
/// its own remediation). The diagnostic-log hint tells the user how to capture
/// a file log for a bug report — without ever spilling tracing onto the
/// terminal by default.
///
/// The block goes to STDERR so stdout stays pipe-clean for `mkv://`/`m2ts://`
/// streaming; the leading mark is ANSI-free when stderr is redirected.
fn fatal(op_key: &str, cause: &str) -> ! {
    let op = crate::strings::get(op_key);
    // WS2: the `Error:` level word is rendered from `error.level_error` (a
    // translatable key, the one home for the three level words) so the fatal
    // block reads `✗ Error: <op> failed: <cause>.` with the code-forward cause
    // produced by `crate::pipe::fmt_err`.
    let level = crate::strings::get(crate::messaging::Level::Error.locale_key());
    eprintln!();
    eprintln!(
        "{} {}.",
        fail_mark(),
        crate::strings::fmt(
            "error.fatal_header",
            &[("level", &level), ("op", &op), ("cause", cause)]
        )
    );
    eprintln!("  {}", crate::strings::get("error.fatal_diagnostic_hint"));
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
/// is treated as absent (crate::pipe::run then reports the missing value).
/// Every flag that consumes the following token as its value. This is the ONE
/// source of truth for flag arity, shared by `collect_urls` (below) and asserted
/// against `parse_flags` by `value_flag_set_matches_parser` — so adding a
/// value-flag to the parser without listing it here fails a test rather than
/// silently mis-parsing (the `-a`/`-s` bug). Boolean flags (`--raw`,
/// `--multipass`, `-q`) are deliberately absent.
pub(crate) const VALUE_FLAGS: &[&str] = &[
    "-t",
    "--title",
    "-a",
    "--audio",
    "-s",
    "--subtitles",
    "-k",
    "--keydb",
    "--key-url",
    "--key-auth",
    "--log-file",
    "--log-level",
];

fn collect_urls(args: &[String]) -> Vec<String> {
    // A positional token (not a flag, not a flag's value) is a stream URL — even
    // a schemeless one, which we KEEP so `parse_url` can reject it with a clear
    // "needs a scheme" error rather than silently dropping it. To tell a flag's
    // value apart from a positional we must know flag arity: `VALUE_FLAGS`.
    let mut urls = Vec::new();
    let mut skip_next = false;
    let mut skip_is_key_url = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            let consume_key_url = skip_is_key_url;
            skip_is_key_url = false;
            // `--key-url`'s value is itself a URL (the key service) — always
            // consumed. For other value-flags, a value that looks like a stream
            // URL is a misplaced positional; reclassify it so `-k disc:// mkv://`
            // still rips.
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
        eprintln!("{}", crate::strings::get("error.info_usage"));
        std::process::exit(1);
    }

    let url = &args[0];
    let parsed = libfreemkv::parse_url(url);

    match &parsed {
        libfreemkv::StreamUrl::Disc { device } => {
            // The device comes from the source URL (`disc:///dev/sgN`), not a flag.
            let dev = device.as_ref().map(|d| d.to_string_lossy().to_string());
            let flags = &args[1..];
            // --share routes to drive-info module (capture + GitHub submit)
            if flags.iter().any(|a| a == "--share" || a == "-s") {
                crate::info::run(dev.as_deref(), flags);
            } else {
                crate::disc_info::run(dev.as_deref(), flags);
            }
        }
        libfreemkv::StreamUrl::Iso { path } => {
            // Listing titles needs NO AACS key — only clear UDF navigation.
            // Scan the ISO keylessly and reuse disc_info's full title list
            // (duration, size, clip count, video/audio/subtitle streams).
            // Going through the key-gated `input()` here would hit libfreemkv's
            // no-key gate and surface E7022 for an encrypted disc, and would
            // only ever open a single title. `--keydb` is accepted but the
            // listing never requires it. `--full` shows every title.
            //
            // Flags go through the SAME parser the `disc://` route uses, so an
            // unknown one exits 1 here too. This route used to scan the list for
            // `--full` and ignore every other token, so a typo'd flag produced
            // output that had quietly dropped what the user asked for.
            let flags = match crate::disc_info::parse_info_flags(&args[1..]) {
                crate::disc_info::InfoParse::Ok(f) => f,
                crate::disc_info::InfoParse::Help => {
                    println!("{}", crate::strings::get("disc.usage"));
                    return;
                }
                crate::disc_info::InfoParse::Unknown(opt) => {
                    crate::disc_info::reject_unknown_option(&opt)
                }
            };
            let (disc, _reader) = match libfreemkv::scan_iso(
                std::path::Path::new(path),
                libfreemkv::ScanOptions::default(),
            ) {
                Ok(pair) => pair,
                Err(e) => fatal("error.op_info", &crate::pipe::fmt_err(&e)),
            };
            if !flags.quiet {
                println!("freemkv {}", libfreemkv::VERSION_LABEL);
                println!();
            }
            crate::disc_info::print_disc_titles(&disc, &flags);
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
                                    tags.push(crate::strings::get(k));
                                }
                                if a.secondary {
                                    tags.push(crate::strings::get("stream.secondary"));
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
                Err(e) => fatal("error.op_info", &crate::pipe::fmt_err(&e)),
            }
        }
        libfreemkv::StreamUrl::Unknown { .. } => {
            eprintln!(
                "{}",
                crate::strings::fmt("error.info_unknown_url", &[("url", url)])
            );
            std::process::exit(1);
        }
        _ => {
            eprintln!(
                "{}",
                crate::strings::fmt("error.info_unsupported_url", &[("url", url)])
            );
            std::process::exit(1);
        }
    }
}

fn usage() {
    println!("freemkv {}", libfreemkv::VERSION_LABEL);
    println!();
    println!("{}", crate::strings::get("usage.synopsis_1"));
    println!("{}", crate::strings::get("usage.synopsis_2"));
    println!("{}", crate::strings::get("usage.synopsis_4"));
    println!();
    println!("{}", crate::strings::get("usage.subcommands_header"));
    println!("{}", crate::strings::get("usage.subcmd.info"));
    println!("{}", crate::strings::get("usage.subcmd.update_keys"));
    println!("{}", crate::strings::get("usage.subcmd.version"));
    println!("{}", crate::strings::get("usage.subcmd.help"));
    println!();
    println!("{}", crate::strings::get("usage.subcommands_note"));
    println!();
    println!("{}", crate::strings::get("usage.urls_header"));
    println!("{}", crate::strings::get("usage.url.disc_auto"));
    println!("{}", crate::strings::get("usage.url.disc_linux"));
    println!("{}", crate::strings::get("usage.url.disc_windows"));
    println!("{}", crate::strings::get("usage.url.mkv"));
    println!("{}", crate::strings::get("usage.url.m2ts"));
    println!("{}", crate::strings::get("usage.url.network"));
    println!("{}", crate::strings::get("usage.url.stdio"));
    println!("{}", crate::strings::get("usage.url.iso"));
    println!("{}", crate::strings::get("usage.url.null"));
    println!();
    println!("{}", crate::strings::get("usage.url.scheme_note"));
    println!("{}", crate::strings::get("usage.url.path_note"));
    println!();
    println!("{}", crate::strings::get("usage.examples_header"));
    println!("{}", crate::strings::get("usage.ex.rip_mkv"));
    println!("{}", crate::strings::get("usage.ex.rip_m2ts"));
    println!("{}", crate::strings::get("usage.ex.rip_drive"));
    println!("{}", crate::strings::get("usage.ex.rip_title"));
    println!("{}", crate::strings::get("usage.ex.rip_titles"));
    println!("{}", crate::strings::get("usage.ex.rip_iso"));
    println!("{}", crate::strings::get("usage.ex.rip_iso_raw"));
    println!("{}", crate::strings::get("usage.ex.rip_iso_mp"));
    println!("{}", crate::strings::get("usage.ex.iso_to_mkv"));
    println!("{}", crate::strings::get("usage.ex.network"));
    println!("{}", crate::strings::get("usage.ex.network_recv"));
    println!("{}", crate::strings::get("usage.ex.stdio"));
    println!("{}", crate::strings::get("usage.ex.benchmark"));
    println!("{}", crate::strings::get("usage.ex.info"));
    println!();
    println!("{}", crate::strings::get("usage.flags_header"));
    println!("{}", crate::strings::get("usage.flag.title"));
    println!("{}", crate::strings::get("usage.flag.audio"));
    println!("{}", crate::strings::get("usage.flag.subtitles"));
    println!("{}", crate::strings::get("usage.flag.keydb"));
    println!("{}", crate::strings::get("usage.flag.key_url_1"));
    println!("{}", crate::strings::get("usage.flag.key_url_2"));
    println!("{}", crate::strings::get("usage.flag.key_url_3"));
    println!("{}", crate::strings::get("usage.flag.key_auth"));
    println!("{}", crate::strings::get("usage.flag.log_level_1"));
    println!("{}", crate::strings::get("usage.flag.log_level_2"));
    println!("{}", crate::strings::get("usage.flag.log_level_3"));
    println!("{}", crate::strings::get("usage.flag.log_file"));
    println!("{}", crate::strings::get("usage.flag.quiet"));
    println!("{}", crate::strings::get("usage.flag.raw"));
    println!("{}", crate::strings::get("usage.flag.multipass"));
    println!("{}", crate::strings::get("usage.flag.share"));
    println!("{}", crate::strings::get("usage.flag.mask"));
}

/// True if a command's argument list requests its help (`--help` / `-h`).
/// Used to route `freemkv <cmd> --help` to the per-command help text before the
/// command's own parser runs.
fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// `freemkv info --help` / `freemkv help info`.
fn help_info() {
    println!("freemkv {}", libfreemkv::VERSION_LABEL);
    println!();
    println!("{}", crate::strings::get("help.info.usage"));
    println!();
    println!("{}", crate::strings::get("help.info.desc"));
    println!();
    println!("{}", crate::strings::get("help.info.examples_header"));
    println!("{}", crate::strings::get("help.info.ex_disc"));
    println!("{}", crate::strings::get("help.info.ex_iso"));
    println!();
    println!("{}", crate::strings::get("help.info.flags_header"));
    println!("{}", crate::strings::get("help.info.flag_full"));
    println!("{}", crate::strings::get("help.info.flag_basic"));
    println!("{}", crate::strings::get("help.info.flag_verbose"));
    println!("{}", crate::strings::get("help.info.flag_share"));
}

/// `freemkv update-keys --help` / `freemkv help update-keys`.
fn help_update_keys() {
    println!("freemkv {}", libfreemkv::VERSION_LABEL);
    println!();
    println!("{}", crate::strings::get("help.update_keys.usage"));
    println!();
    println!("{}", crate::strings::get("help.update_keys.desc"));
    println!();
    println!(
        "{}",
        crate::strings::get("help.update_keys.examples_header")
    );
    println!("{}", crate::strings::get("help.update_keys.ex"));
    println!();
    println!("{}", crate::strings::get("help.update_keys.flags_header"));
    println!("{}", crate::strings::get("help.update_keys.flag_url"));
}

/// Resolve where `update-keys` saves the downloaded keydb: `--keydb <path>`
/// wins, else the standard location (first existing search path, else the
/// default). Factored out so the "`--keydb` is honored" behaviour is unit
/// testable without a network fetch — the prior bug was this flag being ignored
/// and the keydb always landing at the default location.
fn update_keys_dest(args: &[String]) -> std::path::PathBuf {
    let mut keydb: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--keydb" {
            i += 1;
            keydb = args.get(i).cloned();
        }
        i += 1;
    }
    crate::pipe::resolved_keydb_path(&keydb)
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
            eprintln!("{}", crate::strings::get("keys.usage"));
            std::process::exit(1);
        }
    };
    // The download lands at the `--keydb` path when given, else the standard
    // location.
    let dest = update_keys_dest(args);
    // Fetch the keydb bytes via ureq (HTTP **and** HTTPS) and hand them to the
    // keydb source to verify + atomically save to `dest`. The CLI supplies its
    // own SSRF-guarded `ureq` transport (`crate::keydb_fetch::fetch`); the keydb
    // source stays transport-agnostic on the update path.
    let result = freemkv_keysources::KeydbSource::new(dest).update(crate::keydb_fetch::fetch, url);
    match result {
        Ok(result) => {
            println!(
                "{}",
                crate::strings::fmt(
                    "keys.updated",
                    &[
                        ("entries", &result.entries.to_string()),
                        ("bytes", &result.bytes.to_string()),
                    ]
                )
            );
            println!(
                "{}",
                crate::strings::fmt(
                    "keys.saved",
                    &[("path", &result.path.display().to_string())]
                )
            );
        }
        Err(e) => fatal("error.op_update_keys", &crate::pipe::fmt_err(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::{SUBCOMMANDS, collect_urls, update_keys_dest};

    /// Walk every string value in a locale document.
    fn each_string(v: &serde_json::Value, f: &mut impl FnMut(&str)) {
        match v {
            serde_json::Value::Object(m) => m.values().for_each(|x| each_string(x, f)),
            serde_json::Value::Array(a) => a.iter().for_each(|x| each_string(x, f)),
            serde_json::Value::String(s) => f(s),
            _ => {}
        }
    }

    /// Subcommand names a string tells the user to TYPE, as opposed to the many
    /// places "freemkv" is just the product name in a sentence ("Quit freemkv",
    /// "Wordt van kracht nadat u freemkv opnieuw start"). A command claim is a
    /// `freemkv <word>` whose `<word>` is followed by something argument-shaped:
    /// a flag, a `<placeholder>`, an `[optional]`, a `scheme://` URL, the
    /// description column of a usage example, or a closing quote.
    fn commands_named_in(value: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut from = 0;
        while let Some(off) = value[from..].find("freemkv ") {
            let start = from + off + "freemkv ".len();
            from = start;
            let word: String = value[start..]
                .chars()
                .take_while(|c| (c.is_ascii_lowercase()) || c.is_ascii_digit() || *c == '-')
                .collect();
            if word.is_empty() {
                continue;
            }
            let rest = &value[start + word.len()..];
            // `freemkv disc://…` — the URL grammar, not a subcommand. An empty
            // remainder is the product name ending a sentence.
            if rest.starts_with("://") || rest.is_empty() {
                continue;
            }
            let spaces = rest.len() - rest.trim_start_matches(' ').len();
            let next = rest.trim_start_matches(' ');
            let is_command = match spaces {
                // `'freemkv info'` — a quoted command reference.
                0 => rest.starts_with(['\'', '»', '`', '"']),
                // `freemkv update-keys --url <u>` / `freemkv verify [disc://]`
                // / `freemkv verify disc:///dev/sg4`.
                1 => {
                    next.starts_with(['-', '[', '<'])
                        || next.split(' ').next().is_some_and(|t| t.contains("://"))
                }
                // The description column of a `usage.ex.*` line.
                _ => true,
            };
            if is_command {
                out.insert(word);
            }
        }
        out
    }

    #[test]
    fn every_command_named_in_a_locale_exists() {
        // `error.E2000`, `error.E7020`, `drive.share_hint`, `drive.share_usage`,
        // `disc.usage`, `remux.usage`, `help.remux.*` and `help.verify.*` all
        // instructed the user to run `freemkv drive-info` / `disc-info` /
        // `remux` / `verify`. None of those are dispatched: they fall through to
        // the URL grammar and fail. Checked across ALL bundled locales, since
        // the strings were wrong in all 29.
        let mut offenders: std::collections::BTreeSet<(String, String)> = Default::default();
        for code in freemkv_i18n::SHIPPED_CODES {
            let raw = freemkv_i18n::bundled_locale_json(code)
                .unwrap_or_else(|| panic!("{code} listed as shipped but not loadable"));
            let doc: serde_json::Value =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("{code}.json invalid: {e}"));
            each_string(&doc, &mut |s| {
                for cmd in commands_named_in(s) {
                    if !SUBCOMMANDS.contains(&cmd.as_str()) {
                        offenders.insert((code.to_string(), cmd));
                    }
                }
            });
        }
        assert!(
            offenders.is_empty(),
            "locale strings tell the user to run subcommands that do not exist \
             (the dispatcher accepts {SUBCOMMANDS:?}): {offenders:?}"
        );
    }

    /// Regression: `update-keys --keydb <path>` must save the download to that
    /// path. The flag used to be ignored (the keydb always went to the default
    /// location); `update_keys_dest` now honors it.
    #[test]
    fn update_keys_honors_keydb_flag() {
        let args: Vec<String> = [
            "--url",
            "http://x/k.zip",
            "--keydb",
            "/custom/path/keydb.cfg",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            update_keys_dest(&args),
            std::path::PathBuf::from("/custom/path/keydb.cfg"),
            "--keydb must be the download destination"
        );
    }

    /// Without `--keydb`, the destination resolves through the standard
    /// search/default policy — never the bogus override above.
    #[test]
    fn update_keys_without_keydb_flag_uses_standard_location() {
        let args: Vec<String> = ["--url", "http://x/k.zip"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_ne!(
            update_keys_dest(&args),
            std::path::PathBuf::from("/custom/path/keydb.cfg")
        );
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
    fn stream_selection_flag_values_are_not_read_as_urls() {
        // Regression: `-a`/`-s` (and any flag whose value isn't a URL) must not
        // let their value (`none`/`eng`/…) be collected as a third stream URL.
        // Before the scheme://-only rewrite these weren't in the value-flag list,
        // so `freemkv iso://x mkv://y -a none` collected `none` as a URL → 3 URLs
        // → the rip printed usage and silently did nothing. The whole class of
        // "add a flag, forget the list" bug is gone: values are never URLs.
        assert_eq!(
            collect_urls(&v(&[
                "iso://d.iso",
                "mkv://out.mkv",
                "-t",
                "1",
                "-a",
                "none",
                "-s",
                "eng",
            ])),
            v(&["iso://d.iso", "mkv://out.mkv"])
        );
        // Flags-first ordering, and comma lists, are equally safe.
        assert_eq!(
            collect_urls(&v(&[
                "-a",
                "eng,spa",
                "-s",
                "none",
                "disc://",
                "mkv://out.mkv"
            ])),
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
