use super::*;
use libfreemkv::disc::{ContentFormat, Extent};

fn title(dur_secs: f64, start_lba: u32) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
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

// A movie disc yields exactly one output — the main-title staging leaf,
// untouched — so the movie path stays byte-identical.
#[test]
fn movie_yields_a_single_untouched_output() {
    let cfg = Config::default();
    let titles = vec![title(6000.0, 100), title(120.0, 5)];
    let plan = plan_mux_outputs(&titles, &cfg, "movie", "The Matrix", 0, "The Matrix.mkv");
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "The Matrix.mkv");
    assert_eq!(plan[0].title_index, 0);
    assert!(plan[0].episode.is_none());
}

// A season-labelled TV disc fans out to one output per episode title, in disc
// order, numbered sequentially from E01 (no TMDB key configured → sequential),
// keeping the source stem + extension and dropping the play-all/extra.
#[test]
fn tv_disc_fans_out_one_output_per_episode() {
    let cfg = Config::default(); // tv_auto = true, empty tmdb key
    let ep = 44.0 * 60.0;
    let mut titles = vec![title(ep * 6.0, 100)]; // play-all sum-title
    for k in 0..6 {
        titles.push(title(ep + k as f64, 1000 + k * 100)); // 6 episodes
    }
    titles.push(title(90.0, 5)); // extra
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        "tv",
        "Endeavour Season 5",
        44264,
        "Endeavour.mkv",
    );
    assert_eq!(plan.len(), 6, "one output per episode title");
    let episodes: Vec<u16> = plan.iter().map(|o| o.episode.unwrap()).collect();
    assert_eq!(episodes, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(plan[0].filename, "Endeavour_S05E01.mkv");
    assert_eq!(plan[5].filename, "Endeavour_S05E06.mkv");
    // The title indices are the episode cluster (1..=6), not the play-all(0)
    // or the extra(7).
    assert_eq!(
        plan.iter().map(|o| o.title_index).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5, 6]
    );
}

// Preflight must reserve every fanned-out episode MKV (all written to staging
// before the ISO is pruned), not just the selected title (issue: ENOSPC mid-mux).
#[test]
fn mux_reserve_sums_every_planned_episode() {
    let cfg = Config::default(); // tv_auto = true
    let ep = 44.0 * 60.0;
    let titles: Vec<_> = (0..6)
        .map(|k| title(ep + k as f64, 1000 + k * 100))
        .collect();
    let sum: u64 = titles.iter().map(|t| t.size_bytes).sum();
    let selected = titles[0].size_bytes;
    let plan = plan_mux_outputs(&titles, &cfg, "tv", "Show Season 1", 0, "Show.mkv");
    let planned: u64 = plan.iter().map(|o| titles[o.title_index].size_bytes).sum();
    assert_eq!(planned, sum, "plan muxes all six episodes");
    assert_eq!(
        mux_output_reserve_bytes(&titles, &cfg, "tv", "Show Season 1", selected),
        planned,
        "reserve equals the sum of planned outputs"
    );
    // Play-all selected: the plan never muxes it, so reserve the episode sum.
    let mut with_playall = vec![title(ep * 7.0, 50)];
    with_playall.extend(titles.iter().cloned());
    let big = with_playall[0].size_bytes;
    assert!(big > sum);
    assert_eq!(
        mux_output_reserve_bytes(&with_playall, &cfg, "tv", "Show Season 1", big),
        sum
    );
    // ISO output / network sink: nothing muxed lands in staging.
    for (fmt, target) in [
        (crate::server::config::OUTPUT_FORMAT_ISO, ""),
        (
            crate::server::config::OUTPUT_FORMAT_NETWORK,
            "sink.example:9000",
        ),
    ] {
        let c = Config {
            output_format: fmt.to_string(),
            network_target: target.to_string(),
            ..Config::default()
        };
        assert_eq!(
            mux_output_reserve_bytes(&titles, &c, "tv", "Show Season 1", selected),
            0,
            "{fmt}"
        );
    }
    // Movie / tv_auto off: one output, the selected title.
    assert_eq!(
        mux_output_reserve_bytes(&titles, &cfg, "movie", "Some Film", selected),
        selected
    );
    let off = Config {
        tv_auto: false,
        ..Config::default()
    };
    assert_eq!(
        mux_output_reserve_bytes(&titles, &off, "tv", "Show Season 1", selected),
        selected
    );
}

// Disc 2 of a multi-disc season starts numbering where disc 1 left off
// (best-effort uniform split), so its episodes don't collide with disc 1's.
#[test]
fn multi_disc_season_offsets_episode_numbers() {
    let cfg = Config::default();
    let ep = 44.0 * 60.0;
    // 4 episode titles on disc 2 (label carries "Disc 2").
    let titles: Vec<_> = (0..4)
        .map(|k| title(ep + k as f64, 1000 + k * 100))
        .collect();
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        "tv",
        "Endeavour Season 5 Disc 2",
        44264,
        "Endeavour.mkv",
    );
    assert_eq!(plan.len(), 4);
    // disc 2, 4 eps/disc → start at E05.
    assert_eq!(
        plan.iter().map(|o| o.episode.unwrap()).collect::<Vec<_>>(),
        vec![5, 6, 7, 8],
        "disc 2 numbers from E05, not colliding with disc 1's E01..E04"
    );
    assert_eq!(plan[0].filename, "Endeavour_S05E05.mkv");
    assert_eq!(plan[3].filename, "Endeavour_S05E08.mkv");
}

// A single feature carrying a TV label (a TV movie) is one movie-style output, not E01.
#[test]
fn a_single_feature_tv_disc_stays_one_output() {
    let cfg = Config::default(); // tv_auto = true
    let titles = vec![title(95.0 * 60.0, 1000), title(90.0, 5)];
    let plan = plan_mux_outputs(&titles, &cfg, "tv", "Show Season 2", 0, "Show.mkv");
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "Show.mkv");
    assert!(plan[0].episode.is_none());
}

// TMDB runtimes pin an unevenly split disc to its true episodes (not the uniform-split
// guess), and each matched episode carries its TMDB name.
#[test]
fn tmdb_runtimes_align_the_offset_and_name_the_episodes() {
    let ep = |number: u16, runtime_min: u16| crate::server::tmdb::Episode {
        number,
        name: format!("Ep {number}"),
        runtime_min,
    };
    // Disc 2 of a season whose disc 1 held 3 episodes, not this disc's 2.
    let season = vec![ep(1, 30), ep(2, 30), ep(3, 30), ep(4, 60), ep(5, 90)];
    let titles = vec![title(60.0 * 60.0, 1000), title(90.0 * 60.0, 2000)];
    let plan = plan_episode_outputs(
        &titles,
        &[0, 1],
        "Show Season 1 Disc 2",
        &season,
        "Show.mkv",
    );
    assert_eq!(
        plan.iter().map(|o| o.episode.unwrap()).collect::<Vec<_>>(),
        vec![4, 5],
        "aligned by runtime, not the uniform-split guess E03"
    );
    assert_eq!(plan[0].episode_name, "Ep 4");
    assert_eq!(plan[1].episode_name, "Ep 5");
    assert_eq!(plan[1].filename, "Show_S01E05.mkv");
}

// tv_auto=false holds a TV disc on the single-output path (no auto fan-out).
#[test]
fn tv_auto_off_does_not_fan_out() {
    let cfg = Config {
        tv_auto: false,
        ..Config::default()
    };
    let ep = 44.0 * 60.0;
    let titles = vec![title(ep, 1000), title(ep, 2000), title(ep, 3000)];
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        "tv",
        "Endeavour Season 5",
        44264,
        "Endeavour.mkv",
    );
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "Endeavour.mkv");
}
