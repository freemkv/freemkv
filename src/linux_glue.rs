//! The GTK shell's decisions that need no GTK — the platform-neutral half of
//! `linux.rs`.
//!
//! Deliberately NOT `cfg(target_os = "linux")`, for the same reason
//! `win_layout.rs` is not `cfg(windows)`: gating it would make these unit tests
//! unrunnable on the macOS and Windows CI legs. Nothing here links GTK, so the
//! musl CLI build and every other target compile it as plain Rust.
//!
//! What lives here is shell *glue*, not product behaviour: which `gio` action
//! name a `MenuAction` rides on, how a shared `Accel` is spelled for GTK, which
//! stack page a `Page` is, whether a log redraw can append instead of rewrite.
//! Anything that decides what the product DOES stays in `ui.rs`.

use crate::ui::{Accel, Cmd, MenuAction, Page, Row};

/// Every `gio` action the hamburger menu and the accelerators ride on, with
/// the `MenuAction` it performs. One table so the action map, the menu model
/// and the keyboard shortcuts cannot name different things.
pub const ACTIONS: &[(&str, MenuAction)] = &[
    ("open", MenuAction::Cmd(Cmd::Open)),
    ("open-disc", MenuAction::OpenDisc),
    ("close", MenuAction::Cmd(Cmd::Close)),
    ("set-output", MenuAction::Cmd(Cmd::SetOutput)),
    ("run", MenuAction::Cmd(Cmd::Run)),
    ("eject", MenuAction::Cmd(Cmd::Eject)),
    ("select-all", MenuAction::Cmd(Cmd::SelectAll)),
    ("select-none", MenuAction::Cmd(Cmd::SelectNone)),
    ("invert", MenuAction::Cmd(Cmd::Invert)),
    ("toggle-log", MenuAction::Cmd(Cmd::ToggleLog)),
    ("clear-log", MenuAction::Cmd(Cmd::ClearLog)),
    ("settings", MenuAction::Cmd(Cmd::Settings)),
    ("about", MenuAction::Cmd(Cmd::About)),
    ("docs", MenuAction::Cmd(Cmd::Docs)),
    ("check-updates", MenuAction::Cmd(Cmd::CheckUpdates)),
    ("quit", MenuAction::Cmd(Cmd::Quit)),
];

/// The notification's / toast's "show the output" action. Takes the folder as
/// a string parameter, so a notification clicked after a second rip still
/// reveals the folder its own rip wrote to.
pub const REVEAL_ACTION: &str = "reveal-output";

/// Cut/Copy/Paste/Select-All-text get no hamburger row on Linux: GTK text
/// widgets carry their own context menu and standard key bindings.
pub fn is_standard_text_action(a: &MenuAction) -> bool {
    matches!(
        a,
        MenuAction::StandardCut
            | MenuAction::StandardCopy
            | MenuAction::StandardPaste
            | MenuAction::StandardSelectAllText
    )
}

/// The `gio` action name a menu row triggers; `None` for the standard text
/// commands and for the `Cmd`s that are window controls, not menu rows.
pub fn action_name_for(a: &MenuAction) -> Option<&'static str> {
    ACTIONS.iter().find(|(_, m)| m == a).map(|(n, _)| *n)
}

/// The `Cmd` whose running-rip rule gates this action. Open disc is an Open
/// for this purpose (the macOS and Windows shells map it the same way).
pub fn gating_cmd(a: &MenuAction) -> Option<Cmd> {
    match a {
        MenuAction::Cmd(c) => Some(*c),
        MenuAction::OpenDisc => Some(Cmd::Open),
        _ => None,
    }
}

/// Spell a shared `Accel` the way `gtk_accelerator_parse` reads it:
/// `<Primary>` (Ctrl on Linux), a lower-case letter even with Shift held, and
/// a keysym name for punctuation, which the parser does not take literally.
pub fn accel_string(a: &Accel) -> String {
    let mut s = String::new();
    if a.primary {
        s.push_str("<Primary>");
    }
    if a.alt {
        s.push_str("<Alt>");
    }
    if a.shift {
        s.push_str("<Shift>");
    }
    let key = match a.key {
        "," => "comma".to_string(),
        "." => "period".to_string(),
        "/" => "slash".to_string(),
        "?" => "question".to_string(),
        k if k.chars().count() == 1 => k.to_ascii_lowercase(),
        k => k.to_string(),
    };
    s.push_str(&key);
    s
}

/// One `$LC_ALL` / `$LANG` value as a BCP-47-ish tag, or `None` for the
/// "no language" values. `en_US.UTF-8` → `en-US`; `de_DE@euro` → `de-DE`.
pub fn parse_locale_env(v: &str) -> Option<String> {
    if v.is_empty() || v == "C" || v == "POSIX" || v.starts_with("C.") {
        return None;
    }
    let tag = v.split(['.', '@']).next().unwrap_or("").replace('_', "-");
    (!tag.is_empty()).then_some(tag)
}

/// Whether a dropped path is something the app can open: a directory (an
/// extracted disc tree opens as `dir://`) or a file with a source extension.
pub fn is_openable_source(path: &std::path::Path) -> bool {
    path.is_dir()
        || path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            crate::ui::SOURCE_EXTS
                .iter()
                .any(|x| x.eq_ignore_ascii_case(e))
        })
}

/// The format dropdown's rows: the core's groups flattened in order, as the
/// localized labels the dropdown shows. A `GtkDropDown` on 4.10 has no
/// separators, so this is the Windows combo's shape, not the macOS popup's.
pub fn flat_format_labels(groups: &[Vec<&'static str>]) -> Vec<String> {
    groups
        .iter()
        .flatten()
        .map(|f| crate::ui::format_label(f))
        .collect()
}

/// The `GtkStack` child name each page lives under.
pub fn page_name(p: Page) -> &'static str {
    match p {
        Page::Empty => "empty",
        Page::Titles => "titles",
        Page::Progress => "progress",
        Page::Result => "result",
    }
}

/// While ripping (and on the result page) the log takes all the height under
/// the fixed-size progress block; on the other pages the page does.
pub fn log_fills(p: Page) -> bool {
    matches!(p, Page::Progress | Page::Result)
}

/// Root rows and each row's children, from the core's parent walk — the
/// shape a `GtkTreeListModel` is built from.
pub fn tree_shape(rows: &[Row]) -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut kids = vec![Vec::new(); rows.len()];
    let mut roots = Vec::new();
    for (i, parent) in crate::ui::row_parents(rows).into_iter().enumerate() {
        match parent {
            Some(p) => kids[p].push(i),
            None => roots.push(i),
        }
    }
    (roots, kids)
}

/// Row identity, excluding tick state — same signature the other shells use
/// to tell a real tree change from a tick-only redraw.
pub fn rows_sig(rows: &[Row]) -> String {
    rows.iter()
        .map(|r| format!("{}|{}|{}|{}", r.index, r.depth, r.type_s, r.desc))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the log pane last showed: the core's sequence number of its first
/// line (`View::log_first`) and how many lines it had.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogMemo {
    pub first: u64,
    pub len: usize,
}

/// How to bring the log pane from `prev` to `now`.
#[derive(Debug, PartialEq, Eq)]
pub enum LogDelta {
    Same,
    /// Append `log[from..]`; everything before it is already on screen.
    Append(usize),
    Rewrite,
}

/// The cheapest correct redraw, keyed on the core's `log_first`: unchanged
/// means the screen is still a prefix, so only new lines are inserted; a
/// front-trim or a clear moves it and the pane is rebuilt.
pub fn log_delta(prev: &LogMemo, now: &LogMemo) -> LogDelta {
    if now.first != prev.first || now.len < prev.len {
        LogDelta::Rewrite
    } else if now.len == prev.len {
        LogDelta::Same
    } else {
        LogDelta::Append(prev.len)
    }
}

/// A catalog label shaped for a form row. The desktop catalogs end row labels
/// with " :" (a right-aligned label column); a libadwaita row title is a
/// heading, where the trailing colon reads as a typo.
pub fn row_title(label: &str) -> String {
    label
        .trim_end()
        .trim_end_matches([':', '：'])
        .trim_end_matches(|c: char| c.is_whitespace())
        .to_string()
}

/// Whether closing the window must first ask about the rip in flight. The
/// latch keeps it to ONE question per departure, as on macOS.
pub fn needs_rip_confirmation(running: bool, already_confirmed: bool) -> bool {
    running && !already_confirmed
}

/// The Settings "container" dropdown: every format a disc can produce, flat,
/// canonical first. Index-mapped, which is safe because there are no
/// separator rows (the Windows combo's rule).
pub fn container_options() -> Vec<(&'static str, String)> {
    crate::ui::output_formats(true, true)
        .into_iter()
        .flatten()
        .map(|c| (c, crate::ui::format_label(c)))
        .collect()
}

/// A settings dropdown's options: the container list above, else the core's
/// shared table.
pub fn settings_options(key: &str) -> Vec<(&'static str, String)> {
    match key {
        "container" => container_options(),
        _ => crate::ui::enum_options(key),
    }
}

/// The row a stored value selects, falling back to the first so a dropdown
/// never shows blank for a value this build does not offer.
pub fn option_index(opts: &[(&'static str, String)], stored: &str) -> u32 {
    opts.iter().position(|(c, _)| *c == stored).unwrap_or(0) as u32
}

/// The rows a language picker lists: the curated set, then any stored code
/// not on it, so a stored preference stays visible AND removable.
pub fn lang_picker_codes(stored: &str) -> Vec<String> {
    let mut codes: Vec<String> = crate::ui::PICKER_LANGUAGES
        .iter()
        .map(|(c, _)| (*c).to_string())
        .collect();
    for c in crate::ui::lang_selection(stored) {
        if !codes.iter().any(|x| x.eq_ignore_ascii_case(&c)) {
            codes.push(c);
        }
    }
    codes
}

#[cfg(test)]
mod tests {
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
    fn locale_env_values_parse_to_tags() {
        assert_eq!(parse_locale_env("en_US.UTF-8").as_deref(), Some("en-US"));
        assert_eq!(
            parse_locale_env("de_DE.UTF-8@euro").as_deref(),
            Some("de-DE")
        );
        assert_eq!(parse_locale_env("pt_BR").as_deref(), Some("pt-BR"));
        assert!(parse_locale_env("C").is_none());
        assert!(parse_locale_env("C.UTF-8").is_none());
        assert!(parse_locale_env("POSIX").is_none());
        assert!(parse_locale_env("").is_none());
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
            check: Some(Check::Off),
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
}
