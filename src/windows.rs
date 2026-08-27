//! Windows Win32 shell — the second shell over the shared `ui`/`engine`/
//! `settings` core.
//!
//! ## The contract — three steps, no fourth
//!
//! ```text
//! 1. render   App::view() -> View     assign strings/flags to widgets
//! 2. dispatch App::dispatch(cmd)      on any click, menu pick or keystroke
//! 3. perform  the returned Effects    the platform-only actions
//! ```
//!
//! **The invariant:** if a change to `ui.rs` would need mirroring here, the
//! split is wrong. Fix the split, do not mirror. This shell writes NO logic:
//! every decision — which formats exist, which rows carry a checkbox, what the
//! progress caption says, whether Run is enabled — comes from `ui.rs`, so a bug
//! fixed on macOS is fixed here at the same time.
//!
//! The shell also holds no state that duplicates `App`. On macOS six such
//! fields existed and one of them went stale, silently disabling the
//! menu-disabling logic during a rip. The only cached values here are *render
//! memos* (`rows`, `formats`, `log_len`): a signature of what was last painted,
//! used purely to avoid rebuilding a control that has not changed. Rebuilding a
//! tree or a dropdown unconditionally on a 100 ms tick would flicker and drop
//! the user's expansion state mid-rip.
//!
//! ## Toolkit
//!
//! `winsafe` (safe Win32) with stock common controls, so the scrollbars, focus
//! rings, keyboard navigation, high-contrast mode and screen-reader support are
//! all the real Windows ones rather than a drawn imitation. The
//! Common-Controls v6 manifest in `res/freemkv.manifest` (embedded by
//! `build.rs`) is what makes them render themed rather than as Windows-95 grey
//! boxes.
//!
//! ## Windows conventions this deliberately does NOT copy from macOS
//!
//! * Menus mount **in-window** (`SetMenu`) — there is no global menu bar.
//! * **Settings lives under File**, not an app menu: Windows has no app menu.
//! * Accelerators are **Ctrl**, not Cmd; `Alt+F4` quits; `F1` opens the docs.
//! * "Show in Finder" becomes "Show in Explorer".

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use winsafe::{self as w, co, gui, msg, prelude::*};

use crate::ui::{App, Check, Cmd, Effect, LogKind, LogLine, Page, Row, View};

// Window geometry: rects come from `win_layout` (DPI-aware, PerMonitorV2);
// never position from a bare constant or it'll be wrong at non-100% scaling.
// Proportions mirror the macOS shell (tree 46.4% wide, etc.).

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
const ID_DETAIL: u16 = 1002;
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

/// First command id of a language checklist popup; entry *i* is `+ i`.
///
/// Well clear of the `IDM_*` block even though it cannot collide with it: the
/// checklist is tracked with `TPM::RETURNCMD`, which hands the chosen id back
/// as the call's return value and posts no `WM_COMMAND` at all. A reader
/// checking "does 3000 mean two things?" should not have to work that out.
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

/// Map a menu id to a core command.
///
/// The RULE about what is available mid-rip lives in the core
/// (`ui::blocked_while_running`); this shell only says which id means which
/// command, so macOS and Windows cannot disagree about it.
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

/// Whether the launch probe (a disc already in the drive at startup) should
/// run at all.
///
/// Off under `cargo test`: the in-process harness window builds the real shell,
/// and a unit test must never enumerate — let alone spin up — real hardware.
/// Off in every debug harness mode too: those already load a fixture or are
/// mid-screenshot, and a probe would fight them for the page.
fn launch_probe_enabled() -> bool {
    !cfg!(test)
        && [
            "FMKV_OPEN",
            "FMKV_SELFTEST",
            "FMKV_SHOT",
            "FMKV_WIN",
            "FMKV_DUMP_MENUS",
            "FMKV_PAGE",
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
}

// DPI: every physical length in this shell comes from one of these three calls.
// `win_layout` does the arithmetic; this is the only place that asks Windows
// what the DPI *is*.

/// The DPI of the monitor `hwnd` is currently on.
///
/// `GetDpiForWindow` answers 0 for a window that does not exist yet, which
/// happens for real: `WM_GETMINMAXINFO` is delivered during `CreateWindowEx`,
/// before the handle is stored. The system DPI is the right answer then.
#[must_use]
fn window_dpi(hwnd: &w::HWND) -> u32 {
    match hwnd.GetDpiForWindow() {
        0 => system_dpi(),
        d => d,
    }
}

/// The primary monitor's DPI — the only figure available before any window
/// exists, which is when the shell has to pick its initial size and build the
/// Settings and About forms.
#[must_use]
fn system_dpi() -> u32 {
    match unsafe { extra::GetDpiForSystem() } {
        0 => lay::BASE_DPI,
        d => d,
    }
}

thread_local! {
    /// The UI font per DPI, kept alive for the life of the GUI thread.
    ///
    /// A control keeps only a borrowed `HFONT`; deleting the object while it is
    /// still selected paints garbage. Caching also means dragging back and
    /// forth between two monitors creates two fonts, not one per crossing.
    static UI_FONTS: RefCell<Vec<(u32, w::guard::DeleteObjectGuard<w::HFONT>)>> =
        const { RefCell::new(Vec::new()) };
}

/// The shell UI font at `dpi`.
///
/// winsafe builds its own global UI font exactly once, from the plain
/// `SystemParametersInfo` — that is, at the DPI of the primary monitor — and
/// sends it to each control as the control is created. Under PerMonitorV2 that
/// font is right only on the monitor the process started on, and never changes
/// again. A layout that scales under a font that does not is arguably worse
/// than neither, so the shell creates its own.
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

/// Put the DPI-correct UI font on a window and every control under it.
///
/// The tree view is included deliberately: winsafe sets no font on it at all,
/// so it inherits the stock system font, which is a fixed-size relic that does
/// not scale with anything.
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

/// Attach the embedded application icon to the window itself.
///
/// The executable's icon resource and the *window's* icon are two separate
/// mechanisms: the resource is what Explorer shows for the file, while the
/// title bar, Alt-Tab and the taskbar read the window's own `WM_SETICON` pair.
/// Setting only the class icon leaves the title bar with a blurry downscale,
/// because winsafe derives `hIconSm` from `LoadIcon`, which ignores the small
/// size and always hands back the 32 px frame.
///
/// So load each slot explicitly with `LoadImage` at the size Windows actually
/// wants, which makes it select the matching frame out of the icon group — the
/// purpose-drawn 16 px artwork for the title bar, the full-detail 32 px one for
/// Alt-Tab. Sizes come from `GetSystemMetricsForDpi` at the window's own DPI
/// rather than `GetSystemMetrics`: the manifest declares PerMonitorV2, so on a
/// 200% display the small-icon metric is 32, not 16, and the un-scaled call
/// would pick a frame half the size the title bar is about to draw.
///
/// Best-effort throughout: a missing or unreadable icon costs the default
/// Windows glyph, which is never worth failing a launch over.
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

/// Fill the tree's STATE image list with unchecked / checked / mixed glyphs.
///
/// `TVS_CHECKBOXES` gives only two states, so a title whose streams are partly
/// ticked could not be drawn — and getting that wrong is invisible until a user
/// unticks one audio track. The fix is a state image list with a third image,
/// selected per row through `TVIS_STATEIMAGEMASK`.
///
/// The glyphs are drawn by the **theme engine** (`DrawThemeBackground` with the
/// real `BUTTON`/checkbox parts), so they are pixel-identical to every other
/// checkbox on the system rather than hand-painted approximations.
///
/// The list is built at the size the OS reports for a small icon **at this
/// window's DPI** (`GetSystemMetricsForDpi`, not `GetSystemMetrics` — the
/// latter always answers for the primary monitor), and rebuilt when the DPI
/// changes. winsafe's `TreeView::image_list` helper cannot be used: it creates
/// its list hard-coded at 16 × 16, which is a half-size tick at 200%.
fn build_check_images<T: 'static>(tree: &gui::TreeView<T>, dpi: u32) -> w::AnyResult<()> {
    let side =
        w::GetSystemMetricsForDpi(co::SM::CXSMICON, dpi).unwrap_or(lay::Scale::new(dpi).px(16));

    // Already the right size for this DPI — nothing to do.
    if let Some(cur) = unsafe {
        tree.hwnd().SendMessage(msg::TvmGetImageList {
            kind: co::TVSIL::STATE,
        })
    } && cur.GetImageCount() >= 4
        && cur.GetIconSize().is_ok_and(|s| s.cx == side)
    {
        return Ok(());
    }

    let mut il = w::HIMAGELIST::Create(w::SIZE::with(side, side), co::ILC::COLOR32, 4, 0)?;

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
    // placeholder occupies it and the real glyphs land on 1, 2 and 3.
    let states = [
        (co::VS::BUTTON_CHECKBOX_UNCHECKEDNORMAL, 0u32),
        (co::VS::BUTTON_CHECKBOX_UNCHECKEDNORMAL, 0u32),
        (co::VS::BUTTON_CHECKBOX_CHECKEDNORMAL, extra::DFCS_CHECKED),
        (
            co::VS::BUTTON_CHECKBOX_MIXEDNORMAL,
            extra::DFCS_BUTTON3STATE | extra::DFCS_CHECKED,
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

/// Signatures of what was last painted.
///
/// NOT a copy of `App` state — purely a "has this changed?" note, so a 200 ms
/// tick does not rebuild the tree (destroying the user's expansion state) or the
/// format dropdown (dismissing it mid-click) when nothing about them moved. The
/// macOS shell does the same for its format popup (`sync_formats`).
#[derive(Default)]
struct Memo {
    rows: String,
    formats: String,
    log_len: usize,
}

/// One row signature: the identity of the row, not its tick state (tick state
/// is applied separately, without a rebuild).
fn rows_sig(rows: &[Row]) -> String {
    rows.iter()
        .map(|r| format!("{}|{}|{}|{}", r.index, r.depth, r.type_s, r.desc))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text one tree row shows.
///
/// `SysTreeView32` has no real multi-column mode, so the macOS outline's Type
/// and Description columns are joined into the single item label. The disc root
/// carries no type worth repeating.
fn row_text(r: &Row) -> String {
    if r.depth == 0 || r.type_s.is_empty() {
        r.desc.clone()
    } else {
        format!("{}   {}", r.type_s, r.desc)
    }
}

// ── the shell ─────────────────────────────────────────────────────────────

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
    grp_out: gui::Button,
    cmb_format: gui::ComboBox,
    edit_out: gui::Edit,
    btn_browse: gui::Button,
    btn_run: gui::Button,
    btn_eject: gui::Button,
    grp_info: gui::Button,
    detail: gui::Edit,

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
    /// Worker threads push user-visible lines here; a main-thread timer drains
    /// it. Win32 windows are owned by the thread that created them, so nothing
    /// else may touch a control.
    inbox: Arc<Mutex<Vec<String>>>,
}

impl Shell {
    fn new() -> Self {
        let settings = crate::settings::Settings::load();

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
            menu: build_menu().unwrap_or(w::HMENU::NULL),
            accel_table: build_accels().ok(),
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
        let tree = gui::TreeView::new(
            &wnd,
            gui::TreeViewOpts {
                size: (s.px(400), s.px(400)),
                // No TVS::CHECKBOXES: that control-owned list is two-state
                // only. The state image list built in `build_check_images`
                // carries the third (mixed) glyph the core can ask for.
                control_style: co::TVS::HASLINES
                    | co::TVS::LINESATROOT
                    | co::TVS::HASBUTTONS
                    | co::TVS::SHOWSELALWAYS
                    | co::TVS::FULLROWSELECT,
                ctrl_id: ID_TREE,
                ..Default::default()
            },
        );
        let grp_out = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.group.output"),
                width: s.px(300),
                height: s.px(lay::OUT_H),
                control_style: co::BS::GROUPBOX,
                window_style: co::WS::CHILD | co::WS::VISIBLE,
                ..Default::default()
            },
        );
        let cmb_format = gui::ComboBox::new(
            &wnd,
            gui::ComboBoxOpts {
                width: s.px(280),
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
                width: s.px(110),
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
        let grp_info = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.group.info"),
                width: s.px(300),
                height: s.px(200),
                control_style: co::BS::GROUPBOX,
                window_style: co::WS::CHILD | co::WS::VISIBLE,
                ..Default::default()
            },
        );
        let detail = gui::Edit::new(
            &wnd,
            gui::EditOpts {
                text: &crate::strings::get("gui.page.detail_default"),
                width: s.px(280),
                height: s.px(160),
                // Read-only but selectable: the detail block is something users
                // paste into bug reports.
                control_style: co::ES::MULTILINE | co::ES::READONLY | co::ES::AUTOVSCROLL,
                window_style: co::WS::CHILD | co::WS::VISIBLE | co::WS::TABSTOP | co::WS::VSCROLL,
                ctrl_id: ID_DETAIL,
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
        let about = About::new(&wnd);

        Self {
            wnd,
            app: Rc::new(RefCell::new(App::new())),
            settings: Rc::new(RefCell::new(settings)),
            lbl_empty_head,
            lbl_empty_sub,
            btn_open_disc,
            btn_open,
            tree,
            grp_out,
            cmb_format,
            edit_out,
            btn_browse,
            btn_run,
            btn_eject,
            grp_info,
            detail,
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
            inbox: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

// ── menus ─────────────────────────────────────────────────────────────────

/// The View ▸ log item's full menu text — the core's state-dependent label
/// (`ui::log_menu_label`) plus this shell's accelerator hint. One function so
/// the build and the live re-title cannot produce different strings, which is
/// what makes the "already right, skip the redraw" comparison reliable.
fn log_menu_text(label: &str) -> String {
    format!("{label}\tCtrl+L")
}

/// Build the in-window menu bar.
///
/// Windows has no global menu bar and no app menu, so **Settings lives under
/// File** and **About under Help** — copying the macOS placement (Settings in an
/// app menu at ⌘,) would put them somewhere a Windows user never looks. The
/// accelerator hints in the labels are Ctrl-based for the same reason.
fn build_menu() -> w::SysResult<w::HMENU> {
    let g = crate::strings::get;

    let file = w::HMENU::CreatePopupMenu()?;
    file.append_item(&[
        w::MenuItem::Entry {
            cmd_id: IDM_OPEN,
            text: &format!("{}\tCtrl+O", g("gui.menu.open")),
        },
        w::MenuItem::Entry {
            cmd_id: IDM_OPEN_DISC,
            text: &format!("{}\tCtrl+D", g("gui.menu.open_disc")),
        },
        w::MenuItem::Entry {
            cmd_id: IDM_CLOSE,
            text: &format!("{}\tCtrl+W", g("gui.menu.close")),
        },
        w::MenuItem::Separator,
        w::MenuItem::Entry {
            cmd_id: IDM_SET_OUTPUT,
            text: &g("gui.menu.set_output"),
        },
        w::MenuItem::Entry {
            cmd_id: IDM_START_RIP,
            text: &format!("{}\tCtrl+R", g("gui.menu.start_rip")),
        },
        w::MenuItem::Separator,
        w::MenuItem::Entry {
            cmd_id: IDM_EJECT,
            text: &format!("{}\tCtrl+E", g("gui.menu.eject")),
        },
        w::MenuItem::Separator,
        // Windows convention: preferences live in File, not an app menu.
        w::MenuItem::Entry {
            cmd_id: IDM_SETTINGS,
            text: &g("gui.menu.settings"),
        },
        w::MenuItem::Separator,
        w::MenuItem::Entry {
            cmd_id: IDM_EXIT,
            text: &format!("{}\tAlt+F4", g("gui.menu.exit")),
        },
    ])?;

    // Edit menu mixes standard text commands (act on the focused control, so
    // Copy works in the log) with tree-selection commands, which deliberately
    // skip Ctrl+A/Ctrl+C so they don't break text selection in the log.
    let edit = w::HMENU::CreatePopupMenu()?;
    edit.append_item(&[
        w::MenuItem::Entry {
            cmd_id: IDM_COPY,
            text: &format!("{}\tCtrl+C", g("gui.menu.copy")),
        },
        w::MenuItem::Entry {
            cmd_id: IDM_SELECT_ALL_TEXT,
            text: &format!("{}\tCtrl+A", g("gui.menu.select_all_text")),
        },
        w::MenuItem::Separator,
        w::MenuItem::Entry {
            cmd_id: IDM_SELECT_ALL,
            text: &g("gui.menu.select_all_titles"),
        },
        w::MenuItem::Entry {
            cmd_id: IDM_SELECT_NONE,
            text: &g("gui.menu.select_no_titles"),
        },
        w::MenuItem::Entry {
            cmd_id: IDM_INVERT,
            text: &g("gui.menu.invert_titles"),
        },
    ])?;

    let view = w::HMENU::CreatePopupMenu()?;
    view.append_item(&[
        w::MenuItem::Entry {
            cmd_id: IDM_TOGGLE_LOG,
            // State-dependent, and re-applied on every `render` — the log
            // starts visible, so the item starts as "Hide log". See
            // `Shell::sync_log_menu_title`.
            text: &log_menu_text(&crate::ui::log_menu_label(false)),
        },
        w::MenuItem::Entry {
            cmd_id: IDM_CLEAR_LOG,
            text: &format!("{}\tCtrl+K", g("gui.menu.clear_log")),
        },
    ])?;

    let help = w::HMENU::CreatePopupMenu()?;
    help.append_item(&[
        w::MenuItem::Entry {
            cmd_id: IDM_DOCS,
            text: &format!("{}\tF1", g("gui.menu.docs")),
        },
        w::MenuItem::Entry {
            cmd_id: IDM_CHECK_UPDATES,
            text: &g("gui.menu.check_updates"),
        },
        w::MenuItem::Separator,
        // Windows convention: About sits at the bottom of Help.
        w::MenuItem::Entry {
            cmd_id: IDM_ABOUT,
            text: &g("gui.menu.app_about"),
        },
    ])?;

    let bar = w::HMENU::CreateMenu()?;
    bar.append_item(&[
        w::MenuItem::Submenu {
            submenu: &file,
            text: &g("gui.menu.file"),
        },
        w::MenuItem::Submenu {
            submenu: &edit,
            text: &g("gui.menu.edit"),
        },
        w::MenuItem::Submenu {
            submenu: &view,
            text: &g("gui.menu.view"),
        },
        w::MenuItem::Submenu {
            submenu: &help,
            text: &g("gui.menu.help"),
        },
    ])?;
    Ok(bar)
}

/// Ctrl-based accelerators, plus F1 for help — the Windows conventions. Alt+F4
/// is handled by the system, so it needs no entry.
fn build_accels() -> w::SysResult<w::guard::DestroyAcceleratorTableGuard> {
    let ctrl = co::ACCELF::CONTROL | co::ACCELF::VIRTKEY;
    let vk = co::ACCELF::VIRTKEY;
    w::HACCEL::CreateAcceleratorTable(&[
        w::ACCEL {
            fVirt: ctrl,
            key: co::VK::CHAR_O,
            cmd: IDM_OPEN,
        },
        w::ACCEL {
            fVirt: ctrl,
            key: co::VK::CHAR_D,
            cmd: IDM_OPEN_DISC,
        },
        w::ACCEL {
            fVirt: ctrl,
            key: co::VK::CHAR_W,
            cmd: IDM_CLOSE,
        },
        w::ACCEL {
            fVirt: ctrl,
            key: co::VK::CHAR_R,
            cmd: IDM_START_RIP,
        },
        w::ACCEL {
            fVirt: ctrl,
            key: co::VK::CHAR_E,
            cmd: IDM_EJECT,
        },
        w::ACCEL {
            fVirt: ctrl,
            key: co::VK::CHAR_L,
            cmd: IDM_TOGGLE_LOG,
        },
        w::ACCEL {
            fVirt: ctrl,
            key: co::VK::CHAR_K,
            cmd: IDM_CLEAR_LOG,
        },
        w::ACCEL {
            fVirt: vk,
            key: co::VK::F1,
            cmd: IDM_DOCS,
        },
    ])
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
    /// Reposition everything for the current client size.
    ///
    /// Single source of geometry truth, so the layout is identical at every
    /// window size — and deliberately the same proportions as the macOS shell,
    /// expressed top-down (Win32 y grows downward, AppKit's grows upward).
    ///
    /// The numbers come from `win_layout::main_layout`, which takes the DPI of
    /// the monitor the window is on. `cw`/`ch` are physical pixels, as `WM_SIZE`
    /// reports them, and so are the rectangles that come back.
    fn relayout(&self, cw: i32, ch: i32) {
        let v = self.app.borrow().view();
        let hidden = v.log_hidden;
        let l = lay::main_layout(
            window_dpi(self.wnd.hwnd()),
            cw,
            ch,
            lay::MainState {
                page: v.page,
                two_bars: v.show_overall_bar,
                log_hidden: hidden,
                info_rows: self.lbl_keys.len(),
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
        put(&self.tree, l.tree);
        put(&self.grp_out, l.grp_out);
        put(&self.edit_out, l.edit_out);
        put(&self.btn_browse, l.btn_browse);
        put(&self.cmb_format, l.cmb_format);
        put(&self.btn_run, l.btn_run);
        put(&self.btn_eject, l.btn_eject);
        put(&self.grp_info, l.grp_info);
        put(&self.detail, l.detail);

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

    /// The two things besides the rectangles that scale: the type, and the
    /// tri-state tick glyphs in the tree's state image list. Called from
    /// `WM_CREATE` and again from every `WM_DPICHANGED`.
    fn apply_dpi_at(&self, dpi: u32) {
        apply_ui_font(self.wnd.hwnd(), dpi);
        let _ = build_check_images(&self.tree, dpi);
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
            let text = row_text(r);
            // A row whose parent is missing is added at the top level rather
            // than dropped: a row the core decided to show must be reachable.
            let parent = parents[i].and_then(|p| handles[p].as_ref());
            let added = match parent {
                None => self
                    .tree
                    .items()
                    .add_root(&text, None, r.index)
                    .ok()
                    .map(|it| unsafe { it.htreeitem().raw_copy() }),
                Some(p) => self
                    .tree
                    .items()
                    .get(p)
                    .add_child(&text, None, r.index)
                    .ok()
                    .map(|it| unsafe { it.htreeitem().raw_copy() }),
            };
            if let Some(h) = &added {
                self.set_row_state(h, state_for(r.check));
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
        // Expanding above leaves the view parked on the LAST title, so scroll the
        // core's nominated row (first ticked) back to top. `ensure_visible` won't
        // do — it scrolls the minimum distance, landing at the bottom edge.
        if let Some(h) = crate::ui::first_visible_row(rows).and_then(|i| handles[i].as_ref()) {
            let _ = unsafe {
                self.tree.hwnd().SendMessage(msg::TvmSelectItem {
                    action: co::TVGN::FIRSTVISIBLE,
                    hitem: h,
                })
            };
        }
        let _ = self.tree.hwnd().InvalidateRect(None, true);
    }

    /// Refresh only the tick states, leaving the rows (and the user's expansion
    /// and selection) untouched. This is what runs on an ordinary redraw.
    fn sync_tree_states(&self, rows: &[Row]) {
        for root in self.tree.items().iter_root() {
            let apply = |it: &w::gui::TreeViewItem<'_, usize>| {
                let idx = *it.data().borrow();
                if let Some(r) = rows.iter().find(|r| r.index == idx) {
                    self.set_row_state(it.htreeitem(), state_for(r.check));
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

    /// Which row the user just clicked the tick box of, if any.
    ///
    /// `nm_click` on a tree view carries no hit information, so the position has
    /// to be resolved by hand. `msg::TvmHitTest` in winsafe 0.0.28 passes a
    /// pointer-to-a-pointer (a crate bug), so the raw message is sent instead.
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

// ── render ────────────────────────────────────────────────────────────────

impl Shell {
    /// Mutate the core model and REPAINT.
    ///
    /// The single choke-point for every state change, so no event handler can
    /// mutate state and forget to redraw (the "I said something but the log
    /// didn't update" class of bug). The mutable borrow is released before
    /// `render`, which takes its own immutable borrow.
    fn app_mut<R>(&self, f: impl FnOnce(&mut App) -> R) -> R {
        let r = f(&mut self.app.borrow_mut());
        self.render();
        r
    }

    /// Ask before quitting mid-rip. `true` means go ahead.
    ///
    /// Shared by the window's X (`WM_CLOSE`) and File > Exit, which used to
    /// disagree: the X asked, cancelled and tore down, while the menu item
    /// went straight to `PostQuitMessage` and skipped all three. One question,
    /// one place — a second copy of this decision beside a call site is the
    /// bug, which this crate has now proved three separate times.
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

    /// Signal the worker to stop, then WAIT (bounded) for it to put its output
    /// down.
    ///
    /// `Cmd::Cancel` only signals: the worker notices at its next boundary and
    /// unwinds the mux, and it is that unwind which closes and finalises the
    /// partial file. Quitting without waiting lets the process exit mid-write,
    /// so what lands on disk is wherever the OS write cursor happened to be
    /// rather than the deliberate "cancelled — partial output kept" artefact
    /// the GUI claims. `mac.rs` was given exactly this wait; this shell was
    /// not. Bounded by `QUIT_GRACE`, so a wedged drive cannot turn quit into a
    /// hang — and expiring is no worse than the behaviour it replaces.
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
        for c in [&self.grp_out, &self.grp_info] {
            show(c, p == Page::Titles);
        }
        show(&self.cmb_format, p == Page::Titles);
        show(&self.edit_out, p == Page::Titles);
        show(&self.detail, p == Page::Titles);
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
        let sig = rows_sig(&v.title_rows);
        let changed = self.memo.borrow().rows != sig;
        if changed {
            self.rebuild_tree(&v.title_rows);
            self.memo.borrow_mut().rows = sig;
        } else {
            self.sync_tree_states(&v.title_rows);
        }
        let _ = self.detail.set_text(&crlf(&v.detail));

        // ── output row ──
        if self.edit_out.text().unwrap_or_default() != v.output_dir {
            let _ = self.edit_out.set_text(&v.output_dir);
        }
        self.btn_run.hwnd().EnableWindow(v.can_run);

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
        // Rewritten only when it grew, so selection survives an ordinary tick.
        if self.memo.borrow().log_len != v.log.len() {
            let text = log_text(&v.log);
            let _ = self.log.set_text(&text);
            // Keep the newest line in view, as the macOS log does.
            let n = text.chars().count() as i32;
            self.log.set_selection(n, n);
            self.memo.borrow_mut().log_len = v.log.len();
        }

        // The View ▸ log item names the action it will PERFORM, so it has to be
        // re-titled whenever the log's visibility changes — it is a toggle, and
        // a toggle that always says "Show log" is wrong half the time.
        self.sync_log_menu_title(&v.log_menu_label);
        self.sync_menu_enabled();
        self.relayout_now();
    }

    /// Grey out everything that must be unavailable while a rip is in flight.
    ///
    /// The RULE comes from the core (`ui::blocked_while_running`), consulted per
    /// id — never a second hardcoded list, which is how macOS and Windows would
    /// drift. Cancel is deliberately never blocked.
    fn sync_menu_enabled(&self) {
        let Some(bar) = self.wnd.hwnd().GetMenu() else {
            return;
        };
        let running = self.app.borrow().running();
        for &id in MENU_CMD_IDS {
            let blocked = cmd_for(id).map(crate::ui::blocked_while_running) == Some(true);
            // EnableMenuItem searches submenus by command id.
            let _ = bar.EnableMenuItem(w::IdPos::Id(id), !(running && blocked));
        }
    }

    /// Apply the core's format list to the dropdown, preserving the current pick
    /// when it survives. Called from `render`, so the shell never holds an
    /// opinion about which formats exist.
    ///
    /// A combo box has no separators, so the core's groups are flattened; the
    /// grouping order is preserved so the ordinary case is still first.
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

/// Win32 EDIT controls need CRLF; a bare LF renders as one run-on line.
fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// The exact text the log pane shows for a set of log lines.
///
/// A plain EDIT control cannot colour individual lines, which is how the macOS
/// shell shows severity. Rather than drop the information, notices carry a
/// one-character gutter so a problem is still visible in a monochrome control.
/// (Per-line colour would need a RichEdit.) Pulled out of `render` so the
/// gutter rule can be checked without a window.
fn log_text(log: &[LogLine]) -> String {
    log.iter()
        .map(|l| match l.kind {
            LogKind::Notice => format!("! {}", l.text),
            LogKind::Detail | LogKind::Result => l.text.clone(),
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

// ── platform effects ──────────────────────────────────────────────────────

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
                    // `explorer /select,"<path>"` opens the containing folder
                    // with the item highlighted — the Explorer equivalent of
                    // macOS's "reveal in Finder".
                    use std::os::windows::process::CommandExt as _;
                    // `raw_arg` bypasses Rust's argument quoting: explorer.exe
                    // needs the literal `/select,"<path>"` form, which normal
                    // escaping would mangle.
                    if let Err(e) = std::process::Command::new("explorer.exe")
                        .raw_arg(format!("/select,\"{p}\""))
                        .spawn()
                    {
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
                    let _ =
                        self.wnd
                            .hwnd()
                            .ShellExecute("open", &u, None, None, co::SW::SHOWNORMAL);
                }
                Effect::ShowSettings => self.prefs.show(&self.settings.borrow()),
                Effect::ShowAbout => self.about.show(),
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
            }
        }
        self.render();
    }

    /// `SetTimer` can fail (Windows caps live timers per process/session). If
    /// the poller never starts, nothing ever observes `RunState.finished`: the
    /// worker thread runs the rip to completion in the background while this
    /// window sits on the Progress page forever, with Cancel and every menu
    /// command still disabled by `blocked_while_running` and no way for the
    /// operator to learn anything is wrong short of a force-quit. A `let _ =`
    /// here is the same "never die silently" gap `run_main`'s own error path
    /// (below) exists to close — so it gets the same fix: a `MessageBox`,
    /// since the timer that would carry a localized in-window notice is
    /// exactly the one that failed to start.
    fn report_timer_failure(wnd: &gui::WindowMain, id: usize, elapse_ms: u32) {
        if let Err(e) = wnd.hwnd().SetTimer(id, elapse_ms, None) {
            let _ = wnd.hwnd().MessageBox(
                &format!(
                    "freemkv could not start its progress timer ({e}). The rip \
                     may still be running, but this window will not update. \
                     Please restart freemkv."
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
            me.open_disc(false);
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
            let fx = me.app_mut(|a| a.tick());
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

        // Selecting a row updates the detail pane.
        let me = self.clone();
        self.tree.on().tvn_sel_changed(move |p| {
            let h = unsafe { p.itemNew.hItem.raw_copy() };
            if h != w::HTREEITEM::NULL {
                let idx = *me.tree.items().get(&h).data().borrow();
                me.app_mut(|a| a.selected_row = Some(idx));
            }
            Ok(())
        });

        self.prefs.events(self);
        self.about.events(self);
    }

    /// Choose an output format from the dropdown's visible text.
    ///
    /// The label is resolved against the core's list rather than trusted, so an
    /// unknown title can never enter the model — and so selection works in every
    /// locale, since the dropdown shows `format_label`, not the canonical string.
    fn on_format_pick(&self) {
        let Ok(Some(label)) = self.cmb_format.items().selected_text() else {
            return;
        };
        let (disc, mp4) = {
            let a = self.app.borrow();
            (!crate::ui::is_container(&a.source), a.mp4_possible())
        };
        if let Some(f) = crate::ui::format_from_label(&label, disc, mp4) {
            self.act(Cmd::SetFormat(f));
        }
    }

    /// Re-title the View ▸ log menu item to whatever the `View` says it should
    /// read now (`ui::log_menu_label` + this shell's accelerator hint).
    ///
    /// The item is located by walking the bar's submenus and matching the
    /// COMMAND ID, then set by position within the submenu it was found in —
    /// not by `IdPos::Id` on the bar (whose by-command recursion into submenus
    /// is not documented for `SetMenuItemInfo`) and not by a hardcoded
    /// position (the menu is rebuilt on a live language change).
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

    /// Open the disc in the drive. The decision (which drive, what to log) is
    /// the core's — see `ui::App::disc_source`; this is only the two-step
    /// sequencing the scan needs.
    ///
    /// Two `app_mut` calls, not one, ON PURPOSE: `app_mut` repaints, so the
    /// "Opening …" line reaches the log pane BEFORE `open` starts the scan,
    /// which blocks the UI thread for as long as the drive takes to spin up
    /// and the keys take to resolve.
    ///
    /// `announce_missing` is false for the launch probe — see `wm_create`.
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
        let msgs: Vec<String> = self
            .inbox
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        if msgs.is_empty() {
            return;
        }
        for m in &msgs {
            self.app_mut(|a| a.say(LogKind::Result, m));
        }
        // Surface the keydb outcome in the Settings note so the user sees the
        // result in place, not only in the (possibly hidden) log.
        if let Some(last) = msgs.last() {
            self.prefs.set_keydb_note(last);
        }
        self.prefs.set_keydb_updating(false);
        let _ = self.wnd.hwnd().KillTimer(TIMER_DRAIN);
    }
}

// ── Settings ──────────────────────────────────────────────────────────────

/// The option table for a settings dropdown: `(canonical, localized_label)`
/// pairs in menu order.
///
/// The canonical value is what persists and what the engine matches on (e.g.
/// `key_source.starts_with("Online")`); the label is what the localized combo
/// shows. So a combo displays translated text but stores a stable, English
/// identifier — the same decoupling the format dropdown uses. An empty result
/// means "not an enum combo".
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

/// A multi-select language picker: a button that shows the chosen languages,
/// and a checkable popup menu behind it.
///
/// **Why a button-plus-menu and not a dropdown.** Win32 has no checked-list
/// combo box. `CBS_DROPDOWNLIST` is single-select, and a listbox with
/// `LBS_MULTIPLESEL` is not a dropdown at all — it would need permanent
/// vertical space for a 38-row list, three times over, on a Settings page that
/// already has none. A button opening a `TrackPopupMenu` of `MF::CHECKED`
/// entries is the native Windows idiom for "tick several from a long list" —
/// it is what the Explorer and Task Manager column choosers do — it costs one
/// row of height like the text box it replaces, and it draws the ticks with
/// the system's own check bitmaps rather than an imitation of them.
///
/// **Why the string is held here.** The button's title is a SUMMARY
/// ("English, German"); recovering codes from it would mean a second parser in
/// this file, and the whole point of `ui::lang_*` is that both shells share one
/// rule. So the stored preference lives alongside the control and the title is
/// only ever written *from* it: the shell renders, `ui` decides.
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

    /// Show the checklist under the button and apply each tick.
    ///
    /// The menu is re-opened after every tick instead of closing on the first.
    /// A Win32 menu normally closes when an item is chosen, but this one is a
    /// multi-select list: closing after one language would make a typical
    /// two-or-three-language preference three separate trips through a 38-entry
    /// menu. Escape or a click outside returns no command and ends the loop, so
    /// it is no harder to dismiss than any other menu.
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
        for (i, (code, name)) in langs.iter().enumerate() {
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
                    w::BmpPtrStr::from_str(name),
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

/// Builds one labelled row per call, walking a y-cursor down a tab page — the
/// right-aligned label / control-to-its-right layout the macOS Settings uses.
///
/// Every step and every control size is DPI-scaled. The `wd` widths the callers
/// pass are 96-DPI baselines, scaled here rather than at each of the twenty-odd
/// call sites — so a row reads the same as it always did and still comes out
/// the right physical size at 150%.
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

    /// Same contract as `field`, but for a secret (the keyserver bearer
    /// token): `ES::PASSWORD` masks keystrokes with the system bullet
    /// character, like every password box in Windows. Without this,
    /// `keyserver_token` sat in a plain edit control — fully legible during
    /// screen-sharing, presenting, or a screen recording, and the macOS shell
    /// has the identical gap in its own plain `NSTextField`.
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

    /// A language checklist row — see [`LangPicker`].
    ///
    /// Deliberately the same `row_step` and `field_h` as `field`: this replaced
    /// three text boxes, and a taller control would have pushed the Selection
    /// page past the bottom of the Settings window (which is what
    /// `win_layout`'s page-height test measures).
    ///
    /// The title is set here rather than left to `populate`, because winsafe
    /// creates its controls with the parent window and `hwnd()` is still null
    /// at this point — a `SetWindowText` now would be silently dropped.
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

    /// A path row: field plus a browse button that fills it.
    fn path(&mut self, text: &str, val: &str, wd: i32) -> (gui::Edit, gui::Button) {
        self.label(text);
        let e = gui::Edit::new(
            self.page,
            gui::EditOpts {
                text: val,
                position: (self.gutter, self.y),
                width: self.s.px(wd - 40),
                height: self.m.field_h,
                ..Default::default()
            },
        );
        let b = gui::Button::new(
            self.page,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.browse"),
                position: (self.gutter + self.s.px(wd - 36), self.y - self.s.px(1)),
                width: self.s.px(34),
                height: self.s.px(24),
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

/// The Settings window: five tabs, every control mapping to something the engine
/// or the key layer actually consumes.
///
/// Anything that could not reach real code was left out rather than shown as a
/// switch that does nothing — a control that lies is worse than no control.
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
        let (f_dest, btn_browse_dest) = r.path(&g("gui.set.default_dest"), &st.dest_dir, 320);
        fields.push(("dest_dir", f_dest));
        fields.push((
            "filename_template",
            r.field(&g("gui.set.filename_template"), &st.filename_template, 240),
        ));
        r.gap();
        checks.push(("keep_iso", r.check(&g("gui.set.keep_iso"))));
        checks.push(("auto_eject", r.check(&g("gui.set.auto_eject"))));

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
        r.note(&g("gui.set.capture_note"));

        // ── Keys ── keydb + the online key service
        let mut r = Rows::new(&pages[3], dpi);
        combos.push((
            "key_source",
            r.combo("key_source", &g("gui.set.key_source"), 260),
        ));
        r.gap();
        let (f_keydb, btn_browse_keydb) = r.path(&g("gui.set.keydb_path"), &st.keydb_path, 320);
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
        self.wnd.hwnd().SetForegroundWindow();
    }

    /// Fit the tab control and the button row to the real client area.
    ///
    /// `WindowModelessOpts::size` is the OUTER window size, so laying the button
    /// row out against it puts it under the title bar and off the bottom edge.
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
            st.set(k, f.text().unwrap_or_default());
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

    /// Select a tab programmatically (the screenshot harness needs this).
    ///
    /// `TCM_SETCURSEL` moves the tab strip but raises no notification, so the
    /// page swap that winsafe normally does on a click has to be done here too.
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

/// The four (label, value) rows the About box shows, in the current locale.
///
/// A function, not a literal built inline, because the window is created ONCE
/// and reused: `relocalize` needs the same rows again in the new language.
fn about_rows() -> [(String, String); 4] {
    let g = crate::strings::get;
    [
        (
            g("gui.about.version"),
            format!("{} (Windows)", env!("CARGO_PKG_VERSION")),
        ),
        (
            g("gui.about.engine"),
            format!("libfreemkv {}", env!("CARGO_PKG_VERSION")),
        ),
        (g("gui.about.licence"), "MIT".to_string()),
        (
            g("gui.about.keys"),
            crate::settings::Settings::load().keydb_status(),
        ),
    ]
}

impl About {
    fn new(parent: &gui::WindowMain) -> Self {
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
        for (k, v) in about_rows() {
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
                text: "https://freemkv.org",
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

    /// Re-text everything localized here, for a live language change.
    ///
    /// The macOS shell drops its cached About window on relocalize so the next
    /// open rebuilds it in the new language; this shell builds its About ONCE
    /// (winsafe cannot create controls after the window exists) and reused the
    /// old one forever — so About stayed in the language the app started in,
    /// no matter what the operator picked. Same policy, two shells; only one
    /// of them applied it.
    fn relocalize(&self) {
        let g = crate::strings::get;
        let _ = self.wnd.hwnd().SetWindowText(&g("gui.menu.app_about"));
        let rows = about_rows();
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

    fn show(&self) {
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
            sh.perform(vec![Effect::OpenUrl("https://freemkv.org".into())]);
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

/// Save `Settings` to disk and tell the operator whether it worked.
///
/// This crate's dominant defect this round was exactly this policy
/// implemented twice: the OK button reported a failed save
/// (`gui.log.settings_save_error`), and the language-dropdown path saved the
/// SAME struct a few lines away with a bare `let _ =` that dropped the error
/// on the floor. A disk-full or permissions failure there looked identical to
/// success — the operator picks a language, sees the UI relocalize, and has
/// no way to know the keydb path/keyserver token/dest dir edited in the same
/// session never reached `gui-settings.json`. One policy, one call site now.
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
            // Remember the default destination BEFORE reading the form so we can
            // tell whether the user changed it in this Settings session.
            let old_dest = sh.settings.borrow().dest_dir.clone();
            me.read_form(&mut sh.settings.borrow_mut());
            let edited = sh.settings.borrow().clone();
            let new_dest = edited.dest_dir.clone();
            // The active output directory is a separate live value (a one-off
            // folder pick in the main window overrides the default). Re-point it
            // ONLY when the user actually changed the default here.
            let dest_changed = new_dest != old_dest && !new_dest.trim().is_empty();
            sh.app_mut(|a| {
                a.settings = edited;
                if dest_changed {
                    a.output_dir = new_dest.clone();
                }
            });
            save_settings_reporting_error(&sh);
            me.hide();
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
                        LogKind::Result,
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
                let msg = match crate::settings::update_keydb(&url, &path) {
                    Ok(m) => m,
                    Err(e) => e,
                };
                // RECOVER rather than skip the push (see macOS's identical worker):
                // dropping this message leaves the Update button disabled and
                // TIMER_DRAIN firing forever, since drain() stops it only on a batch.
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

    /// Re-text every label, tab and combo after a language change.
    ///
    /// The macOS shell rebuilds its windows instead; winsafe cannot create
    /// controls after a window exists, so this re-texts in place. The result is
    /// the same and it is cheaper — but it does mean every new control must be
    /// added here as well as in `new`.
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
    }
}

impl Shell {
    /// Apply a language change live, in the newly-active locale (the caller has
    /// already swapped the catalog via `strings::set_locale`).
    ///
    /// The menu bar is genuinely rebuilt (an `HMENU` can be replaced after the
    /// window exists); everything else is re-texted in place, because winsafe
    /// cannot create controls post-creation.
    fn relocalize(&self) {
        let g = crate::strings::get;
        if let Ok(bar) = build_menu() {
            let _ = self.wnd.hwnd().SetMenu(&bar);
            let _ = self.wnd.hwnd().DrawMenuBar();
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
        let _ = self.grp_out.hwnd().SetWindowText(&g("gui.group.output"));
        let _ = self.grp_info.hwnd().SetWindowText(&g("gui.group.info"));
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
        // Force the format dropdown and the tree to repaint in the new language.
        self.memo.borrow_mut().formats.clear();
        self.memo.borrow_mut().rows.clear();
        self.prefs.relocalize(&self.settings.borrow());
        // The About box too: it is built once and cached, so nothing else ever
        // re-texts it.
        self.about.relocalize();
        self.render();
    }
}

// Self-screenshot: `PrintWindow` renders the window into a memory DC (Win32's
// counterpart to macOS's `cacheDisplayInRect:`), no capture permission needed.
// Written as BMP (no encoder dep); macOS writes PNG, so captures aren't byte-comparable.

/// True when every byte is identical — i.e. the capture produced nothing.
///
/// Worth checking because the obvious capture call can "succeed" and still
/// return a blank plate, and a screenshot harness that silently writes black
/// images is worse than no harness: it looks like evidence.
fn is_blank(buf: &[u8]) -> bool {
    buf.first()
        .is_some_and(|first| buf.iter().all(|b| b == first))
}

fn snapshot(hwnd: &w::HWND, path: &str) -> w::AnyResult<()> {
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

/// Let Windows actually lay out and paint before capturing.
///
/// Controls create and draw their content lazily during the message loop, so
/// capturing immediately yields an empty tree and the screenshots become
/// worthless as evidence — the same lesson the macOS harness records.
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

    /// The state-image index the TREE CONTROL is actually showing for a row.
    ///
    /// Read back out of the widget, not the model: the missing-checkbox bug on
    /// macOS lived exactly here — the `View` was right and the cell was wrong.
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
        let on = matches!(
            self.app.borrow().tree.check_state(row),
            Check::Off | Check::Mixed
        );
        self.app_mut(|a| a.tree.set_checked(row, on));
    }

    /// Choose an output format in the REAL dropdown and fire the same handler
    /// the selection change fires. Selecting programmatically does not raise
    /// `CBN_SELCHANGE`, and without this the driver would prove only that the
    /// combo changed appearance — which is how the "picked MP4, got MKV" bug
    /// survived on macOS.
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
        self.cmb_format
            .items()
            .iter()
            .map(|it| it.filter_map(|t| t.ok()).collect::<Vec<_>>())
            .unwrap_or_default()
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

/// The WIDGET-level assertions: what the controls are actually showing, versus
/// what the core's `View` said they should show.
///
/// These are the checks a model test can never make — the missing-checkbox bug
/// on macOS lived exactly here, with a correct `View` and a wrong cell. They
/// depend only on whatever source is currently loaded (including none), so both
/// the interactive `self_test` and the `#[test]` in this file's test module run
/// the same code against the same real window. Returns `(passed, description)`
/// per check rather than panicking, so the interactive mode can print a full
/// report instead of dying on the first failure.
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

        // Every row's state image matches the core's tick, checked against the
        // whole `state_for` mapping: a row with no checkbox must show none, and
        // a Mixed row must show the third glyph, not fall back to checked/unchecked.
        for r in &v.title_rows {
            let want = state_for(r.check);
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
            v.title_rows
                .iter()
                .filter(|x| x.type_s == "Title")
                .enumerate()
                .all(|(i, x)| x.desc.starts_with(&format!("{}.", i + 1))),
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
        self.about.show();
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
    // The file dialogs are COM objects, so the apartment must exist for the
    // lifetime of the app. The guard uninitializes on drop.
    let _com = w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE);

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
        // FMKV_OPEN=<path> opens a source before anything else is captured.
        if let Ok(src) = dev_env("FMKV_OPEN") {
            self.drive_open(&src);
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
                self.about.show();
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

// Tests, two tiers, both via `cargo test` on Windows: pure shell decisions (no
// window/message loop) and `widget_checks` (the FMKV_SELFTEST sweep, driven
// against a real window). App/Tree/View behavior lives in tests/gui_model.rs.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Row as ScanRow, Scanned};

    // ── fixtures ──────────────────────────────────────────────────────────

    /// A two-title disc scan with real stream rows, built by hand so the tests
    /// need no disc, no drive and no fixture file. Title 0 has one video and
    /// two audio tracks; title 1 has one video and one audio.
    fn synthetic_disc() -> Scanned {
        fn row(type_s: &str, desc: &str, depth: u8, checkable: bool, title: usize) -> ScanRow {
            ScanRow {
                type_s: type_s.into(),
                desc: desc.into(),
                depth,
                checkable,
                title,
                info: format!("{type_s} info"),
                pid: None,
                duration_secs: 0.0,
                lang: String::new(),
                forced: false,
            }
        }
        let mut rows = vec![row("Bluray disc", "TEST_DISC", 0, false, usize::MAX)];
        for (ti, secs) in [(0usize, 5400.0f64), (1, 600.0)] {
            let mut t = row("Title", &format!("{}.  playlist", ti + 1), 1, true, ti);
            t.duration_secs = secs;
            rows.push(t);
            rows.push(row("Video", "H.264  1080p", 2, false, ti));
            let mut a = row("Audio", "DTS-HD  eng", 2, true, ti);
            a.pid = Some(0x1100 + ti as u16);
            rows.push(a);
            if ti == 0 {
                let mut a2 = row("Audio", "AC-3  fra", 2, true, ti);
                a2.pid = Some(0x1200);
                rows.push(a2);
            }
        }
        Scanned {
            label: "TEST_DISC".into(),
            volume_id: "TEST_DISC".into(),
            rows,
            key_summary: "keys: none needed".into(),
            title_count: 2,
            video_codecs: vec!["H.264".into(), "H.264".into()],
            // The identities the ticked title numbers refer to. This fixture
            // is a self-test harness, so an empty set is right: every identity
            // check is inert, which is what a synthetic disc wants.
            title_ids: Vec::new(),
            details: vec![],
        }
    }

    fn view_rows() -> Vec<Row> {
        let mut app = App::new();
        app.tree = crate::ui::Tree::from_scan(
            &synthetic_disc(),
            "All titles",
            0.0,
            &crate::ui::LangPrefs::default(),
        );
        app.page = Page::Titles;
        app.view().title_rows
    }

    // ── menu routing ──────────────────────────────────────────────────────

    #[test]
    fn every_menu_id_the_shell_enables_also_routes_to_a_command() {
        // `sync_menu_enabled` walks MENU_CMD_IDS and asks `cmd_for` for the
        // rule; an id in the list that `cmd_for` does not know is a menu item
        // that is greyed on no rule at all.
        let unrouted: Vec<u16> = MENU_CMD_IDS
            .iter()
            .copied()
            .filter(|id| cmd_for(*id).is_none())
            .collect();
        assert!(
            unrouted.is_empty(),
            "MENU_CMD_IDS entries with no cmd_for mapping: {unrouted:?}"
        );
    }

    #[test]
    fn the_menu_reaches_every_command_the_core_defines() {
        // The real contract: a command the core knows how to dispatch but no
        // menu id produces is unreachable by keyboard or menu. `SetFormat` is
        // deliberately excluded — it comes from the format combo, not a menu.
        let reachable: Vec<Cmd> = MENU_CMD_IDS.iter().filter_map(|id| cmd_for(*id)).collect();
        for want in [
            Cmd::Open,
            Cmd::Close,
            Cmd::SetOutput,
            Cmd::Run,
            Cmd::Eject,
            Cmd::Settings,
            Cmd::Quit,
            Cmd::SelectAll,
            Cmd::SelectNone,
            Cmd::Invert,
            Cmd::ToggleLog,
            Cmd::ClearLog,
            Cmd::Docs,
            Cmd::CheckUpdates,
            Cmd::About,
        ] {
            assert!(
                reachable.contains(&want),
                "{want:?} is not reachable from any menu id"
            );
        }
        // Cancel is the one command with no menu item: it lives on the
        // progress page's button, where it is always reachable mid-rip.
        assert!(
            !reachable.contains(&Cmd::Cancel),
            "Cancel gained a menu id; `blocked_while_running` never greys it, \
             so the enable pass would leave a live Cancel on a page with no run"
        );
    }

    #[test]
    fn an_id_that_is_not_a_command_routes_nowhere() {
        // Separators and the accelerator-only ids must not fall through to a
        // command; `cmd_for`'s catch-all is what guarantees it.
        assert_eq!(cmd_for(0), None);
        assert_eq!(cmd_for(IDM_COPY), None);
        assert_eq!(cmd_for(IDM_SELECT_ALL_TEXT), None);
        assert_eq!(cmd_for(ID_TREE), None);
    }

    // ── tick glyphs ───────────────────────────────────────────────────────

    #[test]
    fn a_row_with_no_checkbox_shows_no_state_image() {
        // Index 0 is the tree control's "no state image" — the disc root and
        // the implicit Video rows must land there, not on an unchecked box the
        // user would reasonably try to tick.
        assert_eq!(state_for(None), 0);
    }

    #[test]
    fn each_tick_state_gets_its_own_glyph() {
        // Mixed must not collapse onto checked or unchecked: the third glyph is
        // the only thing telling a user some streams under a title are off.
        let all = [
            state_for(None),
            state_for(Some(Check::Off)),
            state_for(Some(Check::On)),
            state_for(Some(Check::Mixed)),
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 4, "two tick states share a glyph: {all:?}");
        // And they must be indices into the image list built by
        // `build_check_images`, which installs exactly three bitmaps at the
        // 1-based indices the tree control reserves.
        for s in [
            state_for(Some(Check::Off)),
            state_for(Some(Check::On)),
            state_for(Some(Check::Mixed)),
        ] {
            assert!((1..=3).contains(&s), "state image index {s} has no bitmap");
        }
    }

    // ── the redraw memo ───────────────────────────────────────────────────

    #[test]
    fn the_row_signature_ignores_tick_state() {
        // `render` rebuilds the tree when the signature changes, destroying
        // expansion/selection. Ticking a box must NOT change the signature — it
        // goes through `sync_tree_states` instead.
        let rows = view_rows();
        let before = rows_sig(&rows);
        let flipped: Vec<Row> = rows
            .iter()
            .cloned()
            .map(|mut r| {
                r.check = match r.check {
                    Some(Check::Off) => Some(Check::On),
                    Some(Check::On) => Some(Check::Mixed),
                    other => other,
                };
                r
            })
            .collect();
        assert_ne!(
            rows.iter().map(|r| r.check).collect::<Vec<_>>(),
            flipped.iter().map(|r| r.check).collect::<Vec<_>>(),
            "the fixture must actually change some tick states"
        );
        assert_eq!(
            before,
            rows_sig(&flipped),
            "a tick change altered the row signature, so every toggle now \
             rebuilds the tree and loses the user's expansion state"
        );
    }

    #[test]
    fn the_row_signature_notices_a_different_set_of_rows() {
        // The other half of the contract: if the rows really did change, the
        // signature must too, or the tree would keep showing the old disc.
        let rows = view_rows();
        let base = rows_sig(&rows);

        let mut renamed = rows.clone();
        renamed[1].desc.push_str(" (remastered)");
        assert_ne!(base, rows_sig(&renamed), "a renamed row went unnoticed");

        let mut retyped = rows.clone();
        retyped[2].type_s = "Subtitle".into();
        assert_ne!(base, rows_sig(&retyped), "a retyped row went unnoticed");

        let mut reindented = rows.clone();
        reindented[2].depth = 1;
        assert_ne!(
            base,
            rows_sig(&reindented),
            "a re-indented row went unnoticed"
        );

        let mut dropped = rows.clone();
        dropped.pop();
        assert_ne!(base, rows_sig(&dropped), "a removed row went unnoticed");

        let mut swapped = rows.clone();
        swapped.swap(2, 3);
        assert_ne!(base, rows_sig(&swapped), "a reordered tree went unnoticed");
    }

    // STOPGAP, NOT COVERAGE: `About` is a live `WindowModeless`, so observing
    // its text after a language switch needs a window/message pump. Source
    // inspection only: fails if the `relocalize()` call/method is removed.
    #[test]
    fn the_language_switch_re_texts_the_cached_about_box_source_inspection_only() {
        let src = include_str!("windows.rs");
        // Concatenated so these needles cannot match this test's own text.
        let call = format!("{}{}", "self.about.relocal", "ize();");
        assert!(
            src.contains(&call),
            "Shell::relocalize no longer re-texts the About box — it is built \
             once and cached, so nothing else ever will"
        );
        // Something only About::relocalize does: re-text the VALUE column from a
        // fresh `about_rows()`. Shell/Prefs::relocalize share the method name,
        // so the name alone would pin nothing.
        let vals = format!(
            "{}{}",
            "for (l, (_, v)) in self.lbl_vals.iter()", ".zip(rows.iter()) {"
        );
        assert!(
            src.contains(&vals),
            "About::relocalize is gone — the About box would keep the launch \
             language's labels and its old keydb status line"
        );
        let rows_fn = format!("{}{}", "fn about_rows() -> ", "[(String, String); 4] {");
        assert!(
            src.contains(&rows_fn),
            "about_rows is gone, so the About rows can only be built once"
        );
    }

    // ── row text ──────────────────────────────────────────────────────────

    #[test]
    fn a_row_label_carries_both_columns_the_mac_outline_shows() {
        // `SysTreeView32` has one column, so the Type and Description the macOS
        // outline shows side by side are joined here. Both must survive the
        // join or this shell shows strictly less than the other one.
        let rows = view_rows();
        let title = rows.iter().find(|r| r.type_s == "Title").unwrap();
        let text = row_text(title);
        assert!(text.contains(&title.type_s), "type dropped from {text:?}");
        assert!(
            text.contains(&title.desc),
            "description dropped from {text:?}"
        );
    }

    #[test]
    fn the_disc_row_does_not_repeat_its_type_in_the_label() {
        // Root's Description is the volume label, Type is the disc format; macOS
        // has a Type column for that, this shell doesn't, and prefixing "Bluray
        // disc  " to the label reads as noise — deliberate shell rule.
        let rows = view_rows();
        let root = &rows[0];
        assert_eq!(root.depth, 0);
        assert!(!root.type_s.is_empty(), "fixture root must carry a type");
        assert_eq!(row_text(root), root.desc);
    }

    // ── the log pane ──────────────────────────────────────────────────────

    #[test]
    fn a_notice_is_marked_in_a_control_that_cannot_show_colour() {
        // macOS colours notices red. A Win32 EDIT cannot colour a single line,
        // so severity is carried by a gutter character instead. Losing it would
        // make a warning indistinguishable from ordinary chatter.
        let log = vec![
            LogLine {
                text: "ordinary".into(),
                kind: LogKind::Detail,
            },
            LogLine {
                text: "something went wrong".into(),
                kind: LogKind::Notice,
            },
            LogLine {
                text: "done".into(),
                kind: LogKind::Result,
            },
        ];
        let rendered = log_text(&log);
        let lines: Vec<&str> = rendered.split("\r\n").map(|s| s.trim_end()).collect();
        assert_eq!(lines.len(), 3, "one rendered line per log line");
        assert_eq!(lines[0], "ordinary", "a detail line is shown verbatim");
        assert_eq!(lines[2], "done", "a result line is shown verbatim");
        assert!(
            lines[1].starts_with("! ") && lines[1].ends_with("something went wrong"),
            "a notice must be marked and still readable: {:?}",
            lines[1]
        );
    }

    #[test]
    fn the_log_pane_uses_crlf_so_lines_do_not_run_together() {
        // A bare LF renders as one run-on line in a Win32 EDIT control.
        let log = vec![
            LogLine {
                text: "one".into(),
                kind: LogKind::Detail,
            },
            LogLine {
                text: "two".into(),
                kind: LogKind::Detail,
            },
        ];
        assert_eq!(log_text(&log), "one\r\ntwo");
    }

    #[test]
    fn crlf_normalizes_every_newline_exactly_once() {
        assert_eq!(crlf("a\nb"), "a\r\nb");
        // Already-CRLF text must not become CRCRLF — the detail pane is
        // re-rendered on every tick, so a doubling bug compounds.
        assert_eq!(crlf("a\r\nb"), "a\r\nb");
        assert_eq!(crlf(&crlf("a\nb")), "a\r\nb");
        assert_eq!(crlf("a\n\nb"), "a\r\n\r\nb");
        assert_eq!(crlf("no breaks"), "no breaks");
    }

    // ── settings dropdowns ────────────────────────────────────────────────

    #[test]
    fn the_shared_dropdowns_come_from_the_core() {
        // Every enum combo but "container" must be the core's table verbatim —
        // a shell-local copy is how the two shells drifted before.
        for key in [
            "selection",
            "rip_mode",
            "key_source",
            "log_level",
            "language",
        ] {
            assert_eq!(
                enum_options(key)
                    .into_iter()
                    .map(|(c, l)| (c.to_string(), l))
                    .collect::<Vec<_>>(),
                crate::ui::enum_options(key)
                    .into_iter()
                    .map(|(c, l)| (c.to_string(), l))
                    .collect::<Vec<_>>(),
                "{key} is not the shared table"
            );
            assert!(!enum_options(key).is_empty(), "{key} lost its options");
        }
    }

    #[test]
    fn the_container_dropdown_stores_a_canonical_format_not_a_label() {
        // This combo is flat, so the shell maps the selected INDEX back to
        // `opts[i].0`. That value is persisted and matched by the engine, so it
        // must be the canonical English string even when the label is not.
        let opts = enum_options("container");
        let canonical: Vec<&str> = opts.iter().map(|(c, _)| *c).collect();
        let expected: Vec<&str> = crate::ui::output_formats(true, true)
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(
            canonical, expected,
            "the container combo no longer offers the core's format list in order"
        );
        for (canon, label) in &opts {
            assert_eq!(*label, crate::ui::format_label(canon), "label mismatch");
            assert!(
                crate::ui::format_by_title(canon, true, true).is_some(),
                "{canon:?} is not a format the core recognizes"
            );
        }
    }

    // ── the real window ───────────────────────────────────────────────────

    /// Builds the real window, loads a synthetic disc into it, renders, and
    /// runs the SAME widget sweep the interactive `FMKV_SELFTEST` mode runs.
    ///
    /// This is the test that makes `self_test`'s widget assertions reachable
    /// from `cargo test`: no disc, no drive and no screenshots, but real
    /// controls, driven through the real message loop (which is the only place
    /// the window exists and the controls have handles).
    ///
    /// One test, not several: the shell registers a window class by name, so a
    /// second window in the same process would collide.
    #[test]
    fn the_real_controls_show_what_the_core_decided() {
        let _com = w::CoInitializeEx(co::COINIT::APARTMENTTHREADED | co::COINIT::DISABLE_OLE1DDE);
        let shell = Shell::new();
        shell.events();

        /// `(passed, description)` for each widget check, filled in from
        /// inside the message loop and read back once it has exited.
        type Report = Rc<RefCell<Option<Vec<(bool, String)>>>>;
        let results: Report = Rc::new(RefCell::new(None));

        let me = shell.clone();
        let sink = results.clone();
        shell.wnd.on().wm_timer(TIMER_HARNESS, move || {
            let _ = me.wnd.hwnd().KillTimer(TIMER_HARNESS);
            // A scan the shell has never seen, with a partial stream selection
            // so a Mixed glyph is actually on screen.
            me.app_mut(|a| {
                a.tree = crate::ui::Tree::from_scan(
                    &synthetic_disc(),
                    "All titles",
                    0.0,
                    &crate::ui::LangPrefs::default(),
                );
                a.source = "Z:\\synthetic.iso".into();
                a.page = Page::Titles;
            });
            let mixed = me
                .app
                .borrow()
                .tree
                .arena
                .iter()
                .enumerate()
                .find(|(_, n)| n.type_s == "Audio")
                .map(|(i, _)| i);
            if let Some(i) = mixed {
                me.app_mut(|a| a.tree.set_checked(i, false));
            }
            me.render();
            pump(120);
            *sink.borrow_mut() = Some(me.widget_checks());
            w::PostQuitMessage(0);
            Ok(())
        });
        // Armed from BOTH create and show: a runner that never raises WM_SHOWWINDOW
        // would leave `run_main` pumping forever (a CI hang, not a failure).
        // Re-arming the same timer id just restarts it, so firing twice is fine.
        let me = shell.clone();
        shell.wnd.on().wm_create(move |_| {
            let _ = me.wnd.hwnd().SetTimer(TIMER_HARNESS, 200, None);
            Ok(0)
        });
        let me = shell.clone();
        shell.wnd.on().wm_show_window(move |_| {
            let _ = me.wnd.hwnd().SetTimer(TIMER_HARNESS, 200, None);
            Ok(())
        });

        shell
            .wnd
            .run_main(None)
            .expect("the main window could not be created or pumped");

        let results = results.borrow_mut().take().expect(
            "the harness timer never fired — the window was never shown, so no \
             widget was ever checked",
        );
        assert!(!results.is_empty(), "the widget sweep checked nothing");
        // A Mixed glyph must actually have been exercised, or the sweep proved
        // nothing about the state a plain checkbox cannot show.
        assert!(
            results
                .iter()
                .any(|(_, m)| m.contains("Some(Mixed)") || m.contains("Mixed")),
            "no Mixed row reached the widget sweep:\n{}",
            results
                .iter()
                .map(|(_, m)| m.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        );
        let failed: Vec<&str> = results
            .iter()
            .filter(|(ok, _)| !ok)
            .map(|(_, m)| m.as_str())
            .collect();
        assert!(
            failed.is_empty(),
            "widget checks failed:\n{}",
            failed.join("\n")
        );
    }

    // Timer failure is reported, not swallowed. STOPGAP, NOT COVERAGE: no
    // Windows toolchain here to force a real SetTimer failure, so this is
    // source inspection only — can't prove the MessageBox fires; needs Windows CI.
    /// This file's own text, with CRLF folded to LF.
    ///
    /// The source-inspection stopgaps below match multi-line needles against
    /// it. Windows CI checks the tree out with CRLF, so a raw `include_str!`
    /// carries `\r\n` and every needle written with `\n` misses — passing on
    /// Unix and failing only on the Windows runner, which is precisely where
    /// nobody can reproduce it locally. Normalise once, here.
    fn own_source() -> String {
        include_str!("windows.rs").replace("\r\n", "\n")
    }

    #[test]
    fn set_timer_failure_is_reported_source_inspection_only() {
        let src = own_source();
        // Split across concatenated literals so this needle can't match the
        // assertion's OWN text via `include_str!` of this same file — see
        // `mac.rs`'s identical guard against a self-matching tautology.
        let bare_tick = format!(
            "{}{}",
            "let _ = self.wnd.hwnd().SetTimer(TIMER_TICK", ", TICK_MS, None);"
        );
        assert!(
            !src.contains(&bare_tick),
            "Effect::StartTicking discards SetTimer's failure again with a \
             bare `let _ =` — if the rip-progress timer fails to start, \
             nothing ever observes RunState.finished and the window sits on \
             the Progress page forever with no way for the operator to \
             learn why"
        );
        let handler = format!("{}{}", "fn report_timer_", "failure");
        assert!(
            src.contains(&handler),
            "no report_timer_failure (or equivalent) handler exists to \
             surface a SetTimer failure to the operator"
        );
        let msgbox = format!("{}{}", "wnd.hwnd().Message", "Box(");
        assert!(
            src.contains(&msgbox),
            "the timer-failure handler no longer shows a MessageBox — a \
             silently-swallowed SetTimer failure is indistinguishable from \
             a hung rip"
        );
    }

    // One settings-save policy, not two. STOPGAP, NOT COVERAGE (same caveat as
    // the timer test above): source inspection only, fails if the language-switch
    // handler stops routing through the shared save helper the OK button uses.
    #[test]
    fn language_switch_reports_a_failed_save_source_inspection_only() {
        let src = own_source();
        // `.settings.borrow().save` `(…)` should appear in exactly ONE place:
        // the shared helper. A second occurrence means a call site re-inlined
        // its own Ok/Err match — the "one policy implemented twice" bug, again.
        let direct_save = format!("{}{}", ".settings.borrow().save", "()");
        let occurrences = src.matches(&direct_save).count();
        assert_eq!(
            occurrences, 1,
            "the direct settings-save call appears {occurrences} times \
             outside this test — it must appear exactly once, inside \
             save_settings_reporting_error, or the language-switch path (or \
             some future path) has silently grown its own copy again"
        );
        let helper = format!("{}{}", "fn save_settings_reporting", "_error");
        assert!(
            src.contains(&helper),
            "the shared save_settings_reporting_error helper is gone"
        );
        let language_call = format!(
            "{}{}",
            "me.read_form(&mut sh.settings.borrow_mut());\n                save_settings_reporting",
            "_error(&sh);"
        );
        assert!(
            src.contains(&language_call),
            "the language combo's cbn_sel_change handler no longer calls \
             save_settings_reporting_error right after committing the form \
             — a failed save on the language-switch path would go \
             unreported again"
        );
    }

    // Language pickers use the shared rules, not a local parser. Source
    // inspection only (proving the menu ticks needs a real HWND): checks the
    // set logic isn't reimplemented here — the drift `ui::lang_*` prevents.
    #[test]
    fn the_language_pickers_own_no_parsing_source_inspection_only() {
        let src = own_source();
        for key in ["audio_langs", "sub_langs", "forced_sub_langs"] {
            assert!(
                src.contains(&format!("r.lang(&g(\"gui.set.{key}\")")),
                "{key} is no longer built with the r.lang checklist row"
            );
            assert!(
                src.contains(&format!("langs.push((\n            \"{key}\",")),
                "{key} is missing from the `langs` registry — it would render \
                 the stored value and OK would never read it back"
            );
        }
        // Every rule must be a call INTO ui, not a copy of one.
        for f in [
            "lang_summary",
            "lang_toggle",
            "lang_is_selected",
            "PICKER_LANGUAGES",
        ] {
            assert!(
                src.contains(&format!("crate::ui::{f}")),
                "the picker no longer goes through ui::{f}"
            );
        }
        // Tells of a hand-rolled second parser: splitting on commas or joining
        // codes back up, anywhere in this file. Concatenated literals so these
        // needles can't match this test's own text via `include_str!`.
        let tells = [
            format!("{}{}", "split(", "',')"),
            format!("{}{}", "split([", "','"),
            format!("{}{}", ".join(", "\",\")"),
        ];
        for needle in &tells {
            assert!(
                !src.contains(needle),
                "windows.rs contains `{needle}` — the comma-separated language \
                 string is being parsed or rebuilt here instead of in \
                 ui::lang_selection / ui::lang_selection_to_string, which is \
                 how the two shells drift apart"
            );
        }
    }

    // Keyserver token is not shown in plaintext. STOPGAP, NOT COVERAGE: whether
    // ES::PASSWORD masks keystrokes needs a real HWND (same gap as above).
    // Source inspection only: fails if the Keys tab uses the plain `field` ctor.
    #[test]
    fn the_keyserver_token_field_is_secure_source_inspection_only() {
        let src = own_source();
        let secure_ctor = format!("{}{}", "fn field_", "secure");
        assert!(
            src.contains(&secure_ctor),
            "the field_secure (ES::PASSWORD) constructor is gone"
        );
        assert!(
            src.contains(&format!(
                "{}{}",
                "co::ES::AUTOHSCROLL | co::ES::PASS", "WORD"
            )),
            "field_secure no longer sets ES::PASSWORD"
        );
        let call = format!(
            "{}{}",
            "r.field_secure(&g(\"gui.set.keyserver_", "token\"), &st.keyserver_token, 320)"
        );
        assert!(
            src.contains(&call),
            "keyserver_token is no longer built with field_secure — the \
             bearer token would render in a plain edit control again, fully \
             legible during screen-sharing or a recording"
        );
    }
}
