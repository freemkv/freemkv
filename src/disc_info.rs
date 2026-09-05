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
    std::process::exit(1);
}

pub fn run(device: Option<&str>, args: &[String]) {
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
                "cli.log_level_not_a_number",
                "--log-level: expected a number 1-4, got '{value}', ignored",
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
    let mut session = libfreemkv::DiscSession::open(target, keyspec).unwrap_or_else(|e| {
        match &e {
            // Autodetect with no drive surfaces as an empty-path DeviceNotFound;
            // keep the dedicated "no drive" message. Any other open failure (or a
            // real path that won't open) renders through the E-code humanizer.
            libfreemkv::Error::DeviceNotFound { path } if path.is_empty() => {
                eprintln!("{}", strings::get("error.no_drive"));
            }
            _ => eprintln!("{}", crate::pipe::fmt_err(&e)),
        }
        std::process::exit(1);
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
        std::process::exit(1);
    }
    // Decompose the session into the owned disc + drive the rest of this command
    // already worked with, so downstream rendering is untouched.
    let mut disc = session.take_disc().expect("scan populated the disc");
    // into_drive is fallible: stage_drive_as_reader moves the drive out, so an
    // empty slot is reachable through ordinary API use. Match the local style
    // the session open above uses.
    let mut drive = session.into_drive().unwrap_or_else(|e| {
        eprintln!("{}", crate::pipe::fmt_err(&e));
        std::process::exit(1);
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
    let unknown = strings::get("disc.format_unknown");
    let format = match disc.format {
        DiscFormat::Uhd => "4K UHD",
        DiscFormat::Fmts => "4K UHD (AACS 2.1 FMTS)",
        DiscFormat::BluRay => "Blu-ray",
        DiscFormat::HdDvd => "HD-DVD",
        DiscFormat::Dvd => "DVD",
        DiscFormat::Unknown => &unknown,
    };
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

    // Unlocker matrix: which registered unlockers actually RAN this rip (not just
    // "matched the disc kind"), so a missing one (e.g. firmware-unlock = "no" on a
    // supported drive) is visible. Registry-driven names; rendering matches autorip's.
    {
        let matrix = disc
            .unlocker_matrix(&drive)
            .into_iter()
            .map(|(name, ok)| format!("{name}: {}", if ok { "yes" } else { "no" }))
            .collect::<Vec<_>>()
            .join(", ");
        out.raw(Normal, &format!("Unlockers — {matrix}"));
    }

    // Verbose: hardware/disc facts, blank line, then the AACS crypto block. Key
    // resolution runs only here (`-v`): sample ciphertext from the live drive and
    // resolve against the local keydb, so the crypto block shows a real unit-key set.
    if verbose {
        if disc.aacs.is_some() {
            crate::pipe::resolve_info_keys(&mut drive, &mut disc, &keydb, &out);
        }

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
            out.blank(Normal);
            // Generation label lives on the normal-level encryption line now; the
            // crypto block leads with the MKB generation number.
            out.raw(
                Normal,
                &format!(
                    "MKB v{}{}",
                    aacs.mkb_version.unwrap_or(0),
                    if aacs.bus_encryption {
                        " (bus encryption)"
                    } else {
                        ""
                    }
                ),
            );
            out.raw(Normal, &format!("Disc hash: {}", aacs.disc_hash));
            // Volume ID (from the SCSI AACS handshake). Absent on an ISO scan
            // (no handshake) — the 16 bytes stay zero there, so only show it
            // when the disc actually yielded one.
            if aacs.volume_id.iter().any(|&b| b != 0) {
                out.raw(Normal, &format!("VID: 0x{}", hex_bytes(&aacs.volume_id)));
            }
            // Keys group: source + count, then the Volume Unique Key (when a VUK
            // path resolved it) and each CPS unit key on its own indented line.
            out.raw(
                Normal,
                &format!(
                    "Keys: {} ({} unit keys)",
                    key_origin_label(aacs.key_source),
                    aacs.unit_keys.len()
                ),
            );
            if let Some(vuk) = aacs.vuk {
                out.raw(Normal, &format!("  VUK:   0x{}", hex_bytes(&vuk)));
            }
            for (cps, key) in &aacs.unit_keys {
                out.raw(Normal, &format!("  CPS {cps}: 0x{}", hex_bytes(key)));
            }
        }
    }

    // Release the drive fd before printing titles
    drive.close();

    out.blank(Normal);

    print_titles(&out, &disc, full, verbose, basic);
}

/// Print a full, localized title list for an already-scanned `Disc` using a
/// fresh `Normal`-level `Output`. This is the entry point for callers that have
/// a scanned disc but no `Output`/verbosity context of their own — notably the
/// `info iso://` path, which scans an ISO **keylessly** (no AACS key needed to
/// list titles) and reuses the exact per-title formatting the drive (`disc://`)
/// path produces: duration, size, clip count, and video/audio/subtitle streams.
///
/// `full` shows every title (otherwise the first 5, with a "+N more" footer).
pub fn print_disc_titles(disc: &Disc, flags: &InfoFlags) {
    let out = Output::new(flags.verbose, flags.quiet);
    let full = flags.full;
    // iso:// is keyless, but format/MKB generation are read at scan time, so state
    // the encryption generation with the SAME renderer the drive path uses
    // (`emit_encryption_line`) — no duplicated match. Unencrypted discs print no line.
    if emit_encryption_line(&out, disc) {
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
        let clip_word = if title.clips.len() != 1 {
            strings::get("disc.clips")
        } else {
            strings::get("disc.clip")
        };

        lines.push(format!(
            "  {:2}. {:14}  {:2}h {:02}m  {:>5.1} GB  {} {}",
            idx + 1,
            sanitize(&title.playlist),
            hours,
            mins,
            gb,
            title.clips.len(),
            clip_word
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

// See docs/disc-info.md — emit_encryption_line: one-line "<gen> encrypted"
// label shared by disc:// and iso://; why "encrypted" stays app-layer English.
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
/// "A/B/C"), or the DVD region numbers (e.g. "1, 2").
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
                "Region-free".to_string()
            } else {
                rs.iter()
                    .map(|r| r.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
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

/// Human label for how AACS keys were resolved. The library holds no
/// user-facing English (it exposes the typed `KeyOrigin` enum); this CLI-side
/// map renders it for `disc-info`.
fn key_origin_label(o: libfreemkv::KeyOrigin) -> &'static str {
    match o {
        libfreemkv::KeyOrigin::DeviceKey => "MKB + device key",
        libfreemkv::KeyOrigin::ProcessingKey => "MKB + processing key",
        libfreemkv::KeyOrigin::KeyDbDerived => "KEYDB (derived)",
        libfreemkv::KeyOrigin::KeyDb => "KEYDB",
        libfreemkv::KeyOrigin::KeyDbUnitKeys => "KEYDB (unit keys)",
        libfreemkv::KeyOrigin::ExternalUk => "external UK",
    }
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
mod tests {
    use super::*;
    use libfreemkv::disc::DiscRegion;

    // `info` flag validation is the same for every URL scheme: `disc:// --typo`
    // used to exit 1, but `iso://x.iso --typo` silently listed titles because that
    // route scanned args for `--full` via `.any()`. Both now go through `parse_info_flags`.

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_unknown_info_flag_is_rejected() {
        for bad in ["--fulll", "--typo", "-z", "--verbse", "--no-such-thing"] {
            assert_eq!(
                parse_info_flags(&args(&[bad])),
                InfoParse::Unknown(bad.to_string()),
                "{bad} must be reported, not silently dropped"
            );
        }
    }

    #[test]
    fn an_unknown_flag_is_rejected_even_after_valid_ones() {
        // The offending token must be the one named, not the first flag seen.
        assert_eq!(
            parse_info_flags(&args(&["--full", "--quiet", "--oops"])),
            InfoParse::Unknown("--oops".to_string())
        );
    }

    #[test]
    fn known_info_flags_are_honoured() {
        let InfoParse::Ok(f) = parse_info_flags(&args(&[
            "--full",
            "--quiet",
            "--verbose",
            "--basic",
            "--keydb",
            "/tmp/k.cfg",
        ])) else {
            panic!("a list of valid flags must parse");
        };
        assert!(f.full && f.quiet && f.verbose && f.basic);
        assert_eq!(f.keydb.as_deref(), Some("/tmp/k.cfg"));
        // Short forms too.
        let InfoParse::Ok(f) = parse_info_flags(&args(&["-f", "-q", "-v", "-b"])) else {
            panic!("short forms must parse");
        };
        assert!(f.full && f.quiet && f.verbose && f.basic);
    }

    // Regression: value-taking flags used to consume the following token
    // unconditionally, so `--keydb --full` set the keydb path to "--full"
    // and dropped `--full`. A token starting with `-` is a flag, not a value.
    #[test]
    fn a_value_flag_does_not_swallow_the_flag_that_follows_it() {
        let InfoParse::Ok(f) = parse_info_flags(&args(&["--keydb", "--full"])) else {
            panic!("a missing --keydb value must not turn --full into one");
        };
        assert_eq!(f.keydb, None, "--full is a flag, not a keydb path");
        assert!(f.full, "--full was the user's request and must still apply");

        // Same for the two logging flags, whose values are consumed here only
        // so they are not mistaken for positionals.
        let InfoParse::Ok(f) = parse_info_flags(&args(&["--log-level", "--basic"])) else {
            panic!("a missing --log-level value must not eat --basic");
        };
        assert!(f.basic);
        let InfoParse::Ok(f) = parse_info_flags(&args(&["--log-file", "--quiet"])) else {
            panic!("a missing --log-file value must not eat --quiet");
        };
        assert!(f.quiet);

        // A real value is still a value — including a keydb path that merely
        // sits next to a flag.
        let InfoParse::Ok(f) = parse_info_flags(&args(&["--keydb", "/tmp/k.cfg", "--full"])) else {
            panic!("a well-formed list must still parse");
        };
        assert_eq!(f.keydb.as_deref(), Some("/tmp/k.cfg"));
        assert!(f.full);
    }

    #[test]
    fn flag_values_are_not_mistaken_for_unknown_options() {
        // `--log-level 3` / `--log-file p.txt` are consumed by logging init; the
        // VALUE must be skipped, or it lands in the unknown-option branch.
        let InfoParse::Ok(f) = parse_info_flags(&args(&[
            "--log-level",
            "3",
            "--log-file",
            "p.txt",
            "--full",
        ])) else {
            panic!("logging flags and their values must be consumed");
        };
        assert!(f.full);
        assert!(f.verbose, "--log-level 3 widens stdout detail");
    }

    #[test]
    fn help_is_reported_rather_than_printed_by_the_parser() {
        assert_eq!(parse_info_flags(&args(&["--help"])), InfoParse::Help);
        assert_eq!(parse_info_flags(&args(&["-h"])), InfoParse::Help);
    }

    #[test]
    fn no_flags_is_all_defaults() {
        assert_eq!(parse_info_flags(&[]), InfoParse::Ok(Box::default()));
    }

    use libfreemkv::{
        AudioChannels, ColorSpace, ContentFormat, DiscFormat, DiscTitle, FrameRate, HdrFormat,
        LabelPurpose, LabelQualifier, Resolution, SampleRate,
    };

    // A minimal synthetic encrypted disc with one rich title, mirroring a
    // keyless ISO scan: titles populated, no AACS key resolved.
    fn synthetic_disc() -> Disc {
        let video = Stream::Video(VideoStream {
            pid: 0x1011,
            codec: Codec::Hevc,
            resolution: Resolution::Unknown,
            frame_rate: FrameRate::Unknown,
            hdr: HdrFormat::Sdr,
            color_space: ColorSpace::Bt709,
            display_aspect: None,
            secondary: false,
            label: String::new(),
            measured_cicp: None,
        });
        let audio = Stream::Audio(AudioStream {
            pid: 0x1100,
            codec: Codec::TrueHd,
            channels: AudioChannels::Unknown,
            language: "eng".to_string(),
            sample_rate: SampleRate::Unknown,
            secondary: false,
            purpose: LabelPurpose::Normal,
            label: String::new(),
        });
        let subtitle = Stream::Subtitle(SubtitleStream {
            pid: 0x1200,
            codec: Codec::Pgs,
            language: "eng".to_string(),
            forced: false,
            qualifier: LabelQualifier::None,
            codec_data: None,
        });

        let title = DiscTitle {
            playlist: "00800.mpls".to_string(),
            playlist_id: 800,
            duration_secs: 7530.0, // 2h 05m
            size_bytes: 50 * 1024 * 1024 * 1024,
            clips: Vec::new(),
            streams: vec![video, audio, subtitle],
            chapters: Vec::new(),
            extents: Vec::new(),
            content_format: ContentFormat::BdTs,
            codec_privates: Vec::new(),
        };

        Disc {
            volume_id: "TEST_DISC".to_string(),
            meta_title: None,
            format: DiscFormat::Uhd,
            capacity_sectors: 0,
            capacity_bytes: 0,
            layers: 1,
            titles: vec![title],
            region: DiscRegion::Free,
            aacs: None, // no key resolved — exactly the `info iso://` keyless case
            css: None,
            encrypted: true,
            aacs_error: None,
            css_error: None,
            content_format: ContentFormat::BdTs,
        }
    }

    #[test]
    fn encryption_label_never_mislabels_a_failed_css_dvd_as_aacs() {
        let mut disc = synthetic_disc();
        // A CSS DVD whose title-key crack failed: encrypted, no css state, a
        // css_error recorded, format DVD, no aacs. Must render CSS, NOT AACS.
        disc.format = DiscFormat::Dvd;
        disc.encrypted = true;
        disc.css = None;
        disc.aacs = None;
        disc.css_error = Some(libfreemkv::Error::MkvInvalid); // any css-path error
        assert_eq!(encryption_label(&disc), Some(EncLabel::Css));

        // A resolved CSS DVD → CSS.
        disc.css_error = None;
        // (css stays None in the fixture; a resolved DVD would set css=Some, but
        // the css_error path above is the regression case. Verify the AACS paths.)

        // A UHD carrier → AACS 2.0.
        let mut uhd = synthetic_disc();
        uhd.format = DiscFormat::Uhd;
        uhd.encrypted = true;
        uhd.css = None;
        uhd.css_error = None;
        assert_eq!(
            encryption_label(&uhd),
            Some(EncLabel::Aacs("AACS 2.0".to_string()))
        );

        // An FMTS carrier → AACS 2.1.
        uhd.format = DiscFormat::Fmts;
        assert_eq!(
            encryption_label(&uhd),
            Some(EncLabel::Aacs("AACS 2.1".to_string()))
        );

        // Unencrypted → no line.
        uhd.encrypted = false;
        assert_eq!(encryption_label(&uhd), None);

        // The case `is_aacs_format` exists for: encrypted, no CSS signal, no `aacs`
        // struct, not an AACS carrier. That's generic unresolved encryption, not
        // "AACS 1.0" — labelling it AACS would claim detection of something specific.
        let mut unknown = synthetic_disc();
        unknown.format = DiscFormat::Dvd;
        unknown.encrypted = true;
        unknown.css = None;
        unknown.css_error = None;
        unknown.aacs = None;
        assert_eq!(encryption_label(&unknown), Some(EncLabel::GenericAacs));

        // Same disc on a real AACS carrier IS an AACS label — so the
        // distinction is the format, not the missing `aacs` struct.
        unknown.format = DiscFormat::BluRay;
        assert_eq!(
            encryption_label(&unknown),
            Some(EncLabel::Aacs("AACS 1.0".to_string()))
        );

        // And every carrier format is recognised as one.
        for f in [
            DiscFormat::BluRay,
            DiscFormat::Uhd,
            DiscFormat::Fmts,
            DiscFormat::HdDvd,
        ] {
            let mut d = synthetic_disc();
            d.format = f;
            assert!(super::is_aacs_format(&d), "{f:?} is an AACS carrier");
        }
        let mut dvd = synthetic_disc();
        dvd.format = DiscFormat::Dvd;
        assert!(!super::is_aacs_format(&dvd), "a DVD is not an AACS carrier");
    }

    #[test]
    fn sanitize_strips_terminal_escape_sequences() {
        // Untrusted on-disc strings (title, volume label, stream label, language)
        // must have control/escape bytes stripped before printing, so a crafted
        // disc cannot inject terminal escapes (color/cursor/OSC).
        let hostile = "Ti\x1b[2Jtle\x07\x1b]0;pwn\x1b\\";
        let clean = sanitize(hostile);
        assert!(
            !clean.contains('\x1b') && !clean.contains('\x07'),
            "control/escape chars stripped, got {clean:?}"
        );
        assert_eq!(clean, "Ti[2Jtle]0;pwn\\", "printable text preserved");
        // The lang_name fallback for an unrecognized code sanitizes too.
        assert!(!lang_name("\x1b[31mzz").contains('\x1b'));
        // Unicode format (Cf) chars — bidi override, zero-width, BOM — are
        // stripped too (char::is_control does NOT catch these).
        let bidi = "abc\u{202E}gnp\u{200B}\u{FEFF}xyz";
        let cleaned = sanitize(bidi);
        assert_eq!(cleaned, "abcgnpxyz", "bidi/zero-width/BOM stripped");
    }

    #[test]
    fn title_lines_lists_encrypted_disc_without_key() {
        // The bug: `info iso://<encrypted>` returned E7022 and listed no titles
        // because it went through the key-gated `input()`. The keyless title
        // list must render the title with its streams and never emit E7022.
        let disc = synthetic_disc();
        let lines = title_lines(&disc, false, false, false);
        let joined = lines.join("\n");

        assert!(
            !joined.contains("E7022"),
            "title list must not surface the no-key error, got:\n{joined}"
        );
        // The title row (playlist + duration) is present.
        assert!(
            joined.contains("00800.mpls"),
            "expected the title's playlist, got:\n{joined}"
        );
        assert!(
            joined.contains("2h 05m"),
            "expected the formatted duration, got:\n{joined}"
        );
        // Stream rows are present (rich per-title output, not just the row).
        assert!(
            joined.contains("HEVC"),
            "expected the video codec, got:\n{joined}"
        );
        assert!(
            joined.contains("English"),
            "expected the audio/subtitle language, got:\n{joined}"
        );
    }

    #[test]
    fn scan_failed_substitutes_detail_and_drops_no_placeholder() {
        // Regression: the handler keyed the format arg "error" while `error.scan_failed`
        // uses `{detail}`, so the real cause was dropped and users saw the literal
        // `{detail}`. Must key "detail" and route the cause through `fmt_err`.
        let rendered = strings::fmt(
            "error.scan_failed",
            &[(
                "detail",
                &crate::pipe::fmt_err(&libfreemkv::Error::DeviceNotReady {
                    path: "/dev/sr0".to_string(),
                }),
            )],
        );
        assert!(
            !rendered.contains("{detail}"),
            "placeholder must be substituted, got:\n{rendered}"
        );
        // WS2: the cause routes through `fmt_err`, which now PREFIXES the
        // language-neutral `E<code>` token (code-forward) ahead of the
        // localized message — the code is shown, not stripped.
        assert!(
            rendered.contains("E1002"),
            "expected the code-forward E1002 token, got:\n{rendered}"
        );
        assert!(
            rendered.starts_with("Scan failed:") && rendered.len() > "Scan failed:".len() + 1,
            "expected the cause appended after the prefix, got:\n{rendered}"
        );
    }

    #[test]
    fn open_failure_renders_through_fmt_err_shows_code() {
        // Regression: the open-failure handler did `eprintln!("{}", e)`, printing
        // libfreemkv's raw `E####: <data>` and bypassing the i18n renderer. Must route
        // through `pipe::fmt_err`, which localizes and shows `E<code>` as a code-forward prefix.
        let rendered = crate::pipe::fmt_err(&libfreemkv::Error::DevicePermission {
            path: "/dev/sg0".to_string(),
        });
        assert!(
            rendered.starts_with("E1001 "),
            "expected the code-forward E1001 prefix, got:\n{rendered}"
        );
        assert!(
            rendered.contains("/dev/sg0"),
            "expected the device path in the localized message, got:\n{rendered}"
        );
        // The localized E1001 text names the actionable fix (disk group / privileges).
        assert!(
            rendered.to_lowercase().contains("disk group")
                || rendered.to_lowercase().contains("privile"),
            "expected the actionable remediation text, got:\n{rendered}"
        );
    }

    #[test]
    fn title_lines_empty_disc_reports_no_titles() {
        let mut disc = synthetic_disc();
        disc.titles.clear();
        let lines = title_lines(&disc, true, false, false);
        assert_eq!(lines, vec![strings::get("disc.no_titles")]);
    }

    #[test]
    fn title_lines_basic_omits_streams() {
        // `--basic` shows only the title row, no stream detail.
        let disc = synthetic_disc();
        let joined = title_lines(&disc, false, false, true).join("\n");
        assert!(joined.contains("00800.mpls"));
        assert!(
            !joined.contains("HEVC"),
            "basic mode must omit stream rows, got:\n{joined}"
        );
    }

    #[test]
    fn label_alignment_preserves_english_layout() {
        // The historical English layout put every stream value at column 17
        // (`Subtitle` is the widest label at 8 chars: 6 + 8 + 1 + 2 = 17). The
        // derived indent must reproduce that exactly so nothing shifts.
        assert_eq!(label_indent("Subtitle"), 17);
        assert_eq!(label_indent("Video"), 14);
        assert_eq!(label_indent("Audio"), 14);

        // First-line prefixes pad to the shared (max) indent of 17, matching the
        // old hardcoded `      Video:     ` / `      Subtitle:  ` strings.
        assert_eq!(label_prefix("Video", 17), "      Video:     ");
        assert_eq!(label_prefix("Subtitle", 17), "      Subtitle:  ");
    }

    #[test]
    fn label_alignment_holds_for_longer_localized_label() {
        // A longer localized subtitle label (German `Untertitel`, Italian
        // `Sottotitoli`) must drive a wider shared indent instead of overrunning
        // a hardcoded 17-space continuation. The value column tracks the label.
        let indent = label_indent("Sottotitoli"); // 6 + 11 + 1 + 2 = 20
        assert_eq!(indent, 20);
        let prefix = label_prefix("Sottotitoli", indent);
        assert_eq!(prefix.chars().count(), indent);
        assert!(prefix.starts_with("      Sottotitoli:"));
        assert!(prefix.ends_with("  "));
    }

    // Pure formatters (665-806): every `disc-info` line is one of these functions'
    // return value. Pure string builders, but nothing exercised their branches, so a
    // mutant swapping "DD+" for "DD" or dropping `[PID …]` passed CI. Locale pinned here.

    fn video(codec: Codec, resolution: Resolution) -> VideoStream {
        VideoStream {
            pid: 0x1011,
            codec,
            resolution,
            frame_rate: FrameRate::Unknown,
            hdr: HdrFormat::Sdr,
            color_space: ColorSpace::Bt709,
            display_aspect: None,
            secondary: false,
            label: String::new(),
            measured_cicp: None,
        }
    }

    fn audio(codec: Codec, channels: AudioChannels, language: &str) -> AudioStream {
        AudioStream {
            pid: 0x1100,
            codec,
            channels,
            language: language.to_string(),
            sample_rate: SampleRate::Unknown,
            secondary: false,
            purpose: LabelPurpose::Normal,
            label: String::new(),
        }
    }

    fn subtitle(language: &str) -> SubtitleStream {
        SubtitleStream {
            pid: 0x1200,
            codec: Codec::Pgs,
            language: language.to_string(),
            forced: false,
            qualifier: LabelQualifier::None,
            codec_data: None,
        }
    }

    #[test]
    fn format_video_assembles_codec_resolution_and_only_the_flags_present() {
        strings::set_locale("en");
        // Bare: codec + resolution, no fps (Unknown), no HDR (SDR), no BT.2020.
        let v = video(Codec::Hevc, Resolution::R1080p);
        assert_eq!(format_video(&v, false), "HEVC 1080p");
        // Verbose appends the PID, upper-case hex, zero-padded to four digits.
        assert_eq!(format_video(&v, true), "HEVC 1080p [PID 0x1011]");

        // Frame rate, HDR name and BT.2020 each add exactly one part when set.
        let mut rich = video(Codec::Hevc, Resolution::R2160p);
        rich.frame_rate = FrameRate::F23_976;
        rich.hdr = HdrFormat::DolbyVision;
        rich.color_space = ColorSpace::Bt2020;
        assert_eq!(
            format_video(&rich, false),
            "HEVC 2160p 23.976fps Dolby Vision BT.2020"
        );

        // A secondary Dolby Vision stream is the enhancement layer (localized).
        let mut el = video(Codec::Hevc, Resolution::R2160p);
        el.secondary = true;
        el.hdr = HdrFormat::DolbyVision;
        assert_eq!(
            format_video(&el, false),
            "HEVC 2160p Dolby Vision Dolby Vision EL"
        );

        // A secondary non-DV stream with a label shows the sanitized label.
        let mut labelled = video(Codec::H264, Resolution::R1080p);
        labelled.secondary = true;
        labelled.label = "PiP\x1b[2J".to_string();
        let out = format_video(&labelled, false);
        assert!(out.contains("PiP") && !out.contains('\x1b'), "got {out}");
    }

    #[test]
    fn format_audio_renders_lang_codec_channels_and_tags() {
        strings::set_locale("en");
        let a = audio(Codec::TrueHd, AudioChannels::Surround51, "eng");
        assert_eq!(format_audio(&a, false), "English TrueHD 5.1");
        // Verbose inserts sample rate and PID before the tags.
        assert_eq!(
            format_audio(&a, true),
            "English TrueHD 5.1 unknown [PID 0x1100]"
        );

        // Purpose + secondary + label collect into one parenthesised group.
        let mut tagged = audio(Codec::Ac3, AudioChannels::Stereo, "fra");
        tagged.purpose = LabelPurpose::Commentary;
        tagged.secondary = true;
        tagged.label = "Director".to_string();
        assert_eq!(
            format_audio(&tagged, false),
            "French DD stereo (Commentary, Secondary, Director)"
        );
    }

    #[test]
    fn format_subtitle_renders_lang_forced_and_qualifier() {
        strings::set_locale("en");
        assert_eq!(format_subtitle(&subtitle("eng"), false), "English");
        assert_eq!(
            format_subtitle(&subtitle("eng"), true),
            "English [PID 0x1200]"
        );

        let mut forced = subtitle("jpn");
        forced.forced = true;
        forced.qualifier = LabelQualifier::Sdh;
        assert_eq!(format_subtitle(&forced, false), "Japanese (forced, SDH)");
    }

    #[test]
    fn region_name_covers_free_bluray_and_dvd() {
        assert_eq!(region_name(&DiscRegion::Free), "Region-free");
        // Empty region lists on either carrier read as region-free, not "".
        assert_eq!(region_name(&DiscRegion::BluRay(vec![])), "Region-free");
        assert_eq!(region_name(&DiscRegion::Dvd(vec![])), "Region-free");
        assert_eq!(
            region_name(&DiscRegion::BluRay(vec![
                BdRegion::A,
                BdRegion::B,
                BdRegion::C
            ])),
            "A/B/C"
        );
        assert_eq!(region_name(&DiscRegion::Dvd(vec![1, 2])), "1, 2");
    }

    #[test]
    fn hex_bytes_is_lower_case_two_digit_no_separator() {
        assert_eq!(hex_bytes(&[]), "");
        assert_eq!(hex_bytes(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn codec_name_maps_the_special_cases_and_falls_through_to_name() {
        assert_eq!(codec_name(Codec::Ac3), "DD");
        assert_eq!(codec_name(Codec::Ac3Plus), "DD+");
        assert_eq!(codec_name(Codec::DvdSub), "DVD Sub");
        assert_eq!(codec_name(Codec::Unknown(0x1b)), "0x1b");
        // Everything else defers to the library's own display name.
        assert_eq!(codec_name(Codec::Hevc), "HEVC");
        assert_eq!(codec_name(Codec::TrueHd), "TrueHD");
        assert_eq!(codec_name(Codec::Pgs), "PGS");
    }

    #[test]
    fn key_origin_label_names_every_source() {
        use libfreemkv::KeyOrigin;
        assert_eq!(key_origin_label(KeyOrigin::DeviceKey), "MKB + device key");
        assert_eq!(
            key_origin_label(KeyOrigin::ProcessingKey),
            "MKB + processing key"
        );
        assert_eq!(key_origin_label(KeyOrigin::KeyDbDerived), "KEYDB (derived)");
        assert_eq!(key_origin_label(KeyOrigin::KeyDb), "KEYDB");
        assert_eq!(
            key_origin_label(KeyOrigin::KeyDbUnitKeys),
            "KEYDB (unit keys)"
        );
        assert_eq!(key_origin_label(KeyOrigin::ExternalUk), "external UK");
    }

    #[test]
    fn aacs_generation_labels_the_carrier_and_falls_back_to_the_minor() {
        let mut disc = synthetic_disc();
        disc.format = DiscFormat::Fmts;
        assert_eq!(aacs_generation(&disc), "AACS 2.1");
        disc.format = DiscFormat::Uhd;
        assert_eq!(aacs_generation(&disc), "AACS 2.0");
        // Non-carrier with no aacs struct falls back to 1.0.
        disc.format = DiscFormat::BluRay;
        disc.aacs = None;
        assert_eq!(aacs_generation(&disc), "AACS 1.0");
    }

    #[test]
    fn lang_name_resolves_codes_sanitizes_unknowns_and_marks_empty() {
        assert_eq!(lang_name("eng"), "English");
        assert_eq!(lang_name("en"), "English"); // 639-1 fallback
        assert_eq!(lang_name(""), "?");
        // An unrecognized code is returned raw, but sanitized of escapes.
        let out = lang_name("\x1b[31mzz");
        assert!(!out.contains('\x1b'), "got {out}");
    }

    #[test]
    fn format_volume_id_title_cases_underscored_labels() {
        assert_eq!(format_volume_id("THE_MOVIE_2020"), "The Movie 2020");
        assert_eq!(format_volume_id("bluray_disc"), "Bluray Disc");
        assert_eq!(format_volume_id(""), "");
    }

    #[test]
    fn title_lines_truncates_to_five_and_footers_the_remainder() {
        // A disc with more titles than the non-`--full` cap (5) lists exactly
        // five and closes with the localized "+N more" footer. `synthetic_disc`
        // has one title, so this many-title footer path had no coverage.
        strings::set_locale("en");
        let mut disc = synthetic_disc();
        let one = disc.titles[0].clone();
        disc.titles = std::iter::repeat_n(one, 8).collect();
        let joined = title_lines(&disc, false, false, false).join("\n");
        // Five rows shown (1..=5), the sixth is not.
        assert!(joined.contains("  1. "), "first title shown: {joined}");
        assert!(joined.contains("  5. "), "fifth title shown: {joined}");
        assert!(!joined.contains("  6. "), "sixth title truncated: {joined}");
        // The footer names the 3 remaining.
        assert!(
            joined.contains('3') && joined.to_lowercase().contains("more"),
            "expected the +N more footer, got: {joined}"
        );
        // `--full` shows every title and prints no footer.
        let full = title_lines(&disc, true, false, false).join("\n");
        assert!(full.contains("  8. "), "full lists the eighth: {full}");
    }

    #[test]
    fn title_lines_aligns_continuation_rows_for_multi_stream_groups() {
        // A title with two of each stream kind exercises the `vi/ai/si > 0`
        // continuation-line arms (the indented rows with no label prefix), which
        // the single-stream `synthetic_disc` never reaches.
        strings::set_locale("en");
        let mut disc = synthetic_disc();
        let title = &mut disc.titles[0];
        title.streams = vec![
            Stream::Video(video(Codec::Hevc, Resolution::R2160p)),
            Stream::Video(video(Codec::H264, Resolution::R1080p)),
            Stream::Audio(audio(Codec::TrueHd, AudioChannels::Surround71, "eng")),
            Stream::Audio(audio(Codec::Ac3, AudioChannels::Stereo, "fra")),
            Stream::Subtitle(subtitle("eng")),
            Stream::Subtitle(subtitle("jpn")),
        ];
        let joined = title_lines(&disc, true, false, false).join("\n");
        // Both members of each group render; the second of each is a
        // continuation row (present, distinct language/codec).
        assert!(
            joined.contains("HEVC") && joined.contains("H.264"),
            "{joined}"
        );
        assert!(
            joined.contains("English") && joined.contains("French"),
            "{joined}"
        );
        assert!(joined.contains("Japanese"), "{joined}");
    }
}
