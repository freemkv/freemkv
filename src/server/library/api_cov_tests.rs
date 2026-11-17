use super::*;

#[test]
fn a_long_title_log_is_tailed_from_a_whole_line() {
    let t = tempfile::tempdir().unwrap();
    let lib = Library::open(t.path(), &t.path().join("logs"));
    assert_eq!(log_tail(&lib, "Missing"), "", "no log yet");
    let path = lib.log_path("Long");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let line = format!("{}\n", "x".repeat(99));
    let mut text = line.repeat(3000);
    text.push_str("last\n");
    std::fs::write(&path, &text).unwrap();
    let tail = log_tail(&lib, "Long");
    assert!(tail.len() < 256 << 10, "{}", tail.len());
    assert!(tail.ends_with("last\n"));
    assert!(tail.lines().all(|l| l == "last" || l.len() == 99));
    let short = lib.log_path("Short");
    std::fs::write(&short, "one\ntwo\n").unwrap();
    assert_eq!(log_tail(&lib, "Short"), "one\ntwo\n");
}
