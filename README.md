[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)

# freemkv

Command-line tool for 4K UHD / Blu-ray / DVD drive identification and disc backup. Built in Rust on [libfreemkv](https://github.com/freemkv/libfreemkv).

Part of the [freemkv](https://github.com/freemkv) project.

## Install

### From source

```bash
cargo install freemkv
```

Or build manually:

```bash
git clone https://github.com/freemkv/freemkv.git
cd freemkv
cargo build --release
sudo cp target/release/freemkv /usr/local/bin/
```

### Requirements

- Linux (SG_IO SCSI passthrough)
- Root access (or user in `cdrom`/`optical` group with device permissions)
- An optical drive

macOS and Windows support is planned.

## Commands

### `freemkv info` — Identify your drive

```
$ sudo freemkv info

Drive Information
  Device:              /dev/sr0
  Vendor:              HL-DT-ST
  Product:             BD-RE BU40N
  Revision:            1.03
  Serial number:       MO6J7HB1010
  Firmware type:       NM00000
  Firmware date:       2018-10-24
  Bus encryption:      17

Profile Match
  Chipset:             MediaTek MT1959
  Unlock mode:         0x01 / 0x44
  Status:              Matched (ready to unlock)
```

Identifies your drive using standard SCSI commands (SPC-4 INQUIRY + MMC-6 GET CONFIGURATION 010C) and matches against 206 bundled profiles. No external files, no network access.

### `freemkv info --share` — Submit your drive profile

Captures raw SCSI response data and submits it as a GitHub issue. This is how new drives get added to the database.

```
$ sudo freemkv info --share

Probing /dev/sr0...
  INQUIRY:        96 bytes captured
  GET_CONFIG:     12 features captured
  MODE SENSE:     captured
  READ BUFFER:    captured

Submit this profile to help expand hardware support? [Y/n] Y
Profile submitted: https://github.com/freemkv/freemkv/issues/42
Done.
```

### `freemkv info --mask` — Mask serial numbers

Replaces serial numbers with format-preserving placeholders before display or sharing.

```
Serial: OEDL016822WL -> AAAA000000AA
```

Use with `--share` to submit a profile with masked serials: `freemkv info --share --mask`

### `freemkv rip` — Back up a disc

```bash
sudo freemkv rip --output /path/to/backups/
```

*Not yet implemented. Track progress at [github.com/freemkv/freemkv](https://github.com/freemkv/freemkv).*

## All Options

```
freemkv info                        Identify your drive
freemkv info --share                Submit drive profile
freemkv info --mask                 Mask serial numbers
freemkv info --share --mask         Submit with masked serials
freemkv info --device /dev/sgN      Specify device path
freemkv info --quiet                Minimal output

freemkv rip                         Back up a disc (planned)
freemkv rip --output /path          Specify output directory

freemkv version                     Show version
freemkv help                        Show help
```

## How It Works

freemkv uses [libfreemkv](https://github.com/freemkv/libfreemkv) for all hardware interaction. The library sends standard SCSI commands to identify the drive, matches it against 206 bundled profiles, and unlocks raw read mode with a single vendor-specific READ BUFFER command. No firmware modifications, no persistent changes to the drive.

## Contributing

Run `freemkv info --share` — that's the single most useful thing you can do. Every submitted profile expands hardware support.

Currently 206 drives supported across the MediaTek MT1959 chipset (LG, ASUS, HP). Renesas chipset (Pioneer) support is planned.

## License

AGPL-3.0-only
