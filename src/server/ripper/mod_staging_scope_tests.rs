use super::{resume_remaining_iso_bytes, scope_bytes, staged_titles, sweep_scope};
use crate::ku_fixture::{K1, bd_image, holding};
use crate::server::config::Config;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

fn mkv_rip(keep_iso: bool) -> Config {
    Config {
        output_format: "mkv".into(),
        keep_iso,
        ..Config::default()
    }
}

// Whether `scope` holds every sector of `title`'s extents.
fn covers(scope: &[(u32, u32)], title: &libfreemkv::DiscTitle) -> bool {
    title.extents.iter().all(|e| {
        scope
            .iter()
            .any(|&(l, n)| l <= e.start_lba && e.start_lba + e.sector_count <= l + n)
    })
}

// keep_iso=false stages only the disc's structure and the muxed titles' extents; a kept ISO
// or an ISO deliverable is the whole disc.
#[test]
fn only_a_discarded_iso_is_scoped_to_its_titles() {
    let fx = bd_image();
    let disc = fx.scan();
    let mut reader = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
    assert_eq!(
        sweep_scope(&mkv_rip(true), &disc, &mut reader, &[0]).unwrap(),
        None
    );
    let iso_out = Config {
        output_format: "iso".into(),
        ..mkv_rip(false)
    };
    assert_eq!(
        sweep_scope(&iso_out, &disc, &mut reader, &[0]).unwrap(),
        None
    );

    let scope = sweep_scope(&mkv_rip(false), &disc, &mut reader, &[0])
        .unwrap()
        .expect("an MKV rip that discards its ISO is scoped");
    assert!(covers(&scope, &disc.titles[0]), "{scope:?}");
    assert!(scope_bytes(&scope) <= disc.capacity_bytes);
    // The disc's structure alone leaves out the stream a title not muxed plays.
    let structure = sweep_scope(&mkv_rip(false), &disc, &mut reader, &[])
        .unwrap()
        .unwrap();
    assert!(!covers(&structure, &disc.titles[0]), "{structure:?}");
    assert!(scope_bytes(&structure) < scope_bytes(&scope));
}

// The staged titles are the main feature, the TV plan's episodes and title 0, once each.
#[test]
fn the_staged_titles_are_every_title_the_mux_reads() {
    let title = |playlist: &str, secs: f64| libfreemkv::DiscTitle {
        selection_evidence: Default::default(),
        playlist: playlist.into(),
        duration_secs: secs,
        ..libfreemkv::DiscTitle::empty()
    };
    let titles = vec![title("00000.mpls", 6000.0), title("00001.mpls", 60.0)];
    let movie = mkv_rip(false);
    assert_eq!(staged_titles(&titles, &movie, "movie", "Film", 1), [0, 1]);
    assert_eq!(staged_titles(&titles, &movie, "movie", "Film", 0), [0]);
    // An index past the disc's titles is never staged.
    assert_eq!(staged_titles(&titles, &movie, "movie", "Film", 9), [0]);
}

/// A scoped staging image works end to end the way the server uses it: the sweep reads only
/// its scope and the mapfile records it, the resume checks read it as fully swept, and the
/// main title opens and muxes from it. A title outside the scope is refused (E6022), never
/// muxed as zeros.
#[test]
fn a_scoped_staging_image_resumes_and_muxes() {
    let fx = bd_image();
    let disc = fx.scan();
    let dir = tempfile::tempdir().unwrap();
    let iso = dir.path().join("disc.iso");
    let mut reader = libfreemkv::test_util::MemSource::new(fx.img.image.clone());
    let scope = sweep_scope(&mkv_rip(false), &disc, &mut reader, &[0])
        .unwrap()
        .unwrap();
    let plan = freemkv_engine::Plan {
        source: "disc://fixture".into(),
        dest: format!("iso://{}", iso.display()),
        titles: freemkv_engine::Selection::Titles(vec![0]),
        raw: true,
        multipass: true,
        ..freemkv_engine::Plan::default()
    };
    let with = freemkv_engine::RunWith {
        held: Some(freemkv_engine::Held::Disc {
            disc: &disc,
            reader: &mut reader,
        }),
        passes: Some(freemkv_engine::MultipassOpts {
            max_passes: 1,
            abort_on_lost_secs: 0,
            is_iso_output: false,
        }),
        scope: Some(&scope),
        locked: true,
        ..freemkv_engine::RunWith::default()
    };
    let report = freemkv_engine::run_with(&plan, with, &freemkv_engine::NoopSink).unwrap();
    assert!(matches!(report, freemkv_engine::Report::Image { .. }));

    // The mapfile records the scope; the resume and completion checks see it fully swept.
    let mapfile = freemkv_engine::mapfile_path_for(&iso);
    let map = freemkv_engine::Mapfile::load(&mapfile).unwrap();
    assert!(map.scope().is_some(), "a scoped image is marked as one");
    let stats = map.stats();
    assert_eq!(stats.bytes_pending, 0, "the scope is fully swept");
    assert!(std::fs::metadata(&iso).unwrap().len() >= stats.bytes_total);
    assert_eq!(
        resume_remaining_iso_bytes(&mapfile, &iso, disc.capacity_bytes),
        Some(0)
    );

    // The main title opens and muxes from the scoped image.
    let calls = Arc::new(AtomicUsize::new(0));
    let keys = freemkv_engine::KeyInput::Resolve(holding(&calls, K1));
    let image =
        crate::server::keysource::open_staged(&iso, fx.scan(), &[0], keys, None, None).unwrap();
    let dest = format!("mkv://{}", dir.path().join("out.mkv").display());
    let out = freemkv_engine::mux_image_titles(
        &image,
        &freemkv_engine::MuxPlan::new(vec![0]),
        &|_| dest.clone(),
        &freemkv_engine::NoopSink,
    );
    assert!(
        matches!(out, freemkv_engine::RipOutcome::Ok { titles_written: 1 }),
        "{out:?}"
    );

    // An image staged without the title's stream refuses to open it for a mux.
    let structure = sweep_scope(&mkv_rip(false), &disc, &mut reader, &[])
        .unwrap()
        .unwrap();
    let mut map = freemkv_engine::Mapfile::load(&mapfile).unwrap();
    map.set_scope(
        structure
            .iter()
            .map(|&(l, n)| (l as u64 * 2048, n as u64 * 2048))
            .collect(),
    );
    map.flush().unwrap();
    let keys = freemkv_engine::KeyInput::Resolve(holding(&calls, K1));
    let Err(e) = crate::server::keysource::open_staged(&iso, fx.scan(), &[0], keys, None, None)
    else {
        panic!("a title outside the staged scope must not open");
    };
    assert_eq!(e.code(), libfreemkv::error::E_IMAGE_SCOPED, "{e}");
}

// Wiring guard: rip_disc sweeps the scope it computed, sizes the progress by it, and hands
// the same scale to the mux phase.
#[test]
fn rip_disc_sweeps_its_staging_scope() {
    let src = crate::server::util::source_lf(include_str!("mod.rs"));
    let at = src
        .find("sweep_scope(&cfg_read, &disc, &mut session.drive, &staged)")
        .expect("rip_disc computes the staging scope from the held drive");
    let run = src[at..]
        .find("scope: staging_scope.as_deref(),")
        .expect("the sweep runs over it");
    assert!(
        src[at..at + run].contains("bytes_sweep,"),
        "the pass context is sized by it"
    );
    assert!(
        src.contains("bytes_total_disc: bytes_swept_total,"),
        "the mux continues on it"
    );
}
