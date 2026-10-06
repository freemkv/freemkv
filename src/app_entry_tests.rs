use super::{
    GUI_LOG_CAP_BYTES, chosen_language, display_present, gui_source, launch_language,
    resolved_locale, trim_oversized_log, wants_gui, windowed_candidates,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[test]
fn a_gui_launch_names_the_source_it_opens() {
    let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let src = |v: &[&str]| gui_source(&a(v));
    assert_eq!(
        src(&["freemkv", "gui", "/m/Disc.iso"]).as_deref(),
        Some("/m/Disc.iso")
    );
    assert_eq!(
        src(&["freemkv", "--lang", "de", "gui", "iso://D.iso"]).as_deref(),
        Some("iso://D.iso")
    );
    assert_eq!(src(&["freemkv", "gui"]), None);
    assert_eq!(src(&["freemkv", "gui", "--verbose"]), None);
    assert_eq!(src(&["freemkv", "info", "iso://D.iso"]), None);
}

#[test]
fn auto_resolves_to_the_system_locale_and_a_pick_to_its_code() {
    let sys = || Some("ar-SA".to_string());
    assert_eq!(resolved_locale("auto", sys).as_deref(), Some("ar-SA"));
    assert_eq!(resolved_locale("Deutsch", sys).as_deref(), Some("de"));
    assert_eq!(resolved_locale("de", || None).as_deref(), Some("de"));
    assert_eq!(resolved_locale("auto", || None), None);
}

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
