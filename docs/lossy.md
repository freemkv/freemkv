# `lossy` module — rationale

## Why this module exists

`completed = true` is not "the file is what you asked for". `MuxOutcome`
carries two independent losses alongside it, and a run can have either with
`completed` set:

- `undelivered_streams` — whole tracks the sink accepted frames for and
  could not put in the finished container (today the `mp4://` sink dropping
  an audio track with no parseable sample entry);
- `errors` / `lost_bytes` — bytes the library READ and could not carry. An
  `mkv://` → `mkv://` re-mux of a Blu-ray 3D rip drops the entire
  dependent-view (right-eye) payload into these, with `undelivered_streams`
  EMPTY, because no whole stream was lost.

The second pair was read nowhere outside test fixtures: the CLI printed
"Complete" and exited 0, and the GUI said "Written to <dir>", over a file
missing one eye of the film. That is the exact "data loss looks like
success" shape this project forbids.

Declared by both crate roots (the `title_identity` / `file_identity`
pattern) so the CLI's `pipe` and the GUI's `engine` render one answer. They
had two before — `print_undelivered_streams` and `undelivered_lines`, each
covering only half of what the outcome carried.

## `lossy_lines` — why a warning, not a failure

**A warning on a still-successful rip, not a failure.** The file is
finalised, structurally valid and playable; it is missing part of its
content, not truncated. Escalating would abandon the title — and, through
the batch policy both shells share, whole multi-title runs — and the GUI's
per-title failure arm DELETES the output file, which would destroy the very
bytes this warning exists to preserve. Loss is reported at `Level::Always`
on the CLI side so `--quiet` cannot hide it.

**Existing strings only.** The locale catalogs live in a separate, pinned
crate, so a new key cannot ship from here and an English-only one would ship
English into 28 other languages. `mp4.excluded_header` already says
"track(s) can't be stored in an MP4 and will be left out — use `mkv://` to
keep everything"; `dir.file_lossy` already says "  {file}: {lost} MB lost"
and is used for exactly this — per-file byte loss — on the extraction path.

## `is_lossy` — the `#[allow(dead_code)]`

The LIBRARY target always exercises this (`lib.rs` re-exports the module, so
nothing in it is dead there). The BIN target's reachability depends on which
shell that platform compiles, and the `cfg` attribute got narrowed twice and
got it wrong twice — first excluding macOS and Windows, which left it dead
on Linux, then including Windows, where the Windows CI compiler promptly
reported it dead anyway. A plain allow is the honest form: the condition
being expressed is "whichever platforms happen to route through
`summarize_stream`", which is not a property worth encoding in an
attribute, and encoding it wrongly is worse than not encoding it.

## `lost_mb` — rounding

Rounding to nearest would render a real loss of a few hundred KB as
"0.00 MB lost" — a line that says nothing was lost, on the one code path
whose entire purpose is to say something was. Rounding up keeps the number
honest in the only direction that matters here, and any non-zero loss
therefore shows at least 0.01.
