//! Host-free smoke test for the web layer's state serialization.
//!
//! This verifies that `RipState` round-trips through serde_json the way
//! `get_state_json` produces it, and the UI's expected fields all survive.
//!
//! Route dispatch + device-name validation are NOT tested here: that coverage now lives against
//! the real `handle_request` in the in-crate `web::web_tests::http` module instead.

use freemkv::server::ripper::{BadRange, RipState};

#[test]
fn test_state_json_serialization_round_trip() {
    // Build a RipState with non-default values in every field we care
    // about, serialize it via serde_json (same crate get_state_json
    // uses), parse back as Value, verify the fields the dashboard reads.
    let state = RipState {
        device: "sg4".to_string(),
        status: "ripping".to_string(),
        disc_present: true,
        disc_name: "TEST DISC".to_string(),
        disc_label: "TEST_DISC_1".to_string(),
        failure_deferred: false,
        failure_finalize: false,
        failure_space: false,
        disc_format: "bluray".to_string(),
        progress_pct: 42,
        progress_gb: 13.5,
        speed_mbs: 27.3,
        eta: "0:14:23".to_string(),
        errors: 2,
        lost_video_secs: 0.125,
        last_sector: 12345,
        current_batch: 16,
        preferred_batch: 32,
        pass: 1,
        total_passes: 3,
        bytes_good: 14_500_000_000,
        bytes_maybe: 4096,
        bytes_lost: 1024,
        bytes_total_disc: 25_000_000_000,
        bad_ranges: vec![BadRange {
            lba: 1000,
            count: 50,
            duration_ms: 12.5,
            chapter: Some(7),
            time_offset_secs: Some(425.0),
        }],
        num_bad_ranges: 1,
        bad_ranges_truncated: 0,
        total_lost_ms: 12.5,
        main_lost_ms: 8.0,
        main_at_risk_ms: 50.0,
        largest_gap_ms: 12.5,
        loss_aborted: false,
        last_error: String::new(),
        output_file: "TEST DISC.mkv".to_string(),
        tmdb_title: "Test Disc".to_string(),
        tmdb_year: 2024,
        tmdb_poster: "https://image.tmdb.org/p/abc.jpg".to_string(),
        tmdb_overview: "An overview.".to_string(),
        tmdb_media_type: "movie".to_string(),
        duration: "1h 47m".to_string(),
        codecs: "H.264 1080p / DTS-HD MA 5.1".to_string(),
        pass_progress_pct: 42,
        pass_eta: "0:14:23".to_string(),
        total_progress_pct: 18,
        total_eta: "1:23:45".to_string(),
        damage_severity: "cosmetic".to_string(),
        failure_reason: None,
        started_epoch_secs: 0,
        key_status: String::new(),
        resumable: None,
        claim_gen: 0,
    };

    // Serialize the same way get_state_json does: serde_json::to_value.
    let v = serde_json::to_value(&state).expect("RipState must serialize");

    assert_eq!(v["device"], "sg4");
    assert_eq!(v["status"], "ripping");
    assert_eq!(v["disc_present"], true);
    assert_eq!(v["disc_name"], "TEST DISC");
    // `disc_label` is server-side bookkeeping (which staging dir this disc
    // owns), not dashboard data — it must stay out of the wire format.
    assert!(
        v.get("disc_label").is_none(),
        "disc_label must not be serialized into the state JSON"
    );
    assert_eq!(v["disc_format"], "bluray");
    assert_eq!(v["progress_pct"], 42);
    assert!(
        (v["progress_gb"].as_f64().unwrap() - 13.5).abs() < 1e-9,
        "progress_gb didn't round-trip: {:?}",
        v["progress_gb"]
    );
    assert!((v["speed_mbs"].as_f64().unwrap() - 27.3).abs() < 1e-9);
    assert_eq!(v["eta"], "0:14:23");
    assert_eq!(v["errors"], 2);
    assert_eq!(v["last_sector"], 12345);
    assert_eq!(v["pass"], 1);
    assert_eq!(v["total_passes"], 3);
    assert_eq!(v["bytes_good"], 14_500_000_000u64);
    assert_eq!(v["bytes_total_disc"], 25_000_000_000u64);
    assert_eq!(v["num_bad_ranges"], 1);
    assert_eq!(v["bad_ranges_truncated"], 0);
    assert_eq!(v["output_file"], "TEST DISC.mkv");
    assert_eq!(v["tmdb_title"], "Test Disc");
    assert_eq!(v["tmdb_year"], 2024);
    assert_eq!(v["duration"], "1h 47m");
    assert_eq!(v["codecs"], "H.264 1080p / DTS-HD MA 5.1");

    // bad_ranges is an array of objects with the documented fields.
    let bad = v["bad_ranges"]
        .as_array()
        .expect("bad_ranges must be array");
    assert_eq!(bad.len(), 1);
    assert_eq!(bad[0]["lba"], 1000);
    assert_eq!(bad[0]["count"], 50);
    assert_eq!(bad[0]["chapter"], 7);

    // Round-trip: stringify and re-parse, then hit the same fields.
    let s = v.to_string();
    let v2: serde_json::Value =
        serde_json::from_str(&s).expect("get_state_json output must re-parse");
    assert_eq!(v2["device"], "sg4");
    assert_eq!(v2["bytes_good"], 14_500_000_000u64);
    assert_eq!(v2["bad_ranges"][0]["lba"], 1000);
}

// Route dispatch, device-name validation, and the SSE route are now driven
// against the REAL handle_request in src/web.rs (mod web_tests::http) — see
// the module doc comment above. No replica dispatcher lives here.
