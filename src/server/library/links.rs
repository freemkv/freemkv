//! ISO-to-MKV links recorded when this app files a rip or a remux: the ISO each MKV was
//! made from, and the MKV and ISO as they were then, so later work can tell the file at a
//! linked path is still the one freemkv wrote, from the disc now at that ISO path.

use crate::server::util::Stamp;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The links file's name in the config folder.
pub const LINKS_FILE: &str = "library-links.json";

static WRITE: Mutex<()> = Mutex::new(());

/// What links.json records about one MKV.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Link {
    pub iso: PathBuf,
    /// The MKV as freemkv wrote it. `None` for a link written by 1.8.3, which proves
    /// nothing about the file at that path now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<Stamp>,
    /// The ISO the MKV was made from. Another disc later filed at the same path differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso_stamp: Option<Stamp>,
    /// One of several files the disc was split into (an episode), never its single title.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub episode: bool,
}

impl Link {
    fn legacy(iso: PathBuf) -> Self {
        Self {
            iso,
            stamp: None,
            iso_stamp: None,
            episode: false,
        }
    }
}

// A 1.8.3 entry is the ISO path alone; it is written back the same way.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum Wire {
    Iso(PathBuf),
    Full(Link),
}

impl From<Wire> for Link {
    fn from(w: Wire) -> Self {
        match w {
            Wire::Iso(iso) => Link::legacy(iso),
            Wire::Full(link) => link,
        }
    }
}

impl From<&Link> for Wire {
    fn from(l: &Link) -> Self {
        if l.stamp.is_none() && l.iso_stamp.is_none() && !l.episode {
            Wire::Iso(l.iso.clone())
        } else {
            Wire::Full(l.clone())
        }
    }
}

/// The ISO's size and modification time, following a symlinked ISO as the scan does.
pub(crate) fn iso_stamp(iso: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(iso).ok().filter(|m| m.is_file())?;
    Some(Stamp {
        size: meta.len(),
        modified: meta.modified().ok()?,
    })
}

/// MKV path to the ISO it was ripped from. Missing or unreadable reads as empty.
pub fn load(config_dir: &Path) -> HashMap<PathBuf, PathBuf> {
    by_iso(entries(config_dir))
}

pub(super) fn read(config_dir: &Path) -> std::io::Result<HashMap<PathBuf, PathBuf>> {
    read_entries(config_dir).map(by_iso)
}

fn by_iso(entries: HashMap<PathBuf, Link>) -> HashMap<PathBuf, PathBuf> {
    entries.into_iter().map(|(mkv, l)| (mkv, l.iso)).collect()
}

/// Every link in full. Missing or unreadable reads as empty.
pub(crate) fn entries(config_dir: &Path) -> HashMap<PathBuf, Link> {
    read_entries(config_dir).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "library links unreadable; reading as empty");
        HashMap::new()
    })
}

pub(crate) fn read_entries(config_dir: &Path) -> std::io::Result<HashMap<PathBuf, Link>> {
    match parse(config_dir) {
        Ok(links) => Ok(links),
        Err(Unreadable::Io(e)) => Err(e),
        Err(Unreadable::Damaged(e)) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{LINKS_FILE} is damaged: {e}"),
        )),
    }
}

enum Unreadable {
    Io(std::io::Error),
    Damaged(serde_json::Error),
}

fn parse(config_dir: &Path) -> Result<HashMap<PathBuf, Link>, Unreadable> {
    match std::fs::read(config_dir.join(LINKS_FILE)) {
        Ok(b) => serde_json::from_slice::<HashMap<PathBuf, Wire>>(&b)
            .map(|m| m.into_iter().map(|(k, w)| (k, w.into())).collect())
            .map_err(Unreadable::Damaged),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(Unreadable::Io(e)),
    }
}

// The links a writer starts from. A Library writer sets a damaged file aside under a name
// of its own and starts empty, as 1.8.3 did: an empty map only takes permission away. A
// delivery refuses it instead (`set_aside` false). A file that cannot be read stays put.
fn read_for_write(config_dir: &Path, set_aside: bool) -> std::io::Result<HashMap<PathBuf, Link>> {
    match parse(config_dir) {
        Ok(links) => Ok(links),
        Err(Unreadable::Io(e)) => Err(e),
        Err(Unreadable::Damaged(e)) if set_aside => {
            let aside = super::set_aside(&config_dir.join(LINKS_FILE))?;
            tracing::warn!(error = %e, aside = %aside.display(), "library links damaged; set aside, starting empty");
            Ok(HashMap::new())
        }
        Err(Unreadable::Damaged(e)) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{LINKS_FILE} is damaged: {e}"),
        )),
    }
}

fn write(config_dir: &Path, links: &HashMap<PathBuf, Link>) -> std::io::Result<()> {
    let wire: HashMap<&PathBuf, Wire> = links.iter().map(|(k, l)| (k, l.into())).collect();
    let json = serde_json::to_vec_pretty(&wire).map_err(std::io::Error::other)?;
    crate::server::ripper::staging::write_marker_durable(&config_dir.join(LINKS_FILE), &json)
}

/// Remember that `mkv` was muxed from `iso`, as both are on disk now.
#[cfg(test)]
pub fn record(config_dir: &Path, mkv: &Path, iso: &Path) -> std::io::Result<()> {
    let stamp = Stamp::read(mkv).ok();
    record_many(config_dir, &[(mkv, stamp)], iso, false, true)
}

/// Forget that `mkv` was made from `iso`; a link to another ISO is left alone.
pub(super) fn forget(config_dir: &Path, mkv: &Path, iso: &Path) -> std::io::Result<()> {
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = read_for_write(config_dir, true)?;
    if links.get(mkv).map(|l| l.iso.as_path()) != Some(iso) {
        return Ok(());
    }
    links.remove(mkv);
    write(config_dir, &links)
}

/// Drop the links in `gone` whose MKV was found missing, unless the link changed since.
pub(super) fn forget_gone(
    config_dir: &Path,
    gone: &HashMap<PathBuf, Link>,
) -> std::io::Result<usize> {
    if gone.is_empty() {
        return Ok(0);
    }
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = read_for_write(config_dir, true)?;
    let before = links.len();
    links.retain(|mkv, link| gone.get(mkv) != Some(link));
    let dropped = before - links.len();
    if dropped > 0 {
        write(config_dir, &links)?;
    }
    Ok(dropped)
}

fn record_many(
    config_dir: &Path,
    mkvs: &[(&Path, Option<Stamp>)],
    iso: &Path,
    episode: bool,
    set_aside: bool,
) -> std::io::Result<()> {
    record_many_from(
        config_dir,
        mkvs,
        iso,
        episode,
        set_aside,
        iso_stamp(iso),
        true,
    )
}

/// Record a frozen job's original provenance, including when delivering an old artifact
/// after its ISO disappeared or was replaced. Unknown provenance grants no ownership.
pub(crate) fn record_frozen_output(
    config_dir: &Path,
    mkv: &Path,
    iso: &Path,
    stamp: Option<Stamp>,
    episode: bool,
    source_stamp: Option<Stamp>,
) -> std::io::Result<()> {
    let stamp = stamp.filter(|_| source_stamp.is_some());
    record_many_from(
        config_dir,
        &[(mkv, stamp)],
        iso,
        episode,
        true,
        source_stamp,
        false,
    )
}

fn record_many_from(
    config_dir: &Path,
    mkvs: &[(&Path, Option<Stamp>)],
    iso: &Path,
    episode: bool,
    set_aside: bool,
    iso_now: Option<Stamp>,
    prune_earlier_sources: bool,
) -> std::io::Result<()> {
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    let mut links = read_for_write(config_dir, set_aside)?;
    let before = links.clone();
    // Links from an earlier disc filed at this ISO path name files that disc made.
    if prune_earlier_sources && let Some(now) = iso_now {
        links.retain(|_, l| l.iso != iso || l.iso_stamp.is_none_or(|s| s == now));
    }
    for (mkv, stamp) in mkvs {
        links.insert(
            mkv.to_path_buf(),
            Link {
                iso: iso.to_path_buf(),
                stamp: *stamp,
                iso_stamp: iso_now,
                episode,
            },
        );
    }
    // Already recorded: no durable rewrite.
    if links == before {
        return Ok(());
    }
    write(config_dir, &links)
}

/// Link every MKV in one completed delivery batch to its sole source ISO, each stamped as
/// it is on disk now; [`record_delivery_stamped`] takes the stamps the delivery verified.
#[cfg(test)]
pub fn record_delivery<'a>(
    config_dir: &str,
    delivered: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<()> {
    record_delivery_stamped(
        config_dir,
        delivered.into_iter().map(|d| {
            let p = Path::new(d);
            (d, Stamp::read(p).ok())
        }),
    )
}

/// [`record_delivery`] with each file's stamp as the delivery verified it (`None` for an
/// ISO, or a file whose stamp is unknown). The batch carries no plan, so its MKVs are linked
/// as episodes only when it holds more than one; a lone MKV is linked as a single title.
pub fn record_delivery_stamped<'a>(
    config_dir: &str,
    delivered: impl IntoIterator<Item = (&'a str, Option<Stamp>)>,
) -> std::io::Result<()> {
    super::wake();
    let (mut mkvs, mut isos) = (Vec::new(), Vec::new());
    for (d, stamp) in delivered {
        let p = Path::new(d);
        match p
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
        {
            Some(e) if e == "mkv" => mkvs.push((p, stamp)),
            Some(e) if e == "iso" => isos.push(p),
            _ => {}
        }
    }
    if let [iso] = &isos[..]
        && !mkvs.is_empty()
    {
        // A delivery holds its staging until its links are saved, so a damaged map is
        // reported for repair rather than set aside.
        record_many(Path::new(config_dir), &mkvs, iso, mkvs.len() > 1, false)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "links_tests.rs"]
mod tests;
