# Changelog

All notable changes to the `freemkv` CLI are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and the
project follows semantic versioning.

## [1.0.0-rc.2]

Second release candidate for 1.0. Adds keyless DVD/CSS ripping and correct DVD
video output, on top of release hardening.

### Added

- Keyless DVD/CSS ripping: a CSS-protected DVD decrypts with no key database, so
  `disc://` to `mkv://` works out of the box on a DVD. Muxing a raw,
  still-scrambled CSS ISO (`iso://`) works too — the title key is recovered from
  the image directly, no pre-decryption step.
- `--log-level N` sets log verbosity (1 = warnings/errors, up to 4 = trace) and
  `--log-file PATH` adds a non-blocking file sink alongside stderr; `RUST_LOG`
  overrides. Logs go to stderr so stdout stays pipe-clean for
  `mkv://`/`m2ts://`.
- Static-binary releases: each tagged release attaches a single static binary
  per platform (Linux x86_64/aarch64, macOS Intel/Silicon, Windows) with a
  `.sha256` checksum, alongside the source archives. See `INSTALL.md`.

### Changed

- Correct DVD video output via libfreemkv's MPEG-2 Program-Stream access-unit
  reassembler (one coded picture per MKV block with reconstructed timestamps),
  replacing the previous per-PES framing that produced corrupted DVD video.
- MKV output records `freemkv <version>` in the Muxing/Writing application
  fields.
- Built on libfreemkv 1.0.0-rc.2: HEVC/H.264/VC-1 param-set keyframe
  correctness, short-read rejection, `BlockDuration` timescale fix, and content-
  key redaction in logs.

## [1.0.0-rc.1]

First release candidate for 1.0 — the first tagged 1.0 milestone of the CLI,
establishing the stream-URL command surface (see "Pre-1.0 development").

## Pre-1.0 development

Versions 0.x were the development series leading up to 1.0. The highlights,
condensed:

- **Stream-URL CLI.** `freemkv <source> <dest>` over `disc://`, `iso://`,
  `mkv://`, `m2ts://`, `network://`, `stdio://`, and `null://`, with `info` for
  disc and file metadata and `update-keys` for fetching a keydb.
- **Correct, safe output.** A resolved decryption key is verified against disc
  content before muxing, so a stale or wrong key can't silently produce garbage;
  `iso://` mux fails fast with a clear message when no usable key is available;
  `info iso://` lists titles without requiring a key. A multi-title disc rip to
  a single-file destination is rejected.
- **Hardening.** Robust argument parsing (unknown flags error, flags before URLs
  are not mistaken for the URL), TOML-safe `info`/`drive.toml` writing, and
  `sigaction`-based SIGINT so a second Ctrl-C reliably exits.
- **Build.** Release profile uses thin LTO and a single codegen unit. Tracks the
  matching libfreemkv recovery and mux improvements throughout.
