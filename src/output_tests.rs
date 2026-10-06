use super::{Level, Output, write_quietly};

struct BrokenPipe;
impl std::io::Write for BrokenPipe {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::ErrorKind::BrokenPipe.into())
    }
}

/// A closed reader is not a failure of ours: the line is dropped, never a panic.
#[test]
fn a_closed_reader_drops_the_line_instead_of_panicking() {
    write_quietly(&mut BrokenPipe, "x", true);
    write_quietly(&mut BrokenPipe, "x", false);
    let mut buf = Vec::new();
    write_quietly(&mut buf, "x", true);
    assert_eq!(buf, b"x\n");
}

// Exercises the full quiet/normal/verbose 3x3 grid directly; previously
// only one point of it was checked, and only via a whole subprocess.
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
/// `blank` goes through the capture seam like every other line, and still
/// honours the level gate.
#[test]
fn a_blank_line_is_captured_and_gated() {
    let ((), text) = super::capture(|| Output::new(false, false).blank(Level::Normal));
    assert_eq!(text, "\n", "blank() bypassed the capture seam");
    let ((), quiet) = super::capture(|| Output::new(false, true).blank(Level::Normal));
    assert_eq!(quiet, "", "a quiet Output printed a Normal blank");
}
