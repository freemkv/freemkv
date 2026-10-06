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
