//! App paths no other test reached: workers that vanish before answering, workers that
//! cannot start, the ready screen's drive rows and the run tick's notices.
use super::tests::SPAWN_FAILS;
use super::*;

// A receiver whose worker is gone without answering.
fn dead<T>() -> std::sync::mpsc::Receiver<T> {
    let (_tx, rx) = std::sync::mpsc::channel();
    rx
}

fn logged(app: &App, needle: &str) -> bool {
    app.log.iter().any(|l| l.text.contains(needle))
}

// Every `spawn_worker` on this thread fails while one of these is alive.
struct NoThreads(bool);

impl NoThreads {
    fn on() -> Self {
        NoThreads(SPAWN_FAILS.with(|f| f.replace(true)))
    }
}

impl Drop for NoThreads {
    fn drop(&mut self) {
        SPAWN_FAILS.with(|f| f.set(self.0));
    }
}

// One ten-minute title, as an image scan reports it.
fn one_title() -> Scanned {
    Scanned {
        selection_model: freemkv_engine::SelectionModel::from_titles(&[{
            let mut title = libfreemkv::DiscTitle::empty();
            title.duration_secs = 600.0;
            title
        }])
        .into_owned(),
        label: "COV_DISC".to_string(),
        volume_id: "COV_DISC".to_string(),
        rows: vec![crate::engine::Row {
            item: String::new(),
            format: String::new(),
            notes: String::new(),
            type_s: "Title".to_string(),
            desc: "1. (0 chapters)".to_string(),
            depth: 1,
            checkable: true,
            title: 0,
            info: String::new(),
            pid: None,
            duration_secs: 600.0,
            lang: String::new(),
            forced: false,
            mirrors: None,
            size_bytes: None,
        }],
        key_summary: "none".to_string(),
        title_count: 1,
        video_codecs: vec!["MPEG-2".to_string()],
        title_sizes: Vec::new(),
        capacity_bytes: 0,
        title_ids: Vec::new(),
        details: Vec::new(),
        keys: None,
        needs_disc: false,
        refusal: None,
    }
}

fn drive(present: Option<bool>) -> HomeDrive {
    HomeDrive {
        device: "/dev/fmkv-cov-sr0".into(),
        display: "E:  COV DRIVE".into(),
        legacy_label: "COV DRIVE".into(),
        present,
    }
}

#[test]
fn a_ready_screen_drive_row_says_whether_it_holds_a_disc() {
    let cases = [
        (
            Some(true),
            crate::strings::get_or("gui.home.loaded", "Disc inserted"),
        ),
        (
            Some(false),
            crate::strings::get_or("gui.home.empty", "Empty"),
        ),
        (
            None,
            crate::strings::get_or("gui.home.unknown", "Disc status unknown"),
        ),
    ];
    for (present, status) in cases {
        assert_eq!(
            home_drive_label(&drive(present)),
            format!("E:  COV DRIVE — {status}")
        );
    }
    let mut app = App::new();
    assert!(app.prefer_home_drive(3).is_empty(), "no such drive");
    app.disc_present = None;
    app.home_drives = vec![drive(None), drive(Some(true))];
    assert_eq!(app.view().disc_present, Some(true));
}

// Each worker that dies before answering is collected on the tick and said, never left
// hanging in its slot.
#[test]
fn workers_that_vanish_are_collected_and_said() {
    let mut app = App::new();

    app.update_check = Some(dead());
    assert_eq!(app.poll_update(), vec![Effect::Redraw]);
    assert!(app.update_check.is_none());
    assert!(logged(
        &app,
        &crate::strings::get_or(
            "gui.log.update_worker_stopped",
            "Update check failed: worker stopped before returning a result",
        )
    ));

    app.keydb = Some(dead());
    assert_eq!(app.poll_keydb(), vec![Effect::Redraw]);
    assert!(app.keydb.is_none());
    assert!(app.keydb_note.is_some());

    app.keyserver_check = Some(dead());
    assert_eq!(app.poll_keyserver(), vec![Effect::Redraw]);
    assert!(app.keyserver_check.is_none());
    assert!(app.keyserver_verdict.is_some());

    app.ejecting = Some(("/nonexistent/cov.iso".into(), dead()));
    app.poll_eject();
    assert!(app.ejecting.is_none());
    assert!(logged(
        &app,
        &crate::strings::get_or(
            "gui.log.eject_worker_stopped",
            "eject worker stopped before returning a result",
        )
    ));

    app.presence = Some(("/nonexistent/cov.iso".into(), dead()));
    assert!(app.poll_presence().is_empty());
    assert!(app.presence.is_none());

    app.saving = Some((dead(), true));
    assert_eq!(app.poll_save(), vec![Effect::Redraw]);
    assert!(app.saving.is_none());
    assert!(
        app.settings_unsaved,
        "an unanswered write is not a saved one"
    );

    app.home_discovery = Some((app.home_generation, false, dead()));
    app.poll_home();
    assert!(app.home_discovery.is_none());
    assert_eq!(
        app.disc_present,
        Some(false),
        "no drive answered: none loaded"
    );

    app.source = "/nonexistent/cov.iso".into();
    app.recheck = Some((app.source.clone(), libfreemkv::Halt::new(), dead()));
    assert_eq!(app.poll_recheck(), vec![Effect::Redraw]);
    assert!(app.recheck.is_none());
    assert!(logged(
        &app,
        &crate::strings::get_or(
            "gui.log.scan_worker_stopped",
            "Source scan worker stopped before returning a result.",
        )
    ));
}

// A recheck that finds reasons says the rip cannot start, and why.
#[test]
fn a_recheck_with_reasons_says_the_rip_cannot_start() {
    let mut app = App::new();
    app.source = "/nonexistent/cov.iso".into();
    let (tx, rx) = std::sync::mpsc::channel();
    app.recheck = Some((app.source.clone(), libfreemkv::Halt::new(), rx));
    tx.send((None, Ok(vec!["cov-reason-one".to_string()])))
        .expect("the receiver is held");
    assert_eq!(app.poll_recheck(), vec![Effect::Redraw]);
    assert!(logged(&app, "cov-reason-one"));
    assert!(app.seed.is_none());
}

// With no thread to spare: discovery is skipped, a recheck says it could not start, and a
// Settings write (and the one queued at Quit) runs inline instead of being lost.
#[test]
fn workers_that_cannot_start_degrade_in_place() {
    let _no = NoThreads::on();
    let mut app = App::new();
    assert!(app.discover_home().is_empty());
    assert!(app.home_discovery.is_none());

    app.source = "/nonexistent/cov.iso".into();
    assert_eq!(
        app.recheck_preflight("/nonexistent/cov.iso", vec![0], None),
        vec![Effect::Redraw]
    );
    assert!(app.recheck.is_none());

    assert!(app.save_settings(true).is_empty());
    assert!(app.saving.is_none());
    assert!(!app.settings_unsaved);
    assert!(logged(
        &app,
        &crate::strings::get_or("gui.log.settings_saved", "Settings saved")
    ));

    app.save_queued = Some(false);
    app.flush_settings();
    assert!(app.save_queued.is_none());
}

// A second Check for updates while one runs starts nothing; a container that cannot hold
// the ticked video says so when it is picked.
#[test]
fn check_updates_in_flight_and_a_container_mismatch_on_pick() {
    let mut app = App::new();
    let (_tx, rx) = std::sync::mpsc::channel();
    app.update_check = Some(rx);
    assert_eq!(app.dispatch(Cmd::CheckUpdates), vec![Effect::Redraw]);

    let sc = one_title();
    app.video_codecs = sc.video_codecs.clone();
    app.tree = Tree::from_scan(&sc, "All titles", 0.0, &LangPrefs::default());
    app.dispatch(Cmd::SetFormat("Selected titles → MP4"));
    if let Some(m) = app.container_mismatch() {
        assert!(logged(&app, &m), "{m}");
    }
}

// "Keep encrypted" on an image source is ignored, and said to be.
#[test]
fn keep_encrypted_on_an_image_is_said_to_be_ignored() {
    let out = tempfile::tempdir().unwrap();
    let mut app = App::new();
    app.apply_scan("/nonexistent/fmkv-cov-raw.iso", Ok(one_title()), false);
    app.output_dir = out.path().display().to_string();
    app.settings.raw = true;
    app.format = "Whole disc → ISO image".into();
    app.dispatch(Cmd::Run);
    if let Some(run) = &app.run {
        run.cancel.store(true, Ordering::SeqCst);
        assert!(logged(
            &app,
            &crate::strings::get_or(
                "gui.log.raw_disc_only",
                "“Keep encrypted (raw)” applies only to a disc in a drive; ignoring it for this source.",
            )
        ));
    }
}

// The run tick moves the worker's lines into the log and says once that sectors were lost.
#[test]
fn the_run_tick_reports_worker_lines_and_unreadable_sectors() {
    let mut app = App::new();
    let st = Arc::new(crate::engine::RunState::default());
    st.lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push("cov worker line".into());
    st.prog
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sectors_bad = 7;
    app.run = Some(st);
    app.page = Page::Progress;
    app.tick();
    app.tick();
    assert!(logged(&app, "cov worker line"));
    let notice = crate::strings::fmt("gui.log.unreadable", &[("n", "7")]);
    assert_eq!(
        app.log.iter().filter(|l| l.text == notice).count(),
        1,
        "said once"
    );
    assert_eq!(app.reported_bad, 7);
    app.run = None;
}

// A committed form that left the startup drive and forced subtitles alone keeps the App's
// current values for them.
#[test]
fn a_form_commit_keeps_values_it_did_not_change() {
    let mut app = App::new();
    let before = app.settings.clone();
    let edited = before.clone();
    app.settings.startup_mode = "Open any loaded disc".into();
    app.settings.preferred_drive = "E:  COV DRIVE".into();
    app.settings.forced_sub_langs = "fra".into();
    let merged = app.commit_settings(&before, edited);
    assert_eq!(merged.startup_mode, "Open any loaded disc");
    assert_eq!(merged.preferred_drive, "E:  COV DRIVE");
    assert_eq!(merged.forced_sub_langs, "fra");
}

#[test]
fn labels_names_and_languages_for_the_edges() {
    let [name, year, _, season, disc, first] = details_labels();
    assert_eq!(detail_label(DetailField::Name), name);
    assert_eq!(detail_label(DetailField::Year), year);
    assert_eq!(detail_label(DetailField::Season), season);
    assert_eq!(detail_label(DetailField::Disc), disc);
    assert_eq!(detail_label(DetailField::FirstEpisode), first);
    let details = DiscDetails {
        name: "Cov".into(),
        ..DiscDetails::default()
    };
    assert_eq!(details.get(DetailField::Name), "Cov");

    assert_eq!(format_key("No such format"), None);
    assert_eq!(format_label("No such format"), "No such format");
    let und = stream_language("und");
    assert!(und.is_empty() || und == "und", "{und}");
    assert!(same_language("Covlandic", "COVLANDIC"));
    let hawaiian = lang_display_name("haw");
    assert!(!hawaiian.is_empty() && hawaiian != "haw", "{hawaiian}");
    assert!(!free_space("/nonexistent/fmkv-cov/dir").is_empty());
}
