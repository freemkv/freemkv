# `Settings::normalize`

Every setting must carry a value the popup can select and the engine can
match on. `#[serde(default)]` fills fields missing from the file; this
additionally snaps an enum-valued field back to its default when the
persisted string is not one of the recognized options (a stale file from
an older build, or a hand-edit). `dest_dir` and `keydb_path` get the
same fallback-to-default treatment when they aren't usable (a
non-absolute destination, an empty keydb path) — every other free-form
field (URLs, numbers) is left as-is.
