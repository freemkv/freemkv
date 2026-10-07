use crate::server::config::Config;
use crate::server::tmdb;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

/// Progress for ONE artifact being moved (e.g. the `.mkv` movie, or its
/// companion `.iso`). Read by the System page's renderMoves() via SSE.
///
/// A single completed rip can move more than one artifact from one staging
/// dir — a movie file and, when `keep_iso` is on, its ISO — so [`MOVE_STATE`]
/// holds a Vec of these, one per planned file, and the UI draws one progress
/// bar per entry (`X-Men: Apocalypse (mkv)` and `X-Men: Apocalypse (iso)`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MoveState {
    /// The title (raw TMDB title, unsanitized — e.g. `X-Men: Apocalypse`).
    pub name: String,
    /// Which artifact this bar tracks: `"iso"`, `"mkv"`, `"m2ts"`, … — derived
    /// from the source file extension. Empty if unknown. The UI labels the bar
    /// `"{name} ({artifact})"`.
    pub artifact: String,
    pub progress_pct: u8,
    pub progress_gb: f64,
    pub total_gb: f64,
    pub speed_mbs: f64,
    pub eta: String,
}

/// Live per-artifact move bars for the staging dir currently being moved.
/// Empty when nothing is moving. One entry per planned file, in move order;
/// because moves within a dir are sequential, at any instant one entry climbs
/// while later ones sit at 0% and earlier ones read 100%.
pub static MOVE_STATE: once_cell::sync::Lazy<Mutex<Vec<MoveState>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(Vec::new()));

/// The on-disk basename of the staging dir currently being moved, or `None`.
/// The Move queue (`build_queue_views`) selects dirs in `StagingState::Done`,
/// and the actively-moving dir stays in `Done` for the duration of the copy —
/// so without this it would appear BOTH as its live progress bars (`_move`) and
/// as a "(moving)" queue row (`_move_queue`), i.e. listed twice. The queue scan
/// reads this and skips the active dir, so the exclusion is by exact on-disk
/// name (robust to any title punctuation the filesystem sanitizer drops) rather
/// than a fragile client-side string match.
pub static ACTIVE_MOVE_DIR: once_cell::sync::Lazy<Mutex<Option<String>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));

// Serializes tests that mutate/observe the process-global move statics
// (MOVE_STATE, ACTIVE_MOVE_DIR, MOVE_ERRORS, MUX_ERRORS). pub(crate) + crate-level so
// tests here and in web.rs lock the SAME mutex and cannot race each other.
#[cfg(test)]
pub(crate) static TEST_STATE_LOCK: Mutex<()> = Mutex::new(());

// Whether another thread can take `m`. Retries briefly so a parallel test's momentary lock
// isn't mistaken for the CALLING thread holding it (which never frees within the window).
#[cfg(test)]
pub(crate) fn test_lock_is_free<T>(m: &Mutex<T>) -> bool {
    (0..200).any(|_| {
        let free = m.try_lock().is_ok();
        if !free {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        free
    })
}

// Clears MOVE_STATE and ACTIVE_MOVE_DIR when the per-directory pass leaves scope, by any path:
// normal completion, a failure continue, or an unwind.
struct MoveStateGuard;

impl MoveStateGuard {
    /// Arm the guard and mark `dir_basename` as the actively-moving staging dir
    /// (so the Move queue excludes it). The returned guard clears both statics
    /// on drop.
    fn arm(dir_basename: String) -> Self {
        *ACTIVE_MOVE_DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir_basename);
        MoveStateGuard
    }
}

impl Drop for MoveStateGuard {
    fn drop(&mut self) {
        // Recover-and-proceed on poison: skipping the clear is exactly the
        // stuck-bar-for-the-process-lifetime failure this guard prevents.
        MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner()).clear();
        *ACTIVE_MOVE_DIR.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

// The artifact tag for a source file, from its extension: "iso", "mkv",
// "m2ts", "mk3d", …. Empty when there is no extension.
fn artifact_label(src: &Path) -> String {
    src.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Per-staging-dir error surfaced to the System page so the user can act
/// on it (e.g. orphaned source files that the container can't unlink due
/// to NFS squash perms). Keyed by staging dir path. The stored entry is
/// always refreshed, but the syslog line is only emitted when the
/// `reason` changes — so repeating the same error on every loop tick
/// updates the UI without spamming the log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MoverError {
    pub path: String,
    pub reason: String,
    pub hint: String,
}

pub static MOVE_ERRORS: once_cell::sync::Lazy<Mutex<BTreeMap<String, MoverError>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(BTreeMap::new()));

fn record_error(path: &str, reason: &str, hint: &str) {
    record_error_with(path, reason, hint, crate::server::log::syslog);
}

// record_error with the log sink injected. The MOVE_ERRORS guard is dropped
// before `log` runs: syslog does blocking (NFS) I/O, mirroring muxer::record_error.
fn record_error_with(path: &str, reason: &str, hint: &str, log: impl FnOnce(&str)) {
    let same_reason = {
        let mut m = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
        let same_reason = m.get(path).map(|e| e.reason == reason).unwrap_or(false);
        m.insert(
            path.to_string(),
            MoverError {
                path: path.to_string(),
                reason: reason.to_string(),
                hint: hint.to_string(),
            },
        );
        same_reason
    };
    if !same_reason {
        log(&format!("Move blocked: {} — {}", path, reason));
    }
}

fn clear_error(path: &str) {
    MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

// Drop any MOVE_ERRORS row keyed by this DESTINATION path, on every exit from move_file that
// leaves valid bytes at dest.
fn clear_stale_dest_error(dest: &Path) {
    clear_error(&dest.to_string_lossy());
}

// Drop MOVE_ERRORS rows whose staging dir is gone. `seen` is every staging child this pass
// listed; a key is dropped only if it is a direct child of staging_root AND was absent.
fn prune_move_errors(staging_root: &str, seen: &std::collections::HashSet<String>) {
    let root = Path::new(staging_root);
    // Recover-and-proceed on poison, matching record_error's peers: leaving
    // the map untouched is the unbounded growth this exists to stop.
    let mut m = MOVE_ERRORS.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|key, _| Path::new(key).parent() != Some(root) || seen.contains(key));
}

/// Operator-initiated clear of a single move error (the System-tab ✕). Removes
/// it from the in-memory map; if the underlying block is still real, the next
/// mover tick re-records it, so dismissing a genuinely-solved error makes it
/// stay gone while a still-stuck one reappears within a tick.
pub fn clear_move_error(path: &str) {
    clear_error(path);
}

/// Operator-initiated clear of ALL move errors (the System-tab "Clear all").
/// Same self-healing semantics: still-real blocks re-record on the next tick.
pub fn clear_all_move_errors() {
    MOVE_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// Outcome of moving a single file. Distinguishes between an active move
/// (Moved / MovedDirty) and a no-op re-check (Skipped) so the caller can
/// suppress webhook spam and log noise on subsequent loop ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveOutcome {
    /// Dest already exists with the same size as src — already moved on
    /// a previous tick. Source may or may not still be present.
    Skipped,
    /// Atomic rename succeeded — src is gone, dest has the bytes.
    Moved,
    /// Copy succeeded but unlink of src failed (perm/NFS issue). Dest has
    /// the bytes. Caller should record an error and stop trying to clean
    /// the staging dir; subsequent ticks will Skip.
    MovedDirty,
    /// Copy itself failed. Caller can retry on the next tick.
    Failed,
    /// Post-copy size check found dst != src even though the copy returned
    /// success. Surfaces distinctly so a half-copied destination (e.g. NFS
    /// server ran out of space mid-copy without surfacing an error) isn't
    /// treated as a successful move. Caller leaves the staging dir alone —
    /// dst is the broken copy, src is the source of truth.
    SizeMismatch,
    /// Post-copy validation failed for a NON-size reason: a structural
    /// check (missing EBML head, short/garbled tail, insufficient TS sync)
    /// or an unreadable destination. Kept distinct from `SizeMismatch` so
    /// the operator gets an accurate hint — an ENOSPC/short-write hint is
    /// wrong for a structurally-invalid-but-correctly-sized copy. Like
    /// `SizeMismatch`, the caller leaves the staging dir alone (src is the
    /// source of truth) and retries next tick.
    PostCopyInvalid,
    /// Destination already exists as a DIFFERENT file (present, non-empty, and a
    /// different size than src, or the same size with different content). The
    /// different-size case is refused by `check_and_move`'s guard; `move_file`
    /// itself refuses only the same-size case and replaces a different-size dest. A wrong title match can resolve two distinct
    /// discs to the same `Title (Year)/Title (Year).ext` path; overwriting would
    /// destroy a good prior rip. We refuse the move, leave the new file in
    /// staging, and surface a collision error for the operator to resolve.
    Collision,
}

// Errors from the post-copy validation step inside move_file. Distinct from
// MoveOutcome (the move-loop's view) — this is the validation helper's view
// of why the copy was rejected; unit-testable via check_post_copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveError {
    /// The post-copy `stat()` of src and dst disagreed on length. `cp`
    /// returned 0 but the destination is short (or, in pathological
    /// cases, longer than src). Reported via the same `record_error`
    /// path that surfaces other move failures on the System page.
    SizeDoesNotMatch { src_size: u64, dst_size: u64 },
    /// MKV-specific: the destination didn't start with the EBML magic
    /// `1A 45 DF A3`. Either the cp truncated at the head, or the
    /// destination wasn't really an MKV to begin with.
    MkvBadHead,
    /// MKV-specific: the destination is too short, or its tail bytes
    /// couldn't be read back. This is a truncation/readability gate, not
    /// a structural EBML parse — see `check_post_copy_mkv`.
    MkvBadTail,
    /// TS / m2ts: not enough sync bytes (0x47) at TS-packet boundaries
    /// in the file head or tail to consider the file structurally
    /// sound. Likely a truncated cp.
    M2tsBadSync,
    /// Could not open the destination for read (NFS gone away, perm,
    /// etc.). Treat as a serious post-copy condition that warrants
    /// quarantine.
    Unreadable(String),
}

impl std::fmt::Display for MoveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveError::SizeDoesNotMatch { src_size, dst_size } => write!(
                f,
                "post-cp size mismatch: src={} bytes, dst={} bytes",
                src_size, dst_size
            ),
            MoveError::MkvBadHead => {
                write!(f, "destination MKV missing EBML header (1A 45 DF A3)")
            }
            MoveError::MkvBadTail => write!(f, "destination MKV tail too short or unreadable"),
            MoveError::M2tsBadSync => write!(
                f,
                "destination m2ts has insufficient TS sync (0x47) at packet boundaries"
            ),
            MoveError::Unreadable(e) => write!(f, "destination unreadable: {}", e),
        }
    }
}

// Stat a path while bypassing the NFS attribute cache — opens a fresh FD and
// fstats it. Use instead of std::fs::metadata within an attribute-cache
// window (NFS acregmin, default 3s) of a write by another process.
fn fresh_metadata(path: &Path) -> std::io::Result<std::fs::Metadata> {
    let f = std::fs::File::open(path)?;
    f.metadata()
}

// Cheap content-identity probe for two files KNOWN to be the same length: compares a fixed-size
// head/tail window from each; any read error conservatively returns false.
fn same_head_and_tail(a: &Path, b: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: u64 = 64 * 1024;

    fn windows(path: &Path, window: u64) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
        let mut f = std::fs::File::open(path)?;
        let size = f.metadata()?.len();
        let n = window.min(size) as usize;
        let mut head = vec![0u8; n];
        f.read_exact(&mut head)?;
        let mut tail = vec![0u8; n];
        f.seek(SeekFrom::End(-(n as i64)))?;
        f.read_exact(&mut tail)?;
        Ok((head, tail))
    }

    match (windows(a, WINDOW), windows(b, WINDOW)) {
        (Ok(wa), Ok(wb)) => wa == wb,
        _ => false,
    }
}

// Copy src -> dest in 4 MiB chunks, publishing the running bytes-written count into `written`
// as we go, so the move loop can show real progress without stat()-ing the destination.
fn copy_counting(
    src: &Path,
    dest: &Path,
    written: &std::sync::atomic::AtomicU64,
) -> std::io::Result<u64> {
    copy_counting_cancellable(src, dest, written, &|| {
        crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed)
    })
}

// copy_counting with the abort signal injected, for testability without touching the
// process-global crate::server::SHUTDOWN (which every mover test shares).
fn copy_counting_cancellable(
    src: &Path,
    dest: &Path,
    written: &std::sync::atomic::AtomicU64,
    cancel: &dyn Fn() -> bool,
) -> std::io::Result<u64> {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;

    // Write to a sibling temp on the DEST filesystem, fsync it, then rename(2)
    // over the final name. Writing directly risks a truncated file at the
    // real name if killed mid-copy, wedging the move on a phantom Collision.
    let tmp = {
        let mut name = dest.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".part-{}", std::process::id()));
        dest.with_file_name(name)
    };
    // Remove any stale temp from a prior interrupted run before we start.
    let _ = std::fs::remove_file(&tmp);
    // The temp name embeds OUR pid, so the line above only clears our own
    // name; orphaned `.part-<other-pid>` temps from prior crashed runs
    // would otherwise linger. Scan the dest parent and remove any of them.
    if let Some(parent) = dest.parent()
        && let Some(stem) = dest.file_name().and_then(|n| n.to_str())
    {
        let prefix = format!("{stem}.part-");
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries {
                // Don't `.flatten()` away per-entry errors: a partial NFS
                // degradation can error on a DirEntry, skipping a `.part-*`
                // orphan; without this WARN, orphans accumulate silently.
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            dir = %parent.display(),
                            "mover: cannot read dir entry while clearing orphaned \
                             .part-* temps; an orphan may be left behind"
                        );
                        continue;
                    }
                };
                if let Some(name) = entry.file_name().to_str()
                    && name.starts_with(&prefix)
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    let copy_to_tmp = || -> std::io::Result<u64> {
        let mut reader = std::fs::File::open(src)?;
        let mut writer = std::fs::File::create(&tmp)?;
        let mut buf = vec![0u8; 4 * 1024 * 1024];
        let mut total = 0u64;
        loop {
            // Honour SIGTERM between chunks: without this the shutdown join
            // blocks for the whole remaining copy until docker stop's grace
            // expires and SIGKILL lands mid-write; Interrupted unlinks the temp.
            if cancel() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "copy aborted: shutdown requested",
                ));
            }
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            total += n as u64;
            written.store(total, Ordering::Relaxed);
        }
        writer.flush()?;
        // fsync the temp before rename: move_file unlinks the source once
        // this returns Ok, so the dest must be durable first. flush() is a
        // no-op on NFS; without sync_all a crash here loses the only copy.
        writer.sync_all()?;
        Ok(total)
    };

    let total = match copy_to_tmp() {
        Ok(t) => t,
        Err(e) => {
            // Drop the partial temp so the next attempt starts clean and
            // no orphan lingers on the dest fs.
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };

    // fsync the dest parent dir so the temp's dirent is durable, rename(2)
    // over the final name, then fsync again so the rename is durable before
    // move_file unlinks the source: a crash never leaves a truncated file.
    if let Some(parent) = dest.parent() {
        libfreemkv::io::fsync::dir(parent);
    }
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Some(parent) = dest.parent() {
        libfreemkv::io::fsync::dir(parent);
    }
    Ok(total)
}

// Verify a destination MKV via the EBML head magic (1A 45 DF A3) and
// readable tail bytes. A truncation/readability gate, NOT a structural
// EBML parse — the mux already validated the stream when it wrote the file.
fn check_post_copy_mkv(dst: &Path) -> Result<(), MoveError> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(dst).map_err(|e| MoveError::Unreadable(e.to_string()))?;

    // Head: EBML magic 1A 45 DF A3 in the first 4 bytes.
    let mut head = [0u8; 4];
    f.read_exact(&mut head)
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    if head != [0x1A, 0x45, 0xDF, 0xA3] {
        return Err(MoveError::MkvBadHead);
    }

    // Tail: confirm the last 8 bytes are readable (not truncated to zero).
    // We do NOT structurally parse EBML — the mux already validated the
    // stream on write; this only catches a cp that truncated the output.
    let size = f
        .metadata()
        .map_err(|e| MoveError::Unreadable(e.to_string()))?
        .len();
    if size < 5 {
        return Err(MoveError::MkvBadTail);
    }
    let tail_len = 8u64.min(size);
    f.seek(SeekFrom::End(-(tail_len as i64)))
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    let mut tail = [0u8; 8];
    let read = f
        .read(&mut tail[..tail_len as usize])
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    if read < tail_len as usize {
        return Err(MoveError::MkvBadTail);
    }
    Ok(())
}

// Verify a destination m2ts has plausible TS sync bytes (0x47) at 192-byte
// BD-TS packet boundaries (4-byte arrival-time prefix + 188-byte TS payload,
// sync at offset 4) in the head and tail; a truncated cp won't align.
fn check_post_copy_m2ts(dst: &Path) -> Result<(), MoveError> {
    use std::io::{Read, Seek, SeekFrom};

    const PKT: u64 = 192;
    const SYNC_OFFSET: usize = 4;
    const SAMPLE_PACKETS: u64 = 8;
    const THRESHOLD: usize = 6; // out of 2 * SAMPLE_PACKETS (head + tail = 16 samples)

    let mut f = std::fs::File::open(dst).map_err(|e| MoveError::Unreadable(e.to_string()))?;
    let size = f
        .metadata()
        .map_err(|e| MoveError::Unreadable(e.to_string()))?
        .len();
    // Require room for two DISTINCT, non-overlapping sample windows: with a
    // single window a small file's head and tail windows would overlap, so
    // one intact head could double-count and let a tail-truncated cp pass.
    if size < PKT * SAMPLE_PACKETS * 2 {
        return Err(MoveError::M2tsBadSync);
    }

    let mut count = 0usize;
    let mut buf = vec![0u8; (PKT * SAMPLE_PACKETS) as usize];

    // Head
    f.read_exact(&mut buf)
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    for i in 0..SAMPLE_PACKETS as usize {
        let off = i * PKT as usize + SYNC_OFFSET;
        if buf[off] == 0x47 {
            count += 1;
        }
    }

    // Tail
    f.seek(SeekFrom::End(-((PKT * SAMPLE_PACKETS) as i64)))
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    f.read_exact(&mut buf)
        .map_err(|e| MoveError::Unreadable(e.to_string()))?;
    for i in 0..SAMPLE_PACKETS as usize {
        let off = i * PKT as usize + SYNC_OFFSET;
        if buf[off] == 0x47 {
            count += 1;
        }
    }

    // 6 / 16 sync bytes is loose, giving cushion for a non-BD-TS m2ts
    // variant with a different prefix layout, while still catching a
    // truncated cp where the tail packets are all garbage.
    if count < THRESHOLD {
        return Err(MoveError::M2tsBadSync);
    }
    Ok(())
}

/// Verify a destination by size only, using a fresh-FD stat that
/// bypasses the NFS attribute cache. Used for ISO files (no
/// lightweight structural check available without parsing UDF).
fn check_post_copy_size(src: &Path, dst: &Path) -> Result<(), MoveError> {
    // Do NOT default to 0 on a stat failure: the old `unwrap_or(0)` let a
    // failed dst stat plus a failed src stat validate as 0 == 0, after
    // which move_file would remove_file(src) and destroy the only copy.
    let dst_size = fresh_metadata(dst)
        .map_err(|e| MoveError::Unreadable(format!("dst stat failed: {e}")))?
        .len();
    let src_size = fresh_metadata(src)
        .map_err(|e| MoveError::Unreadable(format!("src stat failed: {e}")))?
        .len();
    if src_size != dst_size {
        return Err(MoveError::SizeDoesNotMatch { src_size, dst_size });
    }
    Ok(())
}

type StructuralCheck = fn(&Path) -> Result<(), MoveError>;

// The format-aware structural check for a destination, routed by extension
// (case-insensitive). None for formats without one (iso, unknown).
fn structural_check(dst: &Path) -> Option<StructuralCheck> {
    let ext = dst
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    match ext.as_str() {
        // mk3d is byte-identical Matroska — same check as mkv.
        "mkv" | "mk3d" => Some(check_post_copy_mkv),
        "m2ts" => Some(check_post_copy_m2ts),
        _ => None,
    }
}

// Format-aware post-cp validation: the structural check (when the format has one), then a
// fresh-FD size compare.
pub(crate) fn check_post_copy(src: &Path, dst: &Path) -> Result<(), MoveError> {
    // Structural checks only inspect a fixed head/tail window, so a cp
    // truncated beyond it still passes (DATA-LOSS: move_file then unlinks
    // the source). Always pair with the fresh-FD size compare too.
    if let Some(check) = structural_check(dst) {
        check(dst)?;
    }
    check_post_copy_size(src, dst)
}

/// The structural half of `check_post_copy` for a dest whose SOURCE is gone
/// (the src-missing idempotent fast path) — there is no src left to size-compare.
/// A format without a structural check (iso, unknown) can't be told apart from a
/// foreign file at that path, so it is NOT accepted: reporting Moved would tear
/// down staging and announce a delivery we cannot vouch for.
fn dest_structural_ok(dst: &Path) -> bool {
    structural_check(dst).is_some_and(|check| check(dst).is_ok())
}

// One pass of the mover loop: take a config SNAPSHOT, release the lock, then move. Returns
// false if the config could not be read. The move is injected so this is testable.
fn mover_tick(cfg: &Arc<RwLock<Config>>, do_move: impl FnOnce(&Config)) -> bool {
    let snapshot = match cfg.read() {
        Ok(c) => c.clone(),
        Err(e) => {
            tracing::warn!(error = %e, "mover: config lock poisoned, retrying");
            return false;
        }
    };
    do_move(&snapshot);
    true
}

pub fn run(cfg: &Arc<RwLock<Config>>) {
    use std::sync::atomic::Ordering;
    tracing::info!("mover loop starting");
    while !crate::server::SHUTDOWN.load(Ordering::Relaxed) {
        if !mover_tick(cfg, check_and_move) {
            std::thread::sleep(std::time::Duration::from_secs(10));
            continue;
        }
        // SHUTDOWN-responsive sleep — break early on signal so SIGTERM
        // doesn't have to wait the full 10 s tick.
        for _ in 0..100 {
            if crate::server::SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    tracing::info!("mover loop stopping");
}

// Staging dirs already surfaced as stranded (a Fault from
// classify_done_absence). The mover rescans every ~10s, so track warned
// dirs to report each ONCE instead of re-WARNing every tick.
static STRANDED_WARNED: once_cell::sync::Lazy<Mutex<std::collections::HashSet<String>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(std::collections::HashSet::new()));

// Drop STRANDED_WARNED entries whose staging dir is gone. Mirrors prune_move_errors, same
// liveness guarantee.
fn prune_stranded_warned(seen: &std::collections::HashSet<String>) {
    let mut m = STRANDED_WARNED.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|key| seen.contains(key));
}

/// How the mover should treat a failed `.done` read on a staging dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoneAbsence {
    /// `.done` is absent but a governing marker (`.sweeping`/`.muxing`/
    /// `.ripped`/`.completed`/`.failed`/`.review`) shows the ripper/mux worker
    /// still owns the dir — expected "not ready yet" state. Quiet debug, skip
    /// (no WARN).
    InProgress,
    /// A real fault: non-NotFound read error (NFS ESTALE, EACCES), or a
    /// NotFound on a stranded dir with no governing marker. Worth a WARN.
    Fault,
}

// Classify a.done read error. NotFound + any governing marker present is the by-design
// in-progress state; everything else is a fault.
fn classify_done_absence(err_kind: std::io::ErrorKind, dir: &Path) -> DoneAbsence {
    if err_kind == std::io::ErrorKind::NotFound {
        // The staging dir was removed between the `.done` read and now (move
        // finalised, or stop-cleanup): not a stranded dir, so don't WARN.
        if !dir.exists() {
            return DoneAbsence::InProgress;
        }
        // `.sweeping`/`.muxing` mean "not ready", not stranded `Fault` (else
        // a 182-warn flood on one healthy disc). Use retrying
        // `snapshot_staging_disc`, not bare `exists()`, to dodge cold-NFS false negatives.
        let governed = crate::server::ripper::staging::snapshot_staging_disc(dir)
            .map(|s| {
                s.has_sweeping
                    || s.has_muxing
                    || s.has_ripped
                    || s.completed
                    || s.has_failed
                    || s.has_review
            })
            .unwrap_or(false);
        if governed {
            return DoneAbsence::InProgress;
        }
    }
    DoneAbsence::Fault
}

fn check_and_move(cfg: &Config) {
    // Scan staging directory for completed rips (dirs whose state.json is in
    // StagingState::Done; a legacy `.done` file is the fallback for un-migrated
    // dirs — see the per-dir readiness check below).
    let staging_root = &cfg.staging_dir;
    let entries = match std::fs::read_dir(staging_root) {
        Ok(e) => e,
        Err(e) => {
            // Don't swallow this: a dropped NFS mount or a deleted staging
            // root surfaces here, and a silent return makes the mover look
            // healthy while moving nothing. Make it observable.
            tracing::warn!(
                staging = %staging_root,
                error = %e,
                "mover: failed to read staging directory; skipping this pass"
            );
            return;
        }
    };

    // Every staging child this pass listed, for the `MOVE_ERRORS` prune
    // below, including skipped dirs — a still-ripping dir is present but
    // not ready, and must keep any error row it already has.
    let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in entries {
        // Don't silently drop a per-entry error (e.g. NFS ESTALE on a
        // specific dentry): on a loaded share a completed rip would be
        // missed for the whole tick with no trace. Log and skip.
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    staging = %staging_root,
                    error = %e,
                    "mover: per-entry error listing staging root; skipping entry"
                );
                continue;
            }
        };
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        seen_dirs.insert(dir.to_string_lossy().to_string());

        let marker_path = dir.join(".done");

        // Readiness comes from `state.json` when present: a dir is the
        // mover's to file iff `state == Done`; every other state is "not
        // mine yet". Legacy dirs with no `state.json` fall back to `.done`.
        let (marker, state_outputs): (
            serde_json::Value,
            Vec<crate::server::ripper::staging::Output>,
        ) = match crate::server::ripper::staging::read_state(&dir) {
            Some(st) => {
                if st.state != crate::server::ripper::staging::StagingState::Done {
                    // Not handed off to the mover (in progress, held for
                    // review, terminal, or completed) — by-design "not
                    // ready", so keep it quiet: no per-tick WARN spam.
                    tracing::debug!(
                        dir = %dir.display(),
                        state = ?st.state,
                        "mover: staging dir not in Done state; skipping"
                    );
                    continue;
                }
                (mover_marker_value(&st), st.outputs.clone())
            }
            None => {
                // Legacy `.done` file path. No pre-flight exists() check: it
                // races with the read (the file can appear/disappear between
                // syscalls); the read arms are the atomic gate.
                match std::fs::read_to_string(&marker_path) {
                    Ok(data) => match serde_json::from_str(&data) {
                        Ok(v) => (v, Vec::new()),
                        Err(e) => {
                            // Empty/torn `.done` → NOT READY: skip rather than
                            // blind-move under a garbage name.
                            tracing::warn!(
                                marker = %marker_path.display(),
                                error = %e,
                                "mover: .done marker is empty/unparsable; skipping staging dir (not ready)"
                            );
                            continue;
                        }
                    },
                    Err(e) => {
                        // An ABSENT `.done` on a governed dir is expected
                        // in-progress state — quiet debug, skip (the
                        // 182-warn bug). Only a stranded dir is a fault.
                        if classify_done_absence(e.kind(), &dir) == DoneAbsence::InProgress {
                            tracing::debug!(
                                dir = %dir.display(),
                                "mover: staging dir in progress (no .done yet); skipping"
                            );
                            continue;
                        }
                        // Stranded/unreadable dir (Fault). WARN ONCE per
                        // dir — the mover rescans every ~10s and would
                        // otherwise re-WARN forever. First → WARN, then debug.
                        let first = STRANDED_WARNED
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(dir.to_string_lossy().to_string());
                        if first {
                            tracing::warn!(
                                marker = %marker_path.display(),
                                error = %e,
                                "mover: failed to read .done marker; skipping staging dir (stranded: no state.json/.done; further per-tick warns for this dir suppressed)"
                            );
                        } else {
                            tracing::debug!(
                                marker = %marker_path.display(),
                                error = %e,
                                "mover: stranded staging dir (no state.json/.done); still skipping"
                            );
                        }
                        continue;
                    }
                }
            }
        };

        let disc_name = marker["disc_name"].as_str().unwrap_or("").to_string();
        let display_name = marker["title"].as_str().unwrap_or(&disc_name).to_string();

        // A parsable-but-content-empty marker carries no usable destination
        // name; filing it would route the MKV to the output root under an
        // empty name. Treat as NOT READY and skip — never blind-move.
        if display_name.trim().is_empty() {
            tracing::warn!(
                marker = %marker_path.display(),
                "mover: .done marker has empty title and disc_name; skipping staging dir (not ready)"
            );
            continue;
        }

        // Build TMDB result from marker
        let tmdb_result = if !marker["title"].is_null() {
            Some(tmdb::TmdbResult {
                title: marker["title"].as_str().unwrap_or("").to_string(),
                // Clamp before the cast: a year above 65535 would wrap to a
                // small number (e.g. 70000 -> 4464) and mislabel the folder.
                // 9999 is well past any real release year.
                year: marker["year"].as_u64().unwrap_or(0).min(9999) as u16,
                poster_url: marker["poster_url"].as_str().unwrap_or("").to_string(),
                overview: marker["overview"].as_str().unwrap_or("").to_string(),
                media_type: marker["media_type"].as_str().unwrap_or("movie").to_string(),
                tmdb_id: marker["tmdb_id"].as_u64().unwrap_or(0),
            })
        } else {
            None
        };
        // Season number parsed from the disc label at rip time (null for movies
        // / unmarked labels). The TV branch of `build_destination` uses it to
        // place the rip under `Show (Year)/Season NN/`.
        let season = marker["season"]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok());

        // Find ripped files. `keep_iso=false` means don't promote the
        // intermediate ISO (pre-0.25.10 moved 90+ GB ISOs live); filtered
        // here, except `output_format == "iso"`, where it IS the deliverable.
        let move_iso =
            cfg.keep_iso || crate::server::ripper::output_is_iso_image(&cfg.output_format);
        let (mut ripped_files, listing_complete): (Vec<std::path::PathBuf>, bool) =
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    collect_ripped_files(entries.map(|r| r.map(|e| e.path())), move_iso, &dir)
                }
                Err(e) => {
                    // Enumerating the staging dir failed (e.g. transient NFS
                    // read_dir error). Without this arm the dir is skipped
                    // silently forever — a `.done` marker never acted on.
                    record_error(
                        &dir.to_string_lossy(),
                        &format!("cannot list staging directory {}: {}", dir.display(), e),
                        "check that the staging mount is healthy and readable",
                    );
                    continue;
                }
            };

        // For a TV rip, `state_outputs` is the AUTHORITATIVE deliverable
        // list — file exactly those episodes. The dir scan can also surface a
        // leftover partial from a failed mux; unfiltered it'd promote as complete.
        let is_tv_plan = state_outputs.iter().any(|o| o.episode.is_some());
        if is_tv_plan {
            let allowed: std::collections::HashSet<&str> =
                state_outputs.iter().map(|o| o.filename.as_str()).collect();
            ripped_files.retain(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                // Keep the intermediate ISO: `outputs[]` never lists it, but a
                // `keep_iso` rip legitimately promotes it via `move_iso`;
                // dropping it here would let staging teardown destroy it.
                is_iso_file(name) || allowed.contains(name)
            });
        }

        if ripped_files.is_empty() {
            // Nothing the mover should promote. Skip; the dir's lifetime
            // is governed by the ripper (which owns the state.json transition
            // and its own ISO-prune in the keep_iso=false multipass path).
            continue;
        }

        let dir_str = dir.to_string_lossy().to_string();

        // From here the pass can publish move progress and leave via several
        // `continue`s, so arm the RAII clear now. This also marks the dir's
        // basename as actively-moving so the Move queue excludes it.
        let active_dir_name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let _move_state = MoveStateGuard::arm(active_dir_name);

        // Build destination paths. For a TV rip, `state_outputs` maps each file
        // to its episode; the mover renames the leaf to `Show S{NN}E{MM}[ -
        // Name].ext` under `Show (Year)/Season NN/`. A movie leaf is unchanged.
        let mut planned_moves: Vec<(std::path::PathBuf, String)> = Vec::new();
        for file_path in &ripped_files {
            let filename = file_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let episode_leaf = tv_episode_leaf(&tmdb_result, &state_outputs, &filename, season);
            let leaf = episode_leaf.as_deref().unwrap_or(&filename);
            let dest = build_destination(cfg, &tmdb_result, leaf, season);
            planned_moves.push((file_path.clone(), dest));
        }

        // Nothing below touches a destination root before it passes a bounded write test:
        // a stale or hung share holds the move (output kept in staging) and never blocks here.
        if let Some(p) = unreachable_root(cfg, &tmdb_result, &planned_moves) {
            record_error(
                &dir_str,
                &format!(
                    "waiting for the output folder: {} ({}: {})",
                    p.message,
                    p.path.display(),
                    p.detail
                ),
                &format!("{} The rip stays in staging until then.", p.hint),
            );
            continue;
        }

        // Two discs of one boxset share a TMDB title/filename; `disc_variant`
        // gives disc 2 `Title (Year)_2.mkv` instead of a collision. ONE variant
        // covers the whole file set (MKV+ISO matched); a STAT failure aborts as `uncertain`.
        let mut uncertain = false;
        let variant = crate::server::util::disc_variant(|n| {
            if uncertain {
                return false;
            }
            for (src, dest) in planned_moves.iter() {
                match dest_claim(src, &dest_with_variant(dest, n)) {
                    DestClaim::Claimable => {}
                    DestClaim::OtherFile => return false,
                    DestClaim::Unknown => {
                        uncertain = true;
                        return false;
                    }
                }
            }
            true
        });
        match variant {
            Some(n) => {
                for (_, dest) in planned_moves.iter_mut() {
                    *dest = dest_with_variant(dest, n);
                }
            }
            None if uncertain => {
                // Left to the collision guard's stat-error branch, which logs
                // the deferral and retries next tick.
            }
            None => {
                // Every one of the 64 variants is held by a DIFFERENT file.
                // Leave base names in place: the collision guard below then
                // refuses the move and surfaces the error. Never overwrite.
                crate::server::log::syslog(&format!(
                    "Move blocked ({}): every disc-variant destination name is taken by a \
                     different file — resolve the library conflict manually",
                    display_name
                ));
            }
        }

        // FAIL-LOUD destination-root validation (Mercy incident hardening):
        // confirm the root exists/is a dir/is writable before creating a
        // subdir — never `create_dir_all` (swallows a rip into the overlay).
        let mut dest_roots: Vec<String> = Vec::new();
        for (src, _dest) in &planned_moves {
            let fname = src.file_name().unwrap_or_default().to_string_lossy();
            let r = destination_root_for(cfg, &tmdb_result, &fname);
            if !dest_roots.iter().any(|existing| existing == &r) {
                dest_roots.push(r);
            }
        }
        let blocked_root = dest_roots.iter().find_map(|r| {
            validate_destination_root(r)
                .err()
                .map(|reason| (r.clone(), reason))
        });
        if let Some((dest_root, reason)) = blocked_root {
            record_error(
                &dir_str,
                &format!(
                    "destination not available — refusing to move (output preserved in staging): {reason}"
                ),
                "the destination directory/mount is missing or not writable. \
                 Check the configured movie/tv/output directory exists and its \
                 bind-mount (e.g. the NAS share) is present and writable. The \
                 mover will NOT auto-create the root — fix the mount, then it \
                 retries on the next tick.",
            );
            crate::server::log::syslog(&format!(
                "Move BLOCKED — destination root {} unavailable: {} (output preserved in staging: {})",
                absolute_for_log(&dest_root),
                reason,
                dir.display()
            ));
            continue;
        }

        // Create destination directories. The ROOT is confirmed present +
        // writable above, so this only materializes the per-title subdir
        // UNDER that real root — never the root (and thus mount) itself.
        let mut dest_ok = true;
        for (_, dest) in &planned_moves {
            if let Some(parent) = Path::new(dest).parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                record_error(
                    &dir_str,
                    &format!(
                        "cannot create destination directory {}: {e}",
                        absolute_for_log(&parent.to_string_lossy())
                    ),
                    "check write permissions on the output / movie / tv directory",
                );
                dest_ok = false;
            }
        }
        if !dest_ok {
            continue;
        }

        // Move files. Each artifact (movie file, and with keep_iso its ISO)
        // gets its OWN progress bar, not one aggregate. Seed one MOVE_STATE
        // entry per file up front (0%) so all bars appear immediately.
        {
            let seeded: Vec<MoveState> = planned_moves
                .iter()
                .map(|(src, _)| MoveState {
                    name: display_name.clone(),
                    artifact: artifact_label(src),
                    progress_pct: 0,
                    progress_gb: 0.0,
                    total_gb: fresh_metadata(src).map(|m| m.len()).unwrap_or(0) as f64
                        / crate::server::util::BYTES_PER_GIB,
                    speed_mbs: 0.0,
                    eta: String::new(),
                })
                .collect();
            *MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner()) = seeded;
        }
        let mut outcomes: Vec<MoveOutcome> = Vec::new();
        let mut announced_moving = false;
        for (i, (src, dest)) in planned_moves.iter().enumerate() {
            let on_progress = move |pct: u8, gb: f64, total_gb: f64, speed: f64| {
                // Per-file ETA from THIS file's own remaining bytes (not an
                // aggregate) — the bar and its ETA describe one artifact.
                let eta = if speed > 1.0 && total_gb > gb {
                    let secs = ((total_gb - gb) * 1024.0 / speed) as u32;
                    format!("{}:{:02}", secs / 60, secs % 60)
                } else {
                    String::new()
                };
                let mut ms = MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = ms.get_mut(i) {
                    entry.progress_pct = pct;
                    entry.progress_gb = gb;
                    entry.total_gb = total_gb;
                    entry.speed_mbs = speed;
                    entry.eta = eta;
                }
            };
            // Overwrite guard (defence in depth): never clobber a DIFFERENT dest. Same-size
            // dest is content-probed (head+tail) to distinguish idempotent re-move from a real
            // collision; non-NotFound stat errors defer instead of risking move_file.
            let dest_meta = match fresh_metadata(Path::new(dest)) {
                Ok(d) => Some(d),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => {
                    crate::server::log::syslog(&format!(
                        "Move deferred (could not stat destination {}): {} — will retry next tick",
                        dest, e
                    ));
                    outcomes.push(MoveOutcome::Failed);
                    continue;
                }
            };
            if let Some(d) = dest_meta {
                // Dest exists. We need a fresh stat of the source too; a
                // transient src-stat error here is likewise conservative —
                // defer rather than risk clobbering an existing dest.
                let s = match fresh_metadata(src) {
                    Ok(s) => s,
                    Err(e) => {
                        crate::server::log::syslog(&format!(
                            "Move deferred (destination {} exists but could not stat source {:?}): {} — will retry next tick",
                            dest, src, e
                        ));
                        outcomes.push(MoveOutcome::Failed);
                        continue;
                    }
                };
                if s.is_file() && d.is_file() && d.len() > 0 {
                    let collision = if s.len() != d.len() {
                        true
                    } else {
                        // Equal sizes: only a confirmed content match is the
                        // idempotent re-move. Anything else is a collision.
                        !same_head_and_tail(src, Path::new(dest))
                    };
                    if collision {
                        crate::server::log::syslog(&format!(
                            "Move blocked (destination exists, different file): {} ({} B) vs existing {} ({} B)",
                            src.display(),
                            s.len(),
                            dest,
                            d.len()
                        ));
                        outcomes.push(MoveOutcome::Collision);
                        continue;
                    }
                }
            }
            let outcome = move_file(src, Path::new(dest), &on_progress);
            outcomes.push(outcome);
            // Peg this artifact's bar to 100% on success. Skipped (idempotent
            // re-move) reports no progress, so it'd sit at 0% otherwise;
            // Moved/MovedDirty may stop short of 100. Failed/Collision stay stalled.
            if matches!(
                outcome,
                MoveOutcome::Moved | MoveOutcome::MovedDirty | MoveOutcome::Skipped
            ) {
                let mut ms = MOVE_STATE.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = ms.get_mut(i) {
                    entry.progress_pct = 100;
                    entry.progress_gb = entry.total_gb;
                    entry.speed_mbs = 0.0;
                    entry.eta = String::new();
                }
            }
            match outcome {
                MoveOutcome::Collision => {}
                MoveOutcome::Skipped => {
                    // Quiet — already moved on a prior tick; no log noise.
                }
                MoveOutcome::Moved => {
                    if !announced_moving {
                        crate::server::log::syslog(&format!(
                            "Moving: {} ({} files)",
                            display_name,
                            ripped_files.len()
                        ));
                        announced_moving = true;
                    }
                    // Log the FULL ABSOLUTE destination so the operator can
                    // see exactly where bytes landed — never a cwd-relative
                    // path that could hide a wrong-filesystem write (Mercy incident).
                    crate::server::log::syslog(&format!(
                        "Moved {} → {}",
                        src.file_name().unwrap_or_default().to_string_lossy(),
                        absolute_for_log(dest)
                    ));
                }
                MoveOutcome::MovedDirty => {
                    if !announced_moving {
                        crate::server::log::syslog(&format!(
                            "Moving: {} ({} files)",
                            display_name,
                            ripped_files.len()
                        ));
                        announced_moving = true;
                    }
                    crate::server::log::syslog(&format!(
                        "Moved {} → {} but source could not be removed",
                        src.file_name().unwrap_or_default().to_string_lossy(),
                        absolute_for_log(dest)
                    ));
                }
                MoveOutcome::Failed => {
                    crate::server::log::syslog(&format!(
                        "Failed to move {} → {}",
                        src.display(),
                        absolute_for_log(dest)
                    ));
                }
                MoveOutcome::SizeMismatch => {
                    crate::server::log::syslog(&format!(
                        "Move blocked (post-cp size mismatch): {:?} -> {}",
                        src, dest
                    ));
                }
                MoveOutcome::PostCopyInvalid => {
                    crate::server::log::syslog(&format!(
                        "Move blocked (post-cp validation failed — structural/unreadable): {:?} -> {}",
                        src, dest
                    ));
                }
            }
        }

        let any_collision = outcomes.iter().any(|o| matches!(o, MoveOutcome::Collision));
        let any_failed = outcomes.iter().any(|o| matches!(o, MoveOutcome::Failed));
        let any_size_mismatch = outcomes
            .iter()
            .any(|o| matches!(o, MoveOutcome::SizeMismatch));
        let any_post_copy_invalid = outcomes
            .iter()
            .any(|o| matches!(o, MoveOutcome::PostCopyInvalid));
        let any_dirty = outcomes
            .iter()
            .any(|o| matches!(o, MoveOutcome::MovedDirty));
        let any_actively_moved = outcomes
            .iter()
            .any(|o| matches!(o, MoveOutcome::Moved | MoveOutcome::MovedDirty));

        if any_collision {
            record_error(
                &dir_str,
                "destination already exists as a different file — not overwriting",
                "another disc of the same title is normally filed alongside as `_2`, `_3`, ... — reaching this means that could not be done: every variant name is taken, or the destination changed mid-move. Verify/rename the existing library file, or correct the title, then re-run; the new rip is preserved in staging.",
            );
            continue;
        }

        // Surface size-mismatch distinctly so the operator knows the dest is
        // the broken side (src is intact). Checked before `any_failed` so a
        // mixed batch surfaces the more diagnostic reason.
        if any_size_mismatch {
            record_error(
                &dir_str,
                "post-cp validation failed: destination size does not match source",
                "check the destination filesystem for ENOSPC / short writes; the mover removes a broken copy it made and retries next tick (if a dst file remains, remove it)",
            );
            continue;
        }

        if any_post_copy_invalid {
            record_error(
                &dir_str,
                "post-cp validation failed: destination is structurally invalid or unreadable",
                "the copy is the correct size but failed a format/readability check (truncated header/tail, bad TS sync, or unreadable dst); the mover removes a broken copy it made and retries next tick (if a dst file remains, remove it) — see device_system.log for the specific check",
            );
            continue;
        }

        if any_failed {
            // Leave the dir alone; mover will retry next tick.
            // Surface a summary error so the UI shows what's failing.
            record_error(
                &dir_str,
                "copy to destination failed",
                "see device_system.log for the underlying error",
            );
            continue;
        }

        // A rip filed as one MKV plus its ISO: remember the pair for the Library.
        crate::server::library::links::record_delivery(
            &cfg.autorip_dir,
            planned_moves.iter().map(|(_, d)| d.as_str()),
        );

        // Webhook: only fire on cycles where we actually moved bits, and before
        // the teardown gates — a later tick sees only Skipped files and can't.
        if any_actively_moved {
            crate::server::webhook::send_move(
                cfg,
                &display_name,
                webhook_output_path(&planned_moves),
            );
        }

        // Every file this pass could SEE is accounted for, not every file that IS there: a
        // listing error drops an entry the destructive remove_dir_all teardown would then
        // silently delete. Treat like a failed copy: leave the dir and retry next tick.
        if !listing_complete {
            crate::server::log::syslog(&format!(
                "Staging teardown skipped — directory could not be fully listed: {}",
                dir.display()
            ));
            continue;
        }

        // Try to tear down the staging dir; if it can't be removed (typically
        // because the orphan source files can't be unlinked), surface the
        // dir on the UI with a remediation hint.
        let cleanup_err = std::fs::remove_dir_all(&dir).err();

        if cleanup_err.is_none() {
            clear_error(&dir_str);
            crate::server::log::syslog(&format!("Move complete: {}", display_name));
        } else if any_dirty {
            record_error(
                &dir_str,
                "destination has the file but source could not be removed",
                "manually `rm -rf` the staging dir from a host that can write to the staging share, or fix the NFS export so the container can unlink files there",
            );
        } else if let Some(e) = cleanup_err {
            record_error(
                &dir_str,
                &format!("staging cleanup failed: {}", e),
                "manually `rm -rf` the staging dir",
            );
        }

        // MOVE_STATE is cleared by `_move_state`'s Drop as this iteration
        // ends — including via every `continue` above.
    }

    prune_move_errors(staging_root, &seen_dirs);
    // Same unbounded-growth prune for the stranded-dir one-time-warn dedup set.
    prune_stranded_warned(&seen_dirs);
}

// The move webhook's `output_path`: the media file, not the ISO archive, and the
// first by name (a TV rip's first episode) so the listing order can't change it.
fn webhook_output_path(planned_moves: &[(std::path::PathBuf, String)]) -> &str {
    let dests = || planned_moves.iter().map(|(_, d)| d.as_str());
    dests()
        .filter(|d| !is_iso_file(d))
        .min()
        .or_else(|| dests().min())
        .unwrap_or("")
}

// The deliverable files in one staging dir, and whether the listing was COMPLETE (false on any
// per-entry error — must not count as a completed move).
fn collect_ripped_files<I>(
    entries: I,
    move_iso: bool,
    dir: &Path,
) -> (Vec<std::path::PathBuf>, bool)
where
    I: IntoIterator<Item = std::io::Result<std::path::PathBuf>>,
{
    let mut files = Vec::new();
    let mut complete = true;
    for entry in entries {
        let p = match entry {
            Ok(p) => p,
            Err(e) => {
                complete = false;
                record_error(
                    &dir.to_string_lossy(),
                    &format!(
                        "per-entry error listing staging directory {}: {}",
                        dir.display(),
                        e
                    ),
                    "check that the staging mount is healthy and readable; \
                     staging contents are unknown for this directory",
                );
                continue;
            }
        };
        if p.extension()
            .and_then(|x| x.to_str())
            // Match case-insensitively: a disc labelled `.MKV`/`.ISO` (or any
            // mixed case from an external tool) is the same deliverable and must
            // not be silently skipped by an exact-case compare.
            .map(|ext| match ext.to_ascii_lowercase().as_str() {
                // mk3d is byte-identical Matroska (3D main feature) —
                // deliver it exactly like mkv.
                "mkv" | "mk3d" | "m2ts" => true,
                "iso" => move_iso,
                _ => false,
            })
            .unwrap_or(false)
        {
            files.push(p);
        }
    }
    (files, complete)
}

// The media type used to ROUTE a planned move, coalescing an empty media_type to the "movie"
// default, matching how an absent one already defaults.
fn routing_media_type(result: &tmdb::TmdbResult) -> &str {
    if result.media_type.is_empty() {
        "movie"
    } else {
        result.media_type.as_str()
    }
}

// Render disc variant `n` onto a destination path by suffixing its file
// STEM: `.../Title (2024).mkv` at variant 3 -> `.../Title (2024)_3.mkv`.
// Variant 1 returns the path unchanged (first/only disc keeps its name).
fn dest_with_variant(dest: &str, variant: u32) -> String {
    if variant <= 1 {
        return dest.to_string();
    }
    let p = Path::new(dest);
    let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
        return dest.to_string();
    };
    let suffixed = crate::server::util::disc_variant_name(stem, variant);
    let name = match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{suffixed}.{ext}"),
        None => suffixed,
    };
    p.with_file_name(name).to_string_lossy().into_owned()
}

/// Whether a move may take a particular destination path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DestClaim {
    /// Free, or already holding THIS rip's own output (an idempotent re-move).
    Claimable,
    /// Held by a different file. Try the next disc variant.
    OtherFile,
    /// We could not find out. NOT a licence to try another name — see
    /// [`dest_claim`].
    Unknown,
}

// Can this move claim `dest` — is it free, or already THIS rip's own output? Uses the same
// size+content-probe evidence as the collision guard so a retried move is idempotent.
fn dest_claim(src: &Path, dest: &str) -> DestClaim {
    let d = match fresh_metadata(Path::new(dest)) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DestClaim::Claimable,
        Err(_) => return DestClaim::Unknown,
    };
    if !d.is_file() || d.len() == 0 {
        return DestClaim::Claimable;
    }
    // Dest exists and is a real file. A source we cannot stat leaves us unable
    // to compare — unknown, not "someone else's".
    let Ok(s) = fresh_metadata(src) else {
        return DestClaim::Unknown;
    };
    if !s.is_file() {
        return DestClaim::Unknown;
    }
    if s.len() == d.len() && same_head_and_tail(src, Path::new(dest)) {
        DestClaim::Claimable
    } else {
        DestClaim::OtherFile
    }
}

/// True when `filename` is a disc image (`.iso`, case-insensitive). Governs
/// whether a delivered file is routed to the configured `iso_dir` archive.
fn is_iso_file(filename: &str) -> bool {
    Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
}

/// Build the legacy-shaped mover metadata `Value` from a unified [`crate::server::ripper::staging::DiscState`],
/// so the rest of `check_and_move` reads `marker["title"]`/`["season"]`/… exactly
/// as it did from the old `.done` JSON body.
fn mover_marker_value(st: &crate::server::ripper::staging::DiscState) -> serde_json::Value {
    serde_json::json!({
        "title": st.title,
        "disc_name": st.disc_name,
        "format": st.disc_format,
        "year": st.year,
        "media_type": st.media_type,
        "tmdb_id": st.tmdb_id,
        "season": st.season,
        "disc": st.disc_number,
        "poster_url": st.tmdb_poster,
        "overview": st.tmdb_overview,
        "date": st.date,
    })
}

// The TV-episode leaf name for a staging `filename`, or None when this is
// not a TV episode output. Names as "Show S{NN}E{MM}[ - Episode Name].ext";
// build_destination folders it under "Show (Year)/Season NN/".
fn tv_episode_leaf(
    tmdb: &Option<tmdb::TmdbResult>,
    outputs: &[crate::server::ripper::staging::Output],
    filename: &str,
    season: Option<u16>,
) -> Option<String> {
    let result = tmdb.as_ref()?;
    if routing_media_type(result) != "tv" {
        return None;
    }
    let out = outputs.iter().find(|o| o.filename == filename)?;
    let episode = out.episode?;
    let season = season.unwrap_or(1);
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mkv");
    let safe_title = crate::server::util::sanitize_path_display(&result.title);
    if out.episode_name.is_empty() {
        Some(format!("{safe_title} S{season:02}E{episode:02}.{ext}"))
    } else {
        // Sanitize the episode name too: it comes from TMDB and can carry path
        // separators / reserved chars (e.g. "Part 1/2", ":") that would escape
        // the season folder or break the write, just like the title.
        let safe_episode = crate::server::util::sanitize_path_display(&out.episode_name);
        Some(format!(
            "{safe_title} S{season:02}E{episode:02} - {safe_episode}.{ext}"
        ))
    }
}

fn build_destination(
    cfg: &Config,
    tmdb: &Option<tmdb::TmdbResult>,
    filename: &str,
    season: Option<u16>,
) -> String {
    // Kept/output disc images archive to their own FLAT folder when `iso_dir` is configured,
    // sibling to the library, not beside the muxed title — one file per title, no per-title
    // subfolder. Empty `iso_dir` falls through below.
    if is_iso_file(filename) && !cfg.iso_dir.is_empty() {
        let root = resolve_media_root(&cfg.output_dir, &cfg.iso_dir);
        let leaf = match tmdb {
            Some(result) => {
                let safe_title = crate::server::util::sanitize_path_display(&result.title);
                let year_str = if result.year > 0 {
                    format!(" ({})", result.year)
                } else {
                    String::new()
                };
                format!("{safe_title}{year_str}.iso")
            }
            // No TMDB match: keep the (sanitized) source filename, mirroring the
            // no-tmdb fall-through the movie/tv branches use.
            None => crate::server::util::sanitize_path_display(filename),
        };
        return join_path(&root, &leaf);
    }
    // Source extension wins. Pre-0.25.7 this hardcoded ".mkv", which collided when
    // keep_iso=true left the mux output and source ISO both planning to the same path,
    // alternately overwriting each other (2026-05-20).
    let src_ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mkv");
    if let Some(result) = tmdb {
        let safe_title = crate::server::util::sanitize_path_display(&result.title);
        match routing_media_type(result) {
            "movie" if !cfg.movie_dir.is_empty() => {
                let year_str = if result.year > 0 {
                    format!(" ({})", result.year)
                } else {
                    String::new()
                };
                // The movie root is `movie_dir` resolved UNDER `output_dir`: relative joins
                // onto output_dir, absolute wins (back-compat). Pre-fix a relative "movies"
                // resolved against container root / — the 2026-06 "Mercy" incident.
                let root = resolve_media_root(&cfg.output_dir, &cfg.movie_dir);
                let dir = join_path(&root, &format!("{safe_title}{year_str}"));
                // Filename carries the year too, matching the folder and the Plex/Jellyfin
                // `Title (Year)/Title (Year).ext` convention (pre-fix the file was bare
                // `Title.ext`).
                let name = format!("{safe_title}{year_str}.{src_ext}");
                join_path(&dir, &name)
            }
            "tv" if !cfg.tv_dir.is_empty() => {
                // Same join fix as the movie branch: `tv_dir` resolved under
                // `output_dir` (relative joins, absolute wins).
                let root = resolve_media_root(&cfg.output_dir, &cfg.tv_dir);
                // Jellyfin/Plex TV layout: `Show (Year)/Season NN/`, zero-padded. Season is
                // parsed from the disc label at rip time; when absent, default to 1 rather
                // than dumping loose under the show.
                let year_str = if result.year > 0 {
                    format!(" ({})", result.year)
                } else {
                    String::new()
                };
                let show_dir = join_path(&root, &format!("{safe_title}{year_str}"));
                let dir = join_path(&show_dir, &format!("Season {:02}", season.unwrap_or(1)));
                // Sanitize the leaf too: the movie branch derives its leaf from a sanitized
                // title, but this branch used the RAW source filename, so a path separator or
                // traversal sequence could escape tv_dir.
                let safe_filename = crate::server::util::sanitize_path_display(filename);
                join_path(&dir, &safe_filename)
            }
            _ => {
                // Sanitize the leaf for consistency with the movie/tv
                // branches (they sanitize; this fallback used the raw leaf,
                // so e.g. "..mkv" would reach output_dir verbatim).
                join_path(
                    &cfg.output_dir,
                    &crate::server::util::sanitize_path_display(filename),
                )
            }
        }
    } else {
        join_path(
            &cfg.output_dir,
            &crate::server::util::sanitize_path_display(filename),
        )
    }
}

// Join a leaf (or relative subpath) onto a base dir via Path::join, so the OS
// path separator is used and a trailing slash on the base can't produce a `//`
// in the delivered path. Replaces the old `format!("{base}/{leaf}")` joins.
fn join_path(base: &str, leaf: &str) -> String {
    // POSIX '/' on every platform: autorip's library paths are Linux/NFS-style and
    // Windows accepts '/' as a separator, so the output is stable across builds
    // (Path::join would otherwise emit '\' on the Windows target).
    Path::new(base)
        .join(leaf)
        .to_string_lossy()
        .replace('\\', "/")
}

// Resolve a media subdirectory (movie_dir/tv_dir/iso_dir) UNDER output_dir via Path::join: a
// relative sub joins onto output_dir, an absolute sub replaces it (back-compat).
fn resolve_media_root(output_dir: &str, sub: &str) -> String {
    if sub.is_empty() {
        return output_dir.replace('\\', "/");
    }
    Path::new(output_dir)
        .join(sub)
        .to_string_lossy()
        .replace('\\', "/")
}

// The configured destination ROOT directory that governs a planned move, mirroring
// build_destination's root selection exactly. Validated present+writable BEFORE creating any
// subdir tree.
fn destination_root(cfg: &Config, tmdb: &Option<tmdb::TmdbResult>) -> String {
    if let Some(result) = tmdb {
        match routing_media_type(result) {
            "movie" if !cfg.movie_dir.is_empty() => {
                return resolve_media_root(&cfg.output_dir, &cfg.movie_dir);
            }
            "tv" if !cfg.tv_dir.is_empty() => {
                return resolve_media_root(&cfg.output_dir, &cfg.tv_dir);
            }
            _ => {}
        }
    }
    resolve_media_root(&cfg.output_dir, "")
}

// The configured root a given output FILE lands under:.iso uses iso_dir when set, everything
// else uses destination_root.
fn destination_root_for(cfg: &Config, tmdb: &Option<tmdb::TmdbResult>, filename: &str) -> String {
    if is_iso_file(filename) && !cfg.iso_dir.is_empty() {
        return resolve_media_root(&cfg.output_dir, &cfg.iso_dir);
    }
    destination_root(cfg, tmdb)
}

// Fail-loud destination-root validation: the configured root must ALREADY EXIST as a directory
// AND be writable, so the caller preserves output in staging on failure.
fn validate_destination_root(root: &str) -> Result<(), String> {
    if root.is_empty() {
        // An empty root means "no configured dir". An empty string would
        // `create_dir_all("")` → cwd-relative writes, the exact
        // silent-wrong-path failure this check exists to close.
        return Err("destination root is empty (no output/movie/tv directory configured)".into());
    }
    let root_path = Path::new(root);
    // 1. The root must be ABSOLUTE. A relative root resolves against the
    //    process cwd — how the incident wrote inside the container. A
    //    destination mount is always an absolute path.
    if !root_path.is_absolute() {
        return Err(format!(
            "destination root '{root}' is not an absolute path; \
             a destination mount must be configured as an absolute path \
             (e.g. /mnt/media/movies) so it can never resolve \
             relative to the container's working directory"
        ));
    }
    // 2. The root must already EXIST as a directory. If it doesn't, the
    //    mount is absent — do NOT create it (that writes into the overlay).
    match std::fs::metadata(root_path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "destination root '{root}' exists but is not a directory"
            ));
        }
        Err(e) => {
            return Err(format!(
                "destination root '{root}' does not exist (mount missing?): {e}"
            ));
        }
    }
    // 3. The root must be WRITABLE. Probe by creating + removing a unique
    //    temp marker (honest test of dir write/exec perms, RO fs, NFS
    //    squash); unique-named so concurrent ticks can't collide.
    let probe = root_path.join(format!(
        ".autorip-writable-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            // Writability is proven; clean up the probe. A failure here is
            // near-impossible right after a successful create_new, but log it
            // rather than leave a zero-byte marker in the user's media library.
            if std::fs::remove_file(&probe).is_err() {
                tracing::warn!(probe = %probe.display(), "writability probe left behind");
            }
            Ok(())
        }
        Err(e) => Err(format!("destination root '{root}' is not writable: {e}")),
    }
}

// The first destination root of `planned` that is stale, unmounted, failing or not answering.
// A root that is missing, read-only or refused is left to `validate_destination_root`'s messages.
fn unreachable_root(
    cfg: &Config,
    tmdb: &Option<tmdb::TmdbResult>,
    planned: &[(std::path::PathBuf, String)],
) -> Option<crate::server::health::Problem> {
    use crate::server::health::{self, Fault};
    let mut roots: Vec<String> = Vec::new();
    for (src, _) in planned {
        let r = destination_root_for(
            cfg,
            tmdb,
            &src.file_name().unwrap_or_default().to_string_lossy(),
        );
        if !r.is_empty() && Path::new(&r).is_absolute() && !roots.contains(&r) {
            roots.push(r);
        }
    }
    roots.iter().find_map(|r| {
        health::preflight("output", Path::new(r)).err().filter(|p| {
            matches!(
                p.fault,
                Fault::Stale | Fault::Io | Fault::Unresponsive | Fault::Unmounted
            )
        })
    })
}

// Fail-loud-EARLY destination check: validates every configured, non-empty destination root
// (movie/tv/output). Each root is checked at once on its own thread and given up on after the
// folder health check's limit, so a hung network mount reads "not responding" instead of
// blocking the caller (startup, a settings save).
pub(crate) fn check_configured_destinations(cfg: &Config) -> Vec<(String, String)> {
    check_destinations_with(
        cfg,
        crate::server::health::CHECK_TIMEOUT,
        validate_destination_root,
    )
}

// `check_configured_destinations` with the limit and the per-root check given (the tests
// inject one that hangs).
fn check_destinations_with<F>(
    cfg: &Config,
    limit: std::time::Duration,
    validate: F,
) -> Vec<(String, String)>
where
    F: Fn(&str) -> Result<(), String> + Clone + Send + 'static,
{
    let roots = configured_destination_roots(cfg);
    std::thread::scope(|scope| {
        let running: Vec<_> = roots
            .into_iter()
            .map(|root| {
                let validate = validate.clone();
                let r = root.clone();
                (
                    root,
                    scope.spawn(move || validate_bounded(&r, limit, validate)),
                )
            })
            .collect();
        running
            .into_iter()
            .filter_map(|(root, h)| match h.join() {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some((root, reason)),
                Err(_) => Some((
                    root.clone(),
                    format!("destination root '{root}' could not be checked"),
                )),
            })
            .collect()
    })
}

// `validate` on `root` through the folder health check's `bounded`: a root that does not
// answer within `limit` fails as not responding, its probe left to finish on its own.
fn validate_bounded<F>(root: &str, limit: std::time::Duration, validate: F) -> Result<(), String>
where
    F: Fn(&str) -> Result<(), String> + Send + 'static,
{
    use crate::server::health::{Bounded, bounded};
    let owned = root.to_string();
    match bounded(Path::new(root), limit, move || validate(&owned)) {
        Bounded::Done(result) => result,
        Bounded::TimedOut | Bounded::Busy => Err(format!(
            "destination root '{root}' is not responding after {}s (a stale network mount?)",
            limit.as_secs().max(1)
        )),
    }
}

// The distinct resolved destination roots `check_configured_destinations` validates.
fn configured_destination_roots(cfg: &Config) -> Vec<String> {
    // Validate the RESOLVED roots — the same joined paths the move actually
    // uses, not the raw relative `movie_dir`/`tv_dir`. Otherwise this would
    // flag a valid relative "movies" as "not absolute", or miss a bad join.
    let movie_root = if cfg.movie_dir.is_empty() {
        None
    } else {
        Some(resolve_media_root(&cfg.output_dir, &cfg.movie_dir))
    };
    let tv_root = if cfg.tv_dir.is_empty() {
        None
    } else {
        Some(resolve_media_root(&cfg.output_dir, &cfg.tv_dir))
    };
    // Deduplicate identical resolved roots (movie_dir resolving to the same
    // path as output_dir is common) so the operator doesn't see the same
    // warning twice.
    let mut seen: Vec<String> = Vec::new();
    for root in [movie_root, tv_root, Some(cfg.output_dir.replace('\\', "/"))]
        .into_iter()
        .flatten()
    {
        if root.is_empty() || seen.contains(&root) {
            continue;
        }
        seen.push(root);
    }
    seen
}

// Render a destination path as an ABSOLUTE path for logging, so the mover
// never logs a cwd-relative path that hides a wrong-filesystem write.
// A relative path is joined onto the process cwd.
fn absolute_for_log(dest: &str) -> String {
    let p = Path::new(dest);
    if p.is_absolute() {
        return dest.to_string();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p).to_string_lossy().to_string(),
        Err(_) => dest.to_string(),
    }
}

// Move a file with idempotent retry semantics: pre-flight Skipped/Collision check, then an
// atomic rename(2), falling back to a worker-thread copy_counting + unlink.
fn move_file(src: &Path, dest: &Path, on_progress: &dyn Fn(u8, f64, f64, f64)) -> MoveOutcome {
    // Fresh-FD stat on both sides: a cache-served stat on NFS can mis-size
    // either side, spuriously tripping the Skipped or src-missing Moved
    // pre-flights below.
    let src_meta = fresh_metadata(src);
    let dest_meta = fresh_metadata(dest);

    // Pre-flight: dest already matches, stopping the infinite re-copy loop when src can't
    // unlink. Equal LENGTH alone doesn't prove equal CONTENT, so a same-size different file is
    // refused here too; a different-size dest is replaced (the caller's guard refuses it).
    if let (Ok(s), Ok(d)) = (&src_meta, &dest_meta)
        && s.is_file()
        && d.is_file()
        && s.len() == d.len()
        && s.len() > 0
    {
        if same_head_and_tail(src, dest) {
            // Equal length + matching head/tail still isn't proof of a DURABLE dest: a prior
            // copy that failed post-copy validation can match these cheap probes. Re-run
            // validation before treating it as already-moved.
            if check_post_copy(src, dest).is_ok() {
                clear_stale_dest_error(dest);
                return MoveOutcome::Skipped;
            }
            crate::server::log::syslog(&format!(
                "Pre-existing destination failed post-copy validation; re-copying: {:?}",
                dest
            ));
            // Fall through to the copy path below.
        } else {
            crate::server::log::syslog(&format!(
                "Move blocked (destination same size but different content): {:?} vs {:?}",
                src, dest
            ));
            return MoveOutcome::Collision;
        }
    }
    // Pre-flight: src missing but dest present. Must be a genuine NotFound — any OTHER stat
    // error leaves src's fate UNKNOWN, so treating it as gone could report Moved for garbage
    // and destroy the real src. Non-NotFound falls through to the copy path instead.
    if let (Err(e), Ok(d)) = (&src_meta, &dest_meta)
        && e.kind() == std::io::ErrorKind::NotFound
        && d.is_file()
        && d.len() > 0
        // A non-empty dest alone isn't proof it's OUR output — a foreign file can
        // sit here, and with src gone we can't content-compare. A structural check
        // rejects garbage/foreign non-media rather than falsely reporting Moved.
        && dest_structural_ok(dest)
    {
        return MoveOutcome::Moved;
    }

    if std::fs::rename(src, dest).is_ok() {
        clear_stale_dest_error(dest);
        return MoveOutcome::Moved;
    }

    let dest_str = dest.to_string_lossy().to_string();
    // Did `dest` positively NOT exist before this attempt? Failure-cleanup below may only
    // delete dest when true — anything else pre-dates us, routinely a valid MovedDirty
    // leftover. Only genuine NotFound counts; any other stat error leaves prior state UNKNOWN.
    let dest_absent_before =
        matches!(&dest_meta, Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    let src_size = src_meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let total_gb = src_size as f64 / crate::server::util::BYTES_PER_GIB;

    // In-process copy on a worker thread (`copy_counting`), counting bytes
    // for live progress — not NFS stat(), which lags and would pin the bar
    // at 0%. Post-copy validation runs before unlink; src stays intact on failure.
    let src_owned = src.to_path_buf();
    let dest_owned = dest.to_path_buf();
    let written = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let written_w = std::sync::Arc::clone(&written);
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<u64>>();
    let copy_handle = std::thread::spawn(move || {
        let _ = tx.send(copy_counting(&src_owned, &dest_owned, &written_w));
    });

    // Match ripping's progressively smoothed display window (10–60 seconds).
    // on_progress derives move ETA from this same recent rate, so a slow start
    // does not keep depressing speed and inflating ETA throughout a long copy.
    let mut speed = freemkv_engine::SpeedEstimator::new();
    speed.observe(std::time::Instant::now(), 0);
    loop {
        match rx.try_recv() {
            Ok(Ok(_bytes)) => {
                let _ = copy_handle.join();
                on_progress(100, total_gb, total_gb, 0.0);
                // Post-copy validation is format-aware (EBML head+tail for
                // mkv, TS-sync for m2ts, fresh-FD stat for iso) so the NFS
                // attribute cache can't phantom-fail it. Runs before unlink.
                if let Err(e) = check_post_copy(src, Path::new(&dest_str)) {
                    crate::server::log::syslog(&format!(
                        "Post-cp validation failed for {}: {}",
                        dest_str, e
                    ));
                    // A broken copy this attempt made must not stay at the library
                    // name: the next tick's disc-variant search would file around it.
                    if dest_absent_before && let Err(rm) = std::fs::remove_file(&dest_str) {
                        crate::server::log::syslog(&format!(
                            "Could not remove the broken copy {dest_str}: {rm}"
                        ));
                    }
                    // Map failure KIND to outcome for an accurate operator
                    // hint: only a length disagreement is SizeMismatch;
                    // structural/readability failures get PostCopyInvalid.
                    return match e {
                        MoveError::SizeDoesNotMatch { .. } => MoveOutcome::SizeMismatch,
                        MoveError::MkvBadHead
                        | MoveError::MkvBadTail
                        | MoveError::M2tsBadSync
                        | MoveError::Unreadable(_) => MoveOutcome::PostCopyInvalid,
                    };
                }
                clear_stale_dest_error(dest);
                return match std::fs::remove_file(src) {
                    Ok(_) => MoveOutcome::Moved,
                    Err(_) => MoveOutcome::MovedDirty,
                };
            }
            Ok(Err(e)) => {
                let _ = copy_handle.join();
                // Remove the partial destination so the next tick retries cleanly instead of a
                // phantom size-mismatch Collision. Only when dest was positively absent before
                // (dest_absent_before) — else it's typically a valid MovedDirty copy.
                if dest_absent_before {
                    match std::fs::remove_file(&dest_str) {
                        Ok(()) => {}
                        // Record ONLY a leftover that is really there: unlink in a dir the
                        // container cannot write reports EACCES, not ENOENT, even with no file
                        // there. Ask the filesystem instead.
                        Err(rm) => {
                            if Path::new(&dest_str).exists() {
                                record_error(
                                    &dest_str,
                                    "partial copy could not be removed",
                                    &format!(
                                        "partial copy could not be removed from {dest_str}; delete manually to unblock ({rm})"
                                    ),
                                );
                            }
                        }
                    }
                } else {
                    crate::server::log::syslog(&format!(
                        "Copy failed; leaving pre-existing destination in place: {}",
                        dest_str
                    ));
                }
                crate::server::log::syslog(&format!("Copy failed for {}: {}", dest_str, e));
                return MoveOutcome::Failed;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Sender dropped without sending — worker panicked. Same
                // ownership rule as the Ok(Err) arm: only clean up a dest
                // this attempt could have created.
                if dest_absent_before {
                    let _ = std::fs::remove_file(&dest_str);
                }
                crate::server::log::syslog(&format!("Copy thread panicked for {}", dest_str));
                return MoveOutcome::Failed;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Honor SIGTERM mid-copy: run()'s shutdown sleep only gates BETWEEN ticks, so
                // a copy would otherwise run until docker stop's grace expires and SIGKILL
                // lands mid-write. Join is bounded to one chunk.
                if crate::server::SHUTDOWN.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = copy_handle.join();
                    // Drop the partial destination so a restart's first tick doesn't wedge on
                    // a size-mismatch Collision — but only if this attempt could have created
                    // it; a pre-existing dest is not ours to delete.
                    if dest_absent_before {
                        let _ = std::fs::remove_file(&dest_str);
                    }
                    crate::server::log::syslog(&format!(
                        "Move aborted (shutdown) mid-copy: {}",
                        dest_str
                    ));
                    return MoveOutcome::Failed;
                }
                // Progress straight from the bytes we've written — no network stat,
                // so sampling cannot stall on the destination or read stale metadata.
                let done = written.load(std::sync::atomic::Ordering::Relaxed);
                let pct = if let Some(p) = done.saturating_mul(100).checked_div(src_size) {
                    p.min(100) as u8
                } else {
                    0
                };
                let gb = done as f64 / crate::server::util::BYTES_PER_GIB;
                let speed_mbs = speed.observe(std::time::Instant::now(), done);
                on_progress(pct, gb, total_gb, speed_mbs);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}

#[cfg(test)]
#[path = "mover_tests.rs"]
mod tests;
