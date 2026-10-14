//! Linux desktop shell using GTK4 and libadwaita.
//!
//! Render `App::view()` into widgets, route input through `App::dispatch`, and
//! perform the returned effects. Product decisions live in `ui.rs`; toolkit-free
//! glue is tested through `linux_glue.rs` on every platform.
//! File/URI operations use GTK portal-aware APIs for Flatpak compatibility.

// qa's GUI gate (FMKV_GATE), a debug-build hook like FMKV_OPEN.
#[cfg(debug_assertions)]
mod gate;
mod main_view;
mod pick_bar;
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
    gapp: adw::Application,
    window: adw::ApplicationWindow,
    menu_btn: gtk::MenuButton,
    toast_overlay: adw::ToastOverlay,
    main: RefCell<Option<Rc<main_view::MainView>>>,
    memo: RefCell<main_view::Memo>,
    menu_label: RefCell<String>,
    actions: RefCell<Vec<(gio::SimpleAction, Option<Cmd>)>>,
    tick: RefCell<Option<glib::SourceId>>,
    /// An Open Disc's drive enumeration is in flight.
    finding_drives: Cell<bool>,
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
        gapp: gapp.clone(),
        window: window.clone(),
        menu_btn,
        toast_overlay,
        main: RefCell::new(None),
        memo: RefCell::new(main_view::Memo::default()),
        menu_label: RefCell::new(String::new()),
        actions: RefCell::new(Vec::new()),
        tick: RefCell::new(None),
        finding_drives: Cell::new(false),
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
    #[cfg(debug_assertions)]
    if let Ok(dir) = dev_env("FMKV_GATE") {
        gate::start(&shell, dir);
    }
    if launch_probe_enabled() {
        let me = shell.clone();
        glib::timeout_add_local_once(Duration::from_millis(LAUNCH_PROBE_MS), move || {
            match crate::app_entry::launch_source() {
                Some(src) => me.open_path(src),
                None => me.open_disc(false),
            }
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
    ["FMKV_OPEN", "FMKV_NO_PROBE", "FMKV_GATE"]
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

    /// Mutate the model for effects and perform them; `perform` repaints
    /// once at the end, so this never paints twice.
    fn apply(self: &Rc<Self>, f: impl FnOnce(&mut App) -> Vec<Effect>) {
        let fx = f(&mut self.app.borrow_mut());
        self.perform(fx);
    }

    /// The shell's entire job: hand the command to the core, perform what it
    /// asks for, redraw.
    fn act(self: &Rc<Self>, cmd: Cmd) {
        self.apply(|a| a.dispatch(cmd));
    }

    fn say(self: &Rc<Self>, kind: LogKind, text: &str) {
        self.app_mut(|a| a.say(kind, text));
    }

    fn open_path(self: &Rc<Self>, path: &str) {
        self.apply(|a| a.open_async(path));
    }

    /// The launch probe enumerates nothing here. An explicit Open Disc lists
    /// drives on a worker (a SCSI walk), then logs and opens as macOS/Windows.
    fn open_disc(self: &Rc<Self>, announce_missing: bool) {
        if !announce_missing {
            self.apply(|a| match a.disc_source(false) {
                Some(url) => a.open_probe(&url),
                None => vec![],
            });
            return;
        }
        let ok = {
            let a = self.app.borrow();
            glue::action_enabled(Some(Cmd::Open), a.running(), a.opening())
        };
        if !ok || self.finding_drives.replace(true) {
            return;
        }
        let me = self.clone();
        glib::spawn_future_local(async move {
            let drives = gio::spawn_blocking(crate::engine::list_optical_drives).await;
            me.finding_drives.set(false);
            // A panicked enumeration is a failure to report, not an empty drive list.
            let Ok(drives) = drives else {
                me.say(
                    LogKind::Notice,
                    &crate::strings::get_or(
                        "gui.log.drive_scan_failed",
                        "Could not list the optical drives (internal error).",
                    ),
                );
                return;
            };
            let (kind, line, url) = glue::disc_open_plan(&drives);
            me.apply(|a| match url {
                None => {
                    a.say(kind, &line);
                    vec![]
                }
                // State may have changed during the walk: announce the open only if it began.
                Some(u) => {
                    let fx = a.open_async(&u);
                    if !fx.is_empty() {
                        a.say(kind, &line);
                    }
                    fx
                }
            });
        });
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
        let (v, running, opening) = {
            let a = self.app.borrow();
            (a.view(), a.running(), a.opening())
        };
        self.window
            .set_cursor_from_name(opening.then_some("progress"));
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
        if let Some(p) = self.prefs.borrow().as_ref() {
            p.paint_keydb(v.keydb_note.as_deref(), v.keydb_busy);
            p.paint_keyserver(self.app.borrow().keyserver_verdict());
        }
        for (action, gate) in self.actions.borrow().iter() {
            action.set_enabled(glue::action_enabled(gate.clone(), running, opening));
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
                    let me = self.clone();
                    gtk::UriLauncher::new(&u).launch(
                        Some(&self.window),
                        gio::Cancellable::NONE,
                        move |r| {
                            if let Err(e) = r
                                && !e.matches(gtk::DialogError::Dismissed)
                            {
                                me.say(
                                    LogKind::Notice,
                                    &crate::strings::fmt_or(
                                        "gui.log.open_url_failed",
                                        "Could not open {url}: {e}",
                                        &[("url", &u), ("e", &e.to_string())],
                                    ),
                                );
                            }
                        },
                    );
                }
                Effect::ShowSettings(st) => prefs::show(self, &st, None),
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
                } => self.notify_finished(&title, &body, output_dir.as_deref()),
                Effect::Quit => self.window.close(),
                Effect::ConfirmQuit => self.confirm_quit(),
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
            me.apply(App::tick);
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
    /// arrives in `on_pick`; a dismissed dialog calls nothing, a failure logs.
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
        let me = self.clone();
        let done = move |r: Result<gio::File, glib::Error>| {
            let msg = match r {
                Ok(f) => match f.path() {
                    Some(p) => return on_pick(p.to_string_lossy().into_owned()),
                    None => crate::strings::fmt_or(
                        "gui.log.pick_not_local",
                        "Only local files and folders can be used: {p}",
                        &[("p", &f.uri())],
                    ),
                },
                Err(e) if e.matches(gtk::DialogError::Dismissed) => return,
                Err(e) => crate::strings::fmt_or(
                    "gui.log.pick_failed",
                    "Could not open the file chooser: {e}",
                    &[("e", &e.to_string())],
                ),
            };
            me.say(LogKind::Notice, &msg);
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

    /// In-window toast plus a desktop notification via the XDG portal. With an
    /// output folder, both offer to reveal THIS rip's folder; without, neither does.
    fn notify_finished(self: &Rc<Self>, title: &str, body: &str, output_dir: Option<&str>) {
        let action = format!("app.{}", glue::REVEAL_ACTION);
        let show = show_folder_label();

        let toast = adw::Toast::new(&glib::markup_escape_text(title));
        // Plain body: GTK's backend and the portal take plain text; only
        // markup-parsing fdo servers (Plasma, dunst) may misrender a `&`.
        let n = gio::Notification::new(title);
        n.set_body(Some(body));
        if let Some(dir) = output_dir {
            let target = dir.to_variant();
            toast.set_button_label(Some(&show));
            toast.set_action_name(Some(&action));
            toast.set_action_target_value(Some(&target));
            n.set_default_action_and_target_value(&action, Some(&target));
            n.add_button_with_target_value(&show, &action, Some(&target));
        }
        self.toast_overlay.add_toast(toast);
        self.gapp.send_notification(Some("rip-finished"), &n);
    }

    fn show_about(self: &Rc<Self>) {
        let g = crate::strings::get;
        let keys = self.app.borrow().settings.keydb_status();
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

    /// Build (or, after a language switch, rebuild) the window's content and
    /// menu in the active locale, then repaint everything from the model.
    fn relocalize(self: &Rc<Self>) {
        if let Some(old) = self.main.borrow_mut().take() {
            old.detach();
        }
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
            let gate = glue::gating_cmd(ma);
            let ma = ma.clone();
            action.connect_activate(move |_, _| match &ma {
                MenuAction::OpenDisc => me.open_disc(true),
                MenuAction::Cmd(c) => me.act(c.clone()),
                _ => {}
            });
            self.gapp.add_action(&action);
            installed.push((action, gate));
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

        let shortcuts = gio::SimpleAction::new(SHORTCUTS_ACTION, None);
        let me = self.clone();
        shortcuts.connect_activate(move |_, _| me.show_shortcuts());
        self.gapp.add_action(&shortcuts);
        self.gapp
            .set_accels_for_action(&format!("app.{SHORTCUTS_ACTION}"), &[SHORTCUTS_ACCEL]);

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
            // The core decides whether to ask; the answer comes back as a close.
            let fx = { me.app.borrow_mut() }.dispatch(Cmd::QuitRequested);
            if fx.contains(&Effect::ConfirmQuit) {
                me.perform(fx);
                return glib::Propagation::Stop;
            }
            if let Some(id) = me.tick.borrow_mut().take() {
                id.remove();
            }
            glib::Propagation::Proceed
        });

        // Dropping a file or folder on the window opens it, as on macOS and
        // Windows. Refused mid-rip or mid-scan by the core's rule for Open.
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        let me = self.clone();
        drop.connect_drop(move |_, value, _, _| {
            let (running, opening) = {
                let a = me.app.borrow();
                (a.running(), a.opening())
            };
            if !glue::action_enabled(Some(Cmd::Open), running, opening) {
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

    /// Closing mid-rip must not silently tear the rip down. "Stop & Quit" has
    /// the core cancel and WAIT (bounded) for the worker to put the file down,
    /// The Keyboard Shortcuts window (Ctrl+?), rebuilt each time so it is in the
    /// current language.
    fn show_shortcuts(self: &Rc<Self>) {
        let builder = gtk::Builder::from_string(&shortcuts_ui());
        if let Some(w) = builder.object::<gtk::ShortcutsWindow>("shortcuts") {
            w.set_transient_for(Some(&self.window));
            w.present();
        }
    }

    /// then closes; anything else keeps ripping.
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
            me.app_mut(|a| a.confirm_quit());
            me.window.close();
        });
    }
}

/// "Show in Folder" — the result page button, the toast and the notification.
fn show_folder_label() -> String {
    crate::strings::get_or("gui.btn.show_folder", "Show in Folder")
}

/// The Keyboard Shortcuts window's action (Ctrl+?), outside the shared layout: the
/// window is a GNOME convention the other shells do not have.
const SHORTCUTS_ACTION: &str = "shortcuts";
const SHORTCUTS_ACCEL: &str = "<Primary>question";

fn shortcuts_label() -> String {
    crate::strings::get_or("gui.menu.shortcuts", "Keyboard Shortcuts")
}

// One layout row as a menu row bound to its `app.` action, accelerator shown.
fn menu_row(mi: &crate::ui::MenuItem) -> Option<gio::MenuItem> {
    let name = glue::action_name_for(&mi.action)?;
    let item = gio::MenuItem::new(Some(&mi.label), Some(&format!("app.{name}")));
    if let Some(a) = mi.accel {
        item.set_attribute_value("accel", Some(&glue::accel_string(&a).to_variant()));
    }
    Some(item)
}

/// Translate `ui::menu_layout` into the hamburger's `gio::Menu`, GNOME-style and
/// compact: File, Edit, View and Help as submenus (Quit ends File), then Settings,
/// Keyboard Shortcuts and About. Every action and accelerator is still reachable.
fn build_menu_model(log_hidden: bool) -> gio::Menu {
    let layout = crate::ui::menu_layout(log_hidden);
    let is_quit = |mi: &crate::ui::MenuItem| glue::action_name_for(&mi.action) == Some("quit");
    let app_items: Vec<&crate::ui::MenuItem> = layout
        .iter()
        .filter(|g| g.id == MenuGroupId::App)
        .flat_map(|g| g.entries.iter())
        .filter_map(|e| match e {
            MenuEntry::Item(mi) => Some(mi),
            MenuEntry::Separator => None,
        })
        .collect();
    let submenus = gio::Menu::new();
    for group in layout.iter().filter(|g| g.id != MenuGroupId::App) {
        let sub = gio::Menu::new();
        let mut section = gio::Menu::new();
        for entry in &group.entries {
            match entry {
                MenuEntry::Separator if section.n_items() > 0 => {
                    sub.append_section(None, &section);
                    section = gio::Menu::new();
                }
                MenuEntry::Separator => {}
                MenuEntry::Item(mi) => {
                    if let Some(row) = menu_row(mi) {
                        section.append_item(&row);
                    }
                }
            }
        }
        if section.n_items() > 0 {
            sub.append_section(None, &section);
        }
        if group.id == MenuGroupId::File {
            let quit = gio::Menu::new();
            for mi in app_items.iter().filter(|mi| is_quit(mi)) {
                if let Some(row) = menu_row(mi) {
                    quit.append_item(&row);
                }
            }
            sub.append_section(None, &quit);
        }
        if sub.n_items() > 0 {
            submenus.append_submenu(Some(&group.title), &sub);
        }
    }
    let app = gio::Menu::new();
    let find = |name: &str| {
        app_items
            .iter()
            .find(|mi| glue::action_name_for(&mi.action) == Some(name))
    };
    if let Some(row) = find("settings").and_then(|mi| menu_row(mi)) {
        app.append_item(&row);
    }
    let shortcuts = gio::MenuItem::new(
        Some(&shortcuts_label()),
        Some(&format!("app.{SHORTCUTS_ACTION}")),
    );
    shortcuts.set_attribute_value("accel", Some(&SHORTCUTS_ACCEL.to_variant()));
    app.append_item(&shortcuts);
    if let Some(row) = find("about").and_then(|mi| menu_row(mi)) {
        app.append_item(&row);
    }
    let menu = gio::Menu::new();
    menu.append_section(None, &submenus);
    menu.append_section(None, &app);
    menu
}

// Text for GtkBuilder XML: the five predefined entities.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The Keyboard Shortcuts window as GtkBuilder XML, built from the same layout as the
/// menu: one group per menu group, one row per accelerator, so the two cannot drift.
fn shortcuts_ui() -> String {
    let mut groups = String::new();
    let mut add_group = |title: &str, rows: &[(String, String)]| {
        if rows.is_empty() {
            return;
        }
        groups.push_str(&format!(
            "<child><object class=\"GtkShortcutsGroup\"><property name=\"title\">{}</property>",
            xml_escape(title)
        ));
        for (label, accel) in rows {
            groups.push_str(&format!(
                "<child><object class=\"GtkShortcutsShortcut\"><property name=\"title\">{}</property><property name=\"accelerator\">{}</property></object></child>",
                xml_escape(label),
                xml_escape(accel)
            ));
        }
        groups.push_str("</object></child>");
    };
    let layout = crate::ui::menu_layout(false);
    let mut app_rows = Vec::new();
    for group in &layout {
        let rows: Vec<(String, String)> = group
            .entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Item(mi) if glue::action_name_for(&mi.action).is_some() => {
                    mi.accel.map(|a| (mi.label.clone(), glue::accel_string(&a)))
                }
                _ => None,
            })
            .collect();
        if group.id == MenuGroupId::App {
            app_rows = rows;
        } else {
            add_group(&group.title, &rows);
        }
    }
    app_rows.push((shortcuts_label(), SHORTCUTS_ACCEL.to_string()));
    add_group("freemkv", &app_rows);
    format!(
        "<interface><object class=\"GtkShortcutsWindow\" id=\"shortcuts\"><property name=\"modal\">1</property><child><object class=\"GtkShortcutsSection\"><property name=\"section-name\">main</property>{groups}</object></child></object></interface>"
    )
}

#[cfg(test)]
#[path = "linux_tests.rs"]
mod tests;
