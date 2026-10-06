//! The launch decision and the two steps every desktop launch performs first.
//!
//! Deliberately portable and Win32/AppKit-free: it deals in `&str` and
//! closures, never a platform handle, so both shells can share it and
//! `wants_gui` stays unit-testable on any machine. Callers pass the two
//! settings *values* (`language`, `log_level`) rather than a `Settings`, so
//! the binary and the lib never have to agree on a struct identity.

/// True when this invocation should open the desktop UI rather than the CLI.
///
/// Only the app build (`--features gui`) asks. A bare `freemkv` opens the window, and so does
/// an explicit `freemkv gui`; any other argument is a CLI invocation, so the app answers every
/// command the CLI does. `display` is false when no window can be drawn (a Linux SSH session), in
/// which case a bare launch prints usage exactly as the CLI build does. Neither a macOS process
/// serial nor a language flag is a command (see [`command_args`]).
pub fn wants_gui(args: &[String], display: bool) -> bool {
    match command_args(args).first().map(String::as_str) {
        Some("gui") => true,
        Some(_) => false,
        None => display,
    }
}

/// The source a `freemkv gui <file-or-url>` launch opens in place of the drive probe: the first
/// non-flag argument after `gui`.
pub fn gui_source(args: &[String]) -> Option<String> {
    let rest = command_args(args);
    (rest.first().map(String::as_str) == Some("gui"))
        .then(|| rest.get(1).filter(|a| !is_flag_token(a)).cloned())
        .flatten()
}

static LAUNCH_SOURCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the source this launch opens; the first call wins.
pub fn set_launch_source(source: Option<String>) {
    if let Some(s) = source.filter(|s| !s.is_empty()) {
        let _ = LAUNCH_SOURCE.set(s);
    }
}

/// The source given on the command line, if any. Each shell opens it at startup instead of
/// probing the drive.
pub fn launch_source() -> Option<&'static str> {
    LAUNCH_SOURCE.get().map(String::as_str)
}

/// A LaunchServices process-serial argument (`-psn_0_<n>`), passed on the first Finder launch of
/// a quarantined app. Not a flag of this CLI on any platform, so it is ignored everywhere.
pub fn is_process_serial(arg: &str) -> bool {
    arg.starts_with("-psn_")
}

/// The `--language` spellings. Shared with `cli_entry::strip_language_flag`.
pub const LANGUAGE_FLAGS: [&str; 2] = ["--language", "--lang"];

/// Whether a token may be a flag's value: neither a `scheme://` URL nor another flag.
pub fn is_flag_value(v: &str) -> bool {
    !is_url_token(v) && !is_flag_token(v)
}

/// Whether a token is a stream URL (`scheme://...`). ONE rule for every parser.
pub fn is_url_token(s: &str) -> bool {
    s.contains("://")
}

/// Whether a token is a flag. A negative number (`-1`) is a value, not a flag.
pub fn is_flag_token(s: &str) -> bool {
    let mut rest = s.strip_prefix('-').unwrap_or("").chars();
    match rest.next() {
        None => false,
        Some(c) => !c.is_ascii_digit(),
    }
}

// argv past argv[0], a leading process serial and every language flag (with its value, when it
// has one), by the CLI's own rule.
fn command_args(args: &[String]) -> Vec<String> {
    let mut rest = args.get(1..).unwrap_or_default();
    if rest.first().is_some_and(|a| is_process_serial(a)) {
        rest = &rest[1..];
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if LANGUAGE_FLAGS.contains(&rest[i].as_str()) {
            i += if rest.get(i + 1).is_some_and(|v| is_flag_value(v)) {
                2
            } else {
                1
            };
        } else {
            out.push(rest[i].clone());
            i += 1;
        }
    }
    out
}

/// The language a `--lang <code>` on the launch command line asks for; the last one wins, as
/// in the CLI.
pub fn launch_language(args: &[String]) -> Option<String> {
    args.windows(2)
        .filter(|w| LANGUAGE_FLAGS.contains(&w[0].as_str()) && is_flag_value(&w[1]))
        .map(|w| w[1].clone())
        .next_back()
}

static LAUNCH_LANGUAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Record the launch command line's language, which [`apply_locale`] then prefers over the saved
/// setting for this run. Returns whether one was given.
pub fn set_launch_language(args: &[String]) -> bool {
    match launch_language(args) {
        Some(l) => {
            let _ = LAUNCH_LANGUAGE.set(l);
            true
        }
        None => false,
    }
}

// The launch language wins when it names a known locale (or "auto"); an unknown code would
// silently mean "Auto", so the saved choice is kept instead.
fn chosen_language<'a>(saved: &'a str, launch: Option<&'a str>) -> &'a str {
    match launch {
        Some(l) if l.eq_ignore_ascii_case("auto") || crate::ui::locale_code(l) != "auto" => l,
        _ => saved,
    }
}

/// Can a window be drawn? Only Linux asks: over SSH neither variable is set (or one is empty),
/// and a bare launch must print usage rather than fail inside GTK. `get` is `std::env::var_os`.
pub fn display_present(get: impl Fn(&'static str) -> Option<std::ffi::OsString>) -> bool {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .iter()
        .any(|v| get(v).is_some_and(|x| !x.is_empty()))
}

/// Where the Windows console image looks for its windowed sibling, in order, never `me`:
/// `freemkv.exe` beside the shipped `freemkv.com`, then `freemkv-gui.exe` in a cargo build dir.
pub fn windowed_candidates(me: &std::path::Path) -> Vec<std::path::PathBuf> {
    [
        me.with_extension("exe"),
        me.with_file_name("freemkv-gui.exe"),
    ]
    .into_iter()
    .filter(|p| p != me)
    .collect()
}

/// Apply the saved interface language before the shell builds anything, so the
/// first string lookup resolves in the right locale; a launch `--lang` (see
/// [`set_launch_language`]) wins for this run. (A later change in Settings switches live via
/// `strings::set_locale`.)
///
/// `system_locale` is the platform's "what language is this PC in?" call,
/// passed in rather than `cfg`-selected here: a Finder-launched `.app` and a
/// double-clicked `.exe` both inherit no `LANG`, so the i18n crate's env
/// detection would fall back to English for the "Auto" setting.
pub fn apply_locale(language: &str, system_locale: impl FnOnce() -> Option<String>) {
    let language = chosen_language(language, LAUNCH_LANGUAGE.get().map(String::as_str));
    // Not `set_language`: its override would pin a later live "Auto".
    if let Some(tag) = resolved_locale(language, system_locale) {
        crate::strings::set_locale(&tag);
    }
}

/// The locale a language setting resolves to: its code, or for "Auto" the
/// platform's own locale (`None` when that is unknown).
pub fn resolved_locale(
    language: &str,
    system_locale: impl FnOnce() -> Option<String>,
) -> Option<String> {
    match crate::ui::locale_code(language) {
        "auto" => system_locale(),
        code => Some(code.to_string()),
    }
}

/// The GUI's diagnostic log, in the app-support dir.
const GUI_LOG_NAME: &str = "log.txt";

// How large that log may already be at startup before it is started over. `rolling::never`
// never rotates on its own.
const GUI_LOG_CAP_BYTES: u64 = 8 * 1024 * 1024;

// Start a fresh diagnostic log when the existing one has already grown past `cap`, instead of
// appending to it for another session. Best-effort: a log that can't be removed must not stop
// the app.
fn trim_oversized_log(path: &std::path::Path, cap: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > cap) {
        let _ = std::fs::remove_file(path);
    }
}

// Open the GUI log for append. `None` when it can't be opened (`rolling::never` would panic).
fn open_gui_log(dir: &std::path::Path) -> Option<tracing_appender::rolling::RollingFileAppender> {
    tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::NEVER)
        .filename_prefix(GUI_LOG_NAME)
        .build(dir)
        .ok()
}

/// Diagnostic-log guard for the GUI (keeps the non-blocking writer alive).
static GUI_LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

/// Install a file tracing subscriber from the GUI's log settings. Only "Verbose"
/// or the "Log debug messages" toggle turn it on (mapping to debug / trace);
/// Quiet and Normal install nothing, exactly like the CLI's common path. The log
/// is written to `log.txt` in the app-support dir, never the terminal.
pub fn init_gui_logging(log_level: &str) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let level = match log_level {
        "Debug" => "trace",
        "Verbose" => "debug",
        // Quiet / Normal: no diagnostic file log.
        _ => return,
    };

    let dir = crate::settings::support_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    trim_oversized_log(&dir.join(GUI_LOG_NAME), GUI_LOG_CAP_BYTES);
    let Some(file_appender) = open_gui_log(&dir) else {
        return;
    };
    let (nb, guard) = tracing_appender::non_blocking(file_appender);
    let _ = GUI_LOG_GUARD.set(guard);
    let filter = EnvFilter::new(format!("error,freemkv={level},libfreemkv={level}"));
    // try_init: never panic if a subscriber somehow already exists.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_ansi(false).with_writer(nb))
        .try_init();
}

#[cfg(test)]
#[path = "app_entry_tests.rs"]
mod tests;
