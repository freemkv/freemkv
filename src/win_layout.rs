//! DPI-aware geometry for the Windows shell — pure arithmetic, no Win32.
//!
//! Every constant in this module is expressed at the 96-DPI baseline;
//! `Scale::px` converts a baseline value to physical pixels for a given DPI,
//! and `main_layout` turns a DPI plus a client size into the complete set of
//! rectangles used to position controls. Kept as pure functions of
//! `(dpi, client size, page state)` so the arithmetic is testable without a
//! live `HWND`; not `cfg(windows)` since it holds no Win32 types.

use crate::ui::Page;

/// The DPI every constant in this file is expressed in. Windows' own baseline:
/// 100% scaling is 96 DPI, 125% is 120, 150% is 144, 200% is 192.
pub const BASE_DPI: u32 = 96;

// ── the 96-DPI baseline: same proportions as the macOS shell, so the two ──
// look like one product — the selection bar, the title tree over the full
// width, the output area under it, and the log taking a share of the height.

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
/// The selection bar over the title tree: Titles, Audio and Subtitles on one row.
pub const PICK_H: i32 = 30;
/// The selection bar's chooser widths, and the gap after each.
pub const PICK_TITLES_W: i32 = 190;
pub const PICK_MENU_W: i32 = 170;
pub const PICK_GAP: i32 = 18;
/// The output area under the tree: the "Output" label, the folder / browse /
/// format / Run row, and the free-space line under it.
pub const BAR_H: i32 = 70;
pub const BAR_ROW_Y: i32 = 22;
pub const BAR_ROW_H: i32 = 28;
/// Format dropdown and Run button widths on the output row.
pub const FORMAT_W: i32 = 220;
pub const RUN_W: i32 = 110;
/// Fraction of the window height the log takes on the tree page. A ratio, not
/// a length: it is already DPI-independent and must NOT be scaled.
pub const LOG_FRAC: f64 = 0.24;

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
    /// the baseline. The upper clamp is a sanity rail: Windows tops out at 960
    /// DPI (1000%).
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

    /// The inverse of [`px`](Self::px): physical pixels back to the 96-DPI
    /// baseline, rounded the same way.
    #[must_use]
    pub fn unpx(self, v: i32) -> i32 {
        let n = v as i64 * BASE_DPI as i64;
        let d = self.dpi as i64;
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
    /// "Open disc" — the left half of the empty state's button pair.
    pub btn_open_disc: Rect,
    /// "Open file or ISO…" — the right half.
    pub btn_open: Rect,

    // titles page
    /// The selection bar: each chooser's label and the chooser itself.
    pub pick_titles_lbl: Rect,
    pub pick_titles: Rect,
    pub pick_audio_lbl: Rect,
    pub pick_audio: Rect,
    pub pick_subs_lbl: Rect,
    pub pick_subs: Rect,
    /// Eject, at the right end of the selection bar.
    pub btn_eject: Rect,
    /// The tree with its header strip, over the full width.
    pub tree: Rect,
    pub lbl_out: Rect,
    pub edit_out: Rect,
    pub btn_browse: Rect,
    pub cmb_format: Rect,
    pub btn_run: Rect,
    /// The free-space line under the output row.
    pub lbl_free: Rect,

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
    /// How wide the selection bar's three labels draw, in physical pixels.
    pub pick_labels: [i32; 3],
}

/// Lay the main window out.
///
/// `cw`/`ch` are the **physical** client size, as `WM_SIZE` reports it, and the
/// returned rectangles are physical too. The proportional part (`LOG_FRAC`) is
/// applied to that physical size and so needs no scaling; every
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
    // TWO buttons: the empty page offers "Open disc" as well as "Open file or
    // ISO…" so its own headline's source is reachable without the menu bar. The
    // pair straddles the centre with a fixed gap (result page's pattern), symmetric.
    let open_w = s.px(180);
    let open_gap = s.px(16);
    let open_y = cy + s.px(16);
    let open_h = s.px(30);
    let btn_open_disc = Rect::new(cw / 2 - open_gap / 2 - open_w, open_y, open_w, open_h);
    let btn_open = Rect::new(cw / 2 + open_gap / 2, open_y, open_w, open_h);

    // ── titles page ──
    // The selection bar: label, chooser, gap, for each of the three.
    let pick_y = top_y;
    let mut x = pad;
    let mut chooser = |label_w: i32, w: i32| {
        let lbl = Rect::new(x, pick_y + s.px(7), label_w, s.px(16));
        x += label_w + s.px(4);
        let c = Rect::new(x, pick_y + s.px(3), s.px(w), s.px(24));
        x += s.px(w) + s.px(PICK_GAP);
        (lbl, c)
    };
    let (pick_titles_lbl, pick_titles) = chooser(st.pick_labels[0], PICK_TITLES_W);
    let (pick_audio_lbl, pick_audio) = chooser(st.pick_labels[1], PICK_MENU_W);
    let (pick_subs_lbl, pick_subs) = chooser(st.pick_labels[2], PICK_MENU_W);
    let btn_eject = Rect::new(cw - pad - s.px(110), pick_y + s.px(2), s.px(110), s.px(26));

    let bar_h = s.px(BAR_H);
    let tree_y = top_y + s.px(PICK_H);
    let tree = Rect::new(
        pad,
        tree_y,
        cw - pad * 2,
        (top_h - s.px(PICK_H) - pad - bar_h).max(s.px(40)),
    );

    // The output area: folder (flexible), browse, format and Run on one row.
    let bar_y = top_y + top_h - bar_h;
    let row_y = bar_y + s.px(BAR_ROW_Y);
    let gap = pad;
    let run_w = s.px(RUN_W);
    let fmt_w = s.px(FORMAT_W);
    let browse_w = s.px(34);
    let btn_run = Rect::new(cw - pad - run_w, row_y, run_w, s.px(BAR_ROW_H));
    let cmb_format = Rect::new(btn_run.x - gap - fmt_w, row_y + s.px(2), fmt_w, s.px(24));
    let btn_browse = Rect::new(
        cmb_format.x - gap - browse_w,
        row_y + s.px(1),
        browse_w,
        s.px(26),
    );
    let field_w = (btn_browse.x - gap - pad).max(s.px(60));
    let edit_out = Rect::new(pad, row_y + s.px(2), field_w, s.px(23));
    let lbl_out = Rect::new(pad, bar_y + s.px(3), field_w, s.px(16));
    let lbl_free = Rect::new(
        pad + s.px(2),
        row_y + s.px(BAR_ROW_H) + s.px(4),
        field_w,
        s.px(16),
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
        btn_open_disc,
        btn_open,
        pick_titles_lbl,
        pick_titles,
        pick_audio_lbl,
        pick_audio,
        pick_subs_lbl,
        pick_subs,
        btn_eject,
        tree,
        lbl_out,
        edit_out,
        btn_browse,
        cmb_format,
        btn_run,
        lbl_free,
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

/// The number of rows in a dropdown's single vertical list.
///
/// Native popup menus handle lists taller than the screen with their own scrolling;
/// keeping every item in one list ensures all dropdowns open straight down consistently.
#[must_use]
pub fn menu_rows(items: usize) -> usize {
    items.max(1)
}

// ── the title tree's columns ──────────────────────────────────────────────

/// Height of the column header strip over the title tree.
pub const TREE_HEAD_H: i32 = 24;
/// Title-tree row height: room above and below the text, as the macOS outline has.
pub const TREE_ROW_H: i32 = 22;
/// How far each tree level is indented.
pub const TREE_INDENT: i32 = 18;
/// The tick-and-expander column ahead of the core's columns: room for the
/// expander and tick box of a stream row two levels down.
pub const COL_TICK_W: i32 = 84;
/// Narrowest a column may be dragged.
pub const COL_MIN_W: i32 = 36;

/// The header strip and the tree under it, splitting the tree's area.
#[must_use]
pub fn split_tree_header(area: Rect, dpi: u32) -> (Rect, Rect) {
    let hh = Scale::new(dpi).px(TREE_HEAD_H).min(area.h);
    (
        Rect::new(area.x, area.y, area.w, hh),
        Rect::new(area.x, area.y + hh, area.w, area.h - hh),
    )
}

/// A horizontal extent, `x .. x + w`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub x: i32,
    pub w: i32,
}

/// Where each column falls across the tree's client area, in the control's own
/// (logical) coordinates. Under a right-to-left locale the header and tree are
/// mirrored windows (`WS_EX_LAYOUTRTL`), so this same layout lands mirrored on
/// screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeColumns {
    /// The expanders and tick boxes: the tree's own indented part.
    pub tick: Span,
    /// One span per core column (`ui::tree_columns`), in order.
    pub cols: Vec<Span>,
}

/// Lay the columns out across a client `client_w` wide: the tick column first, then each core
/// column at its width (`widths`, at the 96-DPI baseline), fitted by the core's
/// [`crate::ui::fit_column_widths`]: the `flex` one takes what the others leave and, short of
/// room, the text columns give way while lengths and sizes keep theirs.
#[must_use]
pub fn tree_columns(dpi: u32, client_w: i32, widths: &[i32], flex: usize) -> TreeColumns {
    let s = Scale::new(dpi);
    let tick = s.px(COL_TICK_W);
    let mut cols = crate::ui::tree_columns();
    for (i, c) in cols.iter_mut().enumerate() {
        c.flex = i == flex;
        c.min = f64::from(s.px((c.min.round() as i32).max(COL_MIN_W)));
    }
    let base: Vec<f64> = widths.iter().map(|&v| f64::from(s.px(v))).collect();
    let fitted = crate::ui::fit_column_widths(&cols, &base, f64::from(client_w - tick));
    let mut x = tick;
    let cols = fitted
        .into_iter()
        .map(|w| {
            let span = Span {
                x,
                w: w.floor() as i32,
            };
            x += span.w;
            span
        })
        .collect();
    TreeColumns {
        tick: Span { x: 0, w: tick },
        cols,
    }
}

/// The header's item widths for `cols`: the tick column, then one per core
/// column. The header spans the tree's whole outer width while the columns are
/// measured in its client area, which starts `inset` in (the border); the last
/// item runs on over the scroll bar.
#[must_use]
pub fn header_widths(cols: &TreeColumns, header_w: i32, inset: i32) -> Vec<i32> {
    let mut out = vec![cols.tick.w + inset];
    out.extend(cols.cols.iter().map(|c| c.w));
    let before: i32 = out[..out.len() - 1].iter().sum();
    if let Some(last) = out.last_mut() {
        *last = (header_w - before).max(0);
    }
    out
}

/// The baseline widths a finished header drag leaves, from the item dragged
/// (`item`, in header order: 0 is the tick column) and its new width. A column
/// left of the flexible one just takes its new width, the flexible one giving
/// way; from the flexible one on, a divider trades width with its right-hand
/// neighbour. The tick column and the last edge are pinned.
#[must_use]
pub fn drag_column(dpi: u32, cols: &TreeColumns, flex: usize, item: usize, new_w: i32) -> Vec<i32> {
    let s = Scale::new(dpi);
    let mut w: Vec<i32> = cols.cols.iter().map(|c| c.w).collect();
    if let Some(j) = item.checked_sub(1).filter(|&j| j < w.len()) {
        let delta = new_w - w[j];
        if j < flex {
            w[j] = new_w;
        } else if j + 1 < w.len() {
            w[j] = new_w;
            w[j + 1] -= delta;
        }
    }
    let mins = crate::ui::tree_columns()
        .into_iter()
        .map(|c| s.px((c.min.round() as i32).max(COL_MIN_W)));
    w.into_iter()
        .zip(mins)
        .map(|(v, min)| s.unpx(v.max(min)))
        .collect()
}

/// Whether a locale tag (`"ar"`, `"he-IL"`, `"fa_IR"`) is written right to
/// left, so the shell lays its title tree out mirrored.
#[must_use]
pub fn is_rtl_locale(tag: &str) -> bool {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        primary.as_str(),
        "ar" | "he" | "iw" | "fa" | "ur" | "ps" | "sd" | "ug" | "yi" | "dv" | "ckb"
    )
}

// ── the Settings form ─────────────────────────────────────────────────────

/// A dropdown's closed width and its open list's width, for a longest item
/// `text_w` pixels wide. The closed box grows from `control_w` to fit the
/// longest item, up to `max_w` (the room left on its row); the list is never
/// narrower than the box and always wide enough for every item in full.
#[must_use]
pub fn combo_widths(dpi: u32, control_w: i32, text_w: i32, max_w: i32) -> (i32, i32) {
    let s = Scale::new(dpi);
    // The arrow button and the box's own margins; the list's margins plus a
    // vertical scroll bar.
    let closed = (text_w + s.px(34)).clamp(control_w, max_w.max(control_w));
    let list = (text_w + s.px(30)).max(closed);
    (closed, list)
}

/// What asks the Settings form to commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormCommit {
    /// The OK button.
    Ok,
    /// Enter in a text field.
    Enter,
    /// Focus left a text field.
    FocusLost,
}

/// What a commit does: write the form to disk, and close the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitPlan {
    pub save: bool,
    pub close: bool,
}

/// The form's commit rule. OK saves and closes, as it always has. Enter and
/// leaving a text field save only what changed and keep the window open, as
/// the GTK shell's fields do; focus also leaves while the window is being
/// hidden, which is not an edit.
#[must_use]
pub fn form_commit(why: FormCommit, window_visible: bool, changed: bool) -> CommitPlan {
    match why {
        FormCommit::Ok => CommitPlan {
            save: true,
            close: true,
        },
        FormCommit::Enter => CommitPlan {
            save: changed,
            close: false,
        },
        FormCommit::FocusLost => CommitPlan {
            save: changed && window_visible,
            close: false,
        },
    }
}

#[cfg(test)]
#[path = "win_layout_tests.rs"]
mod tests;
