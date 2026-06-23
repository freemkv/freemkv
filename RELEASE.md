# freemkv Release Process

**Complete instruction set for releasing to production.**

Replace `X.Y.Z` throughout with your target version.

## TL;DR — one command

```bash
release.sh X.Y.Z            # do it
release.sh --dry-run X.Y.Z  # preview every command first
```

(The release script lives in the maintainer tooling repo.) The script orchestrates the whole cascade in ONE dependency-ordered, idempotent
run (bump → push → tag libs → re-pin binary git deps → regen locks → tag
binaries → publish keysources in parallel → print CI URLs). The manual phases
below are the fallback / mental model — **prefer the script.**

**FAILURE MODES FROM DEVIATION:**
- v0.17.2: Tagged before bumping Cargo.toml → CI verify failed
- v0.18.7: Used `cargo update --workspace` instead of manual Cargo.lock regeneration → libfreemkv 0.18.6 still baked in release
- Any time: Skipping pre-commit → Mac default Rust accepts lints that CI's 1.86 rejects

---

## The fast-release model (chore/fast-release — READ FIRST)

A release used to take **~10-20 min** because it SERIALIZED through crates.io:
libfreemkv tag → CI publish → index propagation → keysources publish →
propagation → binaries regen `Cargo.lock` (gated by a CI check that libfreemkv
be LIVE on crates.io) → binary build matrices. Each binary waited on a
crates.io publish it didn't actually need.

**The fix: the binaries no longer touch crates.io.** Each binary
(`freemkv`/`autorip`/`bdemu`/`freemkv-tools`) carries a committed
`[patch.crates-io]` in its `Cargo.toml` that redirects libfreemkv +
freemkv-keysources to a **git tag**:

```toml
# in freemkv/autorip/bdemu/freemkv-tools Cargo.toml
[patch.crates-io]
libfreemkv         = { git = "https://github.com/freemkv/libfreemkv",         tag = "vX.Y.Z" }
freemkv-keysources = { git = "https://github.com/freemkv/freemkv-keysources", tag = "vX.Y.Z" }   # not in bdemu/freemkv-tools
```

This patch applies to BOTH the direct deps AND the transitive refs that
`freemkv-unlock-ld` / `freemkv-keysources` make to libfreemkv, **unifying every
libfreemkv reference to a single git-tag source** (no duplicate crate, no
trait-identity mismatch). So a binary release builds the instant its tag exists
— **no crates.io wait.** Target: **~3-5 min** (critical path = the slowest
single binary build matrix).

crates.io publish STILL happens for **external consumers**, but as INDEPENDENT,
parallel work nothing downstream blocks on:
- `libfreemkv` → its own CI publishes on tag (`cargo publish --no-verify`).
- `freemkv-keysources` → `release.sh` runs `cargo publish --no-verify` locally,
  in parallel with the binary builds.

| Repo | Publish target | How |
|---|---|---|
| `libfreemkv` | **crates.io** + git tag | CI auto-publishes on tag (`--no-verify`); binaries git-tag-pin it |
| `freemkv-keysources` | **crates.io** + git tag | `release.sh` publishes locally (`--no-verify`), in parallel; binaries git-tag-pin it |
| `freemkv-unlock` | **git tag only** (NEVER crates.io) | Workspace of unlocker plugins (`ld/` = LibreDrive). Consumers git-pin it |
| `freemkv` / `autorip` / `bdemu` / `freemkv-tools` | binaries (autorip → GHCR on tag) | not on crates.io; git-tag-pin the libs |

**Committed dependency form** (binaries):
- `libfreemkv = "X.Y.Z"` / `freemkv-keysources = "X.Y.Z"` — bare crates.io
  version reqs (kept so the transitive refs match), **redirected to git tags by
  the committed `[patch.crates-io]`** at the bottom of the manifest.
- `freemkv-unlock-ld = { git = ..., tag = "vX.Y.Z" }` (bdemu uses `rev = ...`).
- **NEVER** commit `{ path = ... }` — CI rejects any `Cargo.lock` whose
  libfreemkv source isn't the expected git tag.

**Committed dependency form** (`freemkv-keysources`, a published lib): plain
`libfreemkv = "X.Y.Z"` (crates.io) — keysources publishes to crates.io, so it
CANNOT have a git dep.

**Local dev** overrides the manifest's git-tag patch with a **gitignored
`.cargo/config.toml`** (config-level `[patch.crates-io]` wins over the manifest
one for the same crate) pointing at local sibling paths:
```toml
[patch.crates-io]
libfreemkv = { path = "../libfreemkv" }
freemkv-keysources = { path = "../freemkv-keysources" }
[patch."https://github.com/freemkv/freemkv-unlock"]
freemkv-unlock-ld = { path = "../freemkv-unlock/ld" }
```

### Order (the script does this — do not reorder if doing it by hand)
1. **`freemkv-unlock`** — if changed: bump `ld/Cargo.toml`, push, tag `vX.Y.Z`, push tag. Consumers pin this, so it must exist first.
2. **`libfreemkv`** — bump, push main, tag, push tag. (Kicks its crates.io publish CI — **do NOT wait for it.**)
3. **`freemkv-keysources`** — bump (its `libfreemkv` crates.io req + own version), push, tag. (Tag only here; its crates.io publish is deferred to step 6.)
4. **Confirm the lib TAGS are on the GitHub remote** (`git ls-remote --tags`). Near-instant — this is the ONLY cross-repo barrier, and it is NOT a crates.io wait.
5. **`freemkv` / `autorip` / `bdemu` / `freemkv-tools`** — for each: re-pin the `[patch.crates-io]` git tags (+ base version reqs) to `vX.Y.Z`, regen `Cargo.lock` with the dev `.cargo/config.toml` DISABLED (so the lock references the git tag, not a local path), commit `Cargo.toml` + `Cargo.lock`, push, tag. Each tag kicks an independent CI build immediately. autorip's tag → GHCR image.
6. **`freemkv-keysources` crates.io publish** — `cargo publish --no-verify`, in PARALLEL with the binary builds now running (needs libfreemkv on crates.io; off the binary critical path).

**Regenerate a release `Cargo.lock`** (the v0.18.7 trap):
```bash
mv .cargo/config.toml /tmp/cfg.bak     # disable the dev patch (move OUT of the repo)
rm -f Cargo.lock && cargo +1.86 generate-lockfile
# verify: EXACTLY ONE libfreemkv entry, source = git+...libfreemkv?tag=vX.Y.Z
mv /tmp/cfg.bak .cargo/config.toml
```

### Gotchas (learnings — don't relearn these)
- **The lib TAGS, not the crates.io publish, gate the binaries.** A binary's `Cargo.lock` regen fetches the git tag — so the tag must be PUSHED before the regen. The script confirms this; by hand, push the lib tags first.
- **Exactly ONE libfreemkv in each binary's `Cargo.lock`.** Two entries (a git-tag one + a crates.io one) means the `[patch.crates-io]` git redirect didn't unify the transitive ref → a `trait ... is not satisfied` build error. CI checks the count.
- **keysources still needs libfreemkv on crates.io to PUBLISH** (cargo publish resolves deps from the registry). That's why its publish is step 6, after libfreemkv's crates.io publish lands — but nothing downstream waits on it.
- **`cargo publish --no-verify`** — CI already compiled the exact commit; the default re-verify is a redundant cold build. The script and libfreemkv CI both use `--no-verify`.
- **Check ALL consumers of unlock-ld when rewiring** — `bdemu` consumes it (`freemkv_unlock_ld::profile::load_bundled()`), not just freemkv/autorip.
- **`cargo publish` refuses on a dirty working dir** — the script stashes the dev `.cargo/config.toml` to `/tmp` during publish; by hand, never leave a `.bak` inside the repo.
- **zsh:** `status` is a read-only variable — don't use it as a loop var.
- **Tag must match committed `Cargo.toml` version** (v0.17.2). Commit the bump BEFORE tagging.

### Before / after timing

| | Old (serial via crates.io) | New (fast-release) |
|---|---|---|
| Critical path | lib tag → CI publish → **index propagation** → keysources publish → **propagation** → binaries regen lock → binary matrices | lib tags pushed → confirm tags on remote (seconds) → binary matrices |
| crates.io on binary path? | **yes** (publish + multi-min index lag, ×2) | **no** |
| Tests | serial gate before build | parallel tripwire (build doesn't `needs: test`) |
| `cargo publish` | full re-verify cold build | `--no-verify` |
| Build matrix | 5 targets (1 a no-op) | 4 targets |
| **Wall-clock** | **~10-20 min** | **~3-5 min** (≈ the slowest single binary build matrix; crates.io publishes happen in parallel, off-path) |

### Local dev — fast builds across the shared dep graph

A true `[workspace]` over the separate repos would fight the per-repo release
model (each repo ships its own `Cargo.toml`/`Cargo.lock`), so we DON'T add a
root workspace. Instead each binary's gitignored `.cargo/config.toml` path-patches
libfreemkv/keysources/unlock-ld to the local sibling checkouts, so `cargo build`
in any binary repo compiles against the working tree (not the pinned tags) and
shares one `target/`-relative dep build per repo. To compile the whole graph
once locally, an **optional gitignored root `Cargo.toml`** can declare a virtual
`[workspace]` over the sibling dirs for dev-only builds — keep it out of git so
it never reaches CI (which builds each repo standalone).

---

## Prerequisites

### Toolchain
```bash
rustup toolchain install 1.86 --component clippy,rustfmt
```

CI uses Rust 1.86 pinned in `.github/workflows/ci.yml`. The Mac default toolchain is newer and accepts lints that 1.86 rejects — always use `+1.86` locally before pushing.

### Local Verification Commands
```bash
# All must pass with zero errors/warnings
cargo +1.86 clippy --locked -- -D warnings
cargo +1.86 test --tests
cargo +1.86 build --release
```

Run the Rust 1.86 pre-commit checks (the same fmt + clippy + tests CI runs):
```bash
cargo +1.86 fmt --check                             # all crates
cargo +1.86 clippy --locked -- -D warnings          # all crates
cargo +1.86 test                                    # all crates
cargo +1.86 clippy -p freemkv-autorip --locked -- -D warnings   # one crate
```

---

## Phase 0: Changes & Local Verification

**Before any git operations:**

1. Make code changes to desired crates
2. Run local verification (see above)
3. **STOP IF FAILS** — do not proceed if clippy fails, fix locally first

---

## Manual fallback (only if `release.sh` is unavailable)

> Prefer `release.sh X.Y.Z`. The steps below are the by-hand equivalent.
> Under the fast-release model the binaries git-tag-pin the libs, so libfreemkv
> does **not** need to be on crates.io before the binaries build — only its
> **git tag** must exist. Push lib tags, then build the binaries; the crates.io
> publishes happen in parallel.

## Phase 1: libfreemkv (tag first)

libfreemkv's TAG must exist before downstream crates regen their lockfiles
(their git patch fetches it). Its crates.io publish runs in its own CI — do NOT
wait for it.

### Step 1: Bump Version

Edit `Cargo.toml` to change the `version` field to the new target version:
```bash
cd ~/freemkv/libfreemkv
# Manual edit preferred for clarity:
nano Cargo.toml  # or use your editor
# Change line: version = "OLD" → version = "0.X.Y"

git add Cargo.toml && git commit -m "v0.X.Y: bump version"
git push origin main
```

### Step 2: Tag and Push (kicks crates.io publish CI; do NOT wait)
```bash
cd ~/freemkv/libfreemkv
git tag -a v0.X.Y -m "v0.X.Y" && git push origin v0.X.Y
# Same for freemkv-keysources (bump its libfreemkv crates.io req + version first).
```

**STOP IF TAG PUSH FAILS** — do not proceed. Fix the issue, then retry.

**Do NOT wait for the crates.io publish here.** The binaries git-tag-pin
libfreemkv/keysources, so they only need the TAGS to exist on the remote — which
they now do. (keysources's crates.io publish, in Phase 3, is the only thing that
needs libfreemkv ON crates.io, and it runs in parallel with the binary builds.)

---

## Phase 2: Binary Crates (bdemu, freemkv, autorip, freemkv-tools)

All crates ship the same version number.

### For Each Crate:

#### Step 1: Re-pin git tags + bump Cargo.toml

Update the committed `[patch.crates-io]` git tags AND the base version reqs to
the new version, plus the package version:
```bash
cd ~/freemkv/<crate-name>
# In Cargo.toml:
#   - version = "0.X.Y"
#   - [patch.crates-io] libfreemkv tag = "v0.X.Y"  (and keysources, where present)
#   - libfreemkv = "0.X.Y"  (base req — must be satisfiable by the patched tag;
#                            for a pre-release this must be the EXACT version)
```

#### Step 1b: Regenerate Cargo.lock against the git tags

```bash
mv .cargo/config.toml /tmp/cfg.bak     # disable the dev path patch
rm -f Cargo.lock && cargo +1.86 generate-lockfile
# verify: EXACTLY ONE libfreemkv entry, source = git+...libfreemkv?tag=v0.X.Y
mv /tmp/cfg.bak .cargo/config.toml
```

**STOP IF GENERATE-LOCKFILE FAILS** — usually the lib tag isn't pushed yet, or
the base version req doesn't match the patched tag. Fix and retry.

#### Step 2: Commit Version Bump + Cargo.lock

**Verify version matches expected format:**
```bash
grep '^version' <crate-name>/Cargo.toml
# Should output: version = "0.X.Y" (for this crate)
# The CI verifies this matches the git tag exactly
```

```bash
git add Cargo.toml Cargo.lock && git commit -m "v0.X.Y: bump version"
git push origin main
```

**STOP IF GIT PUSH FAILS** — resolve merge conflicts or other issues before proceeding.

**CRITICAL:** Never tag before committing the Cargo.toml bump. The CI verify job compares `autorip/Cargo.toml` version to git tag and fails on mismatch (bug: v0.17.2).

#### Step 3: Tag (Triggers the binary CI build immediately)
```bash
git tag -a v0.X.Y -m "v0.X.Y" && git push origin v0.X.Y
```

**STOP IF TAG PUSH FAILS** — do not proceed. Fix the issue, then retry.

Repeat for each binary crate. Each tag triggers its own GitHub Actions workflow
**right away** — they build in parallel; none waits on crates.io.

---

## Phase 3 (parallel): keysources crates.io publish

For external consumers only — the binaries don't wait on this. Needs libfreemkv
ON crates.io first (its publish CI from Phase 1):
```bash
cd ~/freemkv/freemkv-keysources
mv .cargo/config.toml /tmp/cfg.bak               # clean tree for publish
cargo +1.86 publish --no-verify                  # CI already compiled this commit
mv /tmp/cfg.bak .cargo/config.toml
```

---

## Phase 4: CI Monitoring

### Verify Version Before Monitoring
```bash
# Confirm version is set correctly in all crates
grep '^version' libfreemkv/Cargo.toml autorip/Cargo.toml freemkv/Cargo.toml bdemu/Cargo.toml
# All should show the same version number (e.g., 0.X.Y)
```

**STOP IF VERSIONS DO NOT MATCH** — do not proceed until all crates have identical versions.

### Monitor autorip CI (Most Critical)
```bash
# Check GitHub Actions: https://github.com/freemkv/autorip/actions
sleep 180 && curl -s "https://api.github.com/repos/freemkv/autorip/actions/runs?tag=v0.X.Y&per_page=1" | python3 -c "import sys,json; d=json.load(sys.stdin); r=d['workflow_runs'][0]; print(f\"Status: {r['status']} -> {r.get('conclusion')}\")"
```

**STOP IF CI FAILS** — do not proceed to deployment. Go to Phase 5 for failure recovery.

**Expected job graph (fast-release):** `verify → { test (parallel tripwire), build (all targets) }`; for autorip, `build → docker → GHCR`. **`build` does NOT `needs: test`** — tests run in parallel and only fail the run; they never block the artifact build. `update-homepage` runs off `verify`/`build`.

Build matrix is **4 targets** (x86_64-apple-darwin was DROPPED — it had a
pre-existing macOS-runner linker issue and built nothing, a no-op leg that only
added wall-clock):
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `aarch64-apple-darwin` (covers macOS)
- `x86_64-pc-windows-msvc`

Per-target artifacts upload as each leg finishes (`fail-fast: false`), so a fast
target's binary isn't gated on the slowest one.

Watchtower on the deploy host polls every ~30s and auto-deploys from `ghcr.io/freemkv/autorip:latest`.

---

## Phase 5: Production Deployment

### Manual Deploy (if needed)

**Pause watchtower first if a rip may be in progress:**
```bash
# Check current state
curl -s https://deploy.example.com/api/state | jq '.status'
# If "ripping", wait for completion before deploying
```

Build and deploy:
```bash
# Build release binary for linux-musl
cd ~/freemkv/autorip
cargo +1.86 build --release --target x86_64-unknown-linux-musl

# Deploy to the host (adjust version as needed)
scp target/x86_64-unknown-linux-musl/release/autorip deploy@deploy.example.com:/tmp/autorip-0.X.Y
ssh deploy@deploy.example.com << 'DEPLOY'
sudo docker cp /tmp/autorip-0.X.Y autorip:/app/autorip
sudo docker restart autorip
sleep 5 && curl http://deploy.example.com/api/version
DEPLOY
```

**STOP IF DEPLOY FAILS** — do not proceed. Check logs, verify container is running, then retry.

### Enable Debug Logging (for troubleshooting)
```bash
curl -X POST https://deploy.example.com/api/debug \
  -H "Content-Type: application/json" \
  -d '{"enabled":true}'

docker logs autorip --tail=500 -f | grep '\[mux\]'
```

---

## Phase 6: Failure Recovery

### If Clippy Fails Locally
Run `cargo +1.86 clippy --locked` first to catch issues before pushing. Common failures:
- `cfg!(feature = "debug")` errors → remove feature check, use only env var
- Missing Cargo.lock commit → ensure both Cargo.toml and Cargo.lock are committed together

**STOP IF CLIPPY FAILS** — do not tag or push until clippy passes with zero warnings.

### If Version Mismatch (CI verify fails)
The CI job compares Cargo.toml version to git tag. If they don't match:
1. Check `<crate-name>/Cargo.toml` version matches expected (e.g., "0.X.Y")
2. Delete old tag, recreate with correct commit SHA:
   ```bash
   git tag -d v0.X.Y && git tag -a v0.X.Y <bump_commit_sha>
   git push origin v0.X.Y --force
   ```

**STOP IF TAG RECREATE FAILS** — verify the commit SHA exists, then retry.

### If CI Build Fails
1. Check workflow logs at https://github.com/freemkv/autorip/actions
2. Fix the issue locally on `main` (do NOT amend the tagged commit)
3. Commit new fix to main: `git push origin main`
4. Delete old tag, recreate with new SHA: `git tag -d v0.X.Y && git tag -a v0.X.Y <new_sha>`
5. Force push tag: `git push origin v0.X.Y --force`

**STOP IF CI FAILS REPEATEDLY** — after 2 failures, investigate root cause before retrying.

### If crates.io Publish Stalls
Wait longer (up to 10 minutes). Verify via API:
```bash
curl https://crates.io/api/v1/crates/libfreemkv | grep version
```

If still failing after 15 min, **STOP** — investigate index sync issues. Do not proceed with downstream releases until libfreemkv is published.

---

## Quick Reference Commands

### Pre-commit Checklist
```bash
# From workspace root (Rust 1.86 — matches CI)
cargo +1.86 fmt --check
cargo +1.86 clippy --locked -- -D warnings
cargo +1.86 test --tests
```

**STOP IF PRE-COMMIT FAILS** — do not proceed until all checks pass.

### Version Bump Pattern (all crates)

**Manual edit preferred for clarity:**
```bash
cd /path/to/crate
nano Cargo.toml  # Change version = "0.X.Z" → "0.X.Y"
git add Cargo.toml && git commit -m "v0.X.Y: bump version" && git push origin main
```

### Tag Creation (NEVER before bump)
```bash
cd /path/to/crate
git tag -a v0.X.Y -m "v0.X.Y" && git push origin v0.X.Y --force
```

**STOP IF TAG PUSH FAILS** — verify commit exists, then retry.

---

## Hard Rules (STOP IMMEDIATELY IF VIOLATED)

1. **Never add `Co-Authored-By: Claude`** to commit messages. One contributor: MattJackson.

2. **Don't tag before bumping Cargo.toml.** CI verify job catches mismatch (v0.17.2 bug). **STOP if you tagged first — delete and recreate the tag.**

3. **Don't skip precommit.** CI's Rust 1.86 catches what Mac default (1.9x) silently accepts. **STOP if clippy fails locally — fix before pushing.**

4. **Don't deploy without `privileged: true`.** Drive enumeration returns 0; UI shows "No drives detected." **STOP deployment if drive_count=0 in logs.**

5. **abort_on_lost_secs=0 means "require perfect rip"**, not "never abort". Default is 0 (perfect-required); set e.g. 30 to tolerate up to 30s of main-movie loss before aborting after retries exhausted.

6. **Pause watchtower before pushing autorip** if a rip is in progress. **STOP and wait for current rip to complete.**

---

## Container Requirements

- **`privileged: true` REQUIRED** for optical SCSI drive access
- Bind mount `/dev:/dev`
- Bind mount `/srv/autorip/config/keys:/root/.config/freemkv` so KEYDB persists across Watchtower restarts

---

## References

- CI workflows: `.github/workflows/ci.yml`, `.github/workflows/release.yml`
- Pre-commit checks (Rust 1.86): `cargo +1.86 fmt --check`, `cargo +1.86 clippy --locked -- -D warnings`, `cargo +1.86 test`
- Release automation: workspace `release.sh`
- Test plan: internal test plan
- Watchtower pause guidance: see release notes

## Critical Warnings (READ BEFORE STARTING)

**DO NOT DEVIATE FROM THIS DOCUMENT.** Each step is mandatory. Skipping or reordering causes failures:

| Bug Version | Deviation | Result |
|-------------|-----------|--------|
| v0.17.2 | Tagged before bumping Cargo.toml | CI verify job failed, release blocked |
| v0.18.7 | Used `cargo update --workspace` | libfreemkv 0.18.6 baked in release image |
| Any time | Skipped Rust 1.86 requirement | Mac default toolchain accepts lints that CI rejects |

**IF ANY STEP FAILS:** STOP immediately. Report the exact error. Do not proceed until resolved.
