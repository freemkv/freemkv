# Changelog

## 0.11.15 (2026-04-21)

### Lint cleanup
- Fix all `cargo fmt` and `cargo clippy -D warnings` issues.

## 0.11.14 (2026-04-21)

### Audit fixes
- libfreemkv 0.11.14: trailing sector fix, verify stop support, O_CLOEXEC, sense format detection.
- CLI verify callback updated for new ProgressFn return type.

## 0.11.13 (2026-04-21)

### Fix: fast reads only in rip path
- libfreemkv 0.11.13: batch reads use 5s fast timeout. No more 10-min recovery blocking binary search.

## 0.11.12 (2026-04-21)

### Halt + events + light recovery
- libfreemkv 0.11.12: drive halt, sector events, 15s light recovery in binary search.

## 0.11.11 (2026-04-20)

### Binary search recovery
- libfreemkv 0.11.11: binary search error recovery for marginal disc sectors.

## 0.11.10 (2026-04-20)

### Version sync
- Unified version with libfreemkv 0.11.10.

## 0.11.9 (2026-04-20)

### Fast verify
- Verify uses fast 5s-timeout reads. Full disc check completes in ~50 min instead of hours.

## 0.11.8 (2026-04-20)

### Disc verify
- **freemkv verify disc://** — sector-by-sector health check. Reports Good/Slow/Recovered/Bad sectors with chapter mapping.

## 0.11.7 (2026-04-19)

### TrueHD fix
- libfreemkv 0.11.7: TrueHD parser rewrite. Zero decode errors on UHD and BD.

## 0.11.6 (2026-04-18)

### TrueHD fix
- All libfreemkv 0.11.6 fixes including TrueHD BD-TS header corruption.

## 0.11.5 (2026-04-18)

### MKV container fixes
- **MKV title tag** — writes disc name instead of playlist filename (e.g. "Dune" not "00800.mpls").
- All libfreemkv 0.11.5 MKV fixes: timestamps, frame rate, HDR, chapters, disposition.

## 0.11.3 (2026-04-18)

### Unified versioning
- All freemkv repos now share the same version number.
- Updated libfreemkv dependency to 0.11.

## 0.10.5 (2026-04-18)

### Single drive session
- **pipe_disc()** — disc rips use one Drive session from open through stream. No double-open, no double-init.
- **DiscStream::new()** — uses the new constructor directly instead of open_drive()/open_iso() helpers.
- **README** — added DVD disc info sample output, listed all 7 bundled languages.

## 0.10.4 (2026-04-16)

### DVD CSS decryption
- **CSS: Encrypted** label for DVD discs (was showing "AACS: Encrypted")
- Added `css_encrypted` locale key to all 7 languages

## 0.10.3 (2026-04-16)

### DVD support
- First successful DVD rip — CSS authentication enables reading scrambled sectors
- Removed internal audit and test plan files from public repo
- Added multi-language section to README
- Added public repo rules to CLAUDE.md

## 0.10.2 (2026-04-15)

### Fixes
- **Disc→ISO batch overflow** — pass detect_max_batch_sectors() to Disc::copy() instead of hardcoded 64 sectors
- **Header scan ordering** — stream info displayed after headers_ready() scan so stdio/network metadata is populated

## 0.10.1 (2026-04-15)

### Clean architecture
- **One pipeline for everything** — `run()` builds job list, loops `pipe()` per title. No separate batch/single paths.
- **CountingStream for progress** — bytes written tracked via wrapper, not baked into streams
- **disc_to_iso uses Disc::copy()** — sector dump, not a stream

### i18n only — zero hardcoded English
- All user-facing strings through `strings::get()` / `strings::fmt()`
- CLI tests check error codes (E9002, E9001) not English messages
- New locale keys: rip.interrupted, rip.drive, rip.disc_label, rip.title_info, etc.

### Cleanup
- Deleted `disc_batch()`, `batch_stream()`, `copy_loop()` — all replaced by single `run()` flow
- Updated error section in en.json to match new error codes

## 0.10.0 (2026-04-15)

### PES pipeline
- **pipe() is 100% PES** — unified `Stream::read()` / `Stream::write()` API, no byte-level fallback
- **codec_privates from DiscTitle** — no separate collection pass in pipe.rs
- **pipe() returns Result** — proper error propagation, no process::exit in pipeline

### Testing + audit
- **122-test plan** (TESTPLAN.md) — full stream matrix UHD/BD/DVD
- **CLI integration tests** — 9 tests covering error handling, help, quiet mode
- **Codebase audit** — all findings fixed
- **CI lint job** — clippy in CI

### Cleanup
- Signal handler uses SeqCst ordering
- Fix clippy warnings in pipe.rs
- Improved disc info output

## 0.9.0 (2026-04-13)

### Pipeline refactor + decrypt-on-read
- **pipe() engine** — single pipeline function for all source→dest combinations
- **Decrypt-on-read** — automatic decryption by default, `--raw` to skip
- **Disc-to-ISO** — `freemkv disc:// iso://Disc.iso` (decrypted or --raw)
- **5 flags** — simplified CLI: `-t`, `-k`, `-v`, `-q`, `--raw`
- **Default all titles** — rips everything unless `-t` specified
- **Fix double-decrypt bug** — IsoStream no longer decrypts when pipeline also decrypts
- **Quiet mode** — `-q` suppresses all output
- **Error code translations** — en + es locale support
- **Honest Quick Start** — README documents KEYDB setup requirement

### Platform
- **Rust 1.86 MSRV** pinned
- **aarch64 build fix** — install cross from prebuilt binary

## 0.8.0 (2026-04-11)

### DVD + batch ripping
- **DVD support** — insert a DVD, get an MKV. Same command as BD/UHD.
- **`--all`** — rip every title from a disc
- **`--min N`** — minimum duration in minutes (with --all)
- **`-t N` repeatable** — rip specific titles
- **Chapters** — MPLS marks flow through to MKV Chapters element
- **Progress for all sources** — percentage + ETA for disc, ISO, m2ts, mkv
- **Ctrl+C handling** in pipe path
- **`iso://` with --all** — batch rip from ISO images

### Cleanup
- Removed dead code (rip.rs, remux.rs — superseded by pipe.rs)
- `--min` warns when used without `--all`

## 0.7.2 (2026-04-11)

### Windows support

- **Windows build target** — x86_64-pc-windows-msvc in release workflow
- **Windows SIGINT** — SetConsoleCtrlHandler for Ctrl+C handling
- **Stable download URLs** — both versioned + stable-name archives per release
- **CI** — cargo check on windows-latest, actions/checkout@v5
- libc dependency gated to unix only

## 0.7.1 (2026-04-11)

### ISO support + SectorReader refactor

- **`iso://` stream** — read Blu-ray ISO images with full title/stream/label scanning
- **`stdio://` stream** — pipe to/from stdin/stdout
- **Raw INQUIRY + GET_CONFIG 010C** in `--share` issue body (inline hex, no download needed)
- libfreemkv 0.7.1 (SectorReader trait, `Disc::scan_image()`, `resolve_encryption()`)

## 0.7.0 (2026-04-11)

### Stream architecture

- **`freemkv <source> <dest>`** — two arguments, any input to any output
- **7 stream types** — `disc://`, `iso://`, `mkv://`, `m2ts://`, `network://`, `stdio://`, `null://`
- **Strict URL format** — all URLs require `scheme://path`, bare paths rejected
- **Pipe mode** (`pipe.rs`) — generic source→dest copy with metadata flow
- **Network streaming** — rip on one machine, remux on another
- **`build.rs`** — auto-generates bundled locale code from `locales/*.json`
- **Updated CLI dispatcher** — URL routing replaces subcommand-based routing
- **FEATURES.md** updated to v0.7.0

## 0.6.0 (2026-04-10)

### MKV output

- **MKV is now the default output format** — `freemkv rip` produces `.mkv` files
- **`--raw` flag** — use `--raw` for original `.m2ts` output
- **`freemkv remux`** — convert existing `.m2ts` files to MKV without a drive

### Restored features

- **`--share` restored** — full drive profile capture + GitHub issue submission (INQUIRY, GET_CONFIG features, READ_BUFFER, zip, base64)
- **i18n string table restored** — `strings.rs` + `locales/en.json`, zero hardcoded English in CLI
- **`disc-info --basic` restored** — show disc info without BD-J labels

### Improvements

- **Safe output filenames** — spaces replaced with underscores, no track numbers (`Dune.mkv`)
- **`--share`/`--mask`/`--quiet` in top-level help** — discoverable from `freemkv help`
- **Works with all drives** — uses new `open()` API that doesn't require profile match
- **Profile status shown** — drive-info shows "Supported" or "Unknown"

### Dependencies

- Added `ureq`, `zip`, `serde_json` for `--share` functionality

## 0.4.0 (2026-04-09)

### Rip command — working end-to-end

- **`freemkv rip`** — fully functional disc backup: scan → decrypt → write m2ts
- **12.5-23 MB/s read speed** on real hardware (BU40N, V for Vendetta BD)
- **AACS 1.0 decryption** — transparent, automatic when KEYDB.cfg available
- **Adaptive error handling** — batch size ramp-down, speed tier reduction, zero-fill skip
- **Progress display** — MB/s, ETA, percentage, error count
- **SIGINT handling** — clean interrupt, partial file preserved, disc ejected

### Stream display improvements

- No more phantom "Unknown(0)" video streams
- Subtitle languages correct (was truncating: "ng " → "eng")
- Secondary streams (commentary, PiP) parsed correctly

### Dependencies

- libfreemkv 0.5.0

## 0.3.0 (2026-04-07)

### Stream labels

- Uses libfreemkv 0.4.0 label system — stream labels from BD-J config files
- Displays label data (purpose, codec hint, variant) alongside MPLS stream info
- Labels are optional enrichment — streams always have codec + language from MPLS

### Dependencies

- libfreemkv 0.4.0

## 0.2.1

- Thin CLI over libfreemkv
- No SCSI code — all logic in library

## 0.2.0

- Initial public release
- disc-info, drive-info commands
- Uses isolang for language display names
