use super::*;

// A process killed mid-save leaves `gui-settings.json.tmp.<pid>`, holding the key-service
// token. A start removes ours older than an hour; nothing else is touched.
#[test]
fn stale_settings_temps_are_removed_at_load() {
    let dir = tempfile::tempdir().unwrap();
    let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
    let file = |name: &str, old: bool| {
        let p = dir.path().join(name);
        std::fs::write(&p, b"{}").unwrap();
        if old {
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(hour_ago)
                .unwrap();
        }
        p
    };
    let stale = file("gui-settings.json.tmp.111", true);
    let fresh = file("gui-settings.json.tmp.222", false);
    let settings = file("gui-settings.json", true);
    let other = file("other.json.tmp.111", true);
    remove_stale_temps(dir.path(), std::time::Duration::from_secs(3600));
    assert!(!stale.exists());
    assert!(fresh.exists() && settings.exists() && other.exists());
}
