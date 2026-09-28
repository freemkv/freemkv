//! ISO-to-MKV links recorded when this app files a rip, so the cross-list
//! pairs its own rips without guessing from titles.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The links file's name in the config folder.
pub const LINKS_FILE: &str = "library-links.json";

static WRITE: Mutex<()> = Mutex::new(());

/// MKV path to the ISO it was ripped from. Missing or unreadable reads as empty.
pub fn load(config_dir: &Path) -> HashMap<PathBuf, PathBuf> {
    std::fs::read(config_dir.join(LINKS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<HashMap<PathBuf, PathBuf>>(&b).ok())
        .unwrap_or_default()
}

/// Remember that `mkv` was muxed from `iso`.
pub fn record(config_dir: &Path, mkv: &Path, iso: &Path) -> std::io::Result<()> {
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = load(config_dir);
    links.insert(mkv.to_path_buf(), iso.to_path_buf());
    let json = serde_json::to_vec_pretty(&links).map_err(std::io::Error::other)?;
    crate::server::ripper::staging::write_marker_durable(&config_dir.join(LINKS_FILE), &json)
}

/// The mover's hook: a disc delivered as exactly one MKV and one ISO is a link.
pub fn record_delivery<'a>(config_dir: &str, delivered: impl IntoIterator<Item = &'a str>) {
    let (mut mkvs, mut isos) = (Vec::new(), Vec::new());
    for d in delivered {
        let p = Path::new(d);
        match p
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
        {
            Some(e) if e == "mkv" => mkvs.push(p),
            Some(e) if e == "iso" => isos.push(p),
            _ => {}
        }
    }
    if let ([mkv], [iso]) = (&mkvs[..], &isos[..])
        && let Err(e) = record(Path::new(config_dir), mkv, iso)
    {
        tracing::warn!(error = %e, "could not record the library link for a delivered rip");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delivery_of_one_mkv_and_one_iso_is_recorded() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().to_str().unwrap();
        record_delivery(dir, ["/m/A (2000)/A (2000).mkv", "/i/A (2000).iso"]);
        record_delivery(dir, ["/m/B/B.mkv"]);
        record_delivery(dir, ["/m/S/e1.mkv", "/m/S/e2.mkv", "/i/S.iso"]);
        let links = load(t.path());
        assert_eq!(links.len(), 1);
        assert_eq!(
            links.get(Path::new("/m/A (2000)/A (2000).mkv")),
            Some(&PathBuf::from("/i/A (2000).iso"))
        );
    }
}
