use super::*;
use crate::server::library::links;
use crate::server::planner::{MediaKind, MediaMetadata, PlannedOutput, RemuxPlan};
use std::path::PathBuf;

fn no_key_tv_auto(dirs: &Dirs) -> Config {
    Config {
        library_dir: dirs.library.to_string_lossy().into_owned(),
        tv_auto: true,
        ..Config::default()
    }
}

// A real image at `<isos>/<name>.iso`.
fn image_iso(dirs: &Dirs, name: &str) -> PathBuf {
    let iso = dirs.isos.as_ref().unwrap().join(format!("{name}.iso"));
    std::fs::create_dir_all(iso.parent().unwrap()).unwrap();
    std::fs::write(&iso, &crate::ku_fixture::bd_image().img.image).unwrap();
    iso
}

#[test]
fn an_episode_row_is_never_planned_as_one_movie_over_the_episode() {
    let (t, lib, dirs) = library_with(&[]);
    let iso = image_iso(&dirs, "Show");
    let episodes: Vec<PathBuf> = (1..=2)
        .map(|e| dirs.library.join(format!("Show/Show S01E0{e}.mkv")))
        .collect();
    std::fs::create_dir_all(dirs.library.join("Show")).unwrap();
    for e in &episodes {
        std::fs::write(e, b"an episode").unwrap();
    }
    // The ripper fanned the disc out; the mover linked every episode to the ISO.
    let delivered: Vec<String> = episodes
        .iter()
        .chain([&iso])
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    links::record_delivery(
        t.path().join("config").to_str().unwrap(),
        delivered.iter().map(String::as_str),
    )
    .unwrap();
    lib.queue.add(vec![NewJob {
        title: "Show S01E01".into(),
        iso,
        target: episodes[0].clone(),
        replace: true,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    let error = maybe_attach_tv_plan(&lib, &no_key_tv_auto(&dirs), &mut job).unwrap_err();
    assert!(error.to_string().contains("Change match"), "{error}");
    // With automatic TV planning off nothing was compared: the text says only what is true.
    let off = Config {
        tv_auto: false,
        ..no_key_tv_auto(&dirs)
    };
    for error in [
        error,
        maybe_attach_tv_plan(&lib, &off, &mut job).unwrap_err(),
    ] {
        let text = error.to_string();
        assert!(
            text.contains("could not find which of the disc's titles"),
            "{text}"
        );
        assert!(!text.contains("cannot be told apart"), "{text}");
    }
    assert!(job.plan.is_none());
    assert!(lib.queue.snapshot().jobs[0].plan.is_none());
    assert_eq!(std::fs::read(&episodes[0]).unwrap(), b"an episode");
}

#[test]
fn a_season_label_plans_tv_without_a_tmdb_key_as_the_ripper_does() {
    let (_t, lib, dirs) = library_with(&[]);
    let iso = image_iso(&dirs, "SHOW_S2_D1");
    lib.queue.add(vec![NewJob {
        title: "SHOW_S2_D1".into(),
        iso,
        target: dirs.library.join("SHOW_S2_D1/SHOW_S2_D1.mkv"),
        replace: false,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    maybe_attach_tv_plan(&lib, &no_key_tv_auto(&dirs), &mut job).unwrap();
    let plan = job.plan.expect("planned");
    assert_eq!(plan.media.kind, Some(MediaKind::Tv), "{plan:?}");
    assert_eq!(plan.media.season, Some(2));
}

#[test]
fn an_episode_row_keeps_only_the_planned_output_that_lands_on_it() {
    let output = |n: usize| PlannedOutput {
        id: format!("episode-{n}"),
        title_index: n,
        episode: Some(n as u16),
        episode_name: String::new(),
        filename: format!("e{n}.mkv"),
    };
    let plan = RemuxPlan {
        media: MediaMetadata::default(),
        outputs: vec![output(1), output(2)],
    };
    let targets = vec![PathBuf::from("/tv/e1.mkv"), PathBuf::from("/tv/e2.mkv")];
    let (kept, at) = only_the_row(plan.clone(), targets.clone(), Path::new("/tv/e2.mkv")).unwrap();
    assert_eq!(kept.outputs, vec![output(2)]);
    assert_eq!(at, vec![PathBuf::from("/tv/e2.mkv")]);
    assert!(only_the_row(plan, targets, Path::new("/tv/E2.mkv")).is_none());
}

// Episode files at `paths`, delivered from `iso` and linked as its episodes.
fn delivered_episodes(t: &tempfile::TempDir, iso: &Path, paths: &[PathBuf]) {
    for p in paths {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"an episode").unwrap();
    }
    let delivered: Vec<String> = paths
        .iter()
        .map(|p| p.as_path())
        .chain([iso])
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    links::record_delivery(
        t.path().join("config").to_str().unwrap(),
        delivered.iter().map(String::as_str),
    )
    .unwrap();
}

#[test]
fn unreadable_links_never_plan_an_episode_row_as_one_movie() {
    let (t, lib, dirs) = library_with(&[]);
    let iso = image_iso(&dirs, "Show");
    let episodes: Vec<PathBuf> = (1..=2)
        .map(|e| dirs.library.join(format!("Show/Show S01E0{e}.mkv")))
        .collect();
    delivered_episodes(&t, &iso, &episodes);
    lib.queue.add(vec![NewJob {
        title: "Show S01E01".into(),
        iso,
        target: episodes[0].clone(),
        replace: true,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    let config = t.path().join("config");
    std::fs::write(config.join(links::LINKS_FILE), b"{ damaged").unwrap();
    let off = Config {
        tv_auto: false,
        ..no_key_tv_auto(&dirs)
    };
    for cfg in [off, no_key_tv_auto(&dirs)] {
        let error = maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap_err();
        assert!(error.to_string().contains(links::LINKS_FILE), "{error}");
        assert!(job.plan.is_none());
        let error = capture_owned(&job, &cfg, &config).unwrap_err();
        assert!(error.to_string().contains(links::LINKS_FILE), "{error}");
    }
}

#[test]
fn remux_of_an_episode_row_with_a_saved_match_never_remuxes_the_whole_disc() {
    let (t, lib, dirs) = library_with(&[]);
    let iso = image_iso(&dirs, "Show");
    let cfg = Config {
        tv_auto: false,
        ..no_key_tv_auto(&dirs)
    };
    let episodes: Vec<PathBuf> = (1..=2)
        .map(|e| dirs.library.join(format!("Show/Show S01E0{e}.mkv")))
        .collect();
    delivered_episodes(&t, &iso, &episodes);
    let media = MediaMetadata {
        title: "Show".into(),
        year: 2001,
        tmdb_id: 7,
        season: Some(1),
        kind: Some(MediaKind::Tv),
        ..Default::default()
    };
    lib.match_store().unwrap().save(&iso, 0, media).unwrap();
    lib.queue.add(vec![NewJob {
        title: "Show S01E02".into(),
        iso,
        target: episodes[1].clone(),
        replace: true,
    }]);
    let mut job = lib.queue.claim_next().unwrap();
    // No output of the corrected plan lands on this row: nothing is planned over it.
    let error = maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap_err();
    assert!(error.to_string().contains("Change match"), "{error}");
    assert!(job.plan.is_none());
}

// Title `k` of three on a menu that authors all three as episodes.
fn episode(k: u32, secs: f64) -> libfreemkv::DiscTitle {
    let mut title = libfreemkv::DiscTitle {
        selection_evidence: Default::default(),
        playlist: String::new(),
        playlist_id: k as u16,
        duration_secs: secs,
        size_bytes: (secs as u64) * 1_000_000,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![libfreemkv::disc::Extent {
            start_lba: 1000 + k * 100,
            sector_count: 1000,
        }],
        content_format: libfreemkv::disc::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    };
    title.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
        roster: "test-menu".into(),
        title_count: 3,
        ordinal: Some(k as usize),
    };
    title
}

#[test]
fn a_corrected_plan_for_an_episode_row_keeps_only_its_output() {
    let ep = 44.0 * 60.0;
    let titles: Vec<libfreemkv::DiscTitle> =
        (0..3u32).map(|k| episode(k, ep + f64::from(k))).collect();
    let cfg = Config {
        library_dir: "/lib".into(),
        ..Config::default()
    };
    let media = MediaMetadata {
        title: "Show".into(),
        year: 2001,
        season: Some(1),
        kind: Some(MediaKind::Tv),
        ..Default::default()
    };
    let (whole, targets) = corrected_plan_for(&cfg, &titles, &media, None).unwrap();
    assert_eq!(whole.outputs.len(), 3);
    let (one, at) = corrected_plan_for(&cfg, &titles, &media, Some(&targets[1])).unwrap();
    assert_eq!(one.outputs, vec![whole.outputs[1].clone()]);
    assert_eq!(at, vec![targets[1].clone()]);
    let elsewhere = Path::new("/lib/Show/other.mkv");
    assert!(corrected_plan_for(&cfg, &titles, &media, Some(elsewhere)).is_err());
}

// Remux on an episode row whose ISO has a current saved match goes in through the saved
// match (no replace); it still plans only that episode, never the whole disc.
#[test]
fn an_episode_row_admitted_with_a_saved_match_plans_only_that_episode() {
    let (t, lib, dirs) = library_with(&[]);
    let iso = image_iso(&dirs, "Show");
    let cfg = Config {
        tv_auto: false,
        ..no_key_tv_auto(&dirs)
    };
    let episodes: Vec<PathBuf> = (1..=2)
        .map(|e| dirs.library.join(format!("Show/Show S01E0{e}.mkv")))
        .collect();
    delivered_episodes(&t, &iso, &episodes);
    let media = MediaMetadata {
        title: "Show".into(),
        year: 2001,
        tmdb_id: 7,
        season: Some(1),
        kind: Some(MediaKind::Tv),
        ..Default::default()
    };
    lib.match_store().unwrap().save(&iso, 0, media).unwrap();
    lib.index_now(&dirs);
    let picked = lib.enqueue(&dirs, |r| {
        r.target.as_deref() == Some(episodes[1].as_path())
    });
    assert_eq!(picked, 1);
    let mut job = lib.queue.claim_next().unwrap();
    assert!(!job.replace, "admitted through the saved match");
    let error = maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap_err();
    assert!(error.to_string().contains("Change match"), "{error}");
    assert!(job.plan.is_none());
}

// Remux on several episode rows of a matched ISO while links.json cannot be read refuses
// with that reason; it never drops all but one row as a whole-disc job.
#[test]
fn matched_episode_rows_are_refused_while_links_cannot_be_read() {
    let (t, lib, dirs) = library_with(&[]);
    let iso = image_iso(&dirs, "Show");
    let episodes: Vec<PathBuf> = (1..=3)
        .map(|e| dirs.library.join(format!("Show/Show S01E0{e}.mkv")))
        .collect();
    delivered_episodes(&t, &iso, &episodes);
    let media = MediaMetadata {
        title: "Show".into(),
        year: 2001,
        tmdb_id: 7,
        season: Some(1),
        kind: Some(MediaKind::Tv),
        ..Default::default()
    };
    lib.match_store().unwrap().save(&iso, 0, media).unwrap();
    lib.index_now(&dirs);
    std::fs::write(t.path().join("config").join(links::LINKS_FILE), b"{damaged").unwrap();
    let refused = lib
        .enqueue_checked(&dirs, |r| r.iso.as_deref() == Some(iso.as_path()))
        .unwrap_err();
    assert!(refused.contains(links::LINKS_FILE), "{refused}");
    assert!(refused.contains("nothing was queued"), "{refused}");
    assert!(!refused.contains("Retry"), "{refused}");
    assert!(lib.queue.snapshot().jobs.is_empty(), "no row was queued");
}

// A request mixing rows with a saved match and plain rows is refused whole: no plain row
// slips into the queue while the matched ones cannot be planned.
#[test]
fn matched_and_plain_rows_are_refused_together_while_links_cannot_be_read() {
    let (t, lib, dirs) = library_with(&[]);
    let iso = image_iso(&dirs, "Show");
    let plain = image_iso(&dirs, "Plain");
    let episodes: Vec<PathBuf> = (1..=3)
        .map(|e| dirs.library.join(format!("Show/Show S01E0{e}.mkv")))
        .collect();
    delivered_episodes(&t, &iso, &episodes);
    let media = MediaMetadata {
        title: "Show".into(),
        year: 2001,
        tmdb_id: 7,
        season: Some(1),
        kind: Some(MediaKind::Tv),
        ..Default::default()
    };
    lib.match_store().unwrap().save(&iso, 0, media).unwrap();
    lib.index_now(&dirs);
    std::fs::write(t.path().join("config").join(links::LINKS_FILE), b"{damaged").unwrap();
    let refused = lib
        .enqueue_checked(&dirs, |r| {
            r.iso.as_deref() == Some(iso.as_path()) || r.iso.as_deref() == Some(plain.as_path())
        })
        .unwrap_err();
    assert!(refused.contains(links::LINKS_FILE), "{refused}");
    assert!(refused.contains("nothing was queued"), "{refused}");
    assert!(!refused.contains("Retry"), "{refused}");
    assert!(
        lib.queue.snapshot().jobs.is_empty(),
        "no row was queued, plain or matched"
    );
}
