# info: drive-profile capture and `--share` submission

## `CAPTURE_COMMAND`

The invocation that produces a drive profile, as printed into artifacts that
LEAVE this machine: the profile TOML header and the auto-filed GitHub issue
body. It shipped as `freemkv drive-info`, a subcommand the dispatcher has
never accepted — so every filed drive-support issue told the next reader to
run a command that does not exist. Named once here, and pinned against the
real dispatcher by `capture_command_is_a_real_subcommand`.

## `DriveIdentity`

The drive identity, as it is safe to put in front of a human.

Every field is an INQUIRY / GET CONFIGURATION string: raw bytes under drive
FIRMWARE control, decoded `from_utf8_lossy`. They reach a real terminal, a
`drive.toml` on disk, and — with `--share` — the body of a GitHub issue on a
public tracker. `disc_info.rs` runs the identical class of field through
`sanitize` before printing; this module printed all of them verbatim, so a
drive whose vendor string carries ESC could colour or overwrite the profile
block a user is about to publish, and one carrying a newline could forge a
whole line of it (including the `# … — freemkv info disc://` TOML header
comment, where a newline ends the comment and starts a key).

Sanitising ONCE here, at the point the strings enter this module, is what
makes that unrepeatable: `run` never holds the raw `DriveId`, so there is no
site left that could print an unsanitised field. The raw bytes are still
captured verbatim into `inquiry.bin` — that file is evidence, not display.

## `drive_identity_lines`

Takes the raw `libfreemkv::DriveId` and sanitises it itself, so the block
cannot be produced from unsanitised fields by mistake — pinned by
`the_printed_drive_block_carries_no_firmware_escape_sequences`.

## `toml_header_comment`

The `drive.toml` header comment, and the blank line after it.

The one place in the file where a firmware string is NOT `toml_escape`d,
because a comment needs no escaping — except that a comment ends at a
newline, so an embedded one would end the comment and let the rest of the
vendor string be read as TOML. It is safe only because `DriveIdentity` has
already removed every control character; that is what
`the_toml_header_comment_is_one_line_whatever_the_firmware_says` pins.

## `may_prompt_for_consent`

Whether to offer the auto-submit prompt at all.

Both channels have to be a terminal, and that is the correction. The gate
used to test stdin only while the question itself is printed to STDERR, so
`freemkv info disc:// --share 2>/dev/null` passed the gate, asked nothing
the user could see, and blocked on a read. A bare Enter — pressed to unstick
an apparently hung command — is the [Y] default, and the posted profile
carries the drive serial unless `--mask` was given. That is publishing
identifying hardware data to a public tracker without the user having been
asked a question they could read.

A non-interactive stdin (a CI runner, cron, `--share </dev/null`) cannot
give informed consent either, and EOF there must never be read as yes. With
no compiled-in token there is nothing to submit with, so no prompt.

## `consent_granted`

Whether the user EXPLICITLY consented to submit the profile.

`n` is `read_line`'s byte count (0 == EOF), `answer` is the trimmed reply,
`affirmative` is the locale's yes-token (from the SAME catalog as the
prompt). Split out from the submit flow so the consent rule is
unit-testable without a real stdin, and so the exfil-relevant decision
lives in exactly one place.

Three things are deliberately NOT consent, because this posts identifying
hardware data (the drive serial, unless `--mask`) to a public tracker:
  * EOF (`n == 0`) — a closed/redirected stdin cannot agree to anything.
  * A bare Enter (empty answer) — a machine-controlled `de.json` can render
    the prompt as "[j/N]" (default NO), so treating Enter as YES would post
    against a hint the user reasonably read as "no". Opt-in, never opt-out.
  * Anything that is not the locale's affirmative token.

## `curl_program`

Which `curl` to run.

A bare program name is resolved by the OS. On Windows that search starts
with the launching executable's own directory and the current working
directory, BEFORE the system directories — so a `curl.exe` sitting in the
folder the user unzipped freemkv into, or simply the folder they happened
to run it from, is preferred over the real one (CWE-427). Windows has
shipped curl in System32 since 1803, so name it outright and leave nothing
for the search order to decide.

Unix keeps the bare name: its `PATH` is the user's own, and it does not
implicitly include the program's directory or the CWD.

## `curl_submit_args`

The exact `curl` argv the auto-submit POST runs.

Split out of `submit_issue` because inside it it was untestable — the only
way to observe the argv was to make a real request to GitHub. It shipped
with no `--connect-timeout`, no `--max-time` and no `--max-filesize`, so the
last step of `freemkv info disc:// --share` could hang forever on a stalled
peer, after the profile was already safely on disk. `-f` is deliberately NOT
passed: the caller reads the response body to recover the issue URL.

## `zip_files`

Archive exactly the named files from `dir`.

It takes a manifest rather than walking the directory, and that is the
whole point. `profile_dir` is derived from firmware-controlled INQUIRY
strings and `create_dir_all` succeeds on a directory that ALREADY EXISTS,
so a directory-walking archive published whatever else happened to be in
there — unrelated local files, or a previous unmasked run's `gc_*.bin` that
this capture did not overwrite. The archive is then base64'd into a GitHub
issue body and can be posted to a public tracker, so "everything in this
directory" is not a safe bound. "Everything this run wrote" is.

A name in the manifest that is missing on disk is skipped rather than
failing the whole submission.

## `sanitize_component`

Reduce an untrusted firmware-derived string to a safe single path
component: lowercase ASCII alphanumerics, `-`, and `_` only; every other
byte (spaces, `/`, `\`, `.`, NUL, multibyte) becomes `-`; runs of `-`
collapse to one; and leading/trailing `-` are trimmed. The result can never
be `.`, `..`, contain a separator, or escape the working directory. Falls
back to `drive` if empty.

## `toml_escape`

Escape a string for embedding inside a TOML basic (double-quoted) string.

The drive identity fields come from raw INQUIRY / GET_CONFIG bytes under
firmware control, so a value can legitimately contain a `"`, `\`, or a
control character (newline, NUL, etc.). Embedded verbatim those break the
`key = "..."` line and make `drive.toml` unparseable. Backslash and quote
are backslash-escaped; the TOML-defined control escapes (`\n`, `\r`, `\t`)
are emitted by name; every other C0 control / DEL becomes a `\uXXXX` escape.
Ordinary printable text passes through unchanged.

## tests: `hostile_drive_id`

A drive whose firmware answers INQUIRY with terminal escapes. Nothing
exotic: these are `from_utf8_lossy` of device-supplied bytes, and a wedged
USB-SATA bridge produces junk here routinely. The interesting part is where
the junk GOES — a terminal, `drive.toml`, and the body of a public GitHub
issue.

## tests: `the_submit_curl_is_named_absolutely_where_the_search_order_is_unsafe`

On Windows the program to run must be named absolutely. Windows resolves a
bare program name by searching the launching executable's own directory and
the current working directory before the system directories — so `freemkv
info disc:// --share`, run from a folder containing a hostile `curl.exe`
(an unzipped download, say), would run that one with the submit token on
its command line. Naming System32's curl outright leaves nothing for the
search order to pick. Testable off Windows because the platform decision
lives in `curl_program` and the string building lives here.

## tests: `the_auto_submit_post_is_bounded_in_time_and_size`

The auto-submit POST is the one network call this module makes, and it is
the LAST thing `--share` does: the profile and its zip are already on disk,
and the manual instructions print either way. It shipped with no bound at
all on connect, total time, or response size, so a stalled peer hung the
command after the work was done.

## tests: `submitting_requires_an_explicit_locale_matched_yes`

Submitting the drive profile requires EXPLICIT consent, and the accept
token comes from the locale — never a hard-coded ASCII "y" against a
machine-controlled prompt.

The prompt string loads from the catalog (`./locales`), so a crafted
`de.json` can render "[j/N]" (default NO). The old parser treated a bare
Enter as YES and eq_ignore_ascii_case("y"), so a German user pressing
Enter — reasonably reading it as "no" — POSTed the profile (with the drive
serial unless `--mask`) to a public tracker. This pins the two rules that
close it.

Mutation caught: bringing back `ans.is_empty()` as an accept (default-yes on
Enter), or hard-coding "y" instead of the locale's affirmative token.

## tests: `scratch_dir`

These tests populate a directory, assert on exactly what is in it, and
`remove_dir_all` it at both ends. A fixed name is shared state: two `cargo
test` processes running against the same checkout delete each other's
fixtures mid-assertion, which reads as a share-safety failure — the most
alarming possible false alarm, since a red here means an unrelated local
file was published.

## tests: `the_archive_carries_only_the_files_this_run_wrote`

The archive is bounded by what this run WROTE, not by what happens to be in
the directory. `profile_dir` is named from firmware-controlled INQUIRY
strings and `create_dir_all` succeeds on an existing directory, so a
directory walk published two things it should not: unrelated local files
that were already there, and a previous UNMASKED run's capture files that
this masked run did not happen to overwrite. The archive goes into a GitHub
issue body that can be posted to a public tracker.

## tests: `consent_is_only_offered_when_the_question_is_visible_and_answerable`

The consent prompt is only offered when the user can actually READ it. The
question goes to stderr, so testing stdin alone let `--share 2>/dev/null`
reach a blocking read with nothing on screen — and bare Enter is the [Y]
default, which posts the drive serial to a public tracker.
</content>
