//! Capture/resume planning and delivery seams with real durable state, without media I/O.
use super::*;

fn cfg(format: &str) -> Config {
    Config {
        output_format: format.into(),
        max_retries: 2,
        ..Default::default()
    }
}

// JSON also exercises the backwards-compatible state format: the pre-fix reader
// silently ignores this field, reproducing a captured job resumed with new settings.
fn tagged(mut st: staging::DiscState, format: &str) -> staging::DiscState {
    let mut json = serde_json::to_value(&st).unwrap();
    json["output_format"] = format.into();
    st = serde_json::from_value(json).unwrap();
    st
}

fn episode_titles() -> Vec<libfreemkv::DiscTitle> {
    let mut titles = crate::selection_test_fixtures::separate_titles();
    for (ordinal, title) in titles.iter_mut().enumerate() {
        title.duration_secs = 2640.0;
        title.selection_evidence.episodes = libfreemkv::disc::EpisodeEvidence::Authored {
            roster: "format-test-menu".into(),
            title_count: 2,
            ordinal: Some(ordinal),
        };
    }
    titles
}

#[test]
fn captured_movie_and_every_episode_keep_container_after_both_setting_switches() {
    for (saved_format, current, ext) in [
        ("mkv", "m2ts", "mkv"),
        ("m2ts", "mkv", "m2ts"),
        ("mkv", "m2ts", "mk3d"),
    ] {
        for kind in [MediaKind::Movie, MediaKind::Tv] {
            let tmp = tempfile::tempdir().unwrap();
            let titles = episode_titles();
            let label = if kind == MediaKind::Tv {
                "SHOW_S1_D1"
            } else {
                "MOVIE"
            };
            let original = super::super::capture_plan(
                &cfg(saved_format),
                &titles,
                (kind, label, 0),
                &format!("Film.{ext}"),
                Some(4),
            )
            .unwrap()
            .outputs
            .unwrap();
            let mut st = tagged(
                staging::DiscState::new(staging::StagingState::Ripped),
                saved_format,
            );
            st.outputs = original.clone();
            st.media_type = kind;
            staging::try_write_state(tmp.path(), &st).unwrap();
            let global = cfg(current);
            let local =
                resume_config(global.clone(), staging::read_state(tmp.path()).as_ref()).unwrap();
            let rebound = rebind_resume_outputs(&titles, &original).unwrap();
            assert_eq!(
                rebound, original,
                "source identity and episode metadata remain fixed"
            );
            assert_eq!(
                resume_primary_output(&rebound, "unused".into()),
                original[0]
            );
            assert_eq!(local.output_format, saved_format);
            assert_eq!(
                global.output_format, current,
                "only the resume's clone changes"
            );
            for output in &rebound {
                let path = tmp.path().join(&output.filename);
                let url = resume_destination(&local, path.to_str().unwrap());
                assert_eq!(url, format!("{saved_format}://{}", path.display()));
                assert!(output.filename.ends_with(&format!(".{ext}")));
                std::fs::write(path, b"muxed").unwrap();
            }
            staging::mark_handoff(tmp.path(), true, |s| s.outputs = rebound.clone()).unwrap();
            assert_eq!(staging::read_state(tmp.path()).unwrap().outputs, original);
            assert_eq!(
                crate::server::muxer::mux_dispatch_verdict(
                    staging::snapshot_staging_disc(tmp.path()).as_ref()
                ),
                crate::server::muxer::MuxVerdict::SkipTerminal
            );
        }
    }
}

#[test]
fn stopped_episode_partials_and_moved_flags_survive_two_reload_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let titles = episode_titles();
    let outputs = super::super::plan_mux_outputs(
        &titles,
        &cfg("m2ts"),
        MediaKind::Tv,
        "SHOW_S1_D1",
        0,
        "Show.m2ts",
        Some(4),
    )
    .unwrap();
    let mut st = tagged(
        staging::DiscState::new(staging::StagingState::Stopped),
        "m2ts",
    );
    st.outputs = outputs.clone();
    std::fs::write(tmp.path().join(&outputs[0].filename), b"complete-primary").unwrap();
    std::fs::write(tmp.path().join(&outputs[1].filename), b"partial-later").unwrap();
    staging::try_write_state(tmp.path(), &st).unwrap();
    for _ in 0..2 {
        let saved = staging::read_state(tmp.path()).unwrap();
        let local = resume_config(cfg("mkv"), Some(&saved)).unwrap();
        assert_eq!(local.output_format, "m2ts");
        assert_eq!(
            rebind_resume_outputs(&titles, &saved.outputs).unwrap(),
            outputs
        );
        assert_eq!(
            std::fs::read(tmp.path().join(&outputs[0].filename)).unwrap(),
            b"complete-primary"
        );
        assert_eq!(
            std::fs::read(tmp.path().join(&outputs[1].filename)).unwrap(),
            b"partial-later"
        );
        staging::mutate_state(tmp.path(), staging::StagingState::Stopped, |s| {
            s.restart_count += 1
        })
        .unwrap();
    }
    staging::mark_handoff(tmp.path(), true, |s| {
        s.outputs[0].moved = true;
    })
    .unwrap();
    let done = staging::read_state(tmp.path()).unwrap();
    assert!(done.outputs[0].moved);
    assert_eq!(done.outputs[0].filename, outputs[0].filename);
    assert_eq!(
        crate::server::muxer::mux_dispatch_verdict(
            staging::snapshot_staging_disc(tmp.path()).as_ref()
        ),
        crate::server::muxer::MuxVerdict::SkipTerminal
    );
    assert!(check_resume_state(&done).is_ok());
}

#[test]
fn metadata_replan_after_settings_switch_keeps_frozen_extension_and_revision() {
    let tmp = tempfile::tempdir().unwrap();
    let titles = episode_titles();
    let mut st = tagged(
        staging::DiscState::new(staging::StagingState::Ripped),
        "m2ts",
    );
    st.outputs = super::super::plan_mux_outputs(
        &titles,
        &cfg("m2ts"),
        MediaKind::Movie,
        "SHOW",
        0,
        "Show.m2ts",
        None,
    )
    .unwrap();
    st.metadata_revision = 7;
    st.iso_path = "Show.iso".into();
    for file in ["Show.iso", "Show.iso.mapfile"] {
        std::fs::write(tmp.path().join(file), b"source").unwrap();
    }
    staging::try_write_state(tmp.path(), &st).unwrap();
    staging::save_user_metadata(
        tmp.path(),
        staging::UserMetadata {
            title: "Corrected".into(),
            year: 2024,
            media_type: MediaKind::Tv,
            episode_start: Some(6),
            tmdb_id: 0,
            poster_url: String::new(),
            overview: String::new(),
        },
    )
    .unwrap();
    let saved = staging::read_state(tmp.path()).unwrap();
    assert!(saved.replan_required);
    assert_eq!(saved.metadata_revision, 8);
    let local = resume_config(cfg("mkv"), Some(&saved)).unwrap();
    let disc = crate::ku_fixture::bd_image().disc;
    let filename = super::super::staged_output_name(
        &saved.title,
        super::super::output_extension_for(&local.output_format, &disc),
    );
    let outputs = super::super::plan_mux_outputs(
        &titles,
        &local,
        MediaKind::Tv,
        "SHOW_S1_D1",
        0,
        &filename,
        Some(6),
    )
    .unwrap();
    assert_eq!(outputs.len(), 2);
    assert!(outputs.iter().all(|o| o.filename.ends_with(".m2ts")));
    assert_eq!(
        outputs.iter().map(|o| o.episode).collect::<Vec<_>>(),
        [Some(6), Some(7)]
    );
    assert!(staging::save_replanned_outputs(tmp.path(), 7, outputs.clone()).is_err());
    staging::save_replanned_outputs(tmp.path(), 8, outputs.clone()).unwrap();
    let final_state = staging::read_state(tmp.path()).unwrap();
    assert_eq!(final_state.outputs, outputs);
    assert!(!final_state.replan_required);
    assert_eq!(
        resume_config(cfg("iso"), Some(&final_state))
            .unwrap()
            .output_format,
        "m2ts"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("Show.iso")).unwrap(),
        b"source"
    );
}

#[test]
fn legacy_suffix_inference_network_ambiguity_and_empty_compatibility_are_explicit() {
    for (names, current, expected) in [
        (vec!["film.mkv", "episode.mk3d"], "m2ts", "mkv"),
        (vec!["film.m2ts"], "mkv", "m2ts"),
        (vec!["film.iso"], "mkv", "iso"),
        (vec!["film.mkv"], "network", "network"),
        (vec![], "iso", "iso"),
        (vec![], "m2ts", "m2ts"),
    ] {
        let mut st = staging::DiscState::new(staging::StagingState::Ripped);
        st.outputs = names
            .into_iter()
            .map(|name| staging::Output {
                filename: name.into(),
                ..Default::default()
            })
            .collect();
        assert_eq!(
            resume_config(cfg(current), Some(&st))
                .unwrap()
                .output_format,
            expected
        );
    }
    for names in [["film.mkv", "episode.m2ts"], ["film.mkv", "episode.bin"]] {
        let mut st = staging::DiscState::new(staging::StagingState::Ripped);
        st.outputs = names
            .into_iter()
            .map(|name| staging::Output {
                filename: name.into(),
                ..Default::default()
            })
            .collect();
        assert!(
            resume_config(cfg("mkv"), Some(&st)).is_err(),
            "ambiguous legacy outputs need review"
        );
    }
}

#[test]
fn frozen_iso_and_network_modes_override_settings_even_without_a_title_plan() {
    for format in ["iso", "network"] {
        let st = tagged(
            staging::DiscState::new(staging::StagingState::Ripped),
            format,
        );
        let mut global = cfg("mkv");
        global.network_target = "current-operator-target".into();
        let local = resume_config(global, Some(&st)).unwrap();
        assert_eq!(local.output_format, format);
        if format == "network" {
            assert!(staging::is_network_output(
                &local.output_format,
                &local.network_target
            ));
            assert_eq!(
                resume_destination(&local, "/unused.mkv"),
                "network://current-operator-target"
            );
            let no_target = resume_config(cfg("iso"), Some(&st)).unwrap();
            assert!(!staging::is_network_output(
                &no_target.output_format,
                &no_target.network_target
            ));
            assert_eq!(
                resume_destination(&no_target, "/local.mkv"),
                "mkv:///local.mkv"
            );
        }
    }
}

#[test]
fn mixed_legacy_plan_is_held_by_real_resume_before_partial_deletion() {
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let iso = tmp.path().join("Show.iso");
    let map = tmp.path().join("Show.iso.mapfile");
    let image = crate::ku_fixtures::bd_image(&[Some(crate::ku_fixture::K1)], 1)
        .img
        .image;
    std::fs::write(&iso, image).unwrap();
    std::fs::write(&map, b"source-map").unwrap();
    let mut st = staging::DiscState::new(staging::StagingState::Ripped);
    st.outputs = ["Show.mkv", "Later.m2ts"]
        .into_iter()
        .map(|filename| staging::Output {
            filename: filename.into(),
            ..Default::default()
        })
        .collect();
    for output in &st.outputs {
        std::fs::write(tmp.path().join(&output.filename), b"retained-partial").unwrap();
    }
    staging::try_write_state(tmp.path(), &st).unwrap();
    let device = "format-mixed-held";
    resume_remux_within(
        &Arc::new(RwLock::new(cfg("mkv"))),
        device,
        ResumeClass::Remux {
            iso_path: iso.clone(),
            mapfile_path: map.clone(),
            display_name: "Show".into(),
            title_confident: None,
        },
        std::time::Duration::ZERO,
        || (),
    );
    let held = staging::read_state(tmp.path()).unwrap();
    assert_eq!(held.state, staging::StagingState::Review);
    assert!(
        held.failure_reason
            .as_deref()
            .unwrap_or("")
            .contains("output format"),
        "{:?}",
        held.failure_reason
    );
    for output in &st.outputs {
        assert_eq!(
            std::fs::read(tmp.path().join(&output.filename)).unwrap(),
            b"retained-partial"
        );
    }
    assert!(iso.exists() && map.exists());
    crate::server::ripper::STATE.lock().unwrap().remove(device);
    crate::server::muxer::MUX_ERRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(tmp.path().to_str().unwrap());
}

#[test]
fn saved_format_survives_metadata_marker_and_stale_lifecycle_writes() {
    for format in ["mkv", "m2ts", "iso", "network"] {
        let tmp = tempfile::tempdir().unwrap();
        let initial = staging::DiscState::new(staging::StagingState::Sweeping);
        let st = tagged(initial.clone(), format);
        staging::try_write_state(tmp.path(), &st).unwrap();
        staging::mutate_state(tmp.path(), staging::StagingState::Ripped, |s| {
            s.title = "Corrected title".into()
        })
        .unwrap();
        let marker = staging::read_state(tmp.path()).unwrap().to_ripped_marker();
        crate::server::muxer::write_marker(tmp.path(), &marker).unwrap();
        // A worker whose snapshot predates capture cannot erase its chosen mode.
        staging::try_write_state(tmp.path(), &initial).unwrap();
        staging::mark_handoff(tmp.path(), true, |_| {}).unwrap();
        staging::write_completed_marker(tmp.path());
        let saved = staging::read_state(tmp.path()).unwrap();
        assert_eq!(
            serde_json::to_value(saved).unwrap()["output_format"],
            format
        );
    }
}

#[test]
fn real_resume_prepares_frozen_movie_and_episode_destinations_before_media_io() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let _g = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (saved_format, current, ext) in [
        ("mkv", "m2ts", "mkv"),
        ("m2ts", "mkv", "m2ts"),
        ("mkv", "m2ts", "mk3d"),
        ("mkv", "iso", "mkv"),
        ("m2ts", "network", "m2ts"),
        ("network", "m2ts", "mkv"),
        ("iso", "mkv", "iso"),
    ] {
        for fanout in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let iso = tmp.path().join("Saved.iso");
            let map = tmp.path().join("Saved.iso.mapfile");
            let k = crate::ku_fixture::K1;
            let image = crate::ku_fixtures::bd_image(&[Some(k), Some(k)], 1)
                .img
                .image;
            std::fs::write(&iso, image).unwrap();
            std::fs::write(&map, b"map-retained").unwrap();
            let (scanned, _) =
                freemkv_engine::scan_image(&freemkv_engine::ImageSource::Iso(iso.clone())).unwrap();
            assert!(scanned.titles.len() >= 2);
            let mut saved = tagged(
                staging::DiscState::new(staging::StagingState::Stopped),
                saved_format,
            );
            saved.media_type = if fanout {
                MediaKind::Tv
            } else {
                MediaKind::Movie
            };
            saved.outputs = if saved_format == "iso" {
                Vec::new()
            } else {
                (0..if fanout { 2 } else { 1 })
                    .map(|i| staging::Output {
                        filename: format!("Saved-E{}.{ext}", i + 4),
                        title_index: i,
                        title_identity: Some(crate::title_identity::TitleIdentity::of(
                            &scanned.titles[i],
                        )),
                        episode: fanout.then_some(i as u16 + 4),
                        episode_name: format!("Episode {i}"),
                        moved: false,
                    })
                    .collect()
            };
            for output in &saved.outputs {
                std::fs::write(tmp.path().join(&output.filename), b"retained-output").unwrap();
            }
            staging::try_write_state(tmp.path(), &saved).unwrap();
            let mut settings = cfg(current);
            settings.network_target = "operator-current-target".into();
            let settings = Arc::new(RwLock::new(settings));
            let prepared = Rc::new(RefCell::new(None));
            let observed = Rc::clone(&prepared);
            PREPARED_RESUME_OBSERVER.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move |local, disc, outputs| {
                    let urls: Vec<_> = outputs
                        .iter()
                        .map(|o| resume_destination(local, &o.filename))
                        .collect();
                    let scope = if super::super::output_is_iso_image(&local.output_format) {
                        vec![0]
                    } else {
                        resume_titles(disc, outputs).unwrap()
                    };
                    *observed.borrow_mut() = Some((
                        local.output_format.clone(),
                        outputs.to_vec(),
                        urls,
                        scope,
                        staging::is_network_output(&local.output_format, &local.network_target),
                    ));
                }))
            });
            let device = "format-prepared-integration";
            resume_remux_within(
                &settings,
                device,
                ResumeClass::Remux {
                    iso_path: iso.clone(),
                    mapfile_path: map.clone(),
                    display_name: "Saved".into(),
                    title_confident: None,
                },
                std::time::Duration::ZERO,
                || (),
            );
            PREPARED_RESUME_OBSERVER.with(|slot| slot.borrow_mut().take());
            let row = crate::server::ripper::STATE.lock().unwrap().remove(device);
            let (format, outputs, urls, scope, network) = prepared
                .borrow_mut()
                .take()
                .unwrap_or_else(|| panic!("resume did not reach media boundary: {row:?}"));
            assert_eq!(format, saved_format);
            let expected_outputs = if saved_format == "iso" {
                vec![staging::Output {
                    filename: "Saved.iso".into(),
                    ..Default::default()
                }]
            } else {
                saved.outputs.clone()
            };
            assert_eq!(outputs, expected_outputs);
            assert_eq!(network, saved_format == "network");
            assert_eq!(settings.read().unwrap().output_format, current);
            assert_eq!(
                scope.len(),
                if fanout && saved_format != "iso" {
                    2
                } else {
                    1
                }
            );
            for (output, url) in outputs.iter().zip(urls) {
                let expected = if network {
                    "network://operator-current-target".into()
                } else {
                    format!(
                        "{}://{}",
                        if saved_format == "m2ts" {
                            "m2ts"
                        } else {
                            "mkv"
                        },
                        output.filename
                    )
                };
                assert_eq!(url, expected);
                if saved_format == "iso" {
                    assert_eq!(tmp.path().join(&output.filename), iso);
                    assert!(iso.is_file());
                } else {
                    assert_eq!(
                        std::fs::read(tmp.path().join(&output.filename)).unwrap(),
                        b"retained-output"
                    );
                }
            }
            assert!(iso.exists() && map.exists());
            assert_eq!(
                staging::read_state(tmp.path()).unwrap().outputs,
                saved.outputs
            );
        }
    }
}
