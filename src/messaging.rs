// freemkv — messaging standard (Level + Code + Message)
// MIT — freemkv project
//
// WS2: every user-visible error line renders as `Error: E<code> <message>`.
// The three-level vocabulary is closed here: `Warn`/`Info` are the
// tracing-log levels (file sink only), `Error` is the terminal failure
// render. Every libfreemkv `Error` variant is a terminal-renderable failure,
// so `level_for` returns `Error` for every current code.
//
// This is the single authority for the Level↔code mapping. The CLI render
// path (`pipe::render_error` / `main::fatal`), the contract test
// (`tests/messaging_contract.rs`), and the docs Codes page generator all read
// `level_for` — do NOT scatter level logic.

/// The closed set of message levels. `Warn`/`Info` belong to the tracing-log
/// channel (file sink); `Error` is the terminal failure render.
///
/// `Warn`/`Info` are not constructed in the CLI binary today — only the
/// terminal `Error` render path is (`pipe::render_error`, `main::fatal`). They
/// exist so the level vocabulary is CLOSED: the contract test and the docs
/// Codes page generator read the full set, and a future "this code is a
/// warning" decision has a typed home. Hence the targeted dead-code allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Level {
    Warn,
    Info,
    Error,
}

impl Level {
    /// The translatable locale key for this level's display word.
    pub fn locale_key(self) -> &'static str {
        match self {
            Level::Warn => "error.level_warn",
            Level::Info => "error.level_info",
            Level::Error => "error.level_error",
        }
    }
}

/// The Level for a libfreemkv error code. Every current code is a terminal
/// failure, so this is uniformly `Error`. It is a function (not a constant) so
/// a later wave can special-case a code to `Warn`/`Info` without touching any
/// call site.
///
/// Consumed by the contract test (`tests/messaging_contract.rs`) and the docs
/// Codes page generator; the CLI render path uses the fixed `Error` level
/// directly, so `level_for` is dead in the binary build alone — keep it as the
/// single authority for the map rather than inlining `Error` at call sites.
#[allow(dead_code)]
pub fn level_for(_code: u16) -> Level {
    Level::Error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_is_error_level() {
        // Locked: every libfreemkv code is a terminal failure render.
        for code in [1000u16, 2000, 3000, 5000, 6009, 7022, 8001, 9023] {
            assert_eq!(level_for(code), Level::Error);
        }
    }

    #[test]
    fn level_locale_keys_are_distinct() {
        assert_ne!(Level::Warn.locale_key(), Level::Info.locale_key());
        assert_ne!(Level::Info.locale_key(), Level::Error.locale_key());
        assert_ne!(Level::Warn.locale_key(), Level::Error.locale_key());
    }
}
