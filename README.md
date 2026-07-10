[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)

# freemkv

Open source 4K UHD / Blu-ray / DVD backup tool. Two arguments — source and destination. Stream URLs let you rip, remux, and transfer between any combination of disc, file, and network.

DVDs (CSS) need no setup. Blu-ray and UHD (AACS) require a `keydb.cfg` supplying disc-specific volume unique keys.

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

Prefer a single plain binary (no archive) with a sha256 checksum? Each
release also publishes unwrapped, ready-to-run binaries like
`freemkv-x86_64-linux`. See [INSTALL.md](INSTALL.md) for the full list,
checksum verification, and per-platform steps.

### 2. Set up decryption keys (UHD discs only)

**DVD:** No setup needed. CSS decryption works out of the box.

**Blu-ray + UHD (AACS):** Require a `keydb.cfg` (default `~/.config/freemkv/keydb.cfg`) holding the disc keys; no AACS keys are compiled in.

**4K UHD (AACS 2.0 / 2.1):** UHD discs use per-disc volume unique keys (VUKs), so freemkv reads them from an optional `keydb.cfg`. Fetch the latest one from a community source and save it to `~/.config/freemkv/keydb.cfg`, or point `update-keys` at a URL:

```bash
freemkv update-keys --url <keydb-url>
```

Once present, it's used automatically.

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
    --log-level N   Log verbosity: 1 = warnings/errors only (default), 2 = info,
                    3 = debug, 4 = trace. At level ≥2 also widens human stdout detail.
    --log-file PATH Also write the full log to PATH (for bug reports)
-q, --quiet         Suppress output
    --raw           Skip decryption (raw encrypted output)
-s, --share         Submit drive profile (with info disc://)
-m, --mask          Mask serial numbers (with --share)
```

Logging goes to **stderr** (so stdout stays clean for piping to `mkv://`/`m2ts://`).
`RUST_LOG` overrides `--log-level` if set.

## Getting a debug log (for bug reports)

If something fails or hangs, re-run with `--log-level 3` and capture the log to a
file:

```bash
freemkv --log-level 3 --log-file freemkv-debug.log disc:// mkv://"Movie.mkv"
```

Level 3 (debug) is recommended for bug reports — comprehensive diagnostics at a
manageable size. On a successful rip the debug log looks similar to info; the extra
detail appears on the failure path (CSS auth, retries, read errors, mux-stage decisions,
stalls) — which is exactly when you need it. If a maintainer asks for maximum detail,
use `--log-level 4` (trace), but note it's a per-sector firehose that can be gigabytes
on a long rip.

If it hangs, let it sit ~30 s, then Ctrl-C — the last `phase=… "alive"` line names
the exact stage and position (e.g. LBA) where it stuck. Attach `freemkv-debug.log`
to your report. **Keys are never written to logs** (the log contains paths and disc
metadata, never CSS/AACS key material).

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

MIT. Built on [libfreemkv](https://github.com/freemkv/libfreemkv).
