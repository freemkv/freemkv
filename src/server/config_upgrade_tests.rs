// Upgrade from v1.8.3: the settings.json it wrote boots this build unchanged.
use super::*;
use crate::server::upgrade_fixture as fx;

// Boot's settings step, reading `settings.json` from `dir`.
fn boot(dir: &std::path::Path) -> Config {
    load_saved(build_bootstrap_config(
        8080,
        dir.to_string_lossy().into_owned(),
    ))
}

fn write_settings(dir: &std::path::Path, body: &serde_json::Value) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_vec_pretty(body).unwrap(),
    )
    .unwrap();
}

#[test]
fn v1_8_3_settings_load_every_field_unchanged() {
    let (_t, root) = fx::root();
    fx::install("v1.8.3/config/settings.json", &root, "config/settings.json");
    let saved: serde_json::Value =
        serde_json::from_str(&fx::text("v1.8.3/config/settings.json", &root)).unwrap();
    let cfg = boot(&root.join("config"));
    let loaded = serde_json::to_value(&cfg).unwrap();
    let saved = saved.as_object().unwrap();
    assert_eq!(
        saved.len(),
        36,
        "every field v1.8.3 serialises is in the fixture"
    );
    for (key, value) in saved {
        assert_eq!(&loaded[key], value, "{key} changed across the upgrade");
    }
    // Fields v1.8.3 never wrote take their defaults.
    assert_eq!(cfg.presentation_language, "");
    assert_eq!(cfg.webhook_urls.len(), 2);
    assert_eq!(
        cfg.webhook_urls[0]
            .headers
            .get("Authorization")
            .map(String::as_str),
        Some("MediaBrowser Token=\"00000000000000000000000000000000\"")
    );
    // Saved again by this build and reloaded, nothing drifts.
    save(&cfg).unwrap();
    let again = serde_json::to_value(boot(&root.join("config"))).unwrap();
    assert_eq!(again, loaded);
}

#[test]
fn unknown_and_removed_fields_are_tolerated() {
    let (_t, root) = fx::root();
    let mut saved: serde_json::Value =
        serde_json::from_str(&fx::text("v1.8.3/config/settings.json", &root)).unwrap();
    let m = saved.as_object_mut().unwrap();
    m.insert("port".into(), serde_json::json!(9999));
    m.insert("abort_on_error".into(), serde_json::json!(false));
    m.insert(
        "a_field_from_the_future".into(),
        serde_json::json!({"x": 1}),
    );
    m.insert(
        "webhook_urls".into(),
        serde_json::json!(["https://old.example.invalid/hook"]),
    );
    let dir = root.join("config");
    write_settings(&dir, &saved);
    let cfg = boot(&dir);
    assert_eq!(cfg.port, 8080, "port stays bootstrap-only");
    assert_eq!(cfg.staging_dir, root.join("staging").to_string_lossy());
    assert_eq!(cfg.tmdb_api_key, "0123456789abcdef0123456789abcdef");
    assert_eq!(
        cfg.on_read_error, "stop",
        "on_read_error wins over abort_on_error"
    );
    assert_eq!(cfg.webhook_urls.len(), 1);
    assert_eq!(cfg.webhook_urls[0].url, "https://old.example.invalid/hook");
}

#[test]
fn any_presentation_language_value_boots_and_plans() {
    let (_t, root) = fx::root();
    for value in [None, Some("all"), Some("none"), Some(""), Some("de")] {
        let mut saved: serde_json::Value =
            serde_json::from_str(&fx::text("v1.8.3/config/settings.json", &root)).unwrap();
        if let Some(v) = value {
            saved["presentation_language"] = serde_json::json!(v);
        }
        let dir = root.join(format!("config-{}", value.unwrap_or("absent")));
        write_settings(&dir, &saved);
        let cfg = boot(&dir);
        assert_eq!(
            cfg.tmdb_api_key, "0123456789abcdef0123456789abcdef",
            "{value:?}"
        );
        assert!(
            crate::ui::valid_presentation_language(&cfg.presentation_language),
            "{value:?} loaded as {:?}",
            cfg.presentation_language
        );
        let plan = crate::server::planner::plan(
            &cfg,
            &crate::selection_test_fixtures::variant_titles(),
            &crate::server::planner::MediaMetadata::default(),
            std::path::Path::new("/m/A/A.mkv"),
        );
        assert!(plan.is_ok(), "{value:?}: {plan:?}");
    }
}
