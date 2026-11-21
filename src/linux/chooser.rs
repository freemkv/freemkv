//! The Linux selection surface: single-line fields opening a popover below them.
use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// The tallest the option list grows before it scrolls inside the popover.
const MAX_HEIGHT: i32 = 360;

/// A single-line field showing title and current value. Its options open in a popover
/// directly below it, so opening never changes the layout around the field.
pub(super) struct Field {
    pub row: adw::ActionRow,
    pub popover: gtk::Popover,
    list: gtk::ListBox,
}

impl std::ops::Deref for Field {
    type Target = adw::ActionRow;
    fn deref(&self) -> &adw::ActionRow {
        &self.row
    }
}

impl Field {
    pub fn new(title: &str) -> Self {
        let row = adw::ActionRow::builder()
            .title(title)
            .use_markup(false)
            .activatable(true)
            .build();
        row.add_css_class("freemkv-chooser");
        row.set_title_lines(0);
        row.set_subtitle_lines(0);
        row.add_suffix(&gtk::Image::from_icon_name("pan-down-symbolic"));

        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(MAX_HEIGHT)
            .child(&list)
            .build();
        let popover = gtk::Popover::builder()
            .has_arrow(false)
            .position(gtk::PositionType::Bottom)
            .child(&scroll)
            .build();
        popover.add_css_class("freemkv-chooser-popover");
        popover.set_parent(&row);

        let weak = popover.downgrade();
        row.connect_activated(move |_| {
            if let Some(pop) = weak.upgrade() {
                pop.popup();
            }
        });
        let sized = row.downgrade();
        popover.connect_show(move |pop| {
            if let Some(row) = sized.upgrade() {
                pop.set_size_request(row.width(), -1);
            }
        });
        let focus = row.downgrade();
        popover.connect_closed(move |_| {
            if let Some(row) = focus.upgrade() {
                row.grab_focus();
            }
        });
        let pop = popover.clone();
        row.connect_destroy(move |_| pop.unparent());
        Field { row, popover, list }
    }

    pub fn add_row(&self, row: &adw::ActionRow) {
        self.list.append(row);
    }

    pub fn remove(&self, row: &adw::ActionRow) {
        self.list.remove(row);
    }

    pub fn close(&self) {
        self.popover.popdown();
    }
}

pub(super) fn option(label: &str, check: &gtk::CheckButton) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(label)
        .use_markup(false)
        .activatable_widget(check)
        .build();
    row.set_title_lines(0);
    row.add_prefix(check);
    row.add_css_class("freemkv-choice");
    row
}

/// Standalone rows need a ListBox to receive the same layout as PreferencesGroup.
pub(super) fn surface(row: &Field) -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    list.append(&row.row);
    list
}

type Changed = Box<dyn Fn(&Choice)>;
pub(super) struct Choice {
    pub row: Field,
    model: gtk::StringList,
    rows: RefCell<Vec<(adw::ActionRow, gtk::CheckButton)>>,
    selected: Cell<u32>,
    painting: Cell<bool>,
    changed: RefCell<Vec<Changed>>,
}
impl Choice {
    pub fn new(title: &str, model: &gtk::StringList) -> Rc<Self> {
        let me = Rc::new(Self {
            row: Field::new(title),
            model: model.clone(),
            rows: RefCell::default(),
            selected: Cell::new(gtk::INVALID_LIST_POSITION),
            painting: Cell::new(false),
            changed: RefCell::default(),
        });
        me.row.add_css_class("freemkv-single-choice");
        let weak = Rc::downgrade(&me);
        model.connect_items_changed(move |_, _, _, _| {
            if let Some(me) = weak.upgrade() {
                me.rebuild();
            }
        });
        me.rebuild();
        me
    }
    fn rebuild(self: &Rc<Self>) {
        for (row, _) in self.rows.borrow_mut().drain(..) {
            self.row.remove(&row);
        }
        let mut leader: Option<gtk::CheckButton> = None;
        for at in 0..self.model.n_items() {
            // Grouped check buttons draw as stock radio buttons: one choice, not several.
            let check = gtk::CheckButton::new();
            check.set_group(leader.as_ref());
            leader.get_or_insert_with(|| check.clone());
            let row = option(self.model.string(at).as_deref().unwrap_or(""), &check);
            let weak = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(me) = weak.upgrade() {
                    me.row.close();
                }
            });
            let weak = Rc::downgrade(self);
            check.connect_toggled(move |check| {
                let Some(me) = weak.upgrade() else { return };
                if me.painting.get() {
                    return;
                }
                if !check.is_active() {
                    // Switching options turns the old one off first; only a lone "off" restores.
                    let weak = Rc::downgrade(&me);
                    gtk::glib::idle_add_local_once(move || {
                        let Some(me) = weak.upgrade() else { return };
                        if !me.rows.borrow().iter().any(|(_, c)| c.is_active()) {
                            me.set_selected(me.selected.get());
                        }
                    });
                    return;
                }
                let changed = me.selected.get() != at;
                me.set_selected(at);
                me.row.close();
                if changed {
                    for cb in me.changed.borrow().iter() {
                        cb(&me);
                    }
                }
            });
            self.row.add_row(&row);
            self.rows.borrow_mut().push((row, check));
        }
        self.set_selected(self.selected.get());
    }
    pub fn selected(&self) -> u32 {
        self.selected.get()
    }
    pub fn set_selected(&self, at: u32) {
        let at = if at < self.model.n_items() {
            at
        } else if self.model.n_items() > 0 {
            0
        } else {
            gtk::INVALID_LIST_POSITION
        };
        self.selected.set(at);
        self.painting.set(true);
        for (index, (_, check)) in self.rows.borrow().iter().enumerate() {
            check.set_active(index as u32 == at);
        }
        let label = (at < self.model.n_items())
            .then(|| self.model.string(at))
            .flatten();
        self.row.set_subtitle(label.as_deref().unwrap_or(""));
        self.painting.set(false);
    }
    pub fn connect_selected_notify(&self, cb: impl Fn(&Self) + 'static) {
        self.changed.borrow_mut().push(Box::new(cb));
    }
}
