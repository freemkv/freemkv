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
}

/// What the shell needs after a scan. Pure data — no engine types.
#[derive(Debug, Clone)]
pub struct Scanned {
    pub label: String,
    pub rows: Vec<Row>,
    pub key_summary: String,
    pub title_count: usize,
    /// Video codec name per title, indexed by canonical title index. Lets the
    /// UI say "MP4 cannot hold MPEG-2" BEFORE a rip instead of surfacing a
    /// bare E9048 after one.
    pub video_codecs: Vec<String>,
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
                    let label = if v.label.is_empty() {
                        String::new()
                    } else {
                        format!("  —  {}", v.label)
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
                                format!("\nLabel: {}", v.label)
                            }
                        ),
                    )
                }
                libfreemkv::Stream::Audio(a) => {
                    let mut tags: Vec<String> = Vec::new();
                    if let Some(p) = purpose_label(a.purpose) {
                        tags.push(p.to_string());
                    }
                    if a.secondary {
                        tags.push("Secondary".into());
                    }
                    if !a.label.is_empty() {
                        tags.push(a.label.clone());
                    }
                    let suffix = if tags.is_empty() {
                        String::new()
                    } else {
                        format!("  —  {}", tags.join(", "))
                    };
                    (
                        "Audio",
                        Some(a.pid),
                        format!("{}  {}  {}{}", a.codec, a.channels, a.language, suffix),
                        format!(
                            "Audio track\n\nCodec: {}\nChannels: {}\nLanguage: {}\nSample rate: {}{}{}",
                            a.codec,
                            a.channels,
                            a.language,
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
                        format!("{}  {}{}", s.codec, s.language, suffix),
                        format!(
                            "Subtitle track\n\nCodec: {}\nLanguage: {}\nForced: {}",
                            s.codec, s.language, s.forced
                        ),
                    )
                }
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
    });
    rows.extend(stream_rows(t, 0));

    Ok(Scanned {
        label: name,
        title_count: 1,
        key_summary: "unencrypted".into(),
        video_codecs: vec![
            t.video_streams()
                .next()
                .map(|v| v.codec.to_string())
                .unwrap_or_default(),
        ],
        rows,
    })
}

/// Scan a source (ISO path today) and flatten it into display rows.
pub fn scan(path: &str) -> Result<Scanned, String> {
    scan_with_keys(path, &KeyConfig::default())
}

/// Scan, then consult the key sources so the key strip reflects a real
/// resolution rather than the scan-time placeholder.
pub fn scan_with_keys(path: &str, keys: &KeyConfig) -> Result<Scanned, String> {
    let (mut disc, mut reader) = libfreemkv::scan_iso(
        std::path::Path::new(path),
        libfreemkv::ScanOptions::default(),
    )
    .map_err(|e| format!("E{} scan failed", e.code()))?;

    let won = resolve_disc_keys(&mut disc, reader.as_mut(), keys);
    let summary = key_summary(&disc, won.as_deref());
    Ok(scanned_from_disc(&disc, summary))
}

/// Build the title tree + info rows from a scanned `Disc`. Shared by the ISO
/// (`scan_with_keys`) and live-drive (`scan_disc_with_keys`) paths so a disc
/// looks identical whether it came from a file or a physical drive.
fn scanned_from_disc(disc: &libfreemkv::Disc, summary: String) -> Scanned {
    let mut rows = Vec::new();
    let label = if disc.volume_id.is_empty() {
        "(no label)".to_string()
    } else {
        disc.volume_id.clone()
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
                    format!("{}   ", t.playlist)
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
        });
        rows.extend(stream_rows(t, ti));
    }

    Scanned {
        label,
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
        rows,
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
    let key_url = (!keys.keyserver_url.trim().is_empty()).then(|| keys.keyserver_url.clone());
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
pub fn scan_disc_with_keys(source: &str, keys: &KeyConfig) -> Result<Scanned, String> {
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
    Ok(scanned_from_disc(disc, summary))
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
    let (mut disc, mut reader) = libfreemkv::scan_iso(
        std::path::Path::new(path),
        libfreemkv::ScanOptions::default(),
    )
    .map_err(|e| format!("E{}", e.code()))?;
    resolve_disc_keys(&mut disc, reader.as_mut(), keys);
    let disc = disc;
    let sel = if titles.is_empty() {
        fe::Selection::MainMovie
    } else {
        fe::Selection::Titles(titles.to_vec())
    };
    let job = fe::Job::new(format!("iso://{path}"), dest).with_selection(sel);
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
}

struct UiSink(Arc<RunState>);

impl fe::Sink for UiSink {
    fn log(&self, _level: fe::Level, msg: &str) {
        if let Ok(mut v) = self.0.lines.lock() {
            v.push(msg.to_string());
        }
    }
    fn progress(&self, p: &fe::Progress) {
        if let Ok(mut g) = self.0.prog.lock() {
            *g = Prog {
                bytes_done: p.bytes_done,
                bytes_total: p.bytes_total,
                speed_bps: p.speed_bps,
                eta_secs: p.eta_secs,
                sectors_bad: p.sectors_bad,
            };
        }
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
/// The library's `Display` is exactly `E<code>` (it carries zero English by
/// design), so this parse is against a stable contract rather than prose.
/// libfreemkv has `io_error_code` internally but does not export it — when it
/// does, delete this and call it.
pub fn error_code(e: &std::io::Error) -> u16 {
    e.to_string().trim_start_matches('E').parse().unwrap_or(0)
}

/// Turn a library error code into something a person can act on.
///
/// The library carries no English by design, so a front-end that just prints
/// `E9048` has told the user nothing. Only codes a user can actually do
/// something about are spelled out; the rest keep the code for a bug report.
pub fn explain(code: u16) -> String {
    match code {
        9048 => "MP4 cannot store this title's video (MP4 holds H.264, HEVC \
                 and AV1 only). Choose MKV, which keeps everything."
            .to_string(),
        7022 | 8005 => "No decryption key for this disc. Check the keydb or \
                        online key service in Settings."
            .to_string(),
        6013 => "This file is not a disc image.".to_string(),
        c => format!("Mux failed (E{c})."),
    }
}

/// Key configuration taken from the user's settings.
#[derive(Clone, Default)]
pub struct KeyConfig {
    pub keydb_path: String,
    pub keyserver_url: String,
    pub keyserver_token: String,
    pub online_only: bool,
}

impl KeyConfig {
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        KeyConfig {
            keydb_path: s.keydb_path.clone(),
            keyserver_url: s.keyserver_url.clone(),
            keyserver_token: s.keyserver_token.clone(),
            online_only: s.key_source.starts_with("Online"),
        }
    }
}

/// Which titles the user ticked, as canonical indices.
pub struct RipRequest {
    pub source: String,
    pub dest_dir: String,
    pub titles: Vec<usize>,
    pub format: String,
    /// PIDs of the ticked audio tracks. Empty = keep every audio track.
    pub audio_pids: Vec<u16>,
    /// PIDs of the ticked subtitle tracks. Empty = keep every subtitle track.
    pub sub_pids: Vec<u16>,
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
    pub keys: KeyConfig,
}

/// Run the real rip on a worker thread: engine title loop + per-title mux.
/// Returns immediately.
pub fn start_rip(req: RipRequest, state: Arc<RunState>) {
    std::thread::spawn(move || {
        let sink = UiSink(state.clone());
        let res = run_blocking(&req, &sink, &state);
        match res {
            Ok(s) => *state.summary.lock().unwrap() = s,
            Err(e) => {
                state.lines.lock().unwrap().push(e.clone());
                *state.summary.lock().unwrap() = e;
            }
        }
        state.finished.store(true, Ordering::Relaxed);
    });
}

/// The decrypted-folder target for a rip: a per-disc subdirectory of the
/// destination, named by the disc's volume label. Exposed so the writability
/// gate is testable without a real disc.
pub fn extract_target(dest_dir: &str, label: &str) -> std::path::PathBuf {
    std::path::Path::new(dest_dir).join(label)
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
    state.lines.lock().unwrap().push(format!(
        "extracting decrypted file tree → {}",
        dest.display()
    ));
    match fe::extract_tree(disc, reader, &dest, req.force, sink) {
        Ok(res) => {
            for f in &res.files {
                if f.bytes_unreadable > 0 {
                    state.lines.lock().unwrap().push(format!(
                        "  {} — {:.1} MB unreadable",
                        f.path.display(),
                        f.bytes_unreadable as f64 / 1_048_576.0
                    ));
                }
            }
            let n = res.files.len();
            if res.bytes_unreadable > 0 {
                Ok(format!(
                    "Decrypted file tree written to {} — {} file(s), {:.1} MB unreadable",
                    dest.display(),
                    n,
                    res.bytes_unreadable as f64 / 1_048_576.0
                ))
            } else {
                Ok(format!(
                    "Decrypted file tree written to {} — {} file(s)",
                    dest.display(),
                    n
                ))
            }
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

/// What real operation an output format maps to. The picker offers nine format
/// strings; six of them used to fall through to a per-title MKV mux. Each now
/// resolves to its true sink so the file the user gets matches what they chose.
#[derive(Clone, Copy)]
enum OutKind {
    /// A per-title file produced through the mux pipeline. The `&str` is BOTH
    /// the dest-URL scheme AND the file extension — `mkv`/`mp4`/`m2ts` for
    /// containers, or `chapters`/`json`/`fvi` for the metadata / index sinks the
    /// resolve layer dispatches on (same as the CLI's `dir_jobs`).
    File(&'static str),
    /// Each title's tracks fanned out to elementary-stream files in a directory
    /// (the CLI's `demux://` sink, which does its own per-track naming).
    Demux,
    /// The whole disc's decrypted UDF file tree, extracted to a per-disc
    /// subdirectory (the CLI's `dir://` → `Disc::extract_tree`).
    DecryptedFolder,
    /// A whole-disc sector image. Needs a physical disc (`disc://`); there is no
    /// iso-file → iso-file decrypt copy, so this is not offered for an ISO source
    /// yet — see the disc:// live-drive work.
    IsoImage,
}

/// Map a picker format string to its real output kind. Order matters only in
/// that each branch's marker is unique across the nine format strings.
fn out_kind(format: &str) -> OutKind {
    if format.contains("decrypted folder") {
        OutKind::DecryptedFolder
    } else if format.contains("ISO image") {
        OutKind::IsoImage
    } else if format.contains("separate track") {
        OutKind::Demux
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

/// Build a per-title output basename from the filename template. `{title}` →
/// the disc/volume label (or container name), `{n}` → the 1-based title number.
/// An empty template falls back to the historical `<label>_t<n>`; a template
/// with no `{n}` gets `_t<n>` appended so multi-title output can never collide.
/// Path separators a user might type are neutralized to keep output in-folder.
fn title_basename(template: &str, label: &str, n: usize) -> String {
    let t = template.trim();
    if t.is_empty() {
        return format!("{label}_t{n}");
    }
    let mut name = t.replace("{title}", label);
    if name.contains("{n}") {
        name = name.replace("{n}", &n.to_string());
    } else {
        name = format!("{name}_t{n}");
    }
    name.replace('/', "_")
}

/// The audio/subtitle selection for this request. It goes on `InputOptions`,
/// NOT `MuxOptions`: our mux uses a URL source (`iso://…`), and `mux_stream`'s
/// Url arm prunes via `InputOptions.selection` — `MuxOptions.selection` is only
/// consulted on the File/Session (live-drive) arms. Putting it on MuxOptions
/// silently kept every track. Empty PID lists = keep none of that class.
fn stream_selection(req: &RipRequest) -> libfreemkv::StreamSelection {
    if req.explicit_streams {
        libfreemkv::StreamSelection {
            audio: libfreemkv::PidFilter::Only(req.audio_pids.clone()),
            subtitle: libfreemkv::PidFilter::Only(req.sub_pids.clone()),
        }
    } else {
        libfreemkv::StreamSelection::default()
    }
}

fn mux_opts(req: &RipRequest) -> libfreemkv::MuxOptions {
    libfreemkv::MuxOptions {
        skip_errors: false,
        batch_sectors: 64,
        raw: req.raw,
        // Selection lives on InputOptions for the Url mux path — see stream_selection.
        selection: libfreemkv::StreamSelection::default(),
        send_deadline: Some(std::time::Duration::from_secs(60)),
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
        OutKind::Demux => {
            let dir = format!("{}/", req.dest_dir);
            (format!("demux://{dir}"), format!("{dir} (per-track files)"))
        }
        OutKind::DecryptedFolder | OutKind::IsoImage => {
            return Err("That output is for a disc source — open an ISO or disc to use it.".into());
        }
    };

    if req.explicit_streams {
        state.lines.lock().unwrap().push(
            "Note: track selection is not applied to container sources yet — every track is kept."
                .to_string(),
        );
    }
    fe::mux_title(
        &src_url,
        &dest_url,
        libfreemkv::InputOptions::default(),
        &mux_opts(req),
        hint,
        sink,
    )
    .map_err(|e| format!("convert failed: {e}"))?;
    state.lines.lock().unwrap().push(format!("wrote {target}"));
    Ok(format!("Written to {}", req.dest_dir))
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
    let multi = indices.len() > 1;

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
            OutKind::Demux => {
                let dir = if multi {
                    format!("{}/t{:02}/", req.dest_dir, idx + 1)
                } else {
                    format!("{}/", req.dest_dir)
                };
                (format!("demux://{dir}"), dir)
            }
            // Whole-disc kinds handled by their own callers.
            OutKind::DecryptedFolder | OutKind::IsoImage => unreachable!(),
        };
        let hint = disc.titles.get(idx).map(|t| t.size_bytes).unwrap_or(0);
        // AACS unit keys must reach the mux or every encrypted title fails
        // with E7022 — the scan resolving them is not enough on its own.
        // Mirrors the engine's own wiring in run.rs.
        let input = libfreemkv::InputOptions {
            title_index: Some(idx),
            unit_keys: disc
                .aacs
                .as_ref()
                .map(|a| a.unit_keys.clone())
                .unwrap_or_default(),
            // The ticked audio/subtitle tracks — applied by the Url mux path.
            selection: stream_selection(req),
            ..Default::default()
        };
        let mux = mux_opts(req);
        match fe::mux_title(source_url, &dest_url, input, &mux, hint, sink) {
            Ok(o) => {
                if !o.completed {
                    // Cancelled or truncated: a partial file is on disk. Keep it —
                    // a partial mp4/mkv is usually watchable up to the cut — but
                    // don't count it as a full write, and SAY it's partial. Never
                    // "nothing written" when a file is sitting in the folder.
                    partial.set(partial.get() + 1);
                    state.lines.lock().unwrap().push(format!(
                        "title {} cancelled — partial output kept: {}",
                        idx + 1,
                        target
                    ));
                    return Ok(());
                }
                state
                    .lines
                    .lock()
                    .unwrap()
                    .push(format!("title {} -> {}", idx + 1, target));
                written.set(written.get() + 1);
                state
                    .titles_done
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                state.lines.lock().unwrap().push(format!(
                    "Title {}: {}",
                    idx + 1,
                    explain(error_code(&e))
                ));
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

    // A "partial N kept" note whenever a cancel left one or more partial files,
    // so the result never reads "Nothing was written" while a file is on disk.
    let partial_note = |lead: &str| {
        if partial.get() > 0 {
            format!(
                "{lead} — {} partial file(s) kept in {}",
                partial.get(),
                req.dest_dir
            )
        } else {
            lead.to_string()
        }
    };
    Ok(match outcome {
        fe::RipOutcome::Halted => partial_note(&format!(
            "Cancelled — {} of {} title(s) completed",
            written.get(),
            indices.len()
        )),
        _ if written.get() == 0 && partial.get() > 0 => partial_note("Cancelled"),
        _ if written.get() == 0 => "Nothing was written".to_string(),
        _ => format!("{} title(s) written to {}", written.get(), req.dest_dir),
    })
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
    let (mut disc, mut reader) = libfreemkv::scan_iso(
        std::path::Path::new(&req.source),
        libfreemkv::ScanOptions::default(),
    )
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
            return Err("ISO-image output needs a physical disc (disc://). This source is already an ISO — choose “decrypted folder” to unpack its files.".into());
        }
        _ => {}
    }
    let disc = disc;

    let sel = if req.titles.is_empty() {
        fe::Selection::MainMovie
    } else {
        fe::Selection::Titles(req.titles.clone())
    };
    let indices = fe::resolve_selection(&disc, &sel);
    state
        .lines
        .lock()
        .unwrap()
        .push(format!("selection resolved to titles {indices:?}"));
    if indices.is_empty() {
        return Err("Nothing selected to rip.".into());
    }
    let src_url = format!("iso://{}", req.source);
    mux_selected_titles(&disc, &src_url, req, &indices, sink, state)
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

    // ISO image needs the mapfile-driven disc→ISO copy (the CLI's disc_to_iso /
    // Disc::copy). Not ported to the GUI engine yet — say so plainly rather than
    // write the wrong thing. Every other sink works off the session below.
    if matches!(kind, OutKind::IsoImage) {
        return Err("Whole-disc ISO image from a live drive isn't wired into the GUI yet — it needs the recovery/mapfile copy path (coming with drive validation). Use a title format or decrypted folder for now.".into());
    }

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
    let disc = session.disc().ok_or("scan produced no disc")?;
    let label = if disc.volume_id.is_empty() {
        "disc".to_string()
    } else {
        disc.volume_id.clone()
    };

    // Decrypted folder: extract the UDF tree off the staged drive into a
    // per-disc subdir (same helper the ISO path uses).
    if matches!(kind, OutKind::DecryptedFolder) {
        session.stage_drive_as_reader();
        let mut reader = session
            .take_reader()
            .ok_or("could not stage the drive for extraction")?;
        let disc = session.disc().ok_or("scan produced no disc")?;
        return run_extract_folder(req, disc, reader.as_mut(), &label, sink, state);
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
    let multi = indices.len() > 1;
    let selection = stream_selection(req);
    // Byte-size hints per title, banked before the scan session is dropped
    // (releasing the drive) — each title's mux reopens its own session,
    // mirroring the CLI.
    let hints: Vec<u64> = disc.titles.iter().map(|t| t.size_bytes).collect();
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
            OutKind::Demux => {
                let dir = if multi {
                    format!("{}/t{:02}/", req.dest_dir, idx + 1)
                } else {
                    format!("{}/", req.dest_dir)
                };
                (format!("demux://{dir}"), dir)
            }
            OutKind::DecryptedFolder | OutKind::IsoImage => unreachable!(),
        };
        let hint = hints.get(idx).copied().unwrap_or(0);

        // Shared drive bring-up (open + lock + scan + resolve) — same core as
        // the CLI's pipe_disc. A fresh session per title matches it (the
        // staged reader is consumed by one mux).
        let (mut session, _trace) = fe::open_scan_resolve(
            disc_target(&req.source),
            session_credentials(&req.keys),
            key_factory(&req.keys),
        )
        .map_err(|e| std::io::Error::other(format!("{e}")))?;
        session.stage_drive_as_reader();

        let opts = libfreemkv::MuxOptions {
            skip_errors: false,
            batch_sectors: 64,
            raw: req.raw,
            // Session arm reads selection from MuxOptions (unlike the Url arm).
            selection: selection.clone(),
            send_deadline: Some(std::time::Duration::from_secs(60)),
        };

        match fe::mux_title_session(&mut session, idx, &dest_url, &opts, hint, sink) {
            Ok(o) => {
                if !o.completed {
                    // Cancelled or truncated: a partial file is on disk — keep
                    // it, don't count it as a full write, and say it's partial.
                    partial.set(partial.get() + 1);
                    state.lines.lock().unwrap().push(format!(
                        "title {} cancelled — partial output kept: {}",
                        idx + 1,
                        target
                    ));
                    return Ok(());
                }
                state
                    .lines
                    .lock()
                    .unwrap()
                    .push(format!("title {} -> {}", idx + 1, target));
                written.set(written.get() + 1);
                state.titles_done.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                state.lines.lock().unwrap().push(format!(
                    "Title {}: {}",
                    idx + 1,
                    explain(error_code(&e))
                ));
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

    // A "partial N kept" note whenever a cancel left one or more partial
    // files, so the result never reads "Nothing was written" while a file is
    // on disk.
    let partial_note = |lead: &str| {
        if partial.get() > 0 {
            format!(
                "{lead} — {} partial file(s) kept in {}",
                partial.get(),
                req.dest_dir
            )
        } else {
            lead.to_string()
        }
    };
    Ok(match outcome {
        fe::RipOutcome::Halted => partial_note(&format!(
            "Cancelled — {} of {} title(s) completed",
            written.get(),
            indices.len()
        )),
        _ if written.get() == 0 && partial.get() > 0 => partial_note("Cancelled"),
        _ if written.get() == 0 => "Nothing was written".to_string(),
        _ => format!("{} title(s) written to {}", written.get(), req.dest_dir),
    })
}

#[cfg(test)]
mod key_summary_tests {
    use super::key_summary;

    fn disc(encrypted: bool) -> libfreemkv::Disc {
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

    fn aacs(unit_keys: Vec<(u32, [u8; 16])>) -> libfreemkv::AacsState {
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
}
