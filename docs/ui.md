# ui: rationale overflow

Design notes moved out of `src/ui.rs` doc comments to stay under the
comment-guard caps. Each section is pointed to from a short `//` comment
at the corresponding item.

## `class_or_fallback`

`all` is every PID of the class present on the title. `Only([])` from the
engine means the requested languages are not on this disc for this class —
the user's chosen rule is to fall back to today's behaviour for that
category rather than ship a file missing the whole track class.

## `preferred_pids`

The language matching is `freemkv-engine`'s: the rows are folded back into a
synthetic `DiscTitle` and handed to `resolve_stream_selection_forced`, so the
GUI has no second language matcher to drift from the one the rip uses — name
and 639-1/2T/2B/3 forms all resolve exactly as `-a`/`-s` do on the CLI.

The three classes are resolved in three calls, one per set, so each falls
back independently: German audio missing must not drag the subtitle choice
down with it, and an unparseable tag in one box cannot disturb the other two.

## `title_visible`

ONE predicate, used both to decide which rows the tree shows and which
titles the "Default selection" setting may tick. They were two expressions
before (`d > 0.0 && d < min_eff` for the rows, `d >= min_eff` for the
selection) and disagreed in both directions: a title of unknown length
(`0.0` — "not timed", not "zero seconds") was shown but could not be
ticked, and a filtered-out short title could be ticked but not shown.

`min_eff` is the EFFECTIVE minimum: `from_scan` drops it to 0 when no title
clears it, because hiding every title is never right.

## `Tree::from_scan`

`prefs` are the user's preferred-language defaults: `sel_mode` decides
which TITLES start checked, and `prefs` then narrows which of a checked
title's STREAM rows start checked. Nothing else changes — the rip request
is still built from the checkboxes, so every choice made here is visible
in the tree and can be overridden by hand.

An empty `prefs` ([`LangPrefs::default`]) is exactly the pre-preference
behaviour (every stream of a checked title checked), and so is a category
whose languages match nothing on this title — see [`preferred_pids`]. A
disc must never rip silently without audio because the preferred language
is not on it.

## `Tree::check_state`

The two halves answer two different questions, and the box must not let
one answer the other's. Folding ONLY the children made the glyph and the
rip independent answers, and since [`Tree::set_checked`] on a child never
writes its parent, they disagreed in both directions:

* clear every stream row under a ticked title, one row at a time, and
  the box drew empty while the title was still ripped — a video-only
  extract, which is a supported outcome (see `is_video_only_selection`),
  drawn as "not selected";
* tick the streams of an unticked title back on and the box drew full
  while the rip skipped the title entirely.

## `Tree::toggle`

Both shells carried a comment saying "the core owns cascade + tri-state;
the shell only reports which row was clicked" while each in fact computed
its own answer, and the two answers disagreed: Windows read `Off | Mixed`
as "turn on", macOS read the NSButton's mixed state (`-1`) as "turn off".
So clicking a partly-ticked title selected all of it on one platform and
deselected all of it on the other.

## `Tree::ticked_titles`

Reading the title node's own `checked` flag instead made the drawing and
the rip two independent answers, and `set_checked` on a child never writes
its parent, so they disagreed in both directions: clear every stream row
under a title one at a time and the box drew empty while the stale parent
flag still ripped it; tick a cleared title's streams back one at a time
and the box drew full while the rip skipped it.

## `Tree::ticked_streams_by_title`

The engine used to apply [`Tree::ticked_streams`]'s unioned list to every
title. A feature and its extended cut, say, share PIDs — so unticking a
commentary under title 1 did nothing whenever the same PID stayed ticked
under title 2: the track was written to BOTH outputs, and the tree said
otherwise. The model could always express the per-title decision; only
the request could not carry it. The returned [`TitleStreams`] now names
the no-title-rows fallback case explicitly instead of leaving it to be
inferred from an absence.

Skipping unticked rows before creating a title's slot meant a title whose
rows the user cleared entirely never reached the list, and
`stream_selection_for` read that absence as "no data" and applied the
union — handing the emptied title its sibling's tracks, which on a
shared-PID disc are the very ones just unticked.

A title's slot is created as soon as it is seen to HAVE a stream row,
before the row's tick is read (see above).

## `bib_to_terminologic`

Deliberately a second copy of `freemkv_engine::streams`' table: that one is
private to the engine, and this crate's copy exists to keep a GUI setting
readable, not to select streams. `every_bibliographic_code_the_doc_promises_
resolves` (tests/gui_model.rs) pins the inputs; the engine pins its own.

## `output_formats`

`mp4_ok` false removes the MP4 option rather than offering and refusing it:
a choice that always fails is worse than no choice. `disc_source` is "not
a container" — true for a physical disc AND for an ISO file, because both
carry a whole disc to unpack. That is why "Whole disc → decrypted folder"
(the CLI's `dir://`) is offered for an ISO: `freemkv iso://Disc.iso
dir://out/` is a supported CLI pipeline and the engine's ISO path runs it
(`engine::run_blocking` → `run_extract_folder`).

## `MP4_VIDEO`

Anything else — MPEG-2 from a DVD, VC-1 from an HD DVD, AV1 — has no MP4
mapping, so the mux fails with E9048 after the user has already waited;
say it up front instead. This list admits exactly `Codec::Hevc |
Codec::H264`. It previously also listed AV1, so the desktop app offered
MP4 for an AV1 title, suppressed the pre-rip warning, and then failed at
mux time with a message naming AV1 as supported.

## `missing_key_fallback_tests`

Inverse of [`format_label`]: resolve a LOCALIZED popup label back to the
canonical format string. The shell shows `format_label(canonical)`, so a
non-English selection reads back as translated text — `format_by_title`
only matches the English canonical list, so it would fail in every other
locale. Match on the localized display instead.

A key this crate knows but the pinned i18n tag does not ship must render
as readable English, never as the dotted path. Without the guard in
`format_label` the picker would show `gui.format.video_only` to every
user of a CI build made between wiring a new row and re-tagging i18n.

## `enum_options`

Lives here, not in a shell: this table was duplicated verbatim in
`windows.rs` and `mac.rs` and had already drifted (Windows had grown an
extra arm). A shell that renders a different option set from the other is
a bug by construction, so there is one table and both read it.

## `row_parents`

Both shells rebuild a hierarchical control (an `NSOutlineView`, a
`SysTreeView32`) from the flat `Vec<Row>` the core hands them, and both
were walking the depths themselves. The walk is the same decision on
both, so it lives here. A silently-vanishing title (dropped for lack of a
parent) is far worse than one shown at the wrong indent.

## `the_rip_request_carries_the_identities` test

A ticked title is a NUMBER against the scan on screen, and the rip
re-scans before it uses it. The request therefore has to carry what
those numbers referred to, or the engine has nothing to check the fresh
scan against — the disc can be swapped while the operator reviews the
tree, and title 3 of the new disc is muxed under the old one's name.

A source pin: `start_run` spawns a real rip thread, so no test can
observe the `RipRequest` it builds.

## `a_forced_preference_matching_nothing_keeps_nothing` test

The real case: a UHD feature carrying forced subtitles in French,
German, Spanish and Portuguese, but none in English. Asking for forced
subtitles in English ticked ALL FIVE of them, because this class fell
back to "keep everything" the way audio does when a language is absent.

For audio that fallback is right — a rip with no audio is broken. For
forced subtitles it is actively harmful: they display by themselves
during playback, so the user who asked for English forced subs and has
none gets four languages of unwanted text burned over the picture. A
file with no forced subtitles is, by contrast, entirely normal.

## `neither_about_box_hard_codes_its_version_or_key_count` test

The macOS one did, for all three derived rows, and nothing noticed for
three releases: it read "1.6.0 (macOS)" while the log line beside it
read 1.6.2, and it told every user "keydb ✓ 3 971 entries" — a real
count belonging to whichever machine it was copied from, shown even
with no keydb present. Windows derived all three correctly the whole
time, so this is exactly the kind of drift a shared test catches and
two separately-maintained shells do not.

Source inspection rather than a UI test: neither shell can be
instantiated off its own platform, but both files can be read from
anywhere — which is the point, since this must fail on whichever
machine runs the suite.

## `both_log_panes_keep_the_newest_line_in_view` test

A rip's log only grows, and the interesting line is always the last one
— a warning, a lossy-export note, the failure. `windows.rs` scrolls to
it and says in a comment that it is doing what the macOS log does;
`mac.rs` did no such thing, and had no scroll call anywhere in the file,
so the AppKit user watched a viewport frozen at the top while the run
wrote lines below the fold. The comment described behaviour only one
shell had.

Source inspection for the same reason as the About-box test above:
neither shell can be instantiated off its own platform, but both files
read fine from anywhere — which is the point, since this has to fail on
whichever machine runs the suite.

## `neither_shell_drain_gives_up_on_a_poisoned_inbox` test

Both were `match self.inbox.lock() { Ok(v) => .., Err(_) => return }`.
The early return reads as caution and is a hang: the drain is also what
clears the keydb "busy" flag (the Update button stays disabled forever),
what writes the result into the Settings note, and — critically — what
STOPS the 5 Hz drain timer. Returning skipped all three, so a poisoned
inbox left a timer re-firing for the life of the process, taking the
same poisoned lock and returning again every 200 ms, while the messages
explaining why the worker died sat unread inside it. A poisoned
`Vec<String>` is still a perfectly good `Vec<String>`.

Source inspection for the same reason as the two tests above. Mutation
caught: restoring `Err(_) => return` (or any `if let Ok(..)`) around
either inbox lock.

## `a_failed_probe_says_nothing_but_a_failed_open_still_reports` test

The launch probe opens a drive nobody asked it to open. A drive with an
empty tray is the ORDINARY state of a machine that has one, and
enumerating drives says nothing about whether media is loaded — so a
failed probe is the common case at startup, not the rare one.

It must look exactly like the app did before the probe existed: no
notice, no log line, and the empty page still showing. A prompted open
of the same bad source must still report, because a human asked.

## `a_probe_that_never_answers_is_abandoned_instead_of_ticking_forever` test

`ProbeState` had no deadline and no cancel: while `probe` stayed `Some`,
every tick pushed a `Redraw` and refused to stop the timer, so a drive
wedged inside its SCSI timeouts left the window repainting at 5 Hz
forever with nothing on screen to say why and no way to stop it. The
worker thread cannot be killed — it is blocked in the driver — but the
UI must stop waiting for it.

## `every_offered_format_has_a_translation_key_present_in_english` test

Every string the picker can offer must have a `gui.format.*` key, and
that key must exist in en.json carrying exactly the canonical text.
`format_label`'s catch-all returns the canonical string unchanged, so a
row with no key renders correctly under `en` and is invisible in
testing — but it is untranslatable in the other 28 locales forever.
That is how the three per-track-kind rows shipped keyless. Asserting on
`format_key` (not on the rendered label) is what makes the gap visible:
the label is identical either way.

## `every_offered_format_round_trips_through_its_label` test

A canonical format's localized label must resolve back to that exact
canonical string. The shells persist and the engine matches the
canonical form, so a one-way label is a setting that silently reverts.

REGRESSION PIN, not a fix: this already passed before the keys were
added, because `format_label`'s catch-all round-trips a keyless row
through the `format_by_title` fast path. It is here so that WIRING a
key (which routes the row through `strings::get`) cannot break the
round-trip — the failure mode the key change introduces.

## `format_is_offered`

`View` publishes the chosen format and the offered list as two independent
fields, and nothing kept them in agreement. Opening a source that withdraws
an option — an MPEG-2 DVD after an H.264 Blu-ray withdraws MP4 — left the
model holding a format no longer on the list. A shell then has to invent a
reconciliation policy, and the Win32 one snapped its dropdown to the first
entry without telling the model: the user READ "MKV", pressed Run, and the
engine was handed MP4, which fails at mux time with E9048.

Note: the doc comment immediately above `format_is_offered` in the source
also carries an orphaned fragment ("The path the Information panel shows
as \"Output file\" ... Named the way the engine will name it, so the row
matches what actually lands on disk: `<dir>/<source stem>_t<N>.<ext>`,
where `N` is the 1-based number of the first ticked title. Extracted from
`start_run` so it can be checked without launching a rip.") that predates
a since-removed helper and does not describe `format_is_offered`; kept
here verbatim rather than dropped, per this pass's comments-only scope.

## `App::LOG_MAX`

A multi-hour rip of a damaged disc emits a line per bad-sector retry, and
nothing bounded this during a run — the two `log.clear()` sites are
Clear-log and opening a new source, neither of which fires mid-rip.
Unbounded growth is not just memory: every tick the shells join, re-read
and re-set the WHOLE buffer, so the app got slower the longer it ran,
worst at the end of the longest jobs.

## `App::effective_format`

`self.format` survives closing and reopening the same disc, chosen once
and outliving the source. New source example: pick MP4 on an H.264
Blu-ray, then open an MPEG-2 DVD and MP4 leaves the list. Previously
`view()` published the raw preference alongside a list that no longer
contained it, and the Win32 shell resolved the contradiction by snapping
its dropdown to the first entry and leaving the model alone: the user
read "MKV", pressed Run, and the engine was handed MP4, failing at mux
time with E9048 after the drive had already been read.

## `App::disc_source`

This is the WHOLE decision behind File ▸ Open disc, the empty state's
"Open disc" button and the launch probe — one copy, so the two shells
cannot drift (they had drifted already: the AppKit shell logged three
hardcoded English sentences where the Win32 one used `gui.log.*`).

"No trace" (for `announce_missing == false`) covers more than an absent
drive. A drive with an empty tray is the ordinary case on a machine that
has one, and enumerating drives says nothing about whether media is
loaded — so the probe used to pick that drive, announce it, fail to scan
it, and put an error on screen at every launch. That is worse than the
silence it replaced. When nobody asked, a drive is only worth opening if
it actually holds something.

## `App::open_probe`

The work behind it is a drive enumeration, a SCSI scan and an AACS key
resolution; on the UI thread that froze the window at every launch for
as long as the drive took to answer — seconds on a spun-down drive, and
the whole timeout on a drive that never does. `open` is a direct answer
to something the user just clicked and keeps its synchronous shape; the
probe is not, and had no business blocking the first paint. This change
lands entirely in the portable model: `mac.rs` and `windows.rs` are
untouched, and `windows.rs` compiles on no machine available here.

## `first_visible_row`

A shell that opens every title to match the other shell scrolls the list
as it goes, so the last row expanded — the last title on the disc — is
where the user is left standing. That is never what they came to see.
