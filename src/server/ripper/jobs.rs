//! Stable staging identity, independent of editable library metadata.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::staging;

#[cfg(test)]
pub(super) fn metadata(root: &Path, job: &str) -> Option<crate::server::tmdb::TmdbResult> {
    let st = staging::read_state(&path(root, job).ok()?)?;
    metadata_from_state(st)
}

pub(super) fn metadata_from_state(
    st: staging::DiscState,
) -> Option<crate::server::tmdb::TmdbResult> {
    let m = st.user_metadata.or_else(|| {
        (!st.title.is_empty()).then_some(staging::UserMetadata {
            title: st.title,
            year: st.year,
            media_type: st.media_type,
            episode_start: st.episode_start,
            tmdb_id: st.tmdb_id,
            poster_url: st.tmdb_poster,
            overview: st.tmdb_overview,
        })
    })?;
    Some(crate::server::tmdb::TmdbResult {
        title: m.title,
        year: m.year,
        media_type: m.media_type,
        tmdb_id: m.tmdb_id,
        poster_url: m.poster_url,
        overview: m.overview,
    })
}

/// AACS supplies the disc hash. Other media use a versioned, content-sampled
/// fingerprint including capacity; unreadable samples refuse automatic reuse.
pub(super) fn fingerprint(
    disc: &libfreemkv::Disc,
    reader: &mut dyn libfreemkv::SectorSource,
) -> Result<String, String> {
    if let Some(aacs) = &disc.aacs {
        let hash = libfreemkv::hex::strip_hex_prefix(&aacs.disc_hash).to_ascii_lowercase();
        if hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(hash);
        }
    }
    sample_fingerprint(disc.capacity_bytes, reader)
}

/// Allocate a fresh staging namespace when a non-AACS fingerprint cannot be
/// verified. This is deliberately not an identity: callers must not persist
/// the returned value as `disc_identity` or use it to adopt an existing job.
pub(super) fn fresh_unverified_job(root: &Path) -> Result<String, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create staging: {e}"))?;
    let stamp = crate::server::util::epoch_secs();
    for n in 0..1000u32 {
        let job = if n == 0 {
            format!("unverified-{stamp}")
        } else {
            format!("unverified-{stamp}-{n}")
        };
        match std::fs::create_dir(root.join(&job)) {
            Ok(()) => return Ok(job),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot reserve isolated staging job: {e}")),
        }
    }
    Err("cannot allocate an isolated staging job for the unverified disc".into())
}

fn sample_fingerprint(
    capacity: u64,
    reader: &mut dyn libfreemkv::SectorSource,
) -> Result<String, String> {
    let sectors = capacity / 2048;
    if sectors < 64 || sectors > u64::from(u32::MAX) {
        return Err("disc capacity is unavailable for identity verification".into());
    }
    let mut hash = Sha256::new();
    hash.update(b"freemkv-disc-samples-v1");
    hash.update(capacity.to_le_bytes());
    let mut buf = [0u8; 16 * 2048];
    for lba in [16, sectors / 4, sectors / 2, sectors * 3 / 4, sectors - 16] {
        let n = reader
            .read_sectors(lba as u32, 16, &mut buf, false)
            .map_err(|e| format!("cannot verify disc identity: {e}"))?;
        if n != buf.len() {
            return Err("cannot verify disc identity: short sector read".into());
        }
        hash.update(lba.to_le_bytes());
        hash.update(buf);
    }
    let hex: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("sample-v1-{hex}"))
}

/// Adopt one existing matching job in place. Multiple matches are a hold,
/// never permission to pick arbitrarily or delete another partial capture.
pub(super) fn locate(root: &Path, identity: &str) -> Result<String, String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(format!("disc-{identity}"));
        }
        Err(e) => return Err(format!("cannot inspect staging: {e}")),
    };
    let mut found = None;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot inspect staging entry: {e}"))?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let path = entry.path();
        let saved = staging::read_state(&path);
        let matches = if let Some(saved) = saved.filter(|s| !s.disc_identity.is_empty()) {
            saved.disc_identity == identity
        } else {
            super::resume::find_iso_and_mapfile(&path)
                .and_then(|(_, map)| freemkv_engine::Mapfile::load(&map).ok())
                .and_then(|map| map.disc_hash().map(str::to_owned))
                .is_some_and(|hash| hash.eq_ignore_ascii_case(identity))
        };
        if matches {
            if found.is_some() {
                return Err("more than one staging entry matches this disc; keep both and resolve the duplicate before continuing".into());
            }
            found = Some(entry.file_name().to_string_lossy().into_owned());
        }
    }
    if let Some(found) = found {
        return Ok(found);
    }
    let job = format!("disc-{identity}");
    if std::fs::symlink_metadata(root.join(&job)).is_ok() {
        return Err(
            "the staging job exists but its identity cannot be verified; preserve it for review"
                .into(),
        );
    }
    Ok(job)
}

pub(super) fn path(root: &Path, job: &str) -> Result<PathBuf, String> {
    if !super::is_safe_staging_segment(job) {
        return Err("invalid staging job identity".into());
    }
    let path = root.join(job);
    if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("staging job must not be a symbolic link".into());
    }
    Ok(path)
}

#[cfg(test)]
#[path = "jobs_tests.rs"]
mod tests;
