//! # The Windows shell — BUILD NOTES (not yet implemented)
//!
//! This file is deliberately inert: it compiles to nothing and is not wired
//! into `main.rs`. It exists so the Windows port can start immediately without
//! re-deriving decisions that were already settled while building the Mac
//! shell. Everything below is a recorded fact from that work, not a guess.
//!
//! When the port starts, replace this doc block with the implementation and add
//! `#[cfg(target_os = "windows")] mod windows;` to `main.rs` beside the `mac`
//! line.
//!
//! ---
//!
//! ## 1. What is already done for you
//!
//! **The entire application already exists and is platform-neutral.** The
//! Windows shell writes NO logic. Line counts as of writing:
//!
//! | File | Lines | Role |
//! |---|---|---|
//! | `ui.rs` | 864 | All decisions. Reused verbatim. |
//! | `engine.rs` | 853 | Engine bridge, scan/rip/preflight. Reused verbatim. |
//! | `settings.rs` | 303 | JSON settings, keydb update, update check. Reused verbatim. |
//! | `platform.rs` | 103 | The ONLY per-OS logic. Windows arm already written. |
//! | `mac.rs` | 3429 | Pure shell. This is what gets re-implemented. |
//!
//! The 77 tests in `tests/` are platform-neutral and must pass unchanged on
//! Windows. If a test needs editing to pass, the port has broken something —
//! that is the signal, treat it as such.
//!
//! `platform::free_space_bytes` ALREADY has a `#[cfg(windows)]` arm using
//! `GetDiskFreeSpaceExW`. It has never been compiled. Expect it to need the
//! `use std::os::windows::ffi::OsStrExt` import moved above its use site — it
//! currently sits at the bottom of the module, which is legal but was never
//! type-checked.
//!
//! ## 2. The contract — three steps, no fourth
//!
//! ```text
//! 1. render   App::view() -> View     assign strings/flags to widgets
//! 2. dispatch App::dispatch(cmd)      on any click, menu pick or keystroke
//! 3. perform  the returned Effects    the platform-only actions
//! ```
//!
//! **The invariant:** if a change to `ui.rs` would need mirroring here, the
//! split is wrong. Fix the split, do not mirror.
//!
//! Corollary learned the hard way on macOS: the shell must hold NO state that
//! duplicates `App`. Six such fields existed in `mac.rs` and one of them
//! (`run`) went stale, silently disabling the menu-disabling logic during a
//! rip. They were deleted. Do not reintroduce the pattern here: if you find
//! yourself caching something from `View`, stop.
//!
//! ## 3. Toolkit
//!
//! **`winsafe` 0.0.28** — idiomatic safe Win32, zero dependencies, actively
//! maintained. Chosen over `native-windows-gui` (unmaintained) and `windows-rs`
//! (raw bindings; far more unsafe boilerplate for a plain desktop app).
//! Rationale recorded in the project's internal UI-study notes.
//!
//! `rfd` 0.17 is ALREADY a dependency and is cross-platform — use it for
//! `Effect::PickSource` / `Effect::PickOutputDir` rather than hand-rolling
//! `IFileOpenDialog`.
//!
//! Add to `Cargo.toml`:
//! ```toml
//! [target.'cfg(target_os = "windows")'.dependencies]
//! winsafe = { version = "0.0.28", features = ["gui"] }
//! ```
//!
//! ## 4. Complete `Cmd` list — every one must be reachable
//!
//! `Open, Close, SetOutput, Run, Cancel, Eject, SelectAll, SelectNone, Invert,
//! ClearLog, ToggleLog, Settings, About, Docs, CheckUpdates, Quit`
//!
//! `blocked_while_running(cmd)` returns true for everything EXCEPT
//! `Cancel | About | Docs | Quit`. Consult it when enabling/disabling menu
//! items — do not hardcode a second list.
//!
//! `Cmd::Eject` must be hidden unless the source is a real drive
//! (`View::eject_visible`). Ejecting an ISO is meaningless.
//!
//! ## 5. Complete `Effect` list — what the shell must perform
//!
//! | Effect | Windows action |
//! |---|---|
//! | `PickSource` | `rfd` open dialog filtered to `ui::SOURCE_EXTS`; on choose call `App::open(path)` |
//! | `PickOutputDir` | `rfd` folder dialog |
//! | `Reveal(path)` | `explorer.exe /select,"<path>"` |
//! | `OpenUrl(url)` | `ShellExecuteW(.., "open", url, ..)` |
//! | `ShowSettings` | modeless settings window |
//! | `ShowAbout` | modeless about window |
//! | `Redraw` | call `render()` |
//! | `StartTicking` | `SetTimer` ~100 ms → `App::tick()` |
//! | `StopTicking` | `KillTimer` |
//! | `Quit` | `PostQuitMessage(0)` |
//!
//! ## 6. `View` — the complete render surface
//!
//! ```text
//! page              Page::{Empty, Titles, Progress, Result}
//! title_rows        Vec<Row>  { index, depth, type_s, desc, check: Option<Check> }
//!                             check == None means NO checkbox on that row
//! info              Option<[String; 7]>   pair with InfoRows::LABELS
//! bar_current       f64 0..100
//! bar_overall       f64 0..100
//! caption_current   String
//! caption_overall   String
//! show_overall_bar  bool   false for a single title (two identical bars is a bug)
//! output_dir        String
//! format            String        currently selected
//! formats           Vec<Vec<&str>>  groups; render separators BETWEEN groups
//! can_run           bool          drives the Rip button's enabled state
//! log               Vec<LogLine>  { kind, text }; kind picks the colour
//! log_hidden        bool
//! detail            String        the info pane for the selected row
//! result_summary    String
//! result_heading    String        NEVER hardcode "Finished" — cancels say otherwise
//! eject_visible     bool
//! ```
//!
//! **`formats` must be read from the `View` on every render**, not built once.
//! Building it once was a real bug on macOS: opening an MKV after an ISO left
//! "Whole disc → ISO image" on offer for a source that cannot produce it.
//! `ui::output_formats(disc_source)` is the single source; never hardcode the
//! list in the shell (that was a second bug — two copies that drifted).
//!
//! Likewise `InfoRows::LABELS` owns the seven Information row labels. `mac.rs`
//! had its own copy; they were deduplicated. Do not make a third.
//!
//! ## 7. Widget mapping
//!
//! | Element | Win32 |
//! |---|---|
//! | Title/stream tree | `SysTreeView32`, `TVS_CHECKBOXES` |
//! | Log pane | `EDIT`, `ES_MULTILINE | ES_READONLY`, must stay selectable |
//! | Progress bars | `msctls_progress32` |
//! | Format picker | `COMBOBOX`, `CBS_DROPDOWNLIST` |
//! | Buttons | `BUTTON`, `BS_PUSHBUTTON` |
//! | Info group | `BUTTON` with `BS_GROUPBOX` |
//!
//! **Menus mount in-window** (`SetMenu`) — there is no global menu bar. Put
//! Settings under File, NOT under an app menu; Windows has no app menu.
//! Standard Windows accelerators: Ctrl (not Cmd), Alt+F4 to quit, and F1 for
//! help/docs.
//!
//! The tree's checkbox state must be **tri-state** — a title whose streams are
//! partly ticked shows `Check::Mixed`. `TVS_CHECKBOXES` gives only two states,
//! so use a state image list with a third image and `TVIS_STATEIMAGEMASK`.
//! Getting this wrong is invisible until a user unticks one audio track.
//!
//! ## 8. Bugs the Mac port hit — do not repeat them
//!
//! 1. **A control that lies is worse than no control.** Cut/hide anything not
//!    wired: Eject on a file source, duplicate Open items, settings fields with
//!    no backing. Each of these was caught by the user, not by tests.
//! 2. **Model-level tests pass while the widget is broken.** Two real bugs
//!    (checkboxes that did nothing; menus that stayed live during a rip) were
//!    invisible to assertions on `View` and only appeared when the test drove
//!    the actual controls. Port `self_test` and assert on WIDGETS.
//! 3. **Do not compute anything in the shell.** Speed, ETA, percentages and
//!    byte formatting all come from the engine or `ui.rs` (`fmt_bytes`,
//!    `fmt_hms`, `rate_text`, `bar_caption`, `overall_pct`). Recomputing is the
//!    exact drift the engine split exists to prevent.
//! 4. **An unknown value renders as an em dash, never blank and never `0`.** A
//!    blank Information field reads as a broken panel; `0.0 MB/s` during a
//!    stall reads as a broken rip.
//! 5. **Respect the system theme.** Do not force light mode. On Windows that
//!    means honouring the dark-mode setting rather than hardcoding colours.
//!
//! ## 9. Test harness to port
//!
//! `mac.rs` ends with `self_test(iso, mkv, shot_dir)` — 57 checks that drive
//! real widgets (`drive_click_checkbox`, `drive_menu`, `drive_click_button`,
//! `drive_open`, `drive_log`) and write screenshots. Port it. It found bugs
//! nothing else did, and it is the only way to claim the Windows shell works
//! without a human clicking every control.
//!
//! Windows equivalents: send `BM_CLICK` / `WM_COMMAND` rather than calling
//! handlers directly — calling the handler bypasses exactly the wiring that
//! broke on macOS. Screenshot via `PrintWindow` into a DIB.
//!
//! ## 10. CI
//!
//! `.github/workflows/ci.yml` currently runs `lint`/`test` on macOS plus a
//! Linux `cargo check --lib` that guards the core's portability. Add a
//! `windows-latest` leg for lint+test when this lands.
//!
//! `.github/workflows/release.yml` builds a matrix of two macOS targets. Add
//! `x86_64-pc-windows-msvc` producing `freemkv-gui-x86_64-windows.zip`, and
//! the download page's `guiPlatforms` Windows entry (currently a "Coming soon"
//! card) becomes a real button — the asset name must match exactly.
