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
/// which case a bare launch prints usage exactly as the CLI build does.
pub fn wants_gui(args: &[String], display: bool) -> bool {
    match args.get(1).map(String::as_str) {
        Some("gui") => true,
        Some(_) => false,
        None => display,
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
    use super::{
        GUI_LOG_CAP_BYTES, display_present, trim_oversized_log, wants_gui, windowed_candidates,
    };
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

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
