# Changelog

All notable changes to the `freemkv` CLI are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and the
project follows semantic versioning.

## [1.1.0-beta.1] — UNRELEASED

Inherits libfreemkv 1.1.0-beta.1, including the **post-read decrypt-verify
gate** (encrypted units are verified during the rip; an undecryptable unit is
treated like a bad read) and the **DVD movie-not-menu** fix — DVD rips now begin
at the feature instead of several minutes of the disc menu.

### Added

- New error code **E7025** ("AACS bus key unavailable"), with messages in all
  seven UI languages and full Error-Codes-page coverage.

### Fixed

- `update-keys --keydb <path>` now downloads to that path (previously ignored,
  always wrote the default location).

## [1.0.0-rc.5.1]

### Added

- **`--log-level` validation.** Out-of-range or non-integer values now
  print a clear error and exit non-zero instead of silently falling back
  to a default.
- **`--key-url` scheme validation.** The key URL is validated at startup;
  an unsupported scheme (anything other than `http://` / `https://`)
  prints a specific error rather than failing later at download time.
- **`--language` validation.** An unrecognized language tag is rejected
  with a descriptive error listing accepted values.
- **Full UI localization across 7 locales.** The help/usage screen and all
  user-facing strings (progress labels, result blocks, error messages) are
  now fully localized for all supported locales, not just the runtime
  error path.

### Changed

- **"CSS authentication failed" message clarified.** When the CSS bus-auth
  handshake succeeds but the disc cannot be decrypted, the error message
  now distinguishes between an auth-level failure and an unrecoverable
  title-key failure, making the root cause actionable.

## [1.0.0-rc.4] — UNRELEASED

Cleaner terminal output, an error-message overhaul, and reliability fixes
on the disc-info and pipe paths.

### Changed

- **Two-channel logging: clean terminal, file-only diagnostics.** The
  terminal now carries only curated progress, status, and the final
  result block — no `tracing` DEBUG/TRACE ever reaches it. A diagnostic
  log file is written only when you ask for it (`--log-level N`,
  `--log-file PATH`, or `RUST_LOG`); with none set, no subscriber is
  installed and nothing is logged. The default log path is `./log.txt`,
  written with timestamps on and ANSI colour off so it pastes cleanly
  into a bug report.
- **Fatal-error block.** On a fatal error the CLI prints one readable
  block — the operation, the plain-English cause, and how to enable a
  diagnostic log — instead of a raw error code.
- **Error-message overhaul.** 47 previously unmapped error codes now
  render a clear message, jargon-heavy strings were rewritten in plain
  language, and the `verify` subcommand is fully localized.

### Fixed

- A disc → ISO rip that recovered zero readable bytes now fails instead
  of writing an empty image.
- A CLI pipe that hits an early EOF no longer exits `0` with a
  structurally invalid MKV.
- `disc-info` no longer drops the real scan/drive-open error: the
  underlying cause is routed through the localized error renderer rather
  than being masked by a generic failure.

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
