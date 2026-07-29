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
    for k in ["keep_iso", "capture_without_keys", "debug_log"] {
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
    assert!(
        e.starts_with('/'),
        "tilde should expand to an absolute path"
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
