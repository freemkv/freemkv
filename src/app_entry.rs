//! The launch decision and the two steps every desktop launch performs first.
//!
//! This module is deliberately **portable and Win32/AppKit-free**: it deals in
//! `&str` and closures, never in a platform handle. That is what lets both
//! shells share it *and* lets the routing be unit-tested on any machine —
//! `wants_gui` is a pure function of the argument vector, which is the only
//! part of the launch path that can be wrong without a window to look at.
//!
//! It also keeps the `settings`/`engine` "must build anywhere" rule intact: no
//! type from a platform shell crosses this boundary. Callers pass the two
//! settings *values* (`language`, `log_level`) rather than a `Settings`, so the
//! binary and the lib never have to agree on a struct identity.

/// True when this invocation should open the desktop UI rather than the CLI.
///
/// Two ways in, and only two:
///
/// * an explicit `freemkv gui` (works on every desktop platform), or
/// * a bare launch of a *windowed* image — a macOS `.app` double-click, which
///   passes no arguments. `windowed` is the caller's answer to "was I started
///   as a window?"; on Windows the answer for `freemkv.exe` is always `false`,
///   because the windowed image there is the separate `freemkv-gui.exe`, which
///   does not route through here at all.
///
/// A bare launch from a terminal therefore falls through to the CLI, so
/// `freemkv` alone still prints usage and exits 2 — the CLI contract, intact.
pub fn wants_gui(args: &[String], windowed: bool) -> bool {
    if args.get(1).map(String::as_str) == Some("gui") {
        return true;
    }
    args.len() < 2 && windowed
}

/// Apply the saved interface language before the shell builds anything, so the
/// first string lookup resolves in the right locale. (A later change in
/// Settings switches live via `strings::set_locale`.)
///
/// `system_locale` is the platform's "what language is this PC in?" call,
/// passed in rather than `cfg`-selected here: a Finder-launched `.app` and a
/// double-clicked `.exe` both inherit no `LANG`, so the i18n crate's env
/// detection would fall back to English for the "Auto" setting.
pub fn apply_locale(language: &str, system_locale: impl FnOnce() -> Option<String>) {
    let code = crate::ui::locale_code(language);
    if code == "auto" {
        if let Some(sys) = system_locale() {
            crate::strings::set_locale(&sys);
        }
    } else {
        crate::strings::set_language(code);
    }
}

/// The GUI's diagnostic log, in the app-support dir.
const GUI_LOG_NAME: &str = "log.txt";

/// How large that log may already be at startup before it is started over.
///
/// `rolling::never` appends and never rotates, so without this the file is
/// bounded by nothing but how long the user leaves Verbose/Debug on — every
/// rip of every session, forever, in a directory they never look in.
const GUI_LOG_CAP_BYTES: u64 = 8 * 1024 * 1024;

/// Start a fresh diagnostic log when the existing one has already grown past
/// `cap`, instead of appending to it for another session.
///
/// This bounds growth ACROSS sessions, which is the unbounded part: a single
/// session is still limited only by its own length, and `tracing-appender`
/// offers no size-triggered rotation to fix that without hand-rolling a
/// `Write` wrapper. The log is a debugging aid the user opted into and can
/// delete, so the cheap bound is the proportionate one.
///
/// Best-effort by design: a log that cannot be removed must not stop the app
/// from starting, or from logging.
fn trim_oversized_log(path: &std::path::Path, cap: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > cap) {
        let _ = std::fs::remove_file(path);
    }
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
    let file_appender = tracing_appender::rolling::never(&dir, GUI_LOG_NAME);
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
    use super::{GUI_LOG_CAP_BYTES, trim_oversized_log, wants_gui};

    /// The GUI's diagnostic log must not grow without end.
    ///
    /// `rolling::never` appends and never rotates, and nothing else truncated
    /// the file, so leaving Verbose/Debug on accumulated every line of every
    /// rip of every session in the app-support dir indefinitely.
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
    fn bare_launch_from_a_terminal_is_the_cli() {
        // The CLI contract: `freemkv` alone prints usage and exits 2.
        assert!(!wants_gui(&argv(&[]), false));
    }

    #[test]
    fn bare_launch_of_a_windowed_image_is_the_gui() {
        // A macOS `.app` double-click passes no arguments.
        assert!(wants_gui(&argv(&[]), true));
    }

    #[test]
    fn explicit_gui_subcommand_opens_the_gui_either_way() {
        assert!(wants_gui(&argv(&["gui"]), false));
        assert!(wants_gui(&argv(&["gui"]), true));
    }

    #[test]
    fn every_other_invocation_stays_cli() {
        for a in [
            "--version",
            "--help",
            "-h",
            "info",
            "version",
            "update-keys",
            "/dev/disk2",
            "GUI",   // case-sensitive: not the subcommand
            "gui.x", // no prefix matching
        ] {
            assert!(
                !wants_gui(&argv(&[a]), false),
                "{a} should route to the CLI"
            );
            // …and not even from a windowed launch: arguments always mean CLI
            // unless the first one is exactly `gui`.
            assert!(!wants_gui(&argv(&[a]), true), "{a} should route to the CLI");
        }
    }

    #[test]
    fn gui_must_be_the_first_argument() {
        // `freemkv info gui` is an `info` invocation, not a GUI launch.
        assert!(!wants_gui(&argv(&["info", "gui"]), false));
        assert!(!wants_gui(&argv(&["--version", "gui"]), true));
    }

    #[test]
    fn gui_with_trailing_arguments_still_opens_the_gui() {
        // The window ignores extra arguments; it must not fall through to the
        // CLI and print usage.
        assert!(wants_gui(&argv(&["gui", "--verbose"]), false));
    }

    #[test]
    fn an_empty_argv_never_panics() {
        // argv[0] is not guaranteed by the OS.
        assert!(!wants_gui(&[], false));
        assert!(wants_gui(&[], true));
    }
}
