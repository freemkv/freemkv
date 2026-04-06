[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)

# freemkv

Open source 4K UHD / Blu-ray / DVD backup tool for Linux. Built in Rust.

Part of the [freemkv](https://github.com/freemkv) project.

## Install

```bash
cargo build --release
sudo cp target/release/freemkv /usr/local/bin/
```

## Commands

### `freemkv info` — Drive information

Show your optical drive's details and platform.

```
$ freemkv info

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
```

### `freemkv info --share` — Capture drive profile

Capture your drive's SCSI response data for [bdemu](https://github.com/freemkv/bdemu).

```bash
freemkv info --share profiles/
```

This creates a profile directory with raw SCSI response data that can be used to emulate your drive without real hardware.

### `freemkv info --mask` — Mask serial numbers

Replace serial numbers with format-preserving placeholders (letters → A, digits → 0).

```
Serial: OEDL016822WL → AAAA000000AA
```

Works with both display and `--share`.

### `freemkv rip` — Back up a disc

```bash
freemkv rip disc:0 --output /path/to/output/
```

*Not yet implemented. Track progress at https://github.com/freemkv/freemkv*

## All Options

```
freemkv info                        Show drive information
freemkv info --share [dir]          Capture drive profile
freemkv info --mask                 Mask serial numbers
freemkv info --share --mask         Capture with masked serials
freemkv info --device /dev/sgN      Specify device
freemkv info --quiet                Minimal output

freemkv rip disc:0                  Back up a disc (planned)
freemkv rip disc:0 --output /path   Specify output directory

freemkv version                     Show version
freemkv help                        Show help
```

## Contributing

Contributions welcome. Run `freemkv info --share` to submit your drive's profile and help expand hardware support.

## License

AGPL-3.0