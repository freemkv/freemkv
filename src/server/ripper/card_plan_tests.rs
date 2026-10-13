use super::*;
use libfreemkv::disc::{ContentFormat, EpisodeEvidence, Extent};

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

// Three episodes on an authored roster, plus a short extra outside it.
fn episode_disc() -> Vec<libfreemkv::DiscTitle> {
    let mut titles = vec![
        title(3060.0, 100),
        title(3090.0, 2000),
        title(3100.0, 4000),
        title(60.0, 6000),
    ];
    let title_count = titles.len();
    for (index, t) in titles.iter_mut().enumerate() {
        t.playlist_id = index as u16;
        t.selection_evidence.episodes = EpisodeEvidence::Authored {
            roster: "test-menu".into(),
            title_count,
            ordinal: (index < 3).then_some(index),
        };
    }
    titles
}

fn tv_plan(
    titles: &[libfreemkv::DiscTitle],
    start: Option<u16>,
) -> Vec<super::super::staging::Output> {
    super::super::plan_mux_outputs(
        titles,
        &Config::default(),
        MediaKind::Tv,
        "Show Season 2 Disc 2",
        0,
        "Show.mkv",
        start,
    )
    .unwrap()
}

#[test]
fn an_episode_plan_lists_each_episode_with_its_length() {
    let titles = episode_disc();
    let rows = card_episodes(&titles, "Show Season 2 Disc 2", &tv_plan(&titles, Some(4)));
    let shown: Vec<_> = rows
        .iter()
        .map(|r| (r.season, r.episode, r.duration.as_str()))
        .collect();
    assert_eq!(
        shown,
        [(2, 4, "0h 51m"), (2, 5, "0h 51m"), (2, 6, "0h 51m")]
    );
}

#[test]
fn a_single_feature_lists_no_episodes() {
    let titles = episode_disc();
    let mut plan = tv_plan(&titles, None);
    plan.truncate(1);
    assert!(card_episodes(&titles, "Show", &plan).is_empty());
}

#[test]
fn an_output_without_an_episode_number_is_no_episode_plan() {
    let titles = episode_disc();
    let mut plan = tv_plan(&titles, None);
    plan[1].episode = None;
    assert!(card_episodes(&titles, "Show", &plan).is_empty());
}

#[test]
fn a_published_plan_is_found_by_job_and_a_movie_correction_clears_it() {
    let titles = episode_disc();
    let job = "disc-card-plan-test";
    publish(
        job,
        &titles,
        "Show Season 2 Disc 2",
        "Show.mkv",
        std::path::Path::new("/nonexistent/card-plan.mapfile"),
        &tv_plan(&titles, None),
    );
    assert_eq!(episodes(job).map(|e| e.len()), Some(3));
    assert!(episodes("disc-some-other-job").is_none());

    replan(Config::default(), job, MediaKind::Movie, 0, None);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while episodes(job).is_some() {
        assert!(std::time::Instant::now() < deadline, "replan never landed");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// title(3060 s, LBA 100): 3060 MB over 1000 sectors at 1 MB/s of playback.
#[test]
fn at_risk_time_is_the_damage_inside_the_title_at_its_rate() {
    let t = title(3060.0, 100);
    let sector = 2048u64;
    let inside = (150 * sector, 10 * sector);
    let outside = (5000 * sector, 10 * sector);
    let straddle = (1095 * sector, 10 * sector);
    let ms = |damage: &[(u64, u64)]| at_risk_ms(&t, damage);
    assert_eq!(ms(&[outside]), 0.0);
    assert!((ms(&[inside]) - 10.0 * 2048.0 / 1_000_000.0 * 1000.0).abs() < 1e-9);
    assert!((ms(&[straddle]) - 5.0 * 2048.0 / 1_000_000.0 * 1000.0).abs() < 1e-9);
}

#[test]
fn each_episode_measures_its_own_damage_from_the_mapfile() {
    let titles = episode_disc();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disc.iso.mapfile");
    let mut map = freemkv_engine::Mapfile::open_or_create(&path, 7000 * 2048, "test").unwrap();
    // Episode 2 (LBA 2000..3000) has 20 bad sectors; the rest of the disc reads clean.
    map.record(0, 7000 * 2048, freemkv_engine::SectorStatus::Finished)
        .unwrap();
    map.record(
        2300 * 2048,
        20 * 2048,
        freemkv_engine::SectorStatus::NonTrimmed,
    )
    .unwrap();
    map.flush().unwrap();
    let job = "disc-card-plan-risk-test";
    publish(
        job,
        &titles,
        "Show Season 2 Disc 2",
        "Show.mkv",
        &path,
        &tv_plan(&titles, None),
    );
    let risk: Vec<f64> = episodes(job)
        .unwrap()
        .iter()
        .map(|e| e.at_risk_ms)
        .collect();
    assert_eq!(risk[0], 0.0);
    assert!(risk[1] > 0.0);
    assert_eq!(risk[2], 0.0);
}
