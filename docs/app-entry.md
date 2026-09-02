# app_entry launch decision

Design notes for `src/app_entry.rs`, trimmed out of the source comments to
keep them within the comment-guard's line caps.

## Module: portable launch routing

This module is deliberately portable and Win32/AppKit-free: it deals in
`&str` and closures, never in a platform handle. That is what lets both
shells share it *and* lets the routing be unit-tested on any machine —
`wants_gui` is a pure function of the argument vector, which is the only
part of the launch path that can be wrong without a window to look at.

It also keeps the `settings`/`engine` "must build anywhere" rule intact: no
type from a platform shell crosses this boundary. Callers pass the two
settings *values* (`language`, `log_level`) rather than a `Settings`, so the
binary and the lib never have to agree on a struct identity.

## `wants_gui`

Two ways in, and only two:

* an explicit `freemkv gui` (works on every desktop platform), or
* a bare launch of a *windowed* image — a macOS `.app` double-click, which
  passes no arguments. `windowed` is the caller's answer to "was I started
  as a window?"; on Windows the answer for `freemkv.exe` is always `false`,
  because the windowed image there is the separate `freemkv-gui.exe`, which
  does not route through here at all.

A bare launch from a terminal therefore falls through to the CLI, so
`freemkv` alone still prints usage and exits 2 — the CLI contract, intact.

## `GUI_LOG_CAP_BYTES`

`rolling::never` appends and never rotates, so without this cap the file is
bounded by nothing but how long the user leaves Verbose/Debug on — every
rip of every session, forever, in a directory they never look in.

## `trim_oversized_log`

This bounds growth ACROSS sessions, which is the unbounded part: a single
session is still limited only by its own length, and `tracing-appender`
offers no size-triggered rotation to fix that without hand-rolling a
`Write` wrapper. The log is a debugging aid the user opted into and can
delete, so the cheap bound is the proportionate one.

Best-effort by design: a log that cannot be removed must not stop the app
from starting, or from logging.

## Test: `a_diagnostic_log_past_the_cap_is_started_over_not_appended_to`

`rolling::never` appends and never rotates, and nothing else truncated the
file, so leaving Verbose/Debug on accumulated every line of every rip of
every session in the app-support dir indefinitely.
