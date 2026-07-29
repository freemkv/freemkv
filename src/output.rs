// freemkv — Output writer with verbosity filtering
// MIT — freemkv project
//
// All CLI output goes through this. One filter point for quiet/normal/verbose.
// No `if verbose` scattered through code — tag each line with its level.

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

    /// Print a string from the locale file.
    pub fn print(&self, level: Level, key: &str) {
        if self.level >= level {
            self.line(&strings::get(key));
        }
    }

    /// Print a raw string (not from locale — for computed values like hex, paths).
    pub fn raw(&self, level: Level, text: &str) {
        if self.level >= level {
            self.line(text);
        }
    }

    /// Print raw text without newline.
    pub fn raw_inline(&self, level: Level, text: &str) {
        if self.level >= level {
            if self.stderr {
                eprint!("{}", text);
                let _ = std::io::stderr().flush();
            } else {
                print!("{}", text);
                let _ = std::io::stdout().flush();
            }
        }
    }

    /// Emit one line to the configured channel (stderr when stdout is the data
    /// channel, stdout otherwise).
    fn line(&self, text: &str) {
        if self.stderr {
            eprintln!("{}", text);
        } else {
            println!("{}", text);
        }
    }

    pub fn is_quiet(&self) -> bool {
        self.level == Level::Always
    }

    /// Print a blank line.
    pub fn blank(&self, level: Level) {
        if self.level >= level {
            if self.stderr {
                eprintln!();
            } else {
                println!();
            }
        }
    }
}
