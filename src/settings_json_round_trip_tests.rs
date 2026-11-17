use super::Settings;

/// Saving settings and loading them back must give the same settings, even
/// for long numbers in a float field or in an unknown key kept in `extra`.
/// Without exact float parsing the reload is one bit off and never equal.
fn assert_round_trips(json: &str) {
    let settings: Settings = serde_json::from_str(json).unwrap();
    let reloaded: Settings =
        serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
    assert!(
        settings == reloaded,
        "settings changed after save/load: {json}"
    );
}

/// Fuzz crash input: an overlong window width.
#[test]
fn long_window_width_survives_save_and_load() {
    let digits = format!("2{}{}{}66", "5".repeat(19), "8".repeat(33), "5".repeat(68));
    assert_round_trips(&format!("{{\"win_w\":{digits}}}"));
}

/// Fuzz crash input: an overlong number in an unknown key.
#[test]
fn long_number_in_unknown_key_survives_save_and_load() {
    assert_round_trips(&format!("{{\"\":4{}7}}", "4".repeat(110)));
    assert_round_trips("{\"AAI0\\\\\":1111111111111111111111115}");
}
