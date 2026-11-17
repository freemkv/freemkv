use super::install_keydb;

const KEYDB: &[u8] =
    b"0x1111111111111111111111111111111111111111 = Test | V | 0x22222222222222222222222222222222\n";

#[test]
fn installing_keydb_creates_its_writable_data_directory() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("data/freemkv/keydb.cfg");
    install_keydb(KEYDB, path.to_str().unwrap()).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), KEYDB);
}

#[test]
fn write_failure_keeps_destination_and_action_in_translated_error() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("keydb.cfg");
    std::fs::create_dir(&path).unwrap();
    let error = install_keydb(KEYDB, path.to_str().unwrap()).unwrap_err();
    assert!(error.contains(path.to_str().unwrap()), "{error}");
    assert!(error.contains("directory write permissions"), "{error}");
    assert!(path.is_dir());
}

#[test]
fn invalid_keydb_does_not_claim_a_permissions_problem() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("keydb.cfg");
    let error = install_keydb(b"not a key database", path.to_str().unwrap()).unwrap_err();
    assert!(!error.contains("permissions"), "{error}");
    assert!(!path.exists());
}

#[test]
fn keydb_status_reports_presence_and_age() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("keydb.cfg");
    let mut s = super::Settings {
        keydb_path: path.display().to_string(),
        ..super::Settings::default()
    };
    let missing = s.keydb_status();
    std::fs::write(&path, vec![b'x'; 4096]).unwrap();
    let today = s.keydb_status();
    assert_ne!(today, missing);
    assert!(today.contains('4'), "{today}");
    let day = std::time::Duration::from_secs(86_400);
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    for ago in [day + day / 2, day * 5] {
        f.set_modified(std::time::SystemTime::now() - ago).unwrap();
        let older = s.keydb_status();
        assert_ne!(older, today, "{older}");
    }
    s.keydb_path = temp.path().join("none.cfg").display().to_string();
    assert_eq!(s.keydb_status(), missing);
}

#[test]
fn update_keydb_refuses_a_missing_url_or_destination_and_a_failed_download() {
    assert!(super::update_keydb("  ", "/tmp/k.cfg").is_err());
    assert!(super::update_keydb("https://example.com/k", " ").is_err());
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("keydb.cfg");
    let url = format!("http://127.0.0.1:{port}/keydb.cfg");
    assert!(super::update_keydb(&url, dest.to_str().unwrap()).is_err());
    assert!(!dest.exists());
}
