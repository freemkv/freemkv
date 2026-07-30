//! The Windows desktop entry point — the one function both Windows binaries
//! open the shell through.
//!
//! Windows has no bundle format, so there is no "double-clicked the app" state
//! for `freemkv.exe` to detect: a console-subsystem `.exe` started from
//! Explorer looks exactly like one started from `cmd`, and guessing (parent
//! process, console ownership) would make the CLI contract depend on how the
//! user's terminal spawned it. The shipped answer is therefore a *second image*
//! — `freemkv-gui.exe`, windows-subsystem, no console — which calls straight
//! into `run` below. `freemkv.exe` keeps the byte-for-byte CLI contract and
//! still reaches the same function via `freemkv gui`.
//!
//! This lives in the **lib**, not in a bin, because a bin cannot be called from
//! another bin. It is `cfg(target_os = "windows")`, so on macOS and Linux it —
//! and `crate::windows` with it — compiles to nothing, which is what keeps the
//! lib's "`settings` and `engine` build and test on any platform" rule true.

/// Open the Windows desktop shell. Does not return until the window closes.
pub fn run() {
    let cfg = crate::settings::Settings::load();

    crate::app_entry::apply_locale(&cfg.language, crate::windows::system_locale_code);
    crate::app_entry::init_gui_logging(&cfg.log_level);

    crate::windows::run();
}
