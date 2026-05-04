# Changelog

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
