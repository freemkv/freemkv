//! The review tour: after the gate's checks, every screen and state of the GTK shell the
//! `ui-screenshots` gallery shows that the checks did not already capture. The ready screen
//! with and without drives, the review banner, the log hidden and full of notices, each
//! submenu, About, Keyboard Shortcuts, the file choosers, a rip in progress and each result.
//!
//! The model is put into each state through `crate::gui_gate`'s fixtures and the shell's own
//! handlers. A capture the run cannot take is a `GATE NOTE`, never a failure: the gallery
//! shows the screen as missing.

use gtk4 as gtk;
use gtk4::glib;
use gtk4::prelude::*;
use libadwaita as adw;

use super::Shell;
use super::gate::{descendants, finish, labels_under, save_png};
use crate::engine::RunOutcome;
use crate::gui_gate::{self as fx, Gate, Rip};
use crate::ui::{Cmd, MenuGroupId, Page};

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

/// How long a step waits for GTK to lay out, animate and draw before it captures. About 35
/// steps at two of these each keeps a run well inside the driver's three-minute limit.
const SETTLE: Duration = Duration::from_millis(700);

type Setup = Box<dyn FnOnce(&Rc<Shell>)>;
type Target = Box<dyn Fn(&Rc<Shell>) -> Option<gtk::Widget>>;

/// One capture: put the app in a state, wait, capture `target`, undo what must be undone.
struct Shot {
    name: String,
    setup: Setup,
    target: Target,
    teardown: Setup,
}

fn shot(
    name: impl Into<String>,
    setup: impl FnOnce(&Rc<Shell>) + 'static,
    target: impl Fn(&Rc<Shell>) -> Option<gtk::Widget> + 'static,
    teardown: impl FnOnce(&Rc<Shell>) + 'static,
) -> Shot {
    Shot {
        name: name.into(),
        setup: Box::new(setup),
        target: Box::new(target),
        teardown: Box::new(teardown),
    }
}

fn window(shell: &Rc<Shell>) -> Option<gtk::Widget> {
    Some(shell.window.clone().upcast())
}

fn nothing(_: &Rc<Shell>) {}

/// The first visible top-level window of type `T` (a dialog the shell opened).
fn toplevel<T: IsA<gtk::Widget>>() -> Option<T> {
    gtk::Window::list_toplevels()
        .into_iter()
        .filter(|w| w.is_visible())
        .find_map(|w| w.downcast::<T>().ok())
}

/// The visible in-process file chooser, by type name: GTK's fallback when there is no
/// portal. A portal's chooser is another process and cannot be captured.
fn file_chooser() -> Option<gtk::Window> {
    gtk::Window::list_toplevels()
        .into_iter()
        .filter(|w| w.is_visible() && w.type_().name().contains("FileChooser"))
        .find_map(|w| w.downcast::<gtk::Window>().ok())
}

fn close_toplevels(_: &Rc<Shell>) {
    for w in gtk::Window::list_toplevels() {
        if let Ok(w) = w.downcast::<gtk::Window>()
            && w.is_visible()
            && !w.is::<adw::ApplicationWindow>()
        {
            w.close();
        }
    }
}

/// Show or hide the log through the real command, so the menu label follows.
fn log_shown(shell: &Rc<Shell>, shown: bool) {
    if shell.app.borrow().log_hidden == shown {
        shell.act(Cmd::ToggleLog);
    }
}

/// The ready screen with `drives`, and the tray state the heading reads.
fn home(shell: &Rc<Shell>, drives: Vec<crate::ui::HomeDrive>, present: Option<bool>, log: bool) {
    log_shown(shell, log);
    shell.app_mut(|a| {
        a.page = Page::Empty;
        a.home_drives = drives;
        a.disc_present = present;
    });
}

/// The disc page as the fixture opened it, with the log shown or hidden.
fn disc_page(shell: &Rc<Shell>, present: Option<bool>, log: bool) {
    log_shown(shell, log);
    shell.app_mut(|a| {
        a.page = Page::Titles;
        a.home_drives.clear();
        a.disc_present = present;
    });
}

/// The menu's submenu row titled `title`, activated as a click would; whether it was found.
fn open_submenu(shell: &Rc<Shell>, title: &str) -> bool {
    let Some(popover) = shell.menu_btn.popover() else {
        return false;
    };
    let row = descendants(popover.upcast_ref()).into_iter().find(|w| {
        w.type_().name() == "GtkModelButton"
            && labels_under(w).iter().any(|l| l.text().as_str() == title)
    });
    row.is_some_and(|row| row.activate())
}

fn menu_popover(shell: &Rc<Shell>) -> Option<gtk::Widget> {
    shell.menu_btn.popover().map(|p| p.upcast())
}

/// The first drive row's actions button ("Use at startup", "Eject").
fn drive_actions(shell: &Rc<Shell>) -> Option<gtk::MenuButton> {
    let main = shell.main.borrow().clone()?;
    descendants(main.drives.upcast_ref())
        .into_iter()
        .find_map(|w| w.downcast::<gtk::MenuButton>().ok())
}

pub(super) fn start(shell: &Rc<Shell>, dir: String, g: Rc<RefCell<Gate>>) {
    let present = shell.app.borrow().disc_present;
    let review_before = Rc::new(RefCell::new(None));
    let mut shots = VecDeque::new();

    // ── the ready screen ──
    shots.push_back(shot(
        "home-drives-log-hidden",
        |s| home(s, fx::fake_drives(), None, false),
        window,
        nothing,
    ));
    shots.push_back(shot(
        "home-drive-actions-open",
        |s| {
            home(s, fx::fake_drives(), None, true);
            if let Some(b) = drive_actions(s) {
                b.popup();
            }
        },
        |s| {
            drive_actions(s)
                .and_then(|b| b.popover())
                .map(|p| p.upcast())
        },
        |s| {
            if let Some(b) = drive_actions(s) {
                b.popdown();
            }
        },
    ));
    shots.push_back(shot(
        "home-no-drives",
        |s| home(s, Vec::new(), None, true),
        window,
        nothing,
    ));
    shots.push_back(shot(
        "home-no-disc",
        |s| home(s, Vec::new(), Some(false), true),
        window,
        nothing,
    ));

    // ── the disc page ──
    let before = review_before.clone();
    shots.push_back(shot(
        "disc-page-review-banner",
        move |s| {
            disc_page(s, present, true);
            s.app_mut(|a| *before.borrow_mut() = a.tree.review.replace(fx::review()));
        },
        window,
        move |s| s.app_mut(|a| a.tree.review = review_before.borrow_mut().take()),
    ));
    shots.push_back(shot(
        "disc-page-log-hidden",
        move |s| disc_page(s, present, false),
        window,
        nothing,
    ));
    shots.push_back(shot(
        "disc-page-notices",
        move |s| {
            disc_page(s, present, true);
            s.app_mut(fx::notices);
        },
        window,
        nothing,
    ));

    // ── the menu's submenus, and what they open ──
    for group in crate::ui::menu_layout(false) {
        let name = match group.id {
            MenuGroupId::App => continue,
            MenuGroupId::File => "menu-file",
            MenuGroupId::Edit => "menu-edit",
            MenuGroupId::View => "menu-view",
            MenuGroupId::Help => "menu-help",
        };
        let title = group.title.clone();
        let (g2, label) = (g.clone(), name.to_string());
        shots.push_back(shot(
            name,
            move |s| {
                s.menu_btn.popup();
                let me = s.clone();
                glib::timeout_add_local_once(Duration::from_millis(400), move || {
                    if !open_submenu(&me, &title) {
                        g2.borrow_mut()
                            .note(&label, format!("no submenu row titled {title:?}"));
                    }
                });
            },
            menu_popover,
            |s| s.menu_btn.popdown(),
        ));
    }
    shots.push_back(shot(
        "about",
        |s| s.show_about(),
        |_| toplevel::<adw::AboutWindow>().map(|w| w.upcast()),
        close_toplevels,
    ));
    shots.push_back(shot(
        "shortcuts",
        |s| s.show_shortcuts(),
        |_| toplevel::<gtk::ShortcutsWindow>().map(|w| w.upcast()),
        close_toplevels,
    ));
    for (name, cmd) in [
        ("open-file-dialog", Cmd::Open),
        ("open-folder-dialog", Cmd::OpenFolder),
        ("output-folder-dialog", Cmd::SetOutput),
    ] {
        shots.push_back(shot(
            name,
            move |s| s.act(cmd),
            |_| file_chooser().map(|w| w.upcast()),
            close_toplevels,
        ));
    }

    // ── a rip in progress ──
    for (name, rip, log) in [
        ("rip-single", Rip::Single, true),
        ("rip-multi", Rip::Multi, true),
        ("rip-multi-reading", Rip::Reading, true),
        ("rip-multi-log-hidden", Rip::Multi, false),
        ("rip-stopping", Rip::Stopping, true),
    ] {
        shots.push_back(shot(
            name,
            move |s| {
                log_shown(s, log);
                s.app_mut(|a| fx::show_rip(a, rip));
            },
            window,
            nothing,
        ));
    }
    shots.push_back(shot(
        "rip-confirm-quit",
        |s| {
            s.app_mut(|a| fx::show_rip(a, Rip::Multi));
            s.confirm_quit();
        },
        |_| toplevel::<adw::MessageDialog>().map(|w| w.upcast()),
        |s| {
            close_toplevels(s);
            s.app_mut(|a| fx::end_rip(a, Page::Titles));
        },
    ));

    // ── the result page; the toast last, as it outlives its capture ──
    for (name, outcome, log) in [
        ("result-finished", RunOutcome::Completed, true),
        ("result-finished-log-hidden", RunOutcome::Completed, false),
        ("result-cancelled", RunOutcome::Cancelled, true),
        ("result-failed", RunOutcome::Failed, true),
    ] {
        shots.push_back(shot(
            name,
            move |s| {
                log_shown(s, log);
                s.app_mut(|a| fx::show_result(a, outcome));
            },
            window,
            nothing,
        ));
    }
    shots.push_back(shot(
        "result-finished-toast",
        |s| {
            s.app_mut(|a| fx::show_result(a, RunOutcome::Completed));
            let heading = crate::strings::get_or("gui.notify.rip_finished_title", "Rip finished");
            let body = s.app.borrow().result_summary.clone();
            s.notify_finished(&heading, &body, Some(crate::gui_gate::LONG_DEST));
        },
        window,
        nothing,
    ));

    run(shell.clone(), dir, g, shots);
}

fn run(shell: Rc<Shell>, dir: String, g: Rc<RefCell<Gate>>, mut shots: VecDeque<Shot>) {
    let Some(step) = shots.pop_front() else {
        return finish(&dir, &g.borrow());
    };
    (step.setup)(&shell);
    glib::timeout_add_local_once(SETTLE, move || {
        let path = format!("{dir}/{}.png", step.name);
        match (step.target)(&shell) {
            Some(w) if save_png(&w, &path) => {}
            Some(_) => g
                .borrow_mut()
                .note(&step.name, "the capture was not written"),
            None => g.borrow_mut().note(
                &step.name,
                "nothing to capture: it never appeared in this process",
            ),
        }
        (step.teardown)(&shell);
        glib::timeout_add_local_once(SETTLE, move || run(shell, dir, g, shots));
    });
}
