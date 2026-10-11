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
    // The fixture's webhooks point at `.test` hosts; delivery is under test, not them.
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

// The staged Night Harbor hand-off v1.8.3 wrote, mounted under a fresh root.
fn night_harbor() -> (tempfile::TempDir, PathBuf, Config, PathBuf) {
    let (t, root) = fx::root();
    fx::install("v1.8.3/staging/Night Harbor", &root, "staging/Night Harbor");
    fx::media(&root, "staging/Night Harbor/");
    let cfg = upgraded_config(&root);
    let job = root.join("staging/Night Harbor");
    std::fs::create_dir_all(root.join("out/movies/Night Harbor (2017)")).unwrap();
    (t, root, cfg, job)
}

const BASE_MKV: &str = "out/movies/Night Harbor (2017)/Night Harbor (2017).mkv";
const SECOND_MKV: &str = "out/movies/Night Harbor (2017)/Night Harbor (2017)_2.mkv";
const BASE_ISO: &str = "out/iso/Night Harbor (2017).iso";
const SECOND_ISO: &str = "out/iso/Night Harbor (2017)_2.iso";

fn take_error(job: &Path) -> Option<MoverError> {
    MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&*job.to_string_lossy())
}

fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(tree(&p));
        } else {
            out.push((p.clone(), std::fs::read(&p).unwrap()));
        }
    }
    out.sort();
    out
}

// v1.8.3 copied the MKV in and was stopped (or could not unlink it) before the ISO. With
// no journal its copy is never adopted: the rip is filed again beside it as `_2`.
#[test]
fn an_existing_library_copy_is_never_adopted() {
    let _g = guard();
    let (_t, root, cfg, job) = night_harbor();
    std::fs::write(root.join(BASE_MKV), fx::old_mkv()).unwrap();
    crate::server::ripper::staging::resume_or_quarantine_staging(&cfg.staging_dir);
    check_and_move(&cfg);
    no_error(&job);
    assert_eq!(std::fs::read(root.join(BASE_MKV)).unwrap(), fx::old_mkv());
    assert_eq!(std::fs::read(root.join(SECOND_MKV)).unwrap(), fx::old_mkv());
    assert_eq!(
        std::fs::metadata(root.join(SECOND_ISO)).unwrap().len(),
        16384
    );
    assert!(!root.join(BASE_ISO).exists());
    assert!(!job.exists(), "staging cleaned");

    // The same for an ISO already at its name: one variant for the whole rip.
    let (_t, root, cfg, job) = night_harbor();
    std::fs::write(root.join(BASE_ISO), vec![0u8; 16384]).unwrap();
    check_and_move(&cfg);
    no_error(&job);
    assert_eq!(
        std::fs::read(root.join(BASE_ISO)).unwrap(),
        vec![0u8; 16384]
    );
    assert_eq!(
        std::fs::metadata(root.join(SECOND_ISO)).unwrap().len(),
        16384
    );
    assert_eq!(std::fs::read(root.join(SECOND_MKV)).unwrap(), fx::old_mkv());
    assert!(!root.join(BASE_MKV).exists());
}

// An output v1.8.3 already moved away is dropped: not delivered, claimed or linked, whatever
// sits at its name; the rest is delivered as usual.
#[test]
fn a_moved_away_output_is_dropped_and_the_rest_delivered() {
    let _g = guard();
    let (_t, root, cfg, job) = night_harbor();
    std::fs::rename(job.join("Night Harbor.mkv"), root.join(BASE_MKV)).unwrap();
    crate::server::ripper::staging::resume_or_quarantine_staging(&cfg.staging_dir);
    check_and_move(&cfg);
    no_error(&job);
    assert_eq!(std::fs::read(root.join(BASE_MKV)).unwrap(), fx::old_mkv());
    assert_eq!(std::fs::metadata(root.join(BASE_ISO)).unwrap().len(), 16384);
    assert!(!root.join(SECOND_MKV).exists() && !root.join(SECOND_ISO).exists());
    assert!(
        links(&root).is_empty(),
        "the MKV it did not deliver is not linked"
    );
    assert!(!job.exists(), "staging cleaned");
}

// A staged output under a name the hand-off does not list (another normalization, or a
// resume's name) is never treated as moved away: held, nothing moved or deleted.
#[test]
fn a_staged_output_under_another_name_holds_and_nothing_is_deleted() {
    let _g = guard();
    for (listed, staged) in [
        ("Night Harbor Caf\u{e9}.mkv", "Night Harbor Cafe\u{301}.mkv"),
        ("Night Harbor.mkv", "Night Harbor (1).mkv"),
    ] {
        let (_t, root, cfg, job) = night_harbor();
        std::fs::rename(job.join("Night Harbor.mkv"), job.join(staged)).unwrap();
        let state = job.join("state.json");
        let text = std::fs::read_to_string(&state).unwrap().replace(
            "\"filename\": \"Night Harbor.mkv\"",
            &format!("\"filename\": \"{listed}\""),
        );
        std::fs::write(&state, text).unwrap();
        let before = tree(&job);
        check_and_move(&cfg);
        assert!(take_error(&job).is_some(), "{staged}: held with a reason");
        assert_eq!(tree(&job), before, "{staged}: staging untouched");
        assert!(
            tree(&root.join("out")).is_empty(),
            "{staged}: nothing filed"
        );
    }
}

// Everything v1.8.3 was to file is already in the library: only bookkeeping is left, which
// is removed with the folder, and the rip reads as done. A media file left behind holds.
#[test]
fn a_rip_fully_filed_by_v1_8_3_finishes_and_keeps_every_media_file() {
    let _g = guard();
    let (_t, root, cfg, job) = night_harbor();
    std::fs::rename(job.join("Night Harbor.mkv"), root.join(BASE_MKV)).unwrap();
    std::fs::rename(job.join("Night Harbor.iso"), root.join(BASE_ISO)).unwrap();
    check_and_move(&cfg);
    no_error(&job);
    assert!(!job.exists(), "bookkeeping and the folder removed");
    assert_eq!(std::fs::read(root.join(BASE_MKV)).unwrap(), fx::old_mkv());
    assert_eq!(std::fs::metadata(root.join(BASE_ISO)).unwrap().len(), 16384);

    // keep_iso off: the staged ISO is not deliverable, and is never deleted here.
    let (_t, root, mut cfg, job) = night_harbor();
    cfg.keep_iso = false;
    std::fs::rename(job.join("Night Harbor.mkv"), root.join(BASE_MKV)).unwrap();
    let before = tree(&job);
    check_and_move(&cfg);
    let error = take_error(&job).expect("held");
    assert!(
        error.reason.contains("Night Harbor.iso"),
        "{}",
        error.reason
    );
    assert_eq!(tree(&job), before);
}

// A destination that cannot be examined holds the rip, naming it; no name is assumed.
#[cfg(unix)]
#[test]
fn an_unreadable_destination_holds_and_names_it() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        return; // root reads through the permission this test removes
    }
    let _g = guard();
    let (_t, root, cfg, job) = night_harbor();
    let folder = root.join("out/movies/Night Harbor (2017)");
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o000)).unwrap();
    check_and_move(&cfg);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = take_error(&job).expect("held");
    assert!(
        error.reason.contains("cannot examine the destination")
            && error.reason.contains("Night Harbor (2017).mkv"),
        "{}",
        error.reason
    );
    assert!(job.join("Night Harbor.mkv").exists() && job.join("Night Harbor.iso").exists());
    assert!(!job.join(".delivery.json").exists(), "no plan frozen");
    assert!(tree(&root.join("out")).is_empty());
    check_and_move(&cfg);
    no_error(&job);
    assert!(!job.exists() && root.join(BASE_MKV).exists() && root.join(BASE_ISO).exists());
}

// A title folder that is a link into staging is never a delivery target.
#[cfg(unix)]
#[test]
fn a_title_folder_linked_into_staging_is_refused() {
    let _g = guard();
    let (_t, root, cfg, job) = night_harbor();
    let folder = root.join("out/movies/Night Harbor (2017)");
    std::fs::remove_dir(&folder).unwrap();
    std::os::unix::fs::symlink(&job, &folder).unwrap();
    std::fs::copy(
        job.join("Night Harbor.mkv"),
        job.join("Night Harbor (2017).mkv"),
    )
    .unwrap();
    check_and_move(&cfg);
    assert!(take_error(&job).is_some(), "held with a reason");
    assert_eq!(
        std::fs::read(job.join("Night Harbor.mkv")).unwrap(),
        fx::old_mkv()
    );
    assert!(job.join("Night Harbor.iso").exists() && !root.join(BASE_ISO).exists());
}

// Run `name` alone in a child test process: it sets the process-wide SHUTDOWN.
fn in_child(name: &str) -> bool {
    const CHILD: &str = "FREEMKV_TEST_MOVER_UPGRADE_SHUTDOWN_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let status =
        crate::server::util::child_test(&format!("server::mover::upgrade_tests::{name}"), CHILD);
    assert!(
        status.success(),
        "{name} failed in its child process: {status}"
    );
    false
}

// A shutdown during a legacy rip's pass is an interruption: no Needs Action row, staged
// files kept, and the next start finishes it.
#[test]
fn a_shutdown_during_a_legacy_delivery_leaves_no_row() {
    if !in_child("a_shutdown_during_a_legacy_delivery_leaves_no_row") {
        return;
    }
    let _g = guard();
    let (_t, root, cfg, job) = night_harbor();
    std::fs::rename(job.join("Night Harbor.mkv"), root.join(BASE_MKV)).unwrap();
    crate::server::SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
    check_and_move(&cfg);
    crate::server::SHUTDOWN.store(false, std::sync::atomic::Ordering::SeqCst);
    assert!(take_error(&job).is_none(), "no Needs Action row");
    assert!(job.join("Night Harbor.iso").exists() && !root.join(BASE_ISO).exists());
    check_and_move(&cfg);
    no_error(&job);
    assert!(!job.exists() && root.join(BASE_ISO).exists());
}
