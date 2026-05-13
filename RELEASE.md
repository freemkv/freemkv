# freemkv Release Process

**Complete checklist for releasing v0.18.25 (current) to production.**

Replace `0.18.24` → `0.18.25` throughout with your target version. Follow this order exactly. Skipping steps or changing the sequence will cause CI failures or version mismatches.

---

## Prerequisites

### Toolchain
```bash
rustup toolchain install 1.86 --component clippy,rustfmt
```

CI uses Rust 1.86 pinned in `.github/workflows/ci.yml`. Mac default (e.g., 1.94) accepts lints that 1.86 rejects — always use `+1.86` locally before pushing.

### Local Verification Commands
```bash
# All must pass with zero errors/warnings
cargo +1.86 clippy --locked -- -D warnings
cargo +1.86 test --tests
cargo +1.86 build --release
```

Run `freemkv-private/scripts/precommit.sh` to execute all checks across the workspace:
```bash
cd /Users/mjackson/Developer/freemkv
freemkv-private/scripts/precommit.sh              # all crates
freemkv-private/scripts/precommit.sh autorip      # one crate
freemkv-private/scripts/precommit.sh --no-tests   # fmt+clippy only (faster)
```

---

## Phase 0: Changes & Local Verification

**Before any git operations:**

1. Make code changes to desired crates
2. Run local verification (see above)
3. **Do not commit yet** if clippy fails — fix locally first

---

## Phase 1: libfreemkv Release (First If Applicable)

libfreemkv must be published before downstream crates can use the new version.

### Step 1: Bump Version

Edit `Cargo.toml` to change `version = "0.18.24"` to `version = "0.18.25"` (increment last digit):
```bash
cd /Users/mjackson/Developer/freemkv/libfreemkv
# Manual edit preferred for clarity:
nano Cargo.toml  # or use your editor
# Change line: version = "0.18.24" → version = "0.18.25"

git add Cargo.toml && git commit -m "v0.18.25: bump version"
git push origin main
```

### Step 2: Tag and Push (Triggers crates.io Publish)
```bash
cd /Users/mjackson/Developer/freemkv/libfreemkv
git tag -a v0.18.25 -m "v0.18.25" && git push origin v0.18.25
```

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
cd /Users/mjackson/Developer/freemkv/<crate-name>
nano Cargo.toml  # or use your editor
# Change line: version = "0.18.24" → version = "0.18.25"

# Update dependency versions (if applicable, e.g., autorip depends on libfreemkv)
cargo update -p libfreemkv --precise 0.18.25
```

#### Step 2: Commit Version Bump + Cargo.lock

**Verify version matches expected format:**
```bash
grep '^version' autorip/Cargo.toml
# Should output: version = "0.18.25" (for autorip)
# The CI verifies this matches the git tag exactly
```

```bash
git add Cargo.toml Cargo.lock && git commit -m "v0.18.25: bump version"
git push origin main
```

**CRITICAL:** Never tag before committing the Cargo.toml bump. The CI verify job compares `autorip/Cargo.toml` version to git tag and fails on mismatch (bug: v0.17.2).

#### Step 3: Tag (Triggers CI)
```bash
git tag -a v0.18.25 -m "v0.18.25" && git push origin v0.18.25 --force
```

Repeat for each crate in order. Each tag triggers its own GitHub Actions workflow.

---

## Phase 3: CI Monitoring

### Verify Version Before Monitoring
```bash
# Confirm version is set correctly in all crates
grep '^version' libfreemkv/Cargo.toml autorip/Cargo.toml freemkv/Cargo.toml bdemu/Cargo.toml
# All should show the same version number (e.g., 0.18.25)
```

### Monitor autorip CI (Most Critical)
```bash
# Check GitHub Actions: https://github.com/freemkv/autorip/actions
sleep 180 && curl -s "https://api.github.com/repos/freemkv/autorip/actions/runs?tag=v0.18.25&per_page=1" | python3 -c "import sys,json; d=json.load(sys.stdin); r=d['workflow_runs'][0]; print(f\"Status: {r['status']} -> {r.get('conclusion')}\")"
```

**Expected sequence:** `verify → ci → build (all targets) → docker → GHCR deploy`

Build matrix includes 5 targets:
- `x86_64-unknown-linux-musl`
- `aarch64-unknown-linux-musl`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin` (works; x86_64-darwin has pre-existing linker issue)
- `x86_64-pc-windows-msvc`

Watchtower on rip1 polls every ~30s and auto-deploys from `ghcr.io/freemkv/autorip:latest`.

---

## Phase 4: Production Deployment to rip1

### Manual Deploy (if needed)

**Pause watchtower first if a rip may be in progress:**
```bash
# Check current state
curl -s https://rip1.docker.internal.pq.io/api/state | jq '.status'
# If "ripping", wait for completion before deploying
```

Build and deploy:
```bash
# Build release binary for linux-musl
cd /Users/mjackson/Developer/freemkv/autorip
cargo +1.86 build --release --target x86_64-unknown-linux-musl

# Deploy to rip1 (adjust version as needed)
scp target/x86_64-unknown-linux-musl/release/autorip rip@rip1.docker.internal.pq.io:/tmp/autorip-0.18.25
ssh rip@rip1.docker.internal.pq.io << 'DEPLOY'
sudo docker cp /tmp/autorip-0.18.25 autorip:/app/autorip
sudo docker restart autorip
sleep 5 && curl http://rip1.docker.internal.pq.io/api/version
DEPLOY
```

### Enable Debug Logging (for troubleshooting)
```bash
curl -X POST https://rip1.docker.internal.pq.io/api/debug \
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

### If Version Mismatch (CI verify fails)
The CI job compares Cargo.toml version to git tag. If they don't match:
1. Check `autorip/Cargo.toml` version matches expected (e.g., "0.18.25")
2. Delete old tag, recreate with correct commit SHA:
   ```bash
   git tag -d v0.18.25 && git tag -a v0.18.25 <bump_commit_sha>
   git push origin v0.18.25 --force
   ```

### If CI Build Fails
1. Check workflow logs at https://github.com/freemkv/autorip/actions
2. Fix the issue locally on `main` (do NOT amend the tagged commit)
3. Commit new fix to main: `git push origin main`
4. Delete old tag, recreate with new SHA: `git tag -d v0.18.25 && git tag -a v0.18.25 <new_sha>`
5. Force push tag: `git push origin v0.18.25 --force`

### If crates.io Publish Stalls
Wait longer (up to 10 minutes). Verify via API:
```bash
curl https://crates.io/api/v1/crates/libfreemkv | grep version
```

If still failing after 15 min, investigate index sync issues.

---

## Quick Reference Commands

### Pre-commit Checklist
```bash
# From workspace root
cargo +1.86 clippy --locked -- -D warnings
cargo +1.86 test --tests
freemkv-private/scripts/precommit.sh
```

### Version Bump Pattern (all crates)

**Manual edit preferred for clarity:**
```bash
cd /path/to/crate
nano Cargo.toml  # Change version = "0.18.24" → "0.18.25"
git add Cargo.toml && git commit -m "v0.18.25: bump version" && git push origin main
```

### Tag Creation (NEVER before bump)
```bash
cd /path/to/crate
git tag -a v0.18.25 -m "v0.18.25" && git push origin v0.18.25 --force
```

---

## Hard Rules

1. **Never add `Co-Authored-By: Claude`** to commit messages. One contributor: MattJackson.

2. **Don't tag before bumping Cargo.toml.** CI verify job catches mismatch (v0.17.2 bug).

3. **Don't skip precommit.** CI's Rust 1.86 catches what Mac default (1.9x) silently accepts.

4. **Don't deploy without `privileged: true`.** Drive enumeration returns 0; UI shows "No drives detected."

5. **abort_on_lost_secs=0 means "require perfect rip"**, not "never abort". Default is 0 (perfect-required); set e.g. 30 to tolerate up to 30s of main-movie loss before aborting after retries exhausted.

6. **Pause watchtower before pushing autorip** if a rip is in progress. See `freemkv-private/memory/feedback_release_kills_rip_2026_04_26.md`.

---

## Container Requirements

- **`privileged: true` REQUIRED** for optical SCSI drive access
- Bind mount `/dev:/dev`
- Bind mount `/srv/autorip/config/keys:/root/.config/freemkv` so KEYDB persists across Watchtower restarts

---

## References

- CI workflows: `.github/workflows/ci.yml`, `.github/workflows/release.yml`
- Pre-commit script: `freemkv-private/scripts/precommit.sh`
- Release automation: `freemkv-private/scripts/release.sh`
- Test plan: `freemkv-private/docs/TEST_PLAN.md`
- Watchtower pause guidance: `freemkv-private/memory/feedback_release_kills_rip_2026_04_26.md`
