[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)
[![crates.io](https://img.shields.io/crates/v/libfreemkv)](https://crates.io/crates/libfreemkv)

# freemkv

Open source 4K UHD / Blu-ray / DVD backup tool. One binary, no dependencies. Multi-lingual — the library outputs structured data and error codes, not English text. Build any UI on top.

## Quick Start

**Linux (x86_64):**
```bash
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-linux.tar.gz | tar xz
./freemkv disc-info
```

**macOS (Apple Silicon):**
```bash
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-aarch64-macos.tar.gz | tar xz
./freemkv disc-info
```

**Windows:** Coming soon.

Or install from source: `cargo install freemkv`

## disc-info

```
$ freemkv disc-info

freemkv 0.6.0

Scanning disc...

Disc: Dune
Format: 4K UHD (2L, 90.7 GB)
AACS: Encrypted

Titles

   1. 00800.mpls      2h 35m   88.8 GB  1 clip

      Video:     HEVC 2160p HDR10 BT.2020
                 HEVC 1080p Dolby Vision BT.2020 Dolby Vision EL

      Audio:     English TrueHD 5.1
                 English DD 5.1
                 French DD 5.1
                 German TrueHD 5.1
                 Italian TrueHD 5.1
                 Spanish DD 5.1
                 Hindi DD 5.1

      Subtitle:  English
                 French
                 German
                 Italian
                 Spanish
                 Chinese
                 Korean

      +2 more (use --full to show all)
```

## drive-info

```
$ freemkv drive-info

freemkv 0.6.0

Drive Information
  Device:              /dev/sg4
  Manufacturer:        HL-DT-ST
  Product:             BD-RE BU40N
  Revision:            1.03
  Serial number:       MO6J7HB1010
  Firmware date:       2018-10-24

Platform Information
  Drive platform:      MediaTek MT1959
  Firmware version:    1.03/NM00000
  Profile:             Supported

Run 'freemkv drive-info --share' to help expand drive support.
```

## rip

```
$ freemkv rip --output ~/Movies/

freemkv rip v0.6.0

Opening /dev/sg4... OK
  HL-DT-ST BD-RE BU40N
Waiting for disc... OK
Initializing drive... OK
Probing disc... OK
Scanning disc... OK

  Capacity: 90.7 GB
  AACS:     encrypted (keys found)

Ripping title 1 (2h 35m, 88.8 GB) -> Dune.mkv
  22.1 GB / 88.8 GB  (25%)  17.2 MB/s  ETA 65:23
```

## Stream Labels

freemkv automatically extracts rich stream metadata that other tools can't see. Standard tools only read MPLS data (language code + codec). freemkv reads BD-J authoring files on the disc to identify:

- **Audio purpose** — Commentary, Descriptive Audio, Score
- **Codec detail** — TrueHD, Dolby Digital, Dolby Atmos
- **Forced subtitles** — which tracks are forced/narrative
- **Language variants** — US vs UK English, Castilian vs Latin Spanish
- **SDH** — subtitles for deaf/hard of hearing

Five BD-J format parsers built in (Paramount, Criterion, Pixelogic, Warner CTRM, Deluxe). Detection is automatic.

## Commands

```
freemkv <command> [options]

  drive-info            Show drive hardware and profile match
  disc-info             Show disc titles, streams, and sizes
  rip [options]         Back up a disc title (MKV default, --raw for m2ts)
  remux <in.m2ts>       Convert m2ts to MKV (no drive needed)
  update-keys --url <u> Download and update KEYDB.cfg

Rip options:
  -d, --device /dev/sgN   Specify device (default: auto-detect)
  -k, --keydb /path       Path to KEYDB.cfg for AACS decryption
  -o, --output /path      Output directory
  -t, --title N           Title number (default: 1 = main feature)
  -l, --list              List titles only, don't rip
      --raw               Output raw m2ts instead of MKV

Drive-info options:
  -s, --share             Capture and submit drive profile
  -m, --mask              Mask serial numbers

Global options:
  -q, --quiet             Suppress output
```

## Supported Drives

Works with LG, ASUS, HP, and other MediaTek-based BD-RE drives. Run `freemkv drive-info` to check your drive. Pioneer support planned.

## Contributing

Run `freemkv drive-info --share` to submit your drive's profile and help expand hardware support.

## License

AGPL-3.0-only. Built on [libfreemkv](https://github.com/freemkv/libfreemkv).
