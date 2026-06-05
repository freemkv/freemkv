//! CLI integration tests — run the freemkv binary and check behavior.
//!
//! These tests don't require hardware or disc images. They test error handling,
//! argument parsing, and output formatting.

use std::process::Command;

fn freemkv() -> Command {
    Command::new(env!("CARGO_BIN_EXE_freemkv"))
}

fn combined_output(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

// ── No arguments ────────────────────────────────────────────────────────────

#[test]
fn no_args_shows_usage() {
    let out = freemkv().output().expect("failed to run");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("freemkv"));
}

#[test]
fn help_shows_usage() {
    let out = freemkv().arg("help").output().expect("failed to run");
    assert!(out.status.success());
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
    let combined = combined_output(&out);
    assert!(
        combined.contains("E9002"),
        "expected E9002, got: {combined}"
    );
}

#[test]
fn bad_scheme_errors() {
    let out = freemkv()
        .args(["foo://bar", "mkv://out.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("E9002"),
        "expected E9002, got: {combined}"
    );
}

#[test]
fn missing_iso_errors() {
    let out = freemkv()
        .args(["iso:///nonexistent_test_file.iso", "mkv://out.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
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
    let combined = combined_output(&out);
    assert!(
        combined.contains("E9001"),
        "expected E9001, got: {combined}"
    );
}

// ── --raw + mux rejection ───────────────────────────────────────────────────

#[test]
fn raw_into_mkv_is_rejected() {
    // --raw passes encrypted bytes through; muxing ciphertext is nonsense.
    // The CLI must reject this before doing any work — no disc/ISO needed.
    let out = freemkv()
        .args(["disc:///dev/sg99", "mkv://out.mkv", "--raw"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("--raw cannot be used for muxing"),
        "expected raw-mux rejection, got: {combined}"
    );
}

#[test]
fn raw_into_m2ts_is_rejected() {
    let out = freemkv()
        .args(["disc:///dev/sg99", "m2ts://out.m2ts", "--raw"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("--raw cannot be used for muxing"),
        "expected raw-mux rejection, got: {combined}"
    );
}

// ── Quiet mode ──────────────────────────────────────────────────────────────

#[test]
fn quiet_mode_suppresses_output() {
    let out = freemkv()
        .args(["iso:///nonexistent.iso", "mkv://out.mkv", "-q"])
        .output()
        .expect("failed to run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("freemkv"));
}
