//! Immutable source-to-output planning shared by autorip and Library remux.
//!
//! The planner decides *what* a source produces.  Executors only consume the
//! resulting plan and must never redo TMDB lookup or episode assignment.

use crate::server::config::Config;
use libfreemkv::DiscTitle;
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    pub media: MediaMetadata,
    pub outputs: Vec<PlannedOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    NoTitles,
    NoOutputs,
    DuplicateOutput(String),
    /// The engine's episode selection needs review (E9086).
    Selection(freemkv_engine::SelectionReview),
    /// A later disc's episode numbering has no confirmed First episode.
    LaterDiscStart,
    /// The episode numbers would run past the supported range.
    EpisodeRange,
}

impl PlanError {
    /// What the operator can do about a held selection in the server UI, if anything.
    pub(crate) fn action(&self) -> Option<String> {
        use crate::strings::get_or;
        match self {
            PlanError::Selection(_) => Some(get_or(
                "error.plan.action.roster",
                "If it is a film, correct its match to the movie; to rip it as one title, turn off \"Rip TV seasons automatically\" in Settings.",
            )),
            PlanError::LaterDiscStart | PlanError::EpisodeRange => Some(get_or(
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
            PlanError::LaterDiscStart => get_or(
                "error.plan.later_disc_start",
                "Later-disc episode numbering needs a confirmed First episode; disc number and runtimes do not establish it.",
            ),
            PlanError::EpisodeRange => get_or(
                "error.plan.episode_range",
                "Episode numbering is outside the supported range.",
            ),
            PlanError::Selection(review) => crate::strings::selection_review_text(review),
        };
        f.write_str(&text)
    }
}

/// What `media` produces from `titles`: one output per episode when `cfg.tv_auto`
/// finds a TV episode set, else the main title as `movie_filename`'s leaf.
pub fn plan(
    cfg: &Config,
    titles: &[DiscTitle],
    media: &MediaMetadata,
    movie_filename: &Path,
) -> Result<RemuxPlan, PlanError> {
    check_episode_selection(titles, cfg, media.kind)?;
    let indices = episode_indices(titles, cfg, media.kind);
    // A TV movie or a single detected feature remains one output.
    if indices.len() <= 1 {
        let title_index = main_title(cfg, titles)?;
        return Ok(single_plan(media, title_index, leaf(movie_filename)));
    }
    let season = media.season.unwrap_or(1);
    // The disc number is the caller's (from the ISO labels), never the show title.
    let start =
        confirmed_episode_start(media.disc.unwrap_or(1), indices.len(), media.episode_start)?;
    // Validate numbering before the blocking TMDB fetch.
    let episodes = crate::server::tmdb::season_episodes(media.tmdb_id, season, &cfg.tmdb_api_key);
    let outputs =
        episode_outputs_from_start(titles, &indices, season, start, &episodes, movie_filename);
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

/// Automatic fan-out is a TV-only policy shared by capture and remux.
pub(crate) fn check_episode_selection(
    titles: &[DiscTitle],
    cfg: &Config,
    kind: Option<MediaKind>,
) -> Result<(), PlanError> {
    if !cfg.tv_auto || kind != Some(MediaKind::Tv) || titles.len() <= 1 {
        return Ok(());
    }
    freemkv_engine::SelectionModel::from_titles(titles)
        .select(
            &freemkv_engine::Selection::Episodes,
            &freemkv_engine::StreamFilter::All,
            presentation(cfg),
        )
        .map(|_| ())
        .map_err(PlanError::Selection)
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
        .select(
            &freemkv_engine::Selection::Episodes,
            &freemkv_engine::StreamFilter::All,
            presentation(cfg),
        )
        .unwrap_or_default();
    if indices.len() > 1 {
        indices
    } else {
        Vec::new()
    }
}

fn presentation(cfg: &Config) -> Option<&str> {
    Some(cfg.presentation_language.as_str()).filter(|language| !language.trim().is_empty())
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
    let start = explicit
        .or((disc <= 1).then_some(1))
        .ok_or(PlanError::LaterDiscStart)?;
    if start == 0
        || count == 0
        || usize::from(start)
            .checked_add(count - 1)
            .is_none_or(|last| last > usize::from(u16::MAX))
    {
        return Err(PlanError::EpisodeRange);
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
    fn uneven_disc_split_uses_confirmed_start_not_runtime_guess() {
        assert_eq!(confirmed_episode_start(1, 3, None), Ok(1));
        assert!(matches!(
            confirmed_episode_start(2, 2, None),
            Err(PlanError::LaterDiscStart)
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
                    ordinal: (index < members).then_some(index),
                };
            }
            let plan = plan(&cfg, &titles, &media, Path::new("Show.mkv")).unwrap();
            assert_eq!(plan.outputs.len(), 1);
            assert_eq!(plan.outputs[0].filename, "Show.mkv");
            assert_eq!(plan.outputs[0].episode, None);
        }
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
            PlanError::Selection(freemkv_engine::SelectionReview {
                reason: freemkv_engine::SelectionReviewReason::MissingEpisodeRoster,
                candidates: vec![0],
            }),
            PlanError::LaterDiscStart,
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
            confirmed_episode_start(2, 2, None)
                .unwrap_err()
                .to_string()
                .contains("First episode")
        );
    }
}
