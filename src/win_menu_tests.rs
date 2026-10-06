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
