# `tests/settings.rs` — rationale for individual regression tests

## `derived_paths_stay_absolute_without_a_home_variable`

Every path this app derives from the user's home must be ABSOLUTE, even
when the environment does not say where home is.

`home_dir()` used `unwrap_or_default()`, so an unset `HOME` produced an
empty base and every derived path came out relative: `shellexpand("~/x")`
returned `"x"`, `support_dir()` returned
`"Library/Application Support/freemkv"`, `default_dest_dir()` returned
`"Movies"`. Settings, the downloaded keydb and rip output then landed
relative to the process's working directory.

This was invisible to CI, which always has `HOME` set. It surfaced on a
bare EC2 runner (user-data runs as root with no `HOME`), where the existing
`shellexpand_expands_tilde_only_at_start` assertion — which already
demanded an absolute path — failed. The test was right; the code was wrong.

`HOME` is process-global, so this test mutates and restores it and must not
run concurrently with anything else reading it; it is the only test here
that touches the variable.

## `settings_round_trip_through_a_real_file`

`save()` must actually write, and `load()` must actually read it back.

The mutation run replaced `Settings::save` with `Ok(())` and
`Settings::load` with `Default::default()`, and both survived: every test
here either inspected a `Settings` in memory or went through `serde_json`
directly, so nothing ever proved a value survives a trip to disk. A `save()`
that silently does nothing loses the user's keyserver token and dest dir on
every restart, reporting success each time.

Redirects the home/appdata variables at a temp dir so it never touches the
real `gui-settings.json`. It must redirect the RIGHT ones: `support_dir()`
is per-OS (`$HOME/Library/...` on macOS, `%APPDATA%` on Windows), and a
first version of this test set only `HOME` — so on Windows it read and WROTE
the runner's actual settings and failed in CI. The dest dir is likewise
built from the temp path rather than hardcoded, because `normalize()`
replaces a dest that is not absolute ON THIS OS, and `/tmp/...` is not
absolute on Windows.

## `saved_settings_file_is_private_and_leaves_no_temp_file`

`gui-settings.json` holds `keyserver_token` in plaintext. `save()` used to
be a bare `std::fs::write`: mode defaults to the process umask (typically
0644 on Unix — world/group readable) and it writes the final path
directly, so a crash mid-write leaves a truncated file that `load()`
silently discards via `.ok()`, losing the token with no warning.

This checks the on-disk artifact `save()` actually produces: no stray
`.json.tmp.*` file left behind (the rename completed), and — on Unix,
where the permission bits are meaningful — mode 0600 on the file that
holds the secret.

## `in_a_temp_support_dir` helper

Helper for the tests below: point the per-OS support dir at a fresh
temp directory, run `f`, restore the environment, delete the directory.

Same shape as the round-trip test above (same `HOME_LOCK`, same three
variables, same `catch_unwind` so a failed assertion still restores the
environment) — `support_dir()` is `$HOME/Library/...` on macOS and
`%APPDATA%` on Windows, so redirecting only `HOME` would make these read
and WRITE the runner's real `gui-settings.json`.

## `a_settings_file_with_a_utf8_bom_still_loads_the_users_values`

A leading UTF-8 BOM must not cost the user their whole configuration.

`load()` parsed the file with `serde_json::from_str(...).ok()` and fell
back to `unwrap_or_default()` on ANY failure, so a single invisible
`\u{FEFF}` at the start of `gui-settings.json` reset every setting —
key service URL, token, output folder, keydb path — with no log line and
nothing in the UI. The next save then overwrote the file with the
defaults, so the originals were gone for good.

The BOM is not exotic on the platform that hits it: PowerShell's
`Out-File -Encoding utf8` writes one, and Notepad has historically saved
UTF-8 with a BOM by default — so a Windows user hand-editing the file to
paste a keyserver token silently loses everything.

## `an_unparseable_settings_file_is_preserved_before_a_save_can_overwrite_it`

A settings file that cannot be parsed must be PRESERVED, not overwritten.

Whatever the cause (a truncated write, a hand-edit with a stray comma),
the file still holds the user's key service token and output folder. The
old `load()` returned defaults and left the unreadable file in place, so
the very next `save()` — settings popup, window resize, anything —
replaced it with defaults and the values were unrecoverable.

So: rename it aside before the app can write over it, and prove the
preserved copy still has the original bytes AFTER a save has happened.

## `a_leftover_temp_file_neither_blocks_the_next_save_nor_keeps_the_token`

A failed save must not leave the token lying around — nor poison every save
that follows it.

`save()` writes to `gui-settings.json.tmp.<pid>` and renames. Only the
RENAME branch cleaned that temp file up: a failure inside the write (a full
disk, an I/O error part-way through `write_all`/`sync_all`) returned
immediately, leaving a file that holds `keyserver_token` in plaintext. And
because the name is per-PROCESS, not per-attempt, every later `save()` in
the same process then hit `create_new` → `AlreadyExists` and failed
forever: the user changes a setting, the app says it saved nothing, and the
orphan with the secret in it stays on disk.

The debris is simulated directly — the temp path is deterministic — because
no test can force `write_all` to fail part-way on a real filesystem.
