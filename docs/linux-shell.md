# Linux shell design notes

Rationale for `src/linux.rs` (+ `src/linux/*.rs`), the GTK4 + libadwaita
shell over the shared `ui`/`engine`/`settings` core, and for
`src/linux_glue.rs`, its toolkit-free half.

## The contract — the same three steps as macOS and Windows

```text
1. render   App::view() -> View     assign strings/flags to widgets
2. dispatch App::dispatch(cmd)      on any click, menu pick or keystroke
3. perform  the returned Effects    the platform-only actions
```

The shell holds no state that duplicates `App`. Its only caches are render
memos (`Memo`: the tree's row signature, the format list, what the log pane
last showed) and the painted log-menu label.

`painting` is set while `render` writes to widgets. GTK emits change signals
for programmatic writes (`CheckButton::toggled`, `DropDown::notify::selected`,
`Entry::changed`, `SingleSelection::notify::selected-item`), and every handler
returns early while it is set, so a repaint is never mistaken for the user.

## Where it lives

| File | Role |
| --- | --- |
| `src/linux.rs` | `Shell`: actions, accelerators, effects, choosers, reveal, notifications, quit confirmation, drag-and-drop, keydb drain |
| `src/linux/main_view.rs` | The four pages (empty / titles / progress / result) and the log |
| `src/linux/tree.rs` | Title tree: `GtkColumnView` over `GtkTreeListModel`, tri-state tick boxes |
| `src/linux/prefs.rs` | Settings as an `AdwPreferencesWindow` |
| `src/linux_glue.rs` | Toolkit-free decisions (action table, accelerator spelling, log append detection, …) with unit tests that run on every CI leg |

`linux_glue` is deliberately NOT gated, the same reasoning as
`win_layout.rs`: its tests must run on the macOS and Windows jobs too.

## GNOME conventions this deliberately does NOT copy

* **No menubar.** One `AdwHeaderBar` hamburger built from `ui::menu_layout`:
  each group is a titled section (split at the layout's separators), and the
  App group (About / Settings / Quit) goes last. Cut/Copy/Paste/Select-All-text
  get no row — GTK text widgets bring their own context menu and keys.
* **Accelerators** are registered with `set_accels_for_action` from the same
  layout; the `accel` attribute on a menu item only *displays* a shortcut.
  `<Primary>` is Ctrl. Punctuation is spelled as keysym names (`comma`).
* **Settings has no OK/Cancel.** An `AdwPreferencesWindow` commits when it is
  closed — into the stored settings, into the running `App` and to disk, and
  only if something changed. The rule the other shells apply on OK is kept:
  the live output folder follows the default destination only when the
  default actually changed. The interface language still applies the moment
  it is picked (commit → `app_entry::apply_locale` → rebuild the main window
  and menu → reopen Settings on the same page).
* **"Show in Folder"** (`GtkFileLauncher::open_containing_folder`) rather than
  "Show in Finder" / "Show in Explorer": the FileManager1 / OpenURI portal
  opens the parent folder with the output selected.
* Choosers are `GtkFileDialog` and links use `GtkUriLauncher`, so inside a
  Flatpak everything goes through the XDG portals with no filesystem grant.

## Behaviour notes

* **Launch probe.** As on the other shells, a one-shot 200 ms timer after the
  window is shown runs `App::disc_source(false)` + `open_probe`. Debug builds
  skip it under `FMKV_OPEN=<source>` (which opens that source instead) or
  `FMKV_NO_PROBE=1`, so a development session never touches a real drive.
* **Open disc** logs "Opening …" and defers the blocking scan one beat so the
  line paints first — the reason the other shells use two `app_mut` calls.
* **Quit mid-rip.** Window close, Ctrl+Q and the menu all reach the window's
  `close-request`, which asks once (`AdwMessageDialog`, "Stop & Quit" is
  destructive, "Keep Ripping" is the default). Stop & Quit cancels, waits up
  to `engine::QUIT_GRACE` for the worker to put the file down, then closes.
* **Drag and drop** accepts a folder or a `SOURCE_EXTS` file (any case) and is
  refused mid-rip by the core's rule for `Cmd::Open`.
* **Rip finished.** An in-window `AdwToast` and a `GNotification` (XDG portal
  under Flatpak), both with a "Show in Folder" button bound to
  `app.reveal-output` carrying that rip's folder, so a notification clicked
  after a later rip still reveals the right place.
* **Log pane.** Notice lines are red and detail lines green (libadwaita's
  light/dark shades, following the system style). A redraw appends when what
  is on screen is still a prefix of the log and rewrites after a clear, a new
  source or the core's front-trim (`linux_glue::log_delta`).
* **Title tree.** The tick box sits in the expander column; a click reports
  only the row index and `Tree::toggle` decides the direction and cascade.
  A tick-only redraw repaints the bound boxes in place, so expansion,
  selection and scroll survive. GTK 4.10 has no `ColumnView::scroll_to`, so
  "first ticked row in view" sets the adjustment (rows are uniform height).

## Toolchain floor

`gtk4` is pinned with the `v4_10` feature and `libadwaita` with `v1_4`
(Ubuntu 24.04, Fedora 39+, GNOME runtime 45+). Nothing here needs newer:
`AdwAlertDialog`, `AdwButtonRow`, `ColumnView::scroll_to` and dropdown
sections are deliberately not used. Ubuntu 22.04 (GTK 4.6, libadwaita 1.1)
is below this floor, which matters for the AppImage workflow's runner choice.
