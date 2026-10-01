//! The Windows desktop entry point — the one function both Windows binaries
//! open the shell through.
//!
//! The windowed image (`freemkv.exe`, windows subsystem) calls straight into
//! `run` below. The console image (`freemkv.com`, the CLI) starts that sibling
//! on `freemkv gui` and only falls back to `run` in-process when none is found.
//!
//! This lives in the **lib**, not in a bin, so both binaries can call it, and is
//! `cfg(all(feature = "gui", target_os = "windows"))` so it compiles to nothing elsewhere.

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
