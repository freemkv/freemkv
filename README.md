[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)
[![crates.io](https://img.shields.io/crates/v/libfreemkv)](https://crates.io/crates/libfreemkv)

# freemkv

Open source 4K UHD / Blu-ray / DVD backup tool. Two arguments — source and destination. Stream URLs let you rip, remux, and transfer between any combination of disc, file, and network.

## Quick Start

### 1. Install

```bash
# Linux
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-linux.tar.gz | tar xz
sudo mv freemkv /usr/local/bin/

# macOS
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-apple-darwin.tar.gz | tar xz
sudo mv freemkv /usr/local/bin/

# Windows — download .zip from https://github.com/freemkv/freemkv/releases/latest
```

### 2. Set up decryption keys (one time)

**DVD:** No setup needed. CSS decryption works out of the box.

**Blu-ray / 4K UHD:** You need a KEYDB.cfg file containing AACS decryption keys. This is a community-maintained key database — freemkv cannot ship keys due to legal restrictions.

```bash
freemkv update-keys --url <your-keydb-url>
```

The KEYDB is saved to `~/.config/freemkv/keydb.cfg` and used automatically from then on.

### 3. Rip

```bash
freemkv disc:// mkv://Movie.mkv            # Disc to MKV
freemkv disc:// m2ts://Movie.m2ts           # Disc to raw transport stream
freemkv m2ts://Movie.m2ts mkv://Movie.mkv   # Remux m2ts to MKV
freemkv info disc://                        # Show disc info
```

## How It Works

Every operation is `freemkv <source> <dest>`. Sources and destinations are stream URLs.

### Streams

| Stream | Input | Output | URL |
|--------|-------|--------|-----|
| Disc | Yes | -- | `disc://` or `disc:///dev/sg4` |
| ISO | Yes | -- | `iso://path.iso` |
| MKV | Yes | Yes | `mkv://path` |
| M2TS | Yes | Yes | `m2ts://path` |
| Network | Yes (listen) | Yes (connect) | `network://host:port` |
| Stdio | Yes (stdin) | Yes (stdout) | `stdio://` |
| Null | -- | Yes | `null://` |

All URLs use the `scheme://path` format. No bare paths — always include the scheme prefix.

## Examples

### Rip a disc

```bash
freemkv disc:// mkv://Dune.mkv                     # MKV output
freemkv disc:// m2ts://Dune.m2ts                    # Raw transport stream
freemkv disc:///dev/sg4 mkv://Dune.mkv              # Specific drive
freemkv disc:// mkv://Dune.mkv -t 2                 # Title 2
freemkv disc:// mkv:///media/movies/Dune.mkv        # Absolute path
```

### Rip from ISO image

```bash
freemkv iso://Dune.iso mkv://Dune.mkv               # ISO → MKV
freemkv iso://Dune.iso m2ts://Dune.m2ts              # ISO → m2ts
```

### Remux between formats

```bash
freemkv m2ts://Dune.m2ts mkv://Dune.mkv             # m2ts → MKV
freemkv mkv://Dune.mkv m2ts://Dune.m2ts             # MKV → m2ts
```

### Network streaming (two machines)

Rip on a low-power machine with a disc drive, remux on a high-power server:

```
                           TCP
  [Ripper]  ──────────────────────►  [Transcoder]
  disc drive                          fast CPU
  freemkv disc://                     freemkv network://
    network://10.1.7.11:9000            0.0.0.0:9000 mkv://Dune.mkv
```

**On the transcoder** (start first — it listens):
```bash
freemkv network://0.0.0.0:9000 mkv://Dune.mkv
```

**On the ripper** (connects and streams):
```bash
freemkv disc:// network://10.1.7.11:9000
```

The metadata header flows first — labels, languages, duration, stream layout. The transcoder has everything it needs without touching the disc.

### Pipe to other tools

```bash
freemkv disc:// stdio:// | ffmpeg -i pipe:0 -c copy output.mkv
cat raw.m2ts | freemkv stdio:// mkv://Dune.mkv
```

### Benchmark read speed

```bash
freemkv disc:// null://
```

### Inspect metadata

```bash
freemkv info disc://                                # Disc info
freemkv info m2ts://Dune.m2ts                       # File metadata
freemkv info mkv://Dune.mkv                         # MKV track info
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

Works with LG, ASUS, HP, and other MediaTek-based BD-RE drives on Linux, macOS, and Windows. Run `freemkv info disc://` to check. Pioneer support planned.

## Contributing

Run `freemkv info disc:// --share` to submit your drive's profile and help expand hardware support.

## License

AGPL-3.0-only. Built on [libfreemkv](https://github.com/freemkv/libfreemkv).
