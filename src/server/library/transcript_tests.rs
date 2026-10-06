use super::*;

#[test]
fn progress_matches_the_cli() {
    let gb = 1u64 << 30;
    assert_eq!(
        progress_line(gb / 10, 58 * gb / 10, 55 << 20, Some(113)),
        "  0.1 GB / 5.8 GB  (1.7%)  55.0 MB/s  ETA 1:53"
    );
    assert_eq!(
        progress_line(100 << 20, 500 << 20, 10 << 20, None),
        "  100 MB / 500 MB  (20.0%)  10.0 MB/s  ETA ?:??"
    );
}

#[test]
fn completion_matches_the_cli() {
    let bytes = (5.7 * (1u64 << 30) as f64) as u64;
    assert_eq!(
        complete_line(bytes, 99.0),
        "Complete: 5.7 GB in 99s (59 MB/s)"
    );
}

#[test]
fn verify_names_the_file_the_runtime_and_the_verdict() {
    let p = std::path::Path::new("/m/A/A.mkv");
    let ok = verify_line(p, true, Some(7195.0), 7200.0);
    assert!(ok.starts_with("Verify A.mkv..."), "{ok}");
    assert!(ok.ends_with("runtime 1:59:55 of 2:00:00"), "{ok}");
    assert!(ok.contains(&strings::get("rip.ok")));
    let bad = verify_line(p, false, None, 90.0);
    assert!(bad.contains("FAILED"), "{bad}");
    assert!(bad.ends_with("runtime unknown of 0:01:30"), "{bad}");
}

#[test]
fn progress_without_a_total_shows_bytes_and_speed_only() {
    assert_eq!(
        progress_line(10 << 20, 0, 5 << 20, Some(9)),
        "  10.0 MB  5.0 MB/s"
    );
}

#[test]
fn an_empty_title_prints_only_its_stream_count() {
    let mut t = libfreemkv::DiscTitle::empty();
    assert_eq!(
        stream_lines(&t),
        [format!("  {}: 0", strings::get("disc.streams"))]
    );
    t.duration_secs = 3725.0;
    let lines = stream_lines(&t);
    assert_eq!(lines.len(), 2);
    assert!(lines[1].ends_with(": 1:02:05"), "{lines:?}");
}
