[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)
[![crates.io](https://img.shields.io/crates/v/libfreemkv)](https://crates.io/crates/libfreemkv)

# freemkv

Open source 4K UHD / Blu-ray / DVD backup tool. Two arguments — source and destination. Stream URLs let you rip, remux, and transfer between any combination of disc, file, and network.

## Quick Start

```bash
# Linux
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-linux.tar.gz | tar xz

# Rip a disc to MKV
./freemkv disc:// Dune.mkv

# Rip to raw transport stream
./freemkv disc:// Dune.m2ts

# Remux a file
./freemkv Dune.m2ts Dune.mkv

# Show disc info
./freemkv info disc://
```

## How It Works

Every operation is `freemkv <source> <dest>`. Sources and destinations are stream URLs:

| URL | Direction | Description |
|-----|-----------|-------------|
| `disc://` | Read | Optical drive (auto-detect) |
| `disc:///dev/sg4` | Read | Optical drive (specific device) |
| `Dune.mkv` | Read/Write | Matroska file |
| `Dune.m2ts` | Read/Write | BD transport stream file |
| `network://host:9000` | Read/Write | TCP stream |

Bare file paths infer the format from the extension.

## Examples

### Rip a disc

```bash
freemkv disc:// Dune.mkv                     # MKV output
freemkv disc:// Dune.m2ts                     # Raw transport stream
freemkv disc:///dev/sg4 Dune.mkv              # Specific drive
freemkv disc:// Dune.mkv -t 2                 # Title 2
```

### Remux between formats

```bash
freemkv Dune.m2ts Dune.mkv                   # m2ts → MKV
```

### Network streaming (two machines)

Rip on a low-power machine with a disc drive, remux on a high-power server:

```
                           TCP
  [Ripper]  ──────────────────────►  [Transcoder]
  disc drive                          fast CPU
  freemkv disc://                     freemkv network://
    network://10.1.7.11:9000            0.0.0.0:9000 Dune.mkv
```

**On the transcoder** (start first — it listens):
```bash
freemkv network://0.0.0.0:9000 Dune.mkv
```

**On the ripper** (connects and streams):
```bash
freemkv disc:// network://10.1.7.11:9000
```

The metadata header flows first — labels, languages, duration, stream layout. The transcoder has everything it needs without touching the disc.

### Inspect metadata

```bash
freemkv info disc://                          # Disc info
freemkv info Dune.m2ts                        # File metadata
freemkv info Dune.mkv                         # MKV track info
```

### Disc info

```
$ freemkv info disc://

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

      Subtitle:  English
                 French
                 German
```

## Stream Labels

freemkv reads BD-J authoring files on the disc — metadata that other tools can't see. Standard tools only read MPLS data (language code + codec). freemkv identifies:

- **Audio purpose** — Commentary, Descriptive Audio, Score
- **Codec detail** — TrueHD, Dolby Atmos, DTS-HD MA
- **Forced subtitles** — narrative/foreign language tracks
- **Language variants** — US vs UK English, Castilian vs Latin Spanish

Labels are preserved in all output formats — MKV track names and M2TS metadata headers carry them through.

## Flags

```
-t, --title N       Which title (default: longest)
-k, --keydb PATH    KEYDB.cfg path
-v, --verbose       AACS debug info
-q, --quiet         Suppress output
-l, --list          List titles only (with disc://)
-s, --share         Submit drive profile (with info disc://)
-m, --mask          Mask serial numbers
```

## Building from Source

```bash
cargo install freemkv
```

Or clone and build:
```bash
git clone https://github.com/freemkv/freemkv
cd freemkv/freemkv
cargo build --release
```

## Supported Drives

Works with LG, ASUS, HP, and other MediaTek-based BD-RE drives. Run `freemkv info disc://` to check. Pioneer support planned.

## Contributing

Run `freemkv info disc:// --share` to submit your drive's profile and help expand hardware support.

## License

AGPL-3.0-only. Built on [libfreemkv](https://github.com/freemkv/libfreemkv).
