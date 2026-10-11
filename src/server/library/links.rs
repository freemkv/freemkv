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
    read(config_dir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "library links unreadable; reading as empty");
        HashMap::new()
    })
}

pub(super) fn read(config_dir: &Path) -> std::io::Result<HashMap<PathBuf, PathBuf>> {
    match std::fs::read(config_dir.join(LINKS_FILE)) {
        Ok(b) => serde_json::from_slice(&b).map_err(std::io::Error::other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(e),
    }
}

/// Remember that `mkv` was muxed from `iso`.
pub fn record(config_dir: &Path, mkv: &Path, iso: &Path) -> std::io::Result<()> {
    record_many(config_dir, &[mkv], iso)
}

/// Forget that `mkv` was made from `iso`; a link to another ISO is left alone.
pub(super) fn forget(config_dir: &Path, mkv: &Path, iso: &Path) -> std::io::Result<()> {
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = read(config_dir)?;
    if links.get(mkv).map(PathBuf::as_path) != Some(iso) {
        return Ok(());
    }
    links.remove(mkv);
    let json = serde_json::to_vec_pretty(&links).map_err(std::io::Error::other)?;
    crate::server::ripper::staging::write_marker_durable(&config_dir.join(LINKS_FILE), &json)
}

fn record_many(config_dir: &Path, mkvs: &[&Path], iso: &Path) -> std::io::Result<()> {
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    // Links decide what a remux may overwrite or retire. An unreadable map
    // cannot safely be replaced with a new, incomplete one.
    let mut links = read(config_dir)?;
    for mkv in mkvs {
        links.insert(mkv.to_path_buf(), iso.to_path_buf());
    }
    let json = serde_json::to_vec_pretty(&links).map_err(std::io::Error::other)?;
    crate::server::ripper::staging::write_marker_durable(&config_dir.join(LINKS_FILE), &json)
}

/// Link every MKV in one completed delivery batch to its sole source ISO.
pub fn record_delivery<'a>(
    config_dir: &str,
    delivered: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<()> {
    super::wake();
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
    if let [iso] = &isos[..]
        && !mkvs.is_empty()
    {
        record_many(Path::new(config_dir), &mkvs, iso)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
