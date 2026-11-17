//! One `refresh` round over real temp folders: every configured folder is checked and published.
use super::tests::LAST_LOCK;
use super::*;

#[test]
fn refresh_checks_and_publishes_every_configured_folder() {
    let _g = LAST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let dir = |n: &str| {
        let p = tmp.path().join(n);
        std::fs::create_dir_all(&p).unwrap();
        p.display().to_string()
    };
    let cfg = Config {
        autorip_dir: dir("config"),
        staging_dir: dir("staging"),
        output_dir: dir("output"),
        movie_dir: "Movies".into(),
        tv_dir: "TV".into(),
        iso_dir: String::new(),
        library_dir: String::new(),
        library_iso_dir: String::new(),
        ..Config::default()
    };
    std::fs::create_dir_all(tmp.path().join("output").join("Movies")).unwrap();
    refresh(&cfg);
    let staging = tmp.path().join("staging");
    let m = status_for(&staging).expect("staging was published");
    assert_eq!(m.fault, None, "{m:?}");
    // A configured folder that does not exist is published with a fault.
    let missing = Path::new(&cfg.output_dir).join("TV");
    let m = status_for(&missing).expect("TV was published");
    assert!(m.fault.is_some(), "{m:?}");
}
