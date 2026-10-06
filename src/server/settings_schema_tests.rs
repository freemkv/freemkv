use super::*;

#[test]
fn every_config_field_but_the_bootstrap_ones_is_in_the_schema() {
    let v = serde_json::to_value(Config::default()).unwrap();
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    for k in &keys {
        if *k == "autorip_dir" {
            continue;
        }
        assert!(get(k).is_some(), "Config::{k} has no schema field");
    }
    for f in FIELDS.iter().filter(|f| f.kind.stored()) {
        assert!(
            keys.contains(&f.key),
            "schema field {} is not a Config field",
            f.key
        );
    }
}

#[test]
fn every_choice_says_what_each_option_does() {
    for f in FIELDS {
        let opts: &[Opt] = match f.kind {
            Kind::Choice(o) => o,
            Kind::RipMode => RIP_MODE,
            _ => continue,
        };
        for (v, l, h) in opts {
            assert!(
                !l.is_empty() && !h.is_empty(),
                "{}={v} needs a label and a line",
                f.key
            );
        }
    }
    let s = schema_json();
    let insert = s["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "on_insert")
        .unwrap();
    assert_eq!(insert["options"][2]["help"], "Always start a fresh rip.");
}

#[test]
fn keys_are_unique_and_labelled() {
    for (i, f) in FIELDS.iter().enumerate() {
        assert!(
            FIELDS[..i].iter().all(|g| g.key != f.key),
            "duplicate {}",
            f.key
        );
        let per_option = matches!(f.kind, Kind::Choice(_) | Kind::RipMode);
        assert!(per_option || !f.help.is_empty(), "{} has no help", f.key);
        if !matches!(f.kind, Kind::Action { .. }) {
            assert!(!f.label.is_empty(), "{} has no label", f.key);
        }
    }
}

#[test]
fn conditions_name_real_fields_and_values() {
    for f in FIELDS {
        for w in f.show_if.iter().chain(f.hide_if.iter()) {
            let other = get(w.key).unwrap_or_else(|| panic!("{} depends on {}", f.key, w.key));
            let values: &[Opt] = match other.kind {
                Kind::Choice(o) => o,
                Kind::RipMode => RIP_MODE,
                _ => panic!("{} depends on a non-choice {}", f.key, w.key),
            };
            assert!(
                values.iter().any(|(v, _, _)| *v == w.value),
                "{}: {} has no {}",
                f.key,
                w.key,
                w.value
            );
        }
    }
}

#[test]
fn defaults_are_the_documented_first_boot_values() {
    let c = Config::default();
    assert_eq!(c.staging_dir, "/staging");
    assert_eq!(c.output_dir, "/output");
    assert_eq!(c.on_insert, "scan");
    assert_eq!(c.max_retries, 1);
    assert_eq!(c.max_rip_duration_secs, 28_800);
    assert_eq!(c.min_pass_budget_secs, 5_400);
    assert_eq!(c.log_retention_days, 30);
    assert!(c.main_feature && c.tv_auto && c.auto_eject);
    assert_eq!(c.keydb_path, None);
    assert_eq!(c.port, 8080);
    assert_eq!(c.autorip_dir, "/config");
}

#[test]
fn an_unused_network_target_does_not_block_a_save() {
    // A target validation refuses (unspecified address).
    let lan = "0.0.0.0:9000".to_string();
    let c = Config {
        output_format: "mkv".into(),
        network_target: lan.clone(),
        ..Config::default()
    };
    let body = json!({"network_target": lan, "auto_eject": false});
    assert!(
        parse_patch(&body, &c).is_ok(),
        "mkv output: the target is not checked"
    );
    let body = json!({"network_target": lan, "output_format": "network"});
    assert!(
        parse_patch(&body, &c)
            .unwrap_err()
            .contains("network_target")
    );
}

#[test]
fn dead_settings_load_but_are_not_on_the_form() {
    let s = schema_json();
    let keys: Vec<&str> = s["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["key"].as_str().unwrap())
        .collect();
    assert!(!keys.contains(&"max_rip_duration_secs"));
    assert!(!keys.contains(&"min_pass_budget_secs"));
    assert!(
        !s["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["id"] == "Hidden")
    );
    let mut c = Config::default();
    load_into(&mut c, &json!({"max_rip_duration_secs": 100}));
    assert_eq!(c.max_rip_duration_secs, 100);
}

#[test]
fn a_broken_rule_refuses_the_save_but_a_wrong_type_is_skipped() {
    let mut c = Config::default();
    let body = json!({"auto_eject": false, "output_format": "garbage"});
    assert_eq!(
        parse_patch(&body, &c).unwrap_err(),
        "invalid value for output_format"
    );
    // 1.7.7 skipped a mistyped field and saved the rest.
    let body = json!({"max_retries": "three", "auto_eject": false, "decrypt_threads": -5});
    let p = parse_patch(&body, &c).unwrap();
    apply(&mut c, &p);
    assert!(!c.auto_eject);
    assert_eq!(c.max_retries, Config::default().max_retries);
    assert_eq!(c.decrypt_threads, Config::default().decrypt_threads);
}

#[test]
fn apply_keeps_the_port_and_maps_rip_mode() {
    let mut c = Config {
        port: 9123,
        max_retries: 3,
        ..Config::default()
    };
    let p = parse_patch(&json!({"rip_mode": "single", "auto_eject": false}), &c).unwrap();
    apply(&mut c, &p);
    assert_eq!((c.port, c.max_retries, c.auto_eject), (9123, 0, false));
    let p = parse_patch(&json!({"rip_mode": "multi"}), &c).unwrap();
    apply(&mut c, &p);
    assert_eq!(c.max_retries, 1);
}

#[test]
fn redaction_masks_every_secret_kind() {
    let c = Config {
        tmdb_api_key: "k".into(),
        keyserver_secret: "s".into(),
        keyserver_url: "https://h.example/tok/decode".into(),
        keydb_path: Some("/secret/place/keydb.cfg".into()),
        ..Config::default()
    };
    let v = redacted(&c);
    assert_eq!(v["tmdb_api_key"], SECRET_SENTINEL);
    assert_eq!(v["keyserver_secret"], SECRET_SENTINEL);
    assert_eq!(
        v["keyserver_url"],
        format!("https://h.example/{SECRET_SENTINEL}")
    );
    assert_eq!(v["keydb_path"], "keydb.cfg");
    assert_eq!(v["rip_mode"], "multi");
    assert!(v["keydb_resolved"].as_str().unwrap().contains("keydb.cfg"));
}

#[test]
fn the_schema_json_carries_what_the_form_needs() {
    let s = schema_json();
    let f = s["fields"].as_array().unwrap();
    let fmt = f.iter().find(|f| f["key"] == "output_format").unwrap();
    assert_eq!(fmt["type"], "choice");
    assert_eq!(fmt["options"].as_array().unwrap().len(), 4);
    assert_eq!(fmt["default"], "mkv");
    let mode = f.iter().find(|f| f["key"] == "rip_mode").unwrap();
    assert_eq!(mode["options"][1]["value"], "multi");
    assert_eq!(s["groups"].as_array().unwrap().len(), Group::ALL.len());
}

#[test]
fn an_untouched_form_keeps_the_masked_values() {
    let mut c = Config {
        tmdb_api_key: "k".into(),
        keyserver_secret: "s".into(),
        keyserver_url: "https://h.example/tok/decode".into(),
        keydb_path: Some("/secret/place/keydb.cfg".into()),
        ..Config::default()
    };
    let before = c.clone();
    let body = redacted(&c);
    let p = parse_patch(&body, &c).unwrap();
    apply(&mut c, &p);
    assert_eq!(c.tmdb_api_key, before.tmdb_api_key);
    assert_eq!(c.keyserver_secret, before.keyserver_secret);
    assert_eq!(c.keyserver_url, before.keyserver_url);
    assert_eq!(c.keydb_path, before.keydb_path);
}

#[test]
fn save_time_path_rules_refuse_bad_paths() {
    let c = Config::default();
    for body in [
        json!({"staging_dir": "../../etc"}),
        json!({"output_dir": "relative"}),
        json!({"output_dir": "/a/../b"}),
        json!({"movie_dir": "../escape"}),
        json!({"keydb_path": "relative/keydb.cfg"}),
        json!({"keydb_path": "/a/../keydb.cfg"}),
        json!({"keydb_path": "/a/keydb.txt"}),
    ] {
        assert!(parse_patch(&body, &c).is_err(), "{body} must be refused");
    }
    assert!(
        parse_patch(
            &json!({"staging_dir": "/ok/stage", "keydb_path": "/k/keydb.cfg"}),
            &c
        )
        .is_ok()
    );
}

#[test]
fn the_legacy_abort_flag_maps_to_on_read_error() {
    let mut c = Config::default();
    let p = parse_patch(&json!({"abort_on_error": true}), &c).unwrap();
    apply(&mut c, &p);
    assert_eq!(c.on_read_error, "stop");
    let p = parse_patch(&json!({"abort_on_error": false}), &c).unwrap();
    apply(&mut c, &p);
    assert_eq!(c.on_read_error, "skip");
    // An explicit on_read_error wins over the old flag.
    let p = parse_patch(
        &json!({"abort_on_error": true, "on_read_error": "skip"}),
        &c,
    )
    .unwrap();
    apply(&mut c, &p);
    assert_eq!(c.on_read_error, "skip");

    let mut c = Config::default();
    load_into(&mut c, &json!({"abort_on_error": true}));
    assert_eq!(c.on_read_error, "stop");
    load_into(&mut c, &json!({"abort_on_error": false}));
    assert_eq!(c.on_read_error, "skip");
    load_into(
        &mut c,
        &json!({"abort_on_error": true, "on_read_error": "skip"}),
    );
    assert_eq!(c.on_read_error, "skip");
}

#[test]
fn numbers_clamp_ports_validate_and_bad_loads_keep_the_default() {
    let mut c = Config::default();
    let p = parse_patch(&json!({"log_retention_days": 999_999_999u64}), &c).unwrap();
    apply(&mut c, &p);
    assert_eq!(c.log_retention_days, MAX_RETENTION_DAYS);

    for port in [0u64, 65_536, 70_000] {
        assert!(
            parse_patch(&json!({"port": port}), &c).is_err(),
            "port {port}"
        );
    }
    let p = parse_patch(&json!({"port": 65_535}), &c).unwrap();
    apply(&mut c, &p);
    assert_eq!(c.port, 65_535);

    let mut c = Config::default();
    load_into(
        &mut c,
        &json!({"on_insert": "bogus", "log_retention_days": 7}),
    );
    assert_eq!(c.on_insert, Config::default().on_insert);
    assert_eq!(c.log_retention_days, 7);
}
