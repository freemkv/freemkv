//! Linux GUI entry point. The exact analogue of `mac.rs` / `win_app.rs`:
//! load settings, apply the "Auto" language, install the GUI tracing
//! subscriber, then hand off to the GTK4 shell in `linux.rs`.

/// Called from `main.rs` on Linux when `wants_gui` returns true. Blocks
/// until the window is closed; the return value is the exit status GTK's
/// application runner produced (`0` on normal exit).
pub fn run() -> i32 {
    let (cfg, loaded) = crate::settings::Settings::load_reporting();

    // "Auto" leaves the choice to freemkv-i18n's own POSIX/GNU `LANGUAGE`
    // detection, the one the CLI uses; Flatpak forwards the locale variables.
    crate::app_entry::apply_locale(&cfg.language, || None);

    crate::app_entry::init_gui_logging(&cfg.log_level);
    // Any settings-file warning was queued before the tracing subscriber was
    // installed; emit it now so it lands in the same log the shell writes to.
    loaded.warn();

    crate::linux::run()
}
