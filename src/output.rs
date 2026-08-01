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

    /// Whether a line tagged `level` prints at the configured verbosity.
    ///
    /// THE verbosity gate — `print`, `raw`, `raw_inline` and `blank` all
    /// delegate here rather than each re-stating `self.level >= level`, so
    /// there is one comparison to get right instead of four. `--quiet` is a
    /// contract (a `stdio://` rip pipes the data on stdout and must not have
    /// log lines interleaved), not a preference.
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
        if self.should_print(level) {
            if self.stderr {
                eprintln!();
            } else {
                println!();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Level, Output};

    /// `output.rs` is not formatting — it is the gate that decides whether a
    /// line prints at all, which is the quiet/normal/verbose contract. It had
    /// no tests of its own; the only external check exercised ONE point of the
    /// 3×3 grid through a whole subprocess.
    #[test]
    fn the_verbosity_grid_is_exactly_configured_greater_or_equal_to_line() {
        // (configured Output, line Level, prints?)
        let quiet = Output::new(false, true);
        let normal = Output::new(false, false);
        let verbose = Output::new(true, false);

        let grid: &[(&Output, Level, bool)] = &[
            (&quiet, Level::Always, true),
            (&quiet, Level::Normal, false),
            (&quiet, Level::Verbose, false),
            (&normal, Level::Always, true),
            (&normal, Level::Normal, true),
            (&normal, Level::Verbose, false),
            (&verbose, Level::Always, true),
            (&verbose, Level::Normal, true),
            (&verbose, Level::Verbose, true),
        ];
        for (out, line, want) in grid {
            assert_eq!(
                out.should_print(*line),
                *want,
                "level {} / line {} should print = {want}",
                out.is_quiet(),
                matches!(line, Level::Always)
            );
        }
    }

    /// Quiet is exactly the Always-only level, and quiet WINS over verbose when
    /// both flags are given — otherwise `--quiet --verbose` on a `stdio://`
    /// rip interleaves log text into the piped byte stream.
    #[test]
    fn quiet_is_the_always_only_level_and_beats_verbose() {
        assert!(Output::new(false, true).is_quiet());
        assert!(
            Output::new(true, true).is_quiet(),
            "quiet must win over verbose"
        );
        assert!(!Output::new(false, false).is_quiet());
        assert!(!Output::new(true, false).is_quiet());

        // Quiet still emits Always lines — results and errors are never hidden.
        assert!(Output::new(false, true).should_print(Level::Always));
    }

    /// Routing to stderr must not change WHAT prints, only where. A gate that
    /// consulted `stderr` would silence a `stdio://` rip's error reporting.
    #[test]
    fn the_stderr_route_does_not_change_the_gate() {
        for (v, q) in [(false, false), (true, false), (false, true)] {
            let out = Output::new(v, q);
            let piped = Output::new(v, q).to_stderr();
            for line in [Level::Always, Level::Normal, Level::Verbose] {
                assert_eq!(out.should_print(line), piped.should_print(line));
            }
        }
    }
}
