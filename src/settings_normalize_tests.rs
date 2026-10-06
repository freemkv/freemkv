use super::{
    LoadOutcome, Settings, default_keydb_path, dirs_movies, settings_path, shellexpand, support_dir,
};

/// A scratch directory of this test's own, so nothing here touches the
/// real per-user support dir (and no `HOME` juggling is needed —
/// `load_from` takes the path).
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fmkv-load-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// The three cases `load()` must tell apart; they used to collapse into
// one `unwrap_or_default()` path, hiding discarded settings from the user.
#[test]
fn load_distinguishes_no_file_from_a_good_file_from_an_unreadable_one() {
    let dir = scratch("outcome");

    // 1. No file: the normal first run. Defaults, and NOT reported.
    let missing = dir.join("gui-settings.json");
    let (s, outcome) = Settings::load_from(&missing);
    assert_eq!(outcome, LoadOutcome::Missing);
    assert_eq!(s.keyserver_url, Settings::default().keyserver_url);
    assert!(
        !missing.with_extension("json.bad").exists(),
        "a first run must not leave a .bad file"
    );

    // 2. A good file: parsed, values kept.
    let good = dir.join("good.json");
    std::fs::write(&good, r#"{"keyserver_token":"tok"}"#).unwrap();
    let (s, outcome) = Settings::load_from(&good);
    assert_eq!(outcome, LoadOutcome::Loaded);
    assert_eq!(s.keyserver_token, "tok");
    assert!(
        good.exists(),
        "a good file must be left exactly where it is"
    );

    // 3. Present but unparseable: reported, with the error and the path
    // the original was preserved at.
    let bad = dir.join("bad.json");
    std::fs::write(&bad, "{ not json").unwrap();
    let (s, outcome) = Settings::load_from(&bad);
    assert_eq!(s.keyserver_token, "", "must fall back to defaults");
    match outcome {
        LoadOutcome::Unreadable {
            path,
            error,
            preserved,
        } => {
            assert_eq!(path, bad);
            assert!(!error.is_empty(), "the parse error must be reported");
            let kept = preserved.expect("the original must be preserved");
            assert_eq!(std::fs::read_to_string(kept).unwrap(), "{ not json");
            assert!(!bad.exists(), "the unusable file must be moved aside");
        }
        other => panic!("expected Unreadable, got {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// A file that is not valid UTF-8 is still the user's settings: it must be moved aside,
// not left for the next `save()` to overwrite with defaults.
#[test]
fn a_non_utf8_settings_file_is_preserved_not_overwritten() {
    let dir = scratch("nonutf8");
    let path = dir.join("gui-settings.json");
    std::fs::write(&path, b"{\"dest_dir\":\"C:\\Users\\Jos\xe9\"}").unwrap();
    let (_, outcome) = Settings::load_from(&path);
    match outcome {
        LoadOutcome::Unreadable { preserved, .. } => {
            assert!(preserved.is_some(), "the file must be moved aside");
            assert!(!path.exists());
        }
        other => panic!("expected Unreadable, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// Preserving must never overwrite an already-preserved copy: the first
// `.bad` likely holds the real token; a second corruption is probably
// just the defaults written after the first, and must not replace it.
#[test]
fn a_second_corruption_does_not_clobber_the_first_preserved_copy() {
    let dir = scratch("twice");
    let path = dir.join("gui-settings.json");

    std::fs::write(&path, "first — holds the real token").unwrap();
    let (_, first) = Settings::load_from(&path);
    std::fs::write(&path, "second — only defaults were in here").unwrap();
    let (_, second) = Settings::load_from(&path);

    let (
        LoadOutcome::Unreadable {
            preserved: Some(a), ..
        },
        LoadOutcome::Unreadable {
            preserved: Some(b), ..
        },
    ) = (first, second)
    else {
        panic!("both loads should have preserved the file")
    };
    assert_ne!(a, b, "the second corruption reused the first .bad name");
    assert_eq!(
        std::fs::read_to_string(&a).unwrap(),
        "first — holds the real token",
        "the first preserved copy was overwritten"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// Running out of `.bad` slots must not leave the corrupt file where the next `save()`
// overwrites it with defaults.
#[test]
fn a_full_set_of_bad_slots_still_moves_the_corrupt_file_out_of_harms_way() {
    let dir = scratch("overflow");
    let path = dir.join("gui-settings.json");

    // Every numbered slot `preserve_unreadable` knows: `.json.bad`, then
    // `.json.bad.2` through `.json.bad.9`.
    std::fs::write(path.with_extension("json.bad"), "kept").unwrap();
    for n in 2..10 {
        std::fs::write(path.with_extension(format!("json.bad.{n}")), "kept").unwrap();
    }

    std::fs::write(&path, "{ not json — but it still holds the token").unwrap();
    let (_, outcome) = Settings::load_from(&path);

    let LoadOutcome::Unreadable {
        preserved: Some(kept),
        ..
    } = outcome
    else {
        panic!("the file must still be preserved when the slots are full")
    };
    assert!(
        !path.exists(),
        "the corrupt file was left at the live path, where the next save() \
             overwrites it with defaults"
    );
    assert_eq!(
        std::fs::read_to_string(&kept).unwrap(),
        "{ not json — but it still holds the token"
    );
    assert_eq!(
        std::fs::read_to_string(path.with_extension("json.bad")).unwrap(),
        "kept",
        "the FIRST preserved copy is the valuable one and must survive"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// `normalize()` was previously untested via `load()`, leaving it able to
// be replaced with `()`. A stale enum string from an older build must
// snap back to default, not leave the popup control rendering blank.
#[test]
fn normalize_snaps_every_stale_enum_back_to_its_default() {
    let d = Settings::default();
    let mut s = Settings {
        selection: "bogus".into(),
        rip_mode: "bogus".into(),
        key_source: "bogus".into(),
        log_level: "bogus".into(),
        container: "Matroska (.mkv)".into(), // a real pre-1.5 value
        ..Settings::default()
    };
    s.normalize();
    assert_eq!(s.selection, d.selection);
    assert_eq!(s.rip_mode, d.rip_mode);
    assert_eq!(s.key_source, d.key_source);
    assert_eq!(s.log_level, d.log_level);
    assert_eq!(s.container, d.container);
}

/// A valid value must survive normalization untouched — otherwise the snap
/// is not a repair, it is a reset of the user's choices on every load.
#[test]
fn normalize_leaves_every_recognized_value_alone() {
    let mut s = Settings {
        selection: "All titles".into(),
        rip_mode: "Single pass".into(),
        key_source: "keydb, then online".into(),
        log_level: "Debug".into(),
        ..Settings::default()
    };
    s.normalize();
    assert_eq!(s.selection, "All titles");
    assert_eq!(s.rip_mode, "Single pass");
    assert_eq!(s.key_source, "keydb, then online");
    assert_eq!(s.log_level, "Debug");
}

// A relative destination cannot be written to and renders blank; this
// catches an empty base dir — the unset-`HOME` bug where
// `default_dest_dir()` came back as `"Movies"` and rips landed at CWD.
#[test]
fn a_relative_destination_falls_back_and_an_absolute_one_does_not() {
    let d = Settings::default();

    for bad in ["", "   ", "Movies", "../out"] {
        let mut s = Settings {
            dest_dir: bad.into(),
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.dest_dir, d.dest_dir, "{bad:?} should have fallen back");
    }

    let custom = if cfg!(windows) {
        r"C:\custom\output"
    } else {
        "/custom/output"
    };
    let mut s = Settings {
        dest_dir: custom.into(),
        ..Settings::default()
    };
    s.normalize();
    assert_eq!(s.dest_dir, custom, "an absolute folder must be preserved");
}

// `~/rips/out` is a path the user meant, not a placeholder to throw away.
#[test]
fn a_tilde_destination_is_expanded_not_reset() {
    let mut s = Settings {
        dest_dir: "~/rips/out".into(),
        ..Settings::default()
    };
    s.normalize();
    assert_eq!(s.dest_dir, shellexpand("~/rips/out"));
    assert!(s.dest_dir.ends_with("out") && !s.dest_dir.starts_with('~'));
}

/// A blank keydb location is never left blank: the default is a real path
/// in the support directory, and an empty one means the Keys tab points at
/// nothing while still reporting a configured local source.
#[test]
fn a_blank_keydb_path_falls_back_to_the_default_location() {
    let d = Settings::default();
    for blank in ["", "  ", "\t"] {
        let mut s = Settings {
            keydb_path: blank.into(),
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.keydb_path, d.keydb_path);
    }
    let mut kept = Settings {
        keydb_path: "~/keys/keydb.cfg".into(),
        ..Settings::default()
    };
    kept.normalize();
    assert_eq!(kept.keydb_path, "~/keys/keydb.cfg");
}

/// An upgraded settings file must not silently lose the notification:
/// the absent field comes from `Settings::default()`, not bool's `false`.
#[test]
fn a_settings_file_from_before_notifications_still_opts_in_after_upgrade() {
    let s: Settings = serde_json::from_str("{}").expect("empty JSON must parse");
    assert!(
        s.notify_when_rip_finished,
        "missing field must default to on, not off"
    );

    let default = Settings::default();
    assert!(default.notify_when_rip_finished);

    // An explicit `false` must survive load — the opt-out must actually opt out.
    let s: Settings = serde_json::from_str(r#"{"notify_when_rip_finished":false}"#).unwrap();
    assert!(!s.notify_when_rip_finished);
}

// The four `settings::*` path wrappers forward to `platform::*` but are
// separate functions from what `platform.rs`'s own tests exercise, so
// each could regress to `Default::default()` (e.g. `settings_path()` == "").
#[test]
fn the_derived_paths_are_absolute_and_distinct() {
    let support = support_dir();
    assert!(support.is_absolute(), "support_dir: {support:?}");
    assert!(support.ends_with("freemkv"), "support_dir: {support:?}");

    let settings = settings_path();
    assert!(settings.is_absolute(), "settings_path: {settings:?}");
    assert!(settings.ends_with("gui-settings.json"));
    assert!(settings.starts_with(&support));

    let keydb = default_keydb_path();
    assert!(std::path::Path::new(&keydb).is_absolute(), "keydb: {keydb}");
    assert!(keydb.ends_with("keydb.cfg"));

    let movies = dirs_movies();
    assert!(
        std::path::Path::new(&movies).is_absolute(),
        "movies: {movies}"
    );
    assert!(!movies.is_empty());
    // The output folder is not the app's own state directory.
    assert_ne!(movies, support.to_string_lossy());
}

// Every key `get`/`set` name must round-trip, including ones the external
// suite's key lists omit (`log_level`, `raw`, `force`) — those match arms
// were dead: `cli_parity_flags_persist` reads the struct, not the accessor.
#[test]
fn every_accessor_key_round_trips_including_the_ones_the_suite_missed() {
    let mut s = Settings::default();
    for key in [
        "dest_dir",
        "container",
        "filename_template",
        "selection",
        "min_title_secs",
        "audio_langs",
        "sub_langs",
        "forced_sub_langs",
        "rip_mode",
        "max_passes",
        "abort_lost_secs",
        "key_source",
        "keydb_path",
        "keydb_url",
        "keyserver_url",
        "keyserver_token",
        "language",
        "decrypt_threads",
        "log_level",
    ] {
        s.set(key, format!("value-for-{key}"));
        assert_eq!(
            s.get(key),
            format!("value-for-{key}"),
            "{key} did not round-trip through get/set"
        );
    }
    // An unknown key is inert in both directions, never a panic.
    s.set("not_a_key", "x".into());
    assert_eq!(s.get("not_a_key"), "");

    for key in [
        "keep_iso",
        "auto_eject",
        "notify_when_rip_finished",
        "raw",
        "force",
    ] {
        s.set_bool(key, true);
        assert!(s.get_bool(key), "{key} did not round-trip as true");
        s.set_bool(key, false);
        assert!(!s.get_bool(key), "{key} did not round-trip as false");
    }
    assert!(!s.get_bool("not_a_key"));

    // The bool keys must be genuinely distinct fields — a match arm that
    // reads the neighbouring field looks correct under a one-key test.
    let mut t = Settings::default();
    t.set_bool("raw", true);
    assert!(t.raw && !t.force && !t.keep_iso);
    let mut u = Settings::default();
    u.set_bool("force", true);
    assert!(u.force && !u.raw && !u.keep_iso);
}

// The update-check URL's `owner/repo` must match where releases are actually published;
// pulled via `include_str!` and cross-checked against the README.
#[test]
fn update_check_url_names_the_repo_releases_are_actually_published_to() {
    let src = include_str!("settings.rs");
    let marker = "const URL: &str = \"";
    let start = src
        .find(marker)
        .expect("update-check URL constant not found in settings.rs")
        + marker.len();
    let end = start
        + src[start..]
            .find('"')
            .expect("unterminated URL string literal");
    let url = &src[start..end];

    let prefix = "https://api.github.com/repos/";
    let suffix = "/releases/latest";
    assert!(
        url.starts_with(prefix) && url.ends_with(suffix),
        "update-check URL has an unexpected shape: {url}"
    );
    let repo_path = &url[prefix.len()..url.len() - suffix.len()];

    let readme = include_str!("../README.md");
    assert!(
        readme.contains(&format!("github.com/{repo_path}/releases")),
        "check_for_update() queries https://api.github.com/repos/{repo_path}/releases/latest, \
             but README.md's own release/download links never mention github.com/{repo_path}/releases — \
             the update check is pointed at the wrong repo"
    );
}
