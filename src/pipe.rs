//! Pipe — stream in, stream out.
//!
//! One pipeline for everything:
//!   1. disc→ISO: Disc::copy() (not a stream)
//!   2. Everything else: input → PES → output, one title at a time
//!
//! Batch (multiple titles) is just a for loop calling pipe() per title.

use crate::disc_info::sanitize;
use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::{MuxEvents, MuxInput, MuxOptions};
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn install_signal_handler() {
    #[cfg(unix)]
    unsafe {
        // sigaction, not signal(): on musl, signal() is one-shot and would
        // kill the double-Ctrl-C _exit(130) guard after the first fire.
        // SA_RESTART (no SA_RESETHAND) fixes that and restarts syscalls.
        let mut sa: libc::sigaction = std::mem::zeroed();
        // Cast through a thin pointer: a bare `fn as usize` is a double
        // coercion that clippy 1.97 rejects.
        sa.sa_sigaction = handle_sigint as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        // On failure, degrade gracefully: the handler simply isn't installed.
        let _ = libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }

    #[cfg(windows)]
    unsafe {
        extern "system" fn handler(_: u32) -> i32 {
            INTERRUPTED.store(true, Ordering::SeqCst);
            1
        }
        unsafe extern "system" {
            fn SetConsoleCtrlHandler(
                handler: unsafe extern "system" fn(u32) -> i32,
                add: i32,
            ) -> i32;
        }
        SetConsoleCtrlHandler(handler, 1);
    }
}

#[cfg(unix)]
extern "C" fn handle_sigint(_sig: libc::c_int) {
    if INTERRUPTED.load(Ordering::SeqCst) {
        unsafe { libc::_exit(130) };
    }
    INTERRUPTED.store(true, Ordering::SeqCst);
}

// See docs/pipe.md#SigintHalt — Bridge the process-wide SIGINT flag ([`INTERRUPTED`]) into a…
struct SigintHalt {
    halt: libfreemkv::Halt,
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl SigintHalt {
    fn install() -> Self {
        let halt = libfreemkv::Halt::new();
        let done = Arc::new(AtomicBool::new(false));
        // A SIGINT that already landed (before the mux starts) cancels up front.
        if INTERRUPTED.load(Ordering::SeqCst) {
            halt.cancel();
        }
        let handle = {
            let halt = halt.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    if INTERRUPTED.load(Ordering::SeqCst) {
                        halt.cancel();
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            })
        };
        SigintHalt {
            halt,
            done,
            handle: Some(handle),
        }
    }

    fn halt(&self) -> &libfreemkv::Halt {
        &self.halt
    }
}

impl Drop for SigintHalt {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// See docs/pipe.md#PipeFail — A per-title mux failure carrying both the display…
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

    // See docs/pipe.md#from_typed — A typed library error (a preflight decrypt gate).…
    fn from_typed(e: libfreemkv::Error) -> Self {
        let display = e.to_string();
        let io: std::io::Error = e.into();
        PipeFail {
            result: freemkv_engine::classify_title_error(&io),
            display,
        }
    }

    /// A failure surfaced by `mux_stream`. Classifies the typed `io::Error` via
    /// the engine (kills the CLI E-code string-match) and renders its `E<code>`
    /// Display for the user.
    fn from_mux(e: std::io::Error) -> Self {
        PipeFail {
            result: freemkv_engine::classify_title_error(&e),
            display: format!("{e}"),
        }
    }
}

// See docs/pipe.md#CliMuxEvents — The CLI's [`MuxEvents`] implementation: it renders exactly what…
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
        *self.start.lock().expect("start mutex")
    }
}

impl MuxEvents for CliMuxEvents {
    fn on_output_opened(&self, title: &libfreemkv::DiscTitle) {
        // The stream-info block and mp4-fit warnings, from the resolved title.
        print_stream_info(&self.out, title);
        print_mp4_skips(&self.out, &self.dest, title);
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
        *self.start.lock().expect("start mutex") = Some(now);
        *self.last_update.lock().expect("last_update mutex") = now;
    }

    fn on_write_progress(&self, bytes_written: u64, bytes_total: u64) {
        if self.out.is_quiet() {
            return;
        }
        let now = Instant::now();
        let mut last = self.last_update.lock().expect("last_update mutex");
        if now.duration_since(*last).as_secs_f64() >= 0.5 {
            if let Some(start) = *self.start.lock().expect("start mutex") {
                print_progress(bytes_written, bytes_total, &start);
            }
            *last = now;
        }
    }
}

// See docs/pipe.md#finalize_mux — Render the outcome of a `mux_stream` run into…
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

// See docs/pipe.md#print_lossy_outcome — Print everything a COMPLETED mux still has to…
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

// See docs/pipe.md#fmt_err_str — Render a libfreemkv `E<code>[: <data>]` Display string (or…
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

// See docs/pipe.md#check_selection_coverage — Handle the `-a`/`-s` "no matching stream" case for…
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
            first_error = Some(render_error(&strings::fmt(
                &format!("error.no_lang_match_{}", u.class),
                &args,
            )));
        }
    }
    match first_error {
        Some(msg) => Err(msg),
        None => Ok(()),
    }
}

/// Render a stream-selection error for the user. An unknown language tag lists
/// the languages actually present on the scanned title, so the user can correct
/// the typo against real data (mirroring what `disc-info` shows).
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
            render_error(&strings::fmt(
                "error.unknown_language",
                &[("tag", tag), ("available", &available)],
            ))
        }
    }
}

// See docs/pipe.md#parse_error_code — Parse a libfreemkv Display string of the form…
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

// See docs/pipe.md#parse_stream_spec — Parse an `-a`/`-s` value into a [`freemkv_engine::StreamFilter`]
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
/// libfreemkv does no lookup — the CLI resolves a [`libfreemkv::Key`] from these
/// sources and hands it to `Disc::decrypt_with`. When both `--keydb` and
/// `--key-url` are given, the keydb is consulted first (local-first), so an
/// offline hit never makes a key-service round-trip. Passing `--key-url` alone
/// bypasses the keydb entirely. See [`build_key_sources_quiet`] for the full
/// source-list policy.
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

// See docs/pipe.md#parse_flags — Parse rip flags, returning a clear error string…
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
            // `--key-auth TOKEN` — bearer token for the key service. A token is an
            // opaque string, not a URL; reject only a missing value (a following
            // stream-URL token means the token was omitted).
            "--key-auth" => {
                let flag = &args[i];
                match args.get(i + 1) {
                    Some(t) if !is_url_token(t) => {
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

/// Returns true on success, false on error.
pub fn run(source: &str, dest: &str, args: &[String]) -> bool {
    install_signal_handler();

    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(msg) => {
            // Build a quiet-agnostic Output just to emit the error; flag parse
            // errors must surface even before we know verbose/quiet intent.
            Output::new(false, false).raw(Normal, &msg);
            return false;
        }
    };
    let ParsedFlags {
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
    let selection_flags_used = stream_sel_active || !title_nums.is_empty() || all_titles;

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
        out.raw(Normal, &msg);
        return false;
    }

    // Disc → ISO or Disc → null: use Disc::copy() (not a stream)
    if matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. })
        && matches!(
            parsed_dest,
            libfreemkv::StreamUrl::Iso { .. } | libfreemkv::StreamUrl::Null
        )
    {
        return disc_to_iso(source, dest, &keys, raw, multipass, &out);
    }

    // Any OTHER image source → iso://: the generic image sink. `iso:// → iso://`
    // decrypts an existing image; `dir://` joins this arm when it becomes a
    // source. Not the recovery path — see `image_to_iso`.
    if matches!(parsed_dest, libfreemkv::StreamUrl::Iso { .. }) && parsed_source.is_disc_source() {
        return image_to_iso(source, dest, &keys, &out);
    }

    // Disc / ISO → dir://: decrypted file-tree extraction, not a stream.
    // Placed before the generic mux path. Byte-stream sources, `--raw`, and
    // `--multipass` are already rejected above, so the source here is a disc.
    if matches!(parsed_dest, libfreemkv::StreamUrl::Dir { .. }) {
        return dir_to_extract(source, dest, &keys, &parsed_source, force, &out);
    }

    // Everything else: figure out titles, pipe each one
    // For disc with explicit -t, skip the upfront ISO scan (pipe_disc scans itself)
    let is_disc = matches!(parsed_source, libfreemkv::StreamUrl::Disc { .. });

    // `--multipass`/`--raw` on a non-iso:// dest is already rejected by
    // `preflight_validate`. A disc source skips the upfront `scan_iso` but
    // still honors multiple `-t` flags, building jobs from `title_nums`.
    let iso_disc = if is_disc { None } else { scan_iso(source) };
    let titles = iso_disc.as_ref().map(|(d, _)| d.titles.clone());
    let is_dir_dest = dest_is_directory(dest, &parsed_dest);

    // Resolve the per-title indices to rip (None means a dir-creation error
    // was printed; abort). `-t all` on a DISC scans once to learn the title
    // count and carries each title's IDENTITY for `pipe_disc`'s re-scan to check.
    let (title_nums, disc_identities) = if is_disc && all_titles {
        match resolve_disc_all_titles(&title_nums, disc_title_identities(source, &keys, &out)) {
            Some(pair) => pair,
            // The upfront scan FAILED, and `-t all` cannot proceed without it.
            // The old code left `title_nums` empty, so `pipe_disc` ripped
            // title 1 and exited 0, unaware `-t all` was requested. Fail LOUDLY.
            None => {
                out.raw(
                    Normal,
                    &strings::get_or(
                        "error.disc_scan_all_titles_failed",
                        "Error: could not scan the disc to list its titles, so `-t all` \
                         cannot know which titles to rip. Nothing was ripped — check the \
                         disc and drive and try again.",
                    ),
                );
                return false;
            }
        }
    } else {
        (title_nums, Vec::new())
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
        None => return false,
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

    // Pipe each title
    let mut ok = true;

    // For an ISO source, resolve the AACS unit keys ONCE (keyless scan → local
    // keydb → decrypt_with) and hand them to each title's stream — libfreemkv
    // does no lookup. A disc source resolves per-title inside `pipe_disc`.
    let iso_unit_keys = match iso_disc {
        Some((disc, reader)) => resolve_iso_unit_keys(disc, reader, &keys, &out),
        None => Vec::new(),
    };

    // Fresh-key-on-failure factory for the ISO mux: when an online key service
    // is configured, a unit no upfront key decrypts is re-tried against it.
    // `None` (no `--key-url`) keeps the prior behaviour.
    let iso_key_fetch = if is_disc {
        None
    } else {
        build_iso_key_fetch(source, &keys)
    };

    // A multi-title rip with no specific title named must skip an uncrackable
    // incidental title (menu stub) rather than abort. `-t all` isn't the same
    // as naming titles, so `!all_titles` keeps a stub from aborting it.
    let (multi_title, explicit_selection) = title_policy(jobs.len(), &title_nums, all_titles);

    for (title_idx, dest_url) in &jobs {
        // The MAIN FEATURE is title index 0 (the disc's primary title — first in
        // every title list throughout the codebase). A failure there is always a
        // hard error, even in an all-titles rip: the user wants the movie.
        let is_feature = is_feature_title(*title_idx);
        // Print title info if we have it
        if let (Some(idx), Some(t)) = (title_idx, &titles) {
            if !title_in_range(*idx, t.len()) {
                eprintln!(
                    "{}",
                    strings::fmt(
                        "rip.warning_title_range",
                        &[
                            ("num", &(idx + 1).to_string()),
                            ("count", &t.len().to_string()),
                        ]
                    )
                );
                // An explicitly-requested out-of-range title is a hard failure,
                // not a warning-and-carry-on, or the CLI would exit 0 despite
                // ripping nothing for the requested title.
                ok = false;
                continue;
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

        let result = if is_disc {
            // Disc source: use open_drive() directly — one session, no double init.
            pipe_disc(
                source,
                dest_url,
                title_idx.unwrap_or(0),
                job_identity(&disc_identities, *title_idx),
                &keys,
                raw,
                multipass,
                &streams,
                multi_title,
                &out,
            )
        } else {
            // Non-disc (ISO): translate the -a/-s language policy into PIDs
            // against THIS scanned title. A typo'd language tag fails the
            // whole rip, since it would fail every title identically.
            let selection = match (&titles, title_idx) {
                (Some(t), Some(idx)) if stream_sel_active => match streams.resolve(&t[*idx]) {
                    Ok(sel) => {
                        // A requested language that's simply absent from this
                        // title: error (single) or warn+keep-video (batch).
                        if let Err(msg) =
                            check_selection_coverage(&streams, &t[*idx], idx + 1, multi_title, &out)
                        {
                            out.raw(Normal, &msg);
                            ok = false;
                            break;
                        }
                        sel
                    }
                    Err(e) => {
                        out.raw(Normal, &render_stream_sel_error(&e, &t[*idx]));
                        ok = false;
                        break;
                    }
                },
                _ => libfreemkv::StreamSelection::default(),
            };
            let opts = libfreemkv::InputOptions {
                unit_keys: iso_unit_keys.clone(),
                title_index: *title_idx,
                raw,
                key_fetch: iso_key_fetch.clone(),
                selection,
            };
            pipe(source, dest_url, &opts, &out)
        };

        if let Err(e) = result {
            // The skip / stop / fail decision is the ENGINE's single policy
            // (freemkv_engine::decide_title), shared with autorip + the desktop
            // UI. The CLI keeps only the presentation of each outcome.
            match freemkv_engine::decide_title(
                &e.result,
                is_feature,
                multi_title,
                explicit_selection,
            ) {
                freemkv_engine::TitleAction::Skip => {
                    // An incidental extra title in an all-titles rip is a stub
                    // (E7023 uncrackable, or E6008 empty). Skip with a clear
                    // notice and keep muxing the rest so the command can exit 0.
                    let num = title_idx.map(|i| i + 1).unwrap_or(0);
                    let key = match parse_error_code(&e.display) {
                        Some(("E6008", _)) => "rip.title_skipped_empty",
                        _ => "rip.title_skipped",
                    };
                    out.raw(Normal, &strings::fmt(key, &[("num", &num.to_string())]));
                }
                freemkv_engine::TitleAction::StopHalt => {
                    // Ctrl-C is a FULL STOP: surface the interrupt and break the
                    // whole loop — do NOT continue cancelling each later title.
                    out.raw(Normal, &render_error(&e.display));
                    ok = false;
                    break;
                }
                freemkv_engine::TitleAction::StopNoKey => {
                    // The disc as a whole has no key — every remaining title
                    // fails identically. Fail fast: print once and stop, instead
                    // of iterating all N titles re-printing the same error.
                    out.raw(Normal, &render_error(&e.display));
                    ok = false;
                    break;
                }
                freemkv_engine::TitleAction::StopFatal => {
                    // The title the user actually wants failed hard. Print and
                    // fail, but keep looping in case a later wanted title differs.
                    out.raw(Normal, &render_error(&e.display));
                    ok = false;
                }
                // `Continue` is the `Ok(())` arm above; a decided-Continue on an
                // Err cannot happen (Ok/Halted/NoKey/Stub/Failed are exhaustive).
                freemkv_engine::TitleAction::Continue => {}
            }
        }
        out.blank(Normal);
    }

    ok
}

// ── Pre-flight invocation validation (fail loud and EARLY) ──────────────────

// See docs/pipe.md#is_scheme_only_sink — Whether a destination is a scheme-only sink with…
fn is_scheme_only_sink(parsed_dest: &libfreemkv::StreamUrl) -> bool {
    matches!(
        parsed_dest,
        libfreemkv::StreamUrl::Null | libfreemkv::StreamUrl::Stdio
    )
}

// See docs/pipe.md#preflight_validate — Validate the whole rip invocation BEFORE any drive…
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
        let non_empty = std::fs::read_dir(path)
            .map(|mut it| it.next().is_some())
            .unwrap_or(false);
        if non_empty {
            return Err(strings::fmt(
                "error.dir_dest_not_empty",
                &[("path", &path.display().to_string())],
            ));
        }
    }
    Ok(())
}

// See docs/pipe.md#validate_dir_input — Validate a `dir://` SOURCE: it must exist, be…
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

// See docs/pipe.md#validate_iso_input — Validate an `iso://` input path: must exist, be…
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

// See docs/pipe.md#validate_file_dest — Validate a single-file destination path: the parent directory…
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

// See docs/pipe.md#disc_title_identities — The titles this disc has, for expanding `-t…
fn disc_title_identities(
    source: &str,
    keys: &KeyConfig,
    out: &Output,
) -> Option<Vec<TitleIdentity>> {
    let parsed = libfreemkv::parse_url(source);
    let target = match &parsed {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => {
            libfreemkv::DeviceTarget::Path(p.clone())
        }
        _ => libfreemkv::DeviceTarget::Autodetect,
    };
    let (session, _trace) = freemkv_engine::open_scan_resolve(
        target,
        drive_credentials(keys.keydb_path()),
        key_source_factory(keys, out),
    )
    .ok()?;
    let ids: Vec<TitleIdentity> = session
        .disc()
        .map(|d| d.titles.iter().map(TitleIdentity::of).collect())?;
    // Drop the session before the per-title loop reopens the drive — the same
    // shape the desktop app already uses.
    drop(session);
    (!ids.is_empty()).then_some(ids)
}

// See docs/pipe.md#disc_title_nums — The title list a DISC source should rip,…
pub(crate) fn disc_title_nums(all_titles: bool, requested: &[usize], found: usize) -> Vec<usize> {
    if !all_titles || !requested.is_empty() {
        return requested.to_vec();
    }
    (1..=found).collect()
}

// See docs/pipe.md#resolve_disc_all_titles — Resolve the `(title_nums, identities)` pair for a `-t…
fn resolve_disc_all_titles(
    requested: &[usize],
    scan: Option<Vec<TitleIdentity>>,
) -> Option<(Vec<usize>, Vec<TitleIdentity>)> {
    let ids = scan?;
    Some((disc_title_nums(true, requested, ids.len()), ids))
}

// See docs/pipe.md#build_jobs — Build the `(title_index, dest_url)` job list
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
                Normal,
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
            // A single-file dest can't hold multiple titles: `dir_jobs` would
            // silently turn `movie.mkv` into a directory. Mirror the
            // scanned-source guard above and reject up front.
            if !is_dir_dest {
                out.raw(
                    Normal,
                    &strings::fmt("error.multi_title_needs_dir", &[("dest", dest)]),
                );
                return None;
            }
            dir_jobs(&indices, "disc")
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

// See docs/pipe.md#keyless_scan_opts — Disc source: one open, one scan, one stream.…
fn keyless_scan_opts() -> libfreemkv::ScanOptions {
    libfreemkv::ScanOptions::default()
}

// See docs/pipe.md#drive_scan_opts — ScanOptions for a **live-drive** scan: keyless, plus the…
pub(crate) fn drive_scan_opts(keydb_path: &Option<String>) -> libfreemkv::ScanOptions {
    libfreemkv::ScanOptions {
        credentials: drive_credentials(keydb_path),
        ..Default::default()
    }
}

// See docs/pipe.md#dest_is_directory — Is the destination directory-STYLE — a trailing `/`,…
fn dest_is_directory(dest: &str, parsed_dest: &libfreemkv::StreamUrl) -> bool {
    dest.ends_with('/') || std::path::Path::new(parsed_dest.path_str()).is_dir()
}

// See docs/pipe.md#drive_credentials — Build the AACS host credentials for a live-drive…
pub(crate) fn drive_credentials(
    keydb_path: &Option<String>,
) -> Option<libfreemkv::DriveCredentials> {
    let path = resolved_keydb_path(keydb_path);
    let host_certs = freemkv_keysources::KeydbSource::new(path).host_certs();
    (!host_certs.is_empty()).then_some(libfreemkv::DriveCredentials { host_certs })
}

// See docs/pipe.md#resolve_info_keys — Resolve a **live drive's** AACS unit keys in…
pub(crate) fn resolve_info_keys(
    drive: &mut libfreemkv::Drive,
    disc: &mut libfreemkv::Disc,
    keydb_path: &Option<String>,
    out: &Output,
) {
    let keys = KeyConfig {
        keydb_path: keydb_path.clone(),
        key_url: None,
        key_auth: None,
    };
    resolve_disc_keys(disc, drive, &keys, out);
}

// See docs/pipe.md#scan_iso — Scan an `iso://` source's structure ONCE (keyless). The…
fn scan_iso(source: &str) -> Option<(libfreemkv::Disc, Box<dyn libfreemkv::SectorSource>)> {
    // `dir://` is an image-level source too: `scan_dir` synthesizes a UDF
    // volume and returns the same pair `scan_iso` does. Without this arm,
    // opening a source "as an image" silently rejected folders.
    match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Iso { path } => {
            libfreemkv::scan_iso(std::path::Path::new(&path), keyless_scan_opts()).ok()
        }
        libfreemkv::StreamUrl::Dir { path } => {
            libfreemkv::scan_dir(std::path::Path::new(&path), keyless_scan_opts()).ok()
        }
        _ => None,
    }
}

// See docs/pipe.md#resolve_iso_unit_keys — Resolve an ISO's AACS unit keys from an…
fn resolve_iso_unit_keys(
    mut disc: libfreemkv::Disc,
    mut reader: Box<dyn libfreemkv::SectorSource>,
    keys: &KeyConfig,
    out: &Output,
) -> Vec<(u32, [u8; 16])> {
    resolve_disc_keys(&mut disc, reader.as_mut(), keys, out);
    match disc.decrypt_keys() {
        libfreemkv::DecryptKeys::Aacs { unit_keys, .. } => unit_keys,
        _ => Vec::new(),
    }
}

// See docs/pipe.md#build_iso_key_fetch — Build the fresh-key-on-failure closure for an ISO mux,…
fn build_iso_key_fetch(source: &str, keys: &KeyConfig) -> Option<libfreemkv::sector::KeyFetch> {
    let url = keys.key_url.clone()?;
    // Reuse the SSRF guard the upfront source list uses; a rejected URL means no
    // fetch (rather than POSTing key material to an internal/metadata host).
    if freemkv_keysources::validate_keyserver_url(&url).is_err() {
        return None;
    }
    let auth = keys.key_auth.clone().unwrap_or_default();
    // Iso AND Dir: a folder is an image-level source with the same AACS
    // inputs. Matching only `Iso` silently disabled online key fetch for
    // `dir://` even when the same disc's key was retrievable as an ISO.
    let (path, from_dir) = match libfreemkv::parse_url(source) {
        libfreemkv::StreamUrl::Iso { path } => (path, false),
        libfreemkv::StreamUrl::Dir { path } => (path, true),
        _ => return None,
    };
    // Capture the disc's inf + MKB ONCE; a non-AACS source yields an error → None.
    let p = std::path::Path::new(&path);
    let (inf, mkb, version) = if from_dir {
        libfreemkv::Disc::read_aacs_inputs_from_dir(p).ok()?
    } else {
        libfreemkv::Disc::read_aacs_inputs(p).ok()?
    };
    if inf.is_empty() {
        return None;
    }
    // Disc inputs the lib's `key_fetch` reuses per call (it swaps in the failing
    // `samples`). An ISO has no live-drive VID (all-zero) — VID-optional. The
    // version drives the Unit_Key_RO.inf stride for a VUK-from-server reply.
    let inputs = libfreemkv::DiscInputs {
        disc_hash: String::new(),
        volume_id: [0u8; 16],
        version,
        mkb,
        unit_key_ro: inf,
        samples: Vec::new(),
        volume_label: None,
    };
    // Zero duplicated fetch logic: the lib's `key_fetch` owns the full flow.
    // The CLI only supplies the disc inputs and a way to rebuild its key
    // source (the `--key-url` OnlineSource).
    let make_sources: std::sync::Arc<
        dyn Fn() -> Vec<Box<dyn libfreemkv::keysource::KeySource>> + Send + Sync,
    > = std::sync::Arc::new(move || {
        vec![Box::new(freemkv_keysources::OnlineSource::new(
            url.clone(),
            auth.clone(),
        )) as Box<dyn libfreemkv::keysource::KeySource>]
    });
    Some(libfreemkv::keysource::key_fetch(inputs, make_sources))
}

// See docs/pipe.md#resolved_keydb_path — The keydb path to use: `--keydb <path>` if…
pub(crate) fn resolved_keydb_path(keydb_path: &Option<String>) -> std::path::PathBuf {
    keydb_path
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(freemkv_keysources::existing_keydb_path)
        .or_else(freemkv_keysources::default_keydb_path)
        .unwrap_or_else(|| std::path::PathBuf::from("keydb.cfg"))
}

// See docs/pipe.md#build_key_sources_quiet — Build the ordered `KeySource` list from the key…
fn build_key_sources_quiet(keys: &KeyConfig) -> Vec<Box<dyn freemkv_keysources::KeySource>> {
    freemkv_engine::key_sources(&key_params(keys))
}

// See docs/pipe.md#key_params — Normalize the CLI's flags into the engine's `KeyParams`,…
fn key_params(keys: &KeyConfig) -> freemkv_engine::KeyParams {
    // Local keydb is added whenever the user didn't ask for online-only. (An
    // explicit --keydb, or no key flags at all, both want the keydb.)
    let online_only = keys.key_url.is_some() && keys.keydb_path.is_none();
    let keydb_path = if online_only {
        None
    } else {
        Some(
            resolved_keydb_path(&keys.keydb_path)
                .to_string_lossy()
                .into_owned(),
        )
    };
    freemkv_engine::KeyParams {
        keydb_path,
        key_url: keys.key_url.clone(),
        key_auth: keys.key_auth.clone(),
        online_only,
    }
}

/// Print the SSRF-rejected-`--key-url` warning (once) if the configured key URL
/// fails validation — matching the pre-hoist `build_key_sources` behaviour.
fn warn_ssrf_rejected(keys: &KeyConfig, out: &Output) {
    if let Some(url) = &keys.key_url
        && let Err(e) = freemkv_keysources::validate_keyserver_url(url)
    {
        out.raw(
            Normal,
            &strings::fmt("error.keyserver_url_rejected", &[("error", &e)]),
        );
    }
}

// See docs/pipe.md#key_source_factory — Build the [`libfreemkv::KeySourceFactory`] the library's key resolution
fn key_source_factory(keys: &KeyConfig, out: &Output) -> libfreemkv::KeySourceFactory {
    warn_ssrf_rejected(keys, out);
    let keys = keys.clone();
    std::sync::Arc::new(move || build_key_sources_quiet(&keys))
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

// See docs/pipe.md#resolve_disc_keys — Resolve an AACS key for a keyless-scanned `disc`…
fn resolve_disc_keys(
    disc: &mut libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    keys: &KeyConfig,
    out: &Output,
) {
    let factory = key_source_factory(keys, out);
    let resolved = libfreemkv::resolve_keys_for(reader, disc, factory);
    emit_resolution_trace(out, &resolved.trace);
}

// See docs/pipe.md#render_resolution_trace — Render a [`libfreemkv::aacs::trace::ResolutionTrace`] into human-readable
fn render_resolution_trace(trace: &libfreemkv::aacs::trace::ResolutionTrace) -> Vec<String> {
    use libfreemkv::aacs::trace::{KeyNode, KeyOutcome as KO, UnlockOutcome};

    let mkb = |m: Option<u32>| match m {
        Some(n) => format!(" (MKBv{n})"),
        None => String::new(),
    };
    let mut lines = Vec::new();

    for step in &trace.unlock {
        // `who` is the unlocker's own name() — printed verbatim (no enum to map).
        let outcome = match step.outcome {
            UnlockOutcome::Unlocked => "UNLOCKED".to_string(),
            UnlockOutcome::FirmwareNotUnlockable => "firmware not unlockable".to_string(),
            UnlockOutcome::NoUsableHostCert { mkb: m } => format!("no usable host cert{}", mkb(m)),
            UnlockOutcome::CertRevoked { mkb: m } => format!("host cert revoked{}", mkb(m)),
            UnlockOutcome::HandshakeRejected => "handshake rejected".to_string(),
            UnlockOutcome::VidUnavailable => "Volume ID unavailable".to_string(),
        };
        lines.push(format!("unlock: {} > {outcome}", step.who));
    }

    for step in &trace.keys {
        // `who` is the source's own label() — printed verbatim (no enum to map).
        let nodes: Vec<&str> = step
            .path
            .iter()
            .map(|n| match n {
                KeyNode::MatchedDisc => "matched disc",
                KeyNode::NoEntry => "no entry",
                KeyNode::FoundUnitKeys => "found unit keys",
                KeyNode::FoundVuk => "found VUK",
                KeyNode::FoundMediaKey => "found media key",
                KeyNode::NeedVid => "need VID",
                KeyNode::VidFromUnlock => "VID from drive",
                KeyNode::VidFromKeydb => "VID from keydb",
                KeyNode::NoVid => "no VID",
                KeyNode::DerivedVuk => "derived VUK",
                KeyNode::DerivedUnitKeys => "derived unit keys",
            })
            .collect();
        let outcome = match step.outcome {
            KO::Resolved => "RESOLVED",
            KO::MissingVid => "MISSING VID",
            KO::NoKey => "NO KEY",
        };
        let mut parts = vec![step.who.clone()];
        parts.extend(nodes.into_iter().map(str::to_string));
        parts.push(outcome.to_string());
        lines.push(format!("key: {}", parts.join(" > ")));
    }

    lines
}

#[allow(clippy::too_many_arguments)] // cohesive single-title disc rip
fn pipe_disc(
    source: &str,
    dest: &str,
    title_idx: usize,
    expected: Option<&TitleIdentity>,
    keys: &KeyConfig,
    raw: bool,
    _multipass: bool,
    streams: &freemkv_engine::StreamChoice,
    multi_title: bool,
    out: &Output,
) -> Result<(), PipeFail> {
    let parsed = libfreemkv::parse_url(source);
    let target = match &parsed {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => {
            libfreemkv::DeviceTarget::Path(p.clone())
        }
        _ => libfreemkv::DeviceTarget::Autodetect,
    };

    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    // Drive bring-up (open + lock-tray + scan + resolve keys) is the shared
    // optical core in `freemkv_engine::open_scan_resolve`, also used by the
    // desktop GUI. Tray unlock is guaranteed by `Drive::drop`; CLI keeps only presentation.
    let (mut session, trace) = freemkv_engine::open_scan_resolve(
        target,
        drive_credentials(keys.keydb_path()),
        key_source_factory(keys, out),
    )
    .map_err(|e| {
        PipeFail::fatal(match &e {
            libfreemkv::Error::DeviceNotFound { path } if path.is_empty() => {
                strings::get("error.no_drive")
            }
            _ => format!("{}", e),
        })
    })?;
    emit_resolution_trace(out, &trace);
    // ── Pre-flight validation (borrows the scanned disc; no drive I/O) ──
    // The session is kept intact so `mux_stream(MuxInput::Session)` can take
    // its staged reader; range-check and decrypt gates borrow `disc` immutably.
    let batch = libfreemkv::disc::detect_max_batch_sectors(session.device_path());
    // Resolved -a/-s PID selection for this title (default = keep all).
    let mut selection = libfreemkv::StreamSelection::default();
    {
        let disc = session.disc().expect("scan populated the disc");
        // The same range + identity rules from `run()`'s scanned-source path,
        // centralized in `resolve_scanned_title`: range prevents a panic on
        // `disc.titles[title_idx]` below; identity catches a diverging re-scan.
        let title =
            resolve_scanned_title(&disc.titles, title_idx, expected).map_err(PipeFail::fatal)?;

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

        // Pre-flight decrypt gate (disc-wide): catches a scrambled-but-uncracked
        // CSS disc or an AACS disc with no resolved key before the mux, so the
        // dedicated NoDiscKey/CssKeyMissing message fires. `--raw` discs pass.
        disc.ensure_decryptable(raw).map_err(PipeFail::from_typed)?;

        // Per-title decrypt gate for AACS/non-DVD: same keys `mux_stream`
        // resolves later, but a `None` key fails loudly here rather than
        // muxing garbage. DVD is gated inside `DiscStream::new` instead.
        if needs_pre_mux_title_key(disc.format) {
            let keys = disc.decrypt_keys();
            disc.ensure_title_decryptable(raw, &keys, false)
                .map_err(PipeFail::from_typed)?;
        }
    }

    // The live drive is opened and the source is ready — complete the source
    // "opening…" line started above. The disc→PES construction (DiscStream::new,
    // the per-title CSS crack, the header pump) now lives inside `mux_stream`.
    out.raw(Normal, &strings::get("rip.ok"));

    // Stage the drive as the session's boxed reader, then run the shared
    // driver: builds `DiscStream`, pumps headers (metadata short-circuit runs
    // before the header gate now), opens the sink, and pumps frames to EOF/halt.
    session.stage_drive_as_reader();
    let metadata_sink = is_metadata_sink(dest);
    let events = Arc::new(CliMuxEvents::new(*out, dest.to_string(), metadata_sink));
    let opts = MuxOptions {
        skip_errors: false,
        batch_sectors: batch,
        raw,
        // Interactive stdout/network sink: no per-frame send deadline. A
        // slow-but-alive downstream (paused pager, backpressured pipe) must
        // block, not be reported as interrupted. Ctrl-C still works via SIGINT.
        send_deadline: None,
        selection,
    };
    let sigint = SigintHalt::install();
    let result = libfreemkv::mux_stream(
        MuxInput::Session {
            session: &mut session,
            title_index: title_idx,
        },
        dest,
        &opts,
        sigint.halt(),
        events.clone() as Arc<dyn MuxEvents>,
    );
    drop(sigint);
    finalize_mux(result, out, &events)
}

// See docs/pipe.md#is_metadata_sink — Whether a destination URL is a metadata sink…
fn is_metadata_sink(dest: &str) -> bool {
    matches!(
        libfreemkv::parse_url(dest),
        libfreemkv::StreamUrl::Chapters { .. } | libfreemkv::StreamUrl::Json { .. }
    )
}

// See docs/pipe.md#disc_copy_recovered_data — Whether a completed disc→ISO sweep actually recovered any…
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
    /// disc: sectors were unreadable, or were skipped and left pending. The
    /// image is worth keeping and worth retrying with `--multipass`; what it
    /// is not is complete, and it must not be reported as if it were.
    Lossy,
    /// A usable image, and all of it.
    Complete,
}

// See docs/pipe.md#disc_copy_succeeded — Whether a graded copy is a SUCCESS for…
fn disc_copy_succeeded(verdict: CopyVerdict) -> bool {
    matches!(verdict, CopyVerdict::Complete)
}

// See docs/pipe.md#copy_verdict — Grade a `CopyResult`. Two of the three verdicts…
fn copy_verdict(r: &freemkv_engine::CopyResult) -> CopyVerdict {
    if r.halted {
        CopyVerdict::Interrupted
    } else if !disc_copy_recovered_data(r.bytes_good) {
        CopyVerdict::NoData
    } else if r.bytes_unreadable > 0 || r.bytes_pending > 0 {
        // Bytes the drive never delivered. `bytes_pending` counts as loss for
        // the same reason `bytes_unreadable` does: they were attempted and
        // skipped, and a single-pass sweep has no later pass to fetch them.
        CopyVerdict::Lossy
    } else {
        CopyVerdict::Complete
    }
}

// See docs/pipe.md#disc_copy_options — The copy options a CLI disc→ISO sweep runs…
fn disc_copy_options<'a>(
    raw: bool,
    multipass: bool,
    progress: &'a dyn libfreemkv::progress::Progress,
) -> freemkv_engine::CopyOptions<'a> {
    freemkv_engine::CopyOptions {
        decrypt: !raw,
        multipass,
        halt: None,
        progress: Some(progress),
        ..Default::default()
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

// See docs/pipe.md#pipe — Decide whether a per-title mux failure should abort…
fn pipe(
    source: &str,
    dest: &str,
    opts: &libfreemkv::InputOptions,
    out: &Output,
) -> Result<(), PipeFail> {
    // Source open, header pump/gate, sink open, metadata short-circuit, frame
    // pump, and NoStreams guard all live inside `mux_stream` now. The CLI
    // keeps only presentation, via `CliMuxEvents` (stream-info, progress bar).
    out.raw_inline(Normal, &strings::fmt("rip.opening", &[("device", source)]));
    out.raw(Normal, &strings::get("rip.ok"));

    let metadata_sink = is_metadata_sink(dest);
    let events = Arc::new(CliMuxEvents::new(*out, dest.to_string(), metadata_sink));
    let mux_opts = MuxOptions {
        skip_errors: false,
        batch_sectors: 0, // unused by the URL arm (input() owns batching)
        raw: opts.raw,
        // No per-frame send deadline on the CLI's stdout / network sinks — a
        // slow-but-alive consumer must block rather than surface a spurious
        // interrupt. Ctrl-C still stops the pump via the SIGINT halt.
        send_deadline: None,
        selection: Default::default(),
    };
    let sigint = SigintHalt::install();
    let result = libfreemkv::mux_stream(
        MuxInput::Url {
            url: source,
            opts: opts.clone(),
        },
        dest,
        &mux_opts,
        sigint.halt(),
        events.clone() as Arc<dyn MuxEvents>,
    );
    drop(sigint);
    finalize_mux(result, out, &events)
}

// ── Disc → ISO (raw sector copy, not a stream) ────────────────────────────

// See docs/pipe.md#url_path_of — The filesystem path behind a URL — for…
fn url_path_of(url: &libfreemkv::StreamUrl) -> Option<std::path::PathBuf> {
    use libfreemkv::StreamUrl as U;
    match url {
        U::Mkv { path }
        | U::M2ts { path }
        | U::Mp4 { path }
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

// See docs/pipe.md#image_to_iso — `<image source> → iso://` — write a decrypted…
fn image_to_iso(source: &str, dest: &str, keys: &KeyConfig, out: &Output) -> bool {
    let iso_path = match libfreemkv::parse_url(dest) {
        libfreemkv::StreamUrl::Iso { path } => path,
        _ => return false,
    };

    let (mut disc, reader) = match scan_iso(source) {
        Some(pair) => pair,
        None => {
            out.raw(
                Normal,
                &strings::fmt("error.iso_unreadable", &[("path", source)]),
            );
            return false;
        }
    };

    let mut reader = reader;
    // Resolve keys against the source before copying: an `iso://` destination is
    // a DECRYPTED image, so an encrypted source that yields no key must fail
    // here rather than write ciphertext under a name that promises plaintext.
    resolve_disc_keys(&mut disc, reader.as_mut(), keys, out);
    if let Err(e) = disc.ensure_decryptable(false) {
        out.raw(Normal, &render_error(&e));
        return false;
    }

    // Never write over the source: `write_image` opens the dest with
    // `File::create` before the first read, truncating a still-open input
    // (e.g. an in-place decrypt paste). The GUI has had this guard since round 1.
    if same_file(
        source_path_of(source).as_deref(),
        std::path::Path::new(&iso_path),
    ) {
        out.raw(Normal, &strings::get("error.dest_is_source"));
        return false;
    }

    let total_sectors = disc.capacity_sectors;
    let start = std::time::Instant::now();
    let halt = libfreemkv::halt::Halt::new();
    // AACS decryption is MAP-ONLY: `decrypt_span` refuses without a key map
    // installed. A bare decorator here made every AACS decrypt abort on its
    // first batch — mirrors `freemkv-engine`'s own construction instead.
    let mut keys = disc.decrypt_keys();
    let key_map = if matches!(keys, libfreemkv::decrypt::DecryptKeys::Aacs { .. }) {
        match disc.resolve_content_key_map(reader.as_mut(), &mut keys, None, None) {
            Ok(map) => Some(std::sync::Arc::new(map)),
            Err(e) => {
                out.raw(Normal, &render_error(&e));
                return false;
            }
        }
    } else {
        None
    };
    let content_ranges = disc.encrypted_content_ranges();
    let mut src = libfreemkv::DecryptingSectorSource::new(reader, keys);
    if let Some(map) = key_map {
        src = src.with_key_map(map);
    } else if !content_ranges.is_empty() {
        src = src.with_content_ranges(std::sync::Arc::from(content_ranges));
    }

    let result = libfreemkv::write_image(&mut src, &iso_path, total_sectors, &halt, |_| {
        // `write_image` checks `halt` once per batch and this runs at the end of
        // each batch, so a Ctrl-C is honored on the next one. Same first-Ctrl-C
        // semantics as the disc path, without a second signal being needed.
        if INTERRUPTED.load(Ordering::SeqCst) {
            halt.cancel();
        }
    });

    match result {
        Ok(bytes) => {
            let elapsed = start.elapsed().as_secs_f64();
            let mb = bytes as f64 / (1024.0 * 1024.0);
            let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
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
            true
        }
        Err(libfreemkv::Error::Halted) => {
            // Don't print "Complete" over a partial image, and return failure so
            // a scripted caller's `$?` sees the interruption.
            out.raw(Normal, &strings::get("rip.interrupted"));
            false
        }
        Err(e) => {
            out.raw(Normal, &render_error(&e));
            false
        }
    }
}

/// Returns true on success, false on any failure (no drive, scan error,
/// `Disc::copy` error). The caller propagates this to `main`'s exit code so a
/// scripted `$?` check sees the failure.
fn disc_to_iso(
    source: &str,
    dest: &str,
    keys: &KeyConfig,
    raw: bool,
    multipass: bool,
    out: &Output,
) -> bool {
    let parsed_source = libfreemkv::parse_url(source);
    let parsed_dest = libfreemkv::parse_url(dest);
    let device = match &parsed_source {
        libfreemkv::StreamUrl::Disc { device: Some(p) } => Some(p.clone()),
        _ => None,
    };

    let mut drive = match device {
        Some(ref d) => match libfreemkv::Drive::open(d) {
            Ok(d) => d,
            Err(e) => {
                out.raw(Normal, &render_error(&e));
                return false;
            }
        },
        None => match libfreemkv::find_drive() {
            Some(d) => d,
            None => {
                out.raw(Normal, &strings::get("error.no_drive"));
                return false;
            }
        },
    };
    out.raw(
        Normal,
        &strings::fmt("rip.drive", &[("device", drive.device_path())]),
    );
    debug_drive_step("wait_ready", drive.wait_ready());
    debug_drive_step("init", drive.init());
    // probe_disc is advisory: it routinely fails (no disc, already probed) and
    // the scan below re-derives what it needs, so its result stays discarded.
    let _ = drive.probe_disc();

    let mut disc = match libfreemkv::Disc::scan(&mut drive, &drive_scan_opts(keys.keydb_path())) {
        Ok(d) => d,
        Err(e) => {
            out.raw(
                Normal,
                &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
            );
            return false;
        }
    };
    // Resolve + apply the AACS key so the keys persist in the mapfile during
    // disc→ISO copy (the mux step reads them back to decrypt). Ciphertext is
    // sampled internally so the resolved key is validated against real data.
    resolve_disc_keys(&mut disc, &mut drive, keys, out);

    // Pre-flight decrypt gate: a decrypting copy of an encrypted disc with no
    // usable key would write ciphertext and exit 0. Refuse here, before
    // locking the tray, so `Disc::copy`'s gate is surfaced with a localized message.
    if let Err(e) = disc.ensure_decryptable(raw) {
        out.raw(Normal, &render_error(&e));
        return false;
    }

    let disc_name = sanitize_name(disc.meta_title.as_deref().unwrap_or(&disc.volume_id));
    let (iso_path, is_null) = match &parsed_dest {
        libfreemkv::StreamUrl::Iso { path } => (path.clone(), false),
        libfreemkv::StreamUrl::Null => {
            let p = std::path::PathBuf::from("/dev/null");
            (p, true)
        }
        _ => unreachable!(),
    };

    let total_bytes = disc.capacity_sectors as u64 * libfreemkv::consts::SECTOR_BYTES_U64;
    out.raw(
        Normal,
        &strings::fmt(
            "rip.disc_label",
            &[
                ("name", &disc_name),
                (
                    "size",
                    &format!("{:.1}", total_bytes as f64 / 1_073_741_824.0),
                ),
            ],
        ),
    );
    if !is_null {
        out.raw(
            Normal,
            &strings::fmt("rip.output", &[("path", &iso_path.display().to_string())]),
        );
    }
    out.blank(Normal);

    drive.lock_tray();
    let start = std::time::Instant::now();
    let last_update = std::cell::Cell::new(start);
    // Speed + ETA come from the ENGINE's one derivation (no local math). The CLI
    // only throttles the display and formats the numbers.
    let speed_est = std::cell::RefCell::new(freemkv_engine::SpeedEstimator::new());

    struct CliProgress<'a> {
        out: &'a Output,
        last_update: &'a std::cell::Cell<std::time::Instant>,
        speed_est: &'a std::cell::RefCell<freemkv_engine::SpeedEstimator>,
    }
    impl libfreemkv::progress::Progress for CliProgress<'_> {
        fn report(&self, p: &libfreemkv::progress::PassProgress) -> bool {
            if !self.out.is_quiet() {
                let now = std::time::Instant::now();
                if now.duration_since(self.last_update.get()).as_secs_f64() >= 0.5 {
                    self.last_update.set(now);
                    let (speed_bps, eta_secs) = self
                        .speed_est
                        .borrow_mut()
                        .sample(p.work_done, p.work_total);
                    print_disc_progress(p, speed_bps, eta_secs);
                }
            }
            // Returning false halts the copy. Consult the global SIGINT flag so
            // the FIRST Ctrl-C stops the sweep and lets `unlock_tray()` run
            // below, instead of waiting for `_exit(130)`, which skips it.
            copy_should_continue(INTERRUPTED.load(Ordering::SeqCst))
        }
    }
    let progress = CliProgress {
        out,
        last_update: &last_update,
        speed_est: &speed_est,
    };

    let copy_opts = disc_copy_options(raw, multipass, &progress);
    let success = match freemkv_engine::copy(&disc, &mut drive, &iso_path, &copy_opts) {
        Ok(r) if copy_verdict(&r) == CopyVerdict::Interrupted => {
            // Ctrl-C halted the copy. Don't print "Complete" over a partial
            // ISO — report interrupted/failure so exit is non-zero. Mapfile
            // is preserved, so a later run can resume.
            if !out.is_quiet() {
                eprint!("\r\x1b[K");
            }
            out.raw(Normal, &strings::get("rip.interrupted"));
            false
        }
        Ok(r) if copy_verdict(&r) == CopyVerdict::NoData => {
            // The copy completed but recovered ZERO readable bytes: the ISO on
            // disk is unusable. Don't print "Complete" — a scripted caller
            // checking $? must see non-zero, like the mux paths' NoStreams guard.
            if !out.is_quiet() {
                eprint!("\r\x1b[K");
            }
            let mb_bad = r.bytes_unreadable as f64 / 1_048_576.0;
            out.raw(
                Normal,
                &strings::fmt("rip.no_data", &[("unreadable", &format!("{mb_bad:.1}"))]),
            );
            false
        }
        Ok(r) => {
            if !out.is_quiet() {
                eprint!("\r\x1b[K");
            }
            let verdict = copy_verdict(&r);
            let elapsed = start.elapsed().as_secs_f64();
            let mb = r.bytes_total as f64 / (1024.0 * 1024.0);
            let speed = if elapsed > 0.0 { mb / elapsed } else { 0.0 };
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
            // Report the LOSS whenever there is any, not only when recovery was
            // requested. Gated on `multipass`, a single-pass rip of a scratched
            // disc printed completion and nothing else — never produce that.
            if !disc_copy_succeeded(verdict) {
                let gb_good = r.bytes_good as f64 / 1_073_741_824.0;
                let mb_bad = r.bytes_unreadable as f64 / 1_048_576.0;
                let mb_pending = r.bytes_pending as f64 / 1_048_576.0;
                let mapfile_path = disc.mapfile_for(&iso_path);
                let main_title = disc.titles.first();
                let main_title_bad = main_title
                    .map(|t| freemkv_engine::bytes_bad_in_title_from_mapfile(&mapfile_path, t))
                    .unwrap_or(0);
                // Report damage as a main-title duration only: the old figure
                // multiplied a whole-disc ratio by `disc_dur` (first title's
                // runtime), wrong once bonus content grows the disc past it.
                let main_lost_secs = main_title
                    .map(|t| (t.size_bytes, t.duration_secs))
                    .filter(|&(sz, dur)| main_title_bad > 0 && sz > 0 && dur > 0.0)
                    .map(|(sz, dur)| main_title_bad as f64 / sz as f64 * dur)
                    .unwrap_or(0.0);
                out.raw(
                    Normal,
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
                        Normal,
                        &strings::fmt("rip.damage_lost_movie", &[("time", &main_str)]),
                    );
                }
            }
            // The verdict IS the exit code. Returning a bare `true` here is
            // what let a holed image exit 0.
            disc_copy_succeeded(verdict)
        }
        Err(e) => {
            out.raw(Normal, &render_error(&e));
            false
        }
    };

    drive.unlock_tray();
    success
}

// ── dir:// decrypted file-tree extraction ───────────────────────────────────

// See docs/pipe.md#dir_to_extract — Extract a disc's decrypted file tree to a…
fn dir_to_extract(
    source: &str,
    dest: &str,
    keys: &KeyConfig,
    parsed_source: &libfreemkv::StreamUrl,
    force: bool,
    out: &Output,
) -> bool {
    let dest_path = match libfreemkv::parse_url(dest) {
        libfreemkv::StreamUrl::Dir { path } => path,
        _ => return false,
    };

    // Open the right reader + scan, resolving keys, then extract. The two
    // source kinds differ only in how the `SectorSource` + scanned `Disc` are
    // obtained; the extraction is identical once keys are resolved.
    match parsed_source {
        libfreemkv::StreamUrl::Disc { device } => {
            let mut drive = match device {
                Some(p) => match libfreemkv::Drive::open(p) {
                    Ok(d) => d,
                    Err(e) => {
                        out.raw(Normal, &render_error(&e));
                        return false;
                    }
                },
                None => match libfreemkv::find_drive() {
                    Some(d) => d,
                    None => {
                        out.raw(Normal, &strings::get("error.no_drive"));
                        return false;
                    }
                },
            };
            out.raw(
                Normal,
                &strings::fmt("rip.drive", &[("device", drive.device_path())]),
            );
            debug_drive_step("wait_ready", drive.wait_ready());
            debug_drive_step("init", drive.init());
            let _ = drive.probe_disc();

            let mut disc =
                match libfreemkv::Disc::scan(&mut drive, &drive_scan_opts(keys.keydb_path())) {
                    Ok(d) => d,
                    Err(e) => {
                        out.raw(
                            Normal,
                            &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
                        );
                        return false;
                    }
                };
            resolve_disc_keys(&mut disc, &mut drive, keys, out);
            if let Err(e) = disc.ensure_decryptable(false) {
                out.raw(Normal, &render_error(&e));
                return false;
            }
            drive.lock_tray();
            let ok = run_extract(&disc, &mut drive, &dest_path, force, out);
            drive.unlock_tray();
            ok
        }
        // Iso AND Dir: scan_dir synthesizes a UDF volume over a folder,
        // returning the same pair. Dir used to fall to the `_` arm marked
        // "unreachable", which stopped being true once Dir joined is_disc_source().
        libfreemkv::StreamUrl::Iso { path } | libfreemkv::StreamUrl::Dir { path } => {
            let scan = if matches!(parsed_source, libfreemkv::StreamUrl::Dir { .. }) {
                libfreemkv::scan_dir
            } else {
                libfreemkv::scan_iso
            };
            let (mut disc, mut reader) = match scan(std::path::Path::new(path), keyless_scan_opts())
            {
                Ok(pair) => pair,
                Err(e) => {
                    out.raw(
                        Normal,
                        &strings::fmt("error.scan_failed", &[("detail", &e.to_string())]),
                    );
                    return false;
                }
            };
            resolve_disc_keys(&mut disc, reader.as_mut(), keys, out);
            if let Err(e) = disc.ensure_decryptable(false) {
                out.raw(Normal, &render_error(&e));
                return false;
            }
            run_extract(&disc, &mut reader, &dest_path, force, out)
        }
        _ => {
            // Unreachable: preflight rejects non-disc sources for dir://.
            out.raw(
                Normal,
                &strings::fmt("error.dir_source_unsupported", &[("source", source)]),
            );
            false
        }
    }
}

// See docs/pipe.md#HaltSink — A [`freemkv_engine::Sink`] that forwards `should_cancel` to a
struct HaltSink<'a>(&'a libfreemkv::Halt);

impl freemkv_engine::Sink for HaltSink<'_> {
    fn should_cancel(&self) -> bool {
        self.0.is_cancelled()
    }
}

// See docs/pipe.md#extract_succeeded — Whether a `dir://` extraction counts as a success…
fn extract_succeeded(halted: bool, complete: bool) -> bool {
    !halted && complete
}

/// Run `Disc::extract_tree` (via `freemkv_engine::extract_tree`) and render
/// the result. Shared by the disc:// and iso:// `dir://` paths.
fn run_extract(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    dest_path: &std::path::Path,
    force: bool,
    out: &Output,
) -> bool {
    out.raw(
        Normal,
        &strings::fmt(
            "dir.extracting",
            &[("path", &dest_path.display().to_string())],
        ),
    );
    out.blank(Normal);

    // Bridge the CLI's SIGINT flag into a libfreemkv Halt the producer polls,
    // so a long extract stops promptly, leaving the in-flight file as
    // `.partial`. `SigintHalt` joins the watcher on drop, even on unwind.
    let sigint = SigintHalt::install();

    let outcome =
        freemkv_engine::extract_tree(disc, reader, dest_path, force, &HaltSink(sigint.halt()));
    drop(sigint);

    match outcome {
        Ok(res) => {
            // Per-file loss lines (only the lossy ones, to keep output terse).
            for f in &res.files {
                let lost = f.bytes_unreadable;
                if lost > 0 {
                    out.raw(
                        Normal,
                        &strings::fmt(
                            "dir.file_lossy",
                            &[
                                ("file", &f.path.display().to_string()),
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
            let good_mb = res.bytes_good as f64 / 1_048_576.0;
            if res.complete {
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
                extract_succeeded(res.halted, res.complete)
            } else {
                let lost_mb = res.bytes_lost() as f64 / 1_048_576.0;
                out.raw(
                    Normal,
                    &strings::fmt(
                        "dir.lossy",
                        &[
                            ("files", &res.files.len().to_string()),
                            ("lost", &format!("{lost_mb:.2}")),
                        ],
                    ),
                );
                // A lossy extraction returns failure (non-zero exit) so scripts
                // can detect "extracted but holed" and re-run via iso multipass.
                extract_succeeded(res.halted, res.complete)
            }
        }
        Err(e) => {
            out.raw(Normal, &render_error(&e));
            false
        }
    }
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

// See docs/pipe.md#fmt_disc_damage — Render the disc-level damage string ("lost" / "no…
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
    // Speed + ETA are the ENGINE's derivation (freemkv_engine::SpeedEstimator);
    // the CLI only formats them. bps → MB/s for display.
    let inst_speed_mbps = speed_bps as f64 / 1_048_576.0;
    let bytes_disc = p.bytes_total_disc;
    if bytes_disc == 0 {
        return;
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
    eprint!(
        "\r  {:.1}/{:.1} GB ({:.1}%)  {}  ETA {}    {}    ",
        gb_done,
        gb_total,
        pct,
        fmt_speed(inst_speed_mbps),
        eta,
        damage,
    );
    let _ = std::io::stderr().flush();
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

// See docs/pipe.md#debug_drive_step — Log a discarded drive-handshake step error to stderr…
fn debug_drive_step(step: &str, result: libfreemkv::Result<()>) {
    if let Err(e) = result {
        eprintln!("freemkv: drive {step} (advisory) failed: {e}");
    }
}

// See docs/pipe.md#print_completion_summary — Clear the progress line and print the final…
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

// See docs/pipe.md#print_mp4_skips — For an `mp4://` destination, print the tracks that…
fn print_mp4_skips(out: &Output, dest: &str, title: &libfreemkv::DiscTitle) {
    if !matches!(
        libfreemkv::parse_url(dest),
        libfreemkv::StreamUrl::Mp4 { .. }
    ) {
        return;
    }
    let report = libfreemkv::mp4_fit_report(title);
    if report.skipped.is_empty() {
        return;
    }
    out.raw(
        Normal,
        &strings::fmt(
            "mp4.excluded_header",
            &[("count", &report.skipped.len().to_string())],
        ),
    );
    for (idx, reason) in &report.skipped {
        out.raw(
            Normal,
            &format!(
                "    - {} {}: {}",
                strings::get("stream.track"),
                idx + 1,
                strings::get(mp4_skip_reason_key(reason))
            ),
        );
    }
}

// See docs/pipe.md#mp4_skip_reason_key — The i18n key for an `mp4://` exclusion reason.…
fn mp4_skip_reason_key(reason: &libfreemkv::Mp4SkipReason) -> &'static str {
    match reason {
        libfreemkv::Mp4SkipReason::BitmapSubtitle => "mp4.reason.subtitle",
        libfreemkv::Mp4SkipReason::UnmappableAudio => "mp4.reason.audio",
        libfreemkv::Mp4SkipReason::SecondaryVideo => "mp4.reason.video",
        libfreemkv::Mp4SkipReason::UnmappableVideo => "mp4.reason.video_unmappable",
        // Post-mux reasons added in 1.6.0. Both reuse the existing translated
        // strings, since dedicated wording would need 29 real translations;
        // reusing a correct localisation beats shipping English. Follow-up tracked.
        libfreemkv::Mp4SkipReason::UndescribableAudio | libfreemkv::Mp4SkipReason::NoSamples => {
            "mp4.reason.audio"
        }
        // Mp4SkipReason is #[non_exhaustive] as of 1.6.0, so a new variant must not
        // break this build again. Falling back to the audio wording is a compromise
        // a future variant should replace with its own key.
        _ => "mp4.reason.audio",
    }
}

// See docs/pipe.md#is_url_token — Whether a token is a positional stream URL…
pub(crate) fn is_url_token(s: &str) -> bool {
    s.contains("://")
}

// See docs/pipe.md#is_keyserver_url — Whether a token is a plausible key-service URL…
fn is_keyserver_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// The `Disc::copy` progress callback returns `true` to continue, `false` to
/// halt. Halt the moment SIGINT was seen so the first Ctrl-C stops the copy
/// cleanly (letting the tray unlock on drop) instead of being ignored.
fn copy_should_continue(interrupted: bool) -> bool {
    !interrupted
}

// See docs/pipe.md#title_in_range — Whether a 0-based title index is within a…
fn title_in_range(idx: usize, count: usize) -> bool {
    idx < count
}

// See docs/pipe.md#TitleIdentity_use — What identifies a scanned title ACROSS an independent…
use crate::title_identity::TitleIdentity;

// See docs/pipe.md#title_changed_message — The message shown when a re-scan's title no…
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

// See docs/pipe.md#job_identity — The identity recorded for one job, out of…
fn job_identity(identities: &[TitleIdentity], title_idx: Option<usize>) -> Option<&TitleIdentity> {
    identities.get(title_idx.unwrap_or(0))
}

// See docs/pipe.md#resolve_scanned_title — Pick the title a job refers to out…
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

// See docs/pipe.md#needs_pre_mux_title_key — Whether a disc format needs its per-title key…
fn needs_pre_mux_title_key(format: libfreemkv::DiscFormat) -> bool {
    !matches!(format, libfreemkv::DiscFormat::Dvd)
}

// See docs/pipe.md#normalize_title_nums — The `-t` default: with no `-t N` and…
fn normalize_title_nums(title_nums: &mut Vec<usize>, all_titles: bool) {
    if title_nums.is_empty() && !all_titles {
        title_nums.push(1);
    }
}

// See docs/pipe.md#title_policy — The two inputs the per-title skip/stop policy runs…
fn title_policy(job_count: usize, title_nums: &[usize], all_titles: bool) -> (bool, bool) {
    (job_count > 1, !title_nums.is_empty() && !all_titles)
}

// See docs/pipe.md#is_feature_title — Whether this job is the main feature —…
fn is_feature_title(title_idx: Option<usize>) -> bool {
    title_idx.unwrap_or(0) == 0
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
mod tests {
    use super::{
        KeyConfig, PipeFail, build_jobs, build_key_sources_quiet, copy_should_continue,
        dest_is_directory, disc_copy_recovered_data, disc_title_nums, fmt_disc_damage, fmt_err,
        fmt_err_str, is_keyserver_url, is_metadata_sink, is_scheme_only_sink, is_url_token,
        mp4_skip_reason_key, parse_error_code, parse_flags, parse_stream_spec, preflight_validate,
        render_error, resolved_keydb_path, sanitize_name, title_in_range, validate_dir_input,
        validate_file_dest, validate_iso_input,
    };

    // See docs/pipe.md#a_dir_source_must_exist_and_be_a_directory — `dir://` source validation, which had no test at…
    #[test]
    fn a_dir_source_must_exist_and_be_a_directory() {
        let base = std::env::temp_dir().join(format!("fmkv-dirval-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // A real directory passes.
        assert!(validate_dir_input(&base).is_ok());

        // A path that is not there is refused, and names itself.
        let missing = base.join("no-such-folder");
        let err = validate_dir_input(&missing).expect_err("a missing folder must be refused");
        assert!(
            err.contains(&missing.display().to_string()),
            "the message must name the path the user typed, got: {err}"
        );

        // A FILE where a folder was meant is refused — the mistake a user
        // actually makes is pointing dir:// at the .iso next to the folder.
        let file = base.join("VIDEO_TS.iso");
        std::fs::write(&file, b"not a folder").unwrap();
        assert!(
            validate_dir_input(&file).is_err(),
            "a file is not a folder and must not reach open"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
    use crate::output::Output;
    use crate::strings;
    use libfreemkv::parse_url;

    // ── The `--help` examples must actually run ─────────────────────────────
    // Nothing connected `usage.ex.*` strings to the parser, so one shipped
    // rejected by `build_jobs`. Drives each through the real parser here instead.

    /// Split a `usage.ex.*` line into its command tokens, dropping the leading
    /// `freemkv` and the trailing right-hand description column (separated from
    /// the command by a run of two or more spaces).
    fn example_argv(line: &str) -> Vec<String> {
        let cmd = line.trim_start().split("  ").next().unwrap_or("").trim();
        cmd.split_whitespace()
            .skip(1) // the `freemkv` program name
            .map(str::to_string)
            .collect()
    }

    /// Every rip example printed by `usage()`, straight from the English
    /// catalogue — the exact text a user reads from `freemkv --help`.
    fn shipped_rip_examples() -> Vec<(&'static str, String)> {
        let en: serde_json::Value =
            serde_json::from_str(freemkv_i18n::bundled_locale_json("en").expect("en bundled"))
                .expect("en.json parses");
        // The keys `usage()` prints, in order. `info` is not a rip (no
        // destination URL) and is exercised by the `info` route's own tests.
        let keys = [
            "rip_mkv",
            "rip_m2ts",
            "rip_drive",
            "rip_title",
            "rip_titles",
            "rip_iso",
            "rip_iso_raw",
            "rip_iso_mp",
            "iso_to_mkv",
            "network",
            "network_recv",
            "stdio",
            "benchmark",
        ];
        keys.iter()
            .map(|k| {
                let line = en
                    .get("usage")
                    .and_then(|u| u.get("ex"))
                    .and_then(|e| e.get(k))
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| {
                        panic!(
                            "usage.ex.{k} is listed here but missing from en.json — \
                             keep this list in step with the block `usage()` prints"
                        )
                    });
                (*k, line.to_string())
            })
            .collect()
    }

    #[test]
    fn shipped_help_examples_parse_and_build_runnable_jobs() {
        let found = shipped_rip_examples();
        // A scratch root so a directory-style example's `create_dir_all` lands
        // in target/, not the crate root.
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!("help_examples_{}", std::process::id()));
        std::fs::create_dir_all(&scratch).expect("scratch dir");

        let out = Output::new(false, true);
        for (key, line) in &found {
            let argv = example_argv(line);
            assert!(!argv.is_empty(), "usage.ex.{key}: no command in {line:?}");

            // 1. The flags must parse.
            let flags = parse_flags(&argv)
                .unwrap_or_else(|e| panic!("usage.ex.{key}: flags rejected: {e}\n  {line}"));

            // 2. Source and destination URLs.
            let urls: Vec<&String> = argv.iter().filter(|a| is_url_token(a)).collect();
            assert_eq!(
                urls.len(),
                2,
                "usage.ex.{key}: expected a source and a destination URL, got {urls:?}"
            );
            let (source, dest) = (urls[0].as_str(), urls[1].as_str());
            let is_disc = matches!(parse_url(source), libfreemkv::StreamUrl::Disc { .. });

            // Relocate a filesystem destination under the scratch root so the
            // example's own path is never created in the crate root. The
            // trailing slash (and hence its directory-ness) is preserved.
            let parsed_shipped = parse_url(dest);
            let dest_owned = match parsed_shipped {
                libfreemkv::StreamUrl::Mkv { .. }
                | libfreemkv::StreamUrl::M2ts { .. }
                | libfreemkv::StreamUrl::Iso { .. } => format!(
                    "{}://{}/{}",
                    parsed_shipped.scheme(),
                    scratch.display(),
                    parsed_shipped.path_str()
                ),
                _ => dest.to_string(),
            };
            let parsed_dest = parse_url(&dest_owned);

            // 3. The job set must build. `None` is the CLI's hard rejection —
            //    the exact path `-t 1 -t 3` into `mkv://Movie.mkv` took.
            let titles = None;
            let jobs = build_jobs(
                &titles,
                is_disc,
                &flags.title_nums,
                dest_is_directory(&dest_owned, &parsed_dest),
                &dest_owned,
                &parsed_dest,
                &out,
            );
            assert!(
                jobs.is_some(),
                "usage.ex.{key} is printed by `freemkv --help` but the CLI rejects it:\n  {line}"
            );
            // Every requested title must get its own job — an example that
            // asks for two titles and silently produces one is still wrong.
            if flags.title_nums.len() > 1 {
                assert_eq!(
                    jobs.as_ref().unwrap().len(),
                    flags.title_nums.len(),
                    "usage.ex.{key}: {} titles requested, {} job(s) built:\n  {line}",
                    flags.title_nums.len(),
                    jobs.as_ref().unwrap().len()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn no_two_help_examples_show_the_same_command() {
        // `rip_iso_mp` and `rip_iso_patch` rendered byte-identical commands under
        // different descriptions, so `--help` showed the same invocation twice and
        // one of the two descriptions was necessarily wrong about it.
        let mut seen: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
        for (key, line) in shipped_rip_examples() {
            let cmd = example_argv(&line).join(" ");
            if let Some(prev) = seen.insert(cmd.clone(), key) {
                panic!(
                    "usage.ex.{prev} and usage.ex.{key} print the SAME command \
                     under different descriptions: `freemkv {cmd}`"
                );
            }
        }
    }

    // ── `-t` default (1.6.0): main title unless `-t N` / `-t all` ──────────
    // (normalization lives in `run()`; this pins the parse layer only)

    // See docs/pipe.md#t_all_on_a_disc_expands_to_every_title — `-t all` on a DISC must expand to…
    #[test]
    fn t_all_on_a_disc_expands_to_every_title() {
        // The whole disc.
        assert_eq!(disc_title_nums(true, &[], 12), (1..=12).collect::<Vec<_>>());
        assert_eq!(disc_title_nums(true, &[], 1), vec![1]);

        // An explicit selection wins — `-t 2 -t 5 --title all` keeps the two.
        assert_eq!(disc_title_nums(true, &[2, 5], 12), vec![2, 5]);
        // Without `-t all` nothing is expanded, whatever the count.
        assert_eq!(disc_title_nums(false, &[3], 12), vec![3]);
        assert_eq!(disc_title_nums(false, &[], 12), Vec::<usize>::new());
        // A scan that found nothing expands to nothing rather than [1..=0].
        assert_eq!(disc_title_nums(true, &[], 0), Vec::<usize>::new());
    }

    /// The expansion must route into the multi-title DISC arm of `build_jobs`,
    /// one job per title — not the single-job catch-all.
    #[test]
    fn an_expanded_disc_selection_builds_one_job_per_title() {
        let out = Output::new(false, true);
        let dir = temp_path("t-all-jobs");
        let dest = &format!("{}/", dir.display());
        let parsed = libfreemkv::parse_url(dest);
        let nums = disc_title_nums(true, &[], 4);
        let jobs = build_jobs(&None, true, &nums, true, dest, &parsed, &out)
            .expect("a directory dest accepts a multi-title disc rip");
        assert_eq!(jobs.len(), 4, "expected one job per title, got {jobs:?}");
        // 1-based flags map onto 0-based indices, in order.
        let idx: Vec<Option<usize>> = jobs.iter().map(|(i, _)| *i).collect();
        assert_eq!(idx, vec![Some(0), Some(1), Some(2), Some(3)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A single-title disc collapses to one job — same as `-t 1`, not a
    /// directory of one.
    #[test]
    fn t_all_on_a_one_title_disc_is_a_single_job() {
        let out = Output::new(false, true);
        let dest = &temp_path("t-all-one.mkv").display().to_string();
        let parsed = libfreemkv::parse_url(dest);
        let nums = disc_title_nums(true, &[], 1);
        let jobs = build_jobs(&None, true, &nums, false, dest, &parsed, &out)
            .expect("a single title may go to a single file");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, Some(0));
    }

    // See docs/pipe.md#the_t_default_normalizes_to_the_main_title_only — The `-t` DEFAULT, at the layer that actually…
    #[test]
    fn the_t_default_normalizes_to_the_main_title_only() {
        // The rule `run()` itself applies, called directly. It used to be
        // re-stated as a local copy here, which proved nothing about `run()` —
        // both mutants of the real line survived this test.
        fn normalize(mut nums: Vec<usize>, all_titles: bool) -> Vec<usize> {
            super::normalize_title_nums(&mut nums, all_titles);
            nums
        }
        // No flags at all -> main title only, NOT every title.
        assert_eq!(normalize(vec![], false), vec![1]);
        // `-t all` must NOT be normalized to [1]; empty means all-titles
        // downstream, and injecting [1] here would silently rip one title.
        assert_eq!(normalize(vec![], true), Vec::<usize>::new());
        // An explicit selection is left exactly as given.
        assert_eq!(normalize(vec![3], false), vec![3]);
        assert_eq!(normalize(vec![2, 5], false), vec![2, 5]);
        // `-t all` alongside explicit numbers keeps the numbers.
        assert_eq!(normalize(vec![2], true), vec![2]);
    }

    #[test]
    fn t_all_sets_all_titles_flag_and_no_nums() {
        let f = parse_flags(&["-t".into(), "all".into()]).unwrap();
        assert!(f.all_titles, "-t all must set all_titles");
        assert!(
            f.title_nums.is_empty(),
            "-t all carries no explicit numbers"
        );
    }

    #[test]
    fn t_all_is_case_insensitive() {
        assert!(
            parse_flags(&["-t".into(), "ALL".into()])
                .unwrap()
                .all_titles
        );
        assert!(
            parse_flags(&["--title".into(), "All".into()])
                .unwrap()
                .all_titles
        );
    }

    #[test]
    fn no_title_flag_leaves_empty_nums_and_not_all() {
        // run() then normalizes this to [1] (main title). parse_flags itself
        // leaves it empty + all_titles=false — the state the default keys off.
        let f = parse_flags(&["--raw".into()]).unwrap();
        assert!(f.title_nums.is_empty());
        assert!(!f.all_titles);
    }

    #[test]
    fn explicit_t_number_still_parses() {
        let f = parse_flags(&["-t".into(), "3".into()]).unwrap();
        assert_eq!(f.title_nums, vec![3]);
        assert!(!f.all_titles);
    }

    #[test]
    fn t_zero_still_rejected() {
        // `-t 0` remains invalid (1-based); `all` is the way to get everything.
        assert!(parse_flags(&["-t".into(), "0".into()]).is_err());
    }

    // ── `-a`/`-s` stream selection ─────────────────────────────────────────────
    use freemkv_engine::{StreamFilter, SubtitleFilter};

    #[test]
    fn absent_a_s_flags_default_to_all() {
        let f = parse_flags(&["--raw".into()]).unwrap();
        assert_eq!(f.streams.audio, StreamFilter::All);
        assert_eq!(f.streams.subtitles, StreamFilter::All.into());
        assert!(f.streams.is_all());
    }

    #[test]
    fn audio_langs_parse_into_a_lang_list() {
        let f = parse_flags(&["-a".into(), "eng,spa".into()]).unwrap();
        assert_eq!(
            f.streams.audio,
            StreamFilter::Langs(vec!["eng".into(), "spa".into()])
        );
        // subtitles untouched.
        assert_eq!(f.streams.subtitles, StreamFilter::All.into());
    }

    #[test]
    fn subtitle_flag_sets_only_subtitles() {
        let f = parse_flags(&["-s".into(), "English".into()]).unwrap();
        assert_eq!(
            f.streams.subtitles,
            SubtitleFilter::from(StreamFilter::Langs(vec!["English".into()]))
        );
        assert_eq!(f.streams.audio, StreamFilter::All);
    }

    #[test]
    fn spec_keywords_all_and_none_are_case_insensitive() {
        assert_eq!(parse_stream_spec("all"), StreamFilter::All);
        assert_eq!(parse_stream_spec("ALL"), StreamFilter::All);
        assert_eq!(parse_stream_spec("none"), StreamFilter::None);
        assert_eq!(parse_stream_spec("None"), StreamFilter::None);
    }

    #[test]
    fn spec_trims_and_drops_empty_langs() {
        assert_eq!(
            parse_stream_spec(" eng , , spa "),
            StreamFilter::Langs(vec!["eng".into(), "spa".into()])
        );
    }

    #[test]
    fn a_flag_value_is_not_swallowed_as_a_url() {
        // `-a eng` between two URLs: the value must be consumed, not left to be
        // mistaken for a positional stream URL.
        let f = parse_flags(&["-a".into(), "eng".into(), "iso://x".into()]).unwrap();
        assert_eq!(f.streams.audio, StreamFilter::Langs(vec!["eng".into()]));
    }

    #[test]
    fn a_flag_needs_a_value() {
        // `-a` with a URL immediately after (no value) is an error.
        assert!(parse_flags(&["-a".into(), "iso://x".into(), "mkv://o".into()]).is_err());
    }

    // The decrypt no-key verdict matrix now lives in `libfreemkv::Disc::
    // ensure_decryptable[_keys]`; no-raw-code-leak is covered below.

    // See docs/pipe.md#pipefail_classifies_via_the_engine — `PipeFail::from_mux` classifies the typed `io::Error` `mux_stream` returns
    #[test]
    fn pipefail_classifies_via_the_engine() {
        use freemkv_engine::TitleResult;
        let r = |e: libfreemkv::Error| PipeFail::from_mux(e.into()).result;
        // Skippable stubs (E7023 CssKeyMissing, E6008 MkvInvalid).
        assert_eq!(
            r(libfreemkv::Error::CssKeyMissing),
            TitleResult::SkippableStub
        );
        assert_eq!(r(libfreemkv::Error::MkvInvalid), TitleResult::SkippableStub);
        // Disc-level no-key (E7022 NoDiscKey, E7000 AacsNoKeys) → fail-fast.
        assert_eq!(
            r(libfreemkv::Error::NoDiscKey {
                disc_hash: "abcd1234".into()
            }),
            TitleResult::DiscLevelNoKey
        );
        assert_eq!(
            r(libfreemkv::Error::AacsNoKeys),
            TitleResult::DiscLevelNoKey
        );
        // A real non-stub failure is Failed (never silently skipped).
        assert_eq!(r(libfreemkv::Error::NoStreams), TitleResult::Failed);
        assert_eq!(
            PipeFail::from_mux(std::io::Error::other("boom")).result,
            TitleResult::Failed
        );
        // Halt (Ctrl-C) and typed-error constructors classify correctly too.
        assert_eq!(
            PipeFail::halted("interrupted".into()).result,
            TitleResult::Halted
        );
        assert_eq!(
            PipeFail::from_typed(libfreemkv::Error::NoDiscKey {
                disc_hash: "x".into()
            })
            .result,
            TitleResult::DiscLevelNoKey
        );
        // A plain fatal setup failure is Failed.
        assert_eq!(PipeFail::fatal("boom".into()).result, TitleResult::Failed);
    }

    // The skip/stop/fail POLICY (decide_title) is unit-tested in freemkv-engine;
    // the CLI only classifies PipeFail into a TitleResult (above), so it doesn't re-test the policy.

    // See docs/pipe.md#metadata_sink_detected_for_chapters_and_json — `chapters://` / `json://` are metadata sinks. `mux_stream` short-circuits
    #[test]
    fn metadata_sink_detected_for_chapters_and_json() {
        assert!(is_metadata_sink("chapters:///tmp/out.xml"));
        assert!(is_metadata_sink("json:///tmp/out.json"));
        assert!(!is_metadata_sink("mkv:///tmp/out.mkv"));
        assert!(!is_metadata_sink("null://"));
    }

    /// The skip warning the loop prints (`rip.title_skipped`) exists in en.json
    /// and carries the `{num}` placeholder — so a skipped title surfaces a clear,
    /// localized, non-error message rather than a raw E7023.
    #[test]
    fn rip_title_skipped_string_present_and_localized() {
        let s = strings::fmt("rip.title_skipped", &[("num", "3")]);
        assert_ne!(s, "rip.title_skipped", "missing locale entry");
        assert!(s.contains('3'), "num placeholder not substituted: {s}");
        assert!(
            !s.contains("E7023") && !s.to_lowercase().contains("error"),
            "skip notice must not look like a hard error: {s}"
        );
    }

    // See docs/pipe.md#disc_copy_recovered_data_gates_zero_recovery — The disc→ISO sweep-success guard. `Disc::copy` returns `Ok` even…
    #[test]
    fn disc_copy_recovered_data_gates_zero_recovery() {
        // Whole disc unreadable → no data recovered → not a success.
        assert!(!disc_copy_recovered_data(0));
        // Any recovered bytes → success.
        assert!(disc_copy_recovered_data(1));
        assert!(disc_copy_recovered_data(50_000_000_000));
    }

    // The header-resolution gate that used to live in the CLI now lives in
    // `libfreemkv::mux::mux_stream`, covered by its own tests there.

    // ── fmt_err generalization (english errors for ALL codes) ───────────────

    /// `parse_error_code` splits the libfreemkv `E<code>[: <data>]` Display
    /// form into the code token and its trailing data.
    #[test]
    fn parse_error_code_splits_code_and_data() {
        assert_eq!(parse_error_code("E6009"), Some(("E6009", "")));
        assert_eq!(parse_error_code("E7022: abcdef"), Some(("E7022", "abcdef")));
        assert_eq!(parse_error_code("E5000: 13"), Some(("E5000", "13")));
        // Not an E-code: returns None (falls through to the generic wrapper).
        assert_eq!(parse_error_code("No drive found"), None);
        assert_eq!(parse_error_code("Error: boom"), None);
        assert_eq!(parse_error_code("E"), None);
        assert_eq!(parse_error_code("Eabc"), None);
    }

    // See docs/pipe.md#fmt_err_renders_codes_to_english — A representative sample of codes must render to…
    #[test]
    fn fmt_err_renders_codes_to_english() {
        // E6009 NoStreams — the Theme A zero-output error. Code now prefixed,
        // message dejargoned to the user-facing "no audio or video streams".
        let s = fmt_err_str("E6009");
        assert!(s.starts_with("E6009 "), "code not prefixed: {s}");
        assert!(
            s.to_lowercase().contains("no audio or video streams"),
            "got: {s}"
        );

        // E7023 CssKeyMissing — the Theme B CSS gate error. The user-facing
        // copy is dejargoned: "copy-protected", not "CSS title key".
        let s = fmt_err_str("E7023");
        assert!(s.starts_with("E7023 "), "code not prefixed: {s}");
        assert!(s.to_lowercase().contains("copy-protected"), "got: {s}");

        // E9023 MuxEmpty — the Theme A m2ts zero-frame error. Dejargoned to
        // "empty file" / "video or audio", not the internal "mux" term.
        let s = fmt_err_str("E9023");
        assert!(s.starts_with("E9023 "), "code not prefixed: {s}");
        assert!(s.to_lowercase().contains("empty file"), "got: {s}");

        // E5000 with data → {detail} substituted, code prefixed.
        let s = fmt_err_str("E5000: 13");
        assert!(s.starts_with("E5000 "), "code not prefixed: {s}");
        assert!(s.contains("13"), "detail not substituted: {s}");

        // E7013 Decryption failed — code now prefixed.
        let s = fmt_err_str("E7013");
        assert!(s.starts_with("E7013 "), "code not prefixed: {s}");
        assert!(s.to_lowercase().contains("decryption failed"), "got: {s}");

        // E7022 names the disc by hash, code prefixed.
        let s = fmt_err_str("E7022: deadbeef");
        assert!(s.starts_with("E7022 "), "code not prefixed: {s}");
        assert!(s.contains("deadbeef"), "hash not substituted: {s}");
    }

    // See docs/pipe.md#key_service_failures_do_not_render_as_a_missing_disc_key — What the operator actually reads. During a seven-hour…
    #[test]
    fn key_service_failures_do_not_render_as_a_missing_disc_key() {
        let missing = fmt_err_str("E7022: 422eb0");
        let unavailable = fmt_err_str("E7028");
        let unauthorized = fmt_err_str("E7029");
        let rate_limited = fmt_err_str("E7030");

        for (code, s) in [
            ("E7028", &unavailable),
            ("E7029", &unauthorized),
            ("E7030", &rate_limited),
        ] {
            assert!(s.starts_with(&format!("{code} ")), "code not prefixed: {s}");
            // Not the generic wrapper — a real locale entry exists.
            assert!(
                !s.contains(&format!("error.{code}")),
                "{code} fell through to the raw key path: {s}"
            );
            assert_ne!(*s, missing, "{code} must not reuse E7022's message");
            // Never the sentence that sent the operator hunting for a VUK.
            assert!(
                !s.to_lowercase()
                    .contains("no key source has a decryption key"),
                "{code} must not claim the disc has no key: {s}"
            );
        }

        // Each names its own action, and the three are distinct from each other.
        assert!(
            unavailable.to_lowercase().contains("try again"),
            "E7028 must tell the operator to retry: {unavailable}"
        );
        assert!(
            unauthorized.to_lowercase().contains("token"),
            "E7029 must point at the credentials: {unauthorized}"
        );
        assert!(
            rate_limited.to_lowercase().contains("rate-limiting"),
            "E7030 must name the rate limit: {rate_limited}"
        );
        assert_ne!(unavailable, unauthorized);
        assert_ne!(unauthorized, rate_limited);
    }

    /// The full render-site output: `render_error` prefixes the `Error:` level
    /// word exactly once onto the `E<code> <message>` fragment (WS2 §2.1).
    #[test]
    fn render_error_prefixes_level_once() {
        let rendered = render_error(&"E6009");
        assert!(rendered.starts_with("Error: E6009 "), "got: {rendered}");
        // The level word appears exactly once (no nested doubling).
        assert_eq!(rendered.matches("Error:").count(), 1);
    }

    /// E6000 (DiscRead) Display is `E6000: <sector> 0x..status../0x..sense..`.
    /// The status/sense hex tail is diagnostic noise that must NOT reach the
    /// user — only the sector number is substituted into the localized message.
    #[test]
    fn fmt_err_e6000_strips_status_sense_hex_tail() {
        // Full DiscRead Display: sector + status + sense triple. The code is
        // now shown as a prefix; the status/sense hex tail is still stripped.
        let s = fmt_err_str("E6000: 7476928 0x02/0x03/0x11/0x00");
        assert!(s.starts_with("E6000 "), "code not prefixed: {s}");
        assert!(s.contains("7476928"), "sector number lost: {s}");
        assert!(!s.contains("0x"), "raw hex tail leaked to user: {s}");
        // Sense-only form (no status byte) also strips the tail.
        let s = fmt_err_str("E6000: 100 0x03/0x11/0x00");
        assert!(s.contains("100") && !s.contains("0x"), "got: {s}");
        // Bare sector (no tail at all) renders cleanly.
        let s = fmt_err_str("E6000: 42");
        assert!(s.contains("42") && !s.contains("0x"), "got: {s}");
    }

    // See docs/pipe.md#fmt_err_unknown_code_uses_generic_wrapper — A code with NO locale entry falls back…
    #[test]
    fn fmt_err_unknown_code_uses_generic_wrapper() {
        // E1234 has no locale entry; the generic wrapper keeps the code.
        let s = fmt_err_str("E1234: whatever");
        assert_eq!(s, "E1234 whatever");
        // Through the render site the code is still shown with the level word.
        assert_eq!(render_error(&"E1234: whatever"), "Error: E1234 whatever");
    }

    /// A non-code error string (e.g. a CLI-side message) passes through the
    /// generic wrapper with an empty code, so `fmt_err_str` yields the bare
    /// string and the render site prefixes the level word.
    #[test]
    fn fmt_err_non_code_string_uses_generic() {
        // Empty code → leading space trimmed away by the render contract; the
        // fragment carries just the message.
        let s = fmt_err_str("No BD drive found");
        assert!(s.contains("No BD drive found"), "got: {s}");
        assert!(!s.contains('E'), "no spurious code token: {s}");
        assert_eq!(
            render_error(&"No BD drive found"),
            "Error: No BD drive found"
        );
    }

    // ── negative path: no-keydb AACS disc → E7022 surfaced in English ───────

    // See docs/pipe.md#no_keydb_aacs_disc_surfaces_e7022_in_english — End-to-end negative-path coverage: when the decrypt gate
    #[test]
    fn no_keydb_aacs_disc_surfaces_e7022_in_english() {
        // The error pipe_disc returns, rendered for the user.
        let disp = libfreemkv::Error::NoDiscKey {
            disc_hash: "deadbeefcafe".to_string(),
        }
        .to_string();
        assert!(
            disp.starts_with("E7022"),
            "library Display is E7022: {disp}"
        );
        let rendered = fmt_err_str(&disp);
        // English, names the disc by hash, code SHOWN (WS2: code-forward).
        assert!(rendered.contains("deadbeefcafe"), "hash named: {rendered}");
        assert!(
            rendered.starts_with("E7022 "),
            "code not prefixed: {rendered}"
        );
        assert!(
            rendered.to_lowercase().contains("key"),
            "english key message: {rendered}"
        );
    }

    #[test]
    fn copy_halts_on_first_interrupt() {
        // The Ctrl-C fix: the copy progress callback must return false (halt) the
        // moment SIGINT is seen, so the first Ctrl-C stops the sweep and the
        // tray unlocks on drop — rather than being ignored until `_exit(130)`.
        assert!(copy_should_continue(false), "no interrupt → keep going");
        assert!(!copy_should_continue(true), "interrupt → halt the copy");
    }

    // The `mux_was_interrupted` check that used to live here is gone: the CLI
    // no longer runs the frame loop. `mux_stream` polls `libfreemkv::Halt` and
    // reports interrupts via `MuxOutcome`, mapped to `interrupted_error`.

    #[test]
    fn work_pct_is_finite_when_work_total_zero() {
        // `print_disc_progress` now derives `pct` from `PassProgress::work_pct()`,
        // which guards `work_total == 0` (returns 100.0). The old inline
        // `work_done / work_total` produced `NaN%` for an empty Sweep/Mux pass.
        let p = libfreemkv::progress::PassProgress {
            kind: libfreemkv::progress::PassKind::Sweep,
            work_done: 0,
            work_total: 0,
            bytes_good_total: 0,
            bytes_unreadable_total: 0,
            bytes_pending_total: 0,
            bytes_retryable_total: 0,
            bytes_total_disc: 0,
            disc_duration_secs: None,
            bytes_bad_in_main_title: 0,
            main_title_duration_secs: None,
            main_title_size_bytes: None,
            located: Default::default(),
        };
        let pct = p.work_pct();
        assert!(pct.is_finite(), "work_total==0 must not yield NaN%");
        assert_eq!(pct, 100.0);
    }

    // See docs/pipe.md#disc_damage_unread_is_not_lost — A healthy in-progress rip (zero read errors, large…
    #[test]
    fn disc_damage_unread_is_not_lost() {
        let p = libfreemkv::progress::PassProgress {
            kind: libfreemkv::progress::PassKind::Sweep,
            work_done: 800,
            work_total: 10_000,
            bytes_good_total: 800,
            bytes_unreadable_total: 0,
            // 92% of the disc not yet read — large pending, but ZERO failed.
            bytes_pending_total: 9_200,
            bytes_retryable_total: 0,
            bytes_total_disc: 10_000,
            disc_duration_secs: Some(7200.0),
            bytes_bad_in_main_title: 0,
            main_title_duration_secs: Some(7200.0),
            main_title_size_bytes: Some(10_000),
            located: Default::default(),
        };
        let damage = fmt_disc_damage(&p);
        assert_eq!(
            damage,
            strings::get("rip.damage_none"),
            "unread sectors must not count as lost; got {damage:?}"
        );
        assert!(
            !damage.contains("lost"),
            "healthy rip must not render a 'lost' string; got {damage:?}"
        );
    }

    /// Sectors that actually FAILED to read (unreadable, or retryable =
    /// NonTrimmed/NonScraped awaiting retry) DO count as lost.
    #[test]
    fn disc_damage_failed_reads_are_lost() {
        // Retryable (failed-awaiting-retry) alone triggers "lost".
        let p_retryable = libfreemkv::progress::PassProgress {
            kind: libfreemkv::progress::PassKind::Sweep,
            work_done: 5_000,
            work_total: 10_000,
            bytes_good_total: 4_900,
            bytes_unreadable_total: 0,
            bytes_pending_total: 5_100,
            bytes_retryable_total: 100,
            bytes_total_disc: 10_000,
            disc_duration_secs: Some(7200.0),
            bytes_bad_in_main_title: 0,
            main_title_duration_secs: Some(7200.0),
            main_title_size_bytes: Some(10_000),
            located: Default::default(),
        };
        let damage = fmt_disc_damage(&p_retryable);
        assert!(
            damage.contains("lost"),
            "failed-awaiting-retry must render a 'lost' string; got {damage:?}"
        );
        assert_ne!(damage, strings::get("rip.damage_none"));

        // Unreadable (gave up) alone also triggers "lost".
        let p_unreadable = libfreemkv::progress::PassProgress {
            bytes_unreadable_total: 100,
            bytes_retryable_total: 0,
            ..p_retryable
        };
        assert!(
            fmt_disc_damage(&p_unreadable).contains("lost"),
            "unreadable bytes must render a 'lost' string"
        );
    }

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn stream_info_uses_dedicated_keys() {
        // Regression: `print_stream_info` mislabeled the track count with
        // `disc.titles` and the runtime with `disc.format`. Both now have
        // dedicated keys, which must not equal their own dotted path (a miss).
        assert_ne!(crate::strings::get("disc.streams"), "disc.streams");
        assert_ne!(crate::strings::get("disc.duration"), "disc.duration");
        // And they must be distinct from the keys they were confused with, so a
        // future copy-paste can't silently re-alias them.
        assert_ne!(
            crate::strings::get("disc.streams"),
            crate::strings::get("disc.titles")
        );
        assert_ne!(
            crate::strings::get("disc.duration"),
            crate::strings::get("disc.format")
        );
    }

    #[test]
    fn url_token_detection() {
        assert!(is_url_token("disc://"));
        assert!(is_url_token("mkv://out.mkv"));
        assert!(!is_url_token("1"));
        assert!(!is_url_token("keydb.cfg"));
        assert!(!is_url_token("/path/out.mkv"));
    }

    #[test]
    fn title_one_based_value_accepted() {
        let f = parse_flags(&v(&["-t", "1", "-t", "3"])).unwrap();
        assert_eq!(f.title_nums, vec![1, 3]);
    }

    #[test]
    fn duplicate_title_flags_dedup() {
        // `-t 1 -t 1` must collapse to a single title, not two jobs that both
        // map to the same index and overwrite the same output file.
        let f = parse_flags(&v(&["-t", "1", "-t", "1"])).unwrap();
        assert_eq!(f.title_nums, vec![1]);
        // Out-of-order repeats sort + dedup deterministically.
        let f = parse_flags(&v(&["-t", "3", "-t", "1", "-t", "3"])).unwrap();
        assert_eq!(f.title_nums, vec![1, 3]);
    }

    #[test]
    fn disc_multiple_titles_build_one_job_each() {
        // Regression (HIGH): multiple `-t` on a disc source must build one
        // job per requested title, not silently drop all but the first.
        let out = Output::new(false, true);
        // Repo-local scratch (not /tmp): survives reboots and stays inside the
        // build tree so stray dirs are obvious and cleaned by `cargo clean`.
        let dest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/test-scratch")
            .join(format!("freemkv_test_{}", std::process::id()));
        let dest = format!("mkv://{}", dest_dir.display());
        let parsed_dest = libfreemkv::parse_url(&dest);

        let jobs = build_jobs(
            &None,
            true, // is_disc
            &[1usize, 3usize],
            true, // is_dir_dest — multiple titles require a directory dest
            &dest,
            &parsed_dest,
            &out,
        )
        .expect("dir creation should succeed in temp");

        assert_eq!(jobs.len(), 2, "both -t 1 and -t 3 must produce a job");
        // Title indices are 0-based: -t 1 → 0, -t 3 → 2.
        assert_eq!(jobs[0].0, Some(0));
        assert_eq!(jobs[1].0, Some(2));
        // Distinct output files (no silent overwrite / drop).
        assert_ne!(jobs[0].1, jobs[1].1);
        assert!(jobs[0].1.contains("_t1."), "got {}", jobs[0].1);
        assert!(jobs[1].1.contains("_t3."), "got {}", jobs[1].1);

        let _ = std::fs::remove_dir_all(&dest_dir);
    }

    #[test]
    fn disc_multiple_titles_to_file_dest_rejected() {
        // Regression (MEDIUM): a disc multi-title rip to a single-FILE dest
        // used to fall into dir_jobs, silently turning `movie.mkv` into a
        // directory. Must now be rejected (build returns None) instead.
        let out = Output::new(false, true);
        // A unique temp path, not the process CWD or a shared name: the
        // assertion below is that a directory does NOT exist, so a stray
        // one from another test process would fail this for the wrong reason.
        let file = temp_path("multi-to-file").join("movie.mkv");
        let _ = std::fs::remove_dir_all(&file);
        let dest = format!("mkv://{}", file.display());
        let parsed_dest = libfreemkv::parse_url(&dest);
        let jobs = build_jobs(
            &None,
            true, // is_disc
            &[1usize, 2usize],
            false, // is_dir_dest — a single file can't hold two titles
            &dest,
            &parsed_dest,
            &out,
        );
        assert!(
            jobs.is_none(),
            "multi-title disc to a file dest must be rejected, not silently turned into a dir"
        );
        // The file must NOT have been created as a directory.
        assert!(
            !file.is_dir(),
            "must not have created a directory at the file dest"
        );
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn out_of_range_title_is_failure() {
        // Regression (HIGH): an explicit `-t` past the last title must be a hard
        // failure (caller sets ok=false → non-zero exit), not a warning that
        // still exits 0. title_in_range gates that branch.
        assert!(title_in_range(0, 3), "first title is in range");
        assert!(title_in_range(2, 3), "last title is in range");
        assert!(!title_in_range(3, 3), "one past the end is out of range");
        assert!(!title_in_range(99, 3), "far past the end is out of range");
        assert!(!title_in_range(0, 0), "no titles → any index out of range");
        // `pipe_disc` applies the SAME rule through this function now; it used
        // to spell the comparison out inline, so hardening one copy left the
        // other open, risking an index panic mid-rip if inverted.
    }

    #[test]
    fn disc_single_title_is_single_file_job() {
        // A single `-t` on a disc keeps the one-file path (no directory).
        let out = Output::new(false, true);
        let parsed_dest = libfreemkv::parse_url("mkv://out.mkv");
        let jobs = build_jobs(
            &None,
            true,
            &[2usize],
            false,
            "mkv://out.mkv",
            &parsed_dest,
            &out,
        )
        .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, Some(1));
        assert_eq!(jobs[0].1, "mkv://out.mkv");
    }

    #[test]
    fn title_zero_rejected() {
        // `-t 0` must not underflow to all-titles; it's an explicit error.
        let err = parse_flags(&v(&["-t", "0"])).unwrap_err();
        assert!(err.contains('0'), "got: {err}");
    }

    #[test]
    fn title_non_numeric_rejected() {
        // A bad value must NOT silently leave title_nums empty (= all titles).
        let err = parse_flags(&v(&["-t", "main"])).unwrap_err();
        assert!(!err.is_empty());
    }

    #[test]
    fn title_missing_value_rejected() {
        assert!(parse_flags(&v(&["-t"])).is_err());
        // Followed by a URL → value is missing, not the URL.
        assert!(parse_flags(&v(&["-t", "disc://"])).is_err());
    }

    #[test]
    fn keydb_missing_value_rejected() {
        // `--keydb` with no value must not silently fall back to the default keydb.
        assert!(parse_flags(&v(&["--keydb"])).is_err());
        assert!(parse_flags(&v(&["--keydb", "disc://"])).is_err());
    }

    #[test]
    fn keydb_value_accepted() {
        let f = parse_flags(&v(&["--keydb", "/etc/keydb.cfg"])).unwrap();
        assert_eq!(f.keydb_path.as_deref(), Some("/etc/keydb.cfg"));
    }

    // ── Online key-source flags ────────────────────────────────────────────

    #[test]
    fn is_keyserver_url_accepts_http_only() {
        assert!(is_keyserver_url("http://keys.example/keys"));
        assert!(is_keyserver_url("https://keys.example/keys"));
        // A stream URL with a non-http scheme is NOT a key-service URL value.
        assert!(!is_keyserver_url("disc://"));
        assert!(!is_keyserver_url("mkv://out.mkv"));
        assert!(!is_keyserver_url("ftp://x/keys"));
        assert!(!is_keyserver_url("--quiet"));
    }

    #[test]
    fn key_url_and_auth_parse() {
        let f = parse_flags(&v(&[
            "--key-url",
            "https://keys.example/keys",
            "--key-auth",
            "tok123",
        ]))
        .unwrap();
        assert_eq!(f.key_url.as_deref(), Some("https://keys.example/keys"));
        assert_eq!(f.key_auth.as_deref(), Some("tok123"));
    }

    #[test]
    fn key_url_missing_or_non_http_value_rejected() {
        // No value at all.
        assert!(parse_flags(&v(&["--key-url"])).is_err());
        // A following stream URL with a non-http scheme is NOT the value —
        // value is missing (must not eat the positional `disc://`).
        assert!(parse_flags(&v(&["--key-url", "disc://"])).is_err());
        // A following flag means the value is missing.
        assert!(parse_flags(&v(&["--key-url", "--quiet"])).is_err());
    }

    // ── VAL-2 regression: --key-url scheme validation ──────────────────────
    // Bug: the guard was `!is_url_token(u)`, making the bad-scheme branch
    // `A && !A` — dead code. Fix: guard on `is_keyserver_url(u)` instead.

    /// VAL-2: `--key-url ftp://x` — a non-http(s) scheme — must produce the
    /// bad-scheme error, NOT "requires a value" (the value was present).
    #[test]
    fn val2_key_url_ftp_scheme_gives_bad_scheme_error() {
        let err = parse_flags(&v(&["--key-url", "ftp://x"])).unwrap_err();
        // Must contain the bad-scheme message substring, not the generic
        // "requires a value" substring.
        assert!(
            err.contains("http://") || err.contains("https://"),
            "expected bad-scheme error (mentioning http(s)://), got: {err}"
        );
        assert!(
            !err.contains("requires a value"),
            "must NOT produce flag_needs_value when a value was present: {err}"
        );
        // The bad URL itself must appear in the message so the user can see
        // what was rejected.
        assert!(
            err.contains("ftp://x"),
            "rejected URL missing from error: {err}"
        );
    }

    /// VAL-2: `--key-url disc://` — a stream scheme used as a key-url — must
    /// also produce the bad-scheme error. `disc://` contains `://` but is not
    /// http(s), so it goes through the bad-scheme arm, not the missing-value arm.
    #[test]
    fn val2_key_url_disc_scheme_gives_bad_scheme_error() {
        let err = parse_flags(&v(&["--key-url", "disc://"])).unwrap_err();
        assert!(
            err.contains("http://") || err.contains("https://"),
            "expected bad-scheme error (mentioning http(s)://), got: {err}"
        );
        assert!(
            !err.contains("requires a value"),
            "must NOT produce flag_needs_value when a value (with wrong scheme) was present: {err}"
        );
        assert!(
            err.contains("disc://"),
            "rejected URL missing from error: {err}"
        );
    }

    /// VAL-2 (positive path): `--key-url https://keys.example/keys` must be
    /// accepted and stored verbatim.
    #[test]
    fn val2_key_url_https_accepted() {
        let f = parse_flags(&v(&["--key-url", "https://keys.example/keys"])).unwrap();
        assert_eq!(
            f.key_url.as_deref(),
            Some("https://keys.example/keys"),
            "https key-url must be accepted and stored verbatim"
        );
    }

    /// VAL-2 (positive path): `--key-url http://keys.example/keys` (plain http)
    /// must also be accepted.
    #[test]
    fn val2_key_url_http_accepted() {
        let f = parse_flags(&v(&["--key-url", "http://keys.example/keys"])).unwrap();
        assert_eq!(
            f.key_url.as_deref(),
            Some("http://keys.example/keys"),
            "http key-url must be accepted and stored verbatim"
        );
    }

    /// VAL-2 (missing value): bare `--key-url` with no following token must
    /// produce the flag_needs_value error (not the bad-scheme error).
    #[test]
    fn val2_key_url_no_value_gives_needs_value_error() {
        let err = parse_flags(&v(&["--key-url"])).unwrap_err();
        assert!(
            err.contains("requires a value"),
            "bare --key-url must produce flag_needs_value, got: {err}"
        );
    }

    /// VAL-2 (missing value via flag): `--key-url --quiet` — the value is a
    /// flag, not a URL, so it is missing. Must produce flag_needs_value.
    #[test]
    fn val2_key_url_followed_by_flag_gives_needs_value_error() {
        let err = parse_flags(&v(&["--key-url", "--quiet"])).unwrap_err();
        assert!(
            err.contains("requires a value"),
            "--key-url followed by a flag must produce flag_needs_value, got: {err}"
        );
    }

    #[test]
    fn key_auth_missing_value_rejected() {
        assert!(parse_flags(&v(&["--key-auth"])).is_err());
        // A following stream URL means the token was omitted.
        assert!(parse_flags(&v(&["--key-auth", "disc://"])).is_err());
    }

    /// Source assembly per the agreed design — local-first ordering, pinned via
    /// each source's stable `label()` (`"keydb"` before `"online"`).
    #[test]
    fn build_key_sources_orders_local_first() {
        // keydb only → [Keydb]. (Default location is fine; we only inspect order.)
        let s = build_key_sources_quiet(&KeyConfig {
            keydb_path: Some("keydb.cfg".into()),
            key_url: None,
            key_auth: None,
        });
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].label(),
            "keydb",
            "keydb-only first source is the keydb"
        );

        // neither flag → still [Keydb] (default keydb location).
        let s = build_key_sources_quiet(&KeyConfig::default());
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label(), "keydb", "no flags → keydb only");

        // --key-url only → [Online] (no keydb consulted).
        let s = build_key_sources_quiet(&KeyConfig {
            keydb_path: None,
            key_url: Some("https://8.8.8.8/keys".into()),
            key_auth: None,
        });
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].label(),
            "online",
            "url-only first source is the online one"
        );

        // both → [Keydb, Online] — LOCAL-FIRST.
        let s = build_key_sources_quiet(&KeyConfig {
            keydb_path: Some("keydb.cfg".into()),
            key_url: Some("https://8.8.8.8/keys".into()),
            key_auth: Some("tok".into()),
        });
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].label(), "keydb", "local keydb is tried first");
        assert_eq!(s[1].label(), "online", "online service is the fallback");
    }

    // See docs/pipe.md#build_key_sources_drops_ssrf_rejected_url — SSRF guard: a `--key-url` that resolves to an…
    #[test]
    fn build_key_sources_drops_ssrf_rejected_url() {
        // url-only, metadata endpoint → rejected → zero sources.
        let s = build_key_sources_quiet(&KeyConfig {
            keydb_path: None,
            key_url: Some("http://169.254.169.254/latest/meta-data".into()),
            key_auth: None,
        });
        assert!(
            s.is_empty(),
            "SSRF-rejected url-only must add no online source"
        );

        // url-only, loopback → rejected → zero sources.
        let s = build_key_sources_quiet(&KeyConfig {
            keydb_path: None,
            key_url: Some("https://127.0.0.1:8443/keys".into()),
            key_auth: None,
        });
        assert!(s.is_empty(), "loopback url must be rejected");

        // keydb + rejected url → only the keydb survives.
        let s = build_key_sources_quiet(&KeyConfig {
            keydb_path: Some("keydb.cfg".into()),
            key_url: Some(format!("http://{}.{}.{}.{}/keys", 10, 0, 0, 5)),
            key_auth: None,
        });
        assert_eq!(s.len(), 1, "rejected url dropped; keydb remains");
        assert_eq!(s[0].label(), "keydb", "the surviving source is the keydb");
    }

    #[test]
    fn unknown_flag_is_rejected() {
        // Regression (MEDIUM): a typo'd flag (`--titel`, `--qiet`) used to fall
        // through the catch-all and be silently ignored — defaults used, exit 0.
        // It must now be a hard error.
        assert!(parse_flags(&v(&["--titel", "1"])).is_err());
        assert!(parse_flags(&v(&["--qiet"])).is_err());
        assert!(parse_flags(&v(&["-x"])).is_err());
        // The error names the offending flag.
        let err = parse_flags(&v(&["--bogus"])).unwrap_err();
        assert!(err.contains("--bogus"), "got: {err}");
        // Non-dash positionals (URLs, title values) are NOT rejected here.
        assert!(parse_flags(&v(&["disc://", "mkv://out.mkv"])).is_ok());
        assert!(parse_flags(&v(&["-t", "1", "disc://"])).is_ok());
    }

    #[test]
    fn boolean_flags_parse() {
        // `--log-level 2` (info) widens prose detail → verbose.
        let f = parse_flags(&v(&["--raw", "--multipass", "--log-level", "2", "-q"])).unwrap();
        assert!(f.raw && f.multipass && f.verbose && f.quiet);
        assert!(f.title_nums.is_empty());
        assert!(!f.force, "force defaults off");
    }

    #[test]
    fn force_flag_parses() {
        // `--force` opts into overwriting a non-empty dir:// target.
        let f = parse_flags(&v(&["--force"])).unwrap();
        assert!(f.force);
        assert!(!parse_flags(&v(&[])).unwrap().force);
    }

    // See docs/pipe.md#keydb_does_not_take_the_following_flag_as_its_path — `--keydb` must not accept the next FLAG as…
    #[test]
    fn keydb_does_not_take_the_following_flag_as_its_path() {
        let e = parse_flags(&v(&["--keydb", "--raw"]))
            .expect_err("a flag is not a keydb path — the value is missing");
        assert!(
            e.contains("--keydb"),
            "the error must name the flag whose value is missing: {e}"
        );
        // The real thing still parses, and still wins over nothing.
        let f = parse_flags(&v(&["--keydb", "/tmp/k.cfg", "--raw"])).unwrap();
        assert_eq!(f.keydb_path.as_deref(), Some("/tmp/k.cfg"));
        assert!(f.raw, "--raw must survive a well-formed --keydb");
    }

    // See docs/pipe.md#no_value_flag_takes_the_following_flag_as_its_value — EVERY value-taking flag must refuse the next FLAG…
    #[test]
    fn no_value_flag_takes_the_following_flag_as_its_value() {
        // Derived from VALUE_FLAGS, not hand-listed: a hand-listed version
        // omitted --log-file/--log-level, leaving the hole it closed reopened.
        // Exempts --key-auth (token may start with '-') and --key-url (own scheme check).
        for flag in crate::cli_entry::VALUE_FLAGS
            .iter()
            .copied()
            .filter(|f| !matches!(*f, "--key-auth" | "--key-url"))
        {
            // The property is that the following flag SURVIVES, not that a
            // particular error occurs: a missing value may reject or just
            // decline to consume it — but swallowing `--raw` decrypts unasked.
            match parse_flags(&v(&[flag, "--raw"])) {
                Err(e) => assert!(
                    e.contains(flag),
                    "{flag} rejected, but the error does not name it: {e}"
                ),
                Ok(f) => assert!(
                    f.raw,
                    "{flag} silently ate the following --raw, so the rip runs \
                     without it and writes a decrypted image"
                ),
            }
        }

        // ...and a well-formed value still parses, with the later flag intact.
        let f = parse_flags(&v(&["-a", "eng", "--raw"])).expect("a real value parses");
        assert!(f.raw, "--raw must survive a well-formed -a");
    }

    #[test]
    fn log_level_sets_verbose_at_or_above_two() {
        // Level 1 = quiet prose; 2/3/4 widen it. The numeric value must also be
        // consumed so it is never mistaken for a positional URL.
        assert!(!parse_flags(&v(&["--log-level", "1"])).unwrap().verbose);
        assert!(parse_flags(&v(&["--log-level", "2"])).unwrap().verbose);
        assert!(parse_flags(&v(&["--log-level", "4"])).unwrap().verbose);
    }

    #[test]
    fn schemeless_dest_is_unknown() {
        // Backs the `run()` guard that rejects a schemeless dest up front
        // instead of producing `name_t1.unknown` / `unknown://` outputs.
        assert!(matches!(
            libfreemkv::parse_url("out.mkv"),
            libfreemkv::StreamUrl::Unknown { .. }
        ));
        assert!(matches!(
            libfreemkv::parse_url("/path/out.mkv"),
            libfreemkv::StreamUrl::Unknown { .. }
        ));
        assert!(matches!(
            libfreemkv::parse_url("mkv://out.mkv"),
            libfreemkv::StreamUrl::Mkv { .. }
        ));
    }

    // Adversarial input battery: every bad-input class + combinations, each
    // asserting `preflight_validate` fails loud and early (Err, never panic,
    // never silent success) — the CLI maps Err to nonzero exit + no output.

    /// Run `preflight_validate` on a (source, dest, raw, multipass) tuple,
    /// parsing the URLs the same way `run()` does. Returns the Result so tests
    /// can assert Ok / Err without repeating the parse boilerplate.
    fn preflight(source: &str, dest: &str, raw: bool, multipass: bool) -> Result<(), String> {
        preflight_f(source, dest, raw, multipass, false)
    }

    /// `preflight` with an explicit `--force` value (for `dir://` non-empty
    /// target tests).
    fn preflight_f(
        source: &str,
        dest: &str,
        raw: bool,
        multipass: bool,
        force: bool,
    ) -> Result<(), String> {
        let ps = parse_url(source);
        let pd = parse_url(dest);
        preflight_validate(source, dest, &ps, &pd, raw, multipass, force, false)
    }

    /// `preflight` with title/stream selection flags marked as used.
    fn preflight_sel(source: &str, dest: &str) -> Result<(), String> {
        let ps = parse_url(source);
        let pd = parse_url(dest);
        preflight_validate(source, dest, &ps, &pd, false, false, false, true)
    }

    #[test]
    fn selection_flags_require_disc_or_iso_source() {
        // File/stream sources have no title list: -t/-a/-s must fail loud.
        assert!(preflight_sel("mkv://in.mkv", "mkv://out.mkv").is_err());
        assert!(preflight_sel("m2ts://in.m2ts", "mkv://out.mkv").is_err());
        assert!(preflight_sel("network://0.0.0.0:9000", "mkv://out.mkv").is_err());
        // disc:// IS scanned into titles: selection passes this gate. (iso://
        // shares the same is_disc_source() branch but needs a real file to
        // clear the later reachability check, so it isn't asserted here.)
        assert!(preflight_sel("disc://", &temp_dest("mkv", "sel_ok")).is_ok());
        // No selection flags: a plain file remux stays legal.
        assert!(
            preflight(
                "mkv://in.mkv",
                &temp_dest("mkv", "sel_noflags"),
                false,
                false
            )
            .is_ok()
        );
    }

    /// A unique temp path under the system temp dir (no tempfile dep). Caller
    /// is responsible for cleanup; non-existent by construction.
    pub(super) fn temp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("freemkv_test_{}_{}_{}", tag, std::process::id(), n))
    }

    // See docs/pipe.md#temp_dest — A destination URL whose path is unique and…
    fn temp_dest(scheme: &str, tag: &str) -> String {
        format!("{scheme}://{}", temp_path(tag).display())
    }

    // ── schemes ─────────────────────────────────────────────────────────────

    #[test]
    fn preflight_rejects_schemeless_dest() {
        let e = preflight("iso://in.iso", "out.mkv", false, false).unwrap_err();
        assert!(
            e.contains("scheme"),
            "must guide on missing dest scheme: {e}"
        );
    }

    #[test]
    fn preflight_rejects_schemeless_source() {
        // A real readable ISO dest is irrelevant — the schemeless SOURCE must be
        // caught first. Use a sink dest so dest validation can't mask it.
        let e = preflight("in.iso", "null://", false, false).unwrap_err();
        assert!(
            e.to_lowercase().contains("scheme"),
            "must guide on missing source scheme: {e}"
        );
    }

    #[test]
    fn preflight_rejects_unknown_dest_scheme() {
        // `gopher://x` parses to Unknown (no recognized scheme) → rejected.
        let e = preflight("null://", "gopher://x", false, false).unwrap_err();
        assert!(!e.is_empty());
    }

    // ── --raw / --multipass are iso://-only ─────────────────────────────────

    #[test]
    fn raw_rejected_on_mkv_dest() {
        let e = preflight("disc://", "mkv://out.mkv", true, false).unwrap_err();
        assert!(e.contains("--raw"), "names the offending flag: {e}");
        assert!(e.contains("iso://"), "points at the supported output: {e}");
    }

    #[test]
    fn raw_rejected_on_m2ts_and_null_and_stdio() {
        for dest in ["m2ts://o.m2ts", "null://", "stdio://"] {
            let e = preflight("disc://", dest, true, false)
                .expect_err(&format!("--raw on {dest} must error"));
            assert!(e.contains("--raw"), "{dest}: {e}");
        }
    }

    #[test]
    fn multipass_rejected_on_mkv_dest() {
        let e = preflight("disc://", "mkv://out.mkv", false, true).unwrap_err();
        assert!(e.contains("--multipass"), "names the flag: {e}");
        assert!(e.contains("iso://"), "points at iso://: {e}");
    }

    #[test]
    fn multipass_rejected_on_null_and_stdio_and_network() {
        for dest in ["null://", "stdio://", "network://host:9000"] {
            let e = preflight("disc://", dest, false, true)
                .expect_err(&format!("--multipass on {dest} must error"));
            assert!(e.contains("--multipass"), "{dest}: {e}");
        }
    }

    #[test]
    fn disc_to_mkv_raw_combination_errors() {
        // disc→mkv --raw: the explicit combination called out in the brief.
        assert!(preflight("disc://", "mkv://o.mkv", true, false).is_err());
    }

    #[test]
    fn disc_to_mkv_multipass_combination_errors() {
        assert!(preflight("disc://", "mkv://o.mkv", false, true).is_err());
    }

    #[test]
    fn disc_to_null_raw_and_multipass_error() {
        // disc→null --raw and disc→null --multipass: both error (iso://-only).
        assert!(preflight("disc://", "null://", true, false).is_err());
        assert!(preflight("disc://", "null://", false, true).is_err());
    }

    // ── dir:// (decrypted file-tree extraction) gates ───────────────────────

    /// `--raw` into a `dir://` dest is rejected (dir:// is not iso://, so the
    /// system-wide raw/iso-only gate fires). An encrypted file tree is useless.
    #[test]
    fn dir_dest_rejects_raw() {
        let out = temp_path("dir_raw");
        let dest = format!("dir://{}/", out.display());
        let e = preflight("disc://", &dest, true, false).expect_err("dir:// + --raw must error");
        assert!(e.contains("--raw"), "names the offending flag: {e}");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// `--multipass` into a `dir://` dest is rejected (dir:// is 1-shot;
    /// recovery is the iso:// multipass path's job).
    #[test]
    fn dir_dest_rejects_multipass() {
        let out = temp_path("dir_mp");
        let dest = format!("dir://{}/", out.display());
        let e =
            preflight("disc://", &dest, false, true).expect_err("dir:// + --multipass must error");
        assert!(e.contains("--multipass"), "names the flag: {e}");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A byte-stream source (no filesystem) into `dir://` is rejected up front
    /// — only disc:// / iso:// supply a UDF tree.
    #[test]
    fn dir_dest_rejects_byte_stream_source() {
        for src in [
            "mkv://in.mkv",
            "m2ts://in.m2ts",
            "network://host:9000",
            "stdio://",
        ] {
            let out = temp_path("dir_src");
            let dest = format!("dir://{}/", out.display());
            let e = preflight(src, &dest, false, false)
                .expect_err(&format!("{src} → dir:// should error"));
            assert!(
                e.to_lowercase().contains("dir://") || e.contains("file tree"),
                "{src}: {e}"
            );
            let _ = std::fs::remove_dir_all(&out);
        }
    }

    /// disc:// and iso:// SOURCES into dir:// pass the source gate (an iso://
    /// input still needs a readable file, supplied here).
    #[test]
    fn dir_dest_accepts_disc_and_iso_sources() {
        let out = temp_path("dir_ok");
        let dest = format!("dir://{}/", out.display());
        // disc:// (auto-detect device): source gate passes; dir target created.
        assert!(preflight("disc://", &dest, false, false).is_ok());
        let _ = std::fs::remove_dir_all(&out);

        // iso:// source needs a real, non-empty file.
        let iso = temp_path("dir_ok_iso");
        std::fs::write(&iso, b"not empty").unwrap();
        let out2 = temp_path("dir_ok2");
        let dest2 = format!("dir://{}/", out2.display());
        let src = format!("iso://{}", iso.display());
        assert!(preflight(&src, &dest2, false, false).is_ok());
        let _ = std::fs::remove_file(&iso);
        let _ = std::fs::remove_dir_all(&out2);
    }

    /// A non-empty `dir://` target is refused without `--force`, accepted with.
    #[test]
    fn dir_dest_non_empty_requires_force() {
        let out = temp_path("dir_nonempty");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("x.txt"), b"x").unwrap();
        let dest = format!("dir://{}/", out.display());
        let e =
            preflight_f("disc://", &dest, false, false, false).expect_err("non-empty must error");
        assert!(e.to_lowercase().contains("empty"), "{e}");
        // --force overrides.
        assert!(preflight_f("disc://", &dest, false, false, true).is_ok());
        let _ = std::fs::remove_dir_all(&out);
    }

    // See docs/pipe.md#dir_dest_existing_file_rejected_even_with_force — disc:// → dir:// where the dir:// target is…
    #[test]
    fn dir_dest_existing_file_rejected_even_with_force() {
        let f = temp_path("dir_isfile");
        std::fs::write(&f, b"i am a file, not a folder").unwrap();
        let dest = format!("dir://{}", f.display());

        // Without --force.
        let e = preflight("disc://", &dest, false, false)
            .expect_err("dir:// target that is a file must error");
        assert!(
            e.to_lowercase().contains("file") || e.to_lowercase().contains("folder"),
            "must explain the file/folder mismatch: {e}"
        );

        // --force must NOT rescue it — a regular file is still not a folder.
        let e2 = preflight_f("disc://", &dest, false, false, true)
            .expect_err("dir:// target that is a file must error even with --force");
        assert!(
            e2.to_lowercase().contains("file") || e2.to_lowercase().contains("folder"),
            "--force must not turn a file into a dir:// target: {e2}"
        );
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn raw_and_multipass_accepted_on_iso_dest() {
        // The legit case: iso:// destination accepts both flags. (Source is the
        // live drive, not pre-checked for existence here — device None.)
        assert!(preflight("disc://", &temp_dest("iso", "raw"), true, false).is_ok());
        assert!(preflight("disc://", &temp_dest("iso", "multipass"), false, true).is_ok());
        assert!(preflight("disc://", &temp_dest("iso", "both"), true, true).is_ok());
    }

    #[test]
    fn no_flags_accepted_on_non_iso_dest() {
        // Without the iso-only flags, a mux/sink dest is fine at preflight.
        assert!(preflight("disc://", &temp_dest("mkv", "noflags"), false, false).is_ok());
        assert!(preflight("disc://", "null://", false, false).is_ok());
    }

    // ── drive / device source ────────────────────────────────────────────────

    #[test]
    fn missing_device_path_errors_early() {
        // An explicit device path that doesn't exist must be caught before any
        // open. Use a sink dest so only the source check can fire.
        let e = preflight("disc:///dev/does-not-exist-xyz", "null://", false, false).unwrap_err();
        assert!(
            e.to_lowercase().contains("device") || e.contains("does-not-exist"),
            "must name the missing device: {e}"
        );
    }

    #[test]
    fn auto_detect_device_not_prechecked() {
        // `disc://` with no device path is auto-detect — left to find_drive, so
        // preflight must NOT error on it for source reachability.
        assert!(preflight("disc://", "null://", false, false).is_ok());
    }

    // ── ISO input ────────────────────────────────────────────────────────────

    #[test]
    fn iso_input_missing_errors() {
        let p = temp_path("nope.iso");
        let e = validate_iso_input(&p).unwrap_err();
        assert!(e.to_lowercase().contains("not found"), "{e}");
    }

    #[test]
    fn iso_input_directory_errors() {
        let dir = temp_path("isodir");
        std::fs::create_dir(&dir).unwrap();
        let e = validate_iso_input(&dir).unwrap_err();
        let _ = std::fs::remove_dir(&dir);
        assert!(e.to_lowercase().contains("directory"), "{e}");
    }

    #[test]
    fn iso_input_empty_errors() {
        let f = temp_path("empty.iso");
        std::fs::write(&f, b"").unwrap();
        let e = validate_iso_input(&f).unwrap_err();
        let _ = std::fs::remove_file(&f);
        assert!(e.to_lowercase().contains("empty"), "{e}");
    }

    #[test]
    fn iso_input_nonempty_file_passes_cheap_check() {
        // A non-empty readable file passes the CHEAP preflight (deep image
        // validity is the scan's job, not preflight's).
        let f = temp_path("ok.iso");
        std::fs::write(&f, vec![0u8; 4096]).unwrap();
        let r = validate_iso_input(&f);
        let _ = std::fs::remove_file(&f);
        assert!(r.is_ok(), "non-empty file must pass cheap iso check: {r:?}");
    }

    #[test]
    fn iso_source_missing_errors_through_preflight() {
        // Full path: an iso:// source pointing at a missing file errors in
        // preflight (not just the unit helper).
        let p = temp_path("missing.iso");
        let src = format!("iso://{}", p.display());
        let e = preflight(&src, "null://", false, false).unwrap_err();
        assert!(e.to_lowercase().contains("not found"), "{e}");
    }

    // ── output destination ───────────────────────────────────────────────────

    #[test]
    fn dest_parent_missing_errors() {
        // mkv:// whose parent directory does not exist must error before work.
        let missing_dir = temp_path("no_such_dir");
        let dest = missing_dir.join("movie.mkv");
        let e = validate_file_dest(&dest).unwrap_err();
        assert!(
            e.to_lowercase().contains("director") || e.to_lowercase().contains("exist"),
            "{e}"
        );
    }

    #[test]
    fn dest_is_existing_directory_errors() {
        // A path that is an existing DIRECTORY can't receive a single-file write.
        let dir = temp_path("existing_dir");
        std::fs::create_dir(&dir).unwrap();
        let e = validate_file_dest(&dir).unwrap_err();
        let _ = std::fs::remove_dir(&dir);
        assert!(e.to_lowercase().contains("director"), "{e}");
    }

    #[test]
    fn dest_writable_parent_passes_and_leaves_no_probe_file() {
        // A writable parent + non-existent target passes, and the writability
        // probe must NOT leave its temp file behind.
        let f = temp_path("writable.mkv");
        let r = validate_file_dest(&f);
        assert!(r.is_ok(), "writable dest must pass: {r:?}");
        assert!(
            !f.exists(),
            "the writability probe must clean up its temp file"
        );
    }

    #[test]
    fn dest_writable_check_does_not_truncate_existing_file() {
        // If the target already exists, the probe must NOT truncate it (we open
        // append, not create-new). Pre-seed content and assert it survives.
        let f = temp_path("preexisting.mkv");
        std::fs::write(&f, b"keepme").unwrap();
        let r = validate_file_dest(&f);
        let survived = std::fs::read(&f).unwrap_or_default();
        let _ = std::fs::remove_file(&f);
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(survived, b"keepme", "existing output must not be truncated");
    }

    #[test]
    fn full_preflight_dest_parent_missing_errors() {
        let missing = temp_path("nodir");
        let dest = format!("mkv://{}", missing.join("m.mkv").display());
        let e = preflight("null://", &dest, false, false).unwrap_err();
        assert!(!e.is_empty());
    }

    // ── predicates ───────────────────────────────────────────────────────────

    // See docs/pipe.md#raw_and_multipass_require_a_drive_source — `--raw` / `--multipass` are DRIVE flags, gated on…
    #[test]
    fn raw_and_multipass_require_a_drive_source() {
        assert!(preflight("iso://in.iso", "iso://out.iso", false, true).is_err());
        assert!(preflight("iso://in.iso", "iso://out.iso", true, false).is_err());
        // A drive source still accepts both.
        assert!(preflight("disc://", "iso://out.iso", true, false).is_ok());
        assert!(preflight("disc://", "iso://out.iso", false, true).is_ok());
        // And they remain rejected for a mux destination, as before.
        assert!(preflight("disc://", "mkv://out.mkv", false, true).is_err());
    }

    #[test]
    fn scheme_only_sink_predicate() {
        assert!(is_scheme_only_sink(&parse_url("null://")));
        assert!(is_scheme_only_sink(&parse_url("stdio://")));
        assert!(!is_scheme_only_sink(&parse_url("mkv://x.mkv")));
        assert!(!is_scheme_only_sink(&parse_url("iso://x.iso")));
    }

    // ── null:// multi-title routing fix ──────────────────────────────────────

    // See docs/pipe.md#null_dest_multi_title_routes_all_to_sink — Regression: `null://` on a MULTI-title scanned source must…
    #[test]
    fn null_dest_multi_title_routes_all_to_sink() {
        let titles = Some(vec![
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
        ]);
        let out = Output::new(false, true);
        let parsed = parse_url("null://");
        let jobs = build_jobs(&titles, false, &[], false, "null://", &parsed, &out)
            .expect("null:// multi-title must build jobs, not fail");
        assert_eq!(jobs.len(), 3, "one job per title");
        for (idx, url) in &jobs {
            assert!(idx.is_some(), "each job names its title index");
            assert_eq!(url, "null://", "every title routes to the bare sink");
            assert!(
                matches!(parse_url(url), libfreemkv::StreamUrl::Null),
                "the sink URL must re-parse to Null (not Unknown): {url}"
            );
        }
    }

    /// `stdio://` (the other scheme-only sink) gets the same multi-title routing.
    #[test]
    fn stdio_dest_multi_title_routes_all_to_sink() {
        let titles = Some(vec![
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
        ]);
        let out = Output::new(false, true);
        let parsed = parse_url("stdio://");
        let jobs = build_jobs(&titles, false, &[], false, "stdio://", &parsed, &out)
            .expect("stdio:// multi-title must build jobs");
        assert_eq!(jobs.len(), 2);
        for (_idx, url) in &jobs {
            assert_eq!(url, "stdio://");
        }
    }

    // See docs/pipe.md#demux_dest_multi_title_urls_carry_scheme — `demux://` multi-title routing: each title gets its own
    #[test]
    fn demux_dest_multi_title_urls_carry_scheme() {
        let titles = Some(vec![
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
        ]);
        let out = Output::new(false, true);
        let parsed = parse_url("demux://out/");
        let jobs = build_jobs(&titles, false, &[], false, "demux://out/", &parsed, &out)
            .expect("demux:// multi-title must build jobs");
        assert_eq!(jobs.len(), 2, "one job per title");
        // t01 / t02 subdirs, each a valid Demux URL (scheme present).
        assert_eq!(jobs[0].1, "demux://out/t01/");
        assert_eq!(jobs[1].1, "demux://out/t02/");
        for (idx, url) in &jobs {
            assert!(idx.is_some(), "each job names its title index");
            assert!(
                matches!(parse_url(url), libfreemkv::StreamUrl::Demux { .. }),
                "the job URL must re-parse to Demux (not Unknown): {url}"
            );
        }
    }

    // See docs/pipe.md#kind_filter_dest_multi_title_urls_carry_own_scheme — A multi-title `video://` (kind-filter) dest must carry its…
    #[test]
    fn kind_filter_dest_multi_title_urls_carry_own_scheme() {
        for scheme in ["video", "audio", "sub"] {
            let titles = Some(vec![
                libfreemkv::DiscTitle::empty(),
                libfreemkv::DiscTitle::empty(),
            ]);
            let out = Output::new(false, true);
            let dest = format!("{scheme}://out/");
            let parsed = parse_url(&dest);
            let jobs = build_jobs(&titles, false, &[], false, &dest, &parsed, &out)
                .unwrap_or_else(|| panic!("{scheme}:// multi-title must build jobs"));
            assert_eq!(jobs.len(), 2, "{scheme}: one job per title");
            assert_eq!(jobs[0].1, format!("{scheme}://out/t01/"), "{scheme} t01");
            assert_eq!(jobs[1].1, format!("{scheme}://out/t02/"), "{scheme} t02");
            for (idx, url) in &jobs {
                assert!(idx.is_some(), "{scheme}: each job names its title index");
                // Must re-parse to its OWN kind, not Demux and not Unknown — this
                // is what preserves the kind filter through multi-title fan-out.
                let reparsed = parse_url(url);
                let ok = match scheme {
                    "video" => matches!(reparsed, libfreemkv::StreamUrl::Video { .. }),
                    "audio" => matches!(reparsed, libfreemkv::StreamUrl::Audio { .. }),
                    "sub" => matches!(reparsed, libfreemkv::StreamUrl::Sub { .. }),
                    _ => unreachable!(),
                };
                assert!(ok, "{scheme}: job URL must re-parse to its own kind: {url}");
            }
        }
    }

    // See docs/pipe.md#mp4_skip_reasons_render_distinct_resolving_strings — Every `mp4://` exclusion reason renders a DISTINCT, resolving…
    #[test]
    fn mp4_skip_reasons_render_distinct_resolving_strings() {
        let variants = [
            libfreemkv::Mp4SkipReason::BitmapSubtitle,
            libfreemkv::Mp4SkipReason::UnmappableAudio,
            libfreemkv::Mp4SkipReason::SecondaryVideo,
            libfreemkv::Mp4SkipReason::UnmappableVideo,
        ];
        let mut seen_keys = std::collections::BTreeSet::new();
        let mut seen_msgs = std::collections::BTreeSet::new();
        for v in &variants {
            let key = mp4_skip_reason_key(v);
            // Each variant maps to a distinct i18n key.
            assert!(seen_keys.insert(key), "{v:?}: duplicate reason key {key}");
            // The key resolves — `strings::get` returns the dotted path verbatim
            // on a miss, so a stale/typo'd key would equal the key itself.
            let msg = strings::get(key);
            assert_ne!(msg, key, "{v:?}: key {key} does not resolve in en.json");
            // …and to a distinct rendered message (no two reasons read alike).
            assert!(
                seen_msgs.insert(msg.clone()),
                "{v:?}: duplicate message {msg:?}"
            );
        }
        // The specific Fix-1 pin: the two video reasons must differ.
        assert_ne!(
            strings::get(mp4_skip_reason_key(
                &libfreemkv::Mp4SkipReason::SecondaryVideo
            )),
            strings::get(mp4_skip_reason_key(
                &libfreemkv::Mp4SkipReason::UnmappableVideo
            )),
            "SecondaryVideo and UnmappableVideo must render different strings"
        );
        // The other keys the function emits must also resolve.
        for key in ["stream.track", "mp4.excluded_header"] {
            assert_ne!(strings::get(key), key, "{key} must resolve in en.json");
        }
    }

    /// A real file dest (mkv://) on a multi-title source still routes through
    /// per-title naming (the sink special-case must NOT swallow file dests).
    #[test]
    fn file_dest_multi_title_still_named_per_title() {
        let mut t0 = libfreemkv::DiscTitle::empty();
        t0.playlist = "Movie".into();
        let titles = Some(vec![t0, libfreemkv::DiscTitle::empty()]);
        let out = Output::new(false, true);
        // Directory dest (trailing slash) → one named file per title.
        let dir = temp_path("mkvout");
        let dest = format!("{}/", dir.display());
        let parsed = parse_url(&format!("mkv://{}/", dir.display()));
        let jobs = build_jobs(&titles, false, &[], true, &dest, &parsed, &out);
        let _ = std::fs::remove_dir_all(&dir);
        let jobs = jobs.expect("dir dest builds per-title jobs");
        assert_eq!(jobs.len(), 2);
        for (_idx, url) in &jobs {
            assert!(url.contains("_t"), "per-title file naming preserved: {url}");
        }
    }

    /// `preflight_validate` must NEVER panic, on any combination of adversarial
    /// scheme strings × flag states. The only acceptable outcomes are Ok or Err
    /// — a panic here would crash the CLI on malformed input.
    #[test]
    fn preflight_never_panics_on_adversarial_combinations() {
        let urls = [
            "",
            "://",
            "disc://",
            "disc:///dev/null",
            "iso://",
            "iso://\0",
            "mkv://",
            "m2ts://x",
            "null://",
            "null://trailing",
            "stdio://",
            "network://",
            "network://host:9000",
            "gopher://x",
            "out.mkv",
            "/abs/path",
            "iso://日本語.iso",
            &"iso://".to_string().repeat(1000),
        ];
        for &s in &urls {
            for &d in &urls {
                for raw in [false, true] {
                    for mp in [false, true] {
                        // Must return (Ok or Err), never panic.
                        let _ = preflight(s, d, raw, mp);
                    }
                }
            }
        }
    }

    // WS3 — dropped `-k` short flag (rc.6: `--keydb` long form only). Must be
    // an unknown-flag hard error, not silently consume its value, or a user
    // on the old flag gets a confusing error, or the rip proceeds against the default keydb.

    /// `-k <path>` is no longer a recognized flag: it must be rejected as an
    /// unknown flag (so the user is told to use `--keydb`), never quietly
    /// accepted. The long `--keydb` form (tested elsewhere) is the only spelling.
    #[test]
    fn dropped_short_k_flag_is_unknown() {
        // `-k keydb.cfg` — the dropped short form — must error.
        let err = parse_flags(&v(&["-k", "keydb.cfg"])).unwrap_err();
        assert!(
            err.contains("-k"),
            "unknown-flag error must name `-k`: {err}"
        );
        // It must NOT have been parsed as a keydb path (the parse failed, so no
        // ParsedFlags exists — but assert the long form still works to prove we
        // didn't break `--keydb` while dropping `-k`).
        let f = parse_flags(&v(&["--keydb", "keydb.cfg"])).unwrap();
        assert_eq!(f.keydb_path.as_deref(), Some("keydb.cfg"));
        // Bare `-k` (no value) is likewise unknown, not "needs a value".
        let err = parse_flags(&v(&["-k"])).unwrap_err();
        assert!(
            err.contains("-k") && !err.contains("requires a value"),
            "bare `-k` must be unknown-flag, not flag_needs_value: {err}"
        );
    }

    // See docs/pipe.md#dropped_device_flags_are_unknown — The dropped `--device` / `-d` flags: the device…
    #[test]
    fn dropped_device_flags_are_unknown() {
        for bad in [
            v(&["--device", "/dev/sg0"]),
            v(&["-d", "/dev/sg0"]),
            v(&["--device"]),
            v(&["-d"]),
        ] {
            match parse_flags(&bad) {
                Ok(f) => panic!("{bad:?} must be rejected, got {f:?}"),
                Err(err) => assert!(!err.is_empty(), "{bad:?}: unknown-flag error must be set"),
            }
        }
    }

    // See docs/pipe.md#value_flag_set_matches_parser — The guard `cli_entry::VALUE_FLAGS`' doc comment names, and which…
    #[test]
    fn value_flag_set_matches_parser() {
        // The arity table, written out rather than read back from the code
        // under test. `-k` is deliberately NOT here: it is a retired flag, and
        // `dropped_short_k_flag_is_unknown` (above) pins the rejection.
        const EXPECTED: &[&str] = &[
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
        assert_eq!(
            crate::cli_entry::VALUE_FLAGS,
            EXPECTED,
            "VALUE_FLAGS drifted from the parser's value-taking flags; teach \
             `parse_flags` the new flag (or drop it here) before editing this list"
        );

        // A token no parser arm names, so it can only ever be swallowed as some
        // flag's value or rejected as unknown — never both.
        const SENTINEL: &str = "--freemkv-no-such-flag";
        // The English text of `error.unknown_flag`; the tests run under the
        // default (en) locale, as `dropped_short_k_flag_is_unknown` does.
        const UNKNOWN: &str = "unknown flag";

        for flag in crate::cli_entry::VALUE_FLAGS {
            if let Err(e) = parse_flags(&v(&[flag])) {
                assert!(
                    !e.contains(UNKNOWN),
                    "`{flag}` is in VALUE_FLAGS but `parse_flags` does not know it: {e}"
                );
            }
            // The sentinel is flag-shaped, so this probe measures the flag-guard,
            // not arity, for flags refusing a flag-shaped value (the logging
            // pair) — their arity is covered by a dedicated test instead.
            if matches!(*flag, "--log-file" | "--log-level") {
                continue;
            }
            if let Err(e) = parse_flags(&v(&[flag, SENTINEL])) {
                assert!(
                    !e.contains(UNKNOWN),
                    "`{flag}` must consume `{SENTINEL}` as its value, not leave \
                     it to be parsed as a flag: {e}"
                );
            }
        }

        // The other side of the contract: a boolean flag must NOT be listed,
        // and must leave the next token alone.
        for flag in ["-q", "--quiet", "--raw", "--multipass", "--force"] {
            assert!(
                !crate::cli_entry::VALUE_FLAGS.contains(&flag),
                "`{flag}` takes no value; listing it makes `collect_urls` eat \
                 the next token"
            );
            let e = parse_flags(&v(&[flag, SENTINEL]))
                .expect_err("the sentinel must reach the unknown-flag arm");
            assert!(
                e.contains(UNKNOWN) && e.contains(SENTINEL),
                "`{flag}` swallowed the following token: {e}"
            );
        }
    }

    // See docs/pipe.md#retired_value_flags_are_rejected_by_the_parser — The other half of the arity contract: `cli_entry::RETIRED_VALUE_FLAGS`
    #[test]
    fn retired_value_flags_are_rejected_by_the_parser() {
        for flag in crate::cli_entry::RETIRED_VALUE_FLAGS {
            assert!(
                !crate::cli_entry::VALUE_FLAGS.contains(flag),
                "`{flag}` cannot be both live and retired"
            );
            let err = parse_flags(&v(&[flag, "value"]))
                .expect_err("a retired flag must be rejected, not parsed");
            assert!(
                err.contains("unknown flag") && err.contains(flag),
                "the rejection of `{flag}` must name it: {err}"
            );
        }
    }

    // WS3 — device comes from the source URL (`disc:///dev/sgN`), not a
    // `--device` flag. Pins the exact `parse_url` shape that `info_cmd` /
    // `pipe_disc` / `dir_to_extract` depend on to read the device out.

    #[test]
    fn device_comes_from_disc_url() {
        // Explicit device path → carried in the URL.
        match parse_url("disc:///dev/sg3") {
            libfreemkv::StreamUrl::Disc { device: Some(p) } => {
                assert_eq!(p.to_string_lossy(), "/dev/sg3");
            }
            other => panic!("disc:///dev/sg3 must parse to Disc{{device:Some}}, got {other:?}"),
        }
        // Bare `disc://` → auto-detect (device None); the routes fall back to
        // `find_drive()` rather than a flag.
        match parse_url("disc://") {
            libfreemkv::StreamUrl::Disc { device: None } => {}
            other => panic!("disc:// must parse to Disc{{device:None}}, got {other:?}"),
        }
        // A Windows device path survives the URL too (the route is OS-agnostic;
        // the path string is opaque to the parser).
        match parse_url("disc://D:") {
            libfreemkv::StreamUrl::Disc { device: Some(p) } => {
                assert_eq!(p.to_string_lossy(), "D:");
            }
            other => panic!("disc://D: must parse to Disc{{device:Some}}, got {other:?}"),
        }
    }

    // WS3 — path handling. `sanitize_name` seeds per-title output filenames;
    // it must never emit a path separator, illegal character, or empty stem
    // on any platform, since a rip authored on Linux may be muxed on Windows.

    #[test]
    fn sanitize_name_strips_path_separators_and_illegal_chars() {
        // Forward AND back slashes must not survive, or a `Movie/Part2` stem
        // synthesizes `dir/Movie/Part2_t1.mkv`, escaping the dest dir. The
        // separator is stripped (not converted), so space-only gaps become underscores.
        let s = sanitize_name("Movie/Part 2");
        assert!(!s.contains('/'), "path separator survived: {s}");
        assert_eq!(s, "MoviePart_2", "got {s}");
        let s = sanitize_name(r"A\B");
        assert!(!s.contains('\\'), "backslash survived: {s}");
        assert_eq!(s, "AB", "backslash stripped, not converted: {s}");
        // Windows-illegal punctuation (`: * ? " < > |`) is dropped, not kept.
        let s = sanitize_name(r#"a:b*c?d"e<f>g|h"#);
        for bad in [':', '*', '?', '"', '<', '>', '|', '\\', '/'] {
            assert!(!s.contains(bad), "illegal char {bad:?} survived in {s}");
        }
        assert_eq!(s, "abcdefgh", "got {s}");
    }

    #[test]
    fn sanitize_name_spaces_to_underscores_and_trims() {
        assert_eq!(sanitize_name("  The  Movie  "), "The__Movie");
        // Hyphen and underscore are preserved (legal everywhere).
        assert_eq!(sanitize_name("Director-Cut_2"), "Director-Cut_2");
    }

    #[test]
    fn sanitize_name_empty_or_all_illegal_falls_back_to_disc() {
        // An empty or fully-stripped stem must fall back to "disc" so the
        // per-title filename is never `_t1.mkv` (leading underscore, no stem).
        assert_eq!(sanitize_name(""), "disc");
        assert_eq!(sanitize_name("///"), "disc");
        assert_eq!(sanitize_name(":*?"), "disc");
        assert_eq!(sanitize_name("   "), "disc");
        // A name that is ALL non-ascii is stripped to empty → "disc".
        assert_eq!(sanitize_name("日本語"), "disc");
    }

    // WS3 — keydb path resolution. `resolved_keydb_path` honors an explicit
    // `--keydb` override, else falls back to exe-local/default, and never
    // panics. The search policy itself lives in `freemkv-keysources::paths`.

    #[test]
    fn resolved_keydb_path_honors_explicit_override() {
        // An explicit `--keydb PATH` is used verbatim, never the search policy.
        let p = resolved_keydb_path(&Some("/custom/keydb.cfg".to_string()));
        assert_eq!(p, std::path::PathBuf::from("/custom/keydb.cfg"));
        // A Windows-style override path is passed through unchanged too.
        let p = resolved_keydb_path(&Some(r"C:\keys\keydb.cfg".to_string()));
        assert_eq!(p, std::path::PathBuf::from(r"C:\keys\keydb.cfg"));
    }

    #[test]
    fn resolved_keydb_path_falls_back_without_panicking() {
        // No override → the exe-local/default policy (or the bare `keydb.cfg`
        // last resort). Either way a non-empty path is returned, never a panic.
        let p = resolved_keydb_path(&None);
        assert!(
            p.file_name().is_some_and(|n| n == "keydb.cfg"),
            "fallback must end in keydb.cfg: {}",
            p.display()
        );
    }

    // WS3 — dir:// preflight messaging render. Pins that the rendered message
    // substitutes placeholders (no leftover `{path}`/`{source}`); the dir://
    // gate tests above cover which inputs error, this covers the text.

    #[test]
    fn dir_source_unsupported_message_substitutes_and_guides() {
        // A byte-stream source into dir:// renders the localized guidance with
        // the offending source URL substituted and no leftover placeholder.
        let out = temp_path("dir_msg_src");
        let dest = format!("dir://{}/", out.display());
        let err = preflight("mkv://in.mkv", &dest, false, false).unwrap_err();
        let _ = std::fs::remove_dir_all(&out);
        assert!(
            err.contains("mkv://in.mkv"),
            "source not substituted: {err}"
        );
        assert!(!err.contains("{source}"), "leftover placeholder: {err}");
        // Guides toward a usable source (disc:// or iso://).
        assert!(
            err.contains("disc://") || err.contains("iso://"),
            "must guide to a filesystem source: {err}"
        );
    }

    #[test]
    fn dir_dest_is_file_message_substitutes_path() {
        // A dir:// target that is a regular file renders the path-substituted
        // file/folder mismatch message with no leftover `{path}`.
        let f = temp_path("dir_msg_file");
        std::fs::write(&f, b"x").unwrap();
        let dest = format!("dir://{}", f.display());
        let err = preflight("disc://", &dest, false, false).unwrap_err();
        let _ = std::fs::remove_file(&f);
        assert!(
            err.contains(&f.display().to_string()),
            "path not substituted: {err}"
        );
        assert!(!err.contains("{path}"), "leftover placeholder: {err}");
    }

    // WS3 — the WS2 messaging render shape: `main::fatal` builds the fatal
    // block from `error.fatal_header` and `error.fatal_diagnostic_hint`.
    // Pins the shape so a locale/template drop of the code or level is caught.

    #[test]
    fn fatal_header_assembles_level_op_and_code_forward_cause() {
        // The cause fragment for a real library error, code-forward (E-prefixed).
        let cause = fmt_err(&"E6009");
        assert!(
            cause.starts_with("E6009 "),
            "cause not code-forward: {cause}"
        );

        // The render site assembles `{level}: {op} failed: {cause}`. Reproduce
        // the exact substitution `main::fatal` performs (it isn't callable — it
        // exits the process — so we pin the template + parts it feeds).
        let level = crate::strings::get(crate::messaging::Level::Error.locale_key());
        let op = crate::strings::get("error.op_rip");
        let header = crate::strings::fmt(
            "error.fatal_header",
            &[("level", &level), ("op", &op), ("cause", &cause)],
        );
        // All three parts present, in order, with no leftover placeholders.
        assert!(
            header.starts_with(&format!("{level}:")),
            "level first: {header}"
        );
        assert!(header.contains(&op), "op missing: {header}");
        assert!(header.contains(&cause), "cause missing: {header}");
        assert!(
            !header.contains("{level}") && !header.contains("{op}") && !header.contains("{cause}"),
            "leftover placeholder in fatal header: {header}"
        );
        // The diagnostic-log hint exists and names the --log-level escape hatch.
        let hint = crate::strings::get("error.fatal_diagnostic_hint");
        assert_ne!(hint, "error.fatal_diagnostic_hint", "hint key missing");
        assert!(
            hint.contains("--log-level"),
            "hint must point at the log flag: {hint}"
        );
    }

    // See docs/pipe.md#fatal_operation_keys_all_resolve — Each operation name key the fatal block can…
    #[test]
    fn fatal_operation_keys_all_resolve() {
        for key in [
            "error.op_rip",
            "error.op_info",
            "error.op_verify",
            "error.op_update_keys",
        ] {
            assert_ne!(
                crate::strings::get(key),
                key,
                "fatal op key {key} unresolved (would print the raw key)"
            );
        }
    }

    // WS3 — Windows-only, compile-gated. Doesn't run in the Mac precommit but
    // must compile cleanly so Windows CI validates it. Pins the OS-specific
    // path/keydb shapes the CLI relies on under Windows.

    #[cfg(windows)]
    #[test]
    fn windows_keydb_override_keeps_drive_letter_path() {
        // A Windows override path (drive letter + backslashes) must survive
        // verbatim through the CLI wrapper, unmangled.
        let p = resolved_keydb_path(&Some(r"C:\Users\me\AppData\keydb.cfg".to_string()));
        assert_eq!(
            p,
            std::path::PathBuf::from(r"C:\Users\me\AppData\keydb.cfg")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_sanitize_name_drops_reserved_punctuation() {
        // `:` (drive separator) and `\` must never reach a synthesized
        // filename. Pinned under the Windows build so CI's Windows job
        // catches a regression even if this later becomes cfg-specific.
        let s = sanitize_name(r"C:\Movie");
        assert!(!s.contains(':') && !s.contains('\\'), "got {s}");
    }
}

// See docs/pipe.md#line5859 — The decisions that separate "the rip worked" from…
#[cfg(test)]
mod verdict_tests {
    use super::{
        CopyVerdict, PipeFail, check_selection_coverage, copy_verdict, disc_copy_options,
        disc_copy_succeeded, extract_succeeded, finalize_mux, is_feature_title, title_policy,
    };
    use crate::output::Output;

    fn outcome(completed: bool, bytes: u64) -> libfreemkv::MuxOutcome {
        libfreemkv::MuxOutcome {
            completed,
            output_opened: true,
            bytes_written: bytes,
            errors: 0,
            lost_bytes: 0,
            streams: 3,
            undelivered_streams: vec![],
        }
    }

    fn quiet() -> Output {
        Output::new(false, true)
    }

    // See docs/pipe.md#a_completed_but_lossy_mux_names_the_undelivered_stream — A mux that `completed` can still be LOSSY.…
    #[test]
    fn a_completed_but_lossy_mux_names_the_undelivered_stream() {
        // Pin the catalog so the assertion reads the English wording regardless of
        // the machine's LANG.
        crate::strings::set_locale("en");

        let mut lossy = outcome(true, 1_000_000);
        lossy.undelivered_streams = vec![1];
        let events = super::CliMuxEvents::new(loud(), "mp4:///out/x.mp4".into(), false);

        let (res, printed) = crate::output::capture(|| finalize_mux(Ok(lossy), &loud(), &events));

        // Still a success: the file is finalized and playable, just missing a track.
        assert!(
            res.is_ok(),
            "a completed mux missing one track is not a truncated file"
        );
        assert!(
            printed.contains("Track 2"),
            "the undelivered stream must be NAMED in the output the user sees; got:\n{printed}"
        );
        assert!(
            printed.contains("left out"),
            "the user must be told the file is missing tracks; got:\n{printed}"
        );

        // The clean case must stay clean — no phantom warning on a lossless rip.
        let events = super::CliMuxEvents::new(loud(), "mp4:///out/x.mp4".into(), false);
        let (ok, clean) =
            crate::output::capture(|| finalize_mux(Ok(outcome(true, 1_000_000)), &loud(), &events));
        assert!(ok.is_ok());
        assert!(
            !clean.contains("left out"),
            "a lossless mux must not warn; got:\n{clean}"
        );
    }

    // See docs/pipe.md#a_completed_mux_that_dropped_payload_bytes_says_how_much_it_lost — A mux that dropped PAYLOAD BYTES is lossy…
    #[test]
    fn a_completed_mux_that_dropped_payload_bytes_says_how_much_it_lost() {
        crate::strings::set_locale("en");

        let mut holed = outcome(true, 1_000_000);
        holed.errors = 2;
        holed.lost_bytes = 3 << 20;
        let events = super::CliMuxEvents::new(loud(), "mkv:///out/movie.mkv".into(), false);

        let (res, printed) = crate::output::capture(|| finalize_mux(Ok(holed), &loud(), &events));

        // Still a success for the exit code: the file is finalised and
        // playable, and the per-title loop must not abandon a batch over it —
        // the same call the sibling `undelivered_streams` warning makes.
        assert!(res.is_ok());
        assert!(
            printed.contains("lost"),
            "the loss must be in the output the user sees; got:\n{printed}"
        );
        assert!(
            printed.contains('3'),
            "and it must say HOW MUCH was lost; got:\n{printed}"
        );

        // A lossless mux stays silent: an unconditional warning is worse than
        // none at all.
        let events = super::CliMuxEvents::new(loud(), "mkv:///out/movie.mkv".into(), false);
        let (ok, clean) =
            crate::output::capture(|| finalize_mux(Ok(outcome(true, 1_000_000)), &loud(), &events));
        assert!(ok.is_ok());
        assert!(
            !clean.contains("lost"),
            "a lossless mux must not warn; got:\n{clean}"
        );
    }

    /// Normal verbosity — the level a user gets by default, and the only one at
    /// which `Level::Normal` lines are observable.
    fn loud() -> Output {
        Output::new(false, false)
    }

    // See docs/pipe.md#a_truncated_mux_is_a_failure_not_a_success — EVERY CLI mux funnels its result through `finalize_mux`…
    #[test]
    fn a_truncated_mux_is_a_failure_not_a_success() {
        let events = super::CliMuxEvents::new(quiet(), "mkv:///out/x.mkv".into(), true);

        let good = finalize_mux(Ok(outcome(true, 1_000_000)), &quiet(), &events);
        assert!(good.is_ok(), "a completed mux is a success");

        let truncated = finalize_mux(Ok(outcome(false, 1_000)), &quiet(), &events)
            .expect_err("a mux that did not complete left a truncated file — never Ok");
        // Halted, not Failed: the multi-title loop must FULL-STOP rather than
        // cancel each remaining title one at a time.
        assert_eq!(truncated.result, freemkv_engine::TitleResult::Halted);

        // Bytes written is not the test — a halt after 5 GB is still a halt.
        let big = finalize_mux(Ok(outcome(false, 5_000_000_000)), &quiet(), &events);
        assert!(
            big.is_err(),
            "bytes written cannot excuse an incomplete mux"
        );

        // A hard I/O error is classified through the mux path, not as a halt.
        let failed = finalize_mux(Err(std::io::Error::other("E7022")), &quiet(), &events)
            .expect_err("an errored mux is a failure");
        assert_ne!(failed.result, freemkv_engine::TitleResult::Halted);
    }

    fn copy_result(halted: bool, good: u64, unreadable: u64) -> freemkv_engine::CopyResult {
        freemkv_engine::CopyResult {
            bytes_total: good + unreadable,
            bytes_good: good,
            bytes_unreadable: unreadable,
            bytes_pending: 0,
            recovered_this_pass: good,
            complete: !halted && unreadable == 0,
            halted,
        }
    }

    // See docs/pipe.md#copy_verdict_reports_a_halt_and_a_zero_recovery_as_failures — Both failure verdicts arrive from the engine as…
    #[test]
    fn copy_verdict_reports_a_halt_and_a_zero_recovery_as_failures() {
        // Ctrl-C after 5 GB: a partial ISO, resumable from the mapfile.
        assert_eq!(
            copy_verdict(&copy_result(true, 5_000_000_000, 0)),
            CopyVerdict::Interrupted
        );
        // Ran to the end and read NOTHING: the ISO on disk is all zeroes.
        assert_eq!(
            copy_verdict(&copy_result(false, 0, 50_000_000_000)),
            CopyVerdict::NoData
        );
        // A single recovered byte is enough to have produced something.
        assert_eq!(
            copy_verdict(&copy_result(false, 1, 0)),
            CopyVerdict::Complete
        );
        // 4 KiB of the disc never arrived. The image is usable and worth
        // keeping — it is not the disc, and the exit code has to say so.
        assert_eq!(
            copy_verdict(&copy_result(false, 25_000_000_000, 4096)),
            CopyVerdict::Lossy
        );
        // A halt that also recovered nothing reads as the halt — it is
        // resumable, and telling the user they stopped it is more useful.
        assert_eq!(
            copy_verdict(&copy_result(true, 0, 0)),
            CopyVerdict::Interrupted
        );
    }

    // See docs/pipe.md#a_partial_recovery_is_never_graded_as_a_clean_image — A sweep that LOST sectors is not a…
    #[test]
    fn a_partial_recovery_is_never_graded_as_a_clean_image() {
        // 4 KiB unreadable out of 25 GB: two sectors of the user's film, gone.
        assert_ne!(
            copy_verdict(&copy_result(false, 25_000_000_000, 4096)),
            CopyVerdict::Complete,
            "an image with unreadable sectors must not grade as a clean one"
        );
        // Sectors still PENDING are loss too — they were attempted and skipped,
        // and nothing later in a single-pass run will pick them up.
        let mut pending = copy_result(false, 25_000_000_000, 0);
        pending.bytes_pending = 8192;
        assert_ne!(
            copy_verdict(&pending),
            CopyVerdict::Complete,
            "pending sectors are unread bytes; the image is short of them"
        );
        // The clean sweep is untouched: this must not fail every ordinary rip.
        assert_eq!(
            copy_verdict(&copy_result(false, 25_000_000_000, 0)),
            CopyVerdict::Complete
        );
    }

    // See docs/pipe.md#a_lossy_copy_reports_its_loss_and_fails_the_exit_code — Only a whole image is a success, and…
    #[test]
    fn a_lossy_copy_reports_its_loss_and_fails_the_exit_code() {
        assert!(disc_copy_succeeded(CopyVerdict::Complete));
        for v in [
            CopyVerdict::Lossy,
            CopyVerdict::NoData,
            CopyVerdict::Interrupted,
        ] {
            assert!(
                !disc_copy_succeeded(v),
                "{v:?} must not exit 0 — the image is not what was asked for"
            );
        }

        // The reporting half. `pipe_disc` needs a real drive, so the wiring is
        // pinned in the source: the loss block must be reached by the VERDICT,
        // never by `if multipass`.
        let src = include_str!("pipe.rs").replace("\r\n", "\n");
        let start = src
            .find("            let verdict = copy_verdict(&r);")
            .expect("the success arm still grades its result");
        let end = start
            + src[start..]
                .find("\n        Err(e) => {")
                .expect("the error arm still ends the success arm");
        let arm = &src[start..end];
        assert!(
            !arm.contains("if multipass {"),
            "the loss report must not depend on the recovery strategy"
        );
        assert!(
            arm.contains("rip.mapfile_summary") && arm.contains("disc_copy_succeeded(verdict)"),
            "a lossy sweep must print what it lost and return the verdict"
        );
    }

    // See docs/pipe.md#the_disc_copy_options_honour_raw_multipass_and_progress — `decrypt: !raw` inverted writes a CIPHERTEXT ISO under…
    #[test]
    fn the_disc_copy_options_honour_raw_multipass_and_progress() {
        let nop = |_: &libfreemkv::progress::PassProgress| true;

        let default_flags = disc_copy_options(false, false, &nop);
        assert!(
            default_flags.decrypt,
            "a plain disc->iso rip must DECRYPT; ciphertext is only ever --raw"
        );
        assert!(!default_flags.multipass);
        assert!(
            default_flags.progress.is_some(),
            "a sweep must report progress"
        );
        assert!(default_flags.halt.is_none());

        let raw = disc_copy_options(true, true, &nop);
        assert!(!raw.decrypt, "--raw is ciphertext passthrough");
        assert!(
            raw.multipass,
            "--multipass must reach the copy or recovery never runs"
        );
        assert!(raw.progress.is_some());
    }

    // See docs/pipe.md#a_halted_or_holed_extraction_exits_nonzero — `dir://` extraction hands its bool straight to the…
    #[test]
    fn a_halted_or_holed_extraction_exits_nonzero() {
        assert!(extract_succeeded(false, true));
        assert!(
            !extract_succeeded(true, false),
            "a halted extract is a failure"
        );
        assert!(
            !extract_succeeded(false, false),
            "a holed tree is a failure"
        );
        // Contradictory input still fails closed rather than reporting success.
        assert!(!extract_succeeded(true, true));
    }

    // See docs/pipe.md#a_single_title_rip_never_downgrades_a_failure_to_a_skip — One job and a named title can never…
    #[test]
    fn a_single_title_rip_never_downgrades_a_failure_to_a_skip() {
        use freemkv_engine::{TitleAction, TitleResult, decide_title};

        let (multi, explicit) = title_policy(1, &[2], false);
        assert!(!multi, "one job is not a multi-title rip");
        assert!(explicit, "a named -t is an explicit selection");
        assert!(matches!(
            decide_title(
                &TitleResult::Failed,
                is_feature_title(Some(1)),
                multi,
                explicit
            ),
            TitleAction::StopFatal
        ));

        // A real all-titles batch: an incidental uncrackable stub is skippable.
        let (multi, explicit) = title_policy(12, &[], true);
        assert!(multi);
        assert!(
            !explicit,
            "-t all asks for everything, which is not the same as naming titles"
        );

        // `-t all` expanded into a list must still read as non-explicit, or the
        // first menu stub on an obfuscated disc aborts the whole rip.
        let (_, explicit) = title_policy(12, &[1, 2, 3], true);
        assert!(!explicit);
        // Named titles without -t all stay explicit even in a batch.
        let (multi, explicit) = title_policy(3, &[1, 2, 3], false);
        assert!(multi && explicit);
        // No jobs, no flags: neither.
        assert_eq!(title_policy(0, &[], false), (false, false));
    }

    /// The main feature is title index 0. A failure there is a hard error even
    /// in an all-titles rip; inverted, the movie itself becomes skippable and
    /// the run summarises as success.
    #[test]
    fn only_title_index_zero_is_the_main_feature() {
        assert!(is_feature_title(Some(0)));
        assert!(is_feature_title(None), "an unindexed job is the feature");
        assert!(!is_feature_title(Some(1)));
        assert!(!is_feature_title(Some(11)));
    }

    fn audio(pid: u16, lang: &str) -> libfreemkv::Stream {
        libfreemkv::Stream::Audio(libfreemkv::AudioStream {
            pid,
            codec: libfreemkv::Codec::TrueHd,
            channels: libfreemkv::AudioChannels::Stereo,
            language: lang.into(),
            sample_rate: libfreemkv::SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        })
    }

    // See docs/pipe.md#a_requested_language_absent_from_the_title_is_an_error_for_a_single_title — `-a jpn` against a title that carries only…
    #[test]
    fn a_requested_language_absent_from_the_title_is_an_error_for_a_single_title() {
        let mut title = libfreemkv::DiscTitle::empty();
        title.streams = vec![audio(0x1100, "eng")];
        let streams = freemkv_engine::StreamChoice {
            audio: freemkv_engine::StreamFilter::Langs(vec!["jpn".into()]),
            subtitles: freemkv_engine::StreamFilter::All.into(),
        };

        let err = check_selection_coverage(&streams, &title, 1, false, &quiet())
            .expect_err("a single-title rip must fail rather than ship a soundless file");
        assert!(
            err.to_lowercase().contains("eng") || err.contains("jpn"),
            "the message should name the languages involved: {err}"
        );

        // Same title in a batch: warn loudly and keep going. Assert the
        // warning is actually emitted, not just that the call returns Ok, or
        // a regression that silently drops the warning would still pass.
        let (batch_res, printed) = crate::output::capture(|| {
            check_selection_coverage(&streams, &title, 4, true, &quiet())
        });
        assert!(
            batch_res.is_ok(),
            "a batch must not hard-fail on one title of the wrong language"
        );
        assert!(
            printed.contains("jpn") || printed.to_lowercase().contains("eng"),
            "a batch must still warn loudly about the unmatched language: {printed:?}"
        );

        // A language the title DOES carry is not an error in either mode.
        let matched = freemkv_engine::StreamChoice {
            audio: freemkv_engine::StreamFilter::Langs(vec!["eng".into()]),
            subtitles: freemkv_engine::StreamFilter::All.into(),
        };
        assert!(check_selection_coverage(&matched, &title, 1, false, &quiet()).is_ok());
    }

    /// A `PipeFail` classification is what the loop acts on, so the
    /// constructors must not collapse into each other.
    #[test]
    fn the_failure_classes_stay_distinct() {
        assert_eq!(
            PipeFail::fatal("x".into()).result,
            freemkv_engine::TitleResult::Failed
        );
        assert_eq!(
            PipeFail::halted("x".into()).result,
            freemkv_engine::TitleResult::Halted
        );
    }
}

#[cfg(test)]
mod disc_gate_tests {
    use super::needs_pre_mux_title_key;
    use libfreemkv::DiscFormat;

    // See docs/pipe.md#every_format_but_dvd_is_key_checked_before_the_mux — The per-title decrypt gate runs for every format…
    #[test]
    fn every_format_but_dvd_is_key_checked_before_the_mux() {
        for f in [
            DiscFormat::BluRay,
            DiscFormat::Uhd,
            DiscFormat::Fmts,
            DiscFormat::HdDvd,
        ] {
            assert!(
                needs_pre_mux_title_key(f),
                "{f:?} must be key-checked before the mux or it muxes garbage at exit 0"
            );
        }
        assert!(
            !needs_pre_mux_title_key(DiscFormat::Dvd),
            "a DVD is cracked inside DiscStream::new — gating here is a second drive read"
        );
    }
}

#[cfg(test)]
mod iso_key_tests {
    use super::{KeyConfig, resolve_iso_unit_keys};
    use crate::output::Output;

    /// A reader that returns zeroes. `resolve_iso_unit_keys` only samples
    /// ciphertext through it, and with no key source configured no sample is
    /// consulted at all — the disc's already-resolved keys are what comes back.
    struct ZeroSource;
    impl libfreemkv::SectorSource for ZeroSource {
        fn read_sectors(
            &mut self,
            _lba: u32,
            count: u16,
            buf: &mut [u8],
            _recovery: bool,
        ) -> libfreemkv::Result<usize> {
            let n = count as usize * 2048;
            buf[..n].fill(0);
            Ok(n)
        }
    }

    fn disc(aacs: Option<libfreemkv::AacsState>, encrypted: bool) -> libfreemkv::Disc {
        libfreemkv::Disc {
            volume_id: "TEST".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs,
            css: None,
            encrypted,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    fn aacs(unit_keys: Vec<(u32, [u8; 16])>) -> libfreemkv::AacsState {
        libfreemkv::AacsState {
            version: 1,
            bus_encryption: false,
            mkb_version: None,
            disc_hash: String::new(),
            key_source: libfreemkv::KeyOrigin::ExternalUk,
            vuk: None,
            unit_keys,
            read_data_key: None,
            volume_id: [0u8; 16],
            uk_ro: Vec::new(),
            mkb: Vec::new(),
        }
    }

    // See docs/pipe.md#aacs_unit_keys_are_forwarded_from_the_scan — The keys the scan resolved must be the…
    #[test]
    fn aacs_unit_keys_are_forwarded_from_the_scan() {
        let out = Output::new(false, true);
        // No keydb and no key URL: nothing is consulted, so what comes out is
        // exactly what the scan already had.
        let keys = KeyConfig {
            keydb_path: None,
            key_url: None,
            key_auth: None,
        };

        let want = vec![(0u32, [0xABu8; 16]), (7u32, [0x5Cu8; 16])];
        let got = resolve_iso_unit_keys(
            disc(Some(aacs(want.clone())), true),
            Box::new(ZeroSource),
            &keys,
            &out,
        );
        assert_eq!(got, want, "the scan's unit keys did not reach the mux");
        assert!(
            !got.iter().all(|(_, k)| *k == [0u8; 16]),
            "an all-zero key is a placeholder, not a resolved key"
        );

        // An unencrypted disc contributes NO keys — not a placeholder entry.
        let clear = resolve_iso_unit_keys(disc(None, false), Box::new(ZeroSource), &keys, &out);
        assert!(
            clear.is_empty(),
            "an unencrypted ISO must yield no keys, got {clear:?}"
        );
    }
}

#[cfg(test)]
mod build_jobs_edge_tests {
    use super::{build_jobs, disc_title_nums};
    use crate::output::Output;
    use libfreemkv::parse_url;

    // See docs/pipe.md#an_empty_scanned_title_list_still_builds_a_job — A scanned source whose title list came back…
    #[test]
    fn an_empty_scanned_title_list_still_builds_a_job() {
        let out = Output::new(false, true);
        let dest = "mkv:///tmp/fmkv-empty-scan.mkv";
        let parsed = parse_url(dest);
        let jobs = build_jobs(&Some(vec![]), false, &[], false, dest, &parsed, &out)
            .expect("an empty title list must not fail job building");
        assert_eq!(
            jobs.len(),
            1,
            "an empty scan produced {} jobs — a zero-job rip exits 0 having \
             written nothing",
            jobs.len()
        );
        assert_eq!(jobs[0].0, None, "no title was selected, so no index");
    }

    // See docs/pipe.md#one_title_into_a_directory_is_still_named_per_title — One title going to a DIRECTORY is named…
    #[test]
    fn one_title_into_a_directory_is_still_named_per_title() {
        let out = Output::new(false, true);
        let dir = super::tests::temp_path("one-into-dir");
        let dest = format!("mkv://{}/", dir.display());
        let parsed = parse_url(&dest);

        let titles = Some(vec![
            libfreemkv::DiscTitle::empty(),
            libfreemkv::DiscTitle::empty(),
        ]);
        let jobs = build_jobs(&titles, false, &[1], true, &dest, &parsed, &out)
            .expect("a directory dest accepts a single title");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].0, Some(0), "-t 1 is the first title, 0-based");
        assert!(
            jobs[0].1.ends_with("_t1.mkv"),
            "a directory dest must still name the file per title, got {}",
            jobs[0].1
        );
        assert_ne!(
            jobs[0].1, dest,
            "the directory itself is not the output file"
        );

        // The same title against a single FILE dest goes straight to that file.
        let file = "mkv:///tmp/fmkv-one.mkv";
        let pf = parse_url(file);
        let jobs = build_jobs(&titles, false, &[1], false, file, &pf, &out)
            .expect("a file dest accepts a single title");
        assert_eq!(jobs, vec![(Some(0), file.to_string())]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // See docs/pipe.md#an_expanded_disc_selection_refuses_a_single_file_destination — `-t all` on a disc must be REJECTED…
    #[test]
    fn an_expanded_disc_selection_refuses_a_single_file_destination() {
        let out = Output::new(false, true);
        let dest = "mkv:///tmp/fmkv-t-all-refuse.mkv";
        let parsed = parse_url(dest);
        let nums = disc_title_nums(true, &[], 12);
        assert!(
            build_jobs(&None, true, &nums, false, dest, &parsed, &out).is_none(),
            "twelve titles were accepted into one file"
        );
    }
}

// ── The image-decrypt destination must not be the source ─────────────────────
// `write_image` truncates via `File::create` before the first read, so
// `iso://Disc.iso iso://Disc.iso` zeroed the still-open input; the CLI lacked this guard.
#[cfg(test)]
mod dest_is_source_tests {
    use super::{preflight_validate, same_file, source_path_of, url_path_of};

    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!(
                "freemkv_same_file_{}_{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&d);
            let _ = std::fs::create_dir_all(&d);
            Tmp(d)
        }
        fn file(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, bytes).expect("write fixture");
            p
        }
        fn dir(&self, name: &str) -> std::path::PathBuf {
            let p = self.0.join(name);
            std::fs::create_dir_all(&p).expect("create fixture dir");
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // See docs/pipe.md#the_same_file_under_two_spellings_is_recognised — The same file under two spellings is one…
    #[test]
    fn the_same_file_under_two_spellings_is_recognised() {
        let t = Tmp::new("spellings");
        let abs = t.file("Disc.iso", b"not really an iso");
        std::fs::create_dir_all(t.0.join("sub")).expect("subdir");
        let detoured = t.0.join("sub").join("..").join("Disc.iso");
        assert_ne!(
            detoured.as_path(),
            abs.as_path(),
            "the fixture must not be equal as PATHS, or it proves nothing"
        );
        assert!(same_file(Some(&abs), &abs), "a path is itself");
        assert!(
            same_file(Some(&detoured), &abs),
            "sub/../Disc.iso and Disc.iso are one file"
        );
    }

    /// A different file is not refused — the guard must not break ordinary
    /// rips, which are the overwhelming majority.
    #[test]
    fn two_different_files_are_not_the_same_file() {
        let t = Tmp::new("distinct");
        let a = t.file("In.iso", b"a");
        let b = t.file("Out.iso", b"b");
        assert!(!same_file(Some(&a), &b));
    }

    /// The ordinary case: the destination does not exist yet. It cannot be the
    /// source, and a failed canonicalize must never refuse the rip.
    #[test]
    fn a_destination_that_does_not_exist_yet_is_never_the_source() {
        let t = Tmp::new("missing_dest");
        let a = t.file("In.iso", b"a");
        let dest = t.0.join("does-not-exist.iso");
        assert!(!same_file(Some(&a), &dest));
        // And a source with no filesystem path at all (disc://) is never it.
        assert!(!same_file(None, &dest));
    }

    // See docs/pipe.md#the_image_decrypt_path_actually_calls_the_guard — The guard is actually WIRED, not merely correct
    #[test]
    fn the_image_decrypt_path_actually_calls_the_guard() {
        let src = include_str!("pipe.rs").replace("\r\n", "\n");
        let start = src
            .find("\nfn image_to_iso(source: &str")
            .expect("image_to_iso definition present");
        let end = start
            + src[start..]
                .find("\n    let result = libfreemkv::write_image(")
                .expect("the write call still ends the setup section");
        let body = &src[start..end];
        // Two independent tokens, not one call expression: `cargo fmt` splits
        // arguments across lines, so a joined-text pin would fail on
        // formatting changes alone — a guard that cries wolf gets deleted.
        assert!(
            body.contains("same_file(") && body.contains("source_path_of(source)"),
            "image_to_iso must refuse a destination that IS the source before \
             write_image truncates it"
        );
        assert!(
            body.contains("resolve_content_key_map"),
            "AACS decryption is map-only; without a key map every encrypted \
             image decrypt aborts on its first batch"
        );
    }

    /// Both `iso://` and `dir://` carry a path the guard has to see; anything
    /// else (a live drive) has none.
    #[test]
    fn the_guard_sees_the_path_behind_every_file_backed_scheme() {
        assert!(source_path_of("iso:///media/Disc.iso").is_some());
        assert!(source_path_of("dir:///media/BDMV").is_some());
        assert!(
            source_path_of("disc://").is_none(),
            "a live drive has no path to compare"
        );
    }

    // ── Every file-backed scheme, not just iso:// ────────────────────────────
    // The round-3 guard covered only `Iso`/`Dir`, truncating other schemes'
    // still-open input. Now lives in `preflight_validate`, before any write.

    /// The path behind a URL, for EVERY scheme that names one — the source
    /// side and the destination side both.
    #[test]
    fn every_scheme_that_names_a_file_yields_its_path() {
        for url in [
            "mkv:///m/Movie.mkv",
            "m2ts:///m/Movie.m2ts",
            "mp4:///m/Movie.mp4",
            "iso:///m/Disc.iso",
            "dir:///m/BDMV",
            "demux:///m/tracks",
            "video:///m/tracks",
            "audio:///m/tracks",
            "sub:///m/tracks",
            "fvi:///m/Movie.fvi",
            "chapters:///m/Movie.xml",
            "json:///m/Movie.json",
        ] {
            assert!(
                url_path_of(&libfreemkv::parse_url(url)).is_some(),
                "{url} names a file on disk; the guard must be able to see it"
            );
        }
        // The schemes with no filesystem path at all.
        for url in [
            "disc://",
            "disc:///dev/sg0",
            "network://1.2.3.4:9000",
            "stdio://",
            "null://",
        ] {
            assert!(
                url_path_of(&libfreemkv::parse_url(url)).is_none(),
                "{url} has no path to compare"
            );
        }
    }

    /// Run the preflight gate the way `run` does.
    fn preflight(source: &str, dest: &str) -> Result<(), String> {
        let ps = libfreemkv::parse_url(source);
        let pd = libfreemkv::parse_url(dest);
        // `force = true` so a `dir://` destination is not refused for merely
        // being non-empty — the same-file refusal must not depend on which
        // other check happens to fire first.
        preflight_validate(source, dest, &ps, &pd, false, false, true, false)
    }

    fn refused(source: &str, dest: &str) -> String {
        match preflight(source, dest) {
            Err(m) => m,
            Ok(()) => panic!("{source} → {dest} was ACCEPTED; it destroys the source"),
        }
    }

    /// THE defect. For each file-backed scheme, `X` as both source and
    /// destination is refused — and the source still holds its original bytes
    /// afterwards. Contents, not existence: a truncated file still exists.
    #[test]
    fn a_file_backed_scheme_never_writes_over_its_own_source() {
        let t = Tmp::new("self_dest");
        const BODY: &[u8] = b"the user's only copy of the movie";
        for (scheme, name) in [
            ("mkv", "Movie.mkv"),
            ("m2ts", "Movie.m2ts"),
            ("mp4", "Movie.mp4"),
            ("iso", "Disc.iso"),
        ] {
            let p = t.file(name, BODY);
            let url = format!("{scheme}://{}", p.display());
            let msg = refused(&url, &url);
            assert!(
                msg.contains("overwrite the source"),
                "{scheme}:// self-destination must be refused by name, got: {msg}"
            );
            assert_eq!(
                std::fs::read(&p).expect("source still readable"),
                BODY,
                "{scheme}:// self-destination truncated the source"
            );
        }
        // `dir://` is a source AND a sink; extracting a tree over itself is the
        // same defect with a directory instead of a file.
        let d = t.dir("BDMV");
        std::fs::write(d.join("index.bdmv"), BODY).expect("write tree file");
        let url = format!("dir://{}", d.display());
        refused(&url, &url);
        assert_eq!(
            std::fs::read(d.join("index.bdmv")).expect("tree file survives"),
            BODY,
            "dir:// self-destination clobbered the source tree"
        );
    }

    /// The pairing does not have to share a scheme. Every WRITE-ONLY sink
    /// aimed at the source's own path is the same truncation.
    #[test]
    fn a_write_only_sink_never_aims_at_the_source_path() {
        let t = Tmp::new("cross_scheme");
        const BODY: &[u8] = b"the user's only copy of the disc image";
        let p = t.file("Disc.iso", BODY);
        let source = format!("iso://{}", p.display());
        for dest_scheme in ["mkv", "m2ts", "mp4", "fvi", "chapters", "json"] {
            let dest = format!("{dest_scheme}://{}", p.display());
            let msg = refused(&source, &dest);
            assert!(
                msg.contains("overwrite the source"),
                "iso:// → {dest_scheme}:// onto the same path must be refused, got: {msg}"
            );
            assert_eq!(
                std::fs::read(&p).expect("source still readable"),
                BODY,
                "iso:// → {dest_scheme}:// truncated the source"
            );
        }
        // The per-track directory sinks, pointed at the source TREE.
        let d = t.dir("BDMV");
        std::fs::write(d.join("index.bdmv"), BODY).expect("write tree file");
        let source = format!("dir://{}", d.display());
        for dest_scheme in ["demux", "video", "audio", "sub", "dir"] {
            let dest = format!("{dest_scheme}://{}", d.display());
            refused(&source, &dest);
        }
        assert_eq!(
            std::fs::read(d.join("index.bdmv")).expect("tree file survives"),
            BODY
        );
    }

    /// The case a string compare misses. `./Movie.mkv` and `Movie.mkv` are one
    /// file; so is a symlink to it, and so is a hardlink — which canonicalize
    /// alone does NOT resolve, because both names are already canonical.
    #[test]
    fn a_second_spelling_of_the_source_is_still_the_source() {
        let t = Tmp::new("spelling_dest");
        const BODY: &[u8] = b"one file, several names";
        let p = t.file("Movie.mkv", BODY);
        let source = format!("mkv://{}", p.display());

        // `sub/../Movie.mkv` — `Path`'s `==` cannot normalise `..` away, so
        // only a real canonicalize resolves this.
        let detour = t.dir("sub").join("..").join("Movie.mkv");
        assert_ne!(detour.as_path(), p.as_path(), "the fixture proves nothing");
        refused(&source, &format!("mkv://{}", detour.display()));

        // A symlink pointing at the source.
        #[cfg(unix)]
        {
            let link = t.0.join("Link.mkv");
            std::os::unix::fs::symlink(&p, &link).expect("symlink");
            refused(&source, &format!("mkv://{}", link.display()));

            // A HARDLINK. Both names canonicalize to themselves, so a
            // canonical-path compare says "different file" and the write
            // destroys the source anyway. Only dev+inode catches it.
            let hard = t.0.join("Hard.mkv");
            std::fs::hard_link(&p, &hard).expect("hard link");
            assert_ne!(
                std::fs::canonicalize(&p).expect("canon src"),
                std::fs::canonicalize(&hard).expect("canon hard"),
                "a hardlink must NOT be equal by canonical path, or it proves nothing"
            );
            refused(&source, &format!("mkv://{}", hard.display()));
        }

        assert_eq!(
            std::fs::read(&p).expect("source still readable"),
            BODY,
            "an aliased destination truncated the source"
        );
    }

    /// Different files still rip. The guard must not cost the ordinary case,
    /// which is every real invocation.
    #[test]
    fn two_different_files_are_still_accepted() {
        let t = Tmp::new("distinct_dest");
        let a = t.file("a.mkv", b"aaa");
        let b = t.file("b.mkv", b"bbb");
        for (src_scheme, dest_scheme) in [("mkv", "mkv"), ("iso", "mkv"), ("m2ts", "mp4")] {
            let source = format!("{src_scheme}://{}", a.display());
            let dest = format!("{dest_scheme}://{}", b.display());
            assert!(
                preflight(&source, &dest).is_ok(),
                "{source} → {dest} is two different files and must be allowed"
            );
        }
        // And the overwhelmingly common shape: a destination that does not
        // exist yet.
        let fresh = t.0.join("new.mkv");
        assert!(
            preflight(
                &format!("iso://{}", a.display()),
                &format!("mkv://{}", fresh.display()),
            )
            .is_ok(),
            "a destination that does not exist yet can never be the source"
        );
    }

    // See docs/pipe.md#the_preflight_gate_actually_calls_the_guard — The guard is WIRED into the gate that…
    #[test]
    fn the_preflight_gate_actually_calls_the_guard() {
        let src = include_str!("pipe.rs").replace("\r\n", "\n");
        let start = src
            .find("\nfn preflight_validate(")
            .expect("preflight_validate definition present");
        let end = start
            + src[start..]
                .find("\n/// Validate a `dir://` destination")
                .expect("the next item still ends preflight_validate");
        let body = &src[start..end];
        assert!(
            body.contains("same_file(") && body.contains("url_path_of("),
            "preflight_validate must refuse a destination that IS the source, \
             before any sink is opened for writing"
        );
    }
}

// ── Disc language tags reach a real terminal ─────────────────────────────────
// A language tag is raw MPLS/IFO bytes; unlike `print_stream_info`, these
// error renderers didn't sanitise it — `ESC c` (a full reset) fits in 3 bytes.
#[cfg(test)]
mod language_escape_tests {
    use super::render_stream_sel_error;

    fn title_with_language(lang: &str) -> libfreemkv::DiscTitle {
        libfreemkv::DiscTitle {
            playlist: "00800.mpls".into(),
            playlist_id: 800,
            duration_secs: 60.0,
            size_bytes: 1 << 30,
            clips: Vec::new(),
            streams: vec![libfreemkv::Stream::Audio(libfreemkv::AudioStream {
                pid: 0x1100,
                codec: libfreemkv::Codec::TrueHd,
                channels: libfreemkv::AudioChannels::Unknown,
                language: lang.to_string(),
                sample_rate: libfreemkv::SampleRate::Unknown,
                secondary: false,
                purpose: libfreemkv::LabelPurpose::Normal,
                label: String::new(),
            })],
            chapters: Vec::new(),
            extents: Vec::new(),
            content_format: libfreemkv::ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }
    }

    /// The "languages available on this title" list is disc bytes.
    #[test]
    fn a_crafted_language_tag_cannot_reach_the_terminal_raw() {
        // ESC c is RIS: a full terminal reset, and it fits in three bytes.
        let hostile = "\u{1b}c\n";
        let title = title_with_language(hostile);
        let msg = render_stream_sel_error(
            &freemkv_engine::StreamSelError::UnknownLanguage { tag: "zz".into() },
            &title,
        );
        assert!(
            !msg.chars().any(|c| c.is_control() && c != '\n'),
            "an escape sequence survived into terminal output: {msg:?}"
        );
        assert!(
            !msg.contains('\u{1b}'),
            "ESC survived into terminal output: {msg:?}"
        );
    }

    /// An ordinary tag still names the language it was there to name.
    #[test]
    fn an_ordinary_language_tag_is_still_shown() {
        let title = title_with_language("eng");
        let msg = render_stream_sel_error(
            &freemkv_engine::StreamSelError::UnknownLanguage { tag: "zz".into() },
            &title,
        );
        assert!(msg.contains("eng"), "got {msg:?}");
    }
}

// ── A title's POSITION is not its identity across an independent re-scan ──────
// `-t all` runs N+1 scans sharing only an integer index; a reordered later
// scan can silently mux the wrong title. Drives `resolve_scanned_title` directly.
#[cfg(test)]
mod title_identity_tests {
    use super::{
        TitleIdentity, disc_title_nums, job_identity, resolve_disc_all_titles,
        resolve_scanned_title, title_changed_message,
    };

    // See docs/pipe.md#a_failed_all_titles_scan_aborts_instead_of_ripping_title_one — A `-t all` DISC rip whose upfront scan…
    #[test]
    fn a_failed_all_titles_scan_aborts_instead_of_ripping_title_one() {
        let scan = first_scan();
        let ids: Vec<TitleIdentity> = scan.iter().map(TitleIdentity::of).collect();

        // Scan succeeded: `-t all` expands to every scanned title, identities
        // carried through so each job can verify against its own title.
        let (nums, got_ids) =
            resolve_disc_all_titles(&[], Some(ids.clone())).expect("a good scan proceeds");
        assert_eq!(nums, vec![1, 2, 3, 4]);
        assert_eq!(got_ids, ids);

        // Scan FAILED: the resolver returns None so the caller aborts loudly.
        // Anything other than None here is the resurrected rc-0-over-title-1 bug.
        assert!(
            resolve_disc_all_titles(&[], None).is_none(),
            "a failed -t all scan must abort, not silently rip one title"
        );
    }

    // See docs/pipe.md#title — A title with a distinct playlist and distinct…
    fn title(playlist_id: u16, start_lba: u32) -> libfreemkv::DiscTitle {
        libfreemkv::DiscTitle {
            playlist: format!("{:05}.mpls", playlist_id),
            playlist_id,
            duration_secs: 3600.0,
            size_bytes: 20 << 30,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![libfreemkv::Extent {
                start_lba,
                sector_count: 1000,
            }],
            content_format: libfreemkv::ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }
    }

    /// The first scan: what the job list was built against.
    fn first_scan() -> Vec<libfreemkv::DiscTitle> {
        vec![
            title(800, 1000),
            title(801, 5000),
            title(802, 9000),
            title(803, 13000),
        ]
    }

    // See docs/pipe.md#a_reordered_rescan_fails_loudly_instead_of_muxing_the_wrong_title — THE DEFECT. The re-scan returns the same titles…
    #[test]
    fn a_reordered_rescan_fails_loudly_instead_of_muxing_the_wrong_title() {
        let expected = TitleIdentity::of(&first_scan()[1]);
        let rescan = vec![
            title(800, 1000),
            title(802, 9000), // ← swapped with 801
            title(801, 5000),
            title(803, 13000),
        ];
        let err = resolve_scanned_title(&rescan, 1, Some(&expected))
            .err()
            .unwrap_or_else(|| {
                panic!(
                    "a reordered re-scan resolved index 1 to {}, and the rip would have muxed \
                     it as title 2",
                    rescan[1].playlist
                )
            });
        assert!(
            err.contains("00801.mpls") && err.contains("00802.mpls"),
            "the error must name the title that was expected and the one found: {err:?}"
        );
    }

    /// The other half of what `title_in_range` misses: the list SHRANK, but not
    /// enough for the index to fall off the end. Dropping 801 slides 803 into
    /// index 2, so the old range check passes and the wrong title is muxed.
    #[test]
    fn a_rescan_that_drops_an_earlier_title_fails_rather_than_shifting() {
        let expected = TitleIdentity::of(&first_scan()[2]);
        let rescan = vec![title(800, 1000), title(802, 9000), title(803, 13000)];
        assert!(
            resolve_scanned_title(&rescan, 2, Some(&expected)).is_err(),
            "index 2 is in range but now names 00803.mpls, not the requested 00802.mpls"
        );
    }

    /// The identity must not be defined in terms of the OTHER titles, or a disc
    /// whose re-scan simply misses one trailing title would fail every job. The
    /// requested title is still at its index and still itself: rip it.
    #[test]
    fn dropping_an_unrelated_later_title_still_rips_the_requested_one() {
        let expected = TitleIdentity::of(&first_scan()[1]);
        let rescan = vec![title(800, 1000), title(801, 5000), title(802, 9000)];
        let got = resolve_scanned_title(&rescan, 1, Some(&expected))
            .expect("the requested title is unchanged; dropping 00803.mpls is irrelevant to it");
        assert_eq!(got.playlist, "00801.mpls");
    }

    /// THE NORMAL PATH. A stable disc re-scans identically, and every title
    /// resolves to exactly the one its job was built for.
    #[test]
    fn a_stable_disc_resolves_every_title_exactly_as_before() {
        let scan = first_scan();
        for (idx, want) in scan.iter().enumerate() {
            let expected = TitleIdentity::of(want);
            let got = resolve_scanned_title(&scan, idx, Some(&expected))
                .expect("an unchanged re-scan must resolve every title");
            assert_eq!(got.playlist, want.playlist, "at index {idx}");
        }
    }

    /// An explicit `-t N` on a disc scans exactly once, so there is no earlier
    /// list to disagree with and the index is the only reference there is. That
    /// path must keep working — but the range rule still applies.
    #[test]
    fn with_no_earlier_scan_the_index_is_still_range_checked() {
        let scan = first_scan();
        assert_eq!(
            resolve_scanned_title(&scan, 3, None)
                .expect("no expectation recorded → rip what the index names")
                .playlist,
            "00803.mpls"
        );
        assert!(
            resolve_scanned_title(&scan, 4, None).is_err(),
            "one past the end is still out of range"
        );
        let expected = TitleIdentity::of(&scan[3]);
        assert!(
            resolve_scanned_title(&scan[..2], 3, Some(&expected)).is_err(),
            "a shortened list that drops the index off the end still fails"
        );
    }

    /// The constraint that shapes the identity: duplicate playlists with
    /// identical duration and size are LEGITIMATE, so neither field can tell
    /// two titles apart. Identity has to come from the playlist and the sectors.
    #[test]
    fn identity_is_not_duration_or_size() {
        let a = title(800, 1000);
        let b = title(801, 5000);
        assert_eq!(a.duration_secs, b.duration_secs);
        assert_eq!(a.size_bytes, b.size_bytes);
        assert_ne!(
            TitleIdentity::of(&a),
            TitleIdentity::of(&b),
            "two titles that differ only by playlist and sectors must not share an identity"
        );
    }

    /// Even same-named playlists are separated, as long as they read different
    /// sectors — and if they read the same sectors from the same playlist, the
    /// two rips are byte-identical, so there is nothing to confuse.
    #[test]
    fn same_playlist_name_over_different_sectors_is_a_different_title() {
        let mut a = title(800, 1000);
        let mut b = title(800, 9000);
        a.playlist = "FEATURE".into();
        b.playlist = "FEATURE".into();
        assert_ne!(TitleIdentity::of(&a), TitleIdentity::of(&b));
    }

    // See docs/pipe.md#every_expanded_job_looks_up_the_identity_of_its_own_title — The `-t all` expansion and the identity list…
    #[test]
    fn every_expanded_job_looks_up_the_identity_of_its_own_title() {
        let scan = first_scan();
        let identities: Vec<TitleIdentity> = scan.iter().map(TitleIdentity::of).collect();
        let nums = disc_title_nums(true, &[], identities.len());
        assert_eq!(nums, vec![1, 2, 3, 4]);
        for num in nums {
            let idx = num - 1; // what `build_jobs` stores
            let got = job_identity(&identities, Some(idx)).expect("every job has an identity");
            assert_eq!(*got, TitleIdentity::of(&scan[idx]), "job for -t {num}");
            // And it is the identity that lets THAT title through its own re-scan.
            resolve_scanned_title(&scan, idx, Some(got)).expect("stable disc");
        }
        assert!(
            job_identity(&[], Some(0)).is_none(),
            "no upfront scan → nothing to verify against"
        );
    }

    /// The mismatch message is the whole user-visible surface of this fix: it
    /// must read as a sentence, never as the raw `error.title_changed` key that
    /// the pinned i18n tag does not ship yet.
    #[test]
    fn the_mismatch_message_is_readable_and_carries_no_terminal_escapes() {
        let mut hostile = title(801, 5000);
        // The playlist name is on-disc metadata: three bytes are enough for
        // ESC c, a full terminal reset.
        hostile.playlist = "\u{1b}c\n801".into();
        let msg = title_changed_message(
            2,
            &TitleIdentity::of(&hostile),
            &TitleIdentity::of(&title(802, 9000)),
        );
        assert!(
            !msg.starts_with("error."),
            "the raw key path reached the user: {msg:?}"
        );
        assert!(
            !msg.contains('\u{1b}'),
            "ESC survived into terminal output: {msg:?}"
        );
        assert!(msg.contains("00802.mpls"), "got {msg:?}");
    }
}

// ── Pure progress/stream formatters and the CLI mux-event callbacks ──────────
// Each is reachable in production only behind a real drive or disc image, so
// CI never exercised their branches; tested here as the strings they emit.
#[cfg(test)]
mod formatter_tests {
    use super::{
        CliMuxEvents, fmt_damage_time, fmt_eta, fmt_speed, print_mp4_skips, print_stream_info,
        render_resolution_trace,
    };
    use crate::output::{Output, capture};
    use libfreemkv::MuxEvents;

    fn loud() -> Output {
        Output::new(false, false)
    }

    fn video() -> libfreemkv::Stream {
        libfreemkv::Stream::Video(libfreemkv::VideoStream {
            pid: 0x1011,
            codec: libfreemkv::Codec::Hevc,
            resolution: libfreemkv::Resolution::R2160p,
            frame_rate: libfreemkv::FrameRate::F23_976,
            hdr: libfreemkv::HdrFormat::Hdr10,
            color_space: libfreemkv::ColorSpace::Bt2020,
            display_aspect: None,
            secondary: false,
            label: String::new(),
            measured_cicp: None,
        })
    }

    fn audio(codec: libfreemkv::Codec) -> libfreemkv::Stream {
        libfreemkv::Stream::Audio(libfreemkv::AudioStream {
            pid: 0x1100,
            codec,
            channels: libfreemkv::AudioChannels::Surround51,
            language: "eng".into(),
            sample_rate: libfreemkv::SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        })
    }

    fn subtitle() -> libfreemkv::Stream {
        libfreemkv::Stream::Subtitle(libfreemkv::SubtitleStream {
            pid: 0x1200,
            codec: libfreemkv::Codec::Pgs,
            language: "eng".into(),
            forced: false,
            qualifier: libfreemkv::LabelQualifier::None,
            codec_data: None,
        })
    }

    fn title(streams: Vec<libfreemkv::Stream>, duration_secs: f64) -> libfreemkv::DiscTitle {
        libfreemkv::DiscTitle {
            playlist: "00800.mpls".into(),
            playlist_id: 800,
            duration_secs,
            size_bytes: 40 << 30,
            clips: Vec::new(),
            streams,
            chapters: Vec::new(),
            extents: Vec::new(),
            content_format: libfreemkv::ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }
    }

    #[test]
    fn fmt_speed_scales_through_mb_kb_b_and_stall() {
        assert_eq!(fmt_speed(12.5), "12.5 MB/s");
        assert_eq!(fmt_speed(0.5), "512 KB/s");
        assert_eq!(fmt_speed(0.0004), "419 B/s");
        assert_eq!(fmt_speed(0.0), "stalled");
    }

    #[test]
    fn fmt_eta_shows_mmss_hhmmss_and_a_placeholder_for_unknown() {
        assert_eq!(fmt_eta(0.0), "?:??");
        assert_eq!(fmt_eta(f64::INFINITY), "?:??");
        assert_eq!(fmt_eta(65.0), "1:05");
        assert_eq!(fmt_eta(3725.0), "1:02:05");
    }

    #[test]
    fn fmt_damage_time_picks_the_unit_by_magnitude() {
        assert_eq!(fmt_damage_time(7200.0), "2.0h");
        assert_eq!(fmt_damage_time(90.0), "2m");
        assert_eq!(fmt_damage_time(5.0), "5s");
        assert_eq!(fmt_damage_time(0.25), "0.25s");
        assert_eq!(fmt_damage_time(0.004), "4ms");
    }

    #[test]
    fn print_stream_info_lists_every_stream_and_the_runtime() {
        crate::strings::set_locale("en");
        let t = title(
            vec![video(), audio(libfreemkv::Codec::TrueHd), subtitle()],
            7530.0,
        );
        let (_, printed) = capture(|| print_stream_info(&loud(), &t));
        assert!(printed.contains("Streams: 3"), "got:\n{printed}");
        assert!(printed.contains("HEVC 2160p"), "got:\n{printed}");
        assert!(printed.contains("TrueHD 5.1 eng"), "got:\n{printed}");
        assert!(printed.contains("PGS eng"), "got:\n{printed}");
        // 7530s = 2:05:30, rendered h:mm:ss.
        assert!(printed.contains("2:05:30"), "runtime line, got:\n{printed}");
    }

    #[test]
    fn print_stream_info_omits_the_runtime_when_unknown() {
        crate::strings::set_locale("en");
        let t = title(vec![video()], 0.0);
        let (_, printed) = capture(|| print_stream_info(&loud(), &t));
        assert!(
            !printed.contains("Duration"),
            "no runtime line, got:\n{printed}"
        );
    }

    #[test]
    fn print_mp4_skips_names_the_tracks_mp4_cannot_carry() {
        crate::strings::set_locale("en");
        // TrueHD (unmappable audio) + PGS (bitmap subtitle) cannot ride in MP4.
        let t = title(
            vec![video(), audio(libfreemkv::Codec::TrueHd), subtitle()],
            7530.0,
        );
        let (_, printed) = capture(|| print_mp4_skips(&loud(), "mp4:///out/x.mp4", &t));
        assert!(
            printed.contains("left out"),
            "the header warns, got:\n{printed}"
        );
        assert!(
            printed.contains("Track"),
            "each skip is named, got:\n{printed}"
        );

        // A non-mp4 destination is a no-op, whatever the title carries.
        let (_, none) = capture(|| print_mp4_skips(&loud(), "mkv:///out/x.mkv", &t));
        assert!(none.is_empty(), "mkv:// prints nothing, got:\n{none}");

        // An all-mappable title (H.264 + AC-3) also prints nothing.
        let clean = title(
            vec![
                libfreemkv::Stream::Video(libfreemkv::VideoStream {
                    pid: 0x1011,
                    codec: libfreemkv::Codec::H264,
                    resolution: libfreemkv::Resolution::R1080p,
                    frame_rate: libfreemkv::FrameRate::F23_976,
                    hdr: libfreemkv::HdrFormat::Sdr,
                    color_space: libfreemkv::ColorSpace::Bt709,
                    display_aspect: None,
                    secondary: false,
                    label: String::new(),
                    measured_cicp: None,
                }),
                audio(libfreemkv::Codec::Ac3),
            ],
            7530.0,
        );
        let (_, empty) = capture(|| print_mp4_skips(&loud(), "mp4:///out/x.mp4", &clean));
        assert!(
            empty.is_empty(),
            "a fully-mappable title is silent, got:\n{empty}"
        );
    }

    #[test]
    fn on_output_opened_prints_the_stream_block_and_arms_the_clock() {
        crate::strings::set_locale("en");
        let t = title(vec![video(), audio(libfreemkv::Codec::TrueHd)], 60.0);
        let events = CliMuxEvents::new(loud(), "mkv:///out/x.mkv".into(), false);
        assert!(
            events.start().is_none(),
            "the clock is unset until the sink opens"
        );
        let (_, printed) = capture(|| events.on_output_opened(&t));
        assert!(
            printed.contains("Streams: 2"),
            "the stream block prints, got:\n{printed}"
        );
        assert!(
            events.start().is_some(),
            "on_output_opened arms the completion clock"
        );
    }

    #[test]
    fn on_write_progress_is_silent_when_quiet() {
        // A `stdio://` rip routes to stderr and runs quiet — a progress repaint
        // must never interleave into the piped byte stream.
        let events = CliMuxEvents::new(Output::new(false, true), "stdio://".into(), false);
        let (_, printed) = capture(|| events.on_write_progress(1_000, 2_000));
        assert!(
            printed.is_empty(),
            "quiet suppresses progress, got:\n{printed}"
        );
    }

    #[test]
    fn render_resolution_trace_maps_unlock_and_key_steps() {
        use libfreemkv::aacs::trace::{
            KeyNode, KeyOutcome, KeyStep, ResolutionTrace, UnlockOutcome, UnlockStep,
        };
        let mut trace = ResolutionTrace::new();
        trace.unlock.push(UnlockStep {
            who: "firmware".into(),
            outcome: UnlockOutcome::Unlocked,
        });
        trace.unlock.push(UnlockStep {
            who: "host-cert".into(),
            outcome: UnlockOutcome::NoUsableHostCert { mkb: Some(64) },
        });
        trace.keys.push(KeyStep {
            who: "keydb".into(),
            path: vec![
                KeyNode::MatchedDisc,
                KeyNode::FoundVuk,
                KeyNode::DerivedUnitKeys,
            ],
            outcome: KeyOutcome::Resolved,
        });
        trace.keys.push(KeyStep {
            who: "online".into(),
            path: vec![KeyNode::NoEntry],
            outcome: KeyOutcome::NoKey,
        });

        let lines = render_resolution_trace(&trace);
        assert_eq!(
            lines,
            vec![
                "unlock: firmware > UNLOCKED".to_string(),
                "unlock: host-cert > no usable host cert (MKBv64)".to_string(),
                "key: keydb > matched disc > found VUK > derived unit keys > RESOLVED".to_string(),
                "key: online > no entry > NO KEY".to_string(),
            ]
        );

        // An empty trace renders nothing.
        assert!(render_resolution_trace(&ResolutionTrace::new()).is_empty());
    }
}
