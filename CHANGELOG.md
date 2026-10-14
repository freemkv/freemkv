# Changelog

## [1.8.4] — 2026-10-11

### Breaking / Upgrade notes (1.8.3 → 1.8.4)

- **The Library's files from 1.8.3 load as they are** (server only: the Library's state is the server's): `settings.json`, `library-queue.json` and `library-links.json` are read unchanged. New are `library-matches.json` in the config folder (saved Change match corrections) and a `.delivery.json` journal in a rip's staging folder while it is delivered; new rips stage in `disc-<identity>` folders named by the disc, not by its title.
- **Rips 1.8.3 handed to the mux worker but had not muxed wait in Review** (server only: Review): 1.8.3 recorded no proof of which titles its plan named, so confirm the match once; the rip is then muxed from its saved image without reading the disc again.
- **A delivery 1.8.3 was part-way through finishes** (server only: the mover): outputs 1.8.3 had already moved into the library are left there (not delivered, linked or claimed again) and the rest is delivered; a rip with nothing left to deliver is finished and its staging folder removed. A file 1.8.3 had put in the library without removing its staged copy is filed again as `<name>_2`: delete that duplicate. A staged file under a name the rip does not list holds the rip with nothing moved or deleted: rename or remove it, then choose Retry.
- **1.8.3 captures resume when their recovery map identifies the disc** (server only: staging): a 1.8.3 staging folder that no disc can be matched to, and that nothing else will finish, is kept and named after Clear staging so you can delete it.
- **Links written by 1.8.3 still pair rows but prove nothing about the file** (server only: `library-links.json`): Change match never removes such a file; Remux of its row still replaces the row's MKV as 1.8.3 did.
- **Going back to 1.8.3 loses Library state** (server only: downgrade): 1.8.3 reads the new `library-links.json` as empty and sets it aside the next time it records a link, ignores a queued job's TV or Change match plan, and reads neither delivery journals nor saved matches. Let the remux queue and deliveries finish first. Settings survive: 1.8.3 ignores `presentation_language` (and drops it when it saves), and the 1.8.3 app keeps the new app settings.
- **Reboot library on macOS needs the login service with `KeepAlive`** (server only: the macOS service): the server exits 75 for launchd to start it again, so `KeepAlive` must be true, or conditions that include `SuccessfulExit` false. `macos/install-library-service.sh` sets `KeepAlive` true on a new install and says so when an existing plist would not restart the job.
- **Allow 130 s for the server to stop** (server only: service and container stop): any stop now ends within 116 s. Re-run `macos/install-library-service.sh` to add launchd `ExitTimeOut` 130 to an installed service, then reload it (log out and back in, or `launchctl bootout gui/$(id -u)/io.freemkv.library` then `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/io.freemkv.library.plist`; this stops running work). The Docker example sets `stop_grace_period: 130s`; without compose use `docker run --stop-timeout 130`.
- **Configured staging and output folders must exist** (server only: Library startup): they are no longer created at startup (see Fixed). Create them, or mount the share, yourself; the defaults under the config folder are still created.
- **Builds against libfreemkv 1.8.4 and freemkv-engine 1.8.4** (CLI, app, server): their new title-selection and output-identity interfaces are required, so 1.8.3 and older do not build. With a freemkv-i18n older than 1.8.4 the new messages show in English.
- **A delivery held by a pre-release 1.8.4 build after an SMB remount needs one Retry** (server only: development builds' delivery journals): their journals load as they are; files are now known by size and pinned modification time, which survive a remount.

### Added

- **The drive card lists a TV disc's episodes** (server only): while a TV disc rips, its card shows the episodes the capture plan will produce ("3 episodes · S01E04–E06") and each one's TMDB name and length, in place of the one title length, and how much of each episode is not read cleanly yet; the "Not read yet" figure counts the episodes instead of "the movie". A title correction lists them again under the new match.
- **Presentation language: rip the version of the movie in your language** (CLI, app, server): `--presentation-language de`, the app's Settings ▸ Selection "Presentation language" and the server's `presentation_language` setting choose, as the main title, the cut in that language when the disc carries the movie as several versions of the first title (most of their content shared, in the same order). Audio languages (`-a de`) choose the same way when none is set; trailers, single episodes, decoy playlists and separate copies are never chosen, and a language the disc doesn't match keeps the first title. A tag that isn't a language is refused where you enter it.
- **Change match: correct what an ISO is, and its library files follow** (server only: the Library's Remux page, `GET /api/library/match`, `POST /api/library/match/remux`): confirm the movie or show (season, disc, first episode) an ISO really is; the server saves the match, remuxes the disc under the corrected names and, once every new file is in place, removes the old files it made from that ISO. A file is removed only while it is exactly as freemkv wrote it, from the disc now at that ISO path; anything else is kept and named in the job's log. Later Remux and Retry of the ISO use the saved match, which is ignored once a different disc is filed at that path.
- **Library remuxes of TV discs file episodes into the TV folder** (server only: Library remux): a disc plans as TV by the same rule as a rip (a confident TMDB TV match, or a label naming a season, with "Rip TV seasons automatically" on) and fans out into `Show (Year)/Season NN/` under the TV folder, or is held as a rip would be. The row shows its episode progress; the old single MKV is left in place. A job fails, to be retried, when TMDB cannot be reached to plan it.
- **Reboot library** (server only: System page, `POST /api/system/reboot`): running work stops at its next safe point (rips within 60 s, other services within 30 s more), then the server starts again and the page reconnects by itself. On Linux and in the container it re-executes itself in place, picking up an upgraded binary; on macOS it exits for launchd to start it again (see Upgrade notes) and is refused when nothing would; on Windows it is refused. Reboots are refused within a minute of a start or of the last request, as the Library has no login.
- **Clear staging** (server only: System page, `POST /api/library/staged/clear`): removes failed rips with their saved images, stopped captures of discs whose identity could not be verified (each insert of such a disc starts a new capture, so they piled up), empty leftover entries, and finished remuxes kept on local staging whose Library job failed. Anything ripping, muxing, in Review, resumable, corrected, not yet delivered or still queued to be copied in is kept, and the page says how many items could not be removed.
- **Title corrections stay with the disc** (server only: the Drives page's Change title and Review): a correction is saved in the disc's staging, kept across interrupted rips and later scans, can set the First episode of a TV disc, and is offered while ripping and after an error too. One that changes what the disc produces is replanned from the captured image; once that image is gone it is refused.

### Changed

- **TV episodes come from the disc's authored episode list** (server only: "Rip TV seasons automatically" and Library remux): a TV disc whose episodes cannot be told apart is held in Review with E9086 and the reason, and the message suggests correcting the match to the movie or turning the setting off (or setting the First episode when the numbering is out of range).
- **Deliveries survive restarts and are retried on a schedule** (server only: the mover): each rip's delivery is journaled, so a crash or reboot finishes it without filing anything twice, and a file found at a destination is adopted only when its first and last 64 KiB match the staged rip. A failed copy is retried after 30 s and 2 min, then held; Needs action rows offer Retry and Retry all, say whether a retry is waiting, running or stopped, and keep their hold across restarts.
- **Each disc stages under its own identity** (server only: staging): a reinserted disc resumes its own capture whatever it is titled; a disc whose identity cannot be read gets a fresh capture each insert.
- **Every MKV of a split disc is linked to its ISO** (server only: the Library's links): each episode of a TV rip is its own Library row paired with the disc's ISO (1.8.3 linked only one MKV with one ISO). Remux on an episode row remuxes only that episode, from the title the TV plan puts at that file, and refuses (pointing to Change match) when no planned title lands there.
- **A remux never overwrites a file freemkv did not make** (server only: Library remux): an existing file at an output path must be the row's own MKV or a file freemkv wrote from that ISO and still unchanged, else the job fails with "already exists and was not made from this ISO" and writes nothing. A replaced file is checked again just before the new one lands. `library-links.json` now records each file's and its ISO's size and modification time for this.
- **An empty library or TV folder is never written into** (server only: Library remux): an unmounted share's empty mount point is refused when a job writes, as it already was when queuing; if a folder is genuinely empty, create any folder in it first.
- **The System page shows storage and folder health in one table** (server only: System page), with a Staging card for kept remuxes and Clear staging.
- **"Stop all" leaves audits paused until you press Resume** (server only: Library audits).
- **The disc report names the unlocker that handled the disc** (CLI, app, server): `freemkv info` and the server's drive log print `Unlocker: <name>` instead of every unlocker with yes or no.
- **Audio and subtitle choices made in the selection bar are remembered for the next disc** (app only: the selection bar and Settings ▸ Selection; the CLI and server take languages from flags and settings): a bar choice now also becomes Settings' audio, subtitle and forced-subtitle languages. To keep 1.8.3's per-disc behaviour, turn off the new "Remember audio and subtitle choices" (on by default).
- **Opening an image checks the titles that will be ripped** (app only: the readiness line after Open): it checks, and looks up keys for, the titles the list ticks for your title mode and languages, such as the German cut or every episode, instead of always the first title, and checks again when a Settings change made while opening changes the ticks.
- **Settings are written in the background** (app only: the desktop Settings and the selection bar): saving, and testing a key-service URL, no longer freeze the window on a slow disk or DNS server. A failed save is retried by pressing OK again; Quit waits up to 2 s for each pending write, and a write that fails then is reported only on standard error.
- **A title reads "Episode" only when the disc's episode list names it** (app only: the title list's Notes): the list no longer guesses "Play all" and "Episode" from overlapping sectors.
- **The start page says whether a disc is in the drive** (app only: the empty window): "Disc inserted — ready to open" or "No disc detected" once the launch check answers.

### Fixed

- **TLS library updated for four advisories** (CLI, app, server: online key service and TMDB): the locked rustls is 0.23.46, for RUSTSEC-2026-0346, -0347, -0348 and -0349.
- **Blu-rays rip again on drives whose speed calibration hits a bus fault** (CLI, app, server; libfreemkv and freemkv-unlock): on an LG BU40N (MediaTek) a bus fault during calibration cleared the drive's unlock, and the scan then failed on every disc with "the drive returned a command error"; 1.7.7 ripped the same discs. The unlock is kept when the drive still confirms it, the command after a fault waits for the device to reopen, and a handshake bus fault no longer fails the scan.
- **Logs name the real error** (server only: logs): every error shown as a plain-English line is also logged at warn with its code and full detail (opcode, status, sense).

- **A TV disc's retry passes cover every episode** (server only): the passes after the sweep stopped as soon as the first episode read clean ("muxed title is 100% recovered"), so damage in a later episode went straight to the mux. They now run until every planned episode is clean, or the retries run out.
- **A typed show or movie name gets its poster, year and episode names** (server only: Change title on a drive card): "Use this exact name" saved only the text, so the card stayed blank and episodes were filed without names. When exactly one TMDB title of that type carries the name (and the year, if typed), the correction takes its details; two titles of that name, or none, keep the name as typed.
- **Episodes on a pressing whose play-all trims an episode by a few frames** (libfreemkv): such a disc held every rip as E9086 "no authored episode list"; it now rips its episodes.
- **A share that is not mounted no longer gets folders created on the local disk** (server only: Library startup, rips and Settings): 1.8.3 created the configured staging and output folders at startup, so an unmounted share's rips filled the boot disk or landed hidden under the share. A missing configured staging folder now makes a rip started by inserting a disc wait ("staging folder … is missing — is the share mounted?") and start once it appears; Rip, Resume and Start over are refused with that reason. Saving a new staging folder in Settings creates it, inside a folder that exists and is not empty.
- **A share that drops just before a delivery no longer gets its folders recreated on the local disk** (server only: the mover): title folders are made only below the checked library folder, and only while it is still that folder; otherwise the rip waits in staging and the row says why. A delivery whose title folder disappeared waits instead of recreating it; choose Retry on that row to recreate it if it was removed on purpose. A delivery refused before it starts no longer leaves empty folders behind.
- **The server always exits when told to stop, even with a network share hung** (server only: stop, quit and Reboot library): a thread stuck on a share used to keep the process alive after every service had stopped. Any stop now ends within 116 s, logging what did not stop, with the same exit code; a network share remount no longer starts after a stop request. A thread the kernel holds on a hung SMB mount can still linger until that mount is force-unmounted.
- **A slow network share keeps receiving deliveries** (server only: folder health and the mover): a NAS waking its disks showed as "not responding" and held finished rips for minutes ("an earlier check of this folder never came back"). A folder now counts as not responding only after a check has gone 60 s unanswered; past 5 s it shows as "still checking" and work for it waits; a late answer still counts and clears the warning; and the System page, the Library and the mover go by the same verdict.
- **A library queue file that cannot be read for a moment is no longer replaced** (server only: Library remux queue): 1.8.3 renamed it aside and started empty when a config share answered EIO, ESTALE or EACCES. The read is retried, then the queue holds with the reason and is read again every minute, refusing changes until it loads. Only a file that does not parse is set aside, as `library-queue.json.unreadable` (then `.unreadable.1`, …) so an earlier copy is never overwritten; a damaged `library-links.json` is set aside the same way by a Library remux, while a delivery waits for it to be repaired.
- **A browser that stops reading the live view no longer holds a server thread** (server only: the live-update stream): a write that cannot finish within 60 s ends that stream, on Linux and macOS.
- **The app's Settings window behaves the same on every OS** (app only: the desktop Settings window): on Windows, switching the interface language applies the rest of the form at once and OK with nothing changed no longer reports "Settings saved"; on Linux, a rejected key-service URL is logged as a notice.
- **A keydb update that cannot write its file says where and why** (CLI, app only: the server refreshes its keydb itself): instead of "keydb rejected: E<code>", it names the folder or file and asks to check that it is writable.

## [1.8.3] — 2026-10-08

### Changed

- Add beta Pioneer runtime unlocking through firmware signature discovery while preserving VID retrieval (CLI, app, and server share this drive path). Include release-tooling checks for download links, checksums, and package manifests.

## [1.8.2] — 2026-10-08

### Fixed

- **The Library server image is built against the release's own library versions** (server only: the image build): 1.8.1's image was built from the libraries' development branches and reported the rip library as 1.8.0 (same code).
- **Stop no longer rescans the disc it stopped** (server only: the drive poller belongs to the Library): the disc stays held until it is ejected, the drive is unplugged, or Scan/Rip is pressed.
- **A network share that stops responding no longer hangs Library startup** (server only: Library service startup): the folder is reported as not responding after 5 s and the web UI comes up.
- **A disc error or Stop during key lookup is no longer logged as the key service's final answer** (server only: the key-service retry belongs to the Library), so outage retries are no longer skipped.

### Changed

- **Start reuses the drive, scan and keys from Open** (app only: the server already reuses its session): about 8 s less per drive rip.
- **Key-service retries no longer reread the disc** (server only: the key-service retry belongs to the Library).
- **MKV rips that don't keep the ISO read only the titles they mux** (server only: the staging sweep belongs to the Library), not the whole disc.

## [1.8.1] — 2026-10-07

### Added

- **Connect to Remote Library** in Settings shows local and remote drives together with machine labels, live progress, controls and console logs. An optional return URL connects both UIs; offline Libraries reconnect automatically. Jobs and output paths remain on the owning machine. (server only: Library web UI)

- ARM builds: Windows on ARM gets `freemkv-aarch64-windows-setup.exe` / `freemkv-aarch64-windows.zip` and `freemkv-cli-aarch64-windows.exe`; 64-bit ARM Linux gets `freemkv-arm64.deb`, `freemkv-cli-arm64.deb` and `freemkv-aarch64-linux.AppImage`; 32-bit ARM (Raspberry Pi OS 32-bit) gets `freemkv-cli-armv7-linux` and `freemkv-cli-armhf.deb`; the server image is multi-arch (linux/amd64, linux/arm64, linux/arm/v7) under both `freemkv-library` and `autorip`. Each ships with a `.sha256`. (CLI, app, server)

### Changed

- Local and connected drives share one connection model. Local drives use the live event stream; remote drives poll every three seconds so connected Libraries cannot exhaust browser connections and stall controls. Slow system or review requests no longer delay drive progress. (server only: Drives page)

- Webhooks support custom authentication headers, with masked values in Settings and a Test button for unsaved changes. Existing URL-only webhooks remain supported. (server only: notifications)

- Combine all connected Libraries in shared muxing, moving and title-review panels, with source badges on remote jobs and a source selector in the system log. (server only: Drives page)

- Connected Libraries include the complete Drives-page pipeline: mux/move progress and queues, errors, title reviews and logs, with actions routed to their owner. Brief connection failures have a 15-second grace period before showing offline status. (server only: Library web UI)

- The native macOS Library service installer sets app-level resource limits to prevent default launchd I/O throttling. (server only: macOS service installation)

- File moves show recent transfer speed using the rip estimator’s growing 10–60 second window; move ETA uses that recent rate so a slow start ages out. (server only: Library mover progress)

- Drive warnings wait for repeated probe failures and clear after recovery; the Read / Rip / Finish indicator stays aligned without button styling. macOS drive controls accept stable optical-service IDs, preventing a second drive card when exclusive ripping removes the disk name. (server only: Library polling and web UI)

- macOS: the `.dmg` is no longer built. The app ships as `freemkv-aarch64-macos.zip` / `freemkv-x86_64-macos.zip` (the same ad-hoc-signed `freemkv.app`); it is not notarized, so allow its first launch under System Settings → Privacy & Security → Open Anyway (see [INSTALL.md](INSTALL.md)). (app only: macOS packaging)

## [1.8.0] — 2026-10-05

### Changed

- **An ISO is decrypted unless you ask for raw, on every front end** (CLI, app, server). A multi-pass recovery to an ISO now decrypts as it reads (the app no longer refuses it or asks for 'Keep encrypted (raw)'), and the server's ISO output is delivered decrypted instead of raw, like the CLI's and app's. The server has no raw option, so its ISOs are always decrypted; it acquires the whole disc's keys before the rip and refuses a disc it cannot fully key, as the CLI does.
- **A stopped multi-pass ISO recovery resumes where it stopped** (app only: the server already resumed its rips and the CLI's `--multipass` resumes one pass per run): a decrypting multi-pass rip to an ISO started its sweep over from the beginning after a Stop, and its retry passes could not read encrypted sectors. It now resumes the image and retries with the rip's keys.
- **Output title names** (CLI, app, server): a title ripped from a disc is named by the disc's title (else its volume label) in the container's title field and in `fvi://` provenance; one ripped from an ISO, a folder or a staged image by the disc's title, else its playlist file name (e.g. `00800.mpls`) as before, so an image and its extracted folder name a title alike. Server rips used the playlist file name.
- **TrueHD 7.1 and Atmos tracks are labelled correctly on every rip** (CLI, app, server): live and staged-image rips labelled them 5.1 at 48 kHz from the playlist; they now read the track's own header, as `iso://` rips did.
- **A DVD ripped from a drive to a folder is descrambled** (CLI, app only: the server has no folder output): its title VOBs were written still CSS-scrambled.
- **An encrypted Blu-ray folder can be ripped like its ISO** (CLI, app, server): its keys are looked up and its titles decrypted, instead of the folder being refused as an encrypted copy.
- **`iso:// → iso://` copies the way the app does** (CLI only: the app already copied images this way; the server has no image source): read errors are recorded in a mapfile beside the image and skipped, so a re-run resumes it, and a copy short of some sectors keeps its image and exits 3 like a disc copy, instead of stopping at the first error with no map. The CLI's disc, image and folder copies now run as one engine plan, which also takes the image's lock.
- **Several titles onto one file name write one file per title beside it, from every source** (CLI only: the app and server name each title's file themselves): `-t 3 -t 4 mkv://out.mkv` writes `out_t3.mkv` and `out_t4.mkv` next to `out.mkv` for `disc://`, `iso://` and `dir://` alike. A disc refused it and an image or folder made `out.mkv` a directory.
- `disc:// → null://` (a read test of the whole disc) works on Windows (CLI only: the app and server have no null output).
- The app's multipass rip to MKV now stages only what the mux needs: the disc's file system, its navigation and AACS files, and the chosen titles. On a disc where the location of a bus-encrypted stream file cannot be read, it rips the chosen titles instead of stopping with E6021. "Keep the ISO" still keeps a whole-disc image when one can be made; on such a disc the staged image holds only the chosen titles, so it is deleted after the mux and the result says why. An image staged this way is refused as an ISO or folder source, and for any title it was not staged for (E6022), in the app and the CLI alike; a later `disc:// → iso://` copy to the same path refuses (E6021) until every stream file can be located, then fills in the rest. (app only: the app's staged title rip)
- A disc whose AACS key file (`Unit_Key_RO.inf`) cannot be read now stops `freemkv info` and decrypting rips with error E7031; a `--raw` disc→ISO copy still runs. The app's "Keep encrypted (raw)" whole-disc ISO copy now matches: it also scans past the unreadable key file instead of stopping. "Keep encrypted (raw)" is ignored (with a notice) for a source that is not a disc in the drive, matching the CLI's `--raw` refusal for an image source. A `--raw` disc→ISO copy's mapfile now keeps the disc's resolved AACS keys (or its Volume ID, if unresolved), matching the app's copy. (CLI, app, server)
- A decrypted disc→ISO or image→ISO copy of an AACS disc now stops before the copy with E7032 when an unplayed stream file is encrypted and no held key opens it. E7032 says to rip the titles to MKV or keep an encrypted copy. Every such file is probed at its first unit and up to 32 units across it, and a key must open two probed units before it is used. If no key can be proven (damage), a multi-key copy stops at the file's first encrypted unit with the same E7032. The image→ISO path now reads through the same libfreemkv reader as the GUI and CLI disc→ISO path. It asks the configured key service for a missing key, and creates no output file when it refuses. Ctrl-C while keys are checked, including during a key-service call, now stops the copy before the ISO is created. Before, it was ignored until one batch had been written. An AACS image with titles but no stream folder fails with E6003, naming the folder, instead of E7013. (CLI, app, server)
- **A `disc:// → iso://` copy that recovered SOME but not all of the disc is a SUCCESS again, with its own exit code.** A single-pass copy short of some sectors now exits **3** ("Damaged": kept and usable), not 1 — reversing the 1.6.5 change that made a holed image fail the exit code (see `freemkv help` for the new exit-code table). Both the CLI and the app print the same warning: how much could not be read, and that re-running (`--multipass` in the CLI) may recover more — even after a `--multipass` run, since it only retries once per invocation. NOTHING readable (`freemkv help`'s exit **1**) is still the only case where the ISO is reported unusable; it is kept on disk in both the CLI and the app. The app previously never checked for this at all on a whole-disc ISO copy, so an all-zero image silently reported success — it now reports the same failure the CLI always has. (CLI only: an exit code)
- The app's Single-pass rip mode no longer silently runs multipass recovery when a `--multipass` run's pass count was left typed in the settings; Single-pass now always means one pass, matching the CLI. (app only: an app setting)
- **Separate app and CLI builds on every OS.** Both install the `freemkv` command; install one or the other. The **app** opens its window when run with no arguments and runs the CLI for any command. The **CLI** has no UI libraries (static on Linux) and prints usage when run with no arguments. `freemkv gui` still opens the window in the app. See [INSTALL.md](INSTALL.md). (CLI, app only: packaging; the server ships in its own image)
- CLI binaries are renamed `freemkv-cli-<arch>-<os>` (e.g. `freemkv-cli-x86_64-linux`, `freemkv-cli-x86_64-windows.exe`). The old names are still published for this release only; update download scripts. (CLI only: release asset names)
- Windows app: new per-user installer `freemkv-x86_64-windows-setup.exe` (Start menu entry, adds itself to `PATH`). The portable `.zip` now holds `freemkv.exe` (the window) and `freemkv.com` (so `freemkv` typed in a terminal prints to it). (app only: Windows packaging)
- Linux: the `.deb` is split into `freemkv` (app, `freemkv-amd64.deb`) and `freemkv-cli` (`freemkv-cli-amd64.deb`, no dependencies); each replaces the other. The Flatpak download is now `freemkv-x86_64-linux.flatpak`. (CLI, app only: Linux packaging)
- Homebrew: the cask is now `freemkv` (was `freemkv-app`) and also links the `freemkv` command; the CLI formula is now `freemkv-cli` (was `freemkv`). Existing installs follow the rename. (CLI, app only: Homebrew packaging)
- Versioned duplicate assets are no longer published; every asset has a stable name and a `.sha256`. (CLI, app only: release assets)
- The app's title list shows each title's running time and size in their own right-aligned **Length** and **Size** columns; a title's Description reads `1. 00800.mpls (19 chapters)`. On Windows, whose tree has a single column, they follow the Description in the row's label. (app only: the title list; `freemkv info` and the server's JSON are unchanged)
- The macOS app's Settings save a text field when you press Enter or leave it, without closing the window, as the Linux app's do. Labels too long for their column wrap, dropdowns widen to their longest choice, and a long default destination or keydb path wraps onto more lines beneath its label instead of being cut off. Settings reopened after Cancel show the saved values, not the abandoned edits. (app only: the Settings window)
- The macOS app's progress page for a single-title rip no longer cuts off the top of the Information panel, and its title list keeps the Size column in view. (app only: the macOS window layout)
- The app's title list shows each title's running time and size in their own right-aligned **Length** and **Size** columns; a title's Description reads `1. 00800.mpls (19 chapters)`. On Windows they sit under a column header over the tree, can be resized by dragging its dividers, and read from the right under a right-to-left interface language. (app only: the title list; `freemkv info` and the server's JSON are unchanged)
- The app's Settings keep a text field's edit when you press Enter or leave the field, on Windows as on Linux; a Windows dropdown widens to show its longest choice and its open list shows every choice in full, and the default destination and keydb paths have a full-width field under their label. (app only: the Settings window)

### Added

- The app's title list shows a title's chapters under a collapsed **Chapters** row, named where the disc names them (DVD text data), each with its length. (app only: the title list is the app's)
- `freemkv info` shows a DVD's region from its navigation data; an all-prohibited region mask reads as none. (CLI, app only: the server shows no disc info)
- Linux: a strictly confined Snap of the app and CLI, `freemkv-amd64.snap`, attached to every release. Install it with `snap install --dangerous` and connect `freemkv:optical-write` for drive access; Snap Store publishing follows once the listing is approved. See [INSTALL.md](INSTALL.md). (CLI, app only: Linux packaging)

### Server (replaces autorip)

- **A network share that failed to remount is retried, and nothing is written to the container's own disk meanwhile.** While the share is not mounted, rips, moves and remuxes into it hold or stop with an error. (server only: the share and its rips and moves are the server's)
- **Keys come from the source picked in settings.** AACS Key Source = online asks only the online key service; local uses only the local KEYDB. An autorip `settings.json` loads unchanged, and a stored `http://` Keyserver URL is warned about at startup. (server only: the server's key-source setting)
- The multi-pass recovery (sweep, retry passes, end-of-recovery promotion) runs the engine's passes, the same implementation as the app's and the CLI's; the server keeps its own drive recovery (re-open after a USB bridge crash, a spin-cycle before each retry pass), its device-log lines and its loss gate. (server only: the server's pass loop moved into the engine)
- A staged disc image (a multi-pass rip, a resumed rip, a deferred mux) is muxed through the same image path as the CLI and the app. A mux that fails removes its partial MKV from staging; the ISO and mapfile stay for the retry. (server only: brings the server onto the CLI's and app's image path)
- **The Library's details dialog shows what is in each movie again**, as the retired mkv-audit did: resolution tier and frame size, frame rate, codec and bit depth; HDR (SDR, HDR10, HDR10+, HLG) and Dolby Vision with its profile (e.g. `DV P8 + HDR10`) and light levels; every audio track with its format, channels, lossless or lossy and Atmos, the best track and a warning when a player would start with a worse one, and a flag on a track whose title claims lossless audio its stream does not carry; every subtitle with its format, default and forced flags; an upgrade radar of what a better release of the tier would have (lossless, DV, Atmos, 7.1, UHD); the declared length against the real last frame; and the full header report on demand. Anything the file does not settle reads as unknown, never guessed. A deep audit that finds damage now samples the movie to say where it fails, in which layer, and what to do; a clean one says what it verified. Audits already stored keep their verdicts and deep results: the detail is filled in a few files a minute by a quick read, only while no other audit waits, and the dialog says "Details pending" until then. The Library pages are English only, as before. (server only: the Library is the server's web UI)
- **A network share that drops no longer costs an hour of work, and the Library says what is wrong.** Before a remux or a rip's move starts, its output folder must answer a stat and a create-and-delete within 10 s, on a thread of its own, so a stale or hung share never blocks the server; the folders are also checked every 30 s, with one check per folder in flight at most. While a folder is unhealthy the Library and Remux pages show a banner naming it, the cause in plain words (stale file handle, I/O error, read-only, missing, not responding, permission denied) and the fix (remount the share on the host); new remuxes and moves wait in the queue as "waiting for the output folder" and start on their own once it answers, and running work is never cancelled by a check. A remux stopped by the share (a stale handle, an I/O error, or a timed-out copy, flush or verify) is re-checked once, then retried after 1, 5 and 15 minutes and hourly, for up to 24 hours; a failed verify or an incomplete mux is never retried. A failed remux names the phase it stopped in and the folder at fault, and says the existing MKV is unchanged. The System page lists each folder's health, last good access and last error, and `GET /api/library/folders` serves the same. (server only: the Library, the remux queue and the rip mover)
- **A remux that finished but could not be copied to the share is kept, not thrown away.** When the new MKV has muxed and verified on local staging and only the copy into the output folder (or its flush, check or the final rename) fails, the finished file stays on local staging and the job waits as "Finished locally (N GB) — waiting for the output folder to copy it", with how many copies have failed. A share fault retries it on the same 1, 5, 15 minute and hourly schedule, but a retry only copies, checks and lands the kept file; it never muxes again. A copy that does not match (size or check) is not retried by itself, and a library file that changed meanwhile leaves the kept file alone and says so. The Remux page offers "Retry now" and "Discard staged file" (confirmed in the app's dialog) on such a row, the Library row shows the same note, Stop leaves the kept file offered, and the System page counts the kept files and their size. At startup, debris in the staging folder is removed, kept files older than 7 days or beyond the staging budget (a quarter of the staging disk, at most 500 GB; oldest first) are discarded, and the rest go back in the queue to be copied in. `remux_staged_max_age_days` and `remux_staged_max_gb` in settings.json change those limits (0 = the default). New routes: `POST /api/library/staged/retry` and `POST /api/library/staged/discard` (`{"target": ...}`). (server only: the Library's remux queue)
- The Library's remux with a staging folder (server only: the Library and its remux queue are the server's) now has the engine finish and verify the MKV on local staging and does the copy into the output folder itself: the same `<target>.lock`, 60-second stall limit, flush, size and runtime check of the copy, and atomic landing as before, with the same kept-file, retry and Discard behaviour. The kept file's sidecar is the Library's own format. Startup also removes a job's leftover `<id>.mkv` and `<id>.mkv.lock` from the staging folder, not only its `<id>.mkv.partial`.

## [1.7.7] — 2026-09-26

### Fixed

- Library checkpoints staged delivery writes every 64 MiB to limit queued NFS writes. Stop All reports cancellation in progress; slow remote cleanup runs separately while retaining its artifact lock. Brief connection and detail failures retain cached state quietly, hung polls time out, and reconnecting Libraries refresh their details automatically. (server only)

- Library preserves stale-handle errors from slow probes, write-access checks and capacity checks so its container can recover the NFS mount. Folder health transitions and probe stages are logged for diagnosis. A failed probe no longer permanently blocks later checks. (server only)

- Connected Libraries show one offline warning during connection failures instead of repeating the same error for system and review details. (server only: Drives page)

- Remote drive actions allow time for Stop to drain its worker and return the actual result, instead of reporting a connection failure after five seconds. (server only: connected Libraries)

- Forced PGS subtitles now disappear when their display period ends, instead of remaining until the next subtitle.
- Preserve PGS clear-event timestamps when remuxing, addressing the reproduced issue in #52. A fresh rip is needed to confirm the fix on affected discs.
- Improve audio frame handling and preserve opening audio and MKV timing metadata.
- Keep the Linux desktop interface responsive while opening and scanning sources.
- The app's preferred audio, subtitle and forced-subtitle language pickers name each language in the interface language (e.g. "Deutsch, Englisch" under German), as do their summaries; the stored ISO codes are unchanged. The interface-language dropdown's Auto entry, the keydb status line, the keydb-update and update-check messages, and the progress caption's chapter and track words are translated too. (app only)

### Linux packages

- Add a native `.deb` containing the desktop app and CLI for Ubuntu 24.04 and Linux Mint 22.
- Publish direct `.flatpak` downloads on GitHub Releases, independently of Flathub acceptance.
- Add installation instructions and download links for both formats.

### Quality and maintenance

- Expand subtitle regression tests and add FFmpeg checks for subtitle clearing, audio/video content and timing.
- Add media known-answer tests and Linux/macOS/Windows output comparisons.

## [1.7.6] — 2026-09-26

### Added

- **Linux desktop app (BETA).** A native GTK4 + libadwaita GUI at feature parity with the macOS and Windows apps: source picker (drive, file, folder), title/track tree, format and output choices, rip/cancel/eject, progress page, log pane, Settings, reveal output, hamburger menu. Every decision is made by the shared `ui` core, so it is the same product on all three OSes (issue #56). Flatpak manifest (`org.freemkv.FreeMKV`) and AppImage workflow included; the Flathub submission follows the release.
- **Rip-finished desktop notification** on all three OSes: Notification Center on macOS, a toast on Windows, the XDG portal plus an in-window toast on Linux. Clicking it reveals the output. Controlled by the new "Notify when a rip finishes" setting (default on, upgrade-safe).

### Changed

- **One menu definition for every shell.** macOS, Windows and Linux build their menus from `ui::menu_layout()`; a contract test pins that every user-driveable `Cmd` appears in it.
- **Linux paths follow XDG.** Settings and state live under `$XDG_DATA_HOME/freemkv`; the default output folder is your Videos folder as xdg-user-dirs records it (moved or localized folders are honoured). Relative XDG values are ignored, per the spec.
- The info panel's MKB line reads `MKB v<n>`; the disc type line already says whether it is UHD.

### Fixed

- `freemkv info disc://` no longer rejects `--log-level N`, and `--log-file` given without a path no longer swallows the next option.
- The GUI no longer warns that "MP4 can't hold MPEG-2" when an earlier MP4 choice does not apply to the current disc.
- `info --share` now says why the disc structure could not be captured instead of silently omitting it.
- The log pane on macOS and Windows appends new lines instead of redrawing the whole log every tick, so long rips no longer slow the window down.
- ISO-to-ISO decrypt declares its content extents (#55).
- AppImage tooling is pinned and checksum-verified (the GTK plugin's old download URL no longer existed).
- The release now stamps the real date into the AppStream metadata.

## [1.7.5] — 2026-09-23

### Changed

- freemkv-unlock mirrors the freemkv-firmware 0.9.0 ABI: the drive's `Ake` (`0x06`) and `Bus` (`0x07`) feature levers are retired into a single `Encryption` (`0x06`) lever, so the firmware unlock recipe sets one flag instead of two. Drives on firmware 0.9.0 need this; older firmware is unaffected.

## [1.7.4] — 2026-09-21
### Added

- `info … --share` now bundles non-secret AACS diagnostics (disc hash, AACS version, VID-availability) alongside the disc structure, so a keydb "no key" report carries enough to triage the lookup without shipping the disc (#46).

### Fixed

- keydb fetch uses ureq 3.4.2 with a restored rolling-idle body timeout, so a slow-but-progressing download is no longer killed by an absolute deadline.

### Maintenance

- OpenSSF Scorecard / REUSE / MSRV badges; CI moved to the central reusable workflows.

## [1.7.3] — 2026-09-19

### Added

- `info --share`: capture a shareable, context-aware diagnostic profile for bug reports. On a drive it bundles the drive profile plus, when media is present, the disc's **structure metadata** (BDMV `index.bdmv`/`MovieObject.bdmv`/`PLAYLIST`/`CLIPINF`/`BDJO`/`META`, DVD `VIDEO_TS/*.IFO`); on an ISO or `dir://` it captures just the disc structure. No audio/video essence and no AACS keys are included, so a reporter can reproduce a title-selection issue (e.g. issue #45) without shipping the full ISO. Release builds prompt `[Y/n]` before anything leaves the machine; `--mask` redacts identifiers. See the [Sharing a profile](https://freemkv.org/docs/troubleshooting/#sharing-a-profile---share) guide.

### Fixed

- Multi-angle UHD title selection (via libfreemkv 1.7.3): the MPLS STN table of a multi-angle first PlayItem is now parsed at the correct offset, so autorip/CLI no longer pick the wrong (or no) main feature on affected seamless-branch UHD discs (issue #45).

## [1.7.2] — UNRELEASED

### Changed

- Unified release with freemkv-unlock 1.7.2 (firmware ABI v2). No functional changes to this crate.

## [1.7.1] — UNRELEASED

### Changed

- Version aligned to 1.7.1 for the unified release (LibreDrive unlock fix in freemkv-unlock 1.7.1); no user-facing changes.

## [1.7.0] — 2026-09-02

### Changed

- Version aligned to 1.7.0 for the unified release. Internal CI/lint hardening (stable build + clippy, cargo-deny dependency-audit gate, audience-based comment-guard); no user-facing changes.

## [1.6.14] — 2026-08-31

### Changed

- Version aligned to 1.6.14 for the unified release.

## [1.6.13] — 2026-08-28

### Added

- `info --share` collects a few additional drive-buffer responses on supported drives.

## [1.6.12] — 2026-08-27

### Fixed

- macOS: the disc no longer unmounts mid-rip with error E1000 — freemkv now holds a DiskArbitration claim for the whole rip so the OS can't remount the disc out from under the read.
- A drive whose media state changes mid-rip (remount, disc swap, bus reset) now reacquires and re-verifies the disc instead of skipping the affected sectors as if they were damage.

### Changed

- Internal comment and documentation cleanup.

## [1.6.11] — 2026-08-26

### Changed

- Version aligned to 1.6.11 for the unified release, driven by the libfreemkv
  main-feature selection improvements (main-feature selection now follows the
  disc's own navigation instead of guessing by title size, so it no longer
  picks a decoy over the real feature) and the autorip mux-quarantine fix (a
  stuck disc no longer gets stuck retrying mux forever; see the libfreemkv and
  autorip 1.6.11 notes).

### Added

- Website and Discord links, and a coverage badge.
- Substantially expanded unit-test coverage.

## [1.6.10] — 2026-08-23

### Changed

- Version aligned to 1.6.10 for the unified release. No functional changes to
  this crate; the release was driven by libfreemkv (TrueHD/MLP audio now resyncs
  to the next major-sync access unit after a source transport-stream
  discontinuity, instead of splicing post-gap audio mid-stream — fixing
  decoder-choking seams on discs whose stream carries a continuity-counter gap;
  see the libfreemkv 1.6.10 notes).

## [1.6.9] — 2026-08-22

### Changed

- Version aligned to 1.6.9 for the unified release. No functional changes to
  this crate; the release was driven by autorip (automatic per-episode TV
  ripping — each episode named `S{NN}E{MM}`, with TMDB runtime-aligned episode
  numbering across multi-disc seasons — a Manual Rename option, and a unified
  per-disc staging state file — see the autorip 1.6.9 notes).

## [1.6.8] — 2026-08-21

### Changed

- Version aligned to 1.6.8 for the unified release. No functional changes to
  this crate; the release was driven by autorip (webhooks now fire per pipeline
  stage — Rip / Mux / Move — with the Rip hook firing the moment the drive is
  free again, plus a Ripper-tab activity-banner fix so it also shows during
  moves — see the autorip 1.6.8 notes).

## [1.6.7] — 2026-08-21

### Changed

- Version aligned to 1.6.7 for the unified release. No functional changes to
  this crate; the release was driven by autorip (per-webhook event selection,
  a progress bar per moved artifact, and move-queue / webhook-error fixes —
  see the autorip 1.6.7 notes).

## [1.6.6] — 2026-08-20

### Changed

- Version aligned to 1.6.6 for the unified release. No functional changes
  to this crate; the release was driven by autorip (webhooks may now target
  private/LAN addresses — see the autorip 1.6.6 notes).

## [1.6.5] — 2026-08-20

### Fixed

- **A re-mux that silently dropped data reported a clean "Written to …".** An
  mkv→mkv re-mux that lost bytes counted the loss nowhere the user could see,
  so a job that dropped several megabytes rendered as a successful write with a
  zero exit code. The whole outcome is now graded, the CLI and GUI share one
  loss-reporting path, a partial recovery is reported even without
  `--multipass`, and the exit code follows the verdict.

- **A rip that could not be confirmed written to disk showed the raw text
  `error.E9056`.** The two codes that warn a write-back was never confirmed
  reached the terminal as their literal dotted path — in front of someone
  deciding whether it was safe to delete the source disc — because eleven
  library error codes had no message. All eleven now carry a sentence in every
  one of the 29 locales, and the fixture that guards them is derived from the
  library's own source instead of a hand-copied list that kept going stale.

- **A rip whose worker crashed could report itself as finished.** A panic
  poisoned the shared status lock, and the default status was "completed", so a
  crashed rip could surface as a clean success. Every poisoned read now recovers
  the real value instead of defaulting.

- **The CLI and GUI could freeze mid-rip after an earlier hiccup.** A poisoned
  log lock was treated as an error at several sites: the progress bar could
  stop, later log lines were silently dropped, a cancel after an earlier panic
  re-crashed while naming the partial file it had just preserved, and the GUI's
  "Update keydb now" button could stay disabled for the life of the process.
  All of these paths now recover the buffer and keep going.

- **`freemkv info mkv://Movie.mkv` printed its labels in English in every
  language.** The `File:`/`Duration:`/`Streams:` lines, the `--share` consent
  prompt and its thank-you and refusal lines, and desktop rip-failure messages
  were hard-coded English while `info disc://` was fully localized. They are all
  routed through the catalog now, so the tool answers in one language. A missing
  translation also no longer renders as its own dotted key path.

- **`-t all` on a disc whose scan failed quietly ripped only the first title and
  exited 0.** A failed up-front scan fell through to a single-title catch-all
  that ripped title 1 and reported success. It now prints an error and stops.

- **A value-taking flag could swallow the token after it.** `freemkv --log-file
  --raw disc://` set the log path to `--raw`, consumed the flag, and silently
  wrote a *decrypted* image; `freemkv --log-file disc:// mkv://out.mkv` ate the
  source URL as the log path; and `--log-file` with no path at all wrote no log
  and ran the rip in silence. Both flag parsers now share one predicate, refuse
  a following flag or a `scheme://` URL as the value, and report the mistake
  instead of swallowing it.

- **Dragging a disc onto the Windows window during a rip could discard the rip
  in progress.** An error out of the drop handler unwound past the
  "rip still running" guard and exited. The handler now logs and swallows the
  failure, and the reveal-in-Explorer failure is logged rather than dropped.

- **On Windows, File > Exit skipped the "rip still running" check.** The two
  quit paths disagreed — File > Exit bypassed the running-rip confirmation, the
  cancel signal, and the drain. There is one quit decision now, matching the
  macOS build.

- **A decrypted folder's destination was shown with a Windows backslash** while
  an image showed a forward slash on the same panel. Only the displayed string
  changed; the real extraction path is unchanged.

- **The desktop app's diagnostic log grew without bound.** It appended forever
  and was limited only by how long Verbose/Debug was left on. It is now started
  over once it passes roughly 8 MB, across sessions.

### Added

- **`--help` now lists every URL scheme the tool accepts.** It listed 7 of the
  16 the pipeline handles; `mp4://` and `dir://` (read and write) and the
  write-only sinks (`demux://`, `video://`, `audio://`, `sub://`,
  `chapters://`, `json://`, `fvi://`) were discoverable only from the README.
  The `--language` flag is now documented in both `--help` and the README.

- **`--language auto` follows the environment locale** instead of failing with
  "locale 'auto' not found". It now defers to the environment, matching the
  desktop app.

### Security

- **A crafted drive-profile prompt could submit your drive profile on a bare
  Enter.** The `info disc:// --share` submit prompt draws its text from a
  machine-controlled catalog, and the old parser treated a bare Enter as "yes",
  so a prompt phrased to look like a default-no `[j/N]` could POST the profile
  (including the drive serial, unless `--mask`) to a public tracker. Consent now
  requires an explicit affirmative; a bare Enter and end-of-input both decline.

- **A failed settings write could leave a plaintext token behind in a temp
  file.** The temporary file is now cleaned up on a write failure instead of
  being left on disk.

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
