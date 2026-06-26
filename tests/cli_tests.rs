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
    // Bare invocation prints usage but exits non-zero (2) so a scripted
    // `freemkv; echo $?` sees a failure rather than a false success. Explicit
    // `help`/`--help` is the success path (see `help_shows_usage`).
    let out = freemkv().output().expect("failed to run");
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
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
    // A schemeless destination is caught up front with clear guidance to add a
    // scheme — never silently turned into a `.unknown` file / `unknown://` URL.
    let out = freemkv()
        .args(["/dev/sr0", "output.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("no URL scheme"),
        "expected schemeless-dest guidance, got: {combined}"
    );
}

#[test]
fn schemeless_dest_with_valid_source_errors() {
    // A valid scheme source but schemeless dest must error clearly, not produce
    // a `name_t1.unknown` file or an `unknown://` URL.
    let out = freemkv()
        .args(["iso:///nonexistent.iso", "/path/out.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("no URL scheme"),
        "expected schemeless-dest guidance, got: {combined}"
    );
    assert!(
        !combined.contains("unknown://") && !combined.contains(".unknown"),
        "must not emit unknown scheme/extension, got: {combined}"
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
    // An unrecognized SOURCE scheme (`foo://`) is now caught up front by
    // `preflight_validate` (fail loud and early) with a clear English message
    // that names the offending URL and guides toward a real scheme. No raw error
    // code may reach the user.
    assert!(
        combined.contains("not a usable source URL") && combined.contains("foo://bar"),
        "expected the source-scheme guidance message, got: {combined}"
    );
    assert!(
        !combined.contains("E9002") && !combined.contains("E90"),
        "raw error code must not leak to the user, got: {combined}"
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
    // `fmt_err` renders E9001 (StreamWriteOnly) to its English locale string.
    // WS2: the line is now code-forward — the `E9001` token is SHOWN as a
    // prefix ahead of the localized message (`Error: E9001 Stream is
    // write-only.`), not stripped.
    assert!(
        combined.contains("Stream is write-only"),
        "expected the English E9001 message, got: {combined}"
    );
    assert!(
        combined.contains("E9001"),
        "expected the code-forward E9001 token, got: {combined}"
    );
    assert!(
        combined.contains("Error: E9001"),
        "expected the WS2 level+code render, got: {combined}"
    );
}

// ── --raw + mux rejection ───────────────────────────────────────────────────

#[test]
fn raw_into_mkv_is_rejected() {
    // --raw is iso://-output-only (it writes a raw, still-encrypted disc image).
    // A non-ISO mux destination + --raw is rejected up front before any work —
    // no disc/ISO needed. The message names the flag and points at iso://.
    let out = freemkv()
        .args(["disc:///dev/sg99", "mkv://out.mkv", "--raw"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("--raw") && combined.contains("iso://"),
        "expected raw-iso-only rejection naming the flag + iso://, got: {combined}"
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
        combined.contains("--raw") && combined.contains("iso://"),
        "expected raw-iso-only rejection naming the flag + iso://, got: {combined}"
    );
}

#[test]
fn multipass_into_mkv_is_rejected() {
    // --multipass is iso://-output-only too (multi-pass recovery writes a disc
    // image with a mapfile). A non-ISO destination + --multipass is a hard,
    // early error — replacing the old silent warn-and-ignore.
    let out = freemkv()
        .args(["disc:///dev/sg99", "mkv://out.mkv", "--multipass"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("--multipass") && combined.contains("iso://"),
        "expected multipass-iso-only rejection, got: {combined}"
    );
}

// ── WS3: dropped `-k` short flag (rc.6 — `--keydb` long form only) ──────────

#[test]
fn dropped_short_k_flag_is_rejected_end_to_end() {
    // `-k` was removed; through the real CLI dispatch it must surface the
    // unknown-flag error (naming `-k`) and exit non-zero — never silently
    // consume `keydb.cfg` as a keydb path and proceed against the default.
    let out = freemkv()
        .args(["disc:///dev/sg99", "mkv://out.mkv", "-k", "keydb.cfg"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("unknown flag") && combined.contains("-k"),
        "expected unknown-flag error naming -k, got: {combined}"
    );
    // No raw error code leaks (the message is a CLI validation string).
    assert!(
        !combined.contains("E70"),
        "no raw code expected: {combined}"
    );
}

// ── WS3: dir:// routing pre-flight (byte-stream source rejected) ────────────

#[test]
fn dir_dest_byte_stream_source_rejected_end_to_end() {
    // A byte-stream source (mkv://) into a dir:// target has no UDF file tree —
    // the CLI must reject it up front with the localized guidance, exit non-zero,
    // and never create the target folder.
    let target = std::env::temp_dir().join(format!("freemkv_ws3_dir_{}", std::process::id()));
    let dest = format!("dir://{}/", target.display());
    let out = freemkv()
        .args(["mkv://in.mkv", &dest])
        .output()
        .expect("failed to run");
    let target_made = target.exists();
    let _ = std::fs::remove_dir_all(&target);
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("dir://") && combined.contains("mkv://in.mkv"),
        "expected dir:// source guidance naming the bad source, got: {combined}"
    );
    assert!(
        !target_made,
        "a rejected dir:// source must not create the target folder"
    );
}

#[test]
fn dir_dest_existing_file_rejected_end_to_end() {
    // A dir:// target that is an existing regular FILE must be rejected (you
    // can't extract a tree into a file). Use the auto-detect `disc://` source so
    // the (cheap, side-effect-free) dest-file check is reached: an explicit
    // `disc:///dev/sgN` would trip the earlier device-reachability gate first.
    let f = std::env::temp_dir().join(format!("freemkv_ws3_isfile_{}", std::process::id()));
    std::fs::write(&f, b"i am a file").unwrap();
    let dest = format!("dir://{}", f.display());
    let out = freemkv()
        .args(["disc://", &dest])
        .output()
        .expect("failed to run");
    let _ = std::fs::remove_file(&f);
    assert!(!out.status.success());
    let combined = combined_output(&out);
    // Match a substring unique to the dir_dest_is_file message (en.json) rather
    // than the broad "file"/"folder" check, so a wrong-reason failure (e.g. a
    // reordered check or a different error string) can't silently pass.
    assert!(
        combined.contains("not a folder"),
        "expected dir:// dest-is-file guidance, got: {combined}"
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
