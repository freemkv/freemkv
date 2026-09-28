//! The server shell: the unattended rip daemon with a web UI (`--features server`).
//!
//! `freemkv server` runs it (see [`daemon::run`]). The code arrived from the
//! autorip daemon and keeps its behaviour, its `/config` layout and its
//! `AUTORIP_*` environment names for now.
//!
//! One module graph: the daemon entry and the integration tests under
//! `tests/server/` both reach these modules through `freemkv::server`.

use std::sync::atomic::AtomicBool;

/// Set by SIGTERM/SIGINT; every worker loop polls it to drain and exit.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Full build label: package version + git short hash (e.g. `1.1.1 (g2014a41)`),
/// the same shape libfreemkv stamps into every MKV. Surfaced in `--version`,
/// the UI footer, `/api/version`, and the startup log so the running build is
/// always identifiable. Built by `build.rs`.
pub const VERSION_LABEL: &str = concat!(env!("SERVER_VERSION"), env!("SERVER_GIT_SUFFIX"));

pub mod config;
pub mod daemon;
pub mod keysource;
pub mod library;
pub mod log;
pub mod mover;
pub mod muxer;
pub mod observe;
pub mod review;
pub mod ripper;
pub mod settings_schema;
pub mod tmdb;
pub mod util;
pub mod web;
pub mod webhook;

pub use daemon::run;
