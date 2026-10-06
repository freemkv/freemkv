// freemkv — Output writer with verbosity filtering
// MIT — freemkv project

// All CLI output goes through this: one filter point, tag each line's level.

use crate::strings;
use std::io::Write;

/// Verbosity level attached to each line of output.
///
/// A line prints when the configured [`Output`] level is greater than or equal
/// to the line's level, so the variants are ordered from lowest to highest. A
/// line tagged with a level prints at that verbosity and every higher one.
#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub enum Level {
    /// Always shown — prints at every verbosity, including quiet. Suppresses
    /// nothing. Use for results and errors the user must always see.
    Always,
    /// Normal output — shown at normal and verbose, suppressed when quiet.
    Normal,
    /// Verbose output — shown only when verbose; suppressed at normal and quiet.
    Verbose,
}

// Write `text` and flush, dropping any error. `println!` panics when the reader has gone
// (`freemkv info ... | head -1`: Rust ignores SIGPIPE), which would abort a rip mid-mux.
fn write_quietly(w: &mut impl Write, text: &str, newline: bool) {
    let _ = w.write_all(text.as_bytes());
    if newline {
        let _ = w.write_all(b"\n");
    }
    let _ = w.flush();
}

/// Single filter point for all CLI output.
///
/// Holds the configured verbosity; each `print`/`raw`/`blank` call passes the
/// [`Level`] of that line and is emitted only when the configured level is high
/// enough.
#[derive(Clone, Copy)]
pub struct Output {
    level: Level,
    /// Route all human-facing text to stderr instead of stdout. Set when stdout
    /// IS the data channel (a `stdio://` destination), so logs never corrupt the
    /// piped stream.
    stderr: bool,
}

impl Output {
    /// Build an `Output` from the `--verbose` and `--quiet` flags.
    ///
    /// Quiet wins over verbose when both flags are set: the result is the quiet
    /// level (only [`Level::Always`] lines print).
    pub fn new(verbose: bool, quiet: bool) -> Self {
        let level = if quiet {
            Level::Always
        } else if verbose {
            Level::Verbose
        } else {
            Level::Normal
        };
        Output {
            level,
            stderr: false,
        }
    }

    /// Route all output to stderr instead of stdout. Use when stdout is the data
    /// channel (a `stdio://` destination) so logs never corrupt the piped stream.
    pub fn to_stderr(mut self) -> Self {
        self.stderr = true;
        self
    }

    // Whether a line tagged `level` prints at the configured verbosity.
    // THE single verbosity gate — `print`, `raw`, `raw_inline` and `blank` all
    // delegate here instead of each re-stating `self.level >= level`.
    pub(crate) fn should_print(&self, level: Level) -> bool {
        self.level >= level
    }

    /// Print a string from the locale file.
    pub fn print(&self, level: Level, key: &str) {
        if self.should_print(level) {
            self.line(&strings::get(key));
        }
    }

    /// Print a raw string (not from locale — for computed values like hex, paths).
    pub fn raw(&self, level: Level, text: &str) {
        if self.should_print(level) {
            self.line(text);
        }
    }

    /// Print raw text without newline.
    pub fn raw_inline(&self, level: Level, text: &str) {
        if self.should_print(level) {
            if intercept(text, false) {
                return;
            }
            if self.stderr {
                write_quietly(&mut std::io::stderr().lock(), text, false);
            } else {
                write_quietly(&mut std::io::stdout().lock(), text, false);
            }
        }
    }

    /// Emit one line to the configured channel (stderr when stdout is the data
    /// channel, stdout otherwise).
    fn line(&self, text: &str) {
        if intercept(text, true) {
            return;
        }
        if self.stderr {
            write_quietly(&mut std::io::stderr().lock(), text, true);
        } else {
            write_quietly(&mut std::io::stdout().lock(), text, true);
        }
    }

    pub fn is_quiet(&self) -> bool {
        self.level == Level::Always
    }

    /// Print a blank line.
    pub fn blank(&self, level: Level) {
        if self.should_print(level) {
            // Route through the same seam as every other line (`line`), so a
            // blank is captured/diverted by the test intercept identically — a
            // direct `println!()` here wrote straight past it.
            self.line("");
        }
    }
}

// ── test seam ────────────────────────────────────────────────────────────────
// Lines leave via `line`/`raw_inline`. Libtest's capture is thread-local and
// unreadable from stable Rust, so this captures at the terminal boundary.

#[cfg(test)]
thread_local! {
    /// `Some` while [`capture`] is running on this thread; collects the lines
    /// instead of printing them. Thread-local, so tests running in parallel
    /// cannot capture each other's output.
    static CAPTURED: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with this thread's `Output` text collected instead of printed, and
/// return it alongside `f`'s value. Test-only.
#[cfg(test)]
pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, String) {
    CAPTURED.with(|c| *c.borrow_mut() = Some(String::new()));
    let value = f();
    let text = CAPTURED.with(|c| c.borrow_mut().take()).unwrap_or_default();
    (value, text)
}

/// Divert `text` into the active capture, if any. Returns whether it was
/// diverted (and so must not also be printed).
#[cfg(test)]
fn intercept(text: &str, newline: bool) -> bool {
    CAPTURED.with(|c| match c.borrow_mut().as_mut() {
        Some(buf) => {
            buf.push_str(text);
            if newline {
                buf.push('\n');
            }
            true
        }
        None => false,
    })
}

/// Production build: nothing is ever intercepted.
#[cfg(not(test))]
fn intercept(_text: &str, _newline: bool) -> bool {
    false
}

#[cfg(test)]
#[path = "output_tests.rs"]
mod tests;
