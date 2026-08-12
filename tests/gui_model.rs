//! The decisions the desktop shells make, with NO shell and NO disc attached.
//!
//! `windows.rs` and `mac.rs` are 7,700 lines between them, and almost none of
//! it is drawing: what is selectable, what a tri-state checkbox shows, which
//! titles are ticked, what the output file is called, what the log line says,
//! which rows hang off which. Every one of those is a decision, and a decision
//! belongs in `ui`, where BOTH shells read it and one test covers both.
//!
//! Everything here is driven from a SYNTHETIC scan built in this file, so these
//! run on any host, in CI, with no drive, no disc image and no window server —
//! unlike `tests/app_core.rs`'s tri-state coverage, which is gated behind an
//! `FMKV_TEST_ISO` fixture and therefore does not run in CI today.

use freemkv::engine::{Row as ScanRow, Scanned};
use freemkv::ui::*;

// ── synthetic scans ────────────────────────────────────────────────────────
//
// Shaped exactly like `engine::scan_disc`'s output: a depth-0 disc row, then
// per title a depth-1 Title row followed by its depth-2 stream rows. `title` is
// the CANONICAL disc title index and is deliberately not the row position.

fn row(type_s: &str, desc: &str, depth: u8, checkable: bool, title: usize) -> ScanRow {
    ScanRow {
        type_s: type_s.into(),
        desc: desc.into(),
        depth,
        checkable,
        title,
        info: format!("{type_s} — {desc}"),
        pid: None,
        duration_secs: 0.0,
        lang: String::new(),
        forced: false,
    }
}

/// One title, with `audio` audio tracks and one always-on video track.
fn title_block(ti: usize, secs: f64, audio: usize) -> Vec<ScanRow> {
    let mut t = row("Title", &format!("{}.  playlist", ti + 1), 1, true, ti);
    t.duration_secs = secs;
    let mut out = vec![t, row("Video", "H.264  1080p", 2, false, ti)];
    for a in 0..audio {
        let mut r = row("Audio", &format!("DTS-HD  track {a}"), 2, true, ti);
        r.pid = Some((0x1100 + ti * 16 + a) as u16);
        out.push(r);
    }
    out
}

/// A title with a video track and NOTHING checkable under it — what a
/// video-only source (or a video-only disc title) actually produces, since
/// `stream_rows` marks video rows uncheckable.
fn video_only_disc() -> Scanned {
    let mut t = row("Title", "1.  playlist", 1, true, 0);
    t.duration_secs = 5400.0;
    Scanned {
        label: "VIDEO_ONLY".into(),
        rows: vec![
            row("Bluray disc", "VIDEO_ONLY", 0, false, usize::MAX),
            t,
            row("Video", "H.264  1080p", 2, false, 0),
        ],
        key_summary: "keys: none needed".into(),
        title_count: 1,
        video_codecs: vec!["H.264".into()],
        details: vec![],
    }
}

/// A disc whose titles have the given `(duration_secs, audio_track_count)`.
fn disc(titles: &[(f64, usize)]) -> Scanned {
    let mut rows = vec![row("Bluray disc", "TEST_DISC", 0, false, usize::MAX)];
    for (ti, (secs, audio)) in titles.iter().enumerate() {
        rows.extend(title_block(ti, *secs, *audio));
    }
    Scanned {
        label: "TEST_DISC".into(),
        rows,
        key_summary: "keys: none needed".into(),
        title_count: titles.len(),
        video_codecs: vec!["H.264".into(); titles.len()],
        details: vec![],
    }
}

/// The default fixture: a main feature with two audio tracks and a short extra
/// with one.
fn two_title_disc() -> Scanned {
    disc(&[(5400.0, 2), (600.0, 1)])
}

fn tree(sc: &Scanned, sel_mode: &str, min_secs: f64) -> Tree {
    Tree::from_scan(sc, sel_mode, min_secs, &LangPrefs::default())
}

/// The same tree, built with preferred-language defaults.
fn tree_prefs(sc: &Scanned, sel_mode: &str, prefs: &LangPrefs) -> Tree {
    Tree::from_scan(sc, sel_mode, 0.0, prefs)
}

/// Arena position of the `n`th row of a given type.
fn nth(t: &Tree, type_s: &str, n: usize) -> usize {
    t.arena
        .iter()
        .enumerate()
        .filter(|(_, x)| x.type_s == type_s)
        .nth(n)
        .unwrap_or_else(|| panic!("no {type_s} row #{n}"))
        .0
}

// ══ what is selectable ═════════════════════════════════════════════════════

#[test]
fn the_disc_row_is_not_a_choice() {
    // Ticking "the disc" means nothing — the choice is which titles. A
    // checkbox there is a control that cannot do anything.
    // VERIFIED against the shells' own rule: both render a row with no
    // checkbox as a blank spacer / no state image.
    let t = tree(&two_title_disc(), "Main film only", 0.0);
    let root = t.roots[0];
    assert!(!t.arena[root].checkable(), "the disc root grew a checkbox");
    assert_eq!(t.arena[root].type_s, "Bluray disc");
}

#[test]
fn video_is_implicit_and_carries_no_checkbox() {
    // Every rip includes the video; offering to untick it would offer a
    // combination the engine does not produce. VERIFIED: `engine::stream_rows`
    // sets `checkable: ty != "Video"` for exactly this reason.
    let t = tree(&two_title_disc(), "Main film only", 0.0);
    let videos: Vec<usize> = t
        .arena
        .iter()
        .enumerate()
        .filter(|(_, n)| n.type_s == "Video")
        .map(|(i, _)| i)
        .collect();
    assert!(!videos.is_empty(), "fixture must contain video rows");
    for v in videos {
        assert!(!t.arena[v].checkable(), "row {v} (Video) grew a checkbox");
    }
}

#[test]
fn titles_and_audio_tracks_are_choices() {
    let t = tree(&two_title_disc(), "Main film only", 0.0);
    for ty in ["Title", "Audio"] {
        let any = t.arena.iter().filter(|n| n.type_s == ty).count();
        assert!(any > 0, "fixture must contain {ty} rows");
        for n in t.arena.iter().filter(|n| n.type_s == ty) {
            assert!(n.checkable(), "a {ty} row lost its checkbox");
        }
    }
}

#[test]
fn bulk_selection_never_touches_a_row_that_has_no_checkbox() {
    // Select All / Select None / Invert are advertised as acting on the
    // selection; silently flipping the hidden `checked` flag of a Video or the
    // disc row would put the model in a state no click can produce.
    let t = tree(&two_title_disc(), "Main film only", 0.0);
    let uncheckable: Vec<usize> = t
        .arena
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.checkable())
        .map(|(i, _)| i)
        .collect();
    assert!(!uncheckable.is_empty());
    let before: Vec<bool> = uncheckable
        .iter()
        .map(|&i| *t.arena[i].checked.borrow())
        .collect();
    t.set_all(true);
    t.invert();
    t.set_all(false);
    t.invert();
    let after: Vec<bool> = uncheckable
        .iter()
        .map(|&i| *t.arena[i].checked.borrow())
        .collect();
    assert_eq!(before, after, "a bulk command flipped an uncheckable row");
}

// ══ what the tri-state checkbox shows ══════════════════════════════════════

#[test]
fn a_title_with_every_stream_on_reads_on() {
    let t = tree(&two_title_disc(), "All titles", 0.0);
    let title = nth(&t, "Title", 0);
    t.set_checked(title, true);
    assert_eq!(t.check_state(title), Check::On);
}

#[test]
fn a_title_with_every_stream_off_reads_off() {
    let t = tree(&two_title_disc(), "All titles", 0.0);
    let title = nth(&t, "Title", 0);
    t.set_checked(title, false);
    assert_eq!(t.check_state(title), Check::Off);
}

#[test]
fn a_title_with_some_streams_off_reads_mixed() {
    // The whole reason the tri-state exists: On would claim the user is
    // getting tracks they unticked, Off would claim they are getting none.
    let t = tree(&two_title_disc(), "All titles", 0.0);
    let title = nth(&t, "Title", 0);
    t.set_checked(title, true);
    let one_audio = nth(&t, "Audio", 0);
    *t.arena[one_audio].checked.borrow_mut() = false;
    assert_eq!(
        t.check_state(title),
        Check::Mixed,
        "a title with one of two audio tracks off must read Mixed"
    );
}

#[test]
fn the_video_row_does_not_drag_a_title_into_mixed() {
    // Video is uncheckable and its `checked` flag is meaningless, so folding it
    // into the tri-state would show Mixed for a title with everything ticked.
    // VERIFIED: `check_state` filters children by `checkable()`.
    let t = tree(&two_title_disc(), "All titles", 0.0);
    let title = nth(&t, "Title", 0);
    t.set_checked(title, true);
    let video = nth(&t, "Video", 0);
    assert!(!t.arena[video].checkable());
    *t.arena[video].checked.borrow_mut() = false;
    assert_eq!(
        t.check_state(title),
        Check::On,
        "an uncheckable Video row was folded into the title's tri-state"
    );
}

#[test]
fn a_title_with_a_single_audio_track_is_never_mixed() {
    // One checkable child can only be all-on or all-off; a Mixed glyph there
    // would be unreachable by any click.
    let t = tree(&disc(&[(5400.0, 1)]), "All titles", 0.0);
    let title = nth(&t, "Title", 0);
    for on in [true, false] {
        t.set_checked(title, on);
        assert_eq!(
            t.check_state(title),
            if on { Check::On } else { Check::Off },
            "a single-track title read as Mixed"
        );
    }
}

#[test]
fn a_leaf_row_shows_its_own_tick_and_nothing_else() {
    let t = tree(&two_title_disc(), "All titles", 0.0);
    let audio = nth(&t, "Audio", 0);
    assert!(t.arena[audio].children.is_empty());
    *t.arena[audio].checked.borrow_mut() = true;
    assert_eq!(t.check_state(audio), Check::On);
    *t.arena[audio].checked.borrow_mut() = false;
    assert_eq!(t.check_state(audio), Check::Off);
}

#[test]
fn ticking_a_title_cascades_to_its_streams() {
    // Ticking a title must actually select its tracks, not just paint the
    // parent: `ticked_streams` reads the CHILDREN.
    let t = tree(&two_title_disc(), "All titles", 0.0);
    let title = nth(&t, "Title", 0);
    t.set_checked(title, false);
    for &c in &t.arena[title].children {
        assert!(!*t.arena[c].checked.borrow(), "a stream survived an untick");
    }
    t.set_checked(title, true);
    for &c in &t.arena[title].children {
        assert!(*t.arena[c].checked.borrow(), "a stream missed the tick");
    }
}

// ══ how a View maps to rows ════════════════════════════════════════════════

#[test]
fn every_arena_row_becomes_exactly_one_view_row_in_order() {
    // A shell builds its control straight from `title_rows`; a dropped or
    // reordered row is a row the user cannot reach.
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    let v = app.view();
    assert_eq!(v.title_rows.len(), app.tree.arena.len());
    for (i, r) in v.title_rows.iter().enumerate() {
        assert_eq!(r.index, i, "row {i} carries the wrong arena index");
        assert_eq!(r.type_s, app.tree.arena[i].type_s);
        assert_eq!(r.desc, app.tree.arena[i].desc);
    }
}

#[test]
fn a_view_row_carries_a_tick_only_when_the_row_is_a_choice() {
    // `Row::check == None` is what both shells render as "no checkbox at all".
    // If it disagreed with `checkable()`, one shell would show a checkbox the
    // model refuses to change — the exact macOS bug the self-test was written
    // for.
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    for r in app.view().title_rows {
        assert_eq!(
            r.check.is_some(),
            app.tree.arena[r.index].checkable(),
            "row {} ({}) disagrees about carrying a checkbox",
            r.index,
            r.type_s
        );
    }
}

#[test]
fn a_view_row_reports_the_same_tri_state_the_tree_computed() {
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    // Force one of each state onto the screen at once.
    let t0 = nth(&app.tree, "Title", 0);
    let t1 = nth(&app.tree, "Title", 1);
    app.tree.set_checked(t0, true);
    *app.tree.arena[nth(&app.tree, "Audio", 0)]
        .checked
        .borrow_mut() = false;
    app.tree.set_checked(t1, false);
    let rows = app.view().title_rows;
    assert_eq!(rows[t0].check, Some(Check::Mixed));
    assert_eq!(rows[t1].check, Some(Check::Off));
    for r in &rows {
        assert_eq!(
            r.check,
            app.tree.arena[r.index]
                .checkable()
                .then(|| app.tree.check_state(r.index)),
            "row {} lost its tri-state on the way into the View",
            r.index
        );
    }
}

#[test]
fn view_rows_are_indented_disc_then_title_then_stream() {
    // The depth column is the ONLY thing telling a shell how to nest the tree.
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    let v = app.view();
    let depths: Vec<(&str, u8)> = v
        .title_rows
        .iter()
        .map(|r| (r.type_s.as_str(), r.depth))
        .collect();
    assert_eq!(
        depths,
        vec![
            ("Bluray disc", 0),
            ("Title", 1),
            ("Video", 2),
            ("Audio", 2),
            ("Audio", 2),
            ("Title", 1),
            ("Video", 2),
            ("Audio", 2),
        ]
    );
}

// ══ the shared depth → parent walk ═════════════════════════════════════════

#[test]
fn row_parents_rebuilds_the_disc_title_stream_hierarchy() {
    // Both shells rebuild a hierarchical control from the flat row list. This
    // is the walk they share.
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    let rows = app.view().title_rows;
    let parents = row_parents(&rows);
    assert_eq!(
        parents,
        vec![
            None,    // disc
            Some(0), // title 1 → disc
            Some(1), // video → title 1
            Some(1), // audio → title 1
            Some(1), // audio → title 1
            Some(0), // title 2 → disc
            Some(5), // video → title 2
            Some(5), // audio → title 2
        ]
    );
}

#[test]
fn a_stream_hangs_off_its_own_title_not_the_previous_one() {
    // The bug this guards: resetting the "current title" only on a new disc row
    // would file title 2's tracks under title 1.
    let mut app = App::new();
    app.tree = tree(
        &disc(&[(5400.0, 1), (600.0, 1), (300.0, 1)]),
        "All titles",
        0.0,
    );
    app.page = Page::Titles;
    let rows = app.view().title_rows;
    let parents = row_parents(&rows);
    for (i, r) in rows.iter().enumerate() {
        if r.depth == 2 {
            let p = parents[i].expect("a stream must have a parent");
            assert_eq!(rows[p].depth, 1, "stream {i} hung off a non-title row");
            assert_eq!(
                app.tree.arena[i].title_idx, app.tree.arena[p].title_idx,
                "stream {i} was filed under the wrong title"
            );
        }
    }
}

#[test]
fn row_parents_never_drops_a_row() {
    // A row the core decided to show must always be reachable. A malformed
    // list (a stream before any title) must put the orphan at the top level,
    // not swallow it — a silently vanishing row is the worse failure.
    let orphan = |depth: u8| Row {
        index: 0,
        depth,
        type_s: "Audio".into(),
        desc: "stray".into(),
        check: Some(Check::Off),
    };
    for rows in [vec![orphan(2)], vec![orphan(1)], vec![orphan(2), orphan(1)]] {
        let parents = row_parents(&rows);
        assert_eq!(parents.len(), rows.len(), "row_parents lost a row");
        for (i, p) in parents.iter().enumerate() {
            assert!(
                p.is_none() || p.unwrap() < i,
                "row {i} claims a parent that comes after it"
            );
        }
    }
}

#[test]
fn a_parent_always_precedes_its_children() {
    // Both shells add a child to an already-created parent, so a forward
    // reference would drop the row on the floor.
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    let rows = app.view().title_rows;
    for (i, p) in row_parents(&rows).into_iter().enumerate() {
        if let Some(p) = p {
            assert!(p < i, "row {i} hangs off row {p}, which comes later");
            assert_eq!(
                rows[p].depth + 1,
                rows[i].depth,
                "parent is not one level up"
            );
        }
    }
}

// ══ where a freshly-built tree is left standing ════════════════════════════

#[test]
fn a_rebuilt_tree_opens_on_the_ticked_title_not_the_last_one() {
    // The bug: the Windows shell opens every title to match the macOS outline,
    // and each expand scrolls its children into view, so a 97-title Blu-ray
    // left the user parked at the bottom of the list. The row to show is the
    // ticked one — which under "Longest title" is NOT row 0.
    let sc = disc(&[(600.0, 1), (300.0, 1), (5400.0, 1)]);
    let mut app = App::new();
    app.tree = tree(&sc, "Longest title", 0.0);
    app.page = Page::Titles;
    let rows = app.view().title_rows;

    let at = first_visible_row(&rows).expect("a populated tree must nominate a row");
    assert_eq!(
        rows[at].depth, 1,
        "expected a Title row, got {:?}",
        rows[at]
    );
    assert!(
        at > 0 && at < rows.len() - 1,
        "fixture must tick a title that is neither first nor last"
    );
    assert_eq!(
        rows[at].desc, "3.  playlist",
        "opened on a title other than the ticked one"
    );
}

#[test]
fn every_title_ticked_opens_at_the_top() {
    // Under "All titles" the first ticked row IS the first title, so the list
    // starts where it reads: at the top.
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    let rows = app.view().title_rows;
    assert_eq!(first_visible_row(&rows), Some(1), "{rows:?}");
}

#[test]
fn nothing_ticked_falls_back_to_the_first_row() {
    // A preset can leave everything unticked (min-duration filters the lot).
    // "No answer" must not mean "leave it wherever the expand loop ended".
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    for n in app.tree.arena.iter() {
        *n.checked.borrow_mut() = false;
    }
    let rows = app.view().title_rows;
    assert_eq!(first_visible_row(&rows), Some(0));

    assert_eq!(first_visible_row(&[]), None, "an empty tree has no row");
}

// ══ which titles are ticked ════════════════════════════════════════════════

#[test]
fn the_default_selection_setting_decides_what_starts_ticked() {
    // Three modes, three different answers, all on the same disc. "Main film
    // only" means title 1; "All titles" means every title; "Longest title"
    // means the longest, which here is NOT title 1.
    let sc = disc(&[(600.0, 1), (5400.0, 1), (900.0, 1)]);

    let main = tree(&sc, "Main film only", 0.0);
    assert_eq!(main.ticked_titles(), vec![0]);

    let all = tree(&sc, "All titles", 0.0);
    assert_eq!(all.ticked_titles(), vec![0, 1, 2]);

    let longest = tree(&sc, "Longest title", 0.0);
    assert_eq!(
        longest.ticked_titles(),
        vec![1],
        "\"Longest title\" ticked something other than the longest title"
    );
}

#[test]
fn an_unrecognized_selection_setting_falls_back_to_the_main_film() {
    // A settings file from a newer build must not open a disc with nothing
    // ticked, which would make Rip refuse with "select a title first".
    let sc = two_title_disc();
    assert_eq!(tree(&sc, "Every third title", 0.0).ticked_titles(), vec![0]);
    assert_eq!(tree(&sc, "", 0.0).ticked_titles(), vec![0]);
}

#[test]
fn ticked_titles_are_canonical_disc_indices_not_tree_positions() {
    // THE bug this guards: the minimum-length filter removes short titles from
    // the tree, so the third surviving row can be disc title 5. The engine
    // selects by disc index, so returning a row position would rip the wrong
    // film.
    let sc = disc(&[(30.0, 1), (30.0, 1), (5400.0, 1), (30.0, 1), (3600.0, 1)]);
    let t = tree(&sc, "All titles", 300.0);
    assert_eq!(
        t.title_count(),
        2,
        "the filter should have left exactly the two long titles"
    );
    assert_eq!(
        t.ticked_titles(),
        vec![2, 4],
        "ticked titles must be canonical disc indices, not tree positions"
    );
    // And the tree positions really are different, or the test proves nothing.
    let positions: Vec<usize> = t
        .arena
        .iter()
        .enumerate()
        .filter(|(_, n)| n.type_s == "Title")
        .map(|(i, _)| i)
        .collect();
    assert_ne!(positions, vec![2, 4], "fixture failed to separate the two");
}

#[test]
fn the_disc_row_never_counts_as_a_ticked_title() {
    // The root carries `title_idx == usize::MAX`; leaking it into the
    // selection would hand the engine a nonsense title number.
    let t = tree(&two_title_disc(), "All titles", 0.0);
    t.set_all(true);
    assert_eq!(t.ticked_titles(), vec![0, 1]);
    assert!(!t.ticked_titles().contains(&usize::MAX));
}

#[test]
fn ticked_streams_reports_pids_and_whether_the_user_narrowed_them() {
    // `explicit_streams` is what tells the engine "the user made a choice";
    // reporting it when everything is on would turn a default rip into a
    // filtered one.
    let t = tree(&two_title_disc(), "All titles", 0.0);
    t.set_all(true);
    let (audio, subs, explicit) = t.ticked_streams();
    assert_eq!(audio.len(), 3, "every audio PID should be selected");
    assert!(subs.is_empty(), "the fixture has no subtitle tracks");
    assert!(!explicit, "everything ticked is not a narrowed selection");

    let one = nth(&t, "Audio", 0);
    *t.arena[one].checked.borrow_mut() = false;
    let (audio, _, explicit) = t.ticked_streams();
    assert_eq!(audio.len(), 2);
    assert!(explicit, "unticking a track must be reported as explicit");
    assert!(
        !audio.contains(&t.arena[one].pid.unwrap()),
        "the unticked track's PID is still being sent to the engine"
    );
}

#[test]
fn unticking_every_track_is_an_explicit_empty_selection() {
    // A video-only extract is legal, and it must be distinguishable from "the
    // user chose nothing, give them everything".
    let t = tree(&two_title_disc(), "All titles", 0.0);
    t.set_all(false);
    let (audio, subs, explicit) = t.ticked_streams();
    assert!(audio.is_empty() && subs.is_empty());
    assert!(
        explicit,
        "an all-off selection must read as explicit, or the engine will \
         silently include every track"
    );
}

// ══ the minimum-title-length filter ════════════════════════════════════════

#[test]
fn short_titles_are_hidden_along_with_their_streams() {
    let sc = disc(&[(5400.0, 2), (30.0, 2)]);
    let t = tree(&sc, "All titles", 300.0);
    assert_eq!(t.title_count(), 1);
    // The hidden title's tracks must go with it — an orphan Audio row under
    // the wrong title is worse than no row.
    assert_eq!(
        t.arena.iter().filter(|n| n.type_s == "Audio").count(),
        2,
        "the short title's tracks survived the filter"
    );
}

#[test]
fn the_filter_never_empties_the_list() {
    // A disc of nothing but short titles must still show them: an empty tree
    // reads as "this disc has nothing on it", which is a lie.
    let sc = disc(&[(30.0, 1), (45.0, 1)]);
    let t = tree(&sc, "All titles", 3600.0);
    assert_eq!(
        t.title_count(),
        2,
        "an aggressive minimum length hid every title"
    );
}

#[test]
fn a_title_with_no_known_duration_is_never_hidden() {
    // Duration 0.0 means "unknown", not "zero seconds". Hiding on missing
    // information would make a scan that failed to time its titles look empty.
    let mut sc = disc(&[(5400.0, 1)]);
    sc.rows.extend(title_block(1, 0.0, 1));
    sc.title_count = 2;
    sc.video_codecs.push("H.264".into());
    let t = tree(&sc, "All titles", 300.0);
    assert_eq!(t.title_count(), 2, "a title of unknown length was hidden");
}

// ══ what the output file is called ═════════════════════════════════════════

#[test]
fn the_output_file_is_named_after_the_source_and_the_first_ticked_title() {
    // The Information panel shows this path while the rip runs, so it has to
    // be the file that actually lands on disk.
    assert_eq!(
        output_file_name(
            "/media/Greenland.iso",
            "/out",
            "Selected titles → MKV",
            Some(0)
        ),
        "/out/Greenland_t1.mkv"
    );
    // The title number is 1-based, matching `freemkv -t N` and the tree labels.
    assert_eq!(
        output_file_name(
            "/media/Greenland.iso",
            "/out",
            "Selected titles → MKV",
            Some(4)
        ),
        "/out/Greenland_t5.mkv"
    );
}

#[test]
fn the_output_extension_follows_the_chosen_container() {
    // Showing "…_t1.mkv" while ripping an MP4 was a reported defect: the panel
    // named a file that never appeared.
    for (format, ext) in [
        ("Selected titles → MKV", "mkv"),
        ("Selected titles → MP4", "mp4"),
        ("Selected titles → M2TS", "m2ts"),
    ] {
        assert_eq!(
            output_file_name("/media/Disc.iso", "/out", format, Some(0)),
            format!("/out/Disc_t1.{ext}"),
            "{format} produced the wrong extension"
        );
        // And it must agree with the caption on the progress bar, which is
        // built from the same format string by a different function.
        assert_eq!(
            container_label(format).to_ascii_lowercase(),
            ext,
            "the progress caption and the filename disagree for {format}"
        );
    }
}

#[test]
fn a_run_with_no_ticked_title_still_names_the_file_the_engine_will_write() {
    // A container source has no title rows, so nothing is "ticked"; the engine
    // rips the single title and numbers it 1.
    assert_eq!(
        output_file_name("/media/clip.mkv", "/out", "Selected titles → MKV", None),
        "/out/clip_t1.mkv"
    );
}

#[test]
fn a_source_with_no_stem_still_produces_a_usable_name() {
    // Never a path ending in "_t1.mkv" with nothing in front of it, and never
    // a panic on an odd source string.
    let n = output_file_name("/", "/out", "Selected titles → MKV", Some(0));
    assert_eq!(n, "/out/output_t1.mkv");
}

// ══ the Information panel ══════════════════════════════════════════════════

#[test]
fn the_information_panel_has_a_label_for_every_value() {
    // The shells zip labels against values positionally; a mismatch shifts
    // every row's meaning by one.
    let rows = InfoRows::starting("/media/Disc.iso", "/out/Disc_t1.mkv");
    assert_eq!(InfoRows::labels().len(), rows.as_array().len());
}

#[test]
fn no_information_row_is_ever_blank() {
    // A blank field reads as a broken panel (reported). An unknown value is an
    // em dash, which reads as "not known yet".
    let rows = InfoRows::starting("/no/such/file.iso", "/out/file_t1.mkv");
    for (label, value) in InfoRows::labels().iter().zip(rows.as_array()) {
        assert!(!value.is_empty(), "the {label:?} row is blank");
    }
    // Specifically: a source that does not exist has no size, and says so.
    assert_eq!(rows.source_size, "—");
}

// ══ settings dropdowns ═════════════════════════════════════════════════════

#[test]
fn every_settings_dropdown_stores_a_canonical_value_the_code_matches_on() {
    // The label is translated; the stored value is not. These canonical
    // strings are compared verbatim elsewhere (`selection` in `Tree::from_scan`,
    // `rip_mode` in `start_run`, `key_source` in `KeyConfig`), so renaming one
    // here silently disables the feature it selects.
    let expect: &[(&str, &[&str])] = &[
        (
            "selection",
            &["Main film only", "All titles", "Longest title"],
        ),
        ("rip_mode", &["Multi-pass", "Single pass"]),
        (
            "key_source",
            &[
                "Local keydb only",
                "Online key service only",
                "keydb, then online",
            ],
        ),
        ("log_level", &["Quiet", "Normal", "Verbose", "Debug"]),
    ];
    for (key, want) in expect {
        let got: Vec<&str> = enum_options(key).iter().map(|(c, _)| *c).collect();
        assert_eq!(&got, want, "{key} option values changed");
    }
}

#[test]
fn the_selection_dropdown_offers_exactly_the_modes_the_tree_implements() {
    // A mode on offer that `Tree::from_scan` does not implement falls into its
    // catch-all and silently behaves as "Main film only" — a setting that
    // appears to work and does not.
    let sc = disc(&[(600.0, 1), (5400.0, 1)]);
    let main = tree(&sc, "Main film only", 0.0).ticked_titles();
    let distinct: Vec<Vec<usize>> = enum_options("selection")
        .iter()
        .map(|(canon, _)| tree(&sc, canon, 0.0).ticked_titles())
        .collect();
    assert_eq!(distinct.len(), 3);
    assert_eq!(distinct[0], main);
    assert_ne!(
        distinct[1], main,
        "\"All titles\" behaves identically to \"Main film only\""
    );
    assert_ne!(
        distinct[2], main,
        "\"Longest title\" behaves identically to \"Main film only\""
    );
}

#[test]
fn the_language_dropdown_offers_every_shipped_locale_and_nothing_else() {
    use std::collections::BTreeSet;
    let offered: BTreeSet<&str> = enum_options("language").iter().map(|(c, _)| *c).collect();
    let from_table: BTreeSet<&str> = LOCALES.iter().map(|(_, c)| *c).collect();
    assert_eq!(
        offered, from_table,
        "the language dropdown and LOCALES disagree"
    );
    // Every offered code must normalize back to itself, or picking a language
    // would store a value the startup path then resolves to "auto".
    for (code, _) in enum_options("language") {
        assert_eq!(locale_code(code), code, "{code} does not round-trip");
    }
}

#[test]
fn every_dropdown_label_is_a_real_translation_not_a_lookup_key() {
    // `strings::get` returns the dotted key verbatim on a miss, so a typo'd or
    // missing key shows up in the UI as "gui.set.sel_main". Nothing in a
    // dropdown may look like that.
    for key in ["selection", "rip_mode", "key_source", "log_level"] {
        for (canon, label) in enum_options(key) {
            assert!(!label.is_empty(), "{key}/{canon} has an empty label");
            assert!(
                !(label.starts_with("gui.") && !label.contains(' ')),
                "{key}/{canon} shows the lookup key {label:?} — the string is \
                 missing from the locale catalog"
            );
        }
    }
}

#[test]
fn a_key_with_no_fixed_option_list_is_not_a_dropdown() {
    // Both shells use an empty result to mean "this is a free-form field".
    // A spurious arm here would make a text field persist a menu index.
    for key in [
        "dest_dir",
        "filename_template",
        "max_passes",
        "container",
        "",
    ] {
        assert!(
            enum_options(key).is_empty(),
            "{key} is being treated as an enum dropdown by both shells"
        );
    }
}

// ══ when Rip is enabled, and what it refuses ═══════════════════════════════

#[test]
fn rip_is_offered_only_once_there_is_something_to_rip() {
    let mut app = App::new();
    assert!(!app.view().can_run, "Rip was offered with no source open");
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.page = Page::Titles;
    assert!(
        !app.view().can_run,
        "Rip was offered for a tree with no source path behind it"
    );
    app.source = "/media/Disc.iso".into();
    assert!(app.view().can_run, "Rip stayed disabled with a source open");
}

#[test]
fn rip_refuses_a_disc_with_every_title_unticked() {
    // An empty title list means "main movie" to the engine, so silently
    // accepting it would rip something the user explicitly deselected.
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.source = "/media/Disc.iso".into();
    app.output_dir = "/out".into();
    app.page = Page::Titles;
    app.tree.set_all(false);
    let before = app.log.len();
    app.dispatch(Cmd::Run);
    assert!(!app.running(), "a rip started with no title selected");
    assert!(
        app.log.len() > before,
        "the refusal was silent — the user gets no explanation"
    );
    assert!(
        matches!(app.log.last().map(|l| l.kind), Some(LogKind::Notice)),
        "the refusal was not logged as a notice"
    );
}

#[test]
fn rip_refuses_without_an_output_folder_and_says_so() {
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.source = "/media/Disc.iso".into();
    app.output_dir = "   ".into();
    app.page = Page::Titles;
    app.dispatch(Cmd::Run);
    assert!(!app.running());
    assert_eq!(app.page, Page::Titles, "the page moved on despite refusing");
    assert!(matches!(
        app.log.last().map(|l| l.kind),
        Some(LogKind::Notice)
    ));
}

// ══ what the log line says ═════════════════════════════════════════════════

#[test]
fn an_mp4_that_cannot_hold_the_ticked_titles_is_flagged_before_the_rip() {
    // Named codecs in the warning, not a bare "wrong format": the user has to
    // know WHICH titles are the problem. VERIFIED against `MP4_VIDEO`, which
    // mirrors libfreemkv's mux gate (H.264/HEVC only).
    let mut app = App::new();
    app.tree = tree(&disc(&[(5400.0, 1), (600.0, 1)]), "All titles", 0.0);
    app.video_codecs = vec!["MPEG-2".into(), "H.264".into()];
    app.format = "Selected titles → MP4".into();
    let msg = app
        .container_mismatch()
        .expect("an MPEG-2 title in an MP4 must be flagged");
    assert!(msg.contains("MPEG-2"), "the warning names no codec: {msg}");
    assert!(
        !msg.contains("H.264"),
        "the warning blames a codec MP4 can hold: {msg}"
    );

    // Untick the MPEG-2 title and the objection must disappear — a warning
    // that will not clear teaches the user to ignore it.
    app.tree.set_checked(nth(&app.tree, "Title", 0), false);
    assert_eq!(app.container_mismatch(), None);
}

#[test]
fn a_container_the_source_can_actually_use_raises_no_objection() {
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.video_codecs = vec!["H.264".into(), "HEVC".into()];
    for f in [
        "Selected titles → MKV",
        "Selected titles → MP4",
        "Selected titles → M2TS",
    ] {
        app.format = f.into();
        assert_eq!(
            app.container_mismatch(),
            None,
            "{f} was wrongly objected to"
        );
    }
}

#[test]
fn mp4_is_withdrawn_rather_than_offered_and_refused() {
    // A choice that always fails is worse than no choice.
    let mut app = App::new();
    app.video_codecs = vec!["MPEG-2".into()];
    assert!(!app.mp4_possible());
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.source = "/media/Disc.iso".into();
    app.page = Page::Titles;
    let offered = app.view().formats.concat();
    assert!(
        !offered.iter().any(|f| f.contains("MP4")),
        "MP4 is still on offer for a source that cannot produce one: {offered:?}"
    );
    assert!(
        offered.iter().any(|f| f.contains("MKV")),
        "MKV vanished too"
    );
}

#[test]
fn unknown_codecs_never_withdraw_an_option() {
    // Missing information must not remove a capability — the container source
    // path reports no codecs at all.
    let app = App::new();
    assert!(app.mp4_possible(), "no codec information blocked MP4");
}

#[test]
fn a_log_line_carries_the_severity_a_shell_renders_it_with() {
    // The two shells mark a Notice differently (colour on macOS, a gutter
    // character on Windows) but both read the SAME `kind`, so the kind has to
    // be right at the source.
    let mut app = App::new();
    app.say(LogKind::Notice, "a problem");
    app.say(LogKind::Detail, "chatter");
    let kinds: Vec<LogKind> = app.view().log.iter().map(|l| l.kind).collect();
    assert_eq!(kinds[kinds.len() - 2], LogKind::Notice);
    assert_eq!(kinds[kinds.len() - 1], LogKind::Detail);
}

#[test]
fn clearing_the_log_empties_what_the_shells_render() {
    let mut app = App::new();
    app.say(LogKind::Detail, "something");
    assert!(!app.view().log.is_empty());
    app.dispatch(Cmd::ClearLog);
    assert!(app.view().log.is_empty(), "Clear log left lines behind");
    assert!(
        !app.view().log_hidden,
        "Clear log must not also hide the pane"
    );
}

// ══ pages ══════════════════════════════════════════════════════════════════

#[test]
fn dismissing_the_result_returns_to_the_titles_when_a_disc_is_still_open() {
    // Dropping back to the empty page after a rip would look as if the app had
    // closed the disc.
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.source = "/media/Disc.iso".into();
    app.result_summary = "2 title(s) written".into();
    app.page = Page::Result;
    app.dismiss_result();
    assert_eq!(app.page, Page::Titles);
}

#[test]
fn dismissing_the_result_returns_to_the_empty_page_when_nothing_is_open() {
    let mut app = App::new();
    app.result_summary = "2 title(s) written".into();
    app.page = Page::Result;
    app.dismiss_result();
    assert_eq!(app.page, Page::Empty);
}

#[test]
fn the_second_progress_bar_appears_only_for_a_multi_title_run() {
    // Two identical bars for a single title is a reported defect.
    let mut app = App::new();
    app.run_titles = 1;
    assert!(!app.view().show_overall_bar);
    app.run_titles = 2;
    assert!(app.view().show_overall_bar);
}

#[test]
fn the_progress_captions_name_the_container_the_user_chose() {
    // "Saving to MKV file" while writing an MP4 was a reported defect.
    let mut app = App::new();
    for (format, word) in [
        ("Selected titles → MP4", "MP4"),
        ("Selected titles → M2TS", "M2TS"),
        ("Selected titles → MKV", "MKV"),
    ] {
        app.format = format.into();
        let v = app.view();
        assert!(
            v.saving_current.contains(word),
            "per-title caption {:?} does not name {word}",
            v.saving_current
        );
        assert!(
            v.saving_overall.contains(word),
            "overall caption {:?} does not name {word}",
            v.saving_overall
        );
    }
}

// ── the decisions `start_run` makes on the way to the engine ────────────────

/// `--multipass` is hours of extra drive time in one direction and a silently
/// skipped recovery in the other. Both halves of the condition were
/// unconstrained.
#[test]
fn multipass_needs_both_the_mode_and_a_pass_budget() {
    assert!(wants_multipass("Multi-pass", 5));
    assert!(wants_multipass("Multi-pass", 1));
    // Zero passes is not multipass, whatever the mode says.
    assert!(!wants_multipass("Multi-pass", 0));
    assert!(!wants_multipass("Single pass", 5));
    assert!(!wants_multipass("Single pass", 0));
    // An unrecognised mode is never treated as multipass.
    assert!(!wants_multipass("", 5));
    assert!(!wants_multipass("multi-pass", 5));
}

/// Ciphertext passthrough only applies to a whole-disc ISO. Forwarded to a mux
/// it writes encrypted bytes into a container that claims to hold video.
#[test]
fn raw_only_applies_to_an_iso_output() {
    assert!(raw_applies(true, true));
    assert!(!raw_applies(true, false));
    assert!(!raw_applies(false, true));
    assert!(!raw_applies(false, false));
}

/// Unticking every track is allowed but never silent — a file with no audio is
/// usually an accident. "Made no choice" is a different thing from "chose
/// nothing" and must not warn.
#[test]
fn a_video_only_selection_is_recognised_but_no_choice_at_all_is_not() {
    assert!(is_video_only_selection(true, &[], &[]));
    assert!(!is_video_only_selection(false, &[], &[]));
    assert!(!is_video_only_selection(true, &[4352], &[]));
    assert!(!is_video_only_selection(true, &[], &[4608]));
    assert!(!is_video_only_selection(true, &[4352], &[4608]));
}

/// A rip that ticks no audio and no subtitle track starts, but SAYS so first.
/// The notice is the whole point: the guard is not a refusal, so without the
/// log line an accidental silent movie is discovered after the rip.
#[test]
fn a_video_only_rip_warns_before_it_starts() {
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.source = "/media/Disc.iso".into();
    app.output_dir = "/out".into();
    app.page = Page::Titles;
    // Tick the titles, untick every stream row under them.
    app.tree.set_all(true);
    for i in 0..app.tree.arena.len() {
        if app.tree.arena[i].type_s == "Audio" || app.tree.arena[i].type_s == "Subtitle" {
            app.tree.set_checked(i, false);
        }
    }
    let (audio, sub, explicit) = app.tree.ticked_streams();
    assert!(
        is_video_only_selection(explicit, &audio, &sub),
        "the fixture did not produce a video-only selection: {audio:?} {sub:?} explicit={explicit}"
    );

    app.dispatch(Cmd::Run);
    let notice = freemkv::strings::get("gui.log.video_only_warning");
    assert!(
        app.log.iter().any(|l| l.text == notice),
        "no video-only notice in the log: {:?}",
        app.log.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
}

/// `--raw` with a non-ISO output is dropped, and the user is told. Silently
/// forwarding it writes ciphertext into an MKV; silently dropping it leaves the
/// user believing they got an encrypted image.
#[test]
fn raw_on_a_non_iso_output_says_it_was_ignored() {
    let mut app = App::new();
    app.tree = tree(&two_title_disc(), "All titles", 0.0);
    app.source = "/media/Disc.iso".into();
    app.output_dir = "/out".into();
    app.page = Page::Titles;
    app.tree.set_all(true);
    app.settings.raw = true;
    app.format = "Selected titles → MKV".into();
    assert!(
        !app.format.contains("ISO image"),
        "fixture must not be an ISO output"
    );

    app.dispatch(Cmd::Run);
    let notice = freemkv::strings::get("gui.log.raw_iso_only");
    assert!(
        app.log.iter().any(|l| l.text == notice),
        "no raw-is-iso-only notice in the log: {:?}",
        app.log.iter().map(|l| &l.text).collect::<Vec<_>>()
    );
}

/// The result heading is read from the TYPED verdict, never from the summary
/// text.
///
/// This is the regression that shipped once already: the heading was chosen by
/// substring-matching the engine's English summary, a message was reworded so
/// it read "fail" rather than "failed", and an undecryptable disc rendered
/// under the success heading. A failed run must never share a heading with a
/// completed one, whatever the summary happens to say — including when the
/// summary contains the word "finished" or nothing at all.
#[test]
fn the_result_heading_comes_from_the_verdict_not_the_summary_text() {
    use freemkv::engine::RunOutcome;

    let heading = |outcome, summary: &str| {
        let mut app = App::new();
        app.page = Page::Result;
        app.result_outcome = outcome;
        app.result_summary = summary.into();
        app.view().result_heading
    };

    let finished = heading(RunOutcome::Completed, "2 title(s) written to /out");
    let failed = heading(RunOutcome::Failed, "The disc has no decryption key");
    let cancelled = heading(RunOutcome::Cancelled, "Cancelled — 1 of 3 completed");

    assert_ne!(failed, finished, "a failed rip shares the success heading");
    assert_ne!(cancelled, finished, "a cancelled rip reads as finished");
    assert_ne!(cancelled, failed);

    // And the summary text cannot move any of them. These are the exact
    // wordings that defeated the old substring match.
    for misleading in [
        "Recovery aborted — too much unreadable data to mux a complete title",
        "the mux did not fail cleanly",
        "2 title(s) written to /out",
        "finished",
        "",
    ] {
        assert_eq!(
            heading(RunOutcome::Failed, misleading),
            failed,
            "summary {misleading:?} changed the FAILED heading"
        );
        assert_eq!(
            heading(RunOutcome::Completed, misleading),
            finished,
            "summary {misleading:?} changed the COMPLETED heading"
        );
    }
}

/// The whole GUI→engine seam, end to end, with no disc.
///
/// `start_rip` could be replaced with `()` and nothing in the suite noticed:
/// nothing observed that the worker ran, that `finished` was ever set, or that
/// a summary was filed. A `finished` that never arrives is not a cosmetic
/// failure — `ui::tick` polls it, so the window shows a rip in progress
/// FOREVER with no error, which is the exact hang the `SignalDone` guard was
/// written to contain (its doc records two hand-rolled copies of the pattern
/// that both had the bug).
///
/// A nonexistent source fails fast, so no fixture is needed.
#[test]
fn a_failed_run_always_finishes_and_is_reported_as_failed() {
    use freemkv::engine::{KeyConfig, RipRequest, RunOutcome, RunState, start_rip};
    use std::sync::Arc;

    let state = Arc::new(RunState::default());
    start_rip(
        RipRequest {
            source: "/definitely/not/a/real/file.iso".into(),
            dest_dir: std::env::temp_dir()
                .join("fmkv-gui-model")
                .display()
                .to_string(),
            titles: vec![],
            format: "Selected titles → MKV".into(),
            audio_pids: vec![],
            sub_pids: vec![],
            explicit_streams: false,
            raw: false,
            force: false,
            filename_template: String::new(),
            decrypt_threads: 0,
            multipass: false,
            max_passes: 0,
            abort_lost_secs: 0,
            keep_iso: false,
            auto_eject: false,
            keys: KeyConfig::default(),
        },
        state.clone(),
    );

    // Bounded wait — a worker that never sets `finished` is the defect.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !state.finished.load(std::sync::atomic::Ordering::Relaxed) {
        assert!(
            std::time::Instant::now() < deadline,
            "the rip worker never set `finished` — this is the permanent \
             in-progress hang, not a slow test"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    assert_eq!(
        *state.outcome.lock().unwrap(),
        RunOutcome::Failed,
        "a rip of a nonexistent source reported something other than Failed"
    );
    assert!(
        !state.summary.lock().unwrap().is_empty(),
        "the result page would have shown a blank summary"
    );
}

// ── policies the two shells used to own separately ─────────────────────────

/// Clicking a partly-ticked title selects ALL of it, and clicking again clears
/// it.
///
/// The direction is core policy. Both shells carried a comment saying "the core
/// owns cascade + tri-state; the shell only reports which row was clicked" and
/// both were wrong: each computed its own answer and the two disagreed. Windows
/// read `Off | Mixed` as "turn on"; macOS read the NSButton's mixed state
/// (`-1`) as "turn off". Same disc, same click, opposite results. The Win32
/// self-test harness implemented the rule a third time, so it could never have
/// caught the divergence.
#[test]
fn clicking_a_partly_ticked_title_selects_all_of_it() {
    let t = tree(&two_title_disc(), "All titles", 0.0);
    let title = t
        .arena
        .iter()
        .position(|n| n.type_s == "Title" && !n.children.is_empty())
        .expect("the fixture has a title with streams");

    // Make it Mixed: title on, one stream off.
    t.set_checked(title, true);
    let stream = t.arena[title]
        .children
        .iter()
        .copied()
        .find(|&c| t.arena[c].checkable())
        .expect("the title has a checkable stream");
    t.set_checked(stream, false);
    assert_eq!(t.check_state(title), Check::Mixed, "fixture is not mixed");

    // Mixed -> fully on.
    t.toggle(title);
    assert_eq!(
        t.check_state(title),
        Check::On,
        "a click on a partly-ticked title must select all of it"
    );

    // On -> off. A second click has to undo the first, or the control is a
    // one-way door.
    t.toggle(title);
    assert_eq!(t.check_state(title), Check::Off);

    // Off -> on.
    t.toggle(title);
    assert_eq!(t.check_state(title), Check::On);
}

/// Opening a source that cannot offer the current format moves the MODEL, not
/// just the dropdown.
///
/// `View` publishes `format` and `formats` as two independent fields and
/// nothing kept them in agreement. Pick MP4 on an H.264 disc, then open an
/// MPEG-2 DVD: MP4 leaves the offered list, and the Win32 shell reconciled by
/// snapping its dropdown to the first entry and leaving the model alone. The
/// user READ "MKV", pressed Run, and the engine was handed MP4 — which fails at
/// mux time with E9048, after the drive has already been read.
#[test]
fn a_format_the_source_cannot_offer_is_never_left_selected() {
    // Pure rule first.
    let offered = output_formats(true, false);
    assert!(!format_is_offered("Selected titles → MP4", &offered));
    assert!(format_is_offered(
        reconcile_format("Selected titles → MP4", &offered),
        &offered
    ));
    // A still-available choice is kept exactly as it was.
    let keep = offered[0][0];
    assert_eq!(reconcile_format(keep, &offered), keep);

    // And the View can never publish a format its own list does not contain.
    // An MPEG-2 source withdraws MP4; the standing preference is still MP4.
    let mut app = App::new();
    app.source = "/media/mpeg2.iso".into();
    app.video_codecs = vec!["MPEG-2".to_string()];
    app.format = "Selected titles → MP4".into();
    assert!(!app.mp4_possible(), "fixture must withdraw MP4");

    let v = app.view();
    assert!(
        format_is_offered(&v.format, &v.formats),
        "the view offers {:?} but publishes {:?}",
        v.formats,
        v.format
    );
    assert_ne!(v.format, "Selected titles → MP4");
    // The rip must agree with what the user was shown, not with the stale
    // preference — this is the value that reaches the engine.
    assert_eq!(app.effective_format(), v.format);

    // A source that CAN offer MP4 keeps the preference untouched.
    let mut ok = App::new();
    ok.source = "/media/h264.iso".into();
    ok.video_codecs = vec!["H.264".to_string()];
    ok.format = "Selected titles → MP4".into();
    assert!(ok.mp4_possible());
    assert_eq!(ok.view().format, "Selected titles → MP4");
    assert_eq!(ok.effective_format(), "Selected titles → MP4");
}

// ══ preferred languages ════════════════════════════════════════════════════
//
// The user's request, verbatim: "German & Spanish audio, only German subtitles,
// and forced only if in English." That is THREE independent sets, and each of
// the tests below pins one property of that reading which a plausible
// simplification would break.

/// A one-title disc whose streams carry language tags. `(type, lang, forced)`,
/// in stream order; PIDs are assigned from 0x1100 in that order so a test can
/// name the stream it expects by position.
fn tagged_disc(streams: &[(&str, &str, bool)]) -> Scanned {
    let mut t = row("Title", "1.  playlist", 1, true, 0);
    t.duration_secs = 5400.0;
    let mut rows = vec![
        row("Bluray disc", "TEST_DISC", 0, false, usize::MAX),
        t,
        row("Video", "H.264  1080p", 2, false, 0),
    ];
    for (i, (ty, lang, forced)) in streams.iter().enumerate() {
        let mut r = row(ty, &format!("{ty}  {lang}"), 2, true, 0);
        r.pid = Some(0x1100 + i as u16);
        r.lang = (*lang).into();
        r.forced = *forced;
        rows.push(r);
    }
    Scanned {
        label: "TEST_DISC".into(),
        rows,
        key_summary: "keys: none needed".into(),
        title_count: 1,
        video_codecs: vec!["H.264".into()],
        details: vec![],
    }
}

/// PID of the `i`th stream of `tagged_disc`.
fn spid(i: u16) -> u16 {
    0x1100 + i
}

/// The audio set keeps EVERY listed language at once — it is a set, not a
/// first-match chain and not a single preferred language. "German & Spanish
/// audio" must tick both German and Spanish and leave English alone.
#[test]
fn the_audio_preference_keeps_several_languages_at_once() {
    let sc = tagged_disc(&[
        ("Audio", "deu", false),
        ("Audio", "spa", false),
        ("Audio", "eng", false),
    ]);
    let prefs = LangPrefs::parse("German, Spanish", "", "");
    let t = tree_prefs(&sc, "Main film only", &prefs);

    let (mut audio, subs, explicit) = t.ticked_streams();
    audio.sort_unstable();
    assert_eq!(
        audio,
        vec![spid(0), spid(1)],
        "both German AND Spanish audio must start ticked, and English must not"
    );
    assert!(subs.is_empty(), "the fixture has no subtitles");
    assert!(
        explicit,
        "narrowing the tracks must reach the rip request as an explicit selection"
    );

    // The title itself stays selected — the preference narrows STREAMS, it
    // never unselects the title the user asked to rip.
    assert_eq!(t.ticked_titles(), vec![0]);
    // ...and it reads as a partial selection in the UI, so the user can SEE
    // that a choice was made for them and undo it.
    assert_eq!(t.check_state(nth(&t, "Title", 0)), Check::Mixed);
}

/// The forced-subtitle set is its OWN set, not a filter applied on top of the
/// subtitle set. "Only German subtitles, and forced only if in English" keeps
/// the German non-forced subtitle and the ENGLISH forced one — a German forced
/// subtitle is not wanted, and an English full subtitle is not either.
#[test]
fn the_forced_subtitle_set_is_honoured_independently_of_the_subtitle_set() {
    let sc = tagged_disc(&[
        ("Subtitles", "deu", false), // wanted: German, not forced
        ("Subtitles", "eng", false), // unwanted: English full subtitles
        ("Subtitles", "deu", true),  // unwanted: forced, but not English
        ("Subtitles", "eng", true),  // wanted: forced English
    ]);
    let prefs = LangPrefs::parse("", "German", "English");
    let t = tree_prefs(&sc, "Main film only", &prefs);

    let (audio, mut subs, _) = t.ticked_streams();
    subs.sort_unstable();
    assert!(audio.is_empty(), "the fixture has no audio");
    assert_eq!(
        subs,
        vec![spid(0), spid(3)],
        "expected the German non-forced subtitle and the English FORCED one"
    );

    // The two sides are genuinely independent: swapping them swaps the result.
    let swapped = tree_prefs(
        &sc,
        "Main film only",
        &LangPrefs::parse("", "English", "German"),
    );
    let (_, mut sw, _) = swapped.ticked_streams();
    sw.sort_unstable();
    assert_eq!(sw, vec![spid(1), spid(2)]);
}

/// A disc that simply does not carry the preferred language falls back to
/// today's behaviour FOR THAT CATEGORY — never to an empty class. A rip that
/// silently ships without audio because the disc is Japanese-only is the worst
/// possible outcome of a convenience default.
#[test]
fn a_language_the_disc_lacks_falls_back_to_keeping_that_whole_class() {
    let sc = tagged_disc(&[
        ("Audio", "jpn", false),
        ("Audio", "kor", false),
        ("Subtitles", "deu", false),
        ("Subtitles", "eng", false),
        ("Subtitles", "eng", true),
    ]);
    // German/Spanish audio is not on this disc; German subtitles are.
    let prefs = LangPrefs::parse("German, Spanish", "German", "English");
    let t = tree_prefs(&sc, "Main film only", &prefs);

    let (mut audio, mut subs, _) = t.ticked_streams();
    audio.sort_unstable();
    subs.sort_unstable();
    assert_eq!(
        audio,
        vec![spid(0), spid(1)],
        "no preferred audio on the disc: keep EVERY audio track, as before"
    );
    // The fallback is per category, not disc-wide: the subtitle sets matched,
    // so they are still honoured.
    assert_eq!(
        subs,
        vec![spid(2), spid(4)],
        "the subtitle categories matched and must NOT have fallen back"
    );

    // An unresolvable tag (a typo) is treated the same way — it cannot strip a
    // class either.
    let typo = tree_prefs(
        &sc,
        "Main film only",
        &LangPrefs::parse("Klingonish", "", ""),
    );
    let (mut ta, _, _) = typo.ticked_streams();
    ta.sort_unstable();
    assert_eq!(ta, vec![spid(0), spid(1)]);
}

/// No preference set is EXACTLY today's behaviour: every stream of a checked
/// title checked, every stream of an unchecked title clear.
#[test]
fn an_empty_preference_behaves_exactly_as_before() {
    let sc = tagged_disc(&[
        ("Audio", "deu", false),
        ("Audio", "eng", false),
        ("Subtitles", "eng", true),
    ]);
    let before = tree(&sc, "Main film only", 0.0);
    let after = tree_prefs(&sc, "Main film only", &LangPrefs::parse("", "  ", ",;"));

    for t in [&before, &after] {
        let (mut audio, subs, explicit) = t.ticked_streams();
        audio.sort_unstable();
        assert_eq!(audio, vec![spid(0), spid(1)]);
        assert_eq!(subs, vec![spid(2)]);
        assert!(
            !explicit,
            "with no preference the selection must still be the implicit 'everything'"
        );
        assert_eq!(t.check_state(nth(t, "Title", 0)), Check::On);
    }

    // And a title that does NOT start checked keeps every stream clear, whether
    // or not a preference is set — the preference narrows a selection, it never
    // creates one.
    let two = disc(&[(5400.0, 2), (600.0, 1)]);
    let t = tree_prefs(&two, "Main film only", &LangPrefs::parse("German", "", ""));
    assert_eq!(t.check_state(nth(&t, "Title", 1)), Check::Off);
}

/// The three boxes are parsed as comma/semicolon lists, because a language may
/// be given by NAME and names contain spaces. Splitting on whitespace would
/// turn "Modern Greek" into two tags that resolve to nothing.
#[test]
fn language_lists_split_on_commas_not_on_spaces() {
    let p = LangPrefs::parse("German, Spanish", " Modern Greek ;de ", "en,,");
    assert_eq!(p.audio, vec!["German", "Spanish"]);
    assert_eq!(p.subtitles, vec!["Modern Greek", "de"]);
    assert_eq!(p.forced, vec!["en"]);
    assert!(!p.is_empty());
    assert!(LangPrefs::parse("", " ", " , ; ").is_empty());
}

// ── A video-only title's checkbox ────────────────────────────────────────────
//
// `check_state` folds a title's CHECKABLE children into a tri-state, and video
// rows are not checkable. A title whose only child is video therefore had an
// EMPTY fold set, fell into the "none ticked" arm, and drew unticked — while
// `ticked_titles`, which reads the node's own flag rather than the fold, ripped
// it anyway. `toggle` could not move it either: Off -> set true -> still Off.
// The box was dead, and it showed the opposite of what would happen.

/// The box must say what the rip will do.
#[test]
fn a_video_only_titles_checkbox_matches_what_will_be_ripped() {
    let sc = video_only_disc();
    let t = tree(&sc, "All titles", 0.0);
    let title = t
        .arena
        .iter()
        .position(|n| n.type_s == "Title")
        .expect("the fixture has a title row");

    assert!(
        t.ticked_titles().contains(&0),
        "'All titles' must schedule this title, or the assertion below is \
         vacuous"
    );
    assert_eq!(
        t.check_state(title),
        Check::On,
        "the title WILL be ripped, so its box must not draw empty"
    );
}

/// And the box must be usable: unticking it has to take effect.
#[test]
fn a_video_only_titles_checkbox_can_actually_be_cleared() {
    let sc = video_only_disc();
    let t = tree(&sc, "All titles", 0.0);
    let title = t
        .arena
        .iter()
        .position(|n| n.type_s == "Title")
        .expect("the fixture has a title row");

    t.toggle(title);
    assert_eq!(
        t.check_state(title),
        Check::Off,
        "one click must clear it; the old fold reported Off either way, so \
         toggle read Off and set it back to ticked"
    );
    assert!(
        t.ticked_titles().is_empty(),
        "and clearing the box must actually cancel the title"
    );
}

/// The ordinary case is untouched: a title WITH checkable children still folds
/// to a tri-state.
#[test]
fn a_title_with_checkable_children_still_folds_to_a_tri_state() {
    let sc = two_title_disc();
    let t = tree(&sc, "All titles", 0.0);
    let title = t
        .arena
        .iter()
        .position(|n| n.type_s == "Title")
        .expect("title row");
    assert_eq!(t.check_state(title), Check::On);

    let first_audio = t.arena[title]
        .children
        .iter()
        .copied()
        .find(|&c| t.arena[c].type_s == "Audio")
        .expect("the main feature has audio tracks");
    t.toggle(first_audio);
    assert_eq!(
        t.check_state(title),
        Check::Mixed,
        "some but not all children ticked is still Mixed"
    );
}
