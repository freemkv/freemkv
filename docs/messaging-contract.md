# messaging_contract.rs — rationale notes

Long-form context for comments in `tests/messaging_contract.rs` that would
otherwise blow the comment-guard's per-block caps. Each section below is
pointed to by a short `//` comment at the corresponding call site.

## `codes_carried_by_a_variant`

The distinction between "declared" and "carried by a variant" is real and
load-bearing. A constant with no `code()` arm is a code libfreemkv emits by
NUMBER — into a log line, an accounting counter, a bug report — without ever
building an `Error` for it. E6019 (`E_UDF_NO_USABLE_EXTENT`) is documented
upstream as exactly that: "consumed by the caller, not returned to it". The
fixture below cannot enumerate such a code (there is no variant to construct)
and must not be asked to, but en.json absolutely should still carry a
sentence for it — a number a user can read is a number a user can be
confused by.

## `en_json_has_no_string_for_a_code_libfreemkv_does_not_declare`

Catches a stale `error.E*` string left in en.json after its code was
retired — the mutation being: delete a constant from libfreemkv's
`error.rs` and leave its message behind, where 29 locales' worth of
translators go on maintaining a sentence no build can ever print.

THIS TEST USED TO ANSWER A DIFFERENT QUESTION. It compared en.json against
`all_error_variants()` and called anything else an orphan — reading the
fixture as "the set of codes that may legitimately have a string" while
`every_variant_has_code_message_locales_placeholders_and_level` reads the
same list as "the set that must have one". Two opposite readings of one
hand-maintained list, and the list was missing eleven codes. So the first
reading hid a real gap for releases — E9056/E9057, the codes that tell a
user the rip could NOT be confirmed written to disk, reached them as the
literal text `error.E9056` while they decided whether to delete the source
— and the second reading then reported the party who FIXED that gap as
having introduced eleven stale strings. It failed in whichever direction
the fixture was wrong, and it was always the fixture that was wrong.

It asks libfreemkv now. An `error.E*` key is legitimate if libfreemkv
DECLARES that code — not if this crate happens to construct it. Three of
the eleven (E9019/E9024/E9025) are `dir://` flag gates that `pipe.rs`
intercepts in preflight with its own strings, so no CLI path will ever
render them; they are still `pub` codes of a library with other consumers,
and "unreachable from `pipe.rs`" is not grounds to delete a message. Only
"no longer a code" is, and that is what this now tests.

## `fixture_enumerates_every_error_code_a_variant_can_carry`

Every code libfreemkv can put inside an `Error` must appear in
`all_error_variants()`, and the fixture must claim no code that libfreemkv
has retired.

The mutation this catches: add an error variant to libfreemkv, ship it, and
forget this fixture. A forgotten variant is not merely untested here — it
is INVISIBLE. Every other assertion in this file iterates the fixture, so a
variant absent from it has no English-string requirement, no locale-parity
requirement, no placeholder requirement and no Level requirement, and the
first person to notice is a user reading `error.E9056` off their terminal.
That has now happened three times (E6013/E6014, E9059-E9070, and the eleven
enumerated at the end of `src/test_support.rs`), each time found by someone
auditing a different crate for an unrelated reason.

Codes with no `code()` arm are excluded by construction — see
`codes_carried_by_a_variant`. Their strings are still guarded, from the
other side, by `en_json_has_no_string_for_a_code_libfreemkv_does_not_declare`
above and by freemkv-i18n's own generated-list tests.
