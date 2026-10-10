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

pub(super) fn publish_replacement(
    config_dir: &Path,
    mkv: &Path,
    iso: &Path,
    require_link: bool,
    publish: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = read(config_dir)?;
    if links.get(mkv).is_some_and(|owner| owner != iso)
        || (require_link && links.get(mkv).map(PathBuf::as_path) != Some(iso))
    {
        return Err(std::io::Error::other(
            "replacement output ownership changed before publication",
        ));
    }
    publish()?;
    links.insert(mkv.to_path_buf(), iso.to_path_buf());
    let bytes = serde_json::to_vec_pretty(&links).map_err(std::io::Error::other)?;
    crate::server::ripper::staging::write_marker_durable(&config_dir.join(LINKS_FILE), &bytes)
}

#[cfg(test)]
pub(super) fn retire_owned(
    config_dir: &Path,
    mkv: &Path,
    iso: &Path,
    remove: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    retire_replacement(config_dir, mkv, iso, false, &[], remove)
}

#[cfg(test)]
pub(super) fn retire_confirmed(
    config_dir: &Path,
    mkv: &Path,
    iso: &Path,
    remove: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    retire_replacement(config_dir, mkv, iso, true, &[], remove)
}

pub(super) fn reconcile_replacement(
    config_dir: &Path,
    targets: &[PathBuf],
    iso: &Path,
) -> std::io::Result<()> {
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = read(config_dir)?;
    if targets
        .iter()
        .any(|target| links.get(target).is_some_and(|owner| owner != iso))
    {
        return Err(std::io::Error::other("replacement final ownership changed"));
    }
    for target in targets {
        links.insert(target.clone(), iso.to_owned());
    }
    let bytes = serde_json::to_vec_pretty(&links).map_err(std::io::Error::other)?;
    crate::server::ripper::staging::write_marker_durable(&config_dir.join(LINKS_FILE), &bytes)
}

pub(super) fn retire_replacement(
    config_dir: &Path,
    mkv: &Path,
    iso: &Path,
    confirmed: bool,
    targets: &[PathBuf],
    remove: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    let _guard = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = read(config_dir)?;
    if targets
        .iter()
        .any(|target| links.get(target).map(PathBuf::as_path) != Some(iso))
    {
        return Err(std::io::Error::other(
            "replacement final ownership changed before cleanup",
        ));
    }
    if links.get(mkv).is_some_and(|owner| owner != iso) {
        return Err(std::io::Error::other(
            "replacement output ownership changed",
        ));
    }
    if !confirmed && !links.contains_key(mkv) && mkv.try_exists()? {
        return Err(std::io::Error::other(
            "replacement output no longer has source provenance",
        ));
    }
    remove()?;
    links.remove(mkv);
    let bytes = serde_json::to_vec_pretty(&links).map_err(std::io::Error::other)?;
    crate::server::ripper::staging::write_marker_durable(&config_dir.join(LINKS_FILE), &bytes)
}

fn record_many(config_dir: &Path, mkvs: &[&Path], iso: &Path) -> std::io::Result<()> {
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    // Source provenance authorizes replacement cleanup. An unreadable map
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
