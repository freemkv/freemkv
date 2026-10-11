// Upgrade from v1.8.3: captures it left unmuxed. A BD/AACS capture is found again by its
// disc hash and resumes; a DVD's (no AACS) is left untouched and a fresh capture starts
// beside it, as v1.8.3 recorded no identity for it (documented upgrade behaviour).
use super::*;
use crate::server::ripper::{jobs, staging};
use crate::server::upgrade_fixture as fx;

const BD_HASH: &str = "0x0123456789ABCDEF0123456789ABCDEF01234567";

fn files_under(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            let len = std::fs::metadata(&p).unwrap().len();
            (p, len)
        })
        .collect();
    out.sort();
    out
}

// `case` (capture-crashed, capture-stopped, capture-ripped) installed as `root/staging`.
fn upgraded(case: &str) -> (tempfile::TempDir, PathBuf, Config) {
    let (t, top) = fx::root();
    // Where v1.8.3 wrote it, so the paths its state records still resolve.
    fx::install(
        &format!("v1.8.3/{case}/staging"),
        &top,
        &format!("{case}/staging"),
    );
    fx::media(&top, case);
    let root = top.join(case);
    let cfg = Config {
        staging_dir: root.join("staging").to_string_lossy().into_owned(),
        ..Config::default()
    };
    (t, root, cfg)
}

fn bd_disc() -> libfreemkv::Disc {
    let mut disc = crate::ku_fixture::bd_image().disc;
    disc.aacs.as_mut().unwrap().disc_hash = BD_HASH.into();
    disc
}

fn dvd_disc() -> (libfreemkv::Disc, libfreemkv::test_util::MemSource) {
    let mut disc = crate::ku_fixture::bd_image().disc;
    disc.aacs = None;
    disc.format = libfreemkv::DiscFormat::Dvd;
    disc.content_format = libfreemkv::ContentFormat::DvdPs;
    let image: Vec<u8> = (0..512 * 2048).map(|i| (i / 2048) as u8).collect();
    disc.capacity_bytes = image.len() as u64;
    (disc, libfreemkv::test_util::MemSource::new(image))
}

#[test]
fn a_v1_8_3_bd_capture_is_found_by_its_disc_hash_and_resumes() {
    for case in ["capture-crashed", "capture-stopped"] {
        let (_t, root, cfg) = upgraded(case);
        let job = root.join("staging/Ember Falls");
        let before = files_under(&job);
        // Startup: the sweep keeps it.
        staging::resume_or_quarantine_staging(&cfg.staging_dir);
        assert!(
            job.join("Ember Falls.iso").exists() && job.join("Ember Falls.iso.mapfile").exists()
        );
        // The disc is inserted: its AACS hash finds the v1.8.3 dir (named by title).
        let mut reader =
            libfreemkv::test_util::MemSource::new(crate::ku_fixture::bd_image().img.image);
        let identity = jobs::fingerprint(&bd_disc(), &mut reader).unwrap();
        let found = jobs::locate(&root.join("staging"), &identity).unwrap();
        assert_eq!(found, "Ember Falls", "{case}");
        // Half the disc is still unread: the sweep continues from the mapfile.
        assert_eq!(
            super::super::resumable_for_job(&cfg, &found),
            Some(super::super::state::Resumable::Sweep),
            "{case}"
        );
        let after = files_under(&job);
        for (path, len) in before
            .iter()
            .filter(|(p, _)| p.extension().is_some_and(|e| e == "iso" || e == "mapfile"))
        {
            assert!(
                after.contains(&(path.clone(), *len)),
                "{case}: {} kept",
                path.display()
            );
        }
    }
}

// A capture v1.8.3 finished but never muxed has no title identity proof (v1.8.3 never
// wrote one). Through the mux worker: held for one Review confirmation with the ISO kept,
// then, once confirmed, muxed from the saved ISO without the disc.
#[test]
fn a_v1_8_3_capture_awaiting_mux_is_held_for_review_then_muxes() {
    let (t, root, mut cfg) = upgraded("capture-ripped");
    let job = root.join("staging/Ember Falls");
    staging::resume_or_quarantine_staging(&cfg.staging_dir);
    let mut reader = libfreemkv::test_util::MemSource::new(crate::ku_fixture::bd_image().img.image);
    let identity = jobs::fingerprint(&bd_disc(), &mut reader).unwrap();
    assert_eq!(
        jobs::locate(&root.join("staging"), &identity).unwrap(),
        "Ember Falls"
    );
    // The mux worker owns a Ripped capture; an insert does not race it.
    assert_eq!(super::super::resumable_for_job(&cfg, "Ember Falls"), None);
    let st = staging::read_state(&job).expect("v1.8.3 state reads");
    assert_eq!(
        (st.state, st.title.as_str()),
        (staging::StagingState::Ripped, "Ember Falls")
    );
    assert!(st.outputs.iter().all(|o| o.title_identity.is_none()));

    // The fixture's image stands in for the captured bytes (v1.8.3's state is kept as is),
    // with the clip's unit key so the mux can run.
    let fx_image = crate::ku_fixture::bd_image();
    let iso = job.join("Ember Falls.iso");
    std::fs::write(&iso, &fx_image.img.image).unwrap();
    let mapfile = crate::ku_fixture::write_sidecar(&fx_image, &iso, true);
    let keydb = t.path().join("keydb.cfg");
    let hash = libfreemkv::hex::strip_hex_prefix(&fx_image.disc.aacs.as_ref().unwrap().disc_hash)
        .to_owned();
    let k1: String = crate::ku_fixture::K1
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(&keydb, format!("0x{hash} = KU | U | 1-0x{k1}\n")).unwrap();
    cfg.keydb_path = Some(keydb.to_string_lossy().into_owned());
    cfg.keep_iso = true;
    let before = (
        std::fs::read(&iso).unwrap(),
        std::fs::read(&mapfile).unwrap(),
    );
    let cfg = Arc::new(RwLock::new(cfg));
    let mux = |marker: &crate::server::muxer::RippedMarker| {
        let _env = crate::server::log::env_guard();
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: env access in tests, serialized by env_guard.
        unsafe {
            std::env::set_var("AUTORIP_DIR", t.path());
        }
        remux_from_ripped_marker(&cfg, &job, marker)
    };

    let held = mux(&st.to_ripped_marker());
    assert!(!held.success);
    assert!(
        held.failure_reason
            .as_deref()
            .is_some_and(|r| r.contains("Review queue")),
        "{:?}",
        held.failure_reason
    );
    let st = staging::read_state(&job).unwrap();
    assert_eq!(st.state, staging::StagingState::Review);
    assert!(st.title_identity_review);
    assert_eq!(
        (
            std::fs::read(&iso).unwrap(),
            std::fs::read(&mapfile).unwrap()
        ),
        before
    );

    // One confirmation in the Review queue, and the mux finishes from the saved ISO.
    staging::save_review_metadata(
        &job,
        staging::UserMetadata {
            title: "Ember Falls".into(),
            year: 2021,
            media_type: crate::server::planner::MediaKind::Movie,
            tmdb_id: 0,
            episode_start: None,
            poster_url: String::new(),
            overview: String::new(),
        },
    )
    .unwrap();
    let marker = staging::read_state(&job).unwrap().to_ripped_marker();
    let done = mux(&marker);
    assert!(done.success, "{:?}", done.failure_reason);
    let st = staging::read_state(&job).unwrap();
    assert!(st.outputs.iter().all(|o| o.title_identity.is_some()));
    assert_eq!(
        (
            std::fs::read(&iso).unwrap(),
            std::fs::read(&mapfile).unwrap()
        ),
        before
    );
}

#[test]
fn a_v1_8_3_dvd_capture_is_left_untouched_and_a_fresh_capture_starts() {
    for case in ["capture-crashed", "capture-stopped"] {
        let (_t, root, cfg) = upgraded(case);
        let old = root.join("staging/Quiet Meadow");
        let before = files_under(&old);
        staging::resume_or_quarantine_staging(&cfg.staging_dir);
        let (disc, mut reader) = dvd_disc();
        let identity = jobs::fingerprint(&disc, &mut reader).unwrap();
        assert!(identity.starts_with("sample-v1-"), "{identity}");
        let job = jobs::locate(&root.join("staging"), &identity).unwrap();
        assert_eq!(
            job,
            format!("disc-{identity}"),
            "{case}: a new job, not the old dir"
        );
        assert_eq!(
            super::super::resumable_for_job(&cfg, &job),
            None,
            "{case}: starts fresh"
        );
        // The old capture stays exactly as v1.8.3 left it (bar the startup restart count).
        let after = files_under(&old);
        for (path, len) in &before {
            let name = path.file_name().unwrap();
            if name == "state.json" {
                assert!(path.exists(), "{case}");
                continue;
            }
            assert!(
                after.contains(&(path.clone(), *len)),
                "{case}: {} kept",
                path.display()
            );
        }
        // A second start changes nothing either.
        staging::resume_or_quarantine_staging(&cfg.staging_dir);
        assert!(
            old.join("Quiet Meadow.iso").exists() && old.join("Quiet Meadow.iso.mapfile").exists()
        );
    }
}
