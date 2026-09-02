# src/pipe.rs — internal design notes

Long-form rationale relocated out of private (non-`pub`) doc comments in
`src/pipe.rs` to keep the comment-guard's per-comment cap. Each section is
pointed to by a one-line `// See docs/pipe.md#<anchor>` comment at the
original call site.

## SigintHalt

(originally at `src/pipe.rs:61`)

Bridge the process-wide SIGINT flag ([`INTERRUPTED`]) into a real
[`libfreemkv::Halt`] that the library's long-running loops poll.

`libfreemkv::mux_stream` (and `extract_tree`) take a `&Halt`, not the global
flag — there is no `None` and no hidden global to consult. A watcher thread
flips the halt the moment SIGINT arrives so a long mux/extract stops at the
next frame/file boundary; the guard signals the watcher to exit and joins it
on drop (normal return OR unwind). This is the ONE place the CLI's SIGINT
reaches libfreemkv, replacing the old `INTERRUPTED`-polled-in-the-mux-loop.

## PipeFail

(originally at `src/pipe.rs:118`)

A per-title mux failure carrying both the display string (for the localized
render / skip notice) and whether it was a *skippable title stub*.

The skippability is decided by [`libfreemkv::error::is_skippable_title_stub`]
on the typed `io::Error` `mux_stream` returns — NOT by matching `E7023`/`E6008`
substrings. Setup failures (no drive, out-of-range title, decrypt gate) are
never skippable stubs and construct with [`PipeFail::fatal`].

## from_typed

(originally at `src/pipe.rs:154`)

A typed library error (a preflight decrypt gate). The engine classifier
maps a disc-level no-key (NoDiscKey / KeydbLoad / AacsNoKeys) to
`DiscLevelNoKey` so the loop fails fast instead of iterating every title
with the same error. Display is the error's own `E<code>` rendering.

## CliMuxEvents

(originally at `src/pipe.rs:178`)

The CLI's [`MuxEvents`] implementation: it renders exactly what `pipe`/
`pipe_disc` used to print inline around the frame loop — the stream-info
block, the destination "opening…/ok" pair, and the throttled progress line —
now driven by callbacks from inside `mux_stream`.

Progress is driven from the WRITE side ([`Self::on_write_progress`],
`output.bytes_written()`), exactly as the old loop's
`print_progress(output.bytes_written(), …)`. Reader-side events are ignored
(the CLI never rendered per-sector skips / batch changes on this path).

`Output` is `Copy` (a single verbosity level) so the handle is `'static` and
`Send + Sync`, satisfying `Arc<dyn MuxEvents>`.

## finalize_mux

(originally at `src/pipe.rs:255`)

Render the outcome of a `mux_stream` run into the CLI's exit contract, shared
by `pipe` and `pipe_disc`:
- `Ok(completed)` → clear the progress line, print the completion summary
(unless a metadata sink, which prints none), report any stream the sink
could not deliver, succeed;
- `Ok(!completed)` → an operator interrupt (SIGINT flipped the halt) or a
finalize wedge — print the "incomplete" notice and fail (non-zero exit),
NEVER report a truncated file as success;
- `Err(e)` → the typed failure, classified for the per-title skip triage.

## print_lossy_outcome

(originally at `src/pipe.rs:288`)

Print everything a COMPLETED mux still has to say about what it lost.

The rendering itself is [`crate::lossy::lossy_lines`], shared with the GUI
so the two shells cannot answer this differently again — they already had:
this function reported `undelivered_streams` and the GUI reported the same
field, while `MuxOutcome::errors` / `lost_bytes` (bytes the library read and
could not carry — a Blu-ray 3D dependent view, in the case that produces
them today) were read by NEITHER, so a re-mux that dropped one eye of the
film printed "Complete" and exited 0.

`Level::Always`: a lossy outcome is never silent, so `--quiet` must not hide
it — the same rule the unmatched-language warning follows. On a `stdio://`
rip `Output` is routed to stderr, so this cannot corrupt the piped bytes.

Not a `PipeFail`: the file is finalised, structurally valid and playable, it
is missing content rather than truncated, and truncation is the only thing
the exit contract promises about. Every `PipeFail` from here reaches
`freemkv_engine::decide_title`, where `Failed` abandons the title and
`Halted` full-stops the batch — too blunt for a loss the user can act on
themselves by re-running to `mkv://`.

## fmt_err_str

(originally at `src/pipe.rs:325`)

Render a libfreemkv `E<code>[: <data>]` Display string (or any string) into
the user's language. The library emits errors as `E<code>` or
`E<code>: <data>` (see libfreemkv `error.rs` Display) with NO English; the
CLI owns all i18n. This parses the code, looks up `error.E<code>` in the
locale table, and renders it — for ANY code that has a locale entry — so no
raw `E####` ever reaches a user.

The data after the colon is passed as `{detail}` for the generic case, and
E7022 additionally exposes its disc hash as `{hash}` (its locale string
names the disc). A code with NO locale entry falls back to `error.generic`,
which still echoes the raw `E<code>: <data>` inside a localized wrapper —
the last-resort path, not the common one.

## check_selection_coverage

(originally at `src/pipe.rs:381`)

Handle the `-a`/`-s` "no matching stream" case for ONE title: a requested
language is absent from a track class the title actually carries. Without
this the rip silently ships a file missing that whole class.

- Returns `Ok(())` to proceed. For a **multi-title** rip the missing class is
a per-title WARNING (printed here) and the title keeps its video + whatever
else matched — a batch over a mixed-language library must not hard-fail on
one title.
- Returns `Err(rendered)` for a **single-title** rip: the user asked for a
language that isn't there, so fail loud with the languages that ARE. The
caller prints/propagates `rendered` in its own idiom.

## parse_error_code

(originally at `src/pipe.rs:479`)

Parse a libfreemkv Display string of the form `E<code>` or
`E<code>: <data>` into `("E<code>", "<data>")` (data empty when absent).
Returns `None` for any string that isn't an `E<digits>` code (so arbitrary
CLI error strings fall through to the generic wrapper unchanged).

## parse_stream_spec

(originally at `src/pipe.rs:524`)

Parse an `-a`/`-s` value into a [`freemkv_engine::StreamFilter`]:
`all` → All, `none` → None (video-only for that class), otherwise a
comma-separated language list (names or ISO codes, trimmed, empties
dropped). Keywords are case-insensitive.

## parse_flags

(originally at `src/pipe.rs:576`)

Parse rip flags, returning a clear error string on any misuse:
- `-t`/`--title` with a missing, non-numeric, or `0` value (titles are
1-based; never silently fall through to "all titles").
- `--keydb` with a missing value (never silently use the default).

A value-flag will not consume a following positional URL token
(`scheme://...`) as its value — that means the value is missing.

## is_scheme_only_sink

(originally at `src/pipe.rs:1111`)

Whether a destination is a scheme-only sink with no filesystem path —
`null://` (discard) or `stdio://` (stdout). Such a sink consumes every
selected title through the SAME URL: it can't be given per-title file names,
so the multi-title job builder must not route it through `dir_jobs` (which
would synthesize an invalid `null://stem_t1.null` path).

## preflight_validate

(originally at `src/pipe.rs:1123`)

Validate the whole rip invocation BEFORE any drive open, scan, or file
creation. Returns `Err(message)` — a single, already-localized, ready-to-
print string — on the first problem, so the caller prints it and exits
non-zero with no partial output. `Ok(())` means every checked precondition
holds and the rip may proceed.

Checks, in order (cheapest / most-fundamental first):
1. Source and destination both carry a URL scheme (`scheme://…`).
2. `--raw` / `--multipass` are used only with an `iso://` destination.
3. Source is reachable: a `disc://` device path that is given must exist; an
`iso://` input must exist, be a file (not a dir), and be non-empty.
4. The destination is not the SOURCE. Every sink opens its file with
`File::create`, which truncates the still-open input — the one mistake
that costs the user their only copy.
5. Destination is writable: for a single-file `mkv://`/`m2ts://`/`iso://`
output the parent directory must exist and be writable, and the path must
not already be a directory.

Deep validation (a real UDF/ISO filesystem probe, a live drive handshake) is
left to the scan step, which surfaces its own typed errors; this is the
cheap, side-effect-free gate that catches the common mistakes instantly.

## validate_dir_input

(originally at `src/pipe.rs:1310`)

Validate a `dir://` SOURCE: it must exist, be readable, and be a directory.

The mirror of [`validate_iso_input`]. `dir://` became a first-class input in
1.6.1 and this check did not arrive with it, so a typo'd folder produced an
"Opening ...OK" line followed by a raw OS error instead of the specific,
localized message an `iso://` typo gets.

## validate_iso_input

(originally at `src/pipe.rs:1344`)

Validate an `iso://` input path: must exist, be a regular file (not a
directory), and be non-empty. A deeper "is it a real disc image?" probe is
the scan's job; this catches the instant mistakes (typo'd path, a directory,
a 0-byte stub) before any scan work.

## validate_file_dest

(originally at `src/pipe.rs:1393`)

Validate a single-file destination path: the parent directory must exist,
the path must not already be a directory, and the location must be writable.
Catches "parent dir doesn't exist" and "no write permission" up front instead
of after a scan + mux has already run.

## disc_title_identities

(originally at `src/pipe.rs:1459`)

The titles this disc has, for expanding `-t all` — one [`TitleIdentity`] per
title, in scan order, so the count is `len()` and index `i` carries the
identity of the title `-t {i+1}` was expanded from.

One extra drive open+scan before the per-title loop. `None` on any failure:
the caller falls through to the normal path, which opens the drive again and
reports the real error in the usual place, rather than growing a second
error-reporting path here.

## disc_title_nums

(originally at `src/pipe.rs:1494`)

The title list a DISC source should rip, given `-t all` / `-t N` and the
number of titles the scan actually found.

`-t all` on a disc used to reach `build_jobs` with an EMPTY `title_nums`,
which matches neither the scanned-source arm (a disc has no upfront title
list) nor the multi-title disc arm (which requires `len() > 1`) — so it fell
to the catch-all, produced ONE job, and `pipe_disc` ripped title 1 and
exited 0. The same flag against an `iso://` of the identical disc rips
everything, because there `titles` is `Some(..)`.

Expanding it here rather than inside `build_jobs` keeps this a pure decision
the tests can apply directly — including
`shipped_help_examples_parse_and_build_runnable_jobs`, which calls
`build_jobs` itself and would otherwise never see the expansion.

## resolve_disc_all_titles

(originally at `src/pipe.rs:1515`)

Resolve the `(title_nums, identities)` pair for a `-t all` DISC rip from the
result of the upfront scan, or `None` to signal "abort loudly".

`scan` is `disc_title_identities(..)`: `Some(ids)` when the drive was
scanned, `None` when it failed (`.ok()?`, silently). `-t all` REQUIRES that
scan — it is the only source of the title count — so a `None` here must NOT
be papered over. Returning `None` from this function is the caller's cue to
print an error and exit non-zero, instead of letting an empty `title_nums`
fall through `build_jobs` to its single-title catch-all and rip title 1 with
rc 0. Pure so the abort decision is unit-testable without a drive.

## build_jobs

(originally at `src/pipe.rs:1533`)

Build the `(title_index, dest_url)` job list.

- Scanned source (ISO, etc.) with a title list: select the requested titles
(or all, when none given); one file when a single title goes to a file,
else one file per title in a directory.
- Disc source: there is no upfront title list, so build straight from
`title_nums`. Multiple `-t` flags each get their own job (writing to a
directory when more than one is selected) instead of silently dropping all
but the first. Empty `title_nums` is the single all-titles pass.

Returns `None` (after printing the error) if a needed output directory can't
be created, so the caller can exit non-zero.

## keyless_scan_opts

(originally at `src/pipe.rs:1697`)

Disc source: one open, one scan, one stream. No double init.
ScanOptions for a keyless structure scan — libfreemkv captures structure +
AACS inputs but resolves no key. The CLI resolves the key afterward from the
local keydb (see [`resolve_disc_keys`]).

## drive_scan_opts

(originally at `src/pipe.rs:1705`)

ScanOptions for a **live-drive** scan: keyless, plus the AACS host
credentials for the authenticated handshake (sourced from the local keydb).
A locked drive needs the cert to read its Volume ID; an unlocked /
firmware-unlocked drive takes the OEM path and ignores them. ISO scans use
[`keyless_scan_opts`].

## dest_is_directory

(originally at `src/pipe.rs:1717`)

Is the destination directory-STYLE — a trailing `/`, or an existing
directory on disk? Decides whether a multi-title rip is allowed (one file per
title inside it) or rejected (a single file cannot hold several titles).
Extracted from [`run`] so tests can classify a destination through the same
code the CLI uses instead of restating the rule.

## drive_credentials

(originally at `src/pipe.rs:1726`)

Build the AACS host credentials for a live-drive handshake from the local
keydb — the CONSUMER-side cert extraction (libfreemkv derives none of this;
it only forwards what we hand it). `None` when the keydb carries no host
cert. Used to populate [`libfreemkv::KeySpec::credentials`].

## resolve_info_keys

(originally at `src/pipe.rs:1738`)

Resolve a **live drive's** AACS unit keys in place for `disc-info -v`: sample
ciphertext from the largest title and run the local-keydb key source against
it (no online source — `disc-info` never phones a key service). Populates
`disc.aacs.unit_keys` / `vuk` so the verbose crypto block can show a REAL
resolution instead of the keyless 0. No-op for an unencrypted / non-AACS disc
(`inputs()` returns `None`). The drive must still be open and have been
scanned with [`drive_scan_opts`] so the handshake captured the VID + inf.

## scan_iso

(originally at `src/pipe.rs:1759`)

Scan an `iso://` source's structure ONCE (keyless). The resulting `Disc` is
shared by title enumeration and unit-key resolution so the ISO is not
re-parsed per step; the returned reader is reused for ciphertext sampling
in `resolve_iso_unit_keys`. `None` for a non-iso source or an unreadable
image.

## resolve_iso_unit_keys

(originally at `src/pipe.rs:1779`)

Resolve an ISO's AACS unit keys from an already-scanned `Disc`: sample its
largest title, then local keydb, then decrypt_with. Empty for an unencrypted
ISO or when no key resolves.

Reuses the reader returned by `scan_iso` (the ISO was already opened +
scanned once) purely to sample ciphertext — no second file open, no second
structure scan.

## build_iso_key_fetch

(originally at `src/pipe.rs:1799`)

Build the fresh-key-on-failure closure for an ISO mux, or `None`.

When an online key service is configured (`--key-url`), this returns a shared
[`libfreemkv::sector::KeyFetch`] (built by [`libfreemkv::keysource::key_fetch`])
that the iso:// mux installs into the decrypt decorator. If a unit no held key
decrypts, the decorator hands that ciphertext to the closure, which forwards it
(as content samples) to the key service via [`freemkv_keysources::OnlineSource`]
and returns any unit keys the service derives — mirroring the DVD model (held
key first, ask the key source for the failing data). `None` when no key URL is
set, the URL is SSRF-rejected, or the source isn't an AACS ISO. The library
still makes no network call — this closure is the application's seam to the
key service. The fetch logic lives in the lib; the CLI supplies only the disc
inputs and the source builder.

## resolved_keydb_path

(originally at `src/pipe.rs:1864`)

The keydb path to use: `--keydb <path>` if given; else the first
per-OS search location that exists (Windows `%APPDATA%\freemkv\keydb.cfg`
then the legacy `.config` dotfolder; Linux/macOS `~/.config/freemkv/keydb.cfg`),
else the canonical default location for that OS, else a bare `keydb.cfg`
in the cwd. The search/default policy lives in `freemkv-keysources`.

## build_key_sources_quiet

(originally at `src/pipe.rs:1878`)

Build the ordered `KeySource` list from the key flags, **local-first**:

- `--key-url` only → `[OnlineSource]` (no keydb consulted).
- `--keydb` only / neither → `[KeydbSource]` (the standard CLI behaviour;
"neither" still uses the default keydb location).
- both → `[KeydbSource, OnlineSource]` — a local keydb hit wins and never
makes a network round-trip; the service is the fallback.

`--key-url` is SSRF-validated (via the shared
[`freemkv_keysources::validate_keyserver_url`]) before the online source is
added; a rejected URL prints a warning and the online source is dropped (the
keydb, if any, still applies) rather than POSTing key material to an
internal/metadata host.
The ordered `KeySource` list, WITHOUT any user-facing warning — the pure
build used inside the [`libfreemkv::KeySourceFactory`] closure, which is
invoked repeatedly (per on-decrypt-miss fetch) and must not re-warn. An
SSRF-rejected `--key-url` is silently dropped here; the visible warning is
emitted ONCE up front by [`build_key_sources_quiet`] / [`key_source_factory`].

## key_params

(originally at `src/pipe.rs:1900`)

Normalize the CLI's flags into the engine's `KeyParams`, preserving the
CLI's IMPLICIT online-only derivation (`--key-url` alone, no `--keydb`) and
its default-keydb-location search chain — both stay CLI-boundary concerns;
the engine only sees the already-resolved result.

## key_source_factory

(originally at `src/pipe.rs:1938`)

Build the [`libfreemkv::KeySourceFactory`] the library's key resolution
([`libfreemkv::resolve_keys_for`] / [`libfreemkv::DiscSession::resolve_keys`])
calls to (re)build the ordered sources. Emits the one-time SSRF warning here;
the returned factory is quiet.

## resolve_disc_keys

(originally at `src/pipe.rs:1959`)

Resolve an AACS key for a keyless-scanned `disc` from the configured sources,
reading ciphertext samples through `reader`, and render the structured walk.
No-op for an unencrypted disc (no AACS inputs). Thin app-layer wrapper over
[`libfreemkv::resolve_keys_for`] (which owns sampling / ordered-apply /
banking / fetch construction).

## render_resolution_trace

(originally at `src/pipe.rs:1975`)

Render a [`libfreemkv::aacs::trace::ResolutionTrace`] into human-readable
`who > node > … > OUTCOME` lines — one per unlocker and per key source
consulted. The library trace is English-free typed enums; ALL English
mapping lives here in the app layer. Mirrors autorip's renderer (the two
apps are separate crates, so the mapping is duplicated, not shared).

## is_metadata_sink

(originally at `src/pipe.rs:2151`)

Whether a destination URL is a metadata sink (`chapters://` / `json://`) —
one that writes its whole file from the scanned title at `output()` time and
consumes no PES frames. `mux_stream` short-circuits these BEFORE the header
gate; the CLI only needs to know so it suppresses the completion summary.

## disc_copy_recovered_data

(originally at `src/pipe.rs:2162`)

Whether a completed disc→ISO sweep actually recovered any readable data,
the guard `disc_to_iso` runs before declaring success — the sweep-path
analogue of `mux_produced_output`. `Disc::copy` returns `Ok` even when every
ECC block was unreadable and zero-filled (whole disc unreadable): the ISO on
disk is all zeroes and unusable. Returns `false` (→ caller prints `rip.no_data`
and exits non-zero) when nothing readable came off the disc; `true` only when
at least one byte was recovered.

## disc_copy_succeeded

(originally at `src/pipe.rs:2191`)

Whether a graded copy is a SUCCESS for the exit code.

Only one verdict is. A lossy image joins the two failures here rather than
the success: `$?` is the only thing a script can see, and "24.9 of 25 GB"
is not the disc the user asked for. This mirrors the `dir://` extraction
path, which already prints its per-file loss and then returns failure "so
scripts can detect 'extracted but holed'". The image is kept in every case.

## copy_verdict

(originally at `src/pipe.rs:2202`)

Grade a `CopyResult`. Two of the three verdicts are failures, and BOTH of
them arrive as `Ok(_)` from the engine — a halted sweep and a whole-disc
read failure are both "the copy function returned normally". If either is
misgraded the CLI prints `rip.complete` over an unusable image and exits 0,
which is the single failure this crate must never commit.

Order matters: a halt is reported as interrupted even when it also recovered
nothing, because "you stopped it" is the more useful thing to tell the user
and the mapfile makes it resumable.

## disc_copy_options

(originally at `src/pipe.rs:2226`)

The copy options a CLI disc→ISO sweep runs under.

`decrypt: !raw` is the one that matters most: inverted, the default flags
write a CIPHERTEXT ISO and say nothing. `multipass` dropped means `--multipass`
is accepted and recovery never runs; `progress` dropped means a multi-hour
sweep prints no progress at all.

## pipe

(originally at `src/pipe.rs:2255`)

Decide whether a per-title mux failure should abort the WHOLE rip (fatal) or
be skipped with a warning so the remaining titles still mux.

Core principle: **NO FALSE ERRORS, and a failure in one extra title must not
kill the whole rip.** When `freemkv iso://X mkv://dir/` muxes ALL titles, one
One title: open input, open output, stream PES frames.
Used for non-disc sources (ISO, MKV, M2TS, network, stdio).

## url_path_of

(originally at `src/pipe.rs:2303`)

The filesystem path behind a URL — for EVERY scheme that names one.

The same-file guard needs this on both sides of the invocation, so the
match is exhaustive on purpose: no `_` arm. A scheme added to
`libfreemkv::StreamUrl` fails to compile here rather than silently arriving
unguarded, which is exactly how the round-3 fix — matching `Iso` and `Dir`
alone — left every other pairing able to write over its own source.

The path is returned even for a write-only sink (`fvi://`, `chapters://`,
`json://`, the per-track directories): those are precisely the destinations
that truncate, and one of them aimed at the source's own path is the same
data loss as `mkv://X mkv://X`.

## image_to_iso

(originally at `src/pipe.rs:2347`)

`<image source> → iso://` — write a decrypted sector image from a source that
is NOT a physical drive.

This is the generic `iso://` sink. It is deliberately not
[`disc_to_iso`]: that one is the recovery path (mapfile, `--multipass`,
damage-jump, auto-resume), all of which exists because an optical drive
returns read errors on marginal media. A file-backed source has no marginal
media, so it gets a plain sequential write instead — see
`libfreemkv::io::image_writer` for the full reasoning, including why sharing
the recovery path would let a mapfile resume over a DIFFERENT source.

`--raw` and `--multipass` cannot reach here: they are drive flags and
`preflight_validate` rejects them for a non-drive source.

## dir_to_extract

(originally at `src/pipe.rs:2702`)

Extract a disc's decrypted file tree to a host directory (`dir://`). Routed
here (before the generic mux path) for a `dir://` dest with a disc-source
input (`disc://` or `iso://`). 1-shot, decrypt-only — recovery for damaged
media is the `disc→iso --multipass` then `iso→dir` workflow. Returns true on
success (a fully-clean tree); a lossy extraction prints the per-file summary
and returns false (→ non-zero exit) so a script can re-run via the ISO path.

## HaltSink

(originally at `src/pipe.rs:2809`)

A [`freemkv_engine::Sink`] that forwards `should_cancel` to a
[`libfreemkv::Halt`] the CLI already maintains (bridged from SIGINT via
[`SigintHalt`]). `freemkv_engine::extract_tree` does its own should_cancel
→ Halt bridging internally (a second, generic layer); this adapter is the
thin seam that lets the CLI keep `SigintHalt` — the actual OS-signal
bridge — entirely in the shell, as required.

## extract_succeeded

(originally at `src/pipe.rs:2823`)

Whether a `dir://` extraction counts as a success — and therefore whether
`freemkv` exits 0.

A halted extract is a failure (the tree is a prefix of the disc), and so is
an incomplete one: `extract_tree` returns `Ok` for a run that wrote every
file with holes in them, and a script that only checks `$?` would file a
holed tree as finished media. Scripts rely on the non-zero exit to detect
"extracted but holed" and re-run via ISO multipass, so this is a contract,
not a nicety.

## fmt_disc_damage

(originally at `src/pipe.rs:2964`)

Render the disc-level damage string ("lost" / "no loss") for the live
progress line.

"Lost" means READ FAILED only: `bytes_unreadable_total` (gave up) plus
`bytes_retryable_total` (failed, awaiting retry — NonTrimmed/NonScraped).
It deliberately does NOT include `bytes_pending_total`, which also folds
in not-yet-attempted (NonTried) sectors — counting those would make a
healthy in-progress rip report most of its remaining runtime as "lost".
The title-level path (`bytes_bad_in_main_title`) is already failed-only.

## debug_drive_step

(originally at `src/pipe.rs:3102`)

Log a discarded drive-handshake step error to stderr (debug-grade). These
steps (`wait_ready`, `init`) are best-effort — the subsequent scan re-derives
what it needs — but a failure here is a useful breadcrumb when a later scan
fails, so surface it instead of silently dropping it. The common Ok path is
silent.

## print_completion_summary

(originally at `src/pipe.rs:3113`)

Clear the progress line and print the final `rip.complete` summary. Shared
by `pipe_disc` and `pipe` (identical tail). `\r\x1b[K` erases from the cursor
to end of line, so it adapts to any progress-line width instead of relying on
a fixed run of spaces.

## print_mp4_skips

(originally at `src/pipe.rs:3211`)

For an `mp4://` destination, print the tracks that can't be carried in MP4
(bitmap subs; unmappable audio like TrueHD/LPCM — DTS/DTS-HD ARE carried;
secondary video views; and primary video whose codec has no MP4 mapping like
VC-1/MPEG-2) so a compatibility export is never a silent drop. No-op for
every other scheme.

## mp4_skip_reason_key

(originally at `src/pipe.rs:3247`)

The i18n key for an `mp4://` exclusion reason. Each variant maps to a DISTINCT
string — in particular `SecondaryVideo` ("secondary video view", a dependent
MVC/3D view) must NOT share a message with `UnmappableVideo` (a PRIMARY codec
the MP4 writer can't carry, e.g. VC-1/MPEG-2), or a DVD/BD→mp4:// export would
print "secondary video view" for the main video it is actually dropping.

## is_url_token

(originally at `src/pipe.rs:3271`)

Whether a token is a positional stream URL (`scheme://...`) rather than a
flag value. A value-flag (`-t`, `--keydb`) must not swallow one of these.
`pub(crate)` so `cli_entry`'s copy of the `--log-file`/`--log-level` guard
uses the SAME predicate — the sibling that lacked it swallowed a source URL.

## is_keyserver_url

(originally at `src/pipe.rs:3279`)

Whether a token is a plausible key-service URL value for `--key-url` — i.e.
an `http(s)://` URL. This is the gate that lets `--key-url https://…` accept
its value (which `is_url_token` would otherwise treat as a positional stream
URL) while still rejecting a missing value (a following flag, or a stream
URL with a non-http scheme like `disc://`). The full SSRF/host validation is
`freemkv_keysources::validate_keyserver_url`, applied at source-build time.

## title_in_range

(originally at `src/pipe.rs:3296`)

Whether a 0-based title index is within a source's title count. An explicit
out-of-range `-t` on a scanned source is a hard failure (the caller sets
`ok = false`), so the CLI exits non-zero instead of reporting success after
ripping nothing.

## TitleIdentity_use

(originally at `src/pipe.rs:3304`)

What identifies a scanned title ACROSS an independent re-scan of the same
disc.

A disc rip carries only an integer between scans: the upfront scan that
expands `-t all` produces a title count, `build_jobs` turns that into
positional indices, and every `pipe_disc` call then re-scans the drive and
indexes `disc.titles[idx]`. Position is not identity — if the second scan
returns the titles in a different order, or drops one BEFORE the requested
index, the index still resolves and a DIFFERENT title is muxed under the
requested number, silently. `title_in_range` only catches the case where the
list got short enough for the index to fall off the end.

The type itself lives in [`crate::title_identity`] and is SHARED with the
GUI's `engine`, which asks the identical question one scan later. It was
defined twice — once here and once beside `engine::verify_title_identity` —
and the two definitions disagreed about what a title IS. One definition,
in one place, is the fix; a second one beside a call site is the bug.

## title_changed_message

(originally at `src/pipe.rs:3323`)

The message shown when a re-scan's title no longer matches the one the job
list was built against.

`error.title_changed` is not in the pinned `freemkv-i18n` tag yet, and
`strings::get` echoes the dotted path for a key the catalog does not ship.
Same guard `ui::format_label` uses: treat the echo as "no string" and print
readable English rather than `error.title_changed`.

## job_identity

(originally at `src/pipe.rs:3349`)

The identity recorded for one job, out of the upfront scan's list.

`identities` is indexed exactly as the jobs are — entry `i` is the title
`-t {i+1}` expanded to — and is EMPTY when no upfront scan happened. `None`
then means "nothing to verify against", which is the pre-existing behaviour
for an explicit `-t N`. A job with no title index (`None`) is the whole-disc
/ single-title case that `pipe_disc` treats as index 0.

## resolve_scanned_title

(originally at `src/pipe.rs:3360`)

Pick the title a job refers to out of a FRESHLY scanned title list.

The one place an index from an earlier scan meets a later scan's titles.
Returns the error message to fail the rip with, so both the range rule and
the identity rule are stated once.

`expected` is the identity recorded when the job list was built. `None`
means there was no earlier scan to disagree with (an explicit `-t N` on a
disc source scans exactly once), in which case the index IS the only
reference and only the range rule applies.

## needs_pre_mux_title_key

(originally at `src/pipe.rs:3394`)

Whether a disc format needs its per-title key checked BEFORE the mux runs.

AACS and the other non-DVD formats: yes. `decrypt_keys_for_title` does no
drive I/O for them, so the gate is free, and a `None` key means the mux
would write garbage and exit 0 — the failure has to surface here as
`NoDiscKey`.

DVD: no. Its per-title CSS crack happens inside `DiscStream::new`, driven by
`mux_stream`, so pre-cracking here would be a second drive read. Inverted,
this skips the AACS gate entirely and ships that garbage.

## normalize_title_nums

(originally at `src/pipe.rs:3408`)

The `-t` default: with no `-t N` and no `-t all`, rip the MAIN TITLE only.

Pre-1.6 an empty selection meant all-titles, which on an obfuscated disc
(50+ near-equal-length playlists) turned a 40 GB disc into ~200 GB of
near-duplicate MKVs. `-t all` restores that behaviour explicitly and must
therefore be left ALONE here: normalizing it to `[1]` would silently rip one
title from an explicit all-titles request.

This is a real function rather than three lines inside `run()` because a
test that re-states the rule proves nothing about the caller — the previous
one did exactly that, and both mutants of the line in `run()` survived it.

## title_policy

(originally at `src/pipe.rs:3425`)

The two inputs the per-title skip/stop policy runs on.

Returns `(multi_title, explicit_selection)`. Both feed
`freemkv_engine::decide_title`, which is the single source of the skip / stop
/ fail rule shared with autorip and the desktop UI — but the CLI derives its
arguments here, and getting either wrong changes the verdict without
changing the policy:

- `multi_title` widened to `>=` makes every single-title rip look like a
batch, so `decide_title` returns `Skip` instead of `StopFatal`: a hard
failure on the one title the user asked for prints "title skipped" and the
command EXITS 0. That is this crate's documented historical bug.
- `explicit_selection` inverted does the same in the other direction: an
incidental uncrackable menu stub in an all-titles rip aborts the whole run.

## is_feature_title

(originally at `src/pipe.rs:3443`)

Whether this job is the main feature — title index 0, the disc's primary
title, first in every title list in the codebase. A failure there is always
a hard error even in an all-titles rip: the user wants the movie. Inverted,
a failure on the feature itself becomes a skippable extra.

## a_dir_source_must_exist_and_be_a_directory

(originally at `src/pipe.rs:3484`)

`dir://` source validation, which had no test at all.

It exists to stop a mistyped folder from printing "Opening ... OK"
followed by a bare OS error further down. Without a test, deleting the
call in `preflight_validate` restores that behaviour silently — the
three sibling checks around it are covered, this one was not.

## t_all_on_a_disc_expands_to_every_title

(originally at `src/pipe.rs:3676`)

`-t all` on a DISC must expand to every title, exactly as it already
does for an `iso://` source.

It used to reach `build_jobs` with an empty `title_nums`, matching
neither the scanned-source arm (a disc has no upfront title list) nor
the multi-title disc arm (`len() > 1`), so it fell to the catch-all,
built ONE job, and ripped title 1 while exiting 0. There was no reason
for the flag to mean different things on the two source kinds.

## the_t_default_normalizes_to_the_main_title_only

(originally at `src/pipe.rs:3731`)

The `-t` DEFAULT, at the layer that actually applies it.

`run()` normalizes "no `-t` and no `-t all`" to `[1]` — the 1.6.0
change that stopped an obfuscated 50-playlist disc from producing
~200 GB of near-duplicate MKVs. The comment above used to claim the
parse-layer tests covered it; they do not, and the mutation run
confirmed both mutants of that line survive.

## pipefail_classifies_via_the_engine

(originally at `src/pipe.rs:3873`)

`PipeFail::from_mux` classifies the typed `io::Error` `mux_stream` returns
via `libfreemkv::error::is_skippable_title_stub` — NOT an E-code string
match. The two stub codes (E7023 CssKeyMissing, E6008 MkvInvalid) are
skippable; every other libfreemkv error is fatal.

Mutation: swapping to the wrong codes (e.g. matching E6009/NoStreams or
E7022/NoDiscKey) flips one of these asserts and fails.

## metadata_sink_detected_for_chapters_and_json

(originally at `src/pipe.rs:3929`)

`chapters://` / `json://` are metadata sinks. `mux_stream` short-circuits
them BEFORE its header gate (proven in `libfreemkv::mux::driver` tests),
so a metadata export on a title whose video headers never resolve now
succeeds — the CLI's old post-gate short-circuit (and its `headers_resolved`
helper) are deleted, so the bug cannot recur. The CLI keeps this predicate
only to suppress the completion summary for these sinks.

## disc_copy_recovered_data_gates_zero_recovery

(originally at `src/pipe.rs:3957`)

The disc→ISO sweep-success guard. `Disc::copy` returns `Ok` even when the
whole disc was unreadable (`bytes_good == 0`, every ECC block zero-filled
and marked NonTrimmed) — the ISO is all zeroes and unusable. The guard
must report that as NOT recovered (→ caller prints `rip.no_data`, exits
non-zero), never as a "Complete" success.

## fmt_err_renders_codes_to_english

(originally at `src/pipe.rs:3991`)

A representative sample of codes must render to their ENGLISH locale
strings, prefixed with the language-neutral `E<code>` token (WS2: the
code is SHOWN, not stripped). `fmt_err_str` returns the prefix-free-of-
level `E<code> <message>` fragment; the `Error:` level word is added by
the render site (`render_error`).

## key_service_failures_do_not_render_as_a_missing_disc_key

(originally at `src/pipe.rs:4035`)

What the operator actually reads. During a seven-hour key-service outage
the CLI printed

```text
Error: E7022 No key source has a decryption key for this disc (id: 422EB…)
```

which reads as "this disc is not in the key database" — so the operator
went hunting for a VUK when the correct action was to wait. The three
key-service codes must render as their OWN messages here: transient
(7028), credentials (7029), rate limit (7030) — none of them borrowing
E7022's wording, and each naming a different action.

## fmt_err_unknown_code_uses_generic_wrapper

(originally at `src/pipe.rs:4120`)

A code with NO locale entry falls back to the generic wrapper, which
(WS2) still SHOWS the code via `{code} {detail}` rather than swallowing
it — the last resort, not the common path. The `Error:` level word is
added by the render site, not by `fmt_err_str`.

## no_keydb_aacs_disc_surfaces_e7022_in_english

(originally at `src/pipe.rs:4151`)

End-to-end negative-path coverage: when the decrypt gate
(`Disc::ensure_decryptable`, tested in libfreemkv) fires for a no-keydb
AACS disc, `pipe_disc`/`disc_to_iso` surface `Error::NoDiscKey`'s Display
(`E7022[: hash]`). This test pins the CLI-side rendering: that string must
render to the ENGLISH E7022 message via `fmt_err` (so the user never sees
a raw `E7022`) and name the disc by hash. The exit-code wiring is
exercised by `run()` returning `false` on any `pipe_disc` Err.

## disc_damage_unread_is_not_lost

(originally at `src/pipe.rs:4220`)

A healthy in-progress rip (zero read errors, large unread remainder)
must render the clean "no loss" damage string, NOT a "lost" string.
Regression: `print_disc_progress` used to fold `bytes_pending_total`
(which includes not-yet-read NonTried sectors) into the "lost" total,
so an 8%-done rip displayed ~92% of runtime as "lost (in movie)".

## build_key_sources_drops_ssrf_rejected_url

(originally at `src/pipe.rs:4665`)

SSRF guard: a `--key-url` that resolves to an internal / metadata host is
dropped (not added as a source) — `build_key_sources` does not POST key
material there. With keydb present, the keydb remains; url-only yields no
sources at all.

## keydb_does_not_take_the_following_flag_as_its_path

(originally at `src/pipe.rs:4733`)

`--keydb` must not accept the next FLAG as its path.

The guard here only refused a positional URL token (`scheme://`), so
`freemkv --keydb --raw disc:// mkv://out.mkv` set the keydb path to
"--raw" AND dropped the `--raw` the user asked for — a rip that quietly
decrypts when it was told not to, with no message about either. A
missing value is a missing value: say so.

## no_value_flag_takes_the_following_flag_as_its_value

(originally at `src/pipe.rs:4754`)

EVERY value-taking flag must refuse the next FLAG as its value, not
just the one that was reported.

`--keydb` was fixed above and its commit message claimed the rule was
applied to both parsers. It was not: `-t`, `-a` and `-s` here kept the
URL-only guard, and `cli_entry::parse_logging_flags` — in the file that
DEFINES `is_flag_token` — had no guard at all. Fixing these one at a
time is how the hole stayed open, so this asserts the class.

`--key-auth` is deliberately exempt (a bearer token may legitimately
begin with `-`, which `is_flag_token`'s own doc records), and
`--key-url` carries its own http(s) scheme check.

## temp_dest

(originally at `src/pipe.rs:4887`)

A destination URL whose path is unique and outside the source tree.

Preflight's writability probe creates the destination with
`create_new` and removes it again, so a bare relative dest
(`iso://disc.iso`) probes the crate root. Two `cargo test` processes
running against the same checkout then race on that one filename: the
loser's `create_new` returns `AlreadyExists` and the probe reports the
destination as unwritable, failing an assertion about flag validation
for reasons that have nothing to do with flags. Unique absolute paths
keep the probe honest and leave the tree alone.

## dir_dest_existing_file_rejected_even_with_force

(originally at `src/pipe.rs:5065`)

disc:// → dir:// where the dir:// target is an EXISTING REGULAR FILE is
rejected, and `--force` does NOT override it (force only opts into a
non-empty *directory*; it cannot turn a file into a folder). This pins
the `validate_dir_dest` file-branch on the disc-source path, which the
other dir:// gating tests (raw/multipass/byte-stream/non-empty) leave
uncovered.

## raw_and_multipass_require_a_drive_source

(originally at `src/pipe.rs:5239`)

`--raw` / `--multipass` are DRIVE flags, gated on the source being
`disc://` — not on the destination being `iso://`.

The old gate was `dest_is_iso`, which picked the same runs only because
an ISO destination implied a drive source. Now that any image source can
write an `iso://`, that coincidence is gone, and the first assertion here
is the one that would have silently passed under the old predicate:
there is no drive in an `iso:// → iso://` run, so there are no bad
sectors to sweep and nothing to leave encrypted.

## null_dest_multi_title_routes_all_to_sink

(originally at `src/pipe.rs:5269`)

Regression: `null://` on a MULTI-title scanned source must route every
selected title to the bare `null://` sink — NEVER synthesize an invalid
`null://stem_t1.null` (which `parse_url` rejects → output() error, the
old bug). Each emitted job's dest URL must be exactly `null://`, and every
such URL must re-parse to `StreamUrl::Null` (proving it's a valid sink).

## demux_dest_multi_title_urls_carry_scheme

(originally at `src/pipe.rs:5313`)

`demux://` multi-title routing: each title gets its own
`demux://<dir>/t<NN>/` subdir, and (regression) every job URL must carry
the `demux://` scheme so it re-parses to `Demux` — NOT bare `out/tNN/`,
which `parse_url` rejects as Unknown and `output()` then errors on.

## kind_filter_dest_multi_title_urls_carry_own_scheme

(originally at `src/pipe.rs:5340`)

A multi-title `video://` (kind-filter) dest must carry its OWN scheme into
each per-title subdir job — NOT collapse to `demux://`, which would drop
the video-only filter and dump every track. Same guarantee for `audio://`
and `sub://`. (Regression: `demux_jobs` once hardcoded `demux://`.)

## mp4_skip_reasons_render_distinct_resolving_strings

(originally at `src/pipe.rs:5375`)

Every `mp4://` exclusion reason renders a DISTINCT, resolving string. This
locks Fix 1: `SecondaryVideo` (a dependent MVC/3D view) and
`UnmappableVideo` (a primary codec MP4 can't carry) must NOT share a
message, or the main video being dropped is mislabeled "secondary video
view". Mirrors the messaging-contract pattern: enumerate every variant,
assert each key resolves (not the raw dotted sentinel) and all are unique.

## dropped_device_flags_are_unknown

(originally at `src/pipe.rs:5507`)

The dropped `--device` / `-d` flags: the device now comes from the source
URL (`disc:///dev/sgN`). `parse_flags` must reject `--device`/`-d` as
unknown — they are not silently swallowed (which would let a stray device
path leak through as a positional).

## value_flag_set_matches_parser

(originally at `src/pipe.rs:5526`)

The guard `cli_entry::VALUE_FLAGS`' doc comment names, and which did not
exist: the arity table `collect_urls` splits URLs with, and the parser
that actually consumes those values, are two halves of ONE contract and
nothing checked they agreed. They did not — `-k` was listed as
value-taking long after `parse_flags` dropped it (WS3 above), so the CLI
swallowed the token after a flag it then rejected as unknown.

Three properties, none of them derived by calling either side:
1. the table IS the literal list written here, so adding an entry
without teaching the parser is a diff a reviewer sees;
2. every listed flag is one `parse_flags` KNOWS (a bare occurrence is
never "unknown flag") and one that CONSUMES the following token (the
token is never reported as an unknown flag of its own);
3. every boolean flag is absent from the table AND leaves the following
token to be parsed on its own merits.

## retired_value_flags_are_rejected_by_the_parser

(originally at `src/pipe.rs:5612`)

The other half of the arity contract: `cli_entry::RETIRED_VALUE_FLAGS`
exists so a removed flag's VALUE is stepped over rather than counted as
a positional, and that is only correct while the parser still REJECTS
the flag. An entry the parser quietly accepted would be a flag whose
value is discarded before it ever gets there.

## fatal_operation_keys_all_resolve

(originally at `src/pipe.rs:5812`)

Each operation name key the fatal block can use (`op_rip`, `op_info`,
`op_verify`, `op_update_keys`) must resolve to a real localized word, not
the bare dotted key — otherwise the fatal header reads
`Error: error.op_rip failed: ...`.

## line5859

(originally at `src/pipe.rs:5859`)

The decisions that separate "the rip worked" from "the rip did not", pulled
out of the I/O functions they used to live inside.

Every function under test here is reachable in production ONLY through a
physical drive or a disc image, and there is no fixture in CI — which is why
cargo-mutants could replace whole bodies with `Ok(())` / `true` and nothing
failed. The verdicts themselves are pure, so they are tested as verdicts.

## a_completed_but_lossy_mux_names_the_undelivered_stream

(originally at `src/pipe.rs:5890`)

A mux that `completed` can still be LOSSY. `MuxOutcome::undelivered_streams`
names `title.streams` indices the sink accepted frames for but could not put
in the finished container — today the `mp4://` sink dropping an audio track
no frame of which yielded a parseable sample entry. libfreemkv's contract on
the field is explicit: non-empty means the file does NOT match the pre-mux
plan even with `completed = true`, and "a caller that reports a successful
export must report these too — a lossy outcome is never silent."

Unreported, freemkv prints "Complete", exits 0, and the user finds a missing
audio track months later with nothing in the output that ever mentioned it.

## a_completed_mux_that_dropped_payload_bytes_says_how_much_it_lost

(originally at `src/pipe.rs:5937`)

A mux that dropped PAYLOAD BYTES is lossy too, and nothing said so.

`MuxOutcome::lost_bytes`/`errors` are the library's count of bytes it
read and could not carry into the output: an `mkv://` → `mkv://` re-mux
of a Blu-ray 3D rip drops the entire dependent-view (right-eye) payload
into them, and `undelivered_streams` stays EMPTY because no whole stream
was lost. `completed` is true, so `finalize_mux` printed the completion
summary, returned Ok, and the process exited 0 over a file missing one
eye of the film. Both counters were read nowhere outside test fixtures.

## a_truncated_mux_is_a_failure_not_a_success

(originally at `src/pipe.rs:5988`)

EVERY CLI mux funnels its result through `finalize_mux` — every
`iso://`→`mkv://`, every `disc://` title. `completed == false` means an
interrupted or wedged mux left a TRUNCATED file on disk. Forced to the
success arm, the title loop counts it as written and freemkv exits 0 on
a truncated rip: the one failure this crate must never commit.

## copy_verdict_reports_a_halt_and_a_zero_recovery_as_failures

(originally at `src/pipe.rs:6031`)

Both failure verdicts arrive from the engine as `Ok(_)`. A halted sweep
and a whole-disc read failure are equally "the copy function returned
normally", and either one misgraded prints `rip.complete` over an
unusable image and exits 0.

## a_partial_recovery_is_never_graded_as_a_clean_image

(originally at `src/pipe.rs:6066`)

A sweep that LOST sectors is not a clean image, whatever it recovered.

`--multipass` was the only thing that made a `disc:// → iso://` rip
mention its damage: the loss lines sat inside `if multipass`, so an
ordinary single-pass rip of a scratched disc printed "Complete: 24.9 GB"
and exited 0 over an image with holes in it. The verdict is where this
belongs — the same function whose own doc says a misgrade "prints
rip.complete over an unusable image and exits 0, which is the single
failure this crate must never commit".

## a_lossy_copy_reports_its_loss_and_fails_the_exit_code

(originally at `src/pipe.rs:6099`)

Only a whole image is a success, and the loss lines are not a
`--multipass` feature.

Both halves of the same defect: the report was gated on the recovery
STRATEGY the user picked, and the return value was the literal `true`,
so a single-pass rip that lost sectors printed "Complete" and exited 0.

## the_disc_copy_options_honour_raw_multipass_and_progress

(originally at `src/pipe.rs:6141`)

`decrypt: !raw` inverted writes a CIPHERTEXT ISO under the default
flags, silently. A dropped `multipass` accepts `--multipass` and never
recovers. A dropped `progress` leaves a multi-hour sweep printing
nothing at all.

## a_halted_or_holed_extraction_exits_nonzero

(originally at `src/pipe.rs:6170`)

`dir://` extraction hands its bool straight to the process exit code.
`extract_tree` returns `Ok` for a tree written with holes in every file,
so "did it error" is not the question — scripts check `$?` to detect
"extracted but holed" and re-run via ISO multipass.

## a_single_title_rip_never_downgrades_a_failure_to_a_skip

(originally at `src/pipe.rs:6189`)

One job and a named title can never produce the combination that turns a
hard failure into a skip. Widening `multi_title` to `>=` makes every
single-title rip look like a batch, and the engine then answers `Skip`
for a failure on the only title the user asked for — printing "title
skipped" and exiting 0.

## a_requested_language_absent_from_the_title_is_an_error_for_a_single_title

(originally at `src/pipe.rs:6254`)

`-a jpn` against a title that carries only English must not silently
ship a file with no audio at all. Single-title: a hard error. Batch: a
warning, because a library scan over mixed-language discs must not
hard-fail on one title. The whole function could be replaced with
`Ok(())` and nothing noticed — nothing observed the `Err` case.

## every_format_but_dvd_is_key_checked_before_the_mux

(originally at `src/pipe.rs:6318`)

The per-title decrypt gate runs for every format EXCEPT DVD.

For AACS the check is free (`decrypt_keys_for_title` does no drive I/O)
and a missing key means the mux would write garbage and exit 0 — the
comment on the gate says exactly that. For DVD the crack happens inside
`DiscStream::new`, so gating here would be a second drive read.

## aacs_unit_keys_are_forwarded_from_the_scan

(originally at `src/pipe.rs:6402`)

The keys the scan resolved must be the keys the mux is handed.

Returning an empty vec makes every AACS ISO rip fail E7022 — or, with
`--raw`, write ciphertext into a container. Returning a placeholder
`[(0, [0; 16])]` is worse: the mux runs with a WRONG key and produces
garbage. The one test that would have noticed was fixture-gated and
vacuous.

## an_empty_scanned_title_list_still_builds_a_job

(originally at `src/pipe.rs:6448`)

A scanned source whose title list came back EMPTY is not a scanned
source for job-building purposes.

The guard is `Some(t) if !t.is_empty()`. Forced true, an empty list takes
the scanned arm, `(0..0)` yields no indices, and the rip builds ZERO jobs
— the title loop never runs and the command exits 0 having written
nothing. Falling through to the catch-all instead produces one job, which
then reports the real scan failure where the user can see it.

## one_title_into_a_directory_is_still_named_per_title

(originally at `src/pipe.rs:6473`)

One title going to a DIRECTORY is named per-title inside it, not written
to the directory path itself. The guard is
`indices.len() == 1 && !is_dir_dest`; drop either half and a single-title
rip either writes to the bare directory or a multi-title rip collapses
onto one filename.

## an_expanded_disc_selection_refuses_a_single_file_destination

(originally at `src/pipe.rs:6513`)

`-t all` on a disc must be REJECTED against a single-file destination
rather than silently writing twelve titles over one filename. This is the
rejection the `-t all` expansion newly makes reachable, so the shipped
help examples are checked against it elsewhere.

## the_same_file_under_two_spellings_is_recognised

(originally at `src/pipe.rs:6566`)

The same file under two spellings is one file. This is the case the
guard exists for, and the one a path comparison misses.

The detour goes through `..`, deliberately. `Path`'s own `==` normalises
a `.` component away, so a `./Disc.iso` fixture would pass even with the
canonicalize deleted — a test that proves nothing. `..` is NOT
normalised away (it cannot be, without knowing the filesystem), so only
a real canonicalize resolves it.

## the_image_decrypt_path_actually_calls_the_guard

(originally at `src/pipe.rs:6614`)

The guard is actually WIRED, not merely correct.

`image_to_iso` needs a real image and a real destination to run, so no
test can reach the call site — the same gap that let the GUI's four
label seams ship one at a time. Both anchors are `expect`ed rather than
defaulted: an anchor that stopped matching would otherwise silently
widen the slice into a neighbouring function and pass on its text.

## the_preflight_gate_actually_calls_the_guard

(originally at `src/pipe.rs:6863`)

The guard is WIRED into the gate that runs before anything opens.
`preflight_validate` is the only place in `run` that precedes every
drive open, scan and file creation, so a guard anywhere else is a
guard that fires too late.

## a_failed_all_titles_scan_aborts_instead_of_ripping_title_one

(originally at `src/pipe.rs:6959`)

A `-t all` DISC rip whose upfront scan FAILS must abort, never degrade to
ripping title 1 with rc 0.

`resolve_disc_all_titles` is the pure decision behind the scan-failure
arm in `pipe_disc`. `Some(ids)` expands `-t all` to every scanned title
(1..=N) and carries the identities; `None` (the scan failed) is the
caller's cue to print an error and return false. The old code mapped a
failed scan to `(empty title_nums, no identities)`, which fell through
`build_jobs`'s single-title catch-all and ripped title 1 while printing
"Complete" and exiting 0 — the flagship silent-degradation defect, back
on the transient-scan-failure path. `-t all` is `requested == []`, so an
empty request with a successful scan is what expands to the full list.

Mutation caught: making the `None` arm fall back to a title list instead
of aborting (i.e. reintroducing `None => (title_nums, Vec::new())`).

## title

(originally at `src/pipe.rs:6994`)

A title with a distinct playlist and distinct sectors — the shape a real
scan produces. `duration_secs`/`size_bytes` are deliberately IDENTICAL
across every fixture: titles legitimately carry duplicate playlists with
identical duration and size, so neither may be part of the identity.

## a_reordered_rescan_fails_loudly_instead_of_muxing_the_wrong_title

(originally at `src/pipe.rs:7026`)

THE DEFECT. The re-scan returns the same titles in a different order —
a re-read of a marginal disc, a drive that enumerates playlists in a
different order after a retry. Index 1 still resolves, to title 802.
Ripping that under the name the user asked for (`_t2.mkv`) is the silent
wrong-title mux this check exists to prevent.

## every_expanded_job_looks_up_the_identity_of_its_own_title

(originally at `src/pipe.rs:7144`)

The `-t all` expansion and the identity list have to be indexed the same
way, or every job would verify against the WRONG title's identity — a
check that fails on a perfectly stable disc. `-t all` expands to 1-based
title numbers; `build_jobs` stores them 0-based; the identity list is in
scan order.

