# win_layout.rs — design rationale

## Why this file exists at all

`res/freemkv.manifest` declares **PerMonitorV2** awareness. That is a
promise: Windows stops bitmap-scaling the window and hands the process the
real DPI instead, expecting the app to lay itself out in physical pixels. A
layout written in bare constants (`PAD: i32 = 8`) therefore renders at 8
physical pixels on a 96-DPI desktop *and on a 192-DPI one* — half size, with
clipped text. 125% and 150% are the stock Windows settings on laptops, so
that is most users.

Every constant below is the **96-DPI baseline**; nothing in `windows.rs`
positions a control from a raw constant any more. `Scale::px` converts a
baseline value to physical pixels for a given DPI, and `main_layout` turns a
DPI plus a client size into the complete set of rectangles.

## Why a pure function rather than scaling at each call site

Scaling at the call site would work, but it cannot be tested: every value
would be locked behind a live `HWND` on a real display. Returning a plain
struct of rectangles from `(dpi, client size, page state)` makes the part
most likely to be wrong — the arithmetic — checkable on any host, which is
what the tests at the bottom of this file do. `windows.rs` keeps only the
mechanical "call `SetWindowPos` with these numbers" half.

The module is deliberately NOT `cfg(windows)`: it holds no Win32 types, and
gating it would make its tests unrunnable anywhere but Windows.

## `menu_column_rows` — why columns are balanced and budget is 4/5 screen height

The language checklists are a popup menu of ~38 entries. A Win32 popup that
is taller than the screen does not stay put and does not shrink: Windows
grows scroll arrows at top and bottom, and a checklist you have to scroll to
find "Swedish" in is a worse control than the text box it replaced. So the
menu is broken into columns instead, with `MF::MENUBARBREAK`.

The budget is deliberately four fifths of the screen height, not all of it:
the menu is anchored under a button partway down a dialog, and the taskbar
takes a slice off the bottom. Leaving headroom means the menu drops downward
from the button in the normal case rather than being flipped up over it.

Columns are BALANCED — three columns of 13 rather than two of 18 and one of
2 — because a stub final column reads as a rendering fault.

## `the_tallest_settings_pages_still_fit_the_settings_window` test

The Settings pages stack downwards from `top` with no scrolling, so a
page that grows past the tab's page area silently puts controls off the
bottom of the window — where they are unreachable, at every DPI at once
(each metric scales, so the overflow scales with it).

The Selection page is the one that just grew: default selection,
minimum length + its note, then the three preferred-language boxes and
their note. Recovery is the other tall one (2 fields + 1 combo, 3 notes,
2 gaps, 2 checkboxes).
