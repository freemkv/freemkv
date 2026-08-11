// freemkv — i18n string loader
// MIT — freemkv project
//
// The loader itself now lives in the shared `freemkv-i18n` crate so `autorip`
// can reuse it. This module is a thin re-export so every existing
// `strings::get(...)` / `strings::fmt(...)` / `strings::init()` call site keeps
// working unchanged. New: `strings::error_message(code)` maps a libfreemkv
// error code to its `error.E<code>` string.
pub use freemkv_i18n::*;

/// Strip control/escape characters from untrusted on-disc metadata (volume
/// label, title name, stream labels) before it is DISPLAYED.
///
/// Lives in `strings` because this is the only module BOTH targets compile:
/// `disc_info` (the CLI's, declared by `main.rs`) needs it, and so does
/// `engine` (declared by `lib.rs`, and by `main.rs` only on macOS). Putting it
/// in `engine` compiled on macOS and broke the Linux CLI build — the shells
/// and the CLI genuinely are separate compilations, and only `strings`
/// spans them.
///
/// Distinct from `engine::sanitize_label`, which makes a label safe as a
/// FILENAME component and is lossier on purpose. This one removes only what
/// cannot be safely rendered, so a Japanese or Cyrillic label survives whole.
pub fn sanitize_display(s: &str) -> String {
    s.chars().filter(|&c| !is_unsafe_display_char(c)).collect()
}

/// Characters to strip before display: C0/C1 controls (including ESC, and the
/// newlines that would let a crafted disc forge a whole log line) AND the
/// Unicode format (Cf) characters `char::is_control()` misses —
/// bidirectional overrides/isolates (U+202A-202E, U+2066-2069), zero-width
/// spaces/joiners (U+200B-200F, U+2060-2064) and the BOM (U+FEFF), which can
/// reorder or hide how the rest of a line renders.
pub fn is_unsafe_display_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}')
}
