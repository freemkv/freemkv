//! The front ends' one key rule (pipeline design §2.5; anti-drift measures 1–3). The CLI,
//! the desktop app and the server each build the engine's `Plan` directly, taking its keys
//! from [`key_params`], so which key sources a request asks is written once.
//!
//! Every front end also renders the plan it runs through its own exhaustive destructure of
//! `Plan` (no `..`): a field added to the engine's `Plan` fails to compile until the CLI,
//! the app and the server each handle it.

use freemkv_engine::KeyParamsData;

/// Which key sources a request may ask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeyMode {
    /// The local keydb, then the online service when one is configured.
    #[default]
    Both,
    /// Only the local keydb.
    LocalOnly,
    /// Only the online service.
    OnlineOnly,
}

/// Where a request's keys are looked up (never the keys themselves).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeySettings {
    /// The local keydb, already resolved to a path by the front end.
    pub keydb_path: Option<String>,
    /// The online key service URL.
    pub key_url: Option<String>,
    /// Its bearer token.
    pub key_auth: Option<String>,
    /// Which of the two the request may ask.
    pub mode: KeyMode,
}

/// The CLI's key flags (`--keydb`, `--key-url`, `--key-auth`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CliKeys {
    pub keydb: Option<String>,
    pub key_url: Option<String>,
    pub key_auth: Option<String>,
}

/// The CLI's key flags as settings: `--key-url` with no `--keydb` asks only the key service;
/// otherwise the keydb (`--keydb`, else `default_keydb`) and then the service. The keydb
/// path also supplies the drive handshake's host certificates.
pub fn cli_key_settings(k: &CliKeys, default_keydb: &str) -> KeySettings {
    let online_only = k.key_url.is_some() && k.keydb.is_none();
    KeySettings {
        keydb_path: Some(k.keydb.clone().unwrap_or_else(|| default_keydb.to_string())),
        key_url: k.key_url.clone(),
        key_auth: k.key_auth.clone(),
        mode: if online_only {
            KeyMode::OnlineOnly
        } else {
            KeyMode::Both
        },
    }
}

/// The key-source parameters of `k`, by one rule for every front end: the keydb (skipped
/// by the engine when the request is online-only), the online service and its token unless
/// the request is local-only or has no URL. The drive handshake takes its host
/// certificates from the keydb path in every mode.
pub fn key_params(k: &KeySettings) -> KeyParamsData {
    let nonempty = |s: &Option<String>| s.as_ref().filter(|s| !s.trim().is_empty()).cloned();
    let url = nonempty(&k.key_url).filter(|_| k.mode != KeyMode::LocalOnly);
    let online_only = k.mode == KeyMode::OnlineOnly;
    KeyParamsData {
        keydb_path: nonempty(&k.keydb_path),
        key_auth: url.as_ref().and_then(|_| nonempty(&k.key_auth)),
        key_url: url,
        online_only,
        cert_keydb: nonempty(&k.keydb_path),
    }
}

#[cfg(test)]
#[path = "plan_core_tests.rs"]
mod tests;
