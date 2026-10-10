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
