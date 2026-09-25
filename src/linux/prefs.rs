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
use crate::ui::LogKind;

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
        let r = adw::ActionRow::builder()
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

fn group(desc: Option<&str>) -> adw::PreferencesGroup {
    let grp = adw::PreferencesGroup::new();
    if let Some(d) = desc {
        grp.set_description(Some(&glib::markup_escape_text(d)));
    }
    grp
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
        let r = adw::ComboRow::builder()
            .title(row_title(label))
            .model(&gtk::StringList::new(&refs))
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

fn page(title: &str, icon: &str, groups: &[adw::PreferencesGroup]) -> adw::PreferencesPage {
    let p = adw::PreferencesPage::builder()
        .title(title)
        .name(title)
        .icon_name(icon)
        .build();
    for grp in groups {
        p.add(grp);
    }
    p
}

/// Open Settings, rebuilt fresh from the stored settings so it is always in
/// the current language. `page` reopens on a given tab (the language switch).
pub(super) fn show(shell: &Rc<Shell>, page_name: Option<String>) {
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
        .default_width(680)
        .default_height(640)
        .build();
    let mut f = Form::default();

    // ── Output
    let o1 = group(None);
    f.combo(&o1, "container", &g("gui.set.default_output"));
    f.path(shell, &o1, "dest_dir", &g("gui.set.default_dest"), true);
    f.entry(&o1, "filename_template", &g("gui.set.filename_template"));
    let o2 = group(None);
    f.switch(&o2, "keep_iso", &g("gui.set.keep_iso"));
    f.switch(&o2, "auto_eject", &g("gui.set.auto_eject"));
    window.add(&page(
        &g("gui.tab.output"),
        "folder-videos-symbolic",
        &[o1, o2],
    ));

    // ── Selection
    let s1 = group(Some(&g("gui.set.min_length_note")));
    f.combo(&s1, "selection", &g("gui.set.default_selection"));
    f.entry(&s1, "min_title_secs", &g("gui.set.min_length"));
    let s2 = group(Some(&g("gui.set.lang_prefs_note")));
    f.langs(&s2, "audio_langs", &g("gui.set.audio_langs"));
    f.langs(&s2, "sub_langs", &g("gui.set.sub_langs"));
    f.langs(&s2, "forced_sub_langs", &g("gui.set.forced_sub_langs"));
    window.add(&page(
        &g("gui.tab.selection"),
        "object-select-symbolic",
        &[s1, s2],
    ));

    // ── Recovery
    let r1 = group(Some(&g("gui.set.abort_lost_note")));
    f.combo(&r1, "rip_mode", &g("gui.set.rip_mode"));
    f.entry(&r1, "max_passes", &g("gui.set.max_passes"));
    f.entry(&r1, "abort_lost_secs", &g("gui.set.abort_lost"));
    let r2 = group(Some(&g("gui.set.raw_note")));
    f.switch(&r2, "raw", &g("gui.set.keep_encrypted"));
    let r3 = group(Some(&g("gui.set.capture_note")));
    f.switch(&r3, "force", &g("gui.set.overwrite"));
    window.add(&page(
        &g("gui.tab.recovery"),
        "media-optical-symbolic",
        &[r1, r2, r3],
    ));

    // ── Keys
    let k1 = group(None);
    f.combo(&k1, "key_source", &g("gui.set.key_source"));
    let k2 = group(None);
    f.path(shell, &k2, "keydb_path", &g("gui.set.keydb_path"), false);
    f.entry(&k2, "keydb_url", &g("gui.set.keydb_url"));
    let keydb_row = adw::ActionRow::builder()
        .title(g("gui.set.update_keydb"))
        .subtitle(glib::markup_escape_text(
            &shell.settings.borrow().keydb_status(),
        ))
        .activatable(true)
        .build();
    keydb_row.add_suffix(&gtk::Image::from_icon_name("folder-download-symbolic"));
    k2.add(&keydb_row);
    let k3 = group(None);
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
        &g("gui.tab.keys"),
        "dialog-password-symbolic",
        &[k1, k2, k3],
    ));

    // ── Advanced
    let a1 = group(None);
    let language = f.combo(&a1, "language", &g("gui.set.language"));
    let a2 = group(Some(&g("gui.set.decrypt_threads_note")));
    f.entry(&a2, "decrypt_threads", &g("gui.set.decrypt_threads"));
    let a3 = group(None);
    f.combo(&a3, "log_level", &g("gui.set.log_detail"));
    window.add(&page(
        &g("gui.tab.advanced"),
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
    });
    prefs.populate(&shell.settings.borrow());
    prefs.set_keydb_updating(shell.keydb_updating.get());
    if let Some(name) = page_name {
        window.set_visible_page_name(&name);
    }

    let (me, p) = (shell.clone(), prefs.clone());
    keydb_row.connect_activated(move |_| p.update_keydb(&me));
    let (me, p) = (shell.clone(), prefs.clone());
    test_row.connect_activated(move |_| p.test_keyserver(&me));
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

    /// Commit the form: into the stored copy, into the running `App` (so it
    /// applies now), and to disk. The live output folder follows only when
    /// the DEFAULT destination actually changed, so a one-off pick survives.
    fn commit(&self, shell: &Rc<Shell>) {
        let before = serde_json::to_value(&*shell.settings.borrow()).ok();
        let old_dest = shell.settings.borrow().dest_dir.clone();
        self.read_form(&mut shell.settings.borrow_mut());
        let edited = shell.settings.borrow().clone();
        if serde_json::to_value(&edited).ok() == before {
            return;
        }
        let new_dest = edited.dest_dir.clone();
        let dest_changed = new_dest != old_dest && !new_dest.trim().is_empty();
        shell.app_mut(|a| {
            a.settings = edited;
            if dest_changed {
                a.output_dir = new_dest;
            }
        });
        shell.save_settings_reporting_error();
    }

    /// The language applies at once: commit, swap the catalog, rebuild the
    /// main window, and reopen Settings on the same tab — now translated.
    fn apply_language(self: &Rc<Self>, shell: &Rc<Shell>) {
        let tab = self.window.visible_page_name().map(|s| s.to_string());
        self.commit(shell);
        let lang = shell.settings.borrow().language.clone();
        crate::app_entry::apply_locale(&lang, super::system_locale_code);
        self.window.close();
        shell.relocalize();
        show(shell, tab);
    }

    fn toast(&self, text: &str) {
        self.window
            .add_toast(adw::Toast::new(&glib::markup_escape_text(text)));
    }

    // Validated by the same rule the key layer uses, so the UI cannot accept
    // a URL the engine would later reject.
    fn test_keyserver(&self, shell: &Rc<Shell>) {
        let url = self.field("keyserver_url");
        let msg = if url.trim().is_empty() {
            g("gui.log.no_keyserver")
        } else {
            match freemkv_keysources::validate_keyserver_url(&url) {
                Ok(_) => crate::strings::fmt("gui.log.keyserver_valid", &[("url", &url)]),
                Err(e) => {
                    crate::strings::fmt("gui.log.keyserver_rejected", &[("e", &e.to_string())])
                }
            }
        };
        shell.say(LogKind::Result, &msg);
        self.toast(&msg);
    }

    /// Download the keydb from the LIVE fields (works before the window is
    /// closed), on a worker thread; the shell's drain reports the result.
    fn update_keydb(&self, shell: &Rc<Shell>) {
        if shell.keydb_updating.get() {
            self.set_keydb_note(&crate::strings::get_or(
                "gui.set.keydb_busy",
                "A keydb update is already running — please wait.",
            ));
            return;
        }
        let (mut url, mut path) = (self.field("keydb_url"), self.field("keydb_path"));
        if url.is_empty() {
            url = shell.settings.borrow().keydb_url.clone();
        }
        if path.is_empty() {
            path = shell.settings.borrow().keydb_path.clone();
        }
        if url.trim().is_empty() {
            self.set_keydb_note(&g("gui.set.keydb_no_url"));
            return;
        }
        shell.say(LogKind::Result, &g("gui.log.fetching_keydb"));
        self.set_keydb_note(&g("gui.set.keydb_updating"));
        shell.set_keydb_updating(true);
        let inbox = shell.inbox.clone();
        std::thread::spawn(move || {
            // A panic must still push a terminal message, or Update stays
            // disabled for the rest of the session.
            let msg = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match crate::settings::update_keydb(&url, &path) {
                    Ok(m) => m,
                    Err(e) => e,
                }
            }))
            .unwrap_or_else(|_| "keydb update failed — internal error".to_string());
            inbox.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
        });
        shell.start_drain();
    }

    pub(super) fn set_keydb_note(&self, text: &str) {
        self.keydb_row.set_subtitle(&glib::markup_escape_text(text));
    }

    pub(super) fn set_keydb_updating(&self, updating: bool) {
        self.keydb_row.set_sensitive(!updating);
    }
}
