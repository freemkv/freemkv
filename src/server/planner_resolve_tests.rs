//! The one title policy (`resolve_titles`, design §3.2) every scope consumer uses.
use super::*;
use freemkv_engine::Resolved;
use libfreemkv::disc::{ContentFormat, EpisodeEvidence, Extent};

fn title(duration_secs: f64, start_lba: u32, playlist_id: u16) -> DiscTitle {
    DiscTitle {
        selection_evidence: Default::default(),
        playlist: format!("{playlist_id:05}.mpls"),
        playlist_id,
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

// "Samurai Champloo Vol. 1": four 23-minute episodes, no play-all, extras under 10 minutes.
fn samurai() -> Vec<DiscTitle> {
    let mut titles: Vec<_> = (0..4u16)
        .map(|k| title(1380.0 + f64::from(k), 1000 + u32::from(k) * 2000, k))
        .collect();
    titles.extend((0..3u16).map(|k| title(120.0, 20_000 + u32::from(k) * 2000, 10 + k)));
    titles
}

fn media(kind: Option<MediaKind>) -> MediaMetadata {
    MediaMetadata {
        title: "Samurai Champloo".into(),
        kind,
        ..MediaMetadata::default()
    }
}

fn run(titles: &[usize], episodes: bool) -> Resolved {
    Resolved::Run {
        titles: titles.to_vec(),
        episodes,
    }
}

#[test]
fn a_movie_disc_is_never_split_or_held() {
    let titles = samurai();
    let main = main_title(&Config::default(), &titles).unwrap();
    for kind in [Some(MediaKind::Movie), None] {
        for mode in [ScopeMode::Mux, ScopeMode::SinglePass] {
            assert_eq!(
                resolve_titles(&Config::default(), &titles, &media(kind), None, mode),
                run(&[main], false),
                "{kind:?} {mode:?}"
            );
        }
        let plan = plan(
            &Config::default(),
            &titles,
            &media(kind),
            Path::new("Film.mkv"),
        )
        .unwrap();
        assert_eq!(plan.outputs.len(), 1);
        assert_eq!(plan.outputs[0].filename, "Film.mkv");
    }
}

#[test]
fn a_tv_disc_without_a_roster_holds_with_a_suggestion() {
    let titles = samurai();
    let request = TitleRequest {
        media: media(Some(MediaKind::Tv)),
        ..TitleRequest::default()
    };
    let Err(PlanError::Selection(confirm)) =
        request.run(&Config::default(), &titles, ScopeMode::Mux)
    else {
        panic!("a TV disc without a proven roster must hold for confirmation");
    };
    assert_eq!(
        confirm.reason,
        freemkv_engine::SelectionReviewReason::MissingEpisodeRoster
    );
    assert_eq!(confirm.suggested, [0, 1, 2, 3], "the extras are too short");
    assert_eq!(confirm.clusters, [vec![0, 1, 2, 3]]);
    // The suggestion sizes a key scope; it never authorizes a rip.
    assert_eq!(
        request.scope(&Config::default(), &titles, ScopeMode::Mux),
        [0, 1, 2, 3]
    );
    // The hold persists across a restart (state.json) and back.
    let json = serde_json::to_string(&confirm).unwrap();
    assert!(json.contains("missing-episode-roster"), "{json}");
    assert_eq!(
        serde_json::from_str::<EpisodeConfirm>(&json).unwrap(),
        confirm
    );
    // Turning off TV seasons rips the main title, as before.
    let off = Config {
        tv_auto: false,
        ..Config::default()
    };
    assert!(request.run(&off, &titles, ScopeMode::Mux).is_ok());
}

#[test]
fn an_operator_pick_runs_exactly_the_picked_titles_in_order() {
    let titles = samurai();
    let cfg = Config::default();
    let tv = media(Some(MediaKind::Tv));
    assert_eq!(
        resolve_titles(&cfg, &titles, &tv, Some(&[3, 1, 0, 2]), ScopeMode::Mux),
        run(&[3, 1, 0, 2], true)
    );
    // Out of range and repeated indices are dropped; the order stays the operator's.
    assert_eq!(
        resolve_titles(&cfg, &titles, &tv, Some(&[2, 9, 2, 0]), ScopeMode::Mux),
        run(&[2, 0], true)
    );
    // A Movie pick is one feature, never split.
    let movie = media(Some(MediaKind::Movie));
    assert_eq!(
        resolve_titles(&cfg, &titles, &movie, Some(&[2, 0]), ScopeMode::Mux),
        run(&[2], false)
    );
    // The plan numbers the picked episodes in the picked order.
    let request = TitleRequest {
        media: tv,
        pick: Some(vec![3, 1, 0, 2]),
        pick_mismatch: false,
    };
    let plan = plan_request(&cfg, &titles, &request, Path::new("Show.mkv")).unwrap();
    assert_eq!(
        plan.outputs
            .iter()
            .map(|o| (o.title_index, o.episode))
            .collect::<Vec<_>>(),
        [(3, Some(1)), (1, Some(2)), (0, Some(3)), (2, Some(4))]
    );
    // A TV pick of one title with a First episode is one numbered episode.
    let one = TitleRequest {
        media: MediaMetadata {
            episode_start: Some(7),
            ..request.media.clone()
        },
        pick: Some(vec![1]),
        pick_mismatch: false,
    };
    let plan = plan_request(&cfg, &titles, &one, Path::new("Show.mkv")).unwrap();
    assert_eq!(plan.outputs.len(), 1);
    assert_eq!(plan.outputs[0].episode, Some(7));
    assert_eq!(plan.outputs[0].filename, "Show_S01E07.mkv");
}

#[test]
fn iso_and_single_pass_rips_never_hold() {
    let titles = samurai();
    let cfg = Config::default();
    let tv = media(Some(MediaKind::Tv));
    let main = main_title(&cfg, &titles).unwrap();
    assert_eq!(
        resolve_titles(&cfg, &titles, &tv, None, ScopeMode::WholeDisc),
        run(&[0], false)
    );
    assert_eq!(
        resolve_titles(&cfg, &titles, &tv, None, ScopeMode::SinglePass),
        run(&[main], false)
    );
    // Not even a TV pick splits a single pass; a stale pick never holds an ISO.
    assert_eq!(
        resolve_titles(&cfg, &titles, &tv, Some(&[2, 3]), ScopeMode::SinglePass),
        run(&[main], false)
    );
    let stale = TitleRequest {
        media: tv,
        pick: None,
        pick_mismatch: true,
    };
    assert!(stale.run(&cfg, &titles, ScopeMode::WholeDisc).is_ok());
    assert_eq!(
        stale.run(&cfg, &titles, ScopeMode::Mux),
        Err(PlanError::PickMismatch)
    );
    // The capture mode follows the settings.
    let iso = Config {
        output_format: crate::server::config::OUTPUT_FORMAT_ISO.into(),
        ..Config::default()
    };
    let single = Config {
        max_retries: 0,
        ..Config::default()
    };
    assert_eq!(ScopeMode::capture(&iso), ScopeMode::WholeDisc);
    assert_eq!(ScopeMode::capture(&single), ScopeMode::SinglePass);
    assert_eq!(ScopeMode::capture(&Config::default()), ScopeMode::Mux);
}

// A held capture swept only part of the disc: the dialog offers only what it captured.
#[test]
fn a_held_capture_confirms_only_titles_inside_its_sweep() {
    let confirm = EpisodeConfirm {
        reason: freemkv_engine::SelectionReviewReason::MissingEpisodeRoster,
        suggested: vec![0, 1, 2],
        clusters: vec![vec![0, 1, 2], vec![4, 5]],
        alternates: vec![
            ConfirmAlternate { index: 3, of: 1 },
            ConfirmAlternate { index: 6, of: 4 },
        ],
    };
    assert_eq!(confirm.titles(), [0, 1, 2, 3, 4, 5, 6]);
    let swept = [0, 1, 3, 4];
    let held = confirm.clone().within(|index| swept.contains(&index));
    assert_eq!(held.suggested, [0, 1]);
    assert_eq!(held.clusters, [vec![0, 1]]);
    assert_eq!(held.alternates, [ConfirmAlternate { index: 3, of: 1 }]);
    // A suggestion left with one title suggests nothing.
    let held = confirm.within(|index| index == 0 || index == 4);
    assert!(held.suggested.is_empty() && held.clusters.is_empty());
}

#[test]
fn a_saved_pick_is_remapped_by_identity_onto_a_rescan() {
    let titles = samurai();
    let pick: Vec<_> = [2, 0]
        .iter()
        .map(|&i| crate::title_identity::TitleIdentity::of(&titles[i]))
        .collect();
    // A rescan lists the titles in another order: the pick follows the titles.
    let mut rescan = titles.clone();
    rescan.reverse();
    let request = TitleRequest::new(media(Some(MediaKind::Tv)), Some(&pick), &rescan);
    assert_eq!(request.pick, Some(vec![4, 6]));
    assert!(!request.pick_mismatch);
    // A title the rescan no longer has is a mismatch, never a partial pick.
    let request = TitleRequest::new(media(Some(MediaKind::Tv)), Some(&pick), &rescan[..6]);
    assert_eq!((request.pick, request.pick_mismatch), (None, true));
    // The disc's own episode list still wins only without a pick.
    let mut authored = titles.clone();
    let count = authored.len();
    for (index, t) in authored.iter_mut().enumerate() {
        t.selection_evidence.episodes = EpisodeEvidence::Authored {
            roster: "menu".into(),
            title_count: count,
            ordinal: (index < 4).then_some(index),
        };
    }
    let picked = TitleRequest::new(media(Some(MediaKind::Tv)), Some(&pick), &authored);
    assert_eq!(
        picked.run(&Config::default(), &authored, ScopeMode::Mux),
        Ok(TitleRun {
            titles: vec![2, 0],
            episodes: true
        })
    );
}
