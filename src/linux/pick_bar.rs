//! The disc page's controls over the title tree: the details line (Name, Year, and for TV
//! Season, Disc and First episode, from `DetailsView`), and one row of four equal choosers,
//! Kind, Titles, Audio and Subtitles (Kind from `DetailsView`, the rest from `PickView`).
//! Each chooser expands below its own column and pushes the tree down while open. A choice
//! only reports WHICH entry was picked, or what was typed; what it means, and the re-ticked
//! tree, are the core's.

use gtk4 as gtk;
use gtk4::prelude::*;

use super::Shell;
use super::chooser::{self, Choice};
use crate::ui::{DetailField, DetailsView, PickEntry, PickView};
use libadwaita as adw;
use libadwaita::prelude::*;

use std::cell::RefCell;
use std::rc::Rc;

type Shown = Rc<RefCell<Option<PickView>>>;

/// An inline chooser listing the core's entries as tick boxes. It stays open, so
/// several languages can be ticked in one go.
struct PickMenu {
    row: adw::ExpanderRow,
    rows: RefCell<Vec<adw::ActionRow>>,
    checks: RefCell<Vec<gtk::CheckButton>>,
    /// The entries' labels, tags and separators the list was built from.
    shape: RefCell<Vec<(String, isize, bool)>>,
    on_pick: Rc<dyn Fn(isize)>,
}

impl PickMenu {
    fn new(on_pick: Rc<dyn Fn(isize)>) -> Self {
        PickMenu {
            row: chooser::expander(""),
            rows: RefCell::default(),
            checks: RefCell::default(),
            shape: RefCell::default(),
            on_pick,
        }
    }

    /// Show the closed label and the entries, rebuilding the list only when the entries
    /// themselves changed; otherwise just the ticks.
    fn fill(&self, summary: &str, entries: &[PickEntry]) {
        self.row.set_subtitle(summary);
        let shape: Vec<_> = entries
            .iter()
            .map(|e| (e.label.clone(), e.tag, e.separator_before))
            .collect();
        if *self.shape.borrow() != shape {
            for row in self.rows.borrow_mut().drain(..) {
                self.row.remove(&row);
            }
            let mut checks = Vec::new();
            for e in entries {
                let cb = gtk::CheckButton::new();
                let row = chooser::option(&e.label, &cb);
                if e.separator_before {
                    row.add_css_class("separator");
                }
                let (on_pick, tag) = (self.on_pick.clone(), e.tag);
                cb.connect_toggled(move |_| on_pick(tag));
                self.row.add_row(&row);
                self.rows.borrow_mut().push(row);
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

/// One column of the chooser row: the chooser on its own boxed list, top-aligned so a
/// neighbour that expands does not stretch it, and one line tall while closed.
fn column(row: &adw::ExpanderRow) -> gtk::ListBox {
    row.set_title_lines(1);
    row.set_subtitle_lines(1);
    let list = chooser::surface(row);
    list.set_valign(gtk::Align::Start);
    list.set_hexpand(true);
    list
}

/// The row of four equal choosers (Kind, Titles, Audio, Subtitles) and the review banner.
pub(super) struct PickBar {
    pub widget: gtk::Box,
    /// The four chooser columns, side by side with equal widths.
    pub(super) row: gtk::Box,
    kind_column: gtk::ListBox,
    kinds_list: gtk::StringList,
    kinds_drop: Rc<Choice>,
    titles_list: gtk::StringList,
    titles_drop: Rc<Choice>,
    audio: PickMenu,
    subs: PickMenu,
    /// Why the ticks need checking.
    banner: gtk::Label,
    /// The view the bar was last filled from, which the handlers read a picked entry against.
    shown: Shown,
}

impl PickBar {
    pub(super) fn new(shell: &Rc<Shell>) -> Self {
        let shown: Shown = Rc::default();

        let [_, _, kind, _, _, _] = crate::ui::details_labels();
        let kinds_list = gtk::StringList::new(&[]);
        let kinds_drop = Choice::new(&kind, &kinds_list);
        let me = shell.clone();
        kinds_drop.connect_selected_notify(move |d| {
            if me.painting.get() {
                return;
            }
            let at = d.selected() as usize;
            me.apply(|a| a.set_detail_kind(at));
        });

        let titles_list = gtk::StringList::new(&[]);
        let titles_drop = Choice::new(&crate::ui::pick_labels()[0], &titles_list);
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

        let [_, a, s] = crate::ui::pick_labels();
        audio.row.set_title(&a);
        subs.row.set_title(&s);
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_homogeneous(true);
        let kind_column = column(&kinds_drop.row);
        row.append(&kind_column);
        row.append(&column(&titles_drop.row));
        row.append(&column(&audio.row));
        row.append(&column(&subs.row));
        let banner = gtk::Label::new(None);
        banner.add_css_class("warning");
        banner.set_ellipsize(gtk::pango::EllipsizeMode::End);
        banner.set_hexpand(true);
        banner.set_xalign(0.0);
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        widget.append(&row);
        widget.append(&banner);
        widget.set_visible(false);
        PickBar {
            widget,
            row,
            kind_column,
            kinds_list,
            kinds_drop,
            titles_list,
            titles_drop,
            audio,
            subs,
            banner,
            shown,
        }
    }

    /// Refill from the views when their choices changed; otherwise only put the ticks and the
    /// Kind and Titles choices back to the core's, in case a click left them showing
    /// something else. Kind shows only with the details (a disc, not a container).
    pub(super) fn render(&self, pick: Option<&PickView>, details: Option<&DetailsView>) {
        self.widget.set_visible(pick.is_some());
        self.render_kind(details);
        let Some(v) = pick else {
            self.shown.replace(None);
            return;
        };
        let banner = v.banner.as_deref().unwrap_or("");
        if self.banner.text() != banner {
            self.banner.set_text(banner);
            self.banner.set_tooltip_text(Some(banner));
        }
        self.banner.set_visible(!banner.is_empty());
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

    fn render_kind(&self, details: Option<&DetailsView>) {
        self.kind_column.set_visible(details.is_some());
        let Some(v) = details else { return };
        let have: Vec<String> = (0..self.kinds_list.n_items())
            .filter_map(|i| self.kinds_list.string(i).map(String::from))
            .collect();
        if have != v.kinds {
            let labels: Vec<&str> = v.kinds.iter().map(String::as_str).collect();
            self.kinds_list
                .splice(0, self.kinds_list.n_items(), &labels);
        }
        let at = v.kind_index as u32;
        if self.kinds_drop.selected() != at {
            self.kinds_drop.set_selected(at);
        }
    }
}

// A field's label and its entry, side by side.
fn labelled(label: &str, entry: &gtk::Entry) -> gtk::Box {
    let l = gtk::Label::new(Some(label));
    l.add_css_class("dim-label");
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.append(&l);
    b.append(entry);
    b
}

/// The disc details line: Name and Year, then Season, Disc and First episode while the
/// effective kind is TV.
pub(super) struct DetailsBar {
    pub widget: gtk::Box,
    entries: Vec<(DetailField, gtk::Entry)>,
    /// Season, Disc and First episode, each with its label: shown only for TV.
    tv_fields: Vec<gtk::Box>,
}

impl DetailsBar {
    pub(super) fn new(shell: &Rc<Shell>) -> Self {
        let [name, year, _, season, disc, first] = crate::ui::details_labels();
        let entry = |field: DetailField, chars: i32| {
            let e = gtk::Entry::new();
            e.set_width_chars(chars);
            let me = shell.clone();
            // Every keystroke reaches the core, so Start never misses one.
            e.connect_changed(move |e| {
                if me.painting.get() {
                    return;
                }
                let t = e.text().to_string();
                me.apply(|a| a.set_detail(field, &t));
            });
            (field, e)
        };
        let entries = vec![
            entry(DetailField::Name, 24),
            entry(DetailField::Year, 5),
            entry(DetailField::Season, 3),
            entry(DetailField::Disc, 3),
            entry(DetailField::FirstEpisode, 4),
        ];
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        let name_field = labelled(&name, &entries[0].1);
        name_field.set_hexpand(true);
        entries[0].1.set_hexpand(true);
        widget.append(&name_field);
        widget.append(&labelled(&year, &entries[1].1));
        let tv_fields = vec![
            labelled(&season, &entries[2].1),
            labelled(&disc, &entries[3].1),
            labelled(&first, &entries[4].1),
        ];
        for f in &tv_fields {
            widget.append(f);
        }
        widget.set_visible(false);
        DetailsBar {
            widget,
            entries,
            tv_fields,
        }
    }

    /// Show the core's details. A box is written only when the core holds other text, so
    /// typing is never overwritten.
    pub(super) fn render(&self, details: Option<&DetailsView>) {
        self.widget.set_visible(details.is_some());
        let Some(v) = details else { return };
        for (field, e) in &self.entries {
            let want = v.get(*field);
            if e.text() != want {
                e.set_text(want);
            }
        }
        for f in &self.tv_fields {
            f.set_visible(v.tv);
        }
    }
}
