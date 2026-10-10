//! Immutable source-to-output planning shared by autorip and Library remux.
//!
//! Planners decide *what* a source produces.  Executors only consume the
//! resulting plan and must never redo TMDB lookup or episode assignment.

use crate::server::config::Config;
use libfreemkv::DiscTitle;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const PLAN_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Movie,
    Tv,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaMetadata {
    pub title: String,
    pub year: u16,
    pub tmdb_id: u64,
    pub season: Option<u16>,
    pub disc: Option<u16>,
    #[serde(default)]
    pub episode_start: Option<u16>,
    pub kind: Option<MediaKind>,
}

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
    pub version: u32,
    pub source_iso: PathBuf,
    pub media: MediaMetadata,
    pub outputs: Vec<PlannedOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    NoTitles,
    NoOutputs,
    DuplicateOutput(String),
    SelectionNeedsReview(String),
}

pub trait Planner {
    fn plan(
        &self,
        source_iso: &Path,
        titles: &[DiscTitle],
        media: &MediaMetadata,
        movie_filename: &Path,
    ) -> Result<RemuxPlan, PlanError>;
}

#[derive(Default)]
pub struct MoviePlanner {
    pub preferences: freemkv_engine::SelectionPreferences,
}

impl MoviePlanner {
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            preferences: selection_preferences(cfg),
        }
    }

    pub(crate) fn title_index(&self, titles: &[DiscTitle]) -> Result<usize, PlanError> {
        let report = freemkv_engine::SelectionModel::from_titles(titles).select_with_preferences(
            &freemkv_engine::Selection::MainMovie,
            &freemkv_engine::StreamFilter::All,
            &self.preferences,
        );
        if let Some(reason) = report.review_reason {
            return Err(PlanError::SelectionNeedsReview(reason.key().into()));
        }
        report.indices.first().copied().ok_or(PlanError::NoTitles)
    }
}

impl Planner for MoviePlanner {
    fn plan(
        &self,
        source_iso: &Path,
        titles: &[DiscTitle],
        media: &MediaMetadata,
        movie_filename: &Path,
    ) -> Result<RemuxPlan, PlanError> {
        let title_index = self.title_index(titles)?;
        let filename = leaf(movie_filename);
        Ok(single_plan(source_iso, media, title_index, filename))
    }
}

pub struct TvPlanner<'a> {
    pub cfg: &'a Config,
}

impl Planner for TvPlanner<'_> {
    fn plan(
        &self,
        source_iso: &Path,
        titles: &[DiscTitle],
        media: &MediaMetadata,
        movie_filename: &Path,
    ) -> Result<RemuxPlan, PlanError> {
        let label = if media.season.is_some() {
            format!("{} Season {}", media.title, media.season.unwrap_or(1))
        } else {
            media.title.clone()
        };
        check_episode_selection(titles, self.cfg, media.kind)?;
        let indices = episode_indices(titles, self.cfg, media.kind);
        // A TV movie or a single detected feature remains one output.
        if indices.len() <= 1 {
            return MoviePlanner {
                preferences: selection_preferences(self.cfg),
            }
            .plan(source_iso, titles, media, movie_filename);
        }
        let season = media
            .season
            .or_else(|| crate::server::tmdb::season_from_label(&label))
            .unwrap_or(1);
        let episodes =
            crate::server::tmdb::season_episodes(media.tmdb_id, season, &self.cfg.tmdb_api_key);
        let disc_number = media
            .disc
            .or_else(|| crate::server::tmdb::disc_from_label(&media.title))
            .unwrap_or(1);
        let start = confirmed_episode_start(disc_number, indices.len(), media.episode_start)?;
        let outputs =
            episode_outputs_from_start(titles, &indices, season, start, &episodes, movie_filename);
        validate_outputs(outputs).map(|outputs| RemuxPlan {
            version: PLAN_VERSION,
            source_iso: source_iso.to_path_buf(),
            media: media.clone(),
            outputs,
        })
    }
}

pub fn planner_for<'a>(cfg: &'a Config, media: &MediaMetadata) -> Box<dyn Planner + 'a> {
    if media.kind == Some(MediaKind::Tv) {
        Box::new(TvPlanner { cfg })
    } else {
        Box::new(MoviePlanner {
            preferences: selection_preferences(cfg),
        })
    }
}

/// Automatic fan-out is a TV-only policy shared by capture and remux.
pub(crate) fn check_episode_selection(
    titles: &[DiscTitle],
    cfg: &Config,
    kind: Option<MediaKind>,
) -> Result<(), PlanError> {
    if !cfg.tv_auto || kind != Some(MediaKind::Tv) || titles.len() <= 1 {
        return Ok(());
    }
    let report = freemkv_engine::SelectionModel::from_titles(titles).select_with_preferences(
        &freemkv_engine::Selection::Episodes,
        &freemkv_engine::StreamFilter::All,
        &selection_preferences(cfg),
    );
    if let Some(reason) = report.review_reason {
        return Err(PlanError::SelectionNeedsReview(reason.key().into()));
    }
    Ok(())
}

pub(crate) fn episode_indices(
    titles: &[DiscTitle],
    cfg: &Config,
    kind: Option<MediaKind>,
) -> Vec<usize> {
    if !cfg.tv_auto || kind != Some(MediaKind::Tv) {
        return Vec::new();
    }
    let indices = freemkv_engine::SelectionModel::from_titles(titles)
        .select_with_preferences(
            &freemkv_engine::Selection::Episodes,
            &freemkv_engine::StreamFilter::All,
            &selection_preferences(cfg),
        )
        .indices;
    if indices.len() > 1 {
        indices
    } else {
        Vec::new()
    }
}

fn selection_preferences(cfg: &Config) -> freemkv_engine::SelectionPreferences {
    freemkv_engine::SelectionPreferences {
        presentation_language: (!cfg.presentation_language.trim().is_empty())
            .then(|| cfg.presentation_language.clone()),
    }
}

/// One episode assignment and staging-name policy for capture and remux.
pub(crate) fn episode_outputs(
    titles: &[DiscTitle],
    indices: &[usize],
    season: u16,
    disc: u16,
    episodes: &[crate::server::tmdb::Episode],
    movie_filename: &Path,
    episode_start: Option<u16>,
) -> Result<Vec<PlannedOutput>, PlanError> {
    let start = confirmed_episode_start(disc, indices.len(), episode_start)?;
    Ok(episode_outputs_from_start(
        titles,
        indices,
        season,
        start,
        episodes,
        movie_filename,
    ))
}

fn confirmed_episode_start(
    disc: u16,
    count: usize,
    explicit: Option<u16>,
) -> Result<u16, PlanError> {
    let start = explicit.or((disc <= 1).then_some(1)).ok_or_else(|| PlanError::SelectionNeedsReview(
        "Later-disc episode numbering needs a confirmed First episode; disc number and runtimes do not establish it.".into()
    ))?;
    if start == 0
        || count == 0
        || usize::from(start)
            .checked_add(count - 1)
            .is_none_or(|last| last > usize::from(u16::MAX))
    {
        return Err(PlanError::SelectionNeedsReview(
            "Episode numbering is outside the supported range.".into(),
        ));
    }
    Ok(start)
}

fn episode_outputs_from_start(
    titles: &[DiscTitle],
    indices: &[usize],
    season: u16,
    start: u16,
    episodes: &[crate::server::tmdb::Episode],
    movie_filename: &Path,
) -> Vec<PlannedOutput> {
    let runtimes: Vec<f64> = indices.iter().map(|&i| titles[i].duration_secs).collect();
    let assignments = crate::server::tmdb::map_episodes(&runtimes, episodes, start);
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

fn single_plan(
    source_iso: &Path,
    media: &MediaMetadata,
    title_index: usize,
    filename: String,
) -> RemuxPlan {
    RemuxPlan {
        version: PLAN_VERSION,
        source_iso: source_iso.to_path_buf(),
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
        .unwrap_or_else(|| PathBuf::from("title.mkv").to_string_lossy().into_owned())
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
    fn uneven_disc_split_uses_confirmed_start_not_runtime_guess() {
        assert_eq!(confirmed_episode_start(1, 3, None), Ok(1));
        assert!(matches!(
            confirmed_episode_start(2, 2, None),
            Err(PlanError::SelectionNeedsReview(_))
        ));
        let start = confirmed_episode_start(2, 2, Some(4)).unwrap();
        let titles = vec![title(50.0 * 60.0, 100), title(50.0 * 60.0, 200)];
        let episodes = (1..=5)
            .map(|number| crate::server::tmdb::Episode {
                number,
                name: format!("Episode {number}"),
                runtime_min: if number < 4 { 50 } else { 51 },
            })
            .collect::<Vec<_>>();
        let outputs = episode_outputs_from_start(
            &titles,
            &[0, 1],
            1,
            start,
            &episodes,
            Path::new("Show.mkv"),
        );
        assert_eq!(
            outputs.iter().map(|o| o.episode).collect::<Vec<_>>(),
            [Some(4), Some(5)]
        );
        assert_eq!(outputs[0].episode_name, "Episode 4");
        assert_eq!(outputs[1].filename, "Show_S01E05.mkv");
        assert!(confirmed_episode_start(2, 2, Some(u16::MAX)).is_err());
        assert!(confirmed_episode_start(2, 2, Some(0)).is_err());
    }

    #[test]
    fn ambiguous_tv_selection_never_silently_becomes_a_movie() {
        let cfg = Config::default();
        let titles = vec![title(2700.0, 100), title(2700.0, 200)];
        let media = MediaMetadata {
            kind: Some(MediaKind::Tv),
            ..Default::default()
        };
        let result = TvPlanner { cfg: &cfg }.plan(
            Path::new("disc.iso"),
            &titles,
            &media,
            Path::new("show.mkv"),
        );
        assert!(matches!(result, Err(PlanError::SelectionNeedsReview(_))));
    }

    #[test]
    fn movie_planner_always_returns_one_output() {
        let titles = vec![title(5_000.0, 100), title(120.0, 5)];
        let media = MediaMetadata {
            title: "Film".into(),
            kind: Some(MediaKind::Movie),
            ..Default::default()
        };
        let plan = MoviePlanner::default()
            .plan(
                Path::new("disc.iso"),
                &titles,
                &media,
                Path::new("Film.mkv"),
            )
            .unwrap();
        assert_eq!(plan.outputs.len(), 1);
        assert_eq!(plan.source_iso, Path::new("disc.iso"));
        assert_eq!(plan.outputs[0].title_index, 0);
        assert_eq!(plan.outputs[0].filename, "Film.mkv");
    }

    #[test]
    fn presentation_language_library_freezes_authored_choice_or_requires_review() {
        let titles = crate::selection_test_fixtures::launch_titles();
        let media = MediaMetadata {
            kind: Some(MediaKind::Movie),
            ..Default::default()
        };
        for (language, expected) in [("de", 1), ("en", 0)] {
            let cfg = Config {
                presentation_language: language.into(),
                ..Default::default()
            };
            let plan = planner_for(&cfg, &media)
                .plan(
                    Path::new("disc.iso"),
                    &titles,
                    &media,
                    Path::new("movie.mkv"),
                )
                .unwrap();
            assert_eq!(plan.outputs[0].title_index, expected);
        }
        for language in ["", "fr", "invalid-language"] {
            let cfg = Config {
                presentation_language: language.into(),
                ..Default::default()
            };
            assert!(matches!(
                planner_for(&cfg, &media).plan(
                    Path::new("disc.iso"),
                    &titles,
                    &media,
                    Path::new("movie.mkv")
                ),
                Err(PlanError::SelectionNeedsReview(_))
            ));
        }
    }

    #[test]
    fn movie_planner_preserves_shared_selection_and_empty_source_error() {
        for titles in [vec![], vec![title(120.0, 5), title(5000.0, 100)]] {
            let selected = freemkv_engine::SelectionModel::from_titles(&titles).select(
                &freemkv_engine::Selection::MainMovie,
                &freemkv_engine::StreamFilter::All,
            );
            let planned = MoviePlanner::default().plan(
                Path::new("disc.iso"),
                &titles,
                &MediaMetadata::default(),
                Path::new("film.mkv"),
            );
            if selected.indices.is_empty() {
                assert_eq!(planned, Err(PlanError::NoTitles));
            } else {
                assert_eq!(planned.unwrap().outputs[0].title_index, selected.indices[0]);
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
                member: true,
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
        let plan = planner_for(&cfg, &media)
            .plan(
                Path::new("disc.iso"),
                &titles,
                &media,
                Path::new("Show.mkv"),
            )
            .unwrap();
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
                    clip_id: episode_name.to_ascii_lowercase(),
                    in_time: 0,
                    out_time: (episode * 45_000.0) as u32,
                    duration_secs: episode,
                    source_packets: 0,
                    feed_span: None,
                }];
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
                member: (1..=6).contains(&index),
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
        let plan = TvPlanner { cfg: &cfg }
            .plan(
                Path::new("disc.iso"),
                &titles,
                &media,
                Path::new("Endeavour.mkv"),
            )
            .unwrap();
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
        let plan = TvPlanner { cfg: &cfg }
            .plan(
                Path::new("disc.iso"),
                &titles,
                &media,
                Path::new("Show.mkv"),
            )
            .unwrap();
        assert_eq!(plan.outputs.len(), 1);
        assert_eq!(plan.outputs[0].filename, "Show.mkv");
    }

    #[test]
    fn complete_authored_zero_or_one_episode_roster_keeps_single_output_policy() {
        let cfg = Config::default();
        let media = MediaMetadata {
            kind: Some(MediaKind::Tv),
            ..Default::default()
        };
        for members in [0, 1] {
            let mut titles = vec![title(5700.0, 1000), title(90.0, 5)];
            for (index, title) in titles.iter_mut().enumerate() {
                title.playlist_id = index as u16;
                title.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
                    roster: "test-menu".into(),
                    title_count: 2,
                    member: index < members,
                    ordinal: (index < members).then_some(index),
                };
            }
            let plan = TvPlanner { cfg: &cfg }
                .plan(
                    Path::new("disc.iso"),
                    &titles,
                    &media,
                    Path::new("Show.mkv"),
                )
                .unwrap();
            assert_eq!(plan.outputs.len(), 1);
            assert_eq!(plan.outputs[0].filename, "Show.mkv");
            assert_eq!(plan.outputs[0].episode, None);
        }
    }
}
