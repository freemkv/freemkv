use super::*;

fn tmpdir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static CTR: AtomicU64 = AtomicU64::new(0);
    let n = CTR.fetch_add(1, Ordering::Relaxed);
    // Per project convention, never /tmp (wiped on reboot). Anchor
    // under the workspace's target/ (gitignored) instead.
    let d = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/test-scratch")
        .join(format!(
            "autorip-log-test-{}-{}-{}",
            std::process::id(),
            tag,
            n
        ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("logs")).unwrap();
    d
}

#[test]
fn device_log_writes_iso_timestamped_line() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("iso_ts");
    // Route the test's logs to the tempdir.
    // SAFETY: env access in single-threaded tests.
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_sg_{}", std::process::id());
    device_log(&dev, "hello");
    let content = std::fs::read_to_string(device_log_path(&dev)).unwrap();
    // Format: [YYYY-MM-DDTHH:MM:SSZ] hello
    assert!(content.starts_with('['));
    assert!(content.contains(']'));
    assert!(content.trim_end().ends_with("hello"));
    let bracket = &content[1..21]; // 20-char ISO datetime inside brackets
    assert_eq!(bracket.len(), 20);
    assert!(bracket.ends_with('Z'));
    assert_eq!(bracket.as_bytes()[10], b'T');
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn new_session_writes_build_banner_to_file_not_ring() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("build_banner");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_banner_{}", std::process::id());
    device_log(&dev, "first line");
    device_log(&dev, "second line");

    // File: a build banner precedes the first line (session anchor), and the
    // build label is present so the slice is attributable.
    let content = std::fs::read_to_string(device_log_path(&dev)).unwrap();
    assert!(
        content.contains("log session start") && content.contains(crate::server::VERSION_LABEL),
        "file must carry a build banner: {content}"
    );
    assert_eq!(
        content.matches("log session start").count(),
        1,
        "exactly one banner per session, not per line"
    );
    // The banner must come before the first real line.
    let banner_at = content.find("log session start").unwrap();
    let first_at = content.find("first line").unwrap();
    assert!(banner_at < first_at, "banner must precede the first line");

    // Ring (live UI view) is unchanged — banner is file-only, so line
    // accounting stays exactly as before.
    let ring = get_device_log(&dev, 100);
    assert_eq!(
        ring.len(),
        2,
        "ring holds only the two real lines, no banner"
    );
    assert!(ring.iter().all(|l| !l.contains("log session start")));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn archive_device_log_moves_to_rips_dir() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("archive_move");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_mv_{}", std::process::id());
    device_log(&dev, "pre-archive");
    let live = device_log_path(&dev);
    assert!(std::path::Path::new(&live).exists());

    archive_device_log(&dev);

    // Live file gone after archive.
    assert!(!std::path::Path::new(&live).exists());

    // Rips dir has exactly one file matching the device name.
    let rips_dir = d.join("logs").join("rips");
    let archived: Vec<_> = std::fs::read_dir(&rips_dir)
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(&dev))
        .collect();
    assert_eq!(archived.len(), 1, "expected one archived log file");

    let content = std::fs::read_to_string(archived[0].path()).unwrap();
    assert!(content.contains("pre-archive"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn archive_name_never_replaces_an_earlier_archive() {
    let d = tmpdir("archive_unique");
    let rips = d.to_string_lossy().into_owned();
    let first = unique_archive_path(&rips, "sg4", "T");
    assert!(first.ends_with("/sg4_T.log"));
    std::fs::write(&first, b"a").unwrap();
    let second = unique_archive_path(&rips, "sg4", "T");
    assert_ne!(second, first);
    std::fs::write(&second, b"b").unwrap();
    let third = unique_archive_path(&rips, "sg4", "T");
    assert!(third != first && third != second);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn archive_device_log_skips_a_zero_byte_log() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("archive_zero");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_zero_{}", std::process::id());
    std::fs::write(device_log_path(&dev), b"").unwrap();
    archive_device_log(&dev);
    let rips_dir = d.join("logs").join("rips");
    let archived = std::fs::read_dir(&rips_dir)
        .map(|r| r.filter_map(|e| e.ok()).count())
        .unwrap_or(0);
    assert_eq!(archived, 0, "an empty log is not archived");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn device_log_cleans_control_bytes_from_every_sink() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("ctl_sinks");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_ctl_{}", std::process::id());
    device_log(&dev, "label \u{1b}[2J evil");
    let file = std::fs::read_to_string(device_log_path(&dev)).unwrap();
    let ring = get_device_log(&dev, 10).join("\n");
    for (sink, text) in [("file", file), ("ring", ring)] {
        assert!(!text.contains('\u{1b}'), "{sink} kept the escape: {text:?}");
        assert!(text.contains("label ?[2J evil"), "{sink}: {text:?}");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn archive_device_log_no_op_when_empty() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("archive_empty");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_empty_{}", std::process::id());
    // Don't call device_log — file doesn't exist yet. archive_device_log
    // must not panic or create a junk archive entry.
    archive_device_log(&dev);
    let rips_dir = d.join("logs").join("rips");
    if rips_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&rips_dir)
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(&dev))
            .collect();
        assert!(entries.is_empty());
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn archive_device_log_clears_in_memory_buffer() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("archive_buf");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_buf_{}", std::process::id());
    device_log(&dev, "first");
    device_log(&dev, "second");
    assert!(!get_device_log(&dev, 100).is_empty());

    archive_device_log(&dev);
    assert!(get_device_log(&dev, 100).is_empty());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn archive_failure_keeps_in_memory_ring() {
    // If the on-disk archive fails, the in-memory ring MUST survive so
    // the live UI doesn't go empty while the log is still on disk. Force
    // create_dir_all("logs/rips") to fail via a regular file in its place.
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("archive_fail");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    // Plant a file at logs/rips so the dir can't be created.
    std::fs::write(d.join("logs").join("rips"), b"not a dir").unwrap();

    let dev = format!("test_fail_{}", std::process::id());
    device_log(&dev, "live line");
    assert!(!get_device_log(&dev, 100).is_empty());
    let live = device_log_path(&dev);
    assert!(std::path::Path::new(&live).exists());

    archive_device_log(&dev);

    // Ring preserved (archive failed), live file still on disk.
    assert!(
        !get_device_log(&dev, 100).is_empty(),
        "in-memory ring must be kept when archive rename/setup fails"
    );
    assert!(
        std::path::Path::new(&live).exists(),
        "live log file must remain on disk after a failed archive"
    );
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn forget_device_clears_ring_without_archiving() {
    // Hot-unplug eviction: forget_device drops the in-memory ring but
    // leaves the on-disk device log in place (no archive).
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("forget");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_forget_{}", std::process::id());
    device_log(&dev, "before unplug");
    assert!(!get_device_log(&dev, 100).is_empty());
    let live = device_log_path(&dev);
    assert!(std::path::Path::new(&live).exists());

    forget_device(&dev);

    // Ring evicted...
    assert!(
        get_device_log(&dev, 100).is_empty(),
        "in-memory ring must be evicted on hot-unplug"
    );
    // ...but the durable on-disk log is left untouched (no archive).
    assert!(
        std::path::Path::new(&live).exists(),
        "device log file must remain on disk after forget_device"
    );
    let rips_dir = d.join("logs").join("rips");
    if rips_dir.exists() {
        let archived: Vec<_> = std::fs::read_dir(&rips_dir)
            .unwrap()
            .filter_map(|r| r.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(&dev))
            .collect();
        assert!(archived.is_empty(), "forget_device must not archive");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn sanitize_log_msg_strips_ansi_and_control_bytes() {
    // Log-injection defense: a crafted disc string could inject ANSI
    // escapes into a terminal/log; every control byte (incl. ESC
    // \u{1b}) becomes '?', while ordinary printable text survives.
    assert_eq!(
        sanitize_log_msg("\u{1b}[2J\u{1b}[1;1H"),
        "?[2J?[1;1H",
        "each ESC must become '?', the rest of the CSI text is preserved"
    );

    // A bare ESC is replaced.
    assert_eq!(sanitize_log_msg("a\u{1b}b"), "a?b");

    // Other C0 controls (NUL, BEL, backspace, TAB) and DEL and a C1
    // control (0x9b, single-char CSI) all map to '?'.
    assert_eq!(
        sanitize_log_msg("x\u{0}\u{7}\u{8}\t\u{7f}\u{9b}y"),
        "x??????y",
        "all C0/DEL/C1 control bytes must be neutralized"
    );

    // Newlines are control characters too — the function replaces them
    // (it does NOT preserve line structure), so verify that behavior
    // explicitly rather than assuming they pass through.
    assert_eq!(sanitize_log_msg("line1\nline2"), "line1?line2");

    // Ordinary text — including multibyte UTF-8 — is preserved verbatim.
    assert_eq!(
        sanitize_log_msg("WRAITHLINE_PART_TWO — café 日本語"),
        "WRAITHLINE_PART_TWO — café 日本語",
        "printable UTF-8 must pass through unchanged"
    );
    assert_eq!(sanitize_log_msg(""), "");
}

#[test]
fn sanitize_device_neutralizes_traversal() {
    // The hard invariant at the construction point: a device with a
    // path separator or traversal sequence can't escape logs/.
    for bad in ["../etc/passwd", "a/b", "..", "", ".", "x\\y"] {
        let path = format!("{}/logs/device_{}.log", "/cfg", super::sanitize_device(bad));
        assert!(
            !path.contains(".."),
            "path must not contain traversal for {bad:?}: {path}"
        );
        assert_eq!(
            path.matches("/logs/device_").count(),
            1,
            "device must be a single component for {bad:?}: {path}"
        );
        // The component after device_ must not introduce a new dir.
        assert!(!path.contains("device_../") && !path.contains("device_a/b"));
    }
    // A normal device name passes through unchanged.
    assert_eq!(super::sanitize_device("sg0"), "sg0");
}

#[test]
fn get_device_log_respects_line_limit() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("line_limit");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_lim_{}", std::process::id());
    for i in 0..5 {
        device_log(&dev, &format!("line {i}"));
    }
    let lines = get_device_log(&dev, 3);
    assert_eq!(lines.len(), 3);
    // Tail of the buffer — last 3 lines are 2, 3, 4.
    assert!(lines[2].contains("line 4"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_viewer_keeps_following_after_the_ring_wraps() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("since_wrap");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_since_{}", std::process::id());
    for i in 0..(RING_CAP + 20) {
        device_log(&dev, &format!("line {i}"));
    }
    let (seq, all) = get_device_log_since(&dev, 0);
    assert_eq!(all.len(), RING_CAP);
    device_log(&dev, "after the wrap");
    let (next, new) = get_device_log_since(&dev, seq);
    assert_eq!(new.len(), 1, "a full ring must not hide the next line");
    assert!(new[0].1.contains("after the wrap"));
    assert!(next > seq);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ring_evicts_oldest_past_cap() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("ring_cap");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    let dev = format!("test_ring_{}", std::process::id());
    for i in 0..(RING_CAP + 50) {
        device_log(&dev, &format!("line {i}"));
    }
    // Ring is capped: asking for more than the cap returns at most cap.
    let all = get_device_log(&dev, RING_CAP + 1000);
    assert_eq!(all.len(), RING_CAP, "ring must be capped at RING_CAP");
    // Oldest lines evicted: the first retained line is line 50.
    assert!(all[0].contains("line 50"), "got: {}", all[0]);
    assert!(all[RING_CAP - 1].contains(&format!("line {}", RING_CAP + 49)));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rotate_system_log_archives_only_when_large() {
    let _guard = crate::server::log::env_guard();
    let d = tmpdir("sys_rotate");
    unsafe {
        std::env::set_var("AUTORIP_DIR", &d);
    }
    // Small system log: must NOT rotate.
    syslog("small system message");
    rotate_system_log_if_large();
    assert!(
        std::path::Path::new(&device_log_path("system")).exists(),
        "small system log must be left in place"
    );

    // Grow it past the threshold, then rotate.
    let big = "x".repeat((SYSTEM_LOG_ROTATE_BYTES + 1024) as usize);
    std::fs::write(device_log_path("system"), big).unwrap();
    rotate_system_log_if_large();
    // Invariant: the OVERSIZED log is gone, not that no log exists.
    // `syslog()` runs transitively from other tests without the env
    // guard and can recreate the file post-rotation via AUTORIP_DIR.
    let live_path = device_log_path("system");
    let live_len = std::fs::metadata(&live_path).map(|m| m.len()).unwrap_or(0);
    assert!(
        live_len <= SYSTEM_LOG_ROTATE_BYTES,
        "oversized system log must be rotated out (live log is {live_len} bytes)"
    );
    let rips_dir = d.join("logs").join("rips");
    let archived: Vec<_> = std::fs::read_dir(&rips_dir)
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("system"))
        .collect();
    assert_eq!(archived.len(), 1, "expected one archived system log");
    let _ = std::fs::remove_dir_all(&d);
}
