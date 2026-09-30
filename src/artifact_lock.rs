//! freemkv's half of the artifact lock (stop design v5 §2.5): "The lock is taken **only by
//! the freemkv CLI/GUI and the engine `_with` entries**". Every ISO the CLI or the GUI
//! writes is held under `<final>.lock` for the whole write; the engine calls these writes
//! go through (`copy`, `recover_to_iso`, `multipass_rip_staged`) take none, so nothing is
//! acquired twice.

use libfreemkv::io::ArtifactLock;
use std::path::Path;

/// Take `<iso>.lock`, waiting halt-aware while another freemkv process holds it; the
/// holder's progress is its `.partial` or its mapfile changing (§2.5 T10).
pub(crate) fn hold_iso(iso: &Path, halt: &libfreemkv::Halt) -> libfreemkv::Result<ArtifactLock> {
    let mapfile = freemkv_engine::mapfile_path_for(iso);
    ArtifactLock::acquire(iso, &[mapfile.as_path()], halt)
}

/// End a held lock. §2.5: "**Deleted on success** (after the final rename) … **Kept**
/// after Stop, a failure or a crash, where it guards the resumable artifact."
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
mod tests {
    use super::*;

    // §2.5: "UI text (v5.6, ST4-3): freemkv renders this case with the i18n key
    // `stop.artifact_lock_failed`". Per spec; do not change without a spec citation.
    #[test]
    fn a_lock_timeout_names_the_artifact() {
        let iso = Path::new("/out/Movie.iso");
        let e = libfreemkv::Error::TimedOut {
            op: "artifact_lock",
        };
        let text = lock_failed(&e, iso).expect("E9073 on the lock");
        assert!(text.contains("/out/Movie.iso"), "{text}");
        assert!(!text.contains("{name}"), "{text}");
        assert_eq!(
            text,
            crate::strings::fmt("stop.artifact_lock_failed", &[("name", "/out/Movie.iso")])
        );
        let other = libfreemkv::Error::TimedOut { op: "verify" };
        assert_eq!(lock_failed(&other, iso), None);
        assert_eq!(lock_failed(&libfreemkv::Error::Halted, iso), None);
    }

    // §2.5 "Lifetime": "Deleted on success … Kept after Stop, a failure or a crash".
    // Per spec; do not change without a spec citation.
    #[test]
    fn success_deletes_the_sidecar_and_a_stop_keeps_it() {
        let dir = crate::ku_fixtures::TempDir::new("al-life");
        let iso = dir.path().join("Movie.iso");
        let sidecar = dir.path().join("Movie.iso.lock");
        let halt = libfreemkv::Halt::new();
        release(hold_iso(&iso, &halt).unwrap(), false);
        assert!(sidecar.exists(), "kept after a Stop or a failure");
        release(hold_iso(&iso, &halt).unwrap(), true);
        assert!(!sidecar.exists(), "deleted on success");
    }

    // §2.5 "Progress and wait": the wait is halt-aware; a Stop ends it with `Halted`.
    #[test]
    fn a_held_iso_lock_waits_until_stop() {
        let dir = crate::ku_fixtures::TempDir::new("al-wait");
        let iso = dir.path().join("Movie.iso");
        let _held = hold_iso(&iso, &libfreemkv::Halt::new()).unwrap();
        let halt = libfreemkv::Halt::new();
        let stop = halt.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            stop.cancel();
        });
        let r = hold_iso(&iso, &halt);
        t.join().unwrap();
        assert!(matches!(r, Err(libfreemkv::Error::Halted)), "{r:?}");
    }
}
