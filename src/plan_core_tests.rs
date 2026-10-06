use super::*;

fn keys(mode: KeyMode) -> KeySettings {
    KeySettings {
        keydb_path: Some("/k/keydb.cfg".into()),
        key_url: Some("https://keys.example/keys".into()),
        key_auth: Some("token".into()),
        mode,
    }
}

#[test]
fn one_key_rule_for_every_front_end() {
    let both = key_params(&keys(KeyMode::Both));
    assert_eq!(both.keydb_path.as_deref(), Some("/k/keydb.cfg"));
    assert_eq!(both.key_url.as_deref(), Some("https://keys.example/keys"));
    assert_eq!(both.key_auth.as_deref(), Some("token"));
    assert!(!both.online_only);

    let local = key_params(&keys(KeyMode::LocalOnly));
    assert!(local.key_url.is_none() && local.key_auth.is_none());
    assert!(local.keydb_path.is_some());

    let online = key_params(&keys(KeyMode::OnlineOnly));
    assert!(online.online_only, "the engine then skips the keydb");
    assert_eq!(
        online.cert_keydb.as_deref(),
        Some("/k/keydb.cfg"),
        "the handshake still reads host certificates"
    );
}

#[test]
fn an_empty_url_or_token_asks_nothing_online() {
    let k = KeySettings {
        key_url: Some("  ".into()),
        key_auth: Some("token".into()),
        ..keys(KeyMode::Both)
    };
    let p = key_params(&k);
    assert!(p.key_url.is_none() && p.key_auth.is_none());
    let k = KeySettings {
        key_auth: Some(String::new()),
        ..keys(KeyMode::Both)
    };
    assert!(key_params(&k).key_auth.is_none());
}
