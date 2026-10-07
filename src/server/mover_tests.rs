// The mover must not hold the config lock WHILE moving. Observes the lock from INSIDE the
// injected move — the only place this property is actually observable.
#[test]
fn the_config_lock_is_free_while_the_move_runs() {
    use std::sync::{Arc, RwLock};
    let cfg = Arc::new(RwLock::new(Config::default()));

    let mut observed_writable = false;
    let ok = super::mover_tick(&cfg, |snapshot| {
        // We are "mid-move" here. A writer must be able to proceed.
        observed_writable = cfg.try_write().is_ok();
        // And the snapshot is still usable, so releasing early costs nothing.
        let _ = &snapshot.staging_dir;
    });

    assert!(ok, "mover_tick should succeed on a healthy lock");
    assert!(
        observed_writable,
        "a writer was blocked DURING the move — the guard is held across it, \
             so any POST /api/settings would wedge the web UI for the whole copy"
    );
}
use super::*;

fn cfg_with_dirs(movie_dir: &str, tv_dir: &str, output_dir: &str) -> Config {
    Config {
        output_dir: output_dir.into(),
        movie_dir: movie_dir.into(),
        tv_dir: tv_dir.into(),
        ..Config::default()
    }
}

fn tmdb_movie(title: &str, year: u16) -> tmdb::TmdbResult {
    tmdb::TmdbResult {
        title: title.into(),
        year,
        poster_url: String::new(),
        overview: String::new(),
        media_type: "movie".into(),
        tmdb_id: 0,
    }
}

fn tmdb_tv(title: &str, year: u16) -> tmdb::TmdbResult {
    tmdb::TmdbResult {
        title: title.into(),
        year,
        poster_url: String::new(),
        overview: String::new(),
        media_type: "tv".into(),
        tmdb_id: 0,
    }
}

use crate::server::ripper::staging::Output;

fn ep_output(filename: &str, episode: Option<u16>, episode_name: &str) -> Output {
    Output {
        filename: filename.into(),
        title_index: 0,
        episode,
        episode_name: episode_name.into(),
        moved: false,
    }
}

#[test]
fn tv_episode_leaf_basic() {
    let tmdb = Some(tmdb_tv("Endeavour", 2012));
    let outputs = vec![ep_output("Endeavour_S05E01.mkv", Some(1), "")];
    assert_eq!(
        tv_episode_leaf(&tmdb, &outputs, "Endeavour_S05E01.mkv", Some(5)),
        Some("Endeavour S05E01.mkv".to_string())
    );
}

#[test]
fn tv_episode_leaf_includes_episode_name_when_present() {
    let tmdb = Some(tmdb_tv("Endeavour", 2012));
    let outputs = vec![ep_output("Endeavour_S05E01.mkv", Some(1), "Muse")];
    assert_eq!(
        tv_episode_leaf(&tmdb, &outputs, "Endeavour_S05E01.mkv", Some(5)),
        Some("Endeavour S05E01 - Muse.mkv".to_string())
    );
}

#[test]
fn tv_episode_leaf_none_for_movie_media_type() {
    let tmdb = Some(tmdb_movie("Endeavour", 2012));
    let outputs = vec![ep_output("Endeavour_S05E01.mkv", Some(1), "")];
    assert_eq!(
        tv_episode_leaf(&tmdb, &outputs, "Endeavour_S05E01.mkv", Some(5)),
        None
    );
}

#[test]
fn tv_episode_leaf_none_when_filename_not_in_outputs() {
    let tmdb = Some(tmdb_tv("Endeavour", 2012));
    let outputs = vec![ep_output("Endeavour_S05E01.mkv", Some(1), "")];
    assert_eq!(
        tv_episode_leaf(&tmdb, &outputs, "Somewhere_Else.mkv", Some(5)),
        None
    );
}

#[test]
fn tv_episode_leaf_none_when_output_has_no_episode() {
    let tmdb = Some(tmdb_tv("Endeavour", 2012));
    let outputs = vec![ep_output("Endeavour_S05E01.mkv", None, "")];
    assert_eq!(
        tv_episode_leaf(&tmdb, &outputs, "Endeavour_S05E01.mkv", Some(5)),
        None
    );
}

#[test]
fn tv_episode_leaf_defaults_season_to_one_when_none() {
    let tmdb = Some(tmdb_tv("Firefly", 2002));
    let outputs = vec![ep_output("Firefly_E03.mkv", Some(3), "")];
    assert_eq!(
        tv_episode_leaf(&tmdb, &outputs, "Firefly_E03.mkv", None),
        Some("Firefly S01E03.mkv".to_string())
    );
}

#[test]
fn tv_episode_leaf_sanitizes_title_path_separator() {
    let tmdb = Some(tmdb_tv("Rogue/One", 2016));
    let outputs = vec![ep_output("Rogue.mkv", Some(2), "")];
    let leaf =
        tv_episode_leaf(&tmdb, &outputs, "Rogue.mkv", Some(1)).expect("expected a TV episode leaf");
    assert!(!leaf.contains('/'), "leaf must not contain '/': {leaf}");
    assert_eq!(leaf, "RogueOne S01E02.mkv");
}

// TMDB episode names are untrusted: whatever they contain, the leaf must stay ONE plain
// path component, so the file can't escape (or break) the Season folder.
#[test]
fn tv_episode_leaf_neutralises_hostile_episode_names() {
    let tmdb = Some(tmdb_tv("Show", 2020));
    let cfg = cfg_with_dirs("", "/lib/tv", "/lib");
    let season_dir = "/lib/tv/Show (2020)/Season 01";
    for hostile in [
        "..",
        ".",
        "../../../etc/passwd",
        "..\\..\\Windows\\System32",
        "Part 1/2",
        "C:\\evil",
        "Title: Subtitle",
        "/abs/path",
        "\\\\server\\share",
        "a\0b",
        "Who? *What* <Where> |When| \"Why\"",
        "   ",
        "日本語",
    ] {
        let outputs = vec![ep_output("ep.mkv", Some(2), hostile)];
        let leaf = tv_episode_leaf(&tmdb, &outputs, "ep.mkv", Some(1))
            .expect("a TV episode must still get a leaf");
        for bad in ['/', '\\', ':', '\0', '*', '?', '"', '<', '>', '|'] {
            assert!(
                !leaf.contains(bad),
                "leaf {leaf:?} keeps {bad:?} from {hostile:?}"
            );
        }
        let mut comps = Path::new(&leaf).components();
        assert!(
            matches!(comps.next(), Some(std::path::Component::Normal(_))) && comps.next().is_none(),
            "leaf {leaf:?} from {hostile:?} must be a single normal component"
        );
        assert!(
            leaf.starts_with("Show S01E02 - ") && leaf.ends_with(".mkv"),
            "leaf {leaf:?} must keep the episode prefix and extension"
        );
        let dest = build_destination(&cfg, &tmdb, &leaf, Some(1));
        assert_eq!(
            dest,
            format!("{season_dir}/{leaf}"),
            "a hostile episode name must land directly in the Season folder"
        );
    }
}

/// `MOVE_ERRORS` is process-global. Tests that assert on its contents (or
/// that clear it wholesale) serialize on this so a parallel test thread
/// can't wipe or observe another's entries mid-assertion.
fn errors_guard() -> std::sync::MutexGuard<'static, ()> {
    // The one shared lock (see `TEST_STATE_LOCK`) so web.rs's queue-view
    // test, which also mutates ACTIVE_MOVE_DIR, serializes against these.
    super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[test]
fn artifact_label_from_extension() {
    assert_eq!(artifact_label(Path::new("/s/Title (2024).iso")), "iso");
    assert_eq!(artifact_label(Path::new("/s/Title (2024).mkv")), "mkv");
    assert_eq!(artifact_label(Path::new("/s/Title (2024).m2ts")), "m2ts");
    // Case-folded so the label is stable regardless of on-disk casing.
    assert_eq!(artifact_label(Path::new("/s/Title.MKV")), "mkv");
    // No extension → empty (the UI then omits the "(…)" suffix).
    assert_eq!(artifact_label(Path::new("/s/Title")), "");
}

/// Read one `MOVE_ERRORS` entry and release the lock immediately, so a
/// failing assertion can't panic while holding it (which would poison the
/// map for every other test in the binary).
fn error_snapshot(path: &str) -> Option<MoverError> {
    MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .cloned()
}

#[test]
fn sanitize_dir_name_strips_unsafe_characters() {
    assert_eq!(
        crate::server::util::sanitize_path_display("Aurora: Drift Two"),
        "Aurora Drift Two"
    );
    assert_eq!(
        crate::server::util::sanitize_path_display("M*A*S*H"),
        "MASH"
    );
    assert_eq!(
        crate::server::util::sanitize_path_display("Alien/Predator"),
        "AlienPredator"
    );
    assert_eq!(
        crate::server::util::sanitize_path_display("What's Up, Doc?"),
        "What's Up Doc"
    );
}

#[test]
fn sanitize_dir_name_keeps_allowed_punctuation() {
    assert_eq!(
        crate::server::util::sanitize_path_display("Side Quest - A Long Journey"),
        "Side Quest - A Long Journey"
    );
    assert_eq!(
        crate::server::util::sanitize_path_display("Director_Cut.2019"),
        "Director_Cut.2019"
    );
}

#[test]
fn sanitize_dir_name_trims_whitespace() {
    assert_eq!(
        crate::server::util::sanitize_path_display("  spaced title  "),
        "spaced title"
    );
}

#[test]
fn build_destination_movie_with_year() {
    let cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    let tmdb = Some(tmdb_movie("Aurora Drift Two", 2024));
    let dest = build_destination(&cfg, &tmdb, "disc.mkv", None);
    assert_eq!(
        dest,
        "/out/Movies/Aurora Drift Two (2024)/Aurora Drift Two (2024).mkv"
    );
}

// Drive 4K UHD mis-file (2026-07-22): a disc with NO TMDB match writes media_type: "" into
// its .done marker, which must route as a movie — not fall through to output_dir ROOT,
// which dumped a 53 GB MKV at the bare library root.
#[test]
fn build_destination_empty_media_type_files_as_movie() {
    let cfg = cfg_with_dirs("movies", "tv", "/mnt/media/");
    // A no-TMDB-match rip: title from the disc label, year 0, media_type "".
    let tmdb = Some(tmdb::TmdbResult {
        title: "Drive (2011) - 4K Ultra HD".into(),
        year: 0,
        poster_url: String::new(),
        overview: String::new(),
        media_type: String::new(),
        tmdb_id: 0,
    });
    let dest = build_destination(&cfg, &tmdb, "Drive (2011) - 4K Ultra HD.mkv", None);
    // Files under the movie library in a per-title folder. (sanitize_path_display
    // strips the parens from the disc-label title — same reason the mis-filed
    // name lacked them — so the leaf is "Drive 2011 - 4K Ultra HD".)
    assert_eq!(
        dest, "/mnt/media/movies/Drive 2011 - 4K Ultra HD/Drive 2011 - 4K Ultra HD.mkv",
        "an empty media_type must route as a movie, not dump at the output root"
    );
    // ...and specifically NOT the bare library root (the actual bug).
    assert!(
        !dest.starts_with("/mnt/media//"),
        "empty media_type must not fall through to the output_dir root dump"
    );
    // destination_root must agree (it's what validate_destination_root
    // checks) — the movie library root, not the bare share root.
    assert_eq!(
        destination_root(&cfg, &tmdb),
        "/mnt/media/movies",
        "destination_root must coalesce empty media_type to the movie root, in lock-step"
    );
}

// Mercy incident ROOT CAUSE (2026-06): a RELATIVE movie_dir must join UNDER output_dir,
// not be used standalone. Pre-fix, build_destination used cfg.movie_dir directly,
// resolving against the container root / (the ephemeral overlay) instead of the NFS mount.
#[test]
fn build_destination_relative_movie_dir_joins_under_output_dir() {
    // Reproduces the Mercy incident config (relative movie_dir, NFS output_dir).
    let cfg = cfg_with_dirs("movies", "", "/mnt/media/");
    let tmdb = Some(tmdb_movie("Mercy", 2023));
    let dest = build_destination(&cfg, &tmdb, "Mercy.mkv", None);
    assert_eq!(
        dest, "/mnt/media/movies/Mercy (2023)/Mercy (2023).mkv",
        "a relative movie_dir must resolve UNDER output_dir on the NFS mount"
    );
    // The regression we're guarding against: it must NOT be the bare
    // container-overlay path.
    assert!(
        !dest.starts_with("/movies/"),
        "movie_dir must never be used standalone (Mercy bug: wrote to /movies on the overlay)"
    );
    // And destination_root must agree — it's what validate_destination_root
    // checks, so a correct config validates the REAL mount root.
    assert_eq!(
        destination_root(&cfg, &tmdb),
        "/mnt/media/movies",
        "destination_root must be the joined mount-relative root, not bare 'movies'"
    );
}

/// A relative `tv_dir` is likewise joined under `output_dir` (same bug
/// class as the movie branch).
#[test]
fn build_destination_relative_tv_dir_joins_under_output_dir() {
    let cfg = cfg_with_dirs("", "tv", "/mnt/media/");
    let tmdb = Some(tmdb::TmdbResult {
        title: "Severance".into(),
        year: 2022,
        poster_url: String::new(),
        overview: String::new(),
        media_type: "tv".into(),
        tmdb_id: 0,
    });
    let dest = build_destination(&cfg, &tmdb, "sev_s01e01.mkv", None);
    // No season parsed → default Season 01; series folder carries the year.
    assert_eq!(
        dest,
        "/mnt/media/tv/Severance (2022)/Season 01/sev_s01e01.mkv"
    );
    // A parsed season lands in the matching zero-padded subfolder.
    let dest5 = build_destination(&cfg, &tmdb, "sev_s05e02.mkv", Some(5));
    assert_eq!(
        dest5,
        "/mnt/media/tv/Severance (2022)/Season 05/sev_s05e02.mkv"
    );
    assert!(!dest.starts_with("/tv/"));
}

/// Back-compat: an ABSOLUTE `movie_dir` still wins via Path::join
/// semantics (replaces output_dir entirely) — operators who configured
/// an absolute movie/tv dir keep their existing layout.
#[test]
fn build_destination_absolute_movie_dir_overrides_output_dir() {
    let cfg = cfg_with_dirs("/srv/library/movies", "", "/mnt/media/");
    let tmdb = Some(tmdb_movie("Mercy", 2023));
    let dest = build_destination(&cfg, &tmdb, "Mercy.mkv", None);
    assert_eq!(
        dest, "/srv/library/movies/Mercy (2023)/Mercy (2023).mkv",
        "an absolute movie_dir must override output_dir (Path::join semantics)"
    );
    assert_eq!(destination_root(&cfg, &tmdb), "/srv/library/movies");
}

// The media-root rules on NATIVE paths, so Windows exercises this logic instead of skipping
// it.
#[test]
fn resolve_media_root_joins_natively() {
    let base = if cfg!(windows) {
        r"D:\media"
    } else {
        "/mnt/media"
    };
    let elsewhere = if cfg!(windows) {
        r"E:\library\movies"
    } else {
        "/srv/movies"
    };

    // A relative sub joins UNDER the base.
    let joined = resolve_media_root(base, "movies");
    assert_eq!(
        joined,
        Path::new(base)
            .join("movies")
            .to_string_lossy()
            .replace('\\', "/")
    );
    assert!(
        Path::new(&joined).starts_with(base),
        "a relative sub must stay under output_dir: {joined}"
    );

    // An absolute sub WINS outright — it must not be joined under base.
    assert_eq!(
        resolve_media_root(base, elsewhere),
        elsewhere.replace('\\', "/"),
        "an absolute media dir must override output_dir"
    );

    // An empty sub yields output_dir verbatim.
    assert_eq!(resolve_media_root(base, ""), base.replace('\\', "/"));
}

// The POSIX-separator contract on NATIVE config paths (a Windows drive root on the Windows
// leg): every destination and root is '/'-separated, and each dest lives under its root.
#[test]
fn destinations_and_roots_emit_posix_separators_natively() {
    let base = if cfg!(windows) {
        r"D:\media"
    } else {
        "/mnt/media"
    };
    let mut cfg = cfg_with_dirs("movies", "tv", base);
    cfg.iso_dir = "isos".into();
    let cases = [
        (Some(tmdb_movie("Lumina", 2023)), "Lumina.mkv"),
        (Some(tmdb_movie("Lumina", 2023)), "Lumina.iso"),
        (Some(tmdb_tv("Severance", 2022)), "sev_s01e01.mkv"),
        (None, "disc.mkv"),
    ];
    for (tmdb, file) in cases {
        let dest = build_destination(&cfg, &tmdb, file, Some(1));
        let root = destination_root_for(&cfg, &tmdb, file);
        assert!(!dest.contains('\\'), "dest {dest:?} has a backslash");
        assert!(!root.contains('\\'), "root {root:?} has a backslash");
        assert!(
            dest.starts_with(&format!("{root}/")),
            "dest {dest} must live under its validated root {root}"
        );
    }
}

/// `absolute_for_log`'s actual invariant, on every platform: whatever goes
/// in, what comes out is absolute. The POSIX test above pins the literal
/// pass-through; this pins the property.
#[test]
fn absolute_for_log_is_always_absolute_natively() {
    let abs = if cfg!(windows) {
        r"D:\media\movies\Mercy (2024)\Mercy (2024).mkv"
    } else {
        "/mnt/media/movies/Mercy (2024)/Mercy (2024).mkv"
    };
    assert_eq!(
        absolute_for_log(abs),
        abs,
        "an absolute path passes through"
    );
    let rel = absolute_for_log("movies/Mercy/Mercy.mkv");
    assert!(
        Path::new(&rel).is_absolute(),
        "a relative path must be anchored: {rel}"
    );
}

/// The variant-suffix RULES on native paths: the suffix lands on the stem,
/// the extension survives, and the per-title directory is untouched so a
/// multi-disc set stays in one folder.
#[test]
fn dest_with_variant_suffixes_the_stem_natively() {
    let dir = if cfg!(windows) {
        r"D:\media\movies\Aurora Drift (2024)"
    } else {
        "/mnt/media/movies/Aurora Drift (2024)"
    };
    let d = Path::new(dir)
        .join("Aurora Drift (2024).mkv")
        .to_string_lossy()
        .into_owned();

    assert_eq!(
        dest_with_variant(&d, 1),
        d,
        "the first disc keeps the plain name"
    );

    let v3 = dest_with_variant(&d, 3);
    let p3 = Path::new(&v3);
    assert_eq!(
        p3.parent(),
        Path::new(&d).parent(),
        "the per-title directory must be left alone: {v3}"
    );
    assert_eq!(
        p3.extension().and_then(|e| e.to_str()),
        Some("mkv"),
        "the suffix goes on the stem, never after the extension: {v3}"
    );
    assert_eq!(
        p3.file_stem().and_then(|e| e.to_str()),
        Some("Aurora Drift (2024)_3"),
        "the suffix lands on the stem: {v3}"
    );
}

// resolve_media_root unit semantics: relative joins, absolute wins, trailing slashes
// normalize, empty sub -> output_dir. POSIX-only.
#[cfg(unix)]
#[test]
fn resolve_media_root_semantics() {
    assert_eq!(
        resolve_media_root("/mnt/media/", "movies"),
        "/mnt/media/movies"
    );
    assert_eq!(
        resolve_media_root("/mnt/media", "movies"),
        "/mnt/media/movies"
    );
    assert_eq!(
        resolve_media_root("/mnt/media", "/srv/movies"),
        "/srv/movies"
    );
    assert_eq!(resolve_media_root("/mnt/media", ""), "/mnt/media");
}

#[test]
fn build_destination_movie_without_year_falls_through() {
    let cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    let tmdb = Some(tmdb_movie("Unknown Year", 0));
    let dest = build_destination(&cfg, &tmdb, "disc.mkv", None);
    // year=0 skips the "(YEAR)" suffix; mkv name derived from cleaned title.
    assert_eq!(dest, "/out/Movies/Unknown Year/Unknown Year.mkv");
}

#[test]
fn build_destination_tv_uses_season_1_layout() {
    let cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    let tmdb = Some(tmdb::TmdbResult {
        title: "Severance".into(),
        year: 2022,
        poster_url: String::new(),
        overview: String::new(),
        media_type: "tv".into(),
        tmdb_id: 0,
    });
    let dest = build_destination(&cfg, &tmdb, "sev_s01e01.mkv", None);
    assert_eq!(dest, "/out/TV/Severance (2022)/Season 01/sev_s01e01.mkv");
}

#[test]
fn build_destination_no_tmdb_falls_to_output_dir() {
    let cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    let dest = build_destination(&cfg, &None, "disc.mkv", None);
    assert_eq!(dest, "/out/disc.mkv");
}

#[test]
fn build_destination_empty_movie_dir_falls_to_output_dir() {
    let cfg = cfg_with_dirs("", "/out/TV", "/out");
    let tmdb = Some(tmdb_movie("Movie", 2020));
    let dest = build_destination(&cfg, &tmdb, "disc.mkv", None);
    // movie_dir empty → fall-through to output_dir + filename.
    assert_eq!(dest, "/out/disc.mkv");
}

// Companion to build_destination_empty_movie_dir_falls_to_output_dir: the TV arm's
// !cfg.tv_dir.is_empty() guard is load-bearing the same way.
#[test]
fn build_destination_empty_tv_dir_falls_to_output_dir() {
    let cfg = cfg_with_dirs("/out/Movies", "", "/out");
    let tv = tmdb_tv("Severance", 2022);
    let dest = build_destination(&cfg, &Some(tv.clone()), "sev_s01e01.mkv", None);
    assert_eq!(
        dest, "/out/sev_s01e01.mkv",
        "an empty tv_dir must fall through to the output root, not \
             fabricate a Season 1 tree there"
    );
    assert!(
        !dest.contains("Season 1"),
        "no season tree may be created when tv_dir is unset: {dest}"
    );
    assert_eq!(destination_root(&cfg, &Some(tv)), "/out");
}

// The lock-step contract as a property over the whole configured-dir matrix, including
// empty-dir edges.
#[test]
fn destination_root_and_build_destination_agree_including_empty_dirs() {
    for (movie_dir, tv_dir) in [
        ("/mnt/movies", "/mnt/tv"),
        ("movies", "tv"),
        ("", "/mnt/tv"),
        ("/mnt/movies", ""),
        ("", ""),
    ] {
        for media_type in ["movie", "tv", ""] {
            let cfg = cfg_with_dirs(movie_dir, tv_dir, "/mnt/out");
            let mut r = tmdb_movie("Some Title", 2024);
            r.media_type = media_type.to_string();
            let root = destination_root(&cfg, &Some(r.clone()));
            let dest = build_destination(&cfg, &Some(r), "Disc.mkv", None);
            assert!(
                dest.starts_with(&format!("{root}/")),
                "dest {dest} must live under the validated root {root} \
                     (movie_dir={movie_dir:?} tv_dir={tv_dir:?} media={media_type:?})"
            );
            // Which dir governs this routing decision — empty means the
            // fall-through must be taken by BOTH functions.
            let routing_dir = match media_type {
                "tv" => tv_dir,
                // "" coalesces to "movie" (routing_media_type).
                _ => movie_dir,
            };
            if routing_dir.is_empty() {
                assert_eq!(
                    root, "/mnt/out",
                    "an empty routing dir must validate the output root \
                         (media={media_type:?})"
                );
                assert_eq!(
                    dest, "/mnt/out/Disc.mkv",
                    "an empty routing dir must write the bare leaf at the \
                         output root — no per-title/season tree \
                         (media={media_type:?})"
                );
            }
        }
    }
}

#[test]
fn build_destination_movie_preserves_iso_extension() {
    // Bug fix: pre-0.25.7 a keep_iso=true rip left .mkv and .iso both
    // planning to a hardcoded ".mkv" path, alternately overwritten.
    let cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    let tmdb = Some(tmdb_movie("Lumina", 2023));
    let dest_iso = build_destination(&cfg, &tmdb, "Lumina.iso", None);
    let dest_mkv = build_destination(&cfg, &tmdb, "Lumina.mkv", None);
    assert_eq!(dest_iso, "/out/Movies/Lumina (2023)/Lumina (2023).iso");
    assert_eq!(dest_mkv, "/out/Movies/Lumina (2023)/Lumina (2023).mkv");
    assert_ne!(
        dest_iso, dest_mkv,
        "iso and mkv companions must not collide"
    );
}

#[test]
fn build_destination_movie_preserves_m2ts_extension() {
    let cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    let tmdb = Some(tmdb_movie("Movie", 2024));
    let dest = build_destination(&cfg, &tmdb, "00800.m2ts", None);
    assert_eq!(dest, "/out/Movies/Movie (2024)/Movie (2024).m2ts");
}

#[test]
fn iso_dir_routes_iso_flat_to_its_own_root_relative() {
    // A RELATIVE iso_dir joins under output_dir; the ISO lands FLAT while
    // the MKV companion still files into the movie tree — must not
    // collide. Raw output: '/' is the contract on every platform.
    let mut cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    cfg.iso_dir = "isos".into();
    let tmdb = Some(tmdb_movie("Lumina", 2023));
    let dest_iso = build_destination(&cfg, &tmdb, "Lumina.iso", None);
    let dest_mkv = build_destination(&cfg, &tmdb, "Lumina.mkv", None);
    assert_eq!(dest_iso, "/out/isos/Lumina (2023).iso");
    assert_eq!(dest_mkv, "/out/Movies/Lumina (2023)/Lumina (2023).mkv");
}

#[test]
fn iso_dir_absolute_targets_another_disk() {
    // An ABSOLUTE iso_dir wins via Path::join (own disk / share).
    let mut cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    cfg.iso_dir = "/mnt/archive/isos".into();
    let tmdb = Some(tmdb_movie("Lumina", 2023));
    assert_eq!(
        build_destination(&cfg, &tmdb, "Lumina.iso", None),
        "/mnt/archive/isos/Lumina (2023).iso"
    );
}

#[test]
fn iso_dir_empty_keeps_legacy_alongside_routing() {
    // Default (empty) iso_dir must not change behaviour: the ISO stays
    // beside the muxed title, matching build_destination_movie_preserves_iso_extension.
    let cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    assert!(cfg.iso_dir.is_empty());
    let tmdb = Some(tmdb_movie("Lumina", 2023));
    assert_eq!(
        build_destination(&cfg, &tmdb, "Lumina.iso", None),
        "/out/Movies/Lumina (2023)/Lumina (2023).iso"
    );
}

#[test]
fn destination_root_for_selects_iso_root_only_for_iso_files() {
    // The move loop validates the DISTINCT set of roots; with iso_dir set,
    // a keep_iso delivery spans movie root AND iso root, both validated.
    let mut cfg = cfg_with_dirs("/out/Movies", "/out/TV", "/out");
    cfg.iso_dir = "isos".into();
    let tmdb = Some(tmdb_movie("Lumina", 2023));
    // Raw output, no normalising: destination_root_for emits '/' on every platform.
    assert_eq!(
        destination_root_for(&cfg, &tmdb, "Lumina.iso"),
        "/out/isos",
        "iso files route to the iso root"
    );
    assert_eq!(
        destination_root_for(&cfg, &tmdb, "Lumina.mkv"),
        destination_root(&cfg, &tmdb),
        "non-iso files keep the movie/tv root"
    );
    // Case-insensitive extension match.
    assert_eq!(destination_root_for(&cfg, &tmdb, "Lumina.ISO"), "/out/isos");
}

fn noop_progress(_: u8, _: f64, _: f64, _: f64) {}

#[test]
fn move_file_skips_when_dest_size_matches() {
    // Circuit breaker: a prior tick already cp'd the file but
    // couldn't unlink src. Re-detecting the same-size dest must NOT
    // recopy — that's the bug this fix exists for.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    let dest = tmp.path().join("b.mkv");
    // Valid EBML-framed payload: Skipped now requires the pre-existing
    // dest to pass the same post-copy validation the copy path runs.
    write_minimal_mkv(&src, b"hello world");
    write_minimal_mkv(&dest, b"hello world");
    let outcome = move_file(&src, &dest, &noop_progress);
    assert_eq!(outcome, MoveOutcome::Skipped);
    assert!(src.exists(), "src must remain untouched on Skipped");
    assert!(dest.exists());
}

#[test]
fn move_file_moves_when_dest_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    let dest = tmp.path().join("sub/b.mkv");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::write(&src, b"data data data").unwrap();
    let outcome = move_file(&src, &dest, &noop_progress);
    assert_eq!(outcome, MoveOutcome::Moved);
    assert!(!src.exists(), "rename consumes src");
    assert_eq!(std::fs::read(&dest).unwrap(), b"data data data");
}

#[test]
fn move_file_overwrites_when_dest_size_differs() {
    // A partial dest from a previous failed cp must NOT cause a
    // permanent stall — the new full src should overwrite it.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    let dest = tmp.path().join("b.mkv");
    std::fs::write(&src, b"new full content").unwrap();
    std::fs::write(&dest, b"partial").unwrap();
    let outcome = move_file(&src, &dest, &noop_progress);
    assert_eq!(outcome, MoveOutcome::Moved);
    assert_eq!(std::fs::read(&dest).unwrap(), b"new full content");
}

// FIX 4 — STRANDED_WARNED was inserted-into but never pruned, growing unbounded (the same
// leak MOVE_ERRORS bounds via prune_move_errors).
#[test]
fn prune_stranded_warned_drops_vanished_dirs_keeps_present() {
    let _g = errors_guard();
    {
        let mut m = STRANDED_WARNED.lock().unwrap_or_else(|e| e.into_inner());
        m.clear();
        m.insert("/staging/still_here".to_string());
        m.insert("/staging/removed_by_operator".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    seen.insert("/staging/still_here".to_string());

    prune_stranded_warned(&seen);

    let m = STRANDED_WARNED.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        m.contains("/staging/still_here"),
        "a dir still present this pass must keep its one-time-warn entry"
    );
    assert!(
        !m.contains("/staging/removed_by_operator"),
        "a dir gone from the pass must be pruned so the set can't grow unbounded"
    );
}

// A destination-keyed MOVE_ERRORS row must clear itself once a later move to that same
// destination succeeds.
#[test]
fn a_successful_move_clears_a_stale_destination_keyed_error() {
    let _g = errors_guard();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    let dest = tmp.path().join("b.mkv");
    std::fs::write(&src, b"complete content").unwrap();
    let dest_key = dest.to_string_lossy().to_string();

    // The row a previous failed copy + failed cleanup left behind.
    record_error(
        &dest_key,
        "partial copy could not be removed",
        "delete manually to unblock",
    );
    assert!(
        error_snapshot(&dest_key).is_some(),
        "fixture: the stale row must exist before the successful move"
    );

    let outcome = move_file(&src, &dest, &noop_progress);

    let left = error_snapshot(&dest_key);
    clear_error(&dest_key);
    assert_eq!(outcome, MoveOutcome::Moved, "the move itself must succeed");
    assert!(
        left.is_none(),
        "a successful move to this destination proves the stuck-partial \
             row is stale; leaving it makes MOVE_ERRORS grow one permanent \
             entry per rip with no automatic remover"
    );
}

// Partial-dest cleanup contract: a failed copy
// must NOT leave a partial/garbage dest, or the next tick sees a
// phantom Collision.
#[test]
fn move_file_copy_failure_leaves_no_partial_dest() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    std::fs::write(&src, b"source bytes").unwrap();

    // dest's "parent" is a regular FILE, so rename(2)/File::create both
    // fail ENOTDIR — exercises the failure cleanup without needing a
    // cross-fs mount. No dest can ever be created, so none must remain.
    let not_a_dir = tmp.path().join("blocker");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let dest = not_a_dir.join("b.mkv");

    let outcome = move_file(&src, &dest, &noop_progress);
    assert_eq!(outcome, MoveOutcome::Failed, "copy failure → Failed");
    assert!(!dest.exists(), "no partial destination may be left behind");
    // Source is the only copy and must be preserved on any failure.
    assert!(src.exists(), "source must survive a failed move");
}

#[test]
fn move_file_does_not_skip_an_invalid_same_size_dest() {
    // Regression (finding 9): the Skipped pre-flight accepted a dest on
    // equal length + matching head/tail WITHOUT post-copy validation. A
    // structurally invalid dest must not be treated as already-moved.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    let dest = tmp.path().join("b.mkv");
    // Byte-identical, same length, matching head/tail — but no EBML magic,
    // so check_post_copy rejects it.
    let bytes = vec![0xAAu8; 4096];
    std::fs::write(&src, &bytes).unwrap();
    std::fs::write(&dest, &bytes).unwrap();
    let outcome = move_file(&src, &dest, &noop_progress);
    assert_ne!(
        outcome,
        MoveOutcome::Skipped,
        "an invalid same-size dest must not be accepted as already-moved"
    );
    assert_eq!(
        outcome,
        MoveOutcome::Moved,
        "rename overwrites the bad dest"
    );
}

#[test]
fn move_file_returns_moved_when_src_missing_but_dest_present() {
    // Earlier atomic rename succeeded; src is gone, dest is fine.
    // Re-entering move_file (e.g. on next tick before staging
    // cleanup) must not error out.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    let dest = tmp.path().join("b.mkv");
    // Dest must be a structurally-valid mkv: the src-missing fast path now
    // rejects a foreign/garbage dest rather than mislabelling it Moved.
    write_minimal_mkv(&dest, &vec![0xAA; 256]);
    let outcome = move_file(&src, &dest, &noop_progress);
    assert_eq!(outcome, MoveOutcome::Moved);
}

#[test]
fn move_file_does_not_report_moved_on_foreign_dest_when_src_missing() {
    // src is gone and the dest is a NON-media/foreign file: the idempotent
    // fast path must NOT claim Moved for it (it isn't our output).
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    let dest = tmp.path().join("b.mkv");
    std::fs::write(&dest, b"foreign non-media file, not our rip output").unwrap();
    let outcome = move_file(&src, &dest, &noop_progress);
    assert_ne!(
        outcome,
        MoveOutcome::Moved,
        "a foreign non-media dest is not proof of a completed move; got {outcome:?}"
    );
}

// An ISO (or any format with no structural check) at dest can't be told apart from a foreign
// file once src is gone, so it must not be reported Moved; the foreign file stays untouched.
#[test]
fn move_file_does_not_report_moved_on_unverifiable_dest_when_src_missing() {
    let tmp = tempfile::tempdir().unwrap();
    for name in ["b.iso", "b.ISO", "b.txt", "b"] {
        let src = tmp.path().join(format!("src-{name}"));
        let dest = tmp.path().join(name);
        std::fs::write(&dest, b"somebody else's file").unwrap();
        let outcome = move_file(&src, &dest, &noop_progress);
        assert_ne!(
            outcome,
            MoveOutcome::Moved,
            "unverifiable dest {name} reported Moved with src missing"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            b"somebody else's file",
            "the foreign dest {name} must be left untouched"
        );
    }
}

// The pre-flight "src missing, dest present" branch must require a genuine NotFound on the
// src stat, not just any error (EACCES/EIO/ ESTALE prove nothing).
#[cfg(unix)]
#[test]
fn move_file_does_not_report_moved_on_non_notfound_src_stat_error() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let src_dir = tmp.path().join("staging");
    std::fs::create_dir_all(&src_dir).unwrap();
    let src = src_dir.join("a.mkv");
    std::fs::write(&src, b"still here, just unreadable right now").unwrap();

    let dest = tmp.path().join("b.mkv");
    // Predates this attempt — could be the real completed copy (e.g. a
    // MovedDirty leftover whose src unlink failed on a prior tick) or a
    // stale partial from an unrelated attempt. Either way, not proof.
    std::fs::write(&dest, b"already at the destination").unwrap();

    // Strip all permissions from src's PARENT so `stat(src)` fails with
    // EACCES (can't even traverse to look it up) rather than ENOENT —
    // src still physically exists on disk.
    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let perms_enforced = std::fs::File::open(&src).is_err();
    let outcome = move_file(&src, &dest, &noop_progress);

    // Restore so tempdir cleanup can remove everything.
    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    if !perms_enforced {
        eprintln!(
            "SKIP move_file_does_not_report_moved_on_non_notfound_src_stat_error: \
                 running with read-through privileges (root?)"
        );
        return;
    }
    assert_ne!(
        outcome,
        MoveOutcome::Moved,
        "a non-NotFound src stat error proves nothing about src; got {outcome:?}"
    );
}

#[test]
fn move_file_collides_when_dest_same_size_different_content() {
    // Atomicity/safety contract: when the dest already holds a DIFFERENT
    // file of the SAME length, move_file must NOT clobber it — it
    // returns Collision and leaves both files intact for the operator.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.mkv");
    let dest = tmp.path().join("b.mkv");
    // Equal length, differing content: same 4-byte EBML magic, same total
    // size, but the payload differs (so head/tail probe mismatches).
    write_minimal_mkv(&src, b"AAAAAAAAAAAA");
    write_minimal_mkv(&dest, b"BBBBBBBBBBBB");
    assert_eq!(
        std::fs::metadata(&src).unwrap().len(),
        std::fs::metadata(&dest).unwrap().len(),
        "precondition: equal length"
    );

    let outcome = move_file(&src, &dest, &noop_progress);
    assert_eq!(
        outcome,
        MoveOutcome::Collision,
        "same-size different-content dest must collide, not overwrite"
    );
    // Both originals survive untouched.
    assert_eq!(std::fs::read(&src).unwrap(), {
        let mut b = vec![0x1A, 0x45, 0xDF, 0xA3];
        b.extend_from_slice(b"AAAAAAAAAAAA");
        b
    });
    assert_eq!(std::fs::read(&dest).unwrap(), {
        let mut b = vec![0x1A, 0x45, 0xDF, 0xA3];
        b.extend_from_slice(b"BBBBBBBBBBBB");
        b
    });
}

// Cross-device (EXDEV) copy+unlink SUCCESS path, driven end-to-end through move_file
// against a SEPARATE real filesystem; SKIPS when one isn't available.
#[cfg(unix)]
#[test]
fn move_file_cross_device_copy_unlink_success_when_two_filesystems_exist() {
    // Find a tempdir on a filesystem different from std::env::temp_dir().
    let primary = tempfile::tempdir().unwrap();
    let primary_dev = std::fs::metadata(primary.path())
        .ok()
        .and_then(dev_id_of)
        .expect("stat primary tempdir");

    // Candidate roots that are commonly a distinct filesystem.
    let candidates = ["/dev/shm", "/run/user", "/tmp", "/var/tmp"];
    let secondary_root = candidates.iter().find_map(|root| {
        let p = std::path::Path::new(root);
        if !p.is_dir() {
            return None;
        }
        let dev = std::fs::metadata(p).ok().and_then(dev_id_of)?;
        if dev != primary_dev { Some(p) } else { None }
    });

    let Some(secondary_root) = secondary_root else {
        eprintln!(
            "SKIP move_file_cross_device_copy_unlink_success: no second \
                 filesystem available — EXDEV copy path not exercised (see test doc)"
        );
        return;
    };

    // Source on the secondary fs, dest on the primary fs → rename across
    // them returns EXDEV, forcing the copy+unlink fallback.
    let work = secondary_root.join(format!("autorip-xdev-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&work);
    let src = work.join("a.mkv");
    let dest = primary.path().join("a.mkv");
    write_minimal_mkv(&src, b"cross device payload bytes");

    let outcome = move_file(&src, &dest, &noop_progress);

    // Best-effort cleanup of the secondary-fs scratch dir.
    let _ = std::fs::remove_dir_all(&work);

    // If for some reason rename still succeeded (same fs after all), we'd
    // get Moved too — both acceptable; the key assertions are that the
    // dest holds the bytes and the src was unlinked.
    assert_eq!(
        outcome,
        MoveOutcome::Moved,
        "cross-device move must succeed"
    );
    assert!(
        !src.exists(),
        "src must be unlinked after a successful copy"
    );
    let moved = std::fs::read(&dest).unwrap();
    assert_eq!(
        &moved[..4],
        &[0x1A, 0x45, 0xDF, 0xA3],
        "dest is the moved MKV"
    );
}

// POSIX-only: the `st_dev` device id behind the EXDEV cross-device
// detection above, whose callers are all `#[cfg(unix)]`. Gated to match so
// it is not flagged dead code on the Windows CI leg.
#[cfg(unix)]
fn dev_id_of(m: std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(m.dev())
}

// Helpers for the structural checks: real MKVs are EBML-framed with
// magic [1A 45 DF A3] at offset 0; real BD-TS .m2ts uses 192-byte
// packets with TS sync 0x47 at offset 4 within each packet.

fn write_minimal_mkv(path: &std::path::Path, payload: &[u8]) {
    let mut bytes = vec![0x1A, 0x45, 0xDF, 0xA3];
    bytes.extend_from_slice(payload);
    std::fs::write(path, bytes).unwrap();
}

fn write_minimal_m2ts(path: &std::path::Path, packets: u64) {
    let mut bytes = Vec::with_capacity((packets * 192) as usize);
    for _ in 0..packets {
        // 4-byte arrival-time prefix, then 0x47 sync, then 187 bytes.
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.push(0x47);
        bytes.extend_from_slice(&[0u8; 187]);
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn check_post_copy_size_passes_on_equal_sizes() {
    // Non-mkv/m2ts path: routes to fresh-FD size compare.
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("a.iso");
    let dst = tmp.path().join("b.iso");
    std::fs::write(&src, b"identical bytes here").unwrap();
    std::fs::write(&dst, b"identical bytes here").unwrap();
    assert!(check_post_copy(&src, &dst).is_ok());
}

#[test]
fn check_post_copy_size_catches_short_dst_for_iso() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.iso");
    let dst = tmp.path().join("dst.iso");
    std::fs::write(&src, vec![0u8; 4096]).unwrap();
    std::fs::write(&dst, vec![0u8; 1024]).unwrap();
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(matches!(err, MoveError::SizeDoesNotMatch { .. }));
}

#[test]
fn check_post_copy_size_catches_missing_dst() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.iso");
    let dst = tmp.path().join("never_created.iso");
    std::fs::write(&src, b"some bytes").unwrap();
    // A missing destination must surface as an error, never a silent
    // pass: fresh_metadata's Err previously defaulted to 0 on both
    // sides (0 == 0), letting move_file unlink the source and lose it.
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(matches!(err, MoveError::Unreadable(_)), "got {:?}", err);
}

#[test]
fn check_post_copy_mkv_passes_on_valid_ebml_head_and_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("good.mkv");
    // Body is at least 5 bytes so the tail check has bytes to read.
    write_minimal_mkv(&dst, &vec![0xAA; 256]);
    // src must match dst size: check_post_copy now pairs the
    // structural check with a src-vs-dst size cross-check.
    let src = tmp.path().join("src.mkv");
    write_minimal_mkv(&src, &vec![0xAA; 256]);
    assert!(check_post_copy(&src, &dst).is_ok());
}

#[test]
fn check_post_copy_mkv_rejects_bad_head() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("bad_head.mkv");
    // First 4 bytes are NOT EBML magic.
    std::fs::write(&dst, b"NOPE bytes after").unwrap();
    let src = tmp.path().join("src.mkv");
    std::fs::write(&src, b"any").unwrap();
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(matches!(err, MoveError::MkvBadHead));
}

#[test]
fn check_post_copy_mk3d_runs_matroska_structural_check() {
    // mk3d is byte-identical Matroska, so check_post_copy must route it
    // through the same structural EBML validation as mkv. Mutation check:
    // drop "mk3d" from that arm and the bad-head case would wrongly pass.
    let tmp = tempfile::tempdir().unwrap();

    // Valid mk3d passes the structural + size check.
    let good = tmp.path().join("good.mk3d");
    write_minimal_mkv(&good, &vec![0xAA; 256]);
    let good_src = tmp.path().join("good_src.mk3d");
    write_minimal_mkv(&good_src, &vec![0xAA; 256]);
    assert!(check_post_copy(&good_src, &good).is_ok());

    // A bad-head mk3d is rejected as Matroska — proving the structural
    // check ran rather than the size-only fallback.
    let bad = tmp.path().join("bad.mk3d");
    std::fs::write(&bad, b"NOPE bytes after").unwrap();
    let bad_src = tmp.path().join("bad_src.mk3d");
    std::fs::write(&bad_src, b"NOPE bytes after").unwrap();
    let err = check_post_copy(&bad_src, &bad).unwrap_err();
    assert!(
        matches!(err, MoveError::MkvBadHead),
        "mk3d must be validated as Matroska, got {err:?}"
    );
}

#[test]
fn check_post_copy_mkv_rejects_truncated_tail() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("trunc.mkv");
    // Only 4 bytes total — head OK, but tail check requires >= 5.
    std::fs::write(&dst, [0x1A, 0x45, 0xDF, 0xA3]).unwrap();
    let src = tmp.path().join("src.mkv");
    std::fs::write(&src, b"any").unwrap();
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(matches!(err, MoveError::MkvBadTail));
}

#[test]
fn check_post_copy_m2ts_passes_on_aligned_sync_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("good.m2ts");
    write_minimal_m2ts(&dst, 32); // > 16 packets, plenty for head+tail
    // src must match dst size for the size cross-check.
    let src = tmp.path().join("src.m2ts");
    write_minimal_m2ts(&src, 32);
    assert!(check_post_copy(&src, &dst).is_ok());
}

#[test]
fn check_post_copy_m2ts_rejects_garbage() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("bad.m2ts");
    // Garbage 16 * 192 bytes — no 0x47 at the sync offsets.
    std::fs::write(&dst, vec![0xFE; 16 * 192]).unwrap();
    let src = tmp.path().join("src.m2ts");
    std::fs::write(&src, b"any").unwrap();
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(matches!(err, MoveError::M2tsBadSync));
}

#[test]
fn check_post_copy_m2ts_rejects_short_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dst = tmp.path().join("short.m2ts");
    std::fs::write(&dst, [0u8; 100]).unwrap(); // smaller than 8 * 192
    let src = tmp.path().join("src.m2ts");
    std::fs::write(&src, b"any").unwrap();
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(matches!(err, MoveError::M2tsBadSync));
}

#[test]
fn record_error_dedups_same_reason_without_logging_again() {
    // Same path + same reason twice → the second logs nothing (the log
    // call is gated on reason change); a new reason logs again.
    let _g = errors_guard();
    let path = "/tmp/fakemover-dedup-test";
    clear_error(path);
    let logs = std::cell::Cell::new(0);
    record_error_with(path, "stuck", "do thing", |_| logs.set(logs.get() + 1));
    record_error_with(path, "stuck", "do thing", |_| logs.set(logs.get() + 1));
    assert_eq!(logs.get(), 1, "an unchanged reason must not log again");
    record_error_with(path, "other", "do thing", |_| logs.set(logs.get() + 1));
    record_error_with(path, "stuck", "do thing", |_| logs.set(logs.get() + 1));
    assert_eq!(logs.get(), 3, "a changed reason logs");
    let m = MOVE_ERRORS.lock().unwrap();
    let entry = m.get(path).expect("error recorded");
    assert_eq!(entry.reason, "stuck");
    drop(m);
    clear_error(path);
    assert!(MOVE_ERRORS.lock().unwrap().get(path).is_none());
}

// The syslog write is blocking (NFS) I/O: MOVE_ERRORS must be free while it runs, or the
// System page and every other record/clear stall behind it.
#[test]
fn record_error_does_not_hold_move_errors_while_logging() {
    let _g = errors_guard();
    let path = "/tmp/fakemover-lock-across-log";
    clear_error(path);
    let mut logged = false;
    let mut lock_free = false;
    record_error_with(path, "stuck", "hint", |_| {
        logged = true;
        lock_free = test_lock_is_free(&MOVE_ERRORS);
    });
    let recorded = error_snapshot(path).is_some();
    clear_error(path);
    assert!(logged, "a new reason must be logged");
    assert!(recorded, "the error must still be recorded");
    assert!(
        lock_free,
        "MOVE_ERRORS was held across the blocking syslog write"
    );
}

// 0.25.10 fixes regression tests.

fn marker_json(title: &str) -> String {
    serde_json::json!({
        "title": title,
        "disc_name": title,
        "format": "BD",
        "year": 2024,
        "media_type": "movie",
        "poster_url": "",
        "overview": "",
        "date": "2026-05-20",
    })
    .to_string()
}

fn cfg_for_staging(staging: &std::path::Path, movie_dir: &str, keep_iso: bool) -> Config {
    Config {
        staging_dir: staging.to_string_lossy().to_string(),
        output_dir: staging
            .parent()
            .unwrap()
            .join("output")
            .to_string_lossy()
            .to_string(),
        movie_dir: movie_dir.to_string(),
        tv_dir: String::new(),
        keep_iso,
        ..Config::default()
    }
}

#[test]
fn a_hung_destination_holds_the_move_without_blocking_the_mover() {
    let _g = super::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);
    let disc_dir = staging.join("Held Up");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Held Up")).unwrap();
    let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
    mkv.extend_from_slice(&[0xAAu8; 1024]);
    std::fs::write(disc_dir.join("Held Up.mkv"), &mkv).unwrap();
    // A probe of the movie folder that the "kernel" never returns from.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let hung = crate::server::health::bounded(
        &movie_dir,
        std::time::Duration::from_millis(50),
        move || {
            let _ = rx.recv();
        },
    );
    assert_eq!(hung, crate::server::health::Bounded::TimedOut);
    let started = std::time::Instant::now();
    check_and_move(&cfg);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    let dest = movie_dir.join("Held Up (2024)/Held Up (2024).mkv");
    assert!(
        !dest.exists() && disc_dir.join("Held Up.mkv").exists(),
        "kept in staging"
    );
    let err = MOVE_ERRORS
        .lock()
        .unwrap()
        .get(&*disc_dir.to_string_lossy())
        .cloned()
        .expect("the hold is shown");
    assert!(
        err.reason.contains("waiting for the output folder"),
        "{err:?}"
    );
    assert!(err.reason.contains("not responding"), "{err:?}");
    assert!(err.hint.contains("Remount the share"), "{err:?}");
    // The share answers again: the next tick moves it.
    drop(tx);
    let t = std::time::Instant::now();
    while crate::server::health::preflight("output", &movie_dir).is_err() {
        assert!(t.elapsed() < std::time::Duration::from_secs(5));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    check_and_move(&cfg);
    assert!(dest.exists(), "moved once the folder answers");
    clear_error(&disc_dir.to_string_lossy());
}

#[test]
fn check_and_move_skips_iso_when_keep_iso_false() {
    // Regression for 0.25.10: pre-fix the mover blindly moved ANY .iso in a .done dir,
    // landing a 90+ GB ISO in the movie library (2026-05-20) even with keep_iso=false,
    // since the scan loop beat the ripper's ISO-prune.
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    // One staging "disc dir" with .done + a valid .mkv + an .iso.
    let disc_dir = staging.join("Gleaming For Good");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Gleaming For Good")).unwrap();
    // Valid EBML head + tail-safe body so check_post_copy_mkv passes.
    let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
    mkv.extend_from_slice(&[0xAAu8; 1024]);
    std::fs::write(disc_dir.join("Gleaming For Good.mkv"), &mkv).unwrap();
    std::fs::write(disc_dir.join("Gleaming For Good.iso"), vec![0u8; 4096]).unwrap();

    check_and_move(&cfg);

    // MKV landed in the movie library.
    let mkv_dest = movie_dir.join("Gleaming For Good (2024)/Gleaming For Good (2024).mkv");
    assert!(
        mkv_dest.exists(),
        "MKV should have been moved to {}",
        mkv_dest.display()
    );

    // ISO must NOT have been promoted to the movie library.
    let iso_dest = movie_dir.join("Gleaming For Good (2024)/Gleaming For Good (2024).iso");
    assert!(
        !iso_dest.exists(),
        "ISO must not be moved when keep_iso=false (found at {})",
        iso_dest.display()
    );

    // Staging is torn down on the same tick because the MKV moved
    // cleanly and the orphan ISO was swept by remove_dir_all.
    assert!(
        !disc_dir.exists(),
        "staging disc dir should have been removed after successful MKV move"
    );
}

#[test]
fn check_and_move_delivers_mk3d_main_feature() {
    // Regression: a Blu-ray 3D rip stages <title>.mk3d (byte-identical Matroska). The
    // staging-scan extension whitelist must recognize mk3d for delivery — if dropped, a 3D
    // rip silently never leaves staging.
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    // One .done staging dir holding only a valid .mk3d file.
    let disc_dir = staging.join("Stereo Vista");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Stereo Vista")).unwrap();
    // Valid EBML head + tail-safe body so check_post_copy (mk3d → mkv
    // structural + size checks) passes.
    let mut mk3d = vec![0x1A, 0x45, 0xDF, 0xA3];
    mk3d.extend_from_slice(&[0xAAu8; 1024]);
    std::fs::write(disc_dir.join("Stereo Vista.mk3d"), &mk3d).unwrap();

    check_and_move(&cfg);

    // The mk3d landed in the movie library with its extension preserved.
    let dest = movie_dir.join("Stereo Vista (2024)/Stereo Vista (2024).mk3d");
    assert!(
        dest.exists(),
        "mk3d main feature should have been delivered to {}",
        dest.display()
    );
    // Delivered cleanly → staging torn down.
    assert!(
        !disc_dir.exists(),
        "staging disc dir should have been removed after successful mk3d move"
    );
}

#[cfg(unix)]
#[test]
fn check_and_move_records_error_when_inner_read_dir_fails() {
    // Regression: when read_dir on the staging disc dir fails after .done is parsed, the
    // dir must NOT be skipped silently. Pre-fix this dropped the failure with no
    // record_error/log, invisible on the System page.
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    let disc_dir = staging.join("Unlistable");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Unlistable")).unwrap();

    let dir_str = disc_dir.to_string_lossy().to_string();
    let _g = errors_guard();
    clear_error(&dir_str);

    // Owner execute-only (0o100): search bit lets read_to_string open
    // the known-path .done, but read bit cleared makes read_dir EACCES.
    std::fs::set_permissions(&disc_dir, std::fs::Permissions::from_mode(0o100)).unwrap();
    if std::fs::read_dir(&disc_dir).is_ok() {
        std::fs::set_permissions(&disc_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!(
            "SKIP check_and_move_records_error_when_inner_read_dir_fails: \
                 running with read-through privileges"
        );
        return;
    }

    check_and_move(&cfg);

    // Restore perms so tempdir teardown can recurse.
    std::fs::set_permissions(&disc_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

    let recorded = {
        let m = MOVE_ERRORS.lock().unwrap();
        m.get(&dir_str).cloned()
    };
    clear_error(&dir_str);
    assert!(
        recorded.is_some(),
        "a read_dir failure on the staging dir must record a mover error"
    );
}

#[test]
fn check_and_move_moves_iso_when_keep_iso_true() {
    // Companion to the regression above: with keep_iso=true the operator explicitly wants
    // the ISO promoted alongside the MKV. The build_destination fix already routes them to
    // distinct paths; this pins the filter behaviour.
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), true);

    let disc_dir = staging.join("Keepme");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Keepme")).unwrap();
    let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
    mkv.extend_from_slice(&[0xAAu8; 1024]);
    std::fs::write(disc_dir.join("Keepme.mkv"), &mkv).unwrap();
    std::fs::write(disc_dir.join("Keepme.iso"), vec![0u8; 4096]).unwrap();

    check_and_move(&cfg);

    assert!(movie_dir.join("Keepme (2024)/Keepme (2024).mkv").exists());
    assert!(movie_dir.join("Keepme (2024)/Keepme (2024).iso").exists());
}

// A rip can deliver an MKV and its companion ISO; both must take the SAME `_2` suffix.
#[test]
fn check_and_move_gives_a_discs_companion_files_the_same_variant() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), true);

    // Disc 1 already delivered its MKV — but its ISO never made it, so the
    // base ISO name is FREE while the base MKV name is taken.
    let dest_dir = movie_dir.join("Pair (2024)");
    std::fs::create_dir_all(&dest_dir).unwrap();
    let mut first = vec![0x1A, 0x45, 0xDF, 0xA3];
    first.extend_from_slice(&[0x11u8; 4096]);
    std::fs::write(dest_dir.join("Pair (2024).mkv"), &first).unwrap();

    // Disc 2 in staging, MKV + ISO.
    let disc_dir = staging.join("Pair");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Pair")).unwrap();
    let mut second = vec![0x1A, 0x45, 0xDF, 0xA3];
    second.extend_from_slice(&[0x22u8; 4096]);
    std::fs::write(disc_dir.join("Pair.mkv"), &second).unwrap();
    std::fs::write(disc_dir.join("Pair.iso"), vec![0x33u8; 4096]).unwrap();

    check_and_move(&cfg);

    assert!(
        dest_dir.join("Pair (2024)_2.mkv").exists(),
        "disc 2's MKV must move aside from disc 1's"
    );
    assert!(
        dest_dir.join("Pair (2024)_2.iso").exists(),
        "disc 2's ISO must take the SAME variant as its MKV, not the free base name"
    );
    assert!(
        !dest_dir.join("Pair (2024).iso").exists(),
        "the pair must not be split across the base name and `_2`"
    );
    assert_eq!(
        std::fs::read(dest_dir.join("Pair (2024).mkv")).unwrap(),
        first,
        "disc 1's delivered file must be untouched"
    );
}

// Regression: state.json's outputs[] is the AUTHORITATIVE deliverable list for a TV rip; a
// leftover partial not in outputs[] must not be promoted.
#[test]
fn check_and_move_files_only_outputs_for_a_tv_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let output_dir = tmp.path().join("output");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(output_dir.join("TV")).unwrap();

    let cfg = Config {
        staging_dir: staging.to_string_lossy().to_string(),
        output_dir: output_dir.to_string_lossy().to_string(),
        movie_dir: String::new(),
        tv_dir: "TV".to_string(),
        ..Config::default()
    };

    let disc_dir = staging.join("Endeavour_S05");
    std::fs::create_dir_all(&disc_dir).unwrap();

    let mut st = crate::server::ripper::staging::DiscState::new(
        crate::server::ripper::staging::StagingState::Done,
    );
    st.title = "Endeavour".to_string();
    st.disc_name = "Endeavour".to_string();
    st.year = 2012;
    st.media_type = "tv".to_string();
    st.tmdb_id = 12345;
    st.season = Some(5);
    st.outputs = vec![
        ep_output("Endeavour_S05E01.mkv", Some(1), "Muse"),
        ep_output("Endeavour_S05E02.mkv", Some(2), "Cartouche"),
    ];
    crate::server::ripper::staging::write_state(&disc_dir, &st);

    // Two planned episodes, plus a leftover partial NOT in outputs[].
    std::fs::write(disc_dir.join("Endeavour_S05E01.mkv"), vec![0x11u8; 4096]).unwrap();
    std::fs::write(disc_dir.join("Endeavour_S05E02.mkv"), vec![0x22u8; 4096]).unwrap();
    std::fs::write(disc_dir.join("Endeavour_S05E03.mkv"), vec![0x33u8; 4096]).unwrap();

    check_and_move(&cfg);

    let series_dir = output_dir
        .join("TV")
        .join("Endeavour (2012)")
        .join("Season 05");
    assert!(
        series_dir.join("Endeavour S05E01 - Muse.mkv").exists(),
        "planned episode 1 (in outputs[]) must be filed"
    );
    assert!(
        series_dir.join("Endeavour S05E02 - Cartouche.mkv").exists(),
        "planned episode 2 (in outputs[]) must be filed"
    );

    // The leftover partial must not appear ANYWHERE in the output tree,
    // neither under its raw staging name nor any S05E03 leaf.
    let mut found_leftover = false;
    for entry in walkdir_files(&output_dir) {
        let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == "Endeavour_S05E03.mkv" || name.contains("S05E03") {
            found_leftover = true;
        }
    }
    assert!(
        !found_leftover,
        "the leftover S05E03 partial is not in outputs[] and must NOT be \
             promoted into the library"
    );
}

// Regression: the TV outputs[] filter never listed the intermediate.iso, so a keep_iso=true
// TV rip's ISO was dropped then destroyed by teardown.
#[test]
fn check_and_move_keeps_iso_for_a_tv_dir_with_keep_iso() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let output_dir = tmp.path().join("output");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(output_dir.join("TV")).unwrap();

    let cfg = Config {
        staging_dir: staging.to_string_lossy().to_string(),
        output_dir: output_dir.to_string_lossy().to_string(),
        movie_dir: String::new(),
        tv_dir: "TV".to_string(),
        keep_iso: true,
        ..Config::default()
    };

    let disc_dir = staging.join("Endeavour_S05");
    std::fs::create_dir_all(&disc_dir).unwrap();

    let mut st = crate::server::ripper::staging::DiscState::new(
        crate::server::ripper::staging::StagingState::Done,
    );
    st.title = "Endeavour".to_string();
    st.disc_name = "Endeavour".to_string();
    st.year = 2012;
    st.media_type = "tv".to_string();
    st.tmdb_id = 12345;
    st.season = Some(5);
    st.outputs = vec![ep_output("Endeavour_S05E01.mkv", Some(1), "Muse")];
    crate::server::ripper::staging::write_state(&disc_dir, &st);

    // The planned episode, plus the intermediate ISO (kept via keep_iso,
    // never listed in outputs[]).
    std::fs::write(disc_dir.join("Endeavour_S05E01.mkv"), vec![0x11u8; 4096]).unwrap();
    std::fs::write(disc_dir.join("Endeavour.iso"), vec![0x22u8; 512]).unwrap();

    check_and_move(&cfg);

    let series_dir = output_dir
        .join("TV")
        .join("Endeavour (2012)")
        .join("Season 05");
    assert!(
        series_dir.join("Endeavour S05E01 - Muse.mkv").exists(),
        "the planned episode must be filed"
    );
    assert!(
        series_dir.join("Endeavour.iso").exists(),
        "keep_iso=true must promote the intermediate ISO alongside the \
             episode, not drop it to be destroyed by staging teardown"
    );
}

// Minimal recursive file walk used only by
// check_and_move_files_only_outputs_for_a_tv_dir to scan the whole
// output tree for a leaked leftover file.
fn walkdir_files(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

// POSIX-only: with_file_name re-renders the parent with the platform separator, so a POSIX
// fixture comes back mixed on Windows.
#[cfg(unix)]
#[test]
fn dest_with_variant_suffixes_the_stem_and_leaves_variant_one_alone() {
    let d = "/mnt/media/movies/Aurora Drift (2024)/Aurora Drift (2024).mkv";
    assert_eq!(
        dest_with_variant(d, 1),
        d,
        "the first disc keeps the plain name — existing libraries must not churn"
    );
    assert_eq!(
        dest_with_variant(d, 3),
        "/mnt/media/movies/Aurora Drift (2024)/Aurora Drift (2024)_3.mkv",
        "the suffix goes on the STEM, never after the extension, and the \
             per-title DIRECTORY is left alone so a set stays in one folder"
    );
    // Extensionless leaf: suffix still lands on the name.
    assert_eq!(dest_with_variant("/a/b/Title", 2), "/a/b/Title_2");
    // Same rendering as the staging-directory rule — one policy.
    assert_eq!(
        dest_with_variant("/a/b/Title.mkv", 2),
        format!(
            "/a/b/{}.mkv",
            crate::server::util::disc_variant_name("Title", 2)
        )
    );
}

#[test]
fn copy_counting_copies_bytes_and_publishes_total() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.bin");
    let dst = tmp.path().join("dst.bin");
    // Larger than the 4 MiB chunk so the counter ticks more than once.
    let data = vec![0xABu8; 5 * 1024 * 1024 + 17];
    std::fs::write(&src, &data).unwrap();
    let written = AtomicU64::new(0);
    let n = copy_counting(&src, &dst, &written).unwrap();
    assert_eq!(n, data.len() as u64, "returns total bytes copied");
    assert_eq!(
        written.load(Ordering::Relaxed),
        data.len() as u64,
        "final published count equals the source size"
    );
    assert_eq!(
        std::fs::read(&dst).unwrap(),
        data,
        "dest is a faithful copy"
    );
}

#[test]
fn copy_counting_errors_on_missing_source() {
    use std::sync::atomic::AtomicU64;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("nope.bin");
    let dst = tmp.path().join("dst.bin");
    let written = AtomicU64::new(0);
    assert!(copy_counting(&src, &dst, &written).is_err());
}

// Regression (temp + rename atomicity): a failed/interrupted copy must NOT leave any file
// at the FINAL dest name.
#[test]
fn copy_counting_failure_leaves_no_file_at_final_name() {
    use std::sync::atomic::AtomicU64;
    let tmp = tempfile::tempdir().unwrap();
    // Missing source → the copy errors out. (The same no-final-file
    // invariant holds for a mid-stream SIGKILL: bytes only ever exist
    // at the temp name until the atomic rename.)
    let src = tmp.path().join("missing.bin");
    let dst = tmp.path().join("final.mkv");
    let written = AtomicU64::new(0);
    assert!(copy_counting(&src, &dst, &written).is_err());
    assert!(
        !dst.exists(),
        "a failed copy must leave no file at the final dest name"
    );
    // And no orphan temp lingers next to it.
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("final.mkv.part-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "interrupted copy must not orphan a .part temp, found {leftovers:?}"
    );
}

/// Positive path: a successful `copy_counting` produces the final file
/// atomically (via rename) with the exact source bytes, and leaves no
/// `.part-` temp behind.
#[test]
fn copy_counting_success_renames_atomically_and_cleans_temp() {
    use std::sync::atomic::AtomicU64;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.bin");
    let dst = tmp.path().join("final.bin");
    let data = vec![0x5Au8; 3 * 1024 * 1024 + 5];
    std::fs::write(&src, &data).unwrap();
    let written = AtomicU64::new(0);
    let n = copy_counting(&src, &dst, &written).unwrap();
    assert_eq!(n, data.len() as u64);
    assert_eq!(
        std::fs::read(&dst).unwrap(),
        data,
        "final is a faithful copy"
    );
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".part-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "successful copy must leave no .part temp, found {leftovers:?}"
    );
}

#[test]
fn copy_counting_clears_orphaned_part_temps_from_other_pids() {
    use std::sync::atomic::AtomicU64;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src.bin");
    let dst = tmp.path().join("final.bin");
    std::fs::write(&src, vec![0x11u8; 1024]).unwrap();
    // Simulate orphaned temps left by prior crashed copies of THIS dest
    // under different pids. The current copy must sweep them before
    // writing its own fresh `.part-<pid>`.
    let orphan_a = tmp.path().join("final.bin.part-999991");
    let orphan_b = tmp.path().join("final.bin.part-999992");
    std::fs::write(&orphan_a, b"stale").unwrap();
    std::fs::write(&orphan_b, b"stale").unwrap();
    // An unrelated `.part-*` for a DIFFERENT dest must be left untouched.
    let unrelated = tmp.path().join("other.bin.part-999993");
    std::fs::write(&unrelated, b"keep").unwrap();

    let written = AtomicU64::new(0);
    copy_counting(&src, &dst, &written).unwrap();

    assert!(!orphan_a.exists(), "orphaned .part for this dest removed");
    assert!(!orphan_b.exists(), "orphaned .part for this dest removed");
    assert!(unrelated.exists(), "unrelated .part for other dest kept");
    assert_eq!(std::fs::read(&dst).unwrap(), vec![0x11u8; 1024]);
}

// SIGTERM must be observed BETWEEN CHUNKS, not at the end of the copy.
#[test]
fn copy_counting_aborts_between_chunks_when_shutdown_is_requested() {
    use std::sync::atomic::AtomicU64;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("abort-src.bin");
    let dst = tmp.path().join("abort-final.bin");
    // Two full 4 MiB chunks plus change, so "abort" and "ran to
    // completion" are distinguishable by the byte counter.
    std::fs::write(&src, vec![0x7Eu8; 9 * 1024 * 1024]).unwrap();
    let written = AtomicU64::new(0);

    let err = copy_counting_cancellable(&src, &dst, &written, &|| true)
        .expect_err("a copy must not run to completion after shutdown is requested");

    assert_eq!(
        err.kind(),
        std::io::ErrorKind::Interrupted,
        "the abort must be reported as Interrupted, not as a copy failure"
    );
    assert_eq!(
        written.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the abort must be seen before the first chunk is written"
    );
    assert!(
        !dst.exists(),
        "an aborted copy must not leave a file at the final name"
    );
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".part-"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "an aborted copy must unlink its .part temp, found {leftovers:?}"
    );
    assert_eq!(
        std::fs::read(&src).unwrap().len(),
        9 * 1024 * 1024,
        "the source must survive an aborted copy untouched"
    );
}

// The counterpart: a predicate that never fires must copy everything,
// else "abort immediately, always" would satisfy the test above.
#[test]
fn copy_counting_completes_when_the_abort_signal_stays_low() {
    use std::sync::atomic::AtomicU64;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("noabort-src.bin");
    let dst = tmp.path().join("noabort-final.bin");
    let data = vec![0x2Cu8; 5 * 1024 * 1024];
    std::fs::write(&src, &data).unwrap();
    let written = AtomicU64::new(0);
    let n = copy_counting_cancellable(&src, &dst, &written, &|| false).unwrap();
    assert_eq!(n, 5 * 1024 * 1024);
    assert_eq!(std::fs::read(&dst).unwrap(), data);
}

// A shutdown raised mid-copy is seen at the next chunk boundary, not at the end.
#[test]
fn copy_counting_sees_a_shutdown_raised_mid_copy() {
    use std::sync::atomic::AtomicU64;
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("midabort-src.bin");
    let dst = tmp.path().join("midabort-final.bin");
    std::fs::write(&src, vec![0x3Du8; 9 * 1024 * 1024]).unwrap();
    let written = AtomicU64::new(0);
    let checks = std::cell::Cell::new(0);
    // Low for the first chunk, raised from the second check on.
    let cancel = || {
        checks.set(checks.get() + 1);
        checks.get() > 1
    };

    let err = copy_counting_cancellable(&src, &dst, &written, &cancel)
        .expect_err("a shutdown raised mid-copy must abort it");
    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(
        written.load(std::sync::atomic::Ordering::Relaxed),
        4 * 1024 * 1024,
        "exactly one chunk is written before the abort is seen"
    );
    assert!(!dst.exists());
}

// ---- post-copy integrity + collision hardening tests ----

/// Repo-local, gitignored scratch dir (never /tmp). Each call makes a
/// unique subdir so parallel test threads don't collide.
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-scratch")
        .join(format!("{}-{}-{}", tag, std::process::id(), n));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn check_post_copy_mkv_rejects_truncated_above_head_window() {
    // Load-bearing: a structurally-valid head/tail must NOT pass when
    // dest is shorter than src. Pre-fix the mkv arm did head+tail only,
    // so a truncated copy passed and move_file unlinked the only copy.
    let dir = scratch_dir("mkv-trunc");
    let src = dir.join("src.mkv");
    let dst = dir.join("dst.mkv");
    // Full source: valid EBML + 1 MiB body.
    write_minimal_mkv(&src, &vec![0xAA; 1024 * 1024]);
    // Truncated dest: valid EBML head and a readable tail, but far
    // shorter than src. Structural check alone would pass.
    write_minimal_mkv(&dst, &vec![0xAA; 4096]);
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(
        matches!(err, MoveError::SizeDoesNotMatch { .. }),
        "truncated mkv must be rejected by the size cross-check, got {:?}",
        err
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_post_copy_m2ts_rejects_truncated_above_head_window() {
    // Load-bearing: same as mkv but for the TS-sync path. A copy with
    // fewer packets than src, yet enough intact sync bytes to clear
    // THRESHOLD, must still be rejected by the size cross-check.
    let dir = scratch_dir("m2ts-trunc");
    let src = dir.join("src.m2ts");
    let dst = dir.join("dst.m2ts");
    write_minimal_m2ts(&src, 4096); // full source
    write_minimal_m2ts(&dst, 64); // truncated, but structurally fine
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(
        matches!(err, MoveError::SizeDoesNotMatch { .. }),
        "truncated m2ts must be rejected by the size cross-check, got {:?}",
        err
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_post_copy_m2ts_rejects_overlapping_window_truncation() {
    // Secondary case: an 8..16-packet m2ts is too small for two disjoint
    // sample windows; pre-fix, the overlap let a single intact head
    // count twice to clear THRESHOLD. The 2x size floor now rejects it.
    let dir = scratch_dir("m2ts-overlap");
    let dst = dir.join("dst.m2ts");
    write_minimal_m2ts(&dst, 10); // 1920 bytes — between 1536 and 3072
    let src = dir.join("src.m2ts");
    std::fs::write(&src, b"any").unwrap();
    let err = check_post_copy(&src, &dst).unwrap_err();
    assert!(
        matches!(err, MoveError::M2tsBadSync),
        "8..16-packet m2ts must be rejected (overlapping sample windows), got {:?}",
        err
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_post_copy_m2ts_threshold_is_out_of_sixteen_not_eight() {
    // Regression for the line-390 doc bug: THRESHOLD=6 counts across BOTH the head (8) and
    // tail (8) windows — 6 of 16, not 6 of 8. Prove it: a file with 3 sync bytes in each
    // window (3 per window) must still PASS.
    const PKT: usize = 192;
    const SYNC_OFFSET: usize = 4;
    const PACKETS: usize = 24; // head=0..8, tail=16..24, disjoint middle gap
    let dir = scratch_dir("m2ts-threshold-16");
    let dst = dir.join("dst.m2ts");
    let mut bytes = vec![0u8; PACKETS * PKT];
    // 3 sync bytes in the head window (packets 0,1,2).
    for i in [0, 1, 2] {
        bytes[i * PKT + SYNC_OFFSET] = 0x47;
    }
    // 3 sync bytes in the tail window (last 8 packets: 16..24 → 21,22,23).
    for i in [21, 22, 23] {
        bytes[i * PKT + SYNC_OFFSET] = 0x47;
    }
    std::fs::write(&dst, &bytes).unwrap();
    assert!(
        check_post_copy_m2ts(&dst).is_ok(),
        "3 head + 3 tail = 6 of 16 must clear THRESHOLD; the gate counts \
             across both windows, not 6 of 8 in one"
    );
    // And confirm 5 total (3 head + 2 tail) is below THRESHOLD → rejected,
    // so the gate isn't trivially passing everything.
    bytes[23 * PKT + SYNC_OFFSET] = 0x00;
    std::fs::write(&dst, &bytes).unwrap();
    assert!(
        matches!(check_post_copy_m2ts(&dst), Err(MoveError::M2tsBadSync)),
        "5 of 16 must fall below THRESHOLD=6"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_post_copy_mkv_tail_comment_does_not_claim_64kib_ebml_scan() {
    // Regression for the line-349 doc bug: the MKV tail comment used to claim it reads 64
    // KiB and confirms a well-formed EBML close — neither is true (it reads 8 tail bytes).
    // Source-pin the corrected comment.
    let src = crate::server::util::source_lf(include_str!("mover.rs"));
    let start = src
        .find("fn check_post_copy_mkv")
        .expect("check_post_copy_mkv present");
    let body = &src[start..start + 1200];
    assert!(
        !body.contains("64 KiB"),
        "MKV tail comment must not claim a 64 KiB read the code never does"
    );
    assert!(
        !body.contains("EBML element close"),
        "MKV tail comment must not claim EBML-element-close detection"
    );
}

// Two DIFFERENT discs route to the same Title (Year) path with the SAME byte length (the
// boxset case).
#[test]
fn check_and_move_second_disc_of_a_title_is_filed_beside_the_first() {
    let dir = scratch_dir("collision");
    let staging = dir.join("staging");
    let movie_dir = dir.join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    // Pre-existing (OLD, wrong) library file at the destination path.
    let dest_dir = movie_dir.join("Clash (2024)");
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest_file = dest_dir.join("Clash (2024).mkv");
    let mut old = vec![0x1A, 0x45, 0xDF, 0xA3];
    old.extend_from_slice(&[0x11u8; 4096]);
    std::fs::write(&dest_file, &old).unwrap();

    // NEW rip in staging — SAME byte length, DIFFERENT content.
    let disc_dir = staging.join("Clash");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Clash")).unwrap();
    let mut new = vec![0x1A, 0x45, 0xDF, 0xA3];
    new.extend_from_slice(&[0x22u8; 4096]); // differs in body
    assert_eq!(new.len(), old.len(), "test setup: sizes must match");
    let staged_mkv = disc_dir.join("Clash.mkv");
    std::fs::write(&staged_mkv, &new).unwrap();

    let _g = errors_guard();
    check_and_move(&cfg);

    // Disc 1's library file must be untouched (still the OLD content).
    assert_eq!(
        std::fs::read(&dest_file).unwrap(),
        old,
        "existing library file must NOT be overwritten or removed"
    );
    // Disc 2 must be DELIVERED, not stranded: same title dir, `_2` name.
    let second = dest_dir.join("Clash (2024)_2.mkv");
    assert!(
        second.exists(),
        "the second disc of a title must be filed as `_2`, not left in staging"
    );
    assert_eq!(
        std::fs::read(&second).unwrap(),
        new,
        "the `_2` file must be THIS disc's output"
    );
    // Delivered means delivered — no operator error to clear.
    let key = disc_dir.to_string_lossy().to_string();
    assert!(
        error_snapshot(&key).is_none(),
        "a successfully filed second disc is not an operator error"
    );

    // Idempotency: a retried tick must re-claim `_2`, never walk to `_3`.
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Clash")).unwrap();
    std::fs::write(&staged_mkv, &new).unwrap();
    check_and_move(&cfg);
    assert!(
        !dest_dir.join("Clash (2024)_3.mkv").exists(),
        "a re-move of the SAME disc must re-claim its own name, not litter \
             the library with _3, _4, ..."
    );
    assert_eq!(
        std::fs::read(&second).unwrap(),
        new,
        "the re-move must leave this disc's delivered file intact"
    );
    assert_eq!(
        std::fs::read(&dest_file).unwrap(),
        old,
        "the re-move must still not touch disc 1"
    );

    clear_error(&key);
    std::fs::remove_dir_all(&dir).ok();
}

// The collision guard's stat classification: ONLY NotFound means "safe to move". Any other
// stat error must defer to a later tick.
#[cfg(unix)]
#[test]
fn check_and_move_defers_on_non_notfound_dest_stat_error_and_never_clobbers() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("deststat");
    let staging = dir.join("staging");
    let movie_dir = dir.join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    // A good, DIFFERENT library file already at the destination path.
    let dest_dir = movie_dir.join("Vault (2024)");
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest_file = dest_dir.join("Vault (2024).mkv");
    let mut old = vec![0x1A, 0x45, 0xDF, 0xA3];
    old.extend_from_slice(&[0x77u8; 8192]);
    std::fs::write(&dest_file, &old).unwrap();
    std::fs::set_permissions(&dest_file, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Root (some CI sandboxes) reads through mode 000, so the stat would
    // succeed and this scenario can't be produced. Skip rather than assert
    // something the environment isn't exercising.
    if std::fs::File::open(&dest_file).is_ok() {
        std::fs::set_permissions(&dest_file, std::fs::Permissions::from_mode(0o644)).ok();
        std::fs::remove_dir_all(&dir).ok();
        eprintln!(
            "SKIP check_and_move_defers_on_non_notfound_dest_stat_error: running with \
                 read-through privileges, cannot make the dest stat fail with EACCES"
        );
        return;
    }

    // The new rip in staging, routed to that same destination path.
    let disc_dir = staging.join("Vault");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Vault")).unwrap();
    let mut new = vec![0x1A, 0x45, 0xDF, 0xA3];
    new.extend_from_slice(&[0x88u8; 4096]);
    let staged_mkv = disc_dir.join("Vault.mkv");
    std::fs::write(&staged_mkv, &new).unwrap();

    let key = disc_dir.to_string_lossy().to_string();
    let _g = errors_guard();
    clear_error(&key);
    check_and_move(&cfg);
    std::fs::set_permissions(&dest_file, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(
        std::fs::read(&dest_file).unwrap(),
        old,
        "an unstattable destination must NEVER be overwritten — a transient \
             stat error is not proof the path is free"
    );
    assert!(
        staged_mkv.exists() && disc_dir.exists(),
        "the new rip must stay in staging so a later tick can retry"
    );
    let recorded = error_snapshot(&key);
    clear_error(&key);
    assert!(
        recorded.is_some(),
        "the deferred move must be surfaced to the operator"
    );
    drop(_g);
    std::fs::remove_dir_all(&dir).ok();
}

// The same-size content probe must compare a window large enough to be meaningful, or two
// distinct discs get called identical.
#[test]
fn check_and_move_collision_probe_window_is_large_enough_to_see_a_2kb_diff() {
    let dir = scratch_dir("windowprobe");
    let staging = dir.join("staging");
    let movie_dir = dir.join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    let mut old = vec![0x1A, 0x45, 0xDF, 0xA3];
    old.extend_from_slice(&[0x5Au8; 256 * 1024]);
    let mut new = old.clone();
    new[2000] ^= 0xFF; // sole difference, ~2 KiB in

    let dest_dir = movie_dir.join("Twin (2024)");
    std::fs::create_dir_all(&dest_dir).unwrap();
    let dest_file = dest_dir.join("Twin (2024).mkv");
    std::fs::write(&dest_file, &old).unwrap();

    let disc_dir = staging.join("Twin");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Twin")).unwrap();
    let staged_mkv = disc_dir.join("Twin.mkv");
    std::fs::write(&staged_mkv, &new).unwrap();

    let key = disc_dir.to_string_lossy().to_string();
    let _g = errors_guard();
    clear_error(&key);
    check_and_move(&cfg);

    assert_eq!(
        std::fs::read(&dest_file).unwrap(),
        old,
        "the existing library file must survive untouched"
    );
    let second = dest_dir.join("Twin (2024)_2.mkv");
    assert!(
        second.exists(),
        "a same-size DIFFERENT file must be recognised as another disc and \
             filed as `_2` — never swept up as an idempotent re-move"
    );
    assert_eq!(
        std::fs::read(&second).unwrap(),
        new,
        "the `_2` file must carry the second disc's bytes, not a copy of the first"
    );
    let recorded = error_snapshot(&key);
    clear_error(&key);
    assert!(
        recorded.is_none(),
        "filing the second disc is a success, not an operator error"
    );
    let _ = &staged_mkv;
    drop(_g);
    std::fs::remove_dir_all(&dir).ok();
}

// A failed copy that left NOTHING at the destination must not raise a "partial copy could
// not be removed" error.
#[cfg(unix)]
#[test]
fn move_file_copy_failure_with_no_dest_records_no_partial_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("nopartial");
    let src_dir = dir.join("staging");
    let dest_dir = dir.join("library");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    let src = src_dir.join("a.mkv");
    let dest = dest_dir.join("a.mkv");
    write_minimal_mkv(&src, b"source bytes that cannot be read");

    // src unreadable → the copy fails at `File::open(src)`, before any temp
    // exists. src's DIRECTORY unwritable → `rename(2)` fails first (it must
    // unlink the source name), so we reach the copy branch at all.
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let readable_anyway = std::fs::File::open(&src).is_ok();
    let outcome = if readable_anyway {
        MoveOutcome::Failed // placeholder; skipped below
    } else {
        let _g = errors_guard();
        let dest_key = dest.to_string_lossy().to_string();
        clear_error(&dest_key);
        let o = move_file(&src, &dest, &noop_progress);
        let recorded = error_snapshot(&dest_key);
        clear_error(&dest_key);
        assert!(
            recorded.is_none(),
            "a failed copy that created no destination must not claim a \
                 partial copy needs hand-deleting, got: {recorded:?}"
        );
        assert!(!dest.exists(), "no destination may be left behind");
        o
    };

    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
    if readable_anyway {
        eprintln!(
            "SKIP move_file_copy_failure_with_no_dest_records_no_partial_error: \
                 running with read-through privileges"
        );
    } else {
        assert_eq!(outcome, MoveOutcome::Failed, "a failed copy is Failed");
        assert!(src.exists(), "the source must survive a failed move");
    }
    std::fs::remove_dir_all(&dir).ok();
}

// DATA LOSS regression: a failed copy must never delete a destination that PRE-DATES the
// attempt (a legitimate MovedDirty leftover).
#[cfg(unix)]
#[test]
fn move_file_copy_failure_keeps_pre_existing_dest() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("keepdest");
    let src_dir = dir.join("staging");
    let dest_dir = dir.join("library");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    let src = src_dir.join("keepdest.mkv");
    let dest = dest_dir.join("keepdest.mkv");
    write_minimal_mkv(&src, b"staging source bytes that cannot be read");
    // The complete library copy an earlier MovedDirty tick left behind.
    write_minimal_mkv(&dest, b"the complete library copy from an earlier tick");
    let good_bytes = std::fs::read(&dest).unwrap();

    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let readable_anyway = std::fs::File::open(&src).is_ok();
    let outcome = if readable_anyway {
        MoveOutcome::Failed // placeholder; skipped below
    } else {
        let _g = errors_guard();
        let dest_key = dest.to_string_lossy().to_string();
        clear_error(&dest_key);
        let o = move_file(&src, &dest, &noop_progress);
        clear_error(&dest_key);
        assert!(
            dest.exists(),
            "a pre-existing destination must survive a failed copy — it \
                 may be the only good copy of the rip"
        );
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            good_bytes,
            "the pre-existing destination must be left byte-identical"
        );
        o
    };

    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
    if readable_anyway {
        eprintln!(
            "SKIP move_file_copy_failure_keeps_pre_existing_dest: \
                 running with read-through privileges"
        );
    } else {
        assert_eq!(outcome, MoveOutcome::Failed, "a failed copy is Failed");
        assert!(src.exists(), "the source must survive a failed move");
    }
    std::fs::remove_dir_all(&dir).ok();
}

// Complement of the test above: when dest did NOT pre-date the
// attempt, the failure cleanup still runs and leaves nothing at the
// destination name. (Same chmod harness; dest absent at entry.)
#[cfg(unix)]
#[test]
fn move_file_copy_failure_removes_this_attempts_dest() {
    use std::os::unix::fs::PermissionsExt;

    let dir = scratch_dir("dropdest");
    let src_dir = dir.join("staging");
    let dest_dir = dir.join("library");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    let src = src_dir.join("dropdest.mkv");
    let dest = dest_dir.join("dropdest.mkv");
    write_minimal_mkv(&src, b"staging source bytes that cannot be read");
    assert!(!dest.exists(), "dest must not pre-date the attempt");

    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

    let readable_anyway = std::fs::File::open(&src).is_ok();
    let outcome = if readable_anyway {
        MoveOutcome::Failed // placeholder; skipped below
    } else {
        let _g = errors_guard();
        let dest_key = dest.to_string_lossy().to_string();
        clear_error(&dest_key);
        let o = move_file(&src, &dest, &noop_progress);
        let recorded = error_snapshot(&dest_key);
        clear_error(&dest_key);
        assert!(
            !dest.exists(),
            "no output of THIS attempt may be left at the destination name"
        );
        assert!(
            recorded.is_none(),
            "a failed copy that created no destination must not claim a \
                 partial copy needs hand-deleting, got: {recorded:?}"
        );
        o
    };

    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
    if readable_anyway {
        eprintln!(
            "SKIP move_file_copy_failure_removes_this_attempts_dest: \
                 running with read-through privileges"
        );
    } else {
        assert_eq!(outcome, MoveOutcome::Failed, "a failed copy is Failed");
        assert!(src.exists(), "the source must survive a failed move");
    }
    std::fs::remove_dir_all(&dir).ok();
}

// The System-tab clear-one and Clear-all actually remove entries;
// nothing touched MOVE_ERRORS lifecycle before this, so both handlers
// could become no-ops and every test still passed.
#[test]
fn clear_move_error_and_clear_all_remove_entries() {
    let _g = errors_guard();
    let a = "/staging/clear-one";
    let b = "/staging/clear-two";
    record_error(a, "stuck a", "hint");
    record_error(b, "stuck b", "hint");
    assert!(
        error_snapshot(a).is_some() && error_snapshot(b).is_some(),
        "both recorded"
    );

    clear_move_error(a);
    assert!(
        error_snapshot(a).is_none(),
        "the dismissed error must be removed"
    );
    assert!(
        error_snapshot(b).is_some(),
        "clearing one error must not clear the others"
    );

    clear_all_move_errors();
    let left = MOVE_ERRORS
        .lock()
        .map(|m| m.len())
        .unwrap_or_else(|e| e.into_inner().len());
    assert_eq!(left, 0, "Clear all must empty the map");
}

#[test]
fn check_and_move_idempotent_same_size_same_content_cleans_up() {
    // Regression guard: the content-aware collision check must NOT break the idempotent
    // re-move — an identical dest later must be Skipped/Moved and cleaned up. Hold the
    // shared lock like every MOVE_ERRORS test.
    let _g = errors_guard();
    let dir = scratch_dir("idempotent");
    let staging = dir.join("staging");
    let movie_dir = dir.join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    let mut content = vec![0x1A, 0x45, 0xDF, 0xA3];
    content.extend_from_slice(&[0x33u8; 4096]);

    // Dest already present with identical bytes (prior successful copy).
    let dest_dir = movie_dir.join("Echo (2024)");
    std::fs::create_dir_all(&dest_dir).unwrap();
    std::fs::write(dest_dir.join("Echo (2024).mkv"), &content).unwrap();

    // Staging still holds the same file (its unlink failed last tick).
    let disc_dir = staging.join("Echo");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Echo")).unwrap();
    std::fs::write(disc_dir.join("Echo.mkv"), &content).unwrap();

    check_and_move(&cfg);

    // Staging is torn down — the re-move was recognized as idempotent.
    assert!(
        !disc_dir.exists(),
        "idempotent same-content re-move must clean up staging"
    );
    // No collision error recorded.
    let key = disc_dir.to_string_lossy().to_string();
    {
        let m = MOVE_ERRORS.lock().unwrap();
        assert!(
            !m.contains_key(&key),
            "idempotent re-move must not surface a collision error"
        );
    }
    std::fs::remove_dir_all(&dir).ok();
}

// A pass that ends in one of the four failure branches must still clear the move progress
// bar, not leave a stale one forever.
#[test]
fn a_blocked_pass_clears_the_move_progress_bar() {
    // MOVE_STATE and MOVE_ERRORS are process-global; serialize with the
    // other tests that assert on them.
    let _g = errors_guard();
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    let disc = staging.join("MoveBarDisc");
    std::fs::create_dir_all(&disc).unwrap();
    std::fs::write(disc.join(".done"), marker_json("MoveBarDisc")).unwrap();
    let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
    mkv.extend_from_slice(&[0xC7u8; 2048]);
    std::fs::write(disc.join("MoveBarDisc.mkv"), &mkv).unwrap();

    std::fs::create_dir_all(
        movie_dir
            .join("MoveBarDisc (2024)")
            .join("MoveBarDisc (2024).mkv"),
    )
    .unwrap();

    // The bars an earlier dir's copy left published.
    *MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner()) = vec![MoveState {
        name: "MoveBarDisc".to_string(),
        artifact: "mkv".to_string(),
        progress_pct: 60,
        progress_gb: 1.2,
        total_gb: 2.0,
        speed_mbs: 30.0,
        eta: "1:23".to_string(),
    }];

    check_and_move(&cfg);

    let key = disc.to_string_lossy().to_string();
    let recorded = error_snapshot(&key);
    assert!(
        disc.exists(),
        "the staging dir must be left alone when delivery is blocked \
             (otherwise this test is not exercising a failure branch)"
    );
    assert_eq!(
        recorded.map(|e| e.reason),
        Some("copy to destination failed".to_string()),
        "the pass must have ended in the any_failed branch"
    );
    let bar = MOVE_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    // The active-move marker must be cleared too, so the Move queue no
    // longer excludes this dir once the pass has ended.
    let active_cleared = crate::server::mover::ACTIVE_MOVE_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_none();
    clear_error(&key);
    assert!(
        bar,
        "a blocked pass must clear the move progress bars, not leave a \
             stale one on the System page forever"
    );
    assert!(
        active_cleared,
        "a blocked pass must clear ACTIVE_MOVE_DIR so the Move queue stops \
             excluding the dir"
    );
}

// A per-entry listing error must mark the listing INCOMPLETE, not just
// drop the entry, or the caller can remove_dir_all a file it never
// moved.
#[test]
fn a_per_entry_listing_error_marks_the_listing_incomplete() {
    let _g = errors_guard();
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("IncompleteListing");
    std::fs::create_dir_all(&dir).unwrap();
    let readable = dir.join("Movie.mkv");

    // One entry the pass can see, one it cannot (the degraded-NFS
    // DirEntry error this arm exists for).
    let entries = vec![
        Ok(readable.clone()),
        Err(std::io::Error::other("stale NFS file handle")),
    ];

    let (files, complete) = collect_ripped_files(entries, false, &dir);

    clear_error(&dir.to_string_lossy());
    assert_eq!(
        files,
        vec![readable],
        "the entries that DID enumerate must still be planned"
    );
    assert!(
        !complete,
        "a listing that dropped an entry must report itself INCOMPLETE — \
             reporting it complete is what lets the caller tear the staging \
             dir down over a file it never moved"
    );
}

// Upper/mixed-case deliverable extensions must be planned; a case-sensitive compare would
// silently strand them in staging (and the teardown would then delete them).
#[test]
fn collect_ripped_files_matches_extensions_case_insensitively() {
    let dir = Path::new("/staging/CaseDisc");
    let names = [
        "a.MKV",
        "b.Mk3D",
        "c.M2TS",
        "d.ISO",
        "e.iso",
        "f.TXT",
        "g.mkv.part",
        "noext",
    ];
    let entries = || names.iter().map(|n| Ok(dir.join(n)));
    let leafs = |files: Vec<std::path::PathBuf>| -> Vec<String> {
        files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    };

    let (with_iso, complete) = collect_ripped_files(entries(), true, dir);
    assert!(complete);
    assert_eq!(
        leafs(with_iso),
        ["a.MKV", "b.Mk3D", "c.M2TS", "d.ISO", "e.iso"],
        "every deliverable extension must match regardless of case"
    );
    let (without_iso, _) = collect_ripped_files(entries(), false, dir);
    assert_eq!(
        leafs(without_iso),
        ["a.MKV", "b.Mk3D", "c.M2TS"],
        "move_iso=false must drop .iso in any case"
    );
}

// Inducing a real DirEntry error needs a fault-injecting filesystem, so this pins the guard
// wiring at source level instead.
#[test]
fn the_teardown_is_gated_on_a_complete_listing() {
    let src = crate::server::util::source_lf(include_str!("mover.rs"));
    let guard = src
        .find("if !listing_complete {")
        .expect("check_and_move must refuse to tear down an incompletely-listed staging dir");
    let teardown = src
        .find("let cleanup_err = std::fs::remove_dir_all(&dir).err();")
        .expect("the staging teardown must still exist");
    assert!(
        guard < teardown,
        "the incomplete-listing guard must come BEFORE remove_dir_all — \
             after it, the file the pass never saw is already deleted"
    );
    let body = &src[guard..teardown];
    let body = &body[..body.find("\n        }\n").expect("guard block closes")];
    assert!(
        body.contains("continue;"),
        "the incomplete-listing guard must skip the teardown, not just log"
    );
}

// MOVE_ERRORS rows for a staging dir the operator removed by hand must be pruned — the only
// clear_error call site needs a later pass over that same dir.
#[test]
fn move_errors_for_a_vanished_staging_dir_are_pruned() {
    let _g = errors_guard();
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    // Gone: the dir the operator `rm -rf`'d after reading the hint.
    let gone = staging.join("PruneGhostDisc").to_string_lossy().to_string();
    // Alive: still in staging, no `.done` yet — the pass sees it and
    // skips it, and its row must survive.
    let alive_dir = staging.join("PruneLiveDisc");
    std::fs::create_dir_all(&alive_dir).unwrap();
    let alive = alive_dir.to_string_lossy().to_string();
    // Outside the staging root: the destination-keyed row. Different
    // mount, different liveness story — not ours to prune.
    let outside = movie_dir
        .join("PruneOutside.mkv")
        .to_string_lossy()
        .to_string();

    record_error(&gone, "staging cleanup failed: whatever", "rm -rf it");
    record_error(&alive, "copy to destination failed", "see the log");
    record_error(&outside, "partial copy could not be removed", "delete it");

    check_and_move(&cfg);

    let (g, a, o) = (
        error_snapshot(&gone),
        error_snapshot(&alive),
        error_snapshot(&outside),
    );
    clear_error(&gone);
    clear_error(&alive);
    clear_error(&outside);

    assert!(
        g.is_none(),
        "the row for a staging dir that no longer exists must be pruned"
    );
    assert!(
        a.is_some(),
        "a staging dir that is still present keeps its row, even when \
             this pass skipped it as not-ready"
    );
    assert!(
        o.is_some(),
        "a row keyed outside the staging root must not be pruned by the \
             staging scan"
    );
}

// EXHAUSTIVE mover decider matrix (rc4 hardening): the mover is the third staging-state
// decider, keying ONLY on .done — it either moves the muxed output and tears the dir down,
// or leaves it alone.

/// Outcome of one mover decision, observed from the filesystem.
#[derive(Debug, PartialEq)]
enum MoverVerdict {
    /// Output landed in the library and staging was torn down.
    MovedAndCleaned,
    /// Staging dir left in place, nothing moved to the library.
    LeftAlone,
    /// Output landed in the library but staging was kept.
    MovedNotCleaned,
    /// Staging torn down with nothing in the library.
    CleanedNotMoved,
}

// Build a single staging disc dir, run the real check_and_move, and report whether the MKV
// reached the library and staging was cleaned.
fn mover_verdict(done_body: Option<&[u8]>, with_mkv: bool, extra: &[&str]) -> MoverVerdict {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);

    let disc = staging.join("Disc");
    std::fs::create_dir_all(&disc).unwrap();
    if let Some(body) = done_body {
        std::fs::write(disc.join(".done"), body).unwrap();
    }
    if with_mkv {
        let mut mkv = vec![0x1A, 0x45, 0xDF, 0xA3];
        mkv.extend_from_slice(&[0xAAu8; 1024]);
        std::fs::write(disc.join("Disc.mkv"), &mkv).unwrap();
    }
    for e in extra {
        std::fs::write(disc.join(e), b"x").unwrap();
    }

    // Clear any stale error for this dir before/after so the shared
    // MOVE_ERRORS map doesn't leak across rows.
    let key = disc.to_string_lossy().to_string();
    clear_error(&key);
    check_and_move(&cfg);
    clear_error(&key);

    let moved = movie_dir.join("Disc.mkv").exists()
            || movie_dir
                .join("Disc")
                .join("Disc.mkv")
                .exists()
            // marker has title "Disc" → movie path is "Disc/Disc.mkv" with no year,
            // but marker_json sets year 2024 → "Disc (2024)/Disc (2024).mkv".
            || movie_dir.join("Disc (2024)/Disc (2024).mkv").exists();
    let cleaned = !disc.exists();
    match (moved, cleaned) {
        (true, true) => MoverVerdict::MovedAndCleaned,
        (false, false) => MoverVerdict::LeftAlone,
        (true, false) => MoverVerdict::MovedNotCleaned,
        (false, true) => MoverVerdict::CleanedNotMoved,
    }
}

#[test]
fn mover_decider_matrix() {
    let valid = marker_json("Disc");
    let valid_b = valid.as_bytes();

    // --- no .done marker: mover never acts ---
    assert_eq!(
        mover_verdict(None, true, &[]),
        MoverVerdict::LeftAlone,
        "no .done marker → mover must not move (it keys solely on .done)"
    );
    // .completed / .ripped without .done are not the mover's hand-off.
    assert_eq!(
        mover_verdict(None, true, &[".completed"]),
        MoverVerdict::LeftAlone,
        ".completed without .done is not the mover's signal"
    );
    assert_eq!(
        mover_verdict(None, true, &[".ripped"]),
        MoverVerdict::LeftAlone,
        ".ripped without .done is the mux worker's signal, not the mover's"
    );

    // --- valid .done + movable output → move + clean ---
    assert_eq!(
        mover_verdict(Some(valid_b), true, &[]),
        MoverVerdict::MovedAndCleaned,
        "valid .done + MKV → move to library and tear down staging"
    );

    // --- valid .done but NO movable output → left alone ---
    assert_eq!(
        mover_verdict(Some(valid_b), false, &[]),
        MoverVerdict::LeftAlone,
        "valid .done but no .mkv/.m2ts to move → skip (nothing to promote)"
    );

    // --- torn / empty .done → not-ready, skip (never blind-move) ---
    assert_eq!(
        mover_verdict(Some(b""), true, &[]),
        MoverVerdict::LeftAlone,
        "empty .done (torn write) → not ready, skip"
    );
    assert_eq!(
        mover_verdict(Some(b"{ this is not json"), true, &[]),
        MoverVerdict::LeftAlone,
        "unparseable .done → not ready, skip"
    );

    // --- parseable .done with empty title AND disc_name → skip ---
    let empty_title = serde_json::json!({ "title": "", "disc_name": "" }).to_string();
    assert_eq!(
        mover_verdict(Some(empty_title.as_bytes()), true, &[]),
        MoverVerdict::LeftAlone,
        "parseable .done with empty title+disc_name → no usable dest name, skip"
    );

    // --- valid .done coexisting with .completed → still moves
    //     (.completed does not block the mover; the mux worker's terminal
    //     guard is separate). The mover only needs .done + output. ---
    assert_eq!(
        mover_verdict(Some(valid_b), true, &[".completed"]),
        MoverVerdict::MovedAndCleaned,
        "valid .done + .completed + MKV → mover still files it"
    );
}

#[test]
fn done_absence_in_progress_vs_fault() {
    use std::io::ErrorKind;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("disc");
    std::fs::create_dir_all(&dir).unwrap();

    // Bare dir, .done NotFound, no governing marker → stranded → Fault (WARN).
    assert_eq!(
        classify_done_absence(ErrorKind::NotFound, &dir),
        DoneAbsence::Fault,
        "NotFound with no governing marker is a stranded dir → WARN"
    );

    // A non-NotFound read error is always a fault, even mid-rip.
    assert_eq!(
        classify_done_absence(ErrorKind::PermissionDenied, &dir),
        DoneAbsence::Fault,
        "EACCES/ESTALE etc. → WARN regardless of governing marker"
    );

    // Each governing marker turns a .done NotFound into the by-design in-progress state
    // (quiet skip, no WARN) — this is the 182-warn bug. `.sweeping` is the load-bearing
    // addition; it had no marker before.
    for m in [
        ".sweeping",
        ".muxing",
        ".ripped",
        ".completed",
        ".failed",
        ".review",
    ] {
        let governed = tmp.path().join(format!("disc{m}"));
        std::fs::create_dir_all(&governed).unwrap();
        std::fs::write(governed.join(m), b"x").unwrap();
        assert_eq!(
            classify_done_absence(ErrorKind::NotFound, &governed),
            DoneAbsence::InProgress,
            "NotFound while {m} present is the in-progress state → no WARN"
        );
        // ...but a non-NotFound error on the same dir is still a fault.
        assert_eq!(
            classify_done_absence(ErrorKind::Other, &governed),
            DoneAbsence::Fault,
            "non-NotFound error is a fault even with {m} present"
        );
    }
}

// Convergence round 4 (M3): the governed-marker probe must route through
// snapshot_staging_disc, not bare exists(), so a cold-cache mount can't
// false-negative.sweeping.
#[test]
fn done_absence_sweeping_governed_via_snapshot() {
    use std::io::ErrorKind;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("disc-sweeping");
    std::fs::create_dir_all(&dir).unwrap();
    crate::server::ripper::staging::write_sweeping_marker(&dir);
    // The same snapshot the governed check now consults sees the marker.
    let snap = crate::server::ripper::staging::snapshot_staging_disc(&dir).expect("snapshot");
    assert!(snap.has_sweeping);
    assert_eq!(
        classify_done_absence(ErrorKind::NotFound, &dir),
        DoneAbsence::InProgress,
        "a durably-present .sweeping marker is the in-progress state → no WARN"
    );
}

// Regression: a staging dir that vanished between the.done read and the governing-marker
// probe must be InProgress, not a stranded-dir Fault.
#[test]
fn done_absence_vanished_dir_is_in_progress_not_fault() {
    use std::io::ErrorKind;
    let tmp = tempfile::tempdir().unwrap();
    // Path under the temp root that was never created (or already removed).
    let gone = tmp.path().join("disc-removed");
    assert!(!gone.exists());
    assert_eq!(
        classify_done_absence(ErrorKind::NotFound, &gone),
        DoneAbsence::InProgress,
        "a dir that disappeared out from under the mover is a lifecycle \
             transition, not a stranded-dir fault → no WARN"
    );
    // A non-NotFound error on a missing dir is still surfaced (the read
    // failure itself, not the absence, is what we report).
    assert_eq!(
        classify_done_absence(ErrorKind::PermissionDenied, &gone),
        DoneAbsence::Fault,
        "non-NotFound errors stay faults even when the dir is gone"
    );
}

// Precedence guard for the TOCTOU fix: the vanished-dir check runs BEFORE the
// governing-marker probe, so a SIBLING dir's marker cannot leak into a vanished dir's
// classification.
#[test]
fn done_absence_vanished_dir_ignores_sibling_markers() {
    use std::io::ErrorKind;
    let tmp = tempfile::tempdir().unwrap();

    // A live sibling dir that DOES carry a governing marker.
    let sibling = tmp.path().join("disc-live");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join(".ripped"), b"x").unwrap();

    // The dir we classify never existed — its marker join would resolve
    // under it, not under the sibling.
    let gone = tmp.path().join("disc-gone");
    assert!(!gone.exists());

    assert_eq!(
        classify_done_absence(ErrorKind::NotFound, &gone),
        DoneAbsence::InProgress,
        "a vanished dir is InProgress on its own merits, independent of any \
             sibling's markers"
    );
    // The sibling's classification is independent and unaffected: present
    // dir + marker → InProgress.
    assert_eq!(
        classify_done_absence(ErrorKind::NotFound, &sibling),
        DoneAbsence::InProgress
    );
}

// A dir that EXISTS but carries no governing marker is a genuine stranded Fault — the
// vanished-dir early-return must NOT swallow it.
#[test]
fn done_absence_present_dir_without_marker_is_fault() {
    use std::io::ErrorKind;
    let dir = scratch_dir("strandedfault");
    assert!(dir.exists(), "dir is present");
    // No .ripped/.completed/.failed/.review marker written.
    assert_eq!(
        classify_done_absence(ErrorKind::NotFound, &dir),
        DoneAbsence::Fault,
        "a present dir with no governing marker is genuinely stranded → WARN"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn same_head_and_tail_distinguishes_identical_from_different() {
    let dir = scratch_dir("headtail");
    let a = dir.join("a.bin");
    let b = dir.join("b.bin");
    let c = dir.join("c.bin");
    let base = vec![0x5Au8; 200 * 1024]; // larger than the 64 KiB window
    std::fs::write(&a, &base).unwrap();
    std::fs::write(&b, &base).unwrap();
    // c: same length, differs only in the middle (outside both windows)
    // — head+tail treats it as identical (acceptable: an interior-only
    // real collision is unrealistic, and a full compare every tick isn't).
    let mut mid = base.clone();
    let m = mid.len() / 2;
    mid[m] ^= 0xFF;
    std::fs::write(&c, &mid).unwrap();
    assert!(same_head_and_tail(&a, &b), "identical files match");
    assert!(same_head_and_tail(&a, &c), "interior-only diff matches");

    // e: same length, differs ~2 KiB in — INSIDE the 64 KiB head window,
    // where the window is load-bearing: a much smaller window would wave
    // this real collision through as an idempotent re-move.
    let e = dir.join("e.bin");
    let mut neardiff = base.clone();
    neardiff[2000] ^= 0xFF;
    std::fs::write(&e, &neardiff).unwrap();
    assert!(
        !same_head_and_tail(&a, &e),
        "a difference 2 KiB in must fall inside the head window"
    );

    // d: differs at the head → not identical.
    let d = dir.join("d.bin");
    let mut headdiff = base.clone();
    headdiff[0] ^= 0xFF;
    std::fs::write(&d, &headdiff).unwrap();
    assert!(!same_head_and_tail(&a, &d), "head diff must not match");
    std::fs::remove_dir_all(&dir).ok();
}

// Fail-loud destination validation (Mercy incident hardening): the mover must ERROR +
// preserve-in-staging when the configured root is missing/unwritable, never silently
// create it (writing into the container overlay), and must log FULL ABSOLUTE paths.

/// dest root MISSING → error, and the validation must NOT create it
/// (no silent `create_dir_all` of a dead mount point).
#[test]
fn validate_destination_root_errors_when_missing_and_does_not_create() {
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("movies-mount-gone");
    let missing_str = missing.to_string_lossy().to_string();
    // Precondition: it really is absent.
    assert!(!missing.exists());

    let res = validate_destination_root(&missing_str);
    assert!(res.is_err(), "a missing destination root must be an error");
    let reason = res.unwrap_err();
    assert!(
        reason.contains("does not exist"),
        "error must explain the root is missing, got: {reason}"
    );
    // THE KEY GUARANTEE: validation did not auto-create the root. A real
    // move would then preserve the output in staging, not write 80 GB
    // into a fresh overlay dir.
    assert!(
        !missing.exists(),
        "validate_destination_root must NOT create the missing root (no silent create)"
    );
}

/// dest root PRESENT + WRITABLE → Ok, and the writability probe leaves
/// no marker file behind.
#[test]
fn validate_destination_root_ok_when_present_and_writable() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_string_lossy().to_string();
    assert!(validate_destination_root(&root).is_ok());
    // The probe file must be cleaned up.
    let leftover: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(".autorip-writable-probe")
        })
        .collect();
    assert!(leftover.is_empty(), "writability probe must be removed");
}

/// A RELATIVE root is rejected — the exact shape that produced the
/// incident's "Moved to movies/Mercy/..." cwd-relative write.
#[test]
fn validate_destination_root_rejects_relative_path() {
    let res = validate_destination_root("movies");
    assert!(res.is_err(), "a relative root must be rejected");
    assert!(res.unwrap_err().contains("absolute"));
}

/// An empty root is rejected (would `create_dir_all("")` → cwd writes).
#[test]
fn validate_destination_root_rejects_empty() {
    assert!(validate_destination_root("").is_err());
}

/// A root that exists but is a FILE (not a directory) is rejected.
#[test]
fn validate_destination_root_rejects_non_directory() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file = tmp.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let res = validate_destination_root(&file.to_string_lossy());
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("not a directory"));
}

/// A present-but-read-only root is rejected by the writability probe.
#[cfg(unix)]
#[test]
fn validate_destination_root_rejects_read_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::TempDir::new().unwrap();
    let ro = tmp.path().join("ro-root");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
    // Root runs as uid 0 in some CI sandboxes and can write through 0o500.
    let writable_anyway = std::fs::File::create(ro.join("root-check")).is_ok();
    let res = validate_destination_root(&ro.to_string_lossy());
    // Restore perms so TempDir cleanup works regardless of the outcome.
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).ok();
    if writable_anyway {
        eprintln!("SKIP validate_destination_root_rejects_read_only: running as root");
        return;
    }
    let reason = res.expect_err("a read-only root must be rejected");
    assert!(
        reason.contains("not writable"),
        "read-only root must fail with a writability reason, got: {reason}"
    );
}

/// `destination_root` selects the SAME root `build_destination` routes
/// to, for every media-type / configured-dir combination — so the
/// validation guards exactly the root the move will use.
#[test]
fn destination_root_matches_build_destination_root() {
    let cfg = cfg_with_dirs("/mnt/movies", "/mnt/tv", "/mnt/out");
    // movie with movie_dir set → movie_dir
    assert_eq!(
        destination_root(&cfg, &Some(tmdb_movie("X", 2024))),
        "/mnt/movies"
    );
    // tv with tv_dir set → tv_dir
    let tv = tmdb::TmdbResult {
        title: "Y".into(),
        year: 2024,
        poster_url: String::new(),
        overview: String::new(),
        media_type: "tv".into(),
        tmdb_id: 0,
    };
    assert_eq!(destination_root(&cfg, &Some(tv)), "/mnt/tv");
    // no tmdb → output_dir
    assert_eq!(destination_root(&cfg, &None), "/mnt/out");
    // movie but movie_dir empty → output_dir (matches build_destination
    // fall-through)
    let cfg2 = cfg_with_dirs("", "/mnt/tv", "/mnt/out");
    assert_eq!(
        destination_root(&cfg2, &Some(tmdb_movie("X", 2024))),
        "/mnt/out"
    );
}

// absolute_for_log never yields a cwd-relative path. POSIX-only for
// the pass-through case; absolute_for_log_is_always_absolute_natively
// asserts the invariant everywhere.
#[cfg(unix)]
#[test]
fn absolute_for_log_is_always_absolute() {
    assert_eq!(
        absolute_for_log("/mnt/media/movies/Mercy (2024)/Mercy (2024).mkv"),
        "/mnt/media/movies/Mercy (2024)/Mercy (2024).mkv"
    );
    let rel = absolute_for_log("movies/Mercy/Mercy.mkv");
    assert!(
        std::path::Path::new(&rel).is_absolute(),
        "a relative dest must be rendered as an absolute path for logging, got: {rel}"
    );
    assert!(rel.ends_with("movies/Mercy/Mercy.mkv"));
}

/// `check_configured_destinations` reports each broken root once and
/// stays silent on good/empty roots.
#[test]
fn check_configured_destinations_reports_missing_roots() {
    let tmp = tempfile::TempDir::new().unwrap();
    let good = tmp.path().join("good");
    std::fs::create_dir(&good).unwrap();
    let missing = tmp.path().join("missing");

    // movie_dir missing, tv_dir empty (skipped), output_dir good.
    let cfg = cfg_with_dirs(&missing.to_string_lossy(), "", &good.to_string_lossy());
    let problems = check_configured_destinations(&cfg);
    assert_eq!(
        problems.len(),
        1,
        "only the missing movie_dir should be flagged"
    );
    assert_eq!(problems[0].0, missing.to_string_lossy().replace('\\', "/"));

    // All-good config → no problems.
    let cfg_ok = cfg_with_dirs(&good.to_string_lossy(), "", &good.to_string_lossy());
    assert!(
        check_configured_destinations(&cfg_ok).is_empty(),
        "a fully-present config must report no problems"
    );
}

/// Dedup: when movie_dir == output_dir and both are missing, the
/// operator sees ONE warning, not two.
#[test]
fn check_configured_destinations_deduplicates_identical_roots() {
    let tmp = tempfile::TempDir::new().unwrap();
    let missing = tmp.path().join("same-missing-root");
    let m = missing.to_string_lossy().to_string();
    let cfg = cfg_with_dirs(&m, "", &m); // movie_dir == output_dir
    let problems = check_configured_destinations(&cfg);
    assert_eq!(
        problems.len(),
        1,
        "an identical movie/output root must be reported once, got {problems:?}"
    );
}

#[test]
fn webhook_output_path_is_the_media_file_whatever_the_order() {
    let p = |d: &str| (std::path::PathBuf::from("/s/x"), d.to_string());
    let mkv_iso = [p("/m/T (2000)/T (2000).mkv"), p("/i/T (2000).iso")];
    let iso_mkv = [p("/i/T (2000).iso"), p("/m/T (2000)/T (2000).mkv")];
    assert_eq!(webhook_output_path(&mkv_iso), "/m/T (2000)/T (2000).mkv");
    assert_eq!(webhook_output_path(&iso_mkv), "/m/T (2000)/T (2000).mkv");
    let eps = [p("/tv/S/S S01E02.mkv"), p("/tv/S/S S01E01.mkv")];
    assert_eq!(webhook_output_path(&eps), "/tv/S/S S01E01.mkv");
    assert_eq!(webhook_output_path(&[p("/i/T.iso")]), "/i/T.iso");
    assert_eq!(webhook_output_path(&[]), "");
}

#[test]
fn check_and_move_card_carries_the_create_dir_error() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    let movie_dir = tmp.path().join("output/Movies");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::create_dir_all(&movie_dir).unwrap();
    let cfg = cfg_for_staging(&staging, &movie_dir.to_string_lossy(), false);
    // A FILE where the title folder must go.
    std::fs::write(movie_dir.join("Blocked (2024)"), b"x").unwrap();

    let disc_dir = staging.join("Blocked");
    std::fs::create_dir_all(&disc_dir).unwrap();
    std::fs::write(disc_dir.join(".done"), marker_json("Blocked")).unwrap();
    write_minimal_mkv(&disc_dir.join("Blocked.mkv"), &[0xAA; 1024]);

    let dir_str = disc_dir.to_string_lossy().to_string();
    let _g = errors_guard();
    clear_error(&dir_str);
    check_and_move(&cfg);
    let recorded = error_snapshot(&dir_str);
    clear_error(&dir_str);

    let reason = recorded.expect("a card must be recorded").reason;
    let without_cause = format!(
        "cannot create destination directory {}",
        movie_dir.join("Blocked (2024)").display()
    );
    assert!(
        reason.len() > without_cause.len() + 2 && reason.starts_with(&format!("{without_cause}: ")),
        "the card must carry the OS error, got: {reason}"
    );
    assert!(disc_dir.join("Blocked.mkv").exists(), "staging preserved");
}

// A copy that fails post-copy validation must not leave its broken file at
// the library name (the next tick's variant search would file around it).
#[cfg(unix)]
#[test]
fn move_file_removes_its_own_copy_that_fails_validation() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch_dir("badcopy");
    let src_dir = dir.join("staging");
    let dest_dir = dir.join("library");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dest_dir).unwrap();
    let src = src_dir.join("bad.mkv");
    let dest = dest_dir.join("bad.mkv");
    std::fs::write(&src, b"not an EBML file at all").unwrap();
    // Read-only staging dir: rename(2) can't unlink src, so the copy path runs.
    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let renames_anyway = std::fs::File::create(src_dir.join("root-check")).is_ok();
    let outcome = (!renames_anyway).then(|| move_file(&src, &dest, &noop_progress));
    std::fs::set_permissions(&src_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let Some(outcome) = outcome else {
        eprintln!("SKIP move_file_removes_its_own_copy_that_fails_validation: running as root");
        return;
    };

    assert_eq!(outcome, MoveOutcome::PostCopyInvalid);
    assert!(!dest.exists(), "the broken copy must be removed");
    assert!(src.exists(), "the source is the source of truth");
    std::fs::remove_dir_all(&dir).ok();
}

/// A destination root on a mount that stopped answering (the SMB share that hung the
/// macOS Library service at startup): the check gives up on it within its limit and
/// reports it not responding, while a healthy root still answers. Roots on one dead
/// mount are checked at once, so two of them cost one timeout, not two.
#[test]
fn check_configured_destinations_gives_up_on_a_hung_root() {
    let base = format!("/test/hung-destination-{}", std::process::id());
    let (movies, tv, output) = (
        format!("{base}/movies"),
        format!("{base}/tv"),
        format!("{base}/output"),
    );
    let cfg = cfg_with_dirs(&movies, &tv, &output);
    let hung = [movies.clone(), tv.clone()];
    let limit = std::time::Duration::from_millis(400);
    let started = std::time::Instant::now();
    let problems = check_destinations_with(&cfg, limit, move |root: &str| {
        if hung.iter().any(|h| h == root) {
            // A stat that never comes back (until long after the limit).
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        Ok(())
    });
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_millis(800),
        "two hung roots must cost one timeout, took {took:?}"
    );
    let roots: Vec<&str> = problems.iter().map(|(r, _)| r.as_str()).collect();
    assert_eq!(roots, [movies.as_str(), tv.as_str()], "{problems:?}");
    for (_, reason) in &problems {
        assert!(reason.contains("not responding"), "{reason}");
    }
}

// The real check still runs through the bounded path: a broken root keeps its own reason.
#[test]
fn check_configured_destinations_keeps_the_roots_own_reason() {
    let problems = check_destinations_with(
        &cfg_with_dirs("", "", "relative/output"),
        std::time::Duration::from_secs(5),
        validate_destination_root,
    );
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].1.contains("not an absolute path"),
        "{problems:?}"
    );
}
