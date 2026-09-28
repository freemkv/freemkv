//! The built `freemkv` binary, black-box: what a bare launch and the common flags do in each
//! build. Never bare-launches the app build where a window could open (macOS, Windows).

use std::process::{Command, Output};

fn freemkv() -> Command {
    Command::new(env!("CARGO_BIN_EXE_freemkv"))
}

fn run(args: &[&str]) -> Output {
    freemkv().args(args).output().expect("spawn freemkv")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn version_flag_prints_the_crate_version() {
    let out = run(&["--version"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with(env!("CARGO_PKG_VERSION")),
        "stdout: {stdout}"
    );
}

#[test]
fn help_flags_succeed() {
    for args in [&["--help"][..], &["info", "--help"]] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(!text(&out).trim().is_empty(), "{args:?} printed nothing");
    }
}

#[test]
fn an_argument_that_is_not_a_url_fails() {
    // Any argument is the CLI in both builds, so this never reaches the window.
    let out = run(&["not-a-url"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!text(&out).trim().is_empty());
}

// `freemkv server` is the daemon only in the server build; everywhere else it
// is an ordinary (non-URL) CLI argument and must never start anything.
#[cfg(feature = "server")]
mod server_build {
    use super::*;

    #[test]
    fn server_version_is_the_daemon_label() {
        let out = run(&["server", "--version"]);
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.starts_with(concat!("freemkv server ", env!("CARGO_PKG_VERSION"))),
            "stdout: {stdout}"
        );
    }

    #[test]
    fn an_unknown_server_argument_exits_2() {
        let out = run(&["server", "--no-such-flag"]);
        assert_eq!(out.status.code(), Some(2), "{out:?}");
    }
}

#[cfg(not(feature = "server"))]
#[test]
fn server_is_not_a_command_outside_the_server_build() {
    let out = run(&["server", "--version"]);
    assert!(!out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("freemkv server"));
}

#[cfg(not(feature = "gui"))]
mod cli_build {
    use super::*;

    fn assert_usage(out: &Output) {
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        assert!(!text(out).trim().is_empty(), "bare launch printed nothing");
    }

    #[test]
    fn bare_launch_prints_usage_and_exits_2() {
        assert_usage(&freemkv().output().expect("spawn freemkv"));
    }

    #[test]
    fn bare_launch_ignores_a_display() {
        let out = freemkv()
            .env("DISPLAY", ":0")
            .env("WAYLAND_DISPLAY", "wayland-0")
            .output()
            .expect("spawn freemkv");
        assert_usage(&out);
    }
}

#[cfg(all(feature = "gui", target_os = "linux"))]
#[test]
fn app_bare_launch_without_a_display_prints_usage_and_exits_2() {
    let out = freemkv()
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .expect("spawn freemkv");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(!text(&out).trim().is_empty());
}

#[cfg(all(feature = "gui", windows))]
mod pe_subsystem {
    const CONSOLE: u16 = 3;
    const GUI: u16 = 2;

    // IMAGE_OPTIONAL_HEADER.Subsystem, same offset in PE32 and PE32+.
    fn subsystem(path: &str) -> u16 {
        let b = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        assert_eq!(&b[..2], b"MZ", "{path}: not a PE image");
        let pe = u32::from_le_bytes(b[0x3C..0x40].try_into().unwrap()) as usize;
        assert_eq!(&b[pe..pe + 4], b"PE\0\0", "{path}: bad PE signature");
        let opt = pe + 4 + 20;
        u16::from_le_bytes(b[opt + 68..opt + 70].try_into().unwrap())
    }

    #[test]
    fn the_cli_image_is_console_subsystem() {
        // Shipped as freemkv.com: a console image keeps the shell attached for CLI output.
        assert_eq!(subsystem(env!("CARGO_BIN_EXE_freemkv")), CONSOLE);
    }

    #[test]
    fn the_window_image_is_gui_subsystem() {
        // Shipped as freemkv.exe: a double-click must not flash a console.
        assert_eq!(subsystem(env!("CARGO_BIN_EXE_freemkv-gui")), GUI);
    }
}
