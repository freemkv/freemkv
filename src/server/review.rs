//! Needs-review queue: rips the ripper held back because the title match wasn't
//! confident (see `ripper::rip_disc`). A held rip is a staging dir that has a
//! `.review` marker but no `.done` — so the mover skips it (it only promotes
//! `.done` dirs). The operator resolves each one here: **proceed** as-named,
//! **retitle** (pick the correct movie), or **cancel**.
//!
//! Everything keys off marker files on disk, so held rips survive a restart and
//! never block the drive (the rip is already complete and staged).

use std::path::{Path, PathBuf};

/// One rip awaiting operator review.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HeldRip {
    /// Staging subdir name (the handle used to resolve it).
    pub dir: String,
    /// Title the ripper resolved (the uncertain guess).
    pub title: String,
    /// Year the ripper resolved (0 = none — a common reason it's held).
    pub year: u16,
    pub media_type: String,
    pub episode_start: Option<u16>,
    /// The ripped media file inside the dir (for display).
    pub file: String,
    /// Why it's held (human-readable).
    pub reason: String,
}

/// Display metadata from the unified `state.json`.
fn state_marker(st: &crate::server::ripper::staging::DiscState) -> serde_json::Value {
    serde_json::json!({
        "title": st.title,
        "year": st.year,
        "media_type": st.media_type,
        "episode_start": st.episode_start,
        "failure_reason": st.failure_reason,
        "disc_name": st.disc_name,
    })
}

/// Display metadata from the legacy `.review` JSON body.
fn legacy_marker(dir: &Path) -> serde_json::Value {
    std::fs::read_to_string(dir.join(".review"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// A legacy held rip: a `.review` marker with no `.done`.
fn legacy_held(dir: &Path) -> bool {
    dir.join(".review").exists() && !dir.join(".done").exists()
}

/// Display metadata for `dir` when it is a held-for-review rip: `state ==
/// Review` in the unified store, or a legacy `.review` marker with no `.done`.
fn held_marker(dir: &Path) -> Option<serde_json::Value> {
    if let Some(st) = crate::server::ripper::staging::read_state(dir) {
        return (st.state == crate::server::ripper::staging::StagingState::Review)
            .then(|| state_marker(&st));
    }
    legacy_held(dir).then(|| legacy_marker(dir))
}

fn media_file(dir: &Path) -> Option<String> {
    // read_dir order is platform-dependent, so when a dir holds more than
    // one media file pick deterministically (lexicographically smallest)
    // rather than returning an arbitrary one. Display-only.
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            matches!(ext, "mkv" | "m2ts")
                .then(|| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())
                })
                .flatten()
        })
        .collect();
    names.sort();
    names.into_iter().next()
}

/// List every held rip under `staging_root`: `state == Review` in the unified
/// `state.json` when present, else the legacy `.review` marker with no `.done`.
pub fn list_held(staging_root: &str) -> Vec<HeldRip> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(staging_root) else {
        return out;
    };
    for e in entries.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(m) = held_marker(&dir) else {
            continue;
        };
        let title = m["title"].as_str().unwrap_or("").to_string();
        // Range-validate rather than a truncating `as u16`: a corrupt/
        // hand-edited year > 65535 would otherwise WRAP (e.g. 70000 → 4464).
        // Out-of-range → 0 ("no confident year"), same as a missing field.
        let year = m["year"]
            .as_u64()
            .and_then(|y| u16::try_from(y).ok())
            .unwrap_or(0);
        out.push(HeldRip {
            dir: dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string(),
            title,
            year,
            media_type: m["media_type"].as_str().unwrap_or("movie").to_string(),
            episode_start: m["episode_start"]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok()),
            file: media_file(&dir).unwrap_or_default(),
            reason: if let Some(reason) = m["failure_reason"].as_str() {
                reason.to_string()
            } else if year == 0 {
                "no confident title/year match".into()
            } else {
                "uncertain title match".into()
            },
        });
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// Operator action on a held rip.
pub enum Resolve {
    Proceed,
    Retitle { title: String, year: u16 },
    Cancel,
}

/// Serialises [`resolve`] so two requests for the same held dir can't both pass
/// the held check and race their state writes.
static RESOLVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Resolve a held rip. `dir` is the staging subdir name (not a path — guarded
/// against traversal). When the unified `state.json` is present it is
/// mutated in place (`StagingState`); otherwise the legacy marker files are
/// used. Actions:
/// * `Proceed`            — promote to `Done` / `.review` → `.done` as-named.
/// * `Retitle{title,year}`— rewrite the title/year, then promote to `Done`.
/// * `Cancel`             — mark `Failed` / `.failed` (so it isn't retried),
///   then drop `.review` in the legacy case.
pub fn resolve(staging_root: &str, dir: &str, action: Resolve) -> Result<(), String> {
    // Path-traversal guard: a held-rip handle is a single staging subdir
    // name. Inspect path components rather than substring-matching `..`,
    // which would wrongly reject a title like `Blade..Runner (1982)`.
    if dir.is_empty()
        || Path::new(dir).components().count() != 1
        || Path::new(dir)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("invalid dir".into());
    }
    let d: PathBuf = Path::new(staging_root).join(dir);
    let review = d.join(".review");
    let _serialised = RESOLVE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    // Unified store when present; else operate on the legacy marker files.
    let unified = crate::server::ripper::staging::read_state(&d);
    let held = match &unified {
        Some(st) => st.state == crate::server::ripper::staging::StagingState::Review,
        None => legacy_held(&d),
    };
    if !d.is_dir() || !held {
        return Err("not a held rip".into());
    }
    match action {
        Resolve::Proceed => {
            // Promote the held rip to the mover-facing state, carrying the
            // existing metadata forward (a durable transition — no bare rename,
            // which wouldn't fsync the dirent).
            if let Some(mut st) = unified {
                st.state = crate::server::ripper::staging::StagingState::Done;
                st.title_confident = true;
                crate::server::ripper::staging::try_write_state(&d, &st)
                    .map_err(|e| e.to_string())?;
            } else {
                let body = std::fs::read(&review).map_err(|e| e.to_string())?;
                crate::server::ripper::staging::write_handoff_marker(&d.join(".done"), &body)
                    .map_err(|e| e.to_string())?;
                std::fs::remove_file(&review).map_err(|e| e.to_string())?;
            }
        }
        Resolve::Retitle { title, year } => {
            if title.trim().is_empty() {
                return Err("title required".into());
            }
            if let Some(mut st) = unified {
                st.title = title;
                st.year = year;
                // A non-movie (TV) media_type must survive a retitle; only
                // default to "movie" when the rip has no media_type at all.
                if st.media_type.is_empty() {
                    st.media_type = "movie".into();
                }
                st.state = crate::server::ripper::staging::StagingState::Done;
                st.title_confident = true;
                crate::server::ripper::staging::try_write_state(&d, &st)
                    .map_err(|e| e.to_string())?;
            } else {
                let mut m = legacy_marker(&d);
                if !m.is_object() {
                    m = serde_json::json!({});
                }
                m["title"] = serde_json::json!(title);
                m["year"] = serde_json::json!(year);
                if m.get("media_type").and_then(|v| v.as_str()).is_none() {
                    m["media_type"] = serde_json::json!("movie");
                }
                let serialized = serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?;
                crate::server::ripper::staging::write_handoff_marker(
                    &d.join(".done"),
                    serialized.as_bytes(),
                )
                .map_err(|e| e.to_string())?;
                std::fs::remove_file(&review).map_err(|e| e.to_string())?;
            }
        }
        Resolve::Cancel => {
            // Terminal `.failed` (so it isn't retried). The contract requires
            // propagating a write error and preserving held state on failure,
            // so use the fallible transition, not `write_failed_marker`.
            if let Some(mut st) = unified {
                st.state = crate::server::ripper::staging::StagingState::Failed;
                st.failure_reason = Some("cancelled by operator".to_string());
                st.muxing = false;
                crate::server::ripper::staging::try_write_state(&d, &st)
                    .map_err(|e| e.to_string())?;
            } else {
                let failed_body = serde_json::json!({
                    "reason": "cancelled by operator",
                    "timestamp": crate::server::util::format_iso_datetime(),
                });
                let failed_str =
                    serde_json::to_string_pretty(&failed_body).map_err(|e| e.to_string())?;
                crate::server::ripper::staging::write_handoff_marker(
                    &d.join(".failed"),
                    failed_str.as_bytes(),
                )
                .map_err(|e| e.to_string())?;
                std::fs::remove_file(&review).map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;
