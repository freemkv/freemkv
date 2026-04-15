# freemkv CLI — Rules

## i18n only — no hardcoded English

All user-facing text comes from `strings.rs` (locale JSON files). Never hardcode English strings in Rust code.

- Use `strings::get("key")` for static strings.
- Use `strings::fmt("key", &[("var", "value")])` for parameterized strings.
- If a string key doesn't exist, add it to `locales/en.json` (and `locales/es.json`).
- Error display: `strings::get(&format!("error.E{}", err.code()))` with fallback to `err.to_string()`.

## Architecture

- **CLI is dumb.** All logic lives in libfreemkv. CLI only handles I/O, display, and flag parsing.
- **PES pipeline.** `pipe()` uses `input()` / `output()` — PES frames flow through.
- **disc.copy() for ISO.** `disc_to_iso()` calls `Disc::copy()`, not a stream.
- **No process::exit in pipe.** Functions return bool/Result. Only `main()` exits.
- **Progress is a CLI concern.** Library returns `DiscTitle.size_bytes`. CLI calculates and displays progress.
