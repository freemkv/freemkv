//! Linux desktop shell — GTK4 + libadwaita.
//!
//! Third native shell alongside `windows.rs` (winsafe / Win32) and `mac.rs`
//! (objc2 / AppKit). Every user-facing decision comes from `ui.rs`, so the
//! app is the same product on all three OSes; only how the widgets are drawn
//! differs. See docs/linux-shell.md.
//!
//! The contract is the other shells' three steps and nothing else: `render`
//! assigns `App::view()` to widgets, every click / menu pick / key goes to
//! `App::dispatch`, and the returned `Effect`s are performed here. The
//! toolkit-free part of the glue (action table, accelerator spelling, log
//! append detection, …) lives in `linux_glue.rs` so its tests run on every CI
//! leg, not only a Linux one.
//!
//! GNOME idioms instead of copies of the other shells: an `AdwHeaderBar`
//! hamburger (no menubar), an `AdwPreferencesWindow` for Settings (changes are
//! committed when it closes, the GNOME convention, rather than via OK/Cancel),
//! `GtkFileDialog` / `GtkFileLauncher` / `GtkUriLauncher` so choosers and
//! "show in folder" go through the XDG portals inside a Flatpak, and an
//! `AdwToast` + `GNotification` when a rip finishes.
//!
//! WHY not `egui` / `iced` / Tauri: the project's stated invariant is that
//! every shell looks 100% native on its OS. GTK4 + libadwaita is what a GNOME
//! app looks like on Fedora, Ubuntu, and Pop!_OS in 2026, and what Flathub
//! reviewers expect.

mod main_view;
mod prefs;
mod tree;

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use crate::linux_glue as glue;
use crate::ui::{App, Cmd, Effect, LogKind, MenuAction, MenuEntry, MenuGroupId};

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const APP_ID: &str = "org.freemkv.FreeMKV";
/// The rip poller's period — the same 200 ms the macOS and Windows shells use.
const TICK_MS: u64 = 200;
/// Let the window paint before the launch probe starts looking for a disc.
const LAUNCH_PROBE_MS: u64 = 200;
const WEBSITE: &str = "https://freemkv.org";

/// Entry point called from `linux_app::run`. Blocks until the window is
/// closed. Returns 0 on normal exit, non-zero if GTK could not start.
pub fn run() -> i32 {
    if let Err(e) = adw::init() {
        eprintln!(
            "freemkv: cannot start the desktop app ({e}); the GTK4/libadwaita \
             runtime libraries are not available. The CLI still works."
        );
        return 1;
    }
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_ui);
    // Only argv[0]: GApplication would otherwise read `gui` as a file to open
    // and refuse to start, since this app does not handle `open`.
    let argv0 = std::env::args().next().unwrap_or_else(|| "freemkv".into());
    app.run_with_args(&[argv0]).into()
}

/// The GTK-side shell. Owns the widgets; the model is `app`, and nothing here
/// duplicates its state — `memo`/`menu_label` only record what was painted.
pub(crate) struct Shell {
    app: RefCell<App>,
    /// The stored settings Settings edits, as on macOS/Windows: separate from
    /// `app.settings` until a commit pushes them in.
    settings: RefCell<crate::settings::Settings>,
    gapp: adw::Application,
    window: adw::ApplicationWindow,
    menu_btn: gtk::MenuButton,
    toast_overlay: adw::ToastOverlay,
    main: RefCell<Option<Rc<main_view::MainView>>>,
    memo: RefCell<main_view::Memo>,
    menu_label: RefCell<String>,
    actions: RefCell<Vec<(gio::SimpleAction, Option<Cmd>)>>,
    tick: RefCell<Option<glib::SourceId>>,
    drain: RefCell<Option<glib::SourceId>>,
    /// Worker threads (keydb update) push lines here; a main-loop timer
    /// drains them, because GTK objects never cross threads.
    inbox: Arc<Mutex<Vec<String>>>,
    keydb_updating: Cell<bool>,
    quit_confirmed: Cell<bool>,
    /// True while `render` writes to widgets, so the change signals those
    /// writes emit are not mistaken for the user.
    painting: Cell<bool>,
    prefs: RefCell<Option<Rc<prefs::Prefs>>>,
}

fn build_ui(gapp: &adw::Application) {
    // A second `freemkv gui` activates this instance: show the window we have.
    if let Some(w) = gapp.active_window() {
        w.present();
        return;
    }
    let window = adw::ApplicationWindow::builder()
        .application(gapp)
        .title("freemkv")
        .default_width(1180)
        .default_height(760)
        .build();
    window.set_size_request(900, 600);

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .primary(true)
        .build();
    header.pack_end(&menu_btn);
    toolbar.add_top_bar(&header);
    let toast_overlay = adw::ToastOverlay::new();
    toolbar.set_content(Some(&toast_overlay));
    window.set_content(Some(&toolbar));

    let shell = Rc::new(Shell {
        app: RefCell::new(App::new()),
        settings: RefCell::new(crate::settings::Settings::load()),
        gapp: gapp.clone(),
        window: window.clone(),
        menu_btn,
        toast_overlay,
        main: RefCell::new(None),
        memo: RefCell::new(main_view::Memo::default()),
        menu_label: RefCell::new(String::new()),
        actions: RefCell::new(Vec::new()),
        tick: RefCell::new(None),
        drain: RefCell::new(None),
        inbox: Arc::new(Mutex::new(Vec::new())),
        keydb_updating: Cell::new(false),
        quit_confirmed: Cell::new(false),
        painting: Cell::new(false),
        prefs: RefCell::new(None),
    });

    shell.install_actions();
    shell.install_window_handlers();
    shell.relocalize();
    window.present();

    if let Ok(src) = dev_env("FMKV_OPEN") {
        shell.open_path(&src);
    }
    if launch_probe_enabled() {
        let me = shell.clone();
        glib::timeout_add_local_once(Duration::from_millis(LAUNCH_PROBE_MS), move || {
            me.open_disc(false);
        });
    }
}

/// Development-only environment lookup; a release build has no switches.
fn dev_env(key: &str) -> Result<String, std::env::VarError> {
    if cfg!(debug_assertions) {
        std::env::var(key)
    } else {
        Err(std::env::VarError::NotPresent)
    }
}

// The launch probe scans the drive; a debug session that opened a fixture
// (FMKV_OPEN) or asked for no hardware (FMKV_NO_PROBE) must never touch it.
fn launch_probe_enabled() -> bool {
    ["FMKV_OPEN", "FMKV_NO_PROBE"]
        .iter()
        .all(|k| dev_env(k).is_err())
}

impl Shell {
    /// Mutate the model and repaint — the one choke point, so no handler can
    /// change state and forget to redraw. The borrow ends before `render`.
    fn app_mut<R>(self: &Rc<Self>, f: impl FnOnce(&mut App) -> R) -> R {
        let r = f(&mut self.app.borrow_mut());
        self.render();
        r
    }

    /// The shell's entire job: hand the command to the core, perform what it
    /// asks for, redraw.
    fn act(self: &Rc<Self>, cmd: Cmd) {
        let fx = self.app_mut(|a| a.dispatch(cmd));
        self.perform(fx);
    }

    fn say(self: &Rc<Self>, kind: LogKind, text: &str) {
        self.app_mut(|a| a.say(kind, text));
    }

    fn open_path(self: &Rc<Self>, path: &str) {
        let fx = self.app_mut(|a| a.open(path));
        self.perform(fx);
    }

    /// File ▸ Open disc, the empty page's button and the launch probe. The
    /// decision is `App::disc_source`; the scan is deferred one beat so the
    /// "Opening …" line paints before `open` blocks on the drive.
    fn open_disc(self: &Rc<Self>, announce_missing: bool) {
        let Some(url) = self.app_mut(|a| a.disc_source(announce_missing)) else {
            return;
        };
        if !announce_missing {
            let fx = self.app_mut(|a| a.open_probe(&url));
            self.perform(fx);
            return;
        }
        let me = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(30), move || me.open_path(&url));
    }

    fn select_row(self: &Rc<Self>, idx: usize) {
        if !self.painting.get() {
            self.app_mut(|a| a.selected_row = Some(idx));
        }
    }

    fn toggle_row(self: &Rc<Self>, idx: usize) {
        if !self.painting.get() {
            self.app_mut(|a| a.tree.toggle(idx));
        }
    }

    /// Apply a fully-decided `View`. The only place widgets are written.
    fn render(self: &Rc<Self>) {
        let (v, running) = {
            let a = self.app.borrow();
            (a.view(), a.running())
        };
        let main = self.main.borrow().clone();
        if let Some(m) = main {
            self.painting.set(true);
            m.render(&v, &mut self.memo.borrow_mut());
            self.painting.set(false);
        }
        // The log item names the action it will perform, so the menu is
        // rebuilt whenever that label changes.
        if *self.menu_label.borrow() != v.log_menu_label {
            self.menu_btn
                .set_menu_model(Some(&build_menu_model(v.log_hidden)));
            *self.menu_label.borrow_mut() = v.log_menu_label.clone();
        }
        for (action, gate) in self.actions.borrow().iter() {
            let blocked = gate.is_some_and(crate::ui::blocked_while_running);
            action.set_enabled(!(running && blocked));
        }
    }

    fn perform(self: &Rc<Self>, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::PickSource => self.pick_source(),
                Effect::PickOutputDir => {
                    let initial = self.app.borrow().output_dir.clone();
                    let me = self.clone();
                    self.pick(
                        true,
                        &crate::strings::get("gui.panel.output_msg"),
                        false,
                        &initial,
                        move |p| me.app_mut(|a| a.output_dir = p),
                    );
                }
                Effect::Reveal(p) => self.reveal(&p),
                Effect::OpenUrl(u) => {
                    gtk::UriLauncher::new(&u).launch(
                        Some(&self.window),
                        gio::Cancellable::NONE,
                        |_| {},
                    );
                }
                Effect::ShowSettings => prefs::show(self, None),
                Effect::ShowAbout => self.show_about(),
                Effect::StartTicking => self.start_tick(),
                Effect::StopTicking => {
                    if let Some(id) = self.tick.borrow_mut().take() {
                        id.remove();
                    }
                }
                Effect::NotifyRipFinished {
                    title,
                    body,
                    output_dir,
                } => self.notify_finished(&title, &body, &output_dir),
                Effect::Quit => self.window.close(),
                Effect::Redraw => {}
            }
        }
        self.render();
    }

    fn start_tick(self: &Rc<Self>) {
        if self.tick.borrow().is_some() {
            return;
        }
        let me = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(TICK_MS), move || {
            let fx = me.app_mut(|a| a.tick());
            me.perform(fx);
            glib::ControlFlow::Continue
        });
        *self.tick.borrow_mut() = Some(id);
    }

    fn pick_source(self: &Rc<Self>) {
        let me = self.clone();
        self.pick(
            false,
            &crate::strings::get("gui.panel.source_msg"),
            true,
            "",
            move |p| me.open_path(&p),
        );
    }

    /// The native chooser (`GtkFileDialog`, portal-backed under Flatpak).
    /// Asynchronous, unlike the other shells' modal panels, so the answer
    /// arrives in `on_pick`; a dismissed dialog calls nothing.
    fn pick(
        self: &Rc<Self>,
        folder: bool,
        title: &str,
        filter_source: bool,
        initial: &str,
        on_pick: impl FnOnce(String) + 'static,
    ) {
        let dlg = gtk::FileDialog::builder().title(title).modal(true).build();
        let start = std::path::Path::new(initial);
        if !initial.is_empty() && start.is_dir() {
            dlg.set_initial_folder(Some(&gio::File::for_path(start)));
        }
        if filter_source {
            let media = gtk::FileFilter::new();
            media.set_name(Some(title));
            let mut exts: Vec<String> = crate::ui::SOURCE_EXTS
                .iter()
                .map(|e| e.to_ascii_lowercase())
                .collect();
            exts.dedup();
            for e in &exts {
                media.add_suffix(e);
            }
            let any = gtk::FileFilter::new();
            any.set_name(Some("*"));
            any.add_pattern("*");
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&media);
            filters.append(&any);
            dlg.set_filters(Some(&filters));
            dlg.set_default_filter(Some(&media));
        }
        let done = move |r: Result<gio::File, glib::Error>| {
            if let Some(p) = r.ok().and_then(|f| f.path()) {
                on_pick(p.to_string_lossy().into_owned());
            }
        };
        if folder {
            dlg.select_folder(Some(&self.window), gio::Cancellable::NONE, done);
        } else {
            dlg.open(Some(&self.window), gio::Cancellable::NONE, done);
        }
    }

    /// Open the containing folder with `path` selected — "Show in Finder" /
    /// `explorer /select,` — via the FileManager1 / OpenURI portal.
    fn reveal(self: &Rc<Self>, path: &str) {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
        let me = self.clone();
        launcher.open_containing_folder(Some(&self.window), gio::Cancellable::NONE, move |r| {
            if let Err(e) = r
                && !e.matches(gtk::DialogError::Dismissed)
            {
                me.say(
                    LogKind::Notice,
                    &crate::strings::fmt_or(
                        "gui.log.open_folder_failed",
                        "Could not open the folder: {e}",
                        &[("e", &e.to_string())],
                    ),
                );
            }
        });
    }

    /// In-window toast (with a "show" button) plus a desktop notification via
    /// the XDG portal. Clicking either reveals THIS rip's output folder.
    fn notify_finished(self: &Rc<Self>, title: &str, body: &str, output_dir: &str) {
        let action = format!("app.{}", glue::REVEAL_ACTION);
        let target = output_dir.to_variant();
        let show = show_folder_label();

        let toast = adw::Toast::new(&glib::markup_escape_text(title));
        toast.set_button_label(Some(&show));
        toast.set_action_name(Some(&action));
        toast.set_action_target_value(Some(&target));
        self.toast_overlay.add_toast(toast);

        let n = gio::Notification::new(title);
        n.set_body(Some(body));
        n.set_default_action_and_target_value(&action, Some(&target));
        n.add_button_with_target_value(&show, &action, Some(&target));
        self.gapp.send_notification(Some("rip-finished"), &n);
    }

    fn show_about(self: &Rc<Self>) {
        let g = crate::strings::get;
        let keys = self.settings.borrow().keydb_status();
        let comments = format!(
            "{} libfreemkv {}\n{} MIT\n{} {}",
            g("gui.about.engine"),
            libfreemkv::VERSION_LABEL,
            g("gui.about.licence"),
            g("gui.about.keys"),
            keys,
        );
        let about = adw::AboutWindow::builder()
            .transient_for(&self.window)
            .modal(true)
            .destroy_with_parent(true)
            .title(g("gui.menu.app_about"))
            .application_name("freemkv")
            .application_icon(APP_ID)
            .version(format!("{} (Linux)", env!("CARGO_PKG_VERSION")))
            .comments(comments)
            .website(WEBSITE)
            .issue_url("https://github.com/freemkv/freemkv/issues")
            .license_type(gtk::License::MitX11)
            .build();
        about.present();
    }

    /// Save the Settings copy and tell the operator whether it worked — the
    /// one policy the other shells share.
    fn save_settings_reporting_error(self: &Rc<Self>) {
        let saved = self.settings.borrow().save();
        match saved {
            Ok(()) => self.say(
                LogKind::Result,
                &crate::strings::get("gui.log.settings_saved"),
            ),
            Err(e) => self.say(
                LogKind::Notice,
                &crate::strings::fmt("gui.log.settings_save_error", &[("e", &e)]),
            ),
        }
    }

    fn set_keydb_note(&self, text: &str) {
        if let Some(p) = self.prefs.borrow().as_ref() {
            p.set_keydb_note(text);
        }
    }

    /// The FLAG is the guard against a second download; the insensitive row
    /// is only how it is shown (a Settings window reopened mid-download gets
    /// a fresh row, which reads the flag).
    fn set_keydb_updating(&self, updating: bool) {
        self.keydb_updating.set(updating);
        if let Some(p) = self.prefs.borrow().as_ref() {
            p.set_keydb_updating(updating);
        }
    }

    fn start_drain(self: &Rc<Self>) {
        if self.drain.borrow().is_some() {
            return;
        }
        let me = self.clone();
        let id = glib::timeout_add_local(Duration::from_millis(TICK_MS), move || {
            let msgs: Vec<String> = me
                .inbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
                .collect();
            if msgs.is_empty() {
                return glib::ControlFlow::Continue;
            }
            for m in &msgs {
                me.say(LogKind::Result, m);
            }
            if let Some(last) = msgs.last() {
                me.set_keydb_note(last);
            }
            me.set_keydb_updating(false);
            me.drain.borrow_mut().take();
            glib::ControlFlow::Break
        });
        *self.drain.borrow_mut() = Some(id);
    }

    /// Build (or, after a language switch, rebuild) the window's content and
    /// menu in the active locale, then repaint everything from the model.
    fn relocalize(self: &Rc<Self>) {
        let m = main_view::build(self);
        self.toast_overlay.set_child(Some(&m.root));
        *self.main.borrow_mut() = Some(m);
        *self.memo.borrow_mut() = main_view::Memo::default();
        self.menu_label.borrow_mut().clear();
        self.render();
    }

    fn install_actions(self: &Rc<Self>) {
        let mut installed = Vec::new();
        for (name, ma) in glue::ACTIONS {
            let action = gio::SimpleAction::new(name, None);
            let me = self.clone();
            let ma = *ma;
            action.connect_activate(move |_, _| match ma {
                MenuAction::OpenDisc => me.open_disc(true),
                MenuAction::Cmd(c) => me.act(c),
                _ => {}
            });
            self.gapp.add_action(&action);
            installed.push((action, glue::gating_cmd(&ma)));
        }
        *self.actions.borrow_mut() = installed;

        let reveal = gio::SimpleAction::new(glue::REVEAL_ACTION, Some(glib::VariantTy::STRING));
        let me = self.clone();
        reveal.connect_activate(move |_, p| {
            if let Some(dir) = p.and_then(|v| v.get::<String>()) {
                me.window.present();
                me.reveal(&dir);
            }
        });
        self.gapp.add_action(&reveal);

        for group in crate::ui::menu_layout(false) {
            for entry in &group.entries {
                let MenuEntry::Item(mi) = entry else { continue };
                if let (Some(name), Some(a)) = (glue::action_name_for(&mi.action), mi.accel) {
                    self.gapp
                        .set_accels_for_action(&format!("app.{name}"), &[&glue::accel_string(&a)]);
                }
            }
        }
    }

    fn install_window_handlers(self: &Rc<Self>) {
        let me = self.clone();
        self.window.connect_close_request(move |_| {
            let running = me.app.borrow().running();
            if glue::needs_rip_confirmation(running, me.quit_confirmed.get()) {
                me.confirm_quit();
                return glib::Propagation::Stop;
            }
            if let Some(id) = me.tick.borrow_mut().take() {
                id.remove();
            }
            glib::Propagation::Proceed
        });

        // Dropping a file or folder on the window opens it, as on macOS and
        // Windows. Refused mid-rip by the core's rule for Open.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let me = self.clone();
        drop.connect_drop(move |_, value, _, _| {
            if me.app.borrow().running() && crate::ui::blocked_while_running(Cmd::Open) {
                return false;
            }
            let Some(path) = value
                .get::<gdk::FileList>()
                .ok()
                .and_then(|l| l.files().first().and_then(|f| f.path()))
            else {
                return false;
            };
            let p = path.to_string_lossy().into_owned();
            let me = me.clone();
            if glue::is_openable_source(&path) {
                glib::idle_add_local_once(move || me.open_path(&p));
                true
            } else {
                glib::idle_add_local_once(move || {
                    me.say(
                        LogKind::Notice,
                        &crate::strings::fmt("gui.log.not_supported", &[("p", &p)]),
                    )
                });
                false
            }
        });
        self.window.add_controller(drop);
    }

    /// Closing mid-rip must not silently tear the rip down. "Stop & Quit"
    /// cancels, WAITS (bounded) for the worker to put the file down, then
    /// closes; anything else keeps ripping.
    fn confirm_quit(self: &Rc<Self>) {
        let g = crate::strings::get;
        let dlg = adw::MessageDialog::new(
            Some(&self.window),
            Some(&g("gui.alert.rip_title")),
            Some(&g("gui.alert.rip_body")),
        );
        dlg.add_responses(&[
            ("keep", &g("gui.alert.keep_ripping")),
            ("stop", &g("gui.alert.stop_quit")),
        ]);
        dlg.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
        dlg.set_default_response(Some("keep"));
        dlg.set_close_response("keep");
        let me = self.clone();
        dlg.choose(gio::Cancellable::NONE, move |r| {
            if r != "stop" {
                return;
            }
            me.quit_confirmed.set(true);
            me.act(Cmd::Cancel);
            let run = me.app.borrow().run.clone();
            if let Some(run) = run {
                crate::engine::await_worker_exit(&run, crate::engine::QUIT_GRACE);
            }
            me.window.close();
        });
    }
}

/// "Show in Folder" — the result page button, the toast and the notification.
fn show_folder_label() -> String {
    crate::strings::get_or("gui.btn.show_folder", "Show in Folder")
}

/// Translate `ui::menu_layout` into the hamburger's `gio::Menu`. Each group
/// becomes titled sections (split at the layout's separators); the App group
/// (About / Settings / Quit) goes last, per GNOME convention.
fn build_menu_model(log_hidden: bool) -> gio::Menu {
    let menu = gio::Menu::new();
    let layout = crate::ui::menu_layout(log_hidden);
    let others = layout.iter().filter(|g| g.id != MenuGroupId::App);
    let app = layout.iter().filter(|g| g.id == MenuGroupId::App);
    for group in others.chain(app) {
        let title = (group.id != MenuGroupId::App).then_some(group.title.as_str());
        let mut section = gio::Menu::new();
        let mut first = true;
        for entry in &group.entries {
            match entry {
                MenuEntry::Separator => {
                    if section.n_items() > 0 {
                        menu.append_section(if first { title } else { None }, &section);
                        first = false;
                        section = gio::Menu::new();
                    }
                }
                MenuEntry::Item(mi) => {
                    let Some(name) = glue::action_name_for(&mi.action) else {
                        continue;
                    };
                    let item = gio::MenuItem::new(Some(&mi.label), Some(&format!("app.{name}")));
                    if let Some(a) = mi.accel {
                        item.set_attribute_value(
                            "accel",
                            Some(&glue::accel_string(&a).to_variant()),
                        );
                    }
                    section.append_item(&item);
                }
            }
        }
        if section.n_items() > 0 {
            menu.append_section(if first { title } else { None }, &section);
        }
    }
    menu
}

/// Best-effort `$LC_ALL` / `$LANG` for the "Auto" language setting. Same role
/// as `mac::system_locale_code` / `windows::system_locale_code`.
pub fn system_locale_code() -> Option<String> {
    ["LC_ALL", "LANG"].iter().find_map(|var| {
        std::env::var(var)
            .ok()
            .and_then(|v| glue::parse_locale_env(&v))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sections, not rows, carry the group titles; every row keeps an action.
    #[test]
    fn the_hamburger_has_a_row_for_every_layout_action() {
        let m = build_menu_model(false);
        let mut actions = Vec::new();
        for s in 0..m.n_items() {
            let Some(sec) = m.item_link(s, "section") else {
                continue;
            };
            for i in 0..sec.n_items() {
                if let Some(v) = sec.item_attribute_value(i, "action", None) {
                    actions.push(v.get::<String>().unwrap_or_default());
                }
            }
        }
        for (name, _) in glue::ACTIONS {
            assert!(actions.contains(&format!("app.{name}")), "{name} missing");
        }
    }
}
