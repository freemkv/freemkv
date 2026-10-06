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
