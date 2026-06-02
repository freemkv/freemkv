[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)
[![crates.io](https://img.shields.io/crates/v/libfreemkv)](https://crates.io/crates/libfreemkv)

# freemkv

Open source 4K UHD / Blu-ray / DVD backup tool. Two arguments — source and destination. Stream URLs let you rip, remux, and transfer between any combination of disc, file, and network.

Built-in keys cover DVDs and Blu-rays (AACS 1.0) — no setup. For UHD (AACS 2.0 / 2.1) discs, an optional `keydb.cfg` supplies disc-specific volume unique keys.

## Quick Start

### 1. Install

```bash
# Linux
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo mv freemkv /usr/local/bin/

# macOS (Apple Silicon)
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-aarch64-apple-darwin.tar.gz | tar xz
sudo mv freemkv /usr/local/bin/

# macOS (Intel)
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-apple-darwin.tar.gz | tar xz
sudo mv freemkv /usr/local/bin/

# Windows — download .zip from https://github.com/freemkv/freemkv/releases/latest
```

### 2. Set up decryption keys (UHD discs only)

**DVD:** No setup needed. CSS decryption works out of the box.

**Blu-ray (AACS 1.0):** No setup needed. Built-in keys cover the full MKB range.

**4K UHD (AACS 2.0 / 2.1):** UHD discs use per-disc volume unique keys (VUKs), so freemkv reads them from an optional `keydb.cfg`. Fetch the latest one from a community source and save it to `~/.config/freemkv/keydb.cfg`, or point `update-keys` at a URL:

```bash
freemkv update-keys --url <keydb-url>
```

Once present, it's used automatically.

#### Operator-supplied keys

If you have additional device keys, processing keys, or VUKs you derived yourself, drop them in `~/.config/freemkv/local_keys.cfg`. Same format as `keydb.cfg`. The file is additive — its entries are merged on top of the built-in keys and any main `keydb.cfg`.

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
| ISO | Yes | Yes | `iso://path.iso` |
| MKV | Yes | Yes | `mkv://path` |
| M2TS | Yes | Yes | `m2ts://path` |
| Network | Yes (listen) | Yes (connect) | `network://host:port` |
| Stdio | Yes (stdin) | Yes (stdout) | `stdio://` |
| Null | -- | Yes | `null://` |

All URLs use the `scheme://path` format. No bare paths — always include the scheme prefix.

## Examples

### Rip a disc

```bash
freemkv disc:// mkv://Movie.mkv                     # All titles to MKV
freemkv disc:// mkv://Movie.mkv -t 1                # Main feature only
freemkv disc:// mkv://Movie.mkv -t 1 -t 3           # Titles 1 and 3
freemkv disc:// iso://Disc.iso                      # Full disc to ISO (decrypted)
freemkv disc:// iso://Disc.iso --raw                # Full disc to ISO (encrypted)
freemkv disc:///dev/sg4 mkv://Movie.mkv -t 1        # Specific drive
```

### Rip from ISO image

```bash
freemkv iso://Disc.iso mkv://Movie.mkv              # ISO to MKV
freemkv iso://Disc.iso mkv://Movie.mkv -t 1         # Main feature from ISO
```

### Remux between formats

```bash
freemkv m2ts://Movie.m2ts mkv://Movie.mkv           # m2ts to MKV
freemkv mkv://Movie.mkv m2ts://Movie.m2ts           # MKV to m2ts
```

### Network streaming (two machines)

Rip on a low-power machine with a disc drive, remux on a high-power server:

```
                           TCP
  [Ripper]  ──────────────────────►  [Transcoder]
  disc drive                          fast CPU
  freemkv disc://                     freemkv network://
    network://192.0.2.10:9000            0.0.0.0:9000 mkv://Movie.mkv
```

**On the transcoder** (start first — it listens):
```bash
freemkv network://0.0.0.0:9000 mkv://Movie.mkv
```

**On the ripper** (connects and streams):
```bash
freemkv disc:// network://192.0.2.10:9000
```

The metadata header flows first — labels, languages, duration, stream layout. The transcoder has everything it needs without touching the disc.

### Pipe to other tools

```bash
freemkv disc:// stdio:// | ffmpeg -i pipe:0 -c copy output.mkv
cat raw.m2ts | freemkv stdio:// mkv://Movie.mkv
```

### Benchmark read speed

```bash
freemkv disc:// null://
```

### Inspect metadata

```bash
freemkv info disc://                                # Disc info
freemkv info m2ts://Movie.m2ts                       # File metadata
freemkv info mkv://Movie.mkv                         # MKV track info
```

### Disc info

```
$ freemkv info disc://

Disc: Sample Film
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

### DVD disc info

```
$ freemkv info disc://

Disc: Greenland
Format: DVD (1L, 6.3 GB)
CSS: Encrypted

Titles

   1. VTS_02_3.VOB    1h 59m    5.8 GB  0 clips

      Video:     MPEG-2 480i 29.97fps

      Audio:     English DD
                 English DD
                 English DD

      Subtitle:  English
                 Spanish
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
-t, --title N       Select title (1-based, repeatable). Default: all.
-k, --keydb PATH    KEYDB.cfg path (optional; only required for UHD / AACS 2.0+ discs)
-v, --verbose       Show AACS/drive debug info
-q, --quiet         Suppress output
    --raw           Skip decryption (raw encrypted output)
-s, --share         Submit drive profile (with info disc://)
-m, --mask          Mask serial numbers (with --share)
```

## Multi-language

freemkv is fully localized. All output — errors, status, labels — adapts to your locale. Ships with English, Spanish, French, German, Italian, Portuguese, and Dutch. Contributions for additional languages welcome.

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
