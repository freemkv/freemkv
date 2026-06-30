# freemkv Release Process

**Complete instruction set for releasing to production.**

Replace `X.Y.Z` throughout with your target version.

## TL;DR — one command

```bash
release.sh X.Y.Z            # do it
release.sh --dry-run X.Y.Z  # preview every command first
```

(The release script lives in the maintainer tooling repo.) The script orchestrates the whole cascade in ONE dependency-ordered, idempotent
run (tag freemkv-unlock → libfreemkv → keysources → confirm tags → re-pin binary
git deps → regen locks → tag binaries → print CI URLs). Every crate is
git-tag-only; nothing touches crates.io. The manual phases below are the
fallback / mental model — **prefer the script.**

**FAILURE MODES FROM DEVIATION:**
- v0.17.2: Tagged before bumping Cargo.toml → CI verify failed
- v0.18.7: Used `cargo update --workspace` instead of manual Cargo.lock regeneration → libfreemkv 0.18.6 still baked in release
- Any time: Skipping pre-commit → Mac default Rust accepts lints that CI's 1.86 rejects

---

## The git-tag-only model (off crates.io — READ FIRST)

**Nothing publishes to crates.io anymore.** `freemkv-unlock` is git-tag-only by
policy (never crates.io); `libfreemkv` git-deps it, so libfreemkv can only be
consumed by git tag too; `freemkv-keysources` deps libfreemkv, so it followed.
Every freemkv crate — libs and binaries alike — is consumed end to end by **git
tag**. (External consumers can no longer `cargo add libfreemkv`; they git-dep it.)

**The dependency graph — note the inversion.** `freemkv-unlock` is the BASE:
libfreemkv depends on IT (it defines the `Unlocker` trait libfreemkv
dispatches), not the reverse.

```
freemkv-unlock  ◄── libfreemkv ◄── freemkv-keysources
       ▲                ▲                  ▲
       └── bdemu        └── freemkv / autorip (+ keysources)
```

Each binary (`freemkv`/`autorip`) carries a committed `[patch.crates-io]` that
redirects libfreemkv + freemkv-keysources to a **git tag**:

```toml
# in freemkv / autorip Cargo.toml  (keysources too; bdemu only libfreemkv)
[patch.crates-io]
libfreemkv         = { git = "https://github.com/freemkv/libfreemkv",         tag = "vX.Y.Z" }
freemkv-keysources = { git = "https://github.com/freemkv/freemkv-keysources", tag = "vX.Y.Z" }
```

The patch unifies BOTH the direct dep AND the transitive ref keysources makes to
libfreemkv onto one git-tag source (no duplicate crate, no trait-identity
mismatch). A binary release builds the instant the lib tags exist.

| Repo | Publish target | How |
|---|---|---|
| `freemkv-unlock` | **git tag only** (NEVER crates.io) | Single crate. The BASE — tagged FIRST. libfreemkv + bdemu git-pin it |
| `libfreemkv` | **git tag only** | Off crates.io (git-deps freemkv-unlock). Consumers git-tag-pin it |
| `freemkv-keysources` | **git tag only** | Off crates.io (deps libfreemkv). git-tag-pins libfreemkv; consumers git-tag-pin it |
| `freemkv` / `autorip` / `bdemu` | binaries (autorip → GHCR on tag) | not on crates.io; git-tag-pin the libs |

**Committed dependency forms:**
- binaries + keysources: `libfreemkv = "X.Y.Z"` / `freemkv-keysources = "X.Y.Z"`
  — bare version reqs **redirected to git tags by the committed
  `[patch.crates-io]`** at the bottom of the manifest. **NEVER** commit a
  libfreemkv/keysources `{ path = ... }` — CI rejects any `Cargo.lock` whose
  libfreemkv source isn't the expected git tag.
- libfreemkv + bdemu dep `freemkv-unlock` by a committed
  `{ path = "../freemkv-unlock" }` (it has no crates.io name to
  `[patch.crates-io]`). The release SWAPS path → `{ git, tag }` in the **tagged
  commit** (so the tag is CI-resolvable), then restores the path dep on the
  branch tip. **The branch keeps the path dep; the git-tag form lives only in
  the tag.** (bdemu adds `features = ["emulation"]` — preserved across the swap.)

**Local dev** overrides the manifests' git-tag patches with a **gitignored
`.cargo/config.toml`** (config-level `[patch.crates-io]` wins over the manifest
one) pointing at local sibling paths:
```toml
[patch.crates-io]
libfreemkv = { path = "../libfreemkv" }
freemkv-keysources = { path = "../freemkv-keysources" }
[patch."https://github.com/freemkv/freemkv-unlock"]
freemkv-unlock = { path = "../freemkv-unlock" }
```

### Order (the script does this — do not reorder if doing it by hand)
1. **`freemkv-unlock`** — bump root `Cargo.toml`, `cargo build --all-features`, push, tag `vX.Y.Z`, push tag. The BASE; everything pins it, so it must exist first.
2. **`libfreemkv`** — bump; in the tagged commit, swap `freemkv-unlock` path → `{ git, tag = "vX.Y.Z" }`; push main, tag, push tag; restore the path dep on the branch tip. **Off crates.io — nothing to wait on.**
3. **`freemkv-keysources`** — re-pin its committed `libfreemkv` git tag + bump versions, push, tag. **Off crates.io.**
4. **Confirm the three lib TAGS are on the GitHub remote** (`git ls-remote --tags`). Near-instant — the only cross-repo barrier.
5. **`freemkv` / `autorip` / `bdemu`** — for each: re-pin the `[patch.crates-io]` git tags (+ base version reqs) to `vX.Y.Z`, regen `Cargo.lock` with the dev `.cargo/config.toml` DISABLED (so the lock references the git tag, not a local path), commit `Cargo.toml` + `Cargo.lock`, push, tag. bdemu ALSO swaps its `freemkv-unlock` path → git tag in its tagged commit (then restores). Each tag kicks an independent CI build; autorip's → GHCR image.

**Regenerate a release `Cargo.lock`** (the v0.18.7 trap):
```bash
mv .cargo/config.toml /tmp/cfg.bak     # disable the dev patch (move OUT of the repo)
rm -f Cargo.lock && cargo +1.86 generate-lockfile
# verify: EXACTLY ONE libfreemkv entry, source = git+...libfreemkv?tag=vX.Y.Z
mv /tmp/cfg.bak .cargo/config.toml
```

### Gotchas (learnings — don't relearn these)
- **freemkv-unlock is the BASE — tag it FIRST.** libfreemkv git-deps it; its tag must exist before libfreemkv's tagged commit swaps the path dep → that git tag, and before any binary's lock regen fetches it (transitively via libfreemkv, directly for bdemu).
- **The lib TAGS gate the binaries.** A binary's `Cargo.lock` regen fetches the git tags — so all three lib tags must be PUSHED before the regen. The script confirms this; by hand, push the lib tags first.
- **Exactly ONE libfreemkv in each binary's `Cargo.lock`.** Two entries (e.g. a git-tag one + a stale registry one) means the `[patch.crates-io]` git redirect didn't unify the transitive ref → a `trait ... is not satisfied` build error. CI checks the count.
- **The branch keeps the `freemkv-unlock` PATH dep; the git-tag form lives only in the tag** (libfreemkv + bdemu). The release commits the git form, tags it, then commits a restore. Don't "fix" the branch back to git — local cross-repo dev needs the path.
- **Check ALL consumers of `freemkv-unlock` when rewiring** — `bdemu` consumes its public catalog API (`freemkv_unlock::ld::profiles()`) + the `emulation` feature, not just libfreemkv.
- **zsh:** `status` is a read-only variable — don't use it as a loop var.
- **Tag must match committed `Cargo.toml` version** (v0.17.2). Commit the bump BEFORE tagging.

### Timing

| | git-tag-only model |
|---|---|
| Critical path | tag freemkv-unlock → libfreemkv → keysources → confirm 3 tags on remote (seconds) → binary build matrices |
| crates.io on any path? | **no** (every crate is git-tag-only) |
| Tests | parallel tripwire (build doesn't `needs: test`) |
| Build matrix | 3 binary targets (freemkv / autorip / bdemu) |
| **Wall-clock** | **~3-5 min** (≈ the slowest single binary build matrix) |

### Local dev — fast builds across the shared dep graph

A true `[workspace]` over the separate repos would fight the per-repo release
model (each repo ships its own `Cargo.toml`/`Cargo.lock`), so we DON'T add a
root workspace. Instead each binary's gitignored `.cargo/config.toml` path-patches
libfreemkv/keysources/freemkv-unlock to the local sibling checkouts, so `cargo build`
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
> Every crate is git-tag-only — NOTHING publishes to crates.io. Tag the libs in
> dependency order (freemkv-unlock → libfreemkv → keysources), confirm the tags
> on the remote, then build the binaries.

## Phase 0: freemkv-unlock (the BASE — tag first)

freemkv-unlock is the base of the graph (libfreemkv git-deps it, bdemu deps it),
so its TAG must exist before anything downstream resolves.
```bash
cd ~/freemkv/freemkv-unlock
# bump root Cargo.toml version → 0.X.Y, then:
cargo +1.86 build --all-features                  # proof (covers bdemu's emulation feature)
git add Cargo.toml && git commit -m "v0.X.Y: bump version"
git push origin main
git tag -a v0.X.Y -m "v0.X.Y" && git push origin v0.X.Y
```

## Phase 1: libfreemkv (off crates.io; swap freemkv-unlock path→git in the tag)

libfreemkv's TAG must exist before the binaries regen their lockfiles (their git
patch fetches it). It does **not** go to crates.io. Its committed
`freemkv-unlock = { path = ... }` must become `{ git, tag }` in the TAGGED
commit, then revert to path on the branch tip.
```bash
cd ~/freemkv/libfreemkv
# bump Cargo.toml version → 0.X.Y
# swap the dep:  freemkv-unlock = { git = "https://github.com/freemkv/freemkv-unlock", tag = "v0.X.Y" }
git add Cargo.toml && git commit -m "v0.X.Y: bump version (freemkv-unlock git-pinned for the tag)"
git push origin main
git tag -a v0.X.Y -m "v0.X.Y" && git push origin v0.X.Y
# restore the path dep for ongoing local dev:
# swap back:  freemkv-unlock = { path = "../freemkv-unlock" }
git add Cargo.toml && git commit -m "restore freemkv-unlock path dep for local dev (post-v0.X.Y)"
git push origin main
```

## Phase 2: freemkv-keysources (off crates.io; git-tag-pin libfreemkv)
```bash
cd ~/freemkv/freemkv-keysources
# re-pin its committed [patch.crates-io] libfreemkv tag → v0.X.Y, bump version
git add Cargo.toml && git commit -m "v0.X.Y: bump version"
git push origin main
git tag -a v0.X.Y -m "v0.X.Y" && git push origin v0.X.Y
```

**STOP IF ANY TAG PUSH FAILS** — do not proceed. Fix the issue, then retry.
**No crates.io step** — the binaries only need the three lib TAGS on the remote.

---

## Phase 3: Binary Crates (bdemu, freemkv, autorip)

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
# bdemu ONLY: also swap freemkv-unlock { path } → { git, tag = "v0.X.Y", features = ["emulation"] }
#             for the tagged commit, then restore the path dep after tagging.
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

(There is no crates.io publish phase — every freemkv crate is git-tag-only.)

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

### If a binary's lockfile regen fails (lib tag not resolvable)
Confirm the three lib tags are on their remotes (freemkv-unlock, libfreemkv,
keysources) before regenerating any binary lock:
```bash
for r in freemkv-unlock libfreemkv freemkv-keysources; do
  git -C ~/freemkv/$r ls-remote --tags origin v0.X.Y | grep -q v0.X.Y \
    && echo "$r ✓" || echo "$r ✗ tag missing — push it first"
done
```
A missing tag, or a base version req that the patched tag can't satisfy, makes
`generate-lockfile` fail closed. Push the tag / fix the req, then retry.

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
