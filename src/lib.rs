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
pub mod ui;
