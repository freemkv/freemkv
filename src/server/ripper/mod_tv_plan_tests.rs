use super::*;
use libfreemkv::disc::{ContentFormat, Extent};

#[test]
fn presentation_preference_capture_holds_and_freezes_single_output() {
    let titles = crate::selection_test_fixtures::launch_titles();
    for (language, expected) in [("de", 1), ("en", 0)] {
        let cfg = Config {
            presentation_language: language.into(),
            ..Default::default()
        };
        let plan = plan_mux_outputs(&titles, &cfg, "movie", "Movie", 0, "movie.mkv", None).unwrap();
        assert_eq!(plan[0].title_index, expected);
        assert_eq!(
            plan[0].title_identity,
            Some(crate::title_identity::TitleIdentity::of(&titles[expected]))
        );
        assert_eq!(
            capture_title_index(&titles, &cfg, "movie").unwrap(),
            expected
        );
    }
    for language in ["", "fr"] {
        let cfg = Config {
            presentation_language: language.into(),
            ..Default::default()
        };
        assert!(check_capture_selection(&titles, &cfg, "movie").is_err());
        assert!(plan_mux_outputs(&titles, &cfg, "movie", "Movie", 0, "movie.mkv", None).is_err());
        let iso = Config {
            output_format: "iso".into(),
            ..cfg
        };
        assert!(check_capture_selection(&titles, &iso, "movie").is_ok());
    }
}

fn authored_roster(titles: &mut [libfreemkv::DiscTitle], members: std::ops::Range<usize>) {
    let title_count = titles.len();
    for (index, title) in titles.iter_mut().enumerate() {
        title.playlist_id = index as u16;
        title.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
            roster: "test-menu".into(),
            title_count,
            member: members.contains(&index),
            ordinal: members.contains(&index).then(|| index - members.start),
        };
    }
}

#[test]
fn later_disc_confirmed_first_episode_plans_and_rejects_overflow() {
    let mut titles = vec![title(2640.0, 100), title(2650.0, 200)];
    authored_roster(&mut titles, 0..2);
    let cfg = Config::default();
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        "tv",
        "Show Season 1 Disc 2",
        0,
        "Show.mkv",
        Some(4),
    )
    .unwrap();
    assert_eq!(plan.len(), 2);
    assert!(plan[0].filename.contains("E04"));
    assert!(plan[1].filename.contains("E05"));
    assert!(
        plan_mux_outputs(
            &titles,
            &cfg,
            "tv",
            "Show Season 1 Disc 2",
            0,
            "Show.mkv",
            Some(u16::MAX)
        )
        .is_err()
    );
}

fn title(dur_secs: f64, start_lba: u32) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        selection_evidence: Default::default(),
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
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        "movie",
        "The Matrix",
        0,
        "The Matrix.mkv",
        None,
    )
    .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "The Matrix.mkv");
    assert_eq!(plan[0].title_index, 0);
    assert!(plan[0].episode.is_none());
}

#[test]
fn movie_or_unknown_metadata_does_not_fan_out_from_a_season_like_label() {
    let cfg = Config::default();
    let titles = vec![title(2640.0, 100), title(2650.0, 200)];
    for media_type in ["movie", ""] {
        let plan = plan_mux_outputs(
            &titles,
            &cfg,
            media_type,
            "Show Season 1",
            0,
            "Show.mkv",
            None,
        )
        .unwrap();
        assert_eq!(plan.len(), 1, "{media_type:?}");
        assert_eq!(plan[0].filename, "Show.mkv");
    }
}

#[test]
fn capture_and_remux_both_require_confirmed_later_disc_numbering() {
    use crate::server::planner::{MediaKind, MediaMetadata, planner_for};
    use std::path::Path;
    let cfg = Config::default();
    let mut titles = vec![title(2640.0, 100), title(2650.0, 200)];
    authored_roster(&mut titles, 0..2);
    for season in [None, Some(3)] {
        let label = season.map_or_else(
            || "Show Disc 2".to_string(),
            |s| format!("Show Season {s} Disc 2"),
        );
        for disc in [None, Some(2)] {
            let media = MediaMetadata {
                title: label.clone(),
                season,
                disc,
                kind: Some(MediaKind::Tv),
                ..Default::default()
            };
            let capture = plan_mux_outputs(&titles, &cfg, "tv", &label, 0, "Show.mkv", None);
            let remux = planner_for(&cfg, &media).plan(
                Path::new("disc.iso"),
                &titles,
                &media,
                Path::new("Show.mkv"),
            );
            assert!(matches!(
                capture,
                Err(crate::server::planner::PlanError::SelectionNeedsReview(_))
            ));
            assert!(matches!(
                remux,
                Err(crate::server::planner::PlanError::SelectionNeedsReview(_))
            ));
        }
    }
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
    authored_roster(&mut titles, 1..7);
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        "tv",
        "Endeavour Season 5",
        44264,
        "Endeavour.mkv",
        None,
    );
    let plan = plan.unwrap();
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
    let mut titles: Vec<_> = (0..6)
        .map(|k| title(ep + k as f64, 1000 + k * 100))
        .collect();
    authored_roster(&mut titles, 0..6);
    let sum: u64 = titles.iter().map(|t| t.size_bytes).sum();
    let selected = titles[0].size_bytes;
    let plan = plan_mux_outputs(&titles, &cfg, "tv", "Show Season 1", 0, "Show.mkv", None).unwrap();
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
    authored_roster(&mut with_playall, 1..7);
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

// A disc number does not establish the episode count on preceding discs.
#[test]
fn multi_disc_season_requires_confirmed_episode_numbers() {
    let cfg = Config::default();
    let ep = 44.0 * 60.0;
    // 4 episode titles on disc 2 (label carries "Disc 2").
    let mut titles: Vec<_> = (0..4)
        .map(|k| title(ep + k as f64, 1000 + k * 100))
        .collect();
    authored_roster(&mut titles, 0..4);
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        "tv",
        "Endeavour Season 5 Disc 2",
        44264,
        "Endeavour.mkv",
        None,
    );
    assert!(
        matches!(
            plan,
            Err(crate::server::planner::PlanError::SelectionNeedsReview(_))
        ),
        "disc number does not establish how many episodes earlier discs held"
    );
}

// A single feature carrying a TV label (a TV movie) is one movie-style output, not E01.
#[test]
fn a_single_feature_tv_disc_stays_one_output() {
    let cfg = Config::default(); // tv_auto = true
    let titles = vec![title(95.0 * 60.0, 1000), title(90.0, 5)];
    let plan = plan_mux_outputs(&titles, &cfg, "tv", "Show Season 2", 0, "Show.mkv", None).unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "Show.mkv");
    assert!(plan[0].episode.is_none());
}

// Distinctive runtimes are still not proof of season episode identity.
#[test]
fn tmdb_runtimes_do_not_authorize_episode_numbering() {
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
        None,
    );
    assert!(matches!(
        plan,
        Err(crate::server::planner::PlanError::SelectionNeedsReview(_))
    ));
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
        None,
    );
    let plan = plan.unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "Endeavour.mkv");
}
