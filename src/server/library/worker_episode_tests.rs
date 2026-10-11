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
