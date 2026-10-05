[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-ea4aaa?logo=github-sponsors)](https://github.com/sponsors/MattJackson)
[![Website](https://img.shields.io/badge/website-freemkv.org-2ea44f)](https://freemkv.org)
[![Discord](https://img.shields.io/badge/Discord-join%20chat-5865F2?logo=discord&logoColor=white)](https://discord.gg/yu7xMGTyek)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![CI](https://github.com/freemkv/freemkv/actions/workflows/ci.yml/badge.svg)](https://github.com/freemkv/freemkv/actions/workflows/ci.yml)
[![Latest Release](https://img.shields.io/github/v/release/freemkv/freemkv?label=latest&color=brightgreen)](https://github.com/freemkv/freemkv/releases/latest)
[![codecov](https://codecov.io/gh/freemkv/freemkv/branch/dev/graph/badge.svg)](https://codecov.io/gh/freemkv/freemkv)
[![OpenSSF Best Practices](https://www.bestpractices.dev/projects/14740/badge)](https://www.bestpractices.dev/projects/14740)
[![OpenSSF Scorecard](https://api.securityscorecards.dev/projects/github.com/freemkv/freemkv/badge)](https://scorecard.dev/viewer/?uri=github.com/freemkv/freemkv)
[![REUSE status](https://api.reuse.software/badge/github.com/freemkv/freemkv)](https://api.reuse.software/info/github.com/freemkv/freemkv)
[![MSRV](https://img.shields.io/badge/MSRV-1.98-blue.svg)](#minimum-supported-rust-version)

# freemkv

Open source 4K UHD / Blu-ray / DVD backup tool — a **command-line tool and a native desktop app** for macOS, Windows and Linux. On the command line it's two arguments — source and destination; stream URLs let you rip, remux, and transfer between any combination of disc, file, and network. Or open the desktop app: pick titles and tracks, choose a format, press Rip. Same engine either way.

Two builds ship, both installed as `freemkv`: the **app** (the window, plus the full CLI for any command) and the **CLI** (command line only, no UI libraries). Install one or the other.

DVDs (CSS) need no setup. Blu-ray and UHD (AACS) require a `keydb.cfg` supplying disc-specific volume unique keys.

## Quick Start

### 1. Install

**App:** download from the [latest release](https://github.com/freemkv/freemkv/releases/latest) —
`freemkv-<arch>-macos.zip` on macOS, `freemkv-<arch>-windows-setup.exe` (x86_64 or aarch64) on Windows,
`freemkv-amd64.deb` / `freemkv-arm64.deb`, AppImage, Flatpak or Snap (`freemkv-amd64.snap`) on Linux. On macOS: `brew install --cask freemkv/tap/freemkv`.

**CLI:**

```bash
# macOS
brew install freemkv/tap/freemkv-cli

# Linux x86_64 (static; freemkv-cli-aarch64-linux on arm64, freemkv-cli-armv7-linux on 32-bit Raspberry Pi)
curl -fsSLO https://github.com/freemkv/freemkv/releases/latest/download/freemkv-cli-x86_64-linux
curl -fsSLO https://github.com/freemkv/freemkv/releases/latest/download/freemkv-cli-x86_64-linux.sha256
sha256sum -c freemkv-cli-x86_64-linux.sha256
mv freemkv-cli-x86_64-linux freemkv && chmod +x freemkv && sudo mv freemkv /usr/local/bin/

# Windows — download freemkv-cli-x86_64-windows.exe (freemkv-cli-aarch64-windows.exe on ARM),
# rename to freemkv.exe, put it on PATH
```

See [INSTALL.md](INSTALL.md) for every asset, the `freemkv-cli` `.deb`, checksum
verification, and per-platform steps.

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
| Folder | Yes | Yes | `dir://path/` — decrypted file tree (`VIDEO_TS` / `BDMV`) |
| MKV | Yes | Yes | `mkv://path` |
| M2TS | Yes | Yes | `m2ts://path` |
| MP4 | Yes | Yes | `mp4://path` |
| MPG | Yes | Yes | `mpg://path.mpg` — MPEG program stream (DVD `.VOB`/`.mpg`); writes MPEG-1/2 video |
| Network | Yes (listen) | Yes (connect) | `network://host:port` |
| Stdio | Yes (stdin) | Yes (stdout) | `stdio://` |
| Null | -- | Yes | `null://` |

Extraction sinks write parts of a title rather than a container: `demux://`
(every track as elementary streams), `video://`, `audio://`, `sub://`,
`chapters://` (Matroska XML), `json://` (title structure) and `fvi://` (a
per-picture video index).

All URLs use the `scheme://path` format. No bare paths — always include the scheme prefix.

## Examples

### Rip a disc

```bash
freemkv disc:// mkv://Movie.mkv                     # Main feature (the default since 1.6.0)
freemkv disc:// mkv://Movie.mkv -t 1                # Title 1 explicitly
freemkv disc:// mkv://out.mkv -t 1 -t 3             # Titles 1 and 3: out_t1.mkv and out_t3.mkv beside out.mkv
freemkv disc:// mkv://out/ -t all                   # Every title
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
-t, --title N       Select title (1-based, repeatable). Default: the main
                    title only; use `-t all` for every title.
    --keydb PATH    KEYDB.cfg path (optional; only required for UHD / AACS 2.0+ discs)
    --log-level N   Log verbosity: 1 = warnings/errors only (default), 2 = info,
                    3 = debug, 4 = trace. At level ≥2 also widens human stdout detail.
    --log-file PATH Also write the full log to PATH (for bug reports)
-q, --quiet         Suppress output
    --language CODE Interface language, e.g. `de`, `pt-BR`, or `auto` to follow
                    the environment. `--lang` is an accepted alias. Must appear
                    before the language is needed — it is read out of argv
                    before anything else, so it can go anywhere on the line.
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

freemkv is fully localized. All output — errors, status, labels — adapts to your locale. Ships with 29 complete translations: Catalan, Czech, Danish, Dutch, English, Finnish, French, German, Greek, Hungarian, Indonesian, Italian, Japanese, Korean, Norwegian, Polish, Portuguese (European and Brazilian), Romanian, Russian, Simplified and Traditional Chinese, Slovak, Spanish (European and Latin American), Swedish, Turkish, Ukrainian, and Vietnamese. Contributions for additional languages welcome.

## Building from Source

freemkv isn't on crates.io; install straight from a release tag:
```bash
cargo install --locked --git https://github.com/freemkv/freemkv --tag vX.Y.Z freemkv                   # CLI
cargo install --locked --git https://github.com/freemkv/freemkv --tag vX.Y.Z --features gui freemkv    # app
```

Or clone and build:
```bash
git clone https://github.com/freemkv/freemkv
cd freemkv
cargo build --release                   # CLI
cargo build --release --features gui    # app (Linux: needs libgtk-4-dev libadwaita-1-dev)
```

## Supported Drives

Works with LG, ASUS, HP, and other MediaTek-based BD-RE drives on Linux, macOS, and Windows. Run `freemkv info disc://` to check. Pioneer support planned.

## Contributing

Run `freemkv info disc:// --share` to submit your drive's profile and help expand hardware support.

## Project quality

freemkv pursues the [OpenSSF Best Practices](https://www.bestpractices.dev/projects/14740)
badge. Quality is enforced in CI on every change: `cargo fmt --check`,
`cargo clippy --all-targets -D warnings`, the test suite, `cargo-deny` dependency
scanning, and code coverage via [Codecov](https://codecov.io/gh/freemkv/freemkv).
Releases are code-signed with SHA-256 checksums. See
[CONTRIBUTING.md](CONTRIBUTING.md), [GOVERNANCE.md](GOVERNANCE.md),
[ROADMAP.md](ROADMAP.md), and [SECURITY.md](SECURITY.md).

## Minimum Supported Rust Version

The minimum supported Rust version (MSRV) is **1.98**, declared as
`rust-version` in [`Cargo.toml`](Cargo.toml) and enforced in CI on every change.

## License

MIT. Built on [libfreemkv](https://github.com/freemkv/libfreemkv).

### Media validation

QA runs fixed synthetic media through the CLI on Linux, macOS and Windows.
It checks decoded content, frame/sample counts, timestamps, track metadata and
PGS display/replacement/clear events against committed answers, then compares
platform results. A small DVD image also exercises disc parsing and decoding.
The full DVD/BD/UHD/HD-DVD suite is opt-in through QA's `run_media` input.

With FFmpeg installed, run `python3 tests/media_kat.py --binary target/release/freemkv`.
Run validator regression tests with `python3 -m unittest discover -s tests -p 'test_media*.py'`.
Fixtures contain only generated pixels/audio/subtitles. `--generate` explicitly
rebuilds their answers from authored inputs and FFmpeg, never freemkv output;
normal validation never rewrites them. Review regenerated answers before committing.

Flatpak packaging is generated from the current QA candidate and from matching
version tags for releases. The `Flatpak` workflow builds offline, checks the
installed version and GUI startup, and retains the manifest, vendored sources,
lockfile and commit provenance. Stable releases also carry the `.flatpak` and
an offline build package; Flathub acceptance is not required for these builds.

The `Snap` workflow builds the snap on every push, installs it and checks the
version, CLI mode, GUI startup and declared interfaces. Stable releases carry
`freemkv-amd64.snap`; store publishing is described in
[packaging/snap/README.md](packaging/snap/README.md).
