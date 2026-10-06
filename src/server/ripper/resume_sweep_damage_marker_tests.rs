// Regression: resume_remux previously passed a zeroed
// SweepDamageSnapshot; RippedMarker now carries sweep_* fields.
// Verifies the round-trip serialization of those fields.
#[test]
fn ripped_marker_sweep_fields_round_trip() {
    let marker = crate::server::muxer::RippedMarker {
        schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
        iso_path: "/staging/Foo/Foo.iso".into(),
        mapfile_path: "/staging/Foo/Foo.iso.mapfile".into(),
        display_name: "Foo".into(),
        disc_format: "uhd".into(),
        mkv_filename: "Foo.mkv".into(),
        tmdb_title: "Foo".into(),
        tmdb_year: 2024,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        tmdb_media_type: String::new(),
        max_retries: 3,
        abort_on_lost_secs: 0,
        rip_elapsed_secs: 0.0,
        rip_errors: 0,
        rip_lost_video_secs: 1.23,
        rip_last_sector: 0,
        origin_device: "sg0".into(),
        sweep_errors: 77,
        sweep_total_lost_ms: 2500.0,
        sweep_main_lost_ms: 1200.0,
        sweep_num_bad_ranges: 5,
        sweep_largest_gap_ms: 900.0,
        title_confident: false,
    };

    // Serialize then deserialize (mirrors write_marker / read_marker).
    let json = serde_json::to_string(&marker).expect("serialize");
    let back: crate::server::muxer::RippedMarker =
        serde_json::from_str(&json).expect("deserialize");

    assert_eq!(back.sweep_errors, 77);
    assert!((back.sweep_total_lost_ms - 2500.0).abs() < 0.001);
    assert!((back.sweep_main_lost_ms - 1200.0).abs() < 0.001);
    assert_eq!(back.sweep_num_bad_ranges, 5);
    assert!((back.sweep_largest_gap_ms - 900.0).abs() < 0.001);
}

/// Backward-compat: a marker JSON without sweep_* fields (pre-v0.25.12)
/// must deserialize successfully with sweep_* defaulting to zero.
#[test]
fn ripped_marker_missing_sweep_fields_default_to_zero() {
    // JSON without any sweep_* keys — simulates an old marker on disk.
    let json = r#"{
            "schema_version": 1,
            "iso_path": "/staging/Bar/Bar.iso",
            "mapfile_path": "/staging/Bar/Bar.iso.mapfile",
            "display_name": "Bar",
            "disc_format": "bluray",
            "mkv_filename": "Bar.mkv",
            "tmdb_title": "Bar",
            "tmdb_year": 2020,
            "tmdb_poster": "",
            "tmdb_overview": "",
            "max_retries": 5,
            "abort_on_lost_secs": 30,
            "rip_elapsed_secs": 0.0,
            "rip_errors": 0,
            "rip_lost_video_secs": 0.0,
            "rip_last_sector": 0,
            "origin_device": "sg0"
        }"#;
    let marker: crate::server::muxer::RippedMarker =
        serde_json::from_str(json).expect("old marker must deserialize");
    // schema_version check is done by read_marker, not serde; skip it here.
    assert_eq!(marker.sweep_errors, 0, "missing field must default to 0");
    assert_eq!(
        marker.sweep_total_lost_ms, 0.0,
        "missing field must default to 0.0"
    );
    assert_eq!(
        marker.sweep_main_lost_ms, 0.0,
        "missing field must default to 0.0"
    );
    assert_eq!(
        marker.sweep_num_bad_ranges, 0,
        "missing field must default to 0"
    );
    assert_eq!(
        marker.sweep_largest_gap_ms, 0.0,
        "missing field must default to 0.0"
    );
    assert!(
        !marker.title_confident,
        "missing title_confident must default to false (prior match-check-only behavior)"
    );
}

// Regression: an operator title override must survive the.ripped hand-off so resume_remux
// auto-files into.done. Before the fix RippedMarker didn't carry the verdict.
#[test]
fn ripped_marker_title_confident_round_trips() {
    let mut marker = crate::server::muxer::RippedMarker {
        schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
        iso_path: "/staging/Baz/Baz.iso".into(),
        mapfile_path: "/staging/Baz/Baz.iso.mapfile".into(),
        display_name: "Operator Chosen Title".into(),
        disc_format: "uhd".into(),
        mkv_filename: "Operator_Chosen_Title.mkv".into(),
        tmdb_title: "Operator Chosen Title".into(),
        tmdb_year: 2024,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        tmdb_media_type: String::new(),
        max_retries: 3,
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
    };
    let json = serde_json::to_string(&marker).expect("serialize");
    let back: crate::server::muxer::RippedMarker =
        serde_json::from_str(&json).expect("deserialize");
    assert!(
        back.title_confident,
        "operator-confident verdict must survive the .ripped hand-off"
    );

    // And the low-confidence case round-trips as false.
    marker.title_confident = false;
    let json = serde_json::to_string(&marker).expect("serialize");
    let back: crate::server::muxer::RippedMarker =
        serde_json::from_str(&json).expect("deserialize");
    assert!(!back.title_confident);
}

// Regression: the resumed-rip done card must carry codecs, preferring
// the post-mux STATE value over the (possibly empty) pre-mux
// snapshot either way the done card must not be blank.
#[test]
fn resolve_done_codecs_prefers_post_mux_then_snapshot() {
    // _mux path: pre-mux snapshot empty, post-mux STATE has real codecs.
    assert_eq!(
        super::resolve_done_codecs(Some("HEVC · TrueHD".into()), String::new()),
        "HEVC · TrueHD",
        "post-mux codecs must win when present"
    );
    // User-triggered path: STATE empty post-mux, snapshot carries codecs.
    assert_eq!(
        super::resolve_done_codecs(Some(String::new()), "AVC · DTS".into()),
        "AVC · DTS",
        "empty post-mux STATE must fall back to the pre-mux snapshot"
    );
    // No STATE entry at all → snapshot.
    assert_eq!(
        super::resolve_done_codecs(None, "AVC · DTS".into()),
        "AVC · DTS",
        "absent STATE must fall back to the pre-mux snapshot"
    );
    // Both populated → post-mux is the fresher truth.
    assert_eq!(
        super::resolve_done_codecs(Some("HEVC".into()), "AVC".into()),
        "HEVC"
    );
}

// Regression: resume .done/.review markers omitted media_type, so
// the mover filed TV-show resumes under the movie library. Now
// resolve_media_type mirrors the mover's own default.
#[test]
fn resolve_media_type_defaults_empty_to_movie() {
    assert_eq!(
        super::resolve_media_type("tv"),
        "tv",
        "a carried TV media_type must survive into the marker, not collapse to movie"
    );
    assert_eq!(super::resolve_media_type("movie"), "movie");
    assert_eq!(
        super::resolve_media_type(""),
        "movie",
        "empty (cold resume) must resolve to the mover's own default"
    );
}

// Regression: check_and_mux's secondary done-state update was dropping the
// codec/duration/output_file badges because remux_from_ripped_marker returned a bare bool.
#[test]
fn mux_handoff_outcome_captures_mux_derived_fields() {
    // A private device key so this doesn't race the shared "_mux".
    let key = "_mux_test_capture";
    let bad_ranges = vec![
        super::super::state::BadRange {
            lba: 100,
            count: 32,
            duration_ms: 1500.0,
            chapter: Some(2),
            time_offset_secs: Some(42.0),
        },
        super::super::state::BadRange {
            lba: 5000,
            count: 8,
            duration_ms: 375.0,
            chapter: None,
            time_offset_secs: None,
        },
    ];
    super::super::update_state(
        key,
        super::super::RipState {
            device: key.to_string(),
            status: "done".to_string(),
            codecs: "HEVC · TrueHD".into(),
            duration: "2:14".into(),
            output_file: "/staging/Foo".into(),
            bad_ranges: bad_ranges.clone(),
            bad_ranges_truncated: 3,
            // Combined sweep + mux-time loss the `_mux` done-state writes:
            // these must be captured so the origin device's done card
            // reports real loss in the delivered MKV, not the sweep-only subset.
            errors: 7,
            lost_video_secs: 12.5,
            total_lost_ms: 12500.0,
            main_lost_ms: 9000.0,
            ..Default::default()
        },
    );

    // The capture remux_from_ripped_marker runs on a success.
    let mut outcome = super::MuxHandoffOutcome {
        success: true,
        ..Default::default()
    };
    let rs = super::super::STATE.lock().unwrap().remove(key).unwrap();
    super::apply_success_fields(&mut outcome, &rs);

    assert_eq!(outcome.codecs, "HEVC · TrueHD");
    assert_eq!(outcome.duration, "2:14");
    assert_eq!(outcome.output_file, "/staging/Foo");
    // The bad-ranges drilldown list + truncation count must survive the
    // capture so the origin device's done card isn't left with an empty
    // drilldown for a damaged disc.
    assert_eq!(outcome.bad_ranges.len(), 2);
    assert_eq!(outcome.bad_ranges[0].lba, 100);
    assert_eq!(outcome.bad_ranges[1].count, 8);
    assert_eq!(outcome.bad_ranges_truncated, 3);
    // Combined sweep + mux-time loss figures must survive the capture so
    // the origin device's done card reports the loss in the delivered MKV
    // (matching the `_mux` tile/webhook), not the sweep-only marker subset.
    assert_eq!(outcome.errors, 7);
    assert_eq!(outcome.lost_video_secs, 12.5);
    assert_eq!(outcome.total_lost_ms, 12500.0);
    assert_eq!(outcome.main_lost_ms, 9000.0);
    // STATE entry is cleaned up so the origin update can't read it later.
    assert!(super::super::STATE.lock().unwrap().get(key).is_none());
}
