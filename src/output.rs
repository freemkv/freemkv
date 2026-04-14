// freemkv — Output writer with verbosity filtering
// AGPL-3.0 — freemkv project
//
// All CLI output goes through this. One filter point for quiet/normal/verbose.
// No `if verbose` scattered through code — tag each line with its level.

use crate::strings;
use std::io::Write;

#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub enum Level {
    Quiet,
    Normal,
    Verbose,
}

pub struct Output {
    level: Level,
}

impl Output {
    pub fn new(verbose: bool, quiet: bool) -> Self {
        let level = if quiet {
            Level::Quiet
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

    /// Print a formatted string from the locale file with {key} placeholders.
    pub fn fmt(&self, level: Level, key: &str, args: &[(&str, &str)]) {
        if self.level >= level {
            println!("{}", strings::fmt(key, args));
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
        self.level == Level::Quiet
    }

    /// Print a blank line.
    pub fn blank(&self, level: Level) {
        if self.level >= level {
            println!();
        }
    }
}
