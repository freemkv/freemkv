//! Windows Win32 shell over the shared `ui`/`engine`/`settings` core.
//!
//! ```text
//! 1. render   App::view() -> View     assign strings/flags to widgets
//! 2. dispatch App::dispatch(cmd)      on any click, menu pick or keystroke
//! 3. perform  the returned Effects    the platform-only actions
//! ```
//!
//! Writes no logic and holds no state duplicating `App`; the only cache is
//! render memos to skip rebuilding unchanged controls. Uses `winsafe` with
//! stock common controls and Windows-native conventions, not macOS's.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use winsafe::{self as w, co, gui, msg, prelude::*};

use crate::ui::{App, Check, Cmd, Effect, LogKind, LogLine, Page, Row, View};

// Window geometry: rects come from `win_layout` (DPI-aware, PerMonitorV2);
// never position from a bare constant or it'll be wrong at non-100% scaling.
// Proportions mirror the macOS shell (selection bar, full-width tree, output row).

use crate::win_layout as lay;

/// Poll interval for a running job, in milliseconds. Matches the macOS timer.
const TICK_MS: u32 = 200;
const TIMER_TICK: usize = 1;
/// Drain interval for worker-thread messages (the keydb update).
const TIMER_DRAIN: usize = 2;
/// One-shot: the launch probe for a disc already in the drive. See
/// `Shell::open_disc` and the `wm_create` handler.
const TIMER_LAUNCH_PROBE: usize = 4;
/// How long after the window exists the launch probe fires. Long enough for
/// the empty page to be on screen first, short enough to feel like launch.
const LAUNCH_PROBE_MS: u32 = 200;

/// Resource id of the application icon group embedded by `build.rs`. Must stay
/// in step with `IDI_APP` there — there is no shared header between a build
/// script and the crate it builds, so the two constants are the contract.
const IDI_APP: u16 = 1;

// ── control and menu ids ──────────────────────────────────────────────────

const ID_TREE: u16 = 1000;
const ID_LOG: u16 = 1001;
const ID_FORMAT: u16 = 1003;
const ID_OUTDIR: u16 = 1004;
const ID_BROWSE: u16 = 1005;
const ID_RUN: u16 = 1006;
const ID_CANCEL: u16 = 1007;
const ID_REVEAL: u16 = 1008;
const ID_DONE: u16 = 1009;
const ID_OPEN_EMPTY: u16 = 1010;
/// "Open disc" on the empty page — the second half of the pair, wired to the
/// same handler as File ▸ Open disc.
const ID_OPEN_DISC_EMPTY: u16 = 1014;
const ID_BAR_CUR: u16 = 1011;
const ID_BAR_ALL: u16 = 1012;
const ID_EJECT: u16 = 1013;
const ID_TREE_HEAD: u16 = 1015;
const ID_PICK_TITLES: u16 = 1016;
const ID_PICK_AUDIO: u16 = 1017;
const ID_PICK_SUBS: u16 = 1018;

// Menu command ids. `cmd_for` maps these to core commands, so the enable/
// disable rule lives in `ui::blocked_while_running` and cannot disagree with
// the macOS shell.
const IDM_OPEN: u16 = 2001;
const IDM_OPEN_DISC: u16 = 2002;
const IDM_CLOSE: u16 = 2003;
const IDM_SET_OUTPUT: u16 = 2004;
const IDM_START_RIP: u16 = 2005;
const IDM_EJECT: u16 = 2006;
const IDM_SETTINGS: u16 = 2007;
const IDM_EXIT: u16 = 2008;
const IDM_COPY: u16 = 2009;
const IDM_SELECT_ALL_TEXT: u16 = 2010;
const IDM_SELECT_ALL: u16 = 2011;
const IDM_SELECT_NONE: u16 = 2012;
const IDM_INVERT: u16 = 2013;
const IDM_TOGGLE_LOG: u16 = 2014;
const IDM_CLEAR_LOG: u16 = 2015;
const IDM_DOCS: u16 = 2016;
const IDM_CHECK_UPDATES: u16 = 2017;
const IDM_ABOUT: u16 = 2018;

// First command id of a language checklist popup; entry *i* is `+ i`. Well
// clear of the `IDM_*` block: the checklist uses `TPM::RETURNCMD`, which
// returns the chosen id directly and posts no `WM_COMMAND`.
const IDM_LANG_BASE: u16 = 3000;

/// Every menu id that maps to a core command, in menu order — so the
/// enable/disable pass and the test harness can walk them without a second,
/// drifting list.
const MENU_CMD_IDS: &[u16] = &[
    IDM_OPEN,
    IDM_OPEN_DISC,
    IDM_CLOSE,
    IDM_SET_OUTPUT,
    IDM_START_RIP,
    IDM_EJECT,
    IDM_SETTINGS,
    IDM_EXIT,
    IDM_SELECT_ALL,
    IDM_SELECT_NONE,
    IDM_INVERT,
    IDM_TOGGLE_LOG,
    IDM_CLEAR_LOG,
    IDM_DOCS,
    IDM_CHECK_UPDATES,
    IDM_ABOUT,
];

// Map a menu id to a core command. The RULE about what is available mid-rip
// lives in the core (`ui::blocked_while_running`); this shell only says
// which id means which command.
fn cmd_for(id: u16) -> Option<Cmd> {
    Some(match id {
        // Opening a disc is an Open for menu-enable purposes (blocked mid-rip).
        IDM_OPEN | IDM_OPEN_DISC => Cmd::Open,
        IDM_CLOSE => Cmd::Close,
        IDM_SET_OUTPUT => Cmd::SetOutput,
        IDM_START_RIP => Cmd::Run,
        IDM_EJECT => Cmd::Eject,
        IDM_SETTINGS => Cmd::Settings,
        IDM_EXIT => Cmd::Quit,
        IDM_SELECT_ALL => Cmd::SelectAll,
        IDM_SELECT_NONE => Cmd::SelectNone,
        IDM_INVERT => Cmd::Invert,
        IDM_TOGGLE_LOG => Cmd::ToggleLog,
        IDM_CLEAR_LOG => Cmd::ClearLog,
        IDM_DOCS => Cmd::Docs,
        IDM_CHECK_UPDATES => Cmd::CheckUpdates,
        IDM_ABOUT => Cmd::About,
        _ => return None,
    })
}

/// State-image indices in the tree's state image list. Index 0 means "no state
/// image" to the tree control, so the usable indices are 1-based.
const ST_UNCHECKED: u32 = 1;
const ST_CHECKED: u32 = 2;
const ST_MIXED: u32 = 3;
/// A row the core says carries no checkbox at all gets no state image, so the
/// disc root and the implicit Video rows show nothing tickable — the same
/// decision the macOS shell renders as a blank spacer cell.
const ST_NONE: u32 = 0;

fn state_for(check: Option<Check>) -> u32 {
    match check {
        None => ST_NONE,
        Some(Check::Off) => ST_UNCHECKED,
        Some(Check::On) => ST_CHECKED,
        Some(Check::Mixed) => ST_MIXED,
    }
}

/// The same three glyphs drawn disabled, at `ST_* + DISABLED_OFFSET`: a mirror row
/// (`Row::check_enabled == false`, the MPEG-2 extension) shows its base's tick greyed,
/// as the macOS and GTK shells disable the box.
const DISABLED_OFFSET: u32 = 3;

fn state_for_row(r: &Row) -> u32 {
    let s = state_for(r.check);
    if r.check_enabled || s == ST_NONE {
        s
    } else {
        s + DISABLED_OFFSET
    }
}

/// Development-only environment lookup. In a release build this always fails,
/// so the shipped app has no environment switches at all — the same rule the
/// macOS shell follows.
fn dev_env(key: &str) -> Result<String, std::env::VarError> {
    if cfg!(debug_assertions) {
        std::env::var(key)
    } else {
        Err(std::env::VarError::NotPresent)
    }
}

// Whether the launch probe (a disc already in the drive at startup) should
// run at all. Off under `cargo test` and every debug harness mode: those
// already load a fixture, mid-screenshot, or must never touch real hardware.
fn launch_probe_enabled() -> bool {
    !cfg!(test)
        && [
            "FMKV_OPEN",
            "FMKV_SELFTEST",
            "FMKV_SHOT",
            "FMKV_WIN",
            "FMKV_DUMP_MENUS",
            "FMKV_PAGE",
            "FMKV_GATE",
        ]
        .iter()
        .all(|k| dev_env(k).is_err())
}

// Win32 entry points winsafe does not wrap; declared directly (as `platform.rs`
// does for `GetDiskFreeSpaceExW`) rather than pulling in a second binding crate.

mod extra {
    use std::ffi::c_void;

    #[link(name = "user32")]
    unsafe extern "system" {
        /// Renders a window into a DC. This is the Win32 equivalent of the
        /// macOS shell's `cacheDisplayInRect:` — it asks the window to draw
        /// itself, so the screenshot harness needs no screen-capture
        /// permission and works on a window that is not frontmost.
        pub fn PrintWindow(hwnd: *mut c_void, hdc_blt: *mut c_void, flags: u32) -> i32;
        /// Classic (unthemed) checkbox glyph — the fallback when the user has
        /// theming switched off, so the tri-state ticks never come out blank.
        pub fn DrawFrameControl(hdc: *mut c_void, rc: *mut c_void, ty: u32, state: u32) -> i32;
        /// The DPI-aware `SystemParametersInfo`. The plain one answers for the
        /// *system* DPI whatever monitor the window is on, so a caption font
        /// read through it comes back the wrong size on every secondary
        /// display — the exact bug this file exists to avoid.
        pub fn SystemParametersInfoForDpi(
            action: u32,
            ui_param: u32,
            pv_param: *mut c_void,
            win_ini: u32,
            dpi: u32,
        ) -> i32;
        /// The system (primary-monitor) DPI. Needed before any window exists,
        /// which is when the shell has to choose its initial size.
        pub fn GetDpiForSystem() -> u32;
    }

    /// `SPI_GETNONCLIENTMETRICS` — the shell UI font lives in the returned
    /// `NONCLIENTMETRICS`.
    pub const SPI_GETNONCLIENTMETRICS: u32 = 0x0029;

    /// `PW_RENDERFULLCONTENT` — required to capture DirectComposition-rendered
    /// content; without it modern controls come back blank.
    pub const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

    pub const DFC_BUTTON: u32 = 4;
    pub const DFCS_BUTTONCHECK: u32 = 0x0000_0000;
    pub const DFCS_CHECKED: u32 = 0x0000_0400;
    pub const DFCS_BUTTON3STATE: u32 = 0x0000_0008;
    pub const DFCS_INACTIVE: u32 = 0x0000_0100;
}

// DPI: every physical length in this shell comes from `window_dpi`/`system_dpi`
// (`win_layout` does the arithmetic). `GetDpiForWindow` answers 0 for a window
// that does not exist yet — happens during `WM_GETMINMAXINFO` — so fall back.
#[must_use]
fn window_dpi(hwnd: &w::HWND) -> u32 {
    if let Some(d) = harness_dpi() {
        return d;
    }
    match hwnd.GetDpiForWindow() {
        0 => system_dpi(),
        d => d,
    }
}

// The primary monitor's DPI — the only figure available before any window
// exists, needed to pick the initial size and build the Settings/About forms.
#[must_use]
fn system_dpi() -> u32 {
    if let Some(d) = harness_dpi() {
        return d;
    }
    match unsafe { extra::GetDpiForSystem() } {
        0 => lay::BASE_DPI,
        d => d,
    }
}

// FMKV_DPI=144 lays the shell out as at that DPI (150%) whatever the display,
// so a headless capture can show another scale. Debug builds only (`dev_env`).
fn harness_dpi() -> Option<u32> {
    dev_env("FMKV_DPI").ok().and_then(|d| d.parse().ok())
}

thread_local! {
    // The UI font per DPI, kept alive for the life of the GUI thread. A
    // control keeps only a borrowed `HFONT`; deleting it while still
    // selected paints garbage, so ownership lives here instead.
    static UI_FONTS: RefCell<Vec<(u32, w::guard::DeleteObjectGuard<w::HFONT>)>> =
        const { RefCell::new(Vec::new()) };
}

// The shell UI font at `dpi`.
#[must_use]
fn ui_font(dpi: u32) -> Option<w::HFONT> {
    UI_FONTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((_, f)) = cache.iter().find(|(d, _)| *d == dpi) {
            return Some(unsafe { f.raw_copy() });
        }

        let mut ncm = w::NONCLIENTMETRICS::default();
        let sz = std::mem::size_of::<w::NONCLIENTMETRICS>() as u32;
        let got = unsafe {
            extra::SystemParametersInfoForDpi(
                extra::SPI_GETNONCLIENTMETRICS,
                sz,
                &mut ncm as *mut _ as *mut std::ffi::c_void,
                0,
                dpi,
            ) != 0
        };
        if !got {
            // Per-DPI call refused (it validates args); fall back to system-DPI
            // metrics and rescale by hand. Not a compat path — both imports are
            // Win10 1607 and the manifest already requires 1703 to start at all.
            unsafe {
                w::SystemParametersInfo(
                    co::SPI::GETNONCLIENTMETRICS,
                    sz,
                    &mut ncm,
                    co::SPIF::NoValue,
                )
                .ok()?;
            }
            let sys = system_dpi() as i32;
            ncm.lfMenuFont.lfHeight = w::MulDiv(ncm.lfMenuFont.lfHeight, dpi as i32, sys);
        }

        // `lfMenuFont` rather than `lfMessageFont`, matching the font winsafe
        // itself puts on every control — so this changes the *size*, and only
        // the size, of the type the shell already renders.
        let font = w::HFONT::CreateFontIndirect(&ncm.lfMenuFont).ok()?;
        let handle = unsafe { font.raw_copy() };
        cache.push((dpi, font));
        Some(handle)
    })
}

// Put the DPI-correct UI font on a window and every control under it. The
// tree view is included deliberately: winsafe sets no font on it at all, so
// it inherits the stock system font, which does not scale.
fn apply_ui_font(hwnd: &w::HWND, dpi: u32) {
    let Some(font) = ui_font(dpi) else { return };
    let set = |h: &w::HWND| {
        unsafe {
            h.SendMessage(msg::WmSetFont {
                hfont: font.raw_copy(),
                redraw: true,
            });
        };
    };
    set(hwnd);
    hwnd.EnumChildWindows(|child: w::HWND| {
        set(&child);
        true
    });
}

/// The Windows preferred UI language as a BCP-47 tag ("en-GB", "pt-BR",
/// "zh-Hans-CN"), or `None`.
///
/// A double-clicked `.exe` inherits no `LANG`, so the i18n crate's env-based
/// detection would wrongly fall back to English; this reads the real system
/// language for the "Auto" case. The raw tag is returned as-is —
/// `freemkv_i18n` normalizes and region-resolves it.
pub fn system_locale_code() -> Option<String> {
    // FMKV_SYSTEM_LOCALE=ar stands in for the Windows language, so a capture
    // can show "Auto" in any locale. Debug builds only (`dev_env`).
    if let Ok(tag) = dev_env("FMKV_SYSTEM_LOCALE") {
        return Some(tag);
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultLocaleName(name: *mut u16, size: i32) -> i32;
    }
    // LOCALE_NAME_MAX_LENGTH is 85.
    let mut buf = [0u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 1 {
        return None;
    }
    // The count includes the terminating NUL.
    Some(String::from_utf16_lossy(&buf[..(n as usize - 1)]))
}

// ── window icon ───────────────────────────────────────────────────────────

// Attach the embedded icon to the window (title bar/Alt-Tab/taskbar), not just the class icon.
// Best-effort.
fn set_icons(hwnd: &w::HWND) {
    let Ok(hinst) = w::HINSTANCE::GetModuleHandle(None) else {
        return;
    };
    let dpi = hwnd.GetDpiForWindow();

    // (which slot, which metric pair)
    let slots = [
        (co::ICON_SZ::SMALL, co::SM::CXSMICON, co::SM::CYSMICON),
        (co::ICON_SZ::BIG, co::SM::CXICON, co::SM::CYICON),
    ];

    for (slot, cx_metric, cy_metric) in slots {
        let cx = w::GetSystemMetricsForDpi(cx_metric, dpi)
            .unwrap_or_else(|_| w::GetSystemMetrics(cx_metric));
        let cy = w::GetSystemMetricsForDpi(cy_metric, dpi)
            .unwrap_or_else(|_| w::GetSystemMetrics(cy_metric));

        let loaded = hinst.LoadImageIcon(
            w::IdOicStr::Id(IDI_APP),
            w::SIZE::with(cx, cy),
            co::LR::DEFAULTCOLOR,
        );
        let Ok(mut icon) = loaded else { continue };

        // Leak deliberately: the icon must outlive this call for the window's
        // whole (one per process) lifetime, else the guard drops the HICON the
        // title bar points at. Not `LR::SHARED`, which is unreliable at this size.
        let hicon = icon.leak();
        unsafe {
            hwnd.SendMessage(msg::WmSetIcon { size: slot, hicon });
        }
    }
}

// ── tri-state checkboxes ──────────────────────────────────────────────────

// Fill the tree's STATE image list with unchecked/checked/mixed glyphs, theme-drawn and
// sized/rebuilt per DPI.
const CHECK_IMAGES: u32 = 7;

/// The website the About box shows and opens.
const SITE_URL: &str = "https://freemkv.org";

fn build_check_images<T: 'static>(tree: &gui::TreeView<T>, dpi: u32) -> w::AnyResult<()> {
    let side =
        w::GetSystemMetricsForDpi(co::SM::CXSMICON, dpi).unwrap_or(lay::Scale::new(dpi).px(16));

    // Already the right size for this DPI — nothing to do.
    if let Some(cur) = unsafe {
        tree.hwnd().SendMessage(msg::TvmGetImageList {
            kind: co::TVSIL::STATE,
        })
    } && cur.GetImageCount() >= CHECK_IMAGES
        && cur.GetIconSize().is_ok_and(|s| s.cx == side)
    {
        return Ok(());
    }

    let mut il = w::HIMAGELIST::Create(
        w::SIZE::with(side, side),
        co::ILC::COLOR32,
        CHECK_IMAGES as i32,
        0,
    )?;

    let desktop = w::HWND::GetDesktopWindow();
    let screen_dc = desktop.GetDC()?;
    let theme = tree.hwnd().OpenThemeData("BUTTON");
    let bg = w::HBRUSH::GetSysColorBrush(co::COLOR::WINDOW)?;
    let rc = w::RECT {
        left: 0,
        top: 0,
        right: side,
        bottom: side,
    };

    // Index 0 is "no state image" as far as the tree is concerned, so a
    // placeholder occupies it and the real glyphs land on 1-3, disabled on 4-6.
    let states: [_; CHECK_IMAGES as usize] = [
        (co::VS::BUTTON_CHECKBOX_UNCHECKEDNORMAL, 0u32),
        (co::VS::BUTTON_CHECKBOX_UNCHECKEDNORMAL, 0u32),
        (co::VS::BUTTON_CHECKBOX_CHECKEDNORMAL, extra::DFCS_CHECKED),
        (
            co::VS::BUTTON_CHECKBOX_MIXEDNORMAL,
            extra::DFCS_BUTTON3STATE | extra::DFCS_CHECKED,
        ),
        // 4, 5, 6: the same glyphs disabled (`state_for_row`, DISABLED_OFFSET).
        (
            co::VS::BUTTON_CHECKBOX_UNCHECKEDDISABLED,
            extra::DFCS_INACTIVE,
        ),
        (
            co::VS::BUTTON_CHECKBOX_CHECKEDDISABLED,
            extra::DFCS_CHECKED | extra::DFCS_INACTIVE,
        ),
        (
            co::VS::BUTTON_CHECKBOX_MIXEDDISABLED,
            extra::DFCS_BUTTON3STATE | extra::DFCS_CHECKED | extra::DFCS_INACTIVE,
        ),
    ];

    for (part_state, classic_state) in states {
        let mem_dc = screen_dc.CreateCompatibleDC()?;
        let bmp = screen_dc.CreateCompatibleBitmap(side, side)?;
        {
            let _sel = mem_dc.SelectObject(&*bmp)?;
            mem_dc.FillRect(rc, &bg)?;
            match &theme {
                Some(t) => t.DrawThemeBackground(&mem_dc, part_state, rc, None)?,
                None => {
                    // Classic mode: no theme data, so draw the 3D glyph.
                    let mut r = rc;
                    unsafe {
                        extra::DrawFrameControl(
                            mem_dc.ptr(),
                            &mut r as *mut _ as *mut _,
                            extra::DFC_BUTTON,
                            extra::DFCS_BUTTONCHECK | classic_state,
                        );
                    }
                }
            }
        }
        il.Add(&bmp, None)?;
    }

    // Hand the list to the tree and destroy whatever it held before, so a
    // window dragged back and forth between two monitors does not leak one
    // image list per crossing.
    let old = unsafe {
        tree.hwnd().SendMessage(msg::TvmSetImageList {
            kind: co::TVSIL::STATE,
            himagelist: Some(il.leak()),
        })
    };
    if let Some(old) = old {
        drop(unsafe { w::guard::ImageListDestroyGuard::new(old) });
    }
    Ok(())
}

// ── render memos ──────────────────────────────────────────────────────────

// Signatures of what was last painted, NOT a copy of `App` state — a "has
// this changed?" note so an idle tick does not rebuild the tree/dropdown.
#[derive(Default)]
struct Memo {
    rows: Option<u64>,
    formats: String,
    /// What the log pane shows; `None` until the first render fills it.
    log: Option<LogShown>,
    /// The `running` the menu enable states were last applied for.
    menu_running: Option<bool>,
    /// The selection bar as last filled in; the menus are built from it.
    pick: Option<crate::ui::PickView>,
    /// The free-space line last shown.
    free_line: Option<String>,
}

// One row signature: the core's, so every shell redraws on the same changes.
use crate::ui::rows_sig;

/// How the log pane must change to show `log`, given what it last showed.
#[derive(Debug, PartialEq, Eq)]
enum LogPlan {
    Keep,
    /// Append `log[n..]`; everything before it is already on screen.
    Append(usize),
    Rebuild,
}

/// What the log pane last showed: the sequence number of its first line
/// (`View::log_first`, which moves on every clear or front trim) and how many
/// lines it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogShown {
    first: u64,
    len: usize,
}

// `None` means the pane's content is unknown (fresh view): rebuild.
fn log_plan(shown: Option<LogShown>, first: u64, len: usize) -> LogPlan {
    match shown {
        Some(s) if s.first == first && s.len == len => LogPlan::Keep,
        Some(s) if s.first == first && s.len < len => LogPlan::Append(s.len),
        _ => LogPlan::Rebuild,
    }
}

// A row's cells in the core's column order (`ui::tree_columns`): what the tree
// paints into each column.
fn row_cells(r: &Row, columns: &[crate::ui::Column]) -> Vec<String> {
    columns.iter().map(|c| r.cell(c.id).to_string()).collect()
}

/// The title tree's columns. `SysTreeView32` has none, so a header sits above
/// it: the tick column (the tree's own expanders and tick boxes), then the
/// core's columns, whose cells are painted into each row (`NM_CUSTOMDRAW`).
struct TreeCols {
    /// The core's columns: ids, titles, alignment and the flexible one.
    columns: Vec<crate::ui::Column>,
    /// The user's widths (header drags), one per column, at the 96-DPI baseline.
    widths: Vec<i32>,
    /// Where the columns were last laid out, in the tree's client coordinates.
    laid: Option<lay::TreeColumns>,
    /// The tree's border width: where its client area starts under the header.
    inset: i32,
    /// Each row's cells, by `Row::index` — the data a tree item carries.
    cells: std::collections::HashMap<usize, Vec<String>>,
    /// Set while the shell itself resizes the header items, whose change
    /// notifications are then not a user's drag.
    syncing: bool,
}

impl TreeCols {
    fn new() -> Self {
        let columns = crate::ui::tree_columns();
        let widths = columns.iter().map(|c| c.width.round() as i32).collect();
        Self {
            columns,
            widths,
            laid: None,
            inset: 0,
            cells: std::collections::HashMap::new(),
            syncing: false,
        }
    }

    /// The flexible column's position.
    fn flex(&self) -> usize {
        self.columns.iter().position(|c| c.flex).unwrap_or(0)
    }
}

// ── the shell ─────────────────────────────────────────────────────────────

/// How many times the header texts were applied; the widget sweep reports it.
static TREE_HEAD_SYNCS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[derive(Clone)]
struct Shell {
    wnd: gui::WindowMain,
    /// The single source of truth. The shell holds no state of its own.
    app: Rc<RefCell<App>>,
    settings: Rc<RefCell<crate::settings::Settings>>,

    // empty page
    lbl_empty_head: gui::Label,
    lbl_empty_sub: gui::Label,
    /// "Open disc" — so the empty state offers the source its own headline is
    /// about, not only "Open file or ISO…".
    btn_open_disc: gui::Button,
    btn_open: gui::Button,

    // titles page
    tree: gui::TreeView<usize>,
    tree_head: gui::Header,
    /// The titles last applied to `tree_head`, so a render re-applies only a change.
    tree_head_text: Rc<RefCell<Vec<String>>>,
    cols: Rc<RefCell<TreeCols>>,
    /// The selection bar: Titles, Audio and Subtitles labels, then the choosers.
    lbl_pick: Vec<gui::Label>,
    cmb_pick_titles: gui::ComboBox,
    btn_pick_audio: gui::Button,
    btn_pick_subs: gui::Button,
    lbl_out: gui::Label,
    cmb_format: gui::ComboBox,
    edit_out: gui::Edit,
    btn_browse: gui::Button,
    btn_run: gui::Button,
    btn_eject: gui::Button,
    /// Free space at the output folder, under the output row.
    lbl_free: gui::Label,

    // progress page
    grp_prog: gui::Button,
    lbl_keys: Vec<gui::Label>,
    lbl_vals: Vec<gui::Label>,
    lbl_saving_cur: gui::Label,
    lbl_cur: gui::Label,
    bar_cur: gui::ProgressBar,
    lbl_saving_all: gui::Label,
    lbl_all: gui::Label,
    bar_all: gui::ProgressBar,
    btn_cancel: gui::Button,

    // result page
    lbl_result_head: gui::Label,
    lbl_result_line: gui::Label,
    btn_reveal: gui::Button,
    btn_done: gui::Button,

    // always present
    log: gui::Edit,

    /// Settings and About are built up-front and shown/hidden on demand:
    /// winsafe cannot create a window inside an event closure, so the macOS
    /// shell's build-on-first-open is not available here.
    prefs: Prefs,
    about: About,

    memo: Rc<RefCell<Memo>>,
    /// Recent rip-finished toasts, kept so each one's click handler outlives
    /// `perform` while it can still be clicked in the Action Center.
    toasts: Rc<RefCell<Vec<windows::UI::Notifications::ToastNotification>>>,
    /// Worker threads push user-visible lines here, with the log style they
    /// earn; a main-thread timer drains it. Win32 windows are owned by the
    /// thread that created them, so nothing else may touch a control.
    inbox: Arc<Mutex<Vec<(LogKind, String)>>>,
}

impl Shell {
    fn new() -> Self {
        let settings = crate::settings::Settings::load();
        let menus = win_menus();

        // No window exists yet, so only the system DPI is available. Sizes below
        // are placeholders (`relayout` fixes them on first `WM_SIZE`), but the
        // *window* size is used as-is, so scale it or it's a postage stamp on HiDPI.
        let s = lay::Scale::new(system_dpi());

        let wnd = gui::WindowMain::new(gui::WindowMainOpts {
            title: "freemkv",
            class_name: "FmkvMain",
            // Class icon is the taskbar/Alt-Tab fallback, but not enough alone:
            // winsafe fills hIcon/hIconSm from it via LoadIcon (always the 32px
            // frame), so the title bar would show a downscale; `set_icons` fixes it.
            class_icon: gui::Icon::Id(IDI_APP),
            size: lay::default_size(s.dpi()),
            style: co::WS::CAPTION
                | co::WS::SYSMENU
                | co::WS::CLIPCHILDREN
                | co::WS::BORDER
                | co::WS::VISIBLE
                | co::WS::MINIMIZEBOX
                | co::WS::MAXIMIZEBOX
                | co::WS::SIZEBOX,
            menu: build_menu(&menus).unwrap_or_else(|e| {
                tracing::error!("the menu bar could not be built: {e}");
                w::HMENU::NULL
            }),
            accel_table: build_accels(&menus).ok(),
            ..Default::default()
        });

        // ── empty page ──
        let lbl_empty_head = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &crate::strings::get("gui.page.empty_title"),
                size: (s.px(lay::W - lay::PAD * 2), s.px(26)),
                control_style: co::SS::CENTER,
                ..Default::default()
            },
        );
        let lbl_empty_sub = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &crate::strings::get("gui.page.empty_subtitle"),
                size: (s.px(lay::W - lay::PAD * 2), s.px(20)),
                control_style: co::SS::CENTER,
                ..Default::default()
            },
        );
        let btn_open_disc = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.open_disc"),
                width: s.px(180),
                height: s.px(30),
                ctrl_id: ID_OPEN_DISC_EMPTY,
                ..Default::default()
            },
        );
        let btn_open = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.open_file"),
                width: s.px(180),
                height: s.px(30),
                ctrl_id: ID_OPEN_EMPTY,
                ..Default::default()
            },
        );

        // ── titles page ──
        // The selection bar's labels; their texts are the core's.
        let lbl_pick: Vec<gui::Label> = crate::ui::pick_labels()
            .iter()
            .map(|t| {
                gui::Label::new(
                    &wnd,
                    gui::LabelOpts {
                        text: &format!("{t}:"),
                        size: (s.px(80), s.px(16)),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let cmb_pick_titles = gui::ComboBox::new(
            &wnd,
            gui::ComboBoxOpts {
                width: s.px(lay::PICK_TITLES_W),
                ctrl_id: ID_PICK_TITLES,
                ..Default::default()
            },
        );
        // Audio and Subtitles are multi-select: a split button showing the choice,
        // with a checkable menu behind it (Win32 has no checked-list combo box).
        let mk_pick_btn = |id: u16| {
            gui::Button::new(
                &wnd,
                gui::ButtonOpts {
                    text: "",
                    width: s.px(lay::PICK_MENU_W),
                    height: s.px(24),
                    // BS_SPLITBUTTON, which winsafe does not name: the native drop arrow.
                    control_style: unsafe { co::BS::from_raw(0x0000_000c) } | co::BS::LEFT,
                    ctrl_id: id,
                    ..Default::default()
                },
            )
        };
        let btn_pick_audio = mk_pick_btn(ID_PICK_AUDIO);
        let btn_pick_subs = mk_pick_btn(ID_PICK_SUBS);
        let tree = gui::TreeView::new(
            &wnd,
            gui::TreeViewOpts {
                size: (s.px(400), s.px(400)),
                // No TVS::CHECKBOXES: that list is two-state only; `build_check_images`
                // carries the third (mixed) glyph. NOHSCROLL: a long label is cut at the
                // Length column painted over it, rather than scrolling the columns away.
                control_style: co::TVS::HASLINES
                    | co::TVS::LINESATROOT
                    | co::TVS::HASBUTTONS
                    | co::TVS::SHOWSELALWAYS
                    | co::TVS::FULLROWSELECT
                    | co::TVS::NOHSCROLL,
                ctrl_id: ID_TREE,
                ..Default::default()
            },
        );
        // Texts are set once the window exists (`sync_tree_head`); one item for
        // the tick column, then one per core column.
        let head_items: Vec<(&str, i32)> = std::iter::once(("", s.px(lay::COL_TICK_W)))
            .chain(
                crate::ui::tree_columns()
                    .iter()
                    .map(|c| ("", s.px(c.width.round() as i32))),
            )
            .collect();
        let tree_head = gui::Header::new(
            &wnd,
            gui::HeaderOpts {
                width: s.px(400),
                height: s.px(lay::TREE_HEAD_H),
                control_style: co::HDS::HORZ | co::HDS::FULLDRAG,
                window_style: co::WS::CHILD | co::WS::VISIBLE,
                ctrl_id: ID_TREE_HEAD,
                items: &head_items,
                ..Default::default()
            },
        );
        let lbl_out = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &crate::strings::get("gui.group.output"),
                size: (s.px(300), s.px(16)),
                ..Default::default()
            },
        );
        let cmb_format = gui::ComboBox::new(
            &wnd,
            gui::ComboBoxOpts {
                width: s.px(lay::FORMAT_W),
                ctrl_id: ID_FORMAT,
                ..Default::default()
            },
        );
        let edit_out = gui::Edit::new(
            &wnd,
            gui::EditOpts {
                text: &settings.dest_dir,
                width: s.px(240),
                height: s.px(23),
                ctrl_id: ID_OUTDIR,
                ..Default::default()
            },
        );
        let btn_browse = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.browse"),
                width: s.px(34),
                height: s.px(25),
                ctrl_id: ID_BROWSE,
                ..Default::default()
            },
        );
        let btn_run = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.run_now"),
                width: s.px(lay::RUN_W),
                height: s.px(28),
                control_style: co::BS::DEFPUSHBUTTON,
                ctrl_id: ID_RUN,
                ..Default::default()
            },
        );
        let btn_eject = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.menu.eject"),
                width: s.px(110),
                height: s.px(26),
                ctrl_id: ID_EJECT,
                ..Default::default()
            },
        );
        let lbl_free = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: "",
                size: (s.px(300), s.px(16)),
                control_style: co::SS::LEFT | co::SS::ENDELLIPSIS,
                ..Default::default()
            },
        );

        // ── progress page ──
        let grp_prog = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.group.information"),
                width: s.px(lay::W - lay::PAD * 2),
                height: s.px(132),
                control_style: co::BS::GROUPBOX,
                window_style: co::WS::CHILD | co::WS::VISIBLE,
                ..Default::default()
            },
        );
        // Labels come from the core, never from the shell — otherwise a renamed
        // row here would silently disagree with the macOS shell.
        let labels = crate::ui::InfoRows::labels();
        let mut lbl_keys = Vec::with_capacity(7);
        let mut lbl_vals = Vec::with_capacity(7);
        for k in labels.iter() {
            lbl_keys.push(gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: k,
                    size: (s.px(110), s.px(16)),
                    control_style: co::SS::RIGHT,
                    ..Default::default()
                },
            ));
            lbl_vals.push(gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: "",
                    size: (s.px(lay::W - 200), s.px(16)),
                    control_style: co::SS::LEFT | co::SS::ENDELLIPSIS,
                    ..Default::default()
                },
            ));
        }
        let mk_label = |text: &str, wd: i32, style: co::SS| {
            gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text,
                    size: (wd, s.px(16)),
                    control_style: style,
                    ..Default::default()
                },
            )
        };
        let lbl_saving_cur = mk_label("", s.px(320), co::SS::LEFT);
        let lbl_cur = mk_label("", s.px(330), co::SS::RIGHT);
        let lbl_saving_all = mk_label("", s.px(320), co::SS::LEFT);
        let lbl_all = mk_label("", s.px(330), co::SS::RIGHT);
        let mk_bar = |id: u16| {
            gui::ProgressBar::new(
                &wnd,
                gui::ProgressBarOpts {
                    size: (s.px(lay::W - lay::PAD * 2), s.px(20)),
                    range: (0, 100),
                    ctrl_id: id,
                    ..Default::default()
                },
            )
        };
        let bar_cur = mk_bar(ID_BAR_CUR);
        let bar_all = mk_bar(ID_BAR_ALL);
        let btn_cancel = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.cancel"),
                width: s.px(110),
                height: s.px(30),
                ctrl_id: ID_CANCEL,
                ..Default::default()
            },
        );

        // ── result page ──
        let lbl_result_head = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &crate::strings::get("gui.result.finished"),
                size: (s.px(lay::W - lay::PAD * 2), s.px(26)),
                control_style: co::SS::CENTER,
                ..Default::default()
            },
        );
        let lbl_result_line = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: "",
                size: (s.px(lay::W - lay::PAD * 2), s.px(20)),
                control_style: co::SS::CENTER | co::SS::ENDELLIPSIS,
                ..Default::default()
            },
        );
        let btn_reveal = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.show_explorer"),
                width: s.px(170),
                height: s.px(32),
                ctrl_id: ID_REVEAL,
                ..Default::default()
            },
        );
        let btn_done = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.done"),
                width: s.px(170),
                height: s.px(32),
                control_style: co::BS::DEFPUSHBUTTON,
                ctrl_id: ID_DONE,
                ..Default::default()
            },
        );

        // ── the log ──
        let log = gui::Edit::new(
            &wnd,
            gui::EditOpts {
                text: "",
                width: s.px(lay::W - lay::PAD * 2),
                height: s.px(200),
                // Read-only but SELECTABLE: the log is the thing users paste
                // into bug reports, so copying out of it has to work.
                control_style: co::ES::MULTILINE | co::ES::READONLY | co::ES::AUTOVSCROLL,
                window_style: co::WS::CHILD | co::WS::VISIBLE | co::WS::TABSTOP | co::WS::VSCROLL,
                ctrl_id: ID_LOG,
                ..Default::default()
            },
        );

        let prefs = Prefs::new(&wnd, &settings);
        let about = About::new(&wnd, &settings);

        Self {
            wnd,
            app: Rc::new(RefCell::new(App::new())),
            settings: Rc::new(RefCell::new(settings)),
            lbl_empty_head,
            lbl_empty_sub,
            btn_open_disc,
            btn_open,
            tree,
            tree_head,
            tree_head_text: Rc::new(RefCell::new(Vec::new())),
            cols: Rc::new(RefCell::new(TreeCols::new())),
            lbl_pick,
            cmb_pick_titles,
            btn_pick_audio,
            btn_pick_subs,
            lbl_out,
            cmb_format,
            edit_out,
            btn_browse,
            btn_run,
            btn_eject,
            lbl_free,
            grp_prog,
            lbl_keys,
            lbl_vals,
            lbl_saving_cur,
            lbl_cur,
            bar_cur,
            lbl_saving_all,
            lbl_all,
            bar_all,
            btn_cancel,
            lbl_result_head,
            lbl_result_line,
            btn_reveal,
            btn_done,
            log,
            prefs,
            about,
            memo: Rc::new(RefCell::new(Memo::default())),
            toasts: Rc::new(RefCell::new(Vec::new())),
            inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

// ── menus ─────────────────────────────────────────────────────────────────

// The View > log item's full text: core state-dependent label plus this
// shell's accelerator hint, so build and re-title can't diverge.
fn log_menu_text(label: &str) -> String {
    use crate::ui::{Cmd, MenuAction, MenuEntry};
    let accel = crate::ui::menu_layout(false)
        .iter()
        .flat_map(|g| g.entries.iter())
        .find_map(|e| match e {
            MenuEntry::Item(mi) if mi.action == MenuAction::Cmd(Cmd::ToggleLog) => Some(mi.accel),
            _ => None,
        })
        .flatten();
    crate::win_menu::item_text(label, accel.as_ref())
}

/// Map a shared [`crate::ui::MenuAction`] to the Windows IDM constant whose
/// handler already fires the right [`Cmd`]. `None` means "this action isn't
/// on the Windows menu bar" — Cut/Paste, and any [`MenuAction::Cmd`] variant
/// the menu bar deliberately never surfaces (SetFormat, Cancel).
fn idm_for_action(action: &crate::ui::MenuAction) -> Option<u16> {
    use crate::ui::{Cmd, MenuAction};
    Some(match action {
        MenuAction::Cmd(Cmd::Open) => IDM_OPEN,
        MenuAction::OpenDisc => IDM_OPEN_DISC,
        MenuAction::Cmd(Cmd::Close) => IDM_CLOSE,
        MenuAction::Cmd(Cmd::SetOutput) => IDM_SET_OUTPUT,
        MenuAction::Cmd(Cmd::Run) => IDM_START_RIP,
        MenuAction::Cmd(Cmd::Eject) => IDM_EJECT,
        MenuAction::Cmd(Cmd::Settings) => IDM_SETTINGS,
        MenuAction::Cmd(Cmd::Quit) => IDM_EXIT,
        MenuAction::StandardCopy => IDM_COPY,
        MenuAction::StandardSelectAllText => IDM_SELECT_ALL_TEXT,
        MenuAction::Cmd(Cmd::SelectAll) => IDM_SELECT_ALL,
        MenuAction::Cmd(Cmd::SelectNone) => IDM_SELECT_NONE,
        MenuAction::Cmd(Cmd::Invert) => IDM_INVERT,
        MenuAction::Cmd(Cmd::ToggleLog) => IDM_TOGGLE_LOG,
        MenuAction::Cmd(Cmd::ClearLog) => IDM_CLEAR_LOG,
        MenuAction::Cmd(Cmd::Docs) => IDM_DOCS,
        MenuAction::Cmd(Cmd::CheckUpdates) => IDM_CHECK_UPDATES,
        MenuAction::Cmd(Cmd::About) => IDM_ABOUT,
        MenuAction::StandardCut | MenuAction::StandardPaste => return None,
        MenuAction::Cmd(_) => return None,
    })
}

/// The Windows menu plan for the current locale. Built once per call and fed
/// to both [`build_menu`] and [`build_accels`], so hint and key can't drift.
fn win_menus() -> Vec<crate::win_menu::WinMenu> {
    crate::win_menu::win_menus(
        &crate::ui::menu_layout(false),
        &crate::strings::get("gui.menu.exit"),
    )
}

/// The menu bar from [`crate::win_menu::win_menus`]: adding a menu item is one
/// edit in `ui.rs`. Items with no Windows command id are left off.
fn build_menu(menus: &[crate::win_menu::WinMenu]) -> w::SysResult<w::HMENU> {
    use crate::win_menu::WinEntry;
    let bar = w::HMENU::CreateMenu()?;
    for m in menus {
        let popup = w::HMENU::CreatePopupMenu()?;
        for e in &m.entries {
            match e {
                WinEntry::Separator => popup.append_item(&[w::MenuItem::Separator])?,
                WinEntry::Item { action, text, .. } => {
                    if let Some(cmd_id) = idm_for_action(action) {
                        popup.append_item(&[w::MenuItem::Entry { cmd_id, text }])?;
                    }
                }
            }
        }
        bar.append_item(&[w::MenuItem::Submenu {
            submenu: &popup,
            text: &m.title,
        }])?;
    }
    Ok(bar)
}

/// The accelerator table, derived from the same plan as the menu so every
/// shortcut it shows works. Alt+F4 is handled by the system.
fn build_accels(
    menus: &[crate::win_menu::WinMenu],
) -> w::SysResult<w::guard::DestroyAcceleratorTableGuard> {
    let accels: Vec<w::ACCEL> = crate::win_menu::accel_table(menus)
        .into_iter()
        .filter_map(|(action, k)| {
            let mut f = co::ACCELF::VIRTKEY;
            for (on, flag) in [
                (k.ctrl, co::ACCELF::CONTROL),
                (k.shift, co::ACCELF::SHIFT),
                (k.alt, co::ACCELF::ALT),
            ] {
                if on {
                    f |= flag;
                }
            }
            Some(w::ACCEL {
                fVirt: f,
                // SAFETY: a plain virtual-key value; no pointer or handle involved.
                key: unsafe { co::VK::from_raw(k.vk) },
                cmd: idm_for_action(&action)?,
            })
        })
        .collect();
    w::HACCEL::CreateAcceleratorTable(&accels)
}

// ── layout ────────────────────────────────────────────────────────────────

/// Move one control. Swallows the error: a failed reposition must never abort a
/// resize, and there is nothing useful to tell the user about it.
fn place(c: &impl GuiWindow, x: i32, y: i32, cx: i32, cy: i32) {
    let _ = c.hwnd().SetWindowPos(
        w::HwndPlace::None,
        w::POINT::with(x, y),
        w::SIZE::with(cx.max(0), cy.max(0)),
        co::SWP::NOZORDER | co::SWP::NOACTIVATE,
    );
}

/// Move one control to a rectangle computed by `win_layout`.
fn put(c: &impl GuiWindow, r: lay::Rect) {
    place(c, r.x, r.y, r.w, r.h);
}

fn show(c: &impl GuiWindow, visible: bool) {
    c.hwnd().ShowWindow(if visible {
        co::SW::SHOWNA
    } else {
        co::SW::HIDE
    });
}

impl Shell {
    // Reposition everything for the current client size. `win_layout::main_layout`
    // is the single source of geometry truth, using the window's monitor DPI;
    // `cw`/`ch` and the returned rects are physical pixels, as `WM_SIZE` reports.
    fn relayout(&self, cw: i32, ch: i32) {
        let v = self.app.borrow().view();
        let hidden = v.log_hidden;
        let dpi = window_dpi(self.wnd.hwnd());
        let label_w = |i: usize| {
            self.lbl_pick.get(i).map_or(0, |l| {
                let text = l.hwnd().GetWindowText().unwrap_or_default();
                text_width(l.hwnd(), &text)
            })
        };
        let l = lay::main_layout(
            dpi,
            cw,
            ch,
            lay::MainState {
                page: v.page,
                two_bars: v.show_overall_bar,
                log_hidden: hidden,
                info_rows: self.lbl_keys.len(),
                pick_labels: [label_w(0), label_w(1), label_w(2)],
            },
        );

        put(&self.log, l.log);
        show(&self.log, !hidden);

        // ── empty page ──
        put(&self.lbl_empty_head, l.empty_head);
        put(&self.lbl_empty_sub, l.empty_sub);
        put(&self.btn_open_disc, l.btn_open_disc);
        put(&self.btn_open, l.btn_open);

        // ── titles page ──
        for (lbl, r) in
            self.lbl_pick
                .iter()
                .zip([l.pick_titles_lbl, l.pick_audio_lbl, l.pick_subs_lbl])
        {
            put(lbl, r);
        }
        put(&self.cmb_pick_titles, l.pick_titles);
        put(&self.btn_pick_audio, l.pick_audio);
        put(&self.btn_pick_subs, l.pick_subs);
        put(&self.btn_eject, l.btn_eject);
        let (head, tree) = lay::split_tree_header(l.tree, dpi);
        put(&self.tree_head, head);
        put(&self.tree, tree);
        self.layout_tree_cols(dpi);
        put(&self.lbl_out, l.lbl_out);
        put(&self.edit_out, l.edit_out);
        put(&self.btn_browse, l.btn_browse);
        put(&self.cmb_format, l.cmb_format);
        put(&self.btn_run, l.btn_run);
        put(&self.lbl_free, l.lbl_free);

        // ── progress page ──
        put(&self.grp_prog, l.grp_prog);
        for (i, (key, val)) in l.info_rows.iter().enumerate() {
            put(&self.lbl_keys[i], *key);
            put(&self.lbl_vals[i], *val);
        }
        put(&self.lbl_saving_cur, l.lbl_saving_cur);
        put(&self.lbl_cur, l.lbl_cur);
        put(&self.bar_cur, l.bar_cur);
        put(&self.lbl_saving_all, l.lbl_saving_all);
        put(&self.lbl_all, l.lbl_all);
        put(&self.bar_all, l.bar_all);
        put(&self.btn_cancel, l.btn_cancel);

        // ── result page ──
        put(&self.lbl_result_head, l.result_head);
        put(&self.lbl_result_line, l.result_line);
        put(&self.btn_reveal, l.btn_reveal);
        put(&self.btn_done, l.btn_done);
    }

    fn relayout_now(&self) {
        if let Ok(rc) = self.wnd.hwnd().GetClientRect() {
            self.relayout(rc.right, rc.bottom);
        }
    }

    /// Re-derive everything that is a function of the DPI, at the DPI the
    /// window is on right now.
    fn apply_dpi(&self) {
        self.apply_dpi_at(window_dpi(self.wnd.hwnd()));
    }

    /// The things besides the rectangles that scale: the type, the tri-state
    /// tick glyphs in the tree's state image list, and the tree's row height and
    /// indent. Called from `WM_CREATE` and again from every `WM_DPICHANGED`.
    fn apply_dpi_at(&self, dpi: u32) {
        apply_ui_font(self.wnd.hwnd(), dpi);
        let _ = build_check_images(&self.tree, dpi);
        // After the font: a new font puts the tree back on its own row height.
        let s = lay::Scale::new(dpi);
        unsafe {
            self.tree.hwnd().SendMessage(msg::TvmSetItemHeight {
                height: Some(s.px(lay::TREE_ROW_H) as u32),
            });
            self.tree.hwnd().SendMessage(msg::TvmSetIndent {
                width: s.px(lay::TREE_INDENT) as u32,
            });
        }
    }
}

// ── the tree ──────────────────────────────────────────────────────────────

impl Shell {
    /// Set one row's state-image index, which is how a tri-state tick is drawn.
    fn set_row_state(&self, hitem: &w::HTREEITEM, idx: u32) {
        let mut tvix = w::TVITEMEX::default();
        tvix.hItem = unsafe { hitem.raw_copy() };
        tvix.mask = co::TVIF::STATE;
        tvix.stateMask = co::TVIS::STATEIMAGEMASK;
        // Win32's INDEXTOSTATEIMAGEMASK: the image index lives in bits 12..15.
        tvix.state = unsafe { co::TVIS::from_raw(idx << 12) };
        let _ = unsafe {
            self.tree
                .hwnd()
                .SendMessage(msg::TvmSetItem { tvitem: &tvix })
        };
    }

    fn set_tree_redraw(&self, can_redraw: bool) {
        unsafe {
            self.tree
                .hwnd()
                .SendMessage(msg::WmSetRedraw { can_redraw })
        };
    }

    /// Rebuild the tree from the core's rows. Only called when the row set
    /// actually changed — see `Memo`.
    fn rebuild_tree(&self, rows: &[Row]) {
        {
            let mut cols = self.cols.borrow_mut();
            cols.cells = rows
                .iter()
                .map(|r| (r.index, row_cells(r, &cols.columns)))
                .collect();
        }
        // TreeView has no `set_redraw` wrapper (only ListView does), so the
        // message goes direct. Without it, rebuilding a large tree flickers.
        self.set_tree_redraw(false);
        let _ = self.tree.items().delete_all();

        // Parentage comes from the core (`ui::row_parents`), so this shell and
        // the macOS outline can't nest rows differently. Handles are kept per
        // ROW POSITION so a child attaches to its own parent, not the last one.
        let parents = crate::ui::row_parents(rows);
        let mut handles: Vec<Option<w::HTREEITEM>> = Vec::with_capacity(rows.len());
        for (i, r) in rows.iter().enumerate() {
            // The item's own text is the Item cell, for a screen reader; what shows
            // is painted over it, column by column (`paint_tree_cells`).
            let text = &r.item;
            // A row whose parent is missing is added at the top level rather
            // than dropped: a row the core decided to show must be reachable.
            let parent = parents[i].and_then(|p| handles[p].as_ref());
            let added = match parent {
                None => self
                    .tree
                    .items()
                    .add_root(text, None, r.index)
                    .ok()
                    .map(|it| unsafe { it.htreeitem().raw_copy() }),
                Some(p) => self
                    .tree
                    .items()
                    .get(p)
                    .add_child(text, None, r.index)
                    .ok()
                    .map(|it| unsafe { it.htreeitem().raw_copy() }),
            };
            if let Some(h) = &added {
                self.set_row_state(h, state_for_row(r));
            }
            handles.push(added);
        }
        // The macOS outline opens everything on load; match it so both shells
        // show the same thing without a click.
        for root in self.tree.items().iter_root() {
            let _ = root.expand(true);
            for child in root.iter_children() {
                let _ = child.expand(true);
            }
        }
        self.set_tree_redraw(true);
        // Expanding above leaves the view parked on the LAST title: start at the
        // top, then scroll only as far as the core's chosen row (first ticked) needs.
        if let Some(h) = crate::ui::first_visible_row(rows).and_then(|i| handles[i].as_ref()) {
            if let Some(top) = handles.iter().flatten().next() {
                let _ = unsafe {
                    self.tree.hwnd().SendMessage(msg::TvmSelectItem {
                        action: co::TVGN::FIRSTVISIBLE,
                        hitem: top,
                    })
                };
            }
            let _ = unsafe {
                self.tree
                    .hwnd()
                    .SendMessage(msg::TvmEnsureVisible { hitem: h })
            };
        }
        let _ = self.tree.hwnd().InvalidateRect(None, true);
    }

    /// Refresh only the tick states, leaving the rows (and the user's expansion
    /// and selection) untouched. This is what runs on an ordinary redraw.
    fn sync_tree_states(&self, rows: &[Row]) {
        let mut by_index = std::collections::HashMap::with_capacity(rows.len());
        for r in rows {
            by_index.entry(r.index).or_insert(state_for_row(r));
        }
        for root in self.tree.items().iter_root() {
            let apply = |it: &w::gui::TreeViewItem<'_, usize>| {
                let idx = *it.data().borrow();
                if let Some(&state) = by_index.get(&idx) {
                    self.set_row_state(it.htreeitem(), state);
                }
            };
            apply(&root);
            for title in root.iter_children() {
                apply(&title);
                for stream in title.iter_children() {
                    apply(&stream);
                }
            }
        }
    }

    // Which row the user just clicked the tick box of, if any. `nm_click` carries
    // no hit info, so it's resolved by hand; `msg::TvmHitTest` in winsafe 0.0.28
    // passes a pointer-to-a-pointer (a crate bug), so the raw message is sent.
    fn hit_state_icon(&self) -> Option<usize> {
        let pt = self
            .tree
            .hwnd()
            .ScreenToClient(w::GetCursorPos().ok()?)
            .ok()?;
        let mut hti = w::TVHITTESTINFO {
            pt,
            flags: unsafe { co::TVHT::from_raw(0) },
            hitem: w::HTREEITEM::NULL,
        };
        let ret = unsafe {
            self.tree.hwnd().SendMessage(msg::Wm {
                msg_id: co::TVM::HITTEST.into(),
                wparam: 0,
                lparam: &mut hti as *mut _ as isize,
            })
        };
        if ret == 0 || !hti.flags.has(co::TVHT::ONITEMSTATEICON) {
            return None;
        }
        Some(*self.tree.items().get(&hti.hitem).data().borrow())
    }
}

// ── the tree's columns ────────────────────────────────────────────────────

impl Shell {
    /// Lay the columns out for the tree's current client width, sizing the
    /// header items to match. A no-op when nothing moved, since `render` re-runs
    /// the layout on every tick.
    fn layout_tree_cols(&self, dpi: u32) {
        let Ok(client) = self.tree.hwnd().GetClientRect() else {
            return;
        };
        let mut info = w::WINDOWINFO::default();
        let inset = match self.tree.hwnd().GetWindowInfo(&mut info) {
            Ok(()) => info.cxWindowBorders as i32,
            Err(_) => 0,
        };
        let laid = {
            let c = self.cols.borrow();
            lay::tree_columns(dpi, client.right, &c.widths, c.flex())
        };
        if self.cols.borrow().laid.as_ref() == Some(&laid) {
            return;
        }
        let head_w = self
            .tree_head
            .hwnd()
            .GetClientRect()
            .map(|r| r.right)
            .unwrap_or(0);
        let widths = lay::header_widths(&laid, head_w, inset);
        {
            let mut c = self.cols.borrow_mut();
            c.laid = Some(laid);
            c.inset = inset;
            c.syncing = true;
        }
        for (i, wd) in widths.into_iter().enumerate() {
            self.tree_head.items().get(i as u32).set_width(wd);
        }
        self.cols.borrow_mut().syncing = false;
        let _ = self.tree.hwnd().InvalidateRect(None, true);
    }

    /// The header's texts, in the current language: nothing over the tick
    /// column, then the core's column titles, numeric ones right-aligned like
    /// their cells.
    fn sync_tree_head(&self) {
        let columns = crate::ui::tree_columns();
        let texts: Vec<String> = std::iter::once(String::new())
            .chain(columns.iter().map(|c| c.title.clone()))
            .collect();
        if *self.tree_head_text.borrow() == texts {
            return;
        }
        TREE_HEAD_SYNCS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let set = |i: u32, text: &str, hdf: co::HDF| {
            // `HDF::STRING` must ride along: an item made empty has no string
            // format, and comctl32 then ignores the text it is given.
            let mut hdi = w::HDITEM::default();
            hdi.mask = co::HDI::TEXT | co::HDI::FORMAT;
            hdi.fmt = co::HDF::STRING | hdf;
            let mut wtext = w::WString::from_str(text);
            hdi.set_pszText(Some(&mut wtext));
            unsafe {
                self.tree_head.hwnd().SendMessage(msg::HdmSetItem {
                    index: i,
                    hditem: &hdi,
                });
            }
        };
        set(0, &texts[0], co::HDF::LEFT);
        for (i, c) in columns.iter().enumerate() {
            let align = if c.numeric {
                co::HDF::RIGHT
            } else {
                co::HDF::LEFT
            };
            set(i as u32 + 1, &c.title, align);
        }
        self.cols.borrow_mut().columns = columns;
        *self.tree_head_text.borrow_mut() = texts;
    }

    /// Mirror the title tree and its header under a right-to-left interface
    /// language: `WS_EX_LAYOUTRTL` flips each window's coordinates, so the
    /// columns, the expanders and the tick boxes all read from the right.
    fn apply_tree_direction(&self) {
        let lang = self.settings.borrow().language.clone();
        let rtl = crate::app_entry::resolved_locale(&lang, system_locale_code)
            .is_some_and(|t| lay::is_rtl_locale(&t));
        let flag = co::WS_EX::LAYOUTRTL.raw() as isize;
        for h in [self.tree.hwnd(), self.tree_head.hwnd()] {
            let ex = h.GetWindowLongPtr(co::GWLP::EXSTYLE);
            let want = if rtl { ex | flag } else { ex & !flag };
            if want != ex {
                unsafe { h.SetWindowLongPtr(co::GWLP::EXSTYLE, want) };
                let _ = h.InvalidateRect(None, true);
            }
        }
    }

    /// Paint one row's cells, one per column, over the label the tree drew. The
    /// tree has drawn its expander and tick box left of the label, which stay;
    /// with a mirrored tree the DC is mirrored too, so the same client
    /// coordinates land mirrored.
    fn paint_tree_cells(&self, cd: &w::NMCUSTOMDRAW) {
        let cols = self.cols.borrow();
        let Some(laid) = cols.laid.as_ref() else {
            return;
        };
        // An item mid-insertion has no data yet, and `data()` would panic.
        if cd.lItemlParam == 0 {
            return;
        }
        let hitem = unsafe { w::HTREEITEM::from_ptr(cd.dwItemSpec as _) };
        let idx = *self.tree.items().get(&hitem).data().borrow();
        let Some(label) = self.item_rect(&hitem, true) else {
            return;
        };
        let hdc = &cd.hdc;
        let (top, bottom) = (cd.rc.top, cd.rc.bottom);
        let right = laid.cols.last().map_or(cd.rc.right, |c| c.x + c.w);
        let area = w::RECT {
            left: label.left,
            top,
            right: right.max(cd.rc.right),
            bottom,
        };
        // The row's own selection highlight, carried across every column.
        let selected = cd.uItemState.has(co::CDIS::SELECTED);
        let focused = w::HWND::GetFocus().is_some_and(|f| f == *self.tree.hwnd());
        let (bg, fg) = match (selected, focused) {
            (true, true) => (co::COLOR::HIGHLIGHT, co::COLOR::HIGHLIGHTTEXT),
            (true, false) => (co::COLOR::BTNFACE, co::COLOR::WINDOWTEXT),
            _ => (co::COLOR::WINDOW, co::COLOR::WINDOWTEXT),
        };
        if let Ok(brush) = w::HBRUSH::GetSysColorBrush(bg) {
            let _ = hdc.FillRect(area, &brush);
        }
        let Some(cells) = cols.cells.get(&idx) else {
            return;
        };
        // The tree's own font: the DC is not guaranteed to still hold it after
        // the item has painted.
        let font = unsafe { self.tree.hwnd().SendMessage(msg::WmGetFont {}) };
        let _font = font.as_ref().and_then(|f| hdc.SelectObject(f).ok());
        let _ = hdc.SetBkMode(co::BKMODE::TRANSPARENT);
        let _ = hdc.SetTextColor(w::GetSysColor(fg));
        let pad = lay::Scale::new(window_dpi(self.wnd.hwnd())).px(6);
        for ((span, text), col) in laid.cols.iter().zip(cells).zip(&cols.columns) {
            if text.is_empty() {
                continue;
            }
            // A row indented past its column's start begins after its tick box.
            let mut rc = w::RECT {
                left: span.x.max(label.left) + pad,
                top,
                right: span.x + span.w - pad,
                bottom,
            };
            let align = if col.numeric {
                co::DT::RIGHT
            } else {
                co::DT::LEFT
            };
            let _ = hdc.DrawText(
                text,
                &mut rc,
                align
                    | co::DT::VCENTER
                    | co::DT::SINGLELINE
                    | co::DT::NOPREFIX
                    | co::DT::END_ELLIPSIS,
            );
        }
    }

    /// A tree item's rectangle in the tree's client coordinates; `text_only`
    /// gives its label alone, without the indent, expander and tick box.
    fn item_rect(&self, h: &w::HTREEITEM, text_only: bool) -> Option<w::RECT> {
        let mut rc = w::RECT::default();
        // TVM_GETITEMRECT takes the item in the rectangle it fills.
        unsafe {
            (&mut rc as *mut w::RECT)
                .cast::<isize>()
                .write_unaligned(h.ptr() as isize)
        };
        unsafe {
            self.tree.hwnd().SendMessage(msg::TvmGetItemRect {
                text_only,
                rect: &mut rc,
            })
        }
        .ok()?;
        Some(rc)
    }
}

// ── render ────────────────────────────────────────────────────────────────

impl Shell {
    // Mutate the core model and REPAINT — the choke-point so no handler can
    // mutate and forget to redraw. Mutable borrow drops before `render`'s own.
    fn app_mut<R>(&self, f: impl FnOnce(&mut App) -> R) -> R {
        let r = f(&mut self.app.borrow_mut());
        self.render();
        r
    }

    // Ask before quitting mid-rip; `true` means go ahead. Shared by the window's X and File >
    // Exit, which used to disagree  — one question, one place, not a copy beside each call
    // site.
    fn confirm_quit_mid_rip(&self) -> bool {
        if !self.app.borrow().running() {
            return true;
        }
        let answer = self.wnd.hwnd().MessageBox(
            &format!(
                "{}\n\n{}",
                crate::strings::get("gui.alert.rip_title"),
                crate::strings::get("gui.alert.rip_body")
            ),
            "freemkv",
            co::MB::YESNO | co::MB::ICONWARNING,
        );
        answer.map(|a| a == co::DLGID::YES).unwrap_or(false)
    }

    // Signal the worker to stop, then WAIT (bounded by `QUIT_GRACE`) for it to put its output
    // down, so the partial file is closed/finalised rather than left mid-write.
    fn cancel_and_drain(&self) {
        self.act(Cmd::Cancel);
        let run = self.app.borrow().run.clone();
        if let Some(run) = run {
            crate::engine::await_worker_exit(&run, crate::engine::QUIT_GRACE);
        }
    }

    /// The shell's entire job: hand the command to the core, perform the
    /// platform effects it asks for, redraw. No decisions here.
    fn act(&self, cmd: Cmd) {
        let effects = self.app_mut(|a| a.dispatch(cmd));
        self.perform(effects);
    }

    /// Apply a fully-decided `View` to the widgets. The ONLY place the shell
    /// writes to controls, and it computes nothing.
    fn render(&self) {
        let v = self.app.borrow().view();

        // Format list depends on source kind, so it's re-derived every render,
        // not just at build time — a real macOS bug had "Whole disc → ISO image"
        // still on offer after opening an MKV, which can't produce it.
        self.sync_formats(&v);

        // ── pages ──
        let p = v.page;
        for c in [&self.lbl_empty_head, &self.lbl_empty_sub] {
            show(c, p == Page::Empty);
        }
        for c in [&self.btn_open_disc, &self.btn_open] {
            show(c, p == Page::Empty);
        }

        show(&self.tree, p == Page::Titles);
        show(&self.tree_head, p == Page::Titles);
        if p == Page::Titles {
            self.sync_tree_head();
        }
        // The selection bar hides with no source open.
        let picking = p == Page::Titles && v.pick.is_some();
        for c in &self.lbl_pick {
            show(c, picking);
        }
        show(&self.cmb_pick_titles, picking);
        for c in [&self.btn_pick_audio, &self.btn_pick_subs] {
            show(c, picking);
        }
        for c in [&self.lbl_out, &self.lbl_free] {
            show(c, p == Page::Titles);
        }
        show(&self.cmb_format, p == Page::Titles);
        show(&self.edit_out, p == Page::Titles);
        for c in [&self.btn_browse, &self.btn_run] {
            show(c, p == Page::Titles);
        }
        // Eject is meaningless for an image file, so the button hides — a
        // control that lies is worse than no control.
        show(&self.btn_eject, p == Page::Titles && v.eject_visible);

        let on_prog = p == Page::Progress;
        show(&self.grp_prog, on_prog);
        for c in self.lbl_keys.iter().chain(self.lbl_vals.iter()) {
            show(c, on_prog);
        }
        show(&self.bar_cur, on_prog);
        for c in [&self.lbl_cur, &self.lbl_saving_cur] {
            show(c, on_prog);
        }
        // Two identical bars for a single title is a bug, so the overall row
        // only appears for a multi-title run.
        show(&self.bar_all, on_prog && v.show_overall_bar);
        for c in [&self.lbl_all, &self.lbl_saving_all] {
            show(c, on_prog && v.show_overall_bar);
        }
        show(&self.btn_cancel, on_prog);

        let on_result = p == Page::Result;
        for c in [&self.lbl_result_head, &self.lbl_result_line] {
            show(c, on_result);
        }
        for c in [&self.btn_reveal, &self.btn_done] {
            show(c, on_result);
        }

        // ── tree ──
        let sig = Some(rows_sig(&v.title_rows));
        let changed = self.memo.borrow().rows != sig;
        if changed {
            self.rebuild_tree(&v.title_rows);
            self.memo.borrow_mut().rows = sig;
        } else if p == Page::Titles {
            // A hidden tree needs no ticks; the next Titles render syncs them.
            self.sync_tree_states(&v.title_rows);
        }

        // ── selection bar: refilled only when its choices or the disc's languages change ──
        if self.memo.borrow().pick != v.pick {
            self.fill_pick_bar(v.pick.as_ref());
            self.memo.borrow_mut().pick = v.pick.clone();
        }

        // ── output row ──
        if self.edit_out.text().unwrap_or_default() != v.output_dir {
            let _ = self.edit_out.set_text(&v.output_dir);
        }
        self.btn_run.hwnd().EnableWindow(v.can_run);
        // Set only when the line changes; the core measures it off this thread.
        if self.memo.borrow().free_line.as_deref() != Some(v.free_space_line.as_str()) {
            let _ = self.lbl_free.hwnd().SetWindowText(&v.free_space_line);
            self.memo.borrow_mut().free_line = Some(v.free_space_line.clone());
        }

        // ── progress ──
        if let Some(info) = &v.info {
            for (i, val) in info.iter().enumerate() {
                if let Some(l) = self.lbl_vals.get(i) {
                    let _ = l.hwnd().SetWindowText(val);
                }
            }
        }
        self.bar_cur.set_position(v.bar_current.round() as u32);
        self.bar_all.set_position(v.bar_overall.round() as u32);
        let _ = self.lbl_cur.hwnd().SetWindowText(&v.caption_current);
        let _ = self.lbl_all.hwnd().SetWindowText(&v.caption_overall);
        let _ = self.lbl_saving_cur.hwnd().SetWindowText(&v.saving_current);
        let _ = self.lbl_saving_all.hwnd().SetWindowText(&v.saving_overall);

        // ── result ──
        // NEVER hardcode "Finished" — a cancelled run says otherwise.
        let _ = self.lbl_result_head.hwnd().SetWindowText(&v.result_heading);
        let _ = self.lbl_result_line.hwnd().SetWindowText(&v.result_summary);

        // ── log ──
        // Only new lines are appended, so selection survives an ordinary tick.
        let plan = log_plan(self.memo.borrow().log, v.log_first, v.log.len());
        let changed = match plan {
            LogPlan::Keep => false,
            LogPlan::Rebuild => {
                // The EDIT default cap is 32K chars; the log holds far more.
                self.log.limit_text(None);
                let _ = self.log.set_text(&log_text(&v.log));
                let n = self.log.hwnd().GetWindowTextLength().unwrap_or(0);
                self.log.set_selection(n, n);
                true
            }
            LogPlan::Append(from) => {
                let n = self.log.hwnd().GetWindowTextLength().unwrap_or(0);
                self.log.set_selection(n, n);
                unsafe {
                    self.log.hwnd().SendMessage(msg::EmReplaceSel {
                        can_be_undone: false,
                        replacement_text: w::WString::from_str(log_tail_text(&v.log, from)),
                    });
                }
                true
            }
        };
        if changed {
            // Keep the newest line in view, as the macOS log does.
            unsafe { self.log.hwnd().SendMessage(msg::EmScrollCaret {}) };
            self.memo.borrow_mut().log = Some(LogShown {
                first: v.log_first,
                len: v.log.len(),
            });
        }

        // The View ▸ log item names the action it will PERFORM, so it has to be
        // re-titled whenever the log's visibility changes — it is a toggle, and
        // a toggle that always says "Show log" is wrong half the time.
        self.sync_log_menu_title(&v.log_menu_label);
        self.sync_menu_enabled();
        self.relayout_now();
    }

    // Fill the selection bar's choosers from the view: the Titles list, and the
    // closed text of Audio and Subtitles (their menus are built when opened).
    fn fill_pick_bar(&self, pick: Option<&crate::ui::PickView>) {
        let Some(v) = pick else { return };
        let labels: Vec<String> = v.titles.iter().map(|(_, l)| l.clone()).collect();
        // Only a new list is rebuilt: rebuilding would dismiss the list mid-pick.
        if combo_items(&self.cmb_pick_titles) != labels {
            self.cmb_pick_titles.items().delete_all();
            let _ = self.cmb_pick_titles.items().add(&labels);
            let box_w = self
                .cmb_pick_titles
                .hwnd()
                .GetWindowRect()
                .map_or(0, |r| r.right - r.left);
            let text_w = combo_text_width(&self.cmb_pick_titles, &labels);
            let dpi = window_dpi(self.wnd.hwnd());
            set_dropped_width(
                &self.cmb_pick_titles,
                lay::combo_widths(dpi, box_w, text_w, box_w).1,
            );
        }
        self.cmb_pick_titles
            .items()
            .select(Some(v.title_index() as u32));
        let _ = self.btn_pick_audio.hwnd().SetWindowText(&v.audio_summary);
        let _ = self.btn_pick_subs.hwnd().SetWindowText(&v.subs_summary);
    }

    // A Titles entry was chosen: the core re-ticks the tree for it.
    fn on_pick_titles(&self) {
        let at = self.cmb_pick_titles.items().selected_index();
        let mode = self
            .memo
            .borrow()
            .pick
            .as_ref()
            .zip(at)
            .and_then(|(v, i)| v.title_choice(i as usize));
        if let Some(mode) = mode {
            let fx = self.app_mut(|a| a.pick_titles(mode));
            self.perform(fx);
        }
    }

    // Open the Audio menu under its button and apply the entry chosen.
    fn on_pick_audio(&self) {
        let entries = match self.memo.borrow().pick.as_ref() {
            Some(v) => v.audio_menu(),
            None => return,
        };
        let Some(tag) = track_pick_menu(self.wnd.hwnd(), &self.btn_pick_audio, &entries) else {
            return;
        };
        let choice = self
            .memo
            .borrow()
            .pick
            .as_ref()
            .and_then(|v| v.audio_choice(tag));
        if let Some(code) = choice {
            let fx = self.app_mut(|a| a.pick_audio(code.as_deref()));
            self.perform(fx);
        }
    }

    // Open the Subtitles menu under its button and apply the entry chosen.
    fn on_pick_subs(&self) {
        let entries = match self.memo.borrow().pick.as_ref() {
            Some(v) => v.subs_menu(),
            None => return,
        };
        let Some(tag) = track_pick_menu(self.wnd.hwnd(), &self.btn_pick_subs, &entries) else {
            return;
        };
        let choice = self
            .memo
            .borrow()
            .pick
            .as_ref()
            .and_then(|v| v.subs_choice(tag));
        if let Some(choice) = choice {
            let fx = self.app_mut(|a| a.pick_subtitles(choice));
            self.perform(fx);
        }
    }

    // Grey out everything unavailable while a rip is in flight. The RULE comes
    // from the core (`ui::blocked_while_running`), consulted per id — never a
    // second hardcoded list. Cancel is deliberately never blocked.
    fn sync_menu_enabled(&self) {
        let Some(bar) = self.wnd.hwnd().GetMenu() else {
            return;
        };
        let running = self.app.borrow().running();
        if self.memo.borrow().menu_running == Some(running) {
            return;
        }
        self.memo.borrow_mut().menu_running = Some(running);
        for &id in MENU_CMD_IDS {
            let blocked = cmd_for(id).map(crate::ui::blocked_while_running) == Some(true);
            // EnableMenuItem searches submenus by command id.
            let _ = bar.EnableMenuItem(w::IdPos::Id(id), !(running && blocked));
        }
    }

    // Apply the core's format list to the dropdown, preserving the current pick
    // when it survives. A combo box has no separators, so the core's groups
    // are flattened, keeping their order so the ordinary case stays first.
    fn sync_formats(&self, v: &View) {
        let wanted: Vec<String> = v
            .formats
            .iter()
            .flat_map(|g| g.iter().map(|s| crate::ui::format_label(s)))
            .collect();
        let sig = wanted.join("\n");
        if self.memo.borrow().formats != sig {
            // Rebuilding unconditionally would dismiss the list mid-click.
            self.cmb_format.items().delete_all();
            let _ = self.cmb_format.items().add(&wanted);
            self.memo.borrow_mut().formats = sig;
            // The box keeps its layout width; the open list shows every format in full.
            let box_w = self
                .cmb_format
                .hwnd()
                .GetWindowRect()
                .map_or(0, |r| r.right - r.left);
            let text_w = combo_text_width(&self.cmb_format, &wanted);
            let dpi = window_dpi(self.wnd.hwnd());
            set_dropped_width(
                &self.cmb_format,
                lay::combo_widths(dpi, box_w, text_w, box_w).1,
            );
        }
        let want_label = crate::ui::format_label(&v.format);
        let idx = wanted.iter().position(|t| *t == want_label);
        if self.cmb_format.items().selected_index() != idx.map(|i| i as u32) {
            self.cmb_format
                .items()
                .select(idx.map(|i| i as u32).or(Some(0)));
        }
    }
}

// The entries a dropdown holds, in order.
fn combo_items(c: &gui::ComboBox) -> Vec<String> {
    c.items()
        .iter()
        .map(|it| it.filter_map(|t| t.ok()).collect::<Vec<_>>())
        .unwrap_or_default()
}

// Show a selection-bar menu under its button and return the tag of the entry
// chosen; `None` means dismissed. The entries, ticks and separators are the core's.
fn track_pick_menu(
    owner: &w::HWND,
    btn: &gui::Button,
    entries: &[crate::ui::PickEntry],
) -> Option<isize> {
    let anchor = btn.hwnd().GetWindowRect().ok()?;
    let mut menu = w::HMENU::CreatePopupMenu().ok()?;
    for e in entries {
        if e.separator_before {
            let _ = menu.AppendMenu(co::MF::SEPARATOR, w::IdMenu::None, w::BmpPtrStr::None);
        }
        let flags = if e.on {
            co::MF::STRING | co::MF::CHECKED
        } else {
            co::MF::STRING
        };
        // The core's tags are small and positive; one that is not cannot be an id.
        let Ok(id) = u16::try_from(e.tag) else {
            continue;
        };
        let _ = menu.AppendMenu(flags, w::IdMenu::Id(id), w::BmpPtrStr::from_str(&e.label));
    }
    // RETURNCMD hands the chosen id back rather than posting WM_COMMAND, so these
    // tags cannot collide with the main window's menu ids (see `LangPicker`).
    owner.SetForegroundWindow();
    let picked = menu.TrackPopupMenu(
        co::TPM::LEFTBUTTON | co::TPM::RETURNCMD,
        w::POINT::with(anchor.left, anchor.bottom),
        owner,
    );
    let _ = unsafe { owner.PostMessage(msg::WmNull {}) };
    let _ = menu.DestroyMenu();
    picked.ok()?.map(|id| id as isize)
}

// The exact text the log pane shows. A plain EDIT control can't colour lines
// (unlike macOS), so notices get a one-character gutter instead; pulled out
// of `render` so the gutter rule can be checked without a window.
fn log_text(log: &[LogLine]) -> String {
    log.iter()
        .map(|l| match l.kind {
            LogKind::Notice => format!("! {}", l.text),
            LogKind::Detail | LogKind::Result => l.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

// `log[from..]` as appended after what is already shown: each line gets its
// own leading break unless it is the pane's very first line.
fn log_tail_text(log: &[LogLine], from: usize) -> String {
    let tail = log_text(&log[from..]);
    // A lone empty line is still a line: it needs its break, so only the first line
    // and an empty slice go without one.
    if from == 0 || log[from..].is_empty() {
        tail
    } else {
        format!("\r\n{tail}")
    }
}

// ── platform effects ──────────────────────────────────────────────────────

/// `explorer /select,"<path>"` opens the containing folder with the item
/// highlighted — the Explorer equivalent of macOS's "reveal in Finder".
fn reveal_in_explorer(p: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    // `raw_arg` bypasses Rust's argument quoting: explorer.exe needs the
    // literal `/select,"<path>"` form, which normal escaping would mangle.
    std::process::Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{p}\""))
        .spawn()
        .map(drop)
}

/// The AppUserModelID the process runs under and toasts are sent as.
const APP_ID: &str = "org.freemkv.FreeMKV";
/// How many rip-finished toasts stay clickable: the Action Center's per-app cap.
const TOASTS_KEPT: usize = 20;

// An unpackaged exe can only toast under an AUMID registered here (or on a
// Start-menu shortcut); `CreateToastNotifierWithId` fails silently otherwise.
fn register_toast_app_id() -> w::SysResult<()> {
    let (key, _) = w::HKEY::CURRENT_USER.RegCreateKeyEx(
        &format!(r"Software\Classes\AppUserModelId\{APP_ID}"),
        None,
        co::REG_OPTION::NON_VOLATILE,
        co::KEY::SET_VALUE,
        None,
    )?;
    key.RegSetValueEx(
        Some("DisplayName"),
        w::RegistryValue::Sz("freemkv".to_owned()),
    )
}

/// Show the rip-finished toast; clicking it reveals `output_dir`, if any. The caller
/// keeps the returned toast alive so its `Activated` handler stays wired.
fn show_rip_toast(
    title: &str,
    body: &str,
    output_dir: Option<&str>,
) -> windows::core::Result<windows::UI::Notifications::ToastNotification> {
    use windows::UI::Notifications::{
        ToastNotification, ToastNotificationManager, ToastTemplateType,
    };
    use windows::core::HSTRING;
    let xml = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)?;
    let slots = xml.GetElementsByTagName(&HSTRING::from("text"))?;
    for (i, text) in [title, body].into_iter().enumerate() {
        let node = xml.CreateTextNode(&HSTRING::from(text))?;
        slots.Item(i as u32)?.AppendChild(&node)?;
    }
    let toast = ToastNotification::CreateToastNotification(&xml)?;
    if let Some(dir) = output_dir.map(str::to_owned) {
        // Runs off the UI thread, so the log pane is out of reach: trace it instead.
        toast.Activated(&windows::Foundation::TypedEventHandler::new(move |_, _| {
            if let Err(e) = reveal_in_explorer(&dir) {
                tracing::warn!("toast click could not open {dir} in Explorer: {e}");
            }
            Ok(())
        }))?;
    }
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
    Ok(toast)
}

impl Shell {
    /// Show the real Windows common dialog (`IFileOpenDialog`), so the open
    /// experience is the OS's own — the same dialog every other Windows program
    /// shows, not a drawn imitation.
    fn pick(&self, folder: bool, title: &str, filter_source: bool) -> Option<String> {
        let dlg = w::CoCreateInstance::<w::IFileOpenDialog>(
            &co::CLSID::FileOpenDialog,
            None::<&w::IUnknown>,
            co::CLSCTX::INPROC_SERVER,
        )
        .ok()?;
        let mut opts = dlg.GetOptions().ok()? | co::FOS::FORCEFILESYSTEM;
        opts |= if folder {
            co::FOS::PICKFOLDERS
        } else {
            co::FOS::FILEMUSTEXIST
        };
        dlg.SetOptions(opts).ok()?;
        let _ = dlg.SetTitle(title);
        if !folder && filter_source {
            // The sink list comes from the core, so the picker can never accept
            // something the engine does not handle.
            let pattern = crate::ui::SOURCE_EXTS
                .iter()
                .map(|e| format!("*.{}", e.to_ascii_lowercase()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(";");
            let _ = dlg.SetFileTypes(&[
                (crate::strings::get("gui.panel.source_msg"), pattern),
                ("*.*".to_string(), "*.*".to_string()),
            ]);
        }
        if !dlg.Show(self.wnd.hwnd()).ok()? {
            return None;
        }
        dlg.GetResult()
            .ok()?
            .GetDisplayName(co::SIGDN::FILESYSPATH)
            .ok()
    }

    fn perform(&self, effects: Vec<Effect>) {
        for e in effects {
            match e {
                Effect::PickSource => {
                    if let Some(p) =
                        self.pick(false, &crate::strings::get("gui.panel.source_msg"), true)
                    {
                        let fx = self.app_mut(|a| a.open(&p));
                        self.perform(fx);
                    }
                }
                Effect::PickOutputDir => {
                    if let Some(p) =
                        self.pick(true, &crate::strings::get("gui.panel.output_msg"), false)
                    {
                        self.app_mut(|a| a.output_dir = p);
                    }
                }
                Effect::Reveal(p) => {
                    if let Err(e) = reveal_in_explorer(&p) {
                        // Silent failure is itself a bug: a failed reveal used to
                        // vanish into `let _ =`, leaving a user who clicked "show
                        // in folder" with no clue what went wrong.
                        self.app_mut(|a| {
                            a.say(
                                LogKind::Notice,
                                &crate::strings::fmt_or(
                                    "gui.log.reveal_failed",
                                    "Could not open the folder in Explorer: {e}",
                                    &[("e", &e.to_string())],
                                ),
                            )
                        });
                    }
                }
                Effect::OpenUrl(u) => {
                    if let Err(e) =
                        self.wnd
                            .hwnd()
                            .ShellExecute("open", &u, None, None, co::SW::SHOWNORMAL)
                    {
                        self.app_mut(|a| {
                            a.say(
                                LogKind::Notice,
                                &crate::strings::fmt_or(
                                    "gui.log.open_url_failed",
                                    "Could not open {url}: {e}",
                                    &[("url", &u), ("e", &e.to_string())],
                                ),
                            )
                        });
                    }
                }
                Effect::ShowSettings => self.prefs.show(&self.settings.borrow()),
                Effect::ShowAbout => self.about.show(&self.settings.borrow()),
                Effect::StartTicking => {
                    Self::report_timer_failure(&self.wnd, TIMER_TICK, TICK_MS);
                }
                Effect::StopTicking => {
                    let _ = self.wnd.hwnd().KillTimer(TIMER_TICK);
                }
                // File > Exit must not be a second, unguarded quit path: it used
                // to go straight to PostQuitMessage, skipping confirmation, the
                // Cancel signal, and the drain that WM_CLOSE (the window's X) did.
                Effect::Quit => {
                    if self.confirm_quit_mid_rip() {
                        self.cancel_and_drain();
                        w::PostQuitMessage(0);
                    }
                }
                Effect::Redraw => {}
                Effect::NotifyRipFinished {
                    title,
                    body,
                    output_dir,
                } => {
                    let shown = register_toast_app_id()
                        .map_err(|e| e.to_string())
                        .and_then(|()| {
                            show_rip_toast(&title, &body, output_dir.as_deref())
                                .map_err(|e| e.message().to_string())
                        });
                    match shown {
                        Ok(t) => {
                            let mut kept = self.toasts.borrow_mut();
                            kept.push(t);
                            let over = kept.len().saturating_sub(TOASTS_KEPT);
                            kept.drain(..over);
                        }
                        Err(e) => tracing::warn!("rip-finished toast failed: {e}"),
                    }
                }
            }
        }
        self.render();
    }

    // `SetTimer` can fail; if the poller never starts the rip finishes silently in the
    // background forever.
    fn report_timer_failure(wnd: &gui::WindowMain, id: usize, elapse_ms: u32) {
        if let Err(e) = wnd.hwnd().SetTimer(id, elapse_ms, None) {
            let _ = wnd.hwnd().MessageBox(
                &crate::strings::fmt_or(
                    "gui.dialog.timer_failed",
                    "freemkv could not start its progress timer ({error}). The rip \
                     may still be running, but this window will not update. \
                     Please restart freemkv.",
                    &[("error", &e.to_string())],
                ),
                "freemkv",
                co::MB::ICONERROR,
            );
        }
    }

    fn start_drain(&self) {
        Self::report_timer_failure(&self.wnd, TIMER_DRAIN, TICK_MS);
    }
}

// ── events ────────────────────────────────────────────────────────────────

impl Shell {
    fn events(&self) {
        // Every menu command routes through the core, so the shell decides
        // nothing about what a command means or when it is allowed.
        for &id in MENU_CMD_IDS {
            let me = self.clone();
            self.wnd.on().wm_command_acc_menu(id, move || {
                match id {
                    IDM_OPEN_DISC => me.open_disc(true),
                    _ => {
                        if let Some(cmd) = cmd_for(id) {
                            me.act(cmd);
                        }
                    }
                }
                Ok(())
            });
        }

        // Edit ▸ Copy / Select All act on the focused control, so text commands
        // keep working inside the log — binding them to the tree commands would
        // break copying a log line into a bug report.
        for (id, wm) in [
            (IDM_COPY, co::WM::COPY),
            (IDM_SELECT_ALL_TEXT, unsafe { co::WM::from_raw(0x00B1) }), // EM_SETSEL
        ] {
            self.wnd.on().wm_command_acc_menu(id, move || {
                if let Some(focused) = w::HWND::GetFocus() {
                    let (wp, lp) = if wm == co::WM::COPY { (0, 0) } else { (0, -1) };
                    let _ = unsafe {
                        focused.SendMessage(msg::Wm {
                            msg_id: wm,
                            wparam: wp,
                            lparam: lp,
                        })
                    };
                }
                Ok(())
            });
        }

        let me = self.clone();
        self.wnd.on().wm_create(move |_| {
            // Real DPI is finally knowable now the window exists (may differ from
            // system DPI on a secondary display). `apply_dpi` rebuilds font AND
            // glyph image list at that DPI, replacing fixed-size `build_check_images`.
            me.apply_dpi();
            me.sync_tree_head();
            me.apply_tree_direction();
            // Title-bar icons need the window, so they belong here too.
            set_icons(me.wnd.hwnd());
            me.wnd.hwnd().DragAcceptFiles(true);
            // Nothing is open at launch: show the empty state rather than a
            // tree of invented rows.
            me.render();
            // Launch probe: a disc already in the drive was never scanned at
            // launch (only File > Open disc did that). Fixed via a one-shot timer,
            // not inline in WM_CREATE, so the empty page paints before the scan.
            if launch_probe_enabled() {
                let _ = me
                    .wnd
                    .hwnd()
                    .SetTimer(TIMER_LAUNCH_PROBE, LAUNCH_PROBE_MS, None);
            }
            Ok(0)
        });

        // One shot, killed before it runs so a slow scan can't queue a second.
        // `false` = no "no optical drive found" notice — nobody asked for this
        // probe, so a driveless machine should see nothing.
        let me = self.clone();
        self.wnd.on().wm_timer(TIMER_LAUNCH_PROBE, move || {
            let _ = me.wnd.hwnd().KillTimer(TIMER_LAUNCH_PROBE);
            match crate::app_entry::launch_source() {
                Some(src) => me.perform(me.app_mut(|a| a.open(src))),
                None => me.open_disc(false),
            }
            Ok(())
        });

        let me = self.clone();
        self.wnd.on().wm_size(move |p| {
            me.relayout(p.client_area.cx, p.client_area.cy);
            Ok(())
        });

        // Window moved to a monitor with different scaling: under PerMonitorV2
        // this is the app's cue to redraw at the new scale (Windows won't do it).
        // `lParam` is the suggested rect that keeps apparent size under the cursor.
        let me = self.clone();
        self.wnd.on().wm(co::WM::DPICHANGED, move |p: msg::Wm| {
            // LOWORD is the X DPI; Windows keeps X and Y equal in practice.
            let dpi = (p.wparam & 0xffff) as u32;
            let sug = unsafe { std::ptr::read(p.lparam as *const w::RECT) };

            // Fonts and glyphs first: the resize below triggers WM_SIZE, and
            // the relayout it runs should already be measuring the new type.
            me.apply_dpi_at(dpi);

            let _ = me.wnd.hwnd().SetWindowPos(
                w::HwndPlace::None,
                w::POINT::with(sug.left, sug.top),
                w::SIZE::with(sug.right - sug.left, sug.bottom - sug.top),
                co::SWP::NOZORDER | co::SWP::NOACTIVATE,
            );
            // Belt and braces: SetWindowPos normally raises WM_SIZE, but not
            // when only the position changed (two monitors at the same scale
            // either side of a scale change, or a maximized window).
            me.relayout_now();
            Ok(0)
        });

        // Never shrink below the point layout stops working. `ptMinTrackSize` is
        // an OUTER size, so frame+caption (DPI-dependent) must be added to the
        // client minimum via `GetSystemMetricsForDpi`, not the primary-monitor one.
        let me = self.clone();
        self.wnd.on().wm_get_min_max_info(move |p| {
            let dpi = window_dpi(me.wnd.hwnd());
            let (mw, mh) = lay::min_size(dpi);
            let metric = |sm: co::SM| {
                w::GetSystemMetricsForDpi(sm, dpi).unwrap_or_else(|_| w::GetSystemMetrics(sm))
            };
            let frame_x = metric(co::SM::CXSIZEFRAME) * 2 + metric(co::SM::CXPADDEDBORDER) * 2;
            let frame_y = metric(co::SM::CYSIZEFRAME) * 2
                + metric(co::SM::CXPADDEDBORDER) * 2
                + metric(co::SM::CYCAPTION)
                + metric(co::SM::CYMENU);
            p.info.ptMinTrackSize = w::POINT::with(mw + frame_x, mh + frame_y);
            Ok(())
        });

        // Dropping a file on the window is the ordinary way a Windows user opens
        // something they can see, and it mirrors the macOS Finder drop.
        let me = self.clone();
        self.wnd.on().wm_drop_files(move |p| {
            let hdrop = p.hdrop;
            // Swallow-and-log, never `?`: an `Err` here unwinds into `run()`'s
            // error arm, which MessageBoxes and exits, bypassing quit-confirm/
            // drain and discarding a rip in progress. Log a failed enumeration.
            let mut names = match hdrop.DragQueryFile() {
                Ok(n) => n,
                Err(e) => {
                    me.app_mut(|a| {
                        a.say(
                            LogKind::Notice,
                            &crate::strings::fmt_or(
                                "gui.log.drop_unreadable",
                                "Could not read the dropped item: {e}",
                                &[("e", &e.to_string())],
                            ),
                        )
                    });
                    return Ok(());
                }
            };
            if let Some(path) = names.next() {
                let path = match path {
                    Ok(p) => p,
                    Err(e) => {
                        me.app_mut(|a| {
                            a.say(
                                LogKind::Notice,
                                &crate::strings::fmt_or(
                                    "gui.log.drop_unreadable",
                                    "Could not read the dropped item: {e}",
                                    &[("e", &e.to_string())],
                                ),
                            )
                        });
                        return Ok(());
                    }
                };
                // A DIRECTORY is a valid source (an extracted disc tree opens
                // as `dir://`), and an extension-only allow-list rejects every
                // one of them — the same gap the macOS shell had.
                if std::path::Path::new(&path).is_dir()
                    || crate::ui::SOURCE_EXTS.iter().any(|e| {
                        std::path::Path::new(&path)
                            .extension()
                            .and_then(|x| x.to_str())
                            .is_some_and(|x| x.eq_ignore_ascii_case(e))
                    })
                {
                    let fx = me.app_mut(|a| a.open(&path));
                    me.perform(fx);
                } else {
                    me.app_mut(|a| {
                        a.say(
                            LogKind::Notice,
                            &crate::strings::fmt("gui.log.not_supported", &[("p", &path)]),
                        )
                    });
                }
            }
            Ok(())
        });

        // Closing mid-rip must not silently tear down the worker.
        let me = self.clone();
        self.wnd.on().wm_close(move || {
            // Same decision as File > Exit, asked in one place.
            if !me.confirm_quit_mid_rip() {
                return Ok(()); // keep ripping
            }
            if me.app.borrow().running() {
                me.cancel_and_drain();
            }
            me.wnd.hwnd().DestroyWindow()?;
            Ok(())
        });

        // The rip poller and the worker-message drain.
        let me = self.clone();
        self.wnd.on().wm_timer(TIMER_TICK, move || {
            // Not `app_mut`: `perform` always ends in the one render a tick needs.
            let fx = me.app.borrow_mut().tick();
            me.perform(fx);
            Ok(())
        });
        let me = self.clone();
        self.wnd.on().wm_timer(TIMER_DRAIN, move || {
            me.drain();
            Ok(())
        });

        // ── controls ──
        let me = self.clone();
        self.btn_open.on().bn_clicked(move || {
            me.act(Cmd::Open);
            Ok(())
        });
        // The empty page's "Open disc" — the SAME method File ▸ Open disc
        // runs, so the two entry points cannot behave differently.
        let me = self.clone();
        self.btn_open_disc.on().bn_clicked(move || {
            me.open_disc(true);
            Ok(())
        });
        let me = self.clone();
        self.btn_browse.on().bn_clicked(move || {
            me.act(Cmd::SetOutput);
            Ok(())
        });
        let me = self.clone();
        self.btn_run.on().bn_clicked(move || {
            me.act(Cmd::Run);
            Ok(())
        });
        let me = self.clone();
        self.btn_cancel.on().bn_clicked(move || {
            me.act(Cmd::Cancel);
            Ok(())
        });
        let me = self.clone();
        self.btn_eject.on().bn_clicked(move || {
            me.act(Cmd::Eject);
            Ok(())
        });
        let me = self.clone();
        self.btn_reveal.on().bn_clicked(move || {
            let d = me.app.borrow().output_dir.clone();
            me.perform(vec![Effect::Reveal(d)]);
            Ok(())
        });
        let me = self.clone();
        self.btn_done.on().bn_clicked(move || {
            let fx = me.app_mut(|a| a.dismiss_result());
            me.perform(fx);
            Ok(())
        });

        // The selection bar: each choice goes to the core, which re-ticks the tree.
        let me = self.clone();
        self.cmb_pick_titles.on().cbn_sel_change(move || {
            me.on_pick_titles();
            Ok(())
        });
        // A split button's face and its arrow both open the menu.
        let me = self.clone();
        self.btn_pick_audio.on().bn_clicked(move || {
            me.on_pick_audio();
            Ok(())
        });
        let me = self.clone();
        self.btn_pick_audio.on().bcn_drop_down(move |_| {
            me.on_pick_audio();
            Ok(())
        });
        let me = self.clone();
        self.btn_pick_subs.on().bn_clicked(move || {
            me.on_pick_subs();
            Ok(())
        });
        let me = self.clone();
        self.btn_pick_subs.on().bcn_drop_down(move |_| {
            me.on_pick_subs();
            Ok(())
        });

        // Without an action the dropdown is decoration: it shows a choice the
        // model never hears about, so the rip silently uses the old format.
        let me = self.clone();
        self.cmb_format.on().cbn_sel_change(move || {
            me.on_format_pick();
            Ok(())
        });

        // A typed-in output folder must reach the model, or Run writes somewhere
        // other than what the field says.
        let me = self.clone();
        self.edit_out.on().en_change(move || {
            let t = me.edit_out.text().unwrap_or_default();
            if me.app.borrow().output_dir != t {
                me.app.borrow_mut().output_dir = t;
            }
            Ok(())
        });

        // Clicking the tick box: the core owns the cascade and the tri-state, so
        // the shell only reports which row was clicked.
        let me = self.clone();
        self.tree.on().nm_click(move || {
            if let Some(row) = me.hit_state_icon() {
                // Toggle DIRECTION is core policy, not a shell decision — this used
                // to read `Off | Mixed` here while mac.rs read mixed as "off", so a
                // partly-ticked title selected on Windows and cleared on macOS.
                me.app_mut(|a| a.tree.toggle(row));
            }
            Ok(0)
        });

        // The selected row is the core's, as on macOS.
        let me = self.clone();
        self.tree.on().tvn_sel_changed(move |p| {
            let h = unsafe { p.itemNew.hItem.raw_copy() };
            if h != w::HTREEITEM::NULL {
                let idx = *me.tree.items().get(&h).data().borrow();
                me.app_mut(|a| a.selected_row = Some(idx));
            }
            Ok(())
        });

        // The columns' cells, painted into each row after the tree drew it.
        let me = self.clone();
        self.tree.on().nm_custom_draw(move |cd| {
            Ok(match cd.nmcd.dwDrawStage {
                co::CDDS::PREPAINT => co::CDRF::NOTIFYITEMDRAW,
                co::CDDS::ITEMPREPAINT => co::CDRF::NOTIFYPOSTPAINT,
                co::CDDS::ITEMPOSTPAINT => {
                    me.paint_tree_cells(&cd.nmcd);
                    co::CDRF::DODEFAULT
                }
                _ => co::CDRF::DODEFAULT,
            })
        });

        // A header drag moves the divider between two columns; the shell then
        // lays every column out again from the widths it leaves.
        let me = self.clone();
        self.tree_head.on().hdn_item_changed(move |p| {
            if me.cols.borrow().syncing {
                return Ok(());
            }
            let Some(item) = p.pitem().filter(|it| it.mask.has(co::HDI::WIDTH)) else {
                return Ok(());
            };
            let dpi = window_dpi(me.wnd.hwnd());
            let new_w = item.cxy;
            {
                let mut c = me.cols.borrow_mut();
                let Some(laid) = c.laid.take() else {
                    return Ok(());
                };
                // Item 0 spans the tree's border as well as the tick column.
                let new_w = if p.iItem == 0 { new_w - c.inset } else { new_w };
                let flex = c.flex();
                c.widths = lay::drag_column(dpi, &laid, flex, p.iItem as usize, new_w);
            }
            me.layout_tree_cols(dpi);
            Ok(())
        });

        self.prefs.events(self);
        self.about.events(self);
    }

    // Choose an output format from the dropdown's visible text, resolved
    // against the core's list (not trusted directly) so an unknown value can
    // never enter the model, and selection works in every locale.
    fn on_format_pick(&self) {
        let Ok(Some(label)) = self.cmb_format.items().selected_text() else {
            return;
        };
        let (disc, fit) = {
            let a = self.app.borrow();
            (!crate::ui::is_container(&a.source), a.fit())
        };
        if let Some(f) = crate::ui::format_from_label(&label, disc, fit) {
            self.act(Cmd::SetFormat(f));
        }
    }

    // Re-title the View > log menu item. Located by walking submenus and
    // matching the command id, set by position — not `IdPos::Id` (undocumented
    // submenu recursion) or a hardcoded position (menu rebuilds on language change).
    fn sync_log_menu_title(&self, label: &str) {
        let Some(bar) = self.wnd.hwnd().GetMenu() else {
            return;
        };
        let text = log_menu_text(label);
        let Ok(tops) = bar.GetMenuItemCount() else {
            return;
        };
        for i in 0..tops {
            let Some(sub) = bar.GetSubMenu(i) else {
                continue;
            };
            let Ok(n) = sub.GetMenuItemCount() else {
                continue;
            };
            for j in 0..n {
                if !matches!(
                    sub.item_info(w::IdPos::Pos(j)),
                    Ok(w::MenuItemInfo::Entry { cmd_id, .. }) if cmd_id == IDM_TOGGLE_LOG
                ) {
                    continue;
                }
                // Already right: skip the churn (and the menu-bar redraw).
                if matches!(sub.GetMenuString(w::IdPos::Pos(j)), Ok(cur) if cur == text) {
                    return;
                }
                // `force_heap` so the buffer cannot live inside `wstr` itself
                // (SSO) and be invalidated by a move before the call.
                let mut wstr = w::WString::from_str_force_heap(&text);
                let mut mii = w::MENUITEMINFO::default();
                mii.fMask = co::MIIM::STRING;
                mii.dwTypeData = unsafe { wstr.as_mut_ptr() };
                let _ = sub.SetMenuItemInfo(w::IdPos::Pos(j), &mii);
                let _ = self.wnd.hwnd().DrawMenuBar();
                return;
            }
        }
    }

    // Open the disc in the drive; the decision (which drive, what to log) is
    // the core's (`ui::App::disc_source`). Two `app_mut` calls ON PURPOSE: each
    // repaints, so "Opening …" reaches the log BEFORE `open`'s blocking scan.
    fn open_disc(&self, announce_missing: bool) {
        let Some(url) = self.app_mut(|a| a.disc_source(announce_missing)) else {
            return;
        };
        let fx = self.app_mut(|a| {
            if announce_missing {
                a.open(&url)
            } else {
                a.open_probe(&url)
            }
        });
        self.perform(fx);
    }

    /// Drain worker-thread messages onto the log and into the Settings note.
    fn drain(&self) {
        // RECOVER a poisoned inbox rather than returning (see macOS `onDrain:`):
        // returning stranded `set_keydb_updating(false)` and skipped KillTimer,
        // leaving a 5 Hz timer running for the process's life on a dead lock.
        let msgs: Vec<(LogKind, String)> = self
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        if msgs.is_empty() {
            return;
        }
        for (kind, m) in &msgs {
            self.app_mut(|a| a.say(*kind, m));
        }
        // Surface the keydb outcome in the Settings note so the user sees the
        // result in place, not only in the (possibly hidden) log.
        if let Some((_, last)) = msgs.last() {
            self.prefs.set_keydb_note(last);
        }
        self.prefs.set_keydb_updating(false);
        let _ = self.wnd.hwnd().KillTimer(TIMER_DRAIN);
    }
}

// ── Settings ──────────────────────────────────────────────────────────────

/// The widest of `labels` in a combo's own font, in pixels.
fn combo_text_width(c: &gui::ComboBox, labels: &[String]) -> i32 {
    let Ok(dc) = c.hwnd().GetDC() else {
        return 0;
    };
    let font = unsafe { c.hwnd().SendMessage(msg::WmGetFont {}) };
    let _font = font.as_ref().and_then(|f| dc.SelectObject(f).ok());
    labels
        .iter()
        .filter_map(|t| dc.GetTextExtentPoint32(t).ok())
        .map(|sz| sz.cx)
        .max()
        .unwrap_or(0)
}

/// Let a combo's open list run wider than the box (`CB_SETDROPPEDWIDTH`).
fn set_dropped_width(c: &gui::ComboBox, width: i32) {
    let _ = unsafe {
        c.hwnd().SendMessage(msg::CbSetDroppedWidth {
            min_width: width.max(0) as u32,
        })
    };
}

// The option table for a settings dropdown: `(canonical, localized_label)`
// pairs. Canonical value persists; label is shown. Empty = not an enum combo.
fn enum_options(key: &str) -> Vec<(&'static str, String)> {
    match key {
        // Output container, localized. Windows-only: this combo is FLAT with no
        // separator rows, so the format list maps 1:1 onto indices; macOS's popup
        // interleaves separators and maps by title, so this stays out of `ui`.
        "container" => crate::ui::output_formats(true, true)
            .into_iter()
            .flatten()
            .map(|c| (c, crate::ui::format_label(c)))
            .collect(),
        // Every other dropdown is the shared table, owned by the core so the
        // two shells cannot offer different option sets.
        _ => crate::ui::enum_options(key),
    }
}

// A multi-select language picker: a button showing the chosen languages plus a checkable popup
// menu behind it (Win32 has no checked-list combo box).
#[derive(Clone)]
struct LangPicker {
    btn: gui::Button,
    stored: Rc<RefCell<String>>,
}

impl LangPicker {
    /// Adopt a stored preference and re-title the button from it.
    fn set(&self, stored: &str) {
        *self.stored.borrow_mut() = stored.to_string();
        // `lang_summary` never answers with an empty string — a blank button
        // reads as a control that failed to load, not as "no preference".
        let title = crate::ui::lang_summary(&self.stored.borrow());
        let _ = self.btn.hwnd().SetWindowText(&title);
    }

    fn value(&self) -> String {
        self.stored.borrow().clone()
    }

    // Show the checklist under the button and apply each tick. Re-opened after
    // every tick instead of closing on the first (this is multi-select), so
    // Escape/click-outside is still the only way to dismiss it.
    fn popup(&self, owner: &w::HWND) {
        // Anchored to the button's own rectangle, not the cursor: the menu then
        // lines up under the control it belongs to however it was invoked
        // (mouse, Space, or the keyboard accelerator on the label).
        let Ok(anchor) = self.btn.hwnd().GetWindowRect() else {
            return;
        };
        while let Some(code) = self.track_once(owner, anchor) {
            // The ONLY mutation. `ui::lang_toggle` owns what a tick means, so a
            // click here and a click on macOS cannot come to differ.
            let next = crate::ui::lang_toggle(&self.value(), &code);
            self.set(&next);
        }
    }

    /// One pass of the menu: build it from the current selection, track it, and
    /// return the code the user ticked. `None` means dismissed.
    fn track_once(&self, owner: &w::HWND, anchor: w::RECT) -> Option<String> {
        let mut menu = w::HMENU::CreatePopupMenu().ok()?;
        let langs = crate::ui::PICKER_LANGUAGES;
        // CYMENU (popup row height) and CYSCREEN feed `menu_column_rows`, which
        // turns a list taller than the screen into columns instead of scroll arrows.
        let rows = lay::menu_column_rows(
            langs.len(),
            w::GetSystemMetrics(co::SM::CYMENU),
            w::GetSystemMetrics(co::SM::CYSCREEN),
        );
        let cur = self.value();
        for (i, (code, _english)) in langs.iter().enumerate() {
            let name = crate::ui::lang_display_name(code);
            let mut flags = co::MF::STRING;
            if crate::ui::lang_is_selected(&cur, code) {
                flags |= co::MF::CHECKED;
            }
            if i > 0 && i % rows == 0 {
                // MENUBARBREAK, not MENUBREAK: it draws the vertical rule
                // between columns, without which two columns read as one wide
                // one with a stray gap.
                flags |= co::MF::MENUBARBREAK;
            }
            if menu
                .AppendMenu(
                    flags,
                    w::IdMenu::Id(IDM_LANG_BASE + i as u16),
                    w::BmpPtrStr::from_str(&name),
                )
                .is_err()
            {
                // A menu that is half-built is still usable; showing what was
                // added beats showing nothing at all.
                break;
            }
        }

        // RETURNCMD hands the chosen id back from the call rather than posting
        // WM_COMMAND, so this popup needs no command routing and cannot collide
        // with the main window's menu ids.
        owner.SetForegroundWindow();
        let picked = menu.TrackPopupMenu(
            co::TPM::LEFTBUTTON | co::TPM::RETURNCMD,
            w::POINT::with(anchor.left, anchor.bottom),
            owner,
        );
        // Both required, in this order: the null post is the documented fix for
        // TrackPopupMenu leaving the owner blind to the dismiss click, and since
        // the menu isn't attached to a window, nothing else will destroy it.
        let _ = unsafe { owner.PostMessage(msg::WmNull {}) };
        let _ = menu.DestroyMenu();

        let id = picked.ok()??;
        let idx = u16::try_from(id).ok()?.checked_sub(IDM_LANG_BASE)? as usize;
        langs.get(idx).map(|(code, _)| (*code).to_string())
    }
}

// Builds one labelled row per call, walking a y-cursor down a tab page — the
// right-aligned label / control-to-its-right layout macOS Settings uses. The
// `wd` widths callers pass are 96-DPI baselines, scaled here once.
struct Rows<'a> {
    page: &'a gui::TabPage,
    s: lay::Scale,
    m: lay::FormMetrics,
    y: i32,
    gutter: i32,
    width: i32,
}

impl<'a> Rows<'a> {
    fn new(page: &'a gui::TabPage, dpi: u32) -> Self {
        let m = lay::form_metrics(dpi);
        Rows {
            page,
            s: lay::Scale::new(dpi),
            m,
            y: m.top,
            gutter: m.gutter,
            width: m.width,
        }
    }

    fn label(&self, text: &str) {
        // Static decoration: owned by the page, never touched again.
        let _ = gui::Label::new(
            self.page,
            gui::LabelOpts {
                text,
                position: (self.s.px(8), self.y + self.s.px(3)),
                size: (self.gutter - self.s.px(16), self.s.px(18)),
                control_style: co::SS::RIGHT,
                ..Default::default()
            },
        );
    }

    fn field(&mut self, text: &str, val: &str, wd: i32) -> gui::Edit {
        self.label(text);
        let e = gui::Edit::new(
            self.page,
            gui::EditOpts {
                text: val,
                position: (self.gutter, self.y),
                width: self.s.px(wd),
                height: self.m.field_h,
                ..Default::default()
            },
        );
        self.y += self.m.row_step;
        e
    }

    // Same contract as `field`, but for a secret (keyserver bearer token):
    // `ES::PASSWORD` masks keystrokes with the system bullet character, fixing
    // a plain-text field legible during screen-sharing/recording.
    fn field_secure(&mut self, text: &str, val: &str, wd: i32) -> gui::Edit {
        self.label(text);
        let e = gui::Edit::new(
            self.page,
            gui::EditOpts {
                text: val,
                position: (self.gutter, self.y),
                width: self.s.px(wd),
                height: self.m.field_h,
                control_style: co::ES::AUTOHSCROLL | co::ES::PASSWORD,
                ..Default::default()
            },
        );
        self.y += self.m.row_step;
        e
    }

    // A language checklist row — see `LangPicker`. Same `row_step`/`field_h`
    // as `field` (a taller control overflows the Settings window). Title is
    // set here, not in `populate`: `hwnd()` is still null until after `new`.
    fn lang(&mut self, text: &str, val: &str, wd: i32) -> LangPicker {
        self.label(text);
        let title = crate::ui::lang_summary(val);
        let btn = gui::Button::new(
            self.page,
            gui::ButtonOpts {
                text: &title,
                position: (self.gutter, self.y),
                width: self.s.px(wd),
                height: self.m.field_h,
                // Left-aligned, not the default centred: this sits in a column of
                // text boxes/combos, and a summary re-centring on every tick reads
                // as the whole control jumping about.
                control_style: co::BS::PUSHBUTTON | co::BS::LEFT,
                ..Default::default()
            },
        );
        self.y += self.m.row_step;
        LangPicker {
            btn,
            stored: Rc::new(RefCell::new(val.to_string())),
        }
    }

    /// A path row: the label with the browse button that fills the field, then
    /// the field on a line of its own beneath them at the form's full width, so
    /// a long path reads whole instead of scrolled to one end.
    fn path(&mut self, text: &str, val: &str) -> (gui::Edit, gui::Button) {
        self.label(text);
        let b = gui::Button::new(
            self.page,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.browse"),
                position: (self.gutter, self.y - self.s.px(1)),
                width: self.s.px(34),
                height: self.s.px(24),
                ..Default::default()
            },
        );
        self.y += self.m.row_step;
        let e = gui::Edit::new(
            self.page,
            gui::EditOpts {
                text: val,
                position: (self.s.px(16), self.y),
                width: self.width - self.s.px(32),
                height: self.m.field_h,
                ..Default::default()
            },
        );
        self.y += self.m.row_step;
        (e, b)
    }

    fn check(&mut self, text: &str) -> gui::CheckBox {
        self.label(text);
        let c = gui::CheckBox::new(
            self.page,
            gui::CheckBoxOpts {
                text: "",
                position: (self.gutter, self.y + self.s.px(2)),
                size: (self.s.px(20), self.s.px(18)),
                ..Default::default()
            },
        );
        self.y += self.m.check_step;
        c
    }

    fn combo(&mut self, key: &str, text: &str, wd: i32) -> gui::ComboBox {
        self.label(text);
        let labels: Vec<String> = enum_options(key).into_iter().map(|(_, l)| l).collect();
        let c = gui::ComboBox::new(
            self.page,
            gui::ComboBoxOpts {
                position: (self.gutter, self.y),
                width: self.s.px(wd),
                items: &labels.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                ..Default::default()
            },
        );
        self.y += self.m.row_step;
        c
    }

    fn button(&mut self, text: &str, wd: i32) -> gui::Button {
        let b = gui::Button::new(
            self.page,
            gui::ButtonOpts {
                text,
                position: (self.gutter, self.y),
                width: self.s.px(wd),
                height: self.s.px(26),
                ..Default::default()
            },
        );
        self.y += self.m.button_step;
        b
    }

    /// An explanatory note under a control — never a control that does nothing.
    fn note(&mut self, text: &str) -> gui::Label {
        let l = gui::Label::new(
            self.page,
            gui::LabelOpts {
                text,
                position: (self.s.px(16), self.y),
                size: (self.width - self.s.px(32), self.s.px(32)),
                ..Default::default()
            },
        );
        self.y += self.m.note_step;
        l
    }

    fn gap(&mut self) {
        self.y += self.m.gap;
    }
}

// The Settings window: five tabs, every control mapping to something the
// engine or key layer actually consumes. Anything that couldn't reach real
// code was left out rather than shown as a switch that does nothing.
#[derive(Clone)]
struct Prefs {
    wnd: gui::WindowModeless,
    tab: gui::Tab,
    /// Kept so `select_tab` can swap the visible page. winsafe's own page-swap
    /// runs off the click notification and is private, and a programmatic
    /// `TCM_SETCURSEL` raises no notification.
    pages: Vec<gui::TabPage>,
    /// `(settings key, control)` for every editable control, so OK can read them
    /// back without a second, drifting list of keys.
    fields: Vec<(&'static str, gui::Edit)>,
    checks: Vec<(&'static str, gui::CheckBox)>,
    combos: Vec<(&'static str, gui::ComboBox)>,
    /// Each combo's width as built, in `combos` order — what `fit_combos` grows
    /// from, so a shorter language's labels shrink it back. Read on first fit.
    combo_w: Rc<RefCell<Vec<i32>>>,
    /// The language checklists, in the same registry shape as the rest — a
    /// control that is built but not listed here is silently write-only: it
    /// shows the stored value and OK never reads it back.
    langs: Vec<(&'static str, LangPicker)>,
    lbl_keydb: gui::Label,
    btn_keydb: gui::Button,
    btn_test: gui::Button,
    btn_browse_dest: gui::Button,
    btn_browse_keydb: gui::Button,
    btn_ok: gui::Button,
    btn_cancel: gui::Button,
}

impl Prefs {
    fn new(parent: &gui::WindowMain, st: &crate::settings::Settings) -> Self {
        let g = crate::strings::get;
        // Wide enough that the longest label and its translations fit the gutter
        // without clipping. Built before the main window exists, so system DPI is
        // the only one available — also the DPI it stays at, laid out once.
        let dpi = system_dpi();
        let s = lay::Scale::new(dpi);
        let (ww, wh) = (s.px(lay::PREFS_W), s.px(lay::PREFS_H));
        let wnd = gui::WindowModeless::new(
            parent,
            gui::WindowModelessOpts {
                title: &g("gui.win.settings"),
                class_name: "FmkvPrefs",
                size: (ww, wh),
                // Deliberately NOT visible: shown on demand. winsafe cannot
                // create a window inside an event closure, so unlike the macOS
                // shell this is built up-front and hidden.
                style: co::WS::CAPTION | co::WS::SYSMENU | co::WS::CLIPCHILDREN | co::WS::BORDER,
                ex_style: co::WS_EX::LEFT | co::WS_EX::DLGMODALFRAME,
                ..Default::default()
            },
        );

        // BTNFACE, not the default WINDOW: a settings page is dialog-coloured on
        // Windows, and it means the row labels blend into the page instead of
        // showing as grey bands on white.
        let pages: Vec<gui::TabPage> = (0..5)
            .map(|_| {
                gui::TabPage::new(
                    &wnd,
                    gui::TabPageOpts {
                        class_bg_brush: gui::Brush::Color(co::COLOR::BTNFACE),
                        ..Default::default()
                    },
                )
            })
            .collect();

        let mut fields: Vec<(&'static str, gui::Edit)> = Vec::new();
        let mut checks: Vec<(&'static str, gui::CheckBox)> = Vec::new();
        let mut combos: Vec<(&'static str, gui::ComboBox)> = Vec::new();
        let mut langs: Vec<(&'static str, LangPicker)> = Vec::new();

        // ── Output ── engine Job.dest + the GUI's own naming
        let mut r = Rows::new(&pages[0], dpi);
        combos.push((
            "container",
            r.combo("container", &g("gui.set.default_output"), 320),
        ));
        let (f_dest, btn_browse_dest) = r.path(&g("gui.set.default_dest"), &st.dest_dir);
        fields.push(("dest_dir", f_dest));
        fields.push((
            "filename_template",
            r.field(&g("gui.set.filename_template"), &st.filename_template, 240),
        ));
        r.gap();
        checks.push(("keep_iso", r.check(&g("gui.set.keep_iso"))));
        checks.push(("auto_eject", r.check(&g("gui.set.auto_eject"))));
        checks.push((
            "notify_when_rip_finished",
            r.check(&crate::strings::get_or(
                "gui.set.notify_when_rip_finished",
                "Notify when a rip finishes",
            )),
        ));

        // ── Selection ── engine Job.selection
        let mut r = Rows::new(&pages[1], dpi);
        combos.push((
            "selection",
            r.combo("selection", &g("gui.set.default_selection"), 240),
        ));
        fields.push((
            "min_title_secs",
            r.field(&g("gui.set.min_length"), &st.min_title_secs, 90),
        ));
        r.note(&g("gui.set.min_length_note"));
        r.gap();
        // Three INDEPENDENT language sets (see `ui::LangPrefs`) decide which
        // stream rows start ticked. Checklists, not text boxes: free text like
        // "German" could silently fail to match a `deu` stream, unlike a code list.
        langs.push((
            "audio_langs",
            r.lang(&g("gui.set.audio_langs"), &st.audio_langs, 240),
        ));
        langs.push((
            "sub_langs",
            r.lang(&g("gui.set.sub_langs"), &st.sub_langs, 240),
        ));
        langs.push((
            "forced_sub_langs",
            r.lang(&g("gui.set.forced_sub_langs"), &st.forced_sub_langs, 240),
        ));
        r.note(&g("gui.set.lang_prefs_note"));

        // ── Recovery ── engine Job.mode / abort_on_lost_secs / raw
        let mut r = Rows::new(&pages[2], dpi);
        combos.push(("rip_mode", r.combo("rip_mode", &g("gui.set.rip_mode"), 240)));
        fields.push((
            "max_passes",
            r.field(&g("gui.set.max_passes"), &st.max_passes, 80),
        ));
        fields.push((
            "abort_lost_secs",
            r.field(&g("gui.set.abort_lost"), &st.abort_lost_secs, 80),
        ));
        r.note(&g("gui.set.abort_lost_note"));
        r.gap();
        checks.push(("raw", r.check(&g("gui.set.keep_encrypted"))));
        r.note(&g("gui.set.raw_note"));
        r.gap();
        checks.push(("force", r.check(&g("gui.set.overwrite"))));

        // ── Keys ── keydb + the online key service
        let mut r = Rows::new(&pages[3], dpi);
        combos.push((
            "key_source",
            r.combo("key_source", &g("gui.set.key_source"), 260),
        ));
        r.gap();
        let (f_keydb, btn_browse_keydb) = r.path(&g("gui.set.keydb_path"), &st.keydb_path);
        fields.push(("keydb_path", f_keydb));
        fields.push((
            "keydb_url",
            r.field(&g("gui.set.keydb_url"), &st.keydb_url, 320),
        ));
        let btn_keydb = r.button(&g("gui.set.update_keydb"), 170);
        let lbl_keydb = r.note(&st.keydb_status());
        r.gap();
        fields.push((
            "keyserver_url",
            r.field(&g("gui.set.keyserver_url"), &st.keyserver_url, 320),
        ));
        fields.push((
            "keyserver_token",
            r.field_secure(&g("gui.set.keyserver_token"), &st.keyserver_token, 320),
        ));
        let btn_test = r.button(&g("gui.set.test_connection"), 170);

        // ── Advanced
        let mut r = Rows::new(&pages[4], dpi);
        combos.push(("language", r.combo("language", &g("gui.set.language"), 220)));
        fields.push((
            "decrypt_threads",
            r.field(&g("gui.set.decrypt_threads"), &st.decrypt_threads, 80),
        ));
        r.note(&g("gui.set.decrypt_threads_note"));
        r.gap();
        combos.push((
            "log_level",
            r.combo("log_level", &g("gui.set.log_detail"), 180),
        ));

        let tab_labels = [
            g("gui.tab.output"),
            g("gui.tab.selection"),
            g("gui.tab.recovery"),
            g("gui.tab.keys"),
            g("gui.tab.advanced"),
        ];
        let page_pairs: Vec<(&str, gui::TabPage)> = tab_labels
            .iter()
            .map(|s| s.as_str())
            .zip(pages.iter().cloned())
            .collect();
        let tab = gui::Tab::new(
            &wnd,
            gui::TabOpts {
                position: (s.px(10), s.px(10)),
                size: (ww - s.px(20), wh - s.px(66)),
                pages: &page_pairs,
                ..Default::default()
            },
        );

        let btn_ok = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &g("gui.btn.ok"),
                position: (ww - s.px(108), wh - s.px(44)),
                width: s.px(96),
                height: s.px(28),
                control_style: co::BS::DEFPUSHBUTTON,
                ..Default::default()
            },
        );
        let btn_cancel = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &g("gui.btn.cancel"),
                position: (ww - s.px(212), wh - s.px(44)),
                width: s.px(96),
                height: s.px(28),
                ..Default::default()
            },
        );

        Prefs {
            wnd,
            tab,
            pages,
            fields,
            checks,
            combos,
            combo_w: Rc::new(RefCell::new(Vec::new())),
            langs,
            lbl_keydb,
            btn_keydb,
            btn_test,
            btn_browse_dest,
            btn_browse_keydb,
            btn_ok,
            btn_cancel,
        }
    }

    /// Populate from the stored settings and show. Defaults that are empty stay
    /// empty on purpose (the key endpoints ship blank).
    fn show(&self, st: &crate::settings::Settings) {
        self.populate(st);
        let _ = self.wnd.hwnd().ShowWindow(co::SW::SHOW);
        self.relayout();
        // The font winsafe gave the form, at the DPI it was built at; set here
        // so the combos below are measured in the font they draw with.
        apply_ui_font(self.wnd.hwnd(), system_dpi());
        self.fit_combos();
        self.wnd.hwnd().SetForegroundWindow();
    }

    // Fit the tab control and button row to the real client area.
    // `WindowModelessOpts::size` is the OUTER size, so laying out against it
    // would put the button row under the title bar and off the bottom edge.
    fn relayout(&self) {
        let Ok(rc) = self.wnd.hwnd().GetClientRect() else {
            return;
        };
        // The DPI the form's rows were built at, NOT the window's current DPI:
        // the tab pages inside are fixed at creation, so scaling the chrome to
        // a different figure would leave the two disagreeing. See `Prefs::new`.
        let l = lay::prefs_layout(system_dpi(), rc.right, rc.bottom);
        put(&self.tab, l.tab);
        put(&self.btn_ok, l.btn_ok);
        put(&self.btn_cancel, l.btn_cancel);
    }

    fn populate(&self, st: &crate::settings::Settings) {
        for (k, f) in &self.fields {
            let _ = f.set_text(&st.get(k));
        }
        for (k, c) in &self.checks {
            c.set_check(st.get_bool(k));
        }
        for (k, c) in &self.combos {
            let opts = enum_options(k);
            let want = st.get(k);
            // Map the stored canonical to its menu index. A stored value that
            // matched nothing would leave the combo blank, so fall back to the
            // first row — every combo always shows a value.
            let idx = opts
                .iter()
                .position(|(canon, _)| *canon == want)
                .unwrap_or(0);
            c.items().select(Some(idx as u32));
        }
        // Re-titled from the stored string every time, which is also what makes
        // a language change re-render "Any" in the new interface language.
        for (k, p) in &self.langs {
            p.set(&st.get(k));
        }
        let _ = self.lbl_keydb.hwnd().SetWindowText(&st.keydb_status());
    }

    /// Read every control back into `st` (no save, no close). Shared by OK and
    /// the live language switch so the form-reading rules live in one place.
    fn read_form(&self, st: &mut crate::settings::Settings) {
        for (k, f) in &self.fields {
            // A control that cannot be read leaves the stored value alone, not blanked.
            if let Ok(text) = f.text() {
                st.set(k, text);
            }
        }
        for (k, c) in &self.checks {
            st.set_bool(k, c.is_checked());
        }
        for (k, c) in &self.combos {
            let opts = enum_options(k);
            if let Some(i) = c.items().selected_index()
                && let Some((canon, _)) = opts.get(i as usize)
            {
                // Persist the canonical for the selected row (index-mapped),
                // never the localized label.
                st.set(k, (*canon).to_string());
            }
        }
        // The picker's own string, never its button title: the title is a
        // summary of language NAMES and re-parsing it would mean a second copy
        // of `ui::lang_selection` living in this file.
        for (k, p) in &self.langs {
            st.set(k, p.value());
        }
    }

    fn hide(&self) {
        let _ = self.wnd.hwnd().ShowWindow(co::SW::HIDE);
    }

    /// Size every dropdown to its choices (`lay::combo_widths`): the box grows
    /// to show the longest in full where its row has room, and the open list is
    /// always wide enough for each one, however long the translation.
    fn fit_combos(&self) {
        let dpi = system_dpi();
        let m = lay::form_metrics(dpi);
        let mut base = self.combo_w.borrow_mut();
        if base.is_empty() {
            *base = self
                .combos
                .iter()
                .map(|(_, c)| c.hwnd().GetWindowRect().map_or(0, |r| r.right - r.left))
                .collect();
        }
        for ((k, c), &built) in self.combos.iter().zip(base.iter()) {
            let labels: Vec<String> = enum_options(k).into_iter().map(|(_, l)| l).collect();
            let text_w = combo_text_width(c, &labels);
            let (closed, list) = lay::combo_widths(dpi, built, text_w, m.width - m.gutter);
            // The dropped rectangle's height is the list's; the box's own
            // window rectangle is only the closed field.
            let mut dropped = w::RECT::default();
            let _ = unsafe {
                c.hwnd()
                    .SendMessage(msg::CbGetDroppedControlRect { rect: &mut dropped })
            };
            let _ = c.hwnd().SetWindowPos(
                w::HwndPlace::None,
                w::POINT::new(),
                w::SIZE::with(closed, (dropped.bottom - dropped.top).max(m.field_h)),
                co::SWP::NOMOVE | co::SWP::NOZORDER | co::SWP::NOACTIVATE,
            );
            set_dropped_width(c, list);
        }
    }

    /// Commit the form (`lay::form_commit`): into the stored settings, into the
    /// running `App` so it applies at once, and to disk. The live output folder
    /// follows only when the DEFAULT destination changed, so a one-off folder
    /// pick in the main window survives.
    fn commit(&self, sh: &Shell, why: lay::FormCommit) {
        let before = serde_json::to_value(&*sh.settings.borrow()).ok();
        let mut edited = sh.settings.borrow().clone();
        self.read_form(&mut edited);
        let changed = serde_json::to_value(&edited).ok() != before;
        let plan = lay::form_commit(why, self.wnd.hwnd().IsWindowVisible(), changed);
        if plan.save {
            let old_dest =
                std::mem::replace(&mut *sh.settings.borrow_mut(), edited.clone()).dest_dir;
            let new_dest = edited.dest_dir.clone();
            let dest_changed = new_dest != old_dest && !new_dest.trim().is_empty();
            sh.app_mut(|a| {
                a.settings = edited;
                if dest_changed {
                    a.output_dir = new_dest;
                }
            });
            save_settings_reporting_error(sh);
        }
        if plan.close {
            self.hide();
        }
    }

    // Select a tab programmatically (the screenshot harness needs this).
    // `TCM_SETCURSEL` moves the tab strip but raises no notification, so the
    // page swap winsafe normally does on a click has to be done here too.
    fn select_tab(&self, index: u32) {
        unsafe { self.tab.hwnd().SendMessage(msg::TcmSetCurSel { index }) };
        // The page also has to be POSITIONED and SIZED, not just shown: winsafe
        // only lays out the page that was selected at creation, so every other
        // one is still 0×0 and showing it would display nothing at all.
        let Ok(wr) = self.tab.hwnd().GetWindowRect() else {
            return;
        };
        let Ok(mut rc) = self.wnd.hwnd().ScreenToClientRc(wr) else {
            return;
        };
        unsafe {
            self.tab.hwnd().SendMessage(msg::TcmAdjustRect {
                display_rect: false, // window rect -> the child's ideal rect
                rect: &mut rc,
            });
        }
        for (i, page) in self.pages.iter().enumerate() {
            if i as u32 == index {
                place(
                    page,
                    rc.left,
                    rc.top,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                );
            }
            show(page, i as u32 == index);
        }
    }

    fn set_keydb_note(&self, text: &str) {
        let _ = self.lbl_keydb.hwnd().SetWindowText(text);
    }

    /// Disable the update button while a download is in flight so a second click
    /// cannot spawn a concurrent download.
    fn set_keydb_updating(&self, updating: bool) {
        self.btn_keydb.hwnd().EnableWindow(!updating);
    }
}

// ── About ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct About {
    wnd: gui::WindowModeless,
    btn_site: gui::Button,
    btn_close: gui::Button,
    /// The row LABELS ("Version", "Licence", …) and the website caption, kept
    /// so a live language change can re-text them — see `relocalize`.
    lbl_keys: Vec<gui::Label>,
    /// The row VALUES, kept for the same reason: the keydb status is a
    /// localized sentence, not a constant.
    lbl_vals: Vec<gui::Label>,
}

// The four (label, value) rows the About box shows, in the current locale.
// A function, not an inline literal, because the window is created ONCE and
// reused: `relocalize` needs the same rows again in the new language.
fn about_rows(st: &crate::settings::Settings) -> [(String, String); 4] {
    let g = crate::strings::get;
    [
        (
            g("gui.about.version"),
            format!("{} (Windows)", env!("CARGO_PKG_VERSION")),
        ),
        (
            g("gui.about.engine"),
            format!("libfreemkv {}", libfreemkv::VERSION_LABEL),
        ),
        (g("gui.about.licence"), "MIT".to_string()),
        (g("gui.about.keys"), st.keydb_status()),
    ]
}

impl About {
    fn new(parent: &gui::WindowMain, st: &crate::settings::Settings) -> Self {
        let g = crate::strings::get;
        // As with Settings: built before the main window exists, so the system
        // DPI is the only one on offer, and it is the DPI this form stays at.
        let dpi = system_dpi();
        let s = lay::Scale::new(dpi);
        let (ww, wh) = (s.px(lay::ABOUT_W), s.px(lay::ABOUT_H));
        let wnd = gui::WindowModeless::new(
            parent,
            gui::WindowModelessOpts {
                title: &g("gui.menu.app_about"),
                class_name: "FmkvAbout",
                size: (ww, wh),
                style: co::WS::CAPTION | co::WS::SYSMENU | co::WS::CLIPCHILDREN | co::WS::BORDER,
                ex_style: co::WS_EX::LEFT | co::WS_EX::DLGMODALFRAME,
                ..Default::default()
            },
        );
        let _ = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: "freemkv",
                position: (0, s.px(18)),
                size: (ww, s.px(26)),
                control_style: co::SS::CENTER,
                ..Default::default()
            },
        );
        let mut lbl_keys: Vec<gui::Label> = Vec::new();
        let mut lbl_vals: Vec<gui::Label> = Vec::new();
        let mut y = s.px(62);
        for (k, v) in about_rows(st) {
            lbl_keys.push(gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: &k,
                    position: (s.px(20), y),
                    size: (s.px(130), s.px(18)),
                    control_style: co::SS::RIGHT,
                    ..Default::default()
                },
            ));
            lbl_vals.push(gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: &v,
                    position: (s.px(160), y),
                    size: (ww - s.px(175), s.px(18)),
                    control_style: co::SS::LEFT | co::SS::ENDELLIPSIS,
                    ..Default::default()
                },
            ));
            y += s.px(24);
        }
        // Last key label, with no value beside it: the website button is the
        // value. Kept in the same Vec so `relocalize` walks one list.
        lbl_keys.push(gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &g("gui.about.website"),
                position: (s.px(20), y),
                size: (s.px(130), s.px(18)),
                control_style: co::SS::RIGHT,
                ..Default::default()
            },
        ));
        // A real button rather than styled text: it opens the site in the
        // default browser, so it does what it looks like it does.
        let btn_site = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: SITE_URL,
                position: (s.px(156), y - s.px(3)),
                width: s.px(200),
                height: s.px(24),
                control_style: co::BS::FLAT,
                ..Default::default()
            },
        );
        let btn_close = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &g("gui.btn.close"),
                position: (ww - s.px(110), wh - s.px(46)),
                width: s.px(96),
                height: s.px(28),
                control_style: co::BS::DEFPUSHBUTTON,
                ..Default::default()
            },
        );
        About {
            wnd,
            btn_site,
            btn_close,
            lbl_keys,
            lbl_vals,
        }
    }

    // Re-text everything localized here, for a live language change. Needed because this shell
    // builds its About window ONCE and reuses it (unlike macOS, which drops its cache):
    fn relocalize(&self, st: &crate::settings::Settings) {
        let g = crate::strings::get;
        let _ = self.wnd.hwnd().SetWindowText(&g("gui.menu.app_about"));
        let rows = about_rows(st);
        for (l, (k, _)) in self.lbl_keys.iter().zip(rows.iter()) {
            let _ = l.hwnd().SetWindowText(k);
        }
        // The website caption sits after the four rows.
        if let Some(l) = self.lbl_keys.get(rows.len()) {
            let _ = l.hwnd().SetWindowText(&g("gui.about.website"));
        }
        for (l, (_, v)) in self.lbl_vals.iter().zip(rows.iter()) {
            let _ = l.hwnd().SetWindowText(v);
        }
        let _ = self.btn_close.hwnd().SetWindowText(&g("gui.btn.close"));
    }

    fn show(&self, st: &crate::settings::Settings) {
        // Values are re-read on every open: the keydb row changes with Update,
        // with `keydb_path`, and with its age.
        for (l, (_, v)) in self.lbl_vals.iter().zip(&about_rows(st)) {
            let _ = l.hwnd().SetWindowText(v);
        }
        let _ = self.wnd.hwnd().ShowWindow(co::SW::SHOW);
        self.relayout();
        self.wnd.hwnd().SetForegroundWindow();
    }

    /// Pin Close to the real client area. As with Settings, `size` in
    /// `WindowModelessOpts` is the OUTER size, so a button laid out against it
    /// falls off the bottom edge by the height of the title bar.
    fn relayout(&self) {
        if let Ok(rc) = self.wnd.hwnd().GetClientRect() {
            // The creation DPI, for the reason given in `Prefs::relayout`.
            put(
                &self.btn_close,
                lay::about_close_rect(system_dpi(), rc.right, rc.bottom),
            );
        }
    }

    fn hide(&self) {
        let _ = self.wnd.hwnd().ShowWindow(co::SW::HIDE);
    }

    fn events(&self, shell: &Shell) {
        let me = self.clone();
        self.btn_close.on().bn_clicked(move || {
            me.hide();
            Ok(())
        });
        let sh = shell.clone();
        self.btn_site.on().bn_clicked(move || {
            sh.perform(vec![Effect::OpenUrl(SITE_URL.into())]);
            Ok(())
        });
        // The window's close box hides it rather than destroying it: it is built
        // once and reused, so destroying it would make the next open a
        // use-after-free.
        let me = self.clone();
        self.wnd.on().wm_close(move || {
            me.hide();
            Ok(())
        });
    }
}

// Save `Settings` to disk and tell the operator whether it worked. The ONE call site for this
// policy.
fn save_settings_reporting_error(sh: &Shell) {
    match sh.settings.borrow().save() {
        Ok(()) => sh.app_mut(|a| {
            a.say(
                LogKind::Result,
                &crate::strings::get("gui.log.settings_saved"),
            )
        }),
        Err(e) => sh.app_mut(|a| {
            a.say(
                LogKind::Notice,
                &crate::strings::fmt("gui.log.settings_save_error", &[("e", &e)]),
            )
        }),
    }
}

impl Prefs {
    fn events(&self, shell: &Shell) {
        // OK: commit the form, persist it, and push into the running App so
        // changes take effect at once — App's own copy, loaded at startup,
        // would otherwise stay stale until the next launch.
        let me = self.clone();
        let sh = shell.clone();
        self.btn_ok.on().bn_clicked(move || {
            me.commit(&sh, lay::FormCommit::Ok);
            Ok(())
        });

        // A text field commits on Enter and when focus leaves it, as the GTK
        // shell's do. Enter in a field reaches the window as IDOK: the message
        // loop's `IsDialogMessage` turns it into the dialog's default command.
        for (_, f) in &self.fields {
            let me = self.clone();
            let sh = shell.clone();
            f.on().en_kill_focus(move || {
                me.commit(&sh, lay::FormCommit::FocusLost);
                Ok(())
            });
        }
        let me = self.clone();
        let sh = shell.clone();
        self.wnd
            .on()
            .wm_command(co::DLGID::OK.raw(), co::BN::CLICKED, move || {
                let focus = w::HWND::GetFocus();
                if me
                    .fields
                    .iter()
                    .any(|(_, f)| focus.as_ref() == Some(f.hwnd()))
                {
                    me.commit(&sh, lay::FormCommit::Enter);
                }
                Ok(())
            });

        let me = self.clone();
        self.btn_cancel.on().bn_clicked(move || {
            me.hide();
            Ok(())
        });
        let me = self.clone();
        self.wnd.on().wm_close(move || {
            me.hide();
            Ok(())
        });
        let me = self.clone();
        self.wnd.on().wm_size(move |_| {
            me.relayout();
            Ok(())
        });

        // The browse buttons fill THIS field: dest_dir picks a folder,
        // keydb_path picks a file.
        let me = self.clone();
        let sh = shell.clone();
        self.btn_browse_dest.on().bn_clicked(move || {
            if let Some(d) = sh.pick(true, &crate::strings::get("gui.panel.output_msg"), false) {
                me.set_field("dest_dir", &d);
            }
            Ok(())
        });
        let me = self.clone();
        let sh = shell.clone();
        self.btn_browse_keydb.on().bn_clicked(move || {
            // Any file type — a keydb is not a source media type.
            if let Some(p) = sh.pick(false, &crate::strings::get("gui.set.keydb_path"), false) {
                me.set_field("keydb_path", &p);
            }
            Ok(())
        });

        // Validated by the same rule the key layer uses, so the UI cannot accept
        // a URL the engine would later reject.
        let me = self.clone();
        let sh = shell.clone();
        self.btn_test.on().bn_clicked(move || {
            let url = me.field_value("keyserver_url");
            if url.trim().is_empty() {
                sh.app_mut(|a| {
                    a.say(
                        LogKind::Result,
                        &crate::strings::get("gui.log.no_keyserver"),
                    )
                });
                return Ok(());
            }
            match freemkv_keysources::validate_keyserver_url(&url) {
                Ok(_) => sh.app_mut(|a| {
                    a.say(
                        LogKind::Result,
                        &crate::strings::fmt("gui.log.keyserver_valid", &[("url", &url)]),
                    )
                }),
                Err(e) => sh.app_mut(|a| {
                    a.say(
                        LogKind::Notice,
                        &crate::strings::fmt(
                            "gui.log.keyserver_rejected",
                            &[("e", &e.to_string())],
                        ),
                    )
                }),
            }
            Ok(())
        });

        // Update keydb: reads the LIVE field values, not the last-saved ones, so
        // Update works before OK is pressed.
        let me = self.clone();
        let sh = shell.clone();
        self.btn_keydb.on().bn_clicked(move || {
            let mut url = me.field_value("keydb_url");
            let mut path = me.field_value("keydb_path");
            if url.is_empty() {
                url = sh.settings.borrow().keydb_url.clone();
            }
            if path.is_empty() {
                path = sh.settings.borrow().keydb_path.clone();
            }
            if url.trim().is_empty() {
                // Nothing to fetch — say so in place rather than silently
                // spawning a thread that errors into the (maybe-hidden) log.
                me.set_keydb_note(&crate::strings::get("gui.set.keydb_no_url"));
                return Ok(());
            }
            sh.app_mut(|a| {
                a.say(
                    LogKind::Result,
                    &crate::strings::get("gui.log.fetching_keydb"),
                )
            });
            // Immediate in-Settings feedback: the download is ~20 MB and takes a
            // few seconds; the drain updates this note to the result when done.
            me.set_keydb_note(&crate::strings::get("gui.set.keydb_updating"));
            me.set_keydb_updating(true);
            let inbox = sh.inbox.clone();
            std::thread::spawn(move || {
                // A panic in `update_keydb` must NOT strand the drain: catch it so
                // a terminal message is pushed either way. Dropping it leaves Update
                // disabled and TIMER_DRAIN firing forever (`drain()` stops on batch).
                let msg = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    match crate::settings::update_keydb(&url, &path) {
                        Ok(m) => (LogKind::Result, m),
                        // A failure is a Notice, never logged in the success style.
                        Err(e) => (LogKind::Notice, e),
                    }
                }))
                .unwrap_or_else(|_| {
                    (
                        LogKind::Notice,
                        crate::strings::get_or(
                            "gui.log.keydb_worker_failed",
                            "keydb update failed — internal error",
                        ),
                    )
                });
                // RECOVER rather than skip the push (see macOS's identical worker).
                inbox.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
            });
            sh.start_drain();
            Ok(())
        });

        // Each language button opens its own checklist. Nothing is committed
        // here — the picker holds the edit and OK reads it back with the rest
        // of the form, so Cancel discards it like any other change.
        for (_, picker) in &self.langs {
            let picker = picker.clone();
            let me = self.clone();
            let btn = picker.btn.clone();
            #[allow(clippy::redundant_clone)]
            btn.on().bn_clicked(move || {
                picker.popup(me.wnd.hwnd());
                Ok(())
            });
        }

        // The interface-language dropdown applies the moment a language is
        // picked: commit the form, swap the catalog, and re-text every window.
        let me = self.clone();
        let sh = shell.clone();
        if let Some((_, combo)) = self.combos.iter().find(|(k, _)| *k == "language") {
            let combo = combo.clone();
            #[allow(clippy::redundant_clone)]
            combo.on().cbn_sel_change(move || {
                me.read_form(&mut sh.settings.borrow_mut());
                save_settings_reporting_error(&sh);
                let code = sh.settings.borrow().language.clone();
                crate::strings::set_locale(crate::ui::locale_code(&code));
                sh.relocalize();
                Ok(())
            });
        }
    }

    fn set_field(&self, key: &str, val: &str) {
        for (k, f) in &self.fields {
            if *k == key {
                let _ = f.set_text(val);
            }
        }
    }

    fn field_value(&self, key: &str) -> String {
        self.fields
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, f)| f.text().unwrap_or_default())
            .unwrap_or_default()
    }

    // Re-text every label, tab and combo after a language change. macOS
    // rebuilds its windows instead; winsafe cannot create controls after a
    // window exists, so every new control must be added here too, not just `new`.
    fn relocalize(&self, st: &crate::settings::Settings) {
        let g = crate::strings::get;
        let _ = self.wnd.hwnd().SetWindowText(&g("gui.win.settings"));
        for (i, key) in [
            "gui.tab.output",
            "gui.tab.selection",
            "gui.tab.recovery",
            "gui.tab.keys",
            "gui.tab.advanced",
        ]
        .iter()
        .enumerate()
        {
            let _ = self.tab.items().get(i as u32).set_text(&g(key));
        }
        // Enum combos show localized labels; rebuild them, then re-select from
        // the canonical value so the pick survives the language change.
        for (k, c) in &self.combos {
            let labels: Vec<String> = enum_options(k).into_iter().map(|(_, l)| l).collect();
            c.items().delete_all();
            let _ = c.items().add(&labels);
        }
        self.populate(st);
        let _ = self.btn_ok.hwnd().SetWindowText(&g("gui.btn.ok"));
        let _ = self.btn_cancel.hwnd().SetWindowText(&g("gui.btn.cancel"));
        let _ = self
            .btn_keydb
            .hwnd()
            .SetWindowText(&g("gui.set.update_keydb"));
        let _ = self
            .btn_test
            .hwnd()
            .SetWindowText(&g("gui.set.test_connection"));
        self.fit_combos();
    }
}

impl Shell {
    // Apply a language change live (caller already swapped the catalog via
    // `strings::set_locale`). The menu bar is genuinely rebuilt (`HMENU` can
    // be replaced post-creation); everything else is re-texted in place.
    fn relocalize(&self) {
        let g = crate::strings::get;
        // Grab the menu the window currently owns BEFORE replacing it: `SetMenu`
        // does not free the old `HMENU`, so it must be destroyed by hand or it
        // leaks on every language change.
        let old_bar = self.wnd.hwnd().GetMenu();
        if let Ok(bar) = build_menu(&win_menus()) {
            let _ = self.wnd.hwnd().SetMenu(&bar);
            let _ = self.wnd.hwnd().DrawMenuBar();
            if let Some(mut old) = old_bar {
                let _ = old.DestroyMenu();
            }
        }
        let _ = self
            .lbl_empty_head
            .hwnd()
            .SetWindowText(&g("gui.page.empty_title"));
        let _ = self
            .lbl_empty_sub
            .hwnd()
            .SetWindowText(&g("gui.page.empty_subtitle"));
        let _ = self
            .btn_open_disc
            .hwnd()
            .SetWindowText(&g("gui.btn.open_disc"));
        let _ = self.btn_open.hwnd().SetWindowText(&g("gui.btn.open_file"));
        let _ = self.lbl_out.hwnd().SetWindowText(&g("gui.group.output"));
        for (l, text) in self.lbl_pick.iter().zip(crate::ui::pick_labels()) {
            let _ = l.hwnd().SetWindowText(&format!("{text}:"));
        }
        let _ = self
            .grp_prog
            .hwnd()
            .SetWindowText(&g("gui.group.information"));
        let _ = self.btn_browse.hwnd().SetWindowText(&g("gui.btn.browse"));
        let _ = self.btn_run.hwnd().SetWindowText(&g("gui.btn.run_now"));
        let _ = self.btn_eject.hwnd().SetWindowText(&g("gui.menu.eject"));
        let _ = self.btn_cancel.hwnd().SetWindowText(&g("gui.btn.cancel"));
        let _ = self
            .btn_reveal
            .hwnd()
            .SetWindowText(&g("gui.btn.show_explorer"));
        let _ = self.btn_done.hwnd().SetWindowText(&g("gui.btn.done"));
        // The Information row labels are owned by the core, so they relocalize
        // from the same single source both shells use.
        for (l, text) in self.lbl_keys.iter().zip(crate::ui::InfoRows::labels()) {
            let _ = l.hwnd().SetWindowText(&text);
        }
        self.sync_tree_head();
        self.apply_tree_direction();
        // Force the format dropdown, the tree, the selection bar and the free-space
        // line to repaint in the new language.
        self.memo.borrow_mut().formats.clear();
        self.memo.borrow_mut().rows = None;
        self.memo.borrow_mut().pick = None;
        self.memo.borrow_mut().free_line = None;
        // The rebuilt menu bar starts all-enabled.
        self.memo.borrow_mut().menu_running = None;
        self.prefs.relocalize(&self.settings.borrow());
        // The About box too: it is built once and cached, so nothing else ever
        // re-texts it.
        self.about.relocalize(&self.settings.borrow());
        self.render();
    }
}

// Self-screenshot: `PrintWindow` renders into a memory DC (Win32's counterpart
// to `cacheDisplayInRect:`); BMP output (no encoder dep), unlike macOS's PNG.

// True when every byte is identical: capture "succeeded" but is a blank plate.
fn is_blank(buf: &[u8]) -> bool {
    buf.first()
        .is_some_and(|first| buf.iter().all(|b| b == first))
}

fn snapshot(hwnd: &w::HWND, path: &str) -> w::AnyResult<()> {
    let (cx, cy, buf) = capture(hwnd)?;
    let size = buf.len();
    let mut bi = w::BITMAPINFO::default();
    bi.bmiHeader.biWidth = cx;
    bi.bmiHeader.biHeight = cy;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = co::BI::RGB;

    let mut bfh = w::BITMAPFILEHEADER::default();
    bfh.bfOffBits = (std::mem::size_of::<w::BITMAPFILEHEADER>()
        + std::mem::size_of::<w::BITMAPINFOHEADER>()) as u32;
    bfh.bfSize = bfh.bfOffBits + size as u32;

    let mut out = Vec::with_capacity(size + 64);
    out.extend_from_slice(bfh.serialize());
    out.extend_from_slice(bi.bmiHeader.serialize());
    out.extend_from_slice(&buf);
    std::fs::write(path, &out)?;
    Ok(())
}

// A window's pixels as `(width, height, 32-bit BGRA rows bottom-up)`, the way a DIB holds them.
fn capture(hwnd: &w::HWND) -> w::AnyResult<(i32, i32, Vec<u8>)> {
    let rc = hwnd.GetWindowRect()?;
    let (cx, cy) = ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1));

    let screen_dc = w::HWND::DESKTOP.GetDC()?;
    let hbmp = screen_dc.CreateCompatibleBitmap(cx, cy)?;
    let mem_dc = screen_dc.CreateCompatibleDC()?;

    let stride = (cx * 32 + 31) / 32 * 4;
    let size = (stride * cy) as usize;
    let mut buf = vec![0u8; size];

    // Three ways to get pixels, tried in order until one is not blank.
    // PW_RENDERFULLCONTENT needs DWM composition (fails silently in CI's
    // headless session-0); plain PrintWindow works there; screen blit is last resort.
    let mut last_err: Option<co::ERROR> = None;
    for strategy in 0..3u8 {
        {
            let _sel = mem_dc.SelectObject(&*hbmp)?;
            let rendered = match strategy {
                0 => unsafe {
                    extra::PrintWindow(hwnd.ptr(), mem_dc.ptr(), extra::PW_RENDERFULLCONTENT)
                },
                1 => unsafe { extra::PrintWindow(hwnd.ptr(), mem_dc.ptr(), 0) },
                _ => {
                    mem_dc.BitBlt(
                        w::POINT::new(),
                        w::SIZE::with(cx, cy),
                        &screen_dc,
                        w::POINT::with(rc.left, rc.top),
                        co::ROP::SRCCOPY,
                    )?;
                    1
                }
            };
            if dev_env("FMKV_SHOT_DEBUG").is_ok() {
                println!("  snapshot: {cx}x{cy} strategy={strategy} rendered={rendered}");
            }
        }
        // A FRESH header per attempt: GetDIBits writes back into the one it is
        // given (biSizeImage, biClrUsed …), and handing it a header it has
        // already filled in makes the next call fail with "invalid handle".
        let mut bi = w::BITMAPINFO::default();
        bi.bmiHeader.biWidth = cx;
        bi.bmiHeader.biHeight = cy;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = co::BI::RGB;
        match unsafe {
            screen_dc.GetDIBits(
                &hbmp,
                0,
                cy as u32,
                Some(&mut buf),
                &mut bi,
                co::DIB::RGB_COLORS,
            )
        } {
            // Treat a read failure as "blank" and try the next strategy rather
            // than giving up on the capture entirely.
            Err(e) => {
                if dev_env("FMKV_SHOT_DEBUG").is_ok() {
                    println!("  snapshot: strategy={strategy} GetDIBits failed: {e}");
                }
                last_err = Some(e);
            }
            Ok(_) if !is_blank(&buf) => {
                last_err = None;
                break;
            }
            Ok(_) => {
                if dev_env("FMKV_SHOT_DEBUG").is_ok() {
                    println!("  snapshot: strategy={strategy} produced a blank image");
                }
            }
        }
    }
    if let Some(e) = last_err {
        return Err(Box::new(e));
    }
    Ok((cx, cy, buf))
}

// Let Windows actually lay out and paint before capturing. Controls create
// and draw lazily during the message loop, so capturing immediately yields
// an empty tree — the same lesson the macOS harness records.
fn pump(ms: u64) {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    while std::time::Instant::now() < until {
        let mut msg = w::MSG::default();
        while w::PeekMessage(&mut msg, None, 0, 0, co::PM::REMOVE) {
            w::TranslateMessage(&msg);
            unsafe {
                w::DispatchMessage(&msg);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

// UI driver: drives the REAL controls (BM_CLICK, real WM_COMMANDs) so every
// action goes through the same wiring a mouse click takes — calling handlers
// directly would bypass exactly the wiring that broke on macOS twice.

impl Shell {
    /// Click a button as a user would: `trigger_click` posts `BM_CLICK`.
    fn drive_click(&self, b: &gui::Button) -> bool {
        if !b.hwnd().IsWindowEnabled() {
            return false;
        }
        b.trigger_click();
        pump(60);
        true
    }

    /// Invoke a menu item through its own command route, respecting enablement
    /// exactly as Windows would.
    fn drive_menu(&self, id: u16) -> bool {
        let Some(bar) = self.wnd.hwnd().GetMenu() else {
            return false;
        };
        let Ok(state) = bar.GetMenuState(w::IdPos::Id(id)) else {
            return false;
        };
        if state.has(co::MF::GRAYED) || state.has(co::MF::DISABLED) {
            return false;
        }
        self.wnd.hwnd().SendCommand(w::AccelMenuCtrl::Menu(id));
        pump(60);
        true
    }

    // The state-image index the TREE CONTROL is actually showing for a row.
    // Read back from the widget, not the model: the missing-checkbox bug on
    // macOS lived exactly here — `View` was right, the cell was wrong.
    fn widget_state(&self, row: usize) -> Option<u32> {
        fn find<'a>(
            it: impl Iterator<Item = w::gui::TreeViewItem<'a, usize>>,
            row: usize,
        ) -> Option<w::HTREEITEM> {
            for item in it {
                if *item.data().borrow() == row {
                    return Some(unsafe { item.htreeitem().raw_copy() });
                }
                if let Some(found) = find(item.iter_children(), row) {
                    return Some(found);
                }
            }
            None
        }
        let h = find(self.tree.items().iter_root(), row)?;
        let st = unsafe {
            self.tree.hwnd().SendMessage(msg::TvmGetItemState {
                hitem: &h,
                mask: co::TVIS::STATEIMAGEMASK,
            })
        };
        Some((st & co::TVIS::STATEIMAGEMASK).raw() >> 12)
    }

    /// Toggle a row exactly as the tick-box click handler does.
    fn drive_toggle_row(&self, row: usize) {
        self.app_mut(|a| a.tree.toggle(row));
    }

    // Choose an output format in the REAL dropdown and fire the same handler
    // selection fires. Programmatic selection doesn't raise `CBN_SELCHANGE`;
    // without this the driver only proves appearance changed, not behavior.
    fn drive_pick_format(&self, canonical: &str) -> bool {
        let label = crate::ui::format_label(canonical);
        let titles = self.combo_titles();
        let Some(i) = titles.iter().position(|t| *t == label) else {
            return false;
        };
        self.cmb_format.items().select(Some(i as u32));
        self.on_format_pick();
        pump(30);
        true
    }

    fn combo_titles(&self) -> Vec<String> {
        combo_items(&self.cmb_format)
    }

    fn drive_open(&self, path: &str) {
        let fx = self.app_mut(|a| a.open(path));
        self.perform(fx);
        pump(60);
    }

    fn drive_log(&self) -> String {
        self.log.text().unwrap_or_default()
    }

    fn drive_set_output(&self, path: &str) {
        let _ = self.edit_out.set_text(path);
        self.app.borrow_mut().output_dir = path.to_string();
    }
}

// The WIDGET-level assertions: what controls actually show vs what the core's `View` said they
// should.
#[cfg(any(test, debug_assertions))]
impl Shell {
    fn widget_checks(&self) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        let mut check = |name: &str, ok: bool, detail: String| {
            out.push((ok, format!("{name} — {detail}")));
        };
        let v = self.app.borrow().view();

        // ── the tree shows one item per row the core decided on ──
        check(
            "widget-tree-populated",
            self.tree.items().count() as usize == v.title_rows.len(),
            format!(
                "{} tree items vs {} rows",
                self.tree.items().count(),
                v.title_rows.len()
            ),
        );

        // ── the tree's header: the tick column, then the core's columns ──
        let heads: Vec<String> = self
            .tree_head
            .items()
            .iter()
            .map(|it| it.map(|h| h.text()).collect())
            .unwrap_or_default();
        let columns = crate::ui::tree_columns();
        let want: Vec<String> = std::iter::once(String::new())
            .chain(columns.iter().map(|c| c.title.clone()))
            .collect();
        check(
            "widget-tree-header-columns",
            heads == want,
            format!(
                "header items {heads:?}, syncs {}, formats {:?}",
                TREE_HEAD_SYNCS.load(std::sync::atomic::Ordering::Relaxed),
                (0..heads.len() as u32)
                    .map(|i| self.tree_head.items().get(i).format().raw())
                    .collect::<Vec<_>>()
            ),
        );
        let cells = &self.cols.borrow().cells;
        check(
            "widget-tree-cells-match-the-core",
            v.title_rows
                .iter()
                .all(|r| cells.get(&r.index) == Some(&row_cells(r, &columns))),
            format!("{} rows carry cells", cells.len()),
        );

        // ── the selection bar shows the core's choices ──
        if let Some(pick) = &v.pick {
            let want: Vec<String> = pick.titles.iter().map(|(_, l)| l.clone()).collect();
            let got = combo_items(&self.cmb_pick_titles);
            let at = self.cmb_pick_titles.items().selected_index();
            check(
                "widget-pick-titles-match-the-core",
                got == want && at == Some(pick.title_index() as u32),
                format!("combo shows {got:?} at {at:?}, core offers {want:?}"),
            );
            let audio = self
                .btn_pick_audio
                .hwnd()
                .GetWindowText()
                .unwrap_or_default();
            let subs = self
                .btn_pick_subs
                .hwnd()
                .GetWindowText()
                .unwrap_or_default();
            check(
                "widget-pick-menus-show-the-core-summary",
                audio == pick.audio_summary && subs == pick.subs_summary,
                format!("audio {audio:?}, subtitles {subs:?}"),
            );
        }

        // Every row's state image matches the core's tick, checked against the
        // whole `state_for` mapping: a row with no checkbox must show none, and
        // a Mixed row must show the third glyph, not fall back to checked/unchecked.
        for r in &v.title_rows {
            let want = state_for_row(r);
            let got = self.widget_state(r.index);
            check(
                "widget-row-state-matches-the-core",
                got == Some(want),
                format!(
                    "row {} ({} {:?}): widget shows {got:?}, core says {want}",
                    r.index, r.type_s, r.check
                ),
            );
        }

        // Format combo shows exactly the core's list, localized. Not "the combo
        // is non-empty" — the CONTENT is the property that catches an ISO sink
        // still on offer for an MKV source.
        let want: Vec<String> = v
            .formats
            .iter()
            .flat_map(|g| g.iter().map(|s| crate::ui::format_label(s)))
            .collect();
        let got = self.combo_titles();
        check(
            "widget-format-combo-matches-the-core",
            got == want,
            format!("combo shows {got:?}, core offers {want:?}"),
        );
        let selected: Option<String> = self
            .cmb_format
            .items()
            .selected_index()
            .and_then(|i| self.combo_titles().into_iter().nth(i as usize));
        let want_label = crate::ui::format_label(&v.format);
        check(
            "widget-format-combo-selects-the-current-format",
            selected.as_deref() == Some(want_label.as_str()),
            format!("combo shows {selected:?}, model holds {:?}", v.format),
        );

        // ── Run Now's enablement is the model's, in the widget ──
        check(
            "widget-run-button-follows-can-run",
            self.btn_run.hwnd().IsWindowEnabled() == v.can_run,
            format!(
                "button enabled = {}, View::can_run = {}",
                self.btn_run.hwnd().IsWindowEnabled(),
                v.can_run
            ),
        );

        // ── the log pane shows exactly the text the shell decided on ──
        check(
            "widget-log-shows-the-rendered-lines",
            self.drive_log() == log_text(&v.log),
            format!("log pane holds {:?}", self.drive_log()),
        );
        // An ordinary tick APPENDS (EM_REPLACESEL at the end), not a rebuild:
        // drive that path on the real EDIT control and compare again.
        let shown = self.memo.borrow().log;
        let plan = log_plan(shown, v.log_first, v.log.len() + 2);
        self.app_mut(|a| {
            a.say(LogKind::Detail, "widget check: appended line");
            a.say(LogKind::Notice, "widget check: appended notice");
        });
        let after = self.app.borrow().view();
        check(
            "widget-log-append-matches-the-rendered-lines",
            matches!(plan, LogPlan::Append(_)) && self.drive_log() == log_text(&after.log),
            format!("plan {plan:?}; log pane holds {:?}", self.drive_log()),
        );
        check(
            "widget-log-is-readonly-and-selectable",
            self.log.hwnd().style().has(co::WS::TABSTOP),
            "log text can be focused, selected and copied".to_string(),
        );

        // ── every menu command is present and routes to a core command ──
        let bar = self.wnd.hwnd().GetMenu();
        check(
            "widget-menu-bar",
            bar.as_ref().and_then(|m| m.GetMenuItemCount().ok()) == Some(4),
            format!(
                "{:?} top-level menus (File/Edit/View/Help)",
                bar.as_ref().and_then(|m| m.GetMenuItemCount().ok())
            ),
        );
        let missing: Vec<u16> = match &bar {
            Some(m) => MENU_CMD_IDS
                .iter()
                .copied()
                .filter(|id| m.GetMenuState(w::IdPos::Id(*id)).is_err())
                .collect(),
            None => MENU_CMD_IDS.to_vec(),
        };
        check(
            "widget-every-command-id-is-on-the-menu",
            missing.is_empty(),
            format!("ids present in MENU_CMD_IDS but absent from the bar: {missing:?}"),
        );

        // ── Settings has every tab the shell builds ──
        check(
            "widget-settings-tabs",
            self.prefs.tab.items().count().unwrap_or(0) == 5,
            format!("{} tabs", self.prefs.tab.items().count().unwrap_or(0)),
        );

        out
    }
}

/// Scripted end-user test. Drives the REAL controls and asserts against both the
/// core's `View` and the widgets, so it validates the shell and the model
/// together. Debug builds only.
#[cfg(debug_assertions)]
impl Shell {
    fn self_test(&self, iso: &str, mkv: &str, shot_dir: &str) -> bool {
        let mut results: Vec<(bool, String)> = Vec::new();
        let mut check = |name: &str, ok: bool, detail: &str| {
            results.push((ok, format!("{name} — {detail}")));
        };
        let snap = |n: &str| {
            pump(350);
            let _ = snapshot(self.wnd.hwnd(), &format!("{shot_dir}/{n}.bmp"));
        };
        let view = || self.app.borrow().view();

        // 1 ── empty at launch
        check(
            "empty-at-launch",
            view().page == Page::Empty,
            "no source shows the empty page",
        );
        snap("01-empty");

        // 2 ── open a disc image
        self.drive_open(iso);
        let v = view();
        let titles = v.title_rows.iter().filter(|x| x.type_s == "Title").count();
        check("open-iso", titles > 0, &format!("{titles} titles"));
        check(
            "open-shows-tree",
            v.page == Page::Titles,
            "tree replaces the empty page",
        );
        // The log is the user's only window into what happened, and it is
        // rendered by the SHELL — asserting on the model would not catch a log
        // pane that never received the text.
        let text = self.drive_log();
        check(
            "log-names-the-disc",
            text.contains("opened") && text.contains("title(s)"),
            &format!("log first line: {:?}", text.lines().next().unwrap_or("")),
        );
        check(
            "log-reports-key-state",
            text.contains("keys:"),
            "log carries the key-resolution result",
        );
        check(
            "log-never-claims-an-unearned-key",
            !text.contains("resolved-online"),
            "no placeholder key origin reaches the user",
        );
        check(
            "titles-numbered",
            crate::ui::titles_numbered(&v.title_rows),
            "1-based, matches -t N",
        );
        check(
            "root-not-checkable",
            v.title_rows[0].check.is_none(),
            "the disc root is not a choice",
        );
        check(
            "video-not-checkable",
            v.title_rows
                .iter()
                .filter(|x| x.type_s == "Video")
                .all(|x| x.check.is_none()),
            "video is implicit",
        );
        check("info-text", !v.detail.is_empty(), "detail pane populated");

        // WIDGET-level checks: what controls actually show, not the model — the
        // same sweep also runs as an ordinary `#[test]`, so `cargo test` catches
        // regressions too. (`check`'s borrow of `results` ends above the append.)
        results.extend(self.widget_checks());
        let mut check = |name: &str, ok: bool, detail: &str| {
            results.push((ok, format!("{name} — {detail}")));
        };
        snap("02-titles");

        // ── driving the real tick box changes the model AND the widget
        if let Some(ai) = v.title_rows.iter().position(|x| x.type_s == "Audio") {
            let before_widget = self.widget_state(ai);
            let before = *self.app.borrow().tree.arena[ai].checked.borrow();
            self.drive_toggle_row(ai);
            let after = *self.app.borrow().tree.arena[ai].checked.borrow();
            check(
                "toggle-row-model",
                after != before,
                "ticking a stream row changed the model",
            );
            check(
                "toggle-row-widget",
                self.widget_state(ai) != before_widget,
                &format!(
                    "state image went {:?} -> {:?}",
                    before_widget,
                    self.widget_state(ai)
                ),
            );
            snap("03-checkbox-clicked");
            self.drive_toggle_row(ai);
        }

        // ── REAL MENU ITEMS, invoked through their own command route
        check(
            "menu-select-all",
            self.drive_menu(IDM_SELECT_ALL)
                && self.app.borrow().tree.ticked_titles().len() == titles,
            "Edit ▸ Select All Titles ticked everything",
        );
        check(
            "menu-select-none",
            self.drive_menu(IDM_SELECT_NONE) && self.app.borrow().tree.ticked_titles().is_empty(),
            "Edit ▸ Select No Titles cleared it",
        );
        check(
            "menu-invert",
            self.drive_menu(IDM_INVERT) && self.app.borrow().tree.ticked_titles().len() == titles,
            "Edit ▸ Invert Title Selection flipped it",
        );
        check(
            "menu-clear-log",
            self.drive_menu(IDM_CLEAR_LOG) && view().log.is_empty(),
            "View ▸ Clear log emptied it",
        );
        check(
            "menu-show-log",
            self.drive_menu(IDM_TOGGLE_LOG) && view().log_hidden,
            "View ▸ Show log hid it",
        );
        self.drive_menu(IDM_TOGGLE_LOG);
        check(
            "menu-close",
            self.drive_menu(IDM_CLOSE) && view().page == Page::Empty,
            "File ▸ Close returned to the empty page",
        );
        snap("04-after-close");
        self.drive_open(iso);

        // 3 ── tri-state after a partial stream selection
        let ti = self
            .app
            .borrow()
            .tree
            .arena
            .iter()
            .position(|n| n.type_s == "Title" && !n.children.is_empty());
        if let Some(t) = ti {
            self.app_mut(|a| a.tree.set_checked(t, true));
            let all_on = self.app.borrow().tree.check_state(t) == Check::On;
            let kid = self.app.borrow().tree.arena[t]
                .children
                .iter()
                .copied()
                .find(|&c| self.app.borrow().tree.arena[c].checkable());
            if let Some(c) = kid {
                self.app_mut(|a| *a.tree.arena[c].checked.borrow_mut() = false);
                let st = self.app.borrow().tree.check_state(t);
                check(
                    "tri-state-model",
                    all_on && st != Check::On,
                    "partial reads as mixed",
                );
                // And the widget must actually SHOW the third glyph — this is
                // invisible to any assertion on `View`.
                check(
                    "tri-state-widget",
                    self.widget_state(t) == Some(ST_MIXED),
                    &format!("title state image = {:?}", self.widget_state(t)),
                );
                snap("05-tri-state");
            }
            self.app_mut(|a| a.tree.set_checked(t, true));
        }

        // 4 ── output formats follow the source kind, at the WIDGET level
        check(
            "disc-formats",
            view().formats.concat().join("|").contains("Whole disc"),
            "disc offers image sinks",
        );
        let disc_titles = self.combo_titles().join("|");
        check(
            "disc-combo-offers-iso",
            disc_titles.contains(&crate::ui::format_label("Whole disc → ISO image")),
            "a disc source can be backed up whole",
        );
        for (canon, want) in [
            ("Selected titles → M2TS", "M2TS"),
            ("Selected titles → MKV", "MKV"),
        ] {
            let picked = self.drive_pick_format(canon);
            check(
                &format!("pick-format-{want}"),
                picked && view().format == canon,
                &format!("model now reads {:?}", view().format),
            );
        }
        // The fixture is a DVD (MPEG-2), which MP4 cannot hold — so the option
        // must be ABSENT from the real dropdown, not merely refused.
        check(
            "mp4-absent-for-mpeg2",
            !self.drive_pick_format("Selected titles → MP4"),
            "MP4 is not offered for a source it cannot store",
        );
        if !mkv.is_empty() {
            self.drive_open(mkv);
            check(
                "container-formats",
                !view().formats.concat().join("|").contains("Whole disc"),
                "container hides them",
            );
            let cont = self.combo_titles().join("|");
            check(
                "container-combo-drops-iso",
                !cont.contains(&crate::ui::format_label("Whole disc → ISO image"))
                    && cont.contains(&crate::ui::format_label("Selected titles → MKV")),
                &format!("combo now offers: {cont}"),
            );
            snap("06-container");
            self.drive_open(iso);
            check(
                "combo-restores-for-a-disc",
                self.combo_titles()
                    .join("|")
                    .contains(&crate::ui::format_label("Whole disc → ISO image")),
                "reopening a disc brings the whole-disc sinks back",
            );
        }

        // 5 ── the log commands
        check("log-content", !view().log.is_empty(), "carries real events");
        self.act(Cmd::ClearLog);
        check("log-clear", view().log.is_empty(), "Clear log empties it");
        self.act(Cmd::ToggleLog);
        let hid = view().log_hidden;
        self.act(Cmd::ToggleLog);
        check(
            "log-toggle",
            hid && !view().log_hidden,
            "hides and restores",
        );

        // 6 ── the guard rule comes from the core
        check(
            "blocked-while-running",
            crate::ui::blocked_while_running(Cmd::Run)
                && !crate::ui::blocked_while_running(Cmd::Cancel),
            "Cancel always reachable, Run is not",
        );

        // 7 ── settings persist, and the window has every tab
        check(
            "settings-load",
            !crate::settings::Settings::load().dest_dir.is_empty(),
            "destination restored from disk",
        );
        self.prefs.show(&self.settings.borrow());
        pump(250);
        let _ = snapshot(
            self.prefs.wnd.hwnd(),
            &format!("{shot_dir}/10-settings.bmp"),
        );
        self.prefs.hide();
        self.about.show(&self.settings.borrow());
        pump(250);
        let _ = snapshot(self.about.wnd.hwnd(), &format!("{shot_dir}/11-about.bmp"));
        self.about.hide();

        // 8 ── layout at the extremes, at this window's real DPI
        let dpi = window_dpi(self.wnd.hwnd());
        let big = lay::Scale::new(dpi).px(1700);
        for (cw, ch) in [
            lay::min_size(dpi),
            (big, big * 1050 / 1700),
            lay::default_size(dpi),
        ] {
            self.relayout(cw, ch);
        }
        check("resize", true, "min, large and default");
        snap("07-resized");

        // 9 ── REAL RUN: click Run Now, watch it start, click Cancel
        self.act(Cmd::SelectNone);
        // Resolve the index in its OWN statement (as step 3 does): an `if let
        // Some(t) = self.app.borrow()…` keeps the `Ref` alive across `app_mut`,
        // whose `borrow_mut` then panics and aborts the process outright.
        let first_title = self
            .app
            .borrow()
            .tree
            .arena
            .iter()
            .position(|n| n.type_s == "Title");
        if let Some(t) = first_title {
            self.app_mut(|a| a.tree.set_checked(t, true));
        }
        self.drive_set_output(shot_dir);
        let started = self.drive_click(&self.btn_run);
        std::thread::sleep(std::time::Duration::from_millis(400));
        let fx = self.app_mut(|a| a.tick());
        self.perform(fx);
        check(
            "click-run",
            started && view().page == Page::Progress,
            "clicking Run Now started a job and showed progress",
        );
        check(
            "output-file-is-a-file",
            view().info.as_ref().is_some_and(|i| i[4].ends_with(".mkv")),
            &format!(
                "Output file row reads '{}'",
                view()
                    .info
                    .as_ref()
                    .map(|i| i[4].clone())
                    .unwrap_or_default()
            ),
        );
        snap("08-running");
        check(
            "run-disabled-while-running",
            !view().can_run && !self.btn_run.hwnd().IsWindowEnabled(),
            "Run Now is disabled during a rip, in the widget as well as the model",
        );
        check(
            "menu-blocked-while-running",
            !self.drive_menu(IDM_START_RIP),
            "File ▸ Start rip is refused mid-run",
        );
        let cancelled = self.drive_click(&self.btn_cancel);
        for _ in 0..25 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let fx = self.app_mut(|a| a.tick());
            self.perform(fx);
            if view().page == Page::Result {
                break;
            }
        }
        check(
            "click-cancel",
            cancelled && view().page == Page::Result,
            "Cancel stopped the job and showed the result",
        );
        check(
            "cancel-heading-honest",
            view().result_heading != crate::strings::get("gui.result.finished"),
            &format!("result heading reads '{}'", view().result_heading),
        );
        snap("09-cancelled");
        let done = self.drive_click(&self.btn_done);
        check(
            "click-done",
            done && view().page != Page::Result,
            "Done dismissed the result page",
        );

        // 10 ── engine-facing guards
        check(
            "bad-source",
            crate::engine::scan("C:\\Windows\\win.ini").is_err(),
            "rejected, no panic",
        );
        check(
            "preflight",
            crate::engine::preflight(iso, shot_dir, &[]).is_ok(),
            "answers without executing",
        );
        let ks = crate::settings::Settings::load().keydb_status();
        check(
            "keydb-status",
            ks.contains("keydb found") || ks.contains("no keydb"),
            &ks,
        );

        let passed = results.iter().filter(|(ok, _)| *ok).count();
        for (ok, msg) in &results {
            println!("  {} {}", if *ok { "PASS" } else { "FAIL" }, msg);
        }
        println!("\n{passed}/{} checks passed", results.len());
        passed == results.len()
    }
}

// ── run ───────────────────────────────────────────────────────────────────

/// One-shot timer used to run a development harness *inside* the message loop,
/// which is the only place the window is real and paintable.
const TIMER_HARNESS: usize = 3;

pub fn run() {
    // The gate image has no console, and a panic inside a window procedure aborts the
    // process; the hook leaves the message where the driver prints it.
    if let Ok(dir) = dev_env("FMKV_GATE") {
        std::panic::set_hook(Box::new(move |info| {
            let _ = std::fs::create_dir_all(&dir);
            let bt = std::backtrace::Backtrace::force_capture();
            let _ = std::fs::write(format!("{dir}/panic.txt"), format!("{info}\n{bt}"));
        }));
    }
    // The file dialogs are COM objects, so the apartment must exist for the
    // lifetime of the app. The guard uninitializes on drop.
    let _com = w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE);
    if let Err(e) = &_com {
        tracing::error!("COM could not be initialised; the file dialogs will not open: {e:?}");
    }
    // Before any window exists, so the taskbar and toasts share one identity.
    let _ = w::SetCurrentProcessExplicitAppUserModelID(APP_ID);

    let shell = Shell::new();
    shell.events();

    // Development harness (screenshot / page / self-test hooks). Debug builds
    // only — the shipped release binary has no environment switches. It runs off
    // a one-shot timer so the window is fully created and painted first.
    #[cfg(debug_assertions)]
    {
        let me = shell.clone();
        shell.wnd.on().wm_timer(TIMER_HARNESS, move || {
            let _ = me.wnd.hwnd().KillTimer(TIMER_HARNESS);
            if me.dev_harness() {
                w::PostQuitMessage(0);
            }
            Ok(())
        });
        let me = shell.clone();
        shell.wnd.on().wm_show_window(move |_| {
            if dev_env("FMKV_SELFTEST").is_ok()
                || dev_env("FMKV_SHOT").is_ok()
                || dev_env("FMKV_WIN").is_ok()
                || dev_env("FMKV_DUMP_MENUS").is_ok()
                || dev_env("FMKV_GATE").is_ok()
            {
                let _ = me.wnd.hwnd().SetTimer(TIMER_HARNESS, 400, None);
            }
            Ok(())
        });
    }

    if let Err(e) = shell.wnd.run_main(None) {
        // Never die silently: a GUI that vanishes with no message is the worst
        // possible failure mode.
        let _ = w::HWND::NULL.MessageBox(&e.to_string(), "freemkv", co::MB::ICONERROR);
    }
}

#[cfg(debug_assertions)]
impl Shell {
    /// Returns true when the harness handled the invocation and the app should
    /// quit rather than hand control to the user.
    fn dev_harness(&self) -> bool {
        // FMKV_OPEN=<path> opens a source before anything else is captured,
        // and waits up to a minute for its scan so the capture shows the tree.
        if let Ok(src) = dev_env("FMKV_OPEN") {
            self.drive_open(&src);
            for _ in 0..300 {
                if self.app.borrow().view().page == Page::Titles {
                    break;
                }
                pump(200);
            }
        }

        // FMKV_GATE=<dir>: qa's GUI gate over the source FMKV_OPEN opened. This image
        // has no console, so the report goes to `<dir>/gate.txt` and the verdict
        // to the exit code.
        if let Ok(dir) = dev_env("FMKV_GATE") {
            let _ = std::fs::create_dir_all(&dir);
            let gate = self.gate(&dir);
            let _ = std::fs::write(format!("{dir}/gate.txt"), gate.report());
            std::process::exit(if gate.passed() { 0 } else { 1 });
        }

        // FMKV_SIZE=WxH resizes before snapshotting, to test resize behaviour.
        if let Ok(sz) = dev_env("FMKV_SIZE")
            && let Some((ws, hs)) = sz.split_once('x')
            && let (Ok(nw), Ok(nh)) = (ws.parse::<i32>(), hs.parse::<i32>())
        {
            let _ = self.wnd.hwnd().SetWindowPos(
                w::HwndPlace::None,
                w::POINT::new(),
                w::SIZE::with(nw, nh),
                co::SWP::NOMOVE | co::SWP::NOZORDER,
            );
            self.relayout(nw, nh);
        }

        // FMKV_DUMP_MENUS prints the menu tree, so a review can see every
        // command and accelerator without clicking through them.
        if dev_env("FMKV_DUMP_MENUS").is_ok() {
            if let Some(bar) = self.wnd.hwnd().GetMenu() {
                let n = bar.GetMenuItemCount().unwrap_or(0);
                for i in 0..n {
                    let title = bar
                        .GetMenuString(w::IdPos::Pos(i))
                        .unwrap_or_else(|_| String::new());
                    println!("MENU  {title}");
                    if let Some(sub) = bar.GetSubMenu(i) {
                        let m = sub.GetMenuItemCount().unwrap_or(0);
                        for j in 0..m {
                            match sub.GetMenuString(w::IdPos::Pos(j)) {
                                Ok(s) if s.is_empty() => println!("    ----"),
                                Ok(s) => println!("    {s}"),
                                Err(_) => println!("    ----"),
                            }
                        }
                    }
                }
            }
            return true;
        }

        // FMKV_SELFTEST="<iso>|<mkv>|<shotdir>" drives every control.
        if let Ok(spec) = dev_env("FMKV_SELFTEST") {
            let mut it = spec.split('|');
            let iso = it.next().unwrap_or("").to_string();
            let mkv = it.next().unwrap_or("").to_string();
            let dir = it.next().unwrap_or("C:\\Temp").to_string();
            let _ = std::fs::create_dir_all(&dir);
            pump(400);
            let ok = self.self_test(&iso, &mkv, &dir);
            std::process::exit(if ok { 0 } else { 1 });
        }

        // FMKV_PAGE=empty|progress|result forces a page for a capture.
        if let Ok(page) = dev_env("FMKV_PAGE") {
            match page.as_str() {
                "progress" => {
                    self.app_mut(|a| a.page = Page::Progress);
                }
                "result" => {
                    self.app_mut(|a| {
                        a.result_summary = "2 title(s) written".into();
                        a.page = Page::Result;
                    });
                }
                _ => self.app_mut(|a| a.page = Page::Empty),
            }
            self.render();
            // AFTER render: render writes the bars and the Information rows from
            // the View, so sample values set before it would be wiped.
            if page == "progress" {
                let pct: f64 = dev_env("FMKV_PCT")
                    .ok()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(100.0);
                self.bar_cur.set_position(pct.round() as u32);
                self.bar_all.set_position(100);
                let sample = [
                    "D:\\media\\iso\\Movie.iso",
                    "Movie.iso",
                    &crate::ui::fmt_bytes(6_743_590_912),
                    &format!("{}/s", crate::ui::fmt_bytes(41_400_000)),
                    "C:\\Users\\me\\Videos\\Movie_t1.mkv",
                    &crate::ui::fmt_bytes(4_312_000_000),
                    &crate::ui::fmt_bytes(243_000_000_000),
                ];
                for (l, v) in self.lbl_vals.iter().zip(sample) {
                    let _ = l.hwnd().SetWindowText(v);
                }
                let _ =
                    self.lbl_cur
                        .hwnd()
                        .SetWindowText(&crate::ui::bar_caption(pct, 75, Some(42)));
            }
        }

        // FMKV_WIN=prefs|about captures a secondary window instead.
        if let Ok(which) = dev_env("FMKV_WIN") {
            let hwnd = if which == "prefs" {
                self.prefs.show(&self.settings.borrow());
                if let Ok(tab) = dev_env("FMKV_TAB")
                    && let Ok(i) = tab.parse::<u32>()
                {
                    self.prefs.select_tab(i);
                }
                unsafe { self.prefs.wnd.hwnd().raw_copy() }
            } else {
                self.about.show(&self.settings.borrow());
                unsafe { self.about.wnd.hwnd().raw_copy() }
            };
            pump(600);
            if let Ok(path) = dev_env("FMKV_SHOT") {
                let _ = snapshot(&hwnd, &path);
                println!("wrote {path}");
            }
            return true;
        }

        if let Ok(path) = dev_env("FMKV_SHOT") {
            self.relayout_now();
            pump(800);
            match snapshot(self.wnd.hwnd(), &path) {
                Ok(()) => println!("wrote {path}"),
                Err(e) => println!("snapshot failed: {e}"),
            }
            return true;
        }

        false
    }
}

// qa's GUI gate (`crate::gui_gate`): the title tree's columns and the Settings fields and
// dropdowns, measured in the fonts they draw with and checked for ink in a capture of the
// real window, and captures of each for review.
#[cfg(debug_assertions)]
impl Shell {
    fn gate(&self, dir: &str) -> crate::gui_gate::Gate {
        use crate::gui_gate::{Gate, Region, has_ink};
        let mut g = Gate::default();
        self.relayout_now();
        pump(600);
        let s = lay::Scale::new(window_dpi(self.wnd.hwnd()));
        let pad = s.px(6);
        let v = self.app.borrow().view();
        let titles: Vec<&Row> = v.title_rows.iter().filter(|r| r.depth == 1).collect();
        g.check(
            "fixture-opens",
            v.page == Page::Titles && titles.len() >= 2,
            format!("page {:?}, {} titles", v.page, titles.len()),
        );
        g.check(
            "titles-carry-length-and-size",
            !titles.is_empty()
                && titles
                    .iter()
                    .all(|r| !r.length.is_empty() && !r.size.is_empty()),
            format!(
                "{:?}",
                titles
                    .iter()
                    .map(|r| (&r.length, &r.size))
                    .collect::<Vec<_>>()
            ),
        );

        // ── the header and the cells ──
        let heads: Vec<String> = self
            .tree_head
            .items()
            .iter()
            .map(|it| it.map(|h| h.text()).collect())
            .unwrap_or_default();
        let columns = crate::ui::tree_columns();
        let want: Vec<String> = std::iter::once(String::new())
            .chain(columns.iter().map(|c| c.title.clone()))
            .collect();
        g.check("header-texts", heads == want, format!("{heads:?}"));
        let laid = self.cols.borrow().laid.clone();
        g.check("columns-laid-out", laid.is_some(), format!("{laid:?}"));
        // The numeric columns (Length, Size) must read whole; the text columns
        // end in an ellipsis when a cell outruns them, as on macOS.
        let length = columns.iter().position(|c| c.id == "length");
        if let Some(laid) = &laid {
            let head_w = self.tree_head.hwnd().GetClientRect().map_or(0, |r| r.right);
            let widths = lay::header_widths(laid, head_w, self.cols.borrow().inset);
            for (i, text) in want.iter().enumerate().skip(1) {
                let tw = text_width(self.tree_head.hwnd(), text);
                g.check(
                    "header-text-fits",
                    tw + s.px(12) <= widths[i],
                    format!("{text:?}: {tw} px in a {} px header item", widths[i]),
                );
            }
            for r in &titles {
                for (span, c) in laid.cols.iter().zip(&columns).filter(|(_, c)| c.numeric) {
                    let text = r.cell(c.id);
                    let tw = text_width(self.tree.hwnd(), text);
                    g.check(
                        "cell-fits-its-column",
                        tw + pad * 2 <= span.w,
                        format!("{text:?}: {tw} px in a {} px column", span.w),
                    );
                }
            }
        }

        // ── pixels: the header and the first title's Length cell carry ink ──
        match capture(self.wnd.hwnd()) {
            Ok((cx, cy, px)) => {
                let origin = self.wnd.hwnd().GetWindowRect().unwrap_or_default();
                let region = |rc: w::RECT| Region {
                    x: rc.left.min(rc.right) - origin.left,
                    y: rc.top.min(rc.bottom) - origin.top,
                    w: (rc.right - rc.left).abs(),
                    h: (rc.bottom - rc.top).abs(),
                };
                let head = self.tree_head.hwnd().GetWindowRect().unwrap_or_default();
                g.check(
                    "header-has-ink",
                    has_ink(&px, cx, cy, true, region(head)),
                    format!("{:?}", region(head)),
                );
                let span = laid.as_ref().zip(length).and_then(|(l, i)| l.cols.get(i));
                let cell = titles
                    .first()
                    .and_then(|r| self.tree_item(r.index))
                    .zip(span)
                    .and_then(|(h, span)| {
                        let rc = self.item_rect(&h, false)?;
                        let rc = w::RECT {
                            left: span.x,
                            right: span.x + span.w,
                            ..rc
                        };
                        self.tree.hwnd().ClientToScreenRc(rc).ok()
                    });
                g.check(
                    "length-cell-has-ink",
                    cell.is_some_and(|rc| has_ink(&px, cx, cy, true, region(rc))),
                    format!("{:?}", cell.map(region)),
                );
            }
            Err(e) => g.check("capture-main-window", false, e.to_string()),
        }
        let rtl =
            crate::app_entry::resolved_locale(&self.settings.borrow().language, system_locale_code)
                .is_some_and(|t| lay::is_rtl_locale(&t));
        let mirrored = self.tree.hwnd().GetWindowLongPtr(co::GWLP::EXSTYLE)
            & co::WS_EX::LAYOUTRTL.raw() as isize
            != 0;
        g.check(
            "tree-direction-follows-the-language",
            mirrored == rtl,
            format!("mirrored {mirrored}, right-to-left language {rtl}"),
        );
        let shot = format!("{dir}/titles.bmp");
        g.check(
            "capture-titles",
            snapshot(self.wnd.hwnd(), &shot).is_ok(),
            &shot,
        );

        // ── Settings: a long path and every dropdown's choice read whole ──
        self.settings.borrow_mut().dest_dir = crate::gui_gate::LONG_DEST.into();
        self.prefs.show(&self.settings.borrow());
        pump(600);
        if let Some((_, f)) = self.prefs.fields.iter().find(|(k, _)| *k == "dest_dir") {
            let mut rc = w::RECT::default();
            let _ = unsafe { f.hwnd().SendMessage(msg::EmGetRect { rect: &mut rc }) };
            let tw = text_width(f.hwnd(), crate::gui_gate::LONG_DEST);
            g.check(
                "long-destination-reads-whole",
                f.text().unwrap_or_default() == crate::gui_gate::LONG_DEST
                    && tw <= rc.right - rc.left,
                format!("{tw} px of path in a {} px field", rc.right - rc.left),
            );
        }
        for (k, c) in &self.prefs.combos {
            let mut info = w::COMBOBOXINFO::default();
            let _ = unsafe {
                c.hwnd()
                    .SendMessage(msg::CbGetComboBoxInfo { data: &mut info })
            };
            let shown = c.items().selected_text().ok().flatten().unwrap_or_default();
            let tw = text_width(c.hwnd(), &shown);
            let box_w = info.rcItem.right - info.rcItem.left;
            g.check(
                "dropdown-choice-reads-whole",
                !shown.is_empty() && tw <= box_w,
                format!("{k}: {shown:?} is {tw} px in a {box_w} px box"),
            );
            let labels: Vec<String> = enum_options(k).into_iter().map(|(_, l)| l).collect();
            let longest = combo_text_width(c, &labels);
            let list =
                unsafe { c.hwnd().SendMessage(msg::CbGetDroppedWidth {}) }.unwrap_or(0) as i32;
            g.check(
                "dropdown-list-shows-every-choice",
                longest < list,
                format!("{k}: longest choice {longest} px, list {list} px"),
            );
        }
        for (tab, key, name) in [
            (0, "container", "settings-output"),
            (3, "key_source", "settings-keys"),
            (4, "language", "settings-advanced"),
        ] {
            self.prefs.select_tab(tab);
            pump(400);
            let shot = format!("{dir}/{name}.bmp");
            g.check(
                "capture-settings",
                snapshot(self.prefs.wnd.hwnd(), &shot).is_ok(),
                &shot,
            );
            let Some((_, c)) = self.prefs.combos.iter().find(|(k, _)| *k == key) else {
                continue;
            };
            unsafe { c.hwnd().SendMessage(msg::CbShowDropDown { show: true }) };
            pump(400);
            let mut info = w::COMBOBOXINFO::default();
            let _ = unsafe {
                c.hwnd()
                    .SendMessage(msg::CbGetComboBoxInfo { data: &mut info })
            };
            let shot = format!("{dir}/{name}-list.bmp");
            g.check(
                "open-dropdown-has-ink",
                capture(&info.hwndList).is_ok_and(|(cx, cy, px)| {
                    has_ink(
                        &px,
                        cx,
                        cy,
                        true,
                        Region {
                            x: 0,
                            y: 0,
                            w: cx,
                            h: cy,
                        },
                    )
                }) && snapshot(&info.hwndList, &shot).is_ok(),
                format!("{key}: {shot}"),
            );
            unsafe { c.hwnd().SendMessage(msg::CbShowDropDown { show: false }) };
        }
        self.prefs.hide();
        g
    }

    // The tree item showing core row `row`.
    fn tree_item(&self, row: usize) -> Option<w::HTREEITEM> {
        fn find<'a>(
            it: impl Iterator<Item = w::gui::TreeViewItem<'a, usize>>,
            row: usize,
        ) -> Option<w::HTREEITEM> {
            for item in it {
                if *item.data().borrow() == row {
                    return Some(unsafe { item.htreeitem().raw_copy() });
                }
                if let Some(found) = find(item.iter_children(), row) {
                    return Some(found);
                }
            }
            None
        }
        find(self.tree.items().iter_root(), row)
    }
}

// How wide `text` draws in a control's own font, in pixels.
fn text_width(hwnd: &w::HWND, text: &str) -> i32 {
    let Ok(dc) = hwnd.GetDC() else {
        return 0;
    };
    let font = unsafe { hwnd.SendMessage(msg::WmGetFont {}) };
    let _font = font.as_ref().and_then(|f| dc.SelectObject(f).ok());
    dc.GetTextExtentPoint32(text).map_or(0, |sz| sz.cx)
}

// Tests, two tiers, both via `cargo test` on Windows: pure shell decisions (no
// window/message loop) and `widget_checks` (the FMKV_SELFTEST sweep, driven
// against a real window). App/Tree/View behavior lives in tests/gui_model.rs.

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
