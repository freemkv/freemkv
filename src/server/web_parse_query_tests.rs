use super::*;

// parse_query's `clamp` truncates a field to MAX_FIELD_LEN (256) bytes
// on a char boundary. These pin an off-by-one mutant across three
// shapes: mid-char cutoff, exact cap, and cutoff already on a boundary.
#[test]
fn clamps_query_value_at_char_boundary_when_cutoff_lands_mid_char() {
    // 255 ASCII bytes, then a 2-byte 'é' straddling the 256-byte cutoff
    // (occupies bytes 255..257), then more filler. Byte offset 256 sits
    // inside 'é', so clamp must back up to 255 and drop 'é' entirely.
    let value = format!("{}é{}", "a".repeat(255), "b".repeat(10));
    let url = format!("/x?q={value}");
    let map = parse_query(&url);
    assert_eq!(
        map.get("q").map(String::as_str),
        Some("a".repeat(255).as_str()),
        "must truncate to the last full character before the cutoff, not split 'é'"
    );
}

#[test]
fn an_encoded_value_under_the_decoded_cap_is_not_cut() {
    // 30 CJK chars: 90 decoded bytes but 270 encoded bytes.
    let title = "\u{4e2d}".repeat(30);
    let enc: String = title.bytes().map(|b| format!("%{b:02X}")).collect();
    assert!(enc.len() > 256);
    let map = parse_query(&format!("/x?q={enc}"));
    assert_eq!(map.get("q").map(String::as_str), Some(title.as_str()));
}

#[test]
fn query_value_exactly_at_cap_is_not_truncated() {
    // Exactly 256 bytes (128 two-byte 'é' chars) — s.len() <= n, the
    // `<=` early-return branch, no truncation at all.
    let value = "é".repeat(128);
    assert_eq!(value.len(), 256);
    let url = format!("/x?q={value}");
    let map = parse_query(&url);
    assert_eq!(map.get("q").map(String::as_str), Some(value.as_str()));
}

#[test]
fn query_value_over_cap_already_on_boundary_truncates_cleanly() {
    // 260 plain ASCII bytes: over the cap, but byte 256 is already a
    // char boundary, so the backward-scan loop body never executes.
    let value = "a".repeat(260);
    let url = format!("/x?q={value}");
    let map = parse_query(&url);
    assert_eq!(
        map.get("q").map(String::as_str),
        Some("a".repeat(256).as_str())
    );
}

#[test]
fn tail_file_returns_whole_small_file_untruncated() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("log.txt");
    std::fs::write(&path, b"line1\nline2\nline3\n").unwrap();
    let out = tail_file(path.to_str().unwrap(), 4096).expect("tail must succeed");
    assert_eq!(
        out, "line1\nline2\nline3\n",
        "a file smaller than the cap is returned whole, no head drop"
    );
}

#[test]
fn tail_file_seeks_from_end_and_drops_partial_head_line() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("log.txt");
    // 10 lines of "NNNNNNNN\n" (9 bytes each = 90 bytes). Cap the read at 25
    // bytes so it seeks into the middle of a line — the truncated head line
    // must be dropped so callers never parse a half record.
    let mut content = String::new();
    for i in 0..10 {
        content.push_str(&format!("{:08}\n", i));
    }
    std::fs::write(&path, content.as_bytes()).unwrap();
    let out = tail_file(path.to_str().unwrap(), 25).expect("tail must succeed");
    assert!(
        out.len() <= 25,
        "the read must be bounded by max_bytes, got {} bytes",
        out.len()
    );
    assert!(
        !out.starts_with('\n') && out.ends_with("00000009\n"),
        "the tail must end at the last full line and start after a newline, got: {out:?}"
    );
    // Every returned line must be a complete 8-digit record (no partial head).
    for line in out.lines() {
        assert_eq!(line.len(), 8, "no partial line may survive, got: {line:?}");
    }
}

#[test]
fn tail_file_missing_file_is_an_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("nope.txt");
    assert!(
        tail_file(path.to_str().unwrap(), 4096).is_err(),
        "a missing file must surface an io error, not empty success"
    );
}
