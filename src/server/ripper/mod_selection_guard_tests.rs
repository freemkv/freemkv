use super::*;
use crate::server::planner::PlanError;
use libfreemkv::disc::{ContentFormat, EpisodeEvidence, Extent};

fn titles() -> Vec<libfreemkv::DiscTitle> {
    [100, 200]
        .into_iter()
        .map(|start_lba| libfreemkv::DiscTitle {
            selection_evidence: Default::default(),
            playlist: String::new(),
            playlist_id: start_lba as u16,
            duration_secs: 2700.0,
            size_bytes: 1_000_000,
            clips: Vec::new(),
            streams: Vec::new(),
            chapters: Vec::new(),
            extents: vec![Extent {
                start_lba,
                sector_count: 50,
            }],
            content_format: ContentFormat::BdTs,
            codec_privates: Vec::new(),
        })
        .collect()
}

fn needs_review(result: Result<(), PlanError>) {
    assert!(matches!(result, Err(PlanError::Selection(_))), "{result:?}");
}

#[test]
fn ambiguous_tv_capture_requires_review_for_muxed_formats() {
    for output_format in ["mkv", "m2ts"] {
        let cfg = Config {
            output_format: output_format.into(),
            tv_auto: true,
            ..Default::default()
        };
        needs_review(check_capture_selection(
            &titles(),
            &cfg,
            &crate::server::ripper::test_request(
                crate::server::planner::MediaKind::Tv,
                "",
                0,
                None,
            ),
        ));
    }
}

#[test]
fn ambiguous_tv_iso_capture_remains_available() {
    let cfg = Config {
        output_format: "iso".into(),
        tv_auto: true,
        ..Default::default()
    };
    assert!(
        check_capture_selection(
            &titles(),
            &cfg,
            &crate::server::ripper::test_request(
                crate::server::planner::MediaKind::Tv,
                "",
                0,
                None
            )
        )
        .is_ok()
    );
}

// A movie is never split and never held, even when its titles look like an episode set;
// with "Rip TV seasons automatically" off a TV disc rips its main title.
#[test]
fn selection_guard_preserves_movie_and_disabled_tv_auto_policy() {
    use crate::server::planner::MediaKind;
    let mut cfg = Config::default();
    let main = crate::server::planner::main_title(&cfg, &titles()).unwrap();
    for kind in ["movie", ""] {
        let request = crate::server::ripper::test_request(kind.into(), "", 0, None);
        assert_eq!(capture_title_index(&titles(), &cfg, &request), Ok(main));
        assert!(fanout_episode_indices(&titles(), &cfg, &request, ScopeMode::Mux).is_empty());
    }
    cfg.tv_auto = false;
    let tv = crate::server::ripper::test_request(MediaKind::Tv, "", 0, None);
    assert_eq!(capture_title_index(&titles(), &cfg, &tv), Ok(main));
    assert!(fanout_episode_indices(&titles(), &cfg, &tv, ScopeMode::Mux).is_empty());
}

#[test]
fn authored_tv_evidence_allows_capture_and_replan() {
    let mut titles = titles();
    let title_count = titles.len();
    for (ordinal, title) in titles.iter_mut().enumerate() {
        title.selection_evidence.episodes = EpisodeEvidence::Authored {
            roster: "test-authored-roster".into(),
            title_count,
            ordinal: Some(ordinal),
        };
    }
    let cfg = Config::default();
    assert!(
        check_capture_selection(
            &titles,
            &cfg,
            &crate::server::ripper::test_request(
                crate::server::planner::MediaKind::Tv,
                "",
                0,
                None
            )
        )
        .is_ok()
    );
    assert!(
        resume::check_resume_selection(
            &titles,
            &cfg,
            &crate::server::ripper::test_request(
                crate::server::planner::MediaKind::Tv,
                "",
                0,
                None
            ),
            true,
            true
        )
        .is_ok()
    );
}

#[test]
fn ambiguous_tv_resume_keeps_explicit_plan_but_refuses_reselection() {
    let cfg = Config::default();
    let titles = titles();
    assert!(
        resume::check_resume_selection(
            &titles,
            &cfg,
            &crate::server::ripper::test_request(
                crate::server::planner::MediaKind::Tv,
                "",
                0,
                None
            ),
            true,
            false
        )
        .is_ok()
    );
    needs_review(resume::check_resume_selection(
        &titles,
        &cfg,
        &crate::server::ripper::test_request(crate::server::planner::MediaKind::Tv, "", 0, None),
        true,
        true,
    ));
    needs_review(resume::check_resume_selection(
        &titles,
        &cfg,
        &crate::server::ripper::test_request(crate::server::planner::MediaKind::Tv, "", 0, None),
        false,
        false,
    ));
}

#[test]
fn iso_setting_does_not_authorize_ambiguous_resume_mux() {
    let cfg = Config {
        output_format: "iso".into(),
        ..Default::default()
    };
    needs_review(resume::check_resume_selection(
        &titles(),
        &cfg,
        &crate::server::ripper::test_request(crate::server::planner::MediaKind::Tv, "", 0, None),
        true,
        true,
    ));
}

#[test]
fn explicit_single_output_resume_preserves_nonzero_title_and_metadata() {
    let mut disc = crate::ku_fixture::bd_image().disc;
    disc.titles = titles();
    let plan = vec![staging::Output {
        title_index: 1,
        filename: "chosen-episode.mkv".into(),
        title_identity: Some(crate::title_identity::TitleIdentity::of(&disc.titles[1])),
        episode: Some(7),
        episode_name: "Chosen episode".into(),
        ..Default::default()
    }];
    assert!(
        resume::check_resume_selection(
            &disc.titles,
            &Config::default(),
            &crate::server::ripper::test_request(
                crate::server::planner::MediaKind::Tv,
                "",
                0,
                None
            ),
            true,
            false
        )
        .is_ok()
    );
    assert_eq!(resume::resume_titles(&disc, &plan).unwrap(), vec![1]);
    assert_eq!(
        resume::resume_primary_output(&plan, "wrong-movie.mkv".into()),
        plan[0]
    );
}

#[test]
fn explicit_resume_rejects_out_of_range_titles_without_movie_substitution() {
    let plan = vec![staging::Output {
        title_index: 9,
        filename: "chosen.mkv".into(),
        ..Default::default()
    }];
    assert!(resume::validate_resume_outputs(&titles(), &plan).is_err());
}

// A film labelled like a season ("SERIES_7") held by E9086: the operator's Movie
// correction lifts the hold, as the hold's advice says; a TMDB Movie guess does not
// (a season label outranks it, so a TV disc TMDB mismatched to a film stays TV).
#[test]
fn an_operator_movie_correction_overrides_a_season_label() {
    use crate::server::planner::{MediaKind, MediaMetadata, TitleRequest};
    let cfg = Config::default();
    needs_review(check_capture_selection(
        &titles(),
        &cfg,
        &crate::server::ripper::test_request(MediaKind::Movie, "SERIES_7", 0, None),
    ));
    let operator = MediaMetadata {
        title: "Series 7".into(),
        kind: Some(MediaKind::Movie),
        ..MediaMetadata::default()
    };
    let corrected = TitleRequest {
        media: MediaMetadata::resolve(&operator, None, &["SERIES_7"]),
        ..TitleRequest::default()
    };
    assert_eq!(corrected.media.kind, Some(MediaKind::Movie));
    assert!(check_capture_selection(&titles(), &cfg, &corrected).is_ok());
    assert!(resume::check_resume_selection(&titles(), &cfg, &corrected, false, true).is_ok());
    // The same correction saved with the job resolves the same way on resume.
    let mut saved = staging::DiscState::new(staging::StagingState::Ripped);
    saved.disc_label = "SERIES_7".into();
    saved.user_metadata = Some(staging::UserMetadata {
        title: "Series 7".into(),
        year: 0,
        media_type: MediaKind::Movie,
        episode_start: None,
        season: None,
        disc: None,
        tmdb_id: 0,
        poster_url: String::new(),
        overview: String::new(),
    });
    let request = resume::resume_request(Some(&saved), &titles(), MediaKind::Tv, "");
    assert_eq!(request.media.kind, Some(MediaKind::Movie));
    assert!(resume::check_resume_selection(&titles(), &cfg, &request, false, true).is_ok());
}
