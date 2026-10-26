use super::resume_production_source;

// Regression: resume_remux's success path must fire the completion webhook like rip_disc
// does (both cold auto-resume and the _mux hand-off go through it). Pinned at source level.

#[test]
fn success_path_fires_completion_webhook() {
    let src = resume_production_source();
    let start = src
        .find("Auto-resume complete")
        .expect("resume.rs should log \"Auto-resume complete\" on the success path");
    // Bound the search to the success region: from the completion log
    // line up to the auto_eject branch that follows it.
    let end = src[start..]
        .find("super::should_auto_eject(")
        .map(|i| start + i)
        .expect("the success path ends with the auto_eject branch");
    let region = &src[start..end];
    assert!(
        region.contains("crate::server::webhook::send_rich"),
        "resume_remux success path must fire send_rich (the mux_complete \
             webhook), matching rip_disc; none found between \"Auto-resume \
             complete\" and the auto_eject branch"
    );
    assert!(
        region.contains("event: \"mux_complete\""),
        "the resume completion webhook is the mux stage, so it must use the \
             mux_complete event name (the drive-free rip_complete fires earlier, \
             on the sweep worker)"
    );
}
