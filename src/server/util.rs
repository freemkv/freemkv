/// Bytes per disc sector, `u64`-typed for autorip's sector↔byte arithmetic
/// (predominantly `u64`) so call sites need no per-site cast. Re-exports
/// libfreemkv's single source of truth — not a re-declared literal.
pub const SECTOR_BYTES: u64 = libfreemkv::consts::SECTOR_BYTES_U64;

/// Bytes per mebibyte (2^20), `f64` for human-readable MB/s and MB display
/// math. Single source so the two prior spellings (`1_048_576.0` and
/// `1024.0 * 1024.0`) can't drift.
pub const BYTES_PER_MIB: f64 = 1024.0 * 1024.0;

/// Bytes per gibibyte (2^30), `f64` for human-readable GB display math.
pub const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Milliseconds per second, `f64` for the lost-video time math that converts
/// between the millisecond-resolution damage accounting and second-resolution
/// display/threshold values.
pub const MILLIS_PER_SEC: f64 = 1000.0;

/// Current epoch seconds.
pub fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Howard Hinnant's civil-from-days. Returns (year, month, day) UTC.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Format epoch seconds as YYYY-MM-DD.
pub fn format_date() -> String {
    let (y, m, d) = civil_from_days((epoch_secs() / 86400) as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Format current UTC time as ISO-8601 (YYYY-MM-DDTHH:MM:SSZ).
pub fn format_iso_datetime() -> String {
    let secs = epoch_secs();
    let (y, mo, d) = civil_from_days((secs / 86400) as i64);
    let day = (secs % 86400) as u32;
    let h = day / 3600;
    let mi = (day % 3600) / 60;
    let s = day % 60;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Filesystem-safe ISO timestamp: YYYY-MM-DDTHH-MM-SSZ (colons replaced).
/// Used for rip-archive filenames so they sort correctly and are portable.
pub fn format_iso_datetime_filename() -> String {
    format_iso_datetime().replace(':', "-")
}

// ─── Filename / display helpers — consolidated here as the source of truth.

// Fallback path segment when sanitization yields nothing usable: a constant,
// filesystem-trivial, non-traversing segment callers always receive.
const SAFE_FALLBACK: &str = "untitled";

fn ensure_safe_segment(s: String) -> String {
    // Strip leading dots (hidden-file / "." / ".." defense).
    let stripped = s.trim_start_matches('.');
    // Reject empty or all-dots results (e.g. "", ".", "..", "...").
    if stripped.is_empty() || stripped.chars().all(|c| c == '.') {
        return SAFE_FALLBACK.to_string();
    }
    stripped.to_string()
}

/// Sanitize a name for use as a filesystem path segment with **no spaces**.
/// Used for staging directories and file basenames where snake_case is
/// preferred (so logs and shell commands don't need quoting).
///
/// Keeps `[A-Za-z0-9 \-_.]`, drops everything else, then collapses spaces
/// to underscores. The result can never be empty, `.`, `..`, or all-dots —
/// those collapse to a safe fallback (see `ensure_safe_segment`).
pub fn sanitize_path_compact(name: &str) -> String {
    let filtered = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ' ' || *c == '-' || *c == '_' || *c == '.')
        .collect::<String>()
        .trim()
        .replace(' ', "_");
    ensure_safe_segment(filtered)
}

/// How many discs may hide behind one title before we stop trying. A title
/// with 64 distinct discs behind it is a bug, not a boxset.
pub const MAX_DISC_VARIANTS: u32 = 64;

/// THE "another disc of the same title" naming rule, in one place.
///
/// Yields the variant numbers (1, 2, 3, ... [`MAX_DISC_VARIANTS`]) and
/// returns the first one `claimable` accepts. Callers supply what
/// "claimable" means in their domain (e.g. a staging dir carrying this
/// disc's `.disc-label`, or a library file whose bytes are this disc's
/// output) and render the result with [`disc_variant_name`]. `None` means
/// every variant is taken by some other disc; treat that as an error.
pub fn disc_variant(mut claimable: impl FnMut(u32) -> bool) -> Option<u32> {
    (1..=MAX_DISC_VARIANTS).find(|n| claimable(*n))
}

/// Render a [`disc_variant`] number onto a base name: variant 1 is the bare
/// `base` (the first disc keeps the plain title — display and existing library
/// naming must not change), 2 and up append `_2`, `_3`, ...
pub fn disc_variant_name(base: &str, variant: u32) -> String {
    if variant <= 1 {
        base.to_string()
    } else {
        format!("{base}_{variant}")
    }
}

/// Sanitize a name for a user-visible directory (e.g. the final library
/// destination `movies/Aurora Drift (2024)/`). Spaces preserved; apostrophes
/// kept (filesystems handle them, omitting them mangles "What's Up Doc").
///
/// Same path-segment safety guarantee as [`sanitize_path_compact`]: the
/// result is never empty, `.`, `..`, or all-dots (see `ensure_safe_segment`).
pub fn sanitize_path_display(name: &str) -> String {
    let filtered = name
        .chars()
        .filter(|c| {
            c.is_ascii_alphanumeric()
                || *c == ' '
                || *c == '-'
                || *c == '_'
                || *c == '.'
                || *c == '\''
        })
        .collect::<String>()
        .trim()
        .to_string();
    ensure_safe_segment(filtered)
}

/// Format a number of seconds as `Xh YYm`. Used by the rip card and the
/// disc info banner.
pub fn format_duration_hm(secs: f64) -> String {
    let h = (secs / 3600.0) as u32;
    let m = ((secs % 3600.0) / 60.0) as u32;
    format!("{}h {:02}m", h, m)
}

/// A source file read by `include_str!`, with line endings normalised to `\n`.
///
/// Several tests pin a call site by matching its source text, because the site
/// is only reachable after a real disc scan and an argument swap there would
/// pass every unit test. Those patterns are written with `\n`, but a checkout
/// on Windows has `core.autocrlf=true` by default, so the file arrives with
/// `\r\n` and every multi-line pattern misses — a green suite on Linux and a
/// red one on Windows, over a difference that has nothing to do with the code
/// under test.
#[cfg(test)]
pub fn source_lf(src: &str) -> std::borrow::Cow<'_, str> {
    if src.contains('\r') {
        std::borrow::Cow::Owned(src.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(src)
    }
}

#[cfg(test)]
mod tests {
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
}
