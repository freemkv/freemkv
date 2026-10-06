#[test]
fn only_the_autorip_name_runs_the_daemon() {
    let is = |a0: &str| super::invoked_as_autorip(&[a0.to_string()]);
    assert!(is("/usr/local/bin/autorip"));
    assert!(is("autorip"));
    assert!(!is("/usr/local/bin/freemkv"));
    assert!(!super::invoked_as_autorip(&[]));
}
