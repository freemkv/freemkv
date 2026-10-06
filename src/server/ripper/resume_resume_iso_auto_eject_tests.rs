// Regression: resume_remux's ISO-output success path must honor auto_eject like the MKV
// terminal does (pre-fix it returned without ejecting). Pinned at source level.
#[test]
fn resume_iso_success_path_honors_auto_eject() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    let start = src
        .find("Auto-resume: ISO output complete")
        .expect("resume.rs should log \"Auto-resume: ISO output complete\" on the ISO path");
    // Bound the search to the ISO success region: from the ISO
    // completion log line up to the mux_iso call that opens the MKV
    // path (the next distinct terminal in the function).
    let end = src[start..]
        .find("super::mux::mux_iso")
        .map(|i| start + i)
        .expect("resume.rs should call mux_iso after the ISO branch");
    let region = &src[start..end];
    assert!(
        region.contains("should_auto_eject(cfg_read.auto_eject, device)"),
        "resume_remux ISO success path must gate eject through \
             should_auto_eject(cfg_read.auto_eject, device) — which encodes \
             both \"only when enabled\" and \"never for a synthetic _mux \
             device\" — matching the MKV terminal and the fresh-rip ISO \
             terminal; none found between the ISO completion log and mux_iso"
    );
    assert!(
        region.contains("super::eject_drive"),
        "the ISO auto_eject branch must call super::eject_drive"
    );
}

// The MKV resume terminal's eject must likewise route through the
// shared should_auto_eject predicate, not a bare auto_eject check
// that would let the _mux worker re-eject the physical drive.
#[test]
fn resume_mkv_terminal_gates_eject_through_predicate() {
    let src = crate::server::util::source_lf(include_str!("resume.rs"));
    let start = src
        .find("Honor auto_eject after a successful resume")
        .expect("resume.rs should have the MKV terminal auto_eject comment");
    let region = &src[start..(start + 1000).min(src.len())];
    assert!(
        region.contains("should_auto_eject(cfg_read.auto_eject, device)"),
        "the MKV resume terminal must gate eject through should_auto_eject"
    );
}
