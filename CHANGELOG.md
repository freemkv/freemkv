# Changelog

All notable changes to the `freemkv` CLI are documented here. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and the
project follows semantic versioning.

## [1.4.2] — 2026-07-15

### Changed

- Inherits **libfreemkv 1.4.2**: the mux no longer nulls decryptable video or
  storms the online key server on a bad-encoded region; decrypt and TS-structure
  are separated into distinct primitives so a correct key that yields
  imperfectly-encoded TS is passed to the demuxer rather than treated as loss.

## [1.4.1] — 2026-07-14

### Fixed

- Inherits **libfreemkv 1.4.1**: `freemkv iso://… mkv://…` (and disc rips) no
  longer conceal decryptable video as loss over a single authored-bad TS packet.
  A non-conforming packet is now left for the demuxer to handle instead of the
  whole aligned unit being nulled. Also folds in the unified MVC (Blu-ray 3D)
  track-signal handling.

## [1.4.0] — 2026-07-13

### Added

- **Blu-ray 3D output.** Ripping a 3D disc — `freemkv disc:// mkv://out.mkv` or
  `freemkv iso://<3d>.iso mkv://out.mkv` — now produces a single MVC video track
  carrying both eyes (base + dependent view), via libfreemkv 1.4.0. Non-3D rips
  are byte-for-byte unchanged.

## [1.3.2] — 2026-07-10

Version sync with the workspace; inherits libfreemkv 1.3.2.

## [1.3.1] — 2026-07-10

Inherits libfreemkv 1.3.1 — authoritative HD-DVD title composition (durations,
names, chapters from the Advanced-Content playlist).

### Licensing

- **Relicensed to the MIT License, from 1.3.1 onwards** (releases up to and
  including 1.3.0 remain under AGPL-3.0).

## [1.3.0] — 2026-07-08

Inherits libfreemkv 1.3.0 — AACS 2.1 (FMTS) and HD-DVD as first-class formats,
HD-DVD VC-1 muxing, and display-order timestamps for program-stream video.

### Added

- **`disc-info` labels the AACS generation and HD-DVD / FMTS formats.** The
  encryption line now reads `AACS 1.0` / `AACS 2.0` / `AACS 2.1` (the 2.1 from
  the FMTS format) instead of a bare "encrypted", and HD-DVD / 4K UHD FMTS discs
  are named as their own formats.
- **`disc-info -v` resolves keys and shows the crypto detail.** Verbose now runs
  a local-keydb key resolution (host-cert handshake scan + ciphertext sampling),
  so the Keys line reflects a real unit-key set — with the Volume ID, the Volume
  Unique Key, and each CPS unit key printed. The verbose block leads with the
  drive / device / region, then the MKB generation, hash, VID, and keys.

### Changed

- **`disc-info -v` shows a PID for every stream**, subtitles included (previously
  video and audio only), and adds the disc region.

## [1.2.0] — 2026-07-01

Inherits libfreemkv 1.2.0, including **mux loss concealment** — when a unit
genuinely can't be decrypted, the mux conceals it (rather than emitting
ciphertext or a broken frame) and drops forward to the next keyframe, so a
remux of a disc with an unrecoverable gap still produces a decode-clean MKV;
the loss is logged, not silent.

### Added

- **`disc-info` reports the unlocker matrix.** The `disc-info` command now shows
  which unlockers actually ran for the disc — LibreDrive firmware unlock, AACS,
  CSS — alongside the disc's other metadata, matching the operator-visible report
  autorip logs after each scan. Driven by `Disc::unlocker_matrix()` in libfreemkv
  1.2.0.

### Fixed

- **Read-time key fetch uses the disc's own AACS version.** `update-keys` /
  remux key resolution now carries the disc's AACS version into `DiscInputs`, so
  an on-demand key fetch parses `Unit_Key_RO.inf` at the disc's matching stride
  (AACS-1.0 48-byte vs AACS-2.x 64-byte) instead of assuming one layout.

## [1.1.0]

Inherits libfreemkv 1.1.0, including the **post-read decrypt-verify
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
