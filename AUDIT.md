# freemkv / libfreemkv Code Audit

**Date:** 2026-04-14
**Scope:** Full manual + automated review of both crates
**Codebase:** ~23,400 LOC (libfreemkv) + ~1,400 LOC (freemkv CLI)

---

## 1. Automated Check Results

### 1.1 libfreemkv clippy

14 warnings total, all minor:
- 1 `empty_line_after_doc_comments` in `src/mux/disc.rs:155`
- 1 `type_complexity` in `src/mux/mkvstream.rs:403`
- 12 `doc_overindented_list_items` in `src/scsi/linux.rs:87-103`

**No errors. No logic-affecting warnings.**

### 1.2 freemkv clippy

4 warnings:
- `unused_mut` on `drive` at `src/pipe.rs:223`
- `unused_variable: raw` at `src/pipe.rs:179` and `src/pipe.rs:312`
- `unnecessary_cast` (`u64` -> `u64`) at `src/pipe.rs:249`

### 1.3 Tests

- **libfreemkv:** 41 passed (30 unit + 10 integration + 1 doc-test), 2 doc-tests ignored. 0 failures.
- **freemkv:** 0 tests. No unit test coverage for the CLI.

### 1.4 Formatting

Both crates have minor `cargo fmt` diffs in benches, examples, and a few source files. No blocking issues.

---

## 2. Critical Findings

### C1. Key material not zeroized after use
**Files:** `src/aacs/decrypt.rs`, `src/aacs/keys.rs`, `src/aacs/handshake.rs`, `src/decrypt.rs`
**Severity:** Critical

The `zeroize` crate is in `Cargo.toml` but is never `use`d anywhere in the source. AES keys, VUKs, media keys, unit keys, bus keys, host private keys, and ECDSA scalars all persist in memory as plain `[u8; 16]` / `[u8; 20]` / `[u8; 32]` until the allocator recycles the pages. `DecryptKeys` is `Clone` and contains raw key bytes with no `Drop` zeroization.

**Impact:** Key material extractable from process memory or core dumps. For a product handling DRM keys, this is the highest-priority security fix.

**Fix:**
```rust
// In src/decrypt.rs
use zeroize::Zeroize;

#[derive(Clone)]
pub enum DecryptKeys {
    None,
    Aacs {
        unit_keys: Vec<(u32, [u8; 16])>,
        read_data_key: Option<[u8; 16]>,
    },
    Css { title_key: [u8; 5] },
}

impl Drop for DecryptKeys {
    fn drop(&mut self) {
        match self {
            DecryptKeys::Aacs { unit_keys, read_data_key } => {
                for (_, k) in unit_keys.iter_mut() { k.zeroize(); }
                if let Some(k) = read_data_key { k.zeroize(); }
            }
            DecryptKeys::Css { title_key } => { title_key.zeroize(); }
            _ => {}
        }
    }
}
```
Apply the same pattern to `HostCert`, `DeviceKey`, `DiscEntry`, `AacsState`, and the ECDSA private key scalars in `handshake.rs`.

---

### C2. HTTPS not supported in keydb updater -- HTTP only
**File:** `src/keydb.rs:130`
**Severity:** Critical

`parse_url` requires `http://` prefix and uses raw `TcpStream` -- no TLS. The KEYDB contains cryptographic keys and device identifiers. Downloading over plaintext HTTP exposes this to MITM attacks on any network path.

**Fix:** Either use `ureq` (already in the dependency tree) with TLS, or reject plain HTTP and require `https://`. Example:
```rust
fn http_get(url: &str) -> Result<Vec<u8>> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|_| Error::KeydbConnect { host: url.to_string() })?;
    let mut body = Vec::new();
    resp.into_body().take(100 * 1024 * 1024)
        .read_to_end(&mut body)
        .map_err(|_| Error::KeydbConnect { host: url.to_string() })?;
    Ok(body)
}
```

---

### C3. Silent fallback on decrypt failure -- data corruption risk
**File:** `src/decrypt.rs:47-49`
**Severity:** Critical

```rust
let uk = unit_keys
    .get(unit_key_idx)
    .map(|(_, k)| *k)
    .unwrap_or([0u8; 16]);  // <-- falls back to all-zeros key
```

If `unit_key_idx` is out of range, decryption proceeds with an all-zeros key. This produces garbage output that looks like valid data (no error raised). The user gets a silently corrupted file.

**Fix:** Return an error or skip decryption rather than using a dummy key:
```rust
let uk = match unit_keys.get(unit_key_idx) {
    Some((_, k)) => *k,
    None => return, // skip decryption -- no valid key
};
```

---

## 3. Warning Findings

### W1. Unused `raw` parameter in disc-to-stream pipeline
**File:** `freemkv/src/pipe.rs:179, 312`
**Severity:** Warning

`disc_to_stream()` and `batch_stream()` accept `raw: bool` but never use it. The `--raw` flag works for `disc_to_iso` and single-file pipe but is silently ignored when ripping disc-to-MKV or disc-to-M2TS in batch mode. Users who pass `--raw` expect no decryption but get decrypted output.

**Fix:** Pass `raw` through to the DiscStream and call `stream.set_raw()` when true.

---

### W2. Linux SG_IO: no validation of sg_io_hdr struct layout
**File:** `src/scsi/linux.rs:16-41`
**Severity:** Warning

The `sg_io_hdr` struct is manually defined with `#[repr(C)]` but there is no compile-time assertion that its size matches the kernel's expectation. If the struct has incorrect padding (e.g., on a future architecture or with a different Rust version), the ioctl will silently corrupt memory.

**Fix:** Add a static assertion:
```rust
const _: () = assert!(std::mem::size_of::<sg_io_hdr>() == 64);
```
(The Linux sg_io_hdr is 64 bytes on x86-64. This should be architecture-gated.)

---

### W3. Windows SPTI: DataTransferLength used as bytes_transferred
**File:** `src/scsi/windows.rs:275`
**Severity:** Warning

```rust
bytes_transferred: sptwb.spt.DataTransferLength as usize,
```

After `DeviceIoControl`, `DataTransferLength` reflects the *requested* length, not necessarily the *actual* bytes transferred. On Linux, the equivalent `resid` field is subtracted. Windows may report more bytes than actually returned.

**Impact:** Over-reading the buffer on short transfers could return stale/zeroed data to the caller.

**Fix:** Track the original requested length vs. returned length, or zero the buffer before the call (already done for reads on line 209, which mitigates this).

---

### W4. macOS `unsafe impl Send` on COM interface pointers
**File:** `src/scsi/macos.rs:172`
**Severity:** Warning

```rust
unsafe impl Send for MacScsiTransport {}
```

The comment says "IOKit COM interface pointers are Mach port references -- safe to send between threads." This is true for Mach port *names* (integers), but COM vtable function pointers may have thread affinity. If IOKit creates the plugin on thread A and the vtable functions capture thread-local state, calling them from thread B causes UB.

**Fix:** Document that `MacScsiTransport` must only be created and used on the same thread, or verify with Apple documentation that MMC device interfaces are thread-safe.

---

### W5. Handshake returns partial result on failure
**File:** `src/disc/encrypt.rs:49-53`
**Severity:** Warning

```rust
last_error.map(|_e| HandshakeResult {
    volume_id: [0u8; 16],
    read_data_key: None,
})
```

If all host certs fail, the function returns `Some(HandshakeResult)` with a zeroed volume ID instead of `None`. This means callers think a handshake occurred (with volume_id all zeros) when it actually failed. VUK derivation with a zero volume_id produces an incorrect key, leading to silent decryption failure.

**Fix:** Return `None` when all host certs fail:
```rust
if last_error.is_some() {
    None
} else {
    None
}
// Or simply: always return None if the loop didn't produce a successful result
```

---

### W6. eprintln! used directly in library code
**Files:** `src/drive/mod.rs` (11 occurrences), `src/aacs/keys.rs` (10), `src/aacs/handshake.rs` (2), `src/css/crack.rs` (4)
**Severity:** Warning

A library should not write to stderr. The crate has a proper `Event` system for progress reporting, but the error recovery code in `Drive::read()` bypasses it entirely with direct `eprintln!` calls. This breaks applications that capture stderr, embed the library, or run headless.

**Fix:** Route all diagnostic output through the `Event` system or a `log` crate integration. Remove all `eprintln!` from libfreemkv.

---

### W7. process::exit() scattered through CLI pipe code
**File:** `freemkv/src/pipe.rs` (8 occurrences), `src/main.rs` (8)
**Severity:** Warning

`std::process::exit(1)` bypasses destructors. In the disc rip path, this means `SgIoTransport::drop()` (which unlocks the tray) and `Drive::drop()` (cleanup) never run. The tray stays locked after an error. The signal handler partially addresses this for SIGINT but not for application-level exits.

**Fix:** Return `Result` from pipe functions and let `main()` handle the exit:
```rust
fn pipe(...) -> Result<(), Box<dyn std::error::Error>> {
    // ...
}

fn main() {
    if let Err(e) = run() {
        eprintln!("Error: {}", e);
        std::process::exit(1);  // Only one exit point, after all drops
    }
}
```

---

### W8. No timeout on AACS handshake retries
**File:** `src/disc/encrypt.rs:29-48`
**Severity:** Warning

The handshake loop tries every `host_cert` in the KeyDb sequentially with no overall timeout. A KeyDb with many host certs (the test shows real DBs have 170,000+ entries, though few host certs) combined with a slow/unresponsive drive could block indefinitely.

**Fix:** Add a maximum attempt count or overall timeout to the handshake loop.

---

### W9. Signal handler uses non-async-signal-safe operations
**File:** `freemkv/src/pipe.rs:22-52`
**Severity:** Warning

The SIGINT handler calls `std::process::exit(130)` on second signal, which is not async-signal-safe. This can corrupt heap state if the signal arrives during an allocation.

**Fix:** Use `_exit()` or `libc::_exit(130)` instead of `std::process::exit(130)`:
```rust
extern "C" fn handle_sigint(_sig: libc::c_int) {
    if INTERRUPTED.load(Ordering::Relaxed) {
        unsafe { libc::_exit(130) };
    }
    INTERRUPTED.store(true, Ordering::Relaxed);
}
```

---

## 4. Suggestion Findings

### S1. No CLI unit tests
**File:** `freemkv/src/` (entire crate)
**Severity:** Suggestion

The CLI has 0 tests. URL parsing, flag parsing, batch destination path logic, and the sanitize_name function should all have unit tests. The library has decent coverage but the CLI has none.

---

### S2. Magic numbers in SCSI CDB construction
**Files:** `src/drive/mod.rs`, `src/scsi/mod.rs`, `src/platform/mt1959/mod.rs`
**Severity:** Suggestion

CDB bytes like `[0x1E, 0, 0, 0, 0, 0]` (PREVENT/ALLOW MEDIUM REMOVAL), `[0x1B, 0, 0, 0, 0x02, 0]` (START/STOP UNIT - eject), and `[0x4A, 0x01, ...]` (GET EVENT STATUS NOTIFICATION) appear as bare array literals without named constants.

**Fix:** Define constants in `scsi/mod.rs`:
```rust
pub const SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
pub const SCSI_START_STOP_UNIT: u8 = 0x1B;
pub const SCSI_GET_EVENT_STATUS: u8 = 0x4A;
pub const SCSI_TEST_UNIT_READY: u8 = 0x00;
```

---

### S3. Complex return type in mkvstream
**File:** `src/mux/mkvstream.rs:403`
**Severity:** Suggestion

Clippy flags `io::Result<(DiscTitle, Vec<(u16, Vec<u8>)>)>` as overly complex.

**Fix:** Define a named type:
```rust
type TrackCodecData = Vec<(u16, Vec<u8>)>;
```

---

### S4. Drive::read() is 100+ lines with 3 retry phases
**File:** `src/drive/mod.rs:415-513`
**Severity:** Suggestion

The `read()` method handles normal reads, phase-1 gentle retries, phase-2 fresh start (reopen SCSI), and phase-3 post-restart retries. This is a single 100-line function. Consider extracting each phase into a helper for testability.

---

### S5. Unnecessary clone on profile match
**File:** `src/profile.rs:137, 148`
**Severity:** Suggestion

`profile: p.clone()` clones the entire `DriveProfile` including the firmware blob (which can be large). If profiles were stored behind `Arc`, the clone would be free.

---

### S6. CSS crack test allows non-deterministic failure
**File:** `src/css/crack.rs:326-385`
**Severity:** Suggestion

The `css_crack_recovers_key_from_scrambled_sector` test accepts failure with an `eprintln!` message instead of asserting. A test that silently passes when the attack fails provides no regression protection. Either make the test deterministic with a known-working key/seed pair, or mark it `#[ignore]`.

---

### S7. Network stream has no authentication or encryption
**File:** `src/mux/network.rs:1-5`
**Severity:** Suggestion

The doc comment notes "plain TCP with no encryption" and "TLS support is planned." For a product shipping to users, even LAN-only usage should have a warning at runtime, not just in doc comments. Consider printing a warning when `network://` is used.

---

### S8. IsoSectorReader::capacity() truncates on >8TB images
**File:** `src/mux/iso.rs:36`
**Severity:** Suggestion

```rust
let capacity = (size / SECTOR_SIZE) as u32;
```

A u32 sector count limits capacity to ~8 TB. Current Blu-ray discs max at ~128 GB, so this is not a practical issue today, but it silently truncates if someone processes a very large image.

---

### S9. Redundant `normalize_device_path` function
**File:** `src/scsi/windows.rs:95`
**Severity:** Suggestion

The comment notes "A near-identical `normalize_path` exists in `drive::windows`." This is a maintainability concern -- a bug fix in one copy won't reach the other. Extract to a shared `platform::windows` utility module.

---

### S10. `discover_drives()` missing `#[cfg]` for non-target platforms
**File:** `src/drive/mod.rs:620-633`
**Severity:** Suggestion

The function compiles on all platforms but has no `#[cfg(not(...))]` fallback. On unsupported platforms it would fail to compile. The `open()` function in `scsi/mod.rs` handles this correctly with a catch-all `#[cfg(not(any(...)))]`.

---

## 5. Architecture Assessment

### Strengths

1. **Clean module separation.** SCSI transport, drive management, disc scanning, decryption, and muxing are well-separated. The `SectorReader` trait properly abstracts disc vs. ISO access.

2. **Proper error hierarchy.** Numeric error codes with structured data, no embedded English text. The `Display` impl provides machine-parseable output.

3. **Defensive SCSI layer.** The Linux transport has thorough reset/recovery logic with good documentation of why each step exists. The `resolve_to_sg` function correctly handles sr-to-sg mapping per project conventions.

4. **Stream architecture.** The `IOStream` trait with URL-based dispatch (`open_input`/`open_output`) is clean and extensible. Adding new formats requires minimal changes.

5. **Good test coverage for crypto.** AACS AES roundtrip, CBC roundtrip, CSS LFSR properties, and the Stevenson attack are all well-tested.

6. **Correct endianness handling.** All SCSI CDB construction and disc structure parsing use explicit `from_be_bytes`/`to_be_bytes`. No reliance on native endianness.

### Weaknesses

1. **Key material hygiene.** The `zeroize` dependency exists but is unused. This is the single biggest security gap.

2. **Library code has I/O side effects.** 28 `eprintln!` calls in the library bypass the event system.

3. **CLI lacks structured error handling.** `process::exit()` scattered through pipe functions prevents cleanup.

4. **No integration tests for CLI.** The CLI has zero tests despite having non-trivial URL parsing, flag handling, and batch logic.

---

## 6. Executive Summary

**Overall Health Score: 7 / 10**

The codebase is well-architected with clean module boundaries, proper error types, and thorough SCSI recovery logic. The crypto implementations are correct and tested. However, there are three critical findings that should be addressed before a public release.

### Top 5 Most Impactful Fixes

| Priority | Finding | Impact |
|----------|---------|--------|
| 1 | **C1: Zeroize key material** | Security: cryptographic keys persist in memory indefinitely |
| 2 | **C3: Silent decrypt fallback to zero key** | Correctness: silently produces garbage output on key index mismatch |
| 3 | **C2: HTTP-only keydb updater** | Security: key database downloaded over plaintext, MITM-able |
| 4 | **W5: Handshake returns fake success on failure** | Correctness: zero volume_id causes wrong VUK derivation |
| 5 | **W1: `--raw` flag ignored in batch disc rip** | UX: user intent silently discarded |

Fixes 1-4 are correctness/security issues that affect every rip. Fix 5 is a user-visible bug. Together they represent perhaps 2-3 days of focused work and would significantly improve the product's reliability and security posture.
