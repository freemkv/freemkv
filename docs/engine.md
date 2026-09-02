# engine design notes

Rationale trimmed out of `src/engine.rs` comments to keep them within the
comment-guard's line caps.

## `RunState::outcome_now` poison recovery

`outcome.lock().map(|o| *o).unwrap_or_default()` looks harmless and is not:
`RunOutcome`'s `#[default]` is `Completed`, so a worker that PANICKED — which
is precisely when the verdict matters — poisoned the mutex and the UI
rendered "Finished" over it. The verdict is carried rather than parsed
BECAUSE both abort-for-loss paths once rendered as success.

The same poison-recovery is applied to every `lines` lock in the crate. A
worker that panicked mid-run poisons the log buffer too, and an `unwrap()`
there turns one dead thread into a second panic on the next line written —
losing the diagnostic that would have explained the first.

## `await_worker_exit` and why a quit must wait

`Cmd::Cancel` only SIGNALS: it flips `RunState::cancel` and returns, and the
worker notices at its next boundary, unwinds the mux and drops the sink —
which is what actually closes and finalises the partial file. A quit that
does not wait for that lets AppKit `exit()` the process mid-write, so the
file on disk is whatever the OS write cursor happened to reach rather than
the deliberate "cancelled — partial output kept" artefact the GUI reports.

## `UiSink`'s poison recovery

`if let Ok(..)` reads like defensive coding and is the opposite: a poisoned
`lines` mutex means a worker panicked, which is precisely when the log is the
only record of what happened — and a silent `else` branch would throw away
every subsequent line, so the run would go quiet at the exact moment it
became interesting. Dropping progress frames is milder but the same mistake:
the bar freezes mid-rip with no explanation. A poisoned mutex still holds its
last value and the data behind it is a plain `Vec`/`Prog` with no invariant a
panic could have broken halfway, so recovering is safe as well as correct.

## `error_code` regression history

This used to be `trim_start_matches('E').parse()`, which parses only the bare
form. Every code that carries data — `E7022: <disc hash>`, `E8005: <keydb
path>`, `E6000: <sector> <sense>`, `E6014: <pid>` — returned 0, so
`explain(0)` produced "Mux failed (E0)." for the single most common real
failure (an AACS disc with no key), and the dedicated message for that code
was unreachable from the desktop app. The CLI was unaffected: it uses
`pipe::parse_error_code`, which already parses the digit run.

## `explain`: catalog routing rationale

The library carries no English by design, so a front-end that just prints
`E9048` has told the user nothing. This routes through the catalog — the
same `error.E<code>` keys the CLI renders via `strings::error_message` — so
a desktop-app failure is localized in all 29 locales instead of the
hard-coded English it used to be (GUI/CLI parity).

## `summarize_outcome`: why `Err` carries partial success

`start_rip` files an `Ok` string as the summary and nothing else, so an
outcome returned as `Ok` reads to the user as a rip that worked. A `Failed`
that arrives after some titles already wrote must still say it failed —
reporting "2 title(s) written" for a rip the engine stopped on a full disk is
the same silent-success bug the `code`/`kind` pair was added to make
impossible.

## `summarize_stream`: outcome grading rationale

`completed` is `libfreemkv::MuxOutcome`'s own signal for "drained to a
natural EOF and finalised cleanly" (`false` on an interrupt/halt).
`run_stream` used to discard the whole `MuxOutcome` via a bare `?`, so a
Cancel that landed mid-conversion left a truncated file on disk while still
reporting "Written to <dir>". It takes the outcome rather than two of its
fields (`completed`/`undelivered_streams`) because grading on a hand-picked
subset is how `errors`/`lost_bytes` — the bytes an `mkv://` → `mkv://` re-mux
of a 3D rip drops, with no whole stream missing — came to be read nowhere at
all, and a 3 MB hole rendered as a clean "Written to <dir>".

## `lossy_lines`: why one renderer replaced two

It used to be a second, narrower renderer: this file reported
`undelivered_streams` and nothing else, exactly as the CLI's copy did, so the
loss neither of them reported (`errors`/`lost_bytes` — a Blu-ray 3D dependent
view dropped by an `mkv://` → `mkv://` re-mux) reached no user through either
shell. Two half-answers to one question is the shape this crate keeps having
to un-write; there is one answer now ([`crate::lossy::lossy_lines`]).

## `summarize_image_decrypt`: `CopyResult` regression

This path used to report "Decrypted image written" whenever ANY bytes were
recovered, consulting neither `halted` nor `bytes_unreadable`. Cancel a
decrypt halfway and it claimed success; decrypt an image with unreadable
sectors and it said nothing about them. `CopyResult` carries both signals
precisely so a call site cannot re-derive completion and get it wrong.
Reading `bytes_unreadable > 0` in place of `complete` dropped a third term: a
decrypt that ends with bytes still pending (attempted-and-skipped sectors,
`recovery::copy`'s terminal-result paths) is not complete, and was reported
as a clean write. The partial image is kept in every case, matching the
disc->ISO path's stated policy that an abort never throws away the read.

## `TitleStreams`: the ambiguity it replaces

The field used to be a bare `Vec<(usize, Vec<u16>, Vec<u16>)>` in which a
title's absence meant *either* "this caller has no per-title data, use the
union in `audio_pids`/`sub_pids`" (the CLI and the container path) *or* "the
user deliberately kept nothing under this title". Those want opposite
answers, and the union won both: a title the user emptied was handed its
sibling's tracks. Blu-ray playlists of one feature routinely share PIDs, so
the sibling's tracks are usually exactly the ones just unticked.

## `start_rip`'s `SignalDone` guard

It used to be the closure's last statement, so a panic anywhere in the
scan/mux/recovery chain (all of which run on disc-derived input) skipped it,
`ui::tick` polled `finished` forever, and the window showed a rip in progress
permanently with no error. Mirrors `freemkv-engine`'s `SignalDone`, whose doc
records that two hand-rolled copies of this pattern both had the bug.

## `image_or_dir_scheme` vs `source_scheme`

`source_scheme` classifies by file extension and falls through to `m2ts` for
anything it does not recognise, which is right for its own caller
(`convert_container`, guarded by `is_stream_source`) and wrong here: a disc
image saved as `Disc.img`, `Disc.bin` or with no extension would be handed to
the mux as `m2ts://Disc.img` and re-opened as a single elementary stream.
Reusing `source_scheme` here regressed every image not named `.iso`.

## `planned_output_name`: the bug this replaced

It used to be an independent `format!("{dir}/{stem}_t{n}.{ext}")` in `ui.rs`
that knew about neither the filename template (a shipped setting the engine
applies to every title) nor the disc label (what `run_disc` names titles
after — the source file's stem is only used for a CONTAINER source), and
spelled every whole-disc sink as `_t1.mkv`.

## `sanitize_label` vs `sanitize_name`

The volume label reached the destination path through `title_basename`,
which stripped only `/`, so a label containing `..\..\` escaped the
destination directory on Windows. The CLI has always defended against this
(`pipe::sanitize_name`, whose own test spells out the escape), but the GUI
never called it. Reusing `sanitize_name` here would rename every Japanese,
Cyrillic or accented disc — which the GUI renders correctly today, on every
platform — in order to close a Windows traversal.

## `stream_selection_for`

The audio/subtitle selection for a request goes on `InputOptions`, NOT
`MuxOptions`: our mux uses a URL source (`iso://…`), and `mux_stream`'s Url
arm prunes via `InputOptions.selection` — `MuxOptions.selection` is only
consulted on the File/Session (live-drive) arms. Putting it on `MuxOptions`
silently kept every track. `title` is the CANONICAL disc title index; when
the request carries a per-title breakdown, that title's own ticked PIDs are
used — INCLUDING an empty one, which is the user clearing every row under
that title. Only `TitleStreams::Unspecified` falls back to the union in
`audio_pids`/`sub_pids`.

## `title_session_mux_opts`

It used to build one `MuxOptions` from the UNION of every title's ticked PIDs
before the loop and clone it for each title — exactly the defect the ISO
path already had: two playlists of one feature routinely share PIDs, so
unticking a commentary under title 1 wrote it anyway whenever title 2 still
had it ticked.

## `title_input_options`

Three fields, each of which fails silently and differently if it goes
missing:

- `title_index` — without it the library defaults to `None` and muxes the
  WRONG TITLE, under the filename the user asked for. Nothing downstream can
  tell.
- `unit_keys` — the scan resolving a disc's AACS keys is not enough; they
  have to reach the mux or every encrypted title fails E7022. Mirrors the
  engine's own wiring in `run.rs`.
- `selection` — the ticked audio/subtitle tracks, applied by the Url mux
  path. Dropped, the user gets every track they deselected.

## `an_uncaptured_title_does_not_disarm_the_rest_of_the_selection`

The identities are indexed by canonical title number, exactly like
`picked_ids` in the live-drive loop, so "nothing recorded for title 3" is
`ids.get(3) == None` — a per-title answer. Keyed by SELECTION position
instead, a list that was one entry short made the function return every raw
position unchanged, so ONE unknown title silently put the whole batch back
on the stale-position path this exists to close.

## `damage_note`: the disclosure gap it closes

Without this, a disc that recovers with real unreadable sectors UNDER
`abort_lost_secs` reports a plain "ISO image written to …" / "N title(s)
written to …", identical to a perfect rip: the CLI's own `disc_to_iso`
prints exactly this figure (`rip.mapfile_summary` / `rip.damage_lost_movie`)
for the same condition, so the GUI was hiding damage the CLI discloses.
Reuses those two existing i18n keys rather than adding new ones.

## `recovery_raw`: the shipped-defaults regression

`multipass_rip` REFUSES a real sweep-plus-patch plan with `raw = false`
("multipass implies raw"): a whole-disc image recovery reads sectors it
cannot attribute to a title, so it cannot decrypt them. The GUI was handing
it `req.raw`, which `ui::raw_applies` forces to false for any title output —
so with the SHIPPED DEFAULTS (rip mode "Multi-pass", 5 passes, raw off)
every live-drive rip died before reading a sector, with the engine's own
refusal as the error text. The ISO-output path failed the same way whenever
the user had not ticked "keep encrypted". The one case with no answer is
"whole disc → ISO image", multipass, raw off: refused HERE, before the
drive is staged, with something the user can act on.

## `TitleIdentity`: why duration alone was not enough

This module used to carry its OWN answer — playlist name plus duration —
which cannot separate the case the project's rules name outright: "titles
legitimately carry duplicate playlists with identical duration and size".
Duration is precisely the field that legitimately collides. Two titles
reading the same sectors from the same playlist produce byte-identical
rips, so there is nothing left to confuse once EXTENTS/sectors are keyed on.

## `remap_titles_by_identity`: keyed by title number

`ids` is indexed by CANONICAL title number, not by position in `titles` —
the same shape `picked_ids` uses in the live-drive loop. That keeps the
fallback honest: "no identity for title 3" is `ids.get(3) == None`, a
per-title answer, so a title nobody captured costs only itself. Keyed by
selection position instead, a list that came up one entry short made this
return every raw position unchanged, putting the WHOLE batch back on the
stale-position path this exists to close — silently, and for titles whose
identity was known. A title that CANNOT be found is a hard error: muxing
the remaining ones silently would deliver a subset under the same summary.

## `verify_title_identity`: the single-pass drive gap

The single-pass drive path is the same "position is not identity" shape
`remap_titles_by_identity` handles for the recovery path, one scan later:
`run_disc` scans once to resolve the selection, then EVERY title in the loop
re-opens the drive and scans again, carrying only an integer. A second scan
that lists the titles in a different order, or drops one before the selected
index, still resolves that integer — to a different title, muxed under the
name the user asked for. `expected` is `None` when nothing was recorded for
that index, which leaves the pre-existing behaviour untouched.

## `verify_selection_identity`: detail

The window this closes is the one the per-title check cannot see: the user
ticks titles against the scan on screen and then reviews streams, format and
destination before pressing Start. `run_disc` takes a brand-new scan at that
point, and the ticked numbers were resolved against it with nothing to say
they still refer to the same titles — swap the disc (or let the drive
enumerate differently) and the wrong film is muxed and reported as success
under the name the first disc earned. `picked` is indexed by canonical title
number; an empty `picked` (a caller that saw no scan) disables the check.

## `should_delete_staging_iso`: detail

`keep_iso` alone deleted the image on paths the same function's own policy
says never throw away the read: the mux reports a CANCEL as
`Ok("Cancelled — …")`, so a user who stopped the mux lost the multi-hour
recovery behind it and could only get it back by re-reading the disc; a mux
that failed outright (no space, a missing key, the destination removed)
deleted the one artefact that would have let the user retry the mux alone.
Cancellation is read as a FLAG, never from the summary text — the same rule
`RunOutcome` exists to enforce.

## `run_disc`: detail

Title/metadata/demux sinks mux each selected title off a fresh session
(`fe::mux_title_session`, driven through `fe::run_titles` for the shared
fail-fast/skip/halt policy — the exact loop the ISO path uses);
decrypted-folder extracts the UDF tree to a per-disc subdir. Whole-disc ISO
image is flagged as not yet wired for the GUI (needs the mapfile copy path).

## `run_state_poison_tests`: what each test pins

**`a_poisoned_lock_does_not_turn_a_failure_into_a_completion`** — the UI used
to read the verdict with `unwrap_or_default()`. `RunOutcome`'s `#[default]`
is `Completed`, so a worker that panicked — poisoning the mutex, precisely
the case where the verdict matters — rendered the "Finished" heading over a
run whose real outcome was lost, along with an empty summary line. That is
the same defect `RunOutcome`'s own doc says it exists to prevent: the
verdict is carried rather than parsed BECAUSE both abort-for-loss paths once
rendered as success.

**`the_ui_sink_still_delivers_lines_and_progress_through_a_poisoned_lock`** —
`UiSink::log` was `if let Ok(mut v) = ..lock() { v.push(..) }`, which reads
as caution and behaves as censorship: `UiSink` is the ONE `Sink` the shared
GUI core installs, so every library log line reaches the user through it.
Once any thread panicked holding the buffer the `else` arm swallowed every
line from then on — the run went silent at exactly the moment the log
became the only evidence of what happened. `progress` had the same shape and
froze the bar. Mutation caught: putting either method's `unwrap_or_else`
back to `if let Ok(..)`/`unwrap()`.

**`every_lines_lock_recovers_from_poison`** — that doc says the
poison-recovering form is applied to EVERY `lines` lock, and that
"the source-inspection pins below" enforce it. They did not: the pin above
covers `outcome` and `summary` only. Meanwhile the CANCELLED arm of
`run_stream` and both drain loops in `main.rs` used a bare `unwrap()`, so a
rip cancelled after an earlier worker panic re-panicked while writing the
line naming the partial file it had kept. A source pin because the log
buffer is written from paths that need a live drive or a real disc image;
`main.rs`'s drain loop needs a whole process. Reading the text is the only
thing that can see all of them at once. Both needles are built with
`concat!` so this test's own body cannot match itself. Mutation caught: any
`lines` lock anywhere in these two files reverting to `unwrap()`,
`if let Ok(..)`, or `unwrap_or_default()`.
