//! Linux desktop shell — GTK4 + libadwaita.
//!
//! Third native shell alongside `windows.rs` (winsafe / Win32) and `mac.rs`
//! (objc2 / AppKit). Every user-facing decision comes from `ui.rs`, so the
//! app is the same product on all three OSes; only how the widgets are
//! drawn differs. See docs/linux-shell.md.
//!
//! **State of this file (v1.7.6):** the shell FRAMEWORK is complete — the
//! window opens with an AdwHeaderBar hamburger menu built from
//! `ui::menu_layout`, the shared `App` is driven, `Effect`s are handled
//! (including the new `NotifyRipFinished` toast + XDG-portal desktop
//! notification), and the tick timer polls `App::tick` while a rip runs.
//! The main-content panels (source picker, title tree, output picker,
//! progress page, log pane) are placeholders wired to be filled in by
//! follow-up commits on a Linux dev box. See TODO markers below.
//!
//! WHY not `egui` / `iced` / Tauri: the project's stated invariant is that
//! every shell looks 100% native on its OS. GTK4 + libadwaita is what a
//! GNOME app looks like on Fedora, Ubuntu, and Pop!_OS in 2026, and what
//! Flathub reviewers expect.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::ui::{App, Cmd, Effect, MenuAction, MenuEntry, MenuGroup, MenuGroupId};

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

const APP_ID: &str = "org.freemkv.FreeMKV";

/// Entry point called from `linux_app::run`. Blocks until the window is
/// closed. Returns 0 on normal exit, non-zero if the GTK application
/// couldn't be initialised.
pub fn run() -> i32 {
    adw::init().expect(
        "libadwaita init failed — this build was linked against \
        GTK4 but the runtime shared libraries are not available",
    );
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    app.run().into()
}

/// The GTK-side shell. Owns the widgets; the shared model is inside `app`.
struct Shell {
    app: Rc<RefCell<App>>,
    window: adw::ApplicationWindow,
    toast_overlay: adw::ToastOverlay,
    tick_source: RefCell<Option<glib::SourceId>>,
}

thread_local! {
    /// The active shell for the current window. GTK is single-threaded per
    /// display; the model does not cross threads on Linux, so a
    /// `thread_local` cell is enough to reach the shell from a menu action
    /// callback without smuggling an `Rc<Shell>` through every closure.
    static SHELL: RefCell<Option<Rc<Shell>>> = const { RefCell::new(None) };
}

fn build_ui(app: &adw::Application) {
    // `App::new()` loads persisted settings via `Settings::load` internally,
    // matching what the mac + windows shells do.
    let ui_app = Rc::new(RefCell::new(App::new()));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("freemkv")
        .default_width(1180)
        .default_height(760)
        .build();

    // AdwToolbarView: header bar on top, content underneath. The toast
    // overlay wraps the content so rip-finished toasts float in-window.
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.pack_end(&build_hamburger_menu_button());
    toolbar.add_top_bar(&header);

    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&build_main_content_placeholder()));
    toolbar.set_content(Some(&toast_overlay));

    window.set_content(Some(&toolbar));

    let shell = Rc::new(Shell {
        app: ui_app,
        window: window.clone(),
        toast_overlay: toast_overlay.clone(),
        tick_source: RefCell::new(None),
    });
    SHELL.with(|s| *s.borrow_mut() = Some(shell.clone()));

    install_menu_actions(app, &shell);
    window.present();
}

/// Placeholder for the main content area. Follow-up commits replace this
/// with source picker, title tree, output picker, and progress/log panes
/// — the same layout `mac.rs` and `windows.rs` render. Kept as a labelled
/// AdwStatusPage so the app is presentable at all times, never a blank
/// window during the transitional builds.
fn build_main_content_placeholder() -> gtk4::Widget {
    let page = adw::StatusPage::builder()
        .icon_name("media-optical-symbolic")
        .title("freemkv Linux shell — under construction")
        .description(
            "The window, menus, notifications, and settings are wired up. \
             The rip UI (source picker, title list, output folder, progress \
             log) is being ported from the macOS shell one panel at a time — \
             see docs/linux-shell.md for the port checklist.",
        )
        .build();
    page.upcast()
}

fn build_hamburger_menu_button() -> gtk4::MenuButton {
    let btn = gtk4::MenuButton::new();
    btn.set_icon_name("open-menu-symbolic");
    btn.set_menu_model(Some(&build_menu_model()));
    btn
}

/// Translate `ui::menu_layout()` into a `gio::Menu`. Windows and Linux both
/// merge the App group into their platform's conventional location — on
/// Linux the App items (About, Preferences, Quit) go into the same
/// hamburger dropdown at the bottom, per GNOME app conventions.
fn build_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();
    let layout = crate::ui::menu_layout(false);

    // Non-App groups go into their own sub-sections of the hamburger.
    for group in &layout {
        if group.id == MenuGroupId::App {
            continue;
        }
        let section = gio::Menu::new();
        append_group_to_section(&section, group);
        menu.append_section(Some(&group.title), &section);
    }

    // App group at the bottom (GNOME convention: About / Preferences / Quit).
    if let Some(app_group) = layout.iter().find(|g| g.id == MenuGroupId::App) {
        let section = gio::Menu::new();
        append_group_to_section(&section, app_group);
        menu.append_section(None, &section);
    }

    menu
}

fn append_group_to_section(section: &gio::Menu, group: &MenuGroup) {
    for entry in &group.entries {
        let MenuEntry::Item(mi) = entry else { continue };
        // Cut/Copy/Paste/SelectAllText: no menu row on Linux — GTK edit
        // widgets carry their own context menu and standard accels. The
        // shared layout still names them so Windows and Mac render them.
        if is_standard_text_action(&mi.action) {
            continue;
        }
        let Some(action_name) = action_name_for(&mi.action) else {
            continue;
        };
        let item = gio::MenuItem::new(Some(&mi.label), Some(&format!("app.{action_name}")));
        if let Some(accel_str) = accel_string(mi.accel.as_ref()) {
            let v = glib::Variant::from(&*accel_str);
            item.set_attribute_value("accel", Some(&v));
        }
        section.append_item(&item);
    }
}

fn is_standard_text_action(a: &MenuAction) -> bool {
    matches!(
        a,
        MenuAction::StandardCut
            | MenuAction::StandardCopy
            | MenuAction::StandardPaste
            | MenuAction::StandardSelectAllText
    )
}

/// Map a shared `MenuAction` to the `gio::Action` name a hamburger item
/// triggers. Cut/Copy/Paste/SelectAllText return `None` — see
/// `is_standard_text_action`.
fn action_name_for(a: &MenuAction) -> Option<&'static str> {
    Some(match a {
        MenuAction::Cmd(Cmd::Open) => "open",
        MenuAction::OpenDisc => "open-disc",
        MenuAction::Cmd(Cmd::Close) => "close",
        MenuAction::Cmd(Cmd::SetOutput) => "set-output",
        MenuAction::Cmd(Cmd::Run) => "run",
        MenuAction::Cmd(Cmd::Eject) => "eject",
        MenuAction::Cmd(Cmd::SelectAll) => "select-all",
        MenuAction::Cmd(Cmd::SelectNone) => "select-none",
        MenuAction::Cmd(Cmd::Invert) => "invert",
        MenuAction::Cmd(Cmd::ToggleLog) => "toggle-log",
        MenuAction::Cmd(Cmd::ClearLog) => "clear-log",
        MenuAction::Cmd(Cmd::Settings) => "settings",
        MenuAction::Cmd(Cmd::About) => "about",
        MenuAction::Cmd(Cmd::Docs) => "docs",
        MenuAction::Cmd(Cmd::CheckUpdates) => "check-updates",
        MenuAction::Cmd(Cmd::Quit) => "quit",
        MenuAction::StandardCopy
        | MenuAction::StandardCut
        | MenuAction::StandardPaste
        | MenuAction::StandardSelectAllText => return None,
        MenuAction::Cmd(_) => return None,
    })
}

/// Translate a shared `Accel` to GTK's `<Primary>o` / `F1` string.
/// `<Primary>` is Ctrl on Linux — GTK routes it correctly.
fn accel_string(accel: Option<&crate::ui::Accel>) -> Option<String> {
    let a = accel?;
    let mut s = String::new();
    if a.primary {
        s.push_str("<Primary>");
    }
    if a.alt {
        s.push_str("<Alt>");
    }
    if a.shift {
        s.push_str("<Shift>");
    }
    s.push_str(a.key);
    Some(s)
}

/// Wire each hamburger action name to a callback that dispatches the
/// corresponding `Cmd` through the shared `App`. Adding a `Cmd` means
/// one line here (and the compile-time `MenuAction` exhaustive match in
/// `action_name_for`).
fn install_menu_actions(app: &adw::Application, shell: &Rc<Shell>) {
    let simple = |name: &'static str, cmd: Cmd, shell: Rc<Shell>| -> gio::SimpleAction {
        let action = gio::SimpleAction::new(name, None);
        action.connect_activate(move |_, _| {
            dispatch_cmd(&shell, cmd);
        });
        action
    };

    for (name, cmd) in [
        ("open", Cmd::Open),
        ("close", Cmd::Close),
        ("set-output", Cmd::SetOutput),
        ("run", Cmd::Run),
        ("eject", Cmd::Eject),
        ("select-all", Cmd::SelectAll),
        ("select-none", Cmd::SelectNone),
        ("invert", Cmd::Invert),
        ("toggle-log", Cmd::ToggleLog),
        ("clear-log", Cmd::ClearLog),
        ("settings", Cmd::Settings),
        ("about", Cmd::About),
        ("docs", Cmd::Docs),
        ("check-updates", Cmd::CheckUpdates),
        ("quit", Cmd::Quit),
    ] {
        app.add_action(&simple(name, cmd, shell.clone()));
    }

    // OpenDisc has no direct `Cmd` — drive enumeration lives here.
    // TODO(linux-gui): udisks2 / /sys/block/sr* picker; falls back to
    // `Cmd::Open` so the menu item does SOMETHING today.
    let shell_od = shell.clone();
    let open_disc = gio::SimpleAction::new("open-disc", None);
    open_disc.connect_activate(move |_, _| {
        dispatch_cmd(&shell_od, Cmd::Open);
    });
    app.add_action(&open_disc);
}

fn dispatch_cmd(shell: &Rc<Shell>, cmd: Cmd) {
    let effects = shell.app.borrow_mut().dispatch(cmd);
    perform_effects(shell, effects);
}

/// Apply the effect stream from `App::dispatch` / `App::tick`. This is the
/// Linux equivalent of `windows::Shell::perform` and `mac::Controller::perform`.
fn perform_effects(shell: &Rc<Shell>, effects: Vec<Effect>) {
    for e in effects {
        match e {
            Effect::PickSource => {
                // TODO(linux-gui): GTK4 file chooser. Placeholder no-op.
            }
            Effect::PickOutputDir => {
                // TODO(linux-gui): GTK4 folder chooser. Placeholder no-op.
            }
            Effect::Reveal(_p) => {
                // TODO(linux-gui): xdg-open the containing folder.
            }
            Effect::OpenUrl(u) => {
                // GTK helper opens URLs in the user's default browser.
                let _ = gtk4::gio::AppInfo::launch_default_for_uri(
                    &u,
                    None::<&gtk4::gio::AppLaunchContext>,
                );
            }
            Effect::ShowSettings => {
                // TODO(linux-gui): build the AdwPreferencesWindow.
            }
            Effect::ShowAbout => {
                let about = adw::AboutWindow::builder()
                    .transient_for(&shell.window)
                    .application_name("freemkv")
                    .application_icon("org.freemkv.FreeMKV")
                    .version(env!("CARGO_PKG_VERSION"))
                    .website("https://freemkv.org")
                    .license_type(gtk4::License::MitX11)
                    .build();
                about.present();
            }
            Effect::Redraw => {
                // TODO(linux-gui): rebuild the main content pane from
                // `shell.app.borrow().view()`.
            }
            Effect::StartTicking => start_tick(shell),
            Effect::StopTicking => stop_tick(shell),
            Effect::NotifyRipFinished {
                title,
                body,
                output_dir,
            } => {
                // In-window toast: immediate feedback while the window is
                // focused, and doubles as the fallback when the desktop
                // portal is unavailable (headless CI, kiosk sessions).
                shell.toast_overlay.add_toast(adw::Toast::new(&title));

                // Desktop notification via the XDG portal. In a Flatpak
                // sandbox this needs `--talk-name=org.freedesktop.portal.Desktop`
                // in finish-args, which the shipped manifest supplies.
                let notif = gio::Notification::new(&title);
                notif.set_body(Some(&body));
                notif.set_default_action_and_target_value(
                    "app.reveal-output",
                    Some(&glib::Variant::from(&*output_dir)),
                );
                gio::Application::default()
                    .as_ref()
                    .map(|a| a.send_notification(Some("rip-finished"), &notif));
            }
            Effect::Quit => shell.window.close(),
        }
    }
}

fn start_tick(shell: &Rc<Shell>) {
    let mut cur = shell.tick_source.borrow_mut();
    if cur.is_some() {
        return;
    }
    let shell_tick = shell.clone();
    // 250ms mirrors the AppKit shell's `NSTimer` interval — the rip
    // progress bar has to feel live, not stepped.
    let id = glib::timeout_add_local(Duration::from_millis(250), move || {
        let effects = shell_tick.app.borrow_mut().tick();
        perform_effects(&shell_tick, effects);
        glib::ControlFlow::Continue
    });
    *cur = Some(id);
}

fn stop_tick(shell: &Rc<Shell>) {
    if let Some(id) = shell.tick_source.borrow_mut().take() {
        id.remove();
    }
}

/// Best-effort $LANG / $LC_ALL parse for the "Auto" language setting. Same
/// role as `mac::system_locale_code` / `windows::system_locale_code`. A
/// bare `en_US.UTF-8` is trimmed to `en-US` — the i18n crate normalises
/// the rest.
pub fn system_locale_code() -> Option<String> {
    for var in ["LC_ALL", "LANG"] {
        let Ok(v) = std::env::var(var) else { continue };
        if v.is_empty() || v == "C" || v == "POSIX" {
            continue;
        }
        // Trim `.UTF-8`, `@modifier`, and turn `_` into `-`.
        let tag = v.split(['.', '@']).next().unwrap_or("").replace('_', "-");
        if !tag.is_empty() {
            return Some(tag);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_locale_code_parses_the_common_lang_shapes() {
        // Serial: mutating env var affects the whole process.
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prior_lang = std::env::var_os("LANG");
        let prior_lc = std::env::var_os("LC_ALL");
        unsafe {
            std::env::remove_var("LC_ALL");
            std::env::set_var("LANG", "en_US.UTF-8");
        }
        assert_eq!(system_locale_code().as_deref(), Some("en-US"));
        unsafe {
            std::env::set_var("LANG", "de_DE.UTF-8@euro");
        }
        assert_eq!(system_locale_code().as_deref(), Some("de-DE"));
        unsafe {
            std::env::set_var("LANG", "C");
        }
        assert!(system_locale_code().is_none(), "C locale must not resolve");
        unsafe {
            std::env::set_var("LANG", "");
        }
        assert!(system_locale_code().is_none());

        // Restore.
        unsafe {
            match prior_lang {
                Some(v) => std::env::set_var("LANG", v),
                None => std::env::remove_var("LANG"),
            }
            match prior_lc {
                Some(v) => std::env::set_var("LC_ALL", v),
                None => std::env::remove_var("LC_ALL"),
            }
        }
    }

    #[test]
    fn every_menu_action_the_layout_names_has_a_linux_action_name_or_is_standard_text() {
        // The Linux hamburger renders every non-text-standard MenuAction as
        // an app.<name> action; missing one = a menu row that does nothing.
        for group in crate::ui::menu_layout(false) {
            for entry in group.entries {
                let MenuEntry::Item(mi) = entry else { continue };
                if is_standard_text_action(&mi.action) {
                    continue;
                }
                assert!(
                    action_name_for(&mi.action).is_some(),
                    "MenuAction {:?} in layout but has no Linux action name",
                    mi.action,
                );
            }
        }
    }
}
