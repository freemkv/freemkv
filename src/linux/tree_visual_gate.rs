//! Capture the real GTK tree through expansion, recycling and theme changes.
//! Kept independent of the renderer so the same harness runs on the baseline.
use gtk::glib;
use gtk::prelude::*;
use gtk4 as gtk;
use std::{cell::RefCell, rc::Rc, time::Duration};

use super::TitleTree;
use crate::gui_gate::{Gate, Region, has_ink};
use crate::ui::Row;

pub(in crate::linux) fn start(tree: &TitleTree, dir: String, done: impl FnOnce(bool) + 'static) {
    let Some(model) = tree.selection.model().and_downcast::<gtk::TreeListModel>() else {
        done(false);
        return;
    };
    let theme = libadwaita::StyleManager::default();
    let previous = theme.color_scheme();
    let scroll = tree.widget.clone();
    let g = Rc::new(RefCell::new(Gate::default()));
    let temporary = TitleTree::new(Rc::new(|_| {}), Rc::new(|_| {}));
    let provider = temporary.css.downgrade();
    drop(temporary);
    g.borrow_mut().check(
        "tree-css-provider-released",
        provider.upgrade().is_none(),
        "dropping a tree must release its display-wide CSS provider",
    );
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
        ScenarioTree {
            scroll,
            model,
            rows: tree.rows.clone(),
        },
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
struct ScenarioTree {
    scroll: gtk::ScrolledWindow,
    model: gtk::TreeListModel,
    rows: Rc<RefCell<Vec<Row>>>,
}

fn step(
    tree: ScenarioTree,
    dir: String,
    g: Rc<RefCell<Gate>>,
    mut steps: Vec<(&'static str, u8)>,
    done: Done,
) {
    let ScenarioTree {
        scroll,
        model,
        rows: source_rows,
    } = tree;
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
            let mut labels = Vec::new();
            check_cells(
                scroll.upcast_ref(),
                scroll.upcast_ref(),
                &source_rows.borrow(),
                name,
                &mut g.borrow_mut(),
                &mut labels,
            );
            if action == 1 {
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
                    "tree-content-follows-depth",
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
            step(
                ScenarioTree {
                    scroll,
                    model,
                    rows: source_rows,
                },
                dir,
                g,
                steps,
                done,
            );
        });
    });
}

// Measure only mapped cells intersecting the viewport: GTK can retain bound
// cells outside it while scrolling. Expected checkbox presence comes from the
// app model, independent of the widget visibility/opacity being tested.
fn check_cells(
    w: &gtk::Widget,
    root: &gtk::Widget,
    rows: &[Row],
    scenario: &str,
    g: &mut Gate,
    starts: &mut Vec<(u32, f32)>,
) {
    if let Some(exp) = w.downcast_ref::<gtk::TreeExpander>()
        && exp.is_mapped()
        && exp.compute_bounds(root).is_some_and(|b| {
            b.x() < root.width() as f32
                && b.y() < root.height() as f32
                && b.x() + b.width() > 0.0
                && b.y() + b.height() > 0.0
        })
        && let Some(row) = exp.list_row()
    {
        let idx = super::row_index(Some(row.clone().upcast()));
        let expected = idx.and_then(|i| rows.get(i));
        g.check(
            "tree-cell-model-bound",
            expected.is_some(),
            format!("{scenario}: row {idx:?}"),
        );
        if let Some(expected) = expected {
            let content = exp.child();
            let label = content.as_ref().and_then(|c| c.last_child());
            let start = if expected.check.is_some() {
                content.as_ref().and_then(|c| c.first_child())
            } else {
                label.clone()
            };
            let rtl = exp.direction() == gtk::TextDirection::Rtl;
            let edge = |b: gtk::graphene::Rect| {
                if rtl { b.x() + b.width() } else { b.x() }
            };
            if let Some(b) = start.and_then(|w| w.compute_bounds(root)) {
                starts.push((row.depth(), edge(b)));
            }
            if expected.check.is_none() {
                let gap = content
                    .as_ref()
                    .and_then(|c| c.compute_bounds(root))
                    .zip(label.and_then(|l| l.compute_bounds(root)))
                    .map(|(c, l)| (edge(l) - edge(c)).abs());
                g.check(
                    "tree-nonselectable-has-no-checkbox-slot",
                    gap.is_some_and(|gap| gap <= 1.0),
                    format!("{scenario}: row {idx:?}, label/content start gap {gap:?} px"),
                );
            }
        }
        // A nested row always has a parent branch; an expanded root has a
        // downward branch. Snapshot only the overlay's connector canvas: text,
        // expander arrows and checkboxes cannot contribute any ink here.
        if row.depth() > 0 || row.is_expanded() {
            let lines = exp
                .parent()
                .and_downcast::<gtk::Overlay>()
                .and_then(|overlay| overlay.last_child())
                .and_downcast::<gtk::DrawingArea>();
            let ink = lines
                .as_ref()
                .and_then(|lines| super::super::gate::capture(lines.upcast_ref()))
                .is_some_and(|(width, height, mut pixels)| {
                    // Transparent black and a black rule differ only in alpha.
                    // Present alpha as RGB to the shared colour-variation probe.
                    for pixel in pixels.as_chunks_mut::<4>().0 {
                        let alpha = pixel[3];
                        pixel[..3].fill(alpha);
                    }
                    has_ink(
                        &pixels,
                        width,
                        height,
                        false,
                        Region {
                            x: 0,
                            y: 0,
                            w: width,
                            h: height,
                        },
                    )
                });
            // The downward branch of an expanded row must pass through the
            // centre of the native arrow, not merely exist.
            if row.is_expanded()
                && let Some(lines) = lines.as_ref()
                && let Some(arrow) = super::expander_arrow(exp)
                && let Some(a) = arrow.compute_bounds(lines)
                && let Some((width, height, pixels)) =
                    super::super::gate::capture(lines.upcast_ref())
                && lines.width() > 0
            {
                let centre = (a.x() + a.width() / 2.0) / lines.width() as f32 * width as f32;
                let cx = centre.floor() as i64;
                let y = (height as usize).saturating_sub(2);
                let inked = |x: i64| {
                    (0..width as i64).contains(&x)
                        && pixels
                            .get((y * width as usize + x as usize) * 4 + 3)
                            .is_some_and(|a| *a > 0)
                };
                g.check(
                    "tree-branch-centred-on-arrow",
                    (cx - 1..=cx + 1).any(inked),
                    format!(
                        "{scenario}: row {idx:?}, depth {}, arrow centre x {centre:.1}",
                        row.depth()
                    ),
                );
            }
            g.check(
                "tree-connector-has-ink",
                ink,
                format!("{scenario}: row {idx:?}, depth {}", row.depth()),
            );
        }
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        check_cells(&c, root, rows, scenario, g, starts);
        child = c.next_sibling();
    }
}
