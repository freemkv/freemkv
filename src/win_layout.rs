//! DPI-aware geometry for the Windows shell — pure arithmetic, no Win32.
//!
//! ## Why this file exists at all
//!
//! `res/freemkv.manifest` declares **PerMonitorV2** awareness. That is a
//! promise: Windows stops bitmap-scaling the window and hands the process the
//! real DPI instead, expecting the app to lay itself out in physical pixels. A
//! layout written in bare constants (`PAD: i32 = 8`) therefore renders at 8
//! physical pixels on a 96-DPI desktop *and on a 192-DPI one* — half size, with
//! clipped text. 125% and 150% are the stock Windows settings on laptops, so
//! that is most users.
//!
//! Every constant below is the **96-DPI baseline**; nothing in `windows.rs`
//! positions a control from a raw constant any more. `Scale::px` converts a
//! baseline value to physical pixels for a given DPI, and `main_layout` turns a
//! DPI plus a client size into the complete set of rectangles.
//!
//! ## Why a pure function rather than scaling at each call site
//!
//! Scaling at the call site would work, but it cannot be tested: every value
//! would be locked behind a live `HWND` on a real display. Returning a plain
//! struct of rectangles from `(dpi, client size, page state)` makes the part
//! most likely to be wrong — the arithmetic — checkable on any host, which is
//! what the tests at the bottom of this file do. `windows.rs` keeps only the
//! mechanical "call `SetWindowPos` with these numbers" half.
//!
//! The module is deliberately NOT `cfg(windows)`: it holds no Win32 types, and
//! gating it would make its tests unrunnable anywhere but Windows.

use crate::ui::Page;

/// The DPI every constant in this file is expressed in. Windows' own baseline:
/// 100% scaling is 96 DPI, 125% is 120, 150% is 144, 200% is 192.
pub const BASE_DPI: u32 = 96;

// ── the 96-DPI baseline ───────────────────────────────────────────────────
//
// The same proportions the macOS shell uses, so the two look like one product:
// tree 46.4% wide, log 32% of the height on the tree page and more while
// ripping, Output/Info groups stacked on the right.

/// Default window client size.
pub const W: i32 = 1180;
pub const H: i32 = 760;
/// Smallest size at which the layout still works.
pub const MIN_W: i32 = 1020;
pub const MIN_H: i32 = 620;
pub const PAD: i32 = 8;
/// Reserved strip under the in-window menu bar.
pub const TB_H: i32 = 4;
/// Progress page height with both bars, and with only the per-title bar.
pub const PROG_H: i32 = 292;
pub const PROG_H_ONE: i32 = 246;
/// Result page height — fixed so its contents never drift off-screen.
pub const RESULT_H: i32 = 200;
/// Height of the Output group on the right column.
pub const OUT_H: i32 = 110;
/// Fraction of the content width the title tree takes. A ratio, not a length:
/// it is already DPI-independent and must NOT be scaled.
pub const TREE_FRAC: f64 = 0.464;
/// Fraction of the window height the log takes on the tree page. Also a ratio.
pub const LOG_FRAC: f64 = 0.32;

/// Settings window outer size, and About window outer size.
pub const PREFS_W: i32 = 680;
pub const PREFS_H: i32 = 520;
pub const ABOUT_W: i32 = 420;
pub const ABOUT_H: i32 = 260;

// ── the scale ─────────────────────────────────────────────────────────────

/// A DPI, and the one operation the layout needs from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    dpi: u32,
}

impl Scale {
    /// A scale for the given DPI.
    ///
    /// `GetDpiForWindow` returns 0 for a window that does not exist yet — which
    /// happens for real, because `WM_GETMINMAXINFO` arrives before `WM_CREATE`.
    /// A zero would collapse the whole layout to nothing, so it is clamped to
    /// the baseline. The upper clamp is a sanity rail: Windows tops out at 480
    /// DPI (500%).
    #[must_use]
    pub fn new(dpi: u32) -> Self {
        Self {
            dpi: dpi.clamp(BASE_DPI, 960),
        }
    }

    #[must_use]
    pub const fn dpi(self) -> u32 {
        self.dpi
    }

    /// Convert a 96-DPI baseline length to physical pixels.
    ///
    /// Rounds half away from zero, matching Win32's own `MulDiv`, so a value
    /// scaled here and one scaled by the OS (a themed glyph, a system metric)
    /// agree instead of drifting a pixel apart.
    #[must_use]
    pub fn px(self, v: i32) -> i32 {
        let n = v as i64 * self.dpi as i64;
        let d = BASE_DPI as i64;
        let rounded = if n >= 0 {
            (n + d / 2) / d
        } else {
            (n - d / 2) / d
        };
        rounded as i32
    }
}

/// One control's rectangle, in physical pixels relative to the client area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    #[must_use]
    const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }
}

/// Default window client size at the given DPI.
#[must_use]
pub fn default_size(dpi: u32) -> (i32, i32) {
    let s = Scale::new(dpi);
    (s.px(W), s.px(H))
}

/// Minimum window *track* size at the given DPI. Fed to `WM_GETMINMAXINFO`,
/// which speaks in outer-window pixels, so `windows.rs` adds the frame.
#[must_use]
pub fn min_size(dpi: u32) -> (i32, i32) {
    let s = Scale::new(dpi);
    (s.px(MIN_W), s.px(MIN_H))
}

// ── the main window ───────────────────────────────────────────────────────

/// Every rectangle the main window needs, for one DPI and one client size.
///
/// All four pages are laid out on every pass, exactly as the shell has always
/// done: only one page is visible at a time, and computing the hidden ones
/// costs nothing while keeping the show/hide logic free of geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainLayout {
    pub log: Rect,

    // empty page
    pub empty_head: Rect,
    pub empty_sub: Rect,
    pub btn_open: Rect,

    // titles page
    pub tree: Rect,
    pub grp_out: Rect,
    pub edit_out: Rect,
    pub btn_browse: Rect,
    pub cmb_format: Rect,
    pub btn_run: Rect,
    pub btn_eject: Rect,
    pub grp_info: Rect,
    pub detail: Rect,

    // progress page
    pub grp_prog: Rect,
    /// `(label, value)` per information row, in order.
    pub info_rows: Vec<(Rect, Rect)>,
    pub lbl_saving_cur: Rect,
    pub lbl_cur: Rect,
    pub bar_cur: Rect,
    pub lbl_saving_all: Rect,
    pub lbl_all: Rect,
    pub bar_all: Rect,
    pub btn_cancel: Rect,

    // result page
    pub result_head: Rect,
    pub result_line: Rect,
    pub btn_reveal: Rect,
    pub btn_done: Rect,
}

/// The page-dependent inputs to the layout, kept in one struct so the signature
/// stays readable and a caller cannot silently swap two booleans.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MainState {
    pub page: Page,
    /// Whether the overall-progress bar is showing (a multi-title rip).
    pub two_bars: bool,
    pub log_hidden: bool,
    /// Number of rows in the information group on the progress page.
    pub info_rows: usize,
}

/// Lay the main window out.
///
/// `cw`/`ch` are the **physical** client size, as `WM_SIZE` reports it, and the
/// returned rectangles are physical too. The proportional parts (`TREE_FRAC`,
/// `LOG_FRAC`) are applied to that physical size and so need no scaling; every
/// fixed length, including the minimum sizes the clamps enforce, goes through
/// `Scale::px`.
#[must_use]
pub fn main_layout(dpi: u32, cw: i32, ch: i32, st: MainState) -> MainLayout {
    let s = Scale::new(dpi);
    let pad = s.px(PAD);
    let tb = s.px(TB_H);

    // The log takes a fixed share of the height; more while ripping, and on
    // the result page it fills everything the fixed-height panel leaves.
    let page_h = match st.page {
        Page::Progress => {
            if st.two_bars {
                s.px(PROG_H)
            } else {
                s.px(PROG_H_ONE)
            }
        }
        Page::Result => s.px(RESULT_H),
        _ => 0,
    };
    let log_min = s.px(120);
    let log_h = if st.log_hidden {
        0
    } else if page_h > 0 {
        (ch - tb - page_h - pad * 3).max(log_min)
    } else {
        ((ch as f64 * LOG_FRAC) as i32).max(log_min)
    };

    let top_y = tb + pad;
    let top_h = (ch - log_h - pad * 3 - tb).max(s.px(80));
    let log_y = ch - pad - log_h;

    let log = Rect::new(pad, log_y, cw - pad * 2, log_h);

    // ── empty page ──
    let cy = top_y + top_h / 2;
    let empty_head = Rect::new(pad, cy - s.px(50), cw - pad * 2, s.px(26));
    let empty_sub = Rect::new(pad, cy - s.px(22), cw - pad * 2, s.px(20));
    // Centred as `(cw - w) / 2`, not `cw / 2 - w / 2`: the latter is a pixel
    // off centre whenever the scaled width rounds to an odd number, which it
    // does at 125%.
    let open_w = s.px(180);
    let btn_open = Rect::new((cw - open_w) / 2, cy + s.px(16), open_w, s.px(30));

    // ── titles page ──
    let tree_w = ((cw - pad * 2) as f64 * TREE_FRAC) as i32;
    let tree = Rect::new(pad, top_y, tree_w, top_h);
    let rx = pad + tree_w + pad;
    let rw = cw - rx - pad;
    let out_h = s.px(OUT_H);
    let grp_out = Rect::new(rx, top_y, rw, out_h);
    let edit_out = Rect::new(rx + s.px(12), top_y + s.px(22), rw - s.px(60), s.px(23));
    let btn_browse = Rect::new(rx + rw - s.px(44), top_y + s.px(21), s.px(34), s.px(25));
    let cmb_format = Rect::new(rx + s.px(12), top_y + s.px(56), rw - s.px(150), s.px(24));
    let btn_run = Rect::new(rx + rw - s.px(128), top_y + s.px(54), s.px(116), s.px(28));
    let btn_eject = Rect::new(rx + s.px(12), top_y + out_h + s.px(4), s.px(110), s.px(26));
    let info_y = top_y + out_h + pad + s.px(30);
    let info_h = (top_h - out_h - pad - s.px(30)).max(s.px(60));
    let grp_info = Rect::new(rx, info_y, rw, info_h);
    let detail = Rect::new(
        rx + s.px(10),
        info_y + s.px(20),
        rw - s.px(20),
        (info_h - s.px(32)).max(s.px(20)),
    );

    // ── progress page ──
    let gh = s.px(132);
    let grp_prog = Rect::new(pad, top_y, cw - pad * 2, gh);
    let row_h = s.px(15);
    let info_rows = (0..st.info_rows)
        .map(|i| {
            let yy = top_y + s.px(20) + i as i32 * row_h;
            (
                Rect::new(pad + s.px(6), yy, s.px(110), row_h),
                Rect::new(pad + s.px(124), yy, cw - pad * 2 - s.px(140), row_h),
            )
        })
        .collect();
    let bar1_y = top_y + gh + s.px(22);
    let cap_w = s.px(340);
    let cap_h = s.px(16);
    let cap_dy = s.px(18);
    let lbl_saving_cur = Rect::new(pad, bar1_y - cap_dy, cap_w, cap_h);
    let lbl_cur = Rect::new(cw - pad - cap_w, bar1_y - cap_dy, cap_w, cap_h);
    let bar_cur = Rect::new(pad, bar1_y, cw - pad * 2, s.px(20));
    let bar2_y = bar1_y + s.px(46);
    let lbl_saving_all = Rect::new(pad, bar2_y - cap_dy, cap_w, cap_h);
    let lbl_all = Rect::new(cw - pad - cap_w, bar2_y - cap_dy, cap_w, cap_h);
    let bar_all = Rect::new(pad, bar2_y, cw - pad * 2, s.px(20));
    let btn_cancel = Rect::new(
        cw - pad - s.px(120),
        top_y + page_h - s.px(36),
        s.px(120),
        s.px(30),
    );

    // ── result page ──
    let result_head = Rect::new(pad, top_y + s.px(26), cw - pad * 2, s.px(26));
    let result_line = Rect::new(pad, top_y + s.px(58), cw - pad * 2, s.px(20));
    // The pair straddles the centre with a fixed gap between them, so they stay
    // symmetric at any DPI instead of drifting as each width rounds.
    let res_w = s.px(170);
    let res_gap = s.px(20);
    let res_y = top_y + s.px(100);
    let res_h = s.px(32);
    let btn_reveal = Rect::new(cw / 2 - res_gap / 2 - res_w, res_y, res_w, res_h);
    let btn_done = Rect::new(cw / 2 + res_gap / 2, res_y, res_w, res_h);

    MainLayout {
        log,
        empty_head,
        empty_sub,
        btn_open,
        tree,
        grp_out,
        edit_out,
        btn_browse,
        cmb_format,
        btn_run,
        btn_eject,
        grp_info,
        detail,
        grp_prog,
        info_rows,
        lbl_saving_cur,
        lbl_cur,
        bar_cur,
        lbl_saving_all,
        lbl_all,
        bar_all,
        btn_cancel,
        result_head,
        result_line,
        btn_reveal,
        btn_done,
    }
}

// ── the Settings and About windows ────────────────────────────────────────

/// The Settings window's chrome: the tab control and the button row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefsLayout {
    pub tab: Rect,
    pub btn_ok: Rect,
    pub btn_cancel: Rect,
}

/// Lay the Settings chrome out inside a client area of `cw` × `ch`.
#[must_use]
pub fn prefs_layout(dpi: u32, cw: i32, ch: i32) -> PrefsLayout {
    let s = Scale::new(dpi);
    let m = s.px(10);
    let bw = s.px(96);
    let bh = s.px(28);
    PrefsLayout {
        tab: Rect::new(m, m, cw - m * 2, ch - s.px(56)),
        btn_ok: Rect::new(cw - s.px(108), ch - s.px(38), bw, bh),
        btn_cancel: Rect::new(cw - s.px(212), ch - s.px(38), bw, bh),
    }
}

/// The About window's only repositioned control.
#[must_use]
pub fn about_close_rect(dpi: u32, cw: i32, ch: i32) -> Rect {
    let s = Scale::new(dpi);
    Rect::new(cw - s.px(110), ch - s.px(38), s.px(96), s.px(28))
}

/// Metrics for one row of the Settings form: the right-aligned label gutter and
/// the vertical step between rows. Kept here so the form builder in
/// `windows.rs` holds no bare pixel constant either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormMetrics {
    pub top: i32,
    pub gutter: i32,
    pub width: i32,
    pub field_h: i32,
    pub row_step: i32,
    pub check_step: i32,
    pub button_step: i32,
    pub note_step: i32,
    pub gap: i32,
}

#[must_use]
pub fn form_metrics(dpi: u32) -> FormMetrics {
    let s = Scale::new(dpi);
    FormMetrics {
        top: s.px(16),
        gutter: s.px(250),
        width: s.px(620),
        field_h: s.px(22),
        row_step: s.px(30),
        check_step: s.px(28),
        button_step: s.px(32),
        note_step: s.px(38),
        gap: s.px(12),
    }
}

#[cfg(test)]
mod tests {
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

    fn titles(info_rows: usize) -> MainState {
        MainState {
            page: Page::Titles,
            two_bars: false,
            log_hidden: false,
            info_rows,
        }
    }

    /// The default window at 96 DPI — the geometry that shipped, unchanged.
    /// Every other DPI case is measured against these numbers.
    #[test]
    fn titles_page_at_96_dpi_is_the_shipped_geometry() {
        let l = main_layout(96, 1180, 760, titles(0));

        // ch * 0.32 = 243.2 -> 243; log_y = 760 - 8 - 243 = 509.
        assert_eq!(l.log, Rect::new(8, 509, 1164, 243));
        // top_y = TB_H + PAD = 12; top_h = 760 - 243 - 24 - 4 = 489.
        assert_eq!(l.tree, Rect::new(8, 12, 540, 489)); // 1164 * 0.464 = 540.09
        // rx = 8 + 540 + 8 = 556; rw = 1180 - 556 - 8 = 616.
        assert_eq!(l.grp_out, Rect::new(556, 12, 616, 110));
        assert_eq!(l.edit_out, Rect::new(568, 34, 556, 23));
        assert_eq!(l.btn_browse, Rect::new(1128, 33, 34, 25));
        assert_eq!(l.cmb_format, Rect::new(568, 68, 466, 24));
        assert_eq!(l.btn_run, Rect::new(1044, 66, 116, 28));
        assert_eq!(l.btn_eject, Rect::new(568, 126, 110, 26));
        // info_y = 12 + 110 + 8 + 30 = 160; info_h = 489 - 110 - 8 - 30 = 341.
        assert_eq!(l.grp_info, Rect::new(556, 160, 616, 341));
        assert_eq!(l.detail, Rect::new(566, 180, 596, 309));
    }

    /// 125% — the single most common Windows laptop setting. Same window in
    /// physical pixels (1475 × 950), every fixed length 1.25× the baseline.
    #[test]
    fn titles_page_at_120_dpi() {
        let l = main_layout(120, 1475, 950, titles(0));
        let pad = 10; // 8 * 1.25

        // 950 * 0.32 = 304; log_y = 950 - 10 - 304 = 636.
        assert_eq!(l.log, Rect::new(pad, 636, 1455, 304));
        // top_y = 5 + 10 = 15; top_h = 950 - 304 - 30 - 5 = 611.
        assert_eq!(l.tree, Rect::new(pad, 15, 675, 611)); // 1455 * 0.464 = 675.1
        // rx = 10 + 675 + 10 = 695; rw = 1475 - 695 - 10 = 770.
        assert_eq!(l.grp_out, Rect::new(695, 15, 770, 138)); // OUT_H 110 -> 138 (137.5)
        assert_eq!(l.edit_out, Rect::new(710, 43, 695, 29));
        assert_eq!(l.btn_browse, Rect::new(1410, 41, 43, 31));
        // 150 * 1.25 = 187.5, which MulDiv rounds up to 188: 770 - 188 = 582.
        assert_eq!(l.cmb_format, Rect::new(710, 85, 582, 30));
        assert_eq!(l.btn_run, Rect::new(1305, 83, 145, 35));
        assert_eq!(l.btn_eject, Rect::new(710, 158, 138, 33));
        // info_y = 15 + 138 + 10 + 38 = 201; info_h = 611 - 138 - 10 - 38 = 425.
        assert_eq!(l.grp_info, Rect::new(695, 201, 770, 425));
        assert_eq!(l.detail, Rect::new(708, 226, 745, 385));
    }

    /// 150%.
    #[test]
    fn titles_page_at_144_dpi() {
        let l = main_layout(144, 1770, 1140, titles(0));
        let pad = 12;

        // 1140 * 0.32 = 364.8 -> 364; log_y = 1140 - 12 - 364 = 764.
        assert_eq!(l.log, Rect::new(pad, 764, 1746, 364));
        // top_y = 6 + 12 = 18; top_h = 1140 - 364 - 36 - 6 = 734.
        assert_eq!(l.tree, Rect::new(pad, 18, 810, 734)); // 1746 * 0.464 = 810.1
        // rx = 12 + 810 + 12 = 834; rw = 1770 - 834 - 12 = 924.
        assert_eq!(l.grp_out, Rect::new(834, 18, 924, 165));
        assert_eq!(l.edit_out, Rect::new(852, 51, 834, 35));
        assert_eq!(l.btn_browse, Rect::new(1692, 50, 51, 38));
        assert_eq!(l.cmb_format, Rect::new(852, 102, 699, 36));
        assert_eq!(l.btn_run, Rect::new(1566, 99, 174, 42));
        assert_eq!(l.btn_eject, Rect::new(852, 189, 165, 39));
        // info_y = 18 + 165 + 12 + 45 = 240; info_h = 734 - 165 - 12 - 45 = 512.
        assert_eq!(l.grp_info, Rect::new(834, 240, 924, 512));
        assert_eq!(l.detail, Rect::new(849, 270, 894, 464));
    }

    /// 200% — every fixed length exactly doubles, which makes this the case
    /// where an unscaled constant would be most obvious.
    #[test]
    fn titles_page_at_192_dpi() {
        let l = main_layout(192, 2360, 1520, titles(0));
        let pad = 16;

        // 1520 * 0.32 = 486.4 -> 486; log_y = 1520 - 16 - 486 = 1018.
        assert_eq!(l.log, Rect::new(pad, 1018, 2328, 486));
        // top_y = 8 + 16 = 24; top_h = 1520 - 486 - 48 - 8 = 978.
        assert_eq!(l.tree, Rect::new(pad, 24, 1080, 978)); // 2328 * 0.464 = 1080.2
        // rx = 16 + 1080 + 16 = 1112; rw = 2360 - 1112 - 16 = 1232.
        assert_eq!(l.grp_out, Rect::new(1112, 24, 1232, 220));
        assert_eq!(l.edit_out, Rect::new(1136, 68, 1112, 46));
        assert_eq!(l.btn_browse, Rect::new(2256, 66, 68, 50));
        assert_eq!(l.cmb_format, Rect::new(1136, 136, 932, 48));
        assert_eq!(l.btn_run, Rect::new(2088, 132, 232, 56));
        assert_eq!(l.btn_eject, Rect::new(1136, 252, 220, 52));
        // info_y = 24 + 220 + 16 + 60 = 320; info_h = 978 - 220 - 16 - 60 = 682.
        assert_eq!(l.grp_info, Rect::new(1112, 320, 1232, 682));
        assert_eq!(l.detail, Rect::new(1132, 360, 1192, 618));
    }

    /// The progress page: two bars, seven information rows.
    #[test]
    fn progress_page_scales() {
        let st = MainState {
            page: Page::Progress,
            two_bars: true,
            log_hidden: false,
            info_rows: 7,
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
        };
        let l = main_layout(96, 1180, 760, st);
        assert_eq!(l.result_head, Rect::new(8, 38, 1164, 26));
        assert_eq!(l.result_line, Rect::new(8, 70, 1164, 20));
        assert_eq!(l.btn_reveal, Rect::new(410, 112, 170, 32));
        assert_eq!(l.btn_done, Rect::new(600, 112, 170, 32));
        // The Result page reserves RESULT_H and the log takes the remainder.
        // Without this the "delete the Page::Result arm" mutant is invisible:
        // page_h would fall to 0 and log_h would switch to the LOG_FRAC branch,
        // moving nothing the other assertions look at.
        // 760 - 4 - 200 - 24 = 532.
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
                },
            );
            // The headline and the button share the window's horizontal centre.
            assert_eq!(l.empty_head.x + l.empty_head.w / 2, w / 2, "dpi {dpi}");
            assert_eq!(l.btn_open.x + l.btn_open.w / 2, w / 2, "dpi {dpi}");
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
            },
        );
        // cy = 24 + 978/2 = 513.
        assert_eq!(l.empty_head, Rect::new(16, 413, 2328, 52));
        assert_eq!(l.empty_sub, Rect::new(16, 469, 2328, 40));
        assert_eq!(l.btn_open, Rect::new(1000, 545, 360, 60));
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
                },
            );
            assert_eq!(l.log.h, 0, "dpi {dpi}");
            let s = Scale::new(dpi);
            // top_h = ch - 0 - pad*3 - tb, and the tree fills it.
            assert_eq!(l.tree.h, h - s.px(PAD) * 3 - s.px(TB_H), "dpi {dpi}");
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
                        },
                    );
                    for (name, r) in [
                        ("log", l.log),
                        ("tree", l.tree),
                        ("grp_out", l.grp_out),
                        ("edit_out", l.edit_out),
                        ("cmb_format", l.cmb_format),
                        ("btn_run", l.btn_run),
                        ("grp_info", l.grp_info),
                        ("detail", l.detail),
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
                        l.grp_info.x + l.grp_info.w <= w,
                        "info overruns at dpi {dpi}"
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
        let b = main_layout(192, 2360, 1520, st);
        for (name, x, y) in [
            ("log", a.log, b.log),
            ("grp_out", a.grp_out, b.grp_out),
            ("edit_out", a.edit_out, b.edit_out),
            ("btn_run", a.btn_run, b.btn_run),
            ("btn_eject", a.btn_eject, b.btn_eject),
            ("grp_info", a.grp_info, b.grp_info),
            ("detail", a.detail, b.detail),
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
}
