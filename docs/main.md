# main dispatcher

Design notes for `src/main.rs`, trimmed out of the source comments to keep
them within the comment-guard's line caps.

## Module: the `freemkv` binary

This binary is two things: the gold-standard `freemkv` CLI (`cli_entry` +
`pipe`/`info`/`disc_info`/… copied verbatim), and the native desktop GUI —
AppKit (`mac`) on macOS, Win32 (`freemkv::windows`) on Windows — over the
shared `ui`/`engine`/`settings` core.

The dispatcher routes a CLI-style invocation (any args, or a bare launch
from a terminal) to the CLI shell — byte-for-byte identical to the old
`freemkv` binary — and a windowed launch (a `.app` double-click, or an
explicit `freemkv gui`) to the desktop shell. The decision itself is
`freemkv::app_entry::wants_gui`, where it can be unit-tested.

**Windows note.** This image is console-subsystem and stays the CLI on a
bare launch, because there is nothing to distinguish an Explorer
double-click from a `cmd` invocation. The double-clickable image on Windows
is the sibling binary `freemkv-gui.exe` (`src/bin/freemkv-gui.rs`), which is
windows-subsystem and opens the same shell directly.

## `is_app_bundle_path`

The whole launch decision reduces to this string test, so it is separated
from `current_exe()` — which cannot be steered from a test — and asserted
directly. Forced `true`, `freemkv --help` in a terminal opens a window
instead of printing; forced `false`, a Finder double-click runs the CLI
with no arguments and exits immediately.

## `launched_windowed` (Windows)

Guessing from parent process or console ownership would make the CLI
contract depend on how the terminal spawned it, which is why this is
hardcoded `false` rather than detected.
