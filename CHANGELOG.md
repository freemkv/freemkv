# Changelog

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
