//! The console's wording: what `freemkv iso://… mkv://…` prints for the same
//! job, from the engine's callbacks.
//!
//! The CLI's own printers are private to `pipe.rs`; these mirror them line for
//! line (same locale keys, same number formats) until the CLI moves onto the
//! shared core, when both should call one formatter.

use crate::strings;

/// `  Streams: N` and one line per stream, as `pipe.rs` `print_stream_info`.
pub fn stream_lines(t: &libfreemkv::DiscTitle) -> Vec<String> {
    let clean = strings::sanitize_display;
    let mut out = vec![format!(
        "  {}: {}",
        strings::get("disc.streams"),
        t.streams.len()
    )];
    for s in &t.streams {
        out.push(match s {
            libfreemkv::Stream::Video(v) => {
                let label = if v.label.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", clean(&v.label))
                };
                format!("    {} {}{}", v.codec, v.resolution, label)
            }
            libfreemkv::Stream::Audio(a) => {
                let mut tags: Vec<String> = Vec::new();
                if let Some(key) = purpose_key(a.purpose) {
                    tags.push(strings::get(key));
                }
                if a.secondary {
                    tags.push(strings::get("stream.secondary"));
                }
                if !a.label.is_empty() {
                    tags.push(clean(&a.label));
                }
                let label = if tags.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", tags.join(", "))
                };
                format!(
                    "    {} {} {}{}",
                    a.codec,
                    a.channels,
                    clean(&a.language),
                    label
                )
            }
            libfreemkv::Stream::Subtitle(s) => format!("    {} {}", s.codec, clean(&s.language)),
        });
    }
    if t.duration_secs > 0.0 {
        let d = t.duration_secs as u64;
        out.push(format!(
            "  {}: {}:{:02}:{:02}",
            strings::get("disc.duration"),
            d / 3600,
            d % 3600 / 60,
            d % 60
        ));
    }
    out
}

fn purpose_key(p: libfreemkv::LabelPurpose) -> Option<&'static str> {
    match p {
        libfreemkv::LabelPurpose::Commentary => Some("stream.purpose.commentary"),
        libfreemkv::LabelPurpose::Descriptive => Some("stream.purpose.descriptive"),
        libfreemkv::LabelPurpose::Score => Some("stream.purpose.score"),
        libfreemkv::LabelPurpose::Ime => Some("stream.purpose.ime"),
        libfreemkv::LabelPurpose::Normal => None,
    }
}

/// The in-place progress line, as `pipe.rs` `print_progress` (without its `\r`).
pub fn progress_line(done: u64, total: u64, speed_bps: u64, eta_secs: Option<u64>) -> String {
    let mb_done = done as f64 / 1_048_576.0;
    let speed = speed_bps as f64 / 1_048_576.0;
    if total == 0 {
        return format!("  {mb_done:.1} MB  {speed:.1} MB/s");
    }
    let pct = (done as f64 / total as f64 * 100.0).min(100.0);
    let mb_total = total as f64 / 1_048_576.0;
    let eta = eta_secs.map_or_else(
        || "?:??".to_string(),
        |s| format!("{}:{:02}", s / 60, s % 60),
    );
    if mb_total >= 1024.0 {
        format!(
            "  {:.1} GB / {:.1} GB  ({pct:.1}%)  {speed:.1} MB/s  ETA {eta}",
            mb_done / 1024.0,
            mb_total / 1024.0
        )
    } else {
        format!("  {mb_done:.0} MB / {mb_total:.0} MB  ({pct:.1}%)  {speed:.1} MB/s  ETA {eta}")
    }
}

/// `Complete: 5.7 GB in 99s (59 MB/s)`, as `pipe.rs` `print_completion_summary`.
pub fn complete_line(bytes: u64, secs: f64) -> String {
    let mb = bytes as f64 / 1_048_576.0;
    let (size, unit) = if mb >= 1024.0 {
        (mb / 1024.0, "GB")
    } else {
        (mb, "MB")
    };
    let speed = if secs > 0.0 { mb / secs } else { 0.0 };
    strings::fmt(
        "rip.complete",
        &[
            ("size", &format!("{size:.1}")),
            ("unit", unit),
            ("time", &format!("{secs:.0}")),
            ("speed", &format!("{speed:.0}")),
        ],
    )
}

/// The remux's extra step the CLI does not have: the verify of the new file.
pub fn verify_line(
    path: &std::path::Path,
    ok: bool,
    runtime: Option<f64>,
    expected: f64,
) -> String {
    let t = |s: f64| {
        let s = s as u64;
        format!("{}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!(
        "Verify {name}...{}  runtime {} of {}",
        if ok {
            strings::get("rip.ok")
        } else {
            "FAILED".to_string()
        },
        runtime.map_or_else(|| "unknown".to_string(), t),
        t(expected)
    )
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
