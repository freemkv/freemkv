[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)

# freemkv

Open source command-line tool for 4K UHD / Blu-ray disc backup.

**[Download latest release](https://github.com/freemkv/freemkv/releases/latest)**

Part of the [freemkv](https://github.com/freemkv) project. Built on [libfreemkv](https://github.com/freemkv/libfreemkv).

## Install

Download a prebuilt binary from [releases](https://github.com/freemkv/freemkv/releases/latest), or build from source:

```bash
cargo install freemkv
```

Requires Linux (SG_IO) or macOS (IOKit). Windows planned.

## Commands

```
freemkv <command> [options]

Commands:
  drive-info            Show drive hardware and profile
  disc-info             Show disc titles, streams, and sizes
  rip [--output /path]  Back up a disc
  version               Show version
  help                  Show this help

Options:
  --device /dev/sgN     Specify device (default: auto-detect)
  --keydb /path         Path to KEYDB.cfg for AACS decryption
  --quiet               Minimal output

Drive-info options:
  --share               Share your drive profile to expand support
  --mask                Mask serial numbers (use with --share)
```

## Examples

```bash
# Show drive info
freemkv drive-info

# Show disc contents
freemkv disc-info

# Back up the main title
freemkv rip --output ~/backups/

# Share your drive profile (helps everyone)
freemkv drive-info --share --mask
```

## Contributing

Run `freemkv drive-info --share` to submit your drive's profile. Supports LG, ASUS, HP, and other MediaTek-based drives. Pioneer support planned.

## License

AGPL-3.0-only
