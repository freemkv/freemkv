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
    assert!(
        matches!(result, Err(PlanError::SelectionNeedsReview(_))),
        "{result:?}"
    );
}

#[test]
fn ambiguous_tv_capture_requires_review_for_muxed_formats() {
    for output_format in ["mkv", "m2ts"] {
        let cfg = Config {
            output_format: output_format.into(),
            tv_auto: true,
            ..Default::default()
        };
        needs_review(check_capture_selection(&titles(), &cfg, "tv"));
    }
}

#[test]
fn ambiguous_tv_iso_capture_remains_available() {
    let cfg = Config {
        output_format: "iso".into(),
        tv_auto: true,
        ..Default::default()
    };
    assert!(check_capture_selection(&titles(), &cfg, "tv").is_ok());
}

#[test]
fn selection_guard_preserves_movie_and_disabled_tv_auto_policy() {
    let mut cfg = Config::default();
    for kind in ["movie", ""] {
        assert!(check_capture_selection(&titles(), &cfg, kind).is_ok());
    }
    cfg.tv_auto = false;
    assert!(check_capture_selection(&titles(), &cfg, "tv").is_ok());
}

#[test]
fn authored_tv_evidence_allows_capture_and_replan() {
    let mut titles = titles();
    let title_count = titles.len();
    for (ordinal, title) in titles.iter_mut().enumerate() {
        title.selection_evidence.episodes = EpisodeEvidence::Authored {
            roster: "test-authored-roster".into(),
            title_count,
            member: true,
            ordinal: Some(ordinal),
        };
    }
    let cfg = Config::default();
    assert!(check_capture_selection(&titles, &cfg, "tv").is_ok());
    assert!(resume::check_resume_selection(&titles, &cfg, "tv", true, true).is_ok());
}

#[test]
fn ambiguous_tv_resume_keeps_explicit_plan_but_refuses_reselection() {
    let cfg = Config::default();
    let titles = titles();
    assert!(resume::check_resume_selection(&titles, &cfg, "tv", true, false).is_ok());
    needs_review(resume::check_resume_selection(
        &titles, &cfg, "tv", true, true,
    ));
    needs_review(resume::check_resume_selection(
        &titles, &cfg, "tv", false, false,
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
        "tv",
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
        resume::check_resume_selection(&disc.titles, &Config::default(), "tv", true, false).is_ok()
    );
    assert_eq!(resume::resume_titles(&disc, false, &plan).unwrap(), vec![1]);
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
