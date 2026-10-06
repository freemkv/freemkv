use super::{fe, summarize_extract, summarize_image_decrypt, summarize_outcome, summarize_stream};
use std::io::ErrorKind;

/// A hard failure is never a success, even when earlier titles wrote.
/// This is the silent-success regression: `written > 0` used to take the
/// "N title(s) written" arm and lose the failure entirely.
#[test]
fn failure_after_good_titles_is_an_error_that_keeps_the_count() {
    let out = fe::RipOutcome::Failed {
        title_index: 2,
        code: None,
        kind: ErrorKind::StorageFull,
        data: String::new(),
    };
    let r = summarize_outcome(&out, 2, 0, 3, "/out");
    let msg = r.expect_err("a rip stopped by a full disk must not report success");
    assert!(msg.contains("2 of 3 title(s) written"), "{msg}");
    assert!(msg.contains("title 3 failed"), "{msg}");
    assert!(msg.contains("full"), "{msg}");
}

/// The `kind` half of the cause. A passthrough OS error carries no
/// `E<code>`, so code-only rendering produced "Mux failed (E0)." — the
/// reason `RipOutcome::Failed` carries `kind` at all.
#[test]
fn passthrough_os_errors_are_explained_from_kind_not_code() {
    let kinds = [
        (ErrorKind::StorageFull, "full"),
        (ErrorKind::PermissionDenied, "permission"),
        (ErrorKind::NotFound, "no longer there"),
    ];
    for (kind, want) in kinds {
        let out = fe::RipOutcome::Failed {
            title_index: 0,
            code: None,
            kind,
            data: String::new(),
        };
        let msg = summarize_outcome(&out, 0, 0, 1, "/out").unwrap_err();
        assert!(
            msg.to_lowercase().contains(want),
            "{kind:?} rendered as {msg:?}, wanted {want:?}"
        );
        assert!(!msg.contains("E0"), "{kind:?} fell back to a code: {msg}");
    }
}

/// The `code` half: a typed library failure still routes through `explain`.
#[test]
fn typed_failures_still_explain_the_code() {
    let out = fe::RipOutcome::Failed {
        title_index: 0,
        code: Some(9048),
        kind: ErrorKind::Other,
        data: String::new(),
    };
    let msg = summarize_outcome(&out, 0, 0, 1, "/out").unwrap_err();
    assert!(msg.contains("MP4"), "{msg}");
}

/// A disc-level key failure is a failed rip, not "Nothing was written".
#[test]
fn no_key_is_an_error() {
    let msg = summarize_outcome(&fe::RipOutcome::NoKey, 0, 0, 3, "/out")
        .expect_err("an undecryptable disc must not report success");
    assert!(msg.contains("no decryption key"), "{msg}");
}

/// The successes and the cancel path keep their existing wording.
#[test]
fn success_and_cancel_are_unchanged() {
    let ok = fe::RipOutcome::Ok { titles_written: 2 };
    assert_eq!(
        summarize_outcome(&ok, 2, 0, 2, "/out").unwrap(),
        "2 title(s) written to /out"
    );
    assert_eq!(
        summarize_outcome(&ok, 0, 0, 2, "/out").unwrap(),
        "Nothing was written"
    );
    // Never "Nothing was written" while a partial file sits in the folder.
    let cancelled = summarize_outcome(&ok, 0, 1, 2, "/out").unwrap();
    assert!(cancelled.starts_with("Cancelled"), "{cancelled}");
    assert!(cancelled.contains("1 partial file(s) kept"), "{cancelled}");

    // A title cancelled after earlier ones wrote still names its partial file.
    let last_cancelled = summarize_outcome(&ok, 1, 1, 2, "/out").unwrap();
    assert!(
        last_cancelled.starts_with("1 title(s) written"),
        "{last_cancelled}"
    );
    assert!(
        last_cancelled.contains("1 partial file(s) kept in /out"),
        "{last_cancelled}"
    );

    let halted = summarize_outcome(&fe::RipOutcome::Halted, 1, 0, 3, "/out").unwrap();
    assert_eq!(halted, "Cancelled — 1 of 3 title(s) completed");
}

/// A halt that left a partial file keeps both facts.
#[test]
fn halt_with_partial_keeps_both() {
    let msg = summarize_outcome(&fe::RipOutcome::Halted, 1, 1, 3, "/out").unwrap();
    assert!(msg.contains("1 of 3 title(s) completed"), "{msg}");
    assert!(msg.contains("1 partial file(s) kept in /out"), "{msg}");
}

// ── round-2 concurrency audit: cancel vs. a completed worker ────────────
// `run_stream` used to discard `MuxOutcome` via a bare `?`, so a mid-
// conversion Cancel still reported "Written to <dir>" for a truncated file.

#[test]
fn a_completed_stream_conversion_reports_written() {
    assert_eq!(
        summarize_stream(&clean_outcome(), "/out/movie.mkv", "/out"),
        "Written to /out"
    );
}

// ── A COMPLETED export can still be lossy ───────────────────────────────
// The GUI used to grade on `completed` alone, so a missing audio track
// read as "Finished". These outcomes state the real shape: completed AND lossy.

/// The exact outcome the mp4 sink produces when it has to drop a track.
fn lossy_outcome() -> libfreemkv::MuxOutcome {
    libfreemkv::MuxOutcome {
        halted: false,
        completed: true,
        output_opened: true,
        bytes_written: 4 << 30,
        errors: 0,
        lost_bytes: 0,
        streams: 3,
        undelivered_streams: vec![1],
    }
}

/// The same run with nothing lost — the control that keeps the reporting
/// from being unconditional.
fn clean_outcome() -> libfreemkv::MuxOutcome {
    libfreemkv::MuxOutcome {
        undelivered_streams: Vec::new(),
        ..lossy_outcome()
    }
}

#[test]
fn a_completed_export_that_dropped_a_track_is_not_reported_as_a_clean_write() {
    let o = lossy_outcome();
    assert!(o.completed, "the whole point: this outcome COMPLETED");
    let msg = summarize_stream(&o, "/out/movie.mp4", "/out");
    assert_ne!(
        msg, "Written to /out",
        "a lossy export must not render identically to a complete one"
    );
    assert!(
        msg.contains("/out"),
        "the destination is still worth naming: {msg}"
    );
}

#[test]
fn the_dropped_tracks_are_named_one_per_line_and_one_based() {
    let o = lossy_outcome();
    let lines = super::lossy_lines(&o, "/out/movie.mp4");
    assert_eq!(
        lines.len(),
        2,
        "one header plus one line per dropped track: {lines:?}"
    );
    assert!(
        lines[1].ends_with(" 2"),
        "stream index 1 is track 2 — 1-based, as the CLI and `info` list \
             them: {:?}",
        lines[1]
    );
}

// A mux that dropped PAYLOAD BYTES is lossy too, and `completed` is true.
// lost_bytes/errors count bytes read but not carried — a 3D re-mux drops
// the dependent-view payload with undelivered_streams still empty.
fn byte_lossy_outcome() -> libfreemkv::MuxOutcome {
    libfreemkv::MuxOutcome {
        undelivered_streams: Vec::new(),
        errors: 2,
        lost_bytes: 3 << 20,
        ..lossy_outcome()
    }
}

#[test]
fn a_completed_export_that_dropped_payload_bytes_is_not_a_clean_write() {
    let o = byte_lossy_outcome();
    assert!(o.completed, "the whole point: this outcome COMPLETED");
    assert!(
        o.undelivered_streams.is_empty(),
        "and lost no whole stream — the loss is inside the tracks"
    );
    let msg = summarize_stream(&o, "/out/movie.mkv", "/out");
    assert_ne!(
        msg, "Written to /out",
        "3 MB of dropped payload must not render as a clean write"
    );
    let lines = super::lossy_lines(&o, "/out/movie.mkv");
    assert!(
        !lines.is_empty(),
        "the run that reports the write must report the loss"
    );
    assert!(
        lines.iter().any(|l| l.contains('3')),
        "the size of the loss is the fact the user needs: {lines:?}"
    );
}

#[test]
fn a_clean_export_says_nothing_about_undelivered_tracks() {
    let o = clean_outcome();
    assert!(super::lossy_lines(&o, "/out/movie.mkv").is_empty());
    assert_eq!(
        summarize_stream(&o, "/out/movie.mkv", "/out"),
        "Written to /out",
        "an unconditional warning would be worse than none"
    );
}

#[test]
fn a_stream_conversion_halted_by_cancel_says_so_not_written() {
    let cancelled = libfreemkv::MuxOutcome {
        halted: true,
        completed: false,
        ..clean_outcome()
    };
    let msg = summarize_stream(&cancelled, "/out/movie.mkv", "/out");
    assert!(
        msg.starts_with("Cancelled"),
        "a Cancel mid-conversion must not read as a clean write: {msg}"
    );
    assert!(
        !msg.contains("Written to"),
        "the truncated file must never be reported as a completed write: {msg}"
    );
    assert!(msg.contains("/out/movie.mkv"), "{msg}");
}

// `run_extract_folder` had the identical gap for `res.halted` — the exact
// signal `pipe.rs`'s `extract_succeeded` (`!halted && complete`) already
// gates the CLI's exit code on for this same `libfreemkv::ExtractResult`.

fn extract_result(halted: bool, files: usize, bytes_unreadable: u64) -> libfreemkv::ExtractResult {
    libfreemkv::ExtractResult {
        files: vec![
            libfreemkv::FileResult {
                path: "f".into(),
                bytes_good: 0,
                bytes_unreadable: 0,
                complete: true,
            };
            files
        ],
        bytes_good: 0,
        bytes_unreadable,
        complete: !halted && bytes_unreadable == 0,
        halted,
    }
}

// Covers the image-decrypt summariser's three branches, matching its two
// siblings. `complete` is derived exactly as `CopyResult::new` derives it
// (pending included) so a fixture can't fake a combo the engine can't emit.
fn copy_result(halted: bool, good: u64, unreadable: u64) -> fe::CopyResult {
    copy_result_pending(halted, good, unreadable, 0)
}

fn copy_result_pending(halted: bool, good: u64, unreadable: u64, pending: u64) -> fe::CopyResult {
    fe::CopyResult {
        bytes_total: good + unreadable + pending,
        bytes_good: good,
        bytes_unreadable: unreadable,
        bytes_pending: pending,
        recovered_this_pass: good,
        complete: !halted && unreadable == 0 && pending == 0,
        halted,
    }
}

#[test]
fn a_completed_image_decrypt_reports_written() {
    let msg = summarize_image_decrypt(
        &copy_result(false, 2 * 1_073_741_824, 0),
        std::path::Path::new("/out/Disc.iso"),
    );
    assert_eq!(msg, "Decrypted image written: /out/Disc.iso (2.00 GiB)");
}

#[test]
fn an_image_decrypt_halted_by_cancel_says_so_not_written() {
    // The defect this summariser exists to close: the arm reported
    // "Decrypted image written" for any non-zero byte count, so a Cancel
    // partway through read as a clean result.
    let msg = summarize_image_decrypt(
        &copy_result(true, 1_073_741_824, 0),
        std::path::Path::new("/out/Disc.iso"),
    );
    assert!(
        msg.starts_with("Cancelled"),
        "a cancelled decrypt must not read as a clean write: {msg}"
    );
    assert!(
        msg.contains("kept"),
        "the partial image is kept, and the user must be told: {msg}"
    );
}

#[test]
fn a_lossy_image_decrypt_reports_the_unreadable_bytes() {
    // Distinct from cancelled: `complete` is derived as "nothing pending
    // AND nothing lost AND not interrupted", so branching on it alone
    // would collapse these two into one message.
    let msg = summarize_image_decrypt(
        &copy_result(false, 1_073_741_824, 2 * 1_048_576),
        std::path::Path::new("/out/Disc.iso"),
    );
    assert!(msg.starts_with("Decrypted image written"), "{msg}");
    assert!(
        msg.contains("2.0 MiB unreadable"),
        "damage must be reported, not silently dropped: {msg}"
    );
}

/// The term the old `bytes_unreadable > 0` test dropped. `recovery::copy`
/// returns pending bytes with `halted` false and nothing permanently lost
/// (its terminal-result paths), and that read as a clean write.
#[test]
fn an_image_decrypt_with_bytes_still_pending_is_not_a_clean_write() {
    let msg = summarize_image_decrypt(
        &copy_result_pending(false, 1_073_741_824, 0, 4 * 1_048_576),
        std::path::Path::new("/out/Disc.iso"),
    );
    assert!(
        msg.contains("4.0 MiB not read"),
        "unread bytes must be reported, not silently dropped: {msg}"
    );
    assert_ne!(
        msg, "Decrypted image written: /out/Disc.iso (1.00 GiB)",
        "an incomplete image must not render as the clean-success line"
    );
}

/// Both shortfalls at once — neither term may mask the other.
#[test]
fn an_image_decrypt_reports_lost_and_unread_bytes_together() {
    let msg = summarize_image_decrypt(
        &copy_result_pending(false, 1_073_741_824, 2 * 1_048_576, 4 * 1_048_576),
        std::path::Path::new("/out/Disc.iso"),
    );
    assert!(msg.contains("2.0 MiB unreadable"), "{msg}");
    assert!(msg.contains("4.0 MiB not read"), "{msg}");
}

#[test]
fn a_lossy_extraction_reports_the_unreadable_span_in_mib() {
    let res = extract_result(false, 3, 3 * 1_048_576);
    let msg = summarize_extract(&res, std::path::Path::new("/out/Disc"));
    assert_eq!(
        msg,
        "Decrypted file tree written to /out/Disc — 3 file(s), 3.0 MiB unreadable"
    );
}

#[test]
fn a_completed_extraction_reports_written() {
    let res = extract_result(false, 3, 0);
    let msg = summarize_extract(&res, std::path::Path::new("/out/Disc"));
    assert_eq!(msg, "Decrypted file tree written to /out/Disc — 3 file(s)");
}

#[test]
fn an_extraction_halted_by_cancel_says_so_not_written() {
    // Regression: this used to report "Decrypted file tree written to
    // /out/Disc — 2 file(s)" — indistinguishable from a real, complete
    // extraction — for a run the user actually cancelled partway through.
    let res = extract_result(true, 2, 0);
    let msg = summarize_extract(&res, std::path::Path::new("/out/Disc"));
    assert!(
        msg.starts_with("Cancelled"),
        "a Cancel mid-extraction must not read as a clean write: {msg}"
    );
    assert!(
        !msg.contains("written"),
        "a halted extraction must not use the same wording as a completed one: {msg}"
    );
}

#[test]
fn a_halted_extraction_still_beats_unreadable_wording() {
    // `halted` must win over the `bytes_unreadable > 0` branch too, or a
    // cancelled-with-some-bad-sectors run reports the wrong one of two
    // equally wrong messages instead of the right one.
    let res = extract_result(true, 1, 1_048_576);
    let msg = summarize_extract(&res, std::path::Path::new("/out/Disc"));
    assert!(msg.starts_with("Cancelled"), "{msg}");
}
