# `strings::get_or` / `strings::fmt_or`

## Why the English fallback exists

`Cargo.toml` resolves `freemkv-i18n` from a release TAG, so between wiring a
new string in this crate and re-tagging i18n in the release cascade, the key
simply is not in the catalog yet — and `freemkv_i18n::get` answers a miss by
returning the dotted path itself ("makes missing translations visible",
which is right for a translator and wrong for a user). `usage.url.mp4` in
`--help`, or `error.log_level_needs_value` on a mistyped flag, is strictly
worse than untranslated English: it is still not localized AND it is no
longer readable.

## Why it's a single shared helper

This existed twice already, hand-rolled at the call site — `ui::format_label`
and `pipe::title_changed_message` each wrote out the same
`match get(key) { s if s == key => .. }`. A second definition beside a call
site is how the CLI and the GUI grew different answers to one question in
the first place, so there is one now and both of those call it.

## Why the cleanup isn't enforced by a test

The fallback is a stopgap by construction: once the key ships in a tagged
catalog, the `english` argument stops being reachable and the call can
collapse to a plain `get`. Nothing enforces that cleanup, deliberately — a
test that failed the moment i18n was re-tagged would block the release
cascade it is meant to follow. What IS enforced is that the user never sees
a dotted path: the `usage_en` / `usage_de` / `usage_fr` goldens capture the
whole help text verbatim, so a key with a typo in it and a missing fallback
both show up as a golden diff.

## `fmt_or`

`fmt_or` is `get_or` plus `{placeholder}` substitution — the fallback half of
`fmt`. The substitution runs on whichever text won, so the English stopgap
and the translation take the same arguments and cannot drift.
