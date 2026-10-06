use super::*;

/// The four DPIs Windows actually ships as defaults: 100%, 125%, 150%,
/// 200%. 125% and 150% are the stock settings on laptops.
const DPIS: [u32; 4] = [96, 120, 144, 192];

#[test]
fn px_matches_muldiv_rounding() {
    // 8 px of padding, at each scaling step.
    assert_eq!(Scale::new(96).px(8), 8);
    assert_eq!(Scale::new(120).px(8), 10);
    assert_eq!(Scale::new(144).px(8), 12);
    assert_eq!(Scale::new(192).px(8), 16);

    // Rounds half away from zero, like Win32 MulDiv: 15 * 120 / 96 = 18.75,
    // and 4 * 120 / 96 = 5.0 exactly.
    assert_eq!(Scale::new(120).px(15), 19);
    assert_eq!(Scale::new(120).px(4), 5);
    assert_eq!(Scale::new(144).px(15), 23); // 22.5 -> 23
    assert_eq!(Scale::new(120).px(-15), -19);
    assert_eq!(Scale::new(96).px(0), 0);
}

#[test]
fn zero_and_absurd_dpi_are_clamped() {
    // GetDpiForWindow returns 0 before the window exists (WM_GETMINMAXINFO
    // arrives before WM_CREATE); a 0 would collapse every rectangle.
    assert_eq!(Scale::new(0).dpi(), 96);
    assert_eq!(Scale::new(0).px(8), 8);
    assert_eq!(Scale::new(50).dpi(), 96);
    assert_eq!(Scale::new(100_000).dpi(), 960);
}

#[test]
fn window_sizes_scale() {
    assert_eq!(default_size(96), (1180, 760));
    assert_eq!(default_size(120), (1475, 950));
    assert_eq!(default_size(144), (1770, 1140));
    assert_eq!(default_size(192), (2360, 1520));

    assert_eq!(min_size(96), (1020, 620));
    assert_eq!(min_size(120), (1275, 775));
    assert_eq!(min_size(144), (1530, 930));
    assert_eq!(min_size(192), (2040, 1240));
}

/// What the selection bar's labels measure in the tests: "Titles",
/// "Audio" and "Subtitles" at 9 pt, roughly.
const LABELS: [i32; 3] = [40, 40, 60];

fn titles(info_rows: usize) -> MainState {
    MainState {
        page: Page::Titles,
        two_bars: false,
        log_hidden: false,
        info_rows,
        pick_labels: LABELS,
    }
}

/// The default window at 96 DPI. Every other DPI case is measured against
/// these numbers.
#[test]
fn titles_page_at_96_dpi() {
    let l = main_layout(96, 1180, 760, titles(0));

    // ch * 0.24 = 182.4 -> 182; log_y = 760 - 8 - 182 = 570.
    assert_eq!(l.log, Rect::new(8, 570, 1164, 182));
    // top_y = TB_H + PAD = 12; top_h = 760 - 182 - 24 - 4 = 550.
    // The selection bar: label, 4 px, chooser, 18 px, and so on.
    assert_eq!(l.pick_titles_lbl, Rect::new(8, 19, 40, 16));
    assert_eq!(l.pick_titles, Rect::new(52, 15, 190, 24));
    assert_eq!(l.pick_audio_lbl, Rect::new(260, 19, 40, 16));
    assert_eq!(l.pick_audio, Rect::new(304, 15, 170, 24));
    assert_eq!(l.pick_subs_lbl, Rect::new(492, 19, 60, 16));
    assert_eq!(l.pick_subs, Rect::new(556, 15, 170, 24));
    assert_eq!(l.btn_eject, Rect::new(1062, 14, 110, 26));
    // The tree under the bar, over the full width: 550 - 30 - 8 - 70 = 442.
    assert_eq!(l.tree, Rect::new(8, 42, 1164, 442));
    // The output area: bar_y = 12 + 550 - 70 = 492, its row at 514.
    assert_eq!(l.lbl_out, Rect::new(8, 495, 776, 16));
    assert_eq!(l.btn_run, Rect::new(1062, 514, 110, 28));
    assert_eq!(l.cmb_format, Rect::new(834, 516, 220, 24));
    assert_eq!(l.btn_browse, Rect::new(792, 515, 34, 26));
    assert_eq!(l.edit_out, Rect::new(8, 516, 776, 23));
    assert_eq!(l.lbl_free, Rect::new(10, 546, 776, 16));
}

/// The three rows stack without overlapping at every DPI: the selection
/// bar, the tree, the output area, then the log.
#[test]
fn the_titles_page_stacks_its_rows_at_every_dpi() {
    for dpi in DPIS {
        let s = Scale::new(dpi);
        let (w, h) = default_size(dpi);
        let st = MainState {
            pick_labels: LABELS.map(|v| s.px(v)),
            ..titles(0)
        };
        let l = main_layout(dpi, w, h, st);
        let bottom = |r: Rect| r.y + r.h;
        let right = |r: Rect| r.x + r.w;
        assert!(bottom(l.pick_titles) <= l.tree.y, "dpi {dpi}");
        assert!(right(l.pick_subs) <= l.btn_eject.x, "dpi {dpi}");
        assert_eq!(l.tree.w, w - s.px(PAD) * 2, "dpi {dpi}: full width");
        assert!(bottom(l.tree) < l.lbl_out.y, "dpi {dpi}");
        assert!(bottom(l.lbl_out) <= l.edit_out.y, "dpi {dpi}");
        assert!(right(l.edit_out) < l.btn_browse.x, "dpi {dpi}");
        assert!(right(l.btn_browse) < l.cmb_format.x, "dpi {dpi}");
        assert!(right(l.cmb_format) < l.btn_run.x, "dpi {dpi}");
        assert_eq!(right(l.btn_run), w - s.px(PAD), "dpi {dpi}");
        assert!(bottom(l.edit_out) <= l.lbl_free.y, "dpi {dpi}");
        assert!(bottom(l.lbl_free) < l.log.y, "dpi {dpi}");
    }
}

/// The progress page: two bars, seven information rows.
#[test]
fn progress_page_scales() {
    let st = MainState {
        page: Page::Progress,
        two_bars: true,
        log_hidden: false,
        info_rows: 7,
        pick_labels: LABELS,
    };

    let l = main_layout(96, 1180, 760, st);
    // top_y = 12; gh = 132; bar1_y = 12 + 132 + 22 = 166.
    assert_eq!(l.grp_prog, Rect::new(8, 12, 1164, 132));
    assert_eq!(l.bar_cur, Rect::new(8, 166, 1164, 20));
    assert_eq!(l.lbl_cur, Rect::new(832, 148, 340, 16));
    // The other three captions are computed the same way and were never
    // asserted: cap_dy=18 above each bar, cap_w=340 wide, left-aligned for
    // the "saving" labels and right-aligned for the percentage ones.
    assert_eq!(l.lbl_saving_cur, Rect::new(8, 148, 340, 16));
    assert_eq!(l.bar_all, Rect::new(8, 212, 1164, 20)); // 166 + 46
    assert_eq!(l.lbl_saving_all, Rect::new(8, 194, 340, 16)); // 212 - 18
    assert_eq!(l.lbl_all, Rect::new(832, 194, 340, 16)); // cw - pad - 340
    // btn_cancel: top_y + PROG_H - 36 = 12 + 292 - 36 = 268.
    assert_eq!(l.btn_cancel, Rect::new(1052, 268, 120, 30));
    assert_eq!(l.info_rows.len(), 7);
    assert_eq!(l.info_rows[0].0, Rect::new(14, 32, 110, 15));
    assert_eq!(l.info_rows[0].1, Rect::new(132, 32, 1024, 15));
    assert_eq!(l.info_rows[6].0, Rect::new(14, 122, 110, 15)); // 32 + 6*15
    // log fills what the fixed panel leaves: 760 - 4 - 292 - 24 = 440.
    assert_eq!(l.log.h, 440);

    let l = main_layout(144, 1770, 1140, st);
    // top_y = 18; gh = 198; bar1_y = 18 + 198 + 33 = 249.
    assert_eq!(l.grp_prog, Rect::new(12, 18, 1746, 198));
    assert_eq!(l.bar_cur, Rect::new(12, 249, 1746, 30));
    assert_eq!(l.lbl_cur, Rect::new(1248, 222, 510, 24));
    assert_eq!(l.lbl_saving_cur, Rect::new(12, 222, 510, 24));
    assert_eq!(l.bar_all, Rect::new(12, 318, 1746, 30)); // 249 + 69
    assert_eq!(l.lbl_saving_all, Rect::new(12, 291, 510, 24)); // 318 - 27
    assert_eq!(l.lbl_all, Rect::new(1248, 291, 510, 24));
    // 18 + 438 - 54 = 402.
    assert_eq!(l.btn_cancel, Rect::new(1578, 402, 180, 45));
    assert_eq!(l.info_rows[0].0, Rect::new(21, 48, 165, 23));
    assert_eq!(l.info_rows[6].0, Rect::new(21, 186, 165, 23)); // 48 + 6*23
    // 1140 - 6 - 438 - 36 = 660.
    assert_eq!(l.log.h, 660);

    // One bar only: the panel is shorter, so the log is taller.
    let one = MainState {
        two_bars: false,
        ..st
    };
    assert_eq!(main_layout(96, 1180, 760, one).log.h, 486); // 760-4-246-24
    assert_eq!(main_layout(192, 2360, 1520, one).log.h, 972); // 1520-8-492-48
}

#[test]
fn result_page_scales() {
    let st = MainState {
        page: Page::Result,
        two_bars: false,
        log_hidden: false,
        info_rows: 7,
        pick_labels: LABELS,
    };
    let l = main_layout(96, 1180, 760, st);
    assert_eq!(l.result_head, Rect::new(8, 38, 1164, 26));
    assert_eq!(l.result_line, Rect::new(8, 70, 1164, 20));
    assert_eq!(l.btn_reveal, Rect::new(410, 112, 170, 32));
    assert_eq!(l.btn_done, Rect::new(600, 112, 170, 32));
    // The Result page reserves RESULT_H and the log takes the remainder.
    // Without this the "delete the Page::Result arm" mutant is invisible:
    // page_h falls to 0 and log_h switches to LOG_FRAC (760-4-200-24=532).
    assert_eq!(l.log.h, 532);

    let l = main_layout(192, 2360, 1520, st);
    assert_eq!(l.result_head, Rect::new(16, 76, 2328, 52));
    assert_eq!(l.result_line, Rect::new(16, 140, 2328, 40));
    assert_eq!(l.btn_reveal, Rect::new(820, 224, 340, 64));
    assert_eq!(l.btn_done, Rect::new(1200, 224, 340, 64));
    // 1520 - 8 - 400 - 48 = 1064.
    assert_eq!(l.log.h, 1064);
}

#[test]
fn empty_page_is_centred_at_every_dpi() {
    for dpi in DPIS {
        let (w, h) = default_size(dpi);
        let l = main_layout(
            dpi,
            w,
            h,
            MainState {
                page: Page::Empty,
                two_bars: false,
                log_hidden: false,
                info_rows: 0,
                pick_labels: LABELS,
            },
        );
        // The headline shares the window's horizontal centre, and the
        // button PAIR straddles it symmetrically: equal gaps either side,
        // so their midpoint is the centre.
        assert_eq!(l.empty_head.x + l.empty_head.w / 2, w / 2, "dpi {dpi}");
        assert_eq!(l.btn_open_disc.w, l.btn_open.w, "dpi {dpi}");
        assert_eq!(
            l.btn_open_disc.x + (l.btn_open.x + l.btn_open.w - l.btn_open_disc.x) / 2,
            w / 2,
            "dpi {dpi}"
        );
        // Same row, and they never overlap.
        assert_eq!(l.btn_open_disc.y, l.btn_open.y, "dpi {dpi}");
        assert!(
            l.btn_open_disc.x + l.btn_open_disc.w <= l.btn_open.x,
            "dpi {dpi}"
        );
    }
    let l = main_layout(
        192,
        2360,
        1520,
        MainState {
            page: Page::Empty,
            two_bars: false,
            log_hidden: false,
            info_rows: 0,
            pick_labels: LABELS,
        },
    );
    // log_h = 1520 * 0.24 = 364; top_h = 1520 - 364 - 48 - 8 = 1100; cy = 24 + 550.
    assert_eq!(l.empty_head, Rect::new(16, 474, 2328, 52));
    assert_eq!(l.empty_sub, Rect::new(16, 530, 2328, 40));
    // cw/2 = 1180, gap = 32 → 1180 - 16 - 360 = 804, and 1180 + 16 = 1196.
    assert_eq!(l.btn_open_disc, Rect::new(804, 606, 360, 60));
    assert_eq!(l.btn_open, Rect::new(1196, 606, 360, 60));
}

#[test]
fn hidden_log_gives_its_height_to_the_page() {
    for dpi in DPIS {
        let (w, h) = default_size(dpi);
        let l = main_layout(
            dpi,
            w,
            h,
            MainState {
                page: Page::Titles,
                two_bars: false,
                log_hidden: true,
                info_rows: 0,
                pick_labels: LABELS,
            },
        );
        assert_eq!(l.log.h, 0, "dpi {dpi}");
        let s = Scale::new(dpi);
        // top_h = ch - 0 - pad*3 - tb; the tree fills what the two bars leave.
        let top_h = h - s.px(PAD) * 3 - s.px(TB_H);
        assert_eq!(
            l.tree.h,
            top_h - s.px(PICK_H) - s.px(PAD) - s.px(BAR_H),
            "dpi {dpi}"
        );
    }
}

/// At the minimum window size the layout must still produce non-degenerate
/// rectangles — that is the whole point of MIN_W/MIN_H, and it has to hold
/// at every DPI, not just the one the constants were written for.
#[test]
fn nothing_collapses_at_the_minimum_size() {
    for dpi in DPIS {
        let (w, h) = min_size(dpi);
        for page in [Page::Empty, Page::Titles, Page::Progress, Page::Result] {
            for log_hidden in [false, true] {
                let l = main_layout(
                    dpi,
                    w,
                    h,
                    MainState {
                        page,
                        two_bars: true,
                        log_hidden,
                        info_rows: 7,
                        pick_labels: LABELS.map(|v| Scale::new(dpi).px(v)),
                    },
                );
                for (name, r) in [
                    ("log", l.log),
                    ("tree", l.tree),
                    ("pick_subs", l.pick_subs),
                    ("edit_out", l.edit_out),
                    ("cmb_format", l.cmb_format),
                    ("btn_run", l.btn_run),
                    ("lbl_free", l.lbl_free),
                    ("bar_cur", l.bar_cur),
                    ("bar_all", l.bar_all),
                    ("result_line", l.result_line),
                ] {
                    if name == "log" && log_hidden {
                        continue;
                    }
                    assert!(r.w > 0, "{name} width at dpi {dpi} page {page:?}");
                    assert!(r.h > 0, "{name} height at dpi {dpi} page {page:?}");
                    assert!(r.x >= 0, "{name} x at dpi {dpi} page {page:?}");
                }
                // Nothing may hang off the right edge.
                assert!(l.log.x + l.log.w <= w, "log overruns at dpi {dpi}");
                assert!(l.btn_run.x + l.btn_run.w <= w, "run overruns at dpi {dpi}");
                assert!(
                    l.pick_subs.x + l.pick_subs.w <= l.btn_eject.x,
                    "the selection bar runs into Eject at dpi {dpi}"
                );
            }
        }
    }
}

/// Doubling the DPI while doubling the window doubles every rectangle. The
/// clamped minimums and the `f64` proportions make this exact only when the
/// baseline lands on whole pixels, which the default size does.
#[test]
fn ninety_six_to_one_ninety_two_is_a_clean_doubling() {
    let st = titles(7);
    let a = main_layout(96, 1180, 760, st);
    let b = main_layout(
        192,
        2360,
        1520,
        MainState {
            pick_labels: LABELS.map(|v| v * 2),
            ..st
        },
    );
    for (name, x, y) in [
        ("log", a.log, b.log),
        ("pick_titles", a.pick_titles, b.pick_titles),
        ("pick_subs", a.pick_subs, b.pick_subs),
        ("tree", a.tree, b.tree),
        ("lbl_out", a.lbl_out, b.lbl_out),
        ("edit_out", a.edit_out, b.edit_out),
        ("btn_run", a.btn_run, b.btn_run),
        ("btn_eject", a.btn_eject, b.btn_eject),
        ("lbl_free", a.lbl_free, b.lbl_free),
    ] {
        assert_eq!(y.x, x.x * 2, "{name}.x");
        assert_eq!(y.y, x.y * 2, "{name}.y");
        assert_eq!(y.w, x.w * 2, "{name}.w");
        assert_eq!(y.h, x.h * 2, "{name}.h");
    }
}

#[test]
fn settings_and_about_chrome_scale() {
    // 96 DPI reproduces the shipped numbers exactly.
    let p = prefs_layout(96, 680, 520);
    assert_eq!(p.tab, Rect::new(10, 10, 660, 464));
    assert_eq!(p.btn_ok, Rect::new(572, 482, 96, 28));
    assert_eq!(p.btn_cancel, Rect::new(468, 482, 96, 28));
    assert_eq!(about_close_rect(96, 420, 260), Rect::new(310, 222, 96, 28));

    let p = prefs_layout(144, 1020, 780);
    assert_eq!(p.tab, Rect::new(15, 15, 990, 696));
    assert_eq!(p.btn_ok, Rect::new(858, 723, 144, 42));
    assert_eq!(p.btn_cancel, Rect::new(702, 723, 144, 42));
    assert_eq!(
        about_close_rect(192, 840, 520),
        Rect::new(620, 444, 192, 56)
    );

    let f = form_metrics(96);
    assert_eq!((f.top, f.gutter, f.width, f.field_h), (16, 250, 620, 22));
    assert_eq!((f.row_step, f.check_step, f.note_step), (30, 28, 38));
    let f = form_metrics(120);
    assert_eq!((f.top, f.gutter, f.width, f.field_h), (20, 313, 775, 28));
    assert_eq!((f.row_step, f.check_step, f.note_step), (38, 35, 48));
    let f = form_metrics(192);
    assert_eq!((f.top, f.gutter, f.width, f.field_h), (32, 500, 1240, 44));
    assert_eq!((f.row_step, f.check_step, f.note_step), (60, 56, 76));
}

/// The real list the language pickers show. Kept as a number rather than
/// reading `ui::PICKER_LANGUAGES` so this file stays pure arithmetic.
const LANGS: usize = 38;

#[test]
fn a_short_menu_is_one_column() {
    // 38 rows of 19px is 722px; four fifths of a 1080p screen is 864px, so
    // the whole list fits and must NOT be broken into columns.
    assert_eq!(menu_column_rows(LANGS, 19, 1080), LANGS);
    assert_eq!(menu_column_rows(1, 19, 1080), 1);
}

#[test]
fn a_long_menu_is_split_into_balanced_columns() {
    // A 768px netbook at 150%: 4/5 of 768 is 614px, 26px rows, so 23 fit.
    // 38 entries therefore need two columns — and they come out even (19
    // and 19), not 23 and a stub of 15.
    assert_eq!(menu_column_rows(LANGS, 26, 768), 19);

    // Squeezed harder: 4/5 of 600 is 480, 40px rows, 12 per column. 38
    // needs four columns, balanced at 10 each (10+10+10+8).
    assert_eq!(menu_column_rows(LANGS, 40, 600), 10);
}

#[test]
fn every_entry_is_reachable_at_every_screen_size() {
    // The property that actually matters: the columns must between them
    // hold the whole list, and no column may overflow the budget. Checked
    // across a sweep of plausible screens and DPI-scaled row heights.
    for screen_h in [480, 600, 720, 768, 800, 900, 1080, 1440, 2160] {
        for item_h in [15, 19, 24, 26, 32, 40, 48] {
            let rows = menu_column_rows(LANGS, item_h, screen_h);
            assert!(rows >= 1, "{screen_h}x{item_h} produced an empty column");
            let cols = LANGS.div_ceil(rows);
            assert!(
                rows * cols >= LANGS,
                "{screen_h}x{item_h}: {cols} columns of {rows} cannot hold {LANGS} entries"
            );
            let budget = ((screen_h * 4 / 5) / item_h) as usize;
            assert!(
                rows <= budget.max(1),
                "{screen_h}x{item_h}: column of {rows} exceeds the {budget}-row budget"
            );
        }
    }
}

#[test]
fn degenerate_metrics_do_not_divide_by_zero() {
    // GetSystemMetrics can answer 0 for a metric it does not know, and a
    // 0-height screen is what a disconnected monitor reports mid-hotplug.
    // Neither may panic, and neither may return a 0-row column.
    assert_eq!(menu_column_rows(LANGS, 0, 1080), LANGS);
    assert_eq!(menu_column_rows(LANGS, 19, 0), 1);
    assert_eq!(menu_column_rows(LANGS, -5, -5), 1);
    assert_eq!(menu_column_rows(0, 19, 1080), 1);
}

// Settings pages stack downwards from `top` with no scrolling, so a page that grows past
// the tab's page area puts controls off the bottom of the window at every DPI.
#[test]
fn the_tallest_settings_pages_still_fit_the_settings_window() {
    // COUNT THE REAL PAGE, don't restate it: hand-written row counts left
    // this green while the actual page grew past the window. Instead parse
    // the `// ── Name ──` sections and field/lang/note/check rows from windows.rs.
    let shell = include_str!("windows.rs");
    // Every call that moves the y cursor, per kind: (rows, notes, checks, buttons, gaps).
    let section = |name: &str| -> (usize, usize, usize, usize, usize) {
        let head = format!("// ── {name}");
        let from = shell
            .find(&head)
            .unwrap_or_else(|| panic!("no {name} section in windows.rs"));
        let rest = &shell[from + head.len()..];
        let to = rest.find("// ── ").unwrap_or(rest.len());
        let body = &rest[..to];
        (
            [
                    "r.field(",
                    "r.field_secure(",
                    "r.lang(",
                    "r.combo(",
                    "r.path(",
                ]
                .iter()
                .map(|c| body.matches(c).count())
                .sum::<usize>()
                    // A path row puts its field on a line of its own under the label.
                    + body.matches("r.path(").count(),
            body.matches("r.note(").count(),
            body.matches("r.check(").count(),
            body.matches("r.button(").count(),
            body.matches("r.gap()").count(),
        )
    };

    for dpi in DPIS {
        let f = form_metrics(dpi);
        let s = Scale::new(dpi);
        // `Prefs::new` gives the tab control `PREFS_H - 66` of height; the
        // tab strip and the page's own border take ~28 more before the
        // first row can be drawn.
        let page = s.px(PREFS_H) - s.px(66) - s.px(28);

        for name in ["Output", "Selection", "Recovery", "Keys", "Advanced"] {
            let (fields, notes, checks, buttons, gaps) = section(name);
            assert!(
                fields + notes + checks > 0,
                "{name} section parsed as empty — the marker or the row \
                     helpers were renamed, so this test stopped measuring \
                     anything"
            );
            let needed = f.top
                + fields as i32 * f.row_step
                + notes as i32 * f.note_step
                + checks as i32 * f.check_step
                + buttons as i32 * f.button_step
                + gaps as i32 * f.gap;
            assert!(
                needed <= page,
                "{name} page needs {needed}px of {page}px at {dpi} dpi \
                     ({fields} rows, {notes} notes, {checks} checks, {buttons} buttons, \
                     {gaps} gaps)"
            );
        }
    }
}

// ── the title tree's columns ──

#[test]
fn the_header_takes_its_strip_off_the_top_of_the_tree() {
    let area = Rect::new(8, 12, 540, 489);
    assert_eq!(
        split_tree_header(area, 96),
        (Rect::new(8, 12, 540, 24), Rect::new(8, 36, 540, 465))
    );
    let (head, tree) = split_tree_header(area, 144);
    assert_eq!(head.h, 36);
    assert_eq!(tree.y, 48);
    assert_eq!(head.h + tree.h, area.h, "nothing lost between the two");
}

/// The core's six columns at their widths, Notes (index 3) flexible.
const WIDTHS: [i32; 6] = [190, 76, 260, 240, 66, 66];
const FLEX: usize = 3;

#[test]
fn the_tick_column_comes_first_and_the_flexible_column_takes_the_rest() {
    let c = tree_columns(96, 1140, &WIDTHS, FLEX);
    assert_eq!(c.tick, Span { x: 0, w: 84 });
    // 1140 - 84 - (190 + 76 + 260 + 66 + 66) = 398 for Notes.
    let spans: Vec<(i32, i32)> = c.cols.iter().map(|s| (s.x, s.w)).collect();
    assert_eq!(
        spans,
        [
            (84, 190),
            (274, 76),
            (350, 260),
            (610, 398),
            (1008, 66),
            (1074, 66)
        ]
    );
    let last = c.cols[5];
    assert_eq!(last.x + last.w, 1140, "Size ends at the edge");
}

#[test]
fn the_columns_scale_with_the_dpi() {
    for dpi in DPIS {
        let s = Scale::new(dpi);
        let c = tree_columns(dpi, s.px(1140), &WIDTHS, FLEX);
        assert_eq!(c.tick.w, s.px(COL_TICK_W), "dpi {dpi}");
        for (i, w) in WIDTHS.iter().enumerate().filter(|(i, _)| *i != FLEX) {
            assert_eq!(c.cols[i].w, s.px(*w), "dpi {dpi} column {i}");
        }
        assert_eq!(c.cols[0].x, c.tick.w, "dpi {dpi}: no gap after the ticks");
        for pair in c.cols.windows(2) {
            assert_eq!(pair[0].x + pair[0].w, pair[1].x, "dpi {dpi}: no gaps");
        }
    }
}

#[test]
fn a_narrow_tree_squeezes_the_text_columns_and_never_the_numbers() {
    let c = tree_columns(96, 800, &WIDTHS, FLEX);
    assert_eq!(
        (c.cols[4].w, c.cols[5].w),
        (66, 66),
        "Length and Size keep theirs"
    );
    assert!(c.cols[0].w < 190 && c.cols[FLEX].w >= 120, "{:?}", c.cols);
    let last = c.cols[5];
    assert!(last.x + last.w <= 800, "Size still ends inside the tree");
    let crushed = tree_columns(96, 300, &WIDTHS, FLEX);
    assert_eq!(crushed.cols[FLEX].w, 120, "no narrower than its minimum");
}

#[test]
fn the_header_items_line_up_with_the_client_columns() {
    let c = tree_columns(96, 1140, &WIDTHS, FLEX);
    // A 2 px border each side and a 17 px scroll bar: 1161 outer.
    let w = header_widths(&c, 1161, 2);
    assert_eq!(w, [86, 190, 76, 260, 398, 66, 85]);
    assert_eq!(w[0], c.cols[0].x + 2, "Item's header starts over its cells");
    assert_eq!(w.iter().sum::<i32>(), 1161, "the header is filled exactly");
}

#[test]
fn dragging_a_divider_moves_only_that_divider() {
    let c = tree_columns(96, 1140, &WIDTHS, FLEX);
    // Item's edge 20 px right: Item grows, Notes gives way.
    let w = drag_column(96, &c, FLEX, 1, 210);
    assert_eq!(w[0], 210);
    assert_eq!(tree_columns(96, 1140, &w, FLEX).cols[FLEX].w, 378);
    // Notes' edge 10 px left: Length grows by the same, Size stays put.
    let w = drag_column(96, &c, FLEX, 4, 388);
    assert_eq!((w[4], w[5]), (76, 66));
    let laid = tree_columns(96, 1140, &w, FLEX);
    assert_eq!(laid.cols[4].x, 998);
    assert_eq!(laid.cols[5].x, 1074);
    // The tick column and the last edge are pinned.
    let same: Vec<i32> = c.cols.iter().map(|s| s.w).collect();
    assert_eq!(drag_column(96, &c, FLEX, 0, 300), same);
    assert_eq!(drag_column(96, &c, FLEX, 6, 300), same);
    // Never under the column's minimum.
    assert_eq!(drag_column(96, &c, FLEX, 2, 5)[1], 76);
}

#[test]
fn a_dragged_width_is_kept_at_the_baseline_so_it_survives_a_dpi_change() {
    let c = tree_columns(144, 1710, &WIDTHS, FLEX);
    let w = drag_column(144, &c, FLEX, 2, c.cols[1].w + 30);
    assert_eq!(w[1], 96);
    let at_96 = tree_columns(96, 1140, &w, FLEX);
    assert_eq!(at_96.cols[1].w, 96);
}

#[test]
fn arabic_and_hebrew_are_right_to_left() {
    for tag in ["ar", "he", "ar-SA", "he_IL", "fa-IR", "ur", "AR"] {
        assert!(is_rtl_locale(tag), "{tag}");
    }
    for tag in ["en", "de", "zh-hans", "pt-br", "auto", "", "hu", "fr-ca"] {
        assert!(!is_rtl_locale(tag), "{tag}");
    }
}

// ── the Settings form ──

#[test]
fn a_dropdown_grows_to_its_longest_item_within_its_row() {
    // Fits already: unchanged, and the list is at least as wide as the box.
    assert_eq!(combo_widths(96, 240, 100, 360), (240, 240));
    // Longer: the box grows to fit, the list too.
    assert_eq!(combo_widths(96, 240, 300, 360), (334, 334));
    // Longer than the row: the box stops at the row, the list shows it all.
    assert_eq!(combo_widths(96, 240, 400, 360), (360, 430));
    // A row narrower than the control never shrinks it.
    assert_eq!(combo_widths(96, 240, 400, 100).0, 240);
    // Scaled margins at 150%.
    assert_eq!(combo_widths(144, 360, 450, 900), (501, 501));
}

#[test]
fn enter_and_leaving_a_field_save_without_closing_ok_saves_and_closes() {
    use FormCommit::*;
    let plan = |save, close| CommitPlan { save, close };
    assert_eq!(form_commit(Ok, true, false), plan(true, true));
    assert_eq!(form_commit(Ok, true, true), plan(true, true));
    assert_eq!(form_commit(Enter, true, true), plan(true, false));
    assert_eq!(form_commit(Enter, true, false), plan(false, false));
    assert_eq!(form_commit(FocusLost, true, true), plan(true, false));
    assert_eq!(form_commit(FocusLost, true, false), plan(false, false));
    // The window going away takes focus with it; that is not an edit.
    assert_eq!(form_commit(FocusLost, false, true), plan(false, false));
}

#[test]
fn unpx_inverts_px() {
    for dpi in DPIS {
        let s = Scale::new(dpi);
        for v in [0, 1, 36, 80, 92, 120] {
            assert_eq!(s.unpx(s.px(v)), v, "dpi {dpi}, {v}");
        }
    }
}
