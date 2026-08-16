//! Library facade for the freemkv-gui binary.
//!
//! Exists so integration tests under `tests/` can reach the same modules the
//! binary uses. Keep purely declarative — no logic lives here.
//!
//! The AppKit shell is deliberately absent: `settings` and `engine` must build
//! and be testable on any platform, which is what keeps the logic out of the
//! platform-specific half. The Win32 shell below does not break that rule —
//! it is `cfg(target_os = "windows")`, so everywhere the rule is checked
//! (macOS, Linux CI's portable-core build) it compiles to nothing at all.

/// The launch decision plus the two steps every desktop launch performs first.
/// Portable and Win32/AppKit-free on purpose — the routing is the part that
/// can be wrong without a window to look at, so it must be testable here.
pub mod app_entry;
pub mod engine;
/// Whether two paths name the same file — the ONE definition both shells use
/// to refuse a rip whose destination is its own source. Declared here as well
/// as in `main.rs` (same reason as `title_identity` below): the CLI's `pipe`
/// and the GUI's `engine` are separate compilations, and each having its own
/// answer is how the GUI came to miss a hardlinked destination the CLI
/// refuses.
pub mod file_identity;
/// The hardened keydb downloader (SSRF guard, zero redirects, body cap).
/// Declared here as well as in `main.rs` because `settings.rs` — the GUI's
/// Update-keydb path — lives in this tree and could not otherwise reach it.
pub mod keydb_fetch;
/// What a finished mux still has to tell the user — the ONE renderer both
/// shells use for a completed-but-lossy export. Declared here as well as in
/// `main.rs`, same reason as `title_identity`.
pub mod lossy;
/// The Level↔code messaging standard. Declared here as well as in `main.rs`
/// (same reason as `keydb_fetch` above) so `tests/messaging_contract.rs` can
/// call the REAL `level_for` instead of keeping its own copy-pasted stub —
/// which is exactly what it did before this: a local `fn level_for` that
/// always returned `Level::Error` regardless of its argument, asserted
/// against the literal it had just returned. That assertion could never fail,
/// no matter what the real `messaging::level_for` did.
pub mod messaging;
pub mod platform;
pub mod settings;
// The i18n string facade — the core (`ui`) localizes through it, so the lib
// target needs it too (it is just a re-export of `freemkv_i18n`).
pub mod strings;
/// What a title NUMBER refers to across a re-scan — the ONE definition both
/// shells share. Declared here as well as in `main.rs` (same reason as
/// `strings`): the CLI's `pipe` and the GUI's `engine` are separate
/// compilations, and each having its own answer to "is this the same title?"
/// is exactly how they came to disagree.
pub mod title_identity;
pub mod ui;
// The Windows shell's DPI→geometry arithmetic. Pure, Win32-free and therefore
// deliberately NOT `cfg(windows)`: gating it would make its unit tests
// unrunnable anywhere but Windows, and the arithmetic is the part most likely
// to be wrong.
pub mod win_layout;

// ── Win32 shell — WINDOWS ONLY ──────────────────────────────────────────────
// The shell itself lives here rather than in `src/main.rs` for one structural
// reason: `freemkv-gui.exe` is a *second binary*, and one bin cannot call
// another's modules. Hosting it in the lib gives both images the same shell
// from one compilation — `main.rs` deliberately no longer declares
// `mod windows`, so `windows.rs` (4.4k lines + winsafe) is built exactly once
// per target, not once per binary.
#[cfg(target_os = "windows")]
pub mod win_app;
#[cfg(target_os = "windows")]
pub mod windows;
