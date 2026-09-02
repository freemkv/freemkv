# cli_entry.rs — design notes

Long-form rationale moved out of `src/cli_entry.rs` doc comments to satisfy
the comment-guard's line caps. Each section below is pointed to by a short
`//` comment at the relevant spot in the source.

## Tracing / logging channels

Two-channel design: the **terminal** (Channel 1) is always clean — curated
progress, status, and the final result block only. Zero `tracing`
DEBUG/TRACE (or any tracing level) ever reaches the terminal. Tracing is a
diagnostic stream that only exists when the user explicitly asks for it,
and it goes to a **file** (Channel 2), never stdout/stderr.

A file log is written only when one of these is set:
  * `--log-level N` — N maps 1→warn, 2→info, 3→debug, 4→trace for the
    `freemkv` / `libfreemkv` targets (everything else stays at error).
  * `--log-file PATH` — write to PATH (default level 3/debug if
    `--log-level` is absent, so a lone `--log-file` still captures useful
    detail).
  * `RUST_LOG` — power-user override of the filter; still file-only.

With none of these set, no subscriber is installed at all: the library's
`tracing` events are dropped and the terminal stays pristine. The file
destination defaults to `./log.txt`; ANSI is off and timestamps are on so
the log is clean and copy-pasteable for a bug report.

The subscriber is installed first thing in `run()`, before any tracing
event can be emitted; only the human-facing complaints about bad flag
values are deferred (see `PendingDiag` below), because printing before the
locale is resolved would hard-code English.

## `PendingDiag` — startup diagnostics that can't render yet

Everything `run()` does before `strings::init()` is trapped between two
hard constraints:

  * `--language` has to be read out of argv before the catalog is chosen,
    because it is what chooses it; and
  * `freemkv_i18n::get` lazily installs the environment-derived catalog on
    its first call, after which `set_language` refuses to change it — it
    `debug_assert!`s, prints "warning: --language ignored", and returns.

So a `strings::get(..)` anywhere in the argv pre-pass does not localize the
message: it silently disables `--language` for the whole process. The old
code took the other horn and hard-coded English. Deciding and rendering
are separate steps, so the pre-pass now decides and records a
`PendingDiag`, and `run()` renders it once the catalog is up — same
messages, same order, now in the user's language.

`PendingDiag` owns its arguments (`String`, not `&str`) because the argv
slice these are derived from is rebuilt by `strip_language_flag` before
they are printed.

`render()` is separate from `emit()` so a test can read the localized text
directly: the whole point of deferring is that these go through the
catalog, and a key with a typo in it renders as the literal dotted path —
exactly as untranslated as the hard-coded English this replaced, only less
readable. `emit()` itself must not be called before `strings::init()`.

## `parse_logging_flags`

Split from `init_logging` because that function installs a
PROCESS-GLOBAL `tracing_subscriber` — it can only run once per test
binary, so nothing ever called it and every one of its parse decisions was
unconstrained.

Returns `(level, file, diagnostics)`. An out-of-range or non-numeric
`--log-level` is reported and IGNORED rather than silently clamped, so a
typo does not quietly hand the user a different verbosity than they asked
for. The reports are RETURNED rather than printed: this runs before the
locale is resolved, and printing here would mean hard-coded English — see
the `PendingDiag` section above for why touching `strings` any earlier is
worse than that.

## `strip_language_flag`

Returns the remaining arguments and the requested language code. The value
guard is the same one `collect_urls` applies, and for the same reason: a
value-flag must not swallow a following positional stream URL.
`freemkv --language disc:// mkv://out.mkv` would otherwise eat `disc://` as
the "language", leaving a single URL that silently degrades into an
info/usage no-op. A leading `-` means the value is missing too, not a
language code.

Extracted from `run()`'s inline loop, which no `cargo test` invocation ever
reached — this is exactly the "add a flag, forget the test" shape the
`collect_urls` comments warn about, applied to the one flag that never got
the same treatment.

## `SUBCOMMANDS`

Every word the dispatcher matches `args[1]` against. Note "gui" is NOT
matched in `run`: `app_entry::wants_gui` intercepts it before the CLI
shell is reached at all, so looking for it in `run`'s match arms finds
nothing and makes this list look stale when it is correct. Anything not
here falls through to the source→destination URL grammar, so a string
that tells the user to run `freemkv <word>` for some other `<word>` is
telling them to run a command that does not exist — which is exactly how
`drive-info`, `disc-info`, `remux` and `verify` shipped in the catalogues.
Kept in step with the match arms by `every_command_named_in_a_locale_exists`.

## `fatal`

This is the single terminal-facing error path (Channel 1). It prints a
clean, localized block — never a raw error code, never a tracing event:

```text
✗ <operation> failed: <clean cause>.
  For a diagnostic log, re-run with --log-level 3 (writes ./log.txt).
```

`op_key` is a locale key for the operation name (`error.op_rip`, etc.);
`cause` is the already-localized, human-readable cause (typically from
`crate::pipe::fmt_err`, which renders `E<code>` → a plain-English message
with its own remediation). The diagnostic-log hint tells the user how to
capture a file log for a bug report — without ever spilling tracing onto
the terminal by default. The block goes to STDERR so stdout stays
pipe-clean for `mkv://`/`m2ts://` streaming; the leading mark is ANSI-free
when stderr is redirected.

## `VALUE_FLAGS`

A value-flag normally consumes the following token as its value, but it
must NOT swallow a positional stream URL (`scheme://...`): `freemkv
--keydb disc:// mkv://out.mkv` would otherwise let `--keydb` eat `disc://`,
leaving a single URL that silently routes to `info` instead of ripping. So
if a value-flag is followed by a URL token, the URL is kept as positional
and the flag's value is treated as absent (`crate::pipe::run` then reports
the missing value).

This is the ONE source of truth for flag arity, shared by `collect_urls`
and asserted against `parse_flags` by
`pipe::tests::value_flag_set_matches_parser` — so adding a value-flag to
the parser without listing it here fails a test rather than silently
mis-parsing (the `-a`/`-s` bug). Boolean flags (`--raw`, `--multipass`,
`-q`) are deliberately absent — and so is `-k`: it is a RETIRED flag
(`--keydb` is the only spelling), and while it sat here the table claimed
an arity for a token the parser rejects, so `freemkv -k keydb.cfg …` had
its path quietly eaten before the rejection was reached.

## `is_flag_token`

The companion to the `scheme://` rule above, and the other half of the
same question: a value-flag that blindly takes the next token also takes
the next *flag*. `freemkv --keydb --raw disc:// mkv://out.mkv` set the
keydb path to "--raw" and dropped the `--raw`, and `freemkv info --keydb
--full disc://` dropped the `--full` — in both cases the user was answered
with something they did not ask for, silently.

ONE definition, used by both parsers (`pipe::parse_flags` and
`disc_info::parse_info_flags`), because two spellings of one rule is how
they came to disagree in the first place.

A lone `-` is not a flag (it is the conventional stdin/stdout name), and a
leading dash followed by a DIGIT is a negative number, so `--log-level -1`
still reaches the parser that reports it as out of range rather than being
re-reported as a missing value. Deliberately NOT applied to `--key-auth`:
a bearer token is opaque and may legitimately begin with `-`.

## `RETIRED_VALUE_FLAGS`

They are not flag arity in the `VALUE_FLAGS` sense — `parse_flags`
consumes nothing for them, it rejects them — but `collect_urls` still has
to step over the value, or the value counts as a third positional and the
whole invocation collapses into the bare usage hint. The user is then told
nothing about the flag that is actually gone. Stepping over it leaves
exactly two URLs, the rip route runs, and `parse_flags` delivers the
precise "unknown flag '-k' — try 'freemkv help'".

Kept honest by `pipe::tests::retired_value_flags_are_rejected_by_the_parser`:
every entry must be one the parser REJECTS, and must not also appear in
`VALUE_FLAGS`. `-k` used to sit in `VALUE_FLAGS` itself, which claimed an
arity for a token no parser arm names. `-k` was dropped in rc.6 (`--keydb`
is the only spelling); `--device`/`-d` were dropped when the device moved
into the source URL (`disc:///dev/sgN`).

## `stream_info_lines`

`v.label`, `a.label`, `a.language`, and `s.language` are disc-derived
strings (an MKV/m2ts track name or a raw MPLS/IFO language tag) and go
straight to the real terminal, so each is run through `disc_info::sanitize`
before printing — the same treatment `disc_info.rs` and `pipe.rs` give the
identical fields, so a crafted file can't inject terminal escapes here.
`a.codec` and `a.channels` are the library's own enums, not disc bytes, so
they are not sanitized.

## `TRACK_SINK_URL_LINES`

Every line here was verified against `libfreemkv`'s `parse_url` and its
`input()`/`output()` match arms, NOT against the README — the README is
where these nine schemes were documented while `--help` named none of
them, so it is the thing being checked, not the source of truth.

Their own block because the direction is load-bearing, not decoration:
each one's `input()` arm is `StreamWriteOnly`, so offering them as a
SOURCE is an error the user would otherwise only discover by hitting it.
`mp4://` and `dir://` read and write, so they stay inline with the rest.

## `update_keys_dest`

Factored out so the "`--keydb` is honored" behaviour is unit testable
without a network fetch — the prior bug was this flag being ignored and
the keydb always landing at the default location.

## Test: `a_logging_flag_does_not_swallow_the_following_flag`

`--log-file` took `it.next()` unconditionally, so `freemkv --log-file
--raw disc:// iso:///out/d.iso` set the log path to "--raw" and consumed
the flag — the rip then ran WITHOUT `--raw` and silently wrote a decrypted
image. `--log-level` has the same shape: it consumes the token, fails to
parse it as a number, prints "ignored", and the flag is gone either way.

A sibling fix guarded `--keydb` in `pipe::parse_flags` and its commit
message claimed both parsers were covered under one rule. They were not:
this file DEFINES `is_flag_token` and used it nowhere.

## Test: `stream_info_lines_render_purpose_secondary_and_label_tags`

The `info mkv://` / `info m2ts://` stream lines carry the localized
PURPOSE / secondary / label tags for an audio track, and a video label —
the tag-assembly arms of `stream_info_lines` that the escape-stripping
test never reaches (it uses `LabelPurpose::Normal`, no secondary, no
tags). Each purpose maps to its own locale key, so every arm of the
`purpose_key` match must render; a mutant that dropped the `secondary` tag
or collapsed two purposes would otherwise pass CI.

## Test: `stream_info_lines_strip_terminal_escapes_from_every_disc_controlled_field`

`info mkv://` / `info m2ts://` prints `v.label`, `a.label`, `a.language`,
and `s.language` straight from the file's own track metadata. Each is
disc/file-controlled (an MKV track name or a raw MPLS/IFO language tag),
so a crafted file carrying a terminal escape sequence in any of those four
fields must not have it survive to the terminal — mirroring
`disc_info::sanitize_strips_terminal_escape_sequences`.

## `commands_named_in`

Subcommand names a string tells the user to TYPE, as opposed to the many
places "freemkv" is just the product name in a sentence ("Quit freemkv",
"Wordt van kracht nadat u freemkv opnieuw start"). A command claim is a
`freemkv <word>` whose `<word>` is followed by something argument-shaped:
a flag, a `<placeholder>`, an `[optional]`, a `scheme://` URL, the
description column of a usage example, or a closing quote.

## `mod arg_tests`

The argv decisions `run()` and `init_logging()` make before anything else
happens — each of them previously unreachable from `cargo test`, either
because it was inline in a 400-line dispatcher or because installing a
process-global subscriber can only be done once per binary.

## Test: `is_flag_token_treats_negative_numbers_as_values_not_flags`

The predicate that keeps a value-flag from eating a following flag, and
its one deliberate exception: a leading `-` on a NEGATIVE NUMBER is a
value, not a flag, so `--log-level -1` still reaches the range check
instead of being read as the flag `-1`.

## `flags(...)` test helper

Just the two REQUESTS, for the assertions that are about what the user
asked for. The diagnostics are a separate axis and get their own tests;
folding them into every tuple comparison would have made the interesting
assertions unreadable.

## Test: `a_bad_log_level_is_ignored_rather_than_guessed_at`

Bad input is reported and IGNORED — never silently clamped up to 1, and
never treated as "a log was requested". `0` is out of range and a typo
must not quietly give the user a different verbosity than they asked for.

## Test: `a_refused_log_file_value_records_a_diagnostic`

A refused `--log-file` value must be REPORTED, not swallowed silently. The
`--log-level` arm records a `PendingDiag` for every failure mode (missing
value, out of range, not a number). The `--log-file` arm recorded NONE:
`freemkv --log-file --raw disc:// …` refused "--raw" as the path
(correctly, per the value guard) and then said nothing — no diagnostic, no
log written, the rip ran anyway. The user asked for a log and got neither
a log nor a word why. Absence of a log is itself a bug.

Mutation caught: dropping the `None =>` diagnostic from the `--log-file`
arm (which reverts it to the silent `if let Some(..)` form).

## Test: `every_deferred_startup_diagnostic_resolves_to_real_localized_text`

Every deferred startup diagnostic must survive the round trip through the
catalog: real key, real translation, placeholders filled in.

The defect this closes is subtle. These messages were hard-coded English,
and the fix was to route them through `strings::fmt` — but a key that is
not in the catalog renders as its own dotted path (`freemkv_i18n::get`'s
documented miss behaviour, "makes missing translations visible"). So a
typo turns "--log-level: requires a value…" into
"error.log_level_needs_value", which is not a fix, it is a worse bug:
still not localized AND no longer readable. Nothing else in the suite can
see that, because these run before `strings::init()` and are printed from
`run()`.

The check MUST go against the raw catalog (`strings::get`), never through
`PendingDiag::render`. `render` calls `get_or`/`fmt_or`, whose whole job
is to substitute compiled-in English for a missing key — so a `render() !=
key` assertion can never fail whether or not the key is really translated.
That was the previous version of this test, and it was vacuous in
precisely the way it claimed to catch. `get(key) != key` holds only when
the catalog actually carries a string for the key.

Mutation caught: misspelling any of these keys, dropping one from the
locale catalogs, or renaming a `{placeholder}` on either side.

## Test: `the_pre_locale_argv_pass_prints_nothing_of_its_own`

This is the constraint that makes the whole `PendingDiag` detour
necessary, and it is invisible at every call site. `freemkv_i18n::get`
LAZILY installs the environment-derived catalog on first use, and
`set_language` then refuses to change it. So a `strings::get` in the
pre-pass does not localize the message — it silently disables
`--language` for the whole process — and a bare `eprintln!` there is the
hard-coded English this change removed. Both regressions look completely
reasonable in a diff, and neither one fails any other test: the message
still appears, just in the wrong language or off the wrong catalog.

Mutation caught: putting an `eprintln!`/`println!`/`print!` back into
`parse_logging_flags`, `init_logging` or `strip_language_flag`.

## Test: `the_info_surface_never_prints_english_of_its_own`

The `info` subcommand must speak ONE language, whatever the URL. `freemkv
info disc://` was fully localized and `freemkv info mkv://Movie.mkv` — the
same subcommand, one match arm away — printed `File:` / `Duration:` /
`Streams:` in English in all 29 locales. The `--share` consent flow had
the same split: its manual-fallback instructions came from
`drive.submit_manual` while the question that gates them, the thank-you
and both refusal lines were hard-coded.

A source pin because neither path is reachable from a test: the container
arm needs a real media file open through `libfreemkv::input`, and the
consent flow needs an interactive stdin AND a live drive capture. The
cli-parity goldens do not cover them either — `info_iso` is operator-run
and there is no `info mkv://` case at all — so without this the English
could come straight back with the whole suite green.

Mutation caught: replacing any of these `strings::get`/`get_or` calls with
the literal it renders.
