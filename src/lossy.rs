//! What a COMPLETED mux still has to tell the user — one renderer, both shells.
//!
//! `completed = true` is not "the file is what you asked for". `MuxOutcome`
//! carries two independent losses alongside it: `undelivered_streams` (whole
//! tracks the sink could not deliver) and `errors` / `lost_bytes` (bytes read
//! but not carried). Declared by both crate roots so the CLI's `pipe` and
//! the GUI's `engine` render one answer instead of two.

/// Every line a finished mux must add when the file is not everything that was
/// asked for. Empty when nothing was lost.
///
/// `target` is the destination as the user asked for it, because the loss line
/// names the file it happened to. This is a warning on a still-successful rip,
/// not a failure. The dropped-bytes line reuses `dir.file_lossy`; the
/// undelivered-streams header uses a container-agnostic `mux.undelivered_header`
/// (English fallback until the catalog ships it), since loss isn't mp4-specific.
pub fn lossy_lines(outcome: &libfreemkv::MuxOutcome, target: &str) -> Vec<String> {
    let mut lines = Vec::new();
    if !outcome.undelivered_streams.is_empty() {
        // Container-agnostic: `undelivered_streams` can come from ANY sink, so
        // `mp4.excluded_header`'s "in an MP4" phrasing is wrong for non-mp4. Use
        // a generic header (English until the catalog), twin of `summarize_stream`.
        lines.push(crate::strings::fmt_or(
            "mux.undelivered_header",
            "Note: {count} stream(s) could not be delivered and were left out:",
            &[("count", &outcome.undelivered_streams.len().to_string())],
        ));
        lines.extend(
            outcome
                .undelivered_streams
                .iter()
                // 1-based, matching the CLI, the GUI and the `info` listing.
                .map(|idx| {
                    format!(
                        "    - {} {}",
                        crate::strings::get("stream.track"),
                        idx.saturating_add(1)
                    )
                }),
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

/// The note a mux prints BEFORE it starts when the destination cannot carry every track
/// (G4, mpg-output-design v5 §5): `mux.excluded_header`, then one line per track with its
/// `mux.reason.*`. Empty when nothing is left out. Both shells print it, from libfreemkv's
/// container-neutral plan; a declared-only MPEG-2 extension is never in it (J23).
pub fn excluded_lines(dest: &str, title: &libfreemkv::DiscTitle) -> Vec<String> {
    let url = libfreemkv::parse_url(dest);
    let report = libfreemkv::fit_report(&url, title);
    if report.skipped.is_empty() {
        return Vec::new();
    }
    let container = container_name(&url);
    let reasons: Vec<libfreemkv::SkipReason> = report.skipped.iter().map(|&(_, r)| r).collect();
    let mut lines = vec![crate::strings::fmt(
        "mux.excluded_header",
        &[
            ("count", &report.skipped.len().to_string()),
            ("container", container),
            ("keep", keep_for(&reasons)),
        ],
    )];
    for (idx, reason) in &report.skipped {
        lines.push(format!(
            "    - {} {}: {}",
            crate::strings::get("stream.track"),
            idx + 1,
            crate::strings::fmt(reason_key(*reason), &[("container", container)])
        ));
    }
    lines
}

/// Design §5 `{keep}`: `mpg` if every excluded track is the MPEG-2 extension (only mpg://
/// keeps it), otherwise `mkv`.
pub fn keep_for(reasons: &[libfreemkv::SkipReason]) -> &'static str {
    if !reasons.is_empty()
        && reasons
            .iter()
            .all(|r| *r == libfreemkv::SkipReason::Mp2Extension)
    {
        "mpg"
    } else {
        "mkv"
    }
}

fn container_name(url: &libfreemkv::StreamUrl) -> &'static str {
    match url.scheme() {
        "mp4" => "MP4",
        "mpg" => "MPG",
        "m2ts" => "M2TS",
        "mkv" => "MKV",
        _ => "this output",
    }
}

fn reason_key(reason: libfreemkv::SkipReason) -> &'static str {
    use libfreemkv::SkipReason as R;
    match reason {
        R::BitmapSubtitle | R::UnmappableSubtitle => "mux.reason.subtitle",
        R::SecondaryVideo => "mux.reason.video",
        R::UnmappableVideo => "mux.reason.video_unmappable",
        R::Mp2Extension => "mux.reason.mp2_extension",
        R::NoStreamId => "mux.reason.no_stream_id",
        // UnmappableAudio, and the post-mux NoSamples/UndescribableAudio; SkipReason is
        // #[non_exhaustive], so a new reason reads as an audio mapping gap until named.
        _ => "mux.reason.audio",
    }
}

/// Whether a finished mux lost anything at all — the one question both shells'
/// summary text has to ask before it can say "written".
///
/// The LIBRARY target always exercises this; the BIN target's reachability depends on which
/// shell that platform compiles, so a plain `#[allow(dead_code)]` is used rather than a
/// per-platform `cfg`.
#[allow(dead_code)]
pub fn is_lossy(outcome: &libfreemkv::MuxOutcome) -> bool {
    !outcome.undelivered_streams.is_empty() || outcome.lost_bytes > 0 || outcome.errors > 0
}

// Bytes as MB for the loss line, ROUNDED UP: rounding to nearest would render a real loss as
// "0.00 MB lost" on the one path meant to say something was lost.
fn lost_mb(bytes: u64) -> String {
    if bytes == 0 {
        // Skip events with no byte count attached (`errors > 0`, `lost_bytes`
        // zero): the line still has to be printed, and 0.00 is the truthful
        // number for what the library could quantify.
        return "0.00".to_string();
    }
    // Hundredths of a MiB, rounded up, via one exact division (a rounded
    // 1-MiB/100 constant drifted to "1023.98 MB" for a whole gibibyte).
    // `u128` because `bytes * 100` overflows `u64` for a large enough loss.
    let hundredths = (u128::from(bytes) * 100).div_ceil(1 << 20);
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

#[cfg(test)]
mod tests {
    use super::{excluded_lines, is_lossy, keep_for, lossy_lines, lost_mb};

    fn title(streams: Vec<libfreemkv::Stream>) -> libfreemkv::DiscTitle {
        libfreemkv::DiscTitle {
            streams,
            ..libfreemkv::DiscTitle::empty()
        }
    }
    fn video(codec: libfreemkv::Codec) -> libfreemkv::Stream {
        libfreemkv::Stream::Video(libfreemkv::VideoStream {
            pid: 0x1011,
            codec,
            resolution: libfreemkv::Resolution::R1080p,
            frame_rate: libfreemkv::FrameRate::F23_976,
            hdr: libfreemkv::HdrFormat::Sdr,
            color_space: libfreemkv::ColorSpace::Bt709,
            display_aspect: None,
            secondary: false,
            label: String::new(),
            measured_cicp: None,
        })
    }
    fn audio(pid: u16, codec: libfreemkv::Codec) -> libfreemkv::Stream {
        libfreemkv::Stream::Audio(libfreemkv::AudioStream {
            pid,
            codec,
            channels: libfreemkv::AudioChannels::Stereo,
            language: "eng".into(),
            sample_rate: libfreemkv::SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: String::new(),
        })
    }

    // G4 (mpg-output-design v5 §1.1, §5): one container-neutral pre-mux note for every
    // destination, from libfreemkv's generic fit plan; `{keep}` is mkv unless every exclusion
    // is the MPEG-2 extension.
    #[test]
    fn excluded_lines_name_the_container_and_each_track() {
        crate::strings::set_locale("en");
        let t = title(vec![
            video(libfreemkv::Codec::Hevc),
            audio(0x1100, libfreemkv::Codec::TrueHd),
            audio(0x1101, libfreemkv::Codec::Ac3),
        ]);
        let lines = excluded_lines("mp4:///o/x.mp4", &t);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].contains("MP4") && lines[0].contains("mkv://") && lines[0].contains('1'),
            "{lines:?}"
        );
        assert!(
            lines[1].contains(" 2:") && lines[1].contains("MP4"),
            "track 2, reason: {lines:?}"
        );
        let mpg = excluded_lines("mpg:///o/x.mpg", &t);
        assert_eq!(mpg.len(), 3, "HEVC and TrueHD, not the AC-3 (J24): {mpg:?}");
        assert!(mpg[0].contains("MPG"), "{mpg:?}");
        assert!(
            excluded_lines("mkv:///o/x.mkv", &t).is_empty(),
            "mkv keeps everything"
        );
    }

    // J23: "the generic pre-mux fit report leaves out declared-only extension tracks".
    #[test]
    fn j23_a_declared_only_mp2_extension_is_never_in_the_excluded_note() {
        crate::strings::set_locale("en");
        let main = vec![
            video(libfreemkv::Codec::Mpeg2),
            audio(0xC0, libfreemkv::Codec::Mp2),
        ];
        let mut with_ext = main.clone();
        with_ext.push(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
            pid: 0xD0,
            codec: libfreemkv::Codec::Mp2,
            channels: libfreemkv::AudioChannels::Stereo,
            language: "eng".into(),
            sample_rate: libfreemkv::SampleRate::S48,
            secondary: false,
            purpose: libfreemkv::LabelPurpose::Normal,
            label: libfreemkv::disc::MP2_EXTENSION_LABEL.into(),
        }));
        for dest in ["mkv:///o/x.mkv", "mp4:///o/x.mp4", "mpg:///o/x.mpg"] {
            let lines = excluded_lines(dest, &title(with_ext.clone()));
            assert!(
                !lines.iter().any(|l| l.contains(" 3:")),
                "{dest}: the declared-only extension is listed: {lines:?}"
            );
            assert_eq!(
                lines,
                excluded_lines(dest, &title(main.clone())),
                "{dest}: the extension changed the note"
            );
        }
    }

    #[test]
    fn keep_is_mpg_only_when_every_exclusion_is_the_extension() {
        use libfreemkv::SkipReason as R;
        assert_eq!(keep_for(&[R::Mp2Extension]), "mpg");
        assert_eq!(keep_for(&[R::Mp2Extension, R::UnmappableAudio]), "mkv");
        assert_eq!(keep_for(&[R::BitmapSubtitle]), "mkv");
    }

    /// Every pre-mux reason renders its own resolving line (the retired
    /// `mp4_skip_reasons_render_distinct_resolving_strings`, now container-neutral).
    #[test]
    fn skip_reasons_render_distinct_resolving_strings() {
        use libfreemkv::SkipReason as R;
        crate::strings::set_locale("en");
        let mut seen = std::collections::BTreeSet::new();
        for r in [
            R::BitmapSubtitle,
            R::UnmappableAudio,
            R::SecondaryVideo,
            R::UnmappableVideo,
            R::Mp2Extension,
            R::NoStreamId,
        ] {
            let key = super::reason_key(r);
            let msg = crate::strings::fmt(key, &[("container", "MPG")]);
            assert!(!msg.starts_with("mux."), "{r:?}: {key} does not resolve");
            assert!(!msg.contains('{'), "{r:?}: unfilled placeholder in {msg:?}");
            assert!(seen.insert(msg.clone()), "{r:?}: duplicate message {msg:?}");
        }
        assert_eq!(
            super::reason_key(R::UnmappableSubtitle),
            super::reason_key(R::BitmapSubtitle)
        );
        assert!(!crate::strings::get("mux.excluded_header").starts_with("mux."));
    }

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

    /// Undelivered streams are not an MP4 problem: an mkv/m2ts loss must not
    /// be told its tracks "can't be stored in an MP4".
    #[test]
    fn the_undelivered_header_does_not_blame_mp4_for_other_containers() {
        crate::strings::set_locale("en");
        for target in ["/out/movie.mkv", "/out/movie.m2ts"] {
            let lines = lossy_lines(&outcome(vec![0], 0, 0), target);
            assert_eq!(lines.len(), 2, "header and the track: {lines:?}");
            assert!(!lines[0].to_lowercase().contains("mp4"), "{lines:?}");
            assert!(lines[0].contains('1'), "the count is named: {lines:?}");
        }
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
