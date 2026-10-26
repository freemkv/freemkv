use super::*;
use libfreemkv::disc::{ContentFormat, EpisodeEvidence, Extent};

fn title(dur_secs: f64, start_lba: u32) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        selection_evidence: Default::default(),
        playlist: String::new(),
        playlist_id: 0,
        duration_secs: dur_secs,
        size_bytes: (dur_secs as u64) * 1_000_000,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![Extent {
            start_lba,
            sector_count: 1000,
        }],
        content_format: ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

// Three episodes on an authored roster, plus a short extra outside it.
fn episode_disc() -> Vec<libfreemkv::DiscTitle> {
    let mut titles = vec![
        title(3060.0, 100),
        title(3090.0, 2000),
        title(3100.0, 4000),
        title(60.0, 6000),
    ];
    let title_count = titles.len();
    for (index, t) in titles.iter_mut().enumerate() {
        t.playlist_id = index as u16;
        t.selection_evidence.episodes = EpisodeEvidence::Authored {
            roster: "test-menu".into(),
            title_count,
            ordinal: (index < 3).then_some(index),
        };
    }
    titles
}

fn tv_plan(
    titles: &[libfreemkv::DiscTitle],
    start: Option<u16>,
) -> Vec<super::super::staging::Output> {
    super::super::plan_mux_outputs(
        titles,
        &Config::default(),
        MediaKind::Tv,
        "Show Season 2 Disc 2",
        0,
        "Show.mkv",
        start,
    )
    .unwrap()
}

#[test]
fn an_episode_plan_lists_each_episode_with_its_length() {
    let titles = episode_disc();
    let rows = card_episodes(&titles, "Show Season 2 Disc 2", &tv_plan(&titles, Some(4)));
    let shown: Vec<_> = rows
        .iter()
        .map(|r| (r.season, r.episode, r.duration.as_str()))
        .collect();
    assert_eq!(
        shown,
        [(2, 4, "0h 51m"), (2, 5, "0h 51m"), (2, 6, "0h 51m")]
    );
}

#[test]
fn a_single_feature_lists_no_episodes() {
    let titles = episode_disc();
    let mut plan = tv_plan(&titles, None);
    plan.truncate(1);
    assert!(card_episodes(&titles, "Show", &plan).is_empty());
}

#[test]
fn an_output_without_an_episode_number_is_no_episode_plan() {
    let titles = episode_disc();
    let mut plan = tv_plan(&titles, None);
    plan[1].episode = None;
    assert!(card_episodes(&titles, "Show", &plan).is_empty());
}

// title(3060 s, LBA 100): 3060 MB over 1000 sectors at 1 MB/s of playback.
#[test]
fn at_risk_time_is_the_damage_inside_the_title_at_its_rate() {
    let t = title(3060.0, 100);
    let sector = 2048u64;
    let inside = (150 * sector, 10 * sector);
    let outside = (5000 * sector, 10 * sector);
    let straddle = (1095 * sector, 10 * sector);
    let ms = |damage: &[(u64, u64)]| at_risk_ms(&t, damage);
    assert_eq!(ms(&[outside]), 0.0);
    assert!((ms(&[inside]) - 10.0 * 2048.0 / 1_000_000.0 * 1000.0).abs() < 1e-9);
    assert!((ms(&[straddle]) - 5.0 * 2048.0 / 1_000_000.0 * 1000.0).abs() < 1e-9);
}

// Tasks are admitted into a deterministic queue. Leaving a task queued models
// an indefinitely blocked syscall without sleeping or touching a network share.
type WorkTask = Box<dyn FnOnce() + Send>;
type DamageLoader = Box<dyn Fn(&Path) -> Option<Vec<(u64, u64)>> + Send + Sync>;
type RowPlanner = Box<dyn Fn(&Inputs, &Correction) -> Vec<CardEpisode> + Send + Sync>;
struct FakeWork {
    tasks: Mutex<std::collections::VecDeque<WorkTask>>,
    loader: DamageLoader,
    planner: RowPlanner,
    fail_spawn: std::sync::atomic::AtomicBool,
}
impl FakeWork {
    fn new() -> Self {
        Self {
            tasks: Mutex::new(Default::default()),
            loader: Box::new(|_| Some(vec![(2300 * 2048, 20 * 2048)])),
            planner: Box::new(|inputs, correction| CardIo.plan(inputs, correction)),
            fail_spawn: std::sync::atomic::AtomicBool::new(false),
        }
    }
    fn pop(&self) -> WorkTask {
        self.tasks
            .lock()
            .unwrap()
            .pop_front()
            .expect("admitted task")
    }
    fn len(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }
    fn finish(&self) {
        for _ in 0..100 {
            let next = self.tasks.lock().unwrap().pop_front();
            let Some(task) = next else {
                return;
            };
            task();
        }
        panic!("cache did not become idle");
    }
}
impl CardWork for FakeWork {
    fn damage(&self, path: &Path) -> Option<Vec<(u64, u64)>> {
        (self.loader)(path)
    }
    fn plan(&self, inputs: &Inputs, correction: &Correction) -> Vec<CardEpisode> {
        (self.planner)(inputs, correction)
    }
    fn spawn(&self, task: WorkTask) -> std::io::Result<()> {
        if self
            .fail_spawn
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(std::io::Error::other("injected spawn failure"));
        }
        self.tasks.lock().unwrap().push_back(task);
        Ok(())
    }
}
fn cache() -> (Arc<Cache>, Arc<FakeWork>) {
    let work = Arc::new(FakeWork::new());
    (Cache::new(work.clone()), work)
}
fn publish_test(cache: &Arc<Cache>, job: &str) {
    let titles = episode_disc();
    cache.publish(
        job,
        Inputs {
            titles: titles.clone(),
            label: "Show Season 2 Disc 2".into(),
            filename: "Show.mkv".into(),
            mapfile: job.into(),
        },
        &tv_plan(&titles, Some(4)),
        Correction {
            cfg: Config::default(),
            revision: 0,
            kind: MediaKind::Tv,
            tmdb_id: 0,
            start: Some(4),
        },
    );
}
fn correct(cache: &Arc<Cache>, job: &str, revision: u64, start: u16) {
    cache.replan(
        job,
        Correction {
            cfg: Config::default(),
            revision,
            kind: MediaKind::Tv,
            tmdb_id: 0,
            start: Some(start),
        },
    );
}
fn first(cache: &Arc<Cache>, job: &str) -> u16 {
    cache.episodes(job).unwrap()[0].episode
}
fn counts(cache: &Arc<Cache>) -> (usize, usize) {
    let p = cache.plans.lock().unwrap();
    (p.measuring, p.replanning)
}

#[test]
fn card_reads_return_cached_rows_while_loader_is_paused_without_owning_locks() {
    use std::sync::mpsc;
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let mut fake = FakeWork::new();
    fake.loader = Box::new(move |_| {
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
        Some(vec![(2300 * 2048, 20 * 2048)])
    });
    let work = Arc::new(fake);
    let cache = Cache::new(work.clone());
    publish_test(&cache, "paused");
    assert!(
        cache
            .episodes("paused")
            .unwrap()
            .iter()
            .all(|r| r.at_risk_ms == 0.0)
    );
    let worker = std::thread::spawn(work.pop());
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    // A held cache lock fails immediately instead of hanging the regression test.
    assert!(cache.plans.try_lock().is_ok());
    assert_eq!(first(&cache, "paused"), 4);
    assert_eq!(counts(&cache), (1, 0));
    release.send(()).unwrap();
    worker.join().unwrap();
    let rows = cache.episodes("paused").unwrap();
    assert_eq!(rows[0].at_risk_ms, 0.0);
    assert!(rows[1].at_risk_ms > 0.0);
    assert_eq!(rows[2].at_risk_ms, 0.0);
    work.finish();
}

#[test]
fn burst_polls_and_eviction_preserve_process_measurement_cap() {
    let (cache, work) = cache();
    publish_test(&cache, "a");
    publish_test(&cache, "b");
    std::thread::scope(|scope| {
        for _ in 0..32 {
            let cache = &cache;
            scope.spawn(move || {
                assert_eq!(first(cache, "a"), 4);
                assert_eq!(first(cache, "b"), 4);
            });
        }
    });
    assert_eq!(work.len(), MEASURE_CAP);
    for i in 0..KEEP * 3 {
        let job = format!("replacement-{i}");
        publish_test(&cache, &job);
        assert_eq!(first(&cache, &job), 4);
        assert_eq!(counts(&cache), (MEASURE_CAP, 0));
        assert_eq!(work.len(), MEASURE_CAP);
    }
    assert_eq!(cache.plans.lock().unwrap().entries.len(), KEEP);
    // Two indefinitely held map loads must leave the correction lane usable.
    let newest = format!("replacement-{}", KEEP * 3 - 1);
    correct(&cache, &newest, 1, 20);
    assert_eq!(counts(&cache), (MEASURE_CAP, 1));
    let replan = work.tasks.lock().unwrap().pop_back().unwrap();
    replan();
    assert_eq!(first(&cache, &newest), 20);
    assert_eq!(counts(&cache), (MEASURE_CAP, 0));
    work.finish();
    assert!(cache.episodes("a").is_none());
    assert!(cache.episodes("b").is_none());
    assert_eq!(counts(&cache), (0, 0));
}

#[test]
fn old_measurement_cannot_change_a_new_row_generation_or_incarnation() {
    let mut fake = FakeWork::new();
    fake.planner = Box::new(|inputs, correction| {
        let mut rows = CardIo.plan(inputs, correction);
        // A changed selection order associates each number with a new title.
        rows.rotate_left(1);
        for (index, row) in rows.iter_mut().enumerate() {
            row.episode = correction.start.unwrap() + index as u16;
        }
        rows
    });
    let work = Arc::new(fake);
    let cache = Cache::new(work.clone());
    publish_test(&cache, "job");
    cache.episodes("job");
    let old_measurement = work.pop();
    correct(&cache, "job", 1, 20);
    let replan = work.pop();
    replan();
    assert_eq!(first(&cache, "job"), 20);
    old_measurement();
    let rows = cache.episodes("job").unwrap();
    assert_eq!(rows[0].episode, 20);
    assert!(rows.iter().all(|r| r.at_risk_ms == 0.0));
    work.finish();
    let refreshed = cache.episodes("job").unwrap();
    assert!(refreshed[0].at_risk_ms > 0.0);
    assert_eq!(refreshed[1].at_risk_ms, 0.0);
    cache.plans.lock().unwrap().entries[0].measured = None;
    cache.episodes("job");
    let previous_incarnation = work.pop();
    publish_test(&cache, "job");
    previous_incarnation();
    // Completion of the old map read must not publish into the replacement.
    let p = cache.plans.lock().unwrap();
    assert!(p.entries[0].episodes.iter().all(|r| r.at_risk_ms == 0.0));
    drop(p);
    work.finish();
}

#[test]
fn unreadable_maps_keep_measured_values_and_failed_loads_are_throttled() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let readable = Arc::new(AtomicBool::new(true));
    let read_flag = readable.clone();
    let mut fake = FakeWork::new();
    fake.loader = Box::new(move |_| {
        read_flag
            .load(Ordering::SeqCst)
            .then_some(vec![(2300 * 2048, 20 * 2048)])
    });
    let work = Arc::new(fake);
    let cache = Cache::new(work.clone());
    publish_test(&cache, "job");
    let cold = cache.episodes("job").unwrap();
    assert!(cold.iter().all(|r| r.at_risk_ms == 0.0));
    work.finish();
    let warm = cache.episodes("job").unwrap();
    assert!(warm[1].at_risk_ms > 0.0);
    readable.store(false, Ordering::SeqCst);
    cache.plans.lock().unwrap().entries[0].measured = Some(Instant::now() - MEASURE_EVERY);
    cache.episodes("job");
    work.finish();
    for _ in 0..100 {
        assert_eq!(cache.episodes("job").unwrap(), warm);
    }
    assert_eq!(work.len(), 0);
}

#[test]
fn corrections_coalesce_and_dispatch_other_jobs_when_a_slot_frees() {
    use std::sync::mpsc;
    let (entered_tx, entered) = mpsc::channel();
    let (release, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let mut fake = FakeWork::new();
    fake.planner = Box::new(move |inputs, correction| {
        if inputs.mapfile == Path::new("a") && correction.revision == 1 {
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }
        CardIo.plan(inputs, correction)
    });
    let work = Arc::new(fake);
    let cache = Cache::new(work.clone());
    for job in ["a", "b", "c"] {
        publish_test(&cache, job);
    }
    correct(&cache, "a", 1, 5);
    let worker = std::thread::spawn(work.pop());
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    correct(&cache, "b", 1, 8);
    correct(&cache, "c", 1, 11);
    for revision in 2..=100 {
        correct(&cache, "a", revision, revision as u16);
    }
    assert_eq!(counts(&cache).1, REPLAN_CAP);
    assert_eq!(work.len(), REPLAN_CAP - 1);
    let p = cache.plans.lock().unwrap();
    let a = p.entries.iter().find(|p| p.job == "a").unwrap();
    assert_eq!(a.latest.revision, 100);
    assert!(a.replan_pending);
    drop(p);
    release.send(()).unwrap();
    worker.join().unwrap(); // obsolete revision 1, newest correction pending
    assert_eq!(first(&cache, "a"), 4);
    work.finish();
    assert_eq!(first(&cache, "a"), 100);
    assert_eq!(first(&cache, "b"), 8);
    assert_eq!(first(&cache, "c"), 11);
    work.finish();
    assert_eq!(counts(&cache), (0, 0));
}

#[test]
fn durable_revision_orders_delayed_enqueues_and_movie_clears_tv_rows() {
    let (cache, work) = cache();
    publish_test(&cache, "job");
    correct(&cache, "job", 2, 20);
    correct(&cache, "job", 1, 5); // earlier save reaches the cache last
    work.finish();
    assert_eq!(first(&cache, "job"), 20);
    cache.replan(
        "job",
        Correction {
            cfg: Config::default(),
            revision: 3,
            kind: MediaKind::Movie,
            tmdb_id: 0,
            start: None,
        },
    );
    work.finish();
    assert!(cache.episodes("job").is_none());
    correct(&cache, "job", 4, 30); // empty rows must still admit corrections
    work.finish();
    assert_eq!(first(&cache, "job"), 30);
    work.finish();
}

#[test]
fn republishing_retains_newer_correction_and_rejects_old_scan_completion() {
    let (cache, work) = cache();
    publish_test(&cache, "job");
    correct(&cache, "job", 2, 20);
    let old_scan_worker = work.pop();
    let titles = episode_disc();
    cache.publish(
        "job",
        Inputs {
            titles: titles.clone(),
            label: "Show Season 5 Disc 2".into(),
            filename: "New.mkv".into(),
            mapfile: "new-map".into(),
        },
        &tv_plan(&titles, Some(4)),
        Correction {
            cfg: Config::default(),
            revision: 0,
            kind: MediaKind::Tv,
            tmdb_id: 0,
            start: Some(4),
        },
    );
    let new_scan_worker = work.pop();
    new_scan_worker();
    assert_eq!(first(&cache, "job"), 20);
    old_scan_worker();
    assert_eq!(first(&cache, "job"), 20);
    assert_eq!(cache.episodes("job").unwrap()[0].season, 5);
    work.finish();
    assert_eq!(counts(&cache), (0, 0));
}

#[test]
fn prepublication_correction_is_reconciled_and_old_capture_revision_is_ignored() {
    let (cache, work) = cache();
    let device = "card-plan-prepublication-device";
    let job = "card-plan-prepublication-job";
    let metadata = super::super::staging::UserMetadata {
        episode_start: Some(20),
        media_type: MediaKind::Tv,
        title: "Show".into(),
        year: 0,
        tmdb_id: 0,
        poster_url: String::new(),
        overview: String::new(),
    };
    let mut state = super::super::state::RipState {
        job_id: job.into(),
        ..Default::default()
    };
    state.publish_user_metadata(job, 2, metadata);
    super::super::STATE
        .lock()
        .unwrap()
        .insert(device.into(), state);
    correct(&cache, job, 2, 20); // no entry yet: handler already updated STATE
    publish_test(&cache, job);
    cache.reconcile(&Config::default(), device, job);
    work.finish();
    assert_eq!(first(&cache, job), 20);
    work.finish();
    // Initial revision is authoritative: an older enqueue cannot replace it.
    let titles = episode_disc();
    cache.publish(
        job,
        Inputs {
            titles: titles.clone(),
            label: "Show Season 2".into(),
            filename: "Show.mkv".into(),
            mapfile: job.into(),
        },
        &tv_plan(&titles, Some(30)),
        Correction {
            cfg: Config::default(),
            revision: 3,
            kind: MediaKind::Tv,
            tmdb_id: 0,
            start: Some(30),
        },
    );
    correct(&cache, job, 2, 20);
    assert_eq!(first(&cache, job), 30);
    work.finish();
    super::super::STATE.lock().unwrap().remove(device);
}

#[test]
fn spawn_failure_and_panics_release_owned_slots_and_preserve_latest_work() {
    use std::sync::atomic::Ordering;
    let (cache, work) = cache();
    publish_test(&cache, "job");
    work.fail_spawn.store(true, Ordering::SeqCst);
    correct(&cache, "job", 1, 20);
    assert_eq!(counts(&cache), (0, 0));
    {
        let plans = cache.plans.lock().unwrap();
        assert!(plans.entries[0].replanning.is_none());
        assert!(plans.entries[0].replan_pending);
    }
    cache.episodes("job"); // retry opportunity after a failed spawn
    work.finish();
    assert_eq!(first(&cache, "job"), 20);
    work.finish();

    use std::sync::atomic::AtomicBool;
    let plan_panic = AtomicBool::new(true);
    let load_panic = AtomicBool::new(true);
    let mut fake = FakeWork::new();
    fake.planner = Box::new(move |inputs, correction| {
        if plan_panic.swap(false, Ordering::SeqCst) {
            panic!("injected planner panic");
        }
        CardIo.plan(inputs, correction)
    });
    fake.loader = Box::new(move |_| {
        if load_panic.swap(false, Ordering::SeqCst) {
            panic!("injected loader panic");
        }
        Some(vec![])
    });
    let work = Arc::new(fake);
    let cache = Cache::new(work.clone());
    publish_test(&cache, "job");
    correct(&cache, "job", 1, 20);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(work.pop())).is_err());
    assert_eq!(counts(&cache), (0, 0));
    assert!(cache.plans.lock().unwrap().entries[0].replan_pending);
    cache.dispatch(None); // retained correction can retry after planner panic
    work.pop()();
    assert_eq!(counts(&cache), (1, 0)); // fresh measurement is now pending
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(work.pop())).is_err());
    assert_eq!(counts(&cache), (0, 0));
    assert!(cache.plans.lock().unwrap().entries[0].measuring.is_none());
    assert_eq!(first(&cache, "job"), 20);
    work.finish();
}

#[test]
fn mapfile_damage_interpretation_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disc.iso.mapfile");
    let mut map = freemkv_engine::Mapfile::open_or_create(&path, 7000 * 2048, "test").unwrap();
    map.record(0, 7000 * 2048, freemkv_engine::SectorStatus::Finished)
        .unwrap();
    map.record(
        2300 * 2048,
        20 * 2048,
        freemkv_engine::SectorStatus::NonTrimmed,
    )
    .unwrap();
    map.flush().unwrap();
    assert_eq!(
        CardIo.damage(&path).unwrap(),
        vec![(2300 * 2048, 20 * 2048)]
    );
}

#[test]
fn replan_permits_survive_eviction_and_old_panic_cannot_clear_new_owner() {
    let mut fake = FakeWork::new();
    let fail_once = std::sync::atomic::AtomicBool::new(true);
    fake.planner = Box::new(move |inputs, correction| {
        if fail_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
            panic!("old-incarnation panic");
        }
        CardIo.plan(inputs, correction)
    });
    let work = Arc::new(fake);
    let cache = Cache::new(work.clone());
    publish_test(&cache, "job");
    correct(&cache, "job", 1, 10);
    let old = work.pop();
    publish_test(&cache, "job");
    correct(&cache, "job", 2, 20);
    assert_eq!(counts(&cache).1, REPLAN_CAP);
    let owner = cache.plans.lock().unwrap().entries[0]
        .replanning
        .clone()
        .unwrap();
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(old)).is_err());
    assert!(Arc::ptr_eq(
        cache.plans.lock().unwrap().entries[0]
            .replanning
            .as_ref()
            .unwrap(),
        &owner
    ));
    assert_eq!(counts(&cache).1, 1);
    work.finish();
    assert_eq!(first(&cache, "job"), 20);
    work.finish();

    for job in ["held-a", "held-b"] {
        publish_test(&cache, job);
        correct(&cache, job, 2, 20);
    }
    for i in 0..KEEP * 3 {
        let job = format!("replace-{i}");
        publish_test(&cache, &job);
        correct(&cache, &job, 2, 30);
        assert_eq!(counts(&cache).1, REPLAN_CAP);
        assert_eq!(work.len(), REPLAN_CAP);
    }
    work.finish();
    assert!(cache.episodes("held-a").is_none());
    assert!(cache.episodes("held-b").is_none());
    assert_eq!(first(&cache, &format!("replace-{}", KEEP * 3 - 1)), 30);
    work.finish();
    assert_eq!(counts(&cache), (0, 0));
}

#[test]
fn capture_revision_retains_matching_payload_for_later_older_republication() {
    let (cache, work) = cache();
    let titles = episode_disc();
    cache.publish(
        "job",
        Inputs {
            titles: titles.clone(),
            label: "Show Season 2".into(),
            filename: "Show.mkv".into(),
            mapfile: "job".into(),
        },
        &tv_plan(&titles, Some(30)),
        Correction {
            cfg: Config::default(),
            revision: 3,
            kind: MediaKind::Tv,
            tmdb_id: 0,
            start: Some(30),
        },
    );
    publish_test(&cache, "job"); // replacement scan computed from revision 0
    work.finish();
    assert_eq!(first(&cache, "job"), 30);
    work.finish();
}
