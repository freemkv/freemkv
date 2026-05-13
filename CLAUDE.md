# freemkv — multi-disc media ripping toolchain

Rust workspace for optical-disc backup. **autorip** is the production
component (a containerized service that auto-detects optical drives and
rips inserted discs); the others are libraries and CLIs it composes.

## Project goal

**Recover 100% of readable data from any optical disc, automatically,
without user intervention.** Where "readable" is defined empirically
as: anything `dd` via the kernel block layer (`/dev/sr0`, sr_mod) can
read off the same disc on the same drive. If `dd` can get a sector,
freemkv must too.

The toolchain composes:
- **Pass 1 (sweep)** — sequential read of the whole disc, tolerant of
  bad sectors via skip-ahead (mark NonTrimmed, keep going).
- **Pass N (patch)** — targeted retries on the bad ranges from Pass 1.
  Multi-attempt, with per-sector recovery timeout, cache priming,
  bisection of NonTrimmed ranges to find good middles.
- **Mux** — when mapfile is clean (or accepted-loss threshold reached),
  decrypt + mux ISO → MKV.

Production wrapper (`autorip`) orchestrates this on disc insert via a
docker container; manual control via the `freemkv` CLI.

**Release process:** See [RELEASE.md](RELEASE.md) for complete step-by-step instructions.

v0.18.24 released with mux reader verification logging — confirms ISO file vs drive source during mux phase. Debug logs show `target: "mux"` messages for reader type (FileSectorSource vs Drive), ISO sector count on open, and stall detection (>5s waits in fill_extents).

Legacy exploration (Pass N recovery vs dd via `/dev/sr0`) deployed and tested on live discs.

## Workspace layout

| Crate / dir | Role |
|---|---|
| `autorip/` | Web-orchestrated rip service. `src/{config,ripper,web,log,util}.rs`, `Cargo.toml`, `docker-compose.example.yml`. **Most active code.** |
| `libfreemkv/` | Core library — mapfile, multipass recovery, sector-level retry, AACS decryption |
| `freemkv/` | CLI — disc-info, drive-info, rip, remux, update-keys |
| `bdemu/` | Blu-ray disc emulation (testing) |
| `freemkv-tools/` | Utilities |
| `freemkv-private/scripts/precommit.sh` | The canonical pre-commit (matches CI's Rust 1.86 toolchain) |

## Hot edit files

**libfreemkv** (recovery algorithm):
- `src/disc/mod.rs` — `Disc::copy` (sweep), `Disc::patch` (Pass N retry loop, ~line 1910)
- `src/disc/read_error.rs` — unified `ReadCtx` / `ReadAction` error handling
- `src/drive/mod.rs:420` — `Drive::read` (single-shot CDB, no inline retries by design)
- `src/scsi/mod.rs:68-75` — `READ_TIMEOUT_MS` (10s) and `READ_RECOVERY_TIMEOUT_MS` (60s)

**autorip** (production service):
- `src/config.rs` — `Config` struct, env-var parsing, JSON persistence
- `src/ripper.rs` — main rip loop, retry passes, abort-on-loss check
- `src/web.rs` — HTTP handlers, settings UI, POST routes
- `src/log.rs` — `device_log()` per-device logging

## Test bed (10.1.7.13 / docker-2)

- Ubuntu 24.04, BU40N drive on **direct SATA** at `/dev/sg1` / `/dev/sr0`
- Disc loaded: Dune Part Two UHD, 78.8 GB
- autorip running as `:latest` (v0.18.16) via Watchtower auto-deploy
- Persistent data:
  - `/srv/autorip/staging/` — ISO output
  - `/srv/autorip/config/` — settings.json, logs, history
  - `/srv/autorip/config/keys/keydb.cfg` — AACS keys (bind-mounted to `/root/.config/freemkv` in container)
- User workspace:
  - `/home/matthew/freemkv/bin/` — built freemkv binaries (musl)
  - `/home/matthew/freemkv/mapfiles/` — preserved mapfile snapshots
  - `/home/matthew/freemkv/test/` — paired iso+mapfile for patch experiments

The drive at `/dev/sg1` is the **only** physical test target. There is
no other freemkv-related host. Old infrastructure (gitea.pq.io,
docker.internal.pq.io, classe, Portainer) is decommissioned — do not
reference.

## Monitoring autorip during rip

API endpoints at `https://rip1.docker.internal.pq.io/`:

- `/api/version` — returns `{"version":"0.18.24"}` (current version)
- `/api/state` — JSON with status, disc path, pass name
- `/api/debug` — POST `{"enabled":true/false}` to toggle debug logging

Simple monitor:
```bash
while true; do
  echo "$(date '+%H:%M:%S') $(curl -s https://rip1.docker.internal.pq.io/api/version)"
  curl -s https://rip1.docker.internal.pq.io/api/state | jq '.status, .pass_name'
  sleep 10
done
```

Watchtower auto-deploys from `ghcr.io/freemkv/autorip:latest` every ~30s.

## Debug logging

Enable via `/api/debug`, then monitor with:
```bash
docker logs autorip --tail=500 -f | grep '\[mux\]'
```

Key debug messages (target: "mux"):
- `ISO opened successfully: XXXXX sectors` — confirms ISO file open
- `DiscStream constructed with reader type: FileSectorSource` — mux using ISO
- `fill_extents waiting at LBA XXXX (Ns elapsed)` — slow read stall detection

## Hard rules (paid-for lessons)

1. **Never eject the drive without explicit user consent.** The BU40N
   is a slot-loading drive — eject is irreversible from software, the
   user must physically reload the disc. (Bitten 2026-05-08.)
2. **Don't hammer the live drive in tight retry loops.** Repeated
   failed reads on the same LBAs (~5+ patch runs in 50min) put the
   drive into a fast-fail state where it returns errors in <100ms
   without attempting recovery. Recovery requires eject+reload OR
   significant cool-down. **Iterate algorithm changes against synthetic
   `SectorReader` fixtures, not the live drive.**
3. **Don't tag before bumping `Cargo.toml`.** Verify CI compares them
   and fails on mismatch. v0.17.2 was this bug.
4. **Don't deploy without `privileged: true`.** Drive enumeration
   silently returns 0; UI shows "No drives detected" with no error.
5. **Don't skip precommit.** CI's Rust 1.86 catches what the Mac
   default (1.9x) silently accepts. macOS-only code paths can hide
   Linux compile failures (lesson: `tests/scsi_recovery.rs` v0.17.2
   release blocked on Linux-only `Duration` import inside
   `#[cfg(target_os = "linux")]`).
6. **`abort_on_lost_secs=0` means "require perfect rip"**, not "never
   abort". Multi-pass auto-exits early when `bytes_unreadable=0`.
   Default 0 (perfect-required); set e.g. 30 to tolerate up to 30s of
   main-movie loss before aborting after retries exhausted.
7. **Never add `Co-Authored-By: Claude`** (or any AI attribution) to
   commit messages. One contributor: MattJackson.
8. **Cross-compiling to linux-musl from macOS** uses Homebrew
   `x86_64-linux-musl-gcc` — `CC=x86_64-linux-musl-gcc CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc cargo build --release --target x86_64-unknown-linux-musl`. Faster than waiting on CI.

## Build & test

```bash
# Local build
cd autorip && cargo build --release

# Match CI's Rust 1.86 toolchain (catches drift from newer local toolchain)
freemkv-private/scripts/precommit.sh                 # all crates: fmt + clippy + tests
freemkv-private/scripts/precommit.sh autorip         # one crate
freemkv-private/scripts/precommit.sh --no-tests      # quick fmt+clippy only

# Cross-build linux-musl from macOS for fast deploy iteration
cd freemkv && CC=x86_64-linux-musl-gcc \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc \
  cargo build --release --target x86_64-unknown-linux-musl
# Binary at target/x86_64-unknown-linux-musl/release/freemkv (~6.9 MB)
```

Don't push if precommit fails. Don't `--no-verify`. CI uses Rust 1.86;
the Mac default (e.g., 1.94) silently accepts lints 1.86 rejects.

## Container requirements

- **`privileged: true` is REQUIRED** for optical SCSI drive access.
  Without it the container starts fine but `drive_count=0` and the
  UI reports "No drives detected." Verify it's in
  `docker-compose.yml`.
- Bind mount `/dev:/dev`.
- Bind mount `/srv/autorip/config/keys:/root/.config/freemkv` so KEYDB
  persists across Watchtower restarts.

## Key feature flags / config

### abort_on_lost_secs

**`abort_on_lost_secs=0`**: Require perfect rip — abort if ANY data loss in main movie after retries exhausted
**`abort_on_lost_secs=5`**: Tolerate up to 5 seconds of missing data in main movie
**`abort_on_lost_secs=30`**: Tolerate up to 30 seconds of missing data

Only applies in multi-pass mode (`rip_mode = "multi"`). Multi-pass automatically exits early when `bytes_unreadable == 0`.

### rip_mode

- `"single"`: No retries, direct disc→MKV
- `"multi"`: Retry passes + ISO intermediate + abort check after retries

## Quick references

- GHCR: `ghcr.io/freemkv/autorip` (`:latest`, `:vX.Y.Z`)
- GitHub Actions API: `api.github.com/repos/freemkv/autorip/actions/runs`
- KEYDB URL (direct, lean): `http://fvonline-db.bplaced.net/export/keydb_eng.zip`
- License: AGPL-3.0
