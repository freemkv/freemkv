use super::*;

// A 4 × 3 image of one colour, then with one pixel changed.
fn image() -> Vec<u8> {
    [10u8, 20, 30, 255].repeat(12)
}

#[test]
fn a_blank_region_has_no_ink_and_one_pixel_of_text_has() {
    let mut px = image();
    let all = Region {
        x: 0,
        y: 0,
        w: 4,
        h: 3,
    };
    assert!(!has_ink(&px, 4, 3, false, all));
    // Pixel (2, 1), top-down.
    px[(4 + 2) * 4] = 0;
    assert!(has_ink(&px, 4, 3, false, all));
    assert!(has_ink(
        &px,
        4,
        3,
        false,
        Region {
            x: 2,
            y: 1,
            w: 2,
            h: 1
        }
    ));
    assert!(!has_ink(
        &px,
        4,
        3,
        false,
        Region {
            x: 0,
            y: 0,
            w: 4,
            h: 1
        }
    ));
}

#[test]
fn a_bottom_up_image_is_read_from_its_last_row() {
    let mut px = image();
    // Stored row 0 of a bottom-up image is the picture's bottom row (y = 2).
    px[0] = 0;
    let bottom = Region {
        x: 0,
        y: 2,
        w: 2,
        h: 1,
    };
    assert!(has_ink(&px, 4, 3, true, bottom));
    assert!(!has_ink(&px, 4, 3, false, bottom));
}

#[test]
fn alpha_alone_is_not_ink_and_outside_the_image_is_nothing() {
    let mut px = image();
    px[3] = 0;
    assert!(!has_ink(
        &px,
        4,
        3,
        false,
        Region {
            x: 0,
            y: 0,
            w: 4,
            h: 3
        }
    ));
    assert!(!has_ink(
        &px,
        4,
        3,
        false,
        Region {
            x: 9,
            y: 9,
            w: 4,
            h: 3
        }
    ));
    assert!(!has_ink(
        &px[..8],
        4,
        3,
        false,
        Region {
            x: 0,
            y: 0,
            w: 4,
            h: 3
        }
    ));
}

#[test]
fn the_gate_passes_only_when_every_check_did() {
    let mut g = Gate::default();
    assert!(!g.passed(), "nothing checked is not a pass");
    g.check("fixture-opens", true, "2 titles");
    assert!(g.passed());
    g.check("length-fits", false, "61 px text in a 40 px column");
    assert!(!g.passed());
    let r = g.report();
    assert!(r.contains("GATE PASS fixture-opens — 2 titles\n"), "{r}");
    assert!(r.contains("GATE FAIL length-fits — 61 px"), "{r}");
    assert!(r.ends_with("GATE FAILED: 1 of 2 checks\n"), "{r}");
}

#[test]
fn a_note_is_reported_but_never_fails_the_gate() {
    let mut g = Gate::default();
    g.check("fixture-opens", true, "2 titles");
    g.note("open-file-dialog", "the native dialog never appeared");
    assert!(g.passed());
    let r = g.report();
    assert!(
        r.contains("GATE NOTE open-file-dialog — the native dialog never appeared\n"),
        "{r}"
    );
    assert!(r.ends_with("GATE OK: 1 checks\n"), "{r}");
}

#[test]
fn the_fake_drives_cover_every_status() {
    let drives = fake_drives();
    let present: Vec<_> = drives.iter().map(|d| d.present).collect();
    assert_eq!(present, [Some(true), Some(false), None]);
    let mut app = crate::ui::App::new();
    app.page = crate::ui::Page::Empty;
    app.home_drives = drives;
    assert_eq!(app.view().home_drives.len(), 3);
}

#[test]
fn a_multi_title_rip_shows_both_bars_and_a_single_one_the_top_bar_only() {
    let mut app = crate::ui::App::new();
    show_rip(&mut app, Rip::Multi);
    let v = app.view();
    assert_eq!(v.page, crate::ui::Page::Progress);
    assert!(v.show_overall_bar);
    assert_eq!((v.bar_current, v.bar_overall), (42.0, 57.0));
    assert!(v.info.is_some());
    show_rip(&mut app, Rip::Single);
    assert!(!app.view().show_overall_bar);
    end_rip(&mut app, crate::ui::Page::Titles);
    assert!(!app.running());
    assert_eq!(app.view().page, crate::ui::Page::Titles);
}

#[test]
fn each_result_has_its_own_heading_and_the_review_has_a_banner() {
    let mut app = crate::ui::App::new();
    let mut headings = Vec::new();
    for outcome in [
        crate::engine::RunOutcome::Completed,
        crate::engine::RunOutcome::Cancelled,
        crate::engine::RunOutcome::Failed,
    ] {
        show_result(&mut app, outcome);
        let v = app.view();
        assert_eq!(v.page, crate::ui::Page::Result);
        assert!(!v.result_summary.is_empty());
        headings.push(v.result_heading);
    }
    headings.dedup();
    assert_eq!(headings.len(), 3, "{headings:?}");
    assert!(!review().banner().is_empty());
}

#[test]
fn the_notices_are_logged_as_notices() {
    let mut app = crate::ui::App::new();
    let before = app.log.len();
    notices(&mut app);
    let added = &app.log[before..];
    assert_eq!(added.len(), 3);
    assert!(
        added
            .iter()
            .all(|l| l.kind == crate::ui::LogKind::Notice && !l.text.is_empty())
    );
}
