# derived_paths_stay_absolute_without_userprofile_or_appdata

Every derived path stays absolute with no home variable set.

This is the regression test for the real bug the mutation runner found:
`unwrap_or_default()` produced an empty base, so `support_dir()` became
`"Library/Application Support/freemkv"` and the settings file, the
downloaded keydb and the rip output all landed relative to the process
CWD. Unset `HOME` is not exotic — a container, a systemd unit or `env -i`
all produce it, and autorip ships in Docker.

`tests/settings.rs` covers the Unix side by removing `HOME`; on Windows
the `imp` module reads `USERPROFILE` and `APPDATA` instead, so that test
compiles there but observes whatever the environment happens to hold.
This is the Windows counterpart. It cannot be run from a Mac, but it did
not exist at all, so there was nothing accidentally passing.
