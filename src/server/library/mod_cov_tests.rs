use super::*;

#[test]
fn every_console_line_kind_survives_the_title_log() {
    let kinds = [
        LineKind::Cmd,
        LineKind::Out,
        LineKind::Ok,
        LineKind::Warn,
        LineKind::Err,
        LineKind::Debug,
    ];
    let text: String = kinds
        .iter()
        .map(|k| log_line(*k, &format!("{k:?}\nline")) + "\n")
        .collect();
    let lines = parse_log(&text);
    assert_eq!(lines.len(), kinds.len());
    for (line, kind) in lines.iter().zip(kinds) {
        assert_eq!(line.kind, kind);
        assert_eq!(line.text, format!("{kind:?} line"));
    }
    assert_eq!(parse_log("1\tbogus\tx")[0].kind, LineKind::Out);
}

#[test]
fn the_indexer_wait_ends_early_only_when_woken() {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    assert!(!lib.wait_for_wake(std::time::Duration::from_millis(10)));
    lib.wake_indexer();
    assert!(lib.wait_for_wake(std::time::Duration::from_secs(5)));
    assert!(
        !lib.wait_for_wake(std::time::Duration::from_millis(10)),
        "a wake is taken once"
    );
}

#[test]
fn reaudit_queues_the_files_asked_for_or_every_file() {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    assert_eq!(
        lib.reaudit(None),
        0,
        "an empty library has nothing to audit"
    );
    let file = t.path().join("A.mkv");
    std::fs::write(&file, b"mkv").unwrap();
    let n = lib.reaudit(Some(std::slice::from_ref(&file)));
    assert!(n <= 1, "{n}");
}

#[test]
fn the_landings_kept_for_a_concurrent_scan_are_bounded() {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    let file = t.path().join("A.mkv");
    std::fs::write(&file, b"mkv").unwrap();
    for _ in 0..=LANDINGS_KEPT {
        lib.note_landed(&file, None);
    }
    assert_eq!(lib.landings.lock().unwrap().len(), LANDINGS_KEPT);
    assert_eq!(lib.landed.load(Ordering::SeqCst), LANDINGS_KEPT as u64 + 1);
    lib.note_landed(&t.path().join("gone.mkv"), None);
    assert_eq!(lib.landed.load(Ordering::SeqCst), LANDINGS_KEPT as u64 + 1);
}

#[test]
fn a_queue_file_that_cannot_be_set_aside_blocks_queueing_and_holds() {
    let t = tempfile::tempdir().unwrap();
    let file = t.path().join(queue::QUEUE_FILE);
    std::fs::write(&file, b"{damaged").unwrap();
    for n in 0..1000u32 {
        let name = match n {
            0 => format!("{}.unreadable", queue::QUEUE_FILE),
            n => format!("{}.unreadable.{n}", queue::QUEUE_FILE),
        };
        std::fs::write(t.path().join(name), b"").unwrap();
    }
    let lib = Library::open(t.path(), &t.path().join("logs"));
    let d = Dirs {
        library: t.path().join("movies"),
        tv: None,
        isos: None,
        iso_subfolders: false,
    };
    let why = lib.queue_block(&d, false).expect("queueing waits");
    assert!(why.contains("could not be set aside"), "{why}");
    assert!(why.contains("Nothing was queued"), "{why}");
    let now = crate::server::util::epoch_secs();
    assert!(lib.hold_for_unreadable_queue(now));
    let hold = lib.hold().expect("the queue is held");
    assert_eq!(hold.role, QUEUE_FILE_ROLE);
    assert!(
        hold.message.contains("could not be set aside"),
        "{}",
        hold.message
    );
}
