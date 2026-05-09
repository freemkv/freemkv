# Changelog

## 0.18.1 (2026-05-09)

### Sync release — picks up libfreemkv 0.18.1

Sync release across all freemkv crates to version 0.18.1. User-facing
behavior is unchanged: same flags, same `--multipass` semantics (one
invocation = one pass), same help output, same exit codes.

**CLI source restructure.** The subcommand handlers moved into
`src/cmd/{disc_info,drive_info,info,update_keys,verify}.rs`. `main.rs`
is now a thin dispatcher (~150 LOC, was ~491).

**Migrated off the deprecated `pes::Stream` trait.** The CLI now talks
to libfreemkv via `FrameSource` / `FrameSink` directly. `disc_to_iso`
no longer calls `Disc::copy` — the multipass dispatch (sweep vs patch)
is the CLI's job, not the library's, and is now done inline based on
mapfile state.

See [libfreemkv CHANGELOG](https://github.com/freemkv/libfreemkv/blob/main/CHANGELOG.md#0181-2026-05-09)
for the underlying I/O stack redesign.

## 0.17.7 (2026-05-08)

### Sync release — picks up libfreemkv 0.17.7

Sync release across all freemkv crates to version 0.17.7. No CLI functional changes. Picks up autorip's UI + audit-fix work for users running the CLI directly via `freemkv ... --multipass` (the new sliding-window display, ETA stability, and Pass N early-skip on full recovery all live in the lib path the CLI uses). See [autorip CHANGELOG](https://github.com/freemkv/autorip/blob/main/CHANGELOG.md#0177-2026-05-08) for details.

## 0.17.5 (2026-05-08)

### Sync release — picks up libfreemkv 0.17.5 Pass N recovery improvements

Sync release across all freemkv crates to version 0.17.5. The CLI itself has no functional changes; `--multipass` automatically benefits from the new patch primitive: kernel `/dev/sr0` pread fallback (Linux), per-range watchdog fix, per-sector range budget. See [libfreemkv CHANGELOG](https://github.com/freemkv/libfreemkv/blob/main/CHANGELOG.md#0175-2026-05-08) for details.

## 0.17.0 (2026-05-04)

### Code quality improvements, unified versioning

Sync release across all freemkv crates to version 0.17.0.

**libfreemkv changes:**
- Fixed unwrap safety in `disc/mod.rs:1553` using explicit pattern matching
- Patch pass excludes Unreadable sectors from work list (only retries NonTrimmed/NonScraped)
- Exposes `bytes_bad_in_title` for accurate UI reporting
- All 256 tests pass, clippy clean with `-D warnings`

**bdemu changes:** None. Version sync only.

**freemkv CLI changes:** None. Version sync only.

**autorip changes:** None. Version sync only.

## 0.16.3 (2026-04-30)

### Multipass dispatch, damage-jump algorithm, reverse patch default

Sync release with libfreemkv v0.16.3 changes. See [libfreemkv CHANGELOG](https://github.com/freemkv/libfreemkv/blob/main/CHANGELOG.md#0160-2026-04-30) for details on:
- Multipass dispatch with mapfile-based auto-detection
- Damage-jump algorithm (16-block window, 12% threshold)
- Reverse-direction patch as default
