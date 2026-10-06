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
        // the MP4-only excluded header's "in an MP4" phrasing is wrong for non-mp4. Use
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
            idx.saturating_add(1),
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

/// A small clear `mpg://` file for tests that need a real source whose pre-mux note is not
/// empty: MPEG-2 video and one DVD subpicture track (neither of which MP4 carries).
#[cfg(test)]
pub(crate) fn mpg_source_fixture(path: &std::path::Path) {
    use libfreemkv::{
        Codec, ColorSpace, DiscTitle, FrameRate, HdrFormat, LabelQualifier, PesFrame, Resolution,
        Stream, SubtitleStream, VideoStream,
    };
    let title = DiscTitle {
        streams: vec![
            Stream::Video(VideoStream {
                pid: 0xE0,
                codec: Codec::Mpeg2,
                resolution: Resolution::R576i,
                frame_rate: FrameRate::F25,
                hdr: HdrFormat::Sdr,
                color_space: ColorSpace::Bt470bg,
                display_aspect: None,
                secondary: false,
                label: String::new(),
                measured_cicp: None,
            }),
            Stream::Subtitle(SubtitleStream {
                pid: 0x20,
                codec: Codec::DvdSub,
                language: "eng".into(),
                forced: false,
                qualifier: LabelQualifier::None,
                codec_data: None,
            }),
        ],
        codec_privates: vec![None, None],
        ..DiscTitle::empty()
    };
    // 13818-2 sequence header (720x576, 25 Hz) + sequence_extension; an I or P picture.
    const SEQ: [u8; 22] = [
        0, 0, 1, 0xB3, 0x2D, 0x02, 0x40, 0x23, 0xFF, 0xFF, 0xE3, 0x80, 0, 0, 1, 0xB5, 0x14, 0x8A,
        0x00, 0x01, 0x00, 0x00,
    ];
    let pic = |coding: u8| {
        let mut v = vec![0, 0, 1, 0x00, 0x00, coding << 3, 0xFF, 0xF8];
        v.extend_from_slice(&[0, 0, 1, 0xB5, 0x8F, 0xFF, 0xF3, 0x80, 0x80]);
        v.extend_from_slice(&[0, 0, 1, 0x01, 0x12, 0x34, 0x56]);
        v
    };
    let frame = |track: usize, pts: i64, keyframe: bool, data: Vec<u8>| PesFrame {
        track,
        pts,
        keyframe,
        data,
        duration_ns: None,
        discard_padding_ns: 0,
        source: None,
        coding: None,
    };
    let url = format!("mpg://{}", path.display());
    let mut sink = libfreemkv::output(&url, &title, None).expect("mpg:// output");
    for k in 0..50i64 {
        let pts = 1_000_000_000 + k * 40_000_000;
        let key = k % 10 == 0;
        let mut data = if key { SEQ.to_vec() } else { Vec::new() };
        data.extend(pic(if key { 1 } else { 2 }));
        data.resize(2_000, 0x55);
        sink.write(&frame(0, pts, key, data)).expect("video");
        if k % 10 == 5 {
            let mut spu = vec![0x01, 0xF4];
            spu.resize(500, 0x11);
            sink.write(&frame(1, pts, true, spu)).expect("subpicture");
        }
    }
    sink.finish().expect("finish");
}

#[cfg(test)]
#[path = "lossy_tests.rs"]
mod tests;
