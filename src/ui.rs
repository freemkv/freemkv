//! Platform-neutral UI model.
//!
//! Everything a shell needs to *decide* lives here; a shell only *draws*.
//! No widget type, no `cfg`, no AppKit/Win32 — this file compiles and is
//! tested on any platform, which is what stops a bug fixed on one shell from
//! surviving on the other.
//!
//! The rule: if a change to this file would need mirroring in `mac.rs` or
//! `win.rs`, the split is wrong.

use crate::engine::Scanned;
use std::cell::RefCell;

// ── the title tree ────────────────────────────────────────────────────────

/// A row in the title tree. Owned here so both shells render identical text.
pub struct Node {
    pub type_s: String,
    pub desc: String,
    /// Whether this row carries a checkbox — decided by the scan, not by
    /// re-matching the display string here.
    checkable: bool,
    pub checked: RefCell<bool>,
    pub children: Vec<usize>,
    pub info: String,
    /// Transport PID for audio/subtitle rows; `None` elsewhere.
    pub pid: Option<u16>,
    /// Canonical disc title index — NOT the tree position.
    pub title_idx: usize,
}

impl Node {
    /// Whether this row carries a checkbox.
    ///
    /// Taken from the scan, NOT re-derived from the display string: matching
    /// on `type_s` meant the engine and the tree each decided separately what
    /// is selectable, and a renamed row type would silently grow or lose a
    /// checkbox.
    pub fn checkable(&self) -> bool {
        self.checkable
    }
}

/// Tri-state for a title row: some streams on, none, or all.
#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Check {
    Off,
    On,
    Mixed,
}

/// The tree plus the selection state, with no widgets attached.
#[derive(Default)]
pub struct Tree {
    pub arena: Vec<Node>,
    pub roots: Vec<usize>,
}

impl Tree {
    /// Build from an engine scan. An empty scan yields an empty tree — the
    /// shell shows its empty page rather than inventing rows.
    ///
    /// `sel_mode` is the "Default selection" setting ("Main film only" / "All
    /// titles" / "Longest title") — it decides which titles start checked.
    /// `min_secs` is the "Minimum title length" setting: titles shorter than it
    /// (with a known, non-zero duration) are hidden from the list, since they
    /// are almost always menus and stings — but never so aggressively that the
    /// list would be empty. Canonical `title_idx` values are preserved on the
    /// rows that survive; the engine selects by those, not by tree position.
    pub fn from_scan(sc: &Scanned, sel_mode: &str, min_secs: f64) -> Self {
        // Titles present in the scan, with durations, for the filter + defaults.
        let titles: Vec<(usize, f64)> = sc
            .rows
            .iter()
            .filter(|r| r.depth == 1 && r.type_s == "Title")
            .map(|r| (r.title, r.duration_secs))
            .collect();
        // Never hide every title: if none clear the bar, disable the filter.
        let min_eff = if titles.iter().any(|(_, d)| *d >= min_secs) {
            min_secs
        } else {
            0.0
        };
        // Which title indices start checked.
        let selected: std::collections::HashSet<usize> = match sel_mode {
            "All titles" => titles
                .iter()
                .filter(|(_, d)| *d >= min_eff)
                .map(|(i, _)| *i)
                .collect(),
            "Longest title" => titles
                .iter()
                .filter(|(_, d)| *d >= min_eff)
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| *i)
                .into_iter()
                .collect(),
            // "Main film only" (default): the first disc title.
            _ => std::iter::once(0usize).collect(),
        };

        let mut arena: Vec<Node> = Vec::new();
        let mut roots = Vec::new();
        let mut last_title: Option<usize> = None;
        let mut skip_title = false;
        for r in &sc.rows {
            match r.depth {
                0 => skip_title = false,
                1 => {
                    // Hide a too-short title (and everything under it).
                    skip_title =
                        r.type_s == "Title" && r.duration_secs > 0.0 && r.duration_secs < min_eff;
                    if skip_title {
                        continue;
                    }
                }
                _ => {
                    if skip_title {
                        continue;
                    }
                }
            }
            let idx = arena.len();
            arena.push(Node {
                type_s: r.type_s.clone(),
                desc: r.desc.clone(),
                checkable: r.checkable,
                checked: RefCell::new(r.depth == 1 && selected.contains(&r.title)),
                children: vec![],
                info: r.info.clone(),
                pid: r.pid,
                title_idx: r.title,
            });
            match r.depth {
                0 => roots.push(idx),
                1 => {
                    if let Some(&root) = roots.first() {
                        arena[root].children.push(idx);
                    }
                    last_title = Some(idx);
                }
                _ => {
                    if let Some(t) = last_title {
                        arena[t].children.push(idx);
                        let on = *arena[t].checked.borrow();
                        *arena[idx].checked.borrow_mut() = on;
                    }
                }
            }
        }
        Tree { arena, roots }
    }

    /// Tick state for a row, folding children into a tri-state for titles.
    pub fn check_state(&self, i: usize) -> Check {
        let n = &self.arena[i];
        if n.children.is_empty() {
            return if *n.checked.borrow() {
                Check::On
            } else {
                Check::Off
            };
        }
        let sel: Vec<bool> = n
            .children
            .iter()
            .filter(|&&c| self.arena[c].checkable())
            .map(|&c| *self.arena[c].checked.borrow())
            .collect();
        let on = sel.iter().filter(|x| **x).count();
        if on == 0 {
            Check::Off
        } else if on == sel.len() {
            Check::On
        } else {
            Check::Mixed
        }
    }

    /// Tick a row and cascade to its streams.
    pub fn set_checked(&self, i: usize, on: bool) {
        *self.arena[i].checked.borrow_mut() = on;
        for &c in &self.arena[i].children {
            *self.arena[c].checked.borrow_mut() = on;
        }
    }

    pub fn set_all(&self, on: bool) {
        for n in &self.arena {
            if n.checkable() {
                *n.checked.borrow_mut() = on;
            }
        }
    }

    pub fn invert(&self) {
        for n in &self.arena {
            if n.checkable() {
                let cur = *n.checked.borrow();
                *n.checked.borrow_mut() = !cur;
            }
        }
    }

    /// Canonical indices of ticked titles — what the engine's `Selection`
    /// wants. Tree position is not the index once a disc is listed in full.
    pub fn ticked_titles(&self) -> Vec<usize> {
        self.arena
            .iter()
            .filter(|n| n.type_s == "Title" && *n.checked.borrow() && n.title_idx != usize::MAX)
            .map(|n| n.title_idx)
            .collect()
    }

    /// Number of title rows in the tree. Used by `start_run` to tell a
    /// disc/ISO scan (has titles) from a container (none), and by the
    /// cross-platform tests to assert the tree matches the scan.
    pub fn title_count(&self) -> usize {
        self.arena.iter().filter(|n| n.type_s == "Title").count()
    }

    /// Ticked audio/subtitle PIDs, and whether the user deviated from
    /// "everything" — an empty explicit list legitimately means "none".
    pub fn ticked_streams(&self) -> (Vec<u16>, Vec<u16>, bool) {
        let (mut a, mut s) = (Vec::new(), Vec::new());
        let (mut total, mut on) = (0usize, 0usize);
        for n in &self.arena {
            let Some(pid) = n.pid else { continue };
            total += 1;
            if *n.checked.borrow() {
                on += 1;
                if n.type_s == "Audio" {
                    a.push(pid);
                } else {
                    s.push(pid);
                }
            }
        }
        (a, s, total > 0 && on != total)
    }
}

// ── formatting ────────────────────────────────────────────────────────────

/// Human byte size, so a growing output rolls over instead of reading
/// "6103.5 MB" all the way to a 6 GB file.
pub fn fmt_bytes(b: u64) -> String {
    const K: f64 = 1024.0;
    let f = b as f64;
    if f >= K * K * K {
        format!("{:.2} GB", f / (K * K * K))
    } else if f >= K * K {
        format!("{:.1} MB", f / (K * K))
    } else if f >= K {
        format!("{:.0} KB", f / K)
    } else {
        format!("{b} B")
    }
}

/// `h:mm:ss`.
pub fn fmt_hms(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // Drop the hours field entirely under an hour: "1:36", not "0:01:36".
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Free space on the volume holding `path`.
pub fn free_space(path: &str) -> String {
    crate::platform::free_space_bytes(path)
        .map(fmt_bytes)
        .unwrap_or_else(|| "—".into())
}

// ── which page is on screen ───────────────────────────────────────────────

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Page {
    Empty,
    Titles,
    Progress,
    Result,
}

/// The output sinks offered for a given source kind. Whole-disc sinks make no
/// sense for a container, so they are omitted rather than offered and failed.
/// `mp4_ok` is false when the source's video cannot go in an MP4 at all (a
/// DVD's MPEG-2, an HD DVD's VC-1). The option is then REMOVED rather than
/// offered-and-refused: a choice that always fails is worse than no choice.
/// Pass true when the codecs are unknown — never block on missing information.
pub fn output_formats(disc_source: bool, mp4_ok: bool) -> Vec<Vec<&'static str>> {
    let mut titles = vec!["Selected titles → MKV"];
    if mp4_ok {
        titles.push("Selected titles → MP4");
    }
    titles.push("Selected titles → M2TS");
    titles.push("Selected titles → separate track files");
    let whole = vec!["Whole disc → ISO image", "Whole disc → decrypted folder"];
    let meta = vec!["Chapters → file", "Title info → JSON", "Video index → .fvi"];
    if disc_source {
        vec![titles, whole, meta]
    } else {
        vec![titles, meta]
    }
}

/// Video codecs MP4 can actually carry. Anything else — MPEG-2 from a DVD,
/// VC-1 from an HD DVD, AV1 — has no MP4 mapping, so the mux fails with E9048
/// after the user has already waited; say it up front instead.
///
/// This list MUST match the mux gate in `libfreemkv::mux::mp4`, which admits
/// exactly `Codec::Hevc | Codec::H264`. It previously also listed AV1, so the
/// desktop app offered MP4 for an AV1 title, suppressed the pre-rip warning,
/// and then failed at mux time with a message naming AV1 as supported.
const MP4_VIDEO: &[&str] = &["H.264", "HEVC"];

/// Resolve a popup's visible text back to the canonical format string.
///
/// Shells hold display text; the core holds the authoritative list. Matching
/// here means a shell never invents a format, and both shells resolve the same
/// way instead of each parsing the string.
pub fn format_by_title(title: &str, disc_source: bool, mp4_ok: bool) -> Option<&'static str> {
    output_formats(disc_source, mp4_ok)
        .into_iter()
        .flatten()
        .find(|f| *f == title)
}

/// Sources the file picker accepts, per docs/cli #stream-urls.
pub const SOURCE_EXTS: &[&str] = &["iso", "ISO", "mkv", "m2ts", "mts", "mp4"];

/// True for a container source (single title, no disc scan).
pub fn is_container(path: &str) -> bool {
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

/// Commands that must be unavailable while a rip is in flight. Cancel is
/// deliberately absent — it must always be reachable.
pub fn blocked_while_running(cmd: Cmd) -> bool {
    !matches!(cmd, Cmd::Cancel | Cmd::About | Cmd::Docs | Cmd::Quit)
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Cmd {
    /// The user picked an output format. Carries a `&'static str` borrowed
    /// from [`output_formats`], so an unrecognized title cannot enter the
    /// model — and `Cmd` stays `Copy`.
    SetFormat(&'static str),
    Open,
    Close,
    SetOutput,
    Run,
    Cancel,
    Eject,
    SelectAll,
    SelectNone,
    Invert,
    ClearLog,
    ToggleLog,
    Settings,
    About,
    Docs,
    CheckUpdates,
    Quit,
}

// ── the Information block on the progress page ────────────────────────────

/// Fully-formatted rows, so a shell only assigns strings to labels.
pub struct InfoRows {
    pub source: String,
    pub source_file: String,
    pub source_size: String,
    pub read_rate: String,
    pub output_file: String,
    pub output_size: String,
    pub free_space: String,
}

impl InfoRows {
    /// `dest` is the output FILE, not the folder — the label says "Output
    /// file" and showing a directory there is simply wrong.
    pub fn starting(source: &str, dest: &str) -> Self {
        InfoRows {
            source: source.to_string(),
            source_file: std::path::Path::new(source)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
            // Never leave the row blank — a blank Information field reads as
            // a broken panel (reported). An unknown value is an em dash.
            source_size: std::fs::metadata(source)
                .map(|m| fmt_bytes(m.len()))
                .unwrap_or_else(|_| "—".into()),
            read_rate: "—".into(),
            output_file: dest.to_string(),
            output_size: "0 B".into(),
            free_space: free_space(dest),
        }
    }

    /// Row labels for the Information panel, localized. A function (not a
    /// const) so it reflects the active locale.
    pub fn labels() -> [String; 7] {
        [
            crate::strings::get("gui.info.source"),
            crate::strings::get("gui.info.source_file"),
            crate::strings::get("gui.info.source_size"),
            crate::strings::get("gui.info.read_rate"),
            crate::strings::get("gui.info.output_file"),
            crate::strings::get("gui.info.output_size"),
            crate::strings::get("gui.info.free_space"),
        ]
    }

    pub fn as_array(&self) -> [&str; 7] {
        [
            &self.source,
            &self.source_file,
            &self.source_size,
            &self.read_rate,
            &self.output_file,
            &self.output_size,
            &self.free_space,
        ]
    }
}

/// Read rate for display. `speed_bps` is engine-derived; never recompute it.
pub fn rate_text(speed_bps: u64, running: bool) -> String {
    if speed_bps > 0 {
        format!("{}/s", fmt_bytes(speed_bps))
    } else if running {
        crate::strings::get("gui.info.not_reported")
    } else {
        "—".to_string()
    }
}

/// Bar caption: percent, elapsed, and the engine's ETA when it has one.
pub fn bar_caption(pct: f64, elapsed_secs: u64, eta_secs: Option<u64>) -> String {
    let el = crate::strings::fmt("gui.progress.elapsed", &[("hms", &fmt_hms(elapsed_secs))]);
    let pct = format!("{pct:.0}");
    match eta_secs {
        Some(e) => crate::strings::fmt(
            "gui.progress.caption_eta",
            &[("pct", &pct), ("elapsed", &el), ("hms", &fmt_hms(e))],
        ),
        None => crate::strings::fmt(
            "gui.progress.caption_no_eta",
            &[("pct", &pct), ("elapsed", &el)],
        ),
    }
}

/// The container word for a chosen output format ("MKV" / "MP4" / "M2TS").
/// Single source of the format→container mapping the shells display, so the
/// progress caption ("Saving to MP4 file") always matches the real extension.
pub fn container_label(format: &str) -> &'static str {
    if format.contains("MP4") {
        "MP4"
    } else if format.contains("M2TS") {
        "M2TS"
    } else {
        "MKV"
    }
}

/// Localized display text for a canonical output-format string. The canonical
/// string (returned by `output_formats`, stored in `App.format`, matched by
/// `.contains(...)` in the engine) stays English so ripping keeps working; only
/// what the picker SHOWS is translated. An unknown format returns as-is.
pub fn format_label(canonical: &str) -> String {
    let key = match canonical {
        "Selected titles → MKV" => "gui.format.mkv",
        "Selected titles → MP4" => "gui.format.mp4",
        "Selected titles → M2TS" => "gui.format.m2ts",
        "Selected titles → separate track files" => "gui.format.tracks",
        "Whole disc → ISO image" => "gui.format.iso",
        "Whole disc → decrypted folder" => "gui.format.folder",
        "Chapters → file" => "gui.format.chapters",
        "Title info → JSON" => "gui.format.json",
        "Video index → .fvi" => "gui.format.fvi",
        _ => return canonical.to_string(),
    };
    crate::strings::get(key)
}

/// Inverse of [`format_label`]: resolve a LOCALIZED popup label back to the
/// canonical format string. The shell shows `format_label(canonical)`, so a
/// non-English selection reads back as translated text — `format_by_title`
/// only matches the English canonical list, so it would fail in every other
/// locale. Match on the localized display instead.
pub fn format_from_label(label: &str, disc_source: bool, mp4_ok: bool) -> Option<&'static str> {
    // Canonical (English) fast path first — also covers callers that pass a
    // canonical string directly — then fall back to the localized display.
    format_by_title(label, disc_source, mp4_ok).or_else(|| {
        output_formats(disc_source, mp4_ok)
            .into_iter()
            .flatten()
            .find(|canon| format_label(canon) == label)
    })
}

/// The interface languages the GUI offers, matched 1:1 to the locale files
/// shipped by `freemkv-i18n`. Each entry is `(endonym, code)`; the endonym is
/// shown in the picker (language names are conventionally written in their own
/// language, so they are not translated), the code is the locale-file stem that
/// `freemkv_i18n::set_language` expects. `"auto"` follows the system locale.
/// Regional variants (`pt-br`, `es-419`, `zh-hans`, `zh-hant`) resolve via the
/// crate's full-tag → base-language → English fallback. Adding a locale file
/// means adding one row here.
pub const LOCALES: &[(&str, &str)] = &[
    ("Auto", "auto"),
    ("English", "en"),
    ("Deutsch", "de"),
    ("Español", "es"),
    ("Español (Latinoamérica)", "es-419"),
    ("Français", "fr"),
    ("Italiano", "it"),
    ("Nederlands", "nl"),
    ("Português", "pt"),
    ("Português (Brasil)", "pt-br"),
    ("Polski", "pl"),
    ("Русский", "ru"),
    ("Українська", "uk"),
    ("Čeština", "cs"),
    ("Slovenčina", "sk"),
    ("Svenska", "sv"),
    ("Dansk", "da"),
    ("Norsk", "no"),
    ("Suomi", "fi"),
    ("Română", "ro"),
    ("Magyar", "hu"),
    ("Ελληνικά", "el"),
    ("Türkçe", "tr"),
    ("Català", "ca"),
    ("日本語", "ja"),
    ("한국어", "ko"),
    ("简体中文", "zh-hans"),
    ("繁體中文", "zh-hant"),
    ("Bahasa Indonesia", "id"),
    ("Tiếng Việt", "vi"),
];

/// Map a stored setting (endonym OR code, any case) to a locale code.
/// Anything unrecognized — including "Auto"/"" — resolves to `"auto"`. The
/// picker itself is driven from `LOCALES` directly (see `enum_options`); this
/// is the normalizer used at GUI startup and on settings load.
pub fn locale_code(sel: &str) -> &'static str {
    let s = sel.trim();
    for (name, code) in LOCALES {
        if s.eq_ignore_ascii_case(name) || s.eq_ignore_ascii_case(code) {
            return code;
        }
    }
    "auto"
}

/// Overall progress across a multi-title run.
pub fn overall_pct(titles_done: usize, total: usize, current_pct: f64) -> f64 {
    let total = total.max(1) as f64;
    ((titles_done as f64 + current_pct / 100.0) / total * 100.0).min(100.0)
}

// ══ the application core ══════════════════════════════════════════════════
//
// Model / Update / View. `App` owns every piece of state and every decision;
// a shell does exactly three things:
//
//   1. render `App::view()`            — assign strings and flags to widgets
//   2. call `App::dispatch(cmd)`       — on any click, menu pick or key
//   3. perform the returned `Effect`s  — the platform-only actions
//
// Every button on every platform therefore runs the SAME code. Adding a shell
// (Win32, GTK, a TUI) means implementing render + event → Cmd; it means
// writing no behaviour, and fixing a bug here fixes it everywhere at once.

use crate::engine::{KeyConfig, RipRequest, RunState};
use crate::settings::Settings;
use std::sync::Arc;

/// A platform action the core cannot perform itself. The shell executes it and
/// usually feeds the answer back in as a `Cmd`.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Show a file picker limited to `SOURCE_EXTS`; on choose → `Cmd::Open`.
    PickSource,
    /// Show a folder picker; on choose → set the output directory.
    PickOutputDir,
    /// Reveal a path in the platform file manager.
    Reveal(String),
    /// Open a URL in the default browser.
    OpenUrl(String),
    /// Present the settings window.
    ShowSettings,
    /// Present the about window.
    ShowAbout,
    /// Redraw: state changed.
    Redraw,
    /// Start the periodic tick that polls a running job.
    StartTicking,
    /// Stop it.
    StopTicking,
    Quit,
}

/// One line in the log, with its severity so a shell can colour it.
#[derive(Clone, Debug)]
pub struct LogLine {
    pub text: String,
    pub kind: LogKind,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum LogKind {
    Notice,
    Detail,
    Result,
}

/// Everything the app knows. No widgets, no platform types.
pub struct App {
    pub tree: Tree,
    pub settings: Settings,
    pub page: Page,
    pub log: Vec<LogLine>,
    pub source: String,
    pub output_dir: String,
    pub format: String,
    pub log_hidden: bool,
    pub run: Option<Arc<RunState>>,
    pub run_titles: usize,
    pub run_started: Option<std::time::Instant>,
    pub info: Option<InfoRows>,
    pub result_summary: String,
    pub selected_row: Option<usize>,
    /// Video codec per title, from the scan — used to warn when the chosen
    /// container cannot carry them.
    video_codecs: Vec<String>,
    /// Highest unreadable-sector count already announced, so the notice is
    /// not repeated on every 100 ms tick.
    reported_bad: u64,
}

impl App {
    pub fn new() -> Self {
        let settings = Settings::load();
        let output_dir = settings.dest_dir.clone();
        let format = if settings.container.is_empty() {
            "Selected titles → MKV".to_string()
        } else {
            settings.container.clone()
        };
        let mut app = App {
            tree: Tree::default(),
            settings,
            page: Page::Empty,
            log: Vec::new(),
            source: String::new(),
            output_dir,
            format,
            log_hidden: false,
            run: None,
            run_titles: 0,
            run_started: None,
            info: None,
            result_summary: String::new(),
            selected_row: None,
            video_codecs: Vec::new(),
            reported_bad: 0,
        };
        app.say(
            LogKind::Result,
            &crate::strings::fmt("gui.log.ready", &[("version", env!("CARGO_PKG_VERSION"))]),
        );
        app
    }

    pub fn say(&mut self, kind: LogKind, text: &str) {
        self.log.push(LogLine {
            text: text.into(),
            kind,
        });
    }

    /// True when at least one title on this source could go in an MP4. With no
    /// codec information (an unscanned or container source) this is true — the
    /// UI must not hide an option on a guess.
    pub fn mp4_possible(&self) -> bool {
        let known: Vec<&String> = self.video_codecs.iter().filter(|c| !c.is_empty()).collect();
        known.is_empty() || known.iter().any(|c| MP4_VIDEO.contains(&c.as_str()))
    }

    /// Why the current format cannot hold the ticked titles, if it cannot.
    ///
    /// Answered from the scan, before any rip: a container that will certainly
    /// fail should say so while the user can still change it.
    pub fn container_mismatch(&self) -> Option<String> {
        if !self.format.contains("MP4") {
            return None;
        }
        let ticked = self.tree.ticked_titles();
        let mut bad: Vec<&str> = ticked
            .iter()
            .filter_map(|i| self.video_codecs.get(*i))
            .map(|c| c.as_str())
            .filter(|c| !c.is_empty() && !MP4_VIDEO.contains(c))
            .collect();
        bad.sort_unstable();
        bad.dedup();
        if bad.is_empty() {
            return None;
        }
        Some(crate::strings::fmt(
            "gui.log.mp4_mismatch",
            &[("codecs", &bad.join(" or "))],
        ))
    }

    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// The single entry point for every user action, on every platform.
    pub fn dispatch(&mut self, cmd: Cmd) -> Vec<Effect> {
        if self.running() && blocked_while_running(cmd) {
            return vec![];
        }
        match cmd {
            Cmd::Open => vec![Effect::PickSource],
            Cmd::SetOutput => vec![Effect::PickOutputDir],
            Cmd::Close => {
                self.tree = Tree::default();
                self.source.clear();
                self.page = Page::Empty;
                self.say(
                    LogKind::Result,
                    &crate::strings::get("gui.log.source_closed"),
                );
                vec![Effect::Redraw]
            }
            Cmd::Run => self.start_run(),
            Cmd::Cancel => {
                if let Some(st) = &self.run {
                    st.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.say(LogKind::Result, &crate::strings::get("gui.log.cancelling"));
                }
                vec![Effect::Redraw]
            }
            Cmd::Eject => {
                self.say(
                    LogKind::Result,
                    &crate::strings::get("gui.log.nothing_eject"),
                );
                vec![Effect::Redraw]
            }
            Cmd::SelectAll => {
                self.tree.set_all(true);
                vec![Effect::Redraw]
            }
            Cmd::SelectNone => {
                self.tree.set_all(false);
                vec![Effect::Redraw]
            }
            Cmd::Invert => {
                self.tree.invert();
                vec![Effect::Redraw]
            }
            Cmd::ClearLog => {
                self.log.clear();
                vec![Effect::Redraw]
            }
            Cmd::ToggleLog => {
                self.log_hidden = !self.log_hidden;
                vec![Effect::Redraw]
            }
            Cmd::Settings => vec![Effect::ShowSettings],
            Cmd::About => vec![Effect::ShowAbout],
            Cmd::Docs => vec![Effect::OpenUrl("https://freemkv.org/docs".into())],
            Cmd::CheckUpdates => {
                // Actually check. A menu item that only *says* it is checking
                // is worse than no menu item.
                self.say(
                    LogKind::Result,
                    &crate::strings::get("gui.log.checking_updates"),
                );
                let msg = crate::settings::check_for_update(env!("CARGO_PKG_VERSION"));
                self.say(LogKind::Result, &msg);
                vec![Effect::Redraw]
            }
            Cmd::SetFormat(f) => {
                self.format = f.to_string();
                if let Some(m) = self.container_mismatch() {
                    self.say(LogKind::Notice, &m);
                }
                vec![Effect::Redraw]
            }
            Cmd::Quit => vec![Effect::Quit],
        }
    }

    /// Open a source: scan it, rebuild the tree, report honestly on failure.
    pub fn open(&mut self, path: &str) -> Vec<Effect> {
        let container = is_container(path);
        let disc = crate::engine::is_disc_source(path);
        // "Log detail: Verbose" (or Debug) reveals the resolved keys in the
        // on-open detail block, mirroring the CLI's `info -v`.
        let verbose = self.settings.log_level == "Verbose" || self.settings.log_level == "Debug";
        let scanned = if container {
            crate::engine::scan_stream(path)
        } else if disc {
            crate::engine::scan_disc_with_keys(
                path,
                &KeyConfig::from_settings(&self.settings),
                verbose,
            )
        } else {
            crate::engine::scan_with_keys(path, &KeyConfig::from_settings(&self.settings), verbose)
        };
        match scanned {
            Ok(sc) => {
                self.log.clear();
                self.say(
                    LogKind::Result,
                    &crate::strings::fmt(
                        "gui.log.opened_version",
                        &[("version", env!("CARGO_PKG_VERSION"))],
                    ),
                );
                self.say(
                    LogKind::Detail,
                    &crate::strings::fmt(
                        "gui.log.opened",
                        &[
                            ("label", &sc.label),
                            ("n", &sc.title_count.to_string()),
                            ("keys", &sc.key_summary),
                        ],
                    ),
                );
                // The `info -v` detail block (format, capacity, region, MKB
                // version, disc hash, VID, key state, titles) — so the desktop
                // app surfaces the same disc facts the CLI prints.
                for line in &sc.details {
                    self.say(LogKind::Detail, line);
                }
                self.video_codecs = sc.video_codecs.clone();
                let min_secs = self
                    .settings
                    .min_title_secs
                    .trim()
                    .parse::<f64>()
                    .unwrap_or(0.0);
                self.tree = Tree::from_scan(&sc, &self.settings.selection, min_secs);
                self.source = path.to_string();
                self.page = Page::Titles;
                self.selected_row = None;
                if container {
                    self.say(
                        LogKind::Result,
                        &crate::strings::get("gui.log.ready_convert"),
                    );
                } else if disc {
                    // A live drive isn't a file the ISO preflight can re-scan;
                    // the rip itself surfaces any missing-key error.
                    self.say(LogKind::Result, &crate::strings::get("gui.log.ready_rip"));
                } else {
                    match crate::engine::preflight_with_keys(
                        path,
                        "/tmp",
                        &[],
                        &KeyConfig::from_settings(&self.settings),
                    ) {
                        Ok(v) if v.is_empty() => {
                            self.say(LogKind::Result, &crate::strings::get("gui.log.ready_rip"))
                        }
                        Ok(v) => self.say(
                            LogKind::Notice,
                            &crate::strings::fmt(
                                "gui.log.cannot_rip",
                                &[("reasons", &v.join(", "))],
                            ),
                        ),
                        Err(e) => self.say(LogKind::Notice, &e),
                    }
                }
            }
            Err(e) => {
                self.say(LogKind::Notice, &e);
                self.page = Page::Empty;
            }
        }
        vec![Effect::Redraw]
    }

    fn start_run(&mut self) -> Vec<Effect> {
        if self.source.is_empty() {
            self.say(
                LogKind::Notice,
                &crate::strings::get("gui.log.open_source_first"),
            );
            return vec![Effect::Redraw];
        }
        if self.output_dir.trim().is_empty() {
            self.say(
                LogKind::Notice,
                &crate::strings::get("gui.log.choose_folder_first"),
            );
            return vec![Effect::Redraw];
        }
        let titles = self.tree.ticked_titles();
        // A disc/ISO scan has title rows; if the user unchecked them all, refuse
        // rather than silently ripping the main title (the engine maps an empty
        // list to the main movie). A container source has no title rows, so this
        // guard doesn't fire — the whole stream is the "title".
        if self.tree.title_count() > 0 && titles.is_empty() {
            self.say(
                LogKind::Notice,
                &crate::strings::get("gui.log.select_title_first"),
            );
            return vec![Effect::Redraw];
        }
        let (audio_pids, sub_pids, explicit_streams) = self.tree.ticked_streams();
        // The user narrowed the tracks down to nothing (every audio AND subtitle
        // unchecked): allowed — some want a video-only extract — but never
        // silently. Surface it so an accidental result is caught before the rip.
        if explicit_streams && audio_pids.is_empty() && sub_pids.is_empty() {
            self.say(
                LogKind::Notice,
                &crate::strings::get("gui.log.video_only_warning"),
            );
        }
        // Re-check the MP4/codec mismatch NOW (not just when the format was
        // picked): the user may have ticked an MPEG-2/VC-1 title after choosing
        // MP4, which the picker-time check never saw. Better an up-front notice
        // than a late per-title mux failure.
        if let Some(msg) = self.container_mismatch() {
            self.say(LogKind::Notice, &msg);
        }
        // `--raw` (keep-encrypted) only means anything for a "Whole disc → ISO
        // image" output; for any mux it would write ciphertext into the
        // container. Mirror the CLI's iso-only rule instead of silently
        // forwarding it.
        let iso_output = self.format.contains("ISO image");
        let raw = self.settings.raw && iso_output;
        if self.settings.raw && !iso_output {
            self.say(
                LogKind::Notice,
                &crate::strings::get("gui.log.raw_iso_only"),
            );
        }
        let state = Arc::new(RunState::default());
        self.run = Some(state.clone());
        self.reported_bad = 0;
        self.run_titles = titles.len().max(1);
        self.run_started = Some(std::time::Instant::now());
        // Name the file the way the engine will, so the row matches reality.
        let label = std::path::Path::new(&self.source)
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("output");
        let ext = if self.format.contains("MP4") {
            "mp4"
        } else if self.format.contains("M2TS") {
            "m2ts"
        } else {
            "mkv"
        };
        let first = titles.first().copied().unwrap_or(0) + 1;
        let out_file = format!("{}/{}_t{}.{}", self.output_dir, label, first, ext);
        self.info = Some(InfoRows::starting(&self.source, &out_file));
        self.page = Page::Progress;
        self.say(
            LogKind::Result,
            &crate::strings::fmt("gui.log.starting_rip", &[("dir", &self.output_dir)]),
        );
        let max_passes: u32 = self.settings.max_passes.trim().parse().unwrap_or(0);
        crate::engine::start_rip(
            RipRequest {
                source: self.source.clone(),
                dest_dir: self.output_dir.clone(),
                titles,
                format: self.format.clone(),
                audio_pids,
                sub_pids,
                explicit_streams,
                raw,
                force: self.settings.force,
                filename_template: self.settings.filename_template.clone(),
                decrypt_threads: self
                    .settings
                    .decrypt_threads
                    .trim()
                    .parse::<usize>()
                    .unwrap_or(0),
                multipass: self.settings.rip_mode == "Multi-pass" && max_passes > 0,
                max_passes,
                abort_lost_secs: self.settings.abort_lost_secs.trim().parse().unwrap_or(0),
                keep_iso: self.settings.keep_iso,
                auto_eject: self.settings.auto_eject,
                keys: KeyConfig::from_settings(&self.settings),
            },
            state,
        );
        vec![Effect::Redraw, Effect::StartTicking]
    }

    /// Poll a running job. Called on the shell's timer; returns the effects to
    /// apply. All progress arithmetic is the engine's — never recomputed here.
    pub fn tick(&mut self) -> Vec<Effect> {
        let Some(st) = self.run.clone() else {
            return vec![Effect::StopTicking];
        };
        let lines: Vec<String> = st
            .lines
            .lock()
            .map(|mut v| v.drain(..).collect())
            .unwrap_or_default();
        for l in lines {
            self.say(LogKind::Detail, &l);
        }
        let p = st.prog.lock().map(|g| *g).unwrap_or_default();
        if let Some(info) = &mut self.info {
            info.read_rate = rate_text(p.speed_bps, true);
            info.output_size = fmt_bytes(p.bytes_done);
        }
        // Unreadable sectors are the whole reason this tool exists — say so
        // once, when the count first rises, rather than burying it.
        if p.sectors_bad > self.reported_bad {
            self.reported_bad = p.sectors_bad;
            self.say(
                LogKind::Notice,
                &crate::strings::fmt("gui.log.unreadable", &[("n", &p.sectors_bad.to_string())]),
            );
        }
        if st.finished.load(std::sync::atomic::Ordering::Relaxed) {
            let sum = st.summary.lock().map(|s| s.clone()).unwrap_or_default();
            self.say(LogKind::Result, &sum);
            self.result_summary = sum;
            self.run = None;
            self.page = Page::Result;
            return vec![Effect::Redraw, Effect::StopTicking];
        }
        vec![Effect::Redraw]
    }

    pub fn dismiss_result(&mut self) -> Vec<Effect> {
        self.page = if self.tree.arena.is_empty() {
            Page::Empty
        } else {
            Page::Titles
        };
        vec![Effect::Redraw]
    }

    /// Everything a shell needs to draw the current state.
    pub fn view(&self) -> View {
        let p = self
            .run
            .as_ref()
            .and_then(|st| st.prog.lock().ok().map(|g| *g))
            .unwrap_or_default();
        let pct = if p.bytes_total > 0 {
            p.bytes_done as f64 / p.bytes_total as f64 * 100.0
        } else {
            0.0
        };
        let elapsed = self.run_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let titles_done = self
            .run
            .as_ref()
            .map(|st| st.titles_done.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);
        View {
            page: self.page,
            title_rows: self.rows(),
            info: self
                .info
                .as_ref()
                .map(|i| i.as_array().map(|s| s.to_string())),
            bar_current: pct,
            bar_overall: overall_pct(titles_done, self.run_titles, pct),
            caption_current: bar_caption(pct, elapsed, p.eta_secs),
            caption_overall: bar_caption(
                overall_pct(titles_done, self.run_titles, pct),
                elapsed,
                None,
            ),
            show_overall_bar: self.run_titles > 1,
            saving_current: crate::strings::fmt(
                "gui.progress.saving_current",
                &[("container", container_label(&self.format))],
            ),
            saving_overall: crate::strings::fmt(
                "gui.progress.saving_overall",
                &[("container", container_label(&self.format))],
            ),
            output_dir: self.output_dir.clone(),
            format: self.format.clone(),
            formats: output_formats(!is_container(&self.source), self.mp4_possible()),
            can_run: !self.running() && !self.source.is_empty(),
            log: self.log.clone(),
            log_hidden: self.log_hidden,
            detail: self
                .selected_row
                .and_then(|i| self.tree.arena.get(i))
                .map(|n| n.info.clone())
                .unwrap_or_else(|| crate::strings::get("gui.page.detail_default")),
            result_summary: self.result_summary.clone(),
            // The summary text is engine-emitted English; classify on it, but
            // show a localized heading.
            result_heading: if self.result_summary.starts_with("Cancelled") {
                crate::strings::get("gui.result.cancelled")
            } else if self.result_summary.starts_with("Nothing")
                || self.result_summary.contains("failed")
            {
                crate::strings::get("gui.result.nothing")
            } else {
                crate::strings::get("gui.result.finished")
            },
            eject_visible: false,
        }
    }

    fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for (i, n) in self.tree.arena.iter().enumerate() {
            let depth = if self.tree.roots.contains(&i) {
                0
            } else if n.type_s == "Title" {
                1
            } else {
                2
            };
            out.push(Row {
                index: i,
                depth,
                type_s: n.type_s.clone(),
                desc: n.desc.clone(),
                check: if n.checkable() {
                    Some(self.tree.check_state(i))
                } else {
                    None
                },
            });
        }
        out
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// One rendered tree row — already decided, nothing left to compute.
#[derive(Clone, Debug)]
pub struct Row {
    pub index: usize,
    pub depth: u8,
    pub type_s: String,
    pub desc: String,
    /// `None` means the row carries no checkbox at all.
    pub check: Option<Check>,
}

/// A complete description of the screen. A shell assigns these to widgets and
/// makes no decisions of its own.
pub struct View {
    pub page: Page,
    pub title_rows: Vec<Row>,
    pub info: Option<[String; 7]>,
    pub bar_current: f64,
    pub bar_overall: f64,
    pub caption_current: String,
    pub caption_overall: String,
    /// "Saving to <container> file" — the per-title bar label, format-aware so
    /// it reads "MP4" when MP4 is chosen (never a hardcoded "MKV").
    pub saving_current: String,
    /// "Saving all titles to <container> files" — the overall-bar label.
    pub saving_overall: String,
    pub show_overall_bar: bool,
    pub output_dir: String,
    pub format: String,
    pub formats: Vec<Vec<&'static str>>,
    pub can_run: bool,
    pub log: Vec<LogLine>,
    pub log_hidden: bool,
    pub detail: String,
    pub result_summary: String,
    /// Heading for the result page — never "Finished" after a cancel.
    pub result_heading: String,
    pub eject_visible: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_roll_over_instead_of_staying_in_megabytes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(2048), "2 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        // The bug this pins: a 6 GB output once read "6103.5 M".
        assert_eq!(fmt_bytes(6 * 1024 * 1024 * 1024), "6.00 GB");
        assert_eq!(fmt_bytes(64_424_509_440), "60.00 GB");
    }

    #[test]
    fn an_empty_scan_yields_an_empty_tree() {
        // No source means no rows — the shell shows its empty page rather
        // than a placeholder disc.
        let t = Tree::from_scan(
            &crate::engine::Scanned {
                label: String::new(),
                title_count: 0,
                key_summary: String::new(),
                video_codecs: vec![],
                rows: vec![],
                details: vec![],
            },
            "Main film only",
            0.0,
        );
        assert!(t.roots.is_empty());
        assert!(t.arena.is_empty());
    }
}
