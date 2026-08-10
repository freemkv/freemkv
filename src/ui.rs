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
    pub audio: Vec<String>,
    pub subtitles: Vec<String>,
    /// Independent of `subtitles`, never a narrowing of it.
    pub forced: Vec<String>,
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
            audio: split_langs(audio),
            subtitles: split_langs(subtitles),
            forced: split_langs(forced),
        }
    }

    /// The preferences as persisted in Settings.
    pub fn from_settings(s: &crate::settings::Settings) -> Self {
        Self::parse(&s.audio_langs, &s.sub_langs, &s.forced_sub_langs)
    }

    /// No preference expressed at all — the tree is built exactly as before.
    pub fn is_empty(&self) -> bool {
        self.audio.is_empty() && self.subtitles.is_empty() && self.forced.is_empty()
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

/// Resolve one class's PIDs, falling back to "keep everything in this class"
/// when the preference matched nothing that IS there.
///
/// `all` is every PID of the class present on the title. `Only([])` from the
/// engine means the requested languages are not on this disc for this class —
/// the user's chosen rule is to fall back to today's behaviour for that
/// category rather than ship a file missing the whole track class.
fn class_or_fallback(f: freemkv_engine::PidFilter, all: &[u16]) -> Vec<u16> {
    match f {
        freemkv_engine::PidFilter::All => all.to_vec(),
        freemkv_engine::PidFilter::Only(p) if p.is_empty() => all.to_vec(),
        freemkv_engine::PidFilter::Only(p) => p,
    }
}

/// Which of ONE title's stream PIDs the preferences keep.
///
/// The language matching is `freemkv-engine`'s: the rows are folded back into a
/// synthetic `DiscTitle` and handed to `resolve_stream_selection_forced`, so the
/// GUI has no second language matcher to drift from the one the rip uses — name
/// and 639-1/2T/2B/3 forms all resolve exactly as `-a`/`-s` do on the CLI.
///
/// The three classes are resolved in three calls, one per set, so each falls
/// back independently: German audio missing must not drag the subtitle choice
/// down with it, and an unparseable tag in one box cannot disturb the other two.
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
    out.extend(match normal {
        Ok(sel) => class_or_fallback(sel.subtitle, &all_normal),
        Err(_) => all_normal.clone(),
    });

    // Forced subtitles, matched only against the `forced` set — its own class,
    // so "only German subtitles, forced only if in English" resolves both
    // halves on their own terms.
    //
    // This class does NOT fall back to keeping everything, unlike the two
    // above, and the difference is deliberate. The fallback exists so a
    // language that isn't on the disc cannot produce a file missing a whole
    // track class — a rip with no audio is broken. Neither half of that
    // reasoning holds here:
    //
    //   * A title with no forced subtitles is perfectly normal. They are an
    //     optional overlay for foreign dialogue, not a class every file needs.
    //   * Forced subtitles DISPLAY BY THEMSELVES during playback. Ticking ones
    //     the user did not ask for is not a harmless superset — it burns
    //     wrong-language text onto the screen. Asking for forced-only-in-
    //     English on a disc that has none, and being handed French, German,
    //     Spanish and Portuguese forced tracks that all auto-display, is worse
    //     than being handed nothing.
    //
    // So an unmatched preference keeps NONE here. An empty preference is
    // unaffected: it means "no preference", arrives as `PidFilter::All`, and
    // still keeps every forced track.
    let forced = resolve_stream_selection_forced(
        &title,
        &StreamFilter::None,
        &SubtitleFilter::split(StreamFilter::None, lang_filter(&prefs.forced)),
    );
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
    ///
    /// `prefs` are the user's preferred-language defaults: `sel_mode` decides
    /// which TITLES start checked, and `prefs` then narrows which of a checked
    /// title's STREAM rows start checked. Nothing else changes — the rip request
    /// is still built from the checkboxes, so every choice made here is visible
    /// in the tree and can be overridden by hand.
    ///
    /// An empty `prefs` ([`LangPrefs::default`]) is exactly the pre-preference
    /// behaviour (every stream of a checked title checked), and so is a category
    /// whose languages match nothing on this title — see [`preferred_pids`]. A
    /// disc must never rip silently without audio because the preferred language
    /// is not on it.
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

        // Which stream PIDs the language preferences keep, per canonical title
        // index. Computed only for the titles that start checked — an unchecked
        // title has no ticked streams to narrow — and only when there is a
        // preference at all, so the default build does no extra work.
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
                        // A stream row starts checked when its TITLE does — and,
                        // when the user has language preferences, only if this
                        // PID is one the preferences keep. A row with no PID
                        // (video) is never narrowed: it is not selectable and is
                        // always retained.
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

    /// What a CLICK on row `i`'s tick box does.
    ///
    /// This is a decision, so it lives here and not in a shell. Both shells
    /// carried a comment saying "the core owns cascade + tri-state; the shell
    /// only reports which row was clicked" while each in fact computed its own
    /// answer, and the two answers disagreed: Windows read `Off | Mixed` as
    /// "turn on", macOS read the NSButton's mixed state (`-1`) as "turn off".
    /// So clicking a partly-ticked title selected all of it on one platform and
    /// deselected all of it on the other.
    ///
    /// A partly-ticked title becomes fully ticked. Clicking again clears it —
    /// which is the only reading that makes a second click undo the first.
    pub fn toggle(&self, i: usize) {
        let on = matches!(self.check_state(i), Check::Off | Check::Mixed);
        self.set_checked(i, on);
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

// ── preferred-language pickers ──────────────────────────────────────────────
//
// The three preference boxes used to be free text: the user typed codes or
// names and hoped. That asked people to know that a German audio track is
// tagged `deu` and not `ger` or `de` — and a typo was indistinguishable from
// "this disc has no German", because both simply matched nothing.
//
// The shells now show a checklist of language NAMES and store ISO codes. This
// module owns the list and both directions of the conversion so macOS and
// Windows cannot drift; a shell only renders what it is given.

/// The languages offered in the pickers, as (stored code, English name).
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
        .or_else(|| isolang::Language::from_name(t))
        .map(|l| l.to_639_3().to_string())
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

/// The picker button's title: the chosen languages in English, or a word
/// meaning "no preference" — never an empty button, which reads as broken.
pub fn lang_summary(stored: &str) -> String {
    let codes = lang_selection(stored);
    if codes.is_empty() {
        return crate::strings::get("gui.set.lang_any");
    }
    codes
        .iter()
        .map(|c| lang_display_name(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The English name for a stored code, falling back to the code itself so an
/// unknown tag is still visible rather than blank.
pub fn lang_display_name(code: &str) -> String {
    PICKER_LANGUAGES
        .iter()
        .find(|(c, _)| c.eq_ignore_ascii_case(code))
        .map(|(_, name)| (*name).to_string())
        .or_else(|| {
            isolang::Language::from_639_3(&code.to_ascii_lowercase())
                .map(|l| l.to_name().to_string())
        })
        .unwrap_or_else(|| code.to_string())
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
/// DVD's MPEG-2, an HD DVD's VC-1). The option is then REMOVED rather than
/// offered-and-refused: a choice that always fails is worse than no choice.
/// Pass true when the codecs are unknown — never block on missing information.
///
/// `disc_source` is "not a container" — it is true for a physical disc AND for
/// an ISO file, because both carry a whole disc to unpack. That is why
/// "Whole disc → decrypted folder" (the CLI's `dir://`) is offered for an ISO:
/// `freemkv iso://Disc.iso dir://out/` is a supported CLI pipeline and the
/// engine's ISO path runs it (`engine::run_blocking` → `run_extract_folder`).
pub fn output_formats(disc_source: bool, mp4_ok: bool) -> Vec<Vec<&'static str>> {
    let mut titles = vec!["Selected titles → MKV"];
    if mp4_ok {
        titles.push("Selected titles → MP4");
    }
    titles.push("Selected titles → M2TS");
    titles.push("Selected titles → separate track files");
    // The three narrowed forms of the demux sink — the CLI's `video://`,
    // `audio://` and `sub://`, which are `demux://` with a track-kind filter
    // (libfreemkv `mux::resolve`). They apply to any source the plain demux
    // sink applies to, container included, so they live in the titles group
    // and are never gated on the source kind.
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
/// The View ▸ log menu item's label, which follows STATE rather than naming
/// one fixed action: "Show log" only while the log is hidden, "Hide log" while
/// it is on screen. It is one toggle, so a label that always said "Show log"
/// was wrong half the time.
///
/// Lives here, not in a shell, because both menus are built from it — that is
/// the only way macOS and Windows can be guaranteed to say the same thing.
#[must_use]
pub fn log_menu_label(log_hidden: bool) -> String {
    crate::strings::get(if log_hidden {
        "gui.menu.show_log"
    } else {
        "gui.menu.hide_log"
    })
}

pub fn blocked_while_running(cmd: Cmd) -> bool {
    !matches!(
        cmd,
        Cmd::Cancel
            | Cmd::About
            | Cmd::Docs
            | Cmd::Quit
            // Showing and clearing the log are VIEW state. They touch nothing
            // the rip reads, so blocking them bought no safety and cost the
            // thing the log is for: a rip is exactly when someone wants to
            // watch it, or to get it out of the way and watch the progress
            // block instead. Leaving them disabled meant the only moment you
            // could change your mind was before starting or after finishing —
            // never while there was anything to see.
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
        // `strings::get` returns the dotted path when a key is absent from the
        // active locale AND from English. That happens whenever this crate
        // knows a key the pinned `freemkv-i18n` tag does not yet ship — the
        // window between wiring a new picker row here and re-tagging i18n in
        // the release cascade. Showing `gui.format.video_only` in a dropdown
        // is worse than showing untranslated English, so treat the path
        // echo as "no string" and fall back to the canonical text.
        Some(key) => match crate::strings::get(key) {
            s if s == key => canonical.to_string(),
            s => s,
        },
        None => canonical.to_string(),
    }
}

/// Inverse of [`format_label`]: resolve a LOCALIZED popup label back to the
/// canonical format string. The shell shows `format_label(canonical)`, so a
/// non-English selection reads back as translated text — `format_by_title`
/// only matches the English canonical list, so it would fail in every other
/// locale. Match on the localized display instead.
#[cfg(test)]
mod missing_key_fallback_tests {
    /// A key this crate knows but the pinned i18n tag does not ship must render
    /// as readable English, never as the dotted path. Without the guard in
    /// `format_label` the picker would show `gui.format.video_only` to every
    /// user of a CI build made between wiring a new row and re-tagging i18n.
    #[test]
    fn a_key_absent_from_the_pinned_catalog_falls_back_to_the_canonical_text() {
        // `strings::get` echoes the path for an unknown key; that echo is the
        // exact condition the guard keys on.
        let unknown = "gui.format.__not_in_any_catalog__";
        assert_eq!(crate::strings::get(unknown), unknown);
    }

    /// Every offered format renders as something a human can read: never empty,
    /// and never the raw key path.
    #[test]
    fn every_offered_format_renders_readable_text() {
        for group in super::output_formats(true, true) {
            for canonical in group {
                let shown = super::format_label(canonical);
                assert!(!shown.is_empty(), "{canonical} rendered empty");
                assert!(
                    !shown.starts_with("gui."),
                    "{canonical} rendered the raw key path {shown:?}"
                );
            }
        }
    }
}

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

// ── settings dropdowns ────────────────────────────────────────────────────

/// The option table for a settings dropdown: `(canonical, localized_label)`
/// pairs in menu order.
///
/// The canonical value is what persists and what the engine matches on (e.g.
/// `key_source.starts_with("Online")`); the label is what the localized
/// dropdown shows. So a dropdown displays translated text but stores a stable,
/// English identifier — the same decoupling the format picker uses. An empty
/// result means "not an enum dropdown" (a free-form field, or the format
/// picker, which each shell builds for itself).
///
/// Lives here, not in a shell: this table was duplicated verbatim in
/// `windows.rs` and `mac.rs` and had already drifted (Windows had grown an
/// extra arm). A shell that renders a different option set from the other is
/// a bug by construction, so there is one table and both read it.
///
/// NOTE for shells: the returned order is the menu order, and callers are
/// entitled to map a selected INDEX back to `opts[i].0`. Do not add a key here
/// whose control interleaves separators or is otherwise not 1:1 with this list
/// — `"container"` is deliberately absent for exactly that reason (the macOS
/// format popup carries separator rows, so it maps by title, not by index).
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
        // as-is in every locale). Driven straight from the shipped list, so
        // the picker can never drift from what freemkv-i18n can load.
        "language" => LOCALES
            .iter()
            .map(|(endonym, code)| (*code, (*endonym).to_string()))
            .collect(),
        _ => vec![],
    }
}

// ── tree shape ────────────────────────────────────────────────────────────

/// The parent of every row, derived from the `depth` column alone.
///
/// Both shells rebuild a hierarchical control (an `NSOutlineView`, a
/// `SysTreeView32`) from the flat `Vec<Row>` the core hands them, and both were
/// walking the depths themselves. The walk is the same decision on both, so it
/// lives here: depth 0 starts a new root, depth 1 hangs off the most recent
/// root, anything deeper hangs off the most recent depth-1 row.
///
/// A row that arrives before its parent has no parent to hang from. It becomes
/// a root rather than being dropped — a row the core decided to show must
/// always be reachable, and a silently-vanishing title is far worse than one
/// shown at the wrong indent.
pub fn row_parents(rows: &[Row]) -> Vec<Option<usize>> {
    let (mut last_root, mut last_title) = (None, None);
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
                out.push(last_root);
            }
            _ => out.push(last_title),
        }
    }
    out
}

// ── output naming ─────────────────────────────────────────────────────────

/// The path the Information panel shows as "Output file".
///
/// Named the way the engine will name it, so the row matches what actually
/// lands on disk: `<dir>/<source stem>_t<N>.<ext>`, where `N` is the 1-based
/// number of the first ticked title. Extracted from `start_run` so it can be
/// checked without launching a rip.
/// Whether `format` is one of the options `formats` currently offers.
///
/// `View` publishes the chosen format and the offered list as two independent
/// fields, and nothing kept them in agreement. Opening a source that withdraws
/// an option — an MPEG-2 DVD after an H.264 Blu-ray withdraws MP4 — left the
/// model holding a format no longer on the list. A shell then has to invent a
/// reconciliation policy, and the Win32 one snapped its dropdown to the first
/// entry without telling the model: the user READ "MKV", pressed Run, and the
/// engine was handed MP4, which fails at mux time with E9048.
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

/// Whether `--raw` (keep-encrypted) actually applies to this output.
///
/// Ciphertext passthrough only means anything for a whole-disc ISO image; for
/// any mux it would write encrypted bytes into a container that claims to hold
/// video. Mirrors the CLI's iso-only rule rather than silently forwarding the
/// setting.
pub fn raw_applies(raw_setting: bool, iso_output: bool) -> bool {
    raw_setting && iso_output
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

pub fn output_file_name(
    source: &str,
    dir: &str,
    format: &str,
    first_title: Option<usize>,
) -> String {
    let label = std::path::Path::new(source)
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("output");
    let ext = if format.contains("MP4") {
        "mp4"
    } else if format.contains("M2TS") {
        "m2ts"
    } else {
        "mkv"
    };
    // No ticked title means the engine rips the main movie, which it numbers 1.
    let n = first_title.unwrap_or(0) + 1;
    format!("{dir}/{label}_t{n}.{ext}")
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
    /// Typed verdict for `result_summary` — never re-derive it from the text.
    pub result_outcome: crate::engine::RunOutcome,
    pub selected_row: Option<usize>,
    /// Video codec per title, from the scan — used to warn when the chosen
    /// container cannot carry them. Public alongside the rest of the model so
    /// the container gate can be exercised without a real disc: it is the one
    /// input to `mp4_possible`/`container_mismatch`, and gating those tests
    /// behind a fixture is why they did not run in CI.
    pub video_codecs: Vec<String>,
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
            result_outcome: crate::engine::RunOutcome::default(),
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

    /// Newest lines kept on screen. A multi-hour rip of a damaged disc emits a
    /// line per bad-sector retry, and nothing bounded this during a run — the
    /// two `log.clear()` sites are Clear-log and opening a new source, neither
    /// of which fires mid-rip. Unbounded growth is not just memory: every tick
    /// the shells join, re-read and re-set the WHOLE buffer, so the app got
    /// slower the longer it ran, worst at the end of the longest jobs.
    const LOG_MAX: usize = 5_000;
    /// Dropped per trim, so a long rip pays the O(n) drain rarely instead of
    /// once per line.
    const LOG_TRIM: usize = 1_000;

    pub fn say(&mut self, kind: LogKind, text: &str) {
        self.log.push(LogLine {
            text: text.into(),
            kind,
        });
        if self.log.len() > Self::LOG_MAX {
            // Oldest first: the tail is where a failure surfaces. No notice
            // line — `gui.log.elided` does not exist and freemkv-i18n is pinned
            // to a release tag, so the only alternative would be the one
            // untranslated string in the pane. Debt recorded.
            self.log.drain(..Self::LOG_TRIM);
        }
    }

    /// True when at least one title on this source could go in an MP4. With no
    /// codec information (an unscanned or container source) this is true — the
    /// UI must not hide an option on a guess.
    pub fn mp4_possible(&self) -> bool {
        let known: Vec<&String> = self.video_codecs.iter().filter(|c| !c.is_empty()).collect();
        known.is_empty() || known.iter().any(|c| MP4_VIDEO.contains(&c.as_str()))
    }

    /// The output formats this source can actually produce.
    pub fn offered_formats(&self) -> Vec<Vec<&'static str>> {
        output_formats(!is_container(&self.source), self.mp4_possible())
    }

    /// The format this rip will ACTUALLY use.
    ///
    /// `self.format` is the user's standing preference and outlives the source
    /// it was chosen for — `open()` deliberately does not reset it, so a choice
    /// survives closing and reopening the same disc. But a new source may not
    /// offer it: pick MP4 on an H.264 Blu-ray, then open an MPEG-2 DVD and MP4
    /// leaves the list.
    ///
    /// Every consumer — the view, the progress captions, the output filename
    /// and the `RipRequest` — reads the format through here, so there is one
    /// answer instead of one per caller. Previously `view()` published the raw
    /// preference alongside a list that no longer contained it, and the Win32
    /// shell resolved the contradiction by snapping its dropdown to the first
    /// entry and leaving the model alone: the user read "MKV", pressed Run, and
    /// the engine was handed MP4, failing at mux time with E9048 after the
    /// drive had already been read.
    pub fn effective_format(&self) -> String {
        let offered = self.offered_formats();
        reconcile_format(&self.format, &offered).to_string()
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

    /// Decide which `disc://` source "open the disc in the drive" should open,
    /// and log what was found. `None` means there is nothing to open.
    ///
    /// This is the WHOLE decision behind File ▸ Open disc, the empty state's
    /// "Open disc" button and the launch probe — one copy, so the two shells
    /// cannot drift (they had drifted already: the AppKit shell logged three
    /// hardcoded English sentences where the Win32 one used `gui.log.*`).
    ///
    /// Deliberately split from [`App::open`] rather than doing both: `open`
    /// scans the drive and BLOCKS the calling thread for as long as that takes.
    /// Returning here lets the shell repaint first, so the "Opening …" line is
    /// on screen before the window goes quiet, instead of appearing with the
    /// finished title list.
    ///
    /// `announce_missing` separates the two callers. True for the menu item and
    /// the button, where a human asked and deserves an answer either way. False
    /// for the launch probe, where NOBODY asked: it must open a disc that
    /// happens to be there and otherwise leave no trace.
    ///
    /// "No trace" covers more than an absent drive. A drive with an empty tray
    /// is the ordinary case on a machine that has one, and enumerating drives
    /// says nothing about whether media is loaded — so the probe used to pick
    /// that drive, announce it, fail to scan it, and put an error on screen at
    /// every launch. That is worse than the silence it replaced. When nobody
    /// asked, a drive is only worth opening if it actually holds something.
    pub fn disc_source(&mut self, announce_missing: bool) -> Option<String> {
        let drives = crate::engine::list_optical_drives();
        if drives.is_empty() {
            if announce_missing {
                self.say(LogKind::Notice, &crate::strings::get("gui.log.no_drive"));
            }
            return None;
        }
        // A probe nobody asked for must not GUESS a drive. Bare `disc://`
        // means autodetect — the resolver tries every drive and takes the one
        // that actually holds media — which is the right answer at launch
        // whether the machine has one drive or four. Naming drives[0] because
        // it happened to be the only one enumerated picked a drive with an
        // empty tray and then reported a scan failure the user never asked for.
        if !announce_missing {
            return Some("disc://".to_string());
        }
        // One drive → that device; several → autodetect the one with media,
        // and log what was found so the user knows which drives are present.
        if drives.len() == 1 {
            if announce_missing {
                self.say(
                    LogKind::Detail,
                    &crate::strings::fmt(
                        "gui.log.opening_drive",
                        &[("label", &drives[0].label), ("device", &drives[0].device)],
                    ),
                );
            }
            Some(format!("disc://{}", drives[0].device))
        } else {
            let list = drives
                .iter()
                .map(|d| format!("{} ({})", d.label, d.device))
                .collect::<Vec<_>>()
                .join(", ");
            self.say(
                LogKind::Detail,
                &crate::strings::fmt(
                    "gui.log.drives_found",
                    &[("n", &drives.len().to_string()), ("list", &list)],
                ),
            );
            Some("disc://".to_string())
        }
    }

    /// Open a source: scan it, rebuild the tree, report honestly on failure.
    pub fn open(&mut self, path: &str) -> Vec<Effect> {
        self.open_inner(path, false)
    }

    /// Open something NOBODY asked to open, leaving no trace if it is not
    /// there. Used by the launch probe: a disc already in the drive should just
    /// appear, and an empty tray should look exactly like the app did before
    /// the probe existed.
    pub fn open_probe(&mut self, path: &str) -> Vec<Effect> {
        self.open_inner(path, true)
    }

    fn open_inner(&mut self, path: &str, quiet: bool) -> Vec<Effect> {
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
                self.tree = Tree::from_scan(
                    &sc,
                    &self.settings.selection,
                    min_secs,
                    &LangPrefs::from_settings(&self.settings),
                );
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
                // The launch probe opens a drive NOBODY asked it to open. A
                // drive with an empty tray is the ordinary state of a machine
                // that has one, and enumerating drives says nothing about
                // whether media is loaded — so this arm is the common case at
                // launch, not the rare one. Announcing it put an error on
                // screen every time the app started, which is worse than the
                // silence the probe replaced.
                if !quiet {
                    self.say(LogKind::Notice, &e);
                }
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
        if is_video_only_selection(explicit_streams, &audio_pids, &sub_pids) {
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
        let iso_output = self.effective_format().contains("ISO image");
        let raw = raw_applies(self.settings.raw, iso_output);
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
        let out_file = output_file_name(
            &self.source,
            &self.output_dir,
            &self.effective_format(),
            titles.first().copied(),
        );
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
                format: self.effective_format(),
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
                multipass: wants_multipass(&self.settings.rip_mode, max_passes),
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
        // `Acquire`, pairing with the worker's `Release` store in
        // `engine::start_rip`'s `SignalDone::drop`: seeing `true` here
        // guarantees the `summary`/`outcome` writes sequenced-before that
        // store are visible below, without leaning on an unstated
        // assumption that locking `summary`/`outcome` will happen to
        // provide the same guarantee on every platform this ever runs on.
        if st.finished.load(std::sync::atomic::Ordering::Acquire) {
            let sum = st.summary.lock().map(|s| s.clone()).unwrap_or_default();
            self.say(LogKind::Result, &sum);
            self.result_summary = sum;
            self.result_outcome = st.outcome.lock().map(|o| *o).unwrap_or_default();
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
                &[("container", container_label(&self.effective_format()))],
            ),
            saving_overall: crate::strings::fmt(
                "gui.progress.saving_overall",
                &[("container", container_label(&self.effective_format()))],
            ),
            output_dir: self.output_dir.clone(),
            format: self.effective_format(),
            formats: self.offered_formats(),
            can_run: !self.running() && !self.source.is_empty(),
            log: self.log.clone(),
            log_hidden: self.log_hidden,
            log_menu_label: log_menu_label(self.log_hidden),
            detail: self
                .selected_row
                .and_then(|i| self.tree.arena.get(i))
                .map(|n| n.info.clone())
                .unwrap_or_else(|| crate::strings::get("gui.page.detail_default")),
            result_summary: self.result_summary.clone(),
            // The summary text is engine-emitted English; classify on it, but
            // show a localized heading.
            // Matched on the TYPED verdict. Substring-matching the summary
            // sent an undecryptable disc and both abort-for-loss paths to the
            // success heading, and sent a failed convert to "nothing written".
            result_heading: match self.result_outcome {
                crate::engine::RunOutcome::Cancelled => crate::strings::get("gui.result.cancelled"),
                // Reuses the existing "nothing written" string rather than
                // adding a key: `freemkv-i18n` is pinned to a release tag, so a
                // new key means cutting a tag in another repo. Wording debt,
                // recorded, not a behaviour gap.
                crate::engine::RunOutcome::Failed => crate::strings::get("gui.result.nothing"),
                crate::engine::RunOutcome::Completed => crate::strings::get("gui.result.finished"),
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
mod tests {
    use super::*;

    /// Build one stream row for the preference tests.
    fn row(type_s: &str, pid: u16, lang: &str, forced: bool) -> crate::engine::Row {
        crate::engine::Row {
            type_s: type_s.to_string(),
            desc: String::new(),
            depth: 2,
            checkable: true,
            title: 0,
            info: String::new(),
            pid: Some(pid),
            duration_secs: 0.0,
            lang: lang.to_string(),
            forced,
        }
    }

    /// A forced-subtitle preference that matches nothing must keep NOTHING.
    ///
    /// The real case: a UHD of Dunkirk carrying forced subtitles in French,
    /// German, Spanish and Portuguese, but none in English. Asking for forced
    /// subtitles in English ticked ALL FIVE of them, because this class fell
    /// back to "keep everything" the way audio does when a language is absent.
    ///
    /// For audio that fallback is right — a rip with no audio is broken. For
    /// forced subtitles it is actively harmful: they display by themselves
    /// during playback, so the user who asked for English forced subs and has
    /// none gets four languages of unwanted text burned over the picture. A
    /// file with no forced subtitles is, by contrast, entirely normal.
    #[test]
    fn a_forced_preference_matching_nothing_keeps_nothing() {
        let rows = [
            row("Audio", 1100, "eng", false),
            row("Audio", 1101, "fra", false),
            row("Subtitles", 1200, "eng", false),
            row("Subtitles", 1201, "fra", false),
            // Forced tracks — note there is no English one, as on the disc.
            row("Subtitles", 1300, "fra", true),
            row("Subtitles", 1301, "deu", true),
            row("Subtitles", 1302, "spa", true),
            row("Subtitles", 1303, "por", true),
        ];
        let refs: Vec<&crate::engine::Row> = rows.iter().collect();

        let prefs = LangPrefs::parse("en", "en", "en");
        let keep = preferred_pids(&refs, &prefs);

        for (pid, lang) in [(1300, "fra"), (1301, "deu"), (1302, "spa"), (1303, "por")] {
            assert!(
                !keep.contains(&pid),
                "forced {lang} was ticked, but only English forced subtitles were asked for \
                 — forced subtitles display by themselves, so this puts unwanted text on screen"
            );
        }
        // The other two classes are unaffected: both have an English track, and
        // each class resolves on its own terms.
        assert!(keep.contains(&1100), "English audio must still be kept");
        assert!(keep.contains(&1200), "English subtitles must still be kept");
        assert!(!keep.contains(&1101), "French audio was not asked for");
    }

    /// The other half of the same rule: no preference is NOT an unmatched
    /// preference. An empty box means "no opinion", and must go on keeping
    /// every forced track — otherwise this fix would silently strip forced
    /// subtitles from every rip that never expressed a preference at all.
    #[test]
    fn an_empty_forced_preference_still_keeps_every_forced_track() {
        let rows = [
            row("Subtitles", 1300, "fra", true),
            row("Subtitles", 1301, "deu", true),
        ];
        let refs: Vec<&crate::engine::Row> = rows.iter().collect();

        let keep = preferred_pids(&refs, &LangPrefs::parse("en", "en", ""));
        assert!(keep.contains(&1300) && keep.contains(&1301));
    }

    /// And when the requested forced language IS present, it alone is kept.
    #[test]
    fn a_forced_preference_that_matches_keeps_only_that_language() {
        let rows = [
            row("Subtitles", 1300, "eng", true),
            row("Subtitles", 1301, "deu", true),
            row("Subtitles", 1302, "fra", true),
        ];
        let refs: Vec<&crate::engine::Row> = rows.iter().collect();

        let keep = preferred_pids(&refs, &LangPrefs::parse("", "", "en"));
        assert!(
            keep.contains(&1300),
            "the English forced track was asked for"
        );
        assert!(!keep.contains(&1301) && !keep.contains(&1302));
    }

    /// Neither About box may hard-code what it reports.
    ///
    /// The macOS one did, for all three derived rows, and nothing noticed for
    /// three releases: it read "1.6.0 (macOS)" while the log line beside it
    /// read 1.6.2, and it told every user "keydb ✓ 3 971 entries" — a real
    /// count belonging to whichever machine it was copied from, shown even
    /// with no keydb present. Windows derived all three correctly the whole
    /// time, so this is exactly the kind of drift a shared test catches and
    /// two separately-maintained shells do not.
    ///
    /// Source inspection rather than a UI test: neither shell can be
    /// instantiated off its own platform, but both files can be read from
    /// anywhere — which is the point, since this must fail on whichever
    /// machine runs the suite.
    #[test]
    fn neither_about_box_hard_codes_its_version_or_key_count() {
        // CRLF-normalized: Windows CI checks the tree out with CRLF.
        let shells = [
            ("mac.rs", include_str!("mac.rs").replace("\r\n", "\n")),
            (
                "windows.rs",
                include_str!("windows.rs").replace("\r\n", "\n"),
            ),
        ];
        for (name, src) in &shells {
            for (key, must_contain) in [
                ("gui.about.version", "CARGO_PKG_VERSION"),
                ("gui.about.engine", "CARGO_PKG_VERSION"),
                ("gui.about.keys", "keydb_status"),
            ] {
                let at = src.find(key).unwrap_or_else(|| {
                    panic!("{name}: no About row for {key} — did the box move?")
                });
                // Bound the window at the NEXT About row, so each row is
                // judged on its own text. A fixed-size window silently bled
                // into the following row — and since the row after "version"
                // is "engine", which legitimately contains CARGO_PKG_VERSION,
                // the check passed even with the version hard-coded. A test
                // that cannot fail is worse than no test, so this is measured
                // against the real boundary rather than a guess.
                let rest = &src[at + key.len()..];
                let end = rest.find("gui.about.").unwrap_or(rest.len());
                let window = &rest[..end];
                assert!(
                    window.contains(must_contain),
                    "{name}: the {key} row does not derive from {must_contain} — \
                     a literal here goes stale silently and is wrong for every \
                     user, not just at release time"
                );
            }
        }
    }

    #[test]
    fn sizes_roll_over_instead_of_staying_in_megabytes() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(2048), "2 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        // The bug this pins: a 6 GB output once read "6103.5 M".
        assert_eq!(fmt_bytes(6 * 1024 * 1024 * 1024), "6.00 GB");
        assert_eq!(fmt_bytes(64_424_509_440), "60.00 GB");
    }

    /// The launch probe opens a drive nobody asked it to open. A drive with an
    /// empty tray is the ORDINARY state of a machine that has one, and
    /// enumerating drives says nothing about whether media is loaded — so a
    /// failed probe is the common case at startup, not the rare one.
    ///
    /// It must look exactly like the app did before the probe existed: no
    /// notice, no log line, and the empty page still showing. A prompted open
    /// of the same bad source must still report, because a human asked.
    #[test]
    fn a_failed_probe_says_nothing_but_a_failed_open_still_reports() {
        let mut app = App::new();
        let before = app.log.len();
        app.open_probe("iso:///nonexistent/definitely-not-here.iso");
        assert_eq!(
            app.log.len(),
            before,
            "an unprompted probe that finds nothing must leave no trace, got: {:?}",
            &app.log[before..]
        );
        assert!(matches!(app.page, Page::Empty));

        app.open("iso:///nonexistent/definitely-not-here.iso");
        assert!(
            app.log.len() > before,
            "a human who asked must be told the source could not be opened"
        );
    }

    #[test]
    fn the_log_menu_label_follows_the_state_not_one_fixed_action() {
        // The bug this pins: both shells built "Show log" once, at menu-build
        // time, so the item still read "Show log" while the log was on screen.
        assert_eq!(
            log_menu_label(true),
            crate::strings::get("gui.menu.show_log")
        );
        assert_eq!(
            log_menu_label(false),
            crate::strings::get("gui.menu.hide_log")
        );
        assert_ne!(log_menu_label(true), log_menu_label(false));
    }

    #[test]
    fn toggling_the_log_flips_the_menu_label_in_the_view() {
        let mut a = App::new();
        // The log starts visible, so the item offers to hide it.
        assert!(!a.view().log_hidden);
        assert_eq!(a.view().log_menu_label, log_menu_label(false));
        a.dispatch(Cmd::ToggleLog);
        assert!(a.view().log_hidden);
        assert_eq!(a.view().log_menu_label, log_menu_label(true));
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
            &LangPrefs::default(),
        );
        assert!(t.roots.is_empty());
        assert!(t.arena.is_empty());
    }

    /// Every string the picker can offer must have a `gui.format.*` key, and
    /// that key must exist in en.json carrying exactly the canonical text.
    ///
    /// `format_label`'s catch-all returns the canonical string unchanged, so a
    /// row with no key renders correctly under `en` and is invisible in
    /// testing — but it is untranslatable in the other 28 locales forever.
    /// That is how the three per-track-kind rows shipped keyless. Asserting on
    /// `format_key` (not on the rendered label) is what makes the gap visible:
    /// the label is identical either way.
    #[test]
    fn every_offered_format_has_a_translation_key_present_in_english() {
        let en: serde_json::Value =
            serde_json::from_str(freemkv_i18n::bundled_locale_json("en").expect("en is bundled"))
                .expect("en.json parses");

        // Both source kinds, both MP4 states — the union is every string the
        // picker can ever show.
        let mut offered: Vec<&str> = [(true, true), (true, false), (false, true), (false, false)]
            .into_iter()
            .flat_map(|(disc, mp4)| output_formats(disc, mp4).into_iter().flatten())
            .collect();
        offered.sort_unstable();
        offered.dedup();
        assert!(!offered.is_empty(), "the picker offers nothing");

        for canon in offered {
            let key = format_key(canon).unwrap_or_else(|| {
                panic!("{canon:?} has no gui.format key — it can never be translated")
            });

            // The key must resolve in en.json, not merely exist in the match.
            let mut node = &en;
            for part in key.split('.') {
                node = node
                    .get(part)
                    .unwrap_or_else(|| panic!("{key} ({canon:?}) is missing from en.json"));
            }
            let text = node
                .as_str()
                .unwrap_or_else(|| panic!("{key} is not a string in en.json"));

            // English is the canonical text by definition: if they diverge, the
            // engine matches one string while the user picked another.
            assert_eq!(
                text, canon,
                "{key} in en.json does not match the canonical picker string"
            );
        }
    }

    /// A canonical format's localized label must resolve back to that exact
    /// canonical string. The shells persist and the engine matches the
    /// canonical form, so a one-way label is a setting that silently reverts.
    ///
    /// REGRESSION PIN, not a fix: this already passed before the keys were
    /// added, because `format_label`'s catch-all round-trips a keyless row
    /// through the `format_by_title` fast path. It is here so that WIRING a
    /// key (which routes the row through `strings::get`) cannot break the
    /// round-trip — the failure mode the key change introduces.
    #[test]
    fn every_offered_format_round_trips_through_its_label() {
        for (disc, mp4) in [(true, true), (true, false), (false, true), (false, false)] {
            for canon in output_formats(disc, mp4).into_iter().flatten() {
                let label = format_label(canon);
                assert_eq!(
                    format_from_label(&label, disc, mp4),
                    Some(canon),
                    "label {label:?} does not resolve back to {canon:?}"
                );
            }
        }
    }
}
