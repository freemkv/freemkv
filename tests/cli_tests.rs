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

/// A flag the parser ACTS on must be findable in `--help`.
///
/// `--force` shipped as the only way to write into a non-empty `dir://`
/// target — the rejection tells the user to "pass --force", and `--help`
/// listed every other flag and not that one, so the flag was discoverable
/// only from the error it exists to clear.
///
/// The list is written out rather than read back from the parser: a scraper
/// over `parse_flags` would agree with the code under test by construction.
/// Adding a flag to the parser and not to this list is the thing that must
/// fail here.
#[test]
fn every_flag_the_rip_parser_accepts_is_named_in_the_help() {
    // NOTE ON SCOPE: despite this list's name, `--share` and `--mask` below
    // are NOT rip-parser flags — they belong to the `info` subcommand
    // (`info.rs`), and `pipe::parse_flags` rejects them. They are listed here
    // because the assertion is about `usage()` naming every flag the TOOL
    // accepts, which is the property that matters to a user reading `--help`.
    // Calling them rip-parser flags is what made this list look like it had
    // drifted from `parse_flags` when it had not.
    const FLAGS: &[&str] = &[
        "--title",
        "--audio",
        "--subtitles",
        "--keydb",
        "--key-url",
        "--key-auth",
        "--log-level",
        "--log-file",
        "--quiet",
        "--raw",
        "--multipass",
        "--force",
        "--share",
        "--mask",
    ];
    let out = freemkv().arg("--help").output().expect("failed to run");
    assert!(out.status.success());
    let help = combined_output(&out);
    for f in FLAGS {
        assert!(
            help.contains(f),
            "`{f}` is accepted by the parser but named nowhere in --help"
        );
    }
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

#[test]
fn dropped_device_flag_is_rejected_end_to_end() {
    // The sibling of the `-k` case: `--device` was removed too (the device is
    // carried by `disc:///dev/sgN`), and it also took a value. Its value must
    // be stepped over so the invocation still resolves to source+dest and the
    // user is told WHICH flag is gone — not handed the bare usage hint because
    // "/dev/sg0" counted as a third URL.
    let out = freemkv()
        .args(["--device", "/dev/sg99", "disc://", "mkv://out.mkv"])
        .output()
        .expect("failed to run");
    assert!(!out.status.success());
    let combined = combined_output(&out);
    assert!(
        combined.contains("unknown flag") && combined.contains("--device"),
        "expected unknown-flag error naming --device, got: {combined}"
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

// ── The destination must never BE the source ────────────────────────────────
//
// `freemkv mkv://Movie.mkv mkv://Movie.mkv` used to destroy the user's only
// copy: the sink chain `mux_stream` → `drive_mux` → `output()` →
// `WritebackFile::create_with_size_hint` → `File::create` truncates the
// still-open input, so the run wrote back a partial re-mux over the original.
// Measured against the pre-fix binary on real media: a 7.9 MB MKV came back
// 4.2 MB (and the CLI reported the file as "malformed or truncated"), a 50 MB
// M2TS came back 21 MB while reporting "Complete", and an MP4 was replaced
// outright by 12 MB of preallocated zeroes.
//
// The refusal has to be a preflight refusal — before ANY sink is opened — so
// these run against files that are not real media: nothing should get far
// enough to care.

/// One scratch directory per test, removed on drop.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!(
            "freemkv_cli_dest_is_source_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch dir");
        Scratch(d)
    }
    fn file(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let p = self.0.join(name);
        std::fs::write(&p, bytes).expect("write fixture");
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn ripping_a_file_onto_itself_is_refused_end_to_end() {
    const BODY: &[u8] = b"the user's only copy of the movie -- do not truncate me";
    let s = Scratch::new("self");
    for (scheme, name) in [
        ("mkv", "Movie.mkv"),
        ("m2ts", "Movie.m2ts"),
        ("mp4", "Movie.mp4"),
        ("iso", "Disc.iso"),
    ] {
        let p = s.file(name, BODY);
        let url = format!("{scheme}://{}", p.display());
        let out = freemkv()
            .args([&url, &url])
            .output()
            .expect("failed to run");
        let combined = combined_output(&out);
        assert!(
            !out.status.success(),
            "{scheme}://X {scheme}://X exited 0: {combined}"
        );
        assert!(
            combined.contains("overwrite the source"),
            "expected the dest-is-source refusal for {scheme}://, got: {combined}"
        );
        // Contents, not existence — a truncated file still exists.
        assert_eq!(
            std::fs::read(&p).expect("the source must still be there"),
            BODY,
            "{scheme}://X {scheme}://X destroyed the source"
        );
    }
}

#[test]
fn ripping_a_file_onto_a_second_spelling_of_itself_is_refused_end_to_end() {
    // `./Movie.mkv` vs `Movie.mkv`: one file, two strings. A string compare
    // accepts this and the rip eats the source.
    const BODY: &[u8] = b"one file, two spellings";
    let s = Scratch::new("spelling");
    let p = s.file("Movie.mkv", BODY);
    std::fs::create_dir_all(s.0.join("sub")).expect("subdir");
    let detour = s.0.join("sub").join("..").join("Movie.mkv");
    let out = freemkv()
        .args([
            format!("mkv://{}", p.display()),
            format!("mkv://{}", detour.display()),
        ])
        .output()
        .expect("failed to run");
    let combined = combined_output(&out);
    assert!(!out.status.success(), "accepted a detoured self-dest");
    assert!(
        combined.contains("overwrite the source"),
        "expected the dest-is-source refusal, got: {combined}"
    );
    assert_eq!(std::fs::read(&p).expect("source survives"), BODY);
}

#[test]
fn two_different_files_are_still_rippable_end_to_end() {
    // The guard must not refuse the ordinary invocation. This source is not
    // real media, so the rip fails LATER — on the demux, with its own error.
    // What matters is that it is not the same-file refusal.
    let s = Scratch::new("distinct");
    let a = s.file("a.mkv", b"not real media, but not the destination either");
    let b = s.0.join("b.mkv");
    let out = freemkv()
        .args([
            format!("mkv://{}", a.display()),
            format!("mkv://{}", b.display()),
        ])
        .output()
        .expect("failed to run");
    let combined = combined_output(&out);
    assert!(
        !combined.contains("overwrite the source"),
        "two different files were refused as the same file: {combined}"
    );
}

// ── `info dir://` — an extracted disc FOLDER ────────────────────────────────
//
// `cli_entry`'s info dispatch merges `StreamUrl::Dir` with `StreamUrl::Iso` so
// a folder enumerates exactly like an image, and picks `scan_dir` vs `scan_iso`
// from which of the two it got. Before the merge a folder fell through to the
// catch-all and reported "Cannot get info", while RIPPING from the same folder
// worked. The merge shipped with nothing covering it — no unit test, no
// integration case, no golden — so deleting the `Dir` half, or collapsing the
// scan selector onto `scan_iso`, left the suite green.

/// A folder that looks like a disc backup: `scan_dir` synthesizes a UDF volume
/// over it, `scan_iso` cannot read it at all. That asymmetry is what makes the
/// selector observable from outside the process.
fn bdmv_folder(s: &Scratch, name: &str) -> std::path::PathBuf {
    let root = s.0.join(name);
    std::fs::create_dir_all(root.join("BDMV").join("PLAYLIST")).expect("BDMV skeleton");
    std::fs::create_dir_all(root.join("BDMV").join("STREAM")).expect("BDMV skeleton");
    root
}

#[test]
fn info_on_a_disc_folder_enumerates_it_like_an_image() {
    let s = Scratch::new("info_dir");
    let root = bdmv_folder(&s, "backup");
    let out = freemkv()
        .args(["info", &format!("dir://{}", root.display())])
        .output()
        .expect("failed to run");
    let combined = combined_output(&out);
    // Exit 0: the folder was scanned. An empty BDMV has no titles, which is a
    // successful listing of zero titles — not a failure to read the folder.
    assert!(
        out.status.success(),
        "info dir:// on a disc folder must succeed, got {:?}: {combined}",
        out.status.code()
    );
    // The title-listing header, i.e. `disc_info::print_disc_titles` ran.
    assert!(
        combined.contains("No titles found"),
        "expected the title listing, got: {combined}"
    );
    // NOT the catch-all this route used to fall through to.
    assert!(
        !combined.contains("Cannot get info"),
        "dir:// fell through to the unsupported-URL arm: {combined}"
    );
}

#[test]
fn info_on_a_disc_folder_uses_scan_dir_not_scan_iso() {
    // The same folder through `iso://` MUST fail: `scan_iso` opens it as a file
    // and cannot. If the `Dir | Iso` arm ever picked one scan for both, the
    // `dir://` case above would fail with exactly this error instead.
    let s = Scratch::new("info_dir_iso");
    let root = bdmv_folder(&s, "backup");
    let out = freemkv()
        .args(["info", &format!("iso://{}", root.display())])
        .output()
        .expect("failed to run");
    assert!(
        !out.status.success(),
        "iso:// on a directory must fail — otherwise the dir:// case above \
         proves nothing about which scan ran"
    );
}

#[test]
fn info_on_a_folder_that_is_not_a_backup_says_so() {
    // An ordinary folder is a clean, specific refusal from `scan_dir` — not the
    // catch-all, and not a panic.
    let s = Scratch::new("info_dir_plain");
    let root = s.0.join("just-a-folder");
    std::fs::create_dir_all(&root).expect("plain dir");
    let out = freemkv()
        .args(["info", &format!("dir://{}", root.display())])
        .output()
        .expect("failed to run");
    let combined = combined_output(&out);
    assert!(!out.status.success(), "got: {combined}");
    assert!(
        combined.contains("not a disc backup"),
        "expected scan_dir's own diagnosis, got: {combined}"
    );
    assert!(
        !combined.contains("Cannot get info"),
        "dir:// fell through to the unsupported-URL arm: {combined}"
    );
}

#[test]
fn info_on_a_disc_folder_rejects_an_unknown_flag() {
    // The route runs the SAME `parse_info_flags` the `disc://` route does, so a
    // typo'd flag exits 1 here too rather than silently producing output that
    // dropped what the user asked for.
    let s = Scratch::new("info_dir_flag");
    let root = bdmv_folder(&s, "backup");
    let out = freemkv()
        .args(["info", &format!("dir://{}", root.display()), "--bogus"])
        .output()
        .expect("failed to run");
    let combined = combined_output(&out);
    assert!(!out.status.success(), "got: {combined}");
    assert!(
        combined.contains("--bogus"),
        "the rejection must name the flag, got: {combined}"
    );
}
