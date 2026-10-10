// A new client past the cap closes the oldest stream instead of being refused.

#[test]
fn webhook_key_is_redacted_preserved_and_bound_to_its_destination() {
    let hook = WebhookEntry::parse(0, &serde_json::json!({"url":"https://jf.example/Library/Refresh","headers":{"Authorization":"new-secret-key"}})).unwrap();
    let roundtrip: WebhookEntry =
        serde_json::from_str(&serde_json::to_string(&hook).unwrap()).unwrap();
    assert_eq!(roundtrip, hook);
    let cfg = Config {
        webhook_urls: vec![hook.clone()],
        ..Config::default()
    };
    let redacted = settings_json_redacted(&cfg);
    assert!(!redacted.contains("new-secret-key"));
    assert!(!format!("{hook:?}").contains("new-secret-key"));
    let v: serde_json::Value = serde_json::from_str(&redacted).unwrap();
    let masked = WebhookEntry::parse(0, &v["webhook_urls"][0]).unwrap();
    assert_eq!(
        resolve_webhook_entries(std::slice::from_ref(&masked), &cfg.webhook_urls).unwrap()[0],
        hook
    );
    let mut moved = masked.clone();
    moved.url = "https://other.example/Library/Refresh".into();
    assert!(resolve_webhook_entries(&[moved], &cfg.webhook_urls).is_err());
    let mut renamed = masked.clone();
    renamed.headers = [("X-Other-Key".into(), SECRET_SENTINEL.into())].into();
    assert!(resolve_webhook_entries(&[renamed], &cfg.webhook_urls).is_err());
    let migrated = WebhookEntry::parse(
        0,
        &serde_json::json!({"url":"https://jf.example","jellyfin_api_key":"old-key"}),
    )
    .unwrap();
    assert_eq!(
        migrated.headers["Authorization"],
        "MediaBrowser Token=\"old-key\""
    );
    for headers in [
        serde_json::json!({"Bad Header":"value"}),
        serde_json::json!({"Host":"other.example"}),
        serde_json::json!({"Authorization":"a", "authorization":"b"}),
    ] {
        assert!(
            WebhookEntry::parse(
                0,
                &serde_json::json!({"url":"https://jf.example", "headers":headers})
            )
            .is_err()
        );
    }
    let mut cleared = masked;
    cleared.headers.clear();
    assert!(
        resolve_webhook_entries(&[cleared], &cfg.webhook_urls).unwrap()[0]
            .headers
            .is_empty()
    );
    assert!(
            WebhookEntry::parse(
                0,
                &serde_json::json!({"url":"https://jf.example","headers":{"Authorization":"bad\r\nkey"}})
            )
            .is_err()
        );
}
#[test]
fn the_oldest_event_stream_gives_way() {
    let admitted: Vec<_> = (0..super::MAX_SSE_CLIENTS + 2)
        .map(|_| super::sse_admit())
        .collect();
    let stopped = admitted
        .iter()
        .filter(|(_, f)| f.load(super::Ordering::SeqCst))
        .count();
    assert!(stopped >= 2, "the two oldest are told to stop");
    assert!(!admitted.last().unwrap().1.load(super::Ordering::SeqCst));
    assert!(super::SSE_STREAMS.lock().unwrap().len() <= super::MAX_SSE_CLIENTS);
    for (id, _) in admitted {
        super::sse_leave(id);
    }
}

// An embedded UI asset as text.
fn asset(name: &str) -> &'static str {
    let (_, _, body) = super::ASSETS
        .iter()
        .find(|(n, _, _)| *n == name)
        .unwrap_or_else(|| panic!("no asset {name}"));
    std::str::from_utf8(body).expect("text asset")
}

// Review fixes the browser tests exercise, pinned so they cannot regress quietly.
#[test]
fn the_ui_keeps_its_review_fixes() {
    let ripper = asset("ripper.js");
    assert!(
        ripper.contains("?since="),
        "drive logs follow by sequence, not line count"
    );
    assert!(
        !ripper.contains("view.addEventListener"),
        "handlers bind to the page's own root"
    );
    let app = asset("app.js");
    assert!(
        app.contains("token !== renderToken"),
        "stale renders are dropped"
    );
    let ui = asset("ui.js");
    assert!(ui.contains("MAX_LINES"), "the terminal caps its scrollback");
    let menu = &ui[ui.find("export function menu").unwrap()..ui.find("// ── Modals").unwrap()];
    assert!(
        !menu.contains("document.addEventListener"),
        "menus share one document listener"
    );
    assert!(asset("settings.js").contains("new-password"));
    assert!(asset("settings.js").contains("r.reachable"));
    // Library is the MKVs and never remuxes: its one link to the Remux page is plain
    // navigation, and the header carries no Remux tab.
    let library = asset("library.js");
    assert!(library.contains("id=\"to-remux\" href=\"/remux\""));
    assert!(!super::INDEX_HTML.contains("href=\"/remux\""));
    assert!(library.contains("remux: false"));
    assert!(
        !library.contains("row-more"),
        "the row itself opens the details"
    );
    // Never a browser-native dialog: every confirm and message is the app's modal.
    for (name, _, body) in super::ASSETS {
        let text = std::str::from_utf8(body).unwrap_or("");
        for native in ["alert(", "confirm(", "prompt(", "window.confirm"] {
            let hits = text
                .match_indices(native)
                .filter(|(i, _)| {
                    !text[..*i].ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.')
                })
                .count();
            assert_eq!(hits, 0, "{name} calls the browser's {native}");
        }
    }
    // Both lists filter with the one chip component.
    assert!(library.contains("chipFilter(") && asset("remux.js").contains("chipFilter("));
    assert!(library.contains("download=1") && asset("remux.js").contains("download=1"));
    assert!(
        asset("remux.js").contains("TV · ") && asset("remux.js").contains("o.state === 'done'"),
        "TV multi-output progress must be visible without changing movie rows"
    );
    assert!(!super::INDEX_HTML.contains("jobchip"));
    assert!(
        !asset("system.js").contains("id=\"keys\""),
        "keys live in Settings"
    );
}

// Every module the shell imports is embedded and served.
#[test]
fn every_imported_module_is_embedded() {
    for (name, ctype, body) in super::ASSETS {
        if !name.ends_with(".js") {
            continue;
        }
        assert!(ctype.starts_with("text/javascript"));
        let text = std::str::from_utf8(body).unwrap();
        for part in text
            .split("from './")
            .skip(1)
            .chain(text.split("import('./").skip(1))
        {
            let dep = &part[..part.find('\'').unwrap()];
            assert!(
                super::ASSETS.iter().any(|(n, _, _)| *n == dep),
                "{name} imports {dep}, which is not embedded"
            );
        }
    }
    assert!(super::INDEX_HTML.contains("/assets/app.js"));
}

use super::*;

// handle_accept_loss refuses (409) while the mux worker owns the
// dir (.muxing), else a lock-free state.json write could clobber the
// worker's terminal quarantine and silently drop the operator's Accept.
#[test]
fn accept_loss_entry_verdict_gates_on_muxing() {
    assert_eq!(
        accept_loss_entry_verdict(false, false),
        AcceptLossEntry::NoStagingDir,
        "no staging dir on disk → 404"
    );
    assert_eq!(
        accept_loss_entry_verdict(true, true),
        AcceptLossEntry::MuxInProgress,
        "a dir the worker is actively muxing must be refused (409), not reopened"
    );
    assert_eq!(
        accept_loss_entry_verdict(true, false),
        AcceptLossEntry::Proceed,
        "a present, unowned dir proceeds to arm the override"
    );
}

// H6: only a real NotFound means "gone"; EACCES/ESTALE must not read as a
// missing staging dir (the muxer's definitely_absent rule).
#[cfg(unix)]
#[test]
fn accept_loss_entry_unreadable_dir_is_not_gone() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().unwrap();
    assert_eq!(
        accept_loss_entry_for(&tmp.path().join("missing")),
        AcceptLossEntry::NoStagingDir
    );
    let dangling = tmp.path().join("dangling");
    std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dangling).unwrap();
    assert_eq!(
        accept_loss_entry_for(&dangling),
        AcceptLossEntry::NoStagingDir,
        "a dangling symlink is gone (404), not a present dir"
    );
    let parent = tmp.path().join("locked");
    let dir = parent.join("Disc");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o000)).unwrap();
    let bypass = std::fs::metadata(&dir).is_ok();
    let verdict = accept_loss_entry_for(&dir);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    if bypass {
        eprintln!("skipped: running with permission bypass (root)");
        return;
    }
    assert_eq!(
        verdict,
        AcceptLossEntry::StagingUnreadable,
        "EACCES on the staging dir must not be treated as 'no staging dir'"
    );
    // An unreadable state.json must not read as "not muxing" either.
    let state = dir.join(crate::server::ripper::staging::STATE_FILE);
    std::fs::write(&state, b"{}").unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o000)).unwrap();
    let verdict = accept_loss_entry_for(&dir);
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        verdict,
        AcceptLossEntry::StagingUnreadable,
        "an unreadable state.json must fail closed, not proceed as 'not muxing'"
    );
}

// Regression (bug #3): the Mux and Move queues must be mutually
// exclusive — a disc can never appear in both. Walk a staging dir
// through the post-mux marker sequence and assert that at each step.
#[test]
fn build_queue_views_mutually_exclusive() {
    use std::fs;
    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();
    let disc = tmp.path().join("Border_Town");
    fs::create_dir_all(&disc).unwrap();

    let both_contain = |mux: &[String], mv: &[String]| -> bool {
        mux.iter().any(|m| {
            let name = m.replace(" (queued)", "").replace(" (malformed)", "");
            mv.iter().any(|v| v.replace(" (moving)", "") == name)
        })
    };

    // Step 1: fresh hand-off — `.ripped` only. In the Mux queue, not Move.
    crate::server::muxer::write_marker(
        &disc,
        &crate::server::muxer::RippedMarker {
            schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
            iso_path: "/x/Border_Town/Border_Town.iso".into(),
            mapfile_path: "/x/Border_Town/Border_Town.iso.mapfile".into(),
            display_name: "Border Town".into(),
            disc_format: "uhd".into(),
            mkv_filename: "Border_Town.mkv".into(),
            tmdb_title: "Border Town".into(),
            tmdb_year: 2024,
            tmdb_poster: String::new(),
            tmdb_overview: String::new(),
            tmdb_media_type: "movie".into(),
            max_retries: 5,
            abort_on_lost_secs: 0,
            rip_elapsed_secs: 0.0,
            rip_errors: 0,
            rip_lost_video_secs: 0.0,
            rip_last_sector: 0,
            origin_device: "sg0".into(),
            sweep_errors: 0,
            sweep_total_lost_ms: 0.0,
            sweep_main_lost_ms: 0.0,
            sweep_num_bad_ranges: 0,
            sweep_largest_gap_ms: 0.0,
            title_confident: true,
        },
    )
    .unwrap();
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert_eq!(mux.len(), 1, "fresh .ripped must be in the Mux queue");
    assert!(mv.is_empty(), "not yet in the Move queue");
    assert!(!both_contain(&mux, &mv));

    // Step 2: mux in flight — `.muxing` added. Out of the Mux queue
    // (shown as the live `_mux` device), still not in Move.
    crate::server::ripper::staging::write_muxing_marker(&disc);
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(
        mux.is_empty(),
        "an actively-muxing dir leaves the queued list"
    );
    assert!(mv.is_empty());
    assert!(!both_contain(&mux, &mv));
    crate::server::ripper::staging::clear_muxing_marker(&disc);

    // Step 3: mux done — hand-off to `state: Done` (mover hand-off),
    // `.completed` not yet written. THIS is the double-listing bug
    // window: it must be in the Move queue ONLY.
    crate::server::ripper::staging::mark_handoff(&disc, true, |_s| {}).unwrap();
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(
        mux.is_empty(),
        "a dir in the Move queue (.done) must not also be (queued) in the Mux queue, got {mux:?}"
    );
    assert_eq!(mv.len(), 1, "must be in the Move queue");
    assert!(
        !both_contain(&mux, &mv),
        "BUG #3: a disc must never appear in both the mux and move queues"
    );

    // Step 4: terminal `.completed` lands — still Move-only, never both.
    crate::server::ripper::staging::write_completed_marker(&disc);
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(mux.is_empty());
    assert_eq!(mv.len(), 1);
    assert!(!both_contain(&mux, &mv));
}

// Regression: a moving dir keeps its .done marker, so build_queue_views
// must exclude ACTIVE_MOVE_DIR by exact basename or it double-renders
// (old title-based de-dup broke on punctuation like `:`).
#[test]
fn build_queue_views_excludes_the_actively_moving_dir() {
    use std::fs;
    // Serialize against every test that touches the global move statics.
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();
    // Two pending moves. On disk the title's colon has been sanitized away
    // (`X-Men: Apocalypse` → `X-Men_Apocalypse`), which is exactly what
    // used to defeat the client-side title match.
    let active = tmp.path().join("X-Men_Apocalypse");
    let other = tmp.path().join("Interstellar");
    fs::create_dir_all(&active).unwrap();
    fs::create_dir_all(&other).unwrap();
    fs::write(active.join(".done"), b"{}").unwrap();
    fs::write(other.join(".done"), b"{}").unwrap();

    // Nothing moving yet: both dirs are queued.
    *crate::server::mover::ACTIVE_MOVE_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    let (_, mv, _, _) = build_queue_views(&staging);
    assert_eq!(mv.len(), 2, "with nothing moving, both .done dirs queue");

    // Mark X-Men as the actively-moving dir (by its on-disk basename).
    *crate::server::mover::ACTIVE_MOVE_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some("X-Men_Apocalypse".to_string());
    let (_, mv, _, full) = build_queue_views(&staging);
    assert_eq!(
        mv,
        vec!["Interstellar (moving)".to_string()],
        "the actively-moving dir must be excluded from the queue (shown as bars instead)"
    );
    assert_eq!(
        full, 1,
        "the uncapped count must also exclude the active dir"
    );

    // Clear so no other test observes a stale active dir.
    *crate::server::mover::ACTIVE_MOVE_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

// COMPREHENSIVE rip→mux→move→done state-machine coverage: the three
// views (tile status, Mux queue, Move queue) must stay consistent
// across every marker transition with multiple discs in staging.

/// Build a schema-valid `.ripped` marker for `display_name` whose
/// `origin_device` is `origin`. Keeps the lifecycle tests terse.
fn ripped_marker_for(display_name: &str, origin: &str) -> crate::server::muxer::RippedMarker {
    let safe = display_name.replace(' ', "_");
    crate::server::muxer::RippedMarker {
        schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
        iso_path: format!("/x/{safe}/{safe}.iso"),
        mapfile_path: format!("/x/{safe}/{safe}.iso.mapfile"),
        display_name: display_name.into(),
        disc_format: "uhd".into(),
        mkv_filename: format!("{safe}.mkv"),
        tmdb_title: display_name.into(),
        tmdb_year: 2024,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        tmdb_media_type: "movie".into(),
        max_retries: 5,
        abort_on_lost_secs: 0,
        rip_elapsed_secs: 0.0,
        rip_errors: 0,
        rip_lost_video_secs: 0.0,
        rip_last_sector: 0,
        origin_device: origin.into(),
        sweep_errors: 0,
        sweep_total_lost_ms: 0.0,
        sweep_main_lost_ms: 0.0,
        sweep_num_bad_ranges: 0,
        sweep_largest_gap_ms: 0.0,
        title_confident: true,
    }
}

/// Does `name` appear in BOTH queues at once? (Strips the trailing
/// status suffixes so `"X (queued)"` and `"X (moving)"` compare equal.)
fn in_both_queues(mux: &[String], mv: &[String]) -> bool {
    let strip = |s: &str| -> String {
        s.replace(" (queued)", "")
            .replace(" (malformed)", "")
            .replace(" (moving)", "")
    };
    mux.iter().any(|m| mv.iter().any(|v| strip(m) == strip(v)))
}

// FULL marker lifecycle with device-status folded in: at each step assert
// which queue(s) the disc is in and the tile status; the disc is never
// in two queues. Covers .ripped -> .muxing -> .done -> .completed.
#[test]
fn full_lifecycle_queue_and_status_consistent() {
    use std::fs;
    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();
    let disc = tmp.path().join("Mercy");
    fs::create_dir_all(&disc).unwrap();
    let device = "sg_lifecycle_dev";

    // --- Stage 0: sweep in progress. `.sweeping` marker, tile=ripping.
    crate::server::ripper::staging::write_sweeping_marker(&disc);
    crate::server::ripper::update_state(
        device,
        crate::server::ripper::RipState {
            device: device.to_string(),
            status: "ripping".to_string(),
            disc_name: "Mercy".to_string(),
            ..Default::default()
        },
    );
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(
        mux.is_empty() && mv.is_empty(),
        "during sweep: in neither queue"
    );
    assert_eq!(device_status(device), Some("ripping".into()));

    // --- Stage 1: `.ripped` hand-off. The read is DONE: tile=done(100%),
    // disc enters the Mux queue ONLY. (`write_marker` also clears
    // `.sweeping`.)
    crate::server::muxer::write_marker(&disc, &ripped_marker_for("Mercy", device)).unwrap();
    crate::server::ripper::update_state(
        device,
        crate::server::ripper::RipState {
            device: device.to_string(),
            status: "done".to_string(),
            progress_pct: 100,
            disc_name: "Mercy".to_string(),
            output_file: "Mercy.mkv".to_string(),
            ..Default::default()
        },
    );
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert_eq!(mux.len(), 1, ".ripped → Mux queue");
    assert!(mv.is_empty(), "not in Move queue yet");
    assert!(!in_both_queues(&mux, &mv));
    assert_eq!(
        device_status(device),
        Some("done".into()),
        "tile is 'done' the instant the read finishes, even though the mux is pending"
    );

    // --- Stage 2: mux in flight. `.muxing` lock; disc leaves the static
    // Mux queue (it's the live `_mux` device now); tile stays done.
    crate::server::ripper::staging::write_muxing_marker(&disc);
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(
        mux.is_empty(),
        "actively-muxing dir leaves the (queued) list"
    );
    assert!(mv.is_empty());
    assert!(!in_both_queues(&mux, &mv));
    assert_eq!(device_status(device), Some("done".into()));
    crate::server::ripper::staging::clear_muxing_marker(&disc);

    // --- Stage 3: mux success. Hand-off to `state: Done` written BEFORE
    // `.completed`. Disc moves to the Move queue ONLY — the
    // double-listing bug window.
    crate::server::ripper::staging::mark_handoff(&disc, true, |_s| {}).unwrap();
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(
        mux.is_empty(),
        "a .done dir must NOT still be (queued) in the Mux queue"
    );
    assert_eq!(mv.len(), 1, ".done → Move queue");
    assert!(!in_both_queues(&mux, &mv), "BUG #3: never in both queues");
    assert_eq!(device_status(device), Some("done".into()));

    // --- Stage 4: `.completed` lands (terminal). Still Move-only.
    crate::server::ripper::staging::write_completed_marker(&disc);
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(mux.is_empty());
    assert_eq!(
        mv.len(),
        1,
        "still in the Move queue until the mover relocates it"
    );
    assert!(!in_both_queues(&mux, &mv));

    crate::server::ripper::STATE.lock().unwrap().remove(device);
}

// LOW-CONFIDENCE lifecycle: the mux writes .review (not .done) for an
// operator hold. The disc must leave the Mux queue and never appear in
// both the Mux "(queued)" and Move "(moving)" lists at once.
#[test]
fn review_hold_leaves_mux_queue_no_double_listing() {
    use std::fs;
    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();
    let disc = tmp.path().join("Held_Title");
    fs::create_dir_all(&disc).unwrap();

    crate::server::muxer::write_marker(&disc, &ripped_marker_for("Held Title", "sg0")).unwrap();
    let (mux, _, _, _) = build_queue_views(&staging);
    assert_eq!(mux.len(), 1, "fresh .ripped is queued for mux");

    // Low-confidence mux success: hand-off to `state: Review` instead of
    // `state: Done`, then `.completed`.
    crate::server::ripper::staging::mark_handoff(&disc, false, |_s| {}).unwrap();
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(mux.is_empty(), "a .review dir must leave the Mux queue");
    assert!(
        !in_both_queues(&mux, &mv),
        "never in both queues on the review path"
    );

    crate::server::ripper::staging::write_completed_marker(&disc);
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(mux.is_empty());
    assert!(!in_both_queues(&mux, &mv));
}

/// FAILURE path: a terminal mux failure writes `.failed` (no `.done`/
/// `.completed`). The disc must leave BOTH queues, and the device tile
/// reflects "error".
#[test]
fn abort_failed_leaves_both_queues_and_marks_error() {
    use std::fs;
    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();
    let disc = tmp.path().join("Lossy_Disc");
    fs::create_dir_all(&disc).unwrap();
    let device = "sg_abort_dev";

    crate::server::muxer::write_marker(&disc, &ripped_marker_for("Lossy Disc", device)).unwrap();
    let (mux, _, _, _) = build_queue_views(&staging);
    assert_eq!(mux.len(), 1);

    // A terminal mux failure quarantines: `.failed`, tile=error.
    crate::server::ripper::staging::write_failed_marker(
        &disc,
        "mux finalize failed (unseekable output)",
    );
    crate::server::ripper::update_state(
        device,
        crate::server::ripper::RipState {
            device: device.to_string(),
            status: "error".to_string(),
            disc_name: "Lossy Disc".to_string(),
            last_error: "mux finalize failed (unseekable output)".to_string(),
            ..Default::default()
        },
    );
    let (mux, mv, _, _) = build_queue_views(&staging);
    assert!(mux.is_empty(), ".failed dir must leave the Mux queue");
    assert!(
        mv.is_empty(),
        ".failed dir is NOT in the Move queue (no .done)"
    );
    assert_eq!(device_status(device), Some("error".into()));

    crate::server::ripper::STATE.lock().unwrap().remove(device);
}

/// CONCURRENT devices: two drives, each with its own staged job at a
/// DIFFERENT lifecycle stage, must not cross-contaminate queue
/// membership or device status. Disc A is mid-mux-queue (`.ripped`);
/// disc B has finished (`.done` → Move queue).
#[test]
fn concurrent_devices_no_cross_contamination() {
    use std::fs;
    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();
    let dev_a = "sg_concurrent_a";
    let dev_b = "sg_concurrent_b";

    // Disc A: freshly handed off → Mux queue, tile A = done.
    let disc_a = tmp.path().join("Alpha");
    fs::create_dir_all(&disc_a).unwrap();
    crate::server::muxer::write_marker(&disc_a, &ripped_marker_for("Alpha", dev_a)).unwrap();
    crate::server::ripper::update_state(
        dev_a,
        crate::server::ripper::RipState {
            device: dev_a.to_string(),
            status: "done".to_string(),
            progress_pct: 100,
            disc_name: "Alpha".to_string(),
            ..Default::default()
        },
    );

    // Disc B: mux finished → Move queue, tile B = done.
    let disc_b = tmp.path().join("Beta");
    fs::create_dir_all(&disc_b).unwrap();
    crate::server::muxer::write_marker(&disc_b, &ripped_marker_for("Beta", dev_b)).unwrap();
    crate::server::ripper::staging::mark_handoff(&disc_b, true, |_s| {}).unwrap();
    crate::server::ripper::staging::write_completed_marker(&disc_b);
    crate::server::ripper::update_state(
        dev_b,
        crate::server::ripper::RipState {
            device: dev_b.to_string(),
            status: "done".to_string(),
            progress_pct: 100,
            disc_name: "Beta".to_string(),
            ..Default::default()
        },
    );

    let (mux, mv, _, _) = build_queue_views(&staging);
    // Alpha is in the Mux queue ONLY; Beta in the Move queue ONLY.
    assert!(
        mux.iter().any(|m| m.contains("Alpha")),
        "Alpha must be in the Mux queue"
    );
    assert!(
        !mux.iter().any(|m| m.contains("Beta")),
        "Beta must NOT be in the Mux queue"
    );
    assert!(
        mv.iter().any(|m| m.contains("Beta")),
        "Beta must be in the Move queue"
    );
    assert!(
        !mv.iter().any(|m| m.contains("Alpha")),
        "Alpha must NOT be in the Move queue"
    );
    assert!(
        !in_both_queues(&mux, &mv),
        "neither disc may be in both queues"
    );
    // Each device tile is independent.
    assert_eq!(device_status(dev_a), Some("done".into()));
    assert_eq!(device_status(dev_b), Some("done".into()));

    crate::server::ripper::STATE.lock().unwrap().remove(dev_a);
    crate::server::ripper::STATE.lock().unwrap().remove(dev_b);
}

// get_state_json END-TO-END: the serialized live payload (SSE/dashboard
// source) must never list a disc in both _mux_queue and _move_queue,
// across multiple discs — all three views derive from one snapshot.
#[test]
fn get_state_json_never_double_lists_across_discs() {
    use std::fs;
    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();

    // Three discs spanning the lifecycle: Queued/AlsoQueued (.ripped
    // only → Mux queue) and Moving (.done + .completed → Move queue).
    for (name, finished) in [("Queued", false), ("Moving", true), ("AlsoQueued", false)] {
        let d = tmp.path().join(name);
        fs::create_dir_all(&d).unwrap();
        crate::server::muxer::write_marker(&d, &ripped_marker_for(name, "sg0")).unwrap();
        if finished {
            crate::server::ripper::staging::mark_handoff(&d, true, |_s| {}).unwrap();
            crate::server::ripper::staging::write_completed_marker(&d);
        }
    }

    let json = get_state_json(&staging);
    let v: serde_json::Value = serde_json::from_str(&json).expect("state json must parse");
    let to_names = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(|q| q.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| {
                        s.replace(" (queued)", "")
                            .replace(" (malformed)", "")
                            .replace(" (moving)", "")
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mux_names = to_names("_mux_queue");
    let move_names = to_names("_move_queue");

    assert!(mux_names.contains(&"Queued".to_string()));
    assert!(mux_names.contains(&"AlsoQueued".to_string()));
    assert!(move_names.contains(&"Moving".to_string()));
    // The cross-queue invariant: no disc in both lists.
    for name in &mux_names {
        assert!(
            !move_names.contains(name),
            "BUG #3 (get_state_json): '{name}' is in BOTH _mux_queue and _move_queue"
        );
    }
}

#[test]
fn queue_view_cache_reuses_within_ttl_and_refreshes_after() {
    // build_queue_views_cached lets concurrent per-client SSE calls share
    // ONE staging-dir scan. Pin: a same-dir call within the TTL reuses the
    // stale scan; a different dir, or the TTL elapsing, forces a re-scan.
    use std::fs;
    let tmp_a = tempfile::TempDir::new().unwrap();
    let staging_a = tmp_a.path().to_string_lossy().to_string();
    let disc1 = tmp_a.path().join("First");
    fs::create_dir_all(&disc1).unwrap();
    crate::server::muxer::write_marker(&disc1, &ripped_marker_for("First", "sg0")).unwrap();

    let (mux1, _, _, _) = build_queue_views_cached(&staging_a);
    assert!(
        mux1.iter().any(|s| s.contains("First")),
        "initial scan must see the pre-existing disc"
    );

    // A different staging dir, scanned right after, must reflect ITS
    // OWN contents (empty), not staging_a's cached entry.
    let tmp_b = tempfile::TempDir::new().unwrap();
    let staging_b = tmp_b.path().to_string_lossy().to_string();
    let (mux_b, _, _, _) = build_queue_views_cached(&staging_b);
    assert!(
        mux_b.is_empty(),
        "a different staging dir must not be served staging_a's cached queue"
    );

    // Add a second disc to staging_a's directory, then immediately
    // re-query staging_a within the TTL window: the cache must still
    // return the STALE (pre-addition) view.
    let disc2 = tmp_a.path().join("Second");
    fs::create_dir_all(&disc2).unwrap();
    crate::server::muxer::write_marker(&disc2, &ripped_marker_for("Second", "sg1")).unwrap();
    let (mux2, _, _, _) = build_queue_views_cached(&staging_a);
    assert!(
        !mux2.iter().any(|s| s.contains("Second")),
        "a call within the TTL must reuse the cached (stale) scan, not re-walk the dir"
    );

    // After the TTL elapses, the next call must re-scan and see the new disc.
    std::thread::sleep(QUEUE_VIEW_CACHE_TTL + std::time::Duration::from_millis(150));
    let (mux3, _, _, _) = build_queue_views_cached(&staging_a);
    assert!(
        mux3.iter().any(|s| s.contains("Second")),
        "after the TTL expires, the next call must re-scan and see the new disc"
    );
}

// /api/state (and thus the Dockerfile HEALTHCHECK) must stay responsive
// while a staging-dir refresh is in flight — a slow read_dir must never
// park every other caller. 250ms bound trips a blocked reader, not a hang.
#[test]
fn queue_view_cache_reader_not_blocked_by_in_flight_scan() {
    use std::time::{Duration, Instant};
    const SCAN_MS: u64 = 2000;
    // The reader serves the cached (stale) view in ~1ms, so any measurable
    // delay means it BLOCKED behind the in-flight scan. Bound at HALF the
    // scan: above CI jitter for a hit, yet a real block still trips it.
    const READER_BOUND_MS: u128 = (SCAN_MS / 2) as u128;

    let tmp = tempfile::TempDir::new().unwrap();
    // Unique fixture path (tempdir) so the process-global probe/cache
    // keyed by staging_dir cannot collide with another test.
    let staging = tmp.path().to_string_lossy().to_string();
    let disc = tmp.path().join("Primed");
    std::fs::create_dir_all(&disc).unwrap();
    crate::server::muxer::write_marker(&disc, &ripped_marker_for("Primed", "sg0")).unwrap();

    // Prime the cache with a fast scan (the steady state on the rig:
    // /api/state has been polled once a second for the container's life).
    let (primed, _, _, _) = build_queue_views_cached(&staging);
    assert!(
        primed.iter().any(|s| s.contains("Primed")),
        "priming scan must see the pre-existing disc"
    );

    // Now make this dir's scan pathologically slow, and let the cached
    // entry age past the TTL so the next caller triggers a refresh.
    queue_scan_probe::arm(&staging, SCAN_MS);
    std::thread::sleep(QUEUE_VIEW_CACHE_TTL + Duration::from_millis(50));

    let s2 = staging.clone();
    let refresher = std::thread::spawn(move || build_queue_views_cached(&s2));

    // Bounded wait until the slow scan is genuinely in flight.
    let spin = Instant::now();
    while queue_scan_probe::scans(&staging) < 1 && spin.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        queue_scan_probe::scans(&staging),
        1,
        "the slow refresh scan never started; test setup is wrong"
    );

    // The healthcheck-equivalent read, concurrent with that scan.
    let t0 = Instant::now();
    let (mux, _, _, _) = build_queue_views_cached(&staging);
    let elapsed = t0.elapsed();
    let _ = refresher.join();

    assert!(
        mux.iter().any(|s| s.contains("Primed")),
        "a reader served during a refresh must still get a usable (stale) queue view"
    );
    assert!(
        elapsed.as_millis() < READER_BOUND_MS,
        "/api/state reader blocked {elapsed:?} behind an in-flight staging scan \
             (bound {READER_BOUND_MS}ms, scan {SCAN_MS}ms) — a stalled scan can stall \
             the Docker healthcheck"
    );
}

// Root-cause guard: the Phase-3 prune must NOT evict a stale-but-recent
// key. Evicting at the sub-second serve TTL destroyed the stale snapshot
// stale-while-revalidate needs, forcing the next reader to block the scan.
#[test]
fn queue_view_prune_keeps_stale_but_recent_key_serveable() {
    use std::time::Duration;

    let tmp_a = tempfile::TempDir::new().unwrap();
    let a = tmp_a.path().to_string_lossy().to_string();
    let disc = tmp_a.path().join("Kept");
    std::fs::create_dir_all(&disc).unwrap();
    crate::server::muxer::write_marker(&disc, &ripped_marker_for("Kept", "sg0")).unwrap();
    let (primed, _, _, _) = build_queue_views_cached(&a);
    assert!(
        primed.iter().any(|s| s.contains("Kept")),
        "priming scan must see the disc"
    );

    // Age A's snapshot past the serve TTL — the threshold the OLD prune
    // (wrongly) used to evict, but well within the RETAIN horizon.
    std::thread::sleep(QUEUE_VIEW_CACHE_TTL + Duration::from_millis(50));

    // A scan of an unrelated dir B runs the Phase-3 prune across the whole
    // map. Under the old TTL-based retain this evicted A; it must not now.
    let tmp_b = tempfile::TempDir::new().unwrap();
    let b = tmp_b.path().to_string_lossy().to_string();
    let _ = build_queue_views_cached(&b);

    let map = QUEUE_VIEW_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let kept = map
        .get(&a)
        .and_then(|e| e.snapshot.as_ref())
        .map(|s| s.views().0);
    assert!(
        kept.is_some_and(|mux| mux.iter().any(|s| s.contains("Kept"))),
        "the prune evicted a stale-but-recent key's snapshot; \
             stale-while-revalidate is broken and a concurrent reader of that \
             key will block a full staging scan (the healthcheck stall)"
    );
}

/// The counterpart guard: fixing the blocking above must NOT turn the
/// cache into a thundering herd. N concurrent cold callers must produce
/// ONE scan of the staging dir, not N.
#[test]
fn queue_view_cache_single_flights_concurrent_callers() {
    const SCAN_MS: u64 = 300;
    const CALLERS: usize = 8;

    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();
    let disc = tmp.path().join("Solo");
    std::fs::create_dir_all(&disc).unwrap();
    crate::server::muxer::write_marker(&disc, &ripped_marker_for("Solo", "sg0")).unwrap();

    // Armed BEFORE the first call: this dir has never been scanned, so
    // every caller below is a cold miss racing every other one.
    queue_scan_probe::arm(&staging, SCAN_MS);

    let handles: Vec<_> = (0..CALLERS)
        .map(|_| {
            let s = staging.clone();
            std::thread::spawn(move || build_queue_views_cached(&s))
        })
        .collect();
    for h in handles {
        let (mux, _, _, _) = h.join().expect("caller thread panicked");
        assert!(
            mux.iter().any(|s| s.contains("Solo")),
            "every concurrent caller must get the real queue view"
        );
    }
    assert_eq!(
        queue_scan_probe::scans(&staging),
        1,
        "{CALLERS} concurrent callers caused {} staging scans; single-flight is broken \
             (thundering herd on the staging dir)",
        queue_scan_probe::scans(&staging)
    );

    // And a further caller inside the TTL window still re-uses it.
    let _ = build_queue_views_cached(&staging);
    assert_eq!(
        queue_scan_probe::scans(&staging),
        1,
        "a caller within the TTL must be served from cache"
    );
}

// Run build_queue_views_cached(dir) on a scratch thread, returning None on
// timeout so a regression surfaces as a FAILED assertion, not a CI hang.
// The thread is deliberately never joined — a wedged scan is what we simulate.
fn cached_within(
    dir: &str,
    bound: std::time::Duration,
) -> Option<(Vec<String>, Vec<String>, usize, usize)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let d = dir.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(build_queue_views_cached(&d));
    });
    rx.recv_timeout(bound).ok()
}

/// Spin until this dir has taken `n` scans, or `bound` elapses.
fn await_scans(dir: &str, n: usize, bound: std::time::Duration) -> bool {
    let t0 = std::time::Instant::now();
    while queue_scan_probe::scans(dir) < n {
        if t0.elapsed() >= bound {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    true
}

// A refresher that HANGS (wedged read_dir) must not latch the
// single-flight marker forever: the old plain-bool marker stayed set for
// the process lifetime, freezing queue views with no surfaced failure.
#[test]
fn queue_view_cache_recovers_from_a_refresher_that_never_returns() {
    use std::time::Duration;
    const WEDGE_MS: u64 = 60_000;
    const DEADLINE_MS: u64 = 300;
    const TAKEOVER_BOUND: Duration = Duration::from_secs(4);

    let tmp = tempfile::TempDir::new().unwrap();
    // Unique to this test: the cache, the probe and STATE are all
    // process-global, so a shared fixture name would be a real race.
    let staging = tmp.path().to_string_lossy().to_string();
    let first = tmp.path().join("WedgeFirst");
    std::fs::create_dir_all(&first).unwrap();
    crate::server::muxer::write_marker(&first, &ripped_marker_for("WedgeFirst", "sg0")).unwrap();

    // Steady state: the cache is warm, as it is on the rig after the
    // first second of /api/state polling.
    let (primed, _, _, _) =
        cached_within(&staging, Duration::from_secs(5)).expect("priming scan must return");
    assert!(
        primed.iter().any(|s| s.contains("WedgeFirst")),
        "priming scan must see the pre-existing disc"
    );

    queue_scan_probe::set_refresh_deadline(&staging, DEADLINE_MS);
    // The mount wedges. Age the entry past the TTL so the next caller
    // owns the refresh, then let that caller disappear into `read_dir`.
    queue_scan_probe::arm(&staging, WEDGE_MS);
    std::thread::sleep(QUEUE_VIEW_CACHE_TTL + Duration::from_millis(50));
    let wedged = staging.clone();
    std::thread::spawn(move || build_queue_views_cached(&wedged));
    assert!(
        await_scans(&staging, 1, Duration::from_secs(2)),
        "the wedged refresh never started; test setup is wrong"
    );

    // Reality moves on underneath the wedged refresher.
    let second = tmp.path().join("WedgeSecond");
    std::fs::create_dir_all(&second).unwrap();
    crate::server::muxer::write_marker(&second, &ripped_marker_for("WedgeSecond", "sg1")).unwrap();
    // ...and the mount comes back for anyone who tries again. The
    // original refresher is still parked in its 60 s `read_dir`.
    queue_scan_probe::arm(&staging, 0);

    // Poll as /api/state does. Once the marker's deadline passes, some
    // caller must take the refresh over and publish the new disc.
    let deadline = std::time::Instant::now() + TAKEOVER_BOUND;
    let mut saw_second = false;
    while std::time::Instant::now() < deadline {
        let (mux, _, _, _) = cached_within(&staging, TAKEOVER_BOUND)
            .expect("a caller blocked past the takeover bound behind a wedged refresh");
        if mux.iter().any(|s| s.contains("WedgeSecond")) {
            saw_second = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        saw_second,
        "the single-flight marker latched on a refresher that never returned: \
             the queue views are frozen at the moment the mount wedged and will \
             stay frozen for the process lifetime"
    );
}

// Cold-path counterpart: with nothing to serve and the first scan wedged,
// callers must neither park forever nor pile up (old valve burned an
// HTTP worker per waiter every 5s until /api/state 503'd and restarted).
#[test]
fn queue_view_cache_cold_callers_neither_park_nor_pile_up_on_a_wedged_scan() {
    use std::time::Duration;
    const WEDGE_MS: u64 = 60_000;
    const COLD_WAIT_MS: u64 = 200;
    const CALLER_BOUND: Duration = Duration::from_secs(3);
    const CALLERS: usize = 6;

    let tmp = tempfile::TempDir::new().unwrap();
    let staging = tmp.path().to_string_lossy().to_string();
    let disc = tmp.path().join("ColdWedge");
    std::fs::create_dir_all(&disc).unwrap();
    crate::server::muxer::write_marker(&disc, &ripped_marker_for("ColdWedge", "sg0")).unwrap();

    // Armed before the first ever call: this key is genuinely cold, so
    // there is no snapshot to fall back on.
    queue_scan_probe::arm(&staging, WEDGE_MS);
    queue_scan_probe::set_cold_wait(&staging, COLD_WAIT_MS);
    let wedged = staging.clone();
    std::thread::spawn(move || build_queue_views_cached(&wedged));
    assert!(
        await_scans(&staging, 1, Duration::from_secs(2)),
        "the wedged cold scan never started; test setup is wrong"
    );

    // Every subsequent caller must come back — degraded is fine, wedged
    // is not. This is the /api/state + --healthcheck path.
    for i in 0..CALLERS {
        assert!(
            cached_within(&staging, CALLER_BOUND).is_some(),
            "cold caller {i} was still parked after {CALLER_BOUND:?} behind a wedged \
                 first scan — /api/state (and the Docker HEALTHCHECK) stalls with it"
        );
    }

    // ...and none of them may launch a scan of its own: that is the
    // accumulation that eats an HTTP worker per giving-up caller.
    assert_eq!(
        queue_scan_probe::scans(&staging),
        1,
        "{CALLERS} cold callers launched {} scans of a wedged staging dir; the cold \
             valve abandoned single-flight, so each one burns an HTTP worker thread and \
             its admission token until /api/state 503s",
        queue_scan_probe::scans(&staging),
    );
}

/// Helper: current status string of a device in the global STATE map.
fn device_status(device: &str) -> Option<String> {
    ripper::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .map(|s| s.status.clone())
}

#[test]
fn keydb_body_under_cap_is_accepted() {
    let body = vec![b'x'; 100];
    let out = read_capped_keydb_body(&body[..], 10 * 1024 * 1024).unwrap();
    assert_eq!(out, body);
}

#[test]
fn keydb_body_exactly_at_cap_is_accepted() {
    // The cap is inclusive: a body of exactly max_bytes must pass (no
    // false-positive on a legitimately cap-sized keydb).
    let cap: u64 = 4096;
    let body = vec![b'x'; cap as usize];
    let out = read_capped_keydb_body(&body[..], cap).unwrap();
    assert_eq!(out.len() as u64, cap);
}

#[test]
fn keydb_body_over_cap_is_rejected() {
    // Regression (finding 2): a body one byte past the cap must be
    // detected as TooLarge, not silently truncated to the cap.
    let cap: u64 = 4096;
    let body = vec![b'x'; cap as usize + 1];
    let err = read_capped_keydb_body(&body[..], cap).unwrap_err();
    assert_eq!(err, KeydbReadError::TooLarge);
}

#[test]
fn device_name_accepts_cross_os_keys() {
    // Linux sg, macOS disk, Windows CdRom — the basenames list_drives yields.
    assert!(is_valid_device_name("sg0"));
    assert!(is_valid_device_name("sg4"));
    assert!(is_valid_device_name("sg15"));
    assert!(is_valid_device_name("disk6")); // macOS
    assert!(is_valid_device_name("ioreg:4295125507"));
    assert_eq!(device_path("ioreg:4295125507"), "ioreg:4295125507");
    for bad in [
        "ioreg:",
        "ioreg:../a",
        "ioreg:1/stop",
        "ioreg:18446744073709551616",
    ] {
        assert!(!is_valid_device_name(bad));
    }
    assert!(is_valid_device_name("CdRom0")); // Windows
}

#[test]
fn device_name_rejects_path_traversal_and_typos() {
    // The exact bug that created the phantom "sg4/stop" tab. This is a
    // path-safety boundary, not a drive-existence check — an unknown
    // well-formed name is accepted as *format* and fails downstream.
    assert!(!is_valid_device_name("sg4/stop"));
    assert!(!is_valid_device_name("sg4/verify"));
    assert!(!is_valid_device_name("../etc/passwd"));
    assert!(!is_valid_device_name("sg4 ")); // trailing space
    assert!(!is_valid_device_name("sg")); // too short (< 3)
    assert!(!is_valid_device_name(""));
    assert!(!is_valid_device_name("a/b"));
    assert!(!is_valid_device_name("..")); // dots are separators
}

#[test]
fn poster_url_validation() {
    assert!(is_valid_poster_url(
        "https://image.tmdb.org/t/p/w500/abc.jpg"
    ));
    assert!(is_valid_poster_url("http://example.com/poster.png"));
    // Wrong scheme.
    assert!(!is_valid_poster_url("javascript:alert(1)"));
    assert!(!is_valid_poster_url("ftp://example.com/x.jpg"));
    assert!(!is_valid_poster_url("//example.com/x.jpg"));
    // Attribute-breakout / control chars.
    assert!(!is_valid_poster_url("https://example.com/\"><script>"));
    assert!(!is_valid_poster_url("https://example.com/x'onerror=1"));
    assert!(!is_valid_poster_url("https://example.com/a\nb"));
}

// The dashboard's esc() must HTML-escape all five sensitive characters
// (& < > " '); a textContent/innerHTML round-trip (prior impl) leaves
// " and ' unescaped. Also assert the shipped JS carries the quote escapes.
#[test]
fn dashboard_esc_escapes_all_five() {
    fn esc(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;")
    }
    assert_eq!(esc("\"x<>&'"), "&quot;x&lt;&gt;&amp;&#39;");
    // The shipped JS must escape quotes and apostrophes, not just <>&.
    assert!(asset("ui.js").contains(r#"replace(/"/g,'&quot;')"#));
    assert!(asset("ui.js").contains(r"replace(/'/g,'&#39;')"));
}

// Error text reaches the dashboard via innerHTML — bare URLs need
// escLinks() (built on esc()) to become anchors while staying safe.
#[test]
fn dashboard_error_text_linkifies_urls() {
    assert!(asset("ui.js").contains("function escLinks(s){"));
    // The red error banner.
    assert!(asset("ripper.js").contains("escLinks(s.last_error)"));
    // Only https is linkified — no javascript:/data: anchors.
    assert!(asset("ui.js").contains(r#"/https:\/\/[^\s<>"']+/g"#));
}

// H7: executes the shipped esc/escLinks JS under node (skipped when node
// is absent). A URL next to a quote must not swallow the escaped entity.
#[test]
fn dashboard_esc_links_executes_correctly_under_node() {
    if std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_err()
    {
        // Skip for local dev only; CI must actually execute the JS.
        assert!(
            std::env::var_os("CI").is_none(),
            "node not found on PATH but CI is set: the escLinks test must run on CI"
        );
        eprintln!("skipped: node not found on PATH");
        return;
    }
    let ui = asset("ui.js");
    let esc_at = ui.find("function esc(s){").unwrap();
    let esc_end = esc_at + ui[esc_at..].find('\n').unwrap();
    let links_at = ui.find("function escLinks(s){").unwrap();
    let links_end = links_at + ui[links_at..].find("\nexport ").unwrap();
    let a = |u: &str| {
        format!(
            r#"<a href="{u}" target="_blank" rel="noopener noreferrer" style="color:inherit">{u}</a>"#
        )
    };
    let cases: Vec<(&str, String)> = vec![
        (
            "see 'https://example.com/x' now",
            format!("see &#39;{}&#39; now", a("https://example.com/x")),
        ),
        (
            r#"at "https://example.com/y"."#,
            format!("at &quot;{}&quot;.", a("https://example.com/y")),
        ),
        (
            "https://example.com/q?a=1&b=2.",
            format!("{}.", a("https://example.com/q?a=1&amp;b=2")),
        ),
        (
            "<b>https://example.com/z</b>",
            format!("&lt;b&gt;{}&lt;/b&gt;", a("https://example.com/z")),
        ),
        (
            "no link & 'quotes'",
            "no link &amp; &#39;quotes&#39;".into(),
        ),
    ];
    let inputs: Vec<&str> = cases.iter().map(|c| c.0).collect();
    let script = format!(
        "{}\n{}\nconsole.log(JSON.stringify({}.map(escLinks)));",
        &ui[esc_at..esc_end],
        &ui[links_at..links_end],
        serde_json::to_string(&inputs).unwrap()
    );
    let out = std::process::Command::new("node")
        .arg("-e")
        .arg(&script)
        .output()
        .expect("run node");
    assert!(
        out.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got: Vec<String> = serde_json::from_slice(&out.stdout).expect("node JSON output");
    for ((input, want), got) in cases.iter().zip(&got) {
        assert_eq!(got, want, "escLinks({input:?})");
    }
}

#[test]
fn settings_get_redacts_secrets() {
    let c = Config {
        tmdb_api_key: "real-tmdb-key".into(),
        keyserver_secret: "real-bearer-token".into(),
        ..Config::default()
    };
    let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&c)).unwrap();
    assert_eq!(json["tmdb_api_key"], SECRET_SENTINEL);
    assert_eq!(json["keyserver_secret"], SECRET_SENTINEL);
    // An empty secret stays empty (no sentinel) so the UI shows a blank field.
    let json2: serde_json::Value =
        serde_json::from_str(&settings_json_redacted(&Config::default())).unwrap();
    assert_eq!(json2["tmdb_api_key"], "");
}

#[test]
fn settings_get_masks_keyserver_url_token_in_path() {
    // keyserver_url may carry an auth token in the path
    // (e.g. https://keys.example.com/mytoken/decode). GET must mask the
    // path but keep the origin so the operator can identify the server.
    let c = Config {
        keyserver_url: "https://keys.example.com/mysecrettoken/decode".into(),
        keydb_url: "https://keydb.example.com/authtoken/keydb.zip".into(),
        ..Config::default()
    };
    let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&c)).unwrap();
    // Origin preserved, token-bearing path replaced with sentinel.
    assert_eq!(json["keyserver_url"], "https://keys.example.com/********");
    assert_eq!(json["keydb_url"], "https://keydb.example.com/********");
    // Tokens must not appear in the redacted output.
    assert!(
        !json["keyserver_url"]
            .as_str()
            .unwrap()
            .contains("mysecrettoken")
    );
    assert!(!json["keydb_url"].as_str().unwrap().contains("authtoken"));
    // Empty URLs stay empty (no sentinel so the UI shows a blank field).
    let json2: serde_json::Value =
        serde_json::from_str(&settings_json_redacted(&Config::default())).unwrap();
    assert_eq!(json2["keyserver_url"], "");
    assert_eq!(json2["keydb_url"], "");
}

// handle_settings_post must DROP the config write guard before saving:
// config::save's fs::write+rename can hang on NFS, blocking every
// concurrent cfg.read() (the 0.20.8 lock stall) — pinned against source.
#[test]
fn settings_post_saves_outside_the_config_write_guard() {
    let src = crate::server::util::source_lf(include_str!("web.rs"));
    // Leading newline so this does not match the literal on this line.
    let start = src
        .find("\nfn handle_settings_post(")
        .expect("web.rs must define handle_settings_post");
    let body = &src[start..];

    // The mutation window: a `snapshot` bound from a block that takes
    // `cfg.write()`, closing at the block's `};`.
    let snap = body
        .find("let snapshot: Config = {")
        .expect("handle_settings_post must snapshot the config out of the guard");
    assert!(
        body[snap..].starts_with("let snapshot: Config = {")
            && body[snap..]
                .find("cfg.write()")
                .is_some_and(|w| w < body[snap..].find("\n    };").unwrap_or(usize::MAX)),
        "the write guard must be taken INSIDE the snapshot block"
    );
    let guard_end = snap
        + body[snap..]
            .find("\n    };")
            .expect("the snapshot block must close at function-body indentation");

    let save = guard_end
        + body[guard_end..]
            .find("config::save_coalesced(snapshot, save_gen)")
            .expect("handle_settings_post must queue the snapshot with its generation");

    // Nothing may re-take the write guard between the snapshot block and
    // the save — that is the whole ordering.
    assert!(
        !body[guard_end..save].contains("cfg.write()"),
        "the config write guard must not be held across config::save; \
             re-taking it before the save reintroduces the 0.20.8 lock stall"
    );
    // H9: the generation is allocated INSIDE the guard, after the write
    // lock, so save order matches in-memory mutation order.
    let write_at = snap + body[snap..].find("cfg.write()").unwrap_or(usize::MAX);
    let gen_at = body[snap..guard_end]
        .find("save_gen = config::next_save_generation();")
        .map(|i| snap + i)
        .expect("save_gen must be allocated inside the snapshot (write-guard) block");
    assert!(
        gen_at > write_at,
        "save_gen must be taken after cfg.write()"
    );
    let fn_end = body[1..].find("\nfn ").map_or(body.len(), |i| i + 1);
    assert_eq!(
        body[..fn_end].matches("next_save_generation()").count(),
        1,
        "exactly one generation allocation in handle_settings_post"
    );
    // Awaited with a deadline, never inline-blocking on the save.
    assert!(
        body[save..].contains("rx.recv_timeout("),
        "the handler must await the queued save with a deadline"
    );
}

// NOTE: the keyserver_url sentinel round-trip is now tested
// executing-style by `http::settings_post_masked_keyserver_url_preserves_stored`,
// driving the real handler via a live server, not an inline reimplementation.

/// A stored webhook entry that fires on every stage — the common case in
/// these tests, which predate per-stage flags and care only about URL
/// masking/resolution.
fn we(url: &str) -> WebhookEntry {
    WebhookEntry {
        url: url.to_string(),
        post_rip: true,
        post_mux: true,
        post_move: true,
        headers: Default::default(),
    }
}

/// An incoming (POST-side) webhook with all flags set — mirrors what the
/// UI sends for a fire-on-every-stage hook.
fn inc(url: &str) -> IncomingWebhook {
    IncomingWebhook {
        url: url.to_string(),
        post_rip: true,
        post_mux: true,
        post_move: true,
        headers: Default::default(),
    }
}

/// Just the resolved URLs, for asserting URL resolution independently of
/// the flags (which these tests carry through unchanged).
fn urls(entries: &[WebhookEntry]) -> Vec<String> {
    entries.iter().map(|e| e.url.clone()).collect()
}

#[test]
fn settings_get_masks_webhook_token_keeps_origin() {
    // Webhook URLs embed bearer tokens (Discord/Slack/Jellyfin) in the
    // path, so a GET must mask the token — but keep the origin visible so
    // the operator can tell which hook is which.
    let c = Config {
        webhook_urls: vec![
            we("https://discord.com/api/webhooks/123/secrettoken"),
            we(""),
            we("https://hooks.slack.com/services/AAA/BBB/cccsecret"),
        ],
        ..Config::default()
    };
    let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&c)).unwrap();
    let arr = json["webhook_urls"].as_array().unwrap();
    // Each entry serializes as an object; only the `url` is masked and it
    // carries a stable per-entry index (#<pos>) so two same-origin hooks
    // round-trip unambiguously (#8). The flags pass through untouched.
    assert_eq!(arr[0]["url"], "https://discord.com/********#0");
    assert_eq!(arr[0]["post_rip"], true);
    assert_eq!(arr[0]["post_mux"], true);
    assert_eq!(arr[0]["post_move"], true);
    // Empty entry stays empty (no sentinel) so the UI shows a blank row.
    assert_eq!(arr[1]["url"], "");
    assert_eq!(arr[2]["url"], "https://hooks.slack.com/********#2");
    // The masked form must NOT leak the token.
    assert!(!arr[0]["url"].as_str().unwrap().contains("secrettoken"));
    assert!(!arr[2]["url"].as_str().unwrap().contains("cccsecret"));
}

#[test]
fn settings_get_redacts_keydb_path_to_filename() {
    // keydb_path is an absolute container path (mount layout, username);
    // GET must strip it to the bare filename so a LAN client can't
    // learn the container's filesystem layout.
    let c = Config {
        keydb_path: Some("/data/keys/subdir/KEYDB.cfg".into()),
        ..Config::default()
    };
    let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&c)).unwrap();
    assert_eq!(json["keydb_path"], "KEYDB.cfg");
    assert!(
        !json["keydb_path"].as_str().unwrap().contains('/'),
        "redacted keydb_path must not leak any directory component"
    );
    // No keydb_path set (None) — field passes through untouched (null),
    // no panic, nothing to redact.
    let json_none: serde_json::Value =
        serde_json::from_str(&settings_json_redacted(&Config::default())).unwrap();
    assert!(json_none["keydb_path"].is_null());
    // An explicitly empty string is left alone (not redacted into "" ->
    // something else), matching the other secret fields' "empty stays
    // empty" convention.
    let c_empty = Config {
        keydb_path: Some(String::new()),
        ..Config::default()
    };
    let json_empty: serde_json::Value =
        serde_json::from_str(&settings_json_redacted(&c_empty)).unwrap();
    assert_eq!(json_empty["keydb_path"], "");
}

// Operator-facing diagnostic (issue #46): GET /api/settings surfaces the
// ACTUAL resolved keydb path + present/absent status so the UI shows exactly
// where autorip reads keys — the missing signal that made #46 undiagnosable.
#[test]
fn settings_get_surfaces_resolved_keydb_path_and_status() {
    // Present: canonical file exists at the resolved path.
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config {
        autorip_dir: tmp.path().to_string_lossy().into_owned(),
        keydb_path: None,
        ..Config::default()
    };
    let expected = tmp.path().join("keydb.cfg");
    std::fs::write(&expected, "x").unwrap();
    let json: serde_json::Value = serde_json::from_str(&settings_json_redacted(&cfg)).unwrap();
    let resolved = json["keydb_resolved"].as_str().unwrap();
    assert!(
        resolved.contains(&expected.display().to_string()),
        "resolved path must be shown for operator diagnosis: {resolved}"
    );
    assert!(resolved.contains("file present"));

    // Absent: an explicit path with no file → NOT FOUND status.
    let cfg2 = Config {
        keydb_path: Some("/no/such/dir/keydb.cfg".into()),
        ..Config::default()
    };
    let json2: serde_json::Value = serde_json::from_str(&settings_json_redacted(&cfg2)).unwrap();
    assert!(
        json2["keydb_resolved"]
            .as_str()
            .unwrap()
            .contains("NOT FOUND")
    );
}

#[test]
fn mask_webhook_url_variants() {
    assert_eq!(
        mask_webhook_url("https://discord.com/api/webhooks/1/tok"),
        "https://discord.com/********"
    );
    // Host with port.
    assert_eq!(
        mask_webhook_url("http://jellyfin.example:8096/webhook/abc"),
        "http://jellyfin.example:8096/********"
    );
    // Bare origin, no path → still origin/sentinel.
    assert_eq!(
        mask_webhook_url("https://example.com"),
        "https://example.com/********"
    );
    // No scheme → fully masked (nothing identifiable to keep).
    assert_eq!(mask_webhook_url("not-a-url"), SECRET_SENTINEL);
}

#[test]
fn mask_webhook_url_strips_query_string_token() {
    // Token in query string with no path slash — must not appear in output.
    assert_eq!(
        mask_webhook_url("https://hooks.example.com?token=SUPERSECRET"),
        "https://hooks.example.com/********"
    );
    // Fragment-only (no path) — similarly stripped.
    assert_eq!(
        mask_webhook_url("https://hooks.example.com#frag"),
        "https://hooks.example.com/********"
    );
}

#[test]
fn mask_webhook_url_strips_basic_auth_userinfo() {
    // user:pass@host must NOT leak into the masked value returned to the
    // client. Only scheme://host[:port] survives.
    assert_eq!(
        mask_webhook_url("https://user:pass@host/x"),
        "https://host/********"
    );
    // Userinfo + explicit port.
    assert_eq!(
        mask_webhook_url("https://user:pass@host:8443/webhook/tok"),
        "https://host:8443/********"
    );
    // user-only (no colon) userinfo also stripped.
    assert_eq!(
        mask_webhook_url("http://alice@example.com/hook"),
        "http://example.com/********"
    );
    // An '@' only inside the path (no userinfo in authority) is untouched.
    assert_eq!(
        mask_webhook_url("https://example.com/a@b/c"),
        "https://example.com/********"
    );
    // A bare-origin URL with userinfo (no path) is still stripped.
    assert_eq!(
        mask_webhook_url("https://user:pass@example.com"),
        "https://example.com/********"
    );
}

#[test]
fn is_masked_webhook_recognizes_only_real_placeholders() {
    // Bare sentinel (mask_webhook_url's own output, e.g. no identifiable
    // scheme).
    assert!(is_masked_webhook(SECRET_SENTINEL));
    // Indexed placeholder form produced by mask_webhook_url_indexed.
    assert!(is_masked_webhook(&format!(
        "https://discord.com/{SECRET_SENTINEL}#1"
    )));
    // Index 0 is still a valid, non-empty all-digit index.
    assert!(is_masked_webhook(&format!(
        "https://discord.com/{SECRET_SENTINEL}#0"
    )));
    // Empty index after '#' must NOT be treated as masked.
    assert!(!is_masked_webhook(&format!(
        "https://discord.com/{SECRET_SENTINEL}#"
    )));
    // Non-digit index must NOT be treated as masked.
    assert!(!is_masked_webhook(&format!(
        "https://discord.com/{SECRET_SENTINEL}#abc"
    )));
    // A hostile URL merely ending in "#<digits>" must NOT be misclassified
    // as a masked placeholder — the SSRF bypass an &&->|| mutation in
    // is_masked_webhook would open up (a metadata URL + `#1` fragment).
    assert!(!is_masked_webhook("http://169.254.169.254/x#1"));
    // No '#' at all, and no sentinel — not masked.
    assert!(!is_masked_webhook("http://example.com/hook/realtoken"));
}

#[test]
fn webhook_sentinel_filter_uses_ends_with() {
    // A URL that CONTAINS but does not END WITH the sentinel must NOT be
    // skipped by the SSRF-validation filter — it could be an attacker URL
    // crafted to embed the sentinel in a path segment.
    let sentinel = SECRET_SENTINEL;
    let tricky = format!("https://evil.com/{}@attacker.com/path", sentinel);
    // It is NOT masked, so it is not filtered (it would be validated /
    // rejected by validate_fetch_url).
    assert!(!is_masked_webhook(&tricky));
    // The masked form IS filtered.
    let masked = format!("https://discord.com/{}", sentinel);
    assert!(is_masked_webhook(&masked));
}

// A genuine URL that merely EMBEDS the sentinel is not masked, so the
// resolver must take it verbatim. Using `contains` instead rejected such
// a URL 400 as "ambiguous masked entry", killing settings save entirely.
#[test]
fn a_url_that_only_embeds_the_sentinel_is_saved_verbatim() {
    // Not masked by the strict predicate — the filter validates it.
    let embedded = format!("https://example.com/hook/{SECRET_SENTINEL}/tail");
    assert!(
        !is_masked_webhook(&embedded),
        "fixture must be a NON-masked URL for this test to mean anything"
    );

    let existing = vec![we("https://discord.com/api/webhooks/1/aaa")];
    let resolved = resolve_webhook_entries(&[inc(&embedded)], &existing)
        .expect("a genuine URL must not be rejected as an ambiguous placeholder");
    assert_eq!(
        urls(&resolved),
        vec![embedded],
        "a non-masked entry is taken verbatim"
    );
}

#[test]
fn webhook_post_sentinel_preserves_stored_url() {
    // A GET→POST round-trip of the redacted form must NOT wipe the
    // token-bearing stored URL. A masked placeholder resolves back to its
    // stored secret by origin; a real entry replaces; an empty entry drops.
    let existing = vec![
        we("https://discord.com/api/webhooks/1/aaa"),
        we("https://hooks.slack.com/services/x/y/zzz"),
    ];
    let incoming = [
        inc("https://discord.com/********"), // masked → keep discord secret
        inc("https://example.com/new-hook"), // changed → replace
    ];
    let resolved = resolve_webhook_entries(&incoming, &existing).unwrap();
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].url, "https://discord.com/api/webhooks/1/aaa");
    assert_eq!(resolved[1].url, "https://example.com/new-hook");
}

#[test]
fn webhook_post_masked_resolves_by_origin_not_position() {
    // HIGH regression: the UI reorders masked rows to [slack, discord].
    // Resolving BY POSITION would bind slack's row to discord's secret —
    // a silent confusion bug. By origin, each resolves to its own secret.
    let existing = vec![
        we("https://discord.com/api/webhooks/1/secretA"),
        we("https://hooks.slack.com/services/x/y/secretB"),
    ];
    // Reordered: slack first, discord second (each still masked).
    let reordered = [
        inc("https://hooks.slack.com/********"),
        inc("https://discord.com/********"),
    ];
    let resolved = resolve_webhook_entries(&reordered, &existing).unwrap();
    assert_eq!(
        urls(&resolved),
        vec![
            "https://hooks.slack.com/services/x/y/secretB".to_string(),
            "https://discord.com/api/webhooks/1/secretA".to_string(),
        ],
        "each masked entry must carry its own origin's secret, not the other's"
    );

    // Deleting the discord row and keeping only the (masked) slack row must
    // still resolve slack correctly — never to discord's secret.
    let only_slack = [inc("https://hooks.slack.com/********")];
    let resolved = resolve_webhook_entries(&only_slack, &existing).unwrap();
    assert_eq!(
        urls(&resolved),
        vec!["https://hooks.slack.com/services/x/y/secretB".to_string()]
    );
}

#[test]
fn webhook_post_masked_unresolvable_origin_is_rejected() {
    // A masked entry whose origin matches NO stored URL (the referenced row
    // was deleted) is ambiguous — reject rather than guess. Likewise when
    // two stored hooks share an origin (>1 match).
    let existing = vec![we("https://discord.com/api/webhooks/1/aaa")];
    // Masked slack origin has no stored counterpart → Err.
    let orphan = [inc("https://hooks.slack.com/********")];
    assert!(resolve_webhook_entries(&orphan, &existing).is_err());

    // Two stored discord hooks share an origin → a masked discord entry is
    // ambiguous (>1 match) → Err.
    let two_discord = vec![
        we("https://discord.com/api/webhooks/1/aaa"),
        we("https://discord.com/api/webhooks/2/bbb"),
    ];
    let masked = [inc("https://discord.com/********")];
    assert!(resolve_webhook_entries(&masked, &two_discord).is_err());
}

#[test]
fn webhook_two_same_origin_round_trip_by_index() {
    // Regression (#8): same-origin webhooks used to mask to the SAME
    // placeholder, making a GET→POST round-trip ambiguous and the save
    // permanently rejected. A stable per-entry index fixes that.
    let existing = vec![
        we("https://discord.com/api/webhooks/1/secretA"),
        we("https://discord.com/api/webhooks/2/secretB"),
    ];
    // Exactly what GET /api/settings now emits.
    let masked0 = mask_webhook_url_indexed(&existing[0].url, 0);
    let masked1 = mask_webhook_url_indexed(&existing[1].url, 1);
    assert_ne!(masked0, masked1, "same-origin masks must differ by index");

    let incoming = [inc(&masked0), inc(&masked1)];
    let resolved = resolve_webhook_entries(&incoming, &existing).unwrap();
    assert_eq!(
        urls(&resolved),
        vec![
            "https://discord.com/api/webhooks/1/secretA".to_string(),
            "https://discord.com/api/webhooks/2/secretB".to_string(),
        ],
        "each indexed mask must resolve to its own stored secret"
    );

    // A stale index whose origin mask no longer matches must be rejected,
    // not silently bound to the wrong secret.
    let stale = [inc(&mask_webhook_url_indexed("https://discord.com/x", 5))];
    assert!(resolve_webhook_entries(&stale, &existing).is_err());
}

#[test]
fn resolve_webhook_entries_carries_flags_through_masking() {
    // Per-event flags come from the INCOMING request, never the stored
    // entry — resolving a masked URL must not restore its old flags.
    // The client re-saves as move-only; that intent must win.
    let existing = vec![WebhookEntry {
        url: "https://discord.com/api/webhooks/1/secretA".into(),
        post_rip: true,
        post_mux: true,
        post_move: true,
        headers: Default::default(),
    }];
    let masked = mask_webhook_url_indexed(&existing[0].url, 0);
    let incoming = [IncomingWebhook {
        url: masked,
        post_rip: false,
        post_mux: false,
        post_move: true,
        headers: Default::default(),
    }];
    let resolved = resolve_webhook_entries(&incoming, &existing).unwrap();
    assert_eq!(
        resolved,
        vec![WebhookEntry {
            url: "https://discord.com/api/webhooks/1/secretA".into(),
            post_rip: false,
            post_mux: false,
            post_move: true,
            headers: Default::default(),
        }],
        "URL resolves to the stored secret but the flags follow the new request"
    );
}

#[test]
fn cross_origin_post_rejected_when_origin_host_differs() {
    // A browser on the LAN forging a POST carries an Origin header
    // whose host won't match our Host header → reject.
    assert!(is_cross_origin(
        Some("http://evil.example.com"),
        Some("autorip.test")
    ));
    // Referer fallback host mismatch is likewise rejected (the request
    // helper falls back to Referer when Origin is absent).
    assert!(is_cross_origin(
        Some("http://evil.example.com/page"),
        Some("autorip.test")
    ));
}

#[test]
fn cross_origin_post_allowed_when_origin_absent_or_same() {
    // curl / monitoring scripts send no Origin → allow.
    assert!(!is_cross_origin(None, Some("autorip.test")));
    // Empty Origin → allow.
    assert!(!is_cross_origin(Some(""), Some("autorip.test")));
    // Same host (scheme/path stripped, case-insensitive) → allow.
    assert!(!is_cross_origin(
        Some("http://autorip.test"),
        Some("autorip.test")
    ));
    assert!(!is_cross_origin(
        Some("http://Host.Test:8080/x"),
        Some("host.test:8080")
    ));
    // No Host header to compare against → can't prove cross-origin, allow.
    assert!(!is_cross_origin(Some("http://evil.example.com"), None));
}

#[test]
fn a_url_inside_the_referer_path_or_query_is_not_the_authority() {
    // Same-origin Referer whose query embeds another URL is allowed.
    assert!(!is_cross_origin(
        Some("http://nas:8080/library?next=http://example.com/x"),
        Some("nas:8080")
    ));
    // And a foreign Referer embedding our Host in its query is still rejected.
    assert!(is_cross_origin(
        Some("http://evil.example/p?x=http://nas:8080/"),
        Some("nas:8080")
    ));
}

#[test]
fn keydb_too_large_body_states_the_enforced_cap() {
    let body = keydb_too_large_body();
    assert!(body.contains("100 MiB"), "{body}");
    assert!(serde_json::from_str::<serde_json::Value>(&body).is_ok());
}

#[test]
fn log_bundle_names_files_it_could_not_carry() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut files = vec![("a.log".to_string(), tmp.path().join("a.log"))];
    std::fs::write(tmp.path().join("a.log"), "hello").unwrap();
    files.push(("gone.log".to_string(), tmp.path().join("gone.log")));
    let bytes = build_log_bundle(files).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut notes = String::new();
    std::io::Read::read_to_string(&mut zip.by_name(BUNDLE_NOTES).unwrap(), &mut notes).unwrap();
    assert!(notes.contains("gone.log"), "{notes}");
    assert!(!notes.contains("a.log"), "{notes}");
    assert!(zip.by_name("a.log").is_ok());
}

#[test]
fn log_bundle_is_capped_in_file_count_and_says_so() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut files = Vec::new();
    for i in 0..BUNDLE_MAX_FILES + 2 {
        let p = tmp.path().join(format!("f{i:03}.log"));
        std::fs::write(&p, "x").unwrap();
        files.push((format!("f{i:03}.log"), p));
    }
    let bytes = build_log_bundle(files).unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    assert_eq!(
        zip.len(),
        BUNDLE_MAX_FILES + 1,
        "files plus the notes entry"
    );
    let mut notes = String::new();
    std::io::Read::read_to_string(&mut zip.by_name(BUNDLE_NOTES).unwrap(), &mut notes).unwrap();
    assert!(
        notes.contains(&format!("f{:03}.log", BUNDLE_MAX_FILES)),
        "{notes}"
    );
}

#[test]
fn drives_sort_in_numeric_device_order() {
    let mut v = vec!["sg10", "sg2", "sg0", "sr1", "sg11"];
    v.sort_by_key(|n| natural_key(n));
    assert_eq!(v, ["sg0", "sg2", "sg10", "sg11", "sr1"]);
}

#[test]
fn a_failed_save_does_not_revert_a_newer_good_save() {
    let before = Config::default();
    let failed = Config {
        auto_eject: !before.auto_eject,
        ..Config::default()
    };
    let applied = serde_json::to_value(&failed).ok();
    // A newer save C landed after the failed save B installed its values.
    let newer = Config {
        auto_eject: failed.auto_eject,
        keep_iso: !failed.keep_iso,
        ..Config::default()
    };
    let cfg = Arc::new(RwLock::new(newer.clone()));
    roll_back_settings(&cfg, applied.as_ref(), &before);
    assert_eq!(cfg.read().unwrap().keep_iso, newer.keep_iso);
    // Still B's values: the rollback applies.
    *cfg.write().unwrap() = failed.clone();
    roll_back_settings(&cfg, applied.as_ref(), &before);
    assert_eq!(cfg.read().unwrap().auto_eject, before.auto_eject);
}

#[test]
fn last_lines_keeps_file_order() {
    assert_eq!(last_lines("a\nb\nc\nd", 2), "c\nd");
    assert_eq!(last_lines("a\nb", 5), "a\nb");
    assert_eq!(last_lines("", 5), "");
}

#[test]
fn cross_origin_default_port_normalization() {
    // Origin omits the default port; Host carries it explicitly. These
    // are the SAME origin and must NOT be rejected. (The pre-fix exact
    // string compare 403'd these.)
    assert!(!is_cross_origin(
        Some("http://autorip.test"),
        Some("autorip.test:80")
    ));
    assert!(!is_cross_origin(
        Some("https://autorip.test"),
        Some("autorip.test:443")
    ));
    // Inverse: Origin carries the default port, Host omits it.
    assert!(!is_cross_origin(
        Some("http://autorip.test:80"),
        Some("autorip.test")
    ));
    // IPv6 literal, default-port both sides.
    assert!(!is_cross_origin(Some("http://[::1]"), Some("[::1]:80")));
    // A genuinely different port is still cross-origin.
    assert!(is_cross_origin(
        Some("http://autorip.test:8080"),
        Some("autorip.test:9090")
    ));
    // https default (443) must not collapse onto http default (80):
    // an https Origin compared against a Host carrying :80 is a real
    // mismatch.
    assert!(is_cross_origin(
        Some("https://autorip.test"),
        Some("autorip.test:80")
    ));
}

/// Edge branches of the authority normaliser the higher-level cross-origin
/// tests don't reach directly: a malformed bracketed IPv6 literal, a
/// non-numeric port, and an Origin that normalises to nothing.
#[test]
fn normalize_authority_and_cross_origin_edge_branches() {
    use super::normalize_authority;
    // Malformed IPv6: a `]` followed by junk that is neither empty nor a
    // `:port` must be rejected outright (defensive `None`).
    assert_eq!(normalize_authority("[::1]junk", 80), None);
    // A trailing `:token` whose port is NOT numeric is treated as part of
    // the host, and the scheme's default port is appended instead of
    // silently dropping it.
    assert_eq!(
        normalize_authority("host:notaport", 80).as_deref(),
        Some("host:notaport:80")
    );
    // An Origin present but normalising to nothing (bare scheme, empty
    // authority) can't prove cross-origin, so the request is allowed.
    assert!(!is_cross_origin(Some("http://"), Some("autorip.test:80")));
}

// ── fetch-URL guard ────────────────────────────────────────────────

#[test]
fn validate_fetch_url_allows_lan_and_rejects_invalid_and_bad_scheme() {
    // Home app: loopback, RFC1918 and link-local literals (no DNS) are valid.
    for url in [
        "http://127.0.0.1/x".to_string(),
        "http://169.254.169.254/latest/meta-data/".to_string(),
        format!("http://{}.{}.{}.{}:8080/decode", 10, 0, 0, 5),
        format!("https://{}.{}.{}.{}/", 192, 168, 0, 1),
        "http://[::1]:9000/".to_string(),
    ] {
        assert!(validate_fetch_url(&url).is_ok(), "{url} must be accepted");
    }
    // Addresses that can never be reached are refused.
    for url in [
        "http://0.0.0.0/x",
        "http://224.0.0.1/x",
        "http://240.0.0.1/x",
        "http://[::]/x",
    ] {
        assert!(validate_fetch_url(url).is_err(), "{url} must be refused");
    }
    // Non-http schemes and junk.
    assert!(validate_fetch_url("ftp://example.com/x").is_err());
    assert!(validate_fetch_url("file:///etc/passwd").is_err());
    assert!(validate_fetch_url("not a url").is_err());
    assert!(validate_fetch_url("").is_err());
}

#[test]
fn guarded_get_rejects_invalid_address_and_scheme_before_connecting() {
    // guarded_get runs the address guard FIRST: an unreachable literal is refused with no
    // socket opened (LAN literals pass the guard, so they would connect).
    assert!(guarded_get("http://0.0.0.0/keydb.zip").is_err());
    assert!(guarded_get("http://[ff02::1]:9000/keydb.zip").is_err());
    assert!(guarded_get("file:///etc/passwd").is_err());
}

// A KEYDB body that is SLOW but PROGRESSING must finish. ureq 3.4.1 (#1194) made
// timeout_recv_body a TOTAL deadline that no longer re-arms; autorip restores the rolling
// idle bound itself.
#[test]
fn a_slow_but_progressing_keydb_body_is_not_killed_by_the_header_deadline() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    let listener =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");

    let server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept failed");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => head.push(byte[0]),
            }
        }
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 60\r\n\r\n");
        let _ = sock.flush();
        // Sixty bytes, 100 ms apart: ~6 s of body, with no single gap
        // anywhere near the idle bound. The TOTAL is what matters — see
        // the timeout comment below.
        for _ in 0..60 {
            if sock.write_all(b"k").is_err() {
                return;
            }
            let _ = sock.flush();
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });

    // 100ms per-gap vs a 3s idle bound (30x headroom, so a scheduling stall can't
    // spuriously trip it), and ~6s total body vs that 3s bound (2x, so a TOTAL
    // interpretation still fails).
    let idle = std::time::Duration::from_secs(3);
    let agent = guarded_agent_with_timeouts(
        vec![pinned],
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(30),
        idle,
    );
    let resp = agent
        .get("http://keydb-mirror.test/keydb.zip")
        .call()
        .expect("headers must arrive");
    let started = std::time::Instant::now();
    let mut body = Vec::new();
    let read = resp.into_body().into_reader().read_to_end(&mut body);
    let elapsed = started.elapsed();
    let _ = server.join();

    assert!(
        read.is_ok(),
        "a steadily-progressing body was aborted: {:?}",
        read.err()
    );
    assert_eq!(body, vec![b'k'; 60], "the whole body must arrive");
    // Relative-progress proof, robust to runner speed: the transfer outlasted
    // one idle window, so a TOTAL interpretation would have killed it — it did
    // not, therefore the bound rolled (a slow runner only grows `elapsed`).
    assert!(
        elapsed > idle,
        "body finished within a single idle window ({elapsed:?} <= {idle:?}); \
             the rolling-vs-total distinction is no longer exercised"
    );
}

// The other half: a peer sending headers then NOTHING must be cut off by the rolling idle
// bound, not held for the whole total budget — the protection ureq 2's timeout_read gave.
#[test]
fn a_stalled_body_is_cut_off_by_the_idle_bound_not_the_total_budget() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    let listener =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");

    let server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept failed");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => head.push(byte[0]),
            }
        }
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n");
        let _ = sock.flush();
        // Promise a megabyte and send none of it — hold the socket by
        // BLOCKING ON A READ, not sleeping, so the read returns the moment
        // the client drops the connection instead of outliving the test.
        let mut sink = [0u8; 1];
        let _ = sock.read(&mut sink);
    });

    let idle = std::time::Duration::from_secs(1);
    let agent = guarded_agent_with_timeouts(
        vec![pinned],
        std::time::Duration::from_secs(5),
        // A total budget far larger than the idle bound, so only the idle
        // bound can be what ends this.
        std::time::Duration::from_secs(120),
        idle,
    );
    let started = std::time::Instant::now();
    let resp = agent
        .get("http://keydb-mirror.test/keydb.zip")
        .call()
        .expect("headers must arrive");
    let mut body = Vec::new();
    let read = resp.into_body().into_reader().read_to_end(&mut body);
    let elapsed = started.elapsed();

    assert!(read.is_err(), "a stalled body must not read as success");
    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "a stalled peer was held for {elapsed:?} — the idle bound did not fire, \
             so the total budget is the only thing ending this"
    );
    // Joinable because the stub ends on client disconnect; `drop` on a
    // JoinHandle only detaches, it does not stop the thread.
    let _ = server.join();
}

// The real /api/update-keydb call path sets a request-level ceiling (timeout_global);
// a peer that sends headers then stalls must still be cut off by the idle bound.
#[test]
fn keydb_update_stalled_body_is_cut_off_by_idle_bound_under_a_global_ceiling() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;

    let listener =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");
    let server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("accept failed");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => head.push(byte[0]),
            }
        }
        let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n");
        let _ = sock.flush();
        let mut sink = [0u8; 1];
        let _ = sock.read(&mut sink);
    });

    // Same 3:1 ceiling:idle ratio as production (60s:20s), scaled down.
    let idle = std::time::Duration::from_secs(1);
    let ceiling = std::time::Duration::from_secs(8);
    let started = std::time::Instant::now();
    let resp = keydb_update_call(
        vec![pinned],
        "http://keydb-mirror.test/k.zip",
        ceiling,
        idle,
    )
    .expect("headers must arrive");
    let mut body = Vec::new();
    let read = resp.into_body().into_reader().read_to_end(&mut body);
    let elapsed = started.elapsed();
    let _ = server.join();

    assert!(read.is_err(), "a stalled body must not read as success");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "stalled keydb update held for {elapsed:?}: the global ceiling, not the idle bound, ended it"
    );
}

// The rejection tests above never connect, so they'd still pass if the
// agent silently ignored pinned addresses and re-resolved via live DNS.
// This pins a loopback listener and requests an unresolvable `.test` host.
#[test]
fn guarded_agent_connects_to_the_pinned_address_not_dns() {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc;

    let listener =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("bind stub listener");
    let pinned = listener.local_addr().expect("stub listener address");
    let (tx, rx) = mpsc::channel();

    let server = std::thread::spawn(move || {
        let (mut sock, _peer) = listener.accept().expect("stub listener accept failed");
        let _ = tx.send(());
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => head.push(byte[0]),
            }
        }
        let _ =
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi");
        let _ = sock.flush();
        head
    });

    let sent = guarded_agent_with_timeouts(
        vec![pinned],
        std::time::Duration::from_secs(5),
        std::time::Duration::from_secs(30),
        STALL_TIMEOUT,
    )
    .get("http://keydb-mirror.test/keydb.zip")
    .call();

    rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
        "guarded_agent never connected to the pinned address — the custom \
             resolver is not being consulted, so a DNS rebind between \
             validate_fetch_url and the fetch can still redirect the request",
    );
    let resp = sent.expect("the pinned round-trip must complete");
    assert_eq!(resp.status(), 200, "the stub server's reply must come back");
    let head = server.join().expect("stub server panicked");
    let head = String::from_utf8_lossy(&head);
    assert!(
        head.contains("keydb-mirror.test"),
        "the pinned agent must still address the original host; got: {head}"
    );
}

// Secret-leak guard: guarded_get error strings must never embed the full
// request URL (path/query may hold a token). Uses a public IP with
// nothing listening to reach ureq's Transport error arm, not the SSRF guard.
#[test]
fn guarded_get_ureq_error_does_not_embed_url() {
    // Port 1 on a public IP: passes SSRF guard (it's public) but the
    // connection will be refused immediately (nothing listens on port 1).
    // The URL has a fake token in the path that must not appear in the error.
    let token = "supersecret_api_token_12345";
    let url = format!("http://8.8.8.8:1/keydb/{token}.zip");
    let err = guarded_get(&url).unwrap_err();
    assert!(
        !err.contains(token),
        "ureq transport error must not leak the URL token; got: {err:?}"
    );
    // The error string must be our summary, not ureq's URL-bearing Display.
    assert!(
        err.starts_with("fetch failed:"),
        "error should be our summary; got: {err:?}"
    );
}

// is_transient_resolve_error must say NO to every permanent verdict
// validate_fetch_url can reach without a network round-trip — its
// consumer turns `true` into "the key service is down" and parks a disc.
#[test]
fn a_rejected_url_is_never_classified_as_a_failed_lookup() {
    for url in [
        "",
        "ftp://example.com/keys",
        "http://",
        // Literal, so no DNS is involved — the guard rejects the address.
        "http://0.0.0.0:8080/keys",
        "http://240.0.0.1/latest/meta-data",
    ] {
        let err =
            validate_fetch_url(url).expect_err("this URL must be rejected outright, not accepted");
        assert!(
            !is_transient_resolve_error(&err),
            "{url:?} is a permanent verdict on the URL, but its error \
                 {err:?} classifies as a failed lookup"
        );
    }
}

#[test]
fn validate_network_target_matches_library_rule() {
    // LAN, loopback, link-local, ULA and CGNAT are valid network:// peers.
    let ok = |t: &str| assert!(validate_network_target(t).is_ok(), "{t} must be accepted");
    ok("127.0.0.1:9000");
    ok(&format!("{}.{}.{}.{}:9000", 10, 0, 0, 5));
    ok(&format!("{}.{}.{}.{}:9000", 192, 168, 0, 1));
    ok("169.254.169.254:80");
    ok("100.64.0.1:9000");
    ok("[::1]:9000");
    ok("[fd12::1]:9000");
    // Only addresses that can never be a peer are refused.
    let bad = |t: &str| assert!(validate_network_target(t).is_err(), "{t} must be refused");
    bad("0.0.0.0:9000");
    bad("224.0.0.1:9000");
    bad("255.255.255.255:9000");
    bad("240.0.0.1:9000");
    bad("[::]:9000");
    bad("[ff02::1]:9000");
    // Malformed / missing port.
    bad("nas.example.com");
    bad("169.254.169.254");
    bad("");
}

#[test]
fn validate_network_target_accepts_public_literal() {
    // A public numeric host:port (no DNS needed) should validate.
    assert!(validate_network_target("8.8.8.8:9000").is_ok());
    assert!(validate_network_target("1.1.1.1:443").is_ok());
}

#[test]
fn resolve_with_timeout_resolves_literal() {
    // A numeric literal resolves without touching DNS and returns within
    // the deadline. Shared by validate_network_target + validate_fetch_url.
    let addrs = resolve_with_timeout("9.9.9.9", 853).expect("literal resolves");
    assert!(addrs.iter().any(|a| a.port() == 853 && a.ip().is_ipv4()));
}

#[test]
fn resolve_with_timeout_does_not_leak_inflight_slots() {
    // Regression for the unbounded-thread leak: the in-flight cap is 8.
    // A completed resolve must release its slot, so many sequential
    // resolves succeed — if slots leaked, the 9th+ call would fail fast.
    for _ in 0..40 {
        let addrs = resolve_with_timeout("9.9.9.9", 853).expect("literal resolves");
        assert!(addrs.iter().any(|a| a.port() == 853));
        // Let the detached resolver thread finish (dropping its ConnGuard)
        // before the next iteration so the slot is reliably released.
        std::thread::yield_now();
    }
}

#[test]
fn validate_fetch_url_accepts_public_literal() {
    // A public numeric host (no DNS needed) should validate and yield
    // the pinned address with the default port for the scheme.
    let addrs = validate_fetch_url("https://8.8.8.8/keydb.zip").expect("public IP allowed");
    assert!(addrs.iter().any(|a| a.port() == 443));
    let addrs = validate_fetch_url("http://1.1.1.1:8080/decode").expect("public IP allowed");
    assert!(addrs.iter().any(|a| a.port() == 8080));
}

// ── Connection cap ─────────────────────────────────────────────────

#[test]
fn conn_guard_releases_slot_when_holder_unwinds() {
    // Regression (resolve_with_timeout INFLIGHT leak): the DNS throttle's
    // ConnGuard must release its slot even when the holder unwinds rather
    // than returns — exactly what happens if `thread::spawn` panics.
    static C: AtomicUsize = AtomicUsize::new(0);
    let g0 = ConnGuard::try_acquire(&C, 4).expect("first slot");
    assert_eq!(C.load(Ordering::SeqCst), 1);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = ConnGuard::try_acquire(&C, 4).expect("second slot");
        assert_eq!(C.load(Ordering::SeqCst), 2);
        panic!("simulate thread::spawn unwinding with the guard held");
    }));
    assert!(r.is_err(), "the inner closure must have panicked");
    // The unwound guard's Drop must have released its slot; only g0 remains.
    assert_eq!(
        C.load(Ordering::SeqCst),
        1,
        "guard slot leaked across an unwind"
    );
    drop(g0);
    assert_eq!(C.load(Ordering::SeqCst), 0);
}

// ── A stalled POST must not starve the healthcheck (tiny_http 0.12 has
// no socket read timeout). Fill the body cap, then confirm a bodyless
// request still gets admitted while a body-carrying one does not.
#[test]
fn a_bodyless_request_is_admitted_while_the_body_cap_is_full() {
    static C: AtomicUsize = AtomicUsize::new(0);
    let held: Vec<_> = (0..MAX_INFLIGHT_BODY_HANDLERS)
        .map(|_| {
            ConnGuard::try_acquire(&C, MAX_INFLIGHT_BODY_HANDLERS).expect("under the body cap")
        })
        .collect();
    assert!(
        ConnGuard::try_acquire(&C, MAX_INFLIGHT_BODY_HANDLERS).is_none(),
        "the body cap must actually stop admitting"
    );
    let spare = ConnGuard::try_acquire(&C, MAX_INFLIGHT_HANDLERS);
    assert!(
        spare.is_some(),
        "a bodyless request must still be admitted — this is the slot the \
             healthcheck lives in"
    );
    drop(spare);
    drop(held);
    assert_eq!(C.load(Ordering::SeqCst), 0, "every slot released");
}

// ── Two guards nothing exercised: the pin caps at ureq's fixed 16-slot
// array. A 17th address is an out-of-bounds panic in a resolver that
// runs on every request; validate_fetch_url applies no count limit.
#[test]
fn the_pinned_address_list_cannot_overrun_ureqs_fixed_array() {
    let many: Vec<SocketAddr> = (0..MAX_PINNED_ADDRS + 1)
        .map(|i| SocketAddr::from(([203, 0, 113, 1], 1000 + i as u16)))
        .collect();
    assert_eq!(
        pinned_addrs(&many).len(),
        MAX_PINNED_ADDRS,
        "more addresses than ureq's array holds must be truncated"
    );
    // An ordinary answer is passed through untouched.
    let few: Vec<SocketAddr> = many.iter().copied().take(3).collect();
    assert_eq!(pinned_addrs(&few), few);
}

/// An EMPTY pin must not resolve to anything: the resolver turns that into
/// `HostNotFound` rather than letting the agent fall back to live DNS,
/// which would reopen the rebinding hole the pin exists to close.
#[test]
fn an_empty_pin_yields_no_addresses() {
    assert!(pinned_addrs(&[]).is_empty());
}

// ureq_error_kind is the single chokepoint keeping token-bearing URLs out
// of log sites reaching unauthenticated /api/debug + /api/system. Covers
// every variant, including the non_exhaustive catch-all where BadUri lands.
#[test]
fn no_ureq_error_kind_output_can_carry_a_url() {
    let cases = vec![
        ureq::Error::StatusCode(404),
        ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
        ureq::Error::HostNotFound,
        ureq::Error::ConnectionFailed,
        ureq::Error::TooManyRedirects,
        // The variant the catch-all exists for: its Display embeds the URI.
        ureq::Error::BadUri("https://keydb.example/t/SECRETTOKEN/keydb.zip".into()),
    ];
    for e in &cases {
        let kind = ureq_error_kind(e);
        assert!(
            !kind.contains("://") && !kind.contains("SECRETTOKEN"),
            "a URL reached the summary for {e:?}: {kind}"
        );
    }
    assert_eq!(ureq_error_kind(&ureq::Error::StatusCode(404)), "HTTP 404");
}

// An OS-generated transport error (real errno, e.g. ECONNRESET) must
// surface its descriptive syscall message, not collapse to the useless
// "io: uncategorized error" io.kind() alone prints for Uncategorized.
#[test]
fn ureq_error_kind_surfaces_os_error_detail() {
    // ECONNRESET (54 on macOS/BSD, 104 on Linux) can arrive as the
    // unhelpful Uncategorized kind; build via from_raw_os_error so
    // raw_os_error() is Some and the descriptive Display is used.
    let econnreset = if cfg!(target_os = "linux") { 104 } else { 54 };
    let e = ureq::Error::Io(std::io::Error::from_raw_os_error(econnreset));
    let summary = ureq_error_kind(&e);
    assert!(
        summary.contains(&format!("os error {econnreset}")),
        "the errno detail must be surfaced, got: {summary}"
    );
    // Still URL-free — the whole point of routing through this function.
    assert!(!summary.contains("://"));

    // An io error WITHOUT an errno (constructed from a bare ErrorKind, as
    // ureq/std synthesize) falls back to the fixed kind description.
    let synthetic = ureq::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
    assert_eq!(ureq_error_kind(&synthetic), "io: connection refused");
}

// The fourth ureq log site, missed by round 1: it masked the KEYDB origin
// from the client but logged the raw (token-bearing) error anyway,
// reaching autorip.jsonl and unauthenticated GET /api/debug.
#[test]
fn the_keydb_update_handler_masks_its_ureq_error() {
    let src = crate::server::util::source_lf(include_str!("web.rs"));
    // Anchored on the DEFINITION, not the name (this test mentions it
    // too). Both ends are `expect`ed — an anchor that stops matching
    // would otherwise silently widen the slice to other handlers' logs.
    let start = src
        .find("\nfn handle_update_keydb(request: tiny_http::Request")
        .expect("handle_update_keydb definition present");
    let end = start
        + src[start..]
            .find("\n    // Write to the service-canonical keydb path")
            .expect("the handler's post-fetch section still starts here");
    let body = &src[start..end];
    assert!(
        body.contains("ureq_error_kind(&e)"),
        "the keydb-update handler must summarise its ureq failure through \
             ureq_error_kind, which is URL-free"
    );
    assert!(
        !body.contains("error = %e"),
        "the keydb-update handler must not format its ureq error by Display"
    );
}

// Catches the mutation restoring get_state_json's Err(_) => return "{}" bail-out on a
// poisoned STATE; source-pinned since poisoning a real Mutex would break every other
// STATE-locking test.
#[test]
fn get_state_json_recovers_a_poisoned_state_lock() {
    let src = crate::server::util::source_lf(include_str!("web.rs"));
    // Anchored on the DEFINITION (leading newline) so this test's own
    // mention of the name cannot match, and both ends are `expect`ed so a
    // stale anchor fails loudly instead of silently widening the slice.
    let start = src
        .find("\nfn get_state_json(staging_dir: &str) -> String {")
        .expect("web.rs must define get_state_json");
    let end = start
        + src[start..]
            .find("\n    let move_state =")
            .expect("get_state_json still binds move_state after the STATE lock");
    // Comment lines are stripped: this function's own comment quotes the
    // defective arm verbatim (house style), so a naive substring search
    // would otherwise match the explanation of the fix, not the fix.
    let body: String = src[start..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains("STATE.lock().unwrap_or_else(|e| e.into_inner())"),
        "get_state_json must recover a poisoned STATE guard, like every \
             other STATE consumer in the crate"
    );
    assert!(
        !body.contains("Err(_) =>"),
        "get_state_json must not have an abandon-on-poison arm: it turns \
             one panic into a permanently blank dashboard served with 200, \
             which run_healthcheck reads as healthy forever"
    );
}

#[test]
fn resolve_with_timeout_uses_raii_guard_not_closure_side_fetch_sub() {
    // Source-pin for the INFLIGHT-leak fix: the decrement must NOT be a
    // bare `INFLIGHT.fetch_sub(...)` in the resolver closure (leaked on
    // panic) — it must flow through a ConnGuard whose Drop always runs.
    let src = crate::server::util::source_lf(include_str!("web.rs"));
    let start = src
        .find("pub(crate) fn resolve_with_timeout")
        .expect("resolve_with_timeout present");
    let body = &src[start..start + 1500];
    assert!(
        body.contains("ConnGuard::try_acquire(&INFLIGHT, MAX_INFLIGHT)"),
        "resolve_with_timeout must acquire its slot via ConnGuard"
    );
    assert!(
        !body.contains("INFLIGHT.fetch_sub"),
        "resolve_with_timeout must not decrement INFLIGHT by hand; the \
             ConnGuard Drop owns the release"
    );
}

#[test]
fn conn_guard_enforces_cap_and_releases_on_drop() {
    static C: AtomicUsize = AtomicUsize::new(0);
    let g1 = ConnGuard::try_acquire(&C, 2);
    let g2 = ConnGuard::try_acquire(&C, 2);
    assert!(g1.is_some());
    assert!(g2.is_some());
    assert_eq!(C.load(Ordering::SeqCst), 2);
    // Third over the cap is rejected.
    assert!(ConnGuard::try_acquire(&C, 2).is_none());
    // Dropping one frees a slot so the next acquire succeeds.
    drop(g1);
    assert_eq!(C.load(Ordering::SeqCst), 1);
    let g3 = ConnGuard::try_acquire(&C, 2);
    assert!(g3.is_some());
    drop(g2);
    drop(g3);
    assert_eq!(C.load(Ordering::SeqCst), 0);
}

// ── percent_decode trailing %XX ────────────────────────────────────

#[test]
fn percent_decode_handles_trailing_encoded_byte() {
    // A value ending in a percent-encoded byte must decode (the old
    // off-by-one dropped it through as literal text).
    assert_eq!(percent_decode("a%20b"), "a b");
    assert_eq!(percent_decode("end%20"), "end ");
    // A bare trailing '%' or incomplete '%X' stays literal (no panic).
    assert_eq!(percent_decode("100%"), "100%");
    assert_eq!(percent_decode("50%2"), "50%2");
}

// media_type was the one title-override field neither clamped nor
// allow-listed on an unauthenticated route whose value is persisted and
// re-broadcast; the router only ever acts on "tv" vs everything else.
#[test]
fn media_type_is_reduced_to_the_routers_vocabulary() {
    assert_eq!(normalize_media_type("tv"), "tv");
    assert_eq!(normalize_media_type("movie"), "movie");
    // Case/whitespace noise still resolves to the real values.
    assert_eq!(normalize_media_type(" TV "), "tv");
    // Anything else routes as a movie today, so it is stored as one
    // instead of being persisted verbatim.
    assert_eq!(normalize_media_type("anime"), "movie");
    assert_eq!(normalize_media_type(""), "movie");
    assert_eq!(
        normalize_media_type(&"A".repeat(10_000)),
        "movie",
        "an unbounded caller-supplied string must never reach STATE, the \
             .done marker, or the dashboard broadcast"
    );
}

// Only ASCII hex digits are percent-escape payloads: from_str_radix
// accepts a leading sign, so %+3 used to parse as 3 (a control byte)
// instead of staying literal. RFC 3986 admits HEXDIG only.
#[test]
fn percent_decode_rejects_non_hex_escape_payloads() {
    assert_eq!(
        percent_decode("a%+3b"),
        "a%+3b",
        "'+' is not a hex digit — `%+3` must stay literal, not decode to 0x03"
    );
    assert_eq!(
        percent_decode("%-1"),
        "%-1",
        "a leading '-' must not be accepted as a sign either"
    );
    // Whitespace is the other thing from_str_radix-adjacent parsers trip
    // on; and the valid cases must keep working, upper and lower case.
    assert_eq!(percent_decode("%2f"), "/");
    assert_eq!(percent_decode("%2F"), "/");
}

// ── Real HTTP integration: drive handle_request via a live server —
// tiny_http::Request has no public constructor, so these tests bind a
// loopback server and hand a received real Request to production code.
mod http {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    // One real request/response round-trip through handle_request: binds
    // an ephemeral loopback server, spawns a client that writes the raw
    // request, and dispatches through production code. Returns (status, body).
    fn roundtrip(
        cfg: &Arc<RwLock<Config>>,
        method: &str,
        path: &str,
        body: Option<&str>,
        extra_headers: &[(&str, &str)],
    ) -> (u16, String) {
        let server = Server::http("127.0.0.1:0").expect("bind loopback server");
        let addr = server.server_addr().to_ip().expect("ip addr");

        let method = method.to_string();
        let path = path.to_string();
        let body = body.map(|b| b.to_string());
        let extra: Vec<(String, String)> = extra_headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("connect");
            let body = body.unwrap_or_default();
            let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n");
            for (k, v) in &extra {
                req.push_str(&format!("{k}: {v}\r\n"));
            }
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
            req.push_str("Connection: close\r\n\r\n");
            req.push_str(&body);
            stream.write_all(req.as_bytes()).expect("write request");
            stream.flush().ok();
            let mut resp = Vec::new();
            stream.read_to_end(&mut resp).expect("read response");
            String::from_utf8_lossy(&resp).to_string()
        });

        // Accept exactly one request and dispatch it through production.
        let request = server.recv().expect("recv request");
        handle_request(request, cfg);

        let raw = client.join().expect("client thread");
        parse_response(&raw)
    }

    // Classify ONE real tiny_http::Request, sent verbatim (roundtrip
    // always adds a Content-Length; the arm that matters here has none),
    // via production carries_body. recv_timeout(5s) fails, not hangs.
    fn carries_body_of(head: &str) -> bool {
        let server = Server::http("127.0.0.1:0").expect("bind loopback server");
        let addr = server.server_addr().to_ip().expect("ip addr");
        let head = head.to_string();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).expect("connect");
            stream.write_all(head.as_bytes()).expect("write request");
            stream.flush().ok();
            let mut resp = Vec::new();
            let _ = stream.read_to_end(&mut resp);
        });
        let request = server
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("server error")
            .expect("the request must arrive within 5s");
        let carries = carries_body(&request);
        let _ = request.respond(tiny_http::Response::empty(204));
        client.join().expect("client thread");
        carries
    }

    // The accept loop reserves a smaller cap for body-carrying requests,
    // decided ONLY by carries_body — the sibling cap test never called
    // it, so an inverted arm there stayed green. Drive with real requests.
    #[test]
    fn carries_body_classifies_real_requests() {
        // No Content-Length, bodyless method — the healthcheck's own
        // shape. This is the slot that must stay available when the body
        // cap is full.
        assert!(
            !carries_body_of("GET /api/state HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
            "a GET with no Content-Length carries no body"
        );
        // Explicit zero length: a body header, but nothing to read.
        assert!(
            !carries_body_of(
                "POST /api/settings HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\
                     Connection: close\r\n\r\n"
            ),
            "Content-Length: 0 is not a body"
        );
        // A real body.
        assert!(
            carries_body_of(
                "POST /api/settings HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\
                     Connection: close\r\n\r\nhello"
            ),
            "a non-zero Content-Length carries a body"
        );
        // No Content-Length on a method that may carry one: assume the
        // reader will wait, and charge it the body cap.
        assert!(
            carries_body_of("POST /api/settings HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"),
            "a POST with no Content-Length must be charged the body cap"
        );
    }

    /// Extract the status code and body from a raw HTTP/1.1 response.
    fn parse_response(raw: &str) -> (u16, String) {
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
        let status_line = head.lines().next().unwrap_or_default();
        // "HTTP/1.1 200 OK"
        let code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or(0);
        (code, body.to_string())
    }

    /// A Config whose autorip_dir points at a writable tempdir so
    /// `config::save` (invoked by handle_settings_post) succeeds and we can
    /// read back the persisted settings.json.
    fn cfg_in_tempdir(dir: &std::path::Path) -> Arc<RwLock<Config>> {
        let c = Config {
            autorip_dir: dir.to_string_lossy().to_string(),
            staging_dir: dir.join("staging").to_string_lossy().to_string(),
            output_dir: dir.join("output").to_string_lossy().to_string(),
            ..Config::default()
        };
        Arc::new(RwLock::new(c))
    }

    // ── Route dispatch + method gating ──────────────────────────────

    #[test]
    fn library_routes_list_queue_and_gate() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg_in_tempdir(dir.path());
        let (lib, isos) = (dir.path().join("movies"), dir.path().join("isos"));
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::create_dir_all(&isos).unwrap();
        std::fs::write(isos.join("New (2020).iso"), b"iso").unwrap();
        {
            let mut c = cfg.write().unwrap();
            c.library_dir = lib.to_string_lossy().into_owned();
            c.library_iso_dir = isos.to_string_lossy().into_owned();
        }
        // Two MKVs with ISOs: one muxed by an older freemkv, one by mkvmerge.
        use crate::server::library::probe::testmkv::mkv;
        for (t, app) in [
            ("Old (2001)", "freemkv 1.6.11 (g1)"),
            ("Merged (2002)", "mkvmerge v96.0 ('x') 64-bit"),
        ] {
            std::fs::create_dir_all(lib.join(t)).unwrap();
            std::fs::write(
                lib.join(t).join(format!("{t}.mkv")),
                mkv(app, Some(60.0), Some(58), true),
            )
            .unwrap();
            std::fs::write(isos.join(format!("{t}.iso")), b"iso").unwrap();
        }
        // Before the first scan the answer is immediate and says so.
        let (code, body) = roundtrip(&cfg, "GET", "/api/library", None, &[]);
        assert_eq!(code, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["scanning"], true);
        let (code, body) = roundtrip(&cfg, "POST", "/api/library/queue/out-of-date", None, &[]);
        assert_eq!(code, 409, "no silent zero before the scan: {body}");

        let c = cfg.read().unwrap().clone();
        let library = crate::server::library::instance(&c);
        // Another test's settings save may wake the shared indexer mid-pass.
        let d = crate::server::library::dirs(&c);
        assert!((0..20).any(|_| library.index_now(&d)));
        // Indexed: the answer comes from memory, even with the folders gone.
        let moved = dir.path().join("moved-away");
        std::fs::rename(&lib, &moved).unwrap();
        let t = std::time::Instant::now();
        let (code, body) = roundtrip(&cfg, "GET", "/api/library", None, &[]);
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
        std::fs::rename(&moved, &lib).unwrap();
        assert_eq!(code, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["rows"].as_array().unwrap().len(), 3, "{body}");
        let row = |t: &str| {
            v["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["title"] == t)
                .unwrap()
                .clone()
        };
        assert_eq!(row("Merged (2002)")["muxed_label"], "mkvmerge 96.0");
        assert_eq!(row("Old (2001)")["muxed_label"], "freemkv 1.6.11");
        assert_eq!(row("Old (2001)")["needs_remux"], true);
        let v = serde_json::json!({"rows": [row("New (2020)")]});
        assert_eq!(v["rows"][0]["kind"], "iso_only");
        let target = v["rows"][0]["target"].as_str().unwrap().to_string();

        let add = |t: &str| {
            let b = serde_json::json!({ "target": t }).to_string();
            roundtrip(&cfg, "POST", "/api/library/queue/add", Some(&b), &[])
        };
        assert!(
            add("/etc/passwd").1.contains("\"queued\":0"),
            "only listed rows"
        );
        assert!(add(&target).1.contains("\"queued\":1"));
        assert!(add(&target).1.contains("\"queued\":0"), "already queued");
        let (_, body) = roundtrip(&cfg, "POST", "/api/library/queue/out-of-date", None, &[]);
        let q: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (q["queued"].as_u64(), q["eligible"].as_u64()),
            (Some(2), Some(3)),
            "{body}"
        );
        let (_, body) = roundtrip(&cfg, "POST", "/api/library/queue/clear-queued", None, &[]);
        assert!(body.contains("\"removed\":3"), "{body}");
        // With the library folder emptied (an unmounted share looks like
        // this), creating a new MKV is refused, one title or many.
        let away = dir.path().join("away");
        std::fs::rename(&lib, &away).unwrap();
        std::fs::create_dir(&lib).unwrap();
        assert!((0..20).any(|_| library.index_now(&d)));
        let (code, body) = add(&target);
        assert_eq!(code, 409, "{body}");
        assert!(body.contains("is empty"), "{body}");
        let (code, _) = roundtrip(&cfg, "POST", "/api/library/queue/all", None, &[]);
        assert_eq!(code, 409);
        std::fs::remove_dir(&lib).unwrap();
        std::fs::rename(&away, &lib).unwrap();
        let (code, body) = roundtrip(&cfg, "GET", "/api/library?download=1", None, &[]);
        assert_eq!(code, 200);
        assert!(
            body.contains("\"rows\""),
            "the download is the same listing"
        );
        let (_, body) = roundtrip(&cfg, "POST", "/api/library/queue/pause", None, &[]);
        assert!(body.contains("\"paused\":true"), "{body}");
        let (code, body) = roundtrip(&cfg, "POST", "/api/library/queue/stop-all", None, &[]);
        assert_eq!(code, 200, "{body}");
        assert!(
            body.contains("\"paused\":false"),
            "stop-all must leave remux unpaused: {body}"
        );
        let (code, _) = roundtrip(&cfg, "GET", "/api/library/console", None, &[]);
        assert_eq!(code, 200);
        let (code, body) = roundtrip(&cfg, "GET", "/api/library/folders", None, &[]);
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["folders"][0]["role"], "output", "{body}");
        assert!(v.get("hold").is_some(), "{body}");
        for action in ["retry", "discard"] {
            let url = format!("/api/library/staged/{action}");
            let (code, _) = roundtrip(&cfg, "POST", &url, Some(r#"{"target":"/x.mkv"}"#), &[]);
            assert_eq!(code, 404, "{action}: no kept file for that title");
            let (code, _) = roundtrip(&cfg, "POST", &url, Some("{}"), &[]);
            assert_eq!(code, 400, "{action}: a target is required");
        }
        let (code, body) = roundtrip(&cfg, "POST", "/api/library/staged/clear", None, &[]);
        assert_eq!(code, 200, "{body}");
        let cleared: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(cleared["discarded"], 0);
        assert_eq!(cleared["failed"], 0);
        let (code, _) = roundtrip(
            &cfg,
            "GET",
            "/api/library/log?title=New%20(2020)",
            None,
            &[],
        );
        assert_eq!(code, 200);
        let (code, _) = roundtrip(&cfg, "GET", "/api/library/log", None, &[]);
        assert_eq!(code, 400);
        let (code, _) = roundtrip(&cfg, "GET", "/api/library/nope", None, &[]);
        assert_eq!(code, 404);
        let (code, _) = roundtrip(&cfg, "GET", "/api/library/queue/all", None, &[]);
        assert_eq!(code, 404, "queue actions are POST only");
        let bad = r#"{"library_dir": "relative/path"}"#;
        let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(bad), &[]);
        assert_eq!(code, 400);
    }

    #[test]
    fn get_version_dispatches_and_returns_running_version() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (code, body) = roundtrip(&cfg, "GET", "/api/version", None, &[]);
        assert_eq!(code, 200);
        assert!(
            body.contains(&format!("\"version\":\"{}\"", crate::server::VERSION_LABEL)),
            "GET /api/version must serve the running version, got: {body}"
        );
    }

    #[test]
    fn unknown_route_returns_404() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (code, body) = roundtrip(&cfg, "GET", "/api/nope", None, &[]);
        assert_eq!(code, 404, "an unknown route must 404");
        assert!(body.contains("not found"));
    }

    #[test]
    fn settings_route_gates_on_method() {
        // GET /api/settings serves redacted settings; a DELETE to the same
        // path falls through to 404 (method-gated, not matched).
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (get_code, _) = roundtrip(&cfg, "GET", "/api/settings", None, &[]);
        assert_eq!(get_code, 200, "GET /api/settings must be served");
        let (del_code, _) = roundtrip(&cfg, "DELETE", "/api/settings", None, &[]);
        assert_eq!(del_code, 404, "DELETE /api/settings must not match");
    }

    #[test]
    fn sse_route_is_served_at_events_not_api_sse() {
        // Pin the ACTUAL served route: production serves /events, and
        // /api/sse is NOT a route and must 404. /events streams forever,
        // so it isn't tested here beyond the fact that it doesn't 404.
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (api_sse_code, _) = roundtrip(&cfg, "GET", "/api/sse", None, &[]);
        assert_eq!(
            api_sse_code, 404,
            "/api/sse is not a real route — production serves /events"
        );
    }

    // ── Device-name validation in dispatch ──────────────────────────

    #[test]
    fn rip_route_rejects_invalid_device_name() {
        // A path-traversal device name must be rejected by the dispatch
        // guard (is_valid_device_name) with 400 — never reaching handle_rip.
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (code, body) = roundtrip(&cfg, "POST", "/api/rip/..%2F..%2Fetc", None, &[]);
        assert_eq!(code, 400, "traversal device name must be rejected");
        assert!(body.contains("invalid device name"));
    }

    // A shape-VALID but nonexistent device name must not create state:
    // looping unauthenticated POST /api/scan/<random> used to grow the
    // STATE/JoinHandle maps without bound until OOM-killed.
    #[test]
    fn unauthenticated_scan_of_a_nonexistent_device_does_not_grow_state() {
        // Assert per-device, never on total STATE size: STATE is a
        // process-global, so a parallel sibling test would fail this
        // spuriously — exactly what comparing total length once did.
        let cfg = Arc::new(RwLock::new(Config::default()));
        let devices: Vec<String> = (0..25).map(|i| format!("zznotadrive{i:03}")).collect();
        for dev in &devices {
            let (code, _) = roundtrip(&cfg, "POST", &format!("/api/scan/{dev}"), None, &[]);
            assert_ne!(
                code, 200,
                "a nonexistent device must not be accepted for scanning"
            );
            assert!(
                !crate::server::ripper::device_known(dev),
                "no STATE entry may be created for unknown device {dev}"
            );
        }
        // Re-check every one after the whole loop: a handler that deferred
        // the insert (spawning a worker that registers later) would pass
        // the per-iteration check above and still leak all 25.
        let leaked: Vec<&String> = devices
            .iter()
            .filter(|d| crate::server::ripper::device_known(d))
            .collect();
        assert!(
            leaked.is_empty(),
            "fabricated devices left in STATE: {leaked:?}"
        );
    }

    // Unknown (never-enumerated) drives get 404 on every state-creating route, not 409 "busy".
    #[test]
    fn state_creating_routes_404_an_unknown_device() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        for route in ["scan", "rip", "eject"] {
            let dev = format!("zzunknown{route}");
            let (code, body) = roundtrip(&cfg, "POST", &format!("/api/{route}/{dev}"), None, &[]);
            assert_eq!(code, 404, "/api/{route} on an unknown device: {body}");
            assert!(!crate::server::ripper::device_known(&dev));
        }
    }

    #[test]
    fn tmdb_search_without_a_key_is_an_error_not_an_empty_result() {
        let cfg = Arc::new(RwLock::new(Config {
            tmdb_api_key: String::new(),
            ..Config::default()
        }));
        let (code, body) = roundtrip(&cfg, "GET", "/api/tmdb/search?q=Dune", None, &[]);
        assert_eq!(code, 400, "{body}");
        assert!(body.contains("TMDB API key"), "{body}");
    }

    #[test]
    fn stop_route_on_a_known_idle_device_answers_ok_and_publishes_idle() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let device = "sgstopknown5";
        ripper::update_state(
            device,
            ripper::RipState {
                device: device.to_string(),
                status: "ripping".to_string(),
                disc_present: true,
                last_error: "old".to_string(),
                ..Default::default()
            },
        );
        let (code, body) = roundtrip(&cfg, "POST", &format!("/api/stop/{device}"), None, &[]);
        assert_eq!(code, 200, "{body}");
        assert!(body.contains(r#""ok":true"#), "{body}");
        {
            let s = ripper::STATE.lock().unwrap();
            assert_eq!(s[device].status, "idle");
            assert!(s[device].last_error.is_empty());
            assert!(s[device].disc_present, "the disc is still in the drive");
        }
        ripper::STATE.lock().unwrap().remove(device);
    }

    #[test]
    fn debug_toggle_defaults_off_for_a_malformed_body() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        for body in ["", "not json", r#"{"enabled":"yes"}"#, r#"{}"#] {
            let (code, resp) = roundtrip(&cfg, "POST", "/api/debug", Some(body), &[]);
            assert_eq!(code, 200, "{resp}");
            assert!(resp.contains(r#""enabled":false"#), "{body:?} -> {resp}");
            assert!(!debug_enabled(), "{body:?} must not enable debug logging");
        }
        let (_, resp) = roundtrip(&cfg, "POST", "/api/debug", Some(r#"{"enabled":true}"#), &[]);
        assert!(resp.contains(r#""enabled":true"#), "{resp}");
        roundtrip(
            &cfg,
            "POST",
            "/api/debug",
            Some(r#"{"enabled":false}"#),
            &[],
        );
        assert!(!debug_enabled());
    }

    #[test]
    fn update_keydb_maps_each_failure_to_its_status() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let (code, _) = roundtrip(&cfg, "POST", "/api/update-keydb", None, &[]);
        assert_eq!(code, 400, "no URL configured");

        cfg.write().unwrap().keydb_url = "ftp://example.com/k".to_string();
        let (code, body) = roundtrip(&cfg, "POST", "/api/update-keydb", None, &[]);
        assert_eq!(code, 400, "{body}");
        assert!(body.contains("KEYDB URL rejected"), "{body}");

        // A reachable server answering 404 is an upstream failure.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let upstream = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
        cfg.write().unwrap().keydb_url = format!("http://127.0.0.1:{port}/k");
        let (code, body) = roundtrip(&cfg, "POST", "/api/update-keydb", None, &[]);
        upstream.join().unwrap();
        assert_eq!(code, 502, "{body}");
        assert!(body.contains("HTTP 404"), "{body}");
    }

    #[test]
    fn an_evicted_event_stream_ends_while_its_client_stays_connected() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let server = Server::http("127.0.0.1:0").expect("bind loopback server");
        let addr = server.server_addr().to_ip().expect("ip addr");
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let request = server.recv().expect("recv request");
            handle_sse(request, &cfg);
            let _ = done_tx.send(());
        });
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        stream
            .write_all(b"GET /events HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let _ = read_first_sse_frame(&mut stream);
        // Newer streams push the oldest (this one) out.
        let newer: Vec<_> = (0..MAX_SSE_CLIENTS).map(|_| sse_admit()).collect();
        let ended = done_rx.recv_timeout(std::time::Duration::from_secs(5));
        for (id, _) in newer {
            sse_leave(id);
        }
        assert!(ended.is_ok(), "an evicted stream must stop on its own");
        drop(stream);
    }

    #[test]
    fn stop_route_rejects_invalid_device_name() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (code, _) = roundtrip(&cfg, "POST", "/api/stop/x", None, &[]);
        // "x" is too short (is_valid_device_name requires len 3..=64) -> 400.
        assert_eq!(code, 400, "a 1-char device name must be rejected");
    }

    // ── CSRF gate on POST ───────────────────────────────────────────

    #[test]
    fn cross_origin_post_is_rejected_403() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some("{}"),
            &[("Origin", "http://evil.example.com")],
        );
        assert_eq!(code, 403, "a cross-origin POST must be rejected");
        assert!(body.contains("cross-origin"));
    }

    // ── read_json_body size limit (via handle_settings_post) ────────

    #[test]
    fn oversize_request_body_is_rejected_413() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        // One byte over MAX_REQUEST_BODY (1 MiB).
        let big = "x".repeat((MAX_REQUEST_BODY as usize) + 1);
        let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(&big), &[]);
        assert_eq!(code, 413, "a body over MAX_REQUEST_BODY must be 413");
    }

    #[test]
    fn exact_cap_request_body_is_accepted_not_413() {
        // Exactly MAX_REQUEST_BODY bytes is in-spec, distinguishing
        // read_body_capped's `>` from a `>=` mutant. Pad valid JSON with
        // leading whitespace (serde_json skips it) out to the cap.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let payload = r#"{"abort_on_lost_secs": 31}"#;
        let padding = " ".repeat((MAX_REQUEST_BODY as usize) - payload.len());
        let body = format!("{padding}{payload}");
        assert_eq!(body.len() as u64, MAX_REQUEST_BODY);
        let (code, resp) = roundtrip(&cfg, "POST", "/api/settings", Some(&body), &[]);
        assert_ne!(
            code, 413,
            "a body of exactly MAX_REQUEST_BODY bytes must not be rejected as too large"
        );
        assert_eq!(
            code, 200,
            "the exact-cap body is valid JSON and must succeed, got: {resp}"
        );
        assert_eq!(cfg.read().unwrap().abort_on_lost_secs, 31);
    }

    #[test]
    fn malformed_json_body_is_rejected_400() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let (code, body) = roundtrip(&cfg, "POST", "/api/settings", Some("{not json"), &[]);
        assert_eq!(code, 400);
        assert!(body.contains("invalid json"));
    }

    // ── handle_settings_post: the real save + the sentinel guard ────

    #[test]
    fn settings_post_persists_a_field_to_disk() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"abort_on_lost_secs": 30}"#),
            &[],
        );
        assert_eq!(code, 200, "a valid settings POST must succeed, got: {body}");
        assert!(body.contains("\"ok\":true"));
        // The in-memory config was mutated...
        assert_eq!(cfg.read().unwrap().abort_on_lost_secs, 30);
        // ...and persisted to settings.json on disk.
        let saved = std::fs::read_to_string(cfg.read().unwrap().settings_file())
            .expect("settings.json must be written");
        assert!(
            saved.contains("\"abort_on_lost_secs\""),
            "the persisted settings.json must carry the field"
        );
    }

    // `?since=` answers JSON with a sequence to continue from.
    #[test]
    fn device_log_since_answers_json() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        crate::server::log::device_log("sincetest", "hello");
        let (code, body) = roundtrip(&cfg, "GET", "/api/logs/sincetest?since=0", None, &[]);
        assert_eq!(code, 200, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["seq"].as_u64().unwrap() >= 1);
        assert!(v["lines"][0][1].as_str().unwrap().contains("hello"));
        let seq = v["seq"].as_u64().unwrap();
        let (_, body) = roundtrip(
            &cfg,
            "GET",
            &format!("/api/logs/sincetest?since={seq}"),
            None,
            &[],
        );
        assert!(body.contains("\"lines\":[]"), "{body}");
        let (code, _) = roundtrip(&cfg, "GET", "/api/logs/sincetest", None, &[]);
        assert_eq!(code, 200, "the plain tail still works");
        let (code, body) = roundtrip(&cfg, "POST", "/api/system/keyserver-test", None, &[]);
        assert_eq!(code, 400, "{body}");
    }

    // A save whose file write fails must leave the running config as it was.
    #[test]
    fn settings_post_rolls_back_when_the_write_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").unwrap();
        let cfg = cfg_in_tempdir(&blocker.join("config"));
        let before = cfg.read().unwrap().auto_eject;
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(&format!(r#"{{"auto_eject": {}}}"#, !before)),
            &[],
        );
        assert_eq!(code, 500, "{body}");
        assert!(body.contains("nothing was changed"), "{body}");
        assert_eq!(cfg.read().unwrap().auto_eject, before, "rolled back");
    }

    // A present-but-invalid on_read_error must not block the legacy
    // abort_on_error migration: the fallback was gated on key-exists but
    // assignment needed a string, so `null` silently skipped both.
    #[test]
    fn settings_post_null_on_read_error_still_applies_the_legacy_migration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        cfg.write().unwrap().on_read_error = "skip".to_string();

        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"on_read_error": null, "abort_on_error": true}"#),
            &[],
        );

        assert_eq!(code, 200, "the PATCH reports success: {body}");
        assert_eq!(
            cfg.read().unwrap().on_read_error,
            "stop",
            "a null on_read_error carries no policy, so the legacy \
                 abort_on_error=true must still migrate to \"stop\" — \
                 answering 200 while applying neither is a save that looks \
                 like it worked and did nothing"
        );
    }

    #[test]
    fn settings_post_masked_keyserver_url_preserves_stored() {
        // The secret-sentinel guard, MASKED half: a POST carrying the
        // masked keyserver_url (containing SECRET_SENTINEL — the form GET
        // returns) must NOT clobber the stored token-bearing URL.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let stored = "https://8.8.8.8/mysecrettoken/decode";
        cfg.write().unwrap().keyserver_url = stored.to_string();

        let masked = format!("https://8.8.8.8/{SECRET_SENTINEL}");
        let patch = format!(r#"{{"keyserver_url": "{masked}"}}"#);
        let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(&patch), &[]);
        assert_eq!(code, 200);
        assert_eq!(
            cfg.read().unwrap().keyserver_url,
            stored,
            "a masked (sentinel) keyserver_url must leave the stored URL intact"
        );
    }

    // H4/H10: a non-bool webhook flag (or a malformed element) is a 400
    // naming the field; nothing in the patch may land.
    #[test]
    fn settings_post_rejects_non_bool_webhook_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let stored = vec![crate::server::config::WebhookEntry {
            url: "https://discord.com/api/webhooks/1/secretA".into(),
            post_rip: true,
            post_mux: true,
            post_move: true,
            headers: Default::default(),
        }];
        cfg.write().unwrap().webhook_urls = stored.clone();
        for (patch, field) in [
            (
                r#"{"tmdb_api_key":"changed","webhook_urls":[{"url":"https://example.com/h","post_rip":"yes"}]}"#,
                "webhook_urls[0].post_rip",
            ),
            (
                r#"{"tmdb_api_key":"changed","webhook_urls":["https://example.com/a",{"url":"https://example.com/h","post_move":1}]}"#,
                "webhook_urls[1].post_move",
            ),
            (
                r#"{"tmdb_api_key":"changed","webhook_urls":[42]}"#,
                "webhook_urls[0]",
            ),
        ] {
            let (code, body) = roundtrip(&cfg, "POST", "/api/settings", Some(patch), &[]);
            assert_eq!(code, 400, "{patch} must be rejected, got {code}: {body}");
            assert!(body.contains(field), "error must name {field}, got: {body}");
            let c = cfg.read().unwrap();
            assert_eq!(
                c.webhook_urls, stored,
                "stored webhooks must survive a rejected POST"
            );
            assert_ne!(
                c.tmdb_api_key, "changed",
                "no field may land on a rejected POST"
            );
        }
    }

    #[test]
    fn settings_post_real_keyserver_url_replaces_stored() {
        // The secret-sentinel guard, REAL-VALUE half: a POST with a genuine
        // (sentinel-free, SSRF-valid) keyserver_url replaces the stored one.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        cfg.write().unwrap().keyserver_url = "https://8.8.8.8/old/decode".to_string();

        // Public IP literal validates without DNS.
        let patch = r#"{"keyserver_url": "https://1.1.1.1/newtoken/decode"}"#;
        let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(patch), &[]);
        assert_eq!(code, 200);
        assert_eq!(
            cfg.read().unwrap().keyserver_url,
            "https://1.1.1.1/newtoken/decode",
            "a real new keyserver_url must replace the stored one"
        );
    }

    #[test]
    fn settings_post_empty_keyserver_url_clears_it() {
        // The clear half: an empty (no-sentinel) keyserver_url writes
        // through, clearing the stored value (disables the online source).
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        cfg.write().unwrap().keyserver_url = "https://8.8.8.8/token/decode".to_string();

        let (code, _) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"keyserver_url": ""}"#),
            &[],
        );
        assert_eq!(code, 200);
        assert_eq!(
            cfg.read().unwrap().keyserver_url,
            "",
            "an empty keyserver_url must clear the stored value"
        );
    }

    #[test]
    fn settings_post_invalid_address_url_is_rejected_400_and_not_stored() {
        // A non-sentinel keyserver_url naming an unreachable address must be
        // rejected before the write guard — stored value untouched.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        cfg.write().unwrap().keyserver_url = "https://8.8.8.8/keep/decode".to_string();

        let (code, _) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"keyserver_url": "https://0.0.0.0/admin"}"#),
            &[],
        );
        assert_eq!(
            code, 400,
            "an invalid-address keyserver_url must be rejected"
        );
        assert_eq!(
            cfg.read().unwrap().keyserver_url,
            "https://8.8.8.8/keep/decode",
            "a rejected keyserver_url must not mutate the stored value"
        );
    }

    #[test]
    fn settings_post_http_keyserver_url_is_rejected_like_the_rip_path() {
        // The rip gates on keysources' https-only validator; save must use the
        // same one, or an http:// URL saves fine and every rip has no online source.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        cfg.write().unwrap().keyserver_url = "https://8.8.8.8/keep/decode".to_string();

        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"keyserver_url": "http://8.8.8.8/decode"}"#),
            &[],
        );
        assert_eq!(code, 400, "http:// keyserver_url must be rejected at save");
        assert!(body.contains("https://"), "error must say why: {body}");
        assert_eq!(
            cfg.read().unwrap().keyserver_url,
            "https://8.8.8.8/keep/decode"
        );
    }

    #[test]
    fn settings_post_keyserver_url_is_stored_trimmed() {
        // Validation runs on the trimmed URL, so the trimmed URL is what is stored.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let patch = r#"{"keyserver_url": "  https://1.1.1.1/decode \n"}"#;
        let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(patch), &[]);
        assert_eq!(code, 200);
        assert_eq!(cfg.read().unwrap().keyserver_url, "https://1.1.1.1/decode");
    }

    #[test]
    fn settings_post_unresolvable_masked_webhook_leaves_output_dir_unmutated() {
        // Red/green regression for the write-guard early-return defect:
        // resolution runs INSIDE `cfg.write()` after ~20 fields have
        // already mutated the live Config; a 400 must not leave that undone.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let original_output_dir = cfg.read().unwrap().output_dir.clone();

        // A valid output_dir change ordered BEFORE an unresolvable masked
        // webhook entry in the patch (mirrors field-mutation order inside
        // the guard: output_dir first, webhook_urls near the end).
        let patch = serde_json::json!({
            "output_dir": "/mnt/zz-settings-guard-fixture-4711/output",
            "webhook_urls": ["https://hooks.slack.com/********"],
        })
        .to_string();
        let (code, body) = roundtrip(&cfg, "POST", "/api/settings", Some(&patch), &[]);

        assert_eq!(
            code, 400,
            "an unresolvable masked webhook_urls entry must be rejected, got: {body}"
        );
        assert_eq!(
            cfg.read().unwrap().output_dir,
            original_output_dir,
            "output_dir on the live Config must be UNCHANGED when the save is \
                 rejected — a partial in-memory mutation must never survive a 400"
        );
    }

    // ── handle_stop / handle_scan / handle_rip reach their handlers ──

    #[test]
    fn stop_route_reaches_handle_stop_with_its_own_drive_not_found() {
        // A well-formed device with no STATE entry must reach handle_stop,
        // which answers with ITS OWN "drive not found" body — proving the
        // route is wired to the handler, not just validated then dropped.
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (code, body) = roundtrip(&cfg, "POST", "/api/stop/sr0", None, &[]);
        assert_eq!(code, 404);
        assert!(
            body.contains("drive not found"),
            "must be handle_stop's response, not the dispatch 404; got: {body}"
        );
        // And the dispatch fallthrough body must NOT appear.
        assert!(
            !body.contains("\"error\":\"not found\""),
            "a wired /api/stop/<dev> must not hit the dispatch 404"
        );
    }

    // ── handle_accept_loss: rejected claim must not arm the override ──

    #[test]
    fn accept_loss_rejected_while_busy_does_not_arm_marker() {
        // A rip in flight means try_claim_active loses, and the request
        // must 409 WITHOUT writing `.accept-loss` — before the fix the
        // marker was written first, leaving the override armed on disk.
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let device = "sgacceptloss1";
        let disc_name = "DamagedDisc";

        let staging_dir = {
            let c = cfg.read().unwrap();
            c.staging_device_dir(&crate::server::util::sanitize_path_compact(disc_name))
        };
        let dir = std::path::Path::new(&staging_dir);
        std::fs::create_dir_all(dir).unwrap();
        // Pre-existing terminal markers a real damaged/failed rip would
        // have left behind — these must survive a rejected accept too.
        std::fs::write(dir.join(ripper::staging::FAILED_MARKER), b"loss").unwrap();

        // Mark the device busy (as if a rip were already running) with a
        // known current disc name, exactly like a real in-flight rip.
        ripper::update_state(
            device,
            ripper::RipState {
                device: device.to_string(),
                status: "ripping".to_string(),
                disc_name: disc_name.to_string(),
                ..Default::default()
            },
        );

        let (code, _) = roundtrip(
            &cfg,
            "POST",
            &format!("/api/accept-loss/{device}"),
            None,
            &[],
        );
        assert_eq!(
            code, 409,
            "accept-loss on a busy device must be rejected, not silently dropped"
        );
        assert!(
            !dir.join(ripper::staging::ACCEPT_LOSS_MARKER).exists(),
            "a rejected accept-loss must NOT arm the one-shot override on disk"
        );
        assert!(
            dir.join(ripper::staging::FAILED_MARKER).exists(),
            "a rejected accept-loss must leave the existing .failed marker intact"
        );
    }

    #[test]
    fn accept_loss_that_cannot_be_saved_is_refused_not_acknowledged() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let device = "sgacceptloss2";
        let disc_name = "UnwritableDisc";
        let staging_dir = {
            let c = cfg.read().unwrap();
            c.staging_device_dir(&crate::server::util::sanitize_path_compact(disc_name))
        };
        let dir = std::path::Path::new(&staging_dir);
        std::fs::create_dir_all(dir).unwrap();
        ripper::update_state(
            device,
            ripper::RipState {
                device: device.to_string(),
                status: "idle".to_string(),
                disc_name: disc_name.to_string(),
                ..Default::default()
            },
        );
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let bypass = std::fs::write(dir.join(".probe"), b"").is_ok();
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            &format!("/api/accept-loss/{device}"),
            None,
            &[],
        );
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if bypass {
            eprintln!("skipped: running with permission bypass (root)");
            return;
        }
        assert_eq!(code, 500, "{body}");
        assert_eq!(
            ripper::STATE.lock().unwrap()[device].status,
            "idle",
            "the refused Accept must release the claim"
        );
    }

    #[test]
    fn accept_loss_transition_reopens_aborted_dir() {
        // A full-HTTP *successful* handle_accept_loss races its own spawned
        // rip worker against this test's read of state.json, so instead this
        // drives the exact read-modify-write closure it passes onward.
        use ripper::staging::{DiscState, StagingState, read_state, write_state};

        // AbortedLoss + a recorded failure reason + a nonzero restart count
        // — exactly what a real over-threshold abort leaves behind — must
        // reopen to Ripped with both annotations cleared.
        let tmp = tempfile::TempDir::new().unwrap();
        let aborted_dir = tmp.path().join("aborted");
        std::fs::create_dir_all(&aborted_dir).unwrap();
        let mut st = DiscState::new(StagingState::AbortedLoss);
        st.failure_reason = Some("main movie: 42s lost over threshold".to_string());
        st.restart_count = 2;
        write_state(&aborted_dir, &st);

        let mut aborted_after = read_state(&aborted_dir).unwrap();
        ripper::staging::apply_accept_loss_reopen(&mut aborted_after);
        write_state(&aborted_dir, &aborted_after);

        let after = read_state(&aborted_dir).unwrap();
        assert_eq!(
            after.state,
            StagingState::Ripped,
            "an AbortedLoss dir must reopen to Ripped so the resume re-mux \
                 (not the abort gate) picks it up"
        );
        assert_eq!(
            after.failure_reason, None,
            "accept-loss must clear the recorded failure reason"
        );
        assert_eq!(
            after.restart_count, 0,
            "accept-loss must reset the restart counter"
        );

        // A Done dir (already muxed) must NOT be pulled back into Ripped —
        // guards against a match-arm regression reopening every terminal
        // state, not just the abort/failed ones.
        let done_dir = tmp.path().join("done");
        std::fs::create_dir_all(&done_dir).unwrap();
        let mut done_st = DiscState::new(StagingState::Done);
        done_st.restart_count = 1;
        write_state(&done_dir, &done_st);

        let mut done_after = read_state(&done_dir).unwrap();
        ripper::staging::apply_accept_loss_reopen(&mut done_after);
        write_state(&done_dir, &done_after);
        let done_after = read_state(&done_dir).unwrap();

        assert_eq!(
            done_after.state,
            StagingState::Done,
            "a Done dir must NOT be reopened by the accept-loss transition"
        );
    }

    // ── FIX 5: accept-loss must respect the .muxing ownership marker —
    // apply_accept_loss_reopen and write_failed_marker are both
    // lock-free RMWs on state.json, so arming mid-mux can clobber a quarantine.
    #[test]
    fn accept_loss_guard_refuses_while_muxing() {
        use ripper::staging::{DiscState, StagingState, write_state};
        let tmp = tempfile::TempDir::new().unwrap();

        // A dir the worker is actively muxing: the guard must see has_muxing.
        let muxing_dir = tmp.path().join("muxing");
        std::fs::create_dir_all(&muxing_dir).unwrap();
        let mut st = DiscState::new(StagingState::Ripped);
        st.muxing = true;
        write_state(&muxing_dir, &st);
        assert!(
            ripper::staging::muxing_status(&muxing_dir).unwrap(),
            "an actively-muxing dir must report is_muxing so accept-loss refuses (409)"
        );

        // A stable terminal dir (mux finished): the guard must let it through.
        let failed_dir = tmp.path().join("failed");
        std::fs::create_dir_all(&failed_dir).unwrap();
        assert!(ripper::staging::write_failed_marker(&failed_dir, "E6008"));
        assert!(
            !ripper::staging::muxing_status(&failed_dir).unwrap(),
            "a settled (non-muxing) dir must NOT report is_muxing; accept-loss proceeds"
        );
    }

    // ── handle_title_override: a title override with no media_type must
    // fall back to the disc's detected tmdb_media_type, not default to
    // "movie" (which collapses every TV episode into one Show (Year).mkv).
    #[test]
    fn title_override_omitted_media_type_preserves_detected_tv() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let device = "sgtitleovrtvpreserve1";
        ripper::update_state(
            device,
            ripper::RipState {
                device: device.to_string(),
                tmdb_media_type: "tv".to_string(),
                ..Default::default()
            },
        );

        let (code, _) = roundtrip(
            &cfg,
            "POST",
            &format!("/api/title/{device}"),
            Some(r#"{"title":"Endeavour","tmdb_id":0}"#),
            &[],
        );
        assert_eq!(code, 200, "a known device must accept the override");

        let stored = ripper::take_title_override(device)
            .expect("handle_title_override must record an override");
        assert_eq!(
            stored.media_type, "tv",
            "omitting media_type must preserve the disc's detected \
                 tmdb_media_type (tv), not fall back to movie"
        );

        crate::server::ripper::STATE.lock().unwrap().remove(device);
    }

    #[test]
    fn title_override_card_shows_what_the_engine_will_use() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let device = "sgtitleovrcard4";
        ripper::update_state(
            device,
            ripper::RipState {
                device: device.to_string(),
                tmdb_title: "Film A".to_string(),
                tmdb_poster: "https://image.tmdb.org/t/p/w185/a.jpg".to_string(),
                tmdb_overview: "About A".to_string(),
                tmdb_media_type: "movie".to_string(),
                ..Default::default()
            },
        );
        let (code, _) = roundtrip(
            &cfg,
            "POST",
            &format!("/api/title/{device}"),
            Some(r#"{"title":"Show B","media_type":"tv","tmdb_id":0}"#),
            &[],
        );
        assert_eq!(code, 200);
        {
            let s = crate::server::ripper::STATE.lock().unwrap();
            let rs = &s[device];
            assert_eq!(rs.tmdb_title, "Show B");
            assert_eq!(rs.tmdb_poster, "", "A's poster must not stay on B's card");
            assert_eq!(rs.tmdb_overview, "");
            assert_eq!(rs.tmdb_media_type, "tv");
        }
        // A second override omitting media_type keeps the first one's tv.
        let (code, _) = roundtrip(
            &cfg,
            "POST",
            &format!("/api/title/{device}"),
            Some(r#"{"title":"Show C","tmdb_id":0}"#),
            &[],
        );
        assert_eq!(code, 200);
        let stored = ripper::take_title_override(device).unwrap();
        assert_eq!(stored.media_type, "tv");
        crate::server::ripper::STATE.lock().unwrap().remove(device);
    }

    #[test]
    fn title_override_omitted_media_type_defaults_movie_when_unknown() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let device = "sgtitleovrnodetect2";
        ripper::update_state(
            device,
            ripper::RipState {
                device: device.to_string(),
                // tmdb_media_type left empty: nothing detected yet.
                ..Default::default()
            },
        );

        let (code, _) = roundtrip(
            &cfg,
            "POST",
            &format!("/api/title/{device}"),
            Some(r#"{"title":"X","tmdb_id":0}"#),
            &[],
        );
        assert_eq!(code, 200, "a known device must accept the override");

        let stored = ripper::take_title_override(device)
            .expect("handle_title_override must record an override");
        assert_eq!(
            stored.media_type, "movie",
            "with no detected media_type, omission must fall back to movie"
        );

        crate::server::ripper::STATE.lock().unwrap().remove(device);
    }

    #[test]
    fn title_override_explicit_media_type_is_honored() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let device = "sgtitleovrexplicit3";
        // Detected as movie, but the operator explicitly overrides to tv:
        // the explicit field must win over whatever STATE says.
        ripper::update_state(
            device,
            ripper::RipState {
                device: device.to_string(),
                tmdb_media_type: "movie".to_string(),
                ..Default::default()
            },
        );

        let (code, _) = roundtrip(
            &cfg,
            "POST",
            &format!("/api/title/{device}"),
            Some(r#"{"title":"X","media_type":"tv","tmdb_id":0}"#),
            &[],
        );
        assert_eq!(code, 200, "a known device must accept the override");

        let stored = ripper::take_title_override(device)
            .expect("handle_title_override must record an override");
        assert_eq!(
            stored.media_type, "tv",
            "an explicit media_type in the request body must be honored \
                 over the detected tmdb_media_type"
        );

        crate::server::ripper::STATE.lock().unwrap().remove(device);
    }

    // ── handle_settings_post: pre-write-guard validation rejections ──

    #[test]
    fn settings_post_rejects_invalid_output_format_enum() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let before = cfg.read().unwrap().output_format.clone();
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"output_format": "garbage"}"#),
            &[],
        );
        assert_eq!(code, 400, "an out-of-vocabulary enum must be rejected");
        assert!(body.contains("invalid value for output_format"));
        assert_eq!(
            cfg.read().unwrap().output_format,
            before,
            "a rejected enum must not mutate the live config"
        );
    }

    #[test]
    fn settings_post_rejects_out_of_range_port() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"port": 70000}"#),
            &[],
        );
        assert_eq!(code, 400, "a port outside 1..=65535 must be rejected");
        assert!(body.contains("port must be 1..=65535"));
    }

    #[test]
    fn settings_post_rejects_the_ports_just_outside_the_valid_range() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        for port in [0, 65536] {
            let (code, body) = roundtrip(
                &cfg,
                "POST",
                "/api/settings",
                Some(&format!(r#"{{"port": {port}}}"#)),
                &[],
            );
            assert_eq!(code, 400, "port {port}: {body}");
            assert!(body.contains("port must be 1..=65535"));
        }
    }

    #[test]
    fn settings_post_rejects_relative_output_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"output_dir": "relative/path"}"#),
            &[],
        );
        assert_eq!(code, 400, "a non-absolute mount root must be rejected");
        assert!(body.contains("output_dir must be an absolute path"));
    }

    #[test]
    fn settings_post_rejects_parent_dir_in_movie_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"movie_dir": "../escape"}"#),
            &[],
        );
        assert_eq!(code, 400, "a climbing sub-directory name must be rejected");
        assert!(body.contains("movie_dir must not contain '..'"));
    }

    #[test]
    fn settings_post_rejects_keydb_path_without_cfg_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let (code, body) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"keydb_path": "/data/keys.txt"}"#),
            &[],
        );
        assert_eq!(code, 400, "a non-.cfg keydb path must be rejected");
        assert!(body.contains("keydb_path must be an absolute .cfg path"));
    }

    // ── handle_settings_post: the write-guard field application ──────

    #[test]
    fn settings_post_applies_scalar_and_clamped_fields() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        let abs_out = tmp.path().join("out");
        let patch = serde_json::json!({
            // Both default to true: patching false tells "applied" from "ignored".
            "auto_eject": false,
            "main_feature": false,
            "capture_without_keys": true,
            "keep_iso": true,
            // Over the ceilings — each must clamp, not persist raw.
            "max_retries": 99,
            "min_length_secs": 40 * 24 * 3600u64,
            "decrypt_threads": 9999,
            "log_retention_days": 99999u64,
            "movie_dir": "films",
            "output_dir": abs_out.to_string_lossy(),
            "port": 9999,
            "output_format": "iso",
        })
        .to_string();
        let (code, body) = roundtrip(&cfg, "POST", "/api/settings", Some(&patch), &[]);
        assert_eq!(code, 200, "a valid multi-field patch must succeed: {body}");
        let c = cfg.read().unwrap();
        assert!(!c.auto_eject && c.capture_without_keys && c.keep_iso);
        // File-only settings are not set by a save (the form never offers them).
        assert!(Config::default().main_feature);
        assert_eq!(c.main_feature, Config::default().main_feature);
        assert_eq!(c.min_length_secs, Config::default().min_length_secs);
        assert_eq!(c.max_retries, 10, "max_retries clamps to 10");
        assert_eq!(c.decrypt_threads, 256, "decrypt_threads clamps to 256");
        assert_eq!(
            c.log_retention_days, 3650,
            "log_retention_days clamps to 10 years"
        );
        assert_eq!(c.movie_dir, "films");
        assert_eq!(c.output_dir, abs_out.to_string_lossy());
        assert_eq!(c.port, 9999);
        assert_eq!(c.output_format, "iso");
    }

    #[test]
    fn settings_post_tmdb_api_key_sentinel_is_ignored_but_real_value_applied() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        cfg.write().unwrap().tmdb_api_key = "original-secret".to_string();

        // The masked sentinel round-trip must NOT clobber the stored key.
        let masked = format!(r#"{{"tmdb_api_key": "{SECRET_SENTINEL}"}}"#);
        let (code, _) = roundtrip(&cfg, "POST", "/api/settings", Some(&masked), &[]);
        assert_eq!(code, 200);
        assert_eq!(
            cfg.read().unwrap().tmdb_api_key,
            "original-secret",
            "the redaction sentinel must be ignored, not stored"
        );

        // A genuine new value replaces it.
        let (code, _) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"tmdb_api_key": "brand-new-key"}"#),
            &[],
        );
        assert_eq!(code, 200);
        assert_eq!(cfg.read().unwrap().tmdb_api_key, "brand-new-key");
    }

    #[test]
    fn settings_post_rip_mode_single_zeroes_retries_multi_bumps_to_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());

        let (code, _) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"rip_mode": "single", "max_retries": 5}"#),
            &[],
        );
        assert_eq!(code, 200);
        assert_eq!(
            cfg.read().unwrap().max_retries,
            0,
            "single mode forces zero retries"
        );

        let (code, _) = roundtrip(
            &cfg,
            "POST",
            "/api/settings",
            Some(r#"{"rip_mode": "multi", "max_retries": 0}"#),
            &[],
        );
        assert_eq!(code, 200);
        assert_eq!(
            cfg.read().unwrap().max_retries,
            1,
            "multi mode with zero retries clamps up to 1"
        );
    }

    // ── handle_system_info (GET /api/system) ────────────────────────

    #[test]
    fn system_info_serves_queues_and_reflects_a_done_staging_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = cfg_in_tempdir(tmp.path());
        // Point staging at a real dir holding one settled (`.done`) rip so
        // build_queue_views emits a Move-queue row.
        let staging = tmp.path().join("staging");
        let done_dir = staging.join("Some_Movie_2024");
        std::fs::create_dir_all(&done_dir).unwrap();
        std::fs::write(done_dir.join(".done"), b"").unwrap();
        cfg.write().unwrap().staging_dir = staging.to_string_lossy().to_string();

        let (code, body) = roundtrip(&cfg, "GET", "/api/system", None, &[]);
        assert_eq!(code, 200, "GET /api/system must be served: {body}");
        let v: serde_json::Value = serde_json::from_str(&body).expect("system info is JSON");
        assert!(v.get("move_queue").is_some(), "must carry a move_queue");
        assert!(v.get("mux_queue").is_some(), "must carry a mux_queue");
        assert!(
            v.get("debug_enabled").is_some(),
            "must report the debug toggle state"
        );
        let mq = v["move_queue"].as_array().expect("move_queue is an array");
        assert!(
            mq.iter()
                .any(|e| e.as_str().unwrap_or("").contains("Some Movie 2024")),
            "the settled .done dir must show in the move queue, got: {mq:?}"
        );
    }

    #[test]
    fn system_reboot_requests_a_fresh_instance_and_preserves_durable_state() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::server::REBOOT.store(false, std::sync::atomic::Ordering::Release);
                crate::server::SHUTDOWN.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _reset = Reset;
        crate::server::REBOOT.store(false, std::sync::atomic::Ordering::Release);
        crate::server::SHUTDOWN.store(false, std::sync::atomic::Ordering::Release);
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (code, body) = roundtrip(&cfg, "POST", "/api/system/reboot", None, &[]);
        assert_eq!(code, 202, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["rebooting"], true);
        assert!(crate::server::REBOOT.load(std::sync::atomic::Ordering::Acquire));
        assert!(crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Acquire));
    }

    // ── handle_device_log (GET /api/logs/<device>) ──────────────────

    #[test]
    fn device_log_route_serves_logged_lines_for_a_valid_device() {
        // A unique device name so parallel tests can't share the ring.
        let device = "zzweblogdev01";
        crate::server::log::device_log(device, "unit-test-device-log-marker");
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (code, body) = roundtrip(&cfg, "GET", &format!("/api/logs/{device}"), None, &[]);
        assert_eq!(code, 200, "a valid device log must be served");
        assert!(
            body.contains("unit-test-device-log-marker"),
            "the served log must carry the logged line, got: {body}"
        );
    }

    // ── move/mux error clear endpoints ──────────────────────────────

    #[test]
    fn error_clear_endpoints_dispatch() {
        // Clear-all wipes process-global MOVE_ERRORS/MUX_ERRORS; hold the
        // shared lock so parallel mover/muxer/resume tests aren't wiped.
        let _g = crate::server::mover::TEST_STATE_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cfg = Arc::new(RwLock::new(Config::default()));
        let (c1, b1) = roundtrip(&cfg, "POST", "/api/move-errors/clear-all", None, &[]);
        assert_eq!(c1, 200);
        assert!(b1.contains("\"ok\":true"));
        let (c2, b2) = roundtrip(&cfg, "POST", "/api/mux-errors/clear-all", None, &[]);
        assert_eq!(c2, 200);
        assert!(b2.contains("\"ok\":true"));
        // clear-one with no path= param is a 400.
        let (c3, b3) = roundtrip(&cfg, "POST", "/api/move-errors/clear?x=1", None, &[]);
        assert_eq!(c3, 400);
        assert!(b3.contains("missing path"));
    }

    // ── SSE first frame (GET /events): it loops forever, so roundtrip
    // (reads to EOF) would hang. Run the handler on its own thread, read
    // the first SSE frame, then drop the socket so the next write fails.

    fn read_first_sse_frame(r: &mut impl Read) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let n = r.read(&mut chunk).expect("read first frame");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let find = |hay: &[u8], pat: &[u8]| hay.windows(pat.len()).position(|w| w == pat);
            if let Some(h) = find(&buf, b"\r\n\r\n") {
                let body = h + 4;
                // An SSE frame ends at its blank line, never at a `}`.
                if let Some(end) = find(&buf[body..], b"\n\n") {
                    buf.truncate(body + end + 2);
                    break;
                }
            }
        }
        buf
    }

    // Global STATE grows under parallel tests, so the frame can span reads; a read ending
    // on a nested `}` must not be taken as the end of the frame.
    #[test]
    fn read_first_sse_frame_waits_for_the_frame_terminator() {
        struct Chunks(Vec<&'static [u8]>);
        impl Read for Chunks {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() {
                    return Ok(0);
                }
                let c = self.0.remove(0);
                out[..c.len()].copy_from_slice(c);
                Ok(c.len())
            }
        }
        let mut r = Chunks(vec![
            b"HTTP/1.1 200 OK\r\n\r\ndata: {\"a\":{\"b\":1}",
            b",\"c\":2}\n\n",
            b"data: {}\n\n",
        ]);
        let buf = read_first_sse_frame(&mut r);
        assert_eq!(
            String::from_utf8_lossy(&buf),
            "HTTP/1.1 200 OK\r\n\r\ndata: {\"a\":{\"b\":1},\"c\":2}\n\n"
        );
    }

    #[test]
    fn sse_emits_a_json_state_first_frame() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let server = Server::http("127.0.0.1:0").expect("bind loopback server");
        let addr = server.server_addr().to_ip().expect("ip addr");

        let handler = std::thread::spawn(move || {
            let request = server.recv().expect("recv request");
            handle_sse(request, &cfg);
        });

        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        stream
            .write_all(b"GET /events HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .expect("write request");
        stream.flush().ok();

        let buf = read_first_sse_frame(&mut stream);
        drop(stream);
        handler.join().expect("handler thread");

        let raw = String::from_utf8_lossy(&buf);
        assert!(
            raw.contains("text/event-stream"),
            "the response must declare the SSE content type, got: {raw}"
        );
        let frame = raw
            .split("\r\n\r\n")
            .nth(1)
            .expect("an SSE body frame must follow the headers");
        let json = frame
            .trim_start()
            .strip_prefix("data: ")
            .expect("the first frame must be a `data: ` event")
            .trim();
        let _: serde_json::Value =
            serde_json::from_str(json).expect("the first SSE frame must carry valid state JSON");
    }
}
