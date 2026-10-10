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
    let reason = crate::server::ripper::staging::read_failed_reason(&held);
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

#[test]
fn media_file_finds_m2ts_and_none_for_no_media() {
    let tmp = std::env::temp_dir().join(format!(
        "autorip-review-media-m2ts-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    assert_eq!(media_file(&tmp), None, "empty dir has no media file");
    touch(&tmp.join("notes.txt"), "x");
    touch(&tmp.join("feature.m2ts"), "x");
    assert_eq!(media_file(&tmp).as_deref(), Some("feature.m2ts"));
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Build a `state.json` in `Review` state with a few multi-episode
/// outputs, so the unified branches in `resolve` actually run (rather
/// than the legacy `.review`-file path).
fn write_review_state(dir: &Path, media_type: &str) {
    use crate::server::ripper::staging::{DiscState, Output, StagingState};
    std::fs::create_dir_all(dir).unwrap();
    let mut st = DiscState::new(StagingState::Review);
    st.title = "Guess".into();
    st.media_type = media_type.into();
    st.season = Some(5);
    st.outputs = vec![
        Output {
            filename: "ep1.mkv".into(),
            title_index: 0,
            title_identity: None,
            episode: Some(1),
            episode_name: String::new(),
            moved: false,
        },
        Output {
            filename: "ep2.mkv".into(),
            title_index: 1,
            title_identity: None,
            episode: Some(2),
            episode_name: String::new(),
            moved: false,
        },
        Output {
            filename: "ep3.mkv".into(),
            title_index: 2,
            title_identity: None,
            episode: Some(3),
            episode_name: String::new(),
            moved: false,
        },
    ];
    crate::server::ripper::staging::write_state(dir, &st);
}

#[test]
fn unified_proceed_transitions_review_to_done() {
    use crate::server::ripper::staging::{StagingState, read_state};
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
    use crate::server::ripper::staging::{StagingState, read_state};
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
    assert!(after.title_confident, "a retitle is operator-confirmed");
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
    use crate::server::ripper::staging::read_state;
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
    use crate::server::ripper::staging::{StagingState, read_state};
    let tmp = std::env::temp_dir().join(format!(
        "autorip-review-unified-cancel-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let held = tmp.join("Show S05");
    write_review_state(&held, "tv");
    crate::server::ripper::staging::mutate_state_if_present(&held, |s| s.muxing = true);
    assert!(read_state(&held).is_some(), "state.json must exist");

    resolve(tmp.to_str().unwrap(), "Show S05", Resolve::Cancel).unwrap();

    let after = read_state(&held).expect("state.json must survive Cancel");
    assert_eq!(after.state, StagingState::Failed);
    assert!(!after.muxing, "cancel must clear the muxing lock");
    assert_eq!(
        after.failure_reason.as_deref(),
        Some("cancelled by operator")
    );

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn unified_retitle_rejects_blank_title() {
    use crate::server::ripper::staging::{StagingState, read_state};
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

fn scratch(tag: &str) -> PathBuf {
    let tmp = std::env::temp_dir().join(format!(
        "autorip-review-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    tmp
}

#[test]
fn list_held_out_of_range_year_reads_as_zero() {
    let tmp = scratch("year-range");
    let held = tmp.join("Big Year");
    std::fs::create_dir_all(&held).unwrap();
    touch(
        &held.join(".review"),
        r#"{"title":"Big Year","year":70000}"#,
    );

    let list = list_held(tmp.to_str().unwrap());
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].year, 0, "70000 must not wrap to 4464");
    assert_eq!(list[0].reason, "no confident title/year match");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn list_held_unified_lists_only_review_state() {
    use crate::server::ripper::staging::{StagingState, mutate_state_if_present};
    let tmp = scratch("unified-list");
    let held = tmp.join("Show S05");
    write_review_state(&held, "tv");
    mutate_state_if_present(&held, |s| s.year = 2012);
    // Unified non-Review states, each with a stale legacy `.review` that
    // the legacy scan alone would call held.
    for (name, state) in [
        ("Done Show", StagingState::Done),
        ("Failed Show", StagingState::Failed),
        ("Ripping Show", StagingState::Sweeping),
    ] {
        let d = tmp.join(name);
        write_review_state(&d, "tv");
        mutate_state_if_present(&d, |s| s.state = state);
        touch(&d.join(".review"), r#"{"title":"Stale","year":0}"#);
    }

    let list = list_held(tmp.to_str().unwrap());
    assert_eq!(list.len(), 1, "only the Review dir is held, got {list:?}");
    assert_eq!(list[0].dir, "Show S05");
    assert_eq!(list[0].title, "Guess", "title comes from state.json");
    assert_eq!(list[0].year, 2012, "year comes from state.json");
    assert!(resolve(tmp.to_str().unwrap(), "Done Show", Resolve::Proceed).is_err());

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Make the next `state.json` write in `dir` fail: its `.tmp` sibling is a dir.
fn block_state_write(dir: &Path) {
    std::fs::create_dir(dir.join("state.json.tmp")).unwrap();
}

#[test]
fn unified_write_failure_surfaces_and_keeps_review() {
    use crate::server::ripper::staging::{StagingState, read_state};
    let tmp = scratch("unified-write-fail");
    let held = tmp.join("Show S05");
    write_review_state(&held, "tv");
    block_state_write(&held);

    for action in [
        Resolve::Proceed,
        Resolve::Retitle {
            title: "Real Show".into(),
            year: 2012,
        },
        Resolve::Cancel,
    ] {
        assert!(
            resolve(tmp.to_str().unwrap(), "Show S05", action).is_err(),
            "a state.json write failure must be reported"
        );
        let after = read_state(&held).unwrap();
        assert_eq!(after.state, StagingState::Review, "rip must stay held");
        assert_eq!(after.title, "Guess");
    }

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn concurrent_resolves_on_one_held_dir_admit_only_one() {
    use crate::server::ripper::staging::{StagingState, read_state};
    let tmp = scratch("concurrent");
    let root = tmp.to_str().unwrap().to_string();
    for round in 0..20 {
        let name = format!("Held {round}");
        write_review_state(&tmp.join(&name), "movie");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn = |action: Resolve| {
            let (root, name, barrier) = (root.clone(), name.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                resolve(&root, &name, action).is_ok()
            })
        };
        let cancel = spawn(Resolve::Cancel);
        let proceed = spawn(Resolve::Proceed);
        let (cancel_ok, proceed_ok) = (cancel.join().unwrap(), proceed.join().unwrap());
        assert!(
            cancel_ok != proceed_ok,
            "exactly one resolve may win (round {round}): cancel={cancel_ok} proceed={proceed_ok}"
        );
        let want = if cancel_ok {
            StagingState::Failed
        } else {
            StagingState::Done
        };
        assert_eq!(read_state(&tmp.join(&name)).unwrap().state, want);
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn legacy_retitle_recovers_non_object_review_body() {
    let tmp = scratch("legacy-array");
    let held = tmp.join("Held");
    std::fs::create_dir_all(&held).unwrap();
    touch(&held.join(".review"), "[]");

    resolve(
        tmp.to_str().unwrap(),
        "Held",
        Resolve::Retitle {
            title: "Sample Movie".into(),
            year: 2024,
        },
    )
    .unwrap();
    let m: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(held.join(".done")).unwrap()).unwrap();
    assert_eq!(m["title"], "Sample Movie");
    assert_eq!(m["media_type"], "movie");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn legacy_proceed_unreadable_review_errs_without_done() {
    let tmp = scratch("legacy-unreadable");
    let held = tmp.join("Held");
    std::fs::create_dir_all(held.join(".review")).unwrap();

    assert!(resolve(tmp.to_str().unwrap(), "Held", Resolve::Proceed).is_err());
    assert!(
        !held.join(".done").exists(),
        "no .done without a marker body"
    );
    assert!(held.join(".review").exists());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn retitle_preserves_non_movie_media_type() {
    let tmp = std::env::temp_dir().join(format!("autorip-review-mediatype-{}", std::process::id()));
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
