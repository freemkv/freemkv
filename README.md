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

freemkv 0.5.0

Scanning disc...

Disc: V For Vendetta
Format: Blu-ray (1L, 25.5 GB)
AACS: Encrypted

Titles

   1. 00003.mpls      2h 12m   21.3 GB  1 clip

      Video:     VC-1 1080p

      Audio:     English DD 5.1 (AudioEnglish)
                 English TrueHD 5.1 (AudioEnglishDolby)
                 French DD 5.1 (AudioFrenchHD)
                 French DD 5.1 (AudioFrench)
                 German DD 5.1 (AudioDeutsch)
                 Italian DD 5.1 (AudioItaliano)
                 Spanish DD 5.1 (AudioCastellano)
                 Japanese DD 5.1

      Subtitle:  English
                 French
                 German
                 German
                 Italian
                 Italian
                 Spanish
```

## drive-info

```
$ freemkv drive-info

freemkv 0.5.0

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

Run 'freemkv drive-info --share' to help expand drive support.
```

## rip

```
$ freemkv rip --output ~/Movies/

freemkv rip v0.5.0

Opening /dev/sg4... OK
  HL-DT-ST BD-RE BU40N
Waiting for disc... OK
Initializing drive... OK
Probing disc... OK
Scanning disc... OK

  Capacity: 25.5 GB (13368800 sectors)
  AACS:     encrypted (keys found)

Ripping title 1 (2h 12m) -> ~/Movies/V For Vendetta_t01.m2ts
  1 extent(s), 21.3 GB
  8532 MB / 21767 MB  (39%)  13.4 MB/s (cur: 15.8)  ETA 16:28
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

  drive-info            Show drive hardware and profile
  disc-info             Show disc titles, streams, and sizes
  rip [--output /path]  Back up a disc (12+ MB/s on BD)
  bench-speed           Test drive read speed
  version               Show version

Options:
  --device /dev/sgN     Specify device (default: auto-detect)
  --keydb /path         Path to KEYDB.cfg for AACS decryption
```

## Supported Drives

Works with LG, ASUS, HP, and other MediaTek-based BD-RE drives. Run `freemkv drive-info` to check your drive. Pioneer support planned.

## Contributing

Run `freemkv drive-info --share` to submit your drive's profile and help expand hardware support.

## License

AGPL-3.0-only. Built on [libfreemkv](https://github.com/freemkv/libfreemkv).
