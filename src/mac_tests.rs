use super::*;
use crate::ui::Cmd;

// ── menu routing ──────────────────────────────────────────────────────

#[test]
fn the_menu_reaches_every_command_the_core_defines() {
    // A selector that falls off the end of `cmd_for` is a menu item with
    // NO rule, so it stays live mid-rip. `SetFormat` (format popup) and
    // `Cancel` (progress page button) are excluded — neither is a menu item.
    let sels = [
        sel!(onOpenFiles:),
        sel!(onOpenDisc:),
        sel!(onCloseDisc:),
        sel!(onBrowseOutput:),
        sel!(onRip:),
        sel!(onCancelRip:),
        sel!(onEject:),
        sel!(onSelectAll:),
        sel!(onSelectNone:),
        sel!(onInvert:),
        sel!(onClearLog:),
        sel!(onToggleLog:),
        sel!(onPrefs:),
        sel!(onAbout:),
        sel!(onDocs:),
        sel!(onCheckUpdates:),
        sel!(onQuit:),
    ];
    let reached: Vec<Cmd> = sels.iter().filter_map(|s| cmd_for(*s)).collect();
    let unrouted: Vec<&Sel> = sels.iter().filter(|s| cmd_for(**s).is_none()).collect();
    assert!(
        unrouted.is_empty(),
        "menu selectors with no cmd_for arm — they would never be greyed \
             during a rip: {unrouted:?}"
    );
    for want in [
        Cmd::Open,
        Cmd::Close,
        Cmd::SetOutput,
        Cmd::Run,
        Cmd::Cancel,
        Cmd::Eject,
        Cmd::SelectAll,
        Cmd::SelectNone,
        Cmd::Invert,
        Cmd::ClearLog,
        Cmd::ToggleLog,
        Cmd::Settings,
        Cmd::About,
        Cmd::Docs,
        Cmd::CheckUpdates,
        Cmd::Quit,
    ] {
        assert!(
            reached.contains(&want),
            "{want:?} is not reachable from any menu selector"
        );
    }
}

#[test]
fn opening_a_disc_is_an_open_for_enablement_purposes() {
    // Two menu items, one rule: File ▸ Open… and File ▸ Open Disc… must
    // both be blocked mid-rip. Mapping Open Disc to anything else (or to
    // nothing) would leave a live "Open Disc" during a rip.
    assert_eq!(cmd_for(sel!(onOpenFiles:)), Some(Cmd::Open));
    assert_eq!(cmd_for(sel!(onOpenDisc:)), Some(Cmd::Open));
    assert!(crate::ui::blocked_while_running(Cmd::Open));
}

#[test]
fn a_selector_that_is_not_a_command_routes_nowhere() {
    // The catch-all must not fall through: the checkbox and popup actions
    // are not menu commands and must not be treated as ones.
    assert_eq!(cmd_for(sel!(onToggle:)), None);
    assert_eq!(cmd_for(sel!(onPickFormat:)), None);
    assert_eq!(cmd_for(sel!(onPickLanguage:)), None);
}

// The drag-and-drop overlay must not be leaked once per language switch.
#[test]
fn the_drop_overlay_is_not_leaked_on_every_language_switch() {
    let src = include_str!("mac.rs");
    let src = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
    let at = src
        .find("fn install_drop_view(")
        .expect("install_drop_view moved — this test cannot see it");
    let body = &src[at..];
    let body = &body[..body.find("\nfn ").unwrap_or(body.len())];

    let leak = format!("{}{}", "std::mem::", "forget(drop)");
    assert!(
        !body.contains(&leak),
        "install_drop_view runs again on every language switch; holding \
             its retain back leaks one whole DropView each time"
    );
}

// A widget list `build_ui` PUSHES into must be emptied by `build_ui`.
#[test]
fn every_widget_list_build_ui_pushes_into_is_cleared_there_first() {
    let src = include_str!("mac.rs");
    let src = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
    let start = src
        .find("fn build_ui(")
        .expect("build_ui moved — this test cannot see it");
    let body = &src[start..];
    let end = body.find("\nfn ").unwrap_or(body.len());
    let body = &body[..end];

    // The ivars that are LISTS of widgets, read off their declared type so
    // a second one added later is covered without editing this test.
    let ty = format!("{}{}", ": RefCell<Vec<Ret", "ained<");
    let lists: Vec<&str> = src
        .match_indices(&ty)
        .filter_map(|(at, _)| src[..at].rsplit('\n').next().map(str::trim))
        .collect();
    assert!(
        !lists.is_empty(),
        "no widget-list ivar found — has Ivars changed shape?"
    );

    for name in lists {
        if !body.contains(name) {
            continue; // built somewhere else (the Settings form's lists)
        }
        // A rebuild must REPLACE the list, not grow it. Assigning the whole
        // vector does that by itself; pushing into it does not, and needs
        // the clear.
        let assigned = body.contains(&format!("{}{}", name, ".borrow_mut() = "));
        let cleared = body.contains(&format!("{}{}", name, ".borrow_mut().clear()"));
        assert!(
            assigned || cleared,
            "`{name}` is pushed into by build_ui and neither assigned nor \
                 cleared there: a second build_ui (a language switch) stacks \
                 its widgets on top of the last one's, forever"
        );
    }
}

// Every action this shell defines must be reachable from the UI.
#[test]
fn every_action_selector_this_shell_defines_is_wired_to_something() {
    let src = include_str!("mac.rs");
    // Production only: a selector named solely by a test is not wired to
    // anything a user can reach, and the tests' own assembled needles
    // are not declarations.
    let src = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
    let decl = format!("{}{}", "#[unsafe(me", "thod(on");

    let mut orphans = Vec::new();
    for (at, _) in src.match_indices(&decl) {
        let rest = &src[at + decl.len()..];
        let end = rest
            .find(':')
            .expect("an action selector always ends at its colon");
        let name = &rest[..end];
        // Built at run time for the same reason as `decl`.
        let target = format!("{}{}{}", "sel!(on", name, ":)");
        if !src.contains(&target) {
            orphans.push(format!("on{name}:"));
        }
    }

    assert!(
        orphans.is_empty(),
        "these handlers are defined and targeted by nothing — no menu \
             item, no control, no timer can reach them: {orphans:?}"
    );
}

// The format popup's rebuild guard has to compare like with like.
#[test]
fn the_format_popup_comparison_counts_the_separators_appkit_reports() {
    let titles = vec!["Selected titles → MKV", "Selected titles → M2TS"];
    let meta = vec!["Chapters → file"];

    let got = popup_item_titles(&[titles.clone(), meta.clone()]);
    assert_eq!(
        got,
        vec![
            crate::ui::format_label("Selected titles → MKV"),
            crate::ui::format_label("Selected titles → M2TS"),
            String::new(),
            crate::ui::format_label("Chapters → file"),
        ],
        "the group boundary is an item in the menu — leaving it out makes \
             the guard compare a 3-item list against AppKit's 4 and never match"
    );

    // One group, no boundary, nothing extra.
    assert_eq!(popup_item_titles(std::slice::from_ref(&meta)).len(), 1);

    // And the shape the real popup is built from: every group boundary
    // accounted for, so the count matches what the menu will hold.
    let real = crate::ui::output_formats(true, true);
    assert_eq!(
        popup_item_titles(&real).len(),
        real.iter().map(Vec::len).sum::<usize>() + real.len() - 1,
        "one separator per boundary between the groups"
    );
}

// ── the log pane ──────────────────────────────────────────────────────

#[test]
fn a_notice_gets_its_own_colour_bucket() {
    // Colour is the ONLY thing marking a problem in this shell's log (the
    // Windows shell uses a gutter character instead), so a Notice sharing
    // a bucket with an ordinary line makes warnings invisible.
    let notice = log_colour(crate::ui::LogKind::Notice);
    let detail = log_colour(crate::ui::LogKind::Detail);
    let result = log_colour(crate::ui::LogKind::Result);
    assert_ne!(notice, detail, "a notice reads as an ordinary detail line");
    assert_ne!(notice, result, "a notice reads as an ordinary result line");
    assert_ne!(detail, result, "detail and result share a colour");
    // `log_append` only has three colours; anything else falls into its
    // catch-all and silently renders as a result line.
    for k in [
        crate::ui::LogKind::Notice,
        crate::ui::LogKind::Detail,
        crate::ui::LogKind::Result,
    ] {
        assert!(log_colour(k) <= 2, "no colour defined for {k:?}");
    }
}

// ── settings dropdowns ────────────────────────────────────────────────

#[test]
fn the_format_popup_is_not_an_index_mapped_enum() {
    // This popup interleaves group SEPARATOR rows, so its index does not
    // line up with the core's flat format list — `read_prefs_form` maps
    // it back by TITLE. A "container" arm here would silently break that.
    assert!(
        enum_options("container").is_empty(),
        "the container popup must not be index-mapped: this shell's popup \
             carries separator rows, so index N is not option N"
    );
    // And the title-based path must actually resolve: every canonical
    // format's localized label round-trips back to the canonical string.
    for canon in crate::ui::output_formats(true, true).into_iter().flatten() {
        let label = crate::ui::format_label(canon);
        assert_eq!(
            crate::ui::format_from_label(&label, true, true),
            Some(canon),
            "{label:?} does not resolve back to {canon:?}"
        );
    }
}

#[test]
fn the_shared_dropdowns_come_from_the_core() {
    // A shell-local copy of this table is how the two shells drifted
    // before, so this shell must hold none.
    for key in [
        "selection",
        "rip_mode",
        "key_source",
        "log_level",
        "language",
    ] {
        let opts = enum_options(key);
        assert!(!opts.is_empty(), "{key} lost its options");
        assert_eq!(
            opts.into_iter()
                .map(|(c, l)| (c.to_string(), l))
                .collect::<Vec<_>>(),
            crate::ui::enum_options(key)
                .into_iter()
                .map(|(c, l)| (c.to_string(), l))
                .collect::<Vec<_>>(),
            "{key} is not the shared table"
        );
    }
}

#[test]
fn a_free_form_setting_is_not_an_enum_popup() {
    // `read_prefs_form` uses an empty result to mean "not an enum": a
    // spurious arm here would make a text field persist an index.
    for key in ["dest_dir", "filename_template", "max_passes", ""] {
        assert!(enum_options(key).is_empty(), "{key} became an enum popup");
    }
}

// ── output field wiring ─────────────────────────────────────────────────
// STOPGAP, NOT COVERAGE: the real bug needs a live NSComboBox in a real
// run loop (`windows.rs` has that harness). Source inspection only.
#[test]
fn the_output_field_has_a_delegate_wired_source_inspection_only() {
    let src = include_str!("mac.rs");
    // Built by concatenation so this needle cannot match the assertion's
    // OWN source via `include_str!` — a self-matching source-inspection
    // test is the tautology this crate already shipped once.
    let wired = format!("{}{}", "fld.set", "Delegate(");
    assert!(
        src.contains(&wired),
        "out_field (`fld`) is never given a delegate — typed edits have \
             nothing to tell the model, so render()'s next tick overwrites \
             them with the stale output_dir"
    );
    let handler = format!("{}{}", "fn control_text_did", "_change");
    assert!(
        src.contains(&handler),
        "no controlTextDidChange: handler exists to push the typed path \
             into App::output_dir"
    );
    let picked = format!("{}{}", "fn combo_box_selection_did", "_change");
    assert!(
        src.contains(&picked),
        "no comboBoxSelectionDidChange: handler exists to push a dropdown \
             pick into App::output_dir"
    );
}

// ── one settings-save policy, not two ───────────────────────────────────
// STOPGAP, NOT COVERAGE: needs a live `Controller`, no harness for that
// exists. Inspects source text for a re-inlined Ok/Err match.
#[test]
fn language_switch_reports_a_failed_save_source_inspection_only() {
    let src = include_str!("mac.rs");
    // `.settings.borrow().save` (…) should appear in exactly ONE place: the
    // shared helper. A second occurrence means someone re-inlined the
    // Ok/Err match at a second call site — "one policy implemented twice".
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
    let commit = fn_body(prod_src(), "fn commit_prefs(");
    assert!(
        commit.contains("self.save_settings_reporting_error();"),
        "commit_prefs no longer saves through save_settings_reporting_error \
             — a failed save from Settings would go unreported again"
    );
    let language = fn_body(prod_src(), "fn on_apply_language(");
    assert!(
        language.contains("self.commit_prefs();"),
        "onApplyLanguage: no longer commits the form through commit_prefs \
             before switching — edits would be lost or the save unreported"
    );
}

// ── Settings text fields commit like the Linux shell's ───────────────────
// Source inspection only: the Enter/blur wiring needs a real window. Every
// route that writes the form goes through the one `commit_prefs`.
#[test]
fn a_settings_field_commits_on_enter_and_on_leaving_it_source_inspection_only() {
    let src = prod_src();
    for sig in [
        "fn on_close_prefs(",
        "fn control_text_did_end_editing(",
        "fn control_text_view_do_command(",
    ] {
        assert!(
            fn_body(src, sig).contains("self.commit_prefs();"),
            "{sig} must commit through commit_prefs"
        );
    }
    assert!(src.contains("#[unsafe(method(controlTextDidEndEditing:))]"));
    assert!(src.contains("#[unsafe(method(control:textView:doCommandBySelector:))]"));
    // Enter claims the newline, or the default OK button closes the window.
    let enter = fn_body(src, "fn control_text_view_do_command(");
    assert!(enter.contains("sel!(insertNewline:)") && enter.contains("Bool::YES"));
    // Focus leaving a closing window is not an edit.
    assert!(fn_body(src, "fn control_text_did_end_editing(").contains("isVisible()"));
    // Every field reports to the controller.
    assert!(fn_body(src, "fn build_prefs(").contains("f.setDelegate("));
    // Cancel stays a discard: it never reads the form.
    assert!(!fn_body(src, "fn on_cancel_prefs(").contains("commit_prefs"));
}

#[test]
fn a_commit_writes_only_what_changed_and_follows_only_a_new_default_folder() {
    let before = crate::settings::Settings::default();
    assert_eq!(
        prefs_commit(&before, &before.clone()),
        PrefsCommit::Unchanged
    );

    let mut other = before.clone();
    other.filename_template = "{title}".into();
    assert_eq!(
        prefs_commit(&before, &other),
        PrefsCommit::Save { new_dest: None },
        "an edit elsewhere must not re-point a one-off output folder"
    );

    let mut moved = before.clone();
    moved.dest_dir = "/Volumes/Media/Rips".into();
    assert_eq!(
        prefs_commit(&before, &moved),
        PrefsCommit::Save {
            new_dest: Some("/Volumes/Media/Rips".into())
        }
    );

    let mut blank = before.clone();
    blank.dest_dir = "  ".into();
    assert_eq!(
        prefs_commit(&before, &blank),
        PrefsCommit::Save { new_dest: None },
        "a cleared default saves but leaves the active folder alone"
    );
}

#[test]
fn a_settings_window_left_by_cancel_is_rebuilt_on_reopen_source_inspection_only() {
    let body = fn_body(prod_src(), "fn perform(&self, effects");
    let at = body
        .find("E::ShowSettings =>")
        .expect("ShowSettings arm moved");
    let arm = &body[at..at + 600.min(body.len() - at)];
    assert!(
        arm.contains(".filter(|w| w.isVisible())"),
        "a hidden Settings window must be rebuilt, or Cancel's discarded \
             edits reappear and the next OK commits them"
    );
}

// Source inspection only: page geometry needs a real window.
#[test]
fn a_one_bar_progress_page_drops_the_second_bar_band_not_its_top_source_inspection_only() {
    let body = fn_body(prod_src(), "fn relayout(");
    assert!(body.contains("v.setBoundsOrigin(NSPoint::new(0.0, band));"));
    assert!(body.contains("CANCEL_Y + band"));
    assert!(fn_body(prod_src(), "fn build_ui(").contains("cancel_btn.borrow_mut() = Some(cancel)"));
}

// Source inspection only: column geometry needs a real window.
#[test]
fn the_size_column_stays_in_view_source_inspection_only() {
    let ui = fn_body(prod_src(), "fn build_ui(");
    assert!(
        ui.contains("ov.setAutoresizesOutlineColumn(false);"),
        "the tick column grows on expand and pushes Size out of the tree"
    );
    assert!(
        fn_body(prod_src(), "fn relayout(").contains("ov.sizeToFit()"),
        "the columns must refit when the tree changes width"
    );
}

#[test]
fn wrapped_rows_size_to_their_text_and_push_down_only_what_is_below() {
    assert_eq!(
        fitted_height(15.2, 18.0),
        18.0,
        "one line keeps the row height"
    );
    assert_eq!(fitted_height(44.3, 22.0), 45.0);
    assert!(sits_below(100.0, 100.0));
    assert!(sits_below(80.0, 100.0));
    assert!(
        !sits_below(120.0, 100.0),
        "the field's own label and browse button stay"
    );
}

#[test]
fn a_popup_widens_to_its_longest_choice_within_the_row() {
    assert_eq!(
        popup_width(220.0, 180.0, 330.0),
        220.0,
        "never narrower than laid out"
    );
    assert_eq!(popup_width(220.0, 290.0, 330.0), 290.0);
    assert_eq!(popup_width(220.0, 400.0, 330.0), 330.0, "capped at the row");
    assert_eq!(
        popup_width(300.0, 400.0, 250.0),
        300.0,
        "a cramped row keeps its width"
    );
}

#[test]
fn long_paths_and_dropdowns_are_not_cut_off_source_inspection_only() {
    let src = prod_src();
    let path = fn_body(src, "    fn path(");
    assert!(path.contains("setWraps(true)") && path.contains("setUsesSingleLineMode(false)"));
    let label = fn_body(src, "    fn label(");
    assert!(label.contains("setWraps(true)") && label.contains("self.extra"));
    for sig in ["    fn combo(", "    fn langs(", "    fn popup("] {
        assert!(
            fn_body(src, sig).contains("fit_popup("),
            "{sig} must fit its popup"
        );
    }
    assert!(fn_body(src, "fn build_prefs(").contains("fit_wrapping_field(f)"));
    assert!(fn_body(src, "fn set_pref_field(").contains("fit_wrapping_field(f)"));
    let rows = &src[src.find("impl Rows {").unwrap()..src.find("fn build_prefs(").unwrap()];
    assert_eq!(
        rows.matches("self.y -= ").count(),
        1,
        "a row must advance through `advance`, or a wrapped label overlaps the next row"
    );
}

// ── the keyserver token is not shown in plaintext ───────────────────────
// STOPGAP, NOT COVERAGE: masking needs a real rendered window. Source
// inspection only: fails if the Keys tab uses the plain `field` ctor.
#[test]
fn the_keyserver_token_field_is_secure_source_inspection_only() {
    let src = include_str!("mac.rs");
    let secure_ctor = format!("{}{}", "fn field_", "secure");
    assert!(
        src.contains(&secure_ctor),
        "the field_secure (NSSecureTextField) constructor is gone"
    );
    let call = format!(
        "{}{}",
        "t.field_secure(\n        mtm,\n        \"keyserver_", "token\","
    );
    assert!(
        src.contains(&call),
        "keyserver_token is no longer built with field_secure — the \
             bearer token would render in a plain NSTextField again, fully \
             legible during screen-sharing or a recording"
    );
}

// ── the incremental log pane ──────────────────────────────────────────
// `render` used to re-append the whole log whenever a line arrived: O(n)
// per line, O(n^2) over a rip. Only a clear or a front trim may rebuild.
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
fn a_clear_rebuilds_a_fresh_pane_rebuilds_and_a_trim_drops_the_head() {
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
        LogPlan::Trim {
            drop: 1_000,
            from: 4_000
        },
        "front-trimmed at the cap: drop the head, append the tail"
    );
}

// ── the tree redraw memo ── `render` used to rebuild the outline on
// every 5 Hz tick even when rows never moved. Windows already had this
// guard (`rows_sig`); real coverage since `rows_sig` is a pure function.
fn rows_sig_fixture() -> Vec<crate::ui::Row> {
    vec![
        crate::ui::Row {
            index: 0,
            depth: 0,
            type_s: String::new(),
            desc: "Disc".into(),
            length: String::new(),
            size: String::new(),
            lang: String::new(),
            item: String::new(),
            format: String::new(),
            notes: String::new(),
            check: None,
            check_enabled: false,
        },
        crate::ui::Row {
            index: 1,
            depth: 1,
            type_s: "Title".into(),
            desc: "Main Feature".into(),
            length: "1:30:00".into(),
            size: "6.8 GB".into(),
            lang: String::new(),
            item: String::new(),
            format: String::new(),
            notes: String::new(),
            check: Some(crate::ui::Check::Off),
            check_enabled: true,
        },
        crate::ui::Row {
            index: 2,
            depth: 2,
            type_s: "Audio".into(),
            desc: "English 5.1".into(),
            length: String::new(),
            size: String::new(),
            lang: String::new(),
            item: String::new(),
            format: String::new(),
            notes: String::new(),
            check: Some(crate::ui::Check::On),
            check_enabled: true,
        },
    ]
}

#[test]
fn the_row_signature_is_stable_for_unchanged_rows() {
    let rows = rows_sig_fixture();
    assert_eq!(
        rows_sig(&rows),
        rows_sig(&rows.clone()),
        "an identical row list must produce an identical signature, or \
             every 200 ms progress tick forces a full outline reload again"
    );
}

#[test]
fn the_row_signature_notices_a_real_change() {
    let rows = rows_sig_fixture();
    let base = rows_sig(&rows);

    let mut renamed = rows.clone();
    renamed[1].desc.push_str(" (remastered)");
    assert_ne!(base, rows_sig(&renamed), "a renamed row went unnoticed");

    let mut retyped = rows.clone();
    retyped[2].type_s = "Subtitle".into();
    assert_ne!(base, rows_sig(&retyped), "a retyped row went unnoticed");

    let mut relengthed = rows.clone();
    relengthed[1].length = "1:29:59".into();
    assert_ne!(base, rows_sig(&relengthed), "a new Length went unnoticed");

    let mut resized = rows.clone();
    resized[1].size = "6.9 GB".into();
    assert_ne!(base, rows_sig(&resized), "a new Size went unnoticed");

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
    swapped.swap(1, 2);
    assert_ne!(base, rows_sig(&swapped), "a reordered tree went unnoticed");
}

#[test]
fn the_row_signature_ignores_tick_state() {
    // A rebuild throws the outline back to the top of the list, so ticking
    // a box must NOT change the signature — it goes down the
    // `sync_check_states` path instead, which repaints ticks in place.
    let rows = rows_sig_fixture();
    let before = rows_sig(&rows);
    let flipped: Vec<crate::ui::Row> = rows
        .iter()
        .cloned()
        .map(|mut r| {
            r.check = match r.check {
                Some(crate::ui::Check::Off) => Some(crate::ui::Check::On),
                Some(crate::ui::Check::On) => Some(crate::ui::Check::Mixed),
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
        "a tick change altered the row signature, so every toggle forces a \
             full reloadData + scrollPoint and jumps the list back to the top"
    );
}

// STOPGAP, NOT COVERAGE: needs a live `Controller` with a real
// `NSOutlineView` to observe call counts. Source inspection only: fails
// if the guard is removed and `render` calls `apply` unconditionally.
#[test]
fn render_gates_the_tree_rebuild_on_the_row_signature_source_inspection_only() {
    let src = include_str!("mac.rs");
    let guard = format!(
        "{}{}",
        "let sig = Some(rows_sig(&v.title_rows));\n            if iv.tree_", "sig.get() != sig {"
    );
    assert!(
        src.contains(&guard),
        "render() no longer compares the tree's row signature before \
             calling TitlesSource::apply — a running rip's 5 Hz tick would \
             force a full outline reloadData + re-expand every 200 ms again"
    );
    // …and the other half: an unchanged signature must still repaint the
    // ticks, or a checkbox click would change nothing on screen at all.
    let in_place = format!(
        "{}{}",
        "} else {\n                src.sync_check_", "states("
    );
    assert!(
        src.contains(&in_place),
        "render() has no in-place tick refresh for the unchanged-signature \
             case — with tick state out of the signature, a click would leave \
             the outline showing the old ticks"
    );
}

// STOPGAP, NOT COVERAGE: `relocalize` rebuilds a live `NSWindow`, which
// this crate cannot stand up outside a real AppKit run loop. Source
// inspection only: fails if the memo reset is removed.
#[test]
fn a_language_switch_forgets_the_tree_memo_source_inspection_only() {
    let src = include_str!("mac.rs");
    let reset = format!("{}{}", "self.ivars().tree_sig", ".set(None);");
    assert!(
        src.contains(&reset),
        "relocalize() no longer clears tree_sig — build_ui installs a BRAND \
             NEW, empty TitlesSource, so the very next render() compares the \
             new rows against the OLD signature, matches, and never calls \
             apply: the titles tree comes back empty after a language change"
    );
}

// ── quitting goes through the same guard as closing ───────────────────
// The alert itself needs a real `NSAlert` on a run loop, but the two
// DECISIONS around it are pure functions and are tested for real here.

#[test]
fn a_quit_asks_exactly_once_and_only_while_a_rip_runs() {
    // Nothing running: never ask, whatever the latch says.
    assert!(!needs_rip_confirmation(false, false));
    assert!(!needs_rip_confirmation(false, true));
    // Running and nobody has answered yet: ask.
    assert!(needs_rip_confirmation(true, false));
    // Already chose "Stop & Quit" on the way out: do NOT ask again.
    // `Cmd::Cancel` only signals the worker, so `running()` stays true
    // when the last-window-closed termination reaches the delegate.
    assert!(!needs_rip_confirmation(true, true));
}

#[test]
fn only_the_first_alert_button_stops_the_rip_and_quits() {
    // NSAlertFirstButtonReturn is the "Stop & Quit" button — the one added
    // first at both call sites.
    assert_eq!(
        quit_choice(objc2_app_kit::NSAlertFirstButtonReturn),
        QuitChoice::StopThenProceed
    );
    // Second button is "Keep ripping".
    assert_eq!(
        quit_choice(objc2_app_kit::NSAlertSecondButtonReturn),
        QuitChoice::Stay
    );
    // And anything else at all — a dismissed panel, a third button someone
    // adds later — must also keep the rip. A quit that throws away hours of
    // ripping must never be the answer to a question nobody answered.
    for r in [0isize, -1, 1, 42, objc2_app_kit::NSAlertThirdButtonReturn] {
        if r == objc2_app_kit::NSAlertFirstButtonReturn {
            continue;
        }
        assert_eq!(quit_choice(r), QuitChoice::Stay, "response {r}");
    }
}

// The AppKit language pickers own no parsing of their own.
#[test]
fn the_language_pickers_own_no_parsing_source_inspection_only() {
    let src = include_str!("mac.rs");
    // Every rule must be a call INTO ui, not a copy of one.
    for f in [
        "lang_toggle",
        "lang_summary",
        "lang_is_selected",
        "lang_selection",
        "PICKER_LANGUAGES",
    ] {
        let needle = format!("{}{}", "crate::ui::", f);
        assert!(
            src.contains(&needle),
            "the picker no longer goes through ui::{f} — the shells are \
                 free to disagree about what a language selection means again"
        );
    }
    // The menu action has to be wired, or none of the above ever runs.
    let action = format!("{}{}", "#[unsafe(method(onToggle", "Lang:))]");
    assert!(
        src.contains(&action),
        "the language menu item's selector is gone; the pickers would \
             render a value nothing can change"
    );
    // The tells of a hand-rolled second parser: splitting/joining the
    // stored comma string anywhere in this file. Built from concatenated
    // literals so these needles can't match this test's own text.
    for needle in [
        format!("{}{}", "split(", "','"),
        format!("{}{}", "split([", "','"),
        format!("{}{}", ".join(", "\",\")"),
    ] {
        assert!(
            !src.contains(&needle),
            "mac.rs contains `{needle}` — the comma-separated language \
                 string is being parsed or rebuilt here instead of in \
                 ui::lang_selection / ui::lang_selection_to_string"
        );
    }
}

// "Stop & Quit" has to STOP before it quits.
#[test]
fn stop_and_quit_waits_for_the_worker_before_letting_the_process_go() {
    let src = include_str!("mac.rs");
    // The QUIT path specifically — the Stop button signals the same way and
    // is not what this is about, so the slice is taken from `confirm_quit`.
    let helper = format!("{}{}", "fn confirm_", "quit(&self) -> QuitChoice {");
    let start = src.find(&helper).expect("the shared confirm_quit is gone");
    let end = start
        + src[start..]
            .find("\n    // Save `Settings` to disk")
            .expect("the next item still ends confirm_quit");
    let body = &src[start..end];
    let cancel = format!("{}{}", "self.act(crate::ui::Cmd::", "Cancel);");
    assert!(
        body.contains(&cancel),
        "confirm_quit no longer signals the worker at all"
    );
    let wait = format!("{}{}", "await_worker_", "exit(");
    assert!(
        body.contains(&wait),
        "the cancel is fire-and-forget: nothing waits for the worker to \
             put its output down before AppKit tears the process out from \
             under it"
    );
}

// STOPGAP, NOT COVERAGE: whether AppKit calls back on ⌘Q needs a live
// `NSApplication`, which this crate cannot stand up in a unit test.
// Source inspection only; fails if any of the three delegate pieces is removed.
#[test]
fn the_app_has_a_delegate_that_gates_quit_and_ends_the_process_source_inspection_only() {
    let src = include_str!("mac.rs");
    // Built by concatenation so these needles cannot match this test's own
    // text through `include_str!`.
    let proto = format!(
        "{}{}",
        "unsafe impl NSApplication", "Delegate for Controller {"
    );
    assert!(
        src.contains(&proto),
        "Controller is not an NSApplicationDelegate — ⌘Q would bypass the \
             rip-in-progress confirmation the close button implements, and \
             closing the last window would leave a headless process running \
             its 5 Hz timer and its rip thread"
    );
    // The SELECTOR, not just the Rust fn name: AppKit dispatches on the
    // Objective-C selector, so a typo there is a silently dead delegate
    // method that still compiles and still reads correctly in Rust.
    let gate = format!(
        "{}{}",
        "#[unsafe(method(applicationShouldTerminate:))]\n        fn should_",
        "terminate(&self, _app: &NSApplication)"
    );
    assert!(
        src.contains(&gate),
        "applicationShouldTerminate: is gone — ⌘Q and File ▸ Quit would \
             terminate straight through a running rip with no confirmation"
    );
    let last_window = format!(
        "{}{}",
        "fn terminate_after_last_",
        "window(&self, _app: &NSApplication) -> bool {\n            true"
    );
    assert!(
        src.contains(&last_window),
        "applicationShouldTerminateAfterLastWindowClosed: no longer \
             returns true — closing the window would leave the process alive \
             with no UI, still ticking and still ripping"
    );
    let wired = format!(
        "{}{}",
        "app.set", "Delegate(Some(objc2::runtime::ProtocolObject::from_ref(&*c)));"
    );
    assert!(
        src.contains(&wired),
        "run() never makes the Controller the NSApplication delegate, so \
             none of the above is ever called"
    );
    // Both routes out must go through ONE confirmation, not two copies of
    // it: a second inlined NSAlert is how ⌘Q and the close button drifted
    // apart in the first place.
    let helper = format!("{}{}", "fn confirm_", "quit(&self) -> QuitChoice {");
    assert!(
        src.contains(&helper),
        "the shared confirm_quit helper is gone"
    );
    let alerts = src
        .matches(&format!("{}{}", "NSAlert::", "new(mtm)"))
        .count();
    assert_eq!(
        alerts, 1,
        "there are {alerts} NSAlert construction sites in this shell; the \
             rip-in-progress question must be asked in exactly one place, or \
             the close path and the quit path can drift apart again"
    );
}

// ── one keydb download at a time ── STOPGAP, NOT COVERAGE: needs a live
// Controller/Settings window to click. Fails if the guard reverts to
// just the button's enabled state, which a rebuild resets to enabled.
#[test]
fn a_second_keydb_download_is_refused_by_state_not_by_a_button_source_inspection_only() {
    let src = include_str!("mac.rs");
    let flag = format!("{}{}", "if self.ivars().keydb_updating", ".get() {");
    assert!(
        src.contains(&flag),
        "onUpdateKeys: no longer checks a controller-held in-flight flag — \
             reopening Settings mid-download hands back an enabled button and \
             a second click spawns a second writer of the same keydb file"
    );
    let restore = format!(
        "{}{}",
        "c.set_keydb_updating(c.ivars().keydb_updating", ".get());"
    );
    assert!(
        src.contains(&restore),
        "build_prefs no longer restores the in-flight state onto the \
             freshly built button, so a running download looks idle"
    );
}

// ── the drain timer stops once drained ── nothing invalidated it, so it
// fired forever after the first keydb update; Windows' `drain()` already
// calls `KillTimer` at the same point. Source inspection only.
#[test]
fn the_drain_timer_stops_itself_once_drained_source_inspection_only() {
    let src = include_str!("mac.rs");
    let stop = format!(
        "{}{}",
        "if let Some(t) = self.ivars().drain.borrow_mut().take",
        "() {\n                t.invalidate();"
    );
    assert!(
        src.contains(&stop),
        "onDrain: no longer invalidates and clears the drain timer once \
             messages are processed — it would go back to polling an always- \
             empty inbox at 5 Hz forever after the first keydb update"
    );
}

// ── codeaudit mac cluster ─────────────────────────────────────────────

fn prod_src() -> &'static str {
    let src = include_str!("mac.rs");
    &src[..src.find("#[cfg(test)]").unwrap_or(src.len())]
}

fn fn_body<'a>(src: &'a str, sig: &str) -> &'a str {
    let at = src
        .find(sig)
        .unwrap_or_else(|| panic!("{sig} moved — this test cannot see it"));
    let indent = at - src[..at].rfind('\n').map_or(0, |n| n + 1);
    let close = format!("\n{}}}", " ".repeat(indent));
    let body = &src[at..];
    &body[..body.find(&close).map_or(body.len(), |e| e + close.len())]
}

// M1: `-[NSApplication activate]` is macOS 14+; the plist promises 11.0.
#[test]
fn activation_falls_back_below_macos_14_source_inspection_only() {
    let src = prod_src();
    let body = fn_body(src, "fn activate_app(");
    let bare = format!("{}{}", ".activ", "ate();");
    assert_eq!(
        src.matches(&bare).count(),
        body.matches(&bare).count(),
        "-[NSApplication activate] called outside activate_app: it is macOS \
             14+ only, so macOS 11-13 (LSMinimumSystemVersion 11.0) abort at launch"
    );
    assert!(fn_body(src, "pub fn run(").contains("activate_app(&app);"));
    assert!(body.contains("respondsToSelector(sel!(activate))"));
    assert!(body.contains("activateIgnoringOtherApps(true)"));
    let plist = include_str!("../macos/Info.plist");
    assert!(plist.contains("<key>LSMinimumSystemVersion</key><string>11.0</string>"));
}

// M3: a denied or failed notification must leave a trace.
#[test]
fn notification_failures_are_logged_source_inspection_only() {
    let body = fn_body(prod_src(), "fn notify_rip_finished(");
    let silent = format!("{}{}", "withCompletionHandler(&req, ", "None)");
    assert!(
        !body.contains(&silent),
        "addNotificationRequest error dropped"
    );
    assert!(
        body.matches("tracing::warn!").count() >= 2,
        "authorization denial/error and post error must both be logged"
    );
}

// M4 + M5: a failed update or a panic is a Notice; the payload never shows.
#[test]
fn a_failed_keydb_update_is_a_notice_and_never_shows_the_panic_payload() {
    use crate::ui::LogKind;
    let ok = keydb_outcome(Ok(Ok("keydb updated — 3 entries".into())));
    assert_eq!(ok, (LogKind::Result, "keydb updated — 3 entries".into()));
    let err = keydb_outcome(Ok(Err("keydb download failed: 404".into())));
    assert_eq!(err.0, LogKind::Notice, "an Err is styled like a success");
    assert_eq!(err.1, "keydb download failed: 404");
    let boom = std::panic::catch_unwind(|| -> Result<String, String> {
        std::panic::panic_any("zip entry out of range")
    });
    let (kind, msg) = keydb_outcome(boom);
    assert_eq!(kind, LogKind::Notice);
    assert_eq!(msg, "keydb update failed — internal error");
    let owned: std::thread::Result<Result<String, String>> =
        Err(Box::new(format!("KEY {}", "0123abcd")));
    let (kind, msg) = keydb_outcome(owned);
    assert_eq!(kind, LogKind::Notice);
    assert!(!msg.contains("0123abcd"), "panic payload leaked: {msg}");
}

// M6: an ad-hoc signing failure must fail the bundle build.
#[test]
fn bundle_sh_fails_when_ad_hoc_signing_fails() {
    let sh = include_str!("../macos/bundle.sh");
    for line in sh
        .lines()
        .filter(|l| l.trim_start().starts_with("codesign"))
    {
        assert!(
            !line.contains("|| true") && !line.contains("2>/dev/null"),
            "codesign failure swallowed: {line}"
        );
    }
    assert!(sh.contains("set -e"));
    assert!(
        sh.matches("codesign --verify").count() >= 2,
        "both signing paths must verify the result"
    );
}

// M8: every item of the shared menu becomes a real, correctly routed item.
#[test]
fn every_shared_menu_item_maps_to_a_selector_that_routes_back_to_it() {
    use crate::ui::{MenuAction, MenuEntry};
    for hidden in [false, true] {
        for g in crate::ui::menu_layout(hidden) {
            for e in &g.entries {
                let MenuEntry::Item(mi) = e else { continue };
                let Some((s, to_ctrl)) = selector_for_action(&mi.action) else {
                    panic!("{:?} in {:?} is silently dropped on macOS", mi.action, g.id);
                };
                match &mi.action {
                    MenuAction::Cmd(c) => {
                        assert!(to_ctrl, "{c:?} would go to the responder chain");
                        assert_eq!(cmd_for(s), Some(*c), "{c:?} runs another command");
                    }
                    MenuAction::OpenDisc => {
                        assert!(to_ctrl);
                        assert_eq!(s, sel!(onOpenDisc:));
                    }
                    MenuAction::StandardCut => assert_eq!((s, to_ctrl), (sel!(cut:), false)),
                    MenuAction::StandardCopy => assert_eq!((s, to_ctrl), (sel!(copy:), false)),
                    MenuAction::StandardPaste => {
                        assert_eq!((s, to_ctrl), (sel!(paste:), false))
                    }
                    MenuAction::StandardSelectAllText => {
                        assert_eq!((s, to_ctrl), (sel!(selectAll:), false))
                    }
                }
            }
        }
    }
}

// M10 + M11: a language switch must forget the log memo and menu handle.
#[test]
fn a_language_switch_forgets_the_log_memo_and_menu_item_source_inspection_only() {
    let body = fn_body(prod_src(), "fn relocalize(");
    for reset in [
        format!("{}{}", "self.ivars().log_shown", ".borrow_mut() = None;"),
        format!(
            "{}{}",
            "self.ivars().log_menu_item", ".borrow_mut() = None;"
        ),
        format!("{}{}", "self.ivars().log_lens", ".borrow_mut().clear();"),
    ] {
        assert!(body.contains(&reset), "relocalize() lost `{reset}`");
    }
}

// M12: the UI-driver helpers must not hide behind a dead-code allow.
#[test]
fn no_production_code_is_allowed_to_be_dead() {
    let allow = format!("{}{}", "#[allow(dead", "_code)]");
    assert!(
        !prod_src().contains(&allow),
        "an allow(dead_code) hides helpers nothing calls; gate them on \
             cfg(debug_assertions) with their only caller, self_test"
    );
}

// M13: the shared Accel -> AppKit key-equivalent translation.
#[test]
fn accelerators_translate_to_appkit_key_equivalents() {
    use crate::ui::Accel;
    assert_eq!(key_equivalent(None), "");
    assert_eq!(key_equivalent(Some(&Accel::primary("c"))), "c");
    assert_eq!(key_equivalent(Some(&Accel::primary("C"))), "c");
    assert_eq!(key_equivalent(Some(&Accel::primary_shift("a"))), "A");
    assert_eq!(key_equivalent(Some(&Accel::bare("F1"))), "");
    let alt = Accel {
        alt: true,
        ..Accel::primary("x")
    };
    assert_eq!(key_equivalent(Some(&alt)), "");
    for g in crate::ui::menu_layout(false) {
        for e in &g.entries {
            if let crate::ui::MenuEntry::Item(mi) = e
                && let Some(a) = &mi.accel
                && a.key.len() == 1
                && !a.alt
            {
                assert!(!key_equivalent(Some(a)).is_empty(), "{:?}", mi.action);
            }
        }
    }
}

// M14: the tick-repaint early-out.
#[test]
fn the_tick_repaint_is_skipped_only_when_no_tick_moved() {
    use crate::ui::{Check, Row};
    let row = |i: usize, c: Option<Check>| Row {
        index: i,
        depth: 0,
        type_s: "Title".into(),
        desc: format!("t{i}"),
        length: String::new(),
        size: String::new(),
        lang: String::new(),
        item: String::new(),
        format: String::new(),
        notes: String::new(),
        check: c,
        check_enabled: c.is_some(),
    };
    let a = vec![row(0, Some(Check::On)), row(1, None)];
    assert!(ticks_match(&a, &a.clone()));
    let flipped = vec![row(0, Some(Check::Off)), row(1, None)];
    assert!(!ticks_match(&a, &flipped), "a click must repaint");
    let mixed = vec![row(0, Some(Check::Mixed)), row(1, None)];
    assert!(!ticks_match(&a, &mixed));
    assert!(!ticks_match(&a, &a[..1]), "a length change must repaint");
    assert!(ticks_match(&[], &[]));
}

// M15: outside an .app bundle (this test binary) the gate must hold.
#[test]
fn notifications_are_gated_off_outside_an_app_bundle() {
    assert!(!notifications_available(), "cargo test runs unbundled");
    // Ungated, UNUserNotificationCenter raises and aborts this process.
    notify_rip_finished("t", "b", Some("/nonexistent"));
    let src = prod_src();
    for (at, _) in src.match_indices("UNUserNotificationCenter::currentNotificationCenter()") {
        let before = &src[src[..at].rfind("\nfn ").unwrap_or(0)..at];
        let before = &before[before.rfind("\npub fn ").unwrap_or(0)..];
        assert!(
            before.contains("notifications_available()"),
            "ungated UNUserNotificationCenter use near byte {at}"
        );
    }
}

// M16: one render per tick/command, not two.
#[test]
fn a_command_or_tick_renders_once_source_inspection_only() {
    let src = prod_src();
    for (at, _) in src.match_indices("perform(fx);") {
        let stmt_start = src[..at].rfind("let fx = ").unwrap_or(0);
        assert!(
            !src[stmt_start..at].contains("app_mut("),
            "app_mut renders and perform renders again: {}",
            &src[stmt_start..at]
        );
    }
    for (at, _) in src.match_indices(".render();") {
        let prev = src[..at].trim_end();
        let prev = &prev[..prev.rfind('\n').unwrap_or(0)];
        let prev_stmt = &prev[prev.rfind(";\n").map_or(0, |i| i + 2)..];
        assert!(
            !prev_stmt.trim_start().contains(".app_mut("),
            "render() straight after app_mut() paints twice: {prev_stmt}"
        );
    }
}

// M17: an unchanged detail pane is not reset (keeps the user's selection).
#[test]
fn the_detail_pane_is_only_reset_when_it_changed_source_inspection_only() {
    let body = fn_body(prod_src(), "fn render(");
    let set = format!("{}{}", "tv.setString(&NSString::from_str(&v", ".detail));");
    let at = body.find(&set).expect("detail setString moved");
    assert!(
        body[at.saturating_sub(200)..at].contains("!= v.detail"),
        "detail text view is reset on every render"
    );
}

// M18: a front trim deletes the dropped lines instead of rebuilding.
#[test]
fn a_front_trim_drops_lines_instead_of_rebuilding() {
    let full = Some(LogShown {
        first: 0,
        len: 5_001,
    });
    assert_eq!(
        log_plan(full, 1_000, 4_001),
        LogPlan::Trim {
            drop: 1_000,
            from: 4_001
        }
    );
    assert_eq!(
        log_plan(full, 1_000, 4_050),
        LogPlan::Trim {
            drop: 1_000,
            from: 4_001
        }
    );
    assert_eq!(log_plan(full, 1_000, 3_000), LogPlan::Rebuild, "shrank");
    assert_eq!(log_plan(full, 5_001, 0), LogPlan::Rebuild, "cleared");
    assert_eq!(log_plan(full, 9_000, 10), LogPlan::Rebuild, "past it");
}

// M18: a trim only deletes when the recorded lengths match the pane exactly.
#[test]
fn a_trim_rebuilds_when_the_recorded_lengths_disagree_with_the_pane() {
    let lens: std::collections::VecDeque<usize> = [3, 5, 2].into();
    assert_eq!(trim_chars(&lens, 2, 10), Some(8));
    assert_eq!(trim_chars(&lens, 0, 10), Some(0));
    assert_eq!(trim_chars(&lens, 2, 11), None, "pane holds an extra line");
    assert_eq!(trim_chars(&lens, 2, 9), None, "pane shorter than recorded");
    assert_eq!(trim_chars(&lens, 4, 10), None, "dropping more than shown");
}

// K2 + C13: the Recovery tab loses the stale capture caption; Output
// gains the notify toggle on the same keyed get_bool/set_bool path.
#[test]
fn settings_form_has_the_notify_toggle_and_no_capture_note() {
    let body = fn_body(prod_src(), "fn build_prefs(");
    let note = format!("{}{}", "gui.set.capture", "_note");
    assert!(!body.contains(&note), "stale capture caption still shown");
    let key = format!("{}{}", "\"notify_when_rip", "_finished\",");
    assert!(
        body.contains(&key),
        "no Notify when a rip finishes checkbox"
    );
}
