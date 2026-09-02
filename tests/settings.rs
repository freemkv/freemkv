//! Settings persistence and helpers. These run on any platform (no UI), so
//! they execute in CI on Linux where the desktop shell does not compile.

use freemkv::settings::Settings;

#[test]
fn defaults_ship_no_endpoints() {
    // Deliberate: freemkv ships no keydb/keyserver URL. A default endpoint
    // would silently point every install at one host.
    let s = Settings::default();
    assert!(s.keydb_url.is_empty(), "keydb URL must default empty");
    assert!(
        s.keyserver_url.is_empty(),
        "key service URL must default empty"
    );
    assert!(s.keyserver_token.is_empty(), "token must default empty");
}

#[test]
fn defaults_are_sane() {
    let s = Settings::default();
    assert_eq!(s.rip_mode, "Multi-pass");
    assert_eq!(s.abort_lost_secs, "0", "0 = require a perfect rip");
    assert_eq!(s.max_passes, "5");
    assert!(
        !s.keydb_path.is_empty(),
        "keydb path should have a default location"
    );
}

#[test]
fn get_set_round_trips_every_key() {
    let mut s = Settings::default();
    for k in [
        "dest_dir",
        "container",
        "filename_template",
        "selection",
        "min_title_secs",
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
    ] {
        s.set(k, format!("v-{k}"));
        assert_eq!(s.get(k), format!("v-{k}"), "key {k} did not round-trip");
    }
}

#[test]
fn bool_keys_round_trip() {
    let mut s = Settings::default();
    // `capture_without_keys` was removed: it was a checkbox wired to nothing,
    // and "Keep encrypted (raw passthrough)" already covers the one output where
    // writing ciphertext means anything.
    for k in ["keep_iso", "auto_eject"] {
        s.set_bool(k, true);
        assert!(s.get_bool(k), "bool key {k} did not round-trip");
        s.set_bool(k, false);
        assert!(!s.get_bool(k));
    }
}

#[test]
fn unknown_keys_are_ignored_not_panicking() {
    let mut s = Settings::default();
    s.set("nope", "x".into());
    s.set_bool("nope", true);
    assert_eq!(s.get("nope"), "");
    assert!(!s.get_bool("nope"));
}

#[test]
fn json_round_trips() {
    let s = Settings {
        keydb_url: "https://example.com/keydb.zip".into(),
        max_passes: "3".into(),
        ..Default::default()
    };
    let json = serde_json::to_string(&s).unwrap();
    let back: Settings = serde_json::from_str(&json).unwrap();
    assert_eq!(back.keydb_url, s.keydb_url);
    assert_eq!(back.max_passes, "3");
}

#[test]
fn partial_json_falls_back_to_defaults() {
    // Forward/backward compatibility: an older or newer settings file that is
    // missing keys must not fail to load.
    let back: Settings = serde_json::from_str(r#"{"max_passes":"9"}"#).unwrap();
    assert_eq!(back.max_passes, "9");
    assert_eq!(back.rip_mode, "Multi-pass", "missing keys take defaults");
}

#[test]
fn keydb_update_refuses_empty_url() {
    let e = freemkv::settings::update_keydb("", "/tmp/x").unwrap_err();
    assert!(e.contains("No keydb update URL"), "got: {e}");
}

#[test]
fn keydb_update_refuses_empty_destination() {
    let e = freemkv::settings::update_keydb("https://example.com/x.zip", "").unwrap_err();
    assert!(e.contains("No keydb.cfg location"), "got: {e}");
}

#[test]
fn keydb_status_reports_missing_file() {
    let s = Settings {
        keydb_path: "/nonexistent/definitely/not/here/keydb.cfg".into(),
        ..Default::default()
    };
    assert!(s.keydb_status().contains("no keydb.cfg found"));
}

#[test]
fn shellexpand_expands_tilde_only_at_start() {
    let e = freemkv::settings::shellexpand("~/x");
    // "Absolute" is spelled differently per OS: a leading slash on Unix, a
    // drive letter on Windows. Requiring '/' tested the spelling, not the
    // property, so it failed on Windows for a correctly-expanded path.
    assert!(
        std::path::Path::new(&e).is_absolute(),
        "tilde should expand to an absolute path, got: {e}"
    );
    assert_eq!(freemkv::settings::shellexpand("/a/~/b"), "/a/~/b");
}

#[test]
fn cli_parity_flags_persist() {
    // --raw, --force and log detail have CLI equivalents; they must survive a
    // save/load round-trip like every other setting.
    let mut s = Settings::default();
    assert!(!s.raw, "raw defaults off — decrypt is the default");
    assert!(!s.force, "force defaults off — never overwrite silently");
    s.set_bool("raw", true);
    s.set_bool("force", true);
    s.set("log_level", "Verbose".into());
    let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
    assert!(back.raw);
    assert!(back.force);
    assert_eq!(back.log_level, "Verbose");
}

/// Guards the process-global `HOME`. Two tests here mutate it, and the test
/// binary runs them in parallel, so without this they would race — the precise
/// shared-global collision this suite was audited for.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Home-derived paths must stay absolute even with no HOME set (regressed via
// unwrap_or_default()). Mutates the process-global HOME; see docs/settings-tests.md.
// See docs/settings-tests.md — derived_paths_stay_absolute_without_a_home_variable
#[test]
fn derived_paths_stay_absolute_without_a_home_variable() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Per-OS: Unix derives from HOME, Windows from USERPROFILE/APPDATA. Clearing
    // only HOME would make this test vacuous on Windows.
    let vars = ["HOME", "USERPROFILE", "APPDATA"];
    let saved: Vec<(&str, Option<std::ffi::OsString>)> =
        vars.iter().map(|v| (*v, std::env::var_os(v))).collect();
    // SAFETY: serialised by HOME_LOCK; every var restored before returning.
    for v in vars {
        unsafe { std::env::remove_var(v) };
    }

    let expanded = freemkv::settings::shellexpand("~/x");
    let support = freemkv::platform::support_dir();
    let dest = freemkv::platform::default_dest_dir();
    let home = freemkv::platform::home_dir();

    for (v, val) in saved {
        if let Some(x) = val {
            unsafe { std::env::set_var(v, x) };
        }
    }

    assert!(!home.as_os_str().is_empty(), "home_dir() was empty");
    assert!(home.is_absolute(), "home_dir() relative: {home:?}");
    assert!(
        std::path::Path::new(&expanded).is_absolute(),
        "shellexpand relative without HOME: {expanded}"
    );
    assert!(support.is_absolute(), "support_dir relative: {support:?}");
    assert!(dest.is_absolute(), "default_dest_dir relative: {dest:?}");
}

// save()/load() must actually round-trip through disk, not just serde_json
// in memory — a no-op save() previously survived every other test here.
// See docs/settings-tests.md — settings_round_trip_through_a_real_file
#[test]
fn settings_round_trip_through_a_real_file() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let vars = ["HOME", "USERPROFILE", "APPDATA"];
    let saved: Vec<(&str, Option<std::ffi::OsString>)> =
        vars.iter().map(|v| (*v, std::env::var_os(v))).collect();

    let dir = std::env::temp_dir().join(format!("fmkv-settings-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: serialised by HOME_LOCK; every var restored before returning.
    for v in vars {
        unsafe { std::env::set_var(v, &dir) };
    }

    // Absolute on whichever OS is running, so `normalize()` keeps it.
    let want_dest = dir.join("out").to_string_lossy().into_owned();
    let want_url = "https://keys.example/api".to_string();

    let outcome = std::panic::catch_unwind({
        let want_dest = want_dest.clone();
        let want_url = want_url.clone();
        move || {
            let mut s = freemkv::settings::Settings::load();
            s.dest_dir = want_dest;
            s.keyserver_url = want_url;
            s.save().expect("save should succeed into a writable home");

            let again = freemkv::settings::Settings::load();
            (again.dest_dir, again.keyserver_url)
        }
    });

    for (v, val) in saved {
        match val {
            Some(x) => unsafe { std::env::set_var(v, x) },
            None => unsafe { std::env::remove_var(v) },
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    let (dest, url) = outcome.expect("round trip panicked");
    assert_eq!(
        dest, want_dest,
        "dest_dir did not survive save+load — save() may be doing nothing"
    );
    assert_eq!(url, want_url, "keyserver_url did not survive save+load");
}

// gui-settings.json holds keyserver_token in plaintext; checks save() leaves
// no stray .tmp file and (Unix) writes it at mode 0600, not the umask default.
// See docs/settings-tests.md — saved_settings_file_is_private_and_leaves_no_temp_file
#[test]
fn saved_settings_file_is_private_and_leaves_no_temp_file() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let vars = ["HOME", "USERPROFILE", "APPDATA"];
    let saved: Vec<(&str, Option<std::ffi::OsString>)> =
        vars.iter().map(|v| (*v, std::env::var_os(v))).collect();

    let dir = std::env::temp_dir().join(format!("fmkv-settings-perm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: serialised by HOME_LOCK; every var restored before returning.
    for v in vars {
        unsafe { std::env::set_var(v, &dir) };
    }

    let outcome = std::panic::catch_unwind(|| {
        let mut s = freemkv::settings::Settings::load();
        s.keyserver_token = "s3cr3t-token".into();
        s.save().expect("save should succeed into a writable home");

        let support = freemkv::settings::support_dir();
        let settings_file = support.join("gui-settings.json");
        assert!(settings_file.exists(), "save() did not create the file");

        // No `.json.tmp.<pid>` (or any other stray file) left in the support
        // dir — the rename must have completed, not left the temp copy
        // sitting next to (or instead of) the real file.
        let leftovers: Vec<String> = std::fs::read_dir(&support)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "gui-settings.json")
            .collect();
        assert!(
            leftovers.is_empty(),
            "save() left stray files behind: {leftovers:?}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&settings_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o600,
                "gui-settings.json (holds keyserver_token in plaintext) is \
                 mode {mode:#o}, not 0600 — readable by other local accounts"
            );
        }
    });

    for (v, val) in saved {
        match val {
            Some(x) => unsafe { std::env::set_var(v, x) },
            None => unsafe { std::env::remove_var(v) },
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    outcome.expect("round trip panicked");
}

// Helper for the tests below: point the per-OS support dir at a fresh temp
// dir, run f, restore env, delete the dir. Must redirect HOME/USERPROFILE/APPDATA
// together or these would read/write the runner's real gui-settings.json.
fn in_a_temp_support_dir<T>(tag: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let vars = ["HOME", "USERPROFILE", "APPDATA"];
    let saved: Vec<(&str, Option<std::ffi::OsString>)> =
        vars.iter().map(|v| (*v, std::env::var_os(v))).collect();

    let dir = std::env::temp_dir().join(format!("fmkv-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // On macOS the support dir is `$HOME/Library/Application Support/freemkv`;
    // create the whole chain so the settings file can be planted before load.
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: serialised by HOME_LOCK; every var restored before returning.
    for v in vars {
        unsafe { std::env::set_var(v, &dir) };
    }
    let support = freemkv::settings::support_dir();
    let outcome = std::fs::create_dir_all(&support)
        .map_err(|e| format!("{e}"))
        .and_then(|()| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&support)))
                .map_err(|_| "closure panicked".to_string())
        });

    for (v, val) in saved {
        match val {
            Some(x) => unsafe { std::env::set_var(v, x) },
            None => unsafe { std::env::remove_var(v) },
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    outcome.unwrap()
}

// A leading UTF-8 BOM must not wipe the config: load() used to fall back to
// defaults on ANY parse failure, silently discarding a file PowerShell/Notepad
// commonly write with a BOM. See docs/settings-tests.md for the incident.
#[test]
fn a_settings_file_with_a_utf8_bom_still_loads_the_users_values() {
    in_a_temp_support_dir("settings-bom", |support| {
        let want_url = "https://keys.example/api";
        let want_token = "s3cr3t-token";
        let body = serde_json::json!({
            "keyserver_url": want_url,
            "keyserver_token": want_token,
            "filename_template": "{title}-bom",
        })
        .to_string();
        std::fs::write(
            support.join("gui-settings.json"),
            format!("\u{feff}{body}").as_bytes(),
        )
        .unwrap();

        let s = Settings::load();
        assert_eq!(
            s.keyserver_url, want_url,
            "a BOM discarded the whole settings file"
        );
        assert_eq!(s.keyserver_token, want_token, "the token was lost to a BOM");
        assert_eq!(s.filename_template, "{title}-bom");
    });
}

// An unparseable settings file must be renamed aside (to .bad), not left in
// place — old load() returned defaults and left it live, so the next save()
// overwrote it and the user's token/dest dir were unrecoverable.
#[test]
fn an_unparseable_settings_file_is_preserved_before_a_save_can_overwrite_it() {
    in_a_temp_support_dir("settings-bad", |support| {
        // Truncated mid-write — the crash-during-save shape.
        let original = r#"{"keyserver_token":"s3cr3t-token","keyserver_url":"https://keys.exa"#;
        let path = support.join("gui-settings.json");
        std::fs::write(&path, original).unwrap();

        let s = Settings::load();
        let bad = support.join("gui-settings.json.bad");
        assert!(
            bad.exists(),
            "the unreadable settings file was not preserved; support dir now: {:?}",
            std::fs::read_dir(support)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        );

        // The app carries on with defaults — and then writes them out.
        s.save().expect("save should succeed into a writable home");

        assert_eq!(
            std::fs::read_to_string(&bad).unwrap(),
            original,
            "the preserved copy no longer holds the user's original bytes"
        );
        assert!(
            !std::fs::read_to_string(&path).unwrap().contains("s3cr3t"),
            "test setup wrong: the live file should be the freshly saved defaults"
        );
    });
}

// A leftover gui-settings.json.tmp.<pid> (per-process, not per-attempt) must
// not block every later save() with AlreadyExists nor keep the token in it
// forever. Debris is simulated since a write can't be forced to fail mid-way.
#[test]
fn a_leftover_temp_file_neither_blocks_the_next_save_nor_keeps_the_token() {
    in_a_temp_support_dir("settings-orphan", |support| {
        let orphan = support.join(format!("gui-settings.json.tmp.{}", std::process::id()));
        std::fs::write(&orphan, r#"{"keyserver_token":"s3cr3t-token"}"#).unwrap();

        let mut s = Settings::load();
        s.keyserver_url = "https://keys.example/".into();
        s.save()
            .expect("a save must not be blocked forever by its own debris");

        assert!(
            !orphan.exists(),
            "the orphaned temp file still holds the token in plaintext"
        );
        let leftovers: Vec<String> = std::fs::read_dir(support)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "gui-settings.json")
            .collect();
        assert!(leftovers.is_empty(), "stray files left: {leftovers:?}");
        assert!(
            std::fs::read_to_string(support.join("gui-settings.json"))
                .unwrap()
                .contains("keys.example"),
            "the save that was supposed to happen did not land"
        );
    });
}
