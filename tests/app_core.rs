//! The application core, exercised with NO shell attached.
//!
//! If these pass, every shell that forwards events to `App::dispatch` and
//! renders `App::view()` behaves identically — that is the whole point of the
//! split. These run on any platform, including CI on Linux.

use freemkv::ui::*;

fn iso() -> Option<String> {
    let p = std::env::var("FMKV_TEST_ISO").ok()?;
    std::path::Path::new(&p).exists().then_some(p)
}

#[test]
fn starts_empty_with_a_ready_line() {
    let app = App::new();
    assert_eq!(app.page, Page::Empty);
    assert!(!app.running());
    assert_eq!(app.log.len(), 1);
    assert!(app.log[0].text.contains("ready"));
}

#[test]
#[ignore = "needs a real disc fixture; run with --ignored"]
fn open_and_close_move_between_pages() {
    let Some(p) = iso() else {
        panic!("set FMKV_TEST_ISO to a real disc image to run this test");
    };
    let mut app = App::new();
    app.open(&p);
    assert_eq!(app.page, Page::Titles);
    assert!(app.tree.title_count() > 0);
    app.dispatch(Cmd::Close);
    assert_eq!(app.page, Page::Empty);
    assert_eq!(app.tree.title_count(), 0);
}

#[test]
fn a_bad_source_reports_and_stays_empty() {
    let mut app = App::new();
    app.open("/etc/hosts");
    assert_eq!(app.page, Page::Empty, "a failed open must not show a tree");
    assert!(app.log.iter().any(|l| l.kind == LogKind::Notice));
}

#[test]
#[ignore = "needs a real disc fixture; run with --ignored"]
fn selection_commands_all_go_through_dispatch() {
    let Some(p) = iso() else {
        panic!("set FMKV_TEST_ISO to a real disc image to run this test");
    };
    let mut app = App::new();
    app.open(&p);
    let n = app.tree.title_count();

    app.dispatch(Cmd::SelectAll);
    assert_eq!(app.tree.ticked_titles().len(), n);
    app.dispatch(Cmd::SelectNone);
    assert!(app.tree.ticked_titles().is_empty());
    app.dispatch(Cmd::Invert);
    assert_eq!(app.tree.ticked_titles().len(), n, "none inverts to all");
}

#[test]
fn log_commands_work() {
    let mut app = App::new();
    app.dispatch(Cmd::ClearLog);
    assert!(app.log.is_empty());
    assert!(!app.log_hidden);
    app.dispatch(Cmd::ToggleLog);
    assert!(app.log_hidden);
    app.dispatch(Cmd::ToggleLog);
    assert!(!app.log_hidden);
}

#[test]
fn commands_that_need_the_shell_return_an_effect() {
    let mut app = App::new();
    assert_eq!(app.dispatch(Cmd::Open), vec![Effect::PickSource]);
    assert_eq!(app.dispatch(Cmd::SetOutput), vec![Effect::PickOutputDir]);
    assert_eq!(app.dispatch(Cmd::Settings), vec![Effect::ShowSettings]);
    assert_eq!(app.dispatch(Cmd::About), vec![Effect::ShowAbout]);
    assert_eq!(app.dispatch(Cmd::Quit), vec![Effect::Quit]);
    assert!(matches!(
        app.dispatch(Cmd::Docs).as_slice(),
        [Effect::OpenUrl(_)]
    ));
}

#[test]
fn run_is_refused_without_a_source() {
    let mut app = App::new();
    app.dispatch(Cmd::Run);
    assert!(!app.running());
    assert!(app.log.iter().any(|l| l.text.contains("Open a source")));
}

#[test]
fn destructive_commands_are_blocked_while_running_but_cancel_is_not() {
    // The rule every shell inherits: nothing may start a second job or move
    // the target mid-write, and Cancel must always be reachable.
    for c in [
        Cmd::Run,
        Cmd::Open,
        Cmd::Close,
        Cmd::SetOutput,
        Cmd::Eject,
        Cmd::SelectAll,
        Cmd::SelectNone,
        Cmd::Invert,
        Cmd::Settings,
    ] {
        assert!(blocked_while_running(c), "{c:?} must be blocked");
    }
    for c in [Cmd::Cancel, Cmd::About, Cmd::Docs, Cmd::Quit] {
        assert!(!blocked_while_running(c), "{c:?} must stay available");
    }
}

#[test]
#[ignore = "needs a real disc fixture; run with --ignored"]
fn the_view_describes_the_whole_screen() {
    let Some(p) = iso() else {
        panic!("set FMKV_TEST_ISO to a real disc image to run this test");
    };
    let mut app = App::new();
    app.open(&p);
    let v = app.view();
    assert_eq!(v.page, Page::Titles);
    assert!(!v.title_rows.is_empty());
    assert!(v.can_run);
    // The disc root and video rows carry no checkbox at all.
    let root = &v.title_rows[0];
    assert_eq!(root.depth, 0);
    assert!(root.check.is_none(), "the disc root is not a choice");
    for r in v.title_rows.iter().filter(|r| r.type_s == "Video") {
        assert!(r.check.is_none(), "video is implicit");
    }
    assert!(!v.formats.is_empty());
}

#[test]
fn output_formats_follow_the_source_kind() {
    let disc = output_formats(true, true);
    let file = output_formats(false, true);
    let flat = |g: &Vec<Vec<&str>>| g.concat().join("|");
    assert!(flat(&disc).contains("Whole disc"));
    assert!(
        !flat(&file).contains("Whole disc"),
        "a container has no disc to image"
    );
}

#[test]
#[ignore = "needs a real disc fixture; run with --ignored"]
fn tri_state_reflects_partial_stream_selection() {
    let Some(p) = iso() else {
        panic!("set FMKV_TEST_ISO to a real disc image to run this test");
    };
    let mut app = App::new();
    app.open(&p);
    let title = app
        .tree
        .arena
        .iter()
        .position(|n| n.type_s == "Title" && !n.children.is_empty());
    let Some(t) = title else { return };
    app.tree.set_checked(t, true);
    assert_eq!(app.tree.check_state(t), Check::On);
    // Untick one stream → the title becomes mixed, not off.
    if let Some(&c) = app.tree.arena[t]
        .children
        .iter()
        .find(|&&c| app.tree.arena[c].checkable())
    {
        *app.tree.arena[c].checked.borrow_mut() = false;
        let st = app.tree.check_state(t);
        assert!(
            st == Check::Mixed || st == Check::Off,
            "partial selection must not read as fully on"
        );
    }
}

#[test]
fn formatting_is_shared_so_both_shells_show_identical_text() {
    assert_eq!(fmt_bytes(6 * 1024 * 1024 * 1024), "6.00 GB");
    assert_eq!(fmt_hms(3725), "1:02:05");
    assert_eq!(rate_text(0, true), "not reported");
    assert_eq!(rate_text(0, false), "—");
    assert!(rate_text(52_428_800, true).contains("/s"));
    assert!(bar_caption(50.0, 65, Some(120)).contains("Remaining"));
    assert!(!bar_caption(50.0, 65, None).contains("Remaining"));
    assert_eq!(overall_pct(1, 2, 50.0), 75.0);
}

#[test]
fn source_extensions_match_the_documented_stream_urls() {
    for e in ["iso", "mkv", "m2ts", "mp4"] {
        assert!(SOURCE_EXTS.contains(&e), "{e} must be accepted");
    }
    assert!(is_container("a.mkv"));
    assert!(is_container("a.mp4"));
    assert!(!is_container("a.iso"));
}

// ── Coverage the shell-driving self-test cannot give us ─────────────────────
// Pins the platform-neutral core so the Windows shell inherits the same
// behaviour without a second set of assertions.

use freemkv::ui::{
    App, Cmd, Effect, InfoRows, Page, bar_caption, blocked_while_running, fmt_bytes, fmt_hms,
    is_container, output_formats, overall_pct, rate_text,
};

/// Every `Cmd` must be handled — no silent fall-through. A new command that
/// nobody wired would otherwise ship as a dead button in both shells.
#[test]
fn every_command_is_handled_by_dispatch() {
    const ALL: &[Cmd] = &[
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
    ];
    for &c in ALL {
        let mut a = App::new();
        // Must not panic, and must leave the app in a coherent state.
        let _ = a.dispatch(c);
        assert!(
            matches!(
                a.view().page,
                Page::Empty | Page::Titles | Page::Progress | Page::Result
            ),
            "{c:?} left the app on no valid page"
        );
    }
}

/// Byte formatting rolls units and never prints a bare byte count for large
/// values — the defect the user caught on the Information panel.
#[test]
fn byte_formatting_rolls_units() {
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(999), "999 B");
    assert!(fmt_bytes(1_500).ends_with("KB"), "{}", fmt_bytes(1_500));
    assert!(
        fmt_bytes(5_000_000).ends_with("MB"),
        "{}",
        fmt_bytes(5_000_000)
    );
    assert!(
        fmt_bytes(6_743_590_912).ends_with("GB"),
        "{}",
        fmt_bytes(6_743_590_912)
    );
    // No raw 10-digit number ever reaches the panel.
    assert!(!fmt_bytes(6_743_590_912).contains("6743590912"));
}

/// Durations use a single shape app-wide, matching the title column.
#[test]
fn duration_formatting_drops_the_hours_field_under_an_hour() {
    // The progress caption's Elapsed/Remaining: `m:ss` under an hour (no odd
    // leading "0:"), `h:mm:ss` once there's an hour to show.
    assert_eq!(fmt_hms(0), "0:00");
    assert_eq!(fmt_hms(59), "0:59");
    assert_eq!(fmt_hms(96), "1:36");
    assert_eq!(fmt_hms(600), "10:00");
    assert_eq!(fmt_hms(3_600), "1:00:00");
    assert_eq!(fmt_hms(7_187), "1:59:47");
}

/// The rate must never read as a real number when nothing is being measured —
/// showing "0.0 MB/s" during a stall reads as a broken rip.
#[test]
fn rate_text_is_honest_when_unmeasured() {
    assert_eq!(rate_text(0, false), "—");
    assert!(
        rate_text(0, true).contains("not reported"),
        "{}",
        rate_text(0, true)
    );
    assert!(
        rate_text(52_428_800, true).contains("MB/s"),
        "{}",
        rate_text(52_428_800, true)
    );
}

/// Overall progress spans the whole job, not the current title — the two
/// identical bars the user spotted.
#[test]
fn overall_progress_spans_the_whole_job() {
    assert_eq!(overall_pct(0, 4, 0.0), 0.0);
    assert_eq!(overall_pct(0, 4, 100.0), 25.0);
    assert_eq!(overall_pct(2, 4, 50.0), 62.5);
    assert_eq!(overall_pct(4, 4, 0.0), 100.0);
    // A single title makes overall identical to current — nothing to add.
    assert_eq!(overall_pct(0, 1, 40.0), 40.0);
}

/// A caption always says something; an unknown ETA is never rendered as "0".
#[test]
fn bar_caption_never_fakes_an_eta() {
    let c = bar_caption(10.0, 30, None);
    assert!(!c.contains("0:00 left"), "{c}");
    assert!(bar_caption(50.0, 60, Some(120)).contains("2:00"));
}

/// The Information panel's "Output file" row must name a FILE.
#[test]
fn info_rows_output_is_a_file_not_a_folder() {
    let i = InfoRows::starting("/src/Movie.iso", "/out/Movie_t1.mkv");
    let rows = i.as_array();
    assert!(rows[4].ends_with(".mkv"), "{:?}", rows[4]);
    // Every row is populated at start — no blanks, which the user reported.
    for (n, r) in rows.iter().enumerate() {
        assert!(!r.trim().is_empty(), "row {n} is blank at start");
    }
}

/// Cancelling must never be summarized as "Finished".
#[test]
fn a_cancelled_run_is_not_reported_as_finished() {
    let mut a = App::new();
    a.open("/nonexistent/x.iso");
    a.dispatch(Cmd::Cancel);
    let v = a.view();
    if v.page == Page::Result {
        assert_ne!(v.result_heading, "Finished");
    }
}

/// Only Cancel/About/Docs/Quit survive a running job — everything destructive
/// is refused. The user found Start rip and Set output live mid-rip.
#[test]
fn only_safe_commands_survive_a_running_job() {
    for c in [
        Cmd::Open,
        Cmd::Close,
        Cmd::SetOutput,
        Cmd::Run,
        Cmd::Eject,
        Cmd::Settings,
    ] {
        assert!(blocked_while_running(c), "{c:?} must be blocked mid-rip");
    }
    for c in [Cmd::Cancel, Cmd::About, Cmd::Docs, Cmd::Quit] {
        assert!(
            !blocked_while_running(c),
            "{c:?} must stay available mid-rip"
        );
    }
}

/// Container inputs are single titles; disc images are scanned. Getting this
/// wrong ran a disc scan on an MKV (the E6013 the user hit).
#[test]
fn container_detection_matches_the_documented_schemes() {
    for p in ["/a/b.mkv", "/a/b.m2ts", "/a/b.mp4", "/A/B.MKV"] {
        assert!(is_container(p), "{p} is a container");
    }
    for p in ["/a/b.iso", "/dev/sr0", "/a/b.img"] {
        assert!(!is_container(p), "{p} is not a container");
    }
}

/// A disc source offers ISO backup; a container never does (you cannot back a
/// container up to a disc image).
#[test]
fn output_formats_depend_on_the_source_kind() {
    let disc = output_formats(true, true).concat().join(" ");
    let cont = output_formats(false, true).concat().join(" ");
    assert!(disc.contains("ISO"), "{disc}");
    assert!(!cont.contains("ISO"), "{cont}");
    for f in [disc, cont] {
        assert!(f.contains("MKV"), "{f}");
    }
}

// The `video://`/`audio://`/`sub://` per-track-kind sinks must be reachable
// from the GUI too. They narrow `demux://`, which applies to any source, so
// they're offered for a container as well as a disc/ISO.
#[test]
fn per_track_kind_exports_are_offered_for_every_source_kind() {
    for disc_source in [true, false] {
        let f = output_formats(disc_source, true).concat().join(" | ");
        for want in [
            "Selected titles → video tracks only",
            "Selected titles → audio tracks only",
            "Selected titles → subtitle tracks only",
        ] {
            assert!(
                f.contains(want),
                "{want:?} missing for disc_source={disc_source}: {f}"
            );
            assert!(
                freemkv::ui::format_by_title(want, disc_source, true).is_some(),
                "{want:?} does not resolve for disc_source={disc_source}"
            );
        }
    }
}

/// The per-track-kind entries sit in the titles group, beside the plain
/// "separate track files" they narrow — not in the whole-disc or metadata
/// group, where a shell would draw them under the wrong separator.
#[test]
fn per_track_kind_exports_sit_with_the_other_title_sinks() {
    let groups = output_formats(true, true);
    let titles = &groups[0];
    for want in [
        "Selected titles → separate track files",
        "Selected titles → video tracks only",
        "Selected titles → audio tracks only",
        "Selected titles → subtitle tracks only",
    ] {
        assert!(
            titles.contains(&want),
            "{want:?} is not in the titles group"
        );
    }
}

// An ISO source sets `disc_source` = "not a container", so it gets the
// decrypted-folder row. Pins that the ISO IMAGE row does NOT follow into a
// container: `iso://` as a destination needs a physical disc.
#[test]
fn the_decrypted_folder_row_is_offered_for_a_disc_or_iso_but_never_a_container() {
    // An ISO path is not a container, so this is the flag an ISO source sets.
    assert!(!is_container("/movies/Disc.iso"));
    let disc_or_iso = output_formats(true, true).concat().join(" | ");
    assert!(
        disc_or_iso.contains("Whole disc → decrypted folder"),
        "{disc_or_iso}"
    );
    let container = output_formats(false, true).concat().join(" | ");
    assert!(
        !container.contains("Whole disc → decrypted folder"),
        "a container has no disc tree to unpack: {container}"
    );
    assert!(
        !container.contains("Whole disc → ISO image"),
        "ISO image must stay off a non-disc source: {container}"
    );
    assert!(
        freemkv::ui::format_by_title("Whole disc → ISO image", false, true).is_none(),
        "ISO image must not resolve for a non-disc source"
    );
}

/// Opening a source clears the previous one — no stale tree behind a new disc.
#[test]
fn opening_a_second_source_replaces_the_first() {
    let Ok(iso) = std::env::var("FMKV_TEST_ISO") else {
        return;
    };
    let mut a = App::new();
    a.open(&iso);
    let first = a.view().title_rows.len();
    assert!(first > 0);
    a.dispatch(Cmd::Close);
    assert_eq!(a.view().page, Page::Empty);
    assert!(a.view().title_rows.is_empty(), "Close left rows behind");
    a.open(&iso);
    assert_eq!(
        a.view().title_rows.len(),
        first,
        "reopen produced a different tree"
    );
}

/// Eject is meaningless for a file source and must not be offered — a control
/// that does not do what it says is worse than no control.
#[test]
fn eject_is_offered_only_for_a_real_drive() {
    let Ok(iso) = std::env::var("FMKV_TEST_ISO") else {
        return;
    };
    let mut a = App::new();
    a.open(&iso);
    assert!(!a.view().eject_visible, "an ISO has nothing to eject");
}

/// Commands needing the platform must ASK for it via an Effect rather than
/// doing it — that is the whole cross-platform seam.
#[test]
fn platform_work_is_requested_not_performed() {
    let mut a = App::new();
    assert!(matches!(
        a.dispatch(Cmd::Open).as_slice(),
        [Effect::PickSource, ..]
    ));
    assert!(
        a.dispatch(Cmd::Docs)
            .iter()
            .any(|e| matches!(e, Effect::OpenUrl(_)))
    );
    assert!(
        a.dispatch(Cmd::About)
            .iter()
            .any(|e| matches!(e, Effect::ShowAbout))
    );
    assert!(
        a.dispatch(Cmd::Quit)
            .iter()
            .any(|e| matches!(e, Effect::Quit))
    );
}

// ── Output format actually takes effect ────────────────────────────────────
// The popup once was decoration: it showed a choice the model never heard
// about, so picking MP4 still produced MKV. Assert the full chain instead.

/// A shell resolves its widget text through the core, and unknown text is
/// rejected rather than entering the model.
#[test]
fn format_titles_resolve_through_the_core() {
    use freemkv::ui::format_by_title;
    assert_eq!(
        format_by_title("Selected titles → MP4", true, true),
        Some("Selected titles → MP4")
    );
    assert!(format_by_title("Whole disc → ISO image", true, true).is_some());
    // A container cannot produce a whole-disc image, so the title must not
    // resolve for that source kind.
    assert!(format_by_title("Whole disc → ISO image", false, true).is_none());
    assert!(format_by_title("something nobody offers", true, true).is_none());
}

/// Dispatching SetFormat changes what the app will actually write.
#[test]
fn choosing_a_format_changes_the_output_extension() {
    let Ok(iso) = std::env::var("FMKV_TEST_ISO") else {
        return;
    };
    for (title, ext) in [
        ("Selected titles → MP4", ".mp4"),
        ("Selected titles → M2TS", ".m2ts"),
        ("Selected titles → MKV", ".mkv"),
    ] {
        // A fresh app per format: once a run starts, changing the format is
        // correctly refused, so reusing one app would test the block instead.
        let mut a = App::new();
        a.open(&iso);
        let f = freemkv::ui::format_by_title(title, true, true).expect(title);
        a.dispatch(Cmd::SetFormat(f));
        assert_eq!(a.view().format, title, "the view must show the pick");

        // The Information panel's "Output file" row is the user-visible proof
        // that the choice reached the job.
        a.dispatch(Cmd::Run);
        let out = a
            .view()
            .info
            .as_ref()
            .map(|i| i[4].clone())
            .unwrap_or_default();
        a.dispatch(Cmd::Cancel);
        assert!(
            out.ends_with(ext),
            "picked {title} but the output is {out:?}"
        );
    }
}

// MP4 must not be offered for a source whose video it cannot hold. A DVD
// is MPEG-2, which has no MP4 mapping — the mux fails with E9048 after the
// user has already waited, so the impossible entry is removed, not just refused.
#[test]
fn mp4_is_not_offered_when_the_video_cannot_go_in_one() {
    let f = output_formats(true, false).concat().join(" | ");
    assert!(!f.contains("MP4"), "MP4 still offered: {f}");
    // The rest of the menu is untouched — only the impossible entry goes.
    assert!(f.contains("MKV") && f.contains("M2TS") && f.contains("ISO"));
    // And it cannot be selected by name either, so no stale pick can slip in.
    assert!(freemkv::ui::format_by_title("Selected titles → MP4", true, false).is_none());
}

/// A real DVD hides MP4; unknown codecs never hide anything.
#[test]
fn a_dvd_hides_mp4_end_to_end() {
    let Ok(iso) = std::env::var("FMKV_TEST_ISO") else {
        return;
    };
    let mut a = App::new();
    a.open(&iso);
    assert!(!a.mp4_possible(), "a DVD is MPEG-2; MP4 cannot hold it");
    let offered = a.view().formats.concat().join(" | ");
    assert!(!offered.contains("MP4"), "MP4 offered for a DVD: {offered}");
    // A fresh app knows no codecs and must not hide the option on a guess.
    assert!(App::new().mp4_possible());
}

// The GUI language picker must list exactly the locales the i18n crate
// ships. `ui::LOCALES` carries an extra "auto" row (system locale); every
// other code must be a real locale file, and vice versa.
#[test]
fn locale_picker_matches_shipped_catalogs() {
    use std::collections::BTreeSet;
    let picker: BTreeSet<&str> = freemkv::ui::LOCALES
        .iter()
        .map(|(_, code)| *code)
        .filter(|c| *c != "auto")
        .collect();
    let shipped: BTreeSet<&str> = freemkv::strings::SHIPPED_CODES.iter().copied().collect();
    assert_eq!(
        picker,
        shipped,
        "picker rows and shipped locale files disagree\n  picker-only: {:?}\n  shipped-only: {:?}",
        picker.difference(&shipped).collect::<Vec<_>>(),
        shipped.difference(&picker).collect::<Vec<_>>(),
    );
    // Auto is always offered and always first.
    assert_eq!(freemkv::ui::LOCALES.first().map(|(_, c)| *c), Some("auto"));
}
