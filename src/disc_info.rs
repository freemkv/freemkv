// freemkv info disc:// — Show disc titles, streams, and sizes. MIT — freemkv project.
// CLI is dumb — all logic in libfreemkv. This file only formats output.

use crate::output::{Level::Normal, Output};
use crate::strings;
use libfreemkv::disc::{BdRegion, DiscRegion};
use libfreemkv::{
    AudioStream, Codec, ColorSpace, Disc, DiscFormat, HdrFormat, LabelPurpose, LabelQualifier,
    ScanOptions, Stream, SubtitleStream, VideoStream,
};

// Strip control/escape chars from untrusted on-disc metadata (title, volume
// label, stream labels) so a crafted disc can't inject terminal escapes
// (color/cursor/OSC) via those fields.
pub(crate) fn sanitize(s: &str) -> String {
    // One implementation, two targets: this was declared only by `main.rs`, so
    // desktop shells (lib target) couldn't call it and went unsanitised. Now lives
    // in `engine`, shared by both.
    crate::strings::sanitize_display(s)
}

/// Flags accepted by `freemkv info <url>`, for every URL scheme.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct InfoFlags {
    pub quiet: bool,
    pub verbose: bool,
    pub full: bool,
    pub basic: bool,
    pub keydb: Option<String>,
    /// The raw `--log-level` value when it failed to parse, so `run` can
    /// report it instead of the value silently becoming level 1.
    pub bad_log_level: Option<String>,
}

/// Outcome of parsing an `info` flag list. `Help` and `Unknown` are returned
/// rather than acted on so the parser stays testable — the caller prints and
/// exits.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InfoParse {
    Ok(Box<InfoFlags>),
    Help,
    /// An option the `info` route does not accept, carrying the offending token.
    Unknown(String),
}

// One parser for every scheme: `iso://` used to scan args for `--full` and
// ignore everything else, so a typo there silently dropped the request
// instead of exiting 1 like `disc://`. Same vocabulary, same rejection.
pub(crate) fn parse_info_flags(args: &[String]) -> InfoParse {
    let mut f = InfoFlags::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--quiet" | "-q" => f.quiet = true,
            "--verbose" | "-v" => f.verbose = true,
            // `--keydb PATH`: used only on `-v` to resolve keys for the crypto block.
            // Accept + capture its value on every path so it isn't mistaken for a
            // positional / unknown option.
            "--keydb" => {
                // Only a real value, never the next flag: `info --keydb
                // --full` used to set the keydb path to "--full" and drop the
                // `--full`. See `cli_entry::is_flag_token`.
                if let Some(v) = args
                    .get(i + 1)
                    .filter(|v| !crate::cli_entry::is_flag_token(v))
                {
                    f.keydb = Some(v.clone());
                    i += 1;
                }
            }
            // `--log-level N` sets the tracing level (in main::init_logging);
            // here it also widens stdout detail at level >= 2. Accept + skip
            // its value so it isn't treated as a positional / unknown option.
            "--log-level" => {
                if let Some(v) = args
                    .get(i + 1)
                    .filter(|v| !crate::cli_entry::is_flag_token(v))
                {
                    match v.parse::<u8>() {
                        Ok(n) if n >= 2 => f.verbose = true,
                        Ok(_) => {}
                        Err(_) => f.bad_log_level = Some(v.clone()),
                    }
                    i += 1;
                }
            }
            "--log-file" => {
                // Skip the path value — but only if there IS one, or the flag
                // that follows is swallowed instead.
                if args
                    .get(i + 1)
                    .is_some_and(|v| !crate::cli_entry::is_flag_token(v))
                {
                    i += 1;
                }
            }
            "--full" | "-f" => f.full = true,
            "--basic" | "-b" => f.basic = true,
            "--share" | "-s" | "--mask" | "-m" => {
                // Drive-profile capture: meaningful only for `disc://`, consumed by
                // `info::run` earlier. Listed so it's reported unsupported-here on
                // an `iso://` URL rather than silently accepted.
                return InfoParse::Unknown(args[i].clone());
            }
            "--help" | "-h" => return InfoParse::Help,
            other => return InfoParse::Unknown(other.to_string()),
        }
        i += 1;
    }
    InfoParse::Ok(Box::new(f))
}

/// Print the offending option and exit 1 — the shared unknown-flag behaviour
/// for every `info` route.
pub(crate) fn reject_unknown_option(opt: &str) -> ! {
    eprintln!("{}", strings::fmt("app.unknown_option", &[("opt", opt)]));
    crate::cli_entry::exit(1);
}

pub(crate) fn run(device: Option<&str>, args: &[String]) {
    let flags = match parse_info_flags(args) {
        InfoParse::Ok(f) => f,
        InfoParse::Help => {
            println!("{}", strings::get("disc.usage"));
            return;
        }
        InfoParse::Unknown(opt) => reject_unknown_option(&opt),
    };
    let InfoFlags {
        quiet,
        verbose,
        full,
        basic,
        keydb,
        bad_log_level,
    } = *flags;

    let out = Output::new(verbose, quiet);

    if let Some(v) = &bad_log_level {
        out.raw(
            Normal,
            &strings::fmt_or(
                "error.log_level_not_a_number",
                "--log-level: expected a number 1–4, got '{value}', ignored",
                &[("value", v)],
            ),
        );
    }
    out.raw(Normal, &format!("freemkv {}", env!("CARGO_PKG_VERSION")));
    out.blank(Normal);
    out.print(Normal, "disc.scanning");
    out.blank(Normal);

    let target = match device {
        Some(p) => libfreemkv::DeviceTarget::Path(std::path::PathBuf::from(p)),
        None => libfreemkv::DeviceTarget::Autodetect,
    };
    // Normal `info` is a fast keyless scan; `-v` supplies AACS host credentials so
    // the handshake captures the VID + Unit_Key_RO.inf a locked drive needs. Open's
    // bring-up is advisory/non-fatal — `session.scan` below is the authoritative gate.
    let keyspec = libfreemkv::KeySpec {
        credentials: if verbose {
            crate::pipe::drive_credentials(&keydb)
        } else {
            None
        },
        ..Default::default()
    };
    let mut session = freemkv_engine::drive::open_session(target, keyspec).unwrap_or_else(|e| {
        match &e {
            // Autodetect with no drive surfaces as an empty-path DeviceNotFound;
            // keep the dedicated "no drive" message. Any other open failure (or a
            // real path that won't open) renders through the E-code humanizer.
            libfreemkv::Error::DeviceNotFound { path } if path.is_empty() => {
                eprintln!("{}", strings::get("error.no_drive"));
            }
            _ => eprintln!("{}", crate::pipe::fmt_err(&e)),
        }
        crate::cli_entry::exit(1);
    });

    // Reads PGS streams to detect forced subtitles from content, matching a rip's
    // muxer. Gated to verbose: it needs AACS keys for encrypted UHD subtitles and
    // reads the clip (slow) — keyless `info` stays fast, using vendor-label forced.
    let scan_opts = ScanOptions {
        probe_forced_subtitles: verbose,
        ..Default::default()
    };
    if let Err(e) = session.scan(scan_opts) {
        eprintln!(
            "{}",
            strings::fmt(
                "error.scan_failed",
                &[("detail", &crate::pipe::fmt_err(&e))]
            )
        );
        crate::cli_entry::exit(1);
    }
    // Decompose the session into the owned disc + drive the rest of this command
    // already worked with, so downstream rendering is untouched.
    let Some(disc) = session.take_disc() else {
        eprintln!(
            "{}",
            strings::fmt(
                "error.scan_failed",
                &[(
                    "detail",
                    &crate::pipe::fmt_err(&libfreemkv::Error::NoStreams)
                )]
            )
        );
        crate::cli_entry::exit(1);
    };
    // into_drive is fallible: stage_drive_as_reader moves the drive out, so an
    // empty slot is reachable through ordinary API use. Match the local style
    // the session open above uses.
    let mut drive = session.into_drive().unwrap_or_else(|e| {
        eprintln!("{}", crate::pipe::fmt_err(&e));
        crate::cli_entry::exit(1);
    });

    // Disc title
    if let Some(ref title) = disc.meta_title {
        out.raw(
            Normal,
            &format!("{}: {}", strings::get("disc.disc"), sanitize(title)),
        );
    } else if !disc.volume_id.is_empty() {
        out.raw(
            Normal,
            &format!(
                "{}: {}",
                strings::get("disc.disc"),
                sanitize(&format_volume_id(&disc.volume_id))
            ),
        );
    }

    // Format and capacity. An unclassified disc must NOT masquerade as Blu-ray
    // — report it distinctly so data/future/unknown discs aren't misread.
    let format = freemkv::engine::format_name(&disc.format);
    let gb = disc.capacity_bytes as f64 / 1_000_000_000.0; // decimal GB, matches disc-marketed capacity
    out.raw(
        Normal,
        &format!(
            "{}: {} ({}L, {:.1} GB)",
            strings::get("disc.format"),
            format,
            disc.layers,
            gb
        ),
    );
    emit_encryption_line(&out, &disc);

    // Report the unlocker that actually handled this rip. Keep the matrix as the
    // source of truth, but do not expose the internal capability list here.
    {
        let matched = matched_unlocker(&disc.unlocker_matrix(&drive));
        out.raw(Normal, &format!("Unlocker: {matched}"));
    }

    // Verbose: hardware/disc facts, blank line, then the AACS crypto block. Key
    // resolution runs only here (`-v`): sample ciphertext from the live drive and
    // resolve against the local keydb, so the crypto block shows a real unit-key set.
    if verbose {
        let status = if disc.aacs.is_some() {
            crate::pipe::resolve_info_keys(&mut drive, &disc, &keydb, &out)
        } else {
            None
        };

        // Sanitize SCSI INQUIRY strings: vendor/product/revision come from the
        // drive/bridge firmware (untrusted — a spoofed enclosure could return
        // terminal escapes), so strip control bytes like every other external field.
        out.raw(
            Normal,
            &format!(
                "Drive: {} {} {}",
                sanitize(drive.drive_id.vendor_id.trim()),
                sanitize(drive.drive_id.product_id.trim()),
                sanitize(drive.drive_id.product_revision.trim())
            ),
        );
        out.raw(Normal, &format!("Device: {}", drive.device_path()));
        out.raw(Normal, &format!("Region: {}", region_name(&disc.region)));

        if let Some(ref aacs) = disc.aacs {
            emit_aacs_block(&out, aacs, status.as_ref());
        }
    }

    // Release the drive fd before printing titles
    drive.close();

    out.blank(Normal);

    print_titles(&out, &disc, full, verbose, basic);
}

/// Select the user-facing unlocker label from the registry-derived runtime
/// results. The matrix remains available to diagnostics; this is the compact
/// rendering used by disc info and autorip.
pub(crate) fn matched_unlocker(matrix: &[(&'static str, bool)]) -> &'static str {
    matrix
        .iter()
        .find_map(|(name, ok)| ok.then_some(*name))
        .unwrap_or("none")
}

/// Print a full, localized title list for an already-scanned `Disc` using a
/// fresh `Normal`-level `Output`. This is the entry point for callers that have
/// a scanned disc but no `Output`/verbosity context of their own — notably the
/// `info iso://` path, which scans an ISO **keylessly** (no AACS key needed to
/// list titles) and reuses the exact per-title formatting the drive (`disc://`)
/// path produces: duration, size, clip count, and video/audio/subtitle streams.
///
/// `full` shows every title (otherwise the first 5, with a "+N more" footer).
pub(crate) fn print_disc_titles(disc: &Disc, flags: &InfoFlags) {
    let out = Output::new(flags.verbose, flags.quiet);
    let full = flags.full;
    // iso:// is keyless, but format/MKB generation are read at scan time, so state
    // the encryption generation with the SAME renderer the drive path uses
    // (`emit_encryption_line`) — no duplicated match. Unencrypted discs print no line.
    if emit_encryption_line(&out, disc) {
        // The keydb lookup key, read from the image without any key.
        if let Some(aacs) = disc.aacs.as_ref().filter(|a| !a.disc_hash.is_empty()) {
            out.raw(Normal, &format!("Disc hash: {}", aacs.disc_hash));
        }
        out.blank(Normal);
    }
    print_titles(&out, disc, full, flags.verbose, flags.basic);
}

// Shared renderer for `run` (drive scan) and `print_disc_titles` (ISO scan):
// builds lines via `title_lines` then emits them through `out` at `Normal`.
fn print_titles(out: &Output, disc: &Disc, full: bool, verbose: bool, basic: bool) {
    for line in title_lines(disc, full, verbose, basic) {
        out.raw(Normal, &line);
    }
}

// Pure formatter: builds the localized title-list as lines (empty = blank
// separator), no I/O, so it's unit-testable against a synthetic `Disc`. The
// single source of truth for `disc://` and `iso://` per-title layout.
fn title_lines(disc: &Disc, full: bool, verbose: bool, basic: bool) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();

    if disc.titles.is_empty() {
        lines.push(strings::get("disc.no_titles"));
        return lines;
    }

    lines.push(strings::get("disc.titles"));
    lines.push(String::new());

    let max_titles = if full { disc.titles.len() } else { 5 };

    // Stream rows align to one shared indent, derived from the widest of the three
    // (localized) labels, so layout holds for any locale instead of hardcoding
    // English widths. Labels don't vary per title, so compute once; skipped in `--basic`.
    let indent = if basic {
        0
    } else {
        [
            strings::get("disc.video"),
            strings::get("disc.audio"),
            strings::get("disc.subtitle"),
        ]
        .iter()
        .map(|l| label_indent(l))
        .max()
        .unwrap_or(17)
    };

    for (idx, title) in disc.titles.iter().take(max_titles).enumerate() {
        // Truncate to whole seconds once, then split with integer math — exact
        // and avoids float-precision display artifacts on the h/m breakdown.
        let total_secs = title.duration_secs as u64;
        let hours = total_secs / 3600;
        let mins = (total_secs % 3600) / 60;
        let gb = title.size_bytes as f64 / 1_000_000_000.0; // decimal GB, matches disc-marketed capacity
        // Only a Blu-ray title is made of clips; a DVD or HD DVD title would read "0 clips".
        let clips = match title.clips.len() {
            0 => String::new(),
            1 => format!("  1 {}", strings::get("disc.clip")),
            n => format!("  {n} {}", strings::get("disc.clips")),
        };

        lines.push(format!(
            "  {:2}. {:14}  {:2}h {:02}m  {:>5.1} GB{}",
            idx + 1,
            sanitize(&title.playlist),
            hours,
            mins,
            gb,
            clips
        ));

        if basic {
            continue;
        }

        // Video
        let videos: Vec<&VideoStream> = title
            .streams
            .iter()
            .filter_map(|s| {
                if let Stream::Video(v) = s {
                    Some(v)
                } else {
                    None
                }
            })
            .collect();
        if !videos.is_empty() {
            lines.push(String::new());
            let label = strings::get("disc.video");
            for (vi, v) in videos.iter().enumerate() {
                let line = format_video(v, verbose);
                if vi == 0 {
                    lines.push(format!("{}{}", label_prefix(&label, indent), line));
                } else {
                    lines.push(format!("{:indent$}{}", "", line, indent = indent));
                }
            }
        }

        // Audio
        let audios: Vec<&AudioStream> = title
            .streams
            .iter()
            .filter_map(|s| {
                if let Stream::Audio(a) = s {
                    Some(a)
                } else {
                    None
                }
            })
            .collect();
        if !audios.is_empty() {
            lines.push(String::new());
            let label = strings::get("disc.audio");
            for (ai, a) in audios.iter().enumerate() {
                let line = format_audio(a, verbose);
                if ai == 0 {
                    lines.push(format!("{}{}", label_prefix(&label, indent), line));
                } else {
                    lines.push(format!("{:indent$}{}", "", line, indent = indent));
                }
            }
        }

        // Subtitles
        let subs: Vec<&SubtitleStream> = title
            .streams
            .iter()
            .filter_map(|s| {
                if let Stream::Subtitle(sub) = s {
                    Some(sub)
                } else {
                    None
                }
            })
            .collect();
        if !subs.is_empty() {
            lines.push(String::new());
            let label = strings::get("disc.subtitle");
            for (si, s) in subs.iter().enumerate() {
                let line = format_subtitle(s, verbose);
                if si == 0 {
                    lines.push(format!("{}{}", label_prefix(&label, indent), line));
                } else {
                    lines.push(format!("{:indent$}{}", "", line, indent = indent));
                }
            }
        }

        lines.push(String::new());
    }

    if disc.titles.len() > max_titles {
        lines.push(strings::fmt(
            "disc.more_titles",
            &[("count", &(disc.titles.len() - max_titles).to_string())],
        ));
        lines.push(String::new());
    }

    lines
}

// Column where a stream's value starts: 6-space lead + label + colon + gap.
// Max across Video/Audio/Subtitle labels reproduces the old hardcoded English
// layout (col 17) while staying correct for wider localized labels.
fn label_indent(label: &str) -> usize {
    6 + label.chars().count() + 1 + 2
}

/// First-line prefix for a stream group: 6-space lead, the label, a colon, then
/// padding so the value text begins exactly at `indent`.
fn label_prefix(label: &str, indent: usize) -> String {
    let head = format!("      {}:", label);
    let pad = indent.saturating_sub(head.chars().count());
    format!("{head}{:pad$}", "", pad = pad)
}

// ── Formatting ──────────────────────────────────────────────────────────────

fn format_video(v: &VideoStream, verbose: bool) -> String {
    let mut parts = vec![codec_name(v.codec).to_string(), v.resolution.to_string()];
    if v.frame_rate != libfreemkv::FrameRate::Unknown {
        parts.push(format!("{}fps", v.frame_rate));
    }
    if v.hdr != HdrFormat::Sdr {
        parts.push(hdr_name(v.hdr).to_string());
    }
    if v.color_space == ColorSpace::Bt2020 {
        parts.push("BT.2020".into());
    }
    // A secondary Dolby Vision video stream is the enhancement layer (the
    // library no longer carries the English descriptor — it's localized here).
    if v.secondary && v.hdr == HdrFormat::DolbyVision {
        parts.push(strings::get("disc.dolby_vision_el"));
    } else if v.secondary && !v.label.is_empty() {
        parts.push(sanitize(&v.label));
    }
    if verbose {
        parts.push(format!("[PID 0x{:04X}]", v.pid));
    }
    parts.join(" ")
}

fn format_audio(a: &AudioStream, verbose: bool) -> String {
    let lang = lang_name(&a.language);
    let codec = codec_name(a.codec);
    let mut s = format!("{} {} {}", lang, codec, a.channels);
    if verbose {
        s.push_str(&format!(" {} [PID 0x{:04X}]", a.sample_rate, a.pid));
    }

    // Combine label (codec/variant info from the library) with locale-rendered
    // purpose / secondary tags. Library guarantees no English in `label`.
    let mut tags: Vec<String> = Vec::new();
    if let Some(key) = purpose_key(a.purpose) {
        tags.push(strings::get(key));
    }
    if a.secondary {
        tags.push(strings::get("stream.secondary"));
    }
    if !a.label.is_empty() {
        tags.push(sanitize(&a.label));
    }
    if !tags.is_empty() {
        s.push_str(&format!(" ({})", tags.join(", ")));
    }
    s
}

fn format_subtitle(s: &SubtitleStream, verbose: bool) -> String {
    let lang = lang_name(&s.language);
    let mut tags: Vec<String> = Vec::new();
    if s.forced {
        tags.push(strings::get("disc.forced"));
    }
    if let Some(key) = qualifier_key(s.qualifier) {
        tags.push(strings::get(key));
    }
    let mut line = if tags.is_empty() {
        lang.to_string()
    } else {
        format!("{} ({})", lang, tags.join(", "))
    };
    if verbose {
        line.push_str(&format!(" [PID 0x{:04X}]", s.pid));
    }
    line
}

// AACS generation label ("AACS 1.0"/"2.0"/"2.1"): FMTS is 2.1, UHD is 2.0,
// everything else renders `AACS {aacs.version}.0`, defaulting to 1.0 when no
// `aacs` struct is present.
fn aacs_generation(disc: &Disc) -> String {
    match disc.format {
        DiscFormat::Fmts => "AACS 2.1".to_string(),
        DiscFormat::Uhd => "AACS 2.0".to_string(),
        _ => format!(
            "AACS {}.0",
            disc.aacs.as_ref().map(|a| a.version).unwrap_or(1)
        ),
    }
}

/// Whether the disc format is a known AACS carrier (BD / UHD / FMTS / HD DVD). A
/// DVD or unclassified disc is NOT — so an encrypted-but-unresolved DVD (e.g. a
/// failed CSS crack) is never mislabeled with an AACS generation.
fn is_aacs_format(disc: &Disc) -> bool {
    matches!(
        disc.format,
        DiscFormat::BluRay | DiscFormat::Uhd | DiscFormat::Fmts | DiscFormat::HdDvd
    )
}

/// The encryption-status line to render for a disc.
#[derive(Debug, PartialEq, Eq)]
enum EncLabel {
    /// CSS (DVD) — resolved or a CSS disc whose key crack failed.
    Css,
    /// AACS with a generation label (BD / UHD / FMTS).
    Aacs(String),
    /// Encrypted, but neither CSS nor a known AACS carrier resolved.
    GenericAacs,
}

fn emit_encryption_line(out: &Output, disc: &Disc) -> bool {
    match encryption_label(disc) {
        Some(EncLabel::Css) => out.print(Normal, "disc.css_encrypted"),
        Some(EncLabel::Aacs(label)) => out.raw(Normal, &format!("{label} encrypted")),
        Some(EncLabel::GenericAacs) => out.print(Normal, "disc.aacs_encrypted"),
        None => return false,
    }
    true
}

// `None` for an unencrypted disc. CSS wins whenever any CSS signal is present
// (resolved state OR a recorded css_error from a failed crack) — a
// failed-CSS DVD must never be mislabeled as AACS.
fn encryption_label(disc: &Disc) -> Option<EncLabel> {
    if !disc.encrypted {
        return None;
    }
    if disc.css.is_some() || disc.css_error.is_some() {
        Some(EncLabel::Css)
    } else if disc.aacs.is_some() || is_aacs_format(disc) {
        Some(EncLabel::Aacs(aacs_generation(disc)))
    } else {
        Some(EncLabel::GenericAacs)
    }
}

/// Human-readable region: "Region-free", the Blu-ray region letters (e.g.
/// "A/B/C"), the DVD region numbers (e.g. "1, 2"; "None" when every region is
/// prohibited), or "Unknown" when the disc records none a scan can read.
fn region_name(region: &DiscRegion) -> String {
    match region {
        DiscRegion::Free => "Region-free".to_string(),
        DiscRegion::BluRay(rs) => {
            if rs.is_empty() {
                "Region-free".to_string()
            } else {
                rs.iter()
                    .map(|r| match r {
                        BdRegion::A => "A",
                        BdRegion::B => "B",
                        BdRegion::C => "C",
                    })
                    .collect::<Vec<_>>()
                    .join("/")
            }
        }
        DiscRegion::Dvd(rs) => {
            if rs.is_empty() {
                "None".to_string()
            } else {
                rs.iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
        DiscRegion::Unknown => "Unknown".to_string(),
    }
}

/// The `info -v` AACS crypto block: MKB, disc hash, VID, and the resolved set's source and
/// key count (KU §3.3: `info` reads the set's `status()`); `None` when it refused.
fn emit_aacs_block(
    out: &Output,
    aacs: &libfreemkv::AacsState,
    status: Option<&libfreemkv::keys::KeySetStatus>,
) {
    out.blank(Normal);
    // The crypto block leads with the MKB generation. Bus-encryption
    // isn't surfaced — `Type: Uhd` already signals AACS 2.0.
    out.raw(Normal, &format!("MKB v{}", aacs.mkb_version.unwrap_or(0)));
    out.raw(Normal, &format!("Disc hash: {}", aacs.disc_hash));
    // Volume ID (from the SCSI AACS handshake). Absent on an ISO scan
    // (no handshake) — the 16 bytes stay zero there, so only show it
    // when the disc actually yielded one.
    if aacs.volume_id.iter().any(|&b| b != 0) {
        out.raw(Normal, &format!("VID: 0x{}", hex_bytes(&aacs.volume_id)));
    }
    // Keys: source + count only. Key bytes never render — this output is pasted into
    // public bug reports.
    let origin = status.and_then(|s| s.origin).unwrap_or("none");
    let proven = status.map_or(0, |s| s.proven);
    out.raw(Normal, &format!("Keys: {origin} ({proven} unit keys)"));
    if let Some(note) = status.and_then(crate::rip_keys::best_effort_note) {
        out.raw(Normal, &note);
    }
}

/// Lower-case hex of a byte slice, no separators (for VID / hash-style fields).
fn hex_bytes(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut acc, b| {
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// Map `LabelPurpose` to its locale string key. `Normal` returns None — no tag.
fn purpose_key(p: LabelPurpose) -> Option<&'static str> {
    match p {
        LabelPurpose::Commentary => Some("stream.purpose.commentary"),
        LabelPurpose::Descriptive => Some("stream.purpose.descriptive"),
        LabelPurpose::Score => Some("stream.purpose.score"),
        LabelPurpose::Ime => Some("stream.purpose.ime"),
        LabelPurpose::Normal => None,
    }
}

/// Map `LabelQualifier` to its locale string key. `Forced` is rendered via
/// `disc.forced` from the existing forced flag, so we skip it here.
fn qualifier_key(q: LabelQualifier) -> Option<&'static str> {
    match q {
        LabelQualifier::Sdh => Some("stream.qualifier.sdh"),
        LabelQualifier::DescriptiveService => Some("stream.qualifier.descriptive_service"),
        LabelQualifier::None | LabelQualifier::Forced => None,
    }
}

fn codec_name(c: Codec) -> String {
    match c {
        Codec::Ac3 => "DD".into(),
        Codec::Ac3Plus => "DD+".into(),
        Codec::DvdSub => "DVD Sub".into(),
        Codec::Unknown(ct) => format!("0x{:02x}", ct),
        other => other.name().into(),
    }
}

fn hdr_name(h: HdrFormat) -> &'static str {
    h.name()
}

fn lang_name(code: &str) -> String {
    if code.is_empty() {
        return "?".to_string();
    }
    isolang::Language::from_639_3(code)
        .or_else(|| isolang::Language::from_639_1(code))
        .map(|l| l.to_name().to_string())
        // An unrecognized code falls back to the raw on-disc bytes; sanitize it,
        // as these come from an untrusted MPLS/IFO language field and could carry
        // terminal-escape sequences (same defense as the other printed fields).
        .unwrap_or_else(|| sanitize(code))
}

fn format_volume_id(vol_id: &str) -> String {
    vol_id
        .replace('_', " ")
        .split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(ch) => format!("{}{}", ch.to_uppercase(), c.as_str().to_lowercase()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[path = "disc_info_tests.rs"]
mod tests;
