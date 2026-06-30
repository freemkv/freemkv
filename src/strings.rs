// freemkv — i18n string loader
// AGPL-3.0 — freemkv project
//
// The loader itself now lives in the shared `freemkv-i18n` crate so `autorip`
// can reuse it. This module is a thin re-export so every existing
// `strings::get(...)` / `strings::fmt(...)` / `strings::init()` call site keeps
// working unchanged. New: `strings::error_message(code)` maps a libfreemkv
// error code to its `error.E<code>` string.
pub use freemkv_i18n::*;
