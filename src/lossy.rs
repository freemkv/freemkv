//! What a COMPLETED mux still has to tell the user — one renderer, both shells.
//!
//! `completed = true` is not "the file is what you asked for". `MuxOutcome`
//! carries two independent losses alongside it, and a run can have either with
//! `completed` set:
//!
//! - `undelivered_streams` — whole tracks the sink accepted frames for and
//!   could not put in the finished container (today the `mp4://` sink dropping
//!   an audio track with no parseable sample entry);
//! - `errors` / `lost_bytes` — bytes the library READ and could not carry. An
//!   `mkv://` → `mkv://` re-mux of a Blu-ray 3D rip drops the entire
//!   dependent-view (right-eye) payload into these, with `undelivered_streams`
//!   EMPTY, because no whole stream was lost.
//!
//! The second pair was read nowhere outside test fixtures: the CLI printed
//! "Complete" and exited 0, and the GUI said "Written to <dir>", over a file
//! missing one eye of the film. That is the exact "data loss looks like
//! success" shape this project forbids.
//!
//! Declared by both crate roots (the `title_identity` / `file_identity`
//! pattern) so the CLI's `pipe` and the GUI's `engine` render one answer. They
//! had two before — `print_undelivered_streams` and `undelivered_lines`, each
//! covering only half of what the outcome carried.

/// Every line a finished mux must add when the file is not everything that was
/// asked for. Empty when nothing was lost.
///
/// `target` is the destination as the user asked for it, because the loss line
/// names the file it happened to.
///
/// **A warning on a still-successful rip, not a failure.** The file is
/// finalised, structurally valid and playable; it is missing part of its
/// content, not truncated. Escalating would abandon the title — and, through
/// the batch policy both shells share, whole multi-title runs — and the GUI's
/// per-title failure arm DELETES the output file, which would destroy the very
/// bytes this warning exists to preserve. Loss is reported at `Level::Always`
/// on the CLI side so `--quiet` cannot hide it.
///
/// **Existing strings only.** The locale catalogs live in a separate, pinned
/// crate, so a new key cannot ship from here and an English-only one would ship
/// English into 28 other languages. `mp4.excluded_header` already says
/// "track(s) can't be stored in an MP4 and will be left out — use `mkv://` to
/// keep everything"; `dir.file_lossy` already says "  {file}: {lost} MB lost"
/// and is used for exactly this — per-file byte loss — on the extraction path.
pub fn lossy_lines(outcome: &libfreemkv::MuxOutcome, target: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if !outcome.undelivered_streams.is_empty() {
        lines.push(crate::strings::fmt(
            "mp4.excluded_header",
            &[("count", &outcome.undelivered_streams.len().to_string())],
        ));
        lines.extend(
            outcome
                .undelivered_streams
                .iter()
                // 1-based, matching the CLI, the GUI and the `info` listing.
                .map(|idx| format!("    - {} {}", crate::strings::get("stream.track"), idx + 1)),
        );
    }
    if outcome.lost_bytes > 0 || outcome.errors > 0 {
        lines.push(crate::strings::fmt(
            "dir.file_lossy",
            &[("file", target), ("lost", &lost_mb(outcome.lost_bytes))],
        ));
    }
    lines
}

/// Whether a finished mux lost anything at all — the one question both shells'
/// summary text has to ask before it can say "written".
///
/// Reached from `engine::summarize_stream`, which only the GUI shells call, so
/// in the BIN target on a platform with no GUI this is genuinely unreachable
/// and `-D warnings` rejects it. The library target always uses it, and so does
/// the bin on macOS and Windows — hence a narrow `cfg_attr` rather than a bare
/// `allow(dead_code)`, which would also hide a real future disuse on the
/// platforms that do call it.
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub fn is_lossy(outcome: &libfreemkv::MuxOutcome) -> bool {
    !outcome.undelivered_streams.is_empty() || outcome.lost_bytes > 0 || outcome.errors > 0
}

/// Bytes as MB for the loss line, ROUNDED UP.
///
/// Rounding to nearest would render a real loss of a few hundred KB as
/// "0.00 MB lost" — a line that says nothing was lost, on the one code path
/// whose entire purpose is to say something was. Rounding up keeps the number
/// honest in the only direction that matters here, and any non-zero loss
/// therefore shows at least 0.01.
fn lost_mb(bytes: u64) -> String {
    if bytes == 0 {
        // Skip events with no byte count attached (`errors > 0`, `lost_bytes`
        // zero): the line still has to be printed, and 0.00 is the truthful
        // number for what the library could quantify.
        return "0.00".to_string();
    }
    // Hundredths of a MiB, rounded up. Computed as one exact division rather
    // than through a rounded 1-MiB/100 constant, whose drift showed up as
    // "1023.98 MB" for a whole gibibyte. `u128` because `bytes * 100` overflows
    // `u64` for a large enough loss.
    let hundredths = (u128::from(bytes) * 100).div_ceil(1 << 20);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

#[cfg(test)]
mod tests {
    use super::{is_lossy, lossy_lines, lost_mb};

    fn outcome(undelivered: Vec<usize>, errors: u64, lost_bytes: u64) -> libfreemkv::MuxOutcome {
        libfreemkv::MuxOutcome {
            completed: true,
            output_opened: true,
            bytes_written: 4 << 30,
            errors,
            lost_bytes,
            streams: 3,
            undelivered_streams: undelivered,
        }
    }

    /// The clean run says nothing. An unconditional warning is worse than none:
    /// it trains the user to ignore the line that matters.
    #[test]
    fn a_mux_that_lost_nothing_produces_no_lines() {
        let o = outcome(Vec::new(), 0, 0);
        assert!(lossy_lines(&o, "/out/movie.mkv").is_empty());
        assert!(!is_lossy(&o));
    }

    /// The dependent-view case: no stream is missing, and 3 MB of payload is.
    #[test]
    fn dropped_payload_bytes_are_named_with_their_size_and_file() {
        crate::strings::set_locale("en");
        let o = outcome(Vec::new(), 2, 3 << 20);
        let lines = lossy_lines(&o, "/out/movie.mkv");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("/out/movie.mkv"), "{lines:?}");
        assert!(lines[0].contains("3.00"), "{lines:?}");
        assert!(lines[0].contains("lost"), "{lines:?}");
        assert!(is_lossy(&o));
    }

    /// Both losses at once are both reported — one is not a substitute for the
    /// other, and the tracks come first because they are the coarser fact.
    #[test]
    fn a_dropped_track_and_dropped_bytes_are_both_reported() {
        crate::strings::set_locale("en");
        let lines = lossy_lines(&outcome(vec![1], 1, 1 << 20), "/out/movie.mp4");
        assert_eq!(lines.len(), 3, "header, the track, the bytes: {lines:?}");
        assert!(lines[1].ends_with(" 2"), "1-based track number: {lines:?}");
        assert!(lines[2].contains("lost"), "{lines:?}");
    }

    /// A loss under a megabyte must not render as "0.00 MB lost" — the number
    /// would contradict the line it is in.
    #[test]
    fn a_sub_megabyte_loss_never_rounds_away_to_nothing() {
        assert_eq!(lost_mb(0), "0.00");
        assert_eq!(lost_mb(1), "0.01", "one byte is still a loss");
        // A hundredth of a MiB is 10485.76 bytes, so 10485 still rounds up to
        // one hundredth and anything past it to two — UP, never down.
        assert_eq!(lost_mb(10_485), "0.01");
        assert_eq!(lost_mb(10_486), "0.02", "rounds UP, never down");
        assert_eq!(lost_mb(1 << 20), "1.00");
        assert_eq!(lost_mb(3 << 20), "3.00");
        assert_eq!(lost_mb(1_073_741_824), "1024.00");
    }

    /// `errors` without a byte count is still a loss the user must see.
    #[test]
    fn a_skip_event_with_no_byte_total_still_reports() {
        crate::strings::set_locale("en");
        let o = outcome(Vec::new(), 1, 0);
        assert!(is_lossy(&o));
        assert_eq!(lossy_lines(&o, "/out/movie.mkv").len(), 1);
    }
}
