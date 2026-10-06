//! The Windows menu bar and its accelerator table as plain data, derived from
//! [`crate::ui::menu_layout`]. No Win32 types and not `cfg(windows)`, so the
//! rule "every shortcut the menu shows is a real key" is tested everywhere.

use crate::ui::{Accel, Cmd, MenuAction, MenuEntry, MenuGroup, MenuGroupId};

/// One row of a Windows popup menu.
#[derive(Clone, Debug, PartialEq)]
pub enum WinEntry {
    Separator,
    Item {
        action: MenuAction,
        /// Label plus the `\t`-separated shortcut hint, as the menu shows it.
        text: String,
        /// The key behind the hint; `None` when the hint is system-handled (Alt+F4).
        accel: Option<Accel>,
    },
}

/// One top-level Windows menu.
#[derive(Clone, Debug)]
pub struct WinMenu {
    pub title: String,
    pub entries: Vec<WinEntry>,
}

/// A Win32 accelerator: virtual-key code plus modifiers.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct WinKey {
    pub vk: u16,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

/// Render an [`Accel`] as a Windows menu suffix ("Ctrl+O", "F1").
/// `primary` is Ctrl on Windows.
pub fn accel_suffix(a: &Accel) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if a.primary {
        parts.push("Ctrl");
    }
    if a.alt {
        parts.push("Alt");
    }
    if a.shift {
        parts.push("Shift");
    }
    let key = if a.key.len() == 1 {
        a.key.to_ascii_uppercase()
    } else {
        a.key.to_string()
    };
    parts.push(&key);
    parts.join("+")
}

pub fn item_text(label: &str, accel: Option<&Accel>) -> String {
    match accel {
        Some(a) => format!("{label}\t{}", accel_suffix(a)),
        None => label.to_string(),
    }
}

/// The virtual key an [`Accel`] names; `None` for a key Windows can't bind.
pub fn win_key(a: &Accel) -> Option<WinKey> {
    let vk = match a.key.as_bytes() {
        [c] if c.is_ascii_alphanumeric() => u16::from(c.to_ascii_uppercase()),
        b"," => 0xBC, // VK_OEM_COMMA
        b"." => 0xBE, // VK_OEM_PERIOD
        [b'F', n @ ..] => match std::str::from_utf8(n).ok()?.parse::<u16>().ok()? {
            n @ 1..=24 => 0x70 + n - 1, // VK_F1..VK_F24
            _ => return None,
        },
        _ => return None,
    };
    Some(WinKey {
        vk,
        ctrl: a.primary,
        shift: a.shift,
        alt: a.alt,
    })
}

/// Text commands the focused EDIT control already runs on its own keys: the
/// menu shows their hint, but a table entry would take the key from it.
pub fn native_edit_action(a: &MenuAction) -> bool {
    matches!(
        a,
        MenuAction::StandardCopy | MenuAction::StandardSelectAllText
    )
}

/// The Windows convention: no App menu — Settings + Exit end File, About ends
/// Help; Cut/Paste are left off (no handler). A group missing from `layout` is
/// skipped rather than fatal.
pub fn win_menus(layout: &[MenuGroup], exit_label: &str) -> Vec<WinMenu> {
    let group = |id: MenuGroupId| layout.iter().find(|g| g.id == id);
    let item = |action: MenuAction, label: &str, accel: Option<Accel>| WinEntry::Item {
        action,
        text: item_text(label, accel.as_ref()),
        accel,
    };
    let entries = |g: &MenuGroup, keep: &dyn Fn(&MenuAction) -> bool| -> Vec<WinEntry> {
        g.entries
            .iter()
            .filter_map(|e| match e {
                MenuEntry::Separator => Some(WinEntry::Separator),
                MenuEntry::Item(mi) if keep(&mi.action) => {
                    Some(item(mi.action, &mi.label, mi.accel))
                }
                MenuEntry::Item(_) => None,
            })
            .collect()
    };
    let promoted = |cmd: Cmd| -> Vec<WinEntry> {
        group(MenuGroupId::App)
            .map(|g| entries(g, &|a| *a == MenuAction::Cmd(cmd)))
            .unwrap_or_default()
            .into_iter()
            .filter(|e| *e != WinEntry::Separator)
            .collect()
    };
    let on_windows =
        |a: &MenuAction| !matches!(a, MenuAction::StandardCut | MenuAction::StandardPaste);

    let mut out = Vec::new();
    for id in [
        MenuGroupId::File,
        MenuGroupId::Edit,
        MenuGroupId::View,
        MenuGroupId::Help,
    ] {
        let Some(g) = group(id) else { continue };
        let mut e = entries(g, &on_windows);
        match id {
            MenuGroupId::File => {
                e.push(WinEntry::Separator);
                e.extend(promoted(Cmd::Settings));
                e.push(WinEntry::Separator);
                e.push(WinEntry::Item {
                    action: MenuAction::Cmd(Cmd::Quit),
                    text: format!("{exit_label}\tAlt+F4"),
                    accel: None,
                });
            }
            MenuGroupId::Help => {
                e.push(WinEntry::Separator);
                e.extend(promoted(Cmd::About));
            }
            _ => {}
        }
        out.push(WinMenu {
            title: g.title.clone(),
            entries: e,
        });
    }
    out
}

/// The accelerator table, derived from the hints `menus` shows so the two
/// cannot disagree. Native text keys are left to the focused control.
pub fn accel_table(menus: &[WinMenu]) -> Vec<(MenuAction, WinKey)> {
    menus
        .iter()
        .flat_map(|m| m.entries.iter())
        .filter_map(|e| match e {
            WinEntry::Item {
                action,
                accel: Some(a),
                ..
            } if !native_edit_action(action) => Some((*action, win_key(a)?)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
#[path = "win_menu_tests.rs"]
mod tests;
