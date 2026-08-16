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
/// The two logging flags, parsed out of the raw argv.
///
/// Split from `init_logging` because that function installs a PROCESS-GLOBAL
/// `tracing_subscriber` — it can only run once per test binary, so nothing ever
/// called it and every one of its parse decisions was unconstrained.
///
/// Returns `(level, file)`. An out-of-range or non-numeric `--log-level` is
/// reported and IGNORED rather than silently clamped, so a typo does not
/// quietly hand the user a different verbosity than they asked for. Strings are
/// not loaded yet at this point in startup, so these diagnostics are plain
/// English by necessity.
fn parse_logging_flags(args: &[String]) -> (Option<u8>, Option<String>) {
    let mut level_num: Option<u8> = None;
    let mut log_file: Option<String> = None;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            // Both arms below refuse a value that is itself a flag. Taking
            // `it.next()` unconditionally meant `--log-file --raw` set the
            // path to "--raw" AND consumed the flag, so the rip ran without
            // `--raw` and silently wrote a decrypted image. `is_flag_token`
            // lets a negative number through, so `--log-level -1` still
            // reaches the range check rather than being reported as missing.
            "--log-level" => match it.next_if(|s| !is_flag_token(s)) {
                Some(s) => match s.parse::<u8>() {
                    Ok(0) => eprintln!("--log-level: value 0 is out of range (1–4), ignored"),
                    Ok(n) => level_num = Some(n.clamp(1, 4)),
                    Err(_) => {
                        eprintln!("--log-level: expected a number 1–4, got '{s}', ignored")
                    }
                },
                None => {
                    eprintln!("--log-level: requires a value (1=warn, 2=info, 3=debug, 4=trace)")
                }
            },
            "--log-file" => {
                if let Some(p) = it.next_if(|s| !is_flag_token(s)) {
                    log_file = Some(p.clone());
                }
            }
            _ => {}
        }
    }
    (level_num, log_file)
}

/// Split a `--log-file` value into the `(directory, filename)` pair the rolling
/// appender needs. A bare filename logs into the current directory; a path with
/// no filename component at all (`""`, `"/"`) is invalid and returns `None`, so
/// the caller reports it instead of writing somewhere unintended.
fn split_log_path(path: &str) -> Option<(std::path::PathBuf, std::ffi::OsString)> {
    let p = std::path::Path::new(path);
    let name = p.file_name()?.to_os_string();
    let dir = match p.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => d.to_path_buf(),
        None => std::path::PathBuf::from("."),
    };
    Some((dir, name))
}

fn init_logging(args: &[String]) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let (level_num, log_file) = parse_logging_flags(args);

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
    let file_appender = match split_log_path(&path) {
        Some((dir, name)) => tracing_appender::rolling::never(dir, name),
        None => {
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

/// Every word a dispatcher matches `args[1]` against. Note "gui" is NOT matched
/// in [`run`]: `app_entry::wants_gui` intercepts it before the CLI shell is
/// reached at all, so looking for it in `run`'s match arms finds nothing and
/// makes this list look stale when it is correct. Anything NOT
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
    let (args, language) = strip_language_flag(&args);
    if let Some(lang) = language {
        crate::strings::set_language(&lang);
    }
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

/// Pull `--language`/`--lang` and its value out of the argument list.
///
/// Returns the remaining arguments and the requested language code. The value
/// guard is the same one `collect_urls` applies, and for the same reason: a
/// value-flag must not swallow a following positional stream URL.
/// `freemkv --language disc:// mkv://out.mkv` would otherwise eat `disc://` as
/// the "language", leaving a single URL that silently degrades into an
/// info/usage no-op. A leading `-` means the value is missing too, not a
/// language code.
///
/// Extracted from `run()`'s inline loop, which no `cargo test` invocation ever
/// reached — this is exactly the "add a flag, forget the test" shape the
/// `collect_urls` comments warn about, applied to the one flag that never got
/// the same treatment.
fn strip_language_flag(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut filtered = Vec::new();
    let mut language = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--language" || args[i] == "--lang" {
            match args.get(i + 1) {
                Some(v) if !is_url(v) && !v.starts_with('-') => {
                    language = Some(v.clone());
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
    (filtered, language)
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
/// value-taking flags (`-t`, `--keydb`, …).
///
/// A value-flag normally consumes the following token as its value, but it must
/// NOT swallow a positional stream URL (`scheme://...`): `freemkv --keydb
/// disc:// mkv://out.mkv` would otherwise let `--keydb` eat `disc://`, leaving a
/// single URL that silently routes to `info` instead of ripping. So if a
/// value-flag is followed by a URL token, the URL is kept as positional and the
/// flag's value is treated as absent (crate::pipe::run then reports the missing
/// value).
///
/// Every flag that consumes the following token as its value. This is the ONE
/// source of truth for flag arity, shared by `collect_urls` (below) and asserted
/// against `parse_flags` by `pipe::tests::value_flag_set_matches_parser` — so
/// adding a value-flag to the parser without listing it here fails a test rather
/// than silently mis-parsing (the `-a`/`-s` bug). Boolean flags (`--raw`,
/// `--multipass`, `-q`) are deliberately absent — and so is `-k`: it is a
/// RETIRED flag (`--keydb` is the only spelling), and while it sat here the
/// table claimed an arity for a token the parser rejects, so `freemkv -k
/// keydb.cfg …` had its path quietly eaten before the rejection was reached.
pub(crate) const VALUE_FLAGS: &[&str] = &[
    "-t",
    "--title",
    "-a",
    "--audio",
    "-s",
    "--subtitles",
    "--keydb",
    "--key-url",
    "--key-auth",
    "--log-file",
    "--log-level",
];

/// Whether a token is another FLAG, and so can never be a flag's value.
///
/// The companion to the `scheme://` rule above, and the other half of the same
/// question: a value-flag that blindly takes the next token also takes the next
/// *flag*. `freemkv --keydb --raw disc:// mkv://out.mkv` set the keydb path to
/// "--raw" and dropped the `--raw`, and `freemkv info --keydb --full disc://`
/// dropped the `--full` — in both cases the user was answered with something
/// they did not ask for, silently.
///
/// ONE definition, used by both parsers (`pipe::parse_flags` and
/// `disc_info::parse_info_flags`), because two spellings of one rule is how
/// they came to disagree in the first place.
///
/// A lone `-` is not a flag (it is the conventional stdin/stdout name), and a
/// leading dash followed by a DIGIT is a negative number, so `--log-level -1`
/// still reaches the parser that reports it as out of range rather than being
/// re-reported as a missing value. Deliberately NOT applied to `--key-auth`:
/// a bearer token is opaque and may legitimately begin with `-`.
pub(crate) fn is_flag_token(s: &str) -> bool {
    let mut rest = s.strip_prefix('-').unwrap_or("").chars();
    match rest.next() {
        None => false,
        Some(c) => !c.is_ascii_digit(),
    }
}

/// Flags this CLI no longer accepts, but which DID take a value.
///
/// They are not flag arity in the `VALUE_FLAGS` sense — `parse_flags` consumes
/// nothing for them, it rejects them — but `collect_urls` still has to step
/// over the value, or the value counts as a third positional and the whole
/// invocation collapses into the bare usage hint. The user is then told
/// nothing about the flag that is actually gone. Stepping over it leaves
/// exactly two URLs, the rip route runs, and `parse_flags` delivers the
/// precise "unknown flag '-k' — try 'freemkv help'".
///
/// Kept honest by `pipe::tests::retired_value_flags_are_rejected_by_the_parser`:
/// every entry must be one the parser REJECTS, and must not also appear in
/// `VALUE_FLAGS`. `-k` used to sit in `VALUE_FLAGS` itself, which claimed an
/// arity for a token no parser arm names.
/// `-k` was dropped in rc.6 (`--keydb` is the only spelling); `--device`/`-d`
/// were dropped when the device moved into the source URL (`disc:///dev/sgN`).
pub(crate) const RETIRED_VALUE_FLAGS: &[&str] = &["-k", "--device", "-d"];

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
            // URL is a misplaced positional; reclassify it so `--keydb disc://
            // mkv://` still rips.
            if !consume_key_url && is_url(arg) {
                urls.push(arg.clone());
            }
            continue;
        }
        if arg.starts_with('-') {
            if VALUE_FLAGS.contains(&arg.as_str()) || RETIRED_VALUE_FLAGS.contains(&arg.as_str()) {
                skip_next = true;
                skip_is_key_url = arg == "--key-url";
            }
        } else {
            urls.push(arg.clone());
        }
    }
    urls
}

/// Format the per-stream summary lines for `info mkv://` / `info m2ts://`.
///
/// `v.label`, `a.label`, `a.language`, and `s.language` are disc-derived
/// strings (an MKV/m2ts track name or a raw MPLS/IFO language tag) and go
/// straight to the real terminal, so each is run through
/// `disc_info::sanitize` before printing — the same treatment `disc_info.rs`
/// and `pipe.rs` give the identical fields, so a crafted file can't inject
/// terminal escapes here. `a.codec` and `a.channels` are the library's own
/// enums, not disc bytes, so they are not sanitized.
fn stream_info_lines(streams: &[libfreemkv::Stream]) -> Vec<String> {
    let mut lines = Vec::with_capacity(streams.len());
    for s in streams {
        match s {
            libfreemkv::Stream::Video(v) => {
                let label = if v.label.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", crate::disc_info::sanitize(&v.label))
                };
                lines.push(format!("  {} {}{}", v.codec, v.resolution, label));
            }
            libfreemkv::Stream::Audio(a) => {
                let mut tags: Vec<String> = Vec::new();
                let purpose_key = match a.purpose {
                    libfreemkv::LabelPurpose::Commentary => Some("stream.purpose.commentary"),
                    libfreemkv::LabelPurpose::Descriptive => Some("stream.purpose.descriptive"),
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
                    tags.push(crate::disc_info::sanitize(&a.label));
                }
                let label = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", tags.join(", "))
                };
                lines.push(format!(
                    "  {} {} {}{}",
                    a.codec,
                    a.channels,
                    crate::disc_info::sanitize(&a.language),
                    label
                ));
            }
            libfreemkv::Stream::Subtitle(s) => {
                lines.push(format!(
                    "  {} {}",
                    s.codec,
                    crate::disc_info::sanitize(&s.language)
                ));
            }
        }
    }
    lines
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
        // `dir://` — an extracted disc FOLDER — enumerates exactly like an
        // image, because `libfreemkv::scan_dir` synthesizes a real UDF volume
        // over the folder and hands back the same `(Disc, SectorSource)` pair
        // `scan_iso` does. The mux path already routed `dir://` through that
        // machinery; `info` was the one place that never learned, so a folder
        // fell through to the catch-all and reported "cannot get info" while
        // ripping from the same folder worked.
        libfreemkv::StreamUrl::Dir { path } | libfreemkv::StreamUrl::Iso { path } => {
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
            // A folder needs scan_dir (which additionally decides the
            // encryption verdict from CONTENT rather than from whether an
            // AACS/ directory survived the copy); an image needs scan_iso.
            let scan = if matches!(parsed, libfreemkv::StreamUrl::Dir { .. }) {
                libfreemkv::scan_dir
            } else {
                libfreemkv::scan_iso
            };
            let (disc, _reader) = match scan(
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
                    for line in stream_info_lines(&meta.streams) {
                        println!("{line}");
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
    // The ONLY way to write into a non-empty `dir://` target, and the target's
    // own rejection tells the user to pass it — so it has to be listed here
    // too, not discoverable only from the error it clears.
    println!("{}", crate::strings::get("usage.flag.force"));
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
    /// A value-taking logging flag must not swallow the NEXT FLAG as its value.
    ///
    /// `--log-file` took `it.next()` unconditionally, so
    /// `freemkv --log-file --raw disc:// iso:///out/d.iso` set the log path to
    /// "--raw" and consumed the flag — the rip then ran WITHOUT `--raw` and
    /// silently wrote a decrypted image. `--log-level` has the same shape: it
    /// consumes the token, fails to parse it as a number, prints "ignored",
    /// and the flag is gone either way.
    ///
    /// A sibling fix guarded `--keydb` in `pipe::parse_flags` and its commit
    /// message claimed both parsers were covered under one rule. They were
    /// not: this file DEFINES `is_flag_token` and used it nowhere.
    #[test]
    fn a_logging_flag_does_not_swallow_the_following_flag() {
        for flag in ["--log-file", "--log-level"] {
            let args: Vec<String> = [flag, "--raw", "disc://", "iso:///out/d.iso"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            let (level, log_file) = super::parse_logging_flags(&args);
            assert!(
                log_file.as_deref() != Some("--raw"),
                "{flag} took the following FLAG as its value; the rip then runs \
                 without --raw and silently writes a decrypted image"
            );
            // And the flag must still be visible to the parser that wants it.
            assert!(
                args.iter().any(|a| a == "--raw"),
                "fixture invariant: --raw must still be in the argv"
            );
            let _ = level;
        }

        // A rejected value must stay AVAILABLE, not be eaten by the peek: the
        // sibling logging flag after it must still parse. `next_if` leaves the
        // token in place; a plain `next().filter(..)` would have consumed it.
        let args: Vec<String> = ["--log-file", "--log-level", "3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (level, log_file) = super::parse_logging_flags(&args);
        assert_eq!(log_file, None, "--log-file had no value to take");
        assert_eq!(
            level,
            Some(3),
            "--log-level was swallowed by the flag before it"
        );
    }

    use super::{SUBCOMMANDS, collect_urls, stream_info_lines, update_keys_dest};

    /// `info mkv://` / `info m2ts://` prints `v.label`, `a.label`,
    /// `a.language`, and `s.language` straight from the file's own track
    /// metadata. Each is disc/file-controlled (an MKV track name or a raw
    /// MPLS/IFO language tag), so a crafted file carrying a terminal escape
    /// sequence in any of those four fields must not have it survive to the
    /// terminal — mirroring `disc_info::sanitize_strips_terminal_escape_sequences`.
    #[test]
    fn stream_info_lines_strip_terminal_escapes_from_every_disc_controlled_field() {
        use libfreemkv::{
            AudioChannels, AudioStream, Codec, ColorSpace, FrameRate, HdrFormat, LabelPurpose,
            Resolution, SampleRate, SubtitleStream, VideoStream,
        };

        let hostile = "\x1b[2Jevil\x07";

        let video = libfreemkv::Stream::Video(VideoStream {
            pid: 0x1011,
            codec: Codec::Hevc,
            resolution: Resolution::Unknown,
            frame_rate: FrameRate::Unknown,
            hdr: HdrFormat::Sdr,
            color_space: ColorSpace::Bt709,
            display_aspect: None,
            secondary: false,
            label: hostile.to_string(),
            measured_cicp: None,
        });
        let audio = libfreemkv::Stream::Audio(AudioStream {
            pid: 0x1100,
            codec: Codec::TrueHd,
            channels: AudioChannels::Unknown,
            language: hostile.to_string(),
            sample_rate: SampleRate::Unknown,
            secondary: false,
            purpose: LabelPurpose::Normal,
            label: hostile.to_string(),
        });
        let subtitle = libfreemkv::Stream::Subtitle(SubtitleStream {
            pid: 0x1200,
            codec: Codec::Pgs,
            language: hostile.to_string(),
            forced: false,
            qualifier: libfreemkv::LabelQualifier::None,
            codec_data: None,
        });

        let lines = stream_info_lines(&[video, audio, subtitle]);
        let joined = lines.join("\n");
        assert!(
            !joined.contains('\x1b') && !joined.contains('\x07'),
            "control/escape chars must be stripped from every stream line, got {joined:?}"
        );
        // Sanity: the fix didn't just drop the field — the printable text
        // (still containing "evil") should survive, sanitized.
        assert!(
            joined.contains("evil"),
            "printable label text should survive sanitization, got {joined:?}"
        );
    }

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
        // --keydb with a real path value.
        assert_eq!(
            collect_urls(&v(&["--keydb", "keydb.cfg", "disc://", "mkv://out.mkv"])),
            v(&["disc://", "mkv://out.mkv"])
        );
    }

    #[test]
    fn a_retired_flags_value_is_stepped_over_so_the_rejection_is_reached() {
        // `-k` and `--device`/`-d` are gone, but they took a value, and the
        // value must not become a third positional: three URLs is the bare
        // usage hint, which says nothing about the flag that was removed. Two
        // URLs routes to the rip, where `parse_flags` names the retired flag.
        for retired in super::RETIRED_VALUE_FLAGS {
            assert_eq!(
                collect_urls(&v(&[retired, "value", "disc://", "mkv://out.mkv"])),
                v(&["disc://", "mkv://out.mkv"]),
                "`{retired}`'s value became a positional"
            );
            // …and a following stream URL is still NOT eaten (the same guard
            // the live value-flags get).
            assert_eq!(
                collect_urls(&v(&[retired, "disc://", "mkv://out.mkv"])),
                v(&["disc://", "mkv://out.mkv"]),
                "`{retired}` swallowed a positional URL"
            );
        }
    }

    #[test]
    fn value_flag_does_not_swallow_positional_url() {
        // Regression: `--keydb` must not eat `disc://`, leaving a single URL
        // that silently routes to `info`. Both URLs must survive as positional.
        assert_eq!(
            collect_urls(&v(&["--keydb", "disc://", "mkv://out.mkv"])),
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

/// The argv decisions `run()` and `init_logging()` make before anything else
/// happens — each of them previously unreachable from `cargo test`, either
/// because it was inline in a 400-line dispatcher or because installing a
/// process-global subscriber can only be done once per binary.
#[cfg(test)]
mod arg_tests {
    use super::{parse_logging_flags, split_log_path, strip_language_flag, wants_help};

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// The common path: no logging flags means NO subscriber is installed and
    /// the terminal stays clean. If either half of that reads as "requested",
    /// every plain `freemkv` run starts writing ./log.txt.
    #[test]
    fn no_logging_flags_requests_nothing() {
        assert_eq!(parse_logging_flags(&v([].as_slice())), (None, None));
        assert_eq!(
            parse_logging_flags(&v(&["freemkv", "iso://a.iso", "mkv://b.mkv", "-t", "2"])),
            (None, None)
        );
    }

    /// `--log-level` maps 1..4 and is CLAMPED, not wrapped.
    #[test]
    fn the_log_level_values_map_and_clamp() {
        for n in 1..=4u8 {
            assert_eq!(
                parse_logging_flags(&v(&["--log-level", &n.to_string()])).0,
                Some(n)
            );
        }
        // Above the range clamps to trace rather than being dropped.
        assert_eq!(parse_logging_flags(&v(&["--log-level", "9"])).0, Some(4));
        assert_eq!(parse_logging_flags(&v(&["--log-level", "255"])).0, Some(4));
    }

    /// Bad input is reported and IGNORED — never silently clamped up to 1, and
    /// never treated as "a log was requested". `0` is out of range and a typo
    /// must not quietly give the user a different verbosity than they asked
    /// for.
    #[test]
    fn a_bad_log_level_is_ignored_rather_than_guessed_at() {
        assert_eq!(parse_logging_flags(&v(&["--log-level", "0"])).0, None);
        assert_eq!(parse_logging_flags(&v(&["--log-level", "xyz"])).0, None);
        assert_eq!(parse_logging_flags(&v(&["--log-level", "-1"])).0, None);
        // Last token, no value: no panic, nothing requested.
        assert_eq!(parse_logging_flags(&v(&["--log-level"])), (None, None));
    }

    /// `--log-file` takes the following token, in either flag order, and does
    /// not need `--log-level` to be present.
    #[test]
    fn the_log_file_flag_takes_the_next_token_in_either_order() {
        assert_eq!(
            parse_logging_flags(&v(&["--log-file", "/tmp/x.log"])),
            (None, Some("/tmp/x.log".to_string()))
        );
        assert_eq!(
            parse_logging_flags(&v(&["--log-file", "a.log", "--log-level", "2"])),
            (Some(2), Some("a.log".to_string()))
        );
        assert_eq!(
            parse_logging_flags(&v(&["--log-level", "2", "--log-file", "a.log"])),
            (Some(2), Some("a.log".to_string()))
        );
        // Trailing `--log-file` with no value: nothing requested, no panic.
        assert_eq!(parse_logging_flags(&v(&["--log-file"])), (None, None));
    }

    /// A bare filename logs into the current directory; a directory-qualified
    /// one logs there. A value with no filename component is invalid and the
    /// caller must report it rather than write somewhere unintended.
    #[test]
    fn the_log_path_splits_into_a_directory_and_a_name() {
        let (dir, name) = split_log_path("log.txt").expect("a bare name is valid");
        assert_eq!(dir, std::path::Path::new("."));
        assert_eq!(name, "log.txt");

        let (dir, name) = split_log_path("/var/log/freemkv.log").expect("a full path is valid");
        assert_eq!(dir, std::path::Path::new("/var/log"));
        assert_eq!(name, "freemkv.log");

        let (dir, name) = split_log_path("logs/a.log").expect("a relative dir is valid");
        assert_eq!(dir, std::path::Path::new("logs"));
        assert_eq!(name, "a.log");

        assert!(
            split_log_path("").is_none(),
            "an empty path has no filename"
        );
        assert!(split_log_path("/").is_none(), "a root path has no filename");
        assert!(split_log_path("..").is_none());
    }

    /// The value guard: `--language` must not swallow a following stream URL.
    /// Without it `freemkv --language disc:// mkv://out.mkv` eats `disc://` as
    /// the language and the rip degrades into a usage no-op with exit 0.
    #[test]
    fn the_language_flag_never_swallows_a_url_or_a_flag() {
        let (args, lang) = strip_language_flag(&v(&["freemkv", "--language", "de", "disc://"]));
        assert_eq!(lang.as_deref(), Some("de"));
        assert_eq!(args, v(&["freemkv", "disc://"]));

        // The short alias behaves identically.
        let (args, lang) = strip_language_flag(&v(&["freemkv", "--lang", "de", "disc://"]));
        assert_eq!(lang.as_deref(), Some("de"));
        assert_eq!(args, v(&["freemkv", "disc://"]));

        // A URL is not a language code — keep it positional.
        let (args, lang) =
            strip_language_flag(&v(&["freemkv", "--language", "disc://", "mkv://x.mkv"]));
        assert_eq!(lang, None);
        assert_eq!(args, v(&["freemkv", "disc://", "mkv://x.mkv"]));

        // Nor is a flag.
        let (args, lang) = strip_language_flag(&v(&["freemkv", "--language", "--verbose"]));
        assert_eq!(lang, None);
        assert_eq!(args, v(&["freemkv", "--verbose"]));

        // Last token: no value, no panic.
        let (args, lang) = strip_language_flag(&v(&["freemkv", "--language"]));
        assert_eq!(lang, None);
        assert_eq!(args, v(&["freemkv"]));

        // Absent entirely: the argument list is untouched.
        let original = v(&["freemkv", "iso://a.iso", "mkv://b.mkv"]);
        let (args, lang) = strip_language_flag(&original);
        assert_eq!(lang, None);
        assert_eq!(args, original);
    }

    /// `freemkv <cmd> --help` routes to the per-command help before the
    /// command's own parser runs. Forced `true`, `freemkv info disc://` prints
    /// help instead of scanning the disc.
    #[test]
    fn help_is_requested_only_by_the_help_flags() {
        assert!(!wants_help(&v([].as_slice())));
        assert!(!wants_help(&v(&["disc://"])));
        assert!(!wants_help(&v(&["--helpful", "-help", "h"])));
        assert!(wants_help(&v(&["--help"])));
        assert!(wants_help(&v(&["-h"])));
        assert!(wants_help(&v(&["disc://", "-h"])));
        assert!(wants_help(&v(&["--share", "--help", "-m"])));
    }
}
