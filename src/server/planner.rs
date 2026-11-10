//! Immutable source-to-output planning shared by autorip and Library remux.
//!
//! The planner decides *what* a source produces.  Executors only consume the
//! resulting plan and must never redo TMDB lookup or episode assignment.

use crate::server::config::Config;
use libfreemkv::DiscTitle;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub use freemkv_engine::metadata::{MediaKind, MediaMetadata, lenient};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedOutput {
    pub id: String,
    pub title_index: usize,
    pub episode: Option<u16>,
    pub episode_name: String,
    pub filename: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemuxPlan {
    pub media: MediaMetadata,
    pub outputs: Vec<PlannedOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    NoTitles,
    NoOutputs,
    DuplicateOutput(String),
    /// An episode set the disc does not prove (E9086): held for the operator to confirm.
    Selection(EpisodeConfirm),
    /// The operator's saved title pick no longer matches the scanned titles.
    PickMismatch,
    /// The episode numbers would run past the supported range.
    EpisodeRange,
}

impl PlanError {
    /// What the operator can do about a held selection in the server UI, if anything.
    pub(crate) fn action(&self) -> Option<String> {
        use crate::strings::get_or;
        match self {
            PlanError::Selection(_) => Some(get_or(
                "error.plan.action.confirm_episodes",
                "Confirm which titles are the episodes, and their order, to rip them; if it is a film, correct its match to Movie.",
            )),
            PlanError::PickMismatch => Some(get_or(
                "error.plan.action.repick",
                "Choose the titles again for this disc.",
            )),
            PlanError::EpisodeRange => Some(get_or(
                "error.plan.action.first_episode",
                "Set the First episode when you correct its match.",
            )),
            PlanError::NoTitles | PlanError::NoOutputs | PlanError::DuplicateOutput(_) => None,
        }
    }

    /// `held` (a sentence naming this error) followed by [`PlanError::action`], if any.
    pub(crate) fn with_action(&self, held: String) -> String {
        match self.action() {
            Some(action) => format!("{held} {action}"),
            None => held,
        }
    }

    /// The episode suggestion a held selection carries for the operator, if any.
    pub(crate) fn confirm(&self) -> Option<&EpisodeConfirm> {
        match self {
            PlanError::Selection(confirm) => Some(confirm),
            _ => None,
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::strings::{fmt_or, get_or};
        let text = match self {
            PlanError::NoTitles => get_or("error.plan.no_titles", "the disc has no usable titles"),
            PlanError::NoOutputs => get_or("error.plan.no_outputs", "the plan has no outputs"),
            PlanError::DuplicateOutput(name) => fmt_or(
                "error.plan.duplicate_output",
                "two outputs would use the same file name: {name}",
                &[("name", name)],
            ),
            PlanError::EpisodeRange => get_or(
                "error.plan.episode_range",
                "Episode numbering is outside the supported range.",
            ),
            PlanError::PickMismatch => get_or(
                "error.plan.pick_mismatch",
                "the chosen titles are not on this disc as scanned",
            ),
            PlanError::Selection(confirm) => format!(
                "E{} {}",
                libfreemkv::error::E_SELECTION_REVIEW_REQUIRED,
                crate::strings::review_reason_text(confirm.reason)
            ),
        };
        f.write_str(&text)
    }
}

/// A suggested episode set the operator confirms (design §3.3, §4.3). Indices are the scan's
/// title order, which a rescan of the same disc or image reproduces; nothing here is proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeConfirm {
    /// Why the disc does not prove its episodes (`SelectionReviewReason::key`).
    #[serde(with = "review_reason")]
    pub reason: freemkv_engine::SelectionReviewReason,
    /// The one qualifying cluster, pre-ticked; empty when none or several qualify.
    #[serde(default)]
    pub suggested: Vec<usize>,
    /// Every cluster of 2+ like titles, shown grouped with nothing ticked.
    #[serde(default)]
    pub clusters: Vec<Vec<usize>>,
    /// Variants folded into a kept title (`of`), listed unticked.
    #[serde(default)]
    pub alternates: Vec<ConfirmAlternate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmAlternate {
    pub index: usize,
    pub of: usize,
}

impl EpisodeConfirm {
    /// Every title the suggestion names: the suggested set, the clusters and the alternates.
    pub fn titles(&self) -> Vec<usize> {
        let mut all: Vec<usize> = (self.suggested.iter().copied())
            .chain(self.clusters.iter().flatten().copied())
            .chain(self.alternates.iter().flat_map(|a| [a.index, a.of]))
            .collect();
        all.sort_unstable();
        all.dedup();
        all
    }

    /// Only the titles `keep` allows (a held capture's swept scope): anything else needs the
    /// disc. A cluster left with fewer than 2 titles is dropped, as is an alternate of a
    /// dropped title.
    pub fn within(mut self, keep: impl Fn(usize) -> bool) -> Self {
        self.suggested.retain(|&index| keep(index));
        for cluster in &mut self.clusters {
            cluster.retain(|&index| keep(index));
        }
        self.clusters.retain(|cluster| cluster.len() >= 2);
        if self.suggested.len() < 2 {
            self.suggested.clear();
        }
        self.alternates
            .retain(|alternate| keep(alternate.index) && keep(alternate.of));
        self
    }
}

mod review_reason {
    use freemkv_engine::SelectionReviewReason as R;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(reason: &R, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(reason.key())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<R, D::Error> {
        let key = String::deserialize(d)?;
        Ok([
            R::IncompleteEpisodeRoster,
            R::ConflictingEpisodeRoster,
            R::InvalidEpisodeRoster,
        ]
        .into_iter()
        .find(|reason| reason.key() == key)
        .unwrap_or(R::MissingEpisodeRoster))
    }
}

/// How a run reads titles (design §3.2, rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeMode {
    /// Whole-disc ISO output: no title choice; title 0 stands for the image.
    WholeDisc,
    /// A single-pass rip muxes one title and never splits or holds.
    SinglePass,
    /// A multipass capture, a resume or a Library remux: picks and episode sets apply.
    Mux,
}

impl ScopeMode {
    /// A fresh capture's mode under `cfg`.
    pub fn capture(cfg: &Config) -> Self {
        if cfg.output_format == crate::server::config::OUTPUT_FORMAT_ISO {
            ScopeMode::WholeDisc
        } else if cfg.max_retries == 0 {
            ScopeMode::SinglePass
        } else {
            ScopeMode::Mux
        }
    }
}

/// The one title policy every scope consumer uses (design §3.2):
/// 1. whole-disc ISO or single-pass: no split and no hold (a single pass muxes a Movie
///    pick's feature, else the main title);
/// 2. an operator `pick` (scan indices, in order): exactly those titles; a Movie keeps one;
/// 3. TV under `tv_auto`: the proven roster, else a suggestion to confirm;
/// 4. otherwise the main film. Movies are never split and never held.
pub fn resolve_titles(
    cfg: &Config,
    titles: &[DiscTitle],
    media: &MediaMetadata,
    pick: Option<&[usize]>,
    mode: ScopeMode,
) -> freemkv_engine::Resolved {
    use freemkv_engine::Resolved;
    let model = freemkv_engine::SelectionModel::from_titles(titles);
    let tv = media.kind == Some(MediaKind::Tv);
    let main = || {
        let titles = model
            .main_title(&freemkv_engine::StreamFilter::All, presentation(cfg))
            .into_iter()
            .collect();
        Resolved::Run {
            titles,
            episodes: false,
        }
    };
    let pick = pick.map(|pick| match model.resolve_explicit(pick, tv) {
        Resolved::Run { mut titles, .. } => {
            if !tv {
                titles.truncate(1);
            }
            titles
        }
        Resolved::Confirm { .. } => Vec::new(),
    });
    match mode {
        ScopeMode::WholeDisc => Resolved::Run {
            titles: (!titles.is_empty()).then_some(0).into_iter().collect(),
            episodes: false,
        },
        ScopeMode::SinglePass => match pick.filter(|pick| !tv && !pick.is_empty()) {
            Some(pick) => Resolved::Run {
                titles: pick,
                episodes: false,
            },
            None => main(),
        },
        ScopeMode::Mux => match pick {
            Some(titles) => Resolved::Run {
                titles,
                episodes: tv,
            },
            None if tv && cfg.tv_auto => model.resolve_episodes(
                &freemkv_engine::StreamFilter::All,
                presentation(cfg),
                cfg.min_length_secs as f64,
            ),
            None => main(),
        },
    }
}

/// What a source is and which titles the operator chose, resolved once for a plan: the
/// metadata from [`MediaMetadata::resolve`] and the saved pick remapped onto the scan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TitleRequest {
    pub media: MediaMetadata,
    /// The operator's titles as scan indices, in order.
    pub pick: Option<Vec<usize>>,
    /// A saved pick did not match the scan: the plan holds and never falls back.
    pub pick_mismatch: bool,
}

/// The run a [`TitleRequest`] resolves to: titles in order, and whether they are episodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleRun {
    pub titles: Vec<usize>,
    pub episodes: bool,
}

impl TitleRequest {
    /// `media` (already resolved) with the saved `pick` remapped onto `titles` by identity.
    pub fn new(
        media: MediaMetadata,
        pick: Option<&[crate::title_identity::TitleIdentity]>,
        titles: &[DiscTitle],
    ) -> Self {
        let remapped = pick.map(|pick| remap_pick(titles, pick));
        Self {
            media,
            pick_mismatch: matches!(remapped, Some(None)),
            pick: remapped.flatten(),
        }
    }

    /// [`resolve_titles`] for this request.
    pub fn resolve(
        &self,
        cfg: &Config,
        titles: &[DiscTitle],
        mode: ScopeMode,
    ) -> freemkv_engine::Resolved {
        resolve_titles(cfg, titles, &self.media, self.pick.as_deref(), mode)
    }

    /// The run, or why it holds: a pick that no longer matches, an episode set to confirm,
    /// or no titles at all.
    pub fn run(
        &self,
        cfg: &Config,
        titles: &[DiscTitle],
        mode: ScopeMode,
    ) -> Result<TitleRun, PlanError> {
        if self.pick_mismatch && mode != ScopeMode::WholeDisc {
            return Err(PlanError::PickMismatch);
        }
        match self.resolve(cfg, titles, mode) {
            freemkv_engine::Resolved::Run { titles, .. } if titles.is_empty() => {
                Err(PlanError::NoTitles)
            }
            freemkv_engine::Resolved::Run { titles, episodes } => Ok(TitleRun { titles, episodes }),
            freemkv_engine::Resolved::Confirm {
                suggested,
                clusters,
                alternates,
                reason,
            } => Err(PlanError::Selection(EpisodeConfirm {
                reason,
                suggested,
                clusters,
                alternates: alternates
                    .into_iter()
                    .map(|a| ConfirmAlternate {
                        index: a.index,
                        of: a.of,
                    })
                    .collect(),
            })),
        }
    }

    /// Every title this request may read: the run, or for a hold the titles it suggests plus
    /// the main title (they size a key scope; they never authorize a rip).
    pub fn scope(&self, cfg: &Config, titles: &[DiscTitle], mode: ScopeMode) -> Vec<usize> {
        let mut scope = match self.run(cfg, titles, mode) {
            Ok(run) => run.titles,
            Err(error) => {
                let mut held = error
                    .confirm()
                    .map(EpisodeConfirm::titles)
                    .unwrap_or_default();
                held.extend(main_title(cfg, titles).ok());
                held
            }
        };
        scope.sort_unstable();
        scope.dedup();
        scope
    }
}

/// `pick` as indices into `titles`, in order; `None` when any identity is missing, repeated,
/// or matches more than one title.
pub fn remap_pick(
    titles: &[DiscTitle],
    pick: &[crate::title_identity::TitleIdentity],
) -> Option<Vec<usize>> {
    let scanned: Vec<_> = titles
        .iter()
        .map(crate::title_identity::TitleIdentity::of)
        .collect();
    let mut out = Vec::with_capacity(pick.len());
    for identity in pick {
        let mut matches = (scanned.iter().enumerate()).filter(|(_, scanned)| *scanned == identity);
        let (index, _) = matches.next()?;
        if matches.next().is_some() || out.contains(&index) {
            return None;
        }
        out.push(index);
    }
    (!out.is_empty()).then_some(out)
}

/// What `media` produces from `titles` (a remux of an image, [`ScopeMode::Mux`]): one output
/// per episode of the resolved run, else its one title as `movie_filename`'s leaf.
pub fn plan(
    cfg: &Config,
    titles: &[DiscTitle],
    media: &MediaMetadata,
    movie_filename: &Path,
) -> Result<RemuxPlan, PlanError> {
    plan_request(
        cfg,
        titles,
        &TitleRequest {
            media: media.clone(),
            ..TitleRequest::default()
        },
        movie_filename,
    )
}

/// [`plan`] for a request that may carry an operator pick.
pub fn plan_request(
    cfg: &Config,
    titles: &[DiscTitle],
    request: &TitleRequest,
    movie_filename: &Path,
) -> Result<RemuxPlan, PlanError> {
    let media = &request.media;
    let run = request.run(cfg, titles, ScopeMode::Mux)?;
    // A TV movie or a single detected feature remains one output.
    if !run.episodes {
        return Ok(single_plan(media, run.titles[0], leaf(movie_filename)));
    }
    let season = media.season.unwrap_or(1);
    // Numbering is checked before the blocking TMDB fetch.
    let start = episode_start(run.titles.len(), media.disc, media.episode_start)?;
    let episodes = crate::server::tmdb::season_episodes(media.tmdb_id, season, &cfg.tmdb_api_key);
    let outputs = episode_outputs_from_start(&run.titles, season, start, &episodes, movie_filename);
    validate_outputs(outputs).map(|outputs| RemuxPlan {
        media: media.clone(),
        outputs,
    })
}

/// The main feature under `cfg`'s presentation language (never a review).
pub(crate) fn main_title(cfg: &Config, titles: &[DiscTitle]) -> Result<usize, PlanError> {
    freemkv_engine::SelectionModel::from_titles(titles)
        .main_title(&freemkv_engine::StreamFilter::All, presentation(cfg))
        .ok_or(PlanError::NoTitles)
}

fn presentation(cfg: &Config) -> Option<&str> {
    Some(cfg.presentation_language.as_str()).filter(|language| !language.trim().is_empty())
}

/// One episode assignment and staging-name policy for capture and remux.
pub(crate) fn episode_outputs(
    indices: &[usize],
    season: u16,
    disc: Option<u16>,
    episodes: &[crate::server::tmdb::Episode],
    movie_filename: &Path,
    episode_start: Option<u16>,
) -> Result<Vec<PlannedOutput>, PlanError> {
    let start = self::episode_start(indices.len(), disc, episode_start)?;
    Ok(episode_outputs_from_start(
        indices,
        season,
        start,
        episodes,
        movie_filename,
    ))
}

/// A disc's first episode number (the engine's `episode_numbers`): the operator's First
/// episode, otherwise disc N begins at `(N-1)*count+1`. Metadata cannot change that number.
pub(crate) fn episode_start(
    count: usize,
    disc: Option<u16>,
    explicit: Option<u16>,
) -> Result<u16, PlanError> {
    freemkv_engine::metadata::episode_numbers(count, disc, explicit)
        .map_err(|_| PlanError::EpisodeRange)
}

fn episode_outputs_from_start(
    indices: &[usize],
    season: u16,
    start: u16,
    episodes: &[crate::server::tmdb::Episode],
    movie_filename: &Path,
) -> Vec<PlannedOutput> {
    let assignments = crate::server::tmdb::numbered_episode_names(indices.len(), episodes, start);
    let stem = movie_filename
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("title");
    let ext = movie_filename
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("mkv");
    indices
        .iter()
        .zip(assignments)
        .map(|(&idx, a)| PlannedOutput {
            id: format!("title-{idx}-episode-{}", a.episode),
            title_index: idx,
            episode: Some(a.episode),
            episode_name: a.name,
            filename: format!("{stem}_S{season:02}E{:02}.{ext}", a.episode),
        })
        .collect()
}

fn single_plan(media: &MediaMetadata, title_index: usize, filename: String) -> RemuxPlan {
    RemuxPlan {
        media: media.clone(),
        outputs: vec![PlannedOutput {
            id: format!("title-{title_index}"),
            title_index,
            episode: None,
            episode_name: String::new(),
            filename,
        }],
    }
}

fn validate_outputs(outputs: Vec<PlannedOutput>) -> Result<Vec<PlannedOutput>, PlanError> {
    let mut seen = std::collections::HashSet::new();
    for output in &outputs {
        if !seen.insert(output.filename.clone()) {
            return Err(PlanError::DuplicateOutput(output.filename.clone()));
        }
    }
    if outputs.is_empty() {
        Err(PlanError::NoOutputs)
    } else {
        Ok(outputs)
    }
}

fn leaf(path: &Path) -> String {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "title.mkv".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfreemkv::disc::{ContentFormat, Extent};

    fn title(duration_secs: f64, start_lba: u32) -> DiscTitle {
        DiscTitle {
            selection_evidence: Default::default(),
            playlist: String::new(),
            playlist_id: 0,
            duration_secs,
            size_bytes: duration_secs as u64 * 1_000_000,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![Extent {
                start_lba,
                sector_count: 1000,
            }],
            content_format: ContentFormat::BdTs,
            codec_privates: Vec::new(),
        }
    }

    #[test]
    fn a_later_disc_is_numbered_from_disc_order_not_tmdb_runtimes() {
        assert_eq!(episode_start(2, Some(1), None), Ok(1));
        assert_eq!(episode_start(2, Some(2), None), Ok(3));
        // Even distinctive TMDB runtimes cannot shift disc-derived numbering.
        let episodes = (1..=5)
            .map(|number| crate::server::tmdb::Episode {
                number,
                name: format!("Episode {number}"),
                runtime_min: if number < 4 { 40 } else { 50 },
            })
            .collect::<Vec<_>>();
        let start = episode_start(2, Some(2), None).unwrap();
        assert_eq!(start, 3);
        // The operator's First episode wins.
        assert_eq!(episode_start(2, Some(2), Some(2)), Ok(2));
        let outputs =
            episode_outputs_from_start(&[0, 1], 1, start, &episodes, Path::new("Show.mkv"));
        assert_eq!(
            outputs.iter().map(|o| o.episode).collect::<Vec<_>>(),
            [Some(3), Some(4)]
        );
        assert_eq!(outputs[0].episode_name, "Episode 3");
        assert_eq!(outputs[1].filename, "Show_S01E04.mkv");
        assert!(episode_start(2, Some(2), Some(u16::MAX)).is_err());
        assert!(episode_start(2, Some(2), Some(0)).is_err());
        // A disc number past the numbering range is held, never wrapped.
        assert_eq!(
            episode_start(2, Some(u16::MAX), None),
            Err(PlanError::EpisodeRange)
        );
    }

    #[test]
    fn ambiguous_tv_selection_never_silently_becomes_a_movie() {
        let cfg = Config::default();
        let titles = vec![title(2700.0, 100), title(2700.0, 200)];
        let media = MediaMetadata {
            kind: Some(MediaKind::Tv),
            ..Default::default()
        };
        let result = plan(&cfg, &titles, &media, Path::new("show.mkv"));
        assert!(matches!(result, Err(PlanError::Selection(_))));
    }

    #[test]
    fn movie_planner_always_returns_one_output() {
        let titles = vec![title(5_000.0, 100), title(120.0, 5)];
        let media = MediaMetadata {
            title: "Film".into(),
            kind: Some(MediaKind::Movie),
            ..Default::default()
        };
        let plan = plan(&Config::default(), &titles, &media, Path::new("Film.mkv")).unwrap();
        assert_eq!(plan.outputs.len(), 1);
        assert_eq!(plan.outputs[0].title_index, 0);
        assert_eq!(plan.outputs[0].filename, "Film.mkv");
    }

    #[test]
    fn presentation_language_picks_a_cut_of_title_zero_and_never_holds() {
        let media = MediaMetadata {
            kind: Some(MediaKind::Movie),
            ..Default::default()
        };
        let cases = [
            ("de", 1, 0),
            ("en", 0, 0),
            ("", 0, 0),
            ("fr", 0, 0),
            ("invalid-language", 0, 0),
        ];
        for (language, variant, separate) in cases {
            let cfg = Config {
                presentation_language: language.into(),
                ..Default::default()
            };
            for (titles, expected) in [
                (crate::selection_test_fixtures::variant_titles(), variant),
                (crate::selection_test_fixtures::separate_titles(), separate),
            ] {
                let plan = plan(&cfg, &titles, &media, Path::new("movie.mkv"));
                let index = plan.map(|plan| plan.outputs[0].title_index);
                assert_eq!(index.ok(), Some(expected), "{language:?}");
            }
        }
    }

    #[test]
    fn movie_planner_preserves_shared_selection_and_empty_source_error() {
        for titles in [vec![], vec![title(120.0, 5), title(5000.0, 100)]] {
            let selected = freemkv_engine::SelectionModel::from_titles(&titles)
                .main_title(&freemkv_engine::StreamFilter::All, None);
            let planned = plan(
                &Config::default(),
                &titles,
                &MediaMetadata::default(),
                Path::new("film.mkv"),
            );
            match selected {
                None => assert_eq!(planned, Err(PlanError::NoTitles)),
                Some(index) => assert_eq!(planned.unwrap().outputs[0].title_index, index),
            }
        }
    }

    #[test]
    fn episode_filenames_follow_authored_order_after_canonical_duration_sort() {
        let mut titles = vec![title(1800.0, 100), title(2700.0, 200), title(2100.0, 300)];
        for (ordinal, title) in titles.iter_mut().enumerate() {
            title.playlist_id = ordinal as u16;
            title.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
                roster: "test-menu".into(),
                title_count: 3,
                ordinal: Some(ordinal),
            };
        }
        titles.sort_by(|a, b| b.duration_secs.total_cmp(&a.duration_secs));
        let cfg = Config::default();
        let media = MediaMetadata {
            kind: Some(MediaKind::Tv),
            season: Some(2),
            ..Default::default()
        };
        let plan = plan(&cfg, &titles, &media, Path::new("Show.mkv")).unwrap();
        assert_eq!(
            plan.outputs
                .iter()
                .map(|o| (o.title_index, o.episode, o.filename.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (2, Some(1), "Show_S02E01.mkv"),
                (0, Some(2), "Show_S02E02.mkv"),
                (1, Some(3), "Show_S02E03.mkv")
            ]
        );
    }

    #[test]
    fn tv_planner_collapses_six_alternate_playlists_to_three_episodes() {
        let cfg = Config::default();
        let episode = 44.0 * 60.0;
        let mut titles = vec![title(episode * 6.0, 100)];
        for (episode_name, start) in [("Islands", 1000), ("Jungles", 2000), ("Deserts", 3000)] {
            for alternate in 0..2 {
                let mut t = title(episode + alternate as f64, start + alternate * 10_000);
                t.clips = vec![libfreemkv::Clip {
                    marks_on_pes_clock: true,
                    clip_id: episode_name.to_ascii_lowercase(),
                    in_time: 0,
                    out_time: (episode * 45_000.0) as u32,
                    duration_secs: episode,
                    source_packets: 0,
                    feed_span: None,
                }];
                t.selection_evidence.segments =
                    libfreemkv::disc::ContentSegment::from_clips(&t.clips);
                titles.push(t);
            }
        }
        titles.push(title(90.0, 5));
        let title_count = titles.len();
        for (index, title) in titles.iter_mut().enumerate() {
            title.playlist_id = index as u16;
            title.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
                roster: "test-menu".into(),
                title_count,
                ordinal: (1..=6).contains(&index).then(|| (index - 1) / 2),
            };
        }
        let media = MediaMetadata {
            title: "Endeavour".into(),
            kind: Some(MediaKind::Tv),
            season: Some(5),
            tmdb_id: 44264,
            ..Default::default()
        };
        let plan = plan(&cfg, &titles, &media, Path::new("Endeavour.mkv")).unwrap();
        assert_eq!(plan.outputs.len(), 3);
        assert_eq!(plan.outputs[0].title_index, 1);
        assert_eq!(plan.outputs[0].episode, Some(1));
        assert_eq!(plan.outputs[0].filename, "Endeavour_S05E01.mkv");
        assert_eq!(plan.outputs[1].title_index, 3);
        assert_eq!(plan.outputs[2].title_index, 5);
        assert_eq!(plan.outputs[2].filename, "Endeavour_S05E03.mkv");
    }

    #[test]
    fn tv_fallbacks_remain_one_output() {
        let titles = vec![title(95.0 * 60.0, 1000), title(90.0, 5)];
        let media = MediaMetadata {
            title: "Show".into(),
            kind: Some(MediaKind::Tv),
            season: Some(2),
            ..Default::default()
        };
        let cfg = Config {
            tv_auto: false,
            ..Config::default()
        };
        let plan = plan(&cfg, &titles, &media, Path::new("Show.mkv")).unwrap();
        assert_eq!(plan.outputs.len(), 1);
        assert_eq!(plan.outputs[0].filename, "Show.mkv");
    }

    // The engine's contract: a complete roster naming one episode is one output; one
    // naming none proves nothing to select and is held (engine bbe7fd8), never guessed.
    #[test]
    fn authored_one_episode_roster_is_one_output_and_an_empty_one_is_held() {
        let cfg = Config::default();
        let media = MediaMetadata {
            kind: Some(MediaKind::Tv),
            ..Default::default()
        };
        let disc = |members: usize| {
            let mut titles = vec![title(5700.0, 1000), title(90.0, 5)];
            for (index, title) in titles.iter_mut().enumerate() {
                title.playlist_id = index as u16;
                title.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
                    roster: "test-menu".into(),
                    title_count: 2,
                    ordinal: (index < members).then_some(index),
                };
            }
            titles
        };
        let plan_one = plan(&cfg, &disc(1), &media, Path::new("Show.mkv")).unwrap();
        assert_eq!(plan_one.outputs.len(), 1);
        assert_eq!(plan_one.outputs[0].filename, "Show.mkv");
        assert_eq!(plan_one.outputs[0].episode, None);
        assert!(matches!(
            plan(&cfg, &disc(0), &media, Path::new("Show.mkv")),
            Err(PlanError::Selection(confirm))
                if confirm.reason == freemkv_engine::SelectionReviewReason::IncompleteEpisodeRoster
        ));
    }

    fn authored_two_episode_disc() -> Vec<DiscTitle> {
        let mut titles = vec![title(2700.0, 100), title(2700.0, 200)];
        for (index, t) in titles.iter_mut().enumerate() {
            t.playlist_id = index as u16;
            t.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
                roster: "test-menu".into(),
                title_count: 2,
                ordinal: Some(index),
            };
        }
        titles
    }

    #[test]
    fn show_title_never_supplies_the_disc_number() {
        // The disc number comes from the ISO's labels (resolved by the caller),
        // never from the TMDB show title.
        let cfg = Config::default();
        let media = MediaMetadata {
            title: "Show Disc 2".into(),
            kind: Some(MediaKind::Tv),
            season: Some(1),
            ..Default::default()
        };
        let plan = plan(
            &cfg,
            &authored_two_episode_disc(),
            &media,
            Path::new("Show.mkv"),
        )
        .unwrap();
        assert_eq!(plan.outputs[0].episode, Some(1));
    }

    #[test]
    fn plan_errors_render_as_localized_prose_not_debug() {
        for error in [
            PlanError::NoTitles,
            PlanError::NoOutputs,
            PlanError::DuplicateOutput("a.mkv".into()),
            PlanError::Selection(EpisodeConfirm {
                reason: freemkv_engine::SelectionReviewReason::MissingEpisodeRoster,
                suggested: vec![0, 1],
                clusters: vec![vec![0, 1]],
                alternates: Vec::new(),
            }),
            PlanError::PickMismatch,
            PlanError::EpisodeRange,
        ] {
            let text = error.to_string();
            assert!(!text.contains("Selection"), "{text}");
            assert!(!text.contains("error.plan"), "{text}");
            assert!(!text.contains("missing-episode-roster"), "{text}");
            assert!(text.contains(' '), "{text}");
        }
        assert!(
            PlanError::DuplicateOutput("a.mkv".into())
                .to_string()
                .contains("a.mkv")
        );
        assert!(
            PlanError::EpisodeRange
                .with_action(PlanError::EpisodeRange.to_string())
                .contains("First episode")
        );
    }
}

#[cfg(test)]
#[path = "planner_resolve_tests.rs"]
mod resolve_tests;
