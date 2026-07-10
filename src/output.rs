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
pub struct Output {
    level: Level,
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
        Output { level }
    }

    /// Print a string from the locale file.
    pub fn print(&self, level: Level, key: &str) {
        if self.level >= level {
            println!("{}", strings::get(key));
        }
    }

    /// Print a raw string (not from locale — for computed values like hex, paths).
    pub fn raw(&self, level: Level, text: &str) {
        if self.level >= level {
            println!("{}", text);
        }
    }

    /// Print raw text without newline.
    pub fn raw_inline(&self, level: Level, text: &str) {
        if self.level >= level {
            print!("{}", text);
            let _ = std::io::stdout().flush();
        }
    }

    pub fn is_quiet(&self) -> bool {
        self.level == Level::Always
    }

    /// Print a blank line.
    pub fn blank(&self, level: Level) {
        if self.level >= level {
            println!();
        }
    }
}
