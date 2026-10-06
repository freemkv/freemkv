use super::{fmt_title_size, title_cells};
use crate::engine::Row;

fn row(depth: u8, duration_secs: f64, size_bytes: Option<u64>) -> Row {
    Row {
        role: None,
        item: String::new(),
        format: String::new(),
        notes: String::new(),
        type_s: String::new(),
        desc: String::new(),
        depth,
        checkable: depth > 0,
        title: 0,
        info: String::new(),
        pid: None,
        duration_secs,
        lang: String::new(),
        forced: false,
        mirrors: None,
        size_bytes,
    }
}

#[test]
fn a_title_size_reads_in_gigabytes_or_whole_megabytes() {
    assert_eq!(fmt_title_size(6_800_000_000), "6.8 GB");
    assert_eq!(fmt_title_size(48_123_456_789), "48.1 GB");
    assert_eq!(fmt_title_size(1_000_000_000), "1.0 GB");
    assert_eq!(fmt_title_size(999_500_000), "1.0 GB");
    assert_eq!(fmt_title_size(999_499_999), "999 MB");
    assert_eq!(fmt_title_size(734_003_200), "734 MB");
    assert_eq!(fmt_title_size(0), "0 MB");
}

#[test]
fn a_title_row_fills_both_cells() {
    let cells = title_cells(&row(1, 8600.0, Some(6_800_000_000)));
    assert_eq!(cells, ("2:23:20".to_string(), "6.8 GB".to_string()));
    assert_eq!(title_cells(&row(1, 1290.9, Some(734_003_200))).0, "21:30");
}

#[test]
fn a_title_without_a_reported_size_leaves_size_empty() {
    assert_eq!(
        title_cells(&row(1, 600.0, None)),
        ("10:00".to_string(), String::new())
    );
}

#[test]
fn disc_and_stream_rows_leave_both_cells_empty() {
    for depth in [0, 2] {
        assert_eq!(
            title_cells(&row(depth, 0.0, None)),
            (String::new(), String::new()),
            "depth {depth}"
        );
    }
}
