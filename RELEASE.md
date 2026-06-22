# freemkv Release Process

**Complete instruction set for releasing to production.**

Replace `X.Y.Z` throughout with your target version. **FOLLOW THIS ORDER EXACTLY. DO NOT THINK. DO NOT DEVIATE. DO NOT OPTIMIZE. These are mandatory instructions, not suggestions.**

**FAILURE MODES FROM DEVIATION:**
- v0.17.2: Tagged before bumping Cargo.toml → CI verify failed
- v0.18.7: Used `cargo update --workspace` instead of manual Cargo.lock regeneration → libfreemkv 0.18.6 still baked in release  
- Any time: Skipping pre-commit → Mac default Rust accepts lints that CI's 1.86 rejects

**RULES:**
1. DO NOT skip steps
2. DO NOT reorder phases
3. DO NOT optimize commands
4. DO NOT think ahead — execute each step, confirm success, then proceed
5. STOP immediately if any step fails — report the error exactly as shown

---

## Dependency & publish model (current — READ FIRST)

As of **1.0.0-rc.3** the workspace is **7 repos** with three different publish
targets. Get this wrong and the release breaks in non-obvious ways.

| Repo | Publish target | How |
|---|---|---|
| `libfreemkv` | **crates.io** | **CI auto-publishes** on tag (`cargo publish`, repo has `CARGO_REGISTRY_TOKEN`) |
| `freemkv-keysources` | **crates.io** | **MANUAL** — its CI only makes a GitHub release. Run `cargo publish` locally (uses your local crates.io credentials) |
| `freemkv-unlock` | **git tag only** (NEVER crates.io) | Public repo; workspace holding the unlocker plugins (`ld/` = LibreDrive). Consumers git-dep it |
| `freemkv` / `autorip` / `bdemu` | binaries (autorip → GHCR image on tag) | not on crates.io |

**Committed dependency form** (all consumers):
- `libfreemkv = "X.Y.Z"` and `freemkv-keysources = "X.Y.Z"` — **bare crates.io versions**
- `freemkv-unlock-ld = { git = "https://github.com/freemkv/freemkv-unlock", tag = "vX.Y.Z" }` — **git dep**
- **NEVER** commit `{ version = "...", path = "..." }` — autorip's release CI rejects any `Cargo.lock` whose libfreemkv source is a local path.

**Local dev** resolves the unpublished crates via a **gitignored `.cargo/config.toml`**
(NOT committed) in each consumer:
```toml
[patch.crates-io]
libfreemkv = { path = "../libfreemkv" }
freemkv-keysources = { path = "../freemkv-keysources" }
[patch."https://github.com/freemkv/freemkv-unlock"]
freemkv-unlock-ld = { path = "../freemkv-unlock/ld" }
```

### Publish ORDER (dependency-driven — do not reorder)
1. **`freemkv-unlock`** — bump `ld/Cargo.toml`, push, `git tag vX.Y.Z` + push tag. (Consumers pin this tag, so it must exist first.)
2. **`libfreemkv`** — bump, push main, tag, push tag → **CI publishes to crates.io**. Wait for it.
3. **`freemkv-keysources`** — bump, push, tag, then **`cargo publish` MANUALLY** (CI won't). Needs libfreemkv on crates.io first.
4. **`freemkv` / `autorip` / `bdemu`** — for each: **regenerate `Cargo.lock` with the local patch DISABLED** so it references crates.io + the unlock git-tag, then commit `Cargo.toml` + `Cargo.lock`, push, tag. autorip's tag builds the GHCR image.

**Regenerate a release `Cargo.lock`** (the v0.18.7 trap — a local-path lock ships the wrong dep):
```bash
mv .cargo/config.toml /tmp/cfg.bak     # disable the local patch (move OUT of the repo, not to a .bak inside it)
rm -f Cargo.lock && cargo generate-lockfile
# verify: libfreemkv/keysources source = registry+...crates.io, unlock-ld source = git+...freemkv-unlock?tag=
mv /tmp/cfg.bak .cargo/config.toml
```

### rc.3 gotchas (learnings — don't relearn these)
- **keysources is NOT auto-published.** Tag success ≠ on crates.io. Its CI run is ~1 min (lint + GH release only); libfreemkv's is ~5 min (build matrix + publish). `cargo publish` keysources by hand.
- **crates.io index LAGS the publish** by minutes. The **Release run / `cargo publish` success is authoritative**, not the index/web API. If downstream `generate-lockfile` says "candidate versions didn't match," the publish may still be propagating — wait, or confirm it actually ran.
- **Check ALL consumers of unlock-ld when rewiring** — `bdemu` consumes it (`freemkv_unlock_ld::profile::load_bundled()` for drive emulation), not just freemkv/autorip. Easy to miss.
- **`cargo publish` refuses on a dirty working dir** — never leave a temp `.bak` *inside* the repo; move the config to `/tmp`.
- **zsh:** `status` is a read-only variable — don't use it as a loop var in release scripts.
- **Tag must match committed `Cargo.toml` version** (v0.17.2). Commit the bump BEFORE tagging.
- **`freemkv-unlock` / delete-to-comply:** libfreemkv ships only the `Unlocker` trait + registry (firmware-clean). Each binary registers a plugin with ONE `register_unlocker(...)` line. No firmware/profiles/blobs anywhere outside `freemkv-unlock`.

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

## Phase 1: libfreemkv Release (First If Applicable)

libfreemkv must be published before downstream crates can use the new version.

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

### Step 2: Tag and Push (Triggers crates.io Publish)
```bash
cd ~/freemkv/libfreemkv
git tag -a v0.X.Y -m "v0.X.Y" && git push origin v0.X.Y
```

**STOP IF TAG PUSH FAILS** — do not proceed. Fix the issue, then retry.

**Wait for crates.io publish (~2-3 minutes)** before proceeding:
```bash
curl https://crates.io/api/v1/crates/libfreemkv | grep version
# Verify the new version appears in response
```

---

## Phase 2: Downstream Crates (bdemu, freemkv, autorip)

All downstream crates must use the same version number.

### For Each Crate (Order: bdemu → freemkv → autorip):

#### Step 1: Bump Cargo.toml

Edit `Cargo.toml` to match libfreemkv version:
```bash
cd ~/freemkv/<crate-name>
nano Cargo.toml  # or use your editor
# Change line: version = "0.X.Z" → version = "0.X.Y"

# Update dependency versions (if applicable, e.g., autorip depends on libfreemkv)
cargo update -p libfreemkv --precise 0.X.Y
```

**STOP IF CARGO UPDATE FAILS** — crates.io may not have published yet. Wait longer.

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

#### Step 3: Tag (Triggers CI)
```bash
git tag -a v0.X.Y -m "v0.X.Y" && git push origin v0.X.Y --force
```

**STOP IF TAG PUSH FAILS** — do not proceed. Fix the issue, then retry.

Repeat for each crate in order (bdemu → freemkv → autorip). Each tag triggers its own GitHub Actions workflow.

---

## Phase 3: CI Monitoring

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

**Expected sequence:** `verify → ci → build (all targets) → docker → GHCR deploy`

Build matrix includes 5 targets:
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin` (works; x86_64-darwin has pre-existing linker issue)
- `x86_64-pc-windows-msvc`

Watchtower on the deploy host polls every ~30s and auto-deploys from `ghcr.io/freemkv/autorip:latest`.

---

## Phase 4: Production Deployment

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

## Phase 5: Failure Recovery

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
