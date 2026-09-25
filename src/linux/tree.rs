//! The title tree: a `GtkColumnView` over a `GtkTreeListModel`, with a
//! tri-state tick box in the expander column and the Type / Description
//! columns the macOS outline shows. Rows are the core's `View::title_rows`;
//! a click only reports WHICH row — the cascade and direction are the core's.

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gio, glib};

use crate::linux_glue as glue;
use crate::ui::{Check, Row};

use std::cell::RefCell;
use std::rc::Rc;

type RowFn = Rc<dyn Fn(usize)>;

/// One tick box currently bound to a row, and the handler that reports it.
struct Bound {
    idx: usize,
    check: glib::WeakRef<gtk::CheckButton>,
    handler: glib::SignalHandlerId,
}

pub(super) struct TitleTree {
    pub widget: gtk::ScrolledWindow,
    selection: gtk::SingleSelection,
    rows: Rc<RefCell<Vec<Row>>>,
    kids: Rc<RefCell<Vec<Vec<usize>>>>,
    bound: Rc<RefCell<Vec<Bound>>>,
}

fn row_index(item: Option<glib::Object>) -> Option<usize> {
    let tlr = item.and_downcast::<gtk::TreeListRow>()?;
    let boxed = tlr.item().and_downcast::<glib::BoxedAnyObject>()?;
    let idx = *boxed.borrow::<usize>();
    Some(idx)
}

fn paint(cb: &gtk::CheckButton, state: Check) {
    cb.set_inconsistent(state == Check::Mixed);
    cb.set_active(state != Check::Off);
}

fn text_column(
    title: &str,
    rows: &Rc<RefCell<Vec<Row>>>,
    pick: fn(&Row) -> &str,
) -> gtk::ColumnViewColumn {
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(|_, li| {
        let Some(li) = li.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let l = gtk::Label::new(None);
        l.set_xalign(0.0);
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        li.set_child(Some(&l));
    });
    let rows = rows.clone();
    f.connect_bind(move |_, li| {
        let Some(li) = li.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(idx), Some(l)) = (
            row_index(li.item()),
            li.child().and_downcast::<gtk::Label>(),
        ) else {
            return;
        };
        let rows = rows.borrow();
        let text = rows.get(idx).map(pick).unwrap_or("");
        l.set_text(text);
        l.set_tooltip_text(Some(text));
    });
    gtk::ColumnViewColumn::new(Some(title), Some(f))
}

impl TitleTree {
    pub(super) fn new(on_select: RowFn, on_toggle: RowFn) -> Self {
        let rows: Rc<RefCell<Vec<Row>>> = Rc::default();
        let kids: Rc<RefCell<Vec<Vec<usize>>>> = Rc::default();
        let bound: Rc<RefCell<Vec<Bound>>> = Rc::default();

        let selection = gtk::SingleSelection::new(None::<gio::ListModel>);
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        selection.connect_selected_item_notify(move |s| {
            if let Some(idx) = row_index(s.selected_item()) {
                on_select(idx);
            }
        });

        let view = gtk::ColumnView::new(Some(selection.clone()));
        view.set_show_row_separators(false);

        // Expander + tick box. The toggle handler is connected per bind and
        // removed per unbind, so a recycled widget never reports a stale row.
        let f = gtk::SignalListItemFactory::new();
        f.connect_setup(|_, li| {
            let Some(li) = li.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let exp = gtk::TreeExpander::new();
            exp.set_child(Some(&gtk::CheckButton::new()));
            li.set_child(Some(&exp));
        });
        let (b_rows, b_bound) = (rows.clone(), bound.clone());
        f.connect_bind(move |_, li| {
            let Some(li) = li.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(exp) = li.child().and_downcast::<gtk::TreeExpander>() else {
                return;
            };
            let tlr = li.item().and_downcast::<gtk::TreeListRow>();
            exp.set_list_row(tlr.as_ref());
            let (Some(idx), Some(cb)) = (
                row_index(li.item()),
                exp.child().and_downcast::<gtk::CheckButton>(),
            ) else {
                return;
            };
            let state = b_rows.borrow().get(idx).and_then(|r| r.check);
            cb.set_visible(state.is_some());
            if let Some(s) = state {
                paint(&cb, s);
            }
            let tog = on_toggle.clone();
            let handler = cb.connect_toggled(move |_| tog(idx));
            b_bound.borrow_mut().push(Bound {
                idx,
                check: cb.downgrade(),
                handler,
            });
        });
        let u_bound = bound.clone();
        f.connect_unbind(move |_, li| {
            let Some(cb) = li
                .downcast_ref::<gtk::ListItem>()
                .and_then(|li| li.child().and_downcast::<gtk::TreeExpander>())
                .and_then(|e| e.child().and_downcast::<gtk::CheckButton>())
            else {
                return;
            };
            let mut b = u_bound.borrow_mut();
            if let Some(pos) = b
                .iter()
                .position(|x| x.check.upgrade().as_ref() == Some(&cb))
            {
                let gone = b.remove(pos);
                cb.disconnect(gone.handler);
            }
        });
        let check_col = gtk::ColumnViewColumn::new(None, Some(f));
        check_col.set_fixed_width(96);
        view.append_column(&check_col);
        let type_col = text_column(&crate::strings::get("gui.col.type"), &rows, |r| &r.type_s);
        type_col.set_fixed_width(110);
        view.append_column(&type_col);
        let desc_col = text_column(&crate::strings::get("gui.col.desc"), &rows, |r| &r.desc);
        desc_col.set_expand(true);
        view.append_column(&desc_col);

        let widget = gtk::ScrolledWindow::builder()
            .child(&view)
            .has_frame(true)
            .vexpand(true)
            .hexpand(true)
            .build();
        TitleTree {
            widget,
            selection,
            rows,
            kids,
            bound,
        }
    }

    /// The row list changed (not just ticks): rebuild the model, every row
    /// expanded, and bring the first ticked row into view as the other shells do.
    pub(super) fn rebuild(&self, rows: &[Row]) {
        *self.rows.borrow_mut() = rows.to_vec();
        let (roots, kids) = glue::tree_shape(rows);
        *self.kids.borrow_mut() = kids;
        let store = |ids: &[usize]| {
            let s = gio::ListStore::new::<glib::BoxedAnyObject>();
            for &i in ids {
                s.append(&glib::BoxedAnyObject::new(i));
            }
            s
        };
        let root = store(&roots);
        let kids = self.kids.clone();
        let model = gtk::TreeListModel::new(root, false, true, move |item| {
            let idx = *item
                .downcast_ref::<glib::BoxedAnyObject>()?
                .borrow::<usize>();
            let kids = kids.borrow();
            let ch = kids.get(idx).filter(|c| !c.is_empty())?;
            Some(store(ch).upcast())
        });
        self.selection.set_model(Some(&model));

        // All rows are expanded, so display position == flat index. GTK 4.10
        // has no `scroll_to`; rows are uniform, so the adjustment is exact enough.
        let at = crate::ui::first_visible_row(rows).unwrap_or(0);
        let adj = self.widget.vadjustment();
        adj.set_value(0.0);
        let n = rows.len().max(1) as f64;
        glib::idle_add_local_once(move || {
            if at > 0 {
                adj.set_value(adj.upper() * at as f64 / n);
            }
        });
    }

    /// Ticks only: repaint the bound boxes in place, keeping expansion,
    /// selection and scroll position.
    pub(super) fn sync_checks(&self, rows: &[Row]) {
        *self.rows.borrow_mut() = rows.to_vec();
        for b in self.bound.borrow().iter() {
            let (Some(cb), Some(state)) =
                (b.check.upgrade(), rows.get(b.idx).and_then(|r| r.check))
            else {
                continue;
            };
            paint(&cb, state);
        }
    }
}
