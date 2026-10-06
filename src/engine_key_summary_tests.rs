use super::{error_code, key_summary};

pub(super) fn disc(encrypted: bool) -> libfreemkv::Disc {
    libfreemkv::Disc {
        volume_id: "T".into(),
        meta_title: None,
        format: libfreemkv::DiscFormat::BluRay,
        capacity_sectors: 1,
        capacity_bytes: 2048,
        layers: 1,
        titles: vec![],
        region: libfreemkv::disc::DiscRegion::Free,
        aacs: None,
        css: None,
        encrypted,
        aacs_error: None,
        css_error: None,
        content_format: libfreemkv::ContentFormat::BdTs,
    }
}

// A scanned disc's AACS state: it carries no key (KU-X2).
pub(super) fn aacs() -> libfreemkv::AacsState {
    libfreemkv::test_util::aacs_state().build()
}

#[test]
fn unencrypted_disc_is_named_so() {
    assert_eq!(key_summary(&disc(false), None), "unencrypted");
}

#[test]
fn css_dvd_is_named_so() {
    let mut d = disc(true);
    d.css = Some(libfreemkv::css::CssState {
        title_key: [0u8; 5],
        crack_span: None,
    });
    assert_eq!(key_summary(&d, None), "CSS (DVD)");
}

/// The regression that started this: `ExternalUk` + empty `unit_keys` is the
/// pre-resolution placeholder. With no resolved set it must never read as unlocked,
/// and must never name a source.
#[test]
fn placeholder_origin_with_no_keys_reads_as_locked() {
    let mut d = disc(true);
    d.aacs = Some(aacs());
    let s = key_summary(&d, None);
    assert_eq!(s, "locked — no key yet");
    assert!(!s.contains("online") && !s.contains("keydb"));
}

/// KU §11.6: the key state "comes from the resolved set". Keys banked on the disc
/// (a resume-injected unit key, a VUK) never read as unlocked without one.
#[test]
fn banked_keys_never_unlock_without_a_set() {
    let mut d = disc(true);
    d.aacs = Some(aacs());
    assert_eq!(key_summary(&d, None), "locked — no key yet");
    let none = libfreemkv::keys::KeyRing::none();
    assert_eq!(key_summary(&d, Some(&none)), "locked — no key yet");
}

// error_code must parse the digit run, not the whole Display string.
// Regression: trim_start_matches('E').parse() returned 0 for every
// error that carries data after the code (E7022's disc hash, etc).
#[test]
fn error_code_parses_codes_that_carry_data() {
    let c = |s: &str| error_code(&std::io::Error::other(s.to_string()));
    // Bare form (already worked).
    assert_eq!(c("E9048"), 9048);
    // Data-carrying forms — all of these used to yield 0.
    assert_eq!(c("E7022: 8f3a1c0d"), 7022);
    assert_eq!(c("E8005: /path/to/keydb.cfg"), 8005);
    assert_eq!(c("E6000: 12345 0x02"), 6000);
    assert_eq!(c("E6014: 0x1100"), 6014);
    // Same range as the libs: u16 boundary kept, wider is no code (not saturated).
    assert_eq!(c("E65535"), 65535);
    assert_eq!(c("E65536"), 0);
    assert_eq!(c("E99999: x"), 0);
    // Not a library code.
    assert_eq!(c("No drive found"), 0);
    assert_eq!(c("E"), 0);
    assert_eq!(c("Eabc"), 0);
}

#[test]
fn recovery_job_carries_the_picked_titles() {
    let j = super::recovery_job("/dev/sr0", "/x/a.iso", &[2, 0]);
    assert!(matches!(j.selection, freemkv_engine::Selection::Titles(ref t) if t == &[2, 0]));
}

#[test]
fn removing_the_staging_iso_removes_its_mapfile() {
    let dir = std::env::temp_dir().join(format!("fmkv-stg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let iso = dir.join("L.iso");
    let d = disc(false);
    let map = d.mapfile_for(&iso);
    std::fs::write(&iso, b"x").unwrap();
    std::fs::write(&map, b"m").unwrap();
    super::remove_staging_iso(iso.to_str().unwrap(), &map);
    assert!(!iso.exists() && !map.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// All three key_source values must produce distinct key sources.
// Regression: key_url was derived only from "is the URL non-empty", so
// "Local keydb only" leaked disc ciphertext to the configured key service.
#[test]
fn key_source_setting_controls_whether_the_online_service_is_used() {
    let cfg = |src: &str| super::KeyConfig {
        keydb_path: "/tmp/keydb.cfg".into(),
        keyserver_url: "https://keys.example/decode".into(),
        keyserver_token: "t".into(),
        online_only: src.starts_with("Online"),
        local_only: src.starts_with("Local"),
    };

    let local = super::key_params(&cfg("Local keydb only"));
    assert!(
        local.key_url.is_none(),
        "Local keydb only must NOT consult the online service"
    );
    assert!(local.keydb_path.is_some());

    let both = super::key_params(&cfg("keydb, then online"));
    assert!(
        both.key_url.is_some(),
        "keydb, then online must consult the online service"
    );
    assert!(both.keydb_path.is_some());
    assert!(!both.online_only);

    let online = super::key_params(&cfg("Online key service only"));
    assert!(online.key_url.is_some());
    assert!(online.online_only, "Online-only must set online_only");
}
