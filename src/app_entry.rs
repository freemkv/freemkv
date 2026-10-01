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
    let code = crate::ui::locale_code(language);
    if code == "auto" {
        if let Some(sys) = system_locale() {
            crate::strings::set_locale(&sys);
        }
    } else {
        // Not `set_language`: its override would pin a later live "Auto".
        crate::strings::set_locale(code);
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
mod tests {
    use super::{
        GUI_LOG_CAP_BYTES, chosen_language, display_present, launch_language, trim_oversized_log,
        wants_gui, windowed_candidates,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    // `rolling::never` panicked on an unopenable log; the app must start without one.
    #[test]
    fn an_unopenable_gui_log_yields_none_instead_of_panicking() {
        let dir = std::env::temp_dir().join(format!("freemkv-guilog-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(super::GUI_LOG_NAME)).unwrap();
        assert!(super::open_gui_log(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // The GUI's diagnostic log must not grow without end; `rolling::never` never rotates on its
    // own.
    #[test]
    fn a_diagnostic_log_past_the_cap_is_started_over_not_appended_to() {
        let dir = std::env::temp_dir().join(format!(
            "fmkv-guilog-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log.txt");

        // Under the cap: a session's history is worth keeping.
        std::fs::write(&log, vec![b'x'; 64]).unwrap();
        trim_oversized_log(&log, 128);
        assert!(
            log.exists(),
            "a log below the cap must survive — this is the ordinary case"
        );

        // Past it: started over rather than appended to for another session.
        std::fs::write(&log, vec![b'x'; 129]).unwrap();
        trim_oversized_log(&log, 128);
        assert!(
            !log.exists(),
            "a log past the cap is appended to forever; nothing else rotates \
             or truncates it"
        );

        // A log that was never written is not an error.
        trim_oversized_log(&log, 128);

        // A zero cap would throw the log away at every startup, capped or not.
        assert_ne!(GUI_LOG_CAP_BYTES, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("freemkv".to_string())
            .chain(rest.iter().map(|s| s.to_string()))
            .collect()
    }

    #[test]
    fn bare_launch_opens_the_window_when_one_can_be_drawn() {
        assert!(wants_gui(&argv(&[]), true));
        // No display (SSH): the CLI contract, usage and exit 2.
        assert!(!wants_gui(&argv(&[]), false));
    }

    #[test]
    fn explicit_gui_subcommand_opens_the_window() {
        assert!(wants_gui(&argv(&["gui"]), true));
        assert!(wants_gui(&argv(&["gui", "--verbose"]), false));
    }

    #[test]
    fn every_other_invocation_is_the_cli() {
        for a in [
            "--version",
            "--help",
            "-h",
            "info",
            "version",
            "update-keys",
            "disc://",
            "/dev/disk2",
            "GUI",   // case-sensitive: not the subcommand
            "gui.x", // no prefix matching
        ] {
            assert!(!wants_gui(&argv(&[a]), true), "{a} should route to the CLI");
        }
        // `freemkv info gui` is an `info` invocation.
        assert!(!wants_gui(&argv(&["info", "gui"]), true));
    }

    // LaunchServices passes `-psn_0_<n>` on the first Finder launch of a quarantined app; routing
    // that to the CLI made the app bounce and quit.
    #[test]
    fn a_macos_process_serial_argument_is_a_bare_launch() {
        assert!(wants_gui(&argv(&["-psn_0_123456"]), true));
        assert!(!wants_gui(&argv(&["-psn_0_123456"]), false));
        assert!(wants_gui(&argv(&["-psn_0_1", "gui"]), false));
        assert!(!wants_gui(&argv(&["-psn_0_1", "info"]), true));
        // Only in argv[1], where LaunchServices puts it.
        assert!(!wants_gui(&argv(&["info", "-psn_0_1"]), true));
    }

    // `freemkv --lang de gui` in the app build must open the window, not reach the CLI's
    // "the app is not part of this build".
    #[test]
    fn a_language_flag_before_the_command_does_not_hide_it() {
        for flag in ["--lang", "--language"] {
            assert!(wants_gui(&argv(&[flag, "de", "gui"]), false), "{flag}");
            assert!(wants_gui(&argv(&[flag, "de"]), true), "{flag}");
            assert!(!wants_gui(&argv(&[flag, "de"]), false), "{flag}");
            assert!(!wants_gui(&argv(&[flag, "de", "info"]), true), "{flag}");
        }
        assert!(wants_gui(
            &argv(&["-psn_0_1", "--lang", "de", "gui"]),
            false
        ));
        // The CLI's value rule: a URL or a flag is never the language, so it stays a command.
        assert!(!wants_gui(&argv(&["--lang", "disc://"]), true));
        assert!(!wants_gui(&argv(&["--lang", "--verbose", "gui"]), true));
        // A value-less flag at the end is skipped alone, as the CLI does.
        assert!(wants_gui(&argv(&["--lang"]), true));
    }

    #[test]
    fn the_launch_language_is_the_cli_flags_value() {
        assert_eq!(
            launch_language(&argv(&["--lang", "de", "gui"])).as_deref(),
            Some("de")
        );
        assert_eq!(
            launch_language(&argv(&["gui", "--language", "fr"])).as_deref(),
            Some("fr")
        );
        // Last one wins, as in `strip_language_flag`.
        assert_eq!(
            launch_language(&argv(&["--lang", "de", "--lang", "fr"])).as_deref(),
            Some("fr")
        );
        assert_eq!(launch_language(&argv(&["gui"])), None);
        assert_eq!(launch_language(&argv(&["--lang", "disc://"])), None);
        assert_eq!(launch_language(&argv(&["--lang"])), None);
    }

    #[test]
    fn a_known_launch_language_overrides_the_saved_one_for_this_run() {
        assert_eq!(chosen_language("English", Some("de")), "de");
        assert_eq!(chosen_language("de", Some("auto")), "auto");
        assert_eq!(chosen_language("de", None), "de");
        // An unknown code would silently mean "Auto"; the saved choice is the better guess.
        assert_eq!(chosen_language("de", Some("xx")), "de");
    }

    #[test]
    fn an_empty_argv_never_panics() {
        // argv[0] is not guaranteed by the OS.
        assert!(wants_gui(&[], true));
        assert!(!wants_gui(&[], false));
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&'static str) -> Option<OsString> + 'a {
        move |k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn a_display_is_present_only_when_a_variable_is_set_and_non_empty() {
        assert!(!display_present(env(&[])));
        assert!(!display_present(env(&[("DISPLAY", "")])));
        assert!(!display_present(env(&[
            ("DISPLAY", ""),
            ("WAYLAND_DISPLAY", "")
        ])));
        assert!(display_present(env(&[("DISPLAY", ":0")])));
        assert!(display_present(env(&[("WAYLAND_DISPLAY", "wayland-0")])));
        assert!(display_present(env(&[
            ("DISPLAY", ""),
            ("WAYLAND_DISPLAY", "wayland-0")
        ])));
        assert!(display_present(env(&[
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "")
        ])));
    }

    #[test]
    fn the_shipped_console_image_starts_its_exe_sibling_first() {
        let dir = Path::new("C:").join("x");
        let me = dir.join("freemkv.com");
        assert_eq!(
            windowed_candidates(&me),
            vec![dir.join("freemkv.exe"), dir.join("freemkv-gui.exe")]
        );
    }

    #[test]
    fn a_cargo_build_dir_image_starts_only_freemkv_gui() {
        let dir = Path::new("target").join("debug");
        let me = dir.join("freemkv.exe");
        assert_eq!(windowed_candidates(&me), vec![dir.join("freemkv-gui.exe")]);
    }

    #[test]
    fn the_console_image_never_launches_itself() {
        for name in [
            "freemkv.com",
            "freemkv.exe",
            "freemkv-gui.exe",
            "freemkv",
            "x.y.z",
        ] {
            let me: PathBuf = Path::new("d").join(name);
            let c = windowed_candidates(&me);
            assert!(!c.contains(&me), "{name}: {c:?}");
        }
        // Already the windowed image: nothing to start, so the shell runs in-process.
        assert!(windowed_candidates(&Path::new("d").join("freemkv-gui.exe")).is_empty());
    }
}
