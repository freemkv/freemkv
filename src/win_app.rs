//! The Windows desktop entry point — the one function both Windows binaries
//! open the shell through.
//!
//! `freemkv-gui.exe` (windows-subsystem, no console) calls straight into
//! `run` below; `freemkv.exe` keeps the byte-for-byte CLI contract and
//! reaches the same function via `freemkv gui`.
//!
//! This lives in the **lib**, not in a bin, so both binaries can call it, and
//! is `cfg(target_os = "windows")` so it compiles to nothing elsewhere.
//! See docs/win-app.md for why the two-binary split exists at all.

/// Open the Windows desktop shell. Does not return until the window closes.
pub fn run() {
    let (cfg, loaded) = crate::settings::Settings::load_reporting();

    crate::app_entry::apply_locale(&cfg.language, crate::windows::system_locale_code);
    crate::app_entry::init_gui_logging(&cfg.log_level);
    // Again, now that there is somewhere for it to go: the subscriber is
    // configured from `cfg.log_level`, so the warning `load_reporting` already
    // emitted had no subscriber to receive it.
    loaded.warn();

    crate::windows::run();
}
