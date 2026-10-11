// Upgrade from v1.8.3: captures it left unmuxed. A BD/AACS capture is found again by
// its disc hash and resumes; a DVD's (no AACS) is left untouched and a fresh capture
// starts beside it, because v1.8.3 recorded no identity for it (documented upgrade
// behaviour).
use super::*;
use crate::server::ripper::{jobs, staging};
use crate::server::upgrade_fixture as fx;

const BD_HASH: &str = "0xA1B2C3D4E5F60718293A4B5C6D7E8F9012345678";

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
    let (t, root) = fx::root();
    fx::install(&format!("v1.8.3/{case}/staging"), &root, "staging");
    let list: Vec<(String, u64, String)> =
        serde_json::from_str(&fx::text("v1.8.3/media.json", &root)).unwrap();
    for (rel, size, _) in list.into_iter().filter(|m| m.0.starts_with(case)) {
        let p = root
            .join("staging")
            .join(rel.split_once("/staging/").unwrap().1);
        std::fs::write(p, vec![0u8; size as usize]).unwrap();
    }
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
// wrote one): its mux holds it for one Review confirmation, keeping the ISO.
#[test]
fn a_v1_8_3_capture_awaiting_mux_is_held_for_review_not_lost() {
    let (_t, root, cfg) = upgraded("capture-ripped");
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
    assert_eq!(st.state, staging::StagingState::Ripped);
    assert_eq!(st.title, "Ember Falls");
    let err = rebind_resume_outputs(&bd_disc().titles, &st.outputs).unwrap_err();
    assert!(err.contains("predates title identity proof"), "{err}");
    let (iso, map) = find_iso_and_mapfile(&job).unwrap();
    staging::hold_title_identity_review(&job, &iso, &map, &err).unwrap();
    let held = staging::read_state(&job).unwrap();
    assert_eq!(held.state, staging::StagingState::Review);
    assert!(held.title_identity_review);
    assert_eq!(std::fs::metadata(&iso).unwrap().len(), 32768);
    assert!(map.exists());
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
