//! Bridge to `freemkv-engine`. Everything the UI knows about discs, keys and
//! rips comes through here — no engine types leak into the AppKit shell.

use freemkv_engine as fe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One row of the title tree, already formatted for display.
#[derive(Debug, Clone)]
pub struct Row {
    /// The row's kind ("Title", "Video", "Audio", "Subtitles", …): matched on, never shown.
    pub type_s: String,
    /// The Item cell in the active locale ("Title 3", "Audio").
    pub item: String,
    /// The Format cell: what the track is ("HEVC 2160p 23.976fps HDR10", "Dolby Digital 5.1").
    pub format: String,
    /// The Notes cell: what sets the row apart (chapters, purpose, forced, playlist).
    pub notes: String,
    /// Format and notes as one line, for a shell without the separate columns.
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
    /// The PID of the row whose tick this row shows, for a row that is no choice
    /// of its own: the DVD MPEG-2 multichannel extension (PID 0xD0|n) follows its
    /// base 0xC0|n, "disabled and mirrors the base" (mpg-output-design v5 §3).
    /// `None` for every other row.
    pub mirrors: Option<u16>,
    /// The title's size in bytes, for the tree's Size column. `Some` only on a
    /// disc's Title rows; a stream source reports no size, and no other row has one.
    pub size_bytes: Option<u64>,
}

/// What the shell needs after a scan. Owned data, with no live disc handles.
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
    /// Engine selection evidence in canonical scan order, independent of display rows.
    pub selection_model: fe::SelectionModel,
    pub key_summary: String,
    pub title_count: usize,
    /// Video codec name per title, indexed by canonical title index. Lets the
    /// UI say "MP4 cannot hold MPEG-2" BEFORE a rip instead of surfacing a
    /// bare E9048 after one.
    pub video_codecs: Vec<String>,
    /// Each title's size in bytes as the disc's own tables give it, indexed by
    /// canonical title index (the shape `video_codecs` uses); `0` where the
    /// scan does not know it. Empty for a container source, whose size is its
    /// file's.
    pub title_sizes: Vec<u64>,
    /// The disc's capacity in bytes; `0` for a container source or when the
    /// scan does not know it.
    pub capacity_bytes: u64,
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
    /// The key set Open resolved for the main title (KU §2.5 "GUI open | `Titles([main])`
    /// for status. The result seeds the rip's `resolve`"): memory only, never written.
    pub keys: Option<libfreemkv::keys::KeyRing>,
    /// Open refused E7034: only the disc's Volume ID can finish the key (KU §4.2).
    pub needs_disc: bool,
    /// Open's answered refusal (not E7034): Start shows it again rather than asking again.
    pub refusal: Option<KeyRefusal>,
}

/// A key refusal Open got from the sources, kept so Start does not ask them twice (KU §2.1
/// invariant 4): re-asked only after a key-settings change or keydb update, for titles
/// Open did not resolve, or when Open's failure was transport-class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRefusal {
    pub code: u16,
    /// What the log said at Open, said again at Start.
    pub text: String,
    /// The titles Open resolved (its scope).
    pub titles: Vec<usize>,
    /// E7028, the key service unreachable (J13 transport class): asking again may succeed.
    pub transport: bool,
}

/// The key settings and the keydb file's state at one moment: a refusal given under one
/// snapshot is not re-asked under the same one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySnapshot(KeyConfig, Option<(std::time::SystemTime, u64)>);

impl KeySnapshot {
    pub fn of(keys: &KeyConfig) -> Self {
        let keydb = key_params(keys)
            .keydb_path
            .and_then(|p| std::fs::metadata(p).ok());
        let stamp = keydb.and_then(|m| Some((m.modified().ok()?, m.len())));
        KeySnapshot(keys.clone(), stamp)
    }
}

fn fmt_dur(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// The disc's format as people name it ("DVD", "Blu-ray", …), shared by the CLI and every GUI.
pub fn format_name(f: &libfreemkv::DiscFormat) -> String {
    use libfreemkv::DiscFormat as F;
    match f {
        F::Uhd => "4K UHD".into(),
        F::Fmts => "4K UHD (AACS 2.1 FMTS)".into(),
        F::BluRay => "Blu-ray".into(),
        F::HdDvd => "HD-DVD".into(),
        F::Dvd => "DVD".into(),
        F::Unknown => crate::strings::get("disc.format_unknown"),
    }
}

fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
}

/// The Item cell of a title row: "Title 3", numbered as `-t` numbers it.
pub fn title_item(index: usize) -> String {
    crate::strings::get_or("gui.item.title", "Title {num}")
        .replace("{num}", &(index + 1).to_string())
}

fn chapters_note(n: usize) -> String {
    match n {
        1 => crate::strings::get_or("gui.item.chapter", "1 chapter"),
        n => crate::strings::get_or("gui.item.chapters", "{n} chapters")
            .replace("{n}", &n.to_string()),
    }
}

fn clips_note(n: usize) -> String {
    let word = if n == 1 { "disc.clip" } else { "disc.clips" };
    format!("{n} {}", crate::strings::get(word))
}

/// The Notes cell of a title row: its proven role, real playlist name, clip count and chapters.
fn title_notes(
    t: &libfreemkv::DiscTitle,
    show_playlist: bool,
    role: Option<freemkv_engine::TitleRole>,
) -> String {
    let play_all = crate::strings::get_or("gui.item.play_all", "Play all");
    let episode = crate::strings::get_or("gui.item.episode", "Episode");
    let mut parts = Vec::new();
    match role {
        Some(freemkv_engine::TitleRole::PlayAll) => parts.push(play_all),
        Some(freemkv_engine::TitleRole::Episode) => parts.push(episode),
        None => {}
    }
    if show_playlist && !t.playlist.is_empty() {
        parts.push(sanitize_display(&t.playlist));
    }
    if !t.clips.is_empty() {
        parts.push(clips_note(t.clips.len()));
    }
    parts.push(chapters_note(t.chapters.len()));
    parts.join(" · ")
}

/// An audio track's purpose in the active locale.
fn purpose_label(p: libfreemkv::LabelPurpose) -> Option<String> {
    use libfreemkv::LabelPurpose as P;
    let key = match p {
        P::Commentary => "stream.purpose.commentary",
        P::Descriptive => "stream.purpose.descriptive",
        P::Score => "stream.purpose.score",
        P::Ime => "stream.purpose.ime",
        P::Normal => return None,
    };
    Some(crate::strings::get(key))
}

// Join the parts that say something; a cell with nothing to say stays empty.
fn cell(parts: Vec<String>, sep: &str) -> String {
    parts
        .into_iter()
        .filter(|p| !p.trim().is_empty() && !p.eq_ignore_ascii_case("unknown"))
        .collect::<Vec<_>>()
        .join(sep)
}

/// The Format and Notes cells of a stream row: what the track is, said once, and what sets
/// it apart from its neighbours. The language is the row's own column.
fn stream_cells(st: &libfreemkv::Stream) -> (String, String) {
    use libfreemkv::Stream;
    let secondary_word = crate::strings::get("stream.secondary");
    let secondary = || secondary_word.clone();
    match st {
        Stream::Video(v) => {
            let aspect = v.display_aspect.map(|(w, h)| format!("{w}:{h}"));
            let hdr = (v.hdr != libfreemkv::HdrFormat::Sdr).then(|| v.hdr.to_string());
            let format = cell(
                [
                    Some(v.codec.to_string()),
                    Some(v.resolution.to_string()),
                    // The library names a rate by its number ("25", "23.976"); the unit is ours.
                    Some(v.frame_rate.to_string())
                        .filter(|r| r.parse::<f64>().is_ok())
                        .map(|r| format!("{r}fps")),
                    hdr,
                    aspect,
                ]
                .into_iter()
                .flatten()
                .collect(),
                " ",
            );
            // A video label is the library's own restatement of this format: never shown.
            let notes = cell(v.secondary.then(secondary).into_iter().collect(), " · ");
            (format, notes)
        }
        Stream::Audio(a) => {
            // The label is "(variant) friendly codec name", either part optional: the name is the
            // Format, the variant (a disc's own tag such as "csp") sets the track apart.
            let label = sanitize_display(a.label.trim());
            let (variant, name) = match label.strip_prefix('(').and_then(|r| r.split_once(')')) {
                Some((v, rest)) => (Some(v.trim().to_string()), rest.trim().to_string()),
                None => (None, label),
            };
            let format = if name.is_empty() {
                libfreemkv::labels::audio_codec_label(&a.codec, &a.channels)
            } else {
                name
            };
            let notes = cell(
                [
                    purpose_label(a.purpose),
                    a.secondary.then(secondary),
                    variant,
                ]
                .into_iter()
                .flatten()
                .collect(),
                " · ",
            );
            (format, notes)
        }
        Stream::Subtitle(s) => {
            use libfreemkv::LabelQualifier as Q;
            let qualifier = match s.qualifier {
                Q::Sdh => Some(crate::strings::get("stream.qualifier.sdh")),
                Q::DescriptiveService => {
                    Some(crate::strings::get("stream.qualifier.descriptive_service"))
                }
                Q::Forced | Q::None => None,
            };
            let forced_word = crate::strings::get_or("gui.item.forced", "Forced");
            let forced = s.forced.then_some(forced_word);
            let notes = cell([forced, qualifier].into_iter().flatten().collect(), " · ");
            (s.codec.to_string(), notes)
        }
    }
}

// Rows for one title's streams, shared by the disc and stream-source paths
// so an MKV shows the same track detail a disc title does.
fn stream_rows(t: &libfreemkv::DiscTitle, ti: usize) -> Vec<Row> {
    t.streams
        .iter()
        .map(|st| {
            let (ty, item_key, pid) = match st {
                libfreemkv::Stream::Video(_) => ("Video", "disc.video", None),
                libfreemkv::Stream::Audio(a) => ("Audio", "disc.audio", Some(a.pid)),
                libfreemkv::Stream::Subtitle(s) => ("Subtitles", "disc.subtitle", Some(s.pid)),
            };
            let (format, notes) = stream_cells(st);
            let info = stream_info(st);
            // Language / forcedness as DATA: the matcher needs the disc's own tags.
            let (lang, forced) = match st {
                libfreemkv::Stream::Video(_) => (String::new(), false),
                libfreemkv::Stream::Audio(a) => (a.language.clone(), false),
                libfreemkv::Stream::Subtitle(s) => (s.language.clone(), s.forced),
            };
            // libfreemkv's `StreamSelection::apply` keeps the extension iff its base is kept,
            // so its row is never a choice: it mirrors base PID 0xC0|n.
            let mirrors = match st {
                libfreemkv::Stream::Audio(a) if a.is_mp2_extension() => {
                    Some(0x00C0 | (a.pid & 0x07))
                }
                _ => None,
            };
            Row {
                type_s: ty.into(),
                item: crate::strings::get(item_key),
                desc: cell(vec![format.clone(), notes.clone()], "  —  "),
                format,
                notes,
                depth: 2,
                checkable: ty != "Video" && mirrors.is_none(),
                title: ti,
                info,
                pid,
                duration_secs: 0.0,
                lang,
                forced,
                mirrors,
                size_bytes: None,
            }
        })
        .collect()
}

// A title's chapters under one collapsed "Chapters" row, view only: each one's length, and its
// name only where the disc carries one. A bare ordinal ("1", "2", what a Blu-ray gives) is no name.
fn chapter_rows(t: &libfreemkv::DiscTitle, ti: usize) -> Vec<Row> {
    if t.chapters.len() < 2 {
        return Vec::new();
    }
    let row = |type_s: &str, item: String, notes: String, depth, duration_secs| Row {
        type_s: type_s.into(),
        item,
        desc: notes.clone(),
        format: String::new(),
        notes,
        depth,
        checkable: false,
        title: ti,
        info: String::new(),
        pid: None,
        duration_secs,
        lang: String::new(),
        forced: false,
        mirrors: None,
        size_bytes: None,
    };
    let group = crate::strings::get_or("gui.item.chapter_list", "Chapters");
    let mut rows = vec![row("Chapters", group, String::new(), 2, 0.0)];
    for (i, c) in t.chapters.iter().enumerate() {
        let end = t
            .chapters
            .get(i + 1)
            .map_or(t.duration_secs, |n| n.time_secs);
        let name = sanitize_display(c.name.trim());
        let notes = if name != (i + 1).to_string() {
            name
        } else {
            String::new()
        };
        let item = crate::strings::get_or("gui.item.chapter_n", "Chapter {n}")
            .replace("{n}", &(i + 1).to_string());
        let length = end.round() - c.time_secs.round();
        rows.push(row("Chapter", item, notes, 3, length.max(0.0)));
    }
    rows
}

// The detail text of a stream row.
fn stream_info(st: &libfreemkv::Stream) -> String {
    match st {
        libfreemkv::Stream::Video(v) => format!(
            "Video track\n\nCodec: {}\nResolution: {}\nFrame rate: {}\nHDR: {}\nColour: {}",
            v.codec, v.resolution, v.frame_rate, v.hdr, v.color_space
        ),
        libfreemkv::Stream::Audio(a) => format!(
            "Audio track\n\nCodec: {}\nChannels: {}\nLanguage: {}\nSample rate: {}",
            a.codec,
            a.channels,
            sanitize_display(&a.language),
            a.sample_rate
        ),
        libfreemkv::Stream::Subtitle(s) => format!(
            "Subtitle track\n\nCodec: {}\nLanguage: {}\nForced: {}",
            s.codec,
            sanitize_display(&s.language),
            s.forced
        ),
    }
}

/// Scan a stream source (`.mkv`, `.mp4`, `.m2ts`) — a single title, but its
/// tracks are real and worth showing. `Stream::info()` carries the parsed
/// `DiscTitle`.
// The library's integration tests and the dev harness call this; the binary's GUI does not.
#[allow(dead_code)]
pub fn scan_stream(path: &str) -> Result<Scanned, String> {
    scan_stream_under(path, &KeyConfig::default(), &OpenToken::default())
}

/// [`scan_stream`] with the open's key settings and token: a loose `.m2ts` clip's keys are
/// looked up from its disc folder.
pub fn scan_stream_under(path: &str, keys: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    // Another source replaces the disc Open held for Start.
    HOLD.release();
    let scheme = crate::ui::container_scheme(path)
        .ok_or_else(|| format!("not a container source: {path}"))?;
    let url = format!("{scheme}://{path}");
    let (found, trace) = loose_clip_keys(path, false, keys, &tok.halt);
    let found =
        stopped_open(found, tok)?.map_err(|e| format!("E{} {}", e.code(), explain(e.code())))?;
    let info = fe::stream_info(&url, found.clone(), &tok.halt).map_err(|e| format!("{e}"))?;
    let t = &info;

    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("stream")
        .to_string();

    let mut rows = vec![Row {
        type_s: "File".into(),
        item: name.clone(),
        format: scheme.to_uppercase(),
        notes: crate::strings::get_or("gui.item.tracks", "{n} tracks")
            .replace("{n}", &t.streams.len().to_string()),
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
        mirrors: None,
        size_bytes: None,
    }];
    rows.push(Row {
        type_s: "Title".into(),
        item: title_item(0),
        format: String::new(),
        notes: chapters_note(t.chapters.len()),
        // The running time is the row's Length cell, not part of this text.
        desc: format!("{} track(s)", t.streams.len()),
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
        mirrors: None,
        size_bytes: None,
    });
    rows.extend(stream_rows(t, 0));

    let mut details = vec![
        format!("File: {name}"),
        format!("Duration: {}", fmt_dur(t.duration_secs)),
        format!("Streams: {}", t.streams.len()),
    ];
    details.extend(crate::rip_keys::render_trace(&trace));
    let key_summary = match found.as_ref().map(|s| s.status().origin) {
        Some(Some(w)) => format!("unlocked via {w}"),
        Some(None) => "unlocked".into(),
        None => "unencrypted".into(),
    };
    Ok(Scanned {
        selection_model: fe::SelectionModel::from_titles(std::slice::from_ref(t)),
        label: name,
        // A container source has no volume id; `run_stream` names its output
        // from the file's own stem instead.
        volume_id: String::new(),
        title_count: 1,
        key_summary,
        video_codecs: vec![
            t.video_streams()
                .next()
                .map(|v| v.codec.to_string())
                .unwrap_or_default(),
        ],
        title_sizes: Vec::new(),
        capacity_bytes: 0,
        // A container is ONE title and it is the file itself; there is no
        // number to carry across a re-scan, but the shape stays the same as
        // the disc scan's so the request never has to special-case it.
        title_ids: vec![TitleIdentity::of(t)],
        rows,
        details,
        keys: None,
        needs_disc: false,
        refusal: None,
    })
}

/// A loose `.m2ts` clip's keys, looked up only from its disc folder (1.8.0, as the CLI's
/// `loose_clip_keys`): never user-supplied. Other sources and raw reads look nothing up.
fn loose_clip_keys(
    path: &str,
    raw: bool,
    keys: &KeyConfig,
    halt: &libfreemkv::Halt,
) -> (
    libfreemkv::Result<Option<libfreemkv::keys::KeyRing>>,
    crate::rip_keys::Trace,
) {
    if raw || crate::ui::container_scheme(path) != Some("m2ts") {
        return (Ok(None), crate::rip_keys::Trace::new());
    }
    fe::resolve_loose_clip(std::path::Path::new(path), &key_factory(keys), Some(halt))
}

/// Scan a source (ISO path today) and flatten it into display rows.
// Consumed by the library's integration tests (`tests/engine_bridge.rs`) and
// the platform unit tests, not by the binary — which compiles this module too
// (`main.rs` has its own `mod engine`), so the bin build sees it as dead.
#[allow(dead_code)]
pub fn scan(path: &str) -> Result<Scanned, String> {
    scan_with_keys(path, &KeyConfig::default())
}

/// One open's own Stop token and progress (stop design v5 §4.3, "The open token"): a user
/// Open or the launch probe. The source changing, the window closing or Quit cancels it.
#[derive(Clone, Default)]
pub struct OpenToken {
    pub halt: libfreemkv::Halt,
    pub progress: libfreemkv::halt::Liveness,
}

/// Scan an image and resolve its main title's keys once (KU §2.5 "GUI open"), so the key
/// strip reflects a real resolution; the set seeds the rip. Key bytes are never logged.
pub fn scan_with_keys(path: &str, keys: &KeyConfig) -> Result<Scanned, String> {
    scan_with_keys_under(path, keys, &OpenToken::default())
}

/// [`scan_with_keys`] under an open's token: its Stop ends the key lookup mid-flight.
pub fn scan_with_keys_under(
    path: &str,
    keys: &KeyConfig,
    tok: &OpenToken,
) -> Result<Scanned, String> {
    // Another source replaces the disc Open held for Start.
    HOLD.release();
    // A FOLDER is an image-level source too: "Open Folder" / drag-and-drop.
    let src = fe::ImageSource::from_path(path);
    let (disc, _reader) = fe::scan_image(&src).map_err(|e| format!("E{} scan failed", e.code()))?;
    let main = fe::resolve_selection(&disc, &fe::Selection::MainMovie);
    let o = crate::rip_keys::ImageOpen {
        scope: libfreemkv::keys::KeyScope::Titles(main.clone()),
        seed: None,
        drive_disc: None,
        halt: Some(tok.halt.clone()),
    };
    let (opened, trace) =
        crate::rip_keys::open_image(&src, crate::rip_keys::sources(&key_params(keys)), o);
    stopped_open(opened.map(|o| o.keys), tok)
        .map(|keys| scanned_with_keys(&disc, keys, &trace, main))
}

// Stop design v5 §4.3: a cancelled open ends `Halted` with no scan, so no key strip update.
fn stopped_open<T>(
    r: libfreemkv::Result<T>,
    tok: &OpenToken,
) -> Result<libfreemkv::Result<T>, String> {
    match r {
        Err(e) if tok.halt.is_cancelled() && matches!(e, libfreemkv::Error::Halted) => {
            Err(format!("E{} {}", e.code(), explain(e.code())))
        }
        r => Ok(r),
    }
}

/// A scan's display rows, plus its key set or refusal and the walk behind it.
fn scanned_with_keys(
    disc: &libfreemkv::Disc,
    keys: libfreemkv::Result<libfreemkv::keys::KeyRing>,
    trace: &crate::rip_keys::Trace,
    titles: Vec<usize>,
) -> Scanned {
    let set = keys.as_ref().ok();
    let mut sc = scanned_from_disc(disc, key_summary(disc, set));
    sc.details.extend(crate::rip_keys::render_trace(trace));
    if let Some(note) = set.and_then(|s| crate::rip_keys::best_effort_note(&s.status())) {
        sc.details.push(note);
    }
    match keys {
        Ok(set) => sc.keys = Some(set),
        Err(e) => {
            let code = e.code();
            let text = format!("E{code} {}", explain(code));
            sc.details.push(text.clone());
            sc.needs_disc = crate::rip_keys::needs_disc(&e);
            sc.refusal = (!sc.needs_disc).then_some(KeyRefusal {
                code,
                text,
                titles,
                transport: code == libfreemkv::error::E_KEY_SERVICE_UNAVAILABLE,
            });
        }
    }
    sc
}

/// The `freemkv info -v` detail block for a scanned disc/ISO — the same facts
/// the CLI prints (format, capacity, region, MKB version, disc hash, VID, key
/// state, title list), as log lines the desktop app shows on open.
fn disc_details(disc: &libfreemkv::Disc, key_summary: &str) -> Vec<String> {
    let mut d = Vec::new();
    d.push(format!("Type: {}", format_name(&disc.format)));
    if disc.capacity_bytes > 0 {
        let gb = disc.capacity_bytes as f64 / 1_000_000_000.0;
        d.push(format!("Capacity: {gb:.1} GB, {} layer(s)", disc.layers));
    }
    match &disc.region {
        libfreemkv::disc::DiscRegion::Free => d.push("Region: free".to_string()),
        libfreemkv::disc::DiscRegion::Unknown => d.push("Region: unknown".to_string()),
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
        // Per-disc bus-encryption flag isn't surfaced here — Type: Uhd
        // already signals AACS 2.0, and whether the drive actually applies
        // bus encryption is an internal detail.
        d.push(format!("MKB v{}", aacs.mkb_version.unwrap_or(0)));
        d.push(format!("Disc hash: {}", aacs.disc_hash));
        if aacs.volume_id.iter().any(|&b| b != 0) {
            let vid: String = aacs.volume_id.iter().map(|b| format!("{b:02x}")).collect();
            d.push(format!("VID: 0x{vid}"));
        }
    }
    d.push(format!("Protection: {key_summary}"));
    // Just the count — the per-title list lives in the UI tree, no need to
    // duplicate it in the log.
    d.push(format!("Titles: {}", disc.titles.len()));
    d
}

/// Build the title tree + info rows from a scanned `Disc`. Shared by the ISO
/// (`scan_with_keys`) and live-drive (`scan_disc_with_keys`) paths so a disc
/// looks identical whether it came from a file or a physical drive.
fn scanned_from_disc(disc: &libfreemkv::Disc, summary: String) -> Scanned {
    let mut rows = Vec::new();
    // The volume id is untrusted disc bytes that become `Scanned.label`, fed
    // to the log pane and disc row text. Sanitise ONCE here, at the
    // boundary, rather than at each of `say()`'s call sites.
    let label = {
        let cleaned = sanitize_display(&disc.volume_id);
        if cleaned.trim().is_empty() {
            "(no label)".to_string()
        } else {
            cleaned
        }
    };

    rows.push(Row {
        type_s: "Disc".into(),
        item: label.clone(),
        format: format_name(&disc.format),
        notes: summary.clone(),
        desc: label.clone(),
        depth: 0,
        checkable: false,
        title: usize::MAX,
        info: format!(
            "Disc information\n\nType: {}\nLabel: {}\nProtection: {}\nTitles: {}",
            format_name(&disc.format),
            label,
            summary,
            disc.titles.len()
        ),
        pid: None,
        duration_secs: 0.0,
        lang: String::new(),
        forced: false,
        mirrors: None,
        size_bytes: None,
    });

    let roles = freemkv_engine::title_roles(&disc.titles);
    for (ti, t) in disc.titles.iter().enumerate() {
        // A DVD title's "playlist" is a name the scan makes up (VTS_xx_y.VOB), naming no file
        // on the disc, so only a real playlist name (untrusted disc bytes, sanitized) is shown.
        let notes = title_notes(t, disc.format != libfreemkv::DiscFormat::Dvd, roles[ti]);
        rows.push(Row {
            type_s: "Title".into(),
            item: title_item(ti),
            format: String::new(),
            desc: format!("{}  —  {notes}", title_item(ti)),
            notes,
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
            mirrors: None,
            size_bytes: Some(t.size_bytes),
        });
        rows.extend(stream_rows(t, ti));
        rows.extend(chapter_rows(t, ti));
    }

    let details = disc_details(disc, &summary);
    Scanned {
        selection_model: fe::SelectionModel::from_disc(disc),
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
        title_sizes: disc.titles.iter().map(|t| t.size_bytes).collect(),
        capacity_bytes: disc.capacity_bytes,
        title_ids: disc.titles.iter().map(TitleIdentity::of).collect(),
        rows,
        details,
        keys: None,
        needs_disc: false,
        refusal: None,
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

/// Whether the disc behind a `disc://` source is still in a drive, `None` when a drive
/// gives no clear answer. Bare `disc://` asks every drive, as its autodetect would.
/// Media presence only (IOKit registry on macOS, TEST UNIT READY elsewhere): no
/// exclusive open, so it never takes the drive from a later open.
/// A disc Open holds for Start is asked through that handle: macOS hides held media from
/// the registry, so the registry would read it as ejected.
#[cfg_attr(test, allow(dead_code))] // unit tests swap in a fake that touches no drive
pub fn disc_present(source: &str) -> Option<bool> {
    disc_present_with(&HOLD, source, registry_disc_present)
}

// `disc_present` over a hold and a registry probe (seams for the tests).
fn disc_present_with(
    hold: &DriveHold,
    source: &str,
    registry: impl FnOnce(&str) -> Option<bool>,
) -> Option<bool> {
    match hold.presence(source, std::time::Instant::now()) {
        Some(verdict) => verdict,
        None => registry(source),
    }
}

#[cfg_attr(test, allow(dead_code))]
fn registry_disc_present(source: &str) -> Option<bool> {
    let paths = match disc_device(source) {
        Some(p) => vec![p],
        None => libfreemkv::list_drives()
            .into_iter()
            .map(|d| d.path)
            .collect(),
    };
    let mut unknown = false;
    for p in paths {
        match libfreemkv::drive_has_disc(std::path::Path::new(&p)) {
            Ok(true) => return Some(true),
            Ok(false) => {}
            Err(_) => unknown = true,
        }
    }
    (!unknown).then_some(false)
}

/// `DeviceTarget` for a `disc://` source: an explicit path, or autodetect.
fn disc_target(source: &str) -> libfreemkv::DeviceTarget {
    match disc_device(source) {
        Some(p) => libfreemkv::DeviceTarget::Path(p.into()),
        None => libfreemkv::DeviceTarget::Autodetect,
    }
}

// Eject through the session's own handle (stop design §2.5), never a second open: a
// Stopped rip's token refuses a fresh `Drive::eject`, and macOS allows only one open.
// Failure goes to the log (the "auto_eject is on but disc stayed put" symptom).
fn eject_disc(session: libfreemkv::DiscSession, sink: &UiSink) {
    use freemkv_engine::Sink as _;
    let device = session.device_path().to_string();
    match session.finish(libfreemkv::Finish::Eject) {
        Ok(()) => sink.log(fe::Level::Info, &format!("ejected {device}")),
        Err(e) => sink.log(fe::Level::Warn, &format!("eject failed: {e}")),
    }
}

// Done with the drive: eject through the handle the session holds, else release it.
fn end_drive(session: libfreemkv::DiscSession, eject: bool, sink: &UiSink) {
    if eject {
        eject_disc(session, sink);
    }
}

// The session's drive as a borrowed reader, leaving the handle in the session.
fn held_source(
    session: &mut libfreemkv::DiscSession,
) -> Result<&mut dyn libfreemkv::SectorSource, String> {
    session
        .source_mut()
        .ok_or_else(|| "could not stage the drive".to_string())
}

// The per-title rip hands each title's handle to its mux, so its eject opens the drive
// once more; the eject itself still ends through `finish`.
fn eject_device(device: &str, sink: &UiSink) {
    use freemkv_engine::Sink as _;
    match fe::drive::open(std::path::Path::new(device)) {
        Ok(drive) => eject_disc(libfreemkv::DiscSession::from_drive(drive), sink),
        Err(e) => sink.log(
            fe::Level::Warn,
            &format!("eject skipped — drive open failed: {e}"),
        ),
    }
}

/// Eject the disc behind a `disc://` source on demand (the Eject command).
/// Bare `disc://` resolves like a rip's `DeviceTarget::Autodetect`: the drive
/// holding media. `Ok` carries the device that was ejected.
pub fn eject_source(source: &str) -> Result<String, String> {
    eject_held_or(&HOLD, source, |dev| match dev {
        Some(p) => fe::drive::open(std::path::Path::new(p)).map_err(|e| format!("{e}")),
        None => libfreemkv::find_drive()
            .ok_or_else(|| "No drive with a disc found — nothing to eject.".to_string()),
    })
}

// The disc Open holds for `source` ejects through that handle, never a second open (macOS
// allows one); a hold on another source is released first so `open` can have its drive.
fn eject_held_or(
    hold: &DriveHold,
    source: &str,
    open: impl FnOnce(Option<&str>) -> Result<libfreemkv::Drive, String>,
) -> Result<String, String> {
    match hold.take().filter(|h| h.source == source) {
        Some(h) => eject_source_with(source, |_| Ok(h.drive)),
        None => eject_source_with(source, open),
    }
}

/// What Open leaves open for Start, so Start does not open, scan and resolve again (the
/// server's "Reusing drive session"): the drive, Open's scan and Open's key set.
struct HeldDrive {
    serial: u64,
    source: String,
    drive: libfreemkv::Drive,
    disc: libfreemkv::Disc,
    /// Open's set; `None` when Open's resolve refused.
    keys: Option<KeySet>,
    /// The key settings Open scanned under (the drive's host certs come from them).
    config: KeyConfig,
    /// The open's own token: the shell cancels it on Close, another Open or Quit.
    open: libfreemkv::Halt,
    /// The last time the idle disc watch found the disc in the drive.
    renewed: std::time::Instant,
}

/// The idle disc watch renews the hold every `PRESENCE_EVERY`; a hold it stopped renewing
/// (the source was closed) is released after this.
const HOLD_LEASE: std::time::Duration = std::time::Duration::from_secs(30);
const HOLD_REAP_EVERY: std::time::Duration = std::time::Duration::from_millis(500);
/// After a release macOS takes a moment to publish the media again, so the registry reads
/// a disc as gone; a presence check this soon after one says "unknown" instead.
const HOLD_SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

/// The one drive Open holds (one open of a drive at a time on macOS). Released by another
/// Open, Eject, the disc leaving, a Start that cannot reuse it, the open's token, or the lease.
struct DriveHold {
    slot: Mutex<HoldSlot>,
    next: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct HoldSlot {
    held: Option<HeldDrive>,
    /// The source last released and when, for `HOLD_SETTLE`.
    released: Option<(String, std::time::Instant)>,
}

static HOLD: DriveHold = DriveHold::new();

/// Close: release the drive Open held, off the caller's thread (closing the handle can
/// block on the drive). Nothing held, nothing done.
pub fn release_held_drive() {
    let _ = std::thread::Builder::new()
        .name("release-held-drive".into())
        .spawn(|| HOLD.release());
}

impl DriveHold {
    const fn new() -> Self {
        DriveHold {
            slot: Mutex::new(HoldSlot {
                held: None,
                released: None,
            }),
            next: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HoldSlot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    // Hold `h`, releasing any earlier hold; its generation, for the reaper.
    fn hold(&self, mut h: HeldDrive) -> u64 {
        h.serial = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let serial = h.serial;
        let mut slot = self.lock();
        slot.release();
        slot.held = Some(h);
        serial
    }

    fn take(&self) -> Option<HeldDrive> {
        self.lock().held.take()
    }

    fn release(&self) {
        self.lock().release();
    }

    /// The held drive's answer for `source`: `None` when nothing is held for it (a hold on
    /// another source is released), so the registry answers. The disc leaving releases it.
    fn presence(&self, source: &str, now: std::time::Instant) -> Option<Option<bool>> {
        use libfreemkv::DriveStatus as S;
        let mut slot = self.lock();
        let Some(h) = slot.held.as_mut().filter(|h| h.source == source) else {
            slot.release();
            let settling = slot.released.as_ref().is_some_and(|(s, at)| {
                s == source && now.saturating_duration_since(*at) < HOLD_SETTLE
            });
            return settling.then_some(None);
        };
        match h.drive.drive_status() {
            S::DiscPresent => {
                h.renewed = now;
                Some(Some(true))
            }
            S::NoDisc | S::TrayOpen => {
                slot.release();
                Some(Some(false))
            }
            S::NotReady | S::Unknown => Some(None),
        }
    }

    /// The reaper's step for hold `serial`: release it once its open was cancelled or its lease
    /// lapsed. Whether that hold is still there to watch.
    fn expire(&self, serial: u64, now: std::time::Instant) -> bool {
        let mut slot = self.lock();
        let Some(h) = slot.held.as_ref().filter(|h| h.serial == serial) else {
            return false;
        };
        if h.open.is_cancelled() || now.saturating_duration_since(h.renewed) >= HOLD_LEASE {
            slot.release();
            return false;
        }
        true
    }
}

impl HoldSlot {
    // Dropping the drive closes its handle.
    fn release(&mut self) {
        if let Some(h) = self.held.take() {
            self.released = Some((h.source, std::time::Instant::now()));
        }
    }
}

// Open's scanned session as a hold: the drive gets a token of its own (the open's is cancelled
// when the shell drops that open, and would refuse Start's reads) and its tray unlocked, as it
// was before the hold, so the drive's button still ejects.
fn held_drive(
    source: &str,
    session: libfreemkv::DiscSession,
    disc: libfreemkv::Disc,
    keys: Option<KeySet>,
    config: &KeyConfig,
    open: &libfreemkv::Halt,
) -> Option<HeldDrive> {
    let mut drive = session.into_drive().ok()?;
    drive.attach(&libfreemkv::Halt::new());
    drive.attach_progress(&libfreemkv::halt::Liveness::new());
    drive.unlock_tray();
    Some(HeldDrive {
        serial: 0,
        source: source.to_string(),
        drive,
        disc,
        keys,
        config: config.clone(),
        open: open.clone(),
        renewed: std::time::Instant::now(),
    })
}

// Hold Open's drive for Start, with a reaper for the lease. A cancelled open holds nothing.
fn hold_for_start(hold: &'static DriveHold, h: Option<HeldDrive>) {
    let Some(h) = h.filter(|h| !h.open.is_cancelled()) else {
        return;
    };
    let serial = hold.hold(h);
    let reaper = std::thread::Builder::new()
        .name("held-drive".into())
        .spawn(move || {
            while hold.expire(serial, std::time::Instant::now()) {
                std::thread::sleep(HOLD_REAP_EVERY);
            }
        });
    if reaper.is_err() {
        hold.release();
    }
}

/// Open's drive for this rip, when it still holds the disc Open scanned under the same
/// source and key settings, for a scan of the same kind (a raw copy scans differently).
/// Anything else is dropped here, closing the handle before the fresh open.
fn reuse_held(
    held: Option<HeldDrive>,
    req: &RipRequest,
    raw_copy: bool,
    sink: &UiSink,
) -> Option<(libfreemkv::DiscSession, libfreemkv::Disc, Option<KeySet>)> {
    use freemkv_engine::Sink as _;
    let mut h = held.filter(|h| h.source == req.source && h.config == req.keys && !raw_copy)?;
    if !same_disc(&mut h.drive, &h.disc) {
        sink.log(
            fe::Level::Info,
            "the disc changed since it was opened; scanning it again",
        );
        return None;
    }
    sink.log(fe::Level::Info, "reusing the drive and scan from Open");
    let mut session = libfreemkv::DiscSession::from_drive(h.drive);
    // As `fe::open_scan` does after its scan: the disc cannot leave mid-rip.
    session.lock_tray();
    Some((session, h.disc, h.keys))
}

// Whether the drive still holds the disc Open scanned: present, with the same volume id and
// capacity (`Disc::identify`, a few sectors, not a scan).
fn same_disc(drive: &mut libfreemkv::Drive, disc: &libfreemkv::Disc) -> bool {
    if drive.drive_status() != libfreemkv::DriveStatus::DiscPresent {
        return false;
    }
    matches!(libfreemkv::Disc::identify(drive),
        Ok(id) if id.volume_id == disc.volume_id && id.capacity_sectors == disc.capacity_sectors)
}

// Whether Open's set already keys everything this rip reads (the server's `keys_cover`): the
// same disc, the rip's scope, and AACS keys for an AACS disc's titles.
fn open_keys_cover(
    disc: &libfreemkv::Disc,
    set: &KeySet,
    scope: &libfreemkv::keys::KeyScope,
) -> bool {
    let aacs_titles = disc.aacs.is_some() && *scope != libfreemkv::keys::KeyScope::None;
    set.is_for(&disc.media_id()) && set.covers(scope) && (set.is_aacs() || !aacs_titles)
}

// `eject_source` over an injected open (`None` = autodetect): one open, then the eject
// through that handle's `finish`.
fn eject_source_with(
    source: &str,
    open: impl FnOnce(Option<&str>) -> Result<libfreemkv::Drive, String>,
) -> Result<String, String> {
    let session = libfreemkv::DiscSession::from_drive(open(disc_device(source).as_deref())?);
    let device = session.device_path().to_string();
    session
        .finish(libfreemkv::Finish::Eject)
        .map_err(|e| format!("{e}"))?;
    Ok(device)
}

/// Host certs / credentials for the AACS bus handshake, from the keydb — the
/// same input the CLI's `drive_credentials` builds. Passed to the shared `fe::open_scan`.
fn session_credentials(keys: &KeyConfig) -> Option<libfreemkv::DriveCredentials> {
    let path = key_settings(keys).keydb_path?;
    let host_certs = freemkv_keysources::KeydbSource::new(path).host_certs();
    (!host_certs.is_empty()).then_some(libfreemkv::DriveCredentials { host_certs })
}

// Normalize the GUI's KeyConfig into the engine's KeyParams. Preserves the
// GUI's shellexpand of keydb_path and explicit online_only toggle; empty
// keydb_path/keyserver_url maps to None (no default-location fallback).
fn key_params(keys: &KeyConfig) -> freemkv_engine::KeyParams {
    crate::plan_core::key_params(&key_settings(keys)).params()
}

/// The app's key settings as the front-end-neutral ones: the settings' keydb path (`~`
/// expanded), its key service and token, and the key-source dropdown.
pub fn key_settings(keys: &KeyConfig) -> crate::plan_core::KeySettings {
    let keydb_path = (!keys.keydb_path.trim().is_empty())
        .then(|| crate::settings::shellexpand(&keys.keydb_path));
    // Gated on the dropdown, not just on "is a URL configured": "Local keydb only" must
    // drop the online source even when a URL is saved.
    let mode = if keys.local_only {
        crate::plan_core::KeyMode::LocalOnly
    } else if keys.online_only {
        crate::plan_core::KeyMode::OnlineOnly
    } else {
        crate::plan_core::KeyMode::Both
    };
    crate::plan_core::KeySettings {
        keydb_path,
        key_url: Some(keys.keyserver_url.clone()),
        key_auth: Some(keys.keyserver_token.clone()),
        mode,
    }
}

/// The engine plan for `req` writing `dest` (an output URL): the app's half of the one
/// plan parser every front end shares.
pub fn gui_plan(req: &RipRequest, dest: &str) -> freemkv_engine::Plan {
    let titles = if req.titles.is_empty() {
        fe::Selection::MainMovie
    } else {
        fe::Selection::Titles(req.titles.clone())
    };
    crate::plan_core::plan(crate::plan_core::PlanRequest {
        source: source_url(&req.source),
        dest: dest.to_string(),
        titles,
        streams: fe::StreamChoice::default(),
        raw: req.raw,
        multipass: req.multipass,
        keys: key_settings(&req.keys),
        force: req.force,
    })
}

// The output a request writes, as a URL: its folder for a tree or per-title files, the
// folder an image lands in for an ISO.
fn plan_dest(req: &RipRequest) -> String {
    let scheme = match out_kind(&req.format) {
        OutKind::DecryptedFolder => "dir",
        OutKind::IsoImage => "iso",
        OutKind::Demux(s) | OutKind::File(s) => s,
    };
    format!("{scheme}://{}", req.dest_dir)
}

// The app's source as a URL: a drive is `disc://…`, an image `iso://`, a folder `dir://`.
fn source_url(source: &str) -> String {
    if source.starts_with("disc://") {
        source.to_string()
    } else {
        format!("{}://{}", image_or_dir_scheme(source), source)
    }
}

/// The app's line for the plan it runs, in the run log. Exhaustive on purpose (anti-drift
/// §2): a field added to the engine's `Plan` fails to compile here until the app handles it.
pub fn plan_line(p: &freemkv_engine::Plan) -> String {
    let freemkv_engine::Plan {
        source,
        dest,
        titles,
        streams,
        raw,
        multipass,
        keys,
        force,
    } = p;
    let freemkv_engine::KeyParamsData {
        keydb_path,
        key_url,
        key_auth,
        online_only,
        cert_keydb,
    } = keys;
    format!(
        "plan: {source} -> {dest} titles={titles:?} streams_all={} raw={raw} multipass={multipass} \
         force={force} keydb={} online={} auth={} online_only={online_only} certs={}",
        streams.is_all(),
        keydb_path.is_some(),
        key_url.is_some(),
        key_auth.is_some(),
        cert_keydb.is_some(),
    )
}

/// The key sources a rip asks, from the user's settings (the same ones the ISO path uses).
fn key_factory(keys: &KeyConfig) -> libfreemkv::KeySourceFactory {
    crate::rip_keys::sources(&key_params(keys))
}

/// Scan a live optical drive (`disc://<device>` or bare `disc://` autodetect) with no key
/// call, then resolve the main title's keys once (KU §2.5 "GUI open"): the SAME `Scanned`
/// shape the ISO path returns, seed set included. The drive, the scan and the set stay held
/// for Start ([`HeldDrive`]). NEEDS HARDWARE to exercise.
pub fn scan_disc_with_keys(
    source: &str,
    keys: &KeyConfig,
    tok: &OpenToken,
) -> Result<Scanned, String> {
    // An earlier Open's drive first: this open may be of the same drive.
    HOLD.release();
    let (disc, mut session) = drive_scan(source, keys, tok)?;
    let main = fe::resolve_selection(&disc, &fe::Selection::MainMovie);
    let scope = libfreemkv::keys::KeyScope::Titles(main.clone());
    let (set, trace) = crate::rip_keys::resolve_observed(
        &disc,
        held_source(&mut session)?,
        scope,
        &key_factory(keys),
        &tok.halt,
        &tok.progress,
    );
    let set = stopped_open(set, tok)?;
    let kept = set.as_ref().ok().cloned();
    let sc = scanned_with_keys(&disc, set, &trace, main);
    let held = held_drive(source, session, disc, kept, keys, &tok.halt);
    hold_for_start(&HOLD, held);
    Ok(sc)
}

/// Open and scan the drive behind `source` with NO key call (`fe::open_scan_with`) under
/// the open's token (stop design v5 §4.3): the disc, and the session holding the drive.
fn drive_scan(
    source: &str,
    keys: &KeyConfig,
    tok: &OpenToken,
) -> Result<(libfreemkv::Disc, libfreemkv::DiscSession), String> {
    // The cold keydb parse is work with no progress signal; T29 must not fire on it.
    let credentials = {
        let _busy = tok.progress.busy();
        session_credentials(keys)
    };
    let mut session = fe::open_scan_with(
        disc_target(source),
        credentials,
        false,
        &tok.halt,
        &tok.progress,
    )
    .map_err(|e| drive_error(&e))?;
    let disc = session.take_disc().ok_or("scan produced no disc")?;
    Ok((disc, session))
}

// "No optical drive found" for autodetect with nothing attached; else the error itself.
fn drive_error(e: &libfreemkv::Error) -> String {
    match e {
        libfreemkv::Error::DeviceNotFound { path } if path.is_empty() => {
            "No optical drive found. Connect a Blu-ray/DVD drive with a disc.".to_string()
        }
        _ => explain(e.code()),
    }
}

// `what` and the catalog explanation of a library failure: `Display` alone is the bare E-code.
fn failed_with(what: &str, e: &libfreemkv::Error) -> String {
    format!("{what}: {}", explain(e.code()))
}

/// Ask the engine whether a job can run, without executing it.
// See `scan` above: used by the integration/platform tests, dead in the bin.
#[allow(dead_code)]
pub fn preflight(path: &str, dest: &str, titles: &[usize]) -> Result<Vec<String>, String> {
    preflight_with_keys(path, dest, titles, None)
}

/// Preflight with the key set Open resolved (`seed`), making NO key request: KU §2.5, the
/// Open result "seeds the rip's `resolve`", so Open, preflight and Run resolve once. With
/// no set the engine reports the missing key; the decrypt judgment stays in the engine.
pub fn preflight_with_keys(
    path: &str,
    dest: &str,
    titles: &[usize],
    seed: Option<&libfreemkv::keys::KeyRing>,
) -> Result<Vec<String>, String> {
    // Folder OR image: a preflight that cannot open a folder reports a spurious failure
    // for a source the rip itself handles.
    let (disc, _reader) =
        fe::scan_image(&fe::ImageSource::from_path(path)).map_err(|e| format!("E{}", e.code()))?;
    let sel = if titles.is_empty() {
        fe::Selection::MainMovie
    } else {
        fe::Selection::Titles(titles.to_vec())
    };
    // Folder OR image — the scan above already dispatches on it, and a
    // preflight run against `iso://<folder>` answers about a source that does
    // not exist in that form.
    let mut job =
        fe::Job::new(format!("{}://{path}", image_or_dir_scheme(path)), dest).with_selection(sel);
    job.keys = seed.cloned();
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
    /// The top bar: the current title's percent, or the current pass's outside a title.
    pub title_pct: f64,
    /// When the top bar's title, or the run's read, began.
    pub title_started: Option<std::time::Instant>,
    /// The bottom bar: the run's bytes done over its planned bytes, never decreasing.
    /// `None` when the run planned no titles; `Some(0.0)` while a planned size is unknown.
    pub batch_pct: Option<f64>,
}

/// The two bars' bookkeeping, updated in the worker's event order so the UI never
/// samples a title boundary half-way.
#[derive(Default)]
struct Bars {
    /// `(title index, size_bytes)` of each title the run muxes; empty when unplanned.
    sizes: Vec<(usize, u64)>,
    /// The read that precedes the titles, when the run has one.
    read: Option<ReadPass>,
    /// Sizes of the titles the run has moved past, whatever their outcome.
    passed: u64,
    /// The title being muxed: index, size, and its fraction done (never decreasing).
    current: Option<(usize, u64, f64)>,
}

/// The pre-title read, measured by its first pass; later passes leave it complete.
#[derive(Default)]
struct ReadPass {
    pass: Option<String>,
    done: u64,
    total: u64,
    over: bool,
}

impl Bars {
    fn size_of(&self, idx: usize) -> u64 {
        self.sizes
            .iter()
            .find(|&&(i, _)| i == idx)
            .map_or(0, |&(_, s)| s)
    }

    fn tick(&mut self, p: &fe::Progress) {
        if let Some((_, _, frac)) = &mut self.current {
            if p.bytes_total > 0 {
                let f = (p.bytes_done as f64 / p.bytes_total as f64).min(1.0);
                *frac = frac.max(f);
            }
            return;
        }
        let Some(r) = self.read.as_mut().filter(|r| !r.over) else {
            return;
        };
        match &r.pass {
            None => {
                r.pass = Some(p.pass.to_string());
                r.total = p.bytes_total;
                r.done = p.bytes_done.min(r.total);
            }
            Some(k) if **k == *p.pass => {
                if r.total == 0 {
                    r.total = p.bytes_total;
                }
                r.done = r.done.max(p.bytes_done.min(r.total));
            }
            Some(_) => r.done = r.total,
        }
    }

    fn end_read(&mut self) {
        if let Some(r) = &mut self.read {
            r.done = r.total;
            r.over = true;
        }
    }

    // The bottom bar's percent: `None` unplanned, `Some(0.0)` while a size is unknown.
    fn batch_pct(&self) -> Option<f64> {
        if self.sizes.is_empty() {
            return None;
        }
        if self.sizes.iter().any(|&(_, s)| s == 0) {
            return Some(0.0);
        }
        let (read_done, read_total) = self.read.as_ref().map_or((0, 0), |r| (r.done, r.total));
        let total = read_total + self.sizes.iter().map(|&(_, s)| s).sum::<u64>();
        let current = self.current.map_or(0.0, |(_, size, f)| size as f64 * f);
        let done = (read_done + self.passed) as f64 + current;
        Some((done / total as f64 * 100.0).min(100.0))
    }

    fn publish(&self, prog: &mut Prog) {
        prog.title_pct = match self.current {
            Some((_, _, f)) => f * 100.0,
            // A pre-title read is its own visible pass.  Keep showing that
            // pass until it ends; the per-title bar resets only between
            // authored titles.
            None if self.read.as_ref().is_some_and(|r| !r.over) && prog.bytes_total > 0 => {
                (prog.bytes_done as f64 / prog.bytes_total as f64 * 100.0).min(100.0)
            }
            // A completed title has cleared `current`, but its last engine
            // progress sample is still 100%.  Do not leak that stale sample
            // into the per-title bar while the next title is being opened.
            None if !self.sizes.is_empty()
                && self.passed < self.sizes.iter().map(|&(_, s)| s).sum::<u64>() =>
            {
                0.0
            }
            None if !self.sizes.is_empty() => 100.0,
            None if prog.bytes_total > 0 => {
                (prog.bytes_done as f64 / prog.bytes_total as f64 * 100.0).min(100.0)
            }
            None => 0.0,
        };
        prog.batch_pct = match (self.batch_pct(), prog.batch_pct) {
            (Some(now), Some(was)) => Some(now.max(was)),
            (now, _) => now,
        };
    }
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
    /// The run refused E7034: only the disc's Volume ID can finish the key (KU §4.2), so
    /// the shell's next Start is the insert-the-disc Retry.
    pub needs_disc: AtomicBool,
    /// Lock order: `bars`, then `prog`.
    bars: Mutex<Bars>,
}

impl RunState {
    // Applies `f` to the bars and republishes them into `prog`, in one critical section.
    fn with_bars(&self, f: impl FnOnce(&mut Bars, &mut Prog)) {
        let mut bars = self.bars.lock().unwrap_or_else(|e| e.into_inner());
        let mut prog = self.prog.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut bars, &mut prog);
        bars.publish(&mut prog);
    }

    /// Plans the bottom bar over the titles the run muxes, as `(index, size_bytes)` from
    /// the engine's scan. The first plan of a run stands.
    pub fn plan_titles(&self, sizes: Vec<(usize, u64)>) {
        self.with_bars(|b, _| {
            if b.sizes.is_empty() {
                b.sizes = sizes;
            }
        });
    }

    /// The run reads the planned titles in one pass set before muxing them: the
    /// bottom bar counts that read's bytes too.
    pub fn begin_read(&self) {
        self.with_bars(|b, p| {
            b.read = Some(ReadPass::default());
            p.title_started = Some(std::time::Instant::now());
        });
    }

    /// Title `idx` starts: the top bar restarts at 0 for it.
    pub fn title_start(&self, idx: usize) {
        self.with_bars(|b, p| {
            if b.current.is_some_and(|(i, _, _)| i == idx) {
                return;
            }
            b.end_read();
            b.current = Some((idx, b.size_of(idx), 0.0));
            p.bytes_done = 0;
            p.bytes_total = 0;
            p.eta_secs = None;
            p.title_started = Some(std::time::Instant::now());
        });
    }

    /// Title `idx` is over, whatever its outcome: the bottom bar counts all of it.
    pub fn title_end(&self, idx: usize) {
        self.with_bars(|b, _| {
            if let Some((i, size, _)) = b.current
                && i == idx
            {
                b.passed += size;
                b.current = None;
            }
        });
    }

    /// One engine progress tick, as the run's sink receives it.
    pub fn progress(&self, p: &fe::Progress) {
        self.with_bars(|b, prog| {
            prog.bytes_done = p.bytes_done;
            prog.bytes_total = p.bytes_total;
            prog.speed_bps = p.speed_bps;
            prog.eta_secs = p.eta_secs;
            prog.sectors_bad = p.sectors_bad;
            b.tick(p);
        });
    }

    /// The run's verdict, recovering from a poisoned lock rather than
    /// defaulting.
    ///
    /// A poisoned mutex still holds the last value written to it; recovering
    /// and reading it preserves that value instead of silently reporting
    /// `RunOutcome::Completed` after a worker panic. The same poison-recovery
    /// is applied to every `lines` lock in the crate.
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
/// Polls `finished` rather than joining the thread — the worker publishes
/// state through `RunState` and nothing hands the UI a `JoinHandle`.
/// `finished` is set only after the worker's drop guard has closed and
/// finalized the output file, so this is exactly the "output is on disk and
/// closed" edge callers need. Safe to call from the main thread: the worker
/// touches only atomics/mutexes on `RunState` and never calls back into the UI.
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

/// `(index, size_bytes)` of each of `indices` on `disc`, for [`RunState::plan_titles`].
fn title_sizes(disc: &libfreemkv::Disc, indices: &[usize]) -> Vec<(usize, u64)> {
    indices
        .iter()
        .map(|&i| (i, disc.titles.get(i).map_or(0, |t| t.size_bytes)))
        .collect()
}

/// One title of a front-end title loop, for the bars: started on creation, over on drop.
struct TitleBars<'a>(&'a RunState, usize);

impl<'a> TitleBars<'a> {
    fn start(state: &'a RunState, idx: usize) -> Self {
        state.title_start(idx);
        Self(state, idx)
    }
}

impl Drop for TitleBars<'_> {
    fn drop(&mut self) {
        self.0.title_end(self.1);
    }
}

// The GUI core's ONLY `Sink`; both methods recover a poisoned lock rather than skip the write —
// a poisoned `lines` mutex means a worker panicked, exactly when the log matters most.
impl fe::Sink for UiSink {
    fn log(&self, _level: fe::Level, msg: &str) {
        self.0
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(msg.to_string());
    }
    fn progress(&self, p: &fe::Progress) {
        self.0.progress(p);
    }
    fn should_cancel(&self) -> bool {
        self.0.cancel.load(Ordering::Relaxed)
    }
    // G4/D4: the pre-mux note at the output opening, the hook the CLI prints it from.
    fn event(&self, e: &fe::Event<'_>) {
        match e {
            fe::Event::OutputOpened { dest, title } => {
                let note = crate::lossy::excluded_lines(dest, title);
                self.0
                    .lines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(note);
            }
            fe::Event::TitleStart { idx, .. } => self.0.title_start(*idx),
            fe::Event::TitleDone { idx, .. } => self.0.title_end(*idx),
            _ => {}
        }
    }
}

// Describe the disc's key state from the resolved set (KU §11.6: "The key count in `info`
// and the GUI comes from the resolved set"), never from keys banked on the disc.
pub(crate) fn key_summary(
    disc: &libfreemkv::Disc,
    set: Option<&libfreemkv::keys::KeyRing>,
) -> String {
    if !disc.encrypted {
        return "unencrypted".into();
    }
    if disc.css.is_some() {
        return "CSS (DVD)".into();
    }
    match set.filter(|s| s.is_aacs() && s.is_for(&disc.media_id())) {
        Some(s) => match s.status().origin {
            Some(w) => format!("unlocked via {w}"),
            None => "unlocked".into(),
        },
        None => "locked — no key yet".into(),
    }
}

/// Recover the library's numeric error code from a muxed `std::io::Error`.
///
/// The library's `Display` is `E<code>` or `E<code>: <data>` (no English by
/// design), so this parses the leading digit run rather than the whole
/// string. Returns `0` if there is no code (or it exceeds `u16`, like the libs').
pub fn error_code(e: &std::io::Error) -> u16 {
    fe::error_code(e).unwrap_or(0)
}

// Map a recovery sweep's terminal flags to the run's result before the ISO
// is muxed, or None to proceed. Twin of the CLI's pipe::copy_verdict:
// aborted_for_loss returns Err — Ok here once hid a loss as a completed rip.
fn recovery_terminal_result(
    halted: bool,
    aborted_for_loss: bool,
    want_iso: bool,
    iso_path: &str,
) -> Option<Result<String, String>> {
    if halted {
        Some(Ok(if want_iso {
            format!("Cancelled — partial ISO kept: {iso_path}")
        } else {
            format!("Cancelled — nothing muxed; partial ISO kept: {iso_path}")
        }))
    } else if aborted_for_loss {
        Some(Err(if want_iso {
            format!("Recovery aborted — too much unreadable data; partial ISO kept: {iso_path}")
        } else {
            format!(
                "Recovery aborted — too much unreadable data to mux a complete title \
                 (raise the lost-seconds tolerance to accept it, or re-run recovery). \
                 Partial ISO kept: {iso_path}"
            )
        }))
    } else {
        None
    }
}

/// Turn a library error code into something a person can act on.
///
/// Routes through the catalog (`error.E<code>`) so failures are localized in
/// all 29 locales instead of hard-coded English. Codes whose catalog string
/// needs runtime data unavailable here (E7022's `{hash}`, E8005's `{detail}`)
/// map to the placeholder-free `error.E7018`. A code with no catalog string
/// keeps its number for a bug report, via a localizable wrapper.
pub fn explain(code: u16) -> String {
    // E7022 needs `{hash}`, which isn't available here; E8005 likewise
    // needs `{detail}`. Both map to the argument-free no-key sibling so
    // nothing renders a literal `{hash}`.
    let key_code = match code {
        7022 | 8005 => 7018,
        c => c,
    };
    let msg = crate::strings::error_message(u32::from(key_code));
    if msg != format!("error.E{key_code}") && !msg.contains('{') {
        return msg;
    }
    // No catalog string (or one that still needs an argument): keep the ORIGINAL
    // code visible for a bug report, inside a localizable wrapper rather than
    // reintroducing hard-coded English.
    crate::strings::fmt_or(
        "error.mux_failed_generic",
        "Mux failed (E{code}).",
        &[("code", &code.to_string())],
    )
}

// Explain why a title died, from RipOutcome::Failed's two-part cause. A
// typed library failure has a code and `explain` handles it; a genuine OS
// error has no E<code> (code: None) and keeps its meaning in `kind`.
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
/// `Err` is not decoration: only [`fe::RipOutcome::Ok`] and a cancel are
/// successes, so `NoKey` and `Failed` must return `Err` even after some
/// titles already wrote — otherwise a rip the engine stopped mid-way reports
/// as if it worked. Pure so the mapping is testable without a disc; both
/// title-loop call sites (staged-ISO and single-pass) share it so they
/// cannot drift apart.
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
            ..
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
        fe::RipOutcome::Ok { .. } => Ok(with_partial(format!(
            "{written} title(s) written to {dest_dir}"
        ))),
    }
}

/// Render a finished single-file/container mux (`run_stream`'s `mkv://` /
/// `m2ts://` / `mp4://` source path) as the run summary.
///
/// Grades on `outcome.completed` (Cancel mid-conversion must not read as
/// success) and on [`crate::lossy::is_lossy`] for a lossy-but-complete mux.
/// The excluded/lossy header is appended, not substituted: the file IS
/// written, it is simply not everything the user asked for. Pure so the
/// mapping is unit-testable without driving a real mux.
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
            // Container-agnostic wording (like the CLI's `lossy_lines`): an
            // undelivered stream isn't mp4-specific, so `mp4.excluded_header`'s
            // "in an MP4" phrasing is wrong for mkv/m2ts. English until catalog.
            crate::strings::fmt_or(
                "mux.undelivered_header",
                "Note: {count} stream(s) could not be delivered and were left out:",
                &[("count", &n.to_string())]
            )
        )
    } else {
        // Bytes lost inside the tracks: no stream is missing, so the
        // excluded-tracks wording would be wrong. Name the loss itself.
        let lines = crate::lossy::lossy_lines(outcome, target);
        if lines.is_empty() {
            // `is_lossy` was true but produced no describable line — append
            // nothing rather than leaving a dangling " —" on the message.
            format!("Written to {dest_dir}")
        } else {
            format!("Written to {dest_dir} — {}", lines.join(" "))
        }
    }
}

/// The lines a GUI run must add when a completed mux did not deliver
/// everything — the front-end twin of the CLI's `pipe::print_lossy_outcome`,
/// sharing the same renderer ([`crate::lossy::lossy_lines`]).
///
/// A warning on a still-successful rip, not a failure: the file is
/// finalised, structurally valid and playable. Returns a `Vec` rather than
/// pushing, so the formatting is testable without a `RipState`.
pub fn lossy_lines(outcome: &libfreemkv::MuxOutcome, target: &str) -> Vec<String> {
    crate::lossy::lossy_lines(outcome, target)
}

/// Render a finished image decrypt (`iso://` -> `iso://`, drive-free) as the
/// run summary — sibling of [`summarize_stream`] and [`summarize_extract`].
///
/// Branches on `halted` first, then on `complete` — never a re-derivation of
/// either (see [`fe::CopyResult`]'s doc). `complete` means "nothing pending
/// AND nothing lost AND not interrupted"; both shortfalls are named in the
/// message when it is false. The partial image is kept in every case.
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

/// Render a finished decrypted-folder extraction (`run_extract_folder`) as
/// the run summary — the `dir://` analogue of `summarize_outcome`.
///
/// Grades on `res.halted`, the same signal `pipe.rs`'s `extract_succeeded`
/// gates the CLI's exit code on for this `libfreemkv::ExtractResult`, so a
/// Cancel mid-extraction cannot read as a complete write. Pure so the
/// mapping is unit-testable without a real disc/extraction.
pub fn summarize_extract(res: &libfreemkv::ExtractResult, dest: &std::path::Path) -> String {
    let n = res.files.len();
    if res.halted {
        format!(
            "Cancelled — {n} file(s) extracted to {} before stopping",
            dest.display()
        )
    } else if res.bytes_unreadable > 0 {
        format!(
            "Decrypted file tree written to {} — {} file(s), {:.1} MiB unreadable",
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

/// A rip's up-front AACS key set (KU §2.1), held in memory only.
pub type KeySet = libfreemkv::keys::KeyRing;

/// Key configuration taken from the user's settings.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
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
/// One representation used to carry two meanings: a title's absence in a
/// bare `Vec<(usize, Vec<u16>, Vec<u16>)>` meant either "no per-title data,
/// use the `audio_pids`/`sub_pids` union" or "the user emptied this title" —
/// and the union won both, handing an emptied title its sibling's tracks.
/// Splitting the two into variants makes the ambiguity unrepresentable: a
/// caller with no breakdown says so, one with a breakdown is believed.
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
    /// The key set Open resolved: the rip asks only for what it lacks (KU §2.5).
    pub seed: Option<libfreemkv::keys::KeyRing>,
    /// The insert-the-disc Retry after E7034 (KU §4.2 Q4): the `disc://` drive holding
    /// the image's disc, scanned with no key call for its Volume ID.
    pub vid_from: Option<String>,
}

/// Run the real rip on a worker thread: engine title loop + per-title mux.
/// Returns immediately.
pub fn start_rip(req: RipRequest, state: Arc<RunState>) {
    let _detached = start_rip_with(req, state, run_blocking);
}

// `start_rip` over an injected run: the worker's verdict and summary path (FT4).
fn start_rip_with(
    req: RipRequest,
    state: Arc<RunState>,
    run: impl FnOnce(&RipRequest, &UiSink, &Arc<RunState>) -> Result<String, String> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    // Sets `finished` on EVERY exit — normal return, an early `?`, or a panic unwinding
    // through. Used to be the closure's last statement, so a panic left `finished` unset and
    // `ui::tick` polled forever.
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
            // `Release`, not `Relaxed`: `finished` is the flag `ui::tick` polls
            // before reading `summary`/`outcome`. `Release` guarantees the
            // mutex writes above are visible to any `Acquire`-load of `finished`.
            self.0.finished.store(true, Ordering::Release);
        }
    }

    std::thread::spawn(move || {
        let _done = SignalDone(state.clone());
        let sink = UiSink(state.clone());
        // The plan this run carries out, in the run log (the one plan parser every front
        // end shares).
        let plan = gui_plan(&req, &plan_dest(&req));
        state
            .lines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(plan_line(&plan));
        let res = run(&req, &sink, &state);
        let cancelled = state.cancel.load(Ordering::Relaxed);
        if let (Err(e), false) = (&res, cancelled) {
            state
                .lines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(e.clone());
        }
        let (text, verdict) = run_verdict(res, cancelled);
        *state
            .summary
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = text;
        *state
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = verdict;
    })
}

/// A finished run's summary and verdict. A user stop is resumable, not a failure, even
/// when it ended the run with an error (a Stop during the up-front resolve): `cancelled`
/// is the typed signal the sink polls, so it beats reading the prose.
fn run_verdict(res: Result<String, String>, cancelled: bool) -> (String, RunOutcome) {
    match res {
        Ok(s) if cancelled => (s, RunOutcome::Cancelled),
        Ok(s) => (s, RunOutcome::Completed),
        Err(_) if cancelled => (
            crate::strings::get("rip.interrupted"),
            RunOutcome::Cancelled,
        ),
        Err(e) => (e, RunOutcome::Failed),
    }
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

// Extract the disc's decrypted UDF file tree to a per-disc SUBDIRECTORY of
// the destination (the CLI's `dir://` -> `Disc::extract_tree`). Targeting a
// subdir (never the raw dest_dir) stops it dumping into ~/Movies.
fn run_extract_folder(
    req: &RipRequest,
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    label: &str,
    set: &libfreemkv::keys::KeyRing,
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
    let plan = fe::Plan {
        raw: false,
        multipass: false,
        ..gui_plan(req, &format!("dir://{}", dest.display()))
    };
    let with = fe::RunWith {
        keys: Some(set.clone()),
        held: Some(fe::Held::Disc { disc, reader }),
        ..fe::RunWith::default()
    };
    let extracted = match fe::run_with(&plan, with, sink) {
        Ok(fe::Report::Tree { extract }) => Ok(extract),
        Ok(_) => Err(libfreemkv::Error::StreamUrlInvalid {
            url: plan.dest.clone(),
        }),
        Err(e) => Err(e),
    };
    match extracted {
        Ok(res) => {
            for f in &res.files {
                if f.bytes_unreadable > 0 {
                    state
                        .lines
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(format!(
                            "  {} — {:.1} MiB unreadable",
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
    crate::ui::container_scheme(path).is_some()
}

// The URL scheme for a source already established as neither a drive nor a stream container: a
// FOLDER is dir://, anything else is an image (iso://). Deliberately NOT source_scheme.
fn image_or_dir_scheme(source: &str) -> &'static str {
    if std::path::Path::new(source).is_dir() {
        "dir"
    } else {
        "iso"
    }
}

fn source_scheme(path: &str) -> &'static str {
    // A container by the one table (G5), else an image: never a guessed m2ts.
    crate::ui::container_scheme(path).unwrap_or("iso")
}

// What real operation an output format maps to. The picker offers twelve
// format strings; six of them used to fall through to a per-title MKV mux.
// Each now resolves to its true sink so the file matches what was chosen.
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

// Map a picker format string to its real output kind. Order matters only in
// that each branch's marker is unique across the twelve format strings —
// `"video tracks"` (lower-case) cannot be confused with `"Video index → .fvi"`.
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
    } else if format.contains("MPG") {
        OutKind::File("mpg")
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

/// The path this request will actually write, decided the way the RIP
/// decides it rather than guessed alongside it.
///
/// The GUI shows this in the Information panel for the run's duration, so
/// any difference from the real thing is a lie on screen for hours. Mirrors
/// `run_blocking`'s dispatch and `out_kind`'s arms, so a new sink cannot be
/// added without this seeing it.
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
        // The SHOWN path, with the same forward slash the File/IsoImage arms
        // use — `extract_target`'s PathBuf renders a Windows backslash. The
        // real extraction path stays `extract_target` (native separators).
        OutKind::DecryptedFolder => format!("{dest_dir}/{}", sanitize_label(&label)),
    }
}

/// The word the progress caption uses for what a format actually writes
/// ("Saving to {container} file").
///
/// Derived from `out_kind`, deliberately: the caption and the sink then
/// cannot disagree, because they are the same decision read twice. The UI's
/// own version tested for MP4, then M2TS, then said MKV — so nine of the
/// twelve offered formats, ISO and JSON and .fvi among them, were captioned
/// with a container they never produce.
pub fn container_word(format: &str) -> &'static str {
    match out_kind(format) {
        OutKind::File("mp4") => "MP4",
        OutKind::File("mpg") => "MPG",
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

/// Re-export of the shared display sanitiser (see
/// [`crate::strings::sanitize_display`]). Named here because this is where the
/// disc-bytes-to-UI boundary lives.
pub use crate::strings::sanitize_display;

/// Make a disc-supplied label safe to use as ONE filename component.
///
/// The volume label is disc bytes — untrusted; a label containing `..\..\`
/// could escape the destination directory on Windows via `title_basename`.
///
/// Deliberately NARROWER than the CLI's `sanitize_name`: that ASCII
/// allow-list also drops apostrophes/colons/periods and collapses any
/// non-Latin label to `"disc"`. This rejects only the path-y and the
/// unrepresentable, keeping the letters (including non-Latin ones).
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

/// Build a per-title output basename from the filename template. `{title}` →
/// the disc/volume label (or container name), `{n}` → the 1-based title number.
/// An empty template falls back to the historical `<label>_t<n>`; a template
/// with no `{n}` gets `_t<n>` appended so multi-title output can never collide.
/// Path separators a user might type are neutralized to keep output in-folder.
pub fn title_basename(template: &str, label: &str, n: usize) -> String {
    // Sanitise ONCE, up front, for every branch. The `{title}` substitution
    // below used to be the only sanitised path, leaving the DEFAULT
    // (empty-template) case joining the raw disc label into the output path.
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
    // `{title}` was sanitised, but the template TEXT itself can carry separators
    // (`../`, `foo/bar`) from the setting, which would escape the folder. Sanitise
    // the assembled name so the "kept in-folder" promise holds on every branch.
    sanitize_label(&name)
}

// The stream filter for one title (or the whole request with no title), handed to the title's
// run (`title_options`).
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
        // Each title's selection reaches its run through `title_options`.
        selection: libfreemkv::StreamSelection::default(),
        title_index: 0,
    }
}

// A container source is a single title — no scan, straight to the mux.
// NOTE: per-track ticks are NOT honoured here yet (verified empirically);
// the caller warns the user rather than silently writing deselected tracks.
fn run_stream(req: &RipRequest, sink: &UiSink, state: &Arc<RunState>) -> Result<String, String> {
    std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;
    let name = std::path::Path::new(&req.source)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("output")
        .to_string();
    let src_url = format!("{}://{}", source_scheme(&req.source), req.source);

    // A container is a single title. Route to the chosen sink; the whole-disc
    // operations have no meaning for one media file.
    let (dest_url, target) = match out_kind(&req.format) {
        OutKind::File(scheme) => {
            // A container is one title (n = 1); honor the template with the
            // file's own name as {title}.
            let base = title_basename(&req.filename_template, &name, 1);
            let out = format!("{}/{}.{}", req.dest_dir, base, scheme);
            // Every sink truncates its output before the source is read.
            if crate::file_identity::same_file(
                Some(std::path::Path::new(&req.source)),
                std::path::Path::new(&out),
            ) {
                return Err("The output would overwrite the source. \
                     Choose a different output folder."
                    .into());
            }
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
    // Keys before any output: a clip none are found for refuses E7022 in the mux's open.
    let watch = CancelWatch::new(state);
    let (found, trace) = loose_clip_keys(&req.source, req.raw, &req.keys, &watch.halt);
    drop(watch);
    log_walk(&trace, sink);
    let found = found.map_err(|e| key_refusal(&e, std::path::Path::new(&req.source), state))?;
    let plan = title_plan(req, &src_url, &dest_url, 0, req.raw);
    let with = fe::RunWith {
        keys: found,
        // Track selection is not applied to container sources yet (the note above).
        title: fe::TitleOptions {
            selection: Some(libfreemkv::StreamSelection::default()),
            ..title_options(req, None)
        },
        ..fe::RunWith::default()
    };
    let o = stopped_before_output(run_title(&plan, with, sink))
        .map_err(|e| format!("convert failed: {e}"))?;
    if !o.completed {
        // Recovering, like every other `lines` lock in this file. A worker
        // that panicked earlier poisons `lines`, so `unwrap()` here would turn
        // one dead thread into a second panic while reporting the partial file.
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

// Whether a demux rip must give each title its own subdirectory. A demux
// sink names files by TRACK not title, so two titles in one dir overwrite
// each other's tracks. Extracted so the `> 1` boundary is testable alone.
fn demux_needs_subdirs(title_count: usize) -> bool {
    title_count > 1
}

// What the image path hands title `idx`'s run. Named (not a struct literal in a closure)
// because two fields fail silently if missing: keys (E7022) and selection (wrong tracks).
fn image_title_run(
    set: &libfreemkv::keys::KeyRing,
    req: &RipRequest,
    idx: usize,
) -> fe::RunWith<'static> {
    fe::RunWith {
        keys: Some(set.clone()),
        title: title_options(req, Some(idx)),
        ..fe::RunWith::default()
    }
}

// Mux the selected titles from `source_url` (original ISO or a staging ISO
// multipass just produced) into the sinks the format maps to. Shared by
// run_blocking's ISO path and run_disc's staging-ISO path — same loop.
fn mux_selected_titles(
    disc: &libfreemkv::Disc,
    source_url: &str,
    req: &RipRequest,
    indices: &[usize],
    set: &libfreemkv::keys::KeyRing,
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
    state.plan_titles(title_sizes(disc, indices));
    let written = std::cell::Cell::new(0usize);
    let partial = std::cell::Cell::new(0usize);
    let outcome = fe::run_titles(indices, !req.titles.is_empty(), sink, |idx| {
        let _bars = TitleBars::start(state, idx);
        // Destination per output kind: a per-title file for the container /
        // metadata / index sinks, or a demux directory (its own per-track
        // naming) for separate track files.
        let (dest_url, target) = title_dest(req, kind, &label, idx, multi);
        let before = output_stamp(&target);
        // Every title reads through the rip's one set (no lookup); an image title is never
        // raw here.
        let plan = title_plan(req, source_url, &dest_url, idx, false);
        let with = image_title_run(set, req, idx);
        let muxed = stopped_before_output(run_title(&plan, with, sink));
        match muxed {
            Ok(o) => {
                if !o.completed {
                    // Cancelled or truncated: a partial file is on disk. Keep it,
                    // don't count it as a full write, and SAY it's partial —
                    // never "nothing written" when a file is in the folder.
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
                    remove_failed_output(&target, before);
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

// Human time for a lost-playback duration, e.g. "4m" or "12.4s". A tiny
// local copy of the CLI's pipe::fmt_damage_time — pipe.rs is CLI-only and
// not part of this crate's lib target, so it can't be reused directly.
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
    // Folder OR image — `Ui::open` already scans a folder through `scan_dir`,
    // so without this the GUI listed a folder's titles and then failed the
    // moment the user pressed Rip.
    let src_path = std::path::Path::new(&req.source);
    whole_image_gate(&req.format, src_path)?;
    let src = fe::ImageSource::from_path(src_path);
    let (disc, _reader) = fe::scan_image(&src).map_err(|e| format!("E{} scan failed", e.code()))?;
    let kind = out_kind(&req.format);
    let whole = matches!(kind, OutKind::DecryptedFolder | OutKind::IsoImage);
    let indices = if whole {
        Vec::new()
    } else {
        // The ticked numbers were resolved against `Ui::open`'s scan; this is a
        // different one, taken now — a re-authored image would renumber titles
        // under a stale selection.
        let scanned: Vec<TitleIdentity> = disc.titles.iter().map(TitleIdentity::of).collect();
        verify_selection_identity(&req.titles, &req.title_ids, &scanned)?;
        let sel = if req.titles.is_empty() {
            fe::Selection::MainMovie
        } else {
            fe::Selection::Titles(req.titles.clone())
        };
        let indices = fe::resolve_selection(&disc, &sel);
        // A staged image holds only the titles it was staged for (checked by extents, so the
        // staging mux's re-mapped selection passes); anything else would mux zero-fill.
        fe::ensure_titles_staged(src_path, &disc, &indices).map_err(|e| gui_error(&e, src_path))?;
        state
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("selection resolved to titles {indices:?}"));
        require_selection(whole, &indices)?;
        indices
    };
    // KU §2.1 invariant 1: ONE resolve over what this rip decrypts, before any output,
    // seeded with Open's set; the Retry after E7034 brings the drive's scan (its VID).
    let scope = if whole {
        libfreemkv::keys::KeyScope::WholeDisc
    } else {
        libfreemkv::keys::KeyScope::Titles(indices.clone())
    };
    let opened = open_rip_image(req, &src, scope, sink, state)?;
    let set = opened.keys.clone();
    let disc = opened.disc;
    let mut reader = opened.reader;
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
            return run_extract_folder(req, &disc, reader.as_mut(), &label, &set, sink, state);
        }
        OutKind::IsoImage => {
            // Decrypt an image without the disc: `iso://In.iso iso://Out.iso`.
            // Single-pass, always: multipass is a DRIVE strategy for re-reading
            // bad sectors, and `recover_to_iso` refuses it without `raw` set.
            let dest =
                std::path::Path::new(&req.dest_dir).join(format!("{}.iso", sanitize_label(&label)));
            // Never write over the source. Uses the SHARED guard, not a local
            // canonical-path comparison, which cannot see a hardlink (two
            // names for one inode).
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
            // One plain pass over the opened image, read through the rip's set; the app
            // holds the image's lock.
            let plan = fe::Plan {
                source: format!("{}://{}", image_or_dir_scheme(&req.source), req.source),
                raw: req.raw,
                multipass: false,
                ..gui_plan(req, &format!("iso://{}", dest.display()))
            };
            let lock = hold_iso_lock(&dest, state)?;
            let with = fe::RunWith {
                keys: Some(set),
                held: Some(fe::Held::Disc {
                    disc: &disc,
                    reader: reader.as_mut(),
                }),
                locked: true,
                ..fe::RunWith::default()
            };
            let copied = match fe::run_with(&plan, with, sink) {
                Ok(fe::Report::Image { copy, .. }) => Ok(copy),
                Ok(_) => Err(libfreemkv::Error::StreamUrlInvalid {
                    url: plan.dest.clone(),
                }),
                Err(e) => Err(e),
            };
            let halted = matches!(&copied, Ok(r) if r.halted);
            let res = copied
                .map_err(|e| failed_with("image decrypt failed", &e))
                .and_then(|result| {
                    if recovery_produced_no_data(result.bytes_good) {
                        let _ = std::fs::remove_file(&dest);
                        return Err("No readable data — no image was written.".into());
                    }
                    Ok(summarize_image_decrypt(&result, &dest))
                });
            release_iso_lock(lock, &res, halted, &dest, state);
            return res;
        }
        _ => {}
    }
    drop(reader);
    // A folder is `dir://`, an image `iso://`. Hardcoding `iso://` here meant
    // the mux re-opened a folder as an image file and failed after a successful
    // scan and key resolution.
    let src_url = format!("{}://{}", image_or_dir_scheme(&req.source), req.source);
    mux_selected_titles(&disc, &src_url, req, &indices, &set, sink, state)
}

// Take `<iso>.lock` for the whole write under the run's Stop (stop design v5 §2.5); a
// frozen holder reads as `stop.artifact_lock_failed` (E9073), a Stop as a cancel.
fn hold_iso_lock(
    iso: &std::path::Path,
    state: &Arc<RunState>,
) -> Result<libfreemkv::io::ArtifactLock, String> {
    let watch = CancelWatch::new(state);
    crate::artifact_lock::hold_iso(iso, &watch.halt).map_err(|e| {
        crate::artifact_lock::lock_failed(&e, iso)
            .unwrap_or_else(|| format!("E{} {}", e.code(), explain(e.code())))
    })
}

// §2.5: "Deleted on success … Kept after Stop, a failure or a crash"; an image this run
// removed guards nothing, so its sidecar goes too.
fn release_iso_lock(
    lock: libfreemkv::io::ArtifactLock,
    res: &Result<String, String>,
    halted: bool,
    iso: &std::path::Path,
    state: &Arc<RunState>,
) {
    let done = (res.is_ok() && !halted) || !iso.exists();
    if halted && !done {
        let kept = crate::strings::get_or("stop.progress_kept", "Progress kept");
        state
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(kept);
    }
    crate::artifact_lock::release(lock, done);
}

/// Open the rip's image and resolve its keys once (`rip_keys::open_image`), seeded with
/// Open's set; the Retry's drive scan supplies the VID. A refusal is logged with its
/// walk; E7034 also says to insert the disc and flags the Retry.
fn open_rip_image(
    req: &RipRequest,
    src: &fe::ImageSource,
    scope: libfreemkv::keys::KeyScope,
    sink: &UiSink,
    state: &Arc<RunState>,
) -> Result<fe::OpenedImage, String> {
    // A Retry stays the Retry until the key sources answered it (no drive, the wrong disc
    // or a Stop leaves it armed): KU §4.2 "Retry does the same `open_scan` and re-open".
    if req.vid_from.is_some() {
        state.needs_disc.store(true, Ordering::SeqCst);
    }
    // The Retry opens a drive; a disc an earlier Open still holds would refuse that open.
    if req.vid_from.is_some() {
        HOLD.release();
    }
    let drive_disc = match &req.vid_from {
        Some(drive) => Some(
            crate::rip_keys::drive_scan(drive, session_credentials(&req.keys))
                .map_err(|e| drive_error(&e))?,
        ),
        None => None,
    };
    let watch = CancelWatch::new(state);
    let o = crate::rip_keys::ImageOpen {
        scope,
        seed: req.seed.clone(),
        drive_disc,
        halt: Some(watch.halt.clone()),
    };
    let (opened, trace) = crate::rip_keys::open_image(src, key_factory(&req.keys), o);
    drop(watch);
    log_walk(&trace, sink);
    if req.vid_from.is_some() && opened.as_ref().map_or_else(answered, |_| true) {
        state.needs_disc.store(false, Ordering::SeqCst);
    }
    let opened = opened.map_err(|e| key_refusal(&e, src.path(), state))?;
    note_best_effort(&opened.keys, state);
    Ok(opened)
}

// Whether the key sources answered a resolve with a verdict (a missing key, a source or
// keydb failure), as opposed to it never running: a drive, disc-identity or Stop error.
fn answered(e: &libfreemkv::Error) -> bool {
    use libfreemkv::error as c;
    let code = e.code();
    matches!(
        code,
        c::E_NO_DISC_KEY
            | c::E_WHOLE_DISC_KEY_MISSING
            | c::E_FMTS_KEY_MISSING
            | c::E_DECRYPT_FAILED
    ) || (c::E_KEY_SERVICE_UNAVAILABLE..=c::E_KEY_SERVICE_RATE_LIMITED).contains(&code)
        || (8000..9000).contains(&code)
}

/// The resolution's per-source walk, in the run log: why a key is missing (labels only).
fn log_walk(trace: &crate::rip_keys::Trace, sink: &UiSink) {
    use fe::Sink as _;
    for line in crate::rip_keys::render_trace(trace) {
        sink.log(fe::Level::Info, &line);
    }
}

/// KU §2.6: an HD DVD set applied without proof says so in the run log.
fn note_best_effort(set: &libfreemkv::keys::KeyRing, state: &Arc<RunState>) {
    if let Some(note) = crate::rip_keys::best_effort_note(&set.status()) {
        state
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(note);
    }
}

/// A key refusal as the run's error text; E7034 adds the insert-the-disc step (KU §4.2)
/// and flags the shell's Retry.
fn key_refusal(e: &libfreemkv::Error, src: &std::path::Path, state: &Arc<RunState>) -> String {
    let msg = gui_error(e, src);
    if !crate::rip_keys::needs_disc(e) {
        return msg;
    }
    state.needs_disc.store(true, Ordering::SeqCst);
    format!("{msg}\n{}", insert_disc_retry())
}

/// E7034's next step in the app (KU §4.2 GUI row: “An "Insert the disc" prompt … and Retry”;
/// JUDGEMENT against Q4: no drive picker, the drive with media is autodetected).
pub fn insert_disc_retry() -> String {
    crate::strings::get_or(
        "gui.log.insert_disc_retry",
        "Insert the disc into a drive, then choose “Start rip” again. Only its Volume ID is \
         read; it is not ripped again.",
    )
}

/// A `Halt` that follows the run's Stop button (`RunState::cancel`) until dropped, so a
/// resolve or a mux the GUI starts outside the engine stops like one inside it.
struct CancelWatch {
    halt: libfreemkv::Halt,
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CancelWatch {
    fn new(state: &Arc<RunState>) -> Self {
        let halt = libfreemkv::Halt::new();
        let done = Arc::new(AtomicBool::new(false));
        let (h, d, st) = (halt.clone(), done.clone(), state.clone());
        let thread = std::thread::spawn(move || {
            while !d.load(Ordering::SeqCst) {
                if st.cancel.load(Ordering::SeqCst) {
                    h.cancel();
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        if state.cancel.load(Ordering::SeqCst) {
            halt.cancel();
        }
        CancelWatch {
            halt,
            done,
            thread: Some(thread),
        }
    }
}

impl Drop for CancelWatch {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
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

// Which of those two a request wants: want_iso || multipass (not &&). As &&
// a --multipass title rip silently loses recovery passes; an ISO request
// without multipass would reach `title_dest`'s unreachable!().
fn recovery_plan(kind: OutKind, multipass: bool) -> DiscPlan {
    let deliver_iso = matches!(kind, OutKind::IsoImage);
    if deliver_iso || multipass {
        DiscPlan::Recover { deliver_iso }
    } else {
        DiscPlan::PerTitle
    }
}

// The `raw` flag a recovery job carries: the user's, except that an image staged for a title
// mux stays RAW on disk (decrypted on the ordinary iso:// re-open into the container). A
// multipass recovery to an ISO decrypts unless the user kept it raw (EO6).
fn recovery_raw(multipass: bool, want_iso: bool, user_raw: bool) -> bool {
    (multipass && !want_iso) || user_raw
}

/// What a title NUMBER actually referred to, so a selection survives a
/// rescan. Lives in [`crate::title_identity`] because the CLI's
/// `pipe::resolve_scanned_title` asks the identical question one scan later.
///
/// Keys on playlist name, numeric id, and EXTENTS/sectors — the fields that
/// stay unique even when duplicate playlists share identical duration and
/// size. Re-exported here (not aliased) so callers and tests keep naming one
/// type.
pub use crate::title_identity::TitleIdentity;

// Confirm the title at `idx` in a FRESH scan is still the one the selection meant, before it is
// muxed under that number. Verifies rather than remaps: a moved title list between two scans is
// a disc/drive problem.
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

// Confirm a whole SELECTION still means what it meant when it was made — the window between
// ticking titles and pressing Start, which run_disc's fresh pre-mux scan can't see on its own.
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

// A title output with no title selected has nothing to rip; refuse before touching the drive.
// A whole-disc output ignores the selection.
fn require_selection(whole: bool, indices: &[usize]) -> Result<(), String> {
    if !whole && indices.is_empty() {
        return Err("Nothing selected to rip.".into());
    }
    Ok(())
}

/// A recovery that read nothing has nothing to mux. Separate from the caller so
/// the boundary is assertable: as `!=` a perfectly good recovery deletes its own
/// ISO and reports "no readable data".
fn recovery_produced_no_data(good_bytes: u64) -> bool {
    good_bytes == 0
}

// An image staged for an MKV rip holds only its titles: never a whole-disc (ISO or
// folder) source. Same engine check as the CLI's `iso://`/`dir://` destinations.
fn whole_image_gate(format: &str, src: &std::path::Path) -> Result<(), String> {
    if !matches!(
        out_kind(format),
        OutKind::DecryptedFolder | OutKind::IsoImage
    ) {
        return Ok(());
    }
    fe::ensure_whole_image(src).map_err(|e| gui_error(&e, src))
}

// An engine refusal about the image at `src`, localized with its path as `{detail}`.
fn gui_error(e: &libfreemkv::Error, src: &std::path::Path) -> String {
    let msg = crate::strings::error_message_with(u32::from(e.code()), &src.display().to_string());
    format!("E{} {msg}", e.code())
}

// Why "keep the image" was not honoured: the staging held only the chosen titles.
fn staging_not_kept_note(keep_iso: bool, scoped: bool) -> String {
    if !(keep_iso && scoped) {
        return String::new();
    }
    let note = crate::strings::get_or(
        "rip.staging_not_kept",
        "The staged image was not kept: the location of a stream file on this disc could not \
         be read, so only the chosen titles were read and it is not a whole-disc image.",
    );
    format!("\n{note}")
}

// The recovery job, carrying the picked titles so loss is judged over them, not title 0.
fn recovery_job(source: &str, iso_path: &str, indices: &[usize]) -> fe::Job {
    fe::Job::new(format!("disc://{source}"), iso_path.to_string())
        .with_selection(fe::Selection::Titles(indices.to_vec()))
}

// Remove the staging ISO and its mapfile sidecar.
fn remove_staging_iso(iso_path: &str, mapfile: &std::path::Path) {
    let _ = std::fs::remove_file(mapfile);
    let _ = std::fs::remove_file(iso_path);
}

// Whether the staging ISO is removed after the title mux. Three conditions, not one: keep_iso
// alone would delete the image on a cancel or failed mux, destroying the one artefact that lets
// the user retry.
fn should_delete_staging_iso(keep_iso: bool, mux_succeeded: bool, cancelled: bool) -> bool {
    !keep_iso && mux_succeeded && !cancelled
}

// Twin of the CLI's `disc_copy_scan_opts`: only a raw whole-disc ISO copy scans on past an
// unreadable AACS key file.
fn disc_raw_copy(kind: OutKind, raw: bool) -> bool {
    matches!(kind, OutKind::IsoImage) && raw
}

// Matches the CLI's `copy_verdict`: NOTHING readable is the only failure, and that (unusable)
// ISO is kept, not deleted. Any copy short of some sectors still succeeds and always names the
// loss and points at another run (shared `disc_copy_verdict` renderer, can't drift from the CLI).
fn iso_recovery_result(result: &fe::MultipassResult, iso_path: &str) -> Result<String, String> {
    if recovery_produced_no_data(result.good_bytes) {
        // Names where it was kept, like every other terminal message here does.
        return Err(format!(
            "{} ISO kept: {iso_path}",
            crate::disc_copy_verdict::iso_no_data_error(result.unreadable_bytes)
        ));
    }
    let mut note = damage_note(result);
    if result.unreadable_bytes > 0 || result.pending_bytes > 0 {
        note.push('\n');
        note.push_str(&crate::disc_copy_verdict::retry_with_multipass_hint());
    }
    Ok(format!("ISO image written to {iso_path}{note}"))
}

// Rip from a live optical drive (disc://). Scans once (no key call), resolves the rip's keys
// once, then runs the chosen sink via fe::run_titles (same loop the ISO path uses). NEEDS
// HARDWARE VALIDATION end-to-end.
fn run_disc(req: &RipRequest, sink: &UiSink, state: &Arc<RunState>) -> Result<String, String> {
    run_disc_scanning(req, sink, state, HOLD.take(), fe::open_scan)
}

/// What a whole-disc output of a drive decrypts (KU §2.5): nothing for a raw ISO copy
/// (“GUI "Keep encrypted", GUI `raw_copy`) | `None`: no key call”), else the whole disc.
fn disc_copy_scope(kind: OutKind, raw: bool) -> libfreemkv::keys::KeyScope {
    crate::rip_keys::copy_scope(disc_raw_copy(kind, raw))
}

/// The rip's one resolve over the drive (KU §2.1 invariant 1), seeded with Open's set and
/// stopped by the Stop button; its walk and any HD DVD note go to the run log.
fn disc_rip_keys(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
    scope: libfreemkv::keys::KeyScope,
    req: &RipRequest,
    sink: &UiSink,
    state: &Arc<RunState>,
) -> Result<libfreemkv::keys::KeyRing, String> {
    let watch = CancelWatch::new(state);
    let sources = key_factory(&req.keys);
    let seed = req.seed.as_ref();
    let (set, trace) =
        crate::rip_keys::resolve(disc, reader, scope, &sources, seed, Some(&watch.halt));
    drop(watch);
    log_walk(&trace, sink);
    let set = set.map_err(|e| key_refusal(&e, std::path::Path::new(&req.source), state))?;
    note_best_effort(&set, state);
    Ok(set)
}

/// [`run_disc`] with the drive scan as an injectable seam. Production always scans via
/// `fe::open_scan` (needs a live drive, so THAT path is untestable here); a test can stub
/// it and observe the exact `raw_copy` `run_disc` handed it — proving the wiring is real,
/// not just that `disc_raw_copy`'s own logic is right. `held` is what Open left open
/// ([`reuse_held`]): when it still fits, there is no open, no scan and, when Open's set
/// covers the rip, no resolve.
fn run_disc_scanning(
    req: &RipRequest,
    sink: &UiSink,
    state: &Arc<RunState>,
    held: Option<HeldDrive>,
    scan: impl FnOnce(
        libfreemkv::DeviceTarget,
        Option<libfreemkv::DriveCredentials>,
        bool,
    ) -> Result<libfreemkv::DiscSession, libfreemkv::Error>,
) -> Result<String, String> {
    if req.decrypt_threads > 0 {
        libfreemkv::set_decrypt_threads(req.decrypt_threads);
    }
    let kind = out_kind(&req.format);

    // Scan once (shared drive core, no key call): titles, label, per-disc name. Open's scan
    // when the drive Open held still has that disc.
    std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;
    let raw_copy = disc_raw_copy(kind, req.raw);
    let (mut session, disc, open_keys) = match reuse_held(held, req, raw_copy, sink) {
        Some(reused) => reused,
        None => {
            let mut session = scan(
                disc_target(&req.source),
                session_credentials(&req.keys),
                raw_copy,
            )
            .map_err(|e| drive_error(&e))?;
            let disc = session.take_disc().ok_or("scan produced no disc")?;
            (session, disc, None)
        }
    };
    // The resolved device path (autodetect included), for the per-title arm's eject.
    // The drive stays in `session`, read through `held_source`, so it ejects on this handle.
    let device = session.device_path().to_string();
    let label = if disc.volume_id.is_empty() {
        "disc".to_string()
    } else {
        disc.volume_id.clone()
    };
    // What THIS scan's numbers refer to. Banked once, before any branch: the
    // recovery arm remaps the selection against the staged image with it, and
    // the per-title arm verifies each re-scan against it.
    let scanned_ids: Vec<TitleIdentity> = disc.titles.iter().map(TitleIdentity::of).collect();
    // ...and the first thing it is used for is verifying it against the scan
    // the SELECTION was made against — everything between ticking titles and
    // pressing Start (a swapped disc, say) happened without a check until now.
    verify_selection_identity(&req.titles, &req.title_ids, &scanned_ids)?;
    // Which titles to rip — same Selection/resolve_selection the ISO path
    // uses, so a live-drive rip gets the same main-title default and
    // out-of-range filtering.
    let sel = if req.titles.is_empty() {
        fe::Selection::MainMovie
    } else {
        fe::Selection::Titles(req.titles.clone())
    };
    let indices = fe::resolve_selection(&disc, &sel);
    let whole = matches!(kind, OutKind::DecryptedFolder | OutKind::IsoImage);
    require_selection(whole, &indices)?;
    let scope = if whole {
        disc_copy_scope(kind, req.raw)
    } else {
        libfreemkv::keys::KeyScope::Titles(indices.clone())
    };
    // KU §2.1 invariant 1: every key this rip reads with, from ONE resolve, up front: Open's,
    // over this same held disc, when it covers the rip; else one seeded with Open's set.
    let set = match open_keys.filter(|k| open_keys_cover(&disc, k, &scope)) {
        Some(set) => {
            use freemkv_engine::Sink as _;
            sink.log(
                fe::Level::Info,
                "keys: the set Open resolved covers this rip; no second resolve",
            );
            note_best_effort(&set, state);
            set
        }
        None => disc_rip_keys(
            &disc,
            held_source(&mut session)?,
            scope.clone(),
            req,
            sink,
            state,
        )?,
    };

    // Decrypted folder: extract the UDF tree off the staged drive into a
    // per-disc subdir (same helper the ISO path uses).
    if matches!(kind, OutKind::DecryptedFolder) {
        crate::rip_keys::gate(&disc, false, Some(&set), &scope)
            .map_err(|e| gui_error(&e, std::path::Path::new(&req.source)))?;
        let reader = held_source(&mut session)?;
        let result = run_extract_folder(req, &disc, reader, &label, &set, sink, state);
        end_drive(session, req.auto_eject, sink);
        return result;
    }

    // True-multipass recovery, or a whole-disc ISO image: recover to a staged
    // ISO via fe::multipass_rip. It's ENCRYPTED (can't attribute sectors to a
    // title), so the mux runs the ordinary iso:// path with the same keys.
    let want_iso = matches!(kind, OutKind::IsoImage);
    if recovery_plan(kind, req.multipass) != DiscPlan::PerTitle {
        // Another label-into-a-path seam (ordinary drive -> ISO rip). Built
        // through `Path::join` rather than string interpolation so the type
        // system carries the boundary.
        let iso_path = std::path::Path::new(&req.dest_dir)
            .join(format!("{}.iso", sanitize_label(&label)))
            .to_string_lossy()
            .into_owned();
        // The user picked title NUMBERS against a scan; the re-scanned staged
        // image may list titles differently if damage dropped a playlist, so
        // `scanned_ids` lets the selection re-resolve by identity.
        let mut job = recovery_job(&req.source, &iso_path, &indices);
        job.raw = recovery_raw(req.multipass, want_iso, req.raw);
        // A decrypting copy reads through the set and refuses up front on anything it
        // lacks (E7026 for Pending forensic keys, KU §5.4); a raw one has no keys at all.
        if !job.raw {
            crate::rip_keys::gate(&disc, false, Some(&set), &scope)
                .map_err(|e| gui_error(&e, std::path::Path::new(&req.source)))?;
            job.keys = Some(set.clone());
        }
        let opts = fe::MultipassOpts {
            max_passes: req.max_passes,
            abort_on_lost_secs: req.abort_lost_secs,
            is_iso_output: want_iso,
        };
        // An MKV deliverable stages only nav/UDF + the chosen titles (never bus-encrypted,
        // AACS BD Pre-recorded 0.953 §3.7) unless a kept, whole image can be made.
        let staging = if want_iso {
            None
        } else {
            fe::mkv_staging_scope(&disc, held_source(&mut session)?, &indices, req.keep_iso)
                .map_err(|e| failed_with("recovery failed", &e))?
        };
        let lock = hold_iso_lock(std::path::Path::new(&iso_path), state)?;
        if !want_iso {
            state.plan_titles(title_sizes(&disc, &indices));
            state.begin_read();
        }
        let mut halted = false;
        let res = (|| -> Result<String, String> {
            // The engine's recovery over the held drive, into the image the app holds the
            // lock on: the sweep, the patch passes, the promotion and the loss gate.
            let plan = fe::Plan {
                source: source_url(&req.source),
                titles: job.selection.clone(),
                raw: job.raw,
                multipass: true,
                ..gui_plan(req, &format!("iso://{iso_path}"))
            };
            let with = fe::RunWith {
                keys: job.keys.clone(),
                held: Some(fe::Held::Disc {
                    disc: &disc,
                    reader: held_source(&mut session)?,
                }),
                passes: Some(opts),
                scope: staging.as_deref(),
                locked: true,
                ..fe::RunWith::default()
            };
            let result = match fe::run_with(&plan, with, sink) {
                Ok(fe::Report::Image {
                    recovery: Some(r), ..
                }) => Ok(r),
                Ok(_) => Err(libfreemkv::Error::StreamUrlInvalid {
                    url: plan.dest.clone(),
                }),
                Err(e) => Err(e),
            }
            .map_err(|e| failed_with("recovery failed", &e))?;
            halted = result.halted;
            // Read phase done: the deliverable (ISO) or the mux source is on disk,
            // so the drive is no longer needed — eject now, exactly like autorip
            // (which ejects at read-complete and muxes from the staged ISO).
            end_drive(session, req.auto_eject, sink);

            // Recovery verdicts are checked for BOTH output kinds, before the
            // want_iso split — the recovered image is kept in every case, so an
            // abort never throws away the read. See `recovery_terminal_result`.
            if let Some(terminal) = recovery_terminal_result(
                result.halted,
                result.aborted_for_loss,
                want_iso,
                &iso_path,
            ) {
                return terminal;
            }

            if want_iso {
                return iso_recovery_result(&result, &iso_path);
            }

            // Title output: mux the selected titles from the recovered (encrypted, see
            // `recovery_raw`) ISO with this rip's set and the drive's scan.
            if recovery_produced_no_data(result.good_bytes) {
                let map_path = disc.mapfile_for(std::path::Path::new(&iso_path));
                remove_staging_iso(&iso_path, &map_path);
                return Err("Recovery produced no readable data — nothing to mux.".into());
            }
            let map_path = disc.mapfile_for(std::path::Path::new(&iso_path));
            let mux = mux_staged_titles(req, &iso_path, disc, set, &indices, &label, sink, state);
            // The staged image is only disposable once the titles it was staged
            // for actually landed. `state.cancel` is the flag the Stop button
            // sets, read directly rather than inferred from the mux's summary.
            let cancelled = state.cancel.load(std::sync::atomic::Ordering::SeqCst);
            // A scoped staging image is not a disc image, so it is never kept (JUDGEMENT).
            let keep = req.keep_iso && staging.is_none();
            if should_delete_staging_iso(keep, mux.is_ok(), cancelled) {
                remove_staging_iso(&iso_path, &map_path);
            }
            if let Err(e) = &mux {
                return Err(format!("{e} — the recovered image is kept: {iso_path}"));
            }
            if cancelled {
                return Ok(format!(
                    "Cancelled — the recovered image is kept: {iso_path}"
                ));
            }
            // The mux above reports its own success text (titles written); it has no way
            // to know THIS stage's recovery left residual damage under tolerance, so the
            // note is appended out here instead.
            mux.map(|s| {
                format!(
                    "{s}{}{}",
                    damage_note(&result),
                    staging_not_kept_note(req.keep_iso, staging.is_some())
                )
            })
        })();
        release_iso_lock(lock, &res, halted, std::path::Path::new(&iso_path), state);
        return res;
    }

    let multi = demux_needs_subdirs(indices.len());
    // What each selected NUMBER refers to on THIS scan, banked before the
    // drive is released. Each title below re-scans, so the index alone doesn't
    // prove the mux is about to read the title picked. See `verify_title_identity`.
    let picked_ids = scanned_ids;
    state.plan_titles(title_sizes(&disc, &indices));
    drop(session);

    // The engine owns the per-title loop (skip/abort policy); we only supply
    // "mux one title" — exactly the ISO path's shape, but each title reopens
    // its own DiscSession off the live drive.
    let written = std::cell::Cell::new(0usize);
    let partial = std::cell::Cell::new(0usize);
    let outcome = fe::run_titles(&indices, !req.titles.is_empty(), sink, |idx| {
        let _bars = TitleBars::start(state, idx);
        let (dest_url, target) = title_dest(req, kind, &label, idx, multi);
        let before = output_stamp(&target);

        // KU §3.3: each title reopens the drive with `open_scan` (no key call) and reads
        // through the rip's one set, after `is_for`. A fresh session per title matches the
        // CLI (the staged reader is consumed by one mux).
        let mut session = match fe::open_scan(
            disc_target(&req.source),
            session_credentials(&req.keys),
            false,
        ) {
            Ok(v) => v,
            // Same bypass as the identity check below: an `Err` returned
            // straight out never reaches the arm that logs a per-title reason,
            // so "no disc in the drive" reaches the user as "Write failed (Other)."
            Err(e) => {
                state
                    .lines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("Title {}: {}", idx + 1, explain(e.code())));
                return Err(e.into());
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
        // log line — and the user is told "Write failed (Other)" instead.
        if let Err(msg) = verify_title_identity(picked_ids.get(idx), &rescanned, idx) {
            state
                .lines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(msg.clone());
            return Err(std::io::Error::other(msg));
        }
        // The rescan must be the disc the set is for, and the set must open this title
        // (KU §3.5, the one gate; CSS from the disc).
        if let Some(d) = session.disc() {
            let title = libfreemkv::keys::KeyScope::Titles(vec![idx]);
            let checked = crate::rip_keys::check_reopened(&set, d)
                .and_then(|()| crate::rip_keys::gate(d, false, Some(&set), &title));
            if let Err(e) = checked {
                let msg = format!("Title {}: {}", idx + 1, explain(e.code()));
                state
                    .lines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(msg);
                return Err(e.into());
            }
        }
        match mux_session_title(req, &mut session, idx, &set, &dest_url, sink) {
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
                // looks like output; remove it. (Demux writes into a
                // directory — nothing to clean.)
                if matches!(kind, OutKind::File(_)) {
                    remove_failed_output(&target, before);
                }
                Err(e)
            }
        }
    });

    // Single-pass reads each title straight off the drive, so the drive is only
    // free once the whole loop is done — eject here (autorip's auto_eject).
    if req.auto_eject {
        eject_device(&device, sink);
    }

    summarize_outcome(
        &outcome,
        written.get(),
        partial.get(),
        indices.len(),
        &req.dest_dir,
    )
}

/// Mux `indices` out of a staged (raw) image with the rip's set, from the drive's scan:
/// KU §4.3 "GUI multipass: staged raw ISO → mux | Yes | The set from Start is reused for the mux
/// (`open_image_with(Known)` or `Seeded`)". `Seeded` only for Pending forensic keys (§5.4).
#[allow(clippy::too_many_arguments)]
fn mux_staged_titles(
    req: &RipRequest,
    iso_path: &str,
    drive: libfreemkv::Disc,
    set: KeySet,
    indices: &[usize],
    label: &str,
    sink: &UiSink,
    state: &Arc<RunState>,
) -> Result<String, String> {
    use std::sync::atomic::AtomicUsize;
    // J14: "an image mux never rescans when the caller has a scanned disc", so the drive's
    // title numbers (the user's picks) index the set and the image alike.
    let keys = if set.forensic_pending() {
        fe::KeyInput::Seeded(key_factory(&req.keys), set)
    } else {
        fe::KeyInput::Known(set)
    };
    let watch = CancelWatch::new(state);
    let opts = fe::OpenImageOptions {
        keys,
        disc: Some(drive),
        scope: Some(libfreemkv::keys::KeyScope::Titles(indices.to_vec())),
        vid: None,
        halt: Some(watch.halt.clone()),
    };
    let src = fe::ImageSource::Iso(iso_path.into());
    let (opened, trace) = fe::open_image_with_traced(&src, opts);
    drop(watch);
    log_walk(&trace, sink);
    let opened = opened.map_err(|e| key_refusal(&e, src.path(), state))?;
    std::fs::create_dir_all(&req.dest_dir).map_err(|e| format!("{e}"))?;
    let kind = out_kind(&req.format);
    let multi = demux_needs_subdirs(indices.len());
    let plan = fe::MuxPlan {
        titles: indices.to_vec(),
        explicit_selection: !req.titles.is_empty(),
        streams: indices
            .iter()
            .map(|&i| (i, stream_selection_for(req, Some(i))))
            .collect(),
        mux: mux_opts(req),
    };
    let dest = |idx: usize| title_dest(req, kind, label, idx, multi).0;
    // The per-title report `mux_selected_titles` makes, from the engine's title events.
    struct Staged<'a> {
        ui: &'a UiSink,
        req: &'a RipRequest,
        kind: OutKind,
        label: &'a str,
        multi: bool,
        written: AtomicUsize,
        partial: AtomicUsize,
    }
    impl fe::Sink for Staged<'_> {
        fn log(&self, level: fe::Level, msg: &str) {
            self.ui.log(level, msg);
        }
        fn progress(&self, p: &fe::Progress) {
            self.ui.progress(p);
        }
        fn should_cancel(&self) -> bool {
            self.ui.should_cancel()
        }
        fn event(&self, e: &fe::Event<'_>) {
            self.ui.event(e);
            let fe::Event::TitleDone { idx, result, .. } = e else {
                return;
            };
            let target = title_dest(self.req, self.kind, self.label, *idx, self.multi).1;
            let mut lines = self.ui.0.lines.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(o) if o.halted && !o.output_opened => {
                    let e: std::io::Error = libfreemkv::Error::Halted.into();
                    lines.push(format!("Title {}: {}", idx + 1, explain(error_code(&e))));
                }
                Ok(o) if !o.completed => {
                    self.partial.fetch_add(1, Ordering::Relaxed);
                    lines.push(format!(
                        "title {} cancelled — partial output kept: {target}",
                        idx + 1
                    ));
                }
                Ok(o) => {
                    lines.push(format!("title {} -> {target}", idx + 1));
                    lines.extend(lossy_lines(o, &target));
                    self.written.fetch_add(1, Ordering::Relaxed);
                    self.ui.0.titles_done.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => lines.push(format!("Title {}: {}", idx + 1, explain(error_code(e)))),
            }
        }
    }
    let staged = Staged {
        ui: sink,
        req,
        kind,
        label,
        multi,
        written: AtomicUsize::new(0),
        partial: AtomicUsize::new(0),
    };
    let outcome = fe::mux_image_titles(&opened, &plan, &dest, &staged);
    summarize_outcome(
        &outcome,
        staged.written.load(Ordering::Relaxed),
        staged.partial.load(Ordering::Relaxed),
        indices.len(),
        &req.dest_dir,
    )
}

/// Title `idx`'s sink URL and the path the log names, for a file or demux output.
fn title_dest(
    req: &RipRequest,
    kind: OutKind,
    label: &str,
    idx: usize,
    multi: bool,
) -> (String, String) {
    match kind {
        OutKind::File(scheme) => {
            let base = title_basename(&req.filename_template, label, idx + 1);
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
        // Whole-disc kinds are handled by their own callers.
        OutKind::DecryptedFolder | OutKind::IsoImage => unreachable!(),
    }
}

/// Mux title `idx` live off a reopened drive through the rip's set: an engine title run over
/// the held session, its progress, output opening and Stop through the Sink.
fn mux_session_title(
    req: &RipRequest,
    session: &mut libfreemkv::DiscSession,
    idx: usize,
    set: &libfreemkv::keys::KeyRing,
    dest: &str,
    sink: &UiSink,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    let plan = title_plan(req, &source_url(&req.source), dest, idx, req.raw);
    let with = fe::RunWith {
        keys: Some(set.clone()),
        held: Some(fe::Held::Session(session)),
        // The live arm keeps this title's own ticks: one union built before the loop wrote
        // tracks unticked under this title whenever a sibling title shared the PID.
        title: title_options(req, Some(idx)),
        ..fe::RunWith::default()
    };
    stopped_before_output(run_title(&plan, with, sink))
}

// The app's plan for title `idx` of `source` (a URL) into `dest`.
fn title_plan(req: &RipRequest, source: &str, dest: &str, idx: usize, raw: bool) -> fe::Plan {
    fe::Plan {
        source: source.to_string(),
        titles: fe::Selection::Titles(vec![idx]),
        raw,
        multipass: false,
        ..gui_plan(req, dest)
    }
}

// A title run's options for title `idx`: its ticked streams, read errors fatal, 64-sector reads.
fn title_options(req: &RipRequest, idx: Option<usize>) -> fe::TitleOptions {
    fe::TitleOptions {
        selection: Some(stream_selection_for(req, idx)),
        skip_errors: false,
        batch_sectors: 64,
    }
}

// Run a title plan; the mux's outcome, or the error it failed with as the library reports it.
fn run_title(
    plan: &fe::Plan,
    with: fe::RunWith<'_>,
    sink: &UiSink,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    match fe::run_with(plan, with, sink) {
        Ok(fe::Report::Title { outcome }) => Ok(outcome),
        Ok(_) => Err(libfreemkv::Error::StreamUrlInvalid {
            url: plan.dest.clone(),
        }
        .into()),
        Err(e) => Err(e.into()),
    }
}

// What is at `path` before a title's mux: length and mtime, `None` when absent.
fn output_stamp(path: &str) -> Option<(u64, Option<std::time::SystemTime>)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()))
}

// Remove the file a failed mux left at `path`, unless it is the one `before` saw untouched:
// an error before the output opened must not delete an earlier rip.
fn remove_failed_output(path: &str, before: Option<(u64, Option<std::time::SystemTime>)>) {
    if output_stamp(path) != before {
        let _ = std::fs::remove_file(path);
    }
}

// A Stop that ended a mux before its output opened is a stop, not a kept partial file.
fn stopped_before_output(
    r: std::io::Result<libfreemkv::MuxOutcome>,
) -> std::io::Result<libfreemkv::MuxOutcome> {
    match r {
        Ok(o) if o.halted && !o.output_opened => Err(libfreemkv::Error::Halted.into()),
        r => r,
    }
}

#[cfg(test)]
#[path = "engine_run_state_poison_tests.rs"]
mod run_state_poison_tests;

// The engine→front-end outcome contract: run_titles emits NoKey/Failed in
// freemkv-engine, and this crate reports each as an Err, never as an Ok success.
#[cfg(test)]
#[path = "engine_outcome_summary_tests.rs"]
mod outcome_summary_tests;

#[cfg(test)]
#[path = "engine_verdict_and_explain_tests.rs"]
mod verdict_and_explain_tests;

#[cfg(test)]
#[path = "engine_key_summary_tests.rs"]
mod key_summary_tests;

// disc_details builds the multi-line disc summary the log pane/CLI show.
// Every branch is reachable only behind a scanned disc, so a mutant that
// dropped the VID line passed; tested against a synthetic disc instead.
#[cfg(test)]
#[path = "engine_disc_details_tests.rs"]
mod disc_details_tests;

#[cfg(test)]
#[path = "engine_selection_tests.rs"]
mod selection_tests;

// The routing and per-title wiring decisions a rip makes before it touches
// a drive: is_disc_source, stream_selection_for, title_index and friends —
// pure over &str/&RipRequest, but wrong in ways that are not cosmetic.
#[cfg(test)]
#[path = "engine_routing_tests.rs"]
mod routing_tests;

// ── Every disc-derived string that reaches a GUI row is display-sanitised. ──
// Round 2 found two missed fields (`playlist`, `language`); test is driven from an
// ENUMERATION of the untrusted fields, forcing a new one to be added here.
#[cfg(test)]
#[path = "engine_display_sanitisation_tests.rs"]
mod display_sanitisation_tests;

#[cfg(test)]
#[path = "engine_held_eject_tests.rs"]
mod held_eject_tests;

#[cfg(test)]
#[path = "engine_pure_helper_tests.rs"]
mod pure_helper_tests;

#[cfg(test)]
#[path = "engine_ku_gui_tests.rs"]
mod ku_gui_tests;

#[cfg(test)]
#[path = "engine_held_drive_tests.rs"]
mod held_drive_tests;
