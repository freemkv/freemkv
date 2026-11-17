//! Capture the real GTK tree through expansion, recycling and theme changes.
//! Kept independent of the renderer so the same harness runs on the baseline.
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::{cell::RefCell, rc::Rc, time::Duration};

use super::TitleTree;
use crate::gui_gate::Gate;

pub(in crate::linux) fn start(tree: &TitleTree, dir: String, done: impl FnOnce(bool) + 'static) {
    let Some(model) = tree.selection.model().and_downcast::<gtk::TreeListModel>() else {
        done(false);
        return;
    };
    let theme = libadwaita::StyleManager::default();
    let previous = theme.color_scheme();
    let scroll = tree.widget.clone();
    let g = Rc::new(RefCell::new(Gate::default()));
    let steps = vec![
        ("light-default", 0),
        ("light-chapters-expanded", 1),
        ("light-title-collapsed", 2),
        ("light-scroll-bottom", 3),
        ("light-scroll-return", 4),
        ("dark-expanded", 5),
    ];
    theme.set_color_scheme(libadwaita::ColorScheme::ForceLight);
    step(
        scroll,
        model,
        dir,
        g,
        steps,
        Box::new(move |ok| {
            theme.set_color_scheme(previous);
            done(ok);
        }),
    );
}

type Done = Box<dyn FnOnce(bool)>;
fn step(
    scroll: gtk::ScrolledWindow,
    model: gtk::TreeListModel,
    dir: String,
    g: Rc<RefCell<Gate>>,
    mut steps: Vec<(&'static str, u8)>,
    done: Done,
) {
    if steps.is_empty() {
        let report = g.borrow().report();
        let written = std::fs::write(format!("{dir}/tree-gate.txt"), report).is_ok();
        done(written && g.borrow().passed());
        return;
    }
    let (name, action) = steps.remove(0);
    if action == 1 || action == 3 {
        let mut pos = 0;
        while pos < model.n_items() {
            if let Some(row) = model.row(pos) {
                row.set_expanded(true);
            }
            pos += 1;
        }
    }
    if action == 2 {
        for pos in 0..model.n_items() {
            if let Some(row) = model.row(pos).filter(|r| r.depth() == 1) {
                row.set_expanded(false);
                break;
            }
        }
    }
    if action == 5 {
        libadwaita::StyleManager::default().set_color_scheme(libadwaita::ColorScheme::ForceDark);
    }
    // Scroll after expansion has been laid out, then allow recycling to settle.
    glib::timeout_add_local_once(Duration::from_millis(500), move || {
        if action == 3 {
            let adj = scroll.vadjustment();
            adj.set_value((adj.upper() - adj.page_size()).max(0.0));
        } else {
            scroll.vadjustment().set_value(0.0);
        }
        glib::timeout_add_local_once(Duration::from_millis(500), move || {
            let rows: Vec<_> = (0..model.n_items()).filter_map(|p| model.row(p)).collect();
            g.borrow_mut().check(
                "tree-has-multiple-titles",
                rows.iter().filter(|r| r.depth() == 1).count() >= 2,
                name,
            );
            if action == 1 {
                let mut labels = Vec::new();
                collect_labels(scroll.upcast_ref(), scroll.upcast_ref(), &mut labels);
                let x = |depth| labels.iter().find(|(d, _)| *d == depth).map(|(_, x)| *x);
                let follows_depth = match (x(1), x(2), x(3)) {
                    (Some(a), Some(b), Some(c))
                        if scroll.direction() == gtk::TextDirection::Rtl =>
                    {
                        a > b && b > c
                    }
                    (Some(a), Some(b), Some(c)) => a < b && b < c,
                    _ => false,
                };
                g.borrow_mut().check(
                    "tree-label-follows-depth",
                    follows_depth,
                    format!("{labels:?}"),
                );
                g.borrow_mut().check(
                    "tree-has-nested-chapters",
                    rows.iter().any(|r| r.depth() >= 3),
                    name,
                );
            }
            if action == 2 {
                g.borrow_mut().check(
                    "tree-title-collapsed",
                    rows.iter().any(|r| r.depth() == 1 && !r.is_expanded()),
                    name,
                );
            }
            if action == 3 {
                g.borrow_mut().check(
                    "tree-scroll-exercises-recycling",
                    scroll.vadjustment().value() > 0.0,
                    name,
                );
            }
            super::super::gate::save(
                scroll.upcast_ref(),
                &format!("{dir}/tree-{name}.png"),
                &mut g.borrow_mut(),
            );
            step(scroll, model, dir, g, steps, done);
        });
    });
}

fn collect_labels(w: &gtk::Widget, root: &gtk::Widget, out: &mut Vec<(u32, f32)>) {
    if let Some(exp) = w.downcast_ref::<gtk::TreeExpander>()
        && let Some(row) = exp.list_row()
    {
        fn label(w: &gtk::Widget) -> Option<gtk::Label> {
            if let Some(l) = w.downcast_ref::<gtk::Label>() {
                return Some(l.clone());
            }
            let mut child = w.first_child();
            while let Some(c) = child {
                if let Some(l) = label(&c) {
                    return Some(l);
                }
                child = c.next_sibling();
            }
            None
        }
        if let Some(l) = exp.child().and_then(|c| label(&c))
            && let Some(b) = l.compute_bounds(root)
        {
            // Compare the start edge in both LTR and RTL layouts.
            out.push((
                row.depth(),
                if w.direction() == gtk::TextDirection::Rtl {
                    b.x() + b.width()
                } else {
                    b.x()
                },
            ));
        }
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        collect_labels(&c, root, out);
        child = c.next_sibling();
    }
}
