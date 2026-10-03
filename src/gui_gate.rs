//! qa's GUI gate: what the Windows and GTK shells check about themselves when a debug build
//! runs with `FMKV_GATE=<dir>` over the synthetic disc folder `tests/gui_fixture.py` writes.
//!
//! The shells measure their own widgets (a cell's text against its column, a path against its
//! field) and capture themselves; the arithmetic and the verdict live here, toolkit-free, so they
//! are tested on every host. Not `cfg`-gated for the same reason as `win_layout`.

/// The default destination the gate puts in Settings: on Windows long enough that the old
/// 280 px field cut it off; on Linux as long as a Settings row holds at 150% type.
pub const LONG_DEST: &str = if cfg!(windows) {
    r"C:\Users\Public\Videos\freemkv rips\Archive of my own discs"
} else {
    "/mnt/library/Videos/freemkv/Archive 2026"
};

/// A rectangle of a captured image, in top-down pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Whether `r` of a 32-bit image `width` × `height` holds more than one colour: text or a rule
/// on its background, not a blank fill. `bottom_up` is a Windows DIB's row order. A region
/// that falls outside the image has no ink.
#[must_use]
pub fn has_ink(px: &[u8], width: i32, height: i32, bottom_up: bool, r: Region) -> bool {
    let (x0, y0) = (r.x.max(0), r.y.max(0));
    let (x1, y1) = ((r.x + r.w).min(width), (r.y + r.h).min(height));
    let mut first: Option<&[u8]> = None;
    for y in y0..y1 {
        let row = if bottom_up { height - 1 - y } else { y };
        for x in x0..x1 {
            let i = (row as usize * width as usize + x as usize) * 4;
            let Some(p) = px.get(i..i + 3) else {
                return false;
            };
            match first {
                None => first = Some(p),
                Some(f) if f != p => return true,
                Some(_) => {}
            }
        }
    }
    false
}

/// The gate's findings, in order.
#[derive(Debug, Default)]
pub struct Gate {
    checks: Vec<(bool, String)>,
}

impl Gate {
    pub fn check(&mut self, name: &str, ok: bool, detail: impl AsRef<str>) {
        self.checks
            .push((ok, format!("{name} — {}", detail.as_ref())));
    }

    /// Whether every check passed; a gate that checked nothing has not passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(|(ok, _)| *ok)
    }

    /// One `GATE PASS …` / `GATE FAIL …` line per check, then the verdict.
    #[must_use]
    pub fn report(&self) -> String {
        let mut out: String = self
            .checks
            .iter()
            .map(|(ok, line)| format!("GATE {} {line}\n", if *ok { "PASS" } else { "FAIL" }))
            .collect();
        let failed = self.checks.iter().filter(|(ok, _)| !ok).count();
        out.push_str(&if self.passed() {
            format!("GATE OK: {} checks\n", self.checks.len())
        } else {
            format!("GATE FAILED: {failed} of {} checks\n", self.checks.len())
        });
        out
    }
}

#[cfg(test)]
mod tests {
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
}
