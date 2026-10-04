//! The selection bar over the title tree: a Titles dropdown and the Audio and Subtitles menus,
//! filled from the core's `PickView`. A choice only reports WHICH entry was picked; what it
//! means, and the re-ticked tree, are the core's.

use gtk4 as gtk;
use gtk4::prelude::*;

use super::Shell;
use crate::ui::{PickEntry, PickView};

use std::cell::RefCell;
use std::rc::Rc;

type Shown = Rc<RefCell<Option<PickView>>>;

/// A menu button whose popover lists the core's entries as tick boxes. It stays open, so
/// several languages can be ticked in one go.
struct PickMenu {
    button: gtk::MenuButton,
    list: gtk::Box,
    checks: RefCell<Vec<gtk::CheckButton>>,
    /// The entries' labels, tags and separators the list was built from.
    shape: RefCell<Vec<(String, isize, bool)>>,
    on_pick: Rc<dyn Fn(isize)>,
}

impl PickMenu {
    fn new(on_pick: Rc<dyn Fn(isize)>) -> Self {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let popover = gtk::Popover::new();
        popover.set_child(Some(&list));
        let button = gtk::MenuButton::builder()
            .popover(&popover)
            .always_show_arrow(true)
            .build();
        PickMenu {
            button,
            list,
            checks: RefCell::default(),
            shape: RefCell::default(),
            on_pick,
        }
    }

    /// Show the closed label and the entries, rebuilding the list only when the entries
    /// themselves changed; otherwise just the ticks.
    fn fill(&self, summary: &str, entries: &[PickEntry]) {
        self.button.set_label(summary);
        let shape: Vec<_> = entries
            .iter()
            .map(|e| (e.label.clone(), e.tag, e.separator_before))
            .collect();
        if *self.shape.borrow() != shape {
            while let Some(c) = self.list.first_child() {
                self.list.remove(&c);
            }
            let mut checks = Vec::new();
            for e in entries {
                if e.separator_before {
                    self.list
                        .append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                }
                let cb = gtk::CheckButton::with_label(&e.label);
                let (on_pick, tag) = (self.on_pick.clone(), e.tag);
                cb.connect_toggled(move |_| on_pick(tag));
                self.list.append(&cb);
                checks.push(cb);
            }
            *self.checks.borrow_mut() = checks;
            *self.shape.borrow_mut() = shape;
        }
        self.sync(entries);
    }

    // A click flips its box before the core answers; this puts back what the core decided.
    fn sync(&self, entries: &[PickEntry]) {
        for (cb, e) in self.checks.borrow().iter().zip(entries) {
            if cb.is_active() != e.on {
                cb.set_active(e.on);
            }
        }
    }
}

pub(super) struct PickBar {
    pub widget: gtk::Box,
    titles_list: gtk::StringList,
    titles_drop: gtk::DropDown,
    audio: PickMenu,
    subs: PickMenu,
    /// The view the bar was last filled from, which the handlers read a picked entry against.
    shown: Shown,
}

// "Titles" and its chooser, side by side.
fn labelled(label: &str, chooser: &impl IsA<gtk::Widget>) -> gtk::Box {
    let l = gtk::Label::new(Some(label));
    l.add_css_class("dim-label");
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.append(&l);
    b.append(chooser);
    b
}

impl PickBar {
    pub(super) fn new(shell: &Rc<Shell>) -> Self {
        let shown: Shown = Rc::default();

        let titles_list = gtk::StringList::new(&[]);
        let titles_drop = gtk::DropDown::new(Some(titles_list.clone()), gtk::Expression::NONE);
        let (me, sh) = (shell.clone(), shown.clone());
        titles_drop.connect_selected_notify(move |d| {
            if me.painting.get() {
                return;
            }
            let mode = sh
                .borrow()
                .as_ref()
                .and_then(|v| v.title_choice(d.selected() as usize));
            if let Some(mode) = mode {
                me.apply(|a| a.pick_titles(mode));
            }
        });

        let (me, sh) = (shell.clone(), shown.clone());
        let audio = PickMenu::new(Rc::new(move |tag| {
            if me.painting.get() {
                return;
            }
            let code = sh.borrow().as_ref().and_then(|v| v.audio_choice(tag));
            if let Some(code) = code {
                me.apply(|a| a.pick_audio(code.as_deref()));
            }
        }));
        let (me, sh) = (shell.clone(), shown.clone());
        let subs = PickMenu::new(Rc::new(move |tag| {
            if me.painting.get() {
                return;
            }
            let choice = sh.borrow().as_ref().and_then(|v| v.subs_choice(tag));
            if let Some(choice) = choice {
                me.apply(|a| a.pick_subtitles(choice));
            }
        }));

        let [t, a, s] = crate::ui::pick_labels();
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 24);
        widget.append(&labelled(&t, &titles_drop));
        widget.append(&labelled(&a, &audio.button));
        widget.append(&labelled(&s, &subs.button));
        widget.set_visible(false);
        PickBar {
            widget,
            titles_list,
            titles_drop,
            audio,
            subs,
            shown,
        }
    }

    /// Refill from the view when its choices changed; otherwise only put the ticks and the
    /// Titles choice back to the core's, in case a click left them showing something else.
    pub(super) fn render(&self, pick: Option<&PickView>) {
        self.widget.set_visible(pick.is_some());
        let Some(v) = pick else {
            self.shown.replace(None);
            return;
        };
        if self.shown.borrow().as_ref() != Some(v) {
            let labels: Vec<&str> = v.titles.iter().map(|(_, l)| l.as_str()).collect();
            let have: Vec<String> = (0..self.titles_list.n_items())
                .filter_map(|i| self.titles_list.string(i).map(String::from))
                .collect();
            if have != labels {
                self.titles_list
                    .splice(0, self.titles_list.n_items(), &labels);
            }
            self.audio.fill(&v.audio_summary, &v.audio_menu());
            self.subs.fill(&v.subs_summary, &v.subs_menu());
            self.shown.replace(Some(v.clone()));
        } else {
            self.audio.sync(&v.audio_menu());
            self.subs.sync(&v.subs_menu());
        }
        let at = v.title_index() as u32;
        if self.titles_drop.selected() != at {
            self.titles_drop.set_selected(at);
        }
    }
}
