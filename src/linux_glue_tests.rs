use super::*;
use crate::ui::{Check, MenuEntry, menu_layout};

#[test]
fn every_menu_row_the_layout_names_has_an_action_or_is_standard_text() {
    for group in menu_layout(false) {
        for entry in group.entries {
            let MenuEntry::Item(mi) = entry else { continue };
            if is_standard_text_action(&mi.action) {
                assert!(action_name_for(&mi.action).is_none());
                continue;
            }
            assert!(
                action_name_for(&mi.action).is_some(),
                "{:?} is in the layout but has no Linux action",
                mi.action
            );
        }
    }
}

#[test]
fn action_names_are_unique() {
    let mut names: Vec<&str> = ACTIONS.iter().map(|(n, _)| *n).collect();
    names.push(REVEAL_ACTION);
    let n = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), n);
}

#[test]
fn open_disc_is_blocked_mid_rip_like_open_and_cancel_is_never_a_menu_row() {
    let od = gating_cmd(&MenuAction::OpenDisc).unwrap();
    assert!(crate::ui::blocked_while_running(od));
    assert!(action_name_for(&MenuAction::Cmd(Cmd::Cancel)).is_none());
    assert!(gating_cmd(&MenuAction::StandardCopy).is_none());
    // The log stays usable while ripping.
    let tl = gating_cmd(&MenuAction::Cmd(Cmd::ToggleLog)).unwrap();
    assert!(!crate::ui::blocked_while_running(tl));
}

#[test]
fn accelerators_are_spelled_the_way_gtk_parses_them() {
    assert_eq!(accel_string(&Accel::primary("o")), "<Primary>o");
    assert_eq!(
        accel_string(&Accel::primary_shift("A")),
        "<Primary><Shift>a"
    );
    assert_eq!(accel_string(&Accel::primary(",")), "<Primary>comma");
    assert_eq!(accel_string(&Accel::bare("F1")), "F1");
}

#[test]
fn every_layout_accelerator_is_a_well_formed_gtk_string() {
    for group in menu_layout(false) {
        for entry in group.entries {
            let MenuEntry::Item(mi) = entry else { continue };
            let Some(a) = mi.accel else { continue };
            let s = accel_string(&a);
            let key = s.rsplit('>').next().unwrap();
            assert!(
                key.chars().all(|c| c.is_ascii_alphanumeric()),
                "{s} would not parse as a GTK accelerator"
            );
        }
    }
}

#[test]
fn a_drop_accepts_folders_and_source_files_in_any_case_only() {
    let dir = std::env::temp_dir();
    assert!(is_openable_source(&dir));
    for ok in [
        "/tmp/x/Movie.iso",
        "/tmp/x/Movie.ISO",
        "/tmp/x/a.MkV",
        "/tmp/x/b.m2ts",
    ] {
        assert!(is_openable_source(std::path::Path::new(ok)), "{ok}");
    }
    for bad in ["/tmp/x/notes.txt", "/tmp/x/noext", "/tmp/x/a.iso.part"] {
        assert!(!is_openable_source(std::path::Path::new(bad)), "{bad}");
    }
}

#[test]
fn the_format_rows_are_every_offered_format_in_order() {
    let groups = crate::ui::output_formats(true, true);
    let labels = flat_format_labels(&groups);
    let flat: Vec<&str> = groups.iter().flatten().copied().collect();
    assert_eq!(labels.len(), flat.len());
    for (label, canon) in labels.iter().zip(flat) {
        assert_eq!(crate::ui::format_from_label(label, true, true), Some(canon));
    }
}

#[test]
fn page_names_are_distinct_and_only_rip_pages_hand_the_log_the_height() {
    let pages = [Page::Empty, Page::Titles, Page::Progress, Page::Result];
    let mut names: Vec<_> = pages.iter().map(|p| page_name(*p)).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 4);
    assert!(!log_fills(Page::Titles) && !log_fills(Page::Empty));
    assert!(log_fills(Page::Progress) && log_fills(Page::Result));
}

fn row(i: usize, depth: u8, t: &str) -> Row {
    Row {
        index: i,
        depth,
        type_s: t.into(),
        desc: format!("d{i}"),
        length: String::new(),
        size: String::new(),
        lang: String::new(),
        item: String::new(),
        format: String::new(),
        notes: String::new(),
        check: Some(Check::Off),
        check_enabled: true,
    }
}

#[test]
fn the_tree_shape_follows_the_core_parent_walk() {
    let rows = vec![
        row(0, 0, "Disc"),
        row(1, 1, "Title"),
        row(2, 2, "Audio"),
        row(3, 1, "Title"),
    ];
    let (roots, kids) = tree_shape(&rows);
    assert_eq!(roots, vec![0]);
    assert_eq!(kids[0], vec![1, 3]);
    assert_eq!(kids[1], vec![2]);
    assert!(kids[2].is_empty() && kids[3].is_empty());
}

#[test]
fn the_row_signature_ignores_ticks_but_not_rows() {
    let a = vec![row(0, 0, "Disc"), row(1, 1, "Title")];
    let mut b = a.clone();
    b[1].check = Some(Check::On);
    assert_eq!(rows_sig(&a), rows_sig(&b));
    b[1].desc = "other".into();
    assert_ne!(rows_sig(&a), rows_sig(&b));
}

#[test]
fn the_row_signature_sees_every_identity_field_and_the_row_count() {
    let a = vec![row(0, 0, "Disc"), row(1, 1, "Title"), row(2, 1, "Title")];
    let changed: [fn(&mut Vec<Row>); 7] = [
        |v| v[1].index = 7,
        |v| v[1].depth = 2,
        |v| v[1].type_s = "Audio".into(),
        |v| v[1].length = "1:30:00".into(),
        |v| v[1].size = "6.8 GB".into(),
        |v| {
            v.pop();
        },
        |v| v.swap(1, 2),
    ];
    for (i, change) in changed.iter().enumerate() {
        let mut b = a.clone();
        change(&mut b);
        assert_ne!(rows_sig(&a), rows_sig(&b), "change #{i} went unnoticed");
    }
}

#[test]
fn the_menu_gate_matches_the_core_dispatch_rule() {
    for (_, ma) in ACTIONS {
        let gate = gating_cmd(ma);
        assert!(
            action_enabled(gate, false, false),
            "{ma:?} greyed while idle"
        );
    }
    let open = Some(Cmd::Open);
    assert!(!action_enabled(open, true, false));
    // A drop or Open Disc mid-scan would be silently refused by the core.
    assert!(!action_enabled(open, false, true));
    assert!(action_enabled(Some(Cmd::Close), false, true));
    assert!(!action_enabled(Some(Cmd::Close), true, false));
    assert!(action_enabled(Some(Cmd::ToggleLog), true, true));
    assert!(action_enabled(None, true, true));
}

#[test]
fn open_disc_names_one_drive_autodetects_several_and_stops_on_none() {
    let drive = |d: &str| crate::engine::OpticalDrive {
        device: d.into(),
        label: "HL-DT-ST\u{202e} BD".into(),
    };
    let (k, _, url) = disc_open_plan(&[]);
    assert_eq!((k, url), (LogKind::Notice, None));
    let (k, line, url) = disc_open_plan(&[drive("/dev/sr0")]);
    assert_eq!(
        (k, url.as_deref()),
        (LogKind::Detail, Some("disc:///dev/sr0"))
    );
    assert!(
        line.contains("/dev/sr0") && !line.contains('\u{202e}'),
        "{line}"
    );
    let (_, line, url) = disc_open_plan(&[drive("/dev/sr0"), drive("/dev/sr1")]);
    assert_eq!(url.as_deref(), Some("disc://"));
    assert!(line.contains("/dev/sr1"), "{line}");
    assert!(!line.contains('\u{202e}'), "{line}");
}

#[test]
fn a_failed_keydb_update_is_logged_as_a_notice() {
    let (k, m) = keydb_update_line(Err("Download failed: x".into()));
    assert_eq!((k, m.as_str()), (LogKind::Notice, "Download failed: x"));
    let (k, _) = keydb_update_line(Ok("keydb updated".into()));
    assert_eq!(k, LogKind::Result);
}

#[test]
fn the_log_appends_while_log_first_holds_and_rebuilds_when_it_moves() {
    let m = |first, len| LogMemo { first, len };
    assert_eq!(log_delta(&m(0, 2), &m(0, 2)), LogDelta::Same);
    assert_eq!(log_delta(&m(0, 2), &m(0, 5)), LogDelta::Append(2));
    assert_eq!(
        log_delta(&LogMemo::default(), &m(0, 1)),
        LogDelta::Append(0)
    );
    // A clear or a front-trim bumps `log_first`: the screen is stale.
    assert_eq!(log_delta(&m(0, 5), &m(5, 0)), LogDelta::Rewrite);
    assert_eq!(log_delta(&m(0, 5000), &m(1000, 4001)), LogDelta::Rewrite);
    // Fewer lines under the same first: nothing to append from.
    assert_eq!(log_delta(&m(0, 5), &m(0, 3)), LogDelta::Rewrite);
}

#[test]
fn log_first_really_moves_on_clear_so_the_pane_rebuilds() {
    let mut app = crate::ui::App::new();
    let before = app.view();
    let _ = app.dispatch(Cmd::ClearLog);
    let after = app.view();
    let prev = LogMemo {
        first: before.log_first,
        len: before.log.len(),
    };
    let now = LogMemo {
        first: after.log_first,
        len: after.log.len(),
    };
    assert_eq!(log_delta(&prev, &now), LogDelta::Rewrite);
}

#[test]
fn row_titles_drop_the_label_column_colon() {
    assert_eq!(row_title("Default output :"), "Default output");
    assert_eq!(row_title("Sortie par défaut\u{a0}:"), "Sortie par défaut");
    assert_eq!(row_title("出力："), "出力");
    assert_eq!(row_title("No colon"), "No colon");
}

#[test]
fn a_quit_asks_once_and_only_mid_rip() {
    assert!(!needs_rip_confirmation(false, false));
    assert!(needs_rip_confirmation(true, false));
    assert!(!needs_rip_confirmation(true, true));
}

#[test]
fn settings_dropdowns_round_trip_their_stored_value() {
    for key in [
        "container",
        "selection",
        "rip_mode",
        "key_source",
        "log_level",
        "language",
    ] {
        let opts = settings_options(key);
        assert!(!opts.is_empty(), "{key} has no options");
        for (i, (canon, _)) in opts.iter().enumerate() {
            assert_eq!(option_index(&opts, canon), i as u32, "{key}/{canon}");
        }
        assert_eq!(option_index(&opts, "__not_offered__"), 0);
    }
}

#[test]
fn a_stored_language_off_the_curated_list_still_gets_a_row() {
    let codes = lang_picker_codes("deu,zz9");
    assert_eq!(codes.len(), crate::ui::PICKER_LANGUAGES.len() + 1);
    assert_eq!(codes.last().map(String::as_str), Some("zz9"));
    assert_eq!(
        lang_picker_codes("").len(),
        crate::ui::PICKER_LANGUAGES.len()
    );
}
