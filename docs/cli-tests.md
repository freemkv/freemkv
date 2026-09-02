# `every_flag_the_rip_parser_accepts_is_named_in_the_help`

A flag the parser ACTS on must be findable in `--help`.

`--force` shipped as the only way to write into a non-empty `dir://`
target — the rejection tells the user to "pass --force", and `--help`
listed every other flag and not that one, so the flag was discoverable
only from the error it exists to clear.

The list is written out rather than read back from the parser: a scraper
over `parse_flags` would agree with the code under test by construction.
Adding a flag to the parser and not to this list is the thing that must
fail here.
