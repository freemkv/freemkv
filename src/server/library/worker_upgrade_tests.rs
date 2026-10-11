// Upgrade from v1.8.3: its remux queue, links and Library folder open in this build,
// list the same rows, and the queued work runs.
use super::super::queue::JobState;
use super::*;
use crate::server::upgrade_fixture as fx;

struct Lines;

impl LineSink for Lines {
    fn line(&self, _kind: LineKind, _text: String) {}
}

fn sink<'a>(lib: &'a Library, arbiter: &'a Arbiter, job: &Job) -> JobSink<'a> {
    JobSink {
        lib,
        job_id: job.id,
        arbiter,
        epoch: arbiter.epoch(),
        log: JobLog(Mutex::new(None)),
        debug: false,
        iso_url: format!("iso://{}", job.iso.display()),
        last_activity: AtomicU64::new(0),
        term: Mutex::new(Term::default()),
    }
}

// The v1.8.3 config folder, remux staging and Library folder under `root`.
fn upgraded(root: &Path) -> (Config, Library, Dirs) {
    fx::install("v1.8.3/config", root, "config");
    fx::install("v1.8.3/remux-stage", root, "remux-stage");
    fx::library_tree(root);
    let cfg = fx::config(root);
    let lib = Library::open(&root.join("config"), &root.join("config/logs"));
    let d = super::super::dirs(&cfg);
    (cfg, lib, d)
}

// The columns v1.8.3's listing was recorded with, for one row.
fn columns(r: &super::super::RowView) -> serde_json::Value {
    let v = serde_json::to_value(r).unwrap();
    serde_json::json!({
        "title": v["title"], "key": v["key"], "kind": v["kind"], "note": v["note"],
        "linked": v["linked"], "mkv": v["mkv"], "iso": v["iso"], "target": v["target"],
        "job_state": v["job"]["state"], "job_note": v["job"]["note"],
        "result": v["result"]["outcome"],
    })
}

fn listed(lib: &Library, d: &Dirs) -> Vec<serde_json::Value> {
    assert!(lib.index_now(d));
    let listing = lib.listing(d);
    assert!(!listing.incomplete && !listing.scanning);
    let mut rows: Vec<_> = listing.rows.iter().map(columns).collect();
    rows.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    rows
}

// v1.8.3 listed only the movie folder; this build adds the TV folder's own MKVs.
// Every row v1.8.3 showed must be there unchanged, and nothing else may appear.
fn same_as_v1_8_3(root: &Path, rows: &[serde_json::Value]) {
    let tv = root.join("out/tv");
    let (added, kept): (Vec<_>, Vec<_>) = rows.iter().cloned().partition(|r| {
        r["mkv"]
            .as_str()
            .is_some_and(|m| Path::new(m).starts_with(&tv))
    });
    assert_eq!(kept, recorded(root));
    for r in &added {
        assert!(Path::new(r["mkv"].as_str().unwrap()).is_file(), "{r}");
        assert_eq!(
            (&r["kind"], &r["iso"]),
            (&"mkv_only".into(), &serde_json::Value::Null)
        );
    }
    assert_eq!(added.len(), 1, "one show in the TV folder");
}

fn recorded(root: &Path) -> Vec<serde_json::Value> {
    let mut rows: Vec<serde_json::Value> =
        serde_json::from_str(&fx::text("v1.8.3/listing-1.8.3.json", root)).unwrap();
    rows.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    rows
}

#[test]
fn v1_8_3_library_lists_the_same_rows_with_no_phantoms() {
    let (_t, root) = fx::root();
    let (_cfg, lib, d) = upgraded(&root);
    assert!(lib.source_match(Path::new("/x.iso")).is_ok());
    same_as_v1_8_3(&root, &listed(&lib, &d));
    // The link to an MKV removed since (Zulu) is kept but makes no row.
    let links = super::super::links::load(&root.join("config"));
    assert_eq!(links.len(), 2);
}

#[test]
fn v1_8_3_queue_opens_and_its_queued_work_runs() {
    let (_t, root) = fx::root();
    let (mut cfg, lib, d) = upgraded(&root);
    let config = root.join("config");
    assert!(!config.join("library-queue.json.unreadable").exists());
    let q = lib.queue.snapshot();
    let states: Vec<_> = q.jobs.iter().map(|j| (j.id, j.state, j.note)).collect();
    assert_eq!(
        states,
        vec![
            (1, JobState::Done, None),
            (2, JobState::Failed, None),
            (3, JobState::Queued, Some(JobNote::Interrupted)),
            (4, JobState::Queued, None),
            (5, JobState::Queued, Some(JobNote::StagedWaiting)),
        ]
    );
    assert_eq!(q.results.len(), 2);
    let kept = q.jobs[4]
        .staged
        .clone()
        .expect("the adopted job keeps its file");
    assert!(kept.exists());

    // Hotel and Delta: planned from their ISO now, as this build plans every job.
    cfg.tmdb_api_key.clear();
    let arbiter = Arbiter::new();
    let image = crate::ku_fixture::bd_image();
    for title in ["Hotel (2008)", "Delta (2004)"] {
        std::fs::write(root.join(format!("out/iso/{title}.iso")), &image.img.image).unwrap();
        let (mut job, _) = next_job(&lib, &arbiter, &d, &|_| Ok(()), u64::MAX).unwrap();
        assert_eq!(job.title, title);
        maybe_attach_tv_plan(&lib, &cfg, &mut job).unwrap();
        let plan = job.plan.clone().expect("planned");
        let s = sink(&lib, &arbiter, &job);
        let ending = remux_planned_with(&job, &plan, &cfg, &s, |request, kept, _| {
            assert!(kept.is_none());
            std::fs::write(&request.target, fx::old_mkv())?;
            Ok(Some(fx::OLD_STAMP.into()))
        });
        assert!(matches!(ending, Ending::Done { .. }), "{title}: {ending:?}");
        finish(&lib, &job, ending, Duration::from_secs(1), &Lines);
        assert!(
            root.join(format!("out/movies/{title}/{title}.mkv"))
                .exists()
        );
    }
    // Golf: only its kept file is copied in.
    let (job, _) = next_job(&lib, &arbiter, &d, &|_| Ok(()), u64::MAX).unwrap();
    assert_eq!((job.id, job.staged.as_ref()), (5, Some(&kept)));
    let ending = remux(&job, &cfg, &sink(&lib, &arbiter, &job));
    assert!(matches!(ending, Ending::Done { .. }), "{ending:?}");
    finish(&lib, &job, ending, Duration::from_secs(1), &Lines);
    let golf = root.join("out/movies/Golf (2007)/Golf (2007).mkv");
    assert_eq!(
        std::fs::read(&golf).unwrap(),
        std::fs::read(fx::fixture(
            "v1.8.3/remux-stage/Golf (2007).1f2335b9f41c.staged.mkv"
        ))
        .unwrap()
    );
    assert!(!kept.exists() && !kept.with_extension("json").exists());

    assert!(next_job(&lib, &arbiter, &d, &|_| Ok(()), u64::MAX).is_none());
    let q = lib.queue.snapshot();
    assert_eq!(q.count(JobState::Done), 4);
    assert_eq!(q.count(JobState::Failed), 1);
    // The Done rows show, the old ones and the new.
    let rows = listed(&lib, &d);
    for key in ["delta", "echo", "golf", "hotel"] {
        let row = rows.iter().find(|r| r["key"] == key).unwrap();
        assert_eq!(row["job_state"], "done", "{row}");
        assert_eq!(row["result"], "done", "{row}");
        assert_eq!(row["kind"], "remux", "{row}");
    }
    let fox = rows.iter().find(|r| r["key"] == "foxtrot").unwrap();
    assert_eq!(
        (&fox["job_state"], &fox["result"]),
        (&"failed".into(), &"failed".into())
    );
    assert_eq!(
        rows.len(),
        recorded(&root).len() + 1,
        "plus the TV folder's show"
    );
}

// The post-1.8.3 build's whole config folder (its plan-carrying queue and saved matches)
// over a Library holding what that queue names: the TV discs it planned and filed.
#[test]
fn a_dev_build_config_folder_opens_without_losing_rows() {
    let (_t, root) = fx::root();
    fx::install("dev-0eb9fd5/config", &root, "config");
    fx::install("v1.8.3/config/settings.json", &root, "config/settings.json");
    fx::library_tree(&root);
    let show = "Coastline II (2016)";
    let disc2 = "Coastline II (2016) - Disc 2";
    for iso in [show, disc2] {
        std::fs::write(root.join(format!("out/iso/{iso}.iso")), b"iso").unwrap();
    }
    for e in 1..=3 {
        let mkv = root.join(format!("out/movies/{show}/{show}_S01E{e:02}.mkv"));
        std::fs::create_dir_all(mkv.parent().unwrap()).unwrap();
        std::fs::write(mkv, fx::old_mkv()).unwrap();
    }
    let cfg = fx::config(&root);
    let lib = Library::open(&root.join("config"), &root.join("config/logs"));
    let d = super::super::dirs(&cfg);

    let config = root.join("config");
    assert!(!config.join("library-queue.json.unreadable").exists());
    let q = lib.queue.snapshot();
    assert_eq!(q.jobs.len(), 2);
    assert!(
        q.jobs
            .iter()
            .all(|j| j.plan.is_some() && j.outputs.len() == 3)
    );
    assert_eq!(q.results.len(), 2);
    let saved = lib
        .source_match(&root.join("out/iso/Bravo (2002).iso"))
        .unwrap()
        .expect("the saved correction");
    assert_eq!(saved.revision, 2);

    let rows = listed(&lib, &d);
    // Every row v1.8.3 listed for these files is still there (its queue columns aside).
    let place = |r: &serde_json::Value| {
        (
            r["key"].clone(),
            r["kind"].clone(),
            r["mkv"].clone(),
            r["iso"].clone(),
        )
    };
    let now: Vec<_> = rows.iter().map(place).collect();
    for before in recorded(&root) {
        assert!(now.contains(&place(&before)), "lost {before}");
    }
    for r in &rows {
        for side in ["mkv", "iso"] {
            if let Some(p) = r[side].as_str() {
                assert!(Path::new(p).is_file(), "phantom {r}");
            }
        }
    }
    let title = |key: &str| rows.iter().find(|r| r["key"] == key).unwrap()["title"].clone();
    assert_eq!(title("bravo"), "Bravo Corrected (2003)");
    assert_eq!(title("delta"), "Delta Show (2004)");
    // The dev build's Done and Failed fan-out jobs show on their source rows.
    let row = |key: &str| rows.iter().find(|r| r["key"] == key).unwrap();
    assert_eq!(
        (
            &row("coastlineii")["job_state"],
            &row("coastlineii")["result"]
        ),
        (&"done".into(), &"done".into())
    );
    let d2 = row("coastlineiidisc2");
    assert_eq!(
        (&d2["job_state"], &d2["result"]),
        (&"failed".into(), &"failed".into())
    );
}
