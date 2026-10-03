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
use crate::ui::Page;

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
        let (me, dir) = (me.clone(), dir.clone());
        glib::timeout_add_local_once(SETTLE, move || run(&me, dir));
        glib::ControlFlow::Break
    });
}

fn run(shell: &Rc<Shell>, dir: String) {
    let _ = std::fs::create_dir_all(&dir);
    let g = Rc::new(RefCell::new(Gate::default()));
    titles(shell, &dir, &mut g.borrow_mut());

    // Settings, with a destination long enough to be cut off.
    shell.settings.borrow_mut().dest_dir = LONG_DEST.into();
    super::prefs::show(shell, None);
    let pages = ["gui.tab.output", "gui.tab.keys", "gui.tab.advanced"];
    settings_page(dir, g, pages.to_vec());
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
        settings_page(dir, g, pages);
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
        .filter_map(|w| w.downcast_ref::<adw::ComboRow>())
        .filter(|r| r.is_mapped())
    {
        let choice = row
            .selected_item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string().to_string())
            .unwrap_or_default();
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

fn save(w: &gtk::Widget, path: &str, g: &mut Gate) {
    let ok = texture(w).is_some_and(|t| t.save_to_png(path).is_ok());
    g.check("capture", ok, path);
}
