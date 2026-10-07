use super::{
    DiscPlan, KeyConfig, OutKind, RipRequest, RunState, TitleIdentity, UiSink, damage_note,
    demux_needs_subdirs, disc_device, disc_raw_copy, fe, image_or_dir_scheme, image_title_run,
    is_disc_source, is_stream_source, iso_recovery_result, mux_opts, out_kind, recovery_plan,
    recovery_produced_no_data, recovery_raw, run_disc_scanning, run_stream,
    should_delete_staging_iso, source_scheme, staging_not_kept_note, stream_selection_for,
    title_options, title_plan, verify_selection_identity, verify_title_identity, whole_image_gate,
};
use std::sync::Arc;

// ── The recovery job's `raw` flag ── `multipass_rip` refuses a real
// sweep-plus-patch plan with `raw = false`; `ui::raw_applies` forces
// false for any title output, so SHIPPED DEFAULTS couldn't rip at all.

/// The exact combination a fresh install produces: rip mode "Multi-pass",
/// 5 passes, raw off, output "Selected titles → MKV". This is the test
/// that would have caught it.
#[test]
fn the_shipped_defaults_produce_a_recovery_the_engine_accepts() {
    let multipass = crate::ui::wants_multipass("Multi-pass", 5);
    let want_iso = matches!(out_kind("Selected titles → MKV"), OutKind::IsoImage);
    let user_raw = crate::ui::raw_applies(false, want_iso, true);
    assert!(multipass, "the default rip mode is a multipass plan");
    assert!(!user_raw, "raw does not apply to a title output");

    let raw = recovery_raw(multipass, want_iso, user_raw);
    assert!(
        raw,
        "a multipass image staged for a title mux stays raw on disk; the mux decrypts"
    );
}

// The three seams inside run_disc/run_blocking that no test can reach
// directly (need a live drive/disc image). Source pins, `expect`ed never
// defaulted, so a stale anchor fails loud instead of silently widening.
#[test]
fn the_private_disc_seams_still_go_through_their_guards() {
    let src = include_str!("engine.rs").replace("\r\n", "\n");
    let slice = |from: &str, to: &str| -> String {
        let a = src
            .find(from)
            .unwrap_or_else(|| panic!("anchor missing: {from}"));
        let b = src[a..]
            .find(to)
            .unwrap_or_else(|| panic!("closing anchor missing: {to}"));
        src[a..a + b].to_string()
    };

    // 1. The recovery job's raw flag — the defect that made every rip on
    //    the shipped defaults fail before reading a sector.
    let recover = slice(
        "        let mut job = recovery_job(&req.source, &iso_path, &indices);",
        "            let result = match fe::run_with(&plan, with, sink) {",
    );
    assert!(
        recover.contains("recovery_raw(req.multipass, want_iso, req.raw)"),
        "the recovery job must take its raw flag from recovery_raw; \
             passing req.raw straight through is what multipass_rip refuses"
    );

    // 0. An image source for an ISO/folder output is checked before the scan.
    let blocking = slice(
        "\nfn run_blocking(",
        "    let src = fe::ImageSource::from_path(src_path);",
    );
    assert!(blocking.contains("whole_image_gate(&req.format, src_path)?;"));
    let titles = slice(
        "\nfn run_blocking(",
        "        .push(format!(\"selection resolved to titles",
    );
    assert!(
        titles.contains("fe::ensure_titles_staged(src_path, &disc, &indices)"),
        "an image mux checks a staged image holds its titles"
    );

    // 1a. An MKV deliverable stages only its scope, and a scoped image is never
    //     kept (AACS BD Pre-recorded 0.953 §3.7: nothing else is safe to read).
    assert!(
        recover.contains(
            "fe::mkv_staging_scope(&disc, held_source(&mut session)?, &indices, req.keep_iso)"
        ),
        "the MKV staging must be scoped through mkv_staging_scope"
    );
    let staged = slice(
        "            // The engine's recovery over the held drive, into the image the app holds",
        "            .map_err(|e| failed_with(\"recovery failed\", &e))?;\n            halted = result.halted;",
    );
    assert!(
        staged.contains("staging.as_deref()"),
        "the scope reaches the passes"
    );
    let keep = slice(
        "        let keep = req.keep_iso && staging.is_none();",
        "                remove_staging_iso(&iso_path, &map_path);",
    );
    assert!(
        keep.contains("should_delete_staging_iso(keep,"),
        "a scoped image is not kept"
    );

    // 1b. The staging mux reuses the rip's set and the drive's scan (KU §4.3, J14):
    //     no rescan, so the user's title numbers stay the drive's.
    let mux = slice(
        "        let mux = mux_staged_titles(",
        "        // The staged image is only disposable",
    );
    assert!(
        mux.contains("mux_staged_titles(req, &iso_path, disc, set, &indices"),
        "the staging mux must mux the drive's picks from the drive's scan"
    );

    // 2. The drive -> ISO destination (the most-travelled label seam).
    let dest = slice(
        "        let iso_path = std::path::Path::new(&req.dest_dir)",
        "        let mut job = recovery_job(",
    );
    assert!(
        dest.contains("sanitize_label(&label)"),
        "the drive -> ISO destination must sanitise the disc label"
    );

    // 3. The image-decrypt destination.
    let img = slice(
        "            let dest =\n                std::path::Path::new(&req.dest_dir)",
        "            // Never write over the source.",
    );
    assert!(
        img.contains("sanitize_label(&label)"),
        "the image-decrypt destination must sanitise the disc label"
    );

    // 4. Nothing else sees WHICH title index the loop hands
    //    `mux_session_title`. 5. Nothing else sees that the
    //    per-title loop calls `verify_title_identity`.
    let rescan = slice(
        "        // This is a DIFFERENT scan from the one the selection was made against.",
        "        match mux_session_title(",
    );
    assert!(
        rescan.contains("verify_title_identity(picked_ids.get(idx), &rescanned, idx)"),
        "the live-drive loop must confirm the title still at this index is \
             the one that was picked, against the identities banked from the \
             FIRST scan; without it an integer is carried across two scans"
    );
    assert!(
        rescan.contains("return Err(std::io::Error::other(msg));"),
        "the identity check's verdict must stop the title — an ignored \
             Err leaves the wrong-title mux running"
    );
    // 5b. And it must SAY why: returning through `?` alone skips the
    //     `Err(e)` arm below — the only thing that puts a per-title
    //     reason in the log pane — reducing it to "Write failed (Other)."
    assert!(
        // Matched on the push alone, not the whole lock expression: the
        // latter is one rustfmt decision away from wrapping across lines,
        // which would fail this pin for an unrelated reason.
        rescan.contains(".push(msg.clone());"),
        "the identity mismatch must reach the log pane: propagating it \
             through `?` alone reduces the wrong-title diagnosis to \
             \"Write failed (Other).\""
    );
    // 5c. The drive bring-up returns through the same `?` and loses its
    //     reason the same way — "no disc in the drive" also arrives as
    //     "Write failed (Other)."
    let bringup = slice(
        "        let mut session = match fe::open_scan(",
        "        // This is a DIFFERENT scan",
    );
    assert!(
        bringup.contains(".push(") && bringup.contains("explain(e.code())"),
        "a failed per-title drive bring-up must say why in the log pane"
    );
    assert!(
        bringup.contains("return Err(e.into());"),
        "the bring-up failure keeps its typed code"
    );

    let session_mux = slice("\nfn mux_session_title(", "\nfn title_plan(");
    assert!(
        session_mux.contains("title_options(req, Some(idx))"),
        "the live-drive loop must build its MuxOptions for THIS title; a \
             selection built once before the loop is the union, and writes \
             tracks the user unticked under this title"
    );
}

// ── "Stop & Quit" must stop before it quits ─── worker closes the
// partial file at its next boundary; AppKit used to answer
// `TerminateNow` immediately, tearing down the process mid-write.

#[test]
fn a_quit_waits_for_a_worker_that_is_still_winding_down() {
    let run = std::sync::Arc::new(super::RunState::default());
    let worker = run.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(120));
        worker
            .finished
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let start = std::time::Instant::now();
    assert!(
        super::await_worker_exit(&run, std::time::Duration::from_secs(5)),
        "the worker finished well inside the grace period and the wait \
             must report that it did"
    );
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(100),
        "it returned before the worker was done — nothing was waited for"
    );
}

#[test]
fn a_wedged_worker_does_not_turn_quit_into_a_hang() {
    let run = super::RunState::default();
    let start = std::time::Instant::now();
    assert!(
        !super::await_worker_exit(&run, std::time::Duration::from_millis(80)),
        "a worker that never finishes must be reported as not finished"
    );
    let waited = start.elapsed();
    assert!(waited >= std::time::Duration::from_millis(80), "{waited:?}");
    assert!(
        waited < std::time::Duration::from_secs(2),
        "the wait must end at its deadline, not linger: {waited:?}"
    );
}

/// A worker that had already finished is not waited for at all.
#[test]
fn a_finished_worker_is_not_waited_for() {
    let run = super::RunState::default();
    run.finished
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let start = std::time::Instant::now();
    assert!(super::await_worker_exit(&run, super::QUIT_GRACE));
    assert!(
        start.elapsed() < std::time::Duration::from_millis(50),
        "quitting after a finished rip must be instant"
    );
}

// The selection the user made is checked against the scan the RIP takes.
// This is the first (longest-window) re-scan in the file; every LATER
// one is identity-checked already. Source pin: needs a live drive.
#[test]
fn the_drive_rip_checks_the_selection_against_the_scan_it_was_made_on() {
    let src = include_str!("engine.rs").replace("\r\n", "\n");
    let start = src
        .find("\nfn run_disc(")
        .expect("run_disc definition present");
    let end = start
        + src[start..]
            .find("\n    // Decrypted folder:")
            .expect("the folder branch still ends the scan section");
    let body = &src[start..end];
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("verify_selection_identity(&req.titles, &req.title_ids,"),
        "the fresh scan must be checked against the identities the ticked \
             numbers referred to, BEFORE any branch resolves them"
    );
}

// The IMAGE path re-scans too, and the same selection has to survive it.
// run_blocking scans the iso://folder source again at Run time; the file
// could be replaced/re-authored/re-mounted between the UI scan and this.
#[test]
fn the_image_rip_checks_the_selection_against_the_scan_it_was_made_on() {
    let src = include_str!("engine.rs").replace("\r\n", "\n");
    let start = src
        .find("\nfn run_blocking(")
        .expect("run_blocking definition present");
    let end = start
        + src[start..]
            .find("\n    let indices = fe::resolve_selection(&disc, &sel);")
            .expect("the selection is still resolved in run_blocking");
    let body = &src[start..end];
    // Whitespace-stripped, not whitespace-collapsed: rustfmt splits this
    // call across lines, and a pin that fails for a line break rather than
    // a behaviour change is a pin that gets deleted.
    let dense: String = body.split_whitespace().collect();
    assert!(
        dense.contains("verify_selection_identity(&req.titles,&req.title_ids,"),
        "the image path resolves ticked numbers against a scan nobody \
             compared to the one they were ticked on"
    );
}

// The GUI's image decrypt must ask the SAME "is the destination the
// source?" question the CLI asks, through the ONE same_file definition —
// a canonical-path check alone misses a hardlink. Source pin: real files.
#[test]
fn the_gui_image_decrypt_asks_the_shared_same_file_question() {
    let src = include_str!("engine.rs").replace("\r\n", "\n");
    let start = src
        .find("\n        OutKind::IsoImage => {")
        .expect("the ISO-image arm is still there");
    let end = start
        + src[start..]
            .find("let lock = hold_iso_lock(&dest, state)?;")
            .expect("the decrypt call still closes the arm's setup");
    let body = &src[start..end];
    assert!(
        body.contains("same_file("),
        "the image-decrypt arm must decide through the shared same_file \
             guard, which catches a hardlinked destination too"
    );
    assert!(
        !body.contains("canonicalize("),
        "a canonical-path comparison beside the call site is the second \
             definition of file identity that let the GUI diverge from the CLI"
    );
}

// Every GUI site that grades a finished mux must grade the LOSS too:
// MuxOutcome::completed alone is not enough (undelivered_streams can be
// non-empty). Source pin: all four sit in closures needing a live drive.
#[test]
fn every_gui_mux_site_reports_the_streams_it_could_not_deliver() {
    let src = include_str!("engine.rs").replace("\r\n", "\n");
    let slice = |from: &str, to: &str| -> String {
        let a = src
            .find(from)
            .unwrap_or_else(|| panic!("anchor missing: {from}"));
        let b = src[a..]
            .find(to)
            .unwrap_or_else(|| panic!("closing anchor missing: {to}"));
        src[a..a + b].to_string()
    };

    // 1 + 2. The single-file/container conversion (`run_stream`): both the
    //        line it pushes and the summary it returns.
    let stream = slice(
        "    .map_err(|e| format!(\"convert failed: {e}\"))?;",
        "// Whether a demux rip must give each title its own subdirectory.",
    );
    assert!(
        stream.contains("lossy_lines(&o, &target)"),
        "the stream conversion must report everything the mux lost — the \
             tracks the sink dropped AND the payload bytes it could not carry"
    );
    // Whitespace-collapsed: the call spans several lines once rustfmt has
    // had it, and the pin is about the ARGUMENTS, not the line breaks.
    let flat: String = stream.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("summarize_stream(&o, &target, &req.dest_dir)"),
        "the stream conversion's SUMMARY must grade the whole outcome, not \
             `completed` alone (true for a lossy export) and not a hand-picked \
             pair of its fields (which is how the byte loss went unread)"
    );

    // 3. The ISO/image per-title loop.
    let iso_loop = slice(
        "        let muxed = stopped_before_output(run_title(&plan, with, sink));",
        "            Err(e) => {",
    );
    assert!(
        iso_loop.contains("lossy_lines(&o, &target)"),
        "the ISO per-title loop must report everything the mux lost"
    );

    // 4. The live-drive per-title loop.
    let disc_loop = slice(
        "        match mux_session_title(req, &mut session, idx, &set, &dest_url, sink) {",
        "            Err(e) => {",
    );
    assert!(
        disc_loop.contains("lossy_lines(&o, &target)"),
        "the live-drive per-title loop must report everything the mux lost"
    );
}

// ── A stream selection is PER TITLE ─── `ticked_streams` used to union
// every title's ticked PIDs across the whole loop, so unticking a shared
// PID under one title left it ticked (and written) under a sibling.

fn req_with_title_pids(pids: Vec<(usize, Vec<u16>, Vec<u16>)>) -> RipRequest {
    RipRequest {
        explicit_streams: true,
        audio_pids: vec![0x1100, 0x1101],
        sub_pids: vec![0x1200],
        title_pids: crate::engine::TitleStreams::PerTitle(pids),
        ..req()
    }
}

/// The defect: one PID ticked under one title and not the other.
#[test]
fn a_pid_unticked_under_one_title_is_not_written_for_that_title() {
    let r = req_with_title_pids(vec![
        // Title 0 keeps both audio tracks.
        (0, vec![0x1100, 0x1101], vec![0x1200]),
        // Title 1 has the commentary unticked.
        (1, vec![0x1100], vec![0x1200]),
    ]);
    let only = |sel: &libfreemkv::StreamSelection| match &sel.audio {
        libfreemkv::PidFilter::Only(v) => v.clone(),
        _ => panic!("an explicit selection must be a PidFilter::Only"),
    };
    assert_eq!(
        only(&stream_selection_for(&r, Some(0))),
        vec![0x1100, 0x1101]
    );
    assert_eq!(
        only(&stream_selection_for(&r, Some(1))),
        vec![0x1100],
        "the commentary was unticked for title 1; the union would have \
             written it anyway because title 0 still has it"
    );
}

// Same defect on the LIVE-DRIVE path, unreached by the ISO fix: the
// Session arm takes selection from MuxOptions, built once from the union
// before the loop. Expectation below is the user's ticks written by hand.
#[test]
fn a_live_drive_rip_applies_each_title_s_own_ticks() {
    let r = req_with_title_pids(vec![
        // The user ticked both audio tracks and the subtitle under title 0…
        (0, vec![0x1100, 0x1101], vec![0x1200]),
        // …and unticked the commentary (0x1101) under title 1.
        (1, vec![0x1100], vec![0x1200]),
    ]);
    let audio_of = |idx: usize| match title_options(&r, Some(idx)).selection.unwrap().audio {
        libfreemkv::PidFilter::Only(v) => v,
        _ => panic!("an explicit selection must be a PidFilter::Only"),
    };
    assert_eq!(
        audio_of(0),
        vec![0x1100, 0x1101],
        "title 0 keeps the commentary the user left ticked"
    );
    assert_eq!(
        audio_of(1),
        vec![0x1100],
        "title 1 must NOT get 0x1101: the user unticked it there, and the \
             union kept it only because title 0 still has it"
    );
    // And the rest of the drive-path options are untouched by this.
    let base = mux_opts(&r);
    let o = title_options(&r, Some(1));
    assert_eq!(o.batch_sectors, base.batch_sectors);
    assert_eq!(o.skip_errors, base.skip_errors);
}

// A request with no per-title breakdown (CLI, container path,
// FMKV_APIDS) falls back to the union for EVERY title — the pre-
// breakdown behaviour. Asserted against TitleStreams::Unspecified.
#[test]
fn without_a_per_title_breakdown_the_union_still_applies() {
    let r = RipRequest {
        title_pids: crate::engine::TitleStreams::Unspecified,
        ..req_with_title_pids(Vec::new())
    };
    for t in [None, Some(0), Some(7)] {
        let s = stream_selection_for(&r, t);
        match (&s.audio, &s.subtitle) {
            (libfreemkv::PidFilter::Only(a), libfreemkv::PidFilter::Only(b)) => {
                assert_eq!(a, &[0x1100, 0x1101], "the union must reach title {t:?}");
                assert_eq!(b, &[0x1200], "the union must reach title {t:?}");
            }
            _ => panic!("explicit selection expected"),
        }
    }
}

// MEANING CHANGED by the absence/empty split: absence used to mean "the
// user emptied it" (the defect). Now only titles with no selectable rows
// are absent; an EMPTY entry means the user kept nothing.
#[test]
fn a_title_the_breakdown_never_mentions_falls_back_to_the_union() {
    let r = req_with_title_pids(vec![(0, vec![0x1100], vec![])]);
    match &stream_selection_for(&r, Some(9)).audio {
        libfreemkv::PidFilter::Only(v) => assert_eq!(v, &[0x1100, 0x1101]),
        _ => panic!("explicit selection expected"),
    }
    // But a title the breakdown DOES mention with an empty list is not
    // "absent": it is the user keeping nothing.
    let emptied = req_with_title_pids(vec![(0, vec![0x1100], vec![]), (1, vec![], vec![])]);
    match &stream_selection_for(&emptied, Some(1)).audio {
        libfreemkv::PidFilter::Only(v) => assert!(
            v.is_empty(),
            "an empty entry is a decision, not a missing one"
        ),
        _ => panic!("explicit selection expected"),
    }
}

// Two titles of one feature that SHARE PIDs — the ordinary Blu-ray shape.
// Title 0 keeps everything; title 1 has every row unticked. Expectations
// are the user's ticks written as literals, not derived from the code.
fn shared_pid_disc() -> crate::engine::Scanned {
    use crate::engine::{Row, Scanned};
    let mk = |ty: &str, ti: usize, pid: Option<u16>| Row {
        role: None,
        type_s: ty.into(),
        item: ty.into(),
        format: String::new(),
        notes: String::new(),
        desc: format!("{ty} of title {ti}"),
        depth: if ty == "Title" { 1 } else { 2 },
        checkable: ty != "Video",
        title: ti,
        info: String::new(),
        pid,
        duration_secs: if ty == "Title" { 5400.0 } else { 0.0 },
        lang: String::new(),
        forced: false,
        mirrors: None,
        size_bytes: None,
    };
    let mut rows = vec![Row {
        depth: 0,
        checkable: false,
        title: usize::MAX,
        ..mk("Bluray disc", usize::MAX, None)
    }];
    for ti in 0..2 {
        rows.push(mk("Title", ti, None));
        rows.push(mk("Video", ti, None));
        // The SAME pids under both titles — that sharing is what makes the
        // union indistinguishable from title 0's own selection.
        rows.push(mk("Audio", ti, Some(0x1100)));
        rows.push(mk("Audio", ti, Some(0x1101)));
        rows.push(mk("Subtitles", ti, Some(0x1200)));
    }
    Scanned {
        label: "SHARED".into(),
        volume_id: "SHARED".into(),
        rows,
        key_summary: String::new(),
        title_count: 2,
        video_codecs: vec!["H.264".into(); 2],
        title_sizes: Vec::new(),
        capacity_bytes: 0,
        title_ids: Vec::new(),
        details: vec![],
        keys: None,
        needs_disc: false,
        refusal: None,
    }
}

// The hole commit 8f9a31c left: ticked_streams_by_title skipped an
// unticked row BEFORE creating its title's slot, so an emptied title
// fell back to the UNION, writing the sibling's tracks into it anyway.
#[test]
fn a_title_the_user_emptied_rips_no_streams_at_all() {
    let sc = shared_pid_disc();
    let t = crate::ui::Tree::from_scan(&sc, "All titles", 0.0, &crate::ui::LangPrefs::default());
    // The user clears every stream row under title 1 and touches nothing
    // under title 0.
    let rows: Vec<usize> = t
        .arena
        .iter()
        .enumerate()
        .filter(|(_, n)| n.title_idx == 1 && n.pid.is_some())
        .map(|(i, _)| i)
        .collect();
    assert_eq!(rows.len(), 3, "fixture must give title 1 three stream rows");
    for i in rows {
        t.set_checked(i, false);
    }

    let (audio_pids, sub_pids, explicit) = t.ticked_streams();
    assert!(explicit, "clearing rows must read as an explicit narrowing");
    let r = RipRequest {
        explicit_streams: explicit,
        audio_pids,
        sub_pids,
        title_pids: t.ticked_streams_by_title(),
        ..req()
    };
    let sel = |ti: usize| {
        let s = stream_selection_for(&r, Some(ti));
        match (s.audio, s.subtitle) {
            (libfreemkv::PidFilter::Only(a), libfreemkv::PidFilter::Only(b)) => (a, b),
            _ => panic!("an explicit selection must be a PidFilter::Only"),
        }
    };
    assert_eq!(
        sel(0),
        (vec![0x1100u16, 0x1101], vec![0x1200u16]),
        "title 0 was left alone and must keep exactly what is ticked there"
    );
    assert_eq!(
        sel(1),
        (Vec::<u16>::new(), Vec::<u16>::new()),
        "the user cleared every row under title 1: it must rip NO audio \
             and NO subtitles. Falling back to the union writes title 0's \
             shared tracks into title 1 anyway."
    );
    // The live-drive path must agree — it reads its selection from
    // MuxOptions, not InputOptions.
    match title_options(&r, Some(1)).selection.unwrap().audio {
        libfreemkv::PidFilter::Only(v) => {
            assert!(v.is_empty(), "the drive path kept tracks title 1 cleared")
        }
        _ => panic!("an explicit selection must be a PidFilter::Only"),
    }
}

/// And "made no choice at all" still means keep everything, per title.
#[test]
fn an_untouched_selection_keeps_every_stream_for_every_title() {
    let r = RipRequest {
        explicit_streams: false,
        title_pids: crate::engine::TitleStreams::PerTitle(vec![(0, vec![0x1100], vec![])]),
        ..req()
    };
    assert!(stream_selection_for(&r, Some(0)).is_all());
    assert!(stream_selection_for(&r, None).is_all());
}

// One scanned title by playlist name + SECTORS read. duration/size are
// IDENTICAL across fixtures (duplicate playlists share them), so identity
// uses neither — multipass re-scan can shift numbers; remap re-resolves.
fn id(playlist: &str, start_lba: u32) -> TitleIdentity {
    TitleIdentity::of(&libfreemkv::DiscTitle {
        playlist: playlist.to_string(),
        playlist_id: playlist
            .trim_end_matches(".mpls")
            .parse::<u16>()
            .unwrap_or(0),
        duration_secs: 7530.0,
        size_bytes: 20 << 30,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![libfreemkv::Extent {
            start_lba,
            sector_count: 1000,
        }],
        content_format: libfreemkv::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    })
}

// ── The case the project's own rule names ── titles can legitimately
// share playlist, duration, and size; only the SECTORS tell them apart.
// Both fixtures below differ in exactly one field, `extents[0].start_lba`.

/// One of a legitimately duplicated pair: same playlist name, same
/// playlist id, same duration, same size — read from different sectors.
fn dup_title(start_lba: u32) -> libfreemkv::DiscTitle {
    libfreemkv::DiscTitle {
        playlist: "00800.mpls".to_string(),
        playlist_id: 800,
        duration_secs: 7530.0,
        size_bytes: 20 << 30,
        clips: Vec::new(),
        streams: Vec::new(),
        chapters: Vec::new(),
        extents: vec![libfreemkv::Extent {
            start_lba,
            sector_count: 1000,
        }],
        content_format: libfreemkv::ContentFormat::BdTs,
        codec_privates: Vec::new(),
    }
}

/// The GUI's live-drive verify must refuse the same pair swapped, instead
/// of muxing the other half of it under the number the user asked for.
#[test]
fn a_duplicate_playlist_swapped_by_the_rescan_is_refused_not_muxed() {
    let first = TitleIdentity::of(&dup_title(1000));
    let second = TitleIdentity::of(&dup_title(9000));
    // The selection was made against a scan whose index 0 was `second`;
    // the rescan lists the pair the other way round.
    let rescan = vec![first, second.clone()];
    assert!(
        verify_title_identity(Some(&second), &rescan, 0).is_err(),
        "index 0 now names the other half of the duplicate pair; muxing it \
             under the picked number is the wrong-title write this guard exists \
             to stop"
    );
}

// ── The SINGLE-PASS drive path has the same seam, one scan later ──
// Each title in the loop re-scans and carries only an integer;
// `verify_title_identity` stops that integer naming a different title.

/// THE DEFECT: the rescan lists the same titles in a different order, so
/// the index still resolves — to the wrong film.
#[test]
fn a_reordered_rescan_stops_the_mux_instead_of_writing_the_wrong_title() {
    let picked = [id("00800.mpls", 7530), id("00001.mpls", 300)];
    let rescan = vec![id("00001.mpls", 300), id("00800.mpls", 7530)];
    let e = verify_title_identity(picked.first(), &rescan, 0)
        .expect_err("index 0 now names 00001.mpls, not the picked feature");
    assert!(
        e.contains("00800.mpls") && e.contains("00001.mpls"),
        "the message must name both titles: {e}"
    );
}

/// A rescan that drops a title BEFORE the selected index leaves the index
/// in range and pointing somewhere else.
#[test]
fn a_rescan_that_drops_an_earlier_title_is_refused_not_shifted() {
    let picked = [
        id("00800.mpls", 7530),
        id("00001.mpls", 300),
        id("00003.mpls", 120),
    ];
    let rescan = vec![id("00800.mpls", 7530), id("00003.mpls", 120)];
    assert!(verify_title_identity(picked.get(1), &rescan, 1).is_err());
}

/// THE NORMAL PATH: a stable disc rescans identically and every title is
/// muxed exactly as before — including when an unrelated LATER title is
/// missing from the second scan, which says nothing about this one.
#[test]
fn a_stable_rescan_passes_every_selected_title() {
    let picked = [id("00800.mpls", 7530), id("00001.mpls", 300)];
    let rescan = vec![
        id("00800.mpls", 7530),
        id("00001.mpls", 300),
        id("00003.mpls", 120),
    ];
    for idx in 0..picked.len() {
        assert_eq!(verify_title_identity(picked.get(idx), &rescan, idx), Ok(()));
    }
    let shorter = vec![id("00800.mpls", 7530), id("00001.mpls", 300)];
    assert_eq!(verify_title_identity(picked.get(1), &shorter, 1), Ok(()));
}

/// Nothing recorded for that index → nothing to disagree with, and the
/// pre-existing behaviour is left exactly as it was.
#[test]
fn with_no_recorded_identity_the_index_is_used_as_before() {
    let rescan = vec![id("00800.mpls", 7530)];
    assert_eq!(verify_title_identity(None, &rescan, 0), Ok(()));
}

/// The rescan is shorter than the index: caught here rather than deeper in
/// the mux, and named.
#[test]
fn a_title_missing_from_the_rescan_is_named() {
    let picked = [id("00800.mpls", 7530), id("00001.mpls", 300)];
    let e = verify_title_identity(picked.get(1), &picked[..1], 1).expect_err("must refuse");
    assert!(e.contains("00001.mpls"), "{e}");
}

/// The playlist name is on-disc metadata and this message is shown to the
/// user, so it goes through the same display sanitiser everything else does.
#[test]
fn a_crafted_playlist_name_cannot_reach_the_ui_raw() {
    let picked = [id("\u{1b}c00800.mpls", 7530)];
    let rescan = vec![id("00001.mpls", 300)];
    let e = verify_title_identity(picked.first(), &rescan, 0).expect_err("must refuse");
    assert!(!e.contains('\u{1b}'), "ESC survived into the UI: {e:?}");
}

// ── The selection is made against a scan the rip never sees ──
// `run_disc` opens a BRAND NEW scan at Start and resolves the ticked
// NUMBERS against it — the longest window for a disc swap to invalidate it.

/// The disc changed under the selection: refused, and named.
#[test]
fn a_selection_made_against_an_earlier_scan_is_refused_when_the_disc_changed() {
    let when_ticked = vec![id("00800.mpls", 1000), id("00003.mpls", 13000)];
    // A different disc: title 0 is something else entirely.
    let fresh = vec![id("00001.mpls", 300), id("00003.mpls", 13000)];
    let e = verify_selection_identity(&[0], &when_ticked, &fresh)
        .expect_err("title 1 is not the title the user ticked");
    assert!(
        e.contains("00800.mpls") && e.contains("00001.mpls"),
        "the message must name both titles: {e}"
    );
}

/// The ordinary case must still rip: the same disc rescans identically, and
/// a selection with nothing recorded behaves exactly as it did before.
#[test]
fn a_selection_that_still_matches_the_fresh_scan_is_allowed() {
    let when_ticked = vec![id("00800.mpls", 1000), id("00003.mpls", 13000)];
    let fresh = when_ticked.clone();
    assert_eq!(
        verify_selection_identity(&[0, 1], &when_ticked, &fresh),
        Ok(())
    );
    assert_eq!(
        verify_selection_identity(&[0, 1], &[], &fresh),
        Ok(()),
        "no identities captured is the pre-existing behaviour, untouched"
    );
}

/// A number that is out of range for the fresh scan is caught HERE, not
/// silently dropped on the way to the mux.
#[test]
fn a_selected_number_the_fresh_scan_no_longer_has_is_refused() {
    let when_ticked = vec![id("00800.mpls", 1000), id("00003.mpls", 13000)];
    let fresh = vec![id("00800.mpls", 1000)];
    assert!(verify_selection_identity(&[1], &when_ticked, &fresh).is_err());
}

/// A single-pass recovery is an ordinary decrypting copy, so the user's
/// setting stands — forcing raw there would hand back an encrypted image
/// nobody asked for.
#[test]
fn a_single_pass_recovery_keeps_the_users_raw_setting() {
    assert!(!recovery_raw(false, true, false));
    assert!(recovery_raw(false, true, true));
}

/// Whole disc → ISO, multipass, raw off: a decrypted image, recovered over passes.
#[test]
fn a_multipass_recovery_to_an_iso_decrypts_unless_raw() {
    assert!(!recovery_raw(true, true, false));
    assert!(recovery_raw(true, true, true));
    // A staged image for a title mux stays raw on disk.
    assert!(recovery_raw(true, false, false));
}

// A GUI ISO or folder output from an image staged for an MKV rip is refused before
// the scan, exactly like the CLI's; an MKV output from it is not.
#[test]
fn a_staged_image_is_refused_as_a_whole_disc_source() {
    let dir = std::env::temp_dir().join(format!("fmkv-gui-staged-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let iso = dir.join("STAGED.iso");
    std::fs::write(&iso, vec![0u8; 2048]).unwrap();
    std::fs::write(
        fe::mapfile_path_for(&iso),
        "# freemkv-scope: 0x0+0x800\n0x0 ? 1\n0x0 0x800 ?\n",
    )
    .unwrap();
    for format in ["ISO image", "decrypted folder"] {
        let err = whole_image_gate(format, &iso).unwrap_err();
        assert!(err.starts_with("E6022 "), "{format}: {err}");
        assert!(err.contains(&iso.display().to_string()), "{err}");
    }
    whole_image_gate("MKV", &iso).expect("an MKV of its titles is what it is for");
    whole_image_gate("ISO image", &dir.join("plain.iso")).expect("no mapfile");
    let _ = std::fs::remove_dir_all(&dir);
}

// JUDGEMENT: a scoped staging image is never kept; the note says why, only then.
#[test]
fn the_not_kept_note_appears_only_for_a_kept_request_on_a_scoped_staging() {
    assert!(staging_not_kept_note(true, true).contains("was not kept"));
    assert_eq!(staging_not_kept_note(true, false), "");
    assert_eq!(staging_not_kept_note(false, true), "");
}

// G4/D4: the GUI prints the pre-mux note where the CLI does, at the output opening
// (fe::Event::OutputOpened, from MuxEvents::on_output_opened), into the run log.
#[test]
fn the_gui_note_comes_from_the_output_opening() {
    crate::strings::set_locale("en");
    let truehd = libfreemkv::Stream::Audio(libfreemkv::AudioStream {
        pid: 0x1100,
        codec: libfreemkv::Codec::TrueHd,
        channels: libfreemkv::AudioChannels::Surround51,
        language: "eng".into(),
        sample_rate: libfreemkv::SampleRate::S48,
        secondary: false,
        purpose: libfreemkv::LabelPurpose::Normal,
        label: String::new(),
    });
    let title = libfreemkv::DiscTitle {
        streams: vec![truehd],
        codec_privates: vec![None],
        ..libfreemkv::DiscTitle::empty()
    };
    let state = Arc::new(RunState::default());
    let sink = UiSink(state.clone());
    fe::Sink::event(
        &sink,
        &fe::Event::OutputOpened {
            dest: "mp4:///out/x.mp4",
            title: &title,
        },
    );
    let lines = state
        .lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("left out") && l.contains("MP4")),
        "the excluded note reaches the run log, got: {lines:?}"
    );
}

// A container whose name reproduces the template's output is never truncated by its own rip.
#[test]
fn a_container_rip_never_overwrites_its_own_source() {
    let dir = std::env::temp_dir().join(format!("fmkv-own-src-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("1.mpg");
    crate::lossy::mpg_source_fixture(&src);
    let before = std::fs::read(&src).unwrap();
    let mut r = req();
    r.source = src.to_string_lossy().into_owned();
    r.format = "Selected titles → MPG".into();
    r.dest_dir = dir.to_string_lossy().into_owned();
    r.filename_template = "{n}".into();
    let state = Arc::new(RunState::default());
    let res = run_stream(&r, &UiSink(state.clone()), &state);
    let kept = std::fs::read(&src).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(res.is_err(), "{res:?}");
    assert_eq!(kept, before, "the source was rewritten");
}

// D4 parity: a mux that fails before its output opens prints no pre-mux note, in the GUI
// as in the CLI (pipe.rs `a_mux_that_fails_before_open_prints_no_note`).
#[test]
fn a_mux_that_fails_before_open_logs_no_note() {
    crate::strings::set_locale("en");
    let dir = std::env::temp_dir().join(format!("fmkv-d4-gui-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.mpg");
    crate::lossy::mpg_source_fixture(&src);
    let mut r = req();
    r.source = src.to_string_lossy().into_owned();
    r.format = "Selected titles → MP4".into();
    r.dest_dir = dir.join("missing").to_string_lossy().into_owned();
    let state = Arc::new(RunState::default());
    let sink = UiSink(state.clone());
    let res = run_stream(&r, &sink, &state);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(res.is_err(), "no such output directory");
    let lines = state
        .lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert!(
        !lines.iter().any(|l| l.contains("left out")),
        "no note for an output that never opened, got: {lines:?}"
    );
}

/// 1.8.0: Open and Start look a loose clip's keys up from its disc folder, as the CLI
/// does; the same clip outside any disc folder refuses E7022 before any output.
#[test]
fn a_loose_clip_reads_with_its_disc_folder_keys() {
    use crate::ku_fixture as kf;
    crate::strings::set_locale("en");
    let dir = std::env::temp_dir().join(format!("fmkv-loose-gui-{}", std::process::id()));
    let fx = kf::bd_image();
    let clip = kf::write_folder(&fx, &dir.join("disc"));
    let lone = dir.join("lone.m2ts");
    std::fs::copy(&clip, &lone).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let rip = |src: &std::path::Path, out: &str| {
        let mut r = req();
        r.source = src.to_string_lossy().into_owned();
        r.dest_dir = dir.join(out).to_string_lossy().into_owned();
        let state = Arc::new(RunState::default());
        let res = run_stream(&r, &UiSink(state.clone()), &state);
        let written = std::fs::read_dir(dir.join(out)).map_or(0, |d| d.count());
        (res, written)
    };
    let (open, open_lone, ok, refused) =
        crate::rip_keys::with_sources(kf::holding(&calls, kf::K1), || {
            let (k, t) = (KeyConfig::default(), super::OpenToken::default());
            let open = super::scan_stream_under(&clip.to_string_lossy(), &k, &t);
            let open_lone = super::scan_stream_under(&lone.to_string_lossy(), &k, &t);
            (open, open_lone, rip(&clip, "a"), rip(&lone, "b"))
        });
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        open.map(|s| s.key_summary),
        Ok("unlocked via online".into())
    );
    assert!(open_lone.is_err_and(|e| e.contains("7022")));
    assert!(
        ok.0.is_ok() && ok.1 == 1,
        "the clip muxes with its folder's keys: {ok:?}"
    );
    assert!(refused.0.is_err_and(|e| e.contains("7022")) && refused.1 == 0);
    assert!(calls.load(std::sync::atomic::Ordering::SeqCst) > 0);
}

fn req() -> RipRequest {
    RipRequest {
        source: "/media/movie.iso".into(),
        dest_dir: "/out".into(),
        titles: vec![],
        title_ids: Vec::new(),
        format: "MKV".into(),
        audio_pids: vec![],
        sub_pids: vec![],
        title_pids: crate::engine::TitleStreams::Unspecified,
        explicit_streams: false,
        raw: false,
        force: false,
        filename_template: String::new(),
        decrypt_threads: 0,
        multipass: false,
        max_passes: 0,
        abort_lost_secs: 0,
        keep_iso: false,
        auto_eject: false,
        keys: KeyConfig::default(),
        seed: None,
        vid_from: None,
    }
}

/// A `disc://` source must route to the live-drive path and nothing else
/// must. Forced `true` an ISO is opened as a drive; forced `false` a drive
/// rip is handed to `scan_iso` with a path of `disc://`.
#[test]
fn only_a_disc_url_is_a_disc_source() {
    assert!(is_disc_source("disc://"));
    assert!(is_disc_source("disc:///dev/sr0"));
    assert!(!is_disc_source("/media/movie.iso"));
    assert!(!is_disc_source("iso:///media/movie.iso"));
    assert!(!is_disc_source(""));
    assert!(!is_disc_source("mkv:///x.mkv"));
}

/// Bare `disc://` means autodetect; `disc://<path>` means that drive. A
/// `Some("")` here becomes `DeviceTarget::Path("")` and opens nothing.
#[test]
fn the_device_path_is_whatever_follows_the_scheme() {
    assert_eq!(disc_device("disc://"), None);
    assert_eq!(disc_device("disc:///dev/sr0").as_deref(), Some("/dev/sr0"));
    assert_eq!(
        disc_device("disc://\\\\.\\D:").as_deref(),
        Some("\\\\.\\D:")
    );
    // Not a disc URL at all — no prefix to strip, so no device.
    assert_eq!(disc_device("/media/movie.iso"), None);
}

/// Container sources skip the disc scan entirely. An ISO must NOT be one of
/// them, or it goes to the single-title container path with no title list.
#[test]
fn container_extensions_are_stream_sources_and_iso_is_not() {
    for good in ["a.mkv", "a.m2ts", "a.mts", "a.mp4", "A.MKV", "/p/a.Mp4"] {
        assert!(is_stream_source(good), "{good} should be a stream source");
    }
    for bad in ["a.iso", "a.bin", "", "disc://", "/no/extension"] {
        assert!(!is_stream_source(bad), "{bad} must not be a stream source");
    }
}

/// The scheme half of the mux source URL, from the one `CONTAINER_SOURCES` table (G5).
/// An extension it does not name is read as an image, never guessed as m2ts.
#[test]
fn the_source_scheme_follows_the_extension() {
    assert_eq!(source_scheme("a.mkv"), "mkv");
    assert_eq!(source_scheme("a.MKV"), "mkv");
    assert_eq!(source_scheme("a.mp4"), "mp4");
    assert_eq!(source_scheme("a.iso"), "iso");
    assert_eq!(source_scheme("a.m2ts"), "m2ts");
    assert_eq!(source_scheme("a.mts"), "m2ts");
    assert_eq!(source_scheme("a.mpg"), "mpg");
    assert_eq!(source_scheme("a.MPEG"), "mpg");
    assert_eq!(source_scheme("VTS_01_1.VOB"), "mpg");
    assert_eq!(source_scheme(""), "iso");
}

// source_scheme and image_or_dir_scheme answer DIFFERENT questions and
// were conflated twice while fixing folder support — source_scheme is
// right only for a container guarded by is_stream_source.
#[test]
fn image_or_dir_scheme_is_not_source_scheme() {
    // An image whose extension is not `.iso` must still be an image.
    for p in ["Disc.img", "Disc.bin", "Disc.udf", "Disc"] {
        assert_eq!(
            image_or_dir_scheme(p),
            "iso",
            "{p} is a disc image, not an elementary stream"
        );
        assert_ne!(
            source_scheme(p),
            "m2ts",
            "{p} is not a transport stream: no scheme is guessed from an unknown extension"
        );
    }
    // A real directory is dir://.
    let d = std::env::temp_dir();
    assert_eq!(image_or_dir_scheme(d.to_str().unwrap()), "dir");
}

/// The marker each `OutKind` is identified by in the sink tables below.
fn sink_marker(k: OutKind) -> String {
    match k {
        OutKind::DecryptedFolder => "folder".to_string(),
        OutKind::IsoImage => "iso".to_string(),
        OutKind::Demux(scheme) => scheme.to_string(),
        OutKind::File(s) => s.to_string(),
    }
}

/// Each of the twelve picker strings resolves to its own sink. Six of them
/// used to fall through to a per-title MKV mux, so this table is the thing
/// that stops the user's chosen format quietly becoming a different one.
#[test]
fn every_picker_format_maps_to_its_own_sink() {
    let cases: &[(&str, &str)] = &[
        ("Whole disc → decrypted folder", "folder"),
        ("Whole disc → ISO image", "iso"),
        ("Each title → separate track files", "demux"),
        ("Each title → MP4 file", "mp4"),
        ("Each title → M2TS file", "m2ts"),
        ("Chapters only (XML)", "chapters"),
        ("Title index (JSON)", "json"),
        ("Title index (.fvi)", "fvi"),
        ("Each title → MKV file", "mkv"),
        // Anything unrecognised is a container mux, not a whole-disc sink.
        ("", "mkv"),
    ];
    for (format, want) in cases {
        let got = sink_marker(out_kind(format));
        assert_eq!(&got, want, "format {format:?} resolved to {got:?}");
    }
}

// The CANONICAL picker strings — exact &'static strs the shells put in
// the dropdown. The table above uses paraphrases, so it couldn't catch
// an entry whose wording misses every out_kind branch and becomes MKV.
#[test]
fn the_real_picker_strings_each_reach_their_own_sink() {
    let want: &[(&str, &str)] = &[
        ("Selected titles → MKV", "mkv"),
        ("Selected titles → MP4", "mp4"),
        ("Selected titles → MPG", "mpg"),
        ("Selected titles → M2TS", "m2ts"),
        ("Selected titles → separate track files", "demux"),
        ("Selected titles → video tracks only", "video"),
        ("Selected titles → audio tracks only", "audio"),
        ("Selected titles → subtitle tracks only", "sub"),
        ("Whole disc → ISO image", "iso"),
        ("Whole disc → decrypted folder", "folder"),
        ("Chapters → file", "chapters"),
        ("Title info → JSON", "json"),
        ("Video index → .fvi", "fvi"),
    ];
    let offered: Vec<&str> = crate::ui::output_formats(true, true)
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(
        offered.len(),
        want.len(),
        "the picker gained or lost an entry without a sink mapping: {offered:?}"
    );
    for (format, marker) in want {
        assert!(offered.contains(format), "{format:?} is no longer offered");
        assert_eq!(
            &sink_marker(out_kind(format)),
            marker,
            "{format:?} resolved to the wrong sink"
        );
    }
    // Distinct sinks: no two picker entries may collapse onto one.
    let mut markers: Vec<String> = offered.iter().map(|f| sink_marker(out_kind(f))).collect();
    markers.sort();
    let n = markers.len();
    markers.dedup();
    assert_eq!(n, markers.len(), "two picker entries share a sink");
}

/// The three per-track-kind entries must build a DIRECTORY dest URL under
/// their own scheme, never a `<scheme>` file extension. `video://out/x.video`
/// is not a thing; `video://out/` is.
#[test]
fn per_track_kind_sinks_are_directory_urls() {
    for (format, scheme) in [
        ("Selected titles → video tracks only", "video"),
        ("Selected titles → audio tracks only", "audio"),
        ("Selected titles → subtitle tracks only", "sub"),
    ] {
        match out_kind(format) {
            OutKind::Demux(s) => assert_eq!(s, scheme),
            other => panic!("{format:?} is {:?}, not a demux sink", sink_marker(other)),
        }
    }
}

// The dest-URL SCHEME a picker string produces, as libfreemkv would parse
// it (sink_marker is only an internal label) — what CLI parity is
// measured against. Only `folder` differs: it is the CLI's dir://.
fn dest_scheme(format: &str) -> String {
    match sink_marker(out_kind(format)).as_str() {
        "folder" => "dir".to_string(),
        other => other.to_string(),
    }
}

// PARITY: for each source kind, the picker offers exactly the sinks the
// CLI supports — no more, no fewer. Failed silently for three sinks
// (video/audio/sub) because nothing compared the two lists before this.
#[test]
fn the_picker_offers_exactly_the_cli_sinks_for_each_source_kind() {
    use std::collections::BTreeSet;

    // Title-level sinks: every one applies to any source that has titles,
    // container included.
    let per_title: &[&str] = &[
        "mkv", "m2ts", "demux", "video", "audio", "sub", "chapters", "json", "fvi",
    ];
    // Whole-disc sinks: `iso://` as a dest needs a physical disc to read,
    // `dir://` needs a disc file tree. Neither exists for a container.
    let whole_disc: &[&str] = &["iso", "dir"];

    for (disc_source, mp4_ok, mpg_ok) in [
        (true, true, true),
        (true, false, true),
        (false, true, false),
        (false, false, false),
        (true, true, false),
    ] {
        let mut want: BTreeSet<String> = per_title.iter().map(|s| s.to_string()).collect();
        if mp4_ok {
            want.insert("mp4".to_string());
        }
        if mpg_ok {
            want.insert("mpg".to_string());
        }
        if disc_source {
            want.extend(whole_disc.iter().map(|s| s.to_string()));
        }

        let fit = crate::ui::Fit {
            mp4: mp4_ok,
            mpg: mpg_ok,
        };
        let got: BTreeSet<String> = crate::ui::output_formats(disc_source, fit)
            .into_iter()
            .flatten()
            .map(dest_scheme)
            .collect();

        assert_eq!(
            got,
            want,
            "disc_source={disc_source} {fit:?}: picker sinks diverge from the CLI's\
                 \n  picker-only: {:?}\n  cli-only: {:?}",
            got.difference(&want).collect::<Vec<_>>(),
            want.difference(&got).collect::<Vec<_>>(),
        );

        // Each scheme must be one libfreemkv actually recognizes — a typo'd
        // row would otherwise agree with a typo'd expectation above.
        for scheme in &got {
            let url = format!("{scheme}://out/");
            assert!(
                !matches!(
                    libfreemkv::parse_url(&url),
                    libfreemkv::StreamUrl::Unknown { .. }
                ),
                "{url} is not a scheme libfreemkv recognizes"
            );
        }
    }
}

/// The ticked tracks must survive into the mux. `Default::default()` here
/// is All/All, i.e. every track the user just deselected.
#[test]
fn explicit_track_ticks_survive_into_the_mux() {
    let mut r = req();
    r.explicit_streams = true;
    r.audio_pids = vec![4352];
    r.sub_pids = vec![];
    let sel = stream_selection_for(&r, None);
    assert_eq!(sel.audio, libfreemkv::PidFilter::Only(vec![4352]));
    // Ticking nothing under subtitles means keep NONE, not keep all.
    assert_eq!(sel.subtitle, libfreemkv::PidFilter::Only(vec![]));
    assert!(!sel.is_all(), "an explicit selection is never All/All");

    // No explicit choice: keep everything.
    r.explicit_streams = false;
    assert!(stream_selection_for(&r, None).is_all());
}

// FT16 (stop design v5 §2.10, T27): "**ST-F1** sets the GUI to no deadline"; "GUI, CLI
// and the engine then behave identically: a halt-aware send only, and the user's Stop
// is the bound". Per spec; do not change without a spec citation proving otherwise.
#[test]
fn gui_mux_on_slow_sink_completes() {
    let mut r = req();
    r.raw = true;
    let o = mux_opts(&r);
    assert!(o.raw, "raw passthrough must reach the mux");
    assert_eq!(o.batch_sectors, 64);
    assert!(!o.skip_errors);
    // Selection is deliberately NOT here — the Url mux arm reads it off
    // InputOptions, and setting it here silently keeps every track.
    assert!(o.selection.is_all());
    r.raw = false;
    assert!(!mux_opts(&r).raw);
}

/// The three fields whose absence is invisible: the wrong title, a lost
/// key, or a discarded track selection, each under the right filename.
#[test]
fn per_title_input_options_carry_the_index_the_keys_and_the_selection() {
    use crate::ku_fixtures::*;
    let fx = bd_image(&[Some(K1)], 1);
    let set = resolve(
        &fx,
        libfreemkv::keys::KeyScope::Titles(vec![0]),
        &[(Answer::Keydb, &[K1])],
        &Calls::default(),
    )
    .unwrap();

    let mut r = req();
    r.explicit_streams = true;
    r.audio_pids = vec![4353, 4354];
    // The two titles disagree, which is the whole point: title 3 has 4354
    // unticked while title 0 keeps it. With one filter for the whole rip
    // the union wins, so this fixture is what makes the assertion below fail.
    r.title_pids = crate::engine::TitleStreams::PerTitle(vec![
        (0, vec![4353, 4354], vec![]),
        (3, vec![4353], vec![]),
    ]);

    for idx in [0usize, 3] {
        let plan = title_plan(&r, "iso:///i.iso", "mkv:///o.mkv", idx, false);
        assert_eq!(
            plan.titles,
            fe::Selection::Titles(vec![idx]),
            "a missing title index muxes title 0 under title {}'s name",
            idx + 1
        );
        let input = image_title_run(&set, &r, idx);
        let keys = input.keys.as_ref().expect("the rip's set must be passed");
        assert!(
            keys.is_aacs() && keys.is_for(&fx.disc.media_id()),
            "the rip's own set"
        );
        // PER TITLE, not the union: the union encoded the defect where a
        // PID unticked under one title was still written whenever a
        // sibling kept it ticked. `want` is written out, not re-derived.
        let want: Vec<u16> = if idx == 0 {
            vec![4353, 4354]
        } else {
            vec![4353]
        };
        match &input.title.selection.as_ref().expect("explicit").audio {
            libfreemkv::PidFilter::Only(got) => assert_eq!(
                *got,
                want,
                "title {} must be muxed with exactly its OWN ticked audio",
                idx + 1
            ),
            other => panic!("an explicit selection must be a PidFilter::Only, got {other:?}"),
        }
    }
}

/// One title fans out into the destination directory; two or more each get
/// their own subdirectory, because a demux sink names files by TRACK. Get
/// this wrong and every title after the first overwrites the one before.
#[test]
fn a_multi_title_demux_gives_each_title_its_own_directory() {
    assert!(!demux_needs_subdirs(1));
    assert!(demux_needs_subdirs(2));
    assert!(demux_needs_subdirs(12));
    // Zero titles never reaches the loop, but must not read as "multi".
    assert!(!demux_needs_subdirs(0));
}

// --multipass on a title output must still recover, and a whole-disc ISO
// must recover even without it. As && the first loses recovery passes,
// the second panics on the per-title loop's unreachable!().
#[test]
fn multipass_and_iso_output_both_route_through_recovery() {
    assert_eq!(
        recovery_plan(OutKind::File("mkv"), true),
        DiscPlan::Recover { deliver_iso: false }
    );
    assert_eq!(
        recovery_plan(OutKind::IsoImage, false),
        DiscPlan::Recover { deliver_iso: true }
    );
    assert_eq!(
        recovery_plan(OutKind::IsoImage, true),
        DiscPlan::Recover { deliver_iso: true }
    );
    assert_eq!(
        recovery_plan(OutKind::File("mkv"), false),
        DiscPlan::PerTitle
    );
    assert_eq!(
        recovery_plan(OutKind::Demux("demux"), false),
        DiscPlan::PerTitle
    );
    // The folder extract is handled before this decision and must not be
    // routed into recovery by it.
    assert_eq!(
        recovery_plan(OutKind::DecryptedFolder, false),
        DiscPlan::PerTitle
    );
}

/// A recovery that salvaged even one byte has something to mux; only zero
/// does not. Inverted, a good recovery deletes its own ISO and reports
/// "no readable data".
#[test]
fn only_a_zero_byte_recovery_has_nothing_to_mux() {
    assert!(recovery_produced_no_data(0));
    assert!(!recovery_produced_no_data(1));
    assert!(!recovery_produced_no_data(50_000_000_000));
}

/// The staging ISO is removed unless the user asked to keep it. Inverted,
/// a multi-hour recovery is deleted against an explicit setting.
#[test]
fn the_staging_iso_is_kept_only_when_keep_iso_is_set() {
    assert!(should_delete_staging_iso(false, true, false));
    assert!(!should_delete_staging_iso(true, true, false));
}

/// A cancelled mux must not take the recovery down with it. The mux
/// reports a cancel as `Ok`, so `keep_iso` alone deleted a multi-hour read
/// the user could then only recover by re-reading the disc.
#[test]
fn a_cancelled_mux_keeps_the_staging_iso() {
    assert!(!should_delete_staging_iso(false, true, true));
}

/// Same for a mux that failed: the staged image is exactly what lets the
/// user retry the mux without touching the drive again.
#[test]
fn a_failed_mux_keeps_the_staging_iso() {
    assert!(!should_delete_staging_iso(false, false, false));
    assert!(!should_delete_staging_iso(false, false, true));
}

/// The key strip names the source whose key the rip's set proved (`status().origin`),
/// read from the set Open resolved: `None` from a disc that WAS unlocked reports no
/// source, and a constant names every disc's source the same.
#[test]
fn the_winning_key_source_is_read_from_the_set() {
    use crate::ku_fixtures::*;
    use libfreemkv::keys::KeyScope;
    let fx = bd_image(&[Some(K1)], 1);
    for (answer, who) in [(Answer::Keydb, "keydb"), (Answer::Online, "online")] {
        let set = resolve(
            &fx,
            KeyScope::Titles(vec![0]),
            &[(answer, &[K1])],
            &Calls::default(),
        );
        let set = set.unwrap();
        let got = super::key_summary(&fx.disc, Some(&set));
        assert_eq!(got, format!("unlocked via {who}"));
    }
}

// The GUI raw disc→ISO copy must scan with raw_copy, exactly as the CLI's `--raw` does.
#[test]
fn the_gui_raw_disc_to_iso_scan_requests_raw_copy() {
    let iso = out_kind("Whole disc → ISO image");
    assert!(
        disc_raw_copy(iso, true),
        "a raw ISO copy must scan with raw_copy"
    );
    assert!(
        !disc_raw_copy(iso, false),
        "a decrypting ISO keeps the fatal E7031"
    );
    assert!(!disc_raw_copy(out_kind("Selected titles → MKV"), true));

    // Behavioural seam (no hardware needed): stub the scan and observe the
    // exact `raw_copy` `run_disc_scanning` handed it for a raw ISO request.
    let mut r = req();
    r.format = "Whole disc → ISO image".into();
    r.raw = true;
    r.dest_dir = std::env::temp_dir().to_string_lossy().into_owned();
    let state = Arc::new(RunState::default());
    let sink = UiSink(state.clone());
    let seen = std::cell::Cell::new(None);
    let _ = run_disc_scanning(&r, &sink, &state, None, |_, _, raw_copy| {
        seen.set(Some(raw_copy));
        Err(libfreemkv::Error::DeviceNotFound {
            path: String::new(),
        })
    });
    assert_eq!(
        seen.get(),
        Some(true),
        "run_disc must hand disc_raw_copy's answer to the scan"
    );
}

// The CLI's verdict: NOTHING readable is the only failure, and even then the ISO
// is kept, not deleted. A holed image always succeeds and always points at another run.
#[test]
fn an_iso_copy_takes_the_cli_copy_verdict() {
    assert!(iso_recovery_result(&clean_result(), "/x.iso").is_ok());
    let no_data = fe::MultipassResult {
        good_bytes: 0,
        unreadable_bytes: 1_048_576,
        complete: false,
        ..clean_result()
    };
    let err =
        iso_recovery_result(&no_data, "/x.iso").expect_err("no data at all is the only failure");
    assert!(
        err.contains("/x.iso"),
        "the no-data error must say where the (unusable but kept) ISO is: {err}"
    );

    // Holed but non-empty: still a SUCCESS — the image is kept and usable — but
    // must name the loss and point at another run, whether or not this run
    // already was multipass (it only retries once per invocation).
    let holed = fe::MultipassResult {
        unreadable_bytes: 1_048_576,
        complete: false,
        ..clean_result()
    };
    let msg = iso_recovery_result(&holed, "/x.iso").expect("a holed copy still succeeds");
    assert!(
        msg.contains("1.0")
            && msg.contains(crate::disc_copy_verdict::retry_with_multipass_hint().as_str()),
        "the loss and the retry hint must both be named: {msg}"
    );
}

// ── damage under tolerance is still disclosed ───────────────────────────

fn clean_result() -> fe::MultipassResult {
    fe::MultipassResult {
        unreadable_bytes: 0,
        pending_bytes: 0,
        good_bytes: 50_000_000_000,
        main_lost_ms: 0.0,
        lost_bytes: 0,
        severity: fe::DamageSeverity::Clean,
        passes: 1,
        aborted_for_loss: false,
        halted: false,
        wedged: false,
        complete: true,
    }
}

/// A perfect recovery adds nothing: the plain success message the caller
/// already builds must not grow a spurious trailing note.
#[test]
fn a_clean_recovery_has_no_damage_note() {
    assert_eq!(damage_note(&clean_result()), "");
}

// The regression engine.rs's success path shipped: a disc with real
// unreadable/pending bytes UNDER abort_lost_secs used to report a plain
// success line, identical to a perfect rip — hiding damage the CLI shows.
#[test]
fn residual_damage_under_tolerance_is_named_in_the_note() {
    let result = fe::MultipassResult {
        unreadable_bytes: 10 * 1_048_576,
        pending_bytes: 2 * 1_048_576,
        good_bytes: 40_000_000_000,
        main_lost_ms: 0.0,
        lost_bytes: 0,
        severity: fe::DamageSeverity::Cosmetic,
        passes: 2,
        ..clean_result()
    };
    let note = damage_note(&result);
    assert!(!note.is_empty(), "damage under tolerance produced no note");
    assert!(note.contains("10.0"), "unreadable MB missing: {note}");
    assert!(note.contains("2.0"), "pending MB missing: {note}");
}

/// `main_lost_ms` is the main-title playback time actually lost — the
/// figure an operator cares about (not just raw byte counts). It must
/// show up in the note, converted to seconds, whenever it is positive.
#[test]
fn lost_playback_time_is_named_when_quantifiable() {
    let result = fe::MultipassResult {
        unreadable_bytes: 1_048_576,
        pending_bytes: 0,
        main_lost_ms: 4_500.0,
        lost_bytes: 0,
        severity: fe::DamageSeverity::Cosmetic,
        ..clean_result()
    };
    crate::strings::set_locale("en");
    let note = damage_note(&result);
    let line = crate::strings::fmt(
        "rip.damage_lost_movie",
        &[("time", &super::fmt_damage_time(4.5))],
    );
    assert!(
        note.ends_with(&format!("\n{line}")),
        "no lost-playback-time line ({line:?}) in: {note}"
    );
}

/// `main_lost_ms` is documented as NaN when the loss cannot be quantified
/// (no title extents). A NaN must not corrupt the note (e.g. print
/// "NaNs") — it is simply omitted, leaving the byte-count line intact.
#[test]
fn unquantifiable_loss_does_not_corrupt_the_note() {
    let result = fe::MultipassResult {
        unreadable_bytes: 1_048_576,
        pending_bytes: 0,
        main_lost_ms: f64::NAN,
        lost_bytes: 0,
        severity: fe::DamageSeverity::Moderate,
        ..clean_result()
    };
    let note = damage_note(&result);
    assert!(!note.to_lowercase().contains("nan"), "{note}");
}
