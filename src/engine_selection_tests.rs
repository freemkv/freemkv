use super::{key_summary_tests::disc, scanned_from_disc};
use crate::ui::{LangPrefs, Tree};
use freemkv_engine::{Selection, SelectionModel, StreamFilter};
use libfreemkv::disc::{EpisodeEvidence, MovieSelectionBasis, NavigationSource};
use libfreemkv::{AudioChannels, AudioStream, Codec, DiscTitle, LabelPurpose, SampleRate, Stream};

#[test]
fn presentation_language_selects_authored_launch_with_audio_all_in_gui() {
    let mut disc = disc(false);
    disc.titles = crate::selection_test_fixtures::launch_titles();
    let scan = scanned_from_disc(&disc, "none".into());
    for (language, expected) in [("de", vec![1]), ("en", vec![0]), ("fr", vec![])] {
        let prefs = LangPrefs {
            presentation_language: Some(language.into()),
            ..Default::default()
        };
        assert!(prefs.audio.is_empty());
        assert_eq!(
            Tree::from_scan(&scan, "Main film only", 0.0, &prefs).ticked_titles(),
            expected
        );
    }
}

fn title(id: u16, program: &str, secs: f64, language: &str) -> DiscTitle {
    let mut title = DiscTitle::empty();
    title.playlist_id = id;
    title.playlist = format!("{id:05}.mpls");
    title.duration_secs = secs;
    title.clips.push(libfreemkv::Clip {
        clip_id: program.into(),
        in_time: 0,
        out_time: (secs * 45_000.0) as u32,
        duration_secs: secs,
        source_packets: 0,
        feed_span: None,
    });
    title.streams.push(Stream::Audio(AudioStream {
        pid: 0x1100,
        codec: Codec::Ac3,
        channels: AudioChannels::Stereo,
        language: language.into(),
        sample_rate: SampleRate::S48,
        secondary: false,
        purpose: LabelPurpose::Normal,
        label: String::new(),
    }));
    title
}

fn authored(titles: &mut [DiscTitle], members: &[usize]) {
    let title_count = titles.len();
    let model = SelectionModel::from_titles(titles);
    let mut identities = std::collections::HashMap::new();
    let mut orders = std::collections::HashMap::new();
    let mut next = 0;
    for &index in members {
        let identity = model.titles()[index].presentation.clone();
        let ordinal = identity
            .as_ref()
            .and_then(|id| identities.get(id))
            .copied()
            .unwrap_or_else(|| {
                let ordinal = next;
                next += 1;
                if let Some(id) = identity {
                    identities.insert(id, ordinal);
                }
                ordinal
            });
        orders.insert(index, ordinal);
    }
    for (index, title) in titles.iter_mut().enumerate() {
        title.selection_evidence.episodes = EpisodeEvidence::Authored {
            roster: "fixture:verified-menu".into(),
            title_count,
            member: members.contains(&index),
            ordinal: orders.get(&index).copied(),
        };
    }
}

#[test]
fn authored_order_survives_canonical_duration_sort_in_cli_and_gui() {
    let mut disc = disc(false);
    for (program, secs) in [("first", 1800.0), ("second", 2700.0), ("third", 2100.0)] {
        for language in ["eng", "deu"] {
            disc.titles
                .push(title(disc.titles.len() as u16, program, secs, language));
        }
    }
    authored(&mut disc.titles, &[0, 1, 2, 3, 4, 5]);
    disc.titles
        .sort_by(|a, b| b.duration_secs.total_cmp(&a.duration_secs));
    let scan = scanned_from_disc(&disc, "none".into());
    for (languages, expected) in [("", vec![4, 0, 2]), ("deu", vec![5, 1, 3])] {
        let prefs = LangPrefs::parse(languages, "", "");
        let audio = if languages.is_empty() {
            StreamFilter::All
        } else {
            StreamFilter::Langs(prefs.audio.clone())
        };
        assert_eq!(
            freemkv_engine::resolve_selection_with_audio(&disc, &Selection::Episodes, &audio),
            expected
        );
        let tree = Tree::from_scan(&scan, "Episodes", 0.0, &prefs);
        assert_eq!(tree.ticked_titles(), expected);
        assert_eq!(
            tree.arena
                .iter()
                .filter(|n| n.type_s == "Title")
                .map(|n| n.title_idx)
                .collect::<Vec<_>>(),
            (0..6).collect::<Vec<_>>(),
            "display numbering stays canonical"
        );
        // The selected canonical indices also remain ordered when submitted explicitly.
        assert_eq!(
            freemkv_engine::resolve_selection_with_audio(
                &disc,
                &Selection::Titles(tree.ticked_titles()),
                &audio
            ),
            expected
        );
    }
}

#[test]
fn gui_modes_match_the_disc_model_with_language_alternates_and_authored_episodes() {
    let mut disc = disc(false);
    disc.titles = vec![
        title(10, "feature", 5300.0, "eng"),
        title(20, "feature", 5300.0, "deu"),
        title(30, "short-episode", 90.0, "eng"),
        title(40, "short-episode", 90.0, "deu"),
        title(50, "long-episode", 3900.0, "eng"),
        title(60, "long-episode", 3900.0, "deu"),
        title(70, "unrelated-play-all", 9000.0, "jpn"),
    ];
    disc.titles[0].selection_evidence.movie_basis =
        MovieSelectionBasis::Navigation(NavigationSource::HdmvFirstPlay);
    authored(&mut disc.titles, &[2, 3, 4, 5]);
    let scan = scanned_from_disc(&disc, "none".into());
    let model = SelectionModel::from_disc(&disc);
    assert_eq!(
        scan.selection_model, model,
        "the bridge must preserve evidence"
    );
    for (languages, main, episodes) in [
        ("", vec![0], vec![2, 4]),
        ("German", vec![1], vec![3, 5]),
        ("eng,deu", vec![0], vec![2, 4]),
        ("jpn", vec![0], vec![2, 4]),
    ] {
        let prefs = LangPrefs::parse(languages, "", "");
        let audio = if prefs.audio.is_empty() {
            StreamFilter::All
        } else {
            StreamFilter::Langs(prefs.audio.clone())
        };
        for (mode, selection, expected) in [
            ("Main film only", Selection::MainMovie, main),
            ("Episodes", Selection::Episodes, episodes),
            ("Longest title", Selection::Longest, vec![6]),
            ("All titles", Selection::All, (0..7).collect()),
            ("No titles", Selection::Titles(vec![]), vec![]),
        ] {
            assert_eq!(model.select(&selection, &audio).indices, expected);
            assert_eq!(
                Tree::from_scan(&scan, mode, 0.0, &prefs).ticked_titles(),
                expected,
                "{mode}, {languages}"
            );
        }
    }
}

#[test]
fn visibility_never_promotes_an_unrelated_movie_in_the_preferred_language() {
    let mut disc = disc(false);
    disc.titles = vec![
        title(1, "canonical", 90.0, "eng"),
        title(2, "unrelated", 5400.0, "deu"),
    ];
    let scan = scanned_from_disc(&disc, "none".into());
    let prefs = LangPrefs::parse("deu", "", "");
    assert_eq!(
        Tree::from_scan(&scan, "Main film only", 0.0, &prefs).ticked_titles(),
        vec![0]
    );
    let filtered = Tree::from_scan(&scan, "Main film only", 300.0, &prefs);
    assert_eq!(filtered.title_count(), 1);
    assert!(filtered.ticked_titles().is_empty());
    let row = filtered
        .arena
        .iter()
        .position(|n| n.type_s == "Title")
        .unwrap();
    filtered.toggle(row);
    assert_eq!(
        filtered.ticked_titles(),
        vec![1],
        "explicit choices still work"
    );
}

#[test]
fn unknown_or_partial_episode_evidence_leaves_candidates_for_explicit_selection() {
    for partial in [false, true] {
        let mut disc = disc(false);
        disc.titles = vec![title(1, "a", 2400.0, "eng"), title(2, "b", 2400.0, "deu")];
        if partial {
            authored(&mut disc.titles, &[0, 1]);
            disc.titles[1].selection_evidence.episodes = EpisodeEvidence::Unknown;
        }
        let mut scan = scanned_from_disc(&disc, "none".into());
        // Even stale display notes are not selection authority.
        for row in scan.rows.iter_mut().filter(|r| r.type_s == "Title") {
            row.notes = "Episode".into();
        }
        let report = scan
            .selection_model
            .select(&Selection::Episodes, &StreamFilter::All);
        assert!(report.requires_review());
        assert!(report.indices.is_empty());
        assert_eq!(report.candidates, vec![0, 1]);
        let tree = Tree::from_scan(&scan, "Episodes", 0.0, &LangPrefs::default());
        assert!(tree.ticked_titles().is_empty());
        assert_eq!(tree.title_count(), 2);
        let row = tree
            .arena
            .iter()
            .position(|n| n.type_s == "Title" && n.title_idx == 1)
            .unwrap();
        tree.toggle(row);
        assert_eq!(tree.ticked_titles(), vec![1]);
    }
}

#[test]
fn visible_episode_subset_keeps_canonical_indices_and_longest_ties_match_engine() {
    let mut disc = disc(false);
    disc.titles = vec![
        title(1, "short", 90.0, "eng"),
        title(2, "a", 2400.0, "eng"),
        title(3, "b", 2400.0, "deu"),
    ];
    authored(&mut disc.titles, &[0, 1, 2]);
    let scan = scanned_from_disc(&disc, "none".into());
    let prefs = LangPrefs::parse("deu", "", "");
    assert_eq!(
        Tree::from_scan(&scan, "Episodes", 300.0, &prefs).ticked_titles(),
        vec![1, 2]
    );
    assert_eq!(
        Tree::from_scan(&scan, "Longest title", 300.0, &prefs).ticked_titles(),
        vec![1]
    );
    assert_eq!(
        scan.selection_model
            .select(&Selection::Longest, &StreamFilter::Langs(prefs.audio))
            .indices,
        vec![1]
    );
}
