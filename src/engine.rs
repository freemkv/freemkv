//! Bridge to `freemkv-engine`. Everything the UI knows about discs, keys and
//! rips comes through here — no engine types leak into the AppKit shell.

use freemkv_engine as fe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One row of the title tree, already formatted for display.
#[derive(Debug, Clone)]
pub struct Row {
    pub type_s: String,
    pub desc: String,
    pub depth: u8,
    pub checkable: bool,
    /// Index of the owning title, for selection bookkeeping.
    pub title: usize,
    pub info: String,
    /// Transport PID for audio/subtitle rows — what `StreamSelection` filters
    /// on. `None` for video (always kept) and for non-stream rows.
    pub pid: Option<u16>,
    /// Title duration in seconds — populated for Title rows (depth 1) so the UI
    /// can honor "Longest title" default selection and the minimum-length
    /// filter. `0.0` for non-title rows (File/disc header, stream rows).
    pub duration_secs: f64,
    /// The stream's language tag exactly as the disc carries it (`"deu"`,
    /// `"eng"`, `""` when untagged). Empty for every non-stream row.
    ///
    /// Carried as DATA rather than left to be scraped back out of `desc`: the
    /// preferred-language defaults hand these to the engine's own language
    /// matcher, and a display string is a formatting decision that must stay
    /// free to change.
    pub lang: String,
    /// Whether a subtitle row is flagged FORCED. Read straight from the flag
    /// libfreemkv put on the stream (including its PGS forced probe) — never
    /// re-derived here. Always `false` for audio and non-stream rows.
    pub forced: bool,
}

/// What the shell needs after a scan. Pure data — no engine types.
#[derive(Debug, Clone)]
pub struct Scanned {
    pub label: String,
    /// The volume id exactly as the disc carries it — NOT display text.
    ///
    /// `label` above is the sanitised, "(no label)"-defaulted form the log
    /// pane and the disc row show. The output filename is built from the raw
    /// id instead (`title_basename` → `sanitize_label`), so the two must be
    /// carried separately or the GUI cannot name the file the rip will write.
    /// Empty for a container source, and for a disc with no volume id.
    pub volume_id: String,
    pub rows: Vec<Row>,
    pub key_summary: String,
    pub title_count: usize,
    /// Video codec name per title, indexed by canonical title index. Lets the
    /// UI say "MP4 cannot hold MPEG-2" BEFORE a rip instead of surfacing a
    /// bare E9048 after one.
    pub video_codecs: Vec<String>,
    /// The `freemkv info -v` detail block (format, capacity, region, MKB
    /// version, disc hash, VID, key state, title list) — shown in the log on
    /// open so the desktop app surfaces the same disc facts the CLI does.
    pub details: Vec<String>,
    /// What each title NUMBER refers to on THIS scan, indexed by canonical
    /// title index (the same shape `video_codecs` uses).
    ///
    /// The user ticks titles against this scan and the rip re-scans before it
    /// muxes them, so the numbers alone do not prove the rip is about to read
    /// the titles that were picked. Carried from here into `RipRequest` so the
    /// engine can check the scan it takes against the scan the operator saw —
    /// the one window `verify_title_identity` could not cover, because it
    /// begins before the engine is called at all.
    pub title_ids: Vec<TitleIdentity>,
}

fn fmt_dur(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
}

/// Rows for one title's streams. Shared by the disc and stream-source paths
/// so an MKV shows the same track detail a disc title does.
/// Human label for an audio track's purpose, matching the CLI's vocabulary.
fn purpose_label(p: libfreemkv::LabelPurpose) -> Option<&'static str> {
    match p {
        libfreemkv::LabelPurpose::Commentary => Some("Commentary"),
        libfreemkv::LabelPurpose::Descriptive => Some("Descriptive"),
        libfreemkv::LabelPurpose::Score => Some("Score"),
        libfreemkv::LabelPurpose::Ime => Some("IME"),
        libfreemkv::LabelPurpose::Normal => None,
    }
}

/// Rows for one title's streams. Shared by the disc and stream-source paths
/// so an MKV shows the same track detail a disc title does.
///
/// Uses the `Display` impls (`HEVC`, `5.1`, `2160p`) rather than `Debug`
/// (`Hevc`, `Surround51`, `R2160p`), and carries the `label` / `purpose` /
/// `secondary` fields the CLI shows — those are what make a commentary track
/// distinguishable from the feature audio.
fn stream_rows(t: &libfreemkv::DiscTitle, ti: usize) -> Vec<Row> {
    t.streams
        .iter()
        .map(|st| {
            let (ty, pid, desc, info) = match st {
                libfreemkv::Stream::Video(v) => {
                    // Stream labels are disc bytes, same as the volume id.
                    let label = if v.label.is_empty() {
                        String::new()
                    } else {
                        format!("  —  {}", sanitize_display(&v.label))
                    };
                    (
                        "Video",
                        None,
                        format!("{}  {}{}", v.codec, v.resolution, label),
                        format!(
                            "Video track\n\nCodec: {}\nResolution: {}\nFrame rate: {}\nHDR: {}\nColour: {}{}",
                            v.codec,
                            v.resolution,
                            v.frame_rate,
                            v.hdr,
                            v.color_space,
                            if v.label.is_empty() {
                                String::new()
                            } else {
                                format!("\nLabel: {}", sanitize_display(&v.label))
                            }
                        ),
                    )
                }
                libfreemkv::Stream::Audio(a) => {
                    // The language code is disc bytes too — an MPLS/IFO field,
                    // not a validated ISO 639-2 code — so it gets the same
                    // treatment as the label two lines below it.
                    let language = sanitize_display(&a.language);
                    let mut tags: Vec<String> = Vec::new();
                    if let Some(p) = purpose_label(a.purpose) {
                        tags.push(p.to_string());
                    }
                    if a.secondary {
                        tags.push("Secondary".into());
                    }
                    if !a.label.is_empty() {
                        tags.push(sanitize_display(&a.label));
                    }
                    let suffix = if tags.is_empty() {
                        String::new()
                    } else {
                        format!("  —  {}", tags.join(", "))
                    };
                    (
                        "Audio",
                        Some(a.pid),
                        format!("{}  {}  {}{}", a.codec, a.channels, language, suffix),
                        format!(
                            "Audio track\n\nCodec: {}\nChannels: {}\nLanguage: {}\nSample rate: {}{}{}",
                            a.codec,
                            a.channels,
                            language,
                            a.sample_rate,
                            if a.secondary { "\nSecondary: yes" } else { "" },
                            if tags.is_empty() {
                                String::new()
                            } else {
                                format!("\nLabel: {}", tags.join(", "))
                            }
                        ),
                    )
                }
                libfreemkv::Stream::Subtitle(s) => {
                    // Disc bytes, same as the audio language above.
                    let language = sanitize_display(&s.language);
                    let mut tags: Vec<String> = Vec::new();
                    if s.forced {
                        tags.push("Forced".into());
                    }
                    let suffix = if tags.is_empty() {
                        String::new()
                    } else {
                        format!("  —  {}", tags.join(", "))
                    };
                    (
                        "Subtitles",
                        Some(s.pid),
                        format!("{}  {}{}", s.codec, language, suffix),
                        format!(
                            "Subtitle track\n\nCodec: {}\nLanguage: {}\nForced: {}",
                            s.codec, language, s.forced
                        ),
                    )
                }
            };
            // Language / forcedness as DATA, read from the same stream the
            // display strings above were formatted from. The preferred-language
            // defaults feed these to the engine's matcher, so they must be the
            // disc's own tags, not a re-parse of `desc`.
            let (lang, forced) = match st {
                libfreemkv::Stream::Video(_) => (String::new(), false),
                libfreemkv::Stream::Audio(a) => (a.language.clone(), false),
                libfreemkv::Stream::Subtitle(s) => (s.language.clone(), s.forced),
            };
            Row {
                type_s: ty.into(),
                desc,
                depth: 2,
                checkable: ty != "Video",
                title: ti,
                info,
                pid,
                duration_secs: 0.0,
                lang,
                forced,
            }
        })
        .collect()
}

/// Scan a stream source (`.mkv`, `.m2ts`) — a single title, but its tracks are
/// real and worth showing. `Stream::info()` carries the parsed `DiscTitle`.
pub fn scan_stream(path: &str) -> Result<Scanned, String> {
    let url = {
        let ext = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let scheme = match ext.as_str() {
            "mkv" => "mkv",
            "mp4" => "mp4",
            _ => "m2ts",
        };
        format!("{scheme}://{path}")
    };
    let opts = libfreemkv::InputOptions::default();
    let stream = libfreemkv::input(&url, &opts).map_err(|e| format!("{e}"))?;
    let t = stream.info();

    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("stream")
        .to_string();

    let mut rows = vec![Row {
        type_s: "File".into(),
        desc: name.clone(),
        depth: 0,
        checkable: false,
        title: usize::MAX,
        info: format!(
            "File\n\nName: {}\nTracks: {}\nDuration: {}",
            name,
            t.streams.len(),
            fmt_dur(t.duration_secs)
        ),
        pid: None,
        duration_secs: 0.0,
        lang: String::new(),
        forced: false,
    }];
    rows.push(Row {
        type_s: "Title".into(),
        desc: format!(
            "{} track(s) , {}",
            t.streams.len(),
            fmt_dur(t.duration_secs)
        ),
        depth: 1,
        checkable: true,
        title: 0,
        info: format!(
            "Title information\n\nTracks: {}\nDuration: {}\nChapters: {}",
            t.streams.len(),
            fmt_dur(t.duration_secs),
            t.chapters.len()
        ),
        pid: None,
        duration_secs: t.duration_secs,
        lang: String::new(),
        forced: false,
    });
    rows.extend(stream_rows(t, 0));

    let details = vec![
        format!("File: {name}"),
        format!("Duration: {}", fmt_dur(t.duration_secs)),
        format!("Streams: {}", t.streams.len()),
    ];
    Ok(Scanned {
        label: name,
        // A container source has no volume id; `run_stream` names its output
        // from the file's own stem instead.
        volume_id: String::new(),
        title_count: 1,
        key_summary: "unencrypted".into(),
        video_codecs: vec![
            t.video_streams()
                .next()
                .map(|v| v.codec.to_string())
                .unwrap_or_default(),
        ],
        // A container is ONE title and it is the file itself; there is no
        // number to carry across a re-scan, but the shape stays the same as
        // the disc scan's so the request never has to special-case it.
        title_ids: vec![TitleIdentity::of(t)],
        rows,
        details,
    })
}

/// Scan a source (ISO path today) and flatten it into display rows.
pub fn scan(path: &str) -> Result<Scanned, String> {
    scan_with_keys(path, &KeyConfig::default(), false)
}

/// Scan, then consult the key sources so the key strip reflects a real
/// resolution rather than the scan-time placeholder. `verbose` mirrors the
/// CLI's `info -v`: the logged detail block gains the resolved keys.
pub fn scan_with_keys(path: &str, keys: &KeyConfig, verbose: bool) -> Result<Scanned, String> {
    // A FOLDER is an image-level source too — `scan_dir` synthesizes a UDF
    // volume over an extracted disc tree and returns the same (Disc, reader)
    // pair `scan_iso` does, so everything downstream is identical.
    //
    // Without this the desktop shells could not open a folder at all: the
    // macOS "Open Folder" command showed a picker and then reported the result
    // unsupported, and dragging a folder onto either window was refused. That
    // is 1.6.1 shipping a headline feature the GUI declines.
    let p = std::path::Path::new(path);
    let scan = if p.is_dir() {
        libfreemkv::scan_dir
    } else {
        libfreemkv::scan_iso
    };
    let (mut disc, mut reader) = scan(p, libfreemkv::ScanOptions::default())
        .map_err(|e| format!("E{} scan failed", e.code()))?;

    let won = resolve_disc_keys(&mut disc, reader.as_mut(), keys);
    let summary = key_summary(&disc, won.as_deref());
    Ok(scanned_from_disc(&disc, summary, verbose))
}

/// Build the title tree + info rows from a scanned `Disc`. Shared by the ISO
/// (`scan_with_keys`) and live-drive (`scan_disc_with_keys`) paths so a disc
/// looks identical whether it came from a file or a physical drive.
/// The `freemkv info -v` detail block for a scanned disc/ISO — the same facts
/// the CLI prints (format, capacity, region, MKB version, disc hash, VID, key
/// state, title list), as log lines the desktop app shows on open.
fn disc_details(disc: &libfreemkv::Disc, key_summary: &str, verbose: bool) -> Vec<String> {
    let mut d = Vec::new();
    d.push(format!("Type: {:?}", disc.format));
    if disc.capacity_bytes > 0 {
        let gb = disc.capacity_bytes as f64 / 1_000_000_000.0;
        d.push(format!("Capacity: {gb:.1} GB, {} layer(s)", disc.layers));
    }
    match &disc.region {
        libfreemkv::disc::DiscRegion::Free => d.push("Region: free".to_string()),
        libfreemkv::disc::DiscRegion::BluRay(rs) if !rs.is_empty() => {
            let names: Vec<String> = rs.iter().map(|r| format!("{r:?}")).collect();
            d.push(format!("Region: Blu-ray {}", names.join("/")));
        }
        libfreemkv::disc::DiscRegion::Dvd(ns) if !ns.is_empty() => {
            let names: Vec<String> = ns.iter().map(|n| n.to_string()).collect();
            d.push(format!("Region: DVD {}", names.join(",")));
        }
        _ => {}
    }
    if let Some(aacs) = &disc.aacs {
        d.push(format!(
            "MKB v{}{}",
            aacs.mkb_version.unwrap_or(0),
            if aacs.bus_encryption {
                " (bus encryption)"
            } else {
                ""
            }
        ));
        d.push(format!("Disc hash: {}", aacs.disc_hash));
        if aacs.volume_id.iter().any(|&b| b != 0) {
            let vid: String = aacs.volume_id.iter().map(|b| format!("{b:02x}")).collect();
            d.push(format!("VID: 0x{vid}"));
        }
    }
    d.push(format!("Protection: {key_summary}"));
    // Verbose (Log detail: Verbose) reveals the resolved keys, like `info -v`:
    // the Volume Unique Key and each CPS unit key.
    if verbose && let Some(aacs) = &disc.aacs {
        if let Some(vuk) = aacs.vuk {
            let h: String = vuk.iter().map(|b| format!("{b:02x}")).collect();
            d.push(format!("  VUK: 0x{h}"));
        }
        for (cps, key) in &aacs.unit_keys {
            let h: String = key.iter().map(|b| format!("{b:02x}")).collect();
            d.push(format!("  CPS {cps}: 0x{h}"));
        }
    }
    // Just the count — the per-title list lives in the UI tree, no need to
    // duplicate it in the log.
    d.push(format!("Titles: {}", disc.titles.len()));
    d
}

fn scanned_from_disc(disc: &libfreemkv::Disc, summary: String, verbose: bool) -> Scanned {
    let mut rows = Vec::new();
    // The volume id is disc bytes — untrusted. It becomes `Scanned.label`,
    // which the GUI pushes into the log pane, and the disc row's text. Left
    // raw, a label carrying newlines forges log lines ("[Result] Rip finished
    // successfully") and one carrying bidi overrides scrambles the pane. The
    // CLI has always sanitised this (`disc_info` prints it through the same
    // helper); the GUI never did. Sanitise ONCE here, at the boundary where
    // disc bytes become UI text, rather than at each of `say()`'s ~15 call
    // sites.
    let label = {
        let cleaned = sanitize_display(&disc.volume_id);
        if cleaned.trim().is_empty() {
            "(no label)".to_string()
        } else {
            cleaned
        }
    };

    rows.push(Row {
        type_s: format!("{:?} disc", disc.format),
        desc: label.clone(),
        depth: 0,
        checkable: false,
        title: usize::MAX,
        info: format!(
            "Disc information\n\nType: {:?}\nLabel: {}\nProtection: {}\nTitles: {}",
            disc.format,
            label,
            summary,
            disc.titles.len()
        ),
        pid: None,
        duration_secs: 0.0,
        lang: String::new(),
        forced: false,
    });

    for (ti, t) in disc.titles.iter().enumerate() {
        rows.push(Row {
            type_s: "Title".into(),
            // Numbered 1-based and named after the playlist, exactly as
            // `freemkv info` lists them — so a title here and a `-t N` on the
            // command line refer to the same thing. Discs legitimately carry
            // duplicate playlists with identical duration/size; the number and
            // name are what tell them apart.
            desc: format!(
                "{}.  {}{} chapter(s) , {} , {}",
                ti + 1,
                if t.playlist.is_empty() {
                    String::new()
                } else {
                    // The playlist name is a filename read off the disc, so it
                    // is untrusted display bytes like the volume id and the
                    // stream labels.
                    format!("{}   ", sanitize_display(&t.playlist))
                },
                t.chapters.len(),
                fmt_dur(t.duration_secs),
                fmt_gb(t.size_bytes)
            ),
            depth: 1,
            checkable: true,
            title: ti,
            info: format!(
                "Title information\n\nIndex: {}\nDuration: {}\nChapters: {}\nSize: {}\nStreams: {}",
                ti + 1,
                fmt_dur(t.duration_secs),
                t.chapters.len(),
                fmt_gb(t.size_bytes),
                t.streams.len()
            ),
            pid: None,
            duration_secs: t.duration_secs,
            lang: String::new(),
            forced: false,
        });
        rows.extend(stream_rows(t, ti));
    }

    let details = disc_details(disc, &summary, verbose);
    Scanned {
        label,
        volume_id: disc.volume_id.clone(),
        title_count: disc.titles.len(),
        key_summary: summary,
        video_codecs: disc
            .titles
            .iter()
            .map(|t| {
                t.video_streams()
                    .next()
                    .map(|v| v.codec.to_string())
                    .unwrap_or_default()
            })
            .collect(),
        title_ids: disc.titles.iter().map(TitleIdentity::of).collect(),
        rows,
        details,
    }
}

// ── live optical drive (disc://) ────────────────────────────────────────────

/// An optical drive the GUI can rip from.
#[derive(Debug, Clone)]
pub struct OpticalDrive {
    /// Platform device path (`/dev/diskN` on macOS) — becomes `disc://<path>`.
    pub device: String,
    /// Human label ("HL-DT-ST BD-RE BU40N") for the picker.
    pub label: String,
}

/// Enumerate the optical drives attached to the machine. Empty if none. This is
/// registry/enumeration only — no exclusive access or disc I/O.
pub fn list_optical_drives() -> Vec<OpticalDrive> {
    libfreemkv::list_drives()
        .into_iter()
        .map(|d| {
            let label = format!("{} {}", d.vendor.trim(), d.model.trim())
                .trim()
                .to_string();
            OpticalDrive {
                device: d.path,
                label: if label.is_empty() {
                    "Optical drive".to_string()
                } else {
                    label
                },
            }
        })
        .collect()
}

/// True for a `disc://` live-drive source.
pub fn is_disc_source(source: &str) -> bool {
    source.starts_with("disc://")
}

/// The device path from a `disc://<device>` source, or `None` for bare
/// `disc://` (autodetect the first drive with media).
fn disc_device(source: &str) -> Option<String> {
    let dev = source.strip_prefix("disc://").unwrap_or("");
    (!dev.is_empty()).then(|| dev.to_string())
}

/// `DeviceTarget` for a `disc://` source: an explicit path, or autodetect.
fn disc_target(source: &str) -> libfreemkv::DeviceTarget {
    match disc_device(source) {
        Some(p) => libfreemkv::DeviceTarget::Path(p.into()),
        None => libfreemkv::DeviceTarget::Autodetect,
    }
}

/// Eject the disc in `device` — the GUI analogue of autorip's `eject_drive`:
/// reopen the drive by its resolved device path and eject, surfacing a failure
/// to the log rather than silently doing nothing (the "auto_eject is on but the
/// disc stayed put, no idea why" symptom autorip hit). Called once, after the
/// rip is done reading the drive, when the `auto_eject` setting is on.
fn eject_disc(device: &str, sink: &UiSink) {
    use freemkv_engine::Sink as _;
    match libfreemkv::Drive::open(std::path::Path::new(device)) {
        Ok(mut drive) => match drive.eject() {
            Ok(()) => sink.log(fe::Level::Info, &format!("ejected {device}")),
            Err(e) => sink.log(fe::Level::Warn, &format!("eject failed: {e}")),
        },
        Err(e) => sink.log(
            fe::Level::Warn,
            &format!("eject skipped — drive open failed: {e}"),
        ),
    }
}

/// Host certs / credentials for the AACS bus handshake, from the keydb — the
/// same input the CLI's `drive_credentials` builds. Passed to the shared
/// `fe::open_scan_resolve`.
fn session_credentials(keys: &KeyConfig) -> Option<libfreemkv::DriveCredentials> {
    let host_certs = freemkv_keysources::KeydbSource::new(keys.keydb_path.clone()).host_certs();
    (!host_certs.is_empty()).then_some(libfreemkv::DriveCredentials { host_certs })
}

/// Normalize the GUI's settings-derived [`KeyConfig`] into the engine's
/// `KeyParams`, preserving the GUI's `shellexpand` of the configured keydb
/// path and its EXPLICIT `online_only` toggle — both stay GUI-boundary
/// concerns; the engine only sees the already-resolved result. An empty
/// `keydb_path` / `keyserver_url` setting (the GUI's "not configured"
/// sentinel) maps to `None`, dropping that source entirely — never a
/// default-location fallback (that's the CLI's policy, not the GUI's).
fn key_params(keys: &KeyConfig) -> freemkv_engine::KeyParams {
    let keydb_path = (!keys.keydb_path.trim().is_empty())
        .then(|| crate::settings::shellexpand(&keys.keydb_path));
    // Gated on the dropdown, not just on "is a URL configured": "Local keydb
    // only" must drop the online source even when a URL is saved.
    let key_url = (!keys.local_only && !keys.keyserver_url.trim().is_empty())
        .then(|| keys.keyserver_url.clone());
    freemkv_engine::KeyParams {
        keydb_path,
        key_url,
        key_auth: Some(keys.keyserver_token.clone()),
        online_only: keys.online_only,
    }
}

/// The key-source factory used to resolve a disc's AACS keys (same sources the
/// ISO path uses, built from the user's settings).
fn key_factory(keys: &KeyConfig) -> libfreemkv::KeySourceFactory {
    freemkv_engine::key_source_factory(&key_params(keys))
}

/// Which key source won, from a resolution trace (for the key strip).
fn won_from_trace(trace: &libfreemkv::aacs::trace::ResolutionTrace) -> Option<String> {
    freemkv_engine::won_source(trace)
}

/// Scan a live optical drive (`disc://<device>` or bare `disc://` autodetect)
/// and resolve its keys, returning the SAME `Scanned` shape the ISO path does —
/// so the title tree renders identically. Mirrors the CLI's `pipe_disc` scan.
/// NEEDS HARDWARE to exercise; the session flow matches the proven CLI path.
pub fn scan_disc_with_keys(
    source: &str,
    keys: &KeyConfig,
    verbose: bool,
) -> Result<Scanned, String> {
    // The shared drive bring-up (open + lock + scan + resolve) — the SAME core
    // the CLI's pipe_disc uses; the GUI just renders the result.
    let (session, trace) = fe::open_scan_resolve(
        disc_target(source),
        session_credentials(keys),
        key_factory(keys),
    )
    .map_err(|e| match &e {
        libfreemkv::Error::DeviceNotFound { path } if path.is_empty() => {
            "No optical drive found. Connect a Blu-ray/DVD drive with a disc.".to_string()
        }
        _ => format!("{e}"),
    })?;
    let won = won_from_trace(&trace);
    let disc = session.disc().ok_or("scan produced no disc")?;
    let summary = key_summary(disc, won.as_deref());
    Ok(scanned_from_disc(disc, summary, verbose))
}

/// Ask the engine whether a job can run, without executing it.
pub fn preflight(path: &str, dest: &str, titles: &[usize]) -> Result<Vec<String>, String> {
    preflight_with_keys(path, dest, titles, &KeyConfig::default())
}

/// Preflight against the user's configured key sources.
///
/// The keys must be resolved BEFORE asking. A fresh scan leaves only a VID-only
/// placeholder AACS state, which the engine correctly reports as unresolved —
/// so preflighting an unscanned-for-keys disc always answers "no key", even
/// when the user has a keydb that would unlock it. Resolving first is what
/// makes the answer reflect reality; the decrypt judgment itself stays in the
/// engine.
pub fn preflight_with_keys(
    path: &str,
    dest: &str,
    titles: &[usize],
    keys: &KeyConfig,
) -> Result<Vec<String>, String> {
    // Folder OR image — the third place this dispatch was needed. A preflight
    // that cannot open a folder reports a spurious failure for a source the
    // rip itself handles.
    let p = std::path::Path::new(path);
    let scan = if p.is_dir() {
        libfreemkv::scan_dir
    } else {
        libfreemkv::scan_iso
    };
    let (mut disc, mut reader) =
        scan(p, libfreemkv::ScanOptions::default()).map_err(|e| format!("E{}", e.code()))?;
    resolve_disc_keys(&mut disc, reader.as_mut(), keys);
    let disc = disc;
    let sel = if titles.is_empty() {
        fe::Selection::MainMovie
    } else {
        fe::Selection::Titles(titles.to_vec())
    };
    // Folder OR image — the scan above already dispatches on it, and a
    // preflight run against `iso://<folder>` answers about a source that does
    // not exist in that form.
    let job =
        fe::Job::new(format!("{}://{path}", image_or_dir_scheme(path)), dest).with_selection(sel);
    // No decrypt gate of our own: the engine's `preflight` delegates that to
    // `resolve_keys`, so a second check here could only drift from it.
    match fe::preflight(&disc, &job) {
        fe::Preflight::Ready => Ok(vec![]),
        fe::Preflight::Blocked(rs) => Ok(rs.iter().map(|r| r.key.to_string()).collect()),
    }
}

/// Progress snapshot handed to the UI thread. Engine-derived; never recomputed.
#[derive(Default, Clone, Copy)]
pub struct Prog {
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub speed_bps: u64,
    /// None until the engine's estimate converges.
    pub eta_secs: Option<u64>,
    pub sectors_bad: u64,
}

/// What a finished run actually was.
///
/// The GUI used to decide this by substring-matching the engine's English
/// summary (`starts_with("Cancelled")`, `contains("failed")`). Three separate
/// messages already defeated it — an undecryptable disc and both
/// abort-for-loss paths all rendered as SUCCESS — and any reworded message
/// would have defeated it again. The verdict is now carried, not parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RunOutcome {
    /// The run produced what it set out to produce.
    #[default]
    Completed,
    /// The user stopped it. Resumable, not a failure.
    Cancelled,
    /// It did not produce the deliverable — no key, a hard title failure, or
    /// recovery aborted because the loss exceeded tolerance.
    Failed,
}

/// Shared state a running rip publishes to the UI.
#[derive(Default)]
pub struct RunState {
    pub prog: Mutex<Prog>,
    pub lines: Mutex<Vec<String>>,
    pub cancel: AtomicBool,
    pub finished: AtomicBool,
    /// Titles fully written so far — drives the overall bar.
    pub titles_done: std::sync::atomic::AtomicUsize,
    pub summary: Mutex<String>,
    /// The typed verdict for [`Self::summary`]. Written in the same place the
    /// summary is, so the two can never disagree.
    pub outcome: Mutex<RunOutcome>,
}

impl RunState {
    /// The run's verdict, RECOVERING from a poisoned lock.
    ///
    /// `outcome.lock().map(|o| *o).unwrap_or_default()` looks harmless and is
    /// not: `RunOutcome`'s `#[default]` is `Completed`, so a worker that
    /// PANICKED — which is precisely when the verdict matters — poisoned the
    /// mutex and the UI rendered "Finished" over it. That is the exact defect
    /// this enum's own doc says it exists to prevent: the verdict is carried
    /// rather than parsed BECAUSE both abort-for-loss paths once rendered as
    /// success.
    ///
    /// A poisoned mutex still holds the last value written to it, and that
    /// value is the truth we want. Recover and read it, matching the
    /// poison-recovering convention used across this ecosystem.
    /// The same poison-recovery is applied to every `lines` lock in the crate.
    /// A worker that panicked mid-run poisons the log buffer too, and an
    /// `unwrap()` there turns one dead thread into a second panic on the next
    /// line written — losing the diagnostic that would have explained the
    /// first. That claim used to be false: the CANCELLED arm of
    /// `run_container` and both drain loops in `main.rs` still used `unwrap()`,
    /// and the poison pin below covered `outcome` and `summary` ONLY, so
    /// nothing enforced the sentence you are reading.
    /// `every_lines_lock_recovers_from_poison` now reads both files and fails
    /// on any `lines` lock that does not recover.
    pub fn outcome_now(&self) -> RunOutcome {
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The run's summary line, recovering from a poisoned lock for the same
    /// reason as [`Self::outcome_now`]: `unwrap_or_default()` here renders an
    /// EMPTY result line, discarding what the worker had already written.
    pub fn summary_now(&self) -> String {
        self.summary
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// How long a quit waits for a cancelled rip to put its output down.
///
/// A cancel is observed at the worker's next frame/sector boundary, which on a
/// healthy rip is milliseconds; the cap is what keeps a wedged drive (a read
/// inside an uninterruptible SCSI timeout) from turning "quit" into "hang".
/// Expiring is no worse than the behaviour this replaces — the process leaves
/// anyway — so the only thing the bound can cost is the wait it was going to
/// lose regardless.
pub const QUIT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Wait (up to `grace`) for a cancelled worker to finish. `true` if it did.
///
/// `Cmd::Cancel` only SIGNALS: it flips `RunState::cancel` and returns, and the
/// worker notices at its next boundary, unwinds the mux and drops the sink —
/// which is what actually closes and finalises the partial file. A quit that
/// does not wait for that lets AppKit `exit()` the process mid-write, so the
/// file on disk is whatever the OS write cursor happened to reach rather than
/// the deliberate "cancelled — partial output kept" artefact the GUI reports.
///
/// Polls `finished` rather than joining the thread: the worker publishes state
/// through `RunState` and nothing hands the UI a `JoinHandle`. `finished` is
/// set by the worker's own drop guard, after the mux has returned and its sink
/// has been dropped, so it is exactly the "output is on disk and closed" edge
/// this needs. Safe to call from the main thread with no borrows held: the
/// worker touches only atomics and mutexes on `RunState` and never calls back
/// into the UI.
pub fn await_worker_exit(run: &RunState, grace: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if run.finished.load(Ordering::SeqCst) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        // Short enough that a normal cancel is imperceptible, long enough not
        // to spin a core while a drive finishes a read.
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

struct UiSink(Arc<RunState>);

/// The GUI core's ONLY `Sink`, so every line and every progress frame the
/// library emits reaches the user through these two methods.
///
/// Both recover a poisoned lock rather than skipping the write. `if let Ok(..)`
/// reads like defensive coding and is the opposite: a poisoned `lines` mutex
/// means a worker panicked, which is precisely when the log is the only record
/// of what happened — and the silent `else` branch threw away every subsequent
/// line, so the run went quiet at the exact moment it became interesting.
/// Dropping progress frames is milder but the same mistake: the bar freezes
/// mid-rip with no explanation. A poisoned mutex still holds its last value and
/// the data behind it is a plain `Vec`/`Prog` with no invariant a panic could
/// have broken halfway, so recovering is safe as well as correct.
impl fe::Sink for UiSink {
    fn log(&self, _level: fe::Level, msg: &str) {
        self.0
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(msg.to_string());
    }
    fn progress(&self, p: &fe::Progress) {
        *self.0.prog.lock().unwrap_or_else(|e| e.into_inner()) = Prog {
            bytes_done: p.bytes_done,
            bytes_total: p.bytes_total,
            speed_bps: p.speed_bps,
            eta_secs: p.eta_secs,
            sectors_bad: p.sectors_bad,
        };
    }
    fn should_cancel(&self) -> bool {
        self.0.cancel.load(Ordering::Relaxed)
    }
}

/// Resolve AACS keys onto a scanned disc. Without this the mux fails E7022 on
/// every encrypted title — scanning alone does not consult any key source.
/// Returns the label of the source that actually produced the key
/// (`"keydb"` / `"online"`), or `None` when nothing resolved.
pub fn resolve_disc_keys(
    disc: &mut libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    keys: &KeyConfig,
) -> Option<String> {
    // The trace is the ONLY authoritative record of which source won.
    // `Disc::aacs.key_source` is `ExternalUk` for every caller-supplied key —
    // and is also the scan-time placeholder — so it cannot answer this.
    freemkv_engine::resolve_disc_keys(disc, reader, &key_params(keys))
}

/// Describe the disc's key state honestly.
///
/// The engine's `resolve_keys` reports `resolved: true` / `"resolved-online"`
/// off `KeyOrigin::ExternalUk` alone — but `scan_aacs_vid_only` stamps exactly
/// that origin as a PLACEHOLDER before any source is consulted, with
/// `unit_keys` empty. So an unkeyed disc claims to be resolved and to have
/// come from the network. Gate on real key material instead.
pub(crate) fn key_summary(disc: &libfreemkv::Disc, won: Option<&str>) -> String {
    if !disc.encrypted {
        return "unencrypted".into();
    }
    if disc.css.is_some() {
        return "CSS (DVD)".into();
    }
    let have_key = disc
        .aacs
        .as_ref()
        .map(|a| !a.unit_keys.is_empty() || a.vuk.is_some())
        .unwrap_or(false);
    match (have_key, won) {
        (true, Some(w)) => format!("unlocked via {w}"),
        (true, None) => "unlocked".into(),
        (false, _) => "locked — no key yet".into(),
    }
}

/// Recover the library's numeric error code from a muxed `std::io::Error`.
///
/// The library's `Display` is `E<code>` OR `E<code>: <data>` (it carries zero
/// English by design), so parse the leading digit run rather than the whole
/// string. libfreemkv has `io_error_code` internally but does not export it —
/// when it does, delete this and call it.
///
/// Regression: this used to be `trim_start_matches('E').parse()`, which parses
/// only the bare form. Every code that carries data — `E7022: <disc hash>`,
/// `E8005: <keydb path>`, `E6000: <sector> <sense>`, `E6014: <pid>` — returned
/// 0, so `explain(0)` produced "Mux failed (E0)." for the single most common
/// real failure (an AACS disc with no key), and the dedicated message for that
/// code was unreachable from the desktop app. The CLI was unaffected: it uses
/// `pipe::parse_error_code`, which already parses the digit run.
pub fn error_code(e: &std::io::Error) -> u16 {
    let s = e.to_string();
    let Some(rest) = s.strip_prefix('E') else {
        return 0;
    };
    let digits_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..digits_end].parse().unwrap_or(0)
}

/// Turn a library error code into something a person can act on.
///
/// The library carries no English by design, so a front-end that just prints
/// `E9048` has told the user nothing. Only codes a user can actually do
/// something about are spelled out; the rest keep the code for a bug report.
pub fn explain(code: u16) -> String {
    match code {
        // Must match `error.E9048` in freemkv-i18n and `ui::MP4_VIDEO`: the mux
        // gate admits H.264 and HEVC only. This used to also claim AV1, so the
        // message could list the user's own failing codec as supported. E9048
        // covers both causes — an unmappable codec AND a title with no video
        // track at all — so the wording names what MP4 needs, not what is wrong.
        9048 => "MP4 needs a video track it can carry (H.264 or HEVC); this \
                 title has none. Choose MKV, which keeps everything."
            .to_string(),
        7022 | 8005 => "No decryption key for this disc. Check the keydb or \
                        online key service in Settings."
            .to_string(),
        // 7028/7029/7030 are NOT 7022. The online key service failed to ANSWER —
        // nothing is known about this disc's key — and each failure is a
        // different thing for a person to do. Folding them into the 7022 message
        // above is the bug: a seven-hour run of HTTP 502s told operators the disc
        // was not in the key database and sent them hunting for a VUK.
        7028 => "The online key service could not be reached, so it never said \
                 whether this disc has a key. Wait a few minutes and try again."
            .to_string(),
        7029 => "The online key service rejected the access token. Fix the key \
                 service token in Settings and try again."
            .to_string(),
        7030 => "The online key service is rate-limiting requests. Wait a few \
                 minutes and try again, or rip fewer discs at once."
            .to_string(),
        6013 => "This file is not a disc image.".to_string(),
        c => format!("Mux failed (E{c})."),
    }
}

/// Explain why a title died, from the two halves of the cause
/// [`fe::RipOutcome::Failed`] carries.
///
/// A typed library failure stringifies as `E<code>: …`, so it arrives with a
/// code and [`explain`] handles it. A genuine OS error is passed through
/// unwrapped by libfreemkv — the full disk has NO `E<code>` to parse, arrives
/// as `code: None`, and keeps its whole meaning in `kind`. Reading only the
/// code rendered "Mux failed (E0)." for it.
fn describe_failure(code: Option<u16>, kind: std::io::ErrorKind) -> String {
    if let Some(c) = code {
        return explain(c);
    }
    match kind {
        std::io::ErrorKind::StorageFull => {
            "The destination drive is full. Free some space and run it again.".to_string()
        }
        std::io::ErrorKind::PermissionDenied => {
            "No permission to write to the destination folder.".to_string()
        }
        std::io::ErrorKind::NotFound => "The destination folder is no longer there.".to_string(),
        k => format!("Write failed ({k:?})."),
    }
}

/// Render a finished title loop as the run summary.
///
/// `Err` is not decoration: `start_rip` files an `Ok` string as the summary and
/// nothing else, so an outcome returned as `Ok` reads to the user as a rip that
/// worked. Only [`fe::RipOutcome::Ok`] and a cancel are successes; `NoKey` and
/// `Failed` are not, and a `Failed` that arrives after some titles already
/// wrote must still say it failed — reporting "2 title(s) written" for a rip
/// the engine stopped on a full disk is the same silent-success bug the
/// `code`/`kind` pair was added to make impossible.
///
/// Pure so the mapping is testable without a disc; both title-loop call sites
/// (staged-ISO and single-pass) share it so they cannot drift apart.
pub fn summarize_outcome(
    outcome: &fe::RipOutcome,
    written: usize,
    partial: usize,
    total: usize,
    dest_dir: &str,
) -> Result<String, String> {
    // Never report "Nothing was written" while a partial file is on disk.
    let with_partial = |lead: String| {
        if partial > 0 {
            format!("{lead} — {partial} partial file(s) kept in {dest_dir}")
        } else {
            lead
        }
    };
    // A failure after N good titles has to keep the N; the user needs to know
    // what survived as well as what stopped it.
    let so_far = || {
        if written > 0 {
            format!("{written} of {total} title(s) written to {dest_dir}, then ")
        } else {
            String::new()
        }
    };
    match outcome {
        fe::RipOutcome::Halted => Ok(with_partial(format!(
            "Cancelled — {written} of {total} title(s) completed"
        ))),
        fe::RipOutcome::NoKey => Err(with_partial(format!(
            "{}the disc has no decryption key — every remaining title would \
             fail the same way. Check the keydb or online key service in Settings.",
            so_far()
        ))),
        fe::RipOutcome::Failed {
            title_index,
            code,
            kind,
        } => Err(with_partial(format!(
            "{}title {} failed: {}",
            so_far(),
            title_index + 1,
            describe_failure(*code, *kind)
        ))),
        fe::RipOutcome::Ok { .. } if written == 0 && partial > 0 => {
            Ok(with_partial("Cancelled".to_string()))
        }
        fe::RipOutcome::Ok { .. } if written == 0 => Ok("Nothing was written".to_string()),
        fe::RipOutcome::Ok { .. } => Ok(format!("{written} title(s) written to {dest_dir}")),
    }
}

/// Render a finished single-file/container mux (`run_stream`'s `mkv://` /
/// `m2ts://` / `mp4://` source path) as the run summary.
///
/// The `mkv://`/`m2ts://` analogue of `summarize_outcome`'s `o.completed`
/// check in the disc/ISO title loop: `completed` is `libfreemkv::MuxOutcome`'s
/// own signal for "drained to a natural EOF and finalised cleanly" (`false`
/// on an interrupt/halt). `run_stream` used to discard the whole
/// `MuxOutcome` via a bare `?`, so a Cancel that landed mid-conversion left a
/// truncated file on disk while still reporting "Written to <dir>" — the same
/// silent-success-on-cancel shape the title loop was already hardened
/// against via `o.completed`, just never applied to this path.
///
/// `completed` is NOT the whole verdict, and neither is `undelivered_streams`.
/// It takes the OUTCOME rather than two of its fields for exactly that reason:
/// grading on a hand-picked subset is how `errors`/`lost_bytes` — the bytes an
/// `mkv://` → `mkv://` re-mux of a 3D rip drops, with no whole stream missing —
/// came to be read nowhere at all, and a 3 MB hole rendered as a clean
/// "Written to <dir>". [`crate::lossy::is_lossy`] is the single question, asked
/// here and by the CLI. The header is appended, not substituted: the file IS
/// written, it is simply not everything the user asked for.
///
/// Pure so the mapping is unit-testable without driving a real mux.
pub fn summarize_stream(outcome: &libfreemkv::MuxOutcome, target: &str, dest_dir: &str) -> String {
    if !outcome.completed {
        return format!("Cancelled — partial output kept: {target}");
    }
    if !crate::lossy::is_lossy(outcome) {
        return format!("Written to {dest_dir}");
    }
    let n = outcome.undelivered_streams.len();
    if n > 0 {
        format!(
            "Written to {dest_dir} — {}",
            crate::strings::fmt("mp4.excluded_header", &[("count", &n.to_string())])
        )
    } else {
        // Bytes lost inside the tracks: no stream is missing, so the
        // excluded-tracks wording would be wrong. Name the loss itself.
        format!(
            "Written to {dest_dir} —{}",
            crate::lossy::lossy_lines(outcome, target).join(" ")
        )
    }
}

/// The lines a GUI run must add when a COMPLETED mux did not deliver
/// everything — the front-end twin of the CLI's `pipe::print_lossy_outcome`,
/// and now literally the same renderer ([`crate::lossy::lossy_lines`]).
///
/// It used to be a second, narrower one: this file reported
/// `undelivered_streams` and nothing else, exactly as the CLI's copy did, so
/// the loss NEITHER of them reported (`errors`/`lost_bytes` — a Blu-ray 3D
/// dependent view dropped by an `mkv://` → `mkv://` re-mux) reached no user
/// through either shell. Two half-answers to one question is the shape this
/// crate keeps having to un-write; there is one answer now.
///
/// A warning on a still-successful rip, not a failure: the file is finalised,
/// structurally valid and playable, and the per-title failure arm below DELETES
/// the output file — escalating would destroy the bytes this is warning about.
///
/// Returns a `Vec` rather than pushing, so the formatting is testable without a
/// `RipState`: reaching the real call sites needs a live drive or a real disc
/// image.
pub fn lossy_lines(outcome: &libfreemkv::MuxOutcome, target: &str) -> Vec<String> {
    crate::lossy::lossy_lines(outcome, target)
}

/// Render a finished image decrypt (`iso://` -> `iso://`, the drive-free
/// `OutKind::IsoImage` path) as the run summary — the third sibling of
/// [`summarize_stream`] and [`summarize_extract`].
///
/// This path reported "Decrypted image written" whenever ANY bytes were
/// recovered, consulting neither `halted` nor `bytes_unreadable`. Cancel a
/// decrypt halfway and it claimed success; decrypt an image with unreadable
/// sectors and it said nothing about them. `CopyResult` carries both signals
/// precisely so a call site cannot re-derive completion and get it wrong —
/// see its doc, which names "reporting a lossy or cancelled rip as complete"
/// as the bug it exists to prevent.
///
/// Branches on `halted` FIRST, like `summarize_extract`, and then on
/// `complete` — never on a re-derivation of it. `complete` is "nothing pending
/// AND nothing lost AND not interrupted", so `halted` has to be taken off the
/// table before it is asked, or "you cancelled" and "the disc is damaged"
/// collapse into one message; but with `halted` already handled, `complete` is
/// exactly the question left. Reading `bytes_unreadable > 0` in its place was
/// the re-derivation this type exists to prevent, and it dropped the third
/// term: a decrypt that ends with bytes still PENDING (attempted-and-skipped
/// sectors, `recovery::copy`'s terminal-result paths) is not complete, and
/// reported as a clean write. Both shortfalls are now named in the message.
/// The partial image is KEPT in every case, matching the disc->ISO path's
/// stated policy that an abort never throws away the read.
///
/// Pure so the mapping is unit-testable without running a decrypt.
pub fn summarize_image_decrypt(result: &fe::CopyResult, dest: &std::path::Path) -> String {
    let gib = result.bytes_good as f64 / 1_073_741_824.0;
    let mib = |b: u64| b as f64 / 1_048_576.0;
    if result.halted {
        return format!(
            "Cancelled — partial image kept: {} ({:.2} GiB recovered)",
            dest.display(),
            gib
        );
    }
    if result.complete {
        return format!(
            "Decrypted image written: {} ({:.2} GiB)",
            dest.display(),
            gib
        );
    }
    let mut shortfall: Vec<String> = Vec::new();
    if result.bytes_unreadable > 0 {
        shortfall.push(format!(
            "{:.1} MiB unreadable",
            mib(result.bytes_unreadable)
        ));
    }
    if result.bytes_pending > 0 {
        shortfall.push(format!("{:.1} MiB not read", mib(result.bytes_pending)));
    }
    // `complete` was false with neither byte count set — the flag is the
    // authority, so say the image is incomplete rather than print a clean
    // success line we cannot justify.
    if shortfall.is_empty() {
        shortfall.push("incomplete".to_string());
    }
    format!(
        "Decrypted image written: {} ({:.2} GiB, {})",
        dest.display(),
        gib,
        shortfall.join(", ")
    )
}

/// Render a finished decrypted-folder extraction (`run_extract_folder`) as the
/// run summary — the `dir://` analogue of `summarize_outcome`.
///
/// `res.halted` is exactly the signal `pipe.rs`'s `extract_succeeded`
/// (`!halted && complete`) already gates the CLI's exit code on for this same
/// `libfreemkv::ExtractResult`; this GUI path never consulted it, so a Cancel
/// mid-extraction (a real in-progress halt, not a partial-then-retried run)
/// still reported "Decrypted file tree written to <dir> — N file(s)" as if
/// the whole tree had landed.
///
/// Pure so the mapping is unit-testable without a real disc/extraction.
pub fn summarize_extract(res: &libfreemkv::ExtractResult, dest: &std::path::Path) -> String {
    let n = res.files.len();
    if res.halted {
        format!(
            "Cancelled — {n} file(s) extracted to {} before stopping",
            dest.display()
        )
    } else if res.bytes_unreadable > 0 {
        format!(
            "Decrypted file tree written to {} — {} file(s), {:.1} MB unreadable",
            dest.display(),
            n,
            res.bytes_unreadable as f64 / 1_048_576.0
        )
    } else {
        format!(
            "Decrypted file tree written to {} — {} file(s)",
            dest.display(),
            n
        )
    }
}

/// Key configuration taken from the user's settings.
#[derive(Clone, Default)]
pub struct KeyConfig {
    pub keydb_path: String,
    pub keyserver_url: String,
    pub keyserver_token: String,
    pub online_only: bool,
    /// The user chose "Local keydb only": the online key service must NOT be
    /// consulted even when a URL is configured.
    ///
    /// Without this, `key_params` derived the online source purely from "is the
    /// URL non-empty", so "Local keydb only" and "keydb, then online" produced
    /// identical key sources — a user who configured a key service and then
    /// switched to local-only for privacy still had disc ciphertext POSTed to
    /// that service on every keydb miss, and the key strip could even report
    /// the disc as unlocked via "online".
    pub local_only: bool,
}

impl KeyConfig {
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        // `key_source` is one of "Local keydb only" / "Online key service only"
        // / "keydb, then online". All three must be represented: matching only
        // the Online arm silently collapses the other two.
        KeyConfig {
            keydb_path: s.keydb_path.clone(),
            keyserver_url: s.keyserver_url.clone(),
            keyserver_token: s.keyserver_token.clone(),
            online_only: s.key_source.starts_with("Online"),
            local_only: s.key_source.starts_with("Local"),
        }
    }
}

/// Whether a request carries a per-title stream breakdown at all.
///
/// This exists because ONE representation was carrying TWO meanings. The field
/// used to be a bare `Vec<(usize, Vec<u16>, Vec<u16>)>` in which a title's
/// absence meant *either* "this caller has no per-title data, use the union in
/// `audio_pids`/`sub_pids`" (the CLI and the container path) *or* "the user
/// deliberately kept nothing under this title". Those want opposite answers,
/// and the union won both: a title the user emptied was handed its sibling's
/// tracks. Blu-ray playlists of one feature routinely share PIDs, so the
/// sibling's tracks are usually exactly the ones just unticked.
///
/// Splitting the two apart into variants makes the ambiguity unrepresentable:
/// a caller that has no breakdown says so, and a caller that has one is
/// believed, empty entries included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TitleStreams {
    /// No per-title breakdown exists — the CLI's shape, and a container source.
    /// Every title gets the union in `audio_pids`/`sub_pids`, which is what
    /// every caller did before the breakdown existed.
    #[default]
    Unspecified,
    /// The user's ticks, per CANONICAL title index. Authoritative: an entry
    /// with empty PID lists means the user kept NOTHING of that class under
    /// that title, and is honoured as such.
    ///
    /// The GUI emits an entry for every title that has selectable stream rows,
    /// ticked or not (see `ui::Tree::ticked_streams_by_title`), so a title
    /// missing from this list has no selectable streams to describe — a
    /// video-only title — and falls back to the union, which cannot narrow
    /// anything it does not have.
    PerTitle(Vec<(usize, Vec<u16>, Vec<u16>)>),
}

impl TitleStreams {
    /// This title's own ticked `(audio, subtitle)` PIDs, or `None` when the
    /// request says nothing about it and the union must be used.
    pub fn for_title(&self, title: Option<usize>) -> Option<(&[u16], &[u16])> {
        let (Self::PerTitle(per), Some(t)) = (self, title) else {
            return None;
        };
        per.iter()
            .find(|(ti, _, _)| *ti == t)
            .map(|(_, a, s)| (a.as_slice(), s.as_slice()))
    }
}

/// Which titles the user ticked, as canonical indices.
#[derive(Clone)]
pub struct RipRequest {
    pub source: String,
    pub dest_dir: String,
    pub titles: Vec<usize>,
    /// What the numbers in `titles` referred to on the scan they were picked
    /// against, indexed by CANONICAL title index (not by selection position —
    /// that is the shape `picked_ids` already uses, and it needs no length
    /// invariant to be right).
    ///
    /// Empty means "nothing was captured", which leaves every check that reads
    /// it inert and the behaviour exactly as it was: a caller that never saw a
    /// scan (the headless harness in `main.rs`) has nothing to promise.
    pub title_ids: Vec<TitleIdentity>,
    pub format: String,
    /// PIDs of the ticked audio tracks. Empty = keep every audio track.
    pub audio_pids: Vec<u16>,
    /// PIDs of the ticked subtitle tracks. Empty = keep every subtitle track.
    pub sub_pids: Vec<u16>,
    /// The ticked PIDs of each title, keyed by CANONICAL title index.
    ///
    /// `audio_pids`/`sub_pids` are the UNION across every title, and applying
    /// that union to each title in turn wrote a track the user had unticked
    /// whenever a sibling title shared its PID — which Blu-ray playlists of one
    /// feature routinely do. [`TitleStreams::Unspecified`] is the caller saying
    /// it has no breakdown, and only then is the union used; see that type for
    /// why "no data" and "an empty selection" cannot share a representation.
    pub title_pids: TitleStreams,
    /// True when the user actually made a per-track choice; distinguishes
    /// "keep everything" from "keep nothing".
    pub explicit_streams: bool,
    /// Ciphertext passthrough — the CLI's `--raw`. ISO output only.
    pub raw: bool,
    /// Overwrite a non-empty destination — the CLI's `--force`.
    pub force: bool,
    /// Output filename template (the `filename_template` setting). `{title}` is
    /// the disc/volume label, `{n}` the title number. Empty or placeholder-free
    /// falls back to `<label>_t<n>`.
    pub filename_template: String,
    /// AACS decrypt thread count (the `decrypt_threads` setting). `0` = auto
    /// (the library sizes its pool itself); `>0` pins the pool to that many.
    pub decrypt_threads: usize,
    /// True when true-multipass recovery is requested for a disc source
    /// (`rip_mode == "Multi-pass"` and `max_passes > 0`). A disc rip then
    /// recovers to a staged ISO via `fe::multipass_rip` before muxing; a
    /// "Whole disc → ISO image" output always recovers to an ISO regardless.
    pub multipass: bool,
    /// Max patch passes for multipass recovery (the `max_passes` setting).
    pub max_passes: u32,
    /// Abort the rip if more than this many seconds of the main title are lost
    /// after recovery (the `abort_lost_secs` setting). `0` = abort on any loss.
    pub abort_lost_secs: u64,
    /// Keep the intermediate ISO after a multipass title rip muxes from it (the
    /// `keep_iso` setting). Ignored for a "Whole disc → ISO image" output (the
    /// ISO is the deliverable).
    pub keep_iso: bool,
    /// Eject the disc once the drive is done being read (the `auto_eject`
    /// setting, mirrors autorip). For a multipass/ISO rip that's after recovery
    /// (before muxing from the staged ISO); for a single-pass rip it's after the
    /// last title is muxed off the drive.
    pub auto_eject: bool,
    pub keys: KeyConfig,
}

/// Run the real rip on a worker thread: engine title loop + per-title mux.
/// Returns immediately.
pub fn start_rip(req: RipRequest, state: Arc<RunState>) {
    /// Sets `finished` on EVERY exit from the worker — normal return, an early
    /// `?` someone adds later, or a panic unwinding through it.
    ///
    /// It used to be the closure's last statement, so a panic anywhere in the
    /// scan/mux/recovery chain (all of which run on disc-derived input) skipped
    /// it, `ui::tick` polled `finished` forever, and the window showed a rip in
    /// progress permanently with no error. Mirrors `freemkv-engine`'s
    /// `SignalDone`, whose doc records that two hand-rolled copies of this
    /// pattern both had the bug.
    struct SignalDone(Arc<RunState>);
    impl Drop for SignalDone {
        fn drop(&mut self) {
            // A panic may have poisoned any of these. Recover the guard instead
            // of unwrapping: a second panic here would leave `finished` unset
            // and reproduce the exact hang this guard exists to prevent.
            let mut summary = self
                .0
                .summary
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if summary.is_empty() {
                *summary = crate::strings::get("gui.result.nothing");
                *self
                    .0
                    .outcome
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = RunOutcome::Failed;
            }
            drop(summary);
            // `Release`, not `Relaxed`: `finished` is the flag `ui::tick`
            // polls before it will read `summary`/`outcome` at all (see
            // `RunState`'s doc). `summary` and `outcome` are each behind
            // their own `Mutex`, so a `tick` that itself takes those locks
            // can never see a torn value — but a bare `Relaxed` store here
            // gives the *worker* no obligation to keep this store ordered
            // after the mutex-protected writes above it in the eyes of a
            // reader that never takes the same locks in between (a future
            // caller polling `finished` and then reading through a
            // lock-free snapshot, or just for the reviewer who has to
            // re-derive the guarantee from first principles every time).
            // `Release` makes the guarantee explicit and free: everything
            // sequenced-before this store (both mutex writes above) is
            // guaranteed visible to any reader that `Acquire`-loads
            // `finished` and observes `true` — matching `ui::tick`'s load.
            self.0.finished.store(true, Ordering::Release);
        }
    }

    std::thread::spawn(move || {
        let _done = SignalDone(state.clone());
        let sink = UiSink(state.clone());
        let res = run_blocking(&req, &sink, &state);
        let (text, verdict) = match res {
            // A user stop is resumable, not a failure. `cancel` is the typed
            // signal the sink already polls, so it beats reading the prose.
            Ok(s) if state.cancel.load(Ordering::Relaxed) => (s, RunOutcome::Cancelled),
            Ok(s) => (s, RunOutcome::Completed),
            Err(e) => {
                state
                    .lines
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(e.clone());
                (e, RunOutcome::Failed)
            }
        };
        *state
            .summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
        *state
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = verdict;
    });
}

/// The decrypted-folder target for a rip: a per-disc subdirectory of the
/// destination, named by the disc's volume label. Exposed so the writability
/// gate is testable without a real disc.
pub fn extract_target(dest_dir: &str, label: &str) -> std::path::PathBuf {
    // Sanitised HERE, not at the call site: the label is disc bytes, and
    // `join` on a label of `..\..\Startup` walks straight out of the chosen
    // destination. Doing it in the seam means a future caller cannot forget.
    std::path::Path::new(dest_dir).join(sanitize_label(label))
}

/// Whether a decrypted-folder extraction may proceed into `dest`: a fresh or
/// empty subdir always may; a populated one needs `force` (the CLI's `--force`).
/// Only a `dir://` tree is gated — ordinary file/MKV output into a populated
/// folder is normal and never blocked.
pub fn folder_writable(dest: &std::path::Path, force: bool) -> Result<(), String> {
    let non_empty = std::fs::read_dir(dest)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if !force && non_empty {
        return Err(format!(
            "{} already exists and is not empty — enable “Overwrite existing files” in Settings to unpack into it.",
            dest.display()
        ));
    }
    Ok(())
}

/// Extract the disc's decrypted UDF file tree to a per-disc SUBDIRECTORY of the
/// destination (`<dest_dir>/<label>/`) — the CLI's `dir://` → `Disc::extract_tree`.
/// Targeting a subdir (never the raw dest_dir) is what stops it dumping into
/// ~/Movies and colliding with everything already there — the reported bug.
fn run_extract_folder(
    req: &RipRequest,
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    label: &str,
    sink: &UiSink,
    state: &Arc<RunState>,
) -> Result<String, String> {
    let dest = extract_target(&req.dest_dir, label);
    // A non-empty target means a previous extract (or another disc). Mirror the
    // CLI's --force gate, but check the SUBDIR — a fresh one is never "not empty".
    folder_writable(&dest, req.force)?;
    state
        .lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(format!(
            "extracting decrypted file tree → {}",
            dest.display()
        ));
    match fe::extract_tree(disc, reader, &dest, req.force, sink) {
        Ok(res) => {
            for f in &res.files {
                if f.bytes_unreadable > 0 {
                    state
                        .lines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(format!(
                            "  {} — {:.1} MB unreadable",
                            f.path.display(),
                            f.bytes_unreadable as f64 / 1_048_576.0
                        ));
                }
            }
            Ok(summarize_extract(&res, &dest))
        }
        Err(e) => Err(format!("Extraction failed: E{}", e.code())),
    }
}

fn is_stream_source(path: &str) -> bool {
    matches!(
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "mkv" | "m2ts" | "mts" | "mp4"
    )
}

/// The URL scheme for a source that has already been established as neither a
/// drive nor a stream container: a FOLDER is `dir://`, anything else is an
/// image (`iso://`).
///
/// Deliberately NOT `source_scheme`. That function classifies by file
/// extension and falls through to `m2ts` for anything it does not recognise,
/// which is right for its own caller (`convert_container`, guarded by
/// `is_stream_source`) and wrong here: a disc image saved as `Disc.img`,
/// `Disc.bin` or with no extension would be handed to the mux as
/// `m2ts://Disc.img` and re-opened as a single elementary stream. Reusing it
/// here regressed every image not named `.iso`.
fn image_or_dir_scheme(source: &str) -> &'static str {
    if std::path::Path::new(source).is_dir() {
        "dir"
    } else {
        "iso"
    }
}

fn source_scheme(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mkv" => "mkv",
        "mp4" => "mp4",
        "iso" => "iso",
        _ => "m2ts",
    }
}

/// What real operation an output format maps to. The picker offers twelve
/// format strings; six of them used to fall through to a per-title MKV mux.
/// Each now resolves to its true sink so the file the user gets matches what
/// they chose.
#[derive(Clone, Copy)]
enum OutKind {
    /// A per-title file produced through the mux pipeline. The `&str` is BOTH
    /// the dest-URL scheme AND the file extension — `mkv`/`mp4`/`m2ts` for
    /// containers, or `chapters`/`json`/`fvi` for the metadata / index sinks the
    /// resolve layer dispatches on (same as the CLI's `dir_jobs`).
    File(&'static str),
    /// Each title's tracks fanned out to elementary-stream files in a directory.
    /// The `&str` is the dest-URL scheme and NOT a file extension: `demux` for
    /// every track, or `video` / `audio` / `sub` for the CLI's narrowed forms,
    /// which are the same `DemuxSink` with a `TrackKind` filter (libfreemkv
    /// `mux::resolve`). All four name their own output files, so the dest URL
    /// is a directory — that is why this cannot be an `OutKind::File`, whose
    /// scheme doubles as the extension of a single per-title file.
    Demux(&'static str),
    /// The whole disc's decrypted UDF file tree, extracted to a per-disc
    /// subdirectory (the CLI's `dir://` → `Disc::extract_tree`).
    DecryptedFolder,
    /// A whole-disc sector image. Needs a physical disc (`disc://`); there is no
    /// iso-file → iso-file decrypt copy, so this is not offered for an ISO source
    /// yet — see the disc:// live-drive work.
    IsoImage,
}

/// Map a picker format string to its real output kind. Order matters only in
/// that each branch's marker is unique across the twelve format strings — note
/// `"video tracks"` is lower-case and two words, so it cannot be confused with
/// `"Video index → .fvi"`.
fn out_kind(format: &str) -> OutKind {
    if format.contains("decrypted folder") {
        OutKind::DecryptedFolder
    } else if format.contains("ISO image") {
        OutKind::IsoImage
    } else if format.contains("separate track") {
        OutKind::Demux("demux")
    } else if format.contains("video tracks") {
        OutKind::Demux("video")
    } else if format.contains("audio tracks") {
        OutKind::Demux("audio")
    } else if format.contains("subtitle tracks") {
        OutKind::Demux("sub")
    } else if format.contains("MP4") {
        OutKind::File("mp4")
    } else if format.contains("M2TS") {
        OutKind::File("m2ts")
    } else if format.contains("Chapters") {
        OutKind::File("chapters")
    } else if format.contains("JSON") {
        OutKind::File("json")
    } else if format.contains(".fvi") {
        OutKind::File("fvi")
    } else {
        OutKind::File("mkv")
    }
}

/// The path this request will actually write, decided the way the RIP decides
/// it rather than guessed alongside it.
///
/// The GUI shows this in the Information panel for the duration of the run, so
/// every difference from the real thing is a lie on screen for hours. It used
/// to be an independent `format!("{dir}/{stem}_t{n}.{ext}")` in `ui.rs` that
/// knew about neither the filename template (a shipped setting the engine
/// applies to every title) nor the disc label (what `run_disc` names titles
/// after — the source file's stem is only used for a CONTAINER source), and
/// spelled every whole-disc sink as `_t1.mkv`.
///
/// Structured as the mirror of `run_blocking`'s dispatch (stream source →
/// `run_stream`, everything else → `run_disc`/the image path) and `out_kind`'s
/// arms, so a new sink cannot be added without this seeing it.
pub fn planned_output_name(
    source: &str,
    dest_dir: &str,
    format: &str,
    first_title: Option<usize>,
    template: &str,
    volume_id: &str,
) -> String {
    // A container source is one title, named from the file's own stem; a disc
    // or image is named from its volume label, with the same "disc" fallback
    // `run_disc` uses for a label-less disc.
    let (label, n) = if is_stream_source(source) {
        let stem = std::path::Path::new(source)
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("output")
            .to_string();
        (stem, 1)
    } else if volume_id.is_empty() {
        ("disc".to_string(), first_title.unwrap_or(0) + 1)
    } else {
        (volume_id.to_string(), first_title.unwrap_or(0) + 1)
    };
    match out_kind(format) {
        OutKind::File(scheme) => {
            format!(
                "{dest_dir}/{}.{scheme}",
                title_basename(template, &label, n)
            )
        }
        // Every track file is named by the demux sink itself, so the target the
        // engine reports is the directory.
        OutKind::Demux(_) => format!("{dest_dir}/ (per-track files)"),
        OutKind::IsoImage => format!("{dest_dir}/{}.iso", sanitize_label(&label)),
        OutKind::DecryptedFolder => extract_target(dest_dir, &label).display().to_string(),
    }
}

/// The word the progress caption uses for what a format actually writes
/// ("Saving to {container} file").
///
/// Derived from [`out_kind`], deliberately: the caption and the sink then
/// cannot disagree, because they are the same decision read twice. The UI's
/// own version tested for MP4, then M2TS, then said MKV — so nine of the
/// twelve offered formats, ISO and JSON and .fvi among them, were captioned
/// with a container they never produce.
pub fn container_word(format: &str) -> &'static str {
    match out_kind(format) {
        OutKind::File("mp4") => "MP4",
        OutKind::File("m2ts") => "M2TS",
        OutKind::File("chapters") => "chapter",
        OutKind::File("json") => "JSON",
        OutKind::File("fvi") => "FVI",
        OutKind::File(_) => "MKV",
        OutKind::Demux("video") => "video track",
        OutKind::Demux("audio") => "audio track",
        OutKind::Demux("sub") => "subtitle track",
        OutKind::Demux(_) => "track",
        OutKind::IsoImage => "ISO",
        // The decrypted file tree as the disc carries it — UDF is the
        // filesystem being copied out, and the one word that is true of every
        // file in it.
        OutKind::DecryptedFolder => "UDF",
    }
}

/// Build a per-title output basename from the filename template. `{title}` →
/// the disc/volume label (or container name), `{n}` → the 1-based title number.
/// An empty template falls back to the historical `<label>_t<n>`; a template
/// with no `{n}` gets `_t<n>` appended so multi-title output can never collide.
/// Re-export of the shared display sanitiser (see
/// [`crate::strings::sanitize_display`]). Named here because this is where the
/// disc-bytes-to-UI boundary lives.
pub use crate::strings::sanitize_display;

/// Make a disc-supplied label safe to use as ONE filename component.
///
/// The volume label is disc bytes — untrusted. It reached the destination path
/// through `title_basename`, which stripped only `/`, so a label containing
/// `..\\..\\` escaped the destination directory on Windows. The CLI has always
/// defended against this (`pipe::sanitize_name`, whose own test spells out the
/// escape), but the GUI never called it.
///
/// This is deliberately NARROWER than the CLI's helper. `sanitize_name` is an
/// ASCII allow-list built for CLI filename cosmetics: it also drops apostrophes,
/// colons and periods, and collapses any non-Latin label to `"disc"`. Reusing
/// it here would rename every Japanese, Cyrillic or accented disc — which the
/// GUI renders correctly today, on every platform — in order to close a Windows
/// traversal. So: reject the path-y and the unrepresentable, keep the letters.
pub fn sanitize_label(label: &str) -> String {
    // Whole-name `.` / `..` are path navigation, not names.
    if label == "." || label == ".." {
        return "disc".to_string();
    }
    let cleaned: String = label
        .chars()
        .map(|c| match c {
            // Separators, the Windows drive-letter colon, the reserved
            // wildcard/redirect set, and any control character.
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Windows rejects a trailing dot or space on a path component.
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']);
    if trimmed.is_empty() {
        return "disc".to_string();
    }
    // A reserved DOS device name is not usable as a file stem on Windows.
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    if is_windows_reserved(stem) {
        return format!("_{trimmed}");
    }
    trimmed.to_string()
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM1-9`, `LPT1-9` — case-insensitive, still
/// reserved on modern Windows.
fn is_windows_reserved(stem: &str) -> bool {
    let s = stem.to_ascii_uppercase();
    matches!(s.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((s.starts_with("COM") || s.starts_with("LPT"))
            && s.len() == 4
            && s.as_bytes()[3].is_ascii_digit()
            && s.as_bytes()[3] != b'0')
}

/// Path separators a user might type are neutralized to keep output in-folder.
pub fn title_basename(template: &str, label: &str, n: usize) -> String {
    // Sanitise ONCE, up front, for every branch. The `{title}` substitution
    // below used to be the only sanitised path, which left the DEFAULT
    // (empty-template) case — the one most users are on — joining the raw
    // disc label into the output path. See `sanitize_label`'s own doc: that
    // is the escape it was added to close, closed on one branch only.
    let label = sanitize_label(label);
    let t = template.trim();
    if t.is_empty() {
        return format!("{label}_t{n}");
    }
    let mut name = t.replace("{title}", &label);
    if name.contains("{n}") {
        name = name.replace("{n}", &n.to_string());
    } else {
        name = format!("{name}_t{n}");
    }
    name
}

/// The audio/subtitle selection for this request. It goes on `InputOptions`,
/// NOT `MuxOptions`: our mux uses a URL source (`iso://…`), and `mux_stream`'s
/// Url arm prunes via `InputOptions.selection` — `MuxOptions.selection` is only
/// consulted on the File/Session (live-drive) arms. Putting it on MuxOptions
/// silently kept every track. Empty PID lists = keep none of that class.
/// The stream filter for one title, or for the whole request when no title is
/// in play.
///
/// `title` is the CANONICAL disc title index. When the request carries a
/// per-title breakdown, that title's own ticked PIDs are used — INCLUDING an
/// empty one, which is the user clearing every row under that title. Only
/// [`TitleStreams::Unspecified`] falls back to the union in
/// `audio_pids`/`sub_pids`, which is the CLI's shape and the behaviour every
/// caller had before the breakdown existed.
fn stream_selection_for(req: &RipRequest, title: Option<usize>) -> libfreemkv::StreamSelection {
    if !req.explicit_streams {
        return libfreemkv::StreamSelection::default();
    }
    let (audio, subtitle) = match req.title_pids.for_title(title) {
        Some((a, s)) => (a.to_vec(), s.to_vec()),
        None => (req.audio_pids.clone(), req.sub_pids.clone()),
    };
    libfreemkv::StreamSelection {
        audio: libfreemkv::PidFilter::Only(audio),
        subtitle: libfreemkv::PidFilter::Only(subtitle),
    }
}

fn mux_opts(req: &RipRequest) -> libfreemkv::MuxOptions {
    libfreemkv::MuxOptions {
        skip_errors: false,
        batch_sectors: 64,
        raw: req.raw,
        // Selection lives on InputOptions for the Url mux path — see
        // stream_selection_for. The Session (live-drive) arm gets its own
        // per-title options from title_session_mux_opts.
        selection: libfreemkv::StreamSelection::default(),
        send_deadline: Some(std::time::Duration::from_secs(60)),
    }
}

/// The mux options for ONE title of a live-drive (single-pass) rip.
///
/// The Session mux arm reads its selection from `MuxOptions`, not from
/// `InputOptions` the way the Url/ISO arm does, so the drive path needs its
/// own per-title options. It used to build one `MuxOptions` from the UNION of
/// every title's ticked PIDs before the loop and clone it for each title —
/// exactly the defect the ISO path already had: two playlists of one feature
/// routinely share PIDs, so unticking a commentary under title 1 wrote it
/// anyway whenever title 2 still had it ticked.
fn title_session_mux_opts(req: &RipRequest, idx: usize) -> libfreemkv::MuxOptions {
    libfreemkv::MuxOptions {
        selection: stream_selection_for(req, Some(idx)),
        ..mux_opts(req)
    }
}

/// A container source is a single title — no scan, straight to the mux.
///
/// NOTE: `MuxOptions.selection` is only applied on the disc/ISO input paths
/// (`resolve.rs` iso://, and the two disc paths in `driver.rs`). The container
/// path builds the stream directly, so per-track ticks are NOT honoured here
/// yet — verified empirically, not assumed. The caller warns the user rather
/// than silently writing tracks they deselected.
fn run_stream(req: &RipRequest, sink: &UiSink, state: &Arc<RunState>) -> Result<String, String> {
    std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;
    let name = std::path::Path::new(&req.source)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("output")
        .to_string();
    let hint = std::fs::metadata(&req.source).map(|m| m.len()).unwrap_or(0);
    let src_url = format!("{}://{}", source_scheme(&req.source), req.source);

    // A container is a single title. Route to the chosen sink; the whole-disc
    // operations have no meaning for one media file.
    let (dest_url, target) = match out_kind(&req.format) {
        OutKind::File(scheme) => {
            // A container is one title (n = 1); honor the template with the
            // file's own name as {title}.
            let base = title_basename(&req.filename_template, &name, 1);
            let out = format!("{}/{}.{}", req.dest_dir, base, scheme);
            (format!("{scheme}://{out}"), out)
        }
        OutKind::Demux(scheme) => {
            let dir = format!("{}/", req.dest_dir);
            (
                format!("{scheme}://{dir}"),
                format!("{dir} (per-track files)"),
            )
        }
        OutKind::DecryptedFolder | OutKind::IsoImage => {
            return Err("That output is for a disc source — open an ISO or disc to use it.".into());
        }
    };

    if req.explicit_streams {
        state.lines.lock().unwrap_or_else(|e| e.into_inner()).push(
            "Note: track selection is not applied to container sources yet — every track is kept."
                .to_string(),
        );
    }
    let o = fe::mux_title(
        &src_url,
        &dest_url,
        libfreemkv::InputOptions::default(),
        &mux_opts(req),
        hint,
        sink,
    )
    .map_err(|e| format!("convert failed: {e}"))?;
    if !o.completed {
        // Recovering, like every other `lines` lock in this file. This arm is
        // the CANCELLED path — reached exactly when something already went
        // wrong — and it sat three lines above a sibling that recovered
        // correctly. A worker that panicked earlier poisons `lines`, so an
        // `unwrap()` here turned one dead thread into a second panic while
        // writing the line that says where the partial file was left. The
        // failure branch needs the protection more than the success branch,
        // not less.
        state
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("cancelled — partial output kept: {target}"));
    } else {
        let mut lines = state.lines.lock().unwrap_or_else(|e| e.into_inner());
        lines.push(format!("wrote {target}"));
        // A completed export can still be LOSSY — a missing track OR bytes
        // dropped inside the tracks. Say so on the same run that reports the
        // write, never after a silent "Finished".
        lines.extend(lossy_lines(&o, &target));
    }
    Ok(summarize_stream(&o, &target, &req.dest_dir))
}

/// Whether a demux rip must give each title its own subdirectory.
///
/// A demux sink fans a title out into one file per elementary track, named by
/// track — NOT by title. Two titles written to the same directory therefore
/// overwrite each other's tracks. One title can go straight into the dest dir;
/// more than one cannot. Extracted so the boundary is assertable: as an inline
/// `indices.len() > 1` the off-by-one was invisible to every test.
fn demux_needs_subdirs(title_count: usize) -> bool {
    title_count > 1
}

/// The per-title mux input for `idx`.
///
/// Three fields, each of which fails silently and differently if it goes
/// missing, which is why this is a named function rather than a struct literal
/// buried in a closure:
///
/// - `title_index` — without it the library defaults to `None` and muxes the
///   WRONG TITLE, under the filename the user asked for. Nothing downstream can
///   tell.
/// - `unit_keys` — the scan resolving a disc's AACS keys is not enough; they
///   have to reach the mux or every encrypted title fails E7022. Mirrors the
///   engine's own wiring in `run.rs`.
/// - `selection` — the ticked audio/subtitle tracks, applied by the Url mux
///   path. Dropped, the user gets every track they deselected.
fn title_input_options(
    disc: &libfreemkv::Disc,
    req: &RipRequest,
    idx: usize,
) -> libfreemkv::InputOptions {
    libfreemkv::InputOptions {
        title_index: Some(idx),
        unit_keys: disc
            .aacs
            .as_ref()
            .map(|a| a.unit_keys.clone())
            .unwrap_or_default(),
        selection: stream_selection_for(req, Some(idx)),
        ..Default::default()
    }
}

/// Mux the selected titles from `source_url` (an `iso://<path>` — either the
/// original ISO source, or a staging ISO a multipass recovery just produced)
/// into the sinks the request's format maps to. Shared by `run_blocking`'s
/// ISO-source path and the title-multipass staging-ISO path in `run_disc` —
/// both need exactly the same per-title mux loop, they differ only in which
/// ISO the titles come from.
fn mux_selected_titles(
    disc: &libfreemkv::Disc,
    source_url: &str,
    req: &RipRequest,
    indices: &[usize],
    sink: &UiSink,
    state: &Arc<RunState>,
) -> Result<String, String> {
    let kind = out_kind(&req.format);
    let label = if disc.volume_id.is_empty() {
        "disc".to_string()
    } else {
        disc.volume_id.clone()
    };
    // Demux fans a single title straight into the dest dir but gives each title
    // of a multi-title rip its own subdir so their track files never collide.
    let multi = demux_needs_subdirs(indices.len());

    std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;

    // The engine owns the per-title loop (skip/abort policy); we only supply
    // "mux one title".
    let written = std::cell::Cell::new(0usize);
    let partial = std::cell::Cell::new(0usize);
    let outcome = fe::run_titles(indices, !req.titles.is_empty(), sink, |idx| {
        // Destination per output kind: a per-title file for the container /
        // metadata / index sinks, or a demux directory (its own per-track
        // naming) for separate track files.
        let (dest_url, target) = match kind {
            OutKind::File(scheme) => {
                let base = title_basename(&req.filename_template, &label, idx + 1);
                let out = format!("{}/{}.{}", req.dest_dir, base, scheme);
                (format!("{scheme}://{out}"), out)
            }
            OutKind::Demux(scheme) => {
                let dir = if multi {
                    format!("{}/t{:02}/", req.dest_dir, idx + 1)
                } else {
                    format!("{}/", req.dest_dir)
                };
                (format!("{scheme}://{dir}"), dir)
            }
            // Whole-disc kinds handled by their own callers.
            OutKind::DecryptedFolder | OutKind::IsoImage => unreachable!(),
        };
        let hint = disc.titles.get(idx).map(|t| t.size_bytes).unwrap_or(0);
        let input = title_input_options(disc, req, idx);
        let mux = mux_opts(req);
        match fe::mux_title(source_url, &dest_url, input, &mux, hint, sink) {
            Ok(o) => {
                if !o.completed {
                    // Cancelled or truncated: a partial file is on disk. Keep it —
                    // a partial mp4/mkv is usually watchable up to the cut — but
                    // don't count it as a full write, and SAY it's partial. Never
                    // "nothing written" when a file is sitting in the folder.
                    partial.set(partial.get() + 1);
                    state
                        .lines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(format!(
                            "title {} cancelled — partial output kept: {}",
                            idx + 1,
                            target
                        ));
                    return Ok(());
                }
                {
                    let mut lines = state.lines.lock().unwrap_or_else(|e| e.into_inner());
                    lines.push(format!("title {} -> {}", idx + 1, target));
                    // Completed, but not everything: a lossy export is never
                    // silent. See `lossy_lines`.
                    lines.extend(lossy_lines(&o, &target));
                }
                written.set(written.get() + 1);
                state
                    .titles_done
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                state
                    .lines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("Title {}: {}", idx + 1, explain(error_code(&e))));
                // A failed per-title FILE mux leaves a 0-byte file behind that
                // looks like output. Remove it so the folder never shows a broken
                // result. (Demux writes into a directory — nothing to clean.)
                if matches!(kind, OutKind::File(_)) {
                    let _ = std::fs::remove_file(&target);
                }
                Err(e)
            }
        }
    });

    summarize_outcome(
        &outcome,
        written.get(),
        partial.get(),
        indices.len(),
        &req.dest_dir,
    )
}

/// Human time for a lost-playback duration, e.g. `4m` or `12.4s`. A tiny local
/// copy of the CLI's `pipe::fmt_damage_time` — that function lives in
/// `pipe.rs`, which is CLI-only and not part of this crate's `lib` target (the
/// GUI shells build against `freemkv::engine`, not `freemkv::pipe`), so it
/// cannot be reused directly without restructuring which crate owns it.
fn fmt_damage_time(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else if secs >= 1.0 {
        format!("{:.0}s", secs)
    } else if secs >= 0.01 {
        format!("{:.2}s", secs)
    } else {
        format!("{:.0}ms", secs * 1000.0)
    }
}

/// A trailing note for a multipass result that finished (not halted, not
/// aborted for loss) but still has residual damage under the configured
/// tolerance — empty string when the recovery was clean.
///
/// Without this, a disc that recovers with real unreadable sectors UNDER
/// `abort_lost_secs` reports a plain "ISO image written to …" / "N title(s)
/// written to …", identical to a perfect rip: the CLI's own `disc_to_iso`
/// prints exactly this figure (`rip.mapfile_summary` / `rip.damage_lost_movie`)
/// for the same condition, so the GUI was hiding damage the CLI discloses.
/// Reuses those two existing i18n keys rather than adding new ones.
fn damage_note(result: &fe::MultipassResult) -> String {
    if result.unreadable_bytes == 0 && result.pending_bytes == 0 {
        return String::new();
    }
    let mut note = format!(
        "\n{}",
        crate::strings::fmt(
            "rip.mapfile_summary",
            &[
                (
                    "good",
                    &format!("{:.2}", result.good_bytes as f64 / 1_073_741_824.0)
                ),
                (
                    "unreadable",
                    &format!("{:.1}", result.unreadable_bytes as f64 / 1_048_576.0)
                ),
                (
                    "pending",
                    &format!("{:.1}", result.pending_bytes as f64 / 1_048_576.0)
                ),
            ],
        )
    );
    if result.main_lost_ms.is_finite() && result.main_lost_ms > 0.0 {
        note.push('\n');
        note.push_str(&crate::strings::fmt(
            "rip.damage_lost_movie",
            &[("time", &fmt_damage_time(result.main_lost_ms / 1000.0))],
        ));
    }
    note
}

fn run_blocking(req: &RipRequest, sink: &UiSink, state: &Arc<RunState>) -> Result<String, String> {
    if is_disc_source(&req.source) {
        return run_disc(req, sink, state);
    }
    if is_stream_source(&req.source) {
        return run_stream(req, sink, state);
    }
    // Pin the AACS decrypt pool if the user set a thread count; 0 leaves the
    // library to size it automatically.
    if req.decrypt_threads > 0 {
        libfreemkv::set_decrypt_threads(req.decrypt_threads);
    }
    // Folder OR image. `Ui::open` already scans a folder through `scan_dir`,
    // so without this the GUI listed a folder's titles and then failed the
    // moment the user pressed Rip — the same "ask for a folder then decline
    // it" defect as the picker, moved to a later button.
    let src_path = std::path::Path::new(&req.source);
    let scan = if src_path.is_dir() {
        libfreemkv::scan_dir
    } else {
        libfreemkv::scan_iso
    };
    let (mut disc, mut reader) = scan(src_path, libfreemkv::ScanOptions::default())
        .map_err(|e| format!("E{} scan failed", e.code()))?;
    // Resolve decryption keys onto the disc BEFORE muxing.
    resolve_disc_keys(&mut disc, reader.as_mut(), &req.keys);

    let kind = out_kind(&req.format);
    let label = if disc.volume_id.is_empty() {
        "disc".to_string()
    } else {
        disc.volume_id.clone()
    };

    // Whole-disc sinks bypass the per-title mux loop entirely — they operate on
    // the disc as a whole, not on a selected title.
    match kind {
        OutKind::DecryptedFolder => {
            std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;
            return run_extract_folder(req, &disc, reader.as_mut(), &label, sink, state);
        }
        OutKind::IsoImage => {
            // Decrypt an image without the disc: `iso://In.iso iso://Out.iso`.
            // The CLI has shipped this since 1.6.1 and the GUI refused it
            // outright, telling the user to pick "decrypted folder" instead —
            // a different deliverable, so the picker offered a choice that
            // could only ever fail.
            //
            // Single-pass, always. Multipass is a DRIVE strategy: it sweeps and
            // re-reads sectors an optical drive returned errors for. A file has
            // no unreadable sectors to retry, and `recover_to_iso` refuses
            // multipass unless `raw` is set, so passing the user's multipass
            // preference through would fail a request that makes sense.
            // Same disc-label-into-a-path hazard as `extract_target`.
            let dest =
                std::path::Path::new(&req.dest_dir).join(format!("{}.iso", sanitize_label(&label)));
            // Never write over the source. The scan holds it open and the
            // decrypt reads from it while writing, so this would destroy the
            // input mid-rip and leave neither file intact.
            //
            // Asked through the SHARED guard, not a comparison written out
            // here: this arm compared canonical paths only, which cannot see a
            // hardlink (two names for one inode are each already canonical), so
            // the desktop app wrote over a source the CLI refuses.
            if crate::file_identity::same_file(
                Some(std::path::Path::new(&req.source)),
                dest.as_path(),
            ) {
                return Err("The output image would overwrite the source. \
                     Choose a different output folder."
                    .into());
            }
            std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;
            state
                .lines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("decrypting image → {}", dest.display()));
            // `image_or_dir_scheme`, not `source_scheme` — see the former's
            // doc. `recover_to_iso` happens to read only `job.mode`/`job.raw`
            // today, so the URL is inert and this is not a live regression;
            // but the sibling arm 40 lines below builds the same field from
            // the same source with the other helper, and a `Job`-consuming
            // callee would resurrect the bug the moment one appeared.
            let mut job = fe::Job::new(
                format!("{}://{}", image_or_dir_scheme(&req.source), req.source),
                dest.display().to_string(),
            );
            job.raw = req.raw;
            job.mode = fe::RipMode::Single;
            let result = fe::recover_to_iso(&disc, reader.as_mut(), &dest, &job, sink)
                .map_err(|e| format!("image decrypt failed: {e}"))?;
            if recovery_produced_no_data(result.bytes_good) {
                let _ = std::fs::remove_file(&dest);
                return Err("No readable data — no image was written.".into());
            }
            return Ok(summarize_image_decrypt(&result, &dest));
        }
        _ => {}
    }
    let disc = disc;

    // The ticked numbers were resolved against `Ui::open`'s scan; this is a
    // different one, taken now. Same seam as `run_disc`, one source kind over:
    // a replaced or re-authored image between opening and Start would renumber
    // the titles under a selection nobody re-checked. Asked here rather than at
    // the top of the function so a whole-disc output (which has no selection to
    // invalidate) is never refused over it.
    verify_selection_identity(
        &req.titles,
        &req.title_ids,
        &disc
            .titles
            .iter()
            .map(TitleIdentity::of)
            .collect::<Vec<_>>(),
    )?;
    let sel = if req.titles.is_empty() {
        fe::Selection::MainMovie
    } else {
        fe::Selection::Titles(req.titles.clone())
    };
    let indices = fe::resolve_selection(&disc, &sel);
    state
        .lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(format!("selection resolved to titles {indices:?}"));
    if indices.is_empty() {
        return Err("Nothing selected to rip.".into());
    }
    // A folder is `dir://`, an image `iso://`. Hardcoding `iso://` here meant
    // the mux re-opened a folder as an image file and failed after a successful
    // scan and key resolution.
    let src_url = format!("{}://{}", image_or_dir_scheme(&req.source), req.source);
    mux_selected_titles(&disc, &src_url, req, &indices, sink, state)
}

/// What a `disc://` rip does with the drive, once the whole-disc extract case
/// is out of the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscPlan {
    /// Sweep + patch to a recovered ISO first. `deliver_iso` means the ISO is
    /// what the user asked for; otherwise it is staging for a title mux.
    Recover { deliver_iso: bool },
    /// Mux each selected title straight off the drive, one session per title.
    PerTitle,
}

/// Which of those two a request wants.
///
/// This is `want_iso || multipass` and both halves matter. As `&&` a
/// `--multipass` title rip silently loses its recovery passes — no patch
/// passes, and the abort-for-loss gate below never runs, so a damaged disc
/// muxes to a hole-ridden file reported as written. And a "whole disc → ISO
/// image" request without multipass would fall through to the per-title loop
/// and hit the `unreachable!()` in its dest match, i.e. panic the rip worker.
fn recovery_plan(kind: OutKind, multipass: bool) -> DiscPlan {
    let deliver_iso = matches!(kind, OutKind::IsoImage);
    if deliver_iso || multipass {
        DiscPlan::Recover { deliver_iso }
    } else {
        DiscPlan::PerTitle
    }
}

/// The `raw` flag a recovery job must carry, or the reason it cannot run.
///
/// `multipass_rip` REFUSES a real sweep-plus-patch plan with `raw = false`
/// ("multipass implies raw"): a whole-disc image recovery reads sectors it
/// cannot attribute to a title, so it cannot decrypt them. The GUI was handing
/// it `req.raw`, which `ui::raw_applies` forces to false for any title output —
/// so with the SHIPPED DEFAULTS (rip mode "Multi-pass", 5 passes, raw off) every
/// live-drive rip died before reading a sector, with the engine's own refusal as
/// the error text. The ISO-output path failed the same way whenever the user had
/// not ticked "keep encrypted".
///
/// The staged image for a title mux is therefore RAW, and the decrypt happens
/// where it always could: the mux re-opens the staged ISO on the ordinary
/// `iso://` path, with the same `KeyConfig`, and resolves keys from it.
///
/// The one case with no answer is "whole disc → ISO image", multipass, raw off:
/// the user asked for a decrypted image and a multipass recovery cannot produce
/// one. That is refused HERE, before the drive is staged, with something the
/// user can act on — rather than after, in the engine's vocabulary.
fn recovery_raw(multipass: bool, want_iso: bool, user_raw: bool) -> Result<bool, String> {
    if !multipass {
        // Single-pass: an ordinary decrypting copy, and `raw` means what the
        // user set it to.
        return Ok(user_raw);
    }
    if !want_iso {
        // Staging image for a mux. Encrypted on disk, decrypted on the way
        // into the container.
        return Ok(true);
    }
    if user_raw {
        return Ok(true);
    }
    Err(
        "A multi-pass recovery reads the whole disc, so the image it writes is \
         encrypted. For a decrypted ISO, set Rip mode to 'Single pass'; to keep \
         the multi-pass recovery, tick 'Keep encrypted (raw)'."
            .into(),
    )
}

/// What a title NUMBER actually referred to, so a selection survives a rescan.
///
/// The type is [`crate::title_identity::TitleIdentity`] and lives there because
/// the CLI's `pipe::resolve_scanned_title` asks the identical question one scan
/// later. This module used to carry its OWN answer — playlist name plus
/// duration — which cannot separate the case the project's rules name outright:
/// "titles legitimately carry duplicate playlists with identical duration and
/// size". Duration is precisely the field that legitimately collides. The
/// shared identity keys on the playlist name, its numeric id, and the EXTENTS,
/// and the sectors are what make it safe: two titles reading the same sectors
/// from the same playlist produce byte-identical rips, so there is nothing left
/// to confuse.
///
/// Re-exported under this path rather than aliased so `engine`'s call sites and
/// its tests keep naming one type, and so nobody adds a second definition here
/// again.
pub use crate::title_identity::TitleIdentity;

/// Translate a selection made against the DRIVE scan into indices valid for a
/// scan of the staged image.
///
/// A multipass recovery can leave a playlist unreadable, and the rescan then
/// yields a SHORTER title list — at which point every number past the gap
/// addresses a different title, and the mux writes the wrong one under the
/// name the user asked for, reporting "1 title(s) written". Positions are not
/// identity; this is the same lesson the audit's identity lens keeps finding.
///
/// `ids` is indexed by CANONICAL title number, not by position in `titles` —
/// the same shape `picked_ids` uses in the live-drive loop. That is what keeps
/// the fallback honest: "no identity for title 3" is `ids.get(3) == None`, a
/// per-title answer, so a title nobody captured costs only itself. Keyed by
/// selection position instead, a list that came up one entry short (an
/// out-of-range number dropped on the way in) made this return every raw
/// position unchanged, putting the WHOLE batch back on the stale-position path
/// this exists to close — silently, and for titles whose identity was known.
///
/// A title that CANNOT be found is a hard error: muxing the remaining ones
/// silently would deliver a subset under the same summary.
fn remap_titles_by_identity(
    iso_path: &str,
    titles: &[usize],
    ids: &[TitleIdentity],
) -> Result<Vec<usize>, String> {
    // Nothing selected, or nothing captured at all: no scan needed, and
    // `Selection::MainMovie` handles the empty case downstream.
    if titles.is_empty() || ids.is_empty() {
        return Ok(titles.to_vec());
    }
    remap_against(titles, ids, &scan_titles(iso_path)?)
}

/// The decision half of [`remap_titles_by_identity`], without the scan.
///
/// Separate so it is TESTABLE: reaching the real call site needs a live drive
/// and a completed multipass recovery, and a test that re-implemented this
/// logic beside it would pass whatever the code did — which is the failure
/// mode this audit keeps finding in its own tests.
fn remap_against(
    titles: &[usize],
    ids: &[TitleIdentity],
    staged: &[TitleIdentity],
) -> Result<Vec<usize>, String> {
    let mut out = Vec::with_capacity(titles.len());
    for &was in titles {
        // No identity recorded for this number: nothing to disagree with, so
        // it keeps its position — and only it does.
        let Some(id) = ids.get(was) else {
            out.push(was);
            continue;
        };
        match staged.iter().position(|s| s == id) {
            Some(now) => out.push(now),
            None => {
                return Err(format!(
                    "Title {} ({}) is not in the recovered image — the damage \
                     destroyed its playlist, so it cannot be muxed.",
                    was + 1,
                    id.describe()
                ));
            }
        }
    }
    Ok(out)
}

/// Confirm the title at `idx` in a FRESH scan is still the one the selection
/// meant, before it is muxed under that number.
///
/// The single-pass drive path is the same "position is not identity" shape
/// `remap_titles_by_identity` handles for the recovery path, one scan later:
/// `run_disc` scans once to resolve the selection, then EVERY title in the loop
/// re-opens the drive and scans again, carrying only an integer. A second scan
/// that lists the titles in a different order, or drops one before the selected
/// index, still resolves that integer — to a different title, muxed under the
/// name the user asked for.
///
/// It VERIFIES rather than remaps: a live drive whose title list moved between
/// two scans of the same disc is a disc/drive problem, not the orderly
/// short-list a recovery produces, so the honest answer is to stop.
///
/// `expected` is `None` when nothing was recorded for that index, which leaves
/// the pre-existing behaviour untouched.
fn verify_title_identity(
    expected: Option<&TitleIdentity>,
    scanned: &[TitleIdentity],
    idx: usize,
) -> Result<(), String> {
    let Some(expected) = expected else {
        return Ok(());
    };
    match scanned.get(idx) {
        Some(found) if found == expected => Ok(()),
        Some(found) => Err(format!(
            "Title {} changed between scans: it was {}, the drive now reports {} at that \
             position. Nothing was written for it.",
            idx + 1,
            expected.describe(),
            found.describe()
        )),
        None => Err(format!(
            "Title {} ({}) is no longer on the disc — the rescan lists only {} title(s).",
            idx + 1,
            expected.describe(),
            scanned.len()
        )),
    }
}

/// Confirm a whole SELECTION still means what it meant when it was made.
///
/// The window this closes is the one the per-title check cannot see: the user
/// ticks titles against the scan on screen and then reviews streams, format and
/// destination before pressing Start. `run_disc` takes a brand-new scan at that
/// point, and the ticked numbers were resolved against it with nothing to say
/// they still refer to the same titles — swap the disc (or let the drive
/// enumerate differently) and the wrong film is muxed and reported as success
/// under the name the first disc earned.
///
/// `picked` is indexed by canonical title number, so an entry that was never
/// captured is `None` and leaves that title exactly as it behaved before; an
/// empty `picked` (a caller that saw no scan) disables the check entirely.
/// Every decision is [`verify_title_identity`]'s — one definition of "is this
/// still the same title?", not a second one beside this call site.
fn verify_selection_identity(
    titles: &[usize],
    picked: &[TitleIdentity],
    scanned: &[TitleIdentity],
) -> Result<(), String> {
    for &t in titles {
        verify_title_identity(picked.get(t), scanned, t)?;
    }
    Ok(())
}

/// The staged image's title identities, in order.
fn scan_titles(iso_path: &str) -> Result<Vec<TitleIdentity>, String> {
    let (disc, _reader) = libfreemkv::scan_iso(
        std::path::Path::new(iso_path),
        libfreemkv::ScanOptions::default(),
    )
    .map_err(|e| format!("could not re-scan the recovered image (E{}).", e.code()))?;
    Ok(disc.titles.iter().map(TitleIdentity::of).collect())
}

/// A recovery that read nothing has nothing to mux. Separate from the caller so
/// the boundary is assertable: as `!=` a perfectly good recovery deletes its own
/// ISO and reports "no readable data".
fn recovery_produced_no_data(good_bytes: u64) -> bool {
    good_bytes == 0
}

/// Whether the staging ISO is removed after the title mux.
///
/// Three conditions, not one. `keep_iso` alone deleted the image on paths the
/// same function's own policy says never throw away the read: the mux reports a
/// CANCEL as `Ok("Cancelled — …")`, so a user who stopped the mux lost the
/// multi-hour recovery behind it and could only get it back by re-reading the
/// disc; a mux that failed outright (no space, a missing key, the destination
/// removed) deleted the one artefact that would have let the user retry the mux
/// alone.
///
/// Cancellation is read as a FLAG, never from the summary text — the same rule
/// `RunOutcome` exists to enforce. Deleting a recovered image on a wording
/// change would be the worst possible version of that defect.
///
/// Inverted, this deletes a multi-hour recovery the user explicitly asked to
/// keep — which is why it is a named function with its own tests.
fn should_delete_staging_iso(keep_iso: bool, mux_succeeded: bool, cancelled: bool) -> bool {
    !keep_iso && mux_succeeded && !cancelled
}

/// Rip from a live optical drive (`disc://`). Scans once to resolve titles and
/// keys, then runs the chosen sink. Title/metadata/demux sinks mux each selected
/// title off a fresh session (`fe::mux_title_session`, driven through
/// `fe::run_titles` for the shared fail-fast/skip/halt policy — the exact loop
/// the ISO path uses); decrypted-folder extracts the UDF tree to a per-disc
/// subdir. Whole-disc ISO image is flagged as not yet wired for the GUI (needs
/// the mapfile copy path — see below).
///
/// NEEDS HARDWARE VALIDATION end-to-end.
fn run_disc(req: &RipRequest, sink: &UiSink, state: &Arc<RunState>) -> Result<String, String> {
    if req.decrypt_threads > 0 {
        libfreemkv::set_decrypt_threads(req.decrypt_threads);
    }
    let kind = out_kind(&req.format);

    // Scan once (shared drive core): titles, label, key state, per-disc name.
    std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;
    let (mut session, _trace) = fe::open_scan_resolve(
        disc_target(&req.source),
        session_credentials(&req.keys),
        key_factory(&req.keys),
    )
    .map_err(|e| match &e {
        libfreemkv::Error::DeviceNotFound { path } if path.is_empty() => {
            "No optical drive found. Connect a Blu-ray/DVD drive with a disc.".to_string()
        }
        _ => format!("{e}"),
    })?;
    // The resolved device path (autodetect included) — captured now so we can
    // eject after the drive is done being read, whichever branch runs.
    let device = session.device_path().to_string();
    let disc = session.disc().ok_or("scan produced no disc")?;
    let label = if disc.volume_id.is_empty() {
        "disc".to_string()
    } else {
        disc.volume_id.clone()
    };
    // What THIS scan's numbers refer to. Banked once, before any branch: the
    // recovery arm remaps the selection against the staged image with it, and
    // the per-title arm verifies each re-scan against it.
    let scanned_ids: Vec<TitleIdentity> = disc.titles.iter().map(TitleIdentity::of).collect();
    // ...and the first thing it is used for is the scan the SELECTION was made
    // against, which is not this one: the tree the user ticked came from an
    // earlier scan, and everything between then and Start (reviewing streams,
    // choosing a format, a swapped disc) happened without a check. Every later
    // re-scan in this function is verified; this one was trusted.
    verify_selection_identity(&req.titles, &req.title_ids, &scanned_ids)?;

    // Decrypted folder: extract the UDF tree off the staged drive into a
    // per-disc subdir (same helper the ISO path uses).
    if matches!(kind, OutKind::DecryptedFolder) {
        session.stage_drive_as_reader();
        let mut reader = session
            .take_reader()
            .ok_or("could not stage the drive for extraction")?;
        let disc = session.disc().ok_or("scan produced no disc")?;
        let result = run_extract_folder(req, disc, reader.as_mut(), &label, sink, state);
        drop(reader);
        drop(session);
        if req.auto_eject {
            eject_disc(&device, sink);
        }
        return result;
    }

    // True-multipass recovery, or a whole-disc ISO image: recover the disc to a
    // (staged) ISO via the shared fe::multipass_rip loop — the same strategy
    // autorip uses (sweep + patch passes to convergence, abort-on-lost). For
    // "Whole disc → ISO image" the recovered ISO IS the deliverable; for a title
    // output with multipass enabled, we then mux the selected titles from the
    // recovered ISO. A multipass image is ENCRYPTED — the recovery reads
    // sectors it cannot attribute to a title, so it cannot decrypt them, which
    // is why `multipass_rip` refuses a non-raw plan outright. The mux therefore
    // runs the ordinary iso:// path over the staged image WITH the same keys,
    // and the decrypt happens once, on the way into the container. See
    // `recovery_raw`.
    let want_iso = matches!(kind, OutKind::IsoImage);
    if recovery_plan(kind, req.multipass) != DiscPlan::PerTitle {
        // The FOURTH label-into-a-path seam, and the most-travelled one: this
        // is the ordinary drive -> ISO rip. Round 1 sanitised the other three
        // (title_basename's default branch, extract_target, and the
        // image-decrypt destination) and missed this, so the traversal stayed
        // open on the path most users take. Built through `Path::join` rather
        // than string interpolation so the type system carries the boundary.
        let iso_path = std::path::Path::new(&req.dest_dir)
            .join(format!("{}.iso", sanitize_label(&label)))
            .to_string_lossy()
            .into_owned();
        session.stage_drive_as_reader();
        let mut reader = session
            .take_reader()
            .ok_or("could not stage the drive for recovery")?;
        let disc = session.disc().ok_or("scan produced no disc")?;
        // The user picked title NUMBERS against a scan. The mux below runs over
        // the staged image, which is scanned again — and if damage destroyed a
        // playlist, the second list is shorter and every number after the gap
        // means a different title. `scanned_ids` (banked above, indexed by
        // canonical title number) is what those numbers refer to, so the
        // selection can be re-resolved by identity rather than by position.
        // See `remap_titles_by_identity`.
        let mut job = fe::Job::new(format!("disc://{}", req.source), iso_path.clone());
        job.raw = recovery_raw(req.multipass, want_iso, req.raw)?;
        let opts = fe::MultipassOpts {
            max_passes: req.max_passes,
            abort_on_lost_secs: req.abort_lost_secs,
            is_iso_output: want_iso,
        };
        let result = fe::multipass_rip(
            disc,
            reader.as_mut(),
            std::path::Path::new(&iso_path),
            &job,
            &opts,
            sink,
        )
        .map_err(|e| format!("recovery failed: {e}"))?;
        drop(reader);
        drop(session);
        // Read phase done: the deliverable (ISO) or the mux source is on disk,
        // so the drive is no longer needed — eject now, exactly like autorip
        // (which ejects at read-complete and muxes from the staged ISO).
        if req.auto_eject {
            eject_disc(&device, sink);
        }

        // Recovery verdicts are checked for BOTH output kinds, before the
        // want_iso split. They used to live inside the ISO branch only, which
        // got the gate exactly backwards: `effective_abort_secs` forces the
        // tolerance to 0 for ISO output, so `abort_on_lost_secs` is only ever a
        // meaningful setting on the title/MKV path — and that path muxed the
        // partial image anyway and reported "N title(s) written". A user who set
        // a 30-second tolerance could be handed a playable MKV missing four
        // minutes of the feature, with nothing in the result to say so.
        //
        // The recovered image is kept in every case, so an abort never throws
        // away the read; the user can retry the mux or keep recovering.
        if result.halted {
            return Ok(if want_iso {
                format!("Cancelled — partial ISO kept: {iso_path}")
            } else {
                format!("Cancelled — nothing muxed; partial ISO kept: {iso_path}")
            });
        }
        if result.aborted_for_loss {
            // Not an Ok: a title muxed from an image this damaged would be
            // missing footage, and reporting that as a written title is the
            // silent-loss failure the gate exists to prevent.
            return if want_iso {
                // Err, not Ok: the partial ISO is worth keeping, but the run did
                // not deliver what was asked for. Returning Ok here exited 0 and
                // rendered the abort as a completed rip.
                Err(format!(
                    "Recovery aborted — too much unreadable data; partial ISO kept: {iso_path}"
                ))
            } else {
                Err(format!(
                    "Recovery aborted — too much unreadable data to mux a complete title \
                     (raise the lost-seconds tolerance to accept it, or re-run recovery). \
                     Partial ISO kept: {iso_path}"
                ))
            };
        }

        if want_iso {
            return Ok(format!(
                "ISO image written to {iso_path}{}",
                damage_note(&result)
            ));
        }

        // Title output: mux the selected titles from the recovered ISO by
        // running the ordinary ISO-source path on it. The staged image is
        // encrypted (see `recovery_raw`), so the fresh scan resolves keys from
        // it exactly as it would for any `iso://` source — `iso_req` inherits
        // this request's `KeyConfig`, and its `raw` is the user's setting,
        // which for a title output is always false.
        if recovery_produced_no_data(result.good_bytes) {
            let _ = std::fs::remove_file(&iso_path);
            return Err("Recovery produced no readable data — nothing to mux.".into());
        }
        // Re-resolve the selection against the staged image before muxing.
        let titles = match remap_titles_by_identity(&iso_path, &req.titles, &scanned_ids) {
            Ok(t) => t,
            Err(e) => {
                return Err(format!("{e} The recovered image is kept: {iso_path}"));
            }
        };
        let iso_req = RipRequest {
            source: iso_path.clone(),
            titles,
            // The numbers above have just been resolved BY IDENTITY against
            // the staged image, so the drive scan's identities no longer line
            // up with them — carrying them on would compare a drive title
            // against whatever the staged image lists at its new number. The
            // remap is the check for this hop.
            title_ids: Vec::new(),
            ..req.clone()
        };
        let mux = run_blocking(&iso_req, sink, state);
        // The staged image is only disposable once the titles it was staged
        // for actually landed. `state.cancel` is the flag the Stop button
        // sets, read directly rather than inferred from the mux's summary.
        let cancelled = state.cancel.load(std::sync::atomic::Ordering::SeqCst);
        if should_delete_staging_iso(req.keep_iso, mux.is_ok(), cancelled) {
            let _ = std::fs::remove_file(&iso_path);
        }
        if let Err(e) = &mux {
            return Err(format!("{e} — the recovered image is kept: {iso_path}"));
        }
        if cancelled {
            return Ok(format!(
                "Cancelled — the recovered image is kept: {iso_path}"
            ));
        }
        // The recursive mux above reports its own success text (titles
        // written); it has no way to know THIS stage's recovery left residual
        // damage under tolerance, so the note is appended out here instead.
        return mux.map(|s| format!("{s}{}", damage_note(&result)));
    }

    // Which titles to rip — same Selection/resolve_selection the ISO path
    // uses, so a live-drive rip gets the same main-title default and
    // out-of-range filtering.
    let sel = if req.titles.is_empty() {
        fe::Selection::MainMovie
    } else {
        fe::Selection::Titles(req.titles.clone())
    };
    let indices = fe::resolve_selection(disc, &sel);
    if indices.is_empty() {
        return Err("Nothing selected to rip.".into());
    }
    let multi = demux_needs_subdirs(indices.len());
    // Byte-size hints per title, banked before the scan session is dropped
    // (releasing the drive) — each title's mux reopens its own session,
    // mirroring the CLI.
    let hints: Vec<u64> = disc.titles.iter().map(|t| t.size_bytes).collect();
    // What each selected NUMBER refers to on THIS scan — banked before the
    // session is dropped, above. Every title below re-opens the drive and scans
    // again, so the index alone does not prove the mux is about to read the
    // title the user picked. See `verify_title_identity`.
    let picked_ids = scanned_ids;
    drop(session);

    // The engine owns the per-title loop (skip/abort policy); we only supply
    // "mux one title" — exactly the ISO path's shape, but each title reopens
    // its own DiscSession off the live drive.
    let written = std::cell::Cell::new(0usize);
    let partial = std::cell::Cell::new(0usize);
    let outcome = fe::run_titles(&indices, !req.titles.is_empty(), sink, |idx| {
        let (dest_url, target) = match kind {
            OutKind::File(scheme) => {
                let base = title_basename(&req.filename_template, &label, idx + 1);
                let out = format!("{}/{}.{}", req.dest_dir, base, scheme);
                (format!("{scheme}://{out}"), out)
            }
            OutKind::Demux(scheme) => {
                let dir = if multi {
                    format!("{}/t{:02}/", req.dest_dir, idx + 1)
                } else {
                    format!("{}/", req.dest_dir)
                };
                (format!("{scheme}://{dir}"), dir)
            }
            OutKind::DecryptedFolder | OutKind::IsoImage => unreachable!(),
        };
        let hint = hints.get(idx).copied().unwrap_or(0);

        // Shared drive bring-up (open + lock + scan + resolve) — same core as
        // the CLI's pipe_disc. A fresh session per title matches it (the
        // staged reader is consumed by one mux).
        let (mut session, _trace) = match fe::open_scan_resolve(
            disc_target(&req.source),
            session_credentials(&req.keys),
            key_factory(&req.keys),
        ) {
            Ok(v) => v,
            // Same bypass as the identity check below: an `Err` returned
            // straight out of this closure never reaches the arm that writes a
            // per-title reason into the log, so "no disc in the drive" reaches
            // the user as "Write failed (Other)."
            Err(e) => {
                let msg = format!("title {}: {e}", idx + 1);
                state
                    .lines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(msg.clone());
                return Err(std::io::Error::other(msg));
            }
        };
        // This is a DIFFERENT scan from the one the selection was made against.
        // Confirm the title still at this index is the one that was picked
        // before muxing it under that number.
        let rescanned: Vec<TitleIdentity> = session
            .disc()
            .map(|d| d.titles.iter().map(TitleIdentity::of).collect())
            .unwrap_or_default();
        // Say it, then stop it. Returning the verdict through `?` alone skips
        // the `Err(e)` arm below — the ONLY place a per-title failure becomes a
        // line in the log pane — and what survives is `RipOutcome::Failed`,
        // which carries an ErrorKind and no message. The verdict has no
        // `E<digits>` prefix for `error_code` to read, so `describe_failure`
        // renders its catch-all, and the user is told "Write failed (Other)."
        // about the one check built to name the two playlists involved.
        if let Err(msg) = verify_title_identity(picked_ids.get(idx), &rescanned, idx) {
            state
                .lines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(msg.clone());
            return Err(std::io::Error::other(msg));
        }
        session.stage_drive_as_reader();

        // Session arm reads selection from MuxOptions (unlike the Url arm),
        // and it is built HERE, per title — one union built before the loop
        // wrote tracks the user had unticked under this title whenever a
        // sibling title shared the PID.
        let opts = title_session_mux_opts(req, idx);

        match fe::mux_title_session(&mut session, idx, &dest_url, &opts, hint, sink) {
            Ok(o) => {
                if !o.completed {
                    // Cancelled or truncated: a partial file is on disk — keep
                    // it, don't count it as a full write, and say it's partial.
                    partial.set(partial.get() + 1);
                    state
                        .lines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(format!(
                            "title {} cancelled — partial output kept: {}",
                            idx + 1,
                            target
                        ));
                    return Ok(());
                }
                {
                    let mut lines = state.lines.lock().unwrap_or_else(|e| e.into_inner());
                    lines.push(format!("title {} -> {}", idx + 1, target));
                    // Completed, but not everything: a lossy export is never
                    // silent. See `lossy_lines`.
                    lines.extend(lossy_lines(&o, &target));
                }
                written.set(written.get() + 1);
                state.titles_done.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                state
                    .lines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("Title {}: {}", idx + 1, explain(error_code(&e))));
                // A failed per-title FILE mux leaves a 0-byte file behind that
                // looks like output. Remove it so the folder never shows a
                // broken result. (Demux writes into a directory — nothing to
                // clean.)
                if matches!(kind, OutKind::File(_)) {
                    let _ = std::fs::remove_file(&target);
                }
                Err(e)
            }
        }
    });

    // Single-pass reads each title straight off the drive, so the drive is only
    // free once the whole loop is done — eject here (autorip's auto_eject).
    if req.auto_eject {
        eject_disc(&device, sink);
    }

    summarize_outcome(
        &outcome,
        written.get(),
        partial.get(),
        indices.len(),
        &req.dest_dir,
    )
}

/// The engine→front-end outcome contract.
///
/// `run_titles` is in freemkv-engine and these renderings are here, so no
/// single-repo test covers the seam: the engine proves it *emits* `NoKey` /
/// `Failed`, and nothing proved this crate does anything with them. It didn't —
/// every variant fell through to an `Ok` string, and `start_rip` files an `Ok`
/// as the run summary, so a rip the engine stopped on a full disk was reported
/// as "2 title(s) written".
#[cfg(test)]
mod run_state_poison_tests {
    /// A panicking worker must not turn its verdict into "Completed".
    ///
    /// The UI read the verdict with `unwrap_or_default()`. `RunOutcome`'s
    /// `#[default]` is `Completed`, so a worker that panicked — poisoning the
    /// mutex, and precisely the case where the verdict matters — rendered the
    /// "Finished" heading over a run whose real outcome was lost, along with
    /// an empty summary line.
    ///
    /// That is the same defect `RunOutcome`'s own doc says it exists to
    /// prevent: the verdict is carried rather than parsed BECAUSE both
    /// abort-for-loss paths once rendered as success. The expectation here
    /// comes from that rule, not from what the accessor happens to do.
    #[test]
    fn a_poisoned_lock_does_not_turn_a_failure_into_a_completion() {
        use super::{RunOutcome, RunState};
        use std::sync::Arc;

        let st = Arc::new(RunState::default());
        *st.outcome.lock().unwrap() = RunOutcome::Failed;
        *st.summary.lock().unwrap() = "aborted: loss exceeded tolerance".to_string();

        // Poison both locks the way a panicking worker would.
        let st2 = Arc::clone(&st);
        let _ = std::thread::spawn(move || {
            let _g1 = st2.outcome.lock().unwrap();
            let _g2 = st2.summary.lock().unwrap();
            panic!("worker died holding the verdict");
        })
        .join();
        assert!(
            st.outcome.is_poisoned() && st.summary.is_poisoned(),
            "fixture invalid: the locks must actually be poisoned"
        );

        assert_eq!(
            st.outcome_now(),
            RunOutcome::Failed,
            "a poisoned lock rendered a FAILED run as Completed — the heading \
             then says Finished over a rip that did not produce its deliverable"
        );
        assert_eq!(
            st.summary_now(),
            "aborted: loss exceeded tolerance",
            "the summary the worker had already written was discarded"
        );
    }

    /// The GUI core's only `Sink` must still deliver after a worker panics.
    ///
    /// `UiSink::log` was `if let Ok(mut v) = ..lock() { v.push(..) }`, which
    /// reads as caution and behaves as censorship: `UiSink` is the ONE `Sink`
    /// the shared GUI core installs, so every library log line reaches the user
    /// through it. Once any thread panicked holding the buffer the `else` arm
    /// swallowed every line from then on — the run went silent at exactly the
    /// moment the log became the only evidence of what happened. `progress` had
    /// the same shape and froze the bar.
    ///
    /// Mutation caught: putting either method's `unwrap_or_else` back to
    /// `if let Ok(..)`/`unwrap()` — the first drops the line, the second panics
    /// the worker that was trying to report the first panic.
    #[test]
    fn the_ui_sink_still_delivers_lines_and_progress_through_a_poisoned_lock() {
        use super::{Prog, RunState, UiSink};
        use freemkv_engine as fe;
        use freemkv_engine::Sink as _;
        use std::sync::Arc;

        let st = Arc::new(RunState::default());
        // Poison both buffers the way a panicking worker would.
        let st2 = Arc::clone(&st);
        let _ = std::thread::spawn(move || {
            // Bound to a differently-named local on purpose. This fixture MUST
            // unwrap — poisoning the lock is the whole point of it — and
            // `every_lines_lock_recovers_from_poison` reads this file whole,
            // test code included. Taking the buffer's lock through a field
            // access spelled the usual way would make that pin fail on its own
            // sibling, so the field is rebound first.
            let buf = &st2.lines;
            let _g1 = buf.lock().unwrap();
            let _g2 = st2.prog.lock().unwrap();
            panic!("worker died holding the log buffer");
        })
        .join();
        assert!(
            st.lines.is_poisoned() && st.prog.is_poisoned(),
            "fixture invalid: both locks must actually be poisoned"
        );

        let sink = UiSink(Arc::clone(&st));
        sink.log(fe::Level::Warn, "E7022: no key for this disc");
        sink.progress(&fe::Progress {
            bytes_done: 7,
            bytes_total: 11,
            speed_bps: 3,
            eta_secs: Some(5),
            sectors_bad: 1,
            ..Default::default()
        });

        assert_eq!(
            *st.lines.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["E7022: no key for this disc".to_string()],
            "the sink dropped the line that explains the failure — after a \
             panic the log is the only record there is"
        );
        let Prog {
            bytes_done,
            bytes_total,
            ..
        } = *st.prog.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            (bytes_done, bytes_total),
            (7, 11),
            "the sink stopped publishing progress, so the bar freezes with no \
             explanation for the rest of the run"
        );
    }

    /// Makes `outcome_now`'s doc claim true instead of merely written down.
    ///
    /// That doc says the poison-recovering form is applied to EVERY `lines`
    /// lock, and that "the source-inspection pins below" enforce it. They did
    /// not: the pin above covers `outcome` and `summary` only. Meanwhile the
    /// CANCELLED arm of `run_container` and both drain loops in `main.rs` used
    /// a bare `unwrap()`, so a rip cancelled after an earlier worker panic
    /// re-panicked while writing the line naming the partial file it had kept.
    ///
    /// A source pin because the log buffer is written from paths that need a
    /// live drive or a real disc image; `main.rs`'s drain loop needs a whole
    /// process. Reading the text is the only thing that can see all of them at
    /// once. Both needles are built with `concat!` so this test's own body
    /// cannot match itself.
    ///
    /// Mutation caught: any `lines` lock anywhere in these two files reverting
    /// to `unwrap()`, `if let Ok(..)`, or `unwrap_or_default()`.
    #[test]
    fn every_lines_lock_recovers_from_poison() {
        let needle = concat!("lines", ".lock()");
        // Both spellings in the tree recover: the closure form
        // `unwrap_or_else(|e| e.into_inner())` and the function form
        // `unwrap_or_else(std::sync::PoisonError::into_inner)`. Matching the
        // recovery rather than one exact literal is deliberate — a pin that
        // only knew one of them would fail on correct code, which is worse
        // than no pin.
        let recovering = concat!(".unwrap", "_or_else(");
        let mut found = 0usize;
        for (name, raw) in [
            ("engine.rs", include_str!("engine.rs")),
            ("main.rs", include_str!("main.rs")),
        ] {
            // Whitespace-collapsed so a lock split over four lines by rustfmt
            // reads the same as one written inline — the formatter must not be
            // able to hide a regression from this pin.
            let src: String = raw.split_whitespace().collect();
            let mut at = 0usize;
            while let Some(i) = src[at..].find(needle) {
                let pos = at + i;
                let tail = &src[pos + needle.len()..];
                let head = &tail[..tail.len().min(80)];
                assert!(
                    tail.starts_with(recovering) && head.contains("into_inner"),
                    "{name}: a log-buffer lock does not recover from poison \
                     (site {}); a worker that panicked mid-run poisoned it, and \
                     this turns one dead thread into a second panic — or a \
                     silently discarded line — losing the diagnostic that \
                     explains the first. Near: {}",
                    found + 1,
                    &tail[..tail.len().min(60)]
                );
                found += 1;
                at = pos + needle.len();
            }
        }
        assert!(
            found >= 8,
            "expected to inspect every log-buffer lock in engine.rs and \
             main.rs, found only {found} — the needle stopped matching and \
             this pin is now vacuous"
        );
    }
}

#[cfg(test)]
mod outcome_summary_tests {
    use super::{
        fe, summarize_extract, summarize_image_decrypt, summarize_outcome, summarize_stream,
    };
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
    //
    // `run_stream` used to discard `libfreemkv::MuxOutcome` entirely via a
    // bare `?`, so a Cancel that landed mid-conversion (a real halt, not a
    // subsequently retried run) still reported "Written to <dir>" for a
    // truncated file — the same silent-success-on-cancel shape
    // `summarize_outcome`/`mux_selected_titles` were already hardened
    // against via `o.completed`, just never applied to the single-file path.

    #[test]
    fn a_completed_stream_conversion_reports_written() {
        assert_eq!(
            summarize_stream(&clean_outcome(), "/out/movie.mkv", "/out"),
            "Written to /out"
        );
    }

    // ── A COMPLETED export can still be lossy ───────────────────────────────
    //
    // `MuxOutcome::undelivered_streams` is the library's signal that the
    // finalised file does not match the pre-mux plan — non-empty *with*
    // `completed = true`. The CLI reports it; the GUI graded on `completed`
    // alone at four sites, so an MP4 export missing an audio track read as
    // "Finished" and nothing the user ever saw mentioned the missing track.
    //
    // The outcomes below are `libfreemkv::MuxOutcome` values, not booleans, so
    // the test states the real shape: completed AND lossy at the same time.

    /// The exact outcome the mp4 sink produces when it has to drop a track.
    fn lossy_outcome() -> libfreemkv::MuxOutcome {
        libfreemkv::MuxOutcome {
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

    /// A mux that dropped PAYLOAD BYTES is lossy too, and `completed` is true
    /// for it.
    ///
    /// `MuxOutcome::lost_bytes`/`errors` count bytes the library read and could
    /// not carry — an `mkv://` → `mkv://` re-mux of a 3D rip drops the whole
    /// dependent-view (right-eye) payload into them, with an EMPTY
    /// `undelivered_streams` because no whole stream was lost. Grading on
    /// `undelivered_streams` alone renders that as a clean "Written to <dir>".
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

    fn extract_result(
        halted: bool,
        files: usize,
        bytes_unreadable: u64,
    ) -> libfreemkv::ExtractResult {
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

    /// The image-decrypt summariser's three branches, matching the coverage
    /// its two siblings already had. It shipped with a doc claiming "Pure so
    /// the mapping is unit-testable" and no test — the exact gap that let the
    /// arm it replaced report a cancelled decrypt as a clean write.
    ///
    /// `complete` is derived here exactly as `CopyResult::new` derives it —
    /// pending included — so a fixture cannot present a combination the engine
    /// never produces and make the summariser look right on it.
    fn copy_result(halted: bool, good: u64, unreadable: u64) -> fe::CopyResult {
        copy_result_pending(halted, good, unreadable, 0)
    }

    fn copy_result_pending(
        halted: bool,
        good: u64,
        unreadable: u64,
        pending: u64,
    ) -> fe::CopyResult {
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
}

#[cfg(test)]
mod key_summary_tests {
    use super::{error_code, key_summary};

    pub(super) fn disc(encrypted: bool) -> libfreemkv::Disc {
        libfreemkv::Disc {
            volume_id: "T".into(),
            meta_title: None,
            format: libfreemkv::DiscFormat::BluRay,
            capacity_sectors: 1,
            capacity_bytes: 2048,
            layers: 1,
            titles: vec![],
            region: libfreemkv::disc::DiscRegion::Free,
            aacs: None,
            css: None,
            encrypted,
            aacs_error: None,
            css_error: None,
            content_format: libfreemkv::ContentFormat::BdTs,
        }
    }

    pub(super) fn aacs(unit_keys: Vec<(u32, [u8; 16])>) -> libfreemkv::AacsState {
        libfreemkv::AacsState {
            version: 1,
            bus_encryption: false,
            mkb_version: None,
            disc_hash: String::new(),
            // The scan-time PLACEHOLDER origin — identical whether or not a key
            // was ever resolved. That is exactly why it cannot be trusted.
            key_source: libfreemkv::KeyOrigin::ExternalUk,
            vuk: None,
            unit_keys,
            read_data_key: None,
            volume_id: [0u8; 16],
            uk_ro: Vec::new(),
            mkb: Vec::new(),
        }
    }

    #[test]
    fn unencrypted_disc_is_named_so() {
        assert_eq!(key_summary(&disc(false), None), "unencrypted");
    }

    #[test]
    fn css_dvd_is_named_so() {
        let mut d = disc(true);
        d.css = Some(libfreemkv::css::CssState {
            title_key: [0u8; 5],
            crack_span: None,
        });
        assert_eq!(key_summary(&d, None), "CSS (DVD)");
    }

    /// The regression that started this: `ExternalUk` + empty `unit_keys` is
    /// the pre-resolution placeholder, NOT a resolved key. It must never read
    /// as unlocked, and must never name a source.
    #[test]
    fn placeholder_origin_with_no_keys_reads_as_locked() {
        let mut d = disc(true);
        d.aacs = Some(aacs(vec![]));
        let s = key_summary(&d, None);
        assert_eq!(s, "locked — no key yet");
        assert!(!s.contains("online") && !s.contains("keydb"));
    }

    /// Same placeholder origin, but real key material present — and the trace
    /// says which source won. The origin is identical in both tests; only the
    /// key material and the trace differ, which is the whole point.
    #[test]
    fn real_keys_name_the_winning_source() {
        let mut d = disc(true);
        d.aacs = Some(aacs(vec![(1, [0x5A; 16])]));
        assert_eq!(key_summary(&d, Some("keydb")), "unlocked via keydb");
        assert_eq!(key_summary(&d, Some("online")), "unlocked via online");
    }

    /// Keys banked but no trace step (e.g. a resume-injected unit key): still
    /// unlocked, but we do NOT invent a source we cannot evidence.
    #[test]
    fn keys_without_a_trace_do_not_invent_a_source() {
        let mut d = disc(true);
        d.aacs = Some(aacs(vec![(1, [0x5A; 16])]));
        assert_eq!(key_summary(&d, None), "unlocked");
    }

    /// A VUK alone (no pre-decrypted unit keys) is still real key material.
    #[test]
    fn a_vuk_alone_counts_as_unlocked() {
        let mut d = disc(true);
        let mut a = aacs(vec![]);
        a.vuk = Some([0x11; 16]);
        d.aacs = Some(a);
        assert_eq!(key_summary(&d, Some("keydb")), "unlocked via keydb");
    }

    /// `error_code` must parse the digit run, not the whole Display string.
    ///
    /// Regression: `trim_start_matches('E').parse()` returned 0 for every error
    /// that carries data after the code, so the desktop app rendered "Mux failed
    /// (E0)." for an AACS disc with no key (E7022 carries the disc hash) and
    /// never reached the dedicated message for it.
    #[test]
    fn error_code_parses_codes_that_carry_data() {
        let c = |s: &str| error_code(&std::io::Error::other(s.to_string()));
        // Bare form (already worked).
        assert_eq!(c("E9048"), 9048);
        // Data-carrying forms — all of these used to yield 0.
        assert_eq!(c("E7022: 8f3a1c0d"), 7022);
        assert_eq!(c("E8005: /path/to/keydb.cfg"), 8005);
        assert_eq!(c("E6000: 12345 0x02"), 6000);
        assert_eq!(c("E6014: 0x1100"), 6014);
        // Not a library code.
        assert_eq!(c("No drive found"), 0);
        assert_eq!(c("E"), 0);
        assert_eq!(c("Eabc"), 0);
    }

    /// All three `key_source` values must produce distinct key sources.
    ///
    /// Regression: `key_url` was derived only from "is the URL non-empty", so
    /// "Local keydb only" and "keydb, then online" were identical — choosing
    /// local-only for privacy still sent disc ciphertext to the configured key
    /// service on every keydb miss.
    #[test]
    fn key_source_setting_controls_whether_the_online_service_is_used() {
        let cfg = |src: &str| super::KeyConfig {
            keydb_path: "/tmp/keydb.cfg".into(),
            keyserver_url: "https://keys.example/decode".into(),
            keyserver_token: "t".into(),
            online_only: src.starts_with("Online"),
            local_only: src.starts_with("Local"),
        };

        let local = super::key_params(&cfg("Local keydb only"));
        assert!(
            local.key_url.is_none(),
            "Local keydb only must NOT consult the online service"
        );
        assert!(local.keydb_path.is_some());

        let both = super::key_params(&cfg("keydb, then online"));
        assert!(
            both.key_url.is_some(),
            "keydb, then online must consult the online service"
        );
        assert!(both.keydb_path.is_some());
        assert!(!both.online_only);

        let online = super::key_params(&cfg("Online key service only"));
        assert!(online.key_url.is_some());
        assert!(online.online_only, "Online-only must set online_only");
    }
}

/// The routing and per-title wiring decisions a rip makes before it touches a
/// drive.
///
/// Every function below is pure over a `&str` or a `&RipRequest`, and every one
/// of them could be replaced wholesale — `-> true`, `-> ""`,
/// `-> Default::default()` — with the whole suite still green. The consequences
/// are not cosmetic: `is_disc_source` forced false sends a live-drive rip down
/// the ISO path, `stream_selection_for` defaulted discards every track the user
/// ticked, and a missing `title_index` muxes the wrong title under the right
/// filename.
#[cfg(test)]
mod routing_tests {
    use super::{
        DiscPlan, KeyConfig, OutKind, RipRequest, TitleIdentity, damage_note, demux_needs_subdirs,
        disc_device, fe, image_or_dir_scheme, is_disc_source, is_stream_source, mux_opts, out_kind,
        recovery_plan, recovery_produced_no_data, recovery_raw, remap_against,
        should_delete_staging_iso, source_scheme, stream_selection_for, title_input_options,
        title_session_mux_opts, verify_selection_identity, verify_title_identity, won_from_trace,
    };

    // ── The recovery job's `raw` flag ──────────────────────────────────────
    //
    // `multipass_rip` refuses a real sweep-plus-patch plan with `raw = false`.
    // The GUI passed `req.raw`, which `ui::raw_applies` forces to false for any
    // title output, so the SHIPPED DEFAULTS could not rip from a drive at all.

    /// The exact combination a fresh install produces: rip mode "Multi-pass",
    /// 5 passes, raw off, output "Selected titles → MKV". This is the test
    /// that would have caught it.
    #[test]
    fn the_shipped_defaults_produce_a_recovery_the_engine_accepts() {
        let multipass = crate::ui::wants_multipass("Multi-pass", 5);
        let want_iso = matches!(out_kind("Selected titles → MKV"), OutKind::IsoImage);
        let user_raw = crate::ui::raw_applies(false, want_iso);
        assert!(multipass, "the default rip mode is a multipass plan");
        assert!(!user_raw, "raw does not apply to a title output");

        let raw = recovery_raw(multipass, want_iso, user_raw)
            .expect("the default settings must produce a runnable recovery");
        assert!(
            raw,
            "a multipass recovery must be raw — the engine refuses it otherwise,              and the refusal is what every default live-drive rip hit"
        );
        // The engine's own gate, applied to what we just produced.
        assert!(
            !(fe::plan_passes(5).multipass && !raw),
            "this is the exact condition multipass_rip returns              multipass_requires_raw for"
        );
    }

    /// The three seams inside `run_disc` / `run_blocking` that no test can
    /// reach: both need a live drive or a real disc image, and they are
    /// private. Round 1 fixed the label-into-a-path defect at four seams and
    /// pinned only the three exported helpers, so reverting either of the two
    /// destination lines below left `cargo test --tests` fully green — and the
    /// same was true of the `raw` flag whose absence broke every default rip.
    ///
    /// Source pins, in the shape `autorip`'s handler pin uses. Each anchor is
    /// `expect`ed, never defaulted: an anchor that stopped matching would
    /// otherwise silently widen the slice and start reading a neighbouring
    /// function.
    #[test]
    fn the_private_disc_seams_still_go_through_their_guards() {
        let src = include_str!("engine.rs").replace("\r\n", "\n");
        let slice = |from: &str, to: &str| -> String {
            let a = src
                .find(from)
                .unwrap_or_else(|| panic!("anchor missing: {from}"));
            let b = src[a..]
                .find(to)
                .unwrap_or_else(|| panic!("closing anchor missing: {to}"));
            src[a..a + b].to_string()
        };

        // 1. The recovery job's raw flag — the defect that made every rip on
        //    the shipped defaults fail before reading a sector.
        let recover = slice(
            "        let mut job = fe::Job::new(format!(\"disc://",
            "        let result = fe::multipass_rip(",
        );
        assert!(
            recover.contains("recovery_raw(req.multipass, want_iso, req.raw)"),
            "the recovery job must take its raw flag from recovery_raw; \
             passing req.raw straight through is what multipass_rip refuses"
        );

        // 1b. The selection is re-resolved by identity before the staging mux.
        //     Reached only after a completed recovery, so nothing else can
        //     see it.
        let mux = slice(
            "        // Re-resolve the selection against the staged image",
            "        let mux = run_blocking(&iso_req, sink, state);",
        );
        assert!(
            mux.contains("remap_titles_by_identity(&iso_path, &req.titles"),
            "the staging mux must re-resolve the user's titles against the \
             recovered image; positions alone address a different title once \
             damage has removed a playlist"
        );

        // 2. The drive -> ISO destination (the most-travelled label seam).
        let dest = slice(
            "        let iso_path = std::path::Path::new(&req.dest_dir)",
            "        session.stage_drive_as_reader();",
        );
        assert!(
            dest.contains("sanitize_label(&label)"),
            "the drive -> ISO destination must sanitise the disc label"
        );

        // 3. The image-decrypt destination.
        let img = slice(
            "            let dest =\n                std::path::Path::new(&req.dest_dir)",
            "            // Never write over the source.",
        );
        assert!(
            img.contains("sanitize_label(&label)"),
            "the image-decrypt destination must sanitise the disc label"
        );

        // 4. The live-drive per-title mux options. `title_session_mux_opts`
        //    is unit-tested, but nothing else can see WHICH title index the
        //    loop hands it: passing a constant, or hoisting one MuxOptions
        //    out of the loop again, is the original defect and leaves every
        //    other test green.
        // 5. The live-drive re-scan's identity check. `verify_title_identity`
        //    is unit-tested, but the ONLY thing that makes it matter is that
        //    the per-title loop calls it between its fresh scan and the mux.
        //    Deleting the call leaves every other test green while every rip
        //    goes back to trusting a position across two scans.
        let rescan = slice(
            "        // This is a DIFFERENT scan from the one the selection was made against.",
            "        session.stage_drive_as_reader();",
        );
        assert!(
            rescan.contains("verify_title_identity(picked_ids.get(idx), &rescanned, idx)"),
            "the live-drive loop must confirm the title still at this index is \
             the one that was picked, against the identities banked from the \
             FIRST scan; without it an integer is carried across two scans"
        );
        assert!(
            rescan.contains("return Err(std::io::Error::other(msg));"),
            "the identity check's verdict must stop the title — an ignored \
             Err leaves the wrong-title mux running"
        );
        // 5b. And it must SAY why. The verdict names both playlists; the
        //     engine's `RipOutcome::Failed` carries only an ErrorKind, and the
        //     message has no `E<digits>` prefix for `error_code`/`explain` to
        //     pick up, so `describe_failure` renders the catch-all "Write
        //     failed (Other)." Returning through `?` alone skips the `Err(e)`
        //     arm below — the only thing that puts a per-title reason in the
        //     log pane — and the whole two-playlist diagnosis is discarded.
        assert!(
            // Matched on the push alone, not the whole lock expression: the
            // latter is one rustfmt decision away from being wrapped across
            // lines, at which point a `contains` on the single-line form fails
            // for a reason that has nothing to do with the behaviour being
            // pinned. (It just did, when the lock gained poison recovery.)
            rescan.contains(".push(msg.clone());"),
            "the identity mismatch must reach the log pane: propagating it \
             through `?` alone reduces the wrong-title diagnosis to \
             \"Write failed (Other).\""
        );
        // 5c. The drive bring-up two statements above returns through the same
        //     `?` and loses its reason the same way — "no disc in the drive"
        //     also arrives as "Write failed (Other)." Same closure, same
        //     bypass, so it gets the same treatment.
        let bringup = slice(
            "        let (mut session, _trace) = match fe::open_scan_resolve(",
            "        // This is a DIFFERENT scan",
        );
        assert!(
            bringup.contains(".push(msg.clone());"),
            "a failed per-title drive bring-up must say why in the log pane"
        );

        let session_mux = slice(
            "        // Session arm reads selection from MuxOptions",
            "        match fe::mux_title_session(",
        );
        assert!(
            session_mux.contains("title_session_mux_opts(req, idx)"),
            "the live-drive loop must build its MuxOptions for THIS title; a \
             selection built once before the loop is the union, and writes \
             tracks the user unticked under this title"
        );
    }

    // ── "Stop & Quit" must stop before it quits ───────────────────────────
    //
    // `Cmd::Cancel` signals and returns; the worker observes the flag at its
    // next boundary and drops the sink, which is what closes the partial file.
    // The AppKit shell used to answer `TerminateNow` immediately, so the
    // process could be torn down mid-write.

    #[test]
    fn a_quit_waits_for_a_worker_that_is_still_winding_down() {
        let run = std::sync::Arc::new(super::RunState::default());
        let worker = run.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            worker
                .finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let start = std::time::Instant::now();
        assert!(
            super::await_worker_exit(&run, std::time::Duration::from_secs(5)),
            "the worker finished well inside the grace period and the wait \
             must report that it did"
        );
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(100),
            "it returned before the worker was done — nothing was waited for"
        );
    }

    #[test]
    fn a_wedged_worker_does_not_turn_quit_into_a_hang() {
        let run = super::RunState::default();
        let start = std::time::Instant::now();
        assert!(
            !super::await_worker_exit(&run, std::time::Duration::from_millis(80)),
            "a worker that never finishes must be reported as not finished"
        );
        let waited = start.elapsed();
        assert!(waited >= std::time::Duration::from_millis(80), "{waited:?}");
        assert!(
            waited < std::time::Duration::from_secs(2),
            "the wait must end at its deadline, not linger: {waited:?}"
        );
    }

    /// A worker that had already finished is not waited for at all.
    #[test]
    fn a_finished_worker_is_not_waited_for() {
        let run = super::RunState::default();
        run.finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let start = std::time::Instant::now();
        assert!(super::await_worker_exit(&run, super::QUIT_GRACE));
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "quitting after a finished rip must be instant"
        );
    }

    /// The selection the user made is checked against the scan the RIP takes.
    ///
    /// `run_disc` opens a brand-new session and reads `disc.titles` again, then
    /// feeds the ticked NUMBERS straight into `resolve_selection`. Every LATER
    /// re-scan in this file is identity-checked; this first one — the one with
    /// the longest window, an operator reviewing the tree before pressing
    /// Start — was not. A source pin because reaching it needs a live drive.
    #[test]
    fn the_drive_rip_checks_the_selection_against_the_scan_it_was_made_on() {
        let src = include_str!("engine.rs").replace("\r\n", "\n");
        let start = src
            .find("\nfn run_disc(")
            .expect("run_disc definition present");
        let end = start
            + src[start..]
                .find("\n    // Decrypted folder:")
                .expect("the folder branch still ends the scan section");
        let body = &src[start..end];
        let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("verify_selection_identity(&req.titles, &req.title_ids,"),
            "the fresh scan must be checked against the identities the ticked \
             numbers referred to, BEFORE any branch resolves them"
        );
    }

    /// The IMAGE path re-scans too, and the same selection has to survive it.
    ///
    /// `run_blocking` is `run_disc`'s sibling for `iso://`/folder sources: the
    /// tree the user ticked came from `Ui::open`'s scan, and this function
    /// scans the source again at Run time. A file can be replaced, re-authored
    /// or re-mounted in between, and the ticked integers would be resolved
    /// against the new list with nothing comparing the two.
    #[test]
    fn the_image_rip_checks_the_selection_against_the_scan_it_was_made_on() {
        let src = include_str!("engine.rs").replace("\r\n", "\n");
        let start = src
            .find("\nfn run_blocking(")
            .expect("run_blocking definition present");
        let end = start
            + src[start..]
                .find("\n    let indices = fe::resolve_selection(&disc, &sel);")
                .expect("the selection is still resolved in run_blocking");
        let body = &src[start..end];
        // Whitespace-stripped, not whitespace-collapsed: rustfmt splits this
        // call across lines, and a pin that fails for a line break rather than
        // a behaviour change is a pin that gets deleted.
        let dense: String = body.split_whitespace().collect();
        assert!(
            dense.contains("verify_selection_identity(&req.titles,&req.title_ids,"),
            "the image path resolves ticked numbers against a scan nobody \
             compared to the one they were ticked on"
        );
    }

    /// The GUI's image decrypt must ask the SAME "is the destination the
    /// source?" question the CLI asks, through the ONE definition of it.
    ///
    /// This arm answered it with canonical-path equality alone, which cannot
    /// see a HARDLINK: two names for one inode are each already canonical, so
    /// the paths differ while the bytes are shared, the guard stays silent, and
    /// `recover_to_iso` truncates the user's only copy while the scan is still
    /// reading it. The CLI's `pipe::same_file` was hardened against exactly
    /// that; a second, narrower definition beside this call site is the bug.
    ///
    /// A source pin because the arm needs a real image and a real destination
    /// to reach — the same reason the seams above are pinned this way.
    #[test]
    fn the_gui_image_decrypt_asks_the_shared_same_file_question() {
        let src = include_str!("engine.rs").replace("\r\n", "\n");
        let start = src
            .find("\n        OutKind::IsoImage => {")
            .expect("the ISO-image arm is still there");
        let end = start
            + src[start..]
                .find("\n            let result = fe::recover_to_iso(")
                .expect("the decrypt call still closes the arm's setup");
        let body = &src[start..end];
        assert!(
            body.contains("same_file("),
            "the image-decrypt arm must decide through the shared same_file \
             guard, which catches a hardlinked destination too"
        );
        assert!(
            !body.contains("canonicalize("),
            "a canonical-path comparison beside the call site is the second \
             definition of file identity that let the GUI diverge from the CLI"
        );
    }

    /// Every GUI site that grades a finished mux must grade the LOSS too.
    ///
    /// `MuxOutcome::completed` is not the whole answer: the library's contract
    /// on `undelivered_streams` is explicit that a non-empty list means the
    /// file does not match the pre-mux plan **even with `completed = true`**,
    /// and that a caller reporting a successful export must report these too.
    /// The CLI does, and both now render through the shared `lossy::lossy_lines`
    /// (the CLI's own `print_undelivered_streams` was folded into it, which is
    /// what `lossy.rs`'s module doc records). These four sites did
    /// not, so a GUI MP4 export missing an audio track read as "Finished".
    ///
    /// A source pin because all four sit inside closures that need a live
    /// drive or a real disc image: `lossy_lines` is unit-tested on its own, but
    /// nothing else can see whether these arms ever call it — and each one that
    /// stopped calling it would leave every other test green.
    #[test]
    fn every_gui_mux_site_reports_the_streams_it_could_not_deliver() {
        let src = include_str!("engine.rs").replace("\r\n", "\n");
        let slice = |from: &str, to: &str| -> String {
            let a = src
                .find(from)
                .unwrap_or_else(|| panic!("anchor missing: {from}"));
            let b = src[a..]
                .find(to)
                .unwrap_or_else(|| panic!("closing anchor missing: {to}"));
            src[a..a + b].to_string()
        };

        // 1 + 2. The single-file/container conversion (`run_stream`): both the
        //        line it pushes and the summary it returns.
        let stream = slice(
            "    .map_err(|e| format!(\"convert failed: {e}\"))?;",
            "/// Whether a demux rip must give each title its own subdirectory.",
        );
        assert!(
            stream.contains("lossy_lines(&o, &target)"),
            "the stream conversion must report everything the mux lost — the \
             tracks the sink dropped AND the payload bytes it could not carry"
        );
        // Whitespace-collapsed: the call spans several lines once rustfmt has
        // had it, and the pin is about the ARGUMENTS, not the line breaks.
        let flat: String = stream.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("summarize_stream(&o, &target, &req.dest_dir)"),
            "the stream conversion's SUMMARY must grade the whole outcome, not \
             `completed` alone (true for a lossy export) and not a hand-picked \
             pair of its fields (which is how the byte loss went unread)"
        );

        // 3. The ISO/image per-title loop.
        let iso_loop = slice(
            "        match fe::mux_title(source_url, &dest_url, input, &mux, hint, sink) {",
            "            Err(e) => {",
        );
        assert!(
            iso_loop.contains("lossy_lines(&o, &target)"),
            "the ISO per-title loop must report everything the mux lost"
        );

        // 4. The live-drive per-title loop.
        let disc_loop = slice(
            "        match fe::mux_title_session(&mut session, idx, &dest_url, &opts, hint, sink) {",
            "            Err(e) => {",
        );
        assert!(
            disc_loop.contains("lossy_lines(&o, &target)"),
            "the live-drive per-title loop must report everything the mux lost"
        );
    }

    // ── A stream selection is PER TITLE ───────────────────────────────────
    //
    // `ticked_streams` unions every title's ticked PIDs, and that one union
    // was applied to every title in the mux loop. Blu-ray playlists of one
    // feature routinely share PIDs, so unticking a commentary under title 1
    // did nothing while the same PID stayed ticked under title 2 — the track
    // was written to BOTH outputs, and the tree showed otherwise.

    fn req_with_title_pids(pids: Vec<(usize, Vec<u16>, Vec<u16>)>) -> RipRequest {
        RipRequest {
            explicit_streams: true,
            audio_pids: vec![0x1100, 0x1101],
            sub_pids: vec![0x1200],
            title_pids: crate::engine::TitleStreams::PerTitle(pids),
            ..req()
        }
    }

    /// The defect: one PID ticked under one title and not the other.
    #[test]
    fn a_pid_unticked_under_one_title_is_not_written_for_that_title() {
        let r = req_with_title_pids(vec![
            // Title 0 keeps both audio tracks.
            (0, vec![0x1100, 0x1101], vec![0x1200]),
            // Title 1 has the commentary unticked.
            (1, vec![0x1100], vec![0x1200]),
        ]);
        let only = |sel: &libfreemkv::StreamSelection| match &sel.audio {
            libfreemkv::PidFilter::Only(v) => v.clone(),
            _ => panic!("an explicit selection must be a PidFilter::Only"),
        };
        assert_eq!(
            only(&stream_selection_for(&r, Some(0))),
            vec![0x1100, 0x1101]
        );
        assert_eq!(
            only(&stream_selection_for(&r, Some(1))),
            vec![0x1100],
            "the commentary was unticked for title 1; the union would have \
             written it anyway because title 0 still has it"
        );
    }

    /// The same defect on the LIVE-DRIVE path, which the ISO fix did not
    /// reach: that path muxes from a `DiscSession`, and the Session arm takes
    /// its selection from `MuxOptions` rather than `InputOptions`, so it built
    /// one `MuxOptions` from the union before the loop and reused it for every
    /// title.
    ///
    /// The expectation below is the user's TICKS written out by hand — 0x1101
    /// is ticked under title 0 and unticked under title 1 — not anything
    /// `stream_selection*` returns. (`per_title_input_options_…` compared
    /// `input.selection` against the union (`stream_selection` as it then was),
    /// i.e. the union against the
    /// union, and so passed for as long as the defect shipped.)
    #[test]
    fn a_live_drive_rip_applies_each_title_s_own_ticks() {
        let r = req_with_title_pids(vec![
            // The user ticked both audio tracks and the subtitle under title 0…
            (0, vec![0x1100, 0x1101], vec![0x1200]),
            // …and unticked the commentary (0x1101) under title 1.
            (1, vec![0x1100], vec![0x1200]),
        ]);
        let audio_of = |idx: usize| match title_session_mux_opts(&r, idx).selection.audio {
            libfreemkv::PidFilter::Only(v) => v,
            _ => panic!("an explicit selection must be a PidFilter::Only"),
        };
        assert_eq!(
            audio_of(0),
            vec![0x1100, 0x1101],
            "title 0 keeps the commentary the user left ticked"
        );
        assert_eq!(
            audio_of(1),
            vec![0x1100],
            "title 1 must NOT get 0x1101: the user unticked it there, and the \
             union kept it only because title 0 still has it"
        );
        // And the rest of the drive-path options are untouched by this.
        let base = mux_opts(&r);
        let o = title_session_mux_opts(&r, 1);
        assert_eq!(o.raw, base.raw);
        assert_eq!(o.batch_sectors, base.batch_sectors);
        assert_eq!(o.skip_errors, base.skip_errors);
        assert_eq!(o.send_deadline, base.send_deadline);
    }

    /// A request that says it has no per-title breakdown — the CLI, the
    /// container path, the `FMKV_APIDS` harness — falls back to the union for
    /// EVERY title, which is exactly what those callers did before the
    /// breakdown existed. This is the regression the per-title fix could
    /// plausibly cause, so it is asserted directly against
    /// `TitleStreams::Unspecified` rather than against an empty list.
    #[test]
    fn without_a_per_title_breakdown_the_union_still_applies() {
        let r = RipRequest {
            title_pids: crate::engine::TitleStreams::Unspecified,
            ..req_with_title_pids(Vec::new())
        };
        for t in [None, Some(0), Some(7)] {
            let s = stream_selection_for(&r, t);
            match (&s.audio, &s.subtitle) {
                (libfreemkv::PidFilter::Only(a), libfreemkv::PidFilter::Only(b)) => {
                    assert_eq!(a, &[0x1100, 0x1101], "the union must reach title {t:?}");
                    assert_eq!(b, &[0x1200], "the union must reach title {t:?}");
                }
                _ => panic!("explicit selection expected"),
            }
        }
    }

    /// MEANING CHANGED by the absence/empty split. This used to read "a title
    /// absent from the breakdown is one the user emptied, so fall back to the
    /// union" — which was the defect itself. Under `TitleStreams::PerTitle` an
    /// emptied title carries an EMPTY ENTRY, so the only titles still absent
    /// are ones with no selectable stream rows at all (a video-only title) or
    /// ones that do not exist. Those genuinely say nothing, and the union — a
    /// filter over PIDs such a title does not have — is the harmless answer.
    #[test]
    fn a_title_the_breakdown_never_mentions_falls_back_to_the_union() {
        let r = req_with_title_pids(vec![(0, vec![0x1100], vec![])]);
        match &stream_selection_for(&r, Some(9)).audio {
            libfreemkv::PidFilter::Only(v) => assert_eq!(v, &[0x1100, 0x1101]),
            _ => panic!("explicit selection expected"),
        }
        // But a title the breakdown DOES mention with an empty list is not
        // "absent": it is the user keeping nothing.
        let emptied = req_with_title_pids(vec![(0, vec![0x1100], vec![]), (1, vec![], vec![])]);
        match &stream_selection_for(&emptied, Some(1)).audio {
            libfreemkv::PidFilter::Only(v) => assert!(
                v.is_empty(),
                "an empty entry is a decision, not a missing one"
            ),
            _ => panic!("explicit selection expected"),
        }
    }

    /// Two titles of one feature that SHARE their PIDs — the ordinary Blu-ray
    /// shape (a feature and its extended cut). Title 0 keeps everything; the
    /// user unticks EVERY stream row under title 1.
    ///
    /// Every expectation below is the user's ticks written out as a literal.
    /// Nothing here asks `ticked_streams_by_title` or `stream_selection_for`
    /// what it thinks the answer is.
    fn shared_pid_disc() -> crate::engine::Scanned {
        use crate::engine::{Row, Scanned};
        let mk = |ty: &str, ti: usize, pid: Option<u16>| Row {
            type_s: ty.into(),
            desc: format!("{ty} of title {ti}"),
            depth: if ty == "Title" { 1 } else { 2 },
            checkable: ty != "Video",
            title: ti,
            info: String::new(),
            pid,
            duration_secs: if ty == "Title" { 5400.0 } else { 0.0 },
            lang: String::new(),
            forced: false,
        };
        let mut rows = vec![Row {
            depth: 0,
            checkable: false,
            title: usize::MAX,
            ..mk("Bluray disc", usize::MAX, None)
        }];
        for ti in 0..2 {
            rows.push(mk("Title", ti, None));
            rows.push(mk("Video", ti, None));
            // The SAME pids under both titles — that sharing is what makes the
            // union indistinguishable from title 0's own selection.
            rows.push(mk("Audio", ti, Some(0x1100)));
            rows.push(mk("Audio", ti, Some(0x1101)));
            rows.push(mk("Subtitles", ti, Some(0x1200)));
        }
        Scanned {
            label: "SHARED".into(),
            volume_id: "SHARED".into(),
            rows,
            key_summary: String::new(),
            title_count: 2,
            video_codecs: vec!["H.264".into(); 2],
            title_ids: Vec::new(),
            details: vec![],
        }
    }

    /// The hole commit 8f9a31c left: `ticked_streams_by_title` skipped an
    /// unticked row BEFORE creating its title's slot, so a title the user
    /// emptied never appeared in the breakdown at all — and
    /// `stream_selection_for` then fell back to the UNION, writing the sibling
    /// title's tracks into the very title the user had cleared.
    #[test]
    fn a_title_the_user_emptied_rips_no_streams_at_all() {
        let sc = shared_pid_disc();
        let t =
            crate::ui::Tree::from_scan(&sc, "All titles", 0.0, &crate::ui::LangPrefs::default());
        // The user clears every stream row under title 1 and touches nothing
        // under title 0.
        let rows: Vec<usize> = t
            .arena
            .iter()
            .enumerate()
            .filter(|(_, n)| n.title_idx == 1 && n.pid.is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(rows.len(), 3, "fixture must give title 1 three stream rows");
        for i in rows {
            t.set_checked(i, false);
        }

        let (audio_pids, sub_pids, explicit) = t.ticked_streams();
        assert!(explicit, "clearing rows must read as an explicit narrowing");
        let r = RipRequest {
            explicit_streams: explicit,
            audio_pids,
            sub_pids,
            title_pids: t.ticked_streams_by_title(),
            ..req()
        };
        let sel = |ti: usize| {
            let s = stream_selection_for(&r, Some(ti));
            match (s.audio, s.subtitle) {
                (libfreemkv::PidFilter::Only(a), libfreemkv::PidFilter::Only(b)) => (a, b),
                _ => panic!("an explicit selection must be a PidFilter::Only"),
            }
        };
        assert_eq!(
            sel(0),
            (vec![0x1100u16, 0x1101], vec![0x1200u16]),
            "title 0 was left alone and must keep exactly what is ticked there"
        );
        assert_eq!(
            sel(1),
            (Vec::<u16>::new(), Vec::<u16>::new()),
            "the user cleared every row under title 1: it must rip NO audio \
             and NO subtitles. Falling back to the union writes title 0's \
             shared tracks into title 1 anyway."
        );
        // The live-drive path must agree — it reads its selection from
        // MuxOptions, not InputOptions.
        match title_session_mux_opts(&r, 1).selection.audio {
            libfreemkv::PidFilter::Only(v) => {
                assert!(v.is_empty(), "the drive path kept tracks title 1 cleared")
            }
            _ => panic!("an explicit selection must be a PidFilter::Only"),
        }
    }

    /// And "made no choice at all" still means keep everything, per title.
    #[test]
    fn an_untouched_selection_keeps_every_stream_for_every_title() {
        let r = RipRequest {
            explicit_streams: false,
            title_pids: crate::engine::TitleStreams::PerTitle(vec![(0, vec![0x1100], vec![])]),
            ..req()
        };
        assert!(stream_selection_for(&r, Some(0)).is_all());
        assert!(stream_selection_for(&r, None).is_all());
    }

    // ── A selection is titles, not numbers ────────────────────────────────
    //
    // The multipass path muxes from a RECOVERED image, which is scanned again.
    // If damage destroyed a playlist the second list is shorter, and every
    // number past the gap addresses a different title — so the wrong film was
    // written under the name the user asked for, reported as "1 title(s)
    // written". `remap_titles_by_identity` re-resolves by what the numbers
    // referred to; these tests drive it directly, since reaching it needs a
    // live drive and a two-hour recovery.

    /// One scanned title, addressed by its playlist name and the SECTORS it is
    /// read from. `duration_secs` and `size_bytes` are deliberately IDENTICAL
    /// across every fixture here: titles legitimately carry duplicate playlists
    /// with identical duration and size, so neither may be part of the
    /// identity, and a fixture that varied them would let a name+duration
    /// identity pass these tests.
    fn id(playlist: &str, start_lba: u32) -> TitleIdentity {
        TitleIdentity::of(&libfreemkv::DiscTitle {
            playlist: playlist.to_string(),
            playlist_id: playlist
                .trim_end_matches(".mpls")
                .parse::<u16>()
                .unwrap_or(0),
            duration_secs: 7530.0,
            size_bytes: 20 << 30,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![libfreemkv::Extent {
                start_lba,
                sector_count: 1000,
            }],
            content_format: libfreemkv::ContentFormat::BdTs,
            codec_privates: Vec::new(),
        })
    }

    /// The whole point: a title that MOVED is followed to its new index.
    #[test]
    fn a_selection_follows_its_titles_when_the_rescan_renumbers_them() {
        // Drive scan: [feature, extra, trailer]; the user picked 0 and 2.
        // `ids` is the whole scan, indexed by title number — the extra is in
        // it even though nobody picked it, because that is what an index means.
        let picked = vec![0usize, 2];
        let ids = vec![
            id("00800.mpls", 1000),
            id("00001.mpls", 5000),
            id("00003.mpls", 13000),
        ];
        // The recovered image lost 00001.mpls, so everything after it shifts.
        let staged = vec![id("00800.mpls", 1000), id("00003.mpls", 13000)];
        assert_eq!(
            remap_against(&picked, &ids, &staged),
            Ok(vec![0, 1]),
            "the trailer moved from index 2 to 1 and must be followed"
        );
    }

    /// Duplicate playlist names are legitimate on real discs, and so are
    /// duplicate DURATIONS — the project's rules say so outright. What tells
    /// the pair apart is the sectors each is read from; matching on the name
    /// alone would pick the first of the pair.
    #[test]
    fn duplicate_playlist_names_are_told_apart_by_their_sectors() {
        let picked = vec![1usize];
        let ids = vec![id("00800.mpls", 1000), id("00800.mpls", 9000)];
        let staged = vec![id("00800.mpls", 1000), id("00800.mpls", 9000)];
        assert_eq!(remap_against(&picked, &ids, &staged), Ok(vec![1]));
    }

    // ── The case the project's own rule names ─────────────────────────────
    //
    // "Titles legitimately carry duplicate playlists with identical duration
    // and size; the index and name are what tell them apart." An identity of
    // name + duration therefore cannot separate the pair the rule describes —
    // duration is precisely the field that legitimately collides. The SECTORS
    // can: two titles that read the same sectors from the same playlist are
    // byte-identical, so there is nothing left to confuse.
    //
    // Both fixtures below are DiscTitle literals differing in exactly one
    // field, `extents[0].start_lba`. Everything a name+duration identity looks
    // at is deliberately identical.

    /// One of a legitimately duplicated pair: same playlist name, same
    /// playlist id, same duration, same size — read from different sectors.
    fn dup_title(start_lba: u32) -> libfreemkv::DiscTitle {
        libfreemkv::DiscTitle {
            playlist: "00800.mpls".to_string(),
            playlist_id: 800,
            duration_secs: 7530.0,
            size_bytes: 20 << 30,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![libfreemkv::Extent {
                start_lba,
                sector_count: 1000,
            }],
            content_format: libfreemkv::ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }
    }

    /// The GUI's remap path must follow the title the user actually picked,
    /// not the first playlist of the same name and length.
    #[test]
    fn a_duplicate_playlist_of_the_same_length_is_still_told_apart_by_its_sectors() {
        let first = TitleIdentity::of(&dup_title(1000));
        let second = TitleIdentity::of(&dup_title(9000));
        assert_ne!(
            first, second,
            "same name and duration, different sectors — not the same title"
        );

        // The user picked the SECOND of the pair (index 1). The staged scan
        // still lists both, in the same order, so the answer is the literal 1.
        assert_eq!(
            remap_against(
                &[1],
                &[first.clone(), second.clone()],
                &[first, second.clone()]
            ),
            Ok(vec![1]),
            "the second of a duplicate pair must remap to index 1, not to the \
             first playlist that happens to share its name and length"
        );
    }

    /// The GUI's live-drive verify must refuse the same pair swapped, instead
    /// of muxing the other half of it under the number the user asked for.
    #[test]
    fn a_duplicate_playlist_swapped_by_the_rescan_is_refused_not_muxed() {
        let first = TitleIdentity::of(&dup_title(1000));
        let second = TitleIdentity::of(&dup_title(9000));
        // The selection was made against a scan whose index 0 was `second`;
        // the rescan lists the pair the other way round.
        let rescan = vec![first, second.clone()];
        assert!(
            verify_title_identity(Some(&second), &rescan, 0).is_err(),
            "index 0 now names the other half of the duplicate pair; muxing it \
             under the picked number is the wrong-title write this guard exists \
             to stop"
        );
    }

    // ── The SINGLE-PASS drive path has the same seam, one scan later ──────
    //
    // `run_disc` resolves the selection against its own scan, then EVERY title
    // in the loop re-opens the drive and scans again — carrying only an
    // integer. `verify_title_identity` is what stops that integer from naming a
    // different title the second time round. Driving the real loop needs a live
    // drive, so these exercise the decision directly, exactly as the
    // `remap_against` tests above do.

    /// THE DEFECT: the rescan lists the same titles in a different order, so
    /// the index still resolves — to the wrong film.
    #[test]
    fn a_reordered_rescan_stops_the_mux_instead_of_writing_the_wrong_title() {
        let picked = [id("00800.mpls", 7530), id("00001.mpls", 300)];
        let rescan = vec![id("00001.mpls", 300), id("00800.mpls", 7530)];
        let e = verify_title_identity(picked.first(), &rescan, 0)
            .expect_err("index 0 now names 00001.mpls, not the picked feature");
        assert!(
            e.contains("00800.mpls") && e.contains("00001.mpls"),
            "the message must name both titles: {e}"
        );
    }

    /// A rescan that drops a title BEFORE the selected index leaves the index
    /// in range and pointing somewhere else.
    #[test]
    fn a_rescan_that_drops_an_earlier_title_is_refused_not_shifted() {
        let picked = [
            id("00800.mpls", 7530),
            id("00001.mpls", 300),
            id("00003.mpls", 120),
        ];
        let rescan = vec![id("00800.mpls", 7530), id("00003.mpls", 120)];
        assert!(verify_title_identity(picked.get(1), &rescan, 1).is_err());
    }

    /// THE NORMAL PATH: a stable disc rescans identically and every title is
    /// muxed exactly as before — including when an unrelated LATER title is
    /// missing from the second scan, which says nothing about this one.
    #[test]
    fn a_stable_rescan_passes_every_selected_title() {
        let picked = [id("00800.mpls", 7530), id("00001.mpls", 300)];
        let rescan = vec![
            id("00800.mpls", 7530),
            id("00001.mpls", 300),
            id("00003.mpls", 120),
        ];
        for idx in 0..picked.len() {
            assert_eq!(verify_title_identity(picked.get(idx), &rescan, idx), Ok(()));
        }
        let shorter = vec![id("00800.mpls", 7530), id("00001.mpls", 300)];
        assert_eq!(verify_title_identity(picked.get(1), &shorter, 1), Ok(()));
    }

    /// Nothing recorded for that index → nothing to disagree with, and the
    /// pre-existing behaviour is left exactly as it was.
    #[test]
    fn with_no_recorded_identity_the_index_is_used_as_before() {
        let rescan = vec![id("00800.mpls", 7530)];
        assert_eq!(verify_title_identity(None, &rescan, 0), Ok(()));
    }

    /// The rescan is shorter than the index: caught here rather than deeper in
    /// the mux, and named.
    #[test]
    fn a_title_missing_from_the_rescan_is_named() {
        let picked = [id("00800.mpls", 7530), id("00001.mpls", 300)];
        let e = verify_title_identity(picked.get(1), &picked[..1], 1).expect_err("must refuse");
        assert!(e.contains("00001.mpls"), "{e}");
    }

    /// The playlist name is on-disc metadata and this message is shown to the
    /// user, so it goes through the same display sanitiser everything else does.
    #[test]
    fn a_crafted_playlist_name_cannot_reach_the_ui_raw() {
        let picked = [id("\u{1b}c00800.mpls", 7530)];
        let rescan = vec![id("00001.mpls", 300)];
        let e = verify_title_identity(picked.first(), &rescan, 0).expect_err("must refuse");
        assert!(!e.contains('\u{1b}'), "ESC survived into the UI: {e:?}");
    }

    /// A title that is GONE is a hard error. Muxing the survivors quietly
    /// would deliver a subset under the same success summary.
    #[test]
    fn a_title_destroyed_by_the_damage_is_refused_not_skipped() {
        let picked = vec![0usize, 1];
        let ids = vec![id("00800.mpls", 7530), id("00001.mpls", 300)];
        let staged = vec![id("00800.mpls", 7530)];
        let e = remap_against(&picked, &ids, &staged).expect_err("must refuse");
        assert!(
            e.contains("00001.mpls") && e.contains("2"),
            "the message must name the title the user asked for: {e}"
        );
    }

    /// Nothing selected means `Selection::MainMovie` downstream — there is
    /// nothing to remap and the empty list must pass through untouched.
    #[test]
    fn an_empty_selection_is_left_alone() {
        assert_eq!(remap_against(&[], &[], &[]), Ok(vec![]));
    }

    /// A title with NO captured identity must cost only itself.
    ///
    /// The identities are indexed by canonical title number, exactly like
    /// `picked_ids` in the live-drive loop, so "nothing recorded for title 3"
    /// is `ids.get(3) == None` — a per-title answer. Keyed by SELECTION
    /// position instead, a list that was one entry short made the function
    /// return every raw position unchanged, so ONE unknown title silently put
    /// the whole batch back on the stale-position path this exists to close.
    #[test]
    fn an_uncaptured_title_does_not_disarm_the_rest_of_the_selection() {
        // The user picked all three titles; identities are known for the first
        // two only.
        let picked = vec![0usize, 1, 2];
        let ids = vec![id("00800.mpls", 1000), id("00001.mpls", 300)];
        // The recovered image lists the pair the other way round.
        let staged = vec![
            id("00001.mpls", 300),
            id("00800.mpls", 1000),
            id("00003.mpls", 13000),
        ];
        assert_eq!(
            remap_against(&picked, &ids, &staged),
            Ok(vec![1, 0, 2]),
            "the two titles WITH an identity must be followed to their new \
             positions; only the one with no identity falls back to its number"
        );
    }

    // ── The selection is made against a scan the rip never sees ────────────
    //
    // The GUI scans, draws the tree, and the user ticks a title — then reviews
    // streams and format before pressing Start. `run_disc` opens a BRAND NEW
    // scan at that point and resolves the ticked NUMBERS against it. That is
    // the same "position is not identity" seam the per-title loop already
    // guards, over the longest window in the product: swap the disc while the
    // operator reviews, and title 3 of the new disc is muxed and reported as
    // success under the name the old one earned.

    /// The disc changed under the selection: refused, and named.
    #[test]
    fn a_selection_made_against_an_earlier_scan_is_refused_when_the_disc_changed() {
        let when_ticked = vec![id("00800.mpls", 1000), id("00003.mpls", 13000)];
        // A different disc: title 0 is something else entirely.
        let fresh = vec![id("00001.mpls", 300), id("00003.mpls", 13000)];
        let e = verify_selection_identity(&[0], &when_ticked, &fresh)
            .expect_err("title 1 is not the title the user ticked");
        assert!(
            e.contains("00800.mpls") && e.contains("00001.mpls"),
            "the message must name both titles: {e}"
        );
    }

    /// The ordinary case must still rip: the same disc rescans identically, and
    /// a selection with nothing recorded behaves exactly as it did before.
    #[test]
    fn a_selection_that_still_matches_the_fresh_scan_is_allowed() {
        let when_ticked = vec![id("00800.mpls", 1000), id("00003.mpls", 13000)];
        let fresh = when_ticked.clone();
        assert_eq!(
            verify_selection_identity(&[0, 1], &when_ticked, &fresh),
            Ok(())
        );
        assert_eq!(
            verify_selection_identity(&[0, 1], &[], &fresh),
            Ok(()),
            "no identities captured is the pre-existing behaviour, untouched"
        );
    }

    /// A number that is out of range for the fresh scan is caught HERE, not
    /// silently dropped on the way to the mux.
    #[test]
    fn a_selected_number_the_fresh_scan_no_longer_has_is_refused() {
        let when_ticked = vec![id("00800.mpls", 1000), id("00003.mpls", 13000)];
        let fresh = vec![id("00800.mpls", 1000)];
        assert!(verify_selection_identity(&[1], &when_ticked, &fresh).is_err());
    }

    /// A single-pass recovery is an ordinary decrypting copy, so the user's
    /// setting stands — forcing raw there would hand back an encrypted image
    /// nobody asked for.
    #[test]
    fn a_single_pass_recovery_keeps_the_users_raw_setting() {
        assert_eq!(recovery_raw(false, true, false), Ok(false));
        assert_eq!(recovery_raw(false, true, true), Ok(true));
    }

    /// Whole disc → ISO, multipass, raw off: the user asked for a decrypted
    /// image and a multipass recovery cannot produce one. Refused up front,
    /// in words the user can act on, rather than after the drive is staged.
    #[test]
    fn a_decrypted_iso_from_a_multipass_recovery_is_refused_before_the_drive() {
        let e = recovery_raw(true, true, false).expect_err("this cannot be honoured");
        assert!(
            e.contains("Single pass") && e.contains("raw"),
            "the refusal must name both ways out, got: {e}"
        );
        // And the same request with raw ticked is allowed.
        assert_eq!(recovery_raw(true, true, true), Ok(true));
    }

    fn req() -> RipRequest {
        RipRequest {
            source: "/media/movie.iso".into(),
            dest_dir: "/out".into(),
            titles: vec![],
            title_ids: Vec::new(),
            format: "MKV".into(),
            audio_pids: vec![],
            sub_pids: vec![],
            title_pids: crate::engine::TitleStreams::Unspecified,
            explicit_streams: false,
            raw: false,
            force: false,
            filename_template: String::new(),
            decrypt_threads: 0,
            multipass: false,
            max_passes: 0,
            abort_lost_secs: 0,
            keep_iso: false,
            auto_eject: false,
            keys: KeyConfig::default(),
        }
    }

    /// A `disc://` source must route to the live-drive path and nothing else
    /// must. Forced `true` an ISO is opened as a drive; forced `false` a drive
    /// rip is handed to `scan_iso` with a path of `disc://`.
    #[test]
    fn only_a_disc_url_is_a_disc_source() {
        assert!(is_disc_source("disc://"));
        assert!(is_disc_source("disc:///dev/sr0"));
        assert!(!is_disc_source("/media/movie.iso"));
        assert!(!is_disc_source("iso:///media/movie.iso"));
        assert!(!is_disc_source(""));
        assert!(!is_disc_source("mkv:///x.mkv"));
    }

    /// Bare `disc://` means autodetect; `disc://<path>` means that drive. A
    /// `Some("")` here becomes `DeviceTarget::Path("")` and opens nothing.
    #[test]
    fn the_device_path_is_whatever_follows_the_scheme() {
        assert_eq!(disc_device("disc://"), None);
        assert_eq!(disc_device("disc:///dev/sr0").as_deref(), Some("/dev/sr0"));
        assert_eq!(
            disc_device("disc://\\\\.\\D:").as_deref(),
            Some("\\\\.\\D:")
        );
        // Not a disc URL at all — no prefix to strip, so no device.
        assert_eq!(disc_device("/media/movie.iso"), None);
    }

    /// Container sources skip the disc scan entirely. An ISO must NOT be one of
    /// them, or it goes to the single-title container path with no title list.
    #[test]
    fn container_extensions_are_stream_sources_and_iso_is_not() {
        for good in ["a.mkv", "a.m2ts", "a.mts", "a.mp4", "A.MKV", "/p/a.Mp4"] {
            assert!(is_stream_source(good), "{good} should be a stream source");
        }
        for bad in ["a.iso", "a.bin", "", "disc://", "/no/extension"] {
            assert!(!is_stream_source(bad), "{bad} must not be a stream source");
        }
    }

    /// The scheme half of the mux source URL. An extension we do not recognise
    /// falls back to `m2ts`, which is the raw-transport-stream reader.
    #[test]
    fn the_source_scheme_follows_the_extension() {
        assert_eq!(source_scheme("a.mkv"), "mkv");
        assert_eq!(source_scheme("a.MKV"), "mkv");
        assert_eq!(source_scheme("a.mp4"), "mp4");
        assert_eq!(source_scheme("a.iso"), "iso");
        assert_eq!(source_scheme("a.m2ts"), "m2ts");
        assert_eq!(source_scheme("a.mts"), "m2ts");
        assert_eq!(source_scheme(""), "m2ts");
    }

    /// `source_scheme` and `image_or_dir_scheme` answer DIFFERENT questions and
    /// were conflated twice while fixing folder support.
    ///
    /// `source_scheme` classifies a STREAM container by extension and falls
    /// through to m2ts, which is right only because its caller is guarded by
    /// `is_stream_source`. Using it for a disc image sent `Disc.img` to the mux
    /// as `m2ts://Disc.img`; using `image_or_dir_scheme` for a container would
    /// call an `.mkv` an ISO.
    #[test]
    fn image_or_dir_scheme_is_not_source_scheme() {
        // An image whose extension is not `.iso` must still be an image.
        for p in ["Disc.img", "Disc.bin", "Disc.udf", "Disc"] {
            assert_eq!(
                image_or_dir_scheme(p),
                "iso",
                "{p} is a disc image, not an elementary stream"
            );
            assert_eq!(
                source_scheme(p),
                "m2ts",
                "source_scheme falls through for {p} — which is why it must not be used here"
            );
        }
        // A real directory is dir://.
        let d = std::env::temp_dir();
        assert_eq!(image_or_dir_scheme(d.to_str().unwrap()), "dir");
    }

    /// The marker each `OutKind` is identified by in the sink tables below.
    fn sink_marker(k: OutKind) -> String {
        match k {
            OutKind::DecryptedFolder => "folder".to_string(),
            OutKind::IsoImage => "iso".to_string(),
            OutKind::Demux(scheme) => scheme.to_string(),
            OutKind::File(s) => s.to_string(),
        }
    }

    /// Each of the twelve picker strings resolves to its own sink. Six of them
    /// used to fall through to a per-title MKV mux, so this table is the thing
    /// that stops the user's chosen format quietly becoming a different one.
    #[test]
    fn every_picker_format_maps_to_its_own_sink() {
        let cases: &[(&str, &str)] = &[
            ("Whole disc → decrypted folder", "folder"),
            ("Whole disc → ISO image", "iso"),
            ("Each title → separate track files", "demux"),
            ("Each title → MP4 file", "mp4"),
            ("Each title → M2TS file", "m2ts"),
            ("Chapters only (XML)", "chapters"),
            ("Title index (JSON)", "json"),
            ("Title index (.fvi)", "fvi"),
            ("Each title → MKV file", "mkv"),
            // Anything unrecognised is a container mux, not a whole-disc sink.
            ("", "mkv"),
        ];
        for (format, want) in cases {
            let got = sink_marker(out_kind(format));
            assert_eq!(&got, want, "format {format:?} resolved to {got:?}");
        }
    }

    /// The CANONICAL picker strings — the exact `&'static str`s the shells put
    /// in the dropdown — each map to a distinct sink. The table above uses
    /// paraphrases, so it could not have caught a picker entry whose wording
    /// misses every `out_kind` branch and silently becomes an MKV mux. That is
    /// precisely how the three per-track-kind sinks would have failed.
    #[test]
    fn the_real_picker_strings_each_reach_their_own_sink() {
        let want: &[(&str, &str)] = &[
            ("Selected titles → MKV", "mkv"),
            ("Selected titles → MP4", "mp4"),
            ("Selected titles → M2TS", "m2ts"),
            ("Selected titles → separate track files", "demux"),
            ("Selected titles → video tracks only", "video"),
            ("Selected titles → audio tracks only", "audio"),
            ("Selected titles → subtitle tracks only", "sub"),
            ("Whole disc → ISO image", "iso"),
            ("Whole disc → decrypted folder", "folder"),
            ("Chapters → file", "chapters"),
            ("Title info → JSON", "json"),
            ("Video index → .fvi", "fvi"),
        ];
        let offered: Vec<&str> = crate::ui::output_formats(true, true)
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(
            offered.len(),
            want.len(),
            "the picker gained or lost an entry without a sink mapping: {offered:?}"
        );
        for (format, marker) in want {
            assert!(offered.contains(format), "{format:?} is no longer offered");
            assert_eq!(
                &sink_marker(out_kind(format)),
                marker,
                "{format:?} resolved to the wrong sink"
            );
        }
        // Distinct sinks: no two picker entries may collapse onto one.
        let mut markers: Vec<String> = offered.iter().map(|f| sink_marker(out_kind(f))).collect();
        markers.sort();
        let n = markers.len();
        markers.dedup();
        assert_eq!(n, markers.len(), "two picker entries share a sink");
    }

    /// The three per-track-kind entries must build a DIRECTORY dest URL under
    /// their own scheme, never a `<scheme>` file extension. `video://out/x.video`
    /// is not a thing; `video://out/` is.
    #[test]
    fn per_track_kind_sinks_are_directory_urls() {
        for (format, scheme) in [
            ("Selected titles → video tracks only", "video"),
            ("Selected titles → audio tracks only", "audio"),
            ("Selected titles → subtitle tracks only", "sub"),
        ] {
            match out_kind(format) {
                OutKind::Demux(s) => assert_eq!(s, scheme),
                other => panic!("{format:?} is {:?}, not a demux sink", sink_marker(other)),
            }
        }
    }

    /// The dest-URL SCHEME a picker string produces, as libfreemkv would parse
    /// it. `sink_marker` is an internal label; this is the thing the CLI would
    /// have been given on the command line, which is what parity is measured
    /// against. Only `folder` differs: it is the CLI's `dir://`.
    fn dest_scheme(format: &str) -> String {
        match sink_marker(out_kind(format)).as_str() {
            "folder" => "dir".to_string(),
            other => other.to_string(),
        }
    }

    /// PARITY: for each source kind, the picker offers exactly the sinks the
    /// CLI supports for that source — no more, no fewer.
    ///
    /// This is the invariant, not the row count: the GUI mirrors the CLI per
    /// source kind. It failed silently for three sinks (`video://`, `audio://`,
    /// `sub://`) because nothing compared the two lists — the picker was only
    /// ever checked against itself. Extending the CLI without extending the
    /// picker now trips here rather than leaving a library sink with no GUI.
    #[test]
    fn the_picker_offers_exactly_the_cli_sinks_for_each_source_kind() {
        use std::collections::BTreeSet;

        // Title-level sinks: every one applies to any source that has titles,
        // container included.
        let per_title: &[&str] = &[
            "mkv", "m2ts", "demux", "video", "audio", "sub", "chapters", "json", "fvi",
        ];
        // Whole-disc sinks: `iso://` as a dest needs a physical disc to read,
        // `dir://` needs a disc file tree. Neither exists for a container.
        let whole_disc: &[&str] = &["iso", "dir"];

        for (disc_source, mp4_ok) in [(true, true), (true, false), (false, true), (false, false)] {
            let mut want: BTreeSet<String> = per_title.iter().map(|s| s.to_string()).collect();
            if mp4_ok {
                want.insert("mp4".to_string());
            }
            if disc_source {
                want.extend(whole_disc.iter().map(|s| s.to_string()));
            }

            let got: BTreeSet<String> = crate::ui::output_formats(disc_source, mp4_ok)
                .into_iter()
                .flatten()
                .map(dest_scheme)
                .collect();

            assert_eq!(
                got,
                want,
                "disc_source={disc_source} mp4_ok={mp4_ok}: picker sinks diverge from the CLI's\
                 \n  picker-only: {:?}\n  cli-only: {:?}",
                got.difference(&want).collect::<Vec<_>>(),
                want.difference(&got).collect::<Vec<_>>(),
            );

            // Each scheme must be one libfreemkv actually recognizes — a typo'd
            // row would otherwise agree with a typo'd expectation above.
            for scheme in &got {
                let url = format!("{scheme}://out/");
                assert!(
                    !matches!(
                        libfreemkv::parse_url(&url),
                        libfreemkv::StreamUrl::Unknown { .. }
                    ),
                    "{url} is not a scheme libfreemkv recognizes"
                );
            }
        }
    }

    /// The ticked tracks must survive into the mux. `Default::default()` here
    /// is All/All, i.e. every track the user just deselected.
    #[test]
    fn explicit_track_ticks_survive_into_the_mux() {
        let mut r = req();
        r.explicit_streams = true;
        r.audio_pids = vec![4352];
        r.sub_pids = vec![];
        let sel = stream_selection_for(&r, None);
        assert_eq!(sel.audio, libfreemkv::PidFilter::Only(vec![4352]));
        // Ticking nothing under subtitles means keep NONE, not keep all.
        assert_eq!(sel.subtitle, libfreemkv::PidFilter::Only(vec![]));
        assert!(!sel.is_all(), "an explicit selection is never All/All");

        // No explicit choice: keep everything.
        r.explicit_streams = false;
        assert!(stream_selection_for(&r, None).is_all());
    }

    /// `mux_opts` carries the raw passthrough, the read batch and the send
    /// deadline. Defaulted, `--raw` is ignored and the deadline that stops a
    /// wedged sink hanging the rip disappears.
    #[test]
    fn mux_options_carry_raw_the_batch_size_and_the_send_deadline() {
        let mut r = req();
        r.raw = true;
        let o = mux_opts(&r);
        assert!(o.raw, "raw passthrough must reach the mux");
        assert_eq!(o.batch_sectors, 64);
        assert_eq!(o.send_deadline, Some(std::time::Duration::from_secs(60)));
        assert!(!o.skip_errors);
        // Selection is deliberately NOT here — the Url mux arm reads it off
        // InputOptions, and setting it here silently keeps every track.
        assert!(o.selection.is_all());

        r.raw = false;
        assert!(!mux_opts(&r).raw);
    }

    /// The three fields whose absence is invisible: the wrong title, a lost
    /// key, or a discarded track selection, each under the right filename.
    #[test]
    fn per_title_input_options_carry_the_index_the_keys_and_the_selection() {
        let mut disc = super::key_summary_tests::disc(true);
        let keys = vec![(0u32, [7u8; 16]), (1u32, [9u8; 16])];
        disc.aacs = Some(super::key_summary_tests::aacs(keys.clone()));

        let mut r = req();
        r.explicit_streams = true;
        r.audio_pids = vec![4353, 4354];
        // The two titles disagree, which is the whole point: title 3 has 4354
        // unticked while title 0 keeps it. With one filter for the whole rip
        // the union wins and 4354 is written for BOTH — so this fixture is
        // what makes the per-title assertion below able to fail.
        r.title_pids = crate::engine::TitleStreams::PerTitle(vec![
            (0, vec![4353, 4354], vec![]),
            (3, vec![4353], vec![]),
        ]);

        for idx in [0usize, 3] {
            let input = title_input_options(&disc, &r, idx);
            assert_eq!(
                input.title_index,
                Some(idx),
                "a missing title_index muxes title 0 under title {}'s name",
                idx + 1
            );
            assert_eq!(
                input.unit_keys, keys,
                "the resolved AACS keys must be passed"
            );
            // PER TITLE, not the union. Asserting against the union
            // encoded the defect: one filter applied to every title, so a PID
            // unticked under one title was still written for it whenever a
            // sibling title kept it ticked.
            //
            // Written out, not re-derived. `title_input_options` fills this
            // field with `stream_selection_for(req, Some(idx))`, so comparing
            // against a fresh call to the same function moves both sides
            // together under any change to what that function decides — the
            // very shape (union vs union) that let the original defect ship.
            let want: Vec<u16> = if idx == 0 {
                vec![4353, 4354]
            } else {
                vec![4353]
            };
            match &input.selection.audio {
                libfreemkv::PidFilter::Only(got) => assert_eq!(
                    *got,
                    want,
                    "title {} must be muxed with exactly its OWN ticked audio",
                    idx + 1
                ),
                other => panic!("an explicit selection must be a PidFilter::Only, got {other:?}"),
            }
        }

        // An unencrypted disc contributes no keys — and no placeholder either.
        let clear = super::key_summary_tests::disc(false);
        assert!(title_input_options(&clear, &r, 0).unit_keys.is_empty());
    }

    /// One title fans out into the destination directory; two or more each get
    /// their own subdirectory, because a demux sink names files by TRACK. Get
    /// this wrong and every title after the first overwrites the one before.
    #[test]
    fn a_multi_title_demux_gives_each_title_its_own_directory() {
        assert!(!demux_needs_subdirs(1));
        assert!(demux_needs_subdirs(2));
        assert!(demux_needs_subdirs(12));
        // Zero titles never reaches the loop, but must not read as "multi".
        assert!(!demux_needs_subdirs(0));
    }

    /// `--multipass` on a title output must still recover, and a whole-disc ISO
    /// must recover even without it. As `&&` the first silently loses its
    /// recovery passes; the second falls into the per-title loop and panics on
    /// the `unreachable!()` in its destination match.
    #[test]
    fn multipass_and_iso_output_both_route_through_recovery() {
        assert_eq!(
            recovery_plan(OutKind::File("mkv"), true),
            DiscPlan::Recover { deliver_iso: false }
        );
        assert_eq!(
            recovery_plan(OutKind::IsoImage, false),
            DiscPlan::Recover { deliver_iso: true }
        );
        assert_eq!(
            recovery_plan(OutKind::IsoImage, true),
            DiscPlan::Recover { deliver_iso: true }
        );
        assert_eq!(
            recovery_plan(OutKind::File("mkv"), false),
            DiscPlan::PerTitle
        );
        assert_eq!(
            recovery_plan(OutKind::Demux("demux"), false),
            DiscPlan::PerTitle
        );
        // The folder extract is handled before this decision and must not be
        // routed into recovery by it.
        assert_eq!(
            recovery_plan(OutKind::DecryptedFolder, false),
            DiscPlan::PerTitle
        );
    }

    /// A recovery that salvaged even one byte has something to mux; only zero
    /// does not. Inverted, a good recovery deletes its own ISO and reports
    /// "no readable data".
    #[test]
    fn only_a_zero_byte_recovery_has_nothing_to_mux() {
        assert!(recovery_produced_no_data(0));
        assert!(!recovery_produced_no_data(1));
        assert!(!recovery_produced_no_data(50_000_000_000));
    }

    /// The staging ISO is removed unless the user asked to keep it. Inverted,
    /// a multi-hour recovery is deleted against an explicit setting.
    #[test]
    fn the_staging_iso_is_kept_only_when_keep_iso_is_set() {
        assert!(should_delete_staging_iso(false, true, false));
        assert!(!should_delete_staging_iso(true, true, false));
    }

    /// A cancelled mux must not take the recovery down with it. The mux
    /// reports a cancel as `Ok`, so `keep_iso` alone deleted a multi-hour read
    /// the user could then only recover by re-reading the disc.
    #[test]
    fn a_cancelled_mux_keeps_the_staging_iso() {
        assert!(!should_delete_staging_iso(false, true, true));
    }

    /// Same for a mux that failed: the staged image is exactly what lets the
    /// user retry the mux without touching the drive again.
    #[test]
    fn a_failed_mux_keeps_the_staging_iso() {
        assert!(!should_delete_staging_iso(false, false, false));
        assert!(!should_delete_staging_iso(false, false, true));
    }

    /// The key strip names the source that unlocked the disc. It is read from
    /// the trace, not passed in — so `None` here means a disc that WAS unlocked
    /// reports no source, and a constant means every disc names the same one.
    #[test]
    fn the_winning_key_source_is_read_from_the_trace() {
        use libfreemkv::aacs::trace::{KeyOutcome, KeyStep, ResolutionTrace};
        let step = |who: &str, outcome| KeyStep {
            who: who.to_string(),
            path: vec![],
            outcome,
        };

        let won = ResolutionTrace {
            unlock: vec![],
            keys: vec![
                step("online", KeyOutcome::NoKey),
                step("keydb", KeyOutcome::Resolved),
            ],
        };
        assert_eq!(won_from_trace(&won).as_deref(), Some("keydb"));

        let lost = ResolutionTrace {
            unlock: vec![],
            keys: vec![
                step("keydb", KeyOutcome::NoKey),
                step("online", KeyOutcome::MissingVid),
            ],
        };
        assert_eq!(won_from_trace(&lost), None);
        assert_eq!(won_from_trace(&ResolutionTrace::new()), None);
    }

    // ── damage under tolerance is still disclosed ───────────────────────────

    fn clean_result() -> fe::MultipassResult {
        fe::MultipassResult {
            unreadable_bytes: 0,
            pending_bytes: 0,
            good_bytes: 50_000_000_000,
            main_lost_ms: 0.0,
            severity: fe::DamageSeverity::Clean,
            passes: 1,
            aborted_for_loss: false,
            halted: false,
            wedged: false,
            complete: true,
        }
    }

    /// A perfect recovery adds nothing: the plain success message the caller
    /// already builds must not grow a spurious trailing note.
    #[test]
    fn a_clean_recovery_has_no_damage_note() {
        assert_eq!(damage_note(&clean_result()), "");
    }

    /// This is the regression `engine.rs`'s success path shipped: a disc that
    /// recovers with real unreadable/pending bytes UNDER `abort_lost_secs`
    /// (so neither `halted` nor `aborted_for_loss` fires) used to report a
    /// plain "ISO image written to …" — identical to a perfect rip. The CLI's
    /// own `disc_to_iso` prints this exact figure for the same condition
    /// (`rip.mapfile_summary`); the GUI was hiding damage the CLI discloses.
    #[test]
    fn residual_damage_under_tolerance_is_named_in_the_note() {
        let result = fe::MultipassResult {
            unreadable_bytes: 10 * 1_048_576,
            pending_bytes: 2 * 1_048_576,
            good_bytes: 40_000_000_000,
            main_lost_ms: 0.0,
            severity: fe::DamageSeverity::Cosmetic,
            passes: 2,
            ..clean_result()
        };
        let note = damage_note(&result);
        assert!(!note.is_empty(), "damage under tolerance produced no note");
        assert!(note.contains("10.0"), "unreadable MB missing: {note}");
        assert!(note.contains("2.0"), "pending MB missing: {note}");
    }

    /// `main_lost_ms` is the main-title playback time actually lost — the
    /// figure an operator cares about (not just raw byte counts). It must
    /// show up in the note, converted to seconds, whenever it is positive.
    #[test]
    fn lost_playback_time_is_named_when_quantifiable() {
        let result = fe::MultipassResult {
            unreadable_bytes: 1_048_576,
            pending_bytes: 0,
            main_lost_ms: 4_500.0,
            severity: fe::DamageSeverity::Cosmetic,
            ..clean_result()
        };
        let note = damage_note(&result);
        assert!(
            note.contains('s') || note.contains('m'),
            "no lost-playback-time figure in: {note}"
        );
    }

    /// `main_lost_ms` is documented as NaN when the loss cannot be quantified
    /// (no title extents). A NaN must not corrupt the note (e.g. print
    /// "NaNs") — it is simply omitted, leaving the byte-count line intact.
    #[test]
    fn unquantifiable_loss_does_not_corrupt_the_note() {
        let result = fe::MultipassResult {
            unreadable_bytes: 1_048_576,
            pending_bytes: 0,
            main_lost_ms: f64::NAN,
            severity: fe::DamageSeverity::Moderate,
            ..clean_result()
        };
        let note = damage_note(&result);
        assert!(!note.to_lowercase().contains("nan"), "{note}");
    }
}

// ── Every disc-derived string that reaches a GUI row is display-sanitised. ──
//
// Round 1 fixed this per finding — `volume_id`, then the video/audio `label`
// fields — and round 2 found the two it missed (`playlist`, `language`)
// sitting two lines from a call that was already there. This test is driven
// from an ENUMERATION of the untrusted fields instead, so a new one has to be
// added here to pass.
#[cfg(test)]
mod display_sanitisation_tests {
    use super::{Row, scanned_from_disc, stream_rows};
    use crate::strings::is_unsafe_display_char;
    use libfreemkv::disc::{BdRegion, DiscRegion};
    use libfreemkv::{
        AudioChannels, AudioStream, Codec, ColorSpace, ContentFormat, Disc, DiscFormat, DiscTitle,
        FrameRate, HdrFormat, LabelPurpose, LabelQualifier, Resolution, SampleRate, Stream,
        SubtitleStream, VideoStream,
    };

    /// A payload that carries one member of every class `is_unsafe_display_char`
    /// rejects: a C0 control, an ESC-introduced OSC, a newline (log forging), a
    /// bidi override and a zero-width joiner.
    const HOSTILE: &str = "a\u{7}b\u{1b}]0;pwned\u{7}c\nLabel: forged\u{202e}e\u{200b}f";

    /// The control payload for the tooltip-shape comparison: same field, no
    /// character the display rule objects to.
    const BENIGN: &str = "abcdef";

    fn hostile_disc() -> Disc {
        let video = Stream::Video(VideoStream {
            pid: 0x1011,
            codec: Codec::Hevc,
            resolution: Resolution::Unknown,
            frame_rate: FrameRate::Unknown,
            hdr: HdrFormat::Sdr,
            color_space: ColorSpace::Bt709,
            display_aspect: None,
            secondary: true,
            label: HOSTILE.to_string(),
            measured_cicp: None,
        });
        let audio = Stream::Audio(AudioStream {
            pid: 0x1100,
            codec: Codec::TrueHd,
            channels: AudioChannels::Unknown,
            language: HOSTILE.to_string(),
            sample_rate: SampleRate::Unknown,
            secondary: true,
            purpose: LabelPurpose::Commentary,
            label: HOSTILE.to_string(),
        });
        let subtitle = Stream::Subtitle(SubtitleStream {
            pid: 0x1200,
            codec: Codec::Pgs,
            language: HOSTILE.to_string(),
            forced: true,
            qualifier: LabelQualifier::None,
            codec_data: None,
        });

        Disc {
            volume_id: HOSTILE.to_string(),
            meta_title: None,
            format: DiscFormat::Uhd,
            capacity_sectors: 0,
            capacity_bytes: 0,
            layers: 1,
            titles: vec![DiscTitle {
                playlist: HOSTILE.to_string(),
                playlist_id: 800,
                duration_secs: 60.0,
                size_bytes: 1 << 30,
                clips: Vec::new(),
                streams: vec![video, audio, subtitle],
                chapters: Vec::new(),
                extents: Vec::new(),
                content_format: ContentFormat::BdTs,
                codec_privates: Vec::new(),
            }],
            region: DiscRegion::BluRay(vec![BdRegion::A]),
            aacs: None,
            css: None,
            encrypted: true,
            aacs_error: None,
            css_error: None,
            content_format: ContentFormat::BdTs,
        }
    }

    /// Row text carrying a character that must never reach the screen.
    ///
    /// `info` is the multi-line tooltip, so its own template newlines are
    /// legitimate and are not counted here — newline INJECTION into it is
    /// caught by the line-count check in `tooltips_keep_their_own_shape`
    /// instead, which is the assertion that can actually tell the two apart.
    /// The same disc with a harmless payload, as the control for the
    /// line-count comparison above.
    fn benign_disc() -> Disc {
        let mut d = hostile_disc();
        d.volume_id = BENIGN.to_string();
        for t in &mut d.titles {
            t.playlist = BENIGN.to_string();
            for st in &mut t.streams {
                match st {
                    Stream::Video(v) => v.label = BENIGN.to_string(),
                    Stream::Audio(a) => {
                        a.label = BENIGN.to_string();
                        a.language = BENIGN.to_string();
                    }
                    Stream::Subtitle(s) => s.language = BENIGN.to_string(),
                }
            }
        }
        d
    }

    fn offenders(rows: &[Row]) -> Vec<String> {
        let mut bad: Vec<String> = Vec::new();
        for r in rows {
            for s in [&r.desc, &r.type_s] {
                if s.chars().any(is_unsafe_display_char) {
                    bad.push(s.clone());
                }
            }
            if r.info
                .chars()
                .any(|c| c != '\n' && is_unsafe_display_char(c))
            {
                bad.push(r.info.clone());
            }
        }
        bad
    }

    /// The rows the GUI renders for a whole disc — the title line (playlist)
    /// and the disc line (volume id) included.
    #[test]
    fn no_disc_derived_row_text_carries_an_unsafe_display_char() {
        let disc = hostile_disc();
        let scanned = scanned_from_disc(&disc, "none".into(), false);
        let bad = offenders(&scanned.rows);
        assert!(bad.is_empty(), "unsanitised row text: {bad:?}");
    }

    /// The stream rows on their own, so a regression in `stream_rows` cannot
    /// hide behind a passing disc-level assertion.
    #[test]
    fn no_stream_row_text_carries_an_unsafe_display_char() {
        let disc = hostile_disc();
        let bad = offenders(&stream_rows(&disc.titles[0], 0));
        assert!(bad.is_empty(), "unsanitised stream row text: {bad:?}");
    }

    /// A crafted field must not be able to add a LINE to a tooltip — the one
    /// unsafe character `offenders` cannot judge inside `info`. Same disc,
    /// same shape, only the payload differs, so any extra line came from it.
    #[test]
    fn tooltips_keep_their_own_shape() {
        let hostile = scanned_from_disc(&hostile_disc(), "none".into(), false);
        let benign = scanned_from_disc(&benign_disc(), "none".into(), false);
        assert_eq!(hostile.rows.len(), benign.rows.len(), "row count differs");
        for (h, b) in hostile.rows.iter().zip(&benign.rows) {
            assert_eq!(
                h.info.lines().count(),
                b.info.lines().count(),
                "tooltip gained a line from the payload:\n{}",
                h.info
            );
        }
    }

    /// The payload has to be able to fail the assertion — a filter that
    /// silently passed everything would make both tests above vacuous.
    #[test]
    fn the_hostile_payload_is_actually_unsafe() {
        assert!(HOSTILE.chars().any(is_unsafe_display_char));
    }
}
