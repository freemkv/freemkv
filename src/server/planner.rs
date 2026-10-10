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

pub struct MoviePlanner;

impl Planner for MoviePlanner {
    fn plan(
        &self,
        source_iso: &Path,
        titles: &[DiscTitle],
        media: &MediaMetadata,
        movie_filename: &Path,
    ) -> Result<RemuxPlan, PlanError> {
        let title_index = titles.first().map(|_| 0).ok_or(PlanError::NoTitles)?;
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
        let kind_is_tv = media.kind == Some(MediaKind::Tv);
        let label = if media.season.is_some() {
            format!("{} Season {}", media.title, media.season.unwrap_or(1))
        } else {
            media.title.clone()
        };
        let indices = if self.cfg.tv_auto && kind_is_tv {
            freemkv_engine::episode_titles(titles)
        } else {
            Vec::new()
        };
        // A TV movie, unknown season, or a single detected feature remains a
        // normal one-output job by policy.
        if indices.len() <= 1 {
            return MoviePlanner.plan(source_iso, titles, media, movie_filename);
        }
        let season = media
            .season
            .or_else(|| crate::server::tmdb::season_from_label(&label))
            .unwrap_or(1);
        let episodes =
            crate::server::tmdb::season_episodes(media.tmdb_id, season, &self.cfg.tmdb_api_key);
        let runtimes: Vec<f64> = indices.iter().map(|&i| titles[i].duration_secs).collect();
        let fallback = 1u16.saturating_add(
            media
                .disc
                .unwrap_or(1)
                .saturating_sub(1)
                .saturating_mul(indices.len() as u16),
        );
        let start = crate::server::tmdb::align_disc_offset(&runtimes, &episodes, fallback);
        let assignments = crate::server::tmdb::map_episodes(&runtimes, &episodes, start);
        let stem = movie_filename
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("title");
        let ext = movie_filename
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("mkv");
        let outputs = indices
            .iter()
            .zip(assignments)
            .map(|(&idx, a)| PlannedOutput {
                id: format!("title-{idx}-episode-{}", a.episode),
                title_index: idx,
                episode: Some(a.episode),
                episode_name: a.name,
                filename: format!("{stem}_S{season:02}E{:02}.{ext}", a.episode),
            })
            .collect();
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
        Box::new(MoviePlanner)
    }
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
    fn movie_planner_always_returns_one_output() {
        let titles = vec![title(5_000.0, 100), title(120.0, 5)];
        let media = MediaMetadata {
            title: "Film".into(),
            kind: Some(MediaKind::Movie),
            ..Default::default()
        };
        let plan = MoviePlanner
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
}
