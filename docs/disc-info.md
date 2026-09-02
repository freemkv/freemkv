# disc_info: encryption-line rendering

## `emit_encryption_line`

Emits the one-line encryption/generation label ("AACS 2.0 encrypted", "CSS
encrypted", …) for a scanned disc, returning whether a line was printed (so a
caller can follow it with a blank). Shared by the drive (`disc://`) and
keyless ISO (`iso://`) info paths so the generation line renders identically
for both — one renderer, no duplicated match.

The generation label is the informative part (a non-translated proper noun).
The word "encrypted" is app-layer English here rather than the i18n table,
deliberately: localizing it would require adding a string to freemkv-i18n (a
versioned crate we keep frozen), and a stale i18n pin would then print the
raw key.
