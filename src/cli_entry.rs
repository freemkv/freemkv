// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool (MIT).
// Usage: freemkv <source> <dest> [flags] | freemkv info <url> [flags]
// (module decls + global allocator live in main.rs; this is the CLI shell entry point.)

/// Worker guard for the optional non-blocking file log layer. Held for the
/// life of the process so buffered records are flushed on exit; `None` when
/// `--log-file` isn't given.
static LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

/// Default diagnostic log path when `--log-level` is given without an explicit
/// `--log-file`. Written in the working directory, matching the fatal-error
/// hint ("re-run with --log-level 3 (writes ./log.txt)").
const DEFAULT_LOG_FILE: &str = "log.txt";

// Tracing has two channels (terminal + optional file log, see
// docs/cli-entry.md § "Tracing / logging channels"); PendingDiag holds a
// startup diagnostic that can't render yet — see § "PendingDiag" below.
struct PendingDiag {
    key: &'static str,
    // English fallback while the pinned freemkv-i18n tag doesn't ship `key`.
    // See crate::strings::get_or — a missing key renders as its own dotted
    // path, worse than this English on the terminal.
    english: &'static str,
    args: Vec<(&'static str, String)>,
}

impl PendingDiag {
    fn new(key: &'static str, english: &'static str) -> Self {
        PendingDiag {
            key,
            english,
            args: Vec::new(),
        }
    }

    fn with(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.args.push((name, value.into()));
        self
    }

    // The localized text, separate from emit() so a test can read it. See
    // docs/cli-entry.md § "PendingDiag" for why a typo'd key is worse than
    // the hard-coded English this replaced.
    fn render(&self) -> String {
        let args: Vec<(&str, &str)> = self.args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        crate::strings::fmt_or(self.key, self.english, &args)
    }

    // Print to stderr. Must not be called before `strings::init()`.
    fn emit(&self) {
        eprintln!("{}", self.render());
    }
}

// The two logging flags, parsed out of the raw argv. Split from
// init_logging so it's unit-testable (that fn installs a process-global
// subscriber). See docs/cli-entry.md § "parse_logging_flags".
fn parse_logging_flags(args: &[String]) -> (Option<u8>, Option<String>, Vec<PendingDiag>) {
    let mut level_num: Option<u8> = None;
    let mut log_file: Option<String> = None;
    let mut diags: Vec<PendingDiag> = Vec::new();
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            // Both arms refuse a value that's itself a flag or a `scheme://` URL
            // (mirrors pipe::parse_flags's guard) — else e.g. `--log-file --raw`
            // would eat `--raw` as the path and silently drop the flag.
            "--log-level" => {
                match it.next_if(|s| !is_flag_token(s) && !crate::pipe::is_url_token(s)) {
                    Some(s) => match s.parse::<u8>() {
                        Ok(0) => diags.push(PendingDiag::new(
                            "error.log_level_out_of_range",
                            "--log-level: value 0 is out of range (1–4), ignored",
                        )),
                        Ok(n) => level_num = Some(n.clamp(1, 4)),
                        Err(_) => diags.push(
                            PendingDiag::new(
                                "error.log_level_not_a_number",
                                "--log-level: expected a number 1–4, got '{value}', ignored",
                            )
                            .with("value", s),
                        ),
                    },
                    None => diags.push(PendingDiag::new(
                        "error.log_level_needs_value",
                        "--log-level: requires a value (1=warn, 2=info, 3=debug, 4=trace)",
                    )),
                }
            }
            "--log-file" => {
                match it.next_if(|s| !is_flag_token(s) && !crate::pipe::is_url_token(s)) {
                    Some(p) => log_file = Some(p.clone()),
                    // Symmetric with --log-level: a refused value must be reported, not
                    // silently dropped, so `run()` can emit it once locale is resolved.
                    None => diags.push(PendingDiag::new(
                        "error.log_file_needs_value",
                        "--log-file: requires a path (e.g. --log-file freemkv.log)",
                    )),
                }
            }
            _ => {}
        }
    }
    (level_num, log_file, diags)
}

// Split a --log-file value into (directory, filename). A bare filename
// logs into the current directory; a path with no filename component
// (`""`, `"/"`) is invalid and returns None, so the caller reports it.
fn split_log_path(path: &str) -> Option<(std::path::PathBuf, std::ffi::OsString)> {
    let p = std::path::Path::new(path);
    let name = p.file_name()?.to_os_string();
    let dir = match p.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => d.to_path_buf(),
        None => std::path::PathBuf::from("."),
    };
    Some((dir, name))
}

// Returns the diagnostics it could not render (see PendingDiag). The
// subscriber is installed HERE, first thing in run(), so no tracing event
// can be emitted before there's somewhere for it to go.
#[must_use = "these diagnostics are never shown unless run() emits them"]
fn init_logging(args: &[String]) -> Vec<PendingDiag> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let (level_num, log_file, mut diags) = parse_logging_flags(args);

    let rust_log = std::env::var("RUST_LOG").is_ok();

    // No `--log-level`, no `--log-file`, no `RUST_LOG`: the user didn't ask for
    // a diagnostic log. Install NOTHING — the terminal stays clean and the
    // library's tracing events are silently dropped. This is the common path.
    if level_num.is_none() && log_file.is_none() && !rust_log {
        return diags;
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
            diags.push(
                PendingDiag::new(
                    "error.log_file_invalid_path",
                    "--log-file: invalid path '{path}' — no diagnostic log written",
                )
                .with("path", path),
            );
            return diags;
        }
    };
    let (nb, guard) = tracing_appender::non_blocking(file_appender);
    let _ = LOG_GUARD.set(guard);
    let file_layer = fmt::layer().with_ansi(false).with_writer(nb);
    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .init();
    diags
}

// Every word the dispatcher matches args[1] against ("gui" is intercepted
// earlier by app_entry::wants_gui, so it's deliberately absent). See
// docs/cli-entry.md § "SUBCOMMANDS" for why this list matters.
#[cfg(test)]
pub(crate) const SUBCOMMANDS: &[&str] = &["info", "update-keys", "version", "help", "gui"];

/// CLI shell entry point — the gold-standard `freemkv` CLI, replicated 1:1.
///
/// Invoked by `main.rs`'s dispatcher for CLI-style invocations. `args` is the
/// full `std::env::args()` vector (arg 0 = program name), matching what the
/// standalone CLI's `main` received, so every downstream parser is unchanged.
pub fn run(args: Vec<String>) {
    let mut pending = init_logging(&args);

    // Parse --language before anything else, with the same is-URL guard `collect_urls`
    // uses: a value-flag must not swallow a following positional URL or flag token
    // (e.g. `--language disc://` or `--language --verbose`) as if it were a language code.
    let (args, language, lang_diags) = strip_language_flag(&args);
    pending.extend(lang_diags);
    if let Some(lang) = language
        && !lang.eq_ignore_ascii_case("auto")
    {
        // `auto` (the GUI's "Auto" option) means "follow the environment": install no
        // override, letting `strings::init()` resolve from LC_ALL/LANG. An unknown
        // code like `xx` still reaches `set_language`, giving a visible warning.
        crate::strings::set_language(&lang);
    }
    crate::strings::init();

    // FIRST point a message can be localized: the argv pre-pass's silent
    // `PendingDiag` complaints emit here, in order, in the resolved language.
    // (`set_language`/`init()` above may already print their own English warnings.)
    for d in &pending {
        d.emit();
    }

    if args.len() < 2 {
        // Bare invocation: print usage but exit non-zero so a scripted
        // `freemkv; echo $?` sees a failure. Explicit `help`/`--help`/`-h`
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
        // NOTE: deliberately no `remux`/conversion verb. The operation IS the
        // URL pair: `freemkv <source-url> <dest-url> [opts]` — source→dest is
        // the whole grammar, so a conversion "command" would be redundant.
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
                    // `pipe::run` already printed the curated cause/result (the pretty
                    // fatal block is emitted inside the rip path, where the cause is
                    // known); just exit non-zero so a scripted `$?` sees the failure.
                    std::process::exit(1);
                }
            } else if urls.len() == 1 {
                // Single URL, no dest — show info. `info_cmd` expects `args[0]` to be
                // the URL, but a preceding flag (e.g. `--verbose disc://`) would land
                // there instead; put the resolved URL first, then the remaining flags.
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

// Pull --language/--lang and its value out of the argument list, with the
// same URL-value guard as collect_urls. See docs/cli-entry.md §
// "strip_language_flag".
fn strip_language_flag(args: &[String]) -> (Vec<String>, Option<String>, Vec<PendingDiag>) {
    let mut filtered = Vec::new();
    let mut language = None;
    let mut diags: Vec<PendingDiag> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--language" || args[i] == "--lang" {
            match args.get(i + 1) {
                Some(v) if !is_url(v) && !v.starts_with('-') => {
                    language = Some(v.clone());
                    i += 2;
                }
                _ => {
                    // Deferred (see `PendingDiag`): this runs BEFORE the catalog is
                    // chosen, so rendering now would lock in the env locale and kill
                    // `--language`. The flag token is kept so the user sees their spelling.
                    diags.push(
                        PendingDiag::new(
                            "error.language_needs_value",
                            "{flag}: requires a language code (e.g. --language de)",
                        )
                        .with("flag", &args[i]),
                    );
                    i += 1;
                }
            }
        } else {
            filtered.push(args[i].clone());
            i += 1;
        }
    }
    (filtered, language, diags)
}

// Print the curated fatal-error block (Channel 1, STDERR, never a raw
// error code or tracing event) and exit non-zero. See docs/cli-entry.md §
// "fatal" for the block format and why it's ANSI-free on redirect.
fn fatal(op_key: &str, cause: &str) -> ! {
    let op = crate::strings::get(op_key);
    // WS2: `Error:` is rendered from the translatable `error.level_error` key so
    // the fatal block reads `✗ Error: <op> failed: <cause>.` with the code-forward
    // cause from `crate::pipe::fmt_err`.
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

// Every flag that consumes the following token as its value — the ONE
// source of truth for flag arity, shared by collect_urls and asserted
// against parse_flags by a test. See docs/cli-entry.md § "VALUE_FLAGS".
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

// Whether a token is another FLAG, and so can never be a flag's value.
// The companion to the scheme:// rule; ONE definition shared by both
// parsers. See docs/cli-entry.md § "is_flag_token".
pub(crate) fn is_flag_token(s: &str) -> bool {
    let mut rest = s.strip_prefix('-').unwrap_or("").chars();
    match rest.next() {
        None => false,
        Some(c) => !c.is_ascii_digit(),
    }
}

// Flags this CLI no longer accepts but which DID take a value; collect_urls
// still steps over the value so it doesn't collapse into a bogus third
// positional. See docs/cli-entry.md § "RETIRED_VALUE_FLAGS".
pub(crate) const RETIRED_VALUE_FLAGS: &[&str] = &["-k", "--device", "-d"];

fn collect_urls(args: &[String]) -> Vec<String> {
    // A positional token (not a flag, not a flag's value) is a stream URL, even a
    // schemeless one — kept so `parse_url` can give a clear "needs a scheme" error
    // rather than silently dropping it. Telling a value apart needs `VALUE_FLAGS`.
    let mut urls = Vec::new();
    let mut skip_next = false;
    let mut skip_is_key_url = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            let consume_key_url = skip_is_key_url;
            skip_is_key_url = false;
            // `--key-url`'s value is itself a URL (the key service) — always consumed.
            // For other value-flags, a value that looks like a stream URL is a
            // misplaced positional; reclassify it so `--keydb disc:// mkv://` rips.
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

// Format the per-stream summary lines for `info mkv://` / `info m2ts://`.
// v.label/a.label/a.language/s.language are disc-derived strings, so each
// is sanitized before printing. See docs/cli-entry.md § "stream_info_lines".
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
        // `dir://` (an extracted disc folder) enumerates like an image: `scan_dir`
        // synthesizes a UDF volume and returns the same pair as `scan_iso`. `info`
        // was the one place that never learned this, so a folder used to fail here.
        libfreemkv::StreamUrl::Dir { path } | libfreemkv::StreamUrl::Iso { path } => {
            // Listing titles needs NO AACS key — scan keylessly and reuse disc_info's
            // full title list; the key-gated `input()` would hit E7022 on an encrypted
            // disc. Flags use the SAME parser as `disc://`, so an unknown one exits 1.
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
                    // LOCALIZED like the `disc://` arm above — these were the last
                    // hard-coded English labels in `info`. Reuses `disc.*` keys (the
                    // info-output LABEL set, not disc-only) instead of minting `info.*`.
                    println!(
                        "{}: {}",
                        crate::strings::get_or("disc.file", "File"),
                        parsed.path_str()
                    );
                    if meta.duration_secs > 0.0 {
                        let d = meta.duration_secs;
                        println!(
                            "{}: {}:{:02}:{:02}",
                            crate::strings::get("disc.duration"),
                            d as u64 / 3600,
                            (d as u64 % 3600) / 60,
                            d as u64 % 60
                        );
                    }
                    println!(
                        "{}: {}",
                        crate::strings::get("disc.streams"),
                        meta.streams.len()
                    );
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

// Destination-only schemes, with English fallback text (crate::strings::
// get_or) until freemkv-i18n ships their keys. See docs/cli-entry.md §
// "TRACK_SINK_URL_LINES".
const TRACK_SINK_URL_LINES: &[(&str, &str)] = &[
    (
        "usage.url.demux",
        "  demux://folder/          Every track as a separate file",
    ),
    (
        "usage.url.video",
        "  video://folder/          Video tracks only",
    ),
    (
        "usage.url.audio",
        "  audio://folder/          Audio tracks only",
    ),
    (
        "usage.url.sub",
        "  sub://folder/            Subtitle tracks only",
    ),
    (
        "usage.url.chapters",
        "  chapters://file.xml      Chapter list (.xml, .txt/.ogm, .vtt)",
    ),
    (
        "usage.url.json",
        "  json://file.json         Title structure as JSON",
    ),
    (
        "usage.url.fvi",
        "  fvi://file.fvi           Per-frame video index",
    ),
];

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
    // EVERY scheme the URL pipeline accepts, not the seven it used to list — over
    // half the working schemes were previously README-only. Split in two: the
    // second group has no `input()` arm (write-only), so it's dest-only below.
    println!("{}", crate::strings::get("usage.urls_header"));
    println!("{}", crate::strings::get("usage.url.disc_auto"));
    println!("{}", crate::strings::get("usage.url.disc_linux"));
    println!("{}", crate::strings::get("usage.url.disc_windows"));
    println!("{}", crate::strings::get("usage.url.mkv"));
    println!("{}", crate::strings::get("usage.url.m2ts"));
    println!(
        "{}",
        crate::strings::get_or("usage.url.mp4", "  mp4://path.mp4           MP4 file")
    );
    println!("{}", crate::strings::get("usage.url.iso"));
    println!(
        "{}",
        crate::strings::get_or(
            "usage.url.dir",
            "  dir://folder/            Decrypted file tree in a folder",
        )
    );
    println!("{}", crate::strings::get("usage.url.network"));
    println!("{}", crate::strings::get("usage.url.stdio"));
    println!("{}", crate::strings::get("usage.url.null"));
    println!();
    println!(
        "{}",
        crate::strings::get_or("usage.tracks_header", "Track outputs (destination only):")
    );
    for (key, english) in TRACK_SINK_URL_LINES {
        println!("{}", crate::strings::get_or(key, english));
    }
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
    // `--language`/`--lang` has worked since the i18n crate landed but was listed
    // nowhere (not here, not the README) — the only way to override the locale.
    println!(
        "{}",
        crate::strings::get_or(
            "usage.flag.language",
            "      --language CODE Interface language (also --lang): a code like de or pt-BR, or auto.",
        )
    );
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

// Resolve where update-keys saves the downloaded keydb: --keydb <path>
// wins, else the standard search/default location. See docs/cli-entry.md §
// "update_keys_dest".
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
    // Fetch keydb bytes via ureq (HTTP+HTTPS) and hand them to the keydb source to
    // verify + atomically save. The CLI supplies its own SSRF-guarded transport
    // (`crate::keydb_fetch::fetch`); the keydb source stays transport-agnostic.
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
    // A value-taking logging flag must not swallow the NEXT FLAG as its
    // value (e.g. --log-file --raw would eat --raw and run without it).
    // See docs/cli-entry.md § "a_logging_flag_does_not_swallow...".
    #[test]
    fn a_logging_flag_does_not_swallow_the_following_flag() {
        for flag in ["--log-file", "--log-level"] {
            let args: Vec<String> = [flag, "--raw", "disc://", "iso:///out/d.iso"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            let (level, log_file, _) = super::parse_logging_flags(&args);
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
        let (level, log_file, _) = super::parse_logging_flags(&args);
        assert_eq!(log_file, None, "--log-file had no value to take");
        assert_eq!(
            level,
            Some(3),
            "--log-level was swallowed by the flag before it"
        );
    }

    use super::{SUBCOMMANDS, collect_urls, stream_info_lines, update_keys_dest};

    // Covers the tag-assembly arms of stream_info_lines (purpose/secondary/
    // label) that the escape-stripping test below never reaches. See
    // docs/cli-entry.md § "stream_info_lines_render_purpose...".
    #[test]
    fn stream_info_lines_render_purpose_secondary_and_label_tags() {
        use libfreemkv::{
            AudioChannels, AudioStream, Codec, ColorSpace, FrameRate, HdrFormat, LabelPurpose,
            LabelQualifier, Resolution, Stream, SubtitleStream, VideoStream,
        };
        crate::strings::set_locale("en");

        // Every purpose arm renders a distinct, non-empty tag.
        for (purpose, needle) in [
            (LabelPurpose::Commentary, "Commentary"),
            (LabelPurpose::Descriptive, "Descriptive"),
            (LabelPurpose::Score, "Score"),
        ] {
            let a = Stream::Audio(AudioStream {
                pid: 0x1100,
                codec: Codec::Ac3,
                channels: AudioChannels::Stereo,
                language: "eng".into(),
                sample_rate: SampleRate::S48,
                secondary: false,
                purpose,
                label: String::new(),
            });
            let line = stream_info_lines(&[a]).join("\n");
            assert!(line.contains(needle), "{purpose:?} → {line:?}");
        }

        // A secondary track with a codec-variant label: both the "Secondary"
        // tag and the label survive, joined in one parenthesised group.
        let tagged = Stream::Audio(AudioStream {
            pid: 0x1100,
            codec: Codec::TrueHd,
            channels: AudioChannels::Surround51,
            language: "fra".into(),
            sample_rate: SampleRate::S48,
            secondary: true,
            purpose: LabelPurpose::Commentary,
            label: "Atmos".into(),
        });
        let line = stream_info_lines(&[tagged]).join("\n");
        assert!(
            line.contains("Commentary") && line.contains("Secondary") && line.contains("Atmos"),
            "all three tags present: {line:?}"
        );

        // A video label renders after the resolution; a subtitle line carries
        // its language.
        let video = Stream::Video(VideoStream {
            pid: 0x1011,
            codec: Codec::Hevc,
            resolution: Resolution::R2160p,
            frame_rate: FrameRate::F23_976,
            hdr: HdrFormat::Hdr10,
            color_space: ColorSpace::Bt2020,
            display_aspect: None,
            secondary: false,
            label: "Feature".into(),
            measured_cicp: None,
        });
        let sub = Stream::Subtitle(SubtitleStream {
            pid: 0x1200,
            codec: Codec::Pgs,
            language: "eng".into(),
            forced: false,
            qualifier: LabelQualifier::None,
            codec_data: None,
        });
        let lines = stream_info_lines(&[video, sub]);
        let joined = lines.join("\n");
        assert!(
            joined.contains("Feature"),
            "video label present: {joined:?}"
        );
        // The container `info` path prints the raw on-disc language tag
        // (sanitized), not a resolved name — unlike the disc-info listing.
        assert!(
            joined.contains("eng"),
            "subtitle language present: {joined:?}"
        );
    }

    use libfreemkv::SampleRate;

    // v.label/a.label/a.language/s.language are disc/file-controlled; a
    // crafted terminal escape in any must not survive to the terminal. See
    // docs/cli-entry.md § "stream_info_lines_strip_terminal_escapes...".
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

    // Subcommand names a string tells the user to TYPE, as opposed to
    // "freemkv" just naming the product in a sentence. See
    // docs/cli-entry.md § "commands_named_in" for the exact heuristic.
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
        // Several locale strings used to instruct running `drive-info`/`disc-info`/
        // `remux`/`verify`, none of which are dispatched — they fall through to the
        // URL grammar and fail. Checked across ALL bundled locales (wrong in all 29).
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
        // Regression: `-a`/`-s` values (`none`/`eng`/…) must not be collected as a
        // third stream URL. Before the scheme://-only rewrite, an unlisted value-flag
        // let its value collect as a URL → 3 URLs → usage printed and nothing ran.
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
        // `-k`/`--device`/`-d` are gone but took a value; that value must not
        // become a third positional (3 URLs = bare usage hint, no mention of the
        // removed flag). 2 URLs routes to the rip, where `parse_flags` names it.
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

// The argv decisions run() and init_logging() make before anything else
// happens; previously unreachable from cargo test. See docs/cli-entry.md
// § "mod arg_tests".
#[cfg(test)]
mod arg_tests {
    use super::{
        PendingDiag, is_flag_token, is_url, parse_logging_flags, split_log_path,
        strip_language_flag, wants_help,
    };

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // The one deliberate exception: a leading `-` on a NEGATIVE NUMBER is a
    // value, not a flag, so `--log-level -1` reaches the range check. See
    // docs/cli-entry.md § "is_flag_token_treats_negative_numbers...".
    #[test]
    fn is_flag_token_treats_negative_numbers_as_values_not_flags() {
        assert!(is_flag_token("--raw"));
        assert!(is_flag_token("-t"));
        assert!(!is_flag_token("-1"), "a negative number is a value");
        assert!(!is_flag_token("-9"));
        assert!(!is_flag_token("disc://"), "a positional is not a flag");
        assert!(!is_flag_token(""), "an empty token is not a flag");
        // A bare '-' has nothing after the dash — not a flag.
        assert!(!is_flag_token("-"));
    }

    /// `is_url` is the schemeless-URL gate: anything carrying `://` is a
    /// positional stream URL, everything else (a keydb path, a bare number) is
    /// not — so a value-flag does not misread its value as a positional.
    #[test]
    fn is_url_matches_only_scheme_bearing_tokens() {
        assert!(is_url("disc://"));
        assert!(is_url("mkv://out.mkv"));
        assert!(is_url("https://keys.example/api"));
        assert!(!is_url("keydb.cfg"));
        assert!(!is_url("/path/to/out.mkv"));
        assert!(!is_url("3"));
    }

    // Just the two REQUESTS; diagnostics are a separate axis with their own
    // tests. See docs/cli-entry.md § "flags(...) test helper".
    fn flags(args: &[String]) -> (Option<u8>, Option<String>) {
        let (level, file, _) = parse_logging_flags(args);
        (level, file)
    }

    fn keys(diags: &[PendingDiag]) -> Vec<&'static str> {
        diags.iter().map(|d| d.key).collect()
    }

    /// The common path: no logging flags means NO subscriber is installed and
    /// the terminal stays clean. If either half of that reads as "requested",
    /// every plain `freemkv` run starts writing ./log.txt.
    #[test]
    fn no_logging_flags_requests_nothing() {
        assert_eq!(flags(&v([].as_slice())), (None, None));
        assert_eq!(
            flags(&v(&["freemkv", "iso://a.iso", "mkv://b.mkv", "-t", "2"])),
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

    // Bad input is reported and IGNORED, never clamped up to 1. See
    // docs/cli-entry.md § "a_bad_log_level_is_ignored_rather_than_guessed_at".
    #[test]
    fn a_bad_log_level_is_ignored_rather_than_guessed_at() {
        assert_eq!(parse_logging_flags(&v(&["--log-level", "0"])).0, None);
        assert_eq!(parse_logging_flags(&v(&["--log-level", "xyz"])).0, None);
        assert_eq!(parse_logging_flags(&v(&["--log-level", "-1"])).0, None);
        // Last token, no value: no panic, nothing requested.
        assert_eq!(flags(&v(&["--log-level"])), (None, None));
    }

    /// `--log-file` takes the following token, in either flag order, and does
    /// not need `--log-level` to be present.
    #[test]
    fn the_log_file_flag_takes_the_next_token_in_either_order() {
        assert_eq!(
            flags(&v(&["--log-file", "/tmp/x.log"])),
            (None, Some("/tmp/x.log".to_string()))
        );
        assert_eq!(
            flags(&v(&["--log-file", "a.log", "--log-level", "2"])),
            (Some(2), Some("a.log".to_string()))
        );
        assert_eq!(
            flags(&v(&["--log-level", "2", "--log-file", "a.log"])),
            (Some(2), Some("a.log".to_string()))
        );
        // Trailing `--log-file` with no value: nothing requested, no panic.
        assert_eq!(flags(&v(&["--log-file"])), (None, None));
    }

    // A refused --log-file value must be REPORTED, not swallowed silently —
    // absence of a log is itself a bug. See docs/cli-entry.md §
    // "a_refused_log_file_value_records_a_diagnostic".
    #[test]
    fn a_refused_log_file_value_records_a_diagnostic() {
        // Next token is a flag: value refused, so the path is None AND a
        // complaint is recorded.
        let (_, file, diags) = parse_logging_flags(&v(&["--log-file", "--raw"]));
        assert_eq!(file, None);
        assert_eq!(keys(&diags), vec!["error.log_file_needs_value"]);

        // Last token, no value at all: same complaint.
        let (_, file, diags) = parse_logging_flags(&v(&["--log-file"]));
        assert_eq!(file, None);
        assert_eq!(keys(&diags), vec!["error.log_file_needs_value"]);

        // A real value still takes cleanly and records nothing.
        let (_, file, diags) = parse_logging_flags(&v(&["--log-file", "out.log"]));
        assert_eq!(file.as_deref(), Some("out.log"));
        assert!(diags.is_empty());
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
        let (args, lang, _) = strip_language_flag(&v(&["freemkv", "--language", "de", "disc://"]));
        assert_eq!(lang.as_deref(), Some("de"));
        assert_eq!(args, v(&["freemkv", "disc://"]));

        // The short alias behaves identically.
        let (args, lang, _) = strip_language_flag(&v(&["freemkv", "--lang", "de", "disc://"]));
        assert_eq!(lang.as_deref(), Some("de"));
        assert_eq!(args, v(&["freemkv", "disc://"]));

        // A URL is not a language code — keep it positional.
        let (args, lang, _) =
            strip_language_flag(&v(&["freemkv", "--language", "disc://", "mkv://x.mkv"]));
        assert_eq!(lang, None);
        assert_eq!(args, v(&["freemkv", "disc://", "mkv://x.mkv"]));

        // Nor is a flag.
        let (args, lang, _) = strip_language_flag(&v(&["freemkv", "--language", "--verbose"]));
        assert_eq!(lang, None);
        assert_eq!(args, v(&["freemkv", "--verbose"]));

        // Last token: no value, no panic.
        let (args, lang, _) = strip_language_flag(&v(&["freemkv", "--language"]));
        assert_eq!(lang, None);
        assert_eq!(args, v(&["freemkv"]));

        // Absent entirely: the argument list is untouched.
        let original = v(&["freemkv", "iso://a.iso", "mkv://b.mkv"]);
        let (args, lang, _) = strip_language_flag(&original);
        assert_eq!(lang, None);
        assert_eq!(args, original);
    }

    // Every deferred diagnostic must round-trip through the real catalog
    // (checked against strings::get, never PendingDiag::render, whose
    // English fallback would make the check vacuous). See docs/cli-entry.md.
    #[test]
    fn every_deferred_startup_diagnostic_resolves_to_real_localized_text() {
        let cases = [
            (v(&["--log-level", "0"]), "error.log_level_out_of_range", ""),
            (
                v(&["--log-level", "xyz"]),
                "error.log_level_not_a_number",
                "xyz",
            ),
            (v(&["--log-level"]), "error.log_level_needs_value", ""),
        ];
        for (args, key, must_contain) in cases {
            let (_, _, diags) = parse_logging_flags(&args);
            assert_eq!(keys(&diags), vec![key], "for argv {args:?}");
            // Assert against the RAW catalog, not `PendingDiag::render`: render's
            // fallback always turns a key echo into English, so `assert_ne!(render,
            // key)` could never fail. `strings::get` echoes the key on a miss instead.
            let raw = crate::strings::get(key);
            assert_ne!(
                raw, key,
                "'{key}' has no entry in the catalog — nothing localizes it, and \
                 only `PendingDiag`'s English fallback was hiding that"
            );
            // Render still has to fill placeholders and carry the typed value.
            let text = diags[0].render();
            assert!(
                !text.contains('{'),
                "'{key}' rendered with an unsubstituted placeholder: {text}"
            );
            assert!(
                text.contains(must_contain),
                "'{key}' dropped the value the user typed: {text}"
            );
        }

        // The `--log-file` invalid-path half lives in `init_logging`, which
        // installs a process-global subscriber and so cannot be called from a
        // test. Its key is checked directly against the raw catalog instead.
        let key = "error.log_file_invalid_path";
        assert_ne!(
            crate::strings::get(key),
            key,
            "'{key}' is not in the catalog"
        );
        let text = PendingDiag::new(key, "unused fallback")
            .with("path", "/")
            .render();
        assert!(text.contains('/') && !text.contains('{'), "{text}");

        // And the language flag's own complaint, for both spellings.
        assert_ne!(
            crate::strings::get("error.language_needs_value"),
            "error.language_needs_value",
            "the language-needs-value key is not in the catalog"
        );
        for flag in ["--language", "--lang"] {
            let (_, lang, diags) = strip_language_flag(&v(&["freemkv", flag]));
            assert_eq!(lang, None);
            assert_eq!(keys(&diags), vec!["error.language_needs_value"]);
            let text = diags[0].render();
            assert!(
                text.contains(flag) && !text.contains('{'),
                "the complaint must name the spelling the user actually typed, \
                 got: {text}"
            );
        }
    }

    // The argv pre-pass must not print anything ITSELF — a strings::get or
    // eprintln! here runs before locale is resolved. See docs/cli-entry.md
    // § "the_pre_locale_argv_pass_prints_nothing_of_its_own".
    #[test]
    fn the_pre_locale_argv_pass_prints_nothing_of_its_own() {
        let src = include_str!("cli_entry.rs").replace("\r\n", "\n");
        let slice = |from: &str, to: &str| -> String {
            let a = src
                .find(from)
                .unwrap_or_else(|| panic!("anchor missing: {from}"));
            let b = src[a..]
                .find(to)
                .unwrap_or_else(|| panic!("closing anchor missing: {to}"));
            src[a..a + b].to_string()
        };
        let regions = [
            (
                "parse_logging_flags",
                slice(
                    "fn parse_logging_flags(args: &[String])",
                    "\n// Every word the dispatcher matches",
                ),
            ),
            (
                "strip_language_flag",
                slice(
                    "fn strip_language_flag(args: &[String])",
                    "\n// Print the curated fatal-error block",
                ),
            ),
        ];
        for (name, body) in regions {
            for banned in ["eprintln!", "println!", "eprint!", "print!"] {
                assert!(
                    !body.contains(banned),
                    "{name} contains a `{banned}`: it runs BEFORE the locale is \
                     resolved, so anything it prints is hard-coded English — \
                     and anything it localizes locks in the wrong catalog and \
                     kills --language. Record a PendingDiag instead."
                );
            }
            assert!(
                body.contains("PendingDiag::new("),
                "{name} no longer records any deferred diagnostic — the bad-input \
                 paths have gone silent"
            );
        }
    }

    // The `info` subcommand must speak ONE language, whatever the URL —
    // source-pinned since neither the container arm nor --share is
    // reachable from a test. See docs/cli-entry.md § "the_info_surface...".
    #[test]
    fn the_info_surface_never_prints_english_of_its_own() {
        // CRLF-normalized: Windows CI checks the tree out with CRLF.
        let entry = include_str!("cli_entry.rs").replace("\r\n", "\n");
        let info = include_str!("info.rs").replace("\r\n", "\n");

        // Banned shape: the literal as a DIRECT macro argument (English also
        // legitimately appears as `get_or`'s fallback `english` arg) — testing
        // what's printed, not what's in the file. Collapsed/`concat!`'d against rustfmt.
        let squash = |s: &str| -> String { s.split_whitespace().collect() };
        let entry_sq = squash(&entry);
        let info_sq = squash(&info);
        for (file, src, needle) in [
            (
                "cli_entry.rs",
                &entry_sq,
                concat!("println!(", "\"File: {}\""),
            ),
            (
                "cli_entry.rs",
                &entry_sq,
                concat!("println!(", "\"Duration: {}"),
            ),
            (
                "cli_entry.rs",
                &entry_sq,
                concat!("println!(", "\"Streams: {}\""),
            ),
            (
                "info.rs",
                &info_sq,
                concat!("println!(", "\"Submitted — thank"),
            ),
            (
                "info.rs",
                &info_sq,
                concat!("eprintln!(", "\"Cannot write {}"),
            ),
            (
                "info.rs",
                &info_sq,
                concat!("eprint!(", "\"Submit this profile"),
            ),
        ] {
            let needle = squash(needle);
            assert!(
                !src.contains(&needle),
                "{file} prints `{needle}` directly — that is one subcommand with \
                 two languages, which is the drift the shared catalog exists to \
                 stop"
            );
        }

        for (file, src, key) in [
            ("cli_entry.rs", &entry, "\"disc.file\""),
            ("cli_entry.rs", &entry, "\"disc.duration\""),
            ("cli_entry.rs", &entry, "\"disc.streams\""),
            ("info.rs", &info, "\"drive.submit_prompt\""),
            ("info.rs", &info, "\"drive.submit_thanks\""),
            ("info.rs", &info, "\"drive.submit_auto_failed\""),
            ("info.rs", &info, "\"drive.submit_declined\""),
            ("info.rs", &info, "\"error.cannot_write\""),
        ] {
            assert!(
                src.contains(key),
                "{file} no longer looks up {key} — the line it rendered has \
                 either gone silent or gone back to English"
            );
        }
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
