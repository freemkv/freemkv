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
    let disc = disc;
    let summary = key_summary(&disc, won.as_deref());
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
        });
        rows.extend(stream_rows(t, ti));
    }

    Ok(Scanned {
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
    })
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

/// Build the ordered key-source list from the user's settings, mirroring the
/// CLI's local-first policy: the keydb unless the user asked for online-only,
/// then the online service when a valid URL is configured.
fn key_sources(
    keydb_path: &str,
    keyserver_url: &str,
    keyserver_token: &str,
    online_only: bool,
) -> Vec<Box<dyn freemkv_keysources::KeySource>> {
    let mut v: Vec<Box<dyn freemkv_keysources::KeySource>> = Vec::new();
    if !online_only && !keydb_path.trim().is_empty() {
        v.push(Box::new(freemkv_keysources::KeydbSource::new(
            crate::settings::shellexpand(keydb_path),
        )));
    }
    if !keyserver_url.trim().is_empty()
        && freemkv_keysources::validate_keyserver_url(keyserver_url).is_ok()
    {
        v.push(Box::new(freemkv_keysources::OnlineSource::new(
            keyserver_url.to_string(),
            keyserver_token.to_string(),
        )));
    }
    v
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
    let k = keys.clone();
    let factory: libfreemkv::KeySourceFactory = std::sync::Arc::new(move || {
        key_sources(
            &k.keydb_path,
            &k.keyserver_url,
            &k.keyserver_token,
            k.online_only,
        )
    });
    let resolved = libfreemkv::resolve_keys_for(reader, disc, factory);
    // The trace is the ONLY authoritative record of which source won.
    // `Disc::aacs.key_source` is `ExternalUk` for every caller-supplied key —
    // and is also the scan-time placeholder — so it cannot answer this.
    resolved
        .trace
        .keys
        .iter()
        .find(|step| step.outcome == libfreemkv::aacs::trace::KeyOutcome::Resolved)
        .map(|step| step.who.clone())
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

/// Mirror the CLI's `--force`, which guards exactly one case: writing a
/// decrypted file tree (`dir://`) into a non-empty directory. Ordinary file
/// output into a populated folder is normal and must not be blocked.
pub fn dest_is_writable(req: &RipRequest) -> Result<(), String> {
    if req.force || !req.format.contains("decrypted folder") {
        return Ok(());
    }
    let non_empty = std::fs::read_dir(&req.dest_dir)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if non_empty {
        return Err(format!(
            "{} is not empty — enable “Overwrite existing files” in Settings to unpack into it.",
            req.dest_dir
        ));
    }
    Ok(())
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

fn out_ext(format: &str) -> &'static str {
    if format.contains("MP4") {
        "mp4"
    } else if format.contains("M2TS") {
        "m2ts"
    } else {
        "mkv"
    }
}

fn mux_opts(req: &RipRequest) -> libfreemkv::MuxOptions {
    let selection = if req.explicit_streams {
        libfreemkv::StreamSelection {
            audio: libfreemkv::PidFilter::Only(req.audio_pids.clone()),
            subtitle: libfreemkv::PidFilter::Only(req.sub_pids.clone()),
        }
    } else {
        libfreemkv::StreamSelection::default()
    };
    libfreemkv::MuxOptions {
        skip_errors: false,
        batch_sectors: 64,
        raw: req.raw,
        selection,
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
    dest_is_writable(req)?;
    let name = std::path::Path::new(&req.source)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("output")
        .to_string();
    let ext = out_ext(&req.format);
    let out = format!("{}/{}.{}", req.dest_dir, name, ext);
    let hint = std::fs::metadata(&req.source).map(|m| m.len()).unwrap_or(0);
    let src_url = format!("{}://{}", source_scheme(&req.source), req.source);
    let dest_url = format!("{ext}://{out}");

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
    state.lines.lock().unwrap().push(format!("wrote {out}"));
    Ok(format!("1 file written to {}", req.dest_dir))
}

fn run_blocking(req: &RipRequest, sink: &UiSink, state: &Arc<RunState>) -> Result<String, String> {
    if is_stream_source(&req.source) {
        return run_stream(req, sink, state);
    }
    let (mut disc, mut reader) = libfreemkv::scan_iso(
        std::path::Path::new(&req.source),
        libfreemkv::ScanOptions::default(),
    )
    .map_err(|e| format!("E{} scan failed", e.code()))?;
    // Resolve decryption keys onto the disc BEFORE muxing.
    resolve_disc_keys(&mut disc, reader.as_mut(), &req.keys);
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

    std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;
    dest_is_writable(req)?;
    let src_url = format!("iso://{}", req.source);
    let ext = out_ext(&req.format);
    let scheme = ext;
    let label = if disc.volume_id.is_empty() {
        "disc"
    } else {
        &disc.volume_id
    };

    // The engine owns the per-title loop (skip/abort policy); we only supply
    // "mux one title".
    let written = std::cell::Cell::new(0usize);
    let outcome = fe::run_titles(&indices, !req.titles.is_empty(), sink, |idx| {
        let out = format!("{}/{}_t{}.{}", req.dest_dir, label, idx + 1, ext);
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
            ..Default::default()
        };
        let mux = mux_opts(req);
        let dest_url = format!("{scheme}://{out}");
        match fe::mux_title(&src_url, &dest_url, input, &mux, hint, sink) {
            Ok(o) => {
                if !o.completed {
                    // Cancelled or truncated: the file exists but is partial.
                    // Counting it as written would be a lie.
                    state.lines.lock().unwrap().push(format!(
                        "title {} incomplete — {} is partial",
                        idx + 1,
                        out
                    ));
                    return Ok(());
                }
                state
                    .lines
                    .lock()
                    .unwrap()
                    .push(format!("title {} -> {}", idx + 1, out));
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
                // A failed mux leaves a 0-byte file behind that looks like
                // output. Remove it so the folder never shows a broken result.
                let _ = std::fs::remove_file(&out);
                Err(e)
            }
        }
    });

    Ok(match outcome {
        fe::RipOutcome::Halted => format!(
            "Cancelled — {} of {} title(s) completed",
            written.get(),
            indices.len()
        ),
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
