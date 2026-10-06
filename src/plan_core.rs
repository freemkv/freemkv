//! The front ends' one way to an engine `Plan` (pipeline design §2.5; anti-drift measures
//! 1–3). The CLI's flags, the desktop app's settings and the server's config each become a
//! `PlanRequest`; `plan` is the one parser that turns it into the engine's `Plan`, so a
//! rule (which key sources a request asks, what "raw" means) is written once.
//!
//! Every front end also renders the plan it runs through its own exhaustive destructure of
//! `Plan` (no `..`): a field added to the engine's `Plan` fails to compile until the CLI,
//! the app and the server each handle it.

use freemkv_engine::{KeyParamsData, Plan, Selection, StreamChoice};

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

/// A front end's request, in front-end-neutral terms.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanRequest {
    pub source: String,
    pub dest: String,
    pub titles: Selection,
    pub streams: StreamChoice,
    pub raw: bool,
    pub multipass: bool,
    pub keys: KeySettings,
    pub force: bool,
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

/// The CLI's whole-disc invocation (`freemkv SOURCE DEST [--raw] [--multipass] [--force]`)
/// as a request.
pub fn cli_request(
    source: &str,
    dest: &str,
    flags: (bool, bool, bool),
    keys: KeySettings,
) -> PlanRequest {
    let (raw, multipass, force) = flags;
    PlanRequest {
        source: source.to_string(),
        dest: dest.to_string(),
        titles: Selection::default(),
        streams: StreamChoice::default(),
        raw,
        multipass,
        keys,
        force,
    }
}

/// The one parser: a [`PlanRequest`] as the engine's [`Plan`].
pub fn plan(req: PlanRequest) -> Plan {
    let PlanRequest {
        source,
        dest,
        titles,
        streams,
        raw,
        multipass,
        keys,
        force,
    } = req;
    Plan {
        source,
        dest,
        titles,
        streams,
        raw,
        multipass,
        keys: key_params(&keys),
        force,
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
