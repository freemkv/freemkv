//! qa's GUI gate on the GTK shell (`crate::gui_gate`), run by a debug build with
//! `FMKV_GATE=<dir>` over the source `FMKV_OPEN` opened: the title tree's Length and Size
//! columns and the Settings path and dropdowns, measured in the widgets themselves and checked
//! for ink in a render of the real window. Writes `<dir>/gate.txt` and the captures, and exits
//! with the verdict.

use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use libadwaita as adw;
use libadwaita::prelude::*;

use super::Shell;
use crate::gui_gate::{Gate, LONG_DEST, Region, has_ink};
use crate::ui::{Cmd, Page};

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

/// How long a step waits for GTK to lay out and draw before it measures.
const SETTLE: Duration = Duration::from_millis(800);

/// Wait (up to a minute) for the fixture's scan, then run the gate.
pub(super) fn start(shell: &Rc<Shell>, dir: String) {
    let me = shell.clone();
    let mut waited = Duration::ZERO;
    glib::timeout_add_local(Duration::from_millis(200), move || {
        waited += Duration::from_millis(200);
        if me.app.borrow().view().page != Page::Titles && waited < Duration::from_secs(60) {
            return glib::ControlFlow::Continue;
        }
        // Give both fixture titles room before checking their rendered cells.
        let log_visible = !me.app.borrow().view().log_hidden;
        if log_visible {
            me.act(Cmd::ToggleLog);
        }
        if let Some(main) = me.main.borrow().as_ref() {
            scroll_to(main.tree.widget.upcast_ref());
        }
        let (me, dir) = (me.clone(), dir.clone());
        glib::timeout_add_local_once(SETTLE, move || run(&me, dir));
        glib::ControlFlow::Break
    });
}

fn run(shell: &Rc<Shell>, dir: String) {
    let _ = std::fs::create_dir_all(&dir);
    let g = Rc::new(RefCell::new(Gate::default()));
    environment(shell, &mut g.borrow_mut());
    titles(shell, &dir, &mut g.borrow_mut());

    let Some(main) = shell.main.borrow().clone() else {
        g.borrow_mut().check("tree-present", false, "no main view");
        return finish(&dir, &g.borrow());
    };
    let me = shell.clone();
    super::tree::visual_gate(&main.tree, dir.clone(), move |ok| {
        g.borrow_mut().check(
            "tree-visual-scenarios",
            ok,
            "see tree-gate.txt and tree-*.png",
        );
        run_after_tree(&me, dir, g);
    });
}

// Assert what GTK and the app actually applied, not just the driver's folder name.
fn environment(shell: &Shell, g: &mut Gate) {
    if let Ok(language) = super::dev_env("FMKV_GATE_LANGUAGE") {
        // Arabic is deliberately a fallback/RTL case: no Arabic catalog ships yet.
        let catalog = crate::strings::bundled_locale_json(&language)
            .or_else(|| crate::strings::bundled_locale_json("en"))
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok());
        let expected = catalog
            .as_ref()
            .and_then(|json| json.pointer("/gui/menu/file"))
            .and_then(serde_json::Value::as_str);
        let actual = crate::strings::get("gui.menu.file");
        g.check(
            "gate-applied-language",
            expected == Some(actual.as_str()),
            format!("requested {language}, rendered menu label {actual:?}"),
        );
        let expected_rtl = language == "ar";
        let actual_rtl = shell.window.direction() == gtk::TextDirection::Rtl;
        g.check(
            "gate-applied-direction",
            actual_rtl == expected_rtl,
            format!("requested RTL={expected_rtl}, GTK RTL={actual_rtl}"),
        );
    }
    if let Ok(scale) = super::dev_env("FMKV_GATE_SCALE") {
        let actual = shell.window.scale_factor();
        g.check(
            "gate-applied-scale",
            scale.parse::<i32>().ok() == Some(actual),
            format!("requested {scale}, GTK scale factor {actual}"),
        );
    }
}

fn run_after_tree(shell: &Rc<Shell>, dir: String, g: Rc<RefCell<Gate>>) {
    let root: gtk::Widget = shell.window.clone().upcast();
    let rows = chooser_rows(&root);
    g.borrow_mut().check(
        "main-chooser-inventory",
        rows.len() == 5,
        format!("{} of 5 selectors", rows.len()),
    );
    let me = shell.clone();
    let next_dir = dir.clone();
    let next_g = g.clone();
    chooser_sequence(
        root,
        dir,
        "main".into(),
        g,
        rows,
        0,
        Box::new(move || {
            menu_checks(&me, next_dir, next_g);
        }),
    );
}

fn menu_checks(shell: &Rc<Shell>, dir: String, g: Rc<RefCell<Gate>>) {
    let content_before = shell.toast_overlay.compute_bounds(&shell.window);
    shell.menu_btn.set_active(true);
    let me = shell.clone();
    glib::timeout_add_local_once(SETTLE, move || {
        let Some(popover) = me.menu_btn.popover() else {
            g.borrow_mut()
                .check("menu-panel", false, "menu has no popover");
            return finish(&dir, &g.borrow());
        };
        let root: gtk::Widget = popover.upcast();
        {
            let mut gate = g.borrow_mut();
            gate.check(
                "menu-keeps-content-in-place",
                content_before == me.toast_overlay.compute_bounds(&me.window),
                "opening the menu must not move or shrink the main content",
            );
            gate.check(
                "menu-compact-width",
                root.width() > 0 && root.width() < me.window.width() / 2,
                format!("menu {}px, window {}px", root.width(), me.window.width()),
            );
        }
        save(&root, &format!("{dir}/menu.png"), &mut g.borrow_mut());
        let rows = chooser_rows(me.menu_list.upcast_ref());
        g.borrow_mut().check(
            "menu-chooser-inventory",
            rows.len() == 4,
            format!("{} of 4 submenus", rows.len()),
        );
        let next_dir = dir.clone();
        let next_g = g.clone();
        chooser_sequence(
            root,
            dir,
            "menu".into(),
            g,
            rows,
            0,
            Box::new(move || {
                me.menu_btn.set_active(false);
                // Settings with a destination long enough to be cut off.
                let mut st = me.app.borrow().settings_form().clone();
                st.dest_dir = LONG_DEST.into();
                super::prefs::show(&me, &st, None);
                settings_page(
                    next_dir,
                    next_g,
                    vec![
                        "gui.tab.output",
                        "gui.tab.selection",
                        "gui.tab.recovery",
                        "gui.tab.keys",
                        "gui.tab.advanced",
                    ],
                );
            }),
        );
    });
}

fn chooser_rows(root: &gtk::Widget) -> Vec<adw::ExpanderRow> {
    descendants(root)
        .into_iter()
        .filter_map(|w| w.downcast::<adw::ExpanderRow>().ok())
        .filter(|w| w.is_mapped() && w.has_css_class("freemkv-chooser"))
        .collect()
}

/// Bring the field to the top of each enclosing viewport, without changing focus
/// or activating anything. This works for both the main window and Preferences.
fn scroll_to(widget: &gtk::Widget) {
    let mut parent = widget.parent();
    while let Some(w) = parent {
        if let Some(scroll) = w.downcast_ref::<gtk::ScrolledWindow>()
            && let Some(bounds) = widget.compute_bounds(scroll)
        {
            let adjustment = scroll.vadjustment();
            adjustment.set_value(adjustment.value() + f64::from(bounds.y()) - 12.0);
        }
        parent = w.parent();
    }
}

/// Expand each real control, measure its option geometry and capture the actual
/// window. Never infer success from widget types or a nonempty inventory alone.
fn chooser_sequence(
    root: gtk::Widget,
    dir: String,
    scope: String,
    g: Rc<RefCell<Gate>>,
    mut rows: Vec<adw::ExpanderRow>,
    index: usize,
    done: Box<dyn FnOnce()>,
) {
    if rows.is_empty() {
        done();
        return;
    }
    let row = rows.remove(0);
    row.set_expanded(true);
    glib::timeout_add_local_once(SETTLE, move || {
        scroll_to(row.upcast_ref());
        glib::timeout_add_local_once(SETTLE, move || {
            let options: Vec<_> = descendants(row.upcast_ref())
                .into_iter()
                .filter(|w| w.has_css_class("freemkv-choice") || w.has_css_class("freemkv-command"))
                .collect();
            let bounds = row.compute_bounds(&root);
            let header = labels_under(row.upcast_ref())
                .into_iter()
                .find(|label| label.text() == row.title());
            let header_bounds = header.and_then(|label| label.compute_bounds(&root));
            let below = options
                .first()
                .and_then(|option| option.compute_bounds(&root))
                .zip(header_bounds)
                .is_some_and(|(option, header)| option.y() >= header.y() + header.height());
            let full_width = bounds.is_some_and(|bounds| {
                options.iter().all(|option| {
                    option
                        .compute_bounds(&root)
                        .is_some_and(|b| b.width() >= bounds.width() - 32.0)
                })
            });
            let description = format!(
                "{scope} #{index}: {:?}, {} options",
                row.title(),
                options.len()
            );
            {
                let mut gate = g.borrow_mut();
                gate.check("chooser-options-present", !options.is_empty(), &description);
                if row.has_css_class("freemkv-single-choice") {
                    let active = descendants(row.upcast_ref())
                        .iter()
                        .filter_map(|w| w.downcast_ref::<gtk::CheckButton>())
                        .filter(|check| check.is_active())
                        .count();
                    gate.check("chooser-single-selection", active == 1, &description);
                }

                gate.check("chooser-expands-below", below, &description);
                gate.check("chooser-options-full-width", full_width, &description);
                gate.check(
                    "chooser-options-spacious",
                    options.iter().all(|w| w.height() >= 40),
                    &description,
                );
                save(
                    &root,
                    &format!("{dir}/{scope}-chooser-{index}.png"),
                    &mut gate,
                );
            }
            // Capture the tail too: long language lists must remain reachable.
            if let Some(last) = options.last() {
                scroll_to(last);
            }
            glib::timeout_add_local_once(SETTLE, move || {
                save(
                    &root,
                    &format!("{dir}/{scope}-chooser-{index}-tail.png"),
                    &mut g.borrow_mut(),
                );
                row.set_expanded(false);
                chooser_sequence(root, dir, scope, g, rows, index + 1, done);
            });
        });
    });
}

// Each page in turn: show it, let it draw, check and capture it; then report and exit.
fn settings_page(dir: String, g: Rc<RefCell<Gate>>, mut pages: Vec<&'static str>) {
    let Some(win) = prefs_window() else {
        g.borrow_mut()
            .check("settings-opens", false, "no Settings window");
        return finish(&dir, &g.borrow());
    };
    if pages.is_empty() {
        win.close();
        return finish(&dir, &g.borrow());
    }
    let page = pages.remove(0);
    win.set_visible_page_name(page);
    glib::timeout_add_local_once(SETTLE, move || {
        settings_checks(&win, page, &dir, &mut g.borrow_mut());
        let root: gtk::Widget = win.clone().upcast();
        let rows = chooser_rows(&root);
        let expected = match page {
            "gui.tab.output" => 2,
            "gui.tab.selection" => 4,
            "gui.tab.advanced" => 2,
            _ => 1,
        };
        g.borrow_mut().check(
            "settings-chooser-inventory",
            rows.len() == expected,
            format!("{page}: {} of {expected}", rows.len()),
        );
        let scope = format!("settings-{}", page.trim_start_matches("gui.tab."));
        let next_dir = dir.clone();
        let next_g = g.clone();
        chooser_sequence(
            root,
            dir,
            scope,
            g,
            rows,
            0,
            Box::new(move || {
                settings_page(next_dir, next_g, pages);
            }),
        );
    });
}

fn finish(dir: &str, g: &Gate) {
    let _ = std::fs::write(format!("{dir}/gate.txt"), g.report());
    std::process::exit(if g.passed() { 0 } else { 1 });
}

fn titles(shell: &Rc<Shell>, dir: &str, g: &mut Gate) {
    let v = shell.app.borrow().view();
    let titles: Vec<_> = v.title_rows.iter().filter(|r| r.depth == 1).collect();
    g.check(
        "fixture-opens",
        v.page == Page::Titles && titles.len() >= 2,
        format!("page {:?}, {} titles", v.page, titles.len()),
    );
    g.check(
        "titles-carry-length-and-size",
        !titles.is_empty()
            && titles
                .iter()
                .all(|r| !r.length.is_empty() && !r.size.is_empty()),
        format!(
            "{:?}",
            titles
                .iter()
                .map(|r| (&r.length, &r.size))
                .collect::<Vec<_>>()
        ),
    );

    let win: gtk::Widget = shell.window.clone().upcast();
    let labels = labels_under(&win);
    let shown = |text: &str| {
        labels
            .iter()
            .find(|l| l.is_mapped() && l.text() == text)
            .cloned()
    };
    let heads = [
        crate::strings::get_or("gui.col.duration", "Length"),
        crate::strings::get_or("gui.col.size", "Size"),
    ];
    let mut cells: Vec<(String, Option<gtk::Label>)> = heads
        .iter()
        .map(|h| (format!("header {h:?}"), shown(h)))
        .collect();
    for r in &titles {
        for text in [&r.length, &r.size] {
            cells.push((format!("cell {text:?}"), shown(text)));
        }
    }
    let px = capture(&win);
    for (what, label) in &cells {
        g.check(
            "column-text-shown-whole",
            label.as_ref().is_some_and(|l| !l.layout().is_ellipsized()),
            format!(
                "{what}: {}",
                match label {
                    None => "not on screen",
                    Some(l) if l.layout().is_ellipsized() => "ellipsized",
                    Some(_) => "whole",
                }
            ),
        );
        let ink = match (label, &px) {
            (Some(l), Some((w, h, px))) => {
                region(l, &win).is_some_and(|r| has_ink(px, *w, *h, false, r))
            }
            _ => false,
        };
        g.check("column-text-has-ink", ink, what);
    }
    save(&win, &format!("{dir}/titles.png"), g);
}

fn settings_checks(win: &adw::PreferencesWindow, page: &str, dir: &str, g: &mut Gate) {
    let root: gtk::Widget = win.clone().upcast();
    let all = descendants(&root);
    // Every dropdown on the page shows its choice in full under its title.
    for row in all
        .iter()
        .filter_map(|w| w.downcast_ref::<adw::ExpanderRow>())
        .filter(|r| r.is_mapped())
    {
        let choice = row.subtitle().to_string();
        let label = labels_under(row.upcast_ref())
            .into_iter()
            .find(|l| l.is_mapped() && l.text() == choice);
        g.check(
            "dropdown-choice-reads-whole",
            !choice.is_empty() && label.as_ref().is_some_and(|l| !l.layout().is_ellipsized()),
            format!(
                "{page} {:?}: {choice:?} {}",
                row.title(),
                match &label {
                    None => "not shown",
                    Some(l) if l.layout().is_ellipsized() => "ellipsized",
                    Some(_) => "whole",
                }
            ),
        );
    }
    // The long destination fits its field.
    if page == "gui.tab.output" {
        let entry = all
            .iter()
            .filter_map(|w| w.downcast_ref::<adw::EntryRow>())
            .find(|r| r.text() == LONG_DEST);
        let text = entry.and_then(|r| {
            descendants(r.upcast_ref())
                .into_iter()
                .find_map(|w| w.downcast::<gtk::Text>().ok())
        });
        let (need, have) = text.as_ref().map_or((0, 0), |t| {
            (
                t.create_pango_layout(Some(LONG_DEST)).pixel_size().0,
                t.width(),
            )
        });
        g.check(
            "long-destination-reads-whole",
            text.is_some() && need <= have,
            format!("{need} px of path in a {have} px field"),
        );
    }
    let name = page.trim_start_matches("gui.tab.");
    save(&root, &format!("{dir}/settings-{name}.png"), g);
}

fn prefs_window() -> Option<adw::PreferencesWindow> {
    gtk::Window::list_toplevels()
        .into_iter()
        .find_map(|w| w.downcast::<adw::PreferencesWindow>().ok())
}

fn descendants(w: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut out = Vec::new();
    let mut child = w.first_child();
    while let Some(c) = child {
        out.extend(descendants(&c));
        child = c.next_sibling();
        out.push(c);
    }
    out
}

fn labels_under(w: &gtk::Widget) -> Vec<gtk::Label> {
    descendants(w)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Label>().ok())
        .collect()
}

// `w`'s bounds within `root`, in the capture's pixels.
fn region(w: &gtk::Label, root: &gtk::Widget) -> Option<Region> {
    let b = w.compute_bounds(root)?;
    Some(Region {
        x: b.x().floor() as i32,
        y: b.y().floor() as i32,
        w: b.width().ceil() as i32,
        h: b.height().ceil() as i32,
    })
}

// A render of `w` as GTK draws it.
fn texture(w: &gtk::Widget) -> Option<gdk::Texture> {
    let paintable = gtk::WidgetPaintable::new(Some(w));
    let snap = gtk::Snapshot::new();
    paintable.snapshot(&snap, f64::from(w.width()), f64::from(w.height()));
    let node = snap.to_node()?;
    // The widget's own box, so pixel (0, 0) is its top-left whatever the node's bounds.
    let area = gtk::graphene::Rect::new(0.0, 0.0, w.width() as f32, w.height() as f32);
    Some(w.native()?.renderer()?.render_texture(node, Some(&area)))
}

// `(width, height, 32-bit rows top-down)` of `w`'s render.
fn capture(w: &gtk::Widget) -> Option<(i32, i32, Vec<u8>)> {
    let t = texture(w)?;
    let (cw, ch) = (t.width(), t.height());
    let mut px = vec![0u8; cw as usize * ch as usize * 4];
    t.download(&mut px, cw as usize * 4);
    Some((cw, ch, px))
}

pub(super) fn save(w: &gtk::Widget, path: &str, g: &mut Gate) {
    let ok = texture(w).is_some_and(|t| t.save_to_png(path).is_ok());
    g.check("capture", ok, path);
}
