# Changelog

## [1.6.4] — 2026-08-15

### Fixed

- **On a few Blu-ray/UHD titles the sound ran on for about half a minute after
  the picture had ended.** For single-clip titles freemkv trimmed the picture
  to the playlist's end mark but not the sound, so a fade authored past the last
  frame was copied through — the file declared one running time but carried up
  to ~36 seconds more sound than picture (measured on `The Bourne Supremacy`:
  picture ends at 1:48:26, sound ran to 1:49:02). Single-clip titles are now
  trimmed to their marks like multi-clip titles already were; a title with no
  extra material past its marks is unchanged.

- **Opening a disc left the title list scrolled to the bottom.** On Windows,
  a freshly scanned disc opened every title so the tracks were visible
  without a click — and each one scrolled its newly revealed tracks into
  view, so the list finished parked on the last title of the disc. On a
  97-title Blu-ray that meant the first thing you saw was the tail end of
  the extras, with the film you came for somewhere far above. The list now
  opens on the title that is actually ticked, which under the default "Main
  film only" is the film, and under "All titles" is the top of the list.

- **Unticking a track under one title could leave it ripping under another.**
  Blu-ray playlists of the same feature routinely share track IDs, and the
  stream selection was applied as one union across every title — so unticking a
  commentary under one title still wrote it to another that shared the ID.
  Selection is now applied per title.

- **A damaged-disc recovery could write the wrong film under the right name.**
  The multi-pass path recovers the disc to an image and re-scans it; the
  selected titles were re-addressed by position, so if the damage dropped a
  playlist every later title number pointed at a different film. Titles are now
  re-resolved by identity (playlist name and duration); a title that is
  genuinely gone is a named error, not a silent substitution reported as
  success.

- **`freemkv iso://Disc.iso iso://Disc.iso` destroyed the source.** The natural
  way to ask for an in-place decrypt truncated the only copy before reading it.
  Source and destination are now compared by canonical path and refused if they
  are the same file — the guard the GUI already had. The same path also aborted
  every AACS image decrypt for lack of a key map; it now builds the resolved key
  map the way the engine does.

- **A video-only title's checkbox showed, and toggled, backwards**, and the
  key-database download could hang forever on a mirror that answered and then
  trickled bytes (the ureq 2→3 port had dropped the body-read timeout, and the
  GUI's "Update keydb now" stayed disabled for the life of the process). Both
  are fixed; the download bound is now rolling, so a slow-but-progressing link
  still finishes.

- **A fresh install could not rip from a drive or an ISO at all.** The shipped
  Multi-pass default collided with the engine's "multipass implies raw"
  refusal, so a first rip failed before reading a sector. The staged recovery
  image is now raw (which it always was physically) and decrypted at mux time;
  the one impossible combination is refused up front, naming both ways out. A
  cancelled or failed mux no longer deletes the multi-hour recovery behind it.

- **The launch probe no longer freezes the window.** Drive enumeration, the
  SCSI scan and key resolution ran synchronously at every startup, freezing the
  first paint until the drive answered — or for the full timeout on a drive that
  never does. The probe now runs off the UI thread through the same seam a
  running rip uses.

- **The last disc-derived strings printed to the terminal and GUI are now
  sanitised.** Playlist names and language codes are raw disc bytes — enough for
  a terminal-reset escape — and had been missed in both the CLI error renderers
  and the GUI rows. A decrypt ending with bytes still pending is also no longer
  reported as a clean write.

## [1.6.3] — 2026-08-10

### Added

- **freemkv installs with Homebrew on macOS and Linux.**

  ```sh
  brew install freemkv/tap/freemkv             # command line
  brew install --cask freemkv/tap/freemkv-app  # desktop app
  ```

  This is now the easiest way in on a Mac, and it sidesteps the security
  prompt entirely. Anything downloaded in a browser is marked by macOS as
  quarantined, and because freemkv is not notarized by Apple, the first
  launch is refused with "Apple could not verify freemkv is free of
  malware". macOS 15 removed the old right-click → Open shortcut, so the
  only way through is System Settings → Privacy & Security → Open Anyway —
  once per download. Homebrew fetches differently and is never marked that
  way, so there is nothing to click through.

  The `.dmg` is still there for anyone who prefers it, and the download page
  now explains the prompt properly instead of giving instructions that no
  longer work.

### Fixed

- **Asking for forced subtitles in one language could tick several others.**
  On a disc carrying forced subtitles in French, German, Spanish and
  Portuguese but none in English, asking for English forced subtitles ticked
  all four. Forced subtitles appear on screen by themselves during playback,
  so this put unwanted text over the picture. A forced-subtitle preference
  that matches nothing on the disc now keeps nothing — a film with no forced
  subtitles is normal, four unasked-for languages are not. Leaving the box
  empty still keeps every forced track, as before.
- **The log can be shown and hidden while a rip is running.** It was locked
  for the duration, so the only moments you could change your mind were
  before starting and after finishing — never while there was anything to
  watch.
- **Hiding the log no longer leaves a blank band across the window.** The
  space it occupied was still reserved, so the title list and the info panel
  stayed short instead of filling the window. (macOS.)
- **"Whole disc → ISO image" works on a disc image.** Decrypting an image to
  an image is something the command line has done since 1.6.1, but the app
  refused it and suggested a different output. It now runs, and refuses only
  if the result would overwrite the file being read.

### Changed

- **The preferred-language settings are now pick-lists.** Audio, subtitle and
  forced-subtitle preferences were free text, which meant knowing that a
  German track is tagged `deu` — not `ger` or `de` — and a typo looked
  exactly like a disc with no German on it. Choose languages by name from a
  list instead. Existing settings keep working.
- **Housekeeping only — ripping, reading and writing are untouched.** The HTTP
  client used to download a key database moved to its current release, an
  archive-handling crate followed, and a macOS dependency that was named but
  never used directly was removed. Duplicated crates in the application's build
  dropped from sixteen to six, so a single copy of each is compiled where two
  were before.
- **Every release is now checked on Linux, macOS and Windows before it is
  published.** The full command-line suite and a real disc rip run on all three,
  and the resulting files are compared byte for byte between them.

### Security

- **The key-database download pins its connection to addresses it has already
  checked, and a test now proves it.** That protection was already in place and
  is unchanged; what was missing was anything that would notice if it stopped
  working, since the existing checks never opened a connection.

## [1.6.2] — 2026-08-08

### Added

- **Track languages can be chosen once instead of on every disc.** Settings now
  takes preferred audio languages, preferred subtitle languages, and — separately
  — the languages to keep forced subtitles for, so "German and Spanish audio,
  German subtitles, forced only if English" is a thing you set once. Each is a
  set, not an order: asking for two languages keeps both. A disc that has none of
  them falls back to what it selected before, so it never rips silent. The
  preference decides what starts ticked and nothing more — every choice is still
  visible and can be changed per disc.

### Fixed

- **A stray moment of sound at the end of an HD-DVD title, and a click at every
  chapter break on a DVD.** Where a title is stitched from segments, a few
  frames of sound arriving just before or just after the picture that marks the
  join were timed against the wrong segment — placing one trailing frame hours
  past the end of an HD-DVD title, and squeezing about half a second of sound
  into an instant at each of a DVD's eight chapter breaks. Sound is now timed
  against the segment it belongs to. Blu-ray was never affected.

All notable changes to `freemkv` (the CLI and the desktop app — one binary,
two shells over `freemkv-engine`) are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and the project
follows semantic versioning.

## [1.6.1] — 2026-08-07

### Added

- **`iso://` is a destination for any image source, not just a drive.**
  `freemkv iso://In.iso iso://Out.iso` decrypts an existing image. `--raw` and
  `--multipass` are drive operations and now say so: they require a `disc://`
  source as well as an `iso://` destination, so a meaningless combination fails
  with a message naming the reason instead of being quietly accepted.
- **`dir://` is a source as well as a destination.** An extracted `VIDEO_TS` or
  `BDMV` folder can be read anywhere an image can — `dir://Movie/ mkv://Movie.mkv`,
  or to any other destination — and the desktop shells accept a dropped folder.

### Fixed

- **The Windows `.zip` contained the console CLI instead of the desktop app.**
  Extracting it and double-clicking opened a console window that printed usage
  and closed. The windowed build was there all along; only the packaging step
  left it out. Release verification now fails if the console binary turns up in
  that zip.
- **Blu-ray titles built from several clips ran minutes long, with sound
  drifting ahead of picture.** One title declared 2h11m and contained 2h13m; the
  worst ran 13 minutes long. Fixed in `libfreemkv` — see its changelog. Titles
  made of a single clip, DVDs and HD-DVDs were never affected.
- **A decrypted DVD image could lose most of its title list** — a disc
  enumerating 38 titles produced an image enumerating 10, silently. Fixed in
  `libfreemkv`.
- **Chapter marks and durations on NTSC DVDs ran about 0.1% short.** Fixed in
  `libfreemkv`.

## [1.6.0] — 2026-08-03

### Fixed

- **A UTF-8 BOM in `gui-settings.json` silently discarded every setting.** Any
  parse failure fell back to defaults with no log line and no notice, and the
  next save overwrote the file — so a user's key service, token, output folder
  and keydb path vanished and the app looked freshly installed. Windows
  PowerShell and Notepad both write UTF-8 with a BOM by default, so hand-editing
  the file to paste a token was enough to trigger it. The BOM is now stripped,
  an unparseable file is reported rather than swallowed, and the original is
  preserved as `gui-settings.json.bad` before anything can overwrite it.
- Key-service outages now surface as their own errors rather than as
  "no decryption key for this disc" (`E7028`/`E7029`/`E7030`).

### Added

- **Per-track-kind export in the desktop picker.** "Selected titles → video /
  audio / subtitle tracks only", matching the CLI's `video://`, `audio://` and
  `sub://` sinks. The GUI is meant to mirror the CLI's output surface per
  source kind; these three were the gap.

### Added

- **One `freemkv` binary, two shells.** The CLI and the desktop app are now the
  same crate over the shared `freemkv-engine`. A CLI-style invocation (any
  arguments, or a bare launch from a terminal) runs the command line — behaviour
  identical to the previous `freemkv` CLI, byte for byte. A windowed launch (a
  `.app` double-click, or `freemkv gui`) opens the desktop app.
- **freemkv for Mac — a native desktop app.** Open a disc or a disc image, tick
  the titles and tracks you want, press Rip. Runs the same `freemkv-engine` as
  the CLI, so it inherits the same recovery, decryption, and mux behaviour.
  - Ships as a `.dmg` per architecture (Apple Silicon and Intel).
  - Reads every source the CLI does — `iso://`, `mkv://`, `m2ts://`, `mp4://` —
    by file picker or Finder drag-and-drop.
  - Per-title and per-track selection with tri-state rollup, an output-format
    picker that follows the source kind, live progress with engine-derived
    speed and ETA, and a copyable log.
  - Key state is reported from the resolution trace, so the app names the
    source that actually unlocked the disc (`keydb` or `online`) instead of
    guessing from the disc's key-origin tag.
  - The Windows app is in development. All decision-making lives in a
    platform-neutral core, so the Windows shell renders the same model and
    reuses the same tests.
- **Audio / subtitle stream selection: `-a` / `-s`.** Choose which language
  tracks land in the output instead of always keeping every audio and subtitle
  stream. Each flag takes `all`, `none`, or a comma-separated language list —
  names or ISO codes, mixed freely and case-insensitively (`-a English,spa`,
  `-s eng`). Default (flag absent) is `all` — bit-for-bit the previous output.
  A language that matches no stream lists the disc's actual languages and fails
  the rip (a typo shouldn't silently ship the wrong file).
- **True multipass disc recovery in the desktop app.** With Multi-pass selected
  in Settings → Recovery, a disc rip recovers the disc to an intermediate image
  through the engine's shared sweep/patch recovery loop (the same strategy
  `autorip` uses — passes to convergence, abort after too many lost seconds),
  then muxes your titles from the recovered image. "**Whole disc → ISO image**"
  output writes that recovered image directly. Single-pass rips are unchanged.
- **The desktop app speaks 29 languages.** The interface is localized into
  every shipped locale (English, German, Spanish + Latin-American Spanish,
  French, Italian, Dutch, Portuguese + Brazilian Portuguese, Polish, Russian,
  Ukrainian, Czech, Slovak, Swedish, Danish, Norwegian, Finnish, Romanian,
  Hungarian, Greek, Turkish, Catalan, Japanese, Korean, Simplified & Traditional
  Chinese, Indonesian, Vietnamese), each natively reviewed. "Auto" follows the
  macOS system language; a live in-app switch takes effect without a restart.
  Regional variants resolve correctly (`pt-BR` ≠ `pt`, Simplified ≠ Traditional).

### Changed — BREAKING

- **`-t` now defaults to the MAIN TITLE only, not all titles.** With no `-t`
  flag, obfuscated discs with 50+ near-equal-length playlists turned a 40 GB
  disc into ~200 GB of near-duplicate MKVs. It now rips title 1 only.
  **Migration:** add `-t all` to restore the old all-titles default. `-t N`
  (repeatable, 1-based) is unchanged; `-t 0` is still invalid.

### Changed

- Internal: the CLI runs on **`freemkv-engine`** — the multi-title rip loop,
  disc→ISO recovery (`copy` / `CopyOptions`, the sweep/patch strategy), and a
  single shared `SpeedEstimator` for progress speed + ETA. It muxes through
  `libfreemkv::mux_stream`, brings drives up through `DiscSession`, scans ISOs
  through `scan_iso`, and resolves AACS keys through the library. No
  user-visible change; fewer places for the front-ends to drift.
- Internal: **one implementation, two shells — no duplication.** Every piece of
  orchestration the CLI and desktop app both need now lives once in
  `freemkv-engine`: the optical-drive bring-up (`open_scan_resolve`), the mux
  scaffolding (`mux_title` / `mux_title_session`, so the desktop app's live-drive
  rip gets speed + ETA), decrypted-folder extraction (`extract_tree`), AACS
  key-source ordering (`key_sources` / `won_source`), and — the big one — the
  multipass recovery **strategy** (`plan_passes`, scope-aware convergence,
  promotion, abort-on-lost, and the `multipass_rip` loop). `autorip`'s proven
  recovery core was moved down verbatim, guarded by characterization tests that
  prove its behaviour is byte-identical; its hardware-specific touch-points
  (transport-crash retry, tray un-wedge) stay in the `autorip` shell.

### Fixed

- **Fail fast on a disc with no decryption key.** A multi-title rip (`-t all`)
  against an AACS disc with no usable key used to print the "no key" error once
  per title. It now stops after the first failure with one clear error.
- **Ctrl-C is a full stop.** Interrupting a multi-title rip previously cancelled
  only the title in progress and moved on. Ctrl-C now stops the whole rip
  immediately; the mapfile/staging is preserved, so re-running resumes.
- **`stdio://` no longer corrupts the piped stream.** Ripping to `stdio://`
  (e.g. `freemkv disc:// stdio:// | …`) used to prepend the banner and
  "opening…" lines to stdout — the same channel carrying the byte stream — so
  the consumer received a corrupted stream. All human-facing text now goes to
  stderr when the destination is `stdio://`; stdout is pure stream data.
- **Title/stream selection on a file source fails loud.** `-t`, `-a`, and `-s`
  only apply to a source that is scanned into a title list (`disc://` /
  `iso://`). Given a stream/file source (`mkv://`, `m2ts://`, `network://`,
  `stdio://`) they were silently ignored. They now error up front with clear
  guidance instead of producing output that quietly kept every track.

### Testing

- **`tests/cli-integration.sh` — a self-contained CLI acceptance test.**
  Builds the binary, generates its own Blu-ray-legal media with ffmpeg
  (H.264 + two AC-3 tracks), and verifies every file-reachable function with
  ffprobe/ffmpeg: version/help, `info` (and its error/exit-code contract),
  remux (streams, languages, and duration preserved, output fully decodes),
  `null://`, `stdio://` (pure-wire-data guard + a freemkv round-trip), the
  selection and `--raw`/`--multipass` gates, and — with `FMKV_ISO_DIR` set —
  read-only `info` on real ISOs. Run it before a release.

### Known limitations

- The desktop app is **not notarized**, so the first launch needs
  right-click → Open.
