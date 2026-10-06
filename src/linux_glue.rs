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

use crate::ui::{Accel, Cmd, LogKind, MenuAction, Page, Row};

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

/// Whether an action gated by `gate` is live: the same rule `App::dispatch`
/// applies first, so a greyed row is exactly a refused command. The drop
/// target and the Open Disc button ask it for `Cmd::Open`.
pub fn action_enabled(gate: Option<Cmd>, running: bool, opening: bool) -> bool {
    let Some(cmd) = gate else { return true };
    !(crate::ui::blocked_while_running(cmd) && (running || (opening && cmd != Cmd::Close)))
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

/// Whether a dropped path is something the app can open: a directory (an
/// extracted disc tree opens as `dir://`) or a file with a source extension.
pub fn is_openable_source(path: &std::path::Path) -> bool {
    path.is_dir() || path.to_str().is_some_and(crate::ui::is_openable_file)
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

/// Row identity, excluding tick state: the core's, shared by every shell.
pub use crate::ui::rows_sig;

/// An explicit Open Disc, given the drives a worker enumerated off the UI
/// thread: the log line and the URL to scan (`None` = no drive, stop). The
/// core's own plan, so this shell cannot choose differently from `App::disc_source(true)`.
pub fn disc_open_plan(drives: &[crate::engine::OpticalDrive]) -> (LogKind, String, Option<String>) {
    crate::ui::disc_open_plan(drives)
}

/// A keydb worker's outcome as a log line: a failure in the Notice style,
/// never the success one.
pub fn keydb_update_line(r: Result<String, String>) -> (LogKind, String) {
    match r {
        Ok(m) => (LogKind::Result, m),
        Err(e) => (LogKind::Notice, e),
    }
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
#[path = "linux_glue_tests.rs"]
mod tests;
