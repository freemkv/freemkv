[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)

# freemkv

Command-line tool for 4K UHD / Blu-ray / DVD drive identification and disc backup.

**[Downloads and quick start at github.com/freemkv](https://github.com/freemkv)**

## Build from Source

```bash
cargo install freemkv
```

Or:

```bash
git clone https://github.com/freemkv/freemkv.git
cd freemkv
cargo build --release
```

Requires Linux (SG_IO SCSI passthrough). macOS and Windows planned.

## All Options

```
freemkv <version>

Usage: freemkv <command> [options]

Commands:
  info                Show drive information
  rip [--output /path]  Back up a disc (coming soon)
  version             Show version
  help                Show this help

Global options:
  --device /dev/sgN   Specify device (default: auto-detect)
  --quiet             Minimal output

Info options:
  --share             Share your drive profile to help expand drive support
  --mask              Mask serial numbers (use with --share)

Examples:
  freemkv info
  freemkv info --share
  freemkv info --share --mask
```

## Contributing

Run `freemkv info --share` to submit your drive profile. 206 drives supported (LG, ASUS, HP). Built on [libfreemkv](https://github.com/freemkv/libfreemkv).

## License

AGPL-3.0-only
