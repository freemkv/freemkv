//! `freemkv-gui.exe` — the image a Windows user double-clicks.
//!
//! Windows has no `.app` bundle, so "run the desktop app" cannot be a property
//! of *how* `freemkv.exe` was started — an Explorer double-click and a `cmd`
//! invocation look identical to the process. It has to be a property of *which
//! image* was started. Hence two binaries from one crate:
//!
//! ```text
//! freemkv.exe      console subsystem   the CLI contract, unchanged
//!                                      (`freemkv gui` still opens the window)
//! freemkv-gui.exe  windows subsystem   double-click → the window, no console
//! ```
//!
//! `#![windows_subsystem = "windows"]` is the whole reason this file exists as
//! a separate binary rather than a flag on the other one: the subsystem is a PE
//! header field chosen at link time, so it cannot be decided at runtime. Without
//! it, double-clicking flashes a console window behind the app and leaves it
//! there for the session.
//!
//! The shell itself is NOT duplicated here — it lives in the lib
//! (`freemkv::win_app` → `freemkv::windows`), which is also what `freemkv gui`
//! calls, so the two entry points can never drift.
//!
//! Cargo cannot make a `[[bin]]` target-conditional (there is no
//! `[target.'cfg(windows)'.bin]`), so the *contents* carry the gate: on macOS
//! and Linux this compiles to an empty `main` and links nothing platform
//! specific. Only the Windows build produces a real program.
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
