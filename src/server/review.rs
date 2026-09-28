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
    /// The ripped media file inside the dir (for display).
    pub file: String,
    /// Why it's held (human-readable).
    pub reason: String,
}

/// Display metadata for a held rip, from the unified `state.json` when present,
/// else the legacy `.review` JSON body.
fn read_marker(dir: &Path) -> serde_json::Value {
    if let Some(st) = crate::ripper::staging::read_state(dir) {
        return serde_json::json!({
            "title": st.title,
            "year": st.year,
            "media_type": st.media_type,
            "disc_name": st.disc_name,
        });
    }
    std::fs::read_to_string(dir.join(".review"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Is `dir` a held-for-review rip? `state == Review` in the unified store, or a
/// legacy `.review` marker with no `.done`.
fn is_held(dir: &Path) -> bool {
    if let Some(st) = crate::ripper::staging::read_state(dir) {
        return st.state == crate::ripper::staging::StagingState::Review;
    }
    dir.join(".review").exists() && !dir.join(".done").exists()
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
        if !dir.is_dir() || !is_held(&dir) {
            continue;
        }
        let m = read_marker(&dir);
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
            file: media_file(&dir).unwrap_or_default(),
            reason: if year == 0 {
                "no confident title/year match".into()
            } else {
                "uncertain title match".into()
            },
        });
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

/// Resolve a held rip. `dir` is the staging subdir name (not a path — guarded
/// against traversal). When the unified `state.json` is present it is
/// mutated in place (`StagingState`); otherwise the legacy marker files are
/// used. Actions:
/// * `Proceed`            — promote to `Done` / `.review` → `.done` as-named.
/// * `Retitle{title,year}`— rewrite the title/year, then promote to `Done`.
/// * `Cancel`             — mark `Failed` / `.failed` (so it isn't retried),
///   then drop `.review` in the legacy case.
pub enum Resolve {
    Proceed,
    Retitle { title: String, year: u16 },
    Cancel,
}

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
    if !d.is_dir() || !is_held(&d) {
        return Err("not a held rip".into());
    }
    // Unified store when present; else operate on the legacy marker files.
    let unified = crate::ripper::staging::read_state(&d).is_some();
    match action {
        Resolve::Proceed => {
            // Promote the held rip to the mover-facing state, carrying the
            // existing metadata forward (a durable transition — no bare rename,
            // which wouldn't fsync the dirent).
            if unified {
                let mut ok = false;
                crate::ripper::staging::mutate_state_if_present(&d, |s| {
                    s.state = crate::ripper::staging::StagingState::Done;
                    s.title_confident = true;
                    ok = true;
                });
                if !ok {
                    return Err("state.json vanished".into());
                }
            } else {
                let body = std::fs::read(&review).map_err(|e| e.to_string())?;
                crate::ripper::staging::write_handoff_marker(&d.join(".done"), &body)
                    .map_err(|e| e.to_string())?;
                std::fs::remove_file(&review).map_err(|e| e.to_string())?;
            }
        }
        Resolve::Retitle { title, year } => {
            if title.trim().is_empty() {
                return Err("title required".into());
            }
            if unified {
                let mut ok = false;
                crate::ripper::staging::mutate_state_if_present(&d, |s| {
                    s.title = title.clone();
                    s.year = year;
                    // A non-movie (TV) media_type must survive a retitle; only
                    // default to "movie" when the rip has no media_type at all.
                    if s.media_type.is_empty() {
                        s.media_type = "movie".into();
                    }
                    s.state = crate::ripper::staging::StagingState::Done;
                    s.title_confident = true;
                    ok = true;
                });
                if !ok {
                    return Err("state.json vanished".into());
                }
            } else {
                let mut m = read_marker(&d);
                if !m.is_object() {
                    m = serde_json::json!({});
                }
                m["title"] = serde_json::json!(title);
                m["year"] = serde_json::json!(year);
                if m.get("media_type").and_then(|v| v.as_str()).is_none() {
                    m["media_type"] = serde_json::json!("movie");
                }
                let serialized = serde_json::to_string_pretty(&m).map_err(|e| e.to_string())?;
                crate::ripper::staging::write_handoff_marker(
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
            if unified {
                let mut st = crate::ripper::staging::read_state(&d)
                    .ok_or_else(|| "state.json vanished".to_string())?;
                st.state = crate::ripper::staging::StagingState::Failed;
                st.failure_reason = Some("cancelled by operator".to_string());
                st.muxing = false;
                crate::ripper::staging::try_write_state(&d, &st).map_err(|e| e.to_string())?;
            } else {
                let failed_body = serde_json::json!({
                    "reason": "cancelled by operator",
                    "timestamp": crate::util::format_iso_datetime(),
                });
                let failed_str =
                    serde_json::to_string_pretty(&failed_body).map_err(|e| e.to_string())?;
                crate::ripper::staging::write_handoff_marker(
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
mod tests {
    use super::*;

    fn touch(p: &Path, body: &str) {
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn lists_only_held_and_resolves() {
        let tmp = std::env::temp_dir().join(format!("autorip-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // held: has .review, no .done
        let held = tmp.join("Some Movie");
        std::fs::create_dir_all(&held).unwrap();
        touch(&held.join(".review"), r#"{"title":"Some Movie","year":0}"#);
        touch(&held.join("Some Movie.mkv"), "x");
        // not held: has .done
        let done = tmp.join("Done Movie (2020)");
        std::fs::create_dir_all(&done).unwrap();
        touch(&done.join(".done"), "{}");

        let held_list = list_held(tmp.to_str().unwrap());
        assert_eq!(held_list.len(), 1);
        assert_eq!(held_list[0].dir, "Some Movie");
        assert_eq!(held_list[0].file, "Some Movie.mkv");
        assert_eq!(held_list[0].year, 0);

        // retitle → .done appears with the new title, .review gone
        resolve(
            tmp.to_str().unwrap(),
            "Some Movie",
            Resolve::Retitle {
                title: "Sample Movie".into(),
                year: 2024,
            },
        )
        .unwrap();
        assert!(held.join(".done").exists());
        assert!(!held.join(".review").exists());
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(held.join(".done")).unwrap()).unwrap();
        assert_eq!(m["title"], "Sample Movie");
        assert_eq!(m["year"], 2024);
        assert!(list_held(tmp.to_str().unwrap()).is_empty());

        // traversal guard
        assert!(resolve(tmp.to_str().unwrap(), "../etc", Resolve::Proceed).is_err());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn list_held_excludes_dir_with_neither_review_nor_done() {
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-nomark-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        // A dir mid-rip: has a `.ripped` progress marker, but neither
        // `.review` (not yet held for review) nor `.done` (not finished).
        let in_progress = tmp.join("Still Ripping");
        std::fs::create_dir_all(&in_progress).unwrap();
        touch(&in_progress.join(".ripped"), "{}");

        let held_list = list_held(tmp.to_str().unwrap());
        assert!(
            held_list.is_empty(),
            "a dir with no .review and no .done must never appear in the held list, got {held_list:?}"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn traversal_guard_rejects_escapes_accepts_dotted_titles() {
        // Component-based guard: reject anything that isn't a single
        // normal path component...
        for bad in ["..", ".", "../etc", "a/b", "/abs", "", "./x"] {
            assert!(
                resolve("/nonexistent-staging-root", bad, Resolve::Proceed).is_err(),
                "should reject {bad:?}"
            );
        }
        // ...but a legitimate title containing `..` is NOT a traversal and
        // must pass the guard (it fails later only because the dir/marker
        // doesn't exist — "not a held rip", not "invalid dir").
        let err = resolve(
            "/nonexistent-staging-root",
            "Blade..Runner (1982)",
            Resolve::Proceed,
        )
        .unwrap_err();
        assert_eq!(err, "not a held rip", "dotted title must clear the guard");
    }

    #[test]
    fn traversal_guard_rejects_escapes_against_a_real_existing_parent() {
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-traversal-real-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let staging_root = tmp.join("staging");
        std::fs::create_dir_all(&staging_root).unwrap();
        // A `.review` marker in the PARENT of staging_root, not inside it —
        // exactly what an escaped ".." would land on if the guard failed.
        touch(
            &tmp.join(".review"),
            r#"{"title":"Parent Escape","year":0}"#,
        );

        for bad in ["..", ".", "../etc", "a/b", "/abs", "./x"] {
            let err = resolve(staging_root.to_str().unwrap(), bad, Resolve::Proceed)
                .expect_err(&format!("should reject {bad:?}"));
            assert_eq!(
                err, "invalid dir",
                "{bad:?} must be rejected by the TRAVERSAL GUARD itself, not a \
                 downstream is_dir()/exists() check — got {err:?}"
            );
        }

        // No write ever escaped to the parent.
        assert!(
            tmp.join(".review").exists(),
            "parent .review must be untouched"
        );
        assert!(
            !tmp.join(".done").exists(),
            "must never have promoted the parent directory"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn proceed_carries_marker_body_into_durable_done() {
        // Regression (finding 7): Proceed writes a DURABLE `.done`
        // (write_handoff_marker: tmp+fsync+rename+dir-fsync) carrying the
        // `.review` JSON forward, not a bare non-fsyncing rename.
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-proceed-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Keeper (2019)");
        std::fs::create_dir_all(&held).unwrap();
        let body = r#"{"title":"Keeper","year":2019,"media_type":"movie"}"#;
        touch(&held.join(".review"), body);

        resolve(tmp.to_str().unwrap(), "Keeper (2019)", Resolve::Proceed).unwrap();

        assert!(held.join(".done").exists(), ".done must be written");
        assert!(!held.join(".review").exists(), ".review must be removed");
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(held.join(".done")).unwrap()).unwrap();
        assert_eq!(m["title"], "Keeper", "marker body carried into .done");
        assert_eq!(m["year"], 2019);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cancel_propagates_write_error_and_preserves_review() {
        // If `.failed` can't be written, Cancel must return Err and leave
        // `.review` intact (so the rip is still visibly held), rather than
        // reporting success after dropping the only marker.
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-cancel-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Held");
        std::fs::create_dir_all(&held).unwrap();
        touch(&held.join(".review"), r#"{"title":"Held","year":0}"#);

        // Make `.failed` un-writable by pre-creating it as a directory, so
        // std::fs::write fails (can't truncate/open a dir as a file).
        std::fs::create_dir(held.join(".failed")).unwrap();

        let res = resolve(tmp.to_str().unwrap(), "Held", Resolve::Cancel);
        assert!(res.is_err(), "cancel must surface the write failure");
        // `.review` must survive so the rip stays held, not orphaned.
        assert!(held.join(".review").exists(), ".review must be preserved");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cancel_success_writes_failed_and_drops_review() {
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-cancelok-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Held");
        std::fs::create_dir_all(&held).unwrap();
        touch(&held.join(".review"), r#"{"title":"Held","year":0}"#);

        resolve(tmp.to_str().unwrap(), "Held", Resolve::Cancel).unwrap();
        assert!(held.join(".failed").exists());
        assert!(!held.join(".review").exists());

        // M2: the `.failed` marker is valid JSON carrying a machine-readable
        // reason, so `read_failed_reason` recovers it (the legacy non-JSON
        // body parsed to None, defeating reason-keyed terminal checks).
        let reason = crate::ripper::staging::read_failed_reason(&held);
        assert_eq!(
            reason.as_deref(),
            Some("cancelled by operator"),
            "cancel must write a JSON .failed whose reason round-trips"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn media_file_is_deterministic_across_multiple() {
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-media-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        touch(&tmp.join("zeta.mkv"), "x");
        touch(&tmp.join("alpha.mkv"), "x");
        touch(&tmp.join("notes.txt"), "x");
        assert_eq!(media_file(&tmp).as_deref(), Some("alpha.mkv"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Build a `state.json` in `Review` state with a few multi-episode
    /// outputs, so the unified branches in `resolve` actually run (rather
    /// than the legacy `.review`-file path).
    fn write_review_state(dir: &Path, media_type: &str) {
        use crate::ripper::staging::{DiscState, Output, StagingState};
        std::fs::create_dir_all(dir).unwrap();
        let mut st = DiscState::new(StagingState::Review);
        st.title = "Guess".into();
        st.media_type = media_type.into();
        st.season = Some(5);
        st.outputs = vec![
            Output {
                filename: "ep1.mkv".into(),
                title_index: 0,
                episode: Some(1),
                episode_name: String::new(),
                moved: false,
            },
            Output {
                filename: "ep2.mkv".into(),
                title_index: 1,
                episode: Some(2),
                episode_name: String::new(),
                moved: false,
            },
            Output {
                filename: "ep3.mkv".into(),
                title_index: 2,
                episode: Some(3),
                episode_name: String::new(),
                moved: false,
            },
        ];
        crate::ripper::staging::write_state(dir, &st);
    }

    #[test]
    fn unified_proceed_transitions_review_to_done() {
        use crate::ripper::staging::{StagingState, read_state};
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-unified-proceed-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Show S05");
        write_review_state(&held, "tv");

        // Confirm we're really exercising the unified (state.json) branch.
        let before = read_state(&held).expect("state.json must exist before resolve");
        assert_eq!(before.state, StagingState::Review);
        assert_eq!(before.outputs.len(), 3);

        resolve(tmp.to_str().unwrap(), "Show S05", Resolve::Proceed).unwrap();

        let after = read_state(&held).expect("state.json must survive Proceed");
        assert_eq!(after.state, StagingState::Done);
        assert!(after.title_confident);
        assert_eq!(after.title, "Guess");
        assert_eq!(after.season, Some(5));
        assert_eq!(
            after.outputs.len(),
            3,
            "multi-episode outputs must be preserved across Proceed"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unified_retitle_sets_title_and_keeps_tv_media_type() {
        use crate::ripper::staging::{StagingState, read_state};
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-unified-retitle-tv-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Show S05");
        write_review_state(&held, "tv");
        assert!(read_state(&held).is_some(), "state.json must exist");

        resolve(
            tmp.to_str().unwrap(),
            "Show S05",
            Resolve::Retitle {
                title: "Real Show".into(),
                year: 2012,
            },
        )
        .unwrap();

        let after = read_state(&held).expect("state.json must survive Retitle");
        assert_eq!(after.state, StagingState::Done);
        assert_eq!(after.title, "Real Show");
        assert_eq!(after.year, 2012);
        assert_eq!(
            after.media_type, "tv",
            "a non-empty media_type must not be overwritten to movie"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unified_retitle_defaults_empty_media_type_to_movie() {
        use crate::ripper::staging::read_state;
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-unified-retitle-empty-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Some Movie");
        write_review_state(&held, "");
        assert!(read_state(&held).is_some(), "state.json must exist");

        resolve(
            tmp.to_str().unwrap(),
            "Some Movie",
            Resolve::Retitle {
                title: "Sample Movie".into(),
                year: 2024,
            },
        )
        .unwrap();

        let after = read_state(&held).unwrap();
        assert_eq!(after.media_type, "movie");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unified_cancel_transitions_review_to_failed() {
        use crate::ripper::staging::{StagingState, read_state};
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-unified-cancel-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Show S05");
        write_review_state(&held, "tv");
        assert!(read_state(&held).is_some(), "state.json must exist");

        resolve(tmp.to_str().unwrap(), "Show S05", Resolve::Cancel).unwrap();

        let after = read_state(&held).expect("state.json must survive Cancel");
        assert_eq!(after.state, StagingState::Failed);
        assert_eq!(
            after.failure_reason.as_deref(),
            Some("cancelled by operator")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unified_retitle_rejects_blank_title() {
        use crate::ripper::staging::{StagingState, read_state};
        let tmp = std::env::temp_dir().join(format!(
            "autorip-review-unified-retitle-blank-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Show S05");
        write_review_state(&held, "tv");
        assert!(read_state(&held).is_some(), "state.json must exist");

        let err = resolve(
            tmp.to_str().unwrap(),
            "Show S05",
            Resolve::Retitle {
                title: "  ".into(),
                year: 0,
            },
        )
        .unwrap_err();
        assert!(!err.is_empty());

        let after = read_state(&held).expect("state.json must still exist");
        assert_eq!(
            after.state,
            StagingState::Review,
            "a rejected retitle must not mutate the held state"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn retitle_preserves_non_movie_media_type() {
        let tmp =
            std::env::temp_dir().join(format!("autorip-review-mediatype-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let held = tmp.join("Some Show");
        std::fs::create_dir_all(&held).unwrap();
        // Marker already carries a non-movie media_type (e.g. a TV title).
        touch(
            &held.join(".review"),
            r#"{"title":"Some Show","year":0,"media_type":"tv"}"#,
        );
        touch(&held.join("Some Show.mkv"), "x");

        resolve(
            tmp.to_str().unwrap(),
            "Some Show",
            Resolve::Retitle {
                title: "Severance".into(),
                year: 2022,
            },
        )
        .unwrap();

        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(held.join(".done")).unwrap()).unwrap();
        assert_eq!(m["title"], "Severance");
        assert_eq!(m["year"], 2022);
        // The retitle must not clobber the existing non-movie marker.
        assert_eq!(m["media_type"], "tv");

        // And when media_type is absent, retitle defaults it to "movie".
        let held2 = tmp.join("Some Movie");
        std::fs::create_dir_all(&held2).unwrap();
        touch(&held2.join(".review"), r#"{"title":"Some Movie","year":0}"#);
        resolve(
            tmp.to_str().unwrap(),
            "Some Movie",
            Resolve::Retitle {
                title: "Sample Movie".into(),
                year: 2024,
            },
        )
        .unwrap();
        let m2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(held2.join(".done")).unwrap()).unwrap();
        assert_eq!(m2["media_type"], "movie");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
