//! Settings as an `AdwPreferencesWindow`: the same five tabs and fields as
//! the macOS/Windows Settings windows, read back by the same keys. GNOME
//! preference windows have no OK/Cancel, so the form is committed when the
//! window closes (only if something changed); the interface language applies
//! the moment it is picked, as on the other shells.

use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;

use super::Shell;
use crate::linux_glue::{self as glue, row_title};
use crate::ui::Cmd;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// A multi-select language picker: an expander row with one tick per
/// language. The stored string lives on the picker, so nothing reaches
/// `Settings` until the window commits.
struct LangRow {
    row: adw::ExpanderRow,
    value: RefCell<String>,
    checks: RefCell<Vec<(String, gtk::CheckButton)>>,
    painting: Cell<bool>,
}

impl LangRow {
    fn new(title: &str) -> Rc<Self> {
        let row = adw::ExpanderRow::builder().title(row_title(title)).build();
        Rc::new(LangRow {
            row,
            value: RefCell::new(String::new()),
            checks: RefCell::new(Vec::new()),
            painting: Cell::new(false),
        })
    }

    fn add_code(self: &Rc<Self>, code: &str) {
        let cb = gtk::CheckButton::new();
        // Plain text: an unknown stored tag is shown verbatim, `&` and all.
        let r = adw::ActionRow::builder()
            .use_markup(false)
            .title(crate::ui::lang_display_name(code))
            .activatable_widget(&cb)
            .build();
        r.add_prefix(&cb);
        let me = self.clone();
        let c = code.to_string();
        cb.connect_toggled(move |_| {
            if me.painting.get() {
                return;
            }
            let next = crate::ui::lang_toggle(&me.value.borrow(), &c);
            me.set(&next);
        });
        self.row.add_row(&r);
        self.checks.borrow_mut().push((code.to_string(), cb));
    }

    fn set(self: &Rc<Self>, stored: &str) {
        for code in glue::lang_picker_codes(stored) {
            let known = self
                .checks
                .borrow()
                .iter()
                .any(|(c, _)| c.eq_ignore_ascii_case(&code));
            if !known {
                self.add_code(&code);
            }
        }
        *self.value.borrow_mut() = stored.to_string();
        self.row
            .set_subtitle(&glib::markup_escape_text(&crate::ui::lang_summary(stored)));
        self.painting.set(true);
        for (code, cb) in self.checks.borrow().iter() {
            cb.set_active(crate::ui::lang_is_selected(stored, code));
        }
        self.painting.set(false);
    }
}

pub(super) struct Prefs {
    window: adw::PreferencesWindow,
    entries: Vec<(&'static str, adw::EntryRow)>,
    switches: Vec<(&'static str, adw::SwitchRow)>,
    combos: Vec<(&'static str, adw::ComboRow)>,
    langs: Vec<(&'static str, Rc<LangRow>)>,
    keydb_row: adw::ActionRow,
    /// Test was pressed: the key-service verdict is toasted when it lands.
    awaiting_verdict: Cell<bool>,
}

/// Collects rows while the pages are built, so the form can be read back by
/// key — a row missing from here would be write-only.
#[derive(Default)]
struct Form {
    entries: Vec<(&'static str, adw::EntryRow)>,
    switches: Vec<(&'static str, adw::SwitchRow)>,
    combos: Vec<(&'static str, adw::ComboRow)>,
    langs: Vec<(&'static str, Rc<LangRow>)>,
}

fn g(key: &str) -> String {
    crate::strings::get(key)
}

/// A settings section: a heading (catalog key) over its rows, as GNOME apps lay
/// them out, with an optional note under the heading.
fn group(title: &str, desc: Option<&str>) -> adw::PreferencesGroup {
    let grp = adw::PreferencesGroup::new();
    grp.set_title(&glib::markup_escape_text(&g(title)));
    if let Some(d) = desc {
        grp.set_description(Some(&glib::markup_escape_text(d)));
    }
    grp
}

/// A dropdown popup row for a `GtkStringList`: the label wraps instead of
/// ellipsizing, with a tick on the current choice as the default list has.
fn wrapping_list_factory() -> gtk::SignalListItemFactory {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_hexpand(true);
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_max_width_chars(48);
        let tick = gtk::Image::from_icon_name("object-select-symbolic");
        item.bind_property("selected", &tick, "opacity")
            .transform_to(|_, sel: bool| Some(if sel { 1.0_f64 } else { 0.0 }))
            .sync_create()
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&label);
        row.append(&tick);
        item.set_child(Some(&row));
    });
    f.connect_bind(|_, obj| {
        let Some(item) = obj.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string());
        let label = item
            .child()
            .and_then(|row| row.first_child())
            .and_downcast::<gtk::Label>();
        if let (Some(label), Some(text)) = (label, text) {
            label.set_text(&text);
        }
    });
    f
}

impl Form {
    fn entry(
        &mut self,
        grp: &adw::PreferencesGroup,
        key: &'static str,
        label: &str,
    ) -> adw::EntryRow {
        let r = adw::EntryRow::builder().title(row_title(label)).build();
        grp.add(&r);
        self.entries.push((key, r.clone()));
        r
    }

    fn secret(&mut self, grp: &adw::PreferencesGroup, key: &'static str, label: &str) {
        let r = adw::PasswordEntryRow::builder()
            .title(row_title(label))
            .build();
        grp.add(&r);
        self.entries.push((key, r.upcast()));
    }

    fn switch(&mut self, grp: &adw::PreferencesGroup, key: &'static str, label: &str) {
        let r = adw::SwitchRow::builder().title(row_title(label)).build();
        grp.add(&r);
        self.switches.push((key, r));
    }

    fn combo(
        &mut self,
        grp: &adw::PreferencesGroup,
        key: &'static str,
        label: &str,
    ) -> adw::ComboRow {
        let labels: Vec<String> = glue::settings_options(key)
            .into_iter()
            .map(|(_, l)| l)
            .collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        // The choice shows in full under the title, and the popup wraps long
        // labels: the default factory cuts both at about 20 characters.
        let r = adw::ComboRow::builder()
            .title(row_title(label))
            .model(&gtk::StringList::new(&refs))
            .use_subtitle(true)
            .list_factory(&wrapping_list_factory())
            .build();
        grp.add(&r);
        self.combos.push((key, r.clone()));
        r
    }

    fn langs(&mut self, grp: &adw::PreferencesGroup, key: &'static str, label: &str) {
        let l = LangRow::new(label);
        grp.add(&l.row);
        self.langs.push((key, l));
    }

    /// A text row with a chooser button that fills it: a folder for the
    /// destination, any file for the keydb.
    fn path(
        &mut self,
        shell: &Rc<Shell>,
        grp: &adw::PreferencesGroup,
        key: &'static str,
        label: &str,
        folder: bool,
    ) {
        let r = self.entry(grp, key, label);
        let btn = gtk::Button::from_icon_name(if folder {
            "folder-open-symbolic"
        } else {
            "document-open-symbolic"
        });
        btn.set_valign(gtk::Align::Center);
        btn.add_css_class("flat");
        let title = if folder {
            g("gui.panel.output_msg")
        } else {
            row_title(&g("gui.set.keydb_path"))
        };
        btn.set_tooltip_text(Some(&title));
        let me = shell.clone();
        let target = r.clone();
        btn.connect_clicked(move |_| {
            let t = target.clone();
            let initial = if folder {
                t.text().to_string()
            } else {
                String::new()
            };
            me.pick(folder, &title, false, &initial, move |p| t.set_text(&p));
        });
        r.add_suffix(&btn);
    }
}

/// `name` is the tab's catalog key, not its (translated) title, so a language
/// switch can reopen the same tab.
fn page(name: &str, icon: &str, groups: &[adw::PreferencesGroup]) -> adw::PreferencesPage {
    let p = adw::PreferencesPage::builder()
        .title(g(name))
        .name(name)
        .icon_name(icon)
        .build();
    for grp in groups {
        p.add(grp);
    }
    p
}

/// Open Settings, rebuilt fresh from `st` (the core's form) so it is always in
/// the current language. `page` reopens on a given tab (the language switch).
pub(super) fn show(shell: &Rc<Shell>, st: &crate::settings::Settings, page_name: Option<String>) {
    if let Some(p) = shell.prefs.borrow().as_ref() {
        p.window.present();
        return;
    }
    let window = adw::PreferencesWindow::builder()
        .title(g("gui.win.settings"))
        .transient_for(&shell.window)
        .modal(true)
        .destroy_with_parent(true)
        .search_enabled(false)
        .default_width(760)
        .default_height(640)
        .build();
    let mut f = Form::default();

    // ── Output
    let o1 = group("gui.set.group_files", None);
    f.combo(&o1, "container", &g("gui.set.default_output"));
    f.path(shell, &o1, "dest_dir", &g("gui.set.default_dest"), true);
    f.entry(&o1, "filename_template", &g("gui.set.filename_template"));
    f.secret(&o1, "tmdb_api_key", &g("gui.set.tmdb_api_key"));
    f.combo(&o1, "startup_mode", &g("gui.set.startup_mode"));
    f.entry(&o1, "preferred_drive", &g("gui.set.preferred_drive"));
    let o2 = group("gui.set.group_after_rip", None);
    f.switch(&o2, "keep_iso", &g("gui.set.keep_iso"));
    f.switch(&o2, "auto_eject", &g("gui.set.auto_eject"));
    f.switch(
        &o2,
        "notify_when_rip_finished",
        &crate::strings::get_or(
            "gui.set.notify_when_rip_finished",
            "Notify when a rip finishes",
        ),
    );
    window.add(&page("gui.tab.output", "folder-videos-symbolic", &[o1, o2]));

    // ── Selection
    let s1 = group("gui.set.group_titles", Some(&g("gui.set.min_length_note")));
    f.combo(&s1, "selection", &g("gui.set.default_selection"));
    f.entry(&s1, "min_title_secs", &g("gui.set.min_length"));
    f.entry(&s1, "tv_season", &g("gui.set.tv_season"));
    f.entry(&s1, "tv_first_episode", &g("gui.set.tv_first_episode"));
    let s2 = group(
        "gui.set.group_languages",
        Some(&g("gui.set.lang_prefs_note")),
    );
    f.entry(
        &s2,
        "presentation_language",
        &crate::ui::presentation_language_label(),
    );
    f.langs(&s2, "audio_langs", &g("gui.set.audio_langs"));
    f.langs(&s2, "sub_langs", &g("gui.set.sub_langs"));
    f.langs(&s2, "forced_sub_langs", &g("gui.set.forced_sub_langs"));
    f.switch(
        &s2,
        "persist_stream_preferences",
        &crate::strings::get_or(
            "gui.set.persist_stream_preferences",
            "Remember audio and subtitle choices",
        ),
    );
    window.add(&page(
        "gui.tab.selection",
        "object-select-symbolic",
        &[s1, s2],
    ));

    // ── Recovery
    let r1 = group(
        "gui.set.group_recovery",
        Some(&g("gui.set.abort_lost_note")),
    );
    f.combo(&r1, "rip_mode", &g("gui.set.rip_mode"));
    f.entry(&r1, "max_passes", &g("gui.set.max_passes"));
    f.entry(&r1, "abort_lost_secs", &g("gui.set.abort_lost"));
    let r2 = group("gui.set.group_raw", Some(&g("gui.set.raw_note")));
    f.switch(&r2, "raw", &g("gui.set.keep_encrypted"));
    let r3 = group("gui.set.group_existing", None);
    f.switch(&r3, "force", &g("gui.set.overwrite"));
    window.add(&page(
        "gui.tab.recovery",
        "media-optical-symbolic",
        &[r1, r2, r3],
    ));

    // ── Keys
    let k1 = group("gui.set.group_key_source", None);
    f.combo(&k1, "key_source", &g("gui.set.key_source"));
    let k2 = group("gui.set.group_keydb", None);
    f.path(shell, &k2, "keydb_path", &g("gui.set.keydb_path"), false);
    f.entry(&k2, "keydb_url", &g("gui.set.keydb_url"));
    let keydb_row = adw::ActionRow::builder()
        .title(g("gui.set.update_keydb"))
        .subtitle(glib::markup_escape_text(&st.keydb_status()))
        .activatable(true)
        .build();
    keydb_row.add_suffix(&gtk::Image::from_icon_name("folder-download-symbolic"));
    k2.add(&keydb_row);
    let k3 = group("gui.set.group_key_service", None);
    f.entry(&k3, "keyserver_url", &g("gui.set.keyserver_url"));
    f.secret(&k3, "keyserver_token", &g("gui.set.keyserver_token"));
    let test_row = adw::ActionRow::builder()
        .title(g("gui.set.test_connection"))
        .activatable(true)
        .build();
    test_row.add_suffix(&gtk::Image::from_icon_name(
        "network-transmit-receive-symbolic",
    ));
    k3.add(&test_row);
    window.add(&page(
        "gui.tab.keys",
        "dialog-password-symbolic",
        &[k1, k2, k3],
    ));

    // ── Advanced
    let a1 = group("gui.set.group_interface", None);
    let language = f.combo(&a1, "language", &g("gui.set.language"));
    let a2 = group(
        "gui.set.group_performance",
        Some(&g("gui.set.decrypt_threads_note")),
    );
    f.entry(&a2, "decrypt_threads", &g("gui.set.decrypt_threads"));
    let a3 = group("gui.set.group_logging", None);
    f.combo(&a3, "log_level", &g("gui.set.log_detail"));
    window.add(&page(
        "gui.tab.advanced",
        "preferences-system-symbolic",
        &[a1, a2, a3],
    ));

    let prefs = Rc::new(Prefs {
        window: window.clone(),
        entries: f.entries,
        switches: f.switches,
        combos: f.combos,
        langs: f.langs,
        keydb_row: keydb_row.clone(),
        awaiting_verdict: Cell::new(false),
    });
    prefs.populate(st);
    let v = shell.app.borrow().view();
    prefs.paint_keydb(v.keydb_note.as_deref(), v.keydb_busy);
    if let Some(name) = page_name {
        window.set_visible_page_name(&name);
    }

    let (me, p) = (shell.clone(), prefs.clone());
    keydb_row.connect_activated(move |_| p.update_keydb(&me));
    let (me, p) = (shell.clone(), prefs.clone());
    test_row.connect_activated(move |_| p.test_keyserver(&me));
    // A text field commits on Enter and when focus leaves it, through the
    // same `commit` as closing, which writes only when something changed.
    // Focus also leaves while the window is torn down; that is not an edit.
    for (_, r) in &prefs.entries {
        let (me, p) = (shell.clone(), prefs.clone());
        r.connect_entry_activated(move |_| {
            p.commit(&me);
            gtk::prelude::GtkWindowExt::set_focus(&p.window, None::<&gtk::Widget>);
        });
        let focus = gtk::EventControllerFocus::new();
        let (me, p) = (shell.clone(), prefs.clone());
        focus.connect_leave(move |_| {
            if p.window.is_visible() {
                p.commit(&me);
            }
        });
        r.add_controller(focus);
    }
    // Blank space has no focus target of its own. Finish editing there too,
    // without claiming the gesture: buttons and other fields still receive it.
    let click = gtk::GestureClick::new();
    click.set_button(1);
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    let p = Rc::downgrade(&prefs);
    click.connect_pressed(move |_, _, x, y| {
        let Some(p) = p.upgrade() else { return };
        let focused = gtk::prelude::GtkWindowExt::focus(&p.window);
        let editing = focused.is_some_and(|w| in_entry_row(&w));
        let target = p.window.pick(x, y, gtk::PickFlags::DEFAULT);
        if editing && !target.is_some_and(|w| in_entry_row(&w)) {
            gtk::prelude::GtkWindowExt::set_focus(&p.window, None::<&gtk::Widget>);
        }
    });
    prefs.window.add_controller(click);
    // Connected after `populate`, so filling the form is not a language pick.
    let (me, p) = (shell.clone(), prefs.clone());
    language.connect_selected_notify(move |_| {
        let (me, p) = (me.clone(), p.clone());
        // Deferred: the rebuild destroys the row whose signal is running.
        glib::idle_add_local_once(move || p.apply_language(&me));
    });
    let (me, p) = (shell.clone(), prefs.clone());
    window.connect_close_request(move |_| {
        p.commit(&me);
        me.prefs.borrow_mut().take();
        glib::Propagation::Proceed
    });

    *shell.prefs.borrow_mut() = Some(prefs);
    window.present();
}

fn in_entry_row(widget: &gtk::Widget) -> bool {
    let mut at = Some(widget.clone());
    while let Some(widget) = at {
        if widget.is::<adw::EntryRow>() {
            return true;
        }
        at = widget.parent();
    }
    false
}

impl Prefs {
    fn populate(&self, st: &crate::settings::Settings) {
        for (k, r) in &self.entries {
            r.set_text(&st.get(k));
        }
        for (k, r) in &self.switches {
            r.set_active(st.get_bool(k));
        }
        for (k, r) in &self.combos {
            r.set_selected(glue::option_index(&glue::settings_options(k), &st.get(k)));
        }
        for (k, l) in &self.langs {
            l.set(&st.get(k));
        }
    }

    /// Every control back into `st` — canonical values, never labels.
    fn read_form(&self, st: &mut crate::settings::Settings) {
        for (k, r) in &self.entries {
            st.set(k, r.text().to_string());
        }
        for (k, r) in &self.switches {
            st.set_bool(k, r.is_active());
        }
        for (k, r) in &self.combos {
            let opts = glue::settings_options(k);
            if let Some((canon, _)) = opts.get(r.selected() as usize) {
                st.set(k, (*canon).to_string());
            }
        }
        for (k, l) in &self.langs {
            st.set(k, l.value.borrow().clone());
        }
    }

    fn field(&self, key: &str) -> String {
        self.entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, r)| r.text().to_string())
            .unwrap_or_default()
    }

    /// Commit the form to the core, which applies it at once and saves only
    /// what changed.
    fn commit(&self, shell: &Rc<Shell>) {
        let mut edited = shell.app.borrow().settings_form().clone();
        self.read_form(&mut edited);
        shell.act(Cmd::CommitSettings(Box::new(edited)));
        // A refused value (an unknown presentation language) shows the kept one again.
        let base = shell.app.borrow().settings_form().clone();
        for (k, r) in &self.entries {
            let kept = base.get(k);
            if r.text() != kept.as_str() {
                r.set_text(&kept);
            }
        }
    }

    /// The language applies at once: commit, swap the catalog, rebuild the
    /// main window, and reopen Settings on the same tab — now translated.
    fn apply_language(self: &Rc<Self>, shell: &Rc<Shell>) {
        let tab = self.window.visible_page_name().map(|s| s.to_string());
        self.commit(shell);
        let lang = shell.app.borrow().settings.language.clone();
        // The LIVE switch; "auto" makes freemkv-i18n re-detect from the
        // environment, exactly as the CLI and the startup path do.
        crate::strings::set_locale(crate::ui::locale_code(&lang));
        self.window.close();
        shell.relocalize();
        let form = shell.app.borrow().settings_form().clone();
        show(shell, &form, tab);
    }

    fn toast(&self, text: &str) {
        self.window
            .add_toast(adw::Toast::new(&glib::markup_escape_text(text)));
    }

    // The core checks the live URL (the host lookup off the UI thread) and logs its
    // verdict; `paint_keyserver` toasts it here too once it lands.
    fn test_keyserver(&self, shell: &Rc<Shell>) {
        self.awaiting_verdict.set(true);
        shell.act(Cmd::TestKeyserver(self.field("keyserver_url")));
    }

    /// Toast the verdict of the Test pressed in this window, once it lands.
    pub(super) fn paint_keyserver(&self, verdict: Option<&str>) {
        if let Some(text) = verdict.filter(|_| self.awaiting_verdict.get()) {
            self.awaiting_verdict.set(false);
            self.toast(text);
        }
    }

    /// Download the keydb from the LIVE fields (works before the window is
    /// closed); the core runs it and reports through the view.
    fn update_keydb(&self, shell: &Rc<Shell>) {
        let url = self.field("keydb_url");
        let path = self.field("keydb_path");
        shell.act(Cmd::UpdateKeydb { url, path });
    }

    /// The Keys row from the view: the update's note (else the status it was
    /// built with), insensitive while one runs.
    pub(super) fn paint_keydb(&self, note: Option<&str>, busy: bool) {
        if let Some(text) = note {
            let escaped = glib::markup_escape_text(text);
            if self.keydb_row.subtitle().as_deref() != Some(escaped.as_str()) {
                self.keydb_row.set_subtitle(&escaped);
            }
        }
        self.keydb_row.set_sensitive(!busy);
    }
}
