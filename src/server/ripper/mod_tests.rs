//! Tests for orchestrator-level helpers that live in this file.
//! State-only helpers and their tests live in `state.rs`.

use super::{
    FmtsGate, FmtsGatePlan, HaltGuard, PatchDecision, SweepingGuard, aacs_failure_message,
    bad_sector_statuses, disk_space_preflight_message, disk_space_required_bytes,
    end_of_recovery_promotion, fmts_gate_decision, fmts_gate_plan, format_lib_error,
    format_pass_error, header_phase_disposition, incomplete_mux_status, is_fmts_key_missing_error,
    is_safe_staging_segment, list_staging_basenames, patch_made_progress, patch_pass_decision,
    plan_passes, pre_pass_converged, prune_intermediate_iso, register_halt, resumable_dir_blocked,
    resumable_for_disc, resume_remaining_iso_bytes, scope_bad_bytes, scope_converged,
    skip_diskcheck_value, staging_dir_matches_disc, staging_disc_owned_by_worker,
    staging_free_bytes,
};
use crate::server::ripper::session::device_halt;
use crate::server::ripper::staging;
use crate::server::ripper::state::Resumable;
use crate::server::util::MILLIS_PER_SEC;
use libfreemkv::{Error, ScsiSense};

/// Build a single-title `Disc` whose main-feature title carries the given
/// streams. Only `titles[0].streams` matters for `disc_is_3d` /
/// `output_extension_for`; everything else is a minimal, valid skeleton.
fn disc_with_main_streams(streams: Vec<libfreemkv::Stream>) -> libfreemkv::Disc {
    let mut title = libfreemkv::DiscTitle::empty();
    title.streams = streams;
    libfreemkv::Disc {
        volume_id: "TEST_DISC".into(),
        meta_title: None,
        format: libfreemkv::DiscFormat::BluRay,
        capacity_sectors: 0,
        capacity_bytes: 0,
        layers: 1,
        titles: vec![title],
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted: false,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    }
}

/// An encrypted disc with NO usable keys — the state `key_readiness` /
/// `keyless_failure_message` exist to describe. `aacs`/`css` are `None`, so
/// `decrypt_keys()` reports `None` and the "no keys" branches are taken.
fn encrypted_keyless_disc() -> libfreemkv::Disc {
    let mut disc = disc_with_main_streams(vec![]);
    disc.encrypted = true;
    disc
}

/// A plain 2D H.264 base-view video stream (not MVC-dependent).
fn video_2d() -> libfreemkv::Stream {
    libfreemkv::Stream::Video(libfreemkv::disc::VideoStream {
        pid: 0x1011,
        codec: libfreemkv::disc::Codec::H264,
        resolution: libfreemkv::disc::Resolution::R1080p,
        frame_rate: libfreemkv::disc::FrameRate::F24,
        hdr: libfreemkv::disc::HdrFormat::Sdr,
        color_space: libfreemkv::disc::ColorSpace::Bt709,
        display_aspect: None,
        secondary: false,
        label: String::new(),
        measured_cicp: None,
    })
}

/// The MVC dependent (right-eye) view — the presence of this in the main
/// feature is what marks a Blu-ray 3D rip. `is_mvc_dependent()` keys off
/// the exact `MVC_DEPENDENT_LABEL`, so we set that label.
fn video_mvc_dependent() -> libfreemkv::Stream {
    libfreemkv::Stream::Video(libfreemkv::disc::VideoStream {
        pid: 0x1012,
        codec: libfreemkv::disc::Codec::H264,
        resolution: libfreemkv::disc::Resolution::R1080p,
        frame_rate: libfreemkv::disc::FrameRate::F24,
        hdr: libfreemkv::disc::HdrFormat::Sdr,
        color_space: libfreemkv::disc::ColorSpace::Bt709,
        display_aspect: None,
        secondary: true,
        label: libfreemkv::disc::MVC_DEPENDENT_LABEL.to_string(),
        measured_cicp: None,
    })
}

/// `disc_is_3d` is true iff the main feature carries an MVC-dependent view.
#[test]
fn disc_is_3d_detects_mvc_dependent_main_feature() {
    assert!(
        super::disc_is_3d(&disc_with_main_streams(vec![
            video_2d(),
            video_mvc_dependent(),
        ])),
        "a main feature with an MVC dependent view is a 3D rip"
    );
    assert!(
        !super::disc_is_3d(&disc_with_main_streams(vec![video_2d()])),
        "a plain base-view-only main feature is not 3D"
    );
    assert!(
        !super::disc_is_3d(&libfreemkv::Disc {
            titles: Vec::new(),
            ..disc_with_main_streams(vec![])
        }),
        "an empty-titles disc must be treated as not-3D, not panic"
    );
}

/// `output_extension_for` picks `mk3d` for a 3D main feature, `mkv`
/// otherwise, and `m2ts` always overrides (passthrough wins over 3D).
#[test]
fn output_extension_for_maps_3d_and_format_override() {
    let disc_3d = disc_with_main_streams(vec![video_2d(), video_mvc_dependent()]);
    let disc_2d = disc_with_main_streams(vec![video_2d()]);
    let disc_empty = libfreemkv::Disc {
        titles: Vec::new(),
        ..disc_with_main_streams(vec![])
    };

    // 3D main feature → mk3d.
    assert_eq!(super::output_extension_for("mkv", &disc_3d), "mk3d");
    // Non-3D → mkv.
    assert_eq!(super::output_extension_for("mkv", &disc_2d), "mkv");
    // m2ts passthrough overrides even for a 3D disc.
    assert_eq!(super::output_extension_for("m2ts", &disc_3d), "m2ts");
    // Empty-titles disc → mkv, no panic.
    assert_eq!(super::output_extension_for("mkv", &disc_empty), "mkv");
}

/// `output_scheme_for` is the URL SCHEME (container), never the `mk3d` filename
/// extension: a 3D rip muxes through `mkv://` since libfreemkv has no `mk3d://`
/// scheme (building `mk3d://…` fails the mux with `StreamUrlInvalid`).
#[test]
fn output_scheme_for_never_returns_mk3d() {
    // A 3D disc still yields the `mkv` scheme even though its extension is mk3d.
    assert_eq!(
        super::output_extension_for("mkv", &disc_3d_for_scheme()),
        "mk3d"
    );
    assert_eq!(super::output_scheme_for("mkv"), "mkv");
    // m2ts stays m2ts; any other/unknown format falls back to the mkv scheme.
    assert_eq!(super::output_scheme_for("m2ts"), "m2ts");
    assert_eq!(super::output_scheme_for("iso"), "mkv");
    // The scheme is a valid libfreemkv output scheme — never the mk3d suffix.
    assert_ne!(super::output_scheme_for("mkv"), "mk3d");
}

fn disc_3d_for_scheme() -> libfreemkv::Disc {
    disc_with_main_streams(vec![video_2d(), video_mvc_dependent()])
}

/// The pre-rip FMTS gate honours `capture_without_keys` exactly like the base
/// no-keys gate: a resolved map always rips; an unresolved one captures the raw
/// ISO when the operator opted in, else skips the disc.
#[test]
fn fmts_gate_decision_honors_capture_without_keys() {
    // Complete map → rip normally, regardless of the capture toggle.
    assert_eq!(fmts_gate_decision(true, false), FmtsGate::Proceed);
    assert_eq!(fmts_gate_decision(true, true), FmtsGate::Proceed);
    // Incomplete map: capture-without-keys ON → capture ISO; OFF → skip.
    assert_eq!(fmts_gate_decision(false, true), FmtsGate::CaptureOnly);
    assert_eq!(fmts_gate_decision(false, false), FmtsGate::Skip);
}

// The FMTS gate's side-effect routing (defects 1 + 2): pins CaptureOnly→defer,
// Skip→quarantine, Proceed→neither.
#[test]
fn fmts_gate_plan_routes_side_effects() {
    assert_eq!(
        fmts_gate_plan(FmtsGate::Proceed),
        FmtsGatePlan {
            defer_forensic_mux: false,
            quarantine: false,
        },
        "Proceed rips normally — no deferral, no quarantine"
    );
    assert_eq!(
        fmts_gate_plan(FmtsGate::CaptureOnly),
        FmtsGatePlan {
            defer_forensic_mux: true,
            quarantine: false,
        },
        "CaptureOnly must defer the forensic mux (capture ISO now), not quarantine"
    );
    assert_eq!(
        fmts_gate_plan(FmtsGate::Skip),
        FmtsGatePlan {
            defer_forensic_mux: false,
            quarantine: true,
        },
        "Skip must quarantine the staging dir, not set the deferral flag"
    );
}

// The FMTS-forensic-key-missing error classifier (defect 1, resume half): must match only
// the leading `E<code>` token, never a substring.
#[test]
fn is_fmts_key_missing_error_matches_only_the_leading_code_token() {
    let fmts: std::io::Error = libfreemkv::Error::FmtsKeyMissing.into();
    assert!(
        is_fmts_key_missing_error(&fmts),
        "Error::FmtsKeyMissing must classify as an FMTS-key-missing error"
    );
    // A user Stop is NOT an FMTS-key-missing error (must not be deferred as one).
    let halted: std::io::Error = libfreemkv::Error::Halted.into();
    assert!(!is_fmts_key_missing_error(&halted));
    // A payload that merely CONTAINS the digits must not false-match.
    let other = std::io::Error::other(format!(
        "E7022: disc-hash …E{}…",
        libfreemkv::error::E_FMTS_KEY_MISSING
    ));
    assert!(!is_fmts_key_missing_error(&other));
}

// Convergence H1 regression: `SweepingGuard::drop` must clear `.sweeping` on every exit
// path, or a leaked marker strands the dir `InProgress` forever.
#[test]
fn sweeping_guard_clears_marker_on_drop() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    staging::write_sweeping_marker(&dir);
    assert_eq!(
        staging::read_state(&dir).map(|s| s.state),
        Some(staging::StagingState::Sweeping),
        "Sweeping state should be present before the guard drops"
    );
    {
        let _guard = SweepingGuard {
            staging: dir.clone(),
        };
        // Still present inside the guard's scope (mirrors the live
        // sweep+patch window).
        assert_eq!(
            staging::read_state(&dir).map(|s| s.state),
            Some(staging::StagingState::Sweeping)
        );
    }
    // Guard dropped at scope end (the early-return / panic case) — Sweeping
    // state gone, so the restart scan won't strand this dir `InProgress`.
    assert!(
        staging::read_state(&dir).map(|s| s.state) != Some(staging::StagingState::Sweeping),
        "Sweeping state must be cleared when SweepingGuard drops"
    );
}

// Convergence H1: on success/`.failed` paths a terminal writer
// already clears `.sweeping`, so the guard's clear must be an
// idempotent no-op that doesn't disturb the terminal marker.
#[test]
fn sweeping_guard_is_idempotent_after_terminal_marker() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    staging::write_sweeping_marker(&dir);
    {
        let _guard = SweepingGuard {
            staging: dir.clone(),
        };
        // Terminal write (e.g. `.failed`) supersedes `Sweeping` first, as on
        // the real quarantine paths.
        staging::write_failed_marker(&dir, "boom");
        assert_eq!(
            staging::read_state(&dir).map(|s| s.state),
            Some(staging::StagingState::Failed)
        );
    }
    // Guard drop is a no-op: the terminal `Failed` state survives (the
    // guard's clear only fires when state == Sweeping).
    assert_eq!(
        staging::read_state(&dir).map(|s| s.state),
        Some(staging::StagingState::Failed),
        "guard drop must not remove the terminal Failed state"
    );
}

// Regression guard for the divergent disk-reclamation bug: inline and resume completion
// paths share `prune_intermediate_iso` so `keep_iso=false` frees the ISO on BOTH routes.
#[test]
fn prune_removes_iso_and_mapfile_when_keep_iso_false() {
    let tmp = tempfile::TempDir::new().unwrap();
    let iso = tmp.path().join("Movie.iso");
    let map = tmp.path().join("Movie.iso.mapfile");
    std::fs::write(&iso, b"iso").unwrap();
    std::fs::write(&map, b"map").unwrap();

    prune_intermediate_iso(
        "sr0", &iso, &map, /* max_retries */ 1, /* keep_iso */ false,
    );

    assert!(!iso.exists(), "ISO must be pruned when keep_iso=false");
    assert!(!map.exists(), "mapfile must be pruned when keep_iso=false");
}

#[test]
fn prune_keeps_iso_and_mapfile_when_keep_iso_true() {
    let tmp = tempfile::TempDir::new().unwrap();
    let iso = tmp.path().join("Movie.iso");
    let map = tmp.path().join("Movie.iso.mapfile");
    std::fs::write(&iso, b"iso").unwrap();
    std::fs::write(&map, b"map").unwrap();

    prune_intermediate_iso(
        "sr0", &iso, &map, /* max_retries */ 1, /* keep_iso */ true,
    );

    assert!(iso.exists(), "ISO must be retained when keep_iso=true");
    assert!(map.exists(), "mapfile must be retained when keep_iso=true");
}

#[test]
fn prune_is_noop_in_direct_mode() {
    // max_retries == 0 is direct mode: no intermediate ISO is ever
    // produced, so the prune must not touch unrelated files.
    let tmp = tempfile::TempDir::new().unwrap();
    let iso = tmp.path().join("Movie.iso");
    let map = tmp.path().join("Movie.iso.mapfile");
    std::fs::write(&iso, b"iso").unwrap();
    std::fs::write(&map, b"map").unwrap();

    prune_intermediate_iso(
        "sr0", &iso, &map, /* max_retries */ 0, /* keep_iso */ false,
    );

    assert!(iso.exists(), "direct mode (max_retries=0) must not prune");
    assert!(map.exists(), "direct mode (max_retries=0) must not prune");
}

#[test]
fn prune_tolerates_already_absent_files() {
    // NotFound is silent: re-running prune, or a path where the mover
    // already relocated/removed the ISO, must not error.
    let tmp = tempfile::TempDir::new().unwrap();
    let iso = tmp.path().join("Gone.iso");
    let map = tmp.path().join("Gone.iso.mapfile");
    // Neither file exists.
    prune_intermediate_iso("sr0", &iso, &map, 1, false);
    assert!(!iso.exists());
    assert!(!map.exists());
}

// Resume/completion matching is EXACT, never prefix ("Redshift" vs "Redshift_2"). Locks in
// the already-fixed HIGH bug.
#[test]
fn staging_match_is_exact_not_prefix() {
    // Direct predicate: exact equality only.
    assert!(staging_dir_matches_disc("Redshift", "Redshift"));
    assert!(!staging_dir_matches_disc("Redshift_2", "Redshift"));
    assert!(!staging_dir_matches_disc("Redshift", "Redshift_2"));
    assert!(!staging_dir_matches_disc("Redshift_2_Extras", "Redshift_2"));

    // End-to-end over a real temp staging dir: both "Redshift" and "Redshift_2"
    // exist; scanning with the production predicate must select ONLY the
    // exact "Redshift".
    let tmp = tempfile::TempDir::new().unwrap();
    for name in ["Redshift", "Redshift_2"] {
        std::fs::create_dir_all(tmp.path().join(name)).unwrap();
    }
    let sanitized = "Redshift";
    let matches: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .flatten()
        .filter_map(|e| {
            e.path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .filter(|basename| staging_dir_matches_disc(basename, sanitized))
        .collect();
    assert_eq!(
        matches,
        vec!["Redshift".to_string()],
        "only the exact 'Redshift' dir must match, not the 'Redshift_2' sibling"
    );
}

// Regression: `output_opened=false` + `finalize_error=Some` must classify as a terminal
// failure (quarantine) carrying its reason; `None` stays resumable.
#[test]
fn header_phase_finalize_error_is_terminal_failure() {
    use super::HeaderPhase;
    for reason in ["header buffer exceeded cap", "header resolution incomplete"] {
        assert_eq!(
            header_phase_disposition(false, Some(reason)),
            HeaderPhase::Failed(reason),
            "output never opened with a finalize_error must be a terminal failure"
        );
    }
    assert_eq!(
        header_phase_disposition(false, None),
        HeaderPhase::ResumableStop,
        "a clean header-phase stop (halt) must stay resumable, not quarantined"
    );
    // output_opened=true is handled by the post-finalize path, never this branch.
    assert_eq!(header_phase_disposition(true, None), HeaderPhase::Produced);
    assert_eq!(
        header_phase_disposition(true, Some("post-mux finalize error")),
        HeaderPhase::Produced
    );
}

// Regression: a hard read error must map to `status="error"` with a non-empty cause, not
// the silent "stopped → idle" halt path.
#[test]
fn read_error_surfaces_as_error_status_not_silent_idle() {
    // A read-error truncation: status="error", reason names the cause.
    let (log_prefix, status, reason) =
        incomplete_mux_status(None, Some("E7015 read failed at LBA 42"));
    assert_eq!(status, "error");
    let reason = reason.expect("read error must carry a failure_reason / last_error");
    assert!(
        reason.contains("E7015 read failed at LBA 42"),
        "failure_reason must name the read-error cause, got: {reason}"
    );
    assert!(log_prefix.contains("read error"));

    // A genuine user halt (no finalize_error, no read_error) stays the
    // pre-existing silent stop → idle with no last_error.
    let (_, status, reason) = incomplete_mux_status(None, None);
    assert_eq!(status, "idle");
    assert!(
        reason.is_none(),
        "a user halt must NOT fabricate a failure_reason"
    );

    // A structural finalize error still wins over a read error (broken
    // file on disk is the stronger signal → quarantine path).
    let (_, status, reason) =
        incomplete_mux_status(Some("cues seek-back failed"), Some("read error too"));
    assert_eq!(status, "failed");
    assert!(reason.unwrap().contains("cues seek-back failed"));
}

// `staging_free_bytes`: a missing/unmounted path must yield `None` (diagnostic-log branch,
// not silent skip); a real path yields `Some`.
#[test]
fn staging_free_bytes_none_for_missing_path_some_for_real() {
    // Nonexistent path → statvfs fails → None (drives the else/warn
    // branch in the rip_disc preflight).
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist-staging-volume");
    assert!(
        staging_free_bytes(&missing.to_string_lossy()).is_none(),
        "a missing staging path must return None so the preflight logs \
             'skipped' rather than silently proceeding"
    );

    // A real, existing directory → Some(free bytes): unix via statvfs,
    // Windows via GetDiskFreeSpaceExW. Only the bare-fallback stub (neither
    // unix nor windows) returns None, so assert Some on both real targets.
    #[cfg(any(unix, windows))]
    assert!(
        staging_free_bytes(&tmp.path().to_string_lossy()).is_some(),
        "an existing staging path must return Some(free_bytes)"
    );
}

// `HaltGuard` must unregister the device's halt-map entry on EVERY exit path (the v0.13.6
// leak class).
#[test]
fn halt_guard_unregisters_on_drop() {
    let device = "sg_haltguard_drop_test";
    // Clean any residue from a prior run so the assertion is meaningful.
    super::unregister_halt(device);
    register_halt(device, libfreemkv::Halt::new());
    assert!(
        device_halt(device).is_some(),
        "halt entry should be registered before the guard drops"
    );
    {
        let _guard = HaltGuard {
            device: device.to_string(),
        };
        // Simulate an early-return error path: leaving this scope drops
        // the guard, which must run `unregister_halt`.
    }
    assert!(
        device_halt(device).is_none(),
        "HaltGuard::drop must unregister the halt-map entry on every exit path"
    );
}

/// The staging-segment guard must reject anything that could escape
/// or resolve to the staging root, so a hostile disc label can never
/// drive `remove_dir_all` outside staging.
#[test]
fn staging_segment_guard_rejects_traversal() {
    // Dangerous: traversal, current-dir, all-dots, empty, separators,
    // absolute.
    for bad in [
        "",
        ".",
        "..",
        "...",
        "/",
        "..\\",
        "a/b",
        "a\\b",
        "/etc",
        "../sibling",
        "./foo",
    ] {
        assert!(
            !is_safe_staging_segment(bad),
            "{bad:?} must be rejected as a staging segment"
        );
    }
    // Safe: ordinary sanitized title names (dots inside a name are
    // fine as long as the whole segment isn't only dots).
    for ok in [
        "Wraithline (2021)",
        "Redline.Chaser (1982)",
        "untitled",
        "A.Movie.With.Dots",
        "disc",
    ] {
        assert!(
            is_safe_staging_segment(ok),
            "{ok:?} must be accepted as a staging segment"
        );
    }
}

/// Build a minimal `DiscTitle` whose single extent spans `[start_lba,
/// start_lba + sector_count)`. Only `extents` matters for
/// `bytes_bad_in_title` / the abort-loss scoping.
fn test_title(start_lba: u32, sector_count: u32) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        playlist: "00800.mpls".to_string(),
        playlist_id: 800,
        duration_secs: 7200.0,
        size_bytes: (sector_count as u64) * 2048,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![libfreemkv::disc::Extent {
            start_lba,
            sector_count,
        }],
        content_format: libfreemkv::disc::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

// The staged image is muxed at the drive title's playlist, wherever the
// image scan lists it; an unknown playlist falls back to the first title.
#[test]
fn image_title_index_follows_the_drive_titles_playlist() {
    let mut image = disc_with_main_streams(vec![]);
    let mut other = test_title(0, 10);
    other.playlist = "00001.mpls".into();
    image.titles = vec![other, test_title(0, 10)];
    assert_eq!(super::image_title_index(&image, &test_title(0, 10)), 1);
    let mut unknown = test_title(0, 10);
    unknown.playlist = "00999.mpls".into();
    assert_eq!(super::image_title_index(&image, &unknown), 0);
    unknown.playlist.clear();
    assert_eq!(super::image_title_index(&image, &unknown), 0);
}

/// The scoped loss is the TOTAL of all in-title gaps, not the single
/// largest one (the old `fold(.., f64::max)` bug). Many scattered
/// small gaps must accumulate against the threshold.
#[test]
fn abort_loss_sums_scattered_in_title_gaps() {
    // Title covers bytes [0, 100_000_000) (sectors 0..~48829).
    let title = test_title(0, 48_829);
    let bps = 1_000_000.0; // 1 byte == 1 us

    // 50 scattered 1 MB gaps inside the title = 50 MB total.
    // At 1 MB/s that is 50 s == 50_000 ms.
    let bad: Vec<(u64, u64)> = (0..50).map(|i| (i * 1_500_000u64, 1_000_000u64)).collect();
    let lost = freemkv_engine::abort_lost_ms(false, &title, &bad, bps);
    // Old fold-max would have reported ~1000 ms (one gap); sum is 50x.
    assert!(
        (lost - 50_000.0).abs() < 1.0,
        "expected ~50_000 ms total, got {lost}"
    );

    // ISO output is whole-disc: same bad ranges sum regardless of
    // title scoping.
    let lost_iso = freemkv_engine::abort_lost_ms(true, &title, &bad, bps);
    assert!((lost_iso - 50_000.0).abs() < 1.0, "iso whole-disc sum");
}

// CHARACTERIZATION TESTS pinning the multipass recovery loop's current behavior.
#[test]
fn char_pass_ordering_sweep_then_n_patch() {
    // Single-pass: direct disc→MKV, no ISO intermediate.
    let single = plan_passes(0);
    assert!(!single.multipass, "max_retries=0 is single-pass (no ISO)");
    assert_eq!(single.sweep_passes, 0, "single-pass runs no sweep pass");
    assert_eq!(single.patch_passes, 0, "single-pass runs no patch passes");
    assert_eq!(single.total_passes, 0, "single-pass reports 0 total passes");

    // Multipass: 1 sweep + N patch, total = N + 2 (sweep + N + mux).
    for n in 1u8..=10 {
        let plan = plan_passes(n);
        assert!(plan.multipass, "max_retries={n} is multipass");
        assert_eq!(plan.sweep_passes, 1, "exactly one Pass-1 sweep");
        assert_eq!(
            plan.patch_passes, n,
            "exactly {n} patch passes for max_retries={n}"
        );
        assert_eq!(
            plan.total_passes,
            n + 2,
            "total = sweep(1) + patch({n}) + mux(1)"
        );
    }
}

// SCOPE-AWARE CONVERGENCE — MKV: only bad bytes INSIDE the muxed title count; converges
// when in-title bad == 0 regardless of out-of-title damage.
#[test]
fn char_convergence_mkv_scopes_to_title() {
    // Title covers bytes [0, 100 MB) — sectors [0, 48829).
    let title = test_title(0, 48_829);

    // Bad range OUTSIDE the title extents (well past 100 MB).
    let out_of_title = [(500_000_000u64, 2048u64)];
    let bad = scope_bad_bytes(false /* MKV */, &out_of_title, &title);
    assert_eq!(bad, 0, "out-of-title damage is not counted for MKV output");
    assert!(
        scope_converged(bad),
        "MKV converges when in-title scope is clean, despite out-of-title damage"
    );

    // Bad range INSIDE the title extents blocks convergence.
    let in_title = [(1_000_000u64, 2048u64)];
    let bad_in = scope_bad_bytes(false, &in_title, &title);
    assert_eq!(bad_in, 2048, "in-title damage is counted for MKV output");
    assert!(
        !scope_converged(bad_in),
        "MKV does not converge while the muxed title still has bad bytes"
    );
}

// SCOPE-AWARE CONVERGENCE — ISO: EVERY bad byte counts (whole-disc
// deliverable); converges only when the whole disc is clean.
#[test]
fn char_convergence_iso_scopes_whole_disc() {
    let title = test_title(0, 48_829);

    // The SAME out-of-title damage that MKV ignores blocks ISO convergence.
    let out_of_title = [(500_000_000u64, 2048u64)];
    let bad = scope_bad_bytes(true /* ISO */, &out_of_title, &title);
    assert_eq!(
        bad, 2048,
        "ISO counts out-of-title damage (whole-disc scope)"
    );
    assert!(
        !scope_converged(bad),
        "ISO does not converge while ANY sector on the disc is bad"
    );

    // A perfectly clean disc converges.
    assert!(
        scope_converged(scope_bad_bytes(true, &[], &title)),
        "ISO converges only when the whole disc is clean"
    );
}

// NO-PROGRESS EXHAUSTION: a patch pass recovering zero bytes stops
// the retry loop; any recovery keeps going.
#[test]
fn char_no_progress_stops_retries() {
    assert!(
        !patch_made_progress(0),
        "recovered==0 is no progress → stop"
    );
    assert!(patch_made_progress(1), "any recovery keeps retrying");
    assert!(patch_made_progress(2048), "any recovery keeps retrying");
}

// Unified convergence decision: scope_bad==0 ⇒ Converged; else recovered==Some(0) ⇒
// NoProgress, else ⇒ Continue.
#[test]
fn char_patch_pass_decision_matrix() {
    // Converged dominates — scope clean means stop regardless of recovery.
    assert_eq!(patch_pass_decision(0, None), PatchDecision::Converged);
    assert_eq!(patch_pass_decision(0, Some(0)), PatchDecision::Converged);
    assert_eq!(patch_pass_decision(0, Some(999)), PatchDecision::Converged);
    // Scope still bad, no pass run yet (loop-top) → keep going.
    assert_eq!(patch_pass_decision(2048, None), PatchDecision::Continue);
    // Scope still bad, last pass recovered nothing → exhausted.
    assert_eq!(
        patch_pass_decision(2048, Some(0)),
        PatchDecision::NoProgress
    );
    // Scope still bad, last pass made progress → keep going.
    assert_eq!(
        patch_pass_decision(2048, Some(4096)),
        PatchDecision::Continue
    );
}

// FAIL-OPEN GUARD: an empty mapfile (0 good, 0 bad) has zero bad bytes but must NOT read as
// a complete rip; `pre_pass_converged` adds `bytes_good > 0` on top of the bare decision.
#[test]
fn char_pre_pass_converged_requires_real_coverage() {
    // Empty mapfile: 0 good, 0 bad. Bare decision says Converged, but the
    // guarded gate must NOT — nothing was read, so run the pass.
    assert_eq!(patch_pass_decision(0, None), PatchDecision::Converged);
    assert!(
        !pre_pass_converged(Some(0), 0),
        "empty mapfile (0 good, 0 bad) must NOT be treated as converged"
    );
    // Genuinely-complete scope: good spans the scope, zero bad → converged,
    // so redundant passes are still skipped.
    assert!(
        pre_pass_converged(Some(0), 4096),
        "complete scope (good>0, bad==0) must still converge"
    );
    // Scope still bad → never converged regardless of good coverage.
    assert!(!pre_pass_converged(Some(2048), 4096));
    assert!(!pre_pass_converged(Some(2048), 0));
}

// PROMOTION DECISION: end-of-recovery promotes NonTrimmed → Unreadable before the abort
// gate.
#[test]
fn char_promotion_nontrimmed_to_unreadable() {
    use freemkv_engine::SectorStatus;
    assert_eq!(
        end_of_recovery_promotion(),
        (
            &[SectorStatus::NonTrimmed, SectorStatus::NonScraped][..],
            SectorStatus::Unreadable,
        ),
        "end-of-recovery promotion is NonTrimmed → Unreadable"
    );
    // Both the promoted-from and promoted-to statuses count as "still bad"
    // for the scope/convergence check (only Finished is good).
    let bad_set = bad_sector_statuses();
    assert!(bad_set.contains(&SectorStatus::NonTrimmed));
    assert!(bad_set.contains(&SectorStatus::Unreadable));
    assert!(
        !bad_set.contains(&SectorStatus::Finished),
        "Finished is the only status that leaves the bad set"
    );
}

// PROMOTION end-to-end: drives a real mapfile through promotion so a NonTrimmed range
// becomes Unreadable and feeds the abort gate. No drive required.
#[test]
fn char_promotion_finalizes_loss_for_abort_gate() {
    use freemkv_engine::Mapfile;

    let tmp = tempfile::tempdir().unwrap();
    let mf_path = tmp.path().join("promote.mapfile");
    let disc_size: u64 = 10 * 2048;
    let bad_pos: u64 = 5 * 2048;
    let bad_size: u64 = 2048;
    {
        let mut map = Mapfile::create(&mf_path, disc_size, "test").unwrap();
        map.record(0, bad_pos, freemkv_engine::SectorStatus::Finished)
            .unwrap();
        // Left "maybe" after the last patch pass.
        map.record(bad_pos, bad_size, freemkv_engine::SectorStatus::NonTrimmed)
            .unwrap();
        map.record(
            bad_pos + bad_size,
            disc_size - bad_pos - bad_size,
            freemkv_engine::SectorStatus::Finished,
        )
        .unwrap();
        map.flush().unwrap();
    }

    let mut map = Mapfile::load(&mf_path).unwrap();
    let title = test_title(0, 10); // whole 10-sector disc is in-title

    // Before promotion the range is NonTrimmed ("maybe"), NOT yet counted
    // as terminal Unreadable — but it IS in the bad set, so the loop would
    // still be trying to recover it (scope not converged).
    let pre = scope_bad_bytes(false, &map.ranges_with(&bad_sector_statuses()), &title);
    assert_eq!(
        pre, bad_size,
        "NonTrimmed range is bad (still being retried)"
    );
    assert!(!scope_converged(pre));

    // Apply the pinned promotion decision.
    let (from, to) = end_of_recovery_promotion();
    for (pos, size) in map.ranges_with(from) {
        map.record(pos, size, to).unwrap();
    }

    // The range is now terminal Unreadable: the abort gate reads it as lost.
    let unreadable = map.ranges_with(&[freemkv_engine::SectorStatus::Unreadable]);
    assert_eq!(unreadable, vec![(bad_pos, bad_size)]);
    let lost_bytes = freemkv_engine::abort_lost_bytes(false, &title, &unreadable);
    assert_eq!(
        lost_bytes, bad_size,
        "promoted-Unreadable bytes are counted as in-title loss by the abort gate"
    );
}

// PASS-1-ONLY (negative side): patch passes have no transport-retry concept — any patch
// error breaks the loop.
#[test]
fn char_patch_passes_have_no_transport_retry() {
    // The patch loop runs exactly `patch_passes` iterations with no inner
    // reopen/resume — the only retry budget is the pass count itself.
    assert_eq!(plan_passes(3).patch_passes, 3);
    // Single-pass has neither a sweep nor a transport-retry surface.
    assert_eq!(plan_passes(0).sweep_passes, 0);
}

#[test]
fn format_pass_error_hardware_wedge() {
    let e = Error::DiscRead {
        sector: 19_965_280,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: 4,
            asc: 0x3E,
            ascq: 0,
        }),
    };
    let s = format_pass_error("Pass 1", &e);
    assert!(s.contains("40.9 GB") || s.contains("40.8 GB") || s.contains("40.7 GB"));
    assert!(s.contains("sector 19965280"));
    assert!(s.to_lowercase().contains("firmware unresponsive"));
    assert!(s.to_lowercase().contains("power-cycle"));
    // No raw "E6000" / hex-tuple cruft.
    assert!(!s.contains("E6000"));
    assert!(!s.contains("0x04/0x3e"));
}

#[test]
fn format_pass_error_medium_error_promises_no_retry() {
    let e = Error::DiscRead {
        sector: 1_000_000,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: 3,
            asc: 0x11,
            ascq: 0,
        }),
    };
    let s = format_pass_error("Pass 1", &e);
    assert!(s.to_lowercase().contains("bad sector"));
    // The message reports a failed operation: nothing skips or retries on its own.
    assert!(!s.to_lowercase().contains("pass 2"), "msg: {s}");
    assert!(!s.contains("skip this region"), "msg: {s}");
    assert!(s.ends_with("clean the disc and retry the rip"), "msg: {s}");
}

#[test]
fn format_pass_error_illegal_request_advises_powercycle() {
    let e = Error::DiscRead {
        sector: 1_000,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: 5,
            asc: 0x24,
            ascq: 0,
        }),
    };
    let s = format_pass_error("Pass 1", &e);
    assert!(s.to_lowercase().contains("rejected command"));
    assert!(s.to_lowercase().contains("power-cycle"));
}

#[test]
fn pass1_exhaustion_message_translates_cause_not_strategy_id() {
    // Regression: the Pass 1 exhaustion fallthrough must surface the
    // underlying SCSI cause via `format_pass_error`, never a bare
    // internal strategy identifier, mirroring `last_sweep_err`'s translation.
    let last_sweep_err = Some(Error::DiscRead {
        sector: 1_000,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: 4,
            asc: 0x3E,
            ascq: 0,
        }),
    });

    let user_msg = super::pass1_last_error(last_sweep_err.as_ref().unwrap());

    // Operator-facing, actionable.
    assert!(user_msg.to_lowercase().contains("power-cycle"));
    // Never leaks the internal strategy identifiers.
    assert!(!user_msg.contains("transport_failure_recovery_exhausted"));
    assert!(!user_msg.contains("unrecoverable_error"));
}

// An I/O failure with no sense data still names the pass and its cause.
#[test]
fn pass1_last_error_names_a_non_scsi_cause() {
    let e = Error::IoError {
        source: std::io::Error::other("bridge gone"),
    };
    assert_eq!(super::pass1_last_error(&e), "Pass 1 failed: bridge gone");
}

#[test]
fn format_pass_error_no_sense_keeps_raw() {
    // Non-SCSI errors (e.g. transport) pass through the original
    // error display so we don't lose information.
    let e = Error::IoError {
        source: std::io::Error::other("io test"),
    };
    let s = format_pass_error("Pass 1", &e);
    assert!(s.contains("Pass 1"));
    assert!(s.contains("io test"));
}

#[test]
fn format_pass_error_no_sense_non_io_gets_english_label() {
    // Regression: a non-SCSI, non-IoError error (no sense triple) must
    // carry an English label, not just the bare code-only Display.
    // Halted ("rip stopped by user") is the actionable example here.
    let s = format_pass_error("Pass 1", &Error::Halted);
    assert!(s.contains("Pass 1 failed"), "msg: {s}");
    // Still routable: the numeric code is preserved.
    assert!(s.contains("E6010"), "msg must keep the code: {s}");
    // ...but no longer opaque: an English label identifies it.
    assert!(
        s.to_lowercase().contains("stopped by user"),
        "msg must label the code: {s}"
    );

    // MapfileInvalid carries a `kind` payload in its Display; the label
    // must still be appended after it.
    let s = format_pass_error("Pass 2", &Error::MapfileInvalid { kind: "hex" });
    assert!(s.contains("E6011"), "msg: {s}");
    assert!(
        s.to_lowercase().contains("mapfile invalid"),
        "msg must label the code: {s}"
    );
}

// ── format_lib_error: setup/scan/open/mux phase rendering ────────
// Library Display is code-only (`E1002: /dev/sg0`); every variant here
// must render as plain English, phase-labeled, with no code or device path.

#[test]
fn format_lib_error_device_permission_says_privileged_not_code() {
    let e = Error::DevicePermission {
        path: "/dev/sg0".into(),
    };
    let s = format_lib_error("Cannot open drive", &e);
    assert!(s.starts_with("Cannot open drive failed:"), "msg: {s}");
    assert!(s.to_lowercase().contains("privileged"), "msg: {s}");
    // No raw code, no leaked device path.
    assert!(!s.contains("E1001"), "msg leaks code: {s}");
    assert!(!s.contains("/dev/sg0"), "msg leaks path: {s}");
}

#[test]
fn format_lib_error_device_not_found_actionable() {
    let e = Error::DeviceNotFound {
        path: "/dev/sg9".into(),
    };
    let s = format_lib_error("Cannot open drive", &e);
    assert!(s.to_lowercase().contains("unplugged"), "msg: {s}");
    assert!(!s.contains("E1000"), "msg: {s}");
    assert!(!s.contains("/dev/sg9"), "msg: {s}");
}

// E7031 (live disc's Unit_Key_RO.inf unreadable) is a dirty/damaged-disc outcome with
// its own advice, not an "unexpected error" nor an "unrecognized AACS stage".
#[test]
fn key_file_unreadable_renders_clean_the_disc_advice() {
    let s = format_lib_error("Disc scan", &Error::AacsKeyFileUnreadable);
    assert!(s.starts_with("Disc scan failed:"), "msg: {s}");
    assert!(s.to_lowercase().contains("clean the disc"), "msg: {s}");
    let s = aacs_failure_message(Some(&Error::AacsKeyFileUnreadable));
    assert!(s.starts_with("Error: E7031 "), "msg: {s}");
    assert!(s.contains("Unit_Key_RO.inf"), "msg: {s}");
    assert!(!s.contains("unrecognized"), "msg: {s}");
}

// E7033 (all offered host certs failed a local keydb check) is a key-database
// problem with its own advice, not the "unexpected error" or "unrecognized
// AACS stage" fallbacks it used to fall through to.
#[test]
fn no_usable_host_cert_renders_refresh_keydb_advice() {
    let s = format_lib_error("Disc scan", &Error::AacsNoUsableHostCert);
    assert!(s.starts_with("Disc scan failed:"), "msg: {s}");
    assert!(
        s.to_lowercase().contains("refresh your key database"),
        "msg: {s}"
    );
    let s = aacs_failure_message(Some(&Error::AacsNoUsableHostCert));
    assert!(s.starts_with("Error: E7033 "), "msg: {s}");
    assert!(!s.contains("unrecognized"), "msg: {s}");
}

#[test]
fn format_lib_error_no_streams_plain_english() {
    let s = format_lib_error("Disc scan", &Error::NoStreams);
    assert!(s.starts_with("Disc scan failed:"), "msg: {s}");
    assert!(s.to_lowercase().contains("no playable video"), "msg: {s}");
    assert!(!s.contains("E6009"), "msg leaks code: {s}");
}

#[test]
fn format_lib_error_udf_not_found_blank_disc_hint() {
    let e = Error::UdfNotFound {
        path: "/some/internal/path".into(),
    };
    let s = format_lib_error("Disc scan", &e);
    assert!(s.to_lowercase().contains("filesystem"), "msg: {s}");
    assert!(!s.contains("E6003"), "msg: {s}");
    assert!(!s.contains("/some/internal/path"), "msg leaks path: {s}");
}

#[test]
fn format_lib_error_disc_read_advises_clean_disc() {
    // A DiscRead WITHOUT sense data (no SCSI triple) — must still render
    // a plain-English clean-the-disc message, not a bare code or sector.
    let e = Error::DiscRead {
        sector: 12345,
        status: None,
        sense: None,
    };
    let s = format_lib_error("Disc scan", &e);
    assert!(s.to_lowercase().contains("could not be read"), "msg: {s}");
    assert!(!s.contains("E6000"), "msg leaks code: {s}");
    assert!(!s.contains("12345"), "msg leaks sector: {s}");
}

#[test]
fn format_lib_error_disc_read_with_sense_uses_pass_decoder() {
    // A DiscRead WITH sense data routes through format_pass_error, so the
    // operator gets the media-damage cause + Pass-2 guidance.
    let e = Error::DiscRead {
        sector: 1_000_000,
        status: Some(2),
        sense: Some(ScsiSense {
            sense_key: 3,
            asc: 0x11,
            ascq: 0,
        }),
    };
    let s = format_lib_error("Disc scan", &e);
    assert!(s.to_lowercase().contains("bad sector"), "msg: {s}");
    assert!(!s.contains("E6000"), "msg leaks code: {s}");
}

#[test]
fn format_lib_error_io_error_surfaces_inner_message() {
    // io::Error Display is already plain English — surface it directly,
    // no synthetic phrasing, no code.
    let e = Error::IoError {
        source: std::io::Error::other("no space left on device"),
    };
    let s = format_lib_error("Open output file", &e);
    assert!(s.starts_with("Open output file failed:"), "msg: {s}");
    assert!(s.contains("no space left on device"), "msg: {s}");
}

#[test]
fn format_lib_error_decrypt_defers_to_aacs_humanizer() {
    let s = format_lib_error("Disc scan", &Error::CssKeyMissing);
    // Routed through aacs_failure_message → CSS-specific text, prefix stripped.
    assert!(s.starts_with("Disc scan failed:"), "msg: {s}");
    assert!(s.to_lowercase().contains("unscramble"), "msg: {s}");
    // The stripped form must NOT carry the Error:/E#### prefix.
    assert!(!s.contains("E7023"), "msg leaks code: {s}");
}

#[test]
fn format_lib_error_never_leaks_bare_code_for_unmapped_variant() {
    // An unmapped variant (EmptyImage has no arm) must hit the generic arm,
    // not dump a code or a Debug rendering.
    let s = format_lib_error("Disc scan", &Error::EmptyImage);
    assert_eq!(
        s,
        "Disc scan failed: an unexpected error occurred. Enable debug logging via \
             /api/debug for details."
    );
}

// ── aacs_failure_message dispatch ──────────────────────────────
// Locked rc.6 standard: every message renders `Error: E<code> <msg>` —
// level word, routable code, one plain-English sentence, never a raw `{e:?}` dump.

#[test]
fn aacs_failure_messages_follow_level_code_format() {
    // Format contract: every rendered message starts with `Error: E<code> `.
    for e in [
        Error::CssKeyMissing,
        Error::KeydbLoad {
            path: "<no keydb in search paths>".into(),
        },
        Error::KeydbLoad {
            path: "/config/keys/keydb.cfg".into(),
        },
        Error::AacsNoKeys,
        Error::AacsCertRejected,
        Error::AacsRawReadUnsupported,
        Error::AacsVidRead,
        Error::AacsDataKey,
        Error::AacsVukNotInKeydb,
        Error::DriveProfileMissing,
        Error::VidCdbUnavailable,
        Error::AacsNoHostCert {
            path: "<no host cert>".into(),
        },
        Error::AacsAgidAlloc,
        Error::AacsKeyFileUnreadable,
        Error::AacsNoUsableHostCert,
    ] {
        let s = aacs_failure_message(Some(&e));
        assert!(
            s.starts_with(&format!("Error: E{} ", e.code())),
            "{e:?} must render `Error: E<code> <msg>`, got: {s}"
        );
        // One line — no embedded newline in the rc.6 single-line format.
        assert!(!s.contains('\n'), "{e:?} message must be one line: {s}");
    }
}

#[test]
fn aacs_failure_keydb_load_missing_path() {
    let e = Error::KeydbLoad {
        path: "<no keydb in search paths>".into(),
    };
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E8005 "), "msg: {s}");
    assert!(s.contains("No keys are available"), "msg: {s}");
    assert!(!s.contains("KEYDB"), "msg must not name the source: {s}");
}

#[test]
fn aacs_failure_keydb_load_corrupt() {
    // A *configured* keydb (real path, not the sentinel) that fails to
    // load must surface that path, not the generic "configure a key
    // source" message reserved for the no-keydb case.
    let path = "/config/keys/keydb.cfg";
    let e = Error::KeydbLoad { path: path.into() };
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E8005 "), "msg: {s}");
    assert!(s.contains(path), "msg must include the failing path: {s}");
    assert!(
        !s.contains("Configure a key source in Settings"),
        "configured-but-failed must not show the no-keydb message: {s}"
    );
    assert!(!s.contains("KEYDB"), "msg must not name the source: {s}");
}

#[test]
fn aacs_failure_cert_rejected_says_host_cert() {
    // E7003 — drive rejected our host cert (HRL).
    let e = Error::AacsCertRejected;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7003 "), "msg: {s}");
    assert!(s.contains("host certificate"), "msg: {s}");
    assert!(s.contains("raw-read mode"), "msg: {s}");
    // No "Update keys/KEYDB" — the key source has the cert; the HRL blocks it.
    assert!(!s.contains("Update KEYDB"), "msg: {s}");
    // Must not leak the debug-dump form the old catch-all emitted.
    assert!(!s.contains("AacsCertRejected"), "msg: {s}");
}

#[test]
fn aacs_failure_cert_verify_collapses_to_host_cert() {
    let e = Error::AacsCertVerify;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7005 "), "msg: {s}");
    assert!(s.contains("host certificate"), "msg: {s}");
}

// The disk-space preflight message must NOT carry a raw "EXXXX:" code prefix (no libfreemkv
// Error is raised here). Guards against re-introducing it.
#[test]
fn disk_space_preflight_message_has_no_raw_error_code_prefix() {
    let required = 100u64 * 1_073_741_824; // 100 GiB
    let avail = 40u64 * 1_073_741_824; // 40 GiB
    let s = disk_space_preflight_message(required, "/staging-local", avail);
    assert!(
        !s.contains("E5000"),
        "raw E5000 code leaked into operator message: {s}"
    );
    // No "ENNNN:" code prefix anywhere (digits-after-E followed by colon).
    for (i, _) in s.match_indices('E') {
        let tail = &s[i + 1..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        assert!(
            digits.is_empty() || !tail[digits.len()..].starts_with(':'),
            "raw EXXXX: code prefix leaked into operator message: {s}"
        );
    }
    // Still reports both the requirement and the actual free space.
    assert!(s.contains("100.0 GiB"), "missing required figure: {s}");
    assert!(s.contains("40.0 GiB"), "missing available figure: {s}");
    assert!(s.contains("/staging-local"), "missing staging path: {s}");
}

#[test]
fn disk_space_preflight_estimates_remaining_image_plus_selected_title() {
    let capacity = 90 * 1_000_000_000u64;
    let title = 10 * 1_000_000_000u64;
    assert_eq!(
        disk_space_required_bytes(capacity, title, None),
        100 * 1_000_000_000,
        "90 GB image plus 10 GB selected title uses a 100 GB estimate"
    );
    assert_eq!(
        disk_space_required_bytes(capacity, title, Some(20 * 1_000_000_000)),
        30 * 1_000_000_000,
        "resume needs the unswept image plus the selected title"
    );
    assert_eq!(
        disk_space_required_bytes(capacity, title, Some(capacity * 2)),
        capacity + title,
        "unswept bytes are capped at disc capacity"
    );
}

// The real resume filter: a valid mapfile yields its unswept bytes; a mapfile
// sized for another disc, a truncated ISO, or no mapfile falls back (None).
#[test]
fn resume_remaining_iso_bytes_rejects_mismatched_or_truncated_state() {
    let dir = std::env::temp_dir().join(format!(
        "autorip-preflight-resume-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let total = 64 * 2048u64;
    let map_path = dir.join("disc.iso.mapfile");
    let iso_path = dir.join("disc.iso");
    let mut map = freemkv_engine::Mapfile::create(&map_path, total, "test").unwrap();
    map.record(0, 16 * 2048, freemkv_engine::SectorStatus::Finished)
        .unwrap();
    drop(map);
    let remaining = total - 16 * 2048;

    assert_eq!(
        resume_remaining_iso_bytes(&map_path, &iso_path, total),
        None,
        "missing ISO: fresh estimate"
    );
    std::fs::File::create(&iso_path)
        .unwrap()
        .set_len(total - 2048)
        .unwrap();
    assert_eq!(
        resume_remaining_iso_bytes(&map_path, &iso_path, total),
        None,
        "truncated ISO: fresh estimate"
    );
    std::fs::File::create(&iso_path)
        .unwrap()
        .set_len(total)
        .unwrap();
    assert_eq!(
        resume_remaining_iso_bytes(&map_path, &iso_path, total),
        Some(remaining),
        "valid resume state reports only the unswept bytes"
    );
    // ISO long enough for the other disc, so only the total_size check rejects it.
    std::fs::File::create(&iso_path)
        .unwrap()
        .set_len(total * 2)
        .unwrap();
    assert_eq!(
        resume_remaining_iso_bytes(&map_path, &iso_path, total * 2),
        None,
        "mapfile total_size mismatch (another disc): fresh estimate"
    );
    assert_eq!(
        resume_remaining_iso_bytes(&dir.join("absent.mapfile"), &iso_path, total),
        None,
        "no mapfile: fresh estimate"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn skip_diskcheck_is_truthy_only() {
    for v in ["1", "true", "TRUE", "yes", " Yes "] {
        assert!(skip_diskcheck_value(Some(v)), "{v:?} must skip");
    }
    for v in ["0", "false", "no", "", "off"] {
        assert!(
            !skip_diskcheck_value(Some(v)),
            "{v:?} must keep the check on"
        );
    }
    assert!(!skip_diskcheck_value(None), "unset keeps the check on");
}

#[test]
fn disk_space_preflight_message_points_at_the_settings_field() {
    let s = disk_space_preflight_message(1 << 30, "/staging", 0);
    assert!(!s.contains("STAGING_DIR"), "no such env var: {s}");
    assert!(
        s.contains("Staging Directory"),
        "names the Settings field: {s}"
    );
}

#[test]
fn aacs_failure_key_rejected_says_host_cert() {
    // E7007 — drive HRL blocked our processing key. Same
    // remediation as cert rejection.
    let e = Error::AacsKeyRejected;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7007 "), "msg: {s}");
    assert!(s.contains("host certificate"), "msg: {s}");
}

#[test]
fn aacs_failure_vid_read_says_vid_missing() {
    let e = Error::AacsVidRead;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7009 "), "msg: {s}");
    assert!(s.contains("Volume ID"), "msg: {s}");
}

#[test]
fn aacs_failure_vid_mac_says_vid_missing() {
    let e = Error::AacsVidMac;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7010 "), "msg: {s}");
    assert!(s.contains("Volume ID"), "msg: {s}");
}

#[test]
fn aacs_failure_data_key_says_mk_missing() {
    let e = Error::AacsDataKey;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7011 "), "msg: {s}");
    assert!(s.contains("media key"), "msg: {s}");
}

#[test]
fn aacs_failure_no_keys_says_all_missing() {
    let e = Error::AacsNoKeys;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7000 "), "msg: {s}");
    assert!(s.contains("No keys are available"), "msg: {s}");
}

#[test]
fn aacs_failure_unknown_aacs_code_uses_generic_7xxx_arm() {
    // Unmapped-but-AACS-range error falls through to the 7xxx
    // catch-all. E_AACS_AGID_ALLOC (7002) is not in any named arm
    // and exercises that path.
    let e = Error::AacsAgidAlloc;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7002 "), "msg: {s}");
    assert!(s.contains("unrecognized stage"), "msg: {s}");
    // Must carry the scheme: a schemeless host never linkifies in the
    // web UI or in a terminal.
    assert!(
        s.contains("https://github.com/freemkv/freemkv/issues"),
        "msg: {s}"
    );
    // No debug-dump leak.
    assert!(!s.contains("AacsAgidAlloc"), "msg: {s}");
}

#[test]
fn aacs_failure_none_falls_back_defensively() {
    let s = aacs_failure_message(None);
    assert!(s.contains("no keys were found"), "msg: {s}");
}

// ── variants landing with v0.25.11 ───────────────────────────────

#[test]
fn aacs_failure_host_cert_rejected_says_host_cert() {
    // E7015 — all host certs in keydb were rejected by the drive.
    let e = Error::AacsHostCertRejected;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7015 "), "msg: {s}");
    assert!(s.contains("host certificate"), "msg: {s}");
    assert!(!s.contains("AacsHostCertRejected"), "msg: {s}");
}

#[test]
fn aacs_failure_raw_read_unsupported_says_no_cert() {
    // E7016 — drive doesn't support raw-read mode AND no host certs.
    let e = Error::AacsRawReadUnsupported;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7016 "), "msg: {s}");
    assert!(s.contains("does not support raw-read mode"), "msg: {s}");
}

#[test]
fn aacs_failure_vid_unavailable_says_vid_missing() {
    // E7017 — alternate VID read failed.
    let e = Error::AacsVidUnavailable;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7017 "), "msg: {s}");
    assert!(s.contains("Volume ID"), "msg: {s}");
}

#[test]
fn aacs_failure_mk_unavailable_says_mk_missing() {
    // E7018 — VID ok, but no DK in keydb walks this MKB.
    let e = Error::AacsMkUnavailable;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7018 "), "msg: {s}");
    assert!(s.contains("media key"), "msg: {s}");
}

#[test]
fn aacs_failure_vuk_not_in_keydb_says_vuk_missing() {
    // E7019 — disc hash isn't in keydb and no derivation path
    // was available.
    let e = Error::AacsVukNotInKeydb;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7019 "), "msg: {s}");
    assert!(s.contains("could not be resolved"), "msg: {s}");
    assert!(!s.contains("KEYDB"), "msg must not name the source: {s}");
}

#[test]
fn aacs_failure_drive_profile_missing_has_dedicated_arm() {
    // E7020 — drive not in profile DB; must not fall through to the
    // generic "report at github.com" catch-all.
    let e = Error::DriveProfileMissing;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7020 "), "msg: {s}");
    assert!(s.contains("profile database"), "msg: {s}");
    assert!(
        !s.contains("github.com"),
        "msg must not say report a bug: {s}"
    );
}

#[test]
fn aacs_failure_vid_cdb_unavailable_has_dedicated_arm() {
    // E7021 — profile present but no VID-retrieval CDB template.
    let e = Error::VidCdbUnavailable;
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7021 "), "msg: {s}");
    assert!(s.contains("Volume ID command"), "msg: {s}");
    assert!(
        !s.contains("github.com"),
        "msg must not say report a bug: {s}"
    );
}

#[test]
fn aacs_failure_no_host_cert_has_dedicated_arm() {
    // E7024 — no host cert available; the OEM auth route can't run.
    let e = Error::AacsNoHostCert {
        path: "<no host cert>".into(),
    };
    let s = aacs_failure_message(Some(&e));
    assert!(s.starts_with("Error: E7024 "), "msg: {s}");
    assert!(s.contains("host certificate"), "msg: {s}");
    assert!(
        !s.contains("github.com"),
        "msg must not say report a bug: {s}"
    );
}

#[test]
fn aacs_failure_message_is_one_line() {
    // Locked rc.6 format contract: one line, `Error: E<code> <message>`,
    // no embedded newline. (Replaces the pre-rc.6 two-line heading/body.)
    for e in [
        Error::AacsNoKeys,
        Error::AacsCertRejected,
        Error::AacsHostCertRejected,
        Error::AacsRawReadUnsupported,
        Error::AacsVidUnavailable,
        Error::AacsMkUnavailable,
        Error::AacsVukNotInKeydb,
        Error::DriveProfileMissing,
        Error::VidCdbUnavailable,
        Error::AacsNoHostCert {
            path: "<no host cert>".into(),
        },
    ] {
        let s = aacs_failure_message(Some(&e));
        assert!(!s.contains('\n'), "{e:?} message must be one line: {s}");
        assert!(
            s.starts_with(&format!("Error: E{} ", e.code())),
            "{e:?} must lead with the level word and code: {s}"
        );
    }
}

#[test]
fn css_crack_failure_is_not_aacs_messaging() {
    // Regression: a CSS crack failure records `Error::CssKeyMissing`, not
    // `aacs_error`. The keyless-disc message must surface the CSS heading,
    // NOT the AACS "check the key source" fallback that `None` produced.
    let msg = aacs_failure_message(Some(&Error::CssKeyMissing));
    assert!(
        msg.to_lowercase().contains("unscramble") || msg.to_lowercase().contains("css"),
        "CSS failure should name the CSS problem, got: {msg}"
    );
    assert!(
        !msg.to_lowercase().contains("key source in settings"),
        "CSS failure must not point the operator at the (AACS) key source: {msg}"
    );
    // Locked rc.6 format: `Error: E<code> <message>`, one line.
    assert!(
        msg.starts_with(&format!(
            "Error: E{} ",
            libfreemkv::error::E_CSS_KEY_MISSING
        )),
        "CSS failure should lead with the level word and E-code: {msg}"
    );
    assert!(!msg.contains('\n'), "CSS message must be one line: {msg}");
}

// Reachability verdict → operator status-line mapping. ONLY the verdicts where the service
// never gave an answer about this disc may claim to be a temporary outage.
#[test]
fn key_service_transient_status_mapping() {
    use crate::server::keysource::ServiceReachability;
    // Never reached at all → the only honest "temporary, will retry".
    let down = super::key_service_transient_status(ServiceReachability::Unreachable)
        .expect("Unreachable is transient");
    assert!(
        down.contains("could not connect") && down.contains("usually temporary"),
        "Unreachable must read as a connection failure we will retry: {down}"
    );
    // 5xx → reached, but it failed on its own side. Quotes the status.
    let server = super::key_service_server_error_status(503);
    assert!(
        server.contains("503") && server.contains("usually temporary"),
        "a 5xx must quote its status and read as temporary: {server}"
    );
    assert_eq!(
        super::key_service_transient_status(ServiceReachability::ServerError(503)),
        Some(server)
    );
    // 429 quota → its own message, not the transport-failure one.
    let quota = super::key_service_transient_status(ServiceReachability::RateLimited)
        .expect("RateLimited is transient");
    assert!(
        quota.contains("too many") && quota.contains("429"),
        "RateLimited status must explain the quota and quote its status: {quota}"
    );
    assert_ne!(
        quota, down,
        "rate limiting and an unreachable service are different situations"
    );
    // Everything the service DID answer is not an outage.
    for answered in [
        ServiceReachability::Answered,
        ServiceReachability::NoKeyForDisc,
        ServiceReachability::NotLicensed,
        ServiceReachability::Unexpected(400),
        ServiceReachability::NotAsked,
    ] {
        assert!(
            super::key_service_transient_status(answered).is_none(),
            "{answered:?} is an answer (or a config fault), not an outage"
        );
    }
}

// The bug: a 422 was reported with the "service could not be reached, wait
// and retry" wording. Each terminal verdict must say what happened, whether
// retrying helps, and carry its HTTP status — and none may claim an outage.
#[test]
fn terminal_key_service_verdicts_never_claim_an_outage() {
    use crate::server::keysource::ServiceReachability;

    let no_key = super::key_service_no_key_reason(ServiceReachability::NoKeyForDisc)
        .expect("422 is a definitive answer with its own wording");
    assert!(
        no_key.contains("answered") && no_key.contains("has no key for this disc"),
        "a 422 must say the service ANSWERED and has no key: {no_key}"
    );
    assert!(
        no_key.contains("will not change the result"),
        "a 422 must tell the operator retrying is pointless: {no_key}"
    );
    assert!(
        no_key.contains("capture without keys"),
        "a 422 must offer a next step: {no_key}"
    );
    assert!(no_key.contains("422"), "the status is needed for support");
    // The exact harm being fixed — none of the outage wording may appear.
    for banned in [
        "could not be reached",
        "never said whether",
        "the service was down",
        "temporary outage",
        "wait a few minutes",
    ] {
        assert!(
            !no_key.to_lowercase().contains(banned),
            "a definitive no-key must not borrow outage wording ({banned:?}): {no_key}"
        );
    }

    // 404 is a licence/config wall, not the 422 no-key and not an outage.
    let unlicensed = super::key_service_no_key_reason(ServiceReachability::NotLicensed)
        .expect("404 has its own wording");
    assert!(
        unlicensed.contains("404") && unlicensed.contains("Settings"),
        "a 404 must quote its status and point at the config: {unlicensed}"
    );
    assert_ne!(unlicensed, no_key, "404 and 422 must not share a message");

    // An unrecognised status says so plainly, and quotes the status.
    let odd = super::key_service_no_key_reason(ServiceReachability::Unexpected(418))
        .expect("an unexpected status still gets a message");
    assert!(
        odd.contains("does not recognise") && odd.contains("418"),
        "an unexpected status must be reported plainly, with the status: {odd}"
    );
    assert_ne!(odd, no_key);
    assert_ne!(odd, unlicensed);

    // Never-asked (bad/blocked URL) is a standing misconfiguration.
    let not_asked = super::key_service_no_key_reason(ServiceReachability::NotAsked)
        .expect("an unusable URL has its own wording");
    assert!(
        not_asked.contains("never contacted") && not_asked.contains("Settings"),
        "an unusable key-service URL must name the config fault: {not_asked}"
    );

    // An ordinary 2xx no-key keeps the existing generic text.
    assert!(
        super::key_service_no_key_reason(ServiceReachability::Answered).is_none(),
        "a plain 2xx no-key must keep the generic no-key message"
    );
    // Transient verdicts are handled by the other mapper, never this one.
    for transient in [
        ServiceReachability::Unreachable,
        ServiceReachability::ServerError(502),
        ServiceReachability::RateLimited,
    ] {
        assert!(super::key_service_no_key_reason(transient).is_none());
    }
}

// a reused (scanned) session must hand rip_disc the scan's real /decode
// verdict; dropping it re-fires the empty probe, which reads a 422 as Answered.
#[test]
fn rip_seed_verdict_carries_the_scan_verdict_for_a_reused_session() {
    use crate::server::keysource::ServiceReachability as R;
    assert_eq!(
        super::rip_seed_verdict(None, Some(R::NoKeyForDisc)),
        Some(R::NoKeyForDisc),
        "reused session: the scan's 422 must reach the rip classifier"
    );
    assert_eq!(
        super::rip_seed_verdict(Some(R::Unreachable), None),
        Some(R::Unreachable)
    );
    assert_eq!(super::rip_seed_verdict(None, None), None);
}

// the deferred/resume path must report the resume decode's verdict, not a
// probe's. A cleartext keyserver_url makes the probe say NotAsked, so a 422 is visible.
#[test]
fn deferred_keyless_texts_use_the_decode_verdict() {
    use crate::server::keysource::ServiceReachability as R;
    let cfg = crate::server::config::Config {
        key_source: "online".into(),
        keyserver_url: "http://8.8.8.8/decode".into(),
        ..Default::default()
    };
    let disc = encrypted_keyless_disc();
    let (_, msg) = super::deferred_keyless_texts(&cfg, &disc, Some(R::NoKeyForDisc));
    assert!(
        msg.contains("HTTP 422") && msg.contains("has no key for this disc"),
        "the resume decode's 422 must be reported: {msg}"
    );
    let (_, down) = super::deferred_keyless_texts(&cfg, &disc, Some(R::Unreachable));
    assert!(down.contains("could not connect"), "{down}");
    // No decode verdict → probe fallback (NotAsked for a refused URL).
    let (_, probed) = super::deferred_keyless_texts(&cfg, &disc, None);
    assert!(probed.contains("never contacted"), "{probed}");
}

// key_source=local with a saved URL: no "Communicating with online keyserver"
// status, no outage retry, and the keyless deferral never probes the URL.
#[test]
fn local_key_source_with_saved_url_makes_no_online_probe() {
    use libfreemkv::keys::KeyScope;
    let cfg = |src: &str| crate::server::config::Config {
        key_source: src.into(),
        keyserver_url: "http://8.8.8.8/decode".into(),
        ..Default::default()
    };
    let disc = encrypted_keyless_disc();
    assert!(!super::announces_online_resolve(
        &cfg("local"),
        &disc,
        &KeyScope::WholeDisc
    ));
    assert!(super::announces_online_resolve(
        &cfg("online"),
        &disc,
        &KeyScope::WholeDisc
    ));
    assert!(!super::should_retry_online_keys(
        crate::server::keysource::uses_online(&cfg("local")),
        false,
        true,
        true
    ));
    // Online, the probe of the refused http:// URL says "never contacted";
    // local must not probe at all, so that text is absent.
    let (_, online) = super::deferred_keyless_texts(&cfg("online"), &disc, None);
    assert!(online.contains("never contacted"), "{online}");
    let (_, local) = super::deferred_keyless_texts(&cfg("local"), &disc, None);
    assert!(!local.contains("never contacted"), "{local}");
}

// 401/403 is a credential rejection — it must get the credential text,
// not the "does not recognise ... check the key-service address" wording.
#[test]
fn unauthorized_decode_verdict_names_the_credentials() {
    use crate::server::keysource::ServiceReachability as R;
    for code in [401u16, 403] {
        let reason = super::key_service_no_key_reason(R::Unauthorized(code))
            .expect("401/403 has its own wording");
        assert!(
            reason.contains("rejected the credentials") && reason.contains("Keyserver API Secret"),
            "{code}: {reason}"
        );
        assert!(reason.contains(&format!("HTTP {code}")), "{reason}");
        assert!(!reason.contains("does not recognise"), "{reason}");
        assert!(super::key_service_transient_status(R::Unauthorized(code)).is_none());
    }
}

// last_error must not double the "No keys — " prefix.
#[test]
fn keyless_not_ripping_error_has_one_prefix() {
    let reason = super::key_service_no_key_reason(
        crate::server::keysource::ServiceReachability::NoKeyForDisc,
    )
    .expect("422 reason");
    let err = super::keyless_not_ripping_error(&format!("No keys — {reason}"));
    assert_eq!(err.matches("No keys").count(), 1, "doubled prefix: {err}");
    assert!(err.starts_with("No keys — not ripping"), "{err}");
    assert!(err.contains("HTTP 422"), "{err}");
    // A disc-error fallback message (no prefix) is kept whole.
    let fb = super::keyless_not_ripping_error("Error: E7000 No keys are available.");
    assert!(fb.ends_with("Error: E7000 No keys are available."), "{fb}");
}

// Config-class verdicts (fixable in Settings without re-inserting the disc) must
// trigger one fresh resolve; per-disc answers and outages must not.
#[test]
fn config_class_seed_verdicts_reresolve_once() {
    use crate::server::keysource::ServiceReachability as R;
    for v in [
        R::Unauthorized(401),
        R::NotAsked,
        R::NotLicensed,
        R::Unexpected(400),
    ] {
        assert!(super::seed_needs_reresolve(None, Some(v)), "{v:?}");
        // rip_disc's own fresh resolve just used the current Settings: no re-run.
        assert!(!super::seed_needs_reresolve(Some(v), None), "fresh {v:?}");
    }
    for v in [
        R::NoKeyForDisc,
        R::Answered,
        R::Unreachable,
        R::ServerError(503),
        R::RateLimited,
    ] {
        assert!(!super::seed_needs_reresolve(None, Some(v)), "{v:?}");
    }
    assert!(!super::seed_needs_reresolve(None, None));
}

// capture_without_keys is the raw-copy mode: only it scans on past an unreadable key file.
#[test]
fn scan_opts_raw_copy_follows_capture_without_keys() {
    for capture in [true, false] {
        let cfg = crate::server::config::Config {
            capture_without_keys: capture,
            keydb_path: Some("/nonexistent/autorip-test/keydb.cfg".into()),
            ..Default::default()
        };
        assert_eq!(super::scan_opts_for(&cfg).raw_copy, capture);
    }
}

// Wiring guard: scan_disc banks its verdict on the session, rip_disc reads it,
// and a config-class seed re-resolves before the outage classifier runs.
#[test]
fn scan_verdict_is_banked_and_read_by_rip() {
    let all = crate::server::util::source_lf(include_str!("mod.rs"));
    let src = &all;
    assert!(
        src.contains("key_verdict: key_reach,"),
        "scan_disc must bank key_reach"
    );
    let seed = src
        .find("let banked_verdict = session.key_verdict.take();")
        .expect("rip_disc must read the banked verdict");
    let reresolve = src[seed..]
        .find("seed_needs_reresolve(resume_decode_reach, banked_verdict)")
        .expect("rip_disc must re-resolve a config-class seed");
    let classify = src[seed..]
        .find("retry_online_keys_on_outage(")
        .expect("rip_disc must classify via the outage retry");
    assert!(
        reresolve < classify,
        "re-resolve must precede classification"
    );
}

// The resume deferral's composed texts: one "no keys" lead, and a terminal
// verdict must not promise an automatic mux that waiting can never deliver.
#[test]
fn deferred_keyless_texts_match_the_verdict() {
    use crate::server::keysource::ServiceReachability as R;
    let cfg = crate::server::config::Config {
        key_source: "online".into(),
        keyserver_url: "http://8.8.8.8/decode".into(),
        ..Default::default()
    };
    let disc = encrypted_keyless_disc();
    // 422 on the resume path: the ISO is already captured and a key may appear
    // later, so no capture advice, no "fix the cause", and the auto-mux tail.
    let (log, state) = super::deferred_keyless_texts(&cfg, &disc, Some(R::NoKeyForDisc));
    for t in [&log, &state] {
        assert_eq!(t.to_lowercase().matches("no keys").count(), 1, "{t}");
        assert!(!t.contains("capture without keys"), "{t}");
        assert!(!t.contains("fixed"), "{t}");
        assert!(t.contains("HTTP 422"), "{t}");
    }
    assert!(log.contains("will mux automatically"), "{log}");
    for v in [R::Unauthorized(403), R::NotLicensed] {
        let (log, state) = super::deferred_keyless_texts(&cfg, &disc, Some(v));
        for t in [&log, &state] {
            assert_eq!(t.to_lowercase().matches("no keys").count(), 1, "{v:?}: {t}");
            assert!(!t.contains("will mux automatically"), "{v:?}: {t}");
        }
        assert!(
            state.starts_with("Ripped to ISO — no keys, mux deferred"),
            "{state}"
        );
    }
    let (log, state) = super::deferred_keyless_texts(&cfg, &disc, Some(R::Unreachable));
    assert!(log.contains("will mux automatically"), "{log}");
    assert!(state.contains("could not connect"), "{state}");
}

// The tile's action button keys off the "Missing keys" prefix; a terminal
// verdict must keep it (the disc really is unrippable) while a transient one
// must NOT (the disc is parked, not failed).
#[test]
fn key_readiness_reports_the_key_service_verdict() {
    use crate::server::keysource::ServiceReachability;
    let mut disc = encrypted_keyless_disc();
    // The precise shape of the bug: the library stamped E7028 ("could not be
    // reached") on a disc the service definitively answered about.
    disc.aacs_error = Some(libfreemkv::Error::KeyServiceUnavailable);

    let tile = super::key_readiness(
        &disc,
        false,
        None,
        false,
        Some(ServiceReachability::NoKeyForDisc),
    );
    assert!(
        tile.starts_with("Missing keys — "),
        "a definitive no-key is still missing keys: {tile}"
    );
    assert!(
        tile.contains("422") && tile.contains("has no key for this disc"),
        "the tile must report what the service said: {tile}"
    );
    assert!(
        !tile.contains("could not be reached"),
        "the E7028 outage wording must not survive a definitive answer: {tile}"
    );

    // Transient: standalone status line, no "Missing keys" prefix (the disc
    // is parked and retryable, so the tile must not offer the failed action).
    let down = super::key_readiness(
        &disc,
        false,
        None,
        false,
        Some(ServiceReachability::Unreachable),
    );
    assert!(!down.starts_with("Missing keys"), "{down}");
    assert!(down.contains("could not connect"), "{down}");

    // No online verdict → unchanged: fall back to the disc's own error.
    let local = super::key_readiness(&disc, false, None, false, None);
    assert!(local.starts_with("Missing keys — "), "{local}");

    // capture-without-keys still overrides every verdict.
    assert_eq!(
        super::key_readiness(
            &disc,
            false,
            None,
            true,
            Some(ServiceReachability::NoKeyForDisc)
        ),
        "Capture without keys — no decryption"
    );
}

// KU-E1: a raw scope's keyless set never passes for a title rip's: the rip resolves.
#[test]
fn a_keyless_set_does_not_cover_a_title_rip() {
    use libfreemkv::keys::{KeyRing, KeyScope};
    let disc = crate::ku_fixture::bd_image().disc;
    let none = KeyRing::none();
    assert!(!super::keys_cover(&disc, &none, &KeyScope::Titles(vec![0])));
    assert!(super::keys_cover(&disc, &none, &KeyScope::None));
}

// KU-E1: the rip's refusal names the cause; a keyed set is ready.
#[test]
fn key_readiness_names_the_rip_refusal() {
    let disc = encrypted_keyless_disc();
    let no_key = libfreemkv::Error::NoDiscKey {
        disc_hash: "ab".into(),
    };
    let tile = super::key_readiness(&disc, false, Some(&no_key), false, None);
    assert_eq!(tile, "Missing keys — no key source has a key for this disc");
    let fmts = libfreemkv::Error::FmtsKeyMissing;
    let tile = super::key_readiness(&disc, false, Some(&fmts), false, None);
    assert!(
        tile.starts_with("Missing keys — ") && !tile.contains("E70"),
        "{tile}"
    );
    assert_eq!(
        super::key_readiness(&disc, true, None, false, None),
        "Ready to rip"
    );
}

// The status-less fallback: with no HTTP status to hand, the three key-SOURCE
// codes must still say the source never answered — never "unrecognized stage"
// (the old 7000..=7999 catch-all) and never "this disc has no key".
#[test]
fn key_source_failure_codes_say_the_source_never_answered() {
    use libfreemkv::error as ec;
    let cases = [
        (
            libfreemkv::Error::KeyServiceUnavailable,
            ec::E_KEY_SERVICE_UNAVAILABLE,
        ),
        (
            libfreemkv::Error::KeyServiceUnauthorized,
            ec::E_KEY_SERVICE_UNAUTHORIZED,
        ),
        (
            libfreemkv::Error::KeyServiceRateLimited,
            ec::E_KEY_SERVICE_RATE_LIMITED,
        ),
    ];
    let mut seen: Vec<String> = Vec::new();
    for (err, code) in cases {
        let msg = super::aacs_failure_message(Some(&err));
        assert!(
            msg.starts_with(&format!("Error: E{code} ")),
            "must keep the locked E-code format: {msg}"
        );
        assert!(
            !msg.contains("unrecognized stage"),
            "E{code} is a known key-SOURCE failure, not an unmapped AACS stage: {msg}"
        );
        assert!(
            msg.contains("never looked for a key") || msg.contains("did not answer"),
            "E{code} must say the source never answered: {msg}"
        );
        assert!(
            !seen.contains(&msg),
            "each key-source failure needs its own message: {msg}"
        );
        seen.push(msg);
    }
}

/// Retry backoff is bounded and monotonic (8s, 16s, 32s, capped) — a small,
/// non-hammering schedule.
#[test]
fn key_service_backoff_is_bounded() {
    assert_eq!(super::key_service_backoff(1).as_secs(), 8);
    assert_eq!(super::key_service_backoff(2).as_secs(), 16);
    assert_eq!(super::key_service_backoff(3).as_secs(), 32);
    // Capped — never grows without bound even if called with a high attempt.
    assert_eq!(super::key_service_backoff(9).as_secs(), 64);
}

#[test]
fn keyless_failure_message_prefers_css_error_over_aacs() {
    // The `.or()` dispatch in keyless_failure_message must consult
    // css_error first. With both set (CSS crack failed, plus a stale
    // AACS error) it must surface CSS messaging.
    let css = Error::CssKeyMissing;
    let aacs = Error::KeydbLoad {
        path: "<no keydb in search paths>".to_string(),
    };
    let msg = super::keyless_failure_message_for(Some(&css), Some(&aacs));
    let lower = msg.to_lowercase();
    assert!(
        lower.contains("unscramble") || lower.contains("css"),
        "css_error must take priority over aacs_error: {msg}"
    );
    assert!(
        msg.contains(&format!("E{}", libfreemkv::error::E_CSS_KEY_MISSING)),
        "expected CSS E-code: {msg}"
    );

    // css_error alone (aacs_error None) — the field-based branch is
    // consulted at all, not just the AACS fallback.
    let msg2 = super::keyless_failure_message_for(Some(&css), None);
    assert!(
        msg2.to_lowercase().contains("unscramble") || msg2.to_lowercase().contains("css"),
        "css_error-only disc must surface CSS messaging: {msg2}"
    );
}

#[test]
fn device_key_strips_unix_path() {
    // autorip keys its state map by the trailing path component
    // ("sg4", "disk2", "CdRom0"); `device_key` strips the leading
    // /dev/ or \\.\ prefix the lib returns in DriveInfo.path.
    assert_eq!(super::device_key("/dev/sg4"), "sg4");
    assert_eq!(super::device_key("/dev/disk2"), "disk2");
    assert_eq!(super::device_key("\\\\.\\CdRom0"), "CdRom0");
    assert_eq!(super::device_key("sg4"), "sg4"); // already a bare name
}

// ── abort-on-loss scoping (Top Gun false-positive regression) ────

/// A title spanning LBA 1000..2000 (sectors), i.e. byte range
/// 1000*2048 .. 2000*2048. `bytes_bad_in_title` intersects bad
/// ranges (byte offsets) with this window.
fn title_lba(start_lba: u32, sector_count: u32, bps: f64) -> libfreemkv::DiscTitle {
    let mut t = libfreemkv::DiscTitle::empty();
    t.extents.push(libfreemkv::disc::Extent {
        start_lba,
        sector_count,
    });
    // size/duration are only used by the caller to derive bps; here
    // we pass bps directly to the helpers, so leave them at zero.
    let _ = bps;
    t
}

#[test]
fn mux_denominator_scopes_to_title_extents_in_single_pass() {
    // 25 GB main title on a 50 GB disc.
    const GB: u64 = 1_073_741_824;
    let disc_capacity = 50 * GB;
    // A title whose single extent spans exactly 25 GB worth of sectors.
    let sectors = (25 * GB / 2048) as u32;
    let title = test_title(0, sectors);
    let extent_bytes = sectors as u64 * 2048;

    // Single-pass (max_retries == 0): denominator must be the title's
    // extent byte sum — the cap DiscStream's BytesRead reaches — so the
    // live progress bar reaches 100% instead of plateauing at ~50%.
    let single = super::mux_progress_denominator(0, disc_capacity, &title);
    assert_eq!(
        single, extent_bytes,
        "single-pass denominator must be the title extent sum, not disc capacity"
    );
    // Sanity: the old (buggy) behavior would have plateaued here.
    let old_pct = extent_bytes * 100 / disc_capacity;
    assert!(
        old_pct < 60,
        "precondition: title/disc ratio is the kind that plateaued ({old_pct}%)"
    );

    // Multipass (max_retries > 0): denominator stays disc capacity, since
    // the ISO highway reads the whole disc image.
    let multi = super::mux_progress_denominator(1, disc_capacity, &title);
    assert_eq!(
        multi, disc_capacity,
        "multipass denominator must remain disc capacity"
    );
}

#[test]
fn mux_denominator_falls_back_when_title_has_no_extents() {
    // Degenerate title with no extents → fall back to the passed total
    // rather than producing a zero denominator (divide-by-zero / no bar).
    let mut title = test_title(0, 1000);
    title.extents.clear();
    let total = 12345;
    assert_eq!(super::mux_progress_denominator(0, total, &title), total);
}

#[test]
fn abort_lost_ms_ignores_out_of_title_loss_for_mkv() {
    // Title occupies sectors 1000..2000; the only unreadable range is at
    // byte offset 0 (scratched menu, pre-title) and doesn't overlap it.
    let bps = 8_250_000.0;
    let title = title_lba(1000, 1000, bps);
    // 50 sectors bad starting at byte 0 (well before the title).
    let bad = vec![(0u64, 50 * 2048)];
    let lost = freemkv_engine::abort_lost_ms(false, &title, &bad, bps);
    assert_eq!(lost, 0.0, "out-of-title loss must not count for MKV mux");
}

#[test]
fn abort_lost_ms_counts_whole_disc_for_iso() {
    // Same out-of-title bad range, but ISO output → whole disc is
    // the deliverable, so it DOES count.
    let bps = 8_250_000.0;
    let title = title_lba(1000, 1000, bps);
    let bad = vec![(0u64, 50 * 2048)];
    let lost = freemkv_engine::abort_lost_ms(true, &title, &bad, bps);
    assert!(lost > 0.0, "ISO output counts whole-disc loss");
}

#[test]
fn abort_lost_ms_counts_in_title_loss_for_mkv() {
    // A bad range that overlaps the title extents counts.
    let bps = 8_250_000.0;
    let title = title_lba(1000, 1000, bps);
    // 10 bad sectors starting at sector 1500 (inside the title).
    let bad = vec![(1500u64 * 2048, 10 * 2048)];
    let lost = freemkv_engine::abort_lost_ms(false, &title, &bad, bps);
    assert!(lost > 0.0, "in-title loss must count for MKV mux");
}

#[test]
fn perfect_in_title_rip_does_not_abort_at_threshold_zero() {
    // THE regression: out-of-title unreadable + 0 in-title loss +
    // abort_on_lost_secs=0 (threshold 0 ms) → NO abort, proceed to
    // mux. Previously `>=` aborted because 0.0 >= 0.0.
    let bps = 8_250_000.0;
    let title = title_lba(1000, 1000, bps);
    let bad = vec![(0u64, 50 * 2048)]; // out-of-title only
    let in_title_lost_ms = freemkv_engine::abort_lost_ms(false, &title, &bad, bps);
    assert_eq!(in_title_lost_ms, 0.0);
    let abort_threshold_ms = 0.0; // abort_on_lost_secs = 0
    assert!(
        !freemkv_engine::should_abort_for_loss(in_title_lost_ms, abort_threshold_ms),
        "a fully-recovered title must NOT abort on out-of-title loss at threshold 0"
    );
}

#[test]
fn iso_output_rejected_in_single_pass_only() {
    // Regression: ISO is whole-disc scoped for abort accounting, but
    // single-pass reads only the title, so it would ACCEPT a disc that
    // multi-pass/resume ABORT on. ISO must require multi-pass to avoid that.
    assert!(
        super::iso_output_needs_multipass("iso", 0),
        "single-pass ISO must be rejected (it cannot honour whole-disc scope)"
    );
    // Multi-pass ISO is allowed (captures the whole-disc image + applies
    // whole-disc scope).
    assert!(!super::iso_output_needs_multipass("iso", 1));
    assert!(!super::iso_output_needs_multipass("iso", 5));
    // Non-ISO formats are unaffected in either mode.
    for fmt in ["mkv", "m2ts", "network"] {
        assert!(
            !super::iso_output_needs_multipass(fmt, 0),
            "{fmt} single-pass ok"
        );
        assert!(
            !super::iso_output_needs_multipass(fmt, 5),
            "{fmt} multi-pass ok"
        );
    }
}

#[test]
fn iso_output_delivers_the_disc_image_not_a_mux() {
    // Regression: rip paths used to default "iso" to an `.mkv` mux, then
    // PRUNE the swept ISO — the opposite of what was requested.
    // `output_is_iso_image` is now the single predicate those decisions key off.
    assert!(
        super::output_is_iso_image("iso"),
        "iso output must be recognised as a whole-disc deliverable"
    );
    for fmt in ["mkv", "m2ts", "network", "garbage", ""] {
        assert!(
            !super::output_is_iso_image(fmt),
            "{fmt} muxes a title and must NOT be treated as a disc image"
        );
    }
}

#[test]
fn iso_output_retains_its_disc_image_even_without_keep_iso() {
    // The swept ISO is the deliverable for iso output, so it must never be
    // pruned regardless of `keep_iso` — pruning it would leave the staging
    // dir with no file for the mover to promote.
    assert!(
        super::retain_intermediate_iso(false, "iso"),
        "iso output must retain its ISO even when keep_iso is off"
    );
    assert!(super::retain_intermediate_iso(true, "iso"));
    // `keep_iso` still governs ISO retention for the mux formats.
    assert!(super::retain_intermediate_iso(true, "mkv"));
    for fmt in ["mkv", "m2ts", "network"] {
        assert!(
            !super::retain_intermediate_iso(false, fmt),
            "{fmt} without keep_iso must prune the intermediate ISO"
        );
    }
}

#[test]
fn any_in_title_loss_aborts_at_threshold_zero() {
    // abort_on_lost_secs=0 still means "perfect in-title required":
    // ANY positive in-title loss aborts.
    let abort_threshold_ms = 0.0;
    assert!(freemkv_engine::should_abort_for_loss(
        0.001,
        abort_threshold_ms
    ));
    assert!(freemkv_engine::should_abort_for_loss(
        5_000.0,
        abort_threshold_ms
    ));
}

#[test]
fn loss_within_threshold_does_not_abort() {
    // abort_on_lost_secs=30 (30_000 ms): 20s lost is tolerated, 31s
    // aborts.
    let threshold = 30_000.0;
    assert!(!freemkv_engine::should_abort_for_loss(20_000.0, threshold));
    assert!(freemkv_engine::should_abort_for_loss(31_000.0, threshold));
}

#[test]
fn nan_loss_aborts_only_a_perfect_rip() {
    // An unquantifiable (NaN) loss aborts the byte-exact threshold-0 gate; the
    // seconds gate never stops a rip for a loss it cannot measure.
    assert!(freemkv_engine::loss_aborts(0, f64::NAN, 0));
    assert!(!freemkv_engine::should_abort_for_loss(f64::NAN, 0.0));
    assert!(!freemkv_engine::should_abort_for_loss(f64::NAN, 30_000.0));
}

// ── final done-card uses in-title loss (telemetry audit Fix 3) ── The `status=done` update
// must report in-title-scoped loss, not whole-disc bytes_unreadable/bps.
#[test]
fn final_done_card_uses_in_title_loss_not_whole_disc() {
    let bps = 8_250_000.0;
    // Title covers sectors 1000..2000. Damage is only in sector 0..50
    // (a scratched menu, before the title).
    let title = title_lba(1000, 1000, bps);
    let bad = vec![(0u64, 50 * 2048)]; // 50 sectors, all out-of-title

    // Whole-disc calculation (the old broken path): non-zero.
    let whole_disc_bytes_unreadable: u64 = 50 * 2048;
    let whole_disc_lost_secs = whole_disc_bytes_unreadable as f64 / bps;
    assert!(whole_disc_lost_secs > 0.0, "whole-disc loss is non-zero");

    // In-title-scoped calculation (the correct path via abort_lost_ms):
    // out-of-title damage does NOT count for MKV output.
    let in_title_lost_ms = freemkv_engine::abort_lost_ms(false, &title, &bad, bps);
    assert_eq!(
        in_title_lost_ms, 0.0,
        "in-title loss must be 0 when all bad sectors are outside title extents"
    );

    // The done headline reports in-title loss (0s), not whole-disc, for a clean mux.
    let (_, done_lost_secs, _) = super::done_headline(true, (50, 0.0), in_title_lost_ms, (0, 0.0));
    assert!(
        done_lost_secs.abs() < 0.001,
        "done card must report 0s lost, not the inflated whole-disc {:.3}s",
        whole_disc_lost_secs
    );
}

/// Sanity: when there IS in-title loss, the done card reports it.
#[test]
fn final_done_card_reports_nonzero_in_title_loss() {
    let bps = 8_250_000.0;
    let title = title_lba(1000, 1000, bps);
    // 10 sectors at LBA 1500 — inside the title.
    let bad = vec![(1500u64 * 2048, 10 * 2048)];
    let in_title_lost_ms = freemkv_engine::abort_lost_ms(false, &title, &bad, bps);
    assert!(in_title_lost_ms > 0.0, "in-title loss should be non-zero");
    let final_lost_secs = in_title_lost_ms / MILLIS_PER_SEC;
    // 10 sectors * 2048 bytes / 8_250_000 bps ≈ 0.00248s
    assert!(
        final_lost_secs > 0.0 && final_lost_secs < 1.0,
        "expected small non-zero lost_secs, got {:.6}",
        final_lost_secs
    );
}

// Regression: single-pass has no mapfile, so done-state `main_lost_ms` must derive from
// `final_lost_secs`, not the zero snapshot.
#[test]
fn single_pass_done_card_main_lost_ms_tracks_final_lost_secs() {
    // Snapshot is the all-zero Default in single-pass mode.
    let snapshot = super::mux::SweepDamageSnapshot::default();
    assert_eq!(
        snapshot.main_lost_ms, 0.0,
        "single-pass snapshot main_lost_ms is the zero Default"
    );

    // The mux reported real in-title loss (demux skipped sectors).
    let final_lost_secs = 1.5_f64;

    // Replicate the fix's branch selection for single-pass (max_retries == 0).
    let max_retries = 0u32;
    let main_lost_ms = if max_retries == 0 {
        final_lost_secs * MILLIS_PER_SEC
    } else {
        snapshot.main_lost_ms
    };
    let total_lost_ms = if max_retries == 0 {
        final_lost_secs * MILLIS_PER_SEC
    } else {
        snapshot.total_lost_ms
    };

    assert!(
        (main_lost_ms - 1500.0).abs() < 0.001,
        "single-pass main_lost_ms must reflect real loss, got {main_lost_ms}"
    );
    assert!(
        (main_lost_ms - total_lost_ms).abs() < 0.001,
        "single-pass main_lost_ms must mirror total_lost_ms"
    );
}

// Multipass keeps the snapshot's sweep loss AND folds in demux-time loss, matching
// single-pass and resume paths.
#[test]
fn multipass_done_card_main_lost_ms_uses_snapshot_plus_demux() {
    let snapshot = super::mux::SweepDamageSnapshot {
        main_lost_ms: 2750.0,
        total_lost_ms: 4000.0,
        ..Default::default()
    };
    let demux_lost_secs = 1.25_f64;
    let max_retries = 3u32;
    // Replicate the fix's done_demux_extra_ms branch.
    let done_demux_extra_ms = if max_retries == 0 {
        0.0
    } else {
        demux_lost_secs * MILLIS_PER_SEC
    };
    let main_lost_ms = if max_retries == 0 {
        0.0
    } else {
        snapshot.main_lost_ms + done_demux_extra_ms
    };
    let total_lost_ms = if max_retries == 0 {
        0.0
    } else {
        snapshot.total_lost_ms + done_demux_extra_ms
    };
    assert!(
        (main_lost_ms - 4000.0).abs() < 0.001,
        "multipass main_lost_ms must be sweep snapshot (2750) + demux (1250), got {main_lost_ms}"
    );
    assert!(
        (total_lost_ms - 5250.0).abs() < 0.001,
        "multipass total_lost_ms must be sweep snapshot (4000) + demux (1250), got {total_lost_ms}"
    );
}

// Regression (cross-path-asymmetry bug): an ACCEPTED fresh multipass done card must fold
// demux-time loss into headline errors/lost_secs, matching resume and single-pass.
#[test]
fn accepted_done_card_folds_demux_loss_into_headline() {
    // The production selection, with the sweep's read loss equal to `final_lost_secs`.
    fn headline(
        max_retries: u32,
        final_errors: u32,
        final_lost_secs: f64,
        mux_errors: u32,
        demux_lost_secs: f64,
    ) -> (u32, f64, f64) {
        super::done_headline(
            max_retries > 0,
            (final_errors, final_lost_secs),
            final_lost_secs * MILLIS_PER_SEC,
            (mux_errors, demux_lost_secs),
        )
    }

    // Single-pass: final_* already carry the demux figures (final_errors ==
    // mux_errors, final_lost_secs == demux_lost_secs). No addition, so no
    // double-counting.
    let (errs, lost, extra) = headline(0, 7, 1.5, 7, 1.5);
    assert_eq!(errs, 7, "single-pass errors unchanged");
    assert!((lost - 1.5).abs() < 0.001, "single-pass lost unchanged");
    assert!(
        (extra - 0.0).abs() < 0.001,
        "single-pass adds no demux extra"
    );

    // Multipass: final_errors is the mapfile bad-sector count (disjoint
    // from the mux demux skips); final_lost_secs is the sweep main loss.
    // Both must gain the demux contribution.
    let (errs, lost, extra) = headline(3, 4, 2.0, 5, 1.0);
    assert_eq!(errs, 9, "multipass errors = sweep 4 + demux 5");
    assert!(
        (lost - 3.0).abs() < 0.001,
        "multipass lost = sweep 2.0 + demux 1.0"
    );
    assert!(
        (extra - 1000.0).abs() < 0.001,
        "multipass demux extra = 1.0s in ms"
    );

    // A clean-mux multipass (zero demux loss) must equal the old behavior.
    let (errs, lost, extra) = headline(3, 4, 2.0, 0, 0.0);
    assert_eq!(
        errs, 4,
        "no demux loss leaves multipass errors at sweep count"
    );
    assert!(
        (lost - 2.0).abs() < 0.001,
        "no demux loss leaves multipass lost at sweep value"
    );
    assert!((extra - 0.0).abs() < 0.001, "no demux loss adds no extra");
}

// A multipass rip with no in-title read loss: `final_lost_secs` fell back to the demux
// loss itself, which the headline must count once, not twice.
#[test]
fn multipass_headline_counts_demux_loss_once_when_the_sweep_lost_nothing() {
    let (errs, lost, extra) = super::done_headline(true, (0, 0.4), 0.0, (2, 0.4));
    assert_eq!(errs, 2);
    assert!((lost - 0.4).abs() < 1e-9, "lost {lost}s, want 0.4s");
    assert!((extra - 400.0).abs() < 1e-9);
}

// ── loss-threshold decision (should_abort_for_loss) ────────────── Pins
// loss-from-skip-count → threshold math: lost_secs = skip_sectors*2048/bps.
fn single_pass_lost_secs(skip_sectors: u64, title_bytes_per_sec: f64) -> f64 {
    if title_bytes_per_sec > 0.0 {
        (skip_sectors as f64) * 2048.0 / title_bytes_per_sec
    } else {
        0.0
    }
}

#[test]
fn single_pass_any_loss_aborts_at_threshold_zero() {
    // abort_on_lost_secs=0 ("require a perfect rip"): the threshold
    // helper must report "abort" for ANY positive skip-derived loss.
    let bps = 8_250_000.0;
    let threshold_ms = 0.0; // abort_on_lost_secs = 0
    let lost = single_pass_lost_secs(10, bps); // 10 skipped sectors
    assert!(lost > 0.0, "skipped sectors must produce positive loss");
    assert!(
        freemkv_engine::should_abort_for_loss(lost * MILLIS_PER_SEC, threshold_ms),
        "single-pass rip with skipped sectors must abort at threshold 0"
    );
}

#[test]
fn single_pass_clean_rip_does_not_abort_at_threshold_zero() {
    // A perfect single-pass rip (zero skips) must NOT abort even at
    // threshold 0 — the gate uses strict `>`.
    let bps = 8_250_000.0;
    let threshold_ms = 0.0;
    let lost = single_pass_lost_secs(0, bps);
    assert_eq!(lost, 0.0);
    assert!(
        !freemkv_engine::should_abort_for_loss(lost * MILLIS_PER_SEC, threshold_ms),
        "a clean single-pass rip must NOT abort at threshold 0"
    );
}

#[test]
fn single_pass_loss_within_threshold_does_not_abort() {
    // abort_on_lost_secs=30: a single-pass rip whose skip-derived loss
    // is under 30s proceeds; over 30s aborts.
    let bps = 8_250_000.0;
    let threshold_ms = 30_000.0;
    // ~1000 skipped sectors ≈ 0.248s lost — well under 30s.
    let small = single_pass_lost_secs(1000, bps);
    assert!(
        !freemkv_engine::should_abort_for_loss(small * MILLIS_PER_SEC, threshold_ms),
        "single-pass loss under threshold must NOT abort, got {small:.3}s"
    );
    // Enough skips to exceed 30s: 30 * bps / 2048 sectors + slack.
    let big_sectors = (31.0 * bps / 2048.0) as u64;
    let big = single_pass_lost_secs(big_sectors, bps);
    assert!(
        freemkv_engine::should_abort_for_loss(big * MILLIS_PER_SEC, threshold_ms),
        "single-pass loss over threshold must abort, got {big:.3}s"
    );
}

// Regression (bug #1/#2): `.ripped` hand-off must write status="done", not
// "ripping"/"idle".
#[test]
fn handoff_status_is_done_read_complete() {
    let device = "sg_handoff_status_test";
    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "done".to_string(),
            progress_pct: 100,
            disc_present: true,
            ..Default::default()
        },
    );
    let (status, pct) = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .map(|s| (s.status.clone(), s.progress_pct))
        .unwrap_or_default();
    assert_eq!(
        status, "done",
        "handoff update_state must write status='done' (read complete), not 'ripping'/'idle'"
    );
    assert_eq!(pct, 100, "a read-complete done card must show 100%");
    // Cleanup: remove the synthetic device entry so it doesn't leak
    // into other tests that inspect STATE.
    super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

// Auto-eject timing contract: the drive ejects EXACTLY ONCE, at the
// `.ripped` hand-off, only when `auto_eject` is on, and NEVER from the
// synthetic `_mux` worker. All four completion sites gate on `should_auto_eject`.

#[test]
fn auto_eject_fires_for_real_device_when_enabled() {
    // A physical drive (sg0/sr1/…) with auto_eject on ejects.
    assert!(super::should_auto_eject(true, "sg0"));
    assert!(super::should_auto_eject(true, "sr1"));
    assert!(super::should_auto_eject(true, "sg12"));
}

#[test]
fn auto_eject_does_not_fire_when_disabled() {
    // auto_eject=false never ejects, regardless of device.
    assert!(!super::should_auto_eject(false, "sg0"));
    assert!(!super::should_auto_eject(false, "sr1"));
    assert!(!super::should_auto_eject(false, "_mux"));
}

#[test]
fn auto_eject_never_fires_from_synthetic_mux_device() {
    // `_mux` reaches completion AFTER the drive already ejected at
    // hand-off, so it must never eject again (may now hold a different
    // disc). The guard keys on the underscore prefix, refusing even with auto_eject on.
    assert!(!super::should_auto_eject(true, "_mux"));
    assert!(!super::should_auto_eject(true, "_move"));
    assert!(!super::should_auto_eject(true, "_anything"));
}

// Eject is "exactly once at read-complete": the `.ripped` hand-off ejects; the later mux
// worker (synthetic `_mux`) is refused.
#[test]
fn auto_eject_is_once_at_handoff_not_at_mux() {
    // Hand-off (real device, enabled): eject.
    assert!(
        super::should_auto_eject(true, "sg0"),
        "the physical drive must eject at the read-complete hand-off"
    );
    // Mux worker completing later (synthetic device): no second eject.
    assert!(
        !super::should_auto_eject(true, "_mux"),
        "the mux worker must NOT re-eject after the hand-off already did"
    );
}

// Regression: a poisoned config `RwLock` must NOT leave the tile wedged in "scanning" —
// `mark_config_lock_poisoned` must flip it to "error" with a populated last_error.
#[test]
fn config_lock_poisoned_marks_error_not_stuck_scanning() {
    let device = "sg_config_poison_test";
    // Simulate the pre-spawn claim: tile is already "scanning".
    assert!(super::try_claim_active(device).is_some());
    let claimed = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .map(|s| s.status.clone())
        .unwrap_or_default();
    assert_eq!(claimed, "scanning", "claim should set status=scanning");

    // The poisoned-lock early-exit path.
    super::mark_config_lock_poisoned(device, "Scan");

    let st = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned()
        .expect("device state present");
    assert_eq!(
        st.status, "error",
        "poisoned config lock must mark the tile 'error', not leave it 'scanning'"
    );
    assert!(
        !st.last_error.is_empty(),
        "poisoned config lock must populate last_error so the operator sees why"
    );

    // Cleanup so the synthetic device doesn't leak into other tests.
    super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

// Regression: end-of-recovery promotion must flush the promoted mapfile so the abort check
// sees Unreadable, not stale NonTrimmed.
#[test]
fn promotion_uses_in_memory_map_and_flush_persists_to_disk() {
    use freemkv_engine::{Mapfile, SectorStatus};

    let tmp = tempfile::tempdir().unwrap();
    let mf_path = tmp.path().join("test.mapfile");

    // Create a mapfile with one NonTrimmed range (simulating a sector
    // that remained "maybe" after all patch passes).
    let disc_size: u64 = 10 * 2048;
    let bad_pos: u64 = 5 * 2048;
    let bad_size: u64 = 2048;
    {
        let mut map = Mapfile::create(&mf_path, disc_size, "test").expect("create mapfile");
        // Mark everything Finished except one NonTrimmed range.
        map.record(0, bad_pos, SectorStatus::Finished)
            .expect("record Finished before bad");
        map.record(bad_pos, bad_size, SectorStatus::NonTrimmed)
            .expect("record NonTrimmed");
        map.record(
            bad_pos + bad_size,
            disc_size - bad_pos - bad_size,
            SectorStatus::Finished,
        )
        .expect("record Finished after bad");
        map.flush().expect("initial flush");
    }

    // Simulate the promotion block: load, promote, flush.
    {
        let mut map = Mapfile::load(&mf_path).expect("load for promotion");
        let nontrimmed = map.ranges_with(&[SectorStatus::NonTrimmed]);
        assert_eq!(nontrimmed.len(), 1, "precondition: one NonTrimmed range");
        for (pos, size) in nontrimmed {
            map.record(pos, size, SectorStatus::Unreadable)
                .expect("promote record");
        }
        // The flush is the critical step the pre-fix code omitted.
        map.flush().expect("promotion flush");

        // Verify in-memory state reflects the promotion.
        let stats = map.stats();
        assert_eq!(
            stats.bytes_unreadable, bad_size,
            "in-memory bytes_unreadable must equal the promoted range size"
        );
        assert_eq!(
            stats.bytes_nontried + stats.bytes_pending,
            0,
            "no NonTrimmed/NonTried must remain after promotion"
        );

        // The abort check now uses this same `map` — verify bad_ranges is
        // populated from it (the pre-fix re-load would return empty here
        // because the flush wasn't done).
        let bad_ranges = map.ranges_with(&[SectorStatus::Unreadable]);
        assert_eq!(
            bad_ranges.len(),
            1,
            "abort check must see one Unreadable range from the promoted in-memory map"
        );
    }

    // Verify the flush wrote the promoted state to disk: a fresh load must
    // see Unreadable, not NonTrimmed.
    let reloaded = Mapfile::load(&mf_path).expect("reload after promotion flush");
    let reloaded_unreadable = reloaded.ranges_with(&[SectorStatus::Unreadable]);
    assert_eq!(
        reloaded_unreadable.len(),
        1,
        "reloaded mapfile must contain the promoted Unreadable range \
             (pre-fix: flush omitted, so disk still held NonTrimmed)"
    );
    let reloaded_nontrimmed = reloaded.ranges_with(&[SectorStatus::NonTrimmed]);
    assert_eq!(
        reloaded_nontrimmed.len(),
        0,
        "reloaded mapfile must have no NonTrimmed after flush"
    );
}

// Regression: `.ripped` hand-off update_state must preserve non-zero damage fields (was
// zeroed by `..Default::default()`).
#[test]
fn handoff_update_state_carries_damage_fields() {
    let device = "sg_handoff_damage_test";
    // Seed STATE with damage-populated entry (as push_pass_state would).
    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "ripping".to_string(),
            errors: 42,
            total_lost_ms: 1500.0,
            main_lost_ms: 800.0,
            num_bad_ranges: 3,
            largest_gap_ms: 600.0,
            ..Default::default()
        },
    );

    let row = super::handoff_done_row(
        device,
        super::RipState {
            device: device.to_string(),
            disc_present: true,
            output_file: "Film.mkv".to_string(),
            ..Default::default()
        },
    );
    assert_eq!(row.status, "done", "the hand-off reports the read complete");
    assert_eq!(row.progress_pct, 100);
    assert_eq!(row.output_file, "Film.mkv", "card fields pass through");
    super::update_state(device, row);

    let state = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned()
        .expect("device should be in STATE");

    assert_eq!(state.errors, 42, "handoff must carry errors from sweep");
    assert!(
        (state.total_lost_ms - 1500.0).abs() < 0.001,
        "handoff must carry total_lost_ms from sweep"
    );
    assert!(
        (state.main_lost_ms - 800.0).abs() < 0.001,
        "handoff must carry main_lost_ms from sweep"
    );
    assert_eq!(
        state.num_bad_ranges, 3,
        "handoff must carry num_bad_ranges from sweep"
    );
    assert!(
        (state.largest_gap_ms - 600.0).abs() < 0.001,
        "handoff must carry largest_gap_ms from sweep"
    );

    // Cleanup.
    super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

// Regression guard for the `entries.flatten()` silent-drop bug: staging-root
// walks now route through `list_staging_basenames`, which lists every child
// and retries/surfaces per-DirEntry NFS errors instead of silently undercounting.
#[test]
fn list_staging_basenames_returns_all_children() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join("Redshift")).unwrap();
    std::fs::create_dir(tmp.path().join("Redshift_2")).unwrap();
    std::fs::write(tmp.path().join("loose.txt"), b"x").unwrap();

    let mut got = list_staging_basenames(tmp.path()).expect("dir exists");
    got.sort();
    assert_eq!(got, vec!["Redshift", "Redshift_2", "loose.txt"]);
}

#[test]
fn list_staging_basenames_empty_dir_is_some_empty() {
    // A genuinely empty staging root must return Some([]) (a trustworthy
    // "no match"), not None — None is reserved for "never opened".
    let tmp = tempfile::TempDir::new().unwrap();
    assert_eq!(list_staging_basenames(tmp.path()), Some(Vec::new()));
}

#[test]
fn list_staging_basenames_missing_dir_is_none() {
    // read_dir never opens -> UNKNOWN -> None, so callers behave exactly
    // like the old `read_dir(...).ok()? / return false` (no false match).
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist");
    assert_eq!(list_staging_basenames(&missing), None);
}

// Regression for the largest-count-vs-union bug: a clean pass returns the
// UNION of every basename seen, never duplicating one. Real FS can't inject
// the per-DirEntry errors for cross-pass union, so this pins the wiring.
#[test]
fn list_staging_basenames_union_does_not_duplicate() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(tmp.path().join("Wraithline")).unwrap();
    std::fs::create_dir(tmp.path().join("Wraithline_Part_Two")).unwrap();

    let got = list_staging_basenames(tmp.path()).expect("dir exists");
    assert_eq!(got.len(), 2, "each child appears exactly once: {got:?}");
    assert!(got.contains(&"Wraithline".to_string()));
    assert!(got.contains(&"Wraithline_Part_Two".to_string()));
}

// Regression: `resumable_for_disc` must find an existing resumable staging dir via
// `list_staging_basenames` (3-retry NFS defense), not a bare `read_dir().flatten()`.
#[test]
fn resumable_for_disc_detects_partial_sweep() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = crate::server::config::Config {
        staging_dir: tmp.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let display_name = "Test Disc";
    let sanitized = crate::server::util::sanitize_path_compact(display_name);

    // Build a real staging layout: <staging>/<sanitized>/<sanitized>.iso
    // plus its `<...>.iso.mapfile`. A freshly created mapfile is one big
    // NonTried region (bytes_pending > 0) -> Resumable::Sweep.
    let disc_dir = tmp.path().join(&sanitized);
    std::fs::create_dir(&disc_dir).unwrap();
    let iso = disc_dir.join(format!("{sanitized}.iso"));
    std::fs::write(&iso, b"x").unwrap();
    let mapfile_path = disc_dir.join(format!("{sanitized}.iso.mapfile"));
    freemkv_engine::Mapfile::create(&mapfile_path, 4096, "test").unwrap();

    assert_eq!(
        resumable_for_disc(&cfg, display_name, ""),
        Some(Resumable::Sweep),
    );
}

// R3 finding 1 regression: `resumable_for_disc` must return None when the dir carries a
// terminal `.failed` or held `.review` marker, even with pending bytes.
#[test]
fn resumable_for_disc_blocked_by_failed_or_review() {
    let display_name = "Stranded Disc";
    let sanitized = crate::server::util::sanitize_path_compact(display_name);

    for marker in [".failed", ".review"] {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = crate::server::config::Config {
            staging_dir: tmp.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        // Partial sweep (bytes_pending > 0) that WOULD be Resumable::Sweep…
        let disc_dir = tmp.path().join(&sanitized);
        std::fs::create_dir(&disc_dir).unwrap();
        let iso = disc_dir.join(format!("{sanitized}.iso"));
        std::fs::write(&iso, b"x").unwrap();
        let mapfile_path = disc_dir.join(format!("{sanitized}.iso.mapfile"));
        freemkv_engine::Mapfile::create(&mapfile_path, 4096, "test").unwrap();
        // Sanity: without the terminal/held marker it IS Sweep-resumable.
        assert_eq!(
            resumable_for_disc(&cfg, display_name, ""),
            Some(Resumable::Sweep),
            "precondition: partial sweep is resumable before {marker}"
        );
        // …but a terminal/held marker blocks the Resume affordance entirely.
        std::fs::write(disc_dir.join(marker), b"{}").unwrap();
        assert_eq!(
            resumable_for_disc(&cfg, display_name, ""),
            None,
            "{marker} must suppress the Resume affordance (operator must Wipe)"
        );
    }
}

// Owner decision #2 regression: `resumable_for_disc` must return None when the dir is owned
// by the mux worker (`.ripped`/`.muxing`).
#[test]
fn resumable_for_disc_blocked_when_owned_by_mux_worker() {
    let display_name = "Mid Mux Disc";
    let sanitized = crate::server::util::sanitize_path_compact(display_name);

    for marker in [".ripped", ".muxing"] {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = crate::server::config::Config {
            staging_dir: tmp.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        // Partial sweep (bytes_pending > 0) that WOULD be Resumable::Sweep…
        let disc_dir = tmp.path().join(&sanitized);
        std::fs::create_dir(&disc_dir).unwrap();
        let iso = disc_dir.join(format!("{sanitized}.iso"));
        std::fs::write(&iso, b"x").unwrap();
        let mapfile_path = disc_dir.join(format!("{sanitized}.iso.mapfile"));
        freemkv_engine::Mapfile::create(&mapfile_path, 4096, "test").unwrap();
        // Sanity: without the worker-owned marker it IS Sweep-resumable.
        assert_eq!(
            resumable_for_disc(&cfg, display_name, ""),
            Some(Resumable::Sweep),
            "precondition: partial sweep is resumable before {marker}"
        );
        // …but a worker-owned marker blocks the Resume affordance entirely.
        std::fs::write(disc_dir.join(marker), b"{}").unwrap();
        assert_eq!(
            resumable_for_disc(&cfg, display_name, ""),
            None,
            "{marker} must suppress Resume (mux worker owns the dir)"
        );
    }
}

/// Build `<root>/<sanitized>` and drop the named empty marker files in it.
fn staging_disc_with_markers(
    root: &std::path::Path,
    sanitized: &str,
    markers: &[&str],
) -> std::path::PathBuf {
    let disc = root.join(sanitized);
    std::fs::create_dir_all(&disc).unwrap();
    for m in markers {
        std::fs::write(disc.join(m), b"{}").unwrap();
    }
    disc
}

// M4: a rip HELD for review writes BOTH `.review` and `.completed`;
// "already ripped" must gate on `.completed` AND NOT `.review`.
#[test]
fn snapshot_hold_does_not_read_held_for_review_as_completed() {
    let tmp = tempfile::TempDir::new().unwrap();
    let san = "Held_Movie";
    let dir = tmp.path().join(san);
    std::fs::create_dir_all(&dir).unwrap();
    // .completed alone → already ripped.
    staging::write_completed_marker(&dir);
    assert!(
        completed_hold(tmp.path(), san),
        ".completed alone must count as already-ripped"
    );
    // Hold for review → NO longer "already ripped" (state becomes Review,
    // which still counts as `completed` but is excluded by `!has_review`).
    staging::mark_handoff(&dir, false, |_| {}).unwrap();
    assert!(
        !completed_hold(tmp.path(), san),
        ".completed + review-hold must NOT count as already-ripped (M4)"
    );
}

// R2 finding 2 regression: `snapshot_hold` must read markers through NFS-resilient
// `snapshot_staging_disc`, not bare `.exists()`.
#[test]
fn snapshot_hold_sees_completed_with_leftover_artifacts() {
    let tmp = tempfile::TempDir::new().unwrap();
    let san = "Finished_Movie";
    // Completed rip whose ISO/mapfile haven't been pruned yet (crash
    // between .completed and the ISO prune, or mover not yet run).
    staging_disc_with_markers(
        tmp.path(),
        san,
        &[
            ".completed",
            "Finished_Movie.iso",
            "Finished_Movie.iso.mapfile",
        ],
    );
    assert!(
        completed_hold(tmp.path(), san),
        ".completed must be detected via snapshot even with leftover ISO/mapfile"
    );
    // No .completed at all → not completed (snapshot agrees).
    let other = "Unfinished_Movie";
    staging_disc_with_markers(
        tmp.path(),
        other,
        &["Unfinished_Movie.iso", "Unfinished_Movie.iso.mapfile"],
    );
    assert!(
        !completed_hold(tmp.path(), other),
        "no .completed → not already-completed"
    );
}

// M4 sanity: `list_held` still surfaces a held dir even when
// `.completed` is also present (keys on `.review`, independent).
#[test]
fn list_held_still_sees_completed_review_dir() {
    let tmp = tempfile::TempDir::new().unwrap();
    let disc = tmp.path().join("Held_Movie");
    std::fs::create_dir_all(&disc).unwrap();
    std::fs::write(disc.join(".review"), r#"{"title":"Held Movie","year":0}"#).unwrap();
    std::fs::write(disc.join(".completed"), b"").unwrap();
    std::fs::write(disc.join("Held_Movie.mkv"), b"x").unwrap();

    let held = crate::server::review::list_held(tmp.path().to_str().unwrap());
    assert_eq!(held.len(), 1, "a .completed+.review dir is still held");
    assert_eq!(held[0].dir, "Held_Movie");
}

// H1: a `.ripped`/`.muxing` dir is OWNED by the mux worker; auto-insert
// must not run a fresh sweep that truncates its ISO.
#[test]
fn staging_disc_owned_by_worker_detects_ripped_and_muxing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let san = "Owned";
    // Nothing yet → not owned.
    let dir = staging_disc_with_markers(tmp.path(), san, &["Owned.iso", "Owned.iso.mapfile"]);
    assert!(!staging_disc_owned_by_worker(tmp.path(), san));
    // Ripped → owned.
    let marker = crate::server::muxer::RippedMarker {
        schema_version: crate::server::muxer::RIPPED_MARKER_SCHEMA,
        iso_path: dir.join("Owned.iso").to_string_lossy().into_owned(),
        mapfile_path: dir.join("Owned.iso.mapfile").to_string_lossy().into_owned(),
        display_name: "Owned".into(),
        disc_format: "bd".into(),
        mkv_filename: "Owned.mkv".into(),
        tmdb_title: "Owned".into(),
        tmdb_year: 0,
        tmdb_poster: String::new(),
        tmdb_overview: String::new(),
        tmdb_media_type: String::new(),
        max_retries: 1,
        abort_on_lost_secs: 0,
        rip_elapsed_secs: 0.0,
        rip_errors: 0,
        rip_lost_video_secs: 0.0,
        rip_last_sector: 0,
        origin_device: "sr0".into(),
        sweep_errors: 0,
        sweep_total_lost_ms: 0.0,
        sweep_main_lost_ms: 0.0,
        sweep_num_bad_ranges: 0,
        sweep_largest_gap_ms: 0.0,
        title_confident: false,
    };
    crate::server::muxer::write_marker(&dir, &marker).unwrap();
    assert!(
        staging_disc_owned_by_worker(tmp.path(), san),
        "Ripped state must mark the dir owned by the mux worker"
    );
    // Ripped + muxing lock held → still owned.
    staging::write_muxing_marker(&dir);
    assert!(
        staging_disc_owned_by_worker(tmp.path(), san),
        "muxing lock must mark the dir owned by the mux worker"
    );
}

// An unmeasurable (NaN) loss with real lost bytes aborts a perfect rip on the bytes; a
// tolerance-configured rip consults seconds only, and a NaN never aborts there.
#[test]
fn unquantifiable_loss_aborts_only_at_threshold_zero() {
    use freemkv_engine::loss_aborts;
    // Zero bitrate → ms is NaN. Real lost bytes, perfect-rip threshold.
    assert!(
        loss_aborts(4096, f64::NAN, 0),
        "lost bytes with an unmeasurable duration must abort at threshold 0"
    );
    // Same unmeasurable loss under a seconds tolerance: bytes are not consulted
    // and a NaN never aborts.
    assert!(
        !loss_aborts(4096, f64::NAN, 3600),
        "the seconds gate never aborts on an unmeasurable loss"
    );
    // Sanity: a genuinely clean rip still proceeds on both branches.
    assert!(
        !loss_aborts(0, 0.0, 0),
        "a clean rip proceeds at threshold 0"
    );
    assert!(
        !loss_aborts(0, 0.0, 30),
        "a clean rip proceeds under a tolerance"
    );
}

// H1 + M3: drive-resume (Remux) selector must skip owned/held/
// terminal dirs, drives real snapshots through `resumable_dir_blocked`.
#[test]
fn resumable_dir_blocked_skips_owned_held_and_terminal() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mk = |name: &str, markers: &[&str]| {
        let d = staging_disc_with_markers(
            tmp.path(),
            name,
            &[&format!("{name}.iso"), &format!("{name}.iso.mapfile")],
        );
        for m in markers {
            std::fs::write(d.join(m), b"{}").unwrap();
        }
        crate::server::ripper::staging::snapshot_staging_disc(&d).unwrap()
    };

    // Plain ISO+mapfile, no governing marker → NOT blocked (resumable).
    assert!(!resumable_dir_blocked(&mk("Plain", &[])));
    // Owned by mux worker.
    assert!(resumable_dir_blocked(&mk("Ripped", &[".ripped"])));
    assert!(resumable_dir_blocked(&mk("Muxing", &[".muxing"])));
    // Held for operator review.
    assert!(resumable_dir_blocked(&mk("Held", &[".review"])));
    // Terminal — including a non-JSON `.failed` body (presence-keyed, M3).
    let failed =
        staging_disc_with_markers(tmp.path(), "Failed", &["Failed.iso", "Failed.iso.mapfile"]);
    std::fs::write(failed.join(".failed"), b"cancelled by operator\n").unwrap();
    let snap = crate::server::ripper::staging::snapshot_staging_disc(&failed).unwrap();
    assert!(snap.has_failed && snap.failed_reason.is_none());
    assert!(
        resumable_dir_blocked(&snap),
        "non-JSON .failed must still block drive-resume (presence-keyed)"
    );
}

#[test]
fn effective_abort_secs_forces_iso_to_zero() {
    use super::effective_abort_secs;
    // ISO output is whole-disc and must be byte-complete: the per-title
    // tolerance is IGNORED (forced to 0 = require 100%), no matter what was
    // configured (e.g. left over from a prior MKV rip).
    assert_eq!(effective_abort_secs("iso", 0), 0);
    assert_eq!(
        effective_abort_secs("iso", 30),
        0,
        "iso must ignore a stored MKV tolerance"
    );
    assert_eq!(effective_abort_secs("iso", 999), 0);
    // Muxed outputs pass the configured value through unchanged.
    assert_eq!(effective_abort_secs("mkv", 30), 30);
    assert_eq!(effective_abort_secs("m2ts", 5), 5);
    assert_eq!(effective_abort_secs("network", 0), 0);
}

#[test]
fn iso_aborts_on_any_loss_despite_configured_tolerance() {
    use super::effective_abort_secs;
    use freemkv_engine::loss_aborts;
    // Bug scenario: a 30s tolerance configured for MKV, then output switched
    // to ISO. The raw config would WRONGLY tolerate a small whole-disc loss…
    let configured = 30u64;
    let lost_bytes = 2048; // one unreadable sector
    let lost_ms = 100.0; // trivial duration — would pass a 30s threshold
    assert!(
        !loss_aborts(lost_bytes, lost_ms, configured),
        "raw stored 30s threshold would tolerate the loss — the cosmetic-only bug"
    );
    // …but the EFFECTIVE iso threshold (0) aborts on any lost byte:
    assert!(
        loss_aborts(lost_bytes, lost_ms, effective_abort_secs("iso", configured)),
        "iso must abort on ANY whole-disc loss regardless of stored tolerance"
    );
    // …while MKV keeps tolerating within its configured threshold:
    assert!(
        !loss_aborts(lost_bytes, lost_ms, effective_abort_secs("mkv", configured)),
        "mkv still tolerates loss within its configured threshold"
    );
}

#[test]
fn accept_loss_override_threshold_proceeds_even_for_a_nan_loss() {
    use freemkv_engine::loss_aborts;
    // The `.accept-loss` override raises the effective threshold to u64::MAX.
    // A real, large in-title loss must then PROCEED (deliver despite damage)…
    assert!(
        !loss_aborts(1_000_000_000, 2_370.0, u64::MAX),
        "operator override (u64::MAX threshold) must deliver despite 2.37s in-movie loss"
    );
    // …and so must an unquantifiable (NaN) one: the override is a positive
    // threshold, and the seconds gate never aborts on a NaN.
    assert!(
        !loss_aborts(0, f64::NAN, u64::MAX),
        "a NaN loss does not abort under the accept-loss override"
    );
}

#[test]
fn is_halt_error_matches_only_the_leading_code_token() {
    use super::is_halt_error;
    // The real Halted → io::Error conversion must classify as a halt.
    let halted: std::io::Error = libfreemkv::Error::Halted.into();
    assert!(
        is_halt_error(&halted),
        "Error::Halted must classify as a halt"
    );
    assert!(
        is_halt_error(&std::io::Error::other("E6010")),
        "bare E6010 (Halted has no payload) must match"
    );
    // Structural failures must NOT be masked as a halt — including the exact
    // round-4 adversarial case: a NoDiscKey whose hex disc-hash payload merely
    // CONTAINS the digits E6010.
    assert!(
        !is_halt_error(&std::io::Error::other("E7022: 0x1234E6010ABCD")),
        "a NoDiscKey hash containing E6010 must NOT be read as a halt"
    );
    assert!(
        !is_halt_error(&std::io::Error::other("E7023: css key missing")),
        "CssKeyMissing must still quarantine, not mask as a halt"
    );
    assert!(
        !is_halt_error(&std::io::Error::other("E60100: some other code")),
        "a longer code with E6010 as a prefix must NOT match"
    );
}

// Mux-time loss gate is the sole enforcement point for decrypt/codec loss; table-drives
// every axis after a mutation run flipped inline conditions unnoticed.
#[test]
fn mux_loss_gate_fires_only_on_mux_contributed_loss_over_threshold() {
    use super::mux_loss_aborts;

    // The case the gate exists for: mux contributed loss over threshold.
    assert!(mux_loss_aborts(true, false, 5.0, 5.0, 2));
    // ...and at zero tolerance, any mux loss at all.
    assert!(mux_loss_aborts(true, false, 0.5, 0.5, 0));

    // Exactly at the threshold is NOT over it.
    assert!(
        !mux_loss_aborts(true, false, 2.0, 2.0, 2),
        "the comparison is strictly greater-than"
    );

    // A failed mux never reaches the gate — the failure path owns it.
    assert!(!mux_loss_aborts(false, false, 5.0, 5.0, 2));

    // ISO output is whole-disc and gated at 100% elsewhere.
    assert!(
        !mux_loss_aborts(true, true, 5.0, 5.0, 2),
        "ISO deliverables are exempt from the mux-time gate"
    );

    // Read-time loss alone already passed the PRE-mux gate. Re-gating it
    // here would double-count and quarantine a rip the operator accepted.
    assert!(
        !mux_loss_aborts(true, false, 99.0, 0.0, 2),
        "no mux-contributed loss means this gate must not fire"
    );
    assert!(
        !mux_loss_aborts(true, false, 99.0, 0.0, 0),
        "...including at zero tolerance"
    );

    // A NaN demux loss is not mux-contributed loss (NaN comparisons are false), so the
    // gate does not fire; the mux reports 0.0, never NaN, for a zero-bitrate title.
    assert!(!mux_loss_aborts(true, false, f64::NAN, f64::NAN, 0));
}

#[test]
fn loss_aborts_zero_threshold_is_byte_exact() {
    use freemkv_engine::loss_aborts;
    // abort_on_lost_secs == 0 → ZERO: any lost byte aborts, regardless of
    // the (bitrate-derived) seconds estimate; exactly zero bytes proceeds.
    assert!(
        loss_aborts(1, 0.0, 0),
        "1 lost byte must abort at threshold 0"
    );
    assert!(
        !loss_aborts(0, 12_345.0, 0),
        "0 lost bytes proceeds at threshold 0 even if the seconds estimate is nonzero"
    );
    assert!(
        loss_aborts(0, f64::NAN, 0),
        "NaN loss fails safe to abort even at threshold 0"
    );
    // abort_on_lost_secs > 0 → seconds threshold (lost_ms is MILLISECONDS,
    // threshold is seconds*1000); bytes are not consulted on this path.
    assert!(
        !loss_aborts(9_999_999, 999.0, 1),
        "999ms under a 1000ms (1s) threshold proceeds (bytes ignored on the seconds path)"
    );
    assert!(
        loss_aborts(0, 1001.0, 1),
        "1001ms over a 1000ms (1s) threshold aborts"
    );
    assert!(
        !loss_aborts(0, 1000.0, 1),
        "exactly 1000ms at a 1s threshold proceeds (strictly greater-than aborts)"
    );
    assert!(
        !loss_aborts(0, f64::NAN, 30),
        "a NaN loss never aborts on the seconds path"
    );
}

// Auto-file (.done) vs hold-for-review (.review): `title_is_confident`
// + `handoff_marker_name` pin auto-vs-manual for both completion routes.
// The three ways a rip earns `.done`, and the one way it doesn't.
#[test]
fn title_confidence_is_key_absent_or_overridden_or_exact_match() {
    use super::title_is_confident;

    // Baseline: key configured, no override, and a label the resolved title
    // does not support → a GUESS. Hold for review.
    assert!(
        !title_is_confident("tmdb-key", false, "BD_ROM_R1", "Casablanca", 1942),
        "a title the operator never confirmed and the label doesn't support must be held"
    );

    // Term 1 — no TMDB key configured. No rip can ever match, so gating on
    // the match would park every rip in `.review` forever.
    assert!(
        title_is_confident("", false, "BD_ROM_R1", "Casablanca", 1942),
        "with no TMDB key the disc-label filename is expected, not a review hold"
    );
    assert!(
        title_is_confident("   \t ", false, "BD_ROM_R1", "Casablanca", 1942),
        "a whitespace-only key is 'not configured' too"
    );

    // Term 2 — the operator picked the title by hand. Nothing beats that.
    assert!(
        title_is_confident("tmdb-key", true, "BD_ROM_R1", "Casablanca", 1942),
        "an operator override is confident by definition"
    );

    // Term 3 — exact label/title match carrying a year.
    assert!(
        title_is_confident("tmdb-key", false, "THE_MATRIX", "The Matrix", 1999),
        "an exact title match with a year is a confident TMDB match"
    );
    // …and the year is load-bearing: a yearless match is not confident.
    assert!(
        !title_is_confident("tmdb-key", false, "THE_MATRIX", "The Matrix", 0),
        "a yearless match is not confident (the mover would file it without a year)"
    );
}

// Confident → hand to the mover; not confident → hold in staging. Both completion paths
// hand off through `staging::mark_handoff` and name it with `staging::handoff_label`.
#[test]
fn handoff_marker_is_done_only_for_a_confident_title() {
    use crate::server::ripper::staging::{StagingState, handoff_label, mark_handoff, read_state};
    assert_eq!(
        handoff_label(true),
        ".done",
        "a confident title is handed to the mover"
    );
    assert_eq!(
        handoff_label(false),
        ".review",
        "an uncertain title is HELD in staging for the operator, never auto-filed"
    );
    for (confident, want) in [(true, StagingState::Done), (false, StagingState::Review)] {
        let tmp = tempfile::tempdir().unwrap();
        mark_handoff(tmp.path(), confident, |_| {}).expect("hand-off");
        assert_eq!(read_state(tmp.path()).expect("state").state, want);
    }
}

// Encrypted-disc keyless retry gate: the outage retry REPLACES the
// `Disc`, so it must fire only for encrypted+keyless+online+capture off.
#[test]
fn online_key_retry_fires_only_for_an_encrypted_keyless_online_disc() {
    use super::should_retry_online_keys;

    assert!(
        should_retry_online_keys(true, false, true, true),
        "online + encrypted + no keys + capture off is the retry case"
    );
    assert!(
        !should_retry_online_keys(false, false, true, true),
        "no online key source → nothing to retry"
    );
    assert!(
        !should_retry_online_keys(true, true, true, true),
        "capture-without-keys keeps its untouched ISO-now path"
    );
    assert!(
        !should_retry_online_keys(true, false, false, true),
        "an unencrypted disc needs no key and must not be re-read"
    );
    assert!(
        !should_retry_online_keys(true, false, true, false),
        "keys already resolved → no outage to recover from"
    );
    // The one an `&&`→`||` slip makes catastrophic: nothing set at all.
    assert!(
        !should_retry_online_keys(false, true, false, false),
        "no condition met must never enter the retry path"
    );
}

// Read-error concealment: only the literal `"skip"` enables
// zero-fill; a `"stop"` operator must never get errors filled in.
#[test]
fn only_on_read_error_skip_enables_zero_fill() {
    use super::skip_read_errors;
    assert!(skip_read_errors("skip"), "'skip' conceals read errors");
    assert!(
        !skip_read_errors("stop"),
        "'stop' must surface the read error, not zero-fill it"
    );
    assert!(
        !skip_read_errors(""),
        "an unset value must not enable concealment"
    );
    assert!(
        !skip_read_errors("SKIP"),
        "only the exact configured token enables concealment"
    );
}

// Header-phase disposition: one predicate routes the mux outcome so `output_opened` is
// consulted exactly once.
#[test]
fn header_phase_routes_opened_failed_and_clean_stop_apart() {
    use super::{HeaderPhase, header_phase_disposition};
    assert_eq!(
        header_phase_disposition(true, None),
        HeaderPhase::Produced,
        "an opened output continues to the normal completion path"
    );
    assert_eq!(
        header_phase_disposition(true, Some("late finalize failure")),
        HeaderPhase::Produced,
        "a finalize error AFTER the output opened belongs to the post-finalize \
             path, not to the header-phase quarantine"
    );
    assert_eq!(
        header_phase_disposition(false, Some("header buffer cap exceeded")),
        HeaderPhase::Failed("header buffer cap exceeded"),
        "no output plus a recorded reason is terminal — quarantine it"
    );
    assert_eq!(
        header_phase_disposition(false, None),
        HeaderPhase::ResumableStop,
        "no output and no reason is a clean stop; the dir stays resumable"
    );
}

// Single-pass vs multipass: the boundary at 0/1. Every route decision along `rip_disc` keys
// off this predicate.
#[test]
fn multipass_starts_at_one_retry() {
    use super::uses_multipass;
    assert!(
        !uses_multipass(0),
        "max_retries=0 is single-pass: no ISO, no mapfile"
    );
    assert!(
        uses_multipass(1),
        "max_retries=1 already sweeps to an ISO and runs a patch pass"
    );
    assert!(uses_multipass(2));
    assert!(uses_multipass(u8::MAX));
}

/// ISO output is only deliverable from the multipass route (single-pass
/// captures no whole-disc image), keyed off the same predicate.
#[test]
fn iso_output_requires_the_multipass_route() {
    use super::iso_output_needs_multipass;
    assert!(
        iso_output_needs_multipass("iso", 0),
        "single-pass ISO has no image to deliver — must be rejected"
    );
    assert!(!iso_output_needs_multipass("iso", 1));
    assert!(!iso_output_needs_multipass("mkv", 0));
}

// Disc-identity guards for the unattended auto-insert path: these pin the
// STATE/Config wrappers it calls. `→ false` re-rips a finished disc and
// O_TRUNCs the staged ISO still being read; `→ true` wedges every disc as done.

fn completed_hold(root: &std::path::Path, san: &str) -> bool {
    staging::snapshot_staging_disc(&root.join(san)).and_then(|s| super::snapshot_hold(&s))
        == Some(super::StagingHold::Completed)
}

fn already_completed(
    cfg: &std::sync::Arc<std::sync::RwLock<crate::server::config::Config>>,
    device: &str,
) -> bool {
    super::disc_staging_hold(cfg, device, false) == Some(super::StagingHold::Completed)
}

fn loss_aborted(
    cfg: &std::sync::Arc<std::sync::RwLock<crate::server::config::Config>>,
    device: &str,
) -> bool {
    super::disc_staging_hold(cfg, device, false) == Some(super::StagingHold::LossAborted)
}

/// Seed `STATE[device].disc_name` and hand back a `Config` pointing at
/// `staging_root`, exactly as the drive thread sees them after a scan.
fn seed_scanned_disc(
    device: &str,
    disc_name: &str,
    staging_root: &std::path::Path,
) -> std::sync::Arc<std::sync::RwLock<crate::server::config::Config>> {
    super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            device.to_string(),
            super::RipState {
                device: device.to_string(),
                disc_name: disc_name.to_string(),
                ..Default::default()
            },
        );
    std::sync::Arc::new(std::sync::RwLock::new(crate::server::config::Config {
        staging_dir: staging_root.to_string_lossy().into_owned(),
        ..Default::default()
    }))
}

/// As [`seed_scanned_disc`], but also records the disc's RAW volume label —
/// what the drive thread puts in `RipState::disc_label` at identify time,
/// and the only thing that tells two discs of a boxset apart.
fn seed_scanned_disc_labelled(
    device: &str,
    disc_name: &str,
    disc_label: &str,
    staging_root: &std::path::Path,
) -> std::sync::Arc<std::sync::RwLock<crate::server::config::Config>> {
    let cfg = seed_scanned_disc(device, disc_name, staging_root);
    super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(device.to_string())
        .and_modify(|s| s.disc_label = disc_label.to_string());
    cfg
}

// THE boxset bug: disc 2 shares disc 1's clean_title, so a title-only staging dir would
// wrongly read disc 2 as "already ripped".
#[test]
fn disc_two_of_a_boxset_is_not_skipped_as_already_ripped() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    // One TMDB title, because clean_title already stripped "Disc N".
    let title = "Boxset Movie";
    let d1 = "sg_boxset_disc1_test";
    let d2 = "sg_boxset_disc2_test";

    // ── Disc 1 rips to completion ────────────────────────────────────
    let cfg1 = seed_scanned_disc_labelled(d1, title, "BOXSET_DISC_1", root);
    let base1 = {
        let c = cfg1.read().unwrap();
        super::staging_basename_for_device(&c, d1).expect("disc 1 has a staging basename")
    };
    assert_eq!(
        base1,
        crate::server::util::sanitize_path_compact(title),
        "the first disc keeps the plain TMDB-title dir — display and output \
             naming must not change"
    );
    staging_disc_with_markers(root, &base1, &[".completed"]);
    staging::write_disc_label(&root.join(&base1), "BOXSET_DISC_1");

    // Same disc back in the drive (container restart, disc still loaded):
    // it must find ITS OWN dir and still be recognised as finished.
    assert!(
        already_completed(&cfg1, d1),
        "re-inserting the SAME disc must still resolve to its own completed \
             dir — otherwise every restart re-sweeps a finished rip"
    );

    // ── Disc 2 goes in ───────────────────────────────────────────────
    let cfg2 = seed_scanned_disc_labelled(d2, title, "BOXSET_DISC_2", root);
    assert!(
        !already_completed(&cfg2, d2),
        "disc 2 of a boxset shares disc 1's TMDB title but is a DIFFERENT \
             disc: it must be ripped, not skipped as already completed"
    );
    let base2 = {
        let c = cfg2.read().unwrap();
        super::staging_basename_for_device(&c, d2).expect("disc 2 has a staging basename")
    };
    assert_ne!(
        base2, base1,
        "disc 2 must not be handed disc 1's staging dir — the raw volume \
             label is what distinguishes them"
    );

    // And it isn't offered disc 1's staging to resume onto either.
    assert!(
        !super::disc_owned_by_worker(&cfg2, d2),
        "disc 2 must not inherit disc 1's worker-ownership verdict"
    );

    forget_device(d1);
    forget_device(d2);
}

// Upgrade path: a pre-`.disc-label` staging dir must keep reading as "this disc" until
// adopted, not re-rip or orphan.
#[test]
fn a_legacy_unlabelled_staging_dir_still_counts_as_the_inserted_disc() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    let device = "sg_boxset_legacy_test";
    let title = "Legacy Movie";
    let sanitized = crate::server::util::sanitize_path_compact(title);

    // Pre-upgrade staging: `.completed`, no `.disc-label`.
    staging_disc_with_markers(root, &sanitized, &[".completed"]);

    let cfg = seed_scanned_disc_labelled(device, title, "LEGACY_DISC_1", root);
    assert!(
        already_completed(&cfg, device),
        "an unlabelled legacy dir must still stop the unattended path \
             re-ripping a disc it already finished"
    );

    // Once adopted, the sibling disc gets its own dir.
    staging::adopt_disc_label(&root.join(&sanitized), "LEGACY_DISC_1");
    let other = "sg_boxset_legacy_sibling_test";
    let cfg2 = seed_scanned_disc_labelled(other, title, "LEGACY_DISC_2", root);
    assert!(
        !already_completed(&cfg2, other),
        "after adoption, a different disc of the same title must rip"
    );

    forget_device(device);
    forget_device(other);
}

fn forget_device(device: &str) {
    super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(device);
}

// A Stop landing in the gap between `handle_rip_request`'s is_cancelled() check and
// rip_disc's own halt registration must be honoured, not discarded.
#[test]
fn rip_entry_halt_carries_a_stop_that_landed_in_the_dispatch_gap() {
    // Unique to this test: HALTS is a process-global registry.
    let device = "sg_rip_entry_halt_carries_dispatch_gap_stop_test";
    super::unregister_halt(device);

    // The spawn site's token, as `spawn_rip_thread` leaves it.
    let spawn_token = libfreemkv::Halt::new();
    super::register_halt(device, spawn_token.clone());

    // `handle_rip_request` has already checked is_cancelled() (false) and
    // is on its way into rip_disc. The operator hits Stop right now:
    // /api/stop resolves the device's registered token and cancels it.
    super::device_halt(device)
        .expect("the spawn-site token must be registered")
        .cancel();

    // rip_disc's entry registration runs a moment later.
    super::install_rip_halt(device);

    assert!(
        super::device_halt(device)
            .expect("a token must still be registered")
            .is_cancelled(),
        "rip_disc's entry registration discarded a Stop that landed after \
             handle_rip_request's check — the rip proceeds and the operator's Stop \
             is a silent no-op"
    );
    super::unregister_halt(device);

    // The ordinary case is unchanged: no pending Stop, fresh live token.
    super::register_halt(device, libfreemkv::Halt::new());
    super::install_rip_halt(device);
    assert!(
        !super::device_halt(device).unwrap().is_cancelled(),
        "a rip with no pending Stop must start with a live token"
    );
    super::unregister_halt(device);
}

/// Put a device into the state `rip_disc` holds while it works.
fn seed_ripping(device: &str) {
    forget_device(device);
    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "ripping".to_string(),
            disc_present: true,
            ..Default::default()
        },
    );
    assert!(
        super::is_busy(device),
        "test setup: the device must be busy before the abort"
    );
}

// The fsync durability gate bails without writing `.done`; before the fix it left `status`
// stuck "ripping" so `is_busy()` never released the drive.
#[test]
fn post_mux_durability_abort_releases_the_drive() {
    // Unique to this test: STATE is process-global and a shared fixture
    // name would race the other tests in this binary.
    let device = "sg_post_mux_durability_abort_releases_drive_test";
    seed_ripping(device);

    super::abort_post_mux_preserving_staging(
        device,
        "Durability gate failed: could not fsync mux output to stable storage; \
             withholding .done/.completed and preserving staging for retry",
        "mux output not durable (fsync failed); rip preserved for retry",
    );

    assert!(
        !super::is_busy(device),
        "the durability-gate early return left status=\"ripping\": is_busy() stays true \
             forever, so the poll loop skips this drive for the container's lifetime while \
             /api/state shows a rip in progress"
    );
    let st = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned()
        .expect("state entry");
    assert!(
        st.last_error.contains("not durable"),
        "the reason must still reach the UI, got {:?}",
        st.last_error
    );
    forget_device(device);
}

/// Same contract for the other post-mux early return: the `.done` /
/// `.review` hand-off marker write failed, so the MKV is staged but the
/// mover has no signal. Resumable, but this rip attempt is over.
#[test]
fn post_mux_marker_write_abort_releases_the_drive() {
    let device = "sg_post_mux_marker_abort_releases_drive_test";
    seed_ripping(device);

    super::abort_post_mux_preserving_staging(
        device,
        ".done marker write failed (disk full); MKV is staged but the mover cannot pick it up",
        "MKV staged but .done marker write failed: disk full",
    );

    assert!(
        !super::is_busy(device),
        "the hand-off-marker early return left status=\"ripping\": the drive is busy \
             forever and no further rip or scan can be dispatched to it"
    );
    let st = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned()
        .expect("state entry");
    assert!(
        st.last_error.contains("marker write failed"),
        "the reason must still reach the UI, got {:?}",
        st.last_error
    );
    forget_device(device);
}

#[test]
fn disc_staging_hold_sees_the_scanned_discs_loss_abort() {
    let device = "sg_loss_aborted_wrapper_test";
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = seed_scanned_disc(device, "Damaged Disc", tmp.path());
    let sanitized = crate::server::util::sanitize_path_compact("Damaged Disc");

    // No staging dir at all → nothing to protect.
    assert!(
        !loss_aborted(&cfg, device),
        "a disc with no staging dir has not aborted on loss"
    );

    // A swept ISO parked on the loss threshold, waiting for the operator to
    // Accept or run another pass. Re-sweeping would clobber it.
    staging_disc_with_markers(tmp.path(), &sanitized, &[staging::ABORTED_LOSS_MARKER]);
    assert!(
        loss_aborted(&cfg, device),
        "an .aborted-loss staging dir for the scanned disc must be recognised"
    );

    // Nothing scanned yet (empty disc_name) → never claim a match; the
    // sanitized empty name would otherwise point at the staging ROOT.
    let unscanned = "sg_loss_aborted_unscanned_test";
    let cfg2 = seed_scanned_disc(unscanned, "", tmp.path());
    assert!(
        !loss_aborted(&cfg2, unscanned),
        "an unscanned device must not match anything"
    );

    forget_device(device);
    forget_device(unscanned);
}

#[test]
fn disc_staging_hold_reads_state_and_staging_together() {
    let device = "sg_already_completed_wrapper_test";
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = seed_scanned_disc(device, "Finished Disc", tmp.path());
    let sanitized = crate::server::util::sanitize_path_compact("Finished Disc");

    assert!(
        !already_completed(&cfg, device),
        "an untouched disc is not already completed"
    );

    let disc_dir = staging_disc_with_markers(tmp.path(), &sanitized, &[]);
    staging::write_completed_marker(&disc_dir);
    assert!(
        already_completed(&cfg, device),
        "a .completed staging dir must stop the unattended path re-ripping it"
    );

    // Held for review: a hand-off is written for a held rip too, but the
    // operator hasn't confirmed the title — the disc is NOT finished.
    staging::mark_handoff(&disc_dir, false, |_| {}).unwrap();
    assert!(
        !already_completed(&cfg, device),
        "a held-for-review dir is awaiting the operator, not finished"
    );

    // A different disc in the drive must not inherit this dir's verdict.
    let other = "sg_already_completed_other_test";
    let cfg2 = seed_scanned_disc(other, "Some Other Disc", tmp.path());
    assert!(
        !already_completed(&cfg2, other),
        "another disc's staging dir must not count as this disc's completion"
    );

    forget_device(device);
    forget_device(other);
}

#[test]
fn disc_owned_by_worker_protects_the_mux_workers_iso() {
    let device = "sg_owned_by_worker_wrapper_test";
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = seed_scanned_disc(device, "Owned Disc", tmp.path());
    let sanitized = crate::server::util::sanitize_path_compact("Owned Disc");

    staging_disc_with_markers(tmp.path(), &sanitized, &["Owned_Disc.iso"]);
    assert!(
        !super::disc_owned_by_worker(&cfg, device),
        "a plain staging dir is not owned by the mux worker"
    );

    // `.ripped` = handed off; the mux worker is about to read this ISO.
    std::fs::write(tmp.path().join(&sanitized).join(".ripped"), b"{}").unwrap();
    assert!(
        super::disc_owned_by_worker(&cfg, device),
        "a .ripped dir is owned by the mux worker — a fresh sweep would truncate its ISO"
    );

    let unscanned = "sg_owned_by_worker_unscanned_test";
    let cfg2 = seed_scanned_disc(unscanned, "", tmp.path());
    assert!(
        !super::disc_owned_by_worker(&cfg2, unscanned),
        "an unscanned device must not match anything"
    );

    forget_device(device);
    forget_device(unscanned);
}

// Regression: the resume-gate config reads (`disc_staging_hold`,
// `disc_owned_by_worker`) must poison-RECOVER, not fail open — a bad-lock
// fail-open re-swept an ISO awaiting Accept / truncated the mux's live ISO.
#[test]
fn resume_gates_recover_from_a_poisoned_config_lock() {
    let device = "sg_resume_gate_poison_test";
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = seed_scanned_disc(device, "Poisoned Disc", tmp.path());
    let sanitized = crate::server::util::sanitize_path_compact("Poisoned Disc");

    // Arm both states the gates protect (one state.json: legacy markers would upgrade to one).
    let dir = staging_disc_with_markers(tmp.path(), &sanitized, &[]);
    let mut st = staging::DiscState::new(staging::StagingState::AbortedLoss);
    st.muxing = true;
    staging::write_state(&dir, &st);

    // Poison the CONFIG lock by panicking while its write guard is held.
    let cfg_poison = cfg.clone();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = cfg_poison.write().unwrap();
        panic!("intentional poison");
    }));
    assert!(cfg.is_poisoned(), "cfg lock must be poisoned for the test");

    // Both gates must still SEE the markers — a fail-open `false` here
    // would clobber the parked ISO / truncate the worker's read.
    assert_eq!(
        super::disc_staging_hold(&cfg, device, false),
        Some(super::StagingHold::OwnedByWorker),
        "disc_staging_hold must recover the poisoned lock, not re-sweep the held ISO"
    );
    assert!(
        super::disc_owned_by_worker(&cfg, device),
        "disc_owned_by_worker must recover the poisoned lock, not truncate the mux worker's ISO"
    );

    forget_device(device);
}

// Seed `device` as a scanned disc in "scanning" (as the insert claim leaves it), stage
// its dir via `arm` next to a sentinel ISO, then run the post-scan dispatch for `mode`.
fn dispatch_over_staged_disc(
    device: &str,
    name: &str,
    mode: crate::server::web::ResumeMode,
    arm: impl FnOnce(&std::path::Path),
) -> (tempfile::TempDir, std::path::PathBuf, super::RipState) {
    let tmp = tempfile::TempDir::new().unwrap();
    let cfg = seed_scanned_disc(device, name, tmp.path());
    super::update_state_with(device, |s| s.status = "scanning".to_string());
    let dir = tmp
        .path()
        .join(crate::server::util::sanitize_path_compact(name));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Sentinel.iso"), b"precious").unwrap();
    arm(&dir);
    // A bogus drive path: reaching rip_disc surfaces as "error", never a real read.
    super::dispatch_rip_request(&cfg, device, "/nonexistent/autorip-test-drive", mode);
    let st = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned()
        .unwrap_or_default();
    forget_device(device);
    (tmp, dir, st)
}

type Arm = fn(&std::path::Path);

// Every staging state a re-sweep would destroy: a finished rip (awaiting or past the
// mover), a held review, a loss-aborted ISO awaiting the operator, a mux-worker-owned dir.
fn protected_staging_arms() -> Vec<(&'static str, Arm)> {
    vec![
        ("completed", |d| staging::write_completed_marker(d)),
        ("done", |d| staging::mark_handoff(d, true, |_| {}).unwrap()),
        // Old-format crash window: `.done` written, `.completed` not yet.
        ("legacy-done", |d| {
            std::fs::write(d.join(".done"), b"{}").unwrap()
        }),
        ("review", |d| {
            staging::mark_handoff(d, false, |_| {}).unwrap()
        }),
        ("aborted-loss", |d| {
            staging::mark_aborted_on_loss(d, "loss over threshold");
        }),
        ("ripped", |d| {
            staging::write_state(d, &staging::DiscState::new(staging::StagingState::Ripped))
        }),
        ("muxing", |d| {
            let mut st = staging::DiscState::new(staging::StagingState::Ripped);
            st.muxing = true;
            staging::write_state(d, &st);
        }),
    ]
}

// 1.7.7: on_insert=rip rips fresh every time and on_insert=resume rips fresh unless the
// disc was started, so finished / held staging is replaced; only the mux worker's is spared.
#[test]
fn unattended_insert_replaces_finished_staging_but_spares_the_mux_worker() {
    for on_insert in ["rip", "resume"] {
        let mode = super::auto_insert_rip_mode(on_insert).expect("an auto-rip mode");
        for (label, arm) in protected_staging_arms() {
            // Resume runs another (non-destructive) pass over a loss-aborted disc.
            if on_insert == "resume" && label == "aborted-loss" {
                continue;
            }
            let device = format!("sg_insert_guard_{on_insert}_{label}_test");
            let (_tmp, dir, st) = dispatch_over_staged_disc(&device, "Guarded Disc", mode, arm);
            let kept = std::fs::read(dir.join("Sentinel.iso")).ok();
            if matches!(label, "ripped" | "muxing") {
                assert_eq!(
                    kept.as_deref(),
                    Some(&b"precious"[..]),
                    "on_insert={on_insert} destroyed a {label} staging dir"
                );
                assert_eq!(st.status, "idle", "{on_insert} over {label}: {st:?}");
            } else {
                assert!(kept.is_none(), "on_insert={on_insert} kept a {label} dir");
            }
        }
    }
}

// Stage a real partial sweep (ISO + pending mapfile) the resume paths accept.
fn stage_partial_sweep(d: &std::path::Path) {
    freemkv_engine::Mapfile::create(&d.join("Sentinel.iso.mapfile"), 4096, "test").unwrap();
}

// on_insert=resume is "continue a resumable rip": a loss-aborted disc gets another
// recovery pass over its kept ISO (reaching rip_disc surfaces as "error" here).
#[test]
fn resume_on_insert_runs_another_pass_over_a_loss_aborted_disc() {
    let mode = super::auto_insert_rip_mode("resume").expect("an auto-rip mode");
    let (_tmp, dir, st) =
        dispatch_over_staged_disc("sg_insert_resume_loss_test", "Lossy Disc", mode, |d| {
            stage_partial_sweep(d);
            staging::mark_aborted_on_loss(d, "loss over threshold");
        });
    assert_eq!(
        st.status, "error",
        "the resume pass must have been attempted: {st:?}"
    );
    assert_eq!(
        std::fs::read(dir.join("Sentinel.iso")).ok().as_deref(),
        Some(&b"precious"[..]),
        "a resume pass must keep the loss-aborted ISO"
    );
}

// Unreadable staging (NFS cold cache, EACCES) is unknown, not empty: stand down rather
// than wipe or sweep over what may be a finished rip.
#[cfg(unix)]
#[test]
fn insert_dispatch_stands_down_on_unreadable_staging() {
    use std::os::unix::fs::PermissionsExt;
    let modes = [
        ("rip", super::auto_insert_rip_mode("rip").expect("mode")),
        (
            "resume",
            super::auto_insert_rip_mode("resume").expect("mode"),
        ),
        ("default", crate::server::web::ResumeMode::Default),
    ];
    for (label, mode) in modes {
        let device = format!("sg_insert_unreadable_{label}_test");
        let (_tmp, dir, st) = dispatch_over_staged_disc(&device, "Cold Disc", mode, |d| {
            staging::mark_handoff(d, true, |_| {}).unwrap();
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o000)).unwrap();
        });
        let unreadable = std::fs::read_dir(&dir).is_err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !unreadable {
            // Root ignores chmod 000; never let CI pass this test vacuously.
            assert!(
                std::env::var_os("CI").is_none(),
                "chmod-000 staging test cannot run as root on CI"
            );
            eprintln!("SKIPPED insert_dispatch_stands_down_on_unreadable_staging: running as root");
            return;
        }
        assert!(
            dir.join("Sentinel.iso").exists(),
            "{label}: unreadable dir touched"
        );
        assert_eq!(
            st.status, "idle",
            "{label} must stand down on unknown staging: {st:?}"
        );
    }
}

// Two drives holding the same disc share one staging dir: a fresh insert on drive B must
// not wipe drive A's live sweep, but a crashed sweep's leftover is stale and starts fresh.
#[test]
fn fresh_insert_spares_a_live_sweep_but_wipes_a_dead_one() {
    let mode = super::auto_insert_rip_mode("rip").expect("an auto-rip mode");
    let tmp = tempfile::TempDir::new().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let other = "sg_insert_live_sweep_other_test";
    seed_scanned_disc(other, "Twin Disc", tmp.path());
    let handle = std::thread::spawn(move || {
        let _ = rx.recv();
    });
    super::register_rip_thread(other, handle).expect("register the other drive's rip");
    let device = "sg_insert_live_sweep_test";
    let cfg = seed_scanned_disc(device, "Twin Disc", tmp.path());
    super::update_state_with(device, |s| s.status = "scanning".to_string());
    let dir = tmp
        .path()
        .join(crate::server::util::sanitize_path_compact("Twin Disc"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Sentinel.iso"), b"precious").unwrap();
    staging::write_sweeping_marker(&dir);
    super::dispatch_rip_request(&cfg, device, "/nonexistent/autorip-test-drive", mode);
    let status = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .map(|s| s.status.clone());
    drop(tx);
    forget_device(other);
    forget_device(device);
    assert!(
        dir.join("Sentinel.iso").exists(),
        "another drive's live sweep was wiped"
    );
    assert_eq!(
        status.as_deref(),
        Some("idle"),
        "must stand down on a live sweep"
    );

    // No drive is sweeping it: a crash leftover, discarded by the fresh rip.
    let (_tmp3, dead, st) =
        dispatch_over_staged_disc("sg_insert_dead_sweep_test", "Twin Disc", mode, |d| {
            staging::write_sweeping_marker(d)
        });
    assert!(!dead.exists(), "a dead sweep's leftover must start fresh");
    assert_eq!(st.status, "error", "and the fresh rip must be attempted");
}

// Run `mode` over an existing but EMPTY staging dir; returns (dir, status).
fn dispatch_over_empty_dir(
    device: &str,
    root: &std::path::Path,
    mode: crate::server::web::ResumeMode,
) -> (std::path::PathBuf, Option<String>) {
    let cfg = seed_scanned_disc(device, "Empty Disc", root);
    super::update_state_with(device, |s| s.status = "scanning".to_string());
    let dir = root.join(crate::server::util::sanitize_path_compact("Empty Disc"));
    std::fs::create_dir_all(&dir).unwrap();
    super::dispatch_rip_request(&cfg, device, "/nonexistent/autorip-test-drive", mode);
    let status = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .map(|s| s.status.clone());
    forget_device(device);
    (dir, status)
}

// A genuinely empty dir (e.g. a rip that bailed right after create_dir_all) must not
// wedge unattended inserts: the wipe path proves emptiness with a non-recursive rmdir.
#[test]
fn fresh_insert_starts_over_a_genuinely_empty_staging_dir() {
    for (label, mode) in [
        ("rip", super::auto_insert_rip_mode("rip").expect("mode")),
        (
            "resume",
            super::auto_insert_rip_mode("resume").expect("mode"),
        ),
        ("default", crate::server::web::ResumeMode::Default),
    ] {
        let tmp = tempfile::TempDir::new().unwrap();
        let device = format!("sg_insert_empty_dir_{label}_test");
        let (_dir, status) = dispatch_over_empty_dir(&device, tmp.path(), mode);
        assert_eq!(status.as_deref(), Some("error"), "{label} must rip fresh");
    }
}

// An empty listing the rmdir can't confirm (the cold-cache NFS signature) stands down.
#[cfg(unix)]
#[test]
fn fresh_insert_stands_down_when_an_empty_listing_cannot_be_confirmed() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("Empty_Disc")).unwrap();
    // Read-only root: the listing works but rmdir fails, as it would on a hidden entry.
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::create_dir(root.join("probe")).is_ok() {
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            std::env::var_os("CI").is_none(),
            "read-only staging test cannot run as root on CI"
        );
        eprintln!("SKIPPED empty-listing rmdir test: running as root");
        return;
    }
    let mode = super::auto_insert_rip_mode("rip").expect("mode");
    let (dir, status) = dispatch_over_empty_dir("sg_insert_empty_unconfirmed_test", root, mode);
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        dir.exists(),
        "an unconfirmed empty listing must not be wiped"
    );
    assert_eq!(status.as_deref(), Some("idle"), "and must stand down");
}

// An unusable `state.json` hides the lifecycle (the mux worker holds such a dir): stand down.
#[test]
fn insert_dispatch_stands_down_on_an_unusable_state_file() {
    let mode = super::auto_insert_rip_mode("rip").expect("an auto-rip mode");
    let (_tmp, dir, st) =
        dispatch_over_staged_disc("sg_insert_corrupt_state_test", "Torn Disc", mode, |d| {
            std::fs::write(d.join(staging::STATE_FILE), b"{not json").unwrap()
        });
    assert!(
        dir.join("Sentinel.iso").exists(),
        "a dir with unusable state.json was wiped"
    );
    assert_eq!(st.status, "idle", "must stand down: {st:?}");
}

// A7: an operator Resume must not sweep over another drive's live sweep or unreadable
// staging; it reports why instead.
#[test]
fn operator_resume_refuses_a_live_sweep_and_unreadable_staging() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let other = "sg_require_live_sweep_other_test";
    seed_scanned_disc(other, "Busy Disc", tmp.path());
    let handle = std::thread::spawn(move || {
        let _ = rx.recv();
    });
    super::register_rip_thread(other, handle).expect("register the other drive's rip");
    let device = "sg_require_live_sweep_test";
    let cfg = seed_scanned_disc(device, "Busy Disc", tmp.path());
    let dir = tmp
        .path()
        .join(crate::server::util::sanitize_path_compact("Busy Disc"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Sentinel.iso"), b"precious").unwrap();
    stage_partial_sweep(&dir);
    staging::write_sweeping_marker(&dir);
    let mode = crate::server::web::ResumeMode::Require;
    super::dispatch_rip_request(&cfg, device, "/nonexistent/autorip-test-drive", mode);
    let st = super::STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(device)
        .cloned()
        .unwrap_or_default();
    drop(tx);
    forget_device(other);
    forget_device(device);
    assert_eq!(st.status, "error", "{st:?}");
    assert!(
        st.last_error.contains("Another drive"),
        "Resume must explain the live sweep, not attempt it: {:?}",
        st.last_error
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = crate::server::web::ResumeMode::Require;
        let (_tmp, dir, st) =
            dispatch_over_staged_disc("sg_require_unreadable_test", "Cold Disc", mode, |d| {
                stage_partial_sweep(d);
                std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o000)).unwrap();
            });
        let unreadable = std::fs::read_dir(&dir).is_err();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            unreadable || std::env::var_os("CI").is_none(),
            "chmod-000 staging test cannot run as root on CI"
        );
        if unreadable {
            assert!(
                st.last_error.contains("Cannot read"),
                "Resume must explain unreadable staging: {:?}",
                st.last_error
            );
        }
    }
}

// A6: an explicit Resume must not sweep over a finished rip the mover may be copying.
#[test]
fn resumable_for_disc_refuses_finished_rips() {
    let display_name = "Finished Resume Disc";
    let sanitized = crate::server::util::sanitize_path_compact(display_name);
    let arms: Vec<(&str, Arm)> = vec![
        ("completed", |d| staging::write_completed_marker(d)),
        ("done", |d| staging::mark_handoff(d, true, |_| {}).unwrap()),
        ("legacy-done", |d| {
            std::fs::write(d.join(".done"), b"{}").unwrap()
        }),
    ];
    for (label, arm) in arms {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = crate::server::config::Config {
            staging_dir: tmp.path().to_string_lossy().into_owned(),
            ..Default::default()
        };
        let disc_dir = tmp.path().join(&sanitized);
        std::fs::create_dir(&disc_dir).unwrap();
        std::fs::write(disc_dir.join(format!("{sanitized}.iso")), b"x").unwrap();
        let mapfile = disc_dir.join(format!("{sanitized}.iso.mapfile"));
        freemkv_engine::Mapfile::create(&mapfile, 4096, "test").unwrap();
        arm(&disc_dir);
        assert_eq!(
            resumable_for_disc(&cfg, display_name, ""),
            None,
            "a {label} dir must not be offered for a resume sweep"
        );
    }
}

// The Default arm (/api/rip with no resume=) shares the same guard path.
#[test]
fn default_rip_dispatch_never_resweeps_protected_staging() {
    for (label, arm) in protected_staging_arms() {
        let device = format!("sg_default_guard_{label}_test");
        let mode = crate::server::web::ResumeMode::Default;
        let (_tmp, dir, st) = dispatch_over_staged_disc(&device, "Guarded Disc", mode, arm);
        assert!(dir.join("Sentinel.iso").exists(), "{label} dir removed");
        assert_eq!(
            st.status, "idle",
            "Default over a {label} dir must not rip: {st:?}"
        );
    }
}

// "Fresh" still means fresh: a stale partial sweep and a terminal `.failed` attempt are
// discarded before the new rip (which here fails on the bogus drive).
#[test]
fn unattended_fresh_rip_discards_stale_partial_and_failed_staging() {
    let arms: Vec<(&str, Arm)> = vec![
        ("partial", |d| {
            std::fs::write(d.join("Sentinel.iso.mapfile"), b"").unwrap()
        }),
        ("failed", |d| {
            staging::write_failed_marker(d, "mux failed");
        }),
    ];
    for (label, arm) in arms {
        let mode = super::auto_insert_rip_mode("rip").expect("an auto-rip mode");
        let device = format!("sg_insert_fresh_{label}_test");
        let (_tmp, dir, st) = dispatch_over_staged_disc(&device, "Stale Disc", mode, arm);
        assert!(!dir.exists(), "on_insert=rip must wipe a stale {label} dir");
        assert_eq!(st.status, "error", "the fresh rip must have been attempted");
    }
    // on_insert=resume has nothing to resume in a terminal `.failed` dir: fresh too.
    let mode = super::auto_insert_rip_mode("resume").expect("an auto-rip mode");
    let (_tmp, dir, _) =
        dispatch_over_staged_disc("sg_insert_resume_failed_test", "Stale Disc", mode, |d| {
            staging::write_failed_marker(d, "mux failed");
        });
    assert!(
        !dir.exists(),
        "on_insert=resume must start a failed disc fresh"
    );
}

// The operator's explicit "Rip"/"Start over" (?resume=no) keeps its clean-slate wipe.
#[test]
fn operator_wipe_dispatch_still_clears_finished_staging() {
    let mode = crate::server::web::ResumeMode::Wipe;
    let (_tmp, dir, st) =
        dispatch_over_staged_disc("sg_operator_wipe_completed_test", "Redo Disc", mode, |d| {
            staging::write_completed_marker(d)
        });
    assert!(
        !dir.exists(),
        "an explicit operator Wipe clears even a finished dir"
    );
    assert_eq!(st.status, "error", "and then attempts the rip");
}

// A drive that is mid-rip must NOT have its STATE entry deleted just because one
// enumeration pass missed it (double-rip guard).
#[test]
fn hot_unplug_teardown_keeps_the_double_rip_guard_for_a_busy_drive() {
    // Unique to this test: STATE is a process-global static and a shared
    // fixture name would race the other tests in this binary.
    let device = "sg_hotplug_busy_double_rip_guard_test";
    forget_device(device);

    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "ripping".to_string(),
            ..Default::default()
        },
    );
    assert!(
        super::is_busy(device),
        "test setup: the device must be busy before the reconcile"
    );

    // The device vanished from the fresh enumeration while ripping.
    let torn_down = super::forget_removed_device(device);

    assert!(
        super::is_busy(device),
        "hot-unplug reconcile deleted the STATE entry of a ripping drive — \
             is_busy() now returns false and a second rip can launch on it"
    );
    assert!(
        !torn_down,
        "teardown of a busy device must be deferred, not executed"
    );

    // An idle drive that really went away is still torn down.
    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "idle".to_string(),
            ..Default::default()
        },
    );
    assert!(
        super::forget_removed_device(device),
        "an idle removed device must still be torn down"
    );
    assert!(
        !super::device_known(device),
        "an idle removed device's STATE entry must be evicted"
    );

    forget_device(device);
}

// The fresh-rip completion tail must log/notify BEFORE it ejects (eject_drive archives the
// log) and route the eject through should_auto_eject.
#[test]
fn the_completion_tail_logs_and_notifies_before_ejecting() {
    let src = crate::server::util::source_lf(include_str!("mod.rs"));
    // Scan just the fresh-rip completion tail (unique anchors), so the
    // ordering checked below is this tail's, not some other eject site's.
    let start = src
        .find("largest_gap_ms: sweep_damage_snapshot.largest_gap_ms,")
        .expect("the fresh-rip completion tail must write its done state");
    let end = src
        .find("// Pure decision: should this completion path auto-eject")
        .expect("should_auto_eject must still be documented below the tail");
    let tail = &src[start..end];
    let log_line = tail
        .find(r#"crate::server::log::device_log(device, "Mux complete");"#)
        .expect("the inline-mux completion tail must log \"Mux complete\"");
    let webhook = tail
        .find("crate::server::webhook::send_rich(")
        .expect("the completion tail must fire the mux_complete webhook");
    let eject = tail
        .find("eject_drive(device_path);")
        .expect("the completion tail must still auto-eject");
    assert!(
        log_line < eject && webhook < eject,
        "\"Mux complete\" and the completion webhook must be emitted \
             BEFORE eject_drive — it archives the device log, so anything \
             after it is lost from this rip's archived log"
    );
    assert!(
        tail.contains("should_auto_eject(cfg_read.auto_eject, device)"),
        "this completion terminal must route its eject through \
             should_auto_eject, like the other two — that predicate is where \
             the \"never from the mux worker\" rule lives"
    );
}

// Teardown must be gated on the WORKER, not the status it already wrote: `is_busy` reads
// FALSE during the post-status tail while the worker is still alive.
#[test]
fn hot_unplug_teardown_defers_while_the_rip_thread_is_still_unwinding() {
    let device = "sg_hotplug_tail_liveness_test";
    forget_device(device);

    // A worker that is still running, registered exactly as the rip
    // dispatch registers one.
    let gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_gate = std::sync::Arc::clone(&gate);
    super::spawn_rip_thread(device, "rip", move || {
        // Watchdog, not the expectation: the assertions below run in
        // microseconds. The bound stops a regression parking this
        // thread for the life of the suite.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !worker_gate.load(std::sync::atomic::Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    })
    .expect("spawn must succeed");

    // The worker's own tail: terminal status already written, thread
    // still executing.
    super::update_state(
        device,
        super::RipState {
            device: device.to_string(),
            status: "done".to_string(),
            ..Default::default()
        },
    );
    assert!(
        !super::is_busy(device),
        "test setup: the unwinding tail is exactly the window is_busy \
             cannot see"
    );

    let torn_down = super::forget_removed_device(device);

    assert!(
        !torn_down,
        "teardown must be deferred while the rip thread is still \
             unwinding — its tail is still using the session, the STATE row \
             and the device log ring"
    );
    assert!(
        super::device_known(device),
        "the STATE row of a device whose worker is still running must \
             survive the hot-unplug reconcile"
    );

    // And it is a DEFERRAL, not a leak: once the worker is gone the next
    // rescan tears the device down.
    gate.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = super::join_rip_thread(device, std::time::Duration::from_secs(5));
    assert!(
        super::forget_removed_device(device),
        "once the worker has exited the deferred teardown must run"
    );
    assert!(
        !super::device_known(device),
        "the deferred teardown must evict the STATE row when it finally runs"
    );

    forget_device(device);
}
