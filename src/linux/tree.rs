//! The title tree: a `GtkColumnView` over a `GtkTreeListModel`, with a
//! one hierarchical Item cell (branches, expander, tick and label), plus metadata columns.
//! Rows are the core's `View::title_rows`; a click only reports WHICH row — the
//! cascade and direction are the core's.

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gio, glib};

use crate::linux_glue as glue;
use crate::ui::{Check, Row};

use std::cell::{Cell, RefCell};
use std::rc::Rc;

type RowFn = Rc<dyn Fn(usize)>;

/// The core's column widths are macOS points; GTK's larger UI font and cell padding need more.
const WIDTH_SCALE: f64 = 1.2;
const INDENT: i32 = 20;

/// One tick box currently bound to a row, and the handler that reports it.
struct Bound {
    idx: usize,
    check: glib::WeakRef<gtk::CheckButton>,
    handler: glib::SignalHandlerId,
    expansion: Option<(gtk::TreeListRow, glib::SignalHandlerId)>,
}

pub(super) struct TitleTree {
    pub widget: gtk::ScrolledWindow,
    css: gtk::CssProvider,
    css_display: gtk::gdk::Display,
    selection: gtk::SingleSelection,
    rows: Rc<RefCell<Vec<Row>>>,
    kids: Rc<RefCell<Vec<Vec<usize>>>>,
    bound: Rc<RefCell<Vec<Bound>>>,
    branches: Rc<RefCell<Vec<Vec<bool>>>>,
    /// The row to bring into view after the next layout, of how many.
    scroll_to: Rc<Cell<Option<(usize, usize)>>>,
}

impl Drop for TitleTree {
    fn drop(&mut self) {
        gtk::style_context_remove_provider_for_display(&self.css_display, &self.css);
    }
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

// One of the core's columns: its cell text is `Row::cell(id)`, right-aligned when numeric.
fn text_column(rows: &Rc<RefCell<Vec<Row>>>, col: &crate::ui::Column) -> gtk::ColumnViewColumn {
    let (id, xalign) = (col.id, if col.numeric { 1.0 } else { 0.0 });
    let f = gtk::SignalListItemFactory::new();
    f.connect_setup(move |_, li| {
        let Some(li) = li.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let l = gtk::Label::new(None);
        l.set_xalign(xalign);
        l.set_valign(gtk::Align::Center);
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
        let text = rows.get(idx).map(|r| r.cell(id)).unwrap_or("");
        l.set_text(text);
        l.set_tooltip_text(Some(text));
    });
    let c = gtk::ColumnViewColumn::new(Some(&col.title), Some(f));
    // The flexible column takes the spare width; the others keep theirs.
    if col.flex {
        c.set_expand(true);
    } else {
        c.set_fixed_width((col.width * WIDTH_SCALE).round() as i32);
    }
    c
}

// Recover only widgets owned by this factory; bind/unbind always use the same layout.
fn tree_cell(
    li: &gtk::ListItem,
) -> Option<(
    gtk::Overlay,
    gtk::TreeExpander,
    gtk::CheckButton,
    gtk::Label,
    gtk::DrawingArea,
)> {
    let overlay = li.child()?.downcast::<gtk::Overlay>().ok()?;
    let exp = overlay.child()?.downcast::<gtk::TreeExpander>().ok()?;
    let content = exp.child()?;
    let check = content.first_child()?.downcast::<gtk::CheckButton>().ok()?;
    let label = content.last_child()?.downcast::<gtk::Label>().ok()?;
    let lines = overlay.last_child()?.downcast::<gtk::DrawingArea>().ok()?;
    Some((overlay, exp, check, label, lines))
}

impl TitleTree {
    pub(super) fn new(on_select: RowFn, on_toggle: RowFn) -> Self {
        let rows: Rc<RefCell<Vec<Row>>> = Rc::default();
        let kids: Rc<RefCell<Vec<Vec<usize>>>> = Rc::default();
        let bound: Rc<RefCell<Vec<Bound>>> = Rc::default();

        let branches: Rc<RefCell<Vec<Vec<bool>>>> = Rc::default();
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
        view.add_css_class("title-tree");
        // The branch canvas must reach the next row, including selected rows.
        let css = gtk::CssProvider::new();
        css.load_from_data(".title-tree row cell { padding-top: 0; padding-bottom: 0; }");
        let css_display = view.display();
        gtk::style_context_add_provider_for_display(
            &css_display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        // Keep the label inside the native expander: its keyboard actions and
        // accessibility remain GTK's, while the whole Item cell follows depth.
        let f = gtk::SignalListItemFactory::new();
        f.connect_setup(|_, li| {
            let Some(li) = li.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let exp = gtk::TreeExpander::new();
            exp.set_indent_for_depth(false);
            exp.set_indent_for_icon(true);
            exp.set_hexpand(true);
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let check = gtk::CheckButton::new();
            content.append(&check);
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            content.append(&label);
            exp.set_child(Some(&content));
            let overlay = gtk::Overlay::new();
            overlay.set_height_request(36);
            overlay.set_child(Some(&exp));
            let lines = gtk::DrawingArea::new();
            lines.set_can_target(false);
            overlay.add_overlay(&lines);
            li.set_child(Some(&overlay));
        });
        let (b_rows, b_bound, b_branches) = (rows.clone(), bound.clone(), branches.clone());
        f.connect_bind(move |_, li| {
            let Some(li) = li.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some((overlay, exp, cb, label, lines)) = tree_cell(li) else {
                return;
            };
            let tlr = li.item().and_downcast::<gtk::TreeListRow>();
            exp.set_list_row(tlr.as_ref());
            let Some(idx) = row_index(li.item()) else {
                return;
            };
            let rows = b_rows.borrow();
            let Some(row) = rows.get(idx) else {
                return;
            };
            let depth = tlr.as_ref().map_or(0, |r| r.depth()) as i32;
            exp.set_margin_start(depth * INDENT);
            label.set_text(row.cell("item"));
            label.set_tooltip_text(Some(row.cell("item")));
            // Non-selectable nodes have no checkbox slot: their label follows
            // the expander directly, including chapter groups and chapter leaves.
            cb.set_visible(row.check.is_some());
            cb.set_sensitive(row.check.is_some() && row.check_enabled);
            cb.set_focusable(row.check.is_some() && row.check_enabled);
            cb.update_property(&[gtk::accessible::Property::Label(row.cell("item"))]);
            if let Some(s) = row.check {
                paint(&cb, s);
            }
            let path = b_branches.borrow().get(idx).cloned().unwrap_or_default();
            let weak_exp = exp.downgrade();
            let weak_overlay = overlay.downgrade();
            lines.set_draw_func(move |area, cr, width, height| {
                let (Some(exp), Some(overlay)) = (weak_exp.upgrade(), weak_overlay.upgrade())
                else {
                    return;
                };
                let Some(content) = exp.child() else {
                    return;
                };
                let (Some(e), Some(c)) = (
                    exp.compute_bounds(&overlay),
                    content.compute_bounds(&overlay),
                ) else {
                    return;
                };
                let rtl = exp.direction() == gtk::TextDirection::Rtl;
                // Measure the native arrow slot rather than assuming its size:
                // the theme, font and display scale may all change it.
                let arrow = if rtl {
                    (e.x() + e.width() - c.x() - c.width()) / 2.0
                } else {
                    (c.x() - e.x()) / 2.0
                } as f64;
                let x = |level: usize| {
                    let p = level as f64 * INDENT as f64 + arrow;
                    if rtl { width as f64 - p } else { p }
                };
                let h = height as f64;
                let middle = h / 2.0;
                let color = area.color();
                cr.set_source_rgba(
                    color.red() as f64,
                    color.green() as f64,
                    color.blue() as f64,
                    0.38,
                );
                cr.set_line_width(1.0);
                let segment = |x1: f64, y1: f64, x2: f64, y2: f64| {
                    cr.move_to(x1.floor() + 0.5, y1);
                    cr.line_to(x2.floor() + 0.5, y2);
                };
                let depth = depth as usize;
                for level in 1..depth {
                    if path.get(level).copied().unwrap_or(false) {
                        segment(x(level - 1), 0.0, x(level - 1), h);
                    }
                }
                if depth > 0 {
                    let parent = x(depth - 1);
                    let bottom = if path.last().copied().unwrap_or(false) {
                        h
                    } else {
                        middle
                    };
                    segment(parent, 0.0, parent, bottom);
                    let expandable = exp.list_row().is_some_and(|r| r.is_expandable());
                    let end = if expandable {
                        x(depth) + if rtl { 8.0 } else { -8.0 }
                    } else {
                        // Leaves have no arrow. Connect to their visible checkbox
                        // or label instead of leaving a floating stub.
                        let target = content
                            .first_child()
                            .filter(|w| w.is_visible())
                            .or_else(|| content.last_child());
                        target
                            .and_then(|w| w.compute_bounds(&overlay))
                            .map_or(x(depth), |b| {
                                if rtl {
                                    f64::from(b.x() + b.width()) + 4.0
                                } else {
                                    f64::from(b.x()) - 4.0
                                }
                            })
                    };
                    segment(parent, middle, end, middle);
                }
                if exp.list_row().is_some_and(|r| r.is_expanded()) {
                    segment(x(depth), middle + 8.0, x(depth), h);
                }
                let _ = cr.stroke();
            });
            let tog = on_toggle.clone();
            let handler = cb.connect_toggled(move |_| tog(idx));
            let expansion = tlr.map(|row| {
                let weak = lines.downgrade();
                let handler = row.connect_expanded_notify(move |_| {
                    if let Some(lines) = weak.upgrade() {
                        lines.queue_draw();
                    }
                });
                (row, handler)
            });
            b_bound.borrow_mut().push(Bound {
                idx,
                check: cb.downgrade(),
                handler,
                expansion,
            });
        });
        let u_bound = bound.clone();
        f.connect_unbind(move |_, li| {
            let Some(li) = li.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some((_, exp, cb, _, lines)) = tree_cell(li) else {
                return;
            };
            let mut b = u_bound.borrow_mut();
            if let Some(pos) = b
                .iter()
                .position(|x| x.check.upgrade().as_ref() == Some(&cb))
            {
                let gone = b.remove(pos);
                cb.disconnect(gone.handler);
                if let Some((row, handler)) = gone.expansion {
                    row.disconnect(handler);
                }
            }
            exp.set_list_row(None::<&gtk::TreeListRow>);
            lines.unset_draw_func();
        });
        for col in crate::ui::tree_columns() {
            if col.id == "item" {
                let item = gtk::ColumnViewColumn::new(Some(&col.title), Some(f.clone()));
                // Reuse the former checkbox column's space, now as part of Item.
                item.set_fixed_width((col.width * WIDTH_SCALE).round() as i32 + 96);
                item.set_resizable(true);
                view.append_column(&item);
            } else {
                view.append_column(&text_column(&rows, &col));
            }
        }

        let widget = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .build();
        super::main_view::card(&widget);
        TitleTree {
            widget,
            css,
            css_display,
            selection,
            rows,
            kids,
            bound,
            branches,
            scroll_to: Rc::default(),
        }
    }

    /// The row list changed (not just ticks): rebuild the model, every row
    /// expanded, and bring the first ticked row into view as the other shells do.
    pub(super) fn rebuild(&self, rows: &[Row]) {
        *self.rows.borrow_mut() = rows.to_vec();
        let (roots, kids) = glue::tree_shape(rows);
        // Each path says whether a node at that depth has a following sibling.
        // Hidden descendants never affect the last-child elbow of their parent.
        fn paths(ids: &[usize], kids: &[Vec<usize>], prefix: &[bool], out: &mut [Vec<bool>]) {
            for (position, &idx) in ids.iter().enumerate() {
                let mut path = prefix.to_vec();
                path.push(position + 1 < ids.len());
                out[idx] = path.clone();
                paths(&kids[idx], kids, &path, out);
            }
        }
        let mut branches = vec![Vec::new(); rows.len()];
        paths(&roots, &kids, &[], &mut branches);
        *self.branches.borrow_mut() = branches;
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
        let model = gtk::TreeListModel::new(root, false, false, move |item| {
            let idx = *item
                .downcast_ref::<glib::BoxedAnyObject>()?
                .borrow::<usize>();
            let kids = kids.borrow();
            let ch = kids.get(idx).filter(|c| !c.is_empty())?;
            Some(store(ch).upcast())
        });
        self.selection.set_model(Some(&model));
        // Open every row but the groups the core starts closed (`ui::starts_collapsed`).
        // Expanding inserts a row's children right after it, so one forward pass reaches all.
        let shown = |pos: u32| {
            let tlr = model.row(pos)?;
            let idx = *tlr
                .item()?
                .downcast_ref::<glib::BoxedAnyObject>()?
                .borrow::<usize>();
            Some((tlr, idx))
        };
        let mut pos = 0;
        while pos < model.n_items() {
            if let Some((tlr, idx)) = shown(pos) {
                tlr.set_expanded(
                    rows.get(idx)
                        .is_some_and(|r| !crate::ui::starts_collapsed(r)),
                );
            }
            pos += 1;
        }

        // Start at the top, then scroll only as far as the core's chosen row needs. GTK 4.10
        // has no `scroll_to`, and rows are uniform, so the adjustment is exact enough.
        let first = crate::ui::first_visible_row(rows).unwrap_or(0);
        let n = model.n_items();
        let at = (0..n)
            .find(|&p| shown(p).is_some_and(|(_, i)| i == first))
            .unwrap_or(0) as usize;
        self.widget.vadjustment().set_value(0.0);
        self.scroll_to.set((at > 0).then_some((at, n as usize)));
        let pending = self.scroll_to.clone();
        // `upper` is only right once the new model is laid out: wait for the
        // next frame's after-paint, then read the latest pending target.
        self.widget.add_tick_callback(move |w, clock| {
            let (w, pending) = (w.clone(), pending.clone());
            let slot: Rc<Cell<Option<glib::SignalHandlerId>>> = Rc::default();
            let own = slot.clone();
            let id = clock.connect_after_paint(move |c| {
                if let Some((at, n)) = pending.take() {
                    let adj = w.vadjustment();
                    let bottom = adj.upper() * (at + 1) as f64 / n as f64;
                    adj.set_value((bottom - adj.page_size()).max(0.0));
                }
                if let Some(id) = own.take() {
                    c.disconnect(id);
                }
            });
            slot.set(Some(id));
            glib::ControlFlow::Break
        });
    }

    /// Ticks only: repaint the bound boxes in place, keeping expansion,
    /// selection and scroll position.
    pub(super) fn sync_checks(&self, rows: &[Row]) {
        for (mine, r) in self.rows.borrow_mut().iter_mut().zip(rows) {
            mine.check = r.check;
        }
        for b in self.bound.borrow().iter() {
            let (Some(cb), Some(state)) =
                (b.check.upgrade(), rows.get(b.idx).and_then(|r| r.check))
            else {
                continue;
            };
            cb.block_signal(&b.handler);
            paint(&cb, state);
            cb.unblock_signal(&b.handler);
        }
    }
}

#[cfg(debug_assertions)]
#[path = "tree_visual_gate.rs"]
mod visual;
#[cfg(debug_assertions)]
pub(super) use visual::start as visual_gate;
