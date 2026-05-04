<p align="center">
  <img src="https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen" alt="Latest release">
  <img src="https://img.shields.io/crates/v/libfreemkv" alt="crates.io">
  <img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="AGPL-3.0">
  <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows-lightgrey" alt="Linux | macOS | Windows">
</p>

# freemkv

Open source 4K UHD / Blu-ray / DVD backup. One binary, no dependencies. Plug in your drive, rip to MKV.

Stream labels extracted automatically — audio purpose, codec detail, forced subtitles, language variants — metadata other tools miss. Bundled drive profiles. 17+ MB/s sustained.

---

## Quick Start

### 1. Install

**Linux:**
```bash
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo mv freemkv /usr/local/bin/
```

**macOS (Apple Silicon):**
```bash
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-aarch64-apple-darwin.tar.gz | tar xz
sudo mv freemkv /usr/local/bin/
```

**macOS (Intel):**
```bash
curl -sL https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-apple-darwin.tar.gz | tar xz
sudo mv freemkv /usr/local/bin/
```

**Windows:** Download [freemkv-x86_64-pc-windows-msvc.zip](https://github.com/freemkv/freemkv/releases/latest/download/freemkv-x86_64-pc-windows-msvc.zip), extract, run from Command Prompt.

**From source:** `cargo install freemkv`

[All downloads](https://github.com/freemkv/freemkv/releases)

### 2. Set up decryption keys (one time)

**DVD:** No setup needed. CSS decryption works out of the box.

**Blu-ray / 4K UHD:** You need a KEYDB.cfg — a community-maintained AACS key database. freemkv cannot ship keys due to legal restrictions (same as every other BD ripping tool).

```bash
freemkv update-keys --url <your-keydb-url>
```

Saved to `~/.config/freemkv/keydb.cfg` and used automatically from then on.

### 3. Rip

```bash
freemkv disc:// mkv://Movie.mkv
```

That's it. Scans the disc, decrypts, muxes to MKV with all streams and labels.

---

## Docker Autorip

Insert a disc, walk away. Unattended ripping with a web dashboard.

```bash
docker pull ghcr.io/freemkv/autorip:latest
```

```yaml
services:
  autorip:
    image: ghcr.io/freemkv/autorip:latest
    devices:
      - /dev/sr0:/dev/sr0
    ports:
      - 8080:8080
    volumes:
      - ./config:/config
      - /mnt/media:/output
    environment:
      - TMDB_API_KEY=your_key
    privileged: true
```

Open `localhost:8080`. Configure KEYDB URL and TMDB key in Settings. Insert disc — it rips, looks up the title, organizes into `Movies/Title (Year)/Title.mkv`, ejects, waits for the next one.

---

## Why freemkv

- **Open source** — pure Rust, AGPL-3.0, library on [crates.io](https://crates.io/crates/libfreemkv)
- **It sees more** — BD-J parsers extract stream labels other tools miss
- **It's fast** — firmware upload removes riplock, adaptive batch sizing, 17+ MB/s
- **It decrypts** — AACS 1.0 + 2.0, CSS — all transparent and automatic
- **It streams** — disc, file, ISO, network, stdin/stdout — any source to any destination
- **It automates** — Docker container with web UI, TMDB, webhooks

---

## Streams

| Stream | Input | Output | URL |
|--------|-------|--------|-----|
| Disc | Yes | -- | `disc://` or `disc:///dev/sg4` |
| ISO | Yes | Yes | `iso://image.iso` |
| MKV | Yes | Yes | `mkv://path` |
| M2TS | Yes | Yes | `m2ts://path` |
| Network | Yes (listen) | Yes (connect) | `network://host:port` |
| Stdio | Yes (stdin) | Yes (stdout) | `stdio://` |
| Null | -- | Yes | `null://` |

### Examples

```bash
freemkv disc:// mkv://Dune.mkv                     # Rip to MKV
freemkv disc:// m2ts://Dune.m2ts                    # Rip to transport stream
freemkv iso://Dune.iso mkv://Dune.mkv               # ISO to MKV
freemkv m2ts://Dune.m2ts mkv://Dune.mkv             # Remux
freemkv disc:// network://10.1.7.11:9000            # Stream over network
freemkv network://0.0.0.0:9000 mkv://Dune.mkv      # Receive from network
freemkv disc:// stdio:// | ffmpeg -i pipe:0 ...     # Pipe to ffmpeg
freemkv info disc://                                 # Show disc info
```

---

## Repos

| | |
|-|-|
| [**freemkv**](https://github.com/freemkv/freemkv) | CLI tool — all commands, flags, streaming examples |
| [**libfreemkv**](https://github.com/freemkv/libfreemkv) | Rust library — API, 7 stream types, architecture, error codes. [crates.io](https://crates.io/crates/libfreemkv) |
| [**autorip**](https://github.com/freemkv/autorip) | Automatic ripper — Docker, web UI, TMDB, webhooks. [ghcr.io](https://ghcr.io/freemkv/autorip) |
| [**bdemu**](https://github.com/freemkv/bdemu) | Drive emulator — develop and test without real hardware |

Supports LG, ASUS, HP, and other MediaTek-based BD-RE drives. Linux, macOS, and Windows. Pioneer planned.

### Help expand drive support

```bash
freemkv info disc:// --share
```

Captures your drive's hardware profile and submits it as a GitHub issue. No disc data, no personal info, no keys. Use `--mask` to anonymize serial numbers.

---

<p align="center">
  <a href="https://github.com/freemkv/freemkv">CLI</a> ·
  <a href="https://github.com/freemkv/libfreemkv">Library</a> ·
  <a href="https://github.com/freemkv/autorip">Autorip</a> ·
  <a href="https://github.com/freemkv/bdemu">Emulator</a> ·
  <a href="https://www.gnu.org/licenses/agpl-3.0.txt">AGPL-3.0</a>
</p>
