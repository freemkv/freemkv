//! The resume mux path (`remux_from_ripped_marker` -> `resume_remux`)
//! must honor the SAME done + eject + queue-membership contract as the
//! fresh-rip path: eject routes through `should_auto_eject`, and on
//! success the dir writes `.done`/`.review` + `.completed` and deletes
//! `.ripped`, landing in the Move queue only. These tests pin the
//! marker-state outcomes without standing up a real ISO + mux pipeline.
use crate::server::ripper::staging;
use tempfile::TempDir;

// On a CONFIDENT resume success the dir holds .done + .completed
// (and .ripped deleted). pending_queue must skip it (Move queue only).
#[test]
fn resume_success_marker_state_is_move_queue_only() {
    let tmp = TempDir::new().unwrap();
    let disc = tmp.path().join("Resumed_Title");
    std::fs::create_dir_all(&disc).unwrap();
    // Post-resume-success marker state (the .ripped delete succeeded).
    std::fs::write(disc.join(staging::DONE_MARKER), b"{}").unwrap();
    staging::write_completed_marker(&disc);

    // Mux queue: must NOT contain it (no .ripped, and .done/.completed
    // are terminal/move-queue markers anyway).
    let mux = crate::server::muxer::pending_queue(tmp.path());
    assert!(
        mux.is_empty(),
        "a resumed-and-completed dir must not be (queued) for mux"
    );

    // The snapshot reports completed → the mux worker won't re-dispatch.
    let snap = staging::snapshot_staging_disc(&disc).expect("populated dir yields snapshot");
    assert!(
        snap.completed,
        "resume success must leave .completed for the mover"
    );
}

// If the post-resume .ripped delete FAILS (NFS), the dir still holds
// .ripped + .done + .completed; it must STILL be Move-queue only and
// the mux worker's dispatch verdict must be terminal (no re-mux).
#[test]
fn resume_success_with_lingering_ripped_is_still_move_only() {
    let tmp = TempDir::new().unwrap();
    let disc = tmp.path().join("Resumed_Title");
    std::fs::create_dir_all(&disc).unwrap();
    crate::server::muxer::write_marker(
        &disc,
        &crate::server::muxer::RippedMarker {
            schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
            iso_path: "/x/Resumed_Title/Resumed_Title.iso".into(),
            mapfile_path: "/x/Resumed_Title/Resumed_Title.iso.mapfile".into(),
            display_name: "Resumed Title".into(),
            disc_format: "uhd".into(),
            mkv_filename: "Resumed_Title.mkv".into(),
            tmdb_title: "Resumed Title".into(),
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
    std::fs::write(disc.join(staging::DONE_MARKER), b"{}").unwrap();
    staging::write_completed_marker(&disc);

    let mux = crate::server::muxer::pending_queue(tmp.path());
    assert!(
        mux.is_empty(),
        "a completed resume must be Move-queue only even if .ripped lingers, got {mux:?}"
    );
    let snap = staging::snapshot_staging_disc(&disc).expect("snapshot");
    assert_eq!(
        crate::server::muxer::mux_dispatch_verdict(Some(&snap)),
        crate::server::muxer::MuxVerdict::SkipTerminal,
        "the mux worker must treat a completed dir as terminal, never re-dispatch"
    );
}

/// A LOW-CONFIDENCE resume success writes `.review` (not `.done`) +
/// `.completed`. Same mutual-exclusion outcome: never in the Mux queue.
#[test]
fn resume_review_success_is_not_in_mux_queue() {
    let tmp = TempDir::new().unwrap();
    let disc = tmp.path().join("Held_Resume");
    std::fs::create_dir_all(&disc).unwrap();
    std::fs::write(disc.join(staging::REVIEW_MARKER), b"{}").unwrap();
    staging::write_completed_marker(&disc);

    let mux = crate::server::muxer::pending_queue(tmp.path());
    assert!(
        mux.is_empty(),
        "a .review+.completed resume must not be (queued) for mux"
    );
}
