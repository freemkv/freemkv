//! The episode plan a drive card lists while a TV disc rips: the same capture plan the
//! mux will commit, kept per job so the card can show it from the first pass on. A title
//! correction replans it from the scan it was planned from, as the mux will.

use crate::server::config::Config;
use crate::server::planner::MediaKind;
use std::sync::Mutex;

/// One planned episode as the drive card shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CardEpisode {
    pub season: u16,
    pub episode: u16,
    /// TMDB episode name; empty when TMDB has none for it.
    pub name: String,
    pub duration: String,
}

// What a correction needs to plan the job again: the scan and the names it planned under.
struct Planned {
    job: String,
    titles: Vec<libfreemkv::DiscTitle>,
    label: String,
    filename: String,
    episodes: Vec<CardEpisode>,
}

// Most recent jobs last; a handful covers every drive a server runs.
const KEEP: usize = 8;
static PLANS: Mutex<Vec<Planned>> = Mutex::new(Vec::new());

/// The card rows for a plan: one per episode output, or none for a single feature.
pub(crate) fn card_episodes(
    titles: &[libfreemkv::DiscTitle],
    label: &str,
    outputs: &[super::staging::Output],
) -> Vec<CardEpisode> {
    if outputs.len() < 2 {
        return Vec::new();
    }
    let season = crate::server::tmdb::season_from_label(label).unwrap_or(1);
    // An output without its episode number makes it no episode plan.
    let rows: Option<Vec<_>> = outputs
        .iter()
        .map(|o| {
            Some(CardEpisode {
                season,
                episode: o.episode?,
                name: o.episode_name.clone(),
                duration: titles
                    .get(o.title_index)
                    .map(|t| crate::server::util::format_duration_hm(t.duration_secs))
                    .unwrap_or_default(),
            })
        })
        .collect();
    rows.unwrap_or_default()
}

/// Record `job`'s capture plan for its card.
pub(crate) fn publish(
    job: &str,
    titles: &[libfreemkv::DiscTitle],
    label: &str,
    filename: &str,
    outputs: &[super::staging::Output],
) {
    let episodes = card_episodes(titles, label, outputs);
    let mut plans = PLANS.lock().unwrap_or_else(|e| e.into_inner());
    plans.retain(|p| p.job != job);
    plans.push(Planned {
        job: job.to_string(),
        titles: titles.to_vec(),
        label: label.to_string(),
        filename: filename.to_string(),
        episodes,
    });
    let over = plans.len().saturating_sub(KEEP);
    plans.drain(..over);
}

/// The episodes `job`'s card lists, if it is an episode plan.
pub(crate) fn episodes(job: &str) -> Option<Vec<CardEpisode>> {
    let plans = PLANS.lock().unwrap_or_else(|e| e.into_inner());
    plans
        .iter()
        .find(|p| p.job == job)
        .map(|p| p.episodes.clone())
        .filter(|e| !e.is_empty())
}

/// Plan `job` again under a correction, off the request thread (the season fetch blocks).
pub(crate) fn replan(cfg: Config, job: &str, kind: MediaKind, tmdb_id: u64, start: Option<u16>) {
    let job = job.to_string();
    let inputs = {
        let plans = PLANS.lock().unwrap_or_else(|e| e.into_inner());
        plans
            .iter()
            .find(|p| p.job == job)
            .map(|p| (p.titles.clone(), p.label.clone(), p.filename.clone()))
    };
    let Some((titles, label, filename)) = inputs else {
        return;
    };
    std::thread::spawn(move || {
        // A Movie correction plans no episodes, whatever the label says.
        let label = super::planning_label(&label, Some(kind)).to_string();
        let plan = super::plan_mux_outputs(&titles, &cfg, kind, &label, tmdb_id, &filename, start);
        let episodes = plan
            .map(|outputs| card_episodes(&titles, &label, &outputs))
            .unwrap_or_default();
        let mut plans = PLANS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = plans.iter_mut().find(|p| p.job == job) {
            p.episodes = episodes;
        }
    });
}

#[cfg(test)]
#[path = "card_plan_tests.rs"]
mod tests;
