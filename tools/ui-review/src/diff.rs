//! Whether a capture changed against its approved baseline, and a picture of where.
//!
//! Two captures of the same screen on the same runner image are normally identical, but text
//! is anti-aliased and a font or driver update moves its soft edges. So a pixel counts as
//! changed only when
//!
//! - its colour moved by more than `threshold`, measured as a perceptual (YIQ) distance from
//!   0 (identical) to 1 (black against white), and
//! - it is not anti-aliasing: a pixel on an edge between a darker and a brighter neighbour,
//!   where that neighbour sits in a flat area in both captures, is a smoothing difference and
//!   is set aside.
//!
//! A screen is changed when its size differs or at least `min_pixels` pixels changed. The
//! defaults pass font-rendering noise and flag a one-word relabel.

use image::{Rgba, RgbaImage};
use std::path::Path;

/// The tolerances. See the module documentation.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub threshold: f64,
    pub min_pixels: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            threshold: 0.1,
            min_pixels: 24,
        }
    }
}

/// What a comparison found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    pub changed: bool,
    /// Pixels over the threshold that are not anti-aliasing.
    pub pixels: usize,
    /// Pixels over the threshold set aside as anti-aliasing.
    pub antialiased: usize,
    pub size_changed: bool,
}

/// The largest YIQ distance, black against white.
const MAX_DELTA: f64 = 35215.0;

/// Compare two images; the highlight is the new image faded, with changed pixels red,
/// anti-aliasing yellow and any area only one image has magenta.
#[must_use]
pub fn compare(old: &RgbaImage, new: &RgbaImage, opt: Options) -> (Outcome, RgbaImage) {
    let (ow, oh) = old.dimensions();
    let (nw, nh) = new.dimensions();
    let (w, h) = (ow.max(nw), oh.max(nh));
    let mut out = RgbaImage::new(w, h);
    let mut found = Outcome {
        size_changed: (ow, oh) != (nw, nh),
        ..Outcome::default()
    };
    let limit = MAX_DELTA * opt.threshold * opt.threshold;
    for y in 0..h {
        for x in 0..w {
            let (Some(a), Some(b)) = (pixel(old, x, y), pixel(new, x, y)) else {
                out.put_pixel(x, y, Rgba([255, 0, 255, 255]));
                found.pixels += 1;
                continue;
            };
            let paint = if a == b || delta(a, b) <= limit {
                faded(b)
            } else if antialiased(old, new, x, y) || antialiased(new, old, x, y) {
                found.antialiased += 1;
                Rgba([255, 200, 0, 255])
            } else {
                found.pixels += 1;
                Rgba([230, 0, 0, 255])
            };
            out.put_pixel(x, y, paint);
        }
    }
    found.changed = found.size_changed || found.pixels >= opt.min_pixels;
    (found, out)
}

/// Compare the PNGs at `old` and `new`; when they changed, write the highlight to `diff`.
/// Byte-identical files are equal without decoding.
pub fn compare_files(
    old: &Path,
    new: &Path,
    diff: &Path,
    opt: Options,
) -> std::io::Result<Outcome> {
    if std::fs::read(old)? == std::fs::read(new)? {
        return Ok(Outcome::default());
    }
    let load = |p: &Path| {
        image::open(p)
            .map(|i| i.to_rgba8())
            .map_err(|e| std::io::Error::other(format!("{}: {e}", p.display())))
    };
    let (found, picture) = compare(&load(old)?, &load(new)?, opt);
    if found.changed {
        if let Some(dir) = diff.parent() {
            std::fs::create_dir_all(dir)?;
        }
        picture
            .save(diff)
            .map_err(|e| std::io::Error::other(format!("{}: {e}", diff.display())))?;
    }
    Ok(found)
}

fn pixel(img: &RgbaImage, x: u32, y: u32) -> Option<Rgba<u8>> {
    (x < img.width() && y < img.height()).then(|| *img.get_pixel(x, y))
}

// A channel blended over white by its alpha, so transparency compares as what is seen.
fn over_white(c: u8, alpha: f64) -> f64 {
    255.0 + (f64::from(c) - 255.0) * alpha
}

fn yiq(p: Rgba<u8>) -> (f64, f64, f64) {
    let alpha = f64::from(p[3]) / 255.0;
    let (r, g, b) = (
        over_white(p[0], alpha),
        over_white(p[1], alpha),
        over_white(p[2], alpha),
    );
    (
        r * 0.298_895_31 + g * 0.586_622_47 + b * 0.114_482_23,
        r * 0.595_977_99 - g * 0.274_176_10 - b * 0.321_801_89,
        r * 0.211_470_17 - g * 0.522_617_11 + b * 0.311_146_94,
    )
}

/// The squared perceptual distance between two colours, 0 to `MAX_DELTA`.
fn delta(a: Rgba<u8>, b: Rgba<u8>) -> f64 {
    let (y1, i1, q1) = yiq(a);
    let (y2, i2, q2) = yiq(b);
    let (dy, di, dq) = (y1 - y2, i1 - i2, q1 - q2);
    0.5053 * dy * dy + 0.299 * di * di + 0.1957 * dq * dq
}

fn brightness(p: Rgba<u8>) -> f64 {
    yiq(p).0
}

/// The new image's pixel as a pale grey, so the marks stand out over the screen.
fn faded(p: Rgba<u8>) -> Rgba<u8> {
    let v = (255.0 + (brightness(p) - 255.0) * 0.15).clamp(0.0, 255.0) as u8;
    Rgba([v, v, v, 255])
}

/// The in-bounds neighbours of `(x, y)`, and whether it lies on the image's edge.
fn neighbours(img: &RgbaImage, x: u32, y: u32) -> (Vec<(u32, u32)>, bool) {
    let (w, h) = img.dimensions();
    let mut out = Vec::with_capacity(8);
    for ny in y.saturating_sub(1)..=(y + 1).min(h - 1) {
        for nx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
            if (nx, ny) != (x, y) {
                out.push((nx, ny));
            }
        }
    }
    (out, x == 0 || y == 0 || x + 1 == w || y + 1 == h)
}

/// Whether `(x, y)` of `img` is an anti-aliased edge pixel: it has both a darker and a
/// brighter neighbour, few neighbours of its own brightness, and the darkest or brightest
/// neighbour lies inside a flat area in both images.
fn antialiased(img: &RgbaImage, other: &RgbaImage, x: u32, y: u32) -> bool {
    let Some(centre) = pixel(img, x, y) else {
        return false;
    };
    let centre = brightness(centre);
    let (around, edge) = neighbours(img, x, y);
    let mut same = usize::from(edge);
    let (mut darkest, mut brightest) = ((0.0, None), (0.0, None));
    for &(nx, ny) in &around {
        let d = brightness(*img.get_pixel(nx, ny)) - centre;
        if d == 0.0 {
            same += 1;
            if same > 2 {
                return false;
            }
        } else if d < darkest.0 {
            darkest = (d, Some((nx, ny)));
        } else if d > brightest.0 {
            brightest = (d, Some((nx, ny)));
        }
    }
    let (Some(lo), Some(hi)) = (darkest.1, brightest.1) else {
        return false;
    };
    (flat(img, lo) && flat(other, lo)) || (flat(img, hi) && flat(other, hi))
}

/// Whether `(x, y)` has at least three neighbours of exactly its colour: inside a flat area.
fn flat(img: &RgbaImage, (x, y): (u32, u32)) -> bool {
    let Some(centre) = pixel(img, x, y) else {
        return false;
    };
    let (around, edge) = neighbours(img, x, y);
    let same = usize::from(edge)
        + around
            .iter()
            .filter(|&&(nx, ny)| *img.get_pixel(nx, ny) == centre)
            .count();
    same > 2
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: Rgba<u8> = Rgba([255, 255, 255, 255]);
    const BLACK: Rgba<u8> = Rgba([0, 0, 0, 255]);

    fn canvas() -> RgbaImage {
        RgbaImage::from_pixel(40, 20, WHITE)
    }

    // A solid black block: a stand-in for a glyph or a control.
    fn block(img: &mut RgbaImage, x0: u32, y0: u32, w: u32, h: u32) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                img.put_pixel(x, y, BLACK);
            }
        }
    }

    #[test]
    fn identical_images_are_unchanged() {
        let mut a = canvas();
        block(&mut a, 5, 5, 10, 8);
        let (found, _) = compare(&a, &a, Options::default());
        assert_eq!(found, Outcome::default());
    }

    #[test]
    fn a_new_block_is_a_change_and_is_marked_red() {
        let a = canvas();
        let mut b = canvas();
        block(&mut b, 5, 5, 10, 8);
        let (found, picture) = compare(&a, &b, Options::default());
        assert!(found.changed);
        assert_eq!(found.pixels, 80);
        assert_eq!(*picture.get_pixel(9, 8), Rgba([230, 0, 0, 255]));
        assert_eq!(*picture.get_pixel(30, 15), faded(WHITE));
    }

    #[test]
    fn a_shift_below_the_threshold_is_unchanged() {
        let a = canvas();
        let b = RgbaImage::from_pixel(40, 20, Rgba([250, 250, 250, 255]));
        let (found, _) = compare(&a, &b, Options::default());
        assert!(!found.changed);
        assert_eq!(found.pixels, 0);
    }

    #[test]
    fn a_softened_edge_is_antialiasing_not_a_change() {
        // The same block; in the new image one pixel on its edge is grey, as a font's
        // smoothing would draw it.
        let mut a = canvas();
        block(&mut a, 5, 5, 10, 8);
        let mut b = a.clone();
        b.put_pixel(15, 8, Rgba([128, 128, 128, 255]));
        let (found, _) = compare(&a, &b, Options::default());
        assert_eq!(found.antialiased, 1);
        assert_eq!(found.pixels, 0);
        assert!(!found.changed);
    }

    #[test]
    fn a_few_stray_pixels_stay_under_the_minimum() {
        let a = canvas();
        let mut b = canvas();
        block(&mut b, 2, 2, 2, 2);
        let (found, _) = compare(&a, &b, Options::default());
        assert_eq!(found.pixels, 4);
        assert!(!found.changed);
    }

    #[test]
    fn a_resized_screen_is_a_change_with_its_new_area_marked() {
        let a = canvas();
        let b = RgbaImage::from_pixel(44, 20, WHITE);
        let (found, picture) = compare(&a, &b, Options::default());
        assert!(found.size_changed && found.changed);
        assert_eq!(found.pixels, 4 * 20);
        assert_eq!(*picture.get_pixel(42, 3), Rgba([255, 0, 255, 255]));
    }
}
