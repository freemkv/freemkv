//! The Linux selection surface: full-width rows expanding below their field.
use gtk4 as gtk;
use gtk4::prelude::*;
use libadwaita as adw;
use libadwaita::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub(super) fn expander(title: &str) -> adw::ExpanderRow {
    let row = adw::ExpanderRow::builder()
        .title(title)
        .use_markup(false)
        .build();
    row.add_css_class("freemkv-chooser");
    row.set_title_lines(0);
    row.set_subtitle_lines(0);
    row
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
pub(super) fn surface(row: &adw::ExpanderRow) -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    list.append(row);
    list
}

type Changed = Box<dyn Fn(&Choice)>;
pub(super) struct Choice {
    pub row: adw::ExpanderRow,
    model: gtk::StringList,
    rows: RefCell<Vec<(adw::ActionRow, gtk::CheckButton)>>,
    selected: Cell<u32>,
    painting: Cell<bool>,
    changed: RefCell<Vec<Changed>>,
}
impl Choice {
    pub fn new(title: &str, model: &gtk::StringList) -> Rc<Self> {
        let me = Rc::new(Self {
            row: expander(title),
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
        for at in 0..self.model.n_items() {
            let check = gtk::CheckButton::new();
            let row = option(self.model.string(at).as_deref().unwrap_or(""), &check);
            let weak = Rc::downgrade(self);
            check.connect_toggled(move |_| {
                let Some(me) = weak.upgrade() else { return };
                if me.painting.get() {
                    return;
                }
                let changed = me.selected.get() != at;
                me.set_selected(at);
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
