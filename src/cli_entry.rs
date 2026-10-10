// freemkv — Open source 4K UHD / Blu-ray / DVD backup tool (MIT).
// Usage: freemkv <source> <dest> [flags] | freemkv info <url> [flags]
// (module decls + global allocator live in main.rs; this is the CLI shell entry point.)

/// Worker guard for the optional non-blocking file log layer. Held for the
/// life of the process; [`exit`] drops it so buffered records are flushed before
/// the process ends. `None` when no diagnostic log is installed.
static LOG_GUARD: std::sync::Mutex<Option<tracing_appender::non_blocking::WorkerGuard>> =
    std::sync::Mutex::new(None);

/// Flush and close the diagnostic log. The process ends without running
/// destructors, so records still queued would otherwise be lost.
pub(crate) fn flush_log() {
    drop(LOG_GUARD.lock().unwrap_or_else(|e| e.into_inner()).take());
}

/// `std::process::exit` after [`flush_log`].
pub(crate) fn exit(code: i32) -> ! {
    flush_log();
    std::process::exit(code)
}

/// Default diagnostic log path when `--log-level` is given without an explicit
/// `--log-file`. Written in the working directory, matching the fatal-error
/// hint ("re-run with --log-level 3 (writes ./log.txt)").
const DEFAULT_LOG_FILE: &str = "log.txt";

// Tracing has two channels; PendingDiag holds a startup diagnostic that can't render yet — see
// § "PendingDiag" below.
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

    // The localized text, separate from emit() so a test can read it.
    fn render(&self) -> String {
        let args: Vec<(&str, &str)> = self.args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        crate::strings::fmt_or(self.key, self.english, &args)
    }

    // Print to stderr. Must not be called before `strings::init()`.
    fn emit(&self) {
        eprintln!("{}", self.render());
    }
}

// The two logging flags, parsed out of the raw argv. Split from init_logging so it's
// unit-testable (that fn installs a process-global subscriber).
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
            "--log-level" => match it.next_if(|s| !is_flag_token(s) && !is_url_token(s)) {
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
            },
            "--log-file" => {
                match it.next_if(|s| !is_flag_token(s) && !is_url_token(s)) {
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

// Open a `--log-file` value for append. `None` for a path with no filename, a
// non-UTF-8 name, or a file that can't be opened (`rolling::never` would panic on it).
fn open_log_file(path: &str) -> Option<tracing_appender::rolling::RollingFileAppender> {
    let (dir, name) = split_log_path(path)?;
    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::NEVER)
        .filename_prefix(name.to_str()?)
        .build(dir)
        .ok()
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
    let opened = open_log_file(&path);
    let file_appender = match opened {
        Some(appender) => appender,
        None => {
            // An invalid or unopenable `--log-file` path is a misconfiguration of
            // the diagnostic channel — report it cleanly on the terminal (this is a
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
    *LOG_GUARD.lock().unwrap_or_else(|e| e.into_inner()) = Some(guard);
    let file_layer = fmt::layer().with_ansi(false).with_writer(nb);
    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .init();
    diags
}

// Every word the dispatcher matches args[1] against. "gui" opens the window in the app
// build; in the CLI build it reaches `run` and prints where to get the app.
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
    let args = drop_process_serial(args);
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
        exit(2);
    }

    match args[1].as_str() {
        // `freemkv <cmd> --help` / `freemkv <cmd> -h` print command-specific help.
        // Handled before the per-command dispatch so the flag never reaches the
        // command's own argument parser.
        "info" if wants_help(&args[2..]) => help_info(),
        "update-keys" if wants_help(&args[2..]) => help_update_keys(),

        "info" => info_cmd(&args[2..]),
        "update-keys" => update_keys(&args[2..]),
        // Only reached in the CLI build; the app build opens the window before `run`.
        "gui" => {
            eprintln!("{}", crate::strings::get("error.gui_not_in_build"));
            exit(2);
        }
        // NOTE: deliberately no `remux`/conversion verb. The operation IS the
        // URL pair: `freemkv <source-url> <dest-url> [opts]` — source→dest is
        // the whole grammar, so a conversion "command" would be redundant.
        "version" | "--version" | "-V" => println!("{}", env!("CARGO_PKG_VERSION")),
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
                exit(2);
            }
        },

        // Everything else: freemkv <source> <dest>
        _ => {
            let urls = collect_urls(&args[1..]);

            if urls.len() == 2 {
                let code = crate::pipe::run(&urls[0], &urls[1], &args[1..]);
                if code != 0 {
                    // `pipe::run` already printed the curated cause/result; just
                    // propagate its exit code (`freemkv help` documents each one).
                    exit(code);
                }
            } else if urls.len() == 1 {
                // Single URL, no dest — show info. `info_cmd` wants the URL at
                // `args[0]`, so prepend it, then drop it from its original slot
                // ONCE, matched canonically (scheme case / slash), so no dup/miss.
                let url = urls[0].clone();
                let mut info_args = vec![url.clone()];
                let mut removed = false;
                for a in &args[1..] {
                    if !removed && same_stream_url(a, &url) {
                        removed = true;
                        continue;
                    }
                    info_args.push(a.clone());
                }
                info_cmd(&info_args);
            } else {
                eprintln!("{}", crate::strings::get("error.usage_hint"));
                exit(1);
            }
        }
    }
    flush_log();
}

/// True if `s` looks like a stream URL (`scheme://...`).
pub(crate) fn is_url_token(s: &str) -> bool {
    freemkv::app_entry::is_url_token(s)
}

/// Whether two argv tokens name the SAME stream URL. Exact byte-equality first
/// (the common case, and the only comparison schemeless tokens support), then a
/// canonical comparison — scheme lowercased, a trailing `/` ignored — so
/// `DISC://` and `disc://`, or `disc://dev/sr0` and `disc://dev/sr0/`, match.
fn same_stream_url(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (canon_url(a), canon_url(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Canonical form of a `scheme://rest` URL for equivalence checks: scheme
/// lowercased, trailing slashes trimmed off the remainder. `None` for a
/// schemeless token, which has no canonical form to compare.
fn canon_url(s: &str) -> Option<String> {
    let (scheme, rest) = s.split_once("://")?;
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        rest.trim_end_matches('/')
    ))
}

// Drop a leading macOS process serial (`-psn_0_<n>`, see `app_entry::is_process_serial`): the
// app build already ignores it, and here it would only be taken for a source.
fn drop_process_serial(mut args: Vec<String>) -> Vec<String> {
    if args
        .get(1)
        .is_some_and(|a| freemkv::app_entry::is_process_serial(a))
    {
        args.remove(1);
    }
    args
}

// Pull --language/--lang and its value out of the argument list, with the same URL-value guard
// as collect_urls.
fn strip_language_flag(args: &[String]) -> (Vec<String>, Option<String>, Vec<PendingDiag>) {
    let mut filtered = Vec::new();
    let mut language = None;
    let mut diags: Vec<PendingDiag> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if freemkv::app_entry::LANGUAGE_FLAGS.contains(&args[i].as_str()) {
            match args.get(i + 1) {
                // Same value-guard as `pipe::parse_flags`, and the one the app build's
                // `wants_gui` applies: a value is neither a URL nor a flag.
                Some(v) if freemkv::app_entry::is_flag_value(v) => {
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

// The cause of a failed `info`. Everything `info` touches is the source, so an OS error
// reads as the OS describes it, never as E5000, whose advice is about the destination.
fn info_failure_cause(e: &libfreemkv::Error) -> String {
    match e {
        libfreemkv::Error::IoError { source } => crate::pipe::fmt_err(source),
        _ => crate::pipe::fmt_err(e),
    }
}

// Print the curated fatal-error block (Channel 1, STDERR, never a raw error code or tracing
// event) and exit non-zero.
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
    exit(1);
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

// Every flag that consumes the following token as its value — the ONE source of truth for flag
// arity, shared by collect_urls and asserted against parse_flags by a test.
pub(crate) const VALUE_FLAGS: &[&str] = &[
    "-t",
    "--title",
    "-a",
    "--audio",
    "--presentation-language",
    "-s",
    "--subtitles",
    "--keydb",
    "--key-url",
    "--key-auth",
    "--log-file",
    "--log-level",
];

// Whether a token is another FLAG, and so can never be a flag's value. The companion to the
// scheme:// rule; ONE definition shared by both parsers.
pub(crate) fn is_flag_token(s: &str) -> bool {
    freemkv::app_entry::is_flag_token(s)
}

// Flags this CLI no longer accepts but which DID take a value; collect_urls still steps over
// the value so it doesn't collapse into a bogus third positional.
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
            if !consume_key_url && is_url_token(arg) {
                urls.push(arg.clone());
            }
            continue;
        }
        // Shared `is_flag_token` (a negative number is a value, not a flag),
        // the SAME flag detection `pipe::parse_flags` and `strip_language_flag`
        // use — one rule, not a bare `starts_with('-')` here and a helper there.
        if is_flag_token(arg) {
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
// v.label/a.label/a.language/s.language are disc-derived strings, so each is sanitized before
// printing.
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

/// Whether `info` reads `url` as a stream container and lists its streams (G10): every
/// stream container, mp4:// and mpg:// included.
fn info_lists_streams(url: &libfreemkv::StreamUrl) -> bool {
    matches!(
        url,
        libfreemkv::StreamUrl::M2ts { .. }
            | libfreemkv::StreamUrl::Mkv { .. }
            | libfreemkv::StreamUrl::Mp4 { .. }
            | libfreemkv::StreamUrl::Mpg { .. }
    )
}

fn info_cmd(args: &[String]) {
    if args.is_empty() {
        eprintln!("{}", crate::strings::get("error.info_usage"));
        exit(1);
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
            // `--share` on an image/folder captures the disc STRUCTURE profile
            // (there is no drive here), mirroring how `disc://` routes `--share`
            // to `info::run` — and with that route's flag parser.
            let share = args[1..].iter().any(|a| a == "--share" || a == "-s");

            // A folder needs scan_dir (which additionally decides the
            // encryption verdict from CONTENT rather than from whether an
            // AACS/ directory survived the copy); an image needs scan_iso.
            let scan = if matches!(parsed, libfreemkv::StreamUrl::Dir { .. }) {
                libfreemkv::scan_dir
            } else {
                libfreemkv::scan_iso
            };

            if share {
                // Same parser as `disc:// --share`: unknown flags exit 1, `--help` prints help.
                let flags = match crate::info::parse_drive_flags(&args[1..]) {
                    crate::info::DriveParse::Ok(f) => f,
                    crate::info::DriveParse::Help => return crate::info::print_share_help(),
                    crate::info::DriveParse::Unknown(opt) => {
                        crate::disc_info::reject_unknown_option(&opt)
                    }
                };
                let (disc, mut reader) = match scan(
                    std::path::Path::new(path),
                    libfreemkv::ScanOptions::default(),
                ) {
                    Ok(pair) => pair,
                    Err(e) => fatal("error.op_info", &info_failure_cause(&e)),
                };
                let label = std::path::Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("disc");
                crate::disc_capture::run(&disc, reader.as_mut(), label, flags.quiet);
                return;
            }

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
            let (disc, _reader) = match scan(
                std::path::Path::new(path),
                libfreemkv::ScanOptions::default(),
            ) {
                Ok(pair) => pair,
                Err(e) => fatal("error.op_info", &info_failure_cause(&e)),
            };
            if !flags.quiet {
                println!("freemkv {}", env!("CARGO_PKG_VERSION"));
                println!();
            }
            crate::disc_info::print_disc_titles(&disc, &flags);
        }
        u if info_lists_streams(u) => {
            // Same parser as the iso/dir/disc arms: `--keydb` is honoured, an unknown flag exits 1.
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
            let Ok(keys) = crate::pipe::info_clip_keys(url, flags.keydb.clone()) else {
                exit(1);
            };
            match freemkv_engine::stream_info(url, keys, &libfreemkv::Halt::new()) {
                Ok(meta) => {
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
                Err(e) => fatal("error.op_info", &info_failure_cause(&e)),
            }
        }
        libfreemkv::StreamUrl::Unknown { .. } => {
            eprintln!(
                "{}",
                crate::strings::fmt("error.info_unknown_url", &[("url", url)])
            );
            exit(1);
        }
        _ => {
            eprintln!(
                "{}",
                crate::strings::fmt("error.info_unsupported_url", &[("url", url)])
            );
            exit(1);
        }
    }
}

// Destination-only schemes, with English fallback text (crate::strings:: get_or) until
// freemkv-i18n ships their keys.
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
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
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
    println!("{}", crate::strings::get("usage.url.mpg"));
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
    println!(
        "{}",
        crate::strings::get("usage.flag.presentation_language")
    );
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
    println!();
    println!(
        "{}",
        crate::strings::get_or("usage.exit_codes_header", "Exit codes:")
    );
    println!(
        "{}",
        crate::strings::get_or("usage.exit_code.ok", "  0    Success.")
    );
    println!(
        "{}",
        crate::strings::get_or(
            "usage.exit_code.failed",
            "  1    Failed: no usable output, or any other error."
        )
    );
    println!(
        "{}",
        crate::strings::get_or(
            "usage.exit_code.usage",
            "  2    Usage: bad command or flags (this text was also printed)."
        )
    );
    println!(
        "{}",
        crate::strings::get_or(
            "usage.exit_code.damaged",
            "  3    Damaged (disc:// -> iso:// or disc:// -> null:// only): kept and \
             usable, but short some sectors — re-run (--multipass) to try for more.",
        )
    );
    println!(
        "{}",
        crate::strings::get_or(
            "usage.exit_code.interrupted",
            "  130  A second Ctrl-C forced an immediate exit."
        )
    );
}

/// True if a command's argument list requests its help (`--help` / `-h`).
/// Used to route `freemkv <cmd> --help` to the per-command help text before the
/// command's own parser runs.
fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// `freemkv info --help` / `freemkv help info`.
fn help_info() {
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
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
    println!("freemkv {}", env!("CARGO_PKG_VERSION"));
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

// Resolve where update-keys saves the downloaded keydb: --keydb <path> wins, else the standard
// search/default location.
fn update_keys_dest(args: &[String]) -> std::path::PathBuf {
    let mut keydb: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--keydb" {
            // Only a real value, never the next flag or URL (as `info --keydb`).
            if let Some(v) = args
                .get(i + 1)
                .filter(|v| !is_flag_token(v) && !is_url_token(v))
            {
                keydb = Some(v.clone());
                i += 1;
            }
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
            exit(1);
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
#[path = "cli_entry_tests.rs"]
mod tests;

// The argv decisions run() and init_logging() make before anything else happens; previously
// unreachable from cargo test.
#[cfg(test)]
#[path = "cli_entry_arg_tests.rs"]
mod arg_tests;
