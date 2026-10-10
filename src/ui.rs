//! Platform-neutral UI model.
//!
//! Everything a shell needs to *decide* lives here; a shell only *draws*.
//! No widget type, no AppKit/Win32 — this file compiles and is tested on any
//! platform (its only `cfg`s are test seams), which is what stops a bug fixed
//! on one shell from surviving on the other.
//!
//! The rule: if a change to this file would need mirroring in `mac.rs` or
//! `windows.rs`, the split is wrong.

use crate::engine::{Scanned, TitleStreams};
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
    /// Arena index of the row whose tick this row shows (see [`Node::mirrors`]).
    mirror: Option<usize>,
    /// The Length and Size cells, empty where the row has none ([`title_cells`]).
    pub length: String,
    pub size: String,
    /// The Language cell, empty but on audio and subtitle rows.
    pub lang: String,
    /// The Item, Format and Notes cells ([`crate::engine::Row`]).
    pub item: String,
    pub format: String,
    pub notes: String,
}

impl Node {
    /// The arena index of the row this one mirrors, if it is no choice of its own.
    ///
    /// The DVD MPEG-2 multichannel extension row: "Its checkbox is **disabled and mirrors
    /// the base**" (mpg-output-design v5 §3). It shows its base's tick, is never
    /// [`checkable`](Node::checkable), and its PID is never sent as a selection.
    pub fn mirrors(&self) -> Option<usize> {
        self.mirror
    }

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

// ── preferred languages ───────────────────────────────────────────────────

/// The user's default language sets, from Settings ▸ Selection.
///
/// THREE independent sets, not one list with modifiers. "German & Spanish
/// audio, only German subtitles, and forced only if in English" is one request,
/// and it needs all three to be separate: forced subtitles translate signs and
/// foreign dialogue for someone listening in the dub, so the language you want
/// them in is not the language you want full subtitles in.
///
/// Each set is a SET, not a priority chain — every track matching ANY listed
/// language is kept, so "German & Spanish audio" keeps both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LangPrefs {
    pub presentation_language: Option<String>,
    pub audio: Vec<String>,
    pub subtitles: Vec<String>,
    /// Independent of `subtitles`, never a narrowing of it.
    pub forced: Vec<String>,
    /// Keep no regular subtitles at all (the selection bar's "None" and "Forced only").
    pub no_subtitles: bool,
    /// Keep no forced subtitles either (the selection bar's "None").
    pub no_forced: bool,
}

impl LangPrefs {
    /// Parse the three persisted strings.
    ///
    /// Separators are `,` and `;` only — NOT whitespace: a language may be
    /// given by name and plenty of names have a space in them ("Modern Greek",
    /// "Simplified Chinese"). Blank entries are dropped, so "de,,es" and
    /// trailing commas are harmless. Tags are kept verbatim — resolving a name
    /// or a 639-1/2T/2B/3 code is the engine's job, not ours.
    pub fn parse(audio: &str, subtitles: &str, forced: &str) -> Self {
        LangPrefs {
            presentation_language: None,
            audio: split_langs(audio),
            subtitles: split_langs(subtitles),
            forced: split_langs(forced),
            no_subtitles: false,
            no_forced: false,
        }
    }

    /// The preferences as persisted in Settings.
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        let mut p = Self::parse(&s.audio_langs, &s.sub_langs, &s.forced_sub_langs);
        p.presentation_language =
            (!s.presentation_language.trim().is_empty()).then(|| s.presentation_language.clone());
        match s.subtitle_mode.as_str() {
            "none" => {
                p.no_subtitles = true;
                p.no_forced = true;
            }
            "forced" => {
                p.no_subtitles = true;
                p.no_forced = false;
            }
            _ => {}
        }
        p
    }

    /// No preference expressed at all — the tree is built exactly as before.
    pub fn is_empty(&self) -> bool {
        self.audio.is_empty()
            && self.subtitles.is_empty()
            && self.forced.is_empty()
            && !self.no_subtitles
            && !self.no_forced
    }

    pub fn selection_preferences(&self) -> freemkv_engine::SelectionPreferences {
        freemkv_engine::SelectionPreferences {
            presentation_language: self.presentation_language.clone(),
        }
    }
}

/// The selection bar's subtitle choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubPick {
    All,
    None,
    /// Forced subtitles only, whatever their language.
    Forced,
    /// Toggle one language (regular and forced subtitles in it).
    Lang(String),
}

/// Stored title modes. "Episodes" is offered only when the shared selection
/// model returns an executable episode selection, not review-only candidates.
pub const PICK_TITLES: &[&str] = &[
    "Main film only",
    "Episodes",
    "Longest title",
    "All titles",
    "No titles",
];

// A title choice's label in the active locale.
fn pick_title_label(mode: &str) -> String {
    match mode {
        "Episodes" => crate::strings::get_or("gui.pick.episodes", "Episodes"),
        "Longest title" => crate::strings::get("gui.set.sel_longest"),
        "All titles" => crate::strings::get("gui.set.sel_all"),
        "No titles" => crate::strings::get_or("gui.pick.none", "None"),
        _ => crate::strings::get("gui.set.sel_main"),
    }
}

// Add `code` to `list`, or take it out if it is there (matched as a language, not as text).
fn toggle_code(list: &mut Vec<String>, code: &str) {
    match list.iter().position(|c| same_language(c, code)) {
        Some(i) => {
            list.remove(i);
        }
        None => list.push(code.to_string()),
    }
}

fn split_langs(s: &str) -> Vec<String> {
    s.split([',', ';'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// An empty list means "no preference", which the engine spells `All`.
fn lang_filter(tags: &[String]) -> freemkv_engine::StreamFilter {
    if tags.is_empty() {
        freemkv_engine::StreamFilter::All
    } else {
        freemkv_engine::StreamFilter::Langs(tags.to_vec())
    }
}

// Resolve one class's PIDs, falling back to "keep everything in this class" when the preference
// matched nothing that IS there.
fn class_or_fallback(f: freemkv_engine::PidFilter, all: &[u16]) -> Vec<u16> {
    match f {
        freemkv_engine::PidFilter::All => all.to_vec(),
        freemkv_engine::PidFilter::Only(p) if p.is_empty() => all.to_vec(),
        freemkv_engine::PidFilter::Only(p) => p,
    }
}

// Which of ONE title's stream PIDs the preferences keep. Reuses freemkv-engine's language
// matcher via resolve_stream_selection_forced.
fn preferred_pids(
    rows: &[&crate::engine::Row],
    prefs: &LangPrefs,
) -> std::collections::HashSet<u16> {
    use freemkv_engine::{StreamFilter, SubtitleFilter, resolve_stream_selection_forced};

    let mut title = libfreemkv::DiscTitle::empty();
    let (mut all_audio, mut all_normal, mut all_forced) = (Vec::new(), Vec::new(), Vec::new());
    for r in rows {
        let Some(pid) = r.pid else { continue };
        match r.type_s.as_str() {
            "Audio" => {
                all_audio.push(pid);
                title
                    .streams
                    .push(libfreemkv::Stream::Audio(libfreemkv::AudioStream {
                        pid,
                        codec: libfreemkv::Codec::Unknown(0),
                        channels: libfreemkv::AudioChannels::Unknown,
                        language: r.lang.clone(),
                        sample_rate: libfreemkv::SampleRate::Unknown,
                        secondary: false,
                        purpose: libfreemkv::LabelPurpose::Normal,
                        label: String::new(),
                    }));
            }
            // Everything else that carries a PID is a subtitle: the scan gives
            // audio and subtitle rows PIDs and nothing else, and video (always
            // kept) has none.
            _ => {
                if r.forced {
                    all_forced.push(pid);
                } else {
                    all_normal.push(pid);
                }
                title
                    .streams
                    .push(libfreemkv::Stream::Subtitle(libfreemkv::SubtitleStream {
                        pid,
                        codec: libfreemkv::Codec::Unknown(0),
                        language: r.lang.clone(),
                        forced: r.forced,
                        qualifier: libfreemkv::LabelQualifier::None,
                        codec_data: None,
                    }));
            }
        }
    }

    let none = || SubtitleFilter::split(StreamFilter::None, StreamFilter::None);
    let mut out: std::collections::HashSet<u16> = std::collections::HashSet::new();

    // Audio. An unresolvable tag (a typo) is not a reason to strip the audio:
    // fall back to keeping the class, like a language that simply isn't there.
    let audio = match resolve_stream_selection_forced(&title, &lang_filter(&prefs.audio), &none()) {
        Ok(sel) => class_or_fallback(sel.audio, &all_audio),
        Err(_) => all_audio.clone(),
    };
    out.extend(audio);

    // Non-forced subtitles, matched only against the `subtitles` set.
    let normal = resolve_stream_selection_forced(
        &title,
        &StreamFilter::None,
        &SubtitleFilter::split(lang_filter(&prefs.subtitles), StreamFilter::None),
    );
    if !prefs.no_subtitles {
        out.extend(match normal {
            Ok(sel) => class_or_fallback(sel.subtitle, &all_normal),
            Err(_) => all_normal.clone(),
        });
    }

    // Forced subtitles have no fallback, unlike audio/normal subs: missing them
    // is normal, but wrong-language ones auto-display during playback, worse
    // than none. Unmatched pref keeps nothing; empty pref (All) keeps everything.
    let forced = resolve_stream_selection_forced(
        &title,
        &StreamFilter::None,
        &SubtitleFilter::split(StreamFilter::None, lang_filter(&prefs.forced)),
    );
    if prefs.no_forced {
        return out;
    }
    out.extend(match forced {
        Ok(sel) => match sel.subtitle {
            // No preference expressed — keep the class, as before.
            freemkv_engine::PidFilter::All => all_forced.clone(),
            // A preference that matched nothing keeps nothing. See above.
            freemkv_engine::PidFilter::Only(p) => p,
        },
        // An unresolvable tag lands here. Keeping none is the safe direction
        // for the same reason: no forced subtitles is a normal file, whereas
        // five unwanted ones auto-display over the picture.
        Err(_) => Vec::new(),
    });

    out
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
    // Preserve the model's authored episode order without reordering display rows.
    episode_order: Vec<usize>,
}

// Does the minimum-title-length filter keep this title? ONE predicate shared by row display and
// "Default selection" ticking, so they can't disagree.
fn title_visible(duration_secs: f64, min_eff: f64) -> bool {
    !(duration_secs > 0.0 && duration_secs < min_eff)
}

impl Tree {
    /// Build from an engine scan. An empty scan yields an empty tree.
    ///
    /// `sel_mode` is the "Default selection" setting; it decides which titles start checked.
    /// `min_secs` hides titles shorter than it (known, non-zero duration), but never so
    /// aggressively the list is empty. `prefs` ranks equivalent language presentations and
    /// narrows checked stream rows; a category matching nothing retains available streams
    /// rather than producing silent output — see `preferred_pids`.
    pub fn from_scan(sc: &Scanned, sel_mode: &str, min_secs: f64, prefs: &LangPrefs) -> Self {
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
        let visible: std::collections::HashSet<usize> = titles
            .iter()
            .filter(|(_, d)| title_visible(*d, min_eff))
            .map(|(i, _)| *i)
            .collect();
        let selection = match sel_mode {
            "All titles" => freemkv_engine::Selection::All,
            "No titles" => freemkv_engine::Selection::Titles(Vec::new()),
            "Episodes" => freemkv_engine::Selection::Episodes,
            "Longest title" => freemkv_engine::Selection::Longest,
            _ => freemkv_engine::Selection::MainMovie,
        };
        // Select against the full evidence snapshot before applying visibility. Hiding the
        // main feature must not promote an unrelated title. Review candidates remain
        // available for manual ticking, but never become automatic executable choices.
        let report = sc.selection_model.select_with_preferences(
            &selection,
            &lang_filter(&prefs.audio),
            &prefs.selection_preferences(),
        );
        let episode_order = if matches!(selection, freemkv_engine::Selection::Episodes) {
            report.indices.clone()
        } else {
            Vec::new()
        };
        let selected: std::collections::HashSet<usize> = report
            .indices
            .into_iter()
            .filter(|i| visible.contains(i))
            .collect();

        // Which stream PIDs the language preferences keep, per canonical title index.
        // Computed only for titles that start checked (unchecked ones have no ticked
        // streams to narrow) and only when a preference exists, to avoid extra work.
        let keep: std::collections::HashMap<usize, std::collections::HashSet<u16>> =
            if prefs.is_empty() {
                Default::default()
            } else {
                selected
                    .iter()
                    .map(|&ti| {
                        let rows: Vec<&crate::engine::Row> = sc
                            .rows
                            .iter()
                            .filter(|r| r.depth >= 2 && r.title == ti)
                            .collect();
                        (ti, preferred_pids(&rows, prefs))
                    })
                    .collect()
            };

        let mut arena: Vec<Node> = Vec::new();
        let mut roots = Vec::new();
        let mut last_title: Option<usize> = None;
        let mut last_group: Option<usize> = None;
        let mut skip_title = false;
        for r in &sc.rows {
            match r.depth {
                0 => skip_title = false,
                1 => {
                    // Hide a too-short title (and everything under it).
                    skip_title = r.type_s == "Title" && !title_visible(r.duration_secs, min_eff);
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
            let (length, size) = title_cells(r);
            // Every painted cell passes one sanitizer: disc bytes never reorder, hide or forge text.
            let cell = crate::strings::sanitize_display;
            arena.push(Node {
                type_s: r.type_s.clone(),
                desc: cell(&r.desc),
                checkable: r.checkable,
                checked: RefCell::new(r.depth == 1 && selected.contains(&r.title)),
                children: vec![],
                info: r.info.clone(),
                pid: r.pid,
                title_idx: r.title,
                mirror: None,
                length,
                size,
                lang: stream_language(&r.lang),
                item: cell(&r.item),
                format: cell(&r.format),
                notes: cell(&r.notes),
            });
            match r.depth {
                0 => roots.push(idx),
                1 => {
                    if let Some(&root) = roots.first() {
                        arena[root].children.push(idx);
                    }
                    last_title = Some(idx);
                }
                3.. => {
                    if let Some(g) = last_group {
                        arena[g].children.push(idx);
                    }
                }
                _ => {
                    last_group = Some(idx);
                    if let Some(t) = last_title {
                        // A mirror row names its base by PID; the base is a sibling
                        // declared before it (libfreemkv places the extension after it).
                        arena[idx].mirror = r.mirrors.and_then(|base| {
                            arena[t]
                                .children
                                .iter()
                                .copied()
                                .find(|&c| arena[c].pid == Some(base) && arena[c].checkable)
                        });
                        arena[t].children.push(idx);
                        // A stream row starts checked when its TITLE does, narrowed by
                        // language preferences if the PID isn't one they keep. A row
                        // with no PID (video) is never narrowed: it's always retained.
                        let on = *arena[t].checked.borrow()
                            && match (r.pid, keep.get(&r.title)) {
                                (Some(pid), Some(set)) => set.contains(&pid),
                                _ => true,
                            };
                        *arena[idx].checked.borrow_mut() = on;
                    }
                }
            }
        }
        Tree {
            arena,
            roots,
            episode_order,
        }
    }

    /// Tick state for a row: the row's OWN flag decides `Off`, its checkable
    /// children decide `On` vs `Mixed`.
    ///
    /// The two halves answer two different questions: for a title the own flag is "rip this
    /// title" (what [`Tree::ticked_titles`] collects), while the children are "which of its
    /// tracks". `Off` means, exactly, "this will not be ripped". A ticked title with no ticked
    /// tracks is `Mixed`: it IS being ripped, and not all of it.
    pub fn check_state(&self, i: usize) -> Check {
        let n = &self.arena[i];
        if !*n.checked.borrow() {
            return Check::Off;
        }
        // A leaf, and a title with NO checkable children (a video-only title,
        // since `stream_rows` marks video rows uncheckable) have nothing that
        // could narrow them, so a ticked one is fully ticked.
        let all_on = n
            .children
            .iter()
            .filter(|&&c| self.arena[c].checkable())
            .all(|&c| *self.arena[c].checked.borrow());
        if all_on { Check::On } else { Check::Mixed }
    }

    /// What a CLICK on row `i`'s tick box does.
    ///
    /// This is a decision, so it lives here and not in a shell — both shells previously
    /// computed their own answer and disagreed on what a mixed state means. A partly-ticked
    /// title becomes fully ticked; clicking again clears it, the only reading that makes a
    /// second click undo the first.
    pub fn toggle(&self, i: usize) {
        // A mirror row's box is drawn disabled; a click that reaches the model anyway
        // (a shell whose widget cannot be disabled per row) changes nothing.
        if self.arena[i].mirror.is_some() {
            return;
        }
        let on = matches!(self.check_state(i), Check::Off | Check::Mixed);
        self.set_checked(i, on);
        // A track ticked under an unticked title would show as selected while its title,
        // which is what is ripped, stays out. Ticking the track picks the title up.
        if on
            && self.arena[i].children.is_empty()
            && let Some(parent) = self
                .arena
                .iter()
                .position(|n| n.children.contains(&i) && n.type_s == "Title")
        {
            *self.arena[parent].checked.borrow_mut() = true;
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
    ///
    /// A title is ripped when its BOX is not empty, i.e. `check_state` — the same fold that
    /// draws it — reports anything but [`Check::Off`]. `Mixed` is ripped — some tracks are
    /// still ticked, which is exactly what the partial glyph promises.
    pub fn ticked_titles(&self) -> Vec<usize> {
        let mut titles: Vec<_> = self
            .arena
            .iter()
            .enumerate()
            .filter(|(i, n)| {
                n.type_s == "Title"
                    && n.title_idx != usize::MAX
                    && self.check_state(*i) != Check::Off
            })
            .map(|(_, n)| n.title_idx)
            .collect();
        if !self.episode_order.is_empty() {
            titles.sort_by_key(|index| {
                self.episode_order
                    .iter()
                    .position(|i| i == index)
                    .unwrap_or(usize::MAX)
            });
        }
        titles
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
            // Only a choice is a selection: a mirror row's PID follows its base in
            // libfreemkv, and counting it would make an all-ticked title look narrowed.
            if !n.checkable() {
                continue;
            }
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

    /// The ticked PIDs of each title, keyed by CANONICAL title index.
    ///
    /// Unlike [`Tree::ticked_streams`], which unions every title's PIDs into one list, this
    /// keeps each title's selection separate — needed because Blu-ray playlists of the same
    /// feature routinely share PIDs. The union still decides `explicit_streams`, and is the
    /// fallback for a source with no title rows at all.
    pub fn ticked_streams_by_title(&self) -> TitleStreams {
        let mut out: Vec<(usize, Vec<u16>, Vec<u16>)> = Vec::new();
        for n in &self.arena {
            let Some(pid) = n.pid else { continue };
            // As in `ticked_streams`: a mirror row is no choice, so it is never sent.
            if !n.checkable() {
                continue;
            }
            // The disc/file header row carries the `usize::MAX` sentinel, not a
            // real title index; never let it become a phantom per-title entry (the
            // engine would rip title usize::MAX), as `ticked_titles` guards it.
            if n.title_idx == usize::MAX {
                continue;
            }
            let slot = match out.iter().position(|(t, _, _)| *t == n.title_idx) {
                Some(i) => i,
                None => {
                    out.push((n.title_idx, Vec::new(), Vec::new()));
                    out.len() - 1
                }
            };
            if !*n.checked.borrow() {
                continue;
            }
            if n.type_s == "Audio" {
                out[slot].1.push(pid);
            } else {
                out[slot].2.push(pid);
            }
        }
        TitleStreams::PerTitle(out)
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

// ── preferred-language pickers: used to be free text (e.g. `deu` vs `ger` vs
// `de`), where a typo was indistinguishable from "disc has no German". Shells
// now show a checklist of names and store ISO codes; this module owns both conversions.

/// The languages offered in the pickers, as (stored code, `gui.lang.<code>` English fallback).
///
/// ISO 639-2/T, which is what disc streams actually carry (`deu`, not `ger`;
/// `fra`, not `fre`) — so a stored value can be compared to a stream tag
/// without translating first.
///
/// Deliberately a CURATED list, not every code isolang knows. The full set is
/// several thousand entries, which is not a menu anyone can use; these are the
/// languages that appear on commercial discs. A code outside this list is
/// still honoured if it is already stored — see [`lang_selection`].
pub const PICKER_LANGUAGES: &[(&str, &str)] = &[
    ("eng", "English"),
    ("spa", "Spanish"),
    ("fra", "French"),
    ("deu", "German"),
    ("ita", "Italian"),
    ("por", "Portuguese"),
    ("nld", "Dutch"),
    ("swe", "Swedish"),
    ("nor", "Norwegian"),
    ("dan", "Danish"),
    ("fin", "Finnish"),
    ("isl", "Icelandic"),
    ("pol", "Polish"),
    ("ces", "Czech"),
    ("slk", "Slovak"),
    ("hun", "Hungarian"),
    ("ron", "Romanian"),
    ("bul", "Bulgarian"),
    ("ell", "Greek"),
    ("rus", "Russian"),
    ("ukr", "Ukrainian"),
    ("tur", "Turkish"),
    ("heb", "Hebrew"),
    ("ara", "Arabic"),
    ("hin", "Hindi"),
    ("tha", "Thai"),
    ("vie", "Vietnamese"),
    ("ind", "Indonesian"),
    ("zho", "Chinese"),
    ("jpn", "Japanese"),
    ("kor", "Korean"),
    ("cat", "Catalan"),
    ("hrv", "Croatian"),
    ("srp", "Serbian"),
    ("slv", "Slovenian"),
    ("est", "Estonian"),
    ("lav", "Latvian"),
    ("lit", "Lithuanian"),
];

/// Normalize one user-supplied tag to the code this module stores.
///
/// Accepts what the free-text boxes accepted — a 639-1 code (`en`), either
/// 639-2 form (`ger`/`deu`), 639-3, or an English name (`German`) — because
/// settings written before the pickers existed contain exactly those, and a
/// stored preference that silently stopped matching would look like the
/// feature had been dropped.
pub fn canonical_lang_code(tag: &str) -> Option<String> {
    let t = tag.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    // A name we already offer, matched case-insensitively.
    if let Some((code, _)) = PICKER_LANGUAGES
        .iter()
        .find(|(_, name)| name.to_ascii_lowercase() == lower)
    {
        return Some((*code).to_string());
    }
    isolang::Language::from_639_1(&lower)
        .or_else(|| isolang::Language::from_639_3(&lower))
        // 639-2/B bibliographic (`ger`, `fre`, `dut`, …) → its /T form, then resolve.
        // `isolang` only knows /T; without this, e.g. `ger` fell through to the
        // verbatim fallback, adding an unofferable duplicate row instead of ticking German.
        .or_else(|| bib_to_terminologic(&lower).and_then(isolang::Language::from_639_3))
        // English name, matched case-insensitively: `from_name` is exact-case
        // (`from_name("german")` misses), so a lowercased settings value or a
        // name outside PICKER_LANGUAGES would otherwise fall through.
        .or_else(|| {
            let needle = lower.clone();
            isolang::Language::match_names(move |name| name.eq_ignore_ascii_case(&needle)).next()
        })
        .map(|l| l.to_639_3().to_string())
}

// The ISO 639-2/B codes that differ from 639-2/T (= 639-3), mapped to /T. Deliberately a second
// copy of freemkv_engine::streams' private table.
fn bib_to_terminologic(code: &str) -> Option<&'static str> {
    Some(match code {
        "alb" => "sqi",
        "arm" => "hye",
        "baq" => "eus",
        "bur" => "mya",
        "chi" => "zho",
        "cze" => "ces",
        "dut" => "nld",
        "fre" => "fra",
        "geo" => "kat",
        "ger" => "deu",
        "gre" => "ell",
        "ice" => "isl",
        "mac" => "mkd",
        "mao" => "mri",
        "may" => "msa",
        "per" => "fas",
        "rum" => "ron",
        "slo" => "slk",
        "tib" => "bod",
        "wel" => "cym",
        _ => return None,
    })
}

/// The stored preference string parsed into canonical codes, order preserved
/// and duplicates dropped. Anything unrecognisable is kept VERBATIM rather
/// than discarded: it may be a valid tag this build does not know, and
/// silently deleting a user's setting is worse than carrying it along.
pub fn lang_selection(stored: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in stored
        .split([',', ';'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let code = canonical_lang_code(tag).unwrap_or_else(|| tag.to_string());
        if !out.iter().any(|c| c.eq_ignore_ascii_case(&code)) {
            out.push(code);
        }
    }
    out
}

/// Codes back to the stored form. One spelling, so a round trip through the
/// picker cannot rewrite a setting into a different-looking equivalent.
pub fn lang_selection_to_string(codes: &[String]) -> String {
    codes.join(",")
}

/// The picker button's title: the chosen languages in the active locale, or a word
/// meaning "no preference" — never an empty button, which reads as broken.
pub fn lang_summary(stored: &str) -> String {
    let codes = lang_selection(stored);
    if codes.is_empty() {
        return crate::strings::get_or("gui.set.lang_any", "Any");
    }
    codes
        .iter()
        .map(|c| lang_display_name(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The name for a stored code: a picker language in the active locale (English when the
/// catalog lacks `gui.lang.<code>`), any other known code in English, and an unknown tag as
/// itself so it is still visible rather than blank.
pub fn lang_display_name(code: &str) -> String {
    if let Some((c, english)) = PICKER_LANGUAGES
        .iter()
        .find(|(c, _)| c.eq_ignore_ascii_case(code))
    {
        return crate::strings::get_or(&format!("gui.lang.{c}"), english);
    }
    isolang::Language::from_639_3(&code.to_ascii_lowercase())
        .map(|l| l.to_name().to_string())
        .unwrap_or_else(|| code.to_string())
}

/// The Language cell for a stream's on-disc tag: its ISO 639 code, one spelling per language
/// (`ger` shows as `deu`), blank when the disc names no language ("und", or nothing).
pub fn stream_language(tag: &str) -> String {
    match canonical_lang_code(tag) {
        Some(c) if c != "und" => c,
        Some(_) => String::new(),
        None => crate::strings::sanitize_display(tag),
    }
}

// Whether two language tags (a code in any ISO 639 form, or a name) name the same language.
fn same_language(a: &str, b: &str) -> bool {
    match (canonical_lang_code(a), canonical_lang_code(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a.eq_ignore_ascii_case(b),
    }
}

/// Toggle one code in a stored preference and return the new stored string.
/// The single mutation both shells call, so a click means the same thing on
/// each and neither reimplements the set logic.
pub fn lang_toggle(stored: &str, code: &str) -> String {
    let mut codes = lang_selection(stored);
    match codes.iter().position(|c| c.eq_ignore_ascii_case(code)) {
        Some(i) => {
            codes.remove(i);
        }
        None => codes.push(code.to_string()),
    }
    lang_selection_to_string(&codes)
}

/// True when `code` is currently chosen — what draws each checkmark.
pub fn lang_is_selected(stored: &str, code: &str) -> bool {
    lang_selection(stored)
        .iter()
        .any(|c| c.eq_ignore_ascii_case(code))
}

/// The output sinks offered for a given source kind. Whole-disc sinks make no
/// sense for a container, so they are omitted rather than offered and failed.
/// `mp4_ok` is false when the source's video cannot go in an MP4 at all (a
/// DVD's MPEG-2, an HD DVD's VC-1); the option is then REMOVED rather than
/// offered-and-refused. Pass true when the codecs are unknown.
///
/// `disc_source` is "not a container": true for a physical disc AND an ISO file, since both
/// carry a whole disc to unpack.
pub fn output_formats(disc_source: bool, fit: impl Into<Fit>) -> Vec<Vec<&'static str>> {
    let fit = fit.into();
    let mut titles = vec!["Selected titles → MKV"];
    if fit.mp4 {
        titles.push("Selected titles → MP4");
    }
    if fit.mpg {
        titles.push("Selected titles → MPG");
    }
    titles.push("Selected titles → M2TS");
    titles.push("Selected titles → separate track files");
    // `video://`, `audio://`, `sub://` are `demux://` with a track-kind filter
    // (libfreemkv `mux::resolve`); they apply to any source the plain demux
    // sink does, container included, so they're always in the titles group.
    titles.push("Selected titles → video tracks only");
    titles.push("Selected titles → audio tracks only");
    titles.push("Selected titles → subtitle tracks only");
    let whole = vec!["Whole disc → ISO image", "Whole disc → decrypted folder"];
    let meta = vec!["Chapters → file", "Title info → JSON", "Video index → .fvi"];
    if disc_source {
        vec![titles, whole, meta]
    } else {
        vec![titles, meta]
    }
}

// Video codecs MP4 can actually carry; anything else has no MP4 mapping, so warn up front
// rather than fail at mux time. MUST match the mux gate in `libfreemkv::mux::mp4`.
const MP4_VIDEO: &[&str] = &["H.264", "HEVC"];

// Video codecs `mpg://` can carry: the 2000 edition of H.222.0 covers MPEG-1/2 only (J24);
// H.264, HEVC and VC-1 follow with F8. MUST match `libfreemkv::mux::mpg`'s plan.
const MPG_VIDEO: &[&str] = &["MPEG-2", "MPEG-1"];

/// Which of the codec-gated containers could hold at least one title of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fit {
    pub mp4: bool,
    pub mpg: bool,
}

impl From<bool> for Fit {
    /// `true` offers every container (codecs unknown); `false` none of the gated ones.
    fn from(ok: bool) -> Self {
        Fit { mp4: ok, mpg: ok }
    }
}

/// Resolve a popup's visible text back to the canonical format string.
///
/// Shells hold display text; the core holds the authoritative list. Matching
/// here means a shell never invents a format, and both shells resolve the same
/// way instead of each parsing the string.
pub fn format_by_title(
    title: &str,
    disc_source: bool,
    fit: impl Into<Fit>,
) -> Option<&'static str> {
    output_formats(disc_source, fit)
        .into_iter()
        .flatten()
        .find(|f| *f == title)
}

pub use crate::sources::container_scheme;

/// Source formats accepted by the file picker: ISO images and every [`crate::sources::CONTAINER_SOURCES`]
/// extension (plus the upper-case forms discs and pickers show).
pub const SOURCE_EXTS: &[&str] = &[
    "iso", "ISO", "mkv", "m2ts", "mts", "mp4", "mpg", "MPG", "mpeg", "MPEG", "vob", "VOB",
];

/// True for a container source (single title, no disc scan).
pub fn is_container(path: &str) -> bool {
    container_scheme(path).is_some()
}

/// A file a shell may open: an ISO image or a container source.
pub fn is_openable_file(path: &str) -> bool {
    is_container(path)
        || std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

/// The View ▸ log menu item's label, which follows STATE rather than naming
/// one fixed action: "Show log" only while the log is hidden, "Hide log" while
/// it is on screen. It is one toggle, so a label that always said "Show log"
/// was wrong half the time.
///
/// Lives here, not in a shell, because both menus are built from it — that is
/// the only way macOS and Windows can be guaranteed to say the same thing.
#[must_use]
pub fn log_menu_label(log_hidden: bool) -> String {
    if log_hidden {
        crate::strings::get_or("gui.menu.show_log", "Show log")
    } else {
        crate::strings::get_or("gui.menu.hide_log", "Hide log")
    }
}

// ── Menu layout, shared by every shell ────────────────────────────────────

/// A menu-driven action. Almost every entry maps to a plain [`Cmd`] the
/// shell just dispatches; the exceptions carry no user-facing text (they
/// live on the focused control's responder chain / edit-control message
/// map) so they never round-trip through the model.
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MenuAction {
    /// Fire this [`Cmd`] on the shared [`App`].
    Cmd(Cmd),
    /// The disc source picker: enumerate optical drives and open one. Lives
    /// per-shell because drive enumeration is OS-specific; `menu_layout`
    /// only names the item so it appears in the File menu at the same
    /// position on every OS.
    OpenDisc,
    /// Standard text commands. Mac wires them to `nil` (responder chain,
    /// so Copy in the log Just Works); Windows wires them to `IDM_COPY`
    /// / `IDM_SELECT_ALL_TEXT` on the focused edit control. Not every
    /// platform surfaces every one — see [`menu_layout`].
    StandardCopy,
    StandardCut,
    StandardPaste,
    StandardSelectAllText,
}

/// Cross-platform accelerator. `primary` is Ctrl on Windows/Linux and ⌘ on
/// macOS — each shell picks the right modifier when it renders. `key` is
/// the base character or virtual-key name; F-keys use `"F1"`, `"F4"`, etc.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Accel {
    pub key: &'static str,
    pub primary: bool,
    pub shift: bool,
    pub alt: bool,
}

impl Accel {
    /// The common case: primary modifier + a single character.
    pub const fn primary(key: &'static str) -> Self {
        Accel {
            key,
            primary: true,
            shift: false,
            alt: false,
        }
    }
    /// Primary + Shift + character (e.g. ⇧⌘A for Select-All-Titles).
    pub const fn primary_shift(key: &'static str) -> Self {
        Accel {
            key,
            primary: true,
            shift: true,
            alt: false,
        }
    }
    /// No modifier (F1 for Help/Docs).
    pub const fn bare(key: &'static str) -> Self {
        Accel {
            key,
            primary: false,
            shift: false,
            alt: false,
        }
    }
}

/// One entry in a menu — a command with an optional accelerator, or a
/// visual separator.
#[derive(Clone, Debug)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

/// One command in a menu.
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub action: MenuAction,
    /// Localized label. Precomputed from `strings::` so all shells present
    /// identical text (the log toggle picks its label from `log_hidden`).
    pub label: String,
    pub accel: Option<Accel>,
}

/// The logical group a menu belongs to. Windows and Linux merge
/// [`MenuGroupId::App`] into `File` (Settings, Exit/Quit) and `Help`
/// (About) per their conventions; macOS renders it as the app menu.
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MenuGroupId {
    App,
    File,
    Edit,
    View,
    Help,
}

/// One top-level menu.
#[derive(Clone, Debug)]
pub struct MenuGroup {
    pub id: MenuGroupId,
    /// Localized group title (e.g. "File", "Edit").
    pub title: String,
    pub entries: Vec<MenuEntry>,
}

/// The canonical menu structure for every shell.
///
/// Adding an item = one edit. Removing one = one edit. Reordering items
/// applies to every OS at once. The three shells then walk this list, map
/// each `MenuAction` to their native handler, and place [`MenuGroupId::App`]
/// wherever their platform's convention says.
///
/// `log_hidden` selects the live label for the log toggle — "Show log" or
/// "Hide log" — so a shell can rebuild its menu after the log is toggled
/// without recomputing the layout itself.
pub fn menu_layout(log_hidden: bool) -> Vec<MenuGroup> {
    let g = |k: &str, fallback: &str| crate::strings::get_or(k, fallback);
    let item = |action: MenuAction, label: String, accel: Option<Accel>| {
        MenuEntry::Item(MenuItem {
            action,
            label,
            accel,
        })
    };
    let sep = || MenuEntry::Separator;

    vec![
        MenuGroup {
            id: MenuGroupId::App,
            title: "freemkv".to_string(),
            entries: vec![
                item(
                    MenuAction::Cmd(Cmd::About),
                    g("gui.menu.app_about", "About freemkv"),
                    None,
                ),
                sep(),
                item(
                    MenuAction::Cmd(Cmd::Settings),
                    g("gui.menu.settings", "Settings…"),
                    Some(Accel::primary(",")),
                ),
                sep(),
                item(
                    MenuAction::Cmd(Cmd::Quit),
                    g("gui.menu.quit", "Quit freemkv"),
                    Some(Accel::primary("q")),
                ),
            ],
        },
        MenuGroup {
            id: MenuGroupId::File,
            title: g("gui.menu.file", "File"),
            entries: vec![
                item(
                    MenuAction::Cmd(Cmd::Open),
                    g("gui.menu.open", "Open…"),
                    Some(Accel::primary("o")),
                ),
                item(
                    MenuAction::OpenDisc,
                    g("gui.menu.open_disc", "Open disc…"),
                    Some(Accel::primary("d")),
                ),
                item(
                    MenuAction::Cmd(Cmd::Close),
                    g("gui.menu.close", "Close"),
                    Some(Accel::primary("w")),
                ),
                sep(),
                item(
                    MenuAction::Cmd(Cmd::SetOutput),
                    g("gui.menu.set_output", "Set output folder…"),
                    None,
                ),
                item(
                    MenuAction::Cmd(Cmd::Run),
                    g("gui.menu.start_rip", "Start rip"),
                    Some(Accel::primary("r")),
                ),
                sep(),
                item(
                    MenuAction::Cmd(Cmd::Eject),
                    g("gui.menu.eject", "Eject disc"),
                    Some(Accel::primary("e")),
                ),
            ],
        },
        MenuGroup {
            id: MenuGroupId::Edit,
            title: g("gui.menu.edit", "Edit"),
            entries: vec![
                // Cut/Paste ship only where the platform wires them — Mac
                // via the responder chain, Windows only surfaces Copy today.
                // Each shell filters what its menu renders.
                item(
                    MenuAction::StandardCut,
                    g("gui.menu.cut", "Cut"),
                    Some(Accel::primary("x")),
                ),
                item(
                    MenuAction::StandardCopy,
                    g("gui.menu.copy", "Copy"),
                    Some(Accel::primary("c")),
                ),
                item(
                    MenuAction::StandardPaste,
                    g("gui.menu.paste", "Paste"),
                    Some(Accel::primary("v")),
                ),
                item(
                    MenuAction::StandardSelectAllText,
                    g("gui.menu.select_all_text", "Select All"),
                    Some(Accel::primary("a")),
                ),
                sep(),
                item(
                    MenuAction::Cmd(Cmd::SelectAll),
                    g("gui.menu.select_all_titles", "Select All Titles"),
                    Some(Accel::primary_shift("A")),
                ),
                item(
                    MenuAction::Cmd(Cmd::SelectNone),
                    g("gui.menu.select_no_titles", "Select No Titles"),
                    None,
                ),
                item(
                    MenuAction::Cmd(Cmd::Invert),
                    g("gui.menu.invert_titles", "Invert Title Selection"),
                    None,
                ),
            ],
        },
        MenuGroup {
            id: MenuGroupId::View,
            title: g("gui.menu.view", "View"),
            entries: vec![
                item(
                    MenuAction::Cmd(Cmd::ToggleLog),
                    log_menu_label(log_hidden),
                    Some(Accel::primary("l")),
                ),
                item(
                    MenuAction::Cmd(Cmd::ClearLog),
                    g("gui.menu.clear_log", "Clear log"),
                    Some(Accel::primary("k")),
                ),
            ],
        },
        MenuGroup {
            id: MenuGroupId::Help,
            title: g("gui.menu.help", "Help"),
            entries: vec![
                item(
                    MenuAction::Cmd(Cmd::Docs),
                    g("gui.menu.docs", "freemkv Documentation"),
                    // macOS uses ⌘? (⇧⌘/ on the responder chain); Windows uses
                    // F1. Both shells map their own accelerator when rendering.
                    Some(Accel::bare("F1")),
                ),
                item(
                    MenuAction::Cmd(Cmd::CheckUpdates),
                    g("gui.menu.check_updates", "Check for updates…"),
                    None,
                ),
            ],
        },
    ]
}

/// Commands that must be unavailable while a rip is in flight. Cancel is
/// deliberately absent — it must always be reachable.
pub fn blocked_while_running(cmd: Cmd) -> bool {
    !matches!(
        cmd,
        Cmd::Cancel
            | Cmd::About
            | Cmd::Docs
            | Cmd::Quit
            // Showing/clearing the log is VIEW state that touches nothing the rip
            // reads, so blocking it bought no safety — it just disabled the log
            // during the one time (mid-rip) someone actually wants to watch it.
            | Cmd::ToggleLog
            | Cmd::ClearLog
    )
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
    ///
    /// `scanned` is what the scan says the rip will read
    /// ([`scanned_source_bytes`]). A source that is a regular file (an ISO
    /// image, a container) shows the file's length; anything else (a drive, a
    /// disc folder) shows `scanned`, or an em dash without it.
    pub fn starting(source: &str, dest: &str, scanned: Option<u64>) -> Self {
        let file_len = std::fs::metadata(source)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        InfoRows {
            source: source.to_string(),
            // A drive source (`disc://…`) has no file name: a dash, not the URL's tail.
            source_file: if source.contains("://") {
                "—".to_string()
            } else {
                std::path::Path::new(source)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("—")
                    .to_string()
            },
            // Never leave the row blank — a blank Information field reads as
            // a broken panel (reported). An unknown value is an em dash.
            source_size: file_len
                .or(scanned)
                .map(fmt_bytes)
                .unwrap_or_else(|| "—".into()),
            read_rate: "—".into(),
            output_file: dest.to_string(),
            output_size: "0 B".into(),
            // Never measured here, on the UI thread: the caller fills in its cached answer.
            free_space: "—".into(),
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

/// What a rip from a drive or disc folder reads, from the scan: the disc's
/// capacity for a whole-disc output, else the sum of the ticked titles' sizes.
/// `None` when any part of that is unknown (a `0` size or capacity, a title the
/// scan does not list, nothing ticked), so the row never shows a guess.
pub fn scanned_source_bytes(
    format: &str,
    titles: &[usize],
    title_sizes: &[u64],
    capacity: u64,
) -> Option<u64> {
    if format.starts_with("Whole disc") {
        return (capacity > 0).then_some(capacity);
    }
    if titles.is_empty() {
        return None;
    }
    titles.iter().try_fold(0u64, |sum, t| {
        let size = *title_sizes.get(*t)?;
        (size > 0).then(|| sum.saturating_add(size))
    })
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

/// The saving captions once Stop is pressed (stop design v5 §3.2 (A), ST-I2's strings):
/// "Finishing …" when every title is already written; else "Stopping …".
/// `None` while no Stop is pending.
pub fn stop_caption(stopping: bool, titles_done: usize, run_titles: usize) -> Option<String> {
    match (stopping, titles_done >= run_titles.max(1)) {
        (false, _) => None,
        (true, true) => Some(crate::strings::get_or("stop.finishing", "Finishing …")),
        (true, false) => Some(crate::strings::get_or("stop.stopping", "Stopping …")),
    }
}

/// The container word for a chosen output format ("MKV" / "MP4" / "ISO" /
/// "chapter" / …), for the progress caption "Saving to {container} file".
///
/// Answered by the ENGINE, from the same `out_kind` dispatch that decides
/// which sink actually runs, so the caption cannot name a container the run
/// does not produce. It used to be a local MP4/M2TS/else-MKV test here, which
/// told every ISO, folder, demux, chapter, JSON and .fvi run that it was
/// writing an MKV.
/// The engine's word, localized where it is a word rather than a format name.
pub fn container_label(format: &str) -> String {
    let word = crate::engine::container_word(format);
    match word {
        "chapter" => crate::strings::get_or("gui.progress.word_chapter", "chapter"),
        "track" => crate::strings::get_or("gui.progress.word_track", "track"),
        "video track" => crate::strings::get_or("gui.progress.word_video_track", "video track"),
        "audio track" => crate::strings::get_or("gui.progress.word_audio_track", "audio track"),
        "subtitle track" => {
            crate::strings::get_or("gui.progress.word_subtitle_track", "subtitle track")
        }
        _ => word.to_string(),
    }
}

/// The `gui.format.*` translation key for a canonical output-format string, or
/// `None` for a string that is not one of the picker's formats.
///
/// Split out of [`format_label`] so the invariant "every string
/// [`output_formats`] offers has a translation key" is directly testable. It
/// was not, and three picker rows shipped with no key: `format_label` fell
/// through to its catch-all and returned raw English in all 29 locales, which
/// looks identical to a working translation under `en`.
pub fn format_key(canonical: &str) -> Option<&'static str> {
    Some(match canonical {
        "Selected titles → MKV" => "gui.format.mkv",
        "Selected titles → MP4" => "gui.format.mp4",
        "Selected titles → MPG" => "gui.format.mpg",
        "Selected titles → M2TS" => "gui.format.m2ts",
        "Selected titles → separate track files" => "gui.format.tracks",
        "Selected titles → video tracks only" => "gui.format.video_only",
        "Selected titles → audio tracks only" => "gui.format.audio_only",
        "Selected titles → subtitle tracks only" => "gui.format.sub_only",
        "Whole disc → ISO image" => "gui.format.iso",
        "Whole disc → decrypted folder" => "gui.format.folder",
        "Chapters → file" => "gui.format.chapters",
        "Title info → JSON" => "gui.format.json",
        "Video index → .fvi" => "gui.format.fvi",
        _ => return None,
    })
}

/// Localized display text for a canonical output-format string. The canonical
/// string (returned by `output_formats`, stored in `App.format`, matched by
/// `.contains(...)` in the engine) stays English so ripping keeps working; only
/// what the picker SHOWS is translated. An unknown format returns as-is.
pub fn format_label(canonical: &str) -> String {
    match format_key(canonical) {
        // `strings::get` echoes the dotted path when a key is missing from both
        // the active locale and English (e.g. a new row not yet in the pinned
        // `freemkv-i18n` tag). That's worse than English, so fall back instead.
        Some(key) => crate::strings::get_or(key, canonical),
        None => canonical.to_string(),
    }
}

#[cfg(test)]
#[path = "ui_missing_key_fallback_tests.rs"]
mod missing_key_fallback_tests;

/// Inverse of [`format_label`]: resolve a LOCALIZED popup label back to the canonical format
/// string, since `format_by_title` only matches English.
pub fn format_from_label(
    label: &str,
    disc_source: bool,
    fit: impl Into<Fit>,
) -> Option<&'static str> {
    // Canonical (English) fast path first — also covers callers that pass a
    // canonical string directly — then fall back to the localized display.
    let fit = fit.into();
    format_by_title(label, disc_source, fit).or_else(|| {
        output_formats(disc_source, fit)
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

// ── settings dropdowns ────────────────────────────────────────────────────

/// The option table for a settings dropdown: `(canonical, localized_label)`
/// pairs in menu order. The canonical value is what persists and what the
/// engine matches on; the label is the localized dropdown text. An empty
/// result means "not an enum dropdown".
///
/// The returned order is the menu order, and callers may map a selected INDEX back to
/// `opts[i].0`; `"container"` is deliberately absent since the macOS format popup carries
/// separator rows and maps by title.
pub fn enum_options(key: &str) -> Vec<(&'static str, String)> {
    let g = crate::strings::get;
    match key {
        "selection" => vec![
            ("Main film only", g("gui.set.sel_main")),
            ("All titles", g("gui.set.sel_all")),
            ("Longest title", g("gui.set.sel_longest")),
        ],
        "rip_mode" => vec![
            ("Multi-pass", g("gui.set.mode_multi")),
            ("Single pass", g("gui.set.mode_single")),
        ],
        "key_source" => vec![
            ("Local keydb only", g("gui.set.key_src_local")),
            ("Online key service only", g("gui.set.key_src_online")),
            ("keydb, then online", g("gui.set.key_src_both")),
        ],
        "log_level" => vec![
            ("Quiet", g("gui.set.log_quiet")),
            ("Normal", g("gui.set.log_normal")),
            ("Verbose", g("gui.set.log_verbose")),
            ("Debug", g("gui.set.log_debug")),
        ],
        // Language: canonical is the locale code, label the endonym (shown
        // as-is in every locale) or, for "auto", the localized word. Driven straight from
        // the shipped list, so the picker can never drift from what freemkv-i18n can load.
        "language" => LOCALES
            .iter()
            .map(|(endonym, code)| match *code {
                "auto" => (
                    *code,
                    crate::strings::get_or("gui.set.language_auto", "Auto"),
                ),
                _ => (*code, (*endonym).to_string()),
            })
            .collect(),
        _ => vec![],
    }
}

// ── title-row cells: the tree's Length and Size columns ───────────────────

/// A row's Length and Size cells, as `(length, size)`.
///
/// Length is the running time of a Title row (depth 1), [`fmt_hms`]-formatted,
/// and empty on every other row. Size is [`fmt_title_size`] of the row's
/// `size_bytes`, empty where the scan reports none.
pub fn title_cells(r: &crate::engine::Row) -> (String, String) {
    let length = if r.depth == 1 || r.type_s == "Chapter" {
        fmt_hms(r.duration_secs.max(0.0) as u64)
    } else {
        String::new()
    };
    (length, r.size_bytes.map(fmt_title_size).unwrap_or_default())
}

/// A title's size in decimal units, as `freemkv info` reports it: `"6.8 GB"`,
/// or whole megabytes below what would round to 1.0 GB (`"734 MB"`).
pub fn fmt_title_size(bytes: u64) -> String {
    if bytes >= 999_500_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

#[cfg(test)]
#[path = "ui_title_cell_tests.rs"]
mod title_cell_tests;

// ── tree shape ────────────────────────────────────────────────────────────

/// The parent of every row, derived from the `depth` column alone: depth 0
/// starts a new root, depth 1 hangs off the most recent root, anything
/// deeper hangs off the most recent depth-1 row.
///
/// A row that arrives before its parent has no parent to hang from. It becomes a root rather
/// than being dropped — a row the core decided to show must always be reachable.
pub fn row_parents(rows: &[Row]) -> Vec<Option<usize>> {
    let (mut last_root, mut last_title, mut last_group) = (None, None, None);
    let mut out = Vec::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        match r.depth {
            0 => {
                last_root = Some(i);
                last_title = None;
                out.push(None);
            }
            1 => {
                last_title = Some(i);
                last_group = None;
                out.push(last_root);
            }
            2 => {
                last_group = Some(i);
                out.push(last_title);
            }
            _ => out.push(last_group.or(last_title)),
        }
    }
    out
}

/// Whether a row's group starts closed: a title's chapter list is there to look at on demand,
/// not to push the streams of the next title off the screen.
pub fn starts_collapsed(r: &Row) -> bool {
    r.type_s == "Chapters"
}

/// Which row a freshly-rebuilt tree should leave sitting at the top.
///
/// Not simply "row 0": under the default "Main film only" the single ticked title can be
/// anywhere in the disc's order, and that title is the point of the screen. So it is the first
/// TICKED row, which under "All titles" is row 0 anyway. Nothing ticked (an empty disc, or a
/// preset that selected nothing) falls back to the first row.
pub fn first_visible_row(rows: &[Row]) -> Option<usize> {
    rows.iter()
        .position(|r| matches!(r.check, Some(Check::On) | Some(Check::Mixed)))
        .or(if rows.is_empty() { None } else { Some(0) })
}

// ── output naming ─────────────────────────────────────────────────────────

/// Whether `format` is one of the options `formats` currently offers.
///
/// `View` publishes the chosen format and the offered list as two independent fields, and
/// nothing kept them in agreement. Opening a source that withdraws an option — an MPEG-2 DVD
/// after an H.264 Blu-ray withdraws MP4 — left the model holding a format no longer on the
/// list.
pub fn format_is_offered(format: &str, formats: &[Vec<&'static str>]) -> bool {
    formats.iter().any(|g| g.contains(&format))
}

/// The format a source should end up on, given what it can actually offer.
///
/// Keeps the current choice when it is still available, and otherwise falls
/// back to the first option on the list. Returning the fallback rather than
/// applying it keeps this assertable on its own.
pub fn reconcile_format<'a>(format: &'a str, formats: &[Vec<&'static str>]) -> &'a str {
    if format_is_offered(format, formats) {
        return format;
    }
    formats.iter().flatten().next().copied().unwrap_or(format)
}

/// Whether the request asks for true-multipass recovery.
///
/// Both halves are load-bearing and neither was asserted. Forced true, every
/// single-pass rip runs a full sweep+patch recovery — hours of extra drive time
/// the user did not ask for. Forced false, `--multipass` at 5 passes silently
/// does nothing, and the abort-for-loss gate that depends on it never runs, so
/// a damaged disc muxes to a hole-ridden file reported as written.
///
/// `max_passes == 0` means "no passes", so it is not multipass whatever the
/// mode says.
pub fn wants_multipass(rip_mode: &str, max_passes: u32) -> bool {
    rip_mode == "Multi-pass" && max_passes > 0
}

/// The `max_passes` a rip request actually carries: zeroed for anything but
/// Multi-pass. `max_passes` alone drives multipass recovery downstream
/// (`fe::plan_passes`), so a stale non-zero setting (left over from a prior
/// Multi-pass run) must not smuggle multipass recovery into a Single-pass rip.
pub fn effective_max_passes(rip_mode: &str, typed_passes: u32) -> u32 {
    if wants_multipass(rip_mode, typed_passes) {
        typed_passes
    } else {
        0
    }
}

/// Whether `--raw` (keep-encrypted) actually applies to this output.
///
/// Ciphertext passthrough only means anything for a whole-disc ISO image; for
/// any mux it would write encrypted bytes into a container that claims to hold
/// video. Mirrors the CLI's iso-only rule rather than silently forwarding the
/// setting. It also mirrors the CLI's disc-only rule: `--raw` copies ciphertext
/// straight off the drive, so it is meaningless (and refused) for an image
/// source, which is already just bytes on disk.
pub fn raw_applies(raw_setting: bool, iso_output: bool, drive_source: bool) -> bool {
    raw_setting && iso_output && drive_source
}

/// Whether the user has narrowed the tracks down to video only.
///
/// Allowed — some people want a video-only extract — but never silently: a
/// file with no audio is usually an accident, and it is far cheaper to say so
/// before the rip than after. `explicit_streams` is what separates "unticked
/// everything" from "made no choice at all", which keeps every track.
pub fn is_video_only_selection(explicit_streams: bool, audio: &[u16], sub: &[u16]) -> bool {
    explicit_streams && audio.is_empty() && sub.is_empty()
}

/// The file (or directory) the run will actually produce, for the Information
/// panel.
///
/// Answered by the ENGINE — `planned_output_name` is built from the same
/// `out_kind` dispatch and the same `title_basename` the rip itself uses.
/// This used to be a parallel `<source stem>_t<n>.<ext>` guess that ignored
/// the filename template, ignored the disc label the engine actually names
/// disc titles after, and called every whole-disc sink an MKV.
pub fn output_file_name(
    source: &str,
    dir: &str,
    format: &str,
    first_title: Option<usize>,
    template: &str,
    disc_label: &str,
) -> String {
    crate::engine::planned_output_name(source, dir, format, first_title, template, disc_label)
}

// ══ the application core ══ Model/Update/View: `App` owns all state and
// decisions; a shell only renders `App::view()`, calls `App::dispatch(cmd)`
// on input, and performs the returned `Effect`s — no behavior of its own.

use crate::engine::{KeyConfig, RipRequest, RunState};
use crate::settings::Settings;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// What Eject does for the open source: only a live drive has a tray.
#[derive(Debug, PartialEq)]
enum EjectAction {
    NothingToEject,
    Eject,
}

fn eject_action(source: &str) -> EjectAction {
    if crate::engine::is_disc_source(source) {
        EjectAction::Eject
    } else {
        EjectAction::NothingToEject
    }
}

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
    /// Fire a native desktop notification announcing a run's end. The
    /// core builds `title` / `body` from `strings::` so all three shells
    /// present the same wording; `output_dir` is the folder the run wrote to,
    /// `None` when it did not complete (nothing to reveal). Gated on
    /// `Settings.notify_when_rip_finished`.
    NotifyRipFinished {
        title: String,
        body: String,
        output_dir: Option<String>,
    },
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

// Scan a source by its kind. The ONE dispatch from a URL to a scan, shared
// by the synchronous open and the launch probe's worker thread. Free-standing
// rather than a method because the worker holds no `App`.
fn scan_source(path: &str, keys: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    if is_container(path) {
        crate::engine::scan_stream_under(path, keys, tok)
    } else if crate::engine::is_disc_source(path) {
        crate::engine::scan_disc_with_keys(path, keys, tok)
    } else {
        crate::engine::scan_with_keys_under(path, keys, tok)
    }
}

// The launch probe's scan: no drive at all is the same quiet failure as an empty tray.
fn probe_source(path: &str, keys: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    if crate::engine::is_disc_source(path) && crate::engine::list_optical_drives().is_empty() {
        return Err(String::new());
    }
    scan_source(path, keys, tok)
}

use crate::engine::OpenToken;

/// A source scan under its open's token, as a value so unit tests can swap in one that
/// never touches a drive.
type ScanFn = fn(&str, &KeyConfig, &OpenToken) -> Result<Scanned, String>;

/// (open scan, probe scan). Unit tests get a pair that refuses `disc://` outright.
#[cfg(not(test))]
const SCANNERS: (ScanFn, ScanFn) = (scan_source, probe_source);
#[cfg(test)]
const SCANNERS: (ScanFn, ScanFn) = (tests::no_drive_scan, tests::no_drive_probe);

/// The disc watch's media check. Unit tests get one that answers "unknown" untouched.
#[cfg(not(test))]
const PRESENCE: fn(&str) -> Option<bool> = crate::engine::disc_present;
#[cfg(test)]
const PRESENCE: fn(&str) -> Option<bool> = tests::no_drive_presence;

/// How often the idle disc watch asks whether the open disc is still in its drive.
const PRESENCE_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// The longest Done or Start waits for a presence verdict before going on without it.
const PRESENCE_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

// The autodetect disc URL: try every drive, take the one holding media.
// Named because the launch probe passes it through three places and a
// typo in any of them would scan the wrong thing.
const PROBE_SOURCE: &str = "disc://";

/// Everything the app knows. No widgets, no platform types.
pub struct App {
    pub tree: Tree,
    pub settings: Settings,
    pub page: Page,
    pub log: Arc<Vec<LogLine>>,
    /// Sequence number of `log[0]`; bumps on every trim or clear, so a shell
    /// that appended incrementally knows its rendered lines went stale.
    pub log_first: u64,
    pub source: String,
    /// Last answered state of the optical tray. This is separate from
    /// `source`: a failed launch probe has no open source but still proves the
    /// tray is empty.
    pub disc_present: Option<bool>,
    pub output_dir: String,
    /// Free space at `output_dir`, measured off this thread.
    pub free: FreeSpace,
    pub format: String,
    pub log_hidden: bool,
    pub run: Option<Arc<RunState>>,
    pub run_titles: usize,
    pub run_started: Option<std::time::Instant>,
    pub info: Option<InfoRows>,
    pub result_summary: String,
    /// Typed verdict for `result_summary` — never re-derive it from the text.
    pub result_outcome: crate::engine::RunOutcome,
    pub selected_row: Option<usize>,
    /// Video codec per title, from the scan — used to warn when the chosen
    /// container cannot carry them. Public alongside the rest of the model so
    /// the container gate can be exercised without a real disc: it is the one
    /// input to `fit`/`container_mismatch`, and gating those tests
    /// behind a fixture is why they did not run in CI.
    pub video_codecs: Vec<String>,
    /// Each title's size and the disc's capacity, from the scan
    /// (`Scanned::title_sizes`, `Scanned::capacity_bytes`): the Source size row
    /// of a source that is not a file.
    pub title_sizes: Vec<u64>,
    pub capacity_bytes: u64,
    /// What each title NUMBER referred to on the scan the tree was built from,
    /// indexed by canonical title index.
    ///
    /// The ticked numbers travel to the engine, which SCANS AGAIN before it
    /// muxes them — and everything the operator does between opening the disc
    /// and pressing Start (reviewing streams, choosing a format, swapping the
    /// disc) happens in between. Sent with the request so the engine can prove
    /// its own scan still means what the tree said, instead of trusting an
    /// integer across the longest window in the app.
    pub title_ids: Vec<crate::engine::TitleIdentity>,
    /// The disc's RAW volume id from the scan, empty for a container source or
    /// when the disc carries none.
    ///
    /// Raw on purpose: this is not display text (`Scanned::label` is, and it is
    /// sanitised and given a "(no label)" fallback for the log pane) — it is
    /// the string the ENGINE will build the output filename from, so the panel
    /// can name the file that will actually appear. It goes nowhere near a
    /// path without `sanitize_label`, which `title_basename` applies.
    pub disc_label: String,
    /// The selection bar: the title choice and the language lists that decide what starts
    /// ticked. From Settings at open; the bar changes them and re-ticks the tree.
    pub pick_mode: String,
    pub pick_prefs: LangPrefs,
    // The scan the tree was built from, and its minimum title length, so a change re-ticks.
    pick_scan: Option<Scanned>,
    pick_min_secs: f64,
    /// The key set Open resolved: the rip's seed (KU §2.5). Memory only.
    seed: Option<crate::engine::KeySet>,
    /// Open or the last run refused E7034: the next Start is the insert-the-disc Retry.
    vid_retry: bool,
    /// Open's answered key refusal, under the key settings it was given for (B2).
    open_refusal: Option<(crate::engine::KeyRefusal, crate::engine::KeySnapshot)>,
    /// The launch probe's in-flight scan, if one is running.
    ///
    /// Its OWN slot, deliberately not `run`. `run` means "a rip is in
    /// progress" and `view`/`tick` read it as exactly that — putting the probe
    /// there would show `Page::Progress` for a rip that is not happening, and
    /// a Cancel button wired to nothing.
    probe: Option<Arc<ProbeState>>,
    opening: Option<std::sync::mpsc::Receiver<OpenedSource>>,
    /// An explicit drive open waiting for the probe to let go of the drive.
    pending: Option<PendingOpen>,
    /// The in-flight background open's token (stop design v5 §4.3).
    open_token: Option<OpenToken>,
    /// T29's idle window for the launch probe: [`PROBE_GRACE`], shorter in tests.
    probe_window: std::time::Duration,
    /// The folder the current run writes to, fixed at Start: the setting can change mid-rip.
    run_dest: String,
    scan: ScanFn,
    probe_scan: ScanFn,
    /// An Eject in flight: the source it ejects, and its worker's verdict
    /// (collected on the tick).
    ejecting: Option<(String, std::sync::mpsc::Receiver<Result<String, String>>)>,
    /// The SCSI eject the worker runs — a seam so tests never touch a drive.
    eject_fn: fn(&str) -> Result<String, String>,
    /// The disc watch's check in flight: the source it asks about, and its verdict.
    presence: Option<(String, std::sync::mpsc::Receiver<Option<bool>>)>,
    /// When the disc watch last asked; `None` asks on the next tick.
    presence_at: Option<std::time::Instant>,
    /// The disc watch's cadence: [`PRESENCE_EVERY`], zero in tests.
    presence_every: std::time::Duration,
    /// The media-presence check the worker runs — a seam so tests never touch a drive.
    presence_fn: fn(&str) -> Option<bool>,
    /// A Check for updates in flight: its worker's one-line verdict, collected on the tick.
    update_check: Option<std::sync::mpsc::Receiver<String>>,
    /// The network check the worker runs — a seam so tests never reach GitHub.
    update_fn: fn(&str) -> String,
    /// Highest unreadable-sector count already announced, so the notice is
    /// not repeated on every 100 ms tick.
    reported_bad: u64,
}

/// The launch probe's handoff: a worker thread writes the scan result once,
/// and `App::tick` picks it up on the UI thread.
///
/// The worker produces a `Result<Scanned, String>` and NOTHING else. Every
/// `App` mutation the result implies — the tree, the log, the page — happens
/// on the tick, because `App` is the UI thread's and is not `Sync`. A worker
/// that could touch it would make the probe a second writer to the model the
/// shell is drawing from.
pub struct ProbeState {
    /// The source the worker scanned. Carried rather than re-derived on the
    /// tick, so the result is classified (container / disc / image) against
    /// exactly the path it came from.
    path: String,
    result: Mutex<Option<Result<crate::engine::Scanned, String>>>,
    /// `Release` on the store, `Acquire` on the load, so seeing `true`
    /// guarantees the `result` write is visible — the same pairing
    /// `RunState::finished` uses.
    done: AtomicBool,
    /// The probe's open token: its Stop and its progress (stop design v5 §4.3, T29).
    token: OpenToken,
    /// T29: idle-only over `token.progress`, so a CDB or key call in flight never counts.
    stall: Mutex<libfreemkv::halt::StallTimer>,
    /// After the Stop: [`PROBE_RELEASE`] windows with no CDB completing, then abandoned.
    release: Mutex<Option<libfreemkv::halt::StallTimer>>,
    window: std::time::Duration,
}

impl ProbeState {
    fn new(path: &str, window: std::time::Duration) -> Self {
        let token = OpenToken::default();
        let stall = libfreemkv::halt::StallTimer::idle_only(window, &token.progress);
        ProbeState {
            path: path.to_string(),
            result: Mutex::new(None),
            done: AtomicBool::new(false),
            token,
            stall: Mutex::new(stall),
            release: Mutex::new(None),
            window,
        }
    }

    // A cancelled worker that still has not let go after its release bound (a hung driver):
    // the UI stops waiting for it. Plain stall timer: a CDB hung `busy()` must not pause it.
    fn abandoned(&self) -> bool {
        if !self.token.halt.is_cancelled() {
            return false;
        }
        let mut t = self.release.lock().unwrap_or_else(|e| e.into_inner());
        let p = &self.token.progress;
        let t = t.get_or_insert_with(|| {
            libfreemkv::halt::StallTimer::new(self.window * PROBE_RELEASE, p)
        });
        matches!(t.poll(p), libfreemkv::halt::Stall::Expired)
    }

    // T29 (stop design v5 §3.1): "30 s of **idle** no-progress"; true once it cancelled.
    fn stalled(&self) -> bool {
        let mut t = self.stall.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(
            t.poll(&self.token.progress),
            libfreemkv::halt::Stall::Expired
        ) {
            self.token.halt.cancel();
        }
        self.token.halt.is_cancelled()
    }
}

/// T29, the launch probe's idle window (stop design v5 §3.1): with no progress for this
/// long the probe's open token is cancelled, and the worker lets go of the drive. A CDB
/// or key call in flight pauses it, so a slow-but-working drive still lands its result.
pub const PROBE_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// T29 windows a cancelled probe gets to let go of the drive before the UI abandons it.
/// §3.2: a recovery READ's Stop takes "60 s + K" (K ≤ 13 s), within 3 × 30 s.
const PROBE_RELEASE: u32 = 3;

/// A drive is single-open, so a second scan while the probe holds it fails for a
/// readable disc. The open waits; if it names the probe's own source it adopts the result.
struct PendingOpen {
    path: String,
    background: bool,
}

struct OpenedSource {
    path: String,
    scanned: Result<Scanned, String>,
    preflight: Option<Result<Vec<String>, String>>,
}

/// An explicit Open Disc, given the drives found: the log line and the URL to scan (`None` =
/// no drive, stop). One or several drives is the rule the Linux shell shares, since it
/// enumerates off the UI thread and cannot call [`App::disc_source`].
pub(crate) fn disc_open_plan(
    drives: &[crate::engine::OpticalDrive],
) -> (LogKind, String, Option<String>) {
    use crate::strings::{fmt_or, get_or, sanitize_display};
    match drives {
        [] => (
            LogKind::Notice,
            get_or(
                "gui.log.no_drive",
                "No optical drive found. Connect a Blu-ray/DVD drive with a disc.",
            ),
            None,
        ),
        // One drive → that device; several → autodetect the one with media. The label is
        // the drive's own vendor/model string (hardware-supplied), so it is sanitized.
        [d] => (
            LogKind::Detail,
            fmt_or(
                "gui.log.opening_drive",
                "Opening {label} ({device})",
                &[
                    ("label", &sanitize_display(&d.label)),
                    ("device", &d.device),
                ],
            ),
            Some(format!("disc://{}", d.device)),
        ),
        _ => {
            let list = drives
                .iter()
                .map(|d| format!("{} ({})", sanitize_display(&d.label), d.device))
                .collect::<Vec<_>>()
                .join(", ");
            let n = drives.len().to_string();
            (
                LogKind::Detail,
                fmt_or(
                    "gui.log.drives_found",
                    "{n} drives found: {list} — using the one with a disc",
                    &[("n", &n), ("list", &list)],
                ),
                Some(PROBE_SOURCE.to_string()),
            )
        }
    }
}

impl App {
    pub fn new() -> Self {
        let settings = Settings::load();
        let output_dir = settings.dest_dir.clone();
        // `Settings::load` normalizes the container to one the picker offers.
        let format = settings.container.clone();
        let mut app = App {
            tree: Tree::default(),
            settings,
            page: Page::Empty,
            log: Arc::default(),
            log_first: 0,
            source: String::new(),
            disc_present: None,
            output_dir,
            free: FreeSpace::default(),
            format,
            log_hidden: false,
            run: None,
            run_titles: 0,
            run_started: None,
            info: None,
            result_summary: String::new(),
            result_outcome: crate::engine::RunOutcome::default(),
            selected_row: None,
            video_codecs: Vec::new(),
            title_sizes: Vec::new(),
            capacity_bytes: 0,
            title_ids: Vec::new(),
            disc_label: String::new(),
            pick_mode: String::new(),
            pick_prefs: LangPrefs::default(),
            pick_scan: None,
            pick_min_secs: 0.0,
            seed: None,
            vid_retry: false,
            open_refusal: None,
            probe: None,
            opening: None,
            pending: None,
            open_token: None,
            probe_window: PROBE_GRACE,
            run_dest: String::new(),
            scan: SCANNERS.0,
            probe_scan: SCANNERS.1,
            ejecting: None,
            eject_fn: crate::engine::eject_source,
            presence: None,
            presence_at: None,
            presence_every: PRESENCE_EVERY,
            presence_fn: PRESENCE,
            update_check: None,
            update_fn: crate::settings::check_for_update,
            reported_bad: 0,
        };
        app.say(
            LogKind::Result,
            &crate::strings::fmt("gui.log.ready", &[("version", env!("CARGO_PKG_VERSION"))]),
        );
        app
    }

    // Newest lines kept on screen: unbounded growth during a multi-hour rip both eats memory
    // and slows every tick, since shells re-read the WHOLE buffer each time.
    const LOG_MAX: usize = 5_000;
    /// Dropped per trim, so a long rip pays the O(n) drain rarely instead of
    /// once per line.
    const LOG_TRIM: usize = 1_000;

    fn clear_log(&mut self) {
        self.log_first += self.log.len() as u64;
        self.log = Arc::default();
    }

    pub fn say(&mut self, kind: LogKind, text: &str) {
        let log = Arc::make_mut(&mut self.log);
        log.push(LogLine {
            text: text.into(),
            kind,
        });
        if log.len() > Self::LOG_MAX {
            // Oldest first: the tail is where a failure surfaces. No "elided" notice
            // line, since `gui.log.elided` doesn't exist and i18n is pinned to a
            // release tag — the alternative is one untranslated string. Debt recorded.
            log.drain(..Self::LOG_TRIM);
            self.log_first += Self::LOG_TRIM as u64;
        }
    }

    /// Which codec-gated containers could hold at least one title: with no codec information
    /// (an unscanned or container source) every one: the UI must not hide an option on a guess.
    pub fn fit(&self) -> Fit {
        let known: Vec<&String> = self.video_codecs.iter().filter(|c| !c.is_empty()).collect();
        let any = |allowed: &[&str]| {
            known.is_empty() || known.iter().any(|c| allowed.contains(&c.as_str()))
        };
        Fit {
            mp4: any(MP4_VIDEO),
            mpg: any(MPG_VIDEO),
        }
    }

    /// The output formats this source can actually produce.
    pub fn offered_formats(&self) -> Vec<Vec<&'static str>> {
        output_formats(!is_container(&self.source), self.fit())
    }

    /// The format this rip will ACTUALLY use.
    ///
    /// `self.format` is the user's standing preference and outlives the source it was chosen
    /// for — `open()` deliberately does not reset it. But a new source may not offer it, so
    /// every consumer (the view, the progress captions, the output filename, the `RipRequest`)
    /// reads the format through here rather than the raw preference, giving one answer instead
    /// of one per caller.
    pub fn effective_format(&self) -> String {
        let offered = self.offered_formats();
        reconcile_format(&self.format, &offered).to_string()
    }

    /// Why the current format cannot hold the ticked titles, if it cannot.
    ///
    /// Answered from the scan, before any rip: a container that will certainly
    /// fail should say so while the user can still change it.
    pub fn container_mismatch(&self) -> Option<String> {
        let format = self.effective_format();
        let (container, allowed) = if format.contains("MP4") {
            ("MP4", MP4_VIDEO)
        } else if format.contains("MPG") {
            ("MPG", MPG_VIDEO)
        } else {
            return None;
        };
        let ticked = self.tree.ticked_titles();
        let mut bad: Vec<&str> = ticked
            .iter()
            .filter_map(|i| self.video_codecs.get(*i))
            .map(|c| c.as_str())
            .filter(|c| !c.is_empty() && !allowed.contains(c))
            .collect();
        bad.sort_unstable();
        bad.dedup();
        if bad.is_empty() {
            return None;
        }
        Some(crate::strings::fmt(
            "gui.log.container_mismatch",
            &[("container", container), ("codecs", &bad.join(" or "))],
        ))
    }

    pub fn running(&self) -> bool {
        self.run.is_some()
    }

    /// The single entry point for every user action, on every platform.
    pub fn dispatch(&mut self, cmd: Cmd) -> Vec<Effect> {
        if blocked_while_running(cmd)
            && (self.running() || self.ejecting.is_some() || (self.opening() && cmd != Cmd::Close))
        {
            return vec![];
        }
        match cmd {
            Cmd::Open => vec![Effect::PickSource],
            Cmd::SetOutput => vec![Effect::PickOutputDir],
            Cmd::Close => {
                self.opening = None;
                self.close_source();
                self.say(
                    LogKind::Result,
                    &crate::strings::get("gui.log.source_closed"),
                );
                vec![Effect::Redraw]
            }
            Cmd::Run => {
                // No presence check may overlap the rip's drive open.
                self.settle_presence();
                self.start_run()
            }
            Cmd::Cancel => {
                if let Some(st) = &self.run {
                    st.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.say(LogKind::Result, &crate::strings::get("gui.log.cancelling"));
                }
                vec![Effect::Redraw]
            }
            Cmd::Eject => self.start_eject(),
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
                self.clear_log();
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
                if self.update_check.is_some() {
                    return vec![Effect::Redraw];
                }
                // The check blocks on the network, so it runs off the UI thread.
                let check = self.update_fn;
                let (tx, rx) = std::sync::mpsc::channel();
                let spawned = std::thread::Builder::new()
                    .name("update-check".into())
                    .spawn(move || {
                        let _ = tx.send(check(env!("CARGO_PKG_VERSION")));
                    });
                if let Err(e) = spawned {
                    self.say(LogKind::Notice, &e.to_string());
                    return vec![Effect::Redraw];
                }
                self.update_check = Some(rx);
                vec![Effect::Redraw, Effect::StartTicking]
            }
            Cmd::SetFormat(f) => {
                self.format = f.to_string();
                if let Some(m) = self.container_mismatch() {
                    self.say(LogKind::Notice, &m);
                }
                vec![Effect::Redraw]
            }
            Cmd::Quit => {
                self.stop_opens();
                vec![Effect::Quit]
            }
        }
    }

    /// The drive URL to open. An explicit request (`announce_missing`) enumerates drives and
    /// logs what it found, or that there is none; the launch probe gets bare `disc://`, silently.
    pub fn disc_source(&mut self, announce_missing: bool) -> Option<String> {
        // A probe nobody asked for must not GUESS: bare `disc://` autodetects the
        // drive with media, vs. naming drives[0] and risking an empty tray. Not
        // enumerated here: `list_optical_drives` is a SCSI walk, left to the probe worker.
        if !announce_missing {
            return Some(PROBE_SOURCE.to_string());
        }
        let (kind, line, url) = disc_open_plan(&crate::engine::list_optical_drives());
        self.say(kind, &line);
        url
    }

    /// Open a source: scan it, rebuild the tree, report honestly on failure.
    pub fn open(&mut self, path: &str) -> Vec<Effect> {
        // The shells' drop handlers call this directly, past `dispatch`'s gate.
        if self.running() {
            return vec![];
        }
        if self.ejecting.is_some() {
            return vec![Effect::Redraw];
        }
        self.opening = None;
        if let Some(tok) = self.open_token.take() {
            tok.halt.cancel();
        }
        if let Some(fx) = self.wait_for_probe(path, false) {
            return fx;
        }
        self.stop_opens();
        self.probe = None;
        self.pending = None;
        let mut fx = self.open_inner(path, false);
        if self.watching_disc() {
            fx.push(Effect::StartTicking);
        }
        fx
    }

    /// Scan and preflight away from the UI thread; tick applies the result.
    #[cfg(any(target_os = "linux", test))]
    pub fn open_async(&mut self, path: &str) -> Vec<Effect> {
        if self.running() || self.opening() || self.ejecting.is_some() {
            return vec![];
        }
        if let Some(fx) = self.wait_for_probe(path, true) {
            return fx;
        }
        self.stop_opens();
        self.probe = None;
        self.spawn_open(path)
    }

    // Park a drive open behind an outstanding probe; `poll_probe` resumes it.
    fn wait_for_probe(&mut self, path: &str, background: bool) -> Option<Vec<Effect>> {
        let probe = self.probe.clone()?;
        if !crate::engine::is_disc_source(path) {
            return None;
        }
        // §4.3: an Open of another source "cancels the probe's open token and keeps the
        // `PendingOpen` pending"; "The UI shows 'waiting for the drive' meanwhile".
        if path != probe.path {
            probe.token.halt.cancel();
            self.say(
                LogKind::Detail,
                &crate::strings::get_or("stop.waiting_for_drive", "Waiting for the drive …"),
            );
        }
        self.pending = Some(PendingOpen {
            path: path.to_owned(),
            background,
        });
        Some(vec![Effect::Redraw, Effect::StartTicking])
    }

    // Cancel every open in flight: the source changing, the window closing, or Quit (§4.3).
    fn stop_opens(&mut self) {
        if let Some(tok) = self.open_token.take() {
            tok.halt.cancel();
        }
        if let Some(p) = &self.probe {
            p.token.halt.cancel();
        }
    }

    fn run_pending(&mut self, open: PendingOpen) -> Vec<Effect> {
        if open.background {
            self.spawn_open(&open.path)
        } else {
            self.open_inner(&open.path, false)
        }
    }

    fn spawn_open(&mut self, path: &str) -> Vec<Effect> {
        let path = path.to_owned();
        let keys = KeyConfig::from_settings(&self.settings);
        let scan = self.scan;
        let tok = OpenToken::default();
        if let Some(old) = self.open_token.replace(tok.clone()) {
            old.halt.cancel();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("open-source".into())
            .spawn(move || {
                let scanned = scan(&path, &keys, &tok);
                // The preflight reuses Open's key set: no second key request (KU §2.5).
                let seed = scanned.as_ref().ok().and_then(|sc| sc.keys.as_ref());
                let preflight = (scanned.is_ok()
                    && !is_container(&path)
                    && !crate::engine::is_disc_source(&path))
                .then(|| crate::engine::preflight_with_keys(&path, "/tmp", &[], seed));
                let _ = tx.send(OpenedSource {
                    path,
                    scanned,
                    preflight,
                });
            });
        match spawned {
            Ok(_) => self.opening = Some(rx),
            Err(e) => {
                self.say(
                    LogKind::Notice,
                    &crate::strings::fmt_or(
                        "gui.log.scan_not_started",
                        "Could not start scanning the source: {detail}",
                        &[("detail", &e.to_string())],
                    ),
                );
                return vec![Effect::Redraw];
            }
        }
        vec![Effect::Redraw, Effect::StartTicking]
    }

    // Forget the open source (Close, or after its disc was ejected).
    fn close_source(&mut self) {
        self.stop_opens();
        // The drive Open held for Start is released now, not at its idle timeout.
        crate::engine::release_held_drive();
        self.probe = None;
        self.pending = None;
        self.clear_source();
    }

    // Forget the open source's model without touching an open or probe that may be in flight.
    fn clear_source(&mut self) {
        self.tree = Tree::default();
        self.source.clear();
        self.disc_present = None;
        self.disc_label.clear();
        // The key set lives in memory for this source only (KU §2.1 invariant 5).
        self.seed = None;
        self.vid_retry = false;
        self.open_refusal = None;
        self.page = Page::Empty;
    }

    // Eject the open disc off the UI thread (SCSI can block for seconds);
    // `tick` collects the verdict.
    fn start_eject(&mut self) -> Vec<Effect> {
        if eject_action(&self.source) == EjectAction::NothingToEject {
            self.say(
                LogKind::Result,
                &crate::strings::get("gui.log.nothing_eject"),
            );
            return vec![Effect::Redraw];
        }
        self.say(
            LogKind::Detail,
            &crate::strings::get_or("gui.log.ejecting", "Ejecting the disc…"),
        );
        let (source, eject) = (self.source.clone(), self.eject_fn);
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("eject".into())
            .spawn(move || {
                let _ = tx.send(eject(&source));
            });
        if let Err(e) = spawned {
            self.say(LogKind::Notice, &e.to_string());
            return vec![Effect::Redraw];
        }
        self.ejecting = Some((self.source.clone(), rx));
        vec![Effect::Redraw, Effect::StartTicking]
    }

    // Log a finished update check's verdict.
    fn poll_update(&mut self) -> Vec<Effect> {
        let msg = match self.update_check.as_ref().map(|rx| rx.try_recv()) {
            None | Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return Vec::new(),
            Some(Ok(m)) => m,
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => crate::strings::get_or(
                "gui.log.update_worker_stopped",
                "Update check failed: worker stopped before returning a result",
            ),
        };
        self.update_check = None;
        self.say(LogKind::Result, &msg);
        vec![Effect::Redraw]
    }

    // Apply a finished eject: the tree now describes a disc that is gone.
    fn poll_eject(&mut self) -> Vec<Effect> {
        let verdict = match self.ejecting.as_ref().map(|(_, rx)| rx.try_recv()) {
            None | Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return Vec::new(),
            Some(Ok(v)) => v,
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => Err(crate::strings::get_or(
                "gui.log.eject_worker_stopped",
                "eject worker stopped before returning a result",
            )),
        };
        let ejected = self.ejecting.take().map(|(src, _)| src);
        match verdict {
            Ok(device) => {
                // Only the ejected disc is stale; never wipe a source opened since.
                if ejected.as_deref() == Some(self.source.as_str()) {
                    self.close_source();
                }
                self.say(
                    LogKind::Result,
                    &crate::strings::fmt_or(
                        "gui.log.ejected",
                        "Ejected the disc ({device}).",
                        &[("device", &device)],
                    ),
                );
            }
            Err(e) => self.say(
                LogKind::Notice,
                &crate::strings::fmt_or(
                    "gui.log.eject_failed",
                    "Eject failed: {error}",
                    &[("error", &e)],
                ),
            ),
        }
        vec![Effect::Redraw]
    }

    // The disc watch runs only for an idle open disc: never during a rip, open, probe or eject.
    fn watching_disc(&self) -> bool {
        crate::engine::is_disc_source(&self.source)
            && matches!(self.page, Page::Titles | Page::Result)
            && self.run.is_none()
            && self.probe.is_none()
            && !self.opening()
            && self.ejecting.is_none()
    }

    // Ask, off the UI thread, whether the open disc is still in its drive.
    fn spawn_presence(&mut self) {
        let (source, check) = (self.source.clone(), self.presence_fn);
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("disc-presence".into())
            .spawn(move || {
                let _ = tx.send(check(&source));
            });
        self.presence_at = Some(std::time::Instant::now());
        if spawned.is_ok() {
            self.presence = Some((self.source.clone(), rx));
        }
    }

    // Collect the watch's verdict, or start the next check once one is due.
    fn poll_presence(&mut self) -> Vec<Effect> {
        let verdict = match self.presence.as_ref().map(|(_, rx)| rx.try_recv()) {
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => return Vec::new(),
            Some(Ok(v)) => v,
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => None,
            None => {
                let due = self
                    .presence_at
                    .is_none_or(|t| t.elapsed() >= self.presence_every);
                if due && self.watching_disc() {
                    self.spawn_presence();
                }
                return Vec::new();
            }
        };
        let checked = self.presence.take().map(|(src, _)| src);
        self.apply_presence(checked, verdict)
    }

    // A verdict counts only for the disc it asked about, and only while that disc sits idle.
    fn apply_presence(&mut self, checked: Option<String>, verdict: Option<bool>) -> Vec<Effect> {
        if verdict != Some(false)
            || checked.as_deref() != Some(self.source.as_str())
            || !self.watching_disc()
        {
            return Vec::new();
        }
        // The disc left its drive. A rip's Result stays up until Done, which then has no
        // tree to return to: the end-of-rip eject must not hide the outcome.
        let page = self.page;
        self.close_source();
        self.disc_present = Some(false);
        if page == Page::Result {
            self.page = Page::Result;
        }
        vec![Effect::Redraw]
    }

    // Done and Start act on the disc's presence now, not on the watch's next verdict. A
    // verdict that is not in within PRESENCE_SETTLE is left to the tick.
    fn settle_presence(&mut self) {
        if !self.watching_disc() {
            return;
        }
        if self.presence.is_none() {
            self.spawn_presence();
        }
        let verdict = match self
            .presence
            .as_ref()
            .map(|(_, rx)| rx.recv_timeout(PRESENCE_SETTLE))
        {
            Some(Ok(v)) => v,
            Some(Err(std::sync::mpsc::RecvTimeoutError::Timeout)) | None => return,
            Some(Err(std::sync::mpsc::RecvTimeoutError::Disconnected)) => None,
        };
        let checked = self.presence.take().map(|(src, _)| src);
        self.apply_presence(checked, verdict);
    }

    pub fn opening(&self) -> bool {
        self.opening.is_some() || self.pending.is_some()
    }

    /// The selection bar's title choice: one of [`PICK_TITLES`]' modes.
    pub fn pick_titles(&mut self, mode: &str) -> Vec<Effect> {
        self.pick_mode = mode.to_string();
        self.repick()
    }

    /// Toggle one audio language in the selection bar; `None` is "All" (no narrowing).
    pub fn pick_audio(&mut self, code: Option<&str>) -> Vec<Effect> {
        match code {
            None => self.pick_prefs.audio.clear(),
            Some(c) => toggle_code(&mut self.pick_prefs.audio, c),
        }
        self.settings.audio_langs = lang_selection_to_string(&self.pick_prefs.audio);
        self.repick()
    }

    /// The selection bar's subtitle choice.
    pub fn pick_subtitles(&mut self, choice: SubPick) -> Vec<Effect> {
        let p = &mut self.pick_prefs;
        match choice {
            SubPick::All => {
                (p.subtitles, p.forced) = (Vec::new(), Vec::new());
                (p.no_subtitles, p.no_forced) = (false, false);
            }
            SubPick::None => {
                (p.subtitles, p.forced) = (Vec::new(), Vec::new());
                (p.no_subtitles, p.no_forced) = (true, true);
            }
            SubPick::Forced => {
                (p.subtitles, p.forced) = (Vec::new(), Vec::new());
                (p.no_subtitles, p.no_forced) = (true, false);
            }
            SubPick::Lang(c) => {
                if p.no_subtitles {
                    (p.subtitles, p.forced) = (Vec::new(), Vec::new());
                }
                (p.no_subtitles, p.no_forced) = (false, false);
                toggle_code(&mut p.subtitles, &c);
                p.forced = p.subtitles.clone();
                if p.subtitles.is_empty() {
                    (p.no_subtitles, p.no_forced) = (true, true);
                }
            }
        }
        self.settings.subtitle_mode = match (p.no_subtitles, p.no_forced) {
            (true, true) => "none",
            (true, false) => "forced",
            _ => "all",
        }
        .into();
        self.settings.sub_langs = lang_selection_to_string(&p.subtitles);
        self.settings.forced_sub_langs = lang_selection_to_string(&p.forced);
        self.repick()
    }

    /// Persist the stream choices made in the title bar. Title mode is not
    /// included: it is disc-driven and may legitimately change between discs.
    pub fn save_pick_preferences(&mut self) -> Vec<Effect> {
        if !self.settings.persist_stream_preferences {
            return vec![];
        }
        match self.settings.save() {
            Ok(()) => vec![],
            Err(e) => {
                self.say(
                    LogKind::Notice,
                    &crate::strings::fmt_or(
                        "gui.log.settings_save_error",
                        "Could not save settings: {e}",
                        &[("e", &e)],
                    ),
                );
                vec![Effect::Redraw]
            }
        }
    }

    // Re-tick the tree from the scan it was built from, under the bar's current choices.
    fn repick(&mut self) -> Vec<Effect> {
        if self.running() {
            return vec![];
        }
        if let Some(sc) = &self.pick_scan {
            self.tree = Tree::from_scan(sc, &self.pick_mode, self.pick_min_secs, &self.pick_prefs);
        }
        vec![Effect::Redraw]
    }

    /// Open something NOBODY asked to open, leaving no trace if it is not
    /// there. Used by the launch probe: a disc already in the drive should
    /// just appear, an empty tray should look like before the probe existed.
    ///
    /// OFF THE UI THREAD, unlike [`App::open`].
    pub fn open_probe(&mut self, path: &str) -> Vec<Effect> {
        // A second probe cannot help and could clobber the first one's result.
        if self.probe.is_some() || self.opening() || self.running() || self.ejecting.is_some() {
            return vec![Effect::Redraw];
        }
        let state = Arc::new(ProbeState::new(path, self.probe_window));
        // Everything the scan needs is copied out HERE, on the UI thread. The
        // worker gets no reference to `App`.
        let keys = KeyConfig::from_settings(&self.settings);
        let path = path.to_string();
        let scan = self.probe_scan;
        let worker = state.clone();
        let spawned = std::thread::Builder::new()
            .name("launch-probe".into())
            .spawn(move || {
                let scanned = scan(&path, &keys, &worker.token);
                if let Ok(mut slot) = worker.result.lock() {
                    *slot = Some(scanned);
                }
                worker.done.store(true, Ordering::Release);
            });
        if spawned.is_err() {
            // Out of threads: the probe is optional, so drop it silently
            // rather than falling back to blocking the UI thread with it.
            return vec![Effect::Redraw];
        }
        self.probe = Some(state);
        vec![Effect::Redraw, Effect::StartTicking]
    }

    fn open_inner(&mut self, path: &str, quiet: bool) -> Vec<Effect> {
        let tok = OpenToken::default();
        let scanned = (self.scan)(path, &KeyConfig::from_settings(&self.settings), &tok);
        self.apply_scan(path, scanned, quiet)
    }

    // Turn a finished scan into model state. Split out of App::open_inner so
    // the launch probe's async result lands through the same code as a sync
    // open, rather than a second, drift-prone definition.
    fn apply_scan(
        &mut self,
        path: &str,
        scanned: Result<Scanned, String>,
        quiet: bool,
    ) -> Vec<Effect> {
        self.apply_opened(path, scanned, quiet, None)
    }

    fn apply_opened(
        &mut self,
        path: &str,
        scanned: Result<Scanned, String>,
        quiet: bool,
        preflight: Option<Result<Vec<String>, String>>,
    ) -> Vec<Effect> {
        let container = is_container(path);
        let disc = crate::engine::is_disc_source(path);
        match scanned {
            Ok(sc) => {
                if disc {
                    self.disc_present = Some(true);
                }
                self.clear_log();
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
                self.title_sizes = sc.title_sizes.clone();
                self.capacity_bytes = sc.capacity_bytes;
                // What the tree's title numbers refer to, kept so the request
                // can carry it to the engine's own (later) scan.
                self.title_ids = sc.title_ids.clone();
                self.disc_label = sc.volume_id.clone();
                // KU §2.5: Open's key set seeds the rip; E7034 arms the insert-the-disc Retry
                // (KU §4.2), so Start scans a drive instead of asking again without the VID.
                self.seed = sc.keys.clone();
                self.vid_retry = sc.needs_disc;
                let keys =
                    crate::engine::KeySnapshot::of(&KeyConfig::from_settings(&self.settings));
                self.open_refusal = sc.refusal.clone().map(|r| (r, keys));
                if sc.needs_disc {
                    self.say(LogKind::Notice, &crate::engine::insert_disc_retry());
                }
                let min_secs = self
                    .settings
                    .min_title_secs
                    .trim()
                    .parse::<f64>()
                    .unwrap_or(0.0);
                self.pick_mode = self.settings.selection.clone();
                self.pick_prefs = LangPrefs::from_settings(&self.settings);
                self.pick_min_secs = min_secs;
                self.tree = Tree::from_scan(&sc, &self.pick_mode, min_secs, &self.pick_prefs);
                self.pick_scan = Some(sc.clone());
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
                    match preflight.unwrap_or_else(|| {
                        crate::engine::preflight_with_keys(path, "/tmp", &[], sc.keys.as_ref())
                    }) {
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
                // An empty tray is the ordinary state at launch, not a rare error —
                // enumerating drives says nothing about loaded media. Announcing it
                // put an error on screen every startup, worse than the silence it replaced.
                if !quiet {
                    self.say(LogKind::Notice, &e);
                }
                // The page shows no source, so none may remain to Start or Eject.
                self.clear_source();
            }
        }
        vec![Effect::Redraw]
    }

    // Open's answered refusal, when asking again could only repeat it (KU §2.1 invariant 4):
    // the same key settings and keydb, titles within Open's scope, not transport-class.
    fn refused_again(&self, titles: &[usize]) -> Option<String> {
        let (refusal, keys) = self.open_refusal.as_ref()?;
        let now = crate::engine::KeySnapshot::of(&KeyConfig::from_settings(&self.settings));
        let within = titles.iter().all(|t| refusal.titles.contains(t));
        (!refusal.transport && *keys == now && within).then(|| refusal.text.clone())
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
        // A number the engine would read as 0 ("single pass", "abort on any loss") is not
        // what the user typed; say so instead of starting a rip under a different rule.
        // Named by the Settings row's own label, minus its trailing colon.
        let bad = [
            (
                crate::strings::get_or("gui.set.max_passes", "Max recovery passes :"),
                &self.settings.max_passes,
                self.settings.max_passes.trim().parse::<u32>().is_ok(),
            ),
            (
                crate::strings::get_or("gui.set.abort_lost", "Abort on lost seconds :"),
                &self.settings.abort_lost_secs,
                self.settings.abort_lost_secs.trim().parse::<u64>().is_ok(),
            ),
        ]
        .into_iter()
        .find(|(_, value, ok)| !value.trim().is_empty() && !ok)
        .map(|(label, value, _)| (label, value.escape_debug().to_string()));
        if let Some((label, value)) = bad {
            let name = label
                .trim_end()
                .trim_end_matches([':', '：'])
                .trim_end_matches(char::is_whitespace);
            self.say(
                LogKind::Notice,
                &crate::strings::fmt_or(
                    "gui.log.bad_number_setting",
                    "“{name}” must be a whole number, not “{value}”. Fix it in Settings, then start again.",
                    &[("name", name), ("value", &value)],
                ),
            );
            return vec![Effect::Redraw];
        }
        let titles = self.tree.ticked_titles();
        // A disc/ISO scan has title rows; if all are unchecked, refuse rather than
        // silently ripping the main title (the engine maps empty to main movie).
        // A container source has no title rows, so this guard never fires there.
        if self.tree.title_count() > 0 && titles.is_empty() {
            self.say(
                LogKind::Notice,
                &crate::strings::get("gui.log.select_title_first"),
            );
            return vec![Effect::Redraw];
        }
        if let Some(text) = self.refused_again(&titles) {
            self.say(LogKind::Notice, &text);
            return vec![Effect::Redraw];
        }
        let (audio_pids, sub_pids, explicit_streams) = self.tree.ticked_streams();
        let title_pids = self.tree.ticked_streams_by_title();
        // The user narrowed the tracks down to nothing (every audio AND subtitle
        // unchecked): allowed — some want a video-only extract — but never
        // silently. Surface it so an accidental result is caught before the rip.
        if is_video_only_selection(explicit_streams, &audio_pids, &sub_pids) {
            self.say(
                LogKind::Notice,
                &crate::strings::get("gui.log.video_only_warning"),
            );
        }
        // Re-check MP4/codec mismatch NOW, not just at format-pick time: the user
        // may have ticked an MPEG-2/VC-1 title afterward. Better an up-front
        // notice than a late per-title mux failure.
        if let Some(msg) = self.container_mismatch() {
            self.say(LogKind::Notice, &msg);
        }
        // `--raw` (keep-encrypted) only makes sense for "Whole disc → ISO image";
        // any mux would write ciphertext into the container. Mirror the CLI's
        // iso-only rule instead of silently forwarding it.
        let iso_output = self.effective_format().contains("ISO image");
        let drive_source = crate::engine::is_disc_source(&self.source);
        let raw = raw_applies(self.settings.raw, iso_output, drive_source);
        if self.settings.raw && !iso_output {
            self.say(
                LogKind::Notice,
                &crate::strings::get("gui.log.raw_iso_only"),
            );
        } else if self.settings.raw && !drive_source {
            self.say(
                LogKind::Notice,
                &crate::strings::get_or(
                    "gui.log.raw_disc_only",
                    "“Keep encrypted (raw)” applies only to a disc in a drive; ignoring it for this source.",
                ),
            );
        }
        // The Retry after E7034 reads the disc's Volume ID from a drive (KU §4.2 Q4).
        let vid_from = if self.vid_retry && !drive_source {
            match self.disc_source(true) {
                Some(drive) => Some(drive),
                None => return vec![Effect::Redraw],
            }
        } else {
            None
        };
        let state = Arc::new(RunState::default());
        self.run = Some(state.clone());
        self.reported_bad = 0;
        self.run_titles = titles.len().max(1);
        self.run_started = Some(std::time::Instant::now());
        self.run_dest = self.output_dir.clone();
        // Name the file the way the engine will, so the row matches reality.
        let out_file = output_file_name(
            &self.source,
            &self.output_dir,
            &self.effective_format(),
            titles.first().copied(),
            &self.settings.filename_template,
            &self.disc_label,
        );
        let scanned = scanned_source_bytes(
            &self.effective_format(),
            &titles,
            &self.title_sizes,
            self.capacity_bytes,
        );
        let mut info = InfoRows::starting(&self.source, &out_file, scanned);
        info.free_space = self.free.get(&self.output_dir);
        // The rip uses that space up: the next ask measures it afresh.
        self.free.forget();
        self.info = Some(info);
        self.page = Page::Progress;
        self.say(
            LogKind::Result,
            &crate::strings::fmt("gui.log.starting_rip", &[("dir", &self.output_dir)]),
        );
        let typed_passes: u32 = self.settings.max_passes.trim().parse().unwrap_or(0);
        let multipass = wants_multipass(&self.settings.rip_mode, typed_passes);
        let max_passes = effective_max_passes(&self.settings.rip_mode, typed_passes);
        crate::engine::start_rip(
            RipRequest {
                source: self.source.clone(),
                dest_dir: self.output_dir.clone(),
                titles,
                // The numbers alone are not the selection: the engine re-scans
                // before it muxes, and these are what they meant on the scan
                // the user actually ticked.
                title_ids: self.title_ids.clone(),
                format: self.effective_format(),
                audio_pids,
                sub_pids,
                title_pids,
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
                multipass,
                max_passes,
                abort_lost_secs: self.settings.abort_lost_secs.trim().parse().unwrap_or(0),
                keep_iso: self.settings.keep_iso,
                auto_eject: self.settings.auto_eject,
                keys: KeyConfig::from_settings(&self.settings),
                seed: self.seed.clone(),
                vid_from,
            },
            state,
        );
        vec![Effect::Redraw, Effect::StartTicking]
    }

    // Collect the launch probe's result if it has finished. Runs on the UI
    // thread, from App::tick, and is where every model mutation the probe
    // implies happens — see ProbeState.
    fn poll_probe(&mut self) -> Vec<Effect> {
        let Some(p) = self.probe.clone() else {
            return Vec::new();
        };
        // `Acquire`, pairing with the worker's `Release` store: seeing `true`
        // guarantees the `result` write is visible.
        if !p.done.load(Ordering::Acquire) {
            // §4.3 T29: "disabled once a same-source `PendingOpen` has adopted the probe"; a
            // cancelled probe is waited on until its worker lets go of the drive, or abandoned.
            let adopted = self.pending.as_ref().is_some_and(|o| o.path == p.path);
            if !adopted {
                p.stalled();
            }
            if !p.abandoned() {
                return Vec::new();
            }
            self.probe = None;
            return match self.pending.take() {
                Some(open) => self.run_pending(open),
                None => Vec::new(),
            };
        }
        self.probe = None;
        let scanned = p.result.lock().ok().and_then(|mut r| r.take());
        let scanned = match (self.pending.take(), scanned) {
            // A probe its Stop ended has no result to adopt: the Open runs afresh.
            (Some(open), Some(Err(_))) if p.token.halt.is_cancelled() => {
                return self.run_pending(open);
            }
            // Adopted: the user asked for exactly this scan, so failures are reported.
            (Some(open), Some(scanned)) if open.path == p.path => {
                let scanned = scanned.map_err(|e| {
                    if e.is_empty() {
                        crate::strings::get_or(
                            "gui.log.no_drive",
                            "No optical drive found. Connect a Blu-ray/DVD drive with a disc.",
                        )
                    } else {
                        e
                    }
                });
                return self.apply_scan(&p.path, scanned, false);
            }
            (Some(open), _) => return self.run_pending(open),
            (None, Some(scanned)) => scanned,
            // The worker set `done` without a result (it panicked or the mutex was
            // poisoned). The probe is optional; drop it without announcing anything.
            (None, None) => return Vec::new(),
        };
        // The user did not wait for us. Anything they opened, or a rip they
        // started, outranks a probe nobody asked for — applying the result now
        // would replace the tree under them.
        if !self.source.is_empty() || self.run.is_some() {
            return Vec::new();
        }
        self.apply_scan(&p.path.clone(), scanned, true)
    }

    /// Poll a running job. Called on the shell's timer; returns the effects to
    /// apply. All progress arithmetic is the engine's — never recomputed here.
    pub fn tick(&mut self) -> Vec<Effect> {
        let mut probe_fx = self.poll_probe();
        probe_fx.extend(self.poll_eject());
        probe_fx.extend(self.poll_update());
        probe_fx.extend(self.poll_presence());
        if let Some(rx) = &self.opening {
            match rx.try_recv() {
                Ok(opened) => {
                    self.opening = None;
                    probe_fx.extend(self.apply_opened(
                        &opened.path,
                        opened.scanned,
                        false,
                        opened.preflight,
                    ));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.opening = None;
                    self.say(
                        LogKind::Notice,
                        &crate::strings::get_or(
                            "gui.log.scan_worker_stopped",
                            "Source scan worker stopped before returning a result.",
                        ),
                    );
                    probe_fx.push(Effect::Redraw);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        let Some(st) = self.run.clone() else {
            // Keep the timer alive while the probe is still out; stopping it
            // here would strand the result with nothing left to collect it.
            let mut fx = probe_fx;
            if self.probe.is_none()
                && !self.opening()
                && self.ejecting.is_none()
                && self.update_check.is_none()
            {
                // An idle disc keeps the tick for its watch, without a redraw per tick.
                if self.presence.is_none() && !self.watching_disc() {
                    fx.push(Effect::StopTicking);
                }
            } else if fx.is_empty() {
                fx.push(Effect::Redraw);
            }
            return fx;
        };
        // Poison-recovering, like the verdict/summary below: a panicked worker
        // poisons the log buffer, and `unwrap_or_default()` would throw away the
        // WHOLE diagnostic, including the lines explaining the panic.
        let lines: Vec<String> = st
            .lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        for l in lines {
            self.say(LogKind::Detail, &l);
        }
        let p = *st.prog.lock().unwrap_or_else(|e| e.into_inner());
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
        // `Acquire`, pairing with the worker's `Release` store in
        // `SignalDone::drop`: seeing `true` guarantees the `summary`/`outcome`
        // writes are visible below, without relying on lock semantics alone.
        if st.finished.load(std::sync::atomic::Ordering::Acquire) {
            // Poison-recovering: a worker that PANICKED is exactly when the
            // verdict matters, and `unwrap_or_default()` turned that into
            // `RunOutcome::Completed` plus an empty summary.
            let sum = st.summary_now();
            self.vid_retry = st.needs_disc.load(std::sync::atomic::Ordering::SeqCst);
            self.say(LogKind::Result, &sum);
            self.result_summary = sum;
            self.result_outcome = st.outcome_now();
            self.run = None;
            self.page = Page::Result;
            // The rip may have ejected its disc: the watch asks on the next tick.
            self.presence_at = None;
            let mut fx = vec![Effect::Redraw];
            if self.update_check.is_none() && !self.watching_disc() {
                fx.push(Effect::StopTicking);
            }
            if self.settings.notify_when_rip_finished {
                let completed = self.result_outcome == crate::engine::RunOutcome::Completed;
                fx.push(Effect::NotifyRipFinished {
                    title: if completed {
                        crate::strings::get_or("gui.notify.rip_finished_title", "Rip finished")
                    } else {
                        result_heading(self.result_outcome)
                    },
                    body: self.result_summary.clone(),
                    output_dir: completed.then(|| self.run_dest.clone()),
                });
            }
            return fx;
        }
        vec![Effect::Redraw]
    }

    pub fn dismiss_result(&mut self) -> Vec<Effect> {
        self.settle_presence();
        self.page = if self.tree.arena.is_empty() {
            Page::Empty
        } else {
            Page::Titles
        };
        vec![Effect::Redraw]
    }

    /// Everything a shell needs to draw the current state.
    fn pick_view(&self) -> Option<PickView> {
        let sc = self.pick_scan.as_ref()?;
        let langs = |kind: &str| {
            let mut out: Vec<String> = Vec::new();
            for r in sc.rows.iter().filter(|r| r.type_s == kind) {
                let code = stream_language(&r.lang);
                if !code.is_empty() && !out.contains(&code) {
                    out.push(code);
                }
            }
            out
        };
        let p = &self.pick_prefs;
        let ticked = |list: &[String], code: &str| list.iter().any(|c| same_language(c, code));
        let audio: Vec<(String, bool)> = langs("Audio")
            .into_iter()
            .map(|c| {
                let on = ticked(&p.audio, &c);
                (c, on)
            })
            .collect();
        let subtitles: Vec<(String, bool)> = langs("Subtitles")
            .into_iter()
            .map(|c| {
                let on = !p.no_subtitles && ticked(&p.subtitles, &c);
                (c, on)
            })
            .collect();
        let has_episodes = !sc
            .selection_model
            .select_with_preferences(
                &freemkv_engine::Selection::Episodes,
                &lang_filter(&p.audio),
                &p.selection_preferences(),
            )
            .indices
            .is_empty();
        let titles = PICK_TITLES
            .iter()
            .filter(|mode| **mode != "Episodes" || has_episodes)
            .map(|mode| (*mode, pick_title_label(mode)))
            .collect();
        let all = crate::strings::get_or("gui.pick.all", "All");
        let none = crate::strings::get_or("gui.pick.none", "None");
        let forced = crate::strings::get_or("gui.pick.forced", "Forced only");
        let audio_all = p.audio.is_empty();
        let subs_none = p.no_subtitles && p.no_forced;
        let subs_forced = p.no_subtitles && !p.no_forced;
        let subs_all = !p.no_subtitles && p.subtitles.is_empty() && !p.no_forced;
        let picked = |v: &[(String, bool)]| {
            v.iter()
                .filter(|(_, on)| *on)
                .map(|(c, _)| c.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        Some(PickView {
            titles,
            title: self.pick_mode.clone(),
            audio_summary: if audio_all {
                all.clone()
            } else {
                picked(&audio)
            },
            subs_summary: if subs_none {
                none
            } else if subs_forced {
                forced
            } else if subs_all {
                all
            } else {
                picked(&subtitles)
            },
            audio,
            subtitles,
            audio_all,
            subs_all,
            subs_none,
            subs_forced,
        })
    }

    pub fn view(&self) -> View {
        let p = self
            .run
            .as_ref()
            .map(|st| *st.prog.lock().unwrap_or_else(|e| e.into_inner()))
            .unwrap_or_default();
        // Top bar: the current title; bottom bar: the whole run (a single title's mirrors it).
        let pct = p.title_pct;
        let overall = p.batch_pct.unwrap_or(pct);
        let elapsed = self.run_started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let title_elapsed = p.title_started.map_or(elapsed, |t| t.elapsed().as_secs());
        let titles_done = self
            .run
            .as_ref()
            .map(|st| st.titles_done.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0);
        let stopping = self
            .run
            .as_ref()
            .is_some_and(|st| st.cancel.load(Ordering::Relaxed));
        View {
            page: self.page,
            disc_present: self.disc_present,
            title_rows: self.rows(),
            pick: self.pick_view(),
            info: self
                .info
                .as_ref()
                .map(|i| i.as_array().map(|s| s.to_string())),
            bar_current: pct,
            bar_overall: overall,
            caption_current: bar_caption(pct, title_elapsed, p.eta_secs),
            caption_overall: bar_caption(overall, elapsed, None),
            show_overall_bar: self.run_titles > 1,
            saving_current: stop_caption(stopping, titles_done, self.run_titles).unwrap_or_else(
                || {
                    crate::strings::fmt(
                        "gui.progress.saving_current",
                        &[("container", &container_label(&self.effective_format()))],
                    )
                },
            ),
            saving_overall: stop_caption(stopping, titles_done, self.run_titles).unwrap_or_else(
                || {
                    crate::strings::fmt(
                        "gui.progress.saving_overall",
                        &[("container", &container_label(&self.effective_format()))],
                    )
                },
            ),
            output_dir: self.output_dir.clone(),
            free_space_line: free_space_line(&self.free.get(&self.output_dir)),
            format: self.effective_format(),
            formats: self.offered_formats(),
            can_run: !self.running() && !self.opening() && !self.source.is_empty(),
            log: Arc::clone(&self.log),
            log_first: self.log_first,
            log_hidden: self.log_hidden,
            log_menu_label: log_menu_label(self.log_hidden),
            detail: self
                .selected_row
                .and_then(|i| self.tree.arena.get(i))
                .map(|n| n.info.clone())
                .unwrap_or_else(|| crate::strings::get("gui.page.detail_default")),
            result_summary: self.result_summary.clone(),
            // Summary text is engine-emitted English; heading is localized, matched
            // on the TYPED verdict — substring-matching the summary text used to send
            // an undecryptable disc and abort-for-loss paths to the success heading.
            result_heading: result_heading(self.result_outcome),
            eject_visible: eject_action(&self.source) == EjectAction::Eject
                && !self.running()
                && self.ejecting.is_none(),
        }
    }

    fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        for (i, n) in self.tree.arena.iter().enumerate() {
            let depth = if self.tree.roots.contains(&i) {
                0
            } else if n.type_s == "Title" {
                1
            } else if n.type_s == "Chapter" {
                3
            } else {
                2
            };
            out.push(Row {
                index: i,
                depth,
                type_s: n.type_s.clone(),
                desc: n.desc.clone(),
                length: n.length.clone(),
                size: n.size.clone(),
                lang: n.lang.clone(),
                item: n.item.clone(),
                format: n.format.clone(),
                notes: n.notes.clone(),
                check: if n.checkable() {
                    Some(self.tree.check_state(i))
                } else {
                    n.mirrors().map(|base| self.tree.check_state(base))
                },
                check_enabled: n.checkable(),
            });
        }
        out
    }
}

// The window closing cancels every open in flight (stop design v5 §4.3).
impl Drop for App {
    fn drop(&mut self) {
        self.stop_opens();
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

/// The selection bar as the shells draw it.
#[derive(Clone, Debug, PartialEq)]
pub struct PickView {
    /// The title choices this disc offers, (mode, label), and the chosen mode.
    pub titles: Vec<(&'static str, String)>,
    pub title: String,
    /// Every audio / subtitle language on the disc as (code, ticked), in disc order.
    pub audio: Vec<(String, bool)>,
    pub subtitles: Vec<(String, bool)>,
    /// Whether "All" is in force: no audio narrowing / every subtitle kept.
    pub audio_all: bool,
    pub subs_all: bool,
    pub subs_none: bool,
    pub subs_forced: bool,
    /// What each dropdown shows closed: "eng, deu", "All", "None".
    pub audio_summary: String,
    pub subs_summary: String,
}

/// One column of the title tree, after the shell's own tick column.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// What [`Row::cell`] takes.
    pub id: &'static str,
    /// The header, in the active locale.
    pub title: String,
    /// Starting width in points.
    pub width: f64,
    /// Right-aligned (lengths and sizes).
    pub numeric: bool,
    /// Takes up any change in the tree's width; the others keep theirs.
    pub flex: bool,
    /// The narrowest it may be squeezed to when the tree is short of width.
    pub min: f64,
}

/// The title tree's columns, the same on every shell.
pub fn tree_columns() -> Vec<Column> {
    let col = |id, title: String, width, min, numeric, flex| Column {
        id,
        title,
        width,
        numeric,
        flex,
        min,
    };
    vec![
        col(
            "item",
            crate::strings::get_or("gui.col.item", "Item"),
            190.0,
            110.0,
            false,
            false,
        ),
        col(
            "lang",
            crate::strings::get_or("gui.col.lang", "Language"),
            76.0,
            76.0,
            false,
            false,
        ),
        col(
            "format",
            crate::strings::get("disc.format"),
            260.0,
            150.0,
            false,
            false,
        ),
        col(
            "notes",
            crate::strings::get_or("gui.col.notes", "Notes"),
            240.0,
            120.0,
            false,
            true,
        ),
        col(
            "length",
            crate::strings::get_or("gui.col.duration", "Length"),
            66.0,
            66.0,
            true,
            false,
        ),
        col(
            "size",
            crate::strings::get_or("gui.col.size", "Size"),
            66.0,
            66.0,
            true,
            false,
        ),
    ]
}

/// Fit the columns into `avail` points, each starting at its width in `widths`: the flexible
/// one takes what the others leave and, short of room, the text columns give way together, in
/// proportion, down to their minimums. Lengths and sizes have no give, so they always read.
pub fn fit_column_widths(cols: &[Column], widths: &[f64], avail: f64) -> Vec<f64> {
    let mut w: Vec<f64> = cols
        .iter()
        .zip(widths)
        .map(|(c, &v)| v.max(c.min))
        .collect();
    if let Some(f) = cols.iter().position(|c| c.flex) {
        let others: f64 = w
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != f)
            .map(|(_, v)| v)
            .sum();
        w[f] = (avail - others).max(cols[f].min);
    }
    let over = w.iter().sum::<f64>() - avail;
    let give: f64 = cols.iter().zip(&w).map(|(c, v)| v - c.min).sum();
    if over > 0.0 && give > 0.0 {
        let k = (over / give).min(1.0);
        for (c, v) in cols.iter().zip(w.iter_mut()) {
            *v -= (*v - c.min) * k;
        }
    }
    w
}

impl Row {
    /// The text of this row's cell in column `id` ([`tree_columns`]).
    pub fn cell(&self, id: &str) -> &str {
        match id {
            "item" => &self.item,
            "lang" => &self.lang,
            "format" => &self.format,
            "length" => &self.length,
            "size" => &self.size,
            _ => &self.notes,
        }
    }
}

/// One entry of a selection-bar menu: a fixed choice or a language, ticked or not.
#[derive(Clone, Debug, PartialEq)]
pub struct PickEntry {
    pub label: String,
    /// What [`PickView::audio_choice`] / [`PickView::subs_choice`] take back.
    pub tag: isize,
    pub on: bool,
    /// Draw a separator above it (the first language after the fixed choices).
    pub separator_before: bool,
}

// Menu tags: the fixed choices, then each language at LANG_TAG + its index.
const ALL_TAG: isize = 1;
const NONE_TAG: isize = 2;
const FORCED_TAG: isize = 3;
const LANG_TAG: isize = 100;

/// The selection bar's three labels: Titles, Audio, Subtitles.
pub fn pick_labels() -> [String; 3] {
    [
        crate::strings::get_or("gui.pick.titles", "Titles"),
        crate::strings::get_or("gui.pick.audio", "Audio"),
        crate::strings::get_or("gui.pick.subtitles", "Subtitles"),
    ]
}

impl PickView {
    fn menu(fixed: Vec<(String, isize, bool)>, langs: &[(String, bool)]) -> Vec<PickEntry> {
        let mut out: Vec<PickEntry> = fixed
            .into_iter()
            .map(|(label, tag, on)| PickEntry {
                label,
                tag,
                on,
                separator_before: false,
            })
            .collect();
        for (k, (code, on)) in langs.iter().enumerate() {
            out.push(PickEntry {
                label: code.clone(),
                tag: LANG_TAG + k as isize,
                on: *on,
                separator_before: k == 0,
            });
        }
        out
    }

    /// The Audio menu: All, then each language on the disc.
    pub fn audio_menu(&self) -> Vec<PickEntry> {
        let all = crate::strings::get_or("gui.pick.all", "All");
        Self::menu(vec![(all, ALL_TAG, self.audio_all)], &self.audio)
    }

    /// The Subtitles menu: All, None, Forced only, then each language on the disc.
    pub fn subs_menu(&self) -> Vec<PickEntry> {
        let all = crate::strings::get_or("gui.pick.all", "All");
        let none = crate::strings::get_or("gui.pick.none", "None");
        let forced = crate::strings::get_or("gui.pick.forced", "Forced only");
        Self::menu(
            vec![
                (all, ALL_TAG, self.subs_all),
                (none, NONE_TAG, self.subs_none),
                (forced, FORCED_TAG, self.subs_forced),
            ],
            &self.subtitles,
        )
    }

    /// What a picked Audio entry means for [`App::pick_audio`]: `Some(None)` is All.
    pub fn audio_choice(&self, tag: isize) -> Option<Option<String>> {
        match tag {
            ALL_TAG => Some(None),
            t => usize::try_from(t - LANG_TAG)
                .ok()
                .and_then(|k| self.audio.get(k))
                .map(|(c, _)| Some(c.clone())),
        }
    }

    /// What a picked Subtitles entry means for [`App::pick_subtitles`].
    pub fn subs_choice(&self, tag: isize) -> Option<SubPick> {
        match tag {
            ALL_TAG => Some(SubPick::All),
            NONE_TAG => Some(SubPick::None),
            FORCED_TAG => Some(SubPick::Forced),
            t => usize::try_from(t - LANG_TAG)
                .ok()
                .and_then(|k| self.subtitles.get(k))
                .map(|(c, _)| SubPick::Lang(c.clone())),
        }
    }

    /// The mode of the Titles entry at `index` (its position in [`PickView::titles`]).
    pub fn title_choice(&self, index: usize) -> Option<&'static str> {
        self.titles.get(index).map(|t| t.0)
    }

    /// The Titles entry to show as chosen.
    pub fn title_index(&self) -> usize {
        self.titles
            .iter()
            .position(|t| t.0 == self.title)
            .unwrap_or(0)
    }
}

/// Free space at a folder, measured off the UI thread: a statvfs on a stale network folder can
/// hang for minutes. One measurement runs at a time; the latest folder asked for meanwhile is
/// measured next. Until a folder's answer is in it reads as unknown ("—").
#[derive(Clone, Default)]
pub struct FreeSpace(Arc<Mutex<FreeState>>);

#[derive(Default)]
struct FreeState {
    /// The last folder measured, and its answer.
    known: Option<(String, String)>,
    busy: bool,
    next: Option<String>,
}

impl FreeSpace {
    /// The free space at `dir` as text, without blocking.
    pub fn get(&self, dir: &str) -> String {
        self.get_with(dir, free_space)
    }

    fn get_with(&self, dir: &str, measure: fn(&str) -> String) -> String {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((d, text)) = &s.known
            && d == dir
        {
            return text.clone();
        }
        if s.busy {
            s.next = Some(dir.to_string());
        } else {
            s.busy = true;
            let state = self.0.clone();
            let first = dir.to_string();
            let spawned = std::thread::Builder::new()
                .name("free-space".into())
                .spawn(move || measure_until_current(&state, first, measure));
            s.busy = spawned.is_ok();
        }
        "—".into()
    }

    /// Drop the answer, so the next ask measures afresh (a rip changes it).
    pub fn forget(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).known = None;
    }
}

// Measure `dir`, then whatever folder was asked for meanwhile, until none is waiting.
fn measure_until_current(state: &Mutex<FreeState>, mut dir: String, measure: fn(&str) -> String) {
    loop {
        let text = measure(&dir);
        let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
        s.known = Some((dir.clone(), text));
        match s.next.take() {
            Some(n) if n != dir => dir = n,
            _ => {
                s.busy = false;
                return;
            }
        }
    }
}

/// The line under the output row: `free` is the free space at the chosen folder.
pub fn free_space_line(free: &str) -> String {
    format!("{} {free}", crate::strings::get("gui.info.free_space"))
}

/// One rendered tree row — already decided, nothing left to compute.
#[derive(Clone, Debug)]
pub struct Row {
    pub index: usize,
    pub depth: u8,
    pub type_s: String,
    pub desc: String,
    /// The Length cell (`"2:23:20"`); empty on every row but a title.
    pub length: String,
    /// The Size cell (`"6.8 GB"`); empty where the scan reports no size.
    pub size: String,
    /// The Language cell (`"deu"`); empty but on audio and subtitle rows.
    pub lang: String,
    /// The Item, Format and Notes cells ("Title 2", "MPEG-2 576i 25fps 16:9", "19 chapters").
    pub item: String,
    pub format: String,
    pub notes: String,
    /// `None` means the row carries no checkbox at all.
    pub check: Option<Check>,
    /// Whether a click on the box does anything. `false` for a mirror row
    /// ([`Node::mirrors`]): the box shows its base's tick and is drawn disabled.
    pub check_enabled: bool,
}

/// A row list's identity, excluding tick state (applied without a rebuild): every painted
/// cell, the shape and the count. Equal signatures let a shell skip reloading its tree.
/// Hashed in place, so a tick-only redraw allocates nothing.
pub fn rows_sig(rows: &[Row]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    rows.len().hash(&mut h);
    for r in rows {
        (r.index, r.depth, &r.type_s, &r.desc, &r.length, &r.size).hash(&mut h);
        (&r.item, &r.lang, &r.format, &r.notes).hash(&mut h);
    }
    h.finish()
}

/// Self-test: each Title row's Item names its 1-based `-t` number, the numbers ascend (a hidden
/// short title leaves a gap), and its description leads with that name.
pub fn titles_numbered(rows: &[Row]) -> bool {
    let mut last = 0;
    rows.iter().filter(|r| r.type_s == "Title").all(|r| {
        let digits: String = r.item.chars().filter(char::is_ascii_digit).collect();
        let Ok(n) = digits.parse::<usize>() else {
            return false;
        };
        let ok =
            n > last && r.item == crate::engine::title_item(n - 1) && r.desc.starts_with(&r.item);
        last = n;
        ok
    })
}

/// The Result page heading for a verdict, matched on the TYPED outcome.
pub(crate) fn result_heading(outcome: crate::engine::RunOutcome) -> String {
    match outcome {
        crate::engine::RunOutcome::Cancelled => crate::strings::get("gui.result.cancelled"),
        // Reuses "nothing written" instead of a new key: `freemkv-i18n` is
        // pinned to a release tag, so a new key means cutting a tag there.
        crate::engine::RunOutcome::Failed => crate::strings::get("gui.result.nothing"),
        crate::engine::RunOutcome::Completed => crate::strings::get("gui.result.finished"),
    }
}

/// Home-page copy for the tray state. Keep the undecided state identical to
/// the normal empty page so a slow launch probe does not flash a false answer.
// This is called by the platform shells, which are separate target-specific
// modules and therefore absent from the portable library build.
#[allow(dead_code)]
pub(crate) fn empty_heading(disc_present: Option<bool>) -> String {
    match disc_present {
        Some(true) => crate::strings::get_or("gui.page.disc_ready_title", "Disc inserted"),
        Some(false) => crate::strings::get("gui.page.empty_title"),
        None => crate::strings::get_or("gui.page.open_source_title", "Open a source"),
    }
}

#[allow(dead_code)]
pub(crate) fn empty_description(disc_present: Option<bool>) -> String {
    let base = crate::strings::get("gui.page.empty_subtitle");
    match disc_present {
        Some(true) => {
            crate::strings::get_or("gui.page.disc_ready", "Disc inserted — ready to open.")
        }
        Some(false) => format!(
            "{base}\n\n{}",
            crate::strings::get_or("gui.page.no_disc", "No disc detected.")
        ),
        None => base,
    }
}

/// A complete description of the screen. A shell assigns these to widgets and
/// makes no decisions of its own.
pub struct View {
    pub page: Page,
    /// Whether a live optical source is currently known to contain media.
    /// `None` means the launch probe has not answered yet.
    pub disc_present: Option<bool>,
    pub title_rows: Vec<Row>,
    /// The selection bar, while a source is open.
    pub pick: Option<PickView>,
    pub info: Option<[String; 7]>,
    pub bar_current: f64,
    pub bar_overall: f64,
    pub caption_current: String,
    pub caption_overall: String,
    /// "Saving to `<container>` file" — the per-title bar label, format-aware so
    /// it reads "MP4" when MP4 is chosen (never a hardcoded "MKV").
    pub saving_current: String,
    /// "Saving all titles to `<container>` files" — the overall-bar label.
    pub saving_overall: String,
    pub show_overall_bar: bool,
    pub output_dir: String,
    /// The line under the output row (free space there); "—" until measured.
    pub free_space_line: String,
    pub format: String,
    pub formats: Vec<Vec<&'static str>>,
    pub can_run: bool,
    pub log: Arc<Vec<LogLine>>,
    /// See [`App::log_first`].
    pub log_first: u64,
    pub log_hidden: bool,
    /// The View ▸ log menu item's label for the CURRENT state — see
    /// [`log_menu_label`]. Carried on the `View` so a shell only assigns it,
    /// exactly like every other piece of text on screen.
    pub log_menu_label: String,
    pub detail: String,
    pub result_summary: String,
    /// Heading for the result page — never "Finished" after a cancel.
    pub result_heading: String,
    pub eject_visible: bool,
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod tests;
