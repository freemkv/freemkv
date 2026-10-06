use super::*;

#[test]
fn free_space_is_measured_off_the_caller_for_the_latest_folder() {
    use std::sync::atomic::AtomicUsize;
    static GATE: Mutex<()> = Mutex::new(());
    static MEASURED: AtomicUsize = AtomicUsize::new(0);
    fn slow(dir: &str) -> String {
        let _g = GATE.lock().unwrap_or_else(|e| e.into_inner());
        MEASURED.fetch_add(1, Ordering::SeqCst);
        format!("free at {dir}")
    }
    let free = FreeSpace::default();
    let held = GATE.lock().unwrap();
    // A hung filesystem never holds the caller: each ask answers at once.
    for dir in ["/a", "/b", "/c"] {
        assert_eq!(free.get_with(dir, slow), "—");
    }
    drop(held);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while free.get_with("/c", slow) != "free at /c" {
        assert!(std::time::Instant::now() < until, "never measured");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(
        MEASURED.load(Ordering::SeqCst),
        2,
        "/b was passed over for /c"
    );
    free.forget();
    assert_eq!(
        free.get_with("/c", slow),
        "—",
        "measured afresh after forget"
    );
}

#[test]
fn titles_are_numbered_as_dash_t_numbers_them() {
    let title = |ti: usize| Row {
        index: ti,
        depth: 1,
        type_s: "Title".into(),
        desc: format!("{}  —  19 chapters", crate::engine::title_item(ti)),
        length: String::new(),
        size: String::new(),
        lang: String::new(),
        item: crate::engine::title_item(ti),
        format: String::new(),
        notes: String::new(),
        check: Some(Check::Off),
        check_enabled: true,
    };
    assert!(titles_numbered(&[title(0), title(1), title(2)]));
    assert!(
        titles_numbered(&[title(0), title(3)]),
        "a hidden title leaves a gap"
    );
    assert!(titles_numbered(&[]));
    assert!(!titles_numbered(&[title(1), title(0)]), "out of order");
    assert!(!titles_numbered(&[title(0), title(0)]), "a repeated number");
    let mut old = title(0);
    old.item = "1. 00800.mpls".into();
    assert!(!titles_numbered(&[old]), "the removed playlist naming");
    let mut desc = title(0);
    desc.desc = "00800.mpls".into();
    assert!(!titles_numbered(&[desc]));
}

#[test]
fn the_row_signature_sees_every_painted_cell_but_not_ticks() {
    let base = vec![Row {
        index: 1,
        depth: 1,
        type_s: "Title".into(),
        desc: "Title 1".into(),
        length: "1:30:00".into(),
        size: "6.8 GB".into(),
        lang: "eng".into(),
        item: "Title 1".into(),
        format: "MPEG-2".into(),
        notes: "19 chapters".into(),
        check: Some(Check::Off),
        check_enabled: true,
    }];
    let mut ticked = base.clone();
    ticked[0].check = Some(Check::On);
    assert_eq!(rows_sig(&base), rows_sig(&ticked));
    let changes: [fn(&mut Row); 10] = [
        |r| r.index = 2,
        |r| r.depth = 2,
        |r| r.type_s = "Audio".into(),
        |r| r.desc = "x".into(),
        |r| r.length = "1:29:59".into(),
        |r| r.size = "6.9 GB".into(),
        |r| r.lang = "deu".into(),
        |r| r.item = "Title 2".into(),
        |r| r.format = "H.264".into(),
        |r| r.notes = "20 chapters".into(),
    ];
    for (i, change) in changes.iter().enumerate() {
        let mut b = base.clone();
        change(&mut b[0]);
        assert_ne!(rows_sig(&base), rows_sig(&b), "change #{i} went unnoticed");
    }
    assert_ne!(rows_sig(&base), rows_sig(&[]));
}

// G5 (design §6): one table maps container extensions to schemes; every derived list
// agrees with it, and mpg/mpeg/vob read as mpg://.
#[test]
fn container_sources_drive_every_extension_decision() {
    for (ext, scheme) in crate::sources::CONTAINER_SOURCES {
        for e in [ext.to_string(), ext.to_ascii_uppercase()] {
            let path = format!("/m/movie.{e}");
            assert_eq!(container_scheme(&path), Some(*scheme), "{path}");
            assert!(is_container(&path), "{path}");
        }
        assert!(SOURCE_EXTS.contains(ext), "{ext} missing from the picker");
    }
    assert_eq!(container_scheme("/m/a.vob"), Some("mpg"));
    assert_eq!(container_scheme("/m/a.MPEG"), Some("mpg"));
    assert_eq!(container_scheme("/m/a.mts"), Some("m2ts"));
    assert_eq!(container_scheme("/m/a.iso"), None);
    assert_eq!(container_scheme("/m/a.txt"), None);
    let extra: Vec<&&str> = SOURCE_EXTS
        .iter()
        .filter(|e| {
            !e.eq_ignore_ascii_case("iso")
                && !crate::sources::CONTAINER_SOURCES
                    .iter()
                    .any(|(x, _)| x.eq_ignore_ascii_case(e))
        })
        .collect();
    assert!(
        extra.is_empty(),
        "picker extensions outside the table: {extra:?}"
    );
}

// Case-sensitive pickers only show a DVD's upper-case MPEG files if the list names them.
#[test]
fn the_picker_lists_upper_case_mpeg_extensions() {
    for ext in ["MPG", "MPEG", "VOB"] {
        assert!(SOURCE_EXTS.contains(&ext), "{ext} missing from the picker");
    }
}

// Design §6: "output_formats(disc_source, mp4_ok, mpg_ok) adds Selected titles → MPG";
// J24: MPG carries MPEG-1/2 video only.
#[test]
fn mpg_is_offered_only_when_a_title_could_go_in_it() {
    let has = |f: Fit| {
        output_formats(true, f)
            .concat()
            .contains(&"Selected titles → MPG")
    };
    assert!(has(Fit {
        mp4: false,
        mpg: true
    }));
    assert!(!has(Fit {
        mp4: true,
        mpg: false
    }));
    assert!(has(true.into()), "unknown codecs offer everything");
    assert!(app_with_titles(&["MPEG-2"]).fit().mpg);
    assert!(!app_with_titles(&["H.264"]).fit().mpg);
    assert_eq!(format_key("Selected titles → MPG"), Some("gui.format.mpg"));
}

// gui.log.container_mismatch names the container, for MP4 and MPG alike.
#[test]
fn the_mismatch_names_whichever_container_was_chosen() {
    crate::strings::set_locale("en");
    let mut app = app_with_titles(&["MPEG-2", "H.264"]);
    app.format = "Selected titles → MPG".into();
    let m = app.container_mismatch().expect("H.264 cannot go in an MPG");
    assert!(
        m.contains("MPG") && m.contains("H.264") && !m.contains("MPEG-2"),
        "{m}"
    );
    app.format = "Selected titles → MP4".into();
    let m = app
        .container_mismatch()
        .expect("MPEG-2 cannot go in an MP4");
    assert!(m.contains("MP4") && m.contains("MPEG-2"), "{m}");
}

// The CLI refuses `--raw` without a disc:// source and an iso:// dest; the GUI ignores it.
#[test]
fn raw_applies_only_to_a_drive_to_iso_copy_like_the_cli() {
    assert!(raw_applies(true, true, true));
    assert!(!raw_applies(true, false, true), "iso-only");
    assert!(
        !raw_applies(true, true, false),
        "an image source is not a disc"
    );
    assert!(!raw_applies(false, true, true));
}

// Single pass must stay single pass even with a stale Multi-pass `max_passes`
// setting still typed in: `fe::plan_passes` decides multipass from the COUNT
// alone, so a non-zero leftover would have silently run multipass recovery.
#[test]
fn single_pass_zeroes_max_passes_whatever_is_typed() {
    assert_eq!(effective_max_passes("Single pass", 5), 0);
    assert_eq!(effective_max_passes("Single pass", 0), 0);
    assert_eq!(effective_max_passes("", 5), 0);
    assert_eq!(effective_max_passes("Multi-pass", 5), 5);
    assert_eq!(
        effective_max_passes("Multi-pass", 0),
        0,
        "zero passes is not multipass whatever the mode says"
    );
}

// A source pin: the disc can be swapped while the operator reviews the tree, so the request
// must carry title identities to check against.
#[test]
fn the_rip_request_carries_the_identities_the_ticked_numbers_referred_to() {
    let src = include_str!("ui.rs").replace("\r\n", "\n");
    let start = src
        .find("\n    fn start_run(&mut self)")
        .expect("start_run definition present");
    let end = start
        + src[start..]
            .find("\n            state,\n        );")
            .expect("the start_rip call still closes the request");
    let body = &src[start..end];
    assert!(
        body.contains("\n                title_ids: self.title_ids.clone(),"),
        "the request must carry the scanned identities alongside the ticked \
             title numbers"
    );
}

/// Build one stream row for the preference tests.
fn row(type_s: &str, pid: u16, lang: &str, forced: bool) -> crate::engine::Row {
    crate::engine::Row {
        item: String::new(),
        format: String::new(),
        notes: String::new(),
        type_s: type_s.to_string(),
        desc: String::new(),
        depth: 2,
        checkable: true,
        title: 0,
        info: String::new(),
        pid: Some(pid),
        duration_secs: 0.0,
        lang: lang.to_string(),
        forced,
        mirrors: None,
        size_bytes: None,
        role: None,
    }
}

#[test]
fn a_chapter_hangs_off_its_titles_chapter_list_which_starts_closed() {
    let row = |depth: u8, type_s: &str| Row {
        index: 0,
        depth,
        type_s: type_s.into(),
        desc: String::new(),
        length: String::new(),
        size: String::new(),
        lang: String::new(),
        item: String::new(),
        format: String::new(),
        notes: String::new(),
        check: None,
        check_enabled: false,
    };
    let rows = [
        row(0, "Disc"),
        row(1, "Title"),
        row(2, "Video"),
        row(2, "Chapters"),
        row(3, "Chapter"),
        row(3, "Chapter"),
        row(1, "Title"),
        row(2, "Audio"),
    ];
    assert_eq!(
        row_parents(&rows),
        [
            None,
            Some(0),
            Some(1),
            Some(1),
            Some(3),
            Some(3),
            Some(0),
            Some(6)
        ]
    );
    let closed: Vec<bool> = rows.iter().map(starts_collapsed).collect();
    assert_eq!(
        closed,
        [false, false, false, true, false, false, false, false]
    );
}

// A disc like Kung Fu's: English and German play-alls, and each episode in both languages.
fn episode_disc() -> Scanned {
    use freemkv_engine::TitleRole::{Episode, PlayAll};
    let mut sc = probe_scan();
    sc.rows.clear();
    let titles = [
        (PlayAll, "eng"),
        (PlayAll, "deu"),
        (Episode, "eng"),
        (Episode, "deu"),
        (Episode, "eng"),
        (Episode, "deu"),
    ];
    for (ti, (role, lang)) in titles.into_iter().enumerate() {
        let mut t = row("Title", 0, "", false);
        (t.depth, t.pid, t.title, t.duration_secs) = (1, None, ti, 2600.0);
        t.role = Some(role);
        sc.rows.push(t);
        let mut a = row("Audio", 0x80 + ti as u16, lang, false);
        a.title = ti;
        sc.rows.push(a);
        let mut sub = row("Subtitles", 0x20 + ti as u16, "eng", false);
        sub.title = ti;
        sc.rows.push(sub);
    }
    sc
}

// The bar's audio choice picks which language's version of a title counts.
#[test]
fn the_selection_bar_picks_episodes_and_the_film_in_the_chosen_languages() {
    let sc = episode_disc();
    let langs = |a: &[&str]| LangPrefs {
        audio: a.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    let ticked = |mode: &str, p: &LangPrefs| Tree::from_scan(&sc, mode, 0.0, p).ticked_titles();
    assert_eq!(ticked("Episodes", &langs(&["deu"])), vec![3, 5]);
    assert_eq!(
        ticked("Episodes", &langs(&["eng", "deu"])),
        vec![2, 3, 4, 5]
    );
    assert_eq!(ticked("Main film only", &langs(&["deu"])), vec![1]);
    assert_eq!(ticked("Main film only", &langs(&[])), vec![0]);
    assert_eq!(ticked("No titles", &langs(&[])), Vec::<usize>::new());
    // A language no title carries does not empty the choice.
    assert_eq!(ticked("Episodes", &langs(&["jpn"])), vec![2, 3, 4, 5]);
}

// `episode_disc` with, on every title, a regular French subtitle (0x40+) and a forced
// English one (0x60+), so the subtitle choices have both kinds to keep or drop.
fn pick_disc() -> Scanned {
    let mut sc = episode_disc();
    let mut rows = Vec::new();
    for r in sc.rows.drain(..) {
        let ti = r.title;
        let last_of_title = r.type_s == "Subtitles";
        rows.push(r);
        if last_of_title {
            for (pid, lang, forced) in [(0x40, "fra", false), (0x60, "eng", true)] {
                let mut s = row("Subtitles", pid + ti as u16, lang, forced);
                s.title = ti;
                rows.push(s);
            }
        }
    }
    sc.rows = rows;
    sc
}

// The stream PIDs ticked under title `ti`.
fn ticked_pids(tree: &Tree, ti: usize) -> Vec<u16> {
    tree.arena
        .iter()
        .filter(|n| n.title_idx == ti && *n.checked.borrow())
        .filter_map(|n| n.pid)
        .collect()
}

#[test]
fn subtitles_none_and_forced_only_untick_regular_subtitles() {
    let sc = pick_disc();
    let pids = |no_subtitles, no_forced| {
        let p = LangPrefs {
            no_subtitles,
            no_forced,
            ..Default::default()
        };
        ticked_pids(&Tree::from_scan(&sc, "Main film only", 0.0, &p), 0)
    };
    // Audio 0x80, regular 0x20 and 0x40, forced 0x60.
    assert_eq!(pids(false, false), vec![0x80, 0x20, 0x40, 0x60], "All");
    assert_eq!(pids(true, true), vec![0x80], "None");
    assert_eq!(pids(true, false), vec![0x80, 0x60], "Forced only");
}

// An App with `sc` open in the selection bar, as a finished scan leaves it.
fn picking(sc: Scanned) -> App {
    let mut app = App::new();
    app.pick_mode = "Main film only".into();
    app.pick_scan = Some(sc);
    app.repick();
    app
}

#[test]
fn the_subtitle_choices_step_through_their_states() {
    let mut app = picking(pick_disc());
    let pick = |app: &App| app.view().pick.expect("a source is open");
    let flags = |app: &App| {
        let p = &app.pick_prefs;
        (p.no_subtitles, p.no_forced)
    };
    let v = pick(&app);
    assert!(v.subs_all && !v.subs_none && !v.subs_forced);
    assert_eq!(v.subs_summary, "All");
    assert_eq!(
        v.subtitles,
        vec![("eng".into(), false), ("fra".into(), false)]
    );

    app.pick_subtitles(SubPick::None);
    assert_eq!(flags(&app), (true, true));
    let v = pick(&app);
    assert!(v.subs_none && !v.subs_all && !v.subs_forced);
    assert_eq!(v.subs_summary, "None");
    assert_eq!(ticked_pids(&app.tree, 0), vec![0x80]);

    app.pick_subtitles(SubPick::Forced);
    assert_eq!(flags(&app), (true, false));
    let v = pick(&app);
    assert!(v.subs_forced && !v.subs_none && !v.subs_all);
    assert_eq!(v.subs_summary, "Forced only");
    assert_eq!(ticked_pids(&app.tree, 0), vec![0x80, 0x60]);

    // A language from Forced only starts a fresh list; it keeps that language's forced
    // subtitles too.
    app.pick_subtitles(SubPick::Lang("fra".into()));
    assert_eq!(flags(&app), (false, false));
    assert_eq!(app.pick_prefs.subtitles, vec!["fra".to_string()]);
    assert_eq!(app.pick_prefs.forced, app.pick_prefs.subtitles);
    let v = pick(&app);
    assert!(!v.subs_all && !v.subs_none && !v.subs_forced);
    assert_eq!(v.subs_summary, "fra");
    assert_eq!(
        v.subtitles,
        vec![("eng".into(), false), ("fra".into(), true)]
    );
    assert_eq!(ticked_pids(&app.tree, 0), vec![0x80, 0x40]);

    app.pick_subtitles(SubPick::Lang("eng".into()));
    assert_eq!(pick(&app).subs_summary, "eng, fra");
    assert_eq!(ticked_pids(&app.tree, 0), vec![0x80, 0x20, 0x40, 0x60]);

    // Untoggling the last language leaves None, not All.
    app.pick_subtitles(SubPick::Lang("eng".into()));
    app.pick_subtitles(SubPick::Lang("fra".into()));
    assert_eq!(flags(&app), (true, true));
    assert!(pick(&app).subs_none);

    app.pick_subtitles(SubPick::All);
    assert_eq!(flags(&app), (false, false));
    assert!(app.pick_prefs.subtitles.is_empty() && app.pick_prefs.forced.is_empty());
    assert!(pick(&app).subs_all);
}

#[test]
fn the_audio_choice_toggles_languages_and_all_clears_them() {
    let mut app = picking(pick_disc());
    app.pick_titles("Episodes");
    let v = app.view().pick.unwrap();
    assert!(v.audio_all);
    assert_eq!(v.audio_summary, "All");
    assert_eq!(v.audio, vec![("eng".into(), false), ("deu".into(), false)]);
    assert_eq!(app.tree.ticked_titles(), vec![2, 3, 4, 5]);

    app.pick_audio(Some("deu"));
    let v = app.view().pick.unwrap();
    assert!(!v.audio_all);
    assert_eq!(v.audio_summary, "deu");
    assert_eq!(v.audio, vec![("eng".into(), false), ("deu".into(), true)]);
    assert_eq!(app.tree.ticked_titles(), vec![3, 5]);

    app.pick_audio(Some("eng"));
    assert_eq!(app.view().pick.unwrap().audio_summary, "eng, deu");
    app.pick_audio(None);
    assert!(app.pick_prefs.audio.is_empty());
    assert!(app.view().pick.unwrap().audio_all);
}

#[test]
fn every_menu_entry_maps_back_to_its_own_choice() {
    let app = picking(pick_disc());
    let v = app.view().pick.unwrap();
    let subs: Vec<_> = v
        .subs_menu()
        .iter()
        .map(|e| (v.subs_choice(e.tag), e.separator_before))
        .collect();
    assert_eq!(
        subs,
        vec![
            (Some(SubPick::All), false),
            (Some(SubPick::None), false),
            (Some(SubPick::Forced), false),
            (Some(SubPick::Lang("eng".into())), true),
            (Some(SubPick::Lang("fra".into())), false),
        ]
    );
    let audio: Vec<_> = v
        .audio_menu()
        .iter()
        .map(|e| v.audio_choice(e.tag))
        .collect();
    assert_eq!(
        audio,
        vec![
            Some(None),
            Some(Some("eng".into())),
            Some(Some("deu".into()))
        ]
    );
    // A tag past the languages, or none at all, means nothing.
    for tag in [0, LANG_TAG + 5, -1] {
        assert_eq!(v.subs_choice(tag), None, "{tag}");
        assert_eq!(v.audio_choice(tag), None, "{tag}");
    }
}

#[test]
fn the_title_choices_offer_episodes_only_on_a_disc_that_proves_them() {
    let mut app = picking(pick_disc());
    let v = app.view().pick.unwrap();
    let modes: Vec<_> = v.titles.iter().map(|t| t.0).collect();
    assert_eq!(modes, PICK_TITLES.to_vec());
    let at = modes.iter().position(|m| *m == "Episodes").unwrap();
    assert_eq!(v.title_choice(at), Some("Episodes"));
    assert_eq!(v.title_choice(modes.len()), None);
    app.pick_titles("Episodes");
    let v = app.view().pick.unwrap();
    assert_eq!((v.title.as_str(), v.title_index()), ("Episodes", at));

    let plain = picking(probe_scan());
    let v = plain.view().pick.unwrap();
    assert!(v.titles.iter().all(|t| t.0 != "Episodes"));
    assert_eq!(v.titles.len(), PICK_TITLES.len() - 1);
}

// A forced-subtitle preference that matches nothing must keep NOTHING — unlike audio, a
// forced track with no match must not fall back to "keep everything": it displays over the
// picture unasked.
#[test]
fn a_forced_preference_matching_nothing_keeps_nothing() {
    let rows = [
        row("Audio", 1100, "eng", false),
        row("Audio", 1101, "fra", false),
        row("Subtitles", 1200, "eng", false),
        row("Subtitles", 1201, "fra", false),
        // Forced tracks — note there is no English one, as on the disc.
        row("Subtitles", 1300, "fra", true),
        row("Subtitles", 1301, "deu", true),
        row("Subtitles", 1302, "spa", true),
        row("Subtitles", 1303, "por", true),
    ];
    let refs: Vec<&crate::engine::Row> = rows.iter().collect();

    let prefs = LangPrefs::parse("en", "en", "en");
    let keep = preferred_pids(&refs, &prefs);

    for (pid, lang) in [(1300, "fra"), (1301, "deu"), (1302, "spa"), (1303, "por")] {
        assert!(
            !keep.contains(&pid),
            "forced {lang} was ticked, but only English forced subtitles were asked for \
                 — forced subtitles display by themselves, so this puts unwanted text on screen"
        );
    }
    // The other two classes are unaffected: both have an English track, and
    // each class resolves on its own terms.
    assert!(keep.contains(&1100), "English audio must still be kept");
    assert!(keep.contains(&1200), "English subtitles must still be kept");
    assert!(!keep.contains(&1101), "French audio was not asked for");
}

// The other half of the same rule: no preference is NOT an unmatched
// preference. An empty box means "no opinion" and keeps every forced
// track, or this fix would silently strip forced subs from every rip.
#[test]
fn an_empty_forced_preference_still_keeps_every_forced_track() {
    let rows = [
        row("Subtitles", 1300, "fra", true),
        row("Subtitles", 1301, "deu", true),
    ];
    let refs: Vec<&crate::engine::Row> = rows.iter().collect();

    let keep = preferred_pids(&refs, &LangPrefs::parse("en", "en", ""));
    assert!(keep.contains(&1300) && keep.contains(&1301));
}

/// And when the requested forced language IS present, it alone is kept.
#[test]
fn a_forced_preference_that_matches_keeps_only_that_language() {
    let rows = [
        row("Subtitles", 1300, "eng", true),
        row("Subtitles", 1301, "deu", true),
        row("Subtitles", 1302, "fra", true),
    ];
    let refs: Vec<&crate::engine::Row> = rows.iter().collect();

    let keep = preferred_pids(&refs, &LangPrefs::parse("", "", "en"));
    assert!(
        keep.contains(&1300),
        "the English forced track was asked for"
    );
    assert!(!keep.contains(&1301) && !keep.contains(&1302));
}

// Neither About box may hard-code what it reports (macOS once did). Source inspection, not
// a UI test: neither shell instantiates off its own platform.
#[test]
fn neither_about_box_hard_codes_its_version_or_key_count() {
    // CRLF-normalized: Windows CI checks the tree out with CRLF.
    let shells = [
        ("mac.rs", include_str!("mac.rs").replace("\r\n", "\n")),
        (
            "windows.rs",
            include_str!("windows.rs").replace("\r\n", "\n"),
        ),
    ];
    for (name, src) in &shells {
        for (key, must_contain) in [
            ("gui.about.version", "CARGO_PKG_VERSION"),
            // The engine row must derive from the LINKED engine's version,
            // not the wrapper's `CARGO_PKG_VERSION` — the two diverge when
            // libfreemkv is pinned to an older tag than this crate.
            ("gui.about.engine", "libfreemkv::VERSION_LABEL"),
            ("gui.about.keys", "keydb_status"),
        ] {
            let at = src
                .find(key)
                .unwrap_or_else(|| panic!("{name}: no About row for {key} — did the box move?"));
            // Bound the window at the NEXT About row so each is judged on its own
            // text. A fixed-size window bled into "engine" after "version", which
            // also contains CARGO_PKG_VERSION, letting a hard-coded version pass.
            let rest = &src[at + key.len()..];
            let end = rest.find("gui.about.").unwrap_or(rest.len());
            let window = &rest[..end];
            assert!(
                window.contains(must_contain),
                "{name}: the {key} row does not derive from {must_contain} — \
                     a literal here goes stale silently and is wrong for every \
                     user, not just at release time"
            );
        }
    }
}

// Both log panes must keep the NEWEST line in view (mac.rs once had no scroll call at all,
// despite a comment claiming parity with windows.rs). Source inspection, same reason as the
// test above.
#[test]
fn both_log_panes_keep_the_newest_line_in_view() {
    // CRLF-normalized: Windows CI checks the tree out with CRLF.
    let mac = include_str!("mac.rs").replace("\r\n", "\n");
    let win = include_str!("windows.rs").replace("\r\n", "\n");

    assert!(
        mac.contains("scrollRangeToVisible"),
        "the macOS log never scrolls: the newest line is written below the \
             visible area and the user has to drag to see it"
    );
    assert!(
        win.contains("self.log.set_selection("),
        "the Windows log no longer scrolls to its newest line"
    );
}

// Neither shell's worker-message drain may bail out on a poisoned inbox: an early return
// there also skips clearing the "busy" flag and stopping the drain timer, hanging the
// process.
#[test]
fn neither_shell_drain_gives_up_on_a_poisoned_inbox() {
    // CRLF-normalized: Windows CI checks the tree out with CRLF.
    let shells = [
        ("mac.rs", include_str!("mac.rs").replace("\r\n", "\n")),
        (
            "windows.rs",
            include_str!("windows.rs").replace("\r\n", "\n"),
        ),
    ];
    // Whitespace-collapsed so rustfmt cannot hide a regression by breaking
    // the expression across lines.
    let needle = concat!("inbox", ".lock()");
    for (name, raw) in &shells {
        let src: String = raw.split_whitespace().collect();
        // EVERY inbox lock, not just the drain's: the keydb worker's `push` has
        // the same `if let Ok(..)` shape, and dropping that message wedges the
        // UI the same way — button never re-enables, timer never stops.
        let mut found = 0usize;
        let mut at = 0usize;
        while let Some(i) = src[at..].find(needle) {
            let pos = at + i;
            let tail = &src[pos + needle.len()..];
            let head = &tail[..tail.len().min(80)];
            assert!(
                head.starts_with(".unwrap_or_else(") && head.contains("into_inner"),
                "{name}: an inbox lock does not recover from poison. That \
                     strands the keydb busy flag and leaves the drain timer \
                     firing forever, discarding the very messages that explain \
                     the panic. Near: {head}"
            );
            found += 1;
            at = pos + needle.len();
        }
        assert!(
            found >= 2,
            "{name}: expected both the keydb worker's push and the drain's \
                 take, found {found} — the needle stopped matching and this pin \
                 is now vacuous"
        );
    }
}

#[test]
fn sizes_roll_over_instead_of_staying_in_megabytes() {
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(2048), "2 KB");
    assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
    // The bug this pins: a 6 GB output once read "6103.5 M".
    assert_eq!(fmt_bytes(6 * 1024 * 1024 * 1024), "6.00 GB");
    assert_eq!(fmt_bytes(64_424_509_440), "60.00 GB");
}

// A failed probe (common case: empty tray) must look exactly like the app did before the
// probe existed — no notice, no log line — while a prompted open of the same bad source
// must still report.
#[test]
fn a_failed_probe_says_nothing_but_a_failed_open_still_reports() {
    const BAD: &str = "iso:///nonexistent/definitely-not-here.iso";
    let mut app = App::new();
    let before = app.log.len();
    // Driven to COMPLETION: the probe is async, so asserting right after
    // `open_probe` would assert about a scan that hadn't run yet — a test
    // that passes because nothing happened is the same as no test.
    app.open_probe(BAD);
    drain_probe(&mut app);
    assert_eq!(
        app.log.len(),
        before,
        "an unprompted probe that finds nothing must leave no trace, got: {:?}",
        &app.log[before..]
    );
    assert!(matches!(app.page, Page::Empty));

    app.open(BAD);
    assert!(
        app.log.len() > before,
        "a human who asked must be told the source could not be opened"
    );
}

/// Tick until the probe has been collected, with a bound so a probe that
/// never finishes fails the test instead of hanging the suite.
fn drain_probe(app: &mut App) -> Vec<Effect> {
    for _ in 0..2_000 {
        let fx = app.tick();
        if app.probe.is_none() {
            return fx;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the launch probe never finished");
}

// ── The launch probe is off the UI thread: it used to run drive enumeration,
// a SCSI scan and AACS key resolution synchronously, freezing the window until
// the drive answered. Now it uses the SAME `StartTicking`+`tick` seam as a rip.

/// `open_probe` must hand the work off and RETURN, asking for the tick
/// that will collect it.
#[test]
fn the_launch_probe_hands_off_instead_of_scanning_inline() {
    let mut app = App::new();
    let fx = app.open_probe(PROBE_SOURCE);
    assert!(
        fx.contains(&Effect::StartTicking),
        "the probe must ask for the tick that collects its result, got {fx:?}"
    );
    assert!(
        app.probe.is_some(),
        "the scan must be outstanding when open_probe returns — if it is \
             already collected, the work happened on this thread"
    );
    drain_probe(&mut app);
}

/// The probe gets its OWN slot. Reusing `run` would put the app on
/// `Page::Progress`, showing a rip that is not happening.
#[test]
fn the_probe_does_not_occupy_the_rip_slot() {
    let mut app = App::new();
    app.open_probe(PROBE_SOURCE);
    assert!(app.run.is_none(), "the probe must not look like a rip");
    assert!(
        !matches!(app.page, Page::Progress),
        "the probe must not put the app on the progress page"
    );
    drain_probe(&mut app);
}

/// A tick with a probe outstanding and no rip must NOT stop the timer —
/// that would strand the result with nothing left to collect it.
#[test]
fn ticking_continues_while_the_probe_is_outstanding() {
    let mut app = App::new();
    app.probe = Some(Arc::new(probe_state(None, false)));
    let fx = app.tick();
    assert!(
        !fx.contains(&Effect::StopTicking),
        "the tick that collects the probe was cancelled before it ran: {fx:?}"
    );
    app.probe = None;
    assert!(
        app.tick().contains(&Effect::StopTicking),
        "with nothing outstanding the timer must stop"
    );
}

/// ...and one that is merely slow is still waited for: the deadline must
/// not cancel the working case.
#[test]
fn a_probe_still_within_its_deadline_is_kept() {
    let mut app = App::new();
    app.probe = Some(Arc::new(probe_state(None, false)));
    let fx = app.tick();
    assert!(app.probe.is_some(), "a live probe was thrown away");
    assert!(!fx.contains(&Effect::StopTicking), "{fx:?}");
}

/// A finished probe is applied on the tick, on the UI thread.
#[test]
fn a_probe_result_is_applied_by_the_tick() {
    let mut app = App::new();
    app.probe = Some(Arc::new(probe_state(Some(Ok(probe_scan())), true)));
    let fx = app.tick();
    assert!(app.probe.is_none(), "a collected probe must clear its slot");
    assert_eq!(app.source, PROBE_SOURCE, "the scanned source must be set");
    assert!(matches!(app.page, Page::Titles));
    assert_eq!(app.tree.title_count(), 1);
    assert!(
        !fx.contains(&Effect::StopTicking),
        "the open disc keeps the tick for its watch: {fx:?}"
    );
}

/// The user did not wait. A probe landing after they opened something
/// else must not replace the tree under them.
#[test]
fn a_late_probe_result_does_not_clobber_what_the_user_opened() {
    let mut app = App::new();
    app.source = "iso:///the/one/they/chose.iso".to_string();
    app.page = Page::Titles;
    app.probe = Some(Arc::new(probe_state(Some(Ok(probe_scan())), true)));
    app.tick();
    assert_eq!(
        app.source, "iso:///the/one/they/chose.iso",
        "the probe overwrote the source the user chose"
    );
    assert_eq!(
        app.tree.title_count(),
        0,
        "the probe replaced the user's tree"
    );
}

/// A worker that panicked sets `done` with no result. The probe is
/// optional: drop it silently rather than panicking the UI thread.
#[test]
fn a_probe_that_left_no_result_is_dropped_silently() {
    let mut app = App::new();
    let before = app.log.len();
    app.probe = Some(Arc::new(probe_state(None, true)));
    app.tick();
    assert!(app.probe.is_none());
    assert_eq!(app.log.len(), before, "a dead probe must say nothing");
}

#[test]
fn explicit_async_open_reports_failure_on_tick() {
    let mut app = App::new();
    let before = app.log.len();
    let fx = app.open_async("/nonexistent/freemkv-async-open-test.iso");
    assert!(fx.contains(&Effect::StartTicking));
    assert!(app.opening());
    assert_eq!(app.log.len(), before);
    for _ in 0..500 {
        app.tick();
        if !app.opening() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(!app.opening());
    assert!(app.log.len() > before);
    assert!(matches!(app.page, Page::Empty));
}

#[test]
fn pending_open_keeps_ticks_and_blocks_ripping_until_result_is_applied() {
    let mut app = App::new();
    app.source = "previous.iso".into();
    let (tx, rx) = std::sync::mpsc::channel();
    app.opening = Some(rx);
    assert!(!app.view().can_run);
    assert!(app.dispatch(Cmd::Run).is_empty());
    assert!(!app.tick().contains(&Effect::StopTicking));
    tx.send(OpenedSource {
        path: "scanned.iso".into(),
        scanned: Ok(probe_scan()),
        preflight: Some(Ok(vec![])),
    })
    .unwrap();
    let fx = app.tick();
    assert!(fx.contains(&Effect::StopTicking));
    assert_eq!(app.source, "scanned.iso");
    assert_eq!(app.tree.title_count(), 1);
    assert!(app.view().can_run);
    assert!(
        app.log
            .iter()
            .any(|l| l.text == crate::strings::get("gui.log.ready_rip"))
    );
}

#[test]
fn another_open_or_launch_probe_cannot_replace_an_explicit_scan() {
    let mut app = App::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.opening = Some(rx);
    assert!(app.open_async("disc://").is_empty());
    assert_eq!(app.open_probe("disc://"), vec![Effect::Redraw]);
    assert!(app.probe.is_none());
    tx.send(OpenedSource {
        path: "first.mkv".into(),
        scanned: Ok(probe_scan()),
        preflight: None,
    })
    .unwrap();
    app.tick();
    assert_eq!(app.source, "first.mkv");
}

#[test]
fn closing_during_open_discards_the_late_result() {
    let mut app = App::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.opening = Some(rx);
    app.dispatch(Cmd::Close);
    assert!(!app.opening());
    assert!(
        tx.send(OpenedSource {
            path: "disc://".into(),
            scanned: Ok(probe_scan()),
            preflight: None,
        })
        .is_err()
    );
    app.tick();
    assert!(app.source.is_empty());
    assert!(matches!(app.page, Page::Empty));
}

#[test]
fn failed_scan_worker_releases_ui_and_reports_error() {
    let mut app = App::new();
    let (tx, rx) = std::sync::mpsc::channel();
    app.opening = Some(rx);
    let before = app.log.len();
    drop(tx);
    assert!(app.tick().contains(&Effect::StopTicking));
    assert!(!app.opening());
    assert_eq!(app.log.len(), before + 1);
}

/// The unit-test scan: a live drive is refused without being touched, so no test
/// depends on (or waits on) whatever hardware the machine running it has.
pub(super) fn no_drive_scan(
    path: &str,
    keys: &KeyConfig,
    tok: &OpenToken,
) -> Result<Scanned, String> {
    if crate::engine::is_disc_source(path) {
        return Err(NO_DRIVE_IN_TESTS.to_string());
    }
    scan_source(path, keys, tok)
}
pub(super) fn no_drive_probe(
    path: &str,
    keys: &KeyConfig,
    tok: &OpenToken,
) -> Result<Scanned, String> {
    if crate::engine::is_disc_source(path) {
        return Err(NO_DRIVE_IN_TESTS.to_string());
    }
    probe_source(path, keys, tok)
}
const NO_DRIVE_IN_TESTS: &str = "unit tests never open a drive";
pub(super) fn no_drive_presence(_: &str) -> Option<bool> {
    None
}

#[test]
fn unit_tests_never_reach_a_real_drive() {
    let mut app = App::new();
    let before = app.log.len();
    app.open(PROBE_SOURCE);
    assert_eq!(
        app.log.get(before).map(|l| l.text.as_str()),
        Some(NO_DRIVE_IN_TESTS)
    );
    app.open_probe(PROBE_SOURCE);
    drain_probe(&mut app);
    assert_eq!(app.log.len(), before + 1, "the probe stays silent");
}

// ── An explicit open while the launch probe holds the drive ─────────────

fn probe_state(result: Option<Result<Scanned, String>>, done: bool) -> ProbeState {
    let mut p = ProbeState::new(PROBE_SOURCE, PROBE_GRACE);
    p.result = Mutex::new(result);
    p.done = AtomicBool::new(done);
    p
}

// ── Stop design v5 §4.3: the open token and the launch probe (T29) ──────────
// T29 scaled: a 100 ms idle window, so each case runs in well under a second.
const T29: std::time::Duration = std::time::Duration::from_millis(100);

// Wait up to `cap` for `tok`'s Stop; `true` if it came.
fn stopped_within(tok: &OpenToken, cap: std::time::Duration) -> bool {
    let until = std::time::Instant::now() + cap;
    loop {
        if tok.halt.is_cancelled() {
            return true;
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

// Tick until the probe is collected and no open is outstanding; the elapsed time.
fn settle(app: &mut App) -> std::time::Duration {
    let started = std::time::Instant::now();
    while app.probe.is_some() || app.opening() {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "never settled"
        );
        app.tick();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    started.elapsed()
}

// Released by the test once its probe has progressed for four windows.
static PROGRESSING_DONE: AtomicBool = AtomicBool::new(false);

fn progressing_probe(_: &str, _: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    while !PROGRESSING_DONE.load(Ordering::Acquire) && !tok.halt.is_cancelled() {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    match tok.halt.is_cancelled() {
        true => Err("stopped".to_string()),
        false => Ok(probe_scan()),
    }
}

// FT15a (T29): "the probe's `Progress` moves every 0.5 × window for 4 windows → the
// result is adopted". Per spec; do not change without a spec citation. The test moves
// the progress before each tick, so a slow runner's late wake-up never reads as idle.
#[test]
fn probe_progressing_past_30s_is_kept() {
    let mut app = App::new();
    (app.probe_scan, app.probe_window) = (progressing_probe, T29);
    app.open_probe(PROBE_SOURCE);
    let started = std::time::Instant::now();
    while started.elapsed() < T29 * 4 {
        let probe = app.probe.clone().expect("the probe is still running");
        probe.token.progress.bump();
        app.tick();
        assert!(
            !probe.token.halt.is_cancelled(),
            "T29 left a progressing probe alone"
        );
        std::thread::sleep(T29 / 2);
    }
    PROGRESSING_DONE.store(true, Ordering::Release);
    // Collected only once done: a tick before then would count the wake-up as idle.
    let probe = app.probe.clone().expect("the probe is still running");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !probe.done.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < until,
            "the probe never finished"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    settle(&mut app);
    assert_eq!(
        app.source, PROBE_SOURCE,
        "a progressing probe's result is kept"
    );
    assert!(matches!(app.page, Page::Titles));
}

// One READ in recovery: `busy()` held for 3 windows with no CDB completing.
fn recovering_probe(_: &str, _: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    let _read = tok.progress.busy();
    std::thread::sleep(T29 * 3);
    match tok.halt.is_cancelled() {
        true => Err("stopped".to_string()),
        false => Ok(probe_scan()),
    }
}

// FT15e (T29): "one READ `Stall`ed for 60 s (scaled; `busy()` held) with no other
// progress → the probe survives and completes". Per spec.
#[test]
fn probe_not_cancelled_during_long_recovery_read() {
    let mut app = App::new();
    (app.probe_scan, app.probe_window) = (recovering_probe, T29);
    app.open_probe(PROBE_SOURCE);
    settle(&mut app);
    assert_eq!(app.source, PROBE_SOURCE, "the probe completed");
}

static FROZEN_SAW_STOP: AtomicBool = AtomicBool::new(false);
fn frozen_probe(_: &str, _: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    let stopped = stopped_within(tok, std::time::Duration::from_secs(3));
    FROZEN_SAW_STOP.store(stopped, Ordering::SeqCst);
    Err(String::new())
}

// FT15b (T29): "no progress → the open token is cancelled at window; the worker releases
// the drive; silent" (§5.0: "must fire within window + 1 s").
#[test]
fn probe_frozen_30s_is_cancelled_and_releases() {
    let mut app = App::new();
    (app.probe_scan, app.probe_window) = (frozen_probe, T29);
    let before = app.log.len();
    app.open_probe(PROBE_SOURCE);
    let took = settle(&mut app);
    assert!(
        FROZEN_SAW_STOP.load(Ordering::SeqCst),
        "the probe's token was never cancelled"
    );
    assert!(took <= T29 + std::time::Duration::from_secs(1), "{took:?}");
    assert_eq!(
        app.log.len(),
        before,
        "a probe nobody asked for stays silent"
    );
    assert!(app.source.is_empty() && matches!(app.page, Page::Empty));
}

static HOLD_RELEASED: Mutex<Option<std::time::Instant>> = Mutex::new(None);
fn holding_probe(_: &str, _: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    stopped_within(tok, std::time::Duration::from_secs(3));
    std::thread::sleep(std::time::Duration::from_millis(50));
    *HOLD_RELEASED.lock().unwrap() = Some(std::time::Instant::now());
    Err("stopped".to_string())
}
static OTHER_OPENED: Mutex<Option<std::time::Instant>> = Mutex::new(None);
fn other_open(_: &str, _: &KeyConfig, _: &OpenToken) -> Result<Scanned, String> {
    *OTHER_OPENED.lock().unwrap() = Some(std::time::Instant::now());
    Ok(probe_scan())
}

// FT15c (§4.3): an Open of another source "cancels the probe's open token and keeps the
// `PendingOpen` pending"; it runs "only once the probe worker reports `done`", and "The UI
// shows 'waiting for the drive' meanwhile". The Open no longer fails with "busy".
#[test]
fn open_during_probe_cancels_probe_then_opens() {
    const OTHER: &str = "disc:///dev/ft15c-other";
    let mut app = App::new();
    (app.probe_scan, app.probe_window, app.scan) = (holding_probe, PROBE_GRACE, other_open);
    app.open_probe(PROBE_SOURCE);
    let probe = app.probe.clone().expect("probe out");
    app.open(OTHER);
    assert!(
        stopped_within(&probe.token, std::time::Duration::ZERO),
        "probe not cancelled"
    );
    let waiting = crate::strings::get("stop.waiting_for_drive");
    assert!(
        app.log.iter().any(|l| l.text == waiting),
        "no 'waiting for the drive'"
    );
    settle(&mut app);
    let (held, opened) = (
        *HOLD_RELEASED.lock().unwrap(),
        *OTHER_OPENED.lock().unwrap(),
    );
    assert!(opened.expect("the Open ran") >= held.expect("the probe let go"));
    assert_eq!(app.source, OTHER, "the Open succeeded");
}

static IDLE_SAW_STOP: AtomicBool = AtomicBool::new(false);
fn idle_probe(_: &str, _: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    if stopped_within(tok, T29 * 4) {
        IDLE_SAW_STOP.store(true, Ordering::SeqCst);
        return Err("stopped".to_string());
    }
    Ok(probe_scan())
}

static FT15D_RESCANS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn counted_open(_: &str, _: &KeyConfig, _: &OpenToken) -> Result<Scanned, String> {
    FT15D_RESCANS.fetch_add(1, Ordering::SeqCst);
    Ok(probe_scan())
}

// FT15d (T29): "a same-source Open adopts a probe that then idles past the window
// (scaled) → not cancelled; the result is adopted; a source change still cancels it".
#[test]
fn adopted_probe_is_never_cancelled_by_t29() {
    let mut app = App::new();
    (app.probe_scan, app.probe_window, app.scan) = (idle_probe, T29, counted_open);
    app.open_probe(PROBE_SOURCE);
    app.open(PROBE_SOURCE);
    settle(&mut app);
    assert!(
        !IDLE_SAW_STOP.load(Ordering::SeqCst),
        "T29 cancelled an adopted probe"
    );
    assert_eq!(
        FT15D_RESCANS.load(Ordering::SeqCst),
        0,
        "not adopted: scanned again"
    );
    assert_eq!(app.source, PROBE_SOURCE, "the probe's result is adopted");

    let mut app = App::new();
    (app.probe_scan, app.probe_window, app.scan) = (idle_probe, T29, other_open);
    app.open_probe(PROBE_SOURCE);
    let probe = app.probe.clone().expect("probe out");
    app.open(PROBE_SOURCE);
    app.open("disc:///dev/ft15d-other");
    assert!(
        stopped_within(&probe.token, std::time::Duration::ZERO),
        "source change"
    );
    settle(&mut app);
}

static FT14_HELD: Mutex<Vec<OpenToken>> = Mutex::new(Vec::new());
fn held_open(_: &str, _: &KeyConfig, tok: &OpenToken) -> Result<Scanned, String> {
    FT14_HELD.lock().unwrap().push(tok.clone());
    stopped_within(tok, std::time::Duration::from_secs(3));
    Err("stopped".to_string())
}

// FT14 (§4.3), App half: "the source changing (a new Open); the window closing; Quit"
// each cancel the in-flight open's token.
#[test]
fn gui_open_token_is_cancelled_by_a_new_open_close_and_quit() {
    for how in ["open", "close", "quit"] {
        FT14_HELD.lock().unwrap().clear();
        let mut app = App::new();
        app.scan = held_open;
        app.open_async("iso:///ft14/a.iso");
        let tok = loop {
            if let Some(t) = FT14_HELD.lock().unwrap().first().cloned() {
                break t;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        match how {
            "open" => drop(app.open("iso:///ft14/b.iso")),
            "close" => drop(app.dispatch(Cmd::Close)),
            _ => drop(app.dispatch(Cmd::Quit)),
        }
        assert!(
            stopped_within(&tok, std::time::Duration::from_secs(1)),
            "{how}"
        );
    }
}

// A probe the drive never answers, even after its Stop, is abandoned rather than waited
// on for the life of the process: the worker can't be killed (blocked in the driver),
// but the UI stops waiting for it. Per spec (§4.3: "The UI thread never blocks").
#[test]
fn a_probe_that_never_answers_is_abandoned_instead_of_ticking_forever() {
    let mut app = App::new();
    app.probe_window = T29;
    let probe = Arc::new(ProbeState::new(PROBE_SOURCE, T29));
    app.probe = Some(probe.clone());
    let started = std::time::Instant::now();
    let mut fx = Vec::new();
    while app.probe.is_some() && started.elapsed() < std::time::Duration::from_secs(2) {
        fx = app.tick();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        app.probe.is_none(),
        "still waiting on a probe that never answers"
    );
    assert!(
        probe.token.halt.is_cancelled(),
        "abandoned only after its Stop"
    );
    assert!(fx.contains(&Effect::StopTicking), "{fx:?}");
    assert!(app.source.is_empty() && matches!(app.page, Page::Empty));
}

static FRESH_OPENS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn fresh_open(_: &str, _: &KeyConfig, _: &OpenToken) -> Result<Scanned, String> {
    FRESH_OPENS.fetch_add(1, Ordering::SeqCst);
    Ok(probe_scan())
}

// §4.3: "A pending Open for the **same** source still adopts the probe's result", but a
// probe its Stop ended has no result: the Open runs afresh, with no stop error shown.
#[test]
fn a_cancelled_probe_that_failed_is_not_adopted() {
    let mut app = App::new();
    app.scan = fresh_open;
    let probe = Arc::new(probe_state(None, false));
    probe.token.halt.cancel();
    app.probe = Some(probe.clone());
    app.open(PROBE_SOURCE);
    let before = app.log.len();
    finish_probe(&probe, Err("E9001 stopped".to_string()));
    settle(&mut app);
    assert_eq!(FRESH_OPENS.load(Ordering::SeqCst), 1, "the Open ran afresh");
    assert_eq!(app.source, PROBE_SOURCE);
    assert!(
        !app.log[before..].iter().any(|l| l.text.contains("stopped")),
        "a spurious stop error"
    );
}

fn in_flight_probe(app: &mut App) -> Arc<ProbeState> {
    let p = Arc::new(probe_state(None, false));
    app.probe = Some(p.clone());
    p
}

fn finish_probe(p: &ProbeState, r: Result<Scanned, String>) {
    *p.result.lock().unwrap() = Some(r);
    p.done.store(true, Ordering::Release);
}

fn tick_until_settled(app: &mut App) {
    for _ in 0..500 {
        app.tick();
        if app.probe.is_none() && !app.opening() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the open never settled");
}

static ADOPT_SCANS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn busy_drive_adopt(_: &str, _: &KeyConfig, _: &OpenToken) -> Result<Scanned, String> {
    ADOPT_SCANS.fetch_add(1, Ordering::SeqCst);
    Err("drive busy".to_string())
}

#[test]
fn a_prompted_open_of_the_probed_drive_adopts_the_probe() {
    let mut app = App::new();
    app.scan = busy_drive_adopt;
    let p = in_flight_probe(&mut app);
    let before = app.log.len();
    app.open(PROBE_SOURCE);
    assert_eq!(
        ADOPT_SCANS.load(Ordering::SeqCst),
        0,
        "a second scan raced the probe for the drive"
    );
    assert_eq!(app.log.len(), before, "{:?}", &app.log[before..]);
    assert!(
        app.opening(),
        "the open must stay pending until the probe lands"
    );
    finish_probe(&p, Ok(probe_scan()));
    app.tick();
    assert_eq!(app.source, PROBE_SOURCE);
    assert!(matches!(app.page, Page::Titles));
    assert!(!app.opening());
}

static ADOPT_ASYNC_SCANS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
fn busy_drive_adopt_async(_: &str, _: &KeyConfig, _: &OpenToken) -> Result<Scanned, String> {
    ADOPT_ASYNC_SCANS.fetch_add(1, Ordering::SeqCst);
    Err("drive busy".to_string())
}

#[test]
fn a_background_open_of_the_probed_drive_adopts_the_probe() {
    let mut app = App::new();
    app.scan = busy_drive_adopt_async;
    let p = in_flight_probe(&mut app);
    app.open_async(PROBE_SOURCE);
    finish_probe(&p, Ok(probe_scan()));
    tick_until_settled(&mut app);
    assert_eq!(ADOPT_ASYNC_SCANS.load(Ordering::SeqCst), 0);
    assert_eq!(app.source, PROBE_SOURCE);
    assert!(matches!(app.page, Page::Titles));
}

/// Adopted, the probe's failure is the answer to a question the user asked.
fn drive_busy(_: &str, _: &KeyConfig, _: &OpenToken) -> Result<Scanned, String> {
    Err("drive busy".to_string())
}

#[test]
fn an_adopted_probe_failure_is_reported() {
    let mut app = App::new();
    app.scan = drive_busy;
    let p = in_flight_probe(&mut app);
    app.open(PROBE_SOURCE);
    finish_probe(&p, Err("no disc in the drive".to_string()));
    app.tick();
    assert!(
        app.log.iter().any(|l| l.text == "no disc in the drive"),
        "the user asked and was told nothing"
    );
    assert!(matches!(app.page, Page::Empty));

    let mut app = App::new();
    app.scan = drive_busy;
    let p = in_flight_probe(&mut app);
    app.open(PROBE_SOURCE);
    finish_probe(&p, Err(String::new()));
    app.tick();
    let last = app.log.last().map(|l| l.text.clone()).unwrap_or_default();
    assert!(
        !last.is_empty(),
        "an empty probe error must not become a blank line"
    );
}

static QUEUED_SCANS: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn record_queued(path: &str, _: &KeyConfig, _: &OpenToken) -> Result<Scanned, String> {
    QUEUED_SCANS.lock().unwrap().push(path.to_string());
    Ok(probe_scan())
}

/// Another drive URL may still be the drive the probe holds: it waits its turn.
#[test]
fn an_open_of_another_drive_url_waits_for_the_probe() {
    const OTHER: &str = "disc:///dev/the-other-one";
    let mut app = App::new();
    app.scan = record_queued;
    let p = in_flight_probe(&mut app);
    app.open(OTHER);
    assert!(
        QUEUED_SCANS.lock().unwrap().is_empty(),
        "scanned while the probe held the drive"
    );
    assert!(app.opening());
    let mut late = probe_scan();
    late.title_count = 7;
    finish_probe(&p, Ok(late));
    app.tick();
    assert_eq!(*QUEUED_SCANS.lock().unwrap(), vec![OTHER.to_string()]);
    assert_eq!(app.source, OTHER, "the user's open must win over the probe");
}

/// A file needs no drive: it is opened at once, as before.
#[test]
fn a_file_open_does_not_wait_for_the_probe() {
    let mut app = App::new();
    let _p = in_flight_probe(&mut app);
    let before = app.log.len();
    app.open("iso:///nonexistent/freemkv-probe-wait-test.iso");
    assert!(app.log.len() > before, "the failure must be reported now");
    assert!(app.probe.is_none() && !app.opening());
}

// ── A finished run's notification ───────────────────────────────────────

fn tick_a_run_that_ended(outcome: crate::engine::RunOutcome) -> Vec<Effect> {
    let mut app = App::new();
    app.settings.notify_when_rip_finished = true;
    app.run_dest = "/out".to_string();
    // Repointed mid-rip (the preferences window can): not where this run wrote.
    app.output_dir = "/elsewhere".to_string();
    let st = Arc::new(RunState::default());
    *st.summary.lock().unwrap() = "summary".to_string();
    *st.outcome.lock().unwrap() = outcome;
    st.finished.store(true, Ordering::Release);
    app.run = Some(st);
    app.tick()
}

fn notification(fx: &[Effect]) -> Option<(String, Option<String>)> {
    fx.iter().find_map(|e| match e {
        Effect::NotifyRipFinished {
            title, output_dir, ..
        } => Some((title.clone(), output_dir.clone())),
        _ => None,
    })
}

#[test]
fn a_cancelled_or_failed_rip_is_not_announced_as_finished() {
    use crate::engine::RunOutcome::{Cancelled, Failed};
    for (outcome, key) in [
        (Cancelled, "gui.result.cancelled"),
        (Failed, "gui.result.nothing"),
    ] {
        let fx = tick_a_run_that_ended(outcome);
        assert_eq!(
            notification(&fx),
            Some((crate::strings::get(key), None)),
            "{outcome:?}: titled by its verdict, and nothing to reveal"
        );
    }
}

#[test]
fn a_completed_rip_reveals_the_folder_it_wrote_to() {
    let fx = tick_a_run_that_ended(crate::engine::RunOutcome::Completed);
    assert_eq!(
        notification(&fx).and_then(|(_, dir)| dir).as_deref(),
        Some("/out"),
        "not the output setting as it reads now"
    );
}

// ── Tree / language edge cases ──────────────────────────────────────────

fn node(type_s: &str, pid: Option<u16>, title_idx: usize) -> Node {
    Node {
        type_s: type_s.to_string(),
        desc: String::new(),
        checkable: pid.is_some(),
        checked: RefCell::new(true),
        children: Vec::new(),
        info: String::new(),
        pid,
        title_idx,
        mirror: None,
        length: String::new(),
        size: String::new(),
        lang: String::new(),
        item: String::new(),
        format: String::new(),
        notes: String::new(),
    }
}

/// The header row's `usize::MAX` sentinel must never become a title to rip.
#[test]
fn a_header_row_with_a_pid_is_not_a_phantom_title() {
    let tree = Tree {
        arena: vec![
            node("Audio", Some(0x1100), usize::MAX),
            node("Audio", Some(0x1101), 0),
        ],
        roots: vec![0, 1],
    };
    let TitleStreams::PerTitle(per) = tree.ticked_streams_by_title() else {
        panic!("expected a per-title breakdown");
    };
    assert_eq!(per, vec![(0, vec![0x1101], vec![])]);
}

/// English names outside the picker list resolve regardless of case.
#[test]
fn a_language_name_outside_the_picker_matches_in_any_case() {
    for tag in ["welsh", "WELSH", "Welsh"] {
        assert_eq!(canonical_lang_code(tag).as_deref(), Some("cym"), "{tag}");
    }
    assert_eq!(canonical_lang_code("basque").as_deref(), Some("eus"));
}

/// A minimal scan result: one title, one row, enough for the tree to
/// count it.
fn probe_scan() -> Scanned {
    Scanned {
        label: "PROBE_DISC".to_string(),
        volume_id: "PROBE_DISC".to_string(),
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
            role: None,
        }],
        key_summary: "none".to_string(),
        title_count: 1,
        video_codecs: vec!["HEVC".to_string()],
        title_sizes: Vec::new(),
        capacity_bytes: 0,
        title_ids: Vec::new(),
        details: Vec::new(),
        keys: None,
        needs_disc: false,
        refusal: None,
    }
}

/// A drive's Source size is what the scan says the rip reads: the ticked titles' total,
/// or the disc's capacity for a whole-disc output.
#[test]
fn a_drive_source_sizes_from_the_scan() {
    let sizes = [4_000_000_000, 0, 1_500_000_000];
    let iso = "Whole disc → ISO image";
    let mkv = "Selected titles → MKV";
    assert_eq!(
        scanned_source_bytes(mkv, &[0, 2], &sizes, 8_500_000_000),
        Some(5_500_000_000)
    );
    assert_eq!(
        scanned_source_bytes(iso, &[0], &sizes, 8_500_000_000),
        Some(8_500_000_000)
    );
    assert_eq!(
        scanned_source_bytes("Whole disc → decrypted folder", &[], &sizes, 8_500_000_000),
        Some(8_500_000_000)
    );
    // Unknown is never a guess: an unsized title, a title the scan does not
    // list, nothing ticked, or no capacity.
    assert_eq!(scanned_source_bytes(mkv, &[0, 1], &sizes, 1), None);
    assert_eq!(scanned_source_bytes(mkv, &[3], &sizes, 1), None);
    assert_eq!(scanned_source_bytes(mkv, &[], &sizes, 1), None);
    assert_eq!(scanned_source_bytes(iso, &[0], &sizes, 0), None);
}

/// The row shows a regular file's own length, else the scan's figure, else an em dash;
/// a folder's directory-entry size is never shown.
#[test]
fn the_source_size_row_uses_the_file_else_the_scan() {
    let dir = crate::ku_fixtures::TempDir::new("info-source-size");
    let iso = dir.path().join("Disc.iso");
    std::fs::write(&iso, [0u8; 2048]).unwrap();
    let iso = iso.to_str().unwrap();
    let folder = dir.path().to_str().unwrap();
    let row = |src: &str, scanned| InfoRows::starting(src, "/out/x.mkv", scanned);
    assert_eq!(
        row("disc://", Some(5 << 30)).source_size,
        fmt_bytes(5 << 30)
    );
    assert_eq!(row("disc://", None).source_size, "—");
    assert_eq!(row(folder, Some(3 << 30)).source_size, fmt_bytes(3 << 30));
    assert_eq!(row(folder, None).source_size, "—");
    assert_eq!(row(iso, Some(9 << 30)).source_size, fmt_bytes(2048));
}

/// Open keeps the scan's sizes for the Information panel, and a later
/// container open clears them.
#[test]
fn open_keeps_the_scans_title_sizes_and_capacity() {
    let mut app = App::new();
    let mut sc = probe_scan();
    sc.title_sizes = vec![7_000_000_000];
    sc.capacity_bytes = 8_500_000_000;
    app.apply_scan("disc:///dev/sr0", Ok(sc), true);
    assert_eq!(app.title_sizes, vec![7_000_000_000]);
    assert_eq!(app.capacity_bytes, 8_500_000_000);
    assert_eq!(
        scanned_source_bytes(
            &app.effective_format(),
            &app.tree.ticked_titles(),
            &app.title_sizes,
            app.capacity_bytes,
        )
        .map(fmt_bytes),
        Some(fmt_bytes(7_000_000_000))
    );
    app.apply_scan("/media/clip.mkv", Ok(probe_scan()), true);
    assert!(app.title_sizes.is_empty());
    assert_eq!(app.capacity_bytes, 0);
}

// A source pin: Start builds the panel from the scan's sizes for the ticked titles.
#[test]
fn start_sizes_the_source_row_from_the_scan() {
    let src = include_str!("ui.rs").replace("\r\n", "\n");
    let start = src
        .find("\n    fn start_run(&mut self)")
        .expect("start_run definition present");
    let body = &src[start..start + src[start..].find("\n    }\n").unwrap()];
    assert!(body.contains("scanned_source_bytes(\n            &self.effective_format(),\n            &titles,\n            &self.title_sizes,\n            self.capacity_bytes,\n        );"));
    assert!(body.contains("InfoRows::starting(&self.source, &out_file, scanned)"));
}

/// FK11, GUI half (KU §4.2 “GUI (image source or staged ISO) | An "Insert the disc"
/// prompt … and Retry”): an Open that needs the disc says so and arms the Retry, so
/// the next Start scans a drive instead of asking the key service again without it.
#[test]
fn an_open_that_needs_the_disc_prompts_and_arms_the_retry() {
    let mut app = App::new();
    let mut sc = probe_scan();
    sc.needs_disc = true;
    app.apply_scan("/media/capture.iso", Ok(sc), false);
    assert!(app.vid_retry, "the next Start is the Retry");
    let said = app
        .log
        .iter()
        .map(|l| l.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(said.contains(&crate::engine::insert_disc_retry()), "{said}");
    app.apply_scan("/media/other.iso", Ok(probe_scan()), false);
    assert!(!app.vid_retry, "a new source starts afresh");
}

/// B2 (KU-F1 review; KU J15, "the key service is never called twice"): Open's
/// answered refusal is shown again at Start with no request, unless the key settings
/// changed, the keydb was updated, Start wants more than Open resolved, or Open's
/// failure was transport-class.
#[test]
fn an_answered_refusal_at_open_is_shown_again_not_asked_again() {
    let out = std::env::temp_dir().join(format!("fmkv-b2-{}", std::process::id()));
    let refused = |transport: bool, titles: Vec<usize>| {
        let mut app = App::new();
        app.output_dir = out.display().to_string();
        let mut sc = probe_scan();
        // A real Open logs the refusal among its details (`scanned_with_keys`).
        sc.details = vec!["E7022 no key for this disc".into()];
        sc.refusal = Some(crate::engine::KeyRefusal {
            code: 7022,
            text: "E7022 no key for this disc".into(),
            titles,
            transport,
        });
        app.apply_scan("/nonexistent/b2.iso", Ok(sc), false);
        app
    };
    let mut app = refused(false, vec![0]);
    app.start_run();
    assert!(app.run.is_none(), "no second ask of an answered refusal");
    let said = app
        .log
        .iter()
        .filter(|l| l.text.contains("no key for this disc"))
        .count();
    assert_eq!(said, 2, "Open said it, and Start says it again");
    app.settings.keyserver_url = "https://keys.test/decode".into();
    app.start_run();
    assert!(app.run.is_some(), "a key-settings change asks again");

    let mut app = refused(true, vec![0]);
    app.start_run();
    assert!(app.run.is_some(), "a transport-class failure asks again");

    let mut app = refused(false, vec![]);
    app.start_run();
    assert!(
        app.run.is_some(),
        "Start wants a title Open did not resolve"
    );
}

fn slow_update(_current: &str) -> String {
    for _ in 0..500 {
        if UPDATE_GATE.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    "stub verdict".into()
}

static UPDATE_GATE: AtomicBool = AtomicBool::new(false);

// The update check blocks on the network: the UI thread must hand it off and return.
#[test]
fn check_for_updates_does_not_block_the_ui_thread() {
    let mut app = App::new();
    app.update_fn = slow_update;
    let fx = app.dispatch(Cmd::CheckUpdates);
    assert!(fx.contains(&Effect::StartTicking), "{fx:?}");
    assert!(
        !app.log.iter().any(|l| l.text.contains("stub verdict")),
        "dispatch waited for the check"
    );
    UPDATE_GATE.store(true, Ordering::SeqCst);
    for _ in 0..500 {
        app.tick();
        if app.log.iter().any(|l| l.text.contains("stub verdict")) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("the verdict never reached the log");
}

// The macOS drop filter: folders and containers open, and so does an `.iso` in any case.
#[test]
fn is_openable_file_takes_containers_and_iso_in_any_case() {
    assert!(is_openable_file("/m/Movie.iso"));
    assert!(is_openable_file("/m/Movie.ISO"));
    assert!(is_openable_file("/m/Movie.mkv"));
    assert!(!is_openable_file("/m/Movie.txt"));
    assert!(!is_openable_file("/m/Movie"));
}

// A typo in a number field must not start the rip under the engine's reading of 0
// ("abort on any loss"), against what the user meant.
#[test]
fn an_unparsable_loss_limit_refuses_to_start() {
    let mut app = app_with_titles(&["H.264"]);
    app.source = "/m/a.iso".into();
    app.output_dir = "/out".into();
    app.settings.abort_lost_secs = "60s".into();
    app.dispatch(Cmd::Run);
    assert!(app.run.is_none(), "the rip started anyway");
    assert!(app.log.iter().any(|l| l.text.contains("60s")));
}

// A file dropped on the window mid-rip must not replace the model the rip is running under.
#[test]
fn open_is_refused_while_a_rip_runs() {
    let mut app = App::new();
    app.source = "/m/a.iso".into();
    app.page = Page::Progress;
    app.run = Some(Arc::default());
    assert!(app.open("/m/b.iso").is_empty());
    assert_eq!(app.source, "/m/a.iso");
    assert_eq!(app.page, Page::Progress);
}

// A failed open shows the empty page, so there must be no source left to Start.
#[test]
fn a_failed_open_leaves_no_source_to_start() {
    let mut app = App::new();
    app.source = "/m/a.iso".into();
    app.tree = Tree::from_scan(&probe_scan(), "All titles", 0.0, &LangPrefs::default());
    app.apply_scan("/m/bad.iso", Err("unreadable".into()), false);
    assert_eq!(app.page, Page::Empty);
    assert!(app.source.is_empty());
    assert!(app.tree.arena.is_empty());
    assert!(!app.view().can_run);
}

fn app_with_titles(codecs: &[&str]) -> App {
    let mut sc = probe_scan();
    sc.rows = (0..codecs.len())
        .map(|t| {
            let mut r = sc.rows[0].clone();
            r.title = t;
            r
        })
        .collect();
    sc.title_count = codecs.len();
    sc.video_codecs = codecs.iter().map(|c| c.to_string()).collect();
    let mut app = App::new();
    app.video_codecs = sc.video_codecs.clone();
    app.tree = Tree::from_scan(&sc, "All titles", 0.0, &LangPrefs::default());
    app.format = "Selected titles → MP4".to_string();
    app
}

#[test]
fn mp4_mismatch_is_reported_when_mp4_is_really_the_format() {
    let app = app_with_titles(&["MPEG-2", "HEVC"]);
    assert!(app.effective_format().contains("MP4"));
    assert!(app.container_mismatch().is_some());
}

// A stale MP4 preference from an earlier source is not what this rip
// uses, so it must not raise an MP4 warning.
#[test]
fn a_stale_mp4_preference_raises_no_mismatch_when_mp4_is_not_offered() {
    let app = app_with_titles(&["MPEG-2"]);
    assert!(!app.effective_format().contains("MP4"));
    assert_eq!(app.container_mismatch(), None);
}

// Shells append the log incrementally keyed on `log_first`; if a trim or a
// clear failed to move it, they would append onto stale lines.
#[test]
fn log_first_moves_on_trim_and_clear_but_not_on_append() {
    let mut app = App::new();
    app.clear_log();
    let base = app.view().log_first;
    for i in 0..App::LOG_MAX {
        app.say(LogKind::Detail, &i.to_string());
    }
    assert_eq!(app.view().log_first, base, "appends alone keep the front");
    app.say(LogKind::Detail, "one more");
    let v = app.view();
    assert_eq!(v.log_first, base + App::LOG_TRIM as u64);
    assert_eq!(v.log[0].text, App::LOG_TRIM.to_string());
    let len = v.log.len() as u64;
    app.dispatch(Cmd::ClearLog);
    let v = app.view();
    assert!(v.log.is_empty());
    assert_eq!(v.log_first, base + App::LOG_TRIM as u64 + len);
}

fn tick_a_finished_run(notify: bool) -> Vec<Effect> {
    let mut app = App::new();
    app.settings.notify_when_rip_finished = notify;
    app.output_dir = "/out".to_string();
    app.run_dest = "/out".to_string();
    let st = Arc::new(RunState::default());
    *st.summary.lock().unwrap() = "3 titles written".to_string();
    st.finished
        .store(true, std::sync::atomic::Ordering::Release);
    app.run = Some(st);
    app.tick()
}

#[test]
fn a_finished_rip_notifies_when_the_setting_is_on() {
    let fx = tick_a_finished_run(true);
    let n: Vec<_> = fx
        .iter()
        .filter_map(|e| match e {
            Effect::NotifyRipFinished {
                body, output_dir, ..
            } => Some((body.as_str(), output_dir.as_deref())),
            _ => None,
        })
        .collect();
    assert_eq!(n, vec![("3 titles written", Some("/out"))]);
}

#[test]
fn a_finished_rip_stays_silent_when_the_setting_is_off() {
    let fx = tick_a_finished_run(false);
    assert!(
        fx.contains(&Effect::StopTicking),
        "the run must still finish"
    );
    assert!(
        !fx.iter()
            .any(|e| matches!(e, Effect::NotifyRipFinished { .. })),
        "got {fx:?}"
    );
}

// ── menu_layout tests ────────────────────────────────────────────────

#[test]
fn the_menu_layout_has_the_five_canonical_groups_in_display_order() {
    let groups = menu_layout(false);
    let ids: Vec<MenuGroupId> = groups.iter().map(|g| g.id).collect();
    // App group ships alongside the others; the Windows/Linux shells fold
    // it into File+Help when they build their native menu bar.
    assert_eq!(
        ids,
        vec![
            MenuGroupId::App,
            MenuGroupId::File,
            MenuGroupId::Edit,
            MenuGroupId::View,
            MenuGroupId::Help,
        ]
    );
    // Every group has at least one entry — an empty menu is always a bug.
    for group in &groups {
        assert!(
            !group.entries.is_empty(),
            "menu group {:?} is empty",
            group.id
        );
    }
}

#[test]
fn every_cmd_the_shells_dispatch_from_a_menu_is_reachable_from_the_layout() {
    // The set the current menu bar wires up. Adding a Cmd that a menu
    // fires means listing it here so the layout can be checked against
    // the shells' expectations.
    let expected: &[Cmd] = &[
        Cmd::Open,
        Cmd::Close,
        Cmd::SetOutput,
        Cmd::Run,
        Cmd::Eject,
        Cmd::SelectAll,
        Cmd::SelectNone,
        Cmd::Invert,
        Cmd::ToggleLog,
        Cmd::ClearLog,
        Cmd::Settings,
        Cmd::About,
        Cmd::Docs,
        Cmd::CheckUpdates,
        Cmd::Quit,
    ];

    let mut found: Vec<Cmd> = Vec::new();
    for group in menu_layout(false) {
        for entry in group.entries {
            if let MenuEntry::Item(mi) = entry
                && let MenuAction::Cmd(c) = mi.action
            {
                found.push(c);
            }
        }
    }
    for c in expected {
        assert!(
            found.iter().any(|f| f == c),
            "Cmd::{c:?} is menu-driven on at least one shell but not in menu_layout"
        );
    }
}

#[test]
fn the_log_toggle_label_follows_the_layout_parameter() {
    // The whole point of taking `log_hidden` on `menu_layout` is that
    // a shell rebuilds its menu after the log is toggled and picks up
    // the new label without an extra "sync the label" call.
    let with_log_shown = menu_layout(false);
    let with_log_hidden = menu_layout(true);
    let toggle_label = |groups: &[MenuGroup]| -> String {
        for g in groups {
            for e in &g.entries {
                if let MenuEntry::Item(mi) = e
                    && mi.action == MenuAction::Cmd(Cmd::ToggleLog)
                {
                    return mi.label.clone();
                }
            }
        }
        panic!("ToggleLog missing from menu_layout")
    };
    assert_eq!(toggle_label(&with_log_shown), log_menu_label(false));
    assert_eq!(toggle_label(&with_log_hidden), log_menu_label(true));
    assert_ne!(
        toggle_label(&with_log_shown),
        toggle_label(&with_log_hidden),
        "the log toggle must swap between Show/Hide as the state changes"
    );
}

#[test]
fn the_log_menu_label_follows_the_state_not_one_fixed_action() {
    // The bug this pins: both shells built "Show log" once, at menu-build
    // time, so the item still read "Show log" while the log was on screen.
    assert_eq!(
        log_menu_label(true),
        crate::strings::get("gui.menu.show_log")
    );
    assert_eq!(
        log_menu_label(false),
        crate::strings::get("gui.menu.hide_log")
    );
    assert_ne!(log_menu_label(true), log_menu_label(false));
}

#[test]
fn toggling_the_log_flips_the_menu_label_in_the_view() {
    let mut a = App::new();
    // The log starts visible, so the item offers to hide it.
    assert!(!a.view().log_hidden);
    assert_eq!(a.view().log_menu_label, log_menu_label(false));
    a.dispatch(Cmd::ToggleLog);
    assert!(a.view().log_hidden);
    assert_eq!(a.view().log_menu_label, log_menu_label(true));
}

#[test]
fn an_empty_scan_yields_an_empty_tree() {
    // No source means no rows — the shell shows its empty page rather
    // than a placeholder disc.
    let t = Tree::from_scan(
        &crate::engine::Scanned {
            label: String::new(),
            volume_id: String::new(),
            title_count: 0,
            key_summary: String::new(),
            video_codecs: vec![],
            title_sizes: Vec::new(),
            capacity_bytes: 0,
            title_ids: vec![],
            rows: vec![],
            details: vec![],
            keys: None,
            needs_disc: false,
            refusal: None,
        },
        "Main film only",
        0.0,
        &LangPrefs::default(),
    );
    assert!(t.roots.is_empty());
    assert!(t.arena.is_empty());
}

// Every string the picker can offer must have a gui.format.* key present in en.json:
// format_label's catch-all otherwise renders correctly under en while staying
// untranslatable elsewhere.
#[test]
fn every_offered_format_has_a_translation_key_present_in_english() {
    let en: serde_json::Value =
        serde_json::from_str(freemkv_i18n::bundled_locale_json("en").expect("en is bundled"))
            .expect("en.json parses");

    // Both source kinds, both MP4 states — the union is every string the
    // picker can ever show.
    let mut offered: Vec<&str> = [(true, true), (true, false), (false, true), (false, false)]
        .into_iter()
        .flat_map(|(disc, mp4)| output_formats(disc, mp4).into_iter().flatten())
        .collect();
    offered.sort_unstable();
    offered.dedup();
    assert!(!offered.is_empty(), "the picker offers nothing");

    for canon in offered {
        let key = format_key(canon).unwrap_or_else(|| {
            panic!("{canon:?} has no gui.format key — it can never be translated")
        });

        // The key must resolve in en.json, not merely exist in the match.
        let mut node = &en;
        for part in key.split('.') {
            node = node
                .get(part)
                .unwrap_or_else(|| panic!("{key} ({canon:?}) is missing from en.json"));
        }
        let text = node
            .as_str()
            .unwrap_or_else(|| panic!("{key} is not a string in en.json"));

        // English is the canonical text by definition: if they diverge, the
        // engine matches one string while the user picked another.
        assert_eq!(
            text, canon,
            "{key} in en.json does not match the canonical picker string"
        );
    }
}

// A canonical format's localized label must resolve back to that exact canonical string, or
// a one-way label is a setting that silently reverts. REGRESSION PIN.
#[test]
fn every_offered_format_round_trips_through_its_label() {
    for (disc, mp4) in [(true, true), (true, false), (false, true), (false, false)] {
        for canon in output_formats(disc, mp4).into_iter().flatten() {
            let label = format_label(canon);
            assert_eq!(
                format_from_label(&label, disc, mp4),
                Some(canon),
                "label {label:?} does not resolve back to {canon:?}"
            );
        }
    }
}

// ── Eject (C12): used to be a stub that always said "the source is a file",
// even with a disc open. The SCSI work is behind `eject_fn`; no drive is touched.

use std::sync::atomic::AtomicUsize;

static EJECT_CALLS: AtomicUsize = AtomicUsize::new(0);

fn fake_eject_ok(_source: &str) -> Result<String, String> {
    EJECT_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok("/dev/fake0".into())
}

fn fake_eject_err(_source: &str) -> Result<String, String> {
    Err("E1000: fake0".into())
}

fn drain_eject(app: &mut App) {
    for _ in 0..2_000 {
        app.tick();
        if app.ejecting.is_none() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("the eject worker never finished");
}

fn logged(app: &App, needle: &str) -> bool {
    app.log.iter().any(|l| l.text.contains(needle))
}

#[test]
fn eject_action_is_decided_by_source_kind() {
    assert_eq!(eject_action("disc://"), EjectAction::Eject);
    assert_eq!(eject_action("disc:///dev/disk4"), EjectAction::Eject);
    for file in [
        "",
        "/m/Movie.iso",
        "/m/BDMV_DIR",
        "mkv:///m/a.mkv",
        "iso:///m/a.iso",
    ] {
        assert_eq!(eject_action(file), EjectAction::NothingToEject, "{file}");
    }
}

#[test]
fn eject_on_a_disc_source_runs_off_thread_and_clears_the_stale_disc() {
    let mut app = App::new();
    app.eject_fn = fake_eject_ok;
    app.source = "disc://".into();
    app.page = Page::Titles;
    app.disc_label = "MOVIE".into();
    let before = EJECT_CALLS.load(Ordering::SeqCst);
    let fx = app.dispatch(Cmd::Eject);
    assert!(
        !logged(&app, &crate::strings::get("gui.log.nothing_eject")),
        "a disc source must not be reported as a file"
    );
    assert!(
        fx.contains(&Effect::StartTicking),
        "the result is collected on the tick: {fx:?}"
    );
    drain_eject(&mut app);
    assert!(
        EJECT_CALLS.load(Ordering::SeqCst) > before,
        "the eject never ran"
    );
    assert!(
        app.source.is_empty(),
        "the ejected disc's source must be closed"
    );
    assert_eq!(app.page, Page::Empty);
    assert!(app.disc_label.is_empty());
}

#[test]
fn a_failed_eject_is_reported_and_keeps_the_disc_open() {
    let mut app = App::new();
    app.eject_fn = fake_eject_err;
    app.source = "disc:///dev/fake0".into();
    app.page = Page::Titles;
    app.dispatch(Cmd::Eject);
    drain_eject(&mut app);
    assert!(
        logged(&app, "E1000: fake0"),
        "the failure must reach the log"
    );
    assert_eq!(app.source, "disc:///dev/fake0");
    assert_eq!(app.page, Page::Titles);
}

#[test]
fn eject_on_a_file_source_still_says_nothing_to_eject() {
    let mut app = App::new();
    app.eject_fn = fake_eject_err;
    app.source = "/m/Movie.iso".into();
    let fx = app.dispatch(Cmd::Eject);
    assert!(logged(&app, &crate::strings::get("gui.log.nothing_eject")));
    assert!(!fx.contains(&Effect::StartTicking));
    assert!(app.ejecting.is_none());
}

// Stop design v5 §3.2 (A): "the GUI updates on the next frame"; ST-I2's strings
// "stopping; … finishing" (§5.7). A Stop after every title is written ends Done.
// Per spec; do not change without a spec citation proving otherwise.
#[test]
fn a_stop_says_stopping_and_finishing_once_every_title_is_written() {
    let mut app = App::new();
    let st: Arc<RunState> = Arc::default();
    app.run = Some(st.clone());
    app.run_titles = 2;
    let (stopping, finishing) = (
        crate::strings::get("stop.stopping"),
        crate::strings::get("stop.finishing"),
    );
    assert_ne!(app.view().saving_current, stopping, "no Stop yet");
    app.dispatch(Cmd::Cancel);
    let v = app.view();
    assert_eq!(
        (v.saving_current, v.saving_overall),
        (stopping.clone(), stopping)
    );
    st.titles_done.store(2, Ordering::SeqCst);
    let v = app.view();
    assert_eq!(
        (v.saving_current, v.saving_overall),
        (finishing.clone(), finishing)
    );
}

// A run's progress as its worker reports it, with both bars sampled after every step.
struct Bars {
    app: App,
    st: Arc<RunState>,
    seen: Vec<(f64, f64)>,
    // The sample at which the last title's final byte was written.
    last_byte: Option<usize>,
}

impl Bars {
    fn new(sizes: &[u64]) -> Self {
        let mut app = App::new();
        let st: Arc<RunState> = Arc::default();
        app.run = Some(st.clone());
        app.run_titles = sizes.len();
        st.plan_titles(sizes.iter().copied().enumerate().collect());
        let mut b = Bars {
            app,
            st,
            seen: vec![],
            last_byte: None,
        };
        b.sample();
        b
    }

    fn sample(&mut self) -> (f64, f64) {
        let v = self.app.view();
        self.seen.push((v.bar_current, v.bar_overall));
        (v.bar_current, v.bar_overall)
    }

    fn tick(&mut self, pass: &'static str, done: u64, total: u64) -> (f64, f64) {
        self.st.progress(&freemkv_engine::Progress {
            pass: pass.into(),
            bytes_done: done,
            bytes_total: total,
            ..Default::default()
        });
        self.sample()
    }

    // One title written in four ticks, counted the way the worker counts it.
    fn title(&mut self, idx: usize, size: u64) -> (f64, f64) {
        self.st.title_start(idx);
        assert_eq!(self.sample().0, 0.0, "title {idx} starts its bar at 0");
        for q in 1..=4 {
            self.tick("mux", size * q / 4, size);
        }
        if idx + 1 == self.app.run_titles {
            self.last_byte = Some(self.seen.len() - 1);
        }
        self.st.titles_done.fetch_add(1, Ordering::SeqCst);
        self.sample();
        self.st.title_end(idx);
        self.sample()
    }

    // The bottom bar never decreases; the top one only at a title's start (checked there).
    fn assert_overall_monotonic(&self) {
        for w in self.seen.windows(2) {
            assert!(w[1].1 >= w[0].1, "overall went backwards: {:?}", self.seen);
        }
    }

    fn assert_full_only_at_end(&self) {
        assert_eq!(self.seen.last().unwrap().1, 100.0, "overall ends full");
        let before = &self.seen[..self.last_byte.expect("the last title ran")];
        assert!(
            before.iter().all(|s| s.1 < 100.0),
            "overall full before the last title ended: {:?}",
            self.seen
        );
    }
}

#[test]
fn two_titles_reset_the_top_bar_and_fill_the_bottom_bar_by_halves() {
    let mut b = Bars::new(&[1000, 1000]);
    assert_eq!(b.title(0, 1000), (100.0, 50.0));
    b.title(1, 1000);
    // Halfway through the second title: the top bar is its half, the bottom three quarters.
    assert!(b.seen.contains(&(50.0, 75.0)), "{:?}", b.seen);
    b.assert_overall_monotonic();
    b.assert_full_only_at_end();
}

#[test]
fn three_titles_never_run_either_bar_backwards_across_a_boundary() {
    let mut b = Bars::new(&[600, 600, 600]);
    let ends: Vec<f64> = (0..3).map(|i| b.title(i, 600).1).collect();
    for (e, want) in ends.iter().zip([100.0 / 3.0, 200.0 / 3.0, 100.0]) {
        assert!((e - want).abs() < 1e-9, "{ends:?}");
    }
    b.assert_overall_monotonic();
    b.assert_full_only_at_end();
    // Within a title the top bar only rises; it drops only at a start.
    let resets = b.seen.windows(2).filter(|w| w[1].0 < w[0].0).count();
    assert_eq!(
        resets, 2,
        "top bar resets once per later title: {:?}",
        b.seen
    );
}

#[test]
fn the_bottom_bar_weighs_titles_by_bytes_not_by_count() {
    let mut b = Bars::new(&[100, 300, 600]);
    assert_eq!(b.title(0, 100).1, 10.0);
    assert_eq!(b.title(1, 300).1, 40.0);
    assert_eq!(b.title(2, 600).1, 100.0);
    b.assert_overall_monotonic();
    b.assert_full_only_at_end();
}

#[test]
fn a_multipass_read_counts_toward_the_bottom_bar_and_never_rewinds_it() {
    let mut b = Bars::new(&[1000, 1000]);
    b.st.begin_read();
    b.sample();
    for q in 1..=4 {
        b.tick("sweep", 500 * q, 2000);
    }
    assert_eq!(b.sample(), (100.0, 50.0), "read done: half the run's bytes");
    // A patch pass is its own pass on the top bar; the read stays counted below.
    assert_eq!(b.tick("patch-scrape", 0, 64), (0.0, 50.0));
    assert_eq!(b.title(0, 1000).1, 75.0);
    assert_eq!(b.title(1, 1000).1, 100.0);
    b.assert_overall_monotonic();
    b.assert_full_only_at_end();
}

#[test]
fn an_unknown_title_size_shows_no_batch_percentage() {
    let mut b = Bars::new(&[0, 1000]);
    b.title(0, 0);
    b.title(1, 1000);
    assert!(b.seen.iter().all(|s| s.1 == 0.0), "{:?}", b.seen);
}

#[test]
fn a_single_title_shows_one_bar_with_the_bottom_mirroring_it() {
    let mut b = Bars::new(&[1000]);
    b.st.title_start(0);
    assert_eq!(b.tick("mux", 250, 1000), (25.0, 25.0));
    assert!(!b.app.view().show_overall_bar);
}

#[test]
fn the_eject_button_shows_for_an_idle_disc_source_only() {
    let mut app = App::new();
    app.source = "disc://".into();
    app.page = Page::Titles;
    assert!(app.view().eject_visible);
    app.run = Some(Arc::default());
    assert!(!app.view().eject_visible, "hidden while a rip runs");
    app.run = None;
    app.eject_fn = fake_eject_gated;
    app.dispatch(Cmd::Eject);
    assert!(
        !app.view().eject_visible,
        "hidden while an eject is in flight"
    );
    EJECT_GATE.store(true, Ordering::SeqCst);
    drain_eject(&mut app);
    app.source = "/m/Movie.iso".into();
    assert!(!app.view().eject_visible);
}

// Released by the tests that use it; every waiter ends up released, so
// sharing one gate across parallel tests can only shorten a wait.
static EJECT_GATE: AtomicBool = AtomicBool::new(false);

fn fake_eject_gated(_source: &str) -> Result<String, String> {
    while !EJECT_GATE.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    Ok("/dev/fake0".into())
}

#[test]
fn a_finished_eject_never_closes_a_source_opened_meanwhile() {
    let mut app = App::new();
    app.eject_fn = fake_eject_gated;
    app.source = "disc://".into();
    app.page = Page::Titles;
    app.dispatch(Cmd::Eject);
    // A drop-to-open during the eject is refused outright...
    app.open("/m/Other.iso");
    assert_eq!(app.source, "disc://", "open must wait for the eject");
    // ...and even if a source changes anyway, the verdict leaves it alone.
    app.source = "/m/Other.iso".into();
    EJECT_GATE.store(true, Ordering::SeqCst);
    drain_eject(&mut app);
    assert_eq!(app.source, "/m/Other.iso");
    assert_eq!(app.page, Page::Titles);
}

// ── The disc watch: a disc that left its drive never leaves its titles behind ──

fn disc_gone(_: &str) -> Option<bool> {
    Some(false)
}

fn disc_still_in(_: &str) -> Option<bool> {
    Some(true)
}

// An idle disc source with its title tree, as a finished rip leaves it.
fn idle_disc(page: Page, presence: fn(&str) -> Option<bool>) -> App {
    let mut app = app_with_titles(&["MPEG-2"]);
    app.source = "disc://".into();
    app.page = page;
    app.presence_fn = presence;
    app.presence_every = std::time::Duration::ZERO;
    app
}

fn tick_until_watched(app: &mut App) {
    for _ in 0..2_000 {
        app.tick();
        if app.presence.is_none() && app.presence_at.is_some() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    panic!("the presence check never finished");
}

#[test]
fn done_with_the_disc_gone_returns_to_the_start_screen() {
    let mut app = idle_disc(Page::Result, disc_gone);
    app.dismiss_result();
    assert_eq!(app.page, Page::Empty, "no titles of a disc that is gone");
    assert!(app.source.is_empty() && app.tree.arena.is_empty());
}

#[test]
fn done_with_the_disc_still_in_returns_to_its_titles() {
    let mut app = idle_disc(Page::Result, disc_still_in);
    app.dismiss_result();
    assert_eq!(app.page, Page::Titles);
    assert_eq!(app.source, "disc://");
}

#[test]
fn done_with_a_file_source_returns_to_its_titles() {
    let mut app = idle_disc(Page::Result, disc_gone);
    app.source = "/m/Movie.iso".into();
    app.dismiss_result();
    assert_eq!(app.page, Page::Titles);
    assert_eq!(app.source, "/m/Movie.iso");
}

#[test]
fn a_disc_removed_while_idle_on_its_titles_resets_to_the_start_screen() {
    let mut app = idle_disc(Page::Titles, disc_gone);
    tick_until_watched(&mut app);
    assert_eq!(app.page, Page::Empty);
    assert!(app.source.is_empty() && app.tree.arena.is_empty());
    assert!(
        app.tick().contains(&Effect::StopTicking),
        "nothing left to watch"
    );
}

#[test]
fn an_idle_disc_keeps_the_tick_for_its_watch() {
    let mut app = idle_disc(Page::Titles, disc_still_in);
    tick_until_watched(&mut app);
    let fx = app.tick();
    assert!(!fx.contains(&Effect::StopTicking), "{fx:?}");
    assert!(
        !fx.contains(&Effect::Redraw),
        "no redraw per idle tick: {fx:?}"
    );
    assert_eq!(app.page, Page::Titles);
}

#[test]
fn a_rip_that_ejected_its_disc_keeps_its_result_until_done() {
    let mut app = idle_disc(Page::Progress, disc_gone);
    let st = Arc::new(RunState::default());
    st.finished.store(true, Ordering::Release);
    app.run = Some(st);
    let fx = app.tick();
    assert!(
        !fx.contains(&Effect::StopTicking),
        "the watch needs the tick"
    );
    tick_until_watched(&mut app);
    assert_eq!(app.page, Page::Result, "the outcome stays on screen");
    assert!(
        app.tree.arena.is_empty(),
        "but the gone disc's titles do not"
    );
    app.dismiss_result();
    assert_eq!(app.page, Page::Empty);
}

static PRESENCE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn counted_gone(_: &str) -> Option<bool> {
    PRESENCE_CALLS.fetch_add(1, Ordering::SeqCst);
    Some(false)
}

#[test]
fn the_watch_never_touches_the_drive_during_a_rip() {
    let mut app = idle_disc(Page::Progress, counted_gone);
    app.run = Some(Arc::default());
    let before = PRESENCE_CALLS.load(Ordering::SeqCst);
    for _ in 0..5 {
        app.tick();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(app.presence.is_none(), "no check started");
    assert_eq!(PRESENCE_CALLS.load(Ordering::SeqCst), before);
    assert_eq!(app.page, Page::Progress);
    assert_eq!(app.source, "disc://");
}

#[test]
fn a_verdict_that_lands_during_a_rip_is_dropped() {
    let mut app = idle_disc(Page::Titles, disc_gone);
    let (tx, rx) = std::sync::mpsc::channel();
    app.presence = Some(("disc://".into(), rx));
    app.run = Some(Arc::default());
    app.page = Page::Progress;
    tx.send(Some(false)).unwrap();
    app.tick();
    assert_eq!(app.source, "disc://");
    assert_eq!(app.page, Page::Progress);
}

#[test]
fn an_app_eject_still_resets_and_a_late_verdict_spares_a_later_source() {
    let mut app = idle_disc(Page::Titles, disc_still_in);
    app.eject_fn = fake_eject_ok;
    app.dispatch(Cmd::Eject);
    drain_eject(&mut app);
    assert_eq!(app.page, Page::Empty);
    assert!(app.source.is_empty());
    // A check still out for the ejected disc lands after a file was opened.
    let mut app = idle_disc(Page::Titles, disc_gone);
    let (tx, rx) = std::sync::mpsc::channel();
    app.presence = Some(("disc://".into(), rx));
    app.source = "/m/Other.iso".into();
    tx.send(Some(false)).unwrap();
    app.tick();
    assert_eq!(app.source, "/m/Other.iso");
    assert_eq!(app.page, Page::Titles);
}

#[test]
fn an_unknown_answer_keeps_the_disc_open() {
    let mut app = idle_disc(Page::Titles, no_drive_presence);
    tick_until_watched(&mut app);
    assert_eq!(app.page, Page::Titles);
    assert_eq!(app.source, "disc://");
}
