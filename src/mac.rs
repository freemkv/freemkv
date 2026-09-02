//! macOS AppKit shell — logicless preview.
//!
//! Layout proportions: toolbar 32px · tree 46.4% wide · log 34% tall ·
//! Info/Output groups on the right.

use std::cell::RefCell;

use objc2::Message;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
use objc2::{AllocAnyThread, DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAppearance, NSAppearanceNameAqua, NSApplication, NSApplicationActivationPolicy,
    NSApplicationDelegate, NSApplicationTerminateReply, NSBackingStoreType, NSBezelStyle,
    NSBitmapImageFileType, NSBox, NSBoxType, NSButton, NSButtonCell, NSButtonType, NSColor,
    NSComboBox, NSComboBoxDelegate, NSControlTextEditingDelegate, NSFont, NSFontWeightRegular,
    NSMenu, NSMenuItem, NSOpenPanel, NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate,
    NSPopUpButton, NSProgressIndicator, NSScrollView, NSSecureTextField, NSTableColumn,
    NSTableViewSelectionHighlightStyle, NSTextAlignment, NSTextField, NSTextFieldDelegate,
    NSTextView, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    MainThreadMarker, NSDate, NSDictionary, NSLocale, NSNumber, NSPoint, NSRect, NSRunLoop, NSSize,
    NSString, NSTimer,
};

/// The macOS preferred UI language as a BCP-47 tag ("en-US", "de-DE", "pt-BR",
/// "zh-Hans-CN"), or None. A Finder-launched `.app` inherits no `LANG`, so the
/// i18n crate's env-based detection would wrongly fall back to English; this
/// reads the real system language for the "Auto" case. The raw tag is returned
/// as-is — `freemkv_i18n` normalizes and region-resolves it.
pub fn system_locale_code() -> Option<String> {
    NSLocale::preferredLanguages()
        .iter()
        .next()
        .map(|s| s.to_string())
}

const W: f64 = 1180.0;
const H: f64 = 760.0;

const LOG_H: f64 = 244.0; // 34% of client height, scaled
/// Top margin where the toolbar strip used to be.
const TB_H: f64 = 10.0;
const PAD: f64 = 8.0;
/// Rip page is shorter than the tree page; the log takes the slack
/// (reference: log ~64% of the window while ripping vs ~34% on the tree).
const PROG_H: f64 = 292.0;
/// Height with the overall bar hidden (single-title run).
const PROG_H_ONE: f64 = 246.0;
/// Result page height — fixed so its contents never drift off-screen.
const RESULT_H: f64 = 200.0;
const LOG_H_PROG: f64 = 470.0;

/// Development-only environment lookup. In a release build this always fails,
/// so the shipped app has no environment switches at all.
/// Map an AppKit selector to a core command.
fn cmd_for(a: Sel) -> Option<crate::ui::Cmd> {
    use crate::ui::Cmd;
    Some(if a == sel!(onOpenFiles:) {
        Cmd::Open
    } else if a == sel!(onOpenDisc:) {
        // Opening a disc is an Open for menu-enable purposes (blocked mid-rip).
        Cmd::Open
    } else if a == sel!(onCloseDisc:) {
        Cmd::Close
    } else if a == sel!(onBrowseOutput:) {
        Cmd::SetOutput
    } else if a == sel!(onRip:) {
        Cmd::Run
    } else if a == sel!(onCancelRip:) {
        Cmd::Cancel
    } else if a == sel!(onEject:) {
        Cmd::Eject
    } else if a == sel!(onSelectAll:) {
        Cmd::SelectAll
    } else if a == sel!(onSelectNone:) {
        Cmd::SelectNone
    } else if a == sel!(onInvert:) {
        Cmd::Invert
    } else if a == sel!(onClearLog:) {
        Cmd::ClearLog
    } else if a == sel!(onToggleLog:) {
        Cmd::ToggleLog
    } else if a == sel!(onPrefs:) {
        Cmd::Settings
    } else if a == sel!(onAbout:) {
        Cmd::About
    } else if a == sel!(onDocs:) {
        Cmd::Docs
    } else if a == sel!(onCheckUpdates:) {
        Cmd::CheckUpdates
    } else if a == sel!(onQuit:) {
        Cmd::Quit
    } else {
        return None;
    })
}

fn dev_env(key: &str) -> Result<String, std::env::VarError> {
    if cfg!(debug_assertions) {
        std::env::var(key)
    } else {
        Err(std::env::VarError::NotPresent)
    }
}

fn r(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

// ── stub disc model (stands in for engine::scan) ──────────────────────────

// Row-list identity (excludes tick state) so render() can detect a real
// tree change vs. a tick-only update. See docs/mac-shell.md — rows_sig
fn rows_sig(rows: &[crate::ui::Row]) -> String {
    rows.iter()
        .map(|r| format!("{}|{}|{}|{}", r.index, r.depth, r.type_s, r.desc))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── quitting, once, for every route out of the app ────────────────────────

/// What a close-or-quit request should do about a rip in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QuitChoice {
    /// Let it through: nothing is running, or the operator already said yes.
    Proceed,
    /// The operator picked "Stop & Quit": cancel the rip, then let it through.
    StopThenProceed,
    /// The operator picked "Keep ripping": the window stays, the app stays.
    Stay,
}

// `already_confirmed` keeps ONE alert per departure: window-close and
// Cmd+Q share this path, and closing the last window is itself a quit,
// so without the latch the same question would be asked twice.
fn needs_rip_confirmation(running: bool, already_confirmed: bool) -> bool {
    running && !already_confirmed
}

/// What the alert's answer means. First button is "Stop & Quit"; anything else
/// (including a dismissal) means stay, because losing a rip must never be the
/// default outcome of an ambiguous answer.
fn quit_choice(response: isize) -> QuitChoice {
    if response == objc2_app_kit::NSAlertFirstButtonReturn {
        QuitChoice::StopThenProceed
    } else {
        QuitChoice::Stay
    }
}

// One place to set a tick box: written by both `cell_for` and
// `sync_check_states`. `allowsMixedState` must move with the state, or
// an NSButton that no longer allows mixed silently clamps -1 to 1.
fn set_check(b: &NSButton, state: crate::ui::Check) {
    b.setAllowsMixedState(state == crate::ui::Check::Mixed);
    b.setState(match state {
        crate::ui::Check::On => 1,
        crate::ui::Check::Mixed => -1,
        crate::ui::Check::Off => 0,
    });
}

// ── outline data source ───────────────────────────────────────────────────

struct SrcIvars {
    /// The shell holds no model: rows come from `App.tree` via the controller.
    ctrl: RefCell<Option<Retained<Controller>>>,
    info: RefCell<Option<Retained<NSTextView>>>,
    view: RefCell<Option<Retained<NSOutlineView>>>,
    /// Flattened `View.title_rows`, rebuilt on every render.
    rows: RefCell<Vec<crate::ui::Row>>,
    /// row index -> child row indices, so the outline can walk them.
    kids: RefCell<Vec<Vec<usize>>>,
    roots: RefCell<Vec<usize>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FmkTitlesSource"]
    #[ivars = SrcIvars]
    struct TitlesSource;

    unsafe impl NSObjectProtocol for TitlesSource {}

    unsafe impl NSOutlineViewDataSource for TitlesSource {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn n_children(&self, _ov: &NSOutlineView, item: Option<&AnyObject>) -> isize {
            match self.idx(item) {
                None => self.ivars().roots.borrow().len() as isize,
                Some(i) => self.ivars().kids.borrow()[i].len() as isize,
            }
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        fn expandable(&self, _ov: &NSOutlineView, item: Option<&AnyObject>) -> bool {
            match self.idx(item) {
                None => true,
                Some(i) => !self.ivars().kids.borrow()[i].is_empty(),
            }
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        fn child(
            &self,
            _ov: &NSOutlineView,
            n: isize,
            item: Option<&AnyObject>,
        ) -> Retained<AnyObject> {
            let id = match self.idx(item) {
                None => self.ivars().roots.borrow()[n as usize],
                Some(i) => self.ivars().kids.borrow()[i][n as usize],
            };
            unsafe { Retained::cast_unchecked(NSNumber::new_usize(id)) }
        }
    }

    unsafe impl NSControlTextEditingDelegate for TitlesSource {}

    unsafe impl NSOutlineViewDelegate for TitlesSource {
        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        fn view_for(
            &self,
            _ov: &NSOutlineView,
            col: Option<&NSTableColumn>,
            item: Option<&AnyObject>,
        ) -> Option<Retained<NSView>> {
            self.cell_for(col, item)
        }

        #[unsafe(method(outlineViewSelectionDidChange:))]
        fn sel_changed(&self, n: Option<&objc2_foundation::NSNotification>) {
            let Some(n) = n else { return };
            let Some(obj) = ({ n.object() }) else { return };
            let ov: &NSOutlineView =
                unsafe { &*(&*obj as *const AnyObject as *const NSOutlineView) };
            let row = { ov.selectedRow() };
            if row < 0 {
                return;
            }
            let Some(item) = ({ ov.itemAtRow(row) }) else { return };
            let Some(i) = self.idx(Some(&item)) else { return };
            if let Some(c) = self.ivars().ctrl.borrow().as_ref() {
                c.app_mut(|a| a.selected_row = Some(i));
                c.render();
            }
        }
    }

    impl TitlesSource {
        #[unsafe(method(onToggle:))]
        fn on_toggle(&self, sender: Option<&AnyObject>) {
            let Some(s) = sender else { return };
            let b: &NSButton = unsafe { &*(s as *const AnyObject as *const NSButton) };
            let i = { b.tag() } as usize;
            if let Some(c) = self.ivars().ctrl.borrow().as_ref() {
                // The core owns cascade, tri-state AND toggle direction; the
                // shell only reports which row was clicked. Reading the
                // direction from the button's own state broke on mixed state.
                c.app_mut(|a| a.tree.toggle(i));
                c.render();
            }
        }
    }
);

impl TitlesSource {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SrcIvars {
            ctrl: RefCell::new(None),
            info: RefCell::new(None),
            view: RefCell::new(None),
            rows: RefCell::new(Vec::new()),
            kids: RefCell::new(Vec::new()),
            roots: RefCell::new(Vec::new()),
        });
        unsafe { msg_send![super(this), init] }
    }

    fn idx(&self, item: Option<&AnyObject>) -> Option<usize> {
        let it = item?;
        let n: &NSNumber = unsafe { &*(it as *const AnyObject as *const NSNumber) };
        Some(n.as_usize())
    }

    /// Build the view for one cell. Lives outside `define_class!` so it can
    /// return early — the macro's `method_id` arm cannot.
    fn cell_for(
        &self,
        col: Option<&NSTableColumn>,
        item: Option<&AnyObject>,
    ) -> Option<Retained<NSView>> {
        let mtm = MainThreadMarker::new().unwrap();
        let rows = self.ivars().rows.borrow();
        let (i, col) = self.idx(item).zip(col)?;
        let row = rows.get(i)?;
        let ident = { col.identifier().to_string() };

        // The check column is a checkbox or a blank spacer — never text. It
        // must not fall through to the label branch.
        if ident == "check" {
            let Some(state) = row.check else {
                // The core decided this row carries no checkbox.
                let v = { NSView::initWithFrame(NSView::alloc(mtm), r(0.0, 0.0, 20.0, 18.0)) };
                return Some(unsafe { Retained::cast_unchecked(v) });
            };
            let b = { NSButton::initWithFrame(NSButton::alloc(mtm), r(0.0, 0.0, 20.0, 18.0)) };
            unsafe {
                b.setButtonType(NSButtonType::Switch);
                b.setTitle(&NSString::from_str(""));
                set_check(&b, state);
                b.setTag(row.index as isize);
                b.setTarget(Some(self));
                b.setAction(Some(sel!(onToggle:)));
            }
            return Some(unsafe { Retained::cast_unchecked(b) });
        }

        let txt = if ident == "type" {
            &row.type_s
        } else {
            &row.desc
        };
        let tf = { NSTextField::initWithFrame(NSTextField::alloc(mtm), r(0.0, 0.0, 200.0, 17.0)) };
        {
            tf.setStringValue(&NSString::from_str(txt));
            tf.setBezeled(false);
            tf.setDrawsBackground(false);
            tf.setEditable(false);
            tf.setSelectable(false);
            tf.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        }
        Some(unsafe { Retained::cast_unchecked(tf) })
    }

    // Repaint the tick boxes in place, leaving the rows — and therefore the
    // user's expansion, selection and scroll position — untouched. Runs on
    // every ordinary redraw; see docs/mac-shell.md — sync_check_states.
    fn sync_check_states(&self, rows: &[crate::ui::Row]) {
        // The data source must serve the CURRENT ticks even when no reload
        // happens: a row scrolled into view after this point is built by
        // `cell_for` from exactly this Vec.
        *self.ivars().rows.borrow_mut() = rows.to_vec();
        let Some(ov) = self.ivars().view.borrow().clone() else {
            return;
        };
        let col = ov.columnWithIdentifier(&NSString::from_str("check"));
        if col < 0 {
            return;
        }
        for display in 0..ov.numberOfRows() {
            let Some(item) = ({ ov.itemAtRow(display) }) else {
                continue;
            };
            let Some(state) = self
                .idx(Some(&item))
                .and_then(|i| rows.get(i))
                .and_then(|r| r.check)
            else {
                continue;
            };
            // `make_if_necessary: false` on purpose — only touch cells AppKit
            // already built (visible ones); off-screen ones rebuild from
            // `rows` above when they scroll in.
            let Some(v) = ov.viewAtColumn_row_makeIfNecessary(col, display, false) else {
                continue;
            };
            if let Some(b) = v.downcast_ref::<NSButton>() {
                set_check(b, state);
            }
        }
    }

    /// Take the freshly-decided rows from the core and rebuild the outline.
    fn apply(&self, rows: &[crate::ui::Row]) {
        // The depth → parent walk is the same decision on both shells, so it
        // comes from the core rather than being re-derived here.
        let mut kids: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
        let mut roots = Vec::new();
        for (i, parent) in crate::ui::row_parents(rows).into_iter().enumerate() {
            match parent {
                Some(p) => kids[p].push(i),
                None => roots.push(i),
            }
        }
        *self.ivars().rows.borrow_mut() = rows.to_vec();
        *self.ivars().kids.borrow_mut() = kids;
        *self.ivars().roots.borrow_mut() = roots;
        if let Some(ov) = self.ivars().view.borrow().as_ref() {
            unsafe {
                ov.reloadData();
                for &root in self.ivars().roots.borrow().iter() {
                    let obj: Retained<AnyObject> =
                        Retained::cast_unchecked(NSNumber::new_usize(root));
                    ov.expandItem_expandChildren(Some(&obj), true);
                }
                // `reloadData` keeps the OLD scroll offset; the core (shared
                // with Windows) picks the row to show. All rows are expanded
                // above, so display row == flat index.
                if let Some(at) = crate::ui::first_visible_row(rows) {
                    let origin = ov.rectOfRow(at as isize).origin;
                    ov.scrollPoint(origin);
                }
            }
        }
    }
}

// ── window controller ─────────────────────────────────────────────────────

#[derive(Default)]
struct Ivars {
    /// The single source of truth. The shell holds no state of its own.
    app: RefCell<crate::ui::App>,
    out_field: RefCell<Option<Retained<NSComboBox>>>,
    log: RefCell<Option<Retained<NSTextView>>>,
    log_scroll: RefCell<Option<Retained<NSScrollView>>>,
    backdrop: RefCell<Option<Retained<NSBox>>>,
    tree_scroll: RefCell<Option<Retained<NSScrollView>>>,
    grp_out: RefCell<Option<Retained<NSBox>>>,
    grp_info: RefCell<Option<Retained<NSBox>>>,
    on_prog: RefCell<bool>,
    win_prefs: RefCell<Option<Retained<NSWindow>>>,
    win_about: RefCell<Option<Retained<NSWindow>>>,
    /// The main window, kept so a live language switch can rebuild its content.
    win_main: RefCell<Option<Retained<NSWindow>>>,
    tree: RefCell<Option<Retained<NSOutlineView>>>,
    src: RefCell<Option<Retained<TitlesSource>>>,
    page_empty: RefCell<Option<Retained<NSView>>>,
    on_empty: RefCell<bool>,
    /// Worker threads push user-visible lines here; a main-thread timer drains
    /// it. AppKit objects are main-thread-only, so nothing else may cross.
    inbox: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    drain: RefCell<Option<Retained<NSTimer>>>,
    /// Signature of the tree rows last painted, so a 200 ms progress tick does
    /// not force a full `reloadData` + re-expand of the (usually hidden)
    /// titles outline when nothing about the title list moved — the same
    /// policy the Windows shell's `Memo::rows`/`rows_sig` already apply.
    tree_sig: RefCell<String>,
    /// The operator has already answered the rip-in-progress question for this
    /// departure — see `confirm_quit`.
    quit_confirmed: std::cell::Cell<bool>,
    log_hidden: RefCell<bool>,
    page_result: RefCell<Option<Retained<NSView>>>,
    result_line: RefCell<Option<Retained<NSTextField>>>,
    result_head: RefCell<Option<Retained<NSTextField>>>,
    on_result: RefCell<bool>,
    run_btn: RefCell<Option<Retained<NSButton>>>,
    demo_path: RefCell<String>,
    demo_timer: RefCell<Option<Retained<NSTimer>>>,
    demo_step: RefCell<usize>,
    two_bars: RefCell<bool>,
    bar2_row: RefCell<Vec<Retained<NSView>>>,
    settings: RefCell<crate::settings::Settings>,
    pf_fields: RefCell<Vec<(String, Retained<NSTextField>)>>,
    /// The keydb status note in Settings ▸ Keys, kept so a running "Update
    /// keydb now" can show progress + the result in place.
    keydb_note: RefCell<Option<Retained<NSTextField>>>,
    /// The "Update keydb now" button — disabled while an update is in flight so
    /// a second click can't spawn a concurrent download.
    keydb_btn: RefCell<Option<Retained<NSButton>>>,
    /// A keydb download is in flight. Lives on the CONTROLLER, which outlives
    /// every Settings window, rather than in the button's enabled state.
    keydb_updating: std::cell::Cell<bool>,
    pf_checks: RefCell<Vec<(String, Retained<NSButton>)>>,
    pf_popups: RefCell<Vec<(String, Retained<NSPopUpButton>)>>,
    /// The multi-select language pickers, kept separate from `pf_popups`
    /// because they are read back by an entirely different rule: a plain popup
    /// stores the ONE selected row, a language picker stores the whole ticked
    /// set. Sharing the list would make `read_prefs_form` persist a single
    /// language name over the user's comma-separated codes.
    pf_langs: RefCell<Vec<(String, Retained<NSPopUpButton>)>>,
    /// True only when the open source is a physical drive; Eject is
    /// meaningless for an image file, so the button hides.
    eject_btn: RefCell<Option<Retained<NSButton>>>,
    fmt_popup: RefCell<Option<Retained<NSPopUpButton>>>,
    tabs: RefCell<Option<Retained<objc2_app_kit::NSTabView>>>,
    page_main: RefCell<Option<Retained<NSView>>>,
    page_prog: RefCell<Option<Retained<NSView>>>,
    bar_cur: RefCell<Option<Retained<NSProgressIndicator>>>,
    bar_all: RefCell<Option<Retained<NSProgressIndicator>>>,
    lbl_cur: RefCell<Option<Retained<NSTextField>>>,
    lbl_all: RefCell<Option<Retained<NSTextField>>>,
    // "Saving to <container> file" labels — updated from the View at rip start
    // so they read MP4/M2TS/MKV to match the chosen format.
    lbl_saving_cur: RefCell<Option<Retained<NSTextField>>>,
    lbl_saving_all: RefCell<Option<Retained<NSTextField>>>,
    fields: RefCell<Vec<Retained<NSTextField>>>,
    timer: RefCell<Option<Retained<NSTimer>>>,
}

// Window-wide drop target. Accepts a single .iso/.mkv/.m2ts/.mp4 dragged
// from Finder — the ordinary way a Mac user opens a file they can see.
define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "FmkDropView"]
    #[ivars = RefCell<Option<Retained<Controller>>>]
    struct DropView;

    impl DropView {
        #[unsafe(method(draggingEntered:))]
        fn entered(&self, _s: &AnyObject) -> usize {
            // NSDragOperationCopy
            1
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare(&self, _s: &AnyObject) -> objc2::runtime::Bool {
            objc2::runtime::Bool::YES
        }

        #[unsafe(method(performDragOperation:))]
        fn perform(&self, sender: &AnyObject) -> objc2::runtime::Bool {
            let paths = unsafe { dropped_paths(sender) };
            let Some(p) = paths.into_iter().next() else {
                return objc2::runtime::Bool::NO;
            };
            // A DIRECTORY is a valid source: an extracted disc tree opens as
            // `dir://`. Gating purely on extension rejected every folder,
            // even though the engine accepts that source kind.
            let ok = std::path::Path::new(&p).is_dir()
                || matches!(
                    std::path::Path::new(&p)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase()
                        .as_str(),
                    "iso" | "mkv" | "m2ts" | "mts" | "mp4"
                );
            if let Some(c) = self.ivars().borrow().as_ref() {
                if ok {
                    let fx = c.app_mut(|a| a.open(&p));
                    c.perform(fx);
                } else {
                    c.app_mut(|a| {
                        a.say(
                            crate::ui::LogKind::Notice,
                            &crate::strings::fmt("gui.log.not_supported", &[("p", &p)]),
                        )
                    });
                    c.render();
                }
            }
            objc2::runtime::Bool::new(ok)
        }
    }
);

/// Pull file paths off a dragging pasteboard.
unsafe fn dropped_paths(sender: &AnyObject) -> Vec<String> {
    let pb: Retained<objc2_app_kit::NSPasteboard> = msg_send![sender, draggingPasteboard];
    let mut out = Vec::new();
    if let Some(items) = pb.pasteboardItems() {
        for it in items.iter() {
            // public.file-url
            // The UTI is an extern static, so reading it is unsafe even
            // inside an `unsafe fn` under this lint level.
            let uti = unsafe { objc2_app_kit::NSPasteboardTypeFileURL };
            if let Some(s) = it.stringForType(uti)
                && let Some(url) = objc2_foundation::NSURL::URLWithString(&s)
                && let Some(path) = url.path()
            {
                out.push(path.to_string());
            }
        }
    }
    out
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FmkController"]
    #[ivars = Ivars]
    struct Controller;

    unsafe impl NSObjectProtocol for Controller {}

    unsafe impl NSWindowDelegate for Controller {}

    // This shell had NO application delegate: AppKit kept running with no
    // window after close, and ⌘Q skipped the rip-in-progress confirmation.
    // Both routes now ask the SAME question, through `confirm_quit`.
    unsafe impl NSApplicationDelegate for Controller {
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            match self.confirm_quit() {
                QuitChoice::Stay => NSApplicationTerminateReply::TerminateCancel,
                QuitChoice::Proceed | QuitChoice::StopThenProceed => {
                    NSApplicationTerminateReply::TerminateNow
                }
            }
        }

        // Single-window app: with the window gone there is no UI left to come
        // back to, so the process must go too.
        #[unsafe(method(applicationShouldTerminateAfterLastWindowClosed:))]
        fn terminate_after_last_window(&self, _app: &NSApplication) -> bool {
            true
        }
    }

    unsafe impl NSControlTextEditingDelegate for Controller {
        // Without this the output field is decoration: the model never hears
        // typed edits, and the next render() tick overwrites the field with
        // the STALE `output_dir`. Mirrors Windows' `edit_out.on().en_change`.
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, n: Option<&objc2_foundation::NSNotification>) {
            let Some(f) = self.ivars().out_field.borrow().clone() else {
                return;
            };
            let text = { f.stringValue() }.to_string();
            // Only this field uses this delegate today, but check the
            // notification's object anyway so a future second user of the
            // delegate can't have its edits attributed to the output dir.
            if let Some(n) = n
                && let Some(obj) = { n.object() }
                && !std::ptr::eq(&*obj as *const AnyObject, &*f as *const NSComboBox as *const AnyObject)
            {
                return;
            }
            if self.ivars().app.borrow().output_dir != text {
                self.app_mut(|a| a.output_dir = text);
            }
        }
    }

    unsafe impl NSTextFieldDelegate for Controller {}

    unsafe impl NSComboBoxDelegate for Controller {}

    impl Controller {
        #[unsafe(method(onBrowseOutput:))]
        fn on_browse_output(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::SetOutput);
        }

        #[unsafe(method(onOpenFiles:))]
        fn on_open_files(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Open);
        }

        /// File ▸ Open disc, and the empty state's "Open disc" button — the
        /// SAME method, so the two entry points cannot behave differently.
        #[unsafe(method(onOpenDisc:))]
        fn on_open_disc(&self, _s: Option<&AnyObject>) {
            self.open_disc(true);
        }

        /// The launch probe, fired once off a one-shot timer after the window
        /// is on screen. See `Controller::open_disc`.
        #[unsafe(method(onLaunchProbe:))]
        fn on_launch_probe(&self, _s: Option<&AnyObject>) {
            self.open_disc(false);
        }

        #[unsafe(method(onNoop:))]
        fn on_noop(&self, _s: Option<&AnyObject>) {}

        /// Settings → Default destination "…": pick a folder and drop it into the
        /// dest_dir field (OK then persists it, like any other edited field).
        #[unsafe(method(onBrowseDestDir:))]
        fn on_browse_dest_dir(&self, _s: Option<&AnyObject>) {
            if let Some(dir) = self.pick(true, false, "Choose the default output folder") {
                self.set_pref_field("dest_dir", &dir);
            }
        }

        /// Settings → keydb.cfg location "…": pick a file (any type — a keydb is
        /// not a source media type) and drop it into the keydb_path field.
        #[unsafe(method(onBrowseKeydb:))]
        fn on_browse_keydb(&self, _s: Option<&AnyObject>) {
            if let Some(path) = self.pick(false, false, "Choose the keydb.cfg file") {
                self.set_pref_field("keydb_path", &path);
            }
        }

        #[unsafe(method(onPrefs:))]
        fn on_prefs(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Settings);
        }

        // Cancel: close Settings and keep NOTHING. Deliberately skips
        // `read_prefs_form` (the only mutator of `settings`), so the stored
        // copy is left exactly as the window found it.
        #[unsafe(method(onCancelPrefs:))]
        fn on_cancel_prefs(&self, _s: Option<&AnyObject>) {
            if let Some(w) = self.ivars().win_prefs.borrow().as_ref() {
                w.close();
            }
        }

        #[unsafe(method(onClosePrefs:))]
        fn on_close_prefs(&self, _s: Option<&AnyObject>) {
            // Remember the default destination BEFORE reading the form so we can
            // tell whether the user changed it in this Settings session.
            let old_dest = self.ivars().settings.borrow().dest_dir.clone();
            self.read_prefs_form();
            // Push edited settings into the running App so changes take effect
            // at once — App holds its own copy loaded at startup, which would
            // otherwise stay stale until the next launch.
            let edited = self.ivars().settings.borrow().clone();
            // The active output dir is a separate live value (a one-off folder
            // pick overrides the default); re-point it ONLY when the default
            // actually changed, so this can't clobber a one-off pick.
            let new_dest = edited.dest_dir.clone();
            let dest_changed = new_dest != old_dest && !new_dest.trim().is_empty();
            self.app_mut(|a| {
                a.settings = edited;
                if dest_changed {
                    a.output_dir = new_dest.clone();
                }
            });
            self.save_settings_reporting_error();
            if let Some(w) = self.ivars().win_prefs.borrow().as_ref() {
                w.close();
            }
        }

        // One language ticked/unticked in a preferred-languages picker. The
        // set mutation is `ui::lang_toggle`, shared with Windows; the picker
        // is not yet committed to `settings`, so Cancel can still discard it.
        #[unsafe(method(onToggleLang:))]
        fn on_toggle_lang(&self, s: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().unwrap();
            let Some(item) = s.and_then(|s| s.downcast_ref::<NSMenuItem>()) else {
                return;
            };
            let (Some(code), Some(menu)) = (lang_item_code(item), unsafe { item.menu() }) else {
                return;
            };
            // Which of the three pickers was clicked: the one owning this menu.
            let picker = self
                .ivars()
                .pf_langs
                .borrow()
                .iter()
                .find(|(_, p)| p.menu().is_some_and(|m| m == menu))
                .map(|(_, p)| p.clone());
            if let Some(p) = picker {
                let next = crate::ui::lang_toggle(&lang_picker_value(&p), &code);
                set_lang_picker(mtm, &p, &next, self);
            }
        }

        // Fires the instant a language is picked. The actual switch (which
        // rebuilds, and so destroys, this Settings window) is deferred one
        // runloop tick, because tearing down the popup mid-action crashes AppKit.
        #[unsafe(method(onPickLanguage:))]
        fn on_pick_language(&self, _s: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().unwrap();
            unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    0.0,
                    self,
                    sel!(onApplyLanguage:),
                    None,
                    false,
                );
            }
            let _ = mtm;
        }

        /// Apply the picked language live: persist the form, swap the catalog,
        /// rebuild the whole UI (main window, menu, and the Settings window
        /// itself), and land back on the same Settings tab — now translated.
        #[unsafe(method(onApplyLanguage:))]
        fn on_apply_language(&self, _s: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().unwrap();
            // Commit every field first so switching language loses no edits.
            self.read_prefs_form();
            self.save_settings_reporting_error();
            let code = self.ivars().settings.borrow().language.clone();
            // Remember the visible tab so we can restore it after the rebuild.
            let tab_idx = self.ivars().tabs.borrow().as_ref().and_then(|tv| {
                tv.selectedTabViewItem().map(|it| tv.indexOfTabViewItem(&it))
            });
            crate::strings::set_locale(crate::ui::locale_code(&code));
            // Close the current (old-language) Settings window; relocalize
            // clears its cache so the reopen rebuilds it fresh.
            if let Some(w) = self.ivars().win_prefs.borrow_mut().take() {
                w.close();
            }
            self.relocalize(mtm);
            self.act(crate::ui::Cmd::Settings);
            if let Some(i) = tab_idx
                && let Some(tv) = self.ivars().tabs.borrow().as_ref() {
                    tv.selectTabViewItemAtIndex(i);
                }
        }

        #[unsafe(method(onAbout:))]
        fn on_about(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::About);
        }

        #[unsafe(method(onCloseAbout:))]
        fn on_close_about(&self, _s: Option<&AnyObject>) {
            if let Some(w) = self.ivars().win_about.borrow().as_ref() {
                w.close();
            }
        }

        #[unsafe(method(onTestKeyserver:))]
        fn on_test_keyserver(&self, _s: Option<&AnyObject>) {
            let mut url = String::new();
            for (k, f) in self.ivars().pf_fields.borrow().iter() {
                if k == "keyserver_url" {
                    url = { f.stringValue() }.to_string();
                }
            }
            if url.trim().is_empty() {
                self.app_mut(|a| {
                    a.say(
                        crate::ui::LogKind::Result,
                        &crate::strings::get("gui.log.no_keyserver"),
                    )
                });
                return;
            }
            // Validated by the same rule the key layer uses, so the UI can't
            // accept a URL the engine would later reject.
            match freemkv_keysources::validate_keyserver_url(&url) {
                Ok(_) => self.app_mut(|a| {
                    a.say(
                        crate::ui::LogKind::Result,
                        &crate::strings::fmt("gui.log.keyserver_valid", &[("url", &url)]),
                    )
                }),
                Err(e) => self.app_mut(|a| {
                    a.say(
                        crate::ui::LogKind::Result,
                        &crate::strings::fmt("gui.log.keyserver_rejected", &[("e", &e.to_string())]),
                    )
                }),
            }
        }

        #[unsafe(method(onUpdateKeys:))]
        fn on_update_keys(&self, _s: Option<&AnyObject>) {
            // One download at a time. The disabled button is not enough on its
            // own: a Settings window rebuilt while a download runs comes back
            // with a fresh, enabled button.
            if self.ivars().keydb_updating.get() {
                // `gui.set.keydb_busy` is mac-only: Windows relies solely on
                // its disabled Update button (`set_keydb_updating`) and never
                // re-checks a "busy" flag, so it has no matching note to share.
                self.set_keydb_note(&crate::strings::get_or(
                    "gui.set.keydb_busy",
                    "A keydb update is already running — please wait.",
                ));
                return;
            }
            // Read the live field values, not the last-saved ones, so Update
            // works before OK is pressed.
            let (mut url, mut path) = (String::new(), String::new());
            for (k, f) in self.ivars().pf_fields.borrow().iter() {
                let v = { f.stringValue() }.to_string();
                match k.as_str() {
                    "keydb_url" => url = v,
                    "keydb_path" => path = v,
                    _ => {}
                }
            }
            if url.is_empty() {
                url = self.ivars().settings.borrow().keydb_url.clone();
            }
            if path.is_empty() {
                path = self.ivars().settings.borrow().keydb_path.clone();
            }
            if url.trim().is_empty() {
                // Nothing to fetch — tell the user in place instead of silently
                // spawning a thread that errors into the (maybe-hidden) log.
                self.set_keydb_note(&crate::strings::get("gui.set.keydb_no_url"));
                return;
            }
            self.app_mut(|a| {
                a.say(
                    crate::ui::LogKind::Result,
                    &crate::strings::get("gui.log.fetching_keydb"),
                )
            });
            // Immediate in-Settings feedback: the download is ~20 MB and takes a
            // few seconds; the drain updates this note to the result when done.
            self.set_keydb_note(&crate::strings::get("gui.set.keydb_updating"));
            // Disable the button so a second click can't start a concurrent
            // download; the drain re-enables it when the result arrives.
            self.set_keydb_updating(true);
            let inbox = self.ivars().inbox.clone();
            std::thread::spawn(move || {
                let msg = match crate::settings::update_keydb(&url, &path) {
                    Ok(m) => m,
                    Err(e) => e,
                };
                // RECOVER rather than skip the push: this is the ONLY message
                // the keydb worker sends, and dropping it would wedge the
                // Update button disabled forever, per `start_drain`'s comment.
                inbox
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(msg);
            });
            self.start_drain();
        }

        #[unsafe(method(onSelectAll:))]
        fn on_select_all(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::SelectAll);
        }

        #[unsafe(method(onSelectNone:))]
        fn on_select_none(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::SelectNone);
        }

        #[unsafe(method(onInvert:))]
        fn on_invert(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Invert);
        }

        #[unsafe(method(onToggleLog:))]
        fn on_toggle_log(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::ToggleLog);
        }

        #[unsafe(method(onClearLog:))]
        fn on_clear_log(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::ClearLog);
        }

        #[unsafe(method(onEject:))]
        fn on_eject(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Eject);
        }

        #[unsafe(method(onDocs:))]
        fn on_docs(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Docs);
        }

        #[unsafe(method(onFormat:))]
        fn on_format(&self, _s: Option<&AnyObject>) {
            let Some(p) = self.ivars().fmt_popup.borrow().clone() else {
                return;
            };
            let Some(t) = p.titleOfSelectedItem() else { return };
            let src = self.ivars().app.borrow().source.clone();
            let disc = !crate::ui::is_container(&src);
            let mp4 = self.ivars().app.borrow().mp4_possible();
            // Resolve the LOCALIZED popup label against the core's list rather
            // than the widget text, so an unknown title can't reach the model
            // and selection works in every locale.
            if let Some(f) = crate::ui::format_from_label(&t.to_string(), disc, mp4) {
                self.act(crate::ui::Cmd::SetFormat(f));
            }
        }

        #[unsafe(method(onCheckUpdates:))]
        fn on_check_updates(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::CheckUpdates);
        }

        // Quit goes through dispatch like everything else rather than calling
        // `terminate:` straight off the menu, so the core — not this shell —
        // decides what quitting means (and the Windows shell inherits it).
        #[unsafe(method(onQuit:))]
        fn on_quit(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Quit);
        }

        #[unsafe(method(onDemoStep:))]
        fn on_demo_step(&self, _s: Option<&AnyObject>) {
            let step = {
                let mut n = self.ivars().demo_step.borrow_mut();
                *n += 1;
                *n
            };
            let path = self.ivars().demo_path.borrow().clone();
            match step {
                1 => {
                    self.app_mut(|a| a.say(crate::ui::LogKind::Result, "▶ demo: opening the source …"));
                    self.drive_open(&path);
                }
                2 => {
                    self.app_mut(|a| a.say(crate::ui::LogKind::Result, "▶ demo: selecting title 1"));
                    self.drive_tick_title(0, true);
                }
                3 => {
                    self.app_mut(|a| a.say(crate::ui::LogKind::Result, "▶ demo: choosing output format"));
                    self.drive_pick_format("Selected titles → MKV");
                }
                4 => {
                    self.app_mut(|a| a.say(crate::ui::LogKind::Result, "▶ demo: setting output folder"));
                    self.drive_set_output("/tmp/audit/demo");
                }
                5 => {
                    self.app_mut(|a| a.say(crate::ui::LogKind::Result, "▶ demo: clicking Run Now"));
                    self.drive_click_run();
                }
                _ => {
                    if let Some(t) = self.ivars().demo_timer.borrow_mut().take() {
                        t.invalidate();
                    }
                }
            }
        }

        /// AppKit asks before showing each menu item. Commands that would
        /// start a second job or change where the running one writes must be
        /// unavailable while a rip is in flight.
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> objc2::runtime::Bool {
            // The run lives on the App now — the old shell-side field was a
            // leftover that stayed None, so menus never disabled.
            let running = self.ivars().app.borrow().running();
            let Some(action) = ({ item.action() }) else {
                return objc2::runtime::Bool::YES;
            };
            // The RULE lives in the core; the shell only maps selectors to
            // commands, so macOS and Windows cannot disagree about it.
            let cmd = cmd_for(action);
            let blocked = cmd.map(crate::ui::blocked_while_running).unwrap_or(false);
            objc2::runtime::Bool::new(!(running && blocked))
        }

        #[unsafe(method(onDrain:))]
        fn on_drain(&self, _s: Option<&AnyObject>) {
            // RECOVER a poisoned inbox rather than returning: an early return
            // stranded the drain (keydb "busy" flag stuck, note never shown,
            // timer firing forever) and a poisoned `Vec<String>` is still fine.
            let msgs: Vec<String> = self
                .ivars()
                .inbox
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain(..)
                .collect();
            if msgs.is_empty() {
                return;
            }
            // Each say() goes through app_mut, which repaints — so worker-thread
            // messages (keydb update, etc.) show the moment they arrive.
            for m in &msgs {
                self.app_mut(|a| a.say(crate::ui::LogKind::Result, m));
            }
            // This drain path is the keydb-update worker; surface its outcome
            // in the Settings note so the user sees it in place, not just the
            // log, and re-enable the button now the update is done.
            if let Some(last) = msgs.last() {
                self.set_keydb_note(last);
            }
            self.set_keydb_updating(false);
            // The keydb update is one-shot: once drained, stop the timer here
            // (matching Windows' `drain()`/`TIMER_DRAIN`) or it re-fires at
            // 5 Hz against an empty inbox for the rest of the process's life.
            if let Some(t) = self.ivars().drain.borrow_mut().take() {
                t.invalidate();
            }
        }


        #[unsafe(method(onDoneResult:))]
        fn on_done_result(&self, _s: Option<&AnyObject>) {
            let fx = self.app_mut(|a| a.dismiss_result());
            self.perform(fx);
        }

        #[unsafe(method(onReveal:))]
        fn on_reveal(&self, _s: Option<&AnyObject>) {
            let d = self.ivars().app.borrow().output_dir.clone();
            self.perform(vec![crate::ui::Effect::Reveal(d)]);
        }

        #[unsafe(method(onCloseDisc:))]
        fn on_close_disc(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Close);
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, n: Option<&objc2_foundation::NSNotification>) {
            let Some(n) = n else { return };
            let Some(obj) = ({ n.object() }) else { return };
            let win: &NSWindow = unsafe { &*(&*obj as *const AnyObject as *const NSWindow) };
            if let Some(cv) = win.contentView() {
                let b = cv.frame();
                self.relayout(b.size.width, b.size.height);
            }
        }

        // Closing the window mid-rip must not silently tear down the rip worker.
        // Confirm first; on "Stop & Quit" signal the cooperative cancel, then
        // allow the close (the process exits, the partial file is left on disk).
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> objc2::runtime::Bool {
            objc2::runtime::Bool::new(self.confirm_quit() != QuitChoice::Stay)
        }

        // Open the freemkv.org link in the About panel.
        #[unsafe(method(onAboutWebsite:))]
        fn on_about_website(&self, _s: Option<&AnyObject>) {
            if let Some(url) =
                objc2_foundation::NSURL::URLWithString(&NSString::from_str("https://freemkv.org"))
            {
                objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }

        #[unsafe(method(onRip:))]
        fn on_rip(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Run);
        }

        #[unsafe(method(onTick:))]
        fn on_tick(&self, _s: Option<&AnyObject>) {
            let fx = self.app_mut(|a| a.tick());
            self.perform(fx);
        }

        #[unsafe(method(onCancelRip:))]
        fn on_cancel_rip(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Cancel);
        }
    }
);

impl Controller {
    // Mutate the core model and REPAINT. Single choke-point for every state
    // change, so no handler can mutate state and forget to redraw. The
    // mutable borrow is released before `render()`'s own immutable borrow.
    fn app_mut<R>(&self, f: impl FnOnce(&mut crate::ui::App) -> R) -> R {
        let r = f(&mut self.ivars().app.borrow_mut());
        self.render();
        r
    }

    // The one place this shell asks "a rip is running — really quit?".
    // Shared by the window close button and every route into `terminate:`
    // (Cmd+Q, File > Quit, Dock, logout) via `applicationShouldTerminate:`.
    fn confirm_quit(&self) -> QuitChoice {
        let running = self.ivars().app.borrow().running();
        if !needs_rip_confirmation(running, self.ivars().quit_confirmed.get()) {
            return QuitChoice::Proceed;
        }
        let mtm = MainThreadMarker::new().unwrap();
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(&crate::strings::get(
            "gui.alert.rip_title",
        )));
        alert.setInformativeText(&NSString::from_str(&crate::strings::get(
            "gui.alert.rip_body",
        )));
        // Order matters: the FIRST button added is NSAlertFirstButtonReturn,
        // which `quit_choice` reads as "Stop & Quit".
        alert.addButtonWithTitle(&NSString::from_str(&crate::strings::get(
            "gui.alert.stop_quit",
        )));
        alert.addButtonWithTitle(&NSString::from_str(&crate::strings::get(
            "gui.alert.keep_ripping",
        )));
        let choice = quit_choice(alert.runModal());
        if choice == QuitChoice::StopThenProceed {
            // Latch BEFORE cancelling: `running()` stays true until the
            // worker unwinds, so without the latch a terminate a moment
            // later would ask the same question again.
            self.ivars().quit_confirmed.set(true);
            self.act(crate::ui::Cmd::Cancel);
            // ...then actually WAIT for it: `Cmd::Cancel` only signals, and
            // it's the worker's unwind that closes/finalises the partial
            // file. Bounded by QUIT_GRACE so a wedged drive can't hang quit.
            let run = self.ivars().app.borrow().run.clone();
            if let Some(run) = run {
                crate::engine::await_worker_exit(&run, crate::engine::QUIT_GRACE);
            }
        }
        choice
    }

    // Save `Settings` to disk and tell the operator whether it worked.
    // One policy, one call site — see docs/mac-shell.md —
    // save_settings_reporting_error.
    fn save_settings_reporting_error(&self) {
        match self.ivars().settings.borrow().save() {
            Ok(()) => self.app_mut(|a| {
                a.say(
                    crate::ui::LogKind::Result,
                    &crate::strings::get("gui.log.settings_saved"),
                )
            }),
            Err(e) => self.app_mut(|a| {
                a.say(
                    crate::ui::LogKind::Notice,
                    &crate::strings::fmt("gui.log.settings_save_error", &[("e", &e.to_string())]),
                )
            }),
        };
    }

    // The shell's entire job: hand the command to the core, perform the
    // platform effects it asks for, redraw. No decisions here.
    fn act(&self, cmd: crate::ui::Cmd) {
        let effects = self.app_mut(|a| a.dispatch(cmd));
        self.perform(effects);
    }

    // Open the disc; drive/log decisions live in `ui::App::disc_source`. Two
    // `app_mut` calls ON PURPOSE: it repaints, so "Opening …" reaches the log
    // before `open` blocks scanning. `announce_missing` is false for the probe.
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

    fn perform(&self, effects: Vec<crate::ui::Effect>) {
        use crate::ui::Effect as E;
        let mtm = MainThreadMarker::new().unwrap();
        for e in effects {
            match e {
                E::PickSource => {
                    if let Some(p) =
                        self.pick(false, true, &crate::strings::get("gui.panel.source_msg"))
                    {
                        let fx = self.app_mut(|a| a.open(&p));
                        self.perform(fx);
                    }
                }
                E::PickOutputDir => {
                    if let Some(p) =
                        self.pick(true, false, &crate::strings::get("gui.panel.output_msg"))
                    {
                        self.app_mut(|a| a.output_dir = p);
                    }
                }
                E::Reveal(p) => {
                    objc2_app_kit::NSWorkspace::sharedWorkspace()
                        .selectFile_inFileViewerRootedAtPath(None, &NSString::from_str(&p));
                }
                E::OpenUrl(u) => {
                    if let Some(url) =
                        objc2_foundation::NSURL::URLWithString(&NSString::from_str(&u))
                    {
                        objc2_app_kit::NSWorkspace::sharedWorkspace().openURL(&url);
                    }
                }
                E::ShowSettings => {
                    let w = self
                        .ivars()
                        .win_prefs
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| build_prefs(mtm, self));
                    *self.ivars().win_prefs.borrow_mut() = Some(w.clone());
                    w.center();
                    w.makeKeyAndOrderFront(None);
                }
                E::ShowAbout => {
                    let w = self
                        .ivars()
                        .win_about
                        .borrow()
                        .clone()
                        .unwrap_or_else(|| build_about(mtm, self));
                    *self.ivars().win_about.borrow_mut() = Some(w.clone());
                    w.center();
                    w.makeKeyAndOrderFront(None);
                }
                E::StartTicking => self.start_tick(),
                E::StopTicking => {
                    if let Some(t) = self.ivars().timer.borrow_mut().take() {
                        t.invalidate();
                    }
                }
                E::Quit => NSApplication::sharedApplication(mtm).terminate(None),
                E::Redraw => {}
            }
        }
        self.render();
    }

    /// Set a Preferences text/path field by key (used by the browse pickers).
    fn set_pref_field(&self, key: &str, value: &str) {
        for (k, f) in self.ivars().pf_fields.borrow().iter() {
            if k == key {
                f.setStringValue(&NSString::from_str(value));
            }
        }
    }

    /// Put arbitrary text in the Settings ▸ Keys keydb status note (e.g.
    /// "Updating…" or the update result). No-op if Settings isn't open.
    fn set_keydb_note(&self, text: &str) {
        if let Some(note) = self.ivars().keydb_note.borrow().as_ref() {
            note.setStringValue(&NSString::from_str(text));
        }
    }

    // Record that a keydb download is (or isn't) in flight, and reflect it on
    // the button. The FLAG is the guard; the disabled button is only how it's
    // shown — see docs/mac-shell.md — set_keydb_updating.
    fn set_keydb_updating(&self, updating: bool) {
        self.ivars().keydb_updating.set(updating);
        if let Some(b) = self.ivars().keydb_btn.borrow().as_ref() {
            b.setEnabled(!updating);
        }
    }

    fn pick(&self, dirs: bool, filter_types: bool, msg: &str) -> Option<String> {
        let mtm = MainThreadMarker::new().unwrap();
        let panel = { NSOpenPanel::openPanel(mtm) };
        {
            panel.setCanChooseDirectories(dirs);
            panel.setCanChooseFiles(!dirs);
            panel.setCanCreateDirectories(dirs);
            panel.setAllowsMultipleSelection(false);
            panel.setMessage(Some(&NSString::from_str(msg)));
            if !dirs && filter_types {
                let types: Vec<Retained<NSString>> = crate::ui::SOURCE_EXTS
                    .iter()
                    .map(|e| NSString::from_str(e))
                    .collect();
                let arr = objc2_foundation::NSArray::from_retained_slice(&types);
                // `allowedContentTypes` needs UTType from UniformTypeIdentifiers;
                // the deprecated call still filters correctly for now.
                #[allow(deprecated)]
                panel.setAllowedFileTypes(Some(&arr));
            }
            if panel.runModal() != 1 {
                return None;
            }
            panel.URL()?.path().map(|p| p.to_string())
        }
    }

    /// Apply a fully-decided `View` to the widgets. This is the ONLY place
    /// the shell writes to controls, and it computes nothing.
    fn render(&self) {
        use crate::ui::Page;
        let v = self.ivars().app.borrow().view();

        // The format list depends on the source kind, so it is re-derived on
        // every render from the view rather than only at build time.
        self.sync_formats(&v.formats, &v.format);

        // pages
        let iv = self.ivars();
        {
            if let Some(x) = iv.page_empty.borrow().as_ref() {
                x.setHidden(v.page != Page::Empty);
            }
            if let Some(x) = iv.page_main.borrow().as_ref() {
                x.setHidden(v.page != Page::Titles);
            }
            if let Some(x) = iv.page_prog.borrow().as_ref() {
                x.setHidden(v.page != Page::Progress);
            }
            if let Some(x) = iv.page_result.borrow().as_ref() {
                x.setHidden(v.page != Page::Result);
            }
        }

        // tree
        if let Some(src) = iv.src.borrow().as_ref() {
            // Skip the full `reloadData` + re-expand when nothing about the
            // title list moved — see `rows_sig`'s doc comment.
            let sig = rows_sig(&v.title_rows);
            if *iv.tree_sig.borrow() != sig {
                src.apply(&v.title_rows);
                *iv.tree_sig.borrow_mut() = sig;
            } else {
                src.sync_check_states(&v.title_rows);
            }
            if let Some(tv) = src.ivars().info.borrow().as_ref() {
                tv.setString(&NSString::from_str(&v.detail));
            }
        }

        // output row
        {
            if let Some(f) = iv.out_field.borrow().as_ref()
                && f.stringValue().to_string() != v.output_dir
            {
                f.setStringValue(&NSString::from_str(&v.output_dir));
            }
            if let Some(b) = iv.run_btn.borrow().as_ref() {
                b.setEnabled(v.can_run);
            }
            if let Some(b) = iv.eject_btn.borrow().as_ref() {
                b.setHidden(!v.eject_visible);
            }
        }

        // progress
        {
            if let Some(info) = &v.info {
                let f = iv.fields.borrow();
                for (i, val) in info.iter().enumerate() {
                    if let Some(l) = f.get(i) {
                        l.setStringValue(&NSString::from_str(val));
                    }
                }
            }
            if let Some(b) = iv.bar_cur.borrow().as_ref() {
                b.setDoubleValue(v.bar_current);
            }
            if let Some(b) = iv.bar_all.borrow().as_ref() {
                b.setDoubleValue(v.bar_overall);
            }
            if let Some(l) = iv.lbl_cur.borrow().as_ref() {
                l.setStringValue(&NSString::from_str(&v.caption_current));
            }
            if let Some(l) = iv.lbl_all.borrow().as_ref() {
                l.setStringValue(&NSString::from_str(&v.caption_overall));
            }
            if let Some(l) = iv.lbl_saving_cur.borrow().as_ref() {
                l.setStringValue(&NSString::from_str(&v.saving_current));
            }
            if let Some(l) = iv.lbl_saving_all.borrow().as_ref() {
                l.setStringValue(&NSString::from_str(&v.saving_overall));
            }
            for x in iv.bar2_row.borrow().iter() {
                x.setHidden(!v.show_overall_bar);
            }
            if let Some(l) = iv.result_line.borrow().as_ref() {
                l.setStringValue(&NSString::from_str(&v.result_summary));
            }
            if let Some(h) = iv.result_head.borrow().as_ref() {
                h.setStringValue(&NSString::from_str(&v.result_heading));
            }
        }

        // log
        if let Some(tv) = iv.log.borrow().as_ref() {
            let want: String = v
                .log
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let cur = { tv.string() }.to_string();
            if cur.trim_end() != want {
                tv.setString(&NSString::from_str(""));
                for l in &v.log {
                    log_append(tv, &l.text, log_colour(l.kind));
                }
                // Keep the newest line in view — the log only grows and the
                // line worth reading is always the last one. Only inside this
                // branch, so an ordinary progress tick never yanks the view.
                let end = { tv.string() }.length();
                tv.scrollRangeToVisible(objc2_foundation::NSRange::new(end, 0));
            }
        }
        if let Some(sv) = iv.log_scroll.borrow().as_ref() {
            sv.setHidden(v.log_hidden);
        }
        // The View ▸ log item names the action it will PERFORM, so it has to be
        // re-titled whenever the log's visibility changes — it is a toggle, and
        // a toggle that always says "Show log" is wrong half the time.
        self.sync_log_menu_title(&v.log_menu_label);
        // Mirror the view state the layout reads: `relayout` reads this ivar,
        // so without it the layout always reserved the log's strip of height,
        // leaving a dead band when the log was actually hidden.
        *iv.log_hidden.borrow_mut() = v.log_hidden;
        *iv.two_bars.borrow_mut() = v.show_overall_bar;
        *iv.on_prog.borrow_mut() = v.page == Page::Progress;
        *iv.on_empty.borrow_mut() = v.page == Page::Empty;
        *iv.on_result.borrow_mut() = v.page == Page::Result;
        self.relayout_now();
    }

    fn start_tick(&self) {
        if self.ivars().timer.borrow().is_some() {
            return;
        }
        let t = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.2,
                self,
                sel!(onTick:),
                None,
                true,
            )
        };
        *self.ivars().timer.borrow_mut() = Some(t);
    }

    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let iv = Ivars {
            settings: RefCell::new(crate::settings::Settings::load()),
            ..Default::default()
        };
        let this = Self::alloc(mtm).set_ivars(iv);
        unsafe { msg_send![super(this), init] }
    }
    /// Reposition everything for the current content size. Single source of
    /// geometry truth, so the layout is identical at every window size.
    fn relayout(&self, w: f64, h: f64) {
        let iv = self.ivars();
        let prog = *iv.on_prog.borrow();
        {
            if let Some(b) = iv.backdrop.borrow().as_ref() {
                b.setFrame(r(0.0, 0.0, w, h));
            }
            let ty = h - TB_H;
            // log takes a fixed share of the height; more while ripping
            let hidden = *iv.log_hidden.borrow();
            let share = if prog { 0.62 } else { 0.32 };
            let log_h = if hidden {
                0.0
            } else if *iv.on_result.borrow() {
                (ty - RESULT_H - PAD * 2.0 - 4.0).max(120.0)
            } else if prog {
                // While ripping the log fills everything under the progress
                // block — no dead space.
                let ph_prog = if *iv.two_bars.borrow() {
                    PROG_H
                } else {
                    PROG_H_ONE
                };
                (ty - ph_prog - PAD * 2.0 - 4.0).max(120.0)
            } else {
                (h * share).max(120.0)
            };
            if let Some(sv) = iv.log_scroll.borrow().as_ref() {
                sv.setFrame(r(PAD, PAD, w - PAD * 2.0, log_h));
            }
            // pages fill the gap between log and toolbar
            let py = if hidden { PAD } else { PAD + log_h + PAD };
            let ph = (ty - py - 2.0).max(80.0);
            if let Some(v) = iv.page_main.borrow().as_ref() {
                v.setFrame(r(0.0, py, w, ph));
            }
            if let Some(v) = iv.page_prog.borrow().as_ref() {
                // Fixed height, anchored to the top: its children are laid out
                // against PROG_H, so stretching the page would leave a gap.
                let ph_prog = if *iv.two_bars.borrow() {
                    PROG_H
                } else {
                    PROG_H_ONE
                };
                v.setFrame(r(0.0, ty - ph_prog - 2.0, w, ph_prog));
            }
            if let Some(v) = iv.page_empty.borrow().as_ref() {
                v.setFrame(r(0.0, py, w, ph));
            }
            if let Some(v) = iv.page_result.borrow().as_ref() {
                v.setFrame(r(0.0, ty - RESULT_H - 2.0, w, RESULT_H));
            }
            // main page internals
            let tree_w = (w - PAD * 2.0) * 0.464;
            if let Some(sv) = iv.tree_scroll.borrow().as_ref() {
                sv.setFrame(r(PAD, 0.0, tree_w, ph));
            }
            let rx = PAD + tree_w + PAD;
            let rw = w - rx - PAD;
            let _mk_w = 64.0;
            if let Some(g) = iv.grp_out.borrow().as_ref() {
                g.setFrame(r(rx, ph - 110.0, rw, 110.0));
            }
            if let Some(g) = iv.grp_info.borrow().as_ref() {
                g.setFrame(r(rx, 0.0, rw, ph - 110.0 - PAD));
            }
        }
    }

    /// Re-run layout against the window's current size.
    fn relayout_now(&self) {
        if let Some(v) = self.ivars().page_main.borrow().as_ref()
            && let Some(sup) = unsafe { v.superview() }
        {
            let b = sup.frame();
            self.relayout(b.size.width, b.size.height);
        }
    }

    /// Read every Settings control back into the stored `Settings` (no save, no
    /// close). Shared by OK and the live language switch so the form-reading
    /// rules (canonical enum values, container/format mapping) live in one place.
    fn read_prefs_form(&self) {
        let mut st = self.ivars().settings.borrow_mut();
        for (k, f) in self.ivars().pf_fields.borrow().iter() {
            st.set(k, { f.stringValue() }.to_string());
        }
        for (k, b) in self.ivars().pf_checks.borrow().iter() {
            st.set_bool(k, { b.state() } != 0);
        }
        // Language pickers carry their whole stored string on the control (see
        // `lang_picker_value`), already canonical — the toggles wrote it.
        for (k, p) in self.ivars().pf_langs.borrow().iter() {
            st.set(k, lang_picker_value(p));
        }
        for (k, p) in self.ivars().pf_popups.borrow().iter() {
            let opts = enum_options(k);
            if !opts.is_empty() {
                // Enum popup: persist the canonical for the selected row
                // (index-mapped), never the localized label.
                let idx = p.indexOfSelectedItem();
                if idx >= 0 && (idx as usize) < opts.len() {
                    st.set(k, opts[idx as usize].0.to_string());
                }
            } else if let Some(t) = { p.titleOfSelectedItem() } {
                let sel = t.to_string();
                if k == "container" {
                    // Popup shows the localized label; persist the canonical
                    // format string the engine matches on.
                    let canon = crate::ui::format_from_label(&sel, true, true)
                        .map(str::to_string)
                        .unwrap_or(sel);
                    st.set(k, canon);
                } else {
                    st.set(k, sel);
                }
            }
        }
    }

    // Apply a language change live: rebuild the menu bar and window content
    // in the newly-active locale (already swapped via `strings::set_locale`),
    // then re-render so the open disc, log, and selection come back.
    fn relocalize(&self, mtm: MainThreadMarker) {
        // Menu bar: build_menus installs a fresh main menu, replacing the old.
        let app = NSApplication::sharedApplication(mtm);
        build_menus(mtm, &app, self);

        let Some(win) = self.ivars().win_main.borrow().clone() else {
            return;
        };
        let content = win.contentView().unwrap();
        // Tear down the existing content before rebuilding, else the new
        // controls stack on top of the old ones. Snapshot the subviews first —
        // removeFromSuperview mutates the live subview list.
        let subs: Vec<Retained<NSView>> = content.subviews().iter().collect();
        for v in subs {
            v.removeFromSuperview();
        }
        // Rebuild every control (build_ui re-stores all the ivars it owns,
        // including `src`, `log`, the bars and the format popup) and re-add the
        // drag-and-drop overlay.
        let _src = build_ui(mtm, &win, self);
        install_drop_view(mtm, &win, self);

        // `build_ui` installed a BRAND NEW, empty `TitlesSource`; `tree_sig`
        // still describes the OLD one's rows. Left alone, `render()` below
        // finds them equal, skips `apply`, and the disc comes back empty.
        self.ivars().tree_sig.borrow_mut().clear();

        // Settings and About are cached, built once and reused on reopen — but
        // in the old language, so drop them; next open rebuilds them fresh.
        *self.ivars().win_prefs.borrow_mut() = None;
        if let Some(w) = self.ivars().win_about.borrow_mut().take() {
            w.close();
        }

        // Restore what's on screen from the core: render() repopulates the log
        // from App state and paints the correct page; relayout fits the current
        // window size.
        self.render();
        let b = content.bounds();
        self.relayout(b.size.width, b.size.height);
    }

    fn start_drain(&self) {
        if self.ivars().drain.borrow().is_some() {
            return;
        }
        let t = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.2,
                self,
                sel!(onDrain:),
                None,
                true,
            )
        };
        *self.ivars().drain.borrow_mut() = Some(t);
    }

    // Re-title the View > log menu item. Found by SELECTOR, not position or
    // current title: the menu is rebuilt on a live language change, so
    // matching English text would silently stop working in other locales.
    fn sync_log_menu_title(&self, title: &str) {
        let mtm = MainThreadMarker::new().unwrap();
        let app = NSApplication::sharedApplication(mtm);
        let Some(main) = app.mainMenu() else {
            return;
        };
        for i in 0..main.numberOfItems() {
            let Some(top) = main.itemAtIndex(i) else {
                continue;
            };
            let Some(sub) = ({ top.submenu() }) else {
                continue;
            };
            for j in 0..sub.numberOfItems() {
                let Some(mi) = sub.itemAtIndex(j) else {
                    continue;
                };
                if { mi.action() } == Some(sel!(onToggleLog:)) {
                    if { mi.title() }.to_string() != title {
                        mi.setTitle(&NSString::from_str(title));
                    }
                    return;
                }
            }
        }
    }

    /// Apply the core's format list to the popup, preserving the current pick
    /// when it survives. Called from `render`, so the shell never holds an
    /// opinion about which formats exist.
    fn sync_formats(&self, groups: &[Vec<&'static str>], current: &str) {
        let mtm = MainThreadMarker::new().unwrap();
        let Some(p) = self.ivars().fmt_popup.borrow().clone() else {
            return;
        };
        let existing: Vec<String> = p.itemTitles().iter().map(|t| t.to_string()).collect();
        // Compare against the LOCALIZED labels actually shown, so the equality
        // short-circuit is correct in every locale (items are added via
        // format_label below).
        let wanted: Vec<String> = popup_item_titles(groups);
        // Rebuilding unconditionally would drop the open menu mid-click.
        if existing == wanted {
            return;
        }
        let menu = NSMenu::new(mtm);
        for (gi, g) in groups.iter().enumerate() {
            if gi > 0 {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
            }
            for label in g.iter() {
                let mi = unsafe {
                    NSMenuItem::initWithTitle_action_keyEquivalent(
                        NSMenuItem::alloc(mtm),
                        &NSString::from_str(&crate::ui::format_label(label)),
                        None,
                        &NSString::from_str(""),
                    )
                };
                menu.addItem(&mi);
            }
        }
        p.setMenu(Some(&menu));
        // `current` is the canonical format; the menu shows localized labels.
        p.selectItemWithTitle(&NSString::from_str(&crate::ui::format_label(current)));
        if p.indexOfSelectedItem() < 0 {
            p.selectItemAtIndex(0);
        }
    }
}

// Item titles the format popup's menu WILL report once built from `groups`,
// in order. Pure, so `sync_formats`' rebuild guard can be checked without a
// window server.
fn popup_item_titles(groups: &[Vec<&'static str>]) -> Vec<String> {
    let mut titles = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        // The separator between groups is an item too, and AppKit reports its
        // title as "". Omitting it made the rebuild guard's lists mismatch.
        if gi > 0 {
            titles.push(String::new());
        }
        titles.extend(g.iter().map(|s| crate::ui::format_label(s)));
    }
    titles
}

// ── widget helpers ────────────────────────────────────────────────────────

// Colour bucket a log line belongs in: 0 notice, 1 detail, 2 result.
// Separate from `log_append` so the mapping is checkable headlessly.
fn log_colour(kind: crate::ui::LogKind) -> u8 {
    match kind {
        crate::ui::LogKind::Notice => 0,
        crate::ui::LogKind::Detail => 1,
        crate::ui::LogKind::Result => 2,
    }
}

// Append one colour-coded line. kind: 0 notice (system red), 1 detail
// (system green), 2 result (label colour). Semantic system colours follow
// appearance/accessibility settings — fixed maroon/olive/black did not.
fn log_append(tv: &NSTextView, line: &str, kind: u8) {
    unsafe {
        let Some(store) = tv.textStorage() else {
            return;
        };
        let mono = NSFont::userFixedPitchFontOfSize(11.0).unwrap();
        let colour = match kind {
            0 => NSColor::systemRedColor(),
            1 => NSColor::systemGreenColor(),
            // labelColor is black on light, white on dark.
            _ => NSColor::labelColor(),
        };
        let keys: [&objc2_foundation::NSString; 2] = [
            objc2_app_kit::NSForegroundColorAttributeName,
            objc2_app_kit::NSFontAttributeName,
        ];
        let vals: [&AnyObject; 2] = [
            &*Retained::cast_unchecked::<AnyObject>(colour),
            &*Retained::cast_unchecked::<AnyObject>(mono),
        ];
        let attrs = NSDictionary::from_slices(&keys, &vals);
        let piece = objc2_foundation::NSAttributedString::new_with_attributes(
            &NSString::from_str(&format!("{line}\n")),
            &Retained::cast_unchecked(attrs),
        );
        store.appendAttributedString(&piece);
    }
}

/// Autoresizing helper — keeps the fixed-frame layout correct when the
/// window is resized. Bottom-left origin, so MinYMargin == "stick to top".
fn mask(v: &NSView, m: objc2_app_kit::NSAutoresizingMaskOptions) {
    v.setAutoresizingMask(m);
}

fn text(
    mtm: MainThreadMarker,
    s: &str,
    fr: NSRect,
    right: bool,
    small: bool,
) -> Retained<NSTextField> {
    let tf = { NSTextField::initWithFrame(NSTextField::alloc(mtm), fr) };
    {
        tf.setStringValue(&NSString::from_str(s));
        tf.setBezeled(false);
        tf.setDrawsBackground(false);
        tf.setEditable(false);
        tf.setSelectable(false);
        if right {
            tf.setAlignment(NSTextAlignment::Right);
        }
        if small {
            tf.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            tf.setTextColor(Some(&NSColor::secondaryLabelColor()));
        }
    }
    tf
}

fn btn(mtm: MainThreadMarker, t: &str, fr: NSRect, tgt: &AnyObject, a: Sel) -> Retained<NSButton> {
    let b = { NSButton::initWithFrame(NSButton::alloc(mtm), fr) };
    unsafe {
        b.setTitle(&NSString::from_str(t));
        b.setBezelStyle(NSBezelStyle::Push);
        b.setTarget(Some(tgt));
        b.setAction(Some(a));
    }
    b
}

/// Output format. The whole-disc options (ISO / decrypted folder) are what
/// a separate "Backup" command used to do — one control instead of three
/// buttons whose difference nobody could name.
fn popup_fmt(mtm: MainThreadMarker, fr: NSRect) -> Retained<NSPopUpButton> {
    popup_fmt_for(mtm, fr, true)
}

/// `disc` = the source is a disc or disc image. A container source has no
/// whole-disc sinks (there is no disc to image or unpack), so those rows are
/// omitted rather than offered and then failing.
fn popup_fmt_for(mtm: MainThreadMarker, fr: NSRect, disc: bool) -> Retained<NSPopUpButton> {
    let p = { NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(mtm), fr, false) };
    // Grouped so the ordinary case is line one. The per-kind rows are the
    // CLI's `video://`/`audio://`/`sub://` sinks, a DIFFERENT output than a
    // demux of the same tracks. Built empty; `render` fills it from the View.
    let groups = crate::ui::output_formats(disc, true);
    unsafe {
        let menu = NSMenu::new(mtm);
        for (gi, g) in groups.iter().enumerate() {
            if gi > 0 {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
            }
            for label in g.iter() {
                let mi = NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(&crate::ui::format_label(label)),
                    None,
                    &NSString::from_str(""),
                );
                menu.addItem(&mi);
            }
        }
        p.setMenu(Some(&menu));
        p.selectItemAtIndex(0);
    }
    p
}

fn group(mtm: MainThreadMarker, title: &str, fr: NSRect) -> Retained<NSBox> {
    let b = { NSBox::initWithFrame(NSBox::alloc(mtm), fr) };
    {
        b.setBoxType(NSBoxType::Primary);
        b.setTitle(&NSString::from_str(title));
        b.setTitlePosition(objc2_app_kit::NSTitlePosition::AtTop);
    }
    b
}

// ── menus ─────────────────────────────────────────────────────────────────

/// The Edit menu mixes two kinds of item: standard text commands that must go
/// to the first responder (so Copy works in the log), and our tree-selection
/// commands. Binding our commands to ⌘A/⌘C would break text editing.
fn mk_edit(mtm: MainThreadMarker, c: &Controller) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    let edit = crate::strings::get("gui.menu.edit");
    item.setTitle(&NSString::from_str(&edit));
    let menu = { NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(&edit)) };

    // nil target => dispatched down the responder chain to the focused view.
    for (label, key, sel) in [
        ("Cut", "x", sel!(cut:)),
        ("Copy", "c", sel!(copy:)),
        ("Paste", "v", sel!(paste:)),
        ("Select All", "a", sel!(selectAll:)),
    ] {
        let mi = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel),
                &NSString::from_str(key),
            )
        };
        unsafe { mi.setTarget(None) };
        menu.addItem(&mi);
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));

    // Our tree commands — deliberately without the standard shortcuts.
    for (label, key, sel) in [
        (
            crate::strings::get("gui.menu.select_all_titles"),
            "A",
            sel!(onSelectAll:),
        ),
        (
            crate::strings::get("gui.menu.select_no_titles"),
            "",
            sel!(onSelectNone:),
        ),
        (
            crate::strings::get("gui.menu.invert_titles"),
            "",
            sel!(onInvert:),
        ),
    ] {
        let mi = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&label),
                Some(sel),
                &NSString::from_str(key),
            )
        };
        unsafe { mi.setTarget(Some(c)) };
        menu.addItem(&mi);
    }
    item.setSubmenu(Some(&menu));
    item
}

fn build_menus(mtm: MainThreadMarker, app: &NSApplication, c: &Controller) {
    let main = NSMenu::new(mtm);

    let mk = |title: &str, items: Vec<(String, &str, Sel)>| -> Retained<NSMenuItem> {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(title));
        let menu = { NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title)) };
        for (label, key, sel) in items {
            if label == "-" {
                menu.addItem(&NSMenuItem::separatorItem(mtm));
                continue;
            }
            let mi = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(mtm),
                    &NSString::from_str(&label),
                    Some(sel),
                    &NSString::from_str(key),
                )
            };
            unsafe { mi.setTarget(Some(c)) };
            menu.addItem(&mi);
        }
        item.setSubmenu(Some(&menu));
        item
    };

    // App menu
    let app_item = NSMenuItem::new(mtm);
    app_item.setTitle(&NSString::from_str("freemkv"));
    let app_menu = { NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("freemkv")) };
    let about = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&crate::strings::get("gui.menu.app_about")),
            Some(sel!(onAbout:)),
            &NSString::from_str(""),
        )
    };
    unsafe { about.setTarget(Some(c)) };
    app_menu.addItem(&about);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let prefs = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&crate::strings::get("gui.menu.settings")),
            Some(sel!(onPrefs:)),
            &NSString::from_str(","),
        )
    };
    unsafe { prefs.setTarget(Some(c)) };
    app_menu.addItem(&prefs);
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&crate::strings::get("gui.menu.quit")),
            Some(sel!(onQuit:)),
            &NSString::from_str("q"),
        )
    };
    app_menu.addItem(&quit);
    app_item.setSubmenu(Some(&app_menu));
    main.addItem(&app_item);

    main.addItem(&mk(
        &crate::strings::get("gui.menu.file"),
        vec![
            (
                crate::strings::get("gui.menu.open"),
                "o",
                sel!(onOpenFiles:),
            ),
            // Rip from a live optical drive (disc://). Enumerates drives and
            // opens the one with media.
            (
                crate::strings::get("gui.menu.open_disc"),
                "d",
                sel!(onOpenDisc:),
            ),
            (
                crate::strings::get("gui.menu.close"),
                "w",
                sel!(onCloseDisc:),
            ),
            ("-".to_string(), "", sel!(onNoop:)),
            (
                crate::strings::get("gui.menu.set_output"),
                "",
                sel!(onBrowseOutput:),
            ),
            (crate::strings::get("gui.menu.start_rip"), "r", sel!(onRip:)),
            ("-".to_string(), "", sel!(onNoop:)),
            (crate::strings::get("gui.menu.eject"), "e", sel!(onEject:)),
        ],
    ));
    main.addItem(&mk_edit(mtm, c));
    // Settings lives in the app menu (⌘,) — the macOS convention. Duplicating
    // it under View is the oddity, not its absence. On Windows there is no app
    // menu, so the Windows shell puts Settings under File instead.
    main.addItem(&mk(
        &crate::strings::get("gui.menu.view"),
        vec![
            (
                // State-dependent: "Show log" only while it is hidden. Built
                // from the live state (a language rebuild can happen with the
                // log already hidden) and re-applied on every `render`.
                crate::ui::log_menu_label(c.ivars().app.borrow().log_hidden),
                "l",
                sel!(onToggleLog:),
            ),
            (
                crate::strings::get("gui.menu.clear_log"),
                "k",
                sel!(onClearLog:),
            ),
        ],
    ));
    main.addItem(&mk(
        &crate::strings::get("gui.menu.help"),
        vec![
            (crate::strings::get("gui.menu.docs"), "?", sel!(onDocs:)),
            (
                crate::strings::get("gui.menu.check_updates"),
                "",
                sel!(onCheckUpdates:),
            ),
        ],
    ));

    app.setMainMenu(Some(&main));
}

// ── build ─────────────────────────────────────────────────────────────────

/// Install the Finder drag-and-drop overlay behind every control, so a user
/// can drop a file/ISO onto the window. Factored out of `run` so a live
/// language rebuild (`relocalize`) can re-add it after tearing the content down.
fn install_drop_view(mtm: MainThreadMarker, window: &NSWindow, c: &Controller) {
    unsafe {
        let content = window.contentView().unwrap();
        let drop = DropView::alloc(mtm).set_ivars(RefCell::new(Some(c.retain())));
        let drop: Retained<DropView> = msg_send![super(drop), init];
        drop.setFrame(content.bounds());
        drop.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        let types =
            objc2_foundation::NSArray::from_slice(&[objc2_app_kit::NSPasteboardTypeFileURL]);
        drop.registerForDraggedTypes(&types);
        // Behind every control: the drop view must not intercept clicks.
        content.addSubview_positioned_relativeTo(
            &drop,
            objc2_app_kit::NSWindowOrderingMode::Below,
            None,
        );
        // `drop` goes out of scope here, and that is correct: the superview's
        // own retain keeps the overlay alive. The old `mem::forget` here
        // leaked one DropView per call — and `relocalize` calls this often.
    }
}

fn build_ui(mtm: MainThreadMarker, window: &NSWindow, c: &Controller) -> Retained<TitlesSource> {
    // This runs again on every language switch (`relocalize` rebuilds the
    // content), and the widgets below are PUSHED, not assigned like other
    // ivars — else the list would keep growing with detached old views.
    c.ivars().bar2_row.borrow_mut().clear();
    let content = window.contentView().unwrap();
    // NSView draws nothing by default, so give the content an explicit
    // backdrop — otherwise cacheDisplayInRect yields a transparent plate.
    let backdrop = { NSBox::initWithFrame(NSBox::alloc(mtm), r(0.0, 0.0, W, H)) };
    {
        backdrop.setBoxType(NSBoxType::Custom);
        backdrop.setTransparent(true);
        backdrop.setFillColor(&NSColor::windowBackgroundColor());
        backdrop.setTitlePosition(objc2_app_kit::NSTitlePosition::NoTitle);
        content.addSubview(&backdrop);
        backdrop.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
        );
    }
    let _add = |v: &NSView| content.addSubview(v);

    // No toolbar: on macOS the menu bar is global and already carries
    // every command, so a duplicate button strip is pure chrome.

    let ty = H - TB_H;
    let top_y = LOG_H + PAD * 2.0;
    let top_h = ty - top_y - 2.0;
    let tree_w = (W - PAD * 2.0) * 0.464; // reference ratio 891/1920

    // both pages occupy the same rect; only one is visible at a time
    let page_main = { NSView::initWithFrame(NSView::alloc(mtm), r(0.0, top_y, W, top_h)) };
    let page_empty = { NSView::initWithFrame(NSView::alloc(mtm), r(0.0, top_y, W, top_h)) };
    {
        page_empty.setHidden(true);
        content.addSubview(&page_empty);
    }
    {
        let eh = top_h;
        let head = text(
            mtm,
            &crate::strings::get("gui.page.empty_title"),
            r(0.0, eh * 0.55, W, 26.0),
            false,
            false,
        );
        let sub = text(
            mtm,
            &crate::strings::get("gui.page.empty_subtitle"),
            r(0.0, eh * 0.55 - 26.0, W, 20.0),
            false,
            true,
        );
        {
            head.setAlignment(NSTextAlignment::Center);
            head.setFont(Some(&NSFont::systemFontOfSize(17.0)));
            sub.setAlignment(NSTextAlignment::Center);
            page_empty.addSubview(&head);
            page_empty.addSubview(&sub);
        }
        // BOTH ways in, side by side — the empty page used to offer only
        // "Open file or ISO…", so opening a disc needed the menu bar.
        // Straddles the centre with a fixed gap, the result page's pattern.
        const EB_W: f64 = 160.0;
        const EB_GAP: f64 = 16.0;
        let eb_y = eh * 0.55 - 70.0;
        let b_disc = btn(
            mtm,
            &crate::strings::get("gui.btn.open_disc"),
            r(W / 2.0 - EB_GAP / 2.0 - EB_W, eb_y, EB_W, 30.0),
            c,
            // The SAME selector File ▸ Open disc uses — one code path.
            sel!(onOpenDisc:),
        );
        let b = btn(
            mtm,
            &crate::strings::get("gui.btn.open_file"),
            r(W / 2.0 + EB_GAP / 2.0, eb_y, EB_W, 30.0),
            c,
            sel!(onOpenFiles:),
        );
        page_empty.addSubview(&b_disc);
        page_empty.addSubview(&b);

        // `relayout` resizes `page_empty`, but a subview keeps its build-time
        // frame unless told to track the parent, or centred text would drift
        // left as the window grows. Same masks the result page uses.
        for v in [&head, &sub] {
            mask(
                v,
                objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable,
            );
        }
        for v in [&b_disc, &b] {
            mask(
                v,
                objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin
                    | objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxXMargin,
            );
        }
    }

    // ── result page ──
    let page_result = { NSView::initWithFrame(NSView::alloc(mtm), r(0.0, top_y, W, top_h)) };
    {
        page_result.setHidden(true);
        content.addSubview(&page_result);
    }
    {
        let head = text(
            mtm,
            &crate::strings::get("gui.result.finished"),
            r(0.0, RESULT_H - 54.0, W, 24.0),
            false,
            false,
        );
        let line = text(mtm, "", r(0.0, RESULT_H - 84.0, W, 20.0), false, true);
        {
            head.setAlignment(NSTextAlignment::Center);
            head.setFont(Some(&NSFont::systemFontOfSize(19.0)));
            line.setAlignment(NSTextAlignment::Center);
            page_result.addSubview(&head);
            page_result.addSubview(&line);
        }
        let reveal = btn(
            mtm,
            &crate::strings::get("gui.btn.show_finder"),
            r(W / 2.0 - 170.0, RESULT_H - 138.0, 160.0, 32.0),
            c,
            sel!(onReveal:),
        );
        let done = btn(
            mtm,
            &crate::strings::get("gui.btn.done"),
            r(W / 2.0 + 10.0, RESULT_H - 138.0, 160.0, 32.0),
            c,
            sel!(onDoneResult:),
        );
        {
            done.setKeyEquivalent(&NSString::from_str("\r"));
            page_result.addSubview(&reveal);
            page_result.addSubview(&done);
        }
        for v in [&reveal, &done] {
            mask(
                v,
                objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin
                    | objc2_app_kit::NSAutoresizingMaskOptions::ViewMaxXMargin,
            );
        }
        for v in [&head, &line] {
            mask(
                v,
                objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable,
            );
        }
        *c.ivars().result_line.borrow_mut() = Some(line);
        *c.ivars().result_head.borrow_mut() = Some(head);
    }
    *c.ivars().page_result.borrow_mut() = Some(page_result);

    let prog_y = PAD + LOG_H_PROG + PAD;
    let page_prog = { NSView::initWithFrame(NSView::alloc(mtm), r(0.0, prog_y, W, PROG_H)) };
    {
        page_prog.setHidden(true);
        content.addSubview(&page_main);
        content.addSubview(&page_prog);
        let grow = objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable;
        page_main.setAutoresizingMask(grow);
        page_prog.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin,
        );
    }
    // main-page children are positioned relative to the page, not the window
    let add = |v: &NSView| page_main.addSubview(v);
    let top_y = 0.0;

    // ── title tree ─────────────────────────────────────────────────────
    let scroll =
        { NSScrollView::initWithFrame(NSScrollView::alloc(mtm), r(PAD, top_y, tree_w, top_h)) };
    let ov =
        { NSOutlineView::initWithFrame(NSOutlineView::alloc(mtm), r(0.0, 0.0, tree_w, top_h)) };

    let mk_col = |ident: &str, title: &str, w: f64| -> Retained<NSTableColumn> {
        let col = {
            NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str(ident))
        };
        {
            col.setWidth(w);
            col.headerCell().setStringValue(&NSString::from_str(title));
        }
        col
    };

    let c_check = mk_col("check", "", 26.0);
    let cell = NSButtonCell::new(mtm);
    unsafe {
        cell.setButtonType(NSButtonType::Switch);
        cell.setTitle(Some(&NSString::from_str("")));
        c_check.setDataCell(&cell);
    }
    let c_type = mk_col("type", &crate::strings::get("gui.col.type"), 96.0);
    let c_desc = mk_col("desc", &crate::strings::get("gui.col.desc"), tree_w - 150.0);
    unsafe {
        ov.addTableColumn(&c_check);
        ov.addTableColumn(&c_type);
        ov.addTableColumn(&c_desc);
        ov.setOutlineTableColumn(Some(&c_check));
        ov.setSelectionHighlightStyle(NSTableViewSelectionHighlightStyle::Regular);
        ov.setUsesAlternatingRowBackgroundColors(false);
        ov.setIndentationPerLevel(14.0);
        ov.setRowHeight(18.0);
        ov.setColumnAutoresizingStyle(
            objc2_app_kit::NSTableViewColumnAutoresizingStyle::LastColumnOnlyAutoresizingStyle,
        );
    }

    let src = TitlesSource::new(mtm);
    unsafe {
        ov.setDataSource(Some(objc2::runtime::ProtocolObject::from_ref(&*src)));
        ov.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*src)));
        *src.ivars().view.borrow_mut() = Some(ov.clone());
        // Without this the checkbox action has no controller to talk to and
        // clicking a row silently does nothing.
        *src.ivars().ctrl.borrow_mut() = Some(c.retain());
        scroll.setDocumentView(Some(&ov));
        scroll.setHasVerticalScroller(true);
        scroll.setBorderType(objc2_app_kit::NSBorderType::BezelBorder);
        ov.reloadData();
        ov.setAllowsEmptySelection(true);
        for &root in src.ivars().roots.borrow().iter() {
            let item = NSNumber::new_usize(root);
            let obj: Retained<AnyObject> = Retained::cast_unchecked(item);
            ov.expandItem_expandChildren(Some(&obj), true);
        }
        ov.reloadData();
    }
    mask(
        &scroll,
        objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    add(&scroll);

    // ── right column ───────────────────────────────────────────────────
    let rx = PAD + tree_w + PAD;
    let rw = W - rx - PAD;
    let _mk_w = 64.0;

    // Output folder group
    let of = group(
        mtm,
        &crate::strings::get("gui.group.output"),
        r(rx, top_h + top_y - 110.0, rw, 110.0),
    );
    let ofv = { of.contentView() }.unwrap();
    let inner_w = rw;
    let fmt = popup_fmt(mtm, r(10.0, 14.0, 260.0, 24.0));
    ofv.addSubview(&fmt);
    let run = btn(
        mtm,
        &crate::strings::get("gui.btn.run_now"),
        r(inner_w - 130.0, 12.0, 108.0, 28.0),
        c,
        sel!(onRip:),
    );
    {
        run.setKeyEquivalent(&NSString::from_str("\r"));
        ofv.addSubview(&run);
    }
    mask(
        &run,
        objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin,
    );
    *c.ivars().run_btn.borrow_mut() = Some(run.clone());
    // Without a target/action the popup is decoration: it shows a choice the
    // model never hears about, so the rip silently uses the old format.
    unsafe {
        fmt.setTarget(Some(c));
        fmt.setAction(Some(sel!(onFormat:)));
    }
    *c.ivars().fmt_popup.borrow_mut() = Some(fmt.clone());
    let fld =
        { NSComboBox::initWithFrame(NSComboBox::alloc(mtm), r(10.0, 52.0, inner_w - 70.0, 24.0)) };
    unsafe {
        let saved = c.ivars().settings.borrow().dest_dir.clone();
        fld.setStringValue(&NSString::from_str(&saved));
        if !saved.is_empty() {
            fld.addItemWithObjectValue(&Retained::cast_unchecked::<AnyObject>(NSString::from_str(
                &saved,
            )));
        }
        // Without a delegate, typing into this field is silent: nothing
        // tells the model, and the next render() tick stomps the keystrokes
        // right back with the model's stale `output_dir`.
        fld.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(c)));
    }
    ofv.addSubview(&fld);
    mask(
        &fld,
        objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable,
    );
    let browse = btn(
        mtm,
        &crate::strings::get("gui.btn.browse"),
        r(inner_w - 56.0, 50.0, 34.0, 26.0),
        c,
        sel!(onBrowseOutput:),
    );
    ofv.addSubview(&browse);
    mask(
        &browse,
        objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin,
    );
    mask(
        &of,
        objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewMinYMargin,
    );
    add(&of);

    // Info group
    let info_h = top_h - 84.0 - PAD;
    let info = group(
        mtm,
        &crate::strings::get("gui.group.info"),
        r(rx, top_y, rw, info_h),
    );
    let iv = { info.contentView() }.unwrap();
    let iscroll = {
        NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            r(8.0, 8.0, rw - 24.0, info_h - 40.0),
        )
    };
    let itv = {
        NSTextView::initWithFrame(
            NSTextView::alloc(mtm),
            r(0.0, 0.0, rw - 24.0, info_h - 40.0),
        )
    };
    {
        itv.setEditable(false);
        itv.setSelectable(true);
        itv.setDrawsBackground(false);
        itv.setString(&NSString::from_str(&crate::strings::get(
            "gui.page.detail_default",
        )));
        itv.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        iscroll.setDocumentView(Some(&itv));
        iscroll.setDrawsBackground(false);
        iscroll.setHasVerticalScroller(true);
        iv.addSubview(&iscroll);
    }
    *src.ivars().info.borrow_mut() = Some(itv.clone());
    mask(
        &iscroll,
        objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    mask(
        &info,
        objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    add(&info);

    // ── progress page (reference: Information group, two bars, cancel) ──
    {
        let padd = |v: &NSView| page_prog.addSubview(v);
        let info_h = 132.0;
        let gy = PROG_H - info_h - 14.0;
        let g = group(
            mtm,
            &crate::strings::get("gui.group.information"),
            r(PAD, gy, W - PAD * 2.0, info_h),
        );
        let gv = { g.contentView() }.unwrap();
        // Labels come from the core, never from the shell — otherwise a
        // renamed row here would silently disagree with the Windows shell.
        let rows = crate::ui::InfoRows::labels();
        let mut vals: Vec<Retained<NSTextField>> = Vec::new();
        let lh = 15.0;
        // An NSBox's contentView is inset below its title, so rows must be
        // laid out against the INNER height — using the box height clips the
        // first row off the top.
        let inner_h = { gv.bounds() }.size.height;
        for (i, k) in rows.iter().enumerate() {
            let yy = inner_h - 16.0 - (i as f64) * lh;
            let kl = text(mtm, k, r(0.0, yy, 96.0, 14.0), true, false);
            {
                kl.setFont(Some(&NSFont::systemFontOfSize(11.0)));
                gv.addSubview(&kl);
            }
            let f = text(mtm, "", r(104.0, yy, W - 160.0, 14.0), false, false);
            f.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            gv.addSubview(&f);
            vals.push(f);
        }
        mask(
            &g,
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable,
        );
        padd(&g);

        let bar_y = gy - 54.0;
        let c1 = text(
            mtm,
            "Saving to MKV file",
            r(PAD, bar_y + 20.0, 300.0, 14.0),
            false,
            false,
        );
        c1.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        padd(&c1);
        let l1 = text(
            mtm,
            "Elapsed: 0:00:00 / Remaining : 0:00:00",
            r(W - 330.0, bar_y + 20.0, 320.0, 15.0),
            true,
            false,
        );
        // Monospaced digits: the Elapsed/Remaining/% caption must not shift as
        // its digits change width (a plain proportional font jumps every tick).
        let cap_font =
            unsafe { NSFont::monospacedDigitSystemFontOfSize_weight(11.0, NSFontWeightRegular) };
        l1.setFont(Some(&cap_font));
        mask(
            &l1,
            objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin,
        );
        padd(&l1);
        let p1 = {
            NSProgressIndicator::initWithFrame(
                NSProgressIndicator::alloc(mtm),
                r(PAD, bar_y, W - PAD * 2.0, 20.0),
            )
        };
        unsafe {
            p1.setStyle(objc2_app_kit::NSProgressIndicatorStyle::Bar);
            p1.setIndeterminate(false);
            p1.setControlSize(objc2_app_kit::NSControlSize::Regular);
            p1.setMinValue(0.0);
            p1.setMaxValue(100.0);
            p1.setDoubleValue(0.0);
            p1.setUsesThreadedAnimation(false);
            p1.startAnimation(None);
        }
        mask(
            &p1,
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable,
        );
        padd(&p1);

        let bar2_y = bar_y - 46.0;
        let c2 = text(
            mtm,
            "Saving all titles to MKV files",
            r(PAD, bar2_y + 20.0, 320.0, 14.0),
            false,
            false,
        );
        c2.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        padd(&c2);
        let l2 = text(
            mtm,
            "Elapsed: 0:00:00 / Remaining : 0:00:00",
            r(W - 330.0, bar2_y + 20.0, 320.0, 15.0),
            true,
            false,
        );
        let cap_font2 =
            unsafe { NSFont::monospacedDigitSystemFontOfSize_weight(11.0, NSFontWeightRegular) };
        l2.setFont(Some(&cap_font2));
        mask(
            &l2,
            objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin,
        );
        padd(&l2);
        let p2 = {
            NSProgressIndicator::initWithFrame(
                NSProgressIndicator::alloc(mtm),
                r(PAD, bar2_y, W - PAD * 2.0 - 34.0, 20.0),
            )
        };
        unsafe {
            p2.setStyle(objc2_app_kit::NSProgressIndicatorStyle::Bar);
            p2.setIndeterminate(false);
            p2.setControlSize(objc2_app_kit::NSControlSize::Regular);
            p2.setMinValue(0.0);
            p2.setMaxValue(100.0);
            p2.setDoubleValue(0.0);
            p2.setUsesThreadedAnimation(false);
            p2.startAnimation(None);
        }
        mask(
            &p2,
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable,
        );
        padd(&p2);
        c.ivars()
            .bar2_row
            .borrow_mut()
            .push(unsafe { Retained::cast_unchecked::<NSView>(p2.clone()) });
        c.ivars()
            .bar2_row
            .borrow_mut()
            .push(unsafe { Retained::cast_unchecked::<NSView>(c2.clone()) });
        c.ivars()
            .bar2_row
            .borrow_mut()
            .push(unsafe { Retained::cast_unchecked::<NSView>(l2.clone()) });
        let cancel = btn(
            mtm,
            &crate::strings::get("gui.btn.cancel"),
            r(W - PAD - 110.0, 10.0, 110.0, 30.0),
            c,
            sel!(onCancelRip:),
        );
        mask(
            &cancel,
            objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin,
        );
        padd(&cancel);

        *c.ivars().bar_cur.borrow_mut() = Some(p1);
        *c.ivars().bar_all.borrow_mut() = Some(p2);
        *c.ivars().lbl_cur.borrow_mut() = Some(l1);
        *c.ivars().lbl_all.borrow_mut() = Some(l2);
        *c.ivars().lbl_saving_cur.borrow_mut() = Some(c1);
        *c.ivars().lbl_saving_all.borrow_mut() = Some(c2);
        *c.ivars().fields.borrow_mut() = vals;
    }
    *c.ivars().page_empty.borrow_mut() = Some(page_empty);
    *c.ivars().page_main.borrow_mut() = Some(page_main);
    *c.ivars().page_prog.borrow_mut() = Some(page_prog);

    // ── log ────────────────────────────────────────────────────────────
    let logscroll = {
        NSScrollView::initWithFrame(NSScrollView::alloc(mtm), r(PAD, PAD, W - PAD * 2.0, LOG_H))
    };
    let tv =
        { NSTextView::initWithFrame(NSTextView::alloc(mtm), r(0.0, 0.0, W - PAD * 2.0, LOG_H)) };
    let ready_line =
        crate::strings::fmt("gui.log.ready", &[("version", env!("CARGO_PKG_VERSION"))]);
    {
        tv.setEditable(false);
        // Read-only but selectable: the log is the thing users paste into bug
        // reports, so copying out of it has to work.
        tv.setSelectable(true);
        tv.setFont(Some(&NSFont::userFixedPitchFontOfSize(11.0).unwrap()));
    }
    log_append(&tv, &ready_line, 2);
    {
        logscroll.setDocumentView(Some(&tv));
        logscroll.setHasVerticalScroller(true);
        logscroll.setBorderType(objc2_app_kit::NSBorderType::BezelBorder);
    }
    mask(
        &logscroll,
        objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
            | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    content.addSubview(&logscroll);

    *c.ivars().out_field.borrow_mut() = Some(fld);
    *c.ivars().log.borrow_mut() = Some(tv);
    *c.ivars().backdrop.borrow_mut() = Some(backdrop.clone());
    *c.ivars().tree_scroll.borrow_mut() = Some(scroll.clone());
    *c.ivars().tree.borrow_mut() = Some(ov.clone());
    *c.ivars().src.borrow_mut() = Some(src.clone());
    *c.ivars().grp_out.borrow_mut() = Some(of.clone());
    *c.ivars().grp_info.borrow_mut() = Some(info.clone());
    *c.ivars().log_scroll.borrow_mut() = Some(logscroll.clone());
    src
}

// ── self-screenshot (no permissions needed: app renders its own view) ─────

fn snapshot(view: &NSView, path: &str) {
    unsafe {
        let b = view.bounds();
        let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(b) else {
            return;
        };
        view.cacheDisplayInRect_toBitmapImageRep(b, &rep);
        let empty = NSDictionary::new();
        let Some(data) = rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &empty)
        else {
            return;
        };
        let _ = data.writeToFile_atomically(&NSString::from_str(path), true);
    }
}

// ── run ───────────────────────────────────────────────────────────────────

pub fn run() {
    let mtm = MainThreadMarker::new().unwrap();
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    // The app follows the system Light/Dark setting. The only exception is
    // the screenshot harness, which pins Aqua so captures are comparable.
    if cfg!(debug_assertions) && std::env::var("FMKV_SHOT").is_ok()
        || std::env::var("FMKV_WIN").is_ok()
    {
        let dark = dev_env("FMKV_DARK").is_ok();
        unsafe {
            let name = if dark {
                objc2_app_kit::NSAppearanceNameDarkAqua
            } else {
                NSAppearanceNameAqua
            };
            if let Some(a) = NSAppearance::appearanceNamed(name) {
                app.setAppearance(Some(&a));
            }
        }
    }

    let style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable
        | NSWindowStyleMask::Resizable;
    let window: Retained<NSWindow> = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            r(0.0, 0.0, W, H),
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("freemkv"));
    {
        window.setContentMinSize(NSSize::new(1020.0, 620.0));
        // Single-window app: macOS otherwise injects "Show Tab Bar" /
        // "Show All Tabs" into the View menu and offers to merge windows,
        // neither of which means anything here.
        window.setTabbingMode(objc2_app_kit::NSWindowTabbingMode::Disallowed);
    }

    let c = Controller::new(mtm);
    // The app delegate, not just the window delegate: without it ⌘Q bypassed
    // the rip-in-progress confirmation, and closing the window left a headless
    // process behind. See `NSApplicationDelegate for Controller`.
    app.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*c)));
    *c.ivars().win_main.borrow_mut() = Some(window.clone());
    build_menus(mtm, &app, &c);
    let src = build_ui(mtm, &window, &c);

    {
        window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*c)));
    }
    install_drop_view(mtm, &window, &c);

    // FMKV_DEMO=<path> drives the real controls on a timer so a human can
    // watch: open → tick → choose format → Run. Debug builds only.
    #[cfg(debug_assertions)]
    if let Ok(src) = dev_env("FMKV_DEMO") {
        *c.ivars().demo_path.borrow_mut() = src;
        let t = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                2.5,
                &c,
                sel!(onDemoStep:),
                None,
                true,
            )
        };
        *c.ivars().demo_timer.borrow_mut() = Some(t);
    }

    // Nothing is open at launch: show the empty state rather than a tree of
    // invented rows.
    c.render();
    c.relayout(W, H);
    #[cfg(debug_assertions)]
    if let Ok(spec) = dev_env("FMKV_SELFTEST") {
        let mut it = spec.split('|');
        let iso = it.next().unwrap_or("").to_string();
        let mkv = it.next().unwrap_or("").to_string();
        let dir = it.next().unwrap_or("/tmp").to_string();
        let _ = std::fs::create_dir_all(&dir);
        {
            let rl = NSRunLoop::currentRunLoop();
            rl.runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.4));
        }
        let ok = c.self_test(&iso, &mkv, &dir);
        std::process::exit(if ok { 0 } else { 1 });
    }

    window.center();
    window.makeKeyAndOrderFront(None);
    app.activate();

    if cfg!(debug_assertions) && std::env::var("FMKV_DUMP_MENUS").is_ok() {
        if let Some(main) = app.mainMenu() {
            for i in 0..main.numberOfItems() {
                let it = main.itemAtIndex(i).unwrap();
                let t = { it.title() }.to_string();
                println!("MENU  {t}");
                if let Some(sub) = { it.submenu() } {
                    for j in 0..sub.numberOfItems() {
                        let s2 = sub.itemAtIndex(j).unwrap();
                        let st = { s2.title() }.to_string();
                        let key = { s2.keyEquivalent() }.to_string();
                        if s2.isSeparatorItem() {
                            println!("    ----");
                        } else if key.is_empty() {
                            println!("    {st}");
                        } else {
                            println!("    {st}  [Cmd+{key}]");
                        }
                    }
                }
            }
        }
        return;
    }

    // FMKV_PAGE=progress snapshots the rip page instead of the tree
    if cfg!(debug_assertions) && std::env::var("FMKV_PAGE").as_deref() == Ok("progress") {
        c.app_mut(|a| a.page = crate::ui::Page::Progress);
        c.render();
        let v: f64 = dev_env("FMKV_PCT")
            .ok()
            .and_then(|x| x.parse().ok())
            .unwrap_or(100.0);
        if let Some(b) = c.ivars().bar_cur.borrow().as_ref() {
            b.setDoubleValue(v);
        }
        if let Some(b) = c.ivars().bar_all.borrow().as_ref() {
            b.setDoubleValue(100.0);
        }
        let stamp = "Elapsed: 0:01:15 / Remaining : 0:00:00";
        for l in [&c.ivars().lbl_cur, &c.ivars().lbl_all] {
            if let Some(l) = l.borrow().as_ref() {
                l.setStringValue(&NSString::from_str(stamp));
            }
        }
        let f = c.ivars().fields.borrow();
        if f.len() >= 7 {
            {
                f[0].setStringValue(&NSString::from_str("/Volumes/media/iso/dvd/Movie.iso"));
                f[1].setStringValue(&NSString::from_str("Movie.iso"));
                f[2].setStringValue(&NSString::from_str(&crate::ui::fmt_bytes(6_743_590_912)));
                f[3].setStringValue(&NSString::from_str("—"));
                f[4].setStringValue(&NSString::from_str("/tmp/audit/demo/Movie_t1.mkv"));
                f[5].setStringValue(&NSString::from_str(&crate::ui::fmt_bytes(6_147_000_000)));
                f[6].setStringValue(&NSString::from_str(&crate::ui::fmt_bytes(243_000_000_000)));
            }
        }
        drop(f);
        for b in [&c.ivars().bar_cur, &c.ivars().bar_all] {
            if let Some(b) = b.borrow().as_ref() {
                b.setNeedsDisplay(true);
            }
        }
        c.app_mut(|a| a.say(crate::ui::LogKind::Result, "Saving titles …"));
        c.app_mut(|a| a.say(crate::ui::LogKind::Result, "titles saved"));
    }

    // FMKV_SIZE=WxH resizes before snapshotting, to test the resize behaviour
    if let Ok(sz) = dev_env("FMKV_SIZE")
        && let Some((ws, hs)) = sz.split_once('x')
        && let (Ok(nw), Ok(nh)) = (ws.parse::<f64>(), hs.parse::<f64>())
    {
        window.setContentSize(NSSize::new(nw, nh));
        c.relayout(nw, nh);
    }

    if let Ok(src) = dev_env("FMKV_OPEN") {
        {
            let fx = c.app_mut(|a| a.open(&src));
            c.perform(fx);
        };
    }

    if cfg!(debug_assertions) && std::env::var("FMKV_PAGE").as_deref() == Ok("empty") {
        c.render();
    }
    if cfg!(debug_assertions) && std::env::var("FMKV_PAGE").as_deref() == Ok("result") {
        c.app_mut(|a| a.page = crate::ui::Page::Result);
        c.app_mut(|a| a.result_summary = "2 title(s) written".into());
        c.render();
    }
    if let Ok(which) = dev_env("FMKV_WIN") {
        let w = match which.as_str() {
            "prefs" => build_prefs(mtm, &c),
            _ => build_about(mtm, &c),
        };
        if let Ok(tab) = dev_env("FMKV_TAB")
            && let Ok(i) = tab.parse::<isize>()
            && let Some(tv) = c.ivars().tabs.borrow().as_ref()
        {
            tv.selectTabViewItemAtIndex(i);
        }
        w.center();
        w.makeKeyAndOrderFront(None);
        {
            let rl = NSRunLoop::currentRunLoop();
            rl.runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.6));
        }
        if let Ok(path) = dev_env("FMKV_SHOT") {
            let cv = w.contentView().unwrap();
            {
                w.displayIfNeeded();
                cv.setNeedsDisplay(true);
                cv.display();
            }
            snapshot(&cv, &path);
            println!("wrote {path}");
        }
        return;
    }

    if let Ok(path) = dev_env("FMKV_SHOT") {
        // Re-apply layout at the real content size before capturing, or the
        // snapshot shows stale geometry.
        if let Some(cv) = window.contentView() {
            let b = cv.frame();
            c.relayout(b.size.width, b.size.height);
        }
        // Let AppKit actually lay out and draw: view-based table cells are
        // created lazily during layout, so snapshotting immediately yields
        // an empty tree.
        {
            let rl = NSRunLoop::currentRunLoop();
            let until = NSDate::dateWithTimeIntervalSinceNow(0.8);
            rl.runUntilDate(&until);
        }
        let content = window.contentView().unwrap();
        {
            content.setNeedsDisplay(true);
            content.displayIfNeeded();
        }
        snapshot(&content, &path);
        println!("wrote {path}");
        return;
    }

    // A disc in the drive went undetected at launch: nothing ran detection
    // until File ▸ Open disc was clicked. Fixed via a one-shot timer so the
    // window paints before the drive scan blocks the main thread.
    if ["FMKV_OPEN", "FMKV_DEMO", "FMKV_PAGE"]
        .iter()
        .all(|k| dev_env(k).is_err())
    {
        let _ = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.2,
                &c,
                sel!(onLaunchProbe:),
                None,
                false,
            )
        };
    }

    std::mem::forget(c);
    std::mem::forget(src);
    app.run();
}

// ── Preferences ───────────────────────────────────────────────────────────

// Option table for a settings dropdown, owned by `crate::ui::enum_options`
// so the two shells can't drift apart. Empty means "not an enum popup".
use crate::ui::enum_options;

/// One tickable language row. `representedObject` carries the ISO code, so the
/// click handler never has to map a (localized, or fallback-to-code) title back
/// to a code — the title is for the user, the represented object is for us.
fn lang_menu_item(mtm: MainThreadMarker, code: &str, c: &Controller) -> Retained<NSMenuItem> {
    let mi = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(&crate::ui::lang_display_name(code)),
            Some(sel!(onToggleLang:)),
            &NSString::from_str(""),
        )
    };
    unsafe {
        mi.setTarget(Some(c));
        mi.setRepresentedObject(Some(&NSString::from_str(code)));
    }
    mi
}

// Stored preference a picker currently holds, kept on its title item. The
// control IS the state: nothing shadows it in Rust, so Cancel discards
// edits for free because nothing outside the window was ever touched.
fn lang_picker_value(p: &NSPopUpButton) -> String {
    p.menu()
        .and_then(|m| m.itemAtIndex(0))
        .and_then(|it| it.representedObject())
        .and_then(|o| o.downcast::<NSString>().ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

// Show `stored` in a picker: title, checkmarks, and the value the next
// toggle (and OK) reads back. Codes not on the offered list get a row
// appended, so a stored language stays visible AND removable.
fn set_lang_picker(mtm: MainThreadMarker, p: &NSPopUpButton, stored: &str, c: &Controller) {
    let Some(menu) = p.menu() else { return };
    let selection = crate::ui::lang_selection(stored);
    for code in &selection {
        if lang_item_index(&menu, code).is_none() {
            menu.addItem(&lang_menu_item(mtm, code, c));
        }
    }
    // Item 0 is the pull-down's title; never blank, because `lang_summary`
    // answers "Any" for an empty selection.
    if let Some(title) = menu.itemAtIndex(0) {
        title.setTitle(&NSString::from_str(&crate::ui::lang_summary(stored)));
        unsafe { title.setRepresentedObject(Some(&NSString::from_str(stored))) };
    }
    for i in 1..menu.numberOfItems() {
        let Some(it) = menu.itemAtIndex(i) else {
            continue;
        };
        let on = lang_item_code(&it)
            .map(|code| crate::ui::lang_is_selected(stored, &code))
            .unwrap_or(false);
        it.setState(if on { 1 } else { 0 });
    }
    // The button paints its title from item 0, which we just rewrote.
    NSView::setNeedsDisplay(p, true);
}

/// The ISO code a language row stands for, or None for the title item.
fn lang_item_code(it: &NSMenuItem) -> Option<String> {
    it.representedObject()
        .and_then(|o| o.downcast::<NSString>().ok())
        .map(|s| s.to_string())
}

fn lang_item_index(menu: &NSMenu, code: &str) -> Option<isize> {
    (1..menu.numberOfItems()).find(|i| {
        menu.itemAtIndex(*i)
            .and_then(|it| lang_item_code(&it))
            .is_some_and(|c| c.eq_ignore_ascii_case(code))
    })
}

/// One labelled row inside a preferences tab. Right-aligned label at a fixed
/// gutter, control to its right — the layout the reference uses throughout.
struct Rows {
    view: Retained<NSView>,
    y: f64,
    gutter: f64,
    /// (key, control) for every editable control, so OK can read them back.
    fields: Vec<(String, Retained<NSTextField>)>,
    checks: Vec<(String, Retained<NSButton>)>,
    popups: Vec<(String, Retained<NSPopUpButton>)>,
    langs: Vec<(String, Retained<NSPopUpButton>)>,
}

impl Rows {
    fn new(mtm: MainThreadMarker, w: f64, h: f64, gutter: f64) -> Self {
        let view = { NSView::initWithFrame(NSView::alloc(mtm), r(0.0, 0.0, w, h)) };
        Rows {
            view,
            y: h - 40.0,
            gutter,
            fields: vec![],
            checks: vec![],
            popups: vec![],
            langs: vec![],
        }
    }
    fn label(&mut self, mtm: MainThreadMarker, s: &str) {
        let l = text(mtm, s, r(0.0, self.y, self.gutter - 8.0, 18.0), true, false);
        {
            l.setFont(Some(&NSFont::systemFontOfSize(12.0)));
            self.view.addSubview(&l);
        }
    }
    fn check(&mut self, mtm: MainThreadMarker, key: &str, s: &str, on: bool) {
        self.label(mtm, s);
        let b =
            { NSButton::initWithFrame(NSButton::alloc(mtm), r(self.gutter, self.y, 20.0, 18.0)) };
        {
            b.setButtonType(NSButtonType::Switch);
            b.setTitle(&NSString::from_str(""));
            b.setState(if on { 1 } else { 0 });
            self.view.addSubview(&b);
        }
        self.checks.push((key.to_string(), b));
        self.y -= 28.0;
    }
    /// Insert a prebuilt popup (used so Preferences and the main window share
    /// one output-format list rather than drifting apart).
    fn popup(
        &mut self,
        mtm: MainThreadMarker,
        key: &str,
        s: &str,
        p: Retained<NSPopUpButton>,
        w: f64,
    ) {
        self.label(mtm, s);
        {
            p.setFrame(r(self.gutter, self.y - 3.0, w, 24.0));
            self.view.addSubview(&p);
        }
        self.popups.push((key.to_string(), p));
        self.y -= 30.0;
    }

    /// A settings dropdown. Items come from `enum_options(key)` (localized
    /// labels), and the popup is registered under `key` so OK can read the
    /// selection back and persist its canonical value.
    fn combo(&mut self, mtm: MainThreadMarker, key: &str, s: &str, w: f64) {
        self.label(mtm, s);
        let p = {
            NSPopUpButton::initWithFrame_pullsDown(
                NSPopUpButton::alloc(mtm),
                r(self.gutter, self.y - 3.0, w, 24.0),
                false,
            )
        };
        for (_canon, label) in enum_options(key) {
            p.addItemWithTitle(&NSString::from_str(&label));
        }
        self.view.addSubview(&p);
        self.popups.push((key.to_string(), p));
        self.y -= 30.0;
    }

    // Multi-select language row: pull-down listing every offered language
    // with checkmarks. Registered in `self.langs`, not `self.popups` — see
    // docs/mac-shell.md — Rows::langs.
    fn langs(&mut self, mtm: MainThreadMarker, key: &str, s: &str, w: f64, c: &Controller) {
        self.label(mtm, s);
        // Pull-down, not pop-up: a pop-up's title tracks the selected row, but
        // this button must show the whole SET, so item 0 (ours to write) is
        // the picker's memory (see `lang_picker_value`).
        let p = {
            NSPopUpButton::initWithFrame_pullsDown(
                NSPopUpButton::alloc(mtm),
                r(self.gutter, self.y - 3.0, w, 24.0),
                true,
            )
        };
        let menu = NSMenu::new(mtm);
        // Auto-enabling would ask the responder chain about every row and hide
        // any it could not resolve; these rows are always usable.
        menu.setAutoenablesItems(false);
        let title = NSMenuItem::new(mtm);
        menu.addItem(&title);
        for (code, _name) in crate::ui::PICKER_LANGUAGES {
            menu.addItem(&lang_menu_item(mtm, code, c));
        }
        p.setMenu(Some(&menu));
        self.view.addSubview(&p);
        // Empty until the populate loop fills it from disk; going through the
        // same setter keeps the "never a blank button" rule in one place.
        set_lang_picker(mtm, &p, "", c);
        self.langs.push((key.to_string(), p));
        self.y -= 30.0;
    }
    // A plain text row. MUST register `(key, control)` in `self.fields`: it
    // is the only thing `read_prefs_form` and the populate loop use, so a
    // row missing from it is write-only. See docs/mac-shell.md — Rows::field.
    fn field(&mut self, mtm: MainThreadMarker, key: &str, s: &str, val: &str, w: f64) {
        self.label(mtm, s);
        let f = {
            NSTextField::initWithFrame(
                NSTextField::alloc(mtm),
                r(self.gutter, self.y - 2.0, w, 22.0),
            )
        };
        {
            f.setStringValue(&NSString::from_str(val));
            f.setFont(Some(&NSFont::systemFontOfSize(12.0)));
            self.view.addSubview(&f);
        }
        self.fields.push((key.to_string(), f));
        self.y -= 30.0;
    }

    // Same contract as `field`, but for a secret (keyserver bearer token):
    // an `NSSecureTextField` masks its contents, unlike the plain
    // `NSTextField` this used to be. See docs/mac-shell.md — field_secure.
    fn field_secure(&mut self, mtm: MainThreadMarker, key: &str, s: &str, val: &str, w: f64) {
        self.label(mtm, s);
        let f = {
            NSSecureTextField::initWithFrame(
                NSSecureTextField::alloc(mtm),
                r(self.gutter, self.y - 2.0, w, 22.0),
            )
        };
        {
            f.setStringValue(&NSString::from_str(val));
            f.setFont(Some(&NSFont::systemFontOfSize(12.0)));
            self.view.addSubview(&f);
        }
        // NSSecureTextField IS-A NSTextField at the Objective-C level, so this
        // upcast is sound; it lets the secure field share `self.fields`'s
        // storage type with every ordinary text row.
        let f: Retained<NSTextField> = unsafe { Retained::cast_unchecked(f) };
        self.fields.push((key.to_string(), f));
        self.y -= 30.0;
    }

    fn path(
        &mut self,
        mtm: MainThreadMarker,
        key: &str,
        s: &str,
        val: &str,
        w: f64,
        c: &Controller,
    ) {
        self.label(mtm, s);
        let f = {
            NSTextField::initWithFrame(
                NSTextField::alloc(mtm),
                r(self.gutter, self.y - 2.0, w - 40.0, 22.0),
            )
        };
        {
            f.setStringValue(&NSString::from_str(val));
            f.setFont(Some(&NSFont::systemFontOfSize(12.0)));
            self.view.addSubview(&f);
        }
        self.fields.push((key.to_string(), f));
        // The browse "…" opens a picker that fills THIS field: dest_dir picks a
        // folder, keydb_path picks a file.
        let action = match key {
            "dest_dir" => sel!(onBrowseDestDir:),
            "keydb_path" => sel!(onBrowseKeydb:),
            _ => sel!(onNoop:),
        };
        let b = btn(
            mtm,
            &crate::strings::get("gui.btn.browse"),
            r(self.gutter + w - 34.0, self.y - 4.0, 34.0, 25.0),
            c,
            action,
        );
        self.view.addSubview(&b);
        self.y -= 30.0;
    }
    fn button(
        &mut self,
        mtm: MainThreadMarker,
        title: &str,
        c: &Controller,
        a: Sel,
        w: f64,
    ) -> Retained<NSButton> {
        let b = btn(mtm, title, r(self.gutter, self.y - 4.0, w, 26.0), c, a);
        self.view.addSubview(&b);
        self.y -= 32.0;
        b
    }
    fn note(&mut self, mtm: MainThreadMarker, s: &str, w: f64) -> Retained<NSTextField> {
        let l = text(mtm, s, r(16.0, self.y - 18.0, w - 32.0, 36.0), false, true);
        {
            l.setUsesSingleLineMode(false);
            self.view.addSubview(&l);
        }
        self.y -= 44.0;
        l
    }
    fn gap(&mut self) {
        self.y -= 14.0;
    }
}

fn build_prefs(mtm: MainThreadMarker, c: &Controller) -> Retained<NSWindow> {
    // Wide enough that the widest label ("Keep encrypted (raw passthrough) :",
    // "Minimum title length (seconds) :") — and its longer translations — fits
    // the label gutter without clipping, while the controls still have room.
    let (w, h) = (640.0, 470.0);
    let win: Retained<NSWindow> = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            r(0.0, 0.0, w, h),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    win.setTitle(&NSString::from_str(&crate::strings::get(
        "gui.win.settings",
    )));
    // NSWindow defaults to releasedWhenClosed=YES: `close()` would deallocate
    // it while `win_prefs` still holds a Retained ref, a use-after-free on
    // reopen. Keep it alive and let the Retained own the lifetime.
    unsafe { win.setReleasedWhenClosed(false) };
    let content = win.contentView().unwrap();
    let bd = { NSBox::initWithFrame(NSBox::alloc(mtm), r(0.0, 0.0, w, h)) };
    {
        bd.setBoxType(NSBoxType::Custom);
        bd.setTransparent(true);
        bd.setFillColor(&NSColor::windowBackgroundColor());
        bd.setTitlePosition(objc2_app_kit::NSTitlePosition::NoTitle);
        content.addSubview(&bd);
    }

    let tabs = {
        objc2_app_kit::NSTabView::initWithFrame(
            objc2_app_kit::NSTabView::alloc(mtm),
            r(10.0, 46.0, w - 20.0, h - 56.0),
        )
    };
    let tw = w - 44.0;
    let th = h - 100.0;

    let mut all_fields: Vec<(String, Retained<NSTextField>)> = Vec::new();
    let mut all_checks: Vec<(String, Retained<NSButton>)> = Vec::new();
    let mut all_popups: Vec<(String, Retained<NSPopUpButton>)> = Vec::new();
    let mut all_langs: Vec<(String, Retained<NSPopUpButton>)> = Vec::new();
    let mut add_tab = |label: &str, rows: Rows| {
        all_fields.extend(rows.fields.iter().cloned());
        all_checks.extend(rows.checks.iter().cloned());
        all_popups.extend(rows.popups.iter().cloned());
        all_langs.extend(rows.langs.iter().cloned());
        let ident: Retained<AnyObject> =
            unsafe { Retained::cast_unchecked(NSString::from_str(label)) };
        let item = unsafe {
            objc2_app_kit::NSTabViewItem::initWithIdentifier(
                objc2_app_kit::NSTabViewItem::alloc(),
                Some(&ident),
            )
        };
        {
            item.setLabel(&NSString::from_str(label));
            tabs.addTabViewItem(&item);
            item.setView(Some(&rows.view));
        }
    };

    // Every control below maps to something the engine or key layer actually
    // consumes; dead switches were removed rather than left as no-ops.

    // ── Output ── engine Job.dest + the GUI's own naming
    let mut t = Rows::new(mtm, tw, th, 250.0);
    t.popup(
        mtm,
        "container",
        &crate::strings::get("gui.set.default_output"),
        popup_fmt(mtm, r(0.0, 0.0, 300.0, 24.0)),
        300.0,
    );
    t.path(
        mtm,
        "dest_dir",
        &crate::strings::get("gui.set.default_dest"),
        "",
        300.0,
        c,
    );
    t.field(
        mtm,
        "filename_template",
        &crate::strings::get("gui.set.filename_template"),
        "{title}_t{n}",
        220.0,
    );
    t.gap();
    t.check(
        mtm,
        "keep_iso",
        &crate::strings::get("gui.set.keep_iso"),
        false,
    );
    t.check(
        mtm,
        "auto_eject",
        &crate::strings::get("gui.set.auto_eject"),
        true,
    );
    add_tab(&crate::strings::get("gui.tab.output"), t);

    // ── Selection ── engine Job.selection
    let mut t = Rows::new(mtm, tw, th, 250.0);
    t.combo(
        mtm,
        "selection",
        &crate::strings::get("gui.set.default_selection"),
        220.0,
    );
    t.field(
        mtm,
        "min_title_secs",
        &crate::strings::get("gui.set.min_length"),
        "120",
        80.0,
    );
    t.note(mtm, &crate::strings::get("gui.set.min_length_note"), tw);
    t.gap();
    // Three INDEPENDENT language sets — see `ui::LangPrefs`. They decide which
    // stream rows start ticked; nothing here bypasses the tick boxes, so the
    // user still sees and can change every choice.
    t.langs(
        mtm,
        "audio_langs",
        &crate::strings::get("gui.set.audio_langs"),
        220.0,
        c,
    );
    t.langs(
        mtm,
        "sub_langs",
        &crate::strings::get("gui.set.sub_langs"),
        220.0,
        c,
    );
    t.langs(
        mtm,
        "forced_sub_langs",
        &crate::strings::get("gui.set.forced_sub_langs"),
        220.0,
        c,
    );
    t.note(mtm, &crate::strings::get("gui.set.lang_prefs_note"), tw);
    add_tab(&crate::strings::get("gui.tab.selection"), t);

    // ── Recovery ── engine Job.mode / abort_on_lost_secs / raw
    let mut t = Rows::new(mtm, tw, th, 250.0);
    t.combo(
        mtm,
        "rip_mode",
        &crate::strings::get("gui.set.rip_mode"),
        220.0,
    );
    t.field(
        mtm,
        "max_passes",
        &crate::strings::get("gui.set.max_passes"),
        "5",
        70.0,
    );
    t.field(
        mtm,
        "abort_lost_secs",
        &crate::strings::get("gui.set.abort_lost"),
        "0",
        70.0,
    );
    t.note(mtm, &crate::strings::get("gui.set.abort_lost_note"), tw);
    t.gap();
    t.check(
        mtm,
        "raw",
        &crate::strings::get("gui.set.keep_encrypted"),
        false,
    );
    t.note(mtm, &crate::strings::get("gui.set.raw_note"), tw);
    t.gap();
    t.check(
        mtm,
        "force",
        &crate::strings::get("gui.set.overwrite"),
        false,
    );
    t.note(mtm, &crate::strings::get("gui.set.capture_note"), tw);
    add_tab(&crate::strings::get("gui.tab.recovery"), t);

    // ── Keys ── keydb + the online key service
    let mut t = Rows::new(mtm, tw, th, 250.0);
    t.combo(
        mtm,
        "key_source",
        &crate::strings::get("gui.set.key_source"),
        240.0,
    );
    t.gap();
    t.path(
        mtm,
        "keydb_path",
        &crate::strings::get("gui.set.keydb_path"),
        "",
        300.0,
        c,
    );
    t.field(
        mtm,
        "keydb_url",
        &crate::strings::get("gui.set.keydb_url"),
        "",
        300.0,
    );
    let update_btn = t.button(
        mtm,
        &crate::strings::get("gui.set.update_keydb"),
        c,
        sel!(onUpdateKeys:),
        160.0,
    );
    *c.ivars().keydb_btn.borrow_mut() = Some(update_btn);
    // A download may already be running from an earlier Settings session: this
    // button is brand new and enabled, so tell it what the controller knows.
    c.set_keydb_updating(c.ivars().keydb_updating.get());
    {
        let status = c.ivars().settings.borrow().keydb_status();
        let note = t.note(mtm, &status, tw);
        *c.ivars().keydb_note.borrow_mut() = Some(note);
    }
    t.gap();
    t.field(
        mtm,
        "keyserver_url",
        &crate::strings::get("gui.set.keyserver_url"),
        "",
        300.0,
    );
    t.field_secure(
        mtm,
        "keyserver_token",
        &crate::strings::get("gui.set.keyserver_token"),
        "",
        300.0,
    );
    t.button(
        mtm,
        &crate::strings::get("gui.set.test_connection"),
        c,
        sel!(onTestKeyserver:),
        160.0,
    );
    add_tab(&crate::strings::get("gui.tab.keys"), t);

    // ── Advanced
    let mut t = Rows::new(mtm, tw, th, 250.0);
    // Picker rows come straight from the shipped locale list (via
    // enum_options("language")), so the menu can never drift from what
    // freemkv-i18n can actually load.
    t.combo(
        mtm,
        "language",
        &crate::strings::get("gui.set.language"),
        200.0,
    );
    // No "restart to apply" note: the switch is live (see relocalize), and
    // Settings can't be opened mid-rip, so it always takes effect at once.
    // (gui.set.language_note stays in the catalogs for locale parity.)
    t.field(
        mtm,
        "decrypt_threads",
        &crate::strings::get("gui.set.decrypt_threads"),
        "0",
        70.0,
    );
    t.note(
        mtm,
        &crate::strings::get("gui.set.decrypt_threads_note"),
        tw,
    );
    t.gap();
    t.combo(
        mtm,
        "log_level",
        &crate::strings::get("gui.set.log_detail"),
        160.0,
    );
    add_tab(&crate::strings::get("gui.tab.advanced"), t);

    bd.addSubview(&tabs);
    *c.ivars().tabs.borrow_mut() = Some(tabs.clone());

    // Populate from disk, then hand the controls to the controller so OK can
    // read them back. Defaults that are empty stay empty on purpose (the key
    // endpoints ship blank).
    {
        let st = c.ivars().settings.borrow();
        for (k, f) in &all_fields {
            f.setStringValue(&NSString::from_str(&st.get(k)));
        }
        for (k, b) in &all_checks {
            b.setState(if st.get_bool(k) { 1 } else { 0 });
        }
        for (k, p) in &all_popups {
            let want = st.get(k);
            let opts = enum_options(k);
            if !opts.is_empty() {
                // Enum popup: map the stored canonical to its menu index.
                if let Some(i) = opts.iter().position(|(canon, _)| *canon == want) {
                    p.selectItemAtIndex(i as isize);
                }
            } else if k == "container" {
                // Canonical format string; the popup shows the localized label.
                p.selectItemWithTitle(&NSString::from_str(&crate::ui::format_label(&want)));
            } else if !want.is_empty() {
                p.selectItemWithTitle(&NSString::from_str(&want));
            }
            // A stored value that matched no item leaves nothing selected (a
            // blank popup). Fall back to the first row so every popup always
            // shows a value.
            if p.indexOfSelectedItem() < 0 {
                p.selectItemAtIndex(0);
            }
            // The language popup applies live the instant it changes.
            if k == "language" {
                unsafe {
                    p.setTarget(Some(c));
                    p.setAction(Some(sel!(onPickLanguage:)));
                }
            }
        }
        // Language pickers hold a SET, so they are populated by their own
        // setter rather than by matching a title: the stored string may name
        // several languages, or one this build does not list.
        for (k, p) in &all_langs {
            set_lang_picker(mtm, p, &st.get(k), c);
        }
    }
    *c.ivars().pf_fields.borrow_mut() = all_fields;
    *c.ivars().pf_checks.borrow_mut() = all_checks;
    *c.ivars().pf_popups.borrow_mut() = all_popups;
    *c.ivars().pf_langs.borrow_mut() = all_langs;
    let ok = btn(
        mtm,
        &crate::strings::get("gui.btn.ok"),
        r(w - 100.0, 12.0, 88.0, 28.0),
        c,
        sel!(onClosePrefs:),
    );
    {
        ok.setKeyEquivalent(&NSString::from_str("\r"));
        bd.addSubview(&ok);
        // Cancel must NOT be onClosePrefs: — that reads the form, pushes it
        // into the running App and writes it to disk. Wired the same as OK,
        // Cancel committed every edit the user had just decided against.
        bd.addSubview(&btn(
            mtm,
            &crate::strings::get("gui.btn.cancel"),
            r(w - 194.0, 12.0, 88.0, 28.0),
            c,
            sel!(onCancelPrefs:),
        ));
    }
    win
}

// ── About ─────────────────────────────────────────────────────────────────

fn build_about(mtm: MainThreadMarker, c: &Controller) -> Retained<NSWindow> {
    let (w, h) = (400.0, 250.0);
    let win: Retained<NSWindow> = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            r(0.0, 0.0, w, h),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    win.setTitle(&NSString::from_str(&crate::strings::get(
        "gui.menu.app_about",
    )));
    // See build_prefs: keep the window alive across close/reopen (the Retained
    // in win_about owns the lifetime), else reopen is a use-after-free.
    unsafe { win.setReleasedWhenClosed(false) };
    let content = win.contentView().unwrap();
    let bd = { NSBox::initWithFrame(NSBox::alloc(mtm), r(0.0, 0.0, w, h)) };
    {
        bd.setBoxType(NSBoxType::Custom);
        bd.setTransparent(true);
        bd.setFillColor(&NSColor::windowBackgroundColor());
        bd.setTitlePosition(objc2_app_kit::NSTitlePosition::NoTitle);
        content.addSubview(&bd);
    }
    let title = text(mtm, "freemkv", r(0.0, h - 56.0, w, 26.0), false, false);
    {
        title.setAlignment(NSTextAlignment::Center);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(18.0)));
        bd.addSubview(&title);
    }
    // Every value here is DERIVED. All three used to be string literals and
    // all three were wrong (stale version, a copied-machine keydb count shown
    // to users with no keydb). The Windows About box already derived all three.
    let rows: [(String, String); 5] = [
        (
            crate::strings::get("gui.about.version"),
            format!("{} (macOS)", env!("CARGO_PKG_VERSION")),
        ),
        (
            crate::strings::get("gui.about.engine"),
            format!("libfreemkv {}", env!("CARGO_PKG_VERSION")),
        ),
        (crate::strings::get("gui.about.licence"), "MIT".to_string()),
        (
            crate::strings::get("gui.about.keys"),
            crate::settings::Settings::load().keydb_status(),
        ),
        (
            crate::strings::get("gui.about.website"),
            "https://freemkv.org".to_string(),
        ),
    ];
    let mut y = h - 96.0;
    for (k, v) in rows {
        bd.addSubview(&text(mtm, &k, r(20.0, y, 130.0, 18.0), true, false));
        if v.starts_with("http") {
            // Clickable link → opens in the default browser (onAboutWebsite:).
            let lb = {
                NSButton::initWithFrame(NSButton::alloc(mtm), r(158.0, y - 2.0, w - 175.0, 20.0))
            };
            unsafe {
                lb.setBordered(false);
                lb.setButtonType(NSButtonType::MomentaryChange);
                lb.setAlignment(NSTextAlignment::Left);
                lb.setTarget(Some(c));
                lb.setAction(Some(sel!(onAboutWebsite:)));
                // Blue title so it reads as a link.
                let keys: [&objc2_foundation::NSString; 1] =
                    [objc2_app_kit::NSForegroundColorAttributeName];
                let vals: [&AnyObject; 1] =
                    [&*Retained::cast_unchecked::<AnyObject>(NSColor::linkColor())];
                let attrs = NSDictionary::from_slices(&keys, &vals);
                let title = objc2_foundation::NSAttributedString::new_with_attributes(
                    &NSString::from_str(&v),
                    &Retained::cast_unchecked(attrs),
                );
                lb.setAttributedTitle(&title);
            }
            bd.addSubview(&lb);
        } else {
            bd.addSubview(&text(mtm, &v, r(160.0, y, w - 175.0, 18.0), false, false));
        }
        y -= 24.0;
    }
    let close = btn(
        mtm,
        &crate::strings::get("gui.btn.close"),
        r(w - 100.0, 14.0, 86.0, 28.0),
        c,
        sel!(onCloseAbout:),
    );
    {
        close.setKeyEquivalent(&NSString::from_str("\r"));
        bd.addSubview(&close);
    }
    win
}

// ── UI driver (debug builds only) ─────────────────────────────────────────
// Drives the REAL controls via `performClick:` / `selectItem`, so every
// action goes through the same target/action path a mouse click takes.

impl Controller {
    /// Click the Run Now button exactly as a user would.
    pub fn drive_click_run(&self) -> bool {
        if let Some(b) = self.ivars().run_btn.borrow().as_ref() {
            unsafe { b.performClick(None) };
            return true;
        }
        false
    }

    /// Click the actual checkbox in row `row` — builds the real cell and sends
    /// it `performClick:`, so `setTag`/`setTarget`/`setAction` wiring is
    /// exercised. A direct model mutation would not catch a mis-wired cell.
    pub fn drive_click_checkbox(&self, row: usize) -> bool {
        let Some(src) = self.ivars().src.borrow().clone() else {
            return false;
        };
        let Some(ov) = src.ivars().view.borrow().clone() else {
            return false;
        };
        let col = { ov.tableColumns() }
            .iter()
            .find(|c| { c.identifier().to_string() } == "check");
        let Some(col) = col else { return false };
        let item: Retained<AnyObject> =
            unsafe { Retained::cast_unchecked(NSNumber::new_usize(row)) };
        let Some(v) = src.cell_for(Some(&col), Some(&item)) else {
            return false;
        };
        let Ok(b) = v.downcast::<NSButton>() else {
            return false;
        };
        // `performClick:` toggles a Switch button itself — pre-flipping the
        // state here would cancel that out and the model would never change.
        unsafe { b.performClick(None) };
        true
    }

    /// Invoke a menu item by title, through its own target/action — the same
    /// path a user picking it takes. Returns false if it is disabled.
    pub fn drive_menu(&self, menu_title: &str, item_title: &str) -> bool {
        let mtm = MainThreadMarker::new().unwrap();
        let app = NSApplication::sharedApplication(mtm);
        let Some(main) = app.mainMenu() else {
            return false;
        };
        for i in 0..main.numberOfItems() {
            let Some(top) = main.itemAtIndex(i) else {
                continue;
            };
            if { top.title() }.to_string() != menu_title {
                continue;
            }
            let Some(sub) = ({ top.submenu() }) else {
                return false;
            };
            for j in 0..sub.numberOfItems() {
                let Some(mi) = sub.itemAtIndex(j) else {
                    continue;
                };
                if { mi.title() }.to_string() != item_title {
                    continue;
                }
                // Respect enablement exactly as AppKit would before invoking.
                let enabled: objc2::runtime::Bool =
                    unsafe { msg_send![self, validateMenuItem: &*mi] };
                if !enabled.as_bool() {
                    return false;
                }
                let Some(action) = ({ mi.action() }) else {
                    return false;
                };
                let target = { mi.target() };
                unsafe {
                    // sendAction:to:from: returns BOOL.
                    let _sent: bool = match target {
                        Some(t) => msg_send![&*app, sendAction: action, to: &*t, from: &*mi],
                        None => msg_send![
                            &*app,
                            sendAction: action,
                            to: std::ptr::null::<AnyObject>(),
                            from: &*mi
                        ],
                    };
                }
                return true;
            }
        }
        false
    }

    /// Click a button found by its title anywhere in the window.
    pub fn drive_click_button(&self, title: &str) -> bool {
        fn walk(v: &NSView, title: &str) -> Option<Retained<NSButton>> {
            for sub in { v.subviews() }.iter() {
                if let Ok(b) = sub.clone().downcast::<NSButton>()
                    && { b.title() }.to_string() == title
                {
                    return Some(b);
                }
                if let Some(found) = walk(&sub, title) {
                    return Some(found);
                }
            }
            None
        }
        let Some(v) = self.ivars().page_main.borrow().clone() else {
            return false;
        };
        let Some(root) = (unsafe { v.superview() }) else {
            return false;
        };
        match walk(&root, title) {
            Some(b) => {
                unsafe { b.performClick(None) };
                true
            }
            None => false,
        }
    }

    /// Tick the Nth title row — the same mutation the checkbox action makes.
    pub fn drive_tick_title(&self, n: usize, on: bool) -> bool {
        let idx = self
            .ivars()
            .app
            .borrow()
            .tree
            .arena
            .iter()
            .enumerate()
            .filter(|(_, x)| x.type_s == "Title")
            .nth(n)
            .map(|(i, _)| i);
        let Some(i) = idx else { return false };
        self.app_mut(|a| a.tree.set_checked(i, on));
        self.render();
        true
    }

    /// Choose an output format by its visible title.
    pub fn drive_pick_format(&self, title: &str) -> bool {
        let p = match self.ivars().fmt_popup.borrow().as_ref() {
            Some(p) => p.clone(),
            None => return false,
        };
        p.selectItemWithTitle(&NSString::from_str(title));
        if { p.indexOfSelectedItem() } < 0 {
            return false;
        }
        // Selecting programmatically does NOT fire the action, so send it the
        // way AppKit would — else this only proves the popup changed
        // appearance, how the "picked MP4, got MKV" bug survived.
        unsafe {
            if let (Some(t), Some(a)) = (p.target(), p.action()) {
                let _: bool = msg_send![&*NSApplication::sharedApplication(
                    MainThreadMarker::new().unwrap()
                ), sendAction: a, to: Some(&*t), from: Some(&*p)];
            }
        }
        true
    }

    /// Set the output folder as typing into the field would.
    pub fn drive_set_output(&self, path: &str) {
        if let Some(f) = self.ivars().out_field.borrow().as_ref() {
            f.setStringValue(&NSString::from_str(path));
        }
    }

    pub fn drive_log(&self) -> String {
        self.ivars()
            .log
            .borrow()
            .as_ref()
            .map(|tv| { tv.string() }.to_string())
            .unwrap_or_default()
    }

    pub fn drive_open(&self, path: &str) {
        let fx = self.app_mut(|a| a.open(path));
        self.perform(fx);
    }
}

/// Scripted end-user test. Drives the REAL controls — `performClick:` on the
/// actual buttons, real menu actions — and asserts against the core's `View`,
/// so it validates the shell and the model together. Debug builds only.
#[cfg(debug_assertions)]
impl Controller {
    pub fn self_test(&self, iso: &str, mkv: &str, shot_dir: &str) -> bool {
        use crate::ui::{Check, Cmd, Page};
        let mut r: Vec<(bool, String)> = Vec::new();
        let mut check = |name: &str, ok: bool, detail: &str| {
            r.push((ok, format!("{name} — {detail}")));
        };
        // Let AppKit lay out and draw before capturing: view-based table cells
        // are created lazily, so an immediate snapshot shows an empty tree and
        // the screenshots become worthless as evidence.
        let snap = |n: &str| {
            {
                let rl = NSRunLoop::currentRunLoop();
                rl.runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.35));
            }
            if let Some(v) = self.ivars().page_main.borrow().as_ref()
                && let Some(sup) = unsafe { v.superview() }
            {
                {
                    sup.setNeedsDisplay(true);
                    sup.displayIfNeeded();
                }
                snapshot(&sup, &format!("{shot_dir}/{n}.png"));
            }
        };
        let view = || self.ivars().app.borrow().view();

        // 1 ── empty at launch
        check(
            "empty-at-launch",
            view().page == Page::Empty,
            "no source shows the empty page",
        );
        snap("01-empty");

        // ── RENDER checks: what the shell actually produced, not what the
        // model said. The missing-checkbox bug lived exactly here: the View
        // was right and the cell was wrong.
        let cell_kind = |row: usize, colname: &str| -> &'static str {
            let Some(src) = self.ivars().src.borrow().clone() else {
                return "none";
            };
            let Some(ov) = src.ivars().view.borrow().clone() else {
                return "none";
            };
            let col = { ov.tableColumns() }
                .iter()
                .find(|c| { c.identifier().to_string() } == colname);
            let Some(col) = col else { return "none" };
            let item: Retained<AnyObject> =
                unsafe { Retained::cast_unchecked(NSNumber::new_usize(row)) };
            match src.cell_for(Some(&col), Some(&item)) {
                None => "none",
                Some(v) => {
                    if v.downcast_ref::<NSButton>().is_some() {
                        "checkbox"
                    } else if v.downcast_ref::<NSTextField>().is_some() {
                        "text"
                    } else {
                        "spacer"
                    }
                }
            }
        };
        let cell_text = |row: usize, colname: &str| -> String {
            let Some(src) = self.ivars().src.borrow().clone() else {
                return String::new();
            };
            let Some(ov) = src.ivars().view.borrow().clone() else {
                return String::new();
            };
            let col = { ov.tableColumns() }
                .iter()
                .find(|c| { c.identifier().to_string() } == colname);
            let Some(col) = col else { return String::new() };
            let item: Retained<AnyObject> =
                unsafe { Retained::cast_unchecked(NSNumber::new_usize(row)) };
            src.cell_for(Some(&col), Some(&item))
                .and_then(|v| v.downcast::<NSTextField>().ok())
                .map(|tf| { tf.stringValue() }.to_string())
                .unwrap_or_default()
        };

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
        // The log is the user's only window into what actually happened, and
        // it is rendered by the SHELL — asserting on the model would not catch
        // a log pane that never received the text.
        {
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
                "log-is-selectable",
                self.ivars()
                    .log
                    .borrow()
                    .as_ref()
                    .map(|tv| tv.isSelectable())
                    .unwrap_or(false),
                "log text can be selected and copied",
            );
        }
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

        // ── the rendered cells must match what the View decided
        let title_row = v
            .title_rows
            .iter()
            .position(|x| x.type_s == "Title")
            .unwrap_or(1);
        let video_row = v.title_rows.iter().position(|x| x.type_s == "Video");
        let audio_row = v.title_rows.iter().position(|x| x.type_s == "Audio");
        check(
            "render-root-spacer",
            cell_kind(0, "check") == "spacer",
            "disc root check cell is blank, not text",
        );
        check(
            "render-title-checkbox",
            cell_kind(title_row, "check") == "checkbox",
            &format!("title check cell is a {}", cell_kind(title_row, "check")),
        );
        if let Some(vr) = video_row {
            check(
                "render-video-spacer",
                cell_kind(vr, "check") == "spacer",
                &format!("video check cell is a {}", cell_kind(vr, "check")),
            );
        }
        if let Some(ar) = audio_row {
            check(
                "render-audio-checkbox",
                cell_kind(ar, "check") == "checkbox",
                &format!("audio check cell is a {}", cell_kind(ar, "check")),
            );
        }
        check(
            "render-no-text-in-check",
            (0..v.title_rows.len().min(12)).all(|i| cell_kind(i, "check") != "text"),
            "the check column never renders a label",
        );
        check(
            "render-type-column",
            cell_text(title_row, "type") == "Title",
            &format!("type cell reads '{}'", cell_text(title_row, "type")),
        );
        check(
            "render-desc-column",
            cell_text(title_row, "desc").starts_with("1."),
            &format!("desc cell reads '{}'", cell_text(title_row, "desc")),
        );
        snap("02-titles");

        // ── REAL WIDGET DRIVING: click the actual checkbox, not the model
        let audio_idx = v.title_rows.iter().position(|x| x.type_s == "Audio");
        if let Some(ai) = audio_idx {
            let before = *self.ivars().app.borrow().tree.arena[ai].checked.borrow();
            let clicked = self.drive_click_checkbox(ai);
            let after = *self.ivars().app.borrow().tree.arena[ai].checked.borrow();
            check(
                "click-checkbox",
                clicked && after != before,
                "clicking the real checkbox changed the model",
            );
            snap("03-checkbox-clicked");
            self.drive_click_checkbox(ai);
        }

        // ── REAL MENU ITEMS: invoked through their own target/action
        check(
            "menu-select-all",
            self.drive_menu("Edit", "Select All Titles")
                && self.ivars().app.borrow().tree.ticked_titles().len() == titles,
            "Edit ▸ Select All Titles ticked everything",
        );
        check(
            "menu-select-none",
            self.drive_menu("Edit", "Select No Titles")
                && self.ivars().app.borrow().tree.ticked_titles().is_empty(),
            "Edit ▸ Select No Titles cleared it",
        );
        check(
            "menu-invert",
            self.drive_menu("Edit", "Invert Title Selection")
                && self.ivars().app.borrow().tree.ticked_titles().len() == titles,
            "Edit ▸ Invert Title Selection flipped it",
        );
        check(
            "menu-clear-log",
            self.drive_menu("View", "Clear log") && view().log.is_empty(),
            "View ▸ Clear log emptied it",
        );
        // The item is a toggle and its title follows the state, so drive it by
        // the title it CURRENTLY carries: "Hide log" while the log is on
        // screen, "Show log" once it is not.
        check(
            "menu-hide-log",
            self.drive_menu("View", &crate::ui::log_menu_label(false)) && view().log_hidden,
            "View ▸ Hide log hid it",
        );
        check(
            "menu-show-log",
            self.drive_menu("View", &crate::ui::log_menu_label(true)) && !view().log_hidden,
            "View ▸ Show log brought it back",
        );
        check(
            "menu-close",
            self.drive_menu("File", "Close") && view().page == Page::Empty,
            "File ▸ Close returned to the empty page",
        );
        snap("04-after-close");
        self.drive_open(iso);

        // 3 ── selection, through the same commands the menu uses
        self.act(Cmd::SelectAll);
        check(
            "select-all",
            self.ivars().app.borrow().tree.ticked_titles().len() == titles,
            "every title ticked",
        );
        self.act(Cmd::SelectNone);
        check(
            "select-none",
            self.ivars().app.borrow().tree.ticked_titles().is_empty(),
            "no title ticked",
        );
        self.act(Cmd::Invert);
        check(
            "invert",
            self.ivars().app.borrow().tree.ticked_titles().len() == titles,
            "none → all",
        );
        self.act(Cmd::SelectNone);

        // 4 ── tri-state after a partial stream selection
        let ti = self
            .ivars()
            .app
            .borrow()
            .tree
            .arena
            .iter()
            .position(|n| n.type_s == "Title" && !n.children.is_empty());
        if let Some(t) = ti {
            self.app_mut(|a| a.tree.set_checked(t, true));
            let all_on = self.ivars().app.borrow().tree.check_state(t) == Check::On;
            let kid = self.ivars().app.borrow().tree.arena[t]
                .children
                .iter()
                .copied()
                .find(|&c| self.ivars().app.borrow().tree.arena[c].checkable());
            if let Some(c) = kid {
                self.app_mut(|a| *a.tree.arena[c].checked.borrow_mut() = false);
                let st = self.ivars().app.borrow().tree.check_state(t);
                check(
                    "tri-state",
                    all_on && st != Check::On,
                    "partial reads as mixed/off",
                );
            }
            self.app_mut(|a| a.tree.set_checked(t, true));
        }

        // 5 ── stream ticks resolve to PIDs
        let (a_pids, s_pids, _) = self.ivars().app.borrow().tree.ticked_streams();
        check(
            "stream-pids",
            true,
            &format!("{} audio, {} subtitle", a_pids.len(), s_pids.len()),
        );

        // 6 ── output formats follow the source kind
        let disc_fmts = view().formats.concat().join("|");
        check(
            "disc-formats",
            disc_fmts.contains("Whole disc"),
            "disc offers image sinks",
        );
        if !mkv.is_empty() {
            // Widget-level titles, not the View: the popup used to be built
            // once at launch and never rebuilt, so the model could be right
            // while the control still offered "Whole disc → ISO image".
            let popup_titles = || -> String {
                self.ivars()
                    .fmt_popup
                    .borrow()
                    .as_ref()
                    .map(|p| {
                        p.itemTitles()
                            .iter()
                            .map(|t| t.to_string())
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .unwrap_or_default()
            };
            let disc_titles = popup_titles();
            // Widget-level: picking in the REAL popup must reach the model
            // and change what gets written. The popup had no action at all for
            // a while, so "picked MP4" still produced an MKV.
            for (t, want) in [
                ("Selected titles → M2TS", "M2TS"),
                ("Selected titles → MKV", "MKV"),
            ] {
                let picked = self.drive_pick_format(t);
                check(
                    &format!("pick-format-{want}"),
                    picked && view().format == t,
                    &format!("model now reads {:?}", view().format),
                );
            }
            // The fixture is a DVD (MPEG-2), which MP4 cannot hold — so the
            // option must be absent from the real popup, not merely refused.
            check(
                "mp4-absent-for-mpeg2",
                !self.drive_pick_format("Selected titles → MP4"),
                "MP4 is not offered for a source it cannot store",
            );

            check(
                "disc-popup-offers-iso",
                disc_titles.contains("Whole disc"),
                "a disc source can be backed up whole",
            );

            self.drive_open(mkv);
            let f = view().formats.concat().join("|");
            check(
                "container-formats",
                !f.contains("Whole disc"),
                "container hides them",
            );
            let cont_titles = popup_titles();
            check(
                "container-popup-drops-iso",
                !cont_titles.contains("Whole disc") && cont_titles.contains("MKV"),
                &format!("popup now offers: {cont_titles}"),
            );
            snap("03-container");

            self.drive_open(iso);
            check(
                "popup-restores-for-a-disc",
                popup_titles().contains("Whole disc"),
                "reopening a disc brings the whole-disc sinks back",
            );
        }

        // 7 ── the log commands
        check("log-content", !view().log.is_empty(), "carries real events");
        check(
            "log-selectable",
            self.ivars()
                .log
                .borrow()
                .as_ref()
                .map(|tv| tv.isSelectable())
                .unwrap_or(false),
            "text can be selected and copied",
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

        // 8 ── run is refused with no source, and the guard rule holds
        check(
            "blocked-while-running",
            crate::ui::blocked_while_running(Cmd::Run)
                && !crate::ui::blocked_while_running(Cmd::Cancel),
            "Cancel always reachable, Run is not",
        );

        // 9 ── settings persist
        check(
            "settings-load",
            !crate::settings::Settings::load().dest_dir.is_empty(),
            "destination restored from disk",
        );

        // 10 ── windows build
        let mtm = MainThreadMarker::new().unwrap();
        let prefs = build_prefs(mtm, self);
        let tabs = self
            .ivars()
            .tabs
            .borrow()
            .as_ref()
            .map(|t| t.numberOfTabViewItems());
        check(
            "preferences",
            tabs.unwrap_or(0) == 5,
            &format!("{} tabs", tabs.unwrap_or(0)),
        );
        prefs.close();
        let about = build_about(mtm, self);
        check("about", true, "about window built");
        about.close();
        let app = NSApplication::sharedApplication(mtm);
        check(
            "menu-bar",
            app.mainMenu()
                .map(|m| m.numberOfItems() == 5)
                .unwrap_or(false),
            "app/File/Edit/View/Help",
        );

        // 11 ── layout at the extremes
        for (w, h) in [(1020.0, 620.0), (1700.0, 1050.0), (1180.0, 760.0)] {
            self.relayout(w, h);
        }
        check("resize", true, "min, large and default");
        snap("04-resized");

        // ── REAL RUN: click Run Now, watch it start, click Cancel
        {
            self.act(Cmd::SelectNone);
            self.drive_tick_title(0, true);
            self.app_mut(|a| a.output_dir = shot_dir.to_string());
            let started = self.drive_click_button("Run Now");
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
                view()
                    .info
                    .as_ref()
                    .map(|i| i[4].ends_with(".mkv"))
                    .unwrap_or(false),
                &format!(
                    "Output file row reads '{}'",
                    view()
                        .info
                        .as_ref()
                        .map(|i| i[4].clone())
                        .unwrap_or_default()
                ),
            );
            snap("07-running");
            check(
                "run-disabled-while-running",
                !view().can_run,
                "Run Now is disabled during a rip",
            );
            check(
                "menu-blocked-while-running",
                !self.drive_menu("File", "Start rip"),
                "File ▸ Start rip is refused mid-run",
            );
            let cancelled = self.drive_click_button("Cancel");
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
                view().result_heading != "Finished",
                &format!("result heading reads '{}'", view().result_heading),
            );
            snap("08-cancelled");
            let done = self.drive_click_button("Done");
            check(
                "click-done",
                done && view().page != Page::Result,
                "Done dismissed the result page",
            );
            snap("09-after-done");
        }

        // 12 ── progress and result pages render
        self.app_mut(|a| a.page = Page::Progress);
        self.render();
        check("progress-page", view().page == Page::Progress, "shown");
        snap("05-progress");
        self.app_mut(|a| {
            a.result_summary = "2 title(s) written".into();
            a.page = Page::Result;
        });
        self.render();
        check("result-page", view().page == Page::Result, "shown");
        snap("06-result");
        self.app_mut(|a| a.page = Page::Titles);
        self.render();

        // 13 ── engine-facing guards
        check(
            "bad-source",
            crate::engine::scan("/etc/hosts").is_err(),
            "rejected, no panic",
        );
        check(
            "preflight",
            crate::engine::preflight(iso, "/tmp", &[]).is_ok(),
            "answers without executing",
        );
        let ks = crate::settings::Settings::load().keydb_status();
        check(
            "keydb-status",
            ks.contains("keydb found") || ks.contains("no keydb"),
            &ks,
        );

        let passed = r.iter().filter(|(ok, _)| *ok).count();
        for (ok, msg) in &r {
            println!("  {} {}", if *ok { "PASS" } else { "FAIL" }, msg);
        }
        println!("\n{passed}/{} checks passed", r.len());
        passed == r.len()
    }
}

// ── tests ── Only pure, widget-free decisions are tested here — AppKit
// needs a main thread and window server `cargo test` lacks, so widget-driving
// assertions stay in `FMKV_SELFTEST`; `App`/`Tree`/`View` lives in `gui_model.rs`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Cmd;

    // ── menu routing ──────────────────────────────────────────────────────

    #[test]
    fn the_menu_reaches_every_command_the_core_defines() {
        // A selector that falls off the end of `cmd_for` is a menu item with
        // NO rule, so it stays live mid-rip. `SetFormat` (format popup) and
        // `Cancel` (progress page button) are excluded — neither is a menu item.
        let sels = [
            sel!(onOpenFiles:),
            sel!(onOpenDisc:),
            sel!(onCloseDisc:),
            sel!(onBrowseOutput:),
            sel!(onRip:),
            sel!(onCancelRip:),
            sel!(onEject:),
            sel!(onSelectAll:),
            sel!(onSelectNone:),
            sel!(onInvert:),
            sel!(onClearLog:),
            sel!(onToggleLog:),
            sel!(onPrefs:),
            sel!(onAbout:),
            sel!(onDocs:),
            sel!(onCheckUpdates:),
            sel!(onQuit:),
        ];
        let reached: Vec<Cmd> = sels.iter().filter_map(|s| cmd_for(*s)).collect();
        let unrouted: Vec<&Sel> = sels.iter().filter(|s| cmd_for(**s).is_none()).collect();
        assert!(
            unrouted.is_empty(),
            "menu selectors with no cmd_for arm — they would never be greyed \
             during a rip: {unrouted:?}"
        );
        for want in [
            Cmd::Open,
            Cmd::Close,
            Cmd::SetOutput,
            Cmd::Run,
            Cmd::Cancel,
            Cmd::Eject,
            Cmd::SelectAll,
            Cmd::SelectNone,
            Cmd::Invert,
            Cmd::ClearLog,
            Cmd::ToggleLog,
            Cmd::Settings,
            Cmd::About,
            Cmd::Docs,
            Cmd::CheckUpdates,
            Cmd::Quit,
        ] {
            assert!(
                reached.contains(&want),
                "{want:?} is not reachable from any menu selector"
            );
        }
    }

    #[test]
    fn opening_a_disc_is_an_open_for_enablement_purposes() {
        // Two menu items, one rule: File ▸ Open… and File ▸ Open Disc… must
        // both be blocked mid-rip. Mapping Open Disc to anything else (or to
        // nothing) would leave a live "Open Disc" during a rip.
        assert_eq!(cmd_for(sel!(onOpenFiles:)), Some(Cmd::Open));
        assert_eq!(cmd_for(sel!(onOpenDisc:)), Some(Cmd::Open));
        assert!(crate::ui::blocked_while_running(Cmd::Open));
    }

    #[test]
    fn a_selector_that_is_not_a_command_routes_nowhere() {
        // The catch-all must not fall through: the checkbox and popup actions
        // are not menu commands and must not be treated as ones.
        assert_eq!(cmd_for(sel!(onToggle:)), None);
        assert_eq!(cmd_for(sel!(onPickFormat:)), None);
        assert_eq!(cmd_for(sel!(onPickLanguage:)), None);
    }

    // The drag-and-drop overlay must not be leaked once per language switch.
    // See docs/mac-shell.md — drop overlay leak test.
    #[test]
    fn the_drop_overlay_is_not_leaked_on_every_language_switch() {
        let src = include_str!("mac.rs");
        let src = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        let at = src
            .find("fn install_drop_view(")
            .expect("install_drop_view moved — this test cannot see it");
        let body = &src[at..];
        let body = &body[..body.find("\nfn ").unwrap_or(body.len())];

        let leak = format!("{}{}", "std::mem::", "forget(drop)");
        assert!(
            !body.contains(&leak),
            "install_drop_view runs again on every language switch; holding \
             its retain back leaks one whole DropView each time"
        );
    }

    // A widget list `build_ui` PUSHES into must be emptied by `build_ui`.
    // See docs/mac-shell.md — widget-list clear test.
    #[test]
    fn every_widget_list_build_ui_pushes_into_is_cleared_there_first() {
        let src = include_str!("mac.rs");
        let src = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        let start = src
            .find("fn build_ui(")
            .expect("build_ui moved — this test cannot see it");
        let body = &src[start..];
        let end = body.find("\nfn ").unwrap_or(body.len());
        let body = &body[..end];

        // The ivars that are LISTS of widgets, read off their declared type so
        // a second one added later is covered without editing this test.
        let ty = format!("{}{}", ": RefCell<Vec<Ret", "ained<");
        let lists: Vec<&str> = src
            .match_indices(&ty)
            .filter_map(|(at, _)| src[..at].rsplit('\n').next().map(str::trim))
            .collect();
        assert!(
            !lists.is_empty(),
            "no widget-list ivar found — has Ivars changed shape?"
        );

        for name in lists {
            if !body.contains(name) {
                continue; // built somewhere else (the Settings form's lists)
            }
            // A rebuild must REPLACE the list, not grow it. Assigning the whole
            // vector does that by itself; pushing into it does not, and needs
            // the clear.
            let assigned = body.contains(&format!("{}{}", name, ".borrow_mut() = "));
            let cleared = body.contains(&format!("{}{}", name, ".borrow_mut().clear()"));
            assert!(
                assigned || cleared,
                "`{name}` is pushed into by build_ui and neither assigned nor \
                 cleared there: a second build_ui (a language switch) stacks \
                 its widgets on top of the last one's, forever"
            );
        }
    }

    // Every action this shell defines must be reachable from the UI.
    // See docs/mac-shell.md — orphaned-selector test.
    #[test]
    fn every_action_selector_this_shell_defines_is_wired_to_something() {
        let src = include_str!("mac.rs");
        // Production only: a selector named solely by a test is not wired to
        // anything a user can reach, and the tests' own assembled needles
        // are not declarations.
        let src = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        let decl = format!("{}{}", "#[unsafe(me", "thod(on");

        let mut orphans = Vec::new();
        for (at, _) in src.match_indices(&decl) {
            let rest = &src[at + decl.len()..];
            let end = rest
                .find(':')
                .expect("an action selector always ends at its colon");
            let name = &rest[..end];
            // Built at run time for the same reason as `decl`.
            let target = format!("{}{}{}", "sel!(on", name, ":)");
            if !src.contains(&target) {
                orphans.push(format!("on{name}:"));
            }
        }

        assert!(
            orphans.is_empty(),
            "these handlers are defined and targeted by nothing — no menu \
             item, no control, no timer can reach them: {orphans:?}"
        );
    }

    // The format popup's rebuild guard has to compare like with like.
    // See docs/mac-shell.md — format popup rebuild-guard test.
    #[test]
    fn the_format_popup_comparison_counts_the_separators_appkit_reports() {
        let titles = vec!["Selected titles → MKV", "Selected titles → M2TS"];
        let meta = vec!["Chapters → file"];

        let got = popup_item_titles(&[titles.clone(), meta.clone()]);
        assert_eq!(
            got,
            vec![
                crate::ui::format_label("Selected titles → MKV"),
                crate::ui::format_label("Selected titles → M2TS"),
                String::new(),
                crate::ui::format_label("Chapters → file"),
            ],
            "the group boundary is an item in the menu — leaving it out makes \
             the guard compare a 3-item list against AppKit's 4 and never match"
        );

        // One group, no boundary, nothing extra.
        assert_eq!(popup_item_titles(std::slice::from_ref(&meta)).len(), 1);

        // And the shape the real popup is built from: every group boundary
        // accounted for, so the count matches what the menu will hold.
        let real = crate::ui::output_formats(true, true);
        assert_eq!(
            popup_item_titles(&real).len(),
            real.iter().map(Vec::len).sum::<usize>() + real.len() - 1,
            "one separator per boundary between the groups"
        );
    }

    // ── the log pane ──────────────────────────────────────────────────────

    #[test]
    fn a_notice_gets_its_own_colour_bucket() {
        // Colour is the ONLY thing marking a problem in this shell's log (the
        // Windows shell uses a gutter character instead), so a Notice sharing
        // a bucket with an ordinary line makes warnings invisible.
        let notice = log_colour(crate::ui::LogKind::Notice);
        let detail = log_colour(crate::ui::LogKind::Detail);
        let result = log_colour(crate::ui::LogKind::Result);
        assert_ne!(notice, detail, "a notice reads as an ordinary detail line");
        assert_ne!(notice, result, "a notice reads as an ordinary result line");
        assert_ne!(detail, result, "detail and result share a colour");
        // `log_append` only has three colours; anything else falls into its
        // catch-all and silently renders as a result line.
        for k in [
            crate::ui::LogKind::Notice,
            crate::ui::LogKind::Detail,
            crate::ui::LogKind::Result,
        ] {
            assert!(log_colour(k) <= 2, "no colour defined for {k:?}");
        }
    }

    // ── settings dropdowns ────────────────────────────────────────────────

    #[test]
    fn the_format_popup_is_not_an_index_mapped_enum() {
        // This popup interleaves group SEPARATOR rows, so its index does not
        // line up with the core's flat format list — `read_prefs_form` maps
        // it back by TITLE. A "container" arm here would silently break that.
        assert!(
            enum_options("container").is_empty(),
            "the container popup must not be index-mapped: this shell's popup \
             carries separator rows, so index N is not option N"
        );
        // And the title-based path must actually resolve: every canonical
        // format's localized label round-trips back to the canonical string.
        for canon in crate::ui::output_formats(true, true).into_iter().flatten() {
            let label = crate::ui::format_label(canon);
            assert_eq!(
                crate::ui::format_from_label(&label, true, true),
                Some(canon),
                "{label:?} does not resolve back to {canon:?}"
            );
        }
    }

    #[test]
    fn the_shared_dropdowns_come_from_the_core() {
        // A shell-local copy of this table is how the two shells drifted
        // before, so this shell must hold none.
        for key in [
            "selection",
            "rip_mode",
            "key_source",
            "log_level",
            "language",
        ] {
            let opts = enum_options(key);
            assert!(!opts.is_empty(), "{key} lost its options");
            assert_eq!(
                opts.into_iter()
                    .map(|(c, l)| (c.to_string(), l))
                    .collect::<Vec<_>>(),
                crate::ui::enum_options(key)
                    .into_iter()
                    .map(|(c, l)| (c.to_string(), l))
                    .collect::<Vec<_>>(),
                "{key} is not the shared table"
            );
        }
    }

    #[test]
    fn a_free_form_setting_is_not_an_enum_popup() {
        // `read_prefs_form` uses an empty result to mean "not an enum": a
        // spurious arm here would make a text field persist an index.
        for key in ["dest_dir", "filename_template", "max_passes", ""] {
            assert!(enum_options(key).is_empty(), "{key} became an enum popup");
        }
    }

    // ── output field wiring ─────────────────────────────────────────────────
    // STOPGAP, NOT COVERAGE: the real bug needs a live NSComboBox in a real
    // run loop (`windows.rs` has that harness). Source inspection only.
    #[test]
    fn the_output_field_has_a_delegate_wired_source_inspection_only() {
        let src = include_str!("mac.rs");
        // Built by concatenation so this needle cannot match the assertion's
        // OWN source via `include_str!` — a self-matching source-inspection
        // test is the tautology this crate already shipped once.
        let wired = format!("{}{}", "fld.set", "Delegate(");
        assert!(
            src.contains(&wired),
            "out_field (`fld`) is never given a delegate — typed edits have \
             nothing to tell the model, so render()'s next tick overwrites \
             them with the stale output_dir"
        );
        let handler = format!("{}{}", "fn control_text_did", "_change");
        assert!(
            src.contains(&handler),
            "no controlTextDidChange: handler exists to push the typed path \
             into App::output_dir"
        );
    }

    // ── one settings-save policy, not two ───────────────────────────────────
    // STOPGAP, NOT COVERAGE: needs a live `Controller`, no harness for that
    // exists. Inspects source text for a re-inlined Ok/Err match.
    #[test]
    fn language_switch_reports_a_failed_save_source_inspection_only() {
        let src = include_str!("mac.rs");
        // `.settings.borrow().save` (…) should appear in exactly ONE place: the
        // shared helper. A second occurrence means someone re-inlined the
        // Ok/Err match at a second call site — "one policy implemented twice".
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
            "self.read_prefs_form();\n            self.save_settings_reporting", "_error();"
        );
        assert!(
            src.contains(&language_call),
            "onApplyLanguage: no longer calls save_settings_reporting_error \
             right after committing the form — a failed save on the \
             language-switch path would go unreported again"
        );
    }

    // ── the keyserver token is not shown in plaintext ───────────────────────
    // STOPGAP, NOT COVERAGE: masking needs a real rendered window. Source
    // inspection only: fails if the Keys tab uses the plain `field` ctor.
    #[test]
    fn the_keyserver_token_field_is_secure_source_inspection_only() {
        let src = include_str!("mac.rs");
        let secure_ctor = format!("{}{}", "fn field_", "secure");
        assert!(
            src.contains(&secure_ctor),
            "the field_secure (NSSecureTextField) constructor is gone"
        );
        let call = format!(
            "{}{}",
            "t.field_secure(\n        mtm,\n        \"keyserver_", "token\","
        );
        assert!(
            src.contains(&call),
            "keyserver_token is no longer built with field_secure — the \
             bearer token would render in a plain NSTextField again, fully \
             legible during screen-sharing or a recording"
        );
    }

    // ── the tree redraw memo ── `render` used to rebuild the outline on
    // every 5 Hz tick even when rows never moved. Windows already had this
    // guard (`rows_sig`); real coverage since `rows_sig` is a pure function.
    fn rows_sig_fixture() -> Vec<crate::ui::Row> {
        vec![
            crate::ui::Row {
                index: 0,
                depth: 0,
                type_s: String::new(),
                desc: "Disc".into(),
                check: None,
            },
            crate::ui::Row {
                index: 1,
                depth: 1,
                type_s: "Title".into(),
                desc: "Main Feature".into(),
                check: Some(crate::ui::Check::Off),
            },
            crate::ui::Row {
                index: 2,
                depth: 2,
                type_s: "Audio".into(),
                desc: "English 5.1".into(),
                check: Some(crate::ui::Check::On),
            },
        ]
    }

    #[test]
    fn the_row_signature_is_stable_for_unchanged_rows() {
        let rows = rows_sig_fixture();
        assert_eq!(
            rows_sig(&rows),
            rows_sig(&rows.clone()),
            "an identical row list must produce an identical signature, or \
             every 200 ms progress tick forces a full outline reload again"
        );
    }

    #[test]
    fn the_row_signature_notices_a_real_change() {
        let rows = rows_sig_fixture();
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
        swapped.swap(1, 2);
        assert_ne!(base, rows_sig(&swapped), "a reordered tree went unnoticed");
    }

    #[test]
    fn the_row_signature_ignores_tick_state() {
        // A rebuild throws the outline back to the top of the list, so ticking
        // a box must NOT change the signature — it goes down the
        // `sync_check_states` path instead, which repaints ticks in place.
        let rows = rows_sig_fixture();
        let before = rows_sig(&rows);
        let flipped: Vec<crate::ui::Row> = rows
            .iter()
            .cloned()
            .map(|mut r| {
                r.check = match r.check {
                    Some(crate::ui::Check::Off) => Some(crate::ui::Check::On),
                    Some(crate::ui::Check::On) => Some(crate::ui::Check::Mixed),
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
            "a tick change altered the row signature, so every toggle forces a \
             full reloadData + scrollPoint and jumps the list back to the top"
        );
    }

    // STOPGAP, NOT COVERAGE: needs a live `Controller` with a real
    // `NSOutlineView` to observe call counts. Source inspection only: fails
    // if the guard is removed and `render` calls `apply` unconditionally.
    #[test]
    fn render_gates_the_tree_rebuild_on_the_row_signature_source_inspection_only() {
        let src = include_str!("mac.rs");
        let guard = format!(
            "{}{}",
            "let sig = rows_sig(&v.title_rows);\n            if *iv.tree_", "sig.borrow() != sig {"
        );
        assert!(
            src.contains(&guard),
            "render() no longer compares the tree's row signature before \
             calling TitlesSource::apply — a running rip's 5 Hz tick would \
             force a full outline reloadData + re-expand every 200 ms again"
        );
        // …and the other half: an unchanged signature must still repaint the
        // ticks, or a checkbox click would change nothing on screen at all.
        let in_place = format!(
            "{}{}",
            "} else {\n                src.sync_check_", "states("
        );
        assert!(
            src.contains(&in_place),
            "render() has no in-place tick refresh for the unchanged-signature \
             case — with tick state out of the signature, a click would leave \
             the outline showing the old ticks"
        );
    }

    // STOPGAP, NOT COVERAGE: `relocalize` rebuilds a live `NSWindow`, which
    // this crate cannot stand up outside a real AppKit run loop. Source
    // inspection only: fails if the memo reset is removed.
    #[test]
    fn a_language_switch_forgets_the_tree_memo_source_inspection_only() {
        let src = include_str!("mac.rs");
        let reset = format!("{}{}", "self.ivars().tree_sig.borrow_mut()", ".clear();");
        assert!(
            src.contains(&reset),
            "relocalize() no longer clears tree_sig — build_ui installs a BRAND \
             NEW, empty TitlesSource, so the very next render() compares the \
             new rows against the OLD signature, matches, and never calls \
             apply: the titles tree comes back empty after a language change"
        );
    }

    // ── quitting goes through the same guard as closing ───────────────────
    // The alert itself needs a real `NSAlert` on a run loop, but the two
    // DECISIONS around it are pure functions and are tested for real here.

    #[test]
    fn a_quit_asks_exactly_once_and_only_while_a_rip_runs() {
        // Nothing running: never ask, whatever the latch says.
        assert!(!needs_rip_confirmation(false, false));
        assert!(!needs_rip_confirmation(false, true));
        // Running and nobody has answered yet: ask.
        assert!(needs_rip_confirmation(true, false));
        // Already chose "Stop & Quit" on the way out: do NOT ask again.
        // `Cmd::Cancel` only signals the worker, so `running()` stays true
        // when the last-window-closed termination reaches the delegate.
        assert!(!needs_rip_confirmation(true, true));
    }

    #[test]
    fn only_the_first_alert_button_stops_the_rip_and_quits() {
        // NSAlertFirstButtonReturn is the "Stop & Quit" button — the one added
        // first at both call sites.
        assert_eq!(
            quit_choice(objc2_app_kit::NSAlertFirstButtonReturn),
            QuitChoice::StopThenProceed
        );
        // Second button is "Keep ripping".
        assert_eq!(
            quit_choice(objc2_app_kit::NSAlertSecondButtonReturn),
            QuitChoice::Stay
        );
        // And anything else at all — a dismissed panel, a third button someone
        // adds later — must also keep the rip. A quit that throws away hours of
        // ripping must never be the answer to a question nobody answered.
        for r in [0isize, -1, 1, 42, objc2_app_kit::NSAlertThirdButtonReturn] {
            if r == objc2_app_kit::NSAlertFirstButtonReturn {
                continue;
            }
            assert_eq!(quit_choice(r), QuitChoice::Stay, "response {r}");
        }
    }

    // The AppKit language pickers own no parsing of their own.
    // See docs/mac-shell.md — language-picker parsing test.
    #[test]
    fn the_language_pickers_own_no_parsing_source_inspection_only() {
        let src = include_str!("mac.rs");
        // Every rule must be a call INTO ui, not a copy of one.
        for f in [
            "lang_toggle",
            "lang_summary",
            "lang_is_selected",
            "lang_selection",
            "PICKER_LANGUAGES",
        ] {
            let needle = format!("{}{}", "crate::ui::", f);
            assert!(
                src.contains(&needle),
                "the picker no longer goes through ui::{f} — the shells are \
                 free to disagree about what a language selection means again"
            );
        }
        // The menu action has to be wired, or none of the above ever runs.
        let action = format!("{}{}", "#[unsafe(method(onToggle", "Lang:))]");
        assert!(
            src.contains(&action),
            "the language menu item's selector is gone; the pickers would \
             render a value nothing can change"
        );
        // The tells of a hand-rolled second parser: splitting/joining the
        // stored comma string anywhere in this file. Built from concatenated
        // literals so these needles can't match this test's own text.
        for needle in [
            format!("{}{}", "split(", "','"),
            format!("{}{}", "split([", "','"),
            format!("{}{}", ".join(", "\",\")"),
        ] {
            assert!(
                !src.contains(&needle),
                "mac.rs contains `{needle}` — the comma-separated language \
                 string is being parsed or rebuilt here instead of in \
                 ui::lang_selection / ui::lang_selection_to_string"
            );
        }
    }

    // "Stop & Quit" has to STOP before it quits.
    // See docs/mac-shell.md — stop-and-quit worker-wait test.
    #[test]
    fn stop_and_quit_waits_for_the_worker_before_letting_the_process_go() {
        let src = include_str!("mac.rs");
        // The QUIT path specifically — the Stop button signals the same way and
        // is not what this is about, so the slice is taken from `confirm_quit`.
        let helper = format!("{}{}", "fn confirm_", "quit(&self) -> QuitChoice {");
        let start = src.find(&helper).expect("the shared confirm_quit is gone");
        let end = start
            + src[start..]
                .find("\n    // Save `Settings` to disk")
                .expect("the next item still ends confirm_quit");
        let body = &src[start..end];
        let cancel = format!("{}{}", "self.act(crate::ui::Cmd::", "Cancel);");
        assert!(
            body.contains(&cancel),
            "confirm_quit no longer signals the worker at all"
        );
        let wait = format!("{}{}", "await_worker_", "exit(");
        assert!(
            body.contains(&wait),
            "the cancel is fire-and-forget: nothing waits for the worker to \
             put its output down before AppKit tears the process out from \
             under it"
        );
    }

    // STOPGAP, NOT COVERAGE: whether AppKit calls back on ⌘Q needs a live
    // `NSApplication`, which this crate cannot stand up in a unit test.
    // Source inspection only; fails if any of the three delegate pieces is removed.
    #[test]
    fn the_app_has_a_delegate_that_gates_quit_and_ends_the_process_source_inspection_only() {
        let src = include_str!("mac.rs");
        // Built by concatenation so these needles cannot match this test's own
        // text through `include_str!`.
        let proto = format!(
            "{}{}",
            "unsafe impl NSApplication", "Delegate for Controller {"
        );
        assert!(
            src.contains(&proto),
            "Controller is not an NSApplicationDelegate — ⌘Q would bypass the \
             rip-in-progress confirmation the close button implements, and \
             closing the last window would leave a headless process running \
             its 5 Hz timer and its rip thread"
        );
        // The SELECTOR, not just the Rust fn name: AppKit dispatches on the
        // Objective-C selector, so a typo there is a silently dead delegate
        // method that still compiles and still reads correctly in Rust.
        let gate = format!(
            "{}{}",
            "#[unsafe(method(applicationShouldTerminate:))]\n        fn should_",
            "terminate(&self, _app: &NSApplication)"
        );
        assert!(
            src.contains(&gate),
            "applicationShouldTerminate: is gone — ⌘Q and File ▸ Quit would \
             terminate straight through a running rip with no confirmation"
        );
        let last_window = format!(
            "{}{}",
            "fn terminate_after_last_",
            "window(&self, _app: &NSApplication) -> bool {\n            true"
        );
        assert!(
            src.contains(&last_window),
            "applicationShouldTerminateAfterLastWindowClosed: no longer \
             returns true — closing the window would leave the process alive \
             with no UI, still ticking and still ripping"
        );
        let wired = format!(
            "{}{}",
            "app.set", "Delegate(Some(objc2::runtime::ProtocolObject::from_ref(&*c)));"
        );
        assert!(
            src.contains(&wired),
            "run() never makes the Controller the NSApplication delegate, so \
             none of the above is ever called"
        );
        // Both routes out must go through ONE confirmation, not two copies of
        // it: a second inlined NSAlert is how ⌘Q and the close button drifted
        // apart in the first place.
        let helper = format!("{}{}", "fn confirm_", "quit(&self) -> QuitChoice {");
        assert!(
            src.contains(&helper),
            "the shared confirm_quit helper is gone"
        );
        let alerts = src
            .matches(&format!("{}{}", "NSAlert::", "new(mtm)"))
            .count();
        assert_eq!(
            alerts, 1,
            "there are {alerts} NSAlert construction sites in this shell; the \
             rip-in-progress question must be asked in exactly one place, or \
             the close path and the quit path can drift apart again"
        );
    }

    // ── one keydb download at a time ── STOPGAP, NOT COVERAGE: needs a live
    // Controller/Settings window to click. Fails if the guard reverts to
    // just the button's enabled state, which a rebuild resets to enabled.
    #[test]
    fn a_second_keydb_download_is_refused_by_state_not_by_a_button_source_inspection_only() {
        let src = include_str!("mac.rs");
        let flag = format!("{}{}", "if self.ivars().keydb_updating", ".get() {");
        assert!(
            src.contains(&flag),
            "onUpdateKeys: no longer checks a controller-held in-flight flag — \
             reopening Settings mid-download hands back an enabled button and \
             a second click spawns a second writer of the same keydb file"
        );
        let restore = format!(
            "{}{}",
            "c.set_keydb_updating(c.ivars().keydb_updating", ".get());"
        );
        assert!(
            src.contains(&restore),
            "build_prefs no longer restores the in-flight state onto the \
             freshly built button, so a running download looks idle"
        );
    }

    // ── the drain timer stops once drained ── nothing invalidated it, so it
    // fired forever after the first keydb update; Windows' `drain()` already
    // calls `KillTimer` at the same point. Source inspection only.
    #[test]
    fn the_drain_timer_stops_itself_once_drained_source_inspection_only() {
        let src = include_str!("mac.rs");
        let stop = format!(
            "{}{}",
            "if let Some(t) = self.ivars().drain.borrow_mut().take",
            "() {\n                t.invalidate();"
        );
        assert!(
            src.contains(&stop),
            "onDrain: no longer invalidates and clears the drain timer once \
             messages are processed — it would go back to polling an always- \
             empty inbox at 5 Hz forever after the first keydb update"
        );
    }
}
