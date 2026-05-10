//! Terminal styling for CLI output.
//!
//! Raw ANSI escape codes — no dependency. Auto-disables when stdout
//! isn't a TTY (so `freemkv | jq` stays clean) or when the user has
//! `NO_COLOR` set (<https://no-color.org>). On Windows, attempts to
//! enable virtual-terminal processing on the console handles before
//! emitting any escapes — if the call fails (legacy `cmd.exe`),
//! styling stays off so the user doesn't see literal `\x1b[...` in
//! their console. Detection runs once lazily on first use.
//!
//! Styling is a CLI concern — `libfreemkv` emits raw text, the CLI
//! decorates. The palette mirrors `freemkv.org` so the marketing
//! image and the actual terminal output agree.

use std::io::IsTerminal;
use std::sync::OnceLock;

static USE_COLOR: OnceLock<bool> = OnceLock::new();

fn on() -> bool {
    *USE_COLOR.get_or_init(|| {
        let no_color = std::env::var_os("NO_COLOR")
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        if no_color {
            return false;
        }
        if !std::io::stdout().is_terminal() {
            return false;
        }
        try_enable_vt()
    })
}

#[cfg(windows)]
fn try_enable_vt() -> bool {
    // Enable ENABLE_VIRTUAL_TERMINAL_PROCESSING on stdout AND stderr.
    // Modern Windows 10+ consoles support it; legacy hosts (older
    // cmd.exe configurations) return zero from SetConsoleMode and we
    // fall back to plaintext.
    //
    // Raw FFI keeps freemkv dep-free; the only Win32 calls we need are
    // GetStdHandle / GetConsoleMode / SetConsoleMode. Std handles are
    // negative magic numbers per the Win32 ABI.
    type Handle = *mut std::ffi::c_void;
    const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
    const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    const INVALID_HANDLE: isize = -1;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> Handle;
        fn GetConsoleMode(h: Handle, mode: *mut u32) -> i32;
        fn SetConsoleMode(h: Handle, mode: u32) -> i32;
    }
    fn enable(which: u32) -> bool {
        unsafe {
            let h = GetStdHandle(which);
            if h.is_null() || h as isize == INVALID_HANDLE {
                return false;
            }
            let mut mode = 0u32;
            if GetConsoleMode(h, &mut mode) == 0 {
                return false;
            }
            SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }
    enable(STD_OUTPUT_HANDLE) && enable(STD_ERROR_HANDLE)
}

#[cfg(not(windows))]
fn try_enable_vt() -> bool {
    true
}

// Palette — matches freemkv.org CSS variables.
//   teal       #0D9488  filled progress bar
//   teal-light #14B8A6  prompts / highlight / percentage / section headers
//   green      #22c55e  success markers (OK)
//   gray       #737373  dim text (version banners, secondary hints)
const TEAL: (u8, u8, u8) = (13, 148, 136);
const TEAL_LIGHT: (u8, u8, u8) = (20, 184, 166);
const GREEN: (u8, u8, u8) = (34, 197, 94);
const GRAY: (u8, u8, u8) = (115, 115, 115);

fn paint(rgb: (u8, u8, u8), s: &str) -> String {
    if !on() {
        return s.to_string();
    }
    format!("\x1b[38;2;{};{};{}m{}\x1b[0m", rgb.0, rgb.1, rgb.2, s)
}

/// Teal — disc titles, percentages, anything we want to draw the eye to.
pub fn hl(s: &str) -> String {
    paint(TEAL_LIGHT, s)
}

/// Gray — version banners, secondary information the reader can skim.
pub fn dim(s: &str) -> String {
    paint(GRAY, s)
}

/// Green — success markers ("OK", "Done"). Used for the "Opening
/// disc:///dev/sg1...OK" / "Opening null://...OK" status suffixes.
pub fn ok(s: &str) -> String {
    paint(GREEN, s)
}

/// Unicode-block progress bar with teal fill, gray empty. ASCII
/// fallback (`[====----]`) when colors are disabled, so piped output
/// or legacy consoles see something readable rather than UTF-8 blocks
/// that may not render. `frac` is clamped to `[0.0, 1.0]`.
pub fn bar(width: usize, frac: f64) -> String {
    let frac = frac.clamp(0.0, 1.0);
    let filled = ((frac * width as f64).round() as usize).min(width);
    let empty = width - filled;
    if on() {
        let f = "█".repeat(filled);
        let e = "░".repeat(empty);
        format!(
            "[\x1b[38;2;{};{};{}m{}\x1b[0m\x1b[38;2;{};{};{}m{}\x1b[0m]",
            TEAL.0, TEAL.1, TEAL.2, f, GRAY.0, GRAY.1, GRAY.2, e
        )
    } else {
        format!("[{}{}]", "=".repeat(filled), "-".repeat(empty))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_zero_is_all_empty() {
        let _ = USE_COLOR.set(false);
        assert_eq!(bar(4, 0.0), "[----]");
    }

    #[test]
    fn bar_full_is_all_filled() {
        let _ = USE_COLOR.set(false);
        assert_eq!(bar(4, 1.0), "[====]");
    }

    #[test]
    fn bar_clamps_above_one() {
        let _ = USE_COLOR.set(false);
        assert_eq!(bar(4, 1.5), "[====]");
    }

    #[test]
    fn bar_half() {
        let _ = USE_COLOR.set(false);
        assert_eq!(bar(4, 0.5), "[==--]");
    }

    #[test]
    fn helpers_pass_text_through_when_disabled() {
        let _ = USE_COLOR.set(false);
        assert_eq!(hl("Dune"), "Dune");
        assert_eq!(dim("freemkv 0.18.4"), "freemkv 0.18.4");
    }
}
