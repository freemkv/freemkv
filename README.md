[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)

# freemkv

Command-line tool for 4K UHD / Blu-ray / DVD drive identification and disc backup. Built in Rust on [libfreemkv](https://github.com/freemkv/libfreemkv).

Part of the [freemkv](https://github.com/freemkv) project.

## Download

**Latest: v0.1.5 (2026-04-06)**

| Platform | | |
|----------|-|---|
| Linux (Intel/AMD) | [**Download**](https://github.com/freemkv/freemkv/releases/download/v0.1.5/freemkv-v0.1.5-x86_64-unknown-linux-gnu.tar.gz) | Most desktops and servers |
| Linux (ARM) | [**Download**](https://github.com/freemkv/freemkv/releases/download/v0.1.5/freemkv-v0.1.5-aarch64-unknown-linux-gnu.tar.gz) | Raspberry Pi, ARM servers |
| macOS | Coming soon | |
| Windows | Coming soon | |

[Older versions](https://github.com/freemkv/freemkv/releases) · Build from source: `cargo install freemkv`

## Quick Start

```bash
wget -qO- https://github.com/freemkv/freemkv/releases/download/v0.1.5/freemkv-v0.1.5-x86_64-unknown-linux-gnu.tar.gz | tar xz
./freemkv info
```

```
freemkv 0.1.5

Drive Information
  Device:              /dev/sg4
  Manufacturer:        HL-DT-ST
  Product:             BD-RE BU40N
  Revision:            1.03
  Serial number:       MO6J7HB1010
  Firmware date:       2018-10-24
  Bus encryption:      17

Platform Information
  Drive platform:      MTK MT1959
  Firmware version:    1.03/NM00000

Run 'freemkv info --share' to share your profile and help expand drive support.
```

```bash
# Share your drive profile
./freemkv info --share

# Share with masked serial
./freemkv info --share --mask

# Back up a disc (coming soon)
./freemkv rip --output ~/backups/
```

## All Options

```
freemkv 0.1.5

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

## Requirements

- Linux (SG_IO SCSI passthrough). macOS and Windows planned.
- Root or `cdrom`/`optical` group for device access

## Contributing

Run `freemkv info --share` — that's the single most useful thing you can do. Every submitted profile expands hardware support.

206 drives supported across the MediaTek MT1959 chipset (LG, ASUS, HP). Renesas (Pioneer) planned.

## License

AGPL-3.0-only
