//! Library facade for the freemkv-gui binary.
//!
//! Exists so integration tests under `tests/` can reach the same modules the
//! binary uses. Keep purely declarative — no logic lives here.
//!
//! The AppKit shell is deliberately absent: `settings` and `engine` must build
//! and be testable on any platform, which is what keeps the logic out of the
//! platform-specific half.

pub mod engine;
pub mod platform;
pub mod settings;
// The i18n string facade — the core (`ui`) localizes through it, so the lib
// target needs it too (it is just a re-export of `freemkv_i18n`).
pub mod strings;
pub mod ui;
