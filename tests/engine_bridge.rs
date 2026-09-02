//! Engine-bridge tests. The scan/rip tests need a real disc image, so they are
//! gated on FMKV_TEST_ISO / FMKV_TEST_MKV / FMKV_TEST_AACS_ISO pointing at one.
//!
//! They are `#[ignore]`, NOT early-returning, so `cargo test` reports them as
//! `ignored` rather than a falsely-passing green. Run them with a fixture:
//!
//! ```sh
//! FMKV_TEST_ISO=/path/to.iso cargo test --test engine_bridge -- --ignored
//! ```
//!
//! Each still keeps its `else` guard so running it WITHOUT the fixture fails
//! loudly rather than silently passing.

use freemkv::engine;

fn test_iso() -> Option<String> {
    let p = std::env::var("FMKV_TEST_ISO").ok()?;
    std::path::Path::new(&p).exists().then_some(p)
}

#[test]
fn scan_rejects_a_non_image_cleanly() {
    // Must be an error string, never a panic: the UI shows this verbatim.
    let err = engine::scan("/etc/hosts").unwrap_err();
    assert!(!err.is_empty());
    assert!(err.starts_with('E'), "errors carry a code: {err}");
}

#[test]
fn scan_rejects_a_missing_path_cleanly() {
    let err = engine::scan("/definitely/not/a/file.iso").unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn preflight_on_bad_source_is_an_error_not_a_panic() {
    assert!(engine::preflight("/definitely/not/a/file.iso", "/tmp", &[]).is_err());
}

#[test]
fn run_state_defaults_are_idle() {
    let s = engine::RunState::default();
    assert!(!s.finished.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!s.cancel.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(s.prog.lock().unwrap().bytes_done, 0);
    assert!(s.lines.lock().unwrap().is_empty());
}

#[test]
#[ignore = "needs a real disc fixture; run with --ignored"]
fn scan_maps_a_real_disc_into_rows() {
    let Some(iso) = test_iso() else {
        panic!("set FMKV_TEST_ISO to a real disc image to run this test");
    };
    let sc = engine::scan(&iso).expect("scan should succeed on a real image");
    assert!(!sc.label.is_empty(), "a disc must have a label");
    assert!(sc.title_count > 0, "a disc must expose titles");
    assert!(
        !sc.key_summary.is_empty(),
        "key status is data, not a log line"
    );

    // Row 0 is the disc root, and it must not be checkable — the reference
    // shows no checkbox there and neither do we.
    let root = &sc.rows[0];
    assert_eq!(root.depth, 0);
    assert!(!root.checkable, "the disc root must not be checkable");
    assert!(root.type_s.ends_with(" disc"), "got {}", root.type_s);

    // Video rows carry no checkbox (video is implicit); audio/subtitle do.
    for r in sc.rows.iter().filter(|r| r.depth == 2) {
        if r.type_s == "Video" {
            assert!(!r.checkable, "video rows must not be checkable");
        } else {
            assert!(r.checkable, "{} rows must be checkable", r.type_s);
        }
    }

    // Every row must carry Info text, or the detail pane shows blanks.
    for r in &sc.rows {
        assert!(!r.info.is_empty(), "row {} has no info", r.desc);
    }
}

#[test]
fn every_title_row_has_a_duration_and_size() {
    let Some(iso) = test_iso() else { return };
    let sc = engine::scan(&iso).unwrap();
    for r in sc.rows.iter().filter(|r| r.depth == 1) {
        assert!(r.desc.contains("chapter(s)"), "got {}", r.desc);
        assert!(r.desc.contains(" GB"), "got {}", r.desc);
        assert!(r.desc.contains(':'), "duration missing in {}", r.desc);
    }
}

#[test]
fn preflight_accepts_a_real_disc() {
    let Some(iso) = test_iso() else { return };
    let blocked = engine::preflight(&iso, "/tmp", &[]).unwrap();
    assert!(blocked.is_empty(), "unexpectedly blocked: {blocked:?}");
}

#[test]
fn preflight_blocks_an_out_of_range_title() {
    let Some(iso) = test_iso() else { return };
    let blocked = engine::preflight(&iso, "/tmp", &[9999]).unwrap();
    assert!(!blocked.is_empty(), "a bogus title index must be blocked");
    // Reasons are stable keys to localize, never English prose.
    assert!(
        blocked.iter().all(|r| !r.contains(' ')),
        "reasons must be stable keys: {blocked:?}"
    );
}

// ── stream sources (mkv / m2ts / mp4) ────────────────────────────────────
//
// Set FMKV_TEST_MKV to a real container to run these.

fn test_mkv() -> Option<String> {
    let p = std::env::var("FMKV_TEST_MKV").ok()?;
    std::path::Path::new(&p).exists().then_some(p)
}

#[test]
fn stream_scan_rejects_a_bad_path_cleanly() {
    assert!(engine::scan_stream("/definitely/not/a/file.mkv").is_err());
}

#[test]
#[ignore = "needs a real MKV fixture; run with --ignored"]
fn stream_scan_exposes_real_tracks() {
    let Some(m) = test_mkv() else {
        panic!("set FMKV_TEST_MKV to a real MKV to run this test");
    };
    let sc = engine::scan_stream(&m).expect("a container should open");
    assert_eq!(sc.title_count, 1, "a container is a single title");
    assert!(
        sc.rows.len() >= 3,
        "file row + title row + at least one track"
    );

    // Root is the file, not a disc, and is not checkable.
    assert_eq!(sc.rows[0].depth, 0);
    assert!(!sc.rows[0].checkable);
    assert_eq!(sc.rows[0].type_s, "File");

    // Tracks must be real, not a placeholder message.
    let tracks: Vec<_> = sc.rows.iter().filter(|r| r.depth == 2).collect();
    assert!(!tracks.is_empty(), "a container must expose its tracks");
    assert!(
        tracks.iter().any(|r| r.type_s == "Video"),
        "expected at least one video track"
    );
    for t in &tracks {
        assert!(!t.desc.is_empty(), "track has no description");
        assert!(!t.info.is_empty(), "track has no info");
        if t.type_s == "Video" {
            assert!(!t.checkable, "video is implicit");
        }
    }
}

#[test]
fn stream_scan_reports_a_duration() {
    let Some(m) = test_mkv() else { return };
    let sc = engine::scan_stream(&m).unwrap();
    let title = sc.rows.iter().find(|r| r.depth == 1).unwrap();
    assert!(title.desc.contains("track(s)"), "got {}", title.desc);
    assert!(
        title.desc.contains(':'),
        "duration missing in {}",
        title.desc
    );
}

#[test]
fn every_title_is_distinguishable_and_indices_are_canonical() {
    let Some(iso) = test_iso() else { return };
    let sc = engine::scan(&iso).unwrap();

    // Like the CLI, every title is listed — discs legitimately carry duplicate
    // playlists with identical duration/size. What must hold is that no two
    // rows LOOK the same: the 1-based number and playlist name disambiguate.
    let titles: Vec<_> = sc.rows.iter().filter(|r| r.depth == 1).collect();
    let mut descs: Vec<&str> = titles.iter().map(|r| r.desc.as_str()).collect();
    descs.sort_unstable();
    let before = descs.len();
    descs.dedup();
    assert_eq!(
        descs.len(),
        before,
        "two visible titles are indistinguishable to the user"
    );

    // Numbering must match `freemkv info` / `-t N`, which are 1-based.
    for (n, r) in titles.iter().enumerate() {
        assert!(
            r.desc.starts_with(&format!("{}.", n + 1)),
            "title {} should be numbered {}: {}",
            n,
            n + 1,
            r.desc
        );
        assert_eq!(r.title, n, "canonical index must match list position");
    }

    // Title rows carry canonical disc indices, which must be unique and
    // ascending — the rip targets these, not tree positions.
    let idx: Vec<usize> = titles.iter().map(|r| r.title).collect();
    let mut sorted = idx.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(idx.len(), sorted.len(), "duplicate canonical indices");
    assert!(idx.windows(2).all(|w| w[0] < w[1]), "indices out of order");
}

#[test]
fn decrypted_folder_extraction_guards_a_populated_subdir() {
    // A decrypted-folder rip extracts into a per-disc SUBDIR (never dest_dir
    // itself, which once collided with ~/Movies). --force gates that subdir
    // only when populated; ordinary file output is never gated.
    let base = std::env::temp_dir().join(format!("fmkv_force_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);

    // The target is a per-disc subdir, not the destination itself.
    assert_eq!(
        engine::extract_target("/out", "MOVIE_LABEL"),
        std::path::Path::new("/out/MOVIE_LABEL"),
    );

    // A fresh (missing) subdir is always writable.
    let fresh = base.join("BRAND_NEW");
    assert!(
        engine::folder_writable(&fresh, false).is_ok(),
        "a fresh subdir must accept an extraction"
    );

    // A populated subdir needs force.
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::write(fresh.join("existing.txt"), b"x").unwrap();
    assert!(
        engine::folder_writable(&fresh, false).is_err(),
        "unpacking into a populated subdir needs --force"
    );
    assert!(
        engine::folder_writable(&fresh, true).is_ok(),
        "force overrides it"
    );
    let _ = std::fs::remove_dir_all(&base);
}

// The key strip must never claim a key the disc doesn't have: a VID-only
// scan stamps a PLACEHOLDER key origin with `unit_keys` empty, which can
// make `resolve_keys` falsely report resolved. Gate on real key material.
#[test]
#[ignore = "needs a real disc fixture; run with --ignored"]
fn unkeyed_aacs_disc_is_not_reported_as_resolved() {
    let Ok(iso) = std::env::var("FMKV_TEST_AACS_ISO") else {
        panic!("set FMKV_TEST_AACS_ISO to an AACS disc image to run this test");
    };
    // No key sources at all: nothing can possibly resolve.
    let sc = freemkv::engine::scan(&iso).expect("scan");
    assert!(
        !sc.key_summary.contains("online") && !sc.key_summary.contains("keydb"),
        "with zero key sources the summary must not name one, got {:?}",
        sc.key_summary
    );
    assert!(
        sc.key_summary.contains("locked"),
        "an unkeyed encrypted disc must read as locked, got {:?}",
        sc.key_summary
    );
}

// Start must not be enabled for a disc we can't decrypt: `preflight`'s
// decrypt gate is satisfied by `disc.aacs.is_some()`, which a VID-only
// scan sets with no key material. Resolve keys first and gate on that.
#[test]
#[ignore = "needs a real disc fixture; run with --ignored"]
fn an_undecryptable_disc_blocks_the_run() {
    let Ok(iso) = std::env::var("FMKV_TEST_AACS_ISO") else {
        panic!("set FMKV_TEST_AACS_ISO to an AACS disc image to run this test");
    };
    // No key sources configured: nothing can resolve.
    let blocked = freemkv::engine::preflight(&iso, "/tmp", &[]).expect("preflight ran");
    assert!(
        !blocked.is_empty(),
        "an encrypted disc with no key source must block, got Ready"
    );
    assert!(
        blocked.iter().any(|r| r.contains("key")),
        "the reason must name the missing key, got {blocked:?}"
    );
}

/// With the user's real key sources the same disc becomes runnable — proving
/// the gate above tracks key state rather than just refusing every AACS disc.
#[test]
#[ignore = "needs a real disc fixture; run with --ignored"]
fn the_same_disc_runs_once_a_key_source_is_configured() {
    let Ok(iso) = std::env::var("FMKV_TEST_AACS_ISO") else {
        panic!("set FMKV_TEST_AACS_ISO to an AACS disc image to run this test");
    };
    let keys = freemkv::engine::KeyConfig::from_settings(&freemkv::settings::Settings::load());
    // Was an early `return`, so on an unconfigured machine this reported PASS
    // while proving nothing — and on a configured one it tested something
    // different. If you are running it, the fixture must be complete.
    assert!(
        !(keys.keydb_path.trim().is_empty() && keys.keyserver_url.trim().is_empty()),
        "configure a keydb path or keyserver URL in settings to run this test"
    );
    let blocked =
        freemkv::engine::preflight_with_keys(&iso, "/tmp", &[], &keys).expect("preflight ran");
    assert!(blocked.is_empty(), "expected Ready, blocked by {blocked:?}");
}
