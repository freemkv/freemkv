# `title_identity` — why one definition, and why these three fields

## Why one definition

Both shells re-scan between picking a title and muxing it — the CLI in
`pipe::resolve_scanned_title`, the GUI in `engine::verify_title_identity` /
`engine::remap_against` — and both first grew their OWN answer to "is this
still the same title?". Two answers to one question is how they drifted: one
keyed on the playlist name plus the sectors, the other on the playlist name
plus the DURATION, which cannot tell apart the exact case the project's own
rule warns about ("titles legitimately carry duplicate playlists with
identical duration and size"). This module is the single answer; a second
definition beside a call site is the bug.

Declared by `lib.rs` AND by `main.rs` — the same shape `strings` uses —
because the CLI's `pipe` and the GUI's `engine` are separate compilations and
`engine` is `cfg(target_os = "macos")` in the binary.

## Why these three fields, and no others

- `playlist` — the disc's own name for the title, what `freemkv info` lists.
- `playlist_id` — the numeric form of the same, so a title whose name is
  empty or unprintable still has something to compare and to show.
- `extents` — the SECTORS the title is read from. This is what makes the
  identity safe rather than merely plausible: the sectors are the physical
  identity of the bytes, so two titles carrying the SAME name and identical
  duration/size still differ here unless they read the same sectors — and if
  they read the same sectors from the same playlist, the two rips are
  byte-identical, so there is nothing to confuse.

Deliberately NOT used: the index (that is the bug this exists to catch), and
duration/size — titles legitimately carry duplicate playlists with identical
duration and size, so neither tells two titles apart.
