[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)
[![crates.io](https://img.shields.io/crates/v/libfreemkv)](https://crates.io/crates/libfreemkv)

# freemkv

Open source 4K UHD / Blu-ray / DVD backup tool. One binary, no dependencies.

## Quick Start

**Linux:**
```bash
wget -qO- https://github.com/freemkv/freemkv/releases/latest/download/freemkv-linux-x86_64.tar.gz | tar xz
./freemkv disc-info
```

**macOS:**
```bash
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-macos-arm64.tar.gz | tar xz
./freemkv disc-info
```

**Windows:** Coming soon.

Or install from source: `cargo install freemkv`

## Example Output

```
freemkv 0.3.0

Scanning disc...

Disc: Barbie
Format: 4K UHD (2L, 76.4 GB)
AACS: Encrypted

Titles

   1. 00350.mpls      1h 54m   69.0 GB  1 clip

      Video:     HEVC 2160p HDR10 BT.2020

      Audio:     English TrueHD 5.1 (TrueHD)
                 English DD 5.1 (Dolby Digital)
                 English DD 5.1 (Descriptive Audio (US))
                 English DD 5.1 (Descriptive Audio (UK))
                 French DD 5.1
                 Italian TrueHD 5.1 (TrueHD)
                 Italian DD 5.1 (Dolby Digital)
                 Spanish DD 5.1

      Subtitle:  English
                 French
                 Italian
                 Spanish
                 Danish (forced)
                 Finnish (forced)
                 Norwegian (forced)
```

## Stream Labels

freemkv automatically extracts rich stream metadata that other tools can't see. Standard Blu-ray tools only read MPLS data (language code + codec). freemkv goes further — it reads BD-J authoring files on the disc to identify:

- **Audio purpose** — Commentary, Descriptive Audio, Score, IME
- **Codec detail** — TrueHD, Dolby Digital, Dolby Atmos, DTS
- **Forced subtitles** — which subtitle tracks are forced/narrative
- **Language variants** — US vs UK English, Castilian vs Latin Spanish
- **SDH** — subtitles for deaf/hard of hearing

Five BD-J format parsers are built in (Paramount, Criterion, Pixelogic, Warner CTRM, Deluxe). Detection is automatic — the right parser runs based on what files exist on the disc.

If no BD-J data is found, streams still have full MPLS data (language, codec, channels). Labels are purely additive.

## Commands

```
freemkv <command> [options]

  drive-info            Show drive hardware and profile
  disc-info             Show disc titles, streams, and sizes
  rip [--output /path]  Back up a disc (coming soon)
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
