# Windows shell design notes

Full rationale for `src/windows.rs`, the Win32 shell over the shared
`ui`/`engine`/`settings` core. The module doc comment keeps only the
API-contract summary; the design history lives here.

## The contract — three steps, no fourth

```text
1. render   App::view() -> View     assign strings/flags to widgets
2. dispatch App::dispatch(cmd)      on any click, menu pick or keystroke
3. perform  the returned Effects    the platform-only actions
```

**The invariant:** if a change to `ui.rs` would need mirroring here, the
split is wrong. Fix the split, do not mirror. This shell writes NO logic:
every decision — which formats exist, which rows carry a checkbox, what the
progress caption says, whether Run is enabled — comes from `ui.rs`, so a bug
fixed on macOS is fixed here at the same time.

The shell also holds no state that duplicates `App`. On macOS six such
fields existed and one of them went stale, silently disabling the
menu-disabling logic during a rip. The only cached values here are *render
memos* (`rows`, `formats`, `log_len`): a signature of what was last painted,
used purely to avoid rebuilding a control that has not changed. Rebuilding a
tree or a dropdown unconditionally on a 100 ms tick would flicker and drop
the user's expansion state mid-rip.

## Toolkit

`winsafe` (safe Win32) with stock common controls, so the scrollbars, focus
rings, keyboard navigation, high-contrast mode and screen-reader support are
all the real Windows ones rather than a drawn imitation. The
Common-Controls v6 manifest in `res/freemkv.manifest` (embedded by
`build.rs`) is what makes them render themed rather than as Windows-95 grey
boxes.

## Windows conventions this deliberately does NOT copy from macOS

* Menus mount **in-window** (`SetMenu`) — there is no global menu bar.
* **Settings lives under File**, not an app menu: Windows has no app menu.
* Accelerators are **Ctrl**, not Cmd; `Alt+F4` quits; `F1` opens the docs.
* "Show in Finder" becomes "Show in Explorer".

## Per-DPI UI font

`winsafe` builds its own global UI font exactly once, from the plain
`SystemParametersInfo` — that is, at the DPI of the primary monitor — and
sends it to each control as the control is created. Under PerMonitorV2 that
font is right only on the monitor the process started on, and never changes
again. A layout that scales under a font that does not is arguably worse
than neither, so the shell creates its own per-DPI font instead
(`ui_font`).

## Window icon (`set_icons`)

The executable's icon resource and the *window's* icon are two separate
mechanisms: the resource is what Explorer shows for the file, while the
title bar, Alt-Tab and the taskbar read the window's own `WM_SETICON` pair.
Setting only the class icon leaves the title bar with a blurry downscale,
because winsafe derives `hIconSm` from `LoadIcon`, which ignores the small
size and always hands back the 32 px frame.

So load each slot explicitly with `LoadImage` at the size Windows actually
wants, which makes it select the matching frame out of the icon group — the
purpose-drawn 16 px artwork for the title bar, the full-detail 32 px one for
Alt-Tab. Sizes come from `GetSystemMetricsForDpi` at the window's own DPI
rather than `GetSystemMetrics`: the manifest declares PerMonitorV2, so on a
200% display the small-icon metric is 32, not 16, and the un-scaled call
would pick a frame half the size the title bar is about to draw.

Best-effort throughout: a missing or unreadable icon costs the default
Windows glyph, which is never worth failing a launch over.

## Tri-state checkbox glyphs (`build_check_images`)

`TVS_CHECKBOXES` gives only two states, so a title whose streams are partly
ticked could not be drawn — and getting that wrong is invisible until a user
unticks one audio track. The fix is a state image list with a third image,
selected per row through `TVIS_STATEIMAGEMASK`.

The glyphs are drawn by the **theme engine** (`DrawThemeBackground` with the
real `BUTTON`/checkbox parts), so they are pixel-identical to every other
checkbox on the system rather than hand-painted approximations.

The list is built at the size the OS reports for a small icon **at this
window's DPI** (`GetSystemMetricsForDpi`, not `GetSystemMetrics` — the
latter always answers for the primary monitor), and rebuilt when the DPI
changes. winsafe's `TreeView::image_list` helper cannot be used: it creates
its list hard-coded at 16 × 16, which is a half-size tick at 200%.

## Confirm-quit-mid-rip dedup (`confirm_quit_mid_rip`)

Shared by the window's X (`WM_CLOSE`) and File > Exit, which used to
disagree: the X asked, cancelled and tore down, while the menu item went
straight to `PostQuitMessage` and skipped all three. One question, one
place — a second copy of this decision beside a call site is the bug,
which this crate has now proved three separate times.

## Quit drains the worker (`cancel_and_drain`)

`Cmd::Cancel` only signals: the worker notices at its next boundary and
unwinds the mux, and it is that unwind which closes and finalises the
partial file. Quitting without waiting lets the process exit mid-write, so
what lands on disk is wherever the OS write cursor happened to be rather
than the deliberate "cancelled — partial output kept" artefact the GUI
claims. `mac.rs` was given exactly this wait; this shell was not. Bounded
by `QUIT_GRACE`, so a wedged drive cannot turn quit into a hang.

## Timer-start failure must not fail silently (`report_timer_failure`)

`SetTimer` can fail (Windows caps live timers per process/session). If the
poller never starts, nothing ever observes `RunState.finished`: the worker
thread runs the rip to completion in the background while this window sits
on the Progress page forever, with Cancel and every menu command still
disabled by `blocked_while_running` and no way for the operator to learn
anything is wrong short of a force-quit. A `let _ =` here is the same
"never die silently" gap `run_main`'s own error path exists to close — so
it gets the same fix: a `MessageBox`, since the timer that would carry a
localized in-window notice is exactly the one that failed to start.

## Language picker: button + popup menu, not a dropdown (`LangPicker`)

Win32 has no checked-list combo box. `CBS_DROPDOWNLIST` is single-select,
and a listbox with `LBS_MULTIPLESEL` is not a dropdown at all — it would
need permanent vertical space for a 38-row list, three times over, on a
Settings page that already has none. A button opening a `TrackPopupMenu`
of `MF::CHECKED` entries is the native Windows idiom for "tick several
from a long list" — it is what the Explorer and Task Manager column
choosers do — it costs one row of height like the text box it replaces,
and it draws the ticks with the system's own check bitmaps rather than an
imitation of them.

**Why the string is held here.** The button's title is a SUMMARY
("English, German"); recovering codes from it would mean a second parser
in this file, and the whole point of `ui::lang_*` is that both shells
share one rule. So the stored preference lives alongside the control and
the title is only ever written *from* it: the shell renders, `ui` decides.

## About window rebuilds on relocalize (`About::relocalize`)

The macOS shell drops its cached About window on relocalize so the next
open rebuilds it in the new language; this shell builds its About ONCE
(winsafe cannot create controls after the window exists) and reused the
old one forever — so About stayed in the language the app started in, no
matter what the operator picked. Same policy, two shells; only one of
them applied it. `relocalize` re-texts the built window's controls in
place instead.

## One settings-save call site (`save_settings_reporting_error`)

This crate's dominant defect this round was exactly this policy
implemented twice: the OK button reported a failed save
(`gui.log.settings_save_error`), and the language-dropdown path saved the
SAME struct a few lines away with a bare `let _ =` that dropped the error
on the floor. A disk-full or permissions failure there looked identical
to success — the operator picks a language, sees the UI relocalize, and
has no way to know the keydb path/keyserver token/dest dir edited in the
same session never reached `gui-settings.json`. One policy, one call
site now.

## Widget-level assertions (`Shell::widget_checks`)

These are the checks a model test can never make — the missing-checkbox
bug on macOS lived exactly here, with a correct `View` and a wrong cell.
They depend only on whatever source is currently loaded (including
none), so both the interactive `self_test` and the `#[test]` in this
file's test module run the same code against the same real window.
Returns `(passed, description)` per check rather than panicking, so the
interactive mode can print a full report instead of dying on the first
failure.
