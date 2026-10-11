//! The server shell: the unattended rip daemon with a web UI (`--features server`).
//!
//! `freemkv server` runs it (see [`daemon::run`]). The code arrived from the
//! autorip daemon and keeps its behaviour, its `/config` layout and its
//! `AUTORIP_*` environment names for now.
//!
//! One module graph: the daemon entry and the integration tests under
//! `tests/server/` both reach these modules through `freemkv::server`.

use std::sync::atomic::AtomicBool;

/// Set by SIGTERM/SIGINT or a reboot request; every worker loop polls it to drain.
pub static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Sleep up to `total`, waking early once [`SHUTDOWN`] is set; whether it is.
pub(crate) fn sleep_unless_shutdown(total: std::time::Duration) -> bool {
    use std::sync::atomic::Ordering;
    let until = std::time::Instant::now() + total;
    while !SHUTDOWN.load(Ordering::Acquire) {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(std::time::Duration::from_millis(200)));
    }
    true
}

/// Set only by SIGTERM/SIGINT: the process must stop. It vetoes the relaunch a
/// reboot would do.
pub static TERMINATE: AtomicBool = AtomicBool::new(false);

/// Set by the System-panel reboot. After the bounded drain the daemon relaunches
/// itself: exec of the startup binary on Linux, exit 75 for the service manager
/// elsewhere.
pub static REBOOT: AtomicBool = AtomicBool::new(false);

/// Full build label: package version + git short hash (e.g. `1.1.1 (g2014a41)`),
/// the same shape libfreemkv stamps into every MKV. Surfaced in `--version`,
/// the UI footer, `/api/version`, and the startup log so the running build is
/// always identifiable. Built by `build.rs`.
pub const VERSION_LABEL: &str = concat!(env!("SERVER_VERSION"), env!("SERVER_GIT_SUFFIX"));

pub mod config;
pub mod daemon;
pub mod health;
pub mod keysource;
pub mod library;
pub mod log;
pub mod mover;
pub mod muxer;
pub mod observe;
pub mod peers;
pub mod planner;
pub mod review;
pub mod ripper;
pub mod settings_schema;
pub mod tmdb;
pub mod util;
pub mod web;
pub mod webhook;

pub use daemon::run;
