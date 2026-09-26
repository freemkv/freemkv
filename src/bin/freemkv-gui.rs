//! The windowed image of the Windows app, shipped as `freemkv.exe`.
//!
//! Two binaries from one crate, distinguished by PE subsystem:
//!
//! ```text
//! freemkv.com  console subsystem   the `freemkv` bin: CLI for any arguments,
//!                                  bare launch starts this image and returns
//! freemkv.exe  windows subsystem   this bin: double-click → the window
//! ```
//!
//! No argument parsing here — the CLI lives in the console image. The window
//! shell lives in `freemkv::win_app` / `freemkv::windows`, shared by both, so
//! the two entry points can't drift. App build only (`required-features`); on
//! non-Windows targets this compiles to an empty `main`.
#![windows_subsystem = "windows"]

// Match `freemkv.exe`: the GUI does the same large, highly concurrent buffer
// churn during a rip, and the two images must not have different allocator
// behaviour.
#[cfg(target_os = "windows")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// No dispatcher and no argument parsing: this image has exactly one job.
/// Anything a user could pass on a command line belongs to `freemkv.exe`, which
/// is the documented CLI and is what a shell completes on.
#[cfg(target_os = "windows")]
fn main() {
    freemkv::win_app::run();
}

/// Not a Windows build: nothing to launch. See the module docs — Cargo has no
/// per-target `[[bin]]` gate, so the guard is here.
#[cfg(not(target_os = "windows"))]
fn main() {}
