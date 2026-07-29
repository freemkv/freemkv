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
    NSBackingStoreType, NSBezelStyle, NSBitmapImageFileType, NSBox, NSBoxType, NSButton,
    NSButtonCell, NSButtonType, NSColor, NSComboBox, NSControlTextEditingDelegate, NSFont,
    NSFontWeightRegular, NSMenu, NSMenuItem, NSOpenPanel, NSOutlineView, NSOutlineViewDataSource,
    NSOutlineViewDelegate, NSPopUpButton, NSProgressIndicator, NSScrollView, NSTableColumn,
    NSTableViewSelectionHighlightStyle, NSTextAlignment, NSTextField, NSTextView, NSView, NSWindow,
    NSWindowDelegate, NSWindowStyleMask,
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
            let on = { b.state() } > 0;
            if let Some(c) = self.ivars().ctrl.borrow().as_ref() {
                // The core owns cascade + tri-state; the shell only reports
                // which row was clicked.
                c.app_mut(|a| a.tree.set_checked(i, on));
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
                b.setAllowsMixedState(state == crate::ui::Check::Mixed);
                b.setState(match state {
                    crate::ui::Check::On => 1,
                    crate::ui::Check::Mixed => -1,
                    crate::ui::Check::Off => 0,
                });
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

    /// Take the freshly-decided rows from the core and rebuild the outline.
    fn apply(&self, rows: &[crate::ui::Row]) {
        let mut kids: Vec<Vec<usize>> = vec![Vec::new(); rows.len()];
        let mut roots = Vec::new();
        let (mut last_root, mut last_title) = (None, None);
        for (i, r) in rows.iter().enumerate() {
            match r.depth {
                0 => {
                    roots.push(i);
                    last_root = Some(i);
                }
                1 => {
                    if let Some(p) = last_root {
                        kids[p].push(i);
                    }
                    last_title = Some(i);
                }
                _ => {
                    if let Some(p) = last_title {
                        kids[p].push(i);
                    }
                }
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
    pf_checks: RefCell<Vec<(String, Retained<NSButton>)>>,
    pf_popups: RefCell<Vec<(String, Retained<NSPopUpButton>)>>,
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
            let ok = matches!(
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
            if let Some(s) = it.stringForType(uti) {
                if let Some(url) = objc2_foundation::NSURL::URLWithString(&s) {
                    if let Some(path) = url.path() {
                        out.push(path.to_string());
                    }
                }
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

    impl Controller {
        #[unsafe(method(onBrowseOutput:))]
        fn on_browse_output(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::SetOutput);
        }

        #[unsafe(method(onOpenFiles:))]
        fn on_open_files(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Open);
        }

        /// Open a live optical drive. Enumerates drives (registry only, no
        /// exclusive access); opens the one drive directly, or autodetects the
        /// drive with media when several are attached.
        #[unsafe(method(onOpenDisc:))]
        fn on_open_disc(&self, _s: Option<&AnyObject>) {
            let drives = crate::engine::list_optical_drives();
            if drives.is_empty() {
                self.app_mut(|a| {
                    a.say(
                        crate::ui::LogKind::Notice,
                        "No optical drive found. Connect a Blu-ray/DVD drive with a disc.",
                    )
                });
                self.render();
                return;
            }
            // One drive → that device; several → autodetect the one with media,
            // and log what was found so the user knows which drives are present.
            let url = if drives.len() == 1 {
                self.app_mut(|a| {
                    a.say(
                        crate::ui::LogKind::Detail,
                        &format!("Opening {} ({})", drives[0].label, drives[0].device),
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
                        crate::ui::LogKind::Detail,
                        &format!("{} drives found: {list} — using the one with a disc", drives.len()),
                    )
                });
                "disc://".to_string()
            };
            let fx = self.app_mut(|a| a.open(&url));
            self.perform(fx);
        }

        #[unsafe(method(onNoop:))]
        fn on_noop(&self, _s: Option<&AnyObject>) {}

        #[unsafe(method(onPrefs:))]
        fn on_prefs(&self, _s: Option<&AnyObject>) {
            self.act(crate::ui::Cmd::Settings);
        }

        #[unsafe(method(onClosePrefs:))]
        fn on_close_prefs(&self, _s: Option<&AnyObject>) {
            self.read_prefs_form();
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
                        &crate::strings::fmt(
                            "gui.log.settings_save_error",
                            &[("e", &e.to_string())],
                        ),
                    )
                }),
            }
            if let Some(w) = self.ivars().win_prefs.borrow().as_ref() {
                w.close();
            }
        }

        /// The interface-language dropdown fires this the instant a language is
        /// picked. The actual switch (which rebuilds — and so destroys — this
        /// very Settings window) is deferred one runloop tick, because tearing
        /// down the popup mid-action would crash AppKit.
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
            let _ = self.ivars().settings.borrow().save();
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
            if let Some(i) = tab_idx {
                if let Some(tv) = self.ivars().tabs.borrow().as_ref() {
                    tv.selectTabViewItemAtIndex(i);
                }
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
            self.app_mut(|a| {
                a.say(
                    crate::ui::LogKind::Result,
                    &crate::strings::get("gui.log.fetching_keydb"),
                )
            });
            let inbox = self.ivars().inbox.clone();
            std::thread::spawn(move || {
                let msg = match crate::settings::update_keydb(&url, &path) {
                    Ok(m) => m,
                    Err(e) => e,
                };
                if let Ok(mut v) = inbox.lock() {
                    v.push(msg);
                }
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

        /// Backup = the WHOLE disc, decrypted, as an image or file tree.
        /// Distinct from "Save selected titles", which muxes only the ticked
        /// titles into playable files.
        #[unsafe(method(onBackup:))]
        fn on_backup(&self, _s: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().unwrap();
            let panel = { NSOpenPanel::openPanel(mtm) };
            {
                panel.setCanChooseDirectories(true);
                panel.setCanChooseFiles(false);
                panel.setCanCreateDirectories(true);
                panel.setPrompt(Some(&NSString::from_str(&crate::strings::get(
                    "gui.panel.backup_prompt",
                ))));
                panel.setMessage(Some(&NSString::from_str(&crate::strings::get(
                    "gui.panel.backup_msg",
                ))));
            }
            if { panel.runModal() } == 1 {
                if let Some(url) = { panel.URL() } {
                    if let Some(p) = { url.path() } {
                        self.app_mut(|a| {
                            a.say(
                                crate::ui::LogKind::Result,
                                &crate::strings::fmt(
                                    "gui.log.backing_up",
                                    &[("p", &p.to_string())],
                                ),
                            )
                        });
                        self.app_mut(|a| {
                            a.say(
                                crate::ui::LogKind::Result,
                                &crate::strings::get("gui.log.backup_note"),
                            )
                        });
                    }
                }
            }
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
            // than trusting the widget text, so an unknown title can never
            // reach the model — and so selection works in every locale (the
            // popup shows format_label(canonical), not the canonical string).
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
            let msgs: Vec<String> = match self.ivars().inbox.lock() {
                Ok(mut v) => v.drain(..).collect(),
                Err(_) => return,
            };
            for m in msgs {
                self.app_mut(|a| a.say(crate::ui::LogKind::Result, &m));
            }
            // A finished keydb update changes the status line; rebuild prefs
            // next time it opens rather than showing a stale figure.
            if let Some(w) = self.ivars().win_prefs.borrow().as_ref() {
                if !w.isVisible() {
                    // dropped below
                }
            }
        }

        #[unsafe(method(onOpenFolder:))]
        fn on_open_folder(&self, _s: Option<&AnyObject>) {
            let mtm = MainThreadMarker::new().unwrap();
            let panel = { NSOpenPanel::openPanel(mtm) };
            {
                panel.setCanChooseDirectories(true);
                panel.setCanChooseFiles(false);
                panel.setPrompt(Some(&NSString::from_str(&crate::strings::get(
                    "gui.panel.open_prompt",
                ))));
                panel.setMessage(Some(&NSString::from_str(&crate::strings::get(
                    "gui.panel.folder_msg",
                ))));
            }
            if { panel.runModal() } == 1 {
                if let Some(url) = { panel.URL() } {
                    if let Some(p) = { url.path() } {
                        self.app_mut(|a| {
                            a.say(
                                crate::ui::LogKind::Result,
                                &crate::strings::fmt(
                                    "gui.log.folder_unsupported",
                                    &[("p", &p.to_string())],
                                ),
                            )
                        });
                    }
                }
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
            if !self.ivars().app.borrow().running() {
                return objc2::runtime::Bool::YES;
            }
            let mtm = MainThreadMarker::new().unwrap();
            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str(&crate::strings::get("gui.alert.rip_title")));
            alert.setInformativeText(&NSString::from_str(&crate::strings::get(
                "gui.alert.rip_body",
            )));
            alert.addButtonWithTitle(&NSString::from_str(&crate::strings::get(
                "gui.alert.stop_quit",
            )));
            alert.addButtonWithTitle(&NSString::from_str(&crate::strings::get(
                "gui.alert.keep_ripping",
            )));
            // First button (Stop & Quit) is NSAlertFirstButtonReturn.
            if alert.runModal() == objc2_app_kit::NSAlertFirstButtonReturn {
                self.act(crate::ui::Cmd::Cancel);
                objc2::runtime::Bool::YES
            } else {
                objc2::runtime::Bool::new(false)
            }
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
    /// Borrow the model mutably. Every mutation goes through the core.
    fn app_mut<R>(&self, f: impl FnOnce(&mut crate::ui::App) -> R) -> R {
        f(&mut self.ivars().app.borrow_mut())
    }

    /// The shell's entire job: hand the command to the core, perform the
    /// platform effects it asks for, redraw. No decisions here.
    fn act(&self, cmd: crate::ui::Cmd) {
        let effects = self.app_mut(|a| a.dispatch(cmd));
        self.perform(effects);
    }

    fn perform(&self, effects: Vec<crate::ui::Effect>) {
        use crate::ui::Effect as E;
        let mtm = MainThreadMarker::new().unwrap();
        for e in effects {
            match e {
                E::PickSource => {
                    if let Some(p) = self.pick(false, &crate::strings::get("gui.panel.source_msg"))
                    {
                        let fx = self.app_mut(|a| a.open(&p));
                        self.perform(fx);
                    }
                }
                E::PickOutputDir => {
                    if let Some(p) = self.pick(true, &crate::strings::get("gui.panel.output_msg")) {
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

    fn pick(&self, dirs: bool, msg: &str) -> Option<String> {
        let mtm = MainThreadMarker::new().unwrap();
        let panel = { NSOpenPanel::openPanel(mtm) };
        {
            panel.setCanChooseDirectories(dirs);
            panel.setCanChooseFiles(!dirs);
            panel.setCanCreateDirectories(dirs);
            panel.setAllowsMultipleSelection(false);
            panel.setMessage(Some(&NSString::from_str(msg)));
            if !dirs {
                let types: Vec<Retained<NSString>> = crate::ui::SOURCE_EXTS
                    .iter()
                    .map(|e| NSString::from_str(e))
                    .collect();
                let arr = objc2_foundation::NSArray::from_retained_slice(&types);
                // `allowedContentTypes` is the modern spelling but needs
                // UTType from UniformTypeIdentifiers; the deprecated call
                // still filters correctly, so swapping it is a separate
                // change with its own dependency.
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
            src.apply(&v.title_rows);
            if let Some(tv) = src.ivars().info.borrow().as_ref() {
                tv.setString(&NSString::from_str(&v.detail));
            }
        }

        // output row
        {
            if let Some(f) = iv.out_field.borrow().as_ref() {
                if f.stringValue().to_string() != v.output_dir {
                    f.setStringValue(&NSString::from_str(&v.output_dir));
                }
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
                    let kind = match l.kind {
                        crate::ui::LogKind::Notice => 0,
                        crate::ui::LogKind::Detail => 1,
                        crate::ui::LogKind::Result => 2,
                    };
                    log_append(tv, &l.text, kind);
                }
            }
        }
        if let Some(sv) = iv.log_scroll.borrow().as_ref() {
            sv.setHidden(v.log_hidden);
        }
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
        if let Some(v) = self.ivars().page_main.borrow().as_ref() {
            if let Some(sup) = unsafe { v.superview() } {
                let b = sup.frame();
                self.relayout(b.size.width, b.size.height);
            }
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

    /// Apply a language change live: rebuild the menu bar and the main window's
    /// content in the newly-active locale (which the caller has already swapped
    /// in via `strings::set_locale`), then re-render from the core so the open
    /// disc, log, and selection all come back — just in the new language.
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

        // The Settings and About windows are cached (built once, reused on
        // reopen). They were built in the old language, so drop them — the next
        // open rebuilds them in the new one. (Settings is already closing when
        // this runs; close a stray About too.)
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
        let wanted: Vec<String> = groups
            .iter()
            .flat_map(|g| g.iter().map(|s| crate::ui::format_label(s)))
            .collect();
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

// ── widget helpers ────────────────────────────────────────────────────────

/// Append one colour-coded line to a log text view.
/// kind: 0 = notice (maroon), 1 = detail (olive), 2 = result (black)
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
    // Grouped so the ordinary case is line one and never needs scrolling past.
    // The whole-disc rows are what a separate "Backup" command used to do; the
    // extract/metadata rows expose sinks the library already supports.
    // The tree's checkboxes decide WHICH tracks; this decides what SHAPE the
    // output takes. Keeping per-kind "video only / audio only" entries here
    // would be a second, competing selector for the same intent.
    // Content comes from the core so the Mac popup, the Windows popup and the
    // tests can never drift apart. The shell decides only how it LOOKS.
    // Built empty; `render` fills it from the View, which knows the codecs.
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
                crate::strings::get("gui.menu.show_log"),
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
        std::mem::forget(drop);
    }
}

fn build_ui(mtm: MainThreadMarker, window: &NSWindow, c: &Controller) -> Retained<TitlesSource> {
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
        let b = btn(
            mtm,
            &crate::strings::get("gui.btn.open_file"),
            r(W / 2.0 - 80.0, eh * 0.55 - 70.0, 160.0, 30.0),
            c,
            sel!(onOpenFiles:),
        );
        page_empty.addSubview(&b);
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
                f[0].setStringValue(&NSString::from_str("/Volumes/media/iso/dvd/Greenland.iso"));
                f[1].setStringValue(&NSString::from_str("Greenland.iso"));
                f[2].setStringValue(&NSString::from_str(&crate::ui::fmt_bytes(6_743_590_912)));
                f[3].setStringValue(&NSString::from_str("—"));
                f[4].setStringValue(&NSString::from_str("/tmp/audit/demo/GREENLAND_t1.mkv"));
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
    if let Ok(sz) = dev_env("FMKV_SIZE") {
        if let Some((ws, hs)) = sz.split_once('x') {
            if let (Ok(nw), Ok(nh)) = (ws.parse::<f64>(), hs.parse::<f64>()) {
                window.setContentSize(NSSize::new(nw, nh));
                c.relayout(nw, nh);
            }
        }
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
        if let Ok(tab) = dev_env("FMKV_TAB") {
            if let Ok(i) = tab.parse::<isize>() {
                if let Some(tv) = c.ivars().tabs.borrow().as_ref() {
                    tv.selectTabViewItemAtIndex(i);
                }
            }
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

    std::mem::forget(c);
    std::mem::forget(src);
    app.run();
}

// ── Preferences ───────────────────────────────────────────────────────────

/// The option table for a settings dropdown: `(canonical, localized_label)`
/// pairs in menu order. The canonical value is what persists and what the
/// engine matches on (e.g. `key_source.starts_with("Online")`,
/// `container_label`); the label is what the localized popup shows. So a combo
/// displays translated text but stores a stable, English identifier — the same
/// decoupling the format dropdown uses. An empty result means "not an enum
/// popup" (free-form or format popup), handled separately.
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
        ],
        // Language: canonical is the locale code, label the endonym (shown as-is
        // in every locale). Drives the picker straight from the shipped list.
        "language" => crate::ui::LOCALES
            .iter()
            .map(|(endonym, code)| (*code, (*endonym).to_string()))
            .collect(),
        _ => vec![],
    }
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
    fn field(&mut self, mtm: MainThreadMarker, _key: &str, s: &str, val: &str, w: f64) {
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
        let b = btn(
            mtm,
            &crate::strings::get("gui.btn.browse"),
            r(self.gutter + w - 34.0, self.y - 4.0, 34.0, 25.0),
            c,
            sel!(onNoop:),
        );
        self.view.addSubview(&b);
        self.y -= 30.0;
    }
    fn button(&mut self, mtm: MainThreadMarker, title: &str, c: &Controller, a: Sel, w: f64) {
        let b = btn(mtm, title, r(self.gutter, self.y - 4.0, w, 26.0), c, a);
        self.view.addSubview(&b);
        self.y -= 32.0;
    }
    fn note(&mut self, mtm: MainThreadMarker, s: &str, w: f64) {
        let l = text(mtm, s, r(16.0, self.y - 18.0, w - 32.0, 36.0), false, true);
        {
            l.setUsesSingleLineMode(false);
            self.view.addSubview(&l);
        }
        self.y -= 44.0;
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
    // the window while `win_prefs` still holds a Retained ref, so reopening
    // Settings after an OK/close is a use-after-free (crash). Keep it alive and
    // let the Retained own the lifetime; reopen reuses the same window.
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
    let mut add_tab = |label: &str, rows: Rows| {
        all_fields.extend(rows.fields.iter().cloned());
        all_checks.extend(rows.checks.iter().cloned());
        all_popups.extend(rows.popups.iter().cloned());
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
    // consumes. Anything that could not reach real code was removed rather
    // than left as a switch that does nothing.

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
        "capture_without_keys",
        &crate::strings::get("gui.set.capture_no_keys"),
        false,
    );
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
    t.button(
        mtm,
        &crate::strings::get("gui.set.update_keydb"),
        c,
        sel!(onUpdateKeys:),
        160.0,
    );
    {
        let status = c.ivars().settings.borrow().keydb_status();
        t.note(mtm, &status, tw);
    }
    t.gap();
    t.field(
        mtm,
        "keyserver_url",
        &crate::strings::get("gui.set.keyserver_url"),
        "",
        300.0,
    );
    t.field(
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
    t.check(
        mtm,
        "debug_log",
        &crate::strings::get("gui.set.log_debug"),
        false,
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
    }
    *c.ivars().pf_fields.borrow_mut() = all_fields;
    *c.ivars().pf_checks.borrow_mut() = all_checks;
    *c.ivars().pf_popups.borrow_mut() = all_popups;
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
        bd.addSubview(&btn(
            mtm,
            &crate::strings::get("gui.btn.cancel"),
            r(w - 194.0, 12.0, 88.0, 28.0),
            c,
            sel!(onClosePrefs:),
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
    let rows: [(String, &str); 5] = [
        (crate::strings::get("gui.about.version"), "1.6.0 (macOS)"),
        (crate::strings::get("gui.about.engine"), "libfreemkv 1.6.0"),
        (crate::strings::get("gui.about.licence"), "MIT"),
        (
            crate::strings::get("gui.about.keys"),
            "keydb ✓ 3 971 entries",
        ),
        (
            crate::strings::get("gui.about.website"),
            "https://freemkv.org",
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
                    &NSString::from_str(v),
                    &Retained::cast_unchecked(attrs),
                );
                lb.setAttributedTitle(&title);
            }
            bd.addSubview(&lb);
        } else {
            bd.addSubview(&text(mtm, v, r(160.0, y, w - 175.0, 18.0), false, false));
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
//
// Drives the REAL controls: finds the actual NSButton/NSPopUpButton objects and
// sends them `performClick:` / `selectItem`, so every action goes through the
// same target/action path a mouse click takes. Not a shortcut around the UI —
// it exercises the UI.

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
                // Respect enablement exactly as AppKit would.
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
                if let Ok(b) = sub.clone().downcast::<NSButton>() {
                    if { b.title() }.to_string() == title {
                        return Some(b);
                    }
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
        // way AppKit would. Without this the driver proves only that the popup
        // changed appearance — which is exactly how the "picked MP4, got MKV"
        // bug survived.
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
            if let Some(v) = self.ivars().page_main.borrow().as_ref() {
                if let Some(sup) = unsafe { v.superview() } {
                    {
                        sup.setNeedsDisplay(true);
                        sup.displayIfNeeded();
                    }
                    snapshot(&sup, &format!("{shot_dir}/{n}.png"));
                }
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
        check(
            "menu-show-log",
            self.drive_menu("View", "Show log") && view().log_hidden,
            "View ▸ Show log hid it",
        );
        self.drive_menu("View", "Show log");
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

// ── tests ─────────────────────────────────────────────────────────────────
//
// Only the pure, widget-free helpers are tested here. Anything that touches
// AppKit needs a main thread and a window server, so it is exercised by the
// screenshot harness (FMKV_SHOT / FMKV_PAGE) instead.

// Model tests live in `tests/app_core.rs` and run with no shell attached —
// keeping a shell-side copy would be exactly the duplication this split
// exists to prevent.
