//! Pipe — stream in, stream out.
//!
//! One pipeline for everything:
//!   1. disc→ISO: Disc::copy() (not a stream)
//!   2. Everything else: input → PES → output, one title at a time
//!
//! Batch (multiple titles) is just a for loop calling pipe() per title.

use crate::cli_entry::is_url_token;
use crate::disc_info::sanitize;
use crate::output::{
    Level::{Always, Normal},
    Output,
};
use crate::strings;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::cli_stop::INTERRUPTED;

struct PipeFail {
    display: String,
    /// How the multi-title loop should classify this failure. The loop feeds it
    /// to `freemkv_engine::decide_title` — the SINGLE source of the skip / stop
    /// / fail policy, shared with autorip and the desktop UI. (Replaces the old
    /// `skippable_stub` bool: the engine now also distinguishes a halt and a
    /// disc-level no-key so the loop can full-stop / fail-fast.)
    result: freemkv_engine::TitleResult,
}

impl PipeFail {
    /// A hard failure that can never be skipped (setup / preflight). Classified
    /// `Failed` so the loop treats it as a hard error for the current title.
    fn fatal(display: String) -> Self {
        PipeFail {
            display,
            result: freemkv_engine::TitleResult::Failed,
        }
    }

    /// A cooperative user stop (Ctrl-C). Classified `Halted` so the loop treats
    /// it as a FULL STOP — not a per-title cancel that carries on.
    fn halted(display: String) -> Self {
        PipeFail {
            display,
            result: freemkv_engine::TitleResult::Halted,
        }
    }

    fn from_typed(e: libfreemkv::Error) -> Self {
        let display = e.to_string();
        let io: std::io::Error = e.into();
        PipeFail {
            result: freemkv_engine::classify_title_error(&io),
            display,
        }
    }

    /// A failure surfaced by `mux_with_keys`. Classifies the typed `io::Error` via
    /// the engine (kills the CLI E-code string-match) and renders its `E<code>`
    /// Display for the user.
    fn from_mux(e: std::io::Error) -> Self {
        PipeFail {
            result: freemkv_engine::classify_title_error(&e),
            display: format!("{e}"),
        }
    }
}

// Renders the engine's per-title loop decisions: a skipped stub and a title that stopped
// the rip.
struct CliTitleLoopSink<'a> {
    out: &'a Output,
}

impl freemkv_engine::Sink for CliTitleLoopSink<'_> {
    fn event(&self, e: &freemkv_engine::Event<'_>) {
        match e {
            freemkv_engine::Event::TitleSkipped { idx, empty } => {
                let key = if *empty {
                    "rip.title_skipped_empty"
                } else {
                    "rip.title_skipped"
                };
                self.out.raw(
                    Normal,
                    &strings::fmt(key, &[("num", &(idx + 1).to_string())]),
                );
                self.out.blank(Normal);
            }
            freemkv_engine::Event::TitleFailed { error, .. } => {
                self.out.raw(Always, &render_error(error));
                self.out.blank(Normal);
            }
            _ => {}
        }
    }

    fn should_cancel(&self) -> bool {
        crate::cli_stop::token().is_cancelled()
    }
}

struct CliMuxEvents {
    out: Output,
    dest: String,
    /// A metadata sink (`chapters://` / `json://`): suppress the post-open blank
    /// line and (at the call site) the completion summary — matching the old
    /// short-circuit which printed neither.
    metadata_sink: bool,
    /// When the frame pump began — set in `on_output_opened`, read back for the
    /// completion summary. `None` until the sink opens.
    start: Mutex<Option<Instant>>,
    /// Last time the progress line was repainted (0.5 s throttle).
    last_update: Mutex<Instant>,
}

impl CliMuxEvents {
    fn new(out: Output, dest: String, metadata_sink: bool) -> Self {
        CliMuxEvents {
            out,
            dest,
            metadata_sink,
            start: Mutex::new(None),
            last_update: Mutex::new(Instant::now()),
        }
    }

    /// The instant the pump began, for the completion summary.
    fn start(&self) -> Option<Instant> {
        *self.start.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl libfreemkv::Events for CliMuxEvents {
    fn event(&self, e: &libfreemkv::Event<'_>) {
        match *e {
            libfreemkv::Event::OutputOpened { title } => self.on_output_opened(title),
            libfreemkv::Event::BytesWritten { bytes, total } => {
                self.on_write_progress(bytes, total)
            }
            _ => {}
        }
    }
}

impl CliMuxEvents {
    fn on_output_opened(&self, title: &libfreemkv::DiscTitle) {
        // The excluded-track note first, as the GUI logs it before the mux starts;
        // no event earlier than this one carries the title.
        print_excluded(&self.out, &self.dest, title);
        print_stream_info(&self.out, title);
        // The destination open notice (the sink is already open here).
        self.out.raw_inline(
            Normal,
            &strings::fmt("rip.opening", &[("device", &self.dest)]),
        );
        self.out.raw(Normal, &strings::get("rip.ok"));
        if !self.metadata_sink {
            self.out.blank(Normal);
        }
        let now = Instant::now();
        *self.start.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
        *self.last_update.lock().unwrap_or_else(|e| e.into_inner()) = now;
    }

    fn on_write_progress(&self, bytes_written: u64, bytes_total: u64) {
        if self.out.is_quiet() {
            return;
        }
        let now = Instant::now();
        let mut last = self.last_update.lock().unwrap_or_else(|e| e.into_inner());
        if now.duration_since(*last).as_secs_f64() >= 0.5 {
            if let Some(start) = *self.start.lock().unwrap_or_else(|e| e.into_inner()) {
                print_progress(bytes_written, bytes_total, &start);
            }
            *last = now;
        }
    }
}

fn finalize_mux(
    result: std::io::Result<libfreemkv::MuxOutcome>,
    out: &Output,
    events: &CliMuxEvents,
) -> Result<(), PipeFail> {
    match result {
        Ok(outcome) if outcome.completed => {
            if !events.metadata_sink {
                let start = events.start().unwrap_or_else(Instant::now);
                print_completion_summary(out, outcome.bytes_written, start);
            }
            // AFTER the summary, never before: `print_completion_summary` is what
            // clears the unterminated progress line, so anything printed ahead of
            // it lands on the tail of that line.
            print_lossy_outcome(out, &outcome, &events.dest);
            Ok(())
        }
        // mux completed == false → a mid-run halt (Ctrl-C). Classify as Halted
        // so the multi-title loop FULL-STOPS instead of cancelling each title.
        Ok(_) => Err(PipeFail::halted(interrupted_error(out))),
        Err(e) => Err(PipeFail::from_mux(e)),
    }
}

fn print_lossy_outcome(out: &Output, outcome: &libfreemkv::MuxOutcome, dest: &str) {
    for line in crate::lossy::lossy_lines(outcome, dest) {
        out.raw(crate::output::Level::Always, &line);
    }
}

/// Format an error for display using i18n strings.
///
/// libfreemkv errors render as `E<code>: <data>`. The no-key mux abort
/// (`E7022`, [`libfreemkv::Error::NoDiscKey`]) gets a dedicated message that
/// names the disc by hash; everything else falls through to the generic
/// wrapper.
pub fn fmt_err(e: &dyn std::fmt::Display) -> String {
    let s = e.to_string();
    fmt_err_str(&s)
}

/// `error.scan_failed` with its cause localized like every other error line.
fn scan_failed_msg(e: &dyn std::fmt::Display) -> String {
    strings::fmt("error.scan_failed", &[("detail", &fmt_err(e))])
}

fn fmt_err_str(s: &str) -> String {
    if let Some((code_part, data)) = parse_error_code(s) {
        let key = format!("error.{code_part}");
        // `strings::get` returns the dotted path verbatim on a miss, so a
        // present locale entry is one whose lookup does NOT equal its own key.
        if strings::get(&key) != key {
            // WS2: keep the language-neutral `E<code>` prefix (shown, not
            // stripped). `Error:` is added once at the render site, never
            // here, so this nests as `{cause}`/`{detail}` without doubling it.
            let localized = if code_part == "E7022" {
                // E7022 names the disc by hash; keep its dedicated placeholder.
                strings::fmt(&key, &[("hash", data), ("detail", data)])
            } else if code_part == "E6000" {
                // E6000 (DiscRead) Display is `E6000: <sector> 0x..hex..` — the
                // status/sense hex tail is diagnostic noise that must not reach
                // the user. Pass ONLY the leading sector number as {detail}.
                let sector = data.split_whitespace().next().unwrap_or(data);
                strings::fmt(&key, &[("detail", sector)])
            } else {
                strings::fmt(&key, &[("detail", data)])
            };
            return format!("{code_part} {localized}");
        }
        // A code with NO locale entry still SHOWS its code via the generic
        // wrapper (`{code} {detail}`), so a missing string never swallows the
        // code. The contract test makes this unreachable for any real variant.
        return strings::fmt("error.generic", &[("code", code_part), ("detail", data)]);
    }
    // A non-code string: the generic `{code} {detail}` wrapper with an empty
    // code leaves a leading space, so trim it or it shows as `Error:  msg`.
    strings::fmt("error.generic", &[("code", ""), ("detail", s)])
        .trim_start()
        .to_string()
}

/// Render an error for a user-facing terminal line, with the `Error:` level
/// word prefixed exactly once (WS2 §2.1). Inline render sites print this; the
/// `fatal()` block instead embeds the prefix-free `fmt_err` fragment as
/// `{cause}` inside `error.fatal_header` and adds the level word itself.
pub fn render_error(e: &dyn std::fmt::Display) -> String {
    let level = strings::get(crate::messaging::Level::Error.locale_key());
    format!("{}: {}", level, fmt_err(e))
}

fn check_selection_coverage(
    streams: &freemkv_engine::StreamChoice,
    title: &libfreemkv::DiscTitle,
    title_num: usize,
    multi_title: bool,
    out: &Output,
) -> Result<(), String> {
    let unmatched = streams.unmatched(title);
    if unmatched.is_empty() {
        return Ok(());
    }
    let mut first_error = None;
    for u in &unmatched {
        // One full message per track class, not one with the class
        // interpolated: German/Polish/Russian grammar can't handle a shared
        // template. `available`/`requested` are sanitised: real terminal output.
        let sanitize_all = |v: &[String]| -> String {
            v.iter()
                .map(|s| crate::disc_info::sanitize(s))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let args = [
            ("num", title_num.to_string()),
            ("requested", sanitize_all(&u.requested)),
            ("available", sanitize_all(&u.available)),
        ];
        let args: Vec<(&str, &str)> = args.iter().map(|(k, v)| (*k, v.as_str())).collect();
        if multi_title {
            // A skipped track is important — show it even in quiet mode.
            out.raw(
                crate::output::Level::Always,
                &strings::fmt(&format!("warn.no_lang_match_{}", u.class), &args),
            );
        } else if first_error.is_none() {
            first_error = Some(strings::fmt(
                &format!("error.no_lang_match_{}", u.class),
                &args,
            ));
        }
    }
    match first_error {
        Some(msg) => Err(msg),
        None => Ok(()),
    }
}

/// Render a stream-selection error for the user. An unknown language tag lists
/// the languages actually present on the scanned title, so the user can correct
/// the typo against real data (mirroring what `disc-info` shows). No level word: the
/// title loop's `TitleFailed` render adds it once.
fn render_stream_sel_error(
    e: &freemkv_engine::StreamSelError,
    title: &libfreemkv::DiscTitle,
) -> String {
    match e {
        freemkv_engine::StreamSelError::UnknownLanguage { tag } => {
            let mut langs: Vec<String> = title
                .streams
                .iter()
                // Sanitised: a language tag is raw MPLS/IFO bytes going to a
                // real terminal, and unlike `print_stream_info`, this path
                // didn't sanitise it — `ESC c` fits in 3 bytes.
                .filter_map(|s| match s {
                    libfreemkv::Stream::Audio(a) if !a.language.is_empty() => {
                        Some(crate::disc_info::sanitize(&a.language))
                    }
                    libfreemkv::Stream::Subtitle(s) if !s.language.is_empty() => {
                        Some(crate::disc_info::sanitize(&s.language))
                    }
                    _ => None,
                })
                .collect();
            langs.sort();
            langs.dedup();
            let available = if langs.is_empty() {
                strings::get("error.stream_none")
            } else {
                langs.join(", ")
            };
            strings::fmt(
                "error.unknown_language",
                &[("tag", tag), ("available", &available)],
            )
        }
    }
}

fn parse_error_code(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('E')?;
    // The code is the leading run of digits after 'E'.
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits_end == 0 {
        return None; // "E" not followed by a digit — not a code.
    }
    let code = &s[..digits_end + 1]; // include the leading 'E'
    let after = &s[digits_end + 1..];
    // Data follows a ": " separator; absent for the bare `E<code>` form.
    let data = after.strip_prefix(':').map(|d| d.trim()).unwrap_or("");
    Some((code, data))
}

// ── CLI entry point ─────────────────────────────────────────────────────────

/// Flags parsed from the rip argument list.
#[derive(Default, Debug)]
struct ParsedFlags {
    presentation_language: Option<String>,
    verbose: bool,
    quiet: bool,
    raw: bool,
    multipass: bool,
    /// `--force`: overwrite into a non-empty `dir://` target.
    force: bool,
    keydb_path: Option<String>,
    key_url: Option<String>,
    key_auth: Option<String>,
    title_nums: Vec<usize>,
    /// `-t all`: rip every title. Without it (and without any `-t N`), the
    /// default is the MAIN TITLE only — obfuscated discs with 50+ similar-
    /// length playlists must not rip everything by accident. See the `-t`
    /// normalization in [`run`].
    all_titles: bool,
    /// `-a`/`-s`: which audio + subtitle streams to keep, as one bundle (video
    /// is always kept). Default keeps everything (archival).
    streams: freemkv_engine::StreamChoice,
}

fn parse_stream_spec(spec: &str) -> freemkv_engine::StreamFilter {
    use freemkv_engine::StreamFilter;
    if spec.eq_ignore_ascii_case("all") {
        return StreamFilter::All;
    }
    if spec.eq_ignore_ascii_case("none") {
        return StreamFilter::None;
    }
    let langs: Vec<String> = spec
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    if langs.is_empty() {
        // `-a ,` or `-a ` → treat as keep-all rather than an empty selection.
        StreamFilter::All
    } else {
        StreamFilter::Langs(langs)
    }
}

/// Where the CLI looks up AACS keys for a disc, assembled from the key flags.
///
/// Every rip asks these sources once, up front, through the one resolve (KU §2.1). When
/// both `--keydb` and `--key-url` are given, the keydb is consulted first (local-first),
/// so an offline hit never makes a key-service round-trip. Passing `--key-url` alone
/// bypasses the keydb entirely. See [`key_params`] for the full source-list policy.
#[derive(Default, Debug, Clone)]
pub struct KeyConfig {
    /// `--keydb PATH` — local `keydb.cfg` (else the standard location).
    keydb_path: Option<String>,
    /// `--key-url URL` — remote key-service base URL (enables the online source).
    key_url: Option<String>,
    /// `--key-auth TOKEN` — bearer token sent to the key service (optional).
    key_auth: Option<String>,
}

impl KeyConfig {
    /// The keydb path as an `Option<String>`, for the drive-handshake host-cert
    /// lookup (which always comes from a keydb, independent of the online source).
    fn keydb_path(&self) -> &Option<String> {
        &self.keydb_path
    }
}

fn parse_flags(args: &[String]) -> Result<ParsedFlags, String> {
    let mut f = ParsedFlags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // `--log-level N` sets the tracing level; widens prose detail at
            // level >= 2. VAL-1: reject a bad value with a clean localized
            // error rather than silently leaving the user without a log file.
            "--log-level" => {
                match args.get(i + 1) {
                    // `is_flag_token` lets `-1` through, so an out-of-range
                    // negative still reaches the range error below rather than
                    // being re-reported as a missing value.
                    Some(s) if !is_url_token(s) && !crate::cli_entry::is_flag_token(s) => {
                        i += 1;
                        match s.parse::<u8>() {
                            Ok(n) if n >= 1 => f.verbose = n >= 2,
                            _ => {
                                return Err(strings::fmt(
                                    "error.invalid_log_level",
                                    &[("value", s)],
                                ));
                            }
                        }
                    }
                    // No value or a URL follows: logging init already handles
                    // the bare --log-level case with its own plain-English
                    // diagnostic; nothing to do here.
                    _ => {}
                }
            }
            // `--log-file PATH` is consumed by logging init; swallow its value
            // here so the path isn't mistaken for a positional / unknown flag.
            "--log-file" => {
                // Not a positional URL, and not another flag. Guarding on
                // `is_url_token` alone meant `--log-file --raw` consumed
                // `--raw`, silently dropping it and writing a decrypted image.
                if args
                    .get(i + 1)
                    .is_some_and(|p| !is_url_token(p) && !crate::cli_entry::is_flag_token(p))
                {
                    i += 1;
                }
            }
            "-q" | "--quiet" => f.quiet = true,
            "--raw" => f.raw = true,
            "--multipass" => f.multipass = true,
            "--force" => f.force = true,
            "-t" | "--title" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    // `-t all` — rip every title (the pre-1.6 default; now opt-in).
                    Some(v) if v.eq_ignore_ascii_case("all") => {
                        i += 1;
                        f.all_titles = true;
                    }
                    // Not a positional URL, and not another flag: `-t --force`
                    // used to swallow a flag. `is_flag_token` lets `-1` through
                    // so a negative number still reaches the range check below.
                    Some(v) if !is_url_token(v) && !crate::cli_entry::is_flag_token(v) => {
                        i += 1;
                        match v.parse::<usize>() {
                            Ok(n) if n >= 1 => f.title_nums.push(n),
                            _ => {
                                return Err(strings::fmt("error.invalid_title", &[("value", v)]));
                            }
                        }
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "-t 1")],
                        ));
                    }
                }
            }
            "--presentation-language" => match args.get(i + 1) {
                Some(v) if !is_url_token(v) && !crate::cli_entry::is_flag_token(v) => {
                    i += 1;
                    f.presentation_language = Some(v.clone());
                }
                _ => {
                    return Err(strings::fmt(
                        "error.flag_needs_value",
                        &[
                            ("flag", "--presentation-language"),
                            ("example", "--presentation-language de"),
                        ],
                    ));
                }
            },
            "-a" | "--audio" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    // Not a positional URL, and not another flag: `-a --raw`
                    // used to set the audio spec to "--raw" and swallow the
                    // `--raw`. See `cli_entry::is_flag_token`.
                    Some(v) if !is_url_token(v) && !crate::cli_entry::is_flag_token(v) => {
                        i += 1;
                        f.streams.audio = parse_stream_spec(v);
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "-a eng,spa")],
                        ));
                    }
                }
            }
            "-s" | "--subtitles" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    // Not a positional URL, and not another flag: `-s --raw`
                    // used to set the subtitle spec to "--raw" and swallow the
                    // `--raw`. See `cli_entry::is_flag_token`.
                    Some(v) if !is_url_token(v) && !crate::cli_entry::is_flag_token(v) => {
                        i += 1;
                        f.streams.subtitles = parse_stream_spec(v).into();
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "-s eng")],
                        ));
                    }
                }
            }
            "--keydb" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    // Not a positional URL, and not another flag: `--keydb
                    // --raw` used to set the path to "--raw" and swallow the
                    // `--raw`. See `cli_entry::is_flag_token`.
                    Some(p) if !is_url_token(p) && !crate::cli_entry::is_flag_token(p) => {
                        i += 1;
                        f.keydb_path = Some(p.clone());
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "--keydb keydb.cfg")],
                        ));
                    }
                }
            }
            // `--key-url URL`: a key-service URL IS `https://…`, which
            // `is_url_token` also matches, so require http(s) directly instead
            // of excluding URL tokens. VAL-2: non-http(s) gets its own error.
            "--key-url" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(u) if is_keyserver_url(u) => {
                        i += 1;
                        f.key_url = Some(u.clone());
                    }
                    Some(u) if u.contains("://") && !is_keyserver_url(u) => {
                        // Has a scheme but not http(s) (`ftp://…`, `disc://…`):
                        // give the clear bad-scheme error instead of the
                        // misleading "requires a value" (the old `A && !A` guard).
                        return Err(strings::fmt("error.key_url_bad_scheme", &[("value", u)]));
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[
                                ("flag", flag),
                                ("example", "--key-url https://keys.example/keys"),
                            ],
                        ));
                    }
                }
            }
            // `--key-auth TOKEN` — bearer token for the key service, an opaque
            // string (not a URL). Reject a missing value: a following stream-URL
            // or FLAG (`--key-auth --raw`) means it was omitted; don't swallow it.
            "--key-auth" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(t) if !is_url_token(t) && !crate::cli_entry::is_flag_token(t) => {
                        i += 1;
                        f.key_auth = Some(t.clone());
                    }
                    _ => {
                        return Err(strings::fmt(
                            "error.flag_needs_value",
                            &[("flag", flag), ("example", "--key-auth TOKEN")],
                        ));
                    }
                }
            }
            // An unrecognized dash-prefixed token is a typo (`--titel`,
            // `--qiet`), not something to silently ignore. Bare `-` and
            // non-dash positionals (URLs) are left for the caller to interpret.
            other if other.starts_with('-') && other != "-" => {
                return Err(strings::fmt("error.unknown_flag", &[("flag", &args[i])]));
            }
            _ => {}
        }
        i += 1;
    }
    // Dedup repeated `-t` values: `-t 1 -t 1` is a no-op, not a double rip
    // (two jobs overwriting the same file). Sort for deterministic rip order.
    f.title_nums.sort_unstable();
    f.title_nums.dedup();
    Ok(f)
}

/// The process exit code: 0 on success, `disc_to_iso`'s own
/// `DISC_COPY_DAMAGED_EXIT` for a kept-but-damaged disc→ISO copy, 1 on any
/// other failure.
pub fn run(source: &str, dest: &str, args: &[String]) -> i32 {
    crate::cli_stop::install();

    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(msg) => {
            // Build a quiet-agnostic Output just to emit the error; flag parse
            // errors must surface even before we know verbose/quiet intent.
            Output::new(false, false).raw(Normal, &msg);
            return 1;
        }
    };
    let ParsedFlags {
        presentation_language,
        verbose,
        quiet,
        raw,
        multipass,
        force,
        keydb_path,
        key_url,
        key_auth,
        mut title_nums,
        all_titles,
        streams,
    } = flags;
    // Stream selection is active when the user narrowed either class; the
    // default (All/All) short-circuits to a no-op so the output is byte-
    // identical to no flags.
    let stream_sel_active = !streams.is_all();

    // Whether the user EXPLICITLY narrowed the rip — captured BEFORE the `-t`
    // default below normalizes an empty selection to `[1]`, so a plain rip with
    // no flags reads as "no selection". `-t all` (all_titles) counts as explicit.
    let automatic_title = title_nums.is_empty() && !all_titles;
    let selection_flags_used = stream_sel_active
        || !title_nums.is_empty()
        || all_titles
        || presentation_language.is_some();

    // `-t` DEFAULT (1.6.0): with no `-t N`/`-t all`, rip the MAIN TITLE only.
    // Pre-1.6 the empty case meant all-titles, which on an obfuscated disc
    // (50+ near-equal playlists) turned a 40 GB disc into ~200 GB of duplicates.
    normalize_title_nums(&mut title_nums, all_titles);

    let keys = KeyConfig {
        keydb_path,
        key_url,
        key_auth,
    };

    let parsed_source = libfreemkv::parse_url(source);
    let parsed_dest = libfreemkv::parse_url(dest);

    // When the destination is `stdio://`, stdout IS the ripped byte stream,
    // so every human-facing line must go to stderr, or the banner and
    // progress corrupt the piped output.
    let mut out = Output::new(verbose, quiet);
    if matches!(parsed_dest, libfreemkv::StreamUrl::Stdio) {
        out = out.to_stderr();
    }

    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);

    // Fail loud and EARLY: validate the whole invocation before any drive
    // open, scan, or file creation, so no partial output is ever produced.
    // Each check is small and unit-tested; this is the single entry point.
    if let Err(msg) = preflight_validate(
        source,
        dest,
        &parsed_source,
        &parsed_dest,
        raw,
        multipass,
        force,
        selection_flags_used,
    ) {
        out.raw(Always, &msg);
        return 1;
    }
    // A whole-disc output (an image, a read test, a file tree) is one engine plan: the
    // engine opens the source, acquires the keys, reads and lands the output.
    let plan = cli_plan(source, dest, &keys, (raw, multipass, force));
    if !matches!(plan.output(), freemkv_engine::Output::Titles) {
        return whole_disc(&plan, crate::cli_stop::token(), &out);
    }

    // Everything else: figure out titles, pipe each one
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });
    // A disc is scanned once up front, with no key call: its titles, and the one resolve.
    let mut disc_scan = None;
    if is_disc {
        match scan_rip_drive(source, &keys) {
            Ok(pair) => disc_scan = Some(pair),
            Err(e) if all_titles && !matches!(e, libfreemkv::Error::Halted) => {
                out.raw(Always, &scan_failed_msg(&e));
                out.raw(Always, &disc_scan_all_titles_failed());
                return 1;
            }
            Err(e) => {
                out.raw(Always, &render_drive_open_error(&e));
                return 1;
            }
        }
    }

    // `--multipass`/`--raw` on a non-iso:// dest is already rejected by
    // `preflight_validate`. A disc source skips the upfront `scan_iso` but
    // still honors multiple `-t` flags, building jobs from `title_nums`.
    let iso_disc = if is_disc { None } else { scan_iso(source) };
    if automatic_title {
        let scanned = disc_scan
            .as_ref()
            .map(|(disc, _, _)| disc)
            .or_else(|| iso_disc.as_ref().map(|(disc, _)| disc));
        if let Some(disc) = scanned {
            match automatic_presentation_title(&disc.titles, &streams.audio, presentation_language)
            {
                Ok(index) => title_nums = vec![index + 1],
                Err(reason) => {
                    out.raw(
                        Always,
                        &format!(
                            "Title selection needs review: {reason}; choose --title explicitly."
                        ),
                    );
                    return 1;
                }
            }
        } else if presentation_language.is_some() {
            out.raw(Always, "Presentation-language selection requires a scanned disc; choose --title explicitly.");
            return 1;
        }
    }
    let titles = iso_disc.as_ref().map(|(d, _)| d.titles.clone());
    let is_dir_dest = dest_is_directory(dest, &parsed_dest);

    // Resolve the per-title indices to rip (None means a dir-creation error
    // was printed; abort). `-t all` on a DISC scans once to learn the title
    // count and carries each title's IDENTITY for `pipe_disc`'s re-scan to check.
    let (title_nums, disc_identities) = if let (Some((d, _, _)), true) = (&disc_scan, all_titles) {
        let ids: Vec<TitleIdentity> = d.titles.iter().map(TitleIdentity::of).collect();
        match resolve_disc_all_titles(&title_nums, (!ids.is_empty()).then_some(ids)) {
            Some(pair) => pair,
            // `-t all` without a title list must fail loudly, never rip title 1 and exit 0.
            None => {
                out.raw(Always, &disc_scan_all_titles_failed());
                return 1;
            }
        }
    } else {
        let identities = if automatic_title {
            disc_scan
                .as_ref()
                .map(|(disc, _, _)| disc.titles.iter().map(TitleIdentity::of).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        (title_nums, identities)
    };
    let jobs = match build_jobs(
        &titles,
        is_disc,
        &title_nums,
        is_dir_dest,
        dest,
        &parsed_dest,
        &out,
    ) {
        Some(j) => j,
        None => return 1,
    };

    // Show summary for multi-title
    if let Some(ref t) = titles
        && jobs.len() > 1
    {
        out.raw(
            Normal,
            &strings::fmt(
                "rip.titles_summary",
                &[
                    ("total", &t.len().to_string()),
                    ("selected", &jobs.len().to_string()),
                ],
            ),
        );
        out.blank(Normal);
    }

    // A leftover image staged for an MKV rip only holds the titles it was staged for.
    if let (Some((disc, _)), Some(src)) = (&iso_disc, source_path_of(source))
        && refuse_unstaged_titles(&src, disc, &jobs, &out)
    {
        return 1;
    }

    // KU §2.1 invariant 1: every key this rip reads with, from ONE resolve over its titles,
    // before its first output byte. A stream source (no AACS) has none.
    let rip_titles: Vec<usize> = jobs.iter().map(|(t, _)| t.unwrap_or(0)).collect();
    let set = if let Some((disc, mut reader, _)) = disc_scan {
        let known: Vec<usize> = rip_titles
            .iter()
            .copied()
            .filter(|&t| t < disc.titles.len())
            .collect();
        let scope = libfreemkv::keys::KeyScope::Titles(known);
        let set = disc_rip_keys(&disc, reader.as_mut(), scope, &keys, &out);
        drop(reader);
        match set {
            Some(set) => Some(set),
            None => return 1,
        }
    } else if iso_disc.is_some() {
        let n = titles.as_ref().map_or(0, Vec::len);
        let known = rip_titles.iter().copied().filter(|&t| t < n).collect();
        let scope = libfreemkv::keys::KeyScope::Titles(known);
        match image_rip_keys(source, scope, &keys, &out) {
            Some(opened) => Some(opened.keys),
            None => return 1,
        }
    } else if let (false, libfreemkv::StreamUrl::M2ts { path }) =
        (raw, libfreemkv::parse_url(source))
    {
        // A loose clip's keys come only from its disc folder, walked up to (1.8.0).
        match loose_clip_keys(&path, &keys, &out) {
            Ok(set) => set,
            Err(()) => return 1,
        }
    } else {
        None
    };
    let set = set.unwrap_or_else(libfreemkv::keys::KeyRing::none);

    // A multi-title rip with no specific title named must skip an uncrackable
    // incidental title (menu stub) rather than abort. `-t all` isn't the same
    // as naming titles, so `!all_titles` keeps a stub from aborting it.
    let (multi_title, explicit_selection) = title_policy(jobs.len(), &title_nums, all_titles);

    // The engine owns the per-title loop and its skip / stop policy; the CLI muxes one
    // title and renders what the loop decides.
    let indices: Vec<usize> = jobs.iter().map(|(t, _)| t.unwrap_or(0)).collect();
    let loop_sink = CliTitleLoopSink { out: &out };
    let outcome =
        freemkv_engine::run_titles_with(&indices, explicit_selection, &loop_sink, |idx| {
            let Some((title_idx, dest_url)) = jobs.iter().find(|(t, _)| t.unwrap_or(0) == idx)
            else {
                return Ok(());
            };
            let r = (|| -> Result<(), PipeFail> {
                if let (Some(idx), Some(t)) = (title_idx, &titles) {
                    if !title_in_range(*idx, t.len()) {
                        return Err(PipeFail::fatal(strings::fmt(
                            "rip.warning_title_range",
                            &[
                                ("num", &(idx + 1).to_string()),
                                ("count", &t.len().to_string()),
                            ],
                        )));
                    }
                    let title = &t[*idx];
                    out.raw(
                        Normal,
                        &strings::fmt(
                            "rip.title_info",
                            &[
                                ("num", &(idx + 1).to_string()),
                                ("duration", &title.duration_display()),
                                ("size", &format!("{:.1}", title.size_gb())),
                            ],
                        ),
                    );
                }
                if is_disc {
                    // Disc source: use open_drive() directly — one session, no double init.
                    return pipe_disc(
                        source,
                        dest_url,
                        title_idx.unwrap_or(0),
                        job_identity(&disc_identities, *title_idx),
                        &keys,
                        &set,
                        raw,
                        &streams,
                        multi_title,
                        &out,
                    );
                }
                // Non-disc (ISO): translate the -a/-s language policy into PIDs against THIS
                // scanned title. A typo'd language tag fails the whole rip.
                let selection = match (&titles, title_idx) {
                    (Some(t), Some(idx)) if stream_sel_active => match streams.resolve(&t[*idx]) {
                        Ok(sel) => {
                            check_selection_coverage(
                                &streams,
                                &t[*idx],
                                idx + 1,
                                multi_title,
                                &out,
                            )
                            .map_err(PipeFail::fatal)?;
                            sel
                        }
                        Err(e) => {
                            return Err(PipeFail::fatal(render_stream_sel_error(&e, &t[*idx])));
                        }
                    },
                    _ => libfreemkv::StreamSelection::default(),
                };
                // Every title reads through the rip's one set: no lookup after it (KU §2.1).
                #[allow(clippy::needless_update)]
                let opts = libfreemkv::InputOptions {
                    title_index: *title_idx,
                    raw,
                    selection,
                    keys: Some(set.clone()),
                    ..Default::default()
                };
                pipe(source, dest_url, &opts, &keys, &out)
            })();
            // A failed title's blank follows its notice, which `CliTitleLoopSink` prints.
            if r.is_ok() {
                out.blank(Normal);
            }
            r.map_err(|e| freemkv_engine::TitleError {
                result: e.result,
                error: std::io::Error::other(e.display),
            })
        });
    let ok = matches!(outcome, freemkv_engine::RipOutcome::Ok { .. });
    if ok { 0 } else { 1 }
}

// ── Pre-flight invocation validation (fail loud and EARLY) ──────────────────

fn is_scheme_only_sink(parsed_dest: &libfreemkv::StreamUrl) -> bool {
    matches!(
        parsed_dest,
        libfreemkv::StreamUrl::Null | libfreemkv::StreamUrl::Stdio
    )
}

#[allow(clippy::too_many_arguments)] // cohesive one-shot invocation validator
fn preflight_validate(
    source: &str,
    dest: &str,
    parsed_source: &libfreemkv::StreamUrl,
    parsed_dest: &libfreemkv::StreamUrl,
    raw: bool,
    multipass: bool,
    force: bool,
    selection_flags_used: bool,
) -> Result<(), String> {
    // 1a. Destination must have a recognized scheme. A schemeless dest parses
    // as Unknown — guide the user rather than failing later with a cryptic
    // StreamUrlInvalid or writing `name_t1.unknown`.
    if matches!(parsed_dest, libfreemkv::StreamUrl::Unknown { .. }) {
        return Err(strings::fmt("error.dest_needs_scheme", &[("dest", dest)]));
    }
    // 1b. Source must have a recognized scheme too. A bare path as source would
    // otherwise fall through to a no-titles / cryptic error far downstream.
    if matches!(parsed_source, libfreemkv::StreamUrl::Unknown { .. }) {
        // A container file by its extension (the one CONTAINER_SOURCES table) is pointed at
        // its own scheme; anything else at iso:// / disc://.
        if let Some(scheme) = crate::sources::container_scheme(source) {
            let container = scheme.to_ascii_uppercase();
            return Err(strings::fmt(
                "error.source_needs_container_scheme",
                &[
                    ("source", source),
                    ("scheme", scheme),
                    ("container", &container),
                ],
            ));
        }
        return Err(strings::fmt(
            "error.source_needs_scheme",
            &[("source", source)],
        ));
    }

    // 1c. Title/stream selection (`-t`/`-a`/`-s`) applies only to a scanned
    // source (disc:// or iso://); a stream/file source has no title list or
    // per-stream map to honor the flags against. Fail loud, don't ignore them.
    if selection_flags_used && !parsed_source.is_disc_source() {
        return Err(strings::fmt(
            "error.selection_disc_only",
            &[("source", source)],
        ));
    }

    // 2. `--raw`/`--multipass` need BOTH a drive source and an `iso://` dest;
    // the source half used to be implied by the dest half (iso:// was only
    // reachable from disc://), but any image source can write iso:// now too.
    if !matches!(parsed_dest, libfreemkv::StreamUrl::Iso { .. }) {
        if raw {
            return Err(strings::fmt("error.raw_iso_only", &[("dest", dest)]));
        }
        if multipass {
            return Err(strings::fmt("error.multipass_iso_only", &[("dest", dest)]));
        }
    } else if !matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. }) {
        if raw {
            return Err(strings::fmt("error.raw_disc_only", &[("source", source)]));
        }
        if multipass {
            return Err(strings::fmt(
                "error.multipass_disc_only",
                &[("source", source)],
            ));
        }
    }

    // 2b. `dir://` output needs a filesystem source (disc:// or iso://); a
    // byte-stream source has no UDF tree, so reject it up front. Writability
    // and non-empty are checked in step 4.
    if matches!(parsed_dest, libfreemkv::StreamUrl::Dir { .. }) && !parsed_source.is_disc_source() {
        return Err(strings::fmt(
            "error.dir_source_unsupported",
            &[("source", source)],
        ));
    }

    // 3. Source reachability.
    match parsed_source {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => {
            // An explicitly named device must exist. (Auto-detect — device None —
            // is left to `find_drive`, which has its own "no drive" message.)
            if !p.exists() {
                return Err(strings::fmt(
                    "error.device_not_found",
                    &[("path", &p.display().to_string())],
                ));
            }
        }
        libfreemkv::StreamUrl::Iso { path } => {
            validate_iso_input(path)?;
        }
        // A dir:// SOURCE is checked here for the same reason an iso:// one
        // is: without it a bad folder path printed "Opening dir://...OK" and
        // only then failed with a bare OS error — right exit code, wrong message.
        libfreemkv::StreamUrl::Dir { path } => {
            validate_dir_input(path)?;
        }
        _ => {}
    }

    // 4. The destination must not BE the source: every sink truncates via
    // `File::create` while the source is still open, wiping the only copy
    // in-place. Compared by filesystem identity, before any drive/file open.
    if let Some(dest_path) = url_path_of(parsed_dest)
        && same_file(url_path_of(parsed_source).as_deref(), &dest_path)
    {
        return Err(strings::get("error.dest_is_source"));
    }

    // 5. Destination writability for a single-file output. Directory dests
    // (created on demand by `dir_jobs`) and scheme-only sinks (no fs path)
    // are not pre-checked here.
    match parsed_dest {
        libfreemkv::StreamUrl::Mkv { path }
        | libfreemkv::StreamUrl::Mp4 { path }
        | libfreemkv::StreamUrl::Mpg { path }
        | libfreemkv::StreamUrl::M2ts { path }
        | libfreemkv::StreamUrl::Iso { path } => {
            // A trailing-slash dest (one-file-per-title directory) is validated by
            // dir_jobs, not here.
            if !dest.ends_with('/') {
                validate_file_dest(path)?;
            }
        }
        // `dir://` target: must be creatable + writable, and (unless --force)
        // empty. The producer re-checks these, but surfacing them here gives a
        // clean localized message with zero side effects.
        libfreemkv::StreamUrl::Dir { path } => {
            validate_dir_dest(path, dest, force)?;
        }
        // `demux://` / `video://` / `audio://` / `sub://` write per-track ES files
        // into a directory (created on demand). Same creatable/writable/non-empty
        // gate as `dir://`.
        libfreemkv::StreamUrl::Demux { dir }
        | libfreemkv::StreamUrl::Video { dir }
        | libfreemkv::StreamUrl::Audio { dir }
        | libfreemkv::StreamUrl::Sub { dir } => {
            validate_dir_dest(dir, dest, force)?;
        }
        _ => {}
    }

    Ok(())
}

/// Validate a `dir://` destination: the path must be creatable and writable
/// (it is created if absent), must not be an existing regular file, and —
/// unless `force` — must be empty.
fn validate_dir_dest(path: &std::path::Path, dest: &str, force: bool) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(strings::fmt("error.dir_dest_invalid", &[("dest", dest)]));
    }
    if path.is_file() {
        return Err(strings::fmt(
            "error.dir_dest_is_file",
            &[("path", &path.display().to_string())],
        ));
    }
    // Side-effect-free preflight: don't create the directory here. `dir_jobs`
    // does `create_dir_all` at write time, so creating it here only risks a
    // stray empty dir if a later step fails. A missing dir reads as empty below.
    if !force {
        let non_empty = match std::fs::read_dir(path) {
            Ok(mut it) => it.next().is_some(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            // An unreadable directory can't be shown empty: the guard would pass blind.
            Err(e) => {
                return Err(strings::fmt(
                    "error.dest_not_writable",
                    &[
                        ("path", &path.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                ));
            }
        };
        if non_empty {
            return Err(strings::fmt(
                "error.dir_dest_not_empty",
                &[("path", &path.display().to_string())],
            ));
        }
    }
    Ok(())
}

fn validate_dir_input(path: &std::path::Path) -> Result<(), String> {
    let md = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(strings::fmt(
                "error.dir_not_found",
                &[("path", &path.display().to_string())],
            ));
        }
        Err(e) => {
            return Err(strings::fmt(
                "error.dir_not_readable",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &e.to_string()),
                ],
            ));
        }
    };
    if !md.is_dir() {
        return Err(strings::fmt(
            "error.dir_is_file",
            &[("path", &path.display().to_string())],
        ));
    }
    Ok(())
}

fn validate_iso_input(path: &std::path::Path) -> Result<(), String> {
    let md = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(strings::fmt(
                "error.iso_not_found",
                &[("path", &path.display().to_string())],
            ));
        }
        Err(e) => {
            return Err(strings::fmt(
                "error.iso_not_readable",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &e.to_string()),
                ],
            ));
        }
    };
    if md.is_dir() {
        return Err(strings::fmt(
            "error.iso_is_dir",
            &[("path", &path.display().to_string())],
        ));
    }
    if md.len() == 0 {
        return Err(strings::fmt(
            "error.iso_empty",
            &[("path", &path.display().to_string())],
        ));
    }
    // Readability: opening for read is cheap and catches permission errors that
    // `metadata` (which only needs directory-traverse) would miss.
    if let Err(e) = std::fs::File::open(path) {
        return Err(strings::fmt(
            "error.iso_not_readable",
            &[
                ("path", &path.display().to_string()),
                ("error", &e.to_string()),
            ],
        ));
    }
    Ok(())
}

fn validate_file_dest(path: &std::path::Path) -> Result<(), String> {
    // An existing directory at the file path can't receive a single-file write.
    if path.is_dir() {
        return Err(strings::fmt(
            "error.dest_is_dir_as_file",
            &[("path", &path.display().to_string())],
        ));
    }
    // The parent directory must exist. `parent()` is None for a bare filename
    // (e.g. `out.mkv`) → parent is the current dir, which exists; treat empty
    // parent as "." so a cwd-relative filename is allowed.
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    if !parent.exists() {
        return Err(strings::fmt(
            "error.dest_parent_missing",
            &[("path", &parent.display().to_string())],
        ));
    }
    // Writability probe: try to create (then remove) the target — the honest
    // test for permissions/read-only filesystems. Only probe when the target
    // doesn't exist yet, so we never truncate real prior output during a dry check.
    if path.exists() {
        match std::fs::OpenOptions::new().append(true).open(path) {
            Ok(_) => {}
            Err(e) => {
                return Err(strings::fmt(
                    "error.dest_not_writable",
                    &[
                        ("path", &path.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                ));
            }
        }
    } else {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(_) => {
                // Remove the just-created probe file so the real mux creates it
                // fresh (with its size hint / fallocate). Best-effort cleanup.
                let _ = std::fs::remove_file(path);
            }
            Err(e) => {
                return Err(strings::fmt(
                    "error.dest_not_writable",
                    &[
                        ("path", &path.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn disc_title_nums(all_titles: bool, requested: &[usize], found: usize) -> Vec<usize> {
    if !all_titles || !requested.is_empty() {
        return requested.to_vec();
    }
    (1..=found).collect()
}

fn resolve_disc_all_titles(
    requested: &[usize],
    scan: Option<Vec<TitleIdentity>>,
) -> Option<(Vec<usize>, Vec<TitleIdentity>)> {
    let ids = scan?;
    Some((disc_title_nums(true, requested, ids.len()), ids))
}

fn build_jobs(
    titles: &Option<Vec<libfreemkv::DiscTitle>>,
    is_disc: bool,
    title_nums: &[usize],
    is_dir_dest: bool,
    dest: &str,
    parsed_dest: &libfreemkv::StreamUrl,
    out: &Output,
) -> Option<Vec<(Option<usize>, String)>> {
    // Lay out one file per selected title under a directory destination.
    // `disc_name` seeds the filename stem; falls back to "disc".
    let dir_jobs = |indices: &[usize], disc_name: &str| -> Option<Vec<(Option<usize>, String)>> {
        let ext = parsed_dest.scheme();
        let dest_dir = std::path::Path::new(parsed_dest.path_str());
        // Fail fast with one clear message if the output directory can't be
        // created; swallowing it here makes every per-title `output()` fail
        // later with a cryptic StreamUrlInvalid/IO error instead.
        if let Err(e) = std::fs::create_dir_all(dest_dir) {
            out.raw(
                Always,
                &strings::fmt(
                    "error.cannot_create_dir",
                    &[
                        ("path", &dest_dir.display().to_string()),
                        ("error", &e.to_string()),
                    ],
                ),
            );
            return None;
        }
        Some(
            indices
                .iter()
                .map(|&idx| {
                    let filename = format!("{}_t{}.{}", disc_name, idx + 1, ext);
                    let url = format!("{}://{}", ext, dest_dir.join(filename).display());
                    (Some(idx), url)
                })
                .collect(),
        )
    };

    // Several titles onto a single-file name: one `<stem>_t<N>.<ext>` per title beside it
    // (`-t 3 -t 4 mkv://d/out.mkv` -> `d/out_t3.mkv`, `d/out_t4.mkv`); `<ext>` is the
    // scheme when the name has none.
    let sibling_jobs = |indices: &[usize]| -> Vec<(Option<usize>, String)> {
        let path = std::path::Path::new(parsed_dest.path_str());
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "disc".to_string());
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_else(|| parsed_dest.scheme().to_string());
        let parent = path.parent().unwrap_or(std::path::Path::new(""));
        indices
            .iter()
            .map(|&idx| {
                let file = parent.join(format!("{stem}_t{}.{ext}", idx + 1));
                (
                    Some(idx),
                    format!("{}://{}", parsed_dest.scheme(), file.display()),
                )
            })
            .collect()
    };

    // A scheme-only sink (null://, stdio://) has no filesystem path, so it
    // can never get per-title naming; all titles route to the SAME sink URL.
    // Without this, `dir_jobs` derives an invalid path and `null://` wrongly fails.
    let sink_jobs = |indices: &[usize]| -> Option<Vec<(Option<usize>, String)>> {
        Some(
            indices
                .iter()
                .map(|&idx| (Some(idx), dest.to_string()))
                .collect(),
        )
    };

    // `demux://` is a directory-target sink that fans each title's tracks out
    // to ES files inside it. A single title writes straight into
    // `demux://<dir>/`; multiple get their own `demux://<dir>/t<NN>/` subdir.
    let demux_jobs = |indices: &[usize], base_dir: &str| -> Vec<(Option<usize>, String)> {
        if indices.len() == 1 {
            return vec![(Some(indices[0]), dest.to_string())];
        }
        let trimmed = base_dir.trim_end_matches('/');
        // Re-prefix the ORIGINAL scheme (`demux`/`video`/`audio`/`sub`):
        // `base_dir` has the scheme stripped, so hardcoding `demux://` would
        // drop the kind filter and dump every track for a `video/audio/sub` rip.
        let scheme = parsed_dest.scheme();
        indices
            .iter()
            .map(|&idx| (Some(idx), format!("{scheme}://{trimmed}/t{:02}/", idx + 1)))
            .collect()
    };
    let demux_dir = parsed_dest.path_str().to_string();

    match titles {
        Some(t) if !t.is_empty() => {
            // Scanned source — select which titles.
            let indices: Vec<usize> = if title_nums.is_empty() {
                // ALL-TITLES rip (no `-t`): one job per scanned title.
                (0..t.len()).collect()
            } else {
                title_nums.iter().map(|n| n.saturating_sub(1)).collect()
            };
            if is_scheme_only_sink(parsed_dest) {
                // null:// / stdio:// — every title to the single sink, no naming.
                sink_jobs(&indices)
            } else if matches!(
                parsed_dest,
                libfreemkv::StreamUrl::Demux { .. }
                    | libfreemkv::StreamUrl::Video { .. }
                    | libfreemkv::StreamUrl::Audio { .. }
                    | libfreemkv::StreamUrl::Sub { .. }
            ) {
                // demux:// — directory sink with its own per-track naming.
                Some(demux_jobs(&indices, &demux_dir))
            } else if indices.len() == 1 && !is_dir_dest {
                Some(vec![(Some(indices[0]), dest.to_string())])
            } else if !is_dir_dest {
                Some(sibling_jobs(&indices))
            } else {
                let disc_name = t
                    .first()
                    .and_then(|ti| {
                        if ti.playlist.is_empty() {
                            None
                        } else {
                            Some(sanitize_name(&ti.playlist))
                        }
                    })
                    .unwrap_or_else(|| "disc".to_string());
                dir_jobs(&indices, &disc_name)
            }
        }
        _ if is_disc && title_nums.len() > 1 => {
            // Disc source, multiple titles requested. pipe_disc scans per title;
            // one job per requested title.
            let indices: Vec<usize> = title_nums.iter().map(|n| n.saturating_sub(1)).collect();
            if is_scheme_only_sink(parsed_dest) {
                // null:// / stdio:// — every requested title to the single sink.
                return sink_jobs(&indices);
            }
            if matches!(
                parsed_dest,
                libfreemkv::StreamUrl::Demux { .. }
                    | libfreemkv::StreamUrl::Video { .. }
                    | libfreemkv::StreamUrl::Audio { .. }
                    | libfreemkv::StreamUrl::Sub { .. }
            ) {
                // demux:// — directory sink with its own per-track naming.
                return Some(demux_jobs(&indices, &demux_dir));
            }
            if !is_dir_dest {
                return Some(sibling_jobs(&indices));
            }
            dir_jobs(&indices, "disc")
        }
        _ if is_disc
            && is_dir_dest
            && title_nums.len() == 1
            && !is_scheme_only_sink(parsed_dest)
            && !matches!(
                parsed_dest,
                libfreemkv::StreamUrl::Demux { .. }
                    | libfreemkv::StreamUrl::Video { .. }
                    | libfreemkv::StreamUrl::Audio { .. }
                    | libfreemkv::StreamUrl::Sub { .. }
            ) =>
        {
            // One title into a directory is still named per title, as for a scanned source.
            dir_jobs(&[title_nums[0].saturating_sub(1)], "disc")
        }
        _ => {
            // No title list, single pass (disc all-titles, single -t, or a
            // streaming source). `-t 0` was rejected during flag parsing, but
            // saturating_sub guards a stray 0 from underflowing to usize::MAX.
            let idx = title_nums.first().map(|n| n.saturating_sub(1));
            Some(vec![(idx, dest.to_string())])
        }
    }
}

// ── The pipeline engine ─────────────────────────────────────────────────────

fn keyless_scan_opts() -> libfreemkv::ScanOptions {
    libfreemkv::ScanOptions::default()
}

fn dest_is_directory(dest: &str, parsed_dest: &libfreemkv::StreamUrl) -> bool {
    dest.ends_with('/') || std::path::Path::new(parsed_dest.path_str()).is_dir()
}

pub(crate) fn drive_credentials(
    keydb_path: &Option<String>,
) -> Option<libfreemkv::DriveCredentials> {
    let path = resolved_keydb_path(keydb_path);
    let host_certs = freemkv_keysources::KeydbSource::new(path).host_certs();
    (!host_certs.is_empty()).then_some(libfreemkv::DriveCredentials { host_certs })
}

/// `freemkv info -v`'s key status (KU §2.5: "`freemkv info` … `Titles([main])` for status"):
/// one resolve against the local keydb, its walk shown; `None` when it refused.
pub(crate) fn resolve_info_keys(
    drive: &mut libfreemkv::Drive,
    disc: &libfreemkv::Disc,
    keydb_path: &Option<String>,
    out: &Output,
) -> Option<libfreemkv::keys::KeySetStatus> {
    let keys = KeyConfig {
        keydb_path: keydb_path.clone(),
        key_url: None,
        key_auth: None,
    };
    let main = freemkv_engine::resolve_selection(disc, &freemkv_engine::Selection::MainMovie);
    let scope = libfreemkv::keys::KeyScope::Titles(main);
    let factory = key_source_factory(&keys, out);
    let (set, trace) = crate::rip_keys::resolve(disc, drive, scope, &factory, None, None);
    emit_resolution_trace(out, &trace);
    set.ok().map(|s| s.status())
}

fn scan_iso(source: &str) -> Option<(libfreemkv::Disc, Box<dyn libfreemkv::SectorSource>)> {
    // `dir://` is an image-level source too: `scan_dir` synthesizes a UDF
    // volume and returns the same pair `scan_iso` does. Without this arm,
    // opening a source "as an image" silently rejected folders.
    match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Iso { path } => {
            libfreemkv::scan_iso(std::path::Path::new(&path), keyless_scan_opts())
        }
        libfreemkv::StreamUrl::Dir { path } => {
            libfreemkv::scan_dir(std::path::Path::new(&path), keyless_scan_opts())
        }
        _ => return None,
    }
    // The engine's own open reports a source it cannot read; the scan's reason goes to the log.
    .map_err(|e| tracing::warn!("keyless scan of {source} failed: {e}"))
    .ok()
}

pub(crate) fn resolved_keydb_path(keydb_path: &Option<String>) -> std::path::PathBuf {
    keydb_path
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(freemkv_keysources::existing_keydb_path)
        .or_else(freemkv_keysources::default_keydb_path)
        .unwrap_or_else(|| std::path::PathBuf::from("keydb.cfg"))
}

#[cfg(test)]
fn build_key_sources_quiet(keys: &KeyConfig) -> Vec<Box<dyn freemkv_keysources::KeySource>> {
    freemkv_engine::key_sources(&key_params(keys))
}

fn key_params(keys: &KeyConfig) -> freemkv_engine::KeyParams {
    crate::plan_core::key_params(&key_settings(keys)).params()
}

/// The CLI's key flags as the front-end-neutral settings: `--key-url` with no `--keydb`
/// asks only the key service; otherwise the keydb (the explicit one, or the default
/// path) and then the service. The default keydb path also supplies the drive
/// handshake's host certificates.
pub(crate) fn key_settings(keys: &KeyConfig) -> crate::plan_core::KeySettings {
    let flags = crate::plan_core::CliKeys {
        keydb: keys.keydb_path.clone(),
        key_url: keys.key_url.clone(),
        key_auth: keys.key_auth.clone(),
    };
    let default = resolved_keydb_path(&keys.keydb_path);
    crate::plan_core::cli_key_settings(&flags, &default.to_string_lossy())
}

/// Print the SSRF-rejected-`--key-url` warning (once) if the configured key URL
/// fails validation — matching the pre-hoist `build_key_sources` behaviour.
fn warn_ssrf_rejected(keys: &KeyConfig, out: &Output) {
    if let Some(url) = &keys.key_url
        && let Err(e) = freemkv_keysources::validate_keyserver_url(url)
    {
        out.raw(
            Always,
            &strings::fmt("error.keyserver_url_rejected", &[("error", &e)]),
        );
    }
}

fn key_source_factory(keys: &KeyConfig, out: &Output) -> libfreemkv::KeySourceFactory {
    warn_ssrf_rejected(keys, out);
    crate::rip_keys::sources(&key_params(keys))
}

/// Render the AACS resolution trace to STDERR (never stdout — that may carry the
/// piped disc stream), suppressed when quiet. English lives here in the app
/// layer; the library trace is typed enums only.
fn emit_resolution_trace(out: &Output, trace: &libfreemkv::aacs::trace::ResolutionTrace) {
    if !out.is_quiet() {
        for line in render_resolution_trace(trace) {
            eprintln!("{line}");
        }
    }
}

use crate::rip_keys::render_trace as render_resolution_trace;

/// `-t all` could not list the disc's titles, so nothing was ripped.
fn disc_scan_all_titles_failed() -> String {
    strings::get_or(
        "error.disc_scan_all_titles_failed",
        "Error: could not scan the disc to list its titles, so `-t all` \
         cannot know which titles to rip. Nothing was ripped — check the \
         disc and drive and try again.",
    )
}

/// A drive that would not open or scan: "no drive" for autodetect, else the coded error.
fn render_drive_open_error(e: &libfreemkv::Error) -> String {
    match e {
        libfreemkv::Error::DeviceNotFound { path } if path.is_empty() => {
            strings::get("error.no_drive")
        }
        libfreemkv::Error::Halted => strings::get("rip.interrupted"),
        _ => render_error(e),
    }
}

/// The `DeviceTarget` a `disc://` URL names: a device path, or autodetect.
fn device_target(source: &str) -> libfreemkv::DeviceTarget {
    match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => libfreemkv::DeviceTarget::Path(p),
        _ => libfreemkv::DeviceTarget::Autodetect,
    }
}

// Every CLI drive scan runs under the process Ctrl-C token (stop design v5 §4.3 item 1).
fn open_drive_scan(
    target: libfreemkv::DeviceTarget,
    credentials: Option<libfreemkv::DriveCredentials>,
    raw: bool,
) -> Result<libfreemkv::DiscSession, libfreemkv::Error> {
    let progress = libfreemkv::halt::Liveness::new();
    freemkv_engine::open_scan_with(
        target,
        credentials,
        raw,
        crate::cli_stop::token(),
        &progress,
    )
}

// The failure of a title's drive reopen. A Ctrl-C there is a full stop, not a failed title.
fn reopen_failure(e: &libfreemkv::Error) -> PipeFail {
    match e {
        libfreemkv::Error::Halted => PipeFail::halted(strings::get("rip.interrupted")),
        libfreemkv::Error::DeviceNotFound { path } if path.is_empty() => {
            PipeFail::fatal(strings::get("error.no_drive"))
        }
        _ => PipeFail::fatal(format!("{e}")),
    }
}

type DriveScan = (libfreemkv::Disc, Box<dyn libfreemkv::SectorSource>, String);

/// Open and scan the drive at `source` with NO key call (KU §3.2 `open_scan`): the disc,
/// its raw reader for the rip's one resolve, and the device path.
fn scan_rip_drive(source: &str, keys: &KeyConfig) -> Result<DriveScan, libfreemkv::Error> {
    let credentials = drive_credentials(keys.keydb_path());
    let mut session = open_drive_scan(device_target(source), credentials, false)?;
    let device = session.device_path().to_string();
    let disc = session.take_disc().ok_or(libfreemkv::Error::NoStreams)?;
    session.stage_drive_as_reader();
    let reader = session.take_reader().ok_or(libfreemkv::Error::NoStreams)?;
    Ok((disc, reader, device))
}

/// Print a key refusal, or the set's HD DVD note; the set on success. E7034 renders its
/// shared text (USER 2026-09-28: no `--vid-from`; the disc is inserted and the rip re-run).
fn report_keys(
    set: libfreemkv::Result<libfreemkv::keys::KeyRing>,
    out: &Output,
) -> Option<libfreemkv::keys::KeyRing> {
    match set {
        Ok(set) => {
            if let Some(note) = crate::rip_keys::best_effort_note(&set.status()) {
                out.raw(Normal, &note);
            }
            Some(set)
        }
        // A Ctrl-C during the resolve is an interrupt, not an error (KU §2.3 step 13).
        Err(libfreemkv::Error::Halted) => {
            out.raw(Normal, &strings::get("rip.interrupted"));
            None
        }
        Err(e) => {
            out.raw(Always, &render_error(&e));
            None
        }
    }
}

/// The rip's one resolve over a drive, before any output (KU §2.3); Ctrl-C stops it.
fn disc_rip_keys(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: libfreemkv::keys::KeyScope,
    keys: &KeyConfig,
    out: &Output,
) -> Option<libfreemkv::keys::KeyRing> {
    let factory = key_source_factory(keys, out);
    let token = crate::cli_stop::token();
    let r = crate::rip_keys::resolve(disc, reader, scope, &factory, None, Some(token));
    emit_resolution_trace(out, &r.1);
    report_keys(r.0, out)
}

/// A loose clip's keys, resolved once from its disc folder; `Ok(None)` when it has none.
/// `Err` once the refusal is shown.
fn loose_clip_keys(
    clip: &std::path::Path,
    keys: &KeyConfig,
    out: &Output,
) -> Result<Option<libfreemkv::keys::KeyRing>, ()> {
    let factory = key_source_factory(keys, out);
    let token = crate::cli_stop::token();
    let (set, trace) = freemkv_engine::resolve_loose_clip(clip, &factory, Some(token));
    emit_resolution_trace(out, &trace);
    match set {
        Ok(None) => Ok(None),
        Ok(Some(set)) => report_keys(Ok(set), out).map(Some).ok_or(()),
        Err(e) => {
            report_keys(Err(e), out);
            Err(())
        }
    }
}

/// The keys `info` opens a loose clip with: looked up from its disc folder as a rip does
/// (1.8.0), in `keydb` when given, else the standard location. `Err` once the refusal is shown.
pub(crate) fn info_clip_keys(
    source: &str,
    keydb: Option<String>,
) -> Result<Option<libfreemkv::keys::KeyRing>, ()> {
    match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::M2ts { path } => {
            let keys = KeyConfig {
                keydb_path: keydb,
                ..KeyConfig::default()
            };
            loose_clip_keys(&path, &keys, &Output::new(false, false))
        }
        _ => Ok(None),
    }
}

/// Open an image rip's source and resolve its keys once (KU §3.2); `halt` stops the
/// resolve. `None` once the refusal is shown.
fn open_rip_image(
    source: &str,
    scope: libfreemkv::keys::KeyScope,
    keys: &KeyConfig,
    halt: &libfreemkv::Halt,
    out: &Output,
) -> Option<freemkv_engine::OpenedImage> {
    let src = freemkv_engine::ImageSource::from_url(source)?;
    let o = crate::rip_keys::ImageOpen {
        scope,
        seed: None,
        drive_disc: None,
        halt: Some(halt.clone()),
    };
    let (opened, trace) = crate::rip_keys::open_image(&src, key_source_factory(keys, out), o);
    emit_resolution_trace(out, &trace);
    let opened = match opened {
        Ok(o) => o,
        Err(e) => {
            report_keys(Err(e), out);
            return None;
        }
    };
    report_keys(Ok(opened.keys.clone()), out)?;
    Some(opened)
}

/// [`open_rip_image`] under the process Ctrl-C token.
fn image_rip_keys(
    source: &str,
    scope: libfreemkv::keys::KeyScope,
    keys: &KeyConfig,
    out: &Output,
) -> Option<freemkv_engine::OpenedImage> {
    open_rip_image(source, scope, keys, crate::cli_stop::token(), out)
}

#[allow(clippy::too_many_arguments)] // cohesive single-title disc rip
fn pipe_disc(
    source: &str,
    dest: &str,
    title_idx: usize,
    expected: Option<&TitleIdentity>,
    keys: &KeyConfig,
    set: &libfreemkv::keys::KeyRing,
    raw: bool,
    streams: &freemkv_engine::StreamChoice,
    multi_title: bool,
    out: &Output,
) -> Result<(), PipeFail> {
    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    // KU §3.3: each title reopens the drive with `open_scan` (no key call) and reads
    // through the rip's one set, after `is_for`. Tray unlock is guaranteed by `Drive::drop`.
    let credentials = drive_credentials(keys.keydb_path());
    let mut session = open_drive_scan(device_target(source), credentials, false)
        .map_err(|e| reopen_failure(&e))?;
    // ── Pre-flight validation (borrows the scanned disc; no drive I/O) ──
    let batch = libfreemkv::disc::detect_max_batch_sectors(session.device_path());
    // Resolved -a/-s PID selection for this title (default = keep all).
    let mut selection = libfreemkv::StreamSelection::default();
    {
        let disc = session
            .disc()
            .ok_or_else(|| PipeFail::from_typed(libfreemkv::Error::NoStreams))?;
        // The same range + identity rules from `run()`'s scanned-source path; range
        // prevents a panic on `disc.titles[title_idx]`, identity catches a diverging re-scan.
        let title =
            resolve_scanned_title(&disc.titles, title_idx, expected).map_err(PipeFail::fatal)?;
        crate::rip_keys::check_reopened(set, disc).map_err(PipeFail::from_typed)?;

        // Translate the -a/-s language policy into PIDs against this scanned
        // title. A bad tag is a hard error (typo). Default All/All is a no-op.
        if !streams.is_all() {
            selection = streams
                .resolve(title)
                .map_err(|e| PipeFail::fatal(render_stream_sel_error(&e, title)))?;
            // A requested language absent from this title: error (single) or
            // warn+keep-video (batch) — never silently ship a track-less file.
            check_selection_coverage(streams, title, title_idx + 1, multi_title, out)
                .map_err(PipeFail::fatal)?;
        }

        // KU §3.5: the one decrypt gate, from the set (AACS) or the disc (CSS).
        let scope = libfreemkv::keys::KeyScope::Titles(vec![title_idx]);
        crate::rip_keys::gate(disc, raw, Some(set), &scope).map_err(PipeFail::from_typed)?;
    }

    // The live drive is opened and the source is ready — complete the source
    // "opening…" line started above.
    out.raw(Normal, &strings::get("rip.ok"));

    // The engine muxes the title off the held drive through the rip's set: it stages the
    // drive, builds the stream, pumps headers, opens the sink and pumps frames.
    let metadata_sink = is_metadata_sink(dest);
    let events = Arc::new(CliMuxEvents::new(*out, dest.to_string(), metadata_sink));
    let plan = title_plan(source, dest, title_idx, raw, keys, streams);
    let with = freemkv_engine::RunWith {
        keys: Some(set.clone()),
        held: Some(freemkv_engine::Held::Session(&mut session)),
        title: freemkv_engine::TitleOptions {
            selection: Some(selection),
            skip_errors: false,
            batch_sectors: batch,
        },
        ..cli_title_run(events.clone())
    };
    finalize_mux(run_title(&plan, with), out, &events)
}

// A CLI title run's context: the process Ctrl-C token as its halt and the CLI's renderer
// as its event listener.
fn cli_title_run(events: Arc<CliMuxEvents>) -> freemkv_engine::RunWith<'static> {
    freemkv_engine::RunWith {
        halt: Some(crate::cli_stop::token().clone()),
        events: Some(events),
        ..freemkv_engine::RunWith::default()
    }
}

// The engine plan for one title of `source` into `dest`.
fn title_plan(
    source: &str,
    dest: &str,
    title_idx: usize,
    raw: bool,
    keys: &KeyConfig,
    streams: &freemkv_engine::StreamChoice,
) -> freemkv_engine::Plan {
    crate::plan_core::plan(crate::plan_core::PlanRequest {
        source: source.to_string(),
        dest: dest.to_string(),
        titles: freemkv_engine::Selection::Titles(vec![title_idx]),
        streams: streams.clone(),
        raw,
        multipass: false,
        keys: key_settings(keys),
        force: false,
    })
}

// Run a title plan; the mux's outcome, or the error it failed with as the library reports it.
fn run_title(
    plan: &freemkv_engine::Plan,
    with: freemkv_engine::RunWith<'_>,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    match freemkv_engine::run_with(plan, with, &freemkv_engine::NoopSink) {
        Ok(freemkv_engine::Report::Title { outcome }) => Ok(outcome),
        Ok(_) => Err(libfreemkv::Error::StreamUrlInvalid {
            url: plan.dest.clone(),
        }
        .into()),
        Err(e) => Err(e.into()),
    }
}

fn is_metadata_sink(dest: &str) -> bool {
    matches!(
        libfreemkv::parse_url(dest),
        libfreemkv::StreamUrl::Chapters { .. } | libfreemkv::StreamUrl::Json { .. }
    )
}

fn disc_copy_recovered_data(bytes_good: u64) -> bool {
    bytes_good > 0
}

/// What a finished `disc:// → iso://` copy actually produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyVerdict {
    /// Ctrl-C landed mid-sweep. The ISO on disk is a prefix of the disc; the
    /// mapfile is preserved so a later run can resume.
    Interrupted,
    /// The sweep ran to the end and recovered ZERO readable bytes — the ISO is
    /// all zeroes.
    NoData,
    /// The sweep finished and produced a usable image that is NOT the whole
    /// disc: sectors were unreadable, or were skipped and left pending. Still
    /// KEPT and reported with a warning naming the loss and pointing at another
    /// run, but a scripted caller must be able to tell it apart from a clean
    /// copy — see [`DISC_COPY_DAMAGED_EXIT`].
    Lossy,
    /// A usable image, and all of it.
    Complete,
}

/// The `disc:// -> iso://` exit code for a `Lossy` copy: kept and reported, but
/// short some sectors. Distinct from both 0 (clean) and 1 (no usable image /
/// interrupted), so `$?` can tell "damaged but usable" apart from either.
const DISC_COPY_DAMAGED_EXIT: i32 = 3;

// NOTHING readable (or an interrupted sweep) is the only hard failure (exit 1).
// `Lossy` is its own exit code — still kept, still reported — never silently
// folded into either a clean 0 or a hard 1.
fn disc_copy_exit_code(verdict: CopyVerdict) -> i32 {
    match verdict {
        CopyVerdict::Complete => 0,
        CopyVerdict::Lossy => DISC_COPY_DAMAGED_EXIT,
        CopyVerdict::NoData | CopyVerdict::Interrupted => 1,
    }
}

fn copy_verdict(r: &freemkv_engine::CopyResult) -> CopyVerdict {
    if r.halted {
        CopyVerdict::Interrupted
    } else if !disc_copy_recovered_data(r.bytes_good) {
        CopyVerdict::NoData
    } else if !r.complete {
        // The engine's own verdict: bytes unreadable or still pending (attempted and skipped;
        // a single-pass sweep has no later pass to fetch them).
        CopyVerdict::Lossy
    } else {
        CopyVerdict::Complete
    }
}

/// Print the interrupt notice and return the error string both pipe paths use
/// when a SIGINT lands mid-mux. The message names the output as incomplete so
/// the user knows not to trust it.
fn interrupted_error(out: &Output) -> String {
    out.blank(Normal);
    out.raw(Normal, &strings::get("error.interrupted_incomplete"));
    strings::get("rip.interrupted")
}

fn pipe(
    source: &str,
    dest: &str,
    opts: &libfreemkv::InputOptions,
    keys: &KeyConfig,
    out: &Output,
) -> Result<(), PipeFail> {
    // Source open, header pump/gate, sink open, metadata short-circuit, frame pump and
    // NoStreams guard all live in the engine's title run. The CLI keeps only presentation,
    // via `CliMuxEvents` (stream-info, progress bar).
    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    out.raw(Normal, &strings::get("rip.ok"));

    let metadata_sink = is_metadata_sink(dest);
    let events = Arc::new(CliMuxEvents::new(*out, dest.to_string(), metadata_sink));
    let streams = freemkv_engine::StreamChoice::default();
    let plan = title_plan(
        source,
        dest,
        opts.title_index.unwrap_or(0),
        opts.raw,
        keys,
        &streams,
    );
    let with = freemkv_engine::RunWith {
        keys: opts.keys.clone(),
        title: freemkv_engine::TitleOptions {
            selection: Some(opts.selection.clone()),
            skip_errors: false,
            // Unused by a URL source (its image highway owns batching).
            batch_sectors: 0,
        },
        ..cli_title_run(events.clone())
    };
    finalize_mux(run_title(&plan, with), out, &events)
}

// ── Disc → ISO (raw sector copy, not a stream) ────────────────────────────

fn url_path_of(url: &libfreemkv::StreamUrl) -> Option<std::path::PathBuf> {
    use libfreemkv::StreamUrl as U;
    match url {
        U::Mkv { path }
        | U::M2ts { path }
        | U::Mp4 { path }
        | U::Mpg { path }
        | U::Iso { path }
        | U::Dir { path }
        | U::Fvi { path }
        | U::Chapters { path }
        | U::Json { path } => Some(path.clone()),
        U::Demux { dir } | U::Video { dir } | U::Audio { dir } | U::Sub { dir } => {
            Some(dir.clone())
        }
        // No filesystem path to compare: a live drive, a socket, stdio, the
        // bit bucket, and a URL we could not parse at all (rejected earlier).
        U::Disc { .. } | U::Network { .. } | U::Stdio | U::Null | U::Unknown { .. } => None,
    }
}

/// The filesystem path behind a source URL, if it has one.
///
/// Split out so the same-file guard is unit-testable without a real image.
fn source_path_of(source: &str) -> Option<std::path::PathBuf> {
    url_path_of(&libfreemkv::parse_url(source))
}

/// Whether two paths name the same existing file — [`crate::file_identity`]
/// owns the answer, and owns it for both shells. It lived here, and the GUI
/// grew a narrower copy of it that a hardlink walks straight through.
use crate::file_identity::same_file;

/// A staged image's never-read sectors are zeros: refuse (E6022) any job whose title
/// extents its scope does not hold. `None` jobs mux title 0. Same engine check as the GUI's.
fn refuse_unstaged_titles(
    image: &std::path::Path,
    disc: &libfreemkv::Disc,
    jobs: &[(Option<usize>, String)],
    out: &Output,
) -> bool {
    let titles: Vec<usize> = jobs.iter().map(|(t, _)| t.unwrap_or(0)).collect();
    match freemkv_engine::ensure_titles_staged(image, disc, &titles) {
        Ok(()) => false,
        Err(e) => {
            out.raw(Always, &render_error(&e));
            true
        }
    }
}

/// What an interrupted disc→ISO copy prints. §2.5: the image and its lock are "Kept after
/// Stop … where it guards the resumable artifact", so a rerun resumes.
fn interrupted_text(iso: &std::path::Path) -> String {
    let stopped = strings::get("rip.interrupted");
    match iso.exists() {
        true => format!(
            "{stopped}\n{}",
            strings::get_or("stop.progress_kept", "Progress kept")
        ),
        false => stopped,
    }
}

// ── Whole-disc outputs (iso://, null://, dir://): one engine plan ───────────

/// The engine plan for this invocation. Every front end builds the same [`freemkv_engine::Plan`]
/// from its own inputs and hands it to `freemkv_engine::run` (the engine owns the open, the
/// keys, the read policy and the landing).
pub(crate) fn cli_plan(
    source: &str,
    dest: &str,
    keys: &KeyConfig,
    flags: (bool, bool, bool),
) -> freemkv_engine::Plan {
    crate::plan_core::plan(crate::plan_core::cli_request(
        source,
        dest,
        flags,
        key_settings(keys),
    ))
}

// The CLI's rendering of the plan it runs (`--log-level 2`). Exhaustive on purpose (anti-drift
// §2): a field added to the engine's `Plan` fails to compile here until the CLI handles it.
fn plan_line(p: &freemkv_engine::Plan) -> String {
    let freemkv_engine::Plan {
        source,
        dest,
        titles,
        streams,
        raw,
        multipass,
        keys,
        force,
    } = p;
    let freemkv_engine::KeyParamsData {
        keydb_path,
        key_url,
        key_auth,
        online_only,
        cert_keydb,
    } = keys;
    format!(
        "plan: {source} -> {dest} titles={titles:?} streams_all={} raw={raw} multipass={multipass} \
         force={force} keydb={} online={} auth={} online_only={online_only} certs={}",
        streams.is_all(),
        keydb_path.is_some(),
        key_url.is_some(),
        key_auth.is_some(),
        cert_keydb.is_some(),
    )
}

// Renders a whole-disc run: the drive, the key walk, the disc label and output, the pass
// progress line. `halt` (the process Ctrl-C token) stops it.
struct WholeDiscSink<'a> {
    out: &'a Output,
    halt: &'a libfreemkv::Halt,
    output: freemkv_engine::Output,
    opened: std::sync::atomic::AtomicBool,
    label: Mutex<Option<(String, u64)>>,
    last: Mutex<(
        Option<std::time::Instant>,
        Option<libfreemkv::progress::PassProgress>,
    )>,
    drew: std::sync::atomic::AtomicBool,
}

impl freemkv_engine::Sink for WholeDiscSink<'_> {
    fn should_cancel(&self) -> bool {
        self.halt.is_cancelled() || !copy_should_continue(INTERRUPTED.load(Ordering::SeqCst))
    }

    fn event(&self, e: &freemkv_engine::Event<'_>) {
        use freemkv_engine::Event as E;
        match e {
            E::SourceOpened { device, disc } => {
                self.opened.store(true, Ordering::SeqCst);
                if let Some(device) = device {
                    self.out
                        .raw(Normal, &strings::fmt("rip.drive", &[("device", device)]));
                }
                let name = sanitize_name(disc.meta_title.as_deref().unwrap_or(&disc.volume_id));
                let bytes = disc.capacity_sectors as u64 * libfreemkv::consts::SECTOR_BYTES_U64;
                *self.label.lock().unwrap_or_else(|e| e.into_inner()) = Some((name, bytes));
            }
            E::Keys { trace, ring } => {
                emit_resolution_trace(self.out, trace);
                if let Some(note) =
                    ring.and_then(|r| crate::rip_keys::best_effort_note(&r.status()))
                {
                    self.out.raw(Normal, &note);
                }
            }
            E::Phase { name: "copy" } => self.copy_header(),
            E::Phase { name: "extract" } => {
                if let freemkv_engine::Output::Tree { path } = &self.output {
                    self.out.raw(
                        Normal,
                        &strings::fmt("dir.extracting", &[("path", &path.display().to_string())]),
                    );
                    self.out.blank(Normal);
                }
            }
            E::Pass(p) => {
                self.last.lock().unwrap_or_else(|e| e.into_inner()).1 = Some((*p).clone());
            }
            _ => {}
        }
    }

    fn progress(&self, p: &freemkv_engine::Progress) {
        if self.out.is_quiet() {
            return;
        }
        let now = std::time::Instant::now();
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last
            .0
            .is_some_and(|t| now.duration_since(t).as_secs_f64() < 0.5)
        {
            return;
        }
        if let Some(pass) = last.1.clone() {
            last.0 = Some(now);
            self.drew.store(true, Ordering::SeqCst);
            print_disc_progress(&pass, p.speed_bps, p.eta_secs);
        }
    }
}

impl WholeDiscSink<'_> {
    // The disc label, its size and the output, before the copy's first read.
    fn copy_header(&self) {
        let label = self.label.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((name, bytes)) = label {
            self.out.raw(
                Normal,
                &strings::fmt(
                    "rip.disc_label",
                    &[
                        ("name", &name),
                        ("size", &format!("{:.1}", bytes as f64 / 1_073_741_824.0)),
                    ],
                ),
            );
        }
        if let freemkv_engine::Output::Image { path, null: false } = &self.output {
            self.out.raw(
                Normal,
                &strings::fmt("rip.output", &[("path", &path.display().to_string())]),
            );
        }
        self.out.blank(Normal);
    }
}

/// Run a whole-disc `plan` (an image, a read test or a file tree) and render it. The exit
/// code: 0 on a complete copy, `DISC_COPY_DAMAGED_EXIT` on an image that is kept and usable
/// but short some sectors, 1 on any other failure.
pub(crate) fn whole_disc(
    plan: &freemkv_engine::Plan,
    halt: &libfreemkv::Halt,
    out: &Output,
) -> i32 {
    let output = plan.output();
    // Never write over the source: the copy creates its destination before the first read.
    if let freemkv_engine::Output::Image { path, null: false } = &output
        && same_file(source_path_of(&plan.source).as_deref(), path)
    {
        out.raw(Always, &strings::get("error.dest_is_source"));
        return 1;
    }
    out.raw(crate::output::Level::Verbose, &plan_line(plan));
    let sink = WholeDiscSink {
        out,
        halt,
        output: output.clone(),
        opened: std::sync::atomic::AtomicBool::new(false),
        label: Mutex::new(None),
        last: Mutex::new((None, None)),
        drew: std::sync::atomic::AtomicBool::new(false),
    };
    let start = std::time::Instant::now();
    let sources = crate::rip_keys::sources(&plan.keys.params());
    let with = freemkv_engine::RunWith {
        sources: Some(sources),
        ..freemkv_engine::RunWith::default()
    };
    let r = freemkv_engine::run_with(plan, with, &sink);
    if sink.drew.load(Ordering::SeqCst) && !out.is_quiet() {
        eprint!("\r\x1b[K");
    }
    let target = match &output {
        freemkv_engine::Output::Image { path, .. } | freemkv_engine::Output::Tree { path } => {
            path.clone()
        }
        freemkv_engine::Output::Titles => std::path::PathBuf::new(),
    };
    match r {
        Ok(freemkv_engine::Report::Image {
            copy, disc, path, ..
        }) => render_copy(&copy, disc.as_deref(), &path, start, out),
        Ok(freemkv_engine::Report::Tree { extract }) => match render_extract(&extract, out) {
            true => 0,
            false => 1,
        },
        // A whole-disc plan never reports a title.
        Ok(freemkv_engine::Report::Title { .. }) => 1,
        Err(libfreemkv::Error::Halted) => {
            out.raw(Normal, &halted_text(&output, &target));
            1
        }
        Err(e) => {
            let text = match crate::artifact_lock::lock_failed(&e, &target) {
                Some(text) => text,
                None if !sink.opened.load(Ordering::SeqCst)
                    && matches!(
                        libfreemkv::parse_url(&plan.source),
                        libfreemkv::StreamUrl::Disc { .. }
                    ) =>
                {
                    render_drive_open_error(&e)
                }
                None => render_error(&e),
            };
            out.raw(Always, &text);
            1
        }
    }
}

// What a halted whole-disc run prints. Only an image resumes; a tree does not keep progress.
fn halted_text(output: &freemkv_engine::Output, target: &std::path::Path) -> String {
    match output {
        freemkv_engine::Output::Image { .. } => interrupted_text(target),
        _ => strings::get("rip.interrupted"),
    }
}

// A finished image copy's verdict, as the CLI has always printed it.
fn render_copy(
    r: &freemkv_engine::CopyResult,
    disc: Option<&libfreemkv::Disc>,
    iso_path: &std::path::Path,
    start: std::time::Instant,
    out: &Output,
) -> i32 {
    match copy_verdict(r) {
        // Ctrl-C halted the copy. Don't print "Complete" over a partial ISO — report
        // interrupted/failure so exit is non-zero. The mapfile is preserved for a resume.
        CopyVerdict::Interrupted => {
            out.raw(Normal, &interrupted_text(iso_path));
            1
        }
        // The copy completed but recovered ZERO readable bytes: the ISO on disk is
        // unusable (but kept). A scripted caller checking $? must see non-zero.
        CopyVerdict::NoData => {
            out.raw(
                Always,
                &crate::disc_copy_verdict::iso_no_data_error(r.bytes_unreadable),
            );
            1
        }
        verdict => {
            let elapsed = start.elapsed().as_secs_f64();
            let mb = r.bytes_total as f64 / (1024.0 * 1024.0);
            let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
            // Report the LOSS whenever there is any, printed BEFORE so "Complete" is last.
            // `Lossy` is still a SUCCESS — the image is kept and usable.
            if verdict == CopyVerdict::Lossy {
                let gb_good = r.bytes_good as f64 / 1_073_741_824.0;
                let mb_bad = r.bytes_unreadable as f64 / 1_048_576.0;
                let mb_pending = r.bytes_pending as f64 / 1_048_576.0;
                let mapfile_path = disc.map_or_else(
                    || freemkv_engine::mapfile_path_for(iso_path),
                    |d| d.mapfile_for(iso_path),
                );
                let main_title = disc.and_then(|d| d.titles.first());
                let main_title_bad = main_title
                    .map(|t| freemkv_engine::bytes_bad_in_title_from_mapfile(&mapfile_path, t))
                    .unwrap_or(0);
                // Damage as a main-title duration only.
                let main_lost_secs = main_title
                    .map(|t| (t.size_bytes, t.duration_secs))
                    .filter(|&(sz, dur)| main_title_bad > 0 && sz > 0 && dur > 0.0)
                    .map(|(sz, dur)| main_title_bad as f64 / sz as f64 * dur)
                    .unwrap_or(0.0);
                out.raw(
                    Always,
                    &strings::fmt(
                        "rip.mapfile_summary",
                        &[
                            ("good", &format!("{gb_good:.2}")),
                            ("unreadable", &format!("{mb_bad:.1}")),
                            ("pending", &format!("{mb_pending:.1}")),
                        ],
                    ),
                );
                if main_lost_secs > 0.0 {
                    let main_str = fmt_damage_time(main_lost_secs);
                    out.raw(
                        Always,
                        &strings::fmt("rip.damage_lost_movie", &[("time", &main_str)]),
                    );
                }
                // Always shown, even after `--multipass`: residual damage is never hidden.
                out.raw(
                    Always,
                    &crate::disc_copy_verdict::retry_with_multipass_hint(),
                );
            }
            out.raw(
                Normal,
                &strings::fmt(
                    "rip.complete",
                    &[
                        ("size", &format!("{:.1}", mb / 1024.0)),
                        ("unit", "GB"),
                        ("time", &format!("{elapsed:.0}")),
                        ("speed", &format!("{speed:.0}")),
                    ],
                ),
            );
            // `Lossy` gets its own DISTINCT exit code: kept and usable, loss already named.
            disc_copy_exit_code(verdict)
        }
    }
}

fn extract_succeeded(halted: bool, complete: bool) -> bool {
    !halted && complete
}

// A finished extraction: per-file loss, then the summary. `false` for a stopped or holed
// tree, so a script can re-run via an iso:// multipass.
fn render_extract(res: &libfreemkv::ExtractResult, out: &Output) -> bool {
    // Per-file loss lines (only the lossy ones, to keep output terse).
    for f in &res.files {
        let lost = f.bytes_unreadable;
        if lost > 0 {
            out.raw(
                Always,
                &strings::fmt(
                    "dir.file_lossy",
                    &[
                        ("file", &sanitize(&f.path.display().to_string())),
                        ("lost", &format!("{:.2}", lost as f64 / 1_048_576.0)),
                    ],
                ),
            );
        }
    }
    if res.halted {
        out.raw(Normal, &strings::get("rip.interrupted"));
        return extract_succeeded(res.halted, res.complete);
    }
    if res.complete {
        let good_mb = res.bytes_good as f64 / 1_048_576.0;
        out.raw(
            Normal,
            &strings::fmt(
                "dir.complete",
                &[
                    ("files", &res.files.len().to_string()),
                    ("size", &format!("{good_mb:.1}")),
                ],
            ),
        );
    } else {
        let lost_mb = res.bytes_lost() as f64 / 1_048_576.0;
        out.raw(
            Always,
            &strings::fmt(
                "dir.lossy",
                &[
                    ("files", &res.files.len().to_string()),
                    ("lost", &format!("{lost_mb:.2}")),
                ],
            ),
        );
    }
    extract_succeeded(res.halted, res.complete)
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn fmt_speed(mbps: f64) -> String {
    if mbps >= 1.0 {
        format!("{:.1} MB/s", mbps)
    } else if mbps * 1024.0 >= 1.0 {
        format!("{:.0} KB/s", mbps * 1024.0)
    } else if mbps > 0.0 {
        format!("{:.0} B/s", mbps * 1_048_576.0)
    } else {
        "stalled".into()
    }
}

fn fmt_eta(secs: f64) -> String {
    if secs <= 0.0 || secs.is_infinite() {
        return "?:??".into();
    }
    let h = secs as u64 / 3600;
    let m = (secs as u64 % 3600) / 60;
    let s = secs as u64 % 60;
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

fn fmt_damage_time(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else if secs >= 1.0 {
        format!("{:.0}s", secs)
    } else if secs >= 0.01 {
        format!("{:.2}s", secs)
    } else {
        format!("{:.0}ms", secs * 1000.0)
    }
}

fn fmt_disc_damage(p: &libfreemkv::progress::PassProgress) -> String {
    let bytes_disc = p.bytes_total_disc;
    if bytes_disc == 0 {
        return strings::get("rip.damage_none");
    }
    let bytes_failed = p
        .bytes_unreadable_total
        .saturating_add(p.bytes_retryable_total);
    let disc_damage_secs = if bytes_failed > 0 {
        p.disc_duration_secs
            .filter(|&d| d > 0.0)
            .map(|dur| bytes_failed as f64 / bytes_disc as f64 * dur)
            .unwrap_or(0.0)
    } else {
        0.0
    };
    let title_damage_secs = if p.bytes_bad_in_main_title > 0 {
        p.main_title_duration_secs
            .zip(p.main_title_size_bytes)
            .filter(|&(dur, sz)| dur > 0.0 && sz > 0)
            .map(|(dur, sz)| p.bytes_bad_in_main_title as f64 / sz as f64 * dur)
    } else {
        None
    };

    if bytes_failed > 0 {
        let disc_str = fmt_damage_time(disc_damage_secs);
        match title_damage_secs {
            Some(ms) if ms > 0.0 && ms < disc_damage_secs * 0.99 => strings::fmt(
                "rip.damage_lost",
                &[("time", &disc_str), ("movie_time", &fmt_damage_time(ms))],
            ),
            Some(_) | None => strings::fmt("rip.damage_lost_movie", &[("time", &disc_str)]),
        }
    } else {
        strings::get("rip.damage_none")
    }
}

fn print_disc_progress(
    p: &libfreemkv::progress::PassProgress,
    speed_bps: u64,
    eta_secs: Option<u64>,
) {
    if let Some(line) = disc_progress_line(p, speed_bps, eta_secs) {
        eprint!("{line}");
        let _ = std::io::stderr().flush();
    }
}

// The repainted progress line; `None` while the disc size is unknown.
fn disc_progress_line(
    p: &libfreemkv::progress::PassProgress,
    speed_bps: u64,
    eta_secs: Option<u64>,
) -> Option<String> {
    // Speed + ETA are the ENGINE's derivation (freemkv_engine::SpeedEstimator);
    // the CLI only formats them. bps → MB/s for display.
    let inst_speed_mbps = speed_bps as f64 / 1_048_576.0;
    let bytes_disc = p.bytes_total_disc;
    if bytes_disc == 0 {
        return None;
    }
    // For Patch modes (Trim/Scrape), show work_done/work_total percentage.
    // bytes_good_total doesn't advance until sectors are recovered, leaving
    // progress stuck at 0% even though patch is working through bad ranges.
    let gb_done = match p.kind {
        libfreemkv::progress::PassKind::Sweep | libfreemkv::progress::PassKind::Mux => {
            p.work_done as f64 / 1_073_741_824.0
        }
        libfreemkv::progress::PassKind::Trim { .. }
        | libfreemkv::progress::PassKind::Scrape { .. } => {
            // Show progress through bad ranges, not just recovered data
            let pct = p.work_pct();
            (pct / 100.0) * (bytes_disc as f64 / 1_073_741_824.0)
        }
        _ => p.bytes_good_total as f64 / 1_073_741_824.0,
    };
    let gb_total = bytes_disc as f64 / 1_073_741_824.0;
    // `work_pct()` guards `work_total == 0` (returns 100.0) so an empty pass
    // can't produce a `NaN%`. Patch modes (Trim/Scrape) show progress through
    // bad ranges; Sweep/Mux show work_done/work_total — same formula either way.
    let pct = p.work_pct();
    // ETA comes from the engine estimator (seconds), not re-derived here.
    let eta = match eta_secs {
        Some(s) => fmt_eta(s as f64),
        None => "?:??".into(),
    };
    let damage = fmt_disc_damage(p);
    Some(format!(
        "\r  {:.1}/{:.1} GB ({:.1}%)  {}  ETA {}    {}    ",
        gb_done,
        gb_total,
        pct,
        fmt_speed(inst_speed_mbps),
        eta,
        damage,
    ))
}

fn print_progress(done: u64, total: u64, start: &std::time::Instant) {
    let elapsed = start.elapsed().as_secs_f64();
    if elapsed <= 0.0 {
        return;
    }
    let mb_done = done as f64 / 1_048_576.0;
    let avg = mb_done / elapsed;

    if total > 0 {
        let pct = (done as f64 / total as f64 * 100.0).min(100.0);
        let mb_total = total as f64 / 1_048_576.0;
        let eta = if avg > 0.0 {
            // `done` can exceed `total` (container overhead vs source-reported
            // size); saturate so the remaining-bytes math never underflows.
            let s = total.saturating_sub(done) as f64 / 1_048_576.0 / avg;
            format!("{}:{:02}", s as u64 / 60, s as u64 % 60)
        } else {
            "?:??".into()
        };
        if mb_total >= 1024.0 {
            eprint!(
                "\r  {:.1} GB / {:.1} GB  ({:.1}%)  {:.1} MB/s  ETA {}    ",
                mb_done / 1024.0,
                mb_total / 1024.0,
                pct,
                avg,
                eta
            );
        } else {
            eprint!(
                "\r  {:.0} MB / {:.0} MB  ({:.1}%)  {:.1} MB/s  ETA {}    ",
                mb_done, mb_total, pct, avg, eta
            );
        }
    } else {
        eprint!("\r  {:.1} MB  {:.1} MB/s    ", mb_done, avg);
    }
    let _ = std::io::stderr().flush();
}

fn print_completion_summary(out: &Output, done: u64, start: std::time::Instant) {
    if !out.is_quiet() {
        eprint!("\r\x1b[K");
    }
    let elapsed = start.elapsed().as_secs_f64();
    let mb = done as f64 / (1024.0 * 1024.0);
    let (sz, unit) = if mb >= 1024.0 {
        (mb / 1024.0, "GB")
    } else {
        (mb, "MB")
    };
    let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
    out.raw(
        Normal,
        &strings::fmt(
            "rip.complete",
            &[
                ("size", &format!("{sz:.1}")),
                ("unit", unit),
                ("time", &format!("{elapsed:.0}")),
                ("speed", &format!("{speed:.0}")),
            ],
        ),
    );
}

fn print_stream_info(out: &Output, meta: &libfreemkv::DiscTitle) {
    out.raw(
        Normal,
        &format!("  {}: {}", strings::get("disc.streams"), meta.streams.len()),
    );
    for s in &meta.streams {
        match s {
            libfreemkv::Stream::Video(v) => {
                let label = if v.label.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", sanitize(&v.label))
                };
                out.raw(
                    Normal,
                    &format!("    {} {}{}", v.codec, v.resolution, label),
                );
            }
            libfreemkv::Stream::Audio(a) => {
                let mut tags: Vec<String> = Vec::new();
                if let Some(key) = audio_purpose_key(a.purpose) {
                    tags.push(strings::get(key));
                }
                if a.secondary {
                    tags.push(strings::get("stream.secondary"));
                }
                if !a.label.is_empty() {
                    tags.push(sanitize(&a.label));
                }
                let label = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", tags.join(", "))
                };
                out.raw(
                    Normal,
                    &format!(
                        "    {} {} {}{}",
                        a.codec,
                        a.channels,
                        sanitize(&a.language),
                        label
                    ),
                );
            }
            libfreemkv::Stream::Subtitle(s) => {
                out.raw(
                    Normal,
                    &format!("    {} {}", s.codec, sanitize(&s.language)),
                );
            }
        }
    }
    if meta.duration_secs > 0.0 {
        let d = meta.duration_secs;
        out.raw(
            Normal,
            &format!(
                "  {}: {}:{:02}:{:02}",
                strings::get("disc.duration"),
                d as u64 / 3600,
                (d as u64 % 3600) / 60,
                d as u64 % 60
            ),
        );
    }
}

/// The pre-mux excluded-track note (G4): what the destination container cannot carry,
/// from the same `lossy::excluded_lines` the GUI log shows. Silent when nothing is left out.
fn print_excluded(out: &Output, dest: &str, title: &libfreemkv::DiscTitle) {
    for line in crate::lossy::excluded_lines(dest, title) {
        out.raw(Normal, &line);
    }
}

fn is_keyserver_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Whether the copy keeps going at a progress report: it stops the moment SIGINT was
/// seen, so the first Ctrl-C stops the copy cleanly (the tray unlocks on drop).
fn copy_should_continue(interrupted: bool) -> bool {
    !interrupted
}

fn title_in_range(idx: usize, count: usize) -> bool {
    idx < count
}

use crate::title_identity::TitleIdentity;

fn title_changed_message(num: usize, expected: &TitleIdentity, found: &TitleIdentity) -> String {
    const KEY: &str = "error.title_changed";
    let args = [
        ("num", num.to_string()),
        ("expected", expected.describe()),
        ("found", found.describe()),
    ];
    crate::strings::fmt_or(
        KEY,
        "Title {num} changed between scans: expected {expected}, the drive now \
         reports {found}. The disc list moved under the rip; nothing was \
         written for this title.",
        &args
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect::<Vec<_>>(),
    )
}

fn job_identity(identities: &[TitleIdentity], title_idx: Option<usize>) -> Option<&TitleIdentity> {
    identities.get(title_idx.unwrap_or(0))
}

fn resolve_scanned_title<'a>(
    titles: &'a [libfreemkv::DiscTitle],
    title_idx: usize,
    expected: Option<&TitleIdentity>,
) -> Result<&'a libfreemkv::DiscTitle, String> {
    if !title_in_range(title_idx, titles.len()) {
        return Err(strings::fmt(
            "error.title_out_of_range",
            &[
                ("num", &(title_idx + 1).to_string()),
                ("count", &titles.len().to_string()),
            ],
        ));
    }
    let title = &titles[title_idx];
    if let Some(expected) = expected {
        let found = TitleIdentity::of(title);
        if found != *expected {
            return Err(title_changed_message(title_idx + 1, expected, &found));
        }
    }
    Ok(title)
}

fn automatic_presentation_title(
    titles: &[libfreemkv::DiscTitle],
    audio: &freemkv_engine::StreamFilter,
    presentation_language: Option<String>,
) -> Result<usize, String> {
    let report = freemkv_engine::SelectionModel::from_titles(titles).select_with_preferences(
        &freemkv_engine::Selection::MainMovie,
        audio,
        &freemkv_engine::SelectionPreferences {
            presentation_language,
        },
    );
    if let Some(reason) = report.review_reason {
        return Err(reason.key().into());
    }
    report
        .indices
        .first()
        .copied()
        .ok_or_else(|| "no-titles".into())
}

fn normalize_title_nums(title_nums: &mut Vec<usize>, all_titles: bool) {
    if title_nums.is_empty() && !all_titles {
        title_nums.push(1);
    }
}

fn title_policy(job_count: usize, title_nums: &[usize], all_titles: bool) -> (bool, bool) {
    (job_count > 1, !title_nums.is_empty() && !all_titles)
}

fn sanitize_name(name: &str) -> String {
    let s = name
        .replace(
            |c: char| !c.is_ascii_alphanumeric() && c != ' ' && c != '-' && c != '_',
            "",
        )
        .trim()
        .replace(' ', "_");
    if s.is_empty() { "disc".to_string() } else { s }
}

/// Map `LabelPurpose` to its locale string key. `Normal` → no tag.
fn audio_purpose_key(p: libfreemkv::LabelPurpose) -> Option<&'static str> {
    match p {
        libfreemkv::LabelPurpose::Commentary => Some("stream.purpose.commentary"),
        libfreemkv::LabelPurpose::Descriptive => Some("stream.purpose.descriptive"),
        libfreemkv::LabelPurpose::Score => Some("stream.purpose.score"),
        libfreemkv::LabelPurpose::Ime => Some("stream.purpose.ime"),
        libfreemkv::LabelPurpose::Normal => None,
    }
}

#[cfg(test)]
#[path = "pipe_staged_image_tests.rs"]
mod staged_image_tests;

#[cfg(test)]
#[path = "pipe_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pipe_verdict_tests.rs"]
mod verdict_tests;

#[cfg(test)]
#[path = "pipe_build_jobs_edge_tests.rs"]
mod build_jobs_edge_tests;

// ── The image-decrypt destination must not be the source ─────────────────────
// `write_image` truncates via `File::create` before the first read, so
// `iso://Disc.iso iso://Disc.iso` zeroed the still-open input; the CLI lacked this guard.
#[cfg(test)]
#[path = "pipe_dest_is_source_tests.rs"]
mod dest_is_source_tests;

// ── Disc language tags reach a real terminal ─────────────────────────────────
// A language tag is raw MPLS/IFO bytes; unlike `print_stream_info`, these
// error renderers didn't sanitise it — `ESC c` (a full reset) fits in 3 bytes.
#[cfg(test)]
#[path = "pipe_language_escape_tests.rs"]
mod language_escape_tests;

// ── A title's POSITION is not its identity across an independent re-scan ──────
// `-t all` runs N+1 scans sharing only an integer index; a reordered later
// scan can silently mux the wrong title. Drives `resolve_scanned_title` directly.
#[cfg(test)]
#[path = "pipe_title_identity_tests.rs"]
mod title_identity_tests;

// ── Pure progress/stream formatters and the CLI mux-event callbacks ──────────
// Each is reachable in production only behind a real drive or disc image, so
// CI never exercised their branches; tested here as the strings they emit.
#[cfg(test)]
#[path = "pipe_formatter_tests.rs"]
mod formatter_tests;

#[cfg(test)]
#[path = "pipe_ku_cli_tests.rs"]
mod ku_cli_tests;
