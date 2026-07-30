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
    let file_appender = tracing_appender::rolling::never(&dir, "log.txt");
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
    use super::wants_gui;

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
