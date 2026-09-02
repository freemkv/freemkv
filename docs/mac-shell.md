# macOS shell internals

Design notes relocated from `src/mac.rs` to keep in-file comments within the
comment-guard's line caps. Grouped by the function each note documents.

## `rows_sig` rationale

Mirrors `windows.rs`'s `rows_sig`, which the Windows shell already uses to
skip `rebuild_tree` on an unchanged 200 ms tick; this shell had no such
guard at all, so a running rip (which ticks the Progress page at 5 Hz)
forced a full outline reload and re-expand of every root every 200 ms for
the life of the rip, even though the titles tree is not the page on
screen and its rows never move while a rip is in flight.

Tick state is deliberately EXCLUDED, exactly as on Windows. A rebuild is
`reloadData` + re-expand + `scrollPoint(first_visible_row)`, so folding
`r.check` in here made every checkbox click throw the outline back to the
top of the list — deselecting extras on a 100-title disc meant re-finding
your place after each click. A tick change is applied in place instead, by
`TitlesSource::sync_check_states`.

## `sync_check_states` rationale

This is what runs on an ordinary redraw, including the one that follows
every checkbox click: the row identities did not change, only their
ticks, so there is nothing to reload. The Windows shell has had this
split since it grew `rows_sig` (`sync_tree_states`); this shell instead
folded the tick state into its signature and rebuilt, which meant
`scrollPoint(first_visible_row)` on every click.

## `save_settings_reporting_error` rationale

This crate's dominant defect this round was exactly this policy
implemented twice: OK (`onClosePrefs:`) reported a failed save
(`gui.log.settings_save_error`), and the language-dropdown path
(`onApplyLanguage:`) saved the SAME struct a few lines away with a bare
`let _ =` that dropped the error on the floor. A disk-full or
permissions failure there looked identical to success — the operator
picks a language, watches the UI relocalize, and has no way to know
the keydb path/keyserver token/dest dir edited in the same session
never reached gui-settings.json. One policy, one call site now.

## `set_keydb_updating` rationale

It used to be the other way round — the button's enabled state was the
only record that a download was running, so closing Settings and
reopening it (`build_prefs` builds a fresh, enabled button) handed the
operator a live button back, and a second click spawned a second thread
writing the same keydb file as the first.

## `Rows::langs` rationale

Carries the whole stored preference string, not a single selected row, so
`read_prefs_form` must treat it differently (see the `pf_langs` comment on
`Ivars`). It replaced a free-text box that asked the user to know ISO
codes; the list is the only place the codes are spelled, so a typo can no
longer silently disable a language preference.

## `Rows::field` rationale

The key used to be `_key` here while `path` pushed it — one form-row
policy, two implementations, one of them hardened — which silently
dropped every free-form setting on this shell, including
`abort_lost_secs` (the abort-for-loss threshold) and `max_passes` (which
decides whether recovery runs at all).

## `field_secure` rationale

Without this, `keyserver_token` sat in a plain `NSTextField` — fully
legible during screen-sharing, presenting, or a screen recording.
`NSSecureTextField` subclasses `NSTextField`, so it slots into the same
`self.fields` list `read_prefs_form` and the populate loop already drive.

## Drop overlay leak test

`install_drop_view` ended with `std::mem::forget`, which was defensible
only while it ran once at launch. It is also called by `relocalize`,
which re-adds the overlay after tearing the window content down — so
every language switch built a fresh `DropView`, handed AppKit its own
retain via `addSubview`, and then withheld ours forever. `relocalize`'s
teardown releases AppKit's retain and not that one, so each switch left
a whole detached view alive for the life of the process.

Nothing needs the forgotten handle: the superview's retain keeps the
view alive exactly as long as it is installed, which is the lifetime
that was wanted. Parking it in an ivar instead would build a reference
cycle — `DropView` holds a `Retained<Controller>`.

Scoped to this function: the two `mem::forget`s before `app.run()` are
deliberate (nothing returns from `run`), so a file-wide ban would be
wrong. Needle assembled at run time so it cannot match itself.

## Widget-list clear test

`build_ui` runs twice: once at launch, and again on every language
switch, where `relocalize` tears the window content down and rebuilds
it. Every other widget ivar is ASSIGNED (`*…borrow_mut() = Some(x)`), so
a rebuild replaces it. `bar2_row` is the one that is `push`ed, and
nothing cleared it — so each language switch appended three more handles
to views already removed from the window, keeping them alive for the
life of the process and growing the list `render()` walks at 5 Hz.

`relocalize` already resets `tree_sig` for exactly this reason. The
clear belongs in `build_ui`, next to the pushes, so a future third
caller is correct without knowing to do it.

Needles assembled at run time so this cannot match its own text.

## Orphaned-selector test

The complement of `the_menu_reaches_every_command_the_core_defines`,
which checks that every core command has a selector. This checks the
other direction: a selector nothing targets is a handler that cannot
run. `onOpenFolder:` sat here fully implemented and wired to no menu
item, button or timer — and, being absent from `cmd_for`, it would also
have had NO rip-in-progress guard the moment anyone did wire it.

Restricted to the `on…:` actions this file owns. AppKit's own callbacks
(`applicationDidFinishLaunching:`, `outlineView:…`, `draggingEntered:`)
are invoked by the framework and named by no `sel!` of ours.

The needle is assembled at run time: written out whole it would match
this test's own text through `include_str!`, and a source-inspection
test that can only ever find itself is the tautology this crate has
already shipped once.

## Format popup rebuild-guard test

`sync_formats` asks whether the popup already shows the wanted formats
and returns early if so, because rebuilding drops an open menu under the
user's cursor mid-click. It compared `NSPopUpButton::itemTitles()` —
which INCLUDES the separator items, each reporting an empty title —
against a flat list of the group labels with no separators in it. With
two groups the lengths can never match, and `output_formats` returns two
groups at minimum (`[titles, meta]`) and three for a disc. So the guard
never once fired: every `render()`, five times a second for a whole rip,
rebuilt the menu it exists to protect.

The empty-string expectation is not read off our own builder — it is
what AppKit reports. A separator item's `title` is `""`, so a menu of
[item, separator, item] answers `itemTitles` with three entries, the
middle one empty.

## Language-picker parsing test

The Win32 shell has had this pin since the pickers were unified
(`windows.rs::the_language_pickers_own_no_parsing_source_inspection_only`);
this shell, which implements the same three pickers against the same
`ui::lang_*` rules, had nothing at all — so a hand-rolled second parser
here would compile, pass every test, and drift from the other shell
exactly as the two halves of this feature did before `ui::lang_*`
existed.

Source inspection: proving the menu really ticks needs a live
`NSPopUpButton` and a run loop. What can be checked is the thing most
likely to go wrong — the set logic being reimplemented in this file.

## Stop-and-quit worker-wait test

`Cmd::Cancel` only SIGNALS the worker — the flag is read at the next
frame boundary — and `applicationShouldTerminate:` answered
`TerminateNow` the instant the alert was dismissed. AppKit then calls
`exit()`, so a worker that was mid-write (flushing a cluster, writing
the MKV trailer) was killed by the process teardown instead of reaching
the graceful "cancelled — partial output kept" path that CLOSES the
sink. The confirmation dialog exists to protect a rip in progress, and
it raced its own protection.

Source inspection: driving `confirm_quit` needs a live `NSAlert` and a
real run loop. The waiting itself is unit-tested in
`engine::await_worker_exit`.
