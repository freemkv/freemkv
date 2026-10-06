//! Regression guards for the multi-pass progress helpers.
//!
//! These tests exist because v0.11.22 shipped several UI regressions
//! (bytes_bad counted NonTried as bad, speed_mbs was zero, errors=0
//! during multipass) that would have been caught by basic assertions
//! on push_pass_state's outputs. Keep this module lightweight but
//! comprehensive enough that each new progress field gets a "does the
//! right thing for the right status" check.

use super::*;

use freemkv_engine::{Mapfile, SectorStatus};

#[test]
fn row_is_busy_matches_scanning_and_ripping_only() {
    let row = |st: &str| RipState {
        status: st.to_string(),
        ..Default::default()
    };
    assert!(row_is_busy(&row("scanning")));
    assert!(row_is_busy(&row("ripping")));
    for st in ["idle", "done", "error", "", "waiting"] {
        assert!(!row_is_busy(&row(st)), "{st} must not count as busy");
    }
}

/// Create a throwaway mapfile inside a fresh `TempDir`. Caller must hold
/// the `TempDir` guard for the test's lifetime so its Drop cleans up the
/// mapfile instead of leaking it into temp_dir().
fn tmp_map(tag: &str, total: u64) -> (tempfile::TempDir, Mapfile) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{tag}.mapfile"));
    let map = Mapfile::create(&path, total, "test").unwrap();
    (dir, map)
}

/// Catches the mutation that feeds the done card the STARVED single-pass `total_lost_ms`
/// instead of the real in-title loss.
#[test]
fn single_pass_done_card_total_lost_ms_drives_severity() {
    // 10 skipped sectors -> below the 51-sector Moderate threshold, so
    // severity is decided purely by the ms-branch.
    let errors: u32 = 10;
    let final_lost_secs: f64 = 1.5; // 1500 ms of in-title loss

    // The wiring: single-pass must carry the real in-title loss.
    let single_pass = super::super::done_card_lost_ms(false, final_lost_secs, 0.0, 0.0);
    assert_eq!(
        single_pass,
        final_lost_secs * MILLIS_PER_SEC,
        "single-pass must derive the done card's lost-ms from the real \
             in-title loss, not from the mapfile snapshot it does not have"
    );

    // End to end, exactly as `rip_disc` publishes it.
    let dev = format!("sg_done_card_severity_{}", std::process::id());
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "done".to_string(),
            errors,
            total_lost_ms: single_pass,
            ..Default::default()
        },
    );
    let snap = STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&dev)
        .cloned()
        .expect("state entry exists");
    assert_eq!(
        snap.damage_severity, "moderate",
        "a >1s in-title loss must reach the done card as moderate damage"
    );

    // And the bug, spelled out: the starved value classifies the same rip
    // as cosmetic, so the two must not be interchangeable.
    assert_eq!(
        damage_severity_for(errors, 0.0),
        "cosmetic",
        "starved total_lost_ms under-classifies a >1s loss — this is the \
             value the wiring must NOT pick in single-pass mode"
    );

    // Multipass keeps the mapfile-derived value, plus the demux extra.
    assert_eq!(
        super::super::done_card_lost_ms(true, final_lost_secs, 4000.0, 100.0),
        4100.0,
        "multipass must keep the snapshot's mapfile-derived loss"
    );

    STATE.lock().unwrap_or_else(|e| e.into_inner()).remove(&dev);
}

fn minimal_title() -> libfreemkv::DiscTitle {
    // Build an almost-empty DiscTitle — enough for the helpers that
    // only touch extents, chapters, duration_secs, size_bytes.
    libfreemkv::DiscTitle {
        playlist: String::new(),
        playlist_id: 0,
        duration_secs: 0.0,
        size_bytes: 0,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: Vec::new(),
        content_format: libfreemkv::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

// Live at-risk/located-drilldown behavior these tests used to cover moved
// into libfreemkv (`locate_ranges` tests, src/disc/mod.rs). The terminal
// build_bad_ranges path (still autorip-side, done card) keeps coverage below.

/// The post-Stop cooldown must be measured on the monotonic clock, not the wall clock.
#[test]
fn the_stop_cooldown_is_not_measured_on_the_wall_clock() {
    let src = crate::server::util::source_lf(include_str!("state.rs"));
    // Start at the STATIC, not at `set_stop_cooldown`: the stored TYPE is
    // half the guarantee (an `Instant` map cannot hold a wall-clock
    // deadline at all), and it is declared above the setter.
    let start = src
        .find("pub(super) static STOP_COOLDOWNS")
        .expect("the cooldown map must exist");
    let end = src
        .find("pub(super) fn forget_device_state(")
        .expect("forget_device_state must follow the cooldown fns");
    // Strip comment lines first: otherwise the pin could be satisfied (or
    // broken) by its own prose, since the doc comment names `epoch_secs()`.
    let region: String = src[start..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // Not "no `epoch_secs()`" — that pins ONE spelling, and the wall clock
    // has several. Any of these reads the clock that can step backwards.
    for spelling in [
        "epoch_secs",
        "SystemTime",
        "UNIX_EPOCH",
        "duration_since",
        "chrono",
    ] {
        assert!(
            !region.contains(spelling),
            "the stop cooldown must not be derived from the wall clock \
                 (found `{spelling}`) — a backward NTP step, host clock reset \
                 or VM snapshot resume wedges the device out of auto-dispatch \
                 for as long as the clock is behind"
        );
    }
    assert!(
        region.contains("std::time::Instant>"),
        "the cooldown deadline must be STORED as a monotonic Instant, so a \
             wall-clock value cannot be put in the map at all"
    );
    assert!(
        region.contains("Instant::now()"),
        "the cooldown deadline must be computed from the monotonic clock"
    );
}

/// Both ends of the cooldown's observable contract, through the real
/// accessors: a freshly-set cooldown suppresses dispatch, and a deadline
/// that has already passed does not.
#[test]
fn a_stop_cooldown_expires() {
    let dev = "sg_cooldown_expiry_test";
    set_stop_cooldown(dev);
    assert!(
        is_in_cooldown(dev),
        "a cooldown just set must suppress the next insert tick"
    );

    // A deadline in the past is exactly what the poll loop sees once the
    // window has elapsed.
    STOP_COOLDOWNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            dev.to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
    assert!(
        !is_in_cooldown(dev),
        "once the deadline has passed the device must dispatch again — \
             a cooldown that never expires silently stops ripping the disc"
    );

    STOP_COOLDOWNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(dev);
}

#[test]
fn stopped_disc_ignores_transient_absence_but_rearms_after_removal() {
    use libfreemkv::DiscPresence::{Absent, Present, Settling};
    let dev = "sg_stop_presence";
    hold_stopped_disc(dev);
    assert_eq!(stopped_disc_presence(dev, Absent), Settling);
    assert_eq!(stopped_disc_presence(dev, Present), Present);
    assert!(try_claim_insert(dev).is_none());
    assert_eq!(stopped_disc_presence(dev, Absent), Settling);
    assert_eq!(stopped_disc_presence(dev, Absent), Absent);
    assert!(try_claim_insert(dev).is_some());
    STATE.lock().unwrap_or_else(|e| e.into_inner()).remove(dev);
    forget_device_state(dev);
}

#[test]
fn stopped_insertion_stays_held_after_cooldown_but_allows_explicit_work() {
    let dev = "sg_stopped_insertion";
    hold_stopped_disc(dev);
    set_stop_cooldown(dev);
    STOP_COOLDOWNS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            dev.into(),
            std::time::Instant::now() - std::time::Duration::from_secs(1),
        );
    assert!(!is_in_cooldown(dev));
    for _ in 0..3 {
        assert!(try_claim_insert(dev).is_none());
    }
    // Manual Scan/Rip bypasses the automatic-insert guard.
    assert!(try_claim_active(dev).is_some());
    release_stopped_disc(dev);
    STATE.lock().unwrap_or_else(|e| e.into_inner()).remove(dev);
    assert!(try_claim_insert(dev).is_some());
    STATE.lock().unwrap_or_else(|e| e.into_inner()).remove(dev);
    // Device removal clears the hold as well.
    hold_stopped_disc(dev);
    forget_device_state(dev);
    assert!(try_claim_insert(dev).is_some());
    STATE.lock().unwrap_or_else(|e| e.into_inner()).remove(dev);
    forget_device_state(dev);
}

/// A disc-supplied extent (untrusted `start_lba`/`sector_count`) must never be able to
/// overflow-panic the rip thread.
#[test]
fn byte_offset_in_title_survives_an_overflowing_extent() {
    let mut title = minimal_title();
    title.extents = vec![libfreemkv::Extent {
        start_lba: u32::MAX - 4,
        sector_count: u32::MAX,
    }];

    // Any LBA at all: the question is only whether this panics/wraps.
    let below = byte_offset_in_title(1000, &title);
    let inside = byte_offset_in_title(u32::MAX - 2, &title);

    assert_eq!(
        below, None,
        "an LBA below an extent that cannot express its own end must not \
             be reported as inside it (wrapped comparison)"
    );
    // Two sectors past the extent's start, so two sectors' worth of bytes
    // into the title.
    assert_eq!(
        inside,
        Some(2 * SECTOR_BYTES),
        "an LBA inside the extent must still map to its byte offset"
    );
}

#[test]
fn build_bad_ranges_excludes_not_yet_tried() {
    // Regression from v0.11.22: an empty rip (all NonTried) reported the
    // whole disc as "bad" via bytes_pending. Guards that "bad" ranges
    // include only `-` (Unreadable), never `?`/`*`/`/`.
    let (_p, mf) = tmp_map("nontried", 10_000);
    let title = minimal_title();
    let (ranges, count, _trunc, lost, largest) = build_bad_ranges(&mf, &title, 1000.0);
    assert!(
        ranges.is_empty(),
        "no Unreadable yet — list should be empty"
    );
    assert_eq!(count, 0);
    assert_eq!(lost, 0.0);
    assert_eq!(largest, 0.0);
}

#[test]
fn build_bad_ranges_ignores_non_trimmed_and_non_scraped() {
    // NonTrimmed/NonScraped mean "pass 1 failed, pass 2 needs to retry" —
    // must NOT appear in the UI's bad-range list yet; only `-` is confirmed bad.
    let (_p, mut mf) = tmp_map("trim_scrape", 10_000);
    mf.record(1000, 200, SectorStatus::NonTrimmed).unwrap();
    mf.record(3000, 100, SectorStatus::NonScraped).unwrap();
    let title = minimal_title();
    let (ranges, count, ..) = build_bad_ranges(&mf, &title, 1000.0);
    assert!(ranges.is_empty());
    assert_eq!(count, 0);
}

#[test]
fn build_bad_ranges_includes_unreadable() {
    let (_p, mut mf) = tmp_map("unreadable", 10_000);
    mf.record(2000, 100, SectorStatus::Unreadable).unwrap();
    let title = minimal_title();
    // bps = 2048 bytes/sec → a 100-byte range is 50 ms.
    let (ranges, count, _trunc, lost, largest) = build_bad_ranges(&mf, &title, 2048.0);
    assert_eq!(count, 1);
    assert_eq!(ranges.len(), 1);
    assert_eq!(ranges[0].lba, 2000 / 2048);
    assert!((lost - 100.0 / 2048.0 * 1000.0).abs() < 0.001);
    assert!((largest - lost).abs() < 0.001);
}

#[test]
fn build_bad_ranges_sorts_by_duration_desc() {
    let (_p, mut mf) = tmp_map("sort", 100_000);
    mf.record(1000, 100, SectorStatus::Unreadable).unwrap(); // small
    mf.record(20_000, 1000, SectorStatus::Unreadable).unwrap(); // big
    mf.record(50_000, 500, SectorStatus::Unreadable).unwrap(); // medium
    let title = minimal_title();
    let (ranges, ..) = build_bad_ranges(&mf, &title, 1000.0);
    assert_eq!(ranges.len(), 3);
    assert!(ranges[0].duration_ms > ranges[1].duration_ms);
    assert!(ranges[1].duration_ms > ranges[2].duration_ms);
}

#[test]
fn build_bad_ranges_truncates_to_50() {
    let (_p, mut mf) = tmp_map("truncate", 10_000_000);
    // 60 unreadable ranges, all same size. Must truncate to 50 with
    // `bad_ranges_truncated = 10`.
    for i in 0..60u64 {
        mf.record(i * 10_000, 100, SectorStatus::Unreadable)
            .unwrap();
    }
    let title = minimal_title();
    let (ranges, count, trunc, ..) = build_bad_ranges(&mf, &title, 1000.0);
    assert_eq!(count, 60);
    assert_eq!(ranges.len(), 50);
    assert_eq!(trunc, 10);
}

#[test]
fn byte_offset_in_title_within_single_extent() {
    let title = libfreemkv::DiscTitle {
        extents: vec![libfreemkv::Extent {
            start_lba: 1000,
            sector_count: 500,
        }],
        ..minimal_title()
    };
    // LBA 1100 is 100 sectors into the extent = 100 * 2048 bytes in title.
    assert_eq!(byte_offset_in_title(1100, &title), Some(100 * 2048));
}

#[test]
fn byte_offset_in_title_across_multiple_extents() {
    let title = libfreemkv::DiscTitle {
        extents: vec![
            libfreemkv::Extent {
                start_lba: 1000,
                sector_count: 100,
            },
            libfreemkv::Extent {
                start_lba: 5000,
                sector_count: 200,
            },
        ],
        ..minimal_title()
    };
    // LBA 5050 is 50 sectors into the 2nd extent; first extent is 100*2048.
    assert_eq!(
        byte_offset_in_title(5050, &title),
        Some(100 * 2048 + 50 * 2048)
    );
}

#[test]
fn byte_offset_in_title_returns_none_outside_extents() {
    let title = libfreemkv::DiscTitle {
        extents: vec![libfreemkv::Extent {
            start_lba: 1000,
            sector_count: 100,
        }],
        ..minimal_title()
    };
    // LBA 200 is before the only extent — probably UDF metadata, no
    // chapter mapping possible.
    assert_eq!(byte_offset_in_title(200, &title), None);
    assert_eq!(byte_offset_in_title(50_000, &title), None);
}

#[test]
fn update_state_with_preserves_untouched_fields() {
    // The whole point of update_state_with: fields the closure doesn't
    // touch must survive (three past regressions were Default::default()
    // wiping live progress fields during a watchdog tick).
    let dev = format!("test-preserve-{}", std::process::id());
    update_state_with(&dev, |s| {
        s.errors = 7;
        s.lost_video_secs = 1.5;
        s.last_sector = 12345;
        s.current_batch = 32;
        s.preferred_batch = 60;
    });
    // Now simulate a watchdog tick that only updates progress + status:
    update_state_with(&dev, |s| {
        s.status = "ripping".to_string();
        s.progress_pct = 42;
    });
    let snap = STATE
        .lock()
        .unwrap()
        .get(&dev)
        .cloned()
        .expect("entry must exist");
    assert_eq!(snap.errors, 7, "errors wiped");
    assert_eq!(snap.lost_video_secs, 1.5, "lost_video_secs wiped");
    assert_eq!(snap.last_sector, 12345, "last_sector wiped");
    assert_eq!(snap.current_batch, 32, "current_batch wiped");
    assert_eq!(snap.preferred_batch, 60, "preferred_batch wiped");
    assert_eq!(snap.progress_pct, 42, "new field not applied");
    assert_eq!(snap.status, "ripping", "new field not applied");
    // device is set independently of the HashMap key by or_insert_with's
    // `RipState { device: device.to_string(), ..Default::default() }`.
    assert_eq!(snap.device, dev, "device field not set on first insert");
}

fn minimal_pass_ctx(device: &str) -> PassContext {
    PassContext {
        device: device.to_string(),
        display_name: "Test Disc".to_string(),
        disc_format: "uhd".to_string(),
        tmdb_title: String::new(),
        tmdb_year: 0,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        tmdb_media_type: String::new(),
        duration: String::new(),
        codecs: String::new(),
        filename: "test.mkv".to_string(),
        bytes_total_disc: 50 * 1_073_741_824, // 50 GB
        batch: 32,
        max_retries: 5,
    }
}

/// Regression: set_pass_progress must not zero total_progress_pct /
/// total_progress_eta set by a previous pass's push_pass_state — the old
/// `..Default::default()` full-RipState replacement zeroed them each pass.
#[test]
fn set_pass_progress_preserves_total_progress_fields() {
    let dev = format!("test-spp-preserve-{}", std::process::id());
    // Simulate what push_pass_state would have written at the end of Pass 1.
    update_state_with(&dev, |s| {
        s.status = "ripping".to_string();
        s.total_progress_pct = 48;
        s.total_eta = "1:30:00".to_string();
        s.pass_progress_pct = 100;
        s.pass_eta = "0:05".to_string();
        s.eta = "0:05".to_string();
        s.speed_mbs = 12.5;
        s.errors = 12;
        s.total_lost_ms = 500.0;
    });
    // Now call set_pass_progress as it is at the start of Pass 2.
    let ctx = minimal_pass_ctx(&dev);
    set_pass_progress(
        &ctx,
        2,                  // pass
        7,                  // total_passes
        40 * 1_073_741_824, // bytes_good
        1_048_576,          // bytes_maybe
        2048,               // bytes_lost
    );
    let snap = STATE
        .lock()
        .unwrap()
        .get(&dev)
        .cloned()
        .expect("entry must exist");
    // These fields must survive the pass-boundary update.
    assert_eq!(
        snap.total_progress_pct, 48,
        "total_progress_pct must not be zeroed by set_pass_progress"
    );
    assert_eq!(
        snap.total_eta, "1:30:00",
        "total_eta must not be cleared by set_pass_progress"
    );
    // pass-specific fields are updated to the new pass.
    assert_eq!(snap.pass, 2, "pass not updated");
    assert_eq!(snap.total_passes, 7, "total_passes not updated");
    // Per-pass fields restart at the pass boundary.
    assert_eq!(snap.pass_progress_pct, 0, "pass bar must restart at 0%");
    assert_eq!(snap.pass_eta, "", "pass_eta must reset");
    assert_eq!(snap.eta, "", "eta must reset");
    assert_eq!(snap.speed_mbs, 0.0, "speed must reset");
    // 40 GiB good of a 50 GiB disc.
    assert_eq!(snap.progress_pct, 80);
    assert!((snap.progress_gb - 40.0).abs() < 0.001);
    // damage fields must also survive (were written by push_pass_state).
    assert_eq!(
        snap.errors, 12,
        "errors must not be zeroed by set_pass_progress"
    );
    assert!(
        (snap.total_lost_ms - 500.0).abs() < 0.001,
        "total_lost_ms must not be zeroed by set_pass_progress"
    );
}

/// Regression: the post-promotion damage snapshot must reflect the final
/// Unreadable sectors and produce non-zero damage fields — guards the
/// build_bad_ranges + update_state_with pattern used after promotion+flush.
#[test]
fn post_promotion_damage_push_is_non_zero_for_damaged_rip() {
    let dev = format!("test-promo-damage-{}", std::process::id());
    // Start with a "clean" state — as push_pass_state would leave it
    // if the last pass saw everything as NonTrimmed (not yet promoted).
    update_state_with(&dev, |s| {
        s.errors = 0;
        s.total_lost_ms = 0.0;
        s.bad_ranges = vec![];
        s.num_bad_ranges = 0;
    });
    // Mapfile with Unreadable sectors (as if promotion already ran).
    // Total must cover the highest position recorded: sector 30050.
    let total_bytes = 100_000u64 * 2048;
    let (_dir, mut map) = tmp_map("promo-damage", total_bytes);
    // Record two separate Unreadable ranges (by byte position).
    map.record(5_000 * 2048, 200 * 2048, SectorStatus::Unreadable)
        .unwrap();
    map.record(30_000 * 2048, 50 * 2048, SectorStatus::Unreadable)
        .unwrap();
    let title = minimal_title();
    let bps = 40_000.0 * 2048.0; // 40k sectors/s

    // Mirror the fix: re-derive damage from the promoted map and push.
    let (bad_ranges, num_bad, truncated, total_lost_ms, largest_gap_ms) =
        build_bad_ranges(&map, &title, bps);
    let main_title_bad = map.ranges_with(&[SectorStatus::Unreadable]);
    let main_bad_bytes = libfreemkv::disc::bytes_bad_in_title(&title, &main_title_bad);
    let main_lost_ms = if bps > 0.0 {
        main_bad_bytes as f64 * MILLIS_PER_SEC / bps
    } else {
        0.0
    };
    let errors = (map.stats().bytes_unreadable / 2048) as u32;
    update_state_with(&dev, |s| {
        s.errors = errors;
        s.total_lost_ms = total_lost_ms;
        s.main_lost_ms = main_lost_ms;
        s.bad_ranges = bad_ranges;
        s.num_bad_ranges = num_bad;
        s.bad_ranges_truncated = truncated;
        s.largest_gap_ms = largest_gap_ms;
    });

    let snap = STATE
        .lock()
        .unwrap()
        .get(&dev)
        .cloned()
        .expect("entry must exist");
    // The marker_damage read from STATE must see non-zero damage.
    assert_eq!(
        snap.errors, 250,
        "errors must reflect promoted unreadable sectors"
    );
    assert!(
        snap.total_lost_ms > 0.0,
        "total_lost_ms must be non-zero after promotion push"
    );
    assert_eq!(
        snap.num_bad_ranges, 2,
        "num_bad_ranges must reflect both unreadable ranges"
    );
    assert!(snap.largest_gap_ms > 0.0, "largest_gap_ms must be non-zero");
}

#[test]
fn spawn_failure_reset_to_idle_clears_busy() {
    // handle_scan/handle_rip set status="scanning" before spawning; on
    // spawn failure they roll back to idle. Pin that an idle push clears
    // is_busy so the next scan/rip isn't rejected with 409.
    let dev = format!("test-spawnfail-{}", std::process::id());
    // Pre-state set by the handler before spawn.
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "scanning".to_string(),
            ..Default::default()
        },
    );
    assert!(is_busy(&dev), "scanning device must read as busy");
    // The exact rollback the handlers perform on spawn failure.
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "idle".to_string(),
            ..Default::default()
        },
    );
    assert!(
        !is_busy(&dev),
        "after spawn-failure reset the device must no longer be busy \
             (else every future scan/rip 409s until restart)"
    );
}

#[test]
fn forget_device_state_clears_title_override_and_cooldown() {
    // Regression: on hot-unplug, TITLE_OVERRIDES and STOP_COOLDOWNS were
    // the only per-device maps not evicted, so stale entries accumulated
    // as device paths churned. forget_device_state must drop both.
    let dev = "/dev/sg-forget-test";
    set_title_override(
        dev,
        crate::server::tmdb::TmdbResult {
            title: "Test".to_string(),
            year: 2000,
            poster_url: String::new(),
            overview: String::new(),
            media_type: "movie".to_string(),
            tmdb_id: 0,
        },
    );
    set_stop_cooldown(dev);
    assert!(is_in_cooldown(dev), "cooldown must be set before eviction");

    forget_device_state(dev);

    assert!(
        !TITLE_OVERRIDES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(dev),
        "title override must be gone after forget_device_state"
    );
    assert!(
        !STOP_COOLDOWNS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(dev),
        "stop cooldown must be gone after forget_device_state"
    );
    assert!(
        !is_in_cooldown(dev),
        "device must not read as in cooldown after eviction"
    );
}

#[test]
fn try_claim_active_checked_refuses_unknown_device_and_state_does_not_grow() {
    // Security: device is shape-checked but unvalidated against real drives;
    // a caller looping fabricated names could grow STATE unbounded. Pin:
    // unknown+known=false is refused, leaves no trace.
    let dev = format!("test-forged-device-{}-xyz", std::process::id());
    assert!(
        STATE.lock().unwrap().get(&dev).is_none(),
        "precondition: device must not already exist"
    );

    assert!(
        try_claim_active_checked(&dev, false).is_none(),
        "an unknown device must not be claimable"
    );
    assert!(
        STATE.lock().unwrap().get(&dev).is_none(),
        "refusing the claim must not have inserted a STATE entry \
             (else looping forged names grows STATE without bound)"
    );

    // Looping the same forged name must not eventually succeed either.
    for _ in 0..5 {
        assert!(try_claim_active_checked(&dev, false).is_none());
    }
    assert!(
        STATE.lock().unwrap().get(&dev).is_none(),
        "repeated attempts on an unknown device must never insert an entry"
    );
}

#[test]
fn try_claim_active_checked_allows_known_true_for_new_device() {
    // known=true is exactly today's try_claim_active behaviour (used by
    // the poll loop's own trusted, just-enumerated device list) — must
    // still create a fresh entry and succeed.
    let dev = format!("test-known-new-{}", std::process::id());
    assert!(try_claim_active_checked(&dev, true).is_some());
    let snap = STATE.lock().unwrap().get(&dev).cloned().unwrap();
    assert_eq!(snap.status, "scanning");
    // Map key and RipState.device are independent — assert device
    // explicitly since deleting the struct literal's field wouldn't fail above.
    assert_eq!(
        snap.device, dev,
        "device field in the freshly-inserted RipState must match"
    );
}

#[test]
fn try_claim_active_checked_allows_unknown_flag_once_device_already_present() {
    // A real device always has a STATE entry before an operator can act on
    // it (poll loop pushes one per tick), so known=false must not block a
    // second legitimate claim on a device already known to STATE.
    let dev = format!("test-known-existing-{}", std::process::id());
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "idle".to_string(),
            ..Default::default()
        },
    );
    assert!(
        try_claim_active_checked(&dev, false).is_some(),
        "a device already present in STATE must remain claimable even \
             when the caller couldn't independently verify it"
    );
}

/// Catches admitting a claim while a TERMINAL-status device's worker is still unwinding.
#[test]
fn try_claim_active_refuses_a_device_whose_worker_is_still_unwinding() {
    let dev = format!("sg_claim_liveness_test_{}", std::process::id());
    let _ = super::super::take_rip_thread(&dev);
    // Terminal status: the worker has published "done" and is now in its
    // tail. STATE says free; the thread says otherwise.
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "done".to_string(),
            disc_present: true,
            ..Default::default()
        },
    );
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    super::super::session::spawn_rip_thread(&dev, "rip", move || {
        let _ = release_rx.recv();
    })
    .expect("the worker owns the device");

    assert!(
        !is_busy(&dev),
        "test setup: the status half of the claim must already read free, \
             otherwise this test cannot distinguish the two facts"
    );
    assert!(
        try_claim_active_checked(&dev, false).is_none(),
        "a claim must be refused while the device's worker thread is still \
             running, even though its status is terminal"
    );
    assert!(
        try_claim_active(&dev).is_none(),
        "the known=true wrapper must refuse on the same grounds"
    );

    // Worker exits; handle stays REGISTERED/unreaped (normal post-rip
    // state). Gate must read a finished handle as "not running".
    drop(release_tx);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while super::super::session::rip_thread_running(&dev) {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker should have exited as soon as its channel closed"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        try_claim_active_checked(&dev, false).is_some(),
        "once the worker has exited the device must be claimable again, \
             with no reaping step in between — the liveness gate must not \
             latch a device shut"
    );
    super::super::take_rip_thread(&dev)
        .expect("the finished handle is still registered")
        .join()
        .expect("worker joins cleanly");
    STATE.lock().unwrap().remove(&dev);
}

/// Catches the H1 duplicate-rip drain window; a claim must be refused for the WHOLE life of
/// the worker thread, even while another thread drains it.
#[test]
fn a_drain_in_flight_never_makes_a_live_worker_claimable() {
    let dev = format!("sg_claim_during_drain_test_{}", std::process::id());
    let _ = super::super::take_rip_thread(&dev);
    // The terminal tail: the worker has published "done" and is still
    // running. The status half of the gate is already open.
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "done".to_string(),
            disc_present: true,
            ..Default::default()
        },
    );
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    super::super::session::spawn_rip_thread(&dev, "rip", move || {
        let _ = release_rx.recv();
    })
    .expect("the worker owns the device");
    assert!(!is_busy(&dev), "test setup: the status half must read free");

    // `/api/stop` arrives and drains. The worker is blocked, so this
    // occupies the full budget and then reports a timeout.
    let drain_dev = dev.clone();
    let drain = std::thread::spawn(move || {
        super::super::session::join_rip_thread(&drain_dev, std::time::Duration::from_millis(600))
    });

    // Hammer the claim for the whole drain window. Every one must lose.
    let until = std::time::Instant::now() + std::time::Duration::from_millis(400);
    let mut attempts = 0u32;
    while std::time::Instant::now() < until {
        assert!(
            try_claim_active_checked(&dev, false).is_none(),
            "a claim must be refused while the device's worker is alive, \
                 even while a concurrent /api/stop drain is polling it"
        );
        attempts += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        attempts > 5,
        "test setup: the drain window was never sampled"
    );

    assert!(
        drain.join().expect("drain thread joins").is_err(),
        "test setup: the drain must have timed out against the blocked \
             worker, which is the window this test is about"
    );
    // And the handle must still be registered after that timeout — a drain
    // that loses the handle also loses the ability to reap the thread.
    assert!(
        super::super::session::rip_thread_running(&dev),
        "a timed-out drain must leave the live worker's handle registered"
    );

    drop(release_tx);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while super::super::session::rip_thread_running(&dev) {
        assert!(std::time::Instant::now() < deadline, "worker should exit");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = super::super::session::join_rip_thread(&dev, std::time::Duration::from_secs(5));
    STATE.lock().unwrap().remove(&dev);
}

#[test]
fn try_claim_active_rejects_second_claim_on_busy_device() {
    // try_claim_active refuses a second claim on an already-busy device,
    // closing the double-rip TOCTOU; `||` mutated to `&&` would go
    // undetected since no other test calls it twice.
    let dev = format!("test-doubleclaim-{}", std::process::id());
    assert!(
        try_claim_active(&dev).is_some(),
        "first claim on a fresh device must succeed"
    );
    assert!(
        try_claim_active(&dev).is_none(),
        "a second claim on an already-scanning device must be refused"
    );
}

#[test]
fn update_state_carries_forward_zero_claim_gen() {
    // Callers push fresh RipStates via ..Default::default() (claim_gen=0);
    // without carry-forward, every push after try_claim_active would
    // reset claim_gen to 0, defeating the stale-worker-detach guard.
    let dev = format!("test-claimgen-carry-{}", std::process::id());
    assert!(
        try_claim_active(&dev).is_some(),
        "claim bumps claim_gen to 1"
    );
    // A normal mid-rip push, exactly as push_pass_state/set_pass_progress
    // build it: claim_gen defaults to 0 via ..Default::default().
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "ripping".to_string(),
            ..Default::default()
        },
    );
    let snap = STATE.lock().unwrap().get(&dev).cloned().unwrap();
    assert_eq!(
        snap.claim_gen, 1,
        "claim_gen must be carried forward from the prior push, not reset to 0"
    );
}

#[test]
fn update_state_does_not_clobber_explicit_nonzero_claim_gen() {
    // Companion to the above: an explicit nonzero claim_gen must be
    // stored verbatim, not overwritten by the previous push's generation.
    let dev = format!("test-claimgen-explicit-{}", std::process::id());
    assert!(try_claim_active(&dev).is_some()); // claim_gen -> 1
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "ripping".to_string(),
            claim_gen: 99,
            ..Default::default()
        },
    );
    let snap = STATE.lock().unwrap().get(&dev).cloned().unwrap();
    assert_eq!(
        snap.claim_gen, 99,
        "an explicit nonzero claim_gen must not be overwritten by the carried-forward value"
    );
}

#[test]
fn take_title_override_returns_and_clears_the_override() {
    // Otherwise only exercised via forget_device_state's eviction test,
    // which never calls take_title_override, so a body->None mutant on
    // either function would pass the whole suite today.
    let dev = format!("/dev/test-override-{}", std::process::id());
    assert!(take_title_override(&dev).is_none(), "no override set yet");
    let picked = crate::server::tmdb::TmdbResult {
        title: "Override Title".to_string(),
        year: 1999,
        poster_url: String::new(),
        overview: String::new(),
        media_type: "movie".to_string(),
        tmdb_id: 0,
    };
    set_title_override(&dev, picked.clone());
    let taken = take_title_override(&dev).expect("override must be present after set");
    assert_eq!(taken.title, "Override Title");
    assert_eq!(taken.year, 1999);
    // take clears it — a second take must come back empty.
    assert!(
        take_title_override(&dev).is_none(),
        "take_title_override must remove the entry, not just read it"
    );
}

#[test]
fn device_known_reflects_state_membership() {
    let dev = format!("test-deviceknown-{}", std::process::id());
    assert!(
        !device_known(&dev),
        "unclaimed device must not read as known"
    );
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "idle".to_string(),
            ..Default::default()
        },
    );
    assert!(
        device_known(&dev),
        "device with a STATE entry must read as known"
    );
}

/// `update_state` must carry `disc_label` forward across the `..Default::default()`
/// fresh-RipState pushes, but never onto a different disc or an empty drive.
#[test]
fn update_state_carries_the_disc_label_but_never_onto_another_disc() {
    let dev = format!("test-disclabel-{}", std::process::id());
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "scanning".to_string(),
            disc_name: "Boxset Movie".to_string(),
            disc_label: "BOXSET_DISC_2".to_string(),
            ..Default::default()
        },
    );

    // A progress push that names the same disc but sets no label.
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "ripping".to_string(),
            disc_name: "Boxset Movie".to_string(),
            ..Default::default()
        },
    );
    let s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        s.get(&dev).map(|r| r.disc_label.as_str()),
        Some("BOXSET_DISC_2"),
        "the raw volume label must survive a default-built state push"
    );
    drop(s);

    // A DIFFERENT disc must not inherit it.
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "scanning".to_string(),
            disc_name: "Some Other Film".to_string(),
            ..Default::default()
        },
    );
    let s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        s.get(&dev).map(|r| r.disc_label.as_str()),
        Some(""),
        "a different disc must not inherit the previous disc's label"
    );
    drop(s);

    // Nor must an ejected / empty drive.
    update_state(
        &dev,
        RipState {
            device: dev.clone(),
            status: "idle".to_string(),
            disc_name: String::new(),
            ..Default::default()
        },
    );
    let s = STATE.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        s.get(&dev).map(|r| r.disc_label.as_str()),
        Some(""),
        "an empty drive must not keep a stale label"
    );
}

#[test]
fn push_pass_state_preserves_every_damage_indicator_field() {
    // push_pass_state's struct literal ends in ..Default::default(), so a
    // deleted field silently falls back to zero/empty, unnoticed until now.
    // Checks damage-indicator fields so a lossy disc can't look clean.
    let dev = format!("test-pps-fields-{}", std::process::id());
    let mut ctx = minimal_pass_ctx(&dev);
    // Non-default values for metadata pass-through fields, since
    // minimal_pass_ctx's empty/0 defaults are indistinguishable from
    // RipState::default() and would hide a field-deletion mutant.
    ctx.tmdb_title = "Example Movie".to_string();
    ctx.tmdb_year = 2024;
    ctx.tmdb_poster = "https://example/poster.jpg".to_string();
    ctx.tmdb_overview = "An example overview.".to_string();
    ctx.tmdb_media_type = "movie".to_string();
    ctx.duration = "2:15:00".to_string();
    ctx.codecs = "HEVC/DTS-HD".to_string();
    let pass_state = std::sync::Mutex::new(PassProgressState::new());
    // Mirrors what the real caller (mod.rs's sweep/patch progress
    // closures) does before invoking push_pass_state: seed the
    // per-pass work_done/work_total the library just reported.
    {
        let mut s = pass_state.lock().unwrap();
        s.last_work_done = 1_000_000;
        s.last_work_total = 2_000_000;
    }

    let located = libfreemkv::progress::LocatedProgress {
        ranges: vec![libfreemkv::progress::LocatedRange {
            lba: 12345,
            count: 7,
            duration_ms: 42.0,
            chapter: Some(3),
            time_offset_secs: Some(120.0),
        }],
        num_ranges: 1,
        truncated: 3,
        main_at_risk_ms: 999.0,
        largest_gap_ms: 888.0,
    };
    let p = libfreemkv::progress::PassProgress {
        kind: libfreemkv::progress::PassKind::Sweep,
        work_done: 1_000_000,
        work_total: 2_000_000,
        bytes_good_total: 500_000,
        bytes_unreadable_total: 20_480, // 10 sectors * 2048
        bytes_pending_total: 8_192,
        bytes_retryable_total: 4_096,
        bytes_total_disc: ctx.bytes_total_disc,
        disc_duration_secs: None,
        bytes_bad_in_main_title: 20_480,
        main_title_duration_secs: None,
        main_title_size_bytes: None,
        located,
    };

    push_pass_state(&ctx, &p, 2048.0, 3, 7, &pass_state);

    let snap = STATE
        .lock()
        .unwrap()
        .get(&dev)
        .cloned()
        .expect("push_pass_state must insert an entry for the device");

    assert_eq!(snap.device, dev, "device field dropped to Default");
    assert_eq!(snap.status, "ripping", "status field dropped to Default");
    assert!(snap.disc_present, "disc_present field dropped to Default");
    assert_eq!(
        snap.disc_name, "Test Disc",
        "disc_name field dropped to Default"
    );
    assert_eq!(
        snap.disc_format, "uhd",
        "disc_format field dropped to Default"
    );
    assert_eq!(
        snap.output_file, "test.mkv",
        "output_file field dropped to Default"
    );
    assert_eq!(snap.pass, 3, "pass field dropped to Default");
    assert_eq!(
        snap.total_passes, 7,
        "total_passes field dropped to Default"
    );
    assert_eq!(
        snap.bytes_good, 500_000,
        "bytes_good field dropped to Default"
    );
    assert_eq!(
        snap.bytes_maybe, 4_096,
        "bytes_maybe field dropped to Default"
    );
    assert_eq!(
        snap.bytes_lost, 20_480,
        "bytes_lost field dropped to Default"
    );
    assert_eq!(
        snap.bytes_total_disc, ctx.bytes_total_disc,
        "bytes_total_disc field dropped to Default"
    );
    assert_eq!(
        snap.num_bad_ranges, 1,
        "num_bad_ranges field dropped to Default"
    );
    assert_eq!(
        snap.bad_ranges_truncated, 3,
        "bad_ranges_truncated field dropped to Default"
    );
    assert_eq!(
        snap.bad_ranges.len(),
        1,
        "bad_ranges field dropped to Default"
    );
    assert_eq!(
        snap.bad_ranges[0].lba, 12345,
        "bad_ranges content lost across the DTO mapping"
    );
    assert!(
        (snap.main_at_risk_ms - 999.0).abs() < 0.001,
        "main_at_risk_ms field dropped to Default"
    );
    assert!(
        (snap.largest_gap_ms - 888.0).abs() < 0.001,
        "largest_gap_ms field dropped to Default"
    );
    assert_eq!(
        snap.errors, 10,
        "errors field dropped to Default (bytes_lost / SECTOR_BYTES)"
    );
    // bps 2048 bytes/s, 20_480 bytes lost -> exactly 10 s.
    assert!(
        (snap.total_lost_ms - 10_000.0).abs() < 0.001,
        "total_lost_ms = bytes_lost * 1000 / bps, got {}",
        snap.total_lost_ms
    );
    assert!(
        (snap.lost_video_secs - 10.0).abs() < 0.001,
        "lost_video_secs = total_lost_ms / 1000, got {}",
        snap.lost_video_secs
    );
    assert_eq!(snap.main_lost_ms, 0.0, "main_lost_ms is the done card's");
    assert_eq!(
        snap.preferred_batch, 32,
        "preferred_batch field dropped to Default"
    );
    assert_eq!(
        snap.current_batch, 32,
        "current_batch field dropped to Default"
    );
    assert_eq!(
        snap.last_sector,
        1_000_000 / SECTOR_BYTES,
        "last_sector field dropped to Default"
    );
    assert_eq!(
        snap.progress_pct, 50,
        "progress_pct field dropped to Default (1_000_000 / 2_000_000)"
    );
    assert_eq!(
        snap.tmdb_title, "Example Movie",
        "tmdb_title field dropped to Default"
    );
    assert_eq!(snap.tmdb_year, 2024, "tmdb_year field dropped to Default");
    assert_eq!(
        snap.tmdb_poster, "https://example/poster.jpg",
        "tmdb_poster field dropped to Default"
    );
    assert_eq!(
        snap.tmdb_overview, "An example overview.",
        "tmdb_overview field dropped to Default"
    );
    assert_eq!(
        snap.tmdb_media_type, "movie",
        "tmdb_media_type field dropped to Default"
    );
    assert_eq!(
        snap.duration, "2:15:00",
        "duration field dropped to Default"
    );
    assert_eq!(
        snap.codecs, "HEVC/DTS-HD",
        "codecs field dropped to Default"
    );
}

#[test]
fn update_state_keeps_started_epoch_across_active_pushes_only() {
    let dev = format!("test-started-epoch-{}", std::process::id());
    let push = |status: &str| {
        update_state(
            &dev,
            RipState {
                device: dev.clone(),
                status: status.to_string(),
                ..Default::default()
            },
        )
    };
    let started = || STATE.lock().unwrap().get(&dev).unwrap().started_epoch_secs;

    push("idle");
    assert_eq!(started(), 0, "an idle device has no start time");
    push("scanning");
    assert!(started() > 12_345, "idle -> active stamps the start");
    update_state_with(&dev, |s| s.started_epoch_secs = 12_345);
    push("ripping");
    assert_eq!(started(), 12_345, "an active push keeps the start");
    push("done");
    assert_eq!(started(), 0, "a terminal push clears the start");
    push("ripping");
    assert!(started() > 12_345, "a new rip stamps afresh");
    STATE.lock().unwrap().remove(&dev);
}

fn pass_progress(work_done: u64, unreadable: u64) -> libfreemkv::progress::PassProgress {
    libfreemkv::progress::PassProgress {
        kind: libfreemkv::progress::PassKind::Sweep,
        work_done,
        work_total: 1000,
        bytes_good_total: 0,
        bytes_unreadable_total: unreadable,
        bytes_pending_total: 0,
        bytes_retryable_total: 0,
        bytes_total_disc: 1000,
        disc_duration_secs: None,
        bytes_bad_in_main_title: 0,
        main_title_duration_secs: None,
        main_title_size_bytes: None,
        located: libfreemkv::progress::LocatedProgress {
            ranges: vec![],
            num_ranges: 0,
            truncated: 0,
            main_at_risk_ms: 0.0,
            largest_gap_ms: 0.0,
        },
    }
}

/// Total bar = done / (disc + max_retries x frozen unreadable + mux), with
/// retry passes counting the disc and each prior retry pass as done.
#[test]
fn push_pass_state_total_progress_uses_frozen_denominator() {
    let dev = format!("test-pps-total-{}", std::process::id());
    let mut ctx = minimal_pass_ctx(&dev);
    ctx.bytes_total_disc = 1000;
    ctx.max_retries = 2;
    let total_pct = || STATE.lock().unwrap().get(&dev).unwrap().total_progress_pct;
    let at = |state: &Mutex<PassProgressState>, done: u64| {
        let mut s = state.lock().unwrap();
        s.last_work_done = done;
        s.last_work_total = 1000;
    };

    // Pass 1: work 1000 + 2 x 100 + 1000 mux = 2200; 600 done -> 27%.
    let pass1 = Mutex::new(PassProgressState::new());
    at(&pass1, 600);
    push_pass_state(&ctx, &pass_progress(600, 100), 2048.0, 1, 4, &pass1);
    assert_eq!(total_pct(), 27);
    // Unreadable grows mid-pass; the denominator stays frozen at 100.
    push_pass_state(&ctx, &pass_progress(600, 500), 2048.0, 1, 4, &pass1);
    assert_eq!(total_pct(), 27, "denominator must stay frozen mid-pass");

    // Pass 3 (second retry): 1000 + 1 x 100 + 50 = 1150 of 2200 -> 52%.
    let pass3 = Mutex::new(PassProgressState::new());
    at(&pass3, 50);
    push_pass_state(&ctx, &pass_progress(50, 100), 2048.0, 3, 4, &pass3);
    assert_eq!(total_pct(), 52);
    STATE.lock().unwrap().remove(&dev);
}

#[test]
fn capped_eta_formats_and_caps() {
    assert_eq!(capped_eta(59), "59s");
    assert_eq!(capped_eta(61), "1:01");
    assert_eq!(capped_eta(3661), "1:01:01");
    assert_eq!(capped_eta(ETA_CAP_SECS), "6:00:00");
    assert_eq!(capped_eta(ETA_CAP_SECS + 1), ">6h");
    assert_eq!(capped_eta(u64::MAX), ">6h");
}

#[test]
fn build_bad_ranges_locates_chapter_time_and_sector_count() {
    let (_p, mut mf) = tmp_map("chapters", 10_000 * 2048);
    // 4 sectors at LBA 1600 (600 sectors into the title), and 2 sectors
    // at LBA 5000, outside every extent.
    mf.record(1600 * 2048, 4 * 2048, SectorStatus::Unreadable)
        .unwrap();
    mf.record(5000 * 2048, 2 * 2048, SectorStatus::Unreadable)
        .unwrap();
    let title = libfreemkv::DiscTitle {
        extents: vec![libfreemkv::Extent {
            start_lba: 1000,
            sector_count: 1000,
        }],
        duration_secs: 20.0,
        size_bytes: 1000 * 2048,
        chapters: vec![
            libfreemkv::disc::Chapter {
                time_secs: 0.0,
                name: "1".into(),
            },
            libfreemkv::disc::Chapter {
                time_secs: 10.0,
                name: "2".into(),
            },
        ],
        ..minimal_title()
    };
    let (ranges, count, ..) = build_bad_ranges(&mf, &title, 2048.0);
    assert_eq!(count, 2);
    let inside = ranges.iter().find(|r| r.lba == 1600).unwrap();
    assert_eq!(inside.count, 4);
    assert_eq!(
        inside.chapter,
        Some(2),
        "600/1000 of 20 s is 12 s: chapter 2"
    );
    assert!((inside.time_offset_secs.unwrap() - 12.0).abs() < 0.001);
    let outside = ranges.iter().find(|r| r.lba == 5000).unwrap();
    assert_eq!(outside.count, 2);
    assert_eq!((outside.chapter, outside.time_offset_secs), (None, None));

    let (ranges, ..) = build_bad_ranges(&mf, &title, 0.0);
    assert!(
        ranges.iter().all(|r| r.duration_ms == 0.0),
        "no bitrate means no duration, not inf"
    );
}
