use super::*;

// Every `app.` action named anywhere in `m`, sections and submenus included.
fn menu_actions(m: &gio::MenuModel, out: &mut Vec<String>) {
    for i in 0..m.n_items() {
        if let Some(v) = m.item_attribute_value(i, "action", None) {
            out.push(v.get::<String>().unwrap_or_default());
        }
        for link in ["section", "submenu"] {
            if let Some(child) = m.item_link(i, link) {
                menu_actions(&child, out);
            }
        }
    }
}

// The compact hamburger (submenus, not one long list) still reaches every layout action.
#[test]
fn the_hamburger_has_a_row_for_every_layout_action() {
    let m = build_menu_model(false);
    let mut actions = Vec::new();
    menu_actions(m.upcast_ref(), &mut actions);
    for (name, _) in glue::ACTIONS {
        assert!(actions.contains(&format!("app.{name}")), "{name} missing");
    }
    assert!(actions.contains(&format!("app.{SHORTCUTS_ACTION}")));
}

// Field feedback: the hamburger ran to ~16 rows. Its top level stays at a handful.
#[test]
fn the_hamburger_top_level_is_compact() {
    let m = build_menu_model(false);
    let mut rows = 0;
    for s in 0..m.n_items() {
        rows += m.item_link(s, "section").map_or(1, |sec| sec.n_items());
    }
    assert!(rows <= 8, "{rows} top-level rows");
}

// The shortcuts window lists every layout accelerator, plus its own.
#[test]
fn the_shortcuts_window_lists_every_accelerator() {
    let ui = shortcuts_ui();
    for group in crate::ui::menu_layout(false) {
        for entry in group.entries {
            let MenuEntry::Item(mi) = entry else { continue };
            let (Some(_), Some(a)) = (glue::action_name_for(&mi.action), mi.accel) else {
                continue;
            };
            let accel = xml_escape(&glue::accel_string(&a));
            assert!(ui.contains(&accel), "{} ({accel}) missing", mi.label);
        }
    }
    assert!(ui.contains(&xml_escape(SHORTCUTS_ACCEL)));
}

// GTK's own keysym table (no display needed), not a character-class guess.
#[test]
fn every_layout_accelerator_names_a_real_gdk_key() {
    const MODS: [&str; 3] = ["<Primary>", "<Alt>", "<Shift>"];
    for group in crate::ui::menu_layout(false) {
        for entry in group.entries {
            let MenuEntry::Item(mi) = entry else { continue };
            let Some(a) = mi.accel else { continue };
            let s = glue::accel_string(&a);
            let mut key = s.as_str();
            while let Some(m) = MODS.iter().find(|m| key.starts_with(**m)) {
                key = &key[m.len()..];
            }
            assert!(gdk::Key::from_name(key).is_some(), "{s}: no GDK key {key}");
        }
    }
}
