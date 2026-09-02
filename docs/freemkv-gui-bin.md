# Why `freemkv-gui.exe` is a separate binary

Windows has no `.app` bundle, so "run the desktop app" cannot be a property
of *how* `freemkv.exe` was started — an Explorer double-click and a `cmd`
invocation look identical to the process. It has to be a property of *which
image* was started. Hence two binaries from one crate.

`#![windows_subsystem = "windows"]` is the whole reason this file exists as
a separate binary rather than a flag on the other one: the subsystem is a PE
header field chosen at link time, so it cannot be decided at runtime. Without
it, double-clicking flashes a console window behind the app and leaves it
there for the session.

The shell itself is NOT duplicated here — it lives in the lib
(`freemkv::win_app` → `freemkv::windows`), which is also what `freemkv gui`
calls, so the two entry points can never drift.

Cargo cannot make a `[[bin]]` target-conditional (there is no
`[target.'cfg(windows)'.bin]`), so the *contents* carry the gate: on macOS
and Linux this compiles to an empty `main` and links nothing platform
specific. Only the Windows build produces a real program.
