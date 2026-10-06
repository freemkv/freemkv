use super::*;

// Sections, not rows, carry the group titles; every row keeps an action.
#[test]
fn the_hamburger_has_a_row_for_every_layout_action() {
    let m = build_menu_model(false);
    let mut actions = Vec::new();
    for s in 0..m.n_items() {
        let Some(sec) = m.item_link(s, "section") else {
            continue;
        };
        for i in 0..sec.n_items() {
            if let Some(v) = sec.item_attribute_value(i, "action", None) {
                actions.push(v.get::<String>().unwrap_or_default());
            }
        }
    }
    for (name, _) in glue::ACTIONS {
        assert!(actions.contains(&format!("app.{name}")), "{name} missing");
    }
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
