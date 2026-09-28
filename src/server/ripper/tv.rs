//! TV episode-title selection.
//!
//! A TV disc lists every title: the episodes, usually a "play all" title whose
//! runtime is the SUM of the episodes, plus extras/menus and sometimes duplicate
//! angles. autorip's movie path takes `titles[0]`; for TV we instead take the
//! *episode cluster* — the group of similar-length titles — so every episode is
//! ripped and the play-all/extras are dropped.

use libfreemkv::DiscTitle;
use std::collections::HashSet;

/// Select the episode titles from a TV disc's full title list, in disc order.
/// Returns indices into `titles`. Drops the "play all" sum-title, extras/menus
/// (far from the episode-length cluster), and duplicate-content titles.
pub fn select_episode_titles(titles: &[DiscTitle], min_len_secs: u64) -> Vec<usize> {
    let durations: Vec<f64> = titles.iter().map(|t| t.duration_secs).collect();
    let cluster = episode_cluster(&durations, min_len_secs as f64);
    dedup_by_content(titles, cluster)
}

// Indices whose duration sits in the modal episode-length cluster.
fn episode_cluster(durations: &[f64], min_len: f64) -> Vec<usize> {
    let cands: Vec<usize> = durations
        .iter()
        .enumerate()
        .filter(|(_, d)| **d >= min_len)
        .map(|(i, _)| i)
        .collect();
    if cands.len() <= 1 {
        return cands;
    }
    let mut lens: Vec<f64> = cands.iter().map(|&i| durations[i]).collect();
    lens.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = lens[lens.len() / 2];
    let tol = (median * 0.25).max(300.0);
    cands
        .into_iter()
        .filter(|&i| (durations[i] - median).abs() <= tol)
        .collect()
}

/// Drop titles whose content duplicates an already-kept one — DVD angles or
/// redundant playlists that point at the same programme (same first-extent
/// start LBA and duration). Keeps the first, preserving disc order.
fn dedup_by_content(titles: &[DiscTitle], indices: Vec<usize>) -> Vec<usize> {
    let mut seen = HashSet::new();
    indices
        .into_iter()
        .filter(|&i| {
            let t = &titles[i];
            let key = (
                t.extents.first().map(|e| e.start_lba).unwrap_or(0),
                t.duration_secs.round() as i64,
            );
            seen.insert(key)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfreemkv::disc::{ContentFormat, Extent};

    fn title(dur_secs: f64, start_lba: u32) -> DiscTitle {
        DiscTitle {
            playlist: String::new(),
            playlist_id: 0,
            duration_secs: dur_secs,
            size_bytes: (dur_secs as u64) * 1_000_000,
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
    fn picks_the_episode_cluster_and_drops_play_all_and_extras() {
        // 6 × ~44-min episodes, one ~264-min "play all" (their sum), one 2-min
        // menu/extra. Min length 20 min drops the extra outright; the play-all
        // is far above the median and dropped by the cluster tolerance.
        let ep = 44.0 * 60.0;
        let mut titles = vec![title(ep * 6.0, 100)]; // play all, index 0
        for k in 0..6 {
            titles.push(title(ep + (k as f64), 1000 + k * 100)); // episodes
        }
        titles.push(title(2.0 * 60.0, 50)); // extra
        let got = select_episode_titles(&titles, 20 * 60);
        assert_eq!(
            got,
            vec![1, 2, 3, 4, 5, 6],
            "exactly the six episodes, in order"
        );
    }

    #[test]
    fn dedups_duplicate_angle_titles() {
        let ep = 44.0 * 60.0;
        // Two identical-content titles (same start LBA + duration) + one distinct.
        let titles = vec![title(ep, 1000), title(ep, 1000), title(ep, 2000)];
        let got = select_episode_titles(&titles, 20 * 60);
        assert_eq!(got, vec![0, 2], "the duplicate of title 0 is dropped");
    }

    #[test]
    fn single_qualifying_title_passes_through() {
        // A disc with one long title (e.g. a TV movie) → that one title.
        let titles = vec![title(90.0 * 60.0, 1000), title(60.0, 50)];
        assert_eq!(select_episode_titles(&titles, 20 * 60), vec![0]);
    }

    #[test]
    fn none_qualify_yields_empty() {
        let titles = vec![title(120.0, 10), title(90.0, 20)];
        assert!(select_episode_titles(&titles, 20 * 60).is_empty());
    }
}
