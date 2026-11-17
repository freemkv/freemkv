//! A resume muxes every episode of a saved TV image, and a mux it cannot key keeps the capture.
use super::*;

struct Job {
    _temp: tempfile::TempDir,
    dir: PathBuf,
    iso: PathBuf,
    cfg: Arc<RwLock<Config>>,
}

impl Job {
    // A captured two-title image saved with `episodes` planned episode outputs, its keydb
    // holding `key` for the disc (none: no keydb).
    fn new(episodes: usize, key: Option<[u8; 16]>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        // SAFETY: each caller holds env_guard for this fixture's lifetime.
        unsafe { std::env::set_var("AUTORIP_DIR", temp.path()) };
        let dir = temp.path().join("staging").join("Show");
        std::fs::create_dir_all(&dir).unwrap();
        let k1 = crate::ku_fixture::K1;
        let fx = crate::ku_fixtures::bd_image(&[Some(k1), Some(k1)], 1);
        let iso = fx.write(&dir, "Show.iso");
        crate::ku_fixtures::sidecar(&fx, &iso, true);
        let map = freemkv_engine::mapfile_path_for(&iso);
        let (scanned, _) =
            freemkv_engine::scan_image(&freemkv_engine::ImageSource::Iso(iso.clone())).unwrap();
        let keydb = temp.path().join("keydb.cfg");
        if let Some(key) = key {
            let hash = libfreemkv::hex::strip_hex_prefix(&fx.disc.aacs.as_ref().unwrap().disc_hash);
            let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
            std::fs::write(&keydb, format!("0x{hash} = ISO | U | 1-0x{hex}\n")).unwrap();
        }
        let mut saved = staging::DiscState::new(staging::StagingState::Ripped);
        saved.output_format = Some("mkv".into());
        saved.media_type = if episodes > 1 {
            MediaKind::Tv
        } else {
            MediaKind::Movie
        };
        saved.disc_name = "Show".into();
        saved.title = "Show".into();
        saved.iso_path = iso.to_string_lossy().into_owned();
        saved.mapfile_path = map.to_string_lossy().into_owned();
        saved.max_retries = 2;
        if episodes > 1 {
            saved.season = Some(1);
        }
        saved.outputs = (0..episodes)
            .map(|i| staging::Output {
                filename: if episodes > 1 {
                    format!("Show - S01E{:02}.mkv", i + 1)
                } else {
                    "Show.mkv".into()
                },
                title_index: i,
                title_identity: Some(crate::title_identity::TitleIdentity::of(&scanned.titles[i])),
                episode: (episodes > 1).then_some(i as u16 + 1),
                episode_name: format!("Episode {}", i + 1),
                moved: false,
            })
            .collect();
        staging::try_write_state(&dir, &saved).unwrap();
        let cfg = Arc::new(RwLock::new(Config {
            output_format: "mkv".into(),
            staging_dir: dir.parent().unwrap().to_string_lossy().into_owned(),
            keydb_path: Some(keydb.to_string_lossy().into_owned()),
            keep_iso: true,
            ..Default::default()
        }));
        Self {
            _temp: temp,
            dir,
            iso,
            cfg,
        }
    }

    fn run(&self) -> MuxHandoffOutcome {
        let marker = staging::read_state(&self.dir).unwrap().to_ripped_marker();
        super::super::test_trace::traced(|| remux_from_ripped_marker(&self.cfg, &self.dir, &marker))
            .0
    }

    fn muxed(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "mkv"))
            .collect();
        out.sort();
        out
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        super::super::STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove("_mux");
        crate::server::muxer::MUX_ERRORS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.dir.to_str().unwrap());
    }
}

fn guards() -> (
    crate::server::log::EnvGuard,
    std::sync::MutexGuard<'static, ()>,
) {
    let env = crate::server::log::env_guard();
    let state = crate::server::mover::TEST_STATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    (env, state)
}

#[test]
fn a_saved_tv_image_muxes_every_planned_episode() {
    let _g = guards();
    let job = Job::new(2, Some(crate::ku_fixture::K1));
    let outcome = job.run();
    assert!(outcome.success, "{:?}", outcome.failure_reason);
    let muxed = job.muxed();
    assert_eq!(muxed.len(), 2, "{muxed:?}");
    for mkv in &muxed {
        assert!(
            std::fs::metadata(mkv).unwrap().len() > 0,
            "{}",
            mkv.display()
        );
    }
    let saved = staging::read_state(&job.dir).unwrap();
    assert_eq!(saved.outputs.len(), 2);
    assert!(job.iso.exists(), "keep_iso retains the image");
}

#[test]
fn a_saved_movie_image_muxes_its_one_output() {
    let _g = guards();
    let job = Job::new(1, Some(crate::ku_fixture::K1));
    let outcome = job.run();
    assert!(outcome.success, "{:?}", outcome.failure_reason);
    assert_eq!(job.muxed().len(), 1);
}

#[test]
fn a_saved_image_with_no_key_keeps_the_capture_for_a_later_mux() {
    let _g = guards();
    for key in [None, Some(crate::ku_fixtures::K2)] {
        let job = Job::new(2, key);
        let before = std::fs::read(&job.iso).unwrap();
        let outcome = job.run();
        assert!(!outcome.success, "key {key:?}");
        assert_eq!(std::fs::read(&job.iso).unwrap(), before);
    }
}

#[test]
fn an_episode_that_cannot_be_written_is_dropped_and_the_rest_deliver() {
    let _g = guards();
    let job = Job::new(2, Some(crate::ku_fixture::K1));
    std::fs::create_dir(job.dir.join("Show - S01E02.mkv")).unwrap();
    let outcome = job.run();
    assert!(outcome.success, "{:?}", outcome.failure_reason);
    let saved = staging::read_state(&job.dir).unwrap();
    assert_eq!(saved.outputs.len(), 1, "{:?}", saved.outputs);
    assert_eq!(saved.outputs[0].filename, "Show - S01E01.mkv");
}

#[test]
fn a_primary_output_that_cannot_be_written_fails_and_keeps_the_capture() {
    let _g = guards();
    let job = Job::new(1, Some(crate::ku_fixture::K1));
    std::fs::create_dir(job.dir.join("Show.mkv")).unwrap();
    let before = std::fs::read(&job.iso).unwrap();
    let outcome = job.run();
    assert!(!outcome.success);
    assert!(outcome.failure_reason.is_some());
    assert_eq!(std::fs::read(&job.iso).unwrap(), before);
}

fn remux_cold(job: &Job, device: &str) {
    let map = freemkv_engine::mapfile_path_for(&job.iso);
    super::super::test_trace::traced(|| {
        resume_remux_within(
            &job.cfg,
            device,
            ResumeClass::Remux {
                iso_path: job.iso.clone(),
                mapfile_path: map,
                display_name: "Show".into(),
                title_confident: None,
            },
            std::time::Duration::ZERO,
            || (),
        )
    });
}

#[test]
fn a_cold_resume_holds_a_job_whose_state_cannot_be_read() {
    let _g = guards();
    let job = Job::new(1, Some(crate::ku_fixture::K1));
    let state = job.dir.join("state.json");
    std::fs::write(&state, b"{ not json").unwrap();
    let before = std::fs::read(&job.iso).unwrap();
    let device = "cold-resume-unreadable";
    remux_cold(&job, device);
    assert_eq!(std::fs::read(&state).unwrap(), b"{ not json");
    assert_eq!(std::fs::read(&job.iso).unwrap(), before);
    assert!(job.muxed().is_empty());
    super::super::STATE.lock().unwrap().remove(device);
}

#[test]
fn a_cold_resume_of_a_legacy_job_with_no_state_never_loses_the_image() {
    let _g = guards();
    let job = Job::new(1, Some(crate::ku_fixture::K1));
    std::fs::remove_file(job.dir.join("state.json")).unwrap();
    let device = "cold-resume-legacy";
    remux_cold(&job, device);
    assert!(job.iso.exists());
    assert!(freemkv_engine::mapfile_path_for(&job.iso).exists());
    super::super::STATE.lock().unwrap().remove(device);
}

#[test]
fn a_resume_stopped_before_its_mux_keeps_the_capture() {
    let _g = guards();
    let job = Job::new(2, Some(crate::ku_fixture::K1));
    let device = "resume-stopped-device";
    let halt = libfreemkv::Halt::new();
    halt.cancel();
    super::super::register_halt(device, halt);
    let before = std::fs::read(&job.iso).unwrap();
    remux_cold(&job, device);
    super::super::unregister_halt(device);
    assert!(job.muxed().is_empty());
    assert_eq!(std::fs::read(&job.iso).unwrap(), before);
    super::super::STATE.lock().unwrap().remove(device);
}

#[test]
fn a_resume_stopped_while_another_owner_holds_the_job_gives_up_quietly() {
    let _g = guards();
    let job = Job::new(1, Some(crate::ku_fixture::K1));
    let device = "resume-lease-stopped";
    let lease = staging::job_lease(&job.dir);
    let _owner = lease.lock().unwrap();
    let halt = libfreemkv::Halt::new();
    halt.cancel();
    super::super::register_halt(device, halt);
    super::super::test_trace::traced(|| {
        resume_remux_within(
            &job.cfg,
            device,
            ResumeClass::Remux {
                iso_path: job.iso.clone(),
                mapfile_path: freemkv_engine::mapfile_path_for(&job.iso),
                display_name: "Show".into(),
                title_confident: Some(true),
            },
            std::time::Duration::from_secs(5),
            || (),
        )
    });
    super::super::unregister_halt(device);
    let row = super::super::STATE.lock().unwrap().remove(device).unwrap();
    assert_eq!(row.status, "idle");
    assert!(job.muxed().is_empty());
}

#[test]
fn a_saved_plan_naming_an_output_outside_staging_is_refused() {
    let _g = guards();
    let job = Job::new(1, Some(crate::ku_fixture::K1));
    staging::mutate_state_if_present(&job.dir, |s| s.outputs[0].filename = "../Escape.mkv".into());
    let device = "resume-escaping-output";
    remux_cold(&job, device);
    let row = super::super::STATE.lock().unwrap().remove(device).unwrap();
    assert_eq!(row.status, "error");
    assert!(
        row.last_error.contains("outside its staging folder"),
        "{}",
        row.last_error
    );
    assert!(!job.dir.parent().unwrap().join("Escape.mkv").exists());
}

#[test]
fn an_accepted_loss_is_delivered_and_the_override_spent() {
    let _g = guards();
    let job = Job::new(1, Some(crate::ku_fixture::K1));
    staging::write_accept_loss_marker(&job.dir);
    assert!(staging::accept_loss_requested(&job.dir));
    let outcome = job.run();
    assert!(outcome.success, "{:?}", outcome.failure_reason);
    assert!(!staging::accept_loss_requested(&job.dir));
    assert_eq!(job.muxed().len(), 1);
}
