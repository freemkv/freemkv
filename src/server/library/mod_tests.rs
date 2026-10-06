use super::*;

#[test]
fn dirs_fall_back_to_where_rips_are_filed() {
    let mut c = Config {
        output_dir: "/media".into(),
        ..Config::default()
    };
    let d = dirs(&c);
    assert_eq!(d.library, PathBuf::from("/media"));
    assert_eq!(d.isos, None);
    assert!(!d.iso_subfolders);
    c.movie_dir = "movies".into();
    c.iso_dir = "isos".into();
    let d = dirs(&c);
    assert_eq!(d.library, PathBuf::from("/media/movies"));
    assert_eq!(d.isos, Some(PathBuf::from("/media/isos")));
    c.library_dir = "/lib".into();
    c.library_iso_dir = "/src".into();
    c.library_iso_subfolders = true;
    assert_eq!(
        dirs(&c),
        Dirs {
            library: "/lib".into(),
            isos: Some("/src".into()),
            iso_subfolders: true
        }
    );
}

fn dirs_in(t: &Path) -> Dirs {
    Dirs {
        library: t.join("lib"),
        isos: Some(t.join("isos")),
        iso_subfolders: false,
    }
}

#[test]
fn an_empty_library_beside_isos_blocks_new_mkvs() {
    let t = tempfile::tempdir().unwrap();
    let d = dirs_in(t.path());
    std::fs::create_dir_all(&d.library).unwrap();
    std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
    std::fs::write(d.isos.as_ref().unwrap().join("A (2000).iso"), b"x").unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    lib.index_now(&d);
    assert!(lib.queue_block(&d, true).unwrap().contains("empty"));
    assert_eq!(
        lib.queue_block(&d, false),
        None,
        "replacing is not creating"
    );
    std::fs::create_dir(d.library.join("Something")).unwrap();
    lib.index_now(&d);
    assert_eq!(lib.queue_block(&d, true), None);
}

#[test]
fn a_scan_that_began_before_a_remux_landed_does_not_undo_it() {
    let t = tempfile::tempdir().unwrap();
    let d = dirs_in(t.path());
    std::fs::create_dir_all(d.library.join("Other")).unwrap();
    std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
    std::fs::write(d.isos.as_ref().unwrap().join("A (2000).iso"), b"x").unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    lib.index_now(&d);
    let landed_at = lib.landed.load(Ordering::SeqCst);
    let scan = lib.scan(&d);
    let target = d.library.join("A (2000)/A (2000).mkv");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(
        &target,
        probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true),
    )
    .unwrap();
    lib.note_landed(&target, None);
    lib.commit_scan(scan, landed_at);
    let snap = lib.snapshot();
    assert!(snap.sigs.contains_key(&target), "the landed sig survives");
    assert!(snap.rows.iter().any(|r| r.mkv.as_ref() == Some(&target)));
}

#[test]
fn an_empty_library_folder_does_not_erase_the_audits() {
    let t = tempfile::tempdir().unwrap();
    let d = dirs_in(t.path());
    let mkv = d.library.join("A/A.mkv");
    std::fs::create_dir_all(mkv.parent().unwrap()).unwrap();
    std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
    std::fs::write(
        &mkv,
        probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true),
    )
    .unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    lib.index_now(&d);
    let sig = FileSig::stat(&mkv).unwrap();
    let report = probe::audit_fast(&mkv).unwrap();
    lib.audits.record_fast(&mkv, sig, report, 1);
    std::fs::remove_dir_all(d.library.join("A")).unwrap();
    lib.index_now(&d);
    assert!(
        lib.audits.report(&mkv, sig).is_some(),
        "an empty folder may be an unmounted share"
    );
}

#[test]
fn a_restart_keeps_every_stored_audit_until_a_file_is_really_gone() {
    let t = tempfile::tempdir().unwrap();
    let d = Dirs {
        library: t.path().join("media/movies"),
        isos: Some(t.path().join("media/iso")),
        iso_subfolders: false,
    };
    let cfg = t.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
    std::fs::write(
        d.isos.as_ref().unwrap().join("2 Fast 2 Furious (2003).iso"),
        b"x",
    )
    .unwrap();
    let bytes = probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true);
    let names = [
        "2 Fast 2 Furious (2003)/2 Fast 2 Furious (2003).mkv",
        "Constantine (2005)/Constantine (2005).mkv",
        "Dune (1984)/Dune (1984).mkv",
        "Dune (2021)/Dune (2021).mkv",
        // Two cuts, neither the feature: an ambiguous row with no MKV of its own.
        "Alien (1979)/Alien Theatrical.mkv",
        "Alien (1979)/Alien Director.mkv",
    ];
    let mut stored = Vec::new();
    {
        let audits = audit::Audits::open(&cfg);
        for n in names {
            let p = d.library.join(n);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, &bytes).unwrap();
            let sig = FileSig::stat(&p).unwrap();
            audits.record_fast(&p, sig, probe::audit_fast(&p).unwrap(), 1);
            stored.push((p, sig));
        }
    }
    let lib = Library::open(&cfg, &t.path().join("logs"));
    lib.set_deep_enabled(true);
    // The audit worker's first refill can run before the indexer's first scan.
    lib.refill_audits();
    lib.index_now(&d);
    lib.refill_audits();
    lib.index_now(&d);
    let reopened = audit::Audits::open(&cfg);
    for (p, sig) in &stored {
        assert!(
            reopened.report(p, *sig).is_some(),
            "{} kept its audit",
            p.display()
        );
    }
    let (gone, gone_sig) = stored[1].clone();
    std::fs::remove_file(&gone).unwrap();
    lib.index_now(&d);
    assert!(
        lib.audits.report(&gone, gone_sig).is_none(),
        "a deleted file loses it"
    );
    assert!(lib.audits.report(&stored[0].0, stored[0].1).is_some());
    assert!(lib.audits.report(&stored[4].0, stored[4].1).is_some());
}

#[test]
fn orphaned_partials_are_swept_but_the_running_one_is_kept() {
    let t = tempfile::tempdir().unwrap();
    let d = dirs_in(t.path());
    let a = d.library.join("A");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(a.join("A.mkv.partial"), b"half").unwrap();
    std::fs::write(a.join("A.mkv"), b"whole").unwrap();
    let b = d.library.join("B");
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join("B.mkv.partial"), b"running").unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    lib.queue.add(vec![queue::NewJob {
        title: "B".into(),
        iso: "/i/B.iso".into(),
        target: b.join("B.mkv"),
        replace: false,
    }]);
    lib.queue.claim_next().unwrap();
    assert_eq!(lib.sweep_partials(&d), 1);
    assert!(!a.join("A.mkv.partial").exists());
    assert!(a.join("A.mkv").exists());
    assert!(
        b.join("B.mkv.partial").exists(),
        "the running job's partial stays"
    );
}

#[test]
fn the_console_is_a_bounded_ring() {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), t.path());
    for i in 0..(CONSOLE_LINES + 5) {
        lib.console(1, LineKind::Out, format!("line {i}"));
    }
    let all = lib.console_since(0);
    assert_eq!(all.len(), CONSOLE_LINES);
    assert_eq!(all[0].text, "line 5");
    let tail = lib.console_since(all[all.len() - 2].seq);
    assert_eq!(tail.len(), 1);
}

#[test]
fn a_log_name_cannot_leave_the_log_folder() {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    let p = lib.log_path("../../etc/passwd");
    assert_eq!(p.parent(), Some(t.path().join("logs").as_path()));
}

#[cfg(unix)]
#[test]
fn an_incomplete_scan_keeps_what_it_could_not_list_unless_it_is_gone() {
    use std::os::unix::fs::PermissionsExt as _;
    let t = tempfile::tempdir().unwrap();
    let d = dirs_in(t.path());
    std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
    let bytes = probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true);
    for name in ["A", "B", "C"] {
        let dir = d.library.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{name}.mkv")), &bytes).unwrap();
    }
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    lib.index_now(&d);
    let (b, c) = (d.library.join("B"), d.library.join("C"));
    let lock = |p: &Path, mode| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap()
    };
    lock(&b, 0o0);
    if std::fs::read_dir(&b).is_ok() {
        lock(&b, 0o755);
        return; // root reads anything
    }
    lib.rescan(&d);
    let snap = lib.snapshot();
    assert!(snap.incomplete);
    let b_mkv = b.join("B.mkv");
    assert!(
        snap.mkvs.iter().any(|m| m.path == b_mkv),
        "kept from before"
    );
    assert!(snap.sigs.contains_key(&b_mkv), "with its earlier signature");
    std::fs::remove_dir_all(&c).unwrap();
    lib.rescan(&d);
    let snap = lib.snapshot();
    lock(&b, 0o755);
    assert!(snap.mkvs.iter().any(|m| m.path == b_mkv));
    assert!(
        !snap.mkvs.iter().any(|m| m.path == c.join("C.mkv")),
        "NotFound is gone"
    );
}

#[test]
fn a_landed_remux_turns_its_iso_only_row_into_a_remux_row_and_queues_its_audit() {
    let t = tempfile::tempdir().unwrap();
    let d = dirs_in(t.path());
    std::fs::create_dir_all(d.library.join("Other")).unwrap();
    std::fs::create_dir_all(d.isos.as_ref().unwrap()).unwrap();
    std::fs::write(d.isos.as_ref().unwrap().join("A (2000).iso"), b"x").unwrap();
    let lib = Library::open(&t.path().join("cfg"), &t.path().join("logs"));
    lib.index_now(&d);
    let target = d.library.join("A (2000)/A (2000).mkv");
    let row = |lib: &Library| {
        lib.snapshot()
            .rows
            .iter()
            .find(|r| r.target.as_ref() == Some(&target))
            .cloned()
            .unwrap()
    };
    assert_eq!(row(&lib).kind, RowKind::IsoOnly);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(
        &target,
        probe::testmkv::mkv("freemkv 1.0.0", Some(60.0), Some(58), true),
    )
    .unwrap();
    lib.note_landed(&target, Some("freemkv 1.0.0".into()));
    let r = row(&lib);
    assert_eq!((r.kind, r.mkv.as_ref()), (RowKind::Remux, Some(&target)));
    let snap = lib.snapshot();
    assert!(
        snap.mkvs
            .iter()
            .any(|m| m.path == target && m.title == "A (2000)")
    );
    assert!(lib.audits.is_queued(&target));
    let sig = FileSig::stat(&target).unwrap();
    assert_eq!(
        lib.probes.cached_stamp(&target, sig),
        Some(Some("freemkv 1.0.0".into()))
    );
}

#[test]
fn the_title_log_round_trips_and_reads_old_lines() {
    let line = log_line(LineKind::Warn, "two\nlines\rhere");
    assert_eq!(line.matches('\n').count(), 0);
    let text = format!(
        "{line}\nplain legacy line\n5\tmystery\tbody\n{}",
        log_line(LineKind::Err, "tab\tinside")
    );
    let lines = parse_log(&text);
    assert_eq!(lines.len(), 4);
    assert_eq!(
        (lines[0].kind, lines[0].text.as_str()),
        (LineKind::Warn, "two lines here")
    );
    assert!(lines[0].ts > 0);
    assert_eq!(
        (lines[1].kind, lines[1].ts, lines[1].text.as_str()),
        (LineKind::Out, 0, "plain legacy line")
    );
    assert_eq!(
        (lines[2].kind, lines[2].ts, lines[2].text.as_str()),
        (LineKind::Out, 5, "body")
    );
    assert_eq!(
        (lines[3].kind, lines[3].text.as_str()),
        (LineKind::Err, "tab\tinside")
    );
    assert_eq!(lines[3].seq, 4);
}
