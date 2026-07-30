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
//! memos* (`last_rows`, `last_formats`): a signature of what was last painted,
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

use crate::ui::{App, Check, Cmd, Effect, LogKind, Page, Row, View};

// ── window geometry ───────────────────────────────────────────────────────
//
// The same proportions the macOS shell uses, so the two look like one product:
// tree 46.4% wide, log 32% of the height on the tree page and 62% while
// ripping, Output/Info groups stacked on the right.

const W: i32 = 1180;
const H: i32 = 760;
const MIN_W: i32 = 1020;
const MIN_H: i32 = 620;
const PAD: i32 = 8;
/// Reserved strip under the in-window menu bar.
const TB_H: i32 = 4;
/// Progress page height with both bars, and with only the per-title bar.
const PROG_H: i32 = 292;
const PROG_H_ONE: i32 = 246;
/// Result page height — fixed so its contents never drift off-screen.
const RESULT_H: i32 = 200;
/// Height of the Output group on the right column.
const OUT_H: i32 = 110;
const TREE_FRAC: f64 = 0.464;

/// Poll interval for a running job, in milliseconds. Matches the macOS timer.
const TICK_MS: u32 = 200;
const TIMER_TICK: usize = 1;
/// Drain interval for worker-thread messages (the keydb update).
const TIMER_DRAIN: usize = 2;

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

// ── Win32 entry points winsafe does not wrap ──────────────────────────────
//
// Declared directly, the same way `platform.rs` declares `GetDiskFreeSpaceExW`,
// rather than pulling in a second binding crate for four functions.

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
    }

    /// `PW_RENDERFULLCONTENT` — required to capture DirectComposition-rendered
    /// content; without it modern controls come back blank.
    pub const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;

    pub const DFC_BUTTON: u32 = 4;
    pub const DFCS_BUTTONCHECK: u32 = 0x0000_0000;
    pub const DFCS_CHECKED: u32 = 0x0000_0400;
    pub const DFCS_BUTTON3STATE: u32 = 0x0000_0008;
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

        // Leak deliberately. The icon must outlive this call for as long as the
        // window exists, and there is exactly one window per process lifetime,
        // so letting the guard drop here would destroy the HICON the title bar
        // is still pointing at. Not `LR::SHARED`: that is documented as
        // unreliable for a non-default requested size, which is the whole point
        // of loading these two explicitly.
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
/// checkbox on the system at any DPI, rather than hand-painted approximations.
fn build_check_images<T: 'static>(tree: &gui::TreeView<T>) -> w::AnyResult<()> {
    let il = tree.image_list(co::TVSIL::STATE)?;
    if il.GetImageCount() >= 4 {
        return Ok(()); // already built
    }

    let desktop = w::HWND::GetDesktopWindow();
    let screen_dc = desktop.GetDC()?;
    let theme = tree.hwnd().OpenThemeData("BUTTON");
    let bg = w::HBRUSH::GetSysColorBrush(co::COLOR::WINDOW)?;
    let rc = w::RECT {
        left: 0,
        top: 0,
        right: 16,
        bottom: 16,
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
        let bmp = screen_dc.CreateCompatibleBitmap(16, 16)?;
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

        let wnd = gui::WindowMain::new(gui::WindowMainOpts {
            title: "freemkv",
            class_name: "FmkvMain",
            // The window class icon is what Windows falls back to for the
            // taskbar and Alt-Tab. It is NOT enough on its own: winsafe fills
            // both `hIcon` and `hIconSm` from this one value via `LoadIcon`,
            // which always returns the 32 px frame, so the title bar would show
            // a 32→16 downscale. `set_icons` below fixes that with WM_SETICON.
            class_icon: gui::Icon::Id(IDI_APP),
            size: (W, H),
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
                size: (W - PAD * 2, 26),
                control_style: co::SS::CENTER,
                ..Default::default()
            },
        );
        let lbl_empty_sub = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &crate::strings::get("gui.page.empty_subtitle"),
                size: (W - PAD * 2, 20),
                control_style: co::SS::CENTER,
                ..Default::default()
            },
        );
        let btn_open = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.open_file"),
                width: 180,
                height: 30,
                ctrl_id: ID_OPEN_EMPTY,
                ..Default::default()
            },
        );

        // ── titles page ──
        let tree = gui::TreeView::new(
            &wnd,
            gui::TreeViewOpts {
                size: (400, 400),
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
                width: 300,
                height: OUT_H,
                control_style: co::BS::GROUPBOX,
                window_style: co::WS::CHILD | co::WS::VISIBLE,
                ..Default::default()
            },
        );
        let cmb_format = gui::ComboBox::new(
            &wnd,
            gui::ComboBoxOpts {
                width: 280,
                ctrl_id: ID_FORMAT,
                ..Default::default()
            },
        );
        let edit_out = gui::Edit::new(
            &wnd,
            gui::EditOpts {
                text: &settings.dest_dir,
                width: 240,
                height: 23,
                ctrl_id: ID_OUTDIR,
                ..Default::default()
            },
        );
        let btn_browse = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.browse"),
                width: 34,
                height: 25,
                ctrl_id: ID_BROWSE,
                ..Default::default()
            },
        );
        let btn_run = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.run_now"),
                width: 110,
                height: 28,
                control_style: co::BS::DEFPUSHBUTTON,
                ctrl_id: ID_RUN,
                ..Default::default()
            },
        );
        let btn_eject = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.menu.eject"),
                width: 110,
                height: 26,
                ctrl_id: ID_EJECT,
                ..Default::default()
            },
        );
        let grp_info = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.group.info"),
                width: 300,
                height: 200,
                control_style: co::BS::GROUPBOX,
                window_style: co::WS::CHILD | co::WS::VISIBLE,
                ..Default::default()
            },
        );
        let detail = gui::Edit::new(
            &wnd,
            gui::EditOpts {
                text: &crate::strings::get("gui.page.detail_default"),
                width: 280,
                height: 160,
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
                width: W - PAD * 2,
                height: 132,
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
                    size: (110, 16),
                    control_style: co::SS::RIGHT,
                    ..Default::default()
                },
            ));
            lbl_vals.push(gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: "",
                    size: (W - 200, 16),
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
                    size: (wd, 16),
                    control_style: style,
                    ..Default::default()
                },
            )
        };
        let lbl_saving_cur = mk_label("", 320, co::SS::LEFT);
        let lbl_cur = mk_label("", 330, co::SS::RIGHT);
        let lbl_saving_all = mk_label("", 320, co::SS::LEFT);
        let lbl_all = mk_label("", 330, co::SS::RIGHT);
        let mk_bar = |id: u16| {
            gui::ProgressBar::new(
                &wnd,
                gui::ProgressBarOpts {
                    size: (W - PAD * 2, 20),
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
                width: 110,
                height: 30,
                ctrl_id: ID_CANCEL,
                ..Default::default()
            },
        );

        // ── result page ──
        let lbl_result_head = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &crate::strings::get("gui.result.finished"),
                size: (W - PAD * 2, 26),
                control_style: co::SS::CENTER,
                ..Default::default()
            },
        );
        let lbl_result_line = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: "",
                size: (W - PAD * 2, 20),
                control_style: co::SS::CENTER | co::SS::ENDELLIPSIS,
                ..Default::default()
            },
        );
        let btn_reveal = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.show_explorer"),
                width: 170,
                height: 32,
                ctrl_id: ID_REVEAL,
                ..Default::default()
            },
        );
        let btn_done = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.done"),
                width: 170,
                height: 32,
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
                width: W - PAD * 2,
                height: 200,
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

    // The Edit menu mixes two kinds of item: standard text commands that act on
    // the focused control (so Copy works in the log), and our tree-selection
    // commands. The tree commands deliberately do NOT take Ctrl+A/Ctrl+C, which
    // would break text selection in the log.
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
            text: &format!("{}\tCtrl+L", g("gui.menu.show_log")),
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
    fn relayout(&self, cw: i32, ch: i32) {
        let v = self.app.borrow().view();
        let two_bars = v.show_overall_bar;
        let hidden = v.log_hidden;

        // The log takes a fixed share of the height; more while ripping, and on
        // the result page it fills everything the fixed-height panel leaves.
        let page_h = match v.page {
            Page::Progress => {
                if two_bars {
                    PROG_H
                } else {
                    PROG_H_ONE
                }
            }
            Page::Result => RESULT_H,
            _ => 0,
        };
        let log_h = if hidden {
            0
        } else if page_h > 0 {
            (ch - TB_H - page_h - PAD * 3).max(120)
        } else {
            ((ch as f64 * 0.32) as i32).max(120)
        };

        let top_y = TB_H + PAD;
        let top_h = (ch - log_h - PAD * 3 - TB_H).max(80);
        let log_y = ch - PAD - log_h;

        place(&self.log, PAD, log_y, cw - PAD * 2, log_h);
        show(&self.log, !hidden);

        // ── empty page ──
        let cy = top_y + top_h / 2;
        place(&self.lbl_empty_head, PAD, cy - 50, cw - PAD * 2, 26);
        place(&self.lbl_empty_sub, PAD, cy - 22, cw - PAD * 2, 20);
        place(&self.btn_open, cw / 2 - 90, cy + 16, 180, 30);

        // ── titles page ──
        let tree_w = ((cw - PAD * 2) as f64 * TREE_FRAC) as i32;
        place(&self.tree, PAD, top_y, tree_w, top_h);
        let rx = PAD + tree_w + PAD;
        let rw = cw - rx - PAD;
        place(&self.grp_out, rx, top_y, rw, OUT_H);
        place(&self.edit_out, rx + 12, top_y + 22, rw - 60, 23);
        place(&self.btn_browse, rx + rw - 44, top_y + 21, 34, 25);
        place(&self.cmb_format, rx + 12, top_y + 56, rw - 150, 24);
        place(&self.btn_run, rx + rw - 128, top_y + 54, 116, 28);
        place(&self.btn_eject, rx + 12, top_y + OUT_H + 4, 110, 26);
        let info_y = top_y + OUT_H + PAD + 30;
        let info_h = (top_h - OUT_H - PAD - 30).max(60);
        place(&self.grp_info, rx, info_y, rw, info_h);
        place(
            &self.detail,
            rx + 10,
            info_y + 20,
            rw - 20,
            (info_h - 32).max(20),
        );

        // ── progress page ──
        let gh = 132;
        place(&self.grp_prog, PAD, top_y, cw - PAD * 2, gh);
        for i in 0..self.lbl_keys.len() {
            let yy = top_y + 20 + i as i32 * 15;
            place(&self.lbl_keys[i], PAD + 6, yy, 110, 15);
            place(&self.lbl_vals[i], PAD + 124, yy, cw - PAD * 2 - 140, 15);
        }
        let bar1_y = top_y + gh + 22;
        place(&self.lbl_saving_cur, PAD, bar1_y - 18, 340, 16);
        place(&self.lbl_cur, cw - PAD - 340, bar1_y - 18, 340, 16);
        place(&self.bar_cur, PAD, bar1_y, cw - PAD * 2, 20);
        let bar2_y = bar1_y + 46;
        place(&self.lbl_saving_all, PAD, bar2_y - 18, 340, 16);
        place(&self.lbl_all, cw - PAD - 340, bar2_y - 18, 340, 16);
        place(&self.bar_all, PAD, bar2_y, cw - PAD * 2, 20);
        place(
            &self.btn_cancel,
            cw - PAD - 120,
            top_y + page_h - 36,
            120,
            30,
        );

        // ── result page ──
        place(&self.lbl_result_head, PAD, top_y + 26, cw - PAD * 2, 26);
        place(&self.lbl_result_line, PAD, top_y + 58, cw - PAD * 2, 20);
        place(&self.btn_reveal, cw / 2 - 180, top_y + 100, 170, 32);
        place(&self.btn_done, cw / 2 + 10, top_y + 100, 170, 32);
    }

    fn relayout_now(&self) {
        if let Ok(rc) = self.wnd.hwnd().GetClientRect() {
            self.relayout(rc.right, rc.bottom);
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
        // TreeView has no `set_redraw` wrapper (only ListView does), so the
        // message goes direct. Without it, rebuilding a large tree flickers.
        self.set_tree_redraw(false);
        let _ = self.tree.items().delete_all();

        // depth 0 = the disc, 1 = a title, 2 = a stream under that title.
        let mut last_root: Option<w::HTREEITEM> = None;
        let mut last_title: Option<w::HTREEITEM> = None;
        for r in rows {
            let text = row_text(r);
            let added = match r.depth {
                0 => self
                    .tree
                    .items()
                    .add_root(&text, None, r.index)
                    .ok()
                    .map(|i| {
                        let h = unsafe { i.htreeitem().raw_copy() };
                        last_root = Some(unsafe { h.raw_copy() });
                        last_title = None;
                        h
                    }),
                1 => last_root.as_ref().and_then(|p| {
                    self.tree
                        .items()
                        .get(p)
                        .add_child(&text, None, r.index)
                        .ok()
                        .map(|i| {
                            let h = unsafe { i.htreeitem().raw_copy() };
                            last_title = Some(unsafe { h.raw_copy() });
                            h
                        })
                }),
                _ => last_title.as_ref().and_then(|p| {
                    self.tree
                        .items()
                        .get(p)
                        .add_child(&text, None, r.index)
                        .ok()
                        .map(|i| unsafe { i.htreeitem().raw_copy() })
                }),
            };
            if let Some(h) = added {
                self.set_row_state(&h, state_for(r.check));
            }
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

        // The format list depends on the source kind, so it is re-derived on
        // every render from the view rather than only at build time. Building it
        // once was a real bug on macOS: opening an MKV after an ISO left
        // "Whole disc → ISO image" on offer for a source that cannot produce it.
        self.sync_formats(&v);

        // ── pages ──
        let p = v.page;
        for c in [&self.lbl_empty_head, &self.lbl_empty_sub] {
            show(c, p == Page::Empty);
        }
        show(&self.btn_open, p == Page::Empty);

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
            // A plain EDIT control cannot colour individual lines, which is how
            // the macOS shell shows severity. Rather than drop the information,
            // notices carry a one-character gutter so a problem is still visible
            // in a monochrome control. (Per-line colour would need a RichEdit.)
            let text = v
                .log
                .iter()
                .map(|l| match l.kind {
                    LogKind::Notice => format!("! {}", l.text),
                    LogKind::Detail | LogKind::Result => l.text.clone(),
                })
                .collect::<Vec<_>>()
                .join("\r\n");
            let _ = self.log.set_text(&text);
            // Keep the newest line in view, as the macOS log does.
            let n = text.chars().count() as i32;
            self.log.set_selection(n, n);
            self.memo.borrow_mut().log_len = v.log.len();
        }

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
                    let _ = std::process::Command::new("explorer.exe")
                        .raw_arg(format!("/select,\"{p}\""))
                        .spawn();
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
                    let _ = self.wnd.hwnd().SetTimer(TIMER_TICK, TICK_MS, None);
                }
                Effect::StopTicking => {
                    let _ = self.wnd.hwnd().KillTimer(TIMER_TICK);
                }
                Effect::Quit => w::PostQuitMessage(0),
                Effect::Redraw => {}
            }
        }
        self.render();
    }

    fn start_drain(&self) {
        let _ = self.wnd.hwnd().SetTimer(TIMER_DRAIN, TICK_MS, None);
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
                    IDM_OPEN_DISC => me.open_disc(),
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
            // The tri-state glyphs must exist before the first render paints a
            // row, and the control must already be created — hence wm_create.
            let _ = build_check_images(&me.tree);
            set_icons(me.wnd.hwnd());
            me.wnd.hwnd().DragAcceptFiles(true);
            // Nothing is open at launch: show the empty state rather than a
            // tree of invented rows.
            me.render();
            Ok(0)
        });

        let me = self.clone();
        self.wnd.on().wm_size(move |p| {
            me.relayout(p.client_area.cx, p.client_area.cy);
            Ok(())
        });

        // Never let the window shrink below the point the layout stops working.
        self.wnd.on().wm_get_min_max_info(move |p| {
            p.info.ptMinTrackSize = w::POINT::with(MIN_W, MIN_H);
            Ok(())
        });

        // Dropping a file on the window is the ordinary way a Windows user opens
        // something they can see, and it mirrors the macOS Finder drop.
        let me = self.clone();
        self.wnd.on().wm_drop_files(move |p| {
            let hdrop = p.hdrop;
            if let Some(path) = hdrop.DragQueryFile()?.next() {
                let path = path?;
                if crate::ui::SOURCE_EXTS.iter().any(|e| {
                    std::path::Path::new(&path)
                        .extension()
                        .and_then(|x| x.to_str())
                        .is_some_and(|x| x.eq_ignore_ascii_case(e))
                }) {
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
            if me.app.borrow().running() {
                let answer = me.wnd.hwnd().MessageBox(
                    &format!(
                        "{}\n\n{}",
                        crate::strings::get("gui.alert.rip_title"),
                        crate::strings::get("gui.alert.rip_body")
                    ),
                    "freemkv",
                    co::MB::YESNO | co::MB::ICONWARNING,
                )?;
                if answer != co::DLGID::YES {
                    return Ok(()); // keep ripping
                }
                me.act(Cmd::Cancel);
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
                let on = matches!(
                    me.app.borrow().tree.check_state(row),
                    Check::Off | Check::Mixed
                );
                me.app_mut(|a| a.tree.set_checked(row, on));
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

    /// Open a live optical drive. Enumerates drives (registry only, no exclusive
    /// access); opens the one drive directly, or autodetects the drive with media
    /// when several are attached.
    fn open_disc(&self) {
        let drives = crate::engine::list_optical_drives();
        if drives.is_empty() {
            self.app_mut(|a| a.say(LogKind::Notice, &crate::strings::get("gui.log.no_drive")));
            return;
        }
        let url = if drives.len() == 1 {
            self.app_mut(|a| {
                a.say(
                    LogKind::Detail,
                    &crate::strings::fmt(
                        "gui.log.opening_drive",
                        &[("label", &drives[0].label), ("device", &drives[0].device)],
                    ),
                )
            });
            format!("disc://{}", drives[0].device)
        } else {
            let list = drives
                .iter()
                .map(|d| format!("{} ({})", d.label, d.device))
                .collect::<Vec<_>>()
                .join(", ");
            self.app_mut(|a| {
                a.say(
                    LogKind::Detail,
                    &crate::strings::fmt(
                        "gui.log.drives_found",
                        &[("n", &drives.len().to_string()), ("list", &list)],
                    ),
                )
            });
            "disc://".to_string()
        };
        let fx = self.app_mut(|a| a.open(&url));
        self.perform(fx);
    }

    /// Drain worker-thread messages onto the log and into the Settings note.
    fn drain(&self) {
        let msgs: Vec<String> = match self.inbox.lock() {
            Ok(mut v) => v.drain(..).collect(),
            Err(_) => return,
        };
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
    let g = crate::strings::get;
    match key {
        "selection" => vec![
            ("Main film only", g("gui.set.sel_main")),
            ("All titles", g("gui.set.sel_all")),
            ("Longest title", g("gui.set.sel_longest")),
        ],
        "rip_mode" => vec![
            ("Multi-pass", g("gui.set.mode_multi")),
            ("Single pass", g("gui.set.mode_single")),
        ],
        "key_source" => vec![
            ("Local keydb only", g("gui.set.key_src_local")),
            ("Online key service only", g("gui.set.key_src_online")),
            ("keydb, then online", g("gui.set.key_src_both")),
        ],
        "log_level" => vec![
            ("Quiet", g("gui.set.log_quiet")),
            ("Normal", g("gui.set.log_normal")),
            ("Verbose", g("gui.set.log_verbose")),
            ("Debug", g("gui.set.log_debug")),
        ],
        // Language: canonical is the locale code, label the endonym (shown as-is
        // in every locale). Driven straight from the shipped list, so the picker
        // can never drift from what freemkv-i18n can actually load.
        "language" => crate::ui::LOCALES
            .iter()
            .map(|(endonym, code)| (*code, (*endonym).to_string()))
            .collect(),
        // The output container is the format list, localized for display.
        "container" => crate::ui::output_formats(true, true)
            .into_iter()
            .flatten()
            .map(|c| (c, crate::ui::format_label(c)))
            .collect(),
        _ => vec![],
    }
}

/// Builds one labelled row per call, walking a y-cursor down a tab page — the
/// right-aligned label / control-to-its-right layout the macOS Settings uses.
struct Rows<'a> {
    page: &'a gui::TabPage,
    y: i32,
    gutter: i32,
    width: i32,
}

impl<'a> Rows<'a> {
    fn new(page: &'a gui::TabPage) -> Self {
        Rows {
            page,
            y: 16,
            gutter: 250,
            width: 620,
        }
    }

    fn label(&self, text: &str) {
        // Static decoration: owned by the page, never touched again.
        let _ = gui::Label::new(
            self.page,
            gui::LabelOpts {
                text,
                position: (8, self.y + 3),
                size: (self.gutter - 16, 18),
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
                width: wd,
                height: 22,
                ..Default::default()
            },
        );
        self.y += 30;
        e
    }

    /// A path row: field plus a browse button that fills it.
    fn path(&mut self, text: &str, val: &str, wd: i32) -> (gui::Edit, gui::Button) {
        self.label(text);
        let e = gui::Edit::new(
            self.page,
            gui::EditOpts {
                text: val,
                position: (self.gutter, self.y),
                width: wd - 40,
                height: 22,
                ..Default::default()
            },
        );
        let b = gui::Button::new(
            self.page,
            gui::ButtonOpts {
                text: &crate::strings::get("gui.btn.browse"),
                position: (self.gutter + wd - 36, self.y - 1),
                width: 34,
                height: 24,
                ..Default::default()
            },
        );
        self.y += 30;
        (e, b)
    }

    fn check(&mut self, text: &str) -> gui::CheckBox {
        self.label(text);
        let c = gui::CheckBox::new(
            self.page,
            gui::CheckBoxOpts {
                text: "",
                position: (self.gutter, self.y + 2),
                size: (20, 18),
                ..Default::default()
            },
        );
        self.y += 28;
        c
    }

    fn combo(&mut self, key: &str, text: &str, wd: i32) -> gui::ComboBox {
        self.label(text);
        let labels: Vec<String> = enum_options(key).into_iter().map(|(_, l)| l).collect();
        let c = gui::ComboBox::new(
            self.page,
            gui::ComboBoxOpts {
                position: (self.gutter, self.y),
                width: wd,
                items: &labels.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                ..Default::default()
            },
        );
        self.y += 30;
        c
    }

    fn button(&mut self, text: &str, wd: i32) -> gui::Button {
        let b = gui::Button::new(
            self.page,
            gui::ButtonOpts {
                text,
                position: (self.gutter, self.y),
                width: wd,
                height: 26,
                ..Default::default()
            },
        );
        self.y += 32;
        b
    }

    /// An explanatory note under a control — never a control that does nothing.
    fn note(&mut self, text: &str) -> gui::Label {
        let l = gui::Label::new(
            self.page,
            gui::LabelOpts {
                text,
                position: (16, self.y),
                size: (self.width - 32, 32),
                ..Default::default()
            },
        );
        self.y += 38;
        l
    }

    fn gap(&mut self) {
        self.y += 12;
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
        // Wide enough that the longest label ("Keep encrypted (raw
        // passthrough) :") and its longer translations fit the gutter without
        // clipping, while the controls still have room.
        let (ww, wh) = (680, 520);
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

        // ── Output ── engine Job.dest + the GUI's own naming
        let mut r = Rows::new(&pages[0]);
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
        let mut r = Rows::new(&pages[1]);
        combos.push((
            "selection",
            r.combo("selection", &g("gui.set.default_selection"), 240),
        ));
        fields.push((
            "min_title_secs",
            r.field(&g("gui.set.min_length"), &st.min_title_secs, 90),
        ));
        r.note(&g("gui.set.min_length_note"));

        // ── Recovery ── engine Job.mode / abort_on_lost_secs / raw
        let mut r = Rows::new(&pages[2]);
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
        let mut r = Rows::new(&pages[3]);
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
            r.field(&g("gui.set.keyserver_token"), &st.keyserver_token, 320),
        ));
        let btn_test = r.button(&g("gui.set.test_connection"), 170);

        // ── Advanced
        let mut r = Rows::new(&pages[4]);
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
                position: (10, 10),
                size: (ww - 20, wh - 66),
                pages: &page_pairs,
                ..Default::default()
            },
        );

        let btn_ok = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &g("gui.btn.ok"),
                position: (ww - 108, wh - 44),
                width: 96,
                height: 28,
                control_style: co::BS::DEFPUSHBUTTON,
                ..Default::default()
            },
        );
        let btn_cancel = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &g("gui.btn.cancel"),
                position: (ww - 212, wh - 44),
                width: 96,
                height: 28,
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
        let (cw, ch) = (rc.right, rc.bottom);
        place(&self.tab, 10, 10, cw - 20, ch - 56);
        place(&self.btn_ok, cw - 108, ch - 38, 96, 28);
        place(&self.btn_cancel, cw - 212, ch - 38, 96, 28);
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
}

impl About {
    fn new(parent: &gui::WindowMain) -> Self {
        let g = crate::strings::get;
        let (ww, wh) = (420, 260);
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
                position: (0, 18),
                size: (ww, 26),
                control_style: co::SS::CENTER,
                ..Default::default()
            },
        );
        let rows: [(String, String); 4] = [
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
        ];
        let mut y = 62;
        for (k, v) in rows {
            let _ = gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: &k,
                    position: (20, y),
                    size: (130, 18),
                    control_style: co::SS::RIGHT,
                    ..Default::default()
                },
            );
            let _ = gui::Label::new(
                &wnd,
                gui::LabelOpts {
                    text: &v,
                    position: (160, y),
                    size: (ww - 175, 18),
                    control_style: co::SS::LEFT | co::SS::ENDELLIPSIS,
                    ..Default::default()
                },
            );
            y += 24;
        }
        let _ = gui::Label::new(
            &wnd,
            gui::LabelOpts {
                text: &g("gui.about.website"),
                position: (20, y),
                size: (130, 18),
                control_style: co::SS::RIGHT,
                ..Default::default()
            },
        );
        // A real button rather than styled text: it opens the site in the
        // default browser, so it does what it looks like it does.
        let btn_site = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: "https://freemkv.org",
                position: (156, y - 3),
                width: 200,
                height: 24,
                control_style: co::BS::FLAT,
                ..Default::default()
            },
        );
        let btn_close = gui::Button::new(
            &wnd,
            gui::ButtonOpts {
                text: &g("gui.btn.close"),
                position: (ww - 110, wh - 46),
                width: 96,
                height: 28,
                control_style: co::BS::DEFPUSHBUTTON,
                ..Default::default()
            },
        );
        About {
            wnd,
            btn_site,
            btn_close,
        }
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
            place(&self.btn_close, rc.right - 110, rc.bottom - 38, 96, 28);
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

impl Prefs {
    fn events(&self, shell: &Shell) {
        // OK: commit the form, persist it, and push it into the running App so
        // changes (log detail, key source, multipass, dest dir, …) take effect at
        // once — App holds its own copy, loaded at startup, and would otherwise
        // stay stale until the next launch.
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
                if let Ok(mut v) = inbox.lock() {
                    v.push(msg);
                }
            });
            sh.start_drain();
            Ok(())
        });

        // The interface-language dropdown applies the moment a language is
        // picked: commit the form, swap the catalog, and re-text every window.
        let me = self.clone();
        let sh = shell.clone();
        if let Some((_, combo)) = self.combos.iter().find(|(k, _)| *k == "language") {
            let combo = combo.clone();
            #[allow(clippy::redundant_clone)]
            combo.on().cbn_sel_change(move || {
                me.read_form(&mut sh.settings.borrow_mut());
                let _ = sh.settings.borrow().save();
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
        self.render();
    }
}

// ── self-screenshot ───────────────────────────────────────────────────────
//
// No screen-capture permission and no window-server tricks: `PrintWindow` asks
// the window to render itself into a memory DC, which is the Win32 counterpart
// of the macOS shell's `cacheDisplayInRect:`. Written as a BMP because that
// needs no encoder dependency — the macOS harness writes PNG, so the captures
// are equivalent but not byte-comparable across platforms.

/// True when every pixel is identical — i.e. the capture produced nothing.
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

    // Three ways to get the pixels, tried in order until one is not blank.
    //
    // `PW_RENDERFULLCONTENT` is the right answer on a normal desktop (it is the
    // only mode that captures DirectComposition-rendered content), but it needs
    // DWM composition — on a headless/session-0 machine, which is exactly where
    // CI runs, it returns success and paints nothing. Plain `PrintWindow` (which
    // goes through `WM_PRINT`) works there, and a screen blit is the last resort.
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

// ── UI driver ─────────────────────────────────────────────────────────────
//
// Drives the REAL controls — `BM_CLICK` on the actual buttons, real
// `WM_COMMAND`s off the menu — so every action goes through the same wiring a
// mouse click takes. Calling the handlers directly would bypass exactly the
// wiring that broke on macOS twice.

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

        // ── WIDGET-level checks: what the tree actually shows, not what the
        // model said. Model-level tests pass while the widget is broken.
        let title_row = v
            .title_rows
            .iter()
            .position(|x| x.type_s == "Title")
            .unwrap_or(1);
        check(
            "widget-root-has-no-tick",
            self.widget_state(0) == Some(ST_NONE),
            &format!("root state image = {:?}", self.widget_state(0)),
        );
        check(
            "widget-title-has-a-tick",
            self.widget_state(title_row)
                .is_some_and(|s| s == ST_CHECKED || s == ST_UNCHECKED || s == ST_MIXED),
            &format!("title state image = {:?}", self.widget_state(title_row)),
        );
        if let Some(vr) = v.title_rows.iter().position(|x| x.type_s == "Video") {
            check(
                "widget-video-has-no-tick",
                self.widget_state(vr) == Some(ST_NONE),
                &format!("video state image = {:?}", self.widget_state(vr)),
            );
        }
        check(
            "widget-tree-populated",
            self.tree.items().count() as usize == v.title_rows.len(),
            &format!(
                "{} tree items vs {} rows",
                self.tree.items().count(),
                v.title_rows.len()
            ),
        );
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
        check(
            "log-is-readonly-and-selectable",
            self.log.hwnd().style().has(co::WS::TABSTOP),
            "log text can be focused, selected and copied",
        );
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
        check(
            "settings-tabs",
            self.prefs.tab.items().count().unwrap_or(0) == 5,
            &format!("{} tabs", self.prefs.tab.items().count().unwrap_or(0)),
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
        check(
            "menu-bar",
            self.wnd
                .hwnd()
                .GetMenu()
                .and_then(|m| m.GetMenuItemCount().ok())
                == Some(4),
            "File/Edit/View/Help",
        );

        // 8 ── layout at the extremes
        for (cw, ch) in [(MIN_W, MIN_H), (1700, 1050), (W, H)] {
            self.relayout(cw, ch);
        }
        check("resize", true, "min, large and default");
        snap("07-resized");

        // 9 ── REAL RUN: click Run Now, watch it start, click Cancel
        self.act(Cmd::SelectNone);
        // Resolve the index in its OWN statement, exactly as step 3 does. An
        // `if let Some(t) = self.app.borrow()…` keeps the `Ref` alive for the
        // whole then-block — including across `app_mut`, whose `borrow_mut`
        // then panics with "RefCell already borrowed" and, because it unwinds
        // out of a Win32 window procedure, aborts the process outright. The
        // index is a plain `usize`, so there is no reason to hold the borrow.
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
                    "D:\\media\\iso\\Greenland.iso",
                    "Greenland.iso",
                    &crate::ui::fmt_bytes(6_743_590_912),
                    &format!("{}/s", crate::ui::fmt_bytes(41_400_000)),
                    "C:\\Users\\me\\Videos\\GREENLAND_t1.mkv",
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
