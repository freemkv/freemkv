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
mod tests {
    use super::*;

    fn menus() -> Vec<WinMenu> {
        win_menus(&crate::ui::menu_layout(false), "Exit")
    }

    fn hinted(menus: &[WinMenu]) -> Vec<(MenuAction, Accel)> {
        menus
            .iter()
            .flat_map(|m| m.entries.iter())
            .filter_map(|e| match e {
                WinEntry::Item {
                    action,
                    accel: Some(a),
                    ..
                } => Some((*action, *a)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_shortcut_the_menu_shows_is_in_the_accelerator_table() {
        let menus = menus();
        let table = accel_table(&menus);
        for (action, a) in hinted(&menus) {
            if native_edit_action(&action) {
                continue;
            }
            let key = win_key(&a).unwrap_or_else(|| panic!("{a:?} has no virtual key"));
            assert!(
                table.contains(&(action, key)),
                "menu shows {} for {action:?} but the accelerator table has no such key",
                accel_suffix(&a)
            );
        }
    }

    #[test]
    fn the_table_binds_nothing_the_menu_does_not_show() {
        let menus = menus();
        let shown = hinted(&menus);
        let table = accel_table(&menus);
        for (action, key) in &table {
            assert!(
                shown
                    .iter()
                    .any(|(a, acc)| a == action && win_key(acc) == Some(*key)),
                "{action:?} is bound to {key:?} with no matching menu hint"
            );
            assert_eq!(
                table.iter().filter(|(_, k)| k == key).count(),
                1,
                "{key:?} is bound twice"
            );
        }
        // Quit is not on the Windows menu (Exit is Alt+F4), so Ctrl+Q stays free.
        assert!(!table.iter().any(|(a, _)| *a == MenuAction::Cmd(Cmd::Quit)));
    }

    #[test]
    fn settings_and_select_all_titles_are_real_keys() {
        let table = accel_table(&menus());
        let ctrl = |vk: u16, shift: bool| WinKey {
            vk,
            ctrl: true,
            shift,
            alt: false,
        };
        assert!(table.contains(&(MenuAction::Cmd(Cmd::Settings), ctrl(0xBC, false))));
        assert!(table.contains(&(MenuAction::Cmd(Cmd::SelectAll), ctrl(u16::from(b'A'), true))));
    }

    // The EDIT controls run Copy and Select All on their own keys; a table entry would take
    // those keys from them.
    #[test]
    fn the_table_leaves_the_text_keys_to_the_focused_control() {
        let menus = menus();
        assert!(
            hinted(&menus)
                .iter()
                .any(|(a, _)| *a == MenuAction::StandardCopy),
            "fixture: the menu shows Copy"
        );
        for (action, _) in accel_table(&menus) {
            assert!(!native_edit_action(&action), "{action:?} is in the table");
        }
        assert!(native_edit_action(&MenuAction::StandardSelectAllText));
        assert!(!native_edit_action(&MenuAction::Cmd(Cmd::SelectAll)));
    }

    #[test]
    fn win_key_maps_letters_punctuation_and_function_keys() {
        assert_eq!(win_key(&Accel::primary("o")).map(|k| k.vk), Some(0x4F));
        assert_eq!(win_key(&Accel::primary(",")).map(|k| k.vk), Some(0xBC));
        assert_eq!(win_key(&Accel::bare("F1")).map(|k| k.vk), Some(0x70));
        assert_eq!(win_key(&Accel::bare("F12")).map(|k| k.vk), Some(0x7B));
        assert_eq!(win_key(&Accel::bare("F0")), None);
        assert_eq!(win_key(&Accel::bare("Home")), None);
        let k = win_key(&Accel::primary_shift("A")).unwrap_or_else(|| panic!("no key"));
        assert!(k.ctrl && k.shift && !k.alt);
    }

    #[test]
    fn menus_follow_the_windows_placement() {
        let menus = menus();
        assert_eq!(menus.len(), 4, "File/Edit/View/Help");
        let texts = |m: &WinMenu| -> Vec<String> {
            m.entries
                .iter()
                .filter_map(|e| match e {
                    WinEntry::Item { text, .. } => Some(text.clone()),
                    WinEntry::Separator => None,
                })
                .collect()
        };
        let file = texts(&menus[0]);
        assert_eq!(file.last().map(String::as_str), Some("Exit\tAlt+F4"));
        assert!(file.iter().any(|t| t.ends_with("\tCtrl+,")), "{file:?}");
        let edit = texts(&menus[1]);
        assert!(
            !edit
                .iter()
                .any(|t| t.ends_with("\tCtrl+X") || t.ends_with("\tCtrl+V"))
        );
        assert!(
            edit.iter().any(|t| t.ends_with("\tCtrl+Shift+A")),
            "{edit:?}"
        );
        let help = &menus[3].entries;
        assert!(matches!(
            help.last(),
            Some(WinEntry::Item {
                action: MenuAction::Cmd(Cmd::About),
                ..
            })
        ));
    }

    #[test]
    fn a_group_missing_from_the_layout_is_skipped_not_fatal() {
        let layout: Vec<MenuGroup> = crate::ui::menu_layout(false)
            .into_iter()
            .filter(|g| !matches!(g.id, MenuGroupId::App | MenuGroupId::Edit))
            .collect();
        let menus = win_menus(&layout, "Exit");
        assert_eq!(menus.len(), 3, "File/View/Help");
        assert!(
            !menus
                .iter()
                .flat_map(|m| m.entries.iter())
                .any(|e| matches!(
                    e,
                    WinEntry::Item {
                        action: MenuAction::Cmd(Cmd::Settings | Cmd::About),
                        ..
                    }
                ))
        );
    }
}
