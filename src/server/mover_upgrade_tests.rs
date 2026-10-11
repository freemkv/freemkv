// Upgrade from v1.8.3: rips it left in staging are delivered by this build's mover.
use super::*;
use crate::server::upgrade_fixture as fx;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, UNIX_EPOCH};

fn guard() -> std::sync::MutexGuard<'static, ()> {
    super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// The v1.8.3 settings bound to `root`, with its output folders mounted.
fn upgraded_config(root: &Path) -> Config {
    let mut cfg = fx::config(root);
    // The fixture's webhooks point at `.invalid` hosts; delivery is under test, not them.
    cfg.webhook_urls.clear();
    for d in ["out/movies", "out/tv", "out/iso", "config"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    cfg
}

fn no_error(dir: &Path) {
    let key = dir.to_string_lossy().to_string();
    let error = MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned();
    assert!(error.is_none(), "{key}: {error:?}");
}

fn links(root: &Path) -> std::collections::HashMap<PathBuf, PathBuf> {
    crate::server::library::links::load(&root.join("config"))
}

#[test]
fn v1_8_3_handed_off_rips_are_delivered_linked_and_cleaned() {
    let _g = guard();
    let (_t, root) = fx::root();
    fx::install("v1.8.3/staging", &root, "staging");
    fx::media(&root, "staging/");
    let cfg = upgraded_config(&root);

    check_and_move(&cfg);

    let out = root.join("out");
    let staging = root.join("staging");
    for name in ["Night Harbor", "Tidewater", "Paper Lantern"] {
        no_error(&staging.join(name));
        assert!(!staging.join(name).exists(), "{name} staging was cleaned");
    }
    let mkv = |p: &str| {
        let p = out.join(p);
        assert_eq!(std::fs::read(&p).unwrap(), fx::old_mkv(), "{}", p.display());
        p
    };
    let iso = |p: &str| {
        let p = out.join(p);
        assert_eq!(
            std::fs::metadata(&p).unwrap().len(),
            16384,
            "{}",
            p.display()
        );
        p
    };
    // Movie with its kept ISO.
    let movie = mkv("movies/Night Harbor (2017)/Night Harbor (2017).mkv");
    let movie_iso = iso("iso/Night Harbor (2017).iso");
    // TV: every episode the state lists, under the show's season folder.
    let e1 = mkv("tv/Tidewater (2019)/Season 01/Tidewater S01E01 - Pilot.mkv");
    let e2 = mkv("tv/Tidewater (2019)/Season 01/Tidewater S01E02.mkv");
    let tv_iso = iso("iso/Tidewater (2019).iso");
    // media_type "" files as a movie, as v1.8.3 did.
    let lantern = mkv("movies/Paper Lantern (1999)/Paper Lantern (1999).mkv");
    let lantern_iso = iso("iso/Paper Lantern (1999).iso");

    let links = links(&root);
    assert_eq!(links.get(&movie), Some(&movie_iso));
    assert_eq!(links.get(&e1), Some(&tv_iso));
    assert_eq!(links.get(&e2), Some(&tv_iso));
    assert_eq!(links.get(&lantern), Some(&lantern_iso));
    assert_eq!(links.len(), 4);
}

// A journal from the post-1.8.3 build (file_id, directory_id, destination_parent_id,
// published), cut off with the MKV delivered and the ISO not.
#[test]
fn a_dev_build_journal_mid_delivery_finishes_without_recopying_or_a_hold() {
    let _g = guard();
    let (_t, root) = fx::root();
    fx::install("dev-0eb9fd5/delivery/staging", &root, "staging");
    let cfg = upgraded_config(&root);
    let job = root.join("staging/disc-5e1c0a3f9b2d4e6f8a7b9c0d1e2f3a4b5c6d7e8f");
    let mkv = fx::old_mkv();
    assert_eq!(mkv.len(), 244, "the journal records a 244-byte MKV");
    std::fs::write(job.join("Long Shore.mkv"), &mkv).unwrap();
    fx::set_mtime(&job.join("Long Shore.mkv"), 1791696065, 978261908);
    std::fs::write(job.join("Long Shore.iso"), vec![0u8; 16384]).unwrap();
    fx::set_mtime(&job.join("Long Shore.iso"), 1791695974, 673465764);
    fx::set_mtime(&job.join("Long Shore.iso.mapfile"), 1791695974, 679148784);
    // The MKV already published: the even second the copy pinned.
    let delivered = root.join("out/movies/Long Shore (2017)/Long Shore (2017).mkv");
    std::fs::create_dir_all(delivered.parent().unwrap()).unwrap();
    std::fs::write(&delivered, &mkv).unwrap();
    fx::set_mtime(&delivered, 1791696064, 0);
    let before = std::fs::metadata(&delivered).unwrap();

    check_and_move(&cfg);

    no_error(&job);
    let after = std::fs::metadata(&delivered).unwrap();
    assert_eq!(
        after.ino(),
        before.ino(),
        "the delivered MKV was not copied again"
    );
    assert_eq!(
        after.modified().unwrap(),
        UNIX_EPOCH + Duration::from_secs(1791696064)
    );
    let iso = root.join("out/iso/Long Shore (2017).iso");
    assert_eq!(std::fs::metadata(&iso).unwrap().len(), 16384);
    assert!(!job.exists(), "staging cleaned once both are in place");
    assert_eq!(links(&root).get(&delivered), Some(&iso));
}

#[test]
fn media_type_empty_movie_tv_or_missing_reads_as_v1_8_3_meant() {
    use crate::server::muxer::RippedMarker;
    use crate::server::planner::MediaKind;
    let (_t, root) = fx::root();
    for (name, kind) in [
        ("movie", MediaKind::Movie),
        ("tv", MediaKind::Tv),
        ("empty", MediaKind::Movie),
    ] {
        let mut ripped: serde_json::Value = serde_json::from_str(&fx::text(
            &format!("v1.8.3/markers/ripped-{name}.json"),
            &root,
        ))
        .unwrap();
        let m: RippedMarker = serde_json::from_value(ripped.clone()).unwrap();
        assert_eq!(m.tmdb_media_type, kind, "ripped-{name}");
        ripped.as_object_mut().unwrap().remove("tmdb_media_type");
        let m: RippedMarker = serde_json::from_value(ripped).unwrap();
        assert_eq!(
            m.tmdb_media_type,
            MediaKind::Movie,
            "ripped-{name} without the field"
        );

        let mut tmdb: serde_json::Value = serde_json::from_str(&fx::text(
            &format!("v1.8.3/markers/tmdb-{name}.json"),
            &root,
        ))
        .unwrap();
        let t: tmdb::TmdbResult = serde_json::from_value(tmdb.clone()).unwrap();
        assert_eq!((t.media_type, t.tmdb_id), (kind, 7), "tmdb-{name}");
        tmdb.as_object_mut().unwrap().remove("media_type");
        let t: tmdb::TmdbResult = serde_json::from_value(tmdb).unwrap();
        assert_eq!(
            t.media_type,
            MediaKind::Movie,
            "tmdb-{name} without the field"
        );
    }
    // The staged state of each kind, and with the field gone.
    for (dir, kind) in [
        ("Night Harbor", MediaKind::Movie),
        ("Tidewater", MediaKind::Tv),
        ("Paper Lantern", MediaKind::Movie),
    ] {
        let rel = format!("v1.8.3/staging/{dir}/state.json");
        let mut st: serde_json::Value = serde_json::from_str(&fx::text(&rel, &root)).unwrap();
        let parsed: crate::server::ripper::staging::DiscState =
            serde_json::from_value(st.clone()).unwrap();
        assert_eq!(parsed.media_type, kind, "{dir}");
        st.as_object_mut().unwrap().remove("media_type");
        let parsed: crate::server::ripper::staging::DiscState = serde_json::from_value(st).unwrap();
        assert_eq!(
            parsed.media_type,
            MediaKind::Movie,
            "{dir} without the field"
        );
    }
}

// v1.8.3 copied the MKV into the library and was stopped (or could not unlink the
// source) before the ISO: both copies of the MKV exist and no journal does. v1.8.3's
// next tick recognised its own copy; this build must not file the rip again as `_2`.
#[test]
fn a_v1_8_3_delivery_cut_off_after_the_mkv_landed_is_not_delivered_twice() {
    let _g = guard();
    let (_t, root) = fx::root();
    fx::install("v1.8.3/staging/Night Harbor", &root, "staging/Night Harbor");
    fx::media(&root, "staging/Night Harbor/");
    let cfg = upgraded_config(&root);
    let landed = root.join("out/movies/Night Harbor (2017)/Night Harbor (2017).mkv");
    std::fs::create_dir_all(landed.parent().unwrap()).unwrap();
    std::fs::write(&landed, fx::old_mkv()).unwrap();

    crate::server::ripper::staging::resume_or_quarantine_staging(&cfg.staging_dir);
    check_and_move(&cfg);

    let job = root.join("staging/Night Harbor");
    no_error(&job);
    for dup in [
        "out/movies/Night Harbor (2017)/Night Harbor (2017)_2.mkv",
        "out/iso/Night Harbor (2017)_2.iso",
    ] {
        assert!(
            !root.join(dup).exists(),
            "{dup} is a second copy of the same rip"
        );
    }
    let iso = root.join("out/iso/Night Harbor (2017).iso");
    assert_eq!(std::fs::metadata(&iso).unwrap().len(), 16384);
    assert_eq!(std::fs::read(&landed).unwrap(), fx::old_mkv());
    assert!(!job.exists(), "staging cleaned");
    assert_eq!(links(&root).get(&landed), Some(&iso));
}

// The same cut-off when the MKV was moved (its staged copy already gone).
#[test]
fn a_v1_8_3_delivery_cut_off_after_the_mkv_moved_files_the_rest() {
    let _g = guard();
    let (_t, root) = fx::root();
    fx::install("v1.8.3/staging/Night Harbor", &root, "staging/Night Harbor");
    fx::media(&root, "staging/Night Harbor/");
    let cfg = upgraded_config(&root);
    let job = root.join("staging/Night Harbor");
    let landed = root.join("out/movies/Night Harbor (2017)/Night Harbor (2017).mkv");
    std::fs::create_dir_all(landed.parent().unwrap()).unwrap();
    std::fs::rename(job.join("Night Harbor.mkv"), &landed).unwrap();

    crate::server::ripper::staging::resume_or_quarantine_staging(&cfg.staging_dir);
    check_and_move(&cfg);

    no_error(&job);
    assert_eq!(
        std::fs::metadata(root.join("out/iso/Night Harbor (2017).iso"))
            .unwrap()
            .len(),
        16384
    );
    assert!(!root.join("out/iso/Night Harbor (2017)_2.iso").exists());
    assert_eq!(std::fs::read(&landed).unwrap(), fx::old_mkv());
    assert!(!job.exists(), "staging cleaned");
}

// What v1.8.3 did not file is not taken over: a different file of the same size is
// another disc's (filed beside it as `_2`), and a moved-away output found nowhere holds.
#[test]
fn a_v1_8_3_cut_off_claims_only_its_own_files() {
    let _g = guard();
    let (_t, root) = fx::root();
    fx::install("v1.8.3/staging/Night Harbor", &root, "staging/Night Harbor");
    fx::media(&root, "staging/Night Harbor/");
    let cfg = upgraded_config(&root);
    let job = root.join("staging/Night Harbor");
    let base = root.join("out/movies/Night Harbor (2017)/Night Harbor (2017).mkv");
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    let mut other = fx::old_mkv();
    let n = other.len();
    other[n / 2] ^= 0xFF;
    other[n - 1] ^= 0xFF;
    std::fs::write(&base, &other).unwrap();
    check_and_move(&cfg);
    no_error(&job);
    assert_eq!(
        std::fs::read(&base).unwrap(),
        other,
        "the other file is untouched"
    );
    assert_eq!(
        std::fs::read(root.join("out/movies/Night Harbor (2017)/Night Harbor (2017)_2.mkv"))
            .unwrap(),
        fx::old_mkv()
    );
    assert!(root.join("out/iso/Night Harbor (2017)_2.iso").exists());

    let (_t, root) = fx::root();
    fx::install("v1.8.3/staging/Night Harbor", &root, "staging/Night Harbor");
    fx::media(&root, "staging/Night Harbor/");
    let cfg = upgraded_config(&root);
    let job = root.join("staging/Night Harbor");
    std::fs::remove_file(job.join("Night Harbor.mkv")).unwrap();
    check_and_move(&cfg);
    let key = job.to_string_lossy().to_string();
    let error = MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    assert!(
        error.is_some_and(|e| e.reason.contains("a planned output is missing")),
        "a lost output with nothing filed holds"
    );
    assert!(
        job.join("Night Harbor.iso").exists(),
        "the rest stays staged"
    );
    assert!(!root.join("out/iso/Night Harbor (2017).iso").exists());
}
