use super::*;
use crate::engine::{Row as ScanRow, Scanned};
use crate::ui::MenuAction;

// ── fixtures ──────────────────────────────────────────────────────────

/// A two-title disc scan with real stream rows, built by hand so the tests
/// need no disc, no drive and no fixture file. Title 0 has one video and
/// two audio tracks; title 1 has one video and one audio.
fn synthetic_disc() -> Scanned {
    fn row(type_s: &str, desc: &str, depth: u8, checkable: bool, title: usize) -> ScanRow {
        ScanRow {
            type_s: type_s.into(),
            item: type_s.into(),
            format: desc.into(),
            notes: String::new(),
            desc: desc.into(),
            depth,
            checkable,
            title,
            info: format!("{type_s} info"),
            pid: None,
            duration_secs: 0.0,
            lang: String::new(),
            forced: false,
            mirrors: None,
            size_bytes: None,
        }
    }
    let mut rows = vec![row("Bluray disc", "TEST_DISC", 0, false, usize::MAX)];
    for (ti, secs) in [(0usize, 5400.0f64), (1, 600.0)] {
        let mut t = row("Title", &format!("{}.  playlist", ti + 1), 1, true, ti);
        t.duration_secs = secs;
        t.size_bytes = Some(6_800_000_000);
        rows.push(t);
        rows.push(row("Video", "H.264  1080p", 2, false, ti));
        let mut a = row("Audio", "DTS-HD  eng", 2, true, ti);
        a.pid = Some(0x1100 + ti as u16);
        rows.push(a);
        if ti == 0 {
            let mut a2 = row("Audio", "AC-3  fra", 2, true, ti);
            a2.pid = Some(0x1200);
            rows.push(a2);
        }
    }
    Scanned {
        selection_model: freemkv_engine::SelectionModel::from_titles(&[5400.0, 600.0].map(
            |secs| {
                let mut title = libfreemkv::DiscTitle::empty();
                title.duration_secs = secs;
                title
            },
        )),
        label: "TEST_DISC".into(),
        volume_id: "TEST_DISC".into(),
        rows,
        key_summary: "keys: none needed".into(),
        title_count: 2,
        video_codecs: vec!["H.264".into(), "H.264".into()],
        title_sizes: Vec::new(),
        capacity_bytes: 0,
        // The identities the ticked title numbers refer to. This fixture
        // is a self-test harness, so an empty set is right: every identity
        // check is inert, which is what a synthetic disc wants.
        title_ids: Vec::new(),
        details: vec![],
        keys: None,
        needs_disc: false,
        refusal: None,
    }
}

fn view_rows() -> Vec<Row> {
    let mut app = App::new();
    app.tree = crate::ui::Tree::from_scan(
        &synthetic_disc(),
        "All titles",
        0.0,
        &crate::ui::LangPrefs::default(),
    );
    app.page = Page::Titles;
    app.view().title_rows
}

// ── menu routing ──────────────────────────────────────────────────────

#[test]
fn every_menu_id_the_shell_enables_also_routes_to_a_command() {
    // `sync_menu_enabled` walks MENU_CMD_IDS and asks `cmd_for` for the
    // rule; an id in the list that `cmd_for` does not know is a menu item
    // that is greyed on no rule at all.
    let unrouted: Vec<u16> = MENU_CMD_IDS
        .iter()
        .copied()
        .filter(|id| cmd_for(*id).is_none())
        .collect();
    assert!(
        unrouted.is_empty(),
        "MENU_CMD_IDS entries with no cmd_for mapping: {unrouted:?}"
    );
}

#[test]
fn the_menu_reaches_every_command_the_core_defines() {
    // The real contract: a command the core knows how to dispatch but no
    // menu id produces is unreachable by keyboard or menu. `SetFormat` is
    // deliberately excluded — it comes from the format combo, not a menu.
    let reachable: Vec<Cmd> = MENU_CMD_IDS.iter().filter_map(|id| cmd_for(*id)).collect();
    for want in [
        Cmd::Open,
        Cmd::Close,
        Cmd::SetOutput,
        Cmd::Run,
        Cmd::Eject,
        Cmd::Settings,
        Cmd::Quit,
        Cmd::SelectAll,
        Cmd::SelectNone,
        Cmd::Invert,
        Cmd::ToggleLog,
        Cmd::ClearLog,
        Cmd::Docs,
        Cmd::CheckUpdates,
        Cmd::About,
    ] {
        assert!(
            reachable.contains(&want),
            "{want:?} is not reachable from any menu id"
        );
    }
    // Cancel is the one command with no menu item: it lives on the
    // progress page's button, where it is always reachable mid-rip.
    assert!(
        !reachable.contains(&Cmd::Cancel),
        "Cancel gained a menu id; `blocked_while_running` never greys it, \
             so the enable pass would leave a live Cancel on a page with no run"
    );
}

#[test]
fn an_id_that_is_not_a_command_routes_nowhere() {
    // Separators and the accelerator-only ids must not fall through to a
    // command; `cmd_for`'s catch-all is what guarantees it.
    assert_eq!(cmd_for(0), None);
    assert_eq!(cmd_for(IDM_COPY), None);
    assert_eq!(cmd_for(IDM_SELECT_ALL_TEXT), None);
    assert_eq!(cmd_for(ID_TREE), None);
}

// ── tick glyphs ───────────────────────────────────────────────────────

#[test]
fn a_row_with_no_checkbox_shows_no_state_image() {
    // Index 0 is the tree control's "no state image" — the disc root and
    // the implicit Video rows must land there, not on an unchecked box the
    // user would reasonably try to tick.
    assert_eq!(state_for(None), 0);
}

#[test]
fn each_tick_state_gets_its_own_glyph() {
    // Mixed must not collapse onto checked or unchecked: the third glyph is
    // the only thing telling a user some streams under a title are off.
    let all = [
        state_for(None),
        state_for(Some(Check::Off)),
        state_for(Some(Check::On)),
        state_for(Some(Check::Mixed)),
    ];
    let mut sorted = all.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 4, "two tick states share a glyph: {all:?}");
    // And they must be indices into the image list built by
    // `build_check_images`, which installs exactly three bitmaps at the
    // 1-based indices the tree control reserves.
    for s in [
        state_for(Some(Check::Off)),
        state_for(Some(Check::On)),
        state_for(Some(Check::Mixed)),
    ] {
        assert!((1..=3).contains(&s), "state image index {s} has no bitmap");
    }
}

#[test]
fn a_mirror_row_shows_its_tick_disabled() {
    // M1b (mpg-output-design v5 §3): the MPEG-2 extension row's box is
    // "disabled and mirrors the base" — its own glyph, 4..=6, never an
    // enabled one the user would reasonably try to click.
    let row = |check: Option<Check>, check_enabled: bool| Row {
        index: 0,
        depth: 2,
        type_s: "Audio".into(),
        desc: String::new(),
        length: String::new(),
        size: String::new(),
        lang: String::new(),
        item: String::new(),
        format: String::new(),
        notes: String::new(),
        check,
        check_enabled,
    };
    let mut seen = Vec::new();
    for c in [Check::Off, Check::On, Check::Mixed] {
        assert_eq!(state_for_row(&row(Some(c), true)), state_for(Some(c)));
        let s = state_for_row(&row(Some(c), false));
        assert!((4..=6).contains(&s), "disabled index {s} has no bitmap");
        seen.push(s);
    }
    seen.dedup();
    assert_eq!(seen.len(), 3, "two disabled states share a glyph");
    assert_eq!(state_for_row(&row(None, false)), ST_NONE);
}

// ── the incremental log pane ──────────────────────────────────────────
// `render` used to re-append the whole log whenever a line arrived: O(n)
// per line, O(n^2) over a rip. Only a clear or a front trim may rebuild.
fn log_of(lines: &[&str]) -> Vec<LogLine> {
    lines
        .iter()
        .map(|t| LogLine {
            text: (*t).into(),
            kind: LogKind::Detail,
        })
        .collect()
}

#[test]
fn a_new_line_appends_only_the_tail() {
    let shown = Some(LogShown { first: 0, len: 2 });
    assert_eq!(
        log_plan(shown, 0, 4),
        LogPlan::Append(2),
        "a grown log must append only the new lines, not rebuild the pane"
    );
    assert_eq!(log_plan(shown, 0, 2), LogPlan::Keep);
    let empty = Some(LogShown { first: 7, len: 0 });
    assert_eq!(log_plan(empty, 7, 3), LogPlan::Append(0));
    assert_eq!(log_plan(empty, 7, 0), LogPlan::Keep);
}

#[test]
fn a_clear_or_trim_rebuilds_and_a_fresh_pane_rebuilds() {
    let full = Some(LogShown {
        first: 0,
        len: 5_000,
    });
    assert_eq!(log_plan(None, 0, 3), LogPlan::Rebuild, "unknown pane");
    assert_eq!(log_plan(full, 5_000, 0), LogPlan::Rebuild, "cleared");
    assert_eq!(
        log_plan(full, 5_000, 2),
        LogPlan::Rebuild,
        "cleared, refilled"
    );
    assert_eq!(
        log_plan(full, 1_000, 5_200),
        LogPlan::Rebuild,
        "front-trimmed at the cap yet longer than what was shown"
    );
}

#[test]
fn appending_the_tail_reproduces_the_full_log_text() {
    let mut log = log_of(&["a", "b", "c"]);
    log[1].kind = LogKind::Notice;
    assert_eq!(log_tail_text(&log, 0), log_text(&log));
    for k in 1..log.len() {
        let joined = format!("{}{}", log_text(&log[..k]), log_tail_text(&log, k));
        assert_eq!(joined, log_text(&log), "split at {k}");
    }
}

#[test]
fn appending_a_blank_line_keeps_its_break() {
    for lines in [&["a", "", "b"][..], &["a", ""][..]] {
        let log = log_of(lines);
        for k in 1..log.len() {
            let joined = format!("{}{}", log_text(&log[..k]), log_tail_text(&log, k));
            assert_eq!(joined, log_text(&log), "{lines:?} split at {k}");
        }
    }
}

#[test]
fn the_log_menu_item_shows_the_layouts_accelerator() {
    let text = log_menu_text("Hide log");
    assert!(text.starts_with("Hide log\t"), "{text}");
    let layout = crate::ui::menu_layout(false);
    let shown = layout
        .iter()
        .flat_map(|g| g.entries.iter())
        .find_map(|e| match e {
            crate::ui::MenuEntry::Item(mi) if mi.action == MenuAction::Cmd(Cmd::ToggleLog) => {
                mi.accel
            }
            _ => None,
        })
        .expect("ToggleLog has an accelerator");
    assert_eq!(text, crate::win_menu::item_text("Hide log", Some(&shown)));
}

// ── the redraw memo ───────────────────────────────────────────────────

#[test]
fn the_row_signature_ignores_tick_state() {
    // `render` rebuilds the tree when the signature changes, destroying
    // expansion/selection. Ticking a box must NOT change the signature — it
    // goes through `sync_tree_states` instead.
    let rows = view_rows();
    let before = rows_sig(&rows);
    let flipped: Vec<Row> = rows
        .iter()
        .cloned()
        .map(|mut r| {
            r.check = match r.check {
                Some(Check::Off) => Some(Check::On),
                Some(Check::On) => Some(Check::Mixed),
                other => other,
            };
            r
        })
        .collect();
    assert_ne!(
        rows.iter().map(|r| r.check).collect::<Vec<_>>(),
        flipped.iter().map(|r| r.check).collect::<Vec<_>>(),
        "the fixture must actually change some tick states"
    );
    assert_eq!(
        before,
        rows_sig(&flipped),
        "a tick change altered the row signature, so every toggle now \
             rebuilds the tree and loses the user's expansion state"
    );
}

#[test]
fn the_row_signature_notices_a_different_set_of_rows() {
    // The other half of the contract: if the rows really did change, the
    // signature must too, or the tree would keep showing the old disc.
    let rows = view_rows();
    let base = rows_sig(&rows);

    let mut renamed = rows.clone();
    renamed[1].desc.push_str(" (remastered)");
    assert_ne!(base, rows_sig(&renamed), "a renamed row went unnoticed");

    let mut retyped = rows.clone();
    retyped[2].type_s = "Subtitle".into();
    assert_ne!(base, rows_sig(&retyped), "a retyped row went unnoticed");

    let mut reindented = rows.clone();
    reindented[2].depth = 1;
    assert_ne!(
        base,
        rows_sig(&reindented),
        "a re-indented row went unnoticed"
    );

    let mut dropped = rows.clone();
    dropped.pop();
    assert_ne!(base, rows_sig(&dropped), "a removed row went unnoticed");

    let mut swapped = rows.clone();
    swapped.swap(2, 3);
    assert_ne!(base, rows_sig(&swapped), "a reordered tree went unnoticed");
}

// STOPGAP, NOT COVERAGE: `About` is a live `WindowModeless`, so observing
// its text after a language switch needs a window/message pump. Source
// inspection only: fails if the `relocalize()` call/method is removed.
#[test]
fn the_language_switch_re_texts_the_cached_about_box_source_inspection_only() {
    let src = include_str!("windows.rs");
    // Concatenated so these needles cannot match this test's own text.
    let call = format!(
        "{}{}",
        "self.about.relocal", "ize(&self.settings.borrow());"
    );
    assert!(
        src.contains(&call),
        "Shell::relocalize no longer re-texts the About box — it is built \
             once and cached, so nothing else ever will"
    );
    // Something only About::relocalize does: re-text the VALUE column from a
    // fresh `about_rows()`. Shell/Prefs::relocalize share the method name,
    // so the name alone would pin nothing.
    let vals = format!(
        "{}{}",
        "for (l, (_, v)) in self.lbl_vals.iter()", ".zip(rows.iter()) {"
    );
    assert!(
        src.contains(&vals),
        "About::relocalize is gone — the About box would keep the launch \
             language's labels and its old keydb status line"
    );
    let rows_fn = format!(
        "{}{}",
        "fn about_rows(st: &crate::settings::Settings) -> ", "[(String, String); 4] {"
    );
    assert!(
        src.contains(&rows_fn),
        "about_rows is gone, so the About rows can only be built once"
    );
}

// ── row cells ─────────────────────────────────────────────────────────

#[test]
fn a_row_paints_the_cores_cells_in_column_order() {
    // The tree paints each column's cell at the header's positions, so the
    // cells must come in `ui::tree_columns` order, one per column.
    let columns = crate::ui::tree_columns();
    let rows = view_rows();
    let title = rows.iter().find(|r| r.type_s == "Title").unwrap();
    let cells = row_cells(title, &columns);
    assert_eq!(cells.len(), columns.len());
    for (c, cell) in columns.iter().zip(&cells) {
        assert_eq!(cell, title.cell(c.id), "column {}", c.id);
    }
    let at = |id: &str| columns.iter().position(|c| c.id == id).unwrap();
    assert_eq!(
        (cells[at("length")].as_str(), cells[at("size")].as_str()),
        ("1:30:00", "6.8 GB")
    );
    assert_eq!(cells[at("item")], title.item);
}

#[test]
fn the_row_signature_notices_a_changed_cell() {
    let rows = view_rows();
    let base = rows_sig(&rows);
    let mut renoted = rows.clone();
    renoted[1].notes.push_str(" (play all)");
    assert_ne!(
        base,
        rows_sig(&renoted),
        "a changed Notes cell went unnoticed"
    );
    let mut relang = rows.clone();
    relang[3].lang = "deu".into();
    assert_ne!(
        base,
        rows_sig(&relang),
        "a changed Language cell went unnoticed"
    );
}

// ── the log pane ──────────────────────────────────────────────────────

#[test]
fn a_notice_is_marked_in_a_control_that_cannot_show_colour() {
    // macOS colours notices red. A Win32 EDIT cannot colour a single line,
    // so severity is carried by a gutter character instead. Losing it would
    // make a warning indistinguishable from ordinary chatter.
    let log = vec![
        LogLine {
            text: "ordinary".into(),
            kind: LogKind::Detail,
        },
        LogLine {
            text: "something went wrong".into(),
            kind: LogKind::Notice,
        },
        LogLine {
            text: "done".into(),
            kind: LogKind::Result,
        },
    ];
    let rendered = log_text(&log);
    let lines: Vec<&str> = rendered.split("\r\n").map(|s| s.trim_end()).collect();
    assert_eq!(lines.len(), 3, "one rendered line per log line");
    assert_eq!(lines[0], "ordinary", "a detail line is shown verbatim");
    assert_eq!(lines[2], "done", "a result line is shown verbatim");
    assert!(
        lines[1].starts_with("! ") && lines[1].ends_with("something went wrong"),
        "a notice must be marked and still readable: {:?}",
        lines[1]
    );
}

#[test]
fn the_log_pane_uses_crlf_so_lines_do_not_run_together() {
    // A bare LF renders as one run-on line in a Win32 EDIT control.
    let log = vec![
        LogLine {
            text: "one".into(),
            kind: LogKind::Detail,
        },
        LogLine {
            text: "two".into(),
            kind: LogKind::Detail,
        },
    ];
    assert_eq!(log_text(&log), "one\r\ntwo");
}

// ── settings dropdowns ────────────────────────────────────────────────

#[test]
fn the_shared_dropdowns_come_from_the_core() {
    // Every enum combo but "container" must be the core's table verbatim —
    // a shell-local copy is how the two shells drifted before.
    for key in [
        "selection",
        "rip_mode",
        "key_source",
        "log_level",
        "language",
    ] {
        assert_eq!(
            enum_options(key)
                .into_iter()
                .map(|(c, l)| (c.to_string(), l))
                .collect::<Vec<_>>(),
            crate::ui::enum_options(key)
                .into_iter()
                .map(|(c, l)| (c.to_string(), l))
                .collect::<Vec<_>>(),
            "{key} is not the shared table"
        );
        assert!(!enum_options(key).is_empty(), "{key} lost its options");
    }
}

#[test]
fn the_container_dropdown_stores_a_canonical_format_not_a_label() {
    // This combo is flat, so the shell maps the selected INDEX back to
    // `opts[i].0`. That value is persisted and matched by the engine, so it
    // must be the canonical English string even when the label is not.
    let opts = enum_options("container");
    let canonical: Vec<&str> = opts.iter().map(|(c, _)| *c).collect();
    let expected: Vec<&str> = crate::ui::output_formats(true, true)
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        canonical, expected,
        "the container combo no longer offers the core's format list in order"
    );
    for (canon, label) in &opts {
        assert_eq!(*label, crate::ui::format_label(canon), "label mismatch");
        assert!(
            crate::ui::format_by_title(canon, true, true).is_some(),
            "{canon:?} is not a format the core recognizes"
        );
    }
}

// ── the real window ───────────────────────────────────────────────────

// Builds the real window and runs the SAME widget sweep `FMKV_SELFTEST`
// runs, through the real message loop. One test: the window class name would collide.
#[test]
fn the_real_controls_show_what_the_core_decided() {
    let _com = w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE);
    let shell = Shell::new();
    shell.events();

    /// `(passed, description)` for each widget check, filled in from
    /// inside the message loop and read back once it has exited.
    type Report = Rc<RefCell<Option<Vec<(bool, String)>>>>;
    let results: Report = Rc::new(RefCell::new(None));

    let me = shell.clone();
    let sink = results.clone();
    shell.wnd.on().wm_timer(TIMER_HARNESS, move || {
        let _ = me.wnd.hwnd().KillTimer(TIMER_HARNESS);
        // A scan the shell has never seen, with a partial stream selection
        // so a Mixed glyph is actually on screen.
        me.app_mut(|a| {
            a.tree = crate::ui::Tree::from_scan(
                &synthetic_disc(),
                "All titles",
                0.0,
                &crate::ui::LangPrefs::default(),
            );
            a.source = "Z:\\synthetic.iso".into();
            a.page = Page::Titles;
        });
        let mixed = me
            .app
            .borrow()
            .tree
            .arena
            .iter()
            .enumerate()
            .find(|(_, n)| n.type_s == "Audio")
            .map(|(i, _)| i);
        if let Some(i) = mixed {
            me.app_mut(|a| a.tree.set_checked(i, false));
        }
        me.render();
        pump(120);
        *sink.borrow_mut() = Some(me.widget_checks());
        w::PostQuitMessage(0);
        Ok(())
    });
    // Armed from BOTH create and show: a runner that never raises WM_SHOWWINDOW
    // would leave `run_main` pumping forever (a CI hang, not a failure).
    // Re-arming the same timer id just restarts it, so firing twice is fine.
    let me = shell.clone();
    shell.wnd.on().wm_create(move |_| {
        let _ = me.wnd.hwnd().SetTimer(TIMER_HARNESS, 200, None);
        Ok(0)
    });
    let me = shell.clone();
    shell.wnd.on().wm_show_window(move |_| {
        let _ = me.wnd.hwnd().SetTimer(TIMER_HARNESS, 200, None);
        Ok(())
    });

    shell
        .wnd
        .run_main(None)
        .expect("the main window could not be created or pumped");

    let results = results.borrow_mut().take().expect(
        "the harness timer never fired — the window was never shown, so no \
             widget was ever checked",
    );
    assert!(!results.is_empty(), "the widget sweep checked nothing");
    // A Mixed glyph must actually have been exercised, or the sweep proved
    // nothing about the state a plain checkbox cannot show.
    assert!(
        results
            .iter()
            .any(|(_, m)| m.contains("Some(Mixed)") || m.contains("Mixed")),
        "no Mixed row reached the widget sweep:\n{}",
        results
            .iter()
            .map(|(_, m)| m.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let failed: Vec<&str> = results
        .iter()
        .filter(|(ok, _)| !ok)
        .map(|(_, m)| m.as_str())
        .collect();
    assert!(
        failed.is_empty(),
        "widget checks failed:\n{}",
        failed.join("\n")
    );
}

// This file's own text, CRLF folded to LF: used by the STOPGAP-NOT-COVERAGE
// source-inspection tests below (no Windows toolchain here to force a real
// SetTimer failure). CRLF fold avoids `\n`-needles missing on Windows CI.
fn own_source() -> String {
    include_str!("windows.rs").replace("\r\n", "\n")
}

#[test]
fn set_timer_failure_is_reported_source_inspection_only() {
    let src = own_source();
    // Split across concatenated literals so this needle can't match the
    // assertion's OWN text via `include_str!` of this same file — see
    // `mac.rs`'s identical guard against a self-matching tautology.
    let bare_tick = format!(
        "{}{}",
        "let _ = self.wnd.hwnd().SetTimer(TIMER_TICK", ", TICK_MS, None);"
    );
    assert!(
        !src.contains(&bare_tick),
        "Effect::StartTicking discards SetTimer's failure again with a \
             bare `let _ =` — if the rip-progress timer fails to start, \
             nothing ever observes RunState.finished and the window sits on \
             the Progress page forever with no way for the operator to \
             learn why"
    );
    let handler = format!("{}{}", "fn report_timer_", "failure");
    assert!(
        src.contains(&handler),
        "no report_timer_failure (or equivalent) handler exists to \
             surface a SetTimer failure to the operator"
    );
    // Only inside the handler's own body: the quit confirmation shows a MessageBox too.
    let start = src.find(&handler).expect("handler present");
    let body = &src[start..start + src[start..].find("\n    }\n").expect("handler ends")];
    let msgbox = format!("{}{}", "wnd.hwnd().Message", "Box(");
    assert!(
        body.contains(&msgbox),
        "the timer-failure handler no longer shows a MessageBox — a \
             silently-swallowed SetTimer failure is indistinguishable from \
             a hung rip"
    );
    let caller = format!(
        "{}{}",
        "Effect::StartTicking => {\n                    Self::report_timer_",
        "failure(&self.wnd, TIMER_TICK"
    );
    assert!(
        src.contains(&caller),
        "Effect::StartTicking no longer routes SetTimer through the failure handler"
    );
}

// One settings-save policy, not two. STOPGAP, NOT COVERAGE (same caveat as
// the timer test above): source inspection only, fails if the language-switch
// handler stops routing through the shared save helper the OK button uses.
#[test]
fn language_switch_reports_a_failed_save_source_inspection_only() {
    let src = own_source();
    // `.settings.borrow().save` `(…)` should appear in exactly ONE place:
    // the shared helper. A second occurrence means a call site re-inlined
    // its own Ok/Err match — the "one policy implemented twice" bug, again.
    let direct_save = format!("{}{}", ".settings.borrow().save", "()");
    let occurrences = src.matches(&direct_save).count();
    assert_eq!(
        occurrences, 1,
        "the direct settings-save call appears {occurrences} times \
             outside this test — it must appear exactly once, inside \
             save_settings_reporting_error, or the language-switch path (or \
             some future path) has silently grown its own copy again"
    );
    let helper = format!("{}{}", "fn save_settings_reporting", "_error");
    assert!(
        src.contains(&helper),
        "the shared save_settings_reporting_error helper is gone"
    );
    let language_call = format!(
        "{}{}",
        "me.read_form(&mut sh.settings.borrow_mut());\n                save_settings_reporting",
        "_error(&sh);"
    );
    assert!(
        src.contains(&language_call),
        "the language combo's cbn_sel_change handler no longer calls \
             save_settings_reporting_error right after committing the form \
             — a failed save on the language-switch path would go \
             unreported again"
    );
}

// Language pickers use the shared rules, not a local parser. Source
// inspection only (proving the menu ticks needs a real HWND): checks the
// set logic isn't reimplemented here — the drift `ui::lang_*` prevents.
#[test]
fn the_language_pickers_own_no_parsing_source_inspection_only() {
    let src = own_source();
    for key in ["audio_langs", "sub_langs", "forced_sub_langs"] {
        assert!(
            src.contains(&format!("r.lang(&g(\"gui.set.{key}\")")),
            "{key} is no longer built with the r.lang checklist row"
        );
        assert!(
            src.contains(&format!("langs.push((\n            \"{key}\",")),
            "{key} is missing from the `langs` registry — it would render \
                 the stored value and OK would never read it back"
        );
    }
    // Every rule must be a call INTO ui, not a copy of one.
    for f in [
        "lang_summary",
        "lang_toggle",
        "lang_is_selected",
        "PICKER_LANGUAGES",
    ] {
        assert!(
            src.contains(&format!("crate::ui::{f}")),
            "the picker no longer goes through ui::{f}"
        );
    }
    // Tells of a hand-rolled second parser: splitting on commas or joining
    // codes back up, anywhere in this file. Concatenated literals so these
    // needles can't match this test's own text via `include_str!`.
    let tells = [
        format!("{}{}", "split(", "',')"),
        format!("{}{}", "split([", "','"),
        format!("{}{}", ".join(", "\",\")"),
    ];
    for needle in &tells {
        assert!(
            !src.contains(needle),
            "windows.rs contains `{needle}` — the comma-separated language \
                 string is being parsed or rebuilt here instead of in \
                 ui::lang_selection / ui::lang_selection_to_string, which is \
                 how the two shells drift apart"
        );
    }
}

// Keyserver token is not shown in plaintext. STOPGAP, NOT COVERAGE: whether
// ES::PASSWORD masks keystrokes needs a real HWND (same gap as above).
// Source inspection only: fails if the Keys tab uses the plain `field` ctor.
#[test]
fn the_keyserver_token_field_is_secure_source_inspection_only() {
    let src = own_source();
    let secure_ctor = format!("{}{}", "fn field_", "secure");
    assert!(
        src.contains(&secure_ctor),
        "the field_secure (ES::PASSWORD) constructor is gone"
    );
    assert!(
        src.contains(&format!(
            "{}{}",
            "co::ES::AUTOHSCROLL | co::ES::PASS", "WORD"
        )),
        "field_secure no longer sets ES::PASSWORD"
    );
    let call = format!(
        "{}{}",
        "r.field_secure(&g(\"gui.set.keyserver_", "token\"), &st.keyserver_token, 320)"
    );
    assert!(
        src.contains(&call),
        "keyserver_token is no longer built with field_secure — the \
             bearer token would render in a plain edit control again, fully \
             legible during screen-sharing or a recording"
    );
}
