# Session State: v0.17.2 Deployment + Drive Detection Fix

## Current Status

**Work completed in this session:** Deployed autorip v0.17.1 and fixed v0.17.2 version mismatch issue. Fixed drive detection problem by ensuring privileged mode is set in docker-compose.yml.

**Deployment status:**
- ✅ v0.17.1 built and pushed to GitHub (commit: 3e14aab)
- ⚠️ v0.17.2 had version mismatch bug - tag created before Cargo.toml was updated
- ✅ Fixed by: deleted old tag, updated Cargo.toml to "0.17.2", recreated tag
- ✅ **v0.17.2 successfully deployed and running** (commit: ce03629)

---

## Portainer API Usage Guide

**Key endpoints for container management:**

### 1. Check Container Config/Status
```bash
curl -s -H "X-API-Key: ptr_f8I/jLRmscKjCcA7vbq1DebmTr++3GKxzOYrT07QECo=" \
  "https://portainer-1.docker.pq.io/api/endpoints/1/docker/containers/json?all=true" | jq '.[] | select(.Names == ["/media-autorip"])'
```

### 2. Get Container Logs (stdout/stderr)
```bash
curl -s -H "X-API-Key: ptr_f8I/jLRmscKjCcA7vbq1DebmTr++3GKxzOYrT07QECo=" \
  "https://portainer-1.docker.pq.io/api/endpoints/1/docker/containers/{container_id}/logs?stdout=1&stderr=1" | head -50
```

### 3. Create Exec Instance (returns null, use logs instead)
```bash
curl -s -H "X-API-Key: ptr_f8I/jLRmscKjCcA7vbq1DebmTr++3GKxzOYrT07QECo=" \
  -H "Content-Type: application/json" \
  -X POST "https://portainer-1.docker.pq.io/api/endpoints/1/docker/exec" \
  -d '{"Container": "{id}", "AttachStdout": true, "AttachStderr": false, "Tty": false, "Cmd": ["sh", "-c", "ls /dev/sg*"]}' | jq '.Id'
# Returns: null (exec API doesn't work via Portainer)
```

### 4. Check Real-time Drive Detection
```bash
curl -s "https://rip.docker.internal.pq.io/api/state"
```

---

## Changes Made to Codebase (v0.17.1 Features)

### 1. Config Option: `abort_on_lost_secs`
**File:** `/Users/mjackson/Developer/freemkv/autorip/src/config.rs`

- Added field: `pub abort_on_lost_secs: u64` (default 0 = no loss acceptable)
- Environment variable: `ABORT_ON_LOST_SECS`
- Load from saved settings JSON

### 2. Abort Check After Retry Loop  
**File:** `/Users/mjackson/Developer/freemkv/autorip/src/ripper.rs` (~lines 2289-2350)

After all retry passes complete, loads mapfile and checks if main movie loss exceeds threshold:
```rust
// Load mapfile for abort-on-loss check
let mut main_lost_ms_for_history = 0.0f64;
if cfg_read.max_retries > 0 && bytes_unreadable > 0 {
    let iso_filename = format!("{}.iso", crate::util::sanitize_path_compact(&display_name));
    let mapfile_path_str = format!("{staging}/{iso_filename}.mapfile");
    if let Ok(map) = libfreemkv::disc::mapfile::Mapfile::load(std::path::Path::new(&mapfile_path_str)) {
        use libfreemkv::disc::mapfile::SectorStatus;
        let bad_ranges = map.ranges_with(&[SectorStatus::Unreadable]);
        if !bad_ranges.is_empty() && title_bytes_per_sec > 0.0 {
            main_lost_ms_for_history = bad_ranges
                .iter()
                .map(|(_, size)| *size as f64 / title_bytes_per_sec * 1000.0)
                .fold(0.0f64, f64::max);
        }
    }

    let abort_threshold_ms = (cfg_read.abort_on_lost_secs * 1000) as f64;
    if cfg_read.abort_on_lost_secs > 0 && main_lost_ms_for_history > abort_threshold_ms {
        // Abort — too much data lost even after retries
    } else {
        // Proceed with mux — acceptable loss or no loss
    }
}
```

**Semantics:**
- `abort_on_lost_secs=0`: I want 100% perfect data. If any main movie loss remains after all retries, abort.
- `abort_on_lost_secs=30`: I'll tolerate up to 30s of missing data. Only abort if loss exceeds 30s after retries exhausted.
- Multi-pass mode automatically exits early (line 2167) when `bytes_pending == 0 && bytes_unreadable == 0`, so clean discs skip unnecessary retry passes.

### 2. Abort Check After Retry Loop  
**File:** `/Users/mjackson/Developer/freemkv/autorip/src/ripper.rs` (~lines 2318-2345)

After all retry passes complete, loads mapfile and checks if main movie loss exceeds threshold:
```rust
// Load mapfile for abort-on-loss check
let mut main_lost_ms_for_history = 0.0f64;
if cfg_read.max_retries > 0 {
    let iso_filename = format!("{}.iso", crate::util::sanitize_path_compact(&display_name));
    let mapfile_path_str = format!("{staging}/{iso_filename}.mapfile");
    if let Ok(map) = libfreemkv::disc::mapfile::Mapfile::load(std::path::Path::new(&mapfile_path_str)) {
        use libfreemkv::disc::mapfile::SectorStatus;
        let bad_ranges = map.ranges_with(&[SectorStatus::Unreadable]);
        if title_bytes_per_sec > 0.0 && !bad_ranges.is_empty() {
            main_lost_ms_for_history = bad_ranges
                .iter()
                .map(|(_, size)| *size as f64 / title_bytes_per_sec * 1000.0)
                .fold(0.0f64, f64::max);
        }
    }
}

// Check abort threshold
let abort_threshold_ms = (cfg_read.abort_on_lost_secs * 1000) as f64;
if cfg_read.abort_on_lost_secs > 0 && main_lost_ms_for_history > abort_threshold_ms {
    crate::log::device_log(
        device,
        &format!(
            "Aborting — {:.2}s lost in main movie (threshold: {}s)",
            main_lost_ms_for_history / 1000.0,
            cfg_read.abort_on_lost_secs
        ),
    );
    update_state_with(device, |s| {
        s.status = "error".to_string();
        if s.last_error.is_empty() {
            s.last_error = format!(
                "aborted — {:.2}s lost in main movie (threshold: {}s)",
                main_lost_ms_for_history / 1000.0,
                cfg_read.abort_on_lost_secs
            );
        }
    });
    if let Ok(mut flags) = HALT_FLAGS.lock() {
        flags.remove(device);
    }
    return; // Skip mux entirely
}
```

### 3. Dynamic Pass Count Display
**Files:** `/Users/mjackson/Developer/freemkv/autorip/src/ripper.rs` + `web.rs`

After Pass 1 completes, calculate actual total passes:
- Clean disc (no bad ranges): `total_passes = 2` (Pass 1 + mux)
- Damaged disc: `total_passes = max_retries + 2`

### 4. Web UI Settings
**File:** `/Users/mjackson/Developer/freemkv/autorip/src/web.rs` (~line 765)

Added new setting in Recovery section with `showIf:{key:'rip_mode',value:'multi'}` to only show when multi-pass mode is selected.

### 5. POST Handler Fix (v0.17.2)
**File:** `/Users/mjackson/Developer/freemkv/autorip/src/web.rs` (~line 1380-1390)

Added missing field handling in `handle_settings_post`:
```rust
if let Some(v) = patch.get("abort_on_lost_secs").and_then(|v| v.as_u64()) {
    c.abort_on_lost_secs = v;
}
if let Some(rip_mode) = patch.get("rip_mode").and_then(|v| v.as_str()) {
    c.max_retries = if rip_mode == "single" { 0 } else { c.max_retries };
    c.keep_iso = rip_mode == "multi";
}
```

---

## Release Checklist (UPDATED)

### Pre-Release Verification
1. [ ] Run local build: `cd autorip && cargo build --release`
2. [ ] Verify no compilation errors or warnings
3. [ ] Check all new features are in codebase
4. [ ] Verify libfreemkv dependency version matches (Cargo.lock)

### Release Process
1. **Update Cargo.toml FIRST** (CRITICAL - this was the v0.17.2 bug):
   ```bash
   cd autorip
   # Edit Cargo.toml: change version = "X.X.X" to new version
   git add Cargo.toml && git commit -m "vNEW: bump version"
   git push origin main
   ```

2. **Create tag AFTER Cargo.toml is committed**:
   ```bash
   git tag vNEW COMMIT_SHA  # Use specific commit SHA that has updated version
   git push origin vNEW
   ```

3. **Verify GitHub Actions triggered**:
   ```bash
   curl -s "https://api.github.com/repos/freemkv/autorip/actions/runs?event=release" | jq '.workflow_runs[0:2] | .[] | {name, status, conclusion}'
   ```

4. **Monitor CI/CD pipeline**:
   - Check for `verify` job failure (version mismatch)
   - Wait for `build` and `docker` jobs to complete
   - Docker image pushed to GHCR as `ghcr.io/freemkv/autorip:latest` and `ghcr.io/freemkv/autorip:vNEW`

5. **Verify deployment**:
   ```bash
   # Poll version API (Watchtower ~30s lag)
   for i in {1..6}; do VERSION=$(curl -s "https://rip.docker.internal.pq.io/api/version" | jq -r '.version'); echo "Poll $i: v$VERSION"; [ "$VERSION" = "vNEW" ] && break; sleep 30; done
   ```

### Testing After Deployment (via Portainer API)
1. **Version check**: `curl https://rip.docker.internal.pq.io/api/version`
2. **Container status**: Check via Portainer logs endpoint for startup messages
3. **Drive detection**: Look for log line: `INFO drive enumerated device=sgX path=/dev/sgX vendor=... model=...`
4. **Real-time state**: `curl https://rip.docker.internal.pq.io/api/state` - should show detected drives with `disc_present=true/false`

---

## Known Issues & Fixes

### Issue: Version mismatch in Release workflow
**Symptom:** `verify` job fails with "Cargo.toml says v0.17.1 but tag is v0.17.2"
**Root cause:** Tag created before Cargo.toml was updated
**Fix:** Always update Cargo.toml → commit → push → THEN create tag

### Issue: abort_on_lost_secs not persisted in POST handler
**Symptom:** Setting field disappears after save/load cycle
**Root cause:** Missing `if let Some(v) = patch.get("abort_on_lost_secs")` in `handle_settings_post`
**Fix (v0.17.2):** Added field handling to web.rs POST handler

### Issue: Drive detection failure (fixed v0.17.2)
**Symptom:** `drive_count=0`, "No drives detected" in UI, API shows empty state
**Root cause:** Container deployed without `privileged: true` required for optical SCSI access
**Fix:** Ensure docker-compose.yml has `privileged: true` (line 6 in autorip/docker-compose.example.yml)

---

## Deployment Status: v0.17.2 Successfully Deployed

**Completed:**
- ✅ v0.17.2 tag created and pushed to GitHub (commit ce03629)
- ✅ GitHub Actions Release workflow completed successfully
  - verify job: passed
  - CI job: passed  
  - build job: passed
  - docker job: passed
- ✅ Docker image pushed to GHCR as ghcr.io/freemkv/autorip:v0.17.2 and :latest
- ✅ Watchtower auto-deployed new image to production server

**Production Verification:**
```bash
curl https://rip.docker.internal.pq.io/api/version
# Returns: {"version":"0.17.2"}

curl https://rip.docker.internal.pq.io/api/state | python3 -c "import sys,json; data=json.load(sys.stdin); print('Detected drives:', list(data.keys()))"
# Returns: Detected drives: ['sg4']
```

**v0.17.2 Features:**
1. `abort_on_lost_secs` config option - aborts rip if main movie loss exceeds threshold
2. Dynamic pass count display - shows actual total passes (2 for clean discs, max_retries+2 for damaged)
3. Single/multi-pass mode selection via UI

**Files Modified:**
- `src/config.rs`: Added `abort_on_lost_secs: u64` field with env var support and JSON persistence
- `src/ripper.rs`: Abort check logic after retry loop, dynamic pass count calculation
- `src/web.rs`: UI setting for abort threshold (shows only in multi-pass mode), POST handler fix

**Testing Recommendations:**
1. Set `abort_on_lost_secs` to a low value (e.g., 30) and rip a damaged disc to verify abort behavior
2. Rip a clean disc to verify dynamic pass count shows "pass 1/1" instead of "pass 1/X"

---

## Credentials & URLs

- **Portainer API Token:** `ptr_f8I/jLRmscKjCcA7vbq1DebmTr++3GKxzOYrT07QECo=`
- **GitHub API:** https://api.github.com/repos/freemkv/autorip/actions/runs?event=release
- **Autorip Version API:** https://rip.docker.internal.pq.io/api/version
- **GHCR Image:** ghcr.io/freemkv/autorip:latest

## Host Access (for debugging)

```bash
# SSH into Docker host
ssh docker

# Check if USB optical drive is visible on host
ls -la /dev/sr* 2>&1 || echo "No sr devices"
cat /sys/class/scsi_generic/*/device/type | sort | uniq -c

# Run autorip container manually to see error output
sudo docker run --privileged --rm -v /dev:/dev ghcr.io/freemkv/autorip:latest
```

## Testing Checklist for Deployments

After each deployment, verify via Portainer API:

1. ✅ Container is running (check `/api/containers/json` Status field)
2. ✅ Logs show startup with `drive_count=1` or higher
3. ✅ Log shows drive enumerated: `INFO drive enumerated device=sgX vendor=... model=...`
4. ✅ Real-time API responds: `curl https://rip.docker.internal.pq.io/api/state`
5. ✅ Drive detected in state: `{ "sg4": { "device":"sg4", "disc_present":true, ... } }`
