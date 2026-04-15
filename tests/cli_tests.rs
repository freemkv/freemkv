//! CLI integration tests — run the freemkv binary and check behavior.
//!
//! These tests don't require hardware or disc images. They test error handling,
//! argument parsing, and output formatting.

use std::process::Command;

fn freemkv() -> Command {
    Command::new(env!("CARGO_BIN_EXE_freemkv"))
}

// ── No arguments ────────────────────────────────────────────────────────────

#[test]
fn no_args_shows_usage() {
    let out = freemkv().output().expect("failed to run");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("freemkv"));
}

#[test]
fn help_shows_usage() {
    let out = freemkv().arg("help").output().expect("failed to run");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("Examples:"));
}

#[test]
fn version_shows_version() {
    let out = freemkv().arg("--version").output().expect("failed to run");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.trim().chars().next().unwrap().is_ascii_digit());
}

// ── Error handling ──────────────────────────────────────────────────────────

#[test]
fn no_scheme_url_errors() {
    let out = freemkv()
        .args(["/dev/sr0", "output.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("not a valid stream URL"));
}

#[test]
fn bad_scheme_errors() {
    let out = freemkv()
        .args(["foo://bar", "mkv://out.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("not a valid stream URL"));
}

#[test]
fn missing_iso_errors() {
    let out = freemkv()
        .args(["iso:///nonexistent_test_file.iso", "mkv://out.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("No such file"));
}

#[test]
fn nonexistent_drive_errors() {
    let out = freemkv()
        .args(["disc:///dev/sg99", "mkv://out.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
}

#[test]
fn null_input_errors() {
    let out = freemkv()
        .args(["null://", "mkv://out.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(combined.contains("write-only"));
}

// ── Quiet mode ──────────────────────────────────────────────────────────────

#[test]
fn quiet_mode_suppresses_output() {
    // Even with an error, quiet mode should suppress the version banner
    let out = freemkv()
        .args(["iso:///nonexistent.iso", "mkv://out.mkv", "-q"])
        .output()
        .expect("failed to run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("freemkv"));
}
