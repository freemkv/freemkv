use super::*;

#[test]
fn source_lf_normalises_crlf_and_leaves_lf_alone() {
    assert_eq!(source_lf("a\r\nb\r\n"), "a\nb\n");
    assert_eq!(source_lf("a\nb\n"), "a\nb\n");
    // A borrowed return on the common path means the normalisation costs
    // nothing on the platform that does not need it.
    assert!(matches!(source_lf("a\nb\n"), std::borrow::Cow::Borrowed(_)));
}

#[test]
fn civil_from_days_epoch() {
    // Unix epoch day 0 = 1970-01-01.
    assert_eq!(civil_from_days(0), (1970, 1, 1));
}

#[test]
fn civil_from_days_leap_year_march() {
    // 2024-03-01 is day 19783 from Unix epoch (verified via Python datetime).
    assert_eq!(civil_from_days(19783), (2024, 3, 1));
}

#[test]
fn civil_from_days_far_future() {
    // 2026-04-24 = day 20567 from epoch.
    assert_eq!(civil_from_days(20567), (2026, 4, 24));
}

#[test]
fn format_iso_datetime_shape() {
    // Can't assert exact value (depends on wall clock) but can assert shape.
    let s = format_iso_datetime();
    assert_eq!(s.len(), 20); // "YYYY-MM-DDTHH:MM:SSZ"
    assert!(s.ends_with('Z'));
    assert_eq!(s.as_bytes()[10], b'T');
    assert_eq!(s.as_bytes()[4], b'-');
    assert_eq!(s.as_bytes()[13], b':');
}

#[test]
fn format_iso_datetime_filename_no_colons() {
    // Filesystem-safe variant replaces `:` with `-`.
    let s = format_iso_datetime_filename();
    assert!(!s.contains(':'));
    assert!(s.ends_with('Z'));
}

#[test]
fn timestamps_format_a_known_instant() {
    // 2024-03-01T13:45:09Z
    let secs = 19783 * 86400 + 13 * 3600 + 45 * 60 + 9;
    assert_eq!(iso_datetime_at(secs), "2024-03-01T13:45:09Z");
    assert_eq!(date_at(secs), "2024-03-01");
    assert_eq!(iso_datetime_at(0), "1970-01-01T00:00:00Z");
    assert_eq!(iso_datetime_at(86399), "1970-01-01T23:59:59Z");
    assert_eq!(iso_datetime_at(86400), "1970-01-02T00:00:00Z");
}

#[test]
fn disc_variant_takes_the_first_free_number_and_caps() {
    assert_eq!(disc_variant(|_| true), Some(1));
    assert_eq!(disc_variant(|n| n >= 3), Some(3));
    assert_eq!(
        disc_variant(|n| n == MAX_DISC_VARIANTS),
        Some(MAX_DISC_VARIANTS)
    );
    assert_eq!(disc_variant(|_| false), None);
    let mut asked = 0;
    let _ = disc_variant(|_| {
        asked += 1;
        false
    });
    assert_eq!(asked, MAX_DISC_VARIANTS);
}

#[test]
fn disc_variant_name_keeps_the_bare_title_for_the_first_disc() {
    assert_eq!(disc_variant_name("Title", 1), "Title");
    assert_eq!(disc_variant_name("Title", 2), "Title_2");
    assert_eq!(disc_variant_name("Title", 64), "Title_64");
}

#[test]
fn overlong_titles_are_capped_to_a_creatable_segment() {
    let long = "a".repeat(300);
    assert_eq!(sanitize_path_compact(&long).len(), MAX_SEGMENT_LEN);
    assert_eq!(sanitize_path_display(&long).len(), MAX_SEGMENT_LEN);
}

#[test]
fn format_date_shape() {
    let s = format_date();
    assert_eq!(s.len(), 10); // "YYYY-MM-DD"
    assert_eq!(s.as_bytes()[4], b'-');
    assert_eq!(s.as_bytes()[7], b'-');
}

// ─── Sanitizer + duration helpers ────────────────────────────────────

#[test]
fn sanitize_path_compact_collapses_spaces_to_underscore() {
    assert_eq!(
        sanitize_path_compact("Aurora Drift Two"),
        "Aurora_Drift_Two"
    );
    assert_eq!(sanitize_path_compact("K for Kestrel"), "K_for_Kestrel");
}

#[test]
fn sanitize_path_compact_strips_unsafe_chars() {
    assert_eq!(
        sanitize_path_compact("Aurora: Drift Two"),
        "Aurora_Drift_Two"
    );
    assert_eq!(sanitize_path_compact("M*A*S*H"), "MASH");
    assert_eq!(sanitize_path_compact("Alien/Predator"), "AlienPredator");
}

#[test]
fn sanitize_path_compact_keeps_dots_dashes_underscores() {
    assert_eq!(sanitize_path_compact("Movie-2024.4K"), "Movie-2024.4K");
}

#[test]
fn sanitize_path_display_keeps_spaces_and_apostrophes() {
    assert_eq!(sanitize_path_display("What's Up Doc"), "What's Up Doc");
    assert_eq!(
        sanitize_path_display("Side Quest - A Long Journey"),
        "Side Quest - A Long Journey"
    );
}

#[test]
fn sanitize_path_display_strips_unsafe_chars() {
    assert_eq!(
        sanitize_path_display("Aurora: Drift Two"),
        "Aurora Drift Two"
    );
    assert_eq!(sanitize_path_display("M*A*S*H"), "MASH");
}

#[test]
fn sanitize_path_display_trims_whitespace() {
    assert_eq!(sanitize_path_display("  spaced title  "), "spaced title");
}

// ─── Hostile path-segment inputs (untrusted disc label / TMDB title) ──
// A disc label or TMDB title must never sanitize to "" (resolves to
// parent), "."/".." (traversal), or a hidden dot-name — verify fallback.

#[test]
fn sanitize_compact_never_emits_empty() {
    // All-non-ASCII (CJK / Arabic) filters down to nothing.
    assert_eq!(sanitize_path_compact("日本語のタイトル"), "untitled");
    assert_eq!(sanitize_path_compact("العنوان"), "untitled");
    assert_eq!(sanitize_path_compact(""), "untitled");
    assert_eq!(sanitize_path_compact("   "), "untitled");
    // Only-punctuation that the filter drops entirely.
    assert_eq!(sanitize_path_compact("***"), "untitled");
}

#[test]
fn sanitize_compact_never_emits_dot_segments() {
    assert_eq!(sanitize_path_compact("."), "untitled");
    assert_eq!(sanitize_path_compact(".."), "untitled");
    assert_eq!(sanitize_path_compact("..."), "untitled");
    // Surrounding whitespace must not reintroduce a traversal segment.
    assert_eq!(sanitize_path_compact("  ..  "), "untitled");
}

#[test]
fn sanitize_compact_strips_leading_dots() {
    // Leading dot would make a hidden file / break resume matching.
    assert_eq!(sanitize_path_compact(".hidden"), "hidden");
    assert_eq!(sanitize_path_compact("..weird"), "weird");
    // A legitimate internal/trailing dot is preserved.
    assert_eq!(sanitize_path_compact("Movie.2024"), "Movie.2024");
}

#[test]
fn sanitize_display_never_emits_empty_or_dot_segments() {
    assert_eq!(sanitize_path_display("日本語のタイトル"), "untitled");
    assert_eq!(sanitize_path_display(""), "untitled");
    assert_eq!(sanitize_path_display("."), "untitled");
    assert_eq!(sanitize_path_display(".."), "untitled");
    assert_eq!(sanitize_path_display("..."), "untitled");
    assert_eq!(sanitize_path_display("  ..  "), "untitled");
    // Leading dots stripped, real content preserved.
    assert_eq!(sanitize_path_display(".A Movie"), "A Movie");
}

#[test]
fn format_duration_hm_zero() {
    assert_eq!(format_duration_hm(0.0), "0h 00m");
}

#[test]
fn format_duration_hm_under_minute() {
    assert_eq!(format_duration_hm(30.0), "0h 00m");
}

#[test]
fn format_duration_hm_pads_minutes() {
    assert_eq!(format_duration_hm(3600.0 + 5.0 * 60.0), "1h 05m");
}

#[test]
fn format_duration_hm_two_hours() {
    assert_eq!(format_duration_hm(2.0 * 3600.0 + 30.0 * 60.0), "2h 30m");
}

#[test]
fn a_stamp_is_read_from_regular_files_only() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    std::fs::write(&file, b"bytes").unwrap();
    let stamp = Stamp::read(&file).unwrap();
    assert_eq!(stamp.size, 5);
    assert_eq!(
        stamp.modified,
        std::fs::metadata(&file).unwrap().modified().unwrap()
    );
    assert!(Stamp::read(temp.path()).is_err(), "a directory");
    #[cfg(unix)]
    {
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(Stamp::read(&link).is_err(), "a symlink");
    }
}

#[test]
fn a_stamp_verifies_size_and_time_and_passes_not_found_through() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    std::fs::write(&file, b"bytes").unwrap();
    pin_mtime(&file).unwrap();
    let stamp = Stamp::read(&file).unwrap();
    stamp.verify(&file).unwrap();
    // Equal bytes under a new inode keep the stamp: names, not ids, identify files.
    let copy = temp.path().join("copy");
    std::fs::write(&copy, b"bytes").unwrap();
    std::fs::rename(&copy, &file).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(stamp.modified)
        .unwrap();
    stamp.verify(&file).unwrap();
    let later = stamp.modified + std::time::Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert!(stamp.verify(&file).is_err(), "time changed");
    std::fs::write(&file, b"longer bytes").unwrap();
    assert!(
        Stamp {
            modified: Stamp::read(&file).unwrap().modified,
            ..stamp
        }
        .verify(&file)
        .is_err(),
        "size changed"
    );
    std::fs::remove_file(&file).unwrap();
    assert_eq!(
        stamp.verify(&file).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn pin_mtime_rounds_down_to_an_even_second() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("file");
    std::fs::write(&file, b"bytes").unwrap();
    let odd = std::time::UNIX_EPOCH + std::time::Duration::new(1_700_000_001, 500);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(odd)
        .unwrap();
    pin_mtime(&file).unwrap();
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
    );
}

#[test]
fn an_older_journal_identity_reads_as_a_stamp() {
    let old = serde_json::json!({
        "file_id": [16777232, 4242],
        "size": 19,
        "modified": {"secs_since_epoch": 1_700_000_000u64, "nanos_since_epoch": 0},
    });
    let stamp: Stamp = serde_json::from_value(old).unwrap();
    assert_eq!(
        stamp,
        Stamp {
            size: 19,
            modified: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        }
    );
    let saved = serde_json::to_value(stamp).unwrap();
    assert_eq!(saved.as_object().unwrap().len(), 2, "{saved}");
}

// A FIFO swapped in at a checked path (NFS carries special files) is refused as not a
// regular file at once; opening it must not wait for a writer and hang the mover.
#[cfg(unix)]
#[test]
fn a_stamp_of_a_fifo_is_refused_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("Title (2024).mkv");
    let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    let path = fifo.clone();
    std::thread::spawn(move || {
        let _ = tx.send(crate::server::util::Stamp::read(&path).map(|_| ()));
    });
    let result = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("Stamp::read blocked on a FIFO");
    let error = result.expect_err("a FIFO is not a delivered file");
    assert!(error.to_string().contains("not a regular file"), "{error}");
}
