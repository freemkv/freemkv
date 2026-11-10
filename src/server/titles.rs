//! The title lists the Titles and Confirm episodes dialogs show (design §4.2, §4.3), and the
//! operator picks they send back. A pick names titles by [`TitleIdentity`], which the client
//! echoes verbatim, never by index alone, so a rescan cannot shift it onto other titles.

use crate::server::config::Config;
use crate::server::planner::{EpisodeConfirm, MediaKind, ScopeMode, TitleRequest};
use crate::title_identity::TitleIdentity;
use libfreemkv::DiscTitle;

/// One scanned title as a dialog lists it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TitleRow {
    /// Scan order, as `title_pick` and `confirm` indices use it.
    pub index: usize,
    /// What a pick sends back for this title.
    pub id: TitleIdentity,
    pub playlist: String,
    pub duration_secs: f64,
    pub duration: String,
    pub size_bytes: u64,
    /// Video, first audio, and how many audio and subtitle tracks.
    pub tracks: String,
    /// The disc's own episode list names it (proven, never a suggestion).
    pub in_roster: bool,
    /// It plays two or more other, disjoint titles (a "play all").
    pub play_all: bool,
    /// A variant folded into this kept title (recap, commentary playlist, alias).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alternate_of: Option<usize>,
    /// For a held capture: whether its sweep read this title (only those can be chosen).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swept: Option<bool>,
}

/// Every scanned title, what the plan runs without a pick, and the suggestion it holds for.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TitleList {
    pub titles: Vec<TitleRow>,
    /// The titles the plan runs without a pick, in order; empty when it holds to confirm.
    pub automatic: Vec<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<EpisodeConfirm>,
}

/// The list for `titles` under `request`'s metadata (its pick is ignored: `automatic` is
/// what the plan does without one).
pub fn list(
    cfg: &Config,
    titles: &[DiscTitle],
    request: &TitleRequest,
    mode: ScopeMode,
) -> TitleList {
    use freemkv_engine::{Resolved, SelectionModel, StreamFilter};
    let model = SelectionModel::from_titles(titles);
    let presentation = crate::server::planner::presentation(cfg);
    let min_secs = cfg.min_length_secs as f64;
    let roster = match model.resolve_episodes(&StreamFilter::All, presentation, min_secs) {
        Resolved::Run {
            titles,
            episodes: true,
        } => titles,
        _ => Vec::new(),
    };
    let suggestion = model.suggest_episodes(&StreamFilter::All, presentation, min_secs);
    let automatic = TitleRequest {
        pick: None,
        pick_mismatch: false,
        ..request.clone()
    };
    let (automatic, confirm) = match automatic.run(cfg, titles, mode) {
        Ok(run) => (run.titles, None),
        Err(error) => (Vec::new(), error.confirm().cloned()),
    };
    let rows = titles
        .iter()
        .enumerate()
        .map(|(index, title)| TitleRow {
            index,
            id: TitleIdentity::of(title),
            playlist: crate::strings::sanitize_display(&title.playlist),
            duration_secs: title.duration_secs,
            duration: crate::server::util::format_duration_hm(title.duration_secs),
            size_bytes: title.size_bytes,
            tracks: tracks(title),
            in_roster: roster.contains(&index),
            play_all: suggestion.play_alls.contains(&index),
            alternate_of: (suggestion.alternates.iter())
                .find(|a| a.index == index)
                .map(|a| a.of),
            swept: None,
        })
        .collect();
    TitleList {
        titles: rows,
        automatic,
        confirm,
    }
}

fn tracks(title: &DiscTitle) -> String {
    use crate::strings::fmt_or;
    let count = |audio: bool| {
        (title.streams.iter())
            .filter(|s| match s {
                libfreemkv::Stream::Audio(_) => audio,
                libfreemkv::Stream::Subtitle(_) => !audio,
                libfreemkv::Stream::Video(_) => false,
            })
            .count()
            .to_string()
    };
    let mut parts = vec![crate::server::ripper::format_codecs(title)];
    parts.retain(|p| !p.is_empty());
    parts.push(fmt_or(
        "server.titles.audio_tracks",
        "{n} audio",
        &[("n", &count(true))],
    ));
    parts.push(fmt_or(
        "server.titles.subtitle_tracks",
        "{n} subtitles",
        &[("n", &count(false))],
    ));
    parts.join(" · ")
}

/// Most titles a pick may name; a disc has far fewer.
const MAX_PICK: usize = 999;

/// The pick a request body's `titles` names: an array of identities as the list sent them.
/// An empty array clears the pick (`None`, back to automatic).
pub fn parse_pick(value: &serde_json::Value) -> Result<Option<Vec<TitleIdentity>>, String> {
    let invalid = not_a_list;
    let items = value.as_array().ok_or_else(invalid)?;
    if items.len() > MAX_PICK {
        return Err(invalid());
    }
    let pick = (items.iter())
        .map(|item| serde_json::from_value(item.clone()).map_err(|_| invalid()))
        .collect::<Result<Vec<TitleIdentity>, _>>()?;
    Ok((!pick.is_empty()).then_some(pick))
}

fn not_a_list() -> String {
    crate::strings::get_or(
        "error.titles.not_a_list",
        "titles must be a list of titles as the title list gave them",
    )
}

/// Whether `count` chosen titles suit `kind`: a Movie is exactly one feature; TV is one or
/// more episodes, and one alone needs its First episode number (`episode_start`).
pub fn check_pick(kind: MediaKind, count: usize, episode_start: Option<u16>) -> Result<(), String> {
    use crate::strings::get_or;
    match kind {
        MediaKind::Movie if count != 1 => Err(get_or(
            "error.titles.movie_one",
            "A movie is one title: choose exactly one.",
        )),
        MediaKind::Tv if count == 0 => Err(get_or(
            "error.titles.tv_none",
            "Choose at least one episode.",
        )),
        MediaKind::Tv if count == 1 && episode_start.is_none() => Err(get_or(
            "error.titles.tv_one_needs_first",
            "One episode needs its First episode number: set it with Change title, then choose again.",
        )),
        _ => Ok(()),
    }
}

/// `pick` as indices into `titles`, or the error a stale or foreign pick gets.
pub fn remap(titles: &[DiscTitle], pick: &[TitleIdentity]) -> Result<Vec<usize>, String> {
    crate::server::planner::remap_pick(titles, pick).ok_or_else(|| {
        crate::strings::get_or(
            "error.titles.mismatch",
            "The chosen titles are not on this disc as scanned; refresh the list and choose again.",
        )
    })
}

#[cfg(test)]
#[path = "titles_tests.rs"]
pub(crate) mod tests;
