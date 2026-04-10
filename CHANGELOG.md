# Changelog

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
