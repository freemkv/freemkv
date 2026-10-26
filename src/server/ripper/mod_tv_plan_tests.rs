use super::*;
use libfreemkv::disc::{ContentFormat, Extent};

#[test]
fn presentation_preference_capture_freezes_the_chosen_cut_and_never_holds() {
    let titles = crate::selection_test_fixtures::variant_titles();
    for (language, expected) in [("de", 1), ("en", 0), ("", 0), ("fr", 0)] {
        let cfg = Config {
            presentation_language: language.into(),
            ..Default::default()
        };
        let plan = plan_mux_outputs(
            &titles,
            &cfg,
            crate::server::planner::MediaKind::Movie,
            "Movie",
            0,
            "movie.mkv",
            None,
        )
        .unwrap();
        assert_eq!(plan[0].title_index, expected);
        assert_eq!(
            plan[0].title_identity,
            Some(crate::title_identity::TitleIdentity::of(&titles[expected]))
        );
        assert_eq!(
            capture_title_index(&titles, &cfg, crate::server::planner::MediaKind::Movie, "")
                .unwrap(),
            expected
        );
        assert!(
            check_capture_selection(&titles, &cfg, crate::server::planner::MediaKind::Movie, "")
                .is_ok()
        );
    }
}

fn authored_roster(titles: &mut [libfreemkv::DiscTitle], members: std::ops::Range<usize>) {
    let title_count = titles.len();
    for (index, title) in titles.iter_mut().enumerate() {
        title.playlist_id = index as u16;
        title.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
            roster: "test-menu".into(),
            title_count,
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
        crate::server::planner::MediaKind::Tv,
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
            crate::server::planner::MediaKind::Tv,
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
        crate::server::planner::MediaKind::Movie,
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

// 1.7.7 tv_auto: a disc whose label names a season is TV even when TMDB did not
// match it as TV (no key, a miss, or a movie match). Off: one output, never held.
#[test]
fn a_season_label_is_tv_for_fan_out_without_a_tmdb_tv_match() {
    let mut titles = vec![title(2640.0, 100), title(2650.0, 200)];
    authored_roster(&mut titles, 0..2);
    let cfg = Config::default();
    for media_type in ["movie", ""] {
        let plan = plan_mux_outputs(
            &titles,
            &cfg,
            media_type.into(),
            "Show Season 1",
            0,
            "Show.mkv",
            None,
        )
        .unwrap();
        assert_eq!(plan.len(), 2, "{media_type:?}");
        assert!(plan[0].filename.contains("S01E01"), "{}", plan[0].filename);
        assert!(plan[1].filename.contains("S01E02"), "{}", plan[1].filename);
        assert!(check_capture_selection(&titles, &cfg, media_type.into(), "Show Season 1").is_ok());
    }
    let off = Config {
        tv_auto: false,
        ..Config::default()
    };
    let plan = plan_mux_outputs(
        &titles,
        &off,
        crate::server::planner::MediaKind::Movie,
        "Show Season 1",
        0,
        "Show.mkv",
        None,
    )
    .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "Show.mkv");
}

// A season-labelled disc with no authored episode evidence is held for review on
// capture and on resume reselection, never silently ripped as one title.
#[test]
fn a_season_labelled_disc_without_episode_evidence_is_held_for_review() {
    let titles = vec![title(2640.0, 100), title(2650.0, 200)];
    let cfg = Config::default();
    for media_type in ["movie", ""] {
        assert!(matches!(
            check_capture_selection(&titles, &cfg, media_type.into(), "Show Season 1"),
            Err(crate::server::planner::PlanError::Selection(_))
        ));
        assert!(matches!(
            resume::check_resume_selection(
                &titles,
                &cfg,
                media_type.into(),
                "Show Season 1",
                false,
                false
            ),
            Err(crate::server::planner::PlanError::Selection(_))
        ));
    }
    let off = Config {
        tv_auto: false,
        ..Config::default()
    };
    assert!(
        check_capture_selection(
            &titles,
            &off,
            crate::server::planner::MediaKind::Movie,
            "Show Season 1"
        )
        .is_ok()
    );
    assert!(
        resume::check_resume_selection(
            &titles,
            &off,
            crate::server::planner::MediaKind::Movie,
            "Show Season 1",
            false,
            false
        )
        .is_ok()
    );
}

#[test]
fn capture_and_remux_number_a_later_disc_as_1_8_3_did() {
    use crate::server::planner::{MediaKind, MediaMetadata, plan};
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
                // Remux callers resolve the disc number from the ISO labels;
                // the planner no longer parses the show title.
                disc: disc.or(Some(2)),
                kind: Some(MediaKind::Tv),
                ..Default::default()
            };
            let capture = plan_mux_outputs(
                &titles,
                &cfg,
                crate::server::planner::MediaKind::Tv,
                &label,
                0,
                "Show.mkv",
                None,
            );
            let remux = plan(&cfg, &titles, &media, Path::new("Show.mkv")).unwrap();
            // 1.8.3: disc 2 of two-episode discs starts at (2-1)*2+1 = E03.
            let numbers = |e: Vec<Option<u16>>| e;
            assert_eq!(
                numbers(capture.unwrap().iter().map(|o| o.episode).collect()),
                [Some(3), Some(4)]
            );
            assert_eq!(
                numbers(remux.outputs.iter().map(|o| o.episode).collect()),
                [Some(3), Some(4)]
            );
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
        crate::server::planner::MediaKind::Tv,
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
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        crate::server::planner::MediaKind::Tv,
        "Show Season 1",
        0,
        "Show.mkv",
        None,
    )
    .unwrap();
    let planned: u64 = plan.iter().map(|o| titles[o.title_index].size_bytes).sum();
    assert_eq!(planned, sum, "plan muxes all six episodes");
    assert_eq!(
        mux_output_reserve_bytes(
            &titles,
            &cfg,
            crate::server::planner::MediaKind::Tv,
            "Show Season 1",
            selected
        ),
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
        mux_output_reserve_bytes(
            &with_playall,
            &cfg,
            crate::server::planner::MediaKind::Tv,
            "Show Season 1",
            big
        ),
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
            mux_output_reserve_bytes(
                &titles,
                &c,
                crate::server::planner::MediaKind::Tv,
                "Show Season 1",
                selected
            ),
            0,
            "{fmt}"
        );
    }
    // Movie / tv_auto off: one output, the selected title.
    assert_eq!(
        mux_output_reserve_bytes(
            &titles,
            &cfg,
            crate::server::planner::MediaKind::Movie,
            "Some Film",
            selected
        ),
        selected
    );
    let off = Config {
        tv_auto: false,
        ..Config::default()
    };
    assert_eq!(
        mux_output_reserve_bytes(
            &titles,
            &off,
            crate::server::planner::MediaKind::Tv,
            "Show Season 1",
            selected
        ),
        selected
    );
}

// 1.8.3 (and autorip 1.7.7): disc N of a season starts at (N-1)*count+1.
#[test]
fn disc_two_of_four_episode_discs_is_e05_to_e08() {
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
        crate::server::planner::MediaKind::Tv,
        "Endeavour Season 5 Disc 2",
        44264,
        "Endeavour.mkv",
        None,
    );
    let plan = plan.unwrap();
    assert_eq!(
        plan.iter().map(|o| o.episode).collect::<Vec<_>>(),
        [Some(5), Some(6), Some(7), Some(8)]
    );
    assert_eq!(plan[0].filename, "Endeavour_S05E05.mkv");
    // An operator First episode still wins.
    let first = plan_mux_outputs(
        &titles,
        &cfg,
        crate::server::planner::MediaKind::Tv,
        "Endeavour Season 5 Disc 2",
        44264,
        "Endeavour.mkv",
        Some(9),
    )
    .unwrap();
    assert_eq!(first[0].episode, Some(9));
}

// A single feature carrying a TV label (a TV movie) is one movie-style output, not E01.
#[test]
fn a_single_feature_tv_disc_stays_one_output() {
    let cfg = Config::default(); // tv_auto = true
    let titles = vec![title(95.0 * 60.0, 1000), title(90.0, 5)];
    let plan = plan_mux_outputs(
        &titles,
        &cfg,
        crate::server::planner::MediaKind::Tv,
        "Show Season 2",
        0,
        "Show.mkv",
        None,
    )
    .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "Show.mkv");
    assert!(plan[0].episode.is_none());
}

// Metadata runtimes cannot override disc order; First episode can correct an uneven split.
#[test]
fn later_disc_numbering_ignores_tmdb_runtime_and_accepts_manual_first_episode() {
    let ep = |number: u16, runtime_min: u16| crate::server::tmdb::Episode {
        number,
        name: format!("Ep {number}"),
        runtime_min,
    };
    // Disc 2 may follow an uneven split; only the operator can know that.
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
    let plan = plan.unwrap();
    assert_eq!(
        plan.iter().map(|o| o.episode).collect::<Vec<_>>(),
        [Some(3), Some(4)]
    );
    assert_eq!(plan[1].episode_name, "Ep 4");
    let corrected = plan_episode_outputs(
        &titles,
        &[0, 1],
        "Show Season 1 Disc 2",
        &season,
        "Show.mkv",
        Some(4),
    )
    .unwrap();
    assert_eq!(
        corrected.iter().map(|o| o.episode).collect::<Vec<_>>(),
        [Some(4), Some(5)]
    );
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
        crate::server::planner::MediaKind::Tv,
        "Endeavour Season 5",
        44264,
        "Endeavour.mkv",
        None,
    );
    let plan = plan.unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].filename, "Endeavour.mkv");
}

#[test]
fn a_selection_review_message_is_a_sentence_not_an_enum_dump() {
    use crate::server::planner::PlanError;
    let roster = selection_review_message(&PlanError::Selection(freemkv_engine::SelectionReview {
        reason: freemkv_engine::SelectionReviewReason::MissingEpisodeRoster,
        candidates: vec![0, 1],
    }));
    assert_eq!(
        roster,
        "Title selection needs review before muxing: E9086 the disc has no authored episode list. No fallback title was selected. If it is a film, correct its match to the movie; to rip it as one title, turn off \"Rip TV seasons automatically\" in Settings."
    );
    // Never the impossible action: the server has no title picker.
    assert!(!roster.contains("Select episode titles"), "{roster}");
    let numbering = selection_review_message(&PlanError::EpisodeRange);
    assert!(!numbering.contains(".."), "{numbering}");
    assert!(numbering.ends_with("Set the First episode when you correct its match."));
    for error in [
        PlanError::NoTitles,
        PlanError::NoOutputs,
        PlanError::DuplicateOutput("Show_S01E01.mkv".into()),
    ] {
        let text = selection_review_message(&error);
        assert!(!text.contains(&format!("{error:?}")), "{text}");
    }
}

// 1.8.3: a single-pass rip muxes the main title as one output and never fans out, so
// it must not pick the first episode and deliver it as the show.
#[test]
fn a_single_pass_tv_rip_muxes_the_main_title_not_the_first_episode() {
    // The play-all first, as a scan lists it, then the two authored episodes.
    let mut titles = vec![title(5290.0, 300), title(2640.0, 100), title(2650.0, 200)];
    authored_roster(&mut titles, 1..3);
    let single = Config {
        max_retries: 0,
        ..Default::default()
    };
    let kind = crate::server::planner::MediaKind::Tv;
    let main = crate::server::planner::main_title(&single, &titles).unwrap();
    assert_ne!(
        main,
        fanout_episode_indices(&titles, &single, kind, "Show Season 1")[0]
    );
    assert_eq!(
        capture_title_index(&titles, &single, kind, "Show Season 1").unwrap(),
        main
    );
    assert!(check_capture_selection(&titles, &single, kind, "Show Season 1").is_ok());
    // Multipass still captures the episode set for its fan-out.
    let multi = Config::default();
    assert_eq!(
        capture_title_index(&titles, &multi, kind, "Show Season 1").unwrap(),
        fanout_episode_indices(&titles, &multi, kind, "Show Season 1")[0]
    );
}

// A failed hand-off falls back to the inline mux of one title only for a one-output plan.
#[test]
fn an_episode_fan_out_never_falls_back_to_the_one_title_inline_mux() {
    let out = |name: &str| staging::Output {
        filename: name.into(),
        ..Default::default()
    };
    assert!(inline_mux_delivers(&[out("Movie.mkv")]));
    let episodes = [out("Show_S01E01.mkv"), out("Show_S01E02.mkv")];
    assert!(!inline_mux_delivers(&episodes));
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let failed = || Err(std::io::Error::other("staging share is read-only"));
    let device = "dev-hand-off-kept";
    let kept = hand_off_or_keep(device, inline_mux_delivers(&episodes), failed);
    assert_eq!(
        kept,
        HandOff::Kept,
        "an episode set is never muxed as one title"
    );
    let row = STATE.lock().unwrap().remove(device).unwrap();
    assert_eq!(row.status, "error");
    assert!(
        row.last_error.contains("hand-off to the mux worker failed"),
        "{}",
        row.last_error
    );
    let movie = hand_off_or_keep(device, inline_mux_delivers(&[out("Movie.mkv")]), failed);
    assert_eq!(movie, HandOff::Inline);
    STATE.lock().unwrap().remove(device);
}

// The single-pass log names the title the rip muxes, not the engine's MainMovie pick.
#[test]
fn the_single_pass_plan_line_names_the_muxed_title() {
    let cfg = Config {
        max_retries: 0,
        ..Default::default()
    };
    let line = plan_line(&single_pass_plan(&cfg, "/dev/sr0", "mkv:///s/Show.mkv", 2));
    assert!(line.contains("Titles([2])"), "{line}");
}

// TMDB down at capture start: the hand-off may attach names later, never renumber titles.
#[test]
fn the_hand_off_replans_a_start_plan_made_without_tmdb() {
    let ep = |number: u16, runtime_min: u16| crate::server::tmdb::Episode {
        number,
        name: format!("Ep {number}"),
        runtime_min,
    };
    let season = vec![ep(1, 30), ep(2, 30), ep(3, 30), ep(4, 60), ep(5, 90)];
    let mut titles = vec![title(60.0 * 60.0, 1000), title(90.0 * 60.0, 2000)];
    authored_roster(&mut titles, 0..2);
    let cfg = Config::default();
    let plan_for = |episodes: Vec<crate::server::tmdb::Episode>, first: Option<u16>| {
        plan_with_season(
            &titles,
            &cfg,
            (
                crate::server::planner::MediaKind::Tv,
                "Show Season 1 Disc 2",
                7,
            ),
            "Show.mkv",
            first,
            move |_, _, _| episodes,
        )
    };
    let numbers = |plan: PlanResult| {
        plan.unwrap()
            .iter()
            .map(|o| (o.episode, o.episode_name.clone()))
            .collect::<Vec<_>>()
    };
    let disc_order = vec![(Some(3), String::new()), (Some(4), String::new())];
    let start = plan_for(Vec::new(), None);
    assert!(start.without_tmdb);
    assert_eq!(
        numbers(handoff_plan(start, || plan_for(season.clone(), None))),
        [(Some(3), "Ep 3".into()), (Some(4), "Ep 4".into())]
    );
    let still_down = handoff_plan(plan_for(Vec::new(), None), || plan_for(Vec::new(), None));
    assert_eq!(numbers(still_down), disc_order);
    let failed = handoff_plan(plan_for(Vec::new(), None), || {
        plan_for(season.clone(), Some(u16::MAX))
    });
    assert_eq!(numbers(failed), disc_order);
    let named = plan_for(season.clone(), None);
    assert!(!named.without_tmdb);
    let kept = handoff_plan(named, || unreachable!("a TMDB-named plan is not refetched"));
    assert_eq!(numbers(kept)[1].1, "Ep 4");
}

// Pass 1 can leave a later episode damaged while the first reads clean: the retry passes
// must measure every planned episode, or they skip as "converged" and mux the damage.
#[test]
fn retry_passes_measure_every_planned_episode() {
    let mut titles = vec![title(2640.0, 100), title(2650.0, 5000), title(2660.0, 9000)];
    authored_roster(&mut titles, 0..3);
    let cfg = Config::default();
    let tv = (
        crate::server::planner::MediaKind::Tv,
        "Show Season 1 Disc 2",
    );
    assert_eq!(recovery_titles(&titles, &cfg, tv, 0), [0, 1, 2]);
    let movie = (crate::server::planner::MediaKind::Movie, "Movie");
    assert_eq!(recovery_titles(&titles, &cfg, movie, 0), [0]);
}
