//! freemkv's half of the artifact lock (stop design v5 §2.5): "The lock is taken **only by
//! the freemkv CLI/GUI and the engine `_with` entries**". Every ISO the CLI or the GUI
//! writes is held under `<final>.lock` for the whole write; the engine calls these writes
//! go through (`copy`, `recover_to_iso`, `multipass_rip_staged`) take none, so nothing is
//! acquired twice.

use libfreemkv::io::ArtifactLock;
use std::path::Path;

// The GUI's (the CLI's whole-disc copies take the lock in the engine's `run`); the CLI binary
// builds this module without the GUI off macOS.
/// Take `<iso>.lock`, waiting halt-aware while another freemkv process holds it; the
/// holder's progress is its `.partial` or its mapfile changing (§2.5 T10).
#[allow(dead_code)]
pub(crate) fn hold_iso(iso: &Path, halt: &libfreemkv::Halt) -> libfreemkv::Result<ArtifactLock> {
    let mapfile = freemkv_engine::mapfile_path_for(iso);
    ArtifactLock::acquire(iso, &[mapfile.as_path()], halt)
}

// The GUI's, as `hold_iso`.
/// End a held lock. §2.5: "**Deleted on success** (after the final rename) … **Kept**
/// after Stop, a failure or a crash, where it guards the resumable artifact."
#[allow(dead_code)]
pub(crate) fn release(lock: ArtifactLock, success: bool) {
    if !success {
        return;
    }
    if let Err(e) = lock.delete() {
        tracing::warn!("could not delete the artifact lock: {e}");
    }
}

/// The text for a lock this op could not take (§2.5 "UI text (v5.6, ST4-3)": the key
/// `stop.artifact_lock_failed`), or `None` when `e` is anything else.
pub(crate) fn lock_failed(e: &libfreemkv::Error, artifact: &Path) -> Option<String> {
    if !matches!(
        e,
        libfreemkv::Error::TimedOut {
            op: "artifact_lock"
        }
    ) {
        return None;
    }
    Some(crate::strings::fmt_or(
        "stop.artifact_lock_failed",
        "Could not take the lock on {name}; another process may be using it.",
        &[("name", &artifact.display().to_string())],
    ))
}

#[cfg(test)]
#[path = "artifact_lock_tests.rs"]
mod tests;
