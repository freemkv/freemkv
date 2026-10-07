use super::*;

// Per project convention, tests never touch /tmp (wiped on reboot).
// Anchor scratch under the workspace's target/ (gitignored), not /tmp.
fn scratch(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(format!(
            "autorip-config-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg_in(dir: &std::path::Path) -> Config {
    Config {
        autorip_dir: dir.to_string_lossy().to_string(),
        ..Config::default()
    }
}

// H9: an older snapshot queued after a newer one must never land last.
#[test]
fn save_coalesced_never_lets_an_older_snapshot_land_last() {
    let d = scratch("save_order");
    let mut older = cfg_in(&d);
    older.tmdb_api_key = "older".into();
    let mut newer = cfg_in(&d);
    newer.tmdb_api_key = "newer".into();
    let path = newer.settings_file();
    let g_old = next_save_generation();
    let g_new = next_save_generation();
    let rx_new = save_coalesced(newer, g_new).expect("spawn writer");
    let rx_old = save_coalesced(older, g_old).expect("spawn writer");
    let wait = std::time::Duration::from_secs(10);
    rx_new.recv_timeout(wait).unwrap().expect("newer save");
    rx_old
        .recv_timeout(wait)
        .unwrap()
        .expect("a superseded save is acknowledged Ok");
    let data = std::fs::read_to_string(&path).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(
        parsed["tmdb_api_key"].as_str(),
        Some("newer"),
        "an older snapshot finishing last must not overwrite the newest one"
    );
    let _ = std::fs::remove_dir_all(&d);
}

// A failed write reports Err to its waiter and does not mark it persisted.
#[test]
fn save_coalesced_reports_write_failure() {
    let d = scratch("save_fail");
    let cfg = cfg_in(&d.join("missing-dir"));
    let rx = save_coalesced(cfg, next_save_generation()).expect("spawn writer");
    let r = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
    assert!(r.is_err(), "a save into a missing dir must report Err");
    let _ = std::fs::remove_dir_all(&d);
}

// H5/H10: a webhook entry with a non-bool flag is dropped AND reported.
#[test]
fn load_saved_warns_on_non_bool_webhook_flag() {
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let d = scratch("webhook_flag");
    let base = cfg_in(&d);
    std::fs::write(
        base.settings_file(),
        serde_json::json!({
            "webhook_urls": [
                {"url": "https://example.com/bad", "post_mux": "yes"},
                {"url": "https://example.com/good", "post_rip": false},
            ]
        })
        .to_string(),
    )
    .unwrap();
    let buf = Buf::default();
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .with_writer(buf.clone())
            .with_ansi(false),
    );
    let cfg = tracing::subscriber::with_default(subscriber, || load_saved(base));
    assert_eq!(
        cfg.webhook_urls,
        vec![WebhookEntry {
            url: "https://example.com/good".into(),
            post_rip: false,
            post_mux: true,
            post_move: true,
            headers: Default::default(),
        }],
        "the malformed entry is dropped; the valid one (absent flags -> true) is kept"
    );
    let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(
        out.contains("webhook_urls[0].post_mux"),
        "dropping a webhook with a non-bool flag must be logged, naming the field; logs:\n{out}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn save_writes_atomically_and_leaves_no_temp() {
    let d = scratch("save_ok");
    let mut cfg = cfg_in(&d);
    cfg.tmdb_api_key = "abc123".into();
    save(&cfg).expect("save must succeed to a writable dir");

    let path = cfg.settings_file();
    let data = std::fs::read_to_string(&path).expect("settings.json written");
    let parsed: serde_json::Value = serde_json::from_str(&data).unwrap();
    assert_eq!(parsed["tmdb_api_key"].as_str(), Some("abc123"));
    // The sibling temp file must be cleaned up (renamed away).
    assert!(
        !std::path::Path::new(&format!("{path}.tmp")).exists(),
        "temp file should not linger after a successful save"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn concurrent_saves_never_corrupt_settings_json() {
    // Two settings saves can run concurrently (one thread per request).
    // The unique-per-call temp name (pid + counter) gives each its own
    // sibling temp, so every rename publishes one writer's COMPLETE bytes.
    let d = scratch("concurrent");
    const N: usize = 16;
    let handles: Vec<_> = (0..N)
        .map(|i| {
            let dir = d.clone();
            std::thread::spawn(move || {
                let mut cfg = cfg_in(&dir);
                // Distinct, generously-sized payload per thread so an
                // interleave would produce invalid JSON, not a value
                // that happens to parse.
                cfg.tmdb_api_key = format!("key-{i}-{}", "x".repeat(4096));
                save(&cfg)
            })
        })
        .collect();
    for h in handles {
        h.join().expect("save thread panicked").expect("save Err");
    }

    // Final file is valid JSON (no interleave corruption).
    let path = cfg_in(&d).settings_file();
    let data = std::fs::read_to_string(&path).expect("settings.json written");
    let parsed: serde_json::Value =
        serde_json::from_str(&data).expect("settings.json must be valid JSON after concurrency");
    let key = parsed["tmdb_api_key"].as_str().unwrap_or("");
    assert!(
        key.starts_with("key-") && key.ends_with(&"x".repeat(4096)),
        "final key must be one writer's COMPLETE value, got {} chars",
        key.len()
    );

    // No temp turds linger (every save renamed its own unique temp away).
    let leftovers: Vec<_> = std::fs::read_dir(&d)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no .tmp files should remain, found {}",
        leftovers.len()
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn save_leaves_prior_settings_untouched_when_write_fails() {
    // Failure-mode contract: if the temp write/fsync fails, the rename
    // must never run, so a pre-existing settings.json is preserved.
    // Force the failure via a path whose parent doesn't exist (ENOENT).
    let d = scratch("save_fail");
    let good = cfg_in(&d);
    // Seed a valid prior file.
    let mut prior = good.clone();
    prior.tmdb_api_key = "PRIOR".into();
    save(&prior).expect("seeding the prior settings.json must succeed");
    let good_path = good.settings_file();
    let before = std::fs::read_to_string(&good_path).unwrap();

    // Now attempt a save whose temp open will fail: settings_file()
    // lives under a non-existent subdirectory, so OpenOptions::open
    // returns ENOENT and save() must bail before any rename.
    let bad = cfg_in(&d.join("does-not-exist"));
    let mut changed = bad.clone();
    changed.tmdb_api_key = "SHOULD_NOT_LAND".into();
    // This save is EXPECTED to fail (ENOENT) — the point of the test.
    assert!(save(&changed).is_err(), "save into a missing dir must Err");

    // The same-path failure: a read-only directory refuses the temp file, so the prior
    // file at THIS path must survive byte for byte.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut same = good.clone();
        same.tmdb_api_key = "SHOULD_NOT_LAND".into();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = save(&same);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(r.is_err(), "a read-only dir must refuse the save");
            assert_eq!(std::fs::read_to_string(&good_path).unwrap(), before);
        }
    }

    // The good file is byte-for-byte intact.
    let after = std::fs::read_to_string(&good_path).unwrap();
    assert_eq!(before, after, "prior settings.json must be untouched");
    // No temp turd left behind in the bad location either.
    assert!(!std::path::Path::new(&format!("{}.tmp", bad.settings_file())).exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn load_saved_clamps_pathological_durations() {
    let d = scratch("clamp");
    let path = cfg_in(&d).settings_file();
    std::fs::write(
        &path,
        serde_json::json!({
            "max_rip_duration_secs": u64::MAX,
            "min_pass_budget_secs": u64::MAX,
            "log_retention_days": u64::MAX,
        })
        .to_string(),
    )
    .unwrap();
    let cfg = load_saved(cfg_in(&d));
    assert_eq!(cfg.max_rip_duration_secs, 30 * 24 * 3600);
    assert_eq!(cfg.min_pass_budget_secs, 30 * 24 * 3600);
    assert_eq!(cfg.log_retention_days, 3650);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn port_not_serialized_into_settings_json() {
    let d = scratch("port");
    let cfg = cfg_in(&d);
    save(&cfg).expect("save must succeed to a writable dir");
    let data = std::fs::read_to_string(cfg.settings_file()).unwrap();
    assert!(
        !data.contains("\"port\""),
        "port is bootstrap-only and must not be persisted: {data}"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn decrypt_threads_clamped_on_load() {
    let d = scratch("decrypt_clamp");
    let base = cfg_in(&d);
    std::fs::write(base.settings_file(), r#"{"decrypt_threads": 100000}"#).unwrap();
    let loaded = load_saved(base);
    assert_eq!(loaded.decrypt_threads, 256, "huge value must clamp to 256");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn decrypt_threads_small_value_preserved() {
    let d = scratch("decrypt_small");
    let base = cfg_in(&d);
    std::fs::write(base.settings_file(), r#"{"decrypt_threads": 8}"#).unwrap();
    let loaded = load_saved(base);
    assert_eq!(loaded.decrypt_threads, 8);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn corrupt_settings_reverts_to_defaults_without_panicking() {
    let d = scratch("corrupt");
    let base = cfg_in(&d);
    // Partial write — invalid JSON. Must not panic and must keep defaults.
    std::fs::write(base.settings_file(), r#"{"max_retries": 5, "abort_on_l"#).unwrap();
    let loaded = load_saved(cfg_in(&d));
    assert_eq!(loaded.max_retries, Config::default().max_retries);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn missing_settings_file_uses_defaults() {
    let d = scratch("missing");
    // No settings.json written.
    let loaded = load_saved(cfg_in(&d));
    assert_eq!(loaded.max_retries, Config::default().max_retries);
    assert_eq!(
        loaded.abort_on_lost_secs,
        Config::default().abort_on_lost_secs
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn save_then_load_roundtrips_and_returns_ok() {
    let d = scratch("roundtrip");
    let mut base = cfg_in(&d);
    base.abort_on_lost_secs = 30;
    base.max_retries = 3;
    base.decrypt_threads = 4;
    save(&base).expect("save must succeed to a writable dir");
    let loaded = load_saved(cfg_in(&d));
    assert_eq!(loaded.abort_on_lost_secs, 30);
    assert_eq!(loaded.max_retries, 3);
    assert_eq!(loaded.decrypt_threads, 4);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn save_to_unwritable_dir_returns_err() {
    // settings_file() under a path whose parent does not exist -> open
    // of the .tmp fails -> Err, not a false success.
    let base = Config {
        autorip_dir: "/nonexistent-autorip-dir-xyz/sub".into(),
        ..Config::default()
    };
    assert!(save(&base).is_err());
}

/// Write a settings.json containing `json` under `dir`, then run it
/// through `load_saved`. Mirrors `cfg_in` but seeds the file first.
fn load_with(dir: &std::path::Path, json: &str) -> Config {
    std::fs::write(cfg_in(dir).settings_file(), json).unwrap();
    load_saved(cfg_in(dir))
}

#[test]
fn library_fields_are_additive_and_tolerant() {
    let d = scratch("library-fields");
    let cfg = load_with(&d, r#"{"movie_dir": "Films"}"#);
    assert_eq!(cfg.library_dir, "");
    assert_eq!(cfg.library_iso_dir, "");
    assert!(
        !cfg.library_iso_subfolders,
        "top-level ISOs only by default"
    );
    let cfg = load_with(
        &d,
        r#"{"library_dir": "/lib", "library_iso_dir": 7, "library_iso_subfolders": "yes", "movie_dir": "Films"}"#,
    );
    assert_eq!(cfg.library_dir, "/lib");
    assert_eq!(cfg.library_iso_dir, "", "wrong type keeps the default");
    assert!(!cfg.library_iso_subfolders);
    assert_eq!(cfg.movie_dir, "Films");
    let cfg = load_with(&d, r#"{"library_iso_subfolders": true}"#);
    assert!(cfg.library_iso_subfolders);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn legacy_abort_on_error_false_migrates_to_skip() {
    let d = scratch("abort_false");
    // Pre-migration settings.json: only the legacy bool, no
    // on_read_error field. False must become the looser "skip",
    // not silently fall through to the default "stop".
    let cfg = load_with(&d, r#"{"abort_on_error": false}"#);
    assert_eq!(cfg.on_read_error, "skip");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn legacy_abort_on_error_true_migrates_to_stop() {
    let d = scratch("abort_true");
    let cfg = load_with(&d, r#"{"abort_on_error": true}"#);
    assert_eq!(cfg.on_read_error, "stop");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn explicit_on_read_error_wins_over_legacy_key() {
    let d = scratch("explicit_wins");
    // A migrated settings.json keeps the stale abort_on_error key
    // alongside the modern field; the explicit field must win so
    // re-loading doesn't flip the policy back.
    let cfg = load_with(&d, r#"{"on_read_error": "skip", "abort_on_error": true}"#);
    assert_eq!(cfg.on_read_error, "skip");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn no_legacy_key_uses_default() {
    let d = scratch("no_legacy");
    let cfg = load_with(&d, r#"{}"#);
    assert_eq!(cfg.on_read_error, "stop");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn on_insert_resume_setting_is_loaded() {
    let d = scratch("on_insert_resume");
    let cfg = load_with(&d, r#"{"on_insert":"resume"}"#);
    assert_eq!(cfg.on_insert, "resume");
    let _ = std::fs::remove_dir_all(&d);
}

// Parses a checked-in, hand-authored settings.json fixture through the
// real `load_saved` and asserts every field independently — not a
// serialize/deserialize self-roundtrip, so a dropped/mis-keyed field fails.
#[test]
fn real_settings_json_fixture_parses_field_by_field() {
    const FIXTURE: &str = include_str!("../../tests/server/fixtures/settings.json");
    let d = scratch("fixture");
    let cfg = load_with(&d, FIXTURE);

    assert_eq!(cfg.staging_dir, "/staging-local");
    assert_eq!(cfg.output_dir, "/alt-output");
    assert_eq!(cfg.movie_dir, "movies");
    assert_eq!(cfg.tv_dir, "tv");
    assert_eq!(cfg.min_length_secs, 900);
    assert!(!cfg.main_feature);
    assert!(!cfg.auto_eject);
    assert_eq!(cfg.on_insert, "rip");
    assert_eq!(cfg.output_format, "iso");
    assert_eq!(cfg.network_target, "nas.example.com:9000");
    assert_eq!(cfg.on_read_error, "skip");
    assert_eq!(cfg.max_retries, 3);
    assert!(cfg.keep_iso);
    assert_eq!(cfg.abort_on_lost_secs, 30);
    assert!(cfg.capture_without_keys);
    assert_eq!(cfg.max_rip_duration_secs, 14400);
    assert_eq!(cfg.min_pass_budget_secs, 1800);
    assert_eq!(cfg.transport_recovery_delay_secs, 10);
    assert_eq!(cfg.tmdb_api_key, "deadbeefcafef00ddeadbeefcafef00d");
    assert_eq!(
        cfg.keydb_path.as_deref(),
        Some("/root/.config/freemkv/keydb.cfg")
    );
    assert_eq!(
        cfg.keydb_url,
        "https://keydb.example.org/export/keydb_eng.zip"
    );
    assert_eq!(cfg.key_source, "online");
    assert_eq!(cfg.keyserver_url, "https://keys.example.org/decode");
    assert_eq!(cfg.keyserver_secret, "s3cr3t-token");
    assert_eq!(cfg.decrypt_threads, 4);
    assert_eq!(cfg.log_retention_days, 14);
    // The empty webhook URL in the fixture array must be filtered out.
    // The fixture stores bare legacy strings, which must load as
    // "fire on every stage" (pre-1.6.8 behaviour).
    assert_eq!(
        cfg.webhook_urls,
        vec![
            WebhookEntry {
                url: "https://discord.com/api/webhooks/1/abc".to_string(),
                post_rip: true,
                post_mux: true,
                post_move: true,
                headers: Default::default(),
            },
            WebhookEntry {
                url: "https://jellyfin.example.org/hook".to_string(),
                post_rip: true,
                post_mux: true,
                post_move: true,
                headers: Default::default(),
            },
        ],
        "empty webhook URLs must be dropped on load; bare strings fire on every stage"
    );
    let _ = std::fs::remove_dir_all(&d);
}

// Exercises a mixed array: object form keeps its flags, bare string
// fires every stage, a missing flag defaults true, and a blank/urlless
// entry drops — the real shape settings.json may carry after upgrade.
#[test]
fn webhook_entries_load_mixed_string_and_object_forms() {
    let d = scratch("webhook-forms");
    let json = r#"{
            "webhook_urls": [
                "https://legacy.example/hook",
                {"url": "https://both.example/hook"},
                {"url": "https://rip-only.example/hook", "post_rip": true, "post_mux": false, "post_move": false},
                {"url": "https://move-only.example/hook", "post_rip": false, "post_mux": false, "post_move": true},
                {"url": "https://legacy-flags.example/hook", "post_rip": false, "post_move": true},
                {"url": "   "},
                {"post_rip": true},
                "  "
            ]
        }"#;
    std::fs::write(d.join("settings.json"), json).unwrap();
    let cfg = load_saved(cfg_in(&d));
    assert_eq!(
        cfg.webhook_urls,
        vec![
            WebhookEntry {
                url: "https://legacy.example/hook".into(),
                post_rip: true,
                post_mux: true,
                post_move: true,
                headers: Default::default(),
            },
            WebhookEntry {
                url: "https://both.example/hook".into(),
                post_rip: true,
                post_mux: true,
                post_move: true,
                headers: Default::default(),
            },
            WebhookEntry {
                url: "https://rip-only.example/hook".into(),
                post_rip: true,
                post_mux: false,
                post_move: false,
                headers: Default::default(),
            },
            WebhookEntry {
                url: "https://move-only.example/hook".into(),
                post_rip: false,
                post_mux: false,
                post_move: true,
                headers: Default::default(),
            },
            // A pre-1.6.8 object with no post_mux key: the mux stage
            // defaults ON so the upgrade never silently drops the
            // completion notification that used to ride post_rip.
            WebhookEntry {
                url: "https://legacy-flags.example/hook".into(),
                post_rip: false,
                post_mux: true,
                post_move: true,
                headers: Default::default(),
            },
        ],
        "object flags load verbatim; bare string and missing flags default to fire-on-every-stage; blank/urlless entries drop"
    );
    let _ = std::fs::remove_dir_all(&d);
}

// Per-event webhook flags must round-trip through `save`/`load_saved`
// unchanged: a saved "move only" hook must still be "move only" on reload.
#[test]
fn webhook_entries_survive_save_load_round_trip() {
    let d = scratch("webhook-roundtrip");
    let mut cfg = cfg_in(&d);
    cfg.webhook_urls = vec![
        WebhookEntry {
            url: "https://both.example/hook".into(),
            post_rip: true,
            post_mux: true,
            post_move: true,
            headers: Default::default(),
        },
        WebhookEntry {
            url: "https://move-only.example/hook".into(),
            post_rip: false,
            post_mux: false,
            post_move: true,
            headers: Default::default(),
        },
    ];
    save(&cfg).expect("save");
    let reloaded = load_saved(cfg_in(&d));
    assert_eq!(reloaded.webhook_urls, cfg.webhook_urls);
    let _ = std::fs::remove_dir_all(&d);
}

/// Malformed / wrong-typed values at the settings.json trust boundary must
/// each fall back to the field default WITHOUT wiping the rest of the file.
/// Drives the real per-field type-gating + enum validation in `load_saved`.
#[test]
fn malformed_values_fall_back_to_defaults_field_independently() {
    let d = scratch("malformed");
    // Every field has a wrong type or an invalid enum value, EXCEPT
    // movie_dir which is well-formed — proving the bad fields don't wipe
    // the good one (independent gating).
    let json = r#"{
            "max_retries": "three",
            "abort_on_lost_secs": -5,
            "main_feature": "yes",
            "on_insert": "explode",
            "output_format": "garbage",
            "on_read_error": "panic",
            "key_source": "telepathy",
            "decrypt_threads": "lots",
            "movie_dir": "Films"
        }"#;
    let cfg = load_with(&d, json);
    let def = Config::default();

    // Wrong-typed / out-of-range numerics keep defaults.
    assert_eq!(
        cfg.max_retries, def.max_retries,
        "string max_retries → default"
    );
    assert_eq!(
        cfg.abort_on_lost_secs, def.abort_on_lost_secs,
        "negative abort_on_lost_secs (not as_u64) → default"
    );
    assert_eq!(cfg.main_feature, def.main_feature, "string bool → default");
    assert_eq!(
        cfg.decrypt_threads, def.decrypt_threads,
        "string usize → default"
    );
    // Invalid enum strings keep defaults (validated against allowed sets).
    assert_eq!(cfg.on_insert, def.on_insert, "unknown on_insert → default");
    assert_eq!(
        cfg.output_format, def.output_format,
        "unknown output_format → default"
    );
    assert_eq!(
        cfg.on_read_error, def.on_read_error,
        "unknown on_read_error → default"
    );
    assert_eq!(
        cfg.key_source, def.key_source,
        "unknown key_source → default"
    );
    // The one well-formed field still loads — bad neighbours didn't wipe it.
    assert_eq!(
        cfg.movie_dir, "Films",
        "a valid field survives bad neighbours"
    );
    let _ = std::fs::remove_dir_all(&d);
}

/// A settings.json that fails to parse entirely (e.g. a partial write from
/// a SIGKILL mid-save) must revert ALL persisted fields to defaults rather
/// than panicking or loading garbage. Drives the real parse-failure branch.
#[test]
fn unparseable_settings_json_reverts_to_defaults() {
    let d = scratch("unparseable");
    let cfg = load_with(&d, "{ this is not valid json ");
    let def = Config::default();
    assert_eq!(cfg.max_retries, def.max_retries);
    assert_eq!(cfg.staging_dir, def.staging_dir);
    assert_eq!(cfg.output_format, def.output_format);
    let _ = std::fs::remove_dir_all(&d);
}

// Numeric knobs above their trust-boundary ceiling are clamped on load;
// complements `load_saved_clamps_pathological_durations` by exercising
// the retention + decrypt_threads ceilings via fixture-shaped JSON.
#[test]
fn over_ceiling_numeric_knobs_are_clamped_on_load() {
    let d = scratch("clamp_ceiling");
    let json = r#"{
            "log_retention_days": 999999,
            "decrypt_threads": 100000,
            "max_retries": 250
        }"#;
    let cfg = load_with(&d, json);
    assert_eq!(cfg.log_retention_days, 3650, "retention clamps to 10y");
    assert_eq!(cfg.decrypt_threads, 256, "decrypt_threads clamps to 256");
    assert_eq!(cfg.max_retries, 10, "max_retries clamps to 10");
    let _ = std::fs::remove_dir_all(&d);
}

// A realistic duration well under the 30-day ceiling must survive `load_saved` unclamped,
// pinned to an absolute value (not the production literal).
#[test]
fn realistic_mid_range_duration_survives_unclamped() {
    let d = scratch("mid_range_duration");
    let path = cfg_in(&d).settings_file();
    std::fs::write(
        &path,
        serde_json::json!({
            "max_rip_duration_secs": 21_600u64, // 6h — realistic, not pathological
        })
        .to_string(),
    )
    .unwrap();
    let cfg = load_saved(cfg_in(&d));
    assert_eq!(
        cfg.max_rip_duration_secs, 21_600,
        "a realistic 6h duration must not be clamped"
    );
    // The shipped 8h UHD default itself must also survive a fresh load
    // (no settings.json override) — this is the value the whole ceiling
    // exists to NOT interfere with in normal operation.
    assert_eq!(Config::default().max_rip_duration_secs, 28_800);
    let _ = std::fs::remove_dir_all(&d);
}

// `Debug for Config` must mask every secret-bearing field. No existing
// test called `format!("{:?}", cfg)`, so a future edit swapping
// `redact(&self.x)` for `&self.x` would otherwise leak silently.
#[test]
fn debug_redacts_all_secret_fields() {
    let cfg = Config {
        tmdb_api_key: "tmdb-real-secret-abc123".into(),
        keydb_url: "https://keydb.example.org/export/keydb_eng.zip?token=KEYDB_SECRET".into(),
        keyserver_url: "https://keys.example.org/decode".into(),
        keyserver_secret: "keyserver-bearer-token-xyz789".into(),
        webhook_urls: vec![
            WebhookEntry {
                url: "https://discord.com/api/webhooks/1/DISCORD_SECRET_TOKEN".into(),
                post_rip: true,
                post_mux: true,
                post_move: true,
                headers: Default::default(),
            },
            WebhookEntry {
                url: "https://hooks.example.com?token=WEBHOOK_SECRET".into(),
                post_rip: true,
                post_mux: true,
                post_move: false,
                headers: Default::default(),
            },
        ],
        ..Config::default()
    };

    let debug_output = format!("{:?}", cfg);

    // None of the raw secrets may appear anywhere in the output.
    assert!(!debug_output.contains("tmdb-real-secret-abc123"));
    assert!(!debug_output.contains("KEYDB_SECRET"));
    assert!(!debug_output.contains("keyserver-bearer-token-xyz789"));
    assert!(!debug_output.contains("DISCORD_SECRET_TOKEN"));
    assert!(!debug_output.contains("WEBHOOK_SECRET"));
    // keyserver_url may carry a token, so it's always redacted too.

    // The redaction markers must be present too, proving the fields
    // were visited and masked rather than absent from a no-op Debug.
    assert!(debug_output.contains("<redacted>"));
    assert!(debug_output.contains("2 redacted")); // webhook_urls count
    // Non-secret fields must still print normally — Debug stays useful.
    assert!(debug_output.contains("Config"));
    assert!(debug_output.contains("port"));
}

// On an all-empty-secrets `Config::default()`, Debug must show "<unset>"
// not "<redacted>" — proving `redact()` distinguishes "no secret" from
// "secret present and hidden" rather than a fixed placeholder.
#[test]
fn debug_marks_empty_secrets_as_unset_not_redacted() {
    let cfg = Config::default();
    let debug_output = format!("{:?}", cfg);
    assert!(debug_output.contains("<unset>"));
}

// `should_relocate_bare_run_dir` — the common case (unmodified path,
// real Docker mount present) must NOT relocate, or every normal
// deployment would redirect rips into the container's ephemeral overlay.
#[test]
fn should_relocate_only_when_default_path_and_mount_absent() {
    // Default path, mount present (normal Docker deployment) -> do NOT relocate.
    assert!(!should_relocate_bare_run_dir("/staging", "/staging", true));
    // Default path, mount absent (bare-run binary, no container) -> relocate.
    assert!(should_relocate_bare_run_dir("/staging", "/staging", false));
    // Customized path -> never relocate, regardless of mount state.
    assert!(!should_relocate_bare_run_dir(
        "/mnt/media/staging",
        "/staging",
        true
    ));
    assert!(!should_relocate_bare_run_dir(
        "/mnt/media/staging",
        "/staging",
        false
    ));
    // Same shape for the output_dir case.
    assert!(!should_relocate_bare_run_dir("/output", "/output", true));
    assert!(should_relocate_bare_run_dir("/output", "/output", false));
}

/// `build_bootstrap_config` must carry the env-derived `port` and
/// `autorip_dir` through into the returned `Config`, not silently fall
/// back to `Config::default()`'s values via the struct-update `..`.
#[test]
fn build_bootstrap_config_carries_env_derived_fields() {
    let cfg = build_bootstrap_config(9999, "/custom/autorip/dir".to_string());
    assert_eq!(cfg.port, 9999);
    assert_eq!(cfg.autorip_dir, "/custom/autorip/dir");
    // Values not sourced from these two env vars still come from
    // Config::default() via the struct-update.
    assert_eq!(cfg.staging_dir, Config::default().staging_dir);
}

/// Setting `decrypt_threads` back to 0 (auto) must reset libfreemkv's pool
/// to its default, not leave the previous explicit count in force.
#[test]
fn apply_decrypt_threads_zero_resets_to_auto() {
    apply_decrypt_threads(0);
    let auto = libfreemkv::decrypt::decrypt_threads();
    let explicit = if auto == 2 { 3 } else { 2 };
    apply_decrypt_threads(explicit);
    assert_eq!(libfreemkv::decrypt::decrypt_threads(), explicit);
    apply_decrypt_threads(0);
    assert_eq!(libfreemkv::decrypt::decrypt_threads(), auto);
}

/// `parse_port_env` — the pure guard behind `load()`'s `PORT` handling.
/// `"0"` is the reserved "ephemeral/unset" sentinel and must be
/// rejected (falls back to 8080 with a warning), not silently bound.
#[test]
fn parse_port_env_rejects_zero_and_garbage_accepts_valid_port() {
    assert_eq!(parse_port_env("8081"), Some(8081));
    assert_eq!(parse_port_env("1"), Some(1));
    assert_eq!(parse_port_env("65535"), Some(65535));
    assert_eq!(parse_port_env("0"), None);
    assert_eq!(parse_port_env("not-a-number"), None);
    assert_eq!(parse_port_env(""), None);
    assert_eq!(parse_port_env("-1"), None);
    assert_eq!(parse_port_env("99999"), None); // out of u16 range
}

/// `dir_is_writable` against a real filesystem: an existing, writable
/// directory must return true; a nonexistent directory (parent doesn't
/// exist either) must return false rather than panicking.
#[test]
fn dir_is_writable_true_for_real_dir_false_for_missing() {
    let d = scratch("writable_probe");
    assert!(dir_is_writable(d.to_str().unwrap()));
    assert!(!dir_is_writable("/nonexistent-autorip-probe-dir-xyz/sub"));
    let _ = std::fs::remove_dir_all(&d);
}

// A configured folder on a hung network mount must not hold startup back: its creation is
// given up on within the limit and reported not responding; the other folders are created.
#[test]
fn startup_folders_are_created_without_waiting_on_a_hung_mount() {
    let d = scratch("bounded-dirs");
    let good = d.join("staging").to_string_lossy().into_owned();
    let hung = format!("/test/hung-output-{}", std::process::id());
    let hung_key = hung.clone();
    let limit = std::time::Duration::from_millis(300);
    let started = std::time::Instant::now();
    let out = ensure_dirs_bounded(vec![good.clone(), hung.clone()], limit, move |p| {
        if p == std::path::Path::new(&hung_key) {
            std::thread::sleep(std::time::Duration::from_secs(3));
            return Ok(());
        }
        std::fs::create_dir_all(p)
    });
    assert!(
        started.elapsed() < std::time::Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], (good.clone(), Ok(())));
    assert!(std::path::Path::new(&good).is_dir());
    assert_eq!(out[1].0, hung);
    let reason = out[1].1.as_ref().unwrap_err();
    assert!(reason.contains("not responding"), "{reason}");
}
