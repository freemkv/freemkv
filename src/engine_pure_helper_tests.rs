// A drive open failure carries the catalog explanation, not the bare E-code.
#[test]
fn a_drive_open_failure_is_explained_not_a_bare_code() {
    let e = libfreemkv::Error::DeviceNotFound {
        path: "/dev/sr9".into(),
    };
    let msg = super::drive_error(&e);
    assert_eq!(msg, super::explain(e.code()));
    assert_ne!(msg, format!("{e}"), "Display is only the code");
    assert!(super::failed_with("recovery failed", &e).starts_with("recovery failed: "));
}

// A title output with nothing selected is refused up front; a whole-disc one needs no titles.
#[test]
fn a_title_output_with_nothing_selected_is_refused() {
    assert_eq!(
        super::require_selection(false, &[]),
        Err("Nothing selected to rip.".to_string())
    );
    assert!(super::require_selection(false, &[0]).is_ok());
    assert!(super::require_selection(true, &[]).is_ok());
}

// A failed mux removes what it left, but never an earlier rip it did not touch.
#[test]
fn a_failed_mux_keeps_an_untouched_earlier_output_and_removes_its_own() {
    use super::{output_stamp, remove_failed_output};
    let dir = std::env::temp_dir().join(format!("fmkv-failed-out-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("old.mkv");
    std::fs::write(&old, b"earlier rip").unwrap();
    let old_s = old.to_str().unwrap();
    remove_failed_output(old_s, output_stamp(old_s));
    assert!(old.exists(), "an untouched earlier output must survive");

    let before = output_stamp(old_s);
    std::fs::write(&old, b"").unwrap();
    remove_failed_output(old_s, before);
    assert!(!old.exists(), "a truncated output is the failed mux's");

    let fresh = dir.join("fresh.mkv");
    let fresh_s = fresh.to_str().unwrap();
    let before = output_stamp(fresh_s);
    std::fs::write(&fresh, b"").unwrap();
    remove_failed_output(fresh_s, before);
    assert!(!fresh.exists());
    std::fs::remove_dir_all(&dir).ok();
}

// `session_credentials` reads the host certs from the same `~`-expanded keydb as key lookup.
#[cfg(unix)]
#[test]
fn session_credentials_expand_a_tilde_keydb_path() {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = std::env::temp_dir().join(format!("fmkv-creds-home-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let hc = format!(
        "| HC | HOST_PRIV_KEY 0x{} | HOST_CERT 0x{}\n",
        "00".repeat(20),
        "a1".repeat(92)
    );
    std::fs::write(home.join("keydb.cfg"), hc).unwrap();
    let old = std::env::var_os("HOME");
    unsafe { std::env::set_var("HOME", &home) };
    let keys = super::KeyConfig {
        keydb_path: "~/keydb.cfg".into(),
        ..Default::default()
    };
    let creds = super::session_credentials(&keys);
    unsafe {
        match old {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }
    std::fs::remove_dir_all(&home).ok();
    assert!(creds.is_some(), "a ~/ keydb path must reach the host certs");
}

use super::{fmt_damage_time, purpose_label};

/// `fmt_damage_time` picks its unit by magnitude across all five bands.
/// The GUI twin of the CLI's `pipe::fmt_damage_time` (which is tested); this
/// copy had no test, so a mutant that swapped a threshold or a unit passed.
#[test]
fn fmt_damage_time_picks_the_unit_by_magnitude() {
    assert_eq!(fmt_damage_time(7200.0), "2.0h"); // >= 1h
    assert_eq!(fmt_damage_time(90.0), "2m"); // >= 1m
    assert_eq!(fmt_damage_time(5.0), "5s"); // >= 1s
    assert_eq!(fmt_damage_time(0.25), "0.25s"); // >= 0.01s
    assert_eq!(fmt_damage_time(0.004), "4ms"); // sub-centisecond
}

/// Every audio `LabelPurpose` maps to its own label, and `Normal` to none —
/// so a stream row's purpose tag is never dropped or mislabeled.
#[test]
fn purpose_label_names_each_purpose_and_omits_normal() {
    use libfreemkv::LabelPurpose;
    let p = |x| purpose_label(x).unwrap_or_default();
    assert_eq!(
        p(LabelPurpose::Commentary),
        crate::strings::get("stream.purpose.commentary")
    );
    assert_eq!(
        p(LabelPurpose::Descriptive),
        crate::strings::get("stream.purpose.descriptive")
    );
    assert_eq!(
        p(LabelPurpose::Score),
        crate::strings::get("stream.purpose.score")
    );
    assert_eq!(
        p(LabelPurpose::Ime),
        crate::strings::get("stream.purpose.ime")
    );
    assert_eq!(purpose_label(LabelPurpose::Normal), None);
}
